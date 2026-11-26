use super::*;
use crate::capabilities::{AltFormat, AltSetting, LinkSpeed, RateRange, SyncKind, UacVersion};

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
    )
}

pub(super) fn build_usb_capability_model(
    device: &Device<Context>,
    handle: &DeviceHandle<Context>,
    speed: Speed,
) -> Result<CapabilityModel, String> {
    let config_descriptor = device
        .active_config_descriptor()
        .map_err(|error| format!("Failed to read active USB config descriptor: {}", error))?;

    let mut alt_settings = Vec::new();
    let mut supported_sample_rates = Vec::new();
    let mut stream_rate_cache =
        std::collections::HashMap::<(u8, Option<u8>), Vec<SamplingFrequencySubrange>>::new();
    let dsd_subslot_override = settings().dsd_subslot_override();
    let mut effective_speed = speed;

    for interface in config_descriptor.interfaces() {
        for descriptor in interface.descriptors() {
            if descriptor.class_code() != USB_CLASS_AUDIO
                || descriptor.sub_class_code() != USB_SUBCLASS_AUDIOSTREAMING
            {
                continue;
            }

            let Some(mut stream_format) = parse_android_streaming_interface_format(&descriptor) else {
                continue;
            };
            if stream_format.format_tag == FORMAT_TAG_DSD {
                if let Some(ss) = dsd_subslot_override {
                    stream_format.subslot_size = ss;
                }
            }
            let sample_rate_ranges = stream_rate_cache
                .entry((descriptor.interface_number(), stream_format.terminal_link))
                .or_insert_with(|| {
                    discover_stream_sample_rate_ranges(
                        device,
                        handle,
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
            dev_eprintln!(
                "[USB] build_usb_capability_model using speed={:?}",
                speed
            );
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

                supported_sample_rates.extend(sample_rates.iter().copied());
                alt_settings.push(AltSetting {
                    interface_number: descriptor.interface_number(),
                    alt_setting: descriptor.setting_number(),
                    endpoint_address: endpoint.address(),
                    format: alt_format(stream_format.format_tag),
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
                    feedback_endpoint: find_feedback_endpoint(
                        &descriptor,
                        endpoint.synch_address(),
                        endpoint.sync_type(),
                        speed,
                    )
                    .map(|feedback| feedback.address),
                    max_packet_bytes,
                    service_interval_us,
                    uac_version: if stream_format.is_uac1 {
                        UacVersion::V1
                    } else {
                        UacVersion::V2
                    },
                });
            }
        }
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
) -> Result<IsoStreamCandidate, SelectError> {
    let config_descriptor = device.active_config_descriptor().map_err(|error| {
        SelectError::NoMatch(format!("Failed to read active USB config descriptor: {}", error))
    })?;
    let mut refused_unsynchronized = false;
    let allow_unsynchronized_async = settings().allow_unsynchronized_async();
    let mut candidates = Vec::new();
    let prefer_padded_24bit_transport = settings().prefer_padded_24bit();
    let dsd_subslot_override = settings().dsd_subslot_override();
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
            if stream_format.format_tag == FORMAT_TAG_DSD {
                if let Some(ss) = dsd_subslot_override {
                    stream_format.subslot_size = ss;
                }
            }

            let compatibility_penalty =
                transport_compatibility_penalty(&stream_format, playback_format);
            if compatibility_penalty == u32::MAX {
                continue;
            }

            let sample_rate_ranges = stream_rate_cache
                .entry((descriptor.interface_number(), stream_format.terminal_link))
                .or_insert_with(|| {
                    discover_stream_sample_rate_ranges(
                        device,
                        handle,
                        descriptor.interface_number(),
                        stream_format.terminal_link,
                        &stream_format.sample_rate_ranges,
                    )
                })
                .clone();
            let sample_rates = representative_sample_rates_from_ranges(&sample_rate_ranges);

            let effective_sample_rate = if playback_format.dsd_transport == DsdTransportMode::Native
                && stream_format.format_tag == FORMAT_TAG_DSD
                && playback_format.dsd_bit_rate > 0
                && stream_format.subslot_size > 0
            {
                playback_format.dsd_bit_rate / (8 * u32::from(stream_format.subslot_size))
            } else {
                playback_format.sample_rate
            };

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
                let feedback_endpoint = find_feedback_endpoint(
                    &descriptor,
                    endpoint.synch_address(),
                    endpoint.sync_type(),
                    speed,
                );
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

                match admit_endpoint(
                    endpoint.sync_type(),
                    endpoint.synch_address(),
                    feedback_endpoint.is_some(),
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

                if max_packet_bytes < required_max_packet_bytes {
                    continue;
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
                    channels: stream_format.channels,
                    subslot_size: stream_format.subslot_size,
                    bit_resolution: stream_format.bit_resolution,
                    sample_rates: sample_rates.clone(),
                    refresh: endpoint.refresh(),
                    synch_address: endpoint.synch_address(),
                    feedback_endpoint,
                    terminal_link: stream_format.terminal_link,
                    is_uac1: stream_format.is_uac1,
                });
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
        if refused_unsynchronized {
            return SelectError::ImplicitFeedback(format!(
                "The only isochronous OUT endpoints that can carry {} / {}-bit / {} ch are asynchronous without an explicit feedback endpoint (implicit feedback is not supported)",
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
        address: endpoint.address,
        transfer_type: endpoint.transfer_type,
        interval: endpoint.interval,
        service_interval_us: service_interval_micros(speed, endpoint.interval),
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
        return Err("USB configuration is missing audio control or playback interfaces".to_string());
    }
    let claimed: HashSet<_> = claimed_handle.claimed_interfaces.iter().copied().collect();
    if claimed != required || claimed.len() != claimed_handle.claimed_interfaces.len() {
        return Err(format!(
            "The host's USB claims {:?} don't match required audio control/playback interfaces {:?}",
            claimed_handle.claimed_interfaces, required
        ));
    }
    Ok(())
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
            inferred, bus, addr
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
    let mut channels = None;
    let mut format_tag = None;
    let mut subslot_size = None;
    let mut bit_resolution = None;
    let mut terminal_link = None;
    let mut sample_rate_ranges = Vec::new();

    let is_uac1 = descriptor.protocol_code() != 0x20;

    for extra in DescriptorIter::new(descriptor.extra()) {
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
                    channels = Some(extra[10] as u16);
                    format_tag =
                        Some(u32::from_le_bytes([extra[6], extra[7], extra[8], extra[9]]) as u16);
                } else if extra.len() >= 7 {
                    format_tag = Some(u16::from_le_bytes([extra[5], extra[6]]));
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
    if candidate.format_tag != FORMAT_TAG_PCM
        && !(is_native_dsd && candidate.format_tag == FORMAT_TAG_DSD)
    {
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
            let found = (descriptor.interface_number(), terminal_link, format.channels);
            if format.format_tag == FORMAT_TAG_PCM {
                return Some(found);
            }
            fallback.get_or_insert(found);
        }
    }
    fallback
}
