use super::*;
use alternate_controls::{
    AlternateControls, AS_ACTIVE_ALT_SETTING_CONTROL, AS_VALID_ALT_SETTINGS_CONTROL,
    MAX_VALID_ALTERNATES_REPLY_LEN,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) enum DeviceKey {
    Usbfs(RawFd),
    #[cfg(target_os = "macos")]
    Darwin(usize),
}

impl DeviceKey {
    fn usbfs(self) -> Option<RawFd> {
        match self {
            Self::Usbfs(fd) => Some(fd),
            #[cfg(target_os = "macos")]
            Self::Darwin(_) => None,
        }
    }
}

pub(super) fn badd_snapshot_matches(
    active_id: u8,
    header: &[u8; 9],
    raw: &[u8],
    snapshot: &[u8],
) -> bool {
    active_id != 0
        && header[0] == 9
        && header[1] == 2
        && header[5] == active_id
        && raw.len() >= 9
        && raw[..9] == *header
        && raw == snapshot
}

/// Return the witnessed BADD capability, or unknown (zero) if unavailable or busy.
pub fn badd_descriptor_witness(fd: RawFd, active_id: u8, snapshot: &[u8]) -> u8 {
    try_badd_descriptor_witness(fd, active_id, snapshot).unwrap_or(0)
}

/// A bounded read-only witness, reserved against native session ownership.
/// On Linux and Android, libusb wraps the fd only to cache descriptors and
/// query speed; witness controls use synchronous usbfs ioctl, never an event
/// pump or asynchronous URB. Descriptor caching may change the file offset.
pub fn try_badd_descriptor_witness(
    fd: RawFd,
    active_id: u8,
    snapshot: &[u8],
) -> Result<u8, crate::Error> {
    let _reservation = super::context::SessionContext::reserve(fd)?;
    if !cfg!(any(target_os = "linux", target_os = "android")) {
        return Ok(0);
    }
    if fd < 0 || active_id == 0 || snapshot.len() < 9 || snapshot.len() > 16384 {
        return Ok(0);
    }
    let duplicated = unsafe { libc::dup(fd) };
    if duplicated < 0 {
        return Ok(0);
    }
    let owned = OwnedUsbFd(duplicated);
    let result = (|| -> Option<u8> {
        let context = Context::new().ok()?;
        let handle = unsafe { context.open_device_with_fd(owned.0) }.ok()?;
        let read_control = |request: u8, value: u16, buf: &mut [u8]| {
            let transfer = UsbfsControlTransfer {
                request_type: 0x80,
                request,
                value,
                index: 0,
                length: u16::try_from(buf.len()).ok()?,
                timeout_ms: 500,
                data: buf.as_mut_ptr().cast(),
            };
            source_control(DeviceKey::Usbfs(owned.0), None, &transfer).ok()
        };
        let mut active = [0u8; 1];
        let get_active = |data: &mut [u8]| read_control(0x08, 0, data) == Some(1);
        if !get_active(&mut active) || active[0] != active_id {
            return None;
        }
        let device = handle.device();
        let speed = device.speed();
        let config = device.active_config_descriptor().ok()?;
        if config.number() != active_id || handle.active_configuration().ok()? != active_id {
            return None;
        }
        let count = device.device_descriptor().ok()?.num_configurations();
        let mut index = None;
        for n in 0..count {
            let c = device.config_descriptor(n).ok()?;
            if c.number() == active_id && index.replace(n).is_some() {
                return None;
            }
        }
        let index = index?;
        let read = |buf: &mut [u8]| read_control(0x06, 0x0200 | u16::from(index), buf);
        let mut header = [0u8; 9];
        if read(&mut header) != Some(9)
            || header[0] != 9
            || header[1] != 2
            || header[5] != active_id
        {
            return None;
        }
        let len = usize::from(u16::from_le_bytes([header[2], header[3]]));
        if !(9..=16384).contains(&len) || len != snapshot.len() {
            return None;
        }
        let mut raw = vec![0; len];
        if read(&mut raw) != Some(len) || !badd_snapshot_matches(active_id, &header, &raw, snapshot)
        {
            return None;
        }
        let proved = super::adc3::badd_only_fixed_rate_witness(&config, &raw, speed);
        active[0] = 0;
        if !get_active(&mut active) || active[0] != active_id {
            return None;
        }
        Some(proved)
    })();
    Ok(result.unwrap_or(0))
}

#[derive(Debug)]
pub(super) struct OwnedUsbFd(pub(super) RawFd);

impl Drop for OwnedUsbFd {
    fn drop(&mut self) {
        if self.0 >= 0 {
            unsafe {
                libc::close(self.0);
            }
        }
    }
}

#[cfg(target_os = "macos")]
#[derive(Debug)]
pub(super) struct DarwinOwner {
    pub(super) handle: Arc<DeviceHandle<Context>>,
    pub(super) claimed: Vec<u8>,
    pub(super) captured: bool,
}
#[cfg(target_os = "macos")]
impl Drop for DarwinOwner {
    fn drop(&mut self) {
        for &interface in self.claimed.iter().rev() {
            match self.handle.release_interface(interface) {
                Ok(()) => log_info!("[USB] host_usb_handback operation=release_interface interface={interface} result=success"),
                Err(error) => log_error!("[USB] host_usb_handback operation=release_interface interface={interface} result=error kind={:?} detail={error}", host_usb_failure_kind(error)),
            }
        }
        if self.captured {
            match self.handle.attach_kernel_driver(0) {
                Ok(()) => log_info!("[USB] host_usb_handback operation=reattach_kernel_driver result=success"),
                Err(error) => log_error!("[USB] host_usb_handback operation=reattach_kernel_driver result=error kind={:?} detail={error}; reconnect the USB device to restore its system driver", host_usb_failure_kind(error)),
            }
        }
    }
}

/// Run a control transfer through the actual source. Darwin uses the same
/// libusb handle that owns the claims and asynchronous transfers.
pub(super) fn source_control(
    key: DeviceKey,
    handle: Option<&DeviceHandle<Context>>,
    request: &UsbfsControlTransfer,
) -> std::io::Result<usize> {
    if let Some(fd) = key.usbfs() {
        let result = unsafe { libc::ioctl(fd, USBDEVFS_CONTROL, request) };
        return if result < 0 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(result as usize)
        };
    }
    #[cfg(target_os = "macos")]
    {
        let handle = handle.expect("Darwin handle");
        let timeout = Duration::from_millis(u64::from(request.timeout_ms));
        let data = unsafe {
            std::slice::from_raw_parts_mut(request.data.cast::<u8>(), usize::from(request.length))
        };
        let result = if request.request_type & 0x80 != 0 {
            handle.read_control(
                request.request_type,
                request.request,
                request.value,
                request.index,
                data,
                timeout,
            )
        } else {
            handle.write_control(
                request.request_type,
                request.request,
                request.value,
                request.index,
                data,
                timeout,
            )
        };
        result.map_err(|error| match error {
            rusb::Error::NoDevice => std::io::Error::from_raw_os_error(libc::ENODEV),
            _ => std::io::Error::other(error.to_string()),
        })
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = handle;
        unreachable!()
    }
}

#[derive(Debug, Clone)]
pub struct UsbDevice {
    pub(super) fd: DeviceKey,
    #[cfg(target_os = "macos")]
    pub(super) darwin: Option<Arc<DarwinOwner>>,
    pub(super) _fd_owner: Option<Arc<OwnedUsbFd>>,
    pub(super) claimed_interfaces: Arc<[u8]>,
    pub vendor_id: u16,
    pub product_id: u16,
    pub product_name: String,
    pub device_name: Option<String>,
}

impl UsbDevice {
    pub fn try_new(
        fd: RawFd,
        vendor_id: u16,
        product_id: u16,
        product_name: String,
        device_name: Option<String>,
        claimed_interfaces: Vec<u8>,
    ) -> Result<Self, String> {
        let duplicated_fd = unsafe { libc::dup(fd) };
        if duplicated_fd < 0 {
            return Err(format!(
                "Failed to duplicate USB file descriptor: {}",
                std::io::Error::last_os_error()
            ));
        }

        Ok(Self {
            fd: DeviceKey::Usbfs(duplicated_fd),
            #[cfg(target_os = "macos")]
            darwin: None,
            _fd_owner: Some(Arc::new(OwnedUsbFd(duplicated_fd))),
            claimed_interfaces: claimed_interfaces.into(),
            vendor_id,
            product_id,
            product_name,
            device_name,
        })
    }

    pub(super) fn same_owner(&self, other: &Self) -> bool {
        if let (Some(left), Some(right)) = (&self._fd_owner, &other._fd_owner) {
            return Arc::ptr_eq(left, right);
        }
        #[cfg(target_os = "macos")]
        if let (Some(left), Some(right)) = (&self.darwin, &other.darwin) {
            return Arc::ptr_eq(left, right);
        }
        false
    }

    pub(super) fn control_handle(&self) -> Option<Arc<DeviceHandle<Context>>> {
        #[cfg(target_os = "macos")]
        if let Some(owner) = &self.darwin {
            return Some(owner.handle.clone());
        }
        None
    }

    pub(super) fn feature_unit_control(
        &self,
        request_type: u8,
        request: u8,
        value: u16,
        index: u16,
        data: &mut [u8],
    ) -> std::io::Result<usize> {
        self.feature_unit_control_inner(
            request_type,
            request,
            value,
            index,
            data,
            Duration::from_secs(1),
            false,
        )
    }

    pub(super) fn feature_unit_control_timed(
        &self,
        request_type: u8,
        request: u8,
        value: u16,
        index: u16,
        data: &mut [u8],
        timeout: Duration,
    ) -> std::io::Result<usize> {
        self.feature_unit_control_inner(request_type, request, value, index, data, timeout, true)
    }

    fn feature_unit_control_inner(
        &self,
        request_type: u8,
        request: u8,
        value: u16,
        index: u16,
        data: &mut [u8],
        timeout: Duration,
        bounded_delay: bool,
    ) -> std::io::Result<usize> {
        let started = std::time::Instant::now();
        if timeout.is_zero() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "routing deadline expired",
            ));
        }
        if !self.claimed_interfaces.contains(&(index as u8)) {
            return Err(std::io::Error::other(
                "AudioControl interface is not claimed",
            ));
        }
        let length = u16::try_from(data.len()).map_err(std::io::Error::other)?;
        #[cfg(test)]
        {
            if let Some(result) = super::fake_dac::dispatch(
                self.fd.usbfs().unwrap(),
                request_type,
                request,
                value,
                index,
                data,
            ) {
                return result;
            }
        }
        let transfer = UsbfsControlTransfer {
            request_type,
            request,
            value,
            index,
            length,
            timeout_ms: timeout
                .as_millis()
                .saturating_add(if timeout.subsec_nanos() % 1_000_000 != 0 {
                    1
                } else {
                    0
                })
                .clamp(1, u32::MAX as u128) as u32,
            data: data.as_mut_ptr().cast(),
        };
        // This synchronous ioctl does not reap URBs. A separate libusb control
        // context on the same fd could consume the output context's completions.
        #[cfg(target_os = "macos")]
        let result = if let Some(owner) = &self.darwin {
            source_control(self.fd, Some(&owner.handle), &transfer)
        } else {
            let n = unsafe {
                libc::ioctl(
                    self.fd.usbfs().expect("usbfs source"),
                    USBDEVFS_CONTROL,
                    &transfer,
                )
            };
            if n < 0 {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(n as usize)
            }
        };
        #[cfg(not(target_os = "macos"))]
        let result = {
            let n = unsafe {
                libc::ioctl(
                    self.fd.usbfs().expect("usbfs source"),
                    USBDEVFS_CONTROL,
                    &transfer,
                )
            };
            if n < 0 {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(n as usize)
            }
        };
        if request_type & 0x60 == USB_TYPE_CLASS {
            let delay = settings().control_delay_us();
            if delay > 0 {
                let delay = Duration::from_micros(delay);
                if bounded_delay {
                    let left = timeout.saturating_sub(started.elapsed());
                    if left < delay {
                        thread::sleep(left);
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::TimedOut,
                            "routing deadline during control delay",
                        ));
                    }
                }
                thread::sleep(delay);
            }
        }
        result
    }
}

pub(super) struct IdleLock {
    #[cfg(target_os = "macos")]
    owner: Option<Arc<DarwinOwner>>,
    pub(super) device_fd: DeviceKey,
    pub(super) context: Context,
    pub(super) handle: Arc<DeviceHandle<Context>>,
    pub(super) claimed_interfaces: Arc<[u8]>,
    pub(super) active_interfaces: Mutex<Vec<u8>>,
    // Keep the wrapped descriptor alive until after the libusb handle drops.
    pub(super) fd_owner: Option<Arc<OwnedUsbFd>>,
}

pub(super) struct ClaimedHandle {
    // Drop before the libusb handle/context; armed only by exclusive setup.
    pub(super) itf_mode: ItfModeLease,
    pub(super) adc3_power: Adc3PowerLease,
    #[cfg(target_os = "macos")]
    owner: Option<Arc<DarwinOwner>>,
    pub(super) device_fd: DeviceKey,
    pub(super) context: Context,
    pub(super) handle: Arc<DeviceHandle<Context>>,
    pub(super) claimed_interfaces: Arc<[u8]>,
    pub(super) active_interfaces: Mutex<Vec<u8>>,
    // Keep the wrapped descriptor alive until after the libusb handle drops.
    pub(super) fd_owner: Option<Arc<OwnedUsbFd>>,
}

// linux/usbdevice_fs.h; use libc's architecture-specific ioctl encoding.
#[repr(C)]
pub(super) struct UsbfsControlTransfer {
    pub(super) request_type: u8,
    pub(super) request: u8,
    pub(super) value: u16,
    pub(super) index: u16,
    pub(super) length: u16,
    pub(super) timeout_ms: u32,
    pub(super) data: *mut c_void,
}

#[repr(C)]
pub(super) struct UsbfsSetInterface {
    pub(super) interface: libc::c_uint,
    pub(super) altsetting: libc::c_uint,
}

// `libc`'s ioctl-number helpers take u32 arguments on Linux and Android but u64
// on the other hosts, which only type-check and unit-test this code.
#[cfg(any(target_os = "linux", target_os = "android"))]
const fn ioctl_nr<T>(read: bool, write: bool, number: u8) -> IoctlRequest {
    match (read, write) {
        (true, true) => libc::_IOWR::<T>(b'U' as u32, number as u32),
        (true, false) => libc::_IOR::<T>(b'U' as u32, number as u32),
        (false, true) => libc::_IOW::<T>(b'U' as u32, number as u32),
        (false, false) => libc::_IO(b'U' as u32, number as u32),
    }
}

#[cfg(not(any(target_os = "linux", target_os = "android")))]
const fn ioctl_nr<T>(read: bool, write: bool, number: u8) -> IoctlRequest {
    match (read, write) {
        (true, true) => libc::_IOWR::<T>(b'U' as u64, number as u64),
        (true, false) => libc::_IOR::<T>(b'U' as u64, number as u64),
        (false, true) => libc::_IOW::<T>(b'U' as u64, number as u64),
        (false, false) => libc::_IO(b'U' as u64, number as u64),
    }
}

pub(super) const USBDEVFS_CONTROL: IoctlRequest = ioctl_nr::<UsbfsControlTransfer>(true, true, 0);
pub(super) const USBDEVFS_SETINTERFACE: IoctlRequest =
    ioctl_nr::<UsbfsSetInterface>(true, false, 4);

impl From<IdleLock> for ClaimedHandle {
    fn from(value: IdleLock) -> Self {
        Self {
            #[cfg(target_os = "macos")]
            owner: value.owner,
            itf_mode: ItfModeLease::new(),
            adc3_power: Adc3PowerLease::new(
                value.device_fd,
                Some(value.handle.clone()),
                value.fd_owner.clone(),
            ),
            device_fd: value.device_fd,
            context: value.context,
            handle: value.handle,
            claimed_interfaces: value.claimed_interfaces,
            active_interfaces: value.active_interfaces,
            fd_owner: value.fd_owner,
        }
    }
}

/// Owns the libusb handle and its wrapped descriptor for every native transfer.
/// Field order closes the handle before the descriptor.
pub(super) struct TransferOwner {
    handle: Arc<DeviceHandle<Context>>,
    _fd_owner: Option<Arc<OwnedUsbFd>>,
    #[cfg(target_os = "macos")]
    _darwin_owner: Option<Arc<DarwinOwner>>,
}
impl TransferOwner {
    pub(super) fn matches(&self, handle: &DeviceHandle<Context>) -> bool {
        self.handle.as_raw() == handle.as_raw()
    }
}

impl ClaimedHandle {
    pub(super) fn transfer_owner(&self) -> Arc<TransferOwner> {
        Arc::new(TransferOwner {
            handle: self.handle.clone(),
            _fd_owner: self.fd_owner.clone(),
            #[cfg(target_os = "macos")]
            _darwin_owner: self.owner.clone(),
        })
    }
    pub(super) fn into_idle_lock(self) -> IdleLock {
        IdleLock {
            #[cfg(target_os = "macos")]
            owner: self.owner,
            device_fd: self.device_fd,
            context: self.context,
            handle: self.handle,
            claimed_interfaces: self.claimed_interfaces,
            active_interfaces: self.active_interfaces,
            fd_owner: self.fd_owner,
        }
    }

    pub(super) fn ensure_interface_claimed(&self, interface_number: u8) -> Result<(), String> {
        if !self.claimed_interfaces.contains(&interface_number) {
            return Err(format!(
                "USB interface {} was not claimed by the host before Dac::open",
                interface_number
            ));
        }
        Ok(())
    }

    pub(super) fn set_alternate_setting(
        &self,
        interface_number: u8,
        alternate_setting: u8,
    ) -> Result<(), String> {
        if self.try_set_alternate_setting(interface_number, alternate_setting)? {
            Ok(())
        } else {
            Err(format!(
                "requested alternate {alternate_setting} is not currently valid"
            ))
        }
    }

    /// Return false only for a validated clear availability bit, before SET.
    /// Control errors and failures after attempting SET remain hard errors.
    pub(super) fn try_set_alternate_setting(
        &self,
        interface_number: u8,
        alternate_setting: u8,
    ) -> Result<bool, String> {
        let (controls, available) =
            self.inspect_alternate_availability(interface_number, alternate_setting)?;
        if !available {
            return Ok(false);
        }
        let request = UsbfsSetInterface {
            interface: interface_number.into(),
            altsetting: alternate_setting.into(),
        };
        // Android's fd and our dup share the usbfs claims. libusb's own
        // per-handle claim table does not, so its alt-setting helper refuses
        // these interfaces. Use the same kernel ioctl Android uses instead.
        if let Some(fd) = self.device_fd.usbfs() {
            let result = unsafe { libc::ioctl(fd, USBDEVFS_SETINTERFACE, &request) };
            if result < 0 {
                return Err(std::io::Error::last_os_error().to_string());
            }
        } else {
            self.handle
                .set_alternate_setting(interface_number, alternate_setting)
                .map_err(|error| error.to_string())?;
        }
        let mut active = self.active_interfaces.lock();
        active.retain(|number| *number != interface_number);
        if alternate_setting != 0 {
            active.push(interface_number);
        }
        drop(active);
        itf_interface_delay(&self.handle.device(), interface_number);
        // Record the successful SET_INTERFACE before verification: callers'
        // existing rollback paths must reset this interface if readback fails.
        if controls.active {
            let mut response = [0u8; 1];
            let length = self.read_alternate_control(
                interface_number,
                AS_ACTIVE_ALT_SETTING_CONTROL,
                &mut response,
            )?;
            alternate_controls::validate_active_alt(&response[..length], alternate_setting)?;
        }
        Ok(true)
    }

    /// Inspect live availability without selecting an alternate setting.
    pub(super) fn alternate_is_available(
        &self,
        interface: u8,
        alternate: u8,
    ) -> Result<bool, String> {
        self.inspect_alternate_availability(interface, alternate)
            .map(|(_, available)| available)
    }

    fn inspect_alternate_availability(
        &self,
        interface_number: u8,
        alternate_setting: u8,
    ) -> Result<(AlternateControls, bool), String> {
        self.ensure_interface_claimed(interface_number)?;
        if alternate_setting == 0 {
            // Always permit alt zero for recovery, even after a control failure.
            return Ok((AlternateControls::default(), true));
        }
        let config = self
            .handle
            .device()
            .active_config_descriptor()
            .map_err(|error| format!("Cannot inspect streaming alternate: {error}"))?;
        let interface = config
            .interfaces()
            .find(|interface| interface.number() == interface_number)
            .ok_or_else(|| format!("USB interface {interface_number} disappeared"))?;
        let descriptor = interface
            .descriptors()
            .find(|descriptor| descriptor.setting_number() == alternate_setting)
            .ok_or_else(|| {
                format!(
                    "USB interface {interface_number} alternate {alternate_setting} disappeared"
                )
            })?;
        if descriptor.class_code() != 1 || descriptor.sub_class_code() != 2 {
            return Ok((AlternateControls::default(), true));
        }

        let controls = alternate_controls::advertised_controls(
            descriptor.protocol_code(),
            descriptor.extra(),
        )?;
        // Valid Alternate Settings describes the whole AS interface. A sibling
        // can advertise it even when this alternate omits its own control field.
        // Keep Active Alternate Setting readback scoped to the selected alternate.
        let valid_control = controls.valid
            || interface.descriptors().any(|sibling| {
                sibling.setting_number() != 0
                    && sibling.setting_number() != alternate_setting
                    && sibling.class_code() == 1
                    && sibling.sub_class_code() == 2
                    && sibling.protocol_code() == descriptor.protocol_code()
                    && matches!(
                        alternate_controls::advertised_controls(
                            sibling.protocol_code(),
                            sibling.extra(),
                        ),
                        Ok(sibling_controls) if sibling_controls.valid
                    )
            });
        if valid_control {
            let declared: Vec<u8> = interface
                .descriptors()
                .map(|descriptor| descriptor.setting_number())
                .collect();
            let mut response = [0; MAX_VALID_ALTERNATES_REPLY_LEN];
            let length = self.read_alternate_control(
                interface_number,
                AS_VALID_ALT_SETTINGS_CONTROL,
                &mut response,
            )?;
            let available = alternate_controls::valid_alternate_is_available(
                &response[..length],
                &declared,
                alternate_setting,
            )?;
            return Ok((controls, available));
        }
        Ok((controls, true))
    }

    fn read_alternate_control(
        &self,
        interface_number: u8,
        value: u16,
        response: &mut [u8],
    ) -> Result<usize, String> {
        let request = UsbfsControlTransfer {
            request_type: 0xa1,
            request: 0x01,
            value,
            index: u16::from(interface_number),
            length: response.len() as u16,
            timeout_ms: 1000,
            data: response.as_mut_ptr().cast(),
        };
        // Keep class requests on the owning usbfs fd, without another libusb
        // completion consumer or any write to the alternate-setting controls.
        let result = source_control(self.device_fd, Some(&self.handle), &request);
        itf_class_control_delay(&self.handle);
        let length = result.map_err(|error| {
            format!("USB interface {interface_number} alternate control {value:#06x}: {error}")
        })?;
        if length > response.len() {
            return Err("Alternate control response exceeds requested length".into());
        }
        Ok(length)
    }
}

pub(super) fn open_claimed_usb_handle(device: &UsbDevice) -> Result<ClaimedHandle, String> {
    if let Some(lock) = IDLE_LOCK.lock().take() {
        if lock.device_fd == device.fd {
            return Ok(lock.into());
        }
    }

    wrap_android_usb_handle(device)
}

pub(super) fn wrap_android_usb_handle(device: &UsbDevice) -> Result<ClaimedHandle, String> {
    #[cfg(target_os = "macos")]
    if let Some(owner) = &device.darwin {
        let handle = owner.handle.clone();
        let descriptor = handle
            .device()
            .device_descriptor()
            .map_err(|e| e.to_string())?;
        let version = descriptor.device_version();
        let revision = (u16::from(version.major() / 10) << 12)
            | (u16::from(version.major() % 10) << 8)
            | (u16::from(version.minor()) << 4)
            | u16::from(version.sub_minor());
        crate::settings::install_descriptor_identity(
            descriptor.vendor_id(),
            descriptor.product_id(),
            revision,
        );
        return Ok(ClaimedHandle {
            owner: Some(owner.clone()),
            itf_mode: ItfModeLease::new(),
            adc3_power: Adc3PowerLease::new(
                device.fd,
                Some(handle.clone()),
                device._fd_owner.clone(),
            ),
            device_fd: device.fd,
            context: handle.device().context().clone(),
            handle,
            claimed_interfaces: Arc::clone(&device.claimed_interfaces),
            active_interfaces: Mutex::new(Vec::new()),
            fd_owner: device._fd_owner.clone(),
        });
    }
    prepare_direct_usb_libusb();

    let context =
        Context::new().map_err(|error| format!("Failed to create libusb context: {}", error))?;
    let handle = unsafe { context.open_device_with_fd(device.fd.usbfs().expect("usbfs source")) }
        .map_err(|error| format!("Failed to wrap USB file descriptor: {}", error))?;

    let handle = Arc::new(handle);
    let descriptor = handle
        .device()
        .device_descriptor()
        .map_err(|error| format!("Failed to read actual USB identity: {error}"))?;
    let version = descriptor.device_version();
    let revision = (u16::from(version.major() / 10) << 12)
        | (u16::from(version.major() % 10) << 8)
        | (u16::from(version.minor()) << 4)
        | u16::from(version.sub_minor());
    crate::settings::install_descriptor_identity(
        descriptor.vendor_id(),
        descriptor.product_id(),
        revision,
    );
    Ok(ClaimedHandle {
        #[cfg(target_os = "macos")]
        owner: None,
        itf_mode: ItfModeLease::new(),
        adc3_power: Adc3PowerLease::new(device.fd, Some(handle.clone()), device._fd_owner.clone()),
        device_fd: device.fd,
        context,
        handle,
        claimed_interfaces: Arc::clone(&device.claimed_interfaces),
        active_interfaces: Mutex::new(Vec::new()),
        fd_owner: device._fd_owner.clone(),
    })
}

pub(super) fn cleanup_claimed_handle(
    claimed_handle: ClaimedHandle,
    active_interface: Option<u8>,
    keep_locked: bool,
) -> bool {
    let mut active = claimed_handle.active_interfaces.lock().clone();
    if let Some(interface_number) = active_interface {
        if !active.contains(&interface_number) {
            active.push(interface_number);
        }
    }
    let mut reset_ok = true;
    for interface_number in active.into_iter().rev() {
        if claimed_handle
            .set_alternate_setting(interface_number, 0)
            .is_err()
        {
            reset_ok = false;
        }
    }

    if reset_ok {
        if let Err(error) = claimed_handle.itf_mode.restore() {
            reset_ok = false;
            set_last_error(Some(format!("ITF PCM mode could not be restored: {error}")));
            log_warn!("[USB] ITF PCM recovery failed; releasing ownership: {error}");
        }
    } else {
        // Do not send vendor mode writes after an unconfirmed interface reset
        // or disconnect, including from the lease's Drop fallback.
        claimed_handle.itf_mode.disarm();
    }
    if reset_ok {
        if let Err(error) = claimed_handle.adc3_power.restore() {
            set_last_error(Some(format!("ADC3 power hand-back failed: {error}")));
            reset_ok = false;
        }
    } else {
        claimed_handle.adc3_power.disarm();
    }
    if reset_ok && keep_locked && current_lock_requested_for_fd(claimed_handle.device_fd) {
        *IDLE_LOCK.lock() = Some(claimed_handle.into_idle_lock());
    }
    reset_ok
}

#[cfg(target_os = "macos")]
fn host_usb_error(operation: impl Into<String>, error: rusb::Error) -> crate::Error {
    let kind = host_usb_failure_kind(error);
    crate::Error::HostUsb {
        operation: operation.into(),
        kind,
        detail: error.to_string(),
    }
}

#[cfg(target_os = "macos")]
fn host_usb_failure_kind(error: rusb::Error) -> crate::HostUsbFailureKind {
    match error {
        rusb::Error::Access => crate::HostUsbFailureKind::AccessDenied,
        rusb::Error::Busy => crate::HostUsbFailureKind::InUse,
        rusb::Error::NoDevice => crate::HostUsbFailureKind::DeviceLost,
        rusb::Error::NotSupported => crate::HostUsbFailureKind::NotSupported,
        _ => crate::HostUsbFailureKind::Other,
    }
}

#[cfg(target_os = "macos")]
pub(crate) fn open_darwin_device(
    attachment: &crate::darwin_discovery::Attachment,
    allow_capture: bool,
) -> Result<(UsbDevice, crate::DeviceInfo), crate::Error> {
    let usb = attachment.device();
    let before = usb.active_config_descriptor().map_err(|error| {
        crate::Error::Claim(format!("active configuration before claim: {error}"))
    })?;
    let expected = before.number();
    let mut interfaces = Vec::new();
    for interface in before.interfaces() {
        if interface.descriptors().any(|alt| {
            (alt.class_code() == USB_CLASS_AUDIO
                && matches!(
                    alt.sub_class_code(),
                    USB_SUBCLASS_AUDIOCONTROL | USB_SUBCLASS_AUDIOSTREAMING
                ))
                || (alt.class_code() == 0xff
                    && alt.endpoint_descriptors().any(|ep| {
                        ep.direction() == Direction::Out
                            && ep.transfer_type() == TransferType::Isochronous
                    }))
        }) {
            interfaces.push(interface.number());
        }
    }
    if interfaces.is_empty() {
        return Err(crate::Error::Claim(
            "active configuration has no eligible playback interfaces".into(),
        ));
    }
    let handle = Arc::new(
        usb.open()
            .map_err(|error| host_usb_error("open USB attachment", error))?,
    );
    let mut owner = DarwinOwner {
        handle: handle.clone(),
        claimed: Vec::new(),
        captured: false,
    };
    // Always try the ordinary claims first. Whole-device takeover is only a
    // recovery path for an explicit opt-in and an ownership/access failure.
    let claim_failure = interfaces.iter().copied().find_map(|interface| {
        log_info!("[USB] host_usb_claim phase=ordinary interface={interface} result=attempt");
        match handle.claim_interface(interface) {
            Ok(()) => {
                owner.claimed.push(interface);
                log_info!("[USB] host_usb_claim phase=ordinary interface={interface} result=success");
                None
            }
            Err(error) => {
                log_warn!("[USB] host_usb_claim phase=ordinary interface={interface} result=error kind={:?} detail={error}", host_usb_failure_kind(error));
                Some((interface, error))
            }
        }
    });
    if let Some((failed_interface, initial_error)) = claim_failure {
        if !allow_capture || !matches!(initial_error, rusb::Error::Access | rusb::Error::Busy) {
            return Err(host_usb_error(
                format!("ordinary claim USB interface {failed_interface}"),
                initial_error,
            ));
        }
        // Never detach while any successful ordinary claim remains. Keep failed
        // releases in the owner so Drop will still attempt to return them.
        let mut rollback_failure = None;
        for index in (0..owner.claimed.len()).rev() {
            let interface = owner.claimed[index];
            log_info!("[USB] host_usb_claim phase=rollback interface={interface} result=attempt");
            match handle.release_interface(interface) {
                Ok(()) => {
                    owner.claimed.remove(index);
                    log_info!(
                        "[USB] host_usb_claim phase=rollback interface={interface} result=success"
                    );
                }
                Err(error) => {
                    log_error!("[USB] host_usb_claim phase=rollback interface={interface} result=error kind={:?} detail={error}", host_usb_failure_kind(error));
                    if rollback_failure.is_none() {
                        rollback_failure = Some((interface, error));
                    }
                }
            }
        }
        if let Some((interface, error)) = rollback_failure {
            return Err(crate::Error::HostUsb {
                operation: format!("rollback USB interface {interface} after ordinary claim {failed_interface} failed"),
                kind: host_usb_failure_kind(error),
                detail: format!("rollback: {error}; original claim: {initial_error}"),
            });
        }
        log_info!("[USB] host_usb_claim phase=capture interface={failed_interface} result=attempt scope=whole_device");
        if let Err(error) = handle.detach_kernel_driver(failed_interface) {
            log_warn!(
                "[USB] host_usb_claim phase=capture result=error kind={:?} detail={error}",
                host_usb_failure_kind(error)
            );
            return Err(crate::Error::HostUsb {
                operation: "whole-device kernel capture (may require root or Apple's device-access entitlement)".into(),
                kind: host_usb_failure_kind(error),
                detail: format!("capture: {error}; original claim interface {failed_interface}: {initial_error}"),
            });
        }
        owner.captured = true;
        log_info!("[USB] host_usb_claim phase=capture result=success scope=whole_device");
        let after_capture = handle
            .device()
            .active_config_descriptor()
            .map_err(|error| {
                crate::Error::Claim(format!("active configuration after capture: {error}"))
            })?;
        if after_capture.number() != expected {
            return Err(crate::Error::Claim(format!(
                "active configuration changed from {expected} to {} during capture",
                after_capture.number()
            )));
        }
        for &interface in &interfaces {
            log_info!("[USB] host_usb_claim phase=reclaim interface={interface} result=attempt");
            match handle.claim_interface(interface) {
                Ok(()) => {
                    owner.claimed.push(interface);
                    log_info!(
                        "[USB] host_usb_claim phase=reclaim interface={interface} result=success"
                    );
                }
                Err(error) => {
                    log_warn!("[USB] host_usb_claim phase=reclaim interface={interface} result=error kind={:?} detail={error}", host_usb_failure_kind(error));
                    return Err(crate::Error::HostUsb { operation: format!("claim USB interface {interface} after capture"), kind: host_usb_failure_kind(error), detail: format!("reclaim: {error}; original claim interface {failed_interface}: {initial_error}") });
                }
            }
        }
    }
    let actual = handle
        .device()
        .active_config_descriptor()
        .map_err(|error| {
            crate::Error::Claim(format!("active configuration after claim: {error}"))
        })?;
    if actual.number() != expected {
        return Err(crate::Error::Claim(format!(
            "active configuration changed from {expected} to {} during claim",
            actual.number()
        )));
    }
    let descriptor = handle
        .device()
        .device_descriptor()
        .map_err(|error| crate::Error::Claim(format!("device descriptor after claim: {error}")))?;
    let product_name = handle
        .read_product_string_ascii(&descriptor)
        .unwrap_or_else(|_| {
            format!(
                "USB DAC {:04x}:{:04x}",
                descriptor.vendor_id(),
                descriptor.product_id()
            )
        });
    let manufacturer = handle.read_manufacturer_string_ascii(&descriptor).ok();
    let mut info = crate::DeviceInfo::new(
        descriptor.vendor_id(),
        descriptor.product_id(),
        product_name.clone(),
    )
    .with_claimed_interfaces(interfaces.clone());
    info.configuration_id = Some(expected);
    info.manufacturer_name = manufacturer;
    let owner = Arc::new(owner);
    let device = UsbDevice {
        fd: DeviceKey::Darwin(usb.as_raw() as usize),
        darwin: Some(owner),
        _fd_owner: None,
        claimed_interfaces: interfaces.into(),
        vendor_id: descriptor.vendor_id(),
        product_id: descriptor.product_id(),
        product_name,
        device_name: None,
    };
    Ok((device, info))
}
