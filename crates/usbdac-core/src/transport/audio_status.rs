//! In-stream audio status notifications, scoped to the owning AC endpoint.

use super::alternate_controls::advertised_controls;
use super::iso::InterruptTransferSlot;
use super::uac1_status::{StreamWatch, Uac1Status};
use super::usbfs::{ClaimedHandle, UsbfsControlTransfer, USBDEVFS_CONTROL};
use super::{ClockChoice, Direction, Error, TransferType, UsageType};
use crate::clock_resolution::{ClockHopKind, ClockResolution};
use crate::descriptors::DescriptorIter;
use libusb1_sys::constants::{LIBUSB_TRANSFER_COMPLETED, LIBUSB_TRANSFER_NO_DEVICE};
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

// ADC3 Appendix A.22 (Table A-22): CUR = 0x01. The interrupt source
// attribute uses the same code as the class-specific Get Control request.
const CURRENT_ATTRIBUTE: u8 = 0x01;
const VERIFICATION_BUDGET: Duration = Duration::from_millis(50);

#[derive(Clone, Debug)]
pub(super) struct StreamStatusTarget {
    protocol: u8,
    control: u8,
    streaming: Vec<(u8, u8)>,
    reads: Vec<ExpectedRead>,
    data_endpoints: Vec<(u8, u8, u8)>,
    uac1_stream: Option<(Option<u32>, Option<u8>)>,
}

impl StreamStatusTarget {
    pub(super) fn new(
        protocol: u8,
        control: u8,
        interface: u8,
        alternate: u8,
        clock: Option<(&ClockChoice, u32)>,
    ) -> Self {
        let mut target = Self {
            protocol,
            control,
            streaming: vec![(interface, alternate)],
            reads: Vec::new(),
            data_endpoints: Vec::new(),
            uac1_stream: None,
        };
        if let Some((choice, rate)) = clock.filter(|(choice, _)| {
            choice.resolution == ClockResolution::Resolved && choice.control_interface == control
        }) {
            target.reads.push(ExpectedRead::new(
                control,
                choice.source_id,
                1,
                &rate.to_le_bytes(),
            ));
            target
                .reads
                .push(ExpectedRead::new(control, choice.source_id, 2, &[1]));
            for hop in &choice.path {
                match hop.kind {
                    ClockHopKind::Selector { selected_pin, .. } => {
                        target
                            .reads
                            .push(ExpectedRead::new(control, hop.id, 1, &[selected_pin]));
                    }
                    ClockHopKind::RateMultiplier {
                        numerator,
                        denominator,
                    } => {
                        target.reads.push(ExpectedRead::new(
                            control,
                            hop.id,
                            1,
                            &numerator.to_le_bytes(),
                        ));
                        target.reads.push(ExpectedRead::new(
                            control,
                            hop.id,
                            2,
                            &denominator.to_le_bytes(),
                        ));
                    }
                    _ => {}
                }
            }
        }
        target
    }

    pub(super) fn uac1(
        control: u8,
        interface: u8,
        alternate: u8,
        address: u8,
        expected_rate: Option<u32>,
        terminal: Option<u8>,
    ) -> Self {
        Self {
            protocol: 0,
            control,
            streaming: vec![(interface, alternate)],
            reads: Vec::new(),
            data_endpoints: vec![(interface, alternate, address)],
            uac1_stream: Some((expected_rate, terminal)),
        }
    }

    pub(super) fn add_streaming_alt(&mut self, interface: u8, alternate: u8) {
        if !self.streaming.contains(&(interface, alternate)) {
            self.streaming.push((interface, alternate));
        }
    }

    /// Register an advertised ADC3 active format already confirmed at activation.
    pub(super) fn add_adc3_active_format(&mut self, interface: u8, expected_mask: u64) {
        if self.protocol == 0x30 {
            // ADC3 5.2.2.1.3 and Table A-38: AS_AUDIO_DATA_FORMAT_CONTROL
            // selects the eight-byte active format bitmap.
            let mut read = ExpectedRead::new(interface, 0, 3, &expected_mask.to_le_bytes());
            read.recheck_on_alternate_change = true;
            self.reads.push(read);
        }
    }
    pub(super) fn add_data_endpoint(&mut self, interface: u8, alternate: u8, address: u8) {
        self.add_streaming_alt(interface, alternate);
        if !self
            .data_endpoints
            .contains(&(interface, alternate, address))
        {
            self.data_endpoints.push((interface, alternate, address));
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Recipient {
    Interface,
    Endpoint,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct Address {
    recipient: Recipient,
    interface: u8,
    entity: u8,
    selector: u8,
}

impl Address {
    fn request_type(self) -> u8 {
        match self.recipient {
            Recipient::Interface => 0xa1,
            Recipient::Endpoint => 0xa2,
        }
    }

    fn index(self) -> u16 {
        u16::from(self.interface) | (u16::from(self.entity) << 8)
    }

    fn notification(record: &[u8]) -> Option<Self> {
        // bInfo bit 1 identifies an endpoint recipient; all other bits must be clear.
        if record.len() != 6 || record[1] != CURRENT_ATTRIBUTE || record[2] != 0 {
            return None;
        }
        let (recipient, interface, entity) = match record[0] {
            0 => (Recipient::Interface, record[4], record[5]),
            2 if record[5] == 0 && valid_endpoint_address(record[4]) => {
                (Recipient::Endpoint, record[4], 0)
            }
            _ => return None,
        };
        Some(Self {
            recipient,
            interface,
            entity,
            selector: record[3],
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ExpectedRead {
    address: Address,
    expected: Vec<u8>,
    recheck_on_alternate_change: bool,
}

impl ExpectedRead {
    fn new(interface: u8, entity: u8, selector: u8, expected: &[u8]) -> Self {
        Self {
            address: Address {
                recipient: Recipient::Interface,
                interface,
                entity,
                selector,
            },
            expected: expected.to_vec(),
            recheck_on_alternate_change: false,
        }
    }

    fn triggered_by(&self, address: Address) -> bool {
        self.address == address
            || (self.address.entity == 0
                && self.address.selector == 1
                && address
                    == Address {
                        selector: 2,
                        ..self.address
                    })
            || (self.recheck_on_alternate_change
                && matches!(address.selector, 1 | 2)
                && address
                    == Address {
                        selector: address.selector,
                        ..self.address
                    })
    }
}

fn add_read(reads: &mut Vec<ExpectedRead>, read: ExpectedRead) -> Result<(), Error> {
    if let Some(previous) = reads
        .iter_mut()
        .find(|previous| previous.address == read.address)
    {
        if previous.expected != read.expected {
            return Err(Error::Control(format!(
                "Contradictory audio status expectations for {:?}",
                read.address
            )));
        }
        previous.recheck_on_alternate_change |= read.recheck_on_alternate_change;
    } else {
        reads.push(read);
    }
    Ok(())
}

#[derive(Clone, Debug)]
struct EndpointFault {
    address: Address,
    armed: bool,
}

struct EndpointStatus {
    endpoint: u8,
    slot: InterruptTransferSlot,
    reads: Vec<ExpectedRead>,
    faults: Vec<EndpointFault>,
    uac1: Option<Uac1Status>,
}

pub(super) struct AudioStatus {
    endpoints: Vec<EndpointStatus>,
}

impl AudioStatus {
    pub(super) fn is_active(&self) -> bool {
        !self.endpoints.is_empty()
    }

    pub(super) fn start(
        claimed: &ClaimedHandle,
        targets: &[StreamStatusTarget],
    ) -> Result<Self, Error> {
        let targets: Vec<_> = targets
            .iter()
            .filter(|target| matches!(target.protocol, 0 | 0x20 | 0x30))
            .collect();
        if targets.is_empty() {
            return Ok(Self {
                endpoints: Vec::new(),
            });
        }
        let config = claimed
            .handle
            .device()
            .active_config_descriptor()
            .map_err(descriptor_error)?;
        let mut endpoints = Vec::new();
        let mut owners = HashSet::new();
        let mut addresses = HashSet::new();
        let mut watched_data = HashSet::new();
        for target in &targets {
            if !owners.insert(target.control)
                || !claimed.claimed_interfaces.contains(&target.control)
            {
                continue;
            }
            let Some(owner) = config
                .interfaces()
                .find(|interface| interface.number() == target.control)
            else {
                continue;
            };
            let Some(ac) = owner.descriptors().find(|descriptor| {
                descriptor.setting_number() == 0
                    && descriptor.class_code() == 1
                    && descriptor.sub_class_code() == 1
                    && descriptor.protocol_code() == target.protocol
            }) else {
                continue;
            };
            let mut status_endpoints = ac.endpoint_descriptors().filter(|endpoint| {
                endpoint.direction() == Direction::In
                    && endpoint.transfer_type() == TransferType::Interrupt
            });
            let Some(endpoint) = status_endpoints.next() else {
                continue;
            };
            if target.protocol == 0 {
                // UAC1 notifications are two-byte status words, with their own
                // originator semantics and GET_STAT/GET_CUR request formats.
                // Optional or ambiguous descriptors leave this owner unwatched.
                if owner
                    .descriptors()
                    .filter(|d| d.setting_number() == 0)
                    .count()
                    != 1
                    || status_endpoints.next().is_some()
                    || !(2..=1024).contains(&endpoint.max_packet_size())
                    || endpoint.interval() == 0
                    || !valid_endpoint_address(endpoint.address())
                {
                    continue;
                }
                // Resolve aliases before arming any slot, including aliases
                // owned by an AC target encountered later in this loop.
                let aliases_control = config.interfaces().any(|interface| {
                    interface.number() != target.control
                        && interface.descriptors().any(|descriptor| {
                            descriptor.class_code() == 1
                                && descriptor.sub_class_code() == 1
                                && descriptor.setting_number() == 0
                                && descriptor.endpoint_descriptors().any(|ep| {
                                    ep.transfer_type() == TransferType::Interrupt
                                        && ep.direction() == Direction::In
                                        && ep.address() == endpoint.address()
                                })
                        })
                });
                let aliases_stream = targets.iter().any(|other| {
                    other.streaming.iter().any(|&(interface, alternate)| {
                        config.interfaces().any(|candidate| {
                            candidate.number() == interface
                                && candidate.descriptors().any(|descriptor| {
                                    descriptor.setting_number() == alternate
                                        && descriptor
                                            .endpoint_descriptors()
                                            .any(|ep| ep.address() == endpoint.address())
                                })
                        })
                    })
                });
                if aliases_control || aliases_stream {
                    continue;
                }
                let watches = (|| {
                    let mut watches = Vec::new();
                    let mut selected = HashSet::new();
                    for other in targets
                        .iter()
                        .filter(|other| other.control == target.control)
                    {
                        if other.protocol != 0 {
                            return None;
                        }
                        let (rate, terminal) = other.uac1_stream?;
                        for &(interface, alternate, address) in &other.data_endpoints {
                            if !claimed.claimed_interfaces.contains(&interface)
                                || !selected.insert((interface, alternate, address))
                            {
                                return None;
                            }
                            let streaming =
                                config.interfaces().find(|i| i.number() == interface)?;
                            let mut descriptors = streaming
                                .descriptors()
                                .filter(|d| d.setting_number() == alternate);
                            let descriptor = descriptors.next()?;
                            if descriptors.next().is_some()
                                || descriptor.class_code() != 1
                                || descriptor.sub_class_code() != 2
                                || descriptor.protocol_code() != 0
                            {
                                return None;
                            }
                            let mut inventory = Vec::new();
                            let mut data = None;
                            for ep in descriptor.endpoint_descriptors() {
                                if !valid_endpoint_address(ep.address())
                                    || ep.transfer_type() != TransferType::Isochronous
                                    || inventory.contains(&ep.address())
                                {
                                    return None;
                                }
                                inventory.push(ep.address());
                                if ep.address() == address {
                                    if !matches!(
                                        ep.usage_type(),
                                        UsageType::Data | UsageType::FeedbackData
                                    ) {
                                        return None;
                                    }
                                    data = Some(ep);
                                }
                            }
                            let data = data?;
                            watches.push(StreamWatch {
                                interface,
                                alternate,
                                endpoint: address,
                                rate: rate.filter(|_| {
                                    uac1_sampling_frequency(data.extra().unwrap_or(&[]))
                                }),
                                terminal,
                                endpoints: inventory,
                            });
                        }
                    }
                    Some(watches)
                })();
                let Some(status) = watches
                    .and_then(|watches| Uac1Status::new(target.control, ac.extra(), watches))
                else {
                    continue;
                };
                if addresses.contains(&endpoint.address()) {
                    continue;
                }
                addresses.insert(endpoint.address());
                let slot = InterruptTransferSlot::new(usize::from(endpoint.max_packet_size()))
                    .map_err(Error::Transfer)?;
                endpoints.push(EndpointStatus {
                    endpoint: endpoint.address(),
                    slot,
                    reads: Vec::new(),
                    faults: Vec::new(),
                    uac1: Some(status),
                });
                continue;
            }
            let mut reads = Vec::new();
            let mut faults = Vec::new();
            let mut fault_advertisements = HashMap::new();
            for target in targets
                .iter()
                .filter(|other| other.control == target.control)
            {
                if target.protocol != ac.protocol_code() {
                    return Err(Error::Control(
                        "Conflicting AudioControl status protocols".into(),
                    ));
                }
                for read in &target.reads {
                    add_read(&mut reads, read.clone())?;
                }
                for &(interface, alternate) in &target.streaming {
                    let streaming = config
                        .interfaces()
                        .find(|candidate| candidate.number() == interface)
                        .ok_or_else(|| {
                            Error::Control("Status target streaming interface disappeared".into())
                        })?;
                    let descriptor = streaming
                        .descriptors()
                        .find(|descriptor| descriptor.setting_number() == alternate)
                        .ok_or_else(|| {
                            Error::Control("Status target streaming alternate disappeared".into())
                        })?;
                    if descriptor.class_code() != 1
                        || descriptor.sub_class_code() != 2
                        || descriptor.protocol_code() != target.protocol
                        || !claimed.claimed_interfaces.contains(&interface)
                    {
                        return Err(Error::Control(
                            "Invalid audio status streaming target".into(),
                        ));
                    }
                    let controls = advertised_controls(target.protocol, descriptor.extra())
                        .map_err(Error::Control)?;
                    for &(_, _, address) in target
                        .data_endpoints
                        .iter()
                        .filter(|&&(i, a, _)| i == interface && a == alternate)
                    {
                        if !valid_endpoint_address(address) {
                            return Err(Error::Control(
                                "Invalid audio status data endpoint address".into(),
                            ));
                        }
                        let mut matches = descriptor
                            .endpoint_descriptors()
                            .filter(|ep| ep.address() == address);
                        let data = matches.next().ok_or_else(|| {
                            Error::Control("Audio status data endpoint disappeared".into())
                        })?;
                        if matches.next().is_some()
                            || data.transfer_type() != TransferType::Isochronous
                            || !matches!(
                                data.usage_type(),
                                UsageType::Data | UsageType::FeedbackData
                            )
                        {
                            return Err(Error::Control(
                                "Invalid audio status data endpoint target".into(),
                            ));
                        }
                        let selectors =
                            endpoint_fault_controls(target.protocol, data.extra().unwrap_or(&[]))
                                .map_err(Error::Control)?;
                        if let Some(previous) =
                            fault_advertisements.insert(address, selectors.clone())
                        {
                            if previous != selectors {
                                return Err(Error::Control(
                                    "Contradictory audio endpoint fault advertisements".into(),
                                ));
                            }
                        }
                        for selector in selectors {
                            let address = Address {
                                recipient: Recipient::Endpoint,
                                interface: address,
                                entity: 0,
                                selector,
                            };
                            if !faults
                                .iter()
                                .any(|fault: &EndpointFault| fault.address == address)
                            {
                                faults.push(EndpointFault {
                                    address,
                                    armed: false,
                                });
                            }
                        }
                    }
                    if controls.active {
                        add_read(&mut reads, ExpectedRead::new(interface, 0, 1, &[alternate]))?;
                    }
                }
            }
            if reads.is_empty() && faults.is_empty() {
                continue;
            }
            if status_endpoints.next().is_some() || endpoint.max_packet_size() < 6 {
                return Err(Error::Control(format!(
                    "Invalid AudioControl status endpoint on interface {}",
                    target.control
                )));
            }
            if !addresses.insert(endpoint.address()) {
                return Err(Error::Control(format!(
                    "AudioControl status endpoint {:#04x} is shared by multiple watched interfaces",
                    endpoint.address()
                )));
            }
            for &address in fault_advertisements.keys() {
                if !watched_data.insert(address) {
                    return Err(Error::Control(
                        "Audio status data endpoint has multiple AudioControl owners".into(),
                    ));
                }
            }
            let slot = InterruptTransferSlot::new(usize::from(endpoint.max_packet_size()))
                .map_err(Error::Transfer)?;
            endpoints.push(EndpointStatus {
                endpoint: endpoint.address(),
                slot,
                reads,
                faults,
                uac1: None,
            });
        }
        // Resolve contradictory targets before arming any endpoint.
        for endpoint in &mut endpoints {
            endpoint.slot.submit(&claimed.handle, endpoint.endpoint)?;
        }
        Ok(Self { endpoints })
    }

    /// Clear pre-stream sticky state immediately before this endpoint's first data submission.
    pub(super) fn arm_endpoint(
        &mut self,
        claimed: &ClaimedHandle,
        address: u8,
    ) -> Result<(), Error> {
        let deadline = Instant::now() + VERIFICATION_BUDGET;
        for endpoint in &mut self.endpoints {
            for fault in &mut endpoint.faults {
                if fault.address.interface == address && !fault.armed {
                    // Reading clears the sticky flag. A valid old true value is intentionally ignored.
                    read_fault(claimed, fault.address, deadline)?;
                    fault.armed = true;
                }
            }
        }
        Ok(())
    }

    pub(super) fn poll(&mut self, claimed: &ClaimedHandle) -> Result<(), Error> {
        let deadline = Instant::now() + VERIFICATION_BUDGET;
        for endpoint in &mut self.endpoints {
            let Some((status, bytes)) = endpoint.slot.take_completion() else {
                continue;
            };
            if status == LIBUSB_TRANSFER_NO_DEVICE {
                return Err(Error::DeviceLost);
            }
            if status != LIBUSB_TRANSFER_COMPLETED {
                // A pending NAK never completes (the transfer has no timeout).
                // Any completed transport error makes the status proof unavailable.
                return Err(Error::Transfer(format!(
                    "AudioControl status endpoint {:#04x} failed: {status}",
                    endpoint.endpoint
                )));
            }
            if let Some(status) = &endpoint.uac1 {
                status.poll(bytes, |request_type, request, value, index, length| {
                    raw_read_control(
                        claimed,
                        request_type,
                        request,
                        value,
                        index,
                        length,
                        deadline,
                    )
                    .map(|(_, response)| response)
                })?;
                endpoint.slot.submit(&claimed.handle, endpoint.endpoint)?;
                continue;
            }
            let pending = triggered_reads(&endpoint.reads, bytes);
            for index in pending {
                read_expected(claimed, &endpoint.reads[index], deadline)?;
            }
            for index in triggered_faults(&endpoint.faults, bytes) {
                let fault = &endpoint.faults[index];
                if read_fault(claimed, fault.address, deadline)? {
                    let name = if fault.address.selector == 2 {
                        "overrun"
                    } else {
                        "underrun"
                    };
                    return Err(Error::Transfer(format!(
                        "Confirmed audio endpoint {:#04x} {name}",
                        fault.address.interface
                    )));
                }
            }
            endpoint.slot.submit(&claimed.handle, endpoint.endpoint)?;
        }
        Ok(())
    }
}

fn triggered_reads(reads: &[ExpectedRead], packet: &[u8]) -> Vec<usize> {
    if packet.len() % 6 != 0 {
        return Vec::new();
    }
    let addresses: HashSet<_> = packet
        .chunks_exact(6)
        .filter_map(Address::notification)
        .collect();
    reads
        .iter()
        .enumerate()
        .filter_map(|(index, read)| {
            addresses
                .iter()
                .any(|address| read.triggered_by(*address))
                .then_some(index)
        })
        .collect()
}

fn descriptor_error(error: rusb::Error) -> Error {
    if error == rusb::Error::NoDevice {
        Error::DeviceLost
    } else {
        Error::Control(format!("Audio status descriptors: {error}"))
    }
}

fn read_expected(
    claimed: &ClaimedHandle,
    read: &ExpectedRead,
    deadline: Instant,
) -> Result<(), Error> {
    let (result, response) = read_control(claimed, read.address, read.expected.len(), deadline)?;
    if result as usize != read.expected.len() || response[..read.expected.len()] != read.expected {
        return Err(Error::Control(format!(
            "Audio status changed or invalid readback for {:?}: expected {:?}, received {:?}",
            read.address,
            read.expected,
            &response[..(result as usize).min(response.len())]
        )));
    }
    Ok(())
}

fn read_control(
    claimed: &ClaimedHandle,
    address: Address,
    length: usize,
    deadline: Instant,
) -> Result<(usize, Vec<u8>), Error> {
    raw_read_control(
        claimed,
        address.request_type(),
        CURRENT_ATTRIBUTE,
        u16::from(address.selector) << 8,
        address.index(),
        length,
        deadline,
    )
}

fn raw_read_control(
    claimed: &ClaimedHandle,
    request_type: u8,
    request: u8,
    value: u16,
    index: u16,
    length: usize,
    deadline: Instant,
) -> Result<(usize, Vec<u8>), Error> {
    let remaining = deadline
        .checked_duration_since(Instant::now())
        .filter(|remaining| !remaining.is_zero())
        .ok_or_else(verification_expired)?;
    // usbfs takes integer milliseconds: a positive sub-millisecond remainder
    // must round up instead of becoming an unbounded zero-timeout request.
    let timeout_ms = ((remaining.as_nanos() + 999_999) / 1_000_000) as u32;
    let mut response = vec![0; length];
    let transfer = UsbfsControlTransfer {
        request_type,
        request,
        value,
        index,
        length: response.len() as u16,
        timeout_ms,
        data: response.as_mut_ptr().cast(),
    };
    // Use the owning usbfs fd: synchronous control I/O must not become a second
    // consumer of libusb's isochronous completion queue.
    let result = unsafe { libc::ioctl(claimed.device_fd, USBDEVFS_CONTROL, &transfer) };
    if result < 0 {
        let error = std::io::Error::last_os_error();
        if matches!(
            error.raw_os_error(),
            Some(libc::ENODEV) | Some(libc::ESHUTDOWN)
        ) {
            return Err(Error::DeviceLost);
        }
        return Err(Error::Control(format!(
            "Audio status read ({request_type:#04x}, {request:#04x}, {value:#06x}, {index:#06x}): {error}"
        )));
    }
    if Instant::now() >= deadline {
        return Err(verification_expired());
    }
    response.truncate((result as usize).min(response.len()));
    Ok((result as usize, response))
}

fn parse_fault(response: &[u8]) -> Result<bool, Error> {
    match response {
        [0] => Ok(false),
        [1] => Ok(true),
        _ => Err(Error::Control(
            "Audio endpoint fault CUR must be exactly one byte containing 0 or 1".into(),
        )),
    }
}

fn read_fault(claimed: &ClaimedHandle, address: Address, deadline: Instant) -> Result<bool, Error> {
    let (length, response) = read_control(claimed, address, 1, deadline)?;
    if length != 1 {
        return Err(Error::Control(
            "Invalid audio endpoint fault CUR length".into(),
        ));
    }
    parse_fault(&response)
}

// UAC1 EP_GENERAL is exactly seven bytes. Duplicate advertisements,
// including identical copies, do not establish a unique endpoint control.
fn uac1_sampling_frequency(extra: &[u8]) -> bool {
    let mut remaining = extra;
    let mut advertised = None;
    while !remaining.is_empty() {
        let length = usize::from(remaining[0]);
        if length < 2 || length > remaining.len() {
            return false;
        }
        let descriptor = &remaining[..length];
        if descriptor[1] == 0x25 && descriptor.get(2) == Some(&1) {
            if advertised.is_some() || length != 7 {
                return false;
            }
            if descriptor[3] & !0x83 != 0 || descriptor[4] > 2 {
                return false;
            }
            advertised = Some(descriptor[3] & 1 != 0);
        }
        remaining = &remaining[length..];
    }
    advertised == Some(true)
}

fn valid_endpoint_address(address: u8) -> bool {
    address & 0x70 == 0 && address & 0x0f != 0
}

fn triggered_faults(faults: &[EndpointFault], packet: &[u8]) -> Vec<usize> {
    if !faults.iter().any(|fault| fault.armed) {
        return Vec::new();
    }
    if packet.len() % 6 != 0 {
        return Vec::new();
    }
    let addresses: HashSet<_> = packet
        .chunks_exact(6)
        .filter_map(Address::notification)
        .collect();
    faults
        .iter()
        .enumerate()
        .filter_map(|(index, fault)| {
            (fault.armed && addresses.contains(&fault.address)).then_some(index)
        })
        .collect()
}

/// EP_GENERAL: fault selectors 2 and 3 occupy the second and third two-bit fields.
fn endpoint_fault_controls(protocol: u8, extra: &[u8]) -> Result<Vec<u8>, String> {
    let length = match protocol {
        0x20 => 8,
        0x30 => 10,
        _ => return Ok(Vec::new()),
    };
    let mut controls = None;
    for descriptor in DescriptorIter::new(extra) {
        if descriptor.len() != length || descriptor[1] != 0x25 || descriptor[2] != 1 {
            continue;
        }
        let bitmap = if protocol == 0x20 {
            u32::from(descriptor[4])
        } else {
            u32::from_le_bytes(descriptor[3..7].try_into().unwrap())
        };
        let mut selectors = Vec::new();
        for selector in [2, 3] {
            match (bitmap >> (2 * (selector - 1))) & 3 {
                0 => {}
                1 => selectors.push(selector as u8),
                _ => return Err("Audio endpoint fault control must be absent or read-only".into()),
            }
        }
        if let Some(previous) = &controls {
            if previous != &selectors {
                return Err("Contradictory standard EP_GENERAL fault advertisements".into());
            }
        }
        controls = Some(selectors);
    }
    Ok(controls.unwrap_or_default())
}

fn verification_expired() -> Error {
    Error::Control("Audio status verification exceeded its 50 ms poll budget".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock_resolution::{ClockHop, ClockHopKind};

    #[test]
    fn uac1_sampling_readback_needs_one_exact_advertisement() {
        let frequency = [7, 0x25, 1, 1, 0, 0, 0];
        assert!(uac1_sampling_frequency(&frequency));
        assert!(!uac1_sampling_frequency(&[]));
        assert!(!uac1_sampling_frequency(&[7, 0x25, 1, 0, 0, 0, 0]));
        assert!(!uac1_sampling_frequency(&[8, 0x25, 1, 1, 0, 0, 0, 0]));
        assert!(!uac1_sampling_frequency(&frequency.repeat(2)));
        let mut malformed = frequency.to_vec();
        malformed.push(0);
        assert!(!uac1_sampling_frequency(&malformed));
        assert!(!uac1_sampling_frequency(&frequency[..6]));
        assert!(!uac1_sampling_frequency(&[7, 0x25, 1, 5, 0, 0, 0]));
        assert!(!uac1_sampling_frequency(&[7, 0x25, 1, 1, 3, 0, 0]));
        assert!(uac1_sampling_frequency(&[7, 0x25, 1, 0x83, 2, 0, 0]));
        // UAC2 EP_GENERAL must never authorize a UAC1 sampling request.
        assert!(!uac1_sampling_frequency(&general_endpoint(0x20, 1)));
    }

    #[test]
    fn uac1_target_keeps_endpoint_rate_and_terminal_separate_from_clock_reads() {
        let target = StreamStatusTarget::uac1(2, 3, 1, 0x81, Some(48_000), Some(7));
        assert_eq!(target.protocol, 0);
        assert_eq!(target.control, 2);
        assert_eq!(target.streaming, vec![(3, 1)]);
        assert_eq!(target.data_endpoints, vec![(3, 1, 0x81)]);
        assert_eq!(target.uac1_stream, Some((Some(48_000), Some(7))));
        assert!(target.reads.is_empty());
    }

    fn general_endpoint(protocol: u8, bitmap: u32) -> Vec<u8> {
        let mut descriptor = vec![0; if protocol == 0x20 { 8 } else { 10 }];
        descriptor[0] = descriptor.len() as u8;
        descriptor[1] = 0x25;
        descriptor[2] = 1;
        if protocol == 0x20 {
            descriptor[4] = bitmap as u8;
        } else {
            descriptor[3..7].copy_from_slice(&bitmap.to_le_bytes());
        }
        descriptor
    }

    #[test]
    fn endpoint_fault_advertisements_are_exact_and_read_only() {
        for protocol in [0x20, 0x30] {
            for (bitmap, selectors) in [(0, vec![]), (4, vec![2]), (16, vec![3]), (20, vec![2, 3])]
            {
                assert_eq!(
                    endpoint_fault_controls(protocol, &general_endpoint(protocol, bitmap)).unwrap(),
                    selectors
                );
            }
            for bitmap in [8, 12, 32, 48] {
                assert!(
                    endpoint_fault_controls(protocol, &general_endpoint(protocol, bitmap)).is_err()
                );
            }
            assert!(endpoint_fault_controls(protocol, &[]).unwrap().is_empty());
            let mut private = general_endpoint(protocol, 20);
            private.push(0);
            private[0] += 1;
            assert!(endpoint_fault_controls(protocol, &private)
                .unwrap()
                .is_empty());
            let mut duplicate = general_endpoint(protocol, 20);
            duplicate.extend(general_endpoint(protocol, 20));
            assert_eq!(
                endpoint_fault_controls(protocol, &duplicate).unwrap(),
                vec![2, 3]
            );
            duplicate.extend(general_endpoint(protocol, 4));
            assert!(endpoint_fault_controls(protocol, &duplicate).is_err());
            let mut no_faults = general_endpoint(protocol, 0);
            no_faults.extend(general_endpoint(protocol, 0));
            assert!(endpoint_fault_controls(protocol, &no_faults)
                .unwrap()
                .is_empty());
        }
        // ADC3 places bmControls at offset 3, not UAC2's offset 4.
        let mut adc3 = general_endpoint(0x30, 20);
        adc3[4] = 0xff;
        assert_eq!(endpoint_fault_controls(0x30, &adc3).unwrap(), vec![2, 3]);
    }

    #[test]
    fn endpoint_notifications_are_scoped_armed_and_deduplicated() {
        for ep in [1, 0x81] {
            let address = Address {
                recipient: Recipient::Endpoint,
                interface: ep,
                entity: 0,
                selector: 2,
            };
            let mut faults = vec![EndpointFault {
                address,
                armed: false,
            }];
            let record = [2, 1, 0, 2, ep, 0];
            assert!(triggered_faults(&faults, &record).is_empty());
            faults[0].armed = true;
            assert_eq!(triggered_faults(&faults, &record.repeat(2)), vec![0]);
            assert_eq!(address.request_type(), 0xa2);
            assert_eq!(address.index(), u16::from(ep));
            for unrelated in [
                [0, 1, 0, 2, ep, 0],
                [3, 1, 0, 2, ep, 0],
                [2, 2, 0, 2, ep, 0],
                [2, 1, 1, 2, ep, 0],
                [2, 1, 0, 3, ep, 0],
                [2, 1, 0, 2, 0, ep],
                [2, 1, 0, 2, ep ^ 0x80, 0],
                [2, 1, 0, 2, 0, 0],
            ] {
                assert!(triggered_faults(&faults, &unrelated).is_empty());
            }
            let mut malformed = record.to_vec();
            malformed.push(0);
            assert!(triggered_faults(&faults, &malformed).is_empty());
        }
    }

    #[test]
    fn fault_cur_requires_one_boolean_byte() {
        assert!(!parse_fault(&[0]).unwrap());
        assert!(parse_fault(&[1]).unwrap());
        for response in [vec![], vec![2], vec![255], vec![0, 1]] {
            assert!(matches!(parse_fault(&response), Err(Error::Control(_))));
        }
    }

    #[test]
    fn wire_filter_and_burst_reads_are_scoped_and_deduplicated() {
        let reads = vec![
            ExpectedRead::new(3, 0, 1, &[2]),
            ExpectedRead::new(1, 9, 1, &[0, 119, 1, 0]),
        ];
        let mut packet = vec![0, 1, 0, 2, 3, 0, 0, 1, 0, 1, 3, 0, 0, 1, 0, 1, 1, 9];
        assert_eq!(triggered_reads(&reads, &packet), vec![0, 1]);
        for record in [
            [1, 1, 0, 1, 1, 9],
            [2, 1, 0, 1, 1, 9],
            [0, 2, 0, 1, 1, 9],
            [0, 1, 1, 1, 1, 9],
            [0, 1, 0, 1, 2, 9],
            [0, 1, 0, 3, 1, 9],
        ] {
            assert!(triggered_reads(&reads, &record).is_empty());
        }
        packet.push(0);
        assert!(triggered_reads(&reads, &packet).is_empty());
    }

    #[test]
    fn adc3_active_format_registration_is_protocol_scoped_and_exact() {
        for protocol in [0, 0x20, 0x30] {
            for mask in [1u64, 1 << 5] {
                let mut target = StreamStatusTarget::new(protocol, 1, 3, 2, None);
                target.add_adc3_active_format(3, mask);
                if protocol != 0x30 {
                    assert!(target.reads.is_empty());
                    continue;
                }
                assert_eq!(target.reads.len(), 1);
                let read = &target.reads[0];
                assert_eq!(read.address, ExpectedRead::new(3, 0, 3, &[]).address);
                assert_eq!(read.address.request_type(), 0xa1);
                assert_eq!(read.address.index(), 3);
                assert_eq!(read.expected, mask.to_le_bytes());
                // Format and alternate notifications retain the exact DSD/PCM
                // mask selected for this interface, including a PCM sync peer.
                for selector in [1, 2, 3] {
                    assert_eq!(triggered_reads(&target.reads, &[0, 1, 0, selector, 3, 0]), vec![0]);
                }
            }
        }
    }

    #[test]
    fn adc3_format_notifications_recheck_pcm_and_alternate_changes() {
        let mut target = StreamStatusTarget::new(0x30, 1, 3, 2, None);
        target.add_adc3_active_format(3, 1);
        target.reads.push(ExpectedRead::new(3, 0, 1, &[2]));
        for selector in [1, 2, 3] {
            let record = [0, 1, 0, selector, 3, 0];
            assert_eq!(
                triggered_reads(&target.reads, &record.repeat(2)),
                if selector == 3 { vec![0] } else { vec![0, 1] }
            );
        }
        // A format selector on an endpoint, other interface/entity, non-CUR
        // attribute, or malformed notification cannot invalidate this proof.
        for record in [
            [2, 1, 0, 3, 3, 0],
            [2, 1, 0, 1, 3, 0],
            [0, 1, 0, 3, 4, 0],
            [0, 1, 0, 3, 3, 1],
            [0, 2, 0, 3, 3, 0],
            [0, 1, 1, 3, 3, 0],
            [0, 1, 0, 4, 3, 0],
        ] {
            assert!(triggered_reads(&target.reads, &record).is_empty());
        }
        assert!(triggered_reads(&target.reads, &[0, 1, 0, 3, 3]).is_empty());

        // Generic selector 3 reads (including UAC2's distinct namespace)
        // never acquire ADC3 alternate-change invalidation implicitly.
        let generic = [ExpectedRead::new(3, 0, 3, &1u64.to_le_bytes())];
        for selector in [1, 2] {
            assert!(triggered_reads(&generic, &[0, 1, 0, selector, 3, 0]).is_empty());
        }
    }

    #[test]
    fn merged_adc3_format_proofs_retain_alternate_invalidation() {
        let mut target = StreamStatusTarget::new(0x30, 1, 3, 2, None);
        target.add_adc3_active_format(3, 1);
        let mut reads = vec![ExpectedRead::new(3, 0, 3, &1u64.to_le_bytes())];
        add_read(&mut reads, target.reads[0].clone()).unwrap();
        assert_eq!(reads.len(), 1);
        assert_eq!(triggered_reads(&reads, &[0, 1, 0, 1, 3, 0]), vec![0]);
        assert!(add_read(&mut reads, ExpectedRead::new(3, 0, 3, &2u64.to_le_bytes())).is_err());
    }

    #[test]
    fn only_resolved_clock_path_has_exact_clock_expectations() {
        let mut choice = ClockChoice {
            control_interface: 1,
            source_id: 9,
            resolution: ClockResolution::Resolved,
            path: vec![
                ClockHop {
                    id: 8,
                    kind: ClockHopKind::Selector {
                        selected_pin: 2,
                        pin_count: 3,
                    },
                },
                ClockHop {
                    id: 7,
                    kind: ClockHopKind::RateMultiplier {
                        numerator: 2,
                        denominator: 3,
                    },
                },
            ],
        };
        let target = StreamStatusTarget::new(0x30, 1, 3, 2, Some((&choice, 48_000)));
        assert_eq!(target.reads.len(), 5);
        assert_eq!(target.reads[0].expected, 48_000u32.to_le_bytes());
        choice.resolution = ClockResolution::FallbackFirstSource;
        assert!(
            StreamStatusTarget::new(0x30, 1, 3, 2, Some((&choice, 48_000)))
                .reads
                .is_empty()
        );
        choice.resolution = ClockResolution::Resolved;
        assert!(
            StreamStatusTarget::new(0x30, 2, 3, 2, Some((&choice, 48_000)))
                .reads
                .is_empty()
        );
    }

    #[test]
    fn identical_expectations_merge_but_contradictions_fail() {
        let mut reads = Vec::new();
        add_read(&mut reads, ExpectedRead::new(1, 9, 1, &[1])).unwrap();
        add_read(&mut reads, ExpectedRead::new(1, 9, 1, &[1])).unwrap();
        assert_eq!(reads.len(), 1);
        assert!(add_read(&mut reads, ExpectedRead::new(1, 9, 1, &[2])).is_err());
        let mut target = StreamStatusTarget::new(0x20, 1, 3, 2, None);
        target.add_streaming_alt(4, 1);
        target.add_streaming_alt(4, 1);
        target.add_data_endpoint(4, 1, 0x81);
        target.add_data_endpoint(4, 1, 0x81);
        assert_eq!(target.streaming, vec![(3, 2), (4, 1)]);
        assert_eq!(target.data_endpoints, vec![(4, 1, 0x81)]);
    }
}
