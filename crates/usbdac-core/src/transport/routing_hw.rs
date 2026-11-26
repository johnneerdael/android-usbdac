//! Session-bound standard UAC2 routing. No vendor commands or extra claims.

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
        if bytes.len() < 16 || bytes.len() > 16 + crate::scarlett::MAX_PAYLOAD {
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
        if bytes.len() < 16 || bytes.len() > 16 + crate::scarlett::MAX_PAYLOAD {
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
    let capability = scarlett_capability(&device, configuration)?.ok_or_else(|| {
        RoutingSessionError::UnsupportedTopology(
            "no exact Scarlett read candidate with a proven notification interface".into(),
        )
    })?;
    if !capability.interface_claimed {
        return Err(RoutingSessionError::Session(Error::Claim(format!(
            "Scarlett read requires the host's explicit claim of vendor interface {}",
            capability.interface
        ))));
    }
    let readback = {
        let idle = IDLE_LOCK.lock();
        let idle = idle
            .as_ref()
            .filter(|idle| idle.device_fd == device.fd)
            .ok_or_else(|| {
                RoutingSessionError::Session(Error::InvalidState(
                    "Scarlett read lost its idle USB owner".into(),
                ))
            })?;
        let io = UsbScarlettIo {
            device: &device,
            handle: &idle.handle,
            interface: capability.interface,
            endpoint: capability.notification_endpoint,
        };
        let mut session = crate::scarlett::ReadSession::initialize(io, capability.candidate)
            .map_err(RoutingSessionError::Scarlett)?;
        session.read_all().map_err(RoutingSessionError::Scarlett)?
    };
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
    let capability = scarlett_capability(device, configuration)?.ok_or_else(|| {
        RoutingSessionError::UnsupportedTopology("no exact Scarlett candidate".into())
    })?;
    if !capability.interface_claimed {
        return Err(RoutingSessionError::Session(Error::Claim(
            "Scarlett mixer requires explicit vendor-interface claim".into(),
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
    };
    let mut session = crate::scarlett::ReadSession::initialize(io, capability.candidate)
        .map_err(RoutingSessionError::Scarlett)?;
    operation(&mut session)
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
}

/// Enforce the small set of setup packets this adapter is allowed to issue.
fn validate_request(request: ControlRequest, input: bool, length: usize) -> Result<(), String> {
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

impl RoutingIo for UsbRoutingIo<'_> {
    type Error = String;
    fn read(&mut self, r: ControlRequest, bytes: &mut [u8]) -> Result<usize, String> {
        validate_request(r, true, bytes.len())?;
        self.device
            .feature_unit_control(r.request_type, r.request, r.value, r.index, bytes)
            .map_err(|e| e.to_string())
    }
    fn write(&mut self, r: ControlRequest, bytes: &[u8]) -> Result<usize, String> {
        validate_request(r, false, bytes.len())?;
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Cluster {
    channels: u8,
    locations: u32,
    names: u8,
}

fn explicit_cluster(bytes: &[u8], offset: usize) -> Result<Cluster, RoutingSessionError> {
    let part = bytes
        .get(offset..offset + 6)
        .ok_or(RoutingSessionError::Invalid(
            InvalidRoute::MalformedDescriptor,
        ))?;
    if part[0] == 0 {
        return Err(RoutingSessionError::Invalid(
            InvalidRoute::UnknownInputChannels,
        ));
    }
    Ok(Cluster {
        channels: part[0],
        locations: u32::from_le_bytes([part[1], part[2], part[3], part[4]]),
        names: part[5],
    })
}

fn resolve_cluster(
    id: u8,
    nodes: &BTreeMap<u8, &[u8]>,
    path: &mut Vec<u8>,
) -> Result<Cluster, RoutingSessionError> {
    resolve_cluster_cached(id, nodes, path, &mut BTreeMap::new())
}

fn resolve_cluster_cached(
    id: u8,
    nodes: &BTreeMap<u8, &[u8]>,
    path: &mut Vec<u8>,
    cache: &mut BTreeMap<u8, Cluster>,
) -> Result<Cluster, RoutingSessionError> {
    if path.contains(&id) || path.len() >= MAX_TOPOLOGY_DEPTH {
        return Err(RoutingSessionError::UnsupportedTopology(
            "cyclic or excessive source topology".into(),
        ));
    }
    // Repeated selector pins may share arbitrarily many upstream branches.
    // Cache completed nodes so a bounded descriptor cannot cause exponential
    // traversal; cycle detection still applies to the current recursion path.
    if let Some(cluster) = cache.get(&id) {
        return Ok(*cluster);
    }
    let bytes = *nodes.get(&id).ok_or_else(|| {
        RoutingSessionError::UnsupportedTopology(format!("unresolved source entity {id}"))
    })?;
    path.push(id);
    let result = (|| match bytes[2] {
        2 if bytes.len() == 17 => explicit_cluster(bytes, 8),
        4 => {
            let count = usize::from(*bytes.get(4).ok_or(RoutingSessionError::Invalid(
                InvalidRoute::MalformedDescriptor,
            ))?);
            let sources = bytes.get(5..5 + count).ok_or(RoutingSessionError::Invalid(
                InvalidRoute::MalformedDescriptor,
            ))?;
            let inputs = sources
                .iter()
                .map(|&source_id| {
                    Ok(InputCluster {
                        source_id,
                        channels: resolve_cluster_cached(source_id, nodes, path, cache)?.channels,
                    })
                })
                .collect::<Result<Vec<_>, RoutingSessionError>>()?;
            MixerUnit::parse(0, bytes, &inputs).map_err(RoutingSessionError::Invalid)?;
            explicit_cluster(bytes, 5 + count)
        }
        5 => {
            let unit = SelectorUnit::parse(0, bytes).map_err(RoutingSessionError::Invalid)?;
            let mut cluster = None;
            for &source in unit.sources() {
                let next = resolve_cluster_cached(source, nodes, path, cache)?;
                if cluster.is_some_and(|prior| prior != next) {
                    return Err(RoutingSessionError::UnsupportedTopology(
                        "selector input channel clusters differ".into(),
                    ));
                }
                cluster = Some(next);
            }
            cluster.ok_or(RoutingSessionError::Invalid(
                InvalidRoute::UnknownInputChannels,
            ))
        }
        6 if bytes.len() >= 10 => {
            let cluster = resolve_cluster_cached(bytes[4], nodes, path, cache)?;
            if bytes.len() != 6 + (usize::from(cluster.channels) + 1) * 4 {
                return Err(RoutingSessionError::Invalid(
                    InvalidRoute::MalformedDescriptor,
                ));
            }
            Ok(cluster)
        }
        _ => Err(RoutingSessionError::UnsupportedTopology(format!(
            "source entity {id} has unsupported channel topology"
        ))),
    })();
    path.pop();
    if let Ok(cluster) = &result {
        cache.insert(id, *cluster);
    }
    result
}

fn inventory_from_config(
    raw: &[u8],
    claims: &[u8],
) -> Result<RoutingInventory, RoutingSessionError> {
    if raw.len() < 9
        || raw[0] != 9
        || raw[1] != 2
        || usize::from(u16::from_le_bytes([raw[2], raw[3]])) != raw.len()
    {
        return Err(RoutingSessionError::Invalid(
            InvalidRoute::MalformedDescriptor,
        ));
    }
    let mut interfaces: BTreeMap<u8, Vec<&[u8]>> = BTreeMap::new();
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
                && bytes[7] == 0x20
                && claims.contains(&bytes[2])
            {
                if interfaces.insert(bytes[2], Vec::new()).is_some() {
                    return Err(RoutingSessionError::Invalid(InvalidRoute::DuplicateEntity));
                }
                current = Some(bytes[2]);
            }
        } else if bytes[1] == 0x24 {
            if let Some(interface) = current {
                interfaces.get_mut(&interface).unwrap().push(bytes);
            }
        }
    }
    let mut inventory = RoutingInventory::default();
    for (interface, descriptors) in interfaces {
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
        if header.len() != 9
            || header[3..5] != [0, 2]
            || usize::from(u16::from_le_bytes([header[6], header[7]]))
                != descriptors.iter().map(|d| d.len()).sum::<usize>()
        {
            return Err(RoutingSessionError::Invalid(
                InvalidRoute::MalformedDescriptor,
            ));
        }
        for &bytes in nodes.values() {
            let unit = match bytes[2] {
                4 => {
                    // Recursive validation also checks the upstream mixer/selector geometry.
                    resolve_cluster(bytes[3], &nodes, &mut Vec::new())?;
                    let count = usize::from(bytes[4]);
                    let inputs = bytes[5..5 + count]
                        .iter()
                        .map(|&source_id| {
                            Ok(InputCluster {
                                source_id,
                                channels: resolve_cluster(source_id, &nodes, &mut Vec::new())?
                                    .channels,
                            })
                        })
                        .collect::<Result<Vec<_>, RoutingSessionError>>()?;
                    RoutingUnit::Mixer(
                        MixerUnit::parse(interface, bytes, &inputs)
                            .map_err(RoutingSessionError::Invalid)?,
                    )
                }
                5 => {
                    resolve_cluster(bytes[3], &nodes, &mut Vec::new())?;
                    RoutingUnit::Selector(
                        SelectorUnit::parse(interface, bytes)
                            .map_err(RoutingSessionError::Invalid)?,
                    )
                }
                _ => continue,
            };
            inventory
                .insert(unit)
                .map_err(RoutingSessionError::Invalid)?;
        }
    }
    Ok(inventory)
}

fn verify_snapshot(
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
    let mut io = UsbRoutingIo { device };
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
    let _gate = USB_SESSION_GATE.lock();
    let _controls = HARDWARE_VOLUME_MUTEX.lock();
    let device = idle_device()?;
    let (configuration, descriptors) = active_descriptors(&device)?;
    let inventory = inventory_from_config(&descriptors, &device.claimed_interfaces)?;
    let mut selectors = Vec::new();
    let mut io = UsbRoutingIo { device: &device };
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
        device,
        configuration,
        descriptors,
        inventory,
        selectors,
    })
}

pub(crate) fn read(
    snapshot: &RoutingSnapshot,
    key: ControlKey,
) -> Result<RouteValue, RoutingSessionError> {
    let _gate = USB_SESSION_GATE.lock();
    let _controls = HARDWARE_VOLUME_MUTEX.lock();
    let device = idle_device()?;
    verify_snapshot(snapshot, &device)?;
    snapshot
        .inventory
        .control(key)
        .map_err(RoutingSessionError::Invalid)?
        .read_current(&mut UsbRoutingIo { device: &device })
        .map_err(RoutingSessionError::Control)
}

#[derive(Debug)]
struct RecordedRouting {
    snapshot: RoutingSnapshot,
    // Original and last library-written value for each edited control.
    values: BTreeMap<ControlKey, (RouteValue, RouteValue)>,
}
static ORIGINAL_ROUTING: Mutex<Option<RecordedRouting>> = Mutex::new(None);

pub(crate) fn apply(
    snapshot: &RoutingSnapshot,
    edits: &[RouteEdit],
) -> Result<RouteReport, RoutingSessionError> {
    let _gate = USB_SESSION_GATE.lock();
    let _controls = HARDWARE_VOLUME_MUTEX.lock();
    let device = idle_device()?;
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
    let mut io = UsbRoutingIo { device: &device };
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
        });
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
        let mut expected = record.snapshot.selectors.clone();
        for item in &mut expected {
            if let Some((_, last)) = record.values.get(&item.key) {
                item.value = *last;
            }
        }
        let mut io = UsbRoutingIo { device: &device };
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
    fn unclaimed_and_other_protocol_interfaces_never_expose_controls() {
        let entities = [terminal(1, 2, 3), terminal(2, 2, 3), selector()];
        assert_eq!(
            inventory_from_config(&configuration(&entities, 0x20), &[4])
                .unwrap()
                .units()
                .count(),
            0
        );
        for protocol in [0, 0x30, 0xff] {
            assert_eq!(
                inventory_from_config(&configuration(&entities, protocol), &[3])
                    .unwrap()
                    .units()
                    .count(),
                0
            );
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
