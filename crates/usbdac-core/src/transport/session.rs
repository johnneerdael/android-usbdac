use super::context::{ScopedBool, ScopedMutex, ScopedU32, ScopedU64};
use super::*;

pub(super) static SESSION_STATE: ScopedMutex<Option<SessionState>> =
    ScopedMutex::new(|context| context.session.clone());
pub(super) static IDLE_LOCK: ScopedMutex<Option<IdleLock>> =
    ScopedMutex::new(|context| context.idle.clone());
pub(super) static LIFECYCLE_STATE: ScopedMutex<LifecycleState> =
    ScopedMutex::new(|context| context.lifecycle.clone());
/// Serializes Feature Unit I/O with other volume calls and final device clear.
pub(super) static HARDWARE_VOLUME_MUTEX: ScopedMutex<()> =
    ScopedMutex::new(|context| context.volume_io.clone());
pub(super) static DISCOVERY_DISABLED: Once = Once::new();
pub(super) static USB_SESSION_CLEAR_PENDING: ScopedBool =
    ScopedBool::new(|context| &context.clear_pending);
pub(super) static HARDWARE_VOLUME_GENERATION: ScopedU64 =
    ScopedU64::new(|context| &context.volume_generation);
/// The DAC's volume before this library first changed it. Outside SESSION_STATE
/// so it outlives stream restarts and a `Dac` that is closed with
/// `HandBack::Keep` and reopened.
pub(super) static ORIGINAL_HARDWARE_VOLUME: ScopedMutex<OriginalVolumeSlot<HardwareVolumeControl>> =
    ScopedMutex::new(|context| context.original_volume.clone());
/// What the next device clear does with that level; set by `Dac::close` just
/// before it.
pub(super) static ORIGINAL_VOLUME_ACTION: ScopedU32 =
    ScopedU32::new(|context| &context.original_volume_action);
pub(super) const ORIGINAL_VOLUME_KEEP: u32 = 0;
pub(super) const ORIGINAL_VOLUME_RESTORE: u32 = 1;
pub(super) const ORIGINAL_VOLUME_FORGET: u32 = 2;

/// Only one negotiation or output session can use the claimed interfaces.
/// Serialize ownership transitions with device clear before the host releases them.
pub(super) static USB_SESSION_ACTIVE: ScopedBool = ScopedBool::new(|context| &context.active);
pub(super) static USB_SESSION_GATE: ScopedMutex<()> =
    ScopedMutex::new(|context| context.gate.clone());

pub(super) struct SessionGuard {
    context: Arc<context::SessionContext>,
}

impl SessionGuard {
    // Caller holds USB_SESSION_GATE and has checked that no session is active.
    pub(super) fn start_locked() -> Self {
        USB_STOP_REQUESTED.store(false, Ordering::SeqCst);
        USB_SESSION_ACTIVE.store(true, Ordering::SeqCst);
        Self {
            context: context::current(),
        }
    }
}

impl Drop for SessionGuard {
    fn drop(&mut self) {
        let _scope = self.context.enter();
        let _session_gate = USB_SESSION_GATE.lock();
        USB_SESSION_ACTIVE.store(false, Ordering::SeqCst);
        complete_pending_clear_locked();
    }
}

// Module-level stop signal. `clear_device()` has no handle to the stream's local
// `stop` flag, so without this the transfer loop would keep running until the
// stream is closed or the USB fd dies. Set from the deferred-clear path; reset
// at session start. A second clear during teardown is harmless (idempotent).
pub(super) static USB_STOP_REQUESTED: ScopedBool = ScopedBool::new(|context| &context.stop);

/// Combined stop predicate for the render/output loops: bail when either the
/// backend's local `stop` Arc or the global clear-requested flag is set.
pub(super) fn usb_stop_requested(stop: &AtomicBool) -> bool {
    stop.load(Ordering::Acquire) || USB_STOP_REQUESTED.load(Ordering::Acquire)
}

pub(super) fn payload_audit_latch() -> PayloadAuditLatch {
    PayloadAuditLatch::new(Duration::from_millis(USB_PAYLOAD_FLICKER_GRACE_MS))
}

pub(super) static USB_DSD_PAYLOAD_LATCH: ScopedMutex<PayloadAuditLatch> =
    ScopedMutex::new(|context| context.dsd_audit.clone());

pub(super) fn usb_dsd_payload_verified() -> bool {
    USB_DSD_PAYLOAD_LATCH.lock().is_verified()
}

/// Tests that read or write the process-global session, settings or payload
/// audit latches take this first, so they cannot observe each other's state.
#[cfg(test)]
pub(crate) static PAYLOAD_LATCH_TEST_LOCK: StdMutex<()> = StdMutex::new(());

pub(super) static USB_PCM_PAYLOAD_LATCH: ScopedMutex<PayloadAuditLatch> =
    ScopedMutex::new(|context| context.pcm_audit.clone());

pub(super) fn usb_pcm_payload_verified() -> bool {
    USB_PCM_PAYLOAD_LATCH.lock().is_verified()
}

/// Diagnostic line for a state transition (not for per-transfer chatter).
pub(super) fn log_usb_event(message: String) {
    log_info!("{}", message);
}

pub(super) fn observe_dsd_payload_audit(ok: bool) {
    let now = Instant::now();
    let mut latch = USB_DSD_PAYLOAD_LATCH.lock();
    if let Some(verified) = latch.observe(ok, now) {
        let constant_for = latch.constant_for(now);
        drop(latch);
        log_payload_audit_transition("DSD", verified, constant_for);
    }
}

pub(super) fn observe_pcm_payload_audit(ok: bool) {
    let now = Instant::now();
    let mut latch = USB_PCM_PAYLOAD_LATCH.lock();
    if let Some(verified) = latch.observe(ok, now) {
        let constant_for = latch.constant_for(now);
        drop(latch);
        log_payload_audit_transition("PCM", verified, constant_for);
    }
}

pub(super) fn log_payload_audit_transition(
    transport: &str,
    verified: bool,
    constant_for: Option<Duration>,
) {
    if verified {
        log_usb_event(format!(
            "[USB-{transport}] wire payload verified: real audio flowing, not a stuck/dead pipeline"
        ));
    } else {
        let constant_ms = constant_for
            .map(|duration| duration.as_millis())
            .unwrap_or(0);
        log_usb_event(format!(
            "[USB-{transport}] wire payload lost: audit failed for {constant_ms}ms (constant/silent transfer)"
        ));
    }
}

pub(super) static LAST_BIT_PERFECT_LOGGED: ScopedBool =
    ScopedBool::new(|context| &context.last_bit_perfect);

/// Transition evidence line for the bit-perfect claim. Both edges are logged
/// unconditionally so field reports can show mode flapping without Developer
/// Mode.
pub(super) fn log_bit_perfect_transition(verified: bool, state: &SessionState) {
    let prev = LAST_BIT_PERFECT_LOGGED.swap(verified, Ordering::Relaxed);
    if prev == verified {
        return;
    }
    let Some(effective) = state.playback_format else {
        return;
    };
    let transport = match effective.dsd_transport {
        DsdTransportMode::DoP => "dop",
        DsdTransportMode::Native => "dsd-native",
        DsdTransportMode::None => "pcm",
    };
    if verified {
        log_usb_event(format!(
            "[USB-BITPERFECT] verified: {} bit/{} Hz {}ch {} direct on \"{}\" — no mixer, no resample, no DSP",
            effective.bit_depth,
            effective.sample_rate,
            effective.channels,
            transport,
            state.device.product_name
        ));
    } else {
        log_usb_event(format!(
            "[USB-BITPERFECT] released: {} bit/{} Hz {}ch {} direct on \"{}\" — {}",
            effective.bit_depth,
            effective.sample_rate,
            effective.channels,
            transport,
            state.device.product_name,
            bit_perfect_release_reason(state)
        ));
    }
}

pub(super) fn bit_perfect_release_reason(state: &SessionState) -> &'static str {
    match bit_perfect_blocker(state) {
        Some(blocker) => blocker.description(),
        None => "wire payload audit failed",
    }
}

/// The bit-perfect verdict for the open stream, logging the transition. False
/// when no stream is open.
pub(crate) fn bit_perfect_now() -> bool {
    let mut guard = SESSION_STATE.lock();
    let Some(state) = guard.as_mut() else {
        return false;
    };
    let verified = direct_path_is_bit_perfect(state);
    state.bit_perfect_verified = verified;
    log_bit_perfect_transition(verified, state);
    verified
}

pub(crate) fn is_usb_session_active() -> bool {
    USB_SESSION_ACTIVE.load(Ordering::SeqCst)
}

/// Whether a DAC is registered (open) in this process.
pub(crate) fn session_registered() -> bool {
    SESSION_STATE.lock().is_some()
}

pub(super) fn clear_session_state() {
    // Finish any in-progress Feature Unit I/O before the host releases its claims.
    let _volume_hold = HARDWARE_VOLUME_MUTEX.lock();
    routing_hw::finish_routing_hand_back();
    finish_original_hardware_volume();
    finish_requested_clock_route();
    release_idle_lock();
    if LIFECYCLE_STATE.lock().engine_state != EngineState::Fallback {
        set_engine_state(EngineState::Idle, None);
    }
    *SESSION_STATE.lock() = None;
    adc3::clear_adc3_negotiation();
    crate::settings::reset();
    context::current().release();
}

// The caller holds USB_SESSION_GATE so the wait API cannot report success while
// a clear is still dropping the idle handle and its device registration.
pub(super) fn complete_pending_clear_locked() {
    if USB_SESSION_ACTIVE.load(Ordering::SeqCst) {
        return;
    }
    if !USB_SESSION_CLEAR_PENDING.swap(false, Ordering::SeqCst) {
        return;
    }

    dev_eprintln!("USB direct: clearing registration after USB I/O stopped");
    clear_session_state();
}

/// Registers the device: checks the host's claims, reads the descriptors and
/// takes the idle handle. Fails with `Busy` while another DAC is registered or
/// a previous session is still stopping.
pub(crate) fn register_device(device: UsbDevice) -> Result<(), crate::Error> {
    register_device_mode(device, RegistrationMode::Playback)
}

/// Explicit IN registration bypasses output discovery and output volume setup.
/// Its host claim handoff is checked by the dedicated capture validator.
pub(super) fn register_capture_device(
    device: UsbDevice,
    configuration: Option<u8>,
) -> Result<(), crate::Error> {
    register_device_mode(device, RegistrationMode::Capture { configuration })
}

pub(super) fn register_duplex_device(
    device: UsbDevice,
    configuration: Option<u8>,
) -> Result<(), crate::Error> {
    register_device_mode(device, RegistrationMode::Duplex { configuration })
}

enum RegistrationMode {
    Playback,
    Capture { configuration: Option<u8> },
    Duplex { configuration: Option<u8> },
}

fn register_device_mode(device: UsbDevice, mode: RegistrationMode) -> Result<(), crate::Error> {
    use crate::Error;
    let _session_gate = USB_SESSION_GATE.lock();
    complete_pending_clear_locked();
    if is_usb_session_active() || SESSION_STATE.lock().is_some() {
        return Err(Error::Busy);
    }
    let capability_model = match mode {
        RegistrationMode::Capture { configuration } => {
            capture_stream::validate_capture_registration(&device, configuration)
                .map_err(Error::Claim)?;
            None
        }
        RegistrationMode::Playback => {
            Some(inspect_usb_capabilities(&device).map_err(Error::Claim)?)
        }
        RegistrationMode::Duplex { configuration } => {
            Some(inspect_duplex_capabilities(&device, configuration).map_err(Error::Claim)?)
        }
    };
    if capability_model
        .as_ref()
        .is_some_and(|model| model.alt_settings.is_empty())
    {
        return Err(Error::Unsupported {
            reason: "the device has no isochronous OUT audio streaming interface".to_string(),
            offered: Vec::new(),
        });
    }
    let mut guard = SESSION_STATE.lock();
    release_idle_lock();
    set_engine_state(EngineState::Idle, None);
    *guard = Some(SessionState {
        device,
        requested_playback_format: None,
        playback_format: None,
        negotiated_uac2_alternate: None,
        lock_requested: true,
        stream_active: false,
        capability_model,
        clock_status: None,
        clock_control_attempted: false,
        clock_control_succeeded: false,
        clock_verification_passed: false,
        dac_mode: DacMode::FullControl,
        last_error: None,
        direct_mode_refusal_reason: None,
        usb_stream_stable: false,
        bit_perfect_verified: false,
        software_volume_active: false,
        unsynchronized_async: false,
        hardware_volume_control: None,
        hardware_volume_generation: HARDWARE_VOLUME_GENERATION.fetch_add(1, Ordering::Relaxed),
        hardware_volume_error: None,
        hardware_volume_state: None,
        startup_volume: None,
        runtime_stats: None,
        service_interval_feedback_routes: Default::default(),
        unreadable_clocks: Default::default(),
    });
    drop(guard);
    if let Err(error) = ensure_idle_lock() {
        *SESSION_STATE.lock() = None;
        return Err(Error::Claim(error));
    }
    Ok(())
}

/// Stops the session now, or as soon as the output thread has finished, and
/// drops the registration.
pub(crate) fn clear_device() {
    let _session_gate = USB_SESSION_GATE.lock();
    USB_SESSION_CLEAR_PENDING.store(true, Ordering::SeqCst);
    if is_usb_session_active() {
        // Signal the running transfer loop to drain before the host releases
        // the interfaces and lets the platform driver rebind.
        USB_STOP_REQUESTED.store(true, Ordering::SeqCst);
        dev_eprintln!(
            "USB direct: clear requested while a session is active; stopping the transfer loop"
        );
        return;
    }

    complete_pending_clear_locked();
}

/// Waits until the output thread has stopped and the registration is gone.
pub(crate) fn wait_for_session_stop(timeout: Duration) -> bool {
    let started_at = Instant::now();
    loop {
        if let Some(_session_gate) = USB_SESSION_GATE.try_lock() {
            if !is_usb_session_active() {
                complete_pending_clear_locked();
                return true;
            }
        }
        if started_at.elapsed() >= timeout {
            return false;
        }
        thread::sleep(Duration::from_millis(10));
    }
}

pub(super) fn set_last_error(error: Option<String>) {
    if let Some(state) = SESSION_STATE.lock().as_mut() {
        state.last_error = error;
    }
}

pub(super) fn set_direct_mode_refusal_reason(reason: Option<String>) {
    if let Some(state) = SESSION_STATE.lock().as_mut() {
        state.direct_mode_refusal_reason = reason;
    }
}

pub(super) fn set_usb_stream_stable(stable: bool) {
    if let Some(state) = SESSION_STATE.lock().as_mut() {
        state.usb_stream_stable = stable;
    }
}

pub(super) fn set_unsynchronized_async(active: bool) {
    if let Some(state) = SESSION_STATE.lock().as_mut() {
        state.unsynchronized_async = active;
    }
}

pub(super) fn set_software_volume_active(active: bool) {
    if let Some(state) = SESSION_STATE.lock().as_mut() {
        state.software_volume_active = active;
        state.bit_perfect_verified = direct_path_is_bit_perfect(state);
    }
}

pub(super) fn set_hardware_volume_error(error: Option<String>) {
    if let Some(state) = SESSION_STATE.lock().as_mut() {
        state.hardware_volume_error = error;
    }
}

pub(super) fn publish_hardware_volume_snapshot(
    generation: u64,
    volume: crate::volume::VolumeState,
) -> Result<(), String> {
    let mut guard = SESSION_STATE.lock();
    let state = guard
        .as_mut()
        .filter(|state| state.hardware_volume_generation == generation)
        .ok_or_else(|| "USB volume route changed during control transfer".to_string())?;
    state.hardware_volume_state = Some(volume);
    state.hardware_volume_error = None;
    Ok(())
}

pub(super) fn set_runtime_stats(runtime_stats: Option<Arc<RuntimeStats>>) {
    if let Some(state) = SESSION_STATE.lock().as_mut() {
        state.runtime_stats = runtime_stats;
    }
}

pub(super) fn set_capability_model(capability_model: Option<CapabilityModel>) {
    if let Some(state) = SESSION_STATE.lock().as_mut() {
        state.capability_model = capability_model;
    }
}

pub(super) fn engine_state_label(state: EngineState) -> &'static str {
    match state {
        EngineState::Idle => "idle",
        EngineState::UsbInit => "usbInit",
        EngineState::UsbReady => "usbReady",
        EngineState::Streaming => "streaming",
        EngineState::Error => "error",
        EngineState::Fallback => "fallback",
    }
}

pub(super) fn set_engine_state(state: EngineState, reason: Option<String>) {
    let mut lifecycle_state = LIFECYCLE_STATE.lock();
    let previous = lifecycle_state.engine_state;
    lifecycle_state.engine_state = state;
    lifecycle_state.reason = reason;
    if previous != state {
        dev_eprintln!("USB direct engine state: {:?} -> {:?}", previous, state);
    }
    dev_eprintln!(
        "[USB][STATE] {}{}",
        engine_state_label(state),
        lifecycle_state
            .reason
            .as_ref()
            .map(|reason| format!(" ({})", reason))
            .unwrap_or_default()
    );
}

pub(super) fn set_clock_status(clock_status: Option<ClockStatus>) {
    if let Some(state) = SESSION_STATE.lock().as_mut() {
        state.clock_status = clock_status;
    }
}

pub(super) fn set_clock_verification(
    attempted: bool,
    succeeded: bool,
    passed: bool,
    reported_sample_rate: Option<u32>,
) {
    if let Some(state) = SESSION_STATE.lock().as_mut() {
        state.clock_control_attempted = attempted;
        state.clock_control_succeeded = succeeded;
        state.clock_verification_passed = passed;
        if let Some(clock_status) = state.clock_status.as_mut() {
            clock_status.reported_sample_rate = reported_sample_rate;
        }
        state.bit_perfect_verified = direct_path_is_bit_perfect(state);
    }
}

pub(super) fn invalidate_clock_verification() {
    if let Some(state) = SESSION_STATE.lock().as_mut() {
        state.clock_verification_passed = false;
        if let Some(clock_status) = state.clock_status.as_mut() {
            clock_status.reported_sample_rate = None;
        }
        state.bit_perfect_verified = direct_path_is_bit_perfect(state);
    }
}

/// What stands in the way of a bit-perfect claim, if anything. The order is the
/// order of precedence of the reported reason.
pub(super) fn bit_perfect_blocker(state: &SessionState) -> Option<BitPerfectBlocker> {
    if !state.stream_active
        || state.runtime_stats.as_ref().is_some_and(|stats| {
            stats.failed.load(Ordering::Acquire) || stats.closed.load(Ordering::Acquire)
        })
    {
        return Some(BitPerfectBlocker::NoStream);
    }

    // Audio that was queued but never reached the DAC cannot be re-sent, so a
    // stream that lost any can no longer claim bit-perfect output. The latch
    // lives in the per-stream runtime stats and clears only with a new stream.
    if state
        .runtime_stats
        .as_ref()
        .is_some_and(|stats| stats.transport_loss_latched())
    {
        return Some(BitPerfectBlocker::LostFrames);
    }

    let (Some(requested), Some(effective)) =
        (state.requested_playback_format, state.playback_format)
    else {
        return Some(BitPerfectBlocker::NoStream);
    };

    // Bit-perfect requires: the format the host asked for (no rate change), a
    // verified clock, no software gain, an endpoint the DAC paces through its own
    // feedback, no lost audio (checked above), and a live wire payload that
    // passed the audit (the PCM payload audit, or for DSD the DoP-marker /
    // native-byte audit).
    if requested.sample_rate != effective.sample_rate
        || requested.bit_depth != effective.bit_depth
        || requested.channels != effective.channels
    {
        return Some(BitPerfectBlocker::FormatChanged);
    }
    if state.runtime_stats.as_ref().is_some_and(|stats| {
        (stats.implicit_feedback.load(Ordering::Acquire)
            || stats.feedback_required.load(Ordering::Acquire))
            && !stats.feedback_locked.load(Ordering::Acquire)
    }) {
        return Some(BitPerfectBlocker::FeedbackUnlocked);
    }
    if !state.clock_verification_passed {
        return Some(BitPerfectBlocker::ClockUnverified);
    }
    if state.software_volume_active {
        return Some(BitPerfectBlocker::SoftwareGain);
    }
    if state.unsynchronized_async {
        return Some(BitPerfectBlocker::UnsynchronizedAsync);
    }
    let payload_ok = match effective.dsd_transport {
        DsdTransportMode::DoP | DsdTransportMode::Native => usb_dsd_payload_verified(),
        DsdTransportMode::None => usb_pcm_payload_verified(),
    };
    (!payload_ok).then_some(BitPerfectBlocker::PayloadUnverified)
}

/// What the open stream says besides the bit-perfect verdict, for
/// `DacStream::stats`.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct StreamVerdicts {
    /// Lossless transfers for long enough, with no underrun and no lost audio.
    pub(crate) stable: bool,
    /// The DAC confirmed the sampling frequency (or the device quirk trusts it).
    pub(crate) clock_verified: bool,
    /// The wire payload audit passes.
    pub(crate) payload_verified: bool,
    /// How long the wire payload has been failing the audit, if it is.
    pub(crate) payload_constant_for_ms: Option<u64>,
}

/// The verdicts of the open stream; all false when none is open.
pub(crate) fn stream_verdicts() -> StreamVerdicts {
    let Some((stable, clock_verified, dsd)) = SESSION_STATE.lock().as_ref().map(|state| {
        (
            state.usb_stream_stable,
            state.clock_verification_passed,
            state
                .playback_format
                .is_some_and(|format| format.dsd_transport != DsdTransportMode::None),
        )
    }) else {
        return StreamVerdicts::default();
    };
    let now = Instant::now();
    let (payload_verified, constant_for) = if dsd {
        let latch = USB_DSD_PAYLOAD_LATCH.lock();
        (latch.is_verified(), latch.constant_for(now))
    } else {
        let latch = USB_PCM_PAYLOAD_LATCH.lock();
        (latch.is_verified(), latch.constant_for(now))
    };
    StreamVerdicts {
        stable,
        clock_verified,
        payload_verified,
        payload_constant_for_ms: constant_for.map(|duration| duration.as_millis() as u64),
    }
}

pub(super) fn direct_path_is_bit_perfect(state: &SessionState) -> bool {
    bit_perfect_blocker(state).is_none()
}

/// The blocker for the open stream; `NoStream` when none is open.
pub(crate) fn current_bit_perfect_blocker() -> Option<BitPerfectBlocker> {
    match SESSION_STATE.lock().as_ref() {
        Some(state) => bit_perfect_blocker(state),
        None => Some(BitPerfectBlocker::NoStream),
    }
}

pub(super) fn set_effective_playback_format(mut playback_format: PlaybackFormat) {
    // Never store 0 Hz — buggy DACs (Fosi DS2 / Savitech) report 0 via
    // GET_CUR even after a successful SET_CUR. If the requested format has
    // a valid rate, use it instead. This stops the 0 from poisoning every
    // downstream consumer (validate_android_direct_request, format
    // selection, stream rate resolution, position calculation).
    if playback_format.sample_rate == 0 {
        if let Some(state) = SESSION_STATE.lock().as_ref() {
            if let Some(req) = state.requested_playback_format {
                if req.sample_rate > 0 {
                    dev_eprintln!(
                        "[USB] set_effective_playback_format: refusing to store 0 Hz, using requested {} Hz instead",
                        req.sample_rate
                    );
                    playback_format.sample_rate = req.sample_rate;
                }
            }
        }
    }
    if let Some(state) = SESSION_STATE.lock().as_mut() {
        state.playback_format = Some(playback_format);
    }
}

pub(super) fn set_dac_mode(mode: DacMode) {
    if let Some(state) = SESSION_STATE.lock().as_mut() {
        state.dac_mode = mode;
    }
}

pub(super) fn set_stream_active(active: bool) {
    // Read before the state lock is taken: the two are never held together.
    let idle_lock_held = IDLE_LOCK.lock().is_some();
    if let Some(state) = SESSION_STATE.lock().as_mut() {
        state.set_stream_active(active, idle_lock_held);
    }
}

impl SessionState {
    /// Records the stream starting or stopping.
    ///
    /// The DAC's volume control belongs to its AudioControl interface, not to the
    /// isochronous stream: its requests are synchronous control transfers on the
    /// registered device. While the idle lock keeps the DAC claimed after the
    /// stream stops (a track change, a stream closed and not yet reopened), the
    /// control, its ranges and the cached level and mute therefore stay valid,
    /// and so does a write that was in flight. The generation is not bumped
    /// either: the route has not changed, and the next stream's discovery bumps
    /// it when it finds a different control.
    ///
    /// Without the idle lock nothing keeps the platform's USB audio driver off
    /// the DAC once the stream has stopped, so the control and what was read
    /// through it are dropped, and writes in flight against it fail as a changed
    /// route.
    pub(super) fn set_stream_active(&mut self, active: bool, idle_lock_held: bool) {
        self.stream_active = active;
        if !active {
            self.unsynchronized_async = false;
        }
        if !active && !idle_lock_held {
            self.hardware_volume_control = None;
            self.hardware_volume_state = None;
            self.hardware_volume_generation =
                HARDWARE_VOLUME_GENERATION.fetch_add(1, Ordering::Relaxed);
        }
    }
}

pub(super) fn release_idle_lock() {
    // The host owns the claims and releases them after the USB session stops.
    IDLE_LOCK.lock().take();
}

pub(super) fn ensure_idle_lock() -> Result<(), String> {
    if IDLE_LOCK.lock().is_some() {
        return Ok(());
    }

    let state = SESSION_STATE
        .lock()
        .as_ref()
        .cloned()
        .ok_or_else(|| "no DAC is open".to_string())?;
    let claimed_handle = open_claimed_usb_handle(&state.device).inspect_err(|error| {
        set_last_error(Some(error.clone()));
        set_direct_mode_refusal_reason(Some(error.clone()));
        set_engine_state(EngineState::Error, Some(error.clone()));
    })?;

    dev_eprintln!(
        "USB direct idle lock using the host's claims for '{}' interfaces {:?}",
        state.device.product_name,
        claimed_handle.claimed_interfaces
    );

    *IDLE_LOCK.lock() = Some(claimed_handle.into_idle_lock());
    Ok(())
}

/// A registered session without a USB device behind it, for host tests.
#[cfg(test)]
pub(crate) struct TestSession;

#[cfg(test)]
impl TestSession {
    /// Registers a fake DAC whose state says "streaming, verified clock".
    pub(crate) fn install() -> Self {
        *SESSION_STATE.lock() = Some(test_state(None));
        Self
    }

    /// Like `install`, with the stream's counters attached.
    pub(crate) fn install_streaming(stats: Arc<RuntimeStats>) -> Self {
        *SESSION_STATE.lock() = Some(test_state(Some(stats)));
        Self
    }
}

#[cfg(test)]
impl Drop for TestSession {
    fn drop(&mut self) {
        *SESSION_STATE.lock() = None;
    }
}

#[cfg(test)]
pub(crate) fn test_pcm_format() -> PlaybackFormat {
    PlaybackFormat {
        pcm_encoding: crate::transport::PcmEncoding::Signed,
        sample_rate: 44_100,
        bit_depth: 16,
        channels: 2,
        is_dop: false,
        dsd_transport: DsdTransportMode::None,
        dsd_bit_rate: 0,
    }
}

#[cfg(test)]
pub(super) fn test_state(runtime_stats: Option<Arc<RuntimeStats>>) -> SessionState {
    SessionState {
        device: UsbDevice {
            fd: -1,
            _fd_owner: Arc::new(OwnedUsbFd(-1)),
            claimed_interfaces: Arc::from(Vec::<u8>::new()),
            vendor_id: 0,
            product_id: 0,
            product_name: "test DAC".to_string(),
            device_name: None,
        },
        requested_playback_format: Some(test_pcm_format()),
        playback_format: Some(test_pcm_format()),
        negotiated_uac2_alternate: None,
        lock_requested: true,
        stream_active: true,
        capability_model: None,
        clock_status: None,
        clock_control_attempted: true,
        clock_control_succeeded: true,
        clock_verification_passed: true,
        dac_mode: DacMode::FullControl,
        last_error: None,
        direct_mode_refusal_reason: None,
        usb_stream_stable: false,
        bit_perfect_verified: false,
        software_volume_active: false,
        unsynchronized_async: false,
        hardware_volume_control: None,
        hardware_volume_generation: 0,
        hardware_volume_error: None,
        hardware_volume_state: None,
        startup_volume: None,
        runtime_stats,
        service_interval_feedback_routes: Default::default(),
        unreadable_clocks: Default::default(),
    }
}
