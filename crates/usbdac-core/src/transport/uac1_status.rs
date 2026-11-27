//! UAC1 interrupt status decoding and scoped active-route verification.

use crate::error::Error;
use std::collections::HashSet;

#[derive(Clone, Debug)]
pub(super) struct StreamWatch {
    pub(super) interface: u8,
    pub(super) alternate: u8,
    pub(super) endpoint: u8,
    pub(super) rate: Option<u32>,
    pub(super) terminal: Option<u8>,
    /// Every endpoint in the selected alternate, including synchronization pipes.
    pub(super) endpoints: Vec<u8>,
}

struct Collection {
    streams: HashSet<u8>,
    entities: HashSet<u8>,
    terminals: HashSet<u8>,
}

fn collection(extra: &[u8]) -> Option<Collection> {
    let mut header = None;
    let mut entities = HashSet::new();
    let mut terminals = HashSet::new();
    let mut offset = 0;
    let mut class_length = 0;
    while offset < extra.len() {
        let length = usize::from(*extra.get(offset)?);
        if length < 2 { return None; }
        let descriptor = extra.get(offset..offset.checked_add(length)?)?;
        offset += length;
        if descriptor[1] != 0x24 { continue; }
        class_length += length;
        match *descriptor.get(2)? {
            1 => {
                if header.is_some() || descriptor.len() < 8
                    || descriptor[3..5] != [0, 1]
                    || descriptor.len() != 8 + usize::from(descriptor[7])
                { return None; }
                let streams: HashSet<_> = descriptor[8..].iter().copied().collect();
                if streams.len() != descriptor[8..].len() { return None; }
                header = Some((streams, usize::from(u16::from_le_bytes([descriptor[5], descriptor[6]]))));
            }
            subtype @ 2..=8 => {
                let id = *descriptor.get(3)?;
                if id == 0 || !entities.insert(id) { return None; }
                if matches!(subtype, 2 | 3) { terminals.insert(id); }
            }
            _ => {}
        }
    }
    let (streams, total) = header?;
    if total != class_length { return None; }
    Some(Collection { streams, entities, terminals })
}

/// A streaming interface belongs to exactly one UAC1 AC alternate-zero header.
pub(super) fn collection_owner(config: &rusb::ConfigDescriptor, stream: u8) -> Option<u8> {
    let mut owner = None;
    for interface in config.interfaces() {
        for ac in interface.descriptors().filter(|ac| ac.setting_number() == 0
            && ac.class_code() == 1 && ac.sub_class_code() == 1 && ac.protocol_code() == 0)
        {
            if collection(ac.extra()).is_some_and(|collection| collection.streams.contains(&stream)) {
                if owner.is_some() { return None; }
                owner = Some(ac.interface_number());
            }
        }
    }
    owner
}

pub(super) struct Uac1Status {
    control: u8,
    entities: HashSet<u8>,
    streams: Vec<StreamWatch>,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Origin { Entity(u8), Interface(u8), Endpoint(u8) }

fn endpoint_address(address: u8) -> bool { address & 0x70 == 0 && address & 0x0f != 0 }

impl Uac1Status {
    pub(super) fn new(control: u8, ac_extra: &[u8], streams: Vec<StreamWatch>) -> Option<Self> {
        let collection = collection(ac_extra)?;
        let mut interfaces = HashSet::new();
        let mut endpoints = HashSet::new();
        if streams.is_empty() { return None; }
        for stream in &streams {
            if !collection.streams.contains(&stream.interface) || stream.interface == control
                || !interfaces.insert(stream.interface) || stream.alternate == 0
                || !stream.endpoints.contains(&stream.endpoint)
                || stream.rate.is_some_and(|rate| rate == 0 || rate > 0x7f_ffff)
                || stream.terminal.is_some_and(|terminal| !collection.terminals.contains(&terminal))
                || stream.endpoints.iter().any(|&endpoint| !endpoint_address(endpoint) || !endpoints.insert(endpoint))
            { return None; }
        }
        Some(Self { control, entities: collection.entities, streams })
    }

    fn origin(&self, status: u8, code: u8) -> Option<Origin> {
        // D7 is Interrupt Pending; D6 is the unsupported private-memory path.
        if status & 0xf0 != 0x80 { return None; }
        match status & 0x0f {
            0 if code == 0 || self.entities.contains(&code) => Some(Origin::Entity(code)),
            1 if self.streams.iter().any(|stream| stream.interface == code) => Some(Origin::Interface(code)),
            2 if endpoint_address(code) => {
                let mut addresses = self.streams.iter().flat_map(|stream| &stream.endpoints)
                    .copied().filter(|&address| if code & 0x80 != 0 { address == code } else { address & 0x0f == code });
                let address = addresses.next()?;
                if addresses.next().is_some() { return None; }
                Some(Origin::Endpoint(address))
            }
            _ => None,
        }
    }

    pub(super) fn poll(&self, packet: &[u8], mut read: impl FnMut(u8, u8, u16, u16, usize) -> Result<Vec<u8>, Error>) -> Result<(), Error> {
        // Reject the whole malformed packet before issuing any control request.
        if packet.len() % 2 != 0 { return Ok(()); }
        let mut seen = HashSet::new();
        let mut verify = HashSet::new();
        let mut notices = Vec::new();
        for record in packet.chunks_exact(2) {
            let Some(origin) = self.origin(record[0], record[1]) else { continue; };
            if !seen.insert(origin) { continue; }
            let affected: Vec<_> = self.streams.iter().enumerate().filter_map(|(index, stream)| {
                let active = match origin {
                    Origin::Entity(entity) => entity == 0 || stream.terminal == Some(entity),
                    Origin::Interface(interface) => stream.interface == interface,
                    Origin::Endpoint(endpoint) => stream.endpoints.contains(&endpoint),
                };
                active.then_some(index)
            }).collect();
            notices.push((origin, affected));
        }
        // Complete active proofs before optional unrelated acknowledgements can
        // consume the interrupt handler's bounded control-transfer budget.
        let mut optional = Vec::new();
        for (origin, affected) in notices {
            if affected.is_empty() { optional.push(origin); continue; }
            let (request_type, index) = match origin {
                Origin::Entity(entity) => (0xa1, u16::from(self.control) | u16::from(entity) << 8),
                Origin::Interface(interface) => (0xa1, u16::from(interface)),
                Origin::Endpoint(endpoint) => (0xa2, u16::from(endpoint)),
            };
            // UAC1 5.2.4.2 reserves the data and currently specifies a null reply.
            // Keep a data stage in the request; accept a short (including null) reply.
            let cleared = read(request_type, 0xff, 0, index, 2).and_then(|data| {
                if data.len() <= 2 { Ok(()) } else { Err(Error::Control("UAC1 GET_STAT returned oversized data".into())) }
            });
            match cleared {
                Err(Error::DeviceLost) => return Err(Error::DeviceLost),
                Err(error) if !affected.is_empty() => return Err(error),
                _ => {}
            }
            verify.extend(affected);
        }
        // Preserve stream ordering, independent of hash iteration order.
        for (index, stream) in self.streams.iter().enumerate() {
            if !verify.contains(&index) { continue; }
            if read(0x81, 0x0a, 0, u16::from(stream.interface), 1)? != [stream.alternate] {
                return Err(Error::Control("UAC1 active alternate changed".into()));
            }
            if let Some(expected) = stream.rate {
                let endpoint = u16::from(stream.endpoint);
                let result = read(0xa2, 0x81, 0x0100, endpoint, 3);
                let data = match result {
                    Err(Error::DeviceLost) => return Err(Error::DeviceLost),
                    Ok(data) if data.len() == 3
                        && (1..=0x7f_ffff).contains(&u32::from_le_bytes([data[0], data[1], data[2], 0])) => data,
                    _ if stream.interface != 0 => read(0xa2, 0x81, 0x0100, endpoint | u16::from(stream.interface) << 8, 3)?,
                    Err(error) => return Err(error),
                    Ok(data) => data,
                };
                if data.len() != 3 || u32::from_le_bytes([data[0], data[1], data[2], 0]) != expected {
                    return Err(Error::Control("UAC1 active sampling frequency changed or could not be verified".into()));
                }
            }
        }
        for origin in optional {
            let Origin::Entity(entity) = origin else { continue; };
            if let Err(Error::DeviceLost) = read(0xa1, 0xff, 0,
                u16::from(self.control) | u16::from(entity) << 8, 2)
            { return Err(Error::DeviceLost); }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn extra() -> Vec<u8> {
        // One AS interface, two terminals and an unrelated Feature Unit.
        vec![9, 0x24, 1, 0, 1, 21, 0, 1, 2,
             4, 0x24, 2, 5, 4, 0x24, 3, 6, 4, 0x24, 6, 7]
    }

    fn stream() -> StreamWatch {
        StreamWatch { interface: 2, alternate: 1, endpoint: 1,
            rate: Some(48_000), terminal: Some(5), endpoints: vec![1, 0x82] }
    }

    fn target() -> Uac1Status { Uac1Status::new(0, &extra(), vec![stream()]).unwrap() }

    #[test]
    fn malformed_memory_reserved_and_unknown_notices_issue_no_requests() {
        for packet in [vec![0x80, 5, 0], vec![0xc0, 5], vec![0x90, 5],
            vec![0, 5], vec![0x80, 99], vec![0x81, 3], vec![0x82, 0x83], vec![0x83, 5]] {
            target().poll(&packet, |_, _, _, _, _| panic!("unexpected read")).unwrap();
        }
    }

    #[test]
    fn null_status_and_reserved_payload_both_revalidate_once() {
        for payload in [vec![], vec![0xfe, 0xff]] {
            let mut calls = Vec::new();
            target().poll(&[0x80, 5, 0x80, 5, 0x81, 2], |kind, request, value, index, length| {
                calls.push((kind, request, value, index, length));
                Ok(match request { 0xff => payload.clone(), 0x0a => vec![1], 0x81 => vec![0x80, 0xbb, 0], _ => unreachable!() })
            }).unwrap();
            assert_eq!(calls, vec![(0xa1, 0xff, 0, 0x500, 2), (0xa1, 0xff, 0, 2, 2),
                (0x81, 0x0a, 0, 2, 1), (0xa2, 0x81, 0x100, 1, 3)]);
        }
    }

    #[test]
    fn timing_endpoint_number_maps_to_full_address_but_reads_audio_rate() {
        let mut calls = Vec::new();
        target().poll(&[0x82, 2, 0x82, 0x82], |kind, request, _, index, _| {
            calls.push((kind, request, index));
            Ok(match request { 0xff => vec![], 0x0a => vec![1], _ => vec![0x80, 0xbb, 0] })
        }).unwrap();
        assert_eq!(calls, vec![(0xa2, 0xff, 0x82), (0x81, 0x0a, 2), (0xa2, 0x81, 1)]);
    }

    #[test]
    fn ambiguous_endpoint_number_is_ignored_but_full_in_address_is_exact() {
        let mut watch = stream();
        watch.endpoints.push(0x81);
        let target = Uac1Status::new(0, &extra(), vec![watch]).unwrap();
        target.poll(&[0x82, 1], |_, _, _, _, _| panic!("ambiguous endpoint read")).unwrap();
        let mut calls = 0;
        target.poll(&[0x82, 0x81], |_, request, _, _, _| {
            calls += 1;
            Ok(match request { 0xff => vec![], 0x0a => vec![1], _ => vec![0x80, 0xbb, 0] })
        }).unwrap();
        assert_eq!(calls, 3);
    }

    #[test]
    fn unrelated_entity_is_optional_and_runs_after_active_proof() {
        let mut calls = Vec::new();
        target().poll(&[0x80, 7, 0x80, 0], |_, request, _, index, _| {
            calls.push((request, index));
            if index == 0x700 { return Err(Error::Control("unsupported".into())); }
            Ok(match request { 0xff => vec![], 0x0a => vec![1], _ => vec![0x80, 0xbb, 0] })
        }).unwrap();
        assert_eq!(calls, vec![(0xff, 0), (0x0a, 2), (0x81, 1), (0xff, 0x700)]);
        assert_eq!(target().poll(&[0x80, 7], |_, _, _, _, _| Err(Error::DeviceLost)), Err(Error::DeviceLost));
    }

    #[test]
    fn active_clear_alternate_and_rate_failures_invalidate() {
        assert!(target().poll(&[0x81, 2], |_, _, _, _, _| Err(Error::Control("stall".into()))).is_err());
        for alternate in [vec![], vec![0], vec![1, 1]] {
            assert!(target().poll(&[0x81, 2], |_, request, _, _, _| Ok(if request == 0xff { vec![] } else { alternate.clone() })).is_err());
        }
        assert!(target().poll(&[0x82, 1], |_, request, _, _, _| Ok(match request {
            0xff => vec![], 0x0a => vec![1], _ => vec![0x44, 0xac, 0]
        })).is_err());
    }

    #[test]
    fn rate_compatibility_fallback_never_hides_detach_or_a_valid_changed_rate() {
        let mut indexes = Vec::new();
        target().poll(&[0x81, 2], |_, request, _, index, _| {
            indexes.push(index);
            match request { 0xff => Ok(vec![]), 0x0a => Ok(vec![1]),
                _ if index == 1 => Err(Error::Control("stall".into())), _ => Ok(vec![0x80, 0xbb, 0]) }
        }).unwrap();
        assert_eq!(indexes, vec![2, 2, 1, 0x201]);
        for invalid in [vec![], vec![0, 0, 0], vec![0, 0, 0x80]] {
            target().poll(&[0x81, 2], |_, request, _, index, _| Ok(match request {
                0xff => vec![], 0x0a => vec![1], _ if index == 1 => invalid.clone(),
                _ => vec![0x80, 0xbb, 0],
            })).unwrap();
        }
        let mut rate_reads = 0;
        assert_eq!(target().poll(&[0x81, 2], |_, request, _, _, _| {
            match request { 0xff => Ok(vec![]), 0x0a => Ok(vec![1]), _ => { rate_reads += 1; Err(Error::DeviceLost) } }
        }), Err(Error::DeviceLost));
        assert_eq!(rate_reads, 1);
    }

    #[test]
    fn collection_and_watch_validation_fails_closed() {
        let mut bytes = extra();
        bytes[3] = 1;
        assert!(Uac1Status::new(0, &bytes, vec![stream()]).is_none());
        let mut watch = stream(); watch.interface = 3;
        assert!(Uac1Status::new(0, &extra(), vec![watch]).is_none());
        let mut watch = stream(); watch.terminal = Some(7);
        assert!(Uac1Status::new(0, &extra(), vec![watch]).is_none());
        let mut watch = stream(); watch.rate = Some(0x80_0000);
        assert!(Uac1Status::new(0, &extra(), vec![watch]).is_none());
        let mut watch = stream(); watch.endpoints.push(1);
        assert!(Uac1Status::new(0, &extra(), vec![watch]).is_none());
        assert!(Uac1Status::new(0, &extra(), vec![stream(), stream()]).is_none());
    }
}
