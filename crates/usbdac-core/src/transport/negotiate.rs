use super::*;

// A read made before a clock write cannot establish the clock after that write.
fn negotiation_reported_rate(
    attempted: bool,
    before: Option<u32>,
    after: Option<u32>,
) -> Option<u32> {
    after.filter(|rate| *rate > 0).or_else(|| {
        (!attempted)
            .then_some(before)
            .flatten()
            .filter(|rate| *rate > 0)
    })
}

fn adaptive_rate_verified(
    attempted: bool,
    before: Option<u32>,
    after: Option<u32>,
    verified_after: bool,
    actual: u32,
) -> bool {
    (!attempted && before == Some(actual)) || (verified_after && after == Some(actual))
}

/// Programs and verifies the DAC clock for `requested_format` and settles on the
/// format the DAC will actually run (which differs only when the exact rate is
/// not available). Runs with the alternate setting at 0 and leaves it there.
///
/// Also returns which clock the rate was programmed on and how it was reached
/// (`None` when the DAC has no clock entities).
#[allow(unused_assignments)]
pub(super) fn negotiate_playback_format(
    requested_format: PlaybackFormat,
) -> Result<(PlaybackFormat, Option<ClockChoice>), crate::Error> {
    use crate::Error;
    clear_adc3_negotiation();
    let device = {
        let mut guard = SESSION_STATE.lock();
        let Some(state) = guard.as_mut() else {
            return Ok((requested_format, None));
        };
        state.negotiated_uac2_alternate = None;
        state.device.clone()
    };

    let claimed_handle = open_claimed_usb_handle(&device).map_err(Error::Claim)?;
    let lock_requested = current_lock_requested_for_fd(device.fd);
    let mut cleanup_interface: Option<u8> = None;
    let mut clock_choice: Option<ClockChoice> = None;
    let mut uac2_alternate = None;
    let negotiated = (|| {
        // SessionGuard excludes transfers; restore the preceding negotiation's
        // selector state before discovering this request's candidates.
        restore_requested_clock_route(&claimed_handle).map_err(Error::Transfer)?;
        let usb_device = claimed_handle.handle.device();
        let speed = resolve_device_speed(&usb_device);
        let capability_model = build_usb_capability_model(
            &usb_device,
            &claimed_handle.handle,
            speed,
            &claimed_handle.claimed_interfaces,
        )
        .ok();
        set_capability_model(capability_model.clone());

        let candidate =
            match select_owned_stream_candidate(&claimed_handle, requested_format, speed) {
                Ok(c) => c,
                Err(error) => {
                    set_last_error(Some(error.message().to_string()));
                    set_direct_mode_refusal_reason(Some(error.message().to_string()));
                    return Err(error.into_error(offered_alt_settings()));
                }
            };

        // Vendor pairs have no AudioControl clock. Negotiation stays read-only;
        // programmable DJM rates await stream setup and observed IN cadence.
        if let Some(profile) = vendor_pcm::for_candidate(&claimed_handle.handle, &candidate)
            .map_err(Error::Transfer)?
        {
            if requested_format.dsd_transport != DsdTransportMode::None
                || !profile.supports_rate(requested_format.sample_rate)
            {
                return Err(Error::Unsupported {
                    reason: "vendor PCM requires a supported native PCM rate".into(),
                    offered: offered_alt_settings(),
                });
            }
            set_effective_playback_format(requested_format);
            set_dac_mode(if profile.rate_control {
                DacMode::FullControl
            } else {
                DacMode::FixedClock
            });
            set_clock_status(None);
            set_clock_verification(
                false,
                false,
                !profile.rate_control,
                (!profile.rate_control).then_some(requested_format.sample_rate),
            );
            return Ok(requested_format);
        }

        // Clock controls and packet pacing use transport words, not source bytes.
        let requested_format = PlaybackFormat {
            sample_rate: native_wire_rate(requested_format, candidate.subslot_size).ok_or_else(
                || Error::Unsupported {
                    reason: "native DSD rate is not a whole wire word cadence".into(),
                    offered: offered_alt_settings(),
                },
            )?,
            ..requested_format
        };

        if let Err(error) = claimed_handle.ensure_interface_claimed(candidate.interface_number) {
            set_last_error(Some(error.clone()));
            set_direct_mode_refusal_reason(Some(error.clone()));
            return Err(Error::Claim(error));
        }

        if let Err(error) = claimed_handle.set_alternate_setting(candidate.interface_number, 0) {
            let msg = format!(
                "Failed to reset USB interface {} to alt 0 before negotiation: {}",
                candidate.interface_number, error
            );
            set_last_error(Some(msg.clone()));
            set_direct_mode_refusal_reason(Some(msg.clone()));
            return Err(Error::Transfer(msg));
        }
        cleanup_interface = Some(candidate.interface_number);
        thread::sleep(Duration::from_millis(100));
        apply_itf_mode(&claimed_handle, &candidate, requested_format.dsd_transport)
            .map_err(Error::Transfer)?;

        if !candidate.is_uac1
            && adc3_route_for_candidate(&usb_device, &claimed_handle.handle, &candidate)
                .map_err(Error::Transfer)?
                .is_some()
        {
            let route =
                negotiate_adc3_clock(&claimed_handle, &candidate, requested_format.sample_rate)
                    .map_err(Error::Transfer)?;
            clock_choice = route.choice.clone();
            set_clock_status(route.choice.as_ref().map(|_| ClockStatus {
                reported_sample_rate: None,
            }));
            remember_adc3_route(route);
            set_effective_playback_format(requested_format);
            set_dac_mode(DacMode::FixedClock);
            set_clock_verification(true, true, true, Some(requested_format.sample_rate));
            return Ok(requested_format);
        }
        // UAC1 owns endpoint rate controls, even in a configuration that also
        // contains UAC2 functions. Never bind another function's clock leaf.
        let clock = if candidate.is_uac1 {
            None
        } else {
            select_requested_clock_route(&claimed_handle, &candidate, requested_format.sample_rate)
                .map_err(Error::Transfer)?;
            candidate_audio_control_clock_choice(&usb_device, &claimed_handle.handle, &candidate)
                .map_err(Error::Transfer)?
                .map(|(clock, choice)| {
                    clock_choice = Some(choice);
                    clock
                })
        };

        let mut effective_format = requested_format;
        let mut dac_mode = DacMode::FullControl;
        let mut clock_attempted = false;
        let mut clock_succeeded = false;
        let mut rate_verified = false;
        let mut reported_rate = None;
        let mut last_message = None;

        if let Some(clock) = clock {
            if let Err(error) = claimed_handle.ensure_interface_claimed(clock.interface_number) {
                set_last_error(Some(error.clone()));
                set_direct_mode_refusal_reason(Some(error.clone()));
                return Err(Error::Claim(error));
            }
            set_clock_status(Some(ClockStatus {
                reported_sample_rate: None,
            }));
            // Raw multiplier hops must never degrade to the generic fixed-clock fallback.
            uac2_clock_ratio(clock_choice.as_ref()).map_err(Error::Transfer)?;
            if let Some(choice) = clock_choice.as_ref() {
                verify_uac2_clock_factors(&claimed_handle.handle, choice)
                    .map_err(Error::Transfer)?;
            }
            let current_rate =
                get_wire_frequency(&claimed_handle.handle, clock, clock_choice.as_ref()).ok();
            let supported_ranges =
                get_wire_frequency_ranges(&claimed_handle.handle, clock, clock_choice.as_ref())
                    .ok();

            if supported_ranges.as_ref().is_some_and(|ranges| {
                sampling_frequency_ranges_support_rate(ranges, requested_format.sample_rate)
            }) || supported_ranges.is_none()
            {
                log::info!(
                    "[USB] NEGOTIATION: usbClaimed={}, alt={}, endpoint=0x{:02x}, rate={}Hz",
                    claimed_handle
                        .claimed_interfaces
                        .contains(&candidate.interface_number),
                    candidate.alt_setting,
                    candidate.endpoint_address,
                    requested_format.sample_rate,
                );
                if settings().has(crate::Quirk::SetInterfaceFirst)
                    && candidate.implicit_feedback.is_none()
                    && requested_format.dsd_transport == DsdTransportMode::None
                {
                    claimed_handle
                        .set_alternate_setting(candidate.interface_number, candidate.alt_setting)
                        .map_err(|error| {
                            Error::Transfer(format!(
                                "interface-first negotiation activation failed: {error}"
                            ))
                        })?;
                }
                let clock_outcome = apply_wire_frequency(
                    &claimed_handle.handle,
                    clock,
                    clock_choice.as_ref(),
                    requested_format.sample_rate,
                    settings().settle_delay_ms(),
                );
                clock_attempted = true;
                clock_succeeded = clock_outcome.set_cur_succeeded || clock_outcome.clock_ok;
                rate_verified = clock_outcome.rate_verified;
                reported_rate = negotiation_reported_rate(
                    true,
                    current_rate,
                    clock_outcome.reported_sample_rate,
                );
                last_message = clock_outcome.message.clone();
                log::info!(
                    "[USB] NEGOTIATION CLOCK: confirmed={}Hz, clockOk={}, rateVerified={}, clockId={}, iface={}",
                    clock_outcome.reported_sample_rate.unwrap_or(0),
                    clock_outcome.clock_ok,
                    clock_outcome.rate_verified,
                    clock.clock_id,
                    clock.interface_number,
                );
                let unusable_after_set = clock_outcome.set_cur_succeeded
                    && clock_outcome
                        .reported_sample_rate
                        .map_or(true, |rate| rate == 0)
                    && !clock_outcome.known_mismatch;
                let allow_unverified = settings().allow_unusable_clock_readback(
                    true,
                    clock_outcome.set_cur_succeeded,
                    clock_outcome.reported_sample_rate,
                    uac2_choice_has_multiplier(clock_choice.as_ref()),
                    requested_format.dsd_transport != DsdTransportMode::None,
                );
                if unusable_after_set && !allow_unverified {
                    return Err(Error::ClockUnverified {
                        requested_hz: requested_format.sample_rate,
                        reported_hz: None,
                    });
                }
                if !clock_outcome.set_cur_succeeded && !clock_outcome.clock_ok {
                    return Err(Error::ClockUnverified {
                        requested_hz: requested_format.sample_rate,
                        reported_hz: clock_outcome.reported_sample_rate.filter(|rate| *rate > 0),
                    });
                }
                if allow_unverified {
                    dac_mode = DacMode::FullControl;
                    set_effective_playback_format(requested_format);
                    set_dac_mode(dac_mode);
                    set_clock_verification(
                        true,
                        true,
                        false,
                        clock_outcome.reported_sample_rate.filter(|rate| *rate > 0),
                    );
                    return Ok(requested_format);
                }
                if clock_outcome.rate_verified
                    && clock_outcome.reported_sample_rate == Some(requested_format.sample_rate)
                {
                    if let Some(choice) = clock_choice.as_ref().filter(|choice| {
                        choice.resolution == crate::clock_resolution::ClockResolution::Resolved
                    }) {
                        uac2_alternate =
                            match super::uac2_live_alternate::select_uac2_live_alternate(
                                &claimed_handle,
                                &candidate,
                                requested_format,
                                choice,
                            ) {
                                Ok(alternate) => alternate,
                                Err(error) => {
                                    // Multiplied routes already own a complete source
                                    // snapshot in the clock planner's outer rollback.
                                    let restore = if clock_outcome.set_cur_succeeded
                                        && !uac2_choice_has_multiplier(Some(choice))
                                    {
                                        restore_uac2_rate_after_alternate_failure(
                                            &claimed_handle,
                                            &candidate,
                                            clock,
                                            choice,
                                            current_rate,
                                            requested_format.sample_rate,
                                        )
                                    } else {
                                        Ok(())
                                    };
                                    return Err(Error::Transfer(match restore {
                                    Ok(()) => error,
                                    Err(restore_error) => format!("{error}; UAC2 source rate restoration uncertain: {restore_error}"),
                                }));
                                }
                            };
                    }
                    dac_mode = DacMode::FullControl;
                    set_effective_playback_format(requested_format);
                    set_dac_mode(dac_mode);
                    set_clock_verification(
                        clock_attempted,
                        clock_succeeded,
                        rate_verified,
                        reported_rate,
                    );
                    if let Some(message) = last_message {
                        dev_eprintln!("USB direct: {}", message);
                    }
                    return Ok(requested_format);
                }
            } else {
                last_message = supported_ranges.as_ref().map(|ranges| {
                    format!(
                        "Requested {} Hz is not supported by clock {} on interface {}; supported rates: {}",
                        requested_format.sample_rate,
                        clock.clock_id,
                        clock.interface_number,
                        format_sampling_frequency_ranges(ranges),
                    )
                });
            }

            if uac2_choice_has_multiplier(clock_choice.as_ref()) && current_rate.is_none() {
                return Err(Error::ClockUnverified {
                    requested_hz: requested_format.sample_rate,
                    reported_hz: reported_rate,
                });
            }
            if requested_format.dsd_transport == DsdTransportMode::Native {
                return Err(Error::Unsupported {
                    reason: "native DSD requires its exact wire clock rate".into(),
                    offered: offered_alt_settings(),
                });
            }
            let adaptive_rate = if uac2_choice_has_multiplier(clock_choice.as_ref()) {
                // A multiplied route can fall back only to its proven current
                // wire cadence; RANGE alone does not establish a running rate.
                current_rate.filter(|rate| {
                    select_stream_candidate(
                        &usb_device,
                        &claimed_handle.handle,
                        PlaybackFormat {
                            sample_rate: *rate,
                            ..requested_format
                        },
                        speed,
                        &claimed_handle.claimed_interfaces,
                    )
                    .is_ok()
                })
            } else {
                choose_adaptive_sample_rate(
                    requested_format.sample_rate,
                    current_rate,
                    supported_ranges.as_deref(),
                    capability_model.as_ref(),
                    &usb_device,
                    &claimed_handle.handle,
                    speed,
                    requested_format,
                    &claimed_handle.claimed_interfaces,
                )?
            };
            if let Some(actual_rate) = adaptive_rate {
                effective_format.sample_rate = actual_rate;
                dac_mode = if actual_rate == requested_format.sample_rate {
                    DacMode::FixedClock
                } else {
                    DacMode::AdaptiveStreaming
                };
                rate_verified = adaptive_rate_verified(
                    clock_attempted,
                    current_rate,
                    reported_rate,
                    rate_verified,
                    actual_rate,
                );
                reported_rate =
                    negotiation_reported_rate(clock_attempted, current_rate, reported_rate);
            } else {
                if uac2_choice_has_multiplier(clock_choice.as_ref()) {
                    return Err(Error::ClockUnverified {
                        requested_hz: requested_format.sample_rate,
                        reported_hz: reported_rate,
                    });
                }
                let message = last_message.unwrap_or_else(|| {
                    format!(
                        "Unable to verify DAC rate for requested {} Hz during negotiation; continuing with direct USB at the requested stream rate",
                        requested_format.sample_rate
                    )
                });
                reported_rate =
                    negotiation_reported_rate(clock_attempted, current_rate, reported_rate);
                last_message = Some(message);
            }
        } else if candidate.is_uac1 {
            // UAC1 rate control belongs to the active streaming endpoint.
            // Negotiation neither writes it nor claims a live rate readback.
            dac_mode = DacMode::FixedClock;
            set_clock_status(Some(ClockStatus {
                reported_sample_rate: None,
            }));
            last_message = Some(format!(
                "UAC 1.0 device: endpoint 0x{:02x} rate evidence awaits streaming alternate activation",
                candidate.endpoint_address,
            ));
        } else if let Some(actual_rate) = choose_adaptive_sample_rate(
            requested_format.sample_rate,
            None,
            None,
            capability_model.as_ref(),
            &usb_device,
            &claimed_handle.handle,
            speed,
            requested_format,
            &claimed_handle.claimed_interfaces,
        )? {
            effective_format.sample_rate = actual_rate;
            dac_mode = if actual_rate == requested_format.sample_rate {
                DacMode::FixedClock
            } else {
                DacMode::AdaptiveStreaming
            };
            let inferred_message = if actual_rate == requested_format.sample_rate {
                format!(
                    "USB direct: no clock control entity; using fixed clock {} Hz",
                    actual_rate
                )
            } else {
                format!(
                    "USB direct: SET_CUR unavailable or unsupported; using adaptive mode at {} Hz for requested {} Hz",
                    actual_rate, requested_format.sample_rate
                )
            };
            dev_eprintln!("{}", inferred_message);
            last_message = Some(inferred_message);
        } else {
            let message = format!(
                "Unable to determine a usable fixed clock rate for requested {} Hz during negotiation; continuing direct USB with assumed stream rate",
                requested_format.sample_rate
            );
            last_message = Some(message);
        }

        // A changed effective rate must satisfy the same pairing and capacity
        // proof. In particular an implicit clock cannot silently degrade to the
        // playback path's unverified fixed-clock fallback.
        if candidate.implicit_feedback.is_some() {
            if !rate_verified
                || clock_choice.as_ref().map_or(true, |choice| {
                    choice.resolution != crate::clock_resolution::ClockResolution::Resolved
                })
            {
                return Err(Error::ImplicitFeedbackUnsupported);
            }
            let selected = select_stream_candidate(
                &usb_device,
                &claimed_handle.handle,
                effective_format,
                speed,
                &claimed_handle.claimed_interfaces,
            )
            .map_err(|e| e.into_error(offered_alt_settings()))?;
            if selected.implicit_feedback != candidate.implicit_feedback {
                return Err(Error::ImplicitFeedbackUnsupported);
            }
        }
        set_effective_playback_format(effective_format);
        set_dac_mode(dac_mode);
        set_clock_verification(
            clock_attempted,
            clock_succeeded,
            rate_verified,
            reported_rate,
        );
        if let Some(message) = last_message {
            dev_eprintln!("USB direct: {}", message);
        }
        Ok(effective_format)
    })();

    if negotiated.is_err() {
        if let Err(error) = restore_requested_clock_route(&claimed_handle) {
            log_warn!("[USB] Failed negotiation could not restore clock selectors: {error}");
        }
    }
    let cleanup_ok = cleanup_claimed_handle(claimed_handle, cleanup_interface, lock_requested);
    if !cleanup_ok || negotiated.is_err() {
        clear_adc3_negotiation();
    }
    if !cleanup_ok && negotiated.is_ok() {
        return Err(Error::Transfer(
            "USB negotiation could not restore a PCM-safe idle state".into(),
        ));
    }
    if negotiated.is_ok() {
        if let Some(state) = SESSION_STATE.lock().as_mut() {
            state.negotiated_uac2_alternate = uac2_alternate;
        }
    }
    negotiated.map(|format| (format, clock_choice))
}

/// Roll back only a source whose pre-write cadence and current route are proven.
/// Never write through a changed selector route or invent a missing old rate.
fn restore_uac2_rate_after_alternate_failure(
    claimed: &ClaimedHandle,
    candidate: &IsoStreamCandidate,
    clock: AudioControlClock,
    choice: &ClockChoice,
    previous_rate: Option<u32>,
    attempted_rate: u32,
) -> Result<(), String> {
    let previous_rate = previous_rate
        .filter(|rate| *rate != 0)
        .ok_or("pre-write source rate was not proven; preserving current state")?;
    let device = claimed.handle.device();
    let config = device
        .active_config_descriptor()
        .map_err(|error| error.to_string())?;
    if config.number() != candidate.configuration_number {
        return Err("live configuration changed; preserving current state".into());
    }
    let live_choice = || {
        strict_implicit_clock(
            &config,
            &claimed.handle,
            choice.control_interface,
            candidate.terminal_link,
        )
    };
    if live_choice().as_ref() != Some(choice) {
        return Err("live clock choice changed; preserving current state".into());
    }
    let live_rate = get_wire_frequency(&claimed.handle, clock, Some(choice))?;
    if live_choice().as_ref() != Some(choice) {
        return Err("live clock choice changed during rate proof; preserving current state".into());
    }
    if live_rate == previous_rate {
        return Ok(());
    }
    if live_rate != attempted_rate {
        return Err(format!(
            "live source rate changed externally to {live_rate} Hz; preserving current state"
        ));
    }
    let restored = apply_wire_frequency(
        &claimed.handle,
        clock,
        Some(choice),
        previous_rate,
        settings().settle_delay_ms(),
    );
    if !restored.clock_ok
        || !restored.rate_verified
        || restored.reported_sample_rate != Some(previous_rate)
    {
        return Err(restored.message.unwrap_or_else(|| {
            format!("could not verify restored source rate {previous_rate} Hz")
        }));
    }
    Ok(())
}

pub(super) fn choose_adaptive_sample_rate(
    requested_sample_rate: u32,
    current_rate: Option<u32>,
    supported_ranges: Option<&[SamplingFrequencySubrange]>,
    capability_model: Option<&CapabilityModel>,
    device: &Device<Context>,
    handle: &DeviceHandle<Context>,
    speed: Speed,
    requested_format: PlaybackFormat,
    claimed_interfaces: &[u8],
) -> Result<Option<u32>, crate::Error> {
    if requested_format.dsd_transport == DsdTransportMode::Native {
        return Ok(current_rate.filter(|rate| *rate == requested_sample_rate));
    }
    let has_rate_evidence = current_rate.is_some()
        || supported_ranges.is_some_and(|ranges| !ranges.is_empty())
        || capability_model
            .is_some_and(|capabilities| !capabilities.supported_sample_rates.is_empty());

    if !has_rate_evidence {
        return Ok(None);
    }

    let mut candidates = Vec::new();
    if let Some(current_rate) = current_rate {
        candidates.push(current_rate);
    }
    if let Some(ranges) = supported_ranges {
        candidates.extend(candidate_rates_from_supported_ranges(
            ranges,
            requested_sample_rate,
        ));
    }
    if let Some(capability_model) = capability_model {
        candidates.extend(capability_model.supported_sample_rates.iter().copied());
    }
    candidates.sort_unstable();
    candidates.dedup();

    let mut ordered_candidates = Vec::new();
    if let Some(current_rate) = current_rate {
        ordered_candidates.push(current_rate);
    }
    ordered_candidates.extend(preferred_adaptive_sample_rates(requested_sample_rate));
    if let Some(ranges) = supported_ranges {
        ordered_candidates.extend(candidate_rates_from_supported_ranges(
            ranges,
            requested_sample_rate,
        ));
    }
    ordered_candidates.extend(candidates);
    ordered_candidates.sort_unstable();
    ordered_candidates.dedup();

    for candidate_rate in ordered_candidates {
        let candidate_format = PlaybackFormat {
            sample_rate: candidate_rate,
            ..requested_format
        };
        if select_stream_candidate(device, handle, candidate_format, speed, claimed_interfaces)
            .is_ok()
        {
            return Ok(Some(candidate_rate));
        }
    }

    Ok(None)
}

pub(super) fn preferred_adaptive_sample_rates(requested_sample_rate: u32) -> Vec<u32> {
    let mut preferred = Vec::new();
    for rate in [
        requested_sample_rate,
        48_000,
        96_000,
        192_000,
        384_000,
        44_100,
        88_200,
        176_400,
        352_800,
    ] {
        if !preferred.contains(&rate) {
            preferred.push(rate);
        }
    }
    preferred
}

pub(super) fn candidate_rates_from_supported_ranges(
    ranges: &[SamplingFrequencySubrange],
    requested_sample_rate: u32,
) -> Vec<u32> {
    let mut candidates = Vec::new();
    let preferred = preferred_adaptive_sample_rates(requested_sample_rate);

    for range in ranges {
        if range.res == 0 || range.min == range.max {
            candidates.push(range.min);
            continue;
        }

        for rate in preferred.iter().copied() {
            if sampling_frequency_ranges_support_rate(std::slice::from_ref(range), rate) {
                candidates.push(rate);
            }
        }

        candidates.push(range.min);
        if range.max != range.min {
            candidates.push(range.max);
        }
    }

    candidates.sort_unstable();
    candidates.dedup();
    candidates
}

pub(super) fn sanitize_playback_format(playback_format: PlaybackFormat) -> PlaybackFormat {
    let bit_depth = if playback_format.is_dop {
        match playback_format.bit_depth {
            24 | 32 => playback_format.bit_depth,
            _ => 24,
        }
    } else if playback_format.dsd_transport == DsdTransportMode::Native {
        playback_format.bit_depth
    } else if playback_format.pcm_encoding == PcmEncoding::Unsigned8 {
        8
    } else if playback_format.pcm_encoding == PcmEncoding::Float32 {
        32
    } else {
        match playback_format.bit_depth {
            16 | 24 | 32 => playback_format.bit_depth,
            _ => 16,
        }
    };
    PlaybackFormat {
        pcm_encoding: playback_format.pcm_encoding,
        sample_rate: playback_format.sample_rate.max(8_000),
        bit_depth,
        channels: playback_format.channels.max(1),
        is_dop: playback_format.is_dop,
        dsd_transport: playback_format.dsd_transport,
        dsd_bit_rate: playback_format.dsd_bit_rate,
    }
}

pub(super) fn transport_compatibility_penalty(
    stream_format: &StreamingInterfaceFormat,
    playback_format: PlaybackFormat,
) -> u32 {
    let is_native_dsd = playback_format.dsd_transport == DsdTransportMode::Native;
    let fmt_ok = if is_native_dsd {
        stream_format.native_dsd.is_some()
    } else {
        stream_format.format_tag == playback_format.pcm_encoding.format_tag()
            || stream_format.uac2_formats.is_some_and(|bits| {
                bits & (1 << (playback_format.pcm_encoding.format_tag() - 1)) != 0
            })
    };
    if !fmt_ok
        || (!is_native_dsd
            && ambiguous_type_i_pcm(
                stream_format.uac2_formats,
                stream_format.subslot_size,
                stream_format.bit_resolution,
            ))
    {
        return u32::MAX;
    }
    if stream_format.subslot_size == 0 || stream_format.channels == 0 {
        return u32::MAX;
    }
    if stream_format.channels != playback_format.channels {
        return u32::MAX;
    }
    if !is_native_dsd {
        if (playback_format.pcm_encoding == PcmEncoding::Unsigned8
            && stream_format.subslot_size != 1)
            || (playback_format.pcm_encoding == PcmEncoding::Float32
                && stream_format.subslot_size != 4)
        {
            return u32::MAX;
        }
        let Some(min_subslot_size) = bytes_per_sample(playback_format.bit_depth) else {
            return u32::MAX;
        };
        if stream_format.bit_resolution != playback_format.bit_depth {
            return u32::MAX;
        }
        if stream_format.subslot_size < min_subslot_size as u8 {
            return u32::MAX;
        }

        let container_bits = u32::from(stream_format.subslot_size) * 8;
        if u32::from(stream_format.bit_resolution) > container_bits {
            return u32::MAX;
        }

        u32::from(stream_format.subslot_size) - min_subslot_size as u32
    } else {
        let container_bits = u32::from(stream_format.subslot_size) * 8;
        if u32::from(stream_format.bit_resolution) > container_bits {
            return u32::MAX;
        }
        0
    }
}

pub(super) fn transport_compatibility_penalty_for_candidate(
    candidate: &IsoStreamCandidate,
    playback_format: PlaybackFormat,
) -> u32 {
    transport_compatibility_penalty(
        &StreamingInterfaceFormat {
            format_tag: candidate.format_tag,
            uac2_formats: candidate.uac2_formats,
            native_dsd: candidate.native_dsd,
            channels: candidate.channels,
            subslot_size: candidate.subslot_size,
            bit_resolution: candidate.bit_resolution,
            terminal_link: candidate.terminal_link,
            is_uac1: candidate.is_uac1,
            sample_rate_ranges: vec![],
        },
        playback_format,
    )
}

#[cfg(test)]
mod evidence_tests {
    use super::{adaptive_rate_verified, negotiation_reported_rate};

    #[test]
    fn attempted_set_never_reports_a_stale_or_synthetic_rate() {
        assert_eq!(negotiation_reported_rate(true, Some(48_000), None), None);
        assert_eq!(negotiation_reported_rate(true, Some(48_000), Some(0)), None);
        assert_eq!(
            negotiation_reported_rate(true, Some(48_000), Some(44_100)),
            Some(44_100)
        );
        assert_eq!(
            negotiation_reported_rate(false, Some(48_000), None),
            Some(48_000)
        );
        assert_eq!(negotiation_reported_rate(false, None, None), None);
        assert!(!adaptive_rate_verified(
            true,
            Some(48_000),
            None,
            false,
            48_000
        ));
        assert!(!adaptive_rate_verified(
            true,
            Some(48_000),
            Some(48_000),
            false,
            48_000
        ));
        assert!(adaptive_rate_verified(
            true,
            Some(48_000),
            Some(44_100),
            true,
            44_100
        ));
        assert!(adaptive_rate_verified(
            false,
            Some(48_000),
            None,
            false,
            48_000
        ));
    }
}
