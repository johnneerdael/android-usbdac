//! Stable capture-only JSON; playback schemas remain unchanged.
use crate::json::BadRequest;
use serde_json::{json, Value};
use usbdac_core::{CaptureRequest, CaptureState, ChannelMap, ChannelPosition, DacCaptureStream};

const POSITIONS: &[(&str, ChannelPosition)] = &[
    ("front_left", ChannelPosition::FrontLeft),
    ("front_right", ChannelPosition::FrontRight),
    ("front_center", ChannelPosition::FrontCenter),
    ("low_frequency", ChannelPosition::LowFrequency),
    ("back_left", ChannelPosition::BackLeft),
    ("back_right", ChannelPosition::BackRight),
    ("front_left_of_center", ChannelPosition::FrontLeftOfCenter),
    ("front_right_of_center", ChannelPosition::FrontRightOfCenter),
    ("back_center", ChannelPosition::BackCenter),
    ("side_left", ChannelPosition::SideLeft),
    ("side_right", ChannelPosition::SideRight),
    ("top_center", ChannelPosition::TopCenter),
    ("top_front_left", ChannelPosition::TopFrontLeft),
    ("top_front_center", ChannelPosition::TopFrontCenter),
    ("top_front_right", ChannelPosition::TopFrontRight),
    ("top_back_left", ChannelPosition::TopBackLeft),
    ("top_back_center", ChannelPosition::TopBackCenter),
    ("top_back_right", ChannelPosition::TopBackRight),
    (
        "top_front_left_of_center",
        ChannelPosition::TopFrontLeftOfCenter,
    ),
    (
        "top_front_right_of_center",
        ChannelPosition::TopFrontRightOfCenter,
    ),
    ("left_low_frequency", ChannelPosition::LeftLowFrequency),
    ("right_low_frequency", ChannelPosition::RightLowFrequency),
    ("top_side_left", ChannelPosition::TopSideLeft),
    ("top_side_right", ChannelPosition::TopSideRight),
    ("bottom_center", ChannelPosition::BottomCenter),
    ("back_left_of_center", ChannelPosition::BackLeftOfCenter),
    ("back_right_of_center", ChannelPosition::BackRightOfCenter),
];

pub fn channel_map(map: &ChannelMap) -> Value {
    Value::Array(
        map.positions()
            .iter()
            .enumerate()
            .map(|(index, position)| {
                let label = POSITIONS
                    .iter()
                    .find(|(_, known)| known == position)
                    .map(|(label, _)| *label)
                    .unwrap_or("unspecified");
                match position {
                    ChannelPosition::Adc3Information { purpose, relationship, group } => json!({"index":index,"label":"adc3_information","topology":{"type":"adc3Information","purpose":purpose,"relationship":relationship,"group":group}}),
                    ChannelPosition::Adc3Ambisonic { ordering, acn, normalization, group } => json!({"index":index,"label":"adc3_ambisonic","topology":{"type":"adc3Ambisonic","ordering":ordering,"acn":acn,"normalization":normalization,"group":group}}),
                    _ => json!({ "index": index, "label": label }),
                }
            })
            .collect(),
    )
}

pub fn parse_request(text: &str) -> Result<CaptureRequest, BadRequest> {
    let value: Value =
        serde_json::from_str(text).map_err(|e| BadRequest(format!("capture request: {e}")))?;
    let map = value
        .as_object()
        .ok_or_else(|| BadRequest("capture request must be an object".into()))?;
    if map.get("encoding").is_some_and(|v| v != "pcm")
        || map.get("rate_policy").is_some_and(|v| v != "exact")
    {
        return Err(BadRequest("capture supports exact linear PCM only".into()));
    }
    let uint = |key: &str| {
        map.get(key)
            .and_then(Value::as_u64)
            .ok_or_else(|| BadRequest(format!("capture {key} must be a non-negative integer")))
    };
    let sample_rate = u32::try_from(uint("sample_rate")?)
        .map_err(|_| BadRequest("sample_rate is out of range".into()))?;
    let channels = u16::try_from(uint("channels")?)
        .map_err(|_| BadRequest("channels is out of range".into()))?;
    let valid_bits = u8::try_from(uint("valid_bits")?)
        .map_err(|_| BadRequest("valid_bits is out of range".into()))?;
    if sample_rate == 0 || channels == 0 || !(1..=32).contains(&valid_bits) {
        return Err(BadRequest(
            "capture needs a positive rate/channel count and 1..32 valid bits".into(),
        ));
    }
    let channel_map = match map.get("channel_map") {
        None | Some(Value::Null) => None,
        Some(value) => {
            let entries = value
                .as_array()
                .ok_or_else(|| BadRequest("channel_map must be an array".into()))?;
            if entries.len() != usize::from(channels) {
                return Err(BadRequest("channel_map must match channels".into()));
            }
            let mut positions = Vec::with_capacity(entries.len());
            for (index, entry) in entries.iter().enumerate() {
                if entry.get("index").and_then(Value::as_u64) != Some(index as u64) {
                    return Err(BadRequest(
                        "channel_map indices must preserve zero-based wire order".into(),
                    ));
                }
                let label = entry
                    .get("label")
                    .and_then(Value::as_str)
                    .ok_or_else(|| BadRequest("channel label is required".into()))?;
                if !matches!(label, "adc3_information" | "adc3_ambisonic")
                    && entry.get("topology").is_some_and(|v| !v.is_null())
                {
                    return Err(BadRequest("unexpected ADC3 topology for channel label".into()));
                }
                let topology = &entry["topology"];
                let field = |name: &str| topology[name].as_u64().and_then(|v|u8::try_from(v).ok()).ok_or_else(||BadRequest(format!("ADC3 channel {name} must be a byte")));
                let position = if label == "adc3_information" && topology["type"] == "adc3Information" {
                    ChannelPosition::Adc3Information {purpose:field("purpose")?,relationship:field("relationship")?,group:field("group")?}
                } else if label == "adc3_ambisonic" && topology["type"] == "adc3Ambisonic" {
                    ChannelPosition::Adc3Ambisonic {ordering:field("ordering")?,acn:field("acn")?,normalization:field("normalization")?,group:field("group")?}
                } else if label == "unspecified" {
                    ChannelPosition::Unspecified {
                        index: index as u16,
                    }
                } else {
                    POSITIONS
                        .iter()
                        .find(|(name, _)| *name == label)
                        .map(|(_, position)| *position)
                        .ok_or_else(|| {
                            BadRequest(format!("unknown capture channel label {label}"))
                        })?
                };
                positions.push(position);
            }
            Some(
                ChannelMap::from_positions(positions)
                    .map_err(|_| BadRequest("invalid capture channel map".into()))?,
            )
        }
    };
    Ok(CaptureRequest {
        sample_rate,
        channels,
        valid_bits,
        channel_map,
    })
}

pub fn format(stream: &DacCaptureStream) -> Value {
    let format = stream.format();
    json!({ "encoding": "pcm", "sample_rate": format.sample_rate(), "channels": format.channels(),
        "valid_bits": format.valid_bits(), "subslot_bytes": format.subslot_bytes(),
        "bytes_per_frame": format.bytes_per_frame(), "channel_map": channel_map(format.channel_map()),
        "configuration_id": stream.configuration_id(), "interface_number": stream.interface_number(),
        "alt_setting": stream.alternate_setting(), "endpoint_address": stream.endpoint_address() })
}

pub fn stats(stream: &DacCaptureStream) -> Value {
    let stats = stream.stats();
    let error = stream
        .last_error()
        .map(|error| json!({ "code": error.code(), "message": error.message() }));
    json!({ "state": match stream.state() { CaptureState::Active => "active", CaptureState::Stopped => "stopped", CaptureState::Failed => "failed" },
        "available_frames": stream.available_frames(), "captured_frames": stats.captured_frames,
        "delivered_frames": stats.delivered_frames, "overrun_frames": stats.overrun_frames,
        "contention_frames": stats.contention_frames,
        "empty_packets": stats.empty_packets, "short_packets": stats.short_packets,
        "failed_packets": stats.failed_packets, "malformed_packets": stats.malformed_packets,
        "malformed_completions": stats.malformed_completions, "failed_transfers": stats.failed_transfers,
        "missing_frames": stats.missing_frames, "reordered_completions": stats.reordered_completions,
        "device_position_frames": stats.device_position_frames(), "error": error })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn channel_identity_round_trips_with_unknown_indices() {
        let map = ChannelMap::from_positions(vec![
            ChannelPosition::FrontRight,
            ChannelPosition::Unspecified { index: 1 },
            ChannelPosition::Adc3Information {purpose:0x91,relationship:0x83,group:7},
            ChannelPosition::Adc3Ambisonic {ordering:2,acn:3,normalization:1,group:9},
        ])
        .unwrap();
        let request = json!({ "sample_rate": 48000, "channels": 4, "valid_bits": 24, "channel_map": channel_map(&map) });
        assert_eq!(
            parse_request(&request.to_string())
                .unwrap_or_else(|error| panic!("capture request rejected: {}", error.0))
                .channel_map,
            Some(map)
        );
    }
    #[test]
    fn capture_rejects_conversion_requests_and_reordered_indices() {
        for request in [
            r#"{"sample_rate":48000,"channels":1,"valid_bits":24,"encoding":"float"}"#,
            r#"{"sample_rate":48000,"channels":1,"valid_bits":24,"rate_policy":"nearest_supported"}"#,
            r#"{"sample_rate":48000,"channels":1,"valid_bits":24,"channel_map":[{"index":1,"label":"unspecified"}]}"#,
        ] {
            assert!(parse_request(request).is_err());
        }
    }
}
