//! Machine-readable shipped boundaries for documentation drift checks.
use usbdac_core::{UacVersion, SHIPPED_SUPPORT as support};
fn main() {
    let versions: Vec<_> = support
        .uac_versions
        .iter()
        .map(|version| match version {
            UacVersion::V1 => 1,
            UacVersion::V2 => 2,
            UacVersion::V3 => 3,
            UacVersion::Unknown => 0,
        })
        .collect();
    let value = serde_json::json!({
        "schema_version": 1,
        "uac_versions": versions,
        "playback": support.playback,
        "pcm_capture": support.pcm_capture,
        "duplex": support.duplex,
        "max_open_devices": support.max_open_devices,
        "vendor_protocols": support.vendor_protocols,
        "type_i_formats": support.type_i_formats,
    });
    println!("{}", serde_json::to_string_pretty(&value).unwrap());
}
