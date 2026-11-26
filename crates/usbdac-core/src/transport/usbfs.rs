use super::*;

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

#[derive(Debug, Clone)]
pub struct UsbDevice {
    pub fd: RawFd,
    pub(super) _fd_owner: Arc<OwnedUsbFd>,
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
            fd: duplicated_fd,
            _fd_owner: Arc::new(OwnedUsbFd(duplicated_fd)),
            claimed_interfaces: claimed_interfaces.into(),
            vendor_id,
            product_id,
            product_name,
            device_name,
        })
    }

    pub(super) fn feature_unit_control(
        &self,
        request_type: u8,
        request: u8,
        value: u16,
        index: u16,
        data: &mut [u8],
    ) -> std::io::Result<usize> {
        if !self.claimed_interfaces.contains(&(index as u8)) {
            return Err(std::io::Error::other("AudioControl interface is not claimed"));
        }
        let length = u16::try_from(data.len()).map_err(std::io::Error::other)?;
        let transfer = UsbfsControlTransfer {
            request_type,
            request,
            value,
            index,
            length,
            timeout_ms: 1000,
            data: data.as_mut_ptr().cast(),
        };
        // This synchronous ioctl does not reap URBs. A separate libusb control
        // context on the same fd could consume the output context's completions.
        let result = unsafe { libc::ioctl(self.fd, USBDEVFS_CONTROL, &transfer) };
        if result < 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(result as usize)
    }
}


pub(super) struct IdleLock {
    pub(super) device_fd: RawFd,
    pub(super) context: Context,
    pub(super) handle: DeviceHandle<Context>,
    pub(super) claimed_interfaces: Arc<[u8]>,
}

pub(super) struct ClaimedHandle {
    pub(super) device_fd: RawFd,
    pub(super) context: Context,
    pub(super) handle: DeviceHandle<Context>,
    pub(super) claimed_interfaces: Arc<[u8]>,
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
pub(super) const USBDEVFS_SETINTERFACE: IoctlRequest = ioctl_nr::<UsbfsSetInterface>(true, false, 4);

impl From<IdleLock> for ClaimedHandle {
    fn from(value: IdleLock) -> Self {
        Self {
            device_fd: value.device_fd,
            context: value.context,
            handle: value.handle,
            claimed_interfaces: value.claimed_interfaces,
        }
    }
}

impl ClaimedHandle {
    pub(super) fn into_idle_lock(self) -> IdleLock {
        IdleLock {
            device_fd: self.device_fd,
            context: self.context,
            handle: self.handle,
            claimed_interfaces: self.claimed_interfaces,
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

    pub(super) fn set_alternate_setting(&self, interface_number: u8, alternate_setting: u8) -> Result<(), String> {
        self.ensure_interface_claimed(interface_number)?;
        let request = UsbfsSetInterface {
            interface: interface_number.into(),
            altsetting: alternate_setting.into(),
        };
        // Android's fd and our dup share the usbfs claims. libusb's own
        // per-handle claim table does not, so its alt-setting helper refuses
        // these interfaces. Use the same kernel ioctl Android uses instead.
        let result = unsafe { libc::ioctl(self.device_fd, USBDEVFS_SETINTERFACE, &request) };
        if result < 0 {
            return Err(std::io::Error::last_os_error().to_string());
        }
        Ok(())
    }
}

pub(super) fn open_claimed_usb_handle(
    device: &UsbDevice,
) -> Result<ClaimedHandle, String> {
    if let Some(lock) = IDLE_LOCK.lock().take() {
        if lock.device_fd == device.fd {
            return Ok(lock.into());
        }
    }

    wrap_android_usb_handle(device)
}

pub(super) fn wrap_android_usb_handle(
    device: &UsbDevice,
) -> Result<ClaimedHandle, String> {
    prepare_direct_usb_libusb();

    let context =
        Context::new().map_err(|error| format!("Failed to create libusb context: {}", error))?;
    let handle = unsafe { context.open_device_with_fd(device.fd) }
        .map_err(|error| format!("Failed to wrap USB file descriptor: {}", error))?;

    Ok(ClaimedHandle {
        device_fd: device.fd,
        context,
        handle,
        claimed_interfaces: Arc::clone(&device.claimed_interfaces),
    })
}

pub(super) fn cleanup_claimed_handle(
    claimed_handle: ClaimedHandle,
    active_interface: Option<u8>,
    keep_locked: bool,
) {
    if let Some(interface_number) = active_interface {
        let _ = claimed_handle.set_alternate_setting(interface_number, 0);
    }

    if keep_locked && current_lock_requested_for_fd(claimed_handle.device_fd) {
        *IDLE_LOCK.lock() = Some(claimed_handle.into_idle_lock());
    }
}

