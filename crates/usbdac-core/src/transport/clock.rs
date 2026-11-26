use super::*;

pub(super) fn find_audio_control_clock(
    device: &Device<Context>,
    handle: &DeviceHandle<Context>,
    terminal_link: Option<u8>,
) -> Option<AudioControlClock> {
    find_audio_control_clock_choice(device, handle, terminal_link).map(|(clock, _)| clock)
}

/// [`find_audio_control_clock`], with how the clock was reached for
/// `StreamFormat::clock`.
pub(super) fn find_audio_control_clock_choice(
    device: &Device<Context>,
    handle: &DeviceHandle<Context>,
    terminal_link: Option<u8>,
) -> Option<(AudioControlClock, ClockChoice)> {
    let config_descriptor = device.active_config_descriptor().ok()?;

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
                    let clock = AudioControlClock {
                        interface_number: ac_iface,
                        clock_id: selection.choice.source_id,
                    };
                    return Some((clock, selection.choice));
                }
                Err(reason) => log_warn!(
                    "[USB] Could not resolve the clock for bTerminalLink={:?} on AC interface {}: {}",
                    terminal_link,
                    ac_iface,
                    reason,
                ),
            }
        }
    }

    dev_eprintln!("[USB] No AudioControl clock source entity found in any interface descriptor");
    None
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

pub(super) fn set_uac1_sampling_frequency(
    handle: &DeviceHandle<Context>,
    interface_number: u8,
    endpoint_address: u8,
    sample_rate: u32,
) -> Result<(), String> {
    let data = [
        (sample_rate & 0xFF) as u8,
        ((sample_rate >> 8) & 0xFF) as u8,
        ((sample_rate >> 16) & 0xFF) as u8,
    ];

    // UAC1 5.2.3.2.3.2: SAM_FREQ_CONTROL SET_CUR is an endpoint request
    // (bmRequestType 0x22, wValue 0x0100, 3-byte LE rate). Real hardware
    // disagrees on wIndex: the spec form is (interface << 8) | endpoint, but
    // Linux snd-usb-audio sends the endpoint address alone, and some legacy
    // firmware only accepts the interface-recipient hybrid. A control STALL
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
            "endpoint+interface (spec)",
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
            Ok(_) => {
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
            "endpoint+interface (spec)",
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
            Ok(transferred) if transferred == data.len() => {
                let rate = u32::from_le_bytes([data[0], data[1], data[2], 0]);
                dev_eprintln!(
                    "[USB] UAC1 GET_CUR: endpoint=0x{:02x} reports {}Hz via wIndex=0x{:04x} ({})",
                    endpoint_address,
                    rate,
                    index,
                    label
                );
                return Ok(rate);
            }
            Ok(transferred) => {
                last_error = format!(
                    "GET_CUR returned {} bytes, expected 3 (wIndex=0x{:04x}, {})",
                    transferred, index, label
                );
            }
            Err(error) => {
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
        .map_err(|error| format!("Failed to read sampling frequency: {}", error))?;
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
    if transferred < 2 {
        return Err(format!(
            "Sampling-frequency GET_RANGE returned {} bytes, expected at least 2",
            transferred
        ));
    }

    let count = u16::from_le_bytes([data[0], data[1]]) as usize;
    if count == 0 {
        return Err("Sampling-frequency GET_RANGE returned zero subranges".to_string());
    }

    let expected = 2 + count * 12;
    if transferred < expected {
        return Err(format!(
            "Sampling-frequency GET_RANGE returned {} bytes, expected at least {} for {} subranges",
            transferred, expected, count
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

    let Some(clock) = find_audio_control_clock(device, handle, terminal_link) else {
        return Vec::new();
    };

    match get_sampling_frequency_ranges(handle, clock.interface_number, clock.clock_id) {
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

    let mut last_set_error = None;
    let mut last_readback_error = None;
    let mut last_reported_rate = None;
    let mut set_cur_succeeded = false;

    for attempt in 1..=3 {
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
                last_set_error = Some(error);
                thread::sleep(Duration::from_millis(settle_delay_ms));
                continue;
            }
        }

        thread::sleep(Duration::from_millis(settle_delay_ms));
        match get_sampling_frequency(handle, interface_number, clock_id) {
            Ok(reported_sample_rate) => {
                last_reported_rate = Some(reported_sample_rate);
                if reported_sample_rate == sample_rate {
                    let validity = match source_controls {
                        Some(controls) => crate::clock_resolution::clock_validity(controls, || {
                            get_clock_validity(handle, interface_number, clock_id)
                        }),
                        // Preserve the malformed-descriptor compatibility path;
                        // it never authorizes automatic selector changes.
                        None => match get_clock_validity(handle, interface_number, clock_id) {
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
                last_readback_error = Some(error);
                continue;
            }
        }
    }

    if let Some(mut reported_sample_rate) = last_reported_rate {
        // Some DACs (e.g. Fosi DS2 / Savitech bridge) report 0 Hz via GET_CUR
        // even after a successful SET_CUR. Replace the bogus 0 with the
        // requested rate so it doesn't poison downstream code paths.
        if reported_sample_rate == 0 {
            dev_eprintln!(
                "[USB] DAC reports 0 Hz after SET_CUR on clock {} / interface {}; falling back to requested {} Hz",
                clock_id, interface_number, sample_rate
            );
            reported_sample_rate = sample_rate;
        }
        return ClockApplyOutcome {
            clock_ok: false,
            rate_verified: false,
            reported_sample_rate: Some(reported_sample_rate),
            known_mismatch: reported_sample_rate != sample_rate,
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
    let device = handle.device();
    let count = device.device_descriptor().ok()?.num_configurations();
    let index = (0..count).find(|index| {
        device
            .config_descriptor(*index)
            .ok()
            .is_some_and(|c| c.number() == config.number())
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
        .ok()?;
    if n != header.len() || header[5] != config.number() {
        return None;
    }
    let length = usize::from(u16::from_le_bytes([header[2], header[3]]));
    if length < header.len() {
        return None;
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
        .ok()?;
    (n == length && bytes[5] == config.number()).then_some(bytes)
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
    if !resolved.proves_identity_cadence() {
        return None;
    }
    Some(ClockChoice {
        control_interface,
        source_id: resolved.clock_id,
        path: resolved.hops,
        resolution: crate::clock_resolution::ClockResolution::Resolved,
    })
}

// Kept outside SessionState so frozen registration/test fixtures need no new
// fields. USB_SESSION_GATE / SessionGuard serialize the lifecycle operations.
#[derive(Clone)]
struct SelectedClockRoute {
    fd: RawFd,
    control: u8,
    streaming: u8,
    plan: crate::clock_resolution::ClockPlan,
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
    let mut sources = HashSet::new();
    let mut ranges = Vec::new();
    for plan in plans {
        if !sources.insert(plan.source_id) {
            continue;
        }
        if sources.len() > 8 {
            break;
        }
        if let Ok((_, _, found)) = probe_clock_source(handle, control, plan.source_id, plan.source_controls) {
            ranges.extend(found);
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
    // Alternate paths have not proved the complete implicit playback/IN pair.
    if candidate.implicit_feedback.is_some() {
        return Ok(());
    }
    let device = claimed.handle.device();
    let Some((control, graph)) =
        owning_clock_graph(&device, &claimed.handle, candidate.interface_number)
    else {
        return Ok(());
    };
    let Some(terminal) = candidate.terminal_link else {
        return Ok(());
    };
    claimed.ensure_interface_claimed(control)?;
    let mut reads = 0usize;
    let Ok(plans) = graph.enumerate_plans(terminal, |id| {
        reads += 1;
        if reads > 16 {
            return Err("clock selector read budget exhausted".into());
        }
        get_clock_selector_pin(&claimed.handle, control, id)
    }) else {
        return Ok(());
    };
    let mut io = UsbClockPlanIo {
        handle: &claimed.handle,
        control,
        rate,
        allow_program: false,
    };
    let mut sources = HashSet::new();
    for plan in plans.into_iter().take(8) {
        // Preserve paths to shared sources if an earlier application fails,
        // but cap all hardware attempts independently of the pure search.
        sources.insert(plan.source_id);
        if sources.len() > 8 {
            break;
        }
        let Ok((validity, current, ranges)) = probe_clock_source(&claimed.handle, control, plan.source_id, plan.source_controls) else {
            continue;
        };
        if !sampling_frequency_ranges_support_rate(&ranges, rate) {
            continue;
        }
        let needs_program = validity == crate::clock_resolution::ClockValidity::Unreported && current != rate;
        if (plan.selectors.iter().any(|pin| pin.original != pin.target) || needs_program)
            && !stop_owned_clock_alternates(claimed, candidate.interface_number)?
        {
            // Never alter a clock shared with an unclaimed sibling interface.
            continue;
        }
        io.allow_program = true;
        match plan.apply(&mut io) {
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
                    plan,
                });
                if !exact {
                    restore_requested_clock_route(claimed)?;
                    return Err("clock route changed immediately after selector transaction".into());
                }
                return Ok(());
            }
            Err(error) if !error.rollback_errors.is_empty() => return Err(error.to_string()),
            Err(error) => log_warn!("[USB] Rejected clock path: {error}"),
        }
    }
    Ok(())
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
        .is_none_or(|r| r.fd != device.fd)
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
