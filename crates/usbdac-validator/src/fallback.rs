//! Exercise the production playback resolver and source-domain stream.
use super::*;
use crate::conversion_check::ConversionCheck;
use usbdac_core::playback::{PlaybackFidelity, PlaybackPolicy, PlaybackTransform};

fn emit_dop_markers(data: &mut [u8], channels: usize, slot: usize, phase: &mut bool) {
    for frame in data.chunks_exact_mut(channels * slot) {
        for word in frame.chunks_exact_mut(slot) {
            word[slot - 1] = if *phase { 0xfa } else { 0x05 };
        }
        *phase = !*phase;
    }
}

pub(super) fn run_fallback(
    dac: &Dac,
    _caps: &Capabilities,
    source: &StreamRequest,
    options: &RunOptions,
    cancel: &AtomicBool,
    device_lost: &AtomicBool,
) -> Value {
    let policy = PlaybackPolicy {
        allow_lossless: true,
        allow_lossy: true,
        dop_confirmed: false,
        allow_dop_pcm: true,
        ..PlaybackPolicy::default()
    };
    let mut evidence = json!({"source_request":format!("{source:?}"), "resolver":"production Dac::resolve_playback", "policy":format!("{policy:?}"), "status":"no_fallback", "source_fidelity":"not_tested", "target_wire_integrity":"not_tested", "conversion_verification":{"status":"not_tested"}, "dop_decoder":"not_observed"});
    if let Some(reason) = stop_reason(cancel, device_lost) {
        evidence["termination_reason"] = json!(reason);
        evidence["status"] = json!(reason);
        return evidence;
    }
    let decision = match dac.resolve_playback(source, &policy) {
        Ok(decision) => decision,
        Err(error) => {
            let lost = is_device_lost(&error);
            evidence["status"] = json!(if lost {
                "device_lost"
            } else if matches!(error, usbdac_core::Error::Unsupported { .. }) {
                "no_fallback"
            } else {
                "inconclusive"
            });
            if lost {
                evidence["termination_reason"] = json!("device_lost");
            }
            evidence["error"] = json!({"stage":"production_resolve", "code":error.code(), "message":error.to_string()});
            return evidence;
        }
    };
    let format = decision.format.clone();
    let fidelity = decision.fidelity;
    let preserve = matches!(
        fidelity,
        PlaybackFidelity::Exact | PlaybackFidelity::BitExact | PlaybackFidelity::Lossless
    );
    if decision.exact_refusal.is_none() {
        evidence["selection_note"] = json!("production resolver admitted the exact source on retry; compare the separately recorded exact result");
    }
    evidence["target_request"] = json!(format!("{:?}", format.request));
    evidence["wire_format"] = format_json(&format);
    evidence["planned_candidates"] = json!(decision
        .candidates
        .iter()
        .map(|c| format!("{c:?}"))
        .collect::<Vec<_>>());
    evidence["attempted_candidate_count"] = json!(decision.attempted_candidates);
    evidence["candidate_order_note"] =
        json!("ordered planned candidates; only the reported prefix was attempted");
    evidence["exact_refusal"] = json!(decision
        .exact_refusal
        .as_ref()
        .map(|e| json!({"code":e.code(), "message":e.to_string()})));
    evidence["conversion"] = json!({"kind":format!("{fidelity:?}"), "description":format!("{:?}", decision.transform), "policy":format!("{:?}", policy.conversion_options)});
    evidence["source_fidelity"] = json!(format!("{fidelity:?}"));
    if let Some(reason) = stop_reason(cancel, device_lost) {
        evidence["termination_reason"] = json!(reason);
        evidence["status"] = json!(reason);
        return evidence;
    }
    let lost = Arc::new(AtomicBool::new(false));
    let listener_lost = lost.clone();
    let async_error: Arc<Mutex<Option<(String, String)>>> = Arc::new(Mutex::new(None));
    let listener_error = async_error.clone();
    let mut stream = match dac.open_playback(
        &decision,
        Arc::new(move |event: DacEvent| {
            if matches!(&event, DacEvent::DeviceLost)
                || matches!(&event, DacEvent::Error(e) if is_device_lost(e))
            {
                listener_lost.store(true, Ordering::Release);
            }
            if let DacEvent::Error(error) = &event {
                if !is_device_lost(error) {
                    if let Ok(mut slot) = listener_error.try_lock() {
                        *slot = Some((error.code().into(), error.to_string()));
                    }
                }
            } else if matches!(
                &event,
                DacEvent::StateChanged(usbdac_core::StreamState::Failed)
            ) {
                if let Ok(mut slot) = listener_error.try_lock() {
                    if slot.is_none() {
                        *slot = Some((
                            "stream_failed".into(),
                            "Driver reported a failed stream".into(),
                        ));
                    }
                }
            }
        }),
    ) {
        Ok(stream) => stream,
        Err(error) => {
            let lost = is_device_lost(&error);
            evidence["status"] = json!(if lost { "device_lost" } else { "failed" });
            if lost {
                evidence["termination_reason"] = json!("device_lost");
            }
            evidence["error"] =
                json!({"stage":"open_playback", "code":error.code(), "message":error.to_string()});
            return evidence;
        }
    };
    let applied_headroom_db = match &decision.transform {
        PlaybackTransform::Pcm { plan, .. } if plan.resamples() => plan.options().headroom_db,
        PlaybackTransform::DsdToPcm { options, .. } => options.headroom_db,
        _ => 0.0,
    };
    let check = Arc::new(Mutex::new(ConversionCheck::new(
        *source,
        format.clone(),
        preserve,
        applied_headroom_db,
    )));
    let observed = check.clone();
    stream.set_observer(Some(Arc::new(move |bytes| {
        if let Ok(mut check) = observed.lock() {
            check.observe(bytes);
        }
    })));
    let (signal_rate, slot, bits) = match source.encoding {
        Encoding::DsdNative { bit_rate } => (bit_rate / 8, 1, 1),
        Encoding::DoP => (
            source.sample_rate,
            if source.bit_depth == 32 { 4 } else { 3 },
            source.bit_depth,
        ),
        Encoding::PcmU8 => (source.sample_rate, 1, 8),
        Encoding::PcmFloat32 => (source.sample_rate, 4, 32),
        Encoding::Pcm => (source.sample_rate, source.bit_depth / 8, source.bit_depth),
    };
    if signal_rate == 0 || decision.source_frame_bytes == 0 || source.channels == 0 {
        evidence["status"] = json!("inconclusive");
        evidence["error"] = json!({"stage":"source_signal", "code":"invalid_geometry"});
        let _ = stream.close();
        return evidence;
    }
    let frames = u64::from(signal_rate) * options.duration_ms.clamp(300, 10000) / 1000;
    let expected_bytes = frames * decision.source_frame_bytes as u64;
    let mut remaining = frames;
    let mut signal = Signal::from_spec(source.encoding, signal_rate, source.channels, slot, bits);
    let mut marker = false;
    let mut buffer = Vec::new();
    let mut offset = 0usize;
    let mut accepted = 0u64;
    let mut source_hash = Sha256::new();
    let deadline = Instant::now() + Duration::from_millis(options.leg_timeout_ms.max(100));
    let mut failure: Option<(String, String, String)> = None;
    while remaining > 0 || offset < buffer.len() {
        if let Some(error) = async_error.lock().ok().and_then(|slot| slot.clone()) {
            failure = Some(("usb_event".into(), error.0, error.1));
            break;
        }
        if lost.load(Ordering::Acquire) || device_lost.load(Ordering::Acquire) {
            failure = Some((
                "write".into(),
                "device_lost".into(),
                "Selected device was lost".into(),
            ));
            break;
        }
        if cancel.load(Ordering::Acquire) {
            failure = Some(("write".into(), "cancelled".into(), "Cancelled".into()));
            break;
        }
        if Instant::now() >= deadline {
            failure = Some((
                "write".into(),
                "timeout".into(),
                "Playback deadline exceeded".into(),
            ));
            break;
        }
        if offset == buffer.len() {
            let count = remaining.min(256) as usize;
            remaining -= count as u64;
            buffer = signal.frames(count);
            offset = 0;
            if source.encoding == Encoding::DoP {
                emit_dop_markers(
                    &mut buffer,
                    source.channels as usize,
                    slot as usize,
                    &mut marker,
                );
            }
        }
        match stream.write(&buffer[offset..]) {
            Ok(0) => std::thread::sleep(Duration::from_millis(2)),
            Ok(n) => {
                source_hash.update(&buffer[offset..offset + n]);
                if let Ok(mut check) = check.lock() {
                    check.source(&buffer[offset..offset + n]);
                }
                offset += n;
                accepted += n as u64;
            }
            Err(error) => {
                failure = Some((
                    "write".into(),
                    if is_device_lost(&error) {
                        "device_lost".into()
                    } else {
                        error.code().into()
                    },
                    error.to_string(),
                ));
                break;
            }
        }
    }
    let mut drain_started = false;
    while failure.is_none() && !stream.is_drained() {
        if let Some(error) = async_error.lock().ok().and_then(|slot| slot.clone()) {
            failure = Some(("usb_event".into(), error.0, error.1));
            break;
        }
        if lost.load(Ordering::Acquire) || device_lost.load(Ordering::Acquire) {
            failure = Some((
                "drain".into(),
                "device_lost".into(),
                "Selected device was lost".into(),
            ));
            break;
        }
        if cancel.load(Ordering::Acquire) {
            failure = Some(("drain".into(), "cancelled".into(), "Cancelled".into()));
            break;
        }
        if Instant::now() >= deadline {
            failure = Some((
                "drain".into(),
                "timeout".into(),
                "Drain deadline exceeded".into(),
            ));
            break;
        }
        if !drain_started {
            match stream.drain() {
                Ok(started) => drain_started = started,
                Err(error) => {
                    failure = Some((
                        "drain".into(),
                        if is_device_lost(&error) {
                            "device_lost".into()
                        } else {
                            error.code().into()
                        },
                        error.to_string(),
                    ));
                    break;
                }
            }
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    let stats = stream.target_stats();
    let conversion = stream.conversion_stats();
    let drained = stream.is_drained();
    let target_frame_bytes = if matches!(format.encoding, Encoding::DsdNative { .. }) {
        usize::from(format.channels)
    } else {
        format.bytes_per_frame()
    };
    let count_ok = accepted == expected_bytes
        && conversion.source_bytes == accepted
        && target_frame_bytes > 0
        && conversion.target_bytes > 0
        && conversion
            .target_bytes
            .is_multiple_of(target_frame_bytes as u64)
        && stats.frames_written == conversion.target_bytes / target_frame_bytes as u64
        && stats.played_frames == stats.frames_written
        && stats.completed_transfers > 0
        && drained;
    let pacing_ok = match format.pacing {
        usbdac_core::Pacing::ExplicitFeedback { .. }
        | usbdac_core::Pacing::ImplicitFeedback { .. } => stats.feedback_locked,
        usbdac_core::Pacing::Unsynchronized => false,
        _ => true,
    };
    let usb_pass = failure.is_none()
        && count_ok
        && stats.payload_verified
        && stats.clock_verified
        && pacing_ok
        && stats.lost_frames == 0
        && stats.underruns == 0
        && stats.short_packets == 0
        && stats.zero_packets == 0
        && stats.urb_timeouts == 0;
    let verification = check
        .lock()
        .map(|check| check.result(frames, signal_rate))
        .unwrap_or_else(|_| json!({"passed":false, "error":"observer state unavailable"}));
    let conversion_pass = verification.get("passed").and_then(Value::as_bool) == Some(true)
        && conversion.clipped_samples == 0;
    let enough_samples = verification
        .get("checked_frames")
        .and_then(Value::as_u64)
        .is_some_and(|n| n >= 64)
        || verification
            .get("source_bytes")
            .and_then(Value::as_u64)
            .is_some_and(|n| n > 0);
    evidence["conversion_verification"] = verification;
    evidence["conversion_verification"]["clipped_samples"] = json!(conversion.clipped_samples);
    evidence["conversion_verification"]["passed"] = json!(conversion_pass);
    evidence["target_wire_integrity"] = json!(if usb_pass { "verified" } else { "unverified" });
    evidence["status"] = json!(if usb_pass && conversion_pass {
        "passed_usb"
    } else if usb_pass && !conversion_pass && enough_samples {
        "failed"
    } else if failure.is_none() {
        "inconclusive"
    } else {
        "failed"
    });
    evidence["digests"] = json!({"source_accepted_sha256":format!("{:x}",source_hash.finalize())});
    evidence["counts"] = json!({"expected_source_bytes":expected_bytes, "source_bytes_accepted":accepted, "source_frames":frames, "source_rate_hz":signal_rate, "target_bytes_accepted":conversion.target_bytes, "clipped_samples":conversion.clipped_samples, "drained":drained, "stats":format!("{stats:?}"), "pacing_verified":pacing_ok});
    if let Some((stage, code, message)) = failure {
        if code == "device_lost" || code == "cancelled" {
            evidence["termination_reason"] = json!(code);
            evidence["status"] = json!(code);
        }
        evidence["error"] = json!({"stage":stage, "code":code, "message":message});
    }
    let close = stream.close();
    evidence["close"] = json!(format!("{close:?}"));
    if let Err(error) = close {
        if is_device_lost(&error) {
            evidence["termination_reason"] = json!("device_lost");
            evidence["status"] = json!("device_lost");
        } else {
            evidence["prior_termination_reason"] = evidence["termination_reason"].clone();
            evidence["status"] = json!("failed");
            evidence["termination_reason"] = json!("retirement_failed");
        }
    }
    evidence
}
