//! Read-only recovery from a clean post-clock Valid Alternate Settings rejection.
use super::*;

#[derive(Debug, Clone)]
pub(super) struct Uac2LiveAlternate {
    pub(super) candidate: IsoStreamCandidate,
    pub(super) format: PlaybackFormat,
    pub(super) clock: ClockChoice,
    implicit_clock: Option<ClockChoice>,
}

pub(super) fn select_uac2_live_alternate(
    claimed: &ClaimedHandle,
    original: &IsoStreamCandidate,
    format: PlaybackFormat,
    clock: &ClockChoice,
) -> Result<Option<Uac2LiveAlternate>, String> {
    // Unsupported protocols keep their existing activation path. Never turn a
    // descriptor/control error into permission to try a different transport.
    if !standard_pcm(original, format) || !resolved(clock) {
        return Ok(None);
    }
    if live_available(claimed, original)? {
        return Ok(None);
    }
    verify_standard_descriptor(claimed, original)?;
    verify_clock(claimed, original, clock, Some(format.sample_rate))?;
    let candidates = scan::select_stream_candidates(
        &claimed.handle.device(),
        &claimed.handle,
        format,
        original.usb_speed,
        &claimed.claimed_interfaces,
        true,
    )
    .map_err(|error| error.message().to_owned())?;
    for candidate in candidates {
        if !compatible(original, &candidate, format) {
            continue;
        }
        verify_standard_descriptor(claimed, &candidate)?;
        verify_clock(claimed, &candidate, clock, Some(format.sample_rate))?;
        if !live_available(claimed, &candidate)? {
            continue;
        }
        // A bitmap read must not hide selector, raw multiplier or rate drift.
        let implicit_clock = verify_clock(claimed, &candidate, clock, Some(format.sample_rate))?;
        verify_clock(claimed, original, clock, Some(format.sample_rate))?;
        return Ok(Some(Uac2LiveAlternate {
            candidate,
            format,
            clock: clock.clone(),
            implicit_clock,
        }));
    }
    Err("No compatible UAC2 alternate is currently available at the verified PCM clock".into())
}

pub(super) fn rebuild_uac2_live_alternate(
    claimed: &ClaimedHandle,
    format: PlaybackFormat,
    speed: Speed,
    pin: &Uac2LiveAlternate,
) -> Result<IsoStreamCandidate, SelectError> {
    let refusal = |message: String| SelectError::NoMatch(message);
    if pin.format != format || pin.candidate.usb_speed != speed {
        return Err(refusal("Pinned UAC2 format or USB speed changed".into()));
    }
    let candidates = scan::select_stream_candidates(
        &claimed.handle.device(),
        &claimed.handle,
        format,
        speed,
        &claimed.claimed_interfaces,
        true,
    )?;
    let candidate = candidates
        .into_iter()
        .find(|candidate| same_transport(candidate, &pin.candidate))
        .ok_or_else(|| refusal("Pinned UAC2 transport evidence changed or disappeared".into()))?;
    verify_standard_descriptor(claimed, &candidate).map_err(refusal)?;
    // Reopening may need to reprogram the rate; the route and raw clock factors
    // must remain pinned before doing so. Activation checks availability later.
    let implicit_clock = verify_clock(claimed, &candidate, &pin.clock, None).map_err(refusal)?;
    if implicit_clock != pin.implicit_clock {
        return Err(refusal(
            "Pinned UAC2 implicit clock evidence changed".into(),
        ));
    }
    Ok(candidate)
}

fn resolved(clock: &ClockChoice) -> bool {
    clock.resolution == crate::clock_resolution::ClockResolution::Resolved
}

fn standard_pcm(candidate: &IsoStreamCandidate, format: PlaybackFormat) -> bool {
    !format.is_dop
        && format.dsd_transport == DsdTransportMode::None
        && !candidate.is_uac1
        && candidate.format_tag == FORMAT_TAG_PCM
        && candidate.uac2_formats.is_some_and(|bits| bits & 1 != 0)
        && candidate.terminal_link.is_some()
        && candidate.channels == format.channels
        && candidate.bit_resolution == format.bit_depth
        && matches!(
            (format.bit_depth, candidate.subslot_size),
            (16, 2..=4) | (24, 3 | 4) | (32, 4)
        )
}

fn compatible(
    original: &IsoStreamCandidate,
    candidate: &IsoStreamCandidate,
    format: PlaybackFormat,
) -> bool {
    standard_pcm(candidate, format)
        && candidate.configuration_number == original.configuration_number
        && candidate.interface_number == original.interface_number
        && candidate.terminal_link == original.terminal_link
        && candidate.usb_speed == original.usb_speed
        && (candidate.alt_setting != original.alt_setting
            || candidate.implicit_feedback != original.implicit_feedback)
        && candidate.sync_type == original.sync_type
        && candidate.usage_type == original.usage_type
        && candidate.feedback_endpoint.is_some() == original.feedback_endpoint.is_some()
        && match (original.implicit_feedback, candidate.implicit_feedback) {
            (None, None) => true,
            (Some(a), Some(b)) => {
                a.interface_number == b.interface_number && a.address == b.address
            }
            _ => false,
        }
}

fn live_available(claimed: &ClaimedHandle, candidate: &IsoStreamCandidate) -> Result<bool, String> {
    let output =
        claimed.alternate_is_available(candidate.interface_number, candidate.alt_setting)?;
    let input = if let Some(source) = candidate.implicit_feedback {
        claimed.alternate_is_available(source.interface_number, source.alt_setting)?
    } else {
        true
    };
    Ok(output && input)
}

fn verify_clock(
    claimed: &ClaimedHandle,
    candidate: &IsoStreamCandidate,
    expected: &ClockChoice,
    rate: Option<u32>,
) -> Result<Option<ClockChoice>, String> {
    let config = claimed
        .handle
        .device()
        .active_config_descriptor()
        .map_err(|e| e.to_string())?;
    claimed.ensure_interface_claimed(expected.control_interface)?;
    let choice = strict_implicit_clock(
        &config,
        &claimed.handle,
        expected.control_interface,
        candidate.terminal_link,
    )
    .ok_or_else(|| "UAC2 live alternate has no proven scoped clock".to_owned())?;
    if !resolved(expected) || choice != *expected {
        return Err("UAC2 live alternate clock route or raw factors changed".into());
    }
    let prove_input = || -> Result<Option<ClockChoice>, String> {
        let Some(source) = candidate.implicit_feedback else {
            return Ok(None);
        };
        claimed.ensure_interface_claimed(source.interface_number)?;
        let input = config
            .interfaces()
            .flat_map(|i| i.descriptors())
            .find(|d| {
                d.interface_number() == source.interface_number
                    && d.setting_number() == source.alt_setting
                    && d.class_code() == USB_CLASS_AUDIO
                    && d.sub_class_code() == USB_SUBCLASS_AUDIOSTREAMING
                    && d.protocol_code() == 0x20
            })
            .ok_or_else(|| "UAC2 implicit input descriptor disappeared".to_owned())?;
        let input_format = scan::parse_streaming_extra(0x20, input.extra())
            .ok_or_else(|| "UAC2 implicit input format disappeared".to_owned())?;
        let input_clock = strict_implicit_clock(
            &config,
            &claimed.handle,
            expected.control_interface,
            input_format.terminal_link,
        )
        .ok_or_else(|| "UAC2 implicit input clock disappeared".to_owned())?;
        if !scan::implicit_clocks_compatible(&choice, &input_clock) {
            return Err("UAC2 implicit input clock changed".into());
        }
        Ok(Some(input_clock))
    };
    let proven_input = prove_input()?;
    if let Some(rate) = rate {
        let clock = AudioControlClock {
            interface_number: choice.control_interface,
            clock_id: choice.source_id,
        };
        let observed = get_wire_frequency(&claimed.handle, clock, Some(&choice))?;
        if observed != rate {
            return Err(format!(
                "UAC2 live alternate clock is {observed} Hz, expected {rate} Hz"
            ));
        }
        // Also re-resolve after the frequency transfer, including non-multiplier selectors.
        let after = strict_implicit_clock(
            &config,
            &claimed.handle,
            expected.control_interface,
            candidate.terminal_link,
        );
        if after.as_ref() != Some(expected) || prove_input()? != proven_input {
            return Err("UAC2 live alternate clock changed during rate readback".into());
        }
    }
    Ok(proven_input)
}

fn verify_standard_descriptor(
    claimed: &ClaimedHandle,
    candidate: &IsoStreamCandidate,
) -> Result<(), String> {
    let config = claimed
        .handle
        .device()
        .active_config_descriptor()
        .map_err(|e| e.to_string())?;
    if config.number() != candidate.configuration_number {
        return Err("UAC2 configuration changed".into());
    }
    let descriptor = config
        .interfaces()
        .flat_map(|i| i.descriptors())
        .find(|d| {
            d.interface_number() == candidate.interface_number
                && d.setting_number() == candidate.alt_setting
        })
        .ok_or_else(|| "UAC2 alternate descriptor disappeared".to_owned())?;
    if descriptor.class_code() != USB_CLASS_AUDIO
        || descriptor.sub_class_code() != USB_SUBCLASS_AUDIOSTREAMING
        || descriptor.protocol_code() != 0x20
    {
        return Err("Live alternate requires a declared standard UAC2 AS interface".into());
    }
    let extra = descriptor.extra();
    let mut offset = 0;
    let mut general = 0;
    let mut type_i = 0;
    while offset < extra.len() {
        let length = usize::from(extra[offset]);
        if length < 2 || length > extra.len() - offset {
            return Err("Malformed UAC2 streaming descriptor".into());
        }
        let d = &extra[offset..offset + length];
        if d[1] == USB_DT_CS_INTERFACE {
            if d.len() < 3 {
                return Err("Short UAC2 class descriptor".into());
            }
            match d[2] {
                UAC2_AS_GENERAL => {
                    if d.len() != 16 || d[5] != UAC2_FORMAT_TYPE_I || d[6] & 1 == 0 {
                        return Err("Live alternate requires standard Type-I PCM AS_GENERAL".into());
                    }
                    general += 1;
                }
                UAC2_FORMAT_TYPE => {
                    if d.len() != 6 || d[3] != UAC2_FORMAT_TYPE_I {
                        return Err("Live alternate requires Type-I format".into());
                    }
                    type_i += 1;
                }
                _ => {}
            }
        }
        offset += length;
    }
    if general != 1 || type_i != 1 {
        return Err("UAC2 PCM descriptors are missing or ambiguous".into());
    }
    let format = scan::parse_streaming_extra(0x20, extra)
        .ok_or_else(|| "UAC2 PCM descriptor cannot be parsed".to_owned())?;
    if format.format_tag != candidate.format_tag
        || format.uac2_formats != candidate.uac2_formats
        || format.channels != candidate.channels
        || format.subslot_size != candidate.subslot_size
        || format.bit_resolution != candidate.bit_resolution
        || format.terminal_link != candidate.terminal_link
    {
        return Err("UAC2 PCM descriptor evidence changed".into());
    }
    Ok(())
}

fn same_feedback(a: Option<FeedbackEndpoint>, b: Option<FeedbackEndpoint>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(a), Some(b)) => {
            a.refresh == b.refresh
                && a.address == b.address
                && a.transfer_type == b.transfer_type
                && a.interval == b.interval
                && a.service_interval_us == b.service_interval_us
                && a.max_packet_bytes == b.max_packet_bytes
        }
        _ => false,
    }
}

fn same_transport(a: &IsoStreamCandidate, b: &IsoStreamCandidate) -> bool {
    a.configuration_number == b.configuration_number
        && a.interface_number == b.interface_number
        && a.alt_setting == b.alt_setting
        && a.endpoint_address == b.endpoint_address
        && a.endpoint_interval == b.endpoint_interval
        && a.service_interval_us == b.service_interval_us
        && a.usb_speed == b.usb_speed
        && a.max_packet_bytes == b.max_packet_bytes
        && a.sync_type == b.sync_type
        && a.usage_type == b.usage_type
        && a.format_tag == b.format_tag
        && a.uac2_formats == b.uac2_formats
        && a.native_dsd == b.native_dsd
        && a.channels == b.channels
        && a.subslot_size == b.subslot_size
        && a.bit_resolution == b.bit_resolution
        && a.refresh == b.refresh
        && a.synch_address == b.synch_address
        && same_feedback(a.feedback_endpoint, b.feedback_endpoint)
        && a.implicit_feedback == b.implicit_feedback
        && a.terminal_link == b.terminal_link
        && a.is_uac1 == b.is_uac1
}

#[cfg(test)]
mod tests {
    use super::*;

    fn format() -> PlaybackFormat {
        PlaybackFormat {
            sample_rate: 96_000,
            bit_depth: 24,
            channels: 2,
            is_dop: false,
            dsd_transport: DsdTransportMode::None,
            dsd_bit_rate: 0,
        }
    }

    fn candidate() -> IsoStreamCandidate {
        IsoStreamCandidate {
            configuration_number: 1,
            interface_number: 1,
            alt_setting: 1,
            endpoint_address: 1,
            endpoint_interval: 1,
            service_interval_us: 125,
            usb_speed: Speed::High,
            max_packet_bytes: 128,
            sync_type: SyncType::Synchronous,
            usage_type: UsageType::Data,
            format_tag: FORMAT_TAG_PCM,
            uac2_formats: Some(1),
            native_dsd: None,
            channels: 2,
            subslot_size: 3,
            bit_resolution: 24,
            sample_rates: vec![96_000],
            refresh: 0,
            synch_address: 0,
            feedback_endpoint: None,
            implicit_feedback: None,
            terminal_link: Some(2),
            is_uac1: false,
        }
    }

    #[test]
    fn replacement_preserves_pcm_width_route_and_pacing() {
        let original = candidate();
        let mut replacement = original.clone();
        replacement.alt_setting = 2;
        replacement.subslot_size = 4;
        assert!(compatible(&original, &replacement, format()));
        replacement.terminal_link = Some(3);
        assert!(!compatible(&original, &replacement, format()));
        replacement.terminal_link = original.terminal_link;
        replacement.sync_type = SyncType::Asynchronous;
        assert!(!compatible(&original, &replacement, format()));
        assert!(!compatible(&original, &original, format()));
    }

    #[test]
    fn dop_private_formats_and_wrong_width_are_excluded() {
        let mut candidate = candidate();
        let mut dop = format();
        dop.is_dop = true;
        assert!(!standard_pcm(&candidate, dop));
        candidate.uac2_formats = None;
        assert!(!standard_pcm(&candidate, format()));
        candidate.uac2_formats = Some(1);
        candidate.bit_resolution = 32;
        assert!(!standard_pcm(&candidate, format()));
    }

    #[test]
    fn implicit_replacement_requires_same_physical_input_pipe() {
        let mut original = candidate();
        original.implicit_feedback = Some(ImplicitFeedbackSource {
            interface_number: 2,
            alt_setting: 1,
            address: 0x82,
            bytes_per_frame: 6,
            service_interval_us: 125,
            max_packet_bytes: 128,
        });
        let mut replacement = original.clone();
        replacement.implicit_feedback.as_mut().unwrap().alt_setting = 2;
        assert!(compatible(&original, &replacement, format()));
        replacement.implicit_feedback.as_mut().unwrap().address = 0x83;
        assert!(!compatible(&original, &replacement, format()));
    }

    #[test]
    fn pinned_identity_includes_raw_feedback_and_packet_layout() {
        let mut original = candidate();
        original.feedback_endpoint = Some(FeedbackEndpoint {
            refresh: 2,
            address: 0x81,
            transfer_type: TransferType::Isochronous,
            interval: 4,
            service_interval_us: 1000,
            max_packet_bytes: 4,
        });
        assert!(same_transport(&original, &original));
        let mut changed = original.clone();
        changed.feedback_endpoint.as_mut().unwrap().refresh += 1;
        assert!(!same_transport(&original, &changed));
        changed = original.clone();
        changed.max_packet_bytes += 1;
        assert!(!same_transport(&original, &changed));
        changed = original.clone();
        changed.sample_rates.push(48_000);
        assert!(same_transport(&original, &changed));
    }
}
