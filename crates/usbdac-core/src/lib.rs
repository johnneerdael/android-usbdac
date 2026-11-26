//! Bit-perfect direct USB DAC output for Android and Linux.
//!
//! `usbdac-core` drives a USB Audio Class 1.0/2.0 DAC through usbfs, without the
//! platform audio stack: PCM at the DAC's native depth, DSD over PCM (DoP) and
//! native DSD, isochronous transfers paced by the DAC's explicit feedback
//! endpoint, loss accounting, and the DAC's own hardware volume.
//!
//! The library never resamples, dithers or scales samples unless the host
//! explicitly enables software gain, and it reports honestly whether the audio
//! on the wire is bit-perfect ([`DacStream::is_bit_perfect`]).
//!
//! ```no_run
//! use std::sync::Arc;
//! use usbdac_core::{Dac, DacConfig, DacEvent, DeviceInfo, StreamRequest};
//!
//! # fn demo(fd: i32) -> usbdac_core::Result<()> {
//! // `fd` comes from UsbDeviceConnection.getFileDescriptor(); the host has
//! // claimed the audio interfaces and lists them here.
//! let info = DeviceInfo::new(0x2fc6, 0xf09a, "Some DAC").with_claimed_interfaces(vec![0u8, 1]);
//! let dac = Dac::open(fd, info, DacConfig::default())?;
//! let format = dac.negotiate(&StreamRequest::pcm(44_100, 2, 16))?;
//! let stream = dac.open_stream(&format, Arc::new(|event: DacEvent| log::info!("{event:?}")))?;
//! let silence = vec![0u8; stream.writable_bytes()];
//! stream.write(&silence)?;
//! # Ok(()) }
//! ```
//!
//! v0.1 supports one open [`Dac`] per process.

#[macro_use]
mod macros;

pub mod descriptors;

mod capabilities;
mod clock_resolution;
mod config;
mod dac;
mod endpoint;
mod error;
mod events;
mod format;
mod iso_packet_scheduler;
mod iso_packet_validation;
mod iso_transport_loss;
mod output_position;
mod payload_audit_latch;
mod priority;
mod quirk;
mod settings;
mod stream;
mod transport;
mod volume;
mod volume_control;

#[cfg(test)]
mod tests;

pub use capabilities::{
    AltFormat, AltSetting, Capabilities, DeviceInfo, LinkSpeed, RateRange, SyncKind, UacVersion,
};
pub use config::{DacConfig, HandBack, QuirkOverride};
pub use dac::Dac;
pub use error::{Error, Result};
pub use events::{DacEvent, DacListener, StreamState};
pub use format::{Encoding, RatePolicy, StreamFormat, StreamRequest};
pub use quirk::Quirk;
pub use stream::{DacStream, StreamStats};
pub use volume::{VolumeControl, VolumeRange, VolumeState, DEFAULT_VOLUME_STEPS};
