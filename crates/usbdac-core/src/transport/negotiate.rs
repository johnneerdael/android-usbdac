use super::*;

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
        let guard = SESSION_STATE.lock();
        let Some(state) = guard.as_ref() else {
            return Ok((requested_format, None));
        };
        state.device.clone()
    };

    let claimed_handle = open_claimed_usb_handle(&device).map_err(Error::Claim)?;
    let lock_requested = current_lock_requested_for_fd(device.fd);
    let mut cleanup_interface: Option<u8> = None;
    let mut clock_choice: Option<ClockChoice> = None;
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

        let candidate = match select_owned_stream_candidate(&claimed_handle, requested_format, speed) {
            Ok(c) => c,
            Err(error) => {
                set_last_error(Some(error.message().to_string()));
                set_direct_mode_refusal_reason(Some(error.message().to_string()));
                return Err(error.into_error(offered_alt_settings()));
            }
        };

        // Clock controls and packet pacing use transport words, not source bytes.
        let requested_format = PlaybackFormat {
            sample_rate: native_wire_rate(requested_format, candidate.subslot_size)
                .ok_or_else(|| Error::Unsupported {
                    reason: "native DSD rate is not a whole wire word cadence".into(),
                    offered: offered_alt_settings(),
                })?,
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

        if adc3_route_for_candidate(&usb_device, &claimed_handle.handle, &candidate).map_err(Error::Transfer)?.is_some() {
            let route = negotiate_adc3_clock(&claimed_handle, &candidate, requested_format.sample_rate)
                .map_err(Error::Transfer)?;
            remember_adc3_route(route);
            set_effective_playback_format(requested_format);
            set_dac_mode(DacMode::FixedClock);
            set_clock_verification(true, true, true, Some(requested_format.sample_rate));
            return Ok(requested_format);
        }
        select_requested_clock_route(&claimed_handle, &candidate, requested_format.sample_rate)
            .map_err(Error::Transfer)?;
        let clock = candidate_audio_control_clock_choice(&usb_device, &claimed_handle.handle, &candidate)
            .map(|(clock, choice)| { clock_choice = Some(choice); clock });

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
            let current_rate = get_sampling_frequency(
                &claimed_handle.handle,
                clock.interface_number,
                clock.clock_id,
            )
            .ok();
            let supported_ranges = get_sampling_frequency_ranges(
                &claimed_handle.handle,
                clock.interface_number,
                clock.clock_id,
            )
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
                let clock_outcome = apply_sampling_frequency(
                    &claimed_handle.handle,
                    clock.interface_number,
                    clock.clock_id,
                    requested_format.sample_rate,
                    settings().settle_delay_ms(),
                );
                clock_attempted = true;
                clock_succeeded = clock_outcome.clock_ok;
                rate_verified = clock_outcome.rate_verified;
                reported_rate = clock_outcome.reported_sample_rate.or(current_rate);
                last_message = clock_outcome.message.clone();
                log::info!(
                    "[USB] NEGOTIATION CLOCK: confirmed={}Hz, clockOk={}, rateVerified={}, clockId={}, iface={}",
                    clock_outcome.reported_sample_rate.unwrap_or(0),
                    clock_outcome.clock_ok,
                    clock_outcome.rate_verified,
                    clock.clock_id,
                    clock.interface_number,
                );
                if clock_outcome.rate_verified
                    && clock_outcome.reported_sample_rate == Some(requested_format.sample_rate)
                {
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

            if requested_format.dsd_transport == DsdTransportMode::Native {
                return Err(Error::Unsupported {
                    reason: "native DSD requires its exact wire clock rate".into(),
                    offered: offered_alt_settings(),
                });
            }
            if let Some(actual_rate) = choose_adaptive_sample_rate(
                requested_format.sample_rate,
                current_rate,
                supported_ranges.as_deref(),
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
                reported_rate = reported_rate.or(current_rate).or(Some(actual_rate));
                rate_verified =
                    current_rate == Some(actual_rate) || reported_rate == Some(actual_rate);
            } else {
                let message = last_message.unwrap_or_else(|| {
                    format!(
                        "Unable to verify DAC rate for requested {} Hz during negotiation; continuing with direct USB at the requested stream rate",
                        requested_format.sample_rate
                    )
                });
                reported_rate = reported_rate
                    .or(current_rate)
                    .or(Some(requested_format.sample_rate));
                last_message = Some(message);
            }
        } else if candidate.is_uac1 {
            match set_uac1_sampling_frequency(
                &claimed_handle.handle,
                candidate.interface_number,
                candidate.endpoint_address,
                requested_format.sample_rate,
            ) {
                Ok(()) => {
                    dev_eprintln!(
                        "[USB] UAC1 SET_CUR: endpoint=0x{:02x} rate={}Hz",
                        candidate.endpoint_address,
                        requested_format.sample_rate
                    );
                    dac_mode = DacMode::FixedClock;
                    reported_rate = Some(requested_format.sample_rate);
                    last_message = Some(format!(
                        "UAC 1.0 device: set sampling frequency to {} Hz on endpoint 0x{:02x}",
                        requested_format.sample_rate, candidate.endpoint_address
                    ));
                }
                Err(e) => {
                    dev_eprintln!(
                        "[USB] UAC1 SET_CUR failed for endpoint=0x{:02x}: {}",
                        candidate.endpoint_address,
                        e
                    );
                    // Defer the verdict: many UAC1 DACs (FiiO BR13) STALL every
                    // SET_CUR encoding while the streaming alt setting is
                    // inactive (alt 0). The stream-open path re-issues SET_CUR
                    // after the alt setting is live and refuses there if it
                    // still fails — bailing here would abort before the only
                    // attempt that can succeed.
                    dac_mode = DacMode::AdaptiveStreaming;
                    reported_rate = Some(requested_format.sample_rate);
                    last_message = Some(format!(
                        "UAC 1.0 device: rate set on endpoint 0x{:02x} deferred ({}); retrying after alt setting is active",
                        candidate.endpoint_address, e
                    ));
                }
            }
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
            reported_rate = Some(requested_format.sample_rate);
            last_message = Some(message);
        }

        // A changed effective rate must satisfy the same pairing and capacity
        // proof. In particular an implicit clock cannot silently degrade to the
        // playback path's unverified fixed-clock fallback.
        if candidate.implicit_feedback.is_some() {
            if !rate_verified
                || clock_choice.as_ref().is_none_or(|choice| {
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
        reported_rate = reported_rate.or(Some(effective_format.sample_rate));
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
    if !cleanup_ok || negotiated.is_err() { clear_adc3_negotiation(); }
    if !cleanup_ok && negotiated.is_ok() {
        return Err(Error::Transfer("USB negotiation could not restore a PCM-safe idle state".into()));
    }
    negotiated.map(|format| (format, clock_choice))
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
    } else {
        match playback_format.bit_depth {
            16 | 24 | 32 => playback_format.bit_depth,
            _ => 16,
        }
    };
    PlaybackFormat {
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
        stream_format.format_tag == FORMAT_TAG_PCM
    };
    if !fmt_ok {
        return u32::MAX;
    }
    if stream_format.subslot_size == 0 || stream_format.channels == 0 {
        return u32::MAX;
    }
    if stream_format.channels != playback_format.channels {
        return u32::MAX;
    }
    if !is_native_dsd {
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
