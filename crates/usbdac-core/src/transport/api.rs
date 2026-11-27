//! The crate-internal facade the public `Dac`/`DacStream` layer calls: it turns
//! [`StreamRequest`]s into the transport's playback formats, runs the two-phase
//! negotiate/open flow and summarises the descriptor scan as [`Capabilities`].

use super::*;
use crate::capabilities::{AltFormat, AltSetting, Capabilities, LinkSpeed, UacVersion};
use crate::error::Error;
use crate::events::EventSink;
use crate::format::{Encoding, RatePolicy, StreamFormat, StreamRequest};

/// Typical DoP carriers: DSD64, DSD128, DSD256 and DSD512 at 16 DSD bits per
/// PCM frame.
const DOP_CARRIER_RATES: [u32; 4] = [176_400, 352_800, 705_600, 1_411_200];

const MIN_NATIVE_DSD_BIT_RATE: u32 = 2_822_400;

fn valid_native_dsd_bit_rate(bit_rate: u32) -> bool {
    bit_rate >= MIN_NATIVE_DSD_BIT_RATE && bit_rate % 32 == 0
}

fn native_dsd_minimum_fits(
    capacity: usize,
    interval_us: u32,
    channels: u16,
    word_bytes: u8,
) -> bool {
    if !(1..=32).contains(&channels) || !matches!(word_bytes, 1 | 2 | 4) || interval_us == 0 {
        return false;
    }
    let wire_rate = MIN_NATIVE_DSD_BIT_RATE / (u32::from(word_bytes) * 8);
    capacity
        >= required_max_packet_bytes(
            wire_rate,
            interval_us,
            usize::from(channels) * usize::from(word_bytes),
        )
}

fn native_dsd_bit_rate(wire_rate: u32, word_bytes: u8) -> Option<u32> {
    wire_rate
        .checked_mul(8)?
        .checked_mul(u32::from(word_bytes))
        .filter(|bit_rate| valid_native_dsd_bit_rate(*bit_rate))
}

/// The transport format a request asks for.
pub(crate) fn playback_format_from_request(
    request: &StreamRequest,
) -> Result<PlaybackFormat, Error> {
    let invalid = |message: &str| Err(Error::InvalidArgument(message.to_string()));
    if request.channels == 0 || request.channels > 32 {
        return invalid("channels must be between 1 and 32");
    }
    let format = match request.encoding {
        Encoding::Pcm | Encoding::PcmU8 | Encoding::PcmFloat32 => {
            if request.sample_rate < 8_000 || request.sample_rate > 1_536_000 {
                return invalid("sample_rate must be between 8000 and 1536000 Hz");
            }
            let pcm_encoding = match request.encoding {
                Encoding::Pcm => PcmEncoding::Signed,
                Encoding::PcmU8 if request.bit_depth == 8 => PcmEncoding::Unsigned8,
                Encoding::PcmFloat32 if request.bit_depth == 32 => PcmEncoding::Float32,
                _ => return invalid("unsigned PCM requires 8 bits; IEEE float requires 32 bits"),
            };
            if pcm_encoding == PcmEncoding::Signed && !matches!(request.bit_depth, 16 | 24 | 32) {
                return invalid("signed PCM bit_depth must be 16, 24 or 32");
            }
            PlaybackFormat {
                pcm_encoding,
                sample_rate: request.sample_rate,
                bit_depth: request.bit_depth,
                channels: request.channels,
                is_dop: false,
                dsd_transport: DsdTransportMode::None,
                dsd_bit_rate: 0,
            }
        }
        Encoding::DoP => {
            if request.sample_rate < 44_100 || request.sample_rate > 1_536_000 {
                return invalid("the DoP carrier rate must be between 44100 and 1536000 Hz");
            }
            if !matches!(request.bit_depth, 24 | 32) {
                return invalid("DoP bit_depth must be 24 or 32");
            }
            PlaybackFormat {
                pcm_encoding: crate::transport::PcmEncoding::Signed,
                sample_rate: request.sample_rate,
                bit_depth: request.bit_depth,
                channels: request.channels,
                is_dop: true,
                dsd_transport: DsdTransportMode::DoP,
                dsd_bit_rate: 0,
            }
        }
        Encoding::DsdNative { bit_rate } => {
            if !valid_native_dsd_bit_rate(bit_rate) {
                return invalid("the native DSD bit_rate must be a DSD rate (at least 2822400)");
            }
            PlaybackFormat {
                pcm_encoding: crate::transport::PcmEncoding::Signed,
                // The wire rate for a 4-byte subslot; the real one follows from
                // the alternate setting that gets chosen.
                sample_rate: bit_rate / 32,
                bit_depth: 1,
                channels: request.channels,
                is_dop: false,
                dsd_transport: DsdTransportMode::Native,
                dsd_bit_rate: bit_rate,
            }
        }
    };
    Ok(sanitize_playback_format(format))
}

/// Checks the DAC takes `request` and settles the wire format.
///
/// This programs the DAC's sampling frequency (and verifies it by reading it
/// back), like a stream start does. It refuses while a stream is open.
pub(crate) fn negotiate(request: &StreamRequest) -> Result<StreamFormat, Error> {
    let requested = playback_format_from_request(request)?;
    let session_gate = USB_SESSION_GATE.lock();
    if USB_SESSION_CLEAR_PENDING.load(Ordering::SeqCst) {
        return Err(Error::InvalidState("the DAC is being closed".to_string()));
    }
    if is_usb_session_active() {
        return Err(Error::InvalidState(
            "a stream is open; close it before negotiating another format".to_string(),
        ));
    }
    let device = {
        let mut guard = SESSION_STATE.lock();
        let Some(state) = guard.as_mut() else {
            return Err(Error::InvalidState("no DAC is open".to_string()));
        };
        state.requested_playback_format = Some(requested);
        state.playback_format = Some(requested);
        state.clock_status = None;
        state.clock_control_attempted = false;
        state.clock_control_succeeded = false;
        state.clock_verification_passed = false;
        state.dac_mode = DacMode::FullControl;
        state.direct_mode_refusal_reason = None;
        state.device.clone()
    };
    let _session = SessionGuard::start_locked();
    drop(session_gate);

    let (effective, clock) = negotiate_playback_format(requested)?;
    let effective = SESSION_STATE
        .lock()
        .as_ref()
        .and_then(|state| state.playback_format)
        .filter(|format| format.sample_rate > 0)
        .unwrap_or(effective);
    if request.rate_policy == RatePolicy::Exact
        && !matches!(request.encoding, Encoding::DsdNative { .. })
        && effective.sample_rate != requested.sample_rate
    {
        return Err(Error::Unsupported {
            reason: format!(
                "the DAC cannot run {} Hz exactly (closest it would run: {} Hz); use RatePolicy::NearestSupported to accept it",
                requested.sample_rate, effective.sample_rate
            ),
            offered: offered_alt_settings(),
        });
    }

    let lock_requested = current_lock_requested_for_fd(device.fd);
    let handle = open_claimed_usb_handle(&device).map_err(Error::Claim)?;
    let usb_device = handle.handle.device();
    let speed = resolve_device_speed(&usb_device);
    // Keep any selectively woken ADC3 domain owned until its HC map is read.
    let selected = select_owned_stream_candidate(&handle, effective, speed)
        .map_err(|error| error.into_error(offered_alt_settings()))
        .and_then(|candidate| {
            let map = playback_map::for_candidate(&usb_device, &handle.handle, &candidate)
                .map_err(Error::Control)?;
            Ok((candidate, map))
        });
    let restored = cleanup_claimed_handle(handle, None, lock_requested);
    if !restored {
        return Err(Error::Control(
            "playback descriptor probe could not restore USB ownership/power".into(),
        ));
    }
    let (candidate, channel_map) = selected?;

    let native = effective.dsd_transport == DsdTransportMode::Native;
    Ok(StreamFormat {
        request: *request,
        encoding: request.encoding,
        sample_rate: native_wire_rate(effective, candidate.subslot_size).ok_or_else(|| {
            Error::InvalidState("selected transport has no exact wire rate".into())
        })?,
        channels: candidate.channels,
        channel_map,
        valid_bits: if native { 1 } else { candidate.bit_resolution },
        subslot_bytes: candidate.subslot_size,
        interface_number: candidate.interface_number,
        alt_setting: candidate.alt_setting,
        endpoint_address: candidate.endpoint_address,
        sync_type: sync_kind(candidate.sync_type),
        explicit_feedback: candidate.feedback_endpoint.is_some(),
        link_speed: link_speed_of(candidate.usb_speed),
        service_interval_us: candidate.service_interval_us,
        // The candidate was admitted, so it has a pacing.
        pacing: candidate.implicit_feedback.as_ref().map_or_else(
            || {
                admitted_pacing(
                    candidate.sync_type,
                    candidate.feedback_endpoint.map(|feedback| feedback.address),
                )
            },
            |source| crate::Pacing::ImplicitFeedback {
                interface_number: source.interface_number,
                alt_setting: source.alt_setting,
                endpoint: source.address,
            },
        ),
        clock,
        native_dsd_signature: if native {
            candidate.native_dsd.map(|p| p.signature())
        } else {
            None
        },
    })
}

/// Whether the session already holds the negotiation of `format`.
pub(super) fn negotiation_matches(format: &StreamFormat) -> bool {
    let Ok(requested) = playback_format_from_request(&format.request) else {
        return false;
    };
    let guard = SESSION_STATE.lock();
    let Some(state) = guard.as_ref() else {
        return false;
    };
    let Some(effective) = state.playback_format else {
        return false;
    };
    let native = matches!(format.encoding, Encoding::DsdNative { .. });
    state.requested_playback_format == Some(requested)
        && effective.channels == requested.channels
        && effective.dsd_transport == requested.dsd_transport
        && effective.dsd_bit_rate == requested.dsd_bit_rate
        && (native || effective.sample_rate == format.sample_rate)
        && (native || effective.bit_depth == format.valid_bits)
}

/// Opens the stream for a negotiated format.
pub(crate) fn open_stream(format: &StreamFormat, events: EventSink) -> Result<StreamHandle, Error> {
    if !negotiation_matches(format) {
        // Something else was negotiated since (or this format never was):
        // settle it again and make sure it is still the same wire layout.
        let again = negotiate(&format.request)?;
        let same = again.interface_number == format.interface_number
            && again.alt_setting == format.alt_setting
            && again.sample_rate == format.sample_rate
            && again.subslot_bytes == format.subslot_bytes
            && again.valid_bits == format.valid_bits
            && again.endpoint_address == format.endpoint_address
            && again.pacing == format.pacing
            && again.native_dsd_signature == format.native_dsd_signature
            && again.channel_map == format.channel_map;
        if !same {
            return Err(Error::InvalidState(
                "the DAC no longer offers that format; negotiate again".to_string(),
            ));
        }
    }
    start_stream(events, format)
}

pub(crate) fn volume_route_signature() -> Option<HardwareVolumeControl> {
    SESSION_STATE
        .lock()
        .as_ref()
        .and_then(|state| state.hardware_volume_control.clone())
}

/// The exact libusb owner retained after a normal lock-requested stream retires.
pub(crate) fn retained_usb_handle_identity() -> Option<usize> {
    IDLE_LOCK
        .lock()
        .as_ref()
        .map(|lock| lock.handle.as_raw() as usize)
}

/// The DAC's capabilities, from the descriptor scan done at open.
pub(crate) fn capabilities() -> Result<Capabilities, Error> {
    let guard = SESSION_STATE.lock();
    let Some(state) = guard.as_ref() else {
        return Err(Error::InvalidState("no DAC is open".to_string()));
    };
    let Some(model) = state.capability_model.as_ref() else {
        return Err(Error::InvalidState(
            "the DAC has not been scanned".to_string(),
        ));
    };
    let alts = &model.alt_settings;
    let pcm: Vec<&AltSetting> = alts
        .iter()
        .filter(|alt| alt.format == AltFormat::Pcm)
        .collect();
    let dsd: Vec<&AltSetting> = alts
        .iter()
        .filter(|alt| alt.format == AltFormat::Dsd && alt.pacing.is_some())
        .collect();

    let mut pcm_sample_rates: Vec<u32> = pcm
        .iter()
        .flat_map(|alt| alt.sample_rates.iter().copied())
        .collect();
    pcm_sample_rates.sort_unstable();
    pcm_sample_rates.dedup();
    let mut pcm_bit_depths: Vec<u8> = pcm.iter().map(|alt| alt.valid_bits).collect();
    pcm_bit_depths.sort_unstable();
    pcm_bit_depths.dedup();
    let mut channel_counts: Vec<u16> = alts.iter().map(|alt| alt.channels).collect();
    channel_counts.sort_unstable();
    channel_counts.dedup();

    let supports = |alt: &AltSetting, rate: u32| {
        let ranges: Vec<SamplingFrequencySubrange> = alt
            .rate_ranges
            .iter()
            .map(|range| SamplingFrequencySubrange {
                min: range.min_hz,
                max: range.max_hz,
                res: range.step_hz,
            })
            .collect();
        sampling_frequency_ranges_support_rate(&ranges, rate)
    };
    let dop_max_carrier = alts
        .iter()
        .filter(|alt| matches!(alt.format, AltFormat::Pcm | AltFormat::Dop))
        .filter(|alt| alt.valid_bits >= 24 && alt.pacing.is_some())
        .flat_map(|alt| {
            DOP_CARRIER_RATES.iter().copied().filter(move |rate| {
                supports(alt, *rate)
                    && alt.max_packet_bytes
                        >= required_max_packet_bytes(
                            *rate,
                            alt.service_interval_us,
                            usize::from(alt.channels) * usize::from(alt.subslot_bytes),
                        )
                    && (alt.format != AltFormat::Dop
                        || playback_designs_dop_capacity(
                            alt.max_packet_bytes,
                            alt.service_interval_us,
                            *rate,
                        ))
            })
        })
        .max();
    let mut native_dsd_bit_rates: Vec<u32> = dsd
        .iter()
        .flat_map(|alt| {
            alt.sample_rates
                .iter()
                .filter_map(move |rate| native_dsd_bit_rate(*rate, alt.subslot_bytes))
        })
        .collect();
    native_dsd_bit_rates.sort_unstable();
    native_dsd_bit_rates.dedup();

    let uac_version = if alts.iter().any(|alt| alt.uac_version == UacVersion::V3) {
        UacVersion::V3
    } else if alts.iter().any(|alt| alt.uac_version == UacVersion::V2) {
        UacVersion::V2
    } else if alts.iter().any(|alt| alt.uac_version == UacVersion::V1) {
        UacVersion::V1
    } else {
        UacVersion::Unknown
    };
    let control = state.hardware_volume_control.as_ref();
    Ok(Capabilities {
        uac_version,
        link_speed: model.link_speed.unwrap_or(LinkSpeed::Unknown),
        alt_settings: alts.clone(),
        pcm_sample_rates,
        pcm_bit_depths,
        channel_counts,
        supports_pcm: !pcm.is_empty(),
        supports_dop: dop_max_carrier.is_some(),
        dop_max_dsd_bit_rate: dop_max_carrier.map(|carrier| carrier * 16),
        supports_native_dsd: !native_dsd_bit_rates.is_empty()
            || dsd.iter().any(|alt| {
                alt.uac_version != UacVersion::V3
                    && alt.rate_ranges.is_empty()
                    && alt.sample_rates.is_empty()
                    && native_dsd_minimum_fits(
                        alt.max_packet_bytes,
                        alt.service_interval_us,
                        alt.channels,
                        alt.subslot_bytes,
                    )
            }),
        native_dsd_bit_rates,
        has_explicit_feedback: alts.iter().any(|alt| alt.feedback_endpoint.is_some()),
        volume: control.map(|control| crate::volume::VolumeRange {
            min_db: crate::volume::raw_to_db(control.min_volume_raw),
            max_db: crate::volume::raw_to_db(control.max_volume_raw),
            res_db: crate::volume::raw_to_db(control.resolution_raw.max(1)),
        }),
        has_hardware_volume: control.is_some(),
        has_mute: control.is_some_and(|control| control.mute_channel.is_some()),
    })
}

/// Opens the DAC: checks the host's claims, scans the descriptors, takes the
/// idle handle and finds the hardware volume control.
pub(crate) fn open_device(fd: RawFd, info: &crate::capabilities::DeviceInfo) -> Result<(), Error> {
    if !cfg!(any(target_os = "linux", target_os = "android")) {
        return Err(Error::PlatformUnsupported);
    }
    let device = UsbDevice::try_new(
        fd,
        info.vendor_id,
        info.product_id,
        info.product_name.clone(),
        info.device_name.clone(),
        info.claimed_interfaces.clone(),
    )
    .map_err(Error::Claim)?;
    validate_registration_handoff(
        &device,
        info.configuration_id,
        &info.unavailable_feedback_interfaces,
    )
    .map_err(Error::Claim)?;
    register_device(device)?;
    discover_idle_volume_control();
    Ok(())
}

/// Register both directions through the native capture proof, preserving playback startup.
pub(crate) fn open_duplex_device(fd: RawFd, info: &crate::DeviceInfo) -> Result<(), Error> {
    if !cfg!(any(target_os = "linux", target_os = "android")) {
        return Err(Error::PlatformUnsupported);
    }
    if !info.unavailable_feedback_interfaces.is_empty() {
        return Err(Error::Claim(
            "duplex requires a complete combined claim plan".into(),
        ));
    }
    let device = UsbDevice::try_new(
        fd,
        info.vendor_id,
        info.product_id,
        info.product_name.clone(),
        info.device_name.clone(),
        info.claimed_interfaces.clone(),
    )
    .map_err(Error::Claim)?;
    super::session::register_duplex_device(device, info.configuration_id)?;
    discover_idle_volume_control();
    Ok(())
}

/// Capture is explicit: validate IN claims without broadening playback's
/// optional implicit-feedback claim policy or requiring an OUT alternate.
pub(crate) fn open_capture_device(fd: RawFd, info: &crate::DeviceInfo) -> Result<(), Error> {
    if !cfg!(any(target_os = "linux", target_os = "android")) {
        return Err(Error::PlatformUnsupported);
    }
    let device = UsbDevice::try_new(
        fd,
        info.vendor_id,
        info.product_id,
        info.product_name.clone(),
        info.device_name.clone(),
        info.claimed_interfaces.clone(),
    )
    .map_err(Error::Claim)?;
    if !info.unavailable_feedback_interfaces.is_empty() {
        return Err(Error::Claim(
            "capture registration does not accept playback feedback claim metadata".into(),
        ));
    }
    register_capture_device(device, info.configuration_id)
}

/// Descriptor-only vendor profile proof before any host claims.
pub(crate) fn vendor_pcm_preflight(
    fd: RawFd,
    info: &crate::DeviceInfo,
) -> Result<Option<(u8, u8)>, Error> {
    vendor_pcm_claims_preflight(fd, info).map(|profile| {
        profile.and_then(|(configuration, interfaces)| {
            (interfaces.len() == 1).then(|| (configuration, interfaces[0]))
        })
    })
}

pub(crate) fn vendor_pcm_claims_preflight(
    fd: RawFd,
    info: &crate::DeviceInfo,
) -> Result<Option<(u8, Vec<u8>)>, Error> {
    if !cfg!(any(target_os = "linux", target_os = "android")) {
        return Err(Error::PlatformUnsupported);
    }
    let device = UsbDevice::try_new(
        fd,
        info.vendor_id,
        info.product_id,
        info.product_name.clone(),
        info.device_name.clone(),
        Vec::new(),
    )
    .map_err(Error::Claim)?;
    let wrapped = wrap_android_usb_handle(&device).map_err(Error::Claim)?;
    let usb = wrapped.handle.device();
    let identity = usb
        .device_descriptor()
        .map_err(|e| Error::Claim(e.to_string()))?;
    if identity.vendor_id() != info.vendor_id || identity.product_id() != info.product_id {
        return Err(Error::Claim(
            "USB identity changed before vendor PCM preflight".into(),
        ));
    }
    let config = usb
        .active_config_descriptor()
        .map_err(|e| Error::Claim(e.to_string()))?;
    if info
        .configuration_id
        .is_some_and(|id| id != config.number())
    {
        return Err(Error::Claim(
            "USB configuration changed before vendor PCM preflight".into(),
        ));
    }
    vendor_pcm::discover(&wrapped.handle, &config, resolve_device_speed(&usb))
        .map(|profile| profile.map(|p| (config.number(), p.claimed_interfaces())))
        .map_err(Error::Claim)
}

pub(crate) fn capture_preflight_device(
    fd: RawFd,
    info: &crate::DeviceInfo,
) -> Result<(u8, Vec<u8>, Vec<u8>), Error> {
    if !cfg!(any(target_os = "linux", target_os = "android")) {
        return Err(Error::PlatformUnsupported);
    }
    let device = UsbDevice::try_new(
        fd,
        info.vendor_id,
        info.product_id,
        info.product_name.clone(),
        info.device_name.clone(),
        Vec::new(),
    )
    .map_err(Error::Claim)?;
    capture_stream::capture_claim_preflight(&device).map_err(|reason| Error::Unsupported {
        reason,
        offered: vec![],
    })
}

/// Closes the DAC: settles the original volume level as `action` says, stops the
/// session and waits for the output thread.
pub(crate) fn close_device(action: u32, timeout: Duration) -> bool {
    set_original_volume_action(action);
    clear_device();
    wait_for_session_stop(timeout)
}

pub(crate) const ORIGINAL_VOLUME_ACTION_KEEP: u32 = ORIGINAL_VOLUME_KEEP;
pub(crate) const ORIGINAL_VOLUME_ACTION_RESTORE: u32 = ORIGINAL_VOLUME_RESTORE;
pub(crate) const ORIGINAL_VOLUME_ACTION_FORGET: u32 = ORIGINAL_VOLUME_FORGET;

/// Read-only descriptor and clock-route preflight; never activates capture.
pub(crate) fn feedback_preflight(
    fd: RawFd,
    info: &crate::DeviceInfo,
) -> Result<(u8, Vec<u8>), Error> {
    let device = UsbDevice::try_new(
        fd,
        info.vendor_id,
        info.product_id,
        info.product_name.clone(),
        info.device_name.clone(),
        info.claimed_interfaces.clone(),
    )
    .map_err(Error::Claim)?;
    preflight_implicit_feedback_interfaces(&device).map_err(Error::Claim)
}

pub(crate) fn await_usb_retirement(timeout: Duration) -> bool {
    wait_for_session_stop(timeout) && !super::session::session_registered()
}

#[cfg(test)]
mod dsd_summary_tests {
    use super::*;

    #[test]
    fn native_summary_rates_are_valid_requests_without_overflow() {
        assert_eq!(native_dsd_bit_rate(352_800, 1), Some(2_822_400));
        assert_eq!(native_dsd_bit_rate(88_200, 4), Some(2_822_400));
        assert_eq!(native_dsd_bit_rate(u32::MAX, 4), None);
        assert_eq!(native_dsd_bit_rate(352_801, 1), None);
        assert_eq!(native_dsd_bit_rate(44_100, 4), None);
        assert_eq!(native_dsd_bit_rate(352_800, 0), None);
    }

    #[test]
    fn unknown_clock_native_support_still_requires_minimum_packet_capacity() {
        assert!(native_dsd_minimum_fits(706, 1000, 2, 1));
        assert!(!native_dsd_minimum_fits(705, 1000, 2, 1));
        // The resolved word width changes whole-frame packet rounding.
        assert!(!native_dsd_minimum_fits(710, 1000, 2, 4));
        assert!(native_dsd_minimum_fits(712, 1000, 2, 4));
        assert!(!native_dsd_minimum_fits(1023, 0, 2, 1));
        assert!(!native_dsd_minimum_fits(1023, 1000, 2, 0));
    }
}
