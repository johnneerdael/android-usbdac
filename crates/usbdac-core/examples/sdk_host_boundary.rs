//! Compiled Rust SDK host-boundary examples.
#![allow(dead_code, unused_variables)]

fn main() { discovery(); fake(); }

fn discovery() {
use usbdac_core::{DeviceInfo, HotplugHub, UsbPermission};
let hub = HotplugHub::new();
let events = hub.subscribe();
let attachment = hub.attached("android-device-name", DeviceInfo::new(1, 2, "DAC"),
    UsbPermission::Required);
// Pass attachment.id through the host permission request/callback.
assert!(hub.permission_changed(attachment.id, UsbPermission::Granted));
hub.detached(attachment.id);
assert!(!hub.permission_changed(attachment.id, UsbPermission::Granted));
drop(events);
}

use usbdac_core::{VolumeControl, Result};
async fn change(volume: VolumeControl) -> Result<()> {
let controls = volume.nonblocking()?;
let applied = controls.set_db(-45.0)?.await?;
assert!(applied.db <= 0.0);
Ok(())
}

fn fake() {
use std::sync::Arc;
use usbdac_core::{fake::FakeDac, DeviceInfo, PlaybackDevice, PlaybackStream, StreamRequest};
let request = StreamRequest::pcm(48_000, 2, 16);
let dac = FakeDac::new(DeviceInfo::new(1, 2, "test"), vec![request], 128).unwrap();
let format = dac.negotiate(&request).unwrap();
let stream = dac.open_stream(&format, Arc::new(|_| {})).unwrap();
assert_eq!(stream.write(&[0; 16]).unwrap(), 16);
assert_eq!(stream.advance(4).unwrap(), 4);
assert_eq!(stream.played_frames(), 4);
dac.detach();
assert!(stream.write(&[0; 4]).is_err());
}
