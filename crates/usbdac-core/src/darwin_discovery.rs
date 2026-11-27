//! Read-only libusb discovery for a fresh macOS USB attachment.
//!
//! An attachment retains its libusb device reference so callers do not reopen a
//! potentially different device by bus address after unplug/replug. Discovery
//! does not claim interfaces, capture a kernel driver, or start audio.
use rusb::{Context, Device, Direction, TransferType, UsbContext};
use serde::Serialize;

#[derive(Clone, Debug, Eq, PartialEq, Hash, Serialize)]
pub struct AttachmentKey {
    pub bus: u8,
    pub address: u8,
    pub ports: Vec<u8>,
}

#[derive(Clone, Debug, Serialize)]
pub struct AttachmentEvidence {
    pub key: AttachmentKey,
    pub vendor_id: u16,
    pub product_id: u16,
    pub device_class: u8,
    pub link_speed: String,
    pub active_configuration: Option<u8>,
    pub audio_playback_interfaces: Vec<u8>,
    pub descriptor_error: Option<String>,
}

/// A libusb device reference bound to the attachment observed during discovery.
#[derive(Clone)]
pub struct Attachment {
    pub evidence: AttachmentEvidence,
    device: Device<Context>,
}

impl Attachment {
    /// Retain this exact attachment when opening; do not resolve by address again.
    pub fn device(&self) -> &Device<Context> {
        &self.device
    }
}

/// Enumerate USB devices without opening, claiming, or capturing them.
pub fn snapshot(context: &Context) -> rusb::Result<Vec<Attachment>> {
    let mut result = Vec::new();
    for device in context.devices()?.iter() {
        let descriptor = match device.device_descriptor() {
            Ok(value) => value,
            Err(_) => continue,
        };
        let key = AttachmentKey {
            bus: device.bus_number(),
            address: device.address(),
            ports: device.port_numbers().unwrap_or_default(),
        };
        let mut evidence = AttachmentEvidence {
            key,
            vendor_id: descriptor.vendor_id(),
            product_id: descriptor.product_id(),
            device_class: descriptor.class_code(),
            link_speed: format!("{:?}", device.speed()),
            active_configuration: None,
            audio_playback_interfaces: Vec::new(),
            descriptor_error: None,
        };
        match device.active_config_descriptor() {
            Ok(config) => {
                evidence.active_configuration = Some(config.number());
                for interface in config.interfaces() {
                    if interface.descriptors().any(|alt| {
                        alt.class_code() == 1
                            && alt.sub_class_code() == 2
                            && alt.endpoint_descriptors().any(|ep| {
                                ep.direction() == Direction::Out
                                    && ep.transfer_type() == TransferType::Isochronous
                            })
                    }) {
                        evidence.audio_playback_interfaces.push(interface.number());
                    }
                }
            }
            Err(error) => evidence.descriptor_error = Some(error.to_string()),
        }
        result.push(Attachment { evidence, device });
    }
    Ok(result)
}

/// A bounded pre-claim descriptor record for the selected attachment only.
#[derive(Debug, Serialize)]
pub struct DescriptorRecord {
    pub kind: &'static str,
    pub value: serde_json::Value,
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut result = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(result, "{byte:02x}");
    }
    result
}

fn usb_error(error: rusb::Error) -> serde_json::Value {
    let kind = match error {
        rusb::Error::Access => "access_denied",
        rusb::Error::Busy => "in_use",
        rusb::Error::NoDevice => "device_lost",
        rusb::Error::NotSupported => "not_supported",
        rusb::Error::Timeout => "timeout",
        _ => "other",
    };
    serde_json::json!({"kind":kind, "detail":error.to_string()})
}

/// Collect cached USB descriptors and attempt one bounded, read-only raw
/// GET_DESCRIPTOR for the active configuration. No interface is claimed and no
/// kernel driver is detached. Only this retained attachment is opened. The
/// control request has a 500 ms timeout; libusb's device-open call itself has no
/// application-settable timeout. Limits and truncation are included in records.
pub fn descriptor_evidence(attachment: &Attachment) -> Vec<DescriptorRecord> {
    use serde_json::json;
    use std::time::Duration;
    const MAX_CONFIGS: usize = 8;
    const MAX_ALTS: usize = 128;
    const MAX_ENDPOINTS: usize = 32;
    const MAX_EXTRA: usize = 1024;
    const MAX_ENDPOINT_EXTRA: usize = 256;
    const MAX_RAW: usize = 4096;
    let usb = attachment.device();
    let mut records = Vec::new();
    let descriptor = match usb.device_descriptor() {
        Ok(d) => d,
        Err(e) => {
            return vec![DescriptorRecord {
                kind: "descriptor_device_error",
                value: usb_error(e),
            }]
        }
    };
    let firmware = descriptor.device_version();
    let usb_version = descriptor.usb_version();
    records.push(DescriptorRecord { kind: "descriptor_device", value: json!({
        "vendor_id":descriptor.vendor_id(), "product_id":descriptor.product_id(),
        "firmware_bcd":format!("{:02}.{}.{}", firmware.major(), firmware.minor(), firmware.sub_minor()),
        "usb_version":format!("{:02}.{}.{}", usb_version.major(), usb_version.minor(), usb_version.sub_minor()),
        "class":descriptor.class_code(), "subclass":descriptor.sub_class_code(), "protocol":descriptor.protocol_code(),
        "max_packet_size":descriptor.max_packet_size(), "num_configurations":descriptor.num_configurations(),
        "config_limit":MAX_CONFIGS, "alternate_limit":MAX_ALTS, "extra_bytes_per_descriptor_limit":MAX_EXTRA
    }) });
    let active = attachment.evidence.active_configuration;
    let mut active_raw = None;
    let mut alt_count = 0usize;
    for index in 0..usize::from(descriptor.num_configurations()).min(MAX_CONFIGS) {
        let config = match usb.config_descriptor(index as u8) {
            Ok(config) => config,
            Err(error) => {
                records.push(DescriptorRecord {
                    kind: "descriptor_config_error",
                    value: json!({"index":index, "error":usb_error(error)}),
                });
                continue;
            }
        };
        if Some(config.number()) == active {
            active_raw = Some((index as u8, config.total_length()));
        }
        let extra = config.extra();
        records.push(DescriptorRecord { kind: "descriptor_config", value: json!({
            "index":index, "number":config.number(), "active":Some(config.number()) == active,
            "total_length":config.total_length(), "interfaces":config.num_interfaces(),
            "max_power_ma":config.max_power(), "self_powered":config.self_powered(), "remote_wakeup":config.remote_wakeup(),
            "extra_hex":hex(&extra[..extra.len().min(MAX_EXTRA)]), "extra_truncated":extra.len() > MAX_EXTRA
        }) });
        for interface in config.interfaces() {
            for alt in interface.descriptors() {
                if alt_count >= MAX_ALTS {
                    continue;
                }
                alt_count += 1;
                let extra = alt.extra();
                let endpoints: Vec<_> = alt.endpoint_descriptors().take(MAX_ENDPOINTS).map(|ep| {
                    let extra = ep.extra().unwrap_or(&[]);
                    json!({"address":ep.address(), "direction":format!("{:?}",ep.direction()),
                        "transfer":format!("{:?}",ep.transfer_type()), "sync":format!("{:?}",ep.sync_type()),
                        "usage":format!("{:?}",ep.usage_type()), "max_packet_size":ep.max_packet_size(),
                        "interval":ep.interval(), "refresh":ep.refresh(), "sync_address":ep.synch_address(),
                        "extra_hex":hex(&extra[..extra.len().min(MAX_ENDPOINT_EXTRA)]), "extra_truncated":extra.len() > MAX_ENDPOINT_EXTRA})
                }).collect();
                records.push(DescriptorRecord { kind: "descriptor_alternate", value: json!({
                    "configuration_index":index, "interface":alt.interface_number(), "alternate":alt.setting_number(),
                    "class":alt.class_code(), "subclass":alt.sub_class_code(), "protocol":alt.protocol_code(),
                    "extra_hex":hex(&extra[..extra.len().min(MAX_EXTRA)]), "extra_truncated":extra.len() > MAX_EXTRA,
                    "endpoint_count":alt.num_endpoints(), "endpoints_truncated":usize::from(alt.num_endpoints()) > MAX_ENDPOINTS, "endpoints":endpoints
                }) });
            }
        }
    }
    records.push(DescriptorRecord { kind: "descriptor_limits", value: json!({"configurations_truncated":usize::from(descriptor.num_configurations()) > MAX_CONFIGS, "alternate_limit":MAX_ALTS, "alternates_may_be_truncated":alt_count == MAX_ALTS}) });
    let value = match active_raw {
        None => {
            json!({"status":"unavailable", "reason":"active configuration index unavailable from cached descriptors"})
        }
        Some((index, total)) => match usb.open() {
            Err(error) => {
                json!({"status":"error", "operation":"open_for_raw_descriptor", "error":usb_error(error)})
            }
            Ok(handle) => {
                let mut bytes = vec![0; usize::from(total).min(MAX_RAW)];
                match handle.read_control(
                    0x80,
                    0x06,
                    0x0200 | u16::from(index),
                    0,
                    &mut bytes,
                    Duration::from_millis(500),
                ) {
                    Ok(length) => {
                        bytes.truncate(length);
                        json!({"status":"ok", "configuration_index":index, "advertised_length":total, "returned_length":length, "truncated":length < usize::from(total), "bytes_hex":hex(&bytes)})
                    }
                    Err(error) => {
                        json!({"status":"error", "operation":"get_configuration_descriptor", "configuration_index":index, "error":usb_error(error)})
                    }
                }
            }
        },
    };
    records.push(DescriptorRecord {
        kind: "descriptor_raw_active_configuration",
        value,
    });
    records
}

/// Summarize a newly attached, unselected device from the cached active
/// configuration. This performs no open, control transfer, claim or detach.
pub fn unselected_candidate_summary(attachment: &Attachment) -> serde_json::Value {
    use serde_json::json;
    const MAX_ALTERNATES: usize = 32;
    const MAX_ENDPOINTS: usize = 8;
    match attachment.device().active_config_descriptor() {
        Err(error) => {
            json!({"evidence":attachment.evidence, "configuration_error":usb_error(error)})
        }
        Ok(config) => {
            let mut alternatives = Vec::new();
            let mut truncated = false;
            for interface in config.interfaces() {
                for alt in interface.descriptors() {
                    if alternatives.len() == MAX_ALTERNATES {
                        truncated = true;
                        continue;
                    }
                    let endpoints: Vec<_> = alt.endpoint_descriptors().take(MAX_ENDPOINTS).map(|ep| json!({
                        "address":ep.address(), "direction":format!("{:?}", ep.direction()),
                        "transfer":format!("{:?}", ep.transfer_type()), "max_packet_size":ep.max_packet_size()
                    })).collect();
                    alternatives.push(json!({"interface":alt.interface_number(), "alternate":alt.setting_number(),
                        "class":alt.class_code(), "subclass":alt.sub_class_code(), "protocol":alt.protocol_code(),
                        "endpoint_count":alt.num_endpoints(), "endpoints_truncated":usize::from(alt.num_endpoints()) > MAX_ENDPOINTS,
                        "endpoints":endpoints}));
                }
            }
            json!({"evidence":attachment.evidence, "active_configuration":config.number(),
                "alternates":alternatives, "alternates_truncated":truncated,
                "limits":{"alternates":MAX_ALTERNATES, "endpoints_per_alternate":MAX_ENDPOINTS}})
        }
    }
}
