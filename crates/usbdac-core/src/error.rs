//! The error type of the public API.

use crate::capabilities::AltSetting;
use std::fmt;

/// Host-side USB admission failure; it does not by itself identify a DAC fault.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum HostUsbFailureKind {
    AccessDenied,
    InUse,
    DeviceLost,
    NotSupported,
    Other,
}

/// Everything the public API can fail with.
///
/// [`Error::code`] returns a stable machine-readable identifier for each
/// variant (the JNI layer puts it in the exception message).
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum Error {
    /// The requested device or operation has a retained owner, or a previous owner is still shutting down.
    Busy,
    /// The platform or requested USB backend is unavailable.
    PlatformUnsupported,
    /// The file descriptor could not be used, or the interfaces the host
    /// claimed do not match what the library needs to drive the device.
    Claim(String),
    /// A macOS libusb open, whole-device capture, or interface claim failed.
    HostUsb {
        operation: String,
        kind: HostUsbFailureKind,
        detail: String,
    },
    /// The DAC offers no alternate setting that can carry the request.
    /// `offered` lists what it does offer.
    Unsupported {
        reason: String,
        offered: Vec<AltSetting>,
    },
    /// The only way to stream the request is an asynchronous endpoint that has
    /// no explicit feedback endpoint, which would need implicit feedback (an
    /// associated IN data endpoint). That is not supported: without feedback the
    /// DAC's clock and the host's packet timing drift apart. Set
    /// [`DacConfig::allow_unsynchronized_async`](crate::DacConfig::allow_unsynchronized_async)
    /// to stream it at the nominal rate anyway (never bit-perfect).
    ImplicitFeedbackUnsupported,
    /// The DAC would not confirm the sampling frequency, and the
    /// configuration requires a verified rate
    /// ([`DacConfig::require_verified_rate`](crate::DacConfig::require_verified_rate)).
    ClockUnverified {
        requested_hz: u32,
        reported_hz: Option<u32>,
    },
    /// The device was unplugged or stopped answering on the bus.
    DeviceLost,
    /// An isochronous transfer, or the stream machinery around it, failed.
    Transfer(String),
    /// A class control request (clock, volume, mute) failed.
    Control(String),
    /// The call is not valid in the current state (closed handle, writing
    /// after `drain()`, a second stream while one is open, ...).
    InvalidState(String),
    /// An argument was out of range or malformed.
    InvalidArgument(String),
    /// The DAC has no usable hardware volume control (none, or its start-up
    /// attenuation could not be verified) and
    /// [`DacConfig::require_hardware_volume`](crate::DacConfig::require_hardware_volume)
    /// is on: a stream would play at the DAC's own level with no way for the
    /// host to bring it down. Ask the user to allow fixed-level output and open
    /// the DAC again with `require_hardware_volume` off. `negotiate` and
    /// `capabilities` still work, so the host can show what the DAC offers.
    VolumeUnavailable,
    /// Reconfiguration did not drain before its deadline. The original stream remains open/draining.
    ReconfigureDrainTimeout,
    /// Stream retirement, retained ownership, or rollback failed. Close the DAC before further use.
    ReconfigureUnusable(String),
}

impl Error {
    /// Stable identifier: `busy`, `platform_unsupported`, `claim`,
    /// `unsupported`, `implicit_feedback_unsupported`, `clock_unverified`, `device_lost`, `transfer`,
    /// `control`, `invalid_state`, `invalid_argument`, `volume_unavailable`, `host_usb`,
    /// `reconfigure_drain_timeout`, `reconfigure_unusable`.
    pub fn code(&self) -> &'static str {
        match self {
            Self::Busy => "busy",
            Self::PlatformUnsupported => "platform_unsupported",
            Self::Claim(_) => "claim",
            Self::HostUsb { .. } => "host_usb",
            Self::Unsupported { .. } => "unsupported",
            Self::ImplicitFeedbackUnsupported => "implicit_feedback_unsupported",
            Self::ClockUnverified { .. } => "clock_unverified",
            Self::DeviceLost => "device_lost",
            Self::Transfer(_) => "transfer",
            Self::Control(_) => "control",
            Self::InvalidState(_) => "invalid_state",
            Self::InvalidArgument(_) => "invalid_argument",
            Self::VolumeUnavailable => "volume_unavailable",
            Self::ReconfigureDrainTimeout => "reconfigure_drain_timeout",
            Self::ReconfigureUnusable(_) => "reconfigure_unusable",
        }
    }

    /// Human-readable description without the code prefix.
    pub fn message(&self) -> String {
        match self {
            Self::Busy => "the device or operation is busy, or a prior owner is shutting down".to_string(),
            Self::PlatformUnsupported => {
                "the requested USB backend is unavailable on this platform".to_string()
            }
            Self::HostUsb { operation, kind, detail } => format!("{operation}: {kind:?}: {detail}"),
            Self::Claim(m)
            | Self::Transfer(m)
            | Self::Control(m)
            | Self::InvalidState(m)
            | Self::InvalidArgument(m)
            | Self::ReconfigureUnusable(m) => m.clone(),
            Self::Unsupported { reason, offered } => {
                format!("{reason} ({} alternate setting(s) offered)", offered.len())
            }
            Self::ImplicitFeedbackUnsupported => "the DAC's asynchronous endpoint has no explicit feedback endpoint and no compatible input stream to take implicit feedback from (DacConfig::allow_unsynchronized_async streams it at the nominal rate, never bit-perfect)".to_string(),
            Self::ClockUnverified {
                requested_hz,
                reported_hz,
            } => match reported_hz {
                Some(reported) => format!(
                    "DAC clock not verified at {requested_hz} Hz (the DAC reports {reported} Hz)"
                ),
                None => format!("DAC clock not verified at {requested_hz} Hz (no readback)"),
            },
            Self::DeviceLost => "the USB device was disconnected".to_string(),
            Self::ReconfigureDrainTimeout => "drain deadline expired; original stream remains open/draining and no alternate or clock was changed".into(),
            Self::VolumeUnavailable => "the DAC has no usable hardware volume control, so a stream would play at the DAC's own level; allow fixed-level output (DacConfig::require_hardware_volume = false) to stream anyway".to_string(),
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code(), self.message())
    }
}

impl std::error::Error for Error {}

/// `Result` with the crate [`Error`].
pub type Result<T> = std::result::Result<T, Error>;

/// Maps a transport-internal `String` failure to [`Error::Control`], or to
/// [`Error::DeviceLost`] when the message says the device went away.
pub(crate) trait OrControl<T> {
    fn or_control(self) -> Result<T>;
}

impl<T> OrControl<T> for std::result::Result<T, String> {
    fn or_control(self) -> Result<T> {
        self.map_err(|message| {
            if is_device_gone(&message) {
                Error::DeviceLost
            } else {
                Error::Control(message)
            }
        })
    }
}

/// Whether a transport error message reports a vanished device (ENODEV or
/// libusb's NO_DEVICE).
pub(crate) fn is_device_gone(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    lower.contains("no such device")
        || lower.contains("os error 19")
        || lower.contains("disconnected")
        || lower.contains("no device")
}
