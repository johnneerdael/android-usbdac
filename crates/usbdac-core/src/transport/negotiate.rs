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

// Private per-batch certificate. Only a retained, identical idle owner may
// consume a clean refusal, and its configuration must remain the same.
pub(crate) struct RetryBaseline {
    owner: usize,
    configuration: Option<u8>,
    clean: bool,
}
impl RetryBaseline {
    pub(crate) fn new(owner: usize) -> Self {
        Self {
            owner,
            configuration: None,
            clean: false,
        }
    }
    pub(crate) fn take_clean(&mut self) -> bool {
        std::mem::take(&mut self.clean)
    }
}

fn idle_snapshot(handle: &DeviceHandle<Context>, claimed: &[u8]) -> Result<(u8, Vec<u8>), String> {
    let config = handle
        .device()
        .active_config_descriptor()
        .map_err(|e| e.to_string())?;
    if handle.active_configuration().map_err(|e| e.to_string())? != config.number() {
        return Err("active configuration changed".into());
    }
    let mut interfaces = Vec::new();
    for interface in config.interfaces() {
        if interface
            .descriptors()
            .any(|d| d.class_code() == 1 && d.sub_class_code() == 2)
        {
            if !claimed.contains(&interface.number()) {
                return Err("unowned streaming interface".into());
            }
            let mut current = [0u8; 1];
            let len = handle
                .read_control(
                    0x81,
                    0x0a,
                    0,
                    u16::from(interface.number()),
                    &mut current,
                    Duration::from_millis(500),
                )
                .map_err(|e| e.to_string())?;
            if len != 1 || current[0] != 0 {
                return Err("streaming interface is not proven idle".into());
            }
            interfaces.push(interface.number());
        }
    }
    Ok((config.number(), interfaces))
}

// A clean UAC1 exclusion requires complete standard Type-I descriptors. The
// general transport parser tolerates truncated rate lists, so it cannot alone
// certify a negative for a later hardware attempt.
fn uac1_type_i_excludes(extra: &[u8], request: PlaybackFormat) -> Option<bool> {
    let mut general = None;
    let mut type_i = None;
    let mut at = 0;
    while at < extra.len() {
        if extra.len() - at < 2 {
            return None;
        }
        let len = usize::from(extra[at]);
        if len < 2 || len > extra.len() - at {
            return None;
        }
        let d = &extra[at..at + len];
        if d[1] == USB_DT_CS_INTERFACE {
            if len < 3 {
                return None;
            }
            match d[2] {
                1 => {
                    if general.replace(d).is_some() {
                        return None;
                    }
                }
                2 => {
                    if type_i.replace(d).is_some() {
                        return None;
                    }
                }
                _ => {}
            }
        }
        at += len;
    }
    let general = general?;
    let type_i = type_i?;
    if general.len() != 7
        || general[3] == 0
        || type_i.len() < 8
        || type_i[3] != 1
        || type_i[4] == 0
        || !(1..=4).contains(&type_i[5])
        || type_i[6] == 0
        || u16::from(type_i[6]) > u16::from(type_i[5]) * 8
    {
        return None;
    }
    let tag = u16::from_le_bytes([general[5], general[6]]);
    // Unknown/private format tags are never evidence of incompatibility.
    if !matches!(
        tag,
        FORMAT_TAG_PCM | FORMAT_TAG_PCM8 | FORMAT_TAG_IEEE_FLOAT
    ) || (tag == FORMAT_TAG_PCM8 && (type_i[5] != 1 || type_i[6] != 8))
        || (tag == FORMAT_TAG_IEEE_FLOAT && (type_i[5] != 4 || type_i[6] != 32))
    {
        return None;
    }
    let rate = |b: &[u8]| u32::from_le_bytes([b[0], b[1], b[2], 0]);
    let count = usize::from(type_i[7]);
    let rate_supported = if count == 0 {
        if type_i.len() != 14 {
            return None;
        }
        let min = rate(&type_i[8..11]);
        let max = rate(&type_i[11..14]);
        if min == 0 || min > max {
            return None;
        }
        (min..=max).contains(&request.sample_rate)
    } else {
        if type_i.len() != 8 + count * 3 {
            return None;
        }
        let mut supported = false;
        for bytes in type_i[8..].chunks_exact(3) {
            let value = rate(bytes);
            if value == 0 {
                return None;
            }
            supported |= value == request.sample_rate;
        }
        supported
    };
    Some(
        tag != request.pcm_encoding.format_tag()
            || u16::from(type_i[4]) != request.channels
            || type_i[6] != request.bit_depth
            || !rate_supported,
    )
}

// A successful, read-only RANGE exclusion on a single direct UAC2 path.
// Selectors and multipliers remain terminal because another reachable path
// may admit the requested rate, or the route could change while probing.
struct LiveRateExclusion;
fn direct_uac2_rate_exclusion(
    claimed: &ClaimedHandle,
    interface: u8,
    terminal: Option<u8>,
    rate: u32,
) -> Option<LiveRateExclusion> {
    let device = claimed.handle.device();
    let (control, graph) = owning_clock_graph(&device, &claimed.handle, interface)?;
    if !claimed.claimed_interfaces.contains(&control) {
        return None;
    }
    let terminal = terminal?;
    let plans = graph
        .enumerate_plans(terminal, |id| {
            get_clock_selector_pin(&claimed.handle, control, id)
        })
        .ok()?;
    if plans.len() != 1
        || plans[0].path.len() != 1
        || !plans[0].selectors.is_empty()
        || !matches!(plans[0].source_controls & 3, 1 | 3)
    {
        return None;
    }
    let config = device.active_config_descriptor().ok()?;
    let raw = read_active_configuration_bytes_checked(&claimed.handle, &config).ok()?;
    let mut offset = 0;
    while offset < raw.len() {
        if raw.len() - offset < 2 {
            return None;
        }
        let len = usize::from(raw[offset]);
        if len < 2 || len > raw.len() - offset {
            return None;
        }
        offset += len;
    }
    let ac = audio_control_descriptors_from_config(&raw, control);
    if !complete_uac2_direct_clock(&ac, terminal, plans[0].source_id) {
        return None;
    }
    let choice = graph.choice_for_plan(control, terminal, &plans[0]);
    if choice.resolution != crate::clock_resolution::ClockResolution::Resolved {
        return None;
    }
    let ranges = get_sampling_frequency_ranges(&claimed.handle, control, choice.source_id).ok()?;
    if !complete_live_range_excludes(&ranges, rate) {
        return None;
    }
    Some(LiveRateExclusion)
}

// Only complete, unambiguous raw RANGE replies can certify absence.
fn complete_live_range_excludes(ranges: &[SamplingFrequencySubrange], rate: u32) -> bool {
    !ranges.is_empty()
        && !ranges.iter().any(|range| {
            range.min == 0 || range.max < range.min || (range.max > range.min && range.res == 0)
        })
        && !sampling_frequency_ranges_support_rate(ranges, rate)
}

fn complete_uac2_direct_clock(bytes: &[u8], terminal: u8, source: u8) -> bool {
    let (mut at, mut terminal_seen, mut source_seen) = (0, false, false);
    while at < bytes.len() {
        if bytes.len() - at < 3 {
            return false;
        }
        let len = usize::from(bytes[at]);
        if len < 3 || len > bytes.len() - at {
            return false;
        }
        let d = &bytes[at..at + len];
        if d[1] == USB_DT_CS_INTERFACE && len >= 4 {
            if d[3] == terminal && matches!(d[2], 2 | 3) {
                if terminal_seen || !matches!((d[2], len), (2, 17) | (3, 12)) {
                    return false;
                }
                let link = if d[2] == 2 { d[7] } else { d[8] };
                if link != source {
                    return false;
                }
                terminal_seen = true;
            }
            if d[3] == source && d[2] == 10 {
                if source_seen || len != 8 {
                    return false;
                }
                source_seen = true;
            }
        }
        at += len;
    }
    terminal_seen && source_seen
}

// The general parser accepts private and truncated descriptors. Do not use it
// as negative evidence unless the complete standard Type I pair is present.
fn complete_uac2_type_i(extra: &[u8]) -> bool {
    let mut at = 0;
    let (mut general, mut format) = (false, false);
    while at < extra.len() {
        if extra.len() - at < 2 {
            return false;
        }
        let len = usize::from(extra[at]);
        if len < 2 || len > extra.len() - at {
            return false;
        }
        let d = &extra[at..at + len];
        if d[1] == USB_DT_CS_INTERFACE {
            if len < 3 {
                return false;
            }
            match d[2] {
                1 => {
                    if general || len != 16 || d[3] == 0 || d[5] != 1 || d[10] == 0 {
                        return false;
                    }
                    let bits = u32::from_le_bytes([d[6], d[7], d[8], d[9]]);
                    if bits == 0 || bits & !0x7 != 0 {
                        return false;
                    }
                    general = true;
                }
                2 => {
                    if format
                        || len != 6
                        || d[3] != 1
                        || !(1..=4).contains(&d[4])
                        || d[5] == 0
                        || d[5] > d[4] * 8
                    {
                        return false;
                    }
                    format = true;
                }
                _ => return false,
            }
        }
        at += len;
    }
    general && format
}

// Only immutable, fully described output incompatibility mints a clean refusal;
// generic SelectError::NoMatch can also represent failed I/O or power changes.
fn descriptor_shape_absent(claimed: &ClaimedHandle, format: PlaybackFormat, speed: Speed) -> bool {
    if format.is_dop || format.dsd_transport != DsdTransportMode::None {
        return false;
    }
    let Ok(config) = claimed.handle.device().active_config_descriptor() else {
        return false;
    };
    if !matches!(
        vendor_pcm::discover(&claimed.handle, &config, speed),
        Ok(None)
    ) {
        return false;
    }
    let mut seen = false;
    for alt in config
        .interfaces()
        .flat_map(|i| i.descriptors())
        .filter(|d| {
            d.setting_number() != 0
                && d.endpoint_descriptors()
                    .any(|ep| ep.direction() == Direction::Out)
        })
    {
        seen = true;
        if alt.class_code() != 1 || alt.sub_class_code() != 2 {
            return false;
        }
        match alt.protocol_code() {
            0 => {
                if uac1_type_i_excludes(alt.extra(), format) != Some(true) {
                    return false;
                }
            }
            0x20 if format.pcm_encoding == PcmEncoding::Signed => {
                if !complete_uac2_type_i(alt.extra()) {
                    return false;
                }
                let Some(parsed) = scan::parse_android_streaming_interface_format(&alt) else {
                    return false;
                };
                if parsed.channels == format.channels
                    && parsed.bit_resolution == format.bit_depth
                    && parsed.uac2_formats.is_some_and(|bits| bits & 1 != 0)
                    && direct_uac2_rate_exclusion(
                        claimed,
                        alt.interface_number(),
                        parsed.terminal_link,
                        format.sample_rate,
                    )
                    .is_none()
                {
                    return false;
                }
            }
            0x30 if format.pcm_encoding == PcmEncoding::Signed => {
                if !adc3::adc3_direct_rate_excludes(claimed, &config, &alt, speed, format) {
                    return false;
                }
            }
            _ => return false,
        }
    }
    seen
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
    rate_policy: crate::RatePolicy,
    mut retry: Option<&mut RetryBaseline>,
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
    let mut snapshot = None;
    let mut clean_descriptor = false;
    let mut clean_clock = None;
    if let Some(batch) = retry.as_deref_mut() {
        batch.clean = false;
    }
    let negotiated = (|| {
        // SessionGuard excludes transfers; restore the preceding negotiation's
        // selector state before discovering this request's candidates.
        restore_requested_clock_route(&claimed_handle).map_err(Error::Transfer)?;
        if let Some(batch) = retry.as_deref_mut() {
            if claimed_handle.handle.as_raw() as usize == batch.owner && lock_requested {
                if let Ok(value) =
                    idle_snapshot(&claimed_handle.handle, &claimed_handle.claimed_interfaces)
                {
                    if batch.configuration.is_none() || batch.configuration == Some(value.0) {
                        batch.configuration = Some(value.0);
                        snapshot = Some(value);
                    }
                }
            }
        }
        let usb_device = claimed_handle.handle.device();
        let speed = resolve_device_speed(&usb_device);
        if snapshot.is_some()
            && rate_policy == crate::RatePolicy::Exact
            && descriptor_shape_absent(&claimed_handle, requested_format, speed)
        {
            clean_descriptor = true;
            return Err(Error::Unsupported {
                reason: "no standard output descriptor carries the requested PCM format".into(),
                offered: offered_alt_settings(),
            });
        }
        let capability_model = build_usb_capability_model(
            &usb_device,
            &claimed_handle.handle,
            speed,
            &claimed_handle.claimed_interfaces,
        )
        .ok();
        set_capability_model(capability_model.clone());

        let (candidate, requested_format) =
            match select_owned_stream_candidate(&claimed_handle, requested_format, speed) {
                Ok(candidate) => (candidate, requested_format),
                Err(first) => {
                    if !matches!(first, SelectError::NoMatch(_))
                        || rate_policy != crate::RatePolicy::NearestSupported
                        || requested_format.dsd_transport == DsdTransportMode::Native
                        || requested_format.is_dop
                    {
                        set_last_error(Some(first.message().to_string()));
                        set_direct_mode_refusal_reason(Some(first.message().to_string()));
                        return Err(first.into_error(offered_alt_settings()));
                    }
                    let mut rates = capability_model
                        .as_ref()
                        .map(|model| rates_for_format(model, requested_format))
                        .unwrap_or_default();
                    rank_sample_rates(&mut rates, requested_format.sample_rate);
                    let mut replacement = None;
                    for rate in rates {
                        let target = PlaybackFormat {
                            sample_rate: rate,
                            ..requested_format
                        };
                        match select_owned_stream_candidate(&claimed_handle, target, speed) {
                            Ok(candidate) => {
                                replacement = Some((candidate, target));
                                break;
                            }
                            Err(SelectError::NoMatch(_)) => continue,
                            Err(error) => return Err(error.into_error(offered_alt_settings())),
                        }
                    }
                    replacement.ok_or_else(|| first.into_error(offered_alt_settings()))?
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

        let original_clock = if snapshot.is_some()
            && !candidate.is_uac1
            && candidate.implicit_feedback.is_none()
            && !requested_format.is_dop
            && requested_format.dsd_transport == DsdTransportMode::None
            && !settings().has(crate::Quirk::SetInterfaceFirst)
        {
            candidate_audio_control_clock_choice(&usb_device, &claimed_handle.handle, &candidate)
                .ok()
                .flatten()
                .and_then(|(clock, choice)| {
                    if uac2_choice_has_multiplier(Some(&choice))
                        || choice.path.iter().any(|hop| {
                            matches!(
                                hop.kind,
                                crate::clock_resolution::ClockHopKind::Selector { .. }
                            )
                        })
                    {
                        return None;
                    }
                    let rate =
                        get_wire_frequency(&claimed_handle.handle, clock, Some(&choice)).ok()?;
                    (rate > 0).then_some((clock, choice, rate))
                })
        } else {
            None
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
                    candidate.is_uac1,
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
                                    let eligible = matches!(
                                        error,
                                        super::uac2_live_alternate::LiveAlternateError::Unavailable
                                    ) && clock_outcome.write_attempted
                                        && clock_outcome.set_cur_succeeded
                                        && !clock_outcome.uncertain_write_seen
                                        && !requested_clock_route_active()
                                        && original_clock.as_ref().is_some_and(
                                            |(original, route, rate)| {
                                                original.clock_id == clock.clock_id
                                                    && original.interface_number
                                                        == clock.interface_number
                                                    && route == choice
                                                    && Some(*rate) == current_rate
                                            },
                                        );
                                    if eligible && restore.is_ok() {
                                        if let Some((_, route, rate)) = original_clock.clone() {
                                            clean_clock =
                                                Some((candidate.clone(), clock, route, rate));
                                        }
                                    }
                                    return Err(Error::Transfer(match restore {
                                        Ok(()) => error.to_string(),
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

    let restoration_error = if negotiated.is_err() {
        restore_requested_clock_route(&claimed_handle).err()
    } else {
        None
    };
    let cleanup_ok = cleanup_claimed_handle(claimed_handle, cleanup_interface, lock_requested);
    if !cleanup_ok || negotiated.is_err() {
        clear_adc3_negotiation();
        if let Some(state) = SESSION_STATE.lock().as_mut() {
            state.requested_playback_format = None;
            state.playback_format = None;
            state.clock_status = None;
            state.clock_control_attempted = false;
            state.clock_control_succeeded = false;
            state.clock_verification_passed = false;
            state.negotiated_uac2_alternate = None;
        }
    }
    if negotiated.is_err() && restoration_error.is_none() && cleanup_ok {
        if let (Some(batch), Some(expected)) = (retry, snapshot.as_ref()) {
            let idle = IDLE_LOCK.lock();
            if let Some(retained) = idle
                .as_ref()
                .filter(|idle| idle.handle.as_raw() as usize == batch.owner)
            {
                let unchanged = idle_snapshot(&retained.handle, &retained.claimed_interfaces)
                    .is_ok_and(|actual| &actual == expected);
                let clock_restored =
                    clean_clock
                        .as_ref()
                        .is_some_and(|(candidate, clock, choice, rate)| {
                            let config = retained.handle.device().active_config_descriptor().ok();
                            config
                                .as_ref()
                                .and_then(|config| {
                                    strict_implicit_clock(
                                        config,
                                        &retained.handle,
                                        choice.control_interface,
                                        candidate.terminal_link,
                                    )
                                })
                                .as_ref()
                                == Some(choice)
                                && get_wire_frequency(&retained.handle, *clock, Some(choice)).ok()
                                    == Some(*rate)
                                && uac2_source_controls(
                                    &retained.handle,
                                    clock.interface_number,
                                    clock.clock_id,
                                )
                                .is_some_and(|controls| {
                                    crate::clock_resolution::clock_validity(controls, || {
                                        get_clock_validity(
                                            &retained.handle,
                                            clock.interface_number,
                                            clock.clock_id,
                                        )
                                    })
                                    .is_ok()
                                })
                        });
                batch.clean = unchanged && (clean_descriptor || clock_restored);
            }
        }
    }
    if restoration_error.is_some() || !cleanup_ok {
        return Err(Error::Transfer(format!(
            "USB negotiation could not restore a PCM-safe idle state: original={:?}; route restoration={:?}; cleanup_ok={cleanup_ok}",
            negotiated.as_ref().err(), restoration_error,
        )));
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
        || restored.uncertain_write_seen
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
    rank_sample_rates(&mut ordered_candidates, requested_sample_rate);

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

fn rate_family(rate: u32) -> u8 {
    if rate > 0 && rate % 11025 == 0 {
        1
    } else if rate > 0 && rate % 8000 == 0 {
        2
    } else {
        0
    }
}

#[doc(hidden)]
pub fn rank_sample_rates(rates: &mut Vec<u32>, requested: u32) {
    rates.retain(|rate| *rate > 0);
    rates.sort_unstable_by_key(|&rate| {
        let class = if rate == requested {
            0
        } else if rate < requested && requested % rate == 0 {
            1
        } else if rate < requested
            && rate_family(rate) != 0
            && rate_family(rate) == rate_family(requested)
        {
            2
        } else if rate < requested {
            3
        } else {
            4
        };
        (
            class,
            if rate <= requested {
                u32::MAX - rate
            } else {
                rate
            },
        )
    });
    rates.dedup();
}

pub(super) fn rates_for_format(model: &CapabilityModel, format: PlaybackFormat) -> Vec<u32> {
    use crate::capabilities::AltFormat;
    let expected = match format.pcm_encoding {
        PcmEncoding::Unsigned8 => AltFormat::PcmU8,
        PcmEncoding::Float32 => AltFormat::PcmFloat32,
        _ => AltFormat::Pcm,
    };
    let mut rates = Vec::new();
    for alt in &model.alt_settings {
        if alt.format != expected
            || alt.channels != format.channels
            || alt.valid_bits != format.bit_depth
        {
            continue;
        }
        rates.extend(alt.sample_rates.iter().copied());
        let ranges: Vec<_> = alt
            .rate_ranges
            .iter()
            .map(|r| SamplingFrequencySubrange {
                min: r.min_hz,
                max: r.max_hz,
                res: r.step_hz,
            })
            .collect();
        rates.extend(candidate_rates_from_supported_ranges(
            &ranges,
            format.sample_rate,
        ));
    }
    rates
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
        if range.min > range.max {
            continue;
        }
        if range.res == 0 {
            if range.min == range.max {
                candidates.push(range.min);
            }
            continue;
        }
        if range.min == range.max {
            candidates.push(range.min);
            continue;
        }

        for rate in preferred.iter().copied().chain(
            (1..=requested_sample_rate)
                .take_while(|v| v.saturating_mul(*v) <= requested_sample_rate)
                .filter(|v| requested_sample_rate % *v == 0)
                .flat_map(|v| [v, requested_sample_rate / v]),
        ) {
            if sampling_frequency_ranges_support_rate(std::slice::from_ref(range), rate) {
                candidates.push(rate);
            }
        }

        let family_unit = match rate_family(requested_sample_rate) {
            1 => 11_025,
            2 => 8_000,
            _ => 0,
        };
        if let Some(family_index) = range
            .max
            .min(requested_sample_rate.saturating_sub(1))
            .checked_div(family_unit)
        {
            let mut rate = family_index * family_unit;
            while rate >= range.min && rate > 0 {
                if sampling_frequency_ranges_support_rate(std::slice::from_ref(range), rate) {
                    candidates.push(rate);
                    break;
                }
                rate = rate.saturating_sub(family_unit);
            }
        }
        let steps = requested_sample_rate.saturating_sub(range.min) / range.res;
        for index in [steps, steps.saturating_add(1)] {
            if let Some(rate) = range.min.checked_add(index.saturating_mul(range.res)) {
                if rate <= range.max {
                    candidates.push(rate);
                }
            }
        }
        candidates.push(range.min);
        let last = range.min + ((range.max - range.min) / range.res) * range.res;
        if last != range.min {
            candidates.push(last);
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
    use super::uac1_type_i_excludes;
    use super::{
        adaptive_rate_verified, candidate_rates_from_supported_ranges, negotiation_reported_rate,
        rank_sample_rates, rates_for_format,
    };
    use crate::transport::types::SamplingFrequencySubrange;

    #[test]
    fn uac1_clean_exclusion_requires_complete_standard_rate_and_encoding() {
        use crate::transport::types::{DsdTransportMode, PcmEncoding, PlaybackFormat};
        let descriptor = [
            7, 0x24, 1, 1, 0, 1, 0, 11, 0x24, 2, 1, 2, 2, 16, 1, 0x80, 0xbb, 0,
        ];
        let request = PlaybackFormat {
            pcm_encoding: PcmEncoding::Signed,
            sample_rate: 44_100,
            bit_depth: 16,
            channels: 2,
            is_dop: false,
            dsd_transport: DsdTransportMode::None,
            dsd_bit_rate: 0,
        };
        assert_eq!(uac1_type_i_excludes(&descriptor, request), Some(true));
        assert_eq!(
            uac1_type_i_excludes(
                &descriptor,
                PlaybackFormat {
                    sample_rate: 48_000,
                    ..request
                }
            ),
            Some(false)
        );
        assert_eq!(
            uac1_type_i_excludes(
                &descriptor,
                PlaybackFormat {
                    pcm_encoding: PcmEncoding::Float32,
                    bit_depth: 32,
                    sample_rate: 48_000,
                    ..request
                }
            ),
            Some(true)
        );
        let mut truncated = descriptor.to_vec();
        truncated[14] = 2;
        assert_eq!(uac1_type_i_excludes(&truncated, request), None);
        let mut zero_rate = descriptor;
        zero_rate[15..18].fill(0);
        assert_eq!(uac1_type_i_excludes(&zero_rate, request), None);
        let mut unknown_tag = descriptor;
        unknown_tag[5] = 0xff;
        assert_eq!(uac1_type_i_excludes(&unknown_tag, request), None);
    }

    #[test]
    fn uac2_exclusion_requires_complete_standard_type_i() {
        let descriptor = [
            16, 0x24, 1, 1, 0, 1, 1, 0, 0, 0, 2, 0, 0, 0, 0, 0, 6, 0x24, 2, 1, 2, 16,
        ];
        assert!(super::complete_uac2_type_i(&descriptor));
        assert!(!super::complete_uac2_type_i(&descriptor[..21]));
        let mut private = descriptor;
        private[9] = 0x80;
        assert!(!super::complete_uac2_type_i(&private));
        let mut bad_format = descriptor;
        bad_format[21] = 24;
        assert!(!super::complete_uac2_type_i(&bad_format));
    }

    #[test]
    fn complete_direct_uac2_route_requires_successful_unambiguous_range_exclusion() {
        let stream = [
            16, 0x24, 1, 1, 0, 1, 1, 0, 0, 0, 2, 0, 0, 0, 0, 0, 6, 0x24, 2, 1, 2, 16,
        ];
        let control = [
            17, 0x24, 2, 1, 1, 1, 0, 5, 0, 0, 0, 0, 0, 0, 0, 0, 0, 8, 0x24, 10, 5, 0, 1, 0, 0,
        ];
        let mut range = vec![2, 0];
        for rate in [48_000u32, 96_000] {
            range.extend_from_slice(&rate.to_le_bytes());
            range.extend_from_slice(&rate.to_le_bytes());
            range.extend_from_slice(&0u32.to_le_bytes());
        }
        assert!(super::complete_uac2_type_i(&stream));
        assert!(super::complete_uac2_direct_clock(&control, 1, 5));
        let parsed = super::parse_sampling_frequency_ranges(&range).unwrap();
        assert!(super::complete_live_range_excludes(&parsed, 192_000));
        assert!(!super::complete_live_range_excludes(&parsed, 96_000));
        assert!(super::parse_sampling_frequency_ranges(&range[..range.len() - 1]).is_err());
        assert!(!super::complete_live_range_excludes(&[], 192_000));
        let ambiguous = [SamplingFrequencySubrange {
            min: 48_000,
            max: 96_000,
            res: 0,
        }];
        assert!(!super::complete_live_range_excludes(&ambiguous, 192_000));
        let mut bad_link = control;
        bad_link[7] = 6;
        assert!(!super::complete_uac2_direct_clock(&bad_link, 1, 5));
    }

    #[test]
    fn adaptive_rates_follow_downsampling_order() {
        let mut rates = vec![44_100, 192_000, 48_000, 96_000, 48_000];
        rank_sample_rates(&mut rates, 192_000);
        assert_eq!(rates, vec![192_000, 96_000, 48_000, 44_100]);
        let ranges = [SamplingFrequencySubrange {
            min: 40_000,
            max: 100_000,
            res: 1000,
        }];
        let mut rates = candidate_rates_from_supported_ranges(&ranges, 72_400);
        rank_sample_rates(&mut rates, 72_400);
        assert_eq!(rates[0], 72_000);
        assert!(rates.contains(&73_000));
        assert!(
            rates.iter().position(|&r| r == 73_000).unwrap()
                > rates.iter().position(|&r| r == 48_000).unwrap()
        );
    }

    #[test]
    fn requested_rate_examples_and_continuous_ceiling_rank_correctly() {
        for (source, rates, expected) in [
            (192_000, vec![44_100, 48_000, 88_200, 96_000], 96_000),
            (176_400, vec![44_100, 48_000, 88_200, 96_000], 88_200),
            (44_100, vec![48_000], 48_000),
        ] {
            let mut rates = rates;
            rank_sample_rates(&mut rates, source);
            assert_eq!(rates[0], expected);
        }
        let ranges = [SamplingFrequencySubrange {
            min: 8_000,
            max: 96_000,
            res: 1,
        }];
        let mut rates = candidate_rates_from_supported_ranges(&ranges, 192_000);
        rank_sample_rates(&mut rates, 192_000);
        assert_eq!(rates[0], 96_000);
    }

    #[test]
    fn zero_resolution_is_only_a_single_fixed_rate() {
        let ranges = [
            SamplingFrequencySubrange {
                min: 44_100,
                max: 44_100,
                res: 0,
            },
            SamplingFrequencySubrange {
                min: 48_000,
                max: 96_000,
                res: 0,
            },
        ];
        assert_eq!(
            candidate_rates_from_supported_ranges(&ranges, 88_200),
            vec![44_100]
        );
    }

    #[test]
    fn stepped_range_includes_aligned_ceiling_and_highest_family_rate() {
        let ranges = [SamplingFrequencySubrange {
            min: 8_000,
            max: 95_777,
            res: 1000,
        }];
        let mut rates = candidate_rates_from_supported_ranges(&ranges, 192_000);
        rank_sample_rates(&mut rates, 192_000);
        assert_eq!(rates[0], 64_000); // highest exact divisor beats non-divisor 95 kHz
        assert!(rates.contains(&95_000));
        assert!(rates.contains(&88_000)); // highest in-range 8 kHz-family cadence
    }

    #[test]
    fn production_candidate_planner_filters_alternates_and_ranks_their_ranges() {
        use crate::capabilities::{
            AltFormat, AltSetting, EndpointUsage, Pacing, RateRange, SyncKind, UacVersion,
        };
        use crate::transport::types::{
            CapabilityModel, DsdTransportMode, PcmEncoding, PlaybackFormat,
        };
        let alt = AltSetting {
            interface_number: 1,
            alt_setting: 1,
            endpoint_address: 1,
            format: AltFormat::Pcm,
            channels: 2,
            subslot_bytes: 2,
            valid_bits: 16,
            rate_ranges: vec![RateRange {
                min_hz: 8_000,
                max_hz: 96_000,
                step_hz: 1,
            }],
            sample_rates: vec![44_100, 48_000, 96_000],
            sync_type: SyncKind::Adaptive,
            feedback_endpoint: None,
            max_packet_bytes: 1024,
            service_interval_us: 125,
            uac_version: UacVersion::V2,
            endpoint_interval: 1,
            usage: EndpointUsage::Data,
            refresh: 0,
            synch_address: 0,
            pacing: Some(Pacing::Adaptive),
        };
        let mut wrong_channels = alt.clone();
        wrong_channels.channels = 1;
        wrong_channels.sample_rates = vec![192_000];
        wrong_channels.rate_ranges.clear();
        let model = CapabilityModel {
            alt_settings: vec![alt, wrong_channels],
            ..Default::default()
        };
        let format = PlaybackFormat {
            pcm_encoding: PcmEncoding::Signed,
            sample_rate: 192_000,
            bit_depth: 16,
            channels: 2,
            is_dop: false,
            dsd_transport: DsdTransportMode::None,
            dsd_bit_rate: 0,
        };
        let mut candidates = rates_for_format(&model, format);
        rank_sample_rates(&mut candidates, format.sample_rate);
        assert_eq!(candidates[0], 96_000);
        assert!(!candidates.contains(&192_000));
    }

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
