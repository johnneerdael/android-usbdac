//! The DAC handle.

mod routing;

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

#[cfg(test)]
fn reconfigure_or_restore<T>(
    requested: &StreamRequest,
    previous: &StreamRequest,
    mut attempt: impl FnMut(&StreamRequest) -> Result<T>,
) -> Result<(T, Option<Error>)> {
    match attempt(requested) {
        Ok(stream) => Ok((stream, None)),
        Err(error) => match attempt(previous) {
            Ok(stream) => Ok((stream, Some(error))),
            Err(rollback) => Err(Error::ReconfigureUnusable(format!(
                "requested format failed: {error}; rollback failed: {rollback}"
            ))),
        },
    }
}

enum CandidateFailure {
    Clean(Error),
    Terminal(Error),
}

// The attempt closure is the production negotiate/open boundary. Only its
// private transport certificate permits the next candidate or final rollback.
fn reconfigure_candidates_or_restore<T>(
    requests: &[StreamRequest],
    previous: &StreamRequest,
    mut attempt: impl FnMut(&StreamRequest) -> std::result::Result<T, CandidateFailure>,
) -> Result<(T, Option<Error>)> {
    let mut first = None;
    for request in requests {
        match attempt(request) {
            Ok(stream) => return Ok((stream, None)),
            Err(CandidateFailure::Clean(error)) => {
                if first.is_none() {
                    first = Some(error);
                }
            }
            Err(CandidateFailure::Terminal(error)) => {
                return Err(Error::ReconfigureUnusable(format!(
                    "candidate outcome is not proven idle; explicit close required: {error}"
                )))
            }
        }
    }
    let first = first.expect("validated nonempty candidate list");
    match attempt(previous) {
        Ok(stream) => Ok((stream, Some(first))),
        Err(CandidateFailure::Clean(rollback) | CandidateFailure::Terminal(rollback)) => {
            Err(Error::ReconfigureUnusable(format!(
                "requested format failed: {first}; rollback failed: {rollback}"
            )))
        }
    }
}

fn validate_reconfigure_candidates(requests: &[StreamRequest]) -> Result<()> {
    if requests.is_empty() || requests.len() > 4 {
        return Err(Error::InvalidArgument(
            "expected one to four reconfiguration candidates".into(),
        ));
    }
    for request in requests {
        transport::api::playback_format_from_request(request)?;
    }
    if requests.len() > 1 {
        let first = requests[0];
        if first.encoding != crate::Encoding::Pcm
            || first.rate_policy != crate::RatePolicy::Exact
            || requests
                .windows(2)
                .any(|pair| pair[0].bit_depth >= pair[1].bit_depth)
            || requests.iter().any(|r| {
                r.encoding != first.encoding
                    || r.sample_rate != first.sample_rate
                    || r.channels != first.channels
                    || r.rate_policy != crate::RatePolicy::Exact
            })
        {
            return Err(Error::InvalidArgument(
                "candidates must be ascending same-rate, same-channel PCM widening".into(),
            ));
        }
    }
    Ok(())
}

/// State shared by the `Dac`, its streams and its volume handle.
pub(crate) struct DacShared {
    pub(crate) events: EventSink,
    pub(crate) context: Arc<transport::context::SessionContext>,
    pub(crate) info: DeviceInfo,
    pub(crate) config: DacConfig,
    closed: AtomicBool,
    pub(crate) control_guard: Mutex<()>,
    close_volume: Mutex<Option<CloseVolume>>,
    pub(crate) stream: Mutex<Option<Weak<StreamInner>>>,
    reconfigure: Mutex<Option<Arc<()>>>,
    reconfigure_unusable: AtomicBool,
    last_feedback_stream: Mutex<Option<(StreamFormat, Arc<transport::RuntimeStats>)>>,
}

struct ReconfigureReservation<'a> {
    shared: &'a DacShared,
    token: Arc<()>,
    terminal_on_drop: bool,
}

impl Drop for ReconfigureReservation<'_> {
    fn drop(&mut self) {
        let mut reservation = self.shared.reconfigure.lock();
        if reservation
            .as_ref()
            .is_some_and(|token| Arc::ptr_eq(token, &self.token))
        {
            *reservation = None;
            if self.terminal_on_drop && !self.shared.closed.load(Ordering::Acquire) {
                self.shared
                    .reconfigure_unusable
                    .store(true, Ordering::Release);
            }
        }
    }
}

impl DacShared {
    pub(crate) fn reject_reconfigure(&self) -> Result<()> {
        let reservation = self.reconfigure.lock();
        if self.reconfigure_unusable.load(Ordering::Acquire) {
            Err(Error::ReconfigureUnusable(
                "explicit close required after failed reconfiguration".into(),
            ))
        } else if reservation.is_some() {
            Err(Error::Busy)
        } else {
            Ok(())
        }
    }

    fn remember_feedback_stream(&self, format: &StreamFormat, stream: &StreamInner) {
        if stream.shared.stats.feedback_trace.is_some() {
            *self.last_feedback_stream.lock() =
                Some((format.clone(), Arc::clone(&stream.shared.stats)));
        }
    }

    pub(crate) fn enter(&self) -> Result<transport::context::Scope> {
        self.ensure_open()?;
        Ok(self.context.enter())
    }

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
        let _control = self.control_guard.lock();
        let _scope = self.context.enter();
        *self.reconfigure.lock() = None;
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
            match volume.or_else(|| self.close_volume.lock().take()) {
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
        drop(_control);
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
/// Distinct USB attachments have independent owners. Opening the same physical
/// attachment twice returns [`Error::Busy`] until its owner retires. The host keeps its
/// `UsbDeviceConnection` open until then.
pub struct Dac {
    shared: Arc<DacShared>,
}

/// A replacement stream on the same physical handle, paused with zero-based counters.
/// A failed requested format can still return a successfully restored old-format stream.
#[derive(Debug)]
pub struct StreamReconfiguration {
    pub stream: DacStream,
    /// The requested-format error when rollback restored the old format; `None` on success.
    pub rollback_cause: Option<Error>,
}

impl std::fmt::Debug for Dac {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Dac")
            .field("info", &self.shared.info)
            .finish_non_exhaustive()
    }
}

impl Dac {
    /// Read-only proof of a fixed-rate vendor PCM profile. Returns the actual
    /// active configuration and the sole interface the host may claim.
    pub fn vendor_pcm_preflight(fd: RawFd, info: &DeviceInfo) -> Result<Option<(u8, u8)>> {
        let _guard = OPEN_GUARD.lock();
        let context = transport::context::SessionContext::reserve(fd)?;
        let _scope = context.enter();
        if transport::session::session_registered() {
            return Err(Error::Busy);
        }
        transport::api::vendor_pcm_preflight(fd, info)
    }

    /// Read-only descriptor proof of the exact active configuration and complete
    /// interface claim set for a fixed-rate vendor PCM profile.
    pub fn vendor_pcm_claims_preflight(
        fd: RawFd,
        info: &DeviceInfo,
    ) -> Result<Option<(u8, Vec<u8>)>> {
        let _guard = OPEN_GUARD.lock();
        let context = transport::context::SessionContext::reserve(fd)?;
        let _scope = context.enter();
        if transport::session::session_registered() {
            return Err(Error::Busy);
        }
        transport::api::vendor_pcm_claims_preflight(fd, info)
    }

    /// Prove the exact optional Scarlett vendor claim from actual USB descriptors.
    /// Requires the host-selected configuration; sends no proprietary requests.
    pub fn scarlett_routing_preflight(
        fd: RawFd,
        info: &DeviceInfo,
    ) -> Result<Option<crate::scarlett::ReadCapability>> {
        let _guard = OPEN_GUARD.lock();
        let context = transport::context::SessionContext::reserve(fd)?;
        let _scope = context.enter();
        if transport::session::session_registered() {
            return Err(Error::Busy);
        }
        transport::routing_hw::scarlett_preflight(fd, info)
    }

    /// Read the active configuration's explicit PCM IN claim plan before the
    /// host claims its AudioControl and input streaming interfaces. Returns
    /// `(configuration, control_interfaces, input_interfaces)` without audio.
    pub fn capture_preflight(fd: RawFd, info: &DeviceInfo) -> Result<(u8, Vec<u8>, Vec<u8>)> {
        let _guard = OPEN_GUARD.lock();
        let context = transport::context::SessionContext::reserve(fd)?;
        let _scope = context.enter();
        if transport::session::session_registered() {
            return Err(Error::Busy);
        }
        let device = transport::api::capture_preflight_device(fd, info)?;
        Ok(device)
    }
    /// Register an explicitly claimed capture device, including an input-only
    /// device. Claim its IN AudioStreaming interfaces and their owning
    /// AudioControl interfaces before calling; no playback claim is needed.
    /// This entry point performs no output-volume setup. Android/JNI capture
    /// permission and claim orchestration is a separate integration step.
    pub fn open_capture_device(fd: RawFd, info: DeviceInfo, config: DacConfig) -> Result<Dac> {
        config.validate()?;
        let _guard = OPEN_GUARD.lock();
        let context = transport::context::SessionContext::reserve(fd)?;
        let _scope = context.enter();
        if transport::session::session_registered() {
            return Err(Error::Busy);
        }
        settings::install(Settings::for_device(config.clone(), &info));
        if let Err(error) = transport::api::open_capture_device(fd, &info) {
            settings::reset();
            return Err(error);
        }
        context.admit();
        Ok(Dac {
            shared: Arc::new(DacShared {
                context: transport::context::current(),
                events: EventHub::start(),
                info,
                config,
                closed: AtomicBool::new(false),
                control_guard: Mutex::new(()),
                close_volume: Mutex::new(None),
                stream: Mutex::new(None),
                reconfigure: Mutex::new(None),
                reconfigure_unusable: AtomicBool::new(false),
                last_feedback_stream: Mutex::new(None),
            }),
        })
    }

    /// Start standalone, exact-format PCM capture on a claimed IN interface.
    /// The capture worker exclusively owns USB until all transfers retire;
    /// concurrent playback, duplex and routing changes are refused.
    pub fn open_capture(
        &self,
        request: &crate::capture::CaptureRequest,
        capacity_frames: usize,
    ) -> Result<crate::capture::DacCaptureStream> {
        let _control = self.shared.control_guard.lock();
        self.shared.reject_reconfigure()?;
        let _scope = self.shared.enter()?;
        let handle = transport::capture_stream::open_capture(
            request,
            capacity_frames,
            self.shared.events.clone(),
        )?;
        Ok(crate::capture::DacCaptureStream::new(
            Arc::clone(&self.shared),
            handle,
        ))
    }

    /// Capture owner-specific retirement evidence before consuming `close`.
    /// Retain this token with this device's claims/FD after a close timeout;
    /// another open device does not affect its result.
    pub fn retirement(&self) -> UsbRetirement {
        UsbRetirement {
            context: self.shared.context.clone(),
        }
    }

    /// Wait until all process USB owners retire. For a particular connection,
    /// use [`Dac::retirement`] so another active DAC does not block hand-back.
    ///
    /// After [`Dac::close`] returns a transfer timeout, retain all host ownership
    /// and call this until it returns `true`. This does not require a live `Dac` handle.
    /// Check retirement using the actual host connection FD after a failed open.
    /// The host must keep this FD valid; caller-supplied USB names are not identity.
    pub fn await_connection_retirement(fd: RawFd, timeout: Duration) -> Result<bool> {
        transport::context::await_connection_retirement(fd, timeout)
    }

    pub fn await_usb_retirement(timeout: Duration) -> bool {
        transport::context::await_all_retirement(timeout)
    }

    /// Identify optional capture interfaces after the host claims the base audio interfaces.
    /// Returns the active configuration ID and source interface numbers without starting audio.
    pub fn feedback_preflight(fd: RawFd, info: &DeviceInfo) -> Result<(u8, Vec<u8>)> {
        let _guard = OPEN_GUARD.lock();
        let context = transport::context::SessionContext::reserve(fd)?;
        let _scope = context.enter();
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
        Self::open_playback_device(fd, info, config, false)
    }

    /// Open a fresh macOS libusb attachment for direct playback. When
    /// `allow_capture` is true libusb may take the whole USB device away from
    /// its kernel driver, subject to macOS authorization.
    #[cfg(target_os = "macos")]
    pub fn open_darwin(
        attachment: crate::darwin_discovery::Attachment,
        config: DacConfig,
        allow_capture: bool,
    ) -> Result<Dac> {
        config.validate()?;
        let _guard = OPEN_GUARD.lock();
        let context = transport::context::SessionContext::reserve_darwin(attachment.device())?;
        let _scope = context.enter();
        if transport::session::session_registered() {
            return Err(Error::Busy);
        }
        let (device, info) = transport::usbfs::open_darwin_device(&attachment, allow_capture)?;
        settings::install(Settings::for_device(config.clone(), &info));
        if let Err(error) = transport::api::register_darwin_device(device, &info) {
            settings::reset();
            return Err(error);
        }
        context.admit();
        let events = EventHub::start();
        if let Some(state) = transport::volume_hw::startup_lowered_volume() {
            events.emit_dac_or_hold(crate::events::DacEvent::VolumeChanged(state));
        }
        Ok(Dac {
            shared: Arc::new(DacShared {
                context: transport::context::current(),
                events,
                info,
                config,
                closed: AtomicBool::new(false),
                control_guard: Mutex::new(()),
                close_volume: Mutex::new(None),
                stream: Mutex::new(None),
                reconfigure: Mutex::new(None),
                reconfigure_unusable: AtomicBool::new(false),
                last_feedback_stream: Mutex::new(None),
            }),
        })
    }

    /// Explicit duplex registration. Claim the base playback interfaces and all
    /// IN/control interfaces returned by capture_preflight for the same configuration.
    /// Ordinary playback registration retains its narrower optional-IN policy.
    pub fn open_duplex_device(fd: RawFd, info: DeviceInfo, config: DacConfig) -> Result<Dac> {
        Self::open_playback_device(fd, info, config, true)
    }

    fn open_playback_device(
        fd: RawFd,
        info: DeviceInfo,
        config: DacConfig,
        duplex: bool,
    ) -> Result<Dac> {
        config.validate()?;
        let _guard = OPEN_GUARD.lock();
        let context = transport::context::SessionContext::reserve(fd)?;
        let _scope = context.enter();
        if transport::session::session_registered() {
            return Err(Error::Busy);
        }
        settings::install(Settings::for_device(config.clone(), &info));
        let registration = if duplex {
            transport::api::open_duplex_device(fd, &info)
        } else {
            transport::api::open_device(fd, &info)
        };
        if let Err(error) = registration {
            settings::reset();
            return Err(error);
        }
        context.admit();
        let events = EventHub::start();
        // The start-up step lowered the level before this handle existed: tell
        // the first listener, as a change made through the library.
        if let Some(state) = transport::volume_hw::startup_lowered_volume() {
            events.emit_dac_or_hold(crate::events::DacEvent::VolumeChanged(state));
        }
        Ok(Dac {
            shared: Arc::new(DacShared {
                context: transport::context::current(),
                events,
                info,
                config,
                closed: AtomicBool::new(false),
                control_guard: Mutex::new(()),
                close_volume: Mutex::new(None),
                stream: Mutex::new(None),
                reconfigure: Mutex::new(None),
                reconfigure_unusable: AtomicBool::new(false),
                last_feedback_stream: Mutex::new(None),
            }),
        })
    }

    /// The identity the host gave at open.
    pub fn info(&self) -> &DeviceInfo {
        &self.shared.info
    }

    /// Export the last stream's opt-in support capture, even after the stream
    /// has closed. A new successfully opened stream replaces it.
    pub fn feedback_trace(&self) -> Option<crate::FeedbackTrace> {
        let (format, stats) = self.shared.last_feedback_stream.lock().clone()?;
        let explicit = *stats.explicit_feedback_diagnostics.lock();
        Some(stats.feedback_trace.as_ref()?.snapshot(format, explicit))
    }

    /// What the DAC offers, from the descriptors scanned at open.
    pub fn capabilities(&self) -> Result<Capabilities> {
        let _scope = self.shared.enter()?;
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
        let _control = self.shared.control_guard.lock();
        self.shared.reject_reconfigure()?;
        self.negotiate_locked(request)
    }

    /// Negotiate a bounded, ordered list of exact PCM formats on one retained owner.
    /// A later candidate is considered only after a certified clean refusal.
    pub fn negotiate_candidates(&self, requests: &[StreamRequest]) -> Result<StreamFormat> {
        let _control = self.shared.control_guard.lock();
        self.shared.reject_reconfigure()?;
        let _scope = self.shared.enter()?;
        transport::api::negotiate_candidates(requests)
    }

    pub(crate) fn negotiate_playback_candidates(
        &self,
        requests: &[StreamRequest],
    ) -> Result<(StreamFormat, usize, Option<Error>)> {
        let _control = self.shared.control_guard.lock();
        self.shared.reject_reconfigure()?;
        let _scope = self.shared.enter()?;
        transport::api::negotiate_playback_candidates(requests)
    }

    fn negotiate_locked(&self, request: &StreamRequest) -> Result<StreamFormat> {
        let _scope = self.shared.enter()?;
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
        let _control = self.shared.control_guard.lock();
        self.shared.reject_reconfigure()?;
        self.open_stream_locked(format, listener)
    }

    fn open_stream_locked(
        &self,
        format: &StreamFormat,
        listener: Arc<dyn DacListener>,
    ) -> Result<DacStream> {
        let _scope = self.shared.enter()?;
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
        self.shared.remember_feedback_stream(format, &inner);
        *slot = Some(Arc::downgrade(&inner));
        drop(slot);
        inner.start_events();
        Ok(DacStream::from_inner(inner))
    }

    /// Drain and replace a playback-only stream without reopening the physical USB handle or host claims.
    ///
    /// Stop the writer before calling. The stream must belong to this DAC. Drain temporarily plays
    /// a paused stream so queued audio can complete. A drain timeout retains the original open
    /// stream in draining state; the caller still owns it and may wait, flush or close it.
    ///
    /// After retirement, normal lock-requested cleanup retains the exact libusb owner in IDLE_LOCK.
    /// Every negotiation/open must reuse that identity; losing it is an unusable-state error, never
    /// an implicit physical reopen. No volume initialization/hand-back occurs. Success or rollback
    /// returns a paused replacement with zero-based counters; supply/reseek PCM and explicitly play.
    /// A rollback failure returns `ReconfigureUnusable` and requires closing the DAC.
    /// While draining, routing and negotiation return `Busy`; volume remains available.
    pub fn reconfigure_stream(
        &self,
        current: &DacStream,
        request: &StreamRequest,
        listener: Arc<dyn DacListener>,
        drain_timeout: Duration,
    ) -> Result<StreamReconfiguration> {
        self.reconfigure_stream_candidates(
            current,
            std::slice::from_ref(request),
            listener,
            drain_timeout,
        )
    }

    /// Replace once using a bounded list of equivalent-rate lossless widening candidates.
    /// Continue only after a certified clean refusal. A terminal failure requires
    /// explicit close; exhaustion restores the previous request once.
    pub fn reconfigure_stream_candidates(
        &self,
        current: &DacStream,
        requests: &[StreamRequest],
        listener: Arc<dyn DacListener>,
        drain_timeout: Duration,
    ) -> Result<StreamReconfiguration> {
        validate_reconfigure_candidates(requests)?;
        let request = &requests[0];
        let control = self.shared.control_guard.lock();
        self.shared.reject_reconfigure()?;
        let _scope = self.shared.enter()?;
        transport::api::playback_format_from_request(request)?;
        let identity = current.reconfigure_identity(&self.shared)?;
        if !current.is_current(&self.shared) {
            return Err(Error::InvalidState("stream is no longer current".into()));
        }
        let token = Arc::new(());
        *self.shared.reconfigure.lock() = Some(Arc::clone(&token));
        let mut reservation = ReconfigureReservation {
            shared: &self.shared,
            token,
            terminal_on_drop: false,
        };
        let previous = current.format().request;
        let deadline = std::time::Instant::now()
            .checked_add(drain_timeout)
            .ok_or_else(|| {
                Error::InvalidArgument("drain timeout overflows monotonic clock".into())
            })?;
        let volume_route = transport::api::volume_route_signature();
        if !current.is_drained() {
            current.drain()?;
            current.play()?;
        }
        drop(control);
        while !current.is_drained() {
            if self.shared.events.device_lost() || self.shared.closed.load(Ordering::Acquire) {
                reservation.terminal_on_drop = true;
                return Err(Error::ReconfigureUnusable(
                    "DAC closed or detached during drain".into(),
                ));
            }
            if matches!(
                current.state(),
                crate::StreamState::Failed | crate::StreamState::Closed
            ) {
                reservation.terminal_on_drop = true;
                return Err(Error::ReconfigureUnusable("original stream failed during drain; retained stream remains available for explicit close".into()));
            }
            if std::time::Instant::now() >= deadline {
                return Err(Error::ReconfigureDrainTimeout);
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        reservation.terminal_on_drop = true;
        let _control = self.shared.control_guard.lock();
        self.shared.ensure_open()?;
        let retirement = current.lock_retirement();
        if self.shared.events.device_lost()
            || !self
                .shared
                .reconfigure
                .lock()
                .as_ref()
                .is_some_and(|token| Arc::ptr_eq(token, &reservation.token))
            || !current.is_current(&self.shared)
            || current.reconfigure_identity(&self.shared)? != identity
            || !current.is_drained()
            || matches!(
                current.state(),
                crate::StreamState::Failed | crate::StreamState::Closed
            )
            || transport::api::volume_route_signature() != volume_route
        {
            return Err(Error::ReconfigureUnusable(
                "stream ownership, state, or volume route changed during drain".into(),
            ));
        }
        current
            .retire_for_reconfigure_locked()
            .map_err(|error| Error::ReconfigureUnusable(error.to_string()))?;
        drop(retirement);
        if transport::api::retained_usb_handle_identity() != Some(identity) {
            return Err(Error::ReconfigureUnusable(
                "old USB owner was not retained after retirement".into(),
            ));
        }
        let mut batch = transport::negotiate::RetryBaseline::new(identity);
        let mut candidate_reservation = Some(
            transport::api::reserve_candidates()
                .map_err(|error| Error::ReconfigureUnusable(error.to_string()))?,
        );
        let mut attempt =
            |target: &StreamRequest| -> std::result::Result<DacStream, CandidateFailure> {
                if transport::api::retained_usb_handle_identity() != Some(identity) {
                    return Err(CandidateFailure::Terminal(Error::ReconfigureUnusable(
                        "retained USB owner was lost before negotiation".into(),
                    )));
                }
                let format = match transport::api::negotiate_attempt(target, &mut batch, identity) {
                    Ok(format) => format,
                    Err(failure) => {
                        return if failure
                            .clean
                            .is_some_and(|proof| proof.belongs_to(identity))
                            && transport::api::retained_usb_handle_identity() == Some(identity)
                            && transport::api::volume_route_signature() == volume_route
                            && !self.shared.events.device_lost()
                            && !self.shared.closed.load(Ordering::Acquire)
                        {
                            Err(CandidateFailure::Clean(failure.error))
                        } else {
                            Err(CandidateFailure::Terminal(failure.error))
                        };
                    }
                };
                if transport::api::retained_usb_handle_identity() != Some(identity) {
                    return Err(CandidateFailure::Terminal(Error::ReconfigureUnusable(
                        "negotiation replaced or lost the USB owner".into(),
                    )));
                }
                // Stream start acquires its own session guard; all candidate
                // negotiations above share the reservation until this commit.
                drop(candidate_reservation.take());
                let opened = self
                    .open_stream_locked(&format, listener.clone())
                    .map_err(CandidateFailure::Terminal)?;
                if !matches!(opened.reconfigure_identity(&self.shared), Ok(found) if found == identity)
                {
                    let _ = opened.retire_for_reconfigure();
                    drop(opened);
                    return Err(CandidateFailure::Terminal(Error::ReconfigureUnusable(
                        "replacement used a different physical USB owner".into(),
                    )));
                }
                if let Err(error) = opened.pause() {
                    let _ = opened.retire_for_reconfigure();
                    return Err(CandidateFailure::Terminal(error));
                }
                if transport::api::volume_route_signature() != volume_route {
                    let _ = opened.retire_for_reconfigure();
                    drop(opened);
                    return Err(CandidateFailure::Terminal(Error::Control(
                    "reconfiguration changed the hardware volume route; explicit close required".into(),
                )));
                }
                Ok(opened)
            };
        let (stream, rollback_cause) =
            reconfigure_candidates_or_restore(requests, &previous, &mut attempt)?;
        reservation.terminal_on_drop = false;
        Ok(StreamReconfiguration {
            stream,
            rollback_cause,
        })
    }

    /// Open playback and PCM capture atomically on one USB worker.
    ///
    /// The output must have a current negotiated format. Shared source clocks
    /// require equal wire rates; distinct proven clock sources retain their own
    /// rates and position counters. Capture never supplies playback timing unless
    /// it exactly matches playback's already selected implicit feedback source.
    /// Closing either stream stops both endpoints and retires their transfers.
    pub fn open_duplex(
        &self,
        format: &StreamFormat,
        capture: &crate::capture::CaptureRequest,
        capacity_frames: usize,
        listener: Arc<dyn DacListener>,
    ) -> Result<(DacStream, crate::capture::DacCaptureStream)> {
        let _control = self.shared.control_guard.lock();
        self.shared.reject_reconfigure()?;

        let _scope = self.shared.enter()?;
        let mut slot = self.shared.stream.lock();
        if slot
            .as_ref()
            .and_then(Weak::upgrade)
            .is_some_and(|stream| !stream.is_closed())
        {
            return Err(Error::Busy);
        }
        self.shared.events.set_stream_listener(Some(listener));
        let (output, input) = match transport::duplex::open(
            format,
            capture,
            capacity_frames,
            self.shared.events.clone(),
        ) {
            Ok(pair) => pair,
            Err(error) => {
                self.shared.events.set_stream_listener(None);
                return Err(error);
            }
        };
        let inner = StreamInner::new(Arc::clone(&self.shared), output, format.clone());
        self.shared.remember_feedback_stream(format, &inner);
        *slot = Some(Arc::downgrade(&inner));
        drop(slot);
        inner.start_events();
        Ok((
            DacStream::from_inner(inner),
            crate::capture::DacCaptureStream::new(Arc::clone(&self.shared), input),
        ))
    }

    /// The hardware volume control, if the DAC has a writable one.
    pub fn volume(&self) -> Option<VolumeControl> {
        let _scope = self.shared.enter().ok()?;
        if !transport::volume_hw::has_hardware_volume() {
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

    /// Select the routing hand-back policy independently of volume hand-back.
    /// The policy remains attached to this owner if USB retirement is delayed.
    pub fn set_routing_hand_back(&self, hand_back: HandBack) -> Result<()> {
        let _control = self.shared.control_guard.lock();
        self.shared.ensure_open()?;
        self.store_routing_hand_back(hand_back);
        Ok(())
    }

    fn store_routing_hand_back(&self, hand_back: HandBack) {
        self.shared.context.routing_hand_back.store(
            match hand_back {
                HandBack::Keep => 1,
                HandBack::RestoreOriginal => 2,
            },
            Ordering::Release,
        );
    }

    /// Set host close policies that remain effective when the last owner drops later.
    #[doc(hidden)]
    pub fn set_close_hand_back(&self, volume: CloseVolume, routing: HandBack) -> Result<()> {
        let _control = self.shared.control_guard.lock();
        self.shared.ensure_open()?;
        *self.shared.close_volume.lock() = Some(volume);
        self.store_routing_hand_back(routing);
        Ok(())
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
        let _control = self.shared.control_guard.lock();
        self.shared.reject_reconfigure()?;
        let _scope = self.shared.enter()?;
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

/// A particular DAC owner's USB retirement, independent of other devices.
#[derive(Clone)]
pub struct UsbRetirement {
    context: Arc<transport::context::SessionContext>,
}
impl UsbRetirement {
    pub fn wait(&self, timeout: Duration) -> bool {
        self.context.retired(timeout)
    }

    /// Routing hand-back outcome after this owner has retired. None means no
    /// route changes were recorded; a pending owner is an explicit error.
    pub fn routing_hand_back_report(
        &self,
    ) -> Result<Option<crate::routing::RoutingHandBackReport>> {
        if !self.context.retired(Duration::ZERO) {
            return Err(Error::InvalidState("USB owner has not retired".into()));
        }
        Ok(self.context.routing_hand_back_report())
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
    fn another_owner_does_not_hide_an_invalid_device_descriptor() {
        let _serial = SESSION_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _registered = transport::session::TestSession::install();
        let info = DeviceInfo::new(1, 2, "Fake DAC");
        let error = Dac::open(-1, info, DacConfig::default()).unwrap_err();
        assert!(matches!(error, Error::Claim(_)));
        assert_eq!(error.code(), "claim");
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
                context: transport::context::current(),
                events: EventHub::start(),
                info: DeviceInfo::new(0x1234, 0x5678, "test DAC"),
                config,
                closed: AtomicBool::new(false),
                control_guard: Mutex::new(()),
                close_volume: Mutex::new(None),
                stream: Mutex::new(None),
                reconfigure: Mutex::new(None),
                reconfigure_unusable: AtomicBool::new(false),
                last_feedback_stream: Mutex::new(None),
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
    fn host_close_override_is_used_on_drop() {
        use crate::transport::fake_dac::ORIGINAL_RAW;
        let fake = lowered_with_a_record();
        let dac = fake_dac_handle(DacConfig {
            hand_back: HandBack::Keep,
            ..DacConfig::default()
        });
        dac.set_close_hand_back(CloseVolume::Restore, HandBack::Keep)
            .unwrap();
        drop(dac);
        assert_eq!(fake.raw(), vec![ORIGINAL_RAW; 2]);
        assert!(!fake.original_recorded());
    }

    #[test]
    fn explicit_close_with_overrides_host_close_policy() {
        let fake = lowered_with_a_record();
        let lowered = fake.raw();
        let dac = fake_dac_handle(DacConfig::default());
        dac.set_close_hand_back(CloseVolume::Restore, HandBack::Keep)
            .unwrap();
        dac.close_with(CloseVolume::Keep).unwrap();
        assert_eq!(fake.raw(), lowered);
        assert!(fake.original_recorded());
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
            context: transport::context::current(),
            events: events.clone(),
            info: DeviceInfo::new(0x1234, 0x5678, "test DAC"),
            config: DacConfig::default(),
            closed: AtomicBool::new(false),
            control_guard: Mutex::new(()),
            close_volume: Mutex::new(None),
            stream: Mutex::new(None),
            reconfigure: Mutex::new(None),
            reconfigure_unusable: AtomicBool::new(false),
            last_feedback_stream: Mutex::new(None),
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
    #[test]
    fn nonblocking_volume_is_bounded_ordered_and_keeps_verified_writes() {
        let fake = transport::fake_dac::Registered::new(false, true);
        let dac = fake_dac_handle(DacConfig::default());
        let volume = dac.volume().unwrap().nonblocking().unwrap();
        let (arrived_tx, arrived_rx) = crossbeam_channel::bounded(1);
        let (release_tx, release_rx) = crossbeam_channel::bounded(1);
        fake.dac.lock().unwrap().hold_next_volume_write = Some((arrived_tx, release_rx));
        let mut first = volume.set_db(-30.0).unwrap();
        // Submission returned while the actual verified USB write is held.
        arrived_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(first.try_result().is_none());
        let mut queued = Vec::new();
        for _ in 0..32 {
            queued.push(volume.set_db(-40.0).unwrap());
        }
        assert!(matches!(volume.set_db(-50.0), Err(Error::Busy)));
        release_tx.send(()).unwrap();
        assert_eq!(first.wait().unwrap().db, -30.0);
        for operation in queued {
            assert_eq!(operation.wait().unwrap().db, -40.0);
        }
        assert_eq!(fake.raw(), vec![-40 * 256, -40 * 256]);
        let result = volume.get().unwrap();
        // A completion can be observed with polling and then cannot be awaited twice.
        let state = result.wait().unwrap();
        assert_eq!(state.db, -40.0);
        dac.close().unwrap();
        assert!(matches!(
            volume.set_db(-20.0).unwrap().wait(),
            Err(Error::InvalidState(_))
        ));
    }

    #[test]
    fn reserved_transition_allows_volume_rejects_negotiate_and_close_invalidates_it() {
        let fake = transport::fake_dac::Registered::new(false, true);
        let dac = fake_dac_handle(DacConfig::default());
        let volume = dac.volume().unwrap();
        // Represent the interval after reconfigure has reserved the owner and
        // released the control gate to wait for the active stream to drain.
        let control = dac.shared.control_guard.lock();
        let token = Arc::new(());
        *dac.shared.reconfigure.lock() = Some(token.clone());
        let reservation = ReconfigureReservation {
            shared: &dac.shared,
            token,
            terminal_on_drop: false,
        };
        drop(control);
        std::thread::scope(|scope| {
            let operation = scope.spawn(|| volume.set_db(-30.0));
            assert_eq!(operation.join().unwrap().unwrap().db, -30.0);
            assert!(matches!(
                dac.negotiate(&StreamRequest::pcm(48_000, 2, 16)),
                Err(Error::Busy)
            ));
        });
        assert_eq!(fake.raw(), vec![-30 * 256, -30 * 256]);
        dac.shared.shutdown(Some(CloseVolume::Keep)).unwrap();
        assert!(dac.shared.reconfigure.lock().is_none());
        drop(reservation);
        assert!(dac.shared.reconfigure.lock().is_none());
    }

    #[test]
    fn reconfigure_reservation_drop_releases_busy_state() {
        let fake = transport::fake_dac::Registered::new(false, true);
        let dac = fake_dac_handle(DacConfig::default());
        let token = Arc::new(());
        *dac.shared.reconfigure.lock() = Some(token.clone());
        let reservation = ReconfigureReservation {
            shared: &dac.shared,
            token,
            terminal_on_drop: false,
        };
        assert!(matches!(dac.shared.reject_reconfigure(), Err(Error::Busy)));
        drop(reservation);
        assert!(dac.shared.reject_reconfigure().is_ok());
        dac.close().unwrap();
        drop(fake);
    }

    #[test]
    fn terminal_reconfigure_reservation_fences_until_close() {
        let fake = transport::fake_dac::Registered::new(false, true);
        let dac = fake_dac_handle(DacConfig::default());
        let token = Arc::new(());
        *dac.shared.reconfigure.lock() = Some(token.clone());
        let reservation = ReconfigureReservation {
            shared: &dac.shared,
            token,
            terminal_on_drop: true,
        };
        drop(reservation);
        assert!(matches!(
            dac.shared.reject_reconfigure(),
            Err(Error::ReconfigureUnusable(_))
        ));
        assert!(matches!(
            dac.negotiate(&StreamRequest::pcm(48_000, 2, 16)),
            Err(Error::ReconfigureUnusable(_))
        ));
        dac.close().unwrap();
        drop(fake);
    }

    #[test]
    fn reconfigure_coordinator_tries_only_certified_refusals_and_rolls_back_once() {
        let old = StreamRequest::pcm(48_000, 2, 32);
        let requests = [
            StreamRequest::pcm(48_000, 2, 16),
            StreamRequest::pcm(48_000, 2, 24),
        ];
        let mut calls = Vec::new();
        let (value, cause) = reconfigure_candidates_or_restore(&requests, &old, |request| {
            calls.push(request.bit_depth);
            if request.bit_depth == 16 {
                Err(CandidateFailure::Clean(Error::Control(
                    "unavailable".into(),
                )))
            } else {
                Ok(request.bit_depth)
            }
        })
        .unwrap();
        assert_eq!(value, 24);
        assert!(cause.is_none());
        assert_eq!(calls, vec![16, 24]);
        calls.clear();
        let (_, cause) = reconfigure_candidates_or_restore(&requests, &old, |request| {
            calls.push(request.bit_depth);
            if request.bit_depth == 32 {
                Ok(())
            } else {
                Err(CandidateFailure::Clean(Error::Control(
                    "unavailable".into(),
                )))
            }
        })
        .unwrap();
        assert!(cause.is_some());
        assert_eq!(calls, vec![16, 24, 32]);
        calls.clear();
        let result: Result<((), Option<Error>)> =
            reconfigure_candidates_or_restore(&requests, &old, |request| {
                calls.push(request.bit_depth);
                Err(CandidateFailure::Terminal(Error::Control(
                    "unknown SET".into(),
                )))
            });
        assert!(matches!(result, Err(Error::ReconfigureUnusable(_))));
        assert_eq!(calls, vec![16]);
    }

    #[test]
    fn widening_candidates_must_be_bounded_ordered_and_same_source_shape() {
        let a = StreamRequest::pcm(48_000, 2, 16);
        let b = StreamRequest::pcm(48_000, 2, 24);
        let c = StreamRequest::pcm(48_000, 2, 32);
        assert!(validate_reconfigure_candidates(&[a, b, c]).is_ok());
        assert!(validate_reconfigure_candidates(&[]).is_err());
        assert!(validate_reconfigure_candidates(&[b, a]).is_err());
        assert!(validate_reconfigure_candidates(&[a, StreamRequest::pcm(96_000, 2, 24)]).is_err());
        assert!(validate_reconfigure_candidates(&[a, StreamRequest::pcm(48_000, 1, 24)]).is_err());
        assert!(
            validate_reconfigure_candidates(&[a, StreamRequest::pcm_float32(48_000, 2)]).is_err()
        );
    }

    #[test]
    fn reconfigure_transaction_attempts_requested_then_previous_and_reports_both_failures() {
        let old = StreamRequest::pcm(44_100, 2, 16);
        let new = StreamRequest::pcm_float32(96_000, 2);
        let mut calls = Vec::new();
        let (restored, cause) = reconfigure_or_restore(&new, &old, |request| {
            calls.push(*request);
            if *request == new {
                Err(Error::Unsupported {
                    reason: "no float alternate".into(),
                    offered: vec![],
                })
            } else {
                Ok("restored paused stream")
            }
        })
        .unwrap();
        assert_eq!(calls, vec![new, old]);
        assert_eq!(restored, "restored paused stream");
        assert_eq!(cause.unwrap().code(), "unsupported");
        let error = reconfigure_or_restore::<()>(&new, &old, |_| {
            Err(Error::Control("clock failed".into()))
        })
        .unwrap_err();
        assert_eq!(error.code(), "reconfigure_unusable");
        let mut calls = Vec::new();
        let (_, cause) = reconfigure_or_restore(&new, &old, |request| {
            calls.push(*request);
            Ok(())
        })
        .unwrap();
        assert_eq!(calls, vec![new]);
        assert!(cause.is_none());
    }
}
