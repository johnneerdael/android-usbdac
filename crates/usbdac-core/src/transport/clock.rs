use super::*;
use crate::clock_resolution::{ClockHopKind, RateRatio};

/// Audio 2.0 5.2.5.3: GET_CUR numerator/denominator are positive u16.
/// No multiplier SET exists: the standard declares both controls read-only.
fn read_uac2_multiplier(handle: &DeviceHandle<Context>, control: u8, entity: u8) -> Result<(u16, u16), String> {
    read_multiplier_factors(|selector, bytes| {
        itf_timed_read_control(handle, USB_DIR_IN | USB_TYPE_CLASS | USB_RECIP_INTERFACE,
            UAC2_REQUEST_GET_CUR, u16::from(selector) << 8,
            (u16::from(entity) << 8) | u16::from(control), bytes, Duration::from_millis(500))
            .map_err(|e| format!("multiplier {entity} GET_CUR {selector}: {e}"))
    })
}
fn read_multiplier_factors(mut read: impl FnMut(u8, &mut [u8]) -> Result<usize, String>) -> Result<(u16,u16), String> {
    let mut factors = [0u16; 2];
    for (index, factor) in factors.iter_mut().enumerate() {
        let mut bytes = [0; 2];
        let n = read(index as u8 + 1, &mut bytes)?;
        if n != 2 { return Err(format!("multiplier control returned {n} bytes, expected 2")); }
        *factor = u16::from_le_bytes(bytes);
        if *factor == 0 { return Err("multiplier factor is zero".into()); }
    }
    Ok((factors[0], factors[1]))
}
pub(super) fn prove_uac2_clock_choice(handle: &DeviceHandle<Context>, graph: &ClockGraph, choice: &ClockChoice) -> Result<ClockChoice, String> {
    if has_multiplier(Some(choice)) && !graph.multiplier_choice_source_exact(choice) {
        return Err("multiplier path has no exact readable source".into());
    }
    let mut proven = choice.clone();
    for hop in &mut proven.path {
        if matches!(hop.kind, ClockHopKind::Multiplier | ClockHopKind::RateMultiplier { .. }) {
            if !graph.multiplier_readable(hop.id) { return Err(format!("multiplier {} has no exact read-only controls", hop.id)); }
            let (numerator, denominator) = read_uac2_multiplier(handle, choice.control_interface, hop.id)?;
            if let ClockHopKind::RateMultiplier { numerator: n, denominator: d } = hop.kind {
                if (n,d) != (numerator,denominator) { return Err("multiplier raw factors drifted".into()); }
            }
            hop.kind = ClockHopKind::RateMultiplier { numerator, denominator };
        }
    }
    uac2_clock_ratio(Some(&proven))?;
    Ok(proven)
}
pub(super) fn uac2_clock_ratio(choice: Option<&ClockChoice>) -> Result<RateRatio,String> {
    let mut factors = Vec::new();
    if let Some(choice) = choice {
        for hop in &choice.path {
            match hop.kind {
                ClockHopKind::Multiplier => return Err("unproved clock multiplier cadence".into()),
                ClockHopKind::RateMultiplier { numerator, denominator } => factors.push((numerator, denominator)),
                _ => {}
            }
        }
    }
    RateRatio::from_factors(&factors)
}
pub(super) fn verify_uac2_clock_factors(handle: &DeviceHandle<Context>, choice: &ClockChoice) -> Result<(),String> {
    uac2_clock_ratio(Some(choice))?;
    for hop in &choice.path {
        if let ClockHopKind::RateMultiplier { numerator, denominator } = hop.kind {
            if read_uac2_multiplier(handle, choice.control_interface, hop.id)? != (numerator,denominator) {
                return Err(format!("multiplier {} raw factors drifted", hop.id));
            }
        }
    }
    Ok(())
}
fn verify_active_uac2_choice(handle: &DeviceHandle<Context>, clock: AudioControlClock, choice: &ClockChoice) -> Result<(), String> {
    if choice.control_interface != clock.interface_number || choice.source_id != clock.clock_id {
        return Err("clock choice source/interface changed".into());
    }
    verify_uac2_clock_factors(handle, choice)?;
    for hop in &choice.path {
        if let ClockHopKind::Selector { selected_pin, pin_count } = hop.kind {
            if pin_count > 1 && get_clock_selector_pin(handle, choice.control_interface, hop.id)? != selected_pin {
                return Err(format!("clock selector {} drifted from selected route", hop.id));
            }
        }
    }
    Ok(())
}
fn has_multiplier(choice: Option<&ClockChoice>) -> bool {
    choice.is_some_and(|c| c.path.iter().any(|h| matches!(h.kind, ClockHopKind::Multiplier | ClockHopKind::RateMultiplier { .. })))
}
fn project_wire_ranges(ratio: RateRatio, ranges: Vec<SamplingFrequencySubrange>) -> Vec<SamplingFrequencySubrange> {
    ranges.into_iter().filter_map(|r| ratio.project_range(r.min,r.max,r.res)
        .map(|(min,max,res)| SamplingFrequencySubrange { min,max,res })).collect()
}
pub(super) fn get_wire_frequency(handle: &DeviceHandle<Context>, clock: AudioControlClock, choice: Option<&ClockChoice>) -> Result<u32,String> {
    if !has_multiplier(choice) { return get_sampling_frequency(handle,clock.interface_number,clock.clock_id); }
    let ratio = uac2_clock_ratio(choice)?;
    if let Some(c) = choice { verify_active_uac2_choice(handle,clock,c)?; }
    let source = get_sampling_frequency(handle,clock.interface_number,clock.clock_id)?;
    if let Some(c) = choice { verify_active_uac2_choice(handle,clock,c)?; }
    ratio.wire_rate(source).ok_or_else(|| "source rate does not map to exact positive wire Hz".into())
}
pub(super) fn get_wire_frequency_ranges(handle: &DeviceHandle<Context>, clock: AudioControlClock, choice: Option<&ClockChoice>) -> Result<Vec<SamplingFrequencySubrange>,String> {
    if !has_multiplier(choice) { return get_sampling_frequency_ranges(handle,clock.interface_number,clock.clock_id); }
    let ratio = uac2_clock_ratio(choice)?;
    if let Some(c) = choice { verify_active_uac2_choice(handle,clock,c)?; }
    let ranges = get_sampling_frequency_ranges(handle,clock.interface_number,clock.clock_id)?;
    if let Some(c) = choice { verify_active_uac2_choice(handle,clock,c)?; }
    Ok(project_wire_ranges(ratio,ranges))
}
pub(super) fn apply_wire_frequency(handle: &DeviceHandle<Context>, clock: AudioControlClock, choice: Option<&ClockChoice>, wire_rate: u32, settle_delay_ms: u64) -> ClockApplyOutcome {
    if !has_multiplier(choice) { return apply_sampling_frequency(handle,clock.interface_number,clock.clock_id,wire_rate,settle_delay_ms); }
    let result = (|| {
        let c = choice.ok_or("missing multiplier clock choice")?;
        if c.source_id != clock.clock_id || c.control_interface != clock.interface_number { return Err("multiplier source changed".into()); }
        let rate = get_wire_frequency(handle,clock,choice)?;
        if rate != wire_rate { return Err(format!("multiplier wire rate {rate} differs from requested {wire_rate}")); }
        let controls = uac2_source_controls(handle,clock.interface_number,clock.clock_id).ok_or("missing exact source controls")?;
        crate::clock_resolution::clock_validity(controls, || get_clock_validity(handle,clock.interface_number,clock.clock_id))?;
        verify_active_uac2_choice(handle,clock,c)?;
        Ok(rate)
    })();
    let (clock_ok,reported_sample_rate,message) = match result { Ok(rate) => (true,Some(rate),None), Err(e) => (false,None,Some(e)) };
    ClockApplyOutcome { clock_ok, rate_verified: clock_ok, reported_sample_rate, known_mismatch: !clock_ok, set_cur_succeeded: false, message }
}

/// Resolve and prove the stream's current AudioControl clock and cadence.
pub(super) fn find_audio_control_clock_choice(
    device: &Device<Context>,
    handle: &DeviceHandle<Context>,
    terminal_link: Option<u8>,
) -> Result<Option<(AudioControlClock, ClockChoice)>, String> {
    let Ok(config_descriptor) = device.active_config_descriptor() else { return Ok(None); };

    for interface in config_descriptor.interfaces() {
        for descriptor in interface.descriptors() {
            if descriptor.class_code() != USB_CLASS_AUDIO
                || descriptor.sub_class_code() != USB_SUBCLASS_AUDIOCONTROL
            {
                continue;
            }

            let ac_iface = descriptor.interface_number();
            let extra = descriptor.extra();
            dev_eprintln!(
                "[USB] AudioControl interface {} alt {} extra bytes: {} bytes",
                ac_iface,
                descriptor.setting_number(),
                extra.len(),
            );

            let raw_descriptors;
            let class_descriptors: &[u8] = if extra.is_empty() {
                dev_eprintln!(
                    "[USB] AudioControl interface {} has empty extra bytes; \
                     attempting raw GET_DESCRIPTOR fallback",
                    ac_iface,
                );
                match read_audio_control_descriptors_from_raw_config(
                    handle,
                    &config_descriptor,
                    ac_iface,
                ) {
                    Some(bytes) => {
                        raw_descriptors = bytes;
                        &raw_descriptors
                    }
                    None => continue,
                }
            } else {
                extra
            };

            let graph = ClockGraph::parse(class_descriptors);
            if !graph.has_clock_entities() {
                dev_eprintln!(
                    "[USB] AudioControl interface {} describes no clock entities",
                    ac_iface
                );
                continue;
            }

            match graph.select_clock(ac_iface, terminal_link, |selector| {
                get_clock_selector_pin(handle, ac_iface, selector)
            }) {
                Ok(selection) => {
                    match &selection.unresolved {
                        None => log_info!(
                            "[USB] Clock resolved via topology: bTerminalLink={:?} -> clock path {:?} -> \
                             Clock Source id={} on AC interface {}{}",
                            terminal_link,
                            selection
                                .choice
                                .path
                                .iter()
                                .skip(1)
                                .map(|hop| hop.id)
                                .collect::<Vec<_>>(),
                            selection.choice.source_id,
                            ac_iface,
                            if selection.assumed_source {
                                " (no descriptor for that ID; assumed to be the source)"
                            } else {
                                ""
                            },
                        ),
                        // The first Clock Source in descriptor order is the last resort.
                        Some(reason) => log_warn!(
                            "[USB] Could not resolve the clock for bTerminalLink={:?} on AC interface {}: \
                             {}. Using the first Clock Source id={}; on a DAC with more than one clock \
                             the sample rate may be set on the wrong one.",
                            terminal_link,
                            ac_iface,
                            reason,
                            selection.choice.source_id,
                        ),
                    }
                    let proven = match prove_uac2_clock_choice(handle, &graph, &selection.choice) {
                        Ok(choice) => choice,
                        Err(error) => return Err(format!("Clock multiplier proof failed: {error}")),
                    };
                    let clock = AudioControlClock {
                        interface_number: ac_iface,
                        clock_id: selection.choice.source_id,
                    };
                    return Ok(Some((clock, proven)));
                }
                Err(reason) => {
                    if terminal_link.is_some_and(|id| graph.terminal_may_use_multiplier(id)) { return Err(reason); }
                    log_warn!("[USB] Could not resolve clock on AC interface {ac_iface}: {reason}");
                },
            }
        }
    }

    dev_eprintln!("[USB] No AudioControl clock source entity found in any interface descriptor");
    Ok(None)
}

/// CX_CLOCK_SELECTOR_CONTROL GET_CUR: the 1-based input pin a Clock Selector
/// currently has selected. wIndex is the selector's entity ID over the
/// AudioControl interface number.
pub(super) fn get_clock_selector_pin(
    handle: &DeviceHandle<Context>,
    interface_number: u8,
    selector_id: u8,
) -> Result<u8, String> {
    let request_type = USB_DIR_IN | USB_TYPE_CLASS | USB_RECIP_INTERFACE;
    let index = (interface_number as u16) | ((selector_id as u16) << 8);
    let mut data = [0u8; 1];
    let transferred = itf_timed_read_control(handle,
            request_type,
            UAC2_REQUEST_GET_CUR,
            UAC2_CLOCK_SELECTOR_CONTROL,
            index,
            &mut data,
            Duration::from_millis(500),
        )
        .map_err(|error| format!("Clock Selector GET_CUR failed: {}", error))?;
    if transferred != 1 {
        return Err(format!(
            "Clock Selector returned {} bytes, expected 1",
            transferred
        ));
    }
    Ok(data[0])
}

/// Read the configuration descriptor with a raw GET_DESCRIPTOR and extract one
/// AudioControl interface's class-specific descriptors. For platforms that give
/// no `extra` bytes for the interface.
pub(super) fn read_audio_control_descriptors_from_raw_config(
    handle: &DeviceHandle<Context>,
    config_descriptor: &ConfigDescriptor,
    ac_interface_number: u8,
) -> Option<Vec<u8>> {
    let raw = read_active_configuration_bytes(handle, config_descriptor)?;
    let descriptors = audio_control_descriptors_from_config(&raw, ac_interface_number);
    if descriptors.is_empty() {
        log::error!(
            "[USB] Raw config descriptor fallback found no class-specific descriptors for interface {}",
            ac_interface_number,
        );
        return None;
    }
    Some(descriptors)
}

pub(super) fn set_sampling_frequency(
    handle: &DeviceHandle<Context>,
    interface_number: u8,
    clock_id: u8,
    sample_rate: u32,
) -> Result<(), String> {
    let request_type = USB_DIR_OUT | USB_TYPE_CLASS | USB_RECIP_INTERFACE;
    let value = UAC2_CLOCK_SOURCE_SAM_FREQ_CONTROL;
    let index = (interface_number as u16) | ((clock_id as u16) << 8);
    let data = sample_rate.to_le_bytes();
    let transferred = itf_timed_write_control(handle,
            request_type,
            UAC2_REQUEST_SET_CUR,
            value,
            index,
            &data,
            Duration::from_secs(1),
        )
        .map_err(|error| format!("Failed to set sampling frequency: {}", error))?;
    if transferred != data.len() {
        return Err(format!("Sampling-frequency SET_CUR transferred {transferred} bytes, expected {}", data.len()));
    }
    Ok(())
}

/// Descriptor evidence is independent of whether a legacy SET_CUR is ACKed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Uac1PlaybackRateProfile {
    pub(super) sampling_control: Option<bool>,
    pub(super) fixed_at_request: bool,
    pub(super) host_paced_at_request: bool,
}

pub(super) fn uac1_playback_rate_profile(
    claimed: &ClaimedHandle,
    candidate: &IsoStreamCandidate,
    rate: u32,
) -> Result<Uac1PlaybackRateProfile, String> {
    if !candidate.is_uac1 || !claimed.claimed_interfaces.contains(&candidate.interface_number) {
        return Err("UAC1 playback rate interface is not claimed".into());
    }
    // Cached descriptors only: do not introduce control IO into evidence lookup.
    let config = claimed.handle.device().active_config_descriptor().map_err(|e| e.to_string())?;
    if config.number() != candidate.configuration_number {
        return Err("UAC1 playback configuration changed".into());
    }
    for interface in config.interfaces() {
        for alt in interface.descriptors() {
            if alt.interface_number() != candidate.interface_number || alt.setting_number() != candidate.alt_setting {
                continue;
            }
            if alt.class_code() != USB_CLASS_AUDIO || alt.sub_class_code() != 2 || alt.protocol_code() != 0 {
                return Err("UAC1 playback alternate no longer describes AudioStreaming 1.0".into());
            }
            let format = parse_android_streaming_interface_format(&alt);
            let matching_format = format.as_ref().filter(|format| format.is_uac1
                && format.format_tag == candidate.format_tag && format.channels == candidate.channels
                && format.subslot_size == candidate.subslot_size && format.bit_resolution == candidate.bit_resolution);
            for ep in alt.endpoint_descriptors() {
                if ep.address() != candidate.endpoint_address { continue; }
                if ep.direction() != Direction::Out || ep.transfer_type() != TransferType::Isochronous
                    || !matches!(ep.usage_type(), UsageType::Data | UsageType::FeedbackData)
                    || ep.sync_type() != candidate.sync_type {
                    return Err("UAC1 playback endpoint is not the selected isochronous OUT endpoint".into());
                }
                // A truncated frequency list must not turn a multi-rate alt into
                // apparently fixed-rate evidence. Use the original AS ranges,
                // never candidate.sample_rates (which selection may have filled).
                let ranges = if uac1_complete_rate_descriptor(alt.extra()) {
                    matching_format.map(|format| format.sample_rate_ranges.as_slice()).unwrap_or(&[])
                } else { &[] };
                let mut profile = uac1_rate_profile(ep.extra().unwrap_or(&[]), ep.sync_type(), ranges, rate);
                if settings().config.quirk_policy == crate::quirk::QuirkPolicy::SourceBacked {
                    let descriptor = claimed.handle.device().device_descriptor().map_err(|error| error.to_string())?;
                    if let Some(corrected) = uac1_sampling_attribute_correction(descriptor.vendor_id(),descriptor.product_id()) {
                        profile.sampling_control = Some(corrected);
                    }
                }
                return Ok(profile);
            }
            return Err("UAC1 playback endpoint disappeared".into());
        }
    }
    Err("UAC1 playback alternate disappeared".into())
}

// Factual attribute corrections, Linux v7.3 quirks.c:2117-2131. Apply only
// after exact fd identity and selected UAC1 OUT descriptor proof. These change
// standard Sampling Frequency Control admission, never a vendor command.
fn uac1_sampling_attribute_correction(vendor: u16, product: u16) -> Option<bool> {
    match (vendor,product) {
        (0x0a92,0x0053) => Some(false),
        (0x041e,0x3020) | (0x0763,0x2003) => Some(true),
        _ => None,
    }
}

fn uac1_complete_rate_descriptor(extra: &[u8]) -> bool {
    let mut formats = 0;
    let mut generals = 0;
    let mut at = 0;
    while at < extra.len() {
        if extra.len() - at < 2 || extra[at] < 2 || usize::from(extra[at]) > extra.len() - at { return false; }
        let d = &extra[at..at + usize::from(extra[at])];
        if d.get(1) == Some(&USB_DT_CS_INTERFACE) && d.get(2) == Some(&UAC2_AS_GENERAL) {
            if d.len() != 7 { return false; }
            generals += 1;
        }
        if d.get(1) == Some(&USB_DT_CS_INTERFACE) && d.get(2) == Some(&UAC2_FORMAT_TYPE) {
            if d.get(3) != Some(&UAC2_FORMAT_TYPE_I) || d.len() < 8 { return false; }
            if d[4] == 0 || !(1..=4).contains(&d[5]) || d[6] == 0 || u16::from(d[6]) > u16::from(d[5]) * 8 { return false; }
            let expected = if d[7] == 0 { 14 } else { 8 + usize::from(d[7]) * 3 };
            if d.len() != expected { return false; }
            formats += 1;
        }
        at += d.len();
    }
    formats == 1 && generals == 1
}

fn uac1_rate_profile(extra: &[u8], sync: SyncType, ranges: &[SamplingFrequencySubrange], rate: u32) -> Uac1PlaybackRateProfile {
    let mut sampling_control = None;
    let mut at = 0;
    let mut seen = false;
    while at < extra.len() {
        if extra.len() - at < 2 || extra[at] < 2 || usize::from(extra[at]) > extra.len() - at {
            sampling_control = None;
            break;
        }
        let d = &extra[at..at + usize::from(extra[at])];
        if d[1] == 0x25 && d.get(2) == Some(&1) {
            // Audio 1.0 4.6.1.2 EP_GENERAL is exactly seven bytes; D0
            // advertises Sampling Frequency Control. D1 is Pitch Control.
            if seen || d.len() != 7 { sampling_control = None; break; }
            seen = true;
            sampling_control = Some(d[3] & 1 != 0);
        }
        at += d.len();
    }
    // UAC1 continuous descriptors describe inclusive integer-Hz intervals.
    // Accept historical res=0 and the parser's canonical res=1 locally;
    // do not change UAC2 RANGE singleton semantics.
    let valid = !ranges.is_empty() && ranges.iter().all(|r|
        r.res <= 1 && valid_uac1_rate(r.min) && valid_uac1_rate(r.max) && r.min <= r.max);
    let supported = valid && valid_uac1_rate(rate) && ranges.iter().any(|r| rate >= r.min && rate <= r.max);
    Uac1PlaybackRateProfile {
        sampling_control,
        fixed_at_request: supported && ranges.iter().all(|r| r.min == rate && r.max == rate),
        host_paced_at_request: supported && matches!(sync, SyncType::Adaptive | SyncType::Synchronous),
    }
}

pub(super) fn uac1_playback_rate_verified(requested: u32, reported: Option<u32>, profile: Uac1PlaybackRateProfile, pcm: bool) -> bool {
    if !valid_uac1_rate(requested) { return false; }
    match reported {
        Some(rate) => valid_uac1_rate(rate) && rate == requested,
        None => pcm && profile.sampling_control == Some(false)
            && (profile.fixed_at_request || profile.host_paced_at_request),
    }
}

pub(super) fn selector_write_required(skip_writes:bool,current:u8,target:u8) -> Result<bool,String> {
    if target==0 {return Err("clock selector pins are one-based".into());}
    if !skip_writes {return Ok(true);}
    if current==target {Ok(false)} else {Err("source-backed clock selector policy preserves the current route".into())}
}

pub(super) fn fixed_rate_live_proof(requested: u32, reported: Option<u32>, descriptor_fixed: bool, clock_valid: bool) -> bool {
    requested > 0 && descriptor_fixed && clock_valid && reported == Some(requested)
}

fn valid_uac1_rate(rate: u32) -> bool { (1..=0x7f_ffff).contains(&rate) }

fn decode_uac1_rate(data: [u8; 3], transferred: usize) -> Result<u32, String> {
    if transferred != 3 { return Err(format!("UAC1 GET_CUR returned {transferred} bytes, expected 3")); }
    let rate = u32::from_le_bytes([data[0], data[1], data[2], 0]);
    if !valid_uac1_rate(rate) { return Err(format!("UAC1 GET_CUR returned invalid positive Hz {rate}")); }
    Ok(rate)
}

fn check_uac1_write(transferred: usize) -> Result<(), String> {
    if transferred != 3 { return Err(format!("UAC1 SET_CUR transferred {transferred} bytes, expected 3")); }
    Ok(())
}

pub(super) fn set_uac1_sampling_frequency(
    handle: &DeviceHandle<Context>,
    interface_number: u8,
    endpoint_address: u8,
    sample_rate: u32,
) -> Result<(), String> {
    if !valid_uac1_rate(sample_rate) { return Err(format!("Invalid UAC1 sampling frequency {sample_rate}")); }
    let data = [
        (sample_rate & 0xFF) as u8,
        ((sample_rate >> 8) & 0xFF) as u8,
        ((sample_rate >> 16) & 0xFF) as u8,
    ];

    // UAC1 5.2.3.2.3.1: SAM_FREQ_CONTROL SET_CUR is an endpoint request
    // (bmRequestType 0x22, wValue 0x0100, 3-byte LE rate). Real hardware
    // disagrees on wIndex: Linux snd-usb-audio sends the endpoint address
    // alone, while some devices require the interface in the high byte;
    // some legacy firmware only accepts the interface-recipient hybrid. A control STALL
    // is auto-cleared before the next SETUP, so failing forms can be tried
    // in sequence. Some devices (FiiO BR13) STALL every form while the
    // streaming alt setting is inactive — the stream-open path retries after
    // the alt setting is live.
    let encodings: [(u8, u16, &str); 3] = [
        (
            USB_DIR_OUT | USB_TYPE_CLASS | USB_RECIP_ENDPOINT,
            endpoint_address as u16,
            "endpoint-only (Linux)",
        ),
        (
            USB_DIR_OUT | USB_TYPE_CLASS | USB_RECIP_ENDPOINT,
            ((interface_number as u16) << 8) | endpoint_address as u16,
            "endpoint+interface (compatibility)",
        ),
        (
            USB_DIR_OUT | USB_TYPE_CLASS | USB_RECIP_INTERFACE,
            endpoint_address as u16,
            "interface-recipient (legacy)",
        ),
    ];

    let mut last_error = String::new();
    for (request_type, index, label) in encodings {
        match itf_timed_write_control(handle,
            request_type,
            UAC2_REQUEST_SET_CUR,
            UAC1_SAM_FREQ_CONTROL,
            index,
            &data,
            Duration::from_secs(1),
        ) {
            Ok(transferred) => {
                check_uac1_write(transferred)?;
                dev_eprintln!(
                    "[USB] UAC1 SET_CUR: endpoint=0x{:02x} rate={}Hz accepted wIndex=0x{:04x} ({})",
                    endpoint_address,
                    sample_rate,
                    index,
                    label
                );
                return Ok(());
            }
            Err(error) => {
                if error == rusb::Error::NoDevice { return Err("UAC1 SET_CUR: no device".into()); }
                dev_eprintln!(
                    "[USB] UAC1 SET_CUR wIndex=0x{:04x} ({}) stalled: {}",
                    index,
                    label,
                    error
                );
                last_error = format!("Failed to set UAC1 sampling frequency: {}", error);
            }
        }
    }
    Err(last_error)
}

pub(super) fn get_uac1_sampling_frequency(
    handle: &DeviceHandle<Context>,
    interface_number: u8,
    endpoint_address: u8,
) -> Result<u32, String> {
    if settings().has(crate::Quirk::SkipSampleRateReadback) {
        return Err("frequency GET_CUR omitted by exact source-backed identity rule; clock remains unverified".into());
    }
    let mut data = [0u8; 3];
    let encodings: [(u8, u16, &str); 2] = [
        (
            USB_DIR_IN | USB_TYPE_CLASS | USB_RECIP_ENDPOINT,
            endpoint_address as u16,
            "endpoint-only (Linux)",
        ),
        (
            USB_DIR_IN | USB_TYPE_CLASS | USB_RECIP_ENDPOINT,
            ((interface_number as u16) << 8) | endpoint_address as u16,
            "endpoint+interface (compatibility)",
        ),
    ];

    let mut last_error = String::new();
    for (request_type, index, label) in encodings {
        match itf_timed_read_control(handle,
            request_type,
            UAC1_REQUEST_GET_CUR,
            UAC1_SAM_FREQ_CONTROL,
            index,
            &mut data,
            Duration::from_secs(1),
        ) {
            Ok(transferred) => {
                let rate = match decode_uac1_rate(data, transferred) {
                    Ok(rate) => rate,
                    Err(error) => { last_error = format!("{error} (wIndex=0x{index:04x}, {label})"); continue; }
                };
                dev_eprintln!(
                    "[USB] UAC1 GET_CUR: endpoint=0x{:02x} reports {}Hz via wIndex=0x{:04x} ({})",
                    endpoint_address,
                    rate,
                    index,
                    label
                );
                return Ok(rate);
            }
            Err(error) => {
                if error == rusb::Error::NoDevice { return Err("UAC1 GET_CUR: no device".into()); }
                dev_eprintln!(
                    "[USB] UAC1 GET_CUR wIndex=0x{:04x} ({}) failed: {}",
                    index,
                    label,
                    error
                );
                last_error = format!("Failed to read UAC1 sampling frequency: {}", error);
            }
        }
    }
    Err(last_error)
}

pub(super) fn get_sampling_frequency(
    handle: &DeviceHandle<Context>,
    interface_number: u8,
    clock_id: u8,
) -> Result<u32, String> {
    if settings().has(crate::Quirk::SkipSampleRateReadback) {
        return Err("frequency GET_CUR omitted by exact source-backed identity rule; clock remains unverified".into());
    }
    let request_type = USB_DIR_IN | USB_TYPE_CLASS | USB_RECIP_INTERFACE;
    let value = UAC2_CLOCK_SOURCE_SAM_FREQ_CONTROL;
    let index = (interface_number as u16) | ((clock_id as u16) << 8);
    let mut data = [0u8; 4];
    let transferred = itf_timed_read_control(handle,
            request_type,
            UAC2_REQUEST_GET_CUR,
            value,
            index,
            &mut data,
            Duration::from_secs(1),
        )
        .map_err(|error| if error == rusb::Error::NoDevice { "Failed to read sampling frequency: no device".into() }
            else { format!("Failed to read sampling frequency: {}", error) })?;
    if transferred != data.len() {
        return Err(format!(
            "Sampling-frequency read returned {} bytes, expected {}",
            transferred,
            data.len()
        ));
    }
    Ok(u32::from_le_bytes(data))
}

/// UAC2 Clock Validity Control (CS_CLOCK_VALID_CONTROL = 0x02).
/// Returns Ok(true) if the clock is valid/stable, Ok(false) if invalid,
/// or Err if the request is not supported.
pub(super) fn get_clock_validity(
    handle: &DeviceHandle<Context>,
    interface_number: u8,
    clock_id: u8,
) -> Result<bool, String> {
    let request_type = USB_DIR_IN | USB_TYPE_CLASS | USB_RECIP_INTERFACE;
    let value = UAC2_CLOCK_SOURCE_CLOCK_VALID_CONTROL;
    let index = (interface_number as u16) | ((clock_id as u16) << 8);
    let mut data = [0u8; 1];
    let transferred = itf_timed_read_control(handle,
            request_type,
            UAC2_REQUEST_GET_CUR,
            value,
            index,
            &mut data,
            Duration::from_millis(500),
        )
        .map_err(|error| format!("Clock Validity GET_CUR failed: {}", error))?;
    if transferred != 1 {
        return Err(format!(
            "Clock Validity returned {} bytes, expected 1",
            transferred
        ));
    }
    Ok(data[0] != 0)
}

// ALSA's exact MOTU identities need bounded settling after programming. Do
// not make read-only source discovery wait or convert transfer errors to retry.
#[cfg(test)]
fn retry_programmed_clock_validity(
    slow: bool, mut read: impl FnMut() -> Result<bool, String>, mut wait: impl FnMut(),
) -> Result<bool, String> {
    let mut valid = read()?;
    if slow {
        for _ in 0..50 {
            if valid { break; }
            wait();
            valid = read()?;
        }
    }
    Ok(valid)
}
pub(super) fn get_programmed_clock_validity(
    handle: &DeviceHandle<Context>, control: u8, source: u8,
) -> Result<bool, String> {
    // Shared-parity release keeps standard validity checking without the
    // unpublished MOTU identity-specific programmed-clock settling loop.
    get_clock_validity(handle, control, source)
}

pub(super) fn get_sampling_frequency_ranges(
    handle: &DeviceHandle<Context>,
    interface_number: u8,
    clock_id: u8,
) -> Result<Vec<SamplingFrequencySubrange>, String> {
    let request_type = USB_DIR_IN | USB_TYPE_CLASS | USB_RECIP_INTERFACE;
    let value = UAC2_CLOCK_SOURCE_SAM_FREQ_CONTROL;
    let index = (interface_number as u16) | ((clock_id as u16) << 8);
    let mut data = [0u8; 512];
    let transferred = itf_timed_read_control(handle,
            request_type,
            UAC2_REQUEST_GET_RANGE,
            value,
            index,
            &mut data,
            Duration::from_secs(1),
        )
        .map_err(|error| format!("Failed to read sampling-frequency range: {}", error))?;
    parse_sampling_frequency_ranges(&data[..transferred])
}

pub(super) fn parse_sampling_frequency_ranges(data: &[u8]) -> Result<Vec<SamplingFrequencySubrange>, String> {
    if data.len() > 512 { return Err("frequency RANGE reply exceeds bounded control buffer".into()); }
    if data.len() < 2 {
        return Err(format!(
            "Sampling-frequency GET_RANGE returned {} bytes, expected at least 2",
            data.len()
        ));
    }

    let count = u16::from_le_bytes([data[0], data[1]]) as usize;
    if count == 0 {
        return Err("Sampling-frequency GET_RANGE returned zero subranges".to_string());
    }

    let expected = 2 + count * 12;
    if data.len() < expected {
        return Err(format!(
            "Sampling-frequency GET_RANGE returned {} bytes, expected at least {} for {} subranges",
            data.len(), expected, count
        ));
    }

    let mut ranges = Vec::with_capacity(count);
    for index in 0..count {
        let offset = 2 + index * 12;
        ranges.push(SamplingFrequencySubrange {
            min: u32::from_le_bytes([
                data[offset],
                data[offset + 1],
                data[offset + 2],
                data[offset + 3],
            ]),
            max: u32::from_le_bytes([
                data[offset + 4],
                data[offset + 5],
                data[offset + 6],
                data[offset + 7],
            ]),
            res: u32::from_le_bytes([
                data[offset + 8],
                data[offset + 9],
                data[offset + 10],
                data[offset + 11],
            ]),
        });
    }

    Ok(ranges)
}

pub(super) fn discover_stream_sample_rate_ranges(
    device: &Device<Context>,
    handle: &DeviceHandle<Context>,
    streaming_interface_number: u8,
    terminal_link: Option<u8>,
    pre_parsed_ranges: &[SamplingFrequencySubrange],
) -> Vec<SamplingFrequencySubrange> {
    if !pre_parsed_ranges.is_empty() {
        return pre_parsed_ranges.to_vec();
    }

    let Ok(Some((clock, choice))) = find_audio_control_clock_choice(device, handle, terminal_link) else {
        return Vec::new();
    };

    match get_wire_frequency_ranges(handle, clock, Some(&choice)) {
        Ok(ranges) => ranges,
        Err(error) => {
            dev_eprintln!(
                "[USB] GET_RANGE probe failed for streaming interface {} via clock {} on AC interface {}: {}",
                streaming_interface_number,
                clock.clock_id,
                clock.interface_number,
                error,
            );
            Vec::new()
        }
    }
}

pub(super) fn representative_sample_rates_from_ranges(
    ranges: &[SamplingFrequencySubrange],
) -> Vec<u32> {
    const COMMON_SAMPLE_RATES: &[u32] = &[
        8_000, 11_025, 16_000, 22_050, 24_000, 32_000, 44_100, 48_000, 64_000, 88_200, 96_000,
        176_400, 192_000, 352_800, 384_000, 705_600, 768_000,
    ];

    let mut sample_rates = Vec::new();

    for &rate in COMMON_SAMPLE_RATES {
        if sampling_frequency_ranges_support_rate(ranges, rate) {
            sample_rates.push(rate);
        }
    }

    for range in ranges {
        sample_rates.push(range.min);
        if range.max != range.min {
            sample_rates.push(range.max);
        }
    }

    sample_rates.sort_unstable();
    sample_rates.dedup();
    sample_rates
}

pub(super) fn sampling_frequency_ranges_support_rate(
    ranges: &[SamplingFrequencySubrange],
    sample_rate: u32,
) -> bool {
    ranges.iter().any(|range| {
        if range.res == 0 {
            range.min == sample_rate && range.max == sample_rate
        } else {
            sample_rate >= range.min
                && sample_rate <= range.max
                && (sample_rate - range.min) % range.res == 0
        }
    })
}

pub(super) fn format_sampling_frequency_ranges(ranges: &[SamplingFrequencySubrange]) -> String {
    ranges
        .iter()
        .map(|range| {
            if range.res == 0 || range.min == range.max {
                format!("{} Hz", range.min)
            } else {
                format!("{}..{} Hz step {}", range.min, range.max, range.res)
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn uac2_source_controls(handle: &DeviceHandle<Context>, interface: u8, source: u8) -> Option<u32> {
    let config = handle.device().active_config_descriptor().ok()?;
    let raw = read_active_configuration_bytes(handle, &config)?;
    let bytes = audio_control_descriptors_from_config(&raw, interface);
    let mut sources = DescriptorIter::new(&bytes).filter(|d| {
        d.len() >= 8 && d[1] == 0x24 && d[2] == 0x0a && d[3] == source
    });
    let controls = u32::from(sources.next()?[5]);
    if sources.next().is_some() { None } else { Some(controls) }
}

pub(super) fn apply_sampling_frequency(
    handle: &DeviceHandle<Context>,
    interface_number: u8,
    clock_id: u8,
    sample_rate: u32,
    settle_delay_ms: u64,
) -> ClockApplyOutcome {
    log::info!(
        "[USB] apply_sampling_frequency: interface={} clockId={} targetRate={}Hz settleDelay={}ms",
        interface_number,
        clock_id,
        sample_rate,
        settle_delay_ms,
    );

    let source_controls = uac2_source_controls(handle, interface_number, clock_id);
    let supported_ranges = get_sampling_frequency_ranges(handle, interface_number, clock_id).ok();
    match &supported_ranges {
        Some(ranges) => {
            log::info!(
                "[USB] GET_RANGE: {} range(s) from clock {} on interface {}",
                ranges.len(),
                clock_id,
                interface_number,
            );
        }
        None => {
            log::warn!(
                "[USB] GET_RANGE: failed for clock {} on interface {}",
                clock_id,
                interface_number,
            );
        }
    }

    if let Some(ranges) = supported_ranges.as_ref() {
        if !sampling_frequency_ranges_support_rate(ranges, sample_rate) {
            return ClockApplyOutcome {
                clock_ok: false,
                rate_verified: false,
                reported_sample_rate: get_sampling_frequency(handle, interface_number, clock_id).ok(),
                known_mismatch: true,
                set_cur_succeeded: false,
                message: Some(format!(
                    "Requested {} Hz is not supported by clock {} on interface {}; supported rates: {}",
                    sample_rate,
                    clock_id,
                    interface_number,
                    format_sampling_frequency_ranges(ranges),
                )),
            };
        }
    }

    // Read current rate for diagnostics, but NEVER trust it as proof
    // that the clock is correct. Always issue SET_CUR unconditionally
    // because the DAC may report a stale rate or may have been
    // reprogrammed by another app / alt-setting change.
    match get_sampling_frequency(handle, interface_number, clock_id) {
        Ok(current_rate) => {
            dev_eprintln!(
                "[USB] GET_CUR (pre-SET): clock {} on interface {} reports {}Hz (target={}Hz)",
                clock_id,
                interface_number,
                current_rate,
                sample_rate,
            );
            let descriptor_fixed = supported_ranges.as_ref().is_some_and(|ranges|
                !ranges.is_empty() && ranges.iter().all(|range| range.min == sample_rate && range.max == sample_rate));
            if settings().has(crate::Quirk::FixedRate) && descriptor_fixed {
                let validity = source_controls.is_some_and(|controls|
                    crate::clock_resolution::clock_validity(controls,|| get_programmed_clock_validity(handle,interface_number,clock_id)).is_ok());
                if fixed_rate_live_proof(sample_rate,Some(current_rate),descriptor_fixed,validity) {
                    return ClockApplyOutcome {clock_ok:true,rate_verified:true,reported_sample_rate:Some(current_rate),
                        known_mismatch:false,set_cur_succeeded:false,message:Some("fixed descriptor and live clock proof; no SET required".into())};
                }
            }
        }
        Err(error) => {
            dev_eprintln!(
                "[USB] GET_CUR (pre-SET): failed for clock {} on interface {}: {}",
                clock_id,
                interface_number,
                error,
            );
        }
    }

    let learning = SESSION_STATE.lock().as_ref().map(|state| Arc::clone(&state.unreadable_clocks));
    let clock_key = (interface_number, clock_id);
    let learned = learning.as_ref().is_some_and(|clocks| clocks.lock().get(&clock_key).copied().unwrap_or(0) >= 3);
    let allow_learned = settings().config.clock_readback_policy
        == crate::config::ClockReadbackPolicy::AllowUnusableAfterSuccessfulSet;
    // Still sample GET_CUR after every SET: learning must never hide a newly
    // readable mismatch or turn an ACK into a verified clock.
    let attempts = if learned && allow_learned { 1 } else { 3 };
    let mut last_set_error = None;
    let mut last_readback_error = None;
    let mut last_reported_rate = None;
    let mut set_cur_succeeded = false;

    for attempt in 1..=attempts {
        log::info!(
            "[USB] SET_CUR attempt {}/3: clock {} interface {} rate {}Hz",
            attempt,
            clock_id,
            interface_number,
            sample_rate,
        );
        match set_sampling_frequency(handle, interface_number, clock_id, sample_rate) {
            Ok(()) => {
                log::info!("[USB] SET_CUR attempt {}/3: succeeded", attempt);
                set_cur_succeeded = true;
            }
            Err(error) => {
                log::error!("[USB] SET_CUR attempt {}/3: failed: {}", attempt, error);
                set_cur_succeeded = false;
                last_set_error = Some(error);
                thread::sleep(Duration::from_millis(settle_delay_ms));
                continue;
            }
        }

        thread::sleep(Duration::from_millis(settle_delay_ms));
        match get_sampling_frequency(handle, interface_number, clock_id) {
            Ok(reported_sample_rate) => {
                if reported_sample_rate > 0 || last_reported_rate.is_none() {
                    last_reported_rate = Some(reported_sample_rate);
                }
                if let Some(clocks) = &learning {
                    let mut clocks = clocks.lock();
                    if reported_sample_rate > 0 { clocks.remove(&clock_key); }
                    else { let strikes = clocks.entry(clock_key).or_default(); *strikes = strikes.saturating_add(1); }
                }
                if reported_sample_rate == sample_rate {
                    let validity = match source_controls {
                        Some(controls) => crate::clock_resolution::clock_validity(controls, || {
                            get_programmed_clock_validity(handle, interface_number, clock_id)
                        }),
                        // Preserve the malformed-descriptor compatibility path;
                        // it never authorizes automatic selector changes.
                        None => match get_programmed_clock_validity(handle, interface_number, clock_id) {
                            Ok(true) => Ok(crate::clock_resolution::ClockValidity::ReportedValid),
                            Ok(false) => Err("clock source reports invalid".into()),
                            Err(error) => {
                                log_warn!("[USB] Clock {}: validity capability unknown in malformed descriptor fallback: {}", clock_id, error);
                                Ok(crate::clock_resolution::ClockValidity::Unreported)
                            }
                        },
                    };
                    match validity {
                        Ok(crate::clock_resolution::ClockValidity::ReportedValid) => {}
                        Ok(crate::clock_resolution::ClockValidity::Unreported)
                            if source_controls.is_none() || supported_ranges.is_some() => {
                            log_info!("[USB] Clock {}: validity unreported; exact {} Hz frequency verified", clock_id, sample_rate);
                        }
                        result => {
                            last_readback_error = Some(format!("clock validity not established: {result:?}"));
                            continue;
                        }
                    }
                    dev_eprintln!(
                        "[USB] SET_CUR verified: {} Hz on clock {} interface {} after attempt {}",
                        sample_rate,
                        clock_id,
                        interface_number,
                        attempt,
                    );
                    return ClockApplyOutcome {
                        clock_ok: true,
                        rate_verified: true,
                        reported_sample_rate: Some(reported_sample_rate),
                        known_mismatch: false,
                        set_cur_succeeded,
                        message: None,
                    };
                }
            }
            Err(error) => {
                if error.contains("no device") {
                    set_cur_succeeded = false;
                    last_readback_error = Some(error);
                    break;
                }
                if let Some(clocks) = &learning {
                    let mut clocks = clocks.lock();
                    let strikes = clocks.entry(clock_key).or_default();
                    *strikes = strikes.saturating_add(1);
                }
                last_readback_error = Some(error);
                continue;
            }
        }
    }
    if learning.as_ref().is_some_and(|clocks| clocks.lock().get(&clock_key).copied().unwrap_or(0) >= 3) {
        log_warn!("[USB] Clock interface={} entity={}: three unusable GET_CUR replies; session-scoped readback quirk suggested (unverified tier only)", interface_number, clock_id);
    }

    if let Some(reported_sample_rate) = last_reported_rate {
        return ClockApplyOutcome {
            clock_ok: false,
            rate_verified: false,
            reported_sample_rate: Some(reported_sample_rate),
            known_mismatch: reported_sample_rate > 0 && reported_sample_rate != sample_rate,
            set_cur_succeeded,
            message: Some(match last_readback_error {
                Some(error) if reported_sample_rate == sample_rate => error,
                _ => format!(
                    "Requested {} Hz, DAC reports {} Hz after SET_CUR on clock {} / interface {}",
                    sample_rate, reported_sample_rate, clock_id, interface_number
                ),
            }),
        };
    }

    ClockApplyOutcome {
        clock_ok: false,
        rate_verified: false,
        reported_sample_rate: None,
        known_mismatch: false,
        set_cur_succeeded,
        message: Some(format!(
            "Failed to set USB clock {} on interface {} to {} Hz{}{}",
            clock_id,
            interface_number,
            sample_rate,
            last_set_error
                .as_ref()
                .map(|error| format!(": {}", error))
                .unwrap_or_default(),
            last_readback_error
                .as_ref()
                .map(|error| format!("; readback failed: {}", error))
                .unwrap_or_default(),
        )),
    }
}

/// GET_DESCRIPTOR indexes configurations, rather than using bConfigurationValue.
/// Verify the value as well so preflight never pairs across a configuration change.
pub(super) fn read_active_configuration_bytes(
    handle: &DeviceHandle<Context>,
    config: &ConfigDescriptor,
) -> Option<Vec<u8>> {
    read_active_configuration_bytes_checked(handle, config).ok()
}

/// Preserve transport failures so capture can distinguish detach from bad data.
pub(super) fn read_active_configuration_bytes_checked(
    handle: &DeviceHandle<Context>,
    config: &ConfigDescriptor,
) -> Result<Vec<u8>, Error> {
    let transport_error = |error: rusb::Error| {
        if error == rusb::Error::NoDevice {
            Error::DeviceLost
        } else {
            Error::Control(format!("cannot read active configuration: {error}"))
        }
    };
    let device = handle.device();
    let count = device.device_descriptor().map_err(transport_error)?.num_configurations();
    let mut selected_index = None;
    for index in 0..count {
        match device.config_descriptor(index) {
            Ok(descriptor) if descriptor.number() == config.number() => {
                selected_index = Some(index);
                break;
            }
            Err(rusb::Error::NoDevice) => return Err(Error::DeviceLost),
            _ => {}
        }
    }
    let index = selected_index.ok_or_else(|| {
        Error::Control("requested USB configuration is no longer available".into())
    })?;
    let mut header = [0u8; 9];
    let n = itf_timed_read_control(handle,
            0x80,
            0x06,
            0x0200 | u16::from(index),
            0,
            &mut header,
            Duration::from_secs(1),
        )
        .map_err(transport_error)?;
    if n != header.len() || header[5] != config.number() {
        return Err(Error::Control("active configuration header is short or changed".into()));
    }
    let length = usize::from(u16::from_le_bytes([header[2], header[3]]));
    if length < header.len() {
        return Err(Error::Control("active configuration length is too short".into()));
    }
    let mut bytes = vec![0u8; length];
    let n = itf_timed_read_control(handle,
            0x80,
            0x06,
            0x0200 | u16::from(index),
            0,
            &mut bytes,
            Duration::from_secs(1),
        )
        .map_err(transport_error)?;
    if n != length || bytes[5] != config.number() {
        return Err(Error::Control("active configuration data is short or changed".into()));
    }
    Ok(bytes)
}

/// Strict implicit proof never uses the playback path's sparse-source fallback.
pub(super) fn strict_implicit_clock(
    config: &ConfigDescriptor,
    handle: &DeviceHandle<Context>,
    control_interface: u8,
    terminal: Option<u8>,
) -> Option<ClockChoice> {
    let terminal = terminal?;
    let descriptor = config
        .interfaces()
        .flat_map(|i| i.descriptors())
        .find(|d| {
            d.interface_number() == control_interface
                && d.class_code() == USB_CLASS_AUDIO
                && d.sub_class_code() == USB_SUBCLASS_AUDIOCONTROL
        })?;
    let raw;
    let extra = if descriptor.extra().is_empty() {
        raw = read_audio_control_descriptors_from_raw_config(handle, config, control_interface)?;
        raw.as_slice()
    } else {
        descriptor.extra()
    };
    let graph = ClockGraph::parse(extra);
    let resolved = graph
        .resolve_terminal_clock(terminal, |selector| {
            get_clock_selector_pin(handle, control_interface, selector)
        })
        .ok()?;
    if resolved.assumed_source {
        return None;
    }
    let choice = ClockChoice {
        control_interface,
        source_id: resolved.clock_id,
        path: resolved.hops,
        resolution: crate::clock_resolution::ClockResolution::Resolved,
    };
    prove_uac2_clock_choice(handle, &graph, &choice).ok()
}

// Kept outside SessionState so frozen registration/test fixtures need no new
// fields. USB_SESSION_GATE / SessionGuard serialize the lifecycle operations.
#[derive(Clone)]
struct SelectedClockRoute {
    fd: RawFd,
    control: u8,
    streaming: u8,
    plan: crate::clock_resolution::ClockPlan,
    choice: Option<ClockChoice>,
    frequency: Option<CaptureFrequency>,
}
static SELECTED_CLOCK_ROUTE: Mutex<Option<SelectedClockRoute>> = Mutex::new(None);

/// A unique UAC2 IAD must prove ownership before automatic selector changes.
/// No adjacency or globally repeated terminal IDs may establish this relation.
pub(super) fn owning_clock_graph(
    device: &Device<Context>,
    handle: &DeviceHandle<Context>,
    streaming: u8,
) -> Option<(u8, ClockGraph)> {
    let config = device.active_config_descriptor().ok()?;
    let raw = read_active_configuration_bytes(handle, &config)?;
    let functions = audio_functions(&raw);
    let mut owners = functions.iter().filter(|(_, ids)| ids.contains(&streaming));
    let (control, ids) = owners.next()?;
    if owners.next().is_some() {
        return None;
    }
    let controls: Vec<_> = config
        .interfaces()
        .flat_map(|i| i.descriptors())
        .filter(|d| {
            ids.contains(&d.interface_number())
                && d.class_code() == USB_CLASS_AUDIO
                && d.sub_class_code() == USB_SUBCLASS_AUDIOCONTROL
                && d.setting_number() == 0
                && d.protocol_code() == 0x20
        })
        .collect();
    if controls.len() != 1 || controls[0].interface_number() != *control {
        return None;
    }
    let bytes = audio_control_descriptors_from_config(&raw, *control);
    Some((*control, ClockGraph::parse(&bytes)))
}

/// Read-only discovery allows rate filtering to see sources reachable through
/// safe writable selector paths, without selecting one during enumeration.
/// Limit source probes independently of the pure graph's work budget.
pub(super) fn selectable_clock_ranges(
    device: &Device<Context>,
    handle: &DeviceHandle<Context>,
    streaming: u8,
    terminal: Option<u8>,
) -> Vec<SamplingFrequencySubrange> {
    let Some((control, graph)) = owning_clock_graph(device, handle, streaming) else {
        return Vec::new();
    };
    let Some(terminal) = terminal else {
        return Vec::new();
    };
    let mut reads = 0usize;
    let Ok(plans) = graph.enumerate_plans(terminal, |id| {
        reads += 1;
        if reads > 16 {
            return Err("clock selector read budget exhausted".into());
        }
        get_clock_selector_pin(handle, control, id)
    }) else {
        return Vec::new();
    };
    let mut sources = Vec::new();
    let mut ranges = Vec::new();
    for plan in plans {
        let Ok(choice) = prove_uac2_clock_choice(handle, &graph, &graph.choice_for_plan(control, terminal, &plan)) else { continue; };
        let Ok(ratio) = uac2_clock_ratio(Some(&choice)) else { continue; };
        if sources.contains(&(plan.source_id, ratio)) { continue; }
        sources.push((plan.source_id, ratio));
        if sources.len() > 8 {
            break;
        }
        if let Ok((_, _, found)) = probe_clock_source(handle, control, plan.source_id, plan.source_controls) {
            if verify_uac2_clock_factors(handle, &choice).is_ok() { ranges.extend(project_wire_ranges(ratio, found)); }
        }
    }
    ranges
}

/// Actual CUR/RANGE evidence is required even when validity is unreported.
/// This probe never changes the source frequency or selector route.
fn probe_clock_source(
    handle: &DeviceHandle<Context>,
    control: u8,
    source: u8,
    controls: u32,
) -> Result<(crate::clock_resolution::ClockValidity, u32, Vec<SamplingFrequencySubrange>), String> {
    if !matches!(controls & 3, 1 | 3) {
        return Err(format!("clock {source} sampling frequency is not readable"));
    }
    let validity = crate::clock_resolution::clock_validity(controls, || {
        get_clock_validity(handle, control, source)
    })?;
    let current = get_sampling_frequency(handle, control, source)?;
    if current == 0 {
        return Err(format!("clock {source} returned zero frequency"));
    }
    let ranges = get_sampling_frequency_ranges(handle, control, source)?;
    if ranges.is_empty()
        || ranges.iter().any(|range| range.min == 0 || range.max < range.min || (range.res == 0 && range.min != range.max))
        || !sampling_frequency_ranges_support_rate(&ranges, current)
    {
        return Err(format!("clock {source} current frequency is outside its ranges"));
    }
    Ok((validity, current, ranges))
}

struct UsbClockPlanIo<'a> {
    handle: &'a DeviceHandle<Context>,
    control: u8,
    rate: u32,
    allow_program: bool,
}
impl crate::clock_resolution::ClockPlanIo for UsbClockPlanIo<'_> {
    fn read_pin(&mut self, selector: u8) -> Result<u8, String> {
        get_clock_selector_pin(self.handle, self.control, selector)
    }
    fn write_pin(&mut self, selector: u8, pin: u8) -> Result<(), String> {
        if settings().has(crate::Quirk::SkipClockSelectorWrites) {
            let current=get_clock_selector_pin(self.handle,self.control,selector)?;
            if !selector_write_required(true,current,pin)? {return Ok(());}
        }
        if pin == 0 {
            return Err("clock selector pins are one-based".into());
        }
        let n = itf_timed_write_control(self.handle,
                USB_DIR_OUT | USB_TYPE_CLASS | USB_RECIP_INTERFACE,
                UAC2_REQUEST_SET_CUR,
                UAC2_CLOCK_SELECTOR_CONTROL,
                u16::from(self.control) | (u16::from(selector) << 8),
                &[pin],
                Duration::from_millis(500),
            )
            .map_err(|e| format!("Clock Selector SET_CUR failed: {e}"))?;
        if n != 1 {
            return Err(format!(
                "Clock Selector SET_CUR transferred {n} bytes, expected 1"
            ));
        }
        Ok(())
    }
    fn validate_source(&mut self, source: u8, controls: u32) -> Result<(), String> {
        let (validity, current, ranges) = probe_clock_source(self.handle, self.control, source, controls)?;
        if !sampling_frequency_ranges_support_rate(&ranges, self.rate) {
            return Err(format!("clock source {source} does not support {} Hz", self.rate));
        }
        if validity == crate::clock_resolution::ClockValidity::Unreported {
            if current != self.rate {
                if !self.allow_program || controls & 3 != 3 {
                    return Err(format!("clock {source} has unreported validity and its rate is not verified at {} Hz", self.rate));
                }
                // This is reached only after the complete selector path and
                // session ownership have been checked. Never infer validity
                // from an unsupported CLOCK_VALID request.
                set_sampling_frequency(self.handle, self.control, source, self.rate)?;
                thread::sleep(Duration::from_millis(settings().settle_delay_ms()));
            }
            if get_sampling_frequency(self.handle, self.control, source)? != self.rate {
                return Err(format!("clock {source} with unreported validity failed exact frequency readback"));
            }
            log_info!("[USB] Clock {source}: validity unreported; exact {} Hz frequency verified", self.rate);
        }
        Ok(())
    }
}

/// Called only with streaming alternates at zero and no transfers. Conservative
/// legacy fallback remains available when strict topology cannot be proven.
pub(super) fn select_requested_clock_route(
    claimed: &ClaimedHandle,
    candidate: &IsoStreamCandidate,
    rate: u32,
) -> Result<(), String> {
    let implicit = candidate.implicit_feedback.is_some();
    let device = claimed.handle.device();
    let Some((control, graph)) =
        owning_clock_graph(&device, &claimed.handle, candidate.interface_number)
    else {
        return Ok(());
    };
    let Some(terminal) = candidate.terminal_link else {
        return Ok(());
    };
    // Ordinary implicit pairs retain their original no-mutation fast path.
    // Only a descriptor branch containing a multiplier needs stricter proof.
    if implicit && !graph.terminal_may_use_multiplier(terminal) {
        return Ok(());
    }
    // A malformed or unknown multiplier may not be erased by a first-source
    // fallback when strict enumeration has no usable path.
    let current_multiplier = match graph.resolve_terminal_clock(terminal, |id| get_clock_selector_pin(&claimed.handle, control, id)) {
        Ok(resolved) => resolved.hops.iter().any(|h| matches!(h.kind, ClockHopKind::Multiplier)),
        Err(_) => graph.terminal_may_use_multiplier(terminal),
    };
    if implicit && !current_multiplier {
        return Ok(());
    }
    claimed.ensure_interface_claimed(control)?;
    let mut reads = 0usize;
    let Ok(plans) = graph.enumerate_plans(terminal, |id| {
        reads += 1;
        if reads > 16 {
            return Err("clock selector read budget exhausted".into());
        }
        get_clock_selector_pin(&claimed.handle, control, id)
    }) else {
        return if current_multiplier { Err("no exact readable multiplier clock plan".into()) } else { Ok(()) };
    };
    // Multiplier paths own source-rate mutation here, under the complete
    // function claim. The later generic apply only verifies terminal cadence.
    let mut multiplier_seen = false;
    for plan in plans.iter().take(8) {
        let raw = graph.choice_for_plan(control, terminal, plan);
        if !has_multiplier(Some(&raw)) {
            if implicit { continue; }
            let mut io = UsbClockPlanIo { handle: &claimed.handle, control, rate, allow_program: false };
            let Ok((validity, current, ranges)) = probe_clock_source(&claimed.handle, control, plan.source_id, plan.source_controls) else {
                continue;
            };
            if !sampling_frequency_ranges_support_rate(&ranges, rate) {
                continue;
            }
            let needs_program = validity == crate::clock_resolution::ClockValidity::Unreported && current != rate;
            let owns_idle_function = if needs_program {
                stop_complete_capture_clock_function(claimed, candidate.interface_number)?
            } else if plan.selectors.iter().any(|pin| pin.original != pin.target) {
                stop_owned_clock_alternates(claimed, candidate.interface_number)?
            } else { true };
            if !owns_idle_function {
                // Never alter a clock shared with an unclaimed sibling interface.
                continue;
            }
            let frequency = needs_program.then_some(CaptureFrequency {
                source: plan.source_id, original: current, target: rate,
            });
            // Retain recovery before selectors or a frequency SET can reach the
            // device. A failed rollback must prevent trying another source.
            *SELECTED_CLOCK_ROUTE.lock() = Some(SelectedClockRoute {
                fd: claimed.device_fd, control, streaming: candidate.interface_number,
                plan: plan.clone(), choice: frequency.map(|_| raw.clone()), frequency,
            });
            let outcome = if let Some(frequency) = frequency {
                let mut frequency_io = CaptureClockIo {
                    usb: UsbClockPlanIo { handle: &claimed.handle, control, rate, allow_program: true },
                    expected_frequency: current, program_attempted: false, factors: None, route: plan,
                };
                apply_playback_frequency_plan(&mut frequency_io, plan, frequency)
            } else {
                io.allow_program = false;
                plan.apply(&mut io)
            };
            match outcome {
                Ok(()) => {
                    let exact = graph
                        .resolve_terminal_clock(terminal, |id| {
                            get_clock_selector_pin(&claimed.handle, control, id)
                        })
                        .is_ok_and(|resolved| {
                            resolved.clock_id == plan.source_id && resolved.path == plan.path
                        });
                    *SELECTED_CLOCK_ROUTE.lock() = Some(SelectedClockRoute {
                        fd: claimed.device_fd,
                        control,
                        streaming: candidate.interface_number,
                        plan: plan.clone(),
                        choice: frequency.map(|_| raw.clone()),
                        frequency,
                    });
                    if !exact {
                        restore_requested_clock_route(claimed)?;
                        return Err("clock route changed immediately after selector transaction".into());
                    }
                    return Ok(());
                }
                Err(error) if !error.rollback_errors.is_empty() => return Err(error.to_string()),
                Err(error) => {
                    *SELECTED_CLOCK_ROUTE.lock() = None;
                    log_warn!("[USB] Rejected clock path: {error}");
                }
            }
            continue;
        }
        multiplier_seen = true;
        if implicit && plan.selectors.iter().any(|p| p.original != p.target) { continue; }
        let Ok(choice) = prove_uac2_clock_choice(&claimed.handle, &graph, &raw) else { continue; };
        let Some(source_rate) = uac2_clock_ratio(Some(&choice))?.source_rate(rate) else { continue; };
        let Ok(original) = get_sampling_frequency(&claimed.handle, control, plan.source_id) else { continue; };
        if original == 0 || !matches!(plan.source_controls & 3, 1 | 3) { continue; }
        let mutation = capture_clock_mutates(plan, original, source_rate);
        if mutation {
            let Ok(ranges) = get_sampling_frequency_ranges(&claimed.handle, control, plan.source_id) else { continue; };
            if !capture_ranges_serve(&ranges, source_rate) || !capture_ranges_serve(&ranges, original) { continue; }
            if !stop_complete_capture_clock_function(claimed, candidate.interface_number)? { continue; }
        }
        let frequency = CaptureFrequency { source: plan.source_id, original, target: source_rate };
        let mut io = CaptureClockIo { usb: UsbClockPlanIo { handle: &claimed.handle, control, rate: source_rate, allow_program: mutation }, expected_frequency: original, program_attempted: false, factors: Some(&choice), route: &plan };
        verify_uac2_clock_factors(&claimed.handle, &choice)?;
        if mutation {
            *SELECTED_CLOCK_ROUTE.lock() = Some(SelectedClockRoute { fd: claimed.device_fd, control, streaming: candidate.interface_number, plan: plan.clone(), choice: Some(choice.clone()), frequency: Some(frequency) });
        }
        let outcome = plan.apply(&mut io);
        let factors = verify_uac2_clock_factors(&claimed.handle, &choice);
        if factors.is_err() {
            return Err("multiplier factors changed during clock selection; preserving external state".into());
        }
        match outcome {
            Ok(()) => {
                *SELECTED_CLOCK_ROUTE.lock() = Some(SelectedClockRoute { fd: claimed.device_fd, control, streaming: candidate.interface_number, plan: plan.clone(), choice: Some(choice), frequency: Some(frequency) });
                let exact = graph.resolve_terminal_clock(terminal, |id| get_clock_selector_pin(&claimed.handle, control, id)).is_ok_and(|r| r.clock_id == plan.source_id && r.path == plan.path);
                if !exact { restore_requested_clock_route(claimed)?; return Err("multiplier route drifted after selection".into()); }
                return Ok(());
            }
            Err(error) => {
                let restored = if io.program_attempted { restore_capture_frequency(&mut io, frequency) } else { Ok(()) };
                if !error.rollback_errors.is_empty() || restored.is_err() { return Err(format!("multiplier clock transaction: {error}; frequency rollback: {restored:?}")); }
                if mutation { *SELECTED_CLOCK_ROUTE.lock() = None; }
            }
        }
    }
    if multiplier_seen || current_multiplier {
        // A readable alternate cannot turn the currently unresolved multiplier
        // route into a legacy identity route.
        let current = graph.resolve_terminal_clock(terminal, |id| get_clock_selector_pin(&claimed.handle, control, id)).map_err(|e| e.to_string())?;
        if current.hops.iter().any(|h| matches!(h.kind, ClockHopKind::Multiplier)) {
            let choice = ClockChoice { control_interface: control, source_id: current.clock_id, path: current.hops, resolution: crate::clock_resolution::ClockResolution::Resolved };
            let proven = prove_uac2_clock_choice(&claimed.handle, &graph, &choice)?;
            let clock = AudioControlClock { interface_number: control, clock_id: proven.source_id };
            // Preserve a proven current cadence for NearestSupported fallback.
            // Exact policy is enforced by the caller comparing returned wire Hz.
            get_wire_frequency(&claimed.handle, clock, Some(&proven))?;
            let controls = uac2_source_controls(&claimed.handle, control, proven.source_id).ok_or("missing multiplier source controls")?;
            crate::clock_resolution::clock_validity(controls, || get_clock_validity(&claimed.handle, control, proven.source_id))?;
            if let Some(plan) = plans.iter().find(|p| p.source_id == proven.source_id && p.path == current.path && p.selectors.iter().all(|s| s.original == s.target)) {
                let source = get_sampling_frequency(&claimed.handle, control, proven.source_id)?;
                verify_uac2_clock_factors(&claimed.handle, &proven)?;
                *SELECTED_CLOCK_ROUTE.lock() = Some(SelectedClockRoute { fd: claimed.device_fd, control, streaming: candidate.interface_number, plan: plan.clone(), frequency: Some(CaptureFrequency { source: proven.source_id, original: source, target: source }), choice: Some(proven) });
            }
        }
    }
    Ok(())
}

/// A standalone capture owns its own snapshot; never borrow the pending
/// playback negotiation slot, even when both sessions use the same FD.
#[derive(Clone)]
pub(super) struct CaptureClockRoute {
    control: u8,
    streaming: u8,
    plan: crate::clock_resolution::ClockPlan,
    frequency: CaptureFrequency,
    choice: ClockChoice,
}
impl CaptureClockRoute {
    pub(super) fn expected_active_source(&self) -> (&ClockChoice, u32) {
        (&self.choice, self.frequency.target)
    }
}
#[derive(Clone, Copy)]
struct CaptureFrequency { source: u8, original: u32, target: u32 }
trait CaptureFrequencyIo {
    fn read_frequency(&mut self, source: u8) -> Result<u32, String>;
    fn write_frequency(&mut self, source: u8, rate: u32) -> Result<(), String>;
}
struct CaptureClockIo<'a> { usb: UsbClockPlanIo<'a>, expected_frequency: u32, program_attempted: bool, factors: Option<&'a ClockChoice>, route: &'a crate::clock_resolution::ClockPlan }
impl CaptureFrequencyIo for CaptureClockIo<'_> {
    fn read_frequency(&mut self, source: u8) -> Result<u32, String> {
        if let Some(choice) = self.factors { verify_uac2_clock_factors(self.usb.handle, choice)?; }
        get_sampling_frequency(self.usb.handle, self.usb.control, source)
    }
    fn write_frequency(&mut self, source: u8, rate: u32) -> Result<(), String> {
        if let Some(choice) = self.factors { verify_uac2_clock_factors(self.usb.handle, choice)?; }
        set_sampling_frequency(self.usb.handle, self.usb.control, source, rate)?;
        thread::sleep(Duration::from_millis(settings().settle_delay_ms()));
        Ok(())
    }
}
impl crate::clock_resolution::ClockPlanIo for CaptureClockIo<'_> {
    fn read_pin(&mut self, selector: u8) -> Result<u8, String> {
        crate::clock_resolution::ClockPlanIo::read_pin(&mut self.usb, selector)
    }
    fn write_pin(&mut self, selector: u8, pin: u8) -> Result<(), String> {
        if let Some(choice) = self.factors { verify_uac2_clock_factors(self.usb.handle, choice)?; }
        if let Some(saved) = self.route.selectors.iter().find(|p| p.selector == selector) {
            let actual = get_clock_selector_pin(self.usb.handle, self.usb.control, selector)?;
            if actual != saved.original && actual != saved.target { return Err("selector drifted; preserving external change".into()); }
        }
        crate::clock_resolution::ClockPlanIo::write_pin(&mut self.usb, selector, pin)
    }
    fn validate_source(&mut self, source: u8, controls: u32) -> Result<(), String> {
        // A working exact current route needs no RANGE request. Any mutation,
        // including a selector-only alternative, retains the strict RANGE proof.
        if !matches!(controls & 3, 1 | 3) {
            return Err("capture clock frequency is not readable".into());
        }
        if self.usb.allow_program {
            let ranges = get_sampling_frequency_ranges(self.usb.handle, self.usb.control, source)?;
            if !capture_ranges_serve(&ranges, self.usb.rate)
                || !capture_ranges_serve(&ranges, self.expected_frequency) {
                return Err("capture clock does not support the exact requested rate".into());
            }
        }
        let before = self.read_frequency(source)?;
        if before != self.expected_frequency {
            return Err("capture source frequency drifted before programming".into());
        }
        let programmed = before != self.usb.rate;
        if programmed {
            if !self.usb.allow_program || controls & 3 != 3 {
                return Err("capture clock frequency is not writable at the requested rate".into());
            }
            self.program_attempted = true;
            self.write_frequency(source, self.usb.rate)?;
        }
        if self.read_frequency(source)? != self.usb.rate {
            return Err("capture clock frequency readback did not match".into());
        }
        crate::clock_resolution::clock_validity(controls, || {
            if programmed {
                get_programmed_clock_validity(self.usb.handle, self.usb.control, source)
            } else {
                get_clock_validity(self.usb.handle, self.usb.control, source)
            }
        })?;
        Ok(())
    }
}
fn capture_clock_mutates(plan: &crate::clock_resolution::ClockPlan, current: u32, requested: u32) -> bool {
    current != requested || plan.selectors.iter().any(|pin| pin.original != pin.target)
}
fn capture_ranges_serve(ranges: &[SamplingFrequencySubrange], rate: u32) -> bool {
    !ranges.is_empty()
        && !ranges.iter().any(|range| range.min == 0 || range.max < range.min
            || (range.res == 0 && range.min != range.max))
        && sampling_frequency_ranges_support_rate(ranges, rate)
}
fn restore_capture_frequency(io: &mut impl CaptureFrequencyIo, saved: CaptureFrequency) -> Result<(), String> {
    if saved.original == saved.target { return Ok(()); }
    let actual = io.read_frequency(saved.source)?;
    if actual == saved.original { return Ok(()); }
    if actual != saved.target {
        return Err("capture frequency drifted; preserving external change".into());
    }
    io.write_frequency(saved.source, saved.original)?;
    if io.read_frequency(saved.source)? != saved.original {
        return Err("capture original frequency did not verify".into());
    }
    Ok(())
}
/// ClockPlan rolls back pins after a failed source check. Restore a possibly
/// effective frequency SET as well, including short writes and readback errors.
fn apply_playback_frequency_plan(
    io: &mut (impl crate::clock_resolution::ClockPlanIo + CaptureFrequencyIo),
    plan: &crate::clock_resolution::ClockPlan,
    frequency: CaptureFrequency,
) -> Result<(), crate::clock_resolution::ClockPlanError> {
    plan.apply(io).map_err(|mut error| {
        if let Err(restore) = restore_capture_frequency(io, frequency) {
            error.rollback_errors.push(format!("frequency: {restore}"));
        }
        error
    })
}

fn restore_capture_transaction(
    io: &mut (impl crate::clock_resolution::ClockPlanIo + CaptureFrequencyIo),
    plan: &crate::clock_resolution::ClockPlan, frequency: CaptureFrequency,
) -> Result<(), String> {
    // Check every pin before touching frequency: an external route change owns
    // the resulting state and must not be silently repaired by capture Drop.
    for pin in &plan.selectors {
        if io.read_pin(pin.selector)? != pin.target {
            return Err("capture selector drifted; preserving external change".into());
        }
    }
    restore_capture_frequency(io, frequency)?;
    plan.restore(io).map_err(|error| error.to_string())
}

fn finish_capture_clock_admission(
    available: Result<bool, String>,
    saved: &mut Option<CaptureClockRoute>,
    restore: impl FnOnce(&CaptureClockRoute) -> Result<(), String>,
) -> Result<bool, String> {
    if matches!(available, Ok(true)) {
        return saved.as_ref().map(|_| true).ok_or_else(|| "missing selected capture clock snapshot".into());
    }
    let restored = saved.as_ref().ok_or_else(|| "missing capture clock hand-back snapshot".to_string())
        .and_then(restore);
    if restored.is_ok() { *saved = None; }
    match (available, restored) {
        (Ok(false), Ok(())) => Ok(false),
        (Err(error), Ok(())) => Err(format!("{error}; capture clock route restored")),
        (Err(error), Err(restore_error)) => Err(format!("{error}; capture clock restoration uncertain: {restore_error}")),
        (Ok(false), Err(restore_error)) => Err(format!("capture alternate unavailable; capture clock restoration uncertain: {restore_error}")),
        (Ok(true), _) => unreachable!(),
    }
}

/// Prefer the current exact route, then bounded writable alternatives. The
/// caller retains even an uncertain transaction until capture ownership ends.
pub(super) fn select_capture_clock_route(
    claimed: &ClaimedHandle, streaming: u8, alternate: u8, terminal: u8, control: u8, rate: u32,
    saved: &mut Option<CaptureClockRoute>,
) -> Result<bool, String> {
    let device = claimed.handle.device();
    let (owner, graph) = owning_clock_graph(&device, &claimed.handle, streaming)
        .ok_or("capture clock fallback requires a unique active UAC2 function")?;
    if owner != control { return Err("capture clock owner changed".into()); }
    let mut reads = 0;
    let plans = graph.enumerate_plans(terminal, |id| {
        reads += 1;
        if reads > 16 { return Err("capture selector read budget exhausted".into()); }
        get_clock_selector_pin(&claimed.handle, control, id)
    })?;
    let mut last = "no capture source serves the exact requested rate".to_string();
    for plan in plans.into_iter().take(8) {
        let Ok(choice) = prove_uac2_clock_choice(&claimed.handle, &graph, &graph.choice_for_plan(control, terminal, &plan)) else { continue; };
        let Some(source_rate) = uac2_clock_ratio(Some(&choice))?.source_rate(rate) else { continue; };
        if !matches!(plan.source_controls & 3, 1 | 3) { continue; }
        let Ok(original) = get_sampling_frequency(&claimed.handle, control, plan.source_id) else { continue; };
        if original == 0 { continue; }
        let mutation = capture_clock_mutates(&plan, original, source_rate);
        if mutation {
            let Ok(ranges) = get_sampling_frequency_ranges(&claimed.handle, control, plan.source_id) else { continue; };
            if !capture_ranges_serve(&ranges, source_rate)
                || !capture_ranges_serve(&ranges, original) { continue; }
            if !stop_complete_capture_clock_function(claimed, streaming)? { continue; }
        }
        let frequency = CaptureFrequency { source: plan.source_id, original, target: source_rate };
        let mut io = CaptureClockIo { usb: UsbClockPlanIo { handle: &claimed.handle, control, rate: source_rate, allow_program: mutation }, expected_frequency: original, program_attempted: false, factors: Some(&choice), route: &plan };
        *saved = Some(CaptureClockRoute { control, streaming, plan: plan.clone(), frequency, choice: choice.clone() });
        verify_uac2_clock_factors(&claimed.handle, &choice)?;
        let outcome = plan.apply(&mut io);
        verify_uac2_clock_factors(&claimed.handle, &choice)?;
        match outcome {
            Ok(()) => {
                let resolved = graph.resolve_terminal_clock(terminal, |id| {
                    get_clock_selector_pin(&claimed.handle, control, id)
                }).map_err(|e| e.to_string())?;
                if resolved.clock_id != plan.source_id || resolved.path != plan.path {
                    return Err("capture clock route changed after selection".into());
                }
                // A valid bitmap rejection belongs to this AS alternate, not
                // to clock-plan search. Hand back before the caller tries its
                // next alternate, without forcing another selector route.
                return finish_capture_clock_admission(
                    claimed.alternate_is_available(streaming, alternate), saved,
                    |route| restore_capture_clock_route(claimed, route),
                );
            }
            Err(error) => {
                // ClockPlan has already rolled its attempted pins back. Restore
                // the addressed source frequency before considering another path.
                let restored = if io.program_attempted {
                    restore_capture_frequency(&mut io, frequency)
                } else { Ok(()) };
                if !io.program_attempted {
                    if let Some(saved) = saved.as_mut() { saved.frequency.target = original; }
                }
                if !error.rollback_errors.is_empty() || restored.is_err() {
                    return Err(format!("capture clock transaction: {error}; frequency rollback: {restored:?}"));
                }
                *saved = None;
                last = error.to_string();
            }
        }
    }
    Err(last)
}
pub(super) fn stop_complete_capture_clock_function(claimed: &ClaimedHandle, streaming: u8) -> Result<bool, String> {
    let config = claimed.handle.device().active_config_descriptor().map_err(|e| e.to_string())?;
    let raw = read_active_configuration_bytes(&claimed.handle, &config).ok_or("cannot re-read capture function")?;
    let functions = audio_functions(&raw);
    let mut owners = functions.iter().filter(|(_, ids)| ids.contains(&streaming));
    let Some((_, ids)) = owners.next() else { return Ok(false); };
    if owners.next().is_some() || ids.iter().any(|id| !claimed.claimed_interfaces.contains(id)) {
        return Ok(false);
    }
    stop_owned_clock_alternates(claimed, streaming)
}
pub(super) fn restore_capture_clock_route(claimed: &ClaimedHandle, saved: &CaptureClockRoute) -> Result<(), String> {
    verify_uac2_clock_factors(&claimed.handle, &saved.choice)?;
    let mutation = saved.frequency.original != saved.frequency.target
        || saved.plan.selectors.iter().any(|pin| pin.original != pin.target);
    if !mutation { return Ok(()); }
    if !stop_complete_capture_clock_function(claimed, saved.streaming)? {
        return Err("capture clock hand-back requires complete idle function ownership".into());
    }
    let mut io = CaptureClockIo { usb: UsbClockPlanIo {
        handle: &claimed.handle, control: saved.control, rate: saved.frequency.target, allow_program: false,
    }, expected_frequency: saved.frequency.target, program_attempted: false, factors: Some(&saved.choice), route: &saved.plan };
    restore_capture_transaction(&mut io, &saved.plan, saved.frequency)?;
    verify_uac2_clock_factors(&claimed.handle, &saved.choice)
}

/// A negotiated plan's selected pins must remain identical until open; never
/// silently repair drift on behalf of an old StreamFormat.
pub(super) fn verify_requested_clock_route(
    claimed: &ClaimedHandle,
    rate: u32,
) -> Result<(), String> {
    let selected = SELECTED_CLOCK_ROUTE.lock().clone();
    let Some(selected) = selected.filter(|r| r.fd == claimed.device_fd) else {
        return Ok(());
    };
    let rate = if let Some(choice) = selected.choice.as_ref() {
        verify_uac2_clock_factors(&claimed.handle, choice)?;
        let source = uac2_clock_ratio(Some(choice))?.source_rate(rate).ok_or("requested rate is not exact at clock source")?;
        if get_sampling_frequency(&claimed.handle, selected.control, choice.source_id)? != source { return Err("negotiated multiplier source frequency drifted".into()); }
        verify_uac2_clock_factors(&claimed.handle, choice)?;
        source
    } else { rate };
    let mut current = selected.plan;
    for pin in &mut current.selectors {
        pin.original = pin.target;
    }
    current
        .apply(&mut UsbClockPlanIo {
            handle: &claimed.handle,
            control: selected.control,
            rate,
            allow_program: false,
        })
        .map_err(|e| format!("negotiated clock route changed or became invalid: {e}"))
}

/// Restore the first saved selector state before renegotiation or hand-back.
/// Leave the slot populated on failure so a later safe hand-back can retry.
pub(super) fn restore_requested_clock_route(claimed: &ClaimedHandle) -> Result<(), String> {
    let mut slot = SELECTED_CLOCK_ROUTE.lock();
    let Some(selected) = slot.as_ref().filter(|r| r.fd == claimed.device_fd) else {
        return Ok(());
    };
    if let (Some(choice), Some(frequency)) = (&selected.choice, selected.frequency) {
        verify_uac2_clock_factors(&claimed.handle, choice)?;
        let mutation = capture_clock_mutates(&selected.plan, frequency.original, frequency.target);
        if mutation && !stop_complete_capture_clock_function(claimed, selected.streaming)? {
            return Err("clock frequency hand-back requires complete idle function ownership".into());
        }
        let mut io = CaptureClockIo { usb: UsbClockPlanIo { handle: &claimed.handle, control: selected.control, rate: frequency.target, allow_program: false }, expected_frequency: frequency.target, program_attempted: false, factors: Some(choice), route: &selected.plan };
        restore_capture_transaction(&mut io, &selected.plan, frequency)?;
        verify_uac2_clock_factors(&claimed.handle, choice)?;
        *slot = None;
        return Ok(());
    }
    if selected
        .plan
        .selectors
        .iter()
        .any(|pin| pin.original != pin.target)
        && !stop_owned_clock_alternates(claimed, selected.streaming)?
    {
        return Err("cannot restore clock route while a sibling interface is unowned".into());
    }
    selected
        .plan
        .restore(&mut UsbClockPlanIo {
            handle: &claimed.handle,
            control: selected.control,
            rate: 0,
            allow_program: false,
        })
        .map_err(|e| e.to_string())?;
    *slot = None;
    Ok(())
}

pub(super) fn finish_requested_clock_route() {
    let Some(device) = SESSION_STATE.lock().as_ref().map(|s| s.device.clone()) else {
        return;
    };
    if SELECTED_CLOCK_ROUTE
        .lock()
        .as_ref()
        .map_or(true, |r| r.fd != device.fd)
    {
        return;
    }
    match open_claimed_usb_handle(&device) {
        Ok(claimed) => {
            if let Err(error) = restore_requested_clock_route(&claimed) {
                log_warn!("[USB] Original clock selectors not restored: {error}");
            }
            cleanup_claimed_handle(claimed, None, false);
        }
        Err(error) => log_warn!("[USB] Cannot open clock route for hand-back: {error}"),
    }
    // Registration is ending, so never carry device-local IDs to a reused fd.
    *SELECTED_CLOCK_ROUTE.lock() = None;
}

/// Stop every streaming alternate in the proven function before changing its
/// clock tree. Missing ownership of even one sibling makes mutation unsafe.
fn stop_owned_clock_alternates(claimed: &ClaimedHandle, streaming: u8) -> Result<bool, String> {
    let device = claimed.handle.device();
    let config = device
        .active_config_descriptor()
        .map_err(|e| e.to_string())?;
    let Some(raw) = read_active_configuration_bytes(&claimed.handle, &config) else {
        return Ok(false);
    };
    let functions = audio_functions(&raw);
    let mut owners = functions.iter().filter(|(_, ids)| ids.contains(&streaming));
    let Some((_, ids)) = owners.next() else {
        return Ok(false);
    };
    if owners.next().is_some() {
        return Ok(false);
    }
    let siblings: HashSet<_> = config
        .interfaces()
        .flat_map(|i| i.descriptors())
        .filter(|d| {
            ids.contains(&d.interface_number())
                && d.class_code() == USB_CLASS_AUDIO
                && d.sub_class_code() == USB_SUBCLASS_AUDIOSTREAMING
        })
        .map(|d| d.interface_number())
        .collect();
    if siblings
        .iter()
        .any(|id| !claimed.claimed_interfaces.contains(id))
    {
        return Ok(false);
    }
    for sibling in siblings {
        claimed.set_alternate_setting(sibling, 0)?;
    }
    Ok(true)
}

// Preserve the clock planner and control results; only the exact ITF device
// models add the documented post-class-control delay, including error paths.
fn itf_timed_read_control(handle: &DeviceHandle<Context>, request_type: u8, request: u8,
                          value: u16, index: u16, data: &mut [u8], timeout: Duration) -> rusb::Result<usize> {
    let result = handle.read_control(request_type, request, value, index, data, timeout);
    if request_type & 0x60 == USB_TYPE_CLASS { itf_class_control_delay(handle); }
    result
}
fn itf_timed_write_control(handle: &DeviceHandle<Context>, request_type: u8, request: u8,
                           value: u16, index: u16, data: &[u8], timeout: Duration) -> rusb::Result<usize> {
    let result = handle.write_control(request_type, request, value, index, data, timeout);
    if request_type & 0x60 == USB_TYPE_CLASS { itf_class_control_delay(handle); }
    result
}

#[cfg(test)]
mod capture_clock_transaction_tests {
    use super::*;
    use crate::clock_resolution::{ClockPinPlan, ClockPlan, ClockPlanIo};

    struct Io { pin: u8, rate: u32, writes: Vec<&'static str> }
    impl ClockPlanIo for Io {
        fn read_pin(&mut self, _: u8) -> Result<u8, String> { Ok(self.pin) }
        fn write_pin(&mut self, _: u8, pin: u8) -> Result<(), String> {
            self.writes.push("pin"); self.pin = pin; Ok(())
        }
        fn validate_source(&mut self, _: u8, _: u32) -> Result<(), String> { Ok(()) }
    }
    impl CaptureFrequencyIo for Io {
        fn read_frequency(&mut self, _: u8) -> Result<u32, String> { Ok(self.rate) }
        fn write_frequency(&mut self, _: u8, rate: u32) -> Result<(), String> {
            self.writes.push("rate"); self.rate = rate; Ok(())
        }
    }
    fn plan() -> ClockPlan {
        ClockPlan { source_id: 9, source_controls: 7, path: vec![8, 9],
            selectors: vec![ClockPinPlan { selector: 8, original: 1, target: 2 }] }
    }
    fn frequency() -> CaptureFrequency {
        CaptureFrequency { source: 9, original: 44_100, target: 48_000 }
    }
    fn route() -> CaptureClockRoute {
        CaptureClockRoute { control: 0, streaming: 1, plan: plan(), frequency: frequency(),
            choice: ClockChoice { control_interface: 0, source_id: 9, path: vec![],
                resolution: crate::clock_resolution::ClockResolution::Resolved } }
    }
    #[test]
    fn ordinary_playback_frequency_failure_restores_rate_and_blocks_uncertain_retry() {
        // Reuse the transaction IO, with source validation simulating a SET
        // that took effect before a short/error reply or failed readback.
        struct FailedSource { io: Io, drift: bool }
        impl ClockPlanIo for FailedSource {
            fn read_pin(&mut self, id: u8) -> Result<u8, String> { self.io.read_pin(id) }
            fn write_pin(&mut self, id: u8, pin: u8) -> Result<(), String> { self.io.write_pin(id, pin) }
            fn validate_source(&mut self, source: u8, _: u32) -> Result<(), String> {
                self.io.write_frequency(source, 48_000)?;
                if self.drift { self.io.rate = 96_000; }
                Err("frequency SET failed or readback did not verify".into())
            }
        }
        impl CaptureFrequencyIo for FailedSource {
            fn read_frequency(&mut self, source: u8) -> Result<u32, String> { self.io.read_frequency(source) }
            fn write_frequency(&mut self, source: u8, rate: u32) -> Result<(), String> { self.io.write_frequency(source, rate) }
        }
        for drift in [false, true] {
            let mut io = FailedSource { io: Io { pin: 1, rate: 44_100, writes: vec![] }, drift };
            let error = apply_playback_frequency_plan(&mut io, &plan(), frequency()).unwrap_err();
            assert!(error.reason.contains("frequency SET"));
            assert_eq!(io.io.pin, 1);
            if drift {
                assert_eq!(io.io.rate, 96_000);
                assert!(!error.rollback_errors.is_empty());
                assert_eq!(io.io.writes, ["pin", "rate", "pin"]);
            } else {
                assert_eq!(io.io.rate, 44_100);
                assert!(error.rollback_errors.is_empty());
                assert_eq!(io.io.writes, ["pin", "rate", "pin", "rate"]);
            }
        }
    }
    #[test]
    fn ordinary_playback_frequency_hand_back_uses_saved_rate() {
        let mut io = Io { pin: 2, rate: 48_000, writes: vec![] };
        restore_capture_transaction(&mut io, &plan(), frequency()).unwrap();
        assert_eq!((io.pin, io.rate), (1, 44_100));
        assert_eq!(io.writes, ["rate", "pin"]);
    }
    #[test]
    fn available_capture_alternate_keeps_selected_clock_snapshot() {
        let mut saved = Some(route());
        assert_eq!(finish_capture_clock_admission(Ok(true), &mut saved, |_| {
            panic!("available alternate must keep its selected clock")
        }), Ok(true));
        let (choice, source_rate) = saved.as_ref().unwrap().expected_active_source();
        assert_eq!((choice.source_id, source_rate), (9, 48_000));
    }
    #[test]
    fn unavailable_capture_alternate_restores_before_returning_false() {
        let mut saved = Some(route());
        let mut io = Io { pin: 2, rate: 48_000, writes: vec![] };
        assert_eq!(finish_capture_clock_admission(Ok(false), &mut saved, |route| {
            restore_capture_transaction(&mut io, &route.plan, route.frequency)
        }), Ok(false));
        assert!(saved.is_none());
        assert_eq!(io.writes, ["rate", "pin"]);
        assert_eq!((io.pin, io.rate), (1, 44_100));
    }
    #[test]
    fn capture_alternate_control_failure_restores_and_preserves_error() {
        let mut saved = Some(route());
        let mut io = Io { pin: 2, rate: 48_000, writes: vec![] };
        let error = finish_capture_clock_admission(Err("malformed alternate bitmap".into()), &mut saved, |route| {
            restore_capture_transaction(&mut io, &route.plan, route.frequency)
        }).unwrap_err();
        assert!(error.contains("malformed alternate bitmap"));
        assert!(saved.is_none());
        assert_eq!((io.pin, io.rate), (1, 44_100));
    }
    #[test]
    fn failed_capture_admission_hand_back_retains_snapshot_and_device_loss() {
        for available in [Ok(false), Err("No such device".into())] {
            let mut saved = Some(route());
            let device_lost = available.is_err();
            let mut io = Io { pin: 3, rate: 48_000, writes: vec![] };
            let error = finish_capture_clock_admission(available, &mut saved, |route| {
                restore_capture_transaction(&mut io, &route.plan, route.frequency)
            }).unwrap_err();
            assert!(saved.is_some());
            assert!(io.writes.is_empty());
            assert!(error.contains("restoration uncertain"));
            assert_eq!(crate::error::is_device_gone(&error), device_lost);
        }
    }
    #[test]
    fn exact_current_route_needs_no_mutation_but_alternatives_do() {
        let mut selected = plan();
        selected.selectors[0].target = selected.selectors[0].original;
        assert!(!capture_clock_mutates(&selected, 48_000, 48_000));
        assert!(capture_clock_mutates(&selected, 44_100, 48_000));
        assert!(capture_clock_mutates(&plan(), 48_000, 48_000));
    }
    #[test]
    fn capture_hand_back_restores_frequency_before_selector() {
        let mut io = Io { pin: 2, rate: 48_000, writes: vec![] };
        restore_capture_transaction(&mut io, &plan(), frequency()).unwrap();
        assert_eq!(io.writes, ["rate", "pin"]);
        assert_eq!((io.pin, io.rate), (1, 44_100));
    }
    #[test]
    fn capture_hand_back_preserves_external_pin_or_frequency_drift() {
        for (pin, rate) in [(3, 48_000), (2, 96_000)] {
            let mut io = Io { pin, rate, writes: vec![] };
            assert!(restore_capture_transaction(&mut io, &plan(), frequency()).is_err());
            assert!(io.writes.is_empty());
            assert_eq!((io.pin, io.rate), (pin, rate));
        }
    }
    #[test]
    fn capture_already_restored_frequency_is_not_programmed_again() {
        let mut io = Io { pin: 2, rate: 44_100, writes: vec![] };
        restore_capture_transaction(&mut io, &plan(), frequency()).unwrap();
        assert_eq!(io.writes, ["pin"]);
    }
    #[test]
    fn motu_validity_retry_is_bounded_and_aborts_control_errors() {
        let mut reads = 0;
        let mut waits = 0;
        assert_eq!(retry_programmed_clock_validity(true, || {
            reads += 1; Ok(reads == 3)
        }, || waits += 1), Ok(true));
        assert_eq!((reads, waits), (3, 2));
        let mut waits = 0;
        assert!(retry_programmed_clock_validity(true, || Err("disconnect".into()), || waits += 1).is_err());
        assert_eq!(waits, 0);
        let mut waits = 0;
        assert_eq!(retry_programmed_clock_validity(true, || Ok(false), || waits += 1), Ok(false));
        assert_eq!(waits, 50);
        let mut waits = 0;
        assert_eq!(retry_programmed_clock_validity(false, || Ok(false), || waits += 1), Ok(false));
        assert_eq!(waits, 0);
    }
}

#[cfg(test)]
mod uac2_multiplier_control_tests {
    use super::*;
    #[test]
    fn multiplier_reads_two_exact_little_endian_positive_controls() {
        let mut selectors = Vec::new();
        let factors = read_multiplier_factors(|selector, bytes| {
            selectors.push(selector);
            bytes.copy_from_slice(if selector == 1 { &[0x34,0x12] } else { &[0x78,0x56] });
            Ok(2)
        }).unwrap();
        assert_eq!(selectors, vec![1,2]);
        assert_eq!(factors, (0x1234,0x5678));
        for count in [0,1,3] {
            assert!(read_multiplier_factors(|_, bytes| { bytes[0] = 1; Ok(count) }).is_err());
        }
        assert!(read_multiplier_factors(|_, _| Ok(2)).is_err());
        assert!(read_multiplier_factors(|_, _| Err("stall".into())).is_err());
    }
    #[test]
    fn unknown_ratios_fail_closed_and_raw_identity_remains_multiplier_backed() {
        let mut choice = ClockChoice { control_interface: 0, source_id: 9,
            path: vec![crate::clock_resolution::ClockHop { id: 8, kind: ClockHopKind::Multiplier }],
            resolution: crate::clock_resolution::ClockResolution::Resolved };
        assert!(uac2_clock_ratio(Some(&choice)).is_err());
        choice.path[0].kind = ClockHopKind::RateMultiplier { numerator: 2, denominator: 2 };
        assert_eq!(uac2_clock_ratio(Some(&choice)).unwrap(), RateRatio::IDENTITY);
        assert!(has_multiplier(Some(&choice)));
        choice.path[0].kind = ClockHopKind::RateMultiplier { numerator: 3, denominator: 2 };
        let projected = project_wire_ranges(uac2_clock_ratio(Some(&choice)).unwrap(), vec![SamplingFrequencySubrange { min: 48000, max: 96000, res: 48000 }]);
        assert_eq!(projected[0].min, 72000);
        assert_eq!(projected[0].max, 144000);
        assert_eq!(projected[0].res, 72000);
    }
}

#[cfg(test)]
mod uac1_playback_rate_tests {
    use super::*;

    fn fixed() -> Vec<SamplingFrequencySubrange> {
        vec![SamplingFrequencySubrange { min: 48_000, max: 48_000, res: 0 }]
    }
    fn endpoint(control: u8) -> [u8; 7] { [7, 0x25, 1, control, 0, 0, 0] }

    #[test]
    fn uac1_control_bit_is_endpoint_local_and_exact() {
        assert_eq!(uac1_rate_profile(&endpoint(0), SyncType::Asynchronous, &fixed(), 48_000).sampling_control, Some(false));
        assert_eq!(uac1_rate_profile(&endpoint(1), SyncType::Asynchronous, &fixed(), 48_000).sampling_control, Some(true));
        // Pitch Control D1 does not imply Sampling Frequency Control D0.
        assert_eq!(uac1_rate_profile(&endpoint(2), SyncType::Asynchronous, &fixed(), 48_000).sampling_control, Some(false));
        for extra in [vec![], vec![6, 0x25, 1, 0, 0, 0], vec![7, 0x25, 1, 0],
            [endpoint(0), endpoint(1)].concat(), vec![7, 0x25, 1, 0, 0, 0, 0, 1]] {
            assert_eq!(uac1_rate_profile(&extra, SyncType::Synchronous, &fixed(), 48_000).sampling_control, None);
        }
    }

    #[test]
    fn uac1_wire_control_requires_exact_positive_signed_24_bit_hz() {
        assert_eq!(decode_uac1_rate([0x80, 0xbb, 0], 3).unwrap(), 48_000);
        assert_eq!(decode_uac1_rate([0xff, 0xff, 0x7f], 3).unwrap(), 0x7f_ffff);
        for bytes in [[0, 0, 0], [0, 0, 0x80], [0xff, 0xff, 0xff]] {
            assert!(decode_uac1_rate(bytes, 3).is_err());
        }
        for length in [0, 1, 2, 4] {
            assert!(decode_uac1_rate([0x80, 0xbb, 0], length).is_err());
            assert!(check_uac1_write(length).is_err());
        }
        assert!(check_uac1_write(3).is_ok());
    }

    #[test]
    fn uac1_readback_overrides_descriptor_evidence_and_ack_is_not_evidence() {
        let fixed = uac1_rate_profile(&endpoint(0), SyncType::Asynchronous, &fixed(), 48_000);
        assert!(uac1_playback_rate_verified(48_000, None, fixed, true));
        assert!(!uac1_playback_rate_verified(48_000, None, fixed, false));
        assert!(!uac1_playback_rate_verified(48_000, Some(44_100), fixed, true));
        assert!(!uac1_playback_rate_verified(48_000, Some(0), fixed, true));
        for sampling_control in [None, Some(true)] {
            let profile = Uac1PlaybackRateProfile { sampling_control, ..fixed };
            assert!(!uac1_playback_rate_verified(48_000, None, profile, true));
            assert!(uac1_playback_rate_verified(48_000, Some(48_000), profile, true));
        }
        assert!(!uac1_playback_rate_verified(0, Some(0), fixed, true));
    }

    #[test]
    fn uac1_host_pacing_requires_declared_supported_rate() {
        for res in [0, 1] {
            let ranges = [SamplingFrequencySubrange { min: 44_100, max: 96_000, res }];
            for sync in [SyncType::Synchronous, SyncType::Adaptive] {
                let profile = uac1_rate_profile(&endpoint(0), sync, &ranges, 48_000);
                assert!(!profile.fixed_at_request);
                assert!(profile.host_paced_at_request);
                assert!(uac1_playback_rate_verified(48_000, None, profile, true));
            }
            assert!(!uac1_rate_profile(&endpoint(0), SyncType::Asynchronous, &ranges, 48_000).host_paced_at_request);
            assert!(!uac1_rate_profile(&endpoint(0), SyncType::Synchronous, &ranges, 192_000).host_paced_at_request);
        }
        assert!(!uac1_rate_profile(&endpoint(0), SyncType::Synchronous, &[], 48_000).host_paced_at_request);
    }

    #[test]
    fn uac1_partial_or_nonstandard_as_descriptors_cannot_prove_fixed_rate() {
        let general = [7, 0x24, 1, 1, 0, 1, 0];
        let single = [11, 0x24, 2, 1, 2, 2, 16, 1, 0x80, 0xbb, 0];
        assert!(uac1_complete_rate_descriptor(&[general.as_slice(), single.as_slice()].concat()));
        let mut truncated_list = single;
        truncated_list[7] = 2;
        assert!(!uac1_complete_rate_descriptor(&[general.as_slice(), truncated_list.as_slice()].concat()));
        assert!(!uac1_complete_rate_descriptor(&single));
        assert!(!uac1_complete_rate_descriptor(&[general.as_slice(), general.as_slice(), single.as_slice()].concat()));
        let continuous = [14, 0x24, 2, 1, 2, 2, 16, 0, 0x44, 0xac, 0, 0, 0x77, 1];
        assert!(uac1_complete_rate_descriptor(&[general.as_slice(), continuous.as_slice()].concat()));
    }
}

#[cfg(test)]
mod source_uac1_attribute_tests {
    use super::*;
    #[test]
    fn sampling_control_corrections_require_exact_identity_and_cannot_supply_rate_evidence() {
        assert_eq!(uac1_sampling_attribute_correction(0x0a92,0x0053),Some(false));
        assert_eq!(uac1_sampling_attribute_correction(0x041e,0x3020),Some(true));
        assert_eq!(uac1_sampling_attribute_correction(0x0763,0x2003),Some(true));
        assert_eq!(uac1_sampling_attribute_correction(0x0763,0x2004),None);
        let profile=Uac1PlaybackRateProfile { sampling_control:Some(true),fixed_at_request:false,host_paced_at_request:false };
        assert!(!uac1_playback_rate_verified(48_000,None,profile,true));
        assert!(!uac1_playback_rate_verified(48_000,Some(44_100),profile,true));
        assert!(uac1_playback_rate_verified(48_000,Some(48_000),profile,true));
    }
}

#[cfg(test)]
mod fixed_rate_source_tests {
    use super::*;
    #[test]
    fn fixed_rate_write_skip_requires_independent_live_and_descriptor_proof() {
        assert!(fixed_rate_live_proof(48_000,Some(48_000),true,true));
        for args in [(Some(44_100),true,true),(None,true,true),(Some(48_000),false,true),(Some(48_000),true,false)] {
            assert!(!fixed_rate_live_proof(48_000,args.0,args.1,args.2));
        }
        assert!(!fixed_rate_live_proof(0,Some(0),true,true));
    }
}

#[cfg(test)]
mod selector_write_source_tests {
    use super::*;
    #[test]
    fn selector_skip_retains_current_readback_and_cannot_change_pin() {
        assert_eq!(selector_write_required(true,2,2),Ok(false));
        assert!(selector_write_required(true,1,2).is_err());
        assert!(selector_write_required(true,0,1).is_err());
        assert!(selector_write_required(true,1,0).is_err());
        assert_eq!(selector_write_required(false,1,2),Ok(true));
    }
}
