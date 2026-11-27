//! Discovery is independent of opening a DAC and never grants USB permission.
use crate::{DeviceInfo, Error, Result};
use crossbeam_channel::{bounded, Receiver, Sender};
use parking_lot::Mutex;
use std::collections::BTreeMap;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::Duration;

/// Permission reported by the host. `Unknown` requires an actual open/claim;
/// discovering a descriptor is not proof that a process can access USB.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsbPermission {
    Unknown,
    Required,
    Granted,
    Denied,
}

/// One physical attachment. An unplug/replug receives a new ID even when the
/// OS reuses the device path. Use `id`, not VID/PID, for permission results.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsbAttachment {
    pub id: u64,
    pub host_key: String,
    pub info: DeviceInfo,
    pub permission: UsbPermission,
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum HotplugEvent {
    /// Initial state, delivered atomically with subscription registration.
    Snapshot(Vec<UsbAttachment>),
    Attached(UsbAttachment),
    Detached(UsbAttachment),
    PermissionChanged(UsbAttachment),
    DiscoveryFailed(String),
}

struct Subscriber {
    sender: Sender<HotplugEvent>,
    overflow: Arc<AtomicBool>,
}
#[derive(Default)]
struct State {
    next_id: u64,
    attached: BTreeMap<String, UsbAttachment>,
    subscribers: Vec<Subscriber>,
}

/// Shared discovery state and subscriptions. On Android feed this from the
/// host's registered USB attach/detach receiver and permission result callback,
/// taking an initial `UsbManager.deviceList` snapshot after registering. The
/// host retains responsibility for prompts, configuration, claims and FD life.
#[derive(Clone, Default)]
pub struct HotplugHub {
    state: Arc<Mutex<State>>,
}
impl HotplugHub {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn snapshot(&self) -> Vec<UsbAttachment> {
        self.state.lock().attached.values().cloned().collect()
    }
    pub fn subscribe(&self) -> HotplugSubscription {
        let (sender, receiver) = bounded(64);
        let overflow = Arc::new(AtomicBool::new(false));
        let mut state = self.state.lock();
        let snapshot = state.attached.values().cloned().collect();
        let _ = sender.try_send(HotplugEvent::Snapshot(snapshot));
        state.subscribers.push(Subscriber {
            sender,
            overflow: overflow.clone(),
        });
        HotplugSubscription {
            receiver,
            overflow,
            state: self.state.clone(),
        }
    }
    /// Record a host-observed attach. Repeated attach broadcasts are idempotent.
    /// Call `permission_changed` for an existing attachment's permission update.
    pub fn attached(
        &self,
        host_key: impl Into<String>,
        info: DeviceInfo,
        permission: UsbPermission,
    ) -> UsbAttachment {
        let host_key = host_key.into();
        let mut state = self.state.lock();
        attach_or_replace(&mut state, host_key, info, permission)
    }
    /// Reconcile a complete, authoritative host snapshot after events may have been missed.
    /// Serialize snapshot collection and event delivery in the host. A device which disappeared
    /// and returned at the same key entirely between snapshots cannot be distinguished here.
    /// Existing keys with the same VID/PID keep their IDs; missing or changed identities are detached.
    pub fn reconcile(&self, devices: Vec<(String, DeviceInfo, UsbPermission)>) {
        let mut state = self.state.lock();
        let mut observed = BTreeMap::new();
        for (key, info, permission) in devices {
            observed.insert(key, (info, permission));
        }
        let missing: Vec<_> = state
            .attached
            .keys()
            .filter(|key| !observed.contains_key(*key))
            .cloned()
            .collect();
        for key in missing {
            let attachment = state.attached.remove(&key).expect("attachment under lock");
            broadcast(&mut state, HotplugEvent::Detached(attachment));
        }
        for (key, (info, permission)) in observed {
            let attachment = attach_or_replace(&mut state, key.clone(), info, permission);
            if attachment.permission != permission {
                let existing = state.attached.get_mut(&key).expect("attachment under lock");
                existing.permission = permission;
                let event = HotplugEvent::PermissionChanged(existing.clone());
                broadcast(&mut state, event);
            }
        }
    }
    /// Reject delayed results for an unplugged device, including a reused path.
    pub fn permission_changed(&self, id: u64, permission: UsbPermission) -> bool {
        let mut state = self.state.lock();
        let Some(attachment) = state.attached.values_mut().find(|a| a.id == id) else {
            return false;
        };
        if attachment.permission != permission {
            attachment.permission = permission;
            let event = HotplugEvent::PermissionChanged(attachment.clone());
            broadcast(&mut state, event);
        }
        true
    }
    pub fn detached(&self, id: u64) -> bool {
        let mut state = self.state.lock();
        let Some(key) = state
            .attached
            .iter()
            .find(|(_, a)| a.id == id)
            .map(|(k, _)| k.clone())
        else {
            return false;
        };
        let attachment = state.attached.remove(&key).expect("attachment under lock");
        broadcast(&mut state, HotplugEvent::Detached(attachment));
        true
    }
}
// VID/PID detects changed models, not an unobserved replacement by the same model.
fn same_identity(existing: &UsbAttachment, info: &DeviceInfo) -> bool {
    existing.info.vendor_id == info.vendor_id && existing.info.product_id == info.product_id
}

fn attach_or_replace(
    state: &mut State,
    host_key: String,
    info: DeviceInfo,
    permission: UsbPermission,
) -> UsbAttachment {
    if let Some(existing) = state.attached.get(&host_key) {
        if same_identity(existing, &info) {
            return existing.clone();
        }
        let old = state
            .attached
            .remove(&host_key)
            .expect("attachment under lock");
        broadcast(state, HotplugEvent::Detached(old));
    }
    state.next_id = state
        .next_id
        .checked_add(1)
        .expect("USB attachment ID exhausted");
    let attachment = UsbAttachment {
        id: state.next_id,
        host_key: host_key.clone(),
        info,
        permission,
    };
    state.attached.insert(host_key, attachment.clone());
    broadcast(state, HotplugEvent::Attached(attachment.clone()));
    attachment
}

fn broadcast(state: &mut State, event: HotplugEvent) {
    state.subscribers.retain(
        |subscriber| match subscriber.sender.try_send(event.clone()) {
            Ok(()) => true,
            Err(crossbeam_channel::TrySendError::Full(_)) => {
                subscriber.overflow.store(true, Ordering::Release);
                true
            }
            Err(crossbeam_channel::TrySendError::Disconnected(_)) => false,
        },
    );
}

/// Bounded, nonblocking producer subscription. Both receive methods return
/// a fresh `Snapshot` after overflow, atomically discarding old hints. Dropping a subscription releases its queue.
pub struct HotplugSubscription {
    receiver: Receiver<HotplugEvent>,
    overflow: Arc<AtomicBool>,
    state: Arc<Mutex<State>>,
}
impl HotplugSubscription {
    fn resync(&self) -> Option<HotplugEvent> {
        let state = self.state.lock();
        if self.overflow.swap(false, Ordering::AcqRel) {
            for _ in 0..64 {
                let _ = self.receiver.try_recv();
            }
            Some(HotplugEvent::Snapshot(
                state.attached.values().cloned().collect(),
            ))
        } else {
            None
        }
    }
    pub fn try_recv(&self) -> std::result::Result<HotplugEvent, crossbeam_channel::TryRecvError> {
        if let Some(snapshot) = self.resync() {
            Ok(snapshot)
        } else {
            self.receiver.try_recv()
        }
    }
    pub fn recv_timeout(
        &self,
        timeout: Duration,
    ) -> std::result::Result<HotplugEvent, crossbeam_channel::RecvTimeoutError> {
        if let Some(snapshot) = self.resync() {
            return Ok(snapshot);
        }
        let event = self.receiver.recv_timeout(timeout)?;
        if let Some(snapshot) = self.resync() {
            Ok(snapshot)
        } else {
            Ok(event)
        }
    }
}

/// Linux native libusb hotplug events, without opening or claiming devices.
/// If libusb lacks hotplug support, polls descriptors every 250 ms; a complete
/// unplug/replug between fallback polls may be missed;
/// Android must use its host receiver instead. Retain this handle to monitor;
/// dropping it stops and joins the worker. This is discovery, not DAC support
/// verification; composite devices with Audio interfaces are included.
pub struct LinuxHotplugMonitor {
    stop: Sender<()>,
    worker: Option<std::thread::JoinHandle<()>>,
    hub: HotplugHub,
}
impl LinuxHotplugMonitor {
    pub fn start() -> Result<Self> {
        #[cfg(target_os = "linux")]
        {
            let context = rusb::Context::new().map_err(|e| Error::Claim(e.to_string()))?;
            let hub = HotplugHub::new();
            use rusb::UsbContext;
            let native = rusb::has_hotplug();
            let registration = if native {
                Some(
                    rusb::HotplugBuilder::new()
                        .enumerate(true)
                        .register(&context, Box::new(NativeHotplug(hub.clone())))
                        .map_err(|e| Error::Claim(e.to_string()))?,
                )
            } else {
                for (key, info) in scan_linux(&context)? {
                    hub.attached(key, info, UsbPermission::Unknown);
                }
                None
            };
            let worker_hub = hub.clone();
            let (stop, receiver) = bounded(1);
            let worker = std::thread::Builder::new()
                .name("usbdac-hotplug".into())
                .spawn(move || {
                    let _registration = registration;
                    while receiver.try_recv() == Err(crossbeam_channel::TryRecvError::Empty) {
                        if native {
                            if let Err(error) =
                                context.handle_events(Some(Duration::from_millis(100)))
                            {
                                broadcast(
                                    &mut worker_hub.state.lock(),
                                    HotplugEvent::DiscoveryFailed(error.to_string()),
                                );
                                break;
                            }
                            continue;
                        }
                        if receiver.recv_timeout(Duration::from_millis(250))
                            != Err(crossbeam_channel::RecvTimeoutError::Timeout)
                        {
                            break;
                        }
                        match scan_linux(&context) {
                            Ok(devices) => {
                                for attachment in worker_hub.snapshot() {
                                    if !devices.iter().any(|(key, info)| {
                                        *key == attachment.host_key
                                            && info.vendor_id == attachment.info.vendor_id
                                            && info.product_id == attachment.info.product_id
                                    }) {
                                        worker_hub.detached(attachment.id);
                                    }
                                }
                                for (key, info) in devices {
                                    worker_hub.attached(key, info, UsbPermission::Unknown);
                                }
                            }
                            Err(e) => broadcast(
                                &mut worker_hub.state.lock(),
                                HotplugEvent::DiscoveryFailed(e.to_string()),
                            ),
                        }
                    }
                })
                .map_err(|e| Error::Claim(e.to_string()))?;
            Ok(Self {
                stop,
                worker: Some(worker),
                hub,
            })
        }
        #[cfg(not(target_os = "linux"))]
        {
            Err(Error::PlatformUnsupported)
        }
    }
    pub fn hub(&self) -> HotplugHub {
        self.hub.clone()
    }
}
#[cfg(target_os = "linux")]
struct NativeHotplug(HotplugHub);
#[cfg(target_os = "linux")]
impl rusb::Hotplug<rusb::Context> for NativeHotplug {
    fn device_arrived(&mut self, device: rusb::Device<rusb::Context>) {
        if let Some((key, info)) = describe_linux(&device) {
            self.0.attached(key, info, UsbPermission::Unknown);
        }
    }
    fn device_left(&mut self, device: rusb::Device<rusb::Context>) {
        let key = linux_path(&device);
        if let Some(attachment) = self.0.snapshot().into_iter().find(|a| a.host_key == key) {
            self.0.detached(attachment.id);
        }
    }
}
#[cfg(target_os = "linux")]
fn linux_path(device: &rusb::Device<rusb::Context>) -> String {
    format!(
        "/dev/bus/usb/{:03}/{:03}",
        device.bus_number(),
        device.address()
    )
}
#[cfg(target_os = "linux")]
fn describe_linux(device: &rusb::Device<rusb::Context>) -> Option<(String, DeviceInfo)> {
    let descriptor = device.device_descriptor().ok()?;
    let audio = descriptor.class_code() == 1
        || (0..descriptor.num_configurations()).any(|index| {
            device.config_descriptor(index).is_ok_and(|config| {
                config
                    .interfaces()
                    .any(|i| i.descriptors().any(|d| d.class_code() == 1))
            })
        });
    if !audio {
        return None;
    }
    let path = linux_path(device);
    let info = DeviceInfo::new(
        descriptor.vendor_id(),
        descriptor.product_id(),
        format!(
            "USB audio {:04x}:{:04x}",
            descriptor.vendor_id(),
            descriptor.product_id()
        ),
    )
    .with_device_name(path.clone());
    Some((path, info))
}
#[cfg(target_os = "linux")]
fn scan_linux(context: &rusb::Context) -> Result<Vec<(String, DeviceInfo)>> {
    use rusb::UsbContext;
    let devices = context.devices().map_err(|e| Error::Claim(e.to_string()))?;
    let mut result = Vec::new();
    for device in devices.iter() {
        if let Some(device) = describe_linux(&device) {
            result.push(device);
        }
    }
    Ok(result)
}
impl Drop for LinuxHotplugMonitor {
    fn drop(&mut self) {
        let _ = self.stop.try_send(());
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}
