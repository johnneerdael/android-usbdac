use super::*;

/// Bootstrap the exact PLAYBACK_FIRST pair without touching application PCM,
/// playback position, loss accounting, or payload verification.
pub(super) fn nominal_pcm_silence(scheduler: &mut IsoPacketScheduler, max_packet: usize,
    stride: usize, epoch: u64) -> Result<IsoTransferPayload, String> {
    let packet_sizes = scheduler.next_transfer_packet_bytes();
    if stride == 0 || packet_sizes.is_empty() || packet_sizes.iter().any(|&n| n == 0 || n > max_packet || n % stride != 0) {
        return Err("invalid PLAYBACK_FIRST nominal silence geometry".into());
    }
    let bytes = packet_sizes.iter().sum();
    Ok(IsoTransferPayload { packet_sizes, transfer_buffer: vec![0; bytes], packet_samples: vec![],
        underrun: false, held: true, frame_count: 0, music_frames: 0, epoch })
}

pub(super) fn retire_bootstrap(slot: &mut IsoTransferSlot, acceptance: PacketAcceptance) -> Result<bool, Error> {
    retire_bootstrap_completion(slot, acceptance).map(|completion| completion.is_some())
}

pub(super) fn retire_bootstrap_completion(slot: &mut IsoTransferSlot, acceptance: PacketAcceptance) -> Result<Option<Instant>, Error> {
    let Some((status, completed, _)) = slot.take_completion() else { return Ok(None); };
    let result = if status == LIBUSB_TRANSFER_COMPLETED {
        slot.validate_packets(acceptance).map_err(Error::Transfer).and_then(|stats| if stats == CompletionStats::default() {
            Ok(())
        } else { Err(Error::Transfer("PLAYBACK_FIRST silence was not accepted unchanged".into())) })
    } else if status == LIBUSB_TRANSFER_NO_DEVICE {
        Err(Error::DeviceLost)
    } else { Err(Error::Transfer(format!("PLAYBACK_FIRST silence transfer status {status}"))) };
    slot.mark_completed();
    result.map(|()| Some(completed))
}

pub(super) fn prepare_iso_transfer_payload(
    scheduler: &mut IsoPacketScheduler,
    pcm_buffer: &Arc<Mutex<PcmRing>>,
    runtime_stats: &RuntimeStats,
    candidate: &IsoStreamCandidate,
    slot_bytes: usize,
    channels: usize,
    bytes_per_frame: usize,
    dsd_transport: DsdTransportMode,
    dsd_big_endian: bool,
    dsd_bit_reverse: bool,
) -> Result<Option<IsoTransferPayload>, String> {
    let paused = runtime_stats.paused.load(Ordering::Acquire);
    let draining = runtime_stats.draining.load(Ordering::Acquire);
    let gain = runtime_stats.software_gain();
    let packet_sizes = scheduler.next_transfer_packet_bytes();
    let total_bytes: usize = packet_sizes.iter().sum();
    if total_bytes == 0 {
        return Ok(None);
    }

    if packet_sizes
        .iter()
        .any(|packet_size| *packet_size > candidate.max_packet_bytes)
    {
        return Err(format!(
            "USB packet exceeds endpoint max packet size: packets={:?}, endpoint_max={}",
            packet_sizes, candidate.max_packet_bytes
        ));
    }

    if packet_sizes
        .iter()
        .any(|packet_size| packet_size % bytes_per_frame != 0)
    {
        return Err(format!(
            "USB packet is not aligned to {}-byte transport frames: {:?}",
            bytes_per_frame, packet_sizes
        ));
    }

    let total_samples = if dsd_transport == DsdTransportMode::Native {
        total_bytes
    } else {
        total_bytes / slot_bytes
    };
    let mut packet_samples = vec![0i32; total_samples];
    // Held transfers carry wire silence but keep the iso stream (and the DAC's
    // clock lock) running. They neither consume the ring nor count as an
    // underrun, and they skip the payload audit: silence is expected there and
    // must not clear the verified state.
    let (underrun, held, music_samples, epoch) = {
        let mut guard = pcm_buffer.lock();
        // After a flush, wait for the same small threshold that starts the
        // stream (never less than one whole transfer), not the deep target.
        // A draining stream plays out whatever is left, however short.
        let refill_samples = if draining {
            1
        } else {
            total_samples.max(runtime_stats.start_threshold_samples)
        };
        let held = paused || guard.awaiting_refill(refill_samples);
        let (underrun, held, music_samples) = if held {
            guard.fill_wire_silence(&mut packet_samples, dsd_transport);
            (false, true, 0)
        } else {
            let pop = guard.pop_music_into_or_pad(&mut packet_samples, channels, dsd_transport);
            // After drain() the padded tail is expected, not an underrun, and a
            // transfer with no music left in it is just held silence.
            (
                pop.underrun && !draining,
                draining && pop.music_samples == 0,
                pop.music_samples,
            )
        };
        // Account the music as in flight before the ring shrinks, so a reader
        // that sees the empty ring also sees the music on the bus.
        runtime_stats
            .inflight_music_frames
            .fetch_add((music_samples / channels.max(1)) as u64, Ordering::Release);
        runtime_stats
            .buffered_samples
            .store(guard.len_samples(), Ordering::Release);
        (underrun, held, music_samples, guard.flush_epoch())
    };
    runtime_stats.last_transfer_held.store(held, Ordering::Release);
    runtime_stats
        .last_transfer_underrun
        .store(underrun, Ordering::Release);
    if gain != 1.0 && dsd_transport == DsdTransportMode::None && music_samples > 0 {
        apply_software_gain(&mut packet_samples[..music_samples], gain);
    }

    let mut transfer_buffer = vec![0u8; total_bytes];
    encode_usb_pcm_slots(
        &packet_samples,
        &mut transfer_buffer,
        candidate.subslot_size,
        candidate.bit_resolution,
        dsd_transport,
        candidate.channels,
        dsd_big_endian,
        dsd_bit_reverse,
    )?;
    if held {
        // Expected silence: leave the audit latch alone.
    } else if dsd_transport != DsdTransportMode::None {
        observe_dsd_payload_audit(audit_dsd_payload(
            &transfer_buffer,
            dsd_transport,
            candidate.subslot_size,
            candidate.channels,
        ));
    } else {
        observe_pcm_payload_audit(audit_pcm_payload(
            &transfer_buffer,
            candidate.subslot_size,
            candidate.channels,
        ));
    }

    Ok(Some(IsoTransferPayload {
        frame_count: packet_samples.len() / channels,
        packet_sizes,
        packet_samples,
        transfer_buffer,
        underrun,
        held,
        music_frames: music_samples / channels.max(1),
        epoch,
    }))
}

/// Publish how much music is submitted to the bus and not yet completed, for
/// transfers filled since the last flush. Called by the output thread, which
/// owns the slots, after it has handled a batch of completions and submissions.
pub(super) fn publish_inflight_music(slots: &[IsoTransferSlot], runtime_stats: &RuntimeStats) {
    let epoch = runtime_stats.flush_epoch.load(Ordering::Acquire);
    let inflight = inflight_music_frames(
        epoch,
        slots
            .iter()
            .filter(|slot| slot.in_flight)
            .map(|slot| (slot.queued_epoch, slot.queued_music_frames as u64)),
    );
    runtime_stats
        .inflight_music_frames
        .store(inflight, Ordering::Relaxed);
}

pub(super) fn publish_transport_loss(runtime_stats: &RuntimeStats, tracker: &LossTracker) {
    runtime_stats
        .lost_frames
        .store(tracker.lost_frames(), Ordering::Relaxed);
    runtime_stats
        .short_packets
        .store(tracker.short_packets(), Ordering::Relaxed);
    runtime_stats
        .zero_packets
        .store(tracker.zero_packets(), Ordering::Relaxed);
    runtime_stats
        .data_urb_timeouts
        .store(tracker.data_urb_timeouts(), Ordering::Relaxed);
    if tracker.is_latched() {
        runtime_stats
            .transport_loss_latched
            .store(true, Ordering::Relaxed);
    }
}

/// A playback completion advances an adaptive source's USB clock only when
/// every submitted packet was accepted unchanged. Held wire silence (pause or
/// flush refill) qualifies by the same geometry as music.
pub(super) fn qualifies_implicit_feedforward_completion(
    status: i32,
    stats: &CompletionStats,
    packet_sizes: &[usize],
    bytes_per_frame: usize,
    queued_frames: u64,
) -> bool {
    status == LIBUSB_TRANSFER_COMPLETED
        && *stats == CompletionStats::default()
        && queued_frames > 0
        && bytes_per_frame > 0
        && packet_sizes.iter().any(|&bytes| bytes > 0)
        && packet_sizes.iter().all(|bytes| bytes % bytes_per_frame == 0)
}

pub(super) fn update_transfer_timing_stats(
    runtime_stats: &RuntimeStats,
    turnaround_us: u64,
    submit_gap_us: u64,
    stream_started_at: Instant,
) {
    runtime_stats
        .last_transfer_turnaround_us
        .store(turnaround_us, Ordering::Relaxed);
    if turnaround_us
        > runtime_stats
            .max_transfer_turnaround_us
            .load(Ordering::Relaxed)
    {
        runtime_stats
            .max_transfer_turnaround_us
            .store(turnaround_us, Ordering::Relaxed);
    }

    runtime_stats
        .last_submit_gap_us
        .store(submit_gap_us, Ordering::Relaxed);
    if submit_gap_us > runtime_stats.max_submit_gap_us.load(Ordering::Relaxed) {
        runtime_stats
            .max_submit_gap_us
            .store(submit_gap_us, Ordering::Relaxed);
    }

    let elapsed_us = stream_started_at.elapsed().as_micros().max(1) as u64;
    let completed_frames = runtime_stats.consumer_frames.load(Ordering::Relaxed);
    let effective_consumer_rate_milli_hz = completed_frames
        .saturating_mul(1_000_000_000)
        .checked_div(elapsed_us)
        .unwrap_or(0);
    runtime_stats
        .effective_consumer_rate_milli_hz
        .store(effective_consumer_rate_milli_hz, Ordering::Relaxed);
}

pub(super) fn drain_iso_transfer_slots(context: &Context, slots: &mut [IsoTransferSlot]) {
    for slot in slots.iter_mut() {
        slot.cancel();
    }

    retire_cancelled_iso_transfer_slots(context, slots);
}

/// Retire a pool after its complete owner batch has requested cancellation.
pub(super) fn retire_cancelled_iso_transfer_slots(context: &Context, slots: &mut [IsoTransferSlot]) {
    let mut warned = false;
    while slots.iter().any(|slot| slot.in_flight) {
        if let Err(error) = context.handle_events(Some(Duration::from_millis(10))) {
            if !warned { log_warn!("USB cancellation retirement pending: {}", error); warned = true; }
            thread::sleep(Duration::from_millis(10));
        }
        for slot in slots.iter_mut() {
            if slot.take_completion().is_some() {
                slot.mark_completed();
            }
        }
    }


}

pub(super) fn drain_feedback_transfer_slot(context: &Context, slot: &mut Option<FeedbackTransferSlot>) {
    let Some(slot) = slot.as_mut() else {
        return;
    };
    slot.cancel();

    let mut warned = false;
    while slot.in_flight {
        if let Err(error) = context.handle_events(Some(Duration::from_millis(10))) {
            if !warned { log_warn!("USB feedback cancellation retirement pending: {}", error); warned = true; }
            thread::sleep(Duration::from_millis(10));
        }
        if slot.take_completion().is_some() {
            slot.mark_completed();
        }
    }


}

/// Cancels the transfers the flush left on the bus and waits until libusb has
/// handed every one back. Returns how many were cancelled (0 when the bus holds
/// nothing of the old position, see `flush_cancel_targets`).
///
/// The cancelled slots are only flagged here: their completions are taken by
/// the output loop, which recognises them (`classify_completion`), skips loss
/// and played accounting, and refills the slot. The buffers stay alive until
/// then. The feedback transfer is not touched: it is an IN transfer that
/// carries no audio, so there is nothing of the old position in it, and its
/// reports are as good after a flush as before.
pub(super) fn cancel_stale_transfers_for_flush(
    context: &Context,
    slots: &mut [IsoTransferSlot],
    current_epoch: u64,
) -> Result<usize, String> {
    let targets = flush_cancel_targets(
        current_epoch,
        slots
            .iter()
            .map(|slot| (slot.in_flight, slot.queued_epoch, slot.queued_music_frames)),
    );
    if targets.is_empty() {
        return Ok(0);
    }
    for &index in &targets {
        slots[index].cancel_for_flush();
    }
    let slots_ref: &[IsoTransferSlot] = slots;
    wait_for_acknowledgements(
        || {
            targets
                .iter()
                .filter(|&&index| !slots_ref[index].completion_ready())
                .count()
        },
        || {
            context
                .handle_events(Some(Duration::from_millis(10)))
                .map_err(|error| format!("libusb_handle_events failed: {}", error))
        },
        FLUSH_CANCEL_TIMEOUT,
    )?;
    Ok(targets.len())
}

/// Fills `slot` from the ring and submits it, as the output loop does for every
/// completed slot. `Ok(false)`: the scheduler had nothing to send.
pub(super) fn submit_next_transfer(
    slot: &mut IsoTransferSlot,
    handle: &DeviceHandle<Context>,
    scheduler: &mut IsoPacketScheduler,
    pcm_buffer: &Arc<Mutex<PcmRing>>,
    runtime_stats: &RuntimeStats,
    candidate: &IsoStreamCandidate,
    slot_bytes: usize,
    channels: usize,
    bytes_per_frame: usize,
    playback_format: PlaybackFormat,
    dsd_big_endian: bool,
    dsd_bit_reverse: bool,
    logged_transfer_preview: &mut bool,
    logged_usb_encode_format: &mut bool,
    underrun_count: &mut u64,
    shared: &StreamShared,
    was_underrun: &mut bool,
) -> Result<bool, String> {
    let payload = match prepare_iso_transfer_payload(
        scheduler,
        pcm_buffer,
        runtime_stats,
        candidate,
        slot_bytes,
        channels,
        bytes_per_frame,
        playback_format.dsd_transport,
        dsd_big_endian,
        dsd_bit_reverse,
    ) {
        Ok(Some(payload)) => payload,
        Ok(None) => return Ok(false),
        Err(error) => {
            return Err(format!("USB direct transfer preparation failed: {}", error));
        }
    };
    record_prepared_iso_transfer(
        &payload,
        playback_format,
        candidate,
        runtime_stats,
        bytes_per_frame,
        logged_transfer_preview,
        logged_usb_encode_format,
        underrun_count,
    );
    note_underrun_edge(shared, was_underrun, payload.underrun);
    let scheduled = payload.transfer_buffer.len() / bytes_per_frame;
    slot.submit(handle, candidate.endpoint_address, payload)
        .map_err(|error| format!("USB direct transfer failed: {}", error))?;
    if candidate.implicit_feedback.is_some() { runtime_stats.implicit_scheduled_frames.fetch_add(scheduled as u64, Ordering::Relaxed); }
    Ok(true)
}

pub(super) fn record_prepared_iso_transfer(
    payload: &IsoTransferPayload,
    playback_format: PlaybackFormat,
    candidate: &IsoStreamCandidate,
    runtime_stats: &RuntimeStats,
    bytes_per_frame: usize,
    logged_transfer_preview: &mut bool,
    logged_usb_encode_format: &mut bool,
    underrun_count: &mut u64,
) {
    if !*logged_usb_encode_format {
        log_info!(
            "[USB] PCM encoding: {}-bit (subslot {} bytes) per descriptor",
            candidate.bit_resolution,
            candidate.subslot_size,
        );
        *logged_usb_encode_format = true;
    }

    runtime_stats.frames_per_packet.store(
        payload.packet_sizes.first().copied().unwrap_or_default() / bytes_per_frame,
        Ordering::Relaxed,
    );

    if payload.underrun {
        *underrun_count = (*underrun_count).saturating_add(1);
        runtime_stats
            .underrun_count
            .store(*underrun_count, Ordering::Relaxed);
        if *underrun_count <= 4 || *underrun_count % 64 == 0 {
            log_info!(
                "[USB] underrun count={} buffer_fill={}ms frames_per_packet={} packets_per_transfer={}",
                *underrun_count,
                runtime_stats.buffer_fill_ms(),
                payload.packet_sizes.first().copied().unwrap_or_default() / bytes_per_frame,
                payload.packet_sizes.len(),
            );
        }
    }

    if !*logged_transfer_preview && payload.packet_samples.iter().any(|sample| *sample != 0) {
        log_stream_debug_preview(
            playback_format,
            candidate,
            &payload.packet_sizes,
            &payload.packet_samples,
            &payload.transfer_buffer,
        );
        *logged_transfer_preview = true;
    }
}

/// One `Underrun` event per run of underrun transfers.
pub(super) fn note_underrun_edge(shared: &StreamShared, was_underrun: &mut bool, underrun: bool) {
    if underrun && !*was_underrun {
        shared.events.emit_stream(crate::events::DacEvent::Underrun);
    }
    *was_underrun = underrun;
}

/// Copies the feedback loop's local counters into the shared statistics.
pub(super) fn publish_feedback_counters(stats: &RuntimeStats, counters: &FeedbackCounters) {
    stats.feedback_reports.store(counters.reports, Ordering::Relaxed);
    stats.feedback_empty.store(counters.empty_polls, Ordering::Relaxed);
    stats.feedback_timeouts.store(counters.timeouts, Ordering::Relaxed);
    stats.feedback_errors.store(counters.errors, Ordering::Relaxed);
    stats.feedback_rejected.store(counters.rejected, Ordering::Relaxed);
    if let Some(reported_at) = counters.last_report_at {
        let since_start = reported_at
            .saturating_duration_since(stats.started_at)
            .as_millis()
            .min(u128::from(u64::MAX - 1)) as u64;
        stats
            .feedback_last_report_ms
            .store(since_start, Ordering::Relaxed);
    }
    if let Some(rate) = counters.last_estimated_sample_rate {
        stats
            .feedback_rate_milli_hz
            .store((rate * 1_000.0).max(0.0) as u64, Ordering::Relaxed);
    }
}

fn publish_feedback_evidence(stats: &RuntimeStats, counters: &mut FeedbackCounters, final_snapshot: bool) {
    let Some(mut diagnostics) = counters.diagnostics else { return };
    if final_snapshot {
        diagnostics.accepted_reports = counters.reports;
        *stats.explicit_feedback_diagnostics.lock() = Some(diagnostics);
    } else if counters.diagnostics_dirty {
        if let Some(mut snapshot) = stats.explicit_feedback_diagnostics.try_lock() {
            diagnostics.accepted_reports = counters.reports;
            *snapshot = Some(diagnostics);
            counters.diagnostics_dirty = false;
        }
    }
}

pub(super) fn run_usb_output_loop(
    usb_owner: StreamUsbOwner,
    candidate: IsoStreamCandidate,
    state: SessionState,
    shared: Arc<StreamShared>,
    stop: Arc<AtomicBool>,
    capture: Option<Arc<duplex::Attachment>>,
    post_alt_readback_hz: Option<u32>,
) {
    let pcm_buffer = Arc::clone(&shared.ring);
    let runtime_stats = Arc::clone(&shared.stats);
    let claimed_handle = usb_owner.claimed();
    let context = &claimed_handle.context;
    let handle = &claimed_handle.handle;
    let playback_format = state.playback_format.unwrap();
    USB_DSD_PAYLOAD_LATCH.lock().reset();
    USB_PCM_PAYLOAD_LATCH.lock().reset();
    LAST_BIT_PERFECT_LOGGED.store(false, Ordering::Relaxed);
    // The profile belongs to this selected stream; mutable session settings
    // must never change the interpretation of queued DSD bytes.
    let dsd_big_endian = candidate.native_dsd.is_some_and(|p| p.big_endian);
    let dsd_bit_reverse = candidate.native_dsd.is_some_and(|p| p.bit_reverse);
    let slot_bytes = candidate.subslot_size as usize;
    if slot_bytes == 0 {
        let message = "the selected alternate setting has a zero-byte subslot".to_string();
        set_last_error(Some(message.clone()));
        set_engine_state(EngineState::Error, Some(message.clone()));
        shared.fail(Error::Transfer(message));
        drop(usb_owner);
        set_stream_active(false);
        return;
    }

    if let Err(error) = validate_transport_against_playback_format(&candidate, playback_format) {
        set_last_error(Some(error.clone()));
        set_engine_state(EngineState::Error, Some(error.clone()));
        shared.fail(Error::Transfer(error));
        drop(usb_owner);
        set_stream_active(false);
        return;
    }

    let (playback_first, programmed_vendor_rate) = match vendor_pcm::for_candidate(&handle, &candidate) {
        Ok(profile) => (profile.is_some(), profile.as_ref().is_some_and(|profile| profile.rate_control)),
        Err(message) => { set_last_error(Some(message.clone())); set_engine_state(EngineState::Error, Some(message.clone())); shared.fail(Error::Transfer(message)); set_stream_active(false); return; }
    };
    let channels = candidate.channels as usize;
    let bytes_per_frame = slot_bytes.saturating_mul(channels);
    // One rule for every service interval and encoding (PCM, DoP, native DSD):
    // the same time on the bus, never 256 packets in flight.
    let bus_config = settings().config.clone();
    let geometry = IsoQueueGeometry::for_stream(
        candidate.service_interval_us,
        bus_config.bus_queue_ms,
        bus_config.transfers_in_flight,
    );
    let mut scheduler = IsoPacketScheduler::with_geometry(
        playback_format.sample_rate,
        bytes_per_frame,
        candidate.service_interval_us,
        geometry,
    );
    let transfer_slot_count = geometry.transfer_slots;
    // One-shot: prove whether the sink links a feedback endpoint at all. If this
    // dump shows a FeedbackData IN endpoint that descriptor parsing missed, the
    // discovery path is wrong; if it shows none, host-side feedback is
    // impossible for this device and only bus-side cushioning can help.
    log_iso_endpoint_descriptors(&handle, &candidate);
    let feedback_endpoint = candidate.feedback_endpoint;
    let mut feedback_slot = None;
    let mut feedback_start_error = None;
    let feedback_per_service = settings().has(crate::Quirk::ExplicitFeedbackPerServiceInterval);
    if let Some(endpoint) = feedback_endpoint {
        if candidate.sync_type == SyncType::Asynchronous {
            let device = handle.device();
            let compatibility = device.device_descriptor().map(|descriptor| {
                FeedbackCompatibility::for_device(
                    descriptor.vendor_id(),
                    descriptor.product_id(),
                    candidate.usb_speed,
                    endpoint,
                    playback_format.sample_rate,
                )
            }).unwrap_or_default();
            match FeedbackTransferSlot::new(endpoint, bus_config.bus_queue_ms, compatibility) {
                Ok(slot) => feedback_slot = Some(slot),
                Err(error) => {
                    feedback_start_error = Some(format!("explicit feedback allocation failed: {}", error));
                }
            }
        } else {
            dev_eprintln!(
                "[USB] Ignoring feedback endpoint 0x{:02x}: data endpoint sync type is {:?}, not asynchronous",
                endpoint.address,
                candidate.sync_type
            );
        }
    } else if candidate.sync_type == SyncType::Asynchronous && candidate.implicit_feedback.is_none() {
        let bus_cushion_ms = geometry.queued_us() / 1000;
        log_warn!(
            "[USB] Async sink exposes no feedback endpoint; host-paced nominal timing with {} \
             queued transfers (~{}ms bus cushion). Drift beyond that cushion will still drop out.",
            transfer_slot_count,
            bus_cushion_ms,
        );
    }
    runtime_stats
        .transfer_queue_depth
        .store(transfer_slot_count, Ordering::Relaxed);
    let mut logged_transfer_preview = false;
    let mut logged_usb_encode_format = false;
    // Lossless isochronous OUT transfers: libusb COMPLETED and the DAC took every
    // submitted byte. Not reset on underrun or loss.
    let mut successful_transfers = 0usize;
    let mut logged_usb_stabilized = false;
    let mut underrun_count = 0u64;
    let mut feedback_counters = FeedbackCounters::default();
    feedback_counters.trace = runtime_stats.feedback_trace.clone();
    feedback_counters.infer_service_interval = post_alt_readback_hz == Some(playback_format.sample_rate);
    let mut feedback_stale_warned = false;
    let feedback_health = feedback_endpoint.filter(|_| candidate.sync_type == SyncType::Asynchronous).map(|endpoint| ExplicitFeedbackHealth::new(
        endpoint.service_interval_us, candidate.is_uac1.then_some(endpoint.refresh)));
    let mut feedback_route = feedback_endpoint
        .filter(|_| feedback_health.is_some()
            && !feedback_per_service
            && feedback_counters.infer_service_interval
            && candidate.usb_speed == Speed::High
            && candidate.service_interval_us > 125)
        .map(|endpoint| FeedbackTimebaseRoute::new(&candidate, endpoint));
    if let Some(route) = feedback_route {
        feedback_counters.cached_service_interval = state.service_interval_feedback_routes.lock().contains(&route);
    }
    if let Some(endpoint) = feedback_endpoint.filter(|_| feedback_health.is_some()) {
        feedback_counters.diagnostics = Some(crate::ExplicitFeedbackDiagnostics {
            endpoint: endpoint.address,
            transfer_type: transfer_type_label(endpoint.transfer_type),
            link_speed: match candidate.usb_speed {
                Speed::Low => "low",
                Speed::Full => "full",
                Speed::High => "high",
                Speed::Super => "super",
                Speed::SuperPlus => "super_plus",
                _ => "unknown",
            },
            interface_number: candidate.interface_number,
            alt_setting: candidate.alt_setting,
            out_endpoint: candidate.endpoint_address,
            nominal_rate_hz: playback_format.sample_rate,
            post_alt_readback_hz,
            out_service_interval_us: candidate.service_interval_us,
            out_max_packet_bytes: candidate.max_packet_bytes,
            out_bytes_per_frame: bytes_per_frame,
            feedback_interval: endpoint.interval,
            feedback_refresh: endpoint.refresh,
            feedback_service_interval_us: endpoint.service_interval_us,
            feedback_max_packet_bytes: endpoint.max_packet_bytes,
            requested_packet_bytes: feedback_slot.as_ref().map_or(0, |slot| slot.packet_bytes),
            packets_per_transfer: feedback_slot.as_ref().map_or(0, |slot| slot.packet_count),
            transfer_timeout_ms: feedback_slot.as_ref().map_or(0, |slot| slot.timeout_ms),
            per_service_interval_quirk: feedback_per_service,
            ..Default::default()
        });
        feedback_counters.diagnostics_dirty = true;
        publish_feedback_evidence(&runtime_stats, &mut feedback_counters, false);
    }
    let mut min_active_slots = transfer_slot_count;
    // The flush epoch the bus was last checked against (see the cancel below).
    let mut handled_flush_epoch = runtime_stats.flush_epoch.load(Ordering::Acquire);
    let acceptance = PacketAcceptance::new(
        playback_format.sample_rate,
        candidate.service_interval_us,
        bytes_per_frame,
    );
    // Lost audio is counted in the unit `consumer_frames` uses, which for native
    // DSD is one byte per channel rather than a transport frame.
    let ring_frame_bytes = ring_bytes_per_frame(
        playback_format.dsd_transport == DsdTransportMode::Native,
        channels,
        bytes_per_frame,
    );
    let mut loss_tracker = LossTracker::new(LossLimits {
        max_consecutive_lossy_urbs: geometry.max_consecutive_lossy_urbs(),
        max_lost_frames: lost_frame_budget(
            runtime_stats.sample_rate,
            USB_MAX_LOST_AUDIO_MS,
        ),
    });
    // Separate gates so benign repacketisation cannot use up the budget for
    // lines that report real loss.
    let mut played_music = PlayedMusicCounter::default();
    let mut loss_log_gate = LossLogGate::new(
        USB_LOSS_LOG_BURST,
        USB_LOSS_LOG_MIN_INTERVAL_MS,
    );
    let mut repacketised_log_gate = LossLogGate::new(
        USB_LOSS_LOG_BURST,
        USB_LOSS_LOG_MIN_INTERVAL_MS,
    );
    let transfer_buffer_capacity = scheduler
        .packets_per_transfer()
        .saturating_mul(candidate.max_packet_bytes.max(1));
    let mut slots = Vec::with_capacity(transfer_slot_count);
    let mut active_slots = 0usize;
    let mut explicit_feedback_fault = feedback_start_error.is_some();
    let mut out_flush_deadline: Option<Instant> = None;
    let mut fatal_error: Option<String> = feedback_start_error;
    let mut implicit_feedback_fault = false;
    let mut capture_failure: Option<Error> = None;
    let mut device_lost = false;
    let mut was_underrun = false;

    let priming_target = runtime_stats.start_threshold_samples;
    log_info!(
        "[USB] Isochronous queue: serviceIntervalUs={} packetsPerTransfer={} slots={} stableTransfers={} \
         packetsInFlight={} transferMs={:.3} busQueueMs={:.3} (configured {}ms in {} transfers) \
         maxConsecutiveLossyUrbs={}",
        candidate.service_interval_us,
        geometry.packets_per_transfer,
        geometry.transfer_slots,
        geometry.stable_transfers,
        geometry.packets_in_flight(),
        geometry.transfer_us() as f64 / 1000.0,
        geometry.queued_us() as f64 / 1000.0,
        bus_config.bus_queue_ms,
        bus_config.transfers_in_flight,
        geometry.max_consecutive_lossy_urbs(),
    );
    log_info!(
        "[USB] Waiting for stream prime: buffered_samples must reach start_threshold_samples={} before isochronous submit (the ring fills on to its {}-sample target while playing)",
        priming_target,
        runtime_stats.buffer_target_samples,
    );
    while fatal_error.is_none() && !usb_stop_requested(&stop) {
        let buffered = runtime_stats.buffered_samples.load(Ordering::Acquire);
        // drain() with a short tail: play it rather than wait for the threshold.
        if buffered >= priming_target
            || (buffered > 0 && runtime_stats.draining.load(Ordering::Acquire))
        {
            log_info!(
                "[USB] USB stream primed: {} samples buffered (target={})",
                buffered,
                priming_target,
            );
            break;
        }
        thread::sleep(Duration::from_millis(2));
    }

    for slot_index in 0..transfer_slot_count {
        if fatal_error.is_some() || usb_stop_requested(&stop) {
            break;
        }
        match IsoTransferSlot::new(
            slot_index,
            scheduler.packets_per_transfer(),
            transfer_buffer_capacity,
        ) {
            Ok(slot) => slots.push(slot),
            Err(error) => {
                fatal_error = Some(format!(
                    "USB direct failed to allocate transfer slot: {}",
                    error
                ));
                break;
            }
        }
    }

    // Only this exact validated vendor pair needs OUT service before IN.
    // Reuse normal slots and the nominal scheduler; callbacks stay on this owner.
    let mut bootstrap = vec![false; slots.len()];
    if playback_first && fatal_error.is_none() && !usb_stop_requested(&stop) {
        for (index, slot) in slots.iter_mut().enumerate() {
            let result = nominal_pcm_silence(&mut scheduler, candidate.max_packet_bytes, bytes_per_frame,
                runtime_stats.flush_epoch.load(Ordering::Acquire))
                .and_then(|payload| slot.submit(&handle, candidate.endpoint_address, payload));
            match result {
                Ok(()) => { bootstrap[index] = true; active_slots += 1; }
                Err(error) => { device_lost = matches!(classify_stream_failure(Some(&error), false, None), Some(Error::DeviceLost)); implicit_feedback_fault = true; fatal_error = Some(error); break; }
            }
        }
    }

    let stream_started_at = Instant::now();
    let mut capture_pool = None;
    if let Some(attachment) = capture.as_ref().filter(|a| !a.shared_implicit) {
        match duplex::InputPool::new(Arc::clone(attachment)) {
            Ok(mut pool) => {
                if let Err(error) = pool.start(&handle, &stop) {
                    device_lost = error == Error::DeviceLost; fatal_error = Some(error.to_string()); capture_failure = Some(error);
                }
                capture_pool = Some(pool);
            }
            Err(error) => fatal_error = Some(error),
        }
    }
    let capture_uses_implicit_feedforward = capture_pool.as_ref()
        .is_some_and(|pool| pool.uses_implicit_feedforward());
    // Duplex readiness requires a completed, fully valid adaptive-source
    // feedforward send, under this same USB event owner.
    while fatal_error.is_none() && !usb_stop_requested(&stop)
        && capture_pool.as_ref().is_some_and(|pool| !pool.primed()) {
        if let Err(error) = context.handle_events(Some(Duration::from_millis(1))) {
            let failure = if error == rusb::Error::NoDevice { Error::DeviceLost }
                else { Error::Transfer(format!("duplex feedforward event pump: {error}")) };
            device_lost = failure == Error::DeviceLost; fatal_error = Some(failure.to_string()); capture_failure = Some(failure); break;
        }
        if let Some(pool) = capture_pool.as_mut() {
            if let Err(error) = pool.pump(&handle, &stop) {
                device_lost = error == Error::DeviceLost; fatal_error = Some(error.to_string()); capture_failure = Some(error); break;
            }
        }
    }
    let mut implicit = None;
    if fatal_error.is_none() && !usb_stop_requested(&stop) {
        if let Some(source) = candidate.implicit_feedback {
            if let Some(trace) = &runtime_stats.feedback_trace {
                trace.record(crate::FeedbackTraceEvent::ImplicitSource {
                    interface_number: source.interface_number,
                    alt_setting: source.alt_setting,
                    endpoint: source.address,
                    service_interval_us: source.service_interval_us,
                    max_packet_bytes: source.max_packet_bytes,
                    bytes_per_frame: source.bytes_per_frame,
                });
            }
            match ImplicitFeedback::new(source, geometry, bytes_per_frame, candidate.max_packet_bytes, playback_format.sample_rate) {
                Ok(mut engine) => {
                    if programmed_vendor_rate { engine.require_vendor_rate_proof(); }
                    if !playback_first {
                        engine.attach_capture(capture.as_ref().filter(|a| a.shared_implicit).cloned());
                    }
                    log_info!("[USB] Implicit feedback source interface={} alt={} endpoint=0x{:02x} inStride={} outStride={} service={}us packetCapacity={}",
                        source.interface_number, source.alt_setting, source.address, source.bytes_per_frame, bytes_per_frame,
                        source.service_interval_us, source.max_packet_bytes);
                    if let Err(error) = engine.start(&handle, &runtime_stats, &stop) { implicit_feedback_fault = true; fatal_error = Some(error); }
                    while fatal_error.is_none() && !usb_stop_requested(&stop) && !engine.primed() {
                        if let Err(error) = context.handle_events(Some(Duration::from_micros(u64::from(candidate.service_interval_us).min(1000)))) {
                            device_lost = error == rusb::Error::NoDevice;
                            implicit_feedback_fault = true; fatal_error = Some(format!("implicit event pump failed: {}", error)); break;
                        }
                        if let Some(pool) = capture_pool.as_mut() {
                            if let Err(error) = pool.pump(&handle, &stop) {
                                device_lost = error == Error::DeviceLost; fatal_error = Some(error.to_string()); capture_failure = Some(error); break;
                            }
                        }
                        if playback_first && !usb_stop_requested(&stop) {
                            for (index, slot) in slots.iter_mut().enumerate() {
                                match retire_bootstrap(slot, acceptance) {
                                    Ok(true) => {
                                        active_slots = active_slots.saturating_sub(1);
                                        bootstrap[index] = false;
                                        let result = nominal_pcm_silence(&mut scheduler, candidate.max_packet_bytes, bytes_per_frame,
                                            runtime_stats.flush_epoch.load(Ordering::Acquire))
                                            .and_then(|payload| slot.submit(&handle, candidate.endpoint_address, payload));
                                        match result {
                                            Ok(()) => { active_slots += 1; bootstrap[index] = true; }
                                            Err(error) => { device_lost = matches!(classify_stream_failure(Some(&error), false, None), Some(Error::DeviceLost)); implicit_feedback_fault = true; fatal_error = Some(error); break; }
                                        }
                                    }
                                    Ok(false) => (),
                                    Err(error) => { device_lost = error == Error::DeviceLost; implicit_feedback_fault = true; fatal_error = Some(error.to_string()); break; }
                                }
                            }
                        }
                        if fatal_error.is_none() && !usb_stop_requested(&stop) {
                            if let Err(error) = engine.pump(&handle, &runtime_stats, &stop) { implicit_feedback_fault = true; fatal_error = Some(error); }
                        }
                    }
                    if fatal_error.is_none() && !usb_stop_requested(&stop) && engine.primed() {
                        if programmed_vendor_rate {
                            engine.promote_vendor_rate_proof();
                            // The write ACK is not readback. Readiness requires
                            // both contiguous cadence proof and a fresh FIFO prime.
                            set_clock_verification(true, true, true, None);
                        }
                        if playback_first {
                            engine.attach_capture(capture.as_ref().filter(|a| a.shared_implicit).cloned());
                        }
                    }
                    implicit = Some(engine);
                }
                Err(error) => { implicit_feedback_fault = true; fatal_error = Some(error); },
            }
        }
    }
    if let Some(slot) = feedback_slot.as_mut().filter(|_| fatal_error.is_none() && !usb_stop_requested(&stop)) {
        let endpoint = slot.endpoint;
        let submission = slot.submit(&handle);
        feedback_counters.record_submit(slot.last_submit_status);
        match submission {
            Ok(()) => {
                log_info!(
                    "[USB] Feedback endpoint 0x{:02x} {} interval={} service={}us maxPacket={} pollPackets={} timeout={}ms (asynchronous, nonblocking)",
                    endpoint.address,
                    transfer_type_label(endpoint.transfer_type),
                    endpoint.interval,
                    endpoint.service_interval_us,
                    endpoint.max_packet_bytes,
                    slot.packet_count,
                    slot.timeout_ms,
                );
            }
            Err(error) => {
                dev_eprintln!(
                    "[USB] Feedback endpoint 0x{:02x} initial submit failed: {}",
                    endpoint.address,
                    error
                );
                feedback_counters.errors = feedback_counters.errors.saturating_add(1);
                explicit_feedback_fault = true;
                match feedback_submit_failure(error, slot.submit_no_device, true) {
                    Some(FeedbackFault::DeviceLost) => { device_lost = true; fatal_error = Some("explicit feedback device disconnected during initial submit".into()); }
                    Some(FeedbackFault::Transfer(error)) => { fatal_error = Some(error); }
                    None => unreachable!("first explicit submit failure is terminal"),
                }
            }
        }
    }

    if fatal_error.is_none() {
        for slot in slots.iter_mut() {
            if slot.in_flight { continue; }
            if usb_stop_requested(&stop) {
                break;
            }
            if let Some(engine) = implicit.as_mut() {
                engine.running = true;
                if let Err(error) = engine.pump(&handle, &runtime_stats, &stop) { fatal_error = Some(error); break; }
                let Some(pattern) = engine.take_pattern(&runtime_stats) else { implicit_feedback_fault = true; fatal_error = Some("implicit prime lost its complete pattern".into()); break; };
                scheduler.set_packet_pattern(pattern);
            }
            let payload = match prepare_iso_transfer_payload(
                &mut scheduler,
                &pcm_buffer,
                &runtime_stats,
                &candidate,
                slot_bytes,
                channels,
                bytes_per_frame,
                playback_format.dsd_transport,
                dsd_big_endian,
                dsd_bit_reverse,
            ) {
                Ok(Some(payload)) => payload,
                Ok(None) => continue,
                Err(error) => {
                    fatal_error = Some(format!(
                        "USB direct transfer preparation failed: {}",
                        error
                    ));
                    break;
                }
            };

            record_prepared_iso_transfer(
                &payload,
                playback_format,
                &candidate,
                &runtime_stats,
                bytes_per_frame,
                &mut logged_transfer_preview,
                &mut logged_usb_encode_format,
                &mut underrun_count,
            );
            note_underrun_edge(&shared, &mut was_underrun, payload.underrun);

            let scheduled = payload.transfer_buffer.len() / bytes_per_frame;
            if let Err(error) = slot.submit(&handle, candidate.endpoint_address, payload) {
                fatal_error = Some(format!("USB direct transfer failed: {}", error));
                break;
            }
            if candidate.implicit_feedback.is_some() { runtime_stats.implicit_scheduled_frames.fetch_add(scheduled as u64, Ordering::Relaxed); }
            if active_slots == 0 {
                if let Some(pool) = capture_pool.as_mut() {
                    pool.note_playback_submitted(
                        slot.submitted_at.expect("successful OUT submit records its timestamp"),
                        Duration::from_micros(geometry.transfer_us()),
                    );
                }
            }
            active_slots = active_slots.saturating_add(1);
        }
    }
    min_active_slots = min_active_slots.min(active_slots);
    publish_inflight_music(&slots, &runtime_stats);
    if active_slots > 0 {
        runtime_stats.started.store(true, Ordering::Release);
        shared.note_bit_perfect();
        shared.refresh_state();
    }

    if fatal_error.is_none() && !usb_stop_requested(&stop)
        && capture_uses_implicit_feedforward && active_slots == 0 {
        let error = Error::Transfer("implicit feedforward started with an empty OUT bus queue".into());
        fatal_error = Some(error.to_string());
        capture_failure = Some(error);
    }
    if let Some(engine) = implicit.as_mut() { engine.running = active_slots > 0; }
    while fatal_error.is_none() && !usb_stop_requested(&stop) {
        if active_slots == 0 && implicit.is_none() { break; }

        if let Err(error) = context.handle_events(Some(if implicit.is_some() {
            Duration::from_micros(u64::from(candidate.service_interval_us).min(1000))
        } else { Duration::from_millis(50) })) {
            device_lost = error == rusb::Error::NoDevice;
            fatal_error = Some(format!("libusb_handle_events failed: {}", error));
            break;
        }

        if usb_stop_requested(&stop) { break; }
        // A flush since the last look: cut what is still queued of the old
        // position. Only transfers filled before the flush are touched; their
        // completions come back below as `CancelledByFlush`.
        let flush_epoch = runtime_stats.flush_epoch.load(Ordering::Acquire);
        if flush_epoch != handled_flush_epoch {
            handled_flush_epoch = flush_epoch;
            if bus_config.cancel_in_flight_on_flush {
                let started = Instant::now();
                let cancel_result = if implicit.is_some() || feedback_slot.is_some()
                    || capture_uses_implicit_feedforward {
                    let targets = flush_cancel_targets(flush_epoch, slots.iter().map(|slot| (slot.in_flight, slot.queued_epoch, slot.queued_music_frames)));
                    for &index in &targets { slots[index].cancel_for_flush(); }
                    if (feedback_slot.is_some() || capture_uses_implicit_feedforward)
                        && !targets.is_empty() {
                        out_flush_deadline.get_or_insert(started + FLUSH_CANCEL_TIMEOUT);
                    }
                    Ok(targets.len())
                } else { cancel_stale_transfers_for_flush(&context, &mut slots, flush_epoch) };
                match cancel_result {
                    Ok(0) => {}
                    Ok(asked) => {
                        log_info!(
                            "[USB] flush: requested cancellation of {} in-flight transfer(s) in {} us \
                             (cancelled so far: {})",
                            asked,
                            started.elapsed().as_micros(),
                            runtime_stats.flush_cancelled_transfers.load(Ordering::Relaxed),
                        );
                    }
                    Err(error) => {
                        fatal_error = Some(error);
                        break;
                    }
                }
            }
        }

        // Adaptive implicit feedforward must see this event batch's validated
        // OUT completions before it decides whether capture timing is stale.
        // Preserve the established order for explicit and ordinary capture.
        if let Some(pool) = capture_pool.as_mut().filter(|pool| !pool.uses_implicit_feedforward()) {
            if let Err(error) = pool.pump(&handle, &stop) {
                device_lost = error == Error::DeviceLost; fatal_error = Some(error.to_string()); capture_failure = Some(error); break;
            }
        }
        if let Some(engine) = implicit.as_mut() {
            if let Err(error) = engine.pump(&handle, &runtime_stats, &stop) {
                device_lost = error.contains("disconnected"); implicit_feedback_fault = true; fatal_error = Some(error); break;
            }
        }
        if let Some(feedback_slot) = feedback_slot.as_mut() {
            let result = pump_feedback_transfer(
                &handle,
                feedback_slot,
                &mut scheduler,
                candidate.service_interval_us,
                candidate.usb_speed,
                candidate.max_packet_bytes / bytes_per_frame.max(1),
                feedback_per_service,
                &mut feedback_counters,
                &stop,
                feedback_health.expect("selected explicit pipe has health deadlines"),
                stream_started_at,
            );
            // Commit the hint only once live timing has accepted a timely
            // report. A conforming report removes an older alternate hint.
            if feedback_counters.reports != 0 {
                if let Some(route) = feedback_route.take() {
                    let mut routes = state.service_interval_feedback_routes.lock();
                    if feedback_counters.selected_service_interval() {
                        routes.insert(route);
                    } else {
                        routes.remove(&route);
                    }
                }
            }
            publish_feedback_counters(&runtime_stats, &feedback_counters);
            publish_feedback_evidence(&runtime_stats, &mut feedback_counters, false);
            if let Err(error) = result {
                explicit_feedback_fault = true;
                match error {
                    FeedbackFault::DeviceLost => { device_lost = true; fatal_error = Some("explicit feedback device disconnected".into()); }
                    FeedbackFault::Transfer(error) => { fatal_error = Some(error); }
                }
                break;
            }
            if let Some(health) = feedback_health {
                let (locked, stale, terminal) = health.state(Instant::now(), stream_started_at, feedback_counters.last_report_at);
                let old_lock = runtime_stats.feedback_locked.swap(locked, Ordering::AcqRel);
                let old_stale = runtime_stats.feedback_stale.swap(stale, Ordering::AcqRel);
                if old_lock != locked || old_stale != stale {
                    if let Some(trace) = &runtime_stats.feedback_trace {
                        trace.record(crate::FeedbackTraceEvent::Lock { locked, stale });
                    }
                }
                if !locked { set_usb_stream_stable(false); }
                if stale && !feedback_stale_warned {
                    shared.events.emit_stream(crate::events::DacEvent::FeedbackStale);
                }
                feedback_stale_warned = stale;
                shared.note_bit_perfect();
                if terminal {
                    explicit_feedback_fault = true;
                    fatal_error = Some("explicit feedback deadline expired without a valid rate report".into());
                    break;
                }
            }
        }

        let mut latest_playback_completion = None;
        for slot in slots.iter_mut() {
            if bootstrap[slot.slot_index] {
                match retire_bootstrap(slot, acceptance) {
                    Ok(true) => {
                        bootstrap[slot.slot_index] = false;
                        active_slots = active_slots.saturating_sub(1);
                        if !usb_stop_requested(&stop) {
                            let pattern = implicit.as_mut().and_then(|engine| engine.take_pattern(&runtime_stats));
                            let Some(pattern) = pattern else {
                                implicit_feedback_fault = true;
                                fatal_error = Some("PLAYBACK_FIRST promotion lost measured timing".into()); break;
                            };
                            scheduler.set_packet_pattern(pattern);
                            match submit_next_transfer(slot, &handle, &mut scheduler, &pcm_buffer, &runtime_stats,
                                &candidate, slot_bytes, channels, bytes_per_frame, playback_format, dsd_big_endian,
                                dsd_bit_reverse, &mut logged_transfer_preview, &mut logged_usb_encode_format,
                                &mut underrun_count, &shared, &mut was_underrun) {
                                Ok(true) => active_slots += 1,
                                Ok(false) => { implicit_feedback_fault = true; fatal_error = Some("empty PLAYBACK_FIRST promotion".into()); break; }
                                Err(error) => { device_lost = matches!(classify_stream_failure(Some(&error), false, None), Some(Error::DeviceLost)); implicit_feedback_fault = true; fatal_error = Some(error); break; }
                            }
                        }
                    }
                    Ok(false) => (),
                    Err(error) => { device_lost = error == Error::DeviceLost; implicit_feedback_fault = true; fatal_error = Some(error.to_string()); break; }
                }
                continue;
            }
            let Some((status, completed_at, _actual_length)) = slot.take_completion() else {
                continue;
            };

            if slot.cancelled_for_flush && out_flush_deadline.is_some_and(|deadline| completed_at >= deadline) {
                fatal_error = Some("OUT flush cancellation acknowledgement deadline expired".into());
            }

            // Cancelled because the stream was flushed: the audio was never
            // played, so it is neither lost nor an underrun nor counted as
            // played, and it must not touch the loss or stability bounds. The
            // slot is free again; refill it (wire silence until the ring has
            // refilled) like any other.
            if classify_completion(status, slot.cancelled_for_flush) == Completion::CancelledByFlush {
                runtime_stats
                    .flush_cancelled_transfers
                    .fetch_add(1, Ordering::Relaxed);
                slot.mark_completed();
                active_slots = active_slots.saturating_sub(1);
                if fatal_error.is_none() && !usb_stop_requested(&stop) && implicit.is_none() {
                    match submit_next_transfer(
                        slot,
                        &handle,
                        &mut scheduler,
                        &pcm_buffer,
                        &runtime_stats,
                        &candidate,
                        slot_bytes,
                        channels,
                        bytes_per_frame,
                        playback_format,
                        dsd_big_endian,
                        dsd_bit_reverse,
                        &mut logged_transfer_preview,
                        &mut logged_usb_encode_format,
                        &mut underrun_count,
                        &shared,
                        &mut was_underrun,
                    ) {
                        Ok(true) => active_slots = active_slots.saturating_add(1),
                        Ok(false) => {}
                        Err(error) => {
                            fatal_error = Some(error);
                            break;
                        }
                    }
                }
                runtime_stats
                    .transfer_queue_depth
                    .store(active_slots, Ordering::Relaxed);
                continue;
            }

            active_slots = active_slots.saturating_sub(1);
            min_active_slots = min_active_slots.min(active_slots);
            runtime_stats
                .transfer_queue_depth
                .store(active_slots, Ordering::Relaxed);
            let turnaround_us = slot.turnaround_us(completed_at).unwrap_or_default();
            let queued_frame_count = slot.queued_frame_count as u64;
            let completed_underrun = slot.queued_underrun;
            let completed_held = slot.queued_held;
            let completed_music_frames = slot.queued_music_frames as u64;
            let completed_epoch = slot.queued_epoch;
            // A single timed-out isochronous URB is a lost slot, not a broken
            // device, so the stream keeps going. Its audio is gone though: it is
            // counted as lost (never as consumed) and the stream is bounded by
            // `loss_tracker` below.
            let timed_out = status == LIBUSB_TRANSFER_TIMED_OUT;
            let transfer_result = match status {
                LIBUSB_TRANSFER_COMPLETED => slot.validate_packets(acceptance),
                LIBUSB_TRANSFER_TIMED_OUT => Ok(slot.account_timeout(acceptance)),
                LIBUSB_TRANSFER_STALL => Err("isochronous transfer stalled".to_string()),
                LIBUSB_TRANSFER_NO_DEVICE => {
                    device_lost = true;
                    Err("USB DAC disconnected".to_string())
                }
                LIBUSB_TRANSFER_OVERFLOW => Err("isochronous transfer overflow".to_string()),
                LIBUSB_TRANSFER_CANCELLED => Err("isochronous transfer cancelled".to_string()),
                LIBUSB_TRANSFER_ERROR => Err("isochronous transfer failed".to_string()),
                other => Err(format!(
                    "isochronous transfer returned status {} ({})",
                    other,
                    iso_transfer_status_label(other)
                )),
            };
            slot.mark_completed();

            let completion_stats = match transfer_result {
                Ok(stats) => stats,
                Err(error) => {
                    fatal_error = Some(format!(
                        "USB direct transfer failed: {}; sample_rate={} channels={} \
                         subslot_bytes={} bit_resolution={} service_interval_us={} interface={} \
                         alt={} endpoint=0x{:02x} speed={:?} sync={}",
                        error,
                        playback_format.sample_rate,
                        channels,
                        slot_bytes,
                        candidate.bit_resolution,
                        candidate.service_interval_us,
                        candidate.interface_number,
                        candidate.alt_setting,
                        candidate.endpoint_address,
                        candidate.usb_speed,
                        sync_type_label(candidate.sync_type),
                    ));
                    break;
                }
            };

            if capture_uses_implicit_feedforward && qualifies_implicit_feedforward_completion(
                status, &completion_stats, &slot.packet_sizes, bytes_per_frame, queued_frame_count,
            ) {
                // Slot iteration is not callback order. Publish the freshest
                // accepted timestamp once, without substituting wall time.
                latest_playback_completion = Some(latest_playback_completion
                    .map_or(completed_at, |previous: Instant| previous.max(completed_at)));
            }

            let urb_loss = UrbLoss::from_completion(&completion_stats, timed_out, ring_frame_bytes)
                .with_held(completed_held);
            let verdict = loss_tracker.record(queued_frame_count, urb_loss);
            if verdict.lossy || urb_loss.short_packets > 0 || urb_loss.zero_packets > 0 {
                publish_transport_loss(&runtime_stats, &loss_tracker);
            }
            if urb_loss.lost_frames > 0 {
                shared
                    .events
                    .emit_stream(crate::events::DacEvent::LostFrames {
                        frames: urb_loss.lost_frames,
                    });
                shared.note_bit_perfect();
            }

            let now_ms = stream_started_at
                .elapsed()
                .as_millis()
                .min(u128::from(u64::MAX)) as u64;
            if timed_out {
                log_warn!(
                    "[USB] ISO transfer timed out; its {} is lost and not re-sent \
                     (consecutiveLossyUrbs={}, dataUrbTimeouts={}, queueDepth={}, \
                     urbLostFrames={}/{}, lostFrames={}, heldLostFrames={})",
                    if completed_held { "held silence" } else { "audio" },
                    loss_tracker.consecutive_lossy_urbs(),
                    loss_tracker.data_urb_timeouts(),
                    active_slots,
                    urb_loss.lost_frames,
                    queued_frame_count,
                    loss_tracker.lost_frames(),
                    loss_tracker.held_lost_frames(),
                );
            } else if urb_loss.short_packets > 0 || urb_loss.zero_packets > 0 {
                // First occurrences plus a bounded periodic summary; neither
                // kind of line may kill the stream on its own.
                if verdict.lossy {
                    if loss_log_gate.admit(now_ms) {
                        log_warn!(
                            "[USB] ISO transfer lost {}: urbLostFrames={}/{} lostFrames={} \
                             shortPackets={} zeroPackets={} consecutiveLossyUrbs={} \
                             lastShort={} lastZero={} descriptorRewrites={} legal={}/{} bytes \
                             frame={}B",
                            if completed_held { "held silence" } else { "audio" },
                            urb_loss.lost_frames,
                            queued_frame_count,
                            loss_tracker.lost_frames(),
                            loss_tracker.short_packets(),
                            loss_tracker.zero_packets(),
                            loss_tracker.consecutive_lossy_urbs(),
                            completion_stats.short_packets,
                            completion_stats.zero_packets,
                            completion_stats.descriptor_rewrites,
                            acceptance.legal_sizes()[0],
                            acceptance.legal_sizes()[1],
                            acceptance.bytes_per_frame(),
                        );
                    }
                } else if repacketised_log_gate.admit(now_ms) {
                    log_warn!(
                        "[USB] ISO packets repacketised without loss: shortPackets={} \
                         lastShort={} descriptorRewrites={} legal={}/{} bytes frame={}B",
                        loss_tracker.short_packets(),
                        completion_stats.short_packets,
                        completion_stats.descriptor_rewrites,
                        acceptance.legal_sizes()[0],
                        acceptance.legal_sizes()[1],
                        acceptance.bytes_per_frame(),
                    );
                }
            }

            if verdict.counts_as_successful_transfer() {
                successful_transfers = successful_transfers.saturating_add(1);
            }

            let completed_transfers = runtime_stats
                .completed_transfers
                .fetch_add(1, Ordering::Relaxed)
                .saturating_add(1);
            runtime_stats
                .consumer_frames
                .fetch_add(verdict.consumed_frames, Ordering::Relaxed);
            // Music the DAC took from this transfer. A transfer that carried
            // held silence has no music, padding and lost frames are not
            // counted, and one filled before a flush belongs to the old epoch.
            // The slot is resubmitted with a fresh payload, so a timed-out
            // transfer is counted here once and never again.
            account_completed_transfer(
                &runtime_stats,
                &mut played_music,
                completed_epoch,
                queued_frame_count,
                completed_music_frames,
                urb_loss.lost_frames,
            );

            if let Some(limit) = verdict.fatal {
                fatal_error = Some(format!(
                    "USB direct transfer failed: {}; lost_frames={} short_packets={} \
                     zero_packets={} data_urb_timeouts={}; sample_rate={} channels={} \
                     subslot_bytes={} bit_resolution={} service_interval_us={} interface={} \
                     alt={} endpoint=0x{:02x} speed={:?} sync={}",
                    limit,
                    loss_tracker.lost_frames(),
                    loss_tracker.short_packets(),
                    loss_tracker.zero_packets(),
                    loss_tracker.data_urb_timeouts(),
                    playback_format.sample_rate,
                    channels,
                    slot_bytes,
                    candidate.bit_resolution,
                    candidate.service_interval_us,
                    candidate.interface_number,
                    candidate.alt_setting,
                    candidate.endpoint_address,
                    candidate.usb_speed,
                    sync_type_label(candidate.sync_type),
                ));
                break;
            }

            let mut submit_gap_us = 0u64;
            if fatal_error.is_none() && !usb_stop_requested(&stop) && implicit.is_none() {
                match submit_next_transfer(
                    slot,
                    &handle,
                    &mut scheduler,
                    &pcm_buffer,
                    &runtime_stats,
                    &candidate,
                    slot_bytes,
                    channels,
                    bytes_per_frame,
                    playback_format,
                    dsd_big_endian,
                    dsd_bit_reverse,
                    &mut logged_transfer_preview,
                    &mut logged_usb_encode_format,
                    &mut underrun_count,
                    &shared,
                    &mut was_underrun,
                ) {
                    Ok(true) => {
                        submit_gap_us = completed_at
                            .elapsed()
                            .as_micros()
                            .min(u128::from(u64::MAX))
                            as u64;
                        active_slots = active_slots.saturating_add(1);
                        runtime_stats
                            .transfer_queue_depth
                            .store(active_slots, Ordering::Relaxed);
                    }
                    Ok(false) => {}
                    Err(error) => {
                        fatal_error = Some(error);
                        break;
                    }
                }
            }

            update_transfer_timing_stats(
                &runtime_stats,
                turnaround_us,
                submit_gap_us,
                stream_started_at,
            );

            if completed_transfers % USB_TIMING_LOG_INTERVAL == 0 {
                let effective_consumer_rate_hz = runtime_stats
                    .effective_consumer_rate_milli_hz
                    .load(Ordering::Relaxed)
                    as f64
                    / 1_000.0;
                let last_feedback_ms = runtime_stats.feedback_last_report_ms.load(Ordering::Relaxed);
                let feedback_age_ms = if last_feedback_ms == u64::MAX { -1 } else {
                    (runtime_stats.started_at.elapsed().as_millis() as u64).saturating_sub(last_feedback_ms) as i64
                };
                let feedback_rate_hz = runtime_stats.feedback_rate_milli_hz.load(Ordering::Relaxed) as f64 / 1000.0;
                dev_eprintln!(
                    "[USB-TIMING] completed={} queueDepth={} minQueueDepth={} configuredQueueDepth={} effectiveRateHz={:.3} lastTurnaroundUs={} maxTurnaroundUs={} lastSubmitGapUs={} maxSubmitGapUs={} feedbackActive={} reports={} timeouts={} errors={} rejected={} feedbackAgeMs={} feedbackRateHz={:.2} lostFrames={} shortPackets={} zeroPackets={} dataUrbTimeouts={} transportLoss={} playedMusicFrames={} outputLatencyMs={:.1}",
                    completed_transfers,
                    runtime_stats.transfer_queue_depth.load(Ordering::Relaxed),
                    min_active_slots,
                    transfer_slot_count,
                    effective_consumer_rate_hz,
                    runtime_stats.last_transfer_turnaround_us.load(Ordering::Relaxed),
                    runtime_stats.max_transfer_turnaround_us.load(Ordering::Relaxed),
                    runtime_stats.last_submit_gap_us.load(Ordering::Relaxed),
                    runtime_stats.max_submit_gap_us.load(Ordering::Relaxed),
                    runtime_stats.feedback_locked.load(Ordering::Acquire),
                    runtime_stats.feedback_reports.load(Ordering::Relaxed),
                    runtime_stats.feedback_timeouts.load(Ordering::Relaxed),
                    runtime_stats.feedback_errors.load(Ordering::Relaxed),
                    runtime_stats.feedback_rejected.load(Ordering::Relaxed),
                    feedback_age_ms,
                    feedback_rate_hz,
                    runtime_stats.lost_frames.load(Ordering::Relaxed),
                    runtime_stats.short_packets.load(Ordering::Relaxed),
                    runtime_stats.zero_packets.load(Ordering::Relaxed),
                    runtime_stats.data_urb_timeouts.load(Ordering::Relaxed),
                    runtime_stats.transport_loss_latched(),
                    runtime_stats.played_music_frames.load(Ordering::Relaxed),
                    runtime_stats.output_latency_ms(),
                );

            }

            if successful_transfers >= geometry.stable_transfers {
                // A stream that has lost audio never asserts stability again.
                if !completed_underrun && !loss_tracker.is_latched() && ((implicit.is_none() && feedback_slot.is_none()) || runtime_stats.feedback_locked.load(Ordering::Acquire)) {
                    set_usb_stream_stable(true);
                    if !logged_usb_stabilized {
                        if feedback_slot.is_none() && implicit.is_none() {
                            scheduler.lock_to_nominal_packet_timing();
                            log_info!(
                                "[USB] USB stream stabilized after {} isochronous transfers (nominal packet timing locked)",
                                successful_transfers
                            );
                        } else {
                            log_info!(
                                "[USB] USB stream stabilized after {} transfers (feedback-driven packet timing active, reports={}, timeouts={})",
                                successful_transfers,
                                runtime_stats.feedback_reports.load(Ordering::Relaxed),
                                runtime_stats.feedback_timeouts.load(Ordering::Relaxed),
                            );
                        }
                        logged_usb_stabilized = true;
                    }
                } else {
                    set_usb_stream_stable(false);
                }
            } else {
                set_usb_stream_stable(false);
            }
        }
        if let Some(deadline) = out_flush_deadline {
            if !slots.iter().any(|slot| slot.in_flight && slot.cancelled_for_flush) {
                out_flush_deadline = None;
            } else if Instant::now() >= deadline {
                fatal_error = Some("OUT flush cancellation acknowledgement deadline expired".into());
            }
        }
        if fatal_error.is_none() && !usb_stop_requested(&stop) {
            if let Some(engine) = implicit.as_mut() {
                if active_slots == 0 {
                    runtime_stats.implicit_empty_bus.fetch_add(1, Ordering::Relaxed);
                    implicit_feedback_fault = true;
                    fatal_error = Some("implicit feedback observed an empty OUT bus queue".into());
                    break;
                }
                for slot in slots.iter_mut().filter(|slot| !slot.in_flight) {
                    if usb_stop_requested(&stop) { break; }
                    let Some(pattern) = engine.take_pattern(&runtime_stats) else {
                        runtime_stats.implicit_waits.fetch_add(1, Ordering::Relaxed); break;
                    };
                    scheduler.set_packet_pattern(pattern);
                    match submit_next_transfer(slot, &handle, &mut scheduler, &pcm_buffer, &runtime_stats,
                        &candidate, slot_bytes, channels, bytes_per_frame, playback_format, dsd_big_endian,
                        dsd_bit_reverse, &mut logged_transfer_preview, &mut logged_usb_encode_format,
                        &mut underrun_count, &shared, &mut was_underrun) {
                        Ok(true) => { active_slots += 1; }
                        Ok(false) => {}
                        Err(error) => { fatal_error = Some(error); break; }
                    }
                }
                let coverage = slots.iter().filter(|slot| slot.in_flight).filter_map(|slot| slot.submitted_at)
                    .map(|at| (at + Duration::from_micros(geometry.transfer_us())).saturating_duration_since(Instant::now()))
                    .max().unwrap_or_default();
                if active_slots == 0 || (engine.last_valid.elapsed() > Duration::from_micros(geometry.queued_us()) && coverage <= Duration::from_micros(u64::from(candidate.service_interval_us))) {
                    runtime_stats.implicit_empty_bus.fetch_add(1, Ordering::Relaxed);
                    implicit_feedback_fault = true;
                    fatal_error = Some("implicit feedback continuity deadline expired (OUT bus coverage exhausted)".into());
                }
                runtime_stats.transfer_queue_depth.store(active_slots, Ordering::Relaxed);
            }
        }
        if fatal_error.is_none() && !usb_stop_requested(&stop) {
            if let Some(pool) = capture_pool.as_mut().filter(|pool| pool.uses_implicit_feedforward()) {
                // An absent OUT queue cannot clock an adaptive IN endpoint.
                // Fail the shared owner before pumping any further capture.
                let result = if active_slots == 0 {
                    Err(Error::Transfer("implicit feedforward observed an empty OUT bus queue".into()))
                } else {
                    latest_playback_completion
                        .map_or(Ok(()), |at| pool.note_playback_completed(at))
                        .and_then(|()| pool.pump(&handle, &stop))
                };
                if let Err(error) = result {
                    device_lost = error == Error::DeviceLost;
                    fatal_error = Some(error.to_string());
                    capture_failure = Some(error);
                }
            }
        }
        publish_inflight_music(&slots, &runtime_stats);
        shared.refresh_state();
        shared.note_bit_perfect();
    }

    publish_feedback_evidence(&runtime_stats, &mut feedback_counters, true);
    // Preserve diagnostic context in the shared duplex failure as well as the
    // output error log. Classify only after the final evidence is attached.
    if explicit_feedback_fault && !device_lost {
        if let (Some(error), Some(diagnostics)) = (
            fatal_error.as_mut(),
            *runtime_stats.explicit_feedback_diagnostics.lock(),
        ) {
            error.push_str(&format!(" | {diagnostics}"));
        }
    }
    let failure = classify_stream_failure(fatal_error.as_deref(), device_lost,
        capture_failure.or_else(|| capture.as_ref().and_then(|attachment| attachment.failure())));
    device_lost = matches!(&failure, Some(Error::DeviceLost));
    if let Some(attachment) = capture.as_ref() {
        attachment.finish(failure.clone());
    }
    if let Some(error) = fatal_error {
        runtime_stats.feedback_locked.store(false, Ordering::Release);
        if feedback_health.is_some() { publish_feedback_counters(&runtime_stats, &feedback_counters); }
        if explicit_feedback_fault && !device_lost {
            runtime_stats.feedback_stale.store(true, Ordering::Release);
            if !feedback_stale_warned { shared.events.emit_stream(crate::events::DacEvent::FeedbackStale); }
            shared.note_bit_perfect();
        }
        if implicit_feedback_fault && !device_lost {
            if programmed_vendor_rate { set_clock_verification(true, true, false, None); }
            runtime_stats.feedback_locked.store(false, Ordering::Release);
            runtime_stats.feedback_stale.store(true, Ordering::Release);
            runtime_stats.feedback_errors.fetch_add(1, Ordering::Relaxed);
            shared.events.emit_stream(crate::events::DacEvent::FeedbackStale);
        }
        if let Some(trace) = &runtime_stats.feedback_trace {
            trace.fail(&error);
        }
        set_usb_stream_stable(false);
        set_last_error(Some(error.clone()));
        set_engine_state(EngineState::Error, Some(error.clone()));
        log_error!("{}", error);
        shared.fail(failure.expect("fatal output error has a shared failure classification"));
    }

    runtime_stats.feedback_locked.store(false, Ordering::Release);
    shared.note_bit_perfect();
    for slot in &mut slots { slot.cancel(); }
    if let Some(pool) = capture_pool.as_mut() { pool.cancel(); }
    if let Some(engine) = implicit.as_mut() { for slot in &mut engine.slots { slot.cancel(); } }
    if let Some(slot) = feedback_slot.as_mut() { slot.cancel(); }
    drain_iso_transfer_slots(&context, &mut slots);
    if let Some(pool) = capture_pool.as_mut() { pool.drain(&context); }
    if let Some(engine) = implicit.as_mut() { drain_iso_transfer_slots(&context, &mut engine.slots); }
    drain_feedback_transfer_slot(&context, &mut feedback_slot);
    runtime_stats.feedback_locked.store(false, Ordering::Release);

    drop(usb_owner);
    set_stream_active(false);
    set_runtime_stats(None);
    set_usb_stream_stable(false);
    if usb_stop_requested(&stop) {
        set_engine_state(EngineState::Idle, None);
    }
}

pub(super) fn log_stream_debug_preview(
    playback_format: PlaybackFormat,
    candidate: &IsoStreamCandidate,
    packet_bytes: &[usize],
    packet_samples: &[i32],
    transfer_buffer: &[u8],
) {
    let frames_per_packet: Vec<usize> = packet_bytes
        .iter()
        .map(|packet| packet / (candidate.subslot_size as usize * candidate.channels as usize))
        .collect();
    let sample_preview: Vec<i32> = packet_samples.iter().take(10).copied().collect();
    let byte_decoded_preview: Vec<i32> = transfer_buffer
        .chunks_exact(candidate.subslot_size as usize)
        .take(10)
        .map(|chunk| {
            let mut bytes = [0u8; 4];
            for (index, byte) in chunk.iter().enumerate() {
                bytes[index] = *byte;
            }
            if candidate.subslot_size < 4 && chunk.last().copied().unwrap_or_default() & 0x80 != 0 {
                for byte in bytes.iter_mut().skip(candidate.subslot_size as usize) {
                    *byte = 0xff;
                }
            }
            i32::from_le_bytes(bytes)
        })
        .collect();
    let first_bytes_preview: Vec<String> = transfer_buffer
        .iter()
        .take(32)
        .map(|byte| format!("{:02x}", byte))
        .collect();

    dev_eprintln!(
        "[USB] Stream start engineOutput=f32@{}Hz transport={} {}-bit {}ch {}-byte-subslot interface={} alt={} endpoint=0x{:02x} sync={} usage={} refresh={} synchAddress=0x{:02x} endpointMaxPacket={} feedback={} packetSizes={:?} framesPerPacket={:?}",
        playback_format.sample_rate,
        format_tag_label(candidate.format_tag),
        candidate.bit_resolution,
        candidate.channels,
        candidate.subslot_size,
        candidate.interface_number,
        candidate.alt_setting,
        candidate.endpoint_address,
        sync_type_label(candidate.sync_type),
        usage_type_label(candidate.usage_type),
        candidate.refresh,
        candidate.synch_address,
        candidate.max_packet_bytes,
        candidate
            .feedback_endpoint
            .map(|feedback| format!(
                "0x{:02x}/{}@interval{}",
                feedback.address,
                transfer_type_label(feedback.transfer_type),
                feedback.interval,
            ))
            .unwrap_or_else(|| "none".to_string()),
        packet_bytes,
        frames_per_packet,
    );
    dev_eprintln!("[USB] PCM sample preview (i32): {:?}", sample_preview);
    dev_eprintln!(
        "[USB] byte-decoded preview (i32 from LE bytes): {:?}",
        byte_decoded_preview
    );
    dev_eprintln!("[USB] first 32 transfer bytes: {:?}", first_bytes_preview);
}

pub(super) fn panic_payload_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).to_string()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "unknown panic".to_string()
    }
}


/// Classify once before notifying either direction of the shared worker.
fn classify_stream_failure(message: Option<&str>, device_lost: bool, capture: Option<Error>) -> Option<Error> {
    if device_lost || message.is_some_and(|message| message.contains("disconnected"))
        || matches!(&capture, Some(Error::DeviceLost)) {
        Some(Error::DeviceLost)
    } else {
        capture.or_else(|| message.map(|message| Error::Transfer(message.to_owned())))
    }
}

#[cfg(test)]
mod duplex_failure_tests {
    use super::*;
    #[test]
    fn classify_disconnect_before_notifying_playback_and_capture() {
        let failure = classify_stream_failure(Some("USB DAC disconnected (implicit feedback)"), false, None);
        assert_eq!(failure, Some(Error::DeviceLost));
        let typed = Error::Transfer("capture packet geometry changed".into());
        assert_eq!(classify_stream_failure(Some(&typed.to_string()), false, Some(typed.clone())), Some(typed));
        assert_eq!(classify_stream_failure(Some("capture completion failed"), false, Some(Error::DeviceLost)), Some(Error::DeviceLost));
        assert_eq!(classify_stream_failure(None, false, None), None);
    }
}

#[cfg(test)]
mod implicit_feedforward_completion_tests {
    use super::*;

    #[test]
    fn accepts_complete_integral_cadence_including_requested_empty_intervals() {
        // A low-rate cadence may deliberately request an empty service interval.
        assert!(qualifies_implicit_feedforward_completion(
            LIBUSB_TRANSFER_COMPLETED, &CompletionStats::default(), &[0, 4, 0, 4], 4, 2,
        ));
        assert!(!qualifies_implicit_feedforward_completion(
            LIBUSB_TRANSFER_COMPLETED, &CompletionStats::default(), &[0, 0], 4, 0,
        ));
        assert!(!qualifies_implicit_feedforward_completion(
            LIBUSB_TRANSFER_COMPLETED, &CompletionStats::default(), &[5, 3], 4, 2,
        ));
        assert!(!qualifies_implicit_feedforward_completion(
            LIBUSB_TRANSFER_COMPLETED, &CompletionStats::default(), &[4], 0, 1,
        ));
    }

    #[test]
    fn rejects_every_noncompleted_status_even_with_full_packet_accounting() {
        for status in [LIBUSB_TRANSFER_TIMED_OUT, LIBUSB_TRANSFER_CANCELLED,
            LIBUSB_TRANSFER_ERROR, LIBUSB_TRANSFER_NO_DEVICE, LIBUSB_TRANSFER_STALL,
            LIBUSB_TRANSFER_OVERFLOW] {
            assert!(!qualifies_implicit_feedforward_completion(
                status, &CompletionStats::default(), &[24, 24], 4, 12,
            ));
        }
    }

    #[test]
    fn rejects_loss_short_packets_and_lossless_repacketization() {
        for stats in [
            CompletionStats { short_packets: 1, ..CompletionStats::default() },
            CompletionStats { zero_packets: 1, ..CompletionStats::default() },
            CompletionStats { descriptor_rewrites: 1, ..CompletionStats::default() },
            CompletionStats { shortfall_frames: 1, ..CompletionStats::default() },
            CompletionStats { lost_bytes: 1, ..CompletionStats::default() },
        ] {
            assert!(!qualifies_implicit_feedforward_completion(
                LIBUSB_TRANSFER_COMPLETED, &stats, &[24, 24], 4, 12,
            ));
        }
    }
}

#[cfg(test)]
mod playback_first_tests {
    use super::*;
    #[test]
    fn nominal_bootstrap_is_wire_silence_without_application_accounting() {
        let geometry = IsoQueueGeometry::for_stream(125, 8, 4);
        let mut scheduler = IsoPacketScheduler::with_geometry(48_000, 24, 125, geometry);
        let payload = nominal_pcm_silence(&mut scheduler, 168, 24, 17).unwrap();
        assert!(payload.packet_sizes.iter().all(|&n| n == 144));
        assert_eq!(payload.transfer_buffer.len(), payload.packet_sizes.iter().sum::<usize>());
        assert!(payload.transfer_buffer.iter().all(|&byte| byte == 0));
        assert!(payload.packet_samples.is_empty());
        assert!(payload.held);
        assert!(!payload.underrun);
        assert_eq!((payload.frame_count, payload.music_frames, payload.epoch), (0, 0, 17));
    }
    #[test]
    fn roland_nominal_silence_preserves_44100_fractional_frame_cadence() {
        let geometry = IsoQueueGeometry::for_stream(125, 8, 4);
        for channels in [4, 10] {
            let stride = channels * 4;
            let mut scheduler = IsoPacketScheduler::with_geometry(44_100, stride, 125, geometry);
            // 8000 microframes represent exactly one second, independent of URB boundaries.
            let mut sizes = Vec::new();
            for _ in 0..8000 / geometry.packets_per_transfer {
                let payload = nominal_pcm_silence(&mut scheduler, 7 * stride, stride, 0).unwrap();
                assert!(payload.transfer_buffer.iter().all(|&byte| byte == 0));
                assert_eq!((payload.frame_count, payload.music_frames), (0, 0));
                sizes.extend(payload.packet_sizes);
            }
            assert!(sizes.iter().all(|&size| size == 5 * stride || size == 6 * stride));
            assert_eq!(sizes.iter().sum::<usize>() / stride, 44_100);
        }
    }
    #[test]
    fn nominal_bootstrap_rejects_capacity_or_frame_mismatch() {
        let geometry = IsoQueueGeometry::for_stream(125, 8, 4);
        let mut scheduler = IsoPacketScheduler::with_geometry(48_000, 24, 125, geometry);
        assert!(nominal_pcm_silence(&mut scheduler, 143, 24, 0).is_err());
        assert!(nominal_pcm_silence(&mut scheduler, 168, 25, 0).is_err());
        assert!(nominal_pcm_silence(&mut scheduler, 168, 0, 0).is_err());
    }
}
