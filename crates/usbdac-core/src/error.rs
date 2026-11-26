//! The error type of the public API.

use crate::capabilities::AltSetting;
use std::fmt;

/// Everything the public API can fail with.
///
/// [`Error::code`] returns a stable machine-readable identifier for each
/// variant (the JNI layer puts it in the exception message).
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum Error {
    /// Another [`Dac`](crate::Dac) is already open in this process (v0.1
    /// supports one DAC at a time), or a previous one is still shutting down.
    Busy,
    /// Opening a DAC needs the Linux usbfs interface (Android or Linux).
    PlatformUnsupported,
    /// The file descriptor could not be used, or the interfaces the host
    /// claimed do not match what the library needs to drive the device.
    Claim(String),
    /// The DAC offers no alternate setting that can carry the request.
    /// `offered` lists what it does offer.
    Unsupported {
        reason: String,
        offered: Vec<AltSetting>,
    },
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
}

impl Error {
    /// Stable identifier: `busy`, `platform_unsupported`, `claim`,
    /// `unsupported`, `clock_unverified`, `device_lost`, `transfer`,
    /// `control`, `invalid_state`, `invalid_argument`.
    pub fn code(&self) -> &'static str {
        match self {
            Self::Busy => "busy",
            Self::PlatformUnsupported => "platform_unsupported",
            Self::Claim(_) => "claim",
            Self::Unsupported { .. } => "unsupported",
            Self::ClockUnverified { .. } => "clock_unverified",
            Self::DeviceLost => "device_lost",
            Self::Transfer(_) => "transfer",
            Self::Control(_) => "control",
            Self::InvalidState(_) => "invalid_state",
            Self::InvalidArgument(_) => "invalid_argument",
        }
    }

    /// Human-readable description without the code prefix.
    pub fn message(&self) -> String {
        match self {
            Self::Busy => "another DAC session is open in this process".to_string(),
            Self::PlatformUnsupported => {
                "direct USB output needs Linux usbfs (Android or Linux)".to_string()
            }
            Self::Claim(m)
            | Self::Transfer(m)
            | Self::Control(m)
            | Self::InvalidState(m)
            | Self::InvalidArgument(m) => m.clone(),
            Self::Unsupported { reason, offered } => {
                format!("{reason} ({} alternate setting(s) offered)", offered.len())
            }
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
