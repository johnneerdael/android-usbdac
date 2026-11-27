//! Validator-owned direct USB fallback: production converters, without platform routing claims.
use super::*;
use usbdac_core::convert::{
    ConversionPlan, ConvertOptions, Converter, PcmFormat, SampleFormat, MAX_RATE, MIN_RATE,
};
use usbdac_core::dsd_pcm::DsdPcmConverter;

fn source_rate(request: &StreamRequest) -> Option<u32> {
    match request.encoding {
        Encoding::Pcm | Encoding::PcmFloat32
            if (MIN_RATE..=MAX_RATE).contains(&request.sample_rate)
                && (1..=8).contains(&request.channels) =>
        {
            Some(request.sample_rate)
        }
        Encoding::DsdNative { bit_rate }
            if matches!(bit_rate, 2_822_400 | 5_644_800 | 11_289_600)
                && (1..=8).contains(&request.channels) =>
        {
            Some(bit_rate / 16)
        }
        Encoding::DoP
            if request.bit_depth == 24
                && matches!(request.sample_rate * 16, 2_822_400 | 5_644_800 | 11_289_600)
                && (1..=8).contains(&request.channels) =>
        {
            Some(request.sample_rate)
        }
        _ => None,
    }
}
fn candidates(caps: &Capabilities, source: &StreamRequest, rate: u32) -> Vec<StreamRequest> {
    let source_depth = if matches!(source.encoding, Encoding::DsdNative { .. } | Encoding::DoP) {
        24
    } else {
        source.bit_depth
    };
    let mut pairs = Vec::new();
    for alt in caps.alt_settings.iter().filter(|a| {
        a.format == AltFormat::Pcm
            && a.channels == source.channels
            && [16, 24, 32].contains(&a.valid_bits)
    }) {
        let mut rates = alt.sample_rates.clone();
        for r in &alt.rate_ranges {
            for candidate in PCM_RATES.iter().copied().chain([r.min_hz, r.max_hz, rate]) {
                if range_contains(r, candidate) && (r.step_hz > 0 || r.min_hz == r.max_hz) {
                    rates.push(candidate);
                }
            }
            if r.step_hz > 0 && rate >= r.min_hz {
                let n = (rate - r.min_hz) / r.step_hz;
                for index in [n, n.saturating_add(1)] {
                    if let Some(candidate) = index
                        .checked_mul(r.step_hz)
                        .and_then(|v| r.min_hz.checked_add(v))
                    {
                        if range_contains(r, candidate) {
                            rates.push(candidate);
                        }
                    }
                }
            }
        }
        for candidate in rates {
            let geometry = alt.subslot_bytes > 0
                && alt.subslot_bytes <= 4
                && u32::from(alt.valid_bits) <= u32::from(alt.subslot_bytes) * 8
                && alt.pacing.is_some()
                && alt.service_interval_us > 0
                && alt.max_packet_bytes > 0
                && (u64::from(candidate) * u64::from(alt.service_interval_us)).div_ceil(1_000_000)
                    * u64::from(alt.channels)
                    * u64::from(alt.subslot_bytes)
                    <= alt.max_packet_bytes as u64;
            let source_sample = match source.encoding {
                Encoding::PcmFloat32 | Encoding::DoP | Encoding::DsdNative { .. } => {
                    Some(SampleFormat::Float32)
                }
                Encoding::Pcm => match source.bit_depth {
                    16 => Some(SampleFormat::I16),
                    24 => Some(SampleFormat::I24),
                    32 => Some(SampleFormat::I32),
                    _ => None,
                },
                _ => None,
            };
            let plan_valid = source_sample
                .zip(SampleFormat::int(alt.valid_bits, alt.subslot_bytes).ok())
                .is_some_and(|(from, to)| {
                    let mut opts = ConvertOptions::default();
                    if candidate == rate {
                        opts.headroom_db = 0.0;
                    }
                    ConversionPlan::pcm(
                        PcmFormat::new(rate, source.channels, from),
                        PcmFormat::new(candidate, source.channels, to),
                        &opts,
                    )
                    .is_ok()
                });
            if (MIN_RATE..=MAX_RATE).contains(&candidate)
                && geometry
                && plan_valid
                && !(source.encoding == Encoding::Pcm
                    && candidate == source.sample_rate
                    && alt.valid_bits == source.bit_depth)
            {
                pairs.push((candidate, alt.valid_bits));
            }
        }
    }
    rank_pairs(&mut pairs, rate, source_depth);
    pairs
        .into_iter()
        .map(|(hz, depth)| StreamRequest::pcm(hz, source.channels, depth))
        .take(8)
        .collect()
}

fn rank_pairs(pairs: &mut Vec<(u32, u8)>, rate: u32, source_depth: u8) {
    pairs.sort_unstable();
    pairs.dedup();
    let mut rates: Vec<_> = pairs.iter().map(|p| p.0).collect();
    usbdac_core::rank_sample_rates(&mut rates, rate);
    pairs.sort_by_key(|(hz, depth)| {
        (
            rates.iter().position(|v| v == hz).unwrap_or(usize::MAX),
            u8::from(*depth < source_depth),
            depth.abs_diff(source_depth),
        )
    });
}

enum Engine {
    Pcm(Converter),
    Dsd(DsdPcmConverter),
}
impl Engine {
    fn process(
        &mut self,
        input: &[u8],
        output: &mut [u8],
    ) -> Result<usbdac_core::convert::Processed, usbdac_core::convert::ConvertError> {
        match self {
            Self::Pcm(c) => c.process(input, output),
            Self::Dsd(c) => c.process(input, output),
        }
    }
    fn finish(&mut self, output: &mut [u8]) -> Result<usize, usbdac_core::convert::ConvertError> {
        match self {
            Self::Pcm(c) => c.finish(output),
            Self::Dsd(c) => c.finish(output),
        }
    }
    fn clipped(&self) -> u64 {
        match self {
            Self::Pcm(c) => c.clipped_samples(),
            Self::Dsd(c) => c.clipped_samples(),
        }
    }
    fn min_output(&self) -> usize {
        match self {
            Self::Pcm(c) => c.min_output_bytes(),
            Self::Dsd(c) => c.min_output_bytes(),
        }
    }
}

/// Validate genuine alternating DoP markers and convert carrier order into native chronological bytes.
fn unpack_dop(
    input: &[u8],
    channels: usize,
    slot: usize,
    marker_phase: &mut bool,
) -> Result<Vec<u8>, String> {
    let frame = channels * slot;
    if frame == 0 || ![3, 4].contains(&slot) || !input.len().is_multiple_of(frame) {
        return Err("invalid DoP input frame length".into());
    }
    let mut output = Vec::with_capacity(input.len() / slot * 2);
    for group in input.chunks_exact(frame) {
        let marker = if *marker_phase { 0xfa } else { 0x05 };
        if group
            .chunks_exact(slot)
            .any(|word| word[slot - 1] != marker || (slot == 4 && word[0] != 0))
        {
            return Err("invalid DoP marker sequence".into());
        }
        for word in group.chunks_exact(slot) {
            output.push(word[slot - 2]);
        }
        for word in group.chunks_exact(slot) {
            output.push(word[slot - 3]);
        }
        *marker_phase = !*marker_phase;
    }
    Ok(output)
}
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
    caps: &Capabilities,
    source: &StreamRequest,
    options: &RunOptions,
    cancel: &AtomicBool,
    device_lost: &AtomicBool,
) -> Value {
    let mut evidence = json!({"source_request":format!("{source:?}"), "resolver":"portable direct USB validator policy; not a platform OutputSelector route; DoP conversion is explicitly requested for validation", "status":"no_fallback", "source_fidelity":"not_tested", "target_wire_integrity":"not_tested", "dop_decoder":"not_observed"});
    let Some(rate) = source_rate(source) else {
        let reason = if source.encoding == Encoding::PcmU8 {
            "U8 source conversion is not provided by the released PCM converter"
        } else if matches!(source.encoding, Encoding::DoP) && source.bit_depth != 24 {
            "DoP fallback accepts canonical 24-bit carrier input only"
        } else if !(1..=8).contains(&source.channels) {
            "Released converters support one to eight channels"
        } else {
            "Source rate is outside the released PCM or DSD64/128/256 converter support"
        };
        evidence["reason"] = json!(reason);
        return evidence;
    };
    let targets = candidates(caps, source, rate);
    evidence["candidate_requests"] =
        json!(targets.iter().map(|t| format!("{t:?}")).collect::<Vec<_>>());
    if targets.is_empty() {
        evidence["reason"] = json!("No descriptor-proven signed PCM target meets converter rate, channel and packet geometry requirements");
        return evidence;
    }
    if let Some(reason) = stop_reason(cancel, device_lost) {
        evidence["termination_reason"] = json!(reason);
        evidence["status"] = json!(reason);
        return evidence;
    }
    // One bounded, driver-owned negotiation preserves the retained USB owner.
    let format = match dac.negotiate_candidates(&targets) {
        Ok(format) => format,
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
            evidence["error"] = json!({"stage":"target_negotiate", "code":error.code(), "message":error.to_string()});
            return evidence;
        }
    };
    let Some(target) = targets
        .iter()
        .find(|candidate| exact_format(candidate, &format))
        .copied()
    else {
        evidence["status"] = json!("inconclusive");
        evidence["error"] = json!({"stage":"target_negotiate", "code":"format_drift", "message":"Negotiated format is not an exact candidate"});
        evidence["wire_format"] = format_json(&format);
        return evidence;
    };
    evidence["target_request"] = json!(format!("{target:?}"));
    evidence["wire_format"] = format_json(&format);
    let mut conversion_options = ConvertOptions::default();
    let is_dsd = matches!(source.encoding, Encoding::DsdNative { .. } | Encoding::DoP);
    if rate == format.sample_rate {
        conversion_options.headroom_db = 0.0;
    }
    evidence["conversion_options"] = json!(format!("{conversion_options:?}"));
    let engine = if is_dsd {
        let bit_rate = match source.encoding {
            Encoding::DsdNative { bit_rate } => bit_rate,
            _ => source.sample_rate * 16,
        };
        let sample = match SampleFormat::int(format.valid_bits, format.subslot_bytes) {
            Ok(v) => v,
            Err(e) => {
                evidence["status"] = json!("inconclusive");
                evidence["error"] =
                    json!({"stage":"target_layout", "code":e.code(), "message":e.to_string()});
                return evidence;
            }
        };
        let target_pcm = PcmFormat::new(format.sample_rate, format.channels, sample);
        match DsdPcmConverter::with_options(
            bit_rate,
            source.channels,
            target_pcm,
            &conversion_options,
        ) {
            Ok(c) => {
                evidence["conversion"] = json!({"kind":"Lossy", "description":"DSD to PCM decimation and PCM conversion", "source_fidelity":"lossy"});
                Engine::Dsd(c)
            }
            Err(e) => {
                evidence["status"] = json!("inconclusive");
                evidence["error"] =
                    json!({"stage":"converter", "code":e.code(), "message":e.to_string()});
                return evidence;
            }
        }
    } else {
        match ConversionPlan::between(source, &format, &conversion_options) {
            Ok(plan) => {
                evidence["conversion"] = json!({"kind":format!("{:?}", plan.kind()), "description":plan.description(), "preserves_bit_perfect":plan.preserves_bit_perfect()});
                match Converter::new(&plan) {
                    Ok(c) => Engine::Pcm(c),
                    Err(e) => {
                        evidence["status"] = json!("inconclusive");
                        evidence["error"] =
                            json!({"stage":"converter", "code":e.code(), "message":e.to_string()});
                        return evidence;
                    }
                }
            }
            Err(e) => {
                evidence["status"] = json!("inconclusive");
                evidence["error"] =
                    json!({"stage":"plan", "code":e.code(), "message":e.to_string()});
                return evidence;
            }
        }
    };
    evidence["source_fidelity"] = if is_dsd {
        json!("lossy")
    } else {
        json!(evidence["conversion"]["kind"])
    };
    if let Some(reason) = stop_reason(cancel, device_lost) {
        evidence["termination_reason"] = json!(reason);
        evidence["status"] = json!(reason);
        return evidence;
    }
    let lost = Arc::new(AtomicBool::new(false));
    let listener_lost = lost.clone();
    let async_error: Arc<Mutex<Option<(String, String)>>> = Arc::new(Mutex::new(None));
    let listener_error = async_error.clone();
    let stream = match dac.open_stream(
        &format,
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
        Ok(s) => s,
        Err(e) => {
            evidence["status"] = json!(if is_device_lost(&e) {
                "device_lost"
            } else {
                "failed"
            });
            if is_device_lost(&e) {
                evidence["termination_reason"] = json!("device_lost");
            }
            evidence["error"] = json!({"stage":"open", "code":e.code(), "message":e.to_string()});
            return evidence;
        }
    };
    let mut engine = engine;
    let (encoding, sample_rate, subslot, bits) = match source.encoding {
        Encoding::DsdNative { bit_rate } => (source.encoding, bit_rate / 8, 1, 1),
        Encoding::DoP => (
            source.encoding,
            source.sample_rate,
            if source.bit_depth == 32 { 4 } else { 3 },
            source.bit_depth,
        ),
        Encoding::PcmFloat32 => (source.encoding, source.sample_rate, 4, 32),
        _ => (
            source.encoding,
            source.sample_rate,
            source.bit_depth / 8,
            source.bit_depth,
        ),
    };
    let mut signal = Signal::from_spec(encoding, sample_rate, source.channels, subslot, bits);
    let mut remaining = u64::from(sample_rate) * options.duration_ms.clamp(300, 10000) / 1000;
    let source_frames = remaining;
    let converter_input_frame_bytes = if is_dsd {
        u64::from(source.channels)
    } else {
        u64::from(source.channels) * u64::from(subslot)
    };
    let converter_input_frames = if matches!(encoding, Encoding::DoP) {
        source_frames * 2
    } else {
        source_frames
    };
    let converter_rate = if is_dsd {
        u64::from(rate) * 2
    } else {
        u64::from(rate)
    };
    let expected_input = converter_input_frames * converter_input_frame_bytes;
    let expected_frames = (u128::from(converter_input_frames) * u128::from(format.sample_rate))
        .div_ceil(u128::from(converter_rate)) as u64;
    let expected_output = expected_frames * format.bytes_per_frame() as u64;
    let mut source_hash = Sha256::new();
    let mut output_hash = Sha256::new();
    let mut produced_bytes = 0u64;
    let mut input_bytes = 0u64;
    let mut source_framed_bytes = 0u64;
    let mut output_bytes = 0u64;
    let mut marker_phase = false;
    let mut read_phase = false;
    let mut output = vec![0u8; 65536usize.max(engine.min_output())];
    let deadline = Instant::now() + Duration::from_millis(options.leg_timeout_ms.max(100));
    let mut failure: Option<(String, String)> = None;
    let mut failure_stage = "stream";
    let mut finishing = false;
    let mut source_bytes = Vec::new();
    let mut offset = 0usize;
    loop {
        if let Some(error) = async_error.lock().ok().and_then(|slot| slot.clone()) {
            failure_stage = "usb_event";
            failure = Some(error);
            break;
        }
        if lost.load(Ordering::Acquire) || device_lost.load(Ordering::Acquire) {
            failure = Some(("device_lost".into(), "Selected device was lost".into()));
            break;
        }
        if cancel.load(Ordering::Acquire) {
            failure = Some(("cancelled".into(), "cancelled".into()));
            break;
        }
        if Instant::now() >= deadline {
            failure = Some(("timeout".into(), "fallback deadline exceeded".into()));
            break;
        }
        if offset == source_bytes.len() && remaining > 0 {
            let count = remaining.min(256) as usize;
            remaining -= count as u64;
            let mut bytes = signal.frames(count);
            source_framed_bytes += bytes.len() as u64;
            if matches!(encoding, Encoding::DoP) {
                emit_dop_markers(
                    &mut bytes,
                    source.channels as usize,
                    subslot as usize,
                    &mut marker_phase,
                );
                source_hash.update(&bytes);
                match unpack_dop(
                    &bytes,
                    source.channels as usize,
                    subslot as usize,
                    &mut read_phase,
                ) {
                    Ok(v) => bytes = v,
                    Err(e) => {
                        failure_stage = "source_framing";
                        failure = Some(("dop_framing".into(), e));
                        break;
                    }
                }
            } else {
                source_hash.update(&bytes);
            }
            source_bytes = bytes;
            offset = 0;
        }
        if offset == source_bytes.len() && remaining == 0 {
            finishing = true;
        }
        let converted = if finishing {
            engine
                .finish(&mut output)
                .map(|produced| usbdac_core::convert::Processed {
                    consumed: 0,
                    produced,
                })
        } else {
            engine.process(&source_bytes[offset..], &mut output)
        };
        let converted = match converted {
            Ok(v) => v,
            Err(e) => {
                failure_stage = if finishing {
                    "converter_finish"
                } else {
                    "converter_process"
                };
                failure = Some((e.code().into(), e.to_string()));
                break;
            }
        };
        offset += converted.consumed;
        produced_bytes += converted.produced as u64;
        input_bytes += converted.consumed as u64;
        if !finishing && converted.consumed == 0 && converted.produced == 0 {
            failure_stage = "converter_process";
            failure = Some((
                "conversion_stalled".into(),
                "Converter made no progress".into(),
            ));
            break;
        }
        if finishing && converted.produced == 0 {
            break;
        }
        let mut written = 0;
        while written < converted.produced {
            if let Some(error) = async_error.lock().ok().and_then(|slot| slot.clone()) {
                failure_stage = "usb_event";
                failure = Some(error);
                break;
            }
            if lost.load(Ordering::Acquire) || device_lost.load(Ordering::Acquire) {
                failure = Some(("device_lost".into(), "Selected device was lost".into()));
                break;
            }
            if cancel.load(Ordering::Acquire) {
                failure = Some(("cancelled".into(), "cancelled".into()));
                break;
            }
            if Instant::now() >= deadline {
                failure_stage = "write";
                failure = Some(("timeout".into(), "fallback write deadline exceeded".into()));
                break;
            }
            match stream.write(&output[written..converted.produced]) {
                Ok(0) => std::thread::sleep(Duration::from_millis(2)),
                Ok(n) => {
                    output_hash.update(&output[written..written + n]);
                    written += n;
                    output_bytes += n as u64;
                }
                Err(e) => {
                    failure_stage = "write";
                    failure = Some((
                        if is_device_lost(&e) {
                            "device_lost".into()
                        } else {
                            e.code().into()
                        },
                        e.to_string(),
                    ));
                    break;
                }
            }
        }
        if failure.is_some() {
            break;
        }
    }
    if failure.is_none() {
        if let Err(e) = stream.drain() {
            failure_stage = "drain";
            failure = Some((
                if is_device_lost(&e) {
                    "device_lost".into()
                } else {
                    e.code().into()
                },
                e.to_string(),
            ));
        }
    }
    while failure.is_none() && !stream.is_drained() {
        if let Some(error) = async_error.lock().ok().and_then(|slot| slot.clone()) {
            failure_stage = "usb_event";
            failure = Some(error);
        } else if lost.load(Ordering::Acquire) || device_lost.load(Ordering::Acquire) {
            failure = Some(("device_lost".into(), "Selected device was lost".into()));
        } else if cancel.load(Ordering::Acquire) {
            failure = Some(("cancelled".into(), "cancelled".into()));
        } else if Instant::now() >= deadline {
            failure_stage = "drain";
            failure = Some(("timeout".into(), "fallback drain deadline exceeded".into()));
        } else {
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    if failure.is_none() && (lost.load(Ordering::Acquire) || device_lost.load(Ordering::Acquire)) {
        failure = Some(("device_lost".into(), "Selected device was lost".into()));
    }
    if failure.is_none() {
        if let Some(error) = async_error.lock().ok().and_then(|slot| slot.clone()) {
            failure_stage = "usb_event";
            failure = Some(error);
        }
    }
    let stats = stream.stats();
    let drained = stream.is_drained();
    let counts_match = input_bytes == expected_input
        && output_bytes == expected_output
        && produced_bytes == output_bytes
        && output_bytes > 0
        && output_bytes.is_multiple_of(format.bytes_per_frame() as u64)
        && stats.frames_written == output_bytes / format.bytes_per_frame() as u64
        && stats.played_frames == stats.frames_written
        && stats.completed_transfers > 0
        && drained;
    let pass = counts_match
        && failure.is_none()
        && stats.bit_perfect
        && stats.payload_verified
        && stats.clock_verified
        && stats.lost_frames == 0
        && stats.underruns == 0
        && stats.short_packets == 0
        && stats.zero_packets == 0
        && stats.urb_timeouts == 0
        && stats.played_frames == stats.frames_written;
    evidence["status"] = json!(if pass {
        "passed_usb"
    } else if failure.is_none() && counts_match {
        "inconclusive"
    } else {
        "failed"
    });
    evidence["target_wire_integrity"] = json!(if pass { "verified" } else { "unverified" });
    evidence["digests"] = json!({"source_generated_sha256":format!("{:x}", source_hash.finalize()), "target_written_sha256":format!("{:x}", output_hash.finalize())});
    evidence["counts"] = json!({"expected_converter_input_bytes":expected_input, "expected_target_frames":expected_frames, "expected_target_bytes":expected_output, "converter_output_bytes":produced_bytes, "drained":drained, "completed_transfers":stats.completed_transfers, "source_framed_bytes":source_framed_bytes, "converter_input_bytes":input_bytes, "target_bytes_written":output_bytes, "clipped_samples":engine.clipped(), "frames_written":stats.frames_written, "played_frames":stats.played_frames, "lost_frames":stats.lost_frames, "underruns":stats.underruns, "payload_verified":stats.payload_verified, "clock_verified":stats.clock_verified, "feedback_locked":stats.feedback_locked, "stats_debug":format!("{stats:?}")});
    if let Some((ref code, ref message)) = failure {
        if code == "device_lost" || code == "cancelled" {
            evidence["termination_reason"] = json!(code);
            evidence["status"] = json!(code);
        }
        evidence["error"] = json!({"stage":failure_stage,"code":code,"message":message});
    }
    if failure.is_none() && !counts_match {
        evidence["error"] = json!({"stage":"evidence", "code":"count_mismatch", "message":"Converter or completed USB frame counts did not match expected output"});
    }
    let close = stream.close();
    evidence["close"] = json!(format!("{close:?}"));
    if let Err(e) = close {
        if is_device_lost(&e) {
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

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn production_rate_order_and_sufficient_depth_are_preserved() {
        let mut pairs = vec![
            (48_000, 16),
            (96_000, 16),
            (48_000, 32),
            (48_000, 24),
            (192_000, 24),
        ];
        rank_pairs(&mut pairs, 48_000, 24);
        assert_eq!(
            pairs,
            vec![
                (48_000, 24),
                (48_000, 32),
                (48_000, 16),
                (96_000, 16),
                (192_000, 24)
            ]
        );
    }
    #[test]
    fn conversion_plan_distinguishes_bit_exact_widening_and_resample() {
        let source = PcmFormat::new(48_000, 2, SampleFormat::I16);
        let wide = ConversionPlan::pcm(
            source,
            PcmFormat::new(48_000, 2, SampleFormat::I24),
            &ConvertOptions::default(),
        )
        .unwrap();
        assert!(wide.preserves_bit_perfect());
        let resampled = ConversionPlan::pcm(
            source,
            PcmFormat::new(96_000, 2, SampleFormat::I24),
            &ConvertOptions::default(),
        )
        .unwrap();
        assert!(!resampled.preserves_bit_perfect());
    }
    #[test]
    fn pcm_engine_process_and_finish_preserve_widened_samples() {
        let source = PcmFormat::new(48_000, 1, SampleFormat::I16);
        let target = PcmFormat::new(48_000, 1, SampleFormat::I24);
        let plan = ConversionPlan::pcm(source, target, &ConvertOptions::default()).unwrap();
        let mut engine = Engine::Pcm(Converter::new(&plan).unwrap());
        let input = [0x34, 0x12, 0xfe, 0xff];
        let mut output = [0u8; 64];
        let result = engine.process(&input, &mut output).unwrap();
        assert_eq!((result.consumed, result.produced), (4, 6));
        assert_eq!(&output[..6], &[0x00, 0x34, 0x12, 0x00, 0xfe, 0xff]);
        assert_eq!(engine.finish(&mut output).unwrap(), 0);
        assert_eq!(engine.clipped(), 0);
    }
    #[test]
    fn dsd_engine_process_and_finish_produce_expected_frames() {
        let target = PcmFormat::new(176_400, 2, SampleFormat::I24);
        let mut engine = Engine::Dsd(DsdPcmConverter::new(2_822_400, 2, target).unwrap());
        let input = vec![0x69; 512];
        let mut output = vec![0u8; 16_384];
        let step = engine.process(&input, &mut output).unwrap();
        assert_eq!(step.consumed, input.len());
        let mut produced = step.produced;
        loop {
            let count = engine.finish(&mut output).unwrap();
            if count == 0 {
                break;
            }
            produced += count;
        }
        assert_eq!(produced, 128 * target.bytes_per_frame());
    }
    #[test]
    fn dop_marker_validation_restores_chronological_interleaving() {
        let input = [
            0x12, 0x34, 0x05, 0x56, 0x78, 0x05, 0x9a, 0xbc, 0xfa, 0xde, 0xf0, 0xfa,
        ];
        let mut phase = false;
        assert_eq!(
            unpack_dop(&input, 2, 3, &mut phase).unwrap(),
            [0x34, 0x78, 0x12, 0x56, 0xbc, 0xf0, 0x9a, 0xde]
        );
        assert!(!phase);
        let mut invalid = input;
        invalid[8] = 0x05;
        let mut initial_phase = false;
        assert!(unpack_dop(&invalid, 2, 3, &mut initial_phase).is_err());
    }
}
