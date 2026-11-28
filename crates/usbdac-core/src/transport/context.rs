//! Explicit device ownership, scoped across facade calls and worker lifetimes.
use super::*;
use parking_lot::{ArcMutexGuard, RawMutex, RwLock};
use std::{cell::RefCell, collections::HashMap, marker::PhantomData, rc::Rc, sync::Weak};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct AttachmentIdentity {
    backend: u8,
    device: u128,
    inode: u128,
}
type RememberedVolume = Arc<Mutex<OriginalVolumeSlot<HardwareVolumeControl>>>;
#[derive(Default)]
struct Registry {
    owners: HashMap<AttachmentIdentity, Weak<SessionContext>>,
    remembered: HashMap<AttachmentIdentity, RememberedVolume>,
}
static REGISTRY: Lazy<Mutex<Registry>> = Lazy::new(|| Mutex::new(Registry::default()));

pub(crate) struct SessionContext {
    identity: Mutex<Option<AttachmentIdentity>>,
    pub(super) session: Arc<Mutex<Option<SessionState>>>,
    pub(super) idle: Arc<Mutex<Option<IdleLock>>>,
    pub(super) lifecycle: Arc<Mutex<LifecycleState>>,
    pub(super) volume_io: Arc<Mutex<()>>,
    pub(super) clear_pending: AtomicBool,
    pub(super) volume_generation: AtomicU64,
    pub(super) original_volume: Arc<Mutex<OriginalVolumeSlot<HardwareVolumeControl>>>,
    pub(super) original_volume_action: AtomicU32,
    keep_original: AtomicBool,
    pub(super) active: AtomicBool,
    pub(super) gate: Arc<Mutex<()>>,
    pub(super) stop: AtomicBool,
    pub(super) dsd_audit: Arc<Mutex<PayloadAuditLatch>>,
    pub(super) pcm_audit: Arc<Mutex<PayloadAuditLatch>>,
    pub(super) last_bit_perfect: AtomicBool,
    pub(crate) settings: Arc<RwLock<Arc<crate::settings::Settings>>>,
    pub(super) clock_route: Arc<Mutex<Option<clock::SelectedClockRoute>>>,
    pub(super) adc3_route: Arc<Mutex<Option<adc3::Adc3Route>>>,
    pub(super) scarlett: Arc<Mutex<Option<routing_hw::RecordedScarlett>>>,
    pub(super) gen1: Arc<Mutex<Option<routing_hw::RecordedGen1>>>,
    pub(super) fcp: Arc<Mutex<Option<routing_hw::RecordedFcp>>>,
    pub(super) routing: Arc<Mutex<Option<routing_hw::RecordedRouting>>>,
    pub(crate) routing_hand_back: AtomicU32,
    pub(super) routing_report: Arc<Mutex<Option<routing_hw::RoutingHandBackReport>>>,
}
impl SessionContext {
    #[cfg(test)]
    pub(crate) fn unregistered() -> Arc<Self> {
        Self::with_original(None)
    }
    fn with_original(original: Option<RememberedVolume>) -> Arc<Self> {
        Arc::new(Self {
            identity: Mutex::new(None),
            session: Arc::new(Mutex::new(None)),
            idle: Arc::new(Mutex::new(None)),
            lifecycle: Arc::new(Mutex::new(LifecycleState::default())),
            volume_io: Arc::new(Mutex::new(())),
            clear_pending: AtomicBool::new(false),
            volume_generation: AtomicU64::new(0),
            original_volume: original
                .unwrap_or_else(|| Arc::new(Mutex::new(OriginalVolumeSlot::new()))),
            original_volume_action: AtomicU32::new(0),
            keep_original: AtomicBool::new(false),
            active: AtomicBool::new(false),
            gate: Arc::new(Mutex::new(())),
            stop: AtomicBool::new(false),
            dsd_audit: Arc::new(Mutex::new(payload_audit_latch())),
            pcm_audit: Arc::new(Mutex::new(payload_audit_latch())),
            last_bit_perfect: AtomicBool::new(false),
            settings: Arc::new(RwLock::new(Arc::new(crate::settings::Settings::default()))),
            clock_route: Arc::new(Mutex::new(None)),
            adc3_route: Arc::new(Mutex::new(None)),
            scarlett: Arc::new(Mutex::new(None)),
            gen1: Arc::new(Mutex::new(None)),
            fcp: Arc::new(Mutex::new(None)),
            routing: Arc::new(Mutex::new(None)),
            routing_hand_back: AtomicU32::new(0),
            routing_report: Arc::new(Mutex::new(None)),
        })
    }
    pub(crate) fn reserve(fd: RawFd) -> Result<Arc<Self>, Error> {
        let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
        if unsafe { libc::fstat(fd, stat.as_mut_ptr()) } != 0 {
            return Err(Error::Claim(std::io::Error::last_os_error().to_string()));
        }
        // SAFETY: successful fstat initialized the entire stat structure.
        let stat = unsafe { stat.assume_init() };
        let identity = AttachmentIdentity {
            backend: 0,
            device: stat.st_dev as u128,
            inode: stat.st_ino as u128,
        };
        Self::reserve_identity(identity)
    }
    #[cfg(target_os = "macos")]
    pub(crate) fn reserve_darwin(device: &Device<Context>) -> Result<Arc<Self>, Error> {
        // Bus, address, and physical port chain identify the live attachment
        // across separate libusb contexts. Retain the libusb device separately
        // for all I/O; never reopen by this key.
        let ports = device
            .port_numbers()
            .map_err(|error| Error::Claim(format!("USB port path: {error}")))?;
        let mut topology = 0u128;
        for port in ports.iter().copied() {
            topology = (topology << 8) | u128::from(port);
        }
        Self::reserve_identity(AttachmentIdentity {
            backend: 1,
            device: (u128::from(device.bus_number()) << 8) | u128::from(device.address()),
            inode: (topology << 8) | ports.len() as u128,
        })
    }
    fn reserve_identity(identity: AttachmentIdentity) -> Result<Arc<Self>, Error> {
        let mut registry = REGISTRY.lock();
        registry.owners.retain(|_, owner| owner.strong_count() > 0);
        if registry
            .owners
            .get(&identity)
            .and_then(Weak::upgrade)
            .is_some()
        {
            return Err(Error::Busy);
        }
        let context = Self::with_original(registry.remembered.get(&identity).cloned());
        *context.identity.lock() = Some(identity);
        registry.owners.insert(identity, Arc::downgrade(&context));
        Ok(context)
    }
    pub(crate) fn enter(self: &Arc<Self>) -> Scope {
        let previous = CURRENT.with(|slot| slot.replace(Some(self.clone())));
        Scope {
            previous,
            _thread: PhantomData,
        }
    }
    pub(super) fn volume_key(&self) -> Option<String> {
        self.identity.lock().map(|id| {
            if id.backend == 0 {
                format!("usbfs:{}:{}", id.device, id.inode)
            } else {
                format!("darwin:{}", id.device)
            }
        })
    }
    /// Consume a remembered-level lease only after successful DAC admission.
    pub(crate) fn admit(&self) {
        if let Some(identity) = *self.identity.lock() {
            REGISTRY.lock().remembered.remove(&identity);
        }
    }
    pub(super) fn keep_original(&self, keep: bool) {
        self.keep_original.store(keep, Ordering::Release);
    }
    pub(super) fn release(&self) {
        if let Some(identity) = self.identity.lock().take() {
            let mut registry = REGISTRY.lock();
            let remembered = &mut registry.remembered;
            if identity.backend == 0
                && self.keep_original.load(Ordering::Acquire)
                && !self.original_volume.lock().is_empty()
            {
                // Keep semantics preserve only original levels, never a live
                // transport, claims or an unscoped current-device fallback.
                if remembered.len() >= 64 {
                    if let Some(oldest) = remembered.keys().next().copied() {
                        remembered.remove(&oldest);
                    }
                    log_warn!("[USB] remembered original-volume limit reached; discarded an inactive record");
                }
                remembered.insert(identity, self.original_volume.clone());
            } else {
                remembered.remove(&identity);
            }
            // Publish retirement only after remembered-level handover is complete.
            registry.owners.remove(&identity);
        }
    }
    pub(crate) fn routing_hand_back_report(&self) -> Option<routing_hw::RoutingHandBackReport> {
        self.routing_report.lock().clone()
    }
    pub(crate) fn retired(self: &Arc<Self>, timeout: Duration) -> bool {
        let _scope = self.enter();
        super::api::await_usb_retirement(timeout)
    }
}
thread_local! { static CURRENT: RefCell<Option<Arc<SessionContext>>> = const { RefCell::new(None) }; }
pub(crate) struct Scope {
    previous: Option<Arc<SessionContext>>,
    _thread: PhantomData<Rc<()>>,
}
impl Drop for Scope {
    fn drop(&mut self) {
        CURRENT.with(|slot| {
            slot.replace(self.previous.take());
        });
    }
}

pub(crate) fn current() -> Arc<SessionContext> {
    if let Some(context) = CURRENT.with(|slot| slot.borrow().clone()) {
        return context;
    }
    #[cfg(test)]
    {
        static LEGACY_TEST_CONTEXT: Lazy<Arc<SessionContext>> =
            Lazy::new(SessionContext::unregistered);
        LEGACY_TEST_CONTEXT.clone()
    }
    #[cfg(not(test))]
    {
        panic!("USB transport called without its explicit owner context")
    }
}

pub(crate) fn await_connection_retirement(fd: RawFd, timeout: Duration) -> Result<bool, Error> {
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    if unsafe { libc::fstat(fd, stat.as_mut_ptr()) } != 0 {
        return Err(Error::Claim(std::io::Error::last_os_error().to_string()));
    }
    // SAFETY: successful fstat initialized stat.
    let stat = unsafe { stat.assume_init() };
    let identity = AttachmentIdentity {
        backend: 0,
        device: stat.st_dev as u128,
        inode: stat.st_ino as u128,
    };
    let owner = REGISTRY
        .lock()
        .owners
        .get(&identity)
        .and_then(Weak::upgrade);
    Ok(owner.map_or(true, |owner| owner.retired(timeout)))
}

pub(crate) fn await_all_retirement(timeout: Duration) -> bool {
    let contexts: Vec<_> = REGISTRY
        .lock()
        .owners
        .values()
        .filter_map(Weak::upgrade)
        .collect();
    let started = Instant::now();
    contexts
        .into_iter()
        .all(|context| context.retired(timeout.saturating_sub(started.elapsed())))
}

/// Static names remain implementation-local selectors; all mutable storage is
/// in the explicitly entered owner. Owning guards retain that owner's mutex.
pub(super) struct ScopedMutex<T>(fn(&SessionContext) -> Arc<Mutex<T>>);
impl<T> ScopedMutex<T> {
    pub(super) const fn new(select: fn(&SessionContext) -> Arc<Mutex<T>>) -> Self {
        Self(select)
    }
    pub(super) fn lock(&self) -> ArcMutexGuard<RawMutex, T> {
        (self.0)(&current()).lock_arc()
    }
    pub(super) fn try_lock_for(&self, timeout: Duration) -> Option<ArcMutexGuard<RawMutex, T>> {
        (self.0)(&current()).try_lock_arc_for(timeout)
    }
    pub(super) fn try_lock(&self) -> Option<ArcMutexGuard<RawMutex, T>> {
        (self.0)(&current()).try_lock_arc()
    }
}
pub(super) struct ScopedBool(fn(&SessionContext) -> &AtomicBool);
impl ScopedBool {
    pub(super) const fn new(select: fn(&SessionContext) -> &AtomicBool) -> Self {
        Self(select)
    }
    pub(super) fn load(&self, order: Ordering) -> bool {
        (self.0)(&current()).load(order)
    }
    pub(super) fn store(&self, value: bool, order: Ordering) {
        (self.0)(&current()).store(value, order);
    }
    pub(super) fn swap(&self, value: bool, order: Ordering) -> bool {
        (self.0)(&current()).swap(value, order)
    }
}
pub(super) struct ScopedU64(fn(&SessionContext) -> &AtomicU64);
impl ScopedU64 {
    pub(super) const fn new(select: fn(&SessionContext) -> &AtomicU64) -> Self {
        Self(select)
    }
    pub(super) fn fetch_add(&self, value: u64, order: Ordering) -> u64 {
        (self.0)(&current()).fetch_add(value, order)
    }
}
pub(super) struct ScopedU32(fn(&SessionContext) -> &AtomicU32);
impl ScopedU32 {
    pub(super) const fn new(select: fn(&SessionContext) -> &AtomicU32) -> Self {
        Self(select)
    }
    pub(super) fn store(&self, value: u32, order: Ordering) {
        (self.0)(&current()).store(value, order);
    }
    pub(super) fn swap(&self, value: u32, order: Ordering) -> u32 {
        (self.0)(&current()).swap(value, order)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::fake_dac;
    #[test]
    fn routing_report_is_cloned_and_scoped_to_its_owner() {
        use crate::routing::{ControlKey, Entity, RoutingHandBackOutcome, RoutingHandBackReport};
        let first = SessionContext::unregistered();
        let second = SessionContext::unregistered();
        let expected = RoutingHandBackReport {
            controls: vec![(
                ControlKey::Selector(Entity {
                    interface: 3,
                    id: 7,
                }),
                RoutingHandBackOutcome::DeadlineExpired,
            )],
        };
        *first.routing_report.lock() = Some(expected.clone());
        let mut copy = first.routing_hand_back_report().unwrap();
        copy.controls.clear();
        assert_eq!(first.routing_hand_back_report(), Some(expected));
        assert_eq!(second.routing_hand_back_report(), None);
    }

    #[test]
    fn independent_owners_keep_volume_payload_clock_state_and_close_separate() {
        let _serial = PAYLOAD_LATCH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let first = SessionContext::unregistered();
        let second = SessionContext::unregistered();
        let first_fd = -19001;
        let second_fd = -19002;
        let first_dac = Arc::new(StdMutex::new(fake_dac::FakeDac {
            volume: [-20 * 256; 3],
            ..Default::default()
        }));
        let second_dac = Arc::new(StdMutex::new(fake_dac::FakeDac {
            volume: [-30 * 256; 3],
            ..Default::default()
        }));
        fake_dac::install(first_fd, first_dac.clone());
        fake_dac::install(second_fd, second_dac.clone());
        struct Cleanup(i32, i32);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                fake_dac::remove(self.0);
                fake_dac::remove(self.1);
            }
        }
        let _cleanup = Cleanup(first_fd, second_fd);
        for (owner, fd, original) in [
            (&first, first_fd, -20 * 256),
            (&second, second_fd, -30 * 256),
        ] {
            let _scope = owner.enter();
            let mut state = fake_dac::state(false, Some(fake_dac::control()));
            state.device.fd = DeviceKey::Usbfs(fd);
            state.device._fd_owner = Some(Arc::new(OwnedUsbFd(fd)));
            state.device.device_name = Some(format!("fake-{fd}"));
            let key = original_volume_device_key(&state.device);
            owner
                .original_volume
                .lock()
                .note(&key, fake_dac::control(), vec![original; 2]);
            *owner.session.lock() = Some(state);
        }
        // The normal control functions execute concurrently under each explicit
        // worker owner; one device's volume mutex cannot serialize the other.
        let a = first.clone();
        let b = second.clone();
        let writer_a = thread::spawn(move || {
            let _scope = a.enter();
            write_volume_db(-45.0).unwrap()
        });
        let writer_b = thread::spawn(move || {
            let _scope = b.enter();
            write_volume_db(-55.0).unwrap()
        });
        assert_eq!(writer_a.join().unwrap().db, -45.0);
        assert_eq!(writer_b.join().unwrap().db, -55.0);
        assert_eq!(first_dac.lock().unwrap().volume[1], -45 * 256);
        assert_eq!(second_dac.lock().unwrap().volume[1], -55 * 256);
        {
            let _scope = first.enter();
            crate::settings::install(crate::settings::Settings::for_device(
                crate::DacConfig {
                    require_verified_rate: false,
                    ..Default::default()
                },
                &crate::DeviceInfo::new(1, 2, "first"),
            ));
            observe_pcm_payload_audit(true);
            assert!(USB_PCM_PAYLOAD_LATCH.lock().is_verified());
            assert!(!crate::settings::settings().require_verified_rate());
            assert!(set_original_volume_action(ORIGINAL_VOLUME_RESTORE));
            clear_session_state();
            assert!(first.session.lock().is_none());
        }
        assert_eq!(first_dac.lock().unwrap().volume[1], -20 * 256);
        {
            let _scope = second.enter();
            assert!(session_registered());
            assert!(crate::settings::settings().require_verified_rate());
            assert!(!USB_PCM_PAYLOAD_LATCH.lock().is_verified());
            assert_eq!(read_volume_state().unwrap().db, -55.0);
            assert!(set_original_volume_action(ORIGINAL_VOLUME_RESTORE));
            clear_session_state();
        }
        assert_eq!(second_dac.lock().unwrap().volume[1], -30 * 256);
    }

    #[test]
    fn duplicate_fds_are_refused_until_that_owner_retires() {
        use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
        let path = std::env::temp_dir().join(format!(
            "usbdac-context-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)
            .unwrap();
        std::fs::remove_file(path).unwrap();
        let duplicate = unsafe { OwnedFd::from_raw_fd(libc::dup(file.as_raw_fd())) };
        let first = SessionContext::reserve(file.as_raw_fd()).unwrap();
        assert!(matches!(
            SessionContext::reserve(duplicate.as_raw_fd()),
            Err(Error::Busy)
        ));
        first.release();
        let next = SessionContext::reserve(duplicate.as_raw_fd()).unwrap();
        next.release();
    }

    #[test]
    fn keep_survives_readonly_preflight_and_failed_open_leases() {
        use std::os::fd::AsRawFd;
        let _serial = PAYLOAD_LATCH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let file = std::fs::File::open("/dev/zero").unwrap();
        let owner = SessionContext::reserve(file.as_raw_fd()).unwrap();
        let key = owner.volume_key().unwrap();
        owner
            .original_volume
            .lock()
            .note(&key, fake_dac::control(), vec![-20 * 256; 2]);
        {
            let _scope = owner.enter();
            let mut forged = fake_dac::device();
            let before = original_volume_device_key(&forged);
            forged.device_name = Some("caller-supplied-different-name".into());
            assert_eq!(original_volume_device_key(&forged), before);
        }
        owner.keep_original(true);
        owner.release();
        // Read-only preflight / failed-open contexts never consume the lease.
        for _ in 0..2 {
            let preflight = SessionContext::reserve(file.as_raw_fd()).unwrap();
            assert!(preflight.original_volume.lock().is_recorded(&key));
            assert!(matches!(
                SessionContext::reserve(file.as_raw_fd()),
                Err(Error::Busy)
            ));
            drop(preflight);
        }
        let reopened = SessionContext::reserve(file.as_raw_fd()).unwrap();
        assert!(reopened.original_volume.lock().is_recorded(&key));
        reopened.admit();
        assert!(REGISTRY.lock().remembered.is_empty());
        reopened.keep_original(false);
        reopened.release();
    }

    #[test]
    fn volume_restore_waits_for_worker_retirement() {
        use super::super::api::{close_device, ORIGINAL_VOLUME_ACTION_RESTORE};
        use super::super::fake_dac::{Registered, ORIGINAL_RAW};
        let fake = Registered::new(false, true);
        fake.set_levels(ORIGINAL_RAW, ORIGINAL_RAW);
        fake.record_original();
        super::super::volume_hw::write_volume_db(-40.0).unwrap();
        let lowered = fake.raw();
        let writes = fake.dac.lock().unwrap().volume_writes.len();
        let guard = {
            let _gate = USB_SESSION_GATE.lock();
            SessionGuard::start_locked()
        };
        assert!(!close_device(
            ORIGINAL_VOLUME_ACTION_RESTORE,
            Duration::ZERO
        ));
        assert_eq!(fake.raw(), lowered);
        assert_eq!(fake.dac.lock().unwrap().volume_writes.len(), writes);
        assert!(fake.original_recorded());
        drop(guard);
        assert_eq!(fake.raw(), vec![ORIGINAL_RAW; 2]);
        assert_eq!(fake.dac.lock().unwrap().volume_writes.len(), writes + 2);
        assert!(!fake.original_recorded());
    }

    #[test]
    fn new_admission_waits_for_complete_owner_retirement() {
        use std::os::fd::AsRawFd;
        let file = std::fs::File::open("/dev/full")
            .or_else(|_| std::fs::File::open("/dev/urandom"))
            .unwrap();
        let owner = SessionContext::reserve(file.as_raw_fd()).unwrap();
        owner.admit();
        // Reuse the existing session model. Its -1 FD owner is a safe sentinel;
        // it observes coordinator ownership, not a real libusb handle.
        let state = super::super::session::test_state(None);
        let fd_owner = Arc::downgrade(state.device._fd_owner.as_ref().expect("usbfs owner"));
        *owner.session.lock() = Some(state);
        let session_guard = {
            let _scope = owner.enter();
            let _gate = USB_SESSION_GATE.lock();
            SessionGuard::start_locked()
        };
        {
            let _scope = owner.enter();
            clear_device();
            assert!(USB_SESSION_CLEAR_PENDING.load(Ordering::SeqCst));
            assert!(USB_STOP_REQUESTED.load(Ordering::SeqCst));
            assert!(!wait_for_session_stop(Duration::ZERO));
            assert!(session_registered());
        }
        assert!(!await_connection_retirement(file.as_raw_fd(), Duration::ZERO).unwrap());
        assert!(fd_owner.upgrade().is_some());
        assert!(matches!(
            SessionContext::reserve(file.as_raw_fd()),
            Err(Error::Busy)
        ));

        // The session guard is held by the worker until the return/retirement
        // boundary. Its drop performs the actual pending-clear coordination.
        drop(session_guard);
        assert!(owner.retired(Duration::ZERO));
        assert!(await_connection_retirement(file.as_raw_fd(), Duration::ZERO).unwrap());
        assert!(owner.session.lock().is_none());
        assert!(fd_owner.upgrade().is_none());
        let replacement = SessionContext::reserve(file.as_raw_fd()).unwrap();
        // Repeated cleanup of the old owner must not release its replacement.
        owner.release();
        assert!(matches!(
            SessionContext::reserve(file.as_raw_fd()),
            Err(Error::Busy)
        ));
        replacement.release();
    }

    #[test]
    fn nested_owner_scope_restores_the_previous_owner() {
        let first = SessionContext::unregistered();
        let second = SessionContext::unregistered();
        let _first = first.enter();
        assert!(Arc::ptr_eq(&current(), &first));
        {
            let _second = second.enter();
            assert!(Arc::ptr_eq(&current(), &second));
        }
        assert!(Arc::ptr_eq(&current(), &first));
    }
}
