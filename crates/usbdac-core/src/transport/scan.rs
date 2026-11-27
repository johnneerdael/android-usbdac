use super::*;
use crate::capabilities::{
    AltFormat, AltSetting, EndpointUsage, LinkSpeed, RateRange, SyncKind, UacVersion,
};

fn type_i_pcm_tag(tag: u16) -> bool {
    matches!(tag, FORMAT_TAG_PCM | FORMAT_TAG_PCM8 | FORMAT_TAG_IEEE_FLOAT)
}

/// Descriptor possibilities do not select a live numeric decoder. Reject equal-geometry
/// signed32/float32 ambiguity until a protocol-backed active-format control is implemented.
pub(super) fn ambiguous_type_i_pcm(bits: Option<u32>, slot: u8, depth: u8) -> bool {
    let Some(bits) = bits else { return false; };
    let signed = bits & 1 != 0 && matches!(depth, 16 | 24 | 32) && slot >= depth / 8;
    let unsigned = bits & 2 != 0 && slot == 1 && depth == 8;
    let float = bits & 4 != 0 && slot == 4 && depth == 32;
    u8::from(signed) + u8::from(unsigned) + u8::from(float) > 1
}

/// Read-only preflight: base AudioControl/playback claims are already held.
/// No alternate is activated and no capture transfer is allocated or submitted.
pub(super) fn preflight_implicit_feedback_interfaces(
    device: &UsbDevice,
) -> Result<(u8, Vec<u8>), String> {
    let claimed = wrap_android_usb_handle(device)?;
    let usb = claimed.handle.device();
    let config = usb.active_config_descriptor().map_err(|e| e.to_string())?;
    validate_android_usb_claims(&claimed, &config)?;
    if vendor_pcm::discover(&claimed.handle, &config, resolve_device_speed(&usb))?.is_some() {
        return Ok((config.number(), Vec::new()));
    }
    Ok((
        config.number(),
        structural_feedback_interfaces(&config, &claimed.handle, resolve_device_speed(&usb), &claimed.claimed_interfaces),
    ))
}

fn structural_feedback_interfaces(
    config: &ConfigDescriptor,
    handle: &DeviceHandle<Context>,
    speed: Speed,
    claimed: &[u8],
) -> Vec<u8> {
    if settings().disable_feedback() {
        return Vec::new();
    }

    let inventory = implicit_source_inventory(config, handle, speed, Some(claimed));
    let mut interfaces = Vec::new();
    for out in config.interfaces().flat_map(|i| i.descriptors()) {
        if out.class_code() != USB_CLASS_AUDIO
            || out.sub_class_code() != USB_SUBCLASS_AUDIOSTREAMING
            || out.setting_number() == 0
        {
            continue;
        }
        let Some(format) = parse_android_streaming_interface_format(&out) else {
            continue;
        };
        if format.is_uac1 || !type_i_pcm_tag(format.format_tag) {
            continue;
        }
        for ep in out.endpoint_descriptors() {
            if ep.direction() != Direction::Out
                || ep.transfer_type() != TransferType::Isochronous
                || ep.sync_type() != SyncType::Asynchronous
                || !(1..=16).contains(&ep.interval())
                || !matches!(
                    ep.usage_type(),
                    UsageType::Data | UsageType::FeedbackData | UsageType::Feedback
                )
                || !valid_standard_implicit_capacity(&ep, speed)
                || !matches!(checked_standard_feedback(&out, ep.address(), ep.synch_address(), ep.sync_type(), speed), Ok(None))
            {
                continue;
            }
            let Ok((capacity, interval)) = standard_endpoint_geometry(&ep, speed) else { continue; };
            let stride = usize::from(format.channels) * usize::from(format.subslot_size);
            if stride == 0 || capacity < stride {
                continue;
            }
            if out.protocol_code() == 0x20 && standard_implicit_pcm_format(&format)
                && format.sample_rate_ranges.is_empty()
            {
                if let Some(clock) = standard_output_clock(config, handle, out.interface_number(), format.terminal_link, claimed) {
                    let ranges = get_wire_frequency_ranges(handle, AudioControlClock {
                        interface_number: clock.control_interface, clock_id: clock.source_id,
                    }, Some(&clock)).unwrap_or_default();
                    if !ranges.is_empty() {
                        interfaces.extend(standard_implicit_routes(&inventory, config, handle, &out, &ep,
                            &format, &clock, &ranges, capacity, interval)
                            .routes.into_iter().map(|(source, _)| source.interface_number));
                        continue;
                    }
                }
                // Live RANGE unavailable: retain the read-only structural claim fallback.
                if let Some(source) = resolve_implicit_source(&inventory, config, handle,
                    out.interface_number(), out.setting_number(), format.terminal_link,
                    ep.synch_address(), interval, None, true) {
                    interfaces.push(source.interface_number);
                }
                continue;
            }
            let mut rates = Vec::new();
            for source in &inventory {
                let ranges = if source.rates.is_empty() {
                    get_wire_frequency_ranges(
                        handle,
                        AudioControlClock { interface_number: source.control_interface, clock_id: source.clock.source_id },
                        Some(&source.clock),
                    )
                    .unwrap_or_default()
                } else {
                    source.rates.clone()
                };
                rates.extend(representative_sample_rates_from_ranges(&ranges));
            }
            rates.sort_unstable();
            rates.dedup();
            if rates.is_empty() {
                if let Some(source) = resolve_implicit_source(
                    &inventory,
                    config,
                    handle,
                    out.interface_number(),
                    out.setting_number(),
                    format.terminal_link,
                    ep.synch_address(),
                    interval,
                    None,
                    out.protocol_code() == 0x20 && standard_implicit_pcm_format(&format),
                ) {
                    interfaces.push(source.interface_number);
                }
            } else {
                for rate in rates {
                    if (!format.sample_rate_ranges.is_empty()
                        && !sampling_frequency_ranges_support_rate(
                            &format.sample_rate_ranges,
                            rate,
                        ))
                        || capacity
                            < required_max_packet_bytes(rate, interval, stride)
                    {
                        continue;
                    }
                    if let Some(source) = resolve_implicit_source(
                        &inventory,
                        config,
                        handle,
                        out.interface_number(),
                        out.setting_number(),
                        format.terminal_link,
                        ep.synch_address(),
                        interval,
                        Some(rate),
                        out.protocol_code() == 0x20 && standard_implicit_pcm_format(&format),
                    ) {
                        interfaces.push(source.interface_number);
                    }
                }
            }
        }
    }
    interfaces.extend(adc3_feedback_interfaces(config, handle, speed, claimed));
    interfaces.sort_unstable();
    interfaces.dedup();
    interfaces
}

/// A standard AS_GENERAL PCM bitmap and valid Type-I geometry are required;
/// the historical private short AS dialect must not acquire fixed endpoint profiles.
fn standard_implicit_pcm_format(format: &StreamingInterfaceFormat) -> bool {
    !format.is_uac1
        && !ambiguous_type_i_pcm(format.uac2_formats, format.subslot_size, format.bit_resolution)
        && type_i_pcm_tag(format.format_tag)
        && format.uac2_formats.is_some_and(|bits| bits & (1 << (format.format_tag - 1)) != 0)
        && match format.format_tag {
            FORMAT_TAG_PCM8 => format.subslot_size == 1 && format.bit_resolution == 8,
            FORMAT_TAG_IEEE_FLOAT => format.subslot_size == 4 && format.bit_resolution == 32,
            _ => true,
        }
        && format.channels > 0
        && format.subslot_size > 0
        && format.bit_resolution > 0
        && format.bit_resolution <= format.subslot_size.saturating_mul(8)
}

struct ImplicitSourceCandidate {
    source: ImplicitFeedbackSource,
    control_interface: u8,
    function_interfaces: Vec<u8>,
    clock: ClockChoice,
    rates: Vec<SamplingFrequencySubrange>,
    marked: bool,
    standard_pcm: bool,
}

/// Audio IADs are authoritative function membership; no adjacency heuristic.
pub(super) fn audio_functions(raw: &[u8]) -> Vec<(u8, Vec<u8>)> {
    let mut result = Vec::new();
    let mut offset = 0;
    while offset + 2 <= raw.len() {
        let len = usize::from(raw[offset]);
        if len < 2 || offset + len > raw.len() {
            break;
        }
        let d = &raw[offset..offset + len];
        if d[1] == 0x0b && len >= 8 && d[4] == USB_CLASS_AUDIO && d[5] == 0 && d[6] == 0x20 {
            let first = d[2];
            let count = d[3];
            if count > 0 && u16::from(first) + u16::from(count) <= 256 {
                result.push((first, (0..count).map(|n| first + n).collect()));
            }
        }
        offset += len;
    }
    result
}

fn implicit_source_inventory(
    config: &ConfigDescriptor,
    handle: &DeviceHandle<Context>,
    speed: Speed,
    claimed_controls: Option<&[u8]>,
) -> Vec<ImplicitSourceCandidate> {
    if settings().has(crate::Quirk::SkipImplicitFeedback) { return Vec::new(); }
    if !matches!(speed, Speed::Full | Speed::High | Speed::Super | Speed::SuperPlus) {
        return Vec::new();
    }
    let Some(raw) = read_active_configuration_bytes(handle, config) else {
        return Vec::new();
    };
    let functions = audio_functions(&raw);
    let mut result = Vec::new();
    for interface in config.interfaces() {
        for alt in interface.descriptors() {
            if alt.class_code() != USB_CLASS_AUDIO
                || alt.sub_class_code() != USB_SUBCLASS_AUDIOSTREAMING
                || alt.setting_number() == 0
            {
                continue;
            }
            let Some(format) = parse_android_streaming_interface_format(&alt) else {
                continue;
            };
            let stride = usize::from(format.channels) * usize::from(format.subslot_size);
            if format.is_uac1
                || format.format_tag != FORMAT_TAG_PCM
                || stride == 0
                || format.bit_resolution == 0
                || format.bit_resolution > format.subslot_size.saturating_mul(8)
            {
                continue;
            }
            let matching: Vec<_> = functions
                .iter()
                .filter(|(_, ids)| ids.contains(&alt.interface_number()))
                .collect();
            let [(control, ids)] = matching.as_slice() else {
                continue;
            };
            if claimed_controls.is_some_and(|claimed| !claimed.contains(control)) { continue; }
            let Some(clock) = strict_implicit_clock(config, handle, *control, format.terminal_link)
            else {
                continue;
            };
            for ep in alt.endpoint_descriptors() {
                if ep.direction() != Direction::In
                    || ep.transfer_type() != TransferType::Isochronous
                    || ep.sync_type() != SyncType::Asynchronous
                    || !(1..=16).contains(&ep.interval())
                    || !valid_standard_implicit_capacity(&ep, speed)
                    || !matches!(ep.usage_type(), UsageType::Data | UsageType::FeedbackData)
                {
                    continue;
                }
                let Ok((capacity, interval)) = standard_endpoint_geometry(&ep, speed) else { continue; };
                if capacity < stride {
                    continue;
                }
                result.push(ImplicitSourceCandidate {
                    source: ImplicitFeedbackSource {
                        interface_number: alt.interface_number(),
                        alt_setting: alt.setting_number(),
                        address: ep.address(),
                        bytes_per_frame: stride,
                        service_interval_us: interval,
                        max_packet_bytes: capacity,
                    },
                    control_interface: *control,
                    function_interfaces: ids.clone(),
                    clock: clock.clone(),
                    rates: format.sample_rate_ranges.clone(),
                    marked: ep.usage_type() == UsageType::FeedbackData,
                    standard_pcm: alt.protocol_code() == 0x20
                        && standard_implicit_pcm_format(&format),
                });
            }
        }
    }
    result
}

/// Internal implicit-source ranking rules. Device-specific fixed/skip rules
/// remain deferred; they never waive standard UAC2/function/clock proof.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PlaybackImplicitProfile {
    Generic,
    #[cfg(test)]
    Fixed { address: u8, interface: u8 },
    Skip,
}

fn playback_implicit_profile(_vendor: u16, _product: u16) -> PlaybackImplicitProfile {
    // Only source-backed exact identity exclusions are active; fixed-pipe
    // vendor mappings still require a separately proven source contract.
    if settings().has(crate::Quirk::SkipImplicitFeedback) { PlaybackImplicitProfile::Skip }
    else { PlaybackImplicitProfile::Generic }
}

/// Apply the identity rule only after a sink's explicit-feedback path has been
/// excluded. Fixed rules cannot fall back to another marked or adjacent pipe.
fn implicit_source_rank(
    profile: PlaybackImplicitProfile,
    source: ImplicitFeedbackSource,
    marked: bool,
    out_interface: u8,
    out_alt: u8,
    linked_address: u8,
) -> Option<u8> {
    match profile {
        PlaybackImplicitProfile::Skip => None,
        #[cfg(test)]
        PlaybackImplicitProfile::Fixed { address, interface } => {
            (source.address == address
                && source.interface_number == interface
                && (linked_address == 0 || linked_address == address))
                .then_some(0)
        }
        PlaybackImplicitProfile::Generic => {
            if linked_address != 0 {
                (source.address == linked_address).then_some(0)
            } else if marked {
                Some(1)
            } else {
                (source.interface_number == out_interface.saturating_add(1)
                    && source.alt_setting == out_alt)
                    .then_some(2)
            }
        }
    }
}

fn implicit_profile_proven(
    profile: PlaybackImplicitProfile,
    _standard_sink: bool,
    _standard_source: bool,
    _rate_proven: bool,
) -> bool {
    match profile {
        PlaybackImplicitProfile::Skip => false,
        PlaybackImplicitProfile::Generic => true,
        #[cfg(test)]
        PlaybackImplicitProfile::Fixed { .. } => {
            _standard_sink && _standard_source && _rate_proven
        }
    }
}

fn resolve_implicit_source(
    inventory: &[ImplicitSourceCandidate],
    config: &ConfigDescriptor,
    handle: &DeviceHandle<Context>,
    out_interface: u8,
    out_alt: u8,
    out_terminal: Option<u8>,
    linked_address: u8,
    service_interval_us: u32,
    sample_rate: Option<u32>,
    standard_sink: bool,
) -> Option<ImplicitFeedbackSource> {
    resolve_implicit_sources(inventory, config, handle, out_interface, out_alt,
        out_terminal, linked_address, service_interval_us, sample_rate, standard_sink, false)
        .into_iter().next()
}

fn resolve_implicit_sources(
    inventory: &[ImplicitSourceCandidate],
    config: &ConfigDescriptor,
    handle: &DeviceHandle<Context>,
    out_interface: u8,
    out_alt: u8,
    out_terminal: Option<u8>,
    linked_address: u8,
    service_interval_us: u32,
    sample_rate: Option<u32>,
    standard_sink: bool,
    retain_alternates: bool,
) -> Vec<ImplicitFeedbackSource> {
    let mut matching = Vec::new();
    let mut standard_sources = Vec::new();
    for (source, rank) in associated_implicit_sources(
        inventory, config, handle, out_interface, out_alt, out_terminal,
        linked_address, service_interval_us, sample_rate.is_some(), standard_sink, None,
    ) {
        if let Some(sample_rate) = sample_rate {
            let rates = if source.rates.is_empty() {
                get_wire_frequency_ranges(
                    handle,
                    AudioControlClock { interface_number: source.control_interface, clock_id: source.clock.source_id },
                    Some(&source.clock),
                ).unwrap_or_default()
            } else { source.rates.clone() };
            if rates.is_empty()
                || !sampling_frequency_ranges_support_rate(&rates, sample_rate)
                || source.source.max_packet_bytes < required_max_packet_bytes(
                    sample_rate, service_interval_us, source.source.bytes_per_frame,
                ) { continue; }
        }
        matching.push((source.source, rank));
        if retain_alternates && source.standard_pcm { standard_sources.push(source.source); }
    }
    if !retain_alternates {
        return choose_implicit_source(matching, out_alt).into_iter().collect();
    }
    let mut sources = ranked_implicit_sources(matching, out_alt, true);
    // Legacy descriptors keep their original selection. Additional live
    // alternates require the standard PCM descriptor proof on both sides.
    if sources.first().is_some_and(|source| standard_sources.contains(source)) {
        sources.retain(|source| standard_sources.contains(source));
    } else {
        sources.truncate(1);
    }
    sources
}

/// Share association proof between exact selection and full-grid publication.
/// Capacity and rate selection deliberately happen after this predicate.
fn associated_implicit_sources<'a>(
    inventory: &'a [ImplicitSourceCandidate], config: &ConfigDescriptor,
    handle: &DeviceHandle<Context>, out_interface: u8, out_alt: u8,
    out_terminal: Option<u8>, linked_address: u8, interval: u32,
    rate_proven: bool, standard_sink: bool, known_clock: Option<&ClockChoice>,
) -> Vec<(&'a ImplicitSourceCandidate, u8)> {
    let Ok(identity) = handle.device().device_descriptor() else { return Vec::new(); };
    let profile = playback_implicit_profile(identity.vendor_id(), identity.product_id());
    let mut clocks = std::collections::HashMap::new();
    let mut matching = Vec::new();
    for source in inventory {
        if !implicit_profile_proven(profile, standard_sink, source.standard_pcm, rate_proven) {
            continue;
        }
        let Some(rank) = implicit_source_rank(profile, source.source, source.marked,
            out_interface, out_alt, linked_address) else { continue; };
        if !source.function_interfaces.contains(&out_interface)
            || (source.source.interface_number == out_interface && source.source.alt_setting != out_alt)
            || source.source.service_interval_us != interval { continue; }
        let clock = if let Some(clock) = known_clock {
            Some(clock)
        } else {
            clocks.entry(source.control_interface).or_insert_with(||
                strict_implicit_clock(config, handle, source.control_interface, out_terminal)).as_ref()
        };
        if clock.is_some_and(|clock| implicit_clocks_compatible(clock, &source.clock)) {
            matching.push((source, rank));
        }
    }
    matching
}

/// Resolve only the unique IAD-owned OUT clock, never a global terminal-ID lookup.
fn standard_output_clock(
    config: &ConfigDescriptor, handle: &DeviceHandle<Context>, interface: u8,
    terminal: Option<u8>, claimed: &[u8],
) -> Option<ClockChoice> {
    let raw = read_active_configuration_bytes(handle, config)?;
    let functions = audio_functions(&raw);
    let matching: Vec<_> = functions.iter().filter(|(_, ids)| ids.contains(&interface)).collect();
    let [(control, _)] = matching.as_slice() else { return None; };
    if !claimed.contains(control) { return None; }
    strict_implicit_clock(config, handle, *control, terminal)
}

fn standard_implicit_routes(
    inventory: &[ImplicitSourceCandidate], config: &ConfigDescriptor, handle: &DeviceHandle<Context>,
    out: &rusb::InterfaceDescriptor, endpoint: &rusb::EndpointDescriptor,
    format: &StreamingInterfaceFormat, clock: &ClockChoice,
    ranges: &[SamplingFrequencySubrange], capacity: usize, interval: u32,
) -> super::implicit_ranges::ImplicitRatePlan {
    let ranges = capacity_limited_rates(ranges, capacity, interval,
        usize::from(format.channels) * usize::from(format.subslot_size)).0;
    let associated = associated_implicit_sources(inventory, config, handle,
        out.interface_number(), out.setting_number(), format.terminal_link, endpoint.synch_address(),
        interval, !ranges.is_empty(), true, Some(clock));
    let matching: Vec<_> = associated.into_iter().map(|(candidate, rank)| (candidate.source, rank)).collect();
    super::implicit_ranges::implicit_rate_routes(&ranges, &matching, out.setting_number())
}

pub(super) fn uac2_choice_has_multiplier(choice: Option<&ClockChoice>) -> bool {
    use crate::clock_resolution::ClockHopKind;
    choice.is_some_and(|choice| choice.path.iter().any(|hop|
        matches!(hop.kind, ClockHopKind::Multiplier | ClockHopKind::RateMultiplier { .. })))
}

/// Implicit feedback shares a wire cadence, not just a source entity ID.
pub(super) fn implicit_clocks_compatible(output: &ClockChoice, input: &ClockChoice) -> bool {
    use crate::clock_resolution::ClockResolution;
    if output.resolution != ClockResolution::Resolved || input.resolution != ClockResolution::Resolved
        || output.control_interface != input.control_interface || output.source_id != input.source_id {
        return false;
    }
    let (Ok(output_ratio), Ok(input_ratio)) = (uac2_clock_ratio(Some(output)), uac2_clock_ratio(Some(input))) else {
        return false;
    };
    output_ratio == input_ratio && output.path.iter().all(|left| {
        input.path.iter().all(|right| left.id != right.id || left.kind == right.kind)
    })
}

pub(super) fn choose_implicit_source(
    matching: Vec<(ImplicitFeedbackSource, u8)>,
    out_alt: u8,
) -> Option<ImplicitFeedbackSource> {
    ranked_implicit_sources(matching, out_alt, false).into_iter().next()
}

/// Prove one physical pipe at the best association rank before exposing its
/// alternates. A lower-ranked declaration can describe another usable alternate
/// of that pipe, but can never introduce a different pipe or resolve ambiguity.
fn ranked_implicit_sources(
    mut matching: Vec<(ImplicitFeedbackSource, u8)>,
    out_alt: u8,
    retain_alternates: bool,
) -> Vec<ImplicitFeedbackSource> {
    let Some(rank) = matching.iter().map(|(_, rank)| *rank).min() else { return Vec::new(); };
    let first = matching.iter().find(|(_, r)| *r == rank).unwrap().0;
    let same_pipe = |source: &ImplicitFeedbackSource| {
        source.interface_number == first.interface_number && source.address == first.address
    };
    if matching.iter().any(|(source, r)| *r == rank && !same_pipe(source)) {
        return Vec::new();
    }
    matching.retain(|(source, r)| same_pipe(source) && (retain_alternates || *r == rank));
    matching.sort_by_key(|(source, r)| (*r, super::implicit_ranges::implicit_source_preference(source, out_alt)));
    let mut sources = Vec::new();
    for (source, _) in matching {
        if !sources.contains(&source) { sources.push(source); }
    }
    if !retain_alternates { sources.truncate(1); }
    sources
}

/// Resolve the selected playback clock within the proven implicit function.
pub(super) fn candidate_audio_control_clock_choice(
    device: &Device<Context>,
    handle: &DeviceHandle<Context>,
    candidate: &IsoStreamCandidate,
) -> Result<Option<(AudioControlClock, ClockChoice)>, String> {
    // ADC3 reports the same public identity with its protocol-specific proof.
    let config = device.active_config_descriptor().map_err(|error| error.to_string())?;
    if config.interfaces().flat_map(|i| i.descriptors())
        .any(|d| d.interface_number() == candidate.interface_number && d.protocol_code() == 0x30) {
        let route = adc3_route_for_candidate(device, handle, candidate)?
            .ok_or_else(|| "ADC3 playback route disappeared".to_string())?;
        return Ok(route.choice.map(|choice| (AudioControlClock {
            interface_number: choice.control_interface, clock_id: choice.source_id,
        }, choice)));
    }
    if let Some(source) = candidate.implicit_feedback {
        let inventory = implicit_source_inventory(&config, handle, candidate.usb_speed, None);
        let source = inventory.iter().find(|entry| entry.source == source)
            .ok_or_else(|| "implicit feedback source clock proof disappeared".to_string())?;
        let clock = strict_implicit_clock(
            &config, handle, source.control_interface, candidate.terminal_link,
        ).ok_or_else(|| "implicit playback clock could not be proved".to_string())?;
        if !implicit_clocks_compatible(&clock, &source.clock) {
            return Err("implicit playback and feedback clock paths disagree".into());
        }
        Ok(Some((AudioControlClock {
            interface_number: clock.control_interface, clock_id: clock.source_id,
        }, clock)))
    } else {
        if let Some((control, graph)) = owning_clock_graph(device, handle, candidate.interface_number) {
            let selected = match graph.select_clock(control, candidate.terminal_link, |id| {
                get_clock_selector_pin(handle, control, id)
            }) {
                Ok(selected) => selected,
                Err(error) if candidate.terminal_link.is_some_and(|terminal|
                    graph.terminal_may_use_multiplier(terminal)) => return Err(error),
                Err(_) => return Ok(None),
            };
            let choice = prove_uac2_clock_choice(handle, &graph, &selected.choice)?;
            return Ok(Some((AudioControlClock { interface_number: control, clock_id: choice.source_id }, choice)));
        }
        find_audio_control_clock_choice(device, handle, candidate.terminal_link)
    }
}

/// Check registration-only Android metadata without expanding runtime device
/// state; actual successful claims remain authoritative for every selection.
pub(super) fn validate_registration_handoff(
    device: &UsbDevice,
    expected_configuration: Option<u8>,
    unavailable: &[u8],
) -> Result<(), String> {
    let claimed = wrap_android_usb_handle(device)?;
    let config = claimed
        .handle
        .device()
        .active_config_descriptor()
        .map_err(|e| e.to_string())?;
    if expected_configuration.is_some_and(|expected| expected != config.number()) {
        return Err(format!(
            "USB active configuration changed: expected {:?}, got {}",
            expected_configuration,
            config.number()
        ));
    }
    validate_android_usb_claims(&claimed, &config)?;
    let unavailable_set: HashSet<_> = unavailable.iter().copied().collect();
    if unavailable_set.len() != unavailable.len()
        || unavailable
            .iter()
            .any(|id| device.claimed_interfaces.contains(id))
    {
        return Err(
            "Unavailable feedback interfaces contain duplicates or overlap successful claims"
                .to_string(),
        );
    }
    let optional = structural_feedback_interfaces(
        &config,
        &claimed.handle,
        resolve_device_speed(&claimed.handle.device()),
        &claimed.claimed_interfaces,
    );
    if unavailable.iter().any(|id| !optional.contains(id)) {
        return Err(
            "Unavailable feedback interface is outside the native preflight allowlist".to_string(),
        );
    }
    Ok(())
}

fn discover_candidate_rate_ranges(
    device: &Device<Context>,
    handle: &DeviceHandle<Context>,
    config: &ConfigDescriptor,
    sources: &[ImplicitSourceCandidate],
    interface: u8,
    terminal: Option<u8>,
    parsed: &[SamplingFrequencySubrange],
) -> Vec<SamplingFrequencySubrange> {
    if !parsed.is_empty() {
        return parsed.to_vec();
    }
    // A function-proven clock takes precedence over the legacy global lookup,
    // whose first AC interface could contain the same local terminal ID.
    for source in sources {
        if source.function_interfaces.contains(&interface) {
            if let Some(clock) =
                strict_implicit_clock(config, handle, source.control_interface, terminal)
            {
                return get_wire_frequency_ranges(
                    handle,
                    AudioControlClock { interface_number: clock.control_interface, clock_id: clock.source_id },
                    Some(&clock),
                )
                .unwrap_or_default();
            }
        }
    }
    let alternatives = selectable_clock_ranges(device, handle, interface, terminal);
    if !alternatives.is_empty() {
        return alternatives;
    }
    discover_stream_sample_rate_ranges(device, handle, interface, terminal, parsed)
}

/// Explicit duplex registration admits only capture preflight IN claims in addition
/// to the unchanged base playback and optional vendor/feedback claims.
fn validate_duplex_capture_claims(claimed: &[u8], controls: &[u8], inputs: &[u8]) -> Result<(), String> {
    if inputs.is_empty() || controls.is_empty()
        || !inputs.iter().chain(controls).all(|id| claimed.contains(id)) {
        return Err("duplex requires capture IN interfaces and their owning AudioControl claims".into());
    }
    Ok(())
}

pub(super) fn inspect_duplex_capabilities(device: &UsbDevice, expected: Option<u8>) -> Result<CapabilityModel, String> {
    let (configuration, controls, inputs) = capture_stream::capture_claim_preflight(device)?;
    if expected.is_some_and(|id| id != configuration) {
        return Err("duplex configuration changed since capture preflight".into());
    }
    let claimed = wrap_android_usb_handle(device)?;
    let descriptor = claimed.handle.device().active_config_descriptor().map_err(|e| e.to_string())?;
    if let Some(profile) = vendor_pcm::discover(&claimed.handle, &descriptor, resolve_device_speed(&claimed.handle.device()))? {
        capture_stream::require_vendor_capture_pcm(&profile)?;
        if !controls.is_empty() || inputs != [profile.input_interface_number] || !profile.claims_match(&device.claimed_interfaces) {
            return Err("vendor PCM duplex requires its exact proven streaming interface claims".into());
        }
    } else {
        validate_duplex_capture_claims(&device.claimed_interfaces, &controls, &inputs)?;
    }
    let usb_device = claimed.handle.device();
    let config = usb_device.active_config_descriptor().map_err(|e| e.to_string())?;
    if config.number() != configuration { return Err("duplex configuration changed".into()); }
    validate_android_usb_claims_with_capture(&claimed, &config, &inputs)?;
    build_usb_capability_model(&usb_device, &claimed.handle, resolve_device_speed(&usb_device), &claimed.claimed_interfaces)
}

pub(super) fn inspect_usb_capabilities(device: &UsbDevice) -> Result<CapabilityModel, String> {
    let claimed_handle = wrap_android_usb_handle(device)?;
    let inspected_device = claimed_handle.handle.device();
    let config = inspected_device
        .active_config_descriptor()
        .map_err(|error| format!("Failed to read active USB config descriptor: {}", error))?;
    // Capability discovery makes clock/endpoint control requests. Reject a
    // mismatched handoff before any of those can implicitly claim interfaces.
    validate_android_usb_claims(&claimed_handle, &config)?;
    build_usb_capability_model(
        &inspected_device,
        &claimed_handle.handle,
        resolve_device_speed(&inspected_device),
        &claimed_handle.claimed_interfaces,
    )
}

pub(super) fn build_usb_capability_model(
    device: &Device<Context>,
    handle: &DeviceHandle<Context>,
    speed: Speed,
    claimed_interfaces: &[u8],
) -> Result<CapabilityModel, String> {
    let config_descriptor = device
        .active_config_descriptor()
        .map_err(|error| format!("Failed to read active USB config descriptor: {}", error))?;

    if let Some(profile) = vendor_pcm::discover(handle, &config_descriptor, speed)? {
        if !profile.claims_match(claimed_interfaces) {
            return Err("vendor PCM requires exactly its proven interface claims".into());
        }
        if settings().disable_feedback() {
            return Err("vendor PCM requires implicit feedback".into());
        }
        let sample_rates = profile.sample_rates();
        return Ok(CapabilityModel {
            supported_sample_rates: sample_rates.clone(), link_speed: Some(link_speed_of(profile.speed)),
            alt_settings: vec![AltSetting {
                interface_number: profile.interface_number, alt_setting: profile.alt_setting,
                endpoint_address: profile.output.address, format: AltFormat::Pcm,
                channels: profile.output.channels, subslot_bytes: profile.output.subslot_bytes, valid_bits: profile.output.subslot_bytes * 8,
                rate_ranges: sample_rates.iter().copied().map(|rate| RateRange { min_hz: rate, max_hz: rate, step_hz: 0 }).collect(),
                sample_rates, sync_type: SyncKind::Asynchronous,
                feedback_endpoint: None, max_packet_bytes: profile.output.max_packet_bytes,
                service_interval_us: profile.output.service_us, uac_version: UacVersion::Unknown,
                endpoint_interval: profile.output.interval, usage: EndpointUsage::Data, refresh: 0, synch_address: 0,
                pacing: Some(crate::capabilities::Pacing::ImplicitFeedback { interface_number: profile.input_interface_number,
                    alt_setting: profile.input_alt_setting, endpoint: profile.input.address }),
            }],
        });
    }
    let implicit_sources = if settings().disable_feedback() {
        Vec::new()
    } else {
        implicit_source_inventory(&config_descriptor, handle, speed, Some(claimed_interfaces))
    };
    let mut alt_settings = Vec::new();
    let mut supported_sample_rates = Vec::new();
    let mut stream_rate_cache =
        std::collections::HashMap::<(u8, Option<u8>), Vec<SamplingFrequencySubrange>>::new();
    let mut standard_grid_cache = std::collections::HashMap::<
        (u8, Option<u8>), Option<(ClockChoice, Vec<SamplingFrequencySubrange>)>,
    >::new();
    let usb_id = device.device_descriptor().ok().map(|d| (d.vendor_id(), d.product_id()));
    let mut effective_speed = speed;

    for interface in config_descriptor.interfaces() {
        for descriptor in interface.descriptors() {
            if descriptor.class_code() != USB_CLASS_AUDIO
                || descriptor.sub_class_code() != USB_SUBCLASS_AUDIOSTREAMING
            {
                continue;
            }

            let Some(mut stream_format) = parse_android_streaming_interface_format(&descriptor)
            else {
                continue;
            };
            let ap80_dsd = matches_ap80_raw_dsd(
                usb_id, config_descriptor.number(), &descriptor, &stream_format,
            );
            stream_format.native_dsd = if ap80_dsd {
                ap80_native_dsd_profile(&settings())
            } else {
                resolve_native_dsd(device, &config_descriptor, &descriptor, &stream_format)
            };
            if native_only_profile(&stream_format) {stream_format.format_tag=FORMAT_TAG_DSD;}
            let playback_designs_dop = resolve_playback_designs_dop(device, &config_descriptor, &descriptor, &stream_format);
            let standard_grid = descriptor.protocol_code() == 0x20
                && standard_implicit_pcm_format(&stream_format)
                && stream_format.sample_rate_ranges.is_empty()
                && stream_format.native_dsd.is_none() && !playback_designs_dop
                && !settings().disable_feedback();
            let only_standard_implicit = standard_grid && !settings().allow_unsynchronized_async()
                && matches!(speed, Speed::Full | Speed::High | Speed::Super | Speed::SuperPlus)
                && descriptor.endpoint_descriptors()
                .filter(|endpoint| endpoint.direction() == Direction::Out
                    && endpoint.transfer_type() == TransferType::Isochronous)
                .all(|endpoint| endpoint.sync_type() == SyncType::Asynchronous
                    && (1..=16).contains(&endpoint.interval())
                    && valid_standard_implicit_capacity(&endpoint, speed)
                    && standard_endpoint_geometry(&endpoint, speed).is_ok()
                    && matches!(endpoint.usage_type(), UsageType::Data | UsageType::FeedbackData | UsageType::Feedback)
                    && matches!(checked_standard_feedback(&descriptor, endpoint.address(),
                        endpoint.synch_address(), endpoint.sync_type(), speed), Ok(None)));
            let sample_rate_ranges = if only_standard_implicit { Vec::new() } else { stream_rate_cache
                .entry((descriptor.interface_number(), stream_format.terminal_link))
                .or_insert_with(|| {
                    discover_candidate_rate_ranges(
                        device,
                        handle,
                        &config_descriptor,
                        &implicit_sources,
                        descriptor.interface_number(),
                        stream_format.terminal_link,
                        &stream_format.sample_rate_ranges,
                    )
                })
                .clone() };
            let sample_rate_ranges = if ap80_dsd {
                ap80_native_dsd_clock_rates(&sample_rate_ranges)
            } else {
                sample_rate_ranges
            };
            let sample_rates = representative_sample_rates_from_ranges(&sample_rate_ranges);

            let speed = if speed == Speed::Unknown {
                infer_high_speed_from_sample_rates(&sample_rate_ranges).unwrap_or(speed)
            } else {
                speed
            };
            dev_eprintln!("[USB] build_usb_capability_model using speed={:?}", speed);
            effective_speed = speed;
            for endpoint in descriptor.endpoint_descriptors() {
                if endpoint.direction() != Direction::Out
                    || endpoint.transfer_type() != TransferType::Isochronous
                {
                    continue;
                }

                let Ok((max_packet_bytes, service_interval_us)) =
                    standard_endpoint_geometry(&endpoint, speed) else { continue; };
                let max_packet_bytes=if settings().has(crate::Quirk::TxLengthPrefix) {
                    if descriptor.interface_number()!=1 || !stream_format.is_uac1 || stream_format.format_tag!=FORMAT_TAG_PCM {continue;}
                    let Some(capacity)=max_packet_bytes.checked_sub(4) else {continue;};
                    capacity
                } else {max_packet_bytes};

                let Ok(discovered_feedback) = checked_standard_feedback(
                    &descriptor,
                    endpoint.address(),
                    endpoint.synch_address(),
                    endpoint.sync_type(),
                    speed,
                ) else { continue; };
                let feedback_address = discovered_feedback.map(|feedback| feedback.address);
                let (mut endpoint_ranges, mut endpoint_rates) = if matches!(speed, Speed::Super | Speed::SuperPlus) || settings().has(crate::Quirk::TxLengthPrefix) {
                    capacity_limited_rates(&sample_rate_ranges, max_packet_bytes, service_interval_us,
                        usize::from(stream_format.channels) * usize::from(stream_format.subslot_size))
                } else {
                    (sample_rate_ranges.clone(), sample_rates.clone())
                };
                let standard_routes = if standard_grid && discovered_feedback.is_none()
                    && endpoint.sync_type() == SyncType::Asynchronous
                    && matches!(endpoint.usage_type(), UsageType::Data | UsageType::FeedbackData | UsageType::Feedback)
                    && (1..=16).contains(&endpoint.interval())
                    && valid_standard_implicit_capacity(&endpoint, speed)
                {
                    let grid = standard_grid_cache.entry((descriptor.interface_number(), stream_format.terminal_link))
                        .or_insert_with(|| {
                            let clock = standard_output_clock(&config_descriptor, handle,
                                descriptor.interface_number(), stream_format.terminal_link, claimed_interfaces)?;
                            let ranges = get_wire_frequency_ranges(handle, AudioControlClock {
                                interface_number: clock.control_interface, clock_id: clock.source_id,
                            }, Some(&clock)).unwrap_or_default();
                            (!ranges.is_empty()).then_some((clock, ranges))
                        });
                    let plan = grid.as_ref().map(|(clock, ranges)| standard_implicit_routes(
                        &implicit_sources, &config_descriptor, handle, &descriptor, &endpoint,
                        &stream_format, clock, ranges, max_packet_bytes, service_interval_us));
                    // Diagnostic fallback retains its historical discovery policy only
                    // when no shared wire grid can be proved.
                    if plan.is_some() || settings().allow_unsynchronized_async() { plan }
                    else { Some(super::implicit_ranges::ImplicitRatePlan {
                        routes: Vec::new(), without_source: Vec::new(),
                    }) }
                } else { None };
                let implicit = if standard_routes.is_none() && discovered_feedback.is_none()
                    && endpoint.sync_type() == SyncType::Asynchronous
                    && !stream_format.is_uac1
                    && type_i_pcm_tag(stream_format.format_tag)
                {
                    endpoint_rates.iter().find_map(|rate| {
                        resolve_implicit_source(
                            &implicit_sources,
                            &config_descriptor,
                            handle,
                            descriptor.interface_number(),
                            descriptor.setting_number(),
                            stream_format.terminal_link,
                            endpoint.synch_address(),
                            service_interval_us,
                            Some(*rate),
                            descriptor.protocol_code() == 0x20
                                && standard_implicit_pcm_format(&stream_format)
                                && (1..=16).contains(&endpoint.interval())
                                && valid_standard_implicit_capacity(&endpoint, speed)
                                && max_packet_bytes >= required_max_packet_bytes(
                                    *rate,
                                    service_interval_us,
                                    usize::from(stream_format.channels)
                                        * usize::from(stream_format.subslot_size),
                                ),
                        )
                    })
                } else {
                    None
                }
                .filter(|source| claimed_interfaces.contains(&source.interface_number));
                if let Some(source) = implicit.filter(|_| matches!(speed, Speed::Super | Speed::SuperPlus)) {
                    (endpoint_ranges, endpoint_rates) = capacity_limited_rates(
                        &endpoint_ranges, source.max_packet_bytes, source.service_interval_us,
                        source.bytes_per_frame,
                    );
                }
                let pacing = if let Some(source) = implicit {
                    Some(crate::capabilities::Pacing::ImplicitFeedback {
                        interface_number: source.interface_number,
                        alt_setting: source.alt_setting,
                        endpoint: source.address,
                    })
                } else if settings().disable_feedback() && discovered_feedback.is_some() {
                    if settings().allow_unsynchronized_async() {
                        Some(admitted_pacing(endpoint.sync_type(), None))
                    } else {
                        None
                    }
                } else {
                    endpoint_pacing(
                        endpoint.sync_type(),
                        endpoint.synch_address(),
                        feedback_address,
                        settings().allow_nominal_uac1(stream_format.is_uac1, speed == Speed::Full,
                            type_i_pcm_tag(stream_format.format_tag), endpoint.synch_address()),
                    )
                };
                let base_alt = AltSetting {
                    interface_number: descriptor.interface_number(),
                    alt_setting: descriptor.setting_number(),
                    endpoint_address: endpoint.address(),
                    format: if stream_format.format_tag == FORMAT_TAG_DSD { AltFormat::Other } else { alt_format(stream_format.format_tag) },
                    channels: stream_format.channels,
                    subslot_bytes: stream_format.subslot_size,
                    valid_bits: stream_format.bit_resolution,
                    rate_ranges: endpoint_ranges
                        .iter()
                        .map(|range| RateRange {
                            min_hz: range.min,
                            max_hz: range.max,
                            step_hz: range.res,
                        })
                        .collect(),
                    sample_rates: endpoint_rates.clone(),
                    sync_type: sync_kind(endpoint.sync_type()),
                    feedback_endpoint: feedback_address,
                    max_packet_bytes,
                    service_interval_us,
                    uac_version: if stream_format.is_uac1 {
                        UacVersion::V1
                    } else {
                        UacVersion::V2
                    },
                    endpoint_interval: endpoint.interval(),
                    usage: endpoint_usage(endpoint.usage_type()),
                    refresh: endpoint.refresh(),
                    synch_address: endpoint.synch_address(),
                    pacing,
                };
                let superspeed = matches!(speed, Speed::Super | Speed::SuperPlus);
                if let Some(plan) = standard_routes {
                    let mut published = false;
                    let output_claimed = claimed_interfaces.contains(&descriptor.interface_number());
                    // Rank/ambiguity was resolved against every physical candidate before
                    // claims are considered; an unavailable preferred pipe cannot be replaced.
                    for (source, ranges) in plan.routes {
                        if !output_claimed || !claimed_interfaces.contains(&source.interface_number) { continue; }
                        let mut alt = base_alt.clone();
                        alt.rate_ranges = ranges.iter().map(|range| RateRange {
                            min_hz: range.min, max_hz: range.max, step_hz: range.res,
                        }).collect();
                        alt.sample_rates = representative_sample_rates_from_ranges(&ranges);
                        alt.pacing = Some(crate::capabilities::Pacing::ImplicitFeedback {
                            interface_number: source.interface_number, alt_setting: source.alt_setting,
                            endpoint: source.address,
                        });
                        supported_sample_rates.extend(alt.sample_rates.iter().copied());
                        alt_settings.push(alt);
                        published = true;
                    }
                    if output_claimed && settings().allow_unsynchronized_async() && endpoint.synch_address() == 0
                        && !plan.without_source.is_empty()
                    {
                        let mut nominal = base_alt.clone();
                        nominal.rate_ranges = plan.without_source.iter().map(|range| RateRange {
                            min_hz: range.min, max_hz: range.max, step_hz: range.res,
                        }).collect();
                        nominal.sample_rates = representative_sample_rates_from_ranges(&plan.without_source);
                        nominal.pacing = Some(admitted_pacing(endpoint.sync_type(), None));
                        supported_sample_rates.extend(nominal.sample_rates.iter().copied());
                        alt_settings.push(nominal);
                        published = true;
                    }
                    if !published {
                        let mut descriptor_only = base_alt.clone();
                        descriptor_only.rate_ranges.clear();
                        descriptor_only.sample_rates.clear();
                        descriptor_only.pacing = None;
                        alt_settings.push(descriptor_only);
                    }
                } else if !superspeed || sample_rate_ranges.is_empty() || !endpoint_ranges.is_empty() {
                    supported_sample_rates.extend(endpoint_rates.iter().copied());
                    alt_settings.push(base_alt.clone());
                }
                if let Some(profile) = stream_format.native_dsd {
                    let mut native = base_alt.clone();
                    native.format = AltFormat::Dsd;
                    native.subslot_bytes = profile.word_bytes;
                    native.valid_bits = 1;
                    // Native selection changes the format tag before resolving
                    // pacing, so it cannot borrow the PCM implicit-feedback pipe.
                    native.pacing = if !claimed_interfaces.contains(&descriptor.interface_number())
                        || descriptor.setting_number() == 0
                        || !matches!(endpoint.usage_type(), UsageType::Data | UsageType::FeedbackData | UsageType::Feedback)
                        || (settings().disable_feedback()
                            && endpoint.sync_type() == SyncType::Asynchronous
                            && !settings().allow_unsynchronized_async())
                    {
                        None
                    } else {
                        match admit_endpoint(endpoint.sync_type(), endpoint.synch_address(),
                            discovered_feedback.is_some(), settings().allow_unsynchronized_async()) {
                            EndpointAdmission::Admit => Some(admitted_pacing(endpoint.sync_type(),
                                feedback_address.filter(|_| !settings().disable_feedback()))),
                            _ => None,
                        }
                    };
                    let (ranges, rates) = capacity_limited_rates(
                        &sample_rate_ranges, max_packet_bytes, service_interval_us,
                        usize::from(stream_format.channels) * usize::from(profile.word_bytes),
                    );
                    // An empty clock response can still be verified during
                    // negotiation; a known range that cannot fit cannot.
                    if !sample_rate_ranges.is_empty() && ranges.is_empty() { continue; }
                    native.rate_ranges = ranges.iter().map(|range| RateRange {
                        min_hz: range.min, max_hz: range.max, step_hz: range.res,
                    }).collect();
                    native.sample_rates = rates;
                    if native.pacing.is_none() {
                        native.rate_ranges.clear();
                        native.sample_rates.clear();
                    }
                    supported_sample_rates.extend(native.sample_rates.iter().copied());
                    alt_settings.push(native);
                }
                if playback_designs_dop {
                    // This exact alternate is excluded by the native resolver.
                    let mut dop = base_alt.clone();
                    dop.format = AltFormat::Dop;
                    dop.subslot_bytes = 3;
                    dop.valid_bits = 24;
                    // Preserve the descriptor's rates but cap the projected wire
                    // carrier by whole stereo frames per USB service interval.
                    let ceiling = ((max_packet_bytes / 6) as u64 * 1_000_000
                        / u64::from(service_interval_us.max(1))).min(u64::from(u32::MAX)) as u32;
                    dop.rate_ranges.retain_mut(|range| {
                        range.max_hz = range.max_hz.min(ceiling);
                        range.min_hz <= range.max_hz
                    });
                    dop.sample_rates.retain(|rate|
                        playback_designs_dop_capacity(max_packet_bytes, service_interval_us, *rate));
                    if !dop.rate_ranges.is_empty() || !dop.sample_rates.is_empty() {
                        alt_settings.push(dop);
                    }
                }
                // Non-PCM masks stay visible as Other, but never masquerade as
                // linear PCM or native DSD without a resolved profile.
            }
        }
    }

    for (candidate, ranges) in adc3::adc3_candidates_with_ranges(device, handle, speed, claimed_interfaces, true) {
        if ranges.is_empty() { continue; }
        let rates = candidate.sample_rates;
        supported_sample_rates.extend(rates.iter().copied());
        alt_settings.push(AltSetting {
            interface_number: candidate.interface_number, alt_setting: candidate.alt_setting,
            endpoint_address: candidate.endpoint_address,
            format: if candidate.native_dsd.is_some() { AltFormat::Dsd } else { alt_format(candidate.format_tag) },
            channels: candidate.channels, subslot_bytes: candidate.subslot_size,
            valid_bits: if candidate.native_dsd.is_some() { 1 } else { candidate.bit_resolution },
            rate_ranges: ranges.iter().map(|r| RateRange { min_hz:r.min,max_hz:r.max,step_hz:r.res }).collect(),
            sample_rates:rates, sync_type:sync_kind(candidate.sync_type),
            feedback_endpoint:candidate.feedback_endpoint.map(|f|f.address),
            max_packet_bytes:candidate.max_packet_bytes,service_interval_us:candidate.service_interval_us,
            uac_version:UacVersion::V3,endpoint_interval:candidate.endpoint_interval,
            usage:endpoint_usage(candidate.usage_type),refresh:candidate.refresh,
            synch_address:candidate.synch_address,
            pacing:Some(if let Some(source) = candidate.implicit_feedback {
                crate::capabilities::Pacing::ImplicitFeedback { interface_number: source.interface_number, alt_setting: source.alt_setting, endpoint: source.address }
            } else { admitted_pacing(candidate.sync_type,candidate.feedback_endpoint.map(|f|f.address)) }),
        });
    }
    // A UAC2 Type I bitmap may expose signed and float on the same physical alternate.
    // Publish distinct layouts while retaining the exact same rate/ownership/pacing evidence.
    let mut extra_formats = Vec::new();
    for alt in &alt_settings {
        if !matches!(alt.format, AltFormat::Pcm | AltFormat::PcmU8 | AltFormat::PcmFloat32) { continue; }
        let bitmap = config_descriptor.interfaces().flat_map(|itf| itf.descriptors())
            .find(|d| d.interface_number() == alt.interface_number && d.setting_number() == alt.alt_setting)
            .and_then(|d| parse_android_streaming_interface_format(&d)).and_then(|f| f.uac2_formats);
        if let Some(bits) = bitmap {
            for (flag, format, slot, depth) in [(2, AltFormat::PcmU8, 1, 8), (4, AltFormat::PcmFloat32, 4, 32)] {
                if bits & flag != 0 && alt.format != format && alt.subslot_bytes == slot && alt.valid_bits == depth {
                    let mut variant = alt.clone(); variant.format = format; extra_formats.push(variant);
                }
            }
        }
    }
    alt_settings.extend(extra_formats);
    supported_sample_rates.sort_unstable();
    supported_sample_rates.dedup();
    alt_settings.sort_by_key(|alt| {
        (
            alt.interface_number,
            alt.alt_setting,
            alt.endpoint_address,
            alt.valid_bits,
            alt.max_packet_bytes,
        )
    });

    Ok(CapabilityModel {
        supported_sample_rates,
        alt_settings,
        link_speed: Some(link_speed_of(effective_speed)),
    })
}

pub(super) fn select_stream_candidate(
    device: &Device<Context>,
    handle: &DeviceHandle<Context>,
    playback_format: PlaybackFormat,
    speed: Speed,
    claimed_interfaces: &[u8],
) -> Result<IsoStreamCandidate, SelectError> {
    select_stream_candidates(device, handle, playback_format, speed, claimed_interfaces, false)
        .map(|candidates| candidates.into_iter().next().expect("nonempty stream candidates"))
}

pub(super) fn select_stream_candidates(
    device: &Device<Context>,
    handle: &DeviceHandle<Context>,
    playback_format: PlaybackFormat,
    speed: Speed,
    claimed_interfaces: &[u8],
    retain_implicit_alternates: bool,
) -> Result<Vec<IsoStreamCandidate>, SelectError> {
    let config_descriptor = device.active_config_descriptor().map_err(|error| {
        SelectError::NoMatch(format!(
            "Failed to read active USB config descriptor: {}",
            error
        ))
    })?;
    if let Some(profile) = vendor_pcm::discover(handle, &config_descriptor, speed).map_err(SelectError::NoMatch)? {
        if !profile.claims_match(claimed_interfaces) {
            return Err(SelectError::Claim("vendor PCM requires exactly its proven interface claims".into()));
        }
        if settings().disable_feedback() {
            return Err(SelectError::NoMatch("vendor PCM requires implicit feedback".into()));
        }
        if playback_format.dsd_transport != DsdTransportMode::None || playback_format.is_dop
            || playback_format.pcm_encoding != PcmEncoding::Signed || !profile.supports_rate(playback_format.sample_rate) || playback_format.bit_depth != profile.output.subslot_bytes * 8
            || playback_format.channels != profile.output.channels {
            return Err(SelectError::NoMatch("vendor PCM requires its supported native PCM rate and channel format".into()));
        }
        return Ok(vec![profile.candidate(config_descriptor.number())]);
    }
    let implicit_sources = implicit_source_inventory(&config_descriptor, handle, speed, Some(claimed_interfaces));
    let mut refused_unsynchronized = false;
    let mut refused_claim = None;
    let allow_unsynchronized_async = settings().allow_unsynchronized_async();
    let mut candidates = Vec::new();
    let prefer_padded_24bit_transport = settings().prefer_padded_24bit();
    let usb_id = device.device_descriptor().ok().map(|d| (d.vendor_id(), d.product_id()));
    let mut stream_rate_cache =
        std::collections::HashMap::<(u8, Option<u8>), Vec<SamplingFrequencySubrange>>::new();
    for interface in config_descriptor.interfaces() {
        for descriptor in interface.descriptors() {
            if descriptor.class_code() != USB_CLASS_AUDIO
                || descriptor.sub_class_code() != USB_SUBCLASS_AUDIOSTREAMING
            {
                continue;
            }

            let mut stream_format = match parse_android_streaming_interface_format(&descriptor) {
                Some(format) => format,
                None => continue,
            };
            if playback_format.dsd_transport == DsdTransportMode::None {
                let tag = playback_format.pcm_encoding.format_tag();
                let present = stream_format.uac2_formats.map_or(stream_format.format_tag == tag,
                    |bits| bits & (1 << (tag - 1)) != 0);
                if !present { continue; }
                stream_format.format_tag = tag;
            }
            let ap80_dsd = matches_ap80_raw_dsd(
                usb_id, config_descriptor.number(), &descriptor, &stream_format,
            );
            stream_format.native_dsd = if ap80_dsd {
                ap80_native_dsd_profile(&settings())
            } else {
                resolve_native_dsd(device, &config_descriptor, &descriptor, &stream_format)
            };
            if native_only_profile(&stream_format) && playback_format.dsd_transport!=DsdTransportMode::Native {continue;}
            let playback_designs_dop = resolve_playback_designs_dop(device, &config_descriptor, &descriptor, &stream_format);
            apply_playback_designs_dop(&mut stream_format, playback_format, playback_designs_dop);
            if playback_format.dsd_transport == DsdTransportMode::Native {
                let Some(profile) = stream_format.native_dsd else { continue; };
                stream_format.subslot_size = profile.word_bytes;
                stream_format.bit_resolution = 1;
                stream_format.format_tag = FORMAT_TAG_DSD;
            }

            let compatibility_penalty =
                transport_compatibility_penalty(&stream_format, playback_format);
            if compatibility_penalty == u32::MAX {
                continue;
            }

            let sample_rate_ranges = stream_rate_cache
                .entry((descriptor.interface_number(), stream_format.terminal_link))
                .or_insert_with(|| {
                    discover_candidate_rate_ranges(
                        device,
                        handle,
                        &config_descriptor,
                        &implicit_sources,
                        descriptor.interface_number(),
                        stream_format.terminal_link,
                        &stream_format.sample_rate_ranges,
                    )
                })
                .clone();
            let sample_rate_ranges = if ap80_dsd {
                ap80_native_dsd_clock_rates(&sample_rate_ranges)
            } else {
                sample_rate_ranges
            };
            let sample_rates = representative_sample_rates_from_ranges(&sample_rate_ranges);

            let Some(effective_sample_rate) = native_wire_rate(playback_format, stream_format.subslot_size) else { continue; };

            if !sample_rate_ranges.is_empty()
                && stream_format.format_tag != FORMAT_TAG_DSD
                && !sampling_frequency_ranges_support_rate(
                    &sample_rate_ranges,
                    effective_sample_rate,
                )
            {
                continue;
            }

            if stream_format.format_tag == FORMAT_TAG_DSD
                && !sample_rate_ranges.is_empty()
                && !sampling_frequency_ranges_support_rate(
                    &sample_rate_ranges,
                    effective_sample_rate,
                )
            {
                continue;
            }

            for endpoint in descriptor.endpoint_descriptors() {
                if !claimed_interfaces.contains(&descriptor.interface_number()) {
                    continue;
                }
                if descriptor.setting_number() == 0 {
                    continue;
                }
                if endpoint.direction() != Direction::Out
                    || endpoint.transfer_type() != TransferType::Isochronous
                {
                    continue;
                }
                if endpoint.interval() == 0 {
                    dev_eprintln!(
                        "USB direct skipping invalid isochronous endpoint=0x{:02x} on interface {} alt {} because bInterval=0",
                        endpoint.address(),
                        descriptor.interface_number(),
                        descriptor.setting_number(),
                    );
                    continue;
                }

                if !matches!(
                    endpoint.usage_type(),
                    UsageType::Data | UsageType::FeedbackData | UsageType::Feedback
                ) {
                    continue;
                }

                let speed = if speed == Speed::Unknown {
                    infer_high_speed_from_sample_rates(&sample_rate_ranges).unwrap_or(speed)
                } else {
                    speed
                };
                dev_eprintln!(
                    "[USB] select_stream_candidate using speed={:?} for interval={} → service={}us",
                    speed,
                    endpoint.interval(),
                    service_interval_micros(speed, endpoint.interval())
                );
                let Ok((max_packet_bytes, service_interval_us)) =
                    standard_endpoint_geometry(&endpoint, speed) else { continue; };
                let max_packet_bytes=if settings().has(crate::Quirk::TxLengthPrefix) {
                    if descriptor.interface_number()!=1 || !stream_format.is_uac1 || stream_format.format_tag!=FORMAT_TAG_PCM {continue;}
                    let Some(capacity)=max_packet_bytes.checked_sub(4) else {continue;};
                    capacity
                } else {max_packet_bytes};
                let Ok(discovered_feedback) = checked_standard_feedback(
                    &descriptor,
                    endpoint.address(),
                    endpoint.synch_address(),
                    endpoint.sync_type(),
                    speed,
                ) else { continue; };
                let discovered_implicit_sources = if endpoint.sync_type() == SyncType::Asynchronous
                    && discovered_feedback.is_none()
                    && !stream_format.is_uac1
                    && type_i_pcm_tag(stream_format.format_tag)
                    && (1..=16).contains(&endpoint.interval())
                    && valid_standard_implicit_capacity(&endpoint, speed)
                {
                    resolve_implicit_sources(
                        &implicit_sources,
                        &config_descriptor,
                        handle,
                        descriptor.interface_number(),
                        descriptor.setting_number(),
                        stream_format.terminal_link,
                        endpoint.synch_address(),
                        service_interval_us,
                        Some(effective_sample_rate),
                        descriptor.protocol_code() == 0x20
                            && standard_implicit_pcm_format(&stream_format),
                        retain_implicit_alternates
                            && descriptor.protocol_code() == 0x20
                            && standard_implicit_pcm_format(&stream_format)
                            && playback_format.dsd_transport == DsdTransportMode::None
                            && !playback_format.is_dop,
                    )
                } else {
                    Vec::new()
                };
                let discovered_implicit = discovered_implicit_sources.first().copied();
                let implicit_feedback = if settings().disable_feedback() {
                    None
                } else {
                    discovered_implicit
                };
                let feedback_endpoint = if settings().disable_feedback() {
                    None
                } else {
                    discovered_feedback
                };
                let bytes_per_frame =
                    stream_format.subslot_size as usize * stream_format.channels as usize;
                let required_max_packet_bytes = required_max_packet_bytes(
                    effective_sample_rate,
                    service_interval_us,
                    bytes_per_frame,
                );

                dev_eprintln!(
                    "USB direct stream candidate interface={} alt={} endpoint=0x{:02x} interval={} service={}us max_packet={} required={} sync={:?} usage={:?} fmt={} channels={} subslot={} bit_resolution={} sample_rates={:?} penalty={}",
                    descriptor.interface_number(),
                    descriptor.setting_number(),
                    endpoint.address(),
                    endpoint.interval(),
                    service_interval_us,
                    max_packet_bytes,
                    required_max_packet_bytes,
                    endpoint.sync_type(),
                    endpoint.usage_type(),
                    format_tag_label(stream_format.format_tag),
                    stream_format.channels,
                    stream_format.subslot_size,
                    stream_format.bit_resolution,
                    sample_rates,
                    compatibility_penalty,
                );

                if bytes_per_frame == 0 || max_packet_bytes < required_max_packet_bytes {
                    continue;
                }
                if let Some(source) = implicit_feedback {
                    if !claimed_interfaces.contains(&source.interface_number) {
                        refused_claim = Some(source.interface_number);
                        continue;
                    }
                }
                if settings().disable_feedback()
                    && endpoint.sync_type() == SyncType::Asynchronous
                    && !allow_unsynchronized_async
                {
                    refused_unsynchronized = true;
                    continue;
                }
                let allow_unsynchronized_async = settings().allow_nominal_uac1(stream_format.is_uac1,
                    speed == Speed::Full, type_i_pcm_tag(stream_format.format_tag), endpoint.synch_address());
                match admit_endpoint(
                    endpoint.sync_type(),
                    endpoint.synch_address(),
                    discovered_feedback.is_some() || discovered_implicit.is_some(),
                    allow_unsynchronized_async,
                ) {
                    EndpointAdmission::Admit => {}
                    EndpointAdmission::SkipMalformed => {
                        dev_eprintln!(
                            "[USB] Skipping async endpoint=0x{:02x} on interface {} alt {} because feedback endpoint 0x{:02x} was not found",
                            endpoint.address(),
                            descriptor.interface_number(),
                            descriptor.setting_number(),
                            endpoint.synch_address(),
                        );
                        continue;
                    }
                    EndpointAdmission::RefuseUnsynchronized => {
                        log_warn!(
                            "[USB] Refusing asynchronous endpoint=0x{:02x} on interface {} alt {}: it has no explicit feedback endpoint, which would need implicit feedback",
                            endpoint.address(),
                            descriptor.interface_number(),
                            descriptor.setting_number(),
                        );
                        refused_unsynchronized = true;
                        continue;
                    }
                }

                // Claims are checked against the chosen physical pipe above, before
                // expansion; an unclaimed source cannot redirect selection.
                let implicit_alternates = if implicit_feedback.is_some() {
                    discovered_implicit_sources.into_iter().map(Some).collect()
                } else {
                    vec![None]
                };
                for implicit_feedback in implicit_alternates {
                    candidates.push(IsoStreamCandidate {
                        configuration_number: config_descriptor.number(),
                        interface_number: descriptor.interface_number(),
                        alt_setting: descriptor.setting_number(),
                        endpoint_address: endpoint.address(),
                        endpoint_interval: endpoint.interval(),
                        service_interval_us,
                        usb_speed: speed,
                        max_packet_bytes,
                        sync_type: endpoint.sync_type(),
                        usage_type: endpoint.usage_type(),
                        format_tag: stream_format.format_tag,
                        uac2_formats: stream_format.uac2_formats,
                        native_dsd: stream_format.native_dsd,
                        channels: stream_format.channels,
                        subslot_size: stream_format.subslot_size,
                        bit_resolution: stream_format.bit_resolution,
                        sample_rates: sample_rates.clone(),
                        refresh: endpoint.refresh(),
                        synch_address: endpoint.synch_address(),
                        feedback_endpoint,
                        implicit_feedback,
                        terminal_link: stream_format.terminal_link,
                        is_uac1: stream_format.is_uac1,
                    });
                }
            }
        }
    }

    for (candidate, ranges) in adc3::adc3_candidates_with_ranges(device, handle, speed, claimed_interfaces,
        playback_format.dsd_transport != DsdTransportMode::DoP) {
        if adc3::adc3_candidate_matches_ranges(&candidate, &ranges, playback_format) {
            candidates.push(candidate);
        }
    }
    let dsd_bit_rate = playback_format.dsd_bit_rate;
    let is_native_dsd = playback_format.dsd_transport == DsdTransportMode::Native;
    candidates.sort_by_key(|candidate| {
        // Admission already requires an exact native word cadence.
        let candidate_rate = native_wire_rate(playback_format, candidate.subslot_size)
            .expect("selected candidate has an exact wire rate");
        let required_bytes = required_max_packet_bytes(
            candidate_rate,
            candidate.service_interval_us,
            candidate.subslot_size as usize * candidate.channels as usize,
        );
        (
            u8::from(
                candidate.sync_type == SyncType::Asynchronous
                    && candidate.feedback_endpoint.is_none()
                    && candidate.implicit_feedback.is_none(),
            ),
            transport_container_preference_rank(
                candidate,
                playback_format,
                prefer_padded_24bit_transport,
            ),
            transport_compatibility_penalty_for_candidate(candidate, playback_format),
            candidate.max_packet_bytes.saturating_sub(required_bytes),
            candidate.interface_number,
            candidate.alt_setting,
        )
    });

    let label_rate = if is_native_dsd && dsd_bit_rate > 0 {
        format!("DSD {} bit/s", dsd_bit_rate)
    } else {
        format!("{} Hz", playback_format.sample_rate)
    };
    if !candidates.is_empty() { return Ok(candidates); }
    if let Some(interface) = refused_claim {
        return Err(SelectError::Claim(format!("Required implicit feedback interface {interface} was not claimed")));
    }
    if refused_unsynchronized {
        return Err(SelectError::ImplicitFeedback(format!(
            "The only isochronous OUT endpoints that can carry {} / {}-bit / {} ch are asynchronous without a usable claimed feedback source",
            label_rate, playback_format.bit_depth, playback_format.channels
        )));
    }
    Err(SelectError::NoMatch(format!(
        "No isochronous OUT endpoint can carry {} / {}-bit / {} ch",
        label_rate, playback_format.bit_depth, playback_format.channels
    )))
}

/// Log every descriptor-level endpoint that *could* be a UAC2 feedback source,
/// plus what the selected alt setting actually advertises.
///
/// This is diagnostic only: it is emitted once per stream start so the log
/// answers "does this DAC expose a feedback pipe that we failed to find?" without
/// needing a host-side USB capture.
pub(super) fn log_iso_endpoint_descriptors(
    handle: &DeviceHandle<Context>,
    candidate: &IsoStreamCandidate,
) {
    dev_eprintln!(
        "[USB] Endpoint audit: selected interface={} alt={} endpoint=0x{:02x}; \
         feedback={} sync={:?}",
        candidate.interface_number,
        candidate.alt_setting,
        candidate.endpoint_address,
        candidate
            .feedback_endpoint
            .map(|feedback| format!(
                "0x{:02x}/{}@interval{}",
                feedback.address,
                transfer_type_label(feedback.transfer_type),
                feedback.interval,
            ))
            .unwrap_or_else(|| "none".to_string()),
        candidate.sync_type,
    );

    if candidate.sync_type != SyncType::Asynchronous {
        return;
    }
    if let Some(source) = candidate.implicit_feedback {
        log_info!("[USB] Implicit feedback source interface={} alt={} endpoint=0x{:02x} stride={} interval={}us capacity={}",
            source.interface_number, source.alt_setting, source.address, source.bytes_per_frame,
            source.service_interval_us, source.max_packet_bytes);
        return;
    }
    if candidate.feedback_endpoint.is_some() {
        return;
    }

    // Only reached for an async sink with no resolved feedback pipe. Report the
    // neighbouring IN endpoints we rejected and why, so a genuine discovery gap
    // is distinguishable from a device that simply has no feedback.
    dev_eprintln!(
        "[USB] Async sink on interface {} alt {} has no resolved feedback endpoint \
         (bSynchAddress=0x{:02x}). Playback is host-paced on nominal packet timing, so DAC \
         oscillator drift can underrun its FIFO at intervals that scale with the drift offset.",
        candidate.interface_number,
        candidate.alt_setting,
        candidate.synch_address,
    );

    let config = match handle.device().active_config_descriptor() {
        Ok(config) => config,
        Err(_) => return,
    };
    for interface in config.interfaces() {
        if interface.number() != candidate.interface_number {
            continue;
        }
        for alt in interface.descriptors() {
            for endpoint in alt.endpoint_descriptors() {
                if endpoint.direction() != Direction::In {
                    continue;
                }
                dev_eprintln!(
                    "[USB] Endpoint audit: unused IN endpoint=0x{:02x} alt={} type={:?} usage={:?} interval={} maxPacket={} on interface {} (async data endpoint has no feedback)",
                    endpoint.address(),
                    alt.setting_number(),
                    endpoint.transfer_type(),
                    endpoint.usage_type(),
                    endpoint.interval(),
                    endpoint.max_packet_size(),
                    interface.number(),
                );
            }
        }
    }
}

fn valid_standard_endpoint_address(address: u8) -> bool {
    address & 0x70 == 0 && address & 0x0f != 0
}

/// SuperSpeed payload and cadence belong to the endpoint companion, never
/// the high-speed transaction bits in wMaxPacketSize.
fn standard_endpoint_geometry(
    endpoint: &rusb::EndpointDescriptor,
    speed: Speed,
) -> Result<(usize, u32), String> {
    if matches!(speed, Speed::Super | Speed::SuperPlus) {
        if !valid_standard_endpoint_address(endpoint.address()) {
            return Err("invalid SuperSpeed endpoint address".into());
        }
        super::endpoint_geometry::ss_endpoint_geometry(
            endpoint.max_packet_size(), endpoint.interval(), endpoint.extra().unwrap_or(&[]), speed,
            endpoint.transfer_type() == TransferType::Isochronous,
        ).map(|(capacity, interval, _)| (capacity, interval))
    } else {
        Ok((effective_iso_packet_bytes(endpoint.max_packet_size(), speed),
            service_interval_micros(speed, endpoint.interval())))
    }
}

fn valid_standard_implicit_capacity(endpoint: &rusb::EndpointDescriptor, speed: Speed) -> bool {
    if matches!(speed, Speed::Super | Speed::SuperPlus) {
        standard_endpoint_geometry(endpoint, speed).is_ok()
    } else {
        valid_implicit_packet_capacity(endpoint.max_packet_size(), speed)
    }
}

/// Preserve the legacy selector for FS/HS and ADC3. Inspect advertised SS
/// feedback before its legacy size/interval filters can hide malformed reports.
fn checked_standard_feedback(
    descriptor: &rusb::InterfaceDescriptor,
    data_address: u8,
    address: u8,
    sync: SyncType,
    speed: Speed,
) -> Result<Option<FeedbackEndpoint>, String> {
    if !matches!(speed, Speed::Super | Speed::SuperPlus) {
        if explicit_feedback_ambiguous(descriptor, address, sync) {
            return Err("ambiguous explicit feedback".into());
        }
        return Ok(find_feedback_endpoint(descriptor, address, sync, speed));
    }
    if !valid_standard_endpoint_address(data_address)
        || data_address & 0x80 != 0
        || (address != 0 && (!valid_standard_endpoint_address(address) || address & 0x80 == 0))
    {
        return Err("invalid SuperSpeed data or feedback association address".into());
    }
    let advertised: Vec<_> = descriptor.endpoint_descriptors().filter(|ep| {
        ep.direction() == Direction::In && if address != 0 {
            ep.address() == address && ep.usage_type() != UsageType::FeedbackData
        } else {
            sync == SyncType::Asynchronous && ep.usage_type() == UsageType::Feedback
        }
    }).collect();
    let endpoint = match advertised.as_slice() {
        [] => return Ok(None),
        [endpoint] => endpoint,
        _ => return Err("ambiguous explicit feedback".into()),
    };
    if !valid_standard_endpoint_address(endpoint.address()) {
        return Err("invalid SuperSpeed feedback endpoint address".into());
    }
    if !matches!(endpoint.transfer_type(), TransferType::Isochronous | TransferType::Interrupt) {
        return Err("invalid explicit feedback transfer type".into());
    }
    let (max_packet_bytes, service_interval_us, _) =
        super::endpoint_geometry::ss_feedback_geometry(
            endpoint.max_packet_size(), endpoint.interval(), endpoint.extra().unwrap_or(&[]),
            endpoint.transfer_type() == TransferType::Isochronous,
        )?;
    if !standard_ss_feedback_shape(
        max_packet_bytes, endpoint.transfer_type(), endpoint.usage_type(),
        endpoint.sync_type(), sync,
        address == 0 || address == endpoint.address(),
        (data_address, endpoint.address()),
    ) {
        return Err("invalid SuperSpeed explicit feedback association or endpoint shape".into());
    }
    Ok(Some(FeedbackEndpoint {
        address: endpoint.address(), transfer_type: endpoint.transfer_type(),
        interval: endpoint.interval(), refresh: endpoint.refresh(),
        service_interval_us, max_packet_bytes,
    }))
}

/// Q32.32 reports use the standard isochronous feedback endpoint shape.
/// Keep the established four-byte compatibility route's admission unchanged.
fn standard_ss_feedback_shape(
    report_bytes: usize,
    transfer: TransferType,
    usage: UsageType,
    feedback_sync: SyncType,
    sink_sync: SyncType,
    associated: bool,
    addresses: (u8, u8),
) -> bool {
    match report_bytes {
        4 => true,
        8 => transfer == TransferType::Isochronous
            && usage == UsageType::Feedback
            && feedback_sync == SyncType::NoSync
            && sink_sync == SyncType::Asynchronous
            && associated
            && (addresses.0 & 0x0f) == (addresses.1 & 0x0f),
        _ => false,
    }
}

/// Descriptor rates are USB sample/word rates, including native DSD. Clip
/// ranges to whole frames per service interval without converting DSD to bits.
fn capacity_limited_rates(
    ranges: &[SamplingFrequencySubrange], capacity: usize, interval: u32, stride: usize,
) -> (Vec<SamplingFrequencySubrange>, Vec<u32>) {
    if stride == 0 || interval == 0 { return (Vec::new(), Vec::new()); }
    let ceiling = ((capacity / stride) as u64 * 1_000_000 / u64::from(interval))
        .min(u64::from(u32::MAX)) as u32;
    let clipped: Vec<_> = ranges.iter().filter_map(|range| {
        if range.min == 0 || range.max < range.min
            || (range.res == 0 && range.min != range.max) { return None; }
        let mut max = range.max.min(ceiling);
        if range.min > max { return None; }
        if range.res != 0 { max -= (max - range.min) % range.res; }
        Some(SamplingFrequencySubrange { min: range.min, max, res: range.res })
    }).collect();
    let rates = representative_sample_rates_from_ranges(&clipped);
    (clipped, rates)
}

pub(super) fn explicit_feedback_ambiguous(
    descriptor: &rusb::InterfaceDescriptor,
    address: u8,
    sync: SyncType,
) -> bool {
    select_explicit_feedback_endpoint(
        descriptor.setting_number(),
        address,
        sync,
        descriptor
            .endpoint_descriptors()
            .map(|ep| FeedbackEndpointCandidate {
                alt_setting: descriptor.setting_number(),
                address: ep.address(),
                transfer_type: ep.transfer_type(),
                usage_type: ep.usage_type(),
                interval: ep.interval(),
                max_packet_size: ep.max_packet_size(),
            }),
    )
    .is_err()
}

pub(super) fn find_feedback_endpoint(
    descriptor: &rusb::InterfaceDescriptor,
    feedback_address: u8,
    sync_type: SyncType,
    speed: Speed,
) -> Option<FeedbackEndpoint> {
    // Only the selected alt is active. Dawn Pro repeats 0x81 in alts 1..4;
    // walking sibling alts made that valid feedback endpoint look ambiguous.
    let endpoints = descriptor
        .endpoint_descriptors()
        .map(|endpoint| FeedbackEndpointCandidate {
            alt_setting: descriptor.setting_number(),
            address: endpoint.address(),
            transfer_type: endpoint.transfer_type(),
            usage_type: endpoint.usage_type(),
            interval: endpoint.interval(),
            max_packet_size: endpoint.max_packet_size(),
        });
    let endpoint = match select_explicit_feedback_endpoint(
        descriptor.setting_number(),
        feedback_address,
        sync_type,
        endpoints,
    ) {
        Ok(endpoint) => endpoint?,
        Err(addresses) => {
            dev_eprintln!(
                "[USB] Async endpoint has {} feedback IN candidates ({:02x?}) on interface {} alt {}; refusing ambiguous feedback selection",
                addresses.len(),
                addresses,
                descriptor.interface_number(),
                descriptor.setting_number(),
            );
            return None;
        }
    };

    let feedback = FeedbackEndpoint {
        refresh: descriptor.endpoint_descriptors().find(|ep| ep.address() == endpoint.address)
            .map(|ep| ep.refresh()).unwrap_or(0),
        address: endpoint.address,
        transfer_type: endpoint.transfer_type,
        interval: endpoint.interval,
        service_interval_us: if endpoint.transfer_type == TransferType::Interrupt && speed == Speed::Full {
            u32::from(endpoint.interval.max(1)) * 1000
        } else { service_interval_micros(speed, endpoint.interval) },
        max_packet_bytes: effective_iso_packet_bytes(endpoint.max_packet_size, speed),
    };
    let resolution = if feedback_address != 0 && endpoint.address == feedback_address {
        "bSynchAddress"
    } else {
        "UAC2 feedback usage"
    };
    dev_eprintln!(
        "[USB] Feedback endpoint resolved from {}: 0x{:02x} {} interval={} service={}us on interface {} alt {}",
        resolution,
        feedback.address,
        transfer_type_label(feedback.transfer_type),
        feedback.interval,
        feedback.service_interval_us,
        descriptor.interface_number(),
        descriptor.setting_number(),
    );
    Some(feedback)
}

pub(super) fn validate_android_usb_claims(
    claimed_handle: &ClaimedHandle,
    config_descriptor: &ConfigDescriptor,
) -> Result<(), String> {
    validate_android_usb_claims_with_capture(claimed_handle, config_descriptor, &[])
}

fn validate_android_usb_claims_with_capture(
    claimed_handle: &ClaimedHandle,
    config_descriptor: &ConfigDescriptor,
    capture_inputs: &[u8],
) -> Result<(), String> {
    if let Some(profile) = vendor_pcm::discover(&claimed_handle.handle, config_descriptor,
        resolve_device_speed(&claimed_handle.handle.device()))? {
        if !profile.claims_match(&claimed_handle.claimed_interfaces)
            || (!capture_inputs.is_empty() && capture_inputs != [profile.input_interface_number]) {
            return Err("vendor PCM permits only the exact native-proven interface claims".into());
        }
        return Ok(());
    }
    let mut required = HashSet::new();
    let mut has_control = false;
    let mut has_playback = false;
    for interface in config_descriptor.interfaces() {
        for descriptor in interface.descriptors() {
            if descriptor.class_code() != USB_CLASS_AUDIO {
                continue;
            }
            match descriptor.sub_class_code() {
                USB_SUBCLASS_AUDIOCONTROL => {
                    required.insert(descriptor.interface_number());
                    has_control = true;
                }
                USB_SUBCLASS_AUDIOSTREAMING
                    if descriptor.endpoint_descriptors().any(|endpoint| {
                        endpoint.direction() == Direction::Out
                            && endpoint.transfer_type() == TransferType::Isochronous
                    }) =>
                {
                    required.insert(descriptor.interface_number());
                    has_playback = true;
                }
                _ => {}
            }
        }
    }
    if !has_control || !has_playback {
        return Err(
            "USB configuration is missing audio control or playback interfaces".to_string(),
        );
    }
    let claimed: HashSet<_> = claimed_handle.claimed_interfaces.iter().copied().collect();
    // Refuse an incomplete base handoff before any topology GET_CUR/GET_RANGE
    // request can make libusb implicitly claim a control interface.
    if !required.is_subset(&claimed) || claimed.len() != claimed_handle.claimed_interfaces.len() {
        return Err(format!(
            "The host's USB claims {:?} omit required audio control/playback interfaces {:?} or contain duplicates",
            claimed_handle.claimed_interfaces, required,
        ));
    }
    let mut optional: HashSet<_> = structural_feedback_interfaces(
        config_descriptor,
        &claimed_handle.handle,
        resolve_device_speed(&claimed_handle.handle.device()),
        &claimed_handle.claimed_interfaces,
    )
    .into_iter()
    .collect();
    optional.extend(capture_inputs.iter().copied());
    if let Ok(identity) = claimed_handle.handle.device().device_descriptor() {
        optional.extend(routing_hw::scarlett_claim_interface(
            config_descriptor,
            identity.vendor_id(),
            identity.product_id(),
        ));
    }
    if !required.is_subset(&claimed)
        || !claimed.is_subset(&required.union(&optional).copied().collect())
        || claimed.len() != claimed_handle.claimed_interfaces.len()
    {
        return Err(format!(
            "The host's USB claims {:?} don't match required audio control/playback interfaces {:?}",
            claimed_handle.claimed_interfaces, required
        ));
    }
    Ok(())
}

/// Reject reserved transaction encodings before deriving an implicit capacity.
pub(super) fn valid_implicit_packet_capacity(raw: u16, speed: Speed) -> bool {
    if raw & 0xe000 != 0 || raw & 0x07ff == 0 {
        return false;
    }
    match speed {
        Speed::Full => raw & 0x1800 == 0,
        Speed::High => (raw >> 11) & 3 != 3,
        _ => false,
    }
}

pub(super) fn effective_iso_packet_bytes(raw_max_packet_size: u16, speed: Speed) -> usize {
    let base_bytes = (raw_max_packet_size & 0x07ff) as usize;
    let transactions = match speed {
        Speed::High | Speed::Super | Speed::SuperPlus => {
            1 + ((raw_max_packet_size >> 11) & 0x03) as usize
        }
        _ => 1,
    };
    base_bytes.saturating_mul(transactions)
}

pub(super) fn service_interval_micros(speed: Speed, interval: u8) -> u32 {
    let exponent = interval.saturating_sub(1).min(15) as u32;
    let multiplier = 1u32 << exponent;
    match speed {
        Speed::High | Speed::Super | Speed::SuperPlus => 125u32.saturating_mul(multiplier),
        _ => 1_000u32.saturating_mul(multiplier),
    }
}

pub(super) fn resolve_device_speed(device: &Device<Context>) -> Speed {
    let speed = device.speed();
    if speed != Speed::Unknown {
        return speed;
    }
    let bus = device.bus_number();
    let addr = device.address();
    if let Some(inferred) = infer_speed_from_sysfs(bus, addr) {
        dev_eprintln!(
            "[USB] libusb reported Unknown speed; inferred {:?} from sysfs (bus={}, addr={})",
            inferred,
            bus,
            addr
        );
        return inferred;
    }
    dev_eprintln!(
        "[USB] speed unknown: libusb=Unknown, sysfs lookup failed (bus={}, addr={})",
        bus,
        addr
    );
    Speed::Unknown
}

pub(super) fn infer_speed_from_sysfs(bus: u8, addr: u8) -> Option<Speed> {
    let entries = std::fs::read_dir("/sys/bus/usb/devices").ok()?;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        // Skip interface entries ("1-2:1.0") — only devices have speed/busnum/devnum
        if name.contains(':') {
            continue;
        }
        let dir = entry.path();
        let busnum = std::fs::read_to_string(dir.join("busnum"))
            .ok()
            .and_then(|s| s.trim().parse::<u32>().ok());
        let devnum = std::fs::read_to_string(dir.join("devnum"))
            .ok()
            .and_then(|s| s.trim().parse::<u32>().ok());
        if busnum == Some(bus as u32) && devnum == Some(addr as u32) {
            let raw = std::fs::read_to_string(dir.join("speed")).ok()?;
            return parse_sysfs_speed(raw.trim());
        }
    }
    None
}

pub(super) fn parse_sysfs_speed(raw: &str) -> Option<Speed> {
    // kernel writes Mbps as integer or "1.5" for low-speed
    let mbps: u32 = if let Ok(v) = raw.parse::<u32>() {
        v
    } else if let Some(dot) = raw.find('.') {
        raw[..dot].parse::<u32>().ok()?
    } else {
        return None;
    };
    match mbps {
        1 | 2 => Some(Speed::Low),
        12 => Some(Speed::Full),
        480 => Some(Speed::High),
        5000 => Some(Speed::Super),
        10000 | 20000 | 24000 => Some(Speed::SuperPlus),
        _ => None,
    }
}

pub(super) fn infer_high_speed_from_sample_rates(
    ranges: &[SamplingFrequencySubrange],
) -> Option<Speed> {
    let max_rate = ranges.iter().map(|r| r.max).max().unwrap_or(0);
    if max_rate > 48_000 {
        dev_eprintln!(
            "[USB] libusb speed Unknown; inferred high-speed from advertised sample rates (max={}>48000Hz)",
            max_rate
        );
        Some(Speed::High)
    } else {
        dev_eprintln!(
            "[USB] speed remains Unknown; max advertised rate={} (<=48kHz, no high-speed inference)",
            max_rate
        );
        None
    }
}

pub(super) fn required_max_packet_bytes(
    sample_rate: u32,
    service_interval_us: u32,
    bytes_per_frame: usize,
) -> usize {
    let max_frames = ((sample_rate as u64 * service_interval_us as u64) + 999_999) / 1_000_000;
    max_frames as usize * bytes_per_frame
}

pub(super) fn bytes_per_sample(bit_depth: u8) -> Option<usize> {
    match bit_depth {
        8 => Some(1),
        16 => Some(2),
        24 => Some(3),
        32 => Some(4),
        _ => None,
    }
}

const UAC2_RAW_DATA: u32 = 1 << 31;

// Preserve the shipped AP80 default and the existing explicit width/order
// overrides after the exact route/endpoint predicate has admitted this layout.
fn ap80_native_dsd_profile(policy: &crate::settings::Settings) -> Option<NativeDsdProfile> {
    let word_bytes = policy.native_dsd_policy(0xc502, 0x0072, 4)
        .map(|(width, _, _)| width).unwrap_or(4);
    if !matches!(word_bytes, 1 | 2 | 4) { return None; }
    let (big_endian, bit_reverse) = policy.native_dsd_order(0xc502, 0x0072, true);
    Some(NativeDsdProfile { word_bytes, big_endian, bit_reverse,
        origin: NativeDsdOrigin::DeviceCompatibility })
}

fn ap80_native_dsd_clock_rates(ranges: &[SamplingFrequencySubrange]) -> Vec<SamplingFrequencySubrange> {
    // DSD64, DSD128 and DSD256 in 32-bit words. The shared clock also
    // lists PCM rates, which are not native DSD modes on this player.
    [88_200, 176_400, 352_800]
        .into_iter()
        .filter(|rate| ranges.is_empty() || sampling_frequency_ranges_support_rate(ranges, *rate))
        .map(|rate| SamplingFrequencySubrange { min: rate, max: rate, res: 0 })
        .collect()
}

fn is_ap80_raw_dsd(
    usb_id: Option<(u16, u16)>,
    route: (u8, u8, u8),
    format: &StreamingInterfaceFormat,
) -> bool {
    usb_id == Some((0xc502, 0x0072))
        && route == (1, 1, 4)
        && format.uac2_formats == Some(UAC2_RAW_DATA)
        && format.channels == 2
        && format.subslot_size == 4
        && format.bit_resolution == 32
}

// This is the AP80 Pro-X layout confirmed by the ALSA parity run.
// Limit the measured clock subset to the proven device, route and OUT geometry.
// Native eligibility, word width and byte order remain profile-resolver policy.
fn matches_ap80_raw_dsd(
    usb_id: Option<(u16, u16)>,
    configuration: u8,
    descriptor: &rusb::InterfaceDescriptor,
    format: &StreamingInterfaceFormat,
) -> bool {
    if !is_ap80_raw_dsd(
        usb_id,
        (configuration, descriptor.interface_number(), descriptor.setting_number()),
        format,
    ) {
        return false;
    }
    let mut outputs = descriptor.endpoint_descriptors()
        .filter(|endpoint| endpoint.direction() == Direction::Out);
    let proven_out = outputs.next().is_some_and(|endpoint|
        endpoint.address() == 0x01
            && endpoint.transfer_type() == TransferType::Isochronous
            && endpoint.usage_type() == UsageType::Data
            && endpoint.interval() == 2
            && endpoint.max_packet_size() == 1024);
    proven_out && outputs.next().is_none()
}

pub(super) fn parse_android_streaming_interface_format(
    descriptor: &rusb::InterfaceDescriptor,
) -> Option<StreamingInterfaceFormat> {
    parse_streaming_extra(descriptor.protocol_code(), descriptor.extra())
}

pub(super) fn parse_streaming_extra(protocol: u8, bytes: &[u8]) -> Option<StreamingInterfaceFormat> {
    let mut channels = None;
    let mut format_tag = None;
    let mut uac2_formats = None;
    let mut subslot_size = None;
    let mut bit_resolution = None;
    let mut terminal_link = None;
    let mut sample_rate_ranges = Vec::new();

    let is_uac1 = match protocol {
        0x00 => true,
        0x20 => false,
        _ => return None,
    };

    for extra in DescriptorIter::new(bytes) {
        if extra.len() < 3 || extra[1] != USB_DT_CS_INTERFACE {
            continue;
        }

        match extra[2] {
            UAC2_AS_GENERAL => {
                if extra.len() >= 4 {
                    terminal_link = Some(extra[3]);
                }
                if is_uac1 {
                    if extra.len() >= 7 {
                        format_tag = Some(u16::from_le_bytes([extra[5], extra[6]]));
                    }
                } else if extra.len() >= 16 {
                    if extra[5] != UAC2_FORMAT_TYPE_I { return None; }
                    channels = Some(extra[10] as u16);
                    let bits = u32::from_le_bytes([extra[6], extra[7], extra[8], extra[9]]);
                    uac2_formats = Some(bits);
                    format_tag = Some(uac2_primary_format(bits));
                } else if extra.len() >= 7 {
                    // Historical private short AS dialect. It is not a standard
                    // UAC2 bitmap, and still needs explicit device/host policy.
                    let tag = u16::from_le_bytes([extra[5], extra[6]]);
                    format_tag = Some(if tag == 8 { FORMAT_TAG_DSD } else { tag });
                }
            }
            UAC2_FORMAT_TYPE if extra.get(3).copied() == Some(UAC2_FORMAT_TYPE_I) => {
                if is_uac1 {
                    if extra.len() >= 8 {
                        channels = Some(extra[4] as u16);
                        subslot_size = Some(extra[5]);
                        bit_resolution = Some(extra[6]);
                        let sam_freq_type = extra[7];
                        if sam_freq_type == 0 {
                            if extra.len() >= 14 {
                                let min = u32::from_le_bytes([extra[8], extra[9], extra[10], 0]);
                                let max = u32::from_le_bytes([extra[11], extra[12], extra[13], 0]);
                                if min > 0 && min <= max {
                                    sample_rate_ranges.push(SamplingFrequencySubrange {
                                        min,
                                        max,
                                        res: u32::from(min != max),
                                    });
                                }
                            }
                        } else {
                            for i in 0..sam_freq_type as usize {
                                let offset = 8 + i * 3;
                                if offset + 3 <= extra.len() {
                                    let rate = u32::from_le_bytes([
                                        extra[offset],
                                        extra[offset + 1],
                                        extra[offset + 2],
                                        0,
                                    ]);
                                    sample_rate_ranges.push(SamplingFrequencySubrange {
                                        min: rate,
                                        max: rate,
                                        res: 0,
                                    });
                                }
                            }
                        }
                    }
                } else if extra.len() >= 6 {
                    subslot_size = Some(extra[4]);
                    bit_resolution = Some(extra[5]);
                }
            }
            _ => {}
        }
    }

    let format_tag = format_tag?;

    let is_dsd_format = format_tag == FORMAT_TAG_DSD;

    let subslot_size = if is_dsd_format {
        subslot_size.unwrap_or(4)
    } else {
        subslot_size?
    };
    let bit_resolution = if is_dsd_format {
        bit_resolution.unwrap_or(1)
    } else {
        bit_resolution?
    };

    if ambiguous_type_i_pcm(uac2_formats, subslot_size, bit_resolution) { return None; }

    Some(StreamingInterfaceFormat {
        format_tag,
        uac2_formats,
        native_dsd: None,
        channels: channels.unwrap_or(2),
        subslot_size,
        bit_resolution,
        terminal_link,
        is_uac1,
        sample_rate_ranges,
    })
}

pub(super) fn format_tag_label(format_tag: u16) -> &'static str {
    match format_tag {
        FORMAT_TAG_PCM => "PCM",
        FORMAT_TAG_PCM8 => "PCM8",
        FORMAT_TAG_IEEE_FLOAT => "IEEE_FLOAT",
        FORMAT_TAG_DSD => "DSD",
        _ => "OTHER",
    }
}

pub(super) fn sync_type_label(sync_type: SyncType) -> &'static str {
    match sync_type {
        SyncType::Adaptive => "adaptive",
        SyncType::Asynchronous => "asynchronous",
        SyncType::Synchronous => "synchronous",
        SyncType::NoSync => "none",
    }
}

pub(super) fn usage_type_label(usage_type: UsageType) -> &'static str {
    match usage_type {
        UsageType::Data => "data",
        UsageType::Feedback => "feedback",
        UsageType::FeedbackData => "feedback-data",
        UsageType::Reserved => "reserved",
    }
}

pub(super) fn transfer_type_label(transfer_type: TransferType) -> &'static str {
    match transfer_type {
        TransferType::Control => "control",
        TransferType::Isochronous => "isochronous",
        TransferType::Bulk => "bulk",
        TransferType::Interrupt => "interrupt",
    }
}

pub(super) fn validate_transport_against_playback_format(
    candidate: &IsoStreamCandidate,
    playback_format: PlaybackFormat,
) -> Result<(), String> {
    let is_native_dsd = playback_format.dsd_transport == DsdTransportMode::Native;
    let required_format = if is_native_dsd { FORMAT_TAG_DSD } else { playback_format.pcm_encoding.format_tag() };
    if if is_native_dsd { candidate.native_dsd.is_none() }
        else { candidate.format_tag != required_format } {
        return Err(format!(
            "USB direct requires {} transport, got {}",
            format_tag_label(required_format),
            format_tag_label(candidate.format_tag),
        ));
    }
    if candidate.channels != playback_format.channels {
        return Err(format!(
            "USB direct requires {} channels, got {}",
            playback_format.channels, candidate.channels
        ));
    }
    if !is_native_dsd {
        if candidate.bit_resolution != playback_format.bit_depth {
            return Err(format!(
                "USB direct requires {}-bit transport, got {}",
                playback_format.bit_depth, candidate.bit_resolution
            ));
        }
        let Some(min_subslot_size) = bytes_per_sample(playback_format.bit_depth) else {
            return Err(format!(
                "USB direct does not support {}-bit sample packing",
                playback_format.bit_depth
            ));
        };
        if candidate.subslot_size < min_subslot_size as u8 {
            return Err(format!(
                "USB direct requires at least {} bytes per subslot for {}-bit transport, got {}",
                min_subslot_size, playback_format.bit_depth, candidate.subslot_size
            ));
        }
    }
    Ok(())
}

pub(super) fn prepare_direct_usb_libusb() {
    DISCOVERY_DISABLED.call_once(|| {
        if let Err(error) = disable_device_discovery() {
            dev_eprintln!(
                "USB direct: failed to disable libusb device discovery: {}",
                error
            );
        }
    });
}

pub(super) fn current_lock_requested_for_fd(device_fd: RawFd) -> bool {
    SESSION_STATE
        .lock()
        .as_ref()
        .map(|state| state.lock_requested && state.device.fd == device_fd)
        .unwrap_or(false)
}

pub(super) fn alt_format(format_tag: u16) -> AltFormat {
    match format_tag {
        FORMAT_TAG_PCM => AltFormat::Pcm,
        FORMAT_TAG_PCM8 => AltFormat::PcmU8,
        FORMAT_TAG_IEEE_FLOAT => AltFormat::PcmFloat32,
        FORMAT_TAG_DSD => AltFormat::Dsd,
        _ => AltFormat::Other,
    }
}

pub(super) fn sync_kind(sync_type: SyncType) -> SyncKind {
    match sync_type {
        SyncType::Asynchronous => SyncKind::Asynchronous,
        SyncType::Adaptive => SyncKind::Adaptive,
        SyncType::Synchronous => SyncKind::Synchronous,
        SyncType::NoSync => SyncKind::None,
    }
}

pub(super) fn endpoint_usage(usage_type: UsageType) -> EndpointUsage {
    match usage_type {
        UsageType::Data => EndpointUsage::Data,
        UsageType::Feedback => EndpointUsage::Feedback,
        UsageType::FeedbackData => EndpointUsage::FeedbackData,
        UsageType::Reserved => EndpointUsage::Reserved,
    }
}

pub(super) fn link_speed_of(speed: Speed) -> LinkSpeed {
    match speed {
        Speed::Low => LinkSpeed::Low,
        Speed::Full => LinkSpeed::Full,
        Speed::High => LinkSpeed::High,
        Speed::Super => LinkSpeed::Super,
        Speed::SuperPlus => LinkSpeed::SuperPlus,
        _ => LinkSpeed::Unknown,
    }
}

pub(super) fn transport_container_preference_rank(
    candidate: &IsoStreamCandidate,
    playback_format: PlaybackFormat,
    prefer_padded_24bit_transport: bool,
) -> u8 {
    if !prefer_padded_24bit_transport || playback_format.bit_depth != 24 {
        return 0;
    }

    match (candidate.bit_resolution, candidate.subslot_size) {
        (24, 4) => 0,
        (24, 3) => 1,
        _ => 2,
    }
}

/// The first PCM playback stream: its interface number, terminal link and
/// channel count. At open this picks the terminal whose Feature Unit the volume
/// control is looked up through.
pub(super) fn first_playback_stream(device: &Device<Context>) -> Option<(u8, u8, u16)> {
    let config = device.active_config_descriptor().ok()?;
    let mut fallback = None;
    for interface in config.interfaces() {
        for descriptor in interface.descriptors() {
            if descriptor.class_code() != USB_CLASS_AUDIO
                || descriptor.sub_class_code() != USB_SUBCLASS_AUDIOSTREAMING
                || descriptor.setting_number() == 0
            {
                continue;
            }
            let has_iso_out = descriptor.endpoint_descriptors().any(|endpoint| {
                endpoint.direction() == Direction::Out
                    && endpoint.transfer_type() == TransferType::Isochronous
            });
            if !has_iso_out {
                continue;
            }
            let Some(format) = parse_android_streaming_interface_format(&descriptor) else {
                continue;
            };
            let Some(terminal_link) = format.terminal_link else {
                continue;
            };
            let found = (
                descriptor.interface_number(),
                terminal_link,
                format.channels,
            );
            if format.format_tag == FORMAT_TAG_PCM {
                return Some(found);
            }
            fallback.get_or_insert(found);
        }
    }
    fallback
}

#[cfg(test)]
mod implicit_discovery_tests {
    use super::*;
    #[test]
    fn uac1_continuous_rates_preserve_interior_hz_and_endpoint_capacity() {
        let parse = |min: u32, max: u32| {
            let min = min.to_le_bytes();
            let max = max.to_le_bytes();
            parse_streaming_extra(0, &[
                7, 0x24, 1, 1, 0, 1, 0,
                14, 0x24, 2, 1, 2, 3, 24, 0,
                min[0], min[1], min[2], max[0], max[1], max[2],
            ]).unwrap().sample_rate_ranges
        };
        let ranges = parse(32_001, 96_003);
        assert_eq!((ranges[0].min, ranges[0].max, ranges[0].res), (32_001, 96_003, 1));
        for rate in [32_001, 44_117, 48_123, 96_003] {
            assert!(sampling_frequency_ranges_support_rate(&ranges, rate));
        }
        for rate in [0, 32_000, 96_004] {
            assert!(!sampling_frequency_ranges_support_rate(&ranges, rate));
        }
        let (clipped, _) = capacity_limited_rates(&ranges, 294, 1000, 6);
        assert!(sampling_frequency_ranges_support_rate(&clipped, 48_123));
        assert!(sampling_frequency_ranges_support_rate(&clipped, 49_000));
        assert!(!sampling_frequency_ranges_support_rate(&clipped, 49_001));
        // The public capability representation retains the integer-Hz step.
        let range = RateRange { min_hz: clipped[0].min, max_hz: clipped[0].max, step_hz: clipped[0].res };
        assert_eq!((range.min_hz, range.max_hz, range.step_hz), (32_001, 49_000, 1));
        let singleton = parse(44_117, 44_117);
        assert_eq!(singleton[0].res, 0);
        assert!(sampling_frequency_ranges_support_rate(&singleton, 44_117));
        assert!(!sampling_frequency_ranges_support_rate(&singleton, 44_118));
        assert!(parse(0, 48_000).is_empty());
        assert!(parse(48_000, 44_100).is_empty());
    }

    #[test]
    fn implicit_clock_proof_compares_exact_ratios_and_shared_raw_hops() {
        use crate::clock_resolution::{ClockHop, ClockHopKind, ClockResolution};
        let choice = |factors: &[(u8, u16, u16)]| ClockChoice {
            control_interface: 0,
            source_id: 10,
            resolution: ClockResolution::Resolved,
            path: factors.iter().map(|&(id, numerator, denominator)| ClockHop {
                id, kind: ClockHopKind::RateMultiplier { numerator, denominator },
            }).collect(),
        };
        let output = choice(&[(3, 3, 2)]);
        let equivalent = choice(&[(4, 6, 4)]);
        assert!(implicit_clocks_compatible(&output, &equivalent));
        assert!(!implicit_clocks_compatible(&output, &choice(&[(4, 1, 1)])));
        // Equal effective ratios cannot conceal disagreement on shared controls.
        assert!(!implicit_clocks_compatible(&output, &choice(&[(3, 6, 4)])));
        let identity = choice(&[(4, 2, 2)]);
        assert!(uac2_choice_has_multiplier(Some(&identity)));
        assert!(implicit_clocks_compatible(&identity, &choice(&[])));
        let mut unread = identity;
        unread.path[0].kind = ClockHopKind::Multiplier;
        assert!(!implicit_clocks_compatible(&unread, &unread));
        assert!(!implicit_clocks_compatible(&choice(&[(4, 0, 1)]), &choice(&[])));
        let mut other_source = equivalent;
        other_source.source_id = 11;
        assert!(!implicit_clocks_compatible(&output, &other_source));
    }

    fn source(interface: u8, alt: u8, address: u8, capacity: usize) -> ImplicitFeedbackSource {
        ImplicitFeedbackSource {
            interface_number: interface,
            alt_setting: alt,
            address,
            bytes_per_frame: 6,
            service_interval_us: 1000,
            max_packet_bytes: capacity,
        }
    }
    #[test]
    fn implicit_pcm_requires_standard_bitmap_and_valid_geometry() {
        let mut format = StreamingInterfaceFormat {
            format_tag: FORMAT_TAG_PCM,
            uac2_formats: Some(1),
            native_dsd: None,
            channels: 2,
            subslot_size: 3,
            bit_resolution: 24,
            terminal_link: Some(1),
            is_uac1: false,
            sample_rate_ranges: Vec::new(),
        };
        assert!(standard_implicit_pcm_format(&format));
        format.uac2_formats = None;
        assert!(!standard_implicit_pcm_format(&format));
        format.uac2_formats = Some(2);
        assert!(!standard_implicit_pcm_format(&format));
        format.uac2_formats = Some(1);
        format.is_uac1 = true;
        assert!(!standard_implicit_pcm_format(&format));
        format.is_uac1 = false;
        format.channels = 0;
        assert!(!standard_implicit_pcm_format(&format));
        format.channels = 2;
        format.bit_resolution = 25;
        assert!(!standard_implicit_pcm_format(&format));
        format.bit_resolution = 0;
        assert!(!standard_implicit_pcm_format(&format));
    }

    #[test]
    fn fixed_profiles_require_standard_wire_proof_and_rate_without_restricting_generic() {
        let fixed = PlaybackImplicitProfile::Fixed { address: 0x84, interface: 3 };
        assert!(implicit_profile_proven(fixed, true, true, true));
        assert!(!implicit_profile_proven(fixed, false, true, true));
        assert!(!implicit_profile_proven(fixed, true, false, true));
        assert!(!implicit_profile_proven(fixed, true, true, false));
        assert!(implicit_profile_proven(PlaybackImplicitProfile::Generic, false, false, false));
        assert!(!implicit_profile_proven(PlaybackImplicitProfile::Skip, true, true, true));
    }

    #[test]
    fn shared_release_does_not_activate_fixed_profiles_or_exclusions() {
        for (vendor, product, address, interface) in [
            (0x0763, 0x2030, 0x81, 3), (0x0763, 0x2031, 0x81, 3),
            (0x0763, 0x2080, 0x81, 2), (0x0763, 0x2081, 0x81, 2),
            (0x2466, 0x8010, 0x81, 2), (0x31e9, 0x0001, 0x81, 2),
            (0x31e9, 0x0002, 0x81, 2), (0x0499, 0x172f, 0x81, 2),
            (0x0d9a, 0x00df, 0x81, 2), (0x19f7, 0x000a, 0x84, 3),
            (0x22f0, 0x0006, 0x81, 3), (0x1686, 0xf029, 0x82, 2),
            (0x2466, 0x8003, 0x86, 2), (0x0499, 0x172a, 0x86, 2),
        ] {
            let _deferred_pipe = (address, interface);
            assert_eq!(playback_implicit_profile(vendor, product), PlaybackImplicitProfile::Generic);
        }
        for identity in [(0x0763, 0x9999), (0x9999, 0x2030),
            (0x20b1, 0x200a), (0x07fd, 0x0005), (0x07fd, 0x0004), (0x20b1, 0x2009), (0, 0)] {
            assert_eq!(playback_implicit_profile(identity.0, identity.1),
                PlaybackImplicitProfile::Generic);
        }
    }

    #[test]
    fn fixed_profile_uses_only_exact_pipe_and_refuses_conflicting_sync_address() {
        let profile = PlaybackImplicitProfile::Fixed { address: 0x84, interface: 3 };
        let fixed = source(3, 7, 0x84, 288);
        // Fixed-source alternates need not match the sink's alternate.
        assert_eq!(implicit_source_rank(profile, fixed, false, 1, 1, 0), Some(0));
        assert_eq!(implicit_source_rank(profile, fixed, false, 1, 1, 0x84), Some(0));
        assert_eq!(implicit_source_rank(profile, fixed, true, 1, 1, 0x81), None);
        // A marked or adjacent source cannot override the product's pipe.
        assert_eq!(implicit_source_rank(profile, source(2, 1, 0x81, 288), true, 1, 1, 0), None);
        assert_eq!(implicit_source_rank(profile, source(2, 1, 0x84, 288), true, 1, 1, 0), None);
        assert_eq!(implicit_source_rank(profile, source(3, 1, 0x81, 288), true, 1, 1, 0), None);
        assert_eq!(implicit_source_rank(PlaybackImplicitProfile::Generic, fixed, false, 1, 1, 0), None);
    }

    #[test]
    fn standard_audio_skip_profiles_veto_linked_and_marked_implicit_sources() {
        for identity in [(0x07fd, 0x0004), (0x20b1, 0x2009)] {
            let _deferred_identity = identity;
            let profile = PlaybackImplicitProfile::Skip;
            for linked in [0, 0x81] {
                assert_eq!(implicit_source_rank(profile, source(2, 1, 0x81, 288), true, 1, 1, linked), None);
            }
        }
    }

    #[test]
    fn prioritizes_declaration_then_marked_source() {
        let plain = source(2, 1, 0x81, 288);
        let marked = source(3, 1, 0x82, 288);
        assert_eq!(
            choose_implicit_source(vec![(plain, 2), (marked, 1)], 1),
            Some(marked)
        );
        assert_eq!(
            choose_implicit_source(vec![(plain, 0), (marked, 1)], 1),
            Some(plain)
        );
    }
    #[test]
    fn alternate_preference_keeps_one_pipe_and_rejects_distinct_ambiguity() {
        let matching_alt = source(2, 2, 0x81, 400);
        let smaller_alt = source(2, 1, 0x81, 288);
        assert_eq!(
            choose_implicit_source(vec![(smaller_alt, 1), (matching_alt, 1)], 2),
            Some(matching_alt)
        );
        assert_eq!(
            choose_implicit_source(vec![(smaller_alt, 1), (matching_alt, 1)], 3),
            Some(smaller_alt)
        );
        assert_eq!(
            choose_implicit_source(vec![(smaller_alt, 1), (source(3, 1, 0x82, 288), 1)], 1),
            None
        );
    }
    #[test]
    fn ranked_exact_sources_keep_pipe_alternates_and_best_rank_ambiguity() {
        let preferred = source(2, 2, 0x81, 400);
        let alternate = source(2, 1, 0x81, 288);
        let lower_rank = source(2, 3, 0x81, 400);
        let other_pipe = source(3, 1, 0x82, 288);
        let matching = vec![(alternate, 1), (other_pipe, 2), (lower_rank, 2), (preferred, 1)];
        assert_eq!(ranked_implicit_sources(matching.clone(), 2, true),
            vec![preferred, alternate, lower_rank]);
        assert_eq!(ranked_implicit_sources(matching.clone(), 2, false), vec![preferred]);
        assert_eq!(choose_implicit_source(matching, 2), Some(preferred));
        assert!(ranked_implicit_sources(vec![(preferred, 1), (other_pipe, 1)], 2, true).is_empty());
        assert!(ranked_implicit_sources(Vec::new(), 2, true).is_empty());
    }

    #[test]
    fn implicit_capacity_rejects_reserved_and_wrong_speed_encodings() {
        assert!(valid_implicit_packet_capacity(288, Speed::Full));
        assert!(valid_implicit_packet_capacity(288 | (2 << 11), Speed::High));
        assert!(!valid_implicit_packet_capacity(
            288 | (1 << 11),
            Speed::Full
        ));
        assert!(!valid_implicit_packet_capacity(
            288 | (3 << 11),
            Speed::High
        ));
        for reserved in [0x2000, 0x4000, 0x8000] {
            assert!(!valid_implicit_packet_capacity(288 | reserved, Speed::High));
        }
        assert!(!valid_implicit_packet_capacity(0, Speed::High));
        assert!(!valid_implicit_packet_capacity(288, Speed::Unknown));
    }
    #[test]
    fn audio_function_membership_requires_valid_uac2_iad() {
        let raw = [8, 0x0b, 4, 3, 1, 0, 0x20, 0];
        assert_eq!(audio_functions(&raw), vec![(4, vec![4, 5, 6])]);
        assert!(audio_functions(&raw[..7]).is_empty());
        assert!(audio_functions(&[8, 0x0b, 255, 2, 1, 0, 0x20, 0]).is_empty());
        assert!(audio_functions(&[8, 0x0b, 4, 3, 1, 0, 0, 0]).is_empty());
    }
}

#[cfg(test)]
mod duplex_claim_tests {
    use super::validate_duplex_capture_claims;

    #[test]
    fn duplex_capture_proof_requires_input_and_owning_control() {
        assert!(validate_duplex_capture_claims(&[0, 1, 2, 3], &[0, 2], &[3]).is_ok());
        assert!(validate_duplex_capture_claims(&[0, 1, 3], &[0, 2], &[3]).is_err());
        assert!(validate_duplex_capture_claims(&[0, 1, 2], &[0, 2], &[3]).is_err());
        assert!(validate_duplex_capture_claims(&[0, 1], &[0], &[]).is_err());
    }
}

#[cfg(test)]
mod standard_capacity_tests {
    use super::*;

    #[test]
    fn superspeed_endpoint_addresses_reject_default_endpoint_and_reserved_bits() {
        for address in [0, 0x80, 0x11, 0x91, 0x71, 0xf1] {
            assert!(!valid_standard_endpoint_address(address));
        }
        for address in [1, 15, 0x81, 0x8f] {
            assert!(valid_standard_endpoint_address(address));
        }
    }

    #[test]
    fn superspeed_q32_reports_require_standard_feedback_and_async_association() {
        let valid = |transfer, usage, feedback_sync, sink_sync, associated| {
            standard_ss_feedback_shape(8, transfer, usage, feedback_sync, sink_sync, associated, (0x01, 0x81))
        };
        assert!(valid(TransferType::Isochronous, UsageType::Feedback,
            SyncType::NoSync, SyncType::Asynchronous, true));
        assert!(!valid(TransferType::Interrupt, UsageType::Feedback,
            SyncType::NoSync, SyncType::Asynchronous, true));
        for usage in [UsageType::Data, UsageType::FeedbackData, UsageType::Reserved] {
            assert!(!valid(TransferType::Isochronous, usage,
                SyncType::NoSync, SyncType::Asynchronous, true));
        }
        assert!(!valid(TransferType::Isochronous, UsageType::Feedback,
            SyncType::Asynchronous, SyncType::Asynchronous, true));
        assert!(!valid(TransferType::Isochronous, UsageType::Feedback,
            SyncType::NoSync, SyncType::Synchronous, true));
        assert!(!valid(TransferType::Isochronous, UsageType::Feedback,
            SyncType::NoSync, SyncType::Asynchronous, false));
        assert!(standard_ss_feedback_shape(4, TransferType::Interrupt, UsageType::Data,
            SyncType::NoSync, SyncType::Asynchronous, true, (0x01, 0x82)));
        // A declared bSynchAddress cannot override standard endpoint-number association.
        assert!(!standard_ss_feedback_shape(8, TransferType::Isochronous, UsageType::Feedback,
            SyncType::NoSync, SyncType::Asynchronous, true, (0x01, 0x82)));
    }

    #[test]
    fn companion_capacity_clips_word_rates_without_dsd_bit_conversion() {
        let ranges = [SamplingFrequencySubrange { min: 44_100, max: 768_000, res: 1 }];
        // Stereo 32-bit words: 192 bytes each 125us carries at most 192k words/s.
        let (clipped, rates) = capacity_limited_rates(&ranges, 192, 125, 8);
        assert_eq!(clipped[0].min, 44_100);
        assert_eq!(clipped[0].max, 192_000);
        assert!(rates.contains(&192_000));
        assert!(rates.iter().all(|rate| required_max_packet_bytes(*rate, 125, 8) <= 192));
    }

    #[test]
    fn native_word_width_is_clipped_independently_of_descriptor_pcm_width() {
        let ranges = [SamplingFrequencySubrange { min: 192_000, max: 384_000, res: 192_000 }];
        assert!(capacity_limited_rates(&ranges, 96, 125, 8).0.is_empty());
        let (native, rates) = capacity_limited_rates(&ranges, 96, 125, 2);
        assert_eq!(native[0].max, 384_000);
        assert!(rates.contains(&192_000));
        assert!(rates.contains(&384_000));
        assert!(capacity_limited_rates(&ranges, 24, 125, 2).0.is_empty());
    }

    #[test]
    fn companion_capacity_preserves_discrete_rate_alignment_and_rejects_too_small() {
        let ranges = [SamplingFrequencySubrange { min: 44_100, max: 176_400, res: 44_100 }];
        let (clipped, _) = capacity_limited_rates(&ranges, 96, 125, 8);
        assert_eq!(clipped[0].max, 88_200);
        assert_eq!(clipped[0].res, 44_100);
        assert!(capacity_limited_rates(&ranges, 1, 125, 8).0.is_empty());
        assert!(capacity_limited_rates(&ranges, 96, 125, 0).0.is_empty());
    }
    #[test]
    fn capacity_limits_native_words_at_full_speed_and_rejects_malformed_ranges() {
        let ranges = [SamplingFrequencySubrange { min: 88_200, max: 352_800, res: 88_200 }];
        // Full-speed 1023-byte packets admit these stereo byte-word rates,
        // while stereo 32-bit words fit only the lowest cadence.
        let (_, rates) = capacity_limited_rates(&ranges, 1023, 1000, 2);
        assert!(rates.contains(&352_800));
        let (_, rates) = capacity_limited_rates(&ranges, 1023, 1000, 8);
        assert_eq!(rates, vec![88_200]);
        for malformed in [
            SamplingFrequencySubrange { min: 0, max: 0, res: 0 },
            SamplingFrequencySubrange { min: 44_100, max: 192_000, res: 0 },
            SamplingFrequencySubrange { min: 192_000, max: 44_100, res: 1 },
        ] {
            let (ranges, rates) = capacity_limited_rates(&[malformed], 1023, 1000, 2);
            assert!(ranges.is_empty());
            assert!(rates.is_empty());
        }
    }

}

#[cfg(test)]
mod ap80_raw_data_tests {
    use super::*;

    #[test]
    fn preserves_uac2_raw_data_without_reclassifying_other_devices_or_pcm() {
        let mut extra = [16, 0x24, 1, 2, 0, 1, 0, 0, 0, 0x80, 2, 3, 0, 0, 0, 0,
                         6, 0x24, 2, 1, 4, 32];
        let raw = parse_streaming_extra(0x20, &extra).unwrap();
        assert_eq!(raw.uac2_formats, Some(0x8000_0000));
        assert_eq!(alt_format(raw.format_tag), AltFormat::Other);
        assert!(is_ap80_raw_dsd(Some((0xc502, 0x0072)), (1, 1, 4), &raw));
        assert!(!is_ap80_raw_dsd(Some((0xc502, 0x0073)), (1, 1, 4), &raw));
        assert!(!is_ap80_raw_dsd(Some((0xc502, 0x0072)), (1, 1, 3), &raw));
        extra[6] = 1;
        extra[9] = 0;
        let pcm = parse_streaming_extra(0x20, &extra).unwrap();
        assert_eq!(alt_format(pcm.format_tag), AltFormat::Pcm);
        assert!(!is_ap80_raw_dsd(Some((0xc502, 0x0072)), (1, 1, 4), &pcm));
    }

    #[test]
    fn ap80_preserves_shipped_big_endian_and_explicit_host_overrides() {
        use crate::{config::DacConfig, settings::Settings};
        let mut policy = Settings::default();
        assert_eq!(ap80_native_dsd_profile(&policy).unwrap().signature(), (4, true, false));
        policy.config = DacConfig { dsd_subslot: Some(2), ..DacConfig::default() };
        assert_eq!(ap80_native_dsd_profile(&policy).unwrap().signature(), (2, true, false));
        policy.config.dsd_big_endian = Some(false);
        assert_eq!(ap80_native_dsd_profile(&policy).unwrap().signature(), (2, false, false));
        policy.config.dsd_subslot = Some(3);
        assert!(ap80_native_dsd_profile(&policy).is_none());
    }

    #[test]
    fn ap80_advertises_verified_native_modes_from_the_shared_clock() {
        let clock = [44_100, 88_200, 176_400, 352_800, 384_000].map(|rate|
            SamplingFrequencySubrange { min: rate, max: rate, res: 0 });
        let rates = ap80_native_dsd_clock_rates(&clock);
        assert_eq!(rates.iter().map(|r| r.min).collect::<Vec<_>>(), [88_200, 176_400, 352_800]);
    }
}
