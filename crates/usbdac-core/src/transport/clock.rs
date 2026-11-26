use super::*;

pub(super) fn find_audio_control_clock(
    device: &Device<Context>,
    handle: &DeviceHandle<Context>,
    terminal_link: Option<u8>,
) -> Option<AudioControlClock> {
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

            if let Some(clock) = resolve_stream_clock(handle, ac_iface, &graph, terminal_link) {
                return Some(clock);
            }

            // Last resort: the first Clock Source entity.
            if let Some(clock_id) = graph.first_clock_source() {
                log_warn!(
                    "[USB] Clock topology for bTerminalLink={:?} could not be resolved; using the \
                     first Clock Source id={} on AC interface {}. On a DAC with more than one clock \
                     the sample rate may be set on the wrong one.",
                    terminal_link,
                    clock_id,
                    ac_iface,
                );
                return Some(AudioControlClock {
                    interface_number: ac_iface,
                    clock_id,
                });
            }
        }
    }

    dev_eprintln!("[USB] No AudioControl clock source entity found in any interface descriptor");
    None
}

/// Walk the terminal's clock chain on one AudioControl interface. `None` means
/// the caller must fall back; the reason is logged here.
pub(super) fn resolve_stream_clock(
    handle: &DeviceHandle<Context>,
    ac_iface: u8,
    graph: &ClockGraph,
    terminal_link: Option<u8>,
) -> Option<AudioControlClock> {
    let Some(terminal) = terminal_link else {
        log_warn!(
            "[USB] No bTerminalLink for the selected stream; cannot trace its clock on AC interface {}",
            ac_iface
        );
        return None;
    };
    let resolved = graph.resolve_terminal_clock(terminal, |selector| {
        get_clock_selector_pin(handle, ac_iface, selector)
    });
    match resolved {
        Ok(resolved) => {
            log_info!(
                "[USB] Clock resolved via topology: bTerminalLink={} -> clock path {:?} -> Clock \
                 Source id={} on AC interface {}{}",
                terminal,
                resolved.path,
                resolved.clock_id,
                ac_iface,
                if resolved.assumed_source {
                    " (no descriptor for that ID; assumed to be the source)"
                } else {
                    ""
                },
            );
            Some(AudioControlClock {
                interface_number: ac_iface,
                clock_id: resolved.clock_id,
            })
        }
        Err(error) => {
            log_warn!(
                "[USB] Could not resolve the clock for bTerminalLink={} on AC interface {}: {}",
                terminal,
                ac_iface,
                error
            );
            None
        }
    }
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
    let transferred = handle
        .read_control(
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
    let config_value = config_descriptor.number();
    let mut raw = vec![0u8; 1024];
    let request_type: u8 = 0x80;
    let descriptor_type_config: u8 = 0x02;
    let read = handle
        .read_control(
            request_type,
            0x06,
            (descriptor_type_config as u16) << 8 | config_value as u16,
            0,
            &mut raw,
            Duration::from_secs(1),
        )
        .ok()?;

    if read < 4 {
        log::error!(
            "[USB] Raw GET_DESCRIPTOR returned only {} bytes; cannot parse config descriptor",
            read,
        );
        return None;
    }
    let total_length = u16::from_le_bytes([raw[2], raw[3]]) as usize;
    if total_length > read {
        let mut full_raw = vec![0u8; total_length];
        match handle.read_control(
            request_type,
            0x06,
            (descriptor_type_config as u16) << 8 | config_value as u16,
            0,
            &mut full_raw,
            Duration::from_secs(1),
        ) {
            Ok(n) => {
                raw = full_raw;
                raw.truncate(n);
            }
            Err(error) => {
                log::error!(
                    "[USB] Raw GET_DESCRIPTOR full read failed: {}; using partial {} bytes",
                    error,
                    read,
                );
                raw.truncate(read);
            }
        }
    } else {
        raw.truncate(read);
    }

    log::info!(
        "[USB] Raw config descriptor: {} bytes for config {}",
        raw.len(),
        config_value,
    );
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
    handle
        .write_control(
            request_type,
            UAC2_REQUEST_SET_CUR,
            value,
            index,
            &data,
            Duration::from_secs(1),
        )
        .map_err(|error| format!("Failed to set sampling frequency: {}", error))?;
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
        match handle.write_control(
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
        match handle.read_control(
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
    let transferred = handle
        .read_control(
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
    let transferred = handle
        .read_control(
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
    let transferred = handle
        .read_control(
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

pub(super) fn representative_sample_rates_from_ranges(ranges: &[SamplingFrequencySubrange]) -> Vec<u32> {
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
                clock_id, interface_number, current_rate, sample_rate,
            );
        }
        Err(error) => {
            dev_eprintln!(
                "[USB] GET_CUR (pre-SET): failed for clock {} on interface {}: {}",
                clock_id, interface_number, error,
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
                    // Rate verified — also check clock validity if supported.
                    let clock_valid = match get_clock_validity(handle, interface_number, clock_id) {
                        Ok(valid) => {
                            dev_eprintln!(
                                "[USB] Clock Validity: clock {} valid={} (after SET_CUR {}Hz)",
                                clock_id, valid, sample_rate,
                            );
                            Some(valid)
                        }
                        Err(e) => {
                            dev_eprintln!(
                                "[USB] Clock Validity: not supported for clock {} ({}); assuming valid",
                                clock_id, e,
                            );
                            None
                        }
                    };
                    if clock_valid == Some(false) {
                        dev_eprintln!(
                            "[USB] Clock {} reports INVALID after SET_CUR {}Hz; retrying",
                            clock_id, sample_rate,
                        );
                        thread::sleep(Duration::from_millis(settle_delay_ms));
                        continue;
                    }
                    dev_eprintln!(
                        "[USB] SET_CUR verified: {} Hz on clock {} interface {} after attempt {}",
                        sample_rate, clock_id, interface_number, attempt,
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
            known_mismatch: true,
            set_cur_succeeded,
            message: Some(format!(
                "Requested {} Hz, DAC reports {} Hz after SET_CUR on clock {} / interface {}",
                sample_rate, reported_sample_rate, clock_id, interface_number
            )),
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

