//! Session-bound standard UAC routing with dormant unpublished vendor controls.

use super::*;
use crate::error::OrControl;
use crate::routing::{
    self, Access, ControlKey, ControlRequest, InputCluster, InvalidRoute, MixerUnit, RouteChange,
    RouteReport, RouteValue, RoutingError, RoutingInventory, RoutingIo, RoutingUnit, SelectorUnit,
    TransactionError, MAX_ROUTE_CHANGES,
};
use std::collections::BTreeMap;

// Release boundary: vendor routing was not shipped by the shared ALSA baseline.
// This private compile-time fence has no feature, host-claim or runtime opt-in.
const VENDOR_ROUTING_RELEASED: bool = false;
const VENDOR_ROUTING_UNAVAILABLE: &str =
    "proprietary vendor routing is not available in this release";

fn require_vendor_routing_release() -> Result<(), RoutingSessionError> {
    if VENDOR_ROUTING_RELEASED {
        Ok(())
    } else {
        Err(RoutingSessionError::UnsupportedTopology(
            VENDOR_ROUTING_UNAVAILABLE.into(),
        ))
    }
}

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
    diagnostics: Vec<RoutingDiagnostic>,
    hand_back_report: Arc<Mutex<Option<RoutingHandBackReport>>>,
}

impl RoutingSnapshot {
    /// None until final retirement completes; remains available after close.
    pub fn hand_back_report(&self) -> Option<RoutingHandBackReport> {
        self.hand_back_report.lock().clone()
    }
    pub fn diagnostics(&self) -> &[RoutingDiagnostic] {
        &self.diagnostics
    }
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

/// A routing unit or function omitted because its addressing cannot be proven.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoutingDiagnostic {
    pub interface: u8,
    pub entity_id: Option<u8>,
    pub detail: String,
}

/// Outcome for one control when the session relinquishes routing ownership.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RoutingHandBackOutcome {
    Restored,
    AlreadyOriginal,
    ExternallyChanged,
    Unreadable(String),
    IdentityOrTopologyChanged(String),
    RestoreUncertain(String),
    DeadlineExpired,
    Disabled,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoutingHandBackReport {
    pub controls: Vec<(ControlKey, RoutingHandBackOutcome)>,
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
    /// Scoped ADC3 power hand-back failed. Preserve an operation failure in its
    /// original typed form; `None` means the routing operation itself succeeded.
    PowerRestore {
        operation: Option<Box<RoutingSessionError>>,
        detail: String,
    },
    Scarlett(crate::scarlett::ReadError<String>),
    ScarlettMix(crate::scarlett::MixWriteError<String>),
    ScarlettMux(crate::scarlett::MuxWriteError<String>),
    Fcp(crate::scarlett::FcpWriteError<String>),
    Gen1Read(crate::scarlett::Gen1Failure<String>),
    Gen1(crate::scarlett::Gen1WriteError<String>),
}

impl std::fmt::Display for RoutingSessionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for RoutingSessionError {}

fn djm_usb_error(error: rusb::Error) -> RoutingSessionError {
    RoutingSessionError::Session(match error {
        rusb::Error::NoDevice => Error::DeviceLost,
        _ => Error::Control(error.to_string()),
    })
}

/// Called only while the session gate, controls mutex and idle owner are held.
/// Descriptor proof reuses streaming admission; a USB ID alone authorizes nothing.
fn djm_route_identity(
    device: &UsbDevice,
    idle: &IdleLock,
    info: &crate::DeviceInfo,
) -> Result<(crate::pioneer::DjmModel, u8), RoutingSessionError> {
    let usb = idle.handle.device();
    let identity = usb.device_descriptor().map_err(djm_usb_error)?;
    if identity.vendor_id() != device.vendor_id
        || identity.product_id() != device.product_id
        || identity.vendor_id() != info.vendor_id
        || identity.product_id() != info.product_id
    {
        return Err(RoutingSessionError::Drift(
            "DJM host and live USB identities differ".into(),
        ));
    }
    let model = crate::pioneer::DjmModel::from_usb_id(identity.vendor_id(), identity.product_id())
        .ok_or_else(|| {
            RoutingSessionError::UnsupportedTopology("no supported DJM routing model".into())
        })?;
    let configuration = idle.handle.active_configuration().map_err(djm_usb_error)?;
    let config = usb.active_config_descriptor().map_err(djm_usb_error)?;
    if config.number() != configuration
        || info.configuration_id.is_some_and(|id| id != configuration)
    {
        return Err(RoutingSessionError::Drift(
            "DJM selected and active configurations differ".into(),
        ));
    }
    let profile = vendor_pcm::discover(&idle.handle, &config, resolve_device_speed(&usb))
        .or_control()
        .map_err(RoutingSessionError::Session)?
        .ok_or_else(|| {
            RoutingSessionError::UnsupportedTopology(
                "DJM routing requires a proven vendor PCM configuration".into(),
            )
        })?;
    if profile.configuration_number != configuration
        || !profile.claims_match(&device.claimed_interfaces)
        || !profile.claims_match(&idle.claimed_interfaces)
    {
        return Err(RoutingSessionError::Session(Error::Claim(
            "DJM routing requires exactly the proven vendor PCM interface claims".into(),
        )));
    }
    if idle.handle.active_configuration().map_err(djm_usb_error)? != configuration {
        return Err(RoutingSessionError::Drift(
            "DJM active configuration changed during proof".into(),
        ));
    }
    Ok((model, configuration))
}

pub(crate) fn djm_discover(
    shared: &crate::dac::DacShared,
) -> Result<crate::pioneer::DjmRoutingProfile, RoutingSessionError> {
    require_vendor_routing_release()?;
    let _gate = USB_SESSION_GATE.lock();
    shared.ensure_open().map_err(RoutingSessionError::Session)?;
    let _controls = HARDWARE_VOLUME_MUTEX.lock();
    let device = idle_device()?;
    let idle = IDLE_LOCK.lock();
    let idle = idle
        .as_ref()
        .filter(|idle| idle.device_fd == device.fd)
        .ok_or_else(|| {
            RoutingSessionError::Session(Error::InvalidState(
                "DJM routing requires the idle USB owner".into(),
            ))
        })?;
    let (model, configuration) = djm_route_identity(&device, idle, &shared.info)?;
    Ok(crate::pioneer::DjmRoutingProfile {
        model,
        configuration,
        controls: model.controls(),
    })
}

pub(crate) fn djm_apply(
    shared: &crate::dac::DacShared,
    model: crate::pioneer::DjmModel,
    configuration: u8,
    key: crate::pioneer::DjmControlKey,
    option: u8,
) -> Result<(), RoutingSessionError> {
    require_vendor_routing_release()?;
    let _gate = USB_SESSION_GATE.lock();
    shared.ensure_open().map_err(RoutingSessionError::Session)?;
    let _controls = HARDWARE_VOLUME_MUTEX.lock();
    let device = idle_device()?;
    let idle = IDLE_LOCK.lock();
    let idle = idle
        .as_ref()
        .filter(|idle| idle.device_fd == device.fd)
        .ok_or_else(|| {
            RoutingSessionError::Session(Error::InvalidState(
                "DJM routing requires the idle USB owner".into(),
            ))
        })?;
    let actual = djm_route_identity(&device, idle, &shared.info)?;
    if actual != (model, configuration) {
        return Err(RoutingSessionError::Drift(
            "DJM model or configuration changed; rediscover routing".into(),
        ));
    }
    let (value, index) = model.request(key, option).ok_or_else(|| {
        RoutingSessionError::Session(Error::InvalidArgument(
            "unsupported DJM control or option ID".into(),
        ))
    })?;
    // Every admitted DJM has exactly interface 0; derive the interface set from
    // fresh streaming proof rather than adding a vendor/Android claim.
    for interface in idle.claimed_interfaces.iter().copied() {
        let mut alternate = [0u8];
        let read = idle
            .handle
            .read_control(
                0x81,
                0x0a,
                0,
                u16::from(interface),
                &mut alternate,
                Duration::from_millis(500),
            )
            .map_err(djm_usb_error)?;
        if read != 1 || alternate[0] != 0 {
            return Err(RoutingSessionError::Session(Error::InvalidState(
                "DJM routing requires verified idle alternate zero".into(),
            )));
        }
    }
    if idle.handle.active_configuration().map_err(djm_usb_error)? != configuration {
        return Err(RoutingSessionError::Drift(
            "DJM configuration changed before option write".into(),
        ));
    }
    shared.ensure_open().map_err(RoutingSessionError::Session)?;
    let written = idle
        .handle
        .write_control(0x40, 0x03, value, index, &[], Duration::from_millis(500))
        .map_err(djm_usb_error)?;
    if written != 0 {
        return Err(RoutingSessionError::Session(Error::Control(
            "DJM option request returned an unexpected payload length".into(),
        )));
    }
    Ok(())
}

/// Return one unambiguous, exact-model optional vendor claim. This is admission
/// of an already opted-in host claim, never an instruction to acquire a claim.
pub(super) fn scarlett_claim_interface(
    config: &ConfigDescriptor,
    vendor: u16,
    product: u16,
) -> Option<u8> {
    if !VENDOR_ROUTING_RELEASED {
        return None;
    }
    scarlett_endpoint(config, vendor, product).map(|(interface, _)| interface)
}

/// Descriptor-only host preclaim proof. Never claims an interface or initializes Scarlett.
pub(crate) fn scarlett_preflight(
    fd: RawFd,
    info: &crate::DeviceInfo,
) -> Result<Option<crate::scarlett::ReadCapability>, Error> {
    if !VENDOR_ROUTING_RELEASED {
        return Ok(None);
    }
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
    if !VENDOR_ROUTING_RELEASED {
        return Ok(None);
    }
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
        if !VENDOR_ROUTING_RELEASED {
            return Err(VENDOR_ROUTING_UNAVAILABLE.into());
        }
        self.device
            .feature_unit_control(0xa1, 0, 0, u16::from(self.interface), bytes)
            .map_err(|e| e.to_string())
    }
    fn prepare_notifications(&mut self) -> Result<(), String> {
        if !VENDOR_ROUTING_RELEASED {
            return Err(VENDOR_ROUTING_UNAVAILABLE.into());
        }
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
        if !VENDOR_ROUTING_RELEASED {
            return Err(VENDOR_ROUTING_UNAVAILABLE.into());
        }
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
        if !VENDOR_ROUTING_RELEASED {
            return Err(VENDOR_ROUTING_UNAVAILABLE.into());
        }
        self.handle
            .read_interrupt(self.endpoint, bytes, timeout.max(Duration::from_millis(1)))
            .map_err(|e| e.to_string())
    }
    fn receive_response(&mut self, bytes: &mut [u8]) -> Result<usize, String> {
        if !VENDOR_ROUTING_RELEASED {
            return Err(VENDOR_ROUTING_UNAVAILABLE.into());
        }
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
    require_vendor_routing_release()?;
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

pub(super) struct RecordedScarlett {
    device: UsbDevice,
    configuration: u8,
    descriptors: Vec<u8>,
    rows: BTreeMap<u8, (crate::scarlett::MixReadback, crate::scarlett::MixReadback)>,
    mux: Option<(crate::scarlett::MuxSnapshot, crate::scarlett::MuxSnapshot)>,
}
static ORIGINAL_SCARLETT: context::ScopedMutex<Option<RecordedScarlett>> =
    context::ScopedMutex::new(|context| context.scarlett.clone());

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
    operation: impl FnOnce(
        UsbScarlettIo<'_>,
        crate::scarlett::CandidateModel,
    ) -> Result<T, RoutingSessionError>,
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
        max_payload: if fcp {
            crate::scarlett::FCP_MAX_PAYLOAD
        } else {
            crate::scarlett::MAX_PAYLOAD
        },
    };
    operation(io, capability.candidate)
}

/// Read only firmware-defined FCP topology on a proven, explicitly claimed idle owner.
pub(crate) fn fcp_read() -> Result<crate::scarlett::FcpReadback, RoutingSessionError> {
    require_vendor_routing_release()?;
    let _gate = USB_SESSION_GATE.lock();
    let _controls = HARDWARE_VOLUME_MUTEX.lock();
    let device = idle_device()?;
    let (configuration, descriptors) = active_descriptors(&device)?;
    let readback = with_scarlett_io(&device, configuration, true, |io, candidate| {
        crate::scarlett::read_fcp(io, candidate).map_err(RoutingSessionError::Scarlett)
    })?;
    let (after, raw) = active_descriptors(&device)?;
    if configuration != after || descriptors != raw {
        return Err(RoutingSessionError::Drift(
            "configuration changed during FCP routing read".into(),
        ));
    }
    Ok(readback)
}

// Gen1 uses standard two-byte class CUR controls on the already claimed AC
// interface. Never infer a vendor-interface claim from the product identity.
fn gen1_interface(raw: &[u8], claims: &[u8]) -> Result<u8, RoutingSessionError> {
    let fail = || {
        RoutingSessionError::UnsupportedTopology(
            "Gen1 requires one claimed UAC2 AC alt0 with an owning streaming function".into(),
        )
    };
    // Validate configuration framing before inspecting ownership descriptors.
    routing_interfaces(raw, claims)?;
    let framed = framed(raw)?;
    let controls: Vec<_> = framed
        .iter()
        .filter(|b| b[1] == 4 && b.len() == 9 && b[3] == 0 && b[5] == 1 && b[6] == 1)
        .collect();
    if controls.len() != 1 {
        return Err(fail());
    }
    let ac = controls[0][2];
    if controls[0][7] != 0x20 || !claims.contains(&ac) {
        return Err(fail());
    }
    if framed.iter().any(|b| b[1] == 11 && b.len() != 8) {
        return Err(fail());
    }
    let associations: Vec<_> = framed
        .iter()
        .filter(|b| {
            b[1] == 11
                && b.len() == 8
                && ac >= b[2]
                && u16::from(ac) < u16::from(b[2]) + u16::from(b[3])
        })
        .copied()
        .collect();
    if associations.len() > 1 {
        return Err(fail());
    }
    let iad = associations.first().copied();
    if iad.is_some_and(|b| b[4] != 1 || b[6] != 0x20) {
        return Err(fail());
    }
    let interfaces = routing_interfaces(raw, claims)?;
    let entities = &interfaces.get(&ac).ok_or_else(fail)?.1;
    let headers: Vec<_> = entities
        .iter()
        .filter(|b| b.len() >= 3 && b[2] == 1)
        .collect();
    if headers.len() != 1
        || headers[0].len() != 9
        || headers[0][3..5] != [0, 2]
        || usize::from(u16::from_le_bytes([headers[0][6], headers[0][7]]))
            != entities.iter().map(|b| b.len()).sum::<usize>()
    {
        return Err(fail());
    }
    let mut entity_ids = BTreeMap::new();
    for b in entities.iter().filter(|b| b.len() >= 3 && b[2] != 1) {
        if b.len() < 4
            || b[3] == 0
            || entity_ids.insert(b[3], ()).is_some()
            || (b[2] == 2 && b.len() != 17)
            || (b[2] == 3 && b.len() != 12)
        {
            return Err(fail());
        }
    }
    let terminals: Vec<_> = entities
        .iter()
        .filter(|b| b.len() >= 4 && matches!(b[2], 2 | 3) && b[4..6] == [1, 1])
        .map(|b| b[3])
        .collect();
    if terminals.is_empty() {
        return Err(fail());
    }
    let mut stream = None;
    let mut linked_terminal = None;
    let mut linked = false;
    for b in &framed {
        if b[1] == 4 {
            linked_terminal = None;
            stream = if b.len() == 9
                && b[3] != 0
                && b[4] != 0
                && b[5] == 1
                && b[6] == 2
                && b[7] == 0x20
                && iad.map_or(true, |iad| {
                    b[2] >= iad[2] && u16::from(b[2]) < u16::from(iad[2]) + u16::from(iad[3])
                }) {
                Some(b[2])
            } else {
                None
            };
        } else if b[1] == 0x24
            && b.len() == 16
            && b[2] == 1
            && stream.is_some()
            && terminals.contains(&b[3])
        {
            let number = stream.unwrap();
            let owners: Vec<_> = framed
                .iter()
                .filter(|d| {
                    d[1] == 11
                        && d.len() == 8
                        && number >= d[2]
                        && u16::from(number) < u16::from(d[2]) + u16::from(d[3])
                })
                .copied()
                .collect();
            if owners.len() > 1 || owners.first().is_some_and(|owner| iad != Some(*owner)) {
                return Err(fail());
            }
            linked_terminal = entities
                .iter()
                .find(|d| d.len() >= 6 && d[3] == b[3] && matches!(d[2], 2 | 3))
                .map(|d| d[2]);
        } else if b[1] == 5
            && matches!(b.len(), 7 | 9)
            && b[2] & 0x0f != 0
            && b[3] & 3 == 1
            && b[3] & 0x30 == 0
            && linked_terminal.is_some_and(|subtype| (b[2] & 0x80 != 0) == (subtype == 3))
        {
            linked = true;
        }
    }
    if !linked {
        return Err(fail());
    }
    Ok(ac)
}

struct UsbGen1Io<'a> {
    device: &'a UsbDevice,
    interface: u8,
}
impl crate::scarlett::Gen1Io for UsbGen1Io<'_> {
    type Error = String;
    fn read(&mut self, entity: u8, value: u16) -> Result<i16, String> {
        if !VENDOR_ROUTING_RELEASED {
            return Err(VENDOR_ROUTING_UNAVAILABLE.into());
        }
        let mut data = [0u8; 2];
        let n = self
            .device
            .feature_unit_control(
                0xa1,
                1,
                value,
                (u16::from(entity) << 8) | u16::from(self.interface),
                &mut data,
            )
            .map_err(|e| e.to_string())?;
        if n != 2 {
            return Err("short Gen1 CUR read".into());
        }
        Ok(i16::from_le_bytes(data))
    }
    fn write(&mut self, entity: u8, value: u16, data: i16) -> Result<(), String> {
        if !VENDOR_ROUTING_RELEASED {
            return Err(VENDOR_ROUTING_UNAVAILABLE.into());
        }
        let mut bytes = data.to_le_bytes();
        let n = self
            .device
            .feature_unit_control(
                0x21,
                1,
                value,
                (u16::from(entity) << 8) | u16::from(self.interface),
                &mut bytes,
            )
            .map_err(|e| e.to_string())?;
        if n != 2 {
            return Err("short Gen1 CUR write".into());
        }
        Ok(())
    }
}

#[derive(Clone)]
struct Gen1Binding {
    device: UsbDevice,
    configuration: u8,
    descriptors: Vec<u8>,
}
pub(super) struct RecordedGen1 {
    binding: Gen1Binding,
    original: crate::scarlett::Gen1Readback,
    last: crate::scarlett::Gen1Readback,
    uncertain: bool,
}
static ORIGINAL_GEN1: context::ScopedMutex<Option<RecordedGen1>> =
    context::ScopedMutex::new(|context| context.gen1.clone());

fn gen1_target(
    device: &UsbDevice,
    descriptors: &[u8],
) -> Result<(crate::scarlett::Gen1Model, u8), RoutingSessionError> {
    let idle = IDLE_LOCK.lock();
    let idle = idle
        .as_ref()
        .filter(|idle| idle.device_fd == device.fd)
        .ok_or_else(|| {
            RoutingSessionError::Session(Error::InvalidState(
                "Gen1 requires the idle USB owner".into(),
            ))
        })?;
    let identity = idle
        .handle
        .device()
        .device_descriptor()
        .map_err(|e| RoutingSessionError::Session(Error::Control(e.to_string())))?;
    if identity.vendor_id() != device.vendor_id || identity.product_id() != device.product_id {
        return Err(RoutingSessionError::Drift(
            "Gen1 host and actual USB identities differ".into(),
        ));
    }
    let model = crate::scarlett::Gen1Model::recognize(identity.vendor_id(), identity.product_id())
        .ok_or_else(|| RoutingSessionError::UnsupportedTopology("no exact Gen1 model".into()))?;
    Ok((
        model,
        gen1_interface(descriptors, &device.claimed_interfaces)?,
    ))
}
fn check_gen1_binding(
    binding: &Gen1Binding,
    device: &UsbDevice,
    configuration: u8,
    descriptors: &[u8],
) -> Result<(), RoutingSessionError> {
    if !Arc::ptr_eq(&binding.device._fd_owner, &device._fd_owner)
        || binding.configuration != configuration
        || binding.descriptors != descriptors
    {
        return Err(RoutingSessionError::Drift(
            "Gen1 attachment or configuration changed".into(),
        ));
    }
    gen1_interface(descriptors, &device.claimed_interfaces)?;
    Ok(())
}
fn check_gen1_after(
    device: &UsbDevice,
    configuration: u8,
    descriptors: &[u8],
) -> Result<(), RoutingSessionError> {
    let (after, raw) = active_descriptors(device)?;
    if after != configuration || raw != descriptors {
        return Err(RoutingSessionError::Drift(
            "Gen1 configuration changed during control access".into(),
        ));
    }
    gen1_target(device, &raw)?;
    Ok(())
}

pub(crate) fn scarlett_gen1_read() -> Result<crate::scarlett::Gen1Readback, RoutingSessionError> {
    require_vendor_routing_release()?;
    let _gate = USB_SESSION_GATE.lock();
    let _controls = HARDWARE_VOLUME_MUTEX.lock();
    let device = idle_device()?;
    let (configuration, descriptors) = active_descriptors(&device)?;
    let (model, interface) = gen1_target(&device, &descriptors)?;
    let readback = crate::scarlett::read_gen1(
        &mut UsbGen1Io {
            device: &device,
            interface,
        },
        model,
    )
    .map_err(RoutingSessionError::Gen1Read);
    let after = check_gen1_after(&device, configuration, &descriptors);
    match readback {
        Ok(readback) => {
            after?;
            Ok(readback)
        }
        Err(error) => {
            if let Err(drift) = after {
                log_warn!("[USB] Gen1 read also lost binding: {drift}");
            }
            Err(error)
        }
    }
}

// Keep independently owned controls even when a later read contains external
// changes to them. Such external values must never become a restore target.
fn merge_gen1_provenance(
    original: &crate::scarlett::Gen1Readback,
    last: &crate::scarlett::Gen1Readback,
    before: &crate::scarlett::Gen1Readback,
    verified: &crate::scarlett::Gen1Readback,
) -> (crate::scarlett::Gen1Readback, crate::scarlett::Gen1Readback) {
    let mut saved = before.clone();
    let mut recent = verified.clone();
    for (index, control) in before.controls.iter().enumerate() {
        if let (Some(old), Some(previous)) = (
            original.controls.iter().find(|c| c.key == control.key),
            last.controls.iter().find(|c| c.key == control.key),
        ) {
            if old != previous {
                saved.controls[index] = *old;
                if control == &verified.controls[index] {
                    recent.controls[index] = *previous;
                }
            }
        }
    }
    (saved, recent)
}

pub(crate) fn scarlett_gen1_apply(
    expected: &crate::scarlett::Gen1Readback,
    edits: &[crate::scarlett::Gen1Edit],
) -> Result<crate::scarlett::Gen1Report, RoutingSessionError> {
    require_vendor_routing_release()?;
    let _gate = USB_SESSION_GATE.lock();
    let _controls = HARDWARE_VOLUME_MUTEX.lock();
    let device = idle_device()?;
    let (configuration, descriptors) = active_descriptors(&device)?;
    let (model, interface) = gen1_target(&device, &descriptors)?;
    let binding = Gen1Binding {
        device: device.clone(),
        configuration,
        descriptors: descriptors.clone(),
    };
    if expected.model != model {
        return Err(RoutingSessionError::Drift(
            "Gen1 readback belongs to another model".into(),
        ));
    }
    let mut records = ORIGINAL_GEN1.lock();
    if let Some(record) = records.as_ref() {
        check_gen1_binding(&record.binding, &device, configuration, &descriptors)?;
        if record.uncertain || !crate::scarlett::same_gen1_layout(&record.last, expected) {
            return Err(RoutingSessionError::Drift(
                "Gen1 layout or rollback state changed".into(),
            ));
        }
        for control in &expected.controls {
            if edits.iter().any(|e| e.key == control.key) {
                if let (Some(old), Some(last)) = (
                    record
                        .original
                        .controls
                        .iter()
                        .find(|c| c.key == control.key),
                    record.last.controls.iter().find(|c| c.key == control.key),
                ) {
                    if old != last && control != last {
                        return Err(RoutingSessionError::Drift(
                            "Gen1 control differs from last verified write".into(),
                        ));
                    }
                }
            }
        }
    }
    let result = crate::scarlett::apply_gen1(
        &mut UsbGen1Io {
            device: &device,
            interface,
        },
        model,
        expected,
        edits,
    );
    // Invalidate on attempted transactions, including errors and uncertain rollback.
    invalidate_route_caches();
    let report = match result {
        Ok(report) => report,
        Err(failure) => {
            if !failure.rollback.is_empty() {
                let record = records.get_or_insert_with(|| RecordedGen1 {
                    binding: binding.clone(),
                    original: expected.clone(),
                    last: expected.clone(),
                    uncertain: true,
                });
                record.uncertain = true;
            }
            // Recheck actual USB identity/config even on transaction errors.
            if let Err(drift) = check_gen1_after(&device, configuration, &descriptors) {
                log_warn!("[USB] Gen1 transaction also lost binding: {drift}");
            }
            return Err(RoutingSessionError::Gen1(failure));
        }
    };
    if report.original != report.verified {
        let record = records.get_or_insert_with(|| RecordedGen1 {
            binding: binding.clone(),
            original: report.original.clone(),
            last: report.verified.clone(),
            uncertain: false,
        });
        let (original, last) = merge_gen1_provenance(
            &record.original,
            &record.last,
            &report.original,
            &report.verified,
        );
        record.original = original;
        record.last = last;
    }
    check_gen1_after(&device, configuration, &descriptors)?;
    Ok(report)
}

fn finish_gen1_hand_back() {
    let Some(record) = ORIGINAL_GEN1.lock().take() else {
        return;
    };
    if settings().config.hand_back != crate::HandBack::RestoreOriginal {
        return;
    }
    let Some(device) = SESSION_STATE.lock().as_ref().map(|s| s.device.clone()) else {
        return;
    };
    let restore = || -> Result<(), RoutingSessionError> {
        let (configuration, descriptors) = active_descriptors(&device)?;
        check_gen1_binding(&record.binding, &device, configuration, &descriptors)?;
        let (model, interface) = gen1_target(&device, &descriptors)?;
        let result = crate::scarlett::restore_gen1(
            &mut UsbGen1Io {
                device: &device,
                interface,
            },
            model,
            &record.last,
            &record.original,
        )
        .map_err(RoutingSessionError::Gen1);
        invalidate_route_caches();
        let after = check_gen1_after(&device, configuration, &descriptors);
        match result {
            Ok(()) => after,
            Err(error) => {
                if let Err(drift) = after {
                    log_warn!("[USB] Gen1 restoration also lost binding: {drift}");
                }
                Err(error)
            }
        }
    };
    if let Err(error) = restore() {
        log_warn!("[USB] Original Gen1 controls not restored: {error}");
    }
}

pub(super) struct RecordedFcp {
    device: UsbDevice,
    configuration: u8,
    descriptors: Vec<u8>,
    original: crate::scarlett::FcpReadback,
    last: crate::scarlett::FcpReadback,
    uncertain: bool,
}
static ORIGINAL_FCP: context::ScopedMutex<Option<RecordedFcp>> =
    context::ScopedMutex::new(|context| context.fcp.clone());

pub(crate) fn fcp_apply(
    expected: &crate::scarlett::FcpReadback,
    routes: &[crate::scarlett::FcpRouteEdit],
    mixes: &[crate::scarlett::FcpMixEdit],
) -> Result<crate::scarlett::FcpWriteReport, RoutingSessionError> {
    require_vendor_routing_release()?;
    let _gate = USB_SESSION_GATE.lock();
    let _controls = HARDWARE_VOLUME_MUTEX.lock();
    let device = idle_device()?;
    let (configuration, descriptors) = active_descriptors(&device)?;
    let mut records = ORIGINAL_FCP.lock();
    if let Some(record) = records.as_ref() {
        if !Arc::ptr_eq(&record.device._fd_owner, &device._fd_owner)
            || record.configuration != configuration
            || record.descriptors != descriptors
            || record.uncertain
            || !crate::scarlett::same_fcp_layout(&record.last, expected)
        {
            return Err(RoutingSessionError::Drift(
                "FCP attachment/configuration or rollback state changed".into(),
            ));
        }
        for bank in &expected.banks {
            if routes.iter().any(|edit| edit.bank == bank.bank) {
                if let (Some(original), Some(last)) = (
                    record.original.banks.iter().find(|b| b.bank == bank.bank),
                    record.last.banks.iter().find(|b| b.bank == bank.bank),
                ) {
                    if original != last && bank != last {
                        return Err(RoutingSessionError::Drift(
                            "FCP bank differs from last verified library write".into(),
                        ));
                    }
                }
            }
        }
        for row in &expected.mixes {
            if mixes.iter().any(|edit| edit.output == row.output) {
                if let (Some(original), Some(last)) = (
                    record
                        .original
                        .mixes
                        .iter()
                        .find(|r| r.output == row.output),
                    record.last.mixes.iter().find(|r| r.output == row.output),
                ) {
                    if original != last && row != last {
                        return Err(RoutingSessionError::Drift(
                            "FCP row differs from last verified library write".into(),
                        ));
                    }
                }
            }
        }
    }
    let report = match with_scarlett_io(&device, configuration, true, |io, candidate| {
        crate::scarlett::apply_fcp(io, candidate, expected, routes, mixes)
            .map_err(RoutingSessionError::Fcp)
    }) {
        Ok(report) => report,
        Err(error) => {
            if let RoutingSessionError::Fcp(failure) = &error {
                invalidate_route_caches();
                if !failure.rollback.is_empty() {
                    let record = records.get_or_insert_with(|| RecordedFcp {
                        device: device.clone(),
                        configuration,
                        descriptors: descriptors.clone(),
                        original: expected.clone(),
                        last: expected.clone(),
                        uncertain: true,
                    });
                    record.uncertain = true;
                    log_warn!("[USB] FCP rollback uncertain; configured hand-back will preserve targets differing from prior verified state");
                }
            }
            return Err(error);
        }
    };
    if report.original != report.verified {
        invalidate_route_caches();
        let record = records.get_or_insert_with(|| RecordedFcp {
            device: device.clone(),
            configuration,
            descriptors: descriptors.clone(),
            original: report.original.clone(),
            last: report.verified.clone(),
            uncertain: false,
        });
        let (original, last) = merge_fcp_provenance(
            &record.original,
            &record.last,
            &report.original,
            &report.verified,
        );
        record.original = original;
        record.last = last;
    }
    let (after, raw) = active_descriptors(&device)?;
    if configuration != after || descriptors != raw {
        return Err(RoutingSessionError::Drift(
            "configuration changed during FCP routing write".into(),
        ));
    }
    Ok(report)
}

// Preserve per-target first-before/last-verified pairs. A later read may contain
// unrelated external edits; those must never become owned hand-back changes.
fn merge_fcp_provenance(
    original: &crate::scarlett::FcpReadback,
    last: &crate::scarlett::FcpReadback,
    before: &crate::scarlett::FcpReadback,
    verified: &crate::scarlett::FcpReadback,
) -> (crate::scarlett::FcpReadback, crate::scarlett::FcpReadback) {
    let mut merged_original = before.clone();
    let mut merged_last = verified.clone();
    for (index, bank) in before.banks.iter().enumerate() {
        if let (Some(old), Some(previous)) = (
            original.banks.iter().find(|b| b.bank == bank.bank),
            last.banks.iter().find(|b| b.bank == bank.bank),
        ) {
            if old != previous {
                merged_original.banks[index] = old.clone();
                if bank == &verified.banks[index] {
                    merged_last.banks[index] = previous.clone();
                }
            }
        }
    }
    for (index, row) in before.mixes.iter().enumerate() {
        if let (Some(old), Some(previous)) = (
            original.mixes.iter().find(|r| r.output == row.output),
            last.mixes.iter().find(|r| r.output == row.output),
        ) {
            if old != previous {
                merged_original.mixes[index] = old.clone();
                if row == &verified.mixes[index] {
                    merged_last.mixes[index] = previous.clone();
                }
            }
        }
    }
    (merged_original, merged_last)
}

fn finish_fcp_hand_back() {
    let Some(record) = ORIGINAL_FCP.lock().take() else {
        return;
    };
    if settings().config.hand_back != crate::HandBack::RestoreOriginal {
        return;
    }
    if record.uncertain {
        log_warn!("[USB] FCP rollback uncertain; hand-back will restore only targets matching prior verified values");
    }
    let Some(device) = SESSION_STATE.lock().as_ref().map(|s| s.device.clone()) else {
        return;
    };
    let restore = || -> Result<(), RoutingSessionError> {
        if !Arc::ptr_eq(&record.device._fd_owner, &device._fd_owner) {
            return Err(RoutingSessionError::Drift(
                "FCP attachment changed before hand-back".into(),
            ));
        }
        let (number, raw) = active_descriptors(&device)?;
        if number != record.configuration || raw != record.descriptors {
            return Err(RoutingSessionError::Drift(
                "FCP configuration changed before hand-back".into(),
            ));
        }
        with_scarlett_io(&device, number, true, |io, candidate| {
            crate::scarlett::restore_fcp(io, candidate, &record.last, &record.original)
                .map_err(RoutingSessionError::Fcp)
        })
    };
    if let Err(error) = restore() {
        log_warn!("[USB] Original FCP routing not restored: {error}");
    }
}

pub(crate) fn scarlett_mix_apply(
    edit: &crate::scarlett::MixRowEdit,
) -> Result<crate::scarlett::MixWriteReport, RoutingSessionError> {
    require_vendor_routing_release()?;
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
    require_vendor_routing_release()?;
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
    require_vendor_routing_release()?;
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
    if !VENDOR_ROUTING_RELEASED {
        ORIGINAL_GEN1.lock().take();
        ORIGINAL_FCP.lock().take();
        ORIGINAL_SCARLETT.lock().take();
        return;
    }
    finish_gen1_hand_back();
    finish_fcp_hand_back();
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

const ROUTING_OPERATION_BUDGET: Duration = Duration::from_secs(4);
const ROUTING_RECOVERY_BUDGET: Duration = Duration::from_secs(2);
fn remaining(deadline: Instant) -> Result<Duration, RoutingSessionError> {
    let left = deadline.saturating_duration_since(Instant::now());
    if left.is_zero() {
        return Err(RoutingSessionError::Session(Error::Control(
            "routing deadline expired".into(),
        )));
    }
    Ok(left.min(Duration::from_secs(1)))
}

struct UsbRoutingIo<'a> {
    deadline: Instant,
    recovery: Arc<Mutex<Option<Instant>>>,
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
        input && (2..=2 + routing::MAX_GAIN_RANGES * 6).contains(&length) && (length - 2) % 6 == 0
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
        let interfaces =
            routing_interfaces_unambiguous(self.descriptors, &self.device.claimed_interfaces)
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
            .feature_unit_control_timed(
                r.request_type,
                r.request,
                r.value,
                r.index,
                bytes,
                remaining(self.deadline).map_err(|e| e.to_string())?,
            )
            .map_err(|e| e.to_string())
    }
    fn write(&mut self, r: ControlRequest, bytes: &[u8]) -> Result<usize, String> {
        self.validate(r, false, bytes.len())?;
        let mut data = bytes.to_vec();
        self.device
            .feature_unit_control_timed(
                r.request_type,
                r.request,
                r.value,
                r.index,
                &mut data,
                remaining(self.deadline).map_err(|e| e.to_string())?,
            )
            .map_err(|e| e.to_string())
    }
    fn begin_recovery(&mut self) {
        let mut recovery = self.recovery.lock();
        self.deadline = *recovery.get_or_insert_with(|| Instant::now() + ROUTING_RECOVERY_BUDGET);
    }
}

fn idle_device() -> Result<UsbDevice, RoutingSessionError> {
    idle_device_until(None)
}
fn idle_device_until(deadline: Option<Instant>) -> Result<UsbDevice, RoutingSessionError> {
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
    let state = match deadline {
        Some(deadline) => SESSION_STATE
            .try_lock_for(remaining(deadline)?)
            .ok_or_else(|| {
                RoutingSessionError::Session(Error::Control(
                    "routing deadline waiting for session state".into(),
                ))
            })?,
        None => SESSION_STATE.lock(),
    };
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
    active_descriptors_until(device, None)
}
fn active_descriptors_until(
    device: &UsbDevice,
    deadline: Option<Instant>,
) -> Result<(u8, Vec<u8>), RoutingSessionError> {
    let idle = match deadline {
        Some(deadline) => IDLE_LOCK
            .try_lock_for(remaining(deadline)?)
            .ok_or_else(|| {
                RoutingSessionError::Session(Error::Control(
                    "routing deadline waiting for idle owner".into(),
                ))
            })?,
        None => IDLE_LOCK.lock(),
    };
    let idle = idle
        .as_ref()
        .filter(|idle| idle.device_fd == device.fd)
        .ok_or_else(|| {
            RoutingSessionError::Session(Error::InvalidState(
                "routing requires the idle USB owner".into(),
            ))
        })?;
    if let Some(deadline) = deadline {
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
        let read_config = |value: u16, bytes: &mut [u8]| -> Result<usize, RoutingSessionError> {
            idle.handle
                .read_control(0x80, 6, value, 0, bytes, remaining(deadline)?)
                .map_err(|e| RoutingSessionError::Session(Error::Control(e.to_string())))
        };
        let read_active = || -> Result<u8, RoutingSessionError> {
            let mut number = [0];
            let n = idle
                .handle
                .read_control(0x80, 8, 0, 0, &mut number, remaining(deadline)?)
                .map_err(|e| RoutingSessionError::Session(Error::Control(e.to_string())))?;
            if n != 1 {
                return Err(RoutingSessionError::Session(Error::Control(
                    "short GET_CONFIGURATION".into(),
                )));
            }
            Ok(number[0])
        };
        let active = read_active()?;
        for index in 0..identity.num_configurations() {
            let mut header = [0u8; 9];
            if read_config(0x0200 | u16::from(index), &mut header)? != header.len() {
                continue;
            }
            if header[0] != 9 || header[1] != 2 {
                return Err(RoutingSessionError::Invalid(
                    InvalidRoute::MalformedDescriptor,
                ));
            }
            if header[5] != active {
                continue;
            }
            let length = usize::from(u16::from_le_bytes([header[2], header[3]]));
            if length < header.len() {
                return Err(RoutingSessionError::Invalid(
                    InvalidRoute::MalformedDescriptor,
                ));
            }
            let mut raw = vec![0; length];
            if read_config(0x0200 | u16::from(index), &mut raw)? != length
                || raw[..header.len()] != header
                || read_active()? != active
            {
                return Err(RoutingSessionError::Drift(
                    "active configuration changed during discovery".into(),
                ));
            }
            return Ok((active, raw));
        }
        return Err(RoutingSessionError::Drift(
            "active configuration descriptor unavailable".into(),
        ));
    }
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
    deadline: Instant,
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
            remaining(deadline)?,
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
            remaining(deadline)?,
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
    Unknown,
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
            if clusters.is_empty() || clusters.iter().any(|c| c.channels != clusters[0].channels) {
                return Err(RoutingSessionError::UnsupportedTopology(
                    "selector input widths differ or are empty".into(),
                ));
            }
            let first = clusters.remove(0);
            return Ok(if clusters.iter().all(|c| c.identity == first.identity) {
                first
            } else {
                Cluster {
                    channels: first.channels,
                    identity: ClusterIdentity::Unknown,
                }
            });
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
    let (interfaces, diagnostics) = routing_interfaces_with_diagnostics(raw, claims)?;
    if !diagnostics.is_empty() {
        return Err(RoutingSessionError::Invalid(InvalidRoute::DuplicateEntity));
    }
    Ok(interfaces)
}
fn routing_interfaces_unambiguous<'a>(
    raw: &'a [u8],
    claims: &[u8],
) -> Result<BTreeMap<u8, (u8, Vec<&'a [u8]>)>, RoutingSessionError> {
    Ok(routing_interfaces_with_diagnostics(raw, claims)?.0)
}

fn routing_interfaces_with_diagnostics<'a>(
    raw: &'a [u8],
    claims: &[u8],
) -> Result<(BTreeMap<u8, (u8, Vec<&'a [u8]>)>, Vec<RoutingDiagnostic>), RoutingSessionError> {
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
    let mut ambiguous = std::collections::BTreeSet::new();
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
                if ambiguous.contains(&bytes[2]) {
                    continue;
                }
                if interfaces
                    .insert(bytes[2], (bytes[7], Vec::new()))
                    .is_some()
                {
                    interfaces.remove(&bytes[2]);
                    ambiguous.insert(bytes[2]);
                    continue;
                }
                current = Some(bytes[2]);
            }
        } else if bytes[1] == 0x24 {
            if let Some(interface) = current {
                interfaces.get_mut(&interface).unwrap().1.push(bytes);
            }
        }
    }
    let diagnostics = ambiguous
        .into_iter()
        .map(|interface| RoutingDiagnostic {
            interface,
            entity_id: None,
            detail: "ambiguous AudioControl function".into(),
        })
        .collect();
    Ok((interfaces, diagnostics))
}

#[cfg(test)]
fn inventory_from_config(
    raw: &[u8],
    claims: &[u8],
) -> Result<RoutingInventory, RoutingSessionError> {
    inventory_with_clusters(raw, claims, &BTreeMap::new())
}
#[cfg(test)]
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

fn inventory_partial(
    raw: &[u8],
    claims: &[u8],
    hc: &BTreeMap<(u8, u16), RoutingCluster>,
) -> Result<(RoutingInventory, Vec<RoutingDiagnostic>), RoutingSessionError> {
    let mut inventory = RoutingInventory::default();
    let mut diagnostics = Vec::new();
    for (interface, (protocol, descriptors)) in routing_interfaces_unambiguous(raw, claims)? {
        let function = (|| -> Result<(), RoutingSessionError> {
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
                let unit = (|| -> Result<RoutingUnit, RoutingSessionError> {
                    if bytes[2] == selector {
                        return Ok(RoutingUnit::Selector(parse_selector(
                            interface, bytes, protocol,
                        )?));
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
                    Ok(unit)
                })();
                match unit {
                    Ok(unit) => inventory
                        .insert(unit)
                        .map_err(RoutingSessionError::Invalid)?,
                    Err(error) => diagnostics.push(RoutingDiagnostic {
                        interface,
                        entity_id: Some(bytes[3]),
                        detail: error.to_string(),
                    }),
                }
            }
            Ok(())
        })();
        if let Err(error) = function {
            diagnostics.push(RoutingDiagnostic {
                interface,
                entity_id: None,
                detail: error.to_string(),
            });
        }
    }
    Ok((inventory, diagnostics))
}

/// Descriptor-only scope proof shared by no-wake discovery and explicit wake.
#[cfg(test)]
fn adc3_routing_scope(
    descriptors: &[&[u8]],
) -> Result<
    (
        Vec<crate::descriptors::uac3::PowerDomain>,
        std::collections::BTreeSet<u8>,
    ),
    RoutingSessionError,
> {
    adc3_routing_scope_for(descriptors, None)
}

fn adc3_routing_scope_for(
    descriptors: &[&[u8]],
    root: Option<u8>,
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
        .filter(|b| matches!(b[2], 5 | 6) && root.map_or(true, |id| b[3] == id))
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
    deadline: Instant,
    recovery: &Arc<Mutex<Option<Instant>>>,
    keys: Option<&[ControlKey]>,
    operation: impl FnOnce() -> Result<T, RoutingSessionError>,
) -> Result<T, RoutingSessionError> {
    if !powered {
        return operation();
    }
    let guard = RoutingPowerLease(super::adc3::Adc3PowerLease::new(device.fd));
    let lease = &guard.0;
    lease.set_routing_deadline(deadline);
    let result = (|| {
        let mut scopes = Vec::new();
        for (interface, (protocol, descriptors)) in
            routing_interfaces_unambiguous(raw, &device.claimed_interfaces)?
        {
            if protocol != 0x30 {
                continue;
            }
            let roots: Vec<_> = descriptors
                .iter()
                .filter(|d| d.len() >= 4 && matches!(d[2], 5 | 6))
                .map(|d| d[3])
                .filter(|id| {
                    keys.map_or(true, |keys| {
                        keys.iter().any(|key| match key {
                            ControlKey::Selector(entity)
                            | ControlKey::Crosspoint { entity, .. } => {
                                entity.interface == interface && entity.id == *id
                            }
                        })
                    })
                })
                .collect();
            let mut domains = Vec::new();
            for root in roots {
                match adc3_routing_scope_for(&descriptors, Some(root)) {
                    Ok((found, _)) => {
                        for domain in found {
                            if !domains.iter().any(
                                |existing: &crate::descriptors::uac3::PowerDomain| {
                                    existing.id == domain.id
                                },
                            ) {
                                domains.push(domain);
                            }
                        }
                    }
                    Err(error) if keys.is_some() => return Err(error),
                    Err(_) => continue,
                }
            }
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
    let recovery_deadline = *recovery
        .lock()
        .get_or_insert_with(|| Instant::now() + ROUTING_RECOVERY_BUDGET);
    lease.set_routing_deadline(recovery_deadline);
    finish_routing_power(result, lease.restore_checked())
}

fn finish_routing_power<T>(
    operation: Result<T, RoutingSessionError>,
    restoration: Result<(), String>,
) -> Result<T, RoutingSessionError> {
    match restoration {
        Ok(()) => operation,
        Err(detail) => Err(RoutingSessionError::PowerRestore {
            operation: operation.err().map(Box::new),
            detail,
        }),
    }
}

/// Query only already claimed AC functions. Sleeping/unknown domains are a
/// typed refusal: capability discovery never changes a device's power state.
fn routing_clusters(
    device: &UsbDevice,
    raw: &[u8],
    deadline: Instant,
    filter: Option<&[ControlKey]>,
) -> Result<(BTreeMap<(u8, u16), RoutingCluster>, Vec<RoutingDiagnostic>), RoutingSessionError> {
    let interfaces = routing_interfaces_unambiguous(raw, &device.claimed_interfaces)?;
    let idle = IDLE_LOCK
        .try_lock_for(remaining(deadline)?)
        .ok_or_else(|| {
            RoutingSessionError::Session(Error::Control(
                "routing deadline waiting for idle owner".into(),
            ))
        })?;
    let idle = idle
        .as_ref()
        .filter(|i| i.device_fd == device.fd)
        .ok_or_else(|| {
            RoutingSessionError::Session(Error::InvalidState("routing lost idle owner".into()))
        })?;
    let mut clusters = BTreeMap::new();
    let mut diagnostics = Vec::new();
    for (interface, (protocol, descriptors)) in interfaces {
        if protocol != 0x30 {
            continue;
        }
        let roots: Vec<_> = descriptors
            .iter()
            .filter(|d| d.len() >= 4 && matches!(d[2], 5 | 6))
            .map(|d| d[3])
            .collect();
        for root in roots {
            if filter.is_some_and(|keys| {
                !keys.iter().any(|key| match key {
                    ControlKey::Selector(entity) | ControlKey::Crosspoint { entity, .. } => {
                        entity.interface == interface && entity.id == root
                    }
                })
            }) {
                continue;
            }
            let result =
                (|| -> Result<BTreeMap<(u8, u16), RoutingCluster>, RoutingSessionError> {
                    let mut local = BTreeMap::new();
                    let (domains, affected) = adc3_routing_scope_for(&descriptors, Some(root))?;
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
                                remaining(deadline)?,
                            )
                            .map_err(|e| {
                                RoutingSessionError::Session(Error::Control(e.to_string()))
                            })?;
                        if n != 1 || state[0] != 0 {
                            return Err(RoutingSessionError::UnsupportedTopology("ADC3 routing requires already-D0 entity domains; waking requires an owned power lease".into()));
                        }
                    }
                    for bytes in &descriptors {
                        if bytes.len() < 4 || !affected.contains(&bytes[3]) {
                            continue;
                        }
                        let id = match bytes.get(2) {
                            Some(2) => Some(
                                crate::descriptors::uac3::parse_input_terminal(bytes)
                                    .map_err(|e| {
                                        RoutingSessionError::UnsupportedTopology(e.to_string())
                                    })?
                                    .cluster_id,
                            ),
                            Some(5) => {
                                let pins = usize::from(*bytes.get(4).ok_or(
                                    RoutingSessionError::Invalid(InvalidRoute::MalformedDescriptor),
                                )?);
                                let pair = bytes.get(5 + pins..7 + pins).ok_or(
                                    RoutingSessionError::Invalid(InvalidRoute::MalformedDescriptor),
                                )?;
                                Some(u16::from_le_bytes([pair[0], pair[1]]))
                            }
                            _ => None,
                        };
                        if let Some(id) = id {
                            if let std::collections::btree_map::Entry::Vacant(e) =
                                local.entry((interface, id))
                            {
                                let cluster =
                                    query_routing_cluster(&idle.handle, interface, id, deadline)?;
                                e.insert(cluster);
                            }
                        }
                    }
                    Ok(local)
                })();
            match result {
                Ok(local) => clusters.extend(local),
                Err(error) => diagnostics.push(RoutingDiagnostic {
                    interface,
                    entity_id: Some(root),
                    detail: error.to_string(),
                }),
            }
        }
    }
    Ok((clusters, diagnostics))
}

fn verify_descriptor_identity(
    snapshot: &RoutingSnapshot,
    device: &UsbDevice,
    deadline: Instant,
) -> Result<(), RoutingSessionError> {
    if !Arc::ptr_eq(&snapshot.device._fd_owner, &device._fd_owner) {
        return Err(RoutingSessionError::Drift(
            "routing snapshot belongs to another attachment".into(),
        ));
    }
    let (configuration, descriptors) = active_descriptors_until(device, Some(deadline))?;
    remaining(deadline)?;
    if configuration != snapshot.configuration || descriptors != snapshot.descriptors {
        return Err(RoutingSessionError::Drift(
            "configuration or descriptors changed; rediscover routing".into(),
        ));
    }
    Ok(())
}
fn selector_dependencies(
    snapshot: &RoutingSnapshot,
    key: ControlKey,
) -> Result<Vec<RouteChange>, RoutingSessionError> {
    let entity = match key {
        ControlKey::Selector(_) => return Ok(Vec::new()),
        ControlKey::Crosspoint { entity, .. } => entity,
    };
    let interfaces =
        routing_interfaces_unambiguous(&snapshot.descriptors, &snapshot.device.claimed_interfaces)?;
    let (protocol, descriptors) = interfaces
        .get(&entity.interface)
        .ok_or_else(|| RoutingSessionError::Drift("routing function disappeared".into()))?;
    let nodes: BTreeMap<_, _> = descriptors
        .iter()
        .filter(|d| d.len() >= 4 && d[2] != 1)
        .map(|d| (d[3], *d))
        .collect();
    let mut pending = vec![entity.id];
    let mut seen = std::collections::BTreeSet::new();
    let mut dependencies = Vec::new();
    while let Some(id) = pending.pop() {
        if !seen.insert(id) {
            continue;
        }
        let bytes = nodes
            .get(&id)
            .ok_or_else(|| RoutingSessionError::Drift("routing source unavailable".into()))?;
        let mixer = if *protocol == 0x30 { 5 } else { 4 };
        let selector = if *protocol == 0x30 { 6 } else { 5 };
        let feature = if *protocol == 0x30 { 7 } else { 6 };
        if bytes[2] == selector {
            let selector_key = ControlKey::Selector(routing::Entity {
                interface: entity.interface,
                id,
            });
            let expected = snapshot
                .selectors
                .iter()
                .find(|v| v.key == selector_key)
                .ok_or_else(|| {
                    RoutingSessionError::Drift(format!("selector {id} is unavailable"))
                })?;
            dependencies.push(*expected);
        }
        if bytes[2] == selector || bytes[2] == mixer {
            let count = usize::from(*bytes.get(4).ok_or(RoutingSessionError::Invalid(
                InvalidRoute::MalformedDescriptor,
            ))?);
            pending.extend_from_slice(bytes.get(5..5 + count).ok_or(
                RoutingSessionError::Invalid(InvalidRoute::MalformedDescriptor),
            )?);
        } else if bytes[2] == feature {
            pending.push(*bytes.get(4).ok_or(RoutingSessionError::Invalid(
                InvalidRoute::MalformedDescriptor,
            ))?);
        }
    }
    dependencies.sort_by_key(|v| v.key);
    Ok(dependencies)
}

fn cluster_dependencies(
    snapshot: &RoutingSnapshot,
    key: ControlKey,
) -> Result<Vec<(u8, u16)>, RoutingSessionError> {
    let entity = match key {
        ControlKey::Selector(entity) | ControlKey::Crosspoint { entity, .. } => entity,
    };
    let interfaces =
        routing_interfaces_unambiguous(&snapshot.descriptors, &snapshot.device.claimed_interfaces)?;
    let Some((protocol, descriptors)) = interfaces.get(&entity.interface) else {
        return Err(RoutingSessionError::Drift(
            "routing function disappeared".into(),
        ));
    };
    if *protocol != 0x30 {
        return Ok(Vec::new());
    }
    let (_, affected) = adc3_routing_scope_for(descriptors, Some(entity.id))?;
    let mut ids = Vec::new();
    for bytes in descriptors
        .iter()
        .filter(|b| b.len() >= 4 && affected.contains(&b[3]))
    {
        let id = match bytes[2] {
            2 => Some(
                crate::descriptors::uac3::parse_input_terminal(bytes)
                    .map_err(|e| RoutingSessionError::UnsupportedTopology(e.to_string()))?
                    .cluster_id,
            ),
            5 => {
                let count = usize::from(*bytes.get(4).ok_or(RoutingSessionError::Invalid(
                    InvalidRoute::MalformedDescriptor,
                ))?);
                let pair = bytes
                    .get(5 + count..7 + count)
                    .ok_or(RoutingSessionError::Invalid(
                        InvalidRoute::MalformedDescriptor,
                    ))?;
                Some(u16::from_le_bytes([pair[0], pair[1]]))
            }
            _ => None,
        };
        if let Some(id) = id {
            ids.push((entity.interface, id));
        }
    }
    Ok(ids)
}

// Compare the full captured HC content, not just the channel count used to
// establish address offsets. Both edits and hand-back use this same proof.
fn cluster_content_matches(
    current: &BTreeMap<(u8, u16), RoutingCluster>,
    saved: &BTreeMap<(u8, u16), RoutingCluster>,
    dependencies: &[(u8, u16)],
) -> bool {
    dependencies
        .iter()
        .all(|id| current.contains_key(id) && current.get(id) == saved.get(id))
}

fn verify_snapshot(
    snapshot: &RoutingSnapshot,
    device: &UsbDevice,
    deadline: Instant,
    recovery: &Arc<Mutex<Option<Instant>>>,
    keys: &[ControlKey],
) -> Result<(), RoutingSessionError> {
    verify_descriptor_identity(snapshot, device, deadline)?;
    let (clusters, failures) =
        routing_clusters(device, &snapshot.descriptors, deadline, Some(keys))?;
    for &key in keys {
        let entity = match key {
            ControlKey::Selector(e) | ControlKey::Crosspoint { entity: e, .. } => e,
        };
        if failures.iter().any(|failure| {
            failure.interface == entity.interface
                && failure.entity_id.map_or(true, |id| id == entity.id)
        }) || !cluster_content_matches(
            &clusters,
            &snapshot.hc_clusters,
            &cluster_dependencies(snapshot, key)?,
        ) {
            return Err(RoutingSessionError::Drift(
                "ADC3 HC Clusters changed or unavailable for this control".into(),
            ));
        }
    }
    let mut io = UsbRoutingIo {
        deadline,
        recovery: recovery.clone(),
        device,
        descriptors: &snapshot.descriptors,
    };
    for &key in keys {
        let expected = selector_dependencies(snapshot, key)?;
        verify_values(&snapshot.inventory, &mut io, &expected)?;
    }
    Ok(())
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
    let deadline = Instant::now() + ROUTING_OPERATION_BUDGET;
    let recovery = Arc::new(Mutex::new(None));
    let _gate = USB_SESSION_GATE
        .try_lock_for(remaining(deadline)?)
        .ok_or_else(|| {
            RoutingSessionError::Session(Error::Control(
                "routing deadline waiting for session gate".into(),
            ))
        })?;
    let _controls = HARDWARE_VOLUME_MUTEX
        .try_lock_for(remaining(deadline)?)
        .ok_or_else(|| {
            RoutingSessionError::Session(Error::Control(
                "routing deadline waiting for control gate".into(),
            ))
        })?;
    let device = idle_device_until(Some(deadline))?;
    let (configuration, descriptors) = active_descriptors_until(&device, Some(deadline))?;
    with_routing_power(
        &device,
        &descriptors,
        powered,
        deadline,
        &recovery,
        None,
        || {
            let (_, mut diagnostics) =
                routing_interfaces_with_diagnostics(&descriptors, &device.claimed_interfaces)?;
            let (hc_clusters, cluster_diagnostics) =
                routing_clusters(&device, &descriptors, deadline, None)?;
            diagnostics.extend(cluster_diagnostics);
            let (inventory, inventory_diagnostics) =
                inventory_partial(&descriptors, &device.claimed_interfaces, &hc_clusters)?;
            diagnostics.extend(inventory_diagnostics);
            let mut selectors = Vec::new();
            let mut io = UsbRoutingIo {
                deadline,
                recovery: recovery.clone(),
                device: &device,
                descriptors: &descriptors,
            };
            for unit in inventory.units() {
                if let RoutingUnit::Selector(selector) = unit {
                    if selector.access() != Access::Absent {
                        let key = ControlKey::Selector(selector.entity());
                        match inventory
                            .control(key)
                            .map_err(RoutingSessionError::Invalid)?
                            .read_current(&mut io)
                        {
                            Ok(value) => selectors.push(RouteChange { key, value }),
                            Err(error) => diagnostics.push(RoutingDiagnostic {
                                interface: selector.entity().interface,
                                entity_id: Some(selector.entity().id),
                                detail: format!("selector CUR unavailable: {error:?}"),
                            }),
                        }
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
                diagnostics,
                hand_back_report: context::current().routing_report.clone(),
            })
        },
    )
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
    let deadline = Instant::now() + ROUTING_OPERATION_BUDGET;
    let recovery = Arc::new(Mutex::new(None));
    let _gate = USB_SESSION_GATE
        .try_lock_for(remaining(deadline)?)
        .ok_or_else(|| {
            RoutingSessionError::Session(Error::Control(
                "routing deadline waiting for session gate".into(),
            ))
        })?;
    let _controls = HARDWARE_VOLUME_MUTEX
        .try_lock_for(remaining(deadline)?)
        .ok_or_else(|| {
            RoutingSessionError::Session(Error::Control(
                "routing deadline waiting for control gate".into(),
            ))
        })?;
    let device = idle_device_until(Some(deadline))?;
    verify_descriptor_identity(snapshot, &device, deadline)?;
    with_routing_power(
        &device,
        &snapshot.descriptors,
        powered,
        deadline,
        &recovery,
        Some(&[key]),
        || {
            verify_snapshot(snapshot, &device, deadline, &recovery, &[key])?;
            snapshot
                .inventory
                .control(key)
                .map_err(RoutingSessionError::Invalid)?
                .read_current(&mut UsbRoutingIo {
                    deadline,
                    recovery: recovery.clone(),
                    device: &device,
                    descriptors: &snapshot.descriptors,
                })
                .map_err(RoutingSessionError::Control)
        },
    )
}

#[derive(Debug)]
pub(super) struct RecordedRouting {
    snapshot: RoutingSnapshot,
    // The first actual pre-write CUR and bounded current ownership evidence.
    values: BTreeMap<ControlKey, RecordedRoute>,
    powered: bool,
}
#[derive(Debug)]
struct RecordedRoute {
    original: RouteValue,
    ownership: Ownership,
    selectors: Vec<RouteChange>,
    forfeited: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ownership {
    None,
    Verified(RouteValue),
    Uncertain {
        previous: Option<RouteValue>,
        target: RouteValue,
    },
}
impl Ownership {
    fn owns(self, value: RouteValue) -> bool {
        match self {
            Self::None => false,
            Self::Verified(current) => current == value,
            Self::Uncertain { previous, target } => previous == Some(value) || target == value,
        }
    }
}
impl RecordedRoute {
    fn record_attempt(
        &mut self,
        before: RouteValue,
        target: RouteValue,
        success: bool,
        rollback_failed: bool,
    ) {
        let prior_owned = before == self.original || self.ownership.owns(before);
        if !prior_owned {
            self.forfeited = true;
        }
        self.ownership = if self.forfeited {
            Ownership::None
        } else if success {
            Ownership::Verified(target)
        } else if rollback_failed {
            Ownership::Uncertain {
                previous: prior_owned.then_some(before),
                target,
            }
        } else {
            Ownership::Verified(before)
        };
    }
}

static ORIGINAL_ROUTING: context::ScopedMutex<Option<RecordedRouting>> =
    context::ScopedMutex::new(|context| context.routing.clone());

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
    let deadline = Instant::now() + ROUTING_OPERATION_BUDGET;
    let recovery = Arc::new(Mutex::new(None));
    let _gate = USB_SESSION_GATE
        .try_lock_for(remaining(deadline)?)
        .ok_or_else(|| {
            RoutingSessionError::Session(Error::Control(
                "routing deadline waiting for session gate".into(),
            ))
        })?;
    let _controls = HARDWARE_VOLUME_MUTEX
        .try_lock_for(remaining(deadline)?)
        .ok_or_else(|| {
            RoutingSessionError::Session(Error::Control(
                "routing deadline waiting for control gate".into(),
            ))
        })?;
    let device = idle_device_until(Some(deadline))?;
    verify_descriptor_identity(snapshot, &device, deadline)?;
    with_routing_power(
        &device,
        &snapshot.descriptors,
        powered,
        deadline,
        &recovery,
        Some(&edits.iter().map(|edit| edit.key).collect::<Vec<_>>()),
        || {
            verify_snapshot(
                snapshot,
                &device,
                deadline,
                &recovery,
                &edits.iter().map(|edit| edit.key).collect::<Vec<_>>(),
            )?;
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
            if originals.as_ref().is_some_and(|prior| {
                !Arc::ptr_eq(&prior.snapshot.device._fd_owner, &device._fd_owner)
            }) {
                return Err(RoutingSessionError::Drift(
                    "routing restoration record belongs to another attachment".into(),
                ));
            }
            let old_count = originals.as_ref().map_or(0, |prior| prior.values.len());
            let new_count = edits
                .iter()
                .map(|edit| edit.key)
                .collect::<std::collections::BTreeSet<_>>()
                .iter()
                .filter(|key| {
                    !originals
                        .as_ref()
                        .is_some_and(|r| r.values.contains_key(*key))
                })
                .count();
            if old_count + new_count > MAX_RECORDED_ROUTES {
                return Err(RoutingSessionError::Invalid(InvalidRoute::TooManyChanges));
            }
            if let Some(prior) = originals.as_ref() {
                for edit in edits {
                    if matches!(edit.key, ControlKey::Crosspoint { .. })
                        && prior.values.get(&edit.key).is_some_and(|entry| {
                            entry.selectors
                                != selector_dependencies(snapshot, edit.key).unwrap_or_default()
                        })
                    {
                        return Err(RoutingSessionError::Drift(
                            "mixer mapping changed since its first edit".into(),
                        ));
                    }
                }
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
                deadline,
                recovery: recovery.clone(),
                device: &device,
                descriptors: &snapshot.descriptors,
            };
            verify_values(&snapshot.inventory, &mut io, &expected)?;
            if let Some(recorded) = originals.as_mut() {
                for edit in edits {
                    if let Some(entry) = recorded.values.get_mut(&edit.key) {
                        if edit.expected != entry.original && !entry.ownership.owns(edit.expected) {
                            entry.forfeited = true;
                            entry.ownership = Ownership::None;
                        }
                    }
                }
            }
            let mut attempted = Vec::new();
            let result = routing::apply_routes_traced(
                &snapshot.inventory,
                &mut io,
                &changes,
                &mut attempted,
            );
            for (key, before, target) in attempted {
                let rollback_failed = match &result {
                    Ok(_) => false,
                    Err(failure) => failure
                        .rollback_failures
                        .iter()
                        .any(|(failed, _)| *failed == key),
                };
                let success = result.is_ok();
                // A new, fully rolled-back edit creates no outstanding ownership.
                if !success
                    && !rollback_failed
                    && !originals
                        .as_ref()
                        .is_some_and(|r| r.values.contains_key(&key))
                {
                    continue;
                }
                let recorded = originals.get_or_insert_with(|| RecordedRouting {
                    snapshot: snapshot.clone(),
                    values: BTreeMap::new(),
                    powered,
                });
                recorded.powered |= powered;
                let dependencies = selector_dependencies(snapshot, key)?;
                let entry = recorded.values.entry(key).or_insert_with(|| RecordedRoute {
                    original: before,
                    ownership: Ownership::None,
                    selectors: dependencies,
                    forfeited: false,
                });
                entry.record_attempt(before, target, success, rollback_failed);
            }
            match result {
                Ok(report) => {
                    if !report.changed.is_empty() {
                        invalidate_route_caches();
                    }
                    Ok(report)
                }
                Err(failure) => {
                    if !failure.rollback_failures.is_empty() {
                        invalidate_route_caches();
                    }
                    Err(RoutingSessionError::Transaction(failure))
                }
            }
        },
    )
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
    let sink = record.snapshot.hand_back_report.clone();
    let mut report = RoutingHandBackReport {
        controls: Vec::new(),
    };
    let fail_all = |report: &mut RoutingHandBackReport, reason: String| {
        report.controls = record
            .values
            .keys()
            .map(|&key| {
                (
                    key,
                    RoutingHandBackOutcome::IdentityOrTopologyChanged(reason.clone()),
                )
            })
            .collect();
    };
    if settings().config.hand_back != crate::HandBack::RestoreOriginal {
        report.controls = record
            .values
            .keys()
            .map(|&key| (key, RoutingHandBackOutcome::Disabled))
            .collect();
        *sink.lock() = Some(report);
        return;
    }
    let Some(device) = SESSION_STATE.lock().as_ref().map(|s| s.device.clone()) else {
        fail_all(&mut report, "attachment detached".into());
        *sink.lock() = Some(report);
        return;
    };
    let deadline = Instant::now() + ROUTING_RECOVERY_BUDGET;
    let recovery = Arc::new(Mutex::new(Some(deadline)));
    let mut restore = || -> Result<RoutingHandBackReport, RoutingSessionError> {
        if !Arc::ptr_eq(&record.snapshot.device._fd_owner, &device._fd_owner) {
            return Err(RoutingSessionError::Drift(
                "attachment changed before routing hand-back".into(),
            ));
        }
        let (number, raw) = active_descriptors_until(&device, Some(deadline))?;
        if number != record.snapshot.configuration || raw != record.snapshot.descriptors {
            return Err(RoutingSessionError::Drift(
                "configuration changed before routing hand-back".into(),
            ));
        }
        with_routing_power(
            &device,
            &raw,
            record.powered,
            deadline,
            &recovery,
            Some(&record.values.keys().copied().collect::<Vec<_>>()),
            || {
                let cluster_result = routing_clusters(
                    &device,
                    &raw,
                    deadline,
                    Some(&record.values.keys().copied().collect::<Vec<_>>()),
                );
                let mut io = UsbRoutingIo {
                    deadline,
                    recovery: recovery.clone(),
                    device: &device,
                    descriptors: &record.snapshot.descriptors,
                };
                let mut entries: Vec<_> = record.values.iter().collect();
                // Selectors are independent addressable controls; mixer mappings must
                // be checked against the selector state at their own first edit.
                entries.sort_by_key(|(key, _)| matches!(key, ControlKey::Selector(_)));
                let mut controls = Vec::with_capacity(entries.len());
                let mut restored_selectors = std::collections::BTreeSet::new();
                let mut deferred = Vec::new();
                for (&key, saved) in entries {
                    let entity = match key {
                        ControlKey::Selector(e) | ControlKey::Crosspoint { entity: e, .. } => e,
                    };
                    let clusters_ok = cluster_result.as_ref().is_ok_and(|(clusters, failures)| {
                        !failures.iter().any(|failure| {
                            failure.interface == entity.interface
                                && failure.entity_id.map_or(true, |id| id == entity.id)
                        }) && cluster_dependencies(&record.snapshot, key).is_ok_and(|ids| {
                            cluster_content_matches(clusters, &record.snapshot.hc_clusters, &ids)
                        })
                    });
                    let mut outcome = if remaining(deadline).is_err() {
                        RoutingHandBackOutcome::DeadlineExpired
                    } else if matches!(key, ControlKey::Crosspoint { .. })
                        && (!clusters_ok
                            || verify_values(&record.snapshot.inventory, &mut io, &saved.selectors)
                                .is_err())
                    {
                        RoutingHandBackOutcome::IdentityOrTopologyChanged(
                            "selector or cluster mapping changed or unreadable".into(),
                        )
                    } else {
                        restore_recorded_control(&record.snapshot.inventory, &mut io, key, saved)
                    };
                    if remaining(deadline).is_err()
                        && matches!(
                            outcome,
                            RoutingHandBackOutcome::Unreadable(_)
                                | RoutingHandBackOutcome::RestoreUncertain(_)
                        )
                    {
                        outcome = RoutingHandBackOutcome::DeadlineExpired;
                    }
                    if matches!(key, ControlKey::Crosspoint { .. })
                        && matches!(
                            outcome,
                            RoutingHandBackOutcome::IdentityOrTopologyChanged(_)
                        )
                        && clusters_ok
                    {
                        deferred.push((controls.len(), key, saved));
                    }
                    if matches!(key, ControlKey::Selector(_))
                        && outcome == RoutingHandBackOutcome::Restored
                    {
                        restored_selectors.insert(key);
                    }
                    controls.push((key, outcome));
                }
                for (index, key, saved) in deferred {
                    if remaining(deadline).is_err() {
                        controls[index].1 = RoutingHandBackOutcome::DeadlineExpired;
                        continue;
                    }
                    if saved
                        .selectors
                        .iter()
                        .any(|dependency| restored_selectors.contains(&dependency.key))
                        && verify_values(&record.snapshot.inventory, &mut io, &saved.selectors)
                            .is_ok()
                    {
                        controls[index].1 = restore_recorded_control(
                            &record.snapshot.inventory,
                            &mut io,
                            key,
                            saved,
                        );
                    }
                }
                report = RoutingHandBackReport {
                    controls: controls.clone(),
                };
                Ok(RoutingHandBackReport { controls })
            },
        )
    };
    match restore() {
        Ok(result) => report = result,
        Err(error) if report.controls.is_empty() => fail_all(&mut report, error.to_string()),
        Err(error) => {
            for (_, outcome) in &mut report.controls {
                if *outcome == RoutingHandBackOutcome::Restored {
                    *outcome = RoutingHandBackOutcome::RestoreUncertain(error.to_string());
                }
            }
        }
    }
    *sink.lock() = Some(report);
}

fn restore_recorded_control<I: RoutingIo>(
    inventory: &RoutingInventory,
    io: &mut I,
    key: ControlKey,
    saved: &RecordedRoute,
) -> RoutingHandBackOutcome
where
    I::Error: std::fmt::Debug,
{
    let control = match inventory.control(key) {
        Ok(control) => control,
        Err(error) => {
            return RoutingHandBackOutcome::IdentityOrTopologyChanged(format!("{error:?}"))
        }
    };
    let current = match control.read_current(io) {
        Ok(value) => value,
        Err(error) => return RoutingHandBackOutcome::Unreadable(format!("{error:?}")),
    };
    if current == saved.original {
        return RoutingHandBackOutcome::AlreadyOriginal;
    }
    if saved.forfeited || !saved.ownership.owns(current) {
        return RoutingHandBackOutcome::ExternallyChanged;
    }
    match routing::write_verified(io, control, saved.original) {
        Ok(()) => RoutingHandBackOutcome::Restored,
        Err(error) => RoutingHandBackOutcome::RestoreUncertain(format!("{error:?}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::routing::Entity;

    #[test]
    fn shared_release_refuses_vendor_reads_before_usb_session_access() {
        const _: () = assert!(!VENDOR_ROUTING_RELEASED);
        assert!(matches!(
            require_vendor_routing_release(),
            Err(RoutingSessionError::UnsupportedTopology(_))
        ));
        assert!(scarlett_discover().unwrap().is_none());
        // A deliberately invalid fd proves Android/native preflight does not
        // wrap or inspect a device before refusing the unpublished capability.
        let info = crate::DeviceInfo::new(0x1235, 0x8211, "Scarlett");
        assert!(scarlett_preflight(-1, &info).unwrap().is_none());
        assert!(matches!(
            scarlett_read(),
            Err(RoutingSessionError::UnsupportedTopology(_))
        ));
        assert!(matches!(
            fcp_read(),
            Err(RoutingSessionError::UnsupportedTopology(_))
        ));
        assert!(matches!(
            scarlett_gen1_read(),
            Err(RoutingSessionError::UnsupportedTopology(_))
        ));
        assert!(matches!(
            scarlett_mux_read(),
            Err(RoutingSessionError::UnsupportedTopology(_))
        ));
    }

    #[test]
    fn routing_power_hand_back_preserves_transaction_and_rollback_failures() {
        let key = ControlKey::Selector(Entity {
            interface: 3,
            id: 8,
        });
        let operation = Err::<(), _>(RoutingSessionError::Transaction(TransactionError {
            failed_change: Some(2),
            cause: RoutingError::Io("write".into()),
            rollback_failures: vec![(key, RoutingError::Io("route restore".into()))],
        }));
        let error = finish_routing_power(operation, Err("power restore".into())).unwrap_err();
        let RoutingSessionError::PowerRestore {
            operation: Some(operation),
            detail,
        } = error
        else {
            panic!("expected both routing and power failures");
        };
        assert_eq!(detail, "power restore");
        let RoutingSessionError::Transaction(transaction) = *operation else {
            panic!("expected the original typed transaction");
        };
        assert_eq!(transaction.failed_change, Some(2));
        assert_eq!(transaction.cause, RoutingError::Io("write".into()));
        assert_eq!(
            transaction.rollback_failures,
            vec![(key, RoutingError::Io("route restore".into()))]
        );
    }

    #[test]
    fn routing_power_hand_back_keeps_single_failures_and_success_distinct() {
        assert_eq!(finish_routing_power(Ok(7), Ok(())).unwrap(), 7);
        assert!(matches!(finish_routing_power::<()>(
            Err(RoutingSessionError::Drift("changed".into())), Ok(())),
            Err(RoutingSessionError::Drift(detail)) if detail == "changed"));
        assert!(
            matches!(finish_routing_power(Ok(()), Err("power restore".into())),
            Err(RoutingSessionError::PowerRestore { operation: None, detail })
                if detail == "power restore")
        );
    }

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
    fn partial_inventory_keeps_independent_selector_when_mixer_width_is_unknown() {
        let mixer = vec![16, 0x24, 4, 9, 2, 99, 7, 2, 0, 0, 0, 0, 0, 0xff, 0, 0];
        let raw = configuration(
            &[terminal(1, 2, 3), terminal(2, 2, 3), selector(), mixer],
            0x20,
        );
        let (inventory, diagnostics) = inventory_partial(&raw, &[3], &BTreeMap::new()).unwrap();
        assert!(inventory
            .control(ControlKey::Selector(Entity {
                interface: 3,
                id: 7
            }))
            .is_ok());
        assert!(inventory
            .control(ControlKey::Crosspoint {
                entity: Entity {
                    interface: 3,
                    id: 9
                },
                input_pin: 2,
                input_channel: 1,
                output_channel: 1
            })
            .is_err());
        assert!(diagnostics
            .iter()
            .any(|d| d.interface == 3 && d.entity_id == Some(9)));
    }

    #[test]
    fn equal_width_selector_with_distinct_spatial_labels_keeps_mixer_indices() {
        let mixer = vec![16, 0x24, 4, 9, 2, 1, 7, 2, 0, 0, 0, 0, 0, 0xff, 0, 0];
        let raw = configuration(
            &[terminal(1, 2, 3), terminal(2, 2, 12), selector(), mixer],
            0x20,
        );
        let (inventory, diagnostics) = inventory_partial(&raw, &[3], &BTreeMap::new()).unwrap();
        assert!(diagnostics.is_empty());
        let point = inventory
            .control(ControlKey::Crosspoint {
                entity: Entity {
                    interface: 3,
                    id: 9,
                },
                input_pin: 2,
                input_channel: 1,
                output_channel: 1,
            })
            .unwrap();
        assert_eq!(point.current_request().unwrap().value, 0x0104);
    }

    #[test]
    fn deadline_never_returns_a_timeout_after_expiry() {
        assert!(remaining(Instant::now() - Duration::from_millis(1)).is_err());
        assert!(
            remaining(Instant::now() + Duration::from_secs(10)).unwrap() <= Duration::from_secs(1)
        );
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
    fn selector_cluster_retains_width_without_claiming_spatial_identity() {
        let different_width =
            configuration(&[terminal(1, 2, 3), terminal(2, 1, 1), selector()], 0x20);
        assert!(matches!(
            inventory_from_config(&different_width, &[3]),
            Err(RoutingSessionError::UnsupportedTopology(_))
        ));
        for other in [terminal(2, 2, 12), {
            let mut terminal = terminal(2, 2, 3);
            terminal[13] = 5;
            terminal
        }] {
            let raw = configuration(&[terminal(1, 2, 3), other, selector()], 0x20);
            assert!(inventory_from_config(&raw, &[3]).is_ok());
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
        // Equal widths preserve addressing despite distinct spatial labels.
        // A prior snapshot must still reject the changed HC content for both
        // apply and hand-back, which share cluster_content_matches.
        let saved = hc.clone();
        let dependencies = [(3, 1), (3, 2), (3, 3)];
        assert!(cluster_content_matches(&hc, &saved, &dependencies));
        hc.get_mut(&(3, 2)).unwrap().content[7] = 1;
        let changed = inventory_with_clusters(&raw, &[3], &hc).unwrap();
        assert_eq!(changed, inventory);
        assert!(!cluster_content_matches(&hc, &saved, &dependencies));
        assert!(cluster_content_matches(&hc, &saved, &[(3, 1), (3, 3)]));
        hc.remove(&(3, 2));
        assert!(!cluster_content_matches(&hc, &saved, &dependencies));
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
    fn ownership_discards_settled_history_and_preserves_ambiguous_pair() {
        let pin = RouteValue::SelectorPin;
        let mut entry = RecordedRoute {
            original: pin(1),
            ownership: Ownership::None,
            selectors: Vec::new(),
            forfeited: false,
        };
        entry.record_attempt(pin(1), pin(2), true, false);
        entry.record_attempt(pin(2), pin(3), true, false);
        assert!(!entry.ownership.owns(pin(2)));
        assert!(entry.ownership.owns(pin(3)));
        entry.record_attempt(pin(3), pin(4), false, true);
        assert!(entry.ownership.owns(pin(3)) && entry.ownership.owns(pin(4)));
        assert!(!entry.ownership.owns(pin(2)));
        entry.record_attempt(pin(3), pin(5), false, false);
        assert_eq!(entry.ownership, Ownership::Verified(pin(3)));
        entry.record_attempt(pin(9), pin(6), true, false);
        assert!(entry.forfeited);
        assert_eq!(entry.ownership, Ownership::None);
        assert_eq!(entry.original, pin(1));
    }

    #[test]
    fn foreign_live_selector_never_triggers_hand_back_write() {
        let raw = configuration(&[terminal(1, 2, 3), terminal(2, 2, 3), selector()], 0x20);
        let inventory = inventory_from_config(&raw, &[3]).unwrap();
        let key = ControlKey::Selector(Entity {
            interface: 3,
            id: 7,
        });
        let saved = RecordedRoute {
            original: RouteValue::SelectorPin(1),
            ownership: Ownership::Verified(RouteValue::SelectorPin(3)),
            selectors: Vec::new(),
            forfeited: false,
        };
        let mut io = ReadIo { pin: 2, reads: 0 };
        assert_eq!(
            restore_recorded_control(&inventory, &mut io, key, &saved),
            RoutingHandBackOutcome::ExternallyChanged
        );
        io.pin = 1;
        assert_eq!(
            restore_recorded_control(&inventory, &mut io, key, &saved),
            RoutingHandBackOutcome::AlreadyOriginal
        );
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

#[cfg(test)]
mod fcp_provenance_tests {
    use super::*;
    use crate::scarlett::{CandidateModel, FcpBank, FcpReadback, MixReadback};
    #[test]
    fn unrelated_external_values_never_become_hand_back_changes() {
        let original = FcpReadback {
            model: CandidateModel::Scarlett16i16Gen4,
            firmware: 1,
            mixer_inputs: 1,
            mixer_outputs: 2,
            sources: vec![],
            destinations: vec![],
            banks: vec![
                FcpBank {
                    bank: 0,
                    raw_words: vec![1],
                },
                FcpBank {
                    bank: 1,
                    raw_words: vec![2],
                },
            ],
            mixes: vec![
                MixReadback {
                    output: 1,
                    coefficients: vec![3],
                },
                MixReadback {
                    output: 2,
                    coefficients: vec![4],
                },
            ],
        };
        let mut last = original.clone();
        last.banks[0].raw_words[0] = 10;
        last.mixes[0].coefficients[0] = 30;
        let mut before = last.clone();
        before.banks[1].raw_words[0] = 20; // External change before a later unrelated edit.
        before.mixes[0].coefficients[0] = 31; // External change to an earlier owned target.
        let mut verified = before.clone();
        verified.mixes[1].coefficients[0] = 40;
        let (saved, recent) = merge_fcp_provenance(&original, &last, &before, &verified);
        assert_eq!(saved.banks[1], recent.banks[1]);
        assert_eq!(saved.banks[1].raw_words, vec![20]);
        assert_eq!(saved.banks[0], original.banks[0]);
        assert_eq!(recent.banks[0], last.banks[0]);
        assert_eq!(saved.mixes[0], original.mixes[0]);
        assert_eq!(recent.mixes[0], last.mixes[0]); // Fresh restore detects external 31 versus 30.
        assert_eq!(saved.mixes[1], original.mixes[1]);
        assert_eq!(recent.mixes[1], verified.mixes[1]);
    }
}

#[cfg(test)]
mod gen1_transport_tests {
    use super::*;
    use crate::scarlett::{Gen1Control, Gen1Key, Gen1Model, Gen1Readback};

    fn topology(iad: bool) -> Vec<u8> {
        let mut raw = vec![9, 2, 0, 0, 2, 1, 0, 0x80, 50];
        if iad {
            raw.extend([8, 11, 3, 2, 1, 0, 0x20, 0]);
        }
        raw.extend([9, 4, 3, 0, 0, 1, 1, 0x20, 0]);
        raw.extend([9, 0x24, 1, 0, 2, 0, 26, 0, 0]);
        raw.extend([17, 0x24, 2, 1, 1, 1, 0, 9, 2, 3, 0, 0, 0, 0, 0, 0, 0]);
        raw.extend([9, 4, 4, 1, 1, 1, 2, 0x20, 0]);
        raw.extend([16, 0x24, 1, 1, 0, 1, 1, 0, 0, 0, 2, 3, 0, 0, 0, 0]);
        raw.extend([7, 5, 1, 1, 64, 0, 1]);
        let length = (raw.len() as u16).to_le_bytes();
        raw[2..4].copy_from_slice(&length);
        raw
    }
    fn device() -> UsbDevice {
        UsbDevice {
            fd: -1,
            _fd_owner: Arc::new(OwnedUsbFd(-1)),
            claimed_interfaces: vec![3].into(),
            vendor_id: 0x1235,
            product_id: 0x8012,
            product_name: String::new(),
            device_name: None,
        }
    }
    #[test]
    fn gen1_binding_requires_claimed_unique_uac2_function_and_terminal_link() {
        for iad in [false, true] {
            let raw = topology(iad);
            assert_eq!(gen1_interface(&raw, &[3]).unwrap(), 3);
            assert!(gen1_interface(&raw, &[4]).is_err());
            let mut unlinked = raw.clone();
            let link = unlinked.len() - 20;
            unlinked[link] = 99;
            assert!(gen1_interface(&unlinked, &[3]).is_err());
            let mut wrong_protocol = raw.clone();
            let ac_protocol = if iad { 24 } else { 16 };
            wrong_protocol[ac_protocol] = 0;
            assert!(gen1_interface(&wrong_protocol, &[3]).is_err());
        }
        let mut conflicting = topology(true);
        conflicting[13] = 0xff; // IAD owns AC but declares the wrong function class.
        assert!(gen1_interface(&conflicting, &[3]).is_err());
        let mut wrong_direction = topology(false);
        let endpoint_address = wrong_direction.len() - 5;
        wrong_direction[endpoint_address] = 0x81;
        assert!(gen1_interface(&wrong_direction, &[3]).is_err());
        let mut zero_alt = topology(false);
        zero_alt[47] = 0;
        assert!(gen1_interface(&zero_alt, &[3]).is_err());
        let raw = topology(true);
        let target = device();
        let binding = Gen1Binding {
            device: target.clone(),
            configuration: 1,
            descriptors: raw.clone(),
        };
        assert!(check_gen1_binding(&binding, &target, 1, &raw).is_ok());
        assert!(check_gen1_binding(&binding, &device(), 1, &raw).is_err());
        assert!(check_gen1_binding(&binding, &target, 2, &raw).is_err());
        let mut changed = raw.clone();
        changed[8] = 51;
        assert!(check_gen1_binding(&binding, &target, 1, &changed).is_err());
    }
    #[test]
    fn gen1_independent_control_provenance_preserves_external_drift() {
        let original = Gen1Readback {
            model: Gen1Model::Scarlett6i6,
            source_offsets: [0; 5],
            matrix_sources: 0,
            output_sources: 0,
            controls: vec![
                Gen1Control {
                    key: Gen1Key::Output(1),
                    value: 1,
                },
                Gen1Control {
                    key: Gen1Key::Output(2),
                    value: 2,
                },
            ],
        };
        let mut last = original.clone();
        last.controls[0].value = 10;
        let mut before = last.clone();
        before.controls[0].value = 11;
        let mut verified = before.clone();
        verified.controls[1].value = 20;
        let (saved, recent) = merge_gen1_provenance(&original, &last, &before, &verified);
        assert_eq!(saved.controls, original.controls);
        assert_eq!(recent.controls[0], last.controls[0]);
        assert_eq!(recent.controls[1], verified.controls[1]);
    }
}
