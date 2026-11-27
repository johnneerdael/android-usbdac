//! Standalone PCM IN ownership. Duplex and implicit-feedback fan-out must join
//! this pool rather than opening a second owner of the same endpoint.
use super::*;
use crate::capture::{CaptureBuffer, CaptureFormat, CapturePacket, CaptureRequest, ChannelMap};
use crate::error::OrControl;
use std::collections::VecDeque;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CaptureEndpoint {
    pub(crate) configuration: u8,
    pub(crate) interface: u8,
    pub(crate) alternate: u8,
    pub(crate) address: u8,
    pub(crate) service_us: u32,
    pub(crate) packet_bytes: usize,
    pub(crate) format: CaptureFormat,
    pub(super) protocol: u8,
    pub(super) terminal: u8,
    pub(super) control: u8,
    pub(super) control_descriptors: Vec<u8>,
    endpoint_descriptors: Vec<u8>,
    sampling_control: bool,
    fixed_rate: bool,
    pub(super) vendor_pcm: Option<vendor_pcm::VendorPcm>,
    pub(super) adc3_route: Option<adc3::Adc3Route>,
    pub(super) standard_feedforward: Option<adc3::FeedforwardEndpoint>,
}

impl CaptureEndpoint {
    /// Variable UAC1 capture without endpoint rate control can be admitted only
    /// through its standard adaptive host-timing pipe and a live PCM proof.
    pub(super) fn requires_host_rate_proof(&self) -> bool {
        self.protocol == 0
            && !self.sampling_control
            && !self.fixed_rate
            && self.standard_feedforward.is_some()
    }

    pub(super) fn feedforward(&self) -> Option<adc3::FeedforwardEndpoint> {
        self.standard_feedforward
            .or_else(|| self.adc3_route.as_ref().and_then(|route| route.feedforward))
    }
}

#[derive(Clone)]
struct RawInterface<'a> {
    number: u8,
    alternate: u8,
    class: u8,
    subclass: u8,
    protocol: u8,
    declared_endpoints: u8,
    descriptors: Vec<&'a [u8]>,
}

fn raw_interfaces(raw: &[u8]) -> Result<Vec<RawInterface<'_>>, String> {
    let mut interfaces: Vec<RawInterface<'_>> = Vec::new();
    let mut offset = 0usize;
    while offset < raw.len() {
        let length = raw.get(offset).copied().map(usize::from).unwrap_or(0);
        if length < 2 || offset + length > raw.len() {
            return Err("truncated capture configuration descriptor".into());
        }
        let d = &raw[offset..offset + length];
        if d[1] == 4 {
            if length < 9 {
                return Err("short interface descriptor".into());
            }
            interfaces.push(RawInterface {
                number: d[2],
                alternate: d[3],
                class: d[5],
                subclass: d[6],
                protocol: d[7],
                declared_endpoints: d[4],
                descriptors: Vec::new(),
            });
        } else if let Some(interface) = interfaces.last_mut() {
            interface.descriptors.push(d);
        }
        offset += length;
    }
    Ok(interfaces)
}

fn capture_control<'a, 'raw>(
    raw: &[u8],
    interfaces: &'a [RawInterface<'raw>],
    streaming: &RawInterface<'_>,
) -> Option<&'a RawInterface<'raw>> {
    let mut owners = interfaces.iter().filter(|ac| {
        if ac.class != USB_CLASS_AUDIO
            || ac.subclass != USB_SUBCLASS_AUDIOCONTROL
            || ac.alternate != 0
            || ac.protocol != streaming.protocol
        {
            return false;
        }
        if streaming.protocol == 0 {
            ac.descriptors.iter().any(|d| {
                d.len() >= 8
                    && d[1] == USB_DT_CS_INTERFACE
                    && d[2] == 1
                    && d.len() == 8 + usize::from(d[7])
                    && d[8..].contains(&streaming.number)
            })
        } else if streaming.protocol == 0x30 {
            let Ok(functions) = adc3::functions(raw) else {
                return false;
            };
            let owners: Vec<_> = functions
                .iter()
                .filter(|iad| {
                    u16::from(streaming.number) >= u16::from(iad.b_first_interface)
                        && u16::from(streaming.number)
                            < u16::from(iad.b_first_interface) + u16::from(iad.b_interface_count)
                })
                .collect();
            owners.len() == 1
                && owners[0].b_first_interface == ac.number
                && crate::descriptors::uac3::function_profile(owners[0], 0x30).is_ok()
        } else {
            audio_functions(raw)
                .iter()
                .any(|(control, ids)| *control == ac.number && ids.contains(&streaming.number))
        }
    });
    let owner = owners.next()?;
    if owners.next().is_some() {
        return None;
    }
    Some(owner)
}

// UAC1 capture links an Output Terminal, so follow source entities back to the
// terminal/mixer's explicit channel cluster. Never guess stereo from a count.
#[derive(Clone)]
enum ClusterCache {
    Unknown,
    Visiting,
    Failed,
    Resolved(ChannelMap, usize),
}

// One resolver belongs to one AC snapshot and one requested channel count.
// Cache subtree depth with successful maps so a cached result cannot bypass
// the independent recursion limit on a longer path.
struct Uac1ClusterResolver<'a> {
    index: [Option<&'a [u8]>; 256],
    duplicate: [bool; 256],
    cache: Vec<ClusterCache>,
    channels: u16,
    work: usize,
}

#[derive(Clone, Copy)]
enum ClusterFailure {
    Intrinsic,
    Cycle,
    Limit,
}

impl<'a> Uac1ClusterResolver<'a> {
    fn new(descriptors: &[&'a [u8]], channels: u16) -> Self {
        let mut index = [None; 256];
        let mut duplicate = [false; 256];
        let mut work = 0usize;
        for &d in descriptors {
            if d.len() >= 4 && d[1] == USB_DT_CS_INTERFACE && d[2] != 1 && d[3] != 0 {
                let id = usize::from(d[3]);
                duplicate[id] |= index[id].is_some();
                index[id] = Some(d);
                work = work.saturating_add(d.len());
            }
        }
        Self {
            index,
            duplicate,
            cache: vec![ClusterCache::Unknown; 256],
            channels,
            work: work.saturating_mul(32).max(1),
        }
    }

    fn resolve(&mut self, id: u8, depth: usize) -> Result<(ChannelMap, usize), ClusterFailure> {
        if depth >= 32 || self.work == 0 {
            return Err(ClusterFailure::Limit);
        }
        self.work -= 1;
        let key = usize::from(id);
        if id == 0 || self.duplicate[key] {
            return Err(ClusterFailure::Intrinsic);
        }
        match &self.cache[key] {
            ClusterCache::Visiting => return Err(ClusterFailure::Cycle),
            ClusterCache::Failed => return Err(ClusterFailure::Intrinsic),
            ClusterCache::Resolved(map, height) => {
                return if depth + height <= 32 {
                    Ok((map.clone(), *height))
                } else {
                    Err(ClusterFailure::Limit)
                }
            }
            ClusterCache::Unknown => {}
        }
        let Some(d) = self.index[key] else {
            return Err(ClusterFailure::Intrinsic);
        };
        self.cache[key] = ClusterCache::Visiting;
        let channels = self.channels;
        let cluster = |offset: usize| -> Result<(ChannelMap, usize), ClusterFailure> {
            if d.len() < offset + 4 || u16::from(d[offset]) != channels {
                return Err(ClusterFailure::Intrinsic);
            }
            ChannelMap::uac1(channels, u16::from_le_bytes([d[offset + 1], d[offset + 2]]))
                .map(|map| (map, 1))
                .map_err(|_| ClusterFailure::Intrinsic)
        };
        let result = match d[2] {
            2 if d.len() >= 12 => cluster(7),
            3 if d.len() >= 9 => self.resolve(d[7], depth + 1).map(|(m, h)| (m, h + 1)),
            6 if d.len() >= 7 => self.resolve(d[4], depth + 1).map(|(m, h)| (m, h + 1)),
            4 if d.len() >= 5 => cluster(5 + usize::from(d[4])),
            7 | 8 if d.len() >= 7 => cluster(7 + usize::from(d[6])),
            5 if d.len() >= 6 && d.len() == 6 + usize::from(d[4]) => {
                let mut result: Result<Option<(ChannelMap, usize)>, ClusterFailure> = Ok(None);
                for source in &d[5..5 + usize::from(d[4])] {
                    result = match (result, self.resolve(*source, depth + 1)) {
                        (Err(e), _) | (_, Err(e)) => Err(e),
                        (Ok(None), Ok((map, height))) => Ok(Some((map, height + 1))),
                        (Ok(Some((old, height))), Ok((map, child_height))) if old == map => {
                            Ok(Some((old, height.max(child_height + 1))))
                        }
                        _ => Err(ClusterFailure::Intrinsic),
                    };
                    if result.is_err() {
                        break;
                    }
                }
                result.and_then(|r| r.ok_or(ClusterFailure::Intrinsic))
            }
            _ => Err(ClusterFailure::Intrinsic),
        };
        self.cache[key] = match &result {
            Ok((map, height)) => ClusterCache::Resolved(map.clone(), *height),
            Err(ClusterFailure::Intrinsic) => ClusterCache::Failed,
            Err(ClusterFailure::Cycle | ClusterFailure::Limit) => ClusterCache::Unknown,
        };
        result
    }
}

/// Parse only standard linear Type I PCM; reject private/float/raw dialects.
fn capture_format(
    as_interface: &RawInterface<'_>,
    control: &RawInterface<'_>,
    rate: u32,
) -> Option<(CaptureFormat, u8, bool)> {
    let mut generals = as_interface
        .descriptors
        .iter()
        .copied()
        .filter(|d| d.len() >= 3 && d[1] == USB_DT_CS_INTERFACE && d[2] == 1);
    let general = generals.next()?;
    if generals.next().is_some() {
        return None;
    }
    let terminal = *general.get(3)?;
    let terminals: Vec<_> = control
        .descriptors
        .iter()
        .filter(|d| {
            d.len() >= 9
                && d[1] == USB_DT_CS_INTERFACE
                && d[2] == 3
                && d[3] == terminal
                && d[4..6] == [1, 1]
        })
        .collect();
    if terminal == 0 || terminals.len() != 1 {
        return None;
    }
    let mut formats = as_interface
        .descriptors
        .iter()
        .copied()
        .filter(|d| d.len() >= 4 && d[1] == USB_DT_CS_INTERFACE && d[2] == 2 && d[3] == 1);
    let pcm = formats.next()?;
    if formats.next().is_some() {
        return None;
    }
    if as_interface.protocol == 0 {
        if general.len() != 7 || general[5..7] != [1, 0] || pcm.len() < 8 {
            return None;
        }
        let channels = u16::from(pcm[4]);
        let rate_count = usize::from(pcm[7]);
        let read_rate =
            |offset| u32::from_le_bytes([pcm[offset], pcm[offset + 1], pcm[offset + 2], 0]);
        let fixed = if rate_count == 0 {
            if pcm.len() != 14 {
                return None;
            }
            let min = read_rate(8);
            let max = read_rate(11);
            if min == 0 || min > max || rate < min || rate > max {
                return None;
            }
            min == max
        } else {
            if pcm.len() != 8 + 3 * rate_count {
                return None;
            }
            if !(0..rate_count).any(|i| read_rate(8 + i * 3) == rate) {
                return None;
            }
            rate_count == 1
        };
        let map = Uac1ClusterResolver::new(&control.descriptors, channels)
            .resolve(general[3], 0)
            .ok()
            .map(|(map, _)| map)
            .or_else(|| ChannelMap::uac1(channels, 0).ok())?;
        Some((
            CaptureFormat::pcm(rate, pcm[6], pcm[5], map).ok()?,
            general[3],
            fixed,
        ))
    } else if as_interface.protocol == 0x20 {
        if general.len() != 16 || general[5] != 1 || pcm.len() != 6 {
            return None;
        }
        let formats = u32::from_le_bytes(general[6..10].try_into().ok()?);
        if formats & 1 == 0 {
            return None;
        }
        let map = ChannelMap::uac2(
            u16::from(general[10]),
            u32::from_le_bytes(general[11..15].try_into().ok()?),
        )
        .ok()?;
        Some((
            CaptureFormat::pcm(rate, pcm[5], pcm[4], map).ok()?,
            general[3],
            false,
        ))
    } else {
        None
    }
}

/// Keep endpoint extras scoped to this endpoint. In particular, a companion
/// belonging to another endpoint or alternate cannot qualify a capture pipe.
fn capture_input_geometry(
    interface: &RawInterface<'_>,
    index: usize,
    speed: Speed,
) -> Option<(usize, u32)> {
    let ep = *interface.descriptors.get(index)?;
    if ep.len() < 7
        || ep[1] != 5
        || ep[2] & 0x80 == 0
        || ep[2] & 0x0f == 0
        || ep[2] & 0x70 != 0
        || ep[3] & 3 != 1
        || !matches!((ep[3] >> 4) & 3, 0 | 2)
        || !(1..=16).contains(&ep[6])
    {
        return None;
    }
    iso_endpoint_geometry(interface, index, speed)
}

/// Shared packet geometry after the caller proves direction and endpoint role.
fn iso_endpoint_geometry(
    interface: &RawInterface<'_>,
    index: usize,
    speed: Speed,
) -> Option<(usize, u32)> {
    let ep = *interface.descriptors.get(index)?;
    if ep.len() < 7 || ep[1] != 5 || !(1..=16).contains(&ep[6]) {
        return None;
    }
    let packet = u16::from_le_bytes([ep[4], ep[5]]);
    if matches!(speed, Speed::Super | Speed::SuperPlus) {
        let extra: Vec<_> = interface.descriptors[index + 1..]
            .iter()
            .take_while(|d| d[1] != 5)
            .flat_map(|d| d.iter().copied())
            .collect();
        let (capacity, service, _) =
            super::endpoint_geometry::ss_endpoint_geometry(packet, ep[6], &extra, speed, true)
                .ok()?;
        return Some((capacity, service));
    }
    let base = packet & 0x7ff;
    if !matches!(speed, Speed::Full | Speed::High)
        || packet & 0xe000 != 0
        || base == 0
        || (speed == Speed::Full && (base > 1023 || packet & 0x1800 != 0))
        || (speed == Speed::High && (base > 1024 || (packet >> 11) & 3 == 3))
    {
        return None;
    }
    Some((
        effective_iso_packet_bytes(packet, speed),
        service_interval_micros(speed, ep[6]),
    ))
}

/// Adaptive UAC1/2 IN requires its same-alternate, explicit timing OUT pipe.
fn standard_capture_feedforward(
    interface: &RawInterface<'_>,
    index: usize,
    speed: Speed,
    disabled: bool,
) -> Result<Option<(adc3::FeedforwardEndpoint, usize)>, String> {
    let ep = interface.descriptors[index];
    if !matches!(interface.protocol, 0 | 0x20) || (ep[3] >> 2) & 3 != 2 {
        return Ok(None);
    }
    if disabled {
        return Err("adaptive capture requires enabled feedforward".into());
    }
    if interface.declared_endpoints != 2 {
        return Err("adaptive capture requires exactly two declared endpoints".into());
    }
    super::standard_feedforward::find_standard_capture_feedforward(
        interface.protocol,
        ep,
        &interface.descriptors,
        speed,
    )
    .map(Some)
}

fn scoped_endpoint_descriptors<'a>(
    interface: &'a RawInterface<'_>,
    index: usize,
) -> impl Iterator<Item = u8> + 'a {
    interface.descriptors[index..]
        .iter()
        .take(1)
        .chain(
            interface.descriptors[index + 1..]
                .iter()
                .take_while(|d| d[1] != 5),
        )
        .flat_map(|d| d.iter().copied())
}

/// Claim discovery is structural: prove SS geometry without assuming a rate.
/// ADC3 retains its own descriptor admission and activation proof.
fn claimable_capture_input(interface: &RawInterface<'_>, speed: Speed) -> bool {
    interface.descriptors.iter().enumerate().any(|(index, ep)| {
        if ep.len() < 7 || ep[1] != 5 {
            return false;
        }
        if standard_capture_feedforward(interface, index, speed, false).is_err() {
            return false;
        }
        if interface.protocol != 0x30 && matches!(speed, Speed::Super | Speed::SuperPlus) {
            return capture_input_geometry(interface, index, speed).is_some();
        }
        ep.len() >= 7
            && ep[1] == 5
            && ep[2] & 0x80 != 0
            && ep[2] & 0x0f != 0
            && ep[3] & 3 == 1
            && matches!((ep[3] >> 4) & 3, 0 | 2)
    })
}

pub(super) fn capture_candidates(
    raw: &[u8],
    configuration: u8,
    speed: Speed,
    rate: u32,
) -> Result<Vec<CaptureEndpoint>, String> {
    if !matches!(
        speed,
        Speed::Full | Speed::High | Speed::Super | Speed::SuperPlus
    ) {
        return Err("capture requires a recognized Full, High or SuperSpeed USB link".into());
    }
    let interfaces = raw_interfaces(raw)?;
    let mut candidates = Vec::new();
    for interface in &interfaces {
        if interface.class != USB_CLASS_AUDIO
            || interface.subclass != USB_SUBCLASS_AUDIOSTREAMING
            || interface.alternate == 0
            || !matches!(interface.protocol, 0 | 0x20)
        {
            continue;
        }
        let Some(control) = capture_control(raw, &interfaces, interface) else {
            continue;
        };
        let Some((format, terminal, fixed_rate)) = capture_format(interface, control, rate) else {
            continue;
        };
        for (index, ep) in interface.descriptors.iter().enumerate() {
            let Some((packet_bytes, service_us)) = capture_input_geometry(interface, index, speed)
            else {
                continue;
            };
            if packet_bytes < required_max_packet_bytes(rate, service_us, format.bytes_per_frame())
            {
                continue;
            }
            let Ok(timing) = standard_capture_feedforward(interface, index, speed, false) else {
                continue;
            };
            if timing.is_some_and(|(pipe, _)| pipe.nominal(rate).is_err()) {
                continue;
            }
            let mut endpoint_descriptors: Vec<_> =
                scoped_endpoint_descriptors(interface, index).collect();
            if let Some((_, timing_index)) = timing {
                endpoint_descriptors.extend(scoped_endpoint_descriptors(interface, timing_index));
            }
            let sampling_control = interface.descriptors[index + 1..]
                .iter()
                .take_while(|d| d[1] != 5)
                .find(|d| d.len() >= 7 && d[1] == 0x25 && d[2] == 1)
                .is_some_and(|d| d[3] & 1 != 0);
            let candidate = CaptureEndpoint {
                configuration,
                interface: interface.number,
                alternate: interface.alternate,
                address: ep[2],
                service_us,
                packet_bytes,
                format: format.clone(),
                protocol: interface.protocol,
                terminal,
                control: control.number,
                control_descriptors: control
                    .descriptors
                    .iter()
                    .filter(|d| d[1] == USB_DT_CS_INTERFACE)
                    .flat_map(|d| d.iter().copied())
                    .collect(),
                endpoint_descriptors,
                standard_feedforward: timing.map(|(pipe, _)| pipe),
                sampling_control,
                fixed_rate,
                adc3_route: None,
                vendor_pcm: None,
            };
            // Refuse unqualified variable UAC1 alternates before selection so
            // they cannot hide a later, explicitly host-timed alternate.
            if candidate.protocol == 0
                && !candidate.sampling_control
                && !candidate.fixed_rate
                && uac1_capture_sampling_control(&candidate).is_err()
            {
                continue;
            }
            candidates.push(candidate);
        }
    }
    Ok(candidates)
}

/// Rebuild the selected standard pipe from fresh wire descriptors, including
/// records whose malformed duplicates would otherwise disappear during parsing.
fn reprove_standard_capture(
    raw: &[u8],
    speed: Speed,
    candidate: &CaptureEndpoint,
) -> Result<(), Error> {
    if raw.len() < 9
        || raw[0] != 9
        || raw[1] != 2
        || usize::from(u16::from_le_bytes([raw[2], raw[3]])) != raw.len()
        || raw[5] != candidate.configuration
    {
        return Err(Error::InvalidState(
            "standard capture configuration header changed".into(),
        ));
    }
    let interfaces = raw_interfaces(raw).map_err(Error::Control)?;
    let mut selected = interfaces.iter().filter(|interface| {
        interface.number == candidate.interface && interface.alternate == candidate.alternate
    });
    let interface = selected
        .next()
        .ok_or_else(|| Error::InvalidState("standard capture alternate disappeared".into()))?;
    if selected.next().is_some()
        || interfaces
            .iter()
            .filter(|interface| interface.number == candidate.control && interface.alternate == 0)
            .count()
            != 1
        || interface
            .descriptors
            .iter()
            .filter(|d| d.len() >= 3 && d[1] == 5 && d[2] == candidate.address)
            .count()
            != 1
    {
        return Err(Error::InvalidState(
            "standard capture descriptors are ambiguous".into(),
        ));
    }
    let rebuilt = capture_candidates(
        raw,
        candidate.configuration,
        speed,
        candidate.format.sample_rate(),
    )
    .map_err(Error::Control)?;
    let mut matching = rebuilt.iter().filter(|endpoint| {
        endpoint.interface == candidate.interface
            && endpoint.alternate == candidate.alternate
            && endpoint.address == candidate.address
    });
    if matching.next() != Some(candidate) || matching.next().is_some() {
        return Err(Error::InvalidState(
            "standard capture descriptor profile changed".into(),
        ));
    }
    Ok(())
}

pub(super) fn verify_standard_capture(
    claimed: &ClaimedHandle,
    candidate: &CaptureEndpoint,
) -> Result<(), Error> {
    if candidate.vendor_pcm.is_some()
        || candidate.adc3_route.is_some()
        || !matches!(candidate.protocol, 0 | 0x20)
    {
        return Ok(());
    }
    if candidate.standard_feedforward.is_some() && settings().disable_feedback() {
        return Err(Error::Unsupported {
            reason: "adaptive capture requires enabled feedforward".into(),
            offered: vec![],
        });
    }
    claimed
        .ensure_interface_claimed(candidate.interface)
        .map_err(Error::Claim)?;
    claimed
        .ensure_interface_claimed(candidate.control)
        .map_err(Error::Claim)?;
    let map_control_error = |e: rusb::Error| {
        if e == rusb::Error::NoDevice {
            Error::DeviceLost
        } else {
            Error::Control(e.to_string())
        }
    };
    let active = claimed
        .handle
        .active_configuration()
        .map_err(map_control_error)?;
    let config = claimed
        .handle
        .device()
        .active_config_descriptor()
        .map_err(map_control_error)?;
    if active != candidate.configuration || config.number() != candidate.configuration {
        return Err(Error::InvalidState(
            "standard capture configuration changed".into(),
        ));
    }
    let raw = clock::read_active_configuration_bytes_checked(&claimed.handle, &config)?;
    if claimed
        .handle
        .active_configuration()
        .map_err(map_control_error)?
        != candidate.configuration
    {
        return Err(Error::InvalidState(
            "standard capture configuration changed during reproof".into(),
        ));
    }
    reprove_standard_capture(
        &raw,
        resolve_device_speed(&claimed.handle.device()),
        candidate,
    )
}

/// Host-timed and duplex activation need explicit endpoint control evidence. Discovery's bool
/// deliberately cannot distinguish a missing record from a bit-clear record.
pub(super) fn uac1_capture_sampling_control(capture: &CaptureEndpoint) -> Result<bool, Error> {
    let invalid = || Error::InvalidState("UAC1 capture sampling control is not proven".into());
    if capture.protocol != 0 || capture.vendor_pcm.is_some() || capture.adc3_route.is_some() {
        return Err(invalid());
    }
    let raw = &capture.endpoint_descriptors;
    let mut at = 0;
    let mut control = None;
    let mut max_packets_only = false;
    while at < raw.len() {
        let length = usize::from(raw[at]);
        if length < 2 || at + length > raw.len() {
            return Err(invalid());
        }
        let d = &raw[at..at + length];
        // The optional feedforward endpoint has its own control record.
        if at != 0 && d[1] == 5 {
            break;
        }
        if d[1] == 0x25 {
            if d.len() != 7 || d[2] != 1 || d[3] & !0x83 != 0 || d[4] > 2 || control.is_some() {
                return Err(invalid());
            }
            control = Some(d[3] & 1 != 0);
            max_packets_only = d[3] & 0x80 != 0;
        }
        at += length;
    }
    match control {
        Some(true) if capture.sampling_control => Ok(true),
        Some(false)
            if !capture.sampling_control
                && (capture.fixed_rate
                    || (capture.requires_host_rate_proof() && !max_packets_only)) =>
        {
            Ok(false)
        }
        _ => Err(invalid()),
    }
}

/// Return the unique UAC1 collection owning this exact selected AS alternate.
fn uac1_collection_owner<'a, 'raw>(
    interfaces: &'a [RawInterface<'raw>],
    streaming: &RawInterface<'_>,
) -> Result<&'a RawInterface<'raw>, Error> {
    let invalid = || {
        Error::InvalidState("UAC1 duplex AudioControl collection is ambiguous or malformed".into())
    };
    let mut owners = Vec::new();
    for ac in interfaces.iter().filter(|ac| {
        ac.class == USB_CLASS_AUDIO
            && ac.subclass == USB_SUBCLASS_AUDIOCONTROL
            && ac.protocol == 0
            && ac.alternate == 0
    }) {
        let headers: Vec<_> = ac
            .descriptors
            .iter()
            .copied()
            .filter(|d| d[1] == USB_DT_CS_INTERFACE && d.get(2) == Some(&1))
            .collect();
        // An unrelated malformed function cannot own this AS. A header that
        // actually names it must satisfy the complete collection contract.
        if !headers
            .iter()
            .any(|d| d.len() >= 8 && d[8..].contains(&streaming.number))
        {
            continue;
        }
        if headers.len() != 1 {
            return Err(invalid());
        }
        let header = headers[0];
        if header.len() < 8
            || header.len() != 8 + usize::from(header[7])
            || header[3..5] != [0, 1]
            || header[8..]
                .iter()
                .enumerate()
                .any(|(index, id)| header[8..8 + index].contains(id))
        {
            return Err(invalid());
        }
        if header[8..].contains(&streaming.number) {
            owners.push(ac);
        }
    }
    if owners.len() != 1
        || interfaces
            .iter()
            .filter(|interface| interface.number == owners[0].number && interface.alternate == 0)
            .count()
            != 1
    {
        return Err(invalid());
    }
    Ok(owners[0])
}

fn reprove_uac1_duplex_owners(
    raw: &[u8],
    speed: Speed,
    playback: &IsoStreamCandidate,
    capture: &CaptureEndpoint,
    playback_rate: u32,
) -> Result<(u8, bool, bool), Error> {
    let invalid =
        || Error::InvalidState("UAC1 duplex playback descriptor profile is not proven".into());
    if !playback.is_uac1
        || capture.protocol != 0
        || playback.configuration_number != capture.configuration
        || playback.usb_speed != speed
        || playback.alt_setting == 0
        || playback.interface_number == capture.interface
        || playback.native_dsd.is_some()
        || playback.uac2_formats.is_some()
        || playback.format_tag != 1
    {
        return Err(invalid());
    }
    reprove_standard_capture(raw, speed, capture)?;
    let interfaces = raw_interfaces(raw).map_err(Error::Control)?;
    let mut selected = interfaces.iter().filter(|interface| {
        interface.number == playback.interface_number && interface.alternate == playback.alt_setting
    });
    let output = selected.next().ok_or_else(invalid)?;
    if selected.next().is_some()
        || output.class != USB_CLASS_AUDIO
        || output.subclass != USB_SUBCLASS_AUDIOSTREAMING
        || output.protocol != 0
        || output.declared_endpoints as usize
            != output.descriptors.iter().filter(|d| d[1] == 5).count()
    {
        return Err(invalid());
    }
    let owner = uac1_collection_owner(&interfaces, output)?;
    let input = interfaces
        .iter()
        .find(|interface| {
            interface.number == capture.interface && interface.alternate == capture.alternate
        })
        .ok_or_else(invalid)?;
    if uac1_collection_owner(&interfaces, input)?.number != capture.control {
        return Err(invalid());
    }
    let generals: Vec<_> = output
        .descriptors
        .iter()
        .copied()
        .filter(|d| d[1] == USB_DT_CS_INTERFACE && d.get(2) == Some(&1))
        .collect();
    let formats: Vec<_> = output
        .descriptors
        .iter()
        .copied()
        .filter(|d| d[1] == USB_DT_CS_INTERFACE && d.get(2) == Some(&2))
        .collect();
    if generals.len() != 1 || generals[0].len() != 7 || formats.len() != 1 {
        return Err(invalid());
    }
    let general = generals[0];
    let format = formats[0];
    if playback.terminal_link != Some(general[3])
        || general[3] == 0
        || u16::from_le_bytes([general[5], general[6]]) != playback.format_tag
        || format.len() < 8
        || format[3] != 1
        || u16::from(format[4]) != playback.channels
        || format[5] != playback.subslot_size
        || format[6] != playback.bit_resolution
        || format.len()
            != if format[7] == 0 {
                14
            } else {
                8 + usize::from(format[7]) * 3
            }
    {
        return Err(invalid());
    }
    let rate = |at| u32::from_le_bytes([format[at], format[at + 1], format[at + 2], 0]);
    let requested = playback_rate;
    let valid_rate = |rate| (1..=0x7f_ffff).contains(&rate);
    if !valid_rate(requested) {
        return Err(invalid());
    }
    if format[7] == 0 {
        if !valid_rate(rate(8))
            || !valid_rate(rate(11))
            || rate(8) > rate(11)
            || requested < rate(8)
            || requested > rate(11)
        {
            return Err(invalid());
        }
    } else if (0..usize::from(format[7])).any(|index| !valid_rate(rate(8 + 3 * index)))
        || !(0..usize::from(format[7])).any(|index| rate(8 + 3 * index) == requested)
    {
        return Err(invalid());
    }
    let fixed_at_request = if format[7] == 0 {
        rate(8) == requested && rate(11) == requested
    } else {
        (0..usize::from(format[7])).all(|index| rate(8 + 3 * index) == requested)
    };
    let terminals: Vec<_> = owner
        .descriptors
        .iter()
        .copied()
        .filter(|d| d[1] == USB_DT_CS_INTERFACE && d.len() >= 4 && d[2] != 1 && d[3] == general[3])
        .collect();
    if terminals.len() != 1
        || terminals[0].len() != 12
        || terminals[0][2] != 2
        || terminals[0][4..6] != [1, 1]
        || u16::from(terminals[0][7]) != playback.channels
    {
        return Err(invalid());
    }
    let endpoints: Vec<_> = output
        .descriptors
        .iter()
        .enumerate()
        .filter(|(_, d)| d[1] == 5 && d.get(2) == Some(&playback.endpoint_address))
        .collect();
    if endpoints.len() != 1 {
        return Err(invalid());
    }
    let (index, ep) = endpoints[0];
    if ep.len() != 9
        || ep[2] & 0xf0 != 0
        || ep[2] & 15 == 0
        || ep[3] & 3 != 1
        || ep[6] != playback.endpoint_interval
        || ep[7] != playback.refresh
        || ep[8] != playback.synch_address
    {
        return Err(invalid());
    }
    let sync = match (ep[3] >> 2) & 3 {
        0 => SyncType::NoSync,
        1 => SyncType::Asynchronous,
        2 => SyncType::Adaptive,
        _ => SyncType::Synchronous,
    };
    let usage = match (ep[3] >> 4) & 3 {
        0 => UsageType::Data,
        2 => UsageType::FeedbackData,
        _ => return Err(invalid()),
    };
    if sync != playback.sync_type
        || usage != playback.usage_type
        || iso_endpoint_geometry(output, index, speed)
            != Some((playback.max_packet_bytes, playback.service_interval_us))
    {
        return Err(invalid());
    }
    let controls: Vec<_> = output.descriptors[index + 1..]
        .iter()
        .copied()
        .take_while(|d| d[1] != 5)
        .filter(|d| d[1] == 0x25)
        .collect();
    if controls.len() != 1
        || controls[0].len() != 7
        || controls[0][2] != 1
        || controls[0][3] & !0x83 != 0
        || controls[0][4] > 2
    {
        return Err(invalid());
    }
    if let Some(feedback) = playback.feedback_endpoint.as_ref() {
        let feedbacks: Vec<_> = output
            .descriptors
            .iter()
            .enumerate()
            .filter(|(_, d)| d[1] == 5 && d.get(2) == Some(&feedback.address))
            .collect();
        if feedbacks.len() != 1 {
            return Err(invalid());
        }
        let (feedback_index, feedback_ep) = feedbacks[0];
        if feedback_ep.len() != 9
            || feedback_ep[2] & 0xf0 != 0x80
            || feedback_ep[2] & 15 == 0
            || !matches!(feedback_ep[3], 0x01 | 0x11)
            || feedback_ep[6] != feedback.interval
            || feedback_ep[7] != feedback.refresh
            || feedback_ep[8] != 0
            || feedback.transfer_type != TransferType::Isochronous
            || (ep[8] != feedback.address && !(ep[8] == 0 && feedback_ep[3] == 0x11))
            || iso_endpoint_geometry(output, feedback_index, speed)
                != Some((feedback.max_packet_bytes, feedback.service_interval_us))
        {
            return Err(invalid());
        }
    }
    Ok((owner.number, controls[0][3] & 1 != 0, fixed_at_request))
}

/// Fresh descriptor proof only: this must never program either endpoint rate.
pub(super) fn uac1_duplex_owners(
    claimed: &ClaimedHandle,
    playback: &IsoStreamCandidate,
    capture: &CaptureEndpoint,
    playback_rate: u32,
) -> Result<u8, Error> {
    let map_error = |error: rusb::Error| {
        if error == rusb::Error::NoDevice {
            Error::DeviceLost
        } else {
            Error::Control(error.to_string())
        }
    };
    for interface in [
        playback.interface_number,
        capture.interface,
        capture.control,
    ] {
        claimed
            .ensure_interface_claimed(interface)
            .map_err(Error::Claim)?;
    }
    let config = claimed
        .handle
        .device()
        .active_config_descriptor()
        .map_err(map_error)?;
    if config.number() != capture.configuration
        || claimed.handle.active_configuration().map_err(map_error)? != capture.configuration
    {
        return Err(Error::InvalidState(
            "UAC1 duplex configuration changed".into(),
        ));
    }
    let raw = clock::read_active_configuration_bytes_checked(&claimed.handle, &config)?;
    let (owner, sampling_control, fixed_at_request) = reprove_uac1_duplex_owners(
        &raw,
        resolve_device_speed(&claimed.handle.device()),
        playback,
        capture,
        playback_rate,
    )?;
    let profile = clock::uac1_playback_rate_profile(claimed, playback, playback_rate)
        .map_err(Error::Control)?;
    if profile.sampling_control != Some(sampling_control)
        || profile.fixed_at_request != fixed_at_request
    {
        return Err(Error::InvalidState(
            "UAC1 duplex playback rate evidence changed".into(),
        ));
    }
    claimed
        .ensure_interface_claimed(owner)
        .map_err(Error::Claim)?;
    if claimed.handle.active_configuration().map_err(map_error)? != capture.configuration {
        return Err(Error::InvalidState(
            "UAC1 duplex configuration changed during reproof".into(),
        ));
    }
    Ok(owner)
}

/// Resolve active-configuration candidates while the caller owns USB claims.
pub(super) fn owned_capture_candidates(
    claimed: &ClaimedHandle,
    config: &ConfigDescriptor,
    rate: u32,
    planned_clock: bool,
) -> Result<Vec<CaptureEndpoint>, String> {
    let speed = resolve_device_speed(&claimed.handle.device());
    if let Some(profile) = vendor_pcm::discover(&claimed.handle, config, speed)? {
        return vendor_capture_candidates(config.number(), profile, rate);
    }
    class_capture_candidates(claimed, config, rate, planned_clock)
}

fn class_capture_candidates(
    claimed: &ClaimedHandle,
    config: &ConfigDescriptor,
    rate: u32,
    planned_clock: bool,
) -> Result<Vec<CaptureEndpoint>, String> {
    let raw = read_active_configuration_bytes(&claimed.handle, config)
        .ok_or("cannot read active capture configuration")?;
    let speed = resolve_device_speed(&claimed.handle.device());
    let mut candidates = capture_candidates(&raw, config.number(), speed, rate)?;
    if settings().disable_feedback() {
        candidates.retain(|candidate| candidate.standard_feedforward.is_none());
    }
    for (candidate, route) in
        adc3::adc3_capture_candidates(claimed, config, speed, rate, planned_clock)?
    {
        candidates.push(adc3_capture_endpoint(
            config.number(),
            rate,
            candidate,
            route,
        )?);
    }
    Ok(candidates)
}

/// A proven timing-only IN pipe must never fall through to class capture.
pub(super) fn require_vendor_capture_pcm(profile: &vendor_pcm::VendorPcm) -> Result<(), String> {
    if !profile.input_carries_pcm {
        return Err("capture is unsupported: vendor IN stream carries playback timing only".into());
    }
    Ok(())
}

fn vendor_capture_candidates(
    configuration: u8,
    profile: vendor_pcm::VendorPcm,
    rate: u32,
) -> Result<Vec<CaptureEndpoint>, String> {
    if require_vendor_capture_pcm(&profile).is_err() || !profile.supports_rate(rate) {
        return Ok(vec![]);
    }
    Ok(vec![vendor_capture_endpoint(configuration, profile, rate)?])
}

fn vendor_capture_endpoint(
    configuration: u8,
    profile: vendor_pcm::VendorPcm,
    rate: u32,
) -> Result<CaptureEndpoint, String> {
    require_vendor_capture_pcm(&profile)?;
    if !profile.supports_rate(rate) {
        return Err("unsupported vendor capture rate".into());
    }
    let format = CaptureFormat::pcm(
        rate,
        profile.input.subslot_bytes * 8,
        profile.input.subslot_bytes,
        ChannelMap::uac2(profile.input.channels, 0)
            .map_err(|e| format!("vendor channel map: {e:?}"))?,
    )
    .map_err(|e| format!("vendor capture format: {e:?}"))?;
    Ok(CaptureEndpoint {
        configuration,
        interface: profile.input_interface_number,
        alternate: profile.input_alt_setting,
        address: profile.input.address,
        service_us: profile.input.service_us,
        packet_bytes: profile.input.max_packet_bytes,
        format,
        protocol: 0xff,
        terminal: 0,
        control: profile.input_interface_number,
        control_descriptors: vec![],
        endpoint_descriptors: vec![],
        sampling_control: false,
        fixed_rate: !profile.rate_control,
        adc3_route: None,
        standard_feedforward: None,
        vendor_pcm: Some(profile),
    })
}

pub(super) fn verify_active_capture_alt(
    claimed: &ClaimedHandle,
    interface: u8,
    expected: u8,
) -> Result<(), Error> {
    claimed
        .ensure_interface_claimed(interface)
        .map_err(Error::Claim)?;
    let mut alternate = [0u8];
    let read = claimed
        .handle
        .read_control(
            0x81,
            0x0a,
            0,
            u16::from(interface),
            &mut alternate,
            Duration::from_millis(500),
        )
        .map_err(|e| {
            if e == rusb::Error::NoDevice {
                Error::DeviceLost
            } else {
                Error::Control(e.to_string())
            }
        })?;
    if read != 1 || alternate[0] != expected {
        return Err(Error::InvalidState(
            "streaming alternate did not verify".into(),
        ));
    }
    Ok(())
}

/// Preserve the vendor activation call sites while sharing standard GET_INTERFACE.
pub(super) fn verify_vendor_interface_alternate(
    claimed: &ClaimedHandle,
    interface: u8,
    expected: u8,
) -> Result<(), Error> {
    verify_active_capture_alt(claimed, interface, expected)
}

pub(super) fn verify_vendor_capture_alternate(
    claimed: &ClaimedHandle,
    candidate: &CaptureEndpoint,
    expected: u8,
) -> Result<(), Error> {
    verify_vendor_interface_alternate(claimed, candidate.interface, expected)
}

/// Rediscover the exact fixed duplex profile without sending class clock or power requests.
pub(super) fn verify_vendor_capture(
    claimed: &ClaimedHandle,
    candidate: &CaptureEndpoint,
) -> Result<(), Error> {
    let expected = candidate
        .vendor_pcm
        .as_ref()
        .ok_or_else(|| Error::InvalidState("missing vendor capture profile".into()))?;
    require_vendor_capture_pcm(expected).map_err(|reason| Error::Unsupported {
        reason,
        offered: vec![],
    })?;
    for interface in expected.claimed_interfaces() {
        claimed
            .ensure_interface_claimed(interface)
            .map_err(Error::Claim)?;
    }
    let config = claimed
        .handle
        .device()
        .active_config_descriptor()
        .map_err(|e| Error::Claim(e.to_string()))?;
    if config.number() != candidate.configuration {
        return Err(Error::InvalidState(
            "vendor capture configuration changed".into(),
        ));
    }
    let actual = vendor_pcm::discover(
        &claimed.handle,
        &config,
        resolve_device_speed(&claimed.handle.device()),
    )
    .map_err(Error::Control)?;
    let reproven = vendor_capture_endpoint(
        candidate.configuration,
        *expected,
        candidate.format.sample_rate(),
    )
    .map_err(Error::Control)?;
    if actual.as_ref() != Some(expected) || &reproven != candidate {
        return Err(Error::InvalidState(
            "vendor capture wire profile changed".into(),
        ));
    }
    Ok(())
}

fn adc3_capture_endpoint(
    configuration: u8,
    rate: u32,
    candidate: IsoStreamCandidate,
    route: adc3::Adc3Route,
) -> Result<CaptureEndpoint, String> {
    let format = CaptureFormat::pcm(
        rate,
        candidate.bit_resolution,
        candidate.subslot_size,
        adc3::capture_channel_map(&route)?,
    )
    .map_err(|e| format!("ADC3 capture format: {e:?}"))?;
    Ok(CaptureEndpoint {
        configuration,
        interface: candidate.interface_number,
        alternate: candidate.alt_setting,
        address: candidate.endpoint_address,
        service_us: candidate.service_interval_us,
        packet_bytes: candidate.max_packet_bytes,
        format,
        protocol: 0x30,
        terminal: candidate.terminal_link.unwrap_or(0),
        control: route.control,
        control_descriptors: vec![],
        endpoint_descriptors: vec![],
        sampling_control: false,
        fixed_rate: route.clock.is_none(),
        adc3_route: Some(route),
        standard_feedforward: None,
        vendor_pcm: None,
    })
}

/// Explicit capture registration has its own narrow claim policy. Playback's
/// optional-feedback claims remain unchanged. Exact vendor pairs can require OUT.
pub(super) fn validate_capture_registration(
    device: &UsbDevice,
    expected_configuration: Option<u8>,
) -> Result<(), String> {
    let claimed = wrap_android_usb_handle(device)?;
    let config = claimed
        .handle
        .device()
        .active_config_descriptor()
        .map_err(|e| e.to_string())?;
    if expected_configuration.is_some_and(|id| id != config.number()) {
        return Err("capture configuration changed since host claim handoff".into());
    }
    if let Some(profile) = vendor_pcm::discover(
        &claimed.handle,
        &config,
        resolve_device_speed(&claimed.handle.device()),
    )? {
        require_vendor_capture_pcm(&profile)?;
        let identity = claimed
            .handle
            .device()
            .device_descriptor()
            .map_err(|e| e.to_string())?;
        if identity.vendor_id() != device.vendor_id || identity.product_id() != device.product_id {
            return Err("capture host and actual USB identities differ".into());
        }
        if !profile.claims_match(&device.claimed_interfaces) {
            return Err(
                "vendor capture requires its exact proven streaming interface claim set".into(),
            );
        }
        return Ok(());
    }
    let raw = read_active_configuration_bytes(&claimed.handle, &config)
        .ok_or("cannot read active capture configuration")?;
    let interfaces = raw_interfaces(&raw)?;
    let speed = resolve_device_speed(&claimed.handle.device());
    let ids: HashSet<u8> = device.claimed_interfaces.iter().copied().collect();
    if ids.len() != device.claimed_interfaces.len() || ids.is_empty() {
        return Err("capture claims are empty or contain duplicates".into());
    }
    let mut allowed = HashSet::new();
    let mut has_input = false;
    for interface in &interfaces {
        if interface.class != USB_CLASS_AUDIO
            || interface.subclass != USB_SUBCLASS_AUDIOSTREAMING
            || !matches!(interface.protocol, 0 | 0x20 | 0x30)
        {
            continue;
        }
        let Some(control) = capture_control(&raw, &interfaces, interface) else {
            continue;
        };
        if claimable_capture_input(interface, speed) {
            allowed.insert(interface.number);
            allowed.insert(control.number);
            if ids.contains(&interface.number) {
                if !ids.contains(&control.number) {
                    return Err("capture's owning AudioControl interface is not claimed".into());
                }
                has_input = true;
            }
        }
    }
    // Explicit host opt-in may add only this exact actual-device vendor interface.
    // It never substitutes for a PCM IN interface and its owning AudioControl.
    let identity = claimed
        .handle
        .device()
        .device_descriptor()
        .map_err(|e| e.to_string())?;
    if identity.vendor_id() != device.vendor_id || identity.product_id() != device.product_id {
        return Err("capture host and actual USB identities differ".into());
    }
    if let Some(interface) = super::routing_hw::scarlett_claim_interface(
        &config,
        identity.vendor_id(),
        identity.product_id(),
    ) {
        allowed.insert(interface);
    }
    if !has_input || !ids.is_subset(&allowed) {
        return Err("capture claims must contain only explicit IN interfaces and their owning AudioControl interfaces".into());
    }
    Ok(())
}

/// Standard descriptor requests only: resolve the explicit capture claim set
/// before Android detaches any interface. No stream or class write is started.
pub(crate) fn capture_claim_preflight(
    device: &UsbDevice,
) -> Result<(u8, Vec<u8>, Vec<u8>), String> {
    let claimed = wrap_android_usb_handle(device)?;
    let config = claimed
        .handle
        .device()
        .active_config_descriptor()
        .map_err(|e| e.to_string())?;
    if let Some(profile) = vendor_pcm::discover(
        &claimed.handle,
        &config,
        resolve_device_speed(&claimed.handle.device()),
    )? {
        require_vendor_capture_pcm(&profile)?;
        return Ok((
            config.number(),
            vec![],
            vec![profile.input_interface_number],
        ));
    }
    let raw = read_active_configuration_bytes(&claimed.handle, &config)
        .ok_or("cannot read active capture configuration")?;
    let interfaces = raw_interfaces(&raw)?;
    let speed = resolve_device_speed(&claimed.handle.device());
    let mut controls = Vec::new();
    let mut inputs = Vec::new();
    for interface in &interfaces {
        if interface.class != USB_CLASS_AUDIO
            || interface.subclass != USB_SUBCLASS_AUDIOSTREAMING
            || interface.alternate == 0
            || !matches!(interface.protocol, 0 | 0x20 | 0x30)
        {
            continue;
        }
        let Some(control) = capture_control(&raw, &interfaces, interface) else {
            continue;
        };
        let probe_rate =
            if interface.protocol == 0 {
                let Some(format) = interface.descriptors.iter().find(|d| {
                    d.len() >= 11 && d[1] == USB_DT_CS_INTERFACE && d[2] == 2 && d[3] == 1
                }) else {
                    continue;
                };
                u32::from_le_bytes([format[8], format[9], format[10], 0])
            } else {
                48_000
            };
        if interface.protocol != 0x30 && capture_format(interface, control, probe_rate).is_none() {
            continue;
        }
        if claimable_capture_input(interface, speed) {
            controls.push(control.number);
            inputs.push(interface.number);
        }
    }
    controls.sort_unstable();
    controls.dedup();
    inputs.sort_unstable();
    inputs.dedup();
    if inputs.is_empty() {
        return Err("active configuration has no supported PCM capture interface".into());
    }
    Ok((config.number(), controls, inputs))
}

struct CaptureUsbOwner {
    claimed: Option<ClaimedHandle>,
    keep_locked: bool,
    clock_route: Option<clock::CaptureClockRoute>,
}
impl CaptureUsbOwner {
    fn claimed(&self) -> &ClaimedHandle {
        self.claimed.as_ref().expect("live capture owner")
    }
}
impl Drop for CaptureUsbOwner {
    fn drop(&mut self) {
        if let Some(claimed) = self.claimed.take() {
            // The worker's transfer pools have retired before this owner drops.
            // Hand the local clock back at alt zero, before cleanup releases any
            // ADC3 power leases or returns the handle to the idle lock.
            let mut keep_locked = self.keep_locked;
            if let Some(route) = self.clock_route.take() {
                let active = claimed.active_interfaces.lock().clone();
                let idle = active
                    .into_iter()
                    .try_for_each(|interface| claimed.set_alternate_setting(interface, 0));
                let restored =
                    idle.and_then(|()| clock::restore_capture_clock_route(&claimed, &route));
                if let Err(error) = restored {
                    keep_locked = false;
                    claimed.adc3_power.disarm();
                    set_last_error(Some(format!("capture clock hand-back failed: {error}")));
                    log_warn!("[USB] Capture clock hand-back failed: {error}");
                }
            }
            cleanup_claimed_handle(claimed, None, keep_locked);
        }
    }
}

/// Build only from the active, strictly resolved capture route after activation.
pub(super) fn status_target(
    claimed: &ClaimedHandle,
    endpoint: &CaptureEndpoint,
) -> Result<Option<audio_status::StreamStatusTarget>, Error> {
    if endpoint.vendor_pcm.is_some() {
        return Ok(None);
    }
    if endpoint.protocol == 0 {
        // Standard capture admission already proved this collection and
        // terminal. Only controllable endpoints required an exact positive
        // activation readback; fixed and host-paced rates are not CUR proof.
        if !claimed.claimed_interfaces.contains(&endpoint.control) {
            return Ok(None);
        }
        let rate = endpoint.format.sample_rate();
        return Ok(Some(audio_status::StreamStatusTarget::uac1(
            endpoint.control,
            endpoint.interface,
            endpoint.alternate,
            endpoint.address,
            (endpoint.sampling_control && rate > 0).then_some(rate),
            Some(endpoint.terminal),
        )));
    }
    if !matches!(endpoint.protocol, 0x20 | 0x30) {
        return Ok(None);
    }
    let proven = if let Some(route) = endpoint.adc3_route.as_ref() {
        let Some(choice) = route.choice.as_ref() else {
            return Ok(None);
        }; // BADD
        route
            .source_rate(endpoint.format.sample_rate())
            .ok()
            .map(|rate| (choice.clone(), rate))
    } else {
        claimed
            .handle
            .device()
            .active_config_descriptor()
            .ok()
            .and_then(|config| {
                let choice = strict_implicit_clock(
                    &config,
                    &claimed.handle,
                    endpoint.control,
                    Some(endpoint.terminal),
                )?;
                let rate = uac2_clock_ratio(Some(&choice))
                    .ok()?
                    .source_rate(endpoint.format.sample_rate())?;
                Some((choice, rate))
            })
    };
    // Optional monitoring never turns an inconclusive extra snapshot into a
    // new startup gate. The completed configuration owns initial verification.
    let mut target = audio_status::StreamStatusTarget::new(
        endpoint.protocol,
        endpoint.control,
        endpoint.interface,
        endpoint.alternate,
        proven.as_ref().map(|(choice, rate)| (choice, *rate)),
    );
    if let Some(route) = endpoint.adc3_route.as_ref() {
        for (interface, expected_mask) in route.active_format_expectations() {
            target.add_adc3_active_format(interface, expected_mask);
        }
    }
    target.add_data_endpoint(endpoint.interface, endpoint.alternate, endpoint.address);
    Ok(Some(target))
}

pub(super) fn configure_capture_endpoint_guarded(
    claimed: &ClaimedHandle,
    candidate: &CaptureEndpoint,
    expected_source: Option<(&ClockChoice, u32)>,
) -> Result<bool, Error> {
    verify_standard_capture(claimed, candidate)?;
    if let Some(profile) = candidate.vendor_pcm.as_ref() {
        verify_vendor_capture(claimed, candidate)?;
        for interface in profile.claimed_interfaces() {
            claimed
                .ensure_interface_claimed(interface)
                .map_err(Error::Claim)?;
            claimed
                .set_alternate_setting(interface, 0)
                .map_err(Error::Claim)?;
            verify_vendor_interface_alternate(claimed, interface, 0)?;
        }
        if profile.requires_output_for_capture {
            claimed
                .set_alternate_setting(profile.interface_number, profile.alt_setting)
                .map_err(Error::Claim)?;
            verify_vendor_interface_alternate(
                claimed,
                profile.interface_number,
                profile.alt_setting,
            )?;
        }
        claimed
            .set_alternate_setting(candidate.interface, candidate.alternate)
            .map_err(Error::Claim)?;
        verify_vendor_capture(claimed, candidate)?;
        verify_vendor_capture_alternate(claimed, candidate, candidate.alternate)?;
        if profile.requires_output_for_capture {
            verify_vendor_interface_alternate(
                claimed,
                profile.interface_number,
                profile.alt_setting,
            )?;
        }
        if profile.rate_control {
            vendor_pcm::program_rate(claimed, profile, candidate.format.sample_rate())?;
        }
        return Ok(true);
    }
    if candidate.protocol == 0 && !candidate.sampling_control && !candidate.fixed_rate {
        // Prove the explicit bit-clear descriptor before any alternate writes.
        // Topology discovery alone cannot distinguish missing control evidence.
        uac1_capture_sampling_control(candidate)?;
    }
    claimed
        .ensure_interface_claimed(candidate.control)
        .map_err(Error::Claim)?;
    claimed
        .set_alternate_setting(candidate.interface, 0)
        .map_err(Error::Claim)?;
    let rate = candidate.format.sample_rate();
    if candidate.protocol == 0 {
        claimed
            .set_alternate_setting(candidate.interface, candidate.alternate)
            .map_err(Error::Claim)?;
        if candidate.sampling_control {
            if let Err(Error::DeviceLost) = set_uac1_sampling_frequency(
                &claimed.handle,
                candidate.interface,
                candidate.address,
                rate,
            )
            .or_control()
            {
                return Err(Error::DeviceLost);
            }
            // A read-only endpoint may stall SET_CUR. Its active rate still
            // needs an exact positive readback before capture can proceed.
            let reported = match get_uac1_sampling_frequency(
                &claimed.handle,
                candidate.interface,
                candidate.address,
            )
            .or_control()
            {
                Ok(reported) => Some(reported),
                Err(Error::DeviceLost) => return Err(Error::DeviceLost),
                Err(_) => None,
            };
            if rate == 0 || reported != Some(rate) {
                return Err(Error::ClockUnverified {
                    requested_hz: rate,
                    reported_hz: reported,
                });
            }
        } else if !candidate.fixed_rate && !candidate.requires_host_rate_proof() {
            return Err(Error::ClockUnverified {
                requested_hz: rate,
                reported_hz: None,
            });
        }
    } else if let Some(route) = &candidate.adc3_route {
        let config = claimed
            .handle
            .device()
            .active_config_descriptor()
            .map_err(|e| Error::Claim(e.to_string()))?;
        let actual = adc3::adc3_capture_route_for_endpoint(
            claimed,
            &config,
            candidate.interface,
            candidate.alternate,
            route,
        )
        .map_err(Error::Control)?;
        if &actual != route {
            return Err(Error::InvalidState(
                "ADC3 capture route changed; negotiate again".into(),
            ));
        }
        if let Some((expected, expected_raw_rate)) = expected_source {
            if actual.choice.as_ref() != Some(expected)
                || route.source_rate(rate).map_err(Error::Control)? != expected_raw_rate
            {
                return Err(Error::InvalidState(
                    "ADC3 capture clock differs from the proven playback source".into(),
                ));
            }
            // A shared playback source is already proven and active. Verify it
            // read-only before IN activation instead of programming its rate.
            adc3::verify_adc3_capture_activation(&claimed.handle, &actual, rate)
                .map_err(Error::Control)?;
        } else {
            adc3::configure_adc3(&claimed.handle, route, rate).map_err(Error::Control)?;
        }
        // Full ADC3 owns an explicit clock choice; BADD retains its existing
        // activation/error contract and must not authorize a retry.
        if route.choice.is_some() {
            if !claimed
                .try_set_alternate_setting(candidate.interface, candidate.alternate)
                .map_err(|error| {
                    if crate::error::is_device_gone(&error) {
                        Error::DeviceLost
                    } else {
                        Error::Claim(error)
                    }
                })?
            {
                return Ok(false);
            }
        } else {
            claimed
                .set_alternate_setting(candidate.interface, candidate.alternate)
                .map_err(Error::Claim)?;
        }
        let actual = adc3::adc3_capture_route_for_endpoint(
            claimed,
            &config,
            candidate.interface,
            candidate.alternate,
            route,
        )
        .map_err(Error::Control)?;
        if &actual != route {
            return Err(Error::InvalidState(
                "ADC3 capture route changed during activation".into(),
            ));
        }
        adc3::verify_adc3_active_formats(&claimed.handle, &actual).or_control()?;
        adc3::verify_adc3_capture_activation(&claimed.handle, &actual, rate)
            .map_err(Error::Control)?;
    } else {
        let config = claimed
            .handle
            .device()
            .active_config_descriptor()
            .map_err(|e| Error::Claim(e.to_string()))?;
        let choice = strict_implicit_clock(
            &config,
            &claimed.handle,
            candidate.control,
            Some(candidate.terminal),
        )
        .ok_or_else(|| Error::Unsupported {
            reason: "capture clock topology cannot prove the terminal's current source".into(),
            offered: vec![],
        })?;
        let source = DescriptorIter::new(&candidate.control_descriptors)
            .find(|d| {
                d.len() == 8
                    && d[1] == USB_DT_CS_INTERFACE
                    && d[2] == 0x0a
                    && d[3] == choice.source_id
            })
            .ok_or_else(|| Error::Control("capture Clock Source descriptor is missing".into()))?;
        if source[5] & 1 == 0 {
            return Err(Error::ClockUnverified {
                requested_hz: rate,
                reported_hz: None,
            });
        }
        let ratio = uac2_clock_ratio(Some(&choice)).map_err(Error::Control)?;
        let source_rate = ratio.source_rate(rate).ok_or_else(|| Error::Unsupported {
            reason: "capture wire rate has no exact representable root source rate".into(),
            offered: vec![],
        })?;
        let clock = AudioControlClock {
            interface_number: candidate.control,
            clock_id: choice.source_id,
        };
        verify_uac2_clock_factors(&claimed.handle, &choice).map_err(Error::Control)?;
        let original_source =
            get_sampling_frequency(&claimed.handle, candidate.control, choice.source_id)
                .map_err(Error::Control)?;
        if expected_source
            .is_some_and(|(expected, original)| expected != &choice || original != original_source)
        {
            return Err(Error::InvalidState(
                "capture clock changed since its rollback snapshot".into(),
            ));
        }
        let reported = get_wire_frequency(&claimed.handle, clock, Some(&choice)).ok();
        if reported != Some(rate) {
            // A caller that already proved the target source rate expects a
            // read-only verification during activation.
            if expected_source.is_some_and(|(_, proven)| proven == source_rate)
                || source[5] & 3 != 3
            {
                return Err(Error::ClockUnverified {
                    requested_hz: rate,
                    reported_hz: reported,
                });
            }
            let ranges = clock::get_uac2_sampling_frequency_ranges(
                &claimed.handle,
                candidate.control,
                choice.source_id,
            )
            .map_err(Error::Control)?;
            if !sampling_frequency_ranges_support_rate(&ranges, source_rate) {
                return Err(Error::Unsupported {
                    reason: "capture source does not support the exact requested rate".into(),
                    offered: vec![],
                });
            }
            // A multiplier-backed source belongs to the complete function;
            // never change it while an unowned sibling can consume that root.
            if choice.path.iter().any(|hop| {
                matches!(
                    hop.kind,
                    crate::clock_resolution::ClockHopKind::RateMultiplier { .. }
                )
            }) && !clock::stop_complete_capture_clock_function(claimed, candidate.interface)
                .map_err(Error::Control)?
            {
                return Err(Error::Control(
                    "capture multiplier programming requires complete idle function ownership"
                        .into(),
                ));
            }
            verify_uac2_clock_factors(&claimed.handle, &choice).map_err(Error::Control)?;
            if strict_implicit_clock(
                &config,
                &claimed.handle,
                candidate.control,
                Some(candidate.terminal),
            )
            .as_ref()
                != Some(&choice)
            {
                return Err(Error::InvalidState(
                    "capture clock route changed before source programming".into(),
                ));
            }
            if get_sampling_frequency(&claimed.handle, candidate.control, choice.source_id)
                .map_err(Error::Control)?
                != original_source
            {
                return Err(Error::InvalidState(
                    "capture source changed before programming".into(),
                ));
            }
            set_sampling_frequency(
                &claimed.handle,
                candidate.control,
                choice.source_id,
                source_rate,
            )
            .map_err(Error::Control)?;
        }
        verify_uac2_clock_factors(&claimed.handle, &choice).map_err(Error::Control)?;
        let reported = get_wire_frequency(&claimed.handle, clock, Some(&choice)).ok();
        if reported != Some(rate) {
            return Err(Error::ClockUnverified {
                requested_hz: rate,
                reported_hz: reported,
            });
        }
        if source[5] & 0x04 != 0
            && get_clock_validity(&claimed.handle, candidate.control, choice.source_id) != Ok(true)
        {
            return Err(Error::ClockUnverified {
                requested_hz: rate,
                reported_hz: reported,
            });
        }
        if !claimed
            .try_set_alternate_setting(candidate.interface, candidate.alternate)
            .map_err(|error| {
                if crate::error::is_device_gone(&error) {
                    Error::DeviceLost
                } else {
                    Error::Claim(error)
                }
            })?
        {
            return Ok(false);
        }
        // Re-read route after activation; no fallback clock can authorize IN.
        if strict_implicit_clock(
            &config,
            &claimed.handle,
            candidate.control,
            Some(candidate.terminal),
        )
        .as_ref()
            != Some(&choice)
        {
            return Err(Error::InvalidState(
                "capture clock route changed during activation".into(),
            ));
        }
        // Activation can change clock controls without changing a selector.
        // Prove both the exact factor snapshot and terminal wire cadence again.
        verify_uac2_clock_factors(&claimed.handle, &choice).map_err(Error::Control)?;
        let reported = get_wire_frequency(&claimed.handle, clock, Some(&choice)).ok();
        verify_capture_wire_rate(rate, reported)?;
        if source[5] & 0x04 != 0
            && get_clock_validity(&claimed.handle, candidate.control, choice.source_id) != Ok(true)
        {
            return Err(Error::ClockUnverified {
                requested_hz: rate,
                reported_hz: reported,
            });
        }
    }
    verify_standard_capture(claimed, candidate)?;
    Ok(true)
}

fn verify_capture_wire_rate(rate: u32, reported: Option<u32>) -> Result<(), Error> {
    if reported == Some(rate) {
        Ok(())
    } else {
        Err(Error::ClockUnverified {
            requested_hz: rate,
            reported_hz: reported,
        })
    }
}

/// Deferred metadata has fixed size and is never locked by a reader. Capture
/// presentation may hold its queue mutex for a long copy; USB completion work
/// must still classify packets, feed implicit timing and retire every endpoint.
#[derive(Default)]
pub(crate) struct DeferredCapture {
    reordered_completions: AtomicU64,
    captured_frames: AtomicU64,
    delivered_frames: AtomicU64,
    overrun_frames: AtomicU64,
    contention_frames: AtomicU64,
    proof_suppressed_frames: AtomicU64,
    empty_packets: AtomicU64,
    short_packets: AtomicU64,
    failed_packets: AtomicU64,
    malformed_packets: AtomicU64,
    malformed_completions: AtomicU64,
    failed_transfers: AtomicU64,
    missing_frames: AtomicU64,
    terminal: AtomicU32,
}
impl DeferredCapture {
    pub(super) fn record(&self, stats: crate::capture::CaptureStats) {
        self.reordered_completions
            .fetch_add(stats.reordered_completions, Ordering::Relaxed);
        self.captured_frames
            .fetch_add(stats.captured_frames, Ordering::Relaxed);
        self.delivered_frames
            .fetch_add(stats.delivered_frames, Ordering::Relaxed);
        self.overrun_frames
            .fetch_add(stats.overrun_frames, Ordering::Relaxed);
        self.contention_frames
            .fetch_add(stats.contention_frames, Ordering::Relaxed);
        self.proof_suppressed_frames
            .fetch_add(stats.proof_suppressed_frames, Ordering::Relaxed);
        self.empty_packets
            .fetch_add(stats.empty_packets, Ordering::Relaxed);
        self.short_packets
            .fetch_add(stats.short_packets, Ordering::Relaxed);
        self.failed_packets
            .fetch_add(stats.failed_packets, Ordering::Relaxed);
        self.malformed_packets
            .fetch_add(stats.malformed_packets, Ordering::Relaxed);
        self.malformed_completions
            .fetch_add(stats.malformed_completions, Ordering::Relaxed);
        self.failed_transfers
            .fetch_add(stats.failed_transfers, Ordering::Relaxed);
        self.missing_frames
            .fetch_add(stats.missing_frames, Ordering::Relaxed);
    }
    pub(super) fn finish(&self, failed: bool) {
        self.terminal
            .fetch_max(if failed { 2 } else { 1 }, Ordering::Release);
    }
    pub(super) fn is_finished(&self) -> bool {
        self.terminal.load(Ordering::Acquire) != 0
    }
    fn take_delta(&self) -> crate::capture::CaptureStats {
        crate::capture::CaptureStats {
            reordered_completions: self.reordered_completions.swap(0, Ordering::Relaxed),
            captured_frames: self.captured_frames.swap(0, Ordering::Relaxed),
            delivered_frames: self.delivered_frames.swap(0, Ordering::Relaxed),
            overrun_frames: self.overrun_frames.swap(0, Ordering::Relaxed),
            contention_frames: self.contention_frames.swap(0, Ordering::Relaxed),
            proof_suppressed_frames: self.proof_suppressed_frames.swap(0, Ordering::Relaxed),
            empty_packets: self.empty_packets.swap(0, Ordering::Relaxed),
            short_packets: self.short_packets.swap(0, Ordering::Relaxed),
            failed_packets: self.failed_packets.swap(0, Ordering::Relaxed),
            malformed_packets: self.malformed_packets.swap(0, Ordering::Relaxed),
            malformed_completions: self.malformed_completions.swap(0, Ordering::Relaxed),
            failed_transfers: self.failed_transfers.swap(0, Ordering::Relaxed),
            missing_frames: self.missing_frames.swap(0, Ordering::Relaxed),
        }
    }
    fn state(&self) -> crate::capture::CaptureState {
        match self.terminal.load(Ordering::Acquire) {
            2 => crate::capture::CaptureState::Failed,
            1 => crate::capture::CaptureState::Stopped,
            _ => crate::capture::CaptureState::Active,
        }
    }
}

/// One USB-worker producer and serialized consumers. Cursor publication also
/// transfers ownership of the corresponding atomic bytes between them.
pub(crate) struct CaptureSpsc {
    bytes: Box<[std::sync::atomic::AtomicU8]>,
    frame_bytes: usize,
    capacity: usize,
    period: usize,
    write_seq: std::sync::atomic::AtomicUsize,
    read_seq: std::sync::atomic::AtomicUsize,
    pub(super) consumer: Mutex<crate::capture::CaptureStats>,
    pub(super) deferred: Arc<DeferredCapture>,
}
impl CaptureSpsc {
    pub(super) fn new(
        capacity: usize,
        frame_bytes: usize,
    ) -> Result<Self, crate::capture::CaptureError> {
        use crate::capture::CaptureError;
        let period = capacity
            .checked_mul(2)
            .filter(|_| capacity != 0)
            .ok_or(CaptureError::InvalidCapacity)?;
        let size = capacity
            .checked_mul(frame_bytes)
            .filter(|size| *size != 0)
            .ok_or(CaptureError::InvalidCapacity)?;
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(size)
            .map_err(|_| CaptureError::AllocationFailed)?;
        bytes.resize_with(size, || std::sync::atomic::AtomicU8::new(0));
        Ok(Self {
            bytes: bytes.into_boxed_slice(),
            frame_bytes,
            capacity,
            period,
            write_seq: std::sync::atomic::AtomicUsize::new(0),
            read_seq: std::sync::atomic::AtomicUsize::new(0),
            consumer: Mutex::new(Default::default()),
            deferred: Arc::new(DeferredCapture::default()),
        })
    }
    fn distance(&self, write: usize, read: usize) -> usize {
        if write >= read {
            write - read
        } else {
            self.period - read + write
        }
    }
    fn advance(&self, seq: usize, frames: usize) -> usize {
        let remaining = self.period - seq;
        if frames >= remaining {
            frames - remaining
        } else {
            seq + frames
        }
    }
    fn active(&self) -> Result<(), crate::capture::CaptureError> {
        use crate::capture::{CaptureError, CaptureState};
        match self.deferred.state() {
            CaptureState::Active => Ok(()),
            CaptureState::Stopped => Err(CaptureError::Stopped),
            CaptureState::Failed => Err(CaptureError::Failed),
        }
    }
    // Sole producer is the USB worker; only complete, classified frames enter.
    pub(super) fn push_packet(&self, data: &[u8]) -> Result<usize, crate::capture::CaptureError> {
        self.active()?;
        if data.len() % self.frame_bytes != 0 {
            return Err(crate::capture::CaptureError::PartialFrame);
        }
        let write = self.write_seq.load(Ordering::Relaxed);
        let read = self.read_seq.load(Ordering::Acquire);
        let occupied = self.distance(write, read);
        debug_assert!(occupied <= self.capacity);
        let accepted = (data.len() / self.frame_bytes).min(self.capacity - occupied);
        let start = (write % self.capacity) * self.frame_bytes;
        let bytes = &data[..accepted * self.frame_bytes];
        let first = bytes.len().min(self.bytes.len() - start);
        for (slot, byte) in self.bytes[start..start + first]
            .iter()
            .chain(self.bytes[..bytes.len() - first].iter())
            .zip(bytes)
        {
            slot.store(*byte, Ordering::Relaxed);
        }
        self.write_seq
            .store(self.advance(write, accepted), Ordering::Release);
        Ok(accepted)
    }
    fn read(&self, dst: &mut [u8]) -> Result<usize, crate::capture::CaptureError> {
        let mut stats = self.consumer.lock();
        self.active()?;
        if dst.len() % self.frame_bytes != 0 {
            return Err(crate::capture::CaptureError::PartialFrame);
        }
        let read = self.read_seq.load(Ordering::Relaxed);
        let write = self.write_seq.load(Ordering::Acquire);
        let available = self.distance(write, read);
        debug_assert!(available <= self.capacity);
        let frames = available.min(dst.len() / self.frame_bytes);
        let start = (read % self.capacity) * self.frame_bytes;
        let bytes = &mut dst[..frames * self.frame_bytes];
        let first = bytes.len().min(self.bytes.len() - start);
        let remaining = bytes.len() - first;
        for (byte, slot) in bytes.iter_mut().zip(
            self.bytes[start..start + first]
                .iter()
                .chain(self.bytes[..remaining].iter()),
        ) {
            *byte = slot.load(Ordering::Relaxed);
        }
        self.read_seq
            .store(self.advance(read, frames), Ordering::Release);
        stats.delivered_frames = stats.delivered_frames.saturating_add(frames as u64);
        Ok(frames)
    }
    fn available_frames(&self) -> usize {
        let _guard = self.consumer.lock();
        self.distance(
            self.write_seq.load(Ordering::Acquire),
            self.read_seq.load(Ordering::Relaxed),
        )
    }
    fn stats(&self) -> crate::capture::CaptureStats {
        let mut stats = self.consumer.lock();
        stats.merge(self.deferred.take_delta());
        *stats
    }
}

pub(crate) enum CapturePresentation {
    Standalone(Arc<Mutex<CaptureBuffer>>),
    Duplex(Arc<CaptureSpsc>),
}
pub(crate) struct CaptureHandle {
    pub(crate) endpoint: CaptureEndpoint,
    pub(crate) presentation: CapturePresentation,
    pub(crate) error: Arc<Mutex<Option<Error>>>,
    pub(crate) stop: Arc<AtomicBool>,
    pub(crate) worker: Option<JoinHandle<()>>,
    pub(crate) paired: bool,
    pub(crate) retired: Option<Arc<AtomicBool>>,
    pub(crate) isolated_failure: Option<Arc<AtomicBool>>,
    pub(crate) capture_retired: Option<Arc<AtomicBool>>,
}
impl CaptureHandle {
    pub(crate) fn available_frames(&self) -> usize {
        match &self.presentation {
            CapturePresentation::Standalone(buffer) => buffer.lock().available_frames(),
            CapturePresentation::Duplex(ring) => ring.available_frames(),
        }
    }
    pub(crate) fn read(&self, dst: &mut [u8]) -> Result<usize, crate::capture::CaptureError> {
        match &self.presentation {
            CapturePresentation::Standalone(buffer) => buffer.lock().read(dst),
            CapturePresentation::Duplex(ring) => ring.read(dst),
        }
    }
    pub(crate) fn stats(&self) -> crate::capture::CaptureStats {
        match &self.presentation {
            CapturePresentation::Standalone(buffer) => buffer.lock().stats(),
            CapturePresentation::Duplex(ring) => ring.stats(),
        }
    }
    pub(crate) fn state(&self) -> crate::capture::CaptureState {
        match &self.presentation {
            CapturePresentation::Standalone(buffer) => buffer.lock().state(),
            CapturePresentation::Duplex(ring) => ring.deferred.state(),
        }
    }
    pub(crate) fn stop(&mut self) -> Result<(), Error> {
        if self
            .isolated_failure
            .as_ref()
            .is_some_and(|flag| flag.load(Ordering::Acquire))
        {
            let retired = self
                .capture_retired
                .as_ref()
                .expect("isolated capture retirement signal");
            let deadline = Instant::now() + Duration::from_secs(3);
            while !retired.load(Ordering::Acquire) && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(10));
            }
            return if retired.load(Ordering::Acquire) {
                Ok(())
            } else {
                Err(Error::Transfer(
                    "USB capture is still retiring; retain host claims and FD".into(),
                ))
            };
        }
        self.stop.store(true, Ordering::Release);
        match &self.presentation {
            CapturePresentation::Standalone(buffer) => {
                let mut buffer = buffer.lock();
                if buffer.state() == crate::capture::CaptureState::Active {
                    buffer.stop();
                }
            }
            CapturePresentation::Duplex(ring) => ring.deferred.finish(false),
        }
        if let Some(worker) = self.worker.take() {
            let deadline = Instant::now() + Duration::from_secs(3);
            while !worker.is_finished() && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(10));
            }
            if !worker.is_finished() {
                self.worker = Some(worker);
                return Err(Error::Transfer(
                    "USB capture is still retiring cancelled transfers; retain host claims and FD"
                        .into(),
                ));
            }
            worker
                .join()
                .map_err(|_| Error::Transfer("USB capture worker panicked".into()))?;
        }
        if self.paired {
            let retired = self
                .retired
                .as_ref()
                .expect("paired capture retirement signal");
            let deadline = Instant::now() + Duration::from_secs(3);
            while !retired.load(Ordering::Acquire) && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(10));
            }
            if !retired.load(Ordering::Acquire) {
                return Err(Error::Transfer(
                    "USB duplex is still retiring; retain host claims and FD".into(),
                ));
            }
        }
        Ok(())
    }
}
impl Drop for CaptureHandle {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

pub(crate) fn open_capture(
    request: &CaptureRequest,
    capacity_frames: usize,
    events: EventSink,
) -> Result<CaptureHandle, Error> {
    if request.sample_rate == 0
        || request.channels == 0
        || request.valid_bits == 0
        || request.valid_bits > 32
    {
        return Err(Error::InvalidArgument(
            "capture needs a nonzero exact PCM rate/channels and 1..32 valid bits".into(),
        ));
    }
    let session_gate = USB_SESSION_GATE.lock();
    if USB_SESSION_CLEAR_PENDING.load(Ordering::SeqCst) || is_usb_session_active() {
        return Err(Error::Busy);
    }
    let device = SESSION_STATE
        .lock()
        .as_ref()
        .map(|state| state.device.clone())
        .ok_or_else(|| Error::InvalidState("no DAC is open".into()))?;
    let session = Arc::new(SessionGuard::start_locked());
    drop(session_gate);
    // This owner is created before transfer pools, so all callbacks retire
    // before an alternate is reset or the existing idle handle is returned.
    let claimed = open_claimed_usb_handle(&device).map_err(Error::Claim)?;
    let mut owner = CaptureUsbOwner {
        claimed: Some(claimed),
        keep_locked: current_lock_requested_for_fd(device.fd),
        clock_route: None,
    };
    let usb = owner.claimed().handle.device();
    let config = usb
        .active_config_descriptor()
        .map_err(|e| Error::Claim(e.to_string()))?;
    let vendor_profile =
        vendor_pcm::discover(&owner.claimed().handle, &config, resolve_device_speed(&usb))
            .map_err(Error::Claim)?;
    let mut candidates = if let Some(profile) = vendor_profile {
        require_vendor_capture_pcm(&profile).map_err(|reason| Error::Unsupported {
            reason,
            offered: vec![],
        })?;
        vendor_capture_candidates(config.number(), profile, request.sample_rate)
    } else {
        class_capture_candidates(owner.claimed(), &config, request.sample_rate, false)
    }
    .map_err(Error::Claim)?;
    candidates.retain(|candidate| {
        candidate.format.satisfies(request)
            && device.claimed_interfaces.contains(&candidate.interface)
            && device.claimed_interfaces.contains(&candidate.control)
    });
    candidates.sort_by_key(|candidate| {
        (
            candidate.format.subslot_bytes(),
            candidate.interface,
            candidate.alternate,
            candidate.address,
        )
    });
    let current = candidates.first().cloned();
    let mut endpoint = if vendor_profile.is_some() || current.as_ref().is_some_and(|candidate| candidate.protocol != 0x30) {
        current
    } else {
        adc3::select_standalone_adc3_capture(owner.claimed(), &config, request)
            .map_err(Error::Control)?
            .map(|(candidate, route)| adc3_capture_endpoint(config.number(), request.sample_rate, candidate, route))
            .transpose().map_err(Error::Control)?
    }.ok_or_else(|| Error::Unsupported {
        reason: "no claimed standard PCM IN alternate matches the exact capture rate, width and channel map".into(), offered: vec![] })?;
    // Live UAC2 alternate validity may reject the preferred descriptor match.
    // Keep retries inside that same capture route; never switch function,
    // terminal, interface or protocol because its first alternate is invalid.
    let standard_uac2 =
        endpoint.protocol == 0x20 && endpoint.vendor_pcm.is_none() && endpoint.adc3_route.is_none();
    let input = (
        endpoint.configuration,
        endpoint.interface,
        endpoint.control,
        endpoint.terminal,
    );
    let mut siblings = candidates.into_iter().skip(1).filter(move |candidate| {
        standard_uac2
            && candidate.protocol == 0x20
            && candidate.vendor_pcm.is_none()
            && candidate.adc3_route.is_none()
            && (
                candidate.configuration,
                candidate.interface,
                candidate.control,
                candidate.terminal,
            ) == input
    });
    let (buffer, geometry) = loop {
        // Reject invalid capacity/transfer geometry before touching clocks.
        let buffer = Arc::new(Mutex::new(
            CaptureBuffer::new(endpoint.format.clone(), capacity_frames)
                .map_err(|e| Error::InvalidArgument(format!("capture buffer: {e:?}")))?,
        ));
        let geometry = IsoQueueGeometry::for_config(endpoint.service_us, &settings().config);
        capture_queue_bytes(endpoint.packet_bytes, geometry)?;
        if standard_uac2 {
            verify_standard_capture(owner.claimed(), &endpoint)?;
            let mut route = None;
            let result = clock::select_capture_clock_route(
                owner.claimed(),
                endpoint.interface,
                endpoint.alternate,
                endpoint.terminal,
                endpoint.control,
                endpoint.format.sample_rate(),
                &mut route,
            );
            // Preserve the owning rollback lease even when selection aborts.
            owner.clock_route = route;
            if !result.or_control()? {
                // False promises successful hand-back and no saved lease.
                // Every other failure aborts without trying another route.
                endpoint = siblings.next().ok_or_else(|| Error::Unsupported {
                    reason: "no matching UAC2 PCM IN alternate is currently available at the verified capture rate".into(),
                    offered: vec![],
                })?;
                continue;
            }
        }
        break (buffer, geometry);
    };
    if !configure_capture_endpoint_guarded(
        owner.claimed(),
        &endpoint,
        owner
            .clock_route
            .as_ref()
            .map(|route| route.expected_active_source()),
    )? {
        // The owner still holds the clock lease and retires it normally. A
        // late clean rejection does not start a worker or retry another route.
        return Err(Error::Unsupported {
            reason: "the selected PCM IN alternate became unavailable before capture activation"
                .into(),
            offered: vec![],
        });
    }
    let error = Arc::new(Mutex::new(None));
    let stop = Arc::new(AtomicBool::new(false));
    let worker_buffer = buffer.clone();
    let worker_error = error.clone();
    let worker_stop = stop.clone();
    let worker_endpoint = endpoint.clone();
    let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
    let worker_context = context::current();
    let worker = thread::Builder::new()
        .name("usbdac-capture".into())
        .spawn(move || {
            let _owner_scope = worker_context.enter();
            // Explicit local drop order keeps the guard/FD alive through retiring
            // transfer Drops, including unwinding and a failed initial submission.
            let _session = session;
            let _device = device;
            let owner = owner;
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                run_capture(
                    &owner,
                    &worker_endpoint,
                    geometry,
                    &worker_buffer,
                    &worker_stop,
                    &ready_tx,
                )
            }))
            .unwrap_or_else(|_| Err(Error::Transfer("USB capture worker panicked".into())));
            if let Err(failure) = result {
                if failure == Error::DeviceLost {
                    events.emit_dac_or_hold(crate::DacEvent::DeviceLost);
                }
                *worker_error.lock() = Some(failure.clone());
                worker_buffer.lock().fail();
                let _ = ready_tx.try_send(Err(failure));
            } else {
                let mut buffer = worker_buffer.lock();
                if buffer.state() == crate::capture::CaptureState::Active {
                    buffer.stop();
                }
            }
        })
        .map_err(|e| Error::Transfer(format!("cannot start capture worker: {e}")))?;
    let mut handle = CaptureHandle {
        endpoint,
        presentation: CapturePresentation::Standalone(buffer),
        error,
        stop,
        worker: Some(worker),
        paired: false,
        retired: None,
        isolated_failure: None,
        capture_retired: None,
    };
    match ready_rx.recv() {
        Ok(Ok(())) => Ok(handle),
        Ok(Err(failure)) => {
            let _ = handle.stop();
            Err(failure)
        }
        Err(_) => {
            let _ = handle.stop();
            Err(Error::Transfer(
                "capture worker stopped before startup".into(),
            ))
        }
    }
}

/// Inspect immutable submission geometry before using descriptors or payload.
/// Device removal is recognized even when presentation has stopped or is busy.
pub(super) fn inspect_slot<'a>(
    slot: &'a IsoTransferSlot,
    status: i32,
    scratch: &mut [CapturePacket],
) -> Result<(&'a [u8], bool), Error> {
    if status != LIBUSB_TRANSFER_COMPLETED {
        return Err(if status == LIBUSB_TRANSFER_NO_DEVICE {
            Error::DeviceLost
        } else {
            Error::Transfer(format!(
                "capture transfer failed: {}",
                iso_transfer_status_label(status)
            ))
        });
    }
    let transfer = unsafe { slot.transfer.as_ref() };
    let data = unsafe { &*slot.user_data };
    if transfer.num_iso_packets != scratch.len() as i32
        || transfer.length < 0
        || transfer.length as usize != slot.submitted_bytes
        || slot.submitted_bytes > data.buffer.len()
        || (slot.submitted_endpoint & 0x80 != 0 && slot.submitted_bytes != data.buffer.len())
        || transfer.endpoint != slot.submitted_endpoint
        || transfer.buffer != data.buffer.as_ptr().cast_mut()
    {
        return Err(Error::Transfer("capture transfer geometry changed".into()));
    }
    let descriptors =
        unsafe { std::slice::from_raw_parts(transfer.iso_packet_desc.as_ptr(), scratch.len()) };
    for (destination, source) in scratch.iter_mut().zip(descriptors) {
        *destination = CapturePacket {
            requested_bytes: source.length as usize,
            actual_bytes: source.actual_length as usize,
            succeeded: source.status == LIBUSB_TRANSFER_COMPLETED,
        };
    }
    Ok((
        &data.buffer[..slot.submitted_bytes],
        descriptors
            .iter()
            .any(|p| p.status == LIBUSB_TRANSFER_NO_DEVICE),
    ))
}

pub(super) fn ingest_slot(
    slot: &IsoTransferSlot,
    status: i32,
    buffer: &mut CaptureBuffer,
    scratch: &mut [CapturePacket],
) -> Result<(), Error> {
    let (data, detached) = match inspect_slot(slot, status, scratch) {
        Ok(completion) => completion,
        Err(error) => {
            if status != LIBUSB_TRANSFER_COMPLETED {
                let _ = buffer.ingest_completion(&[], &[], &[], false, 0);
            } else {
                buffer.note_malformed_completion();
            }
            return Err(error);
        }
    };
    let result = buffer
        .ingest_completion(data, &slot.packet_sizes, scratch, true, 0)
        .map_err(|e| Error::Transfer(format!("capture completion: {e:?}")));
    if detached {
        Err(Error::DeviceLost)
    } else {
        result
    }
}

/// Keep retiring/rearming IN during feedforward startup without presenting
/// audio produced before the host's nominal rate has a verified OUT send.
pub(super) fn discard_unprimed_capture(
    slot: &IsoTransferSlot,
    status: i32,
    scratch: &mut [CapturePacket],
    frame_bytes: usize,
) -> (crate::capture::CaptureStats, Result<(), Error>) {
    let mut stats = crate::capture::CaptureStats::default();
    let result = (|| {
        let (data, detached) = inspect_slot(slot, status, scratch)?;
        if detached {
            return Err(Error::DeviceLost);
        }
        stats
            .record_completion(
                data.len(),
                &slot.packet_sizes,
                scratch,
                true,
                0,
                frame_bytes,
            )
            .map_err(|e| Error::Transfer(format!("capture startup completion: {e:?}")))
    })();
    stats.proof_suppressed_frames = stats.captured_frames;
    (stats, result)
}

/// Consume a complete startup URB without publishing its payload, including
/// the URB that satisfies the endpoint's declared lock promise. A failed or
/// malformed service interval breaks the contiguous proof without changing
/// capture's existing packet classification or typed detach behavior.
pub(super) fn discard_lock_delay_capture(
    slot: &IsoTransferSlot,
    status: i32,
    scratch: &mut [CapturePacket],
    frame_bytes: usize,
    service_us: u32,
    timing_allowed: bool,
    gate: &mut endpoint_lock::LockDelayGate,
) -> (crate::capture::CaptureStats, Result<(), Error>) {
    let mut stats = crate::capture::CaptureStats::default();
    let result = match inspect_slot(slot, status, scratch) {
        Ok((data, detached)) => {
            let result = stats
                .record_completion(
                    data.len(),
                    &slot.packet_sizes,
                    scratch,
                    true,
                    0,
                    frame_bytes,
                )
                .map_err(|e| Error::Transfer(format!("capture startup completion: {e:?}")));
            if detached {
                Err(Error::DeviceLost)
            } else {
                result
            }
        }
        Err(error) => {
            if status == LIBUSB_TRANSFER_COMPLETED {
                stats.malformed_completions = 1;
            } else {
                stats.failed_transfers = 1;
            }
            Err(error)
        }
    };
    let accepted = timing_allowed
        && result.is_ok()
        && stats.failed_packets == 0
        && stats.malformed_packets == 0
        && stats.malformed_completions == 0;
    gate.observe(
        accepted,
        u64::from(service_us).saturating_mul(scratch.len() as u64),
        stats.captured_frames,
    );
    // Pre-lock payload contributes to device position but is not presented.
    stats.proof_suppressed_frames = stats.captured_frames;
    (stats, result)
}

fn oldest_ready_capture(order: &VecDeque<usize>, slots: &[IsoTransferSlot]) -> Option<usize> {
    order
        .front()
        .copied()
        .filter(|index| slots[*index].completion_ready())
}

/// Prove libusb integer widths and total backing storage before class writes
/// or allocating a capture queue, including SSP payloads above u16::MAX.
pub(super) fn capture_queue_bytes(
    packet_bytes: usize,
    geometry: IsoQueueGeometry,
) -> Result<usize, Error> {
    let invalid = || Error::Unsupported {
        reason: "capture packet/queue geometry exceeds libusb or host allocation bounds".into(),
        offered: vec![],
    };
    if packet_bytes == 0
        || packet_bytes > u32::MAX as usize
        || geometry.packets_per_transfer == 0
        || geometry.packets_per_transfer > i32::MAX as usize
        || geometry.transfer_slots == 0
    {
        return Err(invalid());
    }
    let per_transfer = packet_bytes
        .checked_mul(geometry.packets_per_transfer)
        .ok_or_else(invalid)?;
    let total = per_transfer
        .checked_mul(geometry.transfer_slots)
        .ok_or_else(invalid)?;
    let packets = geometry
        .packets_per_transfer
        .checked_mul(geometry.transfer_slots)
        .ok_or_else(invalid)?;
    if packets >= crate::iso_packet_scheduler::PACKET_FLIGHT_LIMIT
        || geometry.packets_per_transfer > crate::iso_packet_scheduler::MAX_PACKETS_PER_TRANSFER
        || per_transfer > i32::MAX as usize
        || total > isize::MAX as usize
    {
        return Err(invalid());
    }
    Ok(per_transfer)
}

/// Two bounded periodic OUT URBs share the data IN worker's event owner and
/// callback retirement. No second event thread or playback clock is involved.
pub(super) struct FeedforwardPool {
    pub(super) slots: Vec<IsoTransferSlot>,
    pipe: adc3::FeedforwardEndpoint,
    nominal_word: Vec<u8>,
    packets: usize,
    cadence: FeedforwardCadence,
    scratch: Vec<CapturePacket>,
    first_valid_send: Option<Instant>,
    requires_post_proof_submission: bool,
    deadline: Duration,
}
/// usbfs schedules every descriptor service slot using bInterval. UAC1's
/// bRefresh selects which of those slots carries a word; intervening slots
/// carry zero-length transactions (USB 2.0 sections 5.6 and 5.12.4.2).
#[derive(Debug, Clone, Copy)]
struct FeedforwardCadence {
    period: usize,
    phase: usize,
}
impl FeedforwardCadence {
    fn new(refresh: Option<u8>) -> Result<Self, String> {
        let period = match refresh {
            None => 1,
            Some(refresh @ 1..=9) => 1usize << refresh,
            Some(_) => return Err("invalid capture feedforward refresh".into()),
        };
        Ok(Self { period, phase: 0 })
    }
    fn plan(self, packets: usize, bytes: usize) -> (Vec<usize>, Self) {
        let lengths = (0..packets)
            .map(|offset| {
                if (self.phase + offset) % self.period == 0 {
                    bytes
                } else {
                    0
                }
            })
            .collect();
        (
            lengths,
            Self {
                phase: (self.phase + packets) % self.period,
                ..self
            },
        )
    }
}
impl FeedforwardPool {
    pub(super) fn for_endpoint(endpoint: &CaptureEndpoint) -> Result<Option<Self>, String> {
        let Some(pipe) = endpoint.feedforward() else {
            return Ok(None);
        };
        let nominal = pipe.nominal(endpoint.format.sample_rate())?;
        if nominal.len() != pipe.bytes {
            return Err("capture feedforward word width mismatch".into());
        }
        // Target 8 ms per URB, capped at 32 service slots.
        // bInterval schedules the transfers; it never rescales the rate value.
        let packets = 8_000usize
            .div_ceil(pipe.service_us as usize)
            .clamp(1, crate::iso_packet_scheduler::MAX_PACKETS_PER_TRANSFER);
        let cadence = FeedforwardCadence::new(pipe.refresh)?;
        let nominal_word = nominal;
        let mut slots = Vec::with_capacity(2);
        for index in 0..2 {
            slots.push(IsoTransferSlot::new(index, packets, packets * pipe.bytes)?);
        }
        Ok(Some(Self {
            slots,
            pipe,
            nominal_word,
            packets,
            cadence,
            scratch: vec![
                CapturePacket {
                    requested_bytes: 0,
                    actual_bytes: 0,
                    succeeded: false
                };
                packets
            ],
            first_valid_send: None,
            requires_post_proof_submission: endpoint.standard_feedforward.is_some(),
            deadline: Duration::from_millis(100).max(Duration::from_micros(
                u64::from(pipe.service_us) * packets as u64 * 4,
            )),
        }))
    }
    fn submit(&mut self, index: usize, handle: &DeviceHandle<Context>) -> Result<(), Error> {
        let (packet_sizes, next_cadence) = self.cadence.plan(self.packets, self.pipe.bytes);
        let transfer_buffer = packet_sizes
            .iter()
            .filter(|&&bytes| bytes != 0)
            .flat_map(|_| self.nominal_word.iter().copied())
            .collect();
        self.slots[index]
            .submit_with_timeout(
                handle,
                self.pipe.address,
                IsoTransferPayload {
                    packet_sizes,
                    packet_samples: vec![],
                    transfer_buffer,
                    underrun: false,
                    held: false,
                    frame_count: 0,
                    music_frames: 0,
                    epoch: 0,
                },
                self.deadline.as_millis().min(u128::from(u32::MAX)) as u32,
            )
            .map_err(|e| {
                if crate::error::is_device_gone(&e) {
                    Error::DeviceLost
                } else {
                    Error::Transfer(format!("capture feedforward submission: {e}"))
                }
            })?;
        self.cadence = next_cadence;
        Ok(())
    }
    pub(super) fn start(
        &mut self,
        handle: &DeviceHandle<Context>,
        stop: &AtomicBool,
    ) -> Result<(), Error> {
        for index in 0..self.slots.len() {
            if usb_stop_requested(stop) {
                break;
            }
            self.submit(index, handle)?;
        }
        Ok(())
    }
    pub(super) fn primed(&self) -> bool {
        self.first_valid_send.is_some()
    }
    pub(super) fn accepts_capture(&self, completed: Instant) -> bool {
        self.first_valid_send
            .is_some_and(|first| completed >= first)
    }
    pub(super) fn accepts_capture_transfer(
        &self,
        completed: Instant,
        submitted_at: Option<Instant>,
    ) -> bool {
        self.accepts_capture(completed)
            && (!self.requires_post_proof_submission
                || submitted_at.is_some_and(|submitted| self.accepts_capture(submitted)))
    }
    pub(super) fn pump(
        &mut self,
        handle: &DeviceHandle<Context>,
        stop: &AtomicBool,
    ) -> Result<(), Error> {
        if usb_stop_requested(stop) {
            return Ok(());
        }
        for index in 0..self.slots.len() {
            if let Some((status, completed, _)) = self.slots[index].take_completion() {
                let result = validate_feedforward_slot(
                    &self.slots[index],
                    status,
                    completed,
                    self.deadline,
                    self.pipe.bytes,
                    &self.nominal_word,
                    &mut self.scratch,
                );
                self.slots[index].mark_completed();
                if result? {
                    self.first_valid_send = Some(
                        self.first_valid_send
                            .map_or(completed, |first| first.min(completed)),
                    );
                }
                self.submit(index, handle)?;
            } else if self.slots[index].in_flight
                && self.slots[index]
                    .submitted_at
                    .is_some_and(|start| start.elapsed() >= self.deadline)
            {
                return Err(Error::Transfer(
                    "capture feedforward deadline missed".into(),
                ));
            }
        }
        Ok(())
    }
    pub(super) fn cancel(&mut self) {
        for slot in &mut self.slots {
            slot.cancel();
        }
    }
    pub(super) fn drain(&mut self, context: &Context) {
        self.cancel();
        drain_iso_transfer_slots(context, &mut self.slots);
    }
}
impl Drop for FeedforwardPool {
    fn drop(&mut self) {
        self.cancel();
    }
}
fn validate_feedforward_slot(
    slot: &IsoTransferSlot,
    status: i32,
    completed: Instant,
    deadline: Duration,
    bytes: usize,
    nominal_word: &[u8],
    scratch: &mut [CapturePacket],
) -> Result<bool, Error> {
    let (data, detached) = inspect_slot(slot, status, scratch).map_err(|error| match error {
        Error::DeviceLost => Error::DeviceLost,
        other => Error::Transfer(format!("capture feedforward completion: {other}")),
    })?;
    if detached {
        return Err(Error::DeviceLost);
    }
    if !slot
        .submitted_at
        .is_some_and(|start| completed.saturating_duration_since(start) < deadline)
    {
        return Err(Error::Transfer(
            "capture feedforward deadline missed".into(),
        ));
    }
    let sent_word = validate_feedforward_packets(bytes, &slot.packet_sizes, scratch)?;
    let expected_bytes: usize = slot.packet_sizes.iter().sum();
    if nominal_word.len() != bytes
        || data.len() != expected_bytes
        || data.chunks(bytes.max(1)).any(|word| word != nominal_word)
    {
        return Err(Error::Transfer(
            "capture feedforward payload changed".into(),
        ));
    }
    Ok(sent_word)
}
fn validate_feedforward_packets(
    bytes: usize,
    planned: &[usize],
    packets: &[CapturePacket],
) -> Result<bool, Error> {
    if bytes == 0
        || packets.is_empty()
        || planned.len() != packets.len()
        || planned.iter().zip(packets).any(|(&expected, packet)| {
            (expected != 0 && expected != bytes)
                || !packet.succeeded
                || packet.requested_bytes != expected
                || packet.actual_bytes != expected
        })
    {
        return Err(Error::Transfer(
            "capture feedforward short or failed isochronous packet".into(),
        ));
    }
    Ok(planned.iter().any(|&length| length != 0))
}

/// Roland capture has no application playback provider. Service the proven
/// companion OUT with nominal digital silence under this same event owner.
/// Duplex never constructs this pool: its real OUT already owns the endpoint.
struct NominalCapturePool {
    slots: Vec<IsoTransferSlot>,
    profile: vendor_pcm::VendorPcm,
    scheduler: IsoPacketScheduler,
    acceptance: PacketAcceptance,
    first_valid: Option<Instant>,
    latest_valid: Option<Instant>,
    deadline: Duration,
}
impl NominalCapturePool {
    fn for_endpoint(endpoint: &CaptureEndpoint) -> Result<Option<Self>, String> {
        let Some(profile) = endpoint
            .vendor_pcm
            .filter(|profile| profile.requires_output_for_capture && !profile.rate_control)
        else {
            return Ok(None);
        };
        let geometry = IsoQueueGeometry::for_config(profile.output.service_us, &settings().config);
        let stride =
            usize::from(profile.output.channels) * usize::from(profile.output.subslot_bytes);
        let transfer_bytes = profile
            .output
            .max_packet_bytes
            .checked_mul(geometry.packets_per_transfer)
            .ok_or("vendor capture companion allocation overflow")?;
        let mut slots = Vec::with_capacity(geometry.transfer_slots);
        for index in 0..geometry.transfer_slots {
            slots.push(IsoTransferSlot::new(
                index,
                geometry.packets_per_transfer,
                transfer_bytes,
            )?);
        }
        Ok(Some(Self {
            slots,
            profile,
            scheduler: IsoPacketScheduler::with_geometry(
                endpoint.format.sample_rate(),
                stride,
                profile.output.service_us,
                geometry,
            ),
            acceptance: PacketAcceptance::new(
                endpoint.format.sample_rate(),
                profile.output.service_us,
                stride,
            ),
            first_valid: None,
            latest_valid: None,
            deadline: Duration::from_millis(100).max(Duration::from_micros(
                u64::from(profile.output.service_us) * geometry.packets_in_flight() as u64 * 4,
            )),
        }))
    }
    fn submit(&mut self, index: usize, handle: &DeviceHandle<Context>) -> Result<(), Error> {
        let stride = usize::from(self.profile.output.channels)
            * usize::from(self.profile.output.subslot_bytes);
        let payload = output::nominal_pcm_silence(
            &mut self.scheduler,
            self.profile.output.max_packet_bytes,
            stride,
            0,
        )
        .map_err(Error::Transfer)?;
        self.slots[index]
            .submit_with_timeout(
                handle,
                self.profile.output.address,
                payload,
                self.deadline.as_millis().min(u128::from(u32::MAX)) as u32,
            )
            .map_err(|error| {
                if crate::error::is_device_gone(&error) {
                    Error::DeviceLost
                } else {
                    Error::Transfer(error)
                }
            })
    }
    fn start(&mut self, handle: &DeviceHandle<Context>, stop: &AtomicBool) -> Result<(), Error> {
        for index in 0..self.slots.len() {
            if usb_stop_requested(stop) {
                break;
            }
            self.submit(index, handle)?;
        }
        Ok(())
    }
    fn pump(&mut self, handle: &DeviceHandle<Context>, stop: &AtomicBool) -> Result<(), Error> {
        if usb_stop_requested(stop) {
            return Ok(());
        }
        for index in 0..self.slots.len() {
            // Bound missing callbacks before retiring: successful late completions
            // cannot revive a companion that has stopped providing cadence.
            if self.slots[index].in_flight
                && self.slots[index]
                    .submitted_at
                    .is_some_and(|start| start.elapsed() >= self.deadline)
            {
                return Err(Error::Transfer(
                    "vendor capture companion OUT deadline missed".into(),
                ));
            }
            if let Some(completed) =
                output::retire_bootstrap_completion(&mut self.slots[index], self.acceptance)?
            {
                self.first_valid = Some(
                    self.first_valid
                        .map_or(completed, |first| first.min(completed)),
                );
                self.latest_valid = Some(
                    self.latest_valid
                        .map_or(completed, |latest| latest.max(completed)),
                );
                self.submit(index, handle)?;
            }
        }
        if self
            .latest_valid
            .is_some_and(|latest| latest.elapsed() >= self.deadline)
        {
            return Err(Error::Transfer(
                "vendor capture companion OUT cadence expired".into(),
            ));
        }
        Ok(())
    }
    fn primed(&self) -> bool {
        self.first_valid.is_some()
    }
    fn accepts_capture(&self, completed: Instant) -> bool {
        self.first_valid.is_some_and(|first| completed >= first)
            && self
                .latest_valid
                .is_some_and(|latest| completed.saturating_duration_since(latest) < self.deadline)
    }
}
impl Drop for NominalCapturePool {
    fn drop(&mut self) {
        for slot in &mut self.slots {
            slot.cancel();
        }
    }
}

/// Keep the USB owner borrowed until every callback has retired. Cancel every
/// endpoint first, including on partial startup and unwinding, before pumping.
struct CaptureTransferBatch<'a> {
    context: &'a Context,
    slots: Vec<IsoTransferSlot>,
    companion: Option<NominalCapturePool>,
    feedforward: Option<FeedforwardPool>,
}
impl Drop for CaptureTransferBatch<'_> {
    fn drop(&mut self) {
        for slot in &mut self.slots {
            slot.cancel();
        }
        if let Some(pool) = self.companion.as_mut() {
            for slot in &mut pool.slots {
                slot.cancel();
            }
        }
        if let Some(pool) = self.feedforward.as_mut() {
            pool.cancel();
        }
        output::retire_cancelled_iso_transfer_slots(self.context, &mut self.slots);
        if let Some(pool) = self.companion.as_mut() {
            output::retire_cancelled_iso_transfer_slots(self.context, &mut pool.slots);
        }
        if let Some(pool) = self.feedforward.as_mut() {
            output::retire_cancelled_iso_transfer_slots(self.context, &mut pool.slots);
        }
    }
}

/// Qualify programmed vendor capture without publishing pre-rate audio. Service
/// time comes from validated whole-frame packets, never callback wall spacing.
struct VendorCaptureRateGate {
    window: implicit::VendorRateWindow,
    pattern: Vec<usize>,
    started: Instant,
    latest: Option<Instant>,
    startup: Duration,
    stale: Duration,
    qualified: bool,
    ever_qualified: bool,
    last_proof: Option<Instant>,
    bad_windows: u8,
}
impl VendorCaptureRateGate {
    fn new(endpoint: &CaptureEndpoint, geometry: IsoQueueGeometry) -> Self {
        let queue_time = Duration::from_micros(
            u64::from(endpoint.service_us) * geometry.packets_in_flight() as u64 * 4,
        );
        Self {
            window: implicit::VendorRateWindow::new(endpoint.format.sample_rate()),
            pattern: vec![0; geometry.packets_per_transfer],
            started: Instant::now(),
            latest: None,
            startup: Duration::from_millis(500).max(queue_time),
            stale: Duration::from_millis(100).max(queue_time),
            qualified: false,
            ever_qualified: false,
            last_proof: None,
            bad_windows: 0,
        }
    }
    fn check(&self, now: Instant) -> Result<(), Error> {
        if (!self.ever_qualified && now.saturating_duration_since(self.started) >= self.startup)
            || self
                .last_proof
                .is_some_and(|proof| now.saturating_duration_since(proof) >= self.startup)
            || self
                .latest
                .is_some_and(|latest| now.saturating_duration_since(latest) >= self.stale)
        {
            return Err(Error::Transfer(
                "vendor capture rate qualification or cadence deadline missed".into(),
            ));
        }
        Ok(())
    }
    fn observe(
        &mut self,
        endpoint: &CaptureEndpoint,
        slot: &IsoTransferSlot,
        status: i32,
        completed: Instant,
        scratch: &mut [CapturePacket],
    ) -> Result<bool, Error> {
        self.check(completed)?;
        let (_, detached) = inspect_slot(slot, status, scratch)?;
        if detached {
            return Err(Error::DeviceLost);
        }
        let stride = endpoint.format.bytes_per_frame();
        let valid = implicit::capture_pattern(
            scratch.iter().map(|packet| {
                (
                    if packet.succeeded {
                        LIBUSB_TRANSFER_COMPLETED
                    } else {
                        -1
                    },
                    packet.actual_bytes,
                    packet.requested_bytes,
                )
            }),
            stride,
            stride,
            endpoint.packet_bytes,
            &mut self.pattern,
        )
        .unwrap_or(false);
        if !valid {
            self.window.reset();
            self.qualified = false;
            return Ok(false);
        }
        self.latest = Some(
            self.latest
                .map_or(completed, |latest| latest.max(completed)),
        );
        let publish = self.qualified;
        let frames = self
            .pattern
            .iter()
            .map(|bytes| (bytes / stride) as u64)
            .sum();
        let service_us = u64::from(endpoint.service_us) * self.pattern.len() as u64;
        if let Some(valid_rate) = self.window.observe(frames, service_us) {
            if valid_rate {
                self.bad_windows = 0;
                self.qualified = true;
                self.ever_qualified = true;
                self.last_proof = Some(completed);
            } else {
                self.qualified = false;
                self.bad_windows += 1;
                if self.bad_windows >= 2 {
                    return Err(Error::ClockUnverified {
                        requested_hz: endpoint.format.sample_rate(),
                        reported_hz: None,
                    });
                }
                return Ok(false);
            }
        }
        Ok(publish)
    }
}

fn run_capture(
    owner: &CaptureUsbOwner,
    endpoint: &CaptureEndpoint,
    geometry: IsoQueueGeometry,
    buffer: &Mutex<CaptureBuffer>,
    stop: &AtomicBool,
    ready: &std::sync::mpsc::SyncSender<Result<(), Error>>,
) -> Result<(), Error> {
    let delay = if endpoint.vendor_pcm.is_some() {
        None
    } else {
        endpoint_lock::selected_delay(
            owner.claimed(),
            endpoint.interface,
            endpoint.alternate,
            endpoint.address,
            true,
        )
        .or_control()?
    };
    let mut lock_delay = endpoint_lock::LockDelayGate::new(delay);
    let mut host_rate = capture_rate::CaptureRateProof::for_endpoint(endpoint, geometry, delay);
    let targets: Vec<_> = status_target(owner.claimed(), endpoint)?
        .into_iter()
        .collect();
    // Declared after the owner and before data transfers: all failure/unwind
    // paths retire the status request before resetting an alternate or handle.
    let mut audio_status = audio_status::AudioStatus::start(owner.claimed(), &targets)?;
    let transfer_bytes = capture_queue_bytes(endpoint.packet_bytes, geometry)?;
    let mut slots = Vec::with_capacity(geometry.transfer_slots);
    let mut order = VecDeque::with_capacity(geometry.transfer_slots);
    let mut reordered_seen = vec![false; geometry.transfer_slots];
    let mut scratch = vec![
        CapturePacket {
            requested_bytes: 0,
            actual_bytes: 0,
            succeeded: false
        };
        geometry.packets_per_transfer
    ];
    for index in 0..geometry.transfer_slots {
        slots.push(
            IsoTransferSlot::new(index, geometry.packets_per_transfer, transfer_bytes)
                .map_err(Error::Transfer)?,
        );
    }
    let companion = NominalCapturePool::for_endpoint(endpoint).map_err(Error::Transfer)?;
    let feedforward = FeedforwardPool::for_endpoint(endpoint).map_err(Error::Transfer)?;
    let mut batch = CaptureTransferBatch {
        context: &owner.claimed().context,
        slots,
        companion,
        feedforward,
    };
    let CaptureTransferBatch {
        slots,
        companion,
        feedforward,
        ..
    } = &mut batch;
    if let Some(pool) = companion.as_mut() {
        pool.start(&owner.claimed().handle, stop)?;
    }
    if let Some(pool) = feedforward.as_mut() {
        pool.start(&owner.claimed().handle, stop)?;
    }
    if usb_stop_requested(stop) {
        return Ok(());
    }
    audio_status.arm_endpoint(owner.claimed(), endpoint.address)?;
    for (index, slot) in slots.iter_mut().enumerate() {
        if usb_stop_requested(stop) {
            return Ok(());
        }
        slot.submit_capture(
            &owner.claimed().handle,
            endpoint.address,
            endpoint.packet_bytes,
            None,
        )
        .map_err(|e| {
            if crate::error::is_device_gone(&e) {
                Error::DeviceLost
            } else {
                Error::Transfer(e)
            }
        })?;
        order.push_back(index);
    }
    let mut vendor_rate = endpoint
        .vendor_pcm
        .as_ref()
        .filter(|profile| profile.rate_control)
        .map(|_| VendorCaptureRateGate::new(endpoint, geometry));
    let input_started = Instant::now();
    let mut latest_input: Option<Instant> = None;
    let mut announced = false;
    if feedforward.is_none() && companion.is_none() && vendor_rate.is_none() && host_rate.is_none()
    {
        let _ = ready.send(Ok(()));
        announced = true;
    }
    while !usb_stop_requested(stop) {
        owner
            .claimed()
            .context
            .handle_events(Some(Duration::from_millis(
                if audio_status.is_revalidating() {
                    1
                } else {
                    10
                },
            )))
            .map_err(|e| {
                if e == rusb::Error::NoDevice {
                    Error::DeviceLost
                } else {
                    Error::Transfer(format!("capture event pump: {e}"))
                }
            })?;
        if usb_stop_requested(stop) {
            break;
        }
        audio_status.poll(owner.claimed())?;
        if let Some(pool) = feedforward.as_mut() {
            pool.pump(&owner.claimed().handle, stop)?;
        }
        if let Some(pool) = companion.as_mut() {
            pool.pump(&owner.claimed().handle, stop)?;
        }
        if !announced
            && companion.is_none()
            && vendor_rate.is_none()
            && host_rate.is_none()
            && feedforward.as_ref().map_or(true, |pool| pool.primed())
        {
            let _ = ready.send(Ok(()));
            announced = true;
        }
        if let Some(&head) = order.front() {
            if !slots[head].completion_ready() {
                for &index in order.iter().skip(1) {
                    if slots[index].completion_ready() && !reordered_seen[index] {
                        reordered_seen[index] = true;
                        buffer.lock().note_reordered_completion();
                    }
                }
            }
        }
        // Callback order is not sample order. Hold later completed buffers
        // until the oldest submitted transfer comes back, then re-arm in order.
        while let Some(index) = oldest_ready_capture(&order, slots) {
            if usb_stop_requested(stop) {
                break;
            }
            let Some((status, completed, _)) = slots[index].take_completion() else {
                break;
            };
            order.pop_front();
            reordered_seen[index] = false;
            let mut queue = buffer.lock();
            if usb_stop_requested(stop) {
                slots[index].mark_completed();
                break;
            }
            let vendor_allowed = if let Some(gate) = vendor_rate.as_mut() {
                match gate.observe(endpoint, &slots[index], status, completed, &mut scratch) {
                    Ok(allowed) => allowed,
                    Err(error) => {
                        // take_completion consumed this callback. Retire its
                        // in-flight ownership before cancellation can drain.
                        drop(queue);
                        slots[index].mark_completed();
                        return Err(error);
                    }
                }
            } else {
                true
            };
            let capture_allowed = vendor_allowed
                && feedforward.as_ref().map_or(true, |pool| {
                    pool.accepts_capture_transfer(completed, slots[index].submitted_at)
                })
                && companion
                    .as_ref()
                    .map_or(true, |pool| pool.accepts_capture(completed));
            let captured_before = companion.as_ref().map(|_| queue.stats().captured_frames);
            let lock_pending = lock_delay.pending();
            let host_allowed = if !lock_pending && capture_allowed {
                if let Some(gate) = host_rate.as_mut() {
                    match gate.observe(endpoint, &slots[index], status, completed, &mut scratch) {
                        Ok(allowed) => allowed,
                        Err(error) => {
                            drop(queue);
                            slots[index].mark_completed();
                            return Err(error);
                        }
                    }
                } else {
                    true
                }
            } else {
                host_rate.is_none()
            };
            let result = if lock_pending {
                let timing_allowed = capture_allowed
                    && slots[index].submitted_at.is_some_and(|submitted| {
                        feedforward
                            .as_ref()
                            .map_or(true, |pool| pool.accepts_capture(submitted))
                            && companion
                                .as_ref()
                                .map_or(true, |pool| pool.accepts_capture(submitted))
                    });
                let (stats, result) = discard_lock_delay_capture(
                    &slots[index],
                    status,
                    &mut scratch,
                    endpoint.format.bytes_per_frame(),
                    endpoint.service_us,
                    timing_allowed,
                    &mut lock_delay,
                );
                queue.merge_stats(stats);
                result
            } else if capture_allowed && host_allowed {
                ingest_slot(&slots[index], status, &mut queue, &mut scratch)
            } else {
                let (stats, result) = discard_unprimed_capture(
                    &slots[index],
                    status,
                    &mut scratch,
                    endpoint.format.bytes_per_frame(),
                );
                queue.merge_stats(stats);
                result
            };
            let captured_input = capture_allowed
                && host_allowed
                && !lock_pending
                && captured_before.is_some_and(|before| queue.stats().captured_frames > before);
            drop(queue);
            slots[index].mark_completed();
            result?;
            if captured_input {
                latest_input = Some(latest_input.map_or(completed, |latest| latest.max(completed)));
            }
            if !usb_stop_requested(stop) {
                slots[index]
                    .submit_capture(
                        &owner.claimed().handle,
                        endpoint.address,
                        endpoint.packet_bytes,
                        None,
                    )
                    .map_err(|e| {
                        if crate::error::is_device_gone(&e) {
                            Error::DeviceLost
                        } else {
                            Error::Transfer(e)
                        }
                    })?;
                order.push_back(index);
            }
        }
        if let Some(gate) = host_rate.as_ref() {
            gate.check(Instant::now())?;
            if !announced && gate.qualified() {
                let _ = ready.send(Ok(()));
                announced = true;
            }
        }
        if let Some(gate) = vendor_rate.as_ref() {
            gate.check(Instant::now())?;
            if !announced && gate.qualified {
                let _ = ready.send(Ok(()));
                announced = true;
            }
        }
        if let Some(pool) = companion.as_ref() {
            if latest_input.unwrap_or(input_started).elapsed() >= pool.deadline {
                return Err(Error::Transfer(
                    "vendor capture IN cadence deadline missed".into(),
                ));
            }
            if !announced && pool.primed() && latest_input.is_some() {
                let _ = ready.send(Ok(()));
                announced = true;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn uac1_topology_resolves_shared_inputs_and_rejects_ambiguity() {
        use super::*;
        let input = [12, 0x24, 2, 1, 0, 0, 0, 2, 3, 0, 0, 0];
        let feature = [7, 0x24, 6, 2, 1, 0, 0];
        let selector = [8, 0x24, 5, 3, 2, 1, 2, 0];
        let descriptors: Vec<&[u8]> = vec![&input, &feature, &selector];
        let mut resolver = Uac1ClusterResolver::new(&descriptors, 2);
        assert!(resolver.resolve(3, 0).is_ok());
        assert!(resolver.resolve(3, 0).is_ok());
        let duplicates: Vec<&[u8]> = vec![&input, &input, &selector];
        assert!(Uac1ClusterResolver::new(&duplicates, 2)
            .resolve(3, 0)
            .is_err());
        let cycle = [7, 0x24, 6, 2, 2, 0, 0];
        let cyclic: Vec<&[u8]> = vec![&cycle];
        assert!(Uac1ClusterResolver::new(&cyclic, 2).resolve(2, 0).is_err());
    }
    #[test]
    fn capture_activation_requires_the_exact_terminal_wire_rate() {
        assert!(super::verify_capture_wire_rate(96_000, Some(96_000)).is_ok());
        // A 48 kHz root under a 2:1 path must not be reported as wire timing.
        assert!(super::verify_capture_wire_rate(96_000, Some(48_000)).is_err());
        assert!(super::verify_capture_wire_rate(96_000, None).is_err());
    }
    use super::*;
    #[test]
    fn vendor_capture_preserves_fixed_packed_pcm_and_unnamed_channel_order() {
        for channels in [8u16, 12] {
            let endpoint = vendor_pcm::VendorEndpoint {
                address: 0x82,
                interval: 1,
                service_us: 125,
                max_packet_bytes: usize::from(channels) * 3 * 7,
                channels,
                subslot_bytes: 3,
            };
            let profile = vendor_pcm::VendorPcm {
                vendor_id: 0x1686,
                product_id: 0xf00c,
                configuration_number: 1,
                interface_number: 0,
                alt_setting: 1,
                input_interface_number: 0,
                input_alt_setting: 1,
                requires_output_for_capture: false,
                input_carries_pcm: true,
                rates: &[48_000],
                rate_control: false,
                speed: Speed::High,
                output: vendor_pcm::VendorEndpoint {
                    address: 1,
                    ..endpoint
                },
                input: endpoint,
            };
            let capture = vendor_capture_endpoint(1, profile, profile.rates[0]).unwrap();
            assert_eq!(capture.format.sample_rate(), 48_000);
            assert_eq!(capture.format.valid_bits(), 24);
            assert_eq!(capture.format.subslot_bytes(), 3);
            assert_eq!(capture.format.channels(), channels);
            assert_eq!(capture.format.bytes_per_frame(), usize::from(channels) * 3);
            assert!(capture
                .format
                .channel_map()
                .positions()
                .iter()
                .enumerate()
                .all(|(index, position)| *position
                    == crate::capture::ChannelPosition::Unspecified {
                        index: index as u16
                    }));
            assert_eq!(capture.control, capture.interface);
            assert!(capture.control_descriptors.is_empty());
            assert!(!capture.sampling_control);
            assert!(capture.fixed_rate);
        }
    }

    fn roland_capture_profile() -> vendor_pcm::VendorPcm {
        let output = vendor_pcm::VendorEndpoint {
            address: 0x05,
            interval: 1,
            service_us: 125,
            max_packet_bytes: 7 * 4 * 4,
            channels: 4,
            subslot_bytes: 4,
        };
        vendor_pcm::VendorPcm {
            vendor_id: 0x0582,
            product_id: 0x012f,
            configuration_number: 1,
            interface_number: 0,
            alt_setting: 1,
            input_interface_number: 1,
            input_alt_setting: 1,
            requires_output_for_capture: true,
            input_carries_pcm: true,
            rates: &[44_100],
            rate_control: false,
            speed: Speed::High,
            output,
            input: vendor_pcm::VendorEndpoint {
                address: 0x85,
                ..output
            },
        }
    }
    #[test]
    fn timing_only_vendor_input_is_never_a_capture_candidate() {
        let mut profile = roland_capture_profile();
        profile.input_carries_pcm = false;
        assert!(require_vendor_capture_pcm(&profile)
            .unwrap_err()
            .contains("capture is unsupported"));
        assert!(vendor_capture_candidates(1, profile, profile.rates[0])
            .unwrap()
            .is_empty());
        assert!(vendor_capture_endpoint(1, profile, profile.rates[0]).is_err());
    }

    #[test]
    fn ordinary_vendor_input_remains_capture_pcm_at_its_fixed_rate() {
        let profile = roland_capture_profile();
        assert!(require_vendor_capture_pcm(&profile).is_ok());
        assert_eq!(
            vendor_capture_candidates(1, profile, profile.rates[0])
                .unwrap()
                .len(),
            1
        );
        assert!(vendor_capture_candidates(1, profile, 48_000)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn roland_capture_requires_exact_pair_and_preserves_separate_input_format() {
        let profile = roland_capture_profile();
        assert!(profile.claims_match(&[0, 1]));
        assert!(profile.claims_match(&[1, 0]));
        for claims in [&[1][..], &[0][..], &[0, 1, 1][..], &[0, 1, 2][..]] {
            assert!(!profile.claims_match(claims));
        }
        let endpoint = vendor_capture_endpoint(1, profile, profile.rates[0]).unwrap();
        assert_eq!(
            (endpoint.interface, endpoint.alternate, endpoint.address),
            (1, 1, 0x85)
        );
        assert_eq!(
            (
                endpoint.format.sample_rate(),
                endpoint.format.valid_bits(),
                endpoint.format.subslot_bytes()
            ),
            (44_100, 32, 4)
        );
        assert!(endpoint.control_descriptors.is_empty());
        assert!(!endpoint.sampling_control);
    }
    fn programmed_capture_endpoint() -> CaptureEndpoint {
        let mut profile = roland_capture_profile();
        profile.vendor_id = 0x2b73;
        profile.product_id = 0x001a;
        profile.rates = &[44_100, 48_000, 96_000];
        profile.rate_control = true;
        profile.requires_output_for_capture = false;
        profile.output.service_us = 1_000;
        profile.output.max_packet_bytes = 128 * 16;
        profile.input.service_us = 1_000;
        profile.input.max_packet_bytes = 128 * 16;
        vendor_capture_endpoint(1, profile, 48_000).unwrap()
    }
    #[test]
    fn isolated_capture_waits_for_each_callback_without_stopping_pair() {
        let endpoint = programmed_capture_endpoint();
        let attachment = Arc::new(duplex::Attachment::new(endpoint, false, 32).unwrap());
        let stop = Arc::new(AtomicBool::new(false));
        let mut reader = attachment.reader(stop.clone());
        let mut pool = duplex::InputPool::new(attachment.clone()).unwrap();
        pool.test_allow_isolation();
        assert!(pool.slots.len() >= 2);
        assert!(pool.isolate_status_failure(
            reader.endpoint.control,
            Error::Transfer("capture fault".into())
        ));
        // These slots simulate callbacks already outstanding when cancellation
        // was requested; no native transfer was submitted by this test.
        for slot in &mut pool.slots[..2] {
            slot.in_flight = true;
            unsafe { &(*slot.user_data).completion }.reset();
        }
        let completion = unsafe { &(*pool.slots[0].user_data).completion };
        *completion.status.lock().unwrap() = Some(LIBUSB_TRANSFER_CANCELLED);
        completion.publish_for_test();
        pool.poll_retirement().unwrap();
        assert!(!reader
            .capture_retired
            .as_ref()
            .unwrap()
            .load(Ordering::Acquire));
        assert!(!reader.retired.as_ref().unwrap().load(Ordering::Acquire));
        assert!(!stop.load(Ordering::Acquire));
        let completion = unsafe { &(*pool.slots[1].user_data).completion };
        *completion.status.lock().unwrap() = Some(LIBUSB_TRANSFER_CANCELLED);
        completion.publish_for_test();
        pool.poll_retirement().unwrap();
        assert!(reader
            .capture_retired
            .as_ref()
            .unwrap()
            .load(Ordering::Acquire));
        assert!(!reader.retired.as_ref().unwrap().load(Ordering::Acquire));
        reader.stop().unwrap();
        assert!(!stop.load(Ordering::Acquire));
    }

    #[test]
    fn programmed_capture_uses_requested_rate_without_a_companion() {
        let endpoint = programmed_capture_endpoint();
        assert_eq!(endpoint.format.sample_rate(), 48_000);
        assert!(!endpoint.fixed_rate);
        assert!(NominalCapturePool::for_endpoint(&endpoint)
            .unwrap()
            .is_none());
        let profile = endpoint.vendor_pcm.unwrap();
        assert_eq!(
            vendor_capture_candidates(1, profile, 96_000).unwrap()[0]
                .format
                .sample_rate(),
            96_000
        );
        assert!(vendor_capture_candidates(1, profile, 88_200)
            .unwrap()
            .is_empty());
    }
    #[test]
    fn programmed_capture_keeps_the_complete_proof_window_unpublished() {
        let endpoint = programmed_capture_endpoint();
        let geometry = IsoQueueGeometry::for_stream(1_000, 8, 4);
        let mut gate = VendorCaptureRateGate::new(&endpoint, geometry);
        gate.pattern = vec![0];
        let slot = completed_slot(0, &[0; 48 * 16]);
        let mut scratch = [CapturePacket {
            requested_bytes: 0,
            actual_bytes: 0,
            succeeded: false,
        }];
        let start = gate.started;
        for millis in 1..=100 {
            assert!(!gate
                .observe(
                    &endpoint,
                    &slot,
                    LIBUSB_TRANSFER_COMPLETED,
                    start + Duration::from_millis(millis),
                    &mut scratch
                )
                .unwrap());
        }
        assert!(gate.qualified);
        assert!(gate
            .observe(
                &endpoint,
                &slot,
                LIBUSB_TRANSFER_COMPLETED,
                start + Duration::from_millis(101),
                &mut scratch
            )
            .unwrap());
        assert!(gate.check(start + Duration::from_millis(601)).is_err());
    }
    #[test]
    fn programmed_capture_requires_a_contiguous_whole_frame_window() {
        let endpoint = programmed_capture_endpoint();
        let geometry = IsoQueueGeometry::for_stream(1_000, 8, 4);
        let mut gate = VendorCaptureRateGate::new(&endpoint, geometry);
        gate.pattern = vec![0];
        let mut slot = completed_slot(0, &[0; 48 * 16]);
        let mut scratch = [CapturePacket {
            requested_bytes: 0,
            actual_bytes: 0,
            succeeded: false,
        }];
        let start = gate.started;
        for millis in 1..50 {
            assert!(!gate
                .observe(
                    &endpoint,
                    &slot,
                    LIBUSB_TRANSFER_COMPLETED,
                    start + Duration::from_millis(millis),
                    &mut scratch
                )
                .unwrap());
        }
        unsafe {
            (*slot.transfer.as_mut().iso_packet_desc.as_mut_ptr()).actual_length -= 1;
        }
        assert!(!gate
            .observe(
                &endpoint,
                &slot,
                LIBUSB_TRANSFER_COMPLETED,
                start + Duration::from_millis(50),
                &mut scratch
            )
            .unwrap());
        unsafe {
            (*slot.transfer.as_mut().iso_packet_desc.as_mut_ptr()).actual_length += 1;
        }
        for millis in 51..=150 {
            assert!(!gate
                .observe(
                    &endpoint,
                    &slot,
                    LIBUSB_TRANSFER_COMPLETED,
                    start + Duration::from_millis(millis),
                    &mut scratch
                )
                .unwrap());
        }
        assert!(gate.qualified);
        assert!(gate
            .observe(
                &endpoint,
                &slot,
                LIBUSB_TRANSFER_COMPLETED,
                start + Duration::from_millis(151),
                &mut scratch
            )
            .unwrap());
    }

    #[test]
    fn programmed_capture_stops_after_two_invalid_windows_or_startup_expiry() {
        let endpoint = programmed_capture_endpoint();
        let geometry = IsoQueueGeometry::for_stream(1_000, 8, 4);
        let mut gate = VendorCaptureRateGate::new(&endpoint, geometry);
        gate.pattern = vec![0];
        let slot = completed_slot(0, &[0; 44 * 16]);
        let mut scratch = [CapturePacket {
            requested_bytes: 0,
            actual_bytes: 0,
            succeeded: false,
        }];
        let start = gate.started;
        for millis in 1..200 {
            assert!(!gate
                .observe(
                    &endpoint,
                    &slot,
                    LIBUSB_TRANSFER_COMPLETED,
                    start + Duration::from_millis(millis),
                    &mut scratch
                )
                .unwrap());
        }
        assert!(gate
            .observe(
                &endpoint,
                &slot,
                LIBUSB_TRANSFER_COMPLETED,
                start + Duration::from_millis(200),
                &mut scratch
            )
            .is_err());
        let fresh = VendorCaptureRateGate::new(&endpoint, geometry);
        assert!(fresh.check(fresh.started + fresh.startup).is_err());
    }

    #[test]
    fn companion_input_progress_requires_accepted_whole_frames() {
        let endpoint = vendor_capture_endpoint(1, roland_capture_profile(), 44_100).unwrap();
        let mut buffer = CaptureBuffer::new(endpoint.format, 8).unwrap();
        let mut slot = completed_slot(0, &[0; 16]);
        let mut scratch = [CapturePacket {
            requested_bytes: 0,
            actual_bytes: 0,
            succeeded: false,
        }];
        for actual_bytes in [17, 15, 0] {
            unsafe {
                (*slot.transfer.as_mut().iso_packet_desc.as_mut_ptr()).actual_length = actual_bytes;
            }
            let before = buffer.stats().captured_frames;
            ingest_slot(&slot, LIBUSB_TRANSFER_COMPLETED, &mut buffer, &mut scratch).unwrap();
            assert_eq!(buffer.stats().captured_frames, before);
        }
        unsafe {
            (*slot.transfer.as_mut().iso_packet_desc.as_mut_ptr()).actual_length = 16;
        }
        ingest_slot(&slot, LIBUSB_TRANSFER_COMPLETED, &mut buffer, &mut scratch).unwrap();
        assert_eq!(buffer.stats().captured_frames, 1);
    }
    #[test]
    fn companion_capture_gate_requires_verified_monotonic_and_fresh_output() {
        let profile = roland_capture_profile();
        let geometry = IsoQueueGeometry::for_stream(125, 8, 4);
        let mut pool = NominalCapturePool {
            slots: vec![],
            profile,
            scheduler: IsoPacketScheduler::with_geometry(44_100, 16, 125, geometry),
            acceptance: PacketAcceptance::new(44_100, 125, 16),
            first_valid: None,
            latest_valid: None,
            deadline: Duration::from_millis(100),
        };
        let at = Instant::now();
        assert!(!pool.primed());
        assert!(!pool.accepts_capture(at));
        pool.first_valid = Some(at + Duration::from_millis(1));
        pool.latest_valid = pool.first_valid;
        assert!(!pool.accepts_capture(at));
        assert!(pool.accepts_capture(at + Duration::from_millis(2)));
        assert!(!pool.accepts_capture(at + Duration::from_millis(101)));
        pool.latest_valid = Some(at + Duration::from_millis(100));
        assert!(pool.accepts_capture(at + Duration::from_millis(101)));
    }

    use crate::capture::{CaptureState, ChannelPosition};

    fn configuration(protocol: u8) -> Vec<u8> {
        let mut raw = vec![9, 2, 0, 0, 2, 1, 0, 0x80, 50];
        if protocol == 0x20 {
            raw.extend_from_slice(&[8, 11, 0, 2, 1, 0, 0x20, 0]);
        }
        raw.extend_from_slice(&[9, 4, 0, 0, 0, 1, 1, protocol, 0]);
        if protocol == 0 {
            raw.extend_from_slice(&[9, 0x24, 1, 0, 1, 39, 0, 1, 1]);
            raw.extend_from_slice(&[12, 0x24, 2, 1, 1, 2, 0, 2, 3, 0, 0, 0]);
            raw.extend_from_slice(&[9, 0x24, 6, 2, 1, 1, 0, 0, 0]);
            raw.extend_from_slice(&[9, 0x24, 3, 3, 1, 1, 0, 2, 0]);
        } else {
            raw.extend_from_slice(&[9, 0x24, 1, 0, 2, 0, 29, 0, 0]);
            raw.extend_from_slice(&[8, 0x24, 10, 10, 3, 7, 0, 0]);
            raw.extend_from_slice(&[12, 0x24, 3, 3, 1, 1, 0, 1, 10, 0, 0, 0]);
        }
        raw.extend_from_slice(&[9, 4, 1, 0, 0, 1, 2, protocol, 0]);
        raw.extend_from_slice(&[9, 4, 1, 1, 1, 1, 2, protocol, 0]);
        if protocol == 0 {
            raw.extend_from_slice(&[7, 0x24, 1, 3, 1, 1, 0]);
            raw.extend_from_slice(&[11, 0x24, 2, 1, 2, 3, 24, 1, 0x80, 0xbb, 0]);
        } else {
            raw.extend_from_slice(&[16, 0x24, 1, 3, 0, 1, 1, 0, 0, 0, 2, 3, 0, 0, 0, 0]);
            raw.extend_from_slice(&[6, 0x24, 2, 1, 4, 24]);
        }
        raw.extend_from_slice(&[9, 5, 0x81, 5, 0, 2, 1, 0, 0]);
        raw.extend_from_slice(&[7, 0x25, 1, 1, 0, 0, 0]);
        let length = raw.len() as u16;
        raw[2..4].copy_from_slice(&length.to_le_bytes());
        raw
    }

    fn superspeed_configuration(protocol: u8, packet: u16, extra: &[u8]) -> Vec<u8> {
        let mut raw = configuration(protocol);
        let endpoint = raw.windows(4).position(|w| w == [9, 5, 0x81, 5]).unwrap();
        raw[endpoint + 4..endpoint + 6].copy_from_slice(&packet.to_le_bytes());
        raw.splice(endpoint + 9..endpoint + 9, extra.iter().copied());
        let length = raw.len() as u16;
        raw[2..4].copy_from_slice(&length.to_le_bytes());
        raw
    }

    fn update_configuration_length(raw: &mut [u8]) {
        let length = raw.len() as u16;
        raw[2..4].copy_from_slice(&length.to_le_bytes());
    }

    fn adaptive_uac2_configuration() -> Vec<u8> {
        let mut raw = configuration(0x20);
        let alternate = raw.windows(4).position(|w| w == [9, 4, 1, 1]).unwrap();
        raw[alternate + 4] = 2;
        let data = raw.windows(4).position(|w| w == [9, 5, 0x81, 5]).unwrap();
        raw.splice(data..data + 9, [7, 5, 0x81, 0x09, 0, 2, 1]);
        let cs = data + 7;
        raw.splice(cs..cs + 7, [8, 0x25, 1, 0, 0, 0, 0, 0]);
        raw.extend_from_slice(&[7, 5, 0x01, 0x11, 3, 0, 1]);
        update_configuration_length(&mut raw);
        raw
    }

    fn adaptive_uac1_configuration(refresh: u8) -> Vec<u8> {
        let mut raw = configuration(0);
        let alternate = raw.windows(4).position(|w| w == [9, 4, 1, 1]).unwrap();
        raw[alternate + 4] = 2;
        let data = raw.windows(4).position(|w| w == [9, 5, 0x81, 5]).unwrap();
        raw[data + 3] = 0x09;
        raw[data + 8] = 1;
        raw.extend_from_slice(&[9, 5, 1, 0x01, 3, 0, 1, refresh, 0]);
        update_configuration_length(&mut raw);
        raw
    }

    #[test]
    fn adaptive_uac1_discovery_reproof_and_capture_gate_require_timing() {
        let raw = adaptive_uac1_configuration(4);
        let endpoint = capture_candidates(&raw, 1, Speed::Full, 48_000)
            .unwrap()
            .remove(0);
        assert_eq!(endpoint.feedforward().unwrap().refresh, Some(4));
        assert!(reprove_standard_capture(&raw, Speed::Full, &endpoint).is_ok());
        let interfaces = raw_interfaces(&raw).unwrap();
        let interface = interfaces.last().unwrap();
        let input = interface
            .descriptors
            .iter()
            .position(|d| d[1] == 5 && d[2] == 0x81)
            .unwrap();
        assert!(standard_capture_feedforward(interface, input, Speed::Full, true).is_err());
        let mut changed = raw.clone();
        let refresh = changed.len() - 2;
        changed[refresh] = 5;
        assert!(reprove_standard_capture(&changed, Speed::Full, &endpoint).is_err());
        for invalid_refresh in [0, 10] {
            assert!(capture_candidates(
                &adaptive_uac1_configuration(invalid_refresh),
                1,
                Speed::Full,
                48_000
            )
            .unwrap()
            .is_empty());
        }
        let mut pool = FeedforwardPool::for_endpoint(&endpoint).unwrap().unwrap();
        let at = Instant::now();
        assert!(!pool.primed());
        pool.first_valid_send = Some(at);
        assert!(!pool.accepts_capture_transfer(at, Some(at - Duration::from_millis(1))));
        assert!(pool.accepts_capture_transfer(at, Some(at)));
    }

    #[test]
    fn variable_adaptive_uac1_needs_explicit_unpadded_host_rate_evidence() {
        for rates in [
            [14, 0x24, 2, 1, 2, 3, 24, 2, 0x80, 0xbb, 0, 0x44, 0xac, 0],
            [14, 0x24, 2, 1, 2, 3, 24, 0, 0x44, 0xac, 0, 0x80, 0xbb, 0],
        ] {
            let mut raw = adaptive_uac1_configuration(4);
            let format = raw.windows(4).position(|w| w == [11, 0x24, 2, 1]).unwrap();
            raw.splice(format..format + 11, rates);
            let control = raw.windows(3).position(|w| w == [7, 0x25, 1]).unwrap();
            raw[control + 3] = 0;
            update_configuration_length(&mut raw);
            let mut padded_raw = raw.clone();
            padded_raw[control + 3] = 0x80;
            assert!(capture_candidates(&padded_raw, 1, Speed::Full, 48_000)
                .unwrap()
                .is_empty());
            let mut missing_raw = raw.clone();
            missing_raw.drain(control..control + 7);
            update_configuration_length(&mut missing_raw);
            assert!(capture_candidates(&missing_raw, 1, Speed::Full, 48_000)
                .unwrap()
                .is_empty());
            let mut asynchronous_raw = raw.clone();
            let data = asynchronous_raw
                .windows(4)
                .position(|w| w == [9, 5, 0x81, 9])
                .unwrap();
            asynchronous_raw[data + 3] = 5;
            asynchronous_raw[data + 8] = 0;
            assert!(
                capture_candidates(&asynchronous_raw, 1, Speed::Full, 48_000)
                    .unwrap()
                    .is_empty()
            );
            for rate in [44_100, 48_000] {
                let endpoint = capture_candidates(&raw, 1, Speed::Full, rate)
                    .unwrap()
                    .remove(0);
                assert!(endpoint.requires_host_rate_proof());
                assert!(!uac1_capture_sampling_control(&endpoint).unwrap());
                assert!(reprove_standard_capture(&raw, Speed::Full, &endpoint).is_ok());
                let geometry = IsoQueueGeometry::for_stream(endpoint.service_us, 8, 4);
                assert!(
                    capture_rate::CaptureRateProof::for_endpoint(&endpoint, geometry, None)
                        .is_some()
                );
                let mut padded = endpoint.clone();
                padded.endpoint_descriptors[12] = 0x80;
                assert!(uac1_capture_sampling_control(&padded).is_err());
                let mut missing = endpoint.clone();
                missing.endpoint_descriptors.drain(9..16);
                assert!(uac1_capture_sampling_control(&missing).is_err());
                let mut untimed = endpoint;
                untimed.standard_feedforward = None;
                assert!(!untimed.requires_host_rate_proof());
                assert!(uac1_capture_sampling_control(&untimed).is_err());
            }
        }
    }

    #[test]
    fn uac1_refresh_phase_spans_both_urbs_and_empty_submissions() {
        for refresh in [1, 4, 9] {
            let mut cadence = FeedforwardCadence::new(Some(refresh)).unwrap();
            let mut words = Vec::new();
            for transfer in 0..140 {
                let (lengths, next) = cadence.plan(8, 3);
                for (packet, length) in lengths.into_iter().enumerate() {
                    if length == 3 {
                        words.push(transfer * 8 + packet);
                    } else {
                        assert_eq!(length, 0);
                    }
                }
                cadence = next;
            }
            assert_eq!(words, (0..1120).step_by(1 << refresh).collect::<Vec<_>>());
        }
        assert!(FeedforwardCadence::new(Some(0)).is_err());
        assert!(FeedforwardCadence::new(Some(10)).is_err());
        assert_eq!(
            FeedforwardCadence::new(None).unwrap().plan(8, 4).0,
            vec![4; 8]
        );
    }

    #[test]
    fn sparse_feedforward_validates_packed_words_and_exact_empty_slots() {
        for word in [&[0, 0, 12][..], &[0, 0, 6, 0][..]] {
            let width = word.len();
            let mut slot = IsoTransferSlot::new(0, 3, width * 3).unwrap();
            slot.submitted_endpoint = 1;
            slot.submitted_bytes = width * 2;
            slot.packet_sizes = vec![width, 0, width];
            let start = Instant::now();
            slot.submitted_at = Some(start);
            unsafe {
                let data = &mut *slot.user_data;
                data.buffer[..width].copy_from_slice(word);
                data.buffer[width..width * 2].copy_from_slice(word);
                let transfer = slot.transfer.as_mut();
                transfer.endpoint = 1;
                transfer.length = (width * 2) as i32;
                transfer.num_iso_packets = 3;
                transfer.buffer = data.buffer.as_mut_ptr();
                for (index, length) in slot.packet_sizes.iter().enumerate() {
                    let packet = &mut *transfer.iso_packet_desc.as_mut_ptr().add(index);
                    packet.length = *length as u32;
                    packet.actual_length = *length as u32;
                    packet.status = LIBUSB_TRANSFER_COMPLETED;
                }
            }
            let mut scratch = [CapturePacket {
                requested_bytes: 0,
                actual_bytes: 0,
                succeeded: false,
            }; 3];
            let deadline = Duration::from_millis(100);
            assert_eq!(
                validate_feedforward_slot(
                    &slot,
                    LIBUSB_TRANSFER_COMPLETED,
                    start,
                    deadline,
                    width,
                    word,
                    &mut scratch
                ),
                Ok(true)
            );
            unsafe {
                (*slot.transfer.as_mut().iso_packet_desc.as_mut_ptr().add(1)).length = width as u32;
            }
            assert!(validate_feedforward_slot(
                &slot,
                LIBUSB_TRANSFER_COMPLETED,
                start,
                deadline,
                width,
                word,
                &mut scratch
            )
            .is_err());
            unsafe {
                (*slot.transfer.as_mut().iso_packet_desc.as_mut_ptr().add(1)).length = 0;
            }
            unsafe {
                (&mut *slot.user_data).buffer[width] ^= 1;
            }
            assert!(validate_feedforward_slot(
                &slot,
                LIBUSB_TRANSFER_COMPLETED,
                start,
                deadline,
                width,
                word,
                &mut scratch
            )
            .is_err());
        }
    }

    #[test]
    fn empty_feedforward_urb_is_valid_but_never_proves_a_report() {
        let word = [0, 0, 12];
        let mut slot = completed_slot(0, &[0; 3]);
        slot.submitted_endpoint = 1;
        slot.submitted_bytes = 0;
        slot.packet_sizes[0] = 0;
        let start = Instant::now();
        slot.submitted_at = Some(start);
        unsafe {
            let transfer = slot.transfer.as_mut();
            transfer.endpoint = 1;
            transfer.length = 0;
            let packet = &mut *transfer.iso_packet_desc.as_mut_ptr();
            packet.length = 0;
            packet.actual_length = 0;
        }
        let mut scratch = [CapturePacket {
            requested_bytes: 0,
            actual_bytes: 0,
            succeeded: false,
        }];
        let deadline = Duration::from_millis(100);
        assert_eq!(
            validate_feedforward_slot(
                &slot,
                LIBUSB_TRANSFER_COMPLETED,
                start,
                deadline,
                3,
                &word,
                &mut scratch
            ),
            Ok(false)
        );
        unsafe {
            (*slot.transfer.as_mut().iso_packet_desc.as_mut_ptr()).actual_length = 3;
        }
        assert!(validate_feedforward_slot(
            &slot,
            LIBUSB_TRANSFER_COMPLETED,
            start,
            deadline,
            3,
            &word,
            &mut scratch
        )
        .is_err());
        unsafe {
            (*slot.transfer.as_mut().iso_packet_desc.as_mut_ptr()).actual_length = 0;
        }
        slot.submitted_endpoint = 0x81;
        unsafe {
            slot.transfer.as_mut().endpoint = 0x81;
        }
        assert!(inspect_slot(&slot, LIBUSB_TRANSFER_COMPLETED, &mut scratch).is_err());
    }

    #[test]
    fn adaptive_uac2_requires_proven_timing_and_representable_nominal_rate() {
        let raw = adaptive_uac2_configuration();
        let interfaces = raw_interfaces(&raw).unwrap();
        let interface = interfaces.last().unwrap();
        let input = interface
            .descriptors
            .iter()
            .position(|d| d[1] == 5 && d[2] == 0x81)
            .unwrap();
        assert!(standard_capture_feedforward(interface, input, Speed::Full, true).is_err());
        let candidate = capture_candidates(&raw, 1, Speed::Full, 48_000)
            .unwrap()
            .remove(0);
        assert_eq!(candidate.feedforward().unwrap().address, 1);
        assert!(candidate.feedforward().unwrap().nominal(1_024_000).is_err());
        assert!(capture_candidates(&raw, 1, Speed::Full, 128_000)
            .unwrap()
            .is_empty());
        let alternate = raw.windows(4).position(|w| w == [9, 4, 1, 1]).unwrap();
        for declared in [1, 3] {
            let mut changed = raw.clone();
            changed[alternate + 4] = declared;
            assert!(capture_candidates(&changed, 1, Speed::Full, 48_000)
                .unwrap()
                .is_empty());
            assert!(!claimable_capture_input(
                raw_interfaces(&changed).unwrap().last().unwrap(),
                Speed::Full
            ));
        }
        // Adaptive UAC1 also requires its proven timing companion.
        let mut uac1 = configuration(0);
        let input = uac1.windows(4).position(|w| w == [9, 5, 0x81, 5]).unwrap();
        uac1[input + 3] = 9;
        assert!(capture_candidates(&uac1, 1, Speed::Full, 48_000)
            .unwrap()
            .is_empty());
        for change in [0, 1, 2] {
            let mut changed = raw.clone();
            let timing = changed.len() - 7;
            match change {
                0 => {
                    changed.truncate(timing);
                }
                1 => {
                    changed[timing + 2] = 2;
                }
                _ => {
                    changed[timing + 3] = 1;
                }
            }
            update_configuration_length(&mut changed);
            assert!(capture_candidates(&changed, 1, Speed::Full, 48_000)
                .unwrap()
                .is_empty());
            assert!(!claimable_capture_input(
                raw_interfaces(&changed).unwrap().last().unwrap(),
                Speed::Full
            ));
        }
    }

    #[test]
    fn adaptive_uac2_reproof_includes_timing_endpoint_and_scoped_extras() {
        let mut raw = adaptive_uac2_configuration();
        raw.extend_from_slice(&[4, 0x25, 0x7f, 0x55]);
        update_configuration_length(&mut raw);
        let candidate = capture_candidates(&raw, 1, Speed::Full, 48_000)
            .unwrap()
            .remove(0);
        assert!(reprove_standard_capture(&raw, Speed::Full, &candidate).is_ok());
        let timing = raw.windows(4).position(|w| w == [7, 5, 1, 0x11]).unwrap();
        for offset in [timing + 6, raw.len() - 1] {
            let mut changed = raw.clone();
            changed[offset] ^= 1;
            assert!(reprove_standard_capture(&changed, Speed::Full, &candidate).is_err());
        }
        let mut duplicate = raw.clone();
        duplicate.extend_from_slice(&[7, 5, 1, 0, 0, 0, 0]);
        update_configuration_length(&mut duplicate);
        assert!(reprove_standard_capture(&duplicate, Speed::Full, &candidate).is_err());
    }

    #[test]
    fn adaptive_capture_only_publishes_input_submitted_after_confirmed_timing() {
        let raw = adaptive_uac2_configuration();
        let endpoint = capture_candidates(&raw, 1, Speed::Full, 48_000)
            .unwrap()
            .remove(0);
        let mut pool = FeedforwardPool::for_endpoint(&endpoint).unwrap().unwrap();
        let at = Instant::now();
        assert!(!pool.accepts_capture_transfer(at + Duration::from_millis(2), Some(at)));
        pool.first_valid_send = Some(at);
        assert!(!pool.accepts_capture_transfer(at + Duration::from_millis(2), None));
        assert!(!pool.accepts_capture_transfer(
            at + Duration::from_millis(2),
            Some(at - Duration::from_millis(1))
        ));
        assert!(pool.accepts_capture_transfer(
            at + Duration::from_millis(2),
            Some(at + Duration::from_millis(1))
        ));
        pool.requires_post_proof_submission = false;
        assert!(pool.accepts_capture_transfer(at + Duration::from_millis(2), None));
    }

    #[test]
    fn standard_capture_reproof_rejects_stale_same_alternate_profiles() {
        for protocol in [0, 0x20] {
            let raw = configuration(protocol);
            let endpoint = capture_candidates(&raw, 1, Speed::Full, 48_000)
                .unwrap()
                .remove(0);
            assert!(reprove_standard_capture(&raw, Speed::Full, &endpoint).is_ok());
            let ep = raw.windows(4).position(|w| w == [9, 5, 0x81, 5]).unwrap();
            let format = raw
                .windows(4)
                .position(|w| {
                    w == if protocol == 0 {
                        [11, 0x24, 2, 1]
                    } else {
                        [6, 0x24, 2, 1]
                    }
                })
                .unwrap();
            let owner = raw
                .windows(4)
                .position(|w| {
                    w == if protocol == 0 {
                        [12, 0x24, 2, 1]
                    } else {
                        [8, 0x24, 10, 10]
                    }
                })
                .unwrap();
            let mut changed_sync = raw.clone();
            changed_sync[ep + 3] ^= 0x04; // Still isochronous, with different synchronization.
            let changed_endpoint = capture_candidates(&changed_sync, 1, Speed::Full, 48_000)
                .unwrap()
                .remove(0);
            assert_eq!(changed_endpoint.packet_bytes, endpoint.packet_bytes);
            assert_eq!(changed_endpoint.service_us, endpoint.service_us);
            assert!(reprove_standard_capture(&changed_sync, Speed::Full, &endpoint).is_err());
            let channel_map = if protocol == 0 {
                owner + 8
            } else {
                raw.windows(4).position(|w| w == [16, 0x24, 1, 3]).unwrap() + 11
            };
            // Preserve interface, alternate, address and valid packet capacity.
            for offset in [
                ep + 3,
                ep + 4,
                ep + 7,
                ep + 9 + 3,
                format + 5,
                owner + 6,
                channel_map,
            ] {
                let mut changed = raw.clone();
                changed[offset] ^= 1;
                assert!(
                    reprove_standard_capture(&changed, Speed::Full, &endpoint).is_err(),
                    "accepted changed standard capture byte {offset} for protocol {protocol}"
                );
            }
            for offset in [0, 1, 2, 5] {
                let mut changed = raw.clone();
                changed[offset] ^= 1;
                assert!(reprove_standard_capture(&changed, Speed::Full, &endpoint).is_err());
            }
        }
    }

    #[test]
    fn standard_capture_reproof_rejects_filtered_duplicate_records() {
        for protocol in [0, 0x20] {
            let raw = configuration(protocol);
            let endpoint = capture_candidates(&raw, 1, Speed::Full, 48_000)
                .unwrap()
                .remove(0);
            // A malformed endpoint with this address is filtered by geometry.
            let mut duplicate = raw.clone();
            duplicate.extend_from_slice(&[7, 5, 0x81, 0, 0, 0, 0]);
            update_configuration_length(&mut duplicate);
            assert_eq!(
                capture_candidates(&duplicate, 1, Speed::Full, 48_000)
                    .unwrap()
                    .len(),
                1
            );
            assert!(reprove_standard_capture(&duplicate, Speed::Full, &endpoint).is_err());
            // A duplicate AS claim is filtered by its wrong class/protocol.
            let mut duplicate = raw.clone();
            duplicate.extend_from_slice(&[9, 4, 1, 1, 0, 0xff, 0, 0xff, 0]);
            update_configuration_length(&mut duplicate);
            assert_eq!(
                capture_candidates(&duplicate, 1, Speed::Full, 48_000)
                    .unwrap()
                    .len(),
                1
            );
            assert!(reprove_standard_capture(&duplicate, Speed::Full, &endpoint).is_err());
            // Duplicate the owner's identity with an otherwise ineligible record.
            let mut duplicate = raw.clone();
            duplicate.extend_from_slice(&[9, 4, 0, 0, 0, 0xff, 0, 0xff, 0]);
            update_configuration_length(&mut duplicate);
            assert!(reprove_standard_capture(&duplicate, Speed::Full, &endpoint).is_err());
        }
    }

    #[test]
    fn standard_capture_reproof_preserves_superspeed_endpoint_extras() {
        for protocol in [0, 0x20] {
            let raw = superspeed_configuration(protocol, 1024, &[6, 0x30, 1, 1, 0, 16]);
            let endpoint = capture_candidates(&raw, 1, Speed::Super, 48_000)
                .unwrap()
                .remove(0);
            assert!(reprove_standard_capture(&raw, Speed::Super, &endpoint).is_ok());
            let companion = raw.windows(2).position(|w| w == [6, 0x30]).unwrap();
            let mut changed = raw.clone();
            changed[companion + 2] = 2;
            assert!(reprove_standard_capture(&changed, Speed::Super, &endpoint).is_err());
            // Unknown endpoint extras also form part of the selected wire snapshot.
            let mut changed = raw.clone();
            changed.extend_from_slice(&[3, 0x26, 1]);
            update_configuration_length(&mut changed);
            assert!(reprove_standard_capture(&changed, Speed::Super, &endpoint).is_err());
        }
    }

    #[test]
    fn standard_superspeed_capture_uses_companion_capacity_for_both_audio_classes() {
        for protocol in [0, 0x20] {
            let raw = superspeed_configuration(protocol, 1024, &[6, 0x30, 1, 1, 0, 16]);
            for speed in [Speed::Super, Speed::SuperPlus] {
                let candidates = capture_candidates(&raw, 1, speed, 48_000).unwrap();
                assert_eq!(candidates.len(), 1);
                assert_eq!(candidates[0].packet_bytes, 4096);
                assert_eq!(candidates[0].service_us, 125);
                assert_eq!(candidates[0].protocol, protocol);
                assert!(candidates[0].vendor_pcm.is_none());
                assert!(candidates[0].adc3_route.is_none());
            }
            // The packet field alone is sufficient, but the effective service
            // capacity cannot carry this rate's largest whole-frame packet.
            let raw = superspeed_configuration(protocol, 1024, &[6, 0x30, 0, 0, 32, 0]);
            assert!(capture_candidates(&raw, 1, Speed::Super, 48_000)
                .unwrap()
                .is_empty());
            let interfaces = raw_interfaces(&raw).unwrap();
            let interface = interfaces.last().unwrap();
            assert!(claimable_capture_input(interface, Speed::Super));
        }
    }

    #[test]
    fn superspeed_capture_rejects_missing_misplaced_and_malformed_companions() {
        for extra in [
            &[][..],
            &[7, 0x25, 1, 1, 0, 0, 0, 6, 0x30, 0, 0, 64, 0][..],
            &[6, 0x30, 0, 0, 0, 0][..],
            &[6, 0x30, 0, 0, 1, 4][..], // capacity exceeds packet
            &[6, 0x30, 0, 3, 64, 0][..],
            &[6, 0x30, 0, 4, 64, 0][..],
            &[6, 0x30, 0, 0, 64, 0, 6, 0x30, 0, 0, 64, 0][..],
            &[7, 5, 0x82, 5, 0, 4, 1, 6, 0x30, 0, 0, 64, 0][..],
        ] {
            let raw = superspeed_configuration(0x20, 1024, extra);
            let candidates = capture_candidates(&raw, 1, Speed::Super, 48_000).unwrap();
            // The last case gives a companion only to endpoint 0x82.
            assert!(candidates.iter().all(|candidate| candidate.address != 0x81));
            let interfaces = raw_interfaces(&raw).unwrap();
            let interface = interfaces.last().unwrap();
            let endpoint = interface
                .descriptors
                .iter()
                .position(|ep| ep[1] == 5)
                .unwrap();
            assert!(capture_input_geometry(interface, endpoint, Speed::Super).is_none());
        }
        for packet in [0, 1025, 0x0c00] {
            let raw = superspeed_configuration(0x20, packet, &[6, 0x30, 0, 0, 64, 0]);
            assert!(capture_candidates(&raw, 1, Speed::Super, 48_000)
                .unwrap()
                .is_empty());
        }
    }

    #[test]
    fn extended_capture_companion_requires_superplus_and_retains_effective_capacity() {
        let raw = superspeed_configuration(
            0x20,
            1024,
            &[6, 0x30, 0, 0x80, 1, 0, 8, 0x31, 0, 0, 0, 0, 1, 0],
        );
        assert!(capture_candidates(&raw, 1, Speed::Super, 48_000)
            .unwrap()
            .is_empty());
        let candidates = capture_candidates(&raw, 1, Speed::SuperPlus, 48_000).unwrap();
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].packet_bytes, 65_536);
        let interfaces = raw_interfaces(&raw).unwrap();
        assert!(!claimable_capture_input(
            interfaces.last().unwrap(),
            Speed::Super
        ));
        assert!(claimable_capture_input(
            interfaces.last().unwrap(),
            Speed::SuperPlus
        ));
    }

    #[test]
    fn ordinary_capture_keeps_full_and_high_speed_packet_geometry() {
        for protocol in [0, 0x20] {
            let raw = configuration(protocol);
            for (speed, service_us) in [(Speed::Full, 1000), (Speed::High, 125)] {
                let candidates = capture_candidates(&raw, 1, speed, 48_000).unwrap();
                assert_eq!(candidates.len(), 1);
                assert_eq!(candidates[0].packet_bytes, 512);
                assert_eq!(candidates[0].service_us, service_us);
            }
            assert!(capture_candidates(&raw, 1, Speed::Super, 48_000)
                .unwrap()
                .is_empty());
        }
    }

    #[test]
    fn feedforward_sends_require_every_exact_packet_and_reject_detach_geometry() {
        let valid = CapturePacket {
            requested_bytes: 4,
            actual_bytes: 4,
            succeeded: true,
        };
        assert!(validate_feedforward_packets(4, &[4, 4], &[valid, valid]).is_ok());
        assert!(validate_feedforward_packets(4, &[], &[]).is_err());
        for packet in [
            CapturePacket {
                actual_bytes: 0,
                ..valid
            },
            CapturePacket {
                actual_bytes: 3,
                ..valid
            },
            CapturePacket {
                actual_bytes: 5,
                ..valid
            },
            CapturePacket {
                requested_bytes: 3,
                ..valid
            },
            CapturePacket {
                succeeded: false,
                ..valid
            },
        ] {
            assert!(validate_feedforward_packets(4, &[4, 4], &[valid, packet]).is_err());
        }
    }

    #[test]
    fn input_only_uac1_uses_terminal_cluster_through_feature_unit() {
        let raw = configuration(0);
        let candidates = capture_candidates(&raw, 1, Speed::Full, 48_000).unwrap();
        assert_eq!(candidates.len(), 1);
        let candidate = &candidates[0];
        assert_eq!(candidate.address, 0x81);
        assert_eq!(candidate.control, 0);
        assert_eq!(candidate.format.subslot_bytes(), 3);
        assert_eq!(candidate.format.valid_bits(), 24);
        assert_eq!(
            candidate.format.channel_map().positions(),
            &[ChannelPosition::FrontLeft, ChannelPosition::FrontRight]
        );
        assert!(candidate.sampling_control);
        assert!(capture_candidates(&raw, 1, Speed::Full, 44_100)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn duplex_capture_requires_explicit_unambiguous_endpoint_rate_control() {
        let raw = configuration(0);
        let candidate = capture_candidates(&raw, 1, Speed::Full, 48_000)
            .unwrap()
            .remove(0);
        assert!(uac1_capture_sampling_control(&candidate).unwrap());
        let mut fixed = candidate.clone();
        fixed.endpoint_descriptors[12] = 0x80; // Explicit no-control + MaxPacketsOnly.
        fixed.sampling_control = false;
        assert!(!uac1_capture_sampling_control(&fixed).unwrap());
        fixed.fixed_rate = false;
        assert!(uac1_capture_sampling_control(&fixed).is_err());
        for descriptors in [
            candidate.endpoint_descriptors[..9].to_vec(),
            [
                candidate.endpoint_descriptors.as_slice(),
                &[7, 0x25, 1, 1, 0, 0, 0],
            ]
            .concat(),
            [&candidate.endpoint_descriptors[..9], &[6, 0x25, 1, 1, 0, 0]].concat(),
            [
                &candidate.endpoint_descriptors[..9],
                &[7, 0x25, 1, 5, 0, 0, 0],
            ]
            .concat(),
        ] {
            let mut malformed = candidate.clone();
            malformed.endpoint_descriptors = descriptors;
            assert!(uac1_capture_sampling_control(&malformed).is_err());
        }
    }

    #[test]
    fn duplex_uac1_fresh_pair_proves_exact_playback_rate_and_terminal() {
        let mut raw = configuration(0);
        let header = raw.windows(4).position(|d| d == [9, 0x24, 1, 0]).unwrap();
        raw[header] = 10;
        raw[header + 5] = 52;
        raw[header + 7] = 2;
        raw.insert(header + 9, 2);
        let control_end = raw.windows(4).position(|d| d == [9, 4, 1, 0]).unwrap();
        raw.splice(
            control_end..control_end,
            [12, 0x24, 2, 4, 1, 1, 0, 2, 3, 0, 0, 0],
        );
        let input_alt = raw.windows(4).position(|d| d == [9, 4, 1, 1]).unwrap();
        let mut output = raw[input_alt..].to_vec();
        output[2] = 2;
        output[12] = 4; // AS_GENERAL bTerminalLink: USB streaming Input Terminal.
        let endpoint = output
            .windows(4)
            .position(|d| d == [9, 5, 0x81, 5])
            .unwrap();
        output[endpoint + 2] = 1;
        raw.extend_from_slice(&[9, 4, 2, 0, 0, 1, 2, 0, 0]);
        raw.extend_from_slice(&output);
        raw[4] = 3;
        update_configuration_length(&mut raw);
        let capture = capture_candidates(&raw, 1, Speed::Full, 48_000)
            .unwrap()
            .remove(0);
        let mut playback = IsoStreamCandidate {
            configuration_number: 1,
            interface_number: 2,
            alt_setting: 1,
            endpoint_address: 1,
            endpoint_interval: 1,
            service_interval_us: 1000,
            usb_speed: Speed::Full,
            max_packet_bytes: 512,
            sync_type: SyncType::Asynchronous,
            usage_type: UsageType::Data,
            format_tag: 1,
            uac2_formats: None,
            native_dsd: None,
            channels: 2,
            subslot_size: 3,
            bit_resolution: 24,
            sample_rates: vec![48_000],
            refresh: 0,
            synch_address: 0,
            feedback_endpoint: None,
            implicit_feedback: None,
            terminal_link: Some(4),
            is_uac1: true,
        };
        assert_eq!(
            reprove_uac1_duplex_owners(&raw, Speed::Full, &playback, &capture, 48_000).unwrap(),
            (0, true, true)
        );
        assert!(
            reprove_uac1_duplex_owners(&raw, Speed::Full, &playback, &capture, 44_100).is_err()
        );
        playback.terminal_link = Some(3);
        assert!(
            reprove_uac1_duplex_owners(&raw, Speed::Full, &playback, &capture, 48_000).is_err()
        );
    }

    #[test]
    fn duplex_uac1_collection_owner_rejects_selected_collection_ambiguity() {
        let raw = configuration(0);
        let interfaces = raw_interfaces(&raw).unwrap();
        assert_eq!(
            uac1_collection_owner(&interfaces, &interfaces[2])
                .unwrap()
                .number,
            0
        );
        let mut duplicated = interfaces.clone();
        duplicated.push(interfaces[0].clone());
        assert!(uac1_collection_owner(&duplicated, &interfaces[2]).is_err());
        let bad_header = [10, 0x24, 1, 0, 1, 40, 0, 2, 1, 1];
        let mut malformed = interfaces.clone();
        malformed[0].descriptors[0] = &bad_header;
        assert!(uac1_collection_owner(&malformed, &interfaces[2]).is_err());
        let unrelated_header = [8, 0x24, 1, 0, 1, 8, 0, 1];
        let mut unrelated = interfaces[0].clone();
        unrelated.number = 7;
        unrelated.descriptors = vec![&unrelated_header];
        let mut multiple_functions = interfaces.clone();
        multiple_functions.push(unrelated);
        assert_eq!(
            uac1_collection_owner(&multiple_functions, &interfaces[2])
                .unwrap()
                .number,
            0
        );
    }

    #[test]
    fn uac2_uses_as_cluster_and_keeps_padding_without_playback() {
        let raw = configuration(0x20);
        let candidates = capture_candidates(&raw, 1, Speed::Full, 48_000).unwrap();
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].format.bytes_per_frame(), 8);
        assert_eq!(candidates[0].format.valid_bits(), 24);
        assert_eq!(
            candidates[0].format.channel_map().positions(),
            &[ChannelPosition::FrontLeft, ChannelPosition::FrontRight]
        );
        // A floating point-only AS bitmap cannot become integer PCM capture.
        let mut float_raw = raw.clone();
        let offset = float_raw
            .windows(6)
            .position(|w| w == [16, 0x24, 1, 3, 0, 1])
            .unwrap();
        float_raw[offset + 6] = 4;
        assert!(capture_candidates(&float_raw, 1, Speed::Full, 48_000)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn malformed_descriptors_wrong_function_and_output_endpoint_are_rejected() {
        let mut raw = configuration(0);
        raw.pop();
        assert!(capture_candidates(&raw, 1, Speed::Full, 48_000).is_err());
        let mut raw = configuration(0);
        let offset = raw.windows(4).position(|w| w == [9, 5, 0x81, 5]).unwrap();
        raw[offset + 2] = 1;
        assert!(capture_candidates(&raw, 1, Speed::Full, 48_000)
            .unwrap()
            .is_empty());
        let mut raw = configuration(0x20);
        raw[9 + 3] = 1; // IAD no longer includes streaming interface 1.
        assert!(capture_candidates(&raw, 1, Speed::Full, 48_000)
            .unwrap()
            .is_empty());
    }

    fn completed_slot(index: usize, bytes: &[u8]) -> IsoTransferSlot {
        let mut slot = IsoTransferSlot::new(index, 1, bytes.len()).unwrap();
        slot.packet_sizes[0] = bytes.len();
        slot.submitted_bytes = bytes.len();
        slot.submitted_endpoint = 0x81;
        unsafe {
            let data = &mut *slot.user_data;
            data.buffer.copy_from_slice(bytes);
            let transfer = slot.transfer.as_mut();
            transfer.endpoint = 0x81;
            transfer.length = bytes.len() as i32;
            transfer.num_iso_packets = 1;
            transfer.buffer = data.buffer.as_mut_ptr();
            let descriptor = &mut *transfer.iso_packet_desc.as_mut_ptr();
            descriptor.length = bytes.len() as u32;
            descriptor.actual_length = bytes.len() as u32;
            descriptor.status = LIBUSB_TRANSFER_COMPLETED;
        }
        slot
    }

    #[test]
    fn capture_lock_delay_counts_short_whole_frames_and_resets_on_malformed_packets() {
        let mut slot = completed_slot(0, &[0; 8]);
        let mut scratch = [CapturePacket {
            requested_bytes: 0,
            actual_bytes: 0,
            succeeded: false,
        }];
        let mut gate =
            endpoint_lock::LockDelayGate::new(Some(endpoint_lock::LockDelay::PcmSampleFrames(3)));
        unsafe {
            (*slot.transfer.as_mut().iso_packet_desc.as_mut_ptr()).actual_length = 4;
        }
        let (stats, result) = discard_lock_delay_capture(
            &slot,
            LIBUSB_TRANSFER_COMPLETED,
            &mut scratch,
            4,
            1000,
            true,
            &mut gate,
        );
        result.unwrap();
        assert_eq!(stats.short_packets, 1);
        assert_eq!(stats.captured_frames, 1);
        assert_eq!(stats.proof_suppressed_frames, 1);
        assert!(gate.pending());
        unsafe {
            (*slot.transfer.as_mut().iso_packet_desc.as_mut_ptr()).actual_length = 3;
        }
        let (stats, result) = discard_lock_delay_capture(
            &slot,
            LIBUSB_TRANSFER_COMPLETED,
            &mut scratch,
            4,
            1000,
            true,
            &mut gate,
        );
        result.unwrap();
        assert_eq!(stats.malformed_packets, 1);
        unsafe {
            (*slot.transfer.as_mut().iso_packet_desc.as_mut_ptr()).actual_length = 8;
        }
        discard_lock_delay_capture(
            &slot,
            LIBUSB_TRANSFER_COMPLETED,
            &mut scratch,
            4,
            1000,
            true,
            &mut gate,
        )
        .1
        .unwrap();
        assert!(gate.pending()); // Two frames after the malformed interval.
        let (stats, result) = discard_lock_delay_capture(
            &slot,
            LIBUSB_TRANSFER_COMPLETED,
            &mut scratch,
            4,
            1000,
            true,
            &mut gate,
        );
        result.unwrap();
        assert!(!gate.pending());
        assert_eq!(stats.captured_frames, 2); // Satisfying URB is suppressed too.
        assert_eq!(stats.proof_suppressed_frames, 2);
    }

    #[test]
    fn capture_lock_delay_requires_timing_and_preserves_packet_detach() {
        let mut slot = completed_slot(0, &[0; 8]);
        let mut scratch = [CapturePacket {
            requested_bytes: 0,
            actual_bytes: 0,
            succeeded: false,
        }];
        let mut gate =
            endpoint_lock::LockDelayGate::new(Some(endpoint_lock::LockDelay::Milliseconds(2)));
        discard_lock_delay_capture(
            &slot,
            LIBUSB_TRANSFER_COMPLETED,
            &mut scratch,
            4,
            1000,
            true,
            &mut gate,
        )
        .1
        .unwrap();
        discard_lock_delay_capture(
            &slot,
            LIBUSB_TRANSFER_COMPLETED,
            &mut scratch,
            4,
            1000,
            false,
            &mut gate,
        )
        .1
        .unwrap();
        discard_lock_delay_capture(
            &slot,
            LIBUSB_TRANSFER_COMPLETED,
            &mut scratch,
            4,
            1000,
            true,
            &mut gate,
        )
        .1
        .unwrap();
        assert!(gate.pending());
        unsafe {
            (*slot.transfer.as_mut().iso_packet_desc.as_mut_ptr()).status =
                LIBUSB_TRANSFER_NO_DEVICE;
        }
        let (stats, result) = discard_lock_delay_capture(
            &slot,
            LIBUSB_TRANSFER_COMPLETED,
            &mut scratch,
            4,
            1000,
            true,
            &mut gate,
        );
        assert!(matches!(result, Err(Error::DeviceLost)));
        assert_eq!(stats.failed_packets, 1);
        unsafe {
            (*slot.transfer.as_mut().iso_packet_desc.as_mut_ptr()).status =
                LIBUSB_TRANSFER_COMPLETED;
        }
        discard_lock_delay_capture(
            &slot,
            LIBUSB_TRANSFER_COMPLETED,
            &mut scratch,
            4,
            1000,
            true,
            &mut gate,
        )
        .1
        .unwrap();
        assert!(gate.pending());
    }

    fn publish(slot: &IsoTransferSlot) {
        unsafe {
            let user_data = &*slot.user_data;
            user_data.completion.reset();
            *user_data.completion.status.lock().unwrap() = Some(LIBUSB_TRANSFER_COMPLETED);
            user_data.completion.publish_for_test();
        }
    }

    #[test]
    fn feedforward_completion_rejects_stall_detach_short_and_late_reports() {
        let payload = [0, 0, 6, 0];
        let mut slot = completed_slot(0, &payload);
        let start = Instant::now();
        slot.submitted_at = Some(start);
        let deadline = Duration::from_millis(100);
        let mut scratch = [CapturePacket {
            requested_bytes: 0,
            actual_bytes: 0,
            succeeded: false,
        }];
        assert!(validate_feedforward_slot(
            &slot,
            LIBUSB_TRANSFER_COMPLETED,
            start + deadline / 2,
            deadline,
            4,
            &payload,
            &mut scratch
        )
        .is_ok());
        assert!(validate_feedforward_slot(
            &slot,
            LIBUSB_TRANSFER_COMPLETED,
            start + deadline,
            deadline,
            4,
            &payload,
            &mut scratch
        )
        .is_err());
        assert!(validate_feedforward_slot(
            &slot,
            LIBUSB_TRANSFER_STALL,
            start,
            deadline,
            4,
            &payload,
            &mut scratch
        )
        .is_err());
        assert_eq!(
            validate_feedforward_slot(
                &slot,
                LIBUSB_TRANSFER_NO_DEVICE,
                start,
                deadline,
                4,
                &payload,
                &mut scratch
            ),
            Err(Error::DeviceLost)
        );
        unsafe {
            (*slot.transfer.as_mut().iso_packet_desc.as_mut_ptr()).actual_length = 3;
        }
        assert!(validate_feedforward_slot(
            &slot,
            LIBUSB_TRANSFER_COMPLETED,
            start,
            deadline,
            4,
            &payload,
            &mut scratch
        )
        .is_err());
        unsafe {
            (*slot.transfer.as_mut().iso_packet_desc.as_mut_ptr()).status =
                LIBUSB_TRANSFER_NO_DEVICE;
        }
        assert_eq!(
            validate_feedforward_slot(
                &slot,
                LIBUSB_TRANSFER_COMPLETED,
                start,
                deadline,
                4,
                &payload,
                &mut scratch
            ),
            Err(Error::DeviceLost)
        );
    }

    #[test]
    fn reordered_real_iso_descriptors_deliver_in_submission_order() {
        let slots = vec![completed_slot(0, &[1, 2]), completed_slot(1, &[3, 4])];
        let mut order = VecDeque::from([0, 1]);
        let mut buffer = CaptureBuffer::new(
            CaptureFormat::pcm(48_000, 16, 2, ChannelMap::uac1(1, 0).unwrap()).unwrap(),
            4,
        )
        .unwrap();
        let mut scratch = [CapturePacket {
            requested_bytes: 0,
            actual_bytes: 0,
            succeeded: false,
        }];
        publish(&slots[1]);
        assert_eq!(oldest_ready_capture(&order, &slots), None);
        publish(&slots[0]);
        while let Some(index) = oldest_ready_capture(&order, &slots) {
            ingest_slot(
                &slots[index],
                LIBUSB_TRANSFER_COMPLETED,
                &mut buffer,
                &mut scratch,
            )
            .unwrap();
            order.pop_front();
        }
        let mut output = [0; 4];
        assert_eq!(buffer.read(&mut output), Ok(2));
        assert_eq!(output, [1, 2, 3, 4]);
        assert_eq!(buffer.state(), CaptureState::Active);
    }

    #[test]
    fn iso_adapter_rejects_mutation_and_reports_disconnect_without_host_overrun() {
        let mut slot = completed_slot(0, &[1, 2]);
        let mut buffer = CaptureBuffer::new(
            CaptureFormat::pcm(48_000, 16, 2, ChannelMap::uac1(1, 0).unwrap()).unwrap(),
            4,
        )
        .unwrap();
        let mut scratch = [CapturePacket {
            requested_bytes: 0,
            actual_bytes: 0,
            succeeded: false,
        }];
        unsafe {
            slot.transfer.as_mut().endpoint = 0x82;
        }
        assert!(ingest_slot(&slot, LIBUSB_TRANSFER_COMPLETED, &mut buffer, &mut scratch).is_err());
        assert_eq!(buffer.available_frames(), 0);
        assert_eq!(buffer.stats().malformed_completions, 1);
        assert_eq!(
            ingest_slot(&slot, LIBUSB_TRANSFER_NO_DEVICE, &mut buffer, &mut scratch),
            Err(Error::DeviceLost)
        );
        assert_eq!(buffer.stats().failed_transfers, 1);
        assert_eq!(buffer.stats().missing_frames, 0);
        assert_eq!(buffer.stats().overrun_frames, 0);
    }
    #[test]
    fn duplex_ring_exact_capacity_and_sequence_wrap() {
        for capacity in [1, 3, 5] {
            let ring = CaptureSpsc::new(capacity, 1).unwrap();
            for cycle in 0..8u8 {
                let input: Vec<u8> = (0..capacity + 1).map(|i| cycle * 10 + i as u8).collect();
                assert_eq!(ring.push_packet(&input), Ok(capacity));
                assert_eq!(ring.available_frames(), capacity);
                assert_eq!(ring.push_packet(&[200]), Ok(0));
                let mut first = [0];
                assert_eq!(ring.read(&mut first), Ok(1));
                assert_eq!(first[0], input[0]);
                assert_eq!(ring.push_packet(&[201]), Ok(1));
                let mut rest = vec![0; capacity];
                assert_eq!(ring.read(&mut rest), Ok(capacity));
                assert_eq!(&rest[..capacity - 1], &input[1..capacity]);
                assert_eq!(rest[capacity - 1], 201);
            }
            ring.deferred.finish(true);
            assert_eq!(
                ring.read(&mut [0]),
                Err(crate::capture::CaptureError::Failed)
            );
        }
    }

    #[test]
    fn duplex_ring_unpublished_read_keeps_owned_frames_until_release() {
        let ring = CaptureSpsc::new(3, 1).unwrap();
        ring.bytes[0].store(99, Ordering::Relaxed);
        assert_eq!(ring.available_frames(), 0); // Bytes alone are not published.
        assert_eq!(ring.push_packet(&[1, 2]), Ok(2));
        // Model the consumer paused after copying one byte but before publishing.
        let guard = ring.consumer.lock();
        let copied = ring.bytes[0].load(Ordering::Relaxed);
        assert_eq!(ring.push_packet(&[3, 4]), Ok(1));
        assert_eq!(copied, 1);
        assert_eq!(ring.bytes[0].load(Ordering::Relaxed), 1);
        ring.read_seq.store(1, Ordering::Release);
        drop(guard);
        assert_eq!(ring.push_packet(&[4, 5]), Ok(1));
        let mut output = [0; 3];
        assert_eq!(ring.read(&mut output), Ok(3));
        assert_eq!(output, [2, 3, 4]);
    }

    #[test]
    fn duplex_reader_contention_preserves_capacity_without_blocking_completion_or_finish() {
        for shared_implicit in [false, true] {
            let endpoint = capture_candidates(&configuration(0), 1, Speed::Full, 48_000)
                .unwrap()
                .remove(0);
            let attachment = duplex::Attachment::new(endpoint, shared_implicit, 1).unwrap();
            let mut reader = attachment.reader(Arc::new(AtomicBool::new(false)));
            let retirement = attachment.retirement_guard();
            let mut slot = completed_slot(0, &[1; 12]); // stereo 24-bit: two frames
            let mut scratch = [CapturePacket {
                requested_bytes: 0,
                actual_bytes: 0,
                succeeded: false,
            }];
            attachment
                .ingest(&slot, LIBUSB_TRANSFER_COMPLETED, &mut scratch)
                .unwrap();
            assert_eq!(reader.read(&mut [0; 6]), Ok(1));

            // Deliberately retain the reader lock on this thread. Any blocking
            // worker append or finish would deadlock here, before OUT resumes.
            let CapturePresentation::Duplex(ring) = &reader.presentation else {
                unreachable!()
            };
            let held = ring.consumer.lock();
            attachment
                .ingest(&slot, LIBUSB_TRANSFER_COMPLETED, &mut scratch)
                .unwrap();
            unsafe {
                (*slot.transfer.as_mut().iso_packet_desc.as_mut_ptr()).actual_length = 6;
            }
            attachment
                .ingest(&slot, LIBUSB_TRANSFER_COMPLETED, &mut scratch)
                .unwrap();
            unsafe {
                (*slot.transfer.as_mut().iso_packet_desc.as_mut_ptr()).actual_length = 0;
            }
            attachment
                .ingest(&slot, LIBUSB_TRANSFER_COMPLETED, &mut scratch)
                .unwrap();
            unsafe {
                (*slot.transfer.as_mut().iso_packet_desc.as_mut_ptr()).actual_length = 5;
            }
            attachment
                .ingest(&slot, LIBUSB_TRANSFER_COMPLETED, &mut scratch)
                .unwrap();
            unsafe {
                (*slot.transfer.as_mut().iso_packet_desc.as_mut_ptr()).status =
                    LIBUSB_TRANSFER_NO_DEVICE;
            }
            assert_eq!(
                attachment.ingest(&slot, LIBUSB_TRANSFER_COMPLETED, &mut scratch),
                Err(Error::DeviceLost)
            );
            assert_eq!(
                attachment.ingest(&slot, LIBUSB_TRANSFER_NO_DEVICE, &mut scratch),
                Err(Error::DeviceLost)
            );
            unsafe {
                slot.transfer.as_mut().endpoint = 0x82;
            }
            assert!(attachment
                .ingest(&slot, LIBUSB_TRANSFER_COMPLETED, &mut scratch)
                .is_err());
            attachment.reordered();
            attachment.finish(Some(Error::DeviceLost));
            drop(retirement);
            assert!(reader.retired.as_ref().unwrap().load(Ordering::Acquire));
            drop(held);

            let stats = reader.stats();
            assert_eq!(stats.captured_frames, 5);
            assert_eq!(stats.delivered_frames, 1);
            assert_eq!(stats.overrun_frames, 3);
            assert_eq!(stats.contention_frames, 0);
            assert_eq!(stats.empty_packets, 1);
            assert_eq!(stats.short_packets, 1);
            assert_eq!(stats.malformed_packets, 1);
            assert_eq!(stats.failed_packets, 1);
            assert_eq!(stats.failed_transfers, 1);
            assert_eq!(stats.malformed_completions, 1);
            assert_eq!(stats.reordered_completions, 1);
            assert_eq!(stats.missing_frames, 0);
            assert_eq!(stats.device_position_frames(), 5);
            assert_eq!(
                stats.captured_frames,
                stats.delivered_frames
                    + reader.available_frames() as u64
                    + stats.overrun_frames
                    + stats.contention_frames
            );
            assert_eq!(reader.state(), CaptureState::Failed);
            assert_eq!(reader.error.lock().clone(), Some(Error::DeviceLost));
            reader.stop().unwrap();
        }
    }
}

#[cfg(test)]
mod capture_geometry_tests {
    use super::*;

    #[test]
    fn extended_capture_queue_checks_host_and_libusb_lengths_before_allocation() {
        let geometry = IsoQueueGeometry::for_stream(125, 16, 8);
        assert_eq!(
            capture_queue_bytes(98_303, geometry).unwrap(),
            98_303 * geometry.packets_per_transfer
        );
        assert!(matches!(
            capture_queue_bytes(0, geometry),
            Err(Error::Unsupported { .. })
        ));
        assert!(matches!(
            capture_queue_bytes(usize::MAX, geometry),
            Err(Error::Unsupported { .. })
        ));
        assert!(matches!(
            capture_queue_bytes(i32::MAX as usize, geometry),
            Err(Error::Unsupported { .. })
        ));
        let too_many = IsoQueueGeometry {
            transfer_slots: 256,
            ..geometry
        };
        assert!(matches!(
            capture_queue_bytes(4, too_many),
            Err(Error::Unsupported { .. })
        ));
        let overflow = IsoQueueGeometry {
            transfer_slots: usize::MAX,
            ..geometry
        };
        assert!(matches!(
            capture_queue_bytes(98_303, overflow),
            Err(Error::Unsupported { .. })
        ));
    }
}
