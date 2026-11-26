//! The DAC handle.

use crate::capabilities::{Capabilities, DeviceInfo};
use crate::config::{DacConfig, HandBack};
use crate::error::{Error, Result};
use crate::events::{DacListener, EventHub, EventSink};
use crate::format::{StreamFormat, StreamRequest};
use crate::settings::{self, Settings};
use crate::stream::{DacStream, StreamInner};
use crate::transport;
use crate::volume::VolumeControl;
use parking_lot::Mutex;
use std::os::fd::RawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};
use std::time::Duration;

/// How long `close` waits for the output thread to finish.
const CLOSE_TIMEOUT: Duration = Duration::from_secs(3);

/// Serializes `Dac::open`, so two concurrent opens cannot disturb each other's
/// settings.
static OPEN_GUARD: Mutex<()> = Mutex::new(());

/// State shared by the `Dac`, its streams and its volume handle.
pub(crate) struct DacShared {
    pub(crate) events: EventSink,
    pub(crate) info: DeviceInfo,
    pub(crate) config: DacConfig,
    closed: AtomicBool,
    pub(crate) stream: Mutex<Option<Weak<StreamInner>>>,
}

impl DacShared {
    pub(crate) fn ensure_open(&self) -> Result<()> {
        if self.closed.load(Ordering::Acquire) {
            Err(Error::InvalidState("the DAC is closed".to_string()))
        } else {
            Ok(())
        }
    }

    /// The stream that is open now, if any.
    fn active_stream(&self) -> Option<Arc<StreamInner>> {
        self.stream
            .lock()
            .as_ref()
            .and_then(Weak::upgrade)
            .filter(|stream| !stream.is_closed())
    }

    fn shutdown(&self) -> Result<()> {
        if self.closed.swap(true, Ordering::AcqRel) {
            return Ok(());
        }
        if let Some(stream) = self.active_stream() {
            let _ = stream.close();
        }
        let action = if self.events.device_lost() {
            transport::api::ORIGINAL_VOLUME_ACTION_FORGET
        } else {
            match self.config.hand_back {
                HandBack::RestoreOriginal => transport::api::ORIGINAL_VOLUME_ACTION_RESTORE,
                HandBack::Keep => transport::api::ORIGINAL_VOLUME_ACTION_KEEP,
            }
        };
        let stopped = transport::api::close_device(action, CLOSE_TIMEOUT);
        self.events.shutdown();
        if stopped {
            Ok(())
        } else {
            Err(Error::Transfer(
                "the USB output thread did not stop in time; the DAC stays busy until it does"
                    .to_string(),
            ))
        }
    }
}

/// An open USB DAC.
///
/// v0.1 supports one open `Dac` per process; a second [`Dac::open`] returns
/// [`Error::Busy`] until the first is closed or dropped. The host keeps its
/// `UsbDeviceConnection` open until then.
pub struct Dac {
    shared: Arc<DacShared>,
}

impl std::fmt::Debug for Dac {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Dac").field("info", &self.shared.info).finish_non_exhaustive()
    }
}

impl Dac {
    /// Opens the DAC behind `fd`.
    ///
    /// `fd` is the usbfs file descriptor of the device (on Android
    /// `UsbDeviceConnection.getFileDescriptor()`); the library duplicates it, so
    /// the host keeps ownership of its own. `info.claimed_interfaces` must be
    /// exactly the AudioControl and playback AudioStreaming interfaces the host
    /// has claimed.
    ///
    /// Opening scans the descriptors, finds the hardware volume control and
    /// records the DAC's current level. It does not touch the clock or start
    /// any audio.
    pub fn open(fd: RawFd, info: DeviceInfo, config: DacConfig) -> Result<Dac> {
        config.validate()?;
        let _guard = OPEN_GUARD.lock();
        if transport::session::session_registered() {
            return Err(Error::Busy);
        }
        settings::install(Settings::for_device(config.clone(), &info));
        if let Err(error) = transport::api::open_device(fd, &info) {
            settings::reset();
            return Err(error);
        }
        Ok(Dac {
            shared: Arc::new(DacShared {
                events: EventHub::start(),
                info,
                config,
                closed: AtomicBool::new(false),
                stream: Mutex::new(None),
            }),
        })
    }

    /// The identity the host gave at open.
    pub fn info(&self) -> &DeviceInfo {
        &self.shared.info
    }

    /// What the DAC offers, from the descriptors scanned at open.
    pub fn capabilities(&self) -> Result<Capabilities> {
        self.shared.ensure_open()?;
        transport::api::capabilities()
    }

    /// Settles the wire format for `request`.
    ///
    /// This programs the DAC's sampling frequency and reads it back (it is the
    /// step that can fail with [`Error::ClockUnverified`] later), so it takes
    /// 100 ms or more. It fails with [`Error::InvalidState`] while a stream is
    /// open. Nothing is resampled: with [`RatePolicy::Exact`] a rate the DAC
    /// cannot run is [`Error::Unsupported`]; with `NearestSupported` the
    /// returned format carries the rate the DAC will run.
    ///
    /// [`RatePolicy::Exact`]: crate::RatePolicy::Exact
    pub fn negotiate(&self, request: &StreamRequest) -> Result<StreamFormat> {
        self.shared.ensure_open()?;
        transport::api::negotiate(request)
    }

    /// Opens the output stream for a negotiated format and starts the output
    /// thread. The stream plays as soon as the ring holds the start threshold
    /// ([`DacConfig::start_threshold_ms`]) of audio. Only one stream can be open
    /// at a time.
    ///
    /// `listener` receives the stream's events; see [`DacListener`].
    pub fn open_stream(
        &self,
        format: &StreamFormat,
        listener: Arc<dyn DacListener>,
    ) -> Result<DacStream> {
        self.shared.ensure_open()?;
        let mut slot = self.shared.stream.lock();
        if slot
            .as_ref()
            .and_then(Weak::upgrade)
            .is_some_and(|stream| !stream.is_closed())
        {
            return Err(Error::InvalidState("a stream is already open".to_string()));
        }
        self.shared.events.set_stream_listener(Some(listener));
        let handle = match transport::api::open_stream(format, self.shared.events.clone()) {
            Ok(handle) => handle,
            Err(error) => {
                self.shared.events.set_stream_listener(None);
                return Err(error);
            }
        };
        let inner = StreamInner::new(Arc::clone(&self.shared), handle, format.clone());
        *slot = Some(Arc::downgrade(&inner));
        drop(slot);
        inner.start_events();
        Ok(DacStream::from_inner(inner))
    }

    /// The hardware volume control, if the DAC has a writable one.
    pub fn volume(&self) -> Option<VolumeControl> {
        if self.shared.ensure_open().is_err() || !transport::volume_hw::has_hardware_volume() {
            return None;
        }
        Some(VolumeControl::new(Arc::clone(&self.shared)))
    }

    /// Sets the listener for events about the device itself (volume changes,
    /// device loss) while no stream is open. While a stream is open its
    /// listener gets those events too.
    pub fn set_listener(&self, listener: Option<Arc<dyn DacListener>>) {
        self.shared.events.set_dac_listener(listener);
    }

    /// Closes the DAC: stops the stream if one is open, hands the volume back
    /// as [`DacConfig::hand_back`] says, and releases the session so another
    /// [`Dac::open`] can succeed. The host may then release its interfaces and
    /// close its connection. Dropping the `Dac` does the same.
    pub fn close(self) -> Result<()> {
        self.shared.shutdown()
    }
}

impl Drop for Dac {
    fn drop(&mut self) {
        if let Err(error) = self.shared.shutdown() {
            log::warn!("closing the DAC: {error}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capabilities::DeviceInfo;

    use crate::transport::session::PAYLOAD_LATCH_TEST_LOCK as SESSION_TEST_LOCK;

    #[test]
    fn a_second_open_while_one_is_registered_is_busy() {
        let _serial = SESSION_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _registered = transport::session::TestSession::install();
        let info = DeviceInfo::new(1, 2, "Fake DAC");
        let error = Dac::open(-1, info, DacConfig::default()).unwrap_err();
        assert_eq!(error, Error::Busy);
        assert_eq!(error.code(), "busy");
    }

    #[test]
    fn a_failed_open_leaves_nothing_registered() {
        let _serial = SESSION_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let info = DeviceInfo::new(1, 2, "Fake DAC");
        let error = Dac::open(-1, info, DacConfig::default()).unwrap_err();
        // No usbfs here (or an invalid descriptor): not Busy, and the slot is free.
        assert!(
            matches!(error, Error::PlatformUnsupported | Error::Claim(_)),
            "{error:?}"
        );
        assert!(!transport::session::session_registered());
    }

    #[test]
    fn an_invalid_config_is_rejected_before_anything_is_touched() {
        let _serial = SESSION_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let config = DacConfig {
            buffer_target_ms: 900,
            ..DacConfig::default()
        };
        let error = Dac::open(-1, DeviceInfo::new(1, 2, "x"), config).unwrap_err();
        assert!(matches!(error, Error::InvalidArgument(_)));
    }
}
