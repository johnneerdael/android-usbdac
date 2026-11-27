use super::*;
use alternate_controls::{
    AlternateControls, AS_ACTIVE_ALT_SETTING_CONTROL, AS_VALID_ALT_SETTINGS_CONTROL,
    MAX_VALID_ALTERNATES_REPLY_LEN,
};

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
            return Err(std::io::Error::other(
                "AudioControl interface is not claimed",
            ));
        }
        let length = u16::try_from(data.len()).map_err(std::io::Error::other)?;
        #[cfg(test)]
        {
            if let Some(result) =
                super::fake_dac::dispatch(self.fd, request_type, request, value, index, data)
            {
                return result;
            }
        }
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
    pub(super) active_interfaces: Mutex<Vec<u8>>,
}

pub(super) struct ClaimedHandle {
    // Drop before the libusb handle/context; armed only by exclusive setup.
    pub(super) itf_mode: ItfModeLease,
    pub(super) adc3_power: Adc3PowerLease,
    pub(super) device_fd: RawFd,
    pub(super) context: Context,
    pub(super) handle: DeviceHandle<Context>,
    pub(super) claimed_interfaces: Arc<[u8]>,
    pub(super) active_interfaces: Mutex<Vec<u8>>,
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
            itf_mode: ItfModeLease::new(value.device_fd),
            adc3_power: Adc3PowerLease::new(value.device_fd),
            device_fd: value.device_fd,
            context: value.context,
            handle: value.handle,
            claimed_interfaces: value.claimed_interfaces,
            active_interfaces: value.active_interfaces,
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
            active_interfaces: self.active_interfaces,
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
        let result = unsafe { libc::ioctl(self.device_fd, USBDEVFS_SETINTERFACE, &request) };
        if result < 0 {
            return Err(std::io::Error::last_os_error().to_string());
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
                format!("USB interface {interface_number} alternate {alternate_setting} disappeared")
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
        let result = unsafe { libc::ioctl(self.device_fd, USBDEVFS_CONTROL, &request) };
        let error = (result < 0).then(std::io::Error::last_os_error);
        itf_class_control_delay(&self.handle);
        if let Some(error) = error {
            return Err(format!(
                "USB interface {interface_number} alternate control {value:#06x}: {error}"
            ));
        }
        let length = result as usize;
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
    prepare_direct_usb_libusb();

    let context =
        Context::new().map_err(|error| format!("Failed to create libusb context: {}", error))?;
    let handle = unsafe { context.open_device_with_fd(device.fd) }
        .map_err(|error| format!("Failed to wrap USB file descriptor: {}", error))?;

    Ok(ClaimedHandle {
        itf_mode: ItfModeLease::new(device.fd),
        adc3_power: Adc3PowerLease::new(device.fd),
        device_fd: device.fd,
        context,
        handle,
        claimed_interfaces: Arc::clone(&device.claimed_interfaces),
        active_interfaces: Mutex::new(Vec::new()),
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
