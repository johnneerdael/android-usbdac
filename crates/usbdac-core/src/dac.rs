//! The DAC handle.

use crate::capabilities::{Capabilities, DeviceInfo};
use crate::config::{CloseVolume, DacConfig, HandBack};
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

    /// Closes with what the configuration says about the original volume
    /// (`None`) or with what the host decided at close.
    fn shutdown(&self, volume: Option<CloseVolume>) -> Result<()> {
        if self.closed.swap(true, Ordering::AcqRel) {
            return Ok(());
        }
        if let Some(stream) = self.active_stream() {
            let _ = stream.close();
        }
        // A DAC that has gone cannot be written to, and what was recorded for
        // it must not survive into a later attachment: forget it, whatever was
        // asked for.
        let action = if self.events.device_lost() {
            transport::api::ORIGINAL_VOLUME_ACTION_FORGET
        } else {
            match volume {
                Some(CloseVolume::Restore) => transport::api::ORIGINAL_VOLUME_ACTION_RESTORE,
                Some(CloseVolume::Keep) => transport::api::ORIGINAL_VOLUME_ACTION_KEEP,
                Some(CloseVolume::Forget) => transport::api::ORIGINAL_VOLUME_ACTION_FORGET,
                None => match self.config.hand_back {
                    HandBack::RestoreOriginal => transport::api::ORIGINAL_VOLUME_ACTION_RESTORE,
                    HandBack::Keep => transport::api::ORIGINAL_VOLUME_ACTION_KEEP,
                },
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
        f.debug_struct("Dac")
            .field("info", &self.shared.info)
            .finish_non_exhaustive()
    }
}

impl Dac {
    /// Wait until USB activity has retired before releasing the host's claimed interfaces and FD.
    ///
    /// After [`Dac::close`] returns a transfer timeout, retain all host ownership
    /// and call this until it returns `true`. This does not require a live `Dac` handle.
    pub fn await_usb_retirement(timeout: Duration) -> bool {
        transport::api::await_usb_retirement(timeout)
    }

    /// Identify optional capture interfaces after the host claims the base audio interfaces.
    /// Returns the active configuration ID and source interface numbers without starting audio.
    pub fn feedback_preflight(fd: RawFd, info: &DeviceInfo) -> Result<(u8, Vec<u8>)> {
        let _guard = OPEN_GUARD.lock();
        if transport::session::session_registered() {
            return Err(Error::Busy);
        }
        transport::api::feedback_preflight(fd, info)
    }

    /// Opens the DAC behind `fd`.
    ///
    /// `fd` is the usbfs file descriptor of the device (on Android
    /// `UsbDeviceConnection.getFileDescriptor()`); the library duplicates it, so
    /// the host keeps ownership of its own. `info.claimed_interfaces` must be
    /// the AudioControl and playback AudioStreaming interfaces plus any
    /// optional capture sources identified by [`Dac::feedback_preflight`].
    ///
    /// Opening scans the descriptors, finds the hardware volume control, records
    /// the DAC's current level and, with the default
    /// [`StartupVolume::Attenuate`](crate::StartupVolume::Attenuate), lowers every
    /// channel to at most -40 dB and verifies it, so that no stream can start
    /// at the DAC's own (often full-scale) level. It does not touch the clock or
    /// start any audio. [`VolumeControl::startup`](crate::VolumeControl::startup)
    /// says what it did.
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
        let events = EventHub::start();
        // The start-up step lowered the level before this handle existed: tell
        // the first listener, as a change made through the library.
        if let Some(state) = transport::volume_hw::startup_lowered_volume() {
            events.emit_dac_or_hold(crate::events::DacEvent::VolumeChanged(state));
        }
        Ok(Dac {
            shared: Arc::new(DacShared {
                events,
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
    /// Fails with [`Error::VolumeUnavailable`] when the stream's own route has
    /// no usable hardware volume control and
    /// [`DacConfig::require_hardware_volume`] is on; nothing is programmed and
    /// no output thread starts.
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
    /// close its connection after success. If closing times out, keep all host
    /// claims and the connection until [`Dac::await_usb_retirement`] returns
    /// `true`. Dropping the `Dac` initiates the same cleanup, but cannot report
    /// a timeout; hosts must still wait for retirement before releasing claims.
    pub fn close(self) -> Result<()> {
        self.shared.shutdown(None)
    }

    /// Like [`close`](Self::close), but the host says what becomes of the level
    /// the DAC had before the library first changed it, instead of
    /// [`DacConfig::hand_back`] deciding at open: write it back (another driver
    /// gets the DAC), keep the record (the host holds on to the DAC or takes it
    /// straight back) or forget it (the DAC was unplugged).
    ///
    /// A DAC the library saw disappear is always forgotten: nothing can be
    /// written to it and a later attachment must start fresh.
    pub fn close_with(self, volume: CloseVolume) -> Result<()> {
        self.shared.shutdown(Some(volume))
    }

    /// Changes the native DSD overrides of the open DAC ([`DacConfig::dsd_big_endian`]
    /// and [`DacConfig::dsd_subslot`]), for a host whose user can change them
    /// while the DAC stays open. `None` goes back to the device quirk.
    ///
    /// They are read when a format is negotiated and when a stream is opened: a
    /// stream that is already open keeps what it started with, and
    /// [`capabilities`](Self::capabilities) reflects a new subslot size after the
    /// next [`negotiate`](Self::negotiate). `subslot` must be 1, 2 or 4.
    pub fn set_dsd_overrides(&self, big_endian: Option<bool>, subslot: Option<u8>) -> Result<()> {
        self.shared.ensure_open()?;
        if subslot.is_some_and(|size| !matches!(size, 1 | 2 | 4)) {
            return Err(Error::InvalidArgument(
                "dsd_subslot must be 1, 2 or 4".to_string(),
            ));
        }
        settings::update(|config| {
            config.dsd_big_endian = big_endian;
            config.dsd_subslot = subslot;
        });
        Ok(())
    }
}

impl Drop for Dac {
    fn drop(&mut self) {
        if let Err(error) = self.shared.shutdown(None) {
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

    /// A `Dac` over the fake registered session.
    fn fake_dac_handle(config: DacConfig) -> Dac {
        Dac {
            shared: Arc::new(DacShared {
                events: EventHub::start(),
                info: DeviceInfo::new(0x1234, 0x5678, "test DAC"),
                config,
                closed: AtomicBool::new(false),
                stream: Mutex::new(None),
            }),
        }
    }

    /// A DAC whose level the library lowered, with the true original on record.
    fn lowered_with_a_record() -> crate::transport::fake_dac::Registered {
        use crate::transport::fake_dac::{Registered, ORIGINAL_RAW};
        let fake = Registered::new(false, true);
        fake.set_levels(ORIGINAL_RAW, ORIGINAL_RAW);
        fake.record_original();
        crate::transport::volume_hw::write_volume_db(-40.0).unwrap();
        assert_ne!(fake.raw(), vec![ORIGINAL_RAW; 2]);
        fake
    }

    #[test]
    fn close_with_restore_writes_the_original_back_and_ends_the_record() {
        use crate::transport::fake_dac::ORIGINAL_RAW;
        let fake = lowered_with_a_record();
        // The configuration says keep: the host's decision at close wins.
        let dac = fake_dac_handle(DacConfig {
            hand_back: HandBack::Keep,
            ..DacConfig::default()
        });
        dac.close_with(CloseVolume::Restore).unwrap();
        assert_eq!(fake.raw(), vec![ORIGINAL_RAW; 2]);
        assert!(!fake.original_recorded());
        assert!(!transport::session::session_registered());
    }

    #[test]
    fn close_with_keep_leaves_the_level_and_the_record() {
        use crate::transport::fake_dac::ORIGINAL_RAW;
        let fake = lowered_with_a_record();
        let lowered = fake.raw();
        // The configuration says restore: again the host's decision wins.
        let dac = fake_dac_handle(DacConfig::default());
        dac.close_with(CloseVolume::Keep).unwrap();
        assert_eq!(fake.raw(), lowered, "the level was not touched");
        assert_ne!(lowered, vec![ORIGINAL_RAW; 2]);
        assert!(
            fake.original_recorded(),
            "a later open still knows the true original"
        );
        assert!(!transport::session::session_registered());
    }

    #[test]
    fn close_with_forget_ends_the_record_without_writing() {
        let fake = lowered_with_a_record();
        let lowered = fake.raw();
        let writes = fake.dac.lock().unwrap().volume_writes.len();
        let dac = fake_dac_handle(DacConfig::default());
        dac.close_with(CloseVolume::Forget).unwrap();
        assert_eq!(fake.raw(), lowered);
        assert_eq!(
            fake.dac.lock().unwrap().volume_writes.len(),
            writes,
            "nothing was written"
        );
        assert!(!fake.original_recorded());
    }

    #[test]
    fn a_dac_the_library_saw_disappear_is_forgotten_whatever_close_asked_for() {
        let fake = lowered_with_a_record();
        let lowered = fake.raw();
        let dac = fake_dac_handle(DacConfig::default());
        dac.shared
            .events
            .emit_dac(crate::events::DacEvent::DeviceLost);
        dac.close_with(CloseVolume::Restore).unwrap();
        assert_eq!(fake.raw(), lowered, "no write to a DAC that is gone");
        assert!(!fake.original_recorded());
    }

    #[test]
    fn a_plain_close_still_follows_the_configuration() {
        use crate::transport::fake_dac::ORIGINAL_RAW;
        let fake = lowered_with_a_record();
        fake_dac_handle(DacConfig::default()).close().unwrap();
        assert_eq!(
            fake.raw(),
            vec![ORIGINAL_RAW; 2],
            "the default is to restore"
        );
        drop(fake);

        let fake = lowered_with_a_record();
        let lowered = fake.raw();
        fake_dac_handle(DacConfig {
            hand_back: HandBack::Keep,
            ..DacConfig::default()
        })
        .close()
        .unwrap();
        assert_eq!(fake.raw(), lowered);
        assert!(fake.original_recorded());
    }

    #[test]
    fn dsd_overrides_change_the_settings_of_the_open_dac_and_are_validated() {
        let fake = crate::transport::fake_dac::Registered::new(false, false);
        fake.configure(DacConfig::default());
        let dac = fake_dac_handle(DacConfig::default());
        assert!(!settings::settings().dsd_big_endian());
        assert_eq!(settings::settings().dsd_subslot_override(), None);

        dac.set_dsd_overrides(Some(true), Some(2)).unwrap();
        assert!(settings::settings().dsd_big_endian());
        assert_eq!(settings::settings().dsd_subslot_override(), Some(2));

        assert!(matches!(
            dac.set_dsd_overrides(None, Some(3)),
            Err(Error::InvalidArgument(_))
        ));
        assert_eq!(
            settings::settings().dsd_subslot_override(),
            Some(2),
            "a refused call changes nothing"
        );

        dac.set_dsd_overrides(None, None).unwrap();
        assert!(!settings::settings().dsd_big_endian());
        assert_eq!(settings::settings().dsd_subslot_override(), None);
        dac.close().unwrap();
        drop(fake);
    }

    #[test]
    fn dsd_overrides_keep_the_quirks_resolved_for_the_device() {
        let fake = crate::transport::fake_dac::Registered::new(false, false);
        fake.configure(DacConfig {
            quirks: vec![crate::config::QuirkOverride {
                vendor_id: 0x1234,
                product_id: 0x5678,
                name_contains: None,
                quirks: vec![
                    crate::quirk::Quirk::SettleDelayMs(80),
                    crate::quirk::Quirk::DsdBigEndian,
                ],
            }],
            ..DacConfig::default()
        });
        let dac = fake_dac_handle(DacConfig::default());
        // The quirk gives big-endian; an override of "little-endian" wins over it.
        assert!(settings::settings().dsd_big_endian());
        dac.set_dsd_overrides(Some(false), None).unwrap();
        assert!(!settings::settings().dsd_big_endian());
        assert_eq!(
            settings::settings().settle_delay_ms(),
            80,
            "the device quirk is still there"
        );
        // `None` goes back to the quirk.
        dac.set_dsd_overrides(None, None).unwrap();
        assert!(settings::settings().dsd_big_endian());
        dac.close().unwrap();
        drop(fake);
    }

    #[test]
    fn the_volume_handle_works_without_a_stream_and_reports_changes() {
        use crate::events::DacEvent;
        use crate::transport::fake_dac::Registered;
        use crate::volume::VolumeControl;

        struct Seen(Mutex<Vec<DacEvent>>);
        impl DacListener for Seen {
            fn on_event(&self, event: DacEvent) {
                self.0.lock().push(event);
            }
        }

        // Holds the process-wide test lock for the whole test.
        let fake = Registered::new(false, true);
        let events = EventHub::start();
        let seen = Arc::new(Seen(Mutex::new(Vec::new())));
        events.set_dac_listener(Some(seen.clone()));
        let shared = Arc::new(DacShared {
            events: events.clone(),
            info: DeviceInfo::new(0x1234, 0x5678, "test DAC"),
            config: DacConfig::default(),
            closed: AtomicBool::new(false),
            stream: Mutex::new(None),
        });
        let volume = VolumeControl::new(shared.clone());

        let range = volume.range().unwrap();
        assert_eq!(
            (range.min_db, range.max_db, range.res_db),
            (-80.0, 0.0, 1.0)
        );
        let initial = volume.get().unwrap();
        assert_eq!(initial.db, -64.0);
        assert_eq!(initial.muted, Some(false));

        let set = volume.set_db(-30.0).unwrap();
        assert_eq!(set.db, -30.0);
        assert_eq!(fake.raw(), vec![-30 * 256, -30 * 256]);
        let up = volume.step(1, 0).unwrap();
        assert!(up.db > -30.0, "{up:?}");
        let half = volume.set_fraction(0.5).unwrap();
        assert!((half.fraction - 0.5).abs() < 0.01, "{half:?}");
        assert_eq!(volume.set_mute(true).unwrap().muted, Some(true));
        assert_eq!(
            volume.original_db(),
            None,
            "nothing was recorded by the fake"
        );

        // An unreachable DAC fails the call; the handle stays usable.
        fake.dac.lock().unwrap().unreachable = true;
        assert!(volume.get().is_err());
        fake.dac.lock().unwrap().unreachable = false;

        for _ in 0..200 {
            if seen.0.lock().len() >= 4 {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        events.shutdown();
        let changes: Vec<f64> = seen
            .0
            .lock()
            .iter()
            .filter_map(|event| match event {
                DacEvent::VolumeChanged(state) => Some(state.db),
                _ => None,
            })
            .collect();
        assert_eq!(
            changes.len(),
            4,
            "set_db, step, set_fraction, set_mute: {changes:?}"
        );
        assert_eq!(changes[0], -30.0);

        shared.closed.store(true, Ordering::Release);
        assert!(matches!(volume.get(), Err(Error::InvalidState(_))));
    }
}
