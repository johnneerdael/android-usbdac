//! Bit-perfect direct USB DAC output for Android and Linux.
//!
//! `usbdac-core` drives a USB Audio Class 1.0, 2.0 or 3.0 DAC through usbfs, without the
//! platform audio stack: PCM at the DAC's native depth, DSD over PCM (DoP) and
//! native DSD, feedback-paced isochronous transfers, loss accounting, and the
//! DAC's own hardware volume. Asynchronous streams can follow explicit rate
//! reports or frame counts from a compatible UAC2 or UAC3 input stream. Use
//! [`StreamFormat::pacing`] for the selected mechanism and
//! [`DacStream::stats`] for live lock and feedback counters.
//!
//! The library never resamples, dithers or scales samples unless the host
//! explicitly enables software gain, and it reports honestly whether the audio
//! on the wire is bit-perfect ([`DacStream::is_bit_perfect`]). (The opt-in
//! [`convert`] module, which the stream never calls, is for a host that wants
//! to convert before writing.)
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
//! Distinct USB attachments have independent [`Dac`] owners and retirement tokens.

#[macro_use]
mod macros;

pub mod capture;
pub mod convert;
pub mod descriptors;
pub mod pioneer;
pub mod routing;
pub mod scarlett;

mod async_volume;
mod capabilities;
mod clock_resolution;
mod config;
mod dac;
mod endpoint;
mod error;
mod events;
pub mod fake;
mod feedback_diagnostics;
mod flush_cancel;
mod format;
#[cfg(any(test, feature = "fuzzing"))]
#[doc(hidden)]
pub mod fuzzing;
pub mod hotplug;
/// Read-only, same-fd BADD fixed-rate proof used by Android preclaim fallback.
#[doc(hidden)]
pub use transport::badd_descriptor_witness;
/// Shared rate ordering used by the automatic selector and USB negotiation.
#[doc(hidden)]
pub use transport::negotiate::rank_sample_rates;

mod iso_packet_scheduler;
mod iso_packet_validation;
mod iso_transport_loss;
mod output_position;
mod payload_audit_latch;
mod priority;
mod quirk;
mod ring_target;
mod settings;
mod stream;
mod transport;
mod volume;
mod volume_control;

#[cfg(test)]
mod tests;

pub use async_volume::{AsyncVolumeControl, VolumeOperation};
pub use capabilities::{
    AltFormat, AltSetting, Capabilities, DeviceInfo, EndpointUsage, LinkSpeed, Pacing, RateRange,
    ShippedSupport, SyncKind, UacVersion, SHIPPED_SUPPORT,
};
pub use capture::{
    CaptureFormat, CaptureRequest, CaptureState, CaptureStats, ChannelMap, ChannelPosition,
    DacCaptureStream,
};
pub use clock_resolution::{ClockChoice, ClockHop, ClockHopKind, ClockResolution};
pub use config::{
    AsyncFallbackPolicy, ClockReadbackPolicy, CloseVolume, DacConfig, FeedbackSmoothing, HandBack,
    QuirkOverride, StartupVolume, UsbQueueProfile,
};
pub use dac::{Dac, StreamReconfiguration, UsbRetirement};
pub use error::{Error, Result};
pub use events::{CompatibilityFallbackTier, DacEvent, DacListener, StreamState};
pub use fake::{PlaybackDevice, PlaybackStream};
pub use feedback_diagnostics::{
    ExplicitFeedbackDiagnostics, ExplicitFeedbackOutcome, ExplicitFeedbackPacket, FeedbackTrace,
    FeedbackTraceEntry, FeedbackTraceEvent,
};
pub use format::{Encoding, RatePolicy, StreamFormat, StreamRequest};
pub use hotplug::{
    HotplugEvent, HotplugHub, HotplugSubscription, LinuxHotplugMonitor, UsbAttachment,
    UsbPermission,
};
pub use quirk::{AlsaQuirkFlag, Quirk, QuirkPolicy, SourceQuirkEntry, SOURCE_QUIRKS};
pub use stream::{BitPerfectBlocker, DacStream, StreamStats};
pub use volume::{
    StartupVolumeReport, VolumeControl, VolumeRange, VolumeState, DEFAULT_VOLUME_STEPS,
};

/// Compiles the README's example as a doctest so it cannot rot.
#[cfg(doctest)]
#[doc = include_str!("../../../README.md")]
struct ReadmeDoctests;

/// Explicit lossy native DSD to PCM conversion.
pub mod dsd_pcm;
