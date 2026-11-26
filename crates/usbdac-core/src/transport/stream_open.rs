use super::*;

/// What a running stream hands back to its owner.
pub(crate) struct StreamHandle {
    pub(crate) stop: Arc<AtomicBool>,
    pub(crate) thread: Option<JoinHandle<()>>,
    pub(crate) shared: Arc<StreamShared>,
}

impl StreamHandle {
    /// Stops the output thread and waits for it. Idempotent.
    pub(crate) fn stop(&mut self) -> Result<(), String> {
        self.stop.store(true, Ordering::Release);
        match self.thread.take() {
            Some(handle) => handle.join().map_err(|payload| {
                format!(
                    "USB output thread panicked: {}",
                    panic_payload_message(payload.as_ref())
                )
            }),
            None => Ok(()),
        }
    }
}

impl Drop for StreamHandle {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

/// Opens the stream for the format the negotiation stored: programs the clock,
/// switches the alternate setting and starts the output thread.
pub(crate) fn start_stream(events: EventSink) -> Result<StreamHandle, Error> {
    // Wait briefly for a previous stream's thread to finish its teardown: a
    // stream closed a moment ago may still be releasing the interface.
    for attempt in 0..15 {
        if !USB_SESSION_ACTIVE.load(Ordering::SeqCst) {
            break;
        }
        if attempt == 0 {
            dev_eprintln!("USB direct: previous session still active, waiting for cleanup...");
        }
        thread::sleep(Duration::from_millis(100));
    }

    let session = {
        let _session_gate = USB_SESSION_GATE.lock();
        if USB_SESSION_CLEAR_PENDING.load(Ordering::SeqCst) {
            return Err(Error::InvalidState("the DAC is being closed".to_string()));
        }
        if is_usb_session_active() {
            return Err(Error::InvalidState(
                "a previous stream is still stopping".to_string(),
            ));
        }
        Arc::new(SessionGuard::start_locked())
    };
    start_stream_inner(events, &session)
}

#[allow(unused_assignments)]
pub(super) fn start_stream_inner(
    events: EventSink,
    session: &Arc<SessionGuard>,
) -> Result<StreamHandle, Error> {
    let config = settings().config.clone();
    let (state, mut playback_format) = {
        let guard = SESSION_STATE.lock();
        let Some(state) = guard.as_ref() else {
            return Err(Error::InvalidState("no DAC is open".to_string()));
        };
        let Some(effective_playback_format) = state.playback_format else {
            return Err(Error::InvalidState(
                "no negotiated stream format; call negotiate first".to_string(),
            ));
        };
        (state.clone(), effective_playback_format)
    };
    set_engine_state(EngineState::UsbInit, None);
    let clock_settle_delay_ms = settings().settle_delay_ms();

    let claimed_handle = open_claimed_usb_handle(&state.device).map_err(|error| {
        set_last_error(Some(error.clone()));
        set_direct_mode_refusal_reason(Some(error.clone()));
        set_engine_state(EngineState::Error, Some(error.clone()));
        Error::Claim(error)
    })?;
    let mut requested_playback_format = state.requested_playback_format.unwrap_or(playback_format);
    let device = claimed_handle.handle.device();
    let speed = resolve_device_speed(&device);
    let lock_requested = current_lock_requested_for_fd(state.device.fd);
    set_capability_model(
        build_usb_capability_model(&device, &claimed_handle.handle, speed).ok(),
    );
    let candidate =
        match select_stream_candidate(&device, &claimed_handle.handle, playback_format, speed) {
            Ok(candidate) => candidate,
            Err(error) => {
                set_last_error(Some(error.message().to_string()));
                set_direct_mode_refusal_reason(Some(error.message().to_string()));
                set_engine_state(
                    EngineState::Error,
                    Some(error.message().to_string()),
                );
                cleanup_claimed_handle(claimed_handle, None, lock_requested);
                return Err(error.into_error(offered_alt_settings()));
            }
        };
    if playback_format.dsd_transport == DsdTransportMode::Native {
        // Native DSD clock frames contain subslot_size bytes per channel.
        // The producer still supplies interleaved byte-rate samples.
        if !matches!(candidate.subslot_size, 1 | 2 | 4)
            || playback_format.dsd_bit_rate == 0 {
            let error = "USB native DSD refused: invalid subslot or source bit rate".to_string();
            set_last_error(Some(error.clone()));
            set_direct_mode_refusal_reason(Some(error.clone()));
            cleanup_claimed_handle(claimed_handle, None, lock_requested);
            return Err(Error::Unsupported {
                reason: error,
                offered: offered_alt_settings(),
            });
        }
        let wire_rate = playback_format.dsd_bit_rate / (8 * u32::from(candidate.subslot_size));
        playback_format.sample_rate = wire_rate;
        requested_playback_format.sample_rate = wire_rate;
        if let Some(current) = SESSION_STATE.lock().as_mut() {
            current.playback_format = Some(playback_format);
            current.requested_playback_format = Some(requested_playback_format);
        }
    }
    let clock = find_audio_control_clock(&device, &claimed_handle.handle, candidate.terminal_link);
    let clock_detection_msg = match &clock {
        Some(c) => format!(
            "Clock found: interface={}, clockId={}, via_terminal_link={:?}",
            c.interface_number, c.clock_id, candidate.terminal_link,
        ),
        None => "Clock NOT FOUND: no UAC2 clock entity in any AudioControl descriptor".to_string(),
    };
    dev_eprintln!(
        "[USB] Clock entity for '{}': {}",
        state.device.product_name, clock_detection_msg
    );

    set_last_error(Some(format!("[clock-diag] {}", clock_detection_msg)));
    set_direct_mode_refusal_reason(None);
    set_usb_stream_stable(false);
    // The hardware volume control of the terminal this stream feeds. Normally
    // the one found when the DAC was opened; replaced if the stream plays
    // through a different Feature Unit.
    refresh_stream_volume_control(
        &state.device,
        &device,
        candidate.terminal_link,
        candidate.interface_number,
        candidate.channels,
        clock.map(|clock| clock.interface_number),
    );
    set_clock_status(clock.map(|_| ClockStatus {
        reported_sample_rate: state
            .clock_status
            .as_ref()
            .and_then(|status| status.reported_sample_rate),
    }));
    set_clock_verification(
        state.clock_control_attempted,
        state.clock_control_succeeded,
        state.clock_verification_passed,
        state
            .clock_status
            .as_ref()
            .and_then(|status| status.reported_sample_rate),
    );

    dev_eprintln!(
        "[USB] Candidate device='{}' mode={:?} requested={}Hz effective={}Hz resampling={} endpoint=0x{:02x} interface={} alt={} clock={} feedbackEndpoint={} sync={:?} usage={:?} service={}us maxPacket={} speed={:?} fmt={} channels={} subslot={} bitResolution={} sampleRates={:?}",
        state.device.product_name,
        state.dac_mode,
        requested_playback_format.sample_rate,
        playback_format.sample_rate,
        requested_playback_format.sample_rate != playback_format.sample_rate,
        candidate.endpoint_address,
        candidate.interface_number,
        candidate.alt_setting,
        clock.map(|clock| clock.clock_id).unwrap_or_default(),
        candidate
            .feedback_endpoint
            .map(|feedback| format!(
                "0x{:02x}/{}",
                feedback.address,
                transfer_type_label(feedback.transfer_type)
            ))
            .unwrap_or_else(|| "none".to_string()),
        candidate.sync_type,
        candidate.usage_type,
        candidate.service_interval_us,
        candidate.max_packet_bytes,
        speed,
        format_tag_label(candidate.format_tag),
        candidate.channels,
        candidate.subslot_size,
        candidate.bit_resolution,
        candidate.sample_rates,
    );

    dev_eprintln!(
        "[USB] Interface={} alt={} endpoint=0x{:02x} syncAddress=0x{:02x} interval={} serviceIntervalUs={} refresh={}",
        candidate.interface_number,
        candidate.alt_setting,
        candidate.endpoint_address,
        candidate.synch_address,
        candidate.endpoint_interval,
        candidate.service_interval_us,
        candidate.refresh,
    );

    if let Err(error) = claimed_handle.ensure_interface_claimed(candidate.interface_number) {
        set_last_error(Some(error.clone()));
        set_direct_mode_refusal_reason(Some(error.clone()));
        set_engine_state(EngineState::Error, Some(error.clone()));
        cleanup_claimed_handle(claimed_handle, None, lock_requested);
        return Err(Error::Claim(error));
    }

    if let Err(error) = claimed_handle.set_alternate_setting(candidate.interface_number, 0) {
        let message = format!(
            "Failed to reset USB interface {} to alt 0: {}",
            candidate.interface_number, error
        );
        set_last_error(Some(message.clone()));
        set_direct_mode_refusal_reason(Some(message.clone()));
        set_engine_state(EngineState::Error, Some(message.clone()));
        cleanup_claimed_handle(claimed_handle, None, lock_requested);
        return Err(Error::Transfer(message));
    }
    thread::sleep(Duration::from_millis(50));

    let mut clock_control_attempted = state.clock_control_attempted;
    let mut clock_control_succeeded = state.clock_control_succeeded;
    let mut clock_verification_passed = state.clock_verification_passed;
    let mut reported_sample_rate = state
        .clock_status
        .as_ref()
        .and_then(|status| status.reported_sample_rate);

    if let Some(clock) = clock {
        if let Err(error) = claimed_handle.ensure_interface_claimed(clock.interface_number) {
            set_last_error(Some(error.clone()));
            set_direct_mode_refusal_reason(Some(error.clone()));
            set_engine_state(EngineState::Error, Some(error.clone()));
            cleanup_claimed_handle(claimed_handle, Some(candidate.interface_number), lock_requested);
            return Err(Error::Claim(error));
        }
        match state.dac_mode {
            DacMode::FullControl => {
                // ALWAYS send SET_CUR + verify, even if a previous negotiation
                // already programmed the clock. The alt-setting reset to 0 above
                // can cause some DACs to revert to a default rate (384kHz etc.),
                // and cached state from a prior session cannot be trusted.
                let clock_outcome = apply_sampling_frequency(
                    &claimed_handle.handle,
                    clock.interface_number,
                    clock.clock_id,
                    playback_format.sample_rate,
                    clock_settle_delay_ms,
                );
                clock_control_attempted = true;
                clock_control_succeeded = clock_outcome.clock_ok;
                reported_sample_rate = clock_outcome.reported_sample_rate;
                clock_verification_passed = clock_outcome.rate_verified
                    && clock_outcome.reported_sample_rate == Some(playback_format.sample_rate);
                set_clock_verification(
                    clock_control_attempted,
                    clock_control_succeeded,
                    clock_verification_passed,
                    reported_sample_rate,
                );

                let clock_result_msg = format!(
                    "[clock-diag] SET_CUR {}Hz: clockOk={}, rateVerified={}, reported={}Hz, clockId={}, iface={}{}",
                    playback_format.sample_rate,
                    clock_outcome.clock_ok,
                    clock_outcome.rate_verified,
                    clock_outcome.reported_sample_rate.unwrap_or(0),
                    clock.clock_id,
                    clock.interface_number,
                    clock_outcome.message.as_ref().map(|m| format!(", msg={}", m)).unwrap_or_default(),
                );
                dev_eprintln!("{}", clock_result_msg);
                set_last_error(Some(clock_result_msg));

                if !clock_outcome.clock_ok {
                    dev_eprintln!(
                        "[USB] SET_CUR {}Hz -> FAILED: DAC reported={}Hz, verified={}",
                        playback_format.sample_rate,
                        clock_outcome.reported_sample_rate.unwrap_or(0),
                        clock_verification_passed,
                    );
                }

                if clock_outcome.known_mismatch {
                    dev_eprintln!(
                        "[USB] SET_CUR {}Hz -> MISMATCH: DAC reports {}Hz",
                        playback_format.sample_rate,
                        clock_outcome.reported_sample_rate.unwrap_or_default(),
                    );
                }

                dev_eprintln!(
                    "[USB] VALIDATION: usbClaimed={}, alt={}, endpoint=0x{:02x}, requestedRate={}Hz, dacRate={}Hz, verified={}",
                    claimed_handle.claimed_interfaces.contains(&candidate.interface_number),
                    candidate.alt_setting,
                    candidate.endpoint_address,
                    playback_format.sample_rate,
                    reported_sample_rate.unwrap_or(0),
                    clock_verification_passed,
                );

                if clock_outcome.set_cur_succeeded
                    && !clock_verification_passed
                    && !clock_outcome.known_mismatch
                {
                    clock_verification_passed = true;
                    reported_sample_rate = Some(playback_format.sample_rate);
                    set_clock_verification(
                        clock_control_attempted,
                        true,
                        true,
                        reported_sample_rate,
                    );
                    dev_eprintln!(
                        "[USB] SET_CUR accepted {}Hz on clock {} but readback unavailable (write-only clock); trusting SET_CUR",
                        playback_format.sample_rate, clock.clock_id,
                    );
                }
            }
            DacMode::FixedClock | DacMode::AdaptiveStreaming => {
                reported_sample_rate = get_sampling_frequency(
                    &claimed_handle.handle,
                    clock.interface_number,
                    clock.clock_id,
                )
                .ok()
                .or(reported_sample_rate);

                if let Some(actual_rate) = reported_sample_rate {
                    if actual_rate != playback_format.sample_rate {
                        let message = format!(
                            "USB direct: DAC reports {} Hz while stream is configured for {} Hz; continuing direct USB with unverified clock",
                            actual_rate, playback_format.sample_rate
                        );
                        set_clock_verification(
                            clock_control_attempted,
                            clock_control_succeeded,
                            false,
                            Some(actual_rate),
                        );
                        dev_eprintln!("{}", message);
                        dev_eprintln!(
                            "[USB] Actual sample rate={}Hz verified=false (reported by DAC, requested {}Hz)",
                            actual_rate,
                            playback_format.sample_rate,
                        );
                        clock_verification_passed = false;
                    } else {
                        clock_verification_passed = true;
                    }
                } else if let Ok(ranges) = get_sampling_frequency_ranges(
                    &claimed_handle.handle,
                    clock.interface_number,
                    clock.clock_id,
                ) {
                    if sampling_frequency_ranges_support_rate(&ranges, playback_format.sample_rate)
                    {
                        reported_sample_rate = Some(playback_format.sample_rate);
                        clock_verification_passed = true;
                        dev_eprintln!(
                            "USB direct: using GET_RANGE-verified fixed clock {} Hz on alt setting {} without SET_CUR",
                            playback_format.sample_rate, candidate.alt_setting
                        );
                    } else {
                        dev_eprintln!(
                            "[USB] Cannot verify fixed clock {}Hz for alt {} from GET_RANGE — will refuse streaming",
                            playback_format.sample_rate, candidate.alt_setting,
                        );
                        reported_sample_rate = None;
                        clock_verification_passed = false;
                        set_clock_verification(
                            clock_control_attempted,
                            clock_control_succeeded,
                            false,
                            None,
                        );
                    }
                } else {
                    dev_eprintln!(
                        "[USB] Cannot verify fixed clock {}Hz for alt {} because GET_RANGE is unavailable — will refuse streaming",
                        playback_format.sample_rate, candidate.alt_setting,
                    );
                    reported_sample_rate = None;
                    clock_verification_passed = false;
                    set_clock_verification(
                        clock_control_attempted,
                        clock_control_succeeded,
                        false,
                        None,
                    );
                }

                set_clock_verification(
                    clock_control_attempted,
                    clock_control_succeeded,
                    clock_verification_passed,
                    reported_sample_rate,
                );
            }
        }
    } else if candidate.is_uac1 {
        // UAC1 has no UAC2 clock entity; the sample rate is set endpoint-level
        // via SET_CUR (UAC1_SAM_FREQ_CONTROL 0x0100) on the streaming endpoint.
        // Re-issue it here because the alt-setting reset to 0 above may have
        // cleared device state, then trust the endpoint-level set. UAC1
        // endpoints are set-only (no GET_CUR readback for sample rate), so
        // clock "verification" is structurally impossible — a successful
        // SET_CUR is the strongest evidence available and is what the UAC1
        // spec defines as the rate-setting mechanism.
        match set_uac1_sampling_frequency(
            &claimed_handle.handle,
            candidate.interface_number,
            candidate.endpoint_address,
            playback_format.sample_rate,
        ) {
            Ok(()) => {
                clock_control_attempted = true;
                clock_control_succeeded = true;
                clock_verification_passed = true;
                reported_sample_rate = Some(playback_format.sample_rate);
                set_dac_mode(DacMode::FixedClock);
                set_clock_verification(
                    clock_control_attempted,
                    clock_control_succeeded,
                    clock_verification_passed,
                    reported_sample_rate,
                );
                let message = format!(
                    "[clock-diag] UAC1: SET_CUR {}Hz on endpoint 0x{:02x} succeeded — trusting endpoint-level rate (no UAC2 clock entity)",
                    playback_format.sample_rate, candidate.endpoint_address
                );
                dev_eprintln!("{}", message);
                set_last_error(Some(message));
            }
            Err(e) => {
                clock_control_attempted = true;
                clock_control_succeeded = false;
                // Only adaptive/synchronous UAC1 endpoints are host-paced, so a
                // rejected SET_CUR is tolerable there. Asynchronous endpoints run
                // their own clock and NoSync endpoints free-run entirely (FiiO
                // BR13: NoSync, stays locked at 48/96 kHz while we stream at the
                // file rate -> chipmunk), so a failed rate set leaves the DAC rate
                // unknown: refuse.
                if uac1_set_cur_failure_tolerable(candidate.sync_type) {
                    clock_verification_passed = true;
                    reported_sample_rate = Some(playback_format.sample_rate);
                    set_dac_mode(DacMode::AdaptiveStreaming);
                    set_clock_verification(
                        true,
                        false,
                        true,
                        Some(playback_format.sample_rate),
                    );
                    let message = format!(
                        "[clock-diag] UAC1 SET_CUR {}Hz on endpoint 0x{:02x} failed: {} — tolerable for {:?} endpoint, trusting host-driven rate",
                        playback_format.sample_rate, candidate.endpoint_address, e, candidate.sync_type,
                    );
                    dev_eprintln!("{}", message);
                    set_last_error(Some(message));
                } else {
                    clock_verification_passed = false;
                    reported_sample_rate = None;
                    set_clock_verification(true, false, false, None);
                    let message = format!(
                        "[clock-diag] UAC1 SET_CUR {}Hz on endpoint 0x{:02x} failed: {} — will refuse ({:?} endpoint is not host-paced)",
                        playback_format.sample_rate, candidate.endpoint_address, e, candidate.sync_type,
                    );
                    dev_eprintln!("{}", message);
                    set_last_error(Some(message));
                }
            }
        }
    } else {
        let message = format!(
            "[clock-diag] Cannot verify {}Hz: no clock entity for alt {} — will refuse",
            playback_format.sample_rate, candidate.alt_setting
        );
        reported_sample_rate = None;
        clock_verification_passed = false;
        set_clock_verification(
            clock_control_attempted,
            clock_control_succeeded,
            false,
            None,
        );
        dev_eprintln!("{}", message);
        set_last_error(Some(message));
    }

    if let Err(error) =
        claimed_handle.set_alternate_setting(candidate.interface_number, candidate.alt_setting)
    {
        let msg = format!(
            "Failed to set USB alt setting {} on interface {}: {}",
            candidate.alt_setting, candidate.interface_number, error
        );
        set_last_error(Some(msg.clone()));
        set_direct_mode_refusal_reason(Some(msg.clone()));
        set_engine_state(EngineState::Error, Some(msg.clone()));
        cleanup_claimed_handle(claimed_handle, None, lock_requested);
        return Err(Error::Transfer(msg));
    }
    thread::sleep(Duration::from_millis(50));

    // UAC1: endpoint SET_CUR is only reliable once the streaming alt setting
    // is active. Re-issue it; success here upgrades the clock verdict even if
    // the pre-alt attempt was rejected.
    if candidate.is_uac1 {
        match set_uac1_sampling_frequency(
            &claimed_handle.handle,
            candidate.interface_number,
            candidate.endpoint_address,
            playback_format.sample_rate,
        ) {
            Ok(()) => {
                clock_control_attempted = true;
                clock_control_succeeded = true;
                clock_verification_passed = true;
                reported_sample_rate = Some(playback_format.sample_rate);
                set_dac_mode(DacMode::FixedClock);
                set_clock_verification(true, true, true, reported_sample_rate);
                let message = format!(
                    "[clock-diag] UAC1: post-alt SET_CUR {}Hz on endpoint 0x{:02x} succeeded — DAC locked to requested rate",
                    playback_format.sample_rate, candidate.endpoint_address
                );
                dev_eprintln!("{}", message);
                set_last_error(Some(message));
            }
            Err(e) => {
                // SET_CUR stalled even with the alt setting active. Probe
                // GET_CUR: some UAC1 firmware auto-locks the rate and only
                // reports it read-only, in which case a matching readback is
                // sufficient verification.
                match get_uac1_sampling_frequency(
                    &claimed_handle.handle,
                    candidate.interface_number,
                    candidate.endpoint_address,
                ) {
                    Ok(reported) if reported == playback_format.sample_rate => {
                        clock_control_attempted = true;
                        clock_control_succeeded = false;
                        clock_verification_passed = true;
                        reported_sample_rate = Some(reported);
                        set_dac_mode(DacMode::FixedClock);
                        set_clock_verification(true, false, true, reported_sample_rate);
                        let message = format!(
                            "[clock-diag] UAC1: post-alt SET_CUR failed but GET_CUR reports requested {}Hz — trusting readback",
                            reported
                        );
                        dev_eprintln!("{}", message);
                        set_last_error(Some(message));
                    }
                    Ok(reported) => {
                        let message = format!(
                            "[clock-diag] UAC1: post-alt SET_CUR failed ({}) and GET_CUR reports {}Hz, requested {}Hz — keeping prior verdict",
                            e, reported, playback_format.sample_rate
                        );
                        dev_eprintln!("{}", message);
                        set_last_error(Some(message));
                    }
                    Err(readback_error) => {
                        let message = format!(
                            "[clock-diag] UAC1: post-alt SET_CUR failed: {} — keeping prior verdict (GET_CUR probe: {})",
                            e, readback_error
                        );
                        dev_eprintln!("{}", message);
                        set_last_error(Some(message));
                    }
                }
            }
        }
    }

    if !claimed_handle
        .claimed_interfaces
        .contains(&candidate.interface_number)
    {
        let refusal = format!(
            "USB direct refused: interface {} not claimed (endpoint=0x{:02x}, alt={})",
            candidate.interface_number, candidate.endpoint_address, candidate.alt_setting
        );
        set_last_error(Some(refusal.clone()));
        set_direct_mode_refusal_reason(Some(refusal.clone()));
        set_engine_state(EngineState::Error, Some(refusal.clone()));
        cleanup_claimed_handle(
            claimed_handle,
            Some(candidate.interface_number),
            lock_requested,
        );
        return Err(Error::Claim(refusal));
    }

    if candidate.alt_setting == 0 {
        let refusal = format!(
            "USB direct refused: invalid alt setting 0 on interface {} (endpoint=0x{:02x})",
            candidate.interface_number, candidate.endpoint_address
        );
        set_last_error(Some(refusal.clone()));
        set_direct_mode_refusal_reason(Some(refusal.clone()));
        set_engine_state(EngineState::Error, Some(refusal.clone()));
        cleanup_claimed_handle(
            claimed_handle,
            Some(candidate.interface_number),
            lock_requested,
        );
        return Err(Error::Unsupported {
            reason: refusal,
            offered: offered_alt_settings(),
        });
    }

    if candidate.endpoint_address == 0 {
        let refusal =
            "USB direct refused: no valid isochronous OUT endpoint found".to_string();
        set_last_error(Some(refusal.clone()));
        set_direct_mode_refusal_reason(Some(refusal.clone()));
        cleanup_claimed_handle(
            claimed_handle,
            Some(candidate.interface_number),
            lock_requested,
        );
        return Err(Error::Unsupported {
            reason: refusal,
            offered: offered_alt_settings(),
        });
    }

    // Post-alt-setting clock re-verification.
    // Some DACs reset their clock when the alt setting changes. Re-read
    // GET_CUR after switching to the streaming alt setting and if the
    // rate is wrong, try one more SET_CUR + verify cycle.
    // Skip for devices with broken clock readback (SkipClockValidation quirk).
    if let Some(clock) = clock {
        let skip_clock_validation = settings().skip_clock_validation();
        if skip_clock_validation && playback_format.dsd_transport == DsdTransportMode::None {
            dev_eprintln!(
                "[USB] Skipping post-alt GET_CUR re-verification (SkipClockValidation quirk)",
            );
        } else {
            match get_sampling_frequency(
                &claimed_handle.handle,
                clock.interface_number,
                clock.clock_id,
            ) {
                Ok(post_alt_rate) => {
                    dev_eprintln!(
                        "[USB] GET_CUR (post-alt): clock {} reports {}Hz (expected {}Hz)",
                        clock.clock_id, post_alt_rate, playback_format.sample_rate,
                    );
                    if post_alt_rate != playback_format.sample_rate {
                        dev_eprintln!(
                            "[USB] DAC clock drifted after alt-setting change ({} -> {}Hz); re-issuing SET_CUR",
                            playback_format.sample_rate, post_alt_rate,
                        );
                        let retry = apply_sampling_frequency(
                            &claimed_handle.handle,
                            clock.interface_number,
                            clock.clock_id,
                            playback_format.sample_rate,
                            clock_settle_delay_ms,
                        );
                        clock_control_attempted = true;
                        clock_control_succeeded = retry.clock_ok;
                        reported_sample_rate = retry.reported_sample_rate;
                        clock_verification_passed = retry.rate_verified
                            && retry.reported_sample_rate == Some(playback_format.sample_rate);
                        set_clock_verification(
                            clock_control_attempted,
                            clock_control_succeeded,
                            clock_verification_passed,
                            reported_sample_rate,
                        );
                        dev_eprintln!(
                            "[USB] Re-SET_CUR result: clockOk={}, verified={}, reported={}Hz",
                            retry.clock_ok,
                            clock_verification_passed,
                            retry.reported_sample_rate.unwrap_or(0),
                        );

                        if retry.set_cur_succeeded
                            && playback_format.dsd_transport == DsdTransportMode::None
                            && !clock_verification_passed
                            && !retry.known_mismatch
                        {
                            clock_verification_passed = true;
                            reported_sample_rate = Some(playback_format.sample_rate);
                            set_clock_verification(
                                clock_control_attempted,
                                true,
                                true,
                                reported_sample_rate,
                            );
                            dev_eprintln!(
                                "[USB] Re-SET_CUR accepted {}Hz on clock {} but readback unavailable (write-only clock); trusting SET_CUR",
                                playback_format.sample_rate, clock.clock_id,
                            );
                        }
                    } else if playback_format.dsd_transport != DsdTransportMode::None {
                        reported_sample_rate = Some(post_alt_rate);
                        clock_verification_passed = true;
                        set_clock_verification(clock_control_attempted, clock_control_succeeded, true, reported_sample_rate);
                    }
                }
                Err(error) => {
                    dev_eprintln!(
                        "[USB] GET_CUR (post-alt): failed for clock {}: {}",
                        clock.clock_id, error,
                    );
                    if playback_format.dsd_transport != DsdTransportMode::None {
                        reported_sample_rate = None;
                        clock_verification_passed = false;
                        set_clock_verification(clock_control_attempted, clock_control_succeeded, false, None);
                    }
                }
            }
        } // end of else (not skip_clock_validation)
    } else if candidate.is_uac1 && playback_format.dsd_transport != DsdTransportMode::None {
        reported_sample_rate = get_uac1_sampling_frequency(
            &claimed_handle.handle, candidate.interface_number, candidate.endpoint_address,
        ).ok();
        clock_verification_passed = reported_sample_rate == Some(playback_format.sample_rate);
        set_clock_verification(clock_control_attempted, clock_control_succeeded, clock_verification_passed, reported_sample_rate);
    }

    dev_eprintln!(
        "USB direct output opened '{}' requested {} Hz / {}-bit / {} ch, transport {} / {} ch / {}-byte subslot on endpoint 0x{:02x} (interface {}, alt {}, speed {:?})",
        state.device.product_name,
        playback_format.sample_rate,
        playback_format.bit_depth,
        playback_format.channels,
        format_tag_label(candidate.format_tag),
        candidate.channels,
        candidate.subslot_size,
        candidate.endpoint_address,
        candidate.interface_number,
        candidate.alt_setting,
        speed,
    );

    // Hard-abort: if the clock is unverified or mismatched, refuse to
    // stream. Sending audio at the wrong sample rate causes chipmunk /
    // distorted playback.  There is NO "continue anyway" path.
    if !clock_verification_passed {
        // Check if this device has the SkipClockValidation quirk.
        // Some DACs (e.g. Fosi DS2 / Savitech bridge) have broken UAC2 clock
        // control — GET_RANGE and GET_CUR always return 0. SET_CUR is accepted
        // silently. Skip verification and trust the SET_CUR.
        let skip_clock_validation = settings().skip_clock_validation();

        if skip_clock_validation && playback_format.dsd_transport == DsdTransportMode::None {
            dev_eprintln!(
                "[USB] Clock verification failed but SkipClockValidation quirk is active — trusting SET_CUR for {}Hz",
                playback_format.sample_rate,
            );
            clock_verification_passed = true;
            reported_sample_rate = Some(playback_format.sample_rate);
            set_clock_verification(
                clock_control_attempted,
                clock_control_succeeded,
                true,
                reported_sample_rate,
            );
        } else {
            set_clock_verification(
                clock_control_attempted,
                clock_control_succeeded,
                false,
                reported_sample_rate,
            );

            let refusal = format!(
                "USB direct refused: DAC clock not verified at {}Hz (reported={}Hz, attempted={}, succeeded={})",
                playback_format.sample_rate,
                reported_sample_rate.unwrap_or(0),
                clock_control_attempted,
                clock_control_succeeded,
            );
            if settings().require_verified_rate() {
                dev_eprintln!("[USB] {}", refusal);
                set_last_error(Some(refusal.clone()));
                set_direct_mode_refusal_reason(Some(refusal.clone()));
                set_engine_state(EngineState::Error, Some(refusal.clone()));
                cleanup_claimed_handle(
                    claimed_handle,
                    Some(candidate.interface_number),
                    lock_requested,
                );
                return Err(Error::ClockUnverified {
                    requested_hz: playback_format.sample_rate,
                    reported_hz: reported_sample_rate.filter(|rate| *rate > 0),
                });
            }
            // require_verified_rate is off: stream anyway. The clock stays
            // marked unverified, so the stream is never reported bit-perfect.
            log_warn!("[USB] {}; continuing because require_verified_rate is off", refusal);
        }
    }

    if let Some(rate) = reported_sample_rate {
        if rate != playback_format.sample_rate {
            // Check if this device has the IgnoreInvalidSampleRate quirk.
            // Some DACs (e.g. Fosi DS2) always report 0Hz via GET_CUR even
            // when operating correctly at the requested rate.
            let ignore_invalid_rate = settings().ignore_invalid_sample_rate();

            if ignore_invalid_rate && playback_format.dsd_transport == DsdTransportMode::None {
                dev_eprintln!(
                    "[USB] DAC reported {}Hz (expected {}Hz) but IgnoreInvalidSampleRate quirk is active — continuing",
                    rate, playback_format.sample_rate,
                );
                reported_sample_rate = Some(playback_format.sample_rate);
            } else {
                let refusal = format!(
                    "USB direct refused: DAC clock mismatch (requested {}Hz, reported {}Hz)",
                    playback_format.sample_rate, rate,
                );
                dev_eprintln!("[USB] {}", refusal);
                set_last_error(Some(refusal.clone()));
                set_direct_mode_refusal_reason(Some(refusal.clone()));
                set_engine_state(EngineState::Error, Some(refusal.clone()));
                cleanup_claimed_handle(
                    claimed_handle,
                    Some(candidate.interface_number),
                    lock_requested,
                );
                return Err(Error::ClockUnverified {
                    requested_hz: playback_format.sample_rate,
                    reported_hz: Some(rate),
                });
            }
        }
    }

    let bytes_per_frame = candidate.subslot_size as usize * candidate.channels as usize;
    if bytes_per_frame == 0 {
        let refusal = format!(
            "USB direct refused: zero-size frame (subslot={}, channels={})",
            candidate.subslot_size, candidate.channels,
        );
        dev_eprintln!("[USB] {}", refusal);
        set_last_error(Some(refusal.clone()));
        set_direct_mode_refusal_reason(Some(refusal.clone()));
        cleanup_claimed_handle(
            claimed_handle,
            Some(candidate.interface_number),
            lock_requested,
        );
        return Err(Error::Transfer(refusal));
    }

    dev_eprintln!("[USB] === PRE-STREAM VALIDATION ===");
    dev_eprintln!(
        "[USB]   requested_rate  = {}Hz",
        playback_format.sample_rate
    );
    dev_eprintln!(
        "[USB]   dac_reported    = {}Hz",
        reported_sample_rate.unwrap_or(0)
    );
    dev_eprintln!("[USB]   clock_verified  = {}", clock_verification_passed);
    dev_eprintln!("[USB]   bit_depth       = {}", playback_format.bit_depth);
    dev_eprintln!("[USB]   channels        = {}", playback_format.channels);
    dev_eprintln!("[USB]   subslot_size    = {} bytes", candidate.subslot_size);
    dev_eprintln!("[USB]   bytes_per_frame = {}", bytes_per_frame);
    dev_eprintln!(
        "[USB]   interval_us     = {}",
        candidate.service_interval_us
    );
    dev_eprintln!(
        "[USB]   max_packet_size = {} bytes",
        candidate.max_packet_bytes
    );
    dev_eprintln!("[USB] ============================");

    // A rate-set clock that reports Clock Valid = false cannot lock its PLL to
    // the requested carrier. The alt-setting switch above already happened, so
    // this is the post-switch state. DoP data is then clocked against this DAC's
    // free-running oscillator instead of the host's nominal packet timing; the
    // resulting beat shows up as slow, periodic chop (first after tens of
    // seconds, then every few seconds) that looks like a transport bug but is
    // really a clock-lock refusal.
    if let Some(clock) = clock {
        match get_clock_validity(&claimed_handle.handle, clock.interface_number, clock.clock_id) {
            Ok(valid) => {
                log_info!(
                    "[clock-diag] Clock Validity after switch: clock={} iface={} valid={} rate={}Hz",
                    clock.clock_id,
                    clock.interface_number,
                    valid,
                    playback_format.sample_rate,
                );
                if !valid {
                    dev_eprintln!(
                        "[clock-diag] DAC reports Clock Valid=false at {}Hz on clock {} — PLL has not locked to the DoP carrier; periodic dropouts are expected until it locks or accepts another rate",
                        playback_format.sample_rate,
                        clock.clock_id,
                    );
                }
            }
            Err(error) => dev_eprintln!(
                "[clock-diag] Clock Validity unavailable for clock {} at {}Hz: {}",
                clock.clock_id,
                playback_format.sample_rate,
                error,
            ),
        }
    }
    log_info!(
        "[USB] FINAL_STREAM_FORMAT: sample_rate_hz={} dac_reported_hz={:?} clock_verified={} \
         app_bit_depth={} transport_bit_resolution={} subslot_bytes={} channels={} \
         endpoint=0x{:02x} alt={} format_tag=0x{:04x} max_packet_bytes={}",
        playback_format.sample_rate,
        reported_sample_rate,
        clock_verification_passed,
        playback_format.bit_depth,
        candidate.bit_resolution,
        candidate.subslot_size,
        playback_format.channels,
        candidate.endpoint_address,
        candidate.alt_setting,
        candidate.format_tag,
        candidate.max_packet_bytes,
    );

    set_last_error(None);
    set_direct_mode_refusal_reason(None);
    let unsynchronized_async =
        candidate.sync_type == SyncType::Asynchronous && candidate.feedback_endpoint.is_none();
    if unsynchronized_async {
        log_warn!(
            "[USB] Streaming an asynchronous endpoint without a feedback endpoint at the nominal rate (allow_unsynchronized_async); the stream is not bit-perfect"
        );
    }
    set_unsynchronized_async(unsynchronized_async);
    set_stream_active(true);
    set_engine_state(EngineState::UsbReady, None);
    set_software_volume_active(false);
    let channels = playback_format.channels as usize;
    let runtime_stats = Arc::new(RuntimeStats::new(
        effective_ring_rate(&playback_format),
        channels,
        config.buffer_capacity_ms as usize,
        config.buffer_target_ms as usize,
        config.start_threshold_ms as usize,
        config.processing_buffer_target_ms as usize,
    ));
    set_runtime_stats(Some(Arc::clone(&runtime_stats)));
    let pcm_buffer = Arc::new(Mutex::new(PcmRing::new(
        runtime_stats.buffer_capacity_samples,
        channels,
    )));
    let shared = Arc::new(StreamShared::new(
        pcm_buffer,
        runtime_stats,
        events,
        playback_format,
        candidate.subslot_size,
    ));
    let stop = Arc::new(AtomicBool::new(false));

    let usb_stop = Arc::clone(&stop);
    let usb_shared = Arc::clone(&shared);
    let backend_product_name = state.device.product_name.clone();
    let backend_endpoint_address = candidate.endpoint_address;
    let backend_interface_number = candidate.interface_number;
    let backend_alt_setting = candidate.alt_setting;
    let output_session = Arc::clone(session);
    let realtime_priority = config.realtime_priority;
    // Publish before spawning: the output thread can fail immediately and must
    // not have its error state overwritten by the caller.
    set_engine_state(EngineState::Streaming, None);
    let thread_handle = match thread::Builder::new()
        .name("usbdac-output".to_string())
        .spawn(move || {
            // Keep the interfaces alive until the loop and its USB handles are done.
            let _session = output_session;
            if realtime_priority && crate::priority::raise_audio_priority().is_err() {
                dev_eprintln!("[AudioSched] Output thread running without SCHED_FIFO");
            }
            let panic_shared = Arc::clone(&usb_shared);
            let panic_stop = Arc::clone(&usb_stop);
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
                run_usb_output_loop(claimed_handle, candidate, state, usb_shared, usb_stop);
            }));
            if let Err(payload) = result {
                panic_stop.store(true, Ordering::Release);
                let message = format!(
                    "USB output thread panicked: {}",
                    panic_payload_message(payload.as_ref())
                );
                log_error!("{}", message);
                set_last_error(Some(message.clone()));
                set_engine_state(EngineState::Error, Some(message.clone()));
                panic_shared.fail(Error::Transfer(message));
                set_stream_active(false);
                set_runtime_stats(None);
                set_usb_stream_stable(false);
            }
        }) {
        Ok(handle) => handle,
        Err(error) => {
            set_stream_active(false);
            set_runtime_stats(None);
            set_engine_state(
                EngineState::Error,
                Some(format!("Failed to spawn the USB output thread: {}", error)),
            );
            return Err(Error::Transfer(format!(
                "Failed to spawn the USB output thread: {}",
                error
            )));
        }
    };

    let dsd_transport_label = match playback_format.dsd_transport {
        DsdTransportMode::Native => "Native",
        DsdTransportMode::DoP => "DoP",
        DsdTransportMode::None => "None",
    };
    log_info!(
        "[USB] Streaming started device='{}' requested={}Hz effective={}Hz endpoint=0x{:02x} interface={} alt={} dsd_transport={}",
        backend_product_name,
        requested_playback_format.sample_rate,
        playback_format.sample_rate,
        backend_endpoint_address,
        backend_interface_number,
        backend_alt_setting,
        dsd_transport_label,
    );
    Ok(StreamHandle {
        stop,
        thread: Some(thread_handle),
        shared,
    })
}

/// The alternate settings the DAC offers, for error reports.
pub(super) fn offered_alt_settings() -> Vec<crate::capabilities::AltSetting> {
    SESSION_STATE
        .lock()
        .as_ref()
        .and_then(|state| state.capability_model.as_ref())
        .map(|model| model.alt_settings.clone())
        .unwrap_or_default()
}
