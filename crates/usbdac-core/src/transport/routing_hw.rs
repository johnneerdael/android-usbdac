//! Session-bound standard UAC1/UAC2/full ADC3 routing. No vendor commands or extra claims.

use super::*;
use crate::routing::{
    self, Access, ControlKey, ControlRequest, InputCluster, InvalidRoute, MixerUnit, RouteChange,
    RouteReport, RouteValue, RoutingError, RoutingInventory, RoutingIo, RoutingUnit, SelectorUnit,
    TransactionError, MAX_ROUTE_CHANGES,
};
use std::collections::BTreeMap;

const MAX_TOPOLOGY_DEPTH: usize = 16;
const MAX_RECORDED_ROUTES: usize = 1024;

/// Optimistic concurrency check and desired value for one routing control.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RouteEdit {
    pub key: ControlKey,
    pub expected: RouteValue,
    pub value: RouteValue,
}

/// Immutable discovery result, tied to an attachment and active configuration.
/// Keeping a snapshot does not retain the host's USB claims after Dac::close.
#[derive(Debug, Clone)]
pub struct RoutingSnapshot {
    device: UsbDevice,
    configuration: u8,
    descriptors: Vec<u8>,
    inventory: RoutingInventory,
    selectors: Vec<RouteChange>,
    hc_clusters: BTreeMap<(u8, u16), RoutingCluster>,
}

impl RoutingSnapshot {
    pub fn configuration(&self) -> u8 {
        self.configuration
    }
    pub fn inventory(&self) -> &RoutingInventory {
        &self.inventory
    }
    pub fn selector_values(&self) -> &[RouteChange] {
        &self.selectors
    }
}

/// Typed failures preserve failed-change and rollback information.
#[derive(Debug)]
pub enum RoutingSessionError {
    Session(Error),
    Invalid(InvalidRoute),
    UnsupportedTopology(String),
    Drift(String),
    Control(RoutingError<String>),
    Transaction(TransactionError<String>),
    Scarlett(crate::scarlett::ReadError<String>),
    ScarlettMix(crate::scarlett::MixWriteError<String>),
    ScarlettMux(crate::scarlett::MuxWriteError<String>),
}

impl std::fmt::Display for RoutingSessionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for RoutingSessionError {}

/// Return one unambiguous, exact-model optional vendor claim. This is admission
/// of an already opted-in host claim, never an instruction to acquire a claim.
pub(super) fn scarlett_claim_interface(
    config: &ConfigDescriptor,
    vendor: u16,
    product: u16,
) -> Option<u8> {
    scarlett_endpoint(config, vendor, product).map(|(interface, _)| interface)
}

/// Descriptor-only host preclaim proof. Never claims an interface or initializes Scarlett.
pub(crate) fn scarlett_preflight(
    fd: RawFd,
    info: &crate::DeviceInfo,
) -> Result<Option<crate::scarlett::ReadCapability>, Error> {
    if !cfg!(any(target_os = "linux", target_os = "android")) {
        return Err(Error::PlatformUnsupported);
    }
    let device = UsbDevice::try_new(
        fd,
        info.vendor_id,
        info.product_id,
        info.product_name.clone(),
        info.device_name.clone(),
        Vec::new(),
    )
    .map_err(Error::Claim)?;
    let wrapped = wrap_android_usb_handle(&device).map_err(Error::Claim)?;
    let usb = wrapped.handle.device();
    let identity = usb
        .device_descriptor()
        .map_err(|e| Error::Control(e.to_string()))?;
    if identity.vendor_id() != info.vendor_id || identity.product_id() != info.product_id {
        return Err(Error::Claim(
            "host and USB device identities differ during Scarlett preflight".into(),
        ));
    }
    let config = usb
        .active_config_descriptor()
        .map_err(|e| Error::Control(e.to_string()))?;
    if info.configuration_id != Some(config.number()) {
        return Err(Error::Claim(
            "Scarlett preflight requires the selected active configuration".into(),
        ));
    }
    let Some(candidate) =
        crate::scarlett::CandidateModel::recognize(identity.vendor_id(), identity.product_id())
    else {
        return Ok(None);
    };
    Ok(
        scarlett_endpoint(&config, identity.vendor_id(), identity.product_id()).map(
            |(interface, notification_endpoint)| crate::scarlett::ReadCapability {
                candidate,
                configuration: config.number(),
                interface,
                notification_endpoint,
                interface_claimed: false,
            },
        ),
    )
}

fn scarlett_endpoint(config: &ConfigDescriptor, vendor: u16, product: u16) -> Option<(u8, u8)> {
    crate::scarlett::CandidateModel::recognize(vendor, product)?;
    let mut found = None;
    for interface in config.interfaces() {
        for descriptor in interface.descriptors() {
            if descriptor.setting_number() != 0 || descriptor.class_code() != 0xff {
                continue;
            }
            // Reject ambiguity, including a second vendor interface with a
            // different shape, instead of guessing Focusrite Control ownership.
            if found.is_some() || descriptor.num_endpoints() != 1 {
                return None;
            }
            let endpoint = descriptor.endpoint_descriptors().next()?;
            if endpoint.transfer_type() != TransferType::Interrupt
                || endpoint.direction() != Direction::In
                || endpoint.max_packet_size() != 8
                || endpoint.interval() == 0
                || endpoint.address() & 0x70 != 0
                || endpoint.address() & 0x0f == 0
            {
                return None;
            }
            found = Some((descriptor.interface_number(), endpoint.address()));
        }
    }
    found
}

fn scarlett_capability(
    device: &UsbDevice,
    configuration: u8,
) -> Result<Option<crate::scarlett::ReadCapability>, RoutingSessionError> {
    let idle = IDLE_LOCK.lock();
    let idle = idle
        .as_ref()
        .filter(|idle| idle.device_fd == device.fd)
        .ok_or_else(|| {
            RoutingSessionError::Session(Error::InvalidState(
                "Scarlett discovery requires the idle USB owner".into(),
            ))
        })?;
    let usb_device = idle.handle.device();
    let identity = usb_device
        .device_descriptor()
        .map_err(|e| RoutingSessionError::Session(Error::Control(e.to_string())))?;
    if identity.vendor_id() != device.vendor_id || identity.product_id() != device.product_id {
        return Err(RoutingSessionError::Drift(
            "host and USB device identities differ".into(),
        ));
    }
    let Some(candidate) =
        crate::scarlett::CandidateModel::recognize(identity.vendor_id(), identity.product_id())
    else {
        return Ok(None);
    };
    let config = usb_device
        .active_config_descriptor()
        .map_err(|e| RoutingSessionError::Session(Error::Control(e.to_string())))?;
    if config.number() != configuration {
        return Err(RoutingSessionError::Drift(
            "configuration changed during Scarlett discovery".into(),
        ));
    }
    let Some((interface, notification_endpoint)) =
        scarlett_endpoint(&config, identity.vendor_id(), identity.product_id())
    else {
        return Ok(None);
    };
    Ok(Some(crate::scarlett::ReadCapability {
        candidate,
        configuration,
        interface,
        notification_endpoint,
        interface_claimed: device.claimed_interfaces.contains(&interface),
    }))
}

/// Descriptor-only read capability; does not initialize the vendor protocol.
pub(crate) fn scarlett_discover(
) -> Result<Option<crate::scarlett::ReadCapability>, RoutingSessionError> {
    let _gate = USB_SESSION_GATE.lock();
    let _controls = HARDWARE_VOLUME_MUTEX.lock();
    let device = idle_device()?;
    let (configuration, _) = active_descriptors(&device)?;
    scarlett_capability(&device, configuration)
}

struct UsbScarlettIo<'a> {
    device: &'a UsbDevice,
    handle: &'a DeviceHandle<Context>,
    interface: u8,
    endpoint: u8,
    max_payload: usize,
}

impl crate::scarlett::ReadIo for UsbScarlettIo<'_> {
    type Error = String;
    fn initial_read(&mut self, bytes: &mut [u8; 24]) -> Result<usize, String> {
        self.device
            .feature_unit_control(0xa1, 0, 0, u16::from(self.interface), bytes)
            .map_err(|e| e.to_string())
    }
    fn prepare_notifications(&mut self) -> Result<(), String> {
        let deadline = Instant::now() + Duration::from_millis(20);
        // Drain outstanding ACKs before either initialization request. The
        // synchronous interrupt helper owns/retires its transfer before return.
        for _ in 0..16 {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Ok(());
            }
            let mut data = [0; 8];
            match self.handle.read_interrupt(
                self.endpoint,
                &mut data,
                remaining.max(Duration::from_millis(1)),
            ) {
                Ok(8) => {}
                Ok(_) => return Err("short Scarlett initialization notification".into()),
                Err(rusb::Error::Timeout) => return Ok(()),
                Err(e) => return Err(e.to_string()),
            }
        }
        Err("Scarlett notification drain exceeded its bounded packet budget".into())
    }
    fn send_request(&mut self, bytes: &[u8]) -> Result<usize, String> {
        if bytes.len() < 16 || bytes.len() > 16 + self.max_payload {
            return Err("unbounded Scarlett request refused".into());
        }
        let mut request = bytes.to_vec();
        self.device
            .feature_unit_control(0x21, 2, 0, u16::from(self.interface), &mut request)
            .map_err(|e| e.to_string())
    }
    fn next_notification(
        &mut self,
        bytes: &mut [u8; 8],
        timeout: Duration,
    ) -> Result<usize, String> {
        self.handle
            .read_interrupt(self.endpoint, bytes, timeout.max(Duration::from_millis(1)))
            .map_err(|e| e.to_string())
    }
    fn receive_response(&mut self, bytes: &mut [u8]) -> Result<usize, String> {
        if bytes.len() < 16 || bytes.len() > 16 + self.max_payload {
            return Err("unbounded Scarlett response refused".into());
        }
        self.device
            .feature_unit_control(0xa1, 3, 0, u16::from(self.interface), bytes)
            .map_err(|e| e.to_string())
    }
}

/// Explicit vendor read: initialize only the already claimed, exact candidate,
/// then admit firmware and read mux-zero/mixer state. Never SET_MUX/SET_MIX.
pub(crate) fn scarlett_read() -> Result<crate::scarlett::RoutingReadback, RoutingSessionError> {
    let _gate = USB_SESSION_GATE.lock();
    let _controls = HARDWARE_VOLUME_MUTEX.lock();
    let device = idle_device()?;
    let (configuration, descriptors) = active_descriptors(&device)?;
    let readback = with_scarlett(&device, configuration, |session| {
        session.read_all().map_err(RoutingSessionError::Scarlett)
    })?;
    let (after, raw) = active_descriptors(&device)?;
    if configuration != after || descriptors != raw {
        return Err(RoutingSessionError::Drift(
            "configuration changed during Scarlett routing read".into(),
        ));
    }
    Ok(readback)
}

struct RecordedScarlett {
    device: UsbDevice,
    configuration: u8,
    descriptors: Vec<u8>,
    rows: BTreeMap<u8, (crate::scarlett::MixReadback, crate::scarlett::MixReadback)>,
    mux: Option<(crate::scarlett::MuxSnapshot, crate::scarlett::MuxSnapshot)>,
}
static ORIGINAL_SCARLETT: Mutex<Option<RecordedScarlett>> = Mutex::new(None);

/// The caller owns session/volume serialization. Recheck exact claim and USB
/// identity for every operation, including configured hand-back.
fn with_scarlett<T>(
    device: &UsbDevice,
    configuration: u8,
    operation: impl FnOnce(
        &mut crate::scarlett::ReadSession<UsbScarlettIo<'_>>,
    ) -> Result<T, RoutingSessionError>,
) -> Result<T, RoutingSessionError> {
    with_scarlett_io(device, configuration, false, |io, candidate| {
        let mut session = crate::scarlett::ReadSession::initialize(io, candidate)
            .map_err(RoutingSessionError::Scarlett)?;
        operation(&mut session)
    })
}

/// Share exact identity/claim admission between legacy controls and FCP reads.
fn with_scarlett_io<T>(
    device: &UsbDevice,
    configuration: u8,
    fcp: bool,
    operation: impl FnOnce(UsbScarlettIo<'_>, crate::scarlett::CandidateModel) -> Result<T, RoutingSessionError>,
) -> Result<T, RoutingSessionError> {
    let capability = scarlett_capability(device, configuration)?.ok_or_else(|| {
        RoutingSessionError::UnsupportedTopology("no exact Scarlett candidate".into())
    })?;
    if capability.candidate.is_fcp() != fcp {
        let message = if fcp {
            "firmware-described routing requires a modern FCP interface"
        } else {
            "modern FCP routing is available through fcp_routing_read"
        };
        return Err(RoutingSessionError::UnsupportedTopology(message.into()));
    }
    if !capability.interface_claimed {
        return Err(RoutingSessionError::Session(Error::Claim(
            "Focusrite routing requires an explicit vendor-interface claim".into(),
        )));
    }
    let idle = IDLE_LOCK.lock();
    let idle = idle
        .as_ref()
        .filter(|idle| idle.device_fd == device.fd)
        .ok_or_else(|| {
            RoutingSessionError::Session(Error::InvalidState(
                "Scarlett mixer lost idle USB owner".into(),
            ))
        })?;
    let io = UsbScarlettIo {
        device,
        handle: &idle.handle,
        interface: capability.interface,
        endpoint: capability.notification_endpoint,
        max_payload: if fcp { crate::scarlett::FCP_MAX_PAYLOAD } else { crate::scarlett::MAX_PAYLOAD },
    };
    operation(io, capability.candidate)
}

/// Read only firmware-defined FCP topology on a proven, explicitly claimed idle owner.
pub(crate) fn fcp_read() -> Result<crate::scarlett::FcpReadback, RoutingSessionError> {
    let _gate = USB_SESSION_GATE.lock();
    let _controls = HARDWARE_VOLUME_MUTEX.lock();
    let device = idle_device()?;
    let (configuration, descriptors) = active_descriptors(&device)?;
    let readback = with_scarlett_io(&device, configuration, true, |io, candidate| {
        crate::scarlett::read_fcp(io, candidate).map_err(RoutingSessionError::Scarlett)
    })?;
    let (after, raw) = active_descriptors(&device)?;
    if configuration != after || descriptors != raw {
        return Err(RoutingSessionError::Drift("configuration changed during FCP routing read".into()));
    }
    Ok(readback)
}

pub(crate) fn scarlett_mix_apply(
    edit: &crate::scarlett::MixRowEdit,
) -> Result<crate::scarlett::MixWriteReport, RoutingSessionError> {
    let _gate = USB_SESSION_GATE.lock();
    let _controls = HARDWARE_VOLUME_MUTEX.lock();
    let device = idle_device()?;
    let (configuration, descriptors) = active_descriptors(&device)?;
    let mut records = ORIGINAL_SCARLETT.lock();
    if let Some(record) = records.as_ref() {
        if !Arc::ptr_eq(&record.device._fd_owner, &device._fd_owner)
            || record.configuration != configuration
            || record.descriptors != descriptors
        {
            return Err(RoutingSessionError::Drift(
                "Scarlett attachment/configuration changed since mixer write".into(),
            ));
        }
        if let Some((_, last)) = record.rows.get(&edit.expected.output) {
            if last != &edit.expected {
                return Err(RoutingSessionError::Drift(
                    "Scarlett row differs from last verified library write".into(),
                ));
            }
        }
    }
    let report = match with_scarlett(&device, configuration, |session| {
        session
            .apply_mix(edit)
            .map_err(RoutingSessionError::ScarlettMix)
    }) {
        Ok(report) => report,
        Err(error) => {
            if matches!(&error, RoutingSessionError::ScarlettMix(failure) if failure.rollback.is_some())
            {
                invalidate_route_caches();
            }
            return Err(error);
        }
    };
    // Record before the post-write descriptor check so a confirmed row is never
    // forgotten if configuration inspection itself fails after the write.
    if report.original != report.verified {
        invalidate_route_caches();
        let record = records.get_or_insert_with(|| RecordedScarlett {
            device: device.clone(),
            configuration,
            descriptors: descriptors.clone(),
            rows: BTreeMap::new(),
            mux: None,
        });
        record
            .rows
            .entry(report.original.output)
            .and_modify(|row| row.1 = report.verified.clone())
            .or_insert((report.original.clone(), report.verified.clone()));
    }
    let (after, raw) = active_descriptors(&device)?;
    if configuration != after || descriptors != raw {
        return Err(RoutingSessionError::Drift(
            "configuration changed during Scarlett mixer write".into(),
        ));
    }
    Ok(report)
}

/// Explicit capability probe and complete per-bank read; legacy reads do not
/// invoke these optional commands, so unsupported replies leave those usable.
pub(crate) fn scarlett_mux_read() -> Result<crate::scarlett::MuxSnapshot, RoutingSessionError> {
    let _gate = USB_SESSION_GATE.lock();
    let _controls = HARDWARE_VOLUME_MUTEX.lock();
    let device = idle_device()?;
    let (configuration, descriptors) = active_descriptors(&device)?;
    let snapshot = with_scarlett(&device, configuration, |session| {
        session
            .read_mux_banks()
            .map_err(RoutingSessionError::Scarlett)
    })?;
    let (after, raw) = active_descriptors(&device)?;
    if after != configuration || raw != descriptors {
        return Err(RoutingSessionError::Drift(
            "configuration changed during Scarlett bank read".into(),
        ));
    }
    Ok(snapshot)
}

pub(crate) fn scarlett_mux_apply(
    edit: &crate::scarlett::MuxEdit,
) -> Result<crate::scarlett::MuxWriteReport, RoutingSessionError> {
    let _gate = USB_SESSION_GATE.lock();
    let _controls = HARDWARE_VOLUME_MUTEX.lock();
    let device = idle_device()?;
    let (configuration, descriptors) = active_descriptors(&device)?;
    let mut records = ORIGINAL_SCARLETT.lock();
    if let Some(record) = records.as_ref() {
        if !Arc::ptr_eq(&record.device._fd_owner, &device._fd_owner)
            || record.configuration != configuration
            || record.descriptors != descriptors
        {
            return Err(RoutingSessionError::Drift(
                "Scarlett attachment/configuration changed since vendor write".into(),
            ));
        }
        if let Some((_, last)) = &record.mux {
            if last != &edit.expected {
                return Err(RoutingSessionError::Drift(
                    "Scarlett banks differ from last verified library write".into(),
                ));
            }
        }
    }
    let report = match with_scarlett(&device, configuration, |session| {
        session
            .apply_mux(edit)
            .map_err(RoutingSessionError::ScarlettMux)
    }) {
        Ok(report) => report,
        Err(error) => {
            if matches!(&error, RoutingSessionError::ScarlettMux(failure) if !failure.rollback.is_empty())
            {
                invalidate_route_caches();
            }
            return Err(error);
        }
    };
    if report.original != report.verified {
        invalidate_route_caches();
        let record = records.get_or_insert_with(|| RecordedScarlett {
            device: device.clone(),
            configuration,
            descriptors: descriptors.clone(),
            rows: BTreeMap::new(),
            mux: None,
        });
        match &mut record.mux {
            Some((_, last)) => *last = report.verified.clone(),
            entry @ None => *entry = Some((report.original.clone(), report.verified.clone())),
        }
    }
    let (after, raw) = active_descriptors(&device)?;
    if configuration != after || descriptors != raw {
        return Err(RoutingSessionError::Drift(
            "configuration changed during Scarlett mux write".into(),
        ));
    }
    Ok(report)
}

fn finish_scarlett_hand_back() {
    let Some(record) = ORIGINAL_SCARLETT.lock().take() else {
        return;
    };
    if settings().config.hand_back != crate::HandBack::RestoreOriginal {
        return;
    }
    let Some(device) = SESSION_STATE.lock().as_ref().map(|s| s.device.clone()) else {
        return;
    };
    let restore = || -> Result<(), RoutingSessionError> {
        if !Arc::ptr_eq(&record.device._fd_owner, &device._fd_owner) {
            return Err(RoutingSessionError::Drift(
                "Scarlett attachment changed before hand-back".into(),
            ));
        }
        let (number, raw) = active_descriptors(&device)?;
        if number != record.configuration || raw != record.descriptors {
            return Err(RoutingSessionError::Drift(
                "Scarlett configuration changed before hand-back".into(),
            ));
        }
        with_scarlett(&device, number, |session| {
            if let Some((original, last)) = &record.mux {
                if let Err(error) = session.replace_mux(last, original) {
                    log_warn!("[USB] Scarlett mux banks not restored: {:?}", error);
                }
            }
            for (original, last) in record.rows.values() {
                // A fresh whole-row equality check prevents overwriting any
                // external edit, even in an otherwise untouched coefficient.
                if let Err(error) = session.replace_mix(last, original) {
                    log_warn!(
                        "[USB] Scarlett mixer row {} not restored: {:?}",
                        original.output,
                        error
                    );
                    // A poisoned failed recovery forbids subsequent writes.
                }
            }
            Ok(())
        })
    };
    if let Err(error) = restore() {
        log_warn!("[USB] Original Scarlett mixer not restored: {error}");
    }
}

struct UsbRoutingIo<'a> {
    device: &'a UsbDevice,
    descriptors: &'a [u8],
}

/// Enforce the small set of setup packets this adapter is allowed to issue.
#[cfg(test)]
fn validate_request(request: ControlRequest, input: bool, length: usize) -> Result<(), String> {
    validate_protocol_request(request, input, length, 0x20)
}
fn validate_protocol_request(
    request: ControlRequest,
    input: bool,
    length: usize,
    protocol: u8,
) -> Result<(), String> {
    if protocol == 0 {
        let valid = request.request_type == if input { 0xa1 } else { 0x21 }
            && request.index >> 8 != 0
            && usize::from(request.length) == length
            && if input {
                matches!(request.request, 0x81..=0x84)
            } else {
                request.request == 1
            }
            && if request.value == 0 {
                length == 1 && matches!(request.request, 1 | 0x81)
            } else {
                request.value >> 8 != 0
                    && request.value as u8 != 0
                    && request.value != 0xffff
                    && length == 2
            };
        return if valid {
            Ok(())
        } else {
            Err("refused non-routing UAC1 control".into())
        };
    }
    if ![0x20, 0x30].contains(&protocol) {
        return Err("unsupported routing protocol".into());
    }
    let valid_length = if request.request == 1 {
        matches!(length, 1 | 2)
    } else {
        input && length >= 2 && length <= 2 + routing::MAX_GAIN_RANGES * 6 && (length - 2) % 6 == 0
    };
    if request.request_type != if input { 0xa1 } else { 0x21 }
        || !matches!(request.request, 1 | 2)
        || (!input && request.request != 1)
        || request.value >> 8 != 1
        || request.index >> 8 == 0
        || usize::from(request.length) != length
        || !valid_length
    {
        return Err("refused non-routing or unbounded USB class control".to_string());
    }
    Ok(())
}

impl UsbRoutingIo<'_> {
    fn validate(&self, request: ControlRequest, input: bool, length: usize) -> Result<(), String> {
        let interfaces = routing_interfaces(self.descriptors, &self.device.claimed_interfaces)
            .map_err(|e| e.to_string())?;
        let protocol = interfaces
            .get(&(request.index as u8))
            .ok_or("routing interface is not claimed")?
            .0;
        validate_protocol_request(request, input, length, protocol)
    }
}

impl RoutingIo for UsbRoutingIo<'_> {
    type Error = String;
    fn read(&mut self, r: ControlRequest, bytes: &mut [u8]) -> Result<usize, String> {
        self.validate(r, true, bytes.len())?;
        self.device
            .feature_unit_control(r.request_type, r.request, r.value, r.index, bytes)
            .map_err(|e| e.to_string())
    }
    fn write(&mut self, r: ControlRequest, bytes: &[u8]) -> Result<usize, String> {
        self.validate(r, false, bytes.len())?;
        let mut data = bytes.to_vec();
        self.device
            .feature_unit_control(r.request_type, r.request, r.value, r.index, &mut data)
            .map_err(|e| e.to_string())
    }
}

fn idle_device() -> Result<UsbDevice, RoutingSessionError> {
    if USB_SESSION_CLEAR_PENDING.load(Ordering::SeqCst) {
        return Err(RoutingSessionError::Session(Error::InvalidState(
            "the DAC is being closed".into(),
        )));
    }
    if is_usb_session_active() {
        return Err(RoutingSessionError::Session(Error::InvalidState(
            "close all streams before routing controls".into(),
        )));
    }
    let state = SESSION_STATE.lock();
    let state = state.as_ref().ok_or_else(|| {
        RoutingSessionError::Session(Error::InvalidState("no DAC is open".into()))
    })?;
    if state.stream_active {
        return Err(RoutingSessionError::Session(Error::InvalidState(
            "a stream is still active".into(),
        )));
    }
    Ok(state.device.clone())
}

fn active_descriptors(device: &UsbDevice) -> Result<(u8, Vec<u8>), RoutingSessionError> {
    let idle = IDLE_LOCK.lock();
    let idle = idle
        .as_ref()
        .filter(|idle| idle.device_fd == device.fd)
        .ok_or_else(|| {
            RoutingSessionError::Session(Error::InvalidState(
                "routing requires the idle USB owner".into(),
            ))
        })?;
    let identity = idle
        .handle
        .device()
        .device_descriptor()
        .map_err(|e| RoutingSessionError::Session(Error::Control(e.to_string())))?;
    if identity.vendor_id() != device.vendor_id || identity.product_id() != device.product_id {
        return Err(RoutingSessionError::Drift(
            "routing device identity changed".into(),
        ));
    }
    let number = idle
        .handle
        .active_configuration()
        .map_err(|e| RoutingSessionError::Session(Error::Control(e.to_string())))?;
    let config = idle
        .handle
        .device()
        .active_config_descriptor()
        .map_err(|e| RoutingSessionError::Session(Error::Control(e.to_string())))?;
    if config.number() != number {
        return Err(RoutingSessionError::Drift(
            "active configuration changed during discovery".into(),
        ));
    }
    let raw = read_active_configuration_bytes(&idle.handle, &config).ok_or_else(|| {
        RoutingSessionError::Session(Error::Control(
            "failed bounded active configuration read".into(),
        ))
    })?;
    if idle
        .handle
        .active_configuration()
        .map_err(|e| RoutingSessionError::Session(Error::Control(e.to_string())))?
        != number
    {
        return Err(RoutingSessionError::Drift(
            "active configuration changed during descriptor read".into(),
        ));
    }
    Ok((number, raw))
}

/// Require complete descriptor framing; truncated topology cannot authorize I/O.
fn framed(bytes: &[u8]) -> Result<Vec<&[u8]>, RoutingSessionError> {
    let mut slices = Vec::new();
    let mut position = 0;
    while position < bytes.len() {
        let length = usize::from(*bytes.get(position).unwrap());
        if length < 2 || position + length > bytes.len() {
            return Err(RoutingSessionError::Invalid(
                InvalidRoute::MalformedDescriptor,
            ));
        }
        slices.push(&bytes[position..position + length]);
        position += length;
    }
    Ok(slices)
}

/// Preserve optional/common/unknown segments too: a selector requires equal
/// Cluster content, not merely equal counts or the parsed speaker relationships.
#[derive(Debug, Clone, PartialEq, Eq)]
struct RoutingCluster {
    parsed: crate::descriptors::uac3::Cluster,
    content: Vec<u8>,
}
impl RoutingCluster {
    fn identity(&self) -> Vec<u8> {
        // Unique descriptor IDs may differ while the described content matches.
        self.content[..4]
            .iter()
            .chain(&self.content[6..])
            .copied()
            .collect()
    }
}
fn query_routing_cluster(
    handle: &DeviceHandle<Context>,
    interface: u8,
    id: u16,
) -> Result<RoutingCluster, RoutingSessionError> {
    use crate::descriptors::uac3;
    let fail = |e: String| RoutingSessionError::UnsupportedTopology(e);
    let mut prefix = [0; 6];
    let n = handle
        .read_control(
            0xa1,
            6,
            id,
            u16::from(interface),
            &mut prefix,
            Duration::from_secs(1),
        )
        .map_err(|e| fail(e.to_string()))?;
    let header =
        uac3::parse_cluster_header(&prefix[..n], id, 16 * 1024).map_err(|e| fail(e.to_string()))?;
    let mut content = vec![0; usize::from(header.length)];
    let n = handle
        .read_control(
            0xa1,
            6,
            id,
            u16::from(interface),
            &mut content,
            Duration::from_secs(1),
        )
        .map_err(|e| fail(e.to_string()))?;
    let parsed = uac3::parse_cluster(&content[..n], header).map_err(|e| fail(e.to_string()))?;
    Ok(RoutingCluster { parsed, content })
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Cluster {
    channels: u8,
    identity: ClusterIdentity,
}
#[derive(Debug, Clone, PartialEq, Eq)]
enum ClusterIdentity {
    Explicit { locations: u32, names: u8 },
    Adc3(Vec<u8>),
}

fn explicit_cluster(
    bytes: &[u8],
    offset: usize,
    protocol: u8,
) -> Result<Cluster, RoutingSessionError> {
    let width = if protocol == 0 { 4 } else { 6 };
    let part = bytes
        .get(offset..offset + width)
        .ok_or(RoutingSessionError::Invalid(
            InvalidRoute::MalformedDescriptor,
        ))?;
    if part[0] == 0 {
        return Err(RoutingSessionError::Invalid(
            InvalidRoute::UnknownInputChannels,
        ));
    }
    let locations = if protocol == 0 {
        u32::from(u16::from_le_bytes([part[1], part[2]]))
    } else {
        u32::from_le_bytes([part[1], part[2], part[3], part[4]])
    };
    Ok(Cluster {
        channels: part[0],
        identity: ClusterIdentity::Explicit {
            locations,
            names: part[width - 1],
        },
    })
}

fn resolve_cluster_cached(
    id: u8,
    nodes: &BTreeMap<u8, &[u8]>,
    path: &mut Vec<u8>,
    cache: &mut BTreeMap<u8, Cluster>,
    protocol: u8,
    interface: u8,
    hc: &BTreeMap<(u8, u16), RoutingCluster>,
) -> Result<Cluster, RoutingSessionError> {
    if path.contains(&id) || path.len() >= MAX_TOPOLOGY_DEPTH {
        return Err(RoutingSessionError::UnsupportedTopology(
            "cyclic or excessive source topology".into(),
        ));
    }
    if let Some(cluster) = cache.get(&id) {
        return Ok(cluster.clone());
    }
    let bytes = *nodes.get(&id).ok_or_else(|| {
        RoutingSessionError::UnsupportedTopology(format!("unresolved source entity {id}"))
    })?;
    path.push(id);
    let mixer_subtype = if protocol == 0x30 { 5 } else { 4 };
    let selector_subtype = if protocol == 0x30 { 6 } else { 5 };
    let feature_subtype = if protocol == 0x30 { 7 } else { 6 };
    let result = (|| {
        let queried = |at: usize| -> Result<Cluster, RoutingSessionError> {
            let pair = bytes.get(at..at + 2).ok_or(RoutingSessionError::Invalid(
                InvalidRoute::MalformedDescriptor,
            ))?;
            let cluster_id = u16::from_le_bytes([pair[0], pair[1]]);
            let c = hc.get(&(interface, cluster_id)).ok_or_else(|| {
                RoutingSessionError::UnsupportedTopology(
                    "ADC3 routing requires a resolved HC Cluster".into(),
                )
            })?;
            let channels = u8::try_from(c.parsed.channels.len())
                .map_err(|_| RoutingSessionError::Invalid(InvalidRoute::UnknownInputChannels))?;
            if channels == 0 {
                return Err(RoutingSessionError::Invalid(
                    InvalidRoute::UnknownInputChannels,
                ));
            }
            Ok(Cluster {
                channels,
                identity: ClusterIdentity::Adc3(c.identity()),
            })
        };
        if bytes[2] == 2 {
            return match protocol {
                0 if bytes.len() == 12 => explicit_cluster(bytes, 7, protocol),
                0x20 if bytes.len() == 17 => explicit_cluster(bytes, 8, protocol),
                0x30 => {
                    crate::descriptors::uac3::parse_input_terminal(bytes)
                        .map_err(|e| RoutingSessionError::UnsupportedTopology(e.to_string()))?;
                    queried(12)
                }
                _ => Err(RoutingSessionError::Invalid(
                    InvalidRoute::MalformedDescriptor,
                )),
            };
        }
        if bytes[2] == mixer_subtype || bytes[2] == selector_subtype {
            let count = usize::from(*bytes.get(4).ok_or(RoutingSessionError::Invalid(
                InvalidRoute::MalformedDescriptor,
            ))?);
            let sources = bytes.get(5..5 + count).ok_or(RoutingSessionError::Invalid(
                InvalidRoute::MalformedDescriptor,
            ))?;
            let mut clusters = Vec::new();
            let mut inputs = Vec::new();
            for &source_id in sources {
                let cluster =
                    resolve_cluster_cached(source_id, nodes, path, cache, protocol, interface, hc)?;
                inputs.push(InputCluster {
                    source_id,
                    channels: cluster.channels,
                });
                clusters.push(cluster);
            }
            if bytes[2] == mixer_subtype {
                let output = if protocol == 0x30 {
                    queried(5 + count)?
                } else {
                    explicit_cluster(bytes, 5 + count, protocol)?
                };
                parse_mixer(interface, bytes, &inputs, protocol, output.channels)?;
                return Ok(output);
            }
            parse_selector(interface, bytes, protocol)?;
            if clusters.is_empty() || clusters.iter().any(|c| c != &clusters[0]) {
                return Err(RoutingSessionError::UnsupportedTopology(
                    "selector input channel clusters differ or are empty".into(),
                ));
            }
            return Ok(clusters.remove(0));
        }
        if bytes[2] == feature_subtype && bytes.len() >= 6 {
            let cluster =
                resolve_cluster_cached(bytes[4], nodes, path, cache, protocol, interface, hc)?;
            let valid = match protocol {
                0 => {
                    bytes[5] > 0
                        && bytes.len()
                            == 7 + (usize::from(cluster.channels) + 1) * usize::from(bytes[5])
                }
                0x20 => bytes.len() == 6 + (usize::from(cluster.channels) + 1) * 4,
                0x30 => crate::descriptors::uac3::parse_feature_unit(bytes)
                    .is_ok_and(|f| f.controls.len() == usize::from(cluster.channels) + 1),
                _ => false,
            };
            if !valid {
                return Err(RoutingSessionError::Invalid(
                    InvalidRoute::MalformedDescriptor,
                ));
            }
            return Ok(cluster);
        }
        Err(RoutingSessionError::UnsupportedTopology(format!(
            "source entity {id} has unsupported channel topology"
        )))
    })();
    path.pop();
    if let Ok(cluster) = &result {
        cache.insert(id, cluster.clone());
    }
    result
}

fn parse_selector(
    interface: u8,
    bytes: &[u8],
    protocol: u8,
) -> Result<SelectorUnit, RoutingSessionError> {
    match protocol {
        0 => SelectorUnit::parse_uac1(interface, bytes),
        0x30 => SelectorUnit::parse_adc3(interface, bytes),
        _ => SelectorUnit::parse(interface, bytes),
    }
    .map_err(RoutingSessionError::Invalid)
}
fn parse_mixer(
    interface: u8,
    bytes: &[u8],
    inputs: &[InputCluster],
    protocol: u8,
    outputs: u8,
) -> Result<MixerUnit, RoutingSessionError> {
    match protocol {
        0 => MixerUnit::parse_uac1(interface, bytes, inputs),
        0x30 => MixerUnit::parse_adc3(interface, bytes, inputs, outputs),
        _ => MixerUnit::parse(interface, bytes, inputs),
    }
    .map_err(RoutingSessionError::Invalid)
}

fn routing_interfaces<'a>(
    raw: &'a [u8],
    claims: &[u8],
) -> Result<BTreeMap<u8, (u8, Vec<&'a [u8]>)>, RoutingSessionError> {
    if raw.len() < 9
        || raw[0] != 9
        || raw[1] != 2
        || usize::from(u16::from_le_bytes([raw[2], raw[3]])) != raw.len()
    {
        return Err(RoutingSessionError::Invalid(
            InvalidRoute::MalformedDescriptor,
        ));
    }
    let mut interfaces = BTreeMap::new();
    let mut current = None;
    for bytes in framed(raw)? {
        if bytes[1] == 4 {
            if bytes.len() != 9 {
                return Err(RoutingSessionError::Invalid(
                    InvalidRoute::MalformedDescriptor,
                ));
            }
            current = None;
            if bytes[3] == 0
                && bytes[5] == 1
                && bytes[6] == 1
                && [0, 0x20, 0x30].contains(&bytes[7])
                && claims.contains(&bytes[2])
            {
                if interfaces
                    .insert(bytes[2], (bytes[7], Vec::new()))
                    .is_some()
                {
                    return Err(RoutingSessionError::Invalid(InvalidRoute::DuplicateEntity));
                }
                current = Some(bytes[2]);
            }
        } else if bytes[1] == 0x24 {
            if let Some(interface) = current {
                interfaces.get_mut(&interface).unwrap().1.push(bytes);
            }
        }
    }
    Ok(interfaces)
}

#[cfg(test)]
fn inventory_from_config(
    raw: &[u8],
    claims: &[u8],
) -> Result<RoutingInventory, RoutingSessionError> {
    inventory_with_clusters(raw, claims, &BTreeMap::new())
}
fn inventory_with_clusters(
    raw: &[u8],
    claims: &[u8],
    hc: &BTreeMap<(u8, u16), RoutingCluster>,
) -> Result<RoutingInventory, RoutingSessionError> {
    let mut inventory = RoutingInventory::default();
    for (interface, (protocol, descriptors)) in routing_interfaces(raw, claims)? {
        let mut nodes = BTreeMap::new();
        let mut header = None;
        for &bytes in &descriptors {
            if bytes.len() < 4 {
                return Err(RoutingSessionError::Invalid(
                    InvalidRoute::MalformedDescriptor,
                ));
            }
            if bytes[2] == 1 {
                if header.replace(bytes).is_some() {
                    return Err(RoutingSessionError::Invalid(
                        InvalidRoute::MalformedDescriptor,
                    ));
                }
            } else if bytes[3] == 0 || nodes.insert(bytes[3], bytes).is_some() {
                return Err(RoutingSessionError::Invalid(InvalidRoute::DuplicateEntity));
            }
        }
        let header = header.ok_or(RoutingSessionError::Invalid(
            InvalidRoute::MalformedDescriptor,
        ))?;
        let total = match protocol {
            0 if header.len() >= 8
                && header[3..5] == [0, 1]
                && header.len() == 8 + usize::from(header[7]) =>
            {
                u16::from_le_bytes([header[5], header[6]])
            }
            0x20 if header.len() == 9 && header[3..5] == [0, 2] => {
                u16::from_le_bytes([header[6], header[7]])
            }
            0x30 => {
                crate::descriptors::uac3::parse_ac_header(header)
                    .map_err(|e| RoutingSessionError::UnsupportedTopology(e.to_string()))?
                    .total_length
            }
            _ => {
                return Err(RoutingSessionError::Invalid(
                    InvalidRoute::MalformedDescriptor,
                ))
            }
        };
        if usize::from(total) != descriptors.iter().map(|d| d.len()).sum::<usize>() {
            return Err(RoutingSessionError::Invalid(
                InvalidRoute::MalformedDescriptor,
            ));
        }
        let mut cache = BTreeMap::new();
        for &bytes in nodes.values() {
            let mixer = if protocol == 0x30 { 5 } else { 4 };
            let selector = if protocol == 0x30 { 6 } else { 5 };
            if bytes[2] != mixer && bytes[2] != selector {
                continue;
            }
            let output = resolve_cluster_cached(
                bytes[3],
                &nodes,
                &mut Vec::new(),
                &mut cache,
                protocol,
                interface,
                hc,
            )?;
            let unit = if bytes[2] == selector {
                RoutingUnit::Selector(parse_selector(interface, bytes, protocol)?)
            } else {
                let count = usize::from(bytes[4]);
                let inputs = bytes[5..5 + count]
                    .iter()
                    .map(|&source_id| {
                        Ok(InputCluster {
                            source_id,
                            channels: resolve_cluster_cached(
                                source_id,
                                &nodes,
                                &mut Vec::new(),
                                &mut cache,
                                protocol,
                                interface,
                                hc,
                            )?
                            .channels,
                        })
                    })
                    .collect::<Result<Vec<_>, RoutingSessionError>>()?;
                RoutingUnit::Mixer(parse_mixer(
                    interface,
                    bytes,
                    &inputs,
                    protocol,
                    output.channels,
                )?)
            };
            inventory
                .insert(unit)
                .map_err(RoutingSessionError::Invalid)?;
        }
    }
    Ok(inventory)
}

/// Descriptor-only scope proof shared by no-wake discovery and explicit wake.
fn adc3_routing_scope(
    descriptors: &[&[u8]],
) -> Result<
    (
        Vec<crate::descriptors::uac3::PowerDomain>,
        std::collections::BTreeSet<u8>,
    ),
    RoutingSessionError,
> {
    let mut domains = BTreeMap::new();
    let mut members = std::collections::BTreeSet::new();
    for bytes in descriptors {
        if bytes.get(2) != Some(&0x10) {
            continue;
        }
        let domain = crate::descriptors::uac3::parse_power_domain(bytes)
            .map_err(|e| RoutingSessionError::UnsupportedTopology(e.to_string()))?;
        if domains.insert(domain.id, domain.clone()).is_some()
            || domain.entities.iter().any(|e| !members.insert(*e))
        {
            return Err(RoutingSessionError::UnsupportedTopology(
                "ADC3 routing power domains overlap or repeat".into(),
            ));
        }
    }
    let mut nodes = BTreeMap::new();
    for bytes in descriptors.iter().filter(|b| b.len() >= 4 && b[2] != 1) {
        if nodes.insert(bytes[3], *bytes).is_some() {
            return Err(RoutingSessionError::Invalid(InvalidRoute::DuplicateEntity));
        }
    }
    let mut affected = std::collections::BTreeSet::new();
    let mut pending: Vec<_> = nodes
        .values()
        .filter(|b| matches!(b[2], 5 | 6))
        .map(|b| b[3])
        .collect();
    while let Some(id) = pending.pop() {
        if !affected.insert(id) {
            continue;
        }
        let bytes = nodes.get(&id).ok_or_else(|| {
            RoutingSessionError::UnsupportedTopology("unresolved ADC3 routing source".into())
        })?;
        match bytes[2] {
            5 | 6 => {
                let pins = usize::from(*bytes.get(4).ok_or(RoutingSessionError::Invalid(
                    InvalidRoute::MalformedDescriptor,
                ))?);
                pending.extend(bytes.get(5..5 + pins).ok_or(RoutingSessionError::Invalid(
                    InvalidRoute::MalformedDescriptor,
                ))?);
            }
            7 => pending.push(*bytes.get(4).ok_or(RoutingSessionError::Invalid(
                InvalidRoute::MalformedDescriptor,
            ))?),
            _ => {}
        }
    }
    Ok((
        domains
            .into_values()
            .filter(|d| d.entities.iter().any(|e| affected.contains(e)))
            .collect(),
        affected,
    ))
}

/// Require every interface in the affected full ADC3 function to be claimed
/// and every AS interface to be idle before the first power write.
fn routing_power_interfaces(
    raw: &[u8],
    claims: &[u8],
    control: u8,
) -> Result<Vec<u8>, RoutingSessionError> {
    let descriptors = framed(raw)?;
    if descriptors.iter().any(|b| b[1] == 11 && b.len() != 8) {
        return Err(RoutingSessionError::Invalid(
            InvalidRoute::MalformedDescriptor,
        ));
    }
    let owners: Vec<_> = descriptors
        .iter()
        .filter(|b| {
            b[1] == 11
                && b.len() == 8
                && u16::from(control) >= u16::from(b[2])
                && u16::from(control) < u16::from(b[2]) + u16::from(b[3])
        })
        .collect();
    let fail = || {
        RoutingSessionError::UnsupportedTopology(
            "powered ADC3 routing requires one fully claimed full Audio function".into(),
        )
    };
    if owners.len() != 1 {
        return Err(fail());
    }
    let owner = owners[0];
    if owner[2] != control
        || owner[3] == 0
        || owner[4..7] != [1, 0, 0x30]
        || u16::from(owner[2]) + u16::from(owner[3]) > 256
    {
        return Err(fail());
    }
    let mut streams = Vec::new();
    for interface in u16::from(owner[2])..u16::from(owner[2]) + u16::from(owner[3]) {
        let interface = interface as u8;
        if descriptors
            .iter()
            .filter(|b| {
                b[1] == 11
                    && b.len() == 8
                    && u16::from(interface) >= u16::from(b[2])
                    && u16::from(interface) < u16::from(b[2]) + u16::from(b[3])
            })
            .count()
            != 1
        {
            return Err(fail());
        }
        let alts: Vec<_> = descriptors
            .iter()
            .filter(|b| b[1] == 4 && b.len() == 9 && b[2] == interface && b[3] == 0)
            .collect();
        if !claims.contains(&interface)
            || alts.len() != 1
            || alts[0][5] != 1
            || alts[0][7] != 0x30
            || alts[0][6] != if interface == control { 1 } else { 2 }
        {
            return Err(fail());
        }
        if interface != control {
            streams.push(interface);
        }
    }
    Ok(streams)
}

// Keep panic unwinding subject to the same no-external-drift-overwrite rule.
struct RoutingPowerLease(super::adc3::Adc3PowerLease);
impl Drop for RoutingPowerLease {
    fn drop(&mut self) {
        if let Err(error) = self.0.restore_checked() {
            log_warn!("[USB] ADC3 routing power hand-back: {error}");
        }
    }
}

fn with_routing_power<T>(
    device: &UsbDevice,
    raw: &[u8],
    powered: bool,
    operation: impl FnOnce() -> Result<T, RoutingSessionError>,
) -> Result<T, RoutingSessionError> {
    if !powered {
        return operation();
    }
    let guard = RoutingPowerLease(super::adc3::Adc3PowerLease::new(device.fd));
    let lease = &guard.0;
    let result = (|| {
        let mut scopes = Vec::new();
        for (interface, (protocol, descriptors)) in
            routing_interfaces(raw, &device.claimed_interfaces)?
        {
            if protocol != 0x30 {
                continue;
            }
            let (domains, _) = adc3_routing_scope(&descriptors)?;
            if domains.is_empty() {
                continue;
            }
            let streams = routing_power_interfaces(raw, &device.claimed_interfaces, interface)?;
            for stream in streams {
                lease
                    .verify_idle(stream)
                    .map_err(|e| RoutingSessionError::Session(Error::Control(e)))?;
            }
            scopes.push((interface, domains));
        }
        // Complete every scope/claim/idle proof before waking any domain.
        for (interface, domains) in scopes {
            for domain in domains {
                lease
                    .acquire(interface, &domain)
                    .map_err(|e| RoutingSessionError::Session(Error::Control(e)))?;
            }
        }
        operation()
    })();
    match lease.restore_checked() {
        Ok(()) => result,
        Err(error) => Err(RoutingSessionError::Session(Error::Control(format!(
            "powered routing power hand-back: {error}; operation failure: {:?}",
            result.as_ref().err()
        )))),
    }
}

/// Query only already claimed AC functions. Sleeping/unknown domains are a
/// typed refusal: capability discovery never changes a device's power state.
fn routing_clusters(
    device: &UsbDevice,
    raw: &[u8],
) -> Result<BTreeMap<(u8, u16), RoutingCluster>, RoutingSessionError> {
    let interfaces = routing_interfaces(raw, &device.claimed_interfaces)?;
    let idle = IDLE_LOCK.lock();
    let idle = idle
        .as_ref()
        .filter(|i| i.device_fd == device.fd)
        .ok_or_else(|| {
            RoutingSessionError::Session(Error::InvalidState("routing lost idle owner".into()))
        })?;
    let mut clusters = BTreeMap::new();
    for (interface, (protocol, descriptors)) in interfaces {
        if protocol != 0x30 {
            continue;
        }
        let (domains, affected) = adc3_routing_scope(&descriptors)?;
        // Prove D0 only for domains containing exposed units or their sources.
        for id in domains.iter().map(|d| d.id) {
            let mut state = [0xff];
            let n = idle
                .handle
                .read_control(
                    0xa1,
                    1,
                    0x0200,
                    (u16::from(id) << 8) | u16::from(interface),
                    &mut state,
                    Duration::from_secs(1),
                )
                .map_err(|e| RoutingSessionError::Session(Error::Control(e.to_string())))?;
            if n != 1 || state[0] != 0 {
                return Err(RoutingSessionError::UnsupportedTopology("ADC3 routing requires already-D0 entity domains; waking requires an owned power lease".into()));
            }
        }
        for bytes in descriptors {
            if bytes.len() < 4 || !affected.contains(&bytes[3]) {
                continue;
            }
            let id = match bytes.get(2) {
                Some(2) => Some(
                    crate::descriptors::uac3::parse_input_terminal(bytes)
                        .map_err(|e| RoutingSessionError::UnsupportedTopology(e.to_string()))?
                        .cluster_id,
                ),
                Some(5) => {
                    let pins = usize::from(*bytes.get(4).ok_or(RoutingSessionError::Invalid(
                        InvalidRoute::MalformedDescriptor,
                    ))?);
                    let pair =
                        bytes
                            .get(5 + pins..7 + pins)
                            .ok_or(RoutingSessionError::Invalid(
                                InvalidRoute::MalformedDescriptor,
                            ))?;
                    Some(u16::from_le_bytes([pair[0], pair[1]]))
                }
                _ => None,
            };
            if let Some(id) = id {
                if !clusters.contains_key(&(interface, id)) {
                    let cluster = query_routing_cluster(&idle.handle, interface, id)?;
                    clusters.insert((interface, id), cluster);
                }
            }
        }
    }
    Ok(clusters)
}

fn verify_descriptor_identity(
    snapshot: &RoutingSnapshot,
    device: &UsbDevice,
) -> Result<(), RoutingSessionError> {
    if !Arc::ptr_eq(&snapshot.device._fd_owner, &device._fd_owner) {
        return Err(RoutingSessionError::Drift(
            "routing snapshot belongs to another attachment".into(),
        ));
    }
    let (configuration, descriptors) = active_descriptors(device)?;
    if configuration != snapshot.configuration || descriptors != snapshot.descriptors {
        return Err(RoutingSessionError::Drift(
            "configuration or descriptors changed; rediscover routing".into(),
        ));
    }
    Ok(())
}
fn verify_snapshot(
    snapshot: &RoutingSnapshot,
    device: &UsbDevice,
) -> Result<(), RoutingSessionError> {
    verify_descriptor_identity(snapshot, device)?;
    if routing_clusters(device, &snapshot.descriptors)? != snapshot.hc_clusters {
        return Err(RoutingSessionError::Drift(
            "ADC3 HC Clusters changed; rediscover routing".into(),
        ));
    }
    let mut io = UsbRoutingIo {
        device,
        descriptors: &snapshot.descriptors,
    };
    verify_values(&snapshot.inventory, &mut io, &snapshot.selectors)
}

fn verify_values<I: RoutingIo<Error = String>>(
    inventory: &RoutingInventory,
    io: &mut I,
    expected: &[RouteChange],
) -> Result<(), RoutingSessionError> {
    for value in expected {
        let control = inventory
            .control(value.key)
            .map_err(RoutingSessionError::Invalid)?;
        let actual = control
            .read_current(io)
            .map_err(RoutingSessionError::Control)?;
        if actual != value.value {
            return Err(RoutingSessionError::Drift(format!(
                "control {:?} changed from {:?} to {:?}",
                value.key, value.value, actual
            )));
        }
    }
    Ok(())
}

pub(crate) fn discover() -> Result<RoutingSnapshot, RoutingSessionError> {
    discover_with_power(false)
}
pub(crate) fn discover_with_power(powered: bool) -> Result<RoutingSnapshot, RoutingSessionError> {
    let _gate = USB_SESSION_GATE.lock();
    let _controls = HARDWARE_VOLUME_MUTEX.lock();
    let device = idle_device()?;
    let (configuration, descriptors) = active_descriptors(&device)?;
    with_routing_power(&device, &descriptors, powered, || {
        let hc_clusters = routing_clusters(&device, &descriptors)?;
        let inventory =
            inventory_with_clusters(&descriptors, &device.claimed_interfaces, &hc_clusters)?;
        let mut selectors = Vec::new();
        let mut io = UsbRoutingIo {
            device: &device,
            descriptors: &descriptors,
        };
        for unit in inventory.units() {
            if let RoutingUnit::Selector(selector) = unit {
                if selector.access() != Access::Absent {
                    let key = ControlKey::Selector(selector.entity());
                    let value = inventory
                        .control(key)
                        .map_err(RoutingSessionError::Invalid)?
                        .read_current(&mut io)
                        .map_err(RoutingSessionError::Control)?;
                    selectors.push(RouteChange { key, value });
                }
            }
        }
        Ok(RoutingSnapshot {
            device: device.clone(),
            configuration,
            descriptors: descriptors.clone(),
            inventory,
            selectors,
            hc_clusters,
        })
    })
}

pub(crate) fn read(
    snapshot: &RoutingSnapshot,
    key: ControlKey,
) -> Result<RouteValue, RoutingSessionError> {
    read_with_power(snapshot, key, false)
}
pub(crate) fn read_with_power(
    snapshot: &RoutingSnapshot,
    key: ControlKey,
    powered: bool,
) -> Result<RouteValue, RoutingSessionError> {
    let _gate = USB_SESSION_GATE.lock();
    let _controls = HARDWARE_VOLUME_MUTEX.lock();
    let device = idle_device()?;
    verify_descriptor_identity(snapshot, &device)?;
    with_routing_power(&device, &snapshot.descriptors, powered, || {
        verify_snapshot(snapshot, &device)?;
        snapshot
            .inventory
            .control(key)
            .map_err(RoutingSessionError::Invalid)?
            .read_current(&mut UsbRoutingIo {
                device: &device,
                descriptors: &snapshot.descriptors,
            })
            .map_err(RoutingSessionError::Control)
    })
}

#[derive(Debug)]
struct RecordedRouting {
    snapshot: RoutingSnapshot,
    // Original and last library-written value for each edited control.
    values: BTreeMap<ControlKey, (RouteValue, RouteValue)>,
    powered: bool,
}
static ORIGINAL_ROUTING: Mutex<Option<RecordedRouting>> = Mutex::new(None);

pub(crate) fn apply(
    snapshot: &RoutingSnapshot,
    edits: &[RouteEdit],
) -> Result<RouteReport, RoutingSessionError> {
    apply_with_power(snapshot, edits, false)
}
pub(crate) fn apply_with_power(
    snapshot: &RoutingSnapshot,
    edits: &[RouteEdit],
    powered: bool,
) -> Result<RouteReport, RoutingSessionError> {
    let _gate = USB_SESSION_GATE.lock();
    let _controls = HARDWARE_VOLUME_MUTEX.lock();
    let device = idle_device()?;
    verify_descriptor_identity(snapshot, &device)?;
    with_routing_power(&device, &snapshot.descriptors, powered, || {
        verify_snapshot(snapshot, &device)?;
        if edits.len() > MAX_ROUTE_CHANGES {
            return Err(RoutingSessionError::Invalid(InvalidRoute::TooManyChanges));
        }
        let mut originals = ORIGINAL_ROUTING.lock();
        if originals.as_ref().is_some_and(|prior| {
            prior.snapshot.configuration != snapshot.configuration
                || prior.snapshot.descriptors != snapshot.descriptors
        }) {
            return Err(RoutingSessionError::Drift(
                "configuration changed since the first routing edit".into(),
            ));
        }
        if originals
            .as_ref()
            .is_some_and(|prior| !Arc::ptr_eq(&prior.snapshot.device._fd_owner, &device._fd_owner))
        {
            return Err(RoutingSessionError::Drift(
                "routing restoration record belongs to another attachment".into(),
            ));
        }
        let old_count = originals.as_ref().map_or(0, |prior| prior.values.len());
        if old_count + edits.len() > MAX_RECORDED_ROUTES {
            return Err(RoutingSessionError::Invalid(InvalidRoute::TooManyChanges));
        }
        let expected = edits
            .iter()
            .map(|e| RouteChange {
                key: e.key,
                value: e.expected,
            })
            .collect::<Vec<_>>();
        let changes = edits
            .iter()
            .map(|e| RouteChange {
                key: e.key,
                value: e.value,
            })
            .collect::<Vec<_>>();
        let mut io = UsbRoutingIo {
            device: &device,
            descriptors: &snapshot.descriptors,
        };
        verify_values(&snapshot.inventory, &mut io, &expected)?;
        let report = match routing::apply_routes(&snapshot.inventory, &mut io, &changes) {
            Ok(report) => report,
            Err(failure) => {
                if !failure.rollback_failures.is_empty() {
                    invalidate_route_caches();
                }
                return Err(RoutingSessionError::Transaction(failure));
            }
        };
        if !report.changed.is_empty() {
            let recorded = originals.get_or_insert_with(|| RecordedRouting {
                snapshot: snapshot.clone(),
                values: BTreeMap::new(),
                powered,
            });
            recorded.powered |= powered;
            for edit in edits {
                if report.changed.contains(&edit.key) {
                    recorded
                        .values
                        .entry(edit.key)
                        .and_modify(|record| record.1 = edit.value)
                        .or_insert((edit.expected, edit.value));
                }
            }
            // A later open must repeat route/clock and configured volume admission.
            invalidate_route_caches();
        }
        Ok(report)
    })
}

fn invalidate_route_caches() {
    // Do not let the previous route's Feature Unit masquerade as protection for
    // a new selector path. Original volume records and actual device levels
    // remain intact; normal negotiation rediscovers and admits the new route.
    install_volume_control(None);
    set_hardware_volume_error(Some(
        "routing changed; negotiate to rediscover the playback volume route".into(),
    ));
    if let Some(state) = SESSION_STATE.lock().as_mut() {
        state.playback_format = None;
        state.requested_playback_format = None;
        state.clock_verification_passed = false;
    }
}

/// Caller holds USB_SESSION_GATE and HARDWARE_VOLUME_MUTEX. Restore only when
/// configured HandBack requests it, attachment/configuration still match and
/// edited controls still contain the last library-written values. A volume-only
/// close override never changes this policy. Always retire the record on close.
pub(super) fn finish_routing_hand_back() {
    finish_scarlett_hand_back();
    let Some(record) = ORIGINAL_ROUTING.lock().take() else {
        return;
    };
    if settings().config.hand_back != crate::HandBack::RestoreOriginal {
        return;
    }
    let Some(device) = SESSION_STATE.lock().as_ref().map(|s| s.device.clone()) else {
        return;
    };
    let restore = || -> Result<(), RoutingSessionError> {
        if !Arc::ptr_eq(&record.snapshot.device._fd_owner, &device._fd_owner) {
            return Err(RoutingSessionError::Drift(
                "attachment changed before routing hand-back".into(),
            ));
        }
        let (number, raw) = active_descriptors(&device)?;
        if number != record.snapshot.configuration || raw != record.snapshot.descriptors {
            return Err(RoutingSessionError::Drift(
                "configuration changed before routing hand-back".into(),
            ));
        }
        with_routing_power(&device, &raw, record.powered, || {
            let mut expected = record.snapshot.selectors.clone();
            for item in &mut expected {
                if let Some((_, last)) = record.values.get(&item.key) {
                    item.value = *last;
                }
            }
            if routing_clusters(&device, &raw)? != record.snapshot.hc_clusters {
                return Err(RoutingSessionError::Drift(
                    "ADC3 HC Clusters changed before routing hand-back".into(),
                ));
            }
            let mut io = UsbRoutingIo {
                device: &device,
                descriptors: &record.snapshot.descriptors,
            };
            verify_values(&record.snapshot.inventory, &mut io, &expected)?;
            let current = record
                .values
                .iter()
                .map(|(&key, &(_, value))| RouteChange { key, value })
                .collect::<Vec<_>>();
            verify_values(&record.snapshot.inventory, &mut io, &current)?;
            let changes = record
                .values
                .iter()
                .map(|(&key, &(value, _))| RouteChange { key, value })
                .collect::<Vec<_>>();
            // Every bounded group retains the same readback and rollback contract.
            for batch in changes.chunks(MAX_ROUTE_CHANGES) {
                routing::apply_routes(&record.snapshot.inventory, &mut io, batch)
                    .map_err(RoutingSessionError::Transaction)?;
            }
            Ok(())
        })
    };
    if let Err(error) = restore() {
        log_warn!("[USB] Original routing not restored: {error}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::routing::Entity;

    fn terminal(id: u8, channels: u8, locations: u32) -> Vec<u8> {
        let mut bytes = vec![17, 0x24, 2, id, 1, 1, 0, 10, channels];
        bytes.extend(locations.to_le_bytes());
        bytes.extend([0, 0, 0, 0]);
        bytes
    }

    fn configuration(entities: &[Vec<u8>], protocol: u8) -> Vec<u8> {
        let ac_length = 9 + entities.iter().map(Vec::len).sum::<usize>();
        let total = 18 + ac_length;
        let mut raw = vec![9, 2, total as u8, (total >> 8) as u8, 1, 1, 0, 0x80, 50];
        raw.extend([9, 4, 3, 0, 0, 1, 1, protocol, 0]);
        raw.extend([
            9,
            0x24,
            1,
            0,
            2,
            1,
            ac_length as u8,
            (ac_length >> 8) as u8,
            0,
        ]);
        for entity in entities {
            raw.extend(entity);
        }
        raw
    }

    fn selector() -> Vec<u8> {
        vec![9, 0x24, 5, 7, 2, 1, 2, 3, 0]
    }

    #[test]
    fn powered_scope_requires_complete_claims_unique_iad_and_full_protocol() {
        let mut raw = vec![9, 2, 0, 0, 3, 1, 0, 0x80, 50];
        raw.extend([8, 11, 3, 3, 1, 0, 0x30, 0]);
        raw.extend([9, 4, 3, 0, 0, 1, 1, 0x30, 0]);
        raw.extend([9, 4, 4, 0, 0, 1, 2, 0x30, 0]);
        raw.extend([9, 4, 5, 0, 0, 1, 2, 0x30, 0]);
        assert_eq!(
            routing_power_interfaces(&raw, &[3, 4, 5], 3).unwrap(),
            vec![4, 5]
        );
        assert!(routing_power_interfaces(&raw, &[3, 4], 3).is_err());
        assert!(routing_power_interfaces(&raw, &[3, 4, 5], 4).is_err());
        let mut badd = raw.clone();
        badd[15] = 0x20;
        assert!(routing_power_interfaces(&badd, &[3, 4, 5], 3).is_err());
        raw.extend([8, 11, 4, 2, 1, 0, 0x30, 0]);
        assert!(routing_power_interfaces(&raw, &[3, 4, 5], 3).is_err());
    }

    #[test]
    fn powered_scope_selects_only_routing_dependencies_and_rejects_ambiguity() {
        let terminal = [4, 0x24, 2, 1];
        let unrelated = [4, 0x24, 2, 2];
        let selector = [9, 0x24, 6, 7, 1, 1, 0, 0, 0];
        let domain = [12, 0x24, 0x10, 9, 0, 0, 0, 0, 1, 1, 0, 0];
        let other = [12, 0x24, 0x10, 10, 0, 0, 0, 0, 1, 2, 0, 0];
        let descriptors: Vec<&[u8]> = vec![&terminal, &unrelated, &selector, &domain, &other];
        let (domains, entities) = adc3_routing_scope(&descriptors).unwrap();
        assert_eq!(domains.iter().map(|d| d.id).collect::<Vec<_>>(), vec![9]);
        assert_eq!(entities.into_iter().collect::<Vec<_>>(), vec![1, 7]);
        let mut duplicate = descriptors.clone();
        duplicate.push(&terminal);
        assert!(adc3_routing_scope(&duplicate).is_err());
        let missing: Vec<&[u8]> = vec![&selector, &domain];
        assert!(adc3_routing_scope(&missing).is_err());
    }

    #[test]
    fn claimed_uac2_topology_resolves_feature_mixer_and_selector_sources() {
        let mut feature = vec![18, 0x24, 6, 5, 1];
        feature.extend([0; 13]);
        let mixer = vec![16, 0x24, 4, 9, 2, 5, 7, 2, 0, 0, 0, 0, 0, 0xff, 0, 0];
        let raw = configuration(
            &[
                terminal(1, 2, 3),
                terminal(2, 2, 3),
                feature,
                selector(),
                mixer,
            ],
            0x20,
        );
        let inventory = inventory_from_config(&raw, &[3]).unwrap();
        assert_eq!(inventory.units().count(), 2);
        let control = inventory
            .control(ControlKey::Crosspoint {
                entity: Entity {
                    interface: 3,
                    id: 9,
                },
                input_pin: 2,
                input_channel: 2,
                output_channel: 2,
            })
            .unwrap();
        assert_eq!(control.current_request().unwrap().value, 0x0107);
        assert_eq!(control.current_request().unwrap().index, 0x0903);
    }

    #[test]
    fn unclaimed_unknown_and_mismatched_protocols_never_expose_controls() {
        let entities = [terminal(1, 2, 3), terminal(2, 2, 3), selector()];
        assert_eq!(
            inventory_from_config(&configuration(&entities, 0x20), &[4])
                .unwrap()
                .units()
                .count(),
            0
        );
        assert_eq!(
            inventory_from_config(&configuration(&entities, 0xff), &[3])
                .unwrap()
                .units()
                .count(),
            0
        );
        // Reinterpreting UAC2 descriptor bytes under another protocol is refused.
        for protocol in [0, 0x30] {
            assert!(inventory_from_config(&configuration(&entities, protocol), &[3]).is_err());
        }
    }

    #[test]
    fn no_default_counts_for_missing_cyclic_or_unsupported_sources() {
        let missing = configuration(&[terminal(1, 2, 3), selector()], 0x20);
        assert!(matches!(
            inventory_from_config(&missing, &[3]),
            Err(RoutingSessionError::UnsupportedTopology(_))
        ));
        let cycle = configuration(&[vec![8, 0x24, 5, 7, 1, 7, 3, 0]], 0x20);
        assert!(matches!(
            inventory_from_config(&cycle, &[3]),
            Err(RoutingSessionError::UnsupportedTopology(_))
        ));
        let unknown = configuration(
            &[
                terminal(1, 2, 3),
                vec![8, 0x24, 9, 2, 0, 0, 0, 0],
                selector(),
            ],
            0x20,
        );
        assert!(matches!(
            inventory_from_config(&unknown, &[3]),
            Err(RoutingSessionError::UnsupportedTopology(_))
        ));
    }

    #[test]
    fn selector_cluster_identity_requires_count_positions_and_names() {
        for other in [terminal(2, 1, 1), terminal(2, 2, 12), {
            let mut terminal = terminal(2, 2, 3);
            terminal[13] = 5;
            terminal
        }] {
            let raw = configuration(&[terminal(1, 2, 3), other, selector()], 0x20);
            assert!(matches!(
                inventory_from_config(&raw, &[3]),
                Err(RoutingSessionError::UnsupportedTopology(_))
            ));
        }
    }

    #[test]
    fn malformed_framing_lengths_ids_and_feature_counts_fail_closed() {
        let raw = configuration(&[terminal(1, 2, 3), terminal(2, 2, 3), selector()], 0x20);
        let mut short = raw.clone();
        short.pop();
        assert!(inventory_from_config(&short, &[3]).is_err());
        let mut malformed = raw.clone();
        malformed[18] = 0;
        assert!(inventory_from_config(&malformed, &[3]).is_err());
        let duplicate = configuration(&[terminal(1, 2, 3), terminal(1, 2, 3), selector()], 0x20);
        assert!(matches!(
            inventory_from_config(&duplicate, &[3]),
            Err(RoutingSessionError::Invalid(InvalidRoute::DuplicateEntity))
        ));
        let bad_feature = configuration(
            &[
                terminal(1, 2, 3),
                vec![10, 0x24, 6, 2, 1, 0, 0, 0, 0, 0],
                selector(),
            ],
            0x20,
        );
        assert!(matches!(
            inventory_from_config(&bad_feature, &[3]),
            Err(RoutingSessionError::Invalid(
                InvalidRoute::MalformedDescriptor
            ))
        ));
    }

    #[test]
    fn uac1_inventory_resolves_explicit_source_clusters() {
        let terminal = vec![12, 0x24, 2, 1, 1, 1, 0, 2, 3, 0, 0, 0];
        let selector = vec![7, 0x24, 5, 7, 1, 1, 0];
        let mixer = vec![12, 0x24, 4, 9, 1, 7, 2, 3, 0, 0, 0xf0, 0];
        let mut raw = configuration(&[terminal, selector, mixer], 0);
        raw[21..23].copy_from_slice(&[0, 1]);
        let total = raw.len() - 18;
        raw[23..25].copy_from_slice(&(total as u16).to_le_bytes());
        raw[25] = 1;
        raw[26] = 4;
        let inventory = inventory_from_config(&raw, &[3]).unwrap();
        let control = inventory
            .control(ControlKey::Crosspoint {
                entity: Entity {
                    interface: 3,
                    id: 9,
                },
                input_pin: 1,
                input_channel: 2,
                output_channel: 2,
            })
            .unwrap();
        assert_eq!(
            (
                control.current_request().unwrap().request,
                control.current_request().unwrap().value
            ),
            (0x81, 0x0202)
        );
        assert!(validate_protocol_request(control.current_request().unwrap(), true, 2, 0).is_ok());
        assert!(
            validate_protocol_request(control.current_request().unwrap(), true, 2, 0x20).is_err()
        );
    }

    #[test]
    fn adc3_inventory_requires_hc_and_preserves_cluster_content_identity() {
        use crate::descriptors::uac3::{Channel, ChannelRelationship, Cluster as HcCluster};
        let terminal = |id, cluster| {
            vec![
                20, 0x24, 2, id, 1, 1, 0, 10, 0, 0, 0, 0, cluster, 0, 0, 0, 0, 0, 0, 0,
            ]
        };
        let selector = vec![13, 0x24, 6, 7, 2, 1, 2, 3, 0, 0, 0, 0, 0];
        let mixer = vec![15, 0x24, 5, 9, 1, 7, 3, 0, 0xf0, 0, 0, 0, 0, 0, 0];
        let entities = [terminal(1, 1), terminal(2, 2), selector, mixer];
        let total = 10 + entities.iter().map(Vec::len).sum::<usize>();
        let mut raw = vec![9, 2, 0, 0, 1, 1, 0, 0x80, 50, 9, 4, 3, 0, 0, 1, 1, 0x30, 0];
        raw.extend([10, 0x24, 1, 1, total as u8, (total >> 8) as u8, 0, 0, 0, 0]);
        for entity in entities {
            raw.extend(entity);
        }
        let len = raw.len() as u16;
        raw[2..4].copy_from_slice(&len.to_le_bytes());
        assert!(inventory_from_config(&raw, &[3]).is_err());
        let channel = |relationship| Channel::Information {
            purpose: 1,
            relationship,
            group: 0,
        };
        let channels = vec![
            channel(ChannelRelationship::Left),
            channel(ChannelRelationship::Right),
        ];
        let mut hc = BTreeMap::new();
        for id in 1..=3 {
            hc.insert(
                (3, id),
                RoutingCluster {
                    parsed: HcCluster {
                        id,
                        channels: channels.clone(),
                    },
                    content: vec![9, 0, 0x26, 0, id as u8, 0, 2, 2, 3],
                },
            );
        }
        let inventory = inventory_with_clusters(&raw, &[3], &hc).unwrap();
        assert_eq!(inventory.units().count(), 2);
        // Equal content with distinct IDs is legal; same count but changed content is not.
        hc.get_mut(&(3, 2)).unwrap().content[7] = 1;
        assert!(inventory_with_clusters(&raw, &[3], &hc).is_err());
        hc.clear();
        assert_eq!(
            inventory_with_clusters(&raw, &[4], &hc)
                .unwrap()
                .units()
                .count(),
            0
        );
    }

    #[test]
    fn usb_adapter_only_allows_bounded_standard_routing_requests() {
        let request = ControlRequest {
            request_type: 0xa1,
            request: 1,
            value: 0x01ff,
            index: 0x0903,
            length: 2,
        };
        assert!(validate_request(request, true, 2).is_ok());
        assert!(validate_request(
            ControlRequest {
                request_type: 0x21,
                ..request
            },
            false,
            2
        )
        .is_ok());
        assert!(validate_request(
            ControlRequest {
                request: 2,
                length: 8,
                ..request
            },
            true,
            8
        )
        .is_ok());
        for bad in [
            ControlRequest {
                request_type: 0xc1,
                ..request
            },
            ControlRequest {
                request: 0xff,
                ..request
            },
            ControlRequest {
                value: 0x0200,
                ..request
            },
            ControlRequest {
                index: 3,
                ..request
            },
            ControlRequest {
                length: 3,
                ..request
            },
            ControlRequest {
                request: 2,
                length: 512,
                ..request
            },
        ] {
            assert!(validate_request(bad, true, usize::from(bad.length)).is_err());
        }
        assert!(validate_request(
            ControlRequest {
                request_type: 0x21,
                request: 2,
                ..request
            },
            false,
            2
        )
        .is_err());
    }

    struct ReadIo {
        pin: u8,
        reads: usize,
    }
    impl RoutingIo for ReadIo {
        type Error = String;
        fn read(&mut self, _: ControlRequest, bytes: &mut [u8]) -> Result<usize, String> {
            self.reads += 1;
            bytes[0] = self.pin;
            Ok(1)
        }
        fn write(&mut self, _: ControlRequest, _: &[u8]) -> Result<usize, String> {
            panic!("drift verification must never write")
        }
    }

    #[test]
    fn expected_values_reject_external_selector_changes_without_writing() {
        let raw = configuration(&[terminal(1, 2, 3), terminal(2, 2, 3), selector()], 0x20);
        let inventory = inventory_from_config(&raw, &[3]).unwrap();
        let expected = [RouteChange {
            key: ControlKey::Selector(Entity {
                interface: 3,
                id: 7,
            }),
            value: RouteValue::SelectorPin(1),
        }];
        let mut io = ReadIo { pin: 2, reads: 0 };
        assert!(matches!(
            verify_values(&inventory, &mut io, &expected),
            Err(RoutingSessionError::Drift(_))
        ));
        io.pin = 1;
        assert!(verify_values(&inventory, &mut io, &expected).is_ok());
    }
}
