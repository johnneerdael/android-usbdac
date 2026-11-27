//! JSON encoding of the core types, and decoding of the requests the host sends.
//!
//! The schemas are documented in `docs/API.md` and are part of the JNI
//! contract: all keys are snake_case, optional values are `null`, enums are
//! lower-case strings.

use serde_json::{json, Value};
use usbdac_core::{
    AltFormat, AltSetting, Capabilities, ClockChoice, ClockHop, ClockHopKind, DacConfig, DacEvent,
    DeviceInfo, Encoding, Error, ExplicitFeedbackPacket, FeedbackTrace, FeedbackTraceEvent,
    HandBack, LinkSpeed, Pacing, Quirk, QuirkOverride, RatePolicy,
    StartupVolume, StartupVolumeReport, StreamFormat, StreamRequest, StreamStats, SyncKind,
    UacVersion, VolumeRange, VolumeState,
};

fn uac(version: UacVersion) -> &'static str {
    match version {
        UacVersion::V1 => "v1",
        UacVersion::V2 => "v2",
        UacVersion::V3 => "v3",
        _ => "unknown",
    }
}

fn speed(speed: LinkSpeed) -> &'static str {
    match speed {
        LinkSpeed::Low => "low",
        LinkSpeed::Full => "full",
        LinkSpeed::High => "high",
        LinkSpeed::Super => "super",
        LinkSpeed::SuperPlus => "super_plus",
        _ => "unknown",
    }
}

fn sync(kind: SyncKind) -> &'static str {
    match kind {
        SyncKind::Asynchronous => "asynchronous",
        SyncKind::Adaptive => "adaptive",
        SyncKind::Synchronous => "synchronous",
        _ => "none",
    }
}

/// `{ "mode": "explicit_feedback", "feedback_endpoint": 129 }`; the endpoint is `null`
/// for every other mode.
pub fn pacing(pacing: Pacing) -> Value {
    let implicit = match pacing {
        Pacing::ImplicitFeedback {
            interface_number,
            alt_setting,
            endpoint,
        } => Some(
            json!({ "interface_number": interface_number, "alt_setting": alt_setting, "endpoint": endpoint }),
        ),
        _ => None,
    };
    json!({
        "mode": pacing.code(),
        "implicit_feedback": implicit,
        "feedback_endpoint": pacing.feedback_endpoint(),
    })
}

fn clock_hop(hop: &ClockHop) -> Value {
    let (selected_pin, pin_count) = match hop.kind {
        ClockHopKind::Selector {
            selected_pin,
            pin_count,
        } => (Some(selected_pin), Some(pin_count)),
        _ => (None, None),
    };
    let mut value = json!({
        "id": hop.id,
        "kind": hop.kind.code(),
        "selected_pin": selected_pin,
        "pin_count": pin_count,
    });
    if let ClockHopKind::RateMultiplier { numerator, denominator } = hop.kind {
        value["numerator"] = json!(numerator);
        value["denominator"] = json!(denominator);
    }
    value
}

/// Which clock the sample rate was set on and how it was reached.
pub fn clock_choice(choice: &ClockChoice) -> Value {
    json!({
        "control_interface": choice.control_interface,
        "source_id": choice.source_id,
        "resolution": choice.resolution.code(),
        "path": choice.path.iter().map(clock_hop).collect::<Vec<_>>(),
    })
}

fn alt(alt: &AltSetting) -> Value {
    json!({
        "interface_number": alt.interface_number,
        "alt_setting": alt.alt_setting,
        "endpoint_address": alt.endpoint_address,
        "format": match alt.format {
            AltFormat::Pcm => "pcm",
            AltFormat::Dsd => "dsd",
            AltFormat::Dop => "dop",
            _ => "other",
        },
        "channels": alt.channels,
        "subslot_bytes": alt.subslot_bytes,
        "valid_bits": alt.valid_bits,
        "rate_ranges": alt.rate_ranges.iter().map(|range| json!({
            "min_hz": range.min_hz,
            "max_hz": range.max_hz,
            "step_hz": range.step_hz,
        })).collect::<Vec<_>>(),
        "sample_rates": alt.sample_rates,
        "sync_type": sync(alt.sync_type),
        "feedback_endpoint": alt.feedback_endpoint,
        "max_packet_bytes": alt.max_packet_bytes,
        "service_interval_us": alt.service_interval_us,
        "uac_version": uac(alt.uac_version),
        "pacing": alt.pacing.map(pacing),
    })
}

pub fn volume_range(range: &VolumeRange) -> Value {
    json!({
        "min_db": range.min_db,
        "max_db": range.max_db,
        "res_db": range.res_db,
    })
}

pub fn volume_state(state: &VolumeState) -> Value {
    json!({
        "db": state.db,
        "fraction": state.fraction,
        "muted": state.muted,
    })
}

pub fn startup_report(report: &StartupVolumeReport) -> Value {
    json!({
        "original_db": report.original_db,
        "applied_db": report.applied_db,
        "limit_db": report.limit_db,
        "attenuated": report.attenuated,
        "clamped_to_minimum": report.clamped_to_minimum,
        "max_db": report.max_db,
    })
}

pub fn capabilities(capabilities: &Capabilities) -> Value {
    json!({
        "uac_version": uac(capabilities.uac_version),
        "link_speed": speed(capabilities.link_speed),
        "pcm_sample_rates": capabilities.pcm_sample_rates,
        "pcm_bit_depths": capabilities.pcm_bit_depths,
        "channel_counts": capabilities.channel_counts,
        "supports_pcm": capabilities.supports_pcm,
        "supports_dop": capabilities.supports_dop,
        "dop_max_dsd_bit_rate": capabilities.dop_max_dsd_bit_rate,
        "supports_native_dsd": capabilities.supports_native_dsd,
        "native_dsd_bit_rates": capabilities.native_dsd_bit_rates,
        "has_explicit_feedback": capabilities.has_explicit_feedback,
        "volume": capabilities.volume.as_ref().map(volume_range),
        "has_hardware_volume": capabilities.has_hardware_volume,
        "has_mute": capabilities.has_mute,
        "alt_settings": capabilities.alt_settings.iter().map(alt).collect::<Vec<_>>(),
    })
}

pub fn stream_format(format_id: u64, format: &StreamFormat) -> Value {
    let (encoding, dsd_bit_rate) = match format.encoding {
        Encoding::Pcm => ("pcm", None),
        Encoding::DoP => ("dop", None),
        Encoding::DsdNative { bit_rate } => ("dsd_native", Some(bit_rate)),
    };
    json!({
        "format_id": format_id,
        "channel_map": crate::capture_json::channel_map(&format.channel_map),
        "encoding": encoding,
        "dsd_bit_rate": dsd_bit_rate,
        "sample_rate": format.sample_rate,
        "channels": format.channels,
        "valid_bits": format.valid_bits,
        "subslot_bytes": format.subslot_bytes,
        "bytes_per_frame": format.bytes_per_frame(),
        "counter_rate_hz": format.counter_rate_hz(),
        "interface_number": format.interface_number,
        "alt_setting": format.alt_setting,
        "endpoint_address": format.endpoint_address,
        "sync_type": sync(format.sync_type),
        "explicit_feedback": format.explicit_feedback,
        "link_speed": speed(format.link_speed),
        "service_interval_us": format.service_interval_us,
        "rate_differs_from_request": format.rate_differs_from_request(),
        "pacing": pacing(format.pacing),
        "clock": format.clock.as_ref().map(clock_choice),
    })
}

fn explicit_feedback_packet(packet: &ExplicitFeedbackPacket) -> Value {
    json!({
        "outcome": packet.outcome.code(),
        "transfer_status": packet.transfer_status,
        "packet_status": packet.packet_status,
        "requested_bytes": packet.requested_bytes,
        "actual_bytes": packet.actual_bytes,
        "raw_bytes": packet.raw_bytes(),
        "frames_per_out_service": packet.frames_per_out_service,
        "decoded_rate_hz": packet.decoded_rate_hz,
    })
}

pub fn feedback_trace(trace: &FeedbackTrace, info: &DeviceInfo) -> Value {
    let explicit = trace.explicit.as_ref().map(|diag| json!({
        "endpoint": diag.endpoint,
        "type": diag.transfer_type,
        "link_speed": diag.link_speed,
        "interface_number": diag.interface_number,
        "alt_setting": diag.alt_setting,
        "out_endpoint": diag.out_endpoint,
        "out_service_interval_us": diag.out_service_interval_us,
        "out_max_packet_bytes": diag.out_max_packet_bytes,
        "out_bytes_per_frame": diag.out_bytes_per_frame,
        "nominal_rate_hz": diag.nominal_rate_hz,
        "feedback_interval": diag.feedback_interval,
        "feedback_refresh": diag.feedback_refresh,
        "feedback_service_interval_us": diag.feedback_service_interval_us,
        "feedback_max_packet_bytes": diag.feedback_max_packet_bytes,
        "requested_packet_bytes": diag.requested_packet_bytes,
        "transfer_slots": diag.transfer_slots,
        "packets_per_transfer": diag.packets_per_transfer,
        "transfer_timeout_ms": diag.transfer_timeout_ms,
        "per_service_interval_quirk": diag.per_service_interval_quirk,
        "post_alt_readback_hz": diag.post_alt_readback_hz,
        "timebase": diag.timebase,
        "inferred_timebase": diag.inferred_timebase,
        "reused_timebase": diag.reused_timebase,
        "successful_submissions": diag.successful_submissions,
        "submit_failures": diag.submit_failures,
        "last_submit_error": diag.last_submit_error,
        "transfer_status_counts": diag.transfer_status_counts,
        "packet_status_counts": diag.packet_status_counts,
        "accepted_reports": diag.accepted_reports,
        "empty_polls": diag.empty_polls,
        "rejected_reports": diag.rejected_reports,
        "packet_samples": diag.packet_samples().map(explicit_feedback_packet).collect::<Vec<_>>(),
    }));
    let entries: Vec<_> = trace.entries.iter().map(|entry| {
        let mut value = match &entry.event {
            FeedbackTraceEvent::Submit { slot, status } => json!({
                "event": "submit", "slot": slot, "status": status,
            }),
            FeedbackTraceEvent::Completion { slot, status, actual_bytes } => json!({
                "event": "completion", "slot": slot, "status": status, "actual_bytes": actual_bytes,
            }),
            FeedbackTraceEvent::ExplicitPacket { index, sample } => {
                let mut packet = explicit_feedback_packet(sample);
                packet["event"] = json!("explicit_packet");
                packet["index"] = json!(index);
                packet
            }
            FeedbackTraceEvent::ImplicitSource {
                interface_number, alt_setting, endpoint, service_interval_us, max_packet_bytes, bytes_per_frame,
            } => json!({
                "event": "implicit_source", "interface_number": interface_number,
                "alt_setting": alt_setting, "endpoint": endpoint, "service_interval_us": service_interval_us,
                "max_packet_bytes": max_packet_bytes, "bytes_per_frame": bytes_per_frame,
            }),
            FeedbackTraceEvent::ImplicitPacket { index, status, requested_bytes, actual_bytes } => json!({
                "event": "implicit_packet", "index": index, "status": status,
                "requested_bytes": requested_bytes, "actual_bytes": actual_bytes,
            }),
            FeedbackTraceEvent::Lock { locked, stale } => json!({
                "event": "lock", "locked": locked, "stale": stale,
            }),
            FeedbackTraceEvent::Timebase { unit, inferred } => json!({
                "event": "timebase", "unit": unit, "inferred": inferred,
            }),
            _ => json!({ "event": "unknown" }),
        };
        value["elapsed_us"] = json!(entry.elapsed_us);
        value
    }).collect();
    json!({
        "schema_version": 1,
        "driver_version": env!("CARGO_PKG_VERSION"),
        "usb": {
            "vendor_id": info.vendor_id, "product_id": info.product_id,
            "configuration_id": info.configuration_id, "claimed_interfaces": info.claimed_interfaces,
            "unavailable_feedback_interfaces": info.unavailable_feedback_interfaces,
        },
        "format": stream_format(0, &trace.format),
        "explicit": explicit,
        "entries": entries,
        "dropped_entries": trace.dropped_entries,
        "failure": trace.failure,
    })
}

pub fn stream_stats(stats: &StreamStats) -> Value {
    let Value::Object(mut value) = json!({
        "state": stats.state.code(),
        "bit_perfect": stats.bit_perfect,
        "bit_perfect_blocker": stats.bit_perfect_blocker.map(|blocker| blocker.code()),
        "software_gain": stats.software_gain,
        "underruns": stats.underruns,
        "lost_frames": stats.lost_frames,
        "short_packets": stats.short_packets,
        "zero_packets": stats.zero_packets,
        "urb_timeouts": stats.urb_timeouts,
        "completed_transfers": stats.completed_transfers,
        "frames_written": stats.frames_written,
        "frames_completed": stats.frames_completed,
        "played_frames": stats.played_frames,
        "latency_frames": stats.latency_frames,
        "latency_ms": stats.latency_ms,
        "queued_frames": stats.queued_frames,
        "buffer_ms": stats.buffer_ms,
        "buffer_target_ms": stats.buffer_target_ms,
        "buffer_capacity_ms": stats.buffer_capacity_ms,
        "transfer_queue_depth": stats.transfer_queue_depth,
        "last_turnaround_us": stats.last_turnaround_us,
        "max_turnaround_us": stats.max_turnaround_us,
        "last_submit_gap_us": stats.last_submit_gap_us,
        "max_submit_gap_us": stats.max_submit_gap_us,
        "effective_rate_hz": stats.effective_rate_hz,
    }) else {
        unreachable!("stats serialize as an object")
    };
    let Value::Object(feedback) = json!({
        "feedback_active": stats.feedback_active,
        "feedback_locked": stats.feedback_locked,
        "feedback_empty": stats.feedback_empty,
        "implicit_fifo_depth": stats.implicit_fifo_depth,
        "implicit_fifo_high_water": stats.implicit_fifo_high_water,
        "implicit_waits": stats.implicit_waits,
        "implicit_empty_bus": stats.implicit_empty_bus,
        "implicit_overruns": stats.implicit_overruns,
        "implicit_in_transfers": stats.implicit_in_transfers,
        "implicit_out_of_order": stats.implicit_out_of_order,
        "implicit_invalid_packets": stats.implicit_invalid_packets,
        "implicit_measured_frames": stats.implicit_measured_frames,
        "implicit_scheduled_frames": stats.implicit_scheduled_frames,

        "feedback_reports": stats.feedback_reports,
        "feedback_timeouts": stats.feedback_timeouts,
        "feedback_errors": stats.feedback_errors,
        "feedback_rejected": stats.feedback_rejected,
        "feedback_age_ms": stats.feedback_age_ms,
        "feedback_rate_hz": stats.feedback_rate_hz,
        "feedback_stale": stats.feedback_stale,
    }) else {
        unreachable!("feedback stats serialize as an object")
    };
    value.extend(feedback);
    Value::Object(value)
}

/// Event type codes of `onEvent(type, a, b, message)`.
pub mod event_type {
    pub const STATE_CHANGED: i32 = 0;
    pub const UNDERRUN: i32 = 1;
    pub const LOST_FRAMES: i32 = 2;
    pub const BIT_PERFECT_CHANGED: i32 = 3;
    pub const VOLUME_CHANGED: i32 = 4;
    pub const FEEDBACK_STALE: i32 = 5;
    pub const ERROR: i32 = 6;
    pub const DEVICE_LOST: i32 = 7;
}

/// Numeric code of an error, for `onEvent(ERROR, code, ...)`.
pub fn error_code(error: &Error) -> i64 {
    match error {
        Error::Busy => 1,
        Error::PlatformUnsupported => 2,
        Error::Claim(_) => 3,
        Error::Unsupported { .. } => 4,
        Error::ClockUnverified { .. } => 5,
        Error::DeviceLost => 6,
        Error::Transfer(_) => 7,
        Error::Control(_) => 8,
        Error::InvalidState(_) => 9,
        Error::InvalidArgument(_) => 10,
        Error::ImplicitFeedbackUnsupported => 11,
        Error::VolumeUnavailable => 12,
        _ => 0,
    }
}

/// The `(type, a, b, message)` of an `onEvent` call.
pub fn event(event: &DacEvent) -> (i32, i64, f64, Option<String>) {
    match event {
        DacEvent::StateChanged(state) => (
            event_type::STATE_CHANGED,
            i64::from(state.code()),
            0.0,
            None,
        ),
        DacEvent::Underrun => (event_type::UNDERRUN, 0, 0.0, None),
        DacEvent::LostFrames { frames } => (
            event_type::LOST_FRAMES,
            i64::try_from(*frames).unwrap_or(i64::MAX),
            0.0,
            None,
        ),
        DacEvent::BitPerfectChanged(on) => {
            (event_type::BIT_PERFECT_CHANGED, i64::from(*on), 0.0, None)
        }
        DacEvent::VolumeChanged(state) => (
            event_type::VOLUME_CHANGED,
            match state.muted {
                Some(true) => 1,
                Some(false) => 0,
                None => -1,
            },
            state.db,
            Some(volume_state(state).to_string()),
        ),
        DacEvent::FeedbackStale => (event_type::FEEDBACK_STALE, 0, 0.0, None),
        DacEvent::Error(error) => (
            event_type::ERROR,
            error_code(error),
            0.0,
            Some(format!("usbdac:{}: {}", error.code(), error.message())),
        ),
        DacEvent::DeviceLost => (event_type::DEVICE_LOST, 0, 0.0, None),
        _ => (-1, 0, 0.0, Some(format!("{event:?}"))),
    }
}

// ---- decoding -----------------------------------------------------------

/// A request the host got wrong.
pub struct BadRequest(pub String);

type Decoded<T> = Result<T, BadRequest>;

fn parse(text: &str, what: &str) -> Decoded<Value> {
    serde_json::from_str(text).map_err(|error| BadRequest(format!("{what}: {error}")))
}

fn object<'a>(value: &'a Value, what: &str) -> Decoded<&'a serde_json::Map<String, Value>> {
    value
        .as_object()
        .ok_or_else(|| BadRequest(format!("{what} must be a JSON object")))
}

fn uint(map: &serde_json::Map<String, Value>, key: &str) -> Decoded<Option<u64>> {
    match map.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_u64()
            .map(Some)
            .ok_or_else(|| BadRequest(format!("{key} must be a non-negative integer"))),
    }
}

fn required_uint(map: &serde_json::Map<String, Value>, key: &str) -> Decoded<u64> {
    uint(map, key)?.ok_or_else(|| BadRequest(format!("{key} is required")))
}

fn narrow<T: TryFrom<u64>>(value: u64, key: &str) -> Decoded<T> {
    T::try_from(value).map_err(|_| BadRequest(format!("{key} is out of range")))
}

fn text(map: &serde_json::Map<String, Value>, key: &str) -> Decoded<Option<String>> {
    match map.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => Ok(Some(text.clone())),
        Some(_) => Err(BadRequest(format!("{key} must be a string"))),
    }
}

fn number(map: &serde_json::Map<String, Value>, key: &str) -> Decoded<Option<f64>> {
    match map.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_f64()
            .map(Some)
            .ok_or_else(|| BadRequest(format!("{key} must be a number"))),
    }
}

fn boolean(map: &serde_json::Map<String, Value>, key: &str) -> Decoded<Option<bool>> {
    match map.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(value)) => Ok(Some(*value)),
        Some(_) => Err(BadRequest(format!("{key} must be true or false"))),
    }
}

pub fn parse_request(json: &str) -> Decoded<StreamRequest> {
    let value = parse(json, "stream request")?;
    let map = object(&value, "stream request")?;
    let channels: u16 = narrow(required_uint(map, "channels")?, "channels")?;
    let rate_policy = match text(map, "rate_policy")?.as_deref() {
        None | Some("exact") => RatePolicy::Exact,
        Some("nearest") | Some("nearest_supported") => RatePolicy::NearestSupported,
        Some(other) => return Err(BadRequest(format!("unknown rate_policy \"{other}\""))),
    };
    let request = match text(map, "encoding")?.as_deref() {
        None | Some("pcm") => StreamRequest::pcm(
            narrow(required_uint(map, "sample_rate")?, "sample_rate")?,
            channels,
            narrow(required_uint(map, "bit_depth")?, "bit_depth")?,
        ),
        Some("dop") => {
            let mut request = match uint(map, "dsd_bit_rate")? {
                Some(bit_rate) => StreamRequest::dop(narrow(bit_rate, "dsd_bit_rate")?, channels),
                None => StreamRequest::pcm(0, channels, 24),
            };
            request.encoding = Encoding::DoP;
            // An explicit carrier rate or depth wins over the derived ones.
            if let Some(rate) = uint(map, "sample_rate")? {
                request.sample_rate = narrow(rate, "sample_rate")?;
            }
            if let Some(depth) = uint(map, "bit_depth")? {
                request.bit_depth = narrow(depth, "bit_depth")?;
            }
            request
        }
        Some("dsd_native") => StreamRequest::dsd_native(
            narrow(required_uint(map, "dsd_bit_rate")?, "dsd_bit_rate")?,
            channels,
        ),
        Some(other) => return Err(BadRequest(format!("unknown encoding \"{other}\""))),
    };
    Ok(request.with_rate_policy(rate_policy))
}

pub fn parse_device_info(json: &str) -> Decoded<DeviceInfo> {
    let value = parse(json, "device info")?;
    let map = object(&value, "device info")?;
    let mut info = DeviceInfo::new(
        narrow(required_uint(map, "vendor_id")?, "vendor_id")?,
        narrow(required_uint(map, "product_id")?, "product_id")?,
        text(map, "product_name")?.unwrap_or_default(),
    );
    info.manufacturer_name = text(map, "manufacturer_name")?;
    info.device_name = text(map, "device_name")?;
    if let Some(list) = map.get("claimed_interfaces") {
        let list = list
            .as_array()
            .ok_or_else(|| BadRequest("claimed_interfaces must be an array".into()))?;
        for entry in list {
            let number = entry
                .as_u64()
                .ok_or_else(|| BadRequest("claimed_interfaces holds interface numbers".into()))?;
            info.claimed_interfaces
                .push(narrow(number, "claimed_interfaces")?);
        }
    }
    if let Some(value) = uint(map, "configuration_id")? {
        info.configuration_id = Some(narrow(value, "configuration_id")?);
    }
    if let Some(list) = map.get("unavailable_feedback_interfaces") {
        for entry in list
            .as_array()
            .ok_or_else(|| BadRequest("unavailable_feedback_interfaces must be an array".into()))?
        {
            info.unavailable_feedback_interfaces.push(narrow(
                entry.as_u64().ok_or_else(|| {
                    BadRequest("unavailable_feedback_interfaces holds interface numbers".into())
                })?,
                "unavailable_feedback_interfaces",
            )?);
        }
    }
    Ok(info)
}

fn parse_quirk(value: &Value) -> Decoded<Quirk> {
    let unknown = |name: &str| BadRequest(format!("unknown quirk \"{name}\""));
    match value {
        Value::String(name) => match name.as_str() {
            "skip_clock_validation" => Ok(Quirk::SkipClockValidation),
            "ignore_invalid_sample_rate" => Ok(Quirk::IgnoreInvalidSampleRate),
            "require_verified_rate" => Ok(Quirk::RequireVerifiedRate),
            "prefer_padded_24bit_transport" => Ok(Quirk::PreferPadded24BitTransport),
            "dsd_big_endian" => Ok(Quirk::DsdBigEndian),
            "dsd_bit_reverse" => Ok(Quirk::DsdBitReverse),
            "explicit_feedback_per_service_interval" => {
                Ok(Quirk::ExplicitFeedbackPerServiceInterval)
            }
            other => Err(unknown(other)),
        },
        Value::Object(map) if map.len() == 1 => {
            let (name, argument) = map.iter().next().expect("one entry");
            let number = argument
                .as_u64()
                .ok_or_else(|| BadRequest(format!("quirk {name} takes a number")))?;
            match name.as_str() {
                "settle_delay_ms" => Ok(Quirk::SettleDelayMs(number)),
                "dsd_subslot_size" => {
                    Ok(Quirk::DsdSubslotSize(narrow(number, "dsd_subslot_size")?))
                }
                other => Err(unknown(other)),
            }
        }
        _ => Err(BadRequest(
            "a quirk is a name or a single-entry object".into(),
        )),
    }
}

pub fn parse_config(json: Option<&str>) -> Decoded<DacConfig> {
    let mut config = DacConfig::default();
    let Some(json) = json.filter(|json| !json.trim().is_empty()) else {
        return Ok(config);
    };
    let value = parse(json, "config")?;
    let map = object(&value, "config")?;
    if let Some(ms) = uint(map, "buffer_target_ms")? {
        config.buffer_target_ms = narrow(ms, "buffer_target_ms")?;
    }
    if let Some(ms) = uint(map, "buffer_capacity_ms")? {
        config.buffer_capacity_ms = narrow(ms, "buffer_capacity_ms")?;
    }
    if let Some(ms) = uint(map, "start_threshold_ms")? {
        config.start_threshold_ms = narrow(ms, "start_threshold_ms")?;
    }
    if let Some(ms) = uint(map, "processing_buffer_target_ms")? {
        config.processing_buffer_target_ms = narrow(ms, "processing_buffer_target_ms")?;
    }
    if let Some(value) = boolean(map, "require_verified_rate")? {
        config.require_verified_rate = value;
    }
    config.clock_settle_delay_ms = uint(map, "clock_settle_delay_ms")?;
    config.dsd_big_endian = boolean(map, "dsd_big_endian")?;
    if let Some(subslot) = uint(map, "dsd_subslot")? {
        config.dsd_subslot = Some(narrow(subslot, "dsd_subslot")?);
    }
    config.padded_24bit = boolean(map, "padded_24bit")?;
    if let Some(value) = boolean(map, "disable_feedback")? {
        config.disable_feedback = value;
    }
    if let Some(value) = boolean(map, "capture_feedback_trace")? {
        config.capture_feedback_trace = value;
    }
    if let Some(value) = boolean(map, "allow_unsynchronized_async")? {
        config.allow_unsynchronized_async = value;
    }
    if let Some(value) = boolean(map, "realtime_priority")? {
        config.realtime_priority = value;
    }
    if let Some(ms) = uint(map, "bus_queue_ms")? {
        config.bus_queue_ms = narrow(ms, "bus_queue_ms")?;
    }
    if let Some(count) = uint(map, "transfers_in_flight")? {
        config.transfers_in_flight = narrow(count, "transfers_in_flight")?;
    }
    if let Some(period) = uint(map, "transfer_period_us")? {
        config.transfer_period_us = Some(narrow(period, "transfer_period_us")?);
    }
    if let Some(value) = boolean(map, "cancel_in_flight_on_flush")? {
        config.cancel_in_flight_on_flush = value;
    }
    if let Some(value) = boolean(map, "require_hardware_volume")? {
        config.require_hardware_volume = value;
    }
    let max_db = number(map, "startup_volume_max_db")?;
    match text(map, "startup_volume")?.as_deref() {
        None => {
            if let Some(max_db) = max_db {
                config.startup_volume = StartupVolume::Attenuate { max_db };
            }
        }
        Some("attenuate") => {
            config.startup_volume = StartupVolume::Attenuate {
                max_db: max_db.unwrap_or(-40.0),
            };
        }
        Some("keep") => config.startup_volume = StartupVolume::Keep,
        Some(other) => return Err(BadRequest(format!("unknown startup_volume \"{other}\""))),
    }
    match text(map, "hand_back")?.as_deref() {
        None => {}
        Some("restore_original") => config.hand_back = HandBack::RestoreOriginal,
        Some("keep") => config.hand_back = HandBack::Keep,
        Some(other) => return Err(BadRequest(format!("unknown hand_back \"{other}\""))),
    }
    if let Some(list) = map.get("quirks") {
        let list = list
            .as_array()
            .ok_or_else(|| BadRequest("quirks must be an array".into()))?;
        for entry in list {
            let entry = object(entry, "a quirk entry")?;
            let quirks = entry
                .get("quirks")
                .and_then(Value::as_array)
                .ok_or_else(|| BadRequest("a quirk entry needs a quirks array".into()))?
                .iter()
                .map(parse_quirk)
                .collect::<Decoded<Vec<_>>>()?;
            config.quirks.push(QuirkOverride {
                vendor_id: narrow(required_uint(entry, "vendor_id")?, "vendor_id")?,
                product_id: narrow(required_uint(entry, "product_id")?, "product_id")?,
                name_contains: text(entry, "name_contains")?,
                quirks,
            });
        }
    }
    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_decode_for_each_encoding() {
        let pcm = parse_request(r#"{"sample_rate":44100,"channels":2,"bit_depth":16}"#)
            .ok()
            .unwrap();
        assert_eq!(pcm, StreamRequest::pcm(44_100, 2, 16));

        let dop = parse_request(r#"{"encoding":"dop","dsd_bit_rate":2822400,"channels":2}"#)
            .ok()
            .unwrap();
        assert_eq!(dop, StreamRequest::dop(2_822_400, 2));

        let native = parse_request(
            r#"{"encoding":"dsd_native","dsd_bit_rate":5644800,"channels":2,"rate_policy":"nearest"}"#,
        )
        .ok()
        .unwrap();
        assert_eq!(
            native,
            StreamRequest::dsd_native(5_644_800, 2).with_rate_policy(RatePolicy::NearestSupported)
        );
    }

    #[test]
    fn a_bad_request_says_what_is_wrong() {
        for (json, needle) in [
            ("[]", "JSON object"),
            (
                r#"{"channels":2,"bit_depth":16}"#,
                "sample_rate is required",
            ),
            (
                r#"{"channels":2,"sample_rate":44100,"bit_depth":16,"encoding":"mp3"}"#,
                "unknown encoding",
            ),
            (
                r#"{"channels":99999,"sample_rate":44100,"bit_depth":16}"#,
                "out of range",
            ),
            ("{", "stream request"),
        ] {
            let error = parse_request(json)
                .err()
                .unwrap_or_else(|| panic!("{json} must fail"));
            assert!(error.0.contains(needle), "{json}: {}", error.0);
        }
    }

    #[test]
    fn device_info_and_config_decode() {
        let info = parse_device_info(
            r#"{"vendor_id":12230,"product_id":61546,"product_name":"MOONDROP Dawn Pro","claimed_interfaces":[0,1,2],"device_name":"/dev/bus/usb/001/004"}"#,
        )
        .ok()
        .unwrap();
        assert_eq!(info.vendor_id, 12230);
        assert_eq!(info.claimed_interfaces, vec![0, 1, 2]);
        assert_eq!(info.device_name.as_deref(), Some("/dev/bus/usb/001/004"));

        let config = parse_config(Some(
            r#"{"buffer_target_ms":200,"require_verified_rate":false,"hand_back":"keep", "allow_unsynchronized_async":true,
                "dsd_big_endian":true,
                "quirks":[{"vendor_id":1,"product_id":2,"name_contains":"X",
                           "quirks":["skip_clock_validation",{"settle_delay_ms":80},{"dsd_subslot_size":2},"explicit_feedback_per_service_interval"]}]}"#,
        ))
        .ok()
        .unwrap();
        assert_eq!(config.buffer_target_ms, 200);
        assert!(!config.require_verified_rate);
        assert_eq!(config.hand_back, HandBack::Keep);
        assert!(config.allow_unsynchronized_async);
        assert_eq!(config.dsd_big_endian, Some(true));
        assert_eq!(
            (config.bus_queue_ms, config.transfers_in_flight),
            (16, 8),
            "unset: the defaults"
        );
        assert!(config.cancel_in_flight_on_flush, "unset: on");
        assert_eq!(config.quirks.len(), 1);
        assert_eq!(
            (config.quirks[0].vendor_id, config.quirks[0].product_id),
            (1, 2)
        );
        assert_eq!(config.quirks[0].name_contains.as_deref(), Some("X"));
        assert_eq!(
            config.quirks[0].quirks,
            vec![
                Quirk::SkipClockValidation,
                Quirk::SettleDelayMs(80),
                Quirk::DsdSubslotSize(2),
                Quirk::ExplicitFeedbackPerServiceInterval,
            ]
        );
        assert_eq!(parse_config(None).ok().unwrap(), DacConfig::default());
        assert!(parse_config(Some(r#"{"hand_back":"sometimes"}"#)).is_err());
        assert!(parse_config(Some(
            r#"{"quirks":[{"vendor_id":1,"product_id":2,"quirks":["nope"]}]}"#
        ))
        .is_err());
    }

    #[test]
    fn the_bus_queue_keys_decode_and_a_bad_value_is_refused() {
        let config = parse_config(Some(r#"{"bus_queue_ms":32,"transfers_in_flight":4,"transfer_period_us":1500}"#))
            .ok()
            .unwrap();
        assert_eq!(config.transfer_period_us, Some(1_500));
        assert!(config.validate().is_ok());
        let bad = parse_config(Some(r#"{"transfer_period_us":0}"#)).ok().unwrap();
        assert!(bad.validate().is_err());
        assert_eq!((config.bus_queue_ms, config.transfers_in_flight), (32, 4));
        config.validate().unwrap();
        assert!(
            parse_config(Some(r#"{"bus_queue_ms":-1}"#)).is_err(),
            "not a non-negative integer"
        );
        // Out-of-range values decode; the core's validation names them at open.
        let out_of_range = parse_config(Some(r#"{"bus_queue_ms":3}"#)).ok().unwrap();
        assert!(matches!(
            out_of_range.validate(),
            Err(Error::InvalidArgument(_))
        ));
    }

    #[test]
    fn the_flush_cancel_key_decodes_and_defaults_on() {
        assert!(
            parse_config(Some("{}"))
                .ok()
                .unwrap()
                .cancel_in_flight_on_flush
        );
        let off = parse_config(Some(r#"{"cancel_in_flight_on_flush":false}"#))
            .ok()
            .unwrap();
        assert!(!off.cancel_in_flight_on_flush);
        assert!(parse_config(Some(r#"{"cancel_in_flight_on_flush":"no"}"#)).is_err());
    }

    #[test]
    fn the_startup_volume_keys_decode_and_default_to_protection() {
        let defaults = parse_config(Some("{}")).ok().unwrap();
        assert_eq!(
            defaults.startup_volume,
            StartupVolume::Attenuate { max_db: -40.0 }
        );
        assert!(defaults.require_hardware_volume);

        let keep = parse_config(Some(
            r#"{"startup_volume":"keep","require_hardware_volume":false}"#,
        ))
        .ok()
        .unwrap();
        assert_eq!(keep.startup_volume, StartupVolume::Keep);
        assert!(!keep.require_hardware_volume);

        let ceiling = parse_config(Some(
            r#"{"startup_volume":"attenuate","startup_volume_max_db":-25.5}"#,
        ))
        .ok()
        .unwrap();
        assert_eq!(
            ceiling.startup_volume,
            StartupVolume::Attenuate { max_db: -25.5 }
        );
        // A ceiling alone means attenuate; "keep" ignores it.
        let alone = parse_config(Some(r#"{"startup_volume_max_db":-30}"#))
            .ok()
            .unwrap();
        assert_eq!(
            alone.startup_volume,
            StartupVolume::Attenuate { max_db: -30.0 }
        );
        let ignored = parse_config(Some(
            r#"{"startup_volume":"keep","startup_volume_max_db":-30}"#,
        ))
        .ok()
        .unwrap();
        assert_eq!(ignored.startup_volume, StartupVolume::Keep);

        assert!(parse_config(Some(r#"{"startup_volume":"loud"}"#)).is_err());
        assert!(parse_config(Some(r#"{"startup_volume_max_db":"quiet"}"#)).is_err());
        // Out-of-range values decode; validation names them at open.
        let loud = parse_config(Some(r#"{"startup_volume_max_db":6}"#))
            .ok()
            .unwrap();
        assert!(matches!(loud.validate(), Err(Error::InvalidArgument(_))));
    }

    #[test]
    fn the_startup_report_and_the_volume_error_encode_as_documented() {
        let (kind, code, _, message) = event(&DacEvent::Error(Error::VolumeUnavailable));
        assert_eq!((kind, code), (6, 12));
        assert!(message.unwrap().starts_with("usbdac:volume_unavailable: "));
        assert_eq!(error_code(&Error::VolumeUnavailable), 12);
    }

    #[test]
    fn implicit_source_and_claim_plan_preserve_identity() {
        assert_eq!(
            pacing(Pacing::ImplicitFeedback {
                interface_number: 2,
                alt_setting: 3,
                endpoint: 0x82
            }),
            json!({"mode":"implicit_feedback","feedback_endpoint":null,"implicit_feedback":{"interface_number":2,"alt_setting":3,"endpoint":130}})
        );
        let info = parse_device_info(r#"{"vendor_id":1,"product_id":2,"configuration_id":4,"claimed_interfaces":[0,1,2],"unavailable_feedback_interfaces":[3]}"#).ok().unwrap();
        assert_eq!(info.configuration_id, Some(4));
        assert_eq!(info.claimed_interfaces, [0, 1, 2]);
        assert_eq!(info.unavailable_feedback_interfaces, [3]);
        assert!(
            parse_config(Some(r#"{"disable_feedback":true}"#))
                .ok()
                .unwrap()
                .disable_feedback
        );
        assert!(!parse_config(None).ok().unwrap().disable_feedback);
    }

    #[test]
    fn pacing_encodes_its_mode_and_only_an_explicit_feedback_endpoint() {
        assert_eq!(
            pacing(Pacing::ExplicitFeedback { endpoint: 0x81 }),
            json!({ "mode": "explicit_feedback", "feedback_endpoint": 129, "implicit_feedback": null })
        );
        for (value, mode) in [
            (Pacing::Adaptive, "adaptive"),
            (Pacing::Synchronous, "synchronous"),
            (Pacing::Unsynchronized, "unsynchronized"),
        ] {
            assert_eq!(
                pacing(value),
                json!({ "mode": mode, "feedback_endpoint": null, "implicit_feedback": null })
            );
        }
    }

    #[test]
    fn an_adc3_multiplier_keeps_the_public_kind_and_reports_its_verified_ratio() {
        assert_eq!(clock_hop(&ClockHop { id: 7, kind: ClockHopKind::RateMultiplier {
            numerator: 2, denominator: 1,
        }}), json!({ "id": 7, "kind": "multiplier", "selected_pin": null,
            "pin_count": null, "numerator": 2, "denominator": 1 }));
    }

    #[test]
    fn a_clock_choice_encodes_the_path_in_the_order_followed() {
        use usbdac_core::ClockResolution;
        let choice = ClockChoice {
            control_interface: 0,
            source_id: 5,
            path: vec![
                ClockHop {
                    id: 1,
                    kind: ClockHopKind::Terminal,
                },
                ClockHop {
                    id: 9,
                    kind: ClockHopKind::Selector {
                        selected_pin: 2,
                        pin_count: 2,
                    },
                },
                ClockHop {
                    id: 8,
                    kind: ClockHopKind::Multiplier,
                },
                ClockHop {
                    id: 5,
                    kind: ClockHopKind::Source,
                },
            ],
            resolution: ClockResolution::Resolved,
        };
        assert_eq!(
            clock_choice(&choice),
            json!({
                "control_interface": 0,
                "source_id": 5,
                "resolution": "resolved",
                "path": [
                    { "id": 1, "kind": "terminal", "selected_pin": null, "pin_count": null },
                    { "id": 9, "kind": "selector", "selected_pin": 2, "pin_count": 2 },
                    { "id": 8, "kind": "multiplier", "selected_pin": null, "pin_count": null },
                    { "id": 5, "kind": "source", "selected_pin": null, "pin_count": null },
                ],
            })
        );

        let fallback = ClockChoice {
            control_interface: 0,
            source_id: 4,
            path: vec![ClockHop {
                id: 4,
                kind: ClockHopKind::Source,
            }],
            resolution: ClockResolution::FallbackFirstSource,
        };
        let value = clock_choice(&fallback);
        assert_eq!(value["resolution"], "fallback_first_source");
        assert_eq!(value["path"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn events_encode_to_the_documented_codes() {
        use usbdac_core::StreamState;
        assert_eq!(
            event(&DacEvent::StateChanged(StreamState::Paused)),
            (0, 2, 0.0, None)
        );
        assert_eq!(event(&DacEvent::Underrun), (1, 0, 0.0, None));
        assert_eq!(
            event(&DacEvent::LostFrames { frames: 44 }),
            (2, 44, 0.0, None)
        );
        assert_eq!(event(&DacEvent::BitPerfectChanged(true)), (3, 1, 0.0, None));
        assert_eq!(event(&DacEvent::FeedbackStale), (5, 0, 0.0, None));
        assert_eq!(event(&DacEvent::DeviceLost), (7, 0, 0.0, None));
        let (kind, code, _, message) = event(&DacEvent::Error(Error::Transfer("boom".into())));
        assert_eq!((kind, code), (6, 7));
        assert_eq!(message.as_deref(), Some("usbdac:transfer: boom"));
        let (kind, muted, db, message) = event(&DacEvent::VolumeChanged(VolumeState {
            db: -12.5,
            fraction: 0.6,
            muted: Some(true),
        }));
        assert_eq!((kind, muted, db), (4, 1, -12.5));
        let parsed: Value = serde_json::from_str(&message.unwrap()).unwrap();
        assert_eq!(parsed["fraction"], 0.6);
    }
}
