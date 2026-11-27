//! UAC2/full-ADC3 in-stream status notifications, scoped to the owning AC endpoint.

use super::alternate_controls::advertised_controls;
use super::iso::InterruptTransferSlot;
use super::usbfs::{ClaimedHandle, UsbfsControlTransfer, USBDEVFS_CONTROL};
use super::{ClockChoice, Direction, Error, TransferType};
use crate::clock_resolution::{ClockHopKind, ClockResolution};
use libusb1_sys::constants::{LIBUSB_TRANSFER_COMPLETED, LIBUSB_TRANSFER_NO_DEVICE};
use std::collections::HashSet;
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

    pub(super) fn add_streaming_alt(&mut self, interface: u8, alternate: u8) {
        if !self.streaming.contains(&(interface, alternate)) {
            self.streaming.push((interface, alternate));
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct Address {
    interface: u8,
    entity: u8,
    selector: u8,
}

impl Address {
    fn notification(record: &[u8]) -> Option<Self> {
        // CUR, class-specific interface recipient, master/channel-independent control.
        if record.len() != 6 || record[0] != 0 || record[1] != CURRENT_ATTRIBUTE || record[2] != 0 {
            return None;
        }
        Some(Self {
            interface: record[4],
            entity: record[5],
            selector: record[3],
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ExpectedRead {
    address: Address,
    expected: Vec<u8>,
}

impl ExpectedRead {
    fn new(interface: u8, entity: u8, selector: u8, expected: &[u8]) -> Self {
        Self {
            address: Address {
                interface,
                entity,
                selector,
            },
            expected: expected.to_vec(),
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
    }
}

fn add_read(reads: &mut Vec<ExpectedRead>, read: ExpectedRead) -> Result<(), Error> {
    if let Some(previous) = reads
        .iter()
        .find(|previous| previous.address == read.address)
    {
        if previous.expected != read.expected {
            return Err(Error::Control(format!(
                "Contradictory audio status expectations for {:?}",
                read.address
            )));
        }
    } else {
        reads.push(read);
    }
    Ok(())
}

struct EndpointStatus {
    endpoint: u8,
    slot: InterruptTransferSlot,
    reads: Vec<ExpectedRead>,
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
            .filter(|target| matches!(target.protocol, 0x20 | 0x30))
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
            let mut reads = Vec::new();
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
                    if controls.active {
                        add_read(&mut reads, ExpectedRead::new(interface, 0, 1, &[alternate]))?;
                    }
                }
            }
            if reads.is_empty() {
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
            let slot = InterruptTransferSlot::new(usize::from(endpoint.max_packet_size()))
                .map_err(Error::Transfer)?;
            endpoints.push(EndpointStatus {
                endpoint: endpoint.address(),
                slot,
                reads,
            });
        }
        // Resolve contradictory targets before arming any endpoint.
        for endpoint in &mut endpoints {
            endpoint.slot.submit(&claimed.handle, endpoint.endpoint)?;
        }
        Ok(Self { endpoints })
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
            let pending = triggered_reads(&endpoint.reads, bytes);
            for index in pending {
                read_expected(claimed, &endpoint.reads[index], deadline)?;
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
    let remaining = deadline
        .checked_duration_since(Instant::now())
        .filter(|remaining| !remaining.is_zero())
        .ok_or_else(verification_expired)?;
    // usbfs takes integer milliseconds: a positive sub-millisecond remainder
    // must round up instead of becoming an unbounded zero-timeout request.
    let timeout_ms = ((remaining.as_nanos() + 999_999) / 1_000_000) as u32;
    let mut response = vec![0; read.expected.len()];
    let request = UsbfsControlTransfer {
        request_type: 0xa1,
        request: CURRENT_ATTRIBUTE,
        value: u16::from(read.address.selector) << 8,
        index: u16::from(read.address.interface) | (u16::from(read.address.entity) << 8),
        length: response.len() as u16,
        timeout_ms,
        data: response.as_mut_ptr().cast(),
    };
    // Use the owning usbfs fd: synchronous control I/O must not become a second
    // consumer of libusb's isochronous completion queue.
    let result = unsafe { libc::ioctl(claimed.device_fd, USBDEVFS_CONTROL, &request) };
    if result < 0 {
        let error = std::io::Error::last_os_error();
        if matches!(
            error.raw_os_error(),
            Some(libc::ENODEV) | Some(libc::ESHUTDOWN)
        ) {
            return Err(Error::DeviceLost);
        }
        return Err(Error::Control(format!(
            "Audio status read {:?}: {error}",
            read.address
        )));
    }
    if Instant::now() >= deadline {
        return Err(verification_expired());
    }
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

fn verification_expired() -> Error {
    Error::Control("Audio status verification exceeded its 50 ms poll budget".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock_resolution::{ClockHop, ClockHopKind};

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
        assert_eq!(target.streaming, vec![(3, 2), (4, 1)]);
    }
}
