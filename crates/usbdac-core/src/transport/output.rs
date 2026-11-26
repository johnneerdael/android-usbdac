use super::*;

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

    let deadline = Instant::now() + Duration::from_millis(2000);
    while slots.iter().any(|slot| slot.in_flight) && Instant::now() < deadline {
        let _ = context.handle_events(Some(Duration::from_millis(10)));
        for slot in slots.iter_mut() {
            if slot.take_completion().is_some() {
                slot.mark_completed();
            }
        }
    }

    let still_in_flight = slots.iter().filter(|slot| slot.in_flight).count();
    if still_in_flight > 0 {
        dev_eprintln!(
            "[USB] teardown drain timed out with {} isochronous transfer(s) still in flight; slots will be leaked",
            still_in_flight
        );
    }
}

pub(super) fn drain_feedback_transfer_slot(context: &Context, slot: &mut Option<FeedbackTransferSlot>) {
    let Some(slot) = slot.as_mut() else {
        return;
    };
    slot.cancel();

    let deadline = Instant::now() + Duration::from_millis(2000);
    while slot.in_flight && Instant::now() < deadline {
        let _ = context.handle_events(Some(Duration::from_millis(10)));
        if slot.take_completion().is_some() {
            slot.mark_completed();
        }
    }

    if slot.in_flight {
        dev_eprintln!(
            "[USB] teardown drain timed out with feedback transfer 0x{:02x} still in flight; slot will be leaked",
            slot.endpoint.address
        );
    }
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

pub(super) fn run_usb_output_loop(
    claimed_handle: ClaimedHandle,
    candidate: IsoStreamCandidate,
    state: SessionState,
    shared: Arc<StreamShared>,
    stop: Arc<AtomicBool>,
) {
    let pcm_buffer = Arc::clone(&shared.ring);
    let runtime_stats = Arc::clone(&shared.stats);
    let ClaimedHandle {
        device_fd,
        context,
        handle,
        claimed_interfaces,
    } = claimed_handle;
    let playback_format = state.playback_format.unwrap();
    USB_DSD_PAYLOAD_LATCH.lock().reset();
    USB_PCM_PAYLOAD_LATCH.lock().reset();
    LAST_BIT_PERFECT_LOGGED.store(false, Ordering::Relaxed);
    let dsd_big_endian = settings().dsd_big_endian();
    let dsd_bit_reverse = settings().dsd_bit_reverse();
    let slot_bytes = candidate.subslot_size as usize;
    if slot_bytes == 0 {
        let message = "the selected alternate setting has a zero-byte subslot".to_string();
        set_last_error(Some(message.clone()));
        set_engine_state(EngineState::Error, Some(message.clone()));
        shared.fail(Error::Transfer(message));
        cleanup_claimed_handle(
            ClaimedHandle {
                device_fd,
                context,
                handle,
                claimed_interfaces,
            },
            Some(candidate.interface_number),
            current_lock_requested_for_fd(device_fd),
        );
        set_stream_active(false);
        return;
    }

    if let Err(error) = validate_transport_against_playback_format(&candidate, playback_format) {
        set_last_error(Some(error.clone()));
        set_engine_state(EngineState::Error, Some(error.clone()));
        shared.fail(Error::Transfer(error));
        cleanup_claimed_handle(
            ClaimedHandle {
                device_fd,
                context,
                handle,
                claimed_interfaces,
            },
            Some(candidate.interface_number),
            current_lock_requested_for_fd(device_fd),
        );
        set_stream_active(false);
        return;
    }

    let channels = candidate.channels as usize;
    let bytes_per_frame = slot_bytes.saturating_mul(channels);
    let geometry = IsoQueueGeometry::for_stream(
        candidate.service_interval_us,
        playback_format.is_dop || playback_format.dsd_transport == DsdTransportMode::Native,
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
    if let Some(endpoint) = feedback_endpoint {
        if candidate.sync_type == SyncType::Asynchronous {
            match FeedbackTransferSlot::new(endpoint) {
                Ok(slot) => feedback_slot = Some(slot),
                Err(error) => {
                    dev_eprintln!(
                        "[USB] Failed to allocate feedback transfer for endpoint 0x{:02x}: {}",
                        endpoint.address,
                        error
                    );
                }
            }
        } else {
            dev_eprintln!(
                "[USB] Ignoring feedback endpoint 0x{:02x}: data endpoint sync type is {:?}, not asynchronous",
                endpoint.address,
                candidate.sync_type
            );
        }
    } else if candidate.sync_type == SyncType::Asynchronous {
        let packets_per_transfer = scheduler.packets_per_transfer();
        let bus_cushion_ms = transfer_slot_count
            .saturating_mul(packets_per_transfer)
            .saturating_mul(candidate.service_interval_us as usize)
            / 1000;
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
    let mut feedback_stale_warned = false;
    let mut min_active_slots = transfer_slot_count;
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
        max_consecutive_lossy_urbs: USB_MAX_CONSECUTIVE_LOSSY_URBS,
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
    let mut fatal_error: Option<String> = None;
    let mut device_lost = false;
    let mut was_underrun = false;

    let priming_target = runtime_stats.start_threshold_samples;
    log_info!(
        "[USB] Isochronous queue: serviceIntervalUs={} packetsPerTransfer={} slots={} stableTransfers={}",
        candidate.service_interval_us,
        geometry.packets_per_transfer,
        geometry.transfer_slots,
        geometry.stable_transfers,
    );
    log_info!(
        "[USB] Waiting for stream prime: buffered_samples must reach start_threshold_samples={} before isochronous submit (the ring fills on to its {}-sample target while playing)",
        priming_target,
        runtime_stats.buffer_target_samples,
    );
    while !usb_stop_requested(&stop) {
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
        if usb_stop_requested(&stop) {
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

    let stream_started_at = Instant::now();
    if fatal_error.is_none() {
        for slot in slots.iter_mut() {
            if usb_stop_requested(&stop) {
                break;
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

            if let Err(error) = slot.submit(&handle, candidate.endpoint_address, payload) {
                fatal_error = Some(format!("USB direct transfer failed: {}", error));
                break;
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

    if let Some(slot) = feedback_slot.as_mut().filter(|_| !usb_stop_requested(&stop)) {
        let endpoint = slot.endpoint;
        match slot.submit(&handle) {
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
                feedback_slot = None;
            }
        }
    }

    while fatal_error.is_none() && (!usb_stop_requested(&stop) || active_slots > 0) {
        if active_slots == 0 {
            break;
        }

        if let Err(error) = context.handle_events(Some(Duration::from_millis(50))) {
            fatal_error = Some(format!("libusb_handle_events failed: {}", error));
            break;
        }

        if let Some(feedback_slot) = feedback_slot.as_mut() {
            pump_feedback_transfer(
                &handle,
                feedback_slot,
                &mut scheduler,
                candidate.service_interval_us,
                candidate.usb_speed,
                &mut feedback_counters,
                &stop,
            );
            publish_feedback_counters(&runtime_stats, &feedback_counters);
        }

        for slot in slots.iter_mut() {
            let Some((status, completed_at, _actual_length)) = slot.take_completion() else {
                continue;
            };

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
            if !usb_stop_requested(&stop) {
                match prepare_iso_transfer_payload(
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
                    Ok(Some(payload)) => {
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

                        if let Err(error) =
                            slot.submit(&handle, candidate.endpoint_address, payload)
                        {
                            fatal_error =
                                Some(format!("USB direct transfer failed: {}", error));
                            break;
                        }
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
                    Ok(None) => {}
                    Err(error) => {
                        fatal_error = Some(format!(
                            "USB direct transfer preparation failed: {}",
                            error
                        ));
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
                let feedback_age_ms = feedback_counters
                    .last_report_at
                    .map(|reported_at| reported_at.elapsed().as_millis() as i64)
                    .unwrap_or(-1);
                let feedback_rate_hz = feedback_counters.last_estimated_sample_rate.unwrap_or(0.0);
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
                    feedback_slot.is_some(),
                    feedback_counters.reports,
                    feedback_counters.timeouts,
                    feedback_counters.errors,
                    feedback_counters.rejected,
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
                let feedback_stale = feedback_slot.is_some()
                    && if feedback_counters.reports == 0 {
                        stream_started_at.elapsed()
                            >= Duration::from_millis(USB_FEEDBACK_STALE_WARN_MS)
                    } else {
                        feedback_age_ms
                            >= USB_FEEDBACK_STALE_WARN_MS as i64
                    };
                runtime_stats
                    .feedback_stale
                    .store(feedback_stale, Ordering::Relaxed);
                if feedback_stale && !feedback_stale_warned {
                    shared.events.emit_stream(crate::events::DacEvent::FeedbackStale);
                    if feedback_counters.reports == 0 {
                        dev_eprintln!(
                            "[USB] Feedback endpoint has produced no valid report after {}ms; nominal timing remains active",
                            stream_started_at.elapsed().as_millis()
                        );
                    } else {
                        dev_eprintln!(
                            "[USB] Feedback report is stale for {}ms; holding last feedback-derived packet timing",
                            feedback_age_ms
                        );
                    }
                    feedback_stale_warned = true;
                } else if !feedback_stale {
                    feedback_stale_warned = false;
                }
            }

            if successful_transfers >= geometry.stable_transfers {
                // A stream that has lost audio never asserts stability again.
                if !completed_underrun && !loss_tracker.is_latched() {
                    set_usb_stream_stable(true);
                    if !logged_usb_stabilized {
                        if feedback_slot.is_none() {
                            scheduler.lock_to_nominal_packet_timing();
                            log_info!(
                                "[USB] USB stream stabilized after {} isochronous transfers (nominal packet timing locked)",
                                successful_transfers
                            );
                        } else {
                            log_info!(
                                "[USB] USB stream stabilized after {} transfers (feedback-driven packet timing active, reports={}, timeouts={})",
                                successful_transfers,
                                feedback_counters.reports,
                                feedback_counters.timeouts,
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
        publish_inflight_music(&slots, &runtime_stats);
        shared.refresh_state();
        shared.note_bit_perfect();
    }

    if let Some(error) = fatal_error {
        set_usb_stream_stable(false);
        set_last_error(Some(error.clone()));
        set_engine_state(EngineState::Error, Some(error.clone()));
        log_error!("{}", error);
        shared.fail(if device_lost {
            Error::DeviceLost
        } else {
            Error::Transfer(error)
        });
    }

    drain_iso_transfer_slots(&context, &mut slots);
    drain_feedback_transfer_slot(&context, &mut feedback_slot);

    cleanup_claimed_handle(
        ClaimedHandle {
            device_fd,
            context,
            handle,
            claimed_interfaces,
        },
        Some(candidate.interface_number),
        current_lock_requested_for_fd(device_fd),
    );
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

