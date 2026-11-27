use super::*;
use crate::error::OrControl;

/// Keep alternate ownership with the handle, including spawn failure and
/// unwinding. Declared before the transfer pools so their Drops retire USB
/// callbacks before this owner resets interfaces or releases the handle.
pub(super) struct StreamUsbOwner {
    claimed: Option<ClaimedHandle>,
    lock_requested: bool,
    capture: Option<Arc<duplex::Attachment>>,
}
impl StreamUsbOwner {
    fn new(
        claimed: ClaimedHandle,
        lock_requested: bool,
        capture: Option<Arc<duplex::Attachment>>,
    ) -> Self {
        Self {
            claimed: Some(claimed),
            lock_requested,
            capture,
        }
    }
    pub(super) fn claimed(&self) -> &ClaimedHandle {
        self.claimed.as_ref().expect("live USB stream owner")
    }
}
impl Drop for StreamUsbOwner {
    fn drop(&mut self) {
        if let Some(claimed) = self.claimed.take() {
            duplex::cleanup_claimed(claimed, self.capture.as_ref(), None, self.lock_requested);
        }
    }
}

/// What a running stream hands back to its owner.
pub(crate) struct StreamHandle {
    pub(crate) usb_handle_identity: usize,
    pub(crate) stop: Arc<AtomicBool>,
    pub(crate) thread: Option<JoinHandle<()>>,
    pub(crate) shared: Arc<StreamShared>,
    pub(crate) capture: Option<capture_stream::CaptureHandle>,
}

impl StreamHandle {
    /// Request stop and wait briefly; a stalled cancellation retains ownership in the worker.
    pub(crate) fn stop(&mut self) -> Result<(), String> {
        self.stop.store(true, Ordering::Release);
        match self.thread.take() {
            Some(handle) => {
                let deadline = Instant::now() + Duration::from_secs(3);
                while !handle.is_finished() && Instant::now() < deadline {
                    thread::sleep(Duration::from_millis(10));
                }
                if !handle.is_finished() {
                    // The worker retains SessionGuard, context, handle, FD and
                    // all transfer storage until cancellation really retires.
                    return Err("USB output is still retiring cancelled transfers".into());
                }
                handle.join().map_err(|payload| {
                    format!(
                        "USB output thread panicked: {}",
                        panic_payload_message(payload.as_ref())
                    )
                })
            }
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
pub(crate) fn start_stream(
    events: EventSink,
    expected: &crate::StreamFormat,
) -> Result<StreamHandle, Error> {
    start_stream_with_capture(events, expected, None)
}

pub(crate) fn start_stream_with_capture(
    events: EventSink,
    expected: &crate::StreamFormat,
    capture: Option<(&crate::capture::CaptureRequest, usize)>,
) -> Result<StreamHandle, Error> {
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
    start_stream_inner_with_capture(events, &session, expected, capture)
}

#[allow(unused_assignments)]
fn start_stream_inner_with_capture(
    events: EventSink,
    session: &Arc<SessionGuard>,
    expected: &crate::StreamFormat,
    capture: Option<(&crate::capture::CaptureRequest, usize)>,
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
    let usb_handle_identity = claimed_handle.handle.as_raw() as usize;
    let mut requested_playback_format = state.requested_playback_format.unwrap_or(playback_format);
    let device = claimed_handle.handle.device();
    let speed = resolve_device_speed(&device);
    let lock_requested = current_lock_requested_for_fd(state.device.fd);
    set_capability_model(
        build_usb_capability_model(
            &device,
            &claimed_handle.handle,
            speed,
            &claimed_handle.claimed_interfaces,
        )
        .ok(),
    );
    let candidate = match select_owned_stream_candidate(&claimed_handle, playback_format, speed) {
        Ok(candidate) => candidate,
        Err(error) => {
            set_last_error(Some(error.message().to_string()));
            set_direct_mode_refusal_reason(Some(error.message().to_string()));
            set_engine_state(EngineState::Error, Some(error.message().to_string()));
            cleanup_claimed_handle(claimed_handle, None, lock_requested);
            return Err(error.into_error(offered_alt_settings()));
        }
    };
    let vendor_profile = match vendor_pcm::for_candidate(&claimed_handle.handle, &candidate) {
        Ok(profile) => profile,
        Err(error) => {
            cleanup_claimed_handle(claimed_handle, None, lock_requested);
            return Err(Error::Transfer(error));
        }
    };
    let channel_map = match playback_map::for_candidate(&device, &claimed_handle.handle, &candidate)
    {
        Ok(map) => map,
        Err(error) => {
            cleanup_claimed_handle(claimed_handle, None, lock_requested);
            return Err(Error::Control(error));
        }
    };
    let selected_pacing = if let Some(source) = candidate.implicit_feedback {
        crate::capabilities::Pacing::ImplicitFeedback {
            interface_number: source.interface_number,
            alt_setting: source.alt_setting,
            endpoint: source.address,
        }
    } else if let Some(feedback) = candidate.feedback_endpoint {
        crate::capabilities::Pacing::ExplicitFeedback {
            endpoint: feedback.address,
        }
    } else if candidate.sync_type == SyncType::Asynchronous {
        crate::capabilities::Pacing::Unsynchronized
    } else {
        expected.pacing
    };
    let selected_rate = native_wire_rate(playback_format, candidate.subslot_size);
    if expected.interface_number != candidate.interface_number
        || expected.alt_setting != candidate.alt_setting
        || expected.endpoint_address != candidate.endpoint_address
        || expected.pacing != selected_pacing
        || Some(expected.sample_rate) != selected_rate
        || expected.subslot_bytes != candidate.subslot_size
        || expected.channels != candidate.channels
        || expected.channel_map != channel_map
        || expected.service_interval_us != candidate.service_interval_us
        || expected.native_dsd_signature
            != (if playback_format.dsd_transport == DsdTransportMode::Native {
                candidate.native_dsd.map(|p| p.signature())
            } else {
                None
            })
    {
        cleanup_claimed_handle(claimed_handle, None, lock_requested);
        return Err(Error::InvalidState(
            "USB stream candidate changed since negotiation; negotiate again".into(),
        ));
    }
    if playback_format.dsd_transport == DsdTransportMode::Native {
        // Native DSD clock frames contain subslot_size bytes per channel.
        // The producer still supplies interleaved byte-rate samples.
        let standard_adc3 = candidate.native_dsd.is_some_and(|profile| {
            profile.origin == native_dsd::NativeDsdOrigin::StandardAdc3
                && profile.word_bytes == 8
                && candidate.subslot_size == 8
                && candidate.bit_resolution == 64
        });
        if (!matches!(candidate.subslot_size, 1 | 2 | 4) && !standard_adc3)
            || selected_rate.is_none()
        {
            let error = "USB native DSD refused: invalid subslot or source bit rate".to_string();
            set_last_error(Some(error.clone()));
            set_direct_mode_refusal_reason(Some(error.clone()));
            cleanup_claimed_handle(claimed_handle, None, lock_requested);
            return Err(Error::Unsupported {
                reason: error,
                offered: offered_alt_settings(),
            });
        }
        let wire_rate = selected_rate.expect("validated native word cadence");
        playback_format.sample_rate = wire_rate;
        requested_playback_format.sample_rate = wire_rate;
        if let Some(current) = SESSION_STATE.lock().as_mut() {
            current.playback_format = Some(playback_format);
            current.requested_playback_format = Some(requested_playback_format);
        }
    }
    let adc3_route = match if vendor_profile.is_some() || candidate.is_uac1 {
        Ok(None)
    } else {
        adc3_route_for_candidate(&device, &claimed_handle.handle, &candidate)
    } {
        Ok(route) => route,
        Err(error) => {
            cleanup_claimed_handle(claimed_handle, None, lock_requested);
            return Err(Error::Transfer(error));
        }
    };
    if let Some(route) = adc3_route.as_ref() {
        if let Err(error) = verify_adc3_route(route) {
            cleanup_claimed_handle(claimed_handle, None, lock_requested);
            return Err(Error::InvalidState(error));
        }
    }
    let clock_choice = if vendor_profile.is_some() || candidate.is_uac1 {
        None
    } else if let Some(route) = adc3_route.as_ref() {
        route.choice.clone().map(|choice| {
            (
                AudioControlClock {
                    interface_number: choice.control_interface,
                    clock_id: choice.source_id,
                },
                choice,
            )
        })
    } else {
        match candidate_audio_control_clock_choice(&device, &claimed_handle.handle, &candidate) {
            Ok(choice) => choice,
            Err(error) => {
                cleanup_claimed_handle(claimed_handle, None, lock_requested);
                return Err(Error::Transfer(error));
            }
        }
    };
    if clock_choice.as_ref().map(|(_, choice)| choice) != expected.clock.as_ref() {
        cleanup_claimed_handle(claimed_handle, None, lock_requested);
        return Err(Error::InvalidState(
            "clock route changed since negotiation".into(),
        ));
    }
    // Select and validate the complete pair while all streaming alternates are idle.
    let mut attachment = match capture
        .map(|(request, capacity)| {
            duplex::prepare(&claimed_handle, &candidate, expected, request, capacity)
        })
        .transpose()
    {
        Ok(value) => value,
        Err(error) => {
            cleanup_claimed_handle(claimed_handle, None, lock_requested);
            return Err(error);
        }
    };
    if let Some(route) = adc3_route.as_ref() {
        if let Err(error) =
            apply_adc3_clock_plan(&claimed_handle, route, playback_format.sample_rate)
        {
            duplex::cleanup_claimed(claimed_handle, attachment.as_ref(), None, lock_requested);
            return Err(Error::Transfer(error));
        }
    }
    // ADC3 must never enter the generic UAC2 frequency programming branch.
    let uac2_choice = if adc3_route.is_some() {
        None
    } else {
        clock_choice.as_ref().map(|(_, choice)| choice)
    };
    let multiplied_clock = uac2_choice_has_multiplier(uac2_choice);
    if let Err(error) = uac2_clock_ratio(uac2_choice) {
        duplex::cleanup_claimed(claimed_handle, attachment.as_ref(), None, lock_requested);
        return Err(Error::Transfer(error));
    }
    let clock = if adc3_route.is_some() {
        None
    } else {
        clock_choice.as_ref().map(|(clock, _)| *clock)
    };
    let clock_detection_msg = if let Some(profile) = vendor_profile.as_ref() {
        format!(
            "vendor PCM model-backed {}Hz (programmable={}); live implicit lock required",
            playback_format.sample_rate, profile.rate_control
        )
    } else {
        match (adc3_route.as_ref(), &clock) {
            (Some(route), _) => match route.choice.as_ref() {
                Some(choice) => format!(
                    "ADC3 clock resolved: interface={}, clockId={}, via_terminal_link={:?}",
                    choice.control_interface, choice.source_id, candidate.terminal_link
                ),
                None => "ADC3 BADD fixed 48000Hz clock".to_string(),
            },
            (None, Some(c)) => format!(
                "Clock found: interface={}, clockId={}, via_terminal_link={:?}",
                c.interface_number, c.clock_id, candidate.terminal_link,
            ),
            (None, None) => {
                "Clock NOT FOUND: no UAC2 clock entity in any AudioControl descriptor".to_string()
            }
        }
    };
    dev_eprintln!(
        "[USB] Clock entity for '{}': {}",
        state.device.product_name,
        clock_detection_msg
    );

    set_last_error(Some(format!("[clock-diag] {}", clock_detection_msg)));
    set_direct_mode_refusal_reason(None);
    set_usb_stream_stable(false);
    // The hardware volume control of the terminal this stream feeds. Normally
    // the one found when the DAC was opened; replaced if the stream plays
    // through a different Feature Unit.
    if vendor_profile.is_some() {
        // The source-proven vendor interface has no class volume contract.
        // Fixed output remains an explicit host opt-in through the normal guard.
        install_volume_control(None);
        set_hardware_volume_error(Some(
            "vendor PCM has fixed output without a proven hardware volume control".into(),
        ));
    } else {
        refresh_stream_volume_control(
            &state.device,
            &device,
            candidate.terminal_link,
            candidate.interface_number,
            candidate.channels,
            adc3_route
                .as_ref()
                .map(|route| route.control)
                .or_else(|| clock.map(|clock| clock.interface_number)),
        );
    }
    // The stream's own terminal may have no volume control, or one the start-up
    // step could not make safe: with a control required, nothing is programmed.
    if let Err(error) = ensure_volume_if_required(config.require_hardware_volume) {
        set_last_error(Some(error.message()));
        set_direct_mode_refusal_reason(Some(error.message()));
        set_engine_state(EngineState::Error, Some(error.message()));
        duplex::cleanup_claimed(claimed_handle, attachment.as_ref(), None, lock_requested);
        return Err(error);
    }
    set_clock_status(
        (candidate.is_uac1
            || clock.is_some()
            || adc3_route
                .as_ref()
                .and_then(|route| route.choice.as_ref())
                .is_some())
        .then(|| ClockStatus {
            reported_sample_rate: state
                .clock_status
                .as_ref()
                .and_then(|status| status.reported_sample_rate),
        }),
    );
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
        duplex::cleanup_claimed(claimed_handle, attachment.as_ref(), None, lock_requested);
        return Err(Error::Claim(error));
    }

    if let Some(source) = candidate
        .implicit_feedback
        .filter(|source| source.interface_number != candidate.interface_number)
    {
        if !claimed_handle
            .claimed_interfaces
            .contains(&source.interface_number)
        {
            duplex::cleanup_claimed(claimed_handle, attachment.as_ref(), None, lock_requested);
            return Err(Error::Claim(
                "implicit source interface was not claimed".into(),
            ));
        }
        if let Err(error) = claimed_handle.ensure_interface_claimed(source.interface_number) {
            duplex::cleanup_claimed(claimed_handle, attachment.as_ref(), None, lock_requested);
            return Err(Error::Claim(error));
        }
        if let Err(error) = claimed_handle.set_alternate_setting(source.interface_number, 0) {
            duplex::cleanup_claimed(claimed_handle, attachment.as_ref(), None, lock_requested);
            return Err(Error::Transfer(error));
        }
    }
    if let Err(error) = claimed_handle.set_alternate_setting(candidate.interface_number, 0) {
        let message = format!(
            "Failed to reset USB interface {} to alt 0: {}",
            candidate.interface_number, error
        );
        set_last_error(Some(message.clone()));
        set_direct_mode_refusal_reason(Some(message.clone()));
        set_engine_state(EngineState::Error, Some(message.clone()));
        duplex::cleanup_claimed(claimed_handle, attachment.as_ref(), None, lock_requested);
        return Err(Error::Transfer(message));
    }
    thread::sleep(Duration::from_millis(50));
    if let Err(error) = if vendor_profile.is_some() {
        Ok(())
    } else {
        apply_itf_mode(&claimed_handle, &candidate, playback_format.dsd_transport)
    } {
        duplex::cleanup_claimed(claimed_handle, attachment.as_ref(), None, lock_requested);
        return Err(Error::Transfer(error));
    }
    if let Err(error) = if vendor_profile.is_some() || candidate.is_uac1 {
        Ok(())
    } else {
        verify_requested_clock_route(&claimed_handle, playback_format.sample_rate)
    } {
        duplex::cleanup_claimed(claimed_handle, attachment.as_ref(), None, lock_requested);
        return Err(Error::InvalidState(error));
    }

    let mut clock_control_attempted = state.clock_control_attempted;
    if settings().has(crate::Quirk::SetInterfaceFirst)
        && candidate.implicit_feedback.is_none()
        && playback_format.dsd_transport == DsdTransportMode::None
    {
        if let Err(error) =
            claimed_handle.set_alternate_setting(candidate.interface_number, candidate.alt_setting)
        {
            duplex::cleanup_claimed(
                claimed_handle,
                attachment.as_ref(),
                Some(candidate.interface_number),
                lock_requested,
            );
            return Err(Error::Transfer(format!(
                "interface-first activation failed: {error}"
            )));
        }
    }
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
            duplex::cleanup_claimed(
                claimed_handle,
                attachment.as_ref(),
                Some(candidate.interface_number),
                lock_requested,
            );
            return Err(Error::Claim(error));
        }
        match state.dac_mode {
            DacMode::FullControl => {
                // ALWAYS send SET_CUR + verify, even if a previous negotiation
                // already programmed the clock. The alt-setting reset to 0 above
                // can cause some DACs to revert to a default rate (384kHz etc.),
                // and cached state from a prior session cannot be trusted.
                let clock_outcome = apply_wire_frequency(
                    &claimed_handle.handle,
                    clock,
                    uac2_choice,
                    playback_format.sample_rate,
                    clock_settle_delay_ms,
                );
                clock_control_attempted = true;
                clock_control_succeeded = clock_outcome.set_cur_succeeded || clock_outcome.clock_ok;
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
            }
            DacMode::FixedClock | DacMode::AdaptiveStreaming => {
                reported_sample_rate =
                    get_wire_frequency(&claimed_handle.handle, clock, uac2_choice)
                        .ok()
                        .or(if multiplied_clock {
                            None
                        } else {
                            reported_sample_rate
                        });

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
                } else if let Ok(ranges) =
                    get_wire_frequency_ranges(&claimed_handle.handle, clock, uac2_choice)
                {
                    if !multiplied_clock
                        && sampling_frequency_ranges_support_rate(
                            &ranges,
                            playback_format.sample_rate,
                        )
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
    } else if let Some(route) = adc3_route.as_ref() {
        if let Err(error) =
            configure_adc3(&claimed_handle.handle, route, playback_format.sample_rate)
        {
            duplex::cleanup_claimed(claimed_handle, attachment.as_ref(), None, lock_requested);
            return Err(Error::Transfer(error));
        }
        clock_control_attempted = true;
        clock_control_succeeded = true;
        clock_verification_passed = true;
        reported_sample_rate = Some(playback_format.sample_rate);
        set_clock_verification(true, true, true, reported_sample_rate);
    } else if let Some(profile) = vendor_profile.as_ref() {
        clock_control_attempted = false;
        clock_control_succeeded = false;
        clock_verification_passed = !profile.rate_control
            && playback_format.dsd_transport == DsdTransportMode::None
            && profile.supports_rate(playback_format.sample_rate);
        reported_sample_rate = if profile.rate_control {
            None
        } else {
            Some(playback_format.sample_rate)
        };
        set_clock_verification(
            false,
            false,
            clock_verification_passed,
            reported_sample_rate,
        );
    } else if candidate.is_uac1 {
        // No live endpoint rate evidence survives SET_INTERFACE. Populate it
        // only after the playback alternate has been activated below.
        clock_control_attempted = false;
        clock_control_succeeded = false;
        clock_verification_passed = false;
        reported_sample_rate = None;
        set_clock_verification(false, false, false, None);
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

    if let Some(capture) = attachment
        .as_mut()
        .filter(|_| vendor_profile.is_none() && !candidate.is_uac1)
    {
        if let Err(error) =
            duplex::configure_with_retry(capture, &claimed_handle, &candidate, expected)
        {
            duplex::cleanup_claimed(claimed_handle, attachment.as_ref(), None, lock_requested);
            return Err(error);
        }
    }
    if let Some(route) = adc3_route.as_ref() {
        if let Err(error) = verify_adc3_implicit_activation(
            &claimed_handle,
            &candidate,
            route,
            playback_format.sample_rate,
        ) {
            duplex::cleanup_claimed(claimed_handle, attachment.as_ref(), None, lock_requested);
            return Err(Error::Transfer(error));
        }
    }
    if let Some(source) = candidate.implicit_feedback {
        if source.interface_number != candidate.interface_number {
            if let Err(error) = claimed_handle.ensure_interface_claimed(source.interface_number) {
                duplex::cleanup_claimed(
                    claimed_handle,
                    attachment.as_ref(),
                    Some(candidate.interface_number),
                    lock_requested,
                );
                return Err(Error::Claim(error));
            }
            if let Err(error) =
                claimed_handle.set_alternate_setting(source.interface_number, source.alt_setting)
            {
                duplex::cleanup_claimed(
                    claimed_handle,
                    attachment.as_ref(),
                    Some(candidate.interface_number),
                    lock_requested,
                );
                return Err(Error::Transfer(format!(
                    "implicit feedback alternate activation failed: {}",
                    error
                )));
            }
        } else if source.alt_setting != candidate.alt_setting {
            duplex::cleanup_claimed(
                claimed_handle,
                attachment.as_ref(),
                Some(candidate.interface_number),
                lock_requested,
            );
            return Err(Error::Transfer(
                "implicit source alternate conflicts with playback".into(),
            ));
        }
    }
    if vendor_profile.is_some() {
        if let Err(error) =
            vendor_pcm::for_candidate(&claimed_handle.handle, &candidate).and_then(|profile| {
                if profile == vendor_profile {
                    Ok(())
                } else {
                    Err("vendor PCM signature changed before activation".into())
                }
            })
        {
            duplex::cleanup_claimed(claimed_handle, attachment.as_ref(), None, lock_requested);
            return Err(Error::Transfer(error));
        }
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
        duplex::cleanup_claimed(claimed_handle, attachment.as_ref(), None, lock_requested);
        return Err(Error::Transfer(msg));
    }
    if let Some(capture) = attachment.as_ref().filter(|capture| {
        capture.shared_implicit
            && !candidate.is_uac1
            && capture.endpoint.vendor_pcm.is_none()
            && capture.endpoint.adc3_route.is_none()
    }) {
        // Playback activated the shared IN source. Reprove its pinned capture
        // contract after both SET_INTERFACE operations, before publishing it.
        if let Err(error) =
            capture_stream::verify_standard_capture(&claimed_handle, &capture.endpoint)
        {
            duplex::cleanup_claimed(
                claimed_handle,
                attachment.as_ref(),
                Some(candidate.interface_number),
                lock_requested,
            );
            return Err(error);
        }
    }
    if vendor_profile.is_some() && attachment.is_none() {
        if let Err(error) =
            vendor_pcm::for_candidate(&claimed_handle.handle, &candidate).and_then(|profile| {
                if profile == vendor_profile {
                    Ok(())
                } else {
                    Err("vendor PCM signature changed after activation".into())
                }
            })
        {
            duplex::cleanup_claimed(
                claimed_handle,
                attachment.as_ref(),
                Some(candidate.interface_number),
                lock_requested,
            );
            return Err(Error::Transfer(error));
        }
    }
    if let Some(profile) = vendor_profile.as_ref() {
        let verification = (|| {
            if let Some(capture) = attachment.as_mut() {
                duplex::configure_with_retry(capture, &claimed_handle, &candidate, expected)?;
            } else {
                capture_stream::verify_vendor_interface_alternate(
                    &claimed_handle,
                    profile.interface_number,
                    profile.alt_setting,
                )?;
                if profile.input_interface_number != profile.interface_number {
                    capture_stream::verify_vendor_interface_alternate(
                        &claimed_handle,
                        profile.input_interface_number,
                        profile.input_alt_setting,
                    )?;
                }
            }
            Ok(())
        })();
        if let Err(error) = verification {
            duplex::cleanup_claimed(
                claimed_handle,
                attachment.as_ref(),
                Some(candidate.interface_number),
                lock_requested,
            );
            return Err(error);
        }
    }
    // Program only after both the exact profile and active streaming
    // alternates are proven. The helper independently reproves the endpoint
    // request contract; a successful write still requires USB cadence proof.
    let programmed_vendor_rate = vendor_profile
        .as_ref()
        .is_some_and(|profile| profile.rate_control);
    if let Some(profile) = vendor_profile
        .as_ref()
        .filter(|profile| profile.rate_control)
    {
        clock_control_attempted = true;
        if let Err(error) =
            vendor_pcm::program_rate(&claimed_handle, profile, playback_format.sample_rate)
        {
            set_clock_verification(true, false, false, None);
            duplex::cleanup_claimed(
                claimed_handle,
                attachment.as_ref(),
                Some(candidate.interface_number),
                lock_requested,
            );
            return Err(error);
        }
        clock_control_succeeded = true;
        clock_verification_passed = false;
        reported_sample_rate = None;
        set_clock_verification(true, true, false, None);
    }
    thread::sleep(Duration::from_millis(50));

    let mut adc3_status_clock_proven = false;
    let mut uac1_status_rate = None;
    if let Some(route) = adc3_route.as_ref() {
        if let Err(error) =
            configure_adc3(&claimed_handle.handle, route, playback_format.sample_rate)
        {
            duplex::cleanup_claimed(
                claimed_handle,
                attachment.as_ref(),
                Some(candidate.interface_number),
                lock_requested,
            );
            return Err(Error::Transfer(error));
        }
        // Both OUT and any embedded implicit IN alternate are active now.
        // Prove their advertised live PCM format before publishing the stream.
        if let Err(error) =
            adc3::verify_adc3_active_formats(&claimed_handle.handle, route).or_control()
        {
            duplex::cleanup_claimed(
                claimed_handle,
                attachment.as_ref(),
                Some(candidate.interface_number),
                lock_requested,
            );
            return Err(error);
        }
        adc3_status_clock_proven = route.clock.is_some();
    }
    if let Some(route) = attachment
        .as_ref()
        .filter(|capture| !capture.shared_implicit)
        .and_then(|capture| capture.endpoint.adc3_route.as_ref())
    {
        // Final OUT activation/programming can affect independently activated IN.
        if let Err(error) =
            adc3::verify_adc3_active_formats(&claimed_handle.handle, route).or_control()
        {
            duplex::cleanup_claimed(
                claimed_handle,
                attachment.as_ref(),
                Some(candidate.interface_number),
                lock_requested,
            );
            return Err(error);
        }
    }

    // A UAC1 SET_CUR ACK may round to a supported rate. Read the active
    // endpoint even when SET stalls: an exact positive CUR also proves a
    // read-only clock. Explicitly absent controls use only descriptor evidence.
    if candidate.is_uac1 {
        let profile = match uac1_playback_rate_profile(
            &claimed_handle,
            &candidate,
            playback_format.sample_rate,
        ) {
            Ok(profile) => profile,
            Err(error) => {
                duplex::cleanup_claimed(
                    claimed_handle,
                    attachment.as_ref(),
                    Some(candidate.interface_number),
                    lock_requested,
                );
                return Err(if crate::error::is_device_gone(&error) {
                    Error::DeviceLost
                } else {
                    Error::Control(error)
                });
            }
        };
        let fixed_readback = if settings().has(crate::Quirk::FixedRate) && profile.fixed_at_request
        {
            get_uac1_sampling_frequency(
                &claimed_handle.handle,
                candidate.interface_number,
                candidate.endpoint_address,
            )
            .ok()
            .filter(|rate| {
                fixed_rate_live_proof(
                    playback_format.sample_rate,
                    Some(*rate),
                    profile.fixed_at_request,
                    true,
                )
            })
        } else {
            None
        };
        if let Some(capture) = attachment.as_ref() {
            match capture.configure_uac1(&claimed_handle, &candidate, expected) {
                Ok((attempted, succeeded, reported)) => {
                    clock_control_attempted = attempted;
                    clock_control_succeeded = succeeded;
                    reported_sample_rate = reported;
                }
                Err(error) => {
                    set_clock_verification(false, false, false, None);
                    duplex::cleanup_claimed(
                        claimed_handle,
                        attachment.as_ref(),
                        Some(candidate.interface_number),
                        lock_requested,
                    );
                    return Err(error);
                }
            }
        } else if let Some(rate) = fixed_readback {
            clock_control_attempted = false;
            clock_control_succeeded = false;
            reported_sample_rate = Some(rate);
        } else if profile.sampling_control != Some(false) {
            clock_control_attempted = true;
            let set_result = set_uac1_sampling_frequency(
                &claimed_handle.handle,
                candidate.interface_number,
                candidate.endpoint_address,
                playback_format.sample_rate,
            );
            clock_control_succeeded = set_result.is_ok();
            if let Err(error) = set_result {
                if crate::error::is_device_gone(&error) {
                    set_clock_verification(true, false, false, None);
                    duplex::cleanup_claimed(
                        claimed_handle,
                        attachment.as_ref(),
                        Some(candidate.interface_number),
                        lock_requested,
                    );
                    return Err(Error::DeviceLost);
                }
                dev_eprintln!("[clock-diag] UAC1 post-alt SET_CUR failed: {}", error);
            }
            match get_uac1_sampling_frequency(
                &claimed_handle.handle,
                candidate.interface_number,
                candidate.endpoint_address,
            ) {
                Ok(rate) => reported_sample_rate = (rate > 0).then_some(rate),
                Err(error) => {
                    if crate::error::is_device_gone(&error) {
                        set_clock_verification(
                            clock_control_attempted,
                            clock_control_succeeded,
                            false,
                            None,
                        );
                        duplex::cleanup_claimed(
                            claimed_handle,
                            attachment.as_ref(),
                            Some(candidate.interface_number),
                            lock_requested,
                        );
                        return Err(Error::DeviceLost);
                    }
                    dev_eprintln!("[clock-diag] UAC1 post-alt GET_CUR unavailable: {}", error);
                    reported_sample_rate = None;
                }
            }
        }
        set_dac_mode(if profile.host_paced_at_request {
            DacMode::AdaptiveStreaming
        } else {
            DacMode::FixedClock
        });
        clock_verification_passed = uac1_playback_rate_verified(
            playback_format.sample_rate,
            reported_sample_rate,
            profile,
            playback_format.dsd_transport == DsdTransportMode::None,
        );
        // Tag only this activation's exact readback from an explicitly
        // controllable endpoint. Descriptor and host-paced admission never
        // authorize a later sampling-frequency CUR claim.
        if profile.sampling_control == Some(true)
            && clock_verification_passed
            && playback_format.sample_rate > 0
            && reported_sample_rate == Some(playback_format.sample_rate)
        {
            uac1_status_rate = Some(playback_format.sample_rate);
        }
        set_clock_verification(
            clock_control_attempted,
            clock_control_succeeded,
            clock_verification_passed,
            reported_sample_rate,
        );
        let message = format!(
            "[clock-diag] UAC1 post-alt endpoint=0x{:02x}: requested={}Hz reported={:?}Hz attempted={} succeeded={} verified={}",
            candidate.endpoint_address, playback_format.sample_rate, reported_sample_rate,
            clock_control_attempted, clock_control_succeeded, clock_verification_passed,
        );
        dev_eprintln!("{}", message);
        set_last_error(Some(message));
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
        duplex::cleanup_claimed(
            claimed_handle,
            attachment.as_ref(),
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
        duplex::cleanup_claimed(
            claimed_handle,
            attachment.as_ref(),
            Some(candidate.interface_number),
            lock_requested,
        );
        return Err(Error::Unsupported {
            reason: refusal,
            offered: offered_alt_settings(),
        });
    }

    if candidate.endpoint_address == 0 {
        let refusal = "USB direct refused: no valid isochronous OUT endpoint found".to_string();
        set_last_error(Some(refusal.clone()));
        set_direct_mode_refusal_reason(Some(refusal.clone()));
        duplex::cleanup_claimed(
            claimed_handle,
            attachment.as_ref(),
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
    let mut post_alt_readback_hz = None;
    if let Some(clock) = clock {
        let skip_clock_validation = settings().skip_clock_validation();
        if skip_clock_validation
            && !multiplied_clock
            && playback_format.dsd_transport == DsdTransportMode::None
        {
            dev_eprintln!(
                "[USB] Skipping post-alt GET_CUR re-verification (SkipClockValidation quirk)",
            );
        } else {
            match get_wire_frequency(&claimed_handle.handle, clock, uac2_choice) {
                Ok(post_alt_rate) => {
                    dev_eprintln!(
                        "[USB] GET_CUR (post-alt): clock {} reports {}Hz (expected {}Hz)",
                        clock.clock_id,
                        post_alt_rate,
                        playback_format.sample_rate,
                    );
                    if multiplied_clock {
                        // The route plan owns root programming. A changed source or
                        // multiplier after activation must not be silently repaired.
                        reported_sample_rate = Some(post_alt_rate);
                        clock_verification_passed = post_alt_rate == playback_format.sample_rate;
                        if clock_verification_passed {
                            // get_wire_frequency verifies the active multiplier
                            // route and converts the live source CUR to wire Hz.
                            post_alt_readback_hz = Some(post_alt_rate);
                        }
                        set_clock_verification(
                            clock_control_attempted,
                            clock_control_succeeded,
                            clock_verification_passed,
                            reported_sample_rate,
                        );
                    } else if post_alt_rate != playback_format.sample_rate {
                        dev_eprintln!(
                            "[USB] DAC clock drifted after alt-setting change ({} -> {}Hz); re-issuing SET_CUR",
                            playback_format.sample_rate, post_alt_rate,
                        );
                        let retry = apply_wire_frequency(
                            &claimed_handle.handle,
                            clock,
                            uac2_choice,
                            playback_format.sample_rate,
                            clock_settle_delay_ms,
                        );
                        clock_control_attempted = true;
                        clock_control_succeeded = retry.set_cur_succeeded || retry.clock_ok;
                        reported_sample_rate = retry.reported_sample_rate;
                        clock_verification_passed = retry.rate_verified
                            && retry.reported_sample_rate == Some(playback_format.sample_rate);
                        if clock_verification_passed {
                            post_alt_readback_hz = retry.reported_sample_rate;
                        }
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
                    } else {
                        post_alt_readback_hz = Some(post_alt_rate);
                        if playback_format.dsd_transport != DsdTransportMode::None {
                            reported_sample_rate = Some(post_alt_rate);
                            clock_verification_passed = true;
                            set_clock_verification(
                                clock_control_attempted,
                                clock_control_succeeded,
                                true,
                                reported_sample_rate,
                            );
                        }
                    }
                }
                Err(error) => {
                    dev_eprintln!(
                        "[USB] GET_CUR (post-alt): failed for clock {}: {}",
                        clock.clock_id,
                        error,
                    );
                    if multiplied_clock || playback_format.dsd_transport != DsdTransportMode::None {
                        reported_sample_rate = None;
                        clock_verification_passed = false;
                        set_clock_verification(
                            clock_control_attempted,
                            clock_control_succeeded,
                            false,
                            None,
                        );
                    }
                }
            }
        } // end of else (not skip_clock_validation)
    } else if let Some(route) = adc3_route.as_ref() {
        // A Full ADC3 route exposes a real source CUR. Reuse the activation
        // verifier, which performs no writes and checks the live source rate,
        // multiplier ratio, selector route and validity after SET_INTERFACE.
        // BADD's descriptor-defined fixed rate is not independent readback.
        if route.clock.is_some()
            && candidate.sync_type == SyncType::Asynchronous
            && candidate.feedback_endpoint.is_some()
            && candidate.usb_speed == Speed::High
            && candidate.service_interval_us > 125
        {
            match verify_adc3_capture_activation(
                &claimed_handle.handle,
                route,
                playback_format.sample_rate,
            ) {
                Ok(()) => post_alt_readback_hz = Some(playback_format.sample_rate),
                Err(error) => log_warn!(
                    "[USB] ADC3 post-alt clock readback unavailable for feedback inference: {}",
                    error
                ),
            }
        }
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

    // Missing PCM clock evidence may use the explicit unverified-rate opt-in.
    // UAC1 positive mismatches and DSD still require exact live evidence.
    if !clock_verification_passed && !programmed_vendor_rate {
        // Check if this device has the SkipClockValidation quirk.
        // Some DACs (e.g. Fosi DS2 / Savitech bridge) have broken UAC2 clock
        // control — GET_RANGE and GET_CUR always return 0. SET_CUR is accepted
        // silently. Skip verification and trust the SET_CUR.
        let unverified_tier = settings().allow_unusable_clock_readback(
            clock_control_attempted,
            clock_control_succeeded,
            reported_sample_rate,
            multiplied_clock,
            playback_format.dsd_transport != DsdTransportMode::None,
        );
        set_clock_verification(
            clock_control_attempted,
            clock_control_succeeded,
            false,
            reported_sample_rate,
        );
        let known_mismatch = reported_sample_rate
            .is_some_and(|rate| rate > 0 && rate != playback_format.sample_rate);
        if known_mismatch
            || (clock_control_attempted && !clock_control_succeeded)
            || ((!unverified_tier && settings().require_verified_rate()) || multiplied_clock)
            || playback_format.dsd_transport != DsdTransportMode::None
        {
            let refusal = format!(
                "USB direct refused: clock not verified at {}Hz (reported={:?}, SET succeeded={})",
                playback_format.sample_rate, reported_sample_rate, clock_control_succeeded
            );
            set_last_error(Some(refusal.clone()));
            set_direct_mode_refusal_reason(Some(refusal.clone()));
            set_engine_state(EngineState::Error, Some(refusal));
            duplex::cleanup_claimed(
                claimed_handle,
                attachment.as_ref(),
                Some(candidate.interface_number),
                lock_requested,
            );
            return Err(Error::ClockUnverified {
                requested_hz: playback_format.sample_rate,
                reported_hz: reported_sample_rate.filter(|rate| *rate > 0),
            });
        }
        log_warn!("[USB] Clock unverified: unusable GET_CUR after successful SET; consider a device-specific readback quirk after hardware validation");
    }

    // Zero is an unusable reply, never a verified/requested frequency.
    if reported_sample_rate == Some(0) {
        reported_sample_rate = None;
    }
    if let Some(rate) = reported_sample_rate {
        if rate != playback_format.sample_rate && (!candidate.is_uac1 || rate > 0) {
            // Check if this device has the IgnoreInvalidSampleRate quirk.
            // Some DACs (e.g. Fosi DS2) always report 0Hz via GET_CUR even
            // when operating correctly at the requested rate.
            let ignore_invalid_rate = settings().ignore_invalid_sample_rate();

            if ignore_invalid_rate
                && !candidate.is_uac1
                && !multiplied_clock
                && playback_format.dsd_transport == DsdTransportMode::None
            {
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
                duplex::cleanup_claimed(
                    claimed_handle,
                    attachment.as_ref(),
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
        duplex::cleanup_claimed(
            claimed_handle,
            attachment.as_ref(),
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
        match get_clock_validity(
            &claimed_handle.handle,
            clock.interface_number,
            clock.clock_id,
        ) {
            Ok(valid) => {
                log_info!(
                    "[clock-diag] Clock Validity after switch: clock={} iface={} valid={} rate={}Hz",
                    clock.clock_id,
                    clock.interface_number,
                    valid,
                    playback_format.sample_rate,
                );
                if !valid {
                    post_alt_readback_hz = None;
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

    // Retain the exact selected route and raw root rate; never derive a
    // Clock Source from a wire cadence alone (UAC1, BADD and vendor PCM).
    let status_targets = (|| -> Result<Vec<audio_status::StreamStatusTarget>, Error> {
        let mut targets = Vec::new();
        if vendor_profile.is_none() {
            if candidate.is_uac1 {
                // Cached descriptor discovery is optional: missing or
                // ambiguous collection ownership leaves playback unchanged.
                if let Ok(config) = claimed_handle.handle.device().active_config_descriptor() {
                    let standard = config
                        .interfaces()
                        .flat_map(|i| i.descriptors())
                        .any(|alt| {
                            alt.interface_number() == candidate.interface_number
                                && alt.setting_number() == candidate.alt_setting
                                && alt.class_code() == USB_CLASS_AUDIO
                                && alt.sub_class_code() == USB_SUBCLASS_AUDIOSTREAMING
                                && alt.protocol_code() == 0
                        });
                    if let Some(control) =
                        uac1_status::collection_owner(&config, candidate.interface_number).filter(
                            |control| {
                                standard && claimed_handle.claimed_interfaces.contains(control)
                            },
                        )
                    {
                        targets.push(audio_status::StreamStatusTarget::uac1(
                            control,
                            candidate.interface_number,
                            candidate.alt_setting,
                            candidate.endpoint_address,
                            uac1_status_rate,
                            candidate.terminal_link,
                        ));
                    }
                }
            }
            if let Some((_, choice)) = clock_choice.as_ref().filter(|_| !candidate.is_uac1) {
                // AS status remains useful without frequency evidence, but a
                // nominal/ignored/fallback rate must never become a CUR claim.
                let proven_clock = if clock_verification_passed
                    && (if adc3_route.is_some() {
                        adc3_status_clock_proven
                    } else {
                        post_alt_readback_hz == Some(playback_format.sample_rate)
                    })
                    && choice.resolution == crate::clock_resolution::ClockResolution::Resolved
                {
                    let source_rate =
                        if let Some(route) = adc3_route.as_ref() {
                            route
                                .source_rate(playback_format.sample_rate)
                                .map_err(Error::Control)?
                        } else {
                            uac2_clock_ratio(Some(choice))
                                .map_err(Error::Control)?
                                .source_rate(playback_format.sample_rate)
                                .ok_or_else(|| {
                                    Error::Control(
                                "status watch cannot represent the exact playback root rate".into())
                                })?
                        };
                    Some((choice, source_rate))
                } else {
                    None
                };
                let protocol = if adc3_route.is_some() { 0x30 } else { 0x20 };
                let mut target = audio_status::StreamStatusTarget::new(
                    protocol,
                    choice.control_interface,
                    candidate.interface_number,
                    candidate.alt_setting,
                    proven_clock,
                );
                if let Some(route) = adc3_route.as_ref() {
                    for (interface, expected_mask) in route.active_format_expectations() {
                        target.add_adc3_active_format(interface, expected_mask);
                    }
                }
                target.add_data_endpoint(
                    candidate.interface_number,
                    candidate.alt_setting,
                    candidate.endpoint_address,
                );
                if let Some(source) = candidate.implicit_feedback {
                    target.add_data_endpoint(
                        source.interface_number,
                        source.alt_setting,
                        source.address,
                    );
                }
                targets.push(target);
                if let (Some(source), Some((_, root_rate))) =
                    (candidate.implicit_feedback, proven_clock)
                {
                    let peer = if let Some(route) = adc3_route.as_ref() {
                        route.implicit_status_route().and_then(|peer| {
                            peer.choice.as_ref().map(|choice| {
                                (
                                    choice.clone(),
                                    peer.source_rate(playback_format.sample_rate),
                                )
                            })
                        })
                    } else {
                        claimed_handle
                            .handle
                            .device()
                            .active_config_descriptor()
                            .ok()
                            .and_then(|config| {
                                let terminal = config
                                    .interfaces()
                                    .flat_map(|i| i.descriptors())
                                    .find(|alt| {
                                        alt.interface_number() == source.interface_number
                                            && alt.setting_number() == source.alt_setting
                                    })
                                    .and_then(|alt| parse_android_streaming_interface_format(&alt))
                                    .and_then(|format| format.terminal_link);
                                strict_implicit_clock(
                                    &config,
                                    &claimed_handle.handle,
                                    choice.control_interface,
                                    terminal,
                                )
                                .map(|peer| {
                                    let rate = uac2_clock_ratio(Some(&peer)).and_then(|ratio| {
                                        ratio.source_rate(playback_format.sample_rate).ok_or_else(
                                            || "implicit status root rate is not exact".into(),
                                        )
                                    });
                                    (peer, rate)
                                })
                            })
                    };
                    if let Some((peer, Ok(peer_rate))) = peer {
                        if peer.control_interface == choice.control_interface
                            && peer.source_id == choice.source_id
                            && peer_rate == root_rate
                        {
                            targets.push(audio_status::StreamStatusTarget::new(
                                protocol,
                                peer.control_interface,
                                source.interface_number,
                                source.alt_setting,
                                Some((&peer, peer_rate)),
                            ));
                        }
                    }
                }
            }
        }
        if let Some(capture) = attachment.as_ref() {
            if let Some(target) = capture_stream::status_target(&claimed_handle, &capture.endpoint)?
            {
                targets.push(target);
            }
        }
        Ok(targets)
    })();
    let status_targets = match status_targets {
        Ok(targets) => targets,
        Err(error) => {
            duplex::cleanup_claimed(
                claimed_handle,
                attachment.as_ref(),
                Some(candidate.interface_number),
                lock_requested,
            );
            return Err(error);
        }
    };

    // Resolve only the selected standard endpoint after its exact alternate
    // has been established. Vendor routes and asynchronous pacing keep their
    // separate bootstrap/feedback contracts.
    let lock_delay = if vendor_profile.is_none()
        && matches!(
            candidate.sync_type,
            SyncType::Synchronous | SyncType::Adaptive
        ) {
        match endpoint_lock::selected_delay(
            &claimed_handle,
            candidate.interface_number,
            candidate.alt_setting,
            candidate.endpoint_address,
            playback_format.dsd_transport != DsdTransportMode::Native,
        )
        .or_control()
        {
            Ok(delay) => delay,
            Err(error) => {
                duplex::cleanup_claimed(
                    claimed_handle,
                    attachment.as_ref(),
                    Some(candidate.interface_number),
                    lock_requested,
                );
                return Err(error);
            }
        }
    } else {
        None
    };

    set_last_error(None);
    set_direct_mode_refusal_reason(None);
    let unsynchronized_async = candidate.sync_type == SyncType::Asynchronous
        && candidate.feedback_endpoint.is_none()
        && candidate.implicit_feedback.is_none();
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
    let compatibility_admissions = CompatibilityAdmissions::new(
        unsynchronized_async,
        // Vendor-rate programming has its own cadence proof and wire protocol;
        // its ACK is never a standard frequency SET/GET_CUR fallback.
        clock_control_attempted && !programmed_vendor_rate,
        clock_control_succeeded,
        clock_verification_passed,
        reported_sample_rate,
    );
    let runtime_stats = Arc::new(
        RuntimeStats::new(
            effective_ring_rate(&playback_format),
            channels,
            config.buffer_capacity_ms as usize,
            config.buffer_target_ms as usize,
            config.start_threshold_ms as usize,
            config.processing_buffer_target_ms as usize,
        )
        .with_feedback_trace(config.capture_feedback_trace),
    );
    if let Some(trace) = runtime_stats.feedback_trace.as_ref() {
        let suggestions: Vec<_> = state
            .unreadable_clocks
            .lock()
            .iter()
            .map(|(key, strikes)| (*key, *strikes))
            .collect();
        trace.record_clock_readback_suggestions(suggestions);
    }
    runtime_stats
        .implicit_feedback
        .store(candidate.implicit_feedback.is_some(), Ordering::Release);
    runtime_stats.feedback_required.store(
        candidate.implicit_feedback.is_some()
            || (candidate.sync_type == SyncType::Asynchronous
                && candidate.feedback_endpoint.is_some()),
        Ordering::Release,
    );
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

    let reader_attachment = attachment.clone();
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
    let stream_usb_owner = StreamUsbOwner::new(claimed_handle, lock_requested, attachment.clone());
    let worker_context = context::current();
    let thread_handle = match thread::Builder::new()
        .name("usbdac-output".to_string())
        .spawn(move || {
            let _owner_scope = worker_context.enter();
            // Keep the interfaces alive until the loop and its USB handles are done.
            let _capture_retirement = attachment
                .as_ref()
                .map(|capture| capture.retirement_guard());
            let _session = output_session;
            if realtime_priority && crate::priority::raise_audio_priority().is_err() {
                dev_eprintln!("[AudioSched] Output thread running without SCHED_FIFO");
            }
            let panic_shared = Arc::clone(&usb_shared);
            let panic_stop = Arc::clone(&usb_stop);
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
                run_usb_output_loop(
                    stream_usb_owner,
                    candidate,
                    state,
                    usb_shared,
                    usb_stop,
                    attachment,
                    post_alt_readback_hz,
                    status_targets,
                    lock_delay,
                    compatibility_admissions,
                );
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
    let capture_handle = reader_attachment
        .as_ref()
        .map(|capture| capture.reader(Arc::clone(&stop)));
    Ok(StreamHandle {
        usb_handle_identity,
        stop,
        thread: Some(thread_handle),
        shared,
        capture: capture_handle,
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
