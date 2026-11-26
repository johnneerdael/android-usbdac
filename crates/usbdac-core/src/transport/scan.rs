use super::*;
use crate::capabilities::{
    AltFormat, AltSetting, EndpointUsage, LinkSpeed, RateRange, SyncKind, UacVersion,
};

/// Read-only preflight: base AudioControl/playback claims are already held.
/// No alternate is activated and no capture transfer is allocated or submitted.
pub(super) fn preflight_implicit_feedback_interfaces(
    device: &UsbDevice,
) -> Result<(u8, Vec<u8>), String> {
    let claimed = wrap_android_usb_handle(device)?;
    let usb = claimed.handle.device();
    let config = usb.active_config_descriptor().map_err(|e| e.to_string())?;
    validate_android_usb_claims(&claimed, &config)?;
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

    let inventory = implicit_source_inventory(config, handle, speed);
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
        if format.is_uac1 || format.format_tag != FORMAT_TAG_PCM {
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
                || !valid_implicit_packet_capacity(ep.max_packet_size(), speed)
                || find_feedback_endpoint(&out, ep.synch_address(), ep.sync_type(), speed).is_some()
                || explicit_feedback_ambiguous(&out, ep.synch_address(), ep.sync_type())
            {
                continue;
            }
            let interval = service_interval_micros(speed, ep.interval());
            let stride = usize::from(format.channels) * usize::from(format.subslot_size);
            if stride == 0 {
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
                        || effective_iso_packet_bytes(ep.max_packet_size(), speed)
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
        && format.uac2_formats.is_some_and(|bits| bits & 1 != 0)
        && format.format_tag == FORMAT_TAG_PCM
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
) -> Vec<ImplicitSourceCandidate> {
    if !matches!(speed, Speed::Full | Speed::High) {
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
            let Some(clock) = strict_implicit_clock(config, handle, *control, format.terminal_link)
            else {
                continue;
            };
            for ep in alt.endpoint_descriptors() {
                if ep.direction() != Direction::In
                    || ep.transfer_type() != TransferType::Isochronous
                    || ep.sync_type() != SyncType::Asynchronous
                    || !(1..=16).contains(&ep.interval())
                    || !valid_implicit_packet_capacity(ep.max_packet_size(), speed)
                    || !matches!(ep.usage_type(), UsageType::Data | UsageType::FeedbackData)
                {
                    continue;
                }
                let capacity = effective_iso_packet_bytes(ep.max_packet_size(), speed);
                if capacity < stride {
                    continue;
                }
                result.push(ImplicitSourceCandidate {
                    source: ImplicitFeedbackSource {
                        interface_number: alt.interface_number(),
                        alt_setting: alt.setting_number(),
                        address: ep.address(),
                        bytes_per_frame: stride,
                        service_interval_us: service_interval_micros(speed, ep.interval()),
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

/// Exact playback rules from Linux sound/usb/implicit.c's fixed endpoint
/// table. These identify a pipe, never waive standard UAC2/function/clock proof.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PlaybackImplicitProfile {
    Generic,
    Fixed { address: u8, interface: u8 },
    Skip,
}

fn playback_implicit_profile(vendor: u16, product: u16) -> PlaybackImplicitProfile {
    use PlaybackImplicitProfile::{Fixed, Generic, Skip};
    match (vendor, product) {
        (0x0763, 0x2030 | 0x2031) => Fixed { address: 0x81, interface: 3 },
        (0x0763, 0x2080 | 0x2081)
        | (0x2466, 0x8010)
        | (0x31e9, 0x0001 | 0x0002)
        | (0x0499, 0x172f)
        | (0x0d9a, 0x00df) => Fixed { address: 0x81, interface: 2 },
        (0x19f7, 0x000a) => Fixed { address: 0x84, interface: 3 },
        (0x22f0, 0x0006) => Fixed { address: 0x81, interface: 3 },
        (0x1686, 0xf029) => Fixed { address: 0x82, interface: 2 },
        (0x2466, 0x8003) | (0x0499, 0x172a) => Fixed { address: 0x86, interface: 2 },
        // MicroBook IIc's standard-audio entry in implicit.c; the vendor
        // protocol entry is deliberately outside this UAC2-only resolver.
        // DIYINHK: quirks.c QUIRK_FLAG_SKIP_IMPLICIT_FB, exact product only.
        (0x07fd, 0x0004) | (0x20b1, 0x2009) => Skip,
        _ => Generic,
    }
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
    standard_sink: bool,
    standard_source: bool,
    sample_rate: Option<u32>,
) -> bool {
    match profile {
        PlaybackImplicitProfile::Skip => false,
        PlaybackImplicitProfile::Generic => true,
        PlaybackImplicitProfile::Fixed { .. } => {
            standard_sink && standard_source && sample_rate.is_some()
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
    // The actual libusb descriptor is authoritative, not Android handoff IDs.
    // Fail closed when identity cannot be read: otherwise a skip/fixed product
    // could accidentally enter the generic path.
    let identity = handle.device().device_descriptor().ok()?;
    let profile = playback_implicit_profile(identity.vendor_id(), identity.product_id());
    let mut matching = Vec::new();
    for source in inventory {
        if !implicit_profile_proven(profile, standard_sink, source.standard_pcm, sample_rate) {
            continue;
        }
        let Some(rank) = implicit_source_rank(
            profile, source.source, source.marked, out_interface, out_alt, linked_address,
        ) else {
            continue;
        };
        if !source.function_interfaces.contains(&out_interface)
            || (source.source.interface_number == out_interface
                && source.source.alt_setting != out_alt)
            || source.source.service_interval_us != service_interval_us
        {
            continue;
        }
        let Some(out_clock) =
            strict_implicit_clock(config, handle, source.control_interface, out_terminal)
        else {
            continue;
        };
        if !implicit_clocks_compatible(&out_clock, &source.clock) {
            continue;
        }
        if let Some(sample_rate) = sample_rate {
            let rates = if source.rates.is_empty() {
                get_wire_frequency_ranges(
                    handle,
                    AudioControlClock { interface_number: source.control_interface, clock_id: source.clock.source_id },
                    Some(&source.clock),
                )
                .unwrap_or_default()
            } else {
                source.rates.clone()
            };
            if rates.is_empty()
                || !sampling_frequency_ranges_support_rate(&rates, sample_rate)
                || source.source.max_packet_bytes
                    < required_max_packet_bytes(
                        sample_rate,
                        service_interval_us,
                        source.source.bytes_per_frame,
                    )
            {
                continue;
            }
        }
        matching.push((source.source, rank));
    }
    choose_implicit_source(matching, out_alt)
}

pub(super) fn uac2_choice_has_multiplier(choice: Option<&ClockChoice>) -> bool {
    use crate::clock_resolution::ClockHopKind;
    choice.is_some_and(|choice| choice.path.iter().any(|hop|
        matches!(hop.kind, ClockHopKind::Multiplier | ClockHopKind::RateMultiplier { .. })))
}

/// Implicit feedback shares a wire cadence, not just a source entity ID.
fn implicit_clocks_compatible(output: &ClockChoice, input: &ClockChoice) -> bool {
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

fn choose_implicit_source(
    mut matching: Vec<(ImplicitFeedbackSource, u8)>,
    out_alt: u8,
) -> Option<ImplicitFeedbackSource> {
    let rank = matching.iter().map(|(_, rank)| *rank).min()?;
    matching.retain(|(_, r)| *r == rank);
    matching.sort_by_key(|(s, _)| (s.alt_setting != out_alt, s.max_packet_bytes, s.alt_setting));
    let first = matching.first()?.0;
    if matching
        .iter()
        .any(|(s, _)| s.interface_number != first.interface_number || s.address != first.address)
    {
        return None;
    }
    Some(first)
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
        let inventory = implicit_source_inventory(&config, handle, candidate.usb_speed);
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
    validate_duplex_capture_claims(&device.claimed_interfaces, &controls, &inputs)?;
    let claimed = wrap_android_usb_handle(device)?;
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

    let implicit_sources = if settings().disable_feedback() {
        Vec::new()
    } else {
        implicit_source_inventory(&config_descriptor, handle, speed)
    };
    let mut alt_settings = Vec::new();
    let mut supported_sample_rates = Vec::new();
    let mut stream_rate_cache =
        std::collections::HashMap::<(u8, Option<u8>), Vec<SamplingFrequencySubrange>>::new();
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
            stream_format.native_dsd = resolve_native_dsd(device, &config_descriptor, &descriptor, &stream_format);
            let playback_designs_dop = resolve_playback_designs_dop(device, &config_descriptor, &descriptor, &stream_format);
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

                let service_interval_us = service_interval_micros(speed, endpoint.interval());
                let max_packet_bytes =
                    effective_iso_packet_bytes(endpoint.max_packet_size(), speed);

                if explicit_feedback_ambiguous(
                    &descriptor,
                    endpoint.synch_address(),
                    endpoint.sync_type(),
                ) {
                    continue;
                }
                let discovered_feedback = find_feedback_endpoint(
                    &descriptor,
                    endpoint.synch_address(),
                    endpoint.sync_type(),
                    speed,
                );
                let feedback_address = discovered_feedback.map(|feedback| feedback.address);
                let implicit = if discovered_feedback.is_none()
                    && endpoint.sync_type() == SyncType::Asynchronous
                    && !stream_format.is_uac1
                    && stream_format.format_tag == FORMAT_TAG_PCM
                {
                    sample_rates.iter().find_map(|rate| {
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
                                && valid_implicit_packet_capacity(endpoint.max_packet_size(), speed)
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
                        settings().allow_unsynchronized_async(),
                    )
                };
                supported_sample_rates.extend(sample_rates.iter().copied());
                alt_settings.push(AltSetting {
                    interface_number: descriptor.interface_number(),
                    alt_setting: descriptor.setting_number(),
                    endpoint_address: endpoint.address(),
                    format: if stream_format.format_tag == FORMAT_TAG_DSD { AltFormat::Other } else { alt_format(stream_format.format_tag) },
                    channels: stream_format.channels,
                    subslot_bytes: stream_format.subslot_size,
                    valid_bits: stream_format.bit_resolution,
                    rate_ranges: sample_rate_ranges
                        .iter()
                        .map(|range| RateRange {
                            min_hz: range.min,
                            max_hz: range.max,
                            step_hz: range.res,
                        })
                        .collect(),
                    sample_rates: sample_rates.clone(),
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
                });
                if let Some(profile) = stream_format.native_dsd {
                    let mut native = alt_settings.last().unwrap().clone();
                    native.format = AltFormat::Dsd;
                    native.subslot_bytes = profile.word_bytes;
                    native.valid_bits = 1;
                    alt_settings.push(native);
                }
                if playback_designs_dop {
                    // This exact alternate is excluded by the native resolver.
                    let mut dop = alt_settings.last().unwrap().clone();
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

    for (candidate, route) in adc3_candidates(device, handle, speed, claimed_interfaces) {
        let rates = candidate.sample_rates.clone();
        supported_sample_rates.extend(rates.iter().copied());
        let ranges = adc3::adc3_wire_ranges(handle, &route).unwrap_or_default();
        let ranges = adc3_capacity_ranges(&candidate, &ranges);
        alt_settings.push(AltSetting {
            interface_number: candidate.interface_number, alt_setting: candidate.alt_setting,
            endpoint_address: candidate.endpoint_address, format: AltFormat::Pcm,
            channels: candidate.channels, subslot_bytes: candidate.subslot_size,
            valid_bits: candidate.bit_resolution,
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
    let config_descriptor = device.active_config_descriptor().map_err(|error| {
        SelectError::NoMatch(format!(
            "Failed to read active USB config descriptor: {}",
            error
        ))
    })?;
    let implicit_sources = implicit_source_inventory(&config_descriptor, handle, speed);
    let mut refused_unsynchronized = false;
    let mut refused_claim = None;
    let allow_unsynchronized_async = settings().allow_unsynchronized_async();
    let mut candidates = Vec::new();
    let prefer_padded_24bit_transport = settings().prefer_padded_24bit();
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
            stream_format.native_dsd = resolve_native_dsd(device, &config_descriptor, &descriptor, &stream_format);
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
                let service_interval_us = service_interval_micros(speed, endpoint.interval());
                let max_packet_bytes =
                    effective_iso_packet_bytes(endpoint.max_packet_size(), speed);
                if explicit_feedback_ambiguous(
                    &descriptor,
                    endpoint.synch_address(),
                    endpoint.sync_type(),
                ) {
                    continue;
                }
                let discovered_feedback = find_feedback_endpoint(
                    &descriptor,
                    endpoint.synch_address(),
                    endpoint.sync_type(),
                    speed,
                );
                let discovered_implicit = if endpoint.sync_type() == SyncType::Asynchronous
                    && discovered_feedback.is_none()
                    && !stream_format.is_uac1
                    && stream_format.format_tag == FORMAT_TAG_PCM
                    && (1..=16).contains(&endpoint.interval())
                    && valid_implicit_packet_capacity(endpoint.max_packet_size(), speed)
                {
                    resolve_implicit_source(
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
                    )
                } else {
                    None
                };
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

                candidates.push(IsoStreamCandidate {
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

    if playback_format.dsd_transport == DsdTransportMode::None {
        for (candidate, route) in adc3_candidates(device, handle, speed, claimed_interfaces) {
            let format = StreamingInterfaceFormat { format_tag:FORMAT_TAG_PCM,uac2_formats:None,native_dsd:None,
                channels:candidate.channels,subslot_size:candidate.subslot_size,bit_resolution:candidate.bit_resolution,
                terminal_link:candidate.terminal_link,is_uac1:false,sample_rate_ranges:vec![] };
            if transport_compatibility_penalty(&format,playback_format) == u32::MAX { continue; }
            let supported = adc3::adc3_wire_ranges(handle, &route)
                .map(|ranges| sampling_frequency_ranges_support_rate(&ranges, playback_format.sample_rate)).unwrap_or(false);
            if supported && candidate.implicit_feedback.map_or(true, |source| source.max_packet_bytes >= required_max_packet_bytes(
                playback_format.sample_rate, source.service_interval_us, source.bytes_per_frame))
                && candidate.max_packet_bytes >= required_max_packet_bytes(playback_format.sample_rate,candidate.service_interval_us,usize::from(candidate.channels)*usize::from(candidate.subslot_size)) {
                candidates.push(candidate);
            }
        }
    }
    let dsd_bit_rate = playback_format.dsd_bit_rate;
    let is_native_dsd = playback_format.dsd_transport == DsdTransportMode::Native;
    candidates.sort_by_key(|candidate| {
        let candidate_rate = if is_native_dsd && dsd_bit_rate > 0 {
            dsd_bit_rate / (8 * u32::from(candidate.subslot_size.max(1)))
        } else {
            playback_format.sample_rate
        };
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
        format!("DSD{}@{}Hz", dsd_bit_rate, dsd_bit_rate / 32)
    } else {
        format!("{} Hz", playback_format.sample_rate)
    };
    candidates.into_iter().next().ok_or_else(|| {
        if let Some(interface) = refused_claim {
            return SelectError::Claim(format!("Required implicit feedback interface {interface} was not claimed"));
        }
        if refused_unsynchronized {
            return SelectError::ImplicitFeedback(format!(
                "The only isochronous OUT endpoints that can carry {} / {}-bit / {} ch are asynchronous without a usable claimed feedback source",
                label_rate, playback_format.bit_depth, playback_format.channels
            ));
        }
        SelectError::NoMatch(format!(
            "No isochronous OUT endpoint can carry {} / {}-bit / {} ch",
            label_rate, playback_format.bit_depth, playback_format.channels
        ))
    })
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
fn valid_implicit_packet_capacity(raw: u16, speed: Speed) -> bool {
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
        16 => Some(2),
        24 => Some(3),
        32 => Some(4),
        _ => None,
    }
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
                                sample_rate_ranges.push(SamplingFrequencySubrange {
                                    min,
                                    max,
                                    res: 0,
                                });
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

// A failed UAC1 SET_CUR is only tolerable when the endpoint is host-paced
// (adaptive/synchronous). Asynchronous runs its own clock; NoSync free-runs
// (FiiO BR13) — either way the DAC rate stays unknown and direct streaming
// would be pitched wrong.
pub(super) fn uac1_set_cur_failure_tolerable(sync_type: SyncType) -> bool {
    matches!(sync_type, SyncType::Adaptive | SyncType::Synchronous)
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
    if if is_native_dsd { candidate.native_dsd.is_none() }
        else { candidate.format_tag != FORMAT_TAG_PCM } {
        return Err(format!(
            "USB direct requires PCM transport, got {}",
            format_tag_label(candidate.format_tag)
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
        let fixed = playback_implicit_profile(0x19f7, 0x000a);
        assert!(implicit_profile_proven(fixed, true, true, Some(48_000)));
        assert!(!implicit_profile_proven(fixed, false, true, Some(48_000)));
        assert!(!implicit_profile_proven(fixed, true, false, Some(48_000)));
        assert!(!implicit_profile_proven(fixed, true, true, None));
        assert!(implicit_profile_proven(PlaybackImplicitProfile::Generic, false, false, None));
        assert!(!implicit_profile_proven(PlaybackImplicitProfile::Skip, true, true, Some(48_000)));
    }

    #[test]
    fn exact_fixed_profiles_preserve_every_linux_pipe() {
        for (vendor, product, address, interface) in [
            (0x0763, 0x2030, 0x81, 3), (0x0763, 0x2031, 0x81, 3),
            (0x0763, 0x2080, 0x81, 2), (0x0763, 0x2081, 0x81, 2),
            (0x2466, 0x8010, 0x81, 2), (0x31e9, 0x0001, 0x81, 2),
            (0x31e9, 0x0002, 0x81, 2), (0x0499, 0x172f, 0x81, 2),
            (0x0d9a, 0x00df, 0x81, 2), (0x19f7, 0x000a, 0x84, 3),
            (0x22f0, 0x0006, 0x81, 3), (0x1686, 0xf029, 0x82, 2),
            (0x2466, 0x8003, 0x86, 2), (0x0499, 0x172a, 0x86, 2),
        ] {
            assert_eq!(playback_implicit_profile(vendor, product),
                PlaybackImplicitProfile::Fixed { address, interface });
        }
        for identity in [(0x0763, 0x9999), (0x9999, 0x2030),
            (0x20b1, 0x200a), (0x07fd, 0x0005), (0, 0)] {
            assert_eq!(playback_implicit_profile(identity.0, identity.1),
                PlaybackImplicitProfile::Generic);
        }
    }

    #[test]
    fn fixed_profile_uses_only_exact_pipe_and_refuses_conflicting_sync_address() {
        let profile = playback_implicit_profile(0x19f7, 0x000a);
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
            let profile = playback_implicit_profile(identity.0, identity.1);
            assert_eq!(profile, PlaybackImplicitProfile::Skip);
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
