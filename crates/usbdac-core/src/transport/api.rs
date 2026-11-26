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

/// The transport format a request asks for.
pub(crate) fn playback_format_from_request(request: &StreamRequest) -> Result<PlaybackFormat, Error> {
    let invalid = |message: &str| Err(Error::InvalidArgument(message.to_string()));
    if request.channels == 0 || request.channels > 32 {
        return invalid("channels must be between 1 and 32");
    }
    let format = match request.encoding {
        Encoding::Pcm => {
            if request.sample_rate < 8_000 || request.sample_rate > 1_536_000 {
                return invalid("sample_rate must be between 8000 and 1536000 Hz");
            }
            if !matches!(request.bit_depth, 16 | 24 | 32) {
                return invalid("PCM bit_depth must be 16, 24 or 32");
            }
            PlaybackFormat {
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
                sample_rate: request.sample_rate,
                bit_depth: request.bit_depth,
                channels: request.channels,
                is_dop: true,
                dsd_transport: DsdTransportMode::DoP,
                dsd_bit_rate: 0,
            }
        }
        Encoding::DsdNative { bit_rate } => {
            if bit_rate < 2_822_400 || bit_rate % 32 != 0 {
                return invalid("the native DSD bit_rate must be a DSD rate (at least 2822400)");
            }
            PlaybackFormat {
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
    let candidate = select_stream_candidate(&usb_device, &handle.handle, effective, speed);
    cleanup_claimed_handle(handle, None, lock_requested);
    let candidate = candidate.map_err(|error| error.into_error(offered_alt_settings()))?;

    let native = effective.dsd_transport == DsdTransportMode::Native;
    Ok(StreamFormat {
        request: *request,
        encoding: request.encoding,
        sample_rate: if native && candidate.subslot_size > 0 {
            effective.dsd_bit_rate / (8 * u32::from(candidate.subslot_size))
        } else {
            effective.sample_rate
        },
        channels: candidate.channels,
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
        pacing: admitted_pacing(
            candidate.sync_type,
            candidate.feedback_endpoint.map(|feedback| feedback.address),
        ),
        clock,
    })
}

/// Whether the session already holds the negotiation of `format`.
fn negotiation_matches(format: &StreamFormat) -> bool {
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
            && again.valid_bits == format.valid_bits;
        if !same {
            return Err(Error::InvalidState(
                "the DAC no longer offers that format; negotiate again".to_string(),
            ));
        }
    }
    start_stream(events)
}

/// The DAC's capabilities, from the descriptor scan done at open.
pub(crate) fn capabilities() -> Result<Capabilities, Error> {
    let guard = SESSION_STATE.lock();
    let Some(state) = guard.as_ref() else {
        return Err(Error::InvalidState("no DAC is open".to_string()));
    };
    let Some(model) = state.capability_model.as_ref() else {
        return Err(Error::InvalidState("the DAC has not been scanned".to_string()));
    };
    let alts = &model.alt_settings;
    let pcm: Vec<&AltSetting> = alts.iter().filter(|alt| alt.format == AltFormat::Pcm).collect();
    let dsd: Vec<&AltSetting> = alts.iter().filter(|alt| alt.format == AltFormat::Dsd).collect();

    let mut pcm_sample_rates: Vec<u32> = pcm.iter().flat_map(|alt| alt.sample_rates.iter().copied()).collect();
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
    let dop_max_carrier = pcm
        .iter()
        .filter(|alt| alt.valid_bits >= 24)
        .flat_map(|alt| DOP_CARRIER_RATES.iter().copied().filter(move |rate| supports(alt, *rate)))
        .max();
    let mut native_dsd_bit_rates: Vec<u32> = dsd
        .iter()
        .flat_map(|alt| {
            alt.sample_rates
                .iter()
                .map(move |rate| rate.saturating_mul(8).saturating_mul(u32::from(alt.subslot_bytes)))
        })
        .collect();
    native_dsd_bit_rates.sort_unstable();
    native_dsd_bit_rates.dedup();

    let uac_version = if alts.iter().any(|alt| alt.uac_version == UacVersion::V2) {
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
        supports_native_dsd: !dsd.is_empty(),
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
pub(crate) fn open_device(
    fd: RawFd,
    info: &crate::capabilities::DeviceInfo,
) -> Result<(), Error> {
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
    register_device(device)?;
    discover_idle_volume_control();
    Ok(())
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
