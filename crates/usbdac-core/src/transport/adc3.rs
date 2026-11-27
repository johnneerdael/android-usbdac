//! Active-configuration ADC3 PCM playback and capture. No configuration changes or clock guesses.
use super::*;
use super::endpoint_geometry::{ss_endpoint_geometry, ss_feedback_geometry};
#[cfg(test)]
use super::endpoint_geometry::ss_periodic_geometry;
use crate::clock_resolution::{ClockHop, ClockHopKind, ClockResolution, ClockPlan, ClockPlanIo, ClockGraph, RateRatio};
use crate::descriptors::uac3 as wire;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Adc3Route {
    config: u8,
    pub(super) control: u8,
    first: u8,
    count: u8,
    profile: wire::FunctionProfile,
    interface: u8,
    alt: u8,
    terminal: u8,
    endpoint: (u8, u8, u16, u32),
    packet_bytes: usize,
    sync_type: SyncType,
    usage_type: UsageType,
    feedback: Option<(u8, u8, usize)>,
    pub(super) feedforward: Option<FeedforwardEndpoint>,
    pub(super) implicit_feedforward: bool,
    implicit: Option<Box<Adc3Route>>,
    companions: Vec<(u8, [u8; 6])>,
    extended_companions: Vec<(u8, [u8; 8])>,
    pcm: (u16, u8, u8),
    frequency_limits: Vec<(u32, u32)>,
    cluster: Option<wire::Cluster>,
    header: Option<wire::AsHeader>,
    pub(super) clock: Option<wire::ClockSource>,
    pub(super) choice: Option<ClockChoice>,
    domains: Vec<wire::PowerDomain>,
    plan: Option<ClockPlan>,
    clock_context: Option<Adc3ClockContext>,
    // Pin only live replacements; ordinary selection retains its container policy.
    live_alternate: bool,
}
impl Adc3Route {
    pub(super) fn implicit_status_route(&self) -> Option<&Adc3Route> {
        self.implicit.as_deref()
    }

    fn ratio(&self) -> Result<RateRatio, String> {
        match &self.choice {
            Some(choice) => adc3_choice_ratio(choice),
            None if matches!(self.profile, wire::FunctionProfile::Badd(_)) => Ok(RateRatio::IDENTITY),
            None => Err("ADC3 clock ratio missing".into()),
        }
    }
    pub(super) fn source_rate(&self, wire: u32) -> Result<u32, String> {
        self.ratio()?.source_rate(wire).ok_or_else(|| "ADC3 wire rate has no exact source rate".into())
    }
    pub(super) fn wire_rate(&self, source: u32) -> Result<u32, String> {
        self.ratio()?.wire_rate(source).ok_or_else(|| "ADC3 source rate has no exact wire rate".into())
    }
}
fn adc3_choice_ratio(choice: &ClockChoice) -> Result<RateRatio, String> {
    RateRatio::from_factors(&choice.path.iter().filter_map(|hop| match hop.kind {
        ClockHopKind::RateMultiplier { numerator, denominator } => Some((numerator, denominator)),
        _ => None,
    }).collect::<Vec<_>>())
}
fn adc3_multiplier_factors(controls: wire::Controls,
    mut read: impl FnMut(u8, &mut [u8; 2]) -> Result<(), String>) -> Result<(u16, u16), String> {
    let mut factors = [0; 2];
    for selector in 1..=2 {
        if !matches!(controls.access(selector), wire::ControlAccess::ReadOnly | wire::ControlAccess::ReadWrite) {
            return Err("ADC3 multiplier factor is not readable".into());
        }
        let mut bytes = [0; 2];
        read(selector, &mut bytes)?;
        factors[usize::from(selector - 1)] = u16::from_le_bytes(bytes);
    }
    if factors.contains(&0) { return Err("ADC3 multiplier factor is zero".into()); }
    Ok((factors[0], factors[1]))
}
/// Read the exact raw factors again: equivalent reduced ratios still indicate drift.
fn verify_adc3_ratios(choice: &ClockChoice,
    mut read: impl FnMut(u8, u8, &mut [u8; 2]) -> Result<(), String>) -> Result<(), String> {
    for hop in &choice.path {
        if let ClockHopKind::RateMultiplier { numerator, denominator } = hop.kind {
            let mut actual = [0; 2];
            for (selector, expected) in [(1, numerator), (2, denominator)] {
                read(hop.id, selector, &mut actual)?;
                if u16::from_le_bytes(actual) != expected {
                    return Err("ADC3 clock multiplier changed; renegotiate".into());
                }
            }
        }
    }
    Ok(())
}
fn verify_adc3_route_ratios(route: &Adc3Route,
    mut read: impl FnMut(u8, u8, &mut [u8; 2]) -> Result<(), String>) -> Result<(), String> {
    if let Some(choice) = &route.choice { verify_adc3_ratios(choice, &mut read)?; }
    if let Some(context) = &route.clock_context {
        for peer in &context.peers {
            verify_adc3_ratios(&peer.original, &mut read)?;
            verify_adc3_ratios(&peer.target, &mut read)?;
        }
    }
    Ok(())
}
pub(super) fn verify_adc3_rate_route(handle: &DeviceHandle<Context>, route: &Adc3Route) -> Result<(), String> {
    verify_adc3_route_ratios(route, |id, selector, bytes|
        read_exact(handle, route.control, id, selector, 1, bytes))?;
    if let Some(choice) = &route.choice {
        for hop in &choice.path {
            if let ClockHopKind::Selector { selected_pin, .. } = hop.kind {
                let mut pin = [0];
                read_exact(handle, route.control, hop.id, 1, 1, &mut pin)?;
                if pin[0] != selected_pin { return Err("ADC3 clock selector changed; renegotiate".into()); }
            }
        }
    }
    Ok(())
}
fn adc3_project_ranges(choice: &ClockChoice, ranges: &[SamplingFrequencySubrange])
    -> Result<Vec<SamplingFrequencySubrange>, String> {
    let ratio = adc3_choice_ratio(choice)?;
    if ranges.is_empty() { return Err("ADC3 clock frequency ranges missing".into()); }
    let mut projected = Vec::new();
    for range in ranges {
        if range.min == 0 || range.min > range.max || (range.res == 0 && range.min != range.max) {
            return Err("ADC3 invalid clock frequency ranges".into());
        }
        if let Some((min, max, res)) = ratio.project_range(range.min, range.max, range.res) {
            projected.push(SamplingFrequencySubrange { min, max, res });
        }
    }
    Ok(projected)
}
/// Intersect inclusive AS limits without moving the clock progression's origin.
/// Empty limits mean the alternate accepts every available clock frequency.
fn adc3_limit_ranges(ranges: &[SamplingFrequencySubrange], limits: &[(u32, u32)])
    -> Vec<SamplingFrequencySubrange> {
    if limits.is_empty() { return ranges.to_vec(); }
    ranges.iter().flat_map(|range| {
        let start = limits.partition_point(|&(_, upper)| upper < range.min);
        limits[start..].iter().take_while(move |&&(lower, _)| lower <= range.max)
            .filter_map(move |&(lower, upper)| {
                let lower = range.min.max(lower);
                let upper = range.max.min(upper);
                if lower > upper { return None; }
                let (min, max) = if range.res == 0 {
                    if range.min != range.max { return None; }
                    (range.min, range.max)
                } else {
                    let origin = u64::from(range.min);
                    let step = u64::from(range.res);
                    let delta = u64::from(lower) - origin;
                    let min = origin + delta.div_ceil(step) * step;
                    let max = origin + (u64::from(upper) - origin) / step * step;
                    if min > max { return None; }
                    (u32::try_from(min).ok()?, u32::try_from(max).ok()?)
                };
                Some(SamplingFrequencySubrange { min, max, res: if min == max { 0 } else { range.res } })
            })
    }).collect()
}
fn adc3_route_limit_ranges(route: &Adc3Route, ranges: &[SamplingFrequencySubrange])
    -> Vec<SamplingFrequencySubrange> {
    let ranges = adc3_limit_ranges(ranges, &route.frequency_limits);
    match route.implicit.as_deref() {
        Some(peer) => adc3_route_limit_ranges(peer, &ranges),
        None => ranges,
    }
}
fn adc3_route_ranges(route: &Adc3Route, source_ranges: &[SamplingFrequencySubrange])
    -> Result<Vec<SamplingFrequencySubrange>, String> {
    let projected = adc3_project_ranges(route.choice.as_ref().ok_or("ADC3 clock path missing")?, source_ranges)?;
    Ok(adc3_route_limit_ranges(route, &projected))
}
fn validate_adc3_static_rate(route: &Adc3Route, rate: u32) -> Result<(), String> {
    let exact = SamplingFrequencySubrange { min: rate, max: rate, res: 0 };
    if rate == 0 || adc3_route_limit_ranges(route, &[exact]).is_empty() {
        return Err("ADC3 rate outside AS alternate frequency limits".into());
    }
    Ok(())
}
pub(super) fn adc3_wire_ranges(handle: &DeviceHandle<Context>, route: &Adc3Route)
    -> Result<Vec<SamplingFrequencySubrange>, String> {
    if matches!(route.profile, wire::FunctionProfile::Badd(_)) {
        return Ok(vec![SamplingFrequencySubrange { min: 48_000, max: 48_000, res: 0 }]);
    }
    // A planned path overlays selector targets that are not active yet. Its
    // raw factors must already match, while pin validation belongs to apply.
    if route.plan.is_some() {
        verify_adc3_route_ratios(route, |entity, selector, bytes|
            read_exact(handle, route.control, entity, selector, 1, bytes))?;
    } else {
        verify_adc3_rate_route(handle, route)?;
    }
    let source = route.clock.as_ref().ok_or("ADC3 clock source missing")?;
    adc3_route_ranges(route, &get_sampling_frequency_ranges(handle, route.control, source.id)?)
}
/// Full ADC3 adaptive-source explicit host-to-device rate pipe. The rate is
/// expressed per USB frame/microframe, independently of either pipe's interval.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct FeedforwardEndpoint {
    pub(super) address: u8,
    pub(super) interval: u8,
    pub(super) service_us: u32,
    pub(super) bytes: usize,
    pub(super) full_speed: bool,
}
impl FeedforwardEndpoint {
    pub(super) fn nominal(self, rate: u32) -> Result<Vec<u8>, String> {
        if !self.full_speed && self.bytes == 8 {
            // USB 3.2 section 4.4.8.4.1: unsigned Q32.32 per 125us bus
            // interval, rounded to nearest using integer arithmetic.
            let value = ((u64::from(rate) << 32) + 4000) / 8000;
            if rate == 0 || value == 0 {
                return Err("ADC3 feedforward nominal rate cannot be represented".into());
            }
            return Ok(value.to_le_bytes().to_vec());
        }
        // Match endpoint.c: round the nominal fixed-point rate first; FS
        // transmits that result shifted from Q16.16 to Q10.14.
        let (value, maximum) = if self.full_speed {
            ((((u64::from(rate) << 13) + 62) / 125) >> 2, 0x00ff_ffffu64)
        } else { (((u64::from(rate) << 10) + 62) / 125, 0x0fff_ffffu64) };
        if rate == 0 || value == 0 || value > maximum || self.bytes != if self.full_speed { 3 } else { 4 } {
            return Err("ADC3 feedforward nominal rate cannot be represented".into());
        }
        Ok((value as u32).to_le_bytes()[..self.bytes].to_vec())
    }
}
static NEGOTIATED: Lazy<Mutex<Option<Adc3Route>>> = Lazy::new(|| Mutex::new(None));
pub(super) fn clear_adc3_negotiation() {
    *NEGOTIATED.lock() = None;
}
pub(super) fn remember_adc3_route(route: Adc3Route) {
    *NEGOTIATED.lock() = Some(route);
}
pub(super) fn verify_adc3_route(route: &Adc3Route) -> Result<(), String> {
    if NEGOTIATED.lock().as_ref() != Some(route) {
        return Err("ADC3 configuration, cluster or clock route changed; negotiate again".into());
    }
    Ok(())
}

pub(super) fn functions(raw: &[u8]) -> Result<Vec<crate::descriptors::Iad>, String> {
    let mut result = Vec::new();
    let mut at = 0;
    while at < raw.len() {
        if raw.len() - at < 2 || raw[at] < 2 || usize::from(raw[at]) > raw.len() - at {
            return Err("malformed USB configuration descriptor chain".into());
        }
        let d = &raw[at..at + usize::from(raw[at])];
        if d[1] == 0x0b {
            let iad = crate::descriptors::parse_iad(d).map_err(|e| e.to_string())?;
            result.push(iad);
        }
        at += d.len();
    }
    Ok(result)
}
fn owning_function(
    config: &ConfigDescriptor,
    handle: &DeviceHandle<Context>,
    interface: u8,
) -> Result<(crate::descriptors::Iad, wire::FunctionProfile, Vec<u8>), String> {
    let raw = read_active_configuration_bytes(handle, config)
        .ok_or("ADC3 cannot read active configuration/IAD")?;
    let matches: Vec<_> = functions(&raw)?
        .into_iter()
        .filter(|iad| {
            u16::from(interface) >= u16::from(iad.b_first_interface)
                && u16::from(interface)
                    < u16::from(iad.b_first_interface) + u16::from(iad.b_interface_count)
        })
        .collect();
    let [iad] = matches.as_slice() else {
        return Err("ADC3 owning IAD missing or overlapping".into());
    };
    let profile = wire::function_profile(iad, 0x30).map_err(|e| e.to_string())?;
    let controls: Vec<_> = config
        .interfaces()
        .flat_map(|i| i.descriptors())
        .filter(|d| {
            d.interface_number() >= iad.b_first_interface
                && u16::from(d.interface_number())
                    < u16::from(iad.b_first_interface) + u16::from(iad.b_interface_count)
                && d.class_code() == 1
                && d.sub_class_code() == 1
                && d.setting_number() == 0
        })
        .collect();
    let [control] = controls.as_slice() else {
        return Err("ADC3 function must own exactly one AudioControl interface".into());
    };
    if control.interface_number() != iad.b_first_interface || control.protocol_code() != 0x30 {
        return Err("ADC3 AudioControl protocol/association mismatch".into());
    }
    if let wire::FunctionProfile::Badd(profile) = profile {
        validate_badd_function(config, iad, profile, resolve_device_speed(&handle.device()))?;
    }
    if profile == wire::FunctionProfile::Full {
        let bytes = control.extra();
        let mut offset = 0;
        let mut ids = HashSet::new();
        let mut header_seen = false;
        while offset < bytes.len() {
            if bytes.len() - offset < 3
                || bytes[offset] < 3
                || usize::from(bytes[offset]) > bytes.len() - offset
            {
                return Err("ADC3 malformed AudioControl descriptor chain".into());
            }
            let d = &bytes[offset..offset + usize::from(bytes[offset])];
            if d[1] == 0x24 {
                if d[2] == 1 {
                    if header_seen {
                        return Err("ADC3 duplicate AudioControl header".into());
                    }
                    let h = wire::parse_ac_header(d).map_err(|e| e.to_string())?;
                    if usize::from(h.total_length) > bytes.len() {
                        return Err(
                            "ADC3 AudioControl total length exceeds available descriptors".into(),
                        );
                    }
                    header_seen = true;
                } else if d.len() < 4 || d[3] == 0 || !ids.insert(d[3]) {
                    return Err("ADC3 invalid or duplicated entity ID".into());
                }
            }
            offset += d.len();
        }
        if !header_seen {
            return Err("ADC3 AudioControl header missing".into());
        }
    }
    Ok((iad.clone(), profile, control.extra().to_vec()))
}
/// Per-interface direction and synchronization evidence, not an inferred
/// cross-interface clock relation. BADD requires one common synchronization type.
#[derive(Clone, Copy)]
struct BaddInterfaceEvidence {
    capture: bool,
    asynchronous: bool,
}
fn validate_badd_inventory(
    profile: wire::BaddProfile,
    interfaces: &[BaddInterfaceEvidence],
) -> Result<(), String> {
    if interfaces.is_empty() || interfaces.len() > 2 {
        return Err("BADD invalid streaming interface inventory".into());
    }
    let capture = interfaces.iter().filter(|i| i.capture).count();
    let playback = interfaces.len() - capture;
    if capture > 1 || playback > 1 {
        return Err("BADD duplicate streaming direction".into());
    }
    if interfaces
        .iter()
        .any(|i| i.asynchronous != interfaces[0].asynchronous)
    {
        return Err("BADD function uses conflicting synchronization types".into());
    }
    let valid = match profile {
        wire::BaddProfile::GenericIo => true,
        wire::BaddProfile::Headphone | wire::BaddProfile::Speaker => playback == 1 && capture == 0,
        wire::BaddProfile::Microphone => capture == 1 && playback == 0,
        wire::BaddProfile::Headset
        | wire::BaddProfile::HeadsetAdapter
        | wire::BaddProfile::Speakerphone => capture == 1 && playback == 1,
    };
    if !valid {
        return Err("BADD streaming directions conflict with function profile".into());
    }
    Ok(())
}
fn validate_badd_function(
    config: &ConfigDescriptor,
    iad: &crate::descriptors::Iad,
    profile: wire::BaddProfile,
    speed: Speed,
) -> Result<(), String> {
    let speed_wire = match speed {
        Speed::Full => wire::BusSpeed::Full,
        Speed::High | Speed::Super | Speed::SuperPlus => wire::BusSpeed::High,
        _ => return Err("BADD playback requires known USB service geometry".into()),
    };
    let end = u16::from(iad.b_first_interface) + u16::from(iad.b_interface_count);
    let mut evidence = Vec::new();
    for interface in config.interfaces().filter(|i| {
        u16::from(i.number()) > u16::from(iad.b_first_interface) && u16::from(i.number()) < end
    }) {
        let alts: Vec<_> = interface.descriptors().collect();
        if alts
            .iter()
            .any(|d| d.class_code() != 1 || d.sub_class_code() != 2 || d.protocol_code() != 0x30)
        {
            return Err("BADD IAD contains a non-ADC3 streaming interface".into());
        }
        let zero: Vec<_> = alts.iter().filter(|d| d.setting_number() == 0).collect();
        if zero.len() != 1 || zero[0].endpoint_descriptors().next().is_some() {
            return Err("BADD zero-bandwidth alternate missing or nonempty".into());
        }
        let mandatory: Vec<_> = alts.iter().filter(|d| d.setting_number() == 1).collect();
        if mandatory.len() != 1 {
            return Err("BADD mandatory continuous alternate missing or repeated".into());
        }
        let mut direction = None;
        let mut common_sync = None;
        for alt in alts.iter().filter(|d| d.setting_number() != 0) {
            let data: Vec<_> = alt
                .endpoint_descriptors()
                .filter(|e| {
                    e.usage_type() == UsageType::Data
                        && e.transfer_type() == TransferType::Isochronous
                })
                .collect();
            let [ep] = data.as_slice() else {
                return Err(
                    "BADD alternate must have exactly one isochronous audio data endpoint".into(),
                );
            };
            let capture = ep.direction() == Direction::In;
            let asynchronous = match ep.sync_type() {
                SyncType::Asynchronous => true,
                SyncType::Synchronous => false,
                _ => return Err("BADD unsupported synchronization type".into()),
            };
            if direction.is_some_and(|previous| previous != capture) {
                return Err("BADD interface changes direction across alternates".into());
            }
            if common_sync.is_some_and(|previous| previous != asynchronous) {
                return Err("BADD interface changes synchronization across alternates".into());
            }
            direction = Some(capture);
            common_sync = Some(asynchronous);
            // Alt 1 is mandatory even if malformed. Additional continuous alts
            // use the same exact geometry/feedback contract; bursting remains
            // inventory-only and cannot become an operational candidate.
            let continuous_interval = if speed == Speed::Full { 1 } else { 4 };
            if alt.setting_number() == 1 || ep.interval() == continuous_interval {
                let (_, _, companion) = adc3_endpoint_geometry(ep, speed)?;
                badd_ss_continuous_geometry(ep.max_packet_size(), ep.interval(), companion)?;
                let (feedback, _) = adc3_feedback(alt, ep, speed)?;
                if !capture && explicit_feedback_ambiguous(alt, ep.synch_address(), ep.sync_type())
                {
                    return Err("BADD explicit feedback is ambiguous".into());
                }
                if !capture
                    && asynchronous
                    && !feedback.is_some_and(|f| {
                        f.transfer_type == TransferType::Isochronous
                            && feedback_packet_bytes_supported(speed, f.max_packet_bytes)
                    })
                {
                    return Err("BADD asynchronous sink requires a correctly sized isochronous feedback endpoint".into());
                }
                let size = ep.max_packet_size().to_le_bytes();
                let endpoint = [
                    7,
                    5,
                    ep.address(),
                    if asynchronous { 5 } else { 13 },
                    size[0],
                    size[1],
                    ep.interval(),
                ];
                wire::infer_badd_format(
                    profile,
                    speed_wire,
                    alt.setting_number(),
                    &endpoint,
                    feedback.is_some(),
                )
                .map_err(|e| e.to_string())?;
            }
        }
        evidence.push(BaddInterfaceEvidence {
            capture: direction.ok_or("BADD streaming interface has no active alternate")?,
            asynchronous: common_sync.unwrap(),
        });
    }
    if evidence.len() + 1 != usize::from(iad.b_interface_count) {
        return Err("BADD IAD interface membership is incomplete".into());
    }
    validate_badd_inventory(profile, &evidence)
}

fn read_exact(
    handle: &DeviceHandle<Context>,
    control: u8,
    entity: u8,
    selector: u8,
    request: u8,
    data: &mut [u8],
) -> Result<(), String> {
    let n = handle
        .read_control(
            0xa1,
            request,
            u16::from(selector) << 8,
            u16::from(control) | (u16::from(entity) << 8),
            data,
            Duration::from_secs(1),
        )
        .map_err(|e| format!("ADC3 control read: {e}"))?;
    if n != data.len() {
        return Err("ADC3 control response has wrong width".into());
    }
    Ok(())
}
fn write_exact(
    handle: &DeviceHandle<Context>,
    control: u8,
    entity: u8,
    selector: u8,
    data: &[u8],
) -> Result<(), String> {
    let n = handle
        .write_control(
            0x21,
            1,
            u16::from(selector) << 8,
            u16::from(control) | (u16::from(entity) << 8),
            data,
            Duration::from_secs(1),
        )
        .map_err(|e| format!("ADC3 control write: {e}"))?;
    if n != data.len() {
        return Err("ADC3 control write has wrong width".into());
    }
    Ok(())
}
pub(super) fn cluster(handle: &DeviceHandle<Context>, control: u8, id: u16) -> Result<wire::Cluster, String> {
    let mut bytes = [0; 6];
    let n = handle
        .read_control(
            0xa1,
            6,
            id,
            u16::from(control),
            &mut bytes,
            Duration::from_secs(1),
        )
        .map_err(|e| format!("ADC3 HC header: {e}"))?;
    let header =
        wire::parse_cluster_header(&bytes[..n], id, 16 * 1024).map_err(|e| e.to_string())?;
    let mut body = vec![0; usize::from(header.length)];
    let n = handle
        .read_control(
            0xa1,
            6,
            id,
            u16::from(control),
            &mut body,
            Duration::from_secs(1),
        )
        .map_err(|e| format!("ADC3 HC body: {e}"))?;
    wire::parse_cluster(&body[..n], header).map_err(|e| e.to_string())
}
fn linked_terminal_clock(descriptors: &[&[u8]], terminal: u8, capture: bool) -> Result<u8, String> {
    let terminals: Vec<_> = descriptors
        .iter()
        .filter(|d| d.get(2) == Some(&if capture { 3 } else { 2 }) && d.get(3) == Some(&terminal))
        .collect();
    let [d] = terminals.as_slice() else {
        return Err("ADC3 linked terminal missing or ambiguous".into());
    };
    let (terminal_type, id) = if capture {
        let term = wire::parse_output_terminal(d).map_err(|e| e.to_string())?;
        (term.terminal_type, term.clock_id)
    } else {
        let term = wire::parse_input_terminal(d).map_err(|e| e.to_string())?;
        (term.terminal_type, term.clock_id)
    };
    if terminal_type != 0x0101 {
        return Err("ADC3 linked terminal is not USB Streaming".into());
    }
    Ok(id)
}
fn clock_route(
    handle: &DeviceHandle<Context>,
    control: u8,
    terminal: u8,
    extra: &[u8],
    capture: bool,
    plan: Option<&ClockPlan>,
) -> Result<(wire::ClockSource, ClockChoice), String> {
    let descriptors: Vec<_> = DescriptorIter::new(extra)
        .filter(|d| d.get(1) == Some(&0x24))
        .collect();
    let mut id = linked_terminal_clock(&descriptors, terminal, capture)?;
    let mut path = vec![ClockHop {
        id: terminal,
        kind: ClockHopKind::Terminal,
    }];
    for _ in 0..16 {
        if path.iter().any(|h| h.id == id) {
            return Err("ADC3 clock cycle".into());
        }
        let nodes: Vec<_> = descriptors
            .iter()
            .filter(|d| d.get(3) == Some(&id))
            .collect();
        let [d] = nodes.as_slice() else {
            return Err("ADC3 clock ID missing or duplicated".into());
        };
        match d[2] {
            0x0b => {
                let source = wire::parse_clock_source(d).map_err(|e| e.to_string())?;
                if !matches!(
                    source.controls.access(1),
                    wire::ControlAccess::ReadOnly | wire::ControlAccess::ReadWrite
                ) {
                    return Err("ADC3 sample frequency is not readable".into());
                }
                path.push(ClockHop {
                    id,
                    kind: ClockHopKind::Source,
                });
                return Ok((
                    source,
                    ClockChoice {
                        control_interface: control,
                        source_id: id,
                        path,
                        resolution: ClockResolution::Resolved,
                    },
                ));
            }
            0x0c => {
                let selector = wire::parse_clock_selector(d).map_err(|e| e.to_string())?;
                if !matches!(
                    selector.controls.access(1),
                    wire::ControlAccess::ReadOnly | wire::ControlAccess::ReadWrite
                ) {
                    return Err("ADC3 selector is not readable".into());
                }
                let mut pin = [0];
                if let Some(saved) = plan.and_then(|p| p.selectors.iter().find(|p| p.selector == id)) {
                    pin[0] = saved.target;
                } else {
                    read_exact(handle, control, id, 1, 1, &mut pin)?;
                }
                let selected = *selector
                    .sources
                    .get(usize::from(pin[0]).wrapping_sub(1))
                    .ok_or("ADC3 selector pin out of range")?;
                path.push(ClockHop {
                    id,
                    kind: ClockHopKind::Selector {
                        selected_pin: pin[0],
                        pin_count: selector.sources.len() as u8,
                    },
                });
                id = selected;
            }
            0x0d => {
                let multiplier = wire::parse_clock_multiplier(d).map_err(|e| e.to_string())?;
                let (numerator, denominator) = adc3_multiplier_factors(multiplier.controls,
                    |selector, bytes| read_exact(handle, control, id, selector, 1, bytes))?;
                path.push(ClockHop { id, kind: ClockHopKind::RateMultiplier { numerator, denominator } });
                id = multiplier.source_id;
            }
            _ => return Err("ADC3 terminal points to unsupported clock entity".into()),
        }
    }
    Err("ADC3 clock path too deep".into())
}
fn domains(
    profile: wire::FunctionProfile,
    extra: &[u8],
    capture: bool,
) -> Result<Vec<wire::PowerDomain>, String> {
    if matches!(profile, wire::FunctionProfile::Badd(_)) {
        return Ok(vec![wire::PowerDomain {
            id: if capture { 11 } else { 10 },
            recovery_d1: 600,
            recovery_d2: 6000,
            entities: if capture {
                vec![4, 5, 6]
            } else {
                vec![1, 2, 3]
            },
            string_id: 0,
        }]);
    }
    let mut result = Vec::new();
    let mut members = HashSet::new();
    let mut ids = HashSet::new();
    for d in
        DescriptorIter::new(extra).filter(|d| d.get(1) == Some(&0x24) && d.get(2) == Some(&0x10))
    {
        let domain = wire::parse_power_domain(d).map_err(|e| e.to_string())?;
        if !ids.insert(domain.id) || domain.entities.iter().any(|id| !members.insert(*id)) {
            return Err("ADC3 power domains overlap or repeat".into());
        }
        result.push(domain);
    }
    Ok(result)
}
pub(super) fn adc3_route_for_candidate(
    device: &Device<Context>,
    handle: &DeviceHandle<Context>,
    candidate: &IsoStreamCandidate,
) -> Result<Option<Adc3Route>, String> {
    let config = device
        .active_config_descriptor()
        .map_err(|e| e.to_string())?;
    let alt = config
        .interfaces()
        .flat_map(|i| i.descriptors())
        .find(|d| {
            d.interface_number() == candidate.interface_number
                && d.setting_number() == candidate.alt_setting
        })
        .ok_or("selected interface disappeared")?;
    if alt.protocol_code() != 0x30 {
        return Ok(None);
    }
    let saved = NEGOTIATED.lock().clone();
    let plan = saved.as_ref().filter(|r| r.config == config.number()
        && r.interface == candidate.interface_number && r.alt == candidate.alt_setting)
        .and_then(|r| r.plan.as_ref());
    let (_, mut route, _) = adc3_pair_for_source(
        adc3_alternate_pairs(&config, handle, &alt, candidate.usb_speed, false, plan)?,
        candidate.implicit_feedback)
        .ok_or("ADC3 implicit source changed since selection")?;
    route.clock_context = saved.as_ref().filter(|saved| saved.plan.as_ref() == route.plan.as_ref())
        .and_then(|saved| saved.clock_context.clone());
    route.live_alternate = saved.as_ref().is_some_and(|saved| saved.config == route.config
        && saved.interface == route.interface && saved.alt == route.alt && saved.live_alternate);
    Ok(Some(route))
}
fn adc3_alternate(
    config: &ConfigDescriptor,
    handle: &DeviceHandle<Context>,
    alt: &rusb::InterfaceDescriptor,
    speed: Speed,
    capture: bool,
) -> Result<(IsoStreamCandidate, Adc3Route), String> {
    adc3_alternate_plan(config, handle, alt, speed, capture, None)
}

fn adc3_endpoint_geometry(
    ep: &rusb::EndpointDescriptor,
    speed: Speed,
) -> Result<(usize, u32, Option<[u8; 6]>), String> {
    if ep.address() & 0x0f == 0 || ep.address() & 0x70 != 0 {
        return Err("ADC3 invalid endpoint address".into());
    }
    if matches!(speed, Speed::Super | Speed::SuperPlus) {
        let (capacity, service, companion) = ss_endpoint_geometry(
            ep.max_packet_size(), ep.interval(), ep.extra().unwrap_or(&[]), speed,
            ep.transfer_type() == TransferType::Isochronous,
        )?;
        return Ok((capacity, service, Some(companion)));
    }
    let packet = ep.max_packet_size();
    let base = packet & 0x7ff;
    if !(1..=16).contains(&ep.interval()) || !matches!(speed, Speed::Full | Speed::High)
        || packet & 0xe000 != 0 || base == 0
        || (speed == Speed::Full && (base > 1023 || packet & 0x1800 != 0))
        || (speed == Speed::High && (base > 1024 || (packet >> 11) & 3 == 3))
    {
        return Err("ADC3 invalid audio endpoint packet geometry".into());
    }
    Ok((effective_iso_packet_bytes(packet, speed), service_interval_micros(speed, ep.interval()), None))
}


fn adc3_feedback(
    alt: &rusb::InterfaceDescriptor,
    ep: &rusb::EndpointDescriptor,
    speed: Speed,
) -> Result<(Option<FeedbackEndpoint>, Option<[u8; 6]>), String> {
    let mut feedback = find_feedback_endpoint(alt, ep.synch_address(), ep.sync_type(), speed);
    let mut companion = None;
    if matches!(speed, Speed::Super | Speed::SuperPlus) {
        if let Some(ref mut feedback) = feedback {
            let endpoint = alt.endpoint_descriptors().find(|e| e.address() == feedback.address)
                .ok_or("ADC3 feedback endpoint disappeared")?;
            let (capacity, service, descriptor) = ss_feedback_geometry(
                endpoint.max_packet_size(), endpoint.interval(), endpoint.extra().unwrap_or(&[]),
                endpoint.transfer_type() == TransferType::Isochronous,
            )?;
            if capacity == 8 {
                validate_enhanced_feedback_shape(ep.address(), endpoint.address(),
                    endpoint.transfer_type(), endpoint.sync_type(), endpoint.usage_type())?;
            }
            feedback.max_packet_bytes = capacity;
            feedback.service_interval_us = service;
            companion = Some(descriptor);
        }
    }
    Ok((feedback, companion))
}

fn adc3_feedforward(config: &ConfigDescriptor, handle: &DeviceHandle<Context>,
    alt: &rusb::InterfaceDescriptor, data: &rusb::EndpointDescriptor, speed: Speed,
) -> Result<(FeedforwardEndpoint, Option<[u8; 6]>), String> {
    if settings().disable_feedback() { return Err("ADC3 adaptive capture requires enabled feedforward".into()); }
    // Section 4.8.2: exactly one explicit, same-AS OUT Feedback/NoSync pipe.
    // Reject an implied adaptive playback pairing and any ambiguous inventory.
    let endpoints: Vec<_> = alt.endpoint_descriptors().collect();
    let feedback: Vec<_> = endpoints.iter().filter(|e| e.usage_type() == UsageType::Feedback).collect();
    let [ep] = feedback.as_slice() else { return Err("ADC3 adaptive capture requires unique explicit feedforward OUT".into()); };
    if endpoints.len() != 2 || ep.direction() != Direction::Out
        || ep.transfer_type() != TransferType::Isochronous || ep.sync_type() != SyncType::NoSync {
        return Err("ADC3 adaptive capture feedforward endpoint has invalid direction/type/synchronization".into());
    }
    let raw = read_active_configuration_bytes(handle, config).ok_or("ADC3 cannot verify feedforward descriptor")?;
    let mut selected = false;
    let mut found = 0;
    let mut at = 0;
    while at < raw.len() {
        let n = usize::from(*raw.get(at).ok_or("ADC3 truncated descriptor")?);
        if n < 2 || n > raw.len() - at { return Err("ADC3 malformed feedforward descriptor chain".into()); }
        let d = &raw[at..at+n];
        if d[1] == 4 {
            selected = d.len() >= 9 && d[2] == alt.interface_number() && d[3] == alt.setting_number();
        } else if selected && d[1] == 5 && d.get(2) == Some(&ep.address()) {
            validate_feedforward_descriptor(d, speed)?;
            found += 1;
        }
        at += n;
    }
    if found != 1 { return Err("ADC3 feedforward standard endpoint missing or duplicated".into()); }
    let (capacity, service, companion) = adc3_endpoint_geometry(ep, speed)?;
    let bytes = usize::from(ep.max_packet_size());
    if capacity != bytes || !feedback_packet_bytes_supported(speed, bytes) {
        return Err("ADC3 feedforward requires exact three/four/eight-byte unburst capacity".into());
    }
    if let Some(d) = companion {
        if d[2] != 0 || d[3] != 0 { return Err("ADC3 feedforward bursting is unsupported".into()); }
    }
    if bytes == 8 {
        validate_enhanced_feedback_shape(data.address(), ep.address(),
            ep.transfer_type(), ep.sync_type(), ep.usage_type())?;
    }
    Ok((FeedforwardEndpoint { address: ep.address(), interval: ep.interval(), service_us: service,
        bytes, full_speed: speed == Speed::Full }, companion))
}
fn adaptive_implicit_inventory(profile: wire::FunctionProfile, duplex: bool,
    endpoints: usize, synch_address: u8, disabled: bool) -> bool {
    profile == wire::FunctionProfile::Full && duplex && endpoints == 1
        && synch_address == 0 && !disabled
}
// BADD Table 6-25 specifies 3/4/8 bytes at FS/HS/SS respectively. Preserve
// legacy four-byte SuperSpeed compatibility after exact companion validation.
fn validate_enhanced_feedback_shape(data: u8, report: u8, transfer: TransferType,
    sync: SyncType, usage: UsageType) -> Result<(), String> {
    // USB 3.2 section 9.6.6: opposite direction with the same endpoint number.
    if report != (data ^ 0x80) || transfer != TransferType::Isochronous
        || sync != SyncType::NoSync || usage != UsageType::Feedback {
        return Err("ADC3 Enhanced SuperSpeed feedback requires opposite-direction matching endpoint number and Isochronous/NoSync/Feedback shape".into());
    }
    Ok(())
}
fn feedback_packet_bytes_supported(speed: Speed, bytes: usize) -> bool {
    match speed {
        Speed::Full => bytes == 3,
        Speed::High => bytes == 4,
        Speed::Super | Speed::SuperPlus => matches!(bytes, 4 | 8),
        _ => false,
    }
}
fn validate_feedforward_descriptor(d: &[u8], speed: Speed) -> Result<(), String> {
    if d.len() != 7 || d[0] != 7 || d[1] != 5 || d[2] & 0xf0 != 0 || d[2] & 15 == 0
        || d[3] != 0x11 || !feedback_packet_bytes_supported(speed, usize::from(u16::from_le_bytes([d[4], d[5]])))
        || !(1..=16).contains(&d[6]) {
        return Err("ADC3 malformed feedforward standard endpoint".into());
    }
    Ok(())
}

fn badd_ss_continuous_geometry(packet: u16, interval: u8, companion: Option<[u8; 6]>) -> Result<(), String> {
    if let Some(d) = companion {
        if interval != 4 || d[2] != 0 || d[3] != 0
            || u16::from_le_bytes([d[4], d[5]]) != packet
        {
            return Err("BADD SuperSpeed requires exact unburst continuous 1ms geometry".into());
        }
    }
    Ok(())
}

fn adc3_alternate_plan(
    config: &ConfigDescriptor,
    handle: &DeviceHandle<Context>,
    alt: &rusb::InterfaceDescriptor,
    speed: Speed,
    capture: bool,
    plan: Option<&ClockPlan>,
) -> Result<(IsoStreamCandidate, Adc3Route), String> {
    adc3_alternate_overlay(config, handle, alt, speed, capture, plan, true, false)
}

// A peer may follow the sink's shared selector targets without owning its plan.
fn adc3_alternate_base(
    config: &ConfigDescriptor, handle: &DeviceHandle<Context>, alt: &rusb::InterfaceDescriptor,
    speed: Speed, capture: bool, plan: Option<&ClockPlan>, verify_plan: bool, allow_implicit_feedforward: bool,
) -> Result<(IsoStreamCandidate, Adc3Route, Vec<SamplingFrequencySubrange>, bool), String> {
    let (iad, profile, extra) = owning_function(config, handle, alt.interface_number())?;
    let control = iad.b_first_interface;
    if profile == wire::FunctionProfile::Full && alt.endpoint_descriptors().filter(|e|
        e.transfer_type() == TransferType::Isochronous
            && matches!(e.usage_type(), UsageType::Data | UsageType::FeedbackData)).count() != 1 {
        return Err("ADC3 AS alternate must own exactly one audio data endpoint".into());
    }
    let endpoints: Vec<_> = alt
        .endpoint_descriptors()
        .filter(|e| {
            e.direction()
                == (if capture {
                    Direction::In
                } else {
                    Direction::Out
                })
                && e.transfer_type() == TransferType::Isochronous
                && (e.usage_type() == UsageType::Data
                    || (profile == wire::FunctionProfile::Full
                        && e.usage_type() == UsageType::FeedbackData
                        && (capture || e.sync_type() == SyncType::Adaptive)))
        })
        .collect();
    let [ep] = endpoints.as_slice() else {
        return Err("ADC3 alternate must have one data endpoint in the requested direction".into());
    };
    if !capture && ep.sync_type() == SyncType::Adaptive && ep.usage_type() == UsageType::FeedbackData
        && (alt.endpoint_descriptors().count() != 1 || ep.synch_address() != 0) {
        return Err("ADC3 marked adaptive OUT must have no extra feedback endpoints".into());
    }
    let (packet_bytes, service_us, data_companion) = adc3_endpoint_geometry(ep, speed)?;
    if matches!(profile, wire::FunctionProfile::Badd(_)) {
        badd_ss_continuous_geometry(ep.max_packet_size(), ep.interval(), data_companion)?;
    }
    if !capture && explicit_feedback_ambiguous(alt, ep.synch_address(), ep.sync_type()) {
        return Err("ADC3 explicit feedback ambiguous".into());
    }
    let (feedback, feedback_companion) = if capture {
        (None, None)
    } else {
        adc3_feedback(alt, ep, speed)?
    };
    if !adc3_sync_supported(profile, capture, ep.sync_type()) {
        return Err("ADC3 synchronization unsupported".into());
    }
    let implicit_feedforward = capture && ep.sync_type() == SyncType::Adaptive
        && adaptive_implicit_inventory(profile, allow_implicit_feedforward,
            alt.endpoint_descriptors().count(), ep.synch_address(), settings().disable_feedback());
    let (feedforward, feedforward_companion) = if capture && ep.sync_type() == SyncType::Adaptive {
        if settings().disable_feedback() { return Err("ADC3 adaptive capture requires enabled feedforward".into()); }
        if implicit_feedforward { (None, None) } else {
            let (pipe, companion) = adc3_feedforward(config, handle, alt, ep, speed)?;
            (Some(pipe), companion)
        }
    } else { (None, None) };
    let mut route = Adc3Route {
        config: config.number(),
        control,
        first: iad.b_first_interface,
        count: iad.b_interface_count,
        profile,
        interface: alt.interface_number(),
        alt: alt.setting_number(),
        terminal: if capture { 6 } else { 1 },
        endpoint: (
            ep.address(),
            ep.interval(),
            ep.max_packet_size(),
            service_interval_micros(speed, ep.interval()),
        ),
        packet_bytes,
        sync_type: ep.sync_type(),
        usage_type: ep.usage_type(),
        feedback: feedback.map(|f| (f.address, f.interval, f.max_packet_bytes)),
        feedforward,
        implicit_feedforward,
        implicit: None,
        companions: data_companion.map(|d| (ep.address(), d)).into_iter()
            .chain(feedback_companion.zip(feedback).map(|(d, f)| (f.address, d)))
            .chain(feedforward_companion.zip(feedforward).map(|(d, f)| (f.address, d)))
            .collect(),
        extended_companions: data_companion.filter(|d| d[3] & 0x80 != 0)
            .map(|_| (ep.address(), ep.extra().unwrap()[6..14].try_into().unwrap()))
            .into_iter().collect(),
        pcm: (0, 0, 0),
        frequency_limits: if profile == wire::FunctionProfile::Full {
            wire::parse_as_frequency_ranges(alt.extra()).map_err(|e| e.to_string())?
        } else { vec![] },
        cluster: None,
        header: None,
        clock: None,
        choice: None,
        domains: domains(profile, &extra, capture)?,
        plan: plan.cloned(),
        clock_context: None,
        live_alternate: false,
    };
    let (channels, subslot, bits, rates) = match profile {
        wire::FunctionProfile::Badd(profile) => {
            let p = ep.max_packet_size().to_le_bytes();
            let endpoint = [
                7,
                5,
                ep.address(),
                if ep.sync_type() == SyncType::Asynchronous {
                    5
                } else {
                    13
                },
                p[0],
                p[1],
                ep.interval(),
            ];
            let f = wire::infer_badd_format(
                profile,
                if speed == Speed::Full {
                    wire::BusSpeed::Full
                } else {
                    wire::BusSpeed::High
                },
                alt.setting_number(),
                &endpoint,
                feedback.is_some(),
            )
            .map_err(|e| e.to_string())?;
            (
                u16::from(f.channels),
                f.subslot_size,
                f.bit_resolution,
                vec![SamplingFrequencySubrange {
                    min: 48000,
                    max: 48000,
                    res: 0,
                }],
            )
        }
        wire::FunctionProfile::Full => {
            let headers: Vec<_> = DescriptorIter::new(alt.extra())
                .filter(|d| d.get(1) == Some(&0x24) && d.get(2) == Some(&1))
                .collect();
            let [h] = headers.as_slice() else {
                return Err("ADC3 AS header missing or ambiguous".into());
            };
            let h = wire::parse_as_header(h).map_err(|e| e.to_string())?;
            if h.formats & 1 == 0
                || h.auxiliary_protocols != 0
                || h.control_size != 0
                || !matches!(h.subslot_size, 2..=4)
                || !matches!(h.bit_resolution, 16 | 24 | 32)
            {
                return Err("ADC3 alternate has no implemented standard PCM shape".into());
            }
            let c = cluster(handle, control, h.cluster_id)?;
            if c.channels.is_empty() {
                return Err("ADC3 cluster has no channels".into());
            }
            let (source, choice) = clock_route(handle, control, h.terminal_link, &extra, capture, plan)?;
            if let Some(plan) = plan.filter(|_| verify_plan) {
                if source.id != plan.source_id || source.controls.0 != plan.source_controls
                    || choice.path.iter().skip(1).map(|h| h.id).collect::<Vec<_>>() != plan.path {
                    return Err("ADC3 alternate clock topology changed".into());
                }
            }
            let ranges = get_sampling_frequency_ranges(handle, control, source.id)?;
            if ranges.is_empty() || ranges.iter().any(|r| r.min == 0 || r.min > r.max) {
                return Err("ADC3 invalid clock frequency ranges".into());
            }
            let result = (
                c.channels.len() as u16,
                h.subslot_size,
                h.bit_resolution,
                adc3_limit_ranges(&adc3_project_ranges(&choice, &ranges)?, &route.frequency_limits),
            );
            route.terminal = h.terminal_link;
            route.cluster = Some(c);
            route.header = Some(h);
            route.clock = Some(source);
            route.choice = Some(choice);
            result
        }
    };
    route.pcm = (channels, subslot, bits);
    let candidate = IsoStreamCandidate {
        configuration_number: config.number(),
        interface_number: alt.interface_number(),
        alt_setting: alt.setting_number(),
        endpoint_address: ep.address(),
        endpoint_interval: ep.interval(),
        service_interval_us: service_us,
        usb_speed: speed,
        max_packet_bytes: packet_bytes,
        sync_type: ep.sync_type(),
        usage_type: ep.usage_type(),
        format_tag: FORMAT_TAG_PCM,
        uac2_formats: None,
        native_dsd: None,
        channels,
        subslot_size: subslot,
        bit_resolution: bits,
        sample_rates: Vec::new(),
        refresh: ep.refresh(),
        synch_address: ep.synch_address(),
        feedback_endpoint: feedback,
        implicit_feedback: None,
        terminal_link: Some(route.terminal),
        is_uac1: false,
    };
    let implicit = adc3_requires_implicit(profile, capture, candidate.sync_type, feedback.is_some(),
        ep.synch_address() != 0 || alt.endpoint_descriptors().any(|e| e.usage_type() == UsageType::Feedback),
        settings().disable_feedback())?;
    Ok((candidate, route, rates, implicit))
}

/// Parse the sink once, then retain every feasible alternate of its one proven IN pipe.
fn adc3_alternate_pairs(
    config: &ConfigDescriptor, handle: &DeviceHandle<Context>, alt: &rusb::InterfaceDescriptor,
    speed: Speed, capture: bool, plan: Option<&ClockPlan>,
) -> Result<Vec<(IsoStreamCandidate, Adc3Route, Vec<SamplingFrequencySubrange>)>, String> {
    let (candidate, route, rates, implicit) =
        adc3_alternate_base(config, handle, alt, speed, capture, plan, true, false)?;
    if !implicit {
        let rates = adc3_capacity_ranges(&candidate, &rates);
        let mut candidate = candidate;
        candidate.sample_rates = representative_sample_rates_from_ranges(&rates);
        return Ok(vec![(candidate, route, rates)]);
    }
    let sources = adc3_implicit_sources(config, handle, &candidate, &route, &rates, plan)?;
    Ok(adc3_expand_implicit_pairs(&candidate, &route, &rates, sources))
}

fn adc3_expand_implicit_pairs(
    candidate: &IsoStreamCandidate, route: &Adc3Route, rates: &[SamplingFrequencySubrange],
    sources: Vec<(ImplicitFeedbackSource, Adc3Route)>,
) -> Vec<(IsoStreamCandidate, Adc3Route, Vec<SamplingFrequencySubrange>)> {
    sources.into_iter().map(|(source, source_route)| {
        let mut candidate = candidate.clone();
        candidate.implicit_feedback = Some(source);
        let ranges = adc3_capacity_ranges(&candidate, &adc3_route_limit_ranges(&source_route, rates));
        candidate.sample_rates = representative_sample_rates_from_ranges(&ranges);
        let mut route = route.clone();
        route.implicit = Some(Box::new(source_route));
        (candidate, route, ranges)
    }).collect()
}

fn adc3_pair_for_source(
    pairs: Vec<(IsoStreamCandidate, Adc3Route, Vec<SamplingFrequencySubrange>)>,
    source: Option<ImplicitFeedbackSource>,
) -> Option<(IsoStreamCandidate, Adc3Route, Vec<SamplingFrequencySubrange>)> {
    pairs.into_iter().find(|(candidate, _, _)| candidate.implicit_feedback == source)
}

// Capture keeps its single-alternate path, including duplex-only feedforward.
fn adc3_alternate_overlay(
    config: &ConfigDescriptor, handle: &DeviceHandle<Context>, alt: &rusb::InterfaceDescriptor,
    speed: Speed, capture: bool, plan: Option<&ClockPlan>, verify_plan: bool, allow_implicit_feedforward: bool,
) -> Result<(IsoStreamCandidate, Adc3Route), String> {
    if !capture {
        return adc3_alternate_pairs(config, handle, alt, speed, false, plan)?
            .into_iter().next().map(|(candidate, route, _)| (candidate, route))
            .ok_or_else(|| "ADC3 implicit feedback source missing".into());
    }
    let (mut candidate, route, rates, _) = adc3_alternate_base(
        config, handle, alt, speed, true, plan, verify_plan, allow_implicit_feedforward)?;
    candidate.sample_rates = representative_sample_rates_from_ranges(&adc3_capacity_ranges(&candidate, &rates));
    Ok((candidate, route))
}

fn adc3_requires_implicit(profile: wire::FunctionProfile, capture: bool, sync: SyncType,
    explicit: bool, advertised_explicit: bool, disabled: bool) -> Result<bool, String> {
    if capture || sync != SyncType::Asynchronous { return Ok(false); }
    if disabled { return Err("ADC3 asynchronous playback requires enabled feedback".into()); }
    if explicit { return Ok(false); }
    // A malformed advertised explicit pipe cannot authorize a different source.
    if advertised_explicit { return Err("ADC3 advertised explicit feedback is unusable".into()); }
    if profile != wire::FunctionProfile::Full {
        return Err("BADD asynchronous playback requires explicit feedback".into());
    }
    Ok(true)
}

fn adc3_route_domain_required(route: &Adc3Route, domain: &wire::PowerDomain) -> bool {
    domain.entities.contains(&route.terminal) || route.choice.as_ref().map_or(false, |choice|
        choice.path.iter().any(|hop| domain.entities.contains(&hop.id)))
}

/// Keep the implicit source's terminal and clock powered until both alts stop.
fn prepare_adc3_implicit_power(claimed: &ClaimedHandle, route: &Adc3Route) -> Result<(), String> {
    let Some(source) = route.implicit.as_deref() else { return Ok(()); };
    if !claimed.claimed_interfaces.contains(&source.interface) {
        return Err("ADC3 implicit source interface was not claimed".into());
    }
    claimed.adc3_power.verify_idle(source.interface)?;
    for domain in source.domains.iter().filter(|domain| adc3_route_domain_required(source, domain)) {
        claimed.adc3_power.acquire(source.control, domain)?;
    }
    Ok(())
}

/// Re-read the current peer path after selector/rate programming, before IN activation.
/// Compare with the retained negotiated target, never choose a replacement peer.
pub(super) fn verify_adc3_implicit_activation(claimed: &ClaimedHandle, candidate: &IsoStreamCandidate,
    route: &Adc3Route, rate: u32) -> Result<(), String> {
    let Some(expected) = route.implicit.as_deref() else { return Ok(()); };
    if settings().disable_feedback() || !claimed.claimed_interfaces.contains(&expected.interface) {
        return Err("ADC3 implicit source disabled or unclaimed".into());
    }
    let config = claimed.handle.device().active_config_descriptor().map_err(|e| e.to_string())?;
    let alt = config.interfaces().flat_map(|i| i.descriptors()).find(|d|
        d.interface_number() == expected.interface && d.setting_number() == expected.alt)
        .ok_or("ADC3 implicit source alternate disappeared")?;
    let (input, actual) = adc3_alternate(&config, &claimed.handle, &alt, candidate.usb_speed, true)?;
    if &actual != expected || input.sync_type != SyncType::Asynchronous
        || input.usage_type != UsageType::FeedbackData || input.service_interval_us != candidate.service_interval_us
        || !adc3_shared_clock(route, &actual)
        || input.max_packet_bytes < required_max_packet_bytes(rate, input.service_interval_us,
            usize::from(input.channels) * usize::from(input.subslot_size)) {
        return Err("ADC3 implicit source signature or clock changed before activation".into());
    }
    for domain in expected.domains.iter().filter(|domain| adc3_route_domain_required(expected, domain)) {
        if PowerUsb(&claimed.adc3_power).read(expected.control, domain.id)? != 0 {
            return Err("ADC3 implicit source power changed before activation".into());
        }
    }
    validate_adc3_clock(&claimed.handle, &actual, rate, false)
}

/// Deduplicate clock-target proofs only when every viable target names one pipe.
fn adc3_preflight_pipe(proofs: &[ImplicitFeedbackSource]) -> Option<u8> {
    let first = proofs.first()?;
    proofs.iter().all(|source|
        (source.interface_number, source.address) == (first.interface_number, first.address))
        .then_some(first.interface_number)
}

/// Read-only Android claim preflight. Requests target only the already owned AC.
/// A writable selector target may prove the pair before Android claims its IN AS.
pub(super) fn adc3_feedback_interfaces(config: &ConfigDescriptor, handle: &DeviceHandle<Context>,
    speed: Speed, claimed: &[u8]) -> Vec<u8> {
    if settings().disable_feedback() { return Vec::new(); }
    let mut result = Vec::new();
    for alt in config.interfaces().flat_map(|i| i.descriptors()).filter(|d|
        d.class_code() == 1 && d.sub_class_code() == 2 && d.protocol_code() == 0x30
            && d.setting_number() != 0 && claimed.contains(&d.interface_number())) {
        let Ok((iad, profile, extra)) = owning_function(config, handle, alt.interface_number()) else { continue; };
        if profile != wire::FunctionProfile::Full || !claimed.contains(&iad.b_first_interface) { continue; }
        if let Ok((candidate, _)) = adc3_alternate(config, handle, &alt, speed, false) {
            if !candidate.sample_rates.is_empty() {
                if let Some(source) = candidate.implicit_feedback { result.push(source.interface_number); }
            }
            // Preserve current exact routes and explicit-feedback preference.
            continue;
        }
        let endpoints: Vec<_> = alt.endpoint_descriptors().filter(|e|
            e.direction() == Direction::Out && e.transfer_type() == TransferType::Isochronous
                && e.usage_type() == UsageType::Data).collect();
        let [endpoint] = endpoints.as_slice() else { continue; };
        if endpoint.sync_type() != SyncType::Asynchronous
            || explicit_feedback_ambiguous(&alt, endpoint.synch_address(), endpoint.sync_type()) { continue; }
        let Ok((explicit, _)) = adc3_feedback(&alt, endpoint, speed) else { continue; };
        if adc3_requires_implicit(profile, false, endpoint.sync_type(), explicit.is_some(),
            endpoint.synch_address() != 0 || alt.endpoint_descriptors().any(|e| e.usage_type() == UsageType::Feedback),
            false) != Ok(true) { continue; }
        let headers: Vec<_> = DescriptorIter::new(alt.extra())
            .filter(|d| d.get(1) == Some(&0x24) && d.get(2) == Some(&1)).collect();
        let [header] = headers.as_slice() else { continue; };
        let Ok(header) = wire::parse_as_header(header) else { continue; };
        let Ok(plans) = adc3_plans(handle, iad.b_first_interface, header.terminal_link, &extra) else { continue; };
        // Inspect every viable plan within the existing eight-target policy.
        // Truncation could hide a conflicting source pipe, so fail closed.
        if plans.len() > 8 { continue; }
        let mut proofs = Vec::new();
        for plan in plans.iter().filter(|p| p.selectors.iter().any(|pin| pin.original != pin.target)) {
            if let Ok((candidate, _)) = adc3_alternate_plan(config, handle, &alt, speed, false, Some(plan)) {
                if !candidate.sample_rates.is_empty() {
                    if let Some(source) = candidate.implicit_feedback { proofs.push(source); }
                }
            }
        }
        if let Some(interface) = adc3_preflight_pipe(&proofs) { result.push(interface); }
    }
    result.sort_unstable(); result.dedup(); result
}

/// ADC3 section 3.11.4.1 permits nominal SOF pacing for an Adaptive sink.
/// Full adaptive sources additionally require the explicit feedforward proof above.
fn adc3_sync_supported(profile: wire::FunctionProfile, _capture: bool, sync: SyncType) -> bool {
    matches!(sync, SyncType::Asynchronous | SyncType::Synchronous)
        || (profile == wire::FunctionProfile::Full && sync == SyncType::Adaptive)
}

fn adc3_shared_clock(output: &Adc3Route, input: &Adc3Route) -> bool {
    if output.profile != wire::FunctionProfile::Full || input.profile != wire::FunctionProfile::Full
        || (output.config, output.first, output.count, output.control) != (input.config, input.first, input.count, input.control)
        || output.clock.is_none() || output.clock != input.clock {
        return false;
    }
    let (Some(out), Some(input)) = (&output.choice, &input.choice) else { return false; };
    out.resolution == ClockResolution::Resolved && input.resolution == ClockResolution::Resolved
        && out.source_id == input.source_id
        && adc3_choice_ratio(out).ok().zip(adc3_choice_ratio(input).ok()).is_some_and(|(a, b)| a == b)
        && out.path.iter().all(|left| input.path.iter().all(|right|
            left.id != right.id || left.kind == right.kind))
}

/// Full ADC3 3.11.4.3: the selected marked adaptive sink supplies device
/// feedforward timing. Service intervals may differ; no packet sizes are copied.
pub(super) fn validate_adaptive_duplex(output: &IsoStreamCandidate, output_route: &Adc3Route,
    output_rate: u32, input_route: &Adc3Route, input_rate: u32) -> Result<(), String> {
    if !input_route.implicit_feedforward { return Ok(()); }
    let capacity = |route: &Adc3Route, rate| route.packet_bytes >= required_max_packet_bytes(
        rate, route.endpoint.3, usize::from(route.pcm.0) * usize::from(route.pcm.1));
    if settings().disable_feedback() || output.sync_type != SyncType::Adaptive
        || output.usage_type != UsageType::FeedbackData || output.endpoint_address & 0x80 != 0
        || output_route.sync_type != SyncType::Adaptive || output_route.usage_type != UsageType::FeedbackData
        || input_route.sync_type != SyncType::Adaptive
        || (output.interface_number, output.alt_setting, output.endpoint_address)
            != (output_route.interface, output_route.alt, output_route.endpoint.0)
        || (output.channels, output.subslot_size, output.bit_resolution) != output_route.pcm
        || output.service_interval_us != output_route.endpoint.3 || output.max_packet_bytes != output_route.packet_bytes
        || input_route.endpoint.0 & 0x80 == 0 || output.interface_number == input_route.interface
        || output_route.feedforward.is_some() || output_route.feedback.is_some()
        || output.implicit_feedback.is_some() || input_route.feedforward.is_some()
        || output_rate != input_rate || !adc3_shared_clock(output_route, input_route)
        || output_route.source_rate(output_rate)? != input_route.source_rate(input_rate)?
        || validate_adc3_static_rate(output_route, output_rate).is_err()
        || validate_adc3_static_rate(input_route, input_rate).is_err()
        || !capacity(output_route, output_rate) || !capacity(input_route, input_rate) {
        return Err("ADC3 adaptive capture requires the selected marked adaptive OUT in the same clocked function at the same wire rate".into());
    }
    Ok(())
}

fn adc3_implicit_pair(output: &IsoStreamCandidate, output_route: &Adc3Route,
    input: &IsoStreamCandidate, input_route: &Adc3Route) -> bool {
    output.sync_type == SyncType::Asynchronous && input.sync_type == SyncType::Asynchronous
        && output.interface_number != input.interface_number
        && input.usage_type == UsageType::FeedbackData
        && input.service_interval_us == output.service_interval_us
        && input.channels != 0 && input.subslot_size != 0
        && input.max_packet_bytes != 0 && output.max_packet_bytes != 0
        && adc3_shared_clock(output_route, input_route)
}

fn adc3_unique_implicit(mut matches: Vec<(ImplicitFeedbackSource, Adc3Route)>)
    -> Result<Vec<(ImplicitFeedbackSource, Adc3Route)>, String> {
    let Some((first, first_route)) = matches.first() else { return Err("ADC3 implicit feedback source missing".into()); };
    if matches.iter().any(|(source, route)|
        (source.interface_number, source.address, source.service_interval_us)
            != (first.interface_number, first.address, first.service_interval_us)
            || route.sync_type != first_route.sync_type || route.usage_type != first_route.usage_type
            || route.endpoint.1 != first_route.endpoint.1
            || !adc3_shared_clock(first_route, route)) {
        return Err("ADC3 implicit feedback source ambiguous".into());
    }
    // Conflicting duplicate descriptors for one alternate cannot pin an exact route.
    for (index, (source, route)) in matches.iter().enumerate() {
        if matches[..index].iter().any(|(other, other_route)|
            source.alt_setting == other.alt_setting && (source != other || route != other_route)) {
            return Err("ADC3 implicit feedback alternate ambiguous".into());
        }
    }
    // Alternate shapes of one proven pipe are not independent sources (3.14.2).
    // Prefer maximum whole-frame rate coverage, then lower reserved bandwidth
    // and finally the stable alternate number. Retain every exact route afterward.
    matches.sort_by_key(|(source, _)| (
        std::cmp::Reverse(source.max_packet_bytes / source.bytes_per_frame),
        source.max_packet_bytes, source.alt_setting,
    ));
    matches.dedup_by(|a, b| a == b);
    Ok(matches)
}

fn adc3_implicit_sources(
    config: &ConfigDescriptor, handle: &DeviceHandle<Context>, output: &IsoStreamCandidate,
    output_route: &Adc3Route, output_ranges: &[SamplingFrequencySubrange], plan: Option<&ClockPlan>,
) -> Result<Vec<(ImplicitFeedbackSource, Adc3Route)>, String> {
    let output_ranges = adc3_capacity_ranges(output, output_ranges);
    let mut matches = Vec::new();
    for alt in config.interfaces().flat_map(|i| i.descriptors()).filter(|d|
        d.class_code() == 1 && d.sub_class_code() == 2 && d.protocol_code() == 0x30
            && d.setting_number() != 0
            && d.interface_number() >= output_route.first
            && u16::from(d.interface_number()) < u16::from(output_route.first) + u16::from(output_route.count)) {
        if alt.interface_number() == output.interface_number { continue; }
        // Explicit USB usage marking is required. Endpoint/interface adjacency is no association.
        if !alt.endpoint_descriptors().any(|e| e.direction() == Direction::In
            && e.transfer_type() == TransferType::Isochronous && e.sync_type() == SyncType::Asynchronous
            && e.usage_type() == UsageType::FeedbackData) { continue; }
        let Ok((input, mut route)) = adc3_alternate_overlay(config, handle, &alt, output.usb_speed, true, plan, false, false) else { continue; };
        if !adc3_implicit_pair(output, output_route, &input, &route) { continue; }
        // Prove at least one actual clock rate survives both alternates and
        // both endpoint capacities before ranking same-pipe alternate shapes.
        // Representative sample rates are deliberately not used as a proxy.
        let ranges = adc3_route_limit_ranges(&route, &output_ranges);
        if adc3_capacity_ranges(&input, &ranges).is_empty() { continue; }
        route.plan = None; // The sink owns the common selector transaction.
        matches.push((ImplicitFeedbackSource {
            interface_number: input.interface_number, alt_setting: input.alt_setting, address: input.endpoint_address,
            bytes_per_frame: usize::from(input.channels) * usize::from(input.subslot_size),
            service_interval_us: input.service_interval_us, max_packet_bytes: input.max_packet_bytes,
        }, route));
    }
    adc3_unique_implicit(matches)
}

pub(super) fn adc3_capacity_ranges(candidate: &IsoStreamCandidate, ranges: &[SamplingFrequencySubrange]) -> Vec<SamplingFrequencySubrange> {
    let ceiling = |capacity: usize, stride: usize, interval: u32| -> u32 {
        if stride == 0 || interval == 0 { return 0; }
        ((capacity / stride) as u64 * 1_000_000 / u64::from(interval)).min(u64::from(u32::MAX)) as u32
    };
    let mut max = ceiling(candidate.max_packet_bytes, usize::from(candidate.channels) * usize::from(candidate.subslot_size), candidate.service_interval_us);
    if let Some(source) = candidate.implicit_feedback {
        max = max.min(ceiling(source.max_packet_bytes, source.bytes_per_frame, source.service_interval_us));
    }
    ranges.iter().filter_map(|r| {
        if r.min == 0 || r.min > r.max || r.min > max { return None; }
        let upper = r.max.min(max);
        let upper = if r.res == 0 { upper } else { r.min + (upper - r.min) / r.res * r.res };
        Some(SamplingFrequencySubrange { min: r.min, max: upper, res: r.res })
    }).collect()
}

pub(super) fn adc3_candidates_with_ranges(
    device: &Device<Context>,
    handle: &DeviceHandle<Context>,
    speed: Speed,
    claimed: &[u8],
) -> Vec<(IsoStreamCandidate, Vec<SamplingFrequencySubrange>)> {
    let Ok(config) = device.active_config_descriptor() else {
        return vec![];
    };
    let mut result = Vec::new();
    for alt in config
        .interfaces()
        .flat_map(|i| i.descriptors())
        .filter(|d| {
            d.class_code() == 1
                && d.sub_class_code() == 2
                && d.protocol_code() == 0x30
                && d.setting_number() != 0
                && claimed.contains(&d.interface_number())
        })
    {
        // Do not allow libusb class requests to reach an unclaimed AC owner.
        if !owning_function(&config, handle, alt.interface_number())
            .ok()
            .is_some_and(|(iad, _, _)| claimed.contains(&iad.b_first_interface))
        {
            continue;
        }
        match adc3_alternate_pairs(&config, handle, &alt, speed, false, None) {
            Ok(pairs) => {
                for pair in pairs {
                    if claimed.contains(&pair.1.control)
                        && pair.0.implicit_feedback.map_or(true, |source| claimed.contains(&source.interface_number)) {
                        result.push((pair.0, pair.2));
                    }
                }
            }
            Err(e) => {
                log::debug!(
                    "ADC3 interface {} alt {} refused: {e}",
                    alt.interface_number(),
                    alt.setting_number()
                );
            }
        }
    }
    result
}
pub(super) fn adc3_capture_route_for_endpoint(
    claimed: &ClaimedHandle,
    config: &ConfigDescriptor,
    interface: u8,
    alternate: u8,
    expected: &Adc3Route,
) -> Result<Adc3Route, String> {
    let alt = config
        .interfaces()
        .flat_map(|i| i.descriptors())
        .find(|d| d.interface_number() == interface && d.setting_number() == alternate)
        .ok_or("ADC3 capture interface disappeared")?;
    let plan = capture_peer_plan(Some(expected), config.number(), interface);
    adc3_alternate_overlay(
        config, &claimed.handle, &alt, resolve_device_speed(&claimed.handle.device()),
        true, plan, !expected.implicit_feedforward, expected.implicit_feedforward,
    )
    .map(|(_, mut route)| { route.clock_context = expected.clock_context.clone(); route })
}

// Resolve the input against the intended pins before any shared selector is
// changed. The duplex pair validator then proves identity and rate compatibility.
fn capture_peer_plan(saved: Option<&Adc3Route>, config: u8, interface: u8) -> Option<&ClockPlan> {
    saved.filter(|route| route.config == config && interface >= route.first
        && u16::from(interface) < u16::from(route.first) + u16::from(route.count))
        .and_then(|route| route.plan.as_ref())
}

/// Capture discovery runs under the same exclusive USB owner as playback.
/// Wake before HC/clock reads; keep successful candidates' domains leased until
/// the selected stream retires. Failed probes restore their writes immediately.
pub(super) fn adc3_capture_candidates(
    claimed: &ClaimedHandle,
    config: &ConfigDescriptor,
    speed: Speed,
    rate: u32,
    planned_clock: bool,
) -> Result<Vec<(IsoStreamCandidate, Adc3Route)>, String> {
    let mut result = Vec::new();
    for alt in config
        .interfaces()
        .flat_map(|i| i.descriptors())
        .filter(|d| {
            d.class_code() == 1
                && d.sub_class_code() == 2
                && d.protocol_code() == 0x30
                && d.setting_number() != 0
                && claimed.claimed_interfaces.contains(&d.interface_number())
                && d.endpoint_descriptors()
                    .any(|e| e.direction() == Direction::In
                        && matches!(e.usage_type(), UsageType::Data | UsageType::FeedbackData))
        })
    {
        let checkpoint = claimed.adc3_power.pending.lock().len();
        let attempt: Result<Option<(IsoStreamCandidate, Adc3Route)>, String> = (|| {
            prepare_adc3_alternate_power(claimed, config, &alt, true)?;
            let saved = if planned_clock { NEGOTIATED.lock().clone() } else { None };
            let plan = capture_peer_plan(saved.as_ref(), config.number(), alt.interface_number());
            let implicit = saved.as_ref().and_then(|r| r.implicit.as_deref()).filter(|r|
                r.interface == alt.interface_number() && r.alt == alt.setting_number());
            let (candidate, mut route) = if implicit.is_some() {
                adc3_alternate_overlay(config, &claimed.handle, &alt, speed, true, plan, false, false)?
            } else {
                adc3_alternate_overlay(config, &claimed.handle, &alt, speed, true, plan,
                    false, planned_clock && saved.is_some())?
            };
            // Keep the projected selector overlay until raw multiplier factors
            // and projected wire ranges have been validated against that path.
            let supports = sampling_frequency_ranges_support_rate(
                &adc3_wire_ranges(&claimed.handle, &route)?, rate);
            if let Some(expected) = implicit {
                route.plan = None;
                if &route != expected { return Err("ADC3 shared capture source changed".into()); }
            }
            for domain in route.domains.iter().filter(|d| adc3_route_domain_required(&route, d)) {
                claimed.adc3_power.acquire(route.control, domain)?;
            }
            if supports && route.feedforward.map_or(true, |pipe| pipe.nominal(rate).is_ok())
                && candidate.max_packet_bytes
                    >= required_max_packet_bytes(
                        rate,
                        candidate.service_interval_us,
                        usize::from(candidate.channels) * usize::from(candidate.subslot_size),
                    )
            {
                Ok(Some((candidate, route)))
            } else {
                Ok(None)
            }
        })();
        match attempt {
            Ok(Some(pair)) => result.push(pair),
            other => {
                let pending = claimed.adc3_power.pending.lock().split_off(checkpoint);
                restore_domains(&mut PowerUsb(&claimed.adc3_power), pending)?;
                if let Err(error) = other {
                    log::debug!(
                        "ADC3 capture interface {} alt {} refused: {error}",
                        alt.interface_number(),
                        alt.setting_number()
                    );
                }
            }
        }
    }
    Ok(result)
}

/// Standalone capture keeps its transaction on the owning lease; never consult
/// or replace playback's pending NEGOTIATED plan. Try every exact current route
/// before considering bounded writable selector alternatives.
pub(super) fn select_standalone_adc3_capture(
    claimed: &ClaimedHandle, config: &ConfigDescriptor, request: &crate::capture::CaptureRequest,
) -> Result<Option<(IsoStreamCandidate, Adc3Route)>, String> {
    let speed = resolve_device_speed(&claimed.handle.device());
    let matches = |candidate: &IsoStreamCandidate, route: &Adc3Route| {
        let Ok(map) = capture_channel_map(route) else { return false; };
        let Ok(format) = crate::capture::CaptureFormat::pcm(request.sample_rate,
            candidate.bit_resolution, candidate.subslot_size, map) else { return false; };
        format.satisfies(request) && adc3_candidate_matches(&claimed.handle, candidate, route,
            PlaybackFormat { sample_rate: request.sample_rate, channels: request.channels,
                bit_depth: request.valid_bits, is_dop: false,
                dsd_transport: DsdTransportMode::None, dsd_bit_rate: 0 })
    };
    let alts: Vec<_> = config.interfaces().flat_map(|i| i.descriptors()).filter(|alt|
        alt.class_code() == 1 && alt.sub_class_code() == 2 && alt.protocol_code() == 0x30
        && alt.setting_number() != 0 && claimed.claimed_interfaces.contains(&alt.interface_number())
        && alt.endpoint_descriptors().any(|ep| ep.direction() == Direction::In
            && ep.transfer_type() == TransferType::Isochronous
            && matches!(ep.usage_type(), UsageType::Data | UsageType::FeedbackData))).collect();
    for alt in &alts {
        prepare_adc3_alternate_power(claimed, config, alt, true)?;
        let Ok(pair) = adc3_alternate(config, &claimed.handle, alt, speed, true) else { continue; };
        if !matches(&pair.0, &pair.1) { continue; }
        claimed.set_alternate_setting(alt.interface_number(), 0)?;
        let frequency = if let Some(clock) = &pair.1.clock {
            let mut bytes = [0; 4];
            read_exact(&claimed.handle, pair.1.control, clock.id, 1, 1, &mut bytes)?;
            let original = u32::from_le_bytes(bytes);
            if original == 0 { continue; }
            Some(FrequencySnapshot { source: clock.id, original, target: pair.1.source_rate(request.sample_rate)? })
        } else { None };
        // Only a clean live bitmap rejection permits another candidate. Keep
        // control failures distinct, but restore the exact clock before either
        // retrying or returning the failure.
        let availability = if configure_adc3(&claimed.handle, &pair.1, request.sample_rate).is_ok() {
            let availability = if pair.1.profile == wire::FunctionProfile::Full {
                claimed.alternate_is_available(pair.1.interface, pair.1.alt)
            } else { Ok(true) };
            if matches!(&availability, Ok(true)) { return Ok(Some(pair)); }
            Some(availability)
        } else { None };
        if let Some(frequency) = frequency {
            if let Err(error) = verify_adc3_rate_route(&claimed.handle, &pair.1).and_then(|()|
                restore_adc3_frequency(&mut Adc3LeaseClockIo {
                    lease: &claimed.adc3_power, control: pair.1.control, ratio_route: Some(&pair.1) }, frequency)) {
                claimed.adc3_power.disarm();
                return Err(format!("ADC3 capture current frequency restoration uncertain: {error}"));
            }
        }
        if let Some(Err(error)) = availability { return Err(error); }
        // A changed current selector is external drift, not permission to retry
        // with a newly observed original pin snapshot.
        if adc3_alternate(config, &claimed.handle, alt, speed, true)?.1 != pair.1 {
            return Err("ADC3 capture current route drifted; renegotiate".into());
        }
    }
    for alt in &alts {
        for pair in adc3_directional_alternative_routes(claimed, config, alt, speed, true, |candidate, route, _| matches(candidate, route))? {
            match apply_adc3_clock_plan(claimed, &pair.1, request.sample_rate) {
                Ok(()) => {
                    let availability = if pair.1.profile == wire::FunctionProfile::Full {
                        claimed.alternate_is_available(pair.1.interface, pair.1.alt)
                    } else { Ok(true) };
                    if matches!(&availability, Ok(true)) { return Ok(Some(pair)); }
                    // Retain acquired power while handing back the selector
                    // and source-frequency transaction before the next choice.
                    claimed.adc3_power.restore_clock()?;
                    availability?;
                }
                Err(error) if claimed.adc3_power.clock.lock().is_some() => return Err(error),
                Err(_) => {}
            }
        }
    }
    Ok(None)
}

pub(super) fn capture_channel_map(route: &Adc3Route) -> Result<crate::capture::ChannelMap, String> {
    use crate::capture::{ChannelMap, ChannelPosition};
    let positions = if let Some(cluster) = &route.cluster {
        cluster
            .channels
            .iter()
            .map(|channel| match channel {
                wire::Channel::Information {
                    purpose,
                    relationship,
                    group,
                } => {
                    let relationship = match relationship {
                        wire::ChannelRelationship::Undefined => 0,
                        wire::ChannelRelationship::Mono => 1,
                        wire::ChannelRelationship::Left => 2,
                        wire::ChannelRelationship::Right => 3,
                        wire::ChannelRelationship::Array => 4,
                        wire::ChannelRelationship::Pattern(code)
                        | wire::ChannelRelationship::Unknown(code) => *code,
                        wire::ChannelRelationship::Spatial(position) => *position as u8,
                    };
                    ChannelPosition::Adc3Information {
                        purpose: *purpose,
                        relationship,
                        group: *group,
                    }
                }
                wire::Channel::Ambisonic {
                    ordering,
                    acn,
                    normalization,
                    group,
                } => ChannelPosition::Adc3Ambisonic {
                    ordering: *ordering,
                    acn: *acn,
                    normalization: *normalization,
                    group: *group,
                },
            })
            .collect()
    } else if route.pcm.0 == 1 {
        vec![ChannelPosition::FrontCenter]
    } else if route.pcm.0 == 2 {
        vec![ChannelPosition::FrontLeft, ChannelPosition::FrontRight]
    } else {
        return Err("BADD invalid channel count".into());
    };
    ChannelMap::from_positions(positions).map_err(|e| format!("ADC3 channel map: {e:?}"))
}

pub(super) fn configure_adc3(
    handle: &DeviceHandle<Context>,
    route: &Adc3Route,
    rate: u32,
) -> Result<(), String> {
    validate_adaptive_capture_rate(route, rate)?;
    validate_adc3_clock(handle, route, rate, true)
}

fn validate_adaptive_capture_rate(route: &Adc3Route, rate: u32) -> Result<(), String> {
    if route.implicit_feedforward && settings().disable_feedback() {
        return Err("ADC3 implicit feedforward disabled".into());
    }
    if let Some(pipe) = route.feedforward {
        if settings().disable_feedback() { return Err("ADC3 adaptive capture requires enabled feedforward".into()); }
        validate_feedforward_capacity(pipe, rate, route.packet_bytes, route.endpoint.3,
            usize::from(route.pcm.0) * usize::from(route.pcm.1))?;
    }
    Ok(())
}
fn validate_feedforward_capacity(pipe: FeedforwardEndpoint, rate: u32, capacity: usize,
    service_us: u32, stride: usize) -> Result<(), String> {
    pipe.nominal(rate)?;
    if capacity < required_max_packet_bytes(rate, service_us, stride) {
        return Err("ADC3 adaptive capture rate exceeds data endpoint capacity".into());
    }
    Ok(())
}
pub(super) fn verify_adc3_capture_activation(handle: &DeviceHandle<Context>, route: &Adc3Route,
    rate: u32) -> Result<(), String> {
    validate_adaptive_capture_rate(route, rate)?;
    validate_adc3_clock(handle, route, rate, false)
}

fn validate_adc3_clock(handle: &DeviceHandle<Context>, route: &Adc3Route, rate: u32,
    allow_frequency_write: bool) -> Result<(), String> {
    if matches!(route.profile, wire::FunctionProfile::Badd(_)) {
        return if rate == 48000 {
            Ok(())
        } else {
            Err("BADD rate must be 48000 Hz".into())
        };
    }
    validate_adc3_static_rate(route, rate)?;
    verify_adc3_rate_route(handle, route)?;
    let wire_rate = rate;
    let rate = route.source_rate(wire_rate)?;
    let source = route.clock.as_ref().ok_or("ADC3 clock source missing")?;
    let ranges = get_sampling_frequency_ranges(handle, route.control, source.id)?;
    // Apply the same raw-range validity contract during programming as during
    // advertisement; an integral conversion alone cannot legalize a bad range.
    let wire_ranges = adc3_route_ranges(route, &ranges)?;
    if !sampling_frequency_ranges_support_rate(&wire_ranges, wire_rate) {
        return Err("ADC3 rate outside source ranges".into());
    }
    let mut current = [0; 4];
    read_exact(handle, route.control, source.id, 1, 1, &mut current)?;
    if u32::from_le_bytes(current) != rate {
        if !allow_frequency_write { return Err("ADC3 source frequency changed before activation".into()); }
        if source.controls.access(1) != wire::ControlAccess::ReadWrite {
            return Err("ADC3 source frequency is read-only and differs".into());
        }
        verify_adc3_rate_route(handle, route)?;
        write_exact(handle, route.control, source.id, 1, &rate.to_le_bytes())?;
        thread::sleep(Duration::from_millis(settings().settle_delay_ms()));
    }
    read_exact(handle, route.control, source.id, 1, 1, &mut current)?;
    if route.wire_rate(u32::from_le_bytes(current))? != wire_rate {
        return Err("ADC3 source frequency verification failed".into());
    }
    verify_adc3_rate_route(handle, route)?;
    let validity = crate::clock_resolution::clock_validity(source.controls.0, || {
        let mut valid = [0];
        read_exact(handle, route.control, source.id, 2, 1, &mut valid)?;
        Ok(valid == [1])
    })?;
    if validity == crate::clock_resolution::ClockValidity::Unreported {
        log_info!("[USB] ADC3 clock {}: validity unreported; exact {} Hz frequency verified", source.id, rate);
    }
    Ok(())
}

/// One lease per exclusive USB owner. Repeated domains share its lifetime and
/// are restored in reverse acquisition order only after transfer retirement.
pub(super) struct Adc3PowerLease {
    fd: RawFd,
    pending: Mutex<Vec<(u8, u8, u8)>>,
    clock: Mutex<Option<(Adc3Route, ClockPlan, FrequencySnapshot, Vec<u8>)>>,
}
impl Adc3PowerLease {
    pub(super) fn new(fd: RawFd) -> Self {
        Self {
            fd,
            pending: Mutex::new(vec![]),
            clock: Mutex::new(None),
        }
    }
    fn control(&self, input: bool, ac: u8, id: u8, state: &mut [u8; 1]) -> Result<(), String> {
        let request = UsbfsControlTransfer {
            request_type: if input { 0xa1 } else { 0x21 },
            request: 1,
            value: 0x0200,
            index: u16::from(ac) | (u16::from(id) << 8),
            length: 1,
            timeout_ms: 1000,
            data: state.as_mut_ptr().cast(),
        };
        let n = unsafe { libc::ioctl(self.fd, USBDEVFS_CONTROL, &request) };
        if n != 1 {
            return Err(if n < 0 {
                std::io::Error::last_os_error().to_string()
            } else {
                "ADC3 power control width mismatch".into()
            });
        }
        Ok(())
    }
    pub(super) fn acquire(&self, ac: u8, d: &wire::PowerDomain) -> Result<(), String> {
        acquire_domain(&mut PowerUsb(self), &mut self.pending.lock(), ac, d)
    }
    pub(super) fn restore(&self) -> Result<(), String> {
        self.restore_clock()?;
        let pending = std::mem::take(&mut *self.pending.lock());
        restore_domains(&mut PowerUsb(self), pending)
    }
    fn restore_clock(&self) -> Result<(), String> {
        // Never power down an owning domain before selector hand-back. Keep
        // failed/drifted state armed for explicit reporting; do not overwrite it.
        if let Some((route, plan, frequency, interfaces)) = self.clock.lock().take() {
            let mut io = Adc3LeaseClockIo { lease: self, control: route.control, ratio_route: Some(&route) };
            let restore = io.verify_ratios().and_then(|()|
                restore_adc3_clock_transaction(&mut io, &plan, frequency, &interfaces,
                    |interface| self.verify_idle(interface)));
            if let Err(error) = restore {
                // No later Drop retry may power down after uncertain hand-back.
                self.pending.lock().clear();
                return Err(format!("ADC3 selector hand-back: {error}"));
            }
        }
        Ok(())
    }
    /// End a scoped routing wake without overwriting an external power change.
    /// Taking the snapshot first also prevents Drop from retrying drifted writes.
    pub(super) fn restore_checked(&self) -> Result<(), String> {
        let pending = std::mem::take(&mut *self.pending.lock());
        restore_domains_checked(&mut PowerUsb(self), pending)
    }
    pub(super) fn verify_idle(&self, interface: u8) -> Result<(), String> {
        let mut alternate = [0xffu8];
        let request = UsbfsControlTransfer { request_type: 0x81, request: 10,
            value: 0, index: u16::from(interface), length: 1, timeout_ms: 1000,
            data: alternate.as_mut_ptr().cast() };
        let n = unsafe { libc::ioctl(self.fd, USBDEVFS_CONTROL, &request) };
        verify_idle_reply(n, alternate[0])
    }
    pub(super) fn disarm(&self) {
        self.pending.lock().clear();
        self.clock.lock().take();
    }
}
struct Adc3LeaseClockIo<'a> { lease: &'a Adc3PowerLease, control: u8, ratio_route: Option<&'a Adc3Route> }
impl Adc3LeaseClockIo<'_> {
    fn verify_ratios(&self) -> Result<(), String> {
        if let Some(route) = self.ratio_route {
            verify_adc3_route_ratios(route, |entity, selector, bytes|
                self.transfer_control(true, entity, selector, bytes))?;
        }
        Ok(())
    }
    fn transfer(&self, input: bool, entity: u8, bytes: &mut [u8]) -> Result<(), String> {
        self.transfer_control(input, entity, 1, bytes)
    }
    fn transfer_control(&self, input: bool, entity: u8, selector: u8, pin: &mut [u8]) -> Result<(), String> {
        let request = UsbfsControlTransfer { request_type: if input { 0xa1 } else { 0x21 },
            request: 1, value: u16::from(selector) << 8, index: u16::from(self.control) | (u16::from(entity) << 8),
            length: pin.len() as u16, timeout_ms: 1000, data: pin.as_mut_ptr().cast() };
        let n = unsafe { libc::ioctl(self.lease.fd, USBDEVFS_CONTROL, &request) };
        if n != pin.len() as i32 { return Err(format!("ADC3 clock control transferred {n}, expected {}", pin.len())); }
        Ok(())
    }
}
impl ClockPlanIo for Adc3LeaseClockIo<'_> {
    fn read_pin(&mut self, selector: u8) -> Result<u8, String> {
        let mut pin = [0]; self.transfer(true, selector, &mut pin)?; Ok(pin[0])
    }
    fn write_pin(&mut self, selector: u8, pin: u8) -> Result<(), String> {
        self.verify_ratios()?;
        self.transfer(false, selector, &mut [pin])?;
        self.verify_ratios()
    }
    fn validate_source(&mut self, _: u8, _: u32) -> Result<(), String> { Ok(()) }
}
#[derive(Clone, Copy)]
struct FrequencySnapshot { source: u8, original: u32, target: u32 }
trait Adc3FrequencyIo {
    fn read_frequency(&mut self, source: u8) -> Result<u32, String>;
    fn write_frequency(&mut self, source: u8, frequency: u32) -> Result<(), String>;
}
impl Adc3FrequencyIo for Adc3LeaseClockIo<'_> {
    fn read_frequency(&mut self, source: u8) -> Result<u32, String> {
        let mut bytes = [0; 4]; self.transfer(true, source, &mut bytes)?; Ok(u32::from_le_bytes(bytes))
    }
    fn write_frequency(&mut self, source: u8, frequency: u32) -> Result<(), String> {
        self.verify_ratios()?;
        self.transfer(false, source, &mut frequency.to_le_bytes())?;
        thread::sleep(Duration::from_millis(settings().settle_delay_ms()));
        self.verify_ratios()
    }
}
fn restore_adc3_clock_transaction(
    io: &mut (impl ClockPlanIo + Adc3FrequencyIo), plan: &ClockPlan,
    frequency: FrequencySnapshot, interfaces: &[u8], mut verify_idle: impl FnMut(u8) -> Result<(), String>,
) -> Result<(), String> {
    for interface in interfaces { verify_idle(*interface)?; }
    verify_adc3_target_pins(io, plan)?;
    restore_adc3_frequency(io, frequency)?;
    plan.restore(io).map_err(|e| e.to_string())
}
fn verify_adc3_target_pins(io: &mut impl ClockPlanIo, plan: &ClockPlan) -> Result<(), String> {
    for pin in &plan.selectors {
        if io.read_pin(pin.selector)? != pin.target {
            return Err("ADC3 selector drifted before frequency hand-back".into());
        }
    }
    Ok(())
}
fn restore_adc3_frequency(io: &mut impl Adc3FrequencyIo, saved: FrequencySnapshot) -> Result<(), String> {
    if saved.original == saved.target { return Ok(()); }
    let current = io.read_frequency(saved.source)?;
    if current == saved.original { return Ok(()); }
    if current != saved.target { return Err("ADC3 source frequency drifted; preserving external change".into()); }
    io.write_frequency(saved.source, saved.original)?;
    if io.read_frequency(saved.source)? != saved.original {
        return Err("ADC3 source frequency rollback did not verify".into());
    }
    Ok(())
}
struct Adc3ClockIo<'a> { handle: &'a DeviceHandle<Context>, route: &'a Adc3Route, rate: u32 }
impl ClockPlanIo for Adc3ClockIo<'_> {
    fn read_pin(&mut self, selector: u8) -> Result<u8, String> {
        let mut pin = [0]; read_exact(self.handle, self.route.control, selector, 1, 1, &mut pin)?; Ok(pin[0])
    }
    fn write_pin(&mut self, selector: u8, pin: u8) -> Result<(), String> {
        verify_adc3_route_ratios(self.route, |entity, selector, bytes|
            read_exact(self.handle, self.route.control, entity, selector, 1, bytes))?;
        write_exact(self.handle, self.route.control, selector, 1, &[pin])?;
        verify_adc3_route_ratios(self.route, |entity, selector, bytes|
            read_exact(self.handle, self.route.control, entity, selector, 1, bytes))
    }
    fn validate_source(&mut self, source: u8, controls: u32) -> Result<(), String> {
        if self.route.clock.as_ref().map(|c| (c.id, c.controls.0)) != Some((source, controls)) {
            return Err("ADC3 selected source descriptor changed".into());
        }
        let context = self.route.clock_context.as_ref().ok_or("ADC3 affected clock snapshot missing")?;
        let config = self.handle.device().active_config_descriptor().map_err(|e| e.to_string())?;
        let (_, _, extra) = owning_function(&config, self.handle, self.route.interface)?;
        for peer in &context.peers {
            let (_, actual) = clock_route(self.handle, self.route.control, peer.terminal, &extra, peer.capture, None)?;
            if actual != peer.target { return Err("ADC3 peer clock changed during selector transaction".into()); }
        }
        configure_adc3(self.handle, self.route, self.rate)
    }
}
trait PowerIo {
    fn read(&mut self, ac: u8, id: u8) -> Result<u8, String>;
    fn write(&mut self, ac: u8, id: u8, state: u8) -> Result<(), String>;
    fn delay(&mut self, micros: u32);
}
struct PowerUsb<'a>(&'a Adc3PowerLease);
impl PowerIo for PowerUsb<'_> {
    fn read(&mut self, ac: u8, id: u8) -> Result<u8, String> {
        let mut state = [0];
        self.0.control(true, ac, id, &mut state)?;
        Ok(state[0])
    }
    fn write(&mut self, ac: u8, id: u8, state: u8) -> Result<(), String> {
        self.0.control(false, ac, id, &mut [state])
    }
    fn delay(&mut self, micros: u32) {
        thread::sleep(Duration::from_micros(u64::from(micros)));
    }
}
fn acquire_domain(
    io: &mut impl PowerIo,
    pending: &mut Vec<(u8, u8, u8)>,
    ac: u8,
    d: &wire::PowerDomain,
) -> Result<(), String> {
    if pending.iter().any(|&(a, id, _)| a == ac && id == d.id) {
        return Ok(());
    }
    let previous = io.read(ac, d.id)?;
    if previous > 2 {
        return Err("ADC3 unknown power state".into());
    }
    // Record before SET so even a short/failed write is restored on unwind.
    pending.push((ac, d.id, previous));
    if previous != 0 {
        io.write(ac, d.id, 0)?;
        io.delay(if previous == 1 {
            d.recovery_d1_micros()
        } else {
            d.recovery_d2_micros()
        });
    }
    if io.read(ac, d.id)? != 0 {
        return Err("ADC3 D0 power verification failed".into());
    }
    Ok(())
}
fn restore_domains(io: &mut impl PowerIo, pending: Vec<(u8, u8, u8)>) -> Result<(), String> {
    let mut error = None;
    for (ac, id, previous) in pending.into_iter().rev() {
        if previous != 0 {
            let result = io.write(ac, id, previous).and_then(|_| {
                if io.read(ac, id)? == previous {
                    Ok(())
                } else {
                    Err("ADC3 power restore verification failed".into())
                }
            });
            if let Err(e) = result {
                error = Some(e);
            }
        }
    }
    error.map_or(Ok(()), Err)
}
fn restore_domains_checked(io: &mut impl PowerIo, pending: Vec<(u8, u8, u8)>) -> Result<(), String> {
    let mut error = None;
    for (ac, id, previous) in pending.into_iter().rev() {
        let result: Result<(), String> = (|| {
            let current = io.read(ac, id)?;
            if current == previous { return Ok(()); }
            if current != 0 { return Err("ADC3 power drifted; preserving external change".into()); }
            if previous != 0 {
                io.write(ac, id, previous)?;
                if io.read(ac, id)? != previous {
                    return Err("ADC3 power restore verification failed".into());
                }
            }
            Ok(())
        })();
        if let Err(e) = result { error = Some(e); }
    }
    error.map_or(Ok(()), Err)
}
impl Drop for Adc3PowerLease {
    fn drop(&mut self) {
        if let Err(e) = self.restore() {
            log_warn!("ADC3 power hand-back failed: {e}");
        }
    }
}
/// Wake only the selected playback terminal's domain. A failing ADC3 probe
/// restores its own lease before considering another alternate/function.
fn prepare_adc3_alternate_power(
    claimed: &ClaimedHandle,
    config: &ConfigDescriptor,
    alt: &rusb::InterfaceDescriptor,
    capture: bool,
) -> Result<(), String> {
    if alt.protocol_code() != 0x30 {
        return Ok(());
    }
    let (iad, profile, extra) = owning_function(config, &claimed.handle, alt.interface_number())?;
    claimed.ensure_interface_claimed(iad.b_first_interface)?;
    let terminal = match profile {
        wire::FunctionProfile::Badd(_) => {
            if capture {
                6
            } else {
                1
            }
        }
        wire::FunctionProfile::Full => {
            let headers: Vec<_> = DescriptorIter::new(alt.extra())
                .filter(|d| d.get(1) == Some(&0x24) && d.get(2) == Some(&1))
                .collect();
            let [h] = headers.as_slice() else {
                return Err("ADC3 AS header missing or ambiguous".into());
            };
            wire::parse_as_header(h)
                .map_err(|e| e.to_string())?
                .terminal_link
        }
    };
    for d in domains(profile, &extra, capture)?
        .into_iter()
        .filter(|d| d.entities.contains(&terminal))
    {
        claimed.adc3_power.acquire(iad.b_first_interface, &d)?;
    }
    Ok(())
}
#[derive(Debug, Clone, PartialEq, Eq)]
struct Adc3PeerClock {
    interface: u8,
    alternate: u8,
    terminal: u8,
    capture: bool,
    original: ClockChoice,
    target: ClockChoice,
}
#[derive(Debug, Clone, PartialEq, Eq)]
struct Adc3ClockContext {
    interfaces: Vec<u8>,
    peers: Vec<Adc3PeerClock>,
    frequencies: Vec<(u8, u32)>,
}
fn verify_idle_reply(transferred: i32, alternate: u8) -> Result<(), String> {
    if transferred != 1 || alternate != 0 {
        return Err("ADC3 affected interface is not confirmed at alternate zero".into());
    }
    Ok(())
}
fn adc3_function_streams(claimed: &ClaimedHandle, config: &ConfigDescriptor,
    first: u8, count: u8) -> Result<Vec<u8>, String> {
    let expected: HashSet<_> = (u16::from(first)..u16::from(first) + u16::from(count))
        .map(|id| u8::try_from(id).map_err(|_| "ADC3 function interface range overflow".to_string()))
        .collect::<Result<_, _>>()?;
    let present: HashSet<_> = config.interfaces().filter(|i| expected.contains(&i.number()))
        .map(|i| i.number()).collect();
    if expected.is_empty() || present != expected
        || expected.iter().any(|id| !claimed.claimed_interfaces.contains(id)) {
        return Err("ADC3 alternate clock requires complete function claims".into());
    }
    let mut streams: Vec<_> = config.interfaces().flat_map(|i| i.descriptors())
        .filter(|d| expected.contains(&d.interface_number()) && d.class_code() == 1 && d.sub_class_code() == 2)
        .map(|d| d.interface_number()).collect::<HashSet<_>>().into_iter().collect();
    streams.sort_unstable();
    if streams.is_empty() || streams.len() > 32 {
        return Err("ADC3 affected stream interface set is empty or excessive".into());
    }
    Ok(streams)
}
/// Claim and quiesce the whole function, including idle duplex/implicit peers.
/// A successful SET_INTERFACE alone does not prove the device reached alt zero.
fn guard_adc3_clock_mutation(claimed: &ClaimedHandle, route: &Adc3Route) -> Result<Vec<u8>, String> {
    let config = claimed.handle.device().active_config_descriptor().map_err(|e| e.to_string())?;
    let interfaces = adc3_function_streams(claimed, &config, route.first, route.count)?;
    if !interfaces.contains(&route.interface) { return Err("ADC3 selected stream escaped function".into()); }
    for interface in &interfaces {
        claimed.set_alternate_setting(*interface, 0)?;
        claimed.adc3_power.verify_idle(*interface)?;
    }
    Ok(interfaces)
}
/// Snapshot every declared peer terminal at both original and proposed pins.
/// Unresolved peers cannot authorize a mutation even while currently idle.
fn adc3_clock_context(claimed: &ClaimedHandle, config: &ConfigDescriptor,
    first: u8, count: u8, control: u8, extra: &[u8], plan: &ClockPlan) -> Result<Adc3ClockContext, String> {
    let interfaces = adc3_function_streams(claimed, config, first, count)?;
    let mut peers = Vec::new();
    let mut sources = HashSet::new();
    for alt in config.interfaces().flat_map(|i| i.descriptors()).filter(|d|
        interfaces.contains(&d.interface_number()) && d.setting_number() != 0) {
        if alt.protocol_code() != 0x30 { return Err("ADC3 affected interface protocol changed".into()); }
        let headers: Vec<_> = DescriptorIter::new(alt.extra())
            .filter(|d| d.get(1) == Some(&0x24) && d.get(2) == Some(&1)).collect();
        let [header] = headers.as_slice() else { return Err("ADC3 peer AS header missing or ambiguous".into()); };
        let terminal = wire::parse_as_header(header).map_err(|e| e.to_string())?.terminal_link;
        let directions: HashSet<_> = alt.endpoint_descriptors()
            .filter(|ep| ep.transfer_type() == TransferType::Isochronous
                && matches!(ep.usage_type(), UsageType::Data | UsageType::FeedbackData))
            .map(|ep| ep.direction() == Direction::In).collect();
        if directions.is_empty() { return Err("ADC3 peer has no proven data direction".into()); }
        for capture in directions {
            let (_, original) = clock_route(&claimed.handle, control, terminal, extra, capture, None)?;
            let (_, target) = clock_route(&claimed.handle, control, terminal, extra, capture, Some(plan))?;
            sources.insert(original.source_id);
            sources.insert(target.source_id);
            peers.push(Adc3PeerClock { interface: alt.interface_number(), alternate: alt.setting_number(),
                terminal, capture, original, target });
            if peers.len() > 64 || sources.len() > 32 { return Err("ADC3 peer clock proof budget exhausted".into()); }
        }
    }
    peers.sort_by_key(|p| (p.interface, p.alternate, p.capture));
    let mut sources: Vec<_> = sources.into_iter().collect();
    sources.sort_unstable();
    let mut frequencies = Vec::new();
    for source in sources {
        let mut bytes = [0; 4];
        read_exact(&claimed.handle, control, source, 1, 1, &mut bytes)?;
        let rate = u32::from_le_bytes(bytes);
        if rate == 0 { return Err("ADC3 affected source frequency snapshot is zero".into()); }
        frequencies.push((source, rate));
    }
    Ok(Adc3ClockContext { interfaces, peers, frequencies })
}
fn adc3_plans(handle: &DeviceHandle<Context>, control: u8, terminal: u8, extra: &[u8]) -> Result<Vec<ClockPlan>, String> {
    let mut reads = 0;
    ClockGraph::parse_adc3(extra)?.enumerate_plans(terminal, |id| {
        reads += 1;
        if reads > 16 { return Err("ADC3 selector read budget exhausted".into()); }
        let mut pin = [0]; read_exact(handle, control, id, 1, 1, &mut pin)?; Ok(pin[0])
    })
}
fn adc3_alternative_routes(
    claimed: &ClaimedHandle, config: &ConfigDescriptor, alt: &rusb::InterfaceDescriptor, speed: Speed, requested: PlaybackFormat,
) -> Result<Vec<(IsoStreamCandidate, Adc3Route)>, String> {
    adc3_directional_alternative_routes(claimed, config, alt, speed, false, |candidate, _, ranges| {
        candidate.implicit_feedback.map_or(true, |source| claimed.claimed_interfaces.contains(&source.interface_number))
            && adc3_candidate_matches_ranges(candidate, ranges, requested)
    })
}
fn adc3_directional_alternative_routes(
    claimed: &ClaimedHandle, config: &ConfigDescriptor, alt: &rusb::InterfaceDescriptor,
    speed: Speed, capture: bool, matches: impl Fn(&IsoStreamCandidate, &Adc3Route, &[SamplingFrequencySubrange]) -> bool,
) -> Result<Vec<(IsoStreamCandidate, Adc3Route)>, String> {
    let (iad, profile, extra) = owning_function(config, &claimed.handle, alt.interface_number())?;
    if profile != wire::FunctionProfile::Full { return Ok(Vec::new()); }
    let headers: Vec<_> = DescriptorIter::new(alt.extra()).filter(|d| d.get(1) == Some(&0x24) && d.get(2) == Some(&1)).collect();
    let [header] = headers.as_slice() else { return Err("ADC3 AS header missing or ambiguous".into()); };
    let terminal = wire::parse_as_header(header).map_err(|e| e.to_string())?.terminal_link;
    let streams = adc3_function_streams(claimed, config, iad.b_first_interface, iad.b_interface_count)?;
    for interface in streams {
        claimed.set_alternate_setting(interface, 0)?;
        claimed.adc3_power.verify_idle(interface)?;
    }
    let domains = domains(profile, &extra, capture)?;
    let checkpoint = claimed.adc3_power.pending.lock().len();
    let result = (|| {
        for domain in &domains { claimed.adc3_power.acquire(iad.b_first_interface, domain)?; }
        let mut reads = 0;
        let plans = ClockGraph::parse_adc3(&extra)?.enumerate_plans(terminal, |id| {
            reads += 1;
            if reads > 16 { return Err("ADC3 selector read budget exhausted".into()); }
            for domain in domains.iter().filter(|d| d.entities.contains(&id)) {
                claimed.adc3_power.acquire(iad.b_first_interface, domain)?;
            }
            let mut pin = [0]; read_exact(&claimed.handle, iad.b_first_interface, id, 1, 1, &mut pin)?; Ok(pin[0])
        })?;
        let mut result = Vec::new();
        for plan in plans.into_iter().filter(|p| p.selectors.iter().any(|p| p.original != p.target)).take(8) {
            for domain in domains.iter().filter(|d| d.entities.iter().any(|id| plan.path.contains(id))) {
                claimed.adc3_power.acquire(iad.b_first_interface, domain)?;
            }
            if let Ok(pairs) = adc3_alternate_pairs(config, &claimed.handle, alt, speed, capture, Some(&plan)) {
                let mut context = None;
                for (candidate, mut route, ranges) in pairs {
                    if !matches(&candidate, &route, &ranges) { continue; }
                    if context.is_none() {
                        let Ok(snapshot) = adc3_clock_context(claimed, config, iad.b_first_interface,
                            iad.b_interface_count, iad.b_first_interface, &extra, &plan) else { break; };
                        context = Some(snapshot);
                    }
                    route.clock_context = context.clone();
                    result.push((candidate, route));
                }
            }
        }
        Ok(result)
    })();
    let pending = claimed.adc3_power.pending.lock().split_off(checkpoint);
    restore_domains(&mut PowerUsb(&claimed.adc3_power), pending)?;
    result
}
/// Apply the saved snapshot only while its complete function and required
/// domains are owned. ClockPlan rejects original-pin drift before writing.
pub(super) fn apply_adc3_clock_plan(claimed: &ClaimedHandle, route: &Adc3Route, rate: u32) -> Result<(), String> {
    let Some(plan) = route.plan.as_ref() else { return Ok(()); };
    validate_adc3_static_rate(route, rate)?;
    let interfaces = guard_adc3_clock_mutation(claimed, route)?;
    for domain in route.domains.iter().filter(|d| adc3_route_domain_required(route, d)) { claimed.adc3_power.acquire(route.control, domain)?; }
    let config = claimed.handle.device().active_config_descriptor().map_err(|e| e.to_string())?;
    let (_, profile, extra) = owning_function(&config, &claimed.handle, route.interface)?;
    if profile != wire::FunctionProfile::Full
        || !adc3_plans(&claimed.handle, route.control, route.terminal, &extra)?.contains(plan) {
        return Err("ADC3 original pins or writable topology changed; renegotiate".into());
    }
    let context = adc3_clock_context(claimed, &config, route.first, route.count, route.control, &extra, plan)?;
    if route.clock_context.as_ref() != Some(&context) {
        return Err("ADC3 affected terminal paths or source frequencies changed; renegotiate".into());
    }
    // Retain uncertain rollback in the lease so cleanup cannot silently power
    // down before reporting selector hand-back failure.
    let mut current = [0; 4];
    read_exact(&claimed.handle, route.control, plan.source_id, 1, 1, &mut current)?;
    let frequency = FrequencySnapshot { source: plan.source_id, original: u32::from_le_bytes(current), target: route.source_rate(rate)? };
    if frequency.original == 0 { return Err("ADC3 source frequency snapshot is zero".into()); }
    verify_adc3_route_ratios(route, |entity, selector, bytes|
        read_exact(&claimed.handle, route.control, entity, selector, 1, bytes))?;
    let ranges = get_sampling_frequency_ranges(&claimed.handle, route.control, plan.source_id)?;
    if !sampling_frequency_ranges_support_rate(&adc3_route_ranges(route, &ranges)?, rate) {
        return Err("ADC3 rate outside alternate clock ranges".into());
    }
    match plan.apply(&mut Adc3ClockIo { handle: &claimed.handle, route, rate }) {
        Ok(()) => { *claimed.adc3_power.clock.lock() = Some((route.clone(), plan.clone(), frequency, interfaces.clone())); Ok(()) }
        Err(error) => {
            let frequency_restore = restore_adc3_frequency(&mut Adc3LeaseClockIo { lease: &claimed.adc3_power, control: route.control, ratio_route: Some(&route) }, frequency);
            if !error.rollback_errors.is_empty() || frequency_restore.is_err() {
                // Do not attempt another candidate or let Drop roll power back
                // after an uncertain selector/frequency restoration.
                *claimed.adc3_power.clock.lock() = Some((route.clone(), plan.clone(), frequency, interfaces.clone()));
            }
            Err(format!("ADC3 alternate clock transaction: {error}; frequency rollback: {frequency_restore:?}"))
        }
    }
}
/// A clean bitmap rejection is recoverable; a failed/malformed read is terminal.
/// Read both pipes even when OUT is unavailable so an IN read failure cannot be hidden.
fn adc3_live_pair_available(route: &Adc3Route,
    mut available: impl FnMut(u8, u8) -> Result<bool, String>) -> Result<bool, String> {
    let output = available(route.interface, route.alt)?;
    let input = match route.implicit.as_deref() {
        Some(peer) => available(peer.interface, peer.alt)?,
        None => true,
    };
    Ok(output && input)
}

fn adc3_live_route_compatible(expected: &Adc3Route, actual: &Adc3Route) -> bool {
    let same_clock = |left: &Adc3Route, right: &Adc3Route|
        left.clock == right.clock && left.choice == right.choice;
    expected.profile == wire::FunctionProfile::Full && actual.profile == expected.profile
        && (actual.config, actual.control, actual.first, actual.count, actual.interface, actual.terminal)
            == (expected.config, expected.control, expected.first, expected.count, expected.interface, expected.terminal)
        && (actual.pcm.0, actual.pcm.2) == (expected.pcm.0, expected.pcm.2)
        && same_clock(expected, actual)
        && match (expected.implicit.as_deref(), actual.implicit.as_deref()) {
            (Some(left), Some(right)) => (left.config, left.control, left.first, left.count)
                == (right.config, right.control, right.first, right.count)
                && (left.interface, left.endpoint.0, left.endpoint.1, left.terminal)
                    == (right.interface, right.endpoint.0, right.endpoint.1, right.terminal)
                && same_clock(left, right),
            (None, None) => true,
            _ => false,
        }
}

/// Called only after the exact requested clock is running. Discover against
/// actual pins, without clock overlays or any selector/frequency programming.
fn adc3_live_alternate(claimed: &ClaimedHandle, route: Adc3Route, speed: Speed, rate: u32)
    -> Result<Adc3Route, String> {
    if route.profile != wire::FunctionProfile::Full
        || adc3_live_pair_available(&route, |interface, alternate|
            claimed.alternate_is_available(interface, alternate))? {
        return Ok(route);
    }
    let config = claimed.handle.device().active_config_descriptor().map_err(|e| e.to_string())?;
    let requested = PlaybackFormat { sample_rate: rate, channels: route.pcm.0, bit_depth: route.pcm.2,
        is_dop: false, dsd_transport: DsdTransportMode::None, dsd_bit_rate: 0 };
    let mut options = Vec::new();
    for alt in config.interfaces().flat_map(|i| i.descriptors()).filter(|alt|
        alt.interface_number() == route.interface && alt.setting_number() != 0
            && alt.class_code() == 1 && alt.sub_class_code() == 2 && alt.protocol_code() == 0x30) {
        let Ok(pairs) = adc3_alternate_pairs(&config, &claimed.handle, &alt, speed, false, None) else { continue; };
        for (candidate, actual, ranges) in pairs {
            if adc3_live_route_compatible(&route, &actual)
                && claimed.claimed_interfaces.contains(&actual.control)
                && claimed.claimed_interfaces.contains(&actual.interface)
                && actual.implicit.as_deref().map_or(true, |peer| claimed.claimed_interfaces.contains(&peer.interface))
                && adc3_candidate_matches_ranges(&candidate, &ranges, requested) {
                options.push((candidate, actual));
            }
        }
    }
    // Keep the selected OUT when another alternate of its proven IN pipe works.
    // Otherwise prefer the original container, lower excess bandwidth and stable IDs.
    options.sort_by_key(|(candidate, actual)| (
        actual.alt != route.alt, candidate.subslot_size != route.pcm.1,
        candidate.max_packet_bytes.saturating_sub(required_max_packet_bytes(rate,
            candidate.service_interval_us, usize::from(candidate.channels) * usize::from(candidate.subslot_size))),
        actual.alt,
        candidate.implicit_feedback.map(|source| (std::cmp::Reverse(source.max_packet_bytes / source.bytes_per_frame),
            source.max_packet_bytes, source.alt_setting)),
    ));
    for (_, mut actual) in options {
        if !adc3_live_pair_available(&actual, |interface, alternate|
            claimed.alternate_is_available(interface, alternate))? { continue; }
        prepare_adc3_implicit_power(claimed, &actual)?;
        actual.plan = route.plan.clone();
        actual.clock_context = route.clock_context.clone();
        // Enumeration and bitmap reads must not conceal clock drift. Reuse the
        // activation check with frequency writes disabled before pinning the pair.
        validate_adc3_clock(&claimed.handle, &actual, rate, false)?;
        actual.live_alternate = true;
        return Ok(actual);
    }
    Err("ADC3 no available same-clock playback alternate".into())
}

/// Preserve successful current paths; only a failed exact current configuration
/// may lead to an alternate selector transaction.
pub(super) fn negotiate_adc3_clock(claimed: &ClaimedHandle, candidate: &IsoStreamCandidate, rate: u32) -> Result<Adc3Route, String> {
    let device = claimed.handle.device();
    let route = adc3_route_for_candidate(&device, &claimed.handle, candidate)?
        .ok_or("ADC3 route disappeared")?;
    if route.plan.is_some() {
        apply_adc3_clock_plan(claimed, &route, rate)?;
        return adc3_live_alternate(claimed, route, candidate.usb_speed, rate);
    }
    let current_frequency = if let Some(source) = route.clock.as_ref() {
        let mut bytes = [0; 4];
        read_exact(&claimed.handle, route.control, source.id, 1, 1, &mut bytes)?;
        Some(FrequencySnapshot { source: source.id, original: u32::from_le_bytes(bytes), target: route.source_rate(rate)? })
    } else { None };
    let current_result = if current_frequency.is_some_and(|f| f.original == 0) {
        Err("ADC3 current source frequency is zero".into())
    } else { configure_adc3(&claimed.handle, &route, rate) };
    let current_error = match current_result {
        Ok(()) => {
            let result = adc3_live_alternate(claimed, route.clone(), candidate.usb_speed, rate);
            if let Err(error) = &result {
                if let Some(frequency) = current_frequency.filter(|f| f.original != 0) {
                    if let Err(restore) = verify_adc3_rate_route(&claimed.handle, &route).and_then(|()|
                        restore_adc3_frequency(&mut Adc3LeaseClockIo { lease: &claimed.adc3_power,
                            control: route.control, ratio_route: Some(&route) }, frequency)) {
                        claimed.adc3_power.disarm();
                        return Err(format!("{error}; ADC3 current frequency restoration uncertain: {restore}"));
                    }
                }
            }
            return result;
        }
        Err(error) => error,
    };
    if let Some(frequency) = current_frequency.filter(|f| f.original != 0) {
        if let Err(error) = verify_adc3_rate_route(&claimed.handle, &route).and_then(|()|
            restore_adc3_frequency(&mut Adc3LeaseClockIo { lease: &claimed.adc3_power, control: route.control, ratio_route: Some(&route) }, frequency)) {
            claimed.adc3_power.disarm();
            return Err(format!("{current_error}; ADC3 current frequency restoration uncertain: {error}"));
        }
    }
    let config = device.active_config_descriptor().map_err(|e| e.to_string())?;
    let alt = config.interfaces().flat_map(|i| i.descriptors()).find(|d|
        d.interface_number() == candidate.interface_number && d.setting_number() == candidate.alt_setting)
        .ok_or("ADC3 alternate disappeared")?;
    let requested = PlaybackFormat { sample_rate: rate, channels: candidate.channels, bit_depth: candidate.bit_resolution, is_dop: false, dsd_transport: DsdTransportMode::None, dsd_bit_rate: 0 };
    for (alternative, route) in adc3_alternative_routes(claimed, &config, &alt, candidate.usb_speed, requested)? {
        if alternative.implicit_feedback != candidate.implicit_feedback { continue; }
        match apply_adc3_clock_plan(claimed, &route, rate) {
            Ok(()) => return adc3_live_alternate(claimed, route, candidate.usb_speed, rate),
            Err(error) if claimed.adc3_power.clock.lock().is_some() => return Err(error),
            Err(_) => {}
        }
    }
    Err(current_error)
}

pub(super) fn select_owned_stream_candidate(
    claimed: &ClaimedHandle,
    requested: PlaybackFormat,
    speed: Speed,
) -> Result<IsoStreamCandidate, SelectError> {
    let uac2_alternate = SESSION_STATE.lock().as_ref()
        .and_then(|state| state.negotiated_uac2_alternate.clone());
    if let Some(alternate) = uac2_alternate {
        return super::uac2_live_alternate::rebuild_uac2_live_alternate(
            claimed, requested, speed, &alternate,
        );
    }
    let device = claimed.handle.device();
    let saved = NEGOTIATED.lock().clone();
    if let Some(route) = saved.filter(|r| r.plan.is_some() || r.implicit.is_some() || r.live_alternate) {
        let config = device.active_config_descriptor().map_err(|e| SelectError::NoMatch(e.to_string()))?;
        let alt = config.interfaces().flat_map(|i| i.descriptors()).find(|d|
            d.interface_number() == route.interface && d.setting_number() == route.alt)
            .ok_or_else(|| SelectError::NoMatch("ADC3 negotiated alternate disappeared".into()))?;
        prepare_adc3_implicit_power(claimed, &route).map_err(SelectError::NoMatch)?;
        prepare_adc3_alternate_power(claimed, &config, &alt, false).map_err(SelectError::NoMatch)?;
        if route.plan.is_some() { guard_adc3_clock_mutation(claimed, &route).map_err(SelectError::NoMatch)?; }
        if route.plan.is_some() {
            for domain in route.domains.iter().filter(|d| adc3_route_domain_required(&route, d)) {
                claimed.adc3_power.acquire(route.control, domain).map_err(SelectError::NoMatch)?;
            }
        }
        let (candidate, mut actual, _) = adc3_alternate_pairs(&config, &claimed.handle, &alt, speed, false, route.plan.as_ref())
            .map_err(SelectError::NoMatch)?.into_iter()
            .find(|(_, actual, _)| actual.implicit == route.implicit)
            .ok_or_else(|| SelectError::NoMatch("ADC3 negotiated implicit source changed".into()))?;
        actual.clock_context = route.clock_context.clone();
        actual.live_alternate = route.live_alternate;
        if actual != route || !adc3_candidate_matches(&claimed.handle, &candidate, &actual, requested) {
            return Err(SelectError::NoMatch("ADC3 negotiated alternate clock changed".into()));
        }
        return Ok(candidate);
    }
    let initial = select_stream_candidate(
        &device,
        &claimed.handle,
        requested,
        speed,
        &claimed.claimed_interfaces,
    );
    let config = device
        .active_config_descriptor()
        .map_err(|e| SelectError::NoMatch(e.to_string()))?;
    if let Ok(candidate) = initial.as_ref() {
        let alt = config
            .interfaces()
            .flat_map(|i| i.descriptors())
            .find(|d| {
                d.interface_number() == candidate.interface_number
                    && d.setting_number() == candidate.alt_setting
            })
            .ok_or_else(|| SelectError::NoMatch("selected interface disappeared".into()))?;
        prepare_adc3_alternate_power(claimed, &config, &alt, false)
            .map_err(SelectError::NoMatch)?;
        if let Some(route) = adc3_route_for_candidate(&device, &claimed.handle, candidate).map_err(SelectError::NoMatch)? {
            prepare_adc3_implicit_power(claimed, &route).map_err(SelectError::NoMatch)?;
        }
        return initial;
    }
    if requested.dsd_transport != DsdTransportMode::None {
        return initial;
    }
    let mut refusal = None;
    for alt in config
        .interfaces()
        .flat_map(|i| i.descriptors())
        .filter(|d| {
            d.class_code() == 1
                && d.sub_class_code() == 2
                && d.protocol_code() == 0x30
                && d.setting_number() != 0
                && claimed.claimed_interfaces.contains(&d.interface_number())
                && d.endpoint_descriptors()
                    .any(|e| e.direction() == Direction::Out)
        })
    {
        // This bounded candidate probe is necessary only when a stopped domain
        // prevents read-only HC/clock discovery. It never activates an alt.
        let attempt = prepare_adc3_alternate_power(claimed, &config, &alt, false)
            .and_then(|_| adc3_alternate_pairs(&config, &claimed.handle, &alt, speed, false, None));
        match attempt {
            Ok(pairs) => {
                for (candidate, route, ranges) in pairs {
                    if candidate.implicit_feedback.map_or(true, |source| claimed.claimed_interfaces.contains(&source.interface_number))
                        && adc3_candidate_matches_ranges(&candidate, &ranges, requested) {
                        prepare_adc3_implicit_power(claimed, &route).map_err(SelectError::NoMatch)?;
                        return Ok(candidate);
                    }
                }
            }
            Err(error) => refusal = Some(error),
        }
        if let Ok(alternatives) = adc3_alternative_routes(claimed, &config, &alt, speed, requested) {
            for (candidate, route) in alternatives {
                if guard_adc3_clock_mutation(claimed, &route).is_ok() {
                    for domain in &route.domains {
                        if domain.entities.contains(&route.terminal) || domain.entities.iter().any(|id| route.plan.as_ref().is_some_and(|p| p.path.contains(id))) {
                            claimed.adc3_power.acquire(route.control, domain).map_err(SelectError::NoMatch)?;
                        }
                    }
                    prepare_adc3_implicit_power(claimed, &route).map_err(SelectError::NoMatch)?;
                    remember_adc3_route(route);
                    return Ok(candidate);
                }
            }
        }
        claimed.adc3_power.restore().map_err(SelectError::NoMatch)?;
    }
    match refusal {
        Some(error) => Err(SelectError::NoMatch(format!(
            "ADC3 playback unavailable: {error}"
        ))),
        None => initial,
    }
}
fn adc3_candidate_matches(
    handle: &DeviceHandle<Context>,
    candidate: &IsoStreamCandidate,
    route: &Adc3Route,
    requested: PlaybackFormat,
) -> bool {
    let ranges = adc3_wire_ranges(handle, route).unwrap_or_default();
    adc3_candidate_matches_ranges(candidate, &ranges, requested)
}
fn adc3_candidate_matches_ranges(candidate: &IsoStreamCandidate, ranges: &[SamplingFrequencySubrange], requested: PlaybackFormat) -> bool {
    if requested.dsd_transport != DsdTransportMode::None
        || requested.channels != candidate.channels
        || requested.bit_depth != candidate.bit_resolution
    {
        return false;
    }
    let supported = sampling_frequency_ranges_support_rate(ranges, requested.sample_rate);
    supported
        && candidate.implicit_feedback.map_or(true, |source| source.max_packet_bytes >= required_max_packet_bytes(
            requested.sample_rate, source.service_interval_us, source.bytes_per_frame))
        && candidate.max_packet_bytes
            >= required_max_packet_bytes(
                requested.sample_rate,
                candidate.service_interval_us,
                usize::from(candidate.channels) * usize::from(candidate.subslot_size),
            )
}

pub(super) fn adc3_badd_volume_topology(
    device: &Device<Context>,
    handle: &DeviceHandle<Context>,
    interface: u8,
    terminal: u8,
    channels: u16,
) -> Result<Option<AudioControlTopology>, String> {
    let config = device
        .active_config_descriptor()
        .map_err(|e| e.to_string())?;
    let (iad, profile, _) = owning_function(&config, handle, interface)?;
    if !matches!(profile, wire::FunctionProfile::Badd(_)) {
        return Ok(None);
    }
    if terminal != 1 || !(1..=2).contains(&channels) {
        return Err("invalid BADD playback volume path".into());
    }
    let mut controls = vec![3];
    controls.extend(std::iter::repeat(12).take(usize::from(channels)));
    Ok(Some(AudioControlTopology {
        interface_number: iad.b_first_interface,
        protocol: AudioControlProtocol::Uac3,
        streaming_interfaces: vec![interface],
        input_terminals: vec![InputTerminal {
            b_terminal_id: 1,
            w_terminal_type: 0x0101,
            b_assoc_terminal: 0,
            b_c_source_id: 9,
            b_nr_channels: channels,
            w_channel_config: 0,
            i_terminal: 0,
        }],
        feature_units: vec![FeatureUnit {
            b_unit_id: 2,
            b_source_id: 1,
            b_control_size: 4,
            bma_controls: controls,
        }],
        output_terminals: vec![OutputTerminal {
            b_terminal_id: 3,
            w_terminal_type: 0x0301,
            b_assoc_terminal: 0,
            b_source_id: 2,
            i_terminal: 0,
        }],
        sources: vec![(2, vec![1]), (3, vec![2])],
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn ratio_choice(factors: &[(u8, u16, u16)]) -> ClockChoice {
        ClockChoice { control_interface: 0, source_id: 9, resolution: ClockResolution::Resolved,
            path: factors.iter().map(|&(id, numerator, denominator)| ClockHop {
                id, kind: ClockHopKind::RateMultiplier { numerator, denominator },
            }).collect() }
    }
    #[test]
    fn multiplier_controls_require_readable_nonzero_exact_factors() {
        let mut reads = vec![];
        assert_eq!(adc3_multiplier_factors(wire::Controls(0b1101), |selector, bytes| {
            reads.push(selector);
            *bytes = if selector == 1 { 147u16 } else { 160u16 }.to_le_bytes();
            Ok(())
        }).unwrap(), (147, 160));
        assert_eq!(reads, vec![1, 2]);
        for controls in [0, 0b0110, 0b1001] {
            assert!(adc3_multiplier_factors(wire::Controls(controls), |_, _| Ok(())).is_err());
        }
        assert!(adc3_multiplier_factors(wire::Controls(0b0101), |_, _| Ok(())).is_err());
        assert!(adc3_multiplier_factors(wire::Controls(0b0101), |_, _| Err("short GET_CUR".into())).is_err());
    }
    #[test]
    fn multiplier_snapshot_rejects_equivalent_raw_drift() {
        let choice = ratio_choice(&[(7, 2, 4)]);
        assert!(verify_adc3_ratios(&choice, |_, selector, bytes| {
            *bytes = if selector == 1 { 1u16 } else { 2u16 }.to_le_bytes(); Ok(())
        }).is_err());
        assert!(verify_adc3_ratios(&choice, |id, selector, bytes| {
            assert_eq!(id, 7);
            *bytes = if selector == 1 { 2u16 } else { 4u16 }.to_le_bytes(); Ok(())
        }).is_ok());
    }
    #[test]
    fn multiplier_ranges_project_wire_rates_and_keep_other_exact_blocks() {
        let choice = ratio_choice(&[(7, 147, 160)]);
        let ranges = adc3_project_ranges(&choice, &[
            SamplingFrequencySubrange { min: 48_000, max: 96_000, res: 48_000 },
            SamplingFrequencySubrange { min: 48_001, max: 48_001, res: 0 },
        ]).unwrap();
        assert_eq!(ranges.len(), 1);
        assert_eq!((ranges[0].min, ranges[0].max, ranges[0].res), (44_100, 88_200, 44_100));
        assert!(adc3_project_ranges(&choice, &[
            SamplingFrequencySubrange { min: 48_000, max: 96_000, res: 0 },
        ]).is_err());
        assert_eq!(adc3_choice_ratio(&choice).unwrap().source_rate(44_100), Some(48_000));
    }
    fn pcm_candidate() -> IsoStreamCandidate {
        IsoStreamCandidate {
            configuration_number: 1,
            interface_number: 5, alt_setting: 1, endpoint_address: 1, endpoint_interval: 4,
            service_interval_us: 1000, usb_speed: Speed::High, max_packet_bytes: 768,
            sync_type: SyncType::Asynchronous, usage_type: UsageType::Data, format_tag: FORMAT_TAG_PCM,
            uac2_formats: None, native_dsd: None, channels: 2, subslot_size: 4, bit_resolution: 32,
            sample_rates: vec![48000, 96000], refresh: 0, synch_address: 0, feedback_endpoint: None,
            implicit_feedback: None, terminal_link: Some(1), is_uac1: false,
        }
    }
    fn implicit_pcm_candidate() -> IsoStreamCandidate {
        let mut source = pcm_candidate();
        source.interface_number = 6;
        source.endpoint_address = 0x81;
        source.usage_type = UsageType::FeedbackData;
        source
    }
    #[test]
    fn adaptive_capture_nominal_rate_uses_bus_units_and_source_rounding() {
        let fs = FeedforwardEndpoint { address: 2, interval: 1, service_us: 1000, bytes: 3, full_speed: true };
        assert_eq!(fs.nominal(48_000).unwrap(), vec![0, 0, 12]);
        assert_eq!(fs.nominal(44_100).unwrap(), vec![0x66, 0x06, 0x0b]);
        assert_eq!(fs.nominal(2).unwrap(), vec![32, 0, 0]);
        let hs = FeedforwardEndpoint { bytes: 4, full_speed: false, service_us: 125, ..fs };
        assert_eq!(hs.nominal(48_000).unwrap(), vec![0, 0, 6, 0]);
        assert_eq!(hs.nominal(44_100).unwrap(), vec![0x33, 0x83, 5, 0]);
        for interval in [1, 4, 16] {
            assert_eq!(FeedforwardEndpoint { interval, service_us: 125 << (interval - 1), ..hs }.nominal(44_100), hs.nominal(44_100));
        }
        assert!(fs.nominal(0).is_err());
        assert!(fs.nominal(u32::MAX).is_err());
        assert!(hs.nominal(u32::MAX).is_err());
        assert!(hs.nominal(32_767_999).is_ok());
        assert!(hs.nominal(32_768_000).is_err());
        assert!(FeedforwardEndpoint { bytes: 4, ..fs }.nominal(48_000).is_err());
    }
    #[test]
    fn enhanced_capture_feedforward_is_unsigned_q32_32_with_data_capacity_guard() {
        let pipe = FeedforwardEndpoint { address: 2, interval: 4, service_us: 1000,
            bytes: 8, full_speed: false };
        assert_eq!(pipe.nominal(48_000).unwrap(), vec![0, 0, 0, 0, 6, 0, 0, 0]);
        assert_eq!(pipe.nominal(44_100).unwrap(), vec![0x33, 0x33, 0x33, 0x83, 5, 0, 0, 0]);
        assert!(pipe.nominal(0).is_err());
        assert!(pipe.nominal(u32::MAX).is_ok());
        assert!(FeedforwardEndpoint { full_speed: true, ..pipe }.nominal(48_000).is_err());
        for interval in [1, 4, 16] {
            assert_eq!(FeedforwardEndpoint { interval, service_us: 125 << (interval - 1), ..pipe }
                .nominal(44_100), pipe.nominal(44_100));
        }
        assert!(validate_feedforward_capacity(pipe, 48_000, 196, 1000, 4).is_ok());
        assert!(validate_feedforward_capacity(pipe, 48_000, 191, 1000, 4).is_err());
        assert!(validate_enhanced_feedback_shape(2, 0x82, TransferType::Isochronous,
            SyncType::NoSync, UsageType::Feedback).is_ok());
        assert!(validate_enhanced_feedback_shape(0x82, 2, TransferType::Isochronous,
            SyncType::NoSync, UsageType::Feedback).is_ok());
        for (report, transfer, sync, usage) in [
            (0x83, TransferType::Isochronous, SyncType::NoSync, UsageType::Feedback),
            (2, TransferType::Isochronous, SyncType::NoSync, UsageType::Feedback),
            (0x82, TransferType::Interrupt, SyncType::NoSync, UsageType::Feedback),
            (0x82, TransferType::Isochronous, SyncType::Adaptive, UsageType::Feedback),
            (0x82, TransferType::Isochronous, SyncType::NoSync, UsageType::Data),
        ] {
            assert!(validate_enhanced_feedback_shape(2, report, transfer, sync, usage).is_err());
        }
        let descriptor = [7, 5, 2, 0x11, 8, 0, 4];
        for speed in [Speed::Super, Speed::SuperPlus] {
            assert!(validate_feedforward_descriptor(&descriptor, speed).is_ok());
            assert!(feedback_packet_bytes_supported(speed, 8));
            assert!(feedback_packet_bytes_supported(speed, 4));
        }
        for speed in [Speed::Full, Speed::High, Speed::Unknown] {
            assert!(validate_feedforward_descriptor(&descriptor, speed).is_err());
        }
    }

    #[test]
    fn adaptive_implicit_inventory_is_duplex_only_and_never_hides_extra_endpoints() {
        let full = wire::FunctionProfile::Full;
        assert!(adaptive_implicit_inventory(full, true, 1, 0, false));
        assert!(!adaptive_implicit_inventory(full, false, 1, 0, false));
        assert!(!adaptive_implicit_inventory(full, true, 1, 0, true));
        for endpoints in [0, 2, 3] {
            assert!(!adaptive_implicit_inventory(full, true, endpoints, 0, false));
        }
        assert!(!adaptive_implicit_inventory(full, true, 1, 2, false));
        assert!(!adaptive_implicit_inventory(wire::FunctionProfile::Badd(wire::BaddProfile::Headset),
            true, 1, 0, false));
    }
    #[test]
    fn adaptive_capture_preserves_extended_in_capacity_for_rate_checks() {
        let pipe = FeedforwardEndpoint { address: 2, interval: 4, service_us: 1000, bytes: 4, full_speed: false };
        // The SSP companion's ordinary wBytesPerInterval is the sentinel 1.
        let extra = [6, 0x30, 0, 0x80, 1, 0, 8, 0x31, 0, 0, 0, 0x10, 0, 0];
        let (capacity, service, _) = ss_periodic_geometry(1024, 4, &extra, true).unwrap();
        assert_eq!(capacity, 4096);
        assert!(validate_feedforward_capacity(pipe, 192_000, capacity, service, 8).is_ok());
        assert!(validate_feedforward_capacity(pipe, 192_000, 1, service, 8).is_err());
        assert!(validate_feedforward_capacity(pipe, 32_768_000, usize::MAX, service, 8).is_err());
    }

    #[test]
    fn adaptive_capture_feedforward_standard_endpoint_is_strict() {
        let valid = [7, 5, 2, 0x11, 3, 0, 1];
        assert!(validate_feedforward_descriptor(&valid, Speed::Full).is_ok());
        for (offset, value) in [(0, 9), (1, 0x25), (2, 0), (2, 0x82), (2, 0x12),
            (3, 1), (3, 0x15), (3, 0x31), (3, 0x91), (4, 4), (5, 8), (6, 0), (6, 17)] {
            let mut bad = valid; bad[offset] = value;
            assert!(validate_feedforward_descriptor(&bad, Speed::Full).is_err(), "{bad:?}");
        }
        assert!(validate_feedforward_descriptor(&valid[..6], Speed::Full).is_err());
        assert!(validate_feedforward_descriptor(&valid, Speed::High).is_err());
        let high = [7, 5, 2, 0x11, 4, 0, 4];
        for speed in [Speed::High, Speed::Super, Speed::SuperPlus] {
            assert!(validate_feedforward_descriptor(&high, speed).is_ok());
        }
        assert!(validate_feedforward_descriptor(&high, Speed::Unknown).is_err());
    }

    #[test]
    fn full_adc3_adaptive_sink_uses_nominal_pacing_but_source_requires_feedforward() {
        let full = wire::FunctionProfile::Full;
        assert!(adc3_sync_supported(full, false, SyncType::Adaptive));
        assert_eq!(admitted_pacing(SyncType::Adaptive, None), crate::capabilities::Pacing::Adaptive);
        assert!(adc3_sync_supported(full, true, SyncType::Adaptive));
        assert!(!adc3_sync_supported(full, false, SyncType::NoSync));
        let badd = wire::FunctionProfile::Badd(wire::BaddProfile::Headset);
        assert!(!adc3_sync_supported(badd, false, SyncType::Adaptive));
        for capture in [true, false] {
            for profile in [full, badd] {
                assert!(adc3_sync_supported(profile, capture, SyncType::Asynchronous));
                assert!(adc3_sync_supported(profile, capture, SyncType::Synchronous));
            }
        }
    }
    #[test]
    fn adc3_explicit_feedback_remains_first_and_malformed_or_disabled_feedback_is_refused() {
        let full = wire::FunctionProfile::Full;
        let badd = wire::FunctionProfile::Badd(wire::BaddProfile::Headset);
        for profile in [full, badd] {
            assert_eq!(adc3_requires_implicit(profile, false, SyncType::Asynchronous, true, true, false), Ok(false));
            assert!(adc3_requires_implicit(profile, false, SyncType::Asynchronous, true, true, true).is_err());
            assert!(adc3_requires_implicit(profile, false, SyncType::Asynchronous, false, true, false).is_err());
            assert_eq!(adc3_requires_implicit(profile, true, SyncType::Asynchronous, false, false, true), Ok(false));
        }
        assert_eq!(adc3_requires_implicit(full, false, SyncType::Asynchronous, false, false, false), Ok(true));
        assert!(adc3_requires_implicit(full, false, SyncType::Asynchronous, false, false, true).is_err());
        assert!(adc3_requires_implicit(badd, false, SyncType::Asynchronous, false, false, false).is_err());
        assert_eq!(adc3_requires_implicit(full, false, SyncType::Adaptive, false, false, true), Ok(false));
    }
    #[test]
    fn adc3_preflight_target_plans_must_prove_one_source_pipe() {
        let proof = ImplicitFeedbackSource { interface_number: 6, alt_setting: 1, address: 0x81,
            bytes_per_frame: 8, service_interval_us: 1000, max_packet_bytes: 768 };
        assert_eq!(adc3_preflight_pipe(&[]), None);
        assert_eq!(adc3_preflight_pipe(&[proof]), Some(6));
        let mut alternate = proof; alternate.alt_setting = 3; alternate.bytes_per_frame = 4;
        assert_eq!(adc3_preflight_pipe(&[proof, alternate, proof]), Some(6));
        let mut another_interface = proof; another_interface.interface_number = 7;
        assert_eq!(adc3_preflight_pipe(&[proof, another_interface]), None);
        let mut another_address = proof; another_address.address = 0x82;
        assert_eq!(adc3_preflight_pipe(&[proof, another_address]), None);
    }
    #[test]
    fn adc3_ranges_honor_both_wire_capacities_and_step_alignment() {
        let mut sink = pcm_candidate();
        let ranges = [SamplingFrequencySubrange { min: 44100, max: 192000, res: 100 }];
        let bounded = adc3_capacity_ranges(&sink, &ranges);
        assert_eq!((bounded[0].min, bounded[0].max, bounded[0].res), (44100, 96000, 100));
        sink.implicit_feedback = Some(ImplicitFeedbackSource { interface_number: 6, alt_setting: 2,
            address: 0x81, bytes_per_frame: 6, service_interval_us: 1000, max_packet_bytes: 300 });
        assert_eq!(adc3_capacity_ranges(&sink, &ranges)[0].max, 50000);
        sink.implicit_feedback.as_mut().unwrap().max_packet_bytes = 264;
        assert!(adc3_capacity_ranges(&sink, &ranges).is_empty());
        // The same formula handles validated SuperSpeed companion capacity.
        sink.usb_speed = Speed::Super;
        sink.service_interval_us = 125;
        sink.max_packet_bytes = 48;
        sink.implicit_feedback.as_mut().unwrap().service_interval_us = 125;
        sink.implicit_feedback.as_mut().unwrap().max_packet_bytes = 42;
        assert_eq!(adc3_capacity_ranges(&sink, &ranges)[0].max, 48000);
    }
    #[test]
    fn superspeed_uses_advertised_service_capacity_and_interval() {
        // 1024 * 4 packets * 3 bursts = 12288 theoretical bytes; the
        // endpoint advertises only 10000 bytes per 1ms service interval.
        let d = [6, 0x30, 3, 2, 0x10, 0x27];
        assert_eq!(ss_periodic_geometry(1024, 4, &d, true).unwrap(), (10000, 1000, d));
        assert_eq!(ss_periodic_geometry(1024, 1, &d, true).unwrap().1, 125);
        assert_eq!(ss_periodic_geometry(1024, 16, &d, true).unwrap().1, 4_096_000);
        assert!(10000 >= required_max_packet_bytes(384_000, 1000, 24));
        assert!(1024 < required_max_packet_bytes(384_000, 1000, 24));
        // Ordinary companions also apply at SuperSpeedPlus link rates.
        assert_eq!(ss_periodic_geometry(1024, 4, &d, true).unwrap().0, 10000);
    }

    #[test]
    fn superspeed_rejects_missing_malformed_reserved_and_extended_geometry() {
        let valid = [6, 0x30, 0, 0, 0, 4];
        for extra in [
            vec![], vec![6, 0x30], vec![5, 0x30, 0, 0, 0, 4],
            vec![6, 0x30, 16, 0, 0, 4], vec![6, 0x30, 0, 3, 0, 4],
            vec![6, 0x30, 0, 1, 0, 4],
            vec![6, 0x30, 0, 4, 0, 4], vec![6, 0x30, 0, 0x80, 0, 4],
            vec![6, 0x30, 0, 0, 0, 0], vec![6, 0x30, 0, 0, 1, 4],
            vec![6, 0x30, 0, 0, 0, 4, 6, 0x30, 0, 0, 0, 4],
            vec![6, 0x30, 0, 0, 0, 4, 8, 0x31, 0, 0, 0, 0, 0, 0],
            vec![6, 0x30, 0, 0, 0, 4, 1],
            vec![6, 0x30, 0, 0, 0, 4, 7, 0x25, 1],
        ] {
            assert!(ss_periodic_geometry(1024, 4, &extra, true).is_err(), "{extra:?}");
        }
        for packet in [0, 1025, 0x0c00, 0x2400] {
            assert!(ss_periodic_geometry(packet, 4, &valid, true).is_err());
        }
        for interval in [0, 17, 255] {
            assert!(ss_periodic_geometry(1024, interval, &valid, true).is_err());
        }
        assert!(ss_periodic_geometry(512, 4, &[6, 0x30, 1, 0, 0, 4], true).is_err());
        let maximum = [6, 0x30, 15, 2, 0, 0xc0];
        assert_eq!(ss_periodic_geometry(1024, 1, &maximum, true).unwrap().0, 49152);
    }

    #[test]
    fn superspeed_plus_uses_exact_extended_capacity_and_rejects_unproven_link_bounds() {
        // USB 3.2 rev 1.1 sections 9.6.7/9.6.8, Tables 9-28/9-29:
        // immediate SSP descriptor, reserved word zero, SS wBytes = 1.
        let descriptor = |capacity: u32| {
            let mut d = vec![6, 0x30, 15, 0x80, 1, 0, 8, 0x31, 0, 0];
            d.extend_from_slice(&capacity.to_le_bytes());
            d
        };
        for capacity in [1, 49_152, 49_153, 65_536, 98_303] {
            let d = descriptor(capacity);
            assert_eq!(ss_periodic_geometry(1024, 1, &d, true).unwrap().0, capacity as usize);
            // The two low Mult bits are ignored when SSP ISO Companion is set.
            let mut ignored_mult = d.clone();
            ignored_mult[3] |= 3;
            assert_eq!(ss_periodic_geometry(1024, 4, &ignored_mult, true).unwrap().0, capacity as usize);
            assert!(ss_periodic_geometry(1024, 1, &d, false).is_err());
            assert!(ss_feedback_geometry(1024, 1, &d, true).is_err());
        }
        // Table 9-29 requires strictly less than the negotiated link bound.
        // Without lanes/rate, only the common 49152 * 2 bound is proven.
        for capacity in [0, 98_304, 196_607, u32::MAX] {
            assert!(ss_periodic_geometry(1024, 1, &descriptor(capacity), true).is_err());
        }
        let valid = descriptor(65_536);
        for (offset, value) in [(3, 0x84), (4, 0), (5, 1), (6, 7), (7, 0x30), (8, 1), (9, 1)] {
            let mut bad = valid.clone();
            bad[offset] = value;
            assert!(ss_periodic_geometry(1024, 1, &bad, true).is_err(), "{bad:?}");
        }
        for length in 6..14 {
            assert!(ss_periodic_geometry(1024, 1, &valid[..length], true).is_err());
        }
        let mut duplicate = valid.clone();
        duplicate.extend_from_slice(&valid[6..]);
        assert!(ss_periodic_geometry(1024, 1, &duplicate, true).is_err());
        let mut delayed = valid.clone();
        delayed.splice(6..6, [2, 0x25]);
        assert!(ss_periodic_geometry(1024, 1, &delayed, true).is_err());
    }

    #[test]
    fn superspeed_periodic_feedback_and_badd_remain_unburst_and_exact() {
        let feedback = [6, 0x30, 0, 0, 4, 0];
        assert_eq!(ss_periodic_geometry(4, 4, &feedback, true).unwrap().0, 4);
        assert_eq!(ss_periodic_geometry(4, 4, &feedback, false).unwrap().0, 4);
        assert!(ss_periodic_geometry(4, 4, &[6, 0x30, 0, 1, 4, 0], false).is_err());
        assert!(ss_feedback_geometry(4, 4, &feedback, true).is_ok());
        assert!(ss_feedback_geometry(4, 4, &[], true).is_err());
        assert!(ss_feedback_geometry(4, 0, &feedback, true).is_err());
        assert!(ss_feedback_geometry(8, 4, &feedback, true).is_err());
        assert!(ss_feedback_geometry(4, 4, &[6, 0x30, 1, 0, 4, 0], true).is_err());
        assert!(ss_feedback_geometry(4, 4, &[6, 0x30, 0, 0, 3, 0], true).is_err());
        for bytes in [192u16, 196, 288, 294] {
            let [lo, hi] = bytes.to_le_bytes();
            let d = [6, 0x30, 0, 0, lo, hi];
            assert!(badd_ss_continuous_geometry(bytes, 4, Some(d)).is_ok());
            assert!(badd_ss_continuous_geometry(bytes, 3, Some(d)).is_err());
            assert!(badd_ss_continuous_geometry(bytes + 1, 4, Some(d)).is_err());
            let mut burst = d;
            burst[2] = 1;
            assert!(badd_ss_continuous_geometry(bytes, 4, Some(burst)).is_err());
            burst[2] = 0;
            burst[3] = 1;
            assert!(badd_ss_continuous_geometry(bytes, 4, Some(burst)).is_err());
        }
    }

    #[test]
    fn badd_whole_function_direction_and_sync_contract() {
        let out = BaddInterfaceEvidence {
            capture: false,
            asynchronous: true,
        };
        let input = BaddInterfaceEvidence {
            capture: true,
            asynchronous: true,
        };
        assert!(validate_badd_inventory(wire::BaddProfile::Headset, &[out, input]).is_ok());
        assert!(validate_badd_inventory(wire::BaddProfile::GenericIo, &[out]).is_ok());
        assert!(validate_badd_inventory(wire::BaddProfile::Microphone, &[input]).is_ok());
        assert!(validate_badd_inventory(wire::BaddProfile::Headset, &[out, out]).is_err());
        assert!(validate_badd_inventory(wire::BaddProfile::Headset, &[out]).is_err());
        assert!(validate_badd_inventory(wire::BaddProfile::Headphone, &[input]).is_err());
        assert!(validate_badd_inventory(wire::BaddProfile::Speaker, &[out, input]).is_err());
        assert!(validate_badd_inventory(wire::BaddProfile::GenericIo, &[]).is_err());
        assert!(validate_badd_inventory(
            wire::BaddProfile::Headset,
            &[
                out,
                BaddInterfaceEvidence {
                    capture: true,
                    asynchronous: false
                }
            ]
        )
        .is_err());
    }
    #[test]
    fn badd_capture_has_its_own_terminal_domain_and_recovery_geometry() {
        let profile = wire::FunctionProfile::Badd(wire::BaddProfile::Microphone);
        let capture = domains(profile, &[], true).unwrap();
        let playback = domains(profile, &[], false).unwrap();
        assert_eq!(capture[0].id, 11);
        assert_eq!(capture[0].entities, vec![4, 5, 6]);
        assert_eq!(playback[0].id, 10);
        assert_eq!(capture[0].recovery_d1_micros(), 30_000);
        assert_eq!(capture[0].recovery_d2_micros(), 300_000);
    }

    #[test]
    fn capture_requires_output_terminal_and_uses_its_clock_field() {
        let output = [19, 0x24, 3, 6, 1, 1, 0, 4, 9, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        let input = [
            20, 0x24, 2, 1, 1, 1, 0, 8, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0,
        ];
        let descriptors = vec![output.as_slice(), input.as_slice()];
        assert_eq!(linked_terminal_clock(&descriptors, 6, true).unwrap(), 9);
        assert_eq!(linked_terminal_clock(&descriptors, 1, false).unwrap(), 8);
        assert!(linked_terminal_clock(&descriptors, 1, true).is_err());
        assert!(linked_terminal_clock(&descriptors, 6, false).is_err());
        let mut malformed = output;
        malformed[4] = 2;
        assert!(linked_terminal_clock(&[malformed.as_slice()], 6, true).is_err());
    }

    #[derive(Default)]
    struct PowerMock {
        states: std::collections::HashMap<(u8, u8), u8>,
        events: Vec<String>,
        fail: Option<u8>,
    }
    impl PowerIo for PowerMock {
        fn read(&mut self, ac: u8, id: u8) -> Result<u8, String> {
            self.events.push(format!("read:{ac}:{id}"));
            Ok(*self.states.get(&(ac, id)).unwrap_or(&0))
        }
        fn write(&mut self, ac: u8, id: u8, state: u8) -> Result<(), String> {
            self.events.push(format!("write:{ac}:{id}:{state}"));
            if state == 0 && self.fail == Some(id) {
                return Err("wake failed".into());
            }
            self.states.insert((ac, id), state);
            Ok(())
        }
        fn delay(&mut self, micros: u32) {
            self.events.push(format!("wait:{micros}"));
        }
    }
    fn domain(id: u8) -> wire::PowerDomain {
        wire::PowerDomain {
            id,
            recovery_d1: 600,
            recovery_d2: 6000,
            entities: vec![1],
            string_id: 0,
        }
    }
    #[test]
    fn scoped_power_handback_preserves_external_drift_and_restores_other_domains() {
        let mut io = PowerMock::default();
        io.states.insert((4, 10), 2); // Another owner changed our D0 state.
        io.states.insert((4, 11), 0);
        assert!(restore_domains_checked(&mut io, vec![(4, 11, 2), (4, 10, 1)]).is_err());
        assert_eq!(io.states[&(4, 10)], 2);
        assert_eq!(io.states[&(4, 11)], 2);
        assert!(!io.events.iter().any(|e| e.starts_with("write:4:10:")));
    }
    #[test]
    fn capture_alternative_graph_uses_output_terminal_clock_and_writable_pins() {
        let mut bytes = vec![19, 0x24, 3, 6, 1, 1, 0, 4, 10, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        bytes.extend_from_slice(&[13, 0x24, 0x0c, 10, 2, 20, 21, 3, 0, 0, 0, 0, 0]);
        for id in [20, 21] {
            bytes.extend_from_slice(&[12, 0x24, 0x0b, id, 1, 7, 0, 0, 0, 0, 0, 0]);
        }
        let plans = ClockGraph::parse_adc3(&bytes).unwrap().enumerate_plans(6, |_| Ok(1)).unwrap();
        assert_eq!(plans.iter().map(|p| p.source_id).collect::<Vec<_>>(), vec![20, 21]);
        assert_eq!(plans[1].selectors[0].original, 1);
        assert_eq!(plans[1].selectors[0].target, 2);
        let descriptors: Vec<_> = DescriptorIter::new(&bytes).collect();
        assert_eq!(linked_terminal_clock(&descriptors, 6, true).unwrap(), 10);
        assert!(linked_terminal_clock(&descriptors, 6, false).is_err());
        bytes[19 + 7] = 1; // Read-only selector keeps only the selected source.
        assert_eq!(ClockGraph::parse_adc3(&bytes).unwrap().enumerate_plans(6, |_| Ok(1)).unwrap().len(), 1);
    }
    #[test]
    fn shared_domains_wake_once_and_restore_after_last_lease_in_reverse_order() {
        let mut io = PowerMock::default();
        io.states.insert((4, 10), 1);
        io.states.insert((4, 11), 2);
        let mut pending = vec![];
        acquire_domain(&mut io, &mut pending, 4, &domain(10)).unwrap();
        acquire_domain(&mut io, &mut pending, 4, &domain(10)).unwrap();
        acquire_domain(&mut io, &mut pending, 4, &domain(11)).unwrap();
        assert_eq!(
            io.events,
            vec![
                "read:4:10",
                "write:4:10:0",
                "wait:30000",
                "read:4:10",
                "read:4:11",
                "write:4:11:0",
                "wait:300000",
                "read:4:11"
            ]
        );
        io.events.push("transfers-retired".into());
        restore_domains(&mut io, pending).unwrap();
        assert_eq!(
            &io.events[8..],
            &[
                "transfers-retired",
                "write:4:11:2",
                "read:4:11",
                "write:4:10:1",
                "read:4:10"
            ]
        );
    }
    #[test]
    fn failed_wake_rolls_back_partial_batch_and_unknown_state_never_writes() {
        let mut io = PowerMock::default();
        io.states.insert((4, 10), 1);
        io.states.insert((4, 11), 2);
        io.fail = Some(11);
        let mut pending = vec![];
        acquire_domain(&mut io, &mut pending, 4, &domain(10)).unwrap();
        assert!(acquire_domain(&mut io, &mut pending, 4, &domain(11)).is_err());
        restore_domains(&mut io, pending).unwrap();
        assert_eq!(io.states.get(&(4, 10)), Some(&1));
        assert_eq!(io.states.get(&(4, 11)), Some(&2));
        let mut pending = vec![];
        io.states.insert((4, 12), 9);
        io.events.clear();
        assert!(acquire_domain(&mut io, &mut pending, 4, &domain(12)).is_err());
        assert!(pending.is_empty());
        assert_eq!(io.events, vec!["read:4:12"]);
    }
    fn live_test_route() -> Adc3Route {
        Adc3Route {
            config: 1,
            control: 4,
            first: 4,
            count: 2,
            profile: wire::FunctionProfile::Full,
            interface: 5,
            alt: 1,
            terminal: 1,
            endpoint: (1, 4, 196, 1000),
            packet_bytes: 196,
            sync_type: SyncType::Asynchronous,
            usage_type: UsageType::Data,
            feedback: Some((0x81, 4, 4)),
            feedforward: None,
            implicit_feedforward: false,
            implicit: None,
            companions: vec![],
            extended_companions: vec![],
            pcm: (2, 2, 16),
            frequency_limits: vec![],
            cluster: Some(wire::Cluster {
                id: 1,
                channels: vec![wire::Channel::Information {
                    purpose: 1,
                    relationship: wire::ChannelRelationship::Mono,
                    group: 0,
                }],
            }),
            header: None,
            clock: None,
            choice: Some(ClockChoice {
                control_interface: 4,
                source_id: 9,
                resolution: ClockResolution::Resolved,
                path: vec![ClockHop {
                    id: 8,
                    kind: ClockHopKind::Selector {
                        selected_pin: 1,
                        pin_count: 2,
                    },
                }],
            }),
            domains: vec![domain(10)],
            plan: None,
            clock_context: None,
            live_alternate: false,
        }
    }

    #[test]
    fn live_alternate_filter_preserves_exact_function_clock_and_pcm_shape() {
        let expected = live_test_route();
        let mut other = expected.clone();
        other.alt = 2;
        other.pcm.1 = 4; // A padded container remains the same exact PCM format.
        assert!(adc3_live_route_compatible(&expected, &other));
        let mutations: [fn(&mut Adc3Route); 7] = [
            |route: &mut Adc3Route| route.interface += 1,
            |route: &mut Adc3Route| route.terminal += 1,
            |route: &mut Adc3Route| route.first += 1,
            |route: &mut Adc3Route| route.pcm.0 += 1,
            |route: &mut Adc3Route| route.pcm.2 = 24,
            |route: &mut Adc3Route| route.choice.as_mut().unwrap().source_id += 1,
            |route: &mut Adc3Route| route.choice.as_mut().unwrap().path.push(ClockHop {
                id: 7, kind: ClockHopKind::RateMultiplier { numerator: 2, denominator: 1 },
            }),
        ];
        for mutate in mutations {
            let mut changed = other.clone();
            mutate(&mut changed);
            assert!(!adc3_live_route_compatible(&expected, &changed));
        }
        let mut paired = expected.clone();
        let mut input = expected.clone();
        input.interface = 6;
        input.endpoint.0 = 0x81;
        paired.implicit = Some(Box::new(input));
        let mut replacement = paired.clone();
        replacement.implicit.as_mut().unwrap().alt = 3;
        assert!(adc3_live_route_compatible(&paired, &replacement));
        replacement.implicit.as_mut().unwrap().control += 1;
        assert!(!adc3_live_route_compatible(&paired, &replacement));
        replacement.implicit.as_mut().unwrap().control -= 1;
        replacement.implicit.as_mut().unwrap().endpoint.0 = 0x82;
        assert!(!adc3_live_route_compatible(&paired, &replacement));
        let candidate = pcm_candidate();
        let requested = PlaybackFormat { sample_rate: 48_000, channels: candidate.channels,
            bit_depth: candidate.bit_resolution, is_dop: false, dsd_transport: DsdTransportMode::None,
            dsd_bit_rate: 0 };
        let ranges = [SamplingFrequencySubrange { min: 48_000, max: 48_000, res: 0 }];
        assert!(adc3_candidate_matches_ranges(&candidate, &ranges, requested));
        assert!(!adc3_candidate_matches_ranges(&candidate, &ranges,
            PlaybackFormat { sample_rate: 44_100, ..requested }));
    }

    #[test]
    fn live_availability_only_rejects_clean_false_and_propagates_peer_read_failure() {
        let mut route = live_test_route();
        let mut peer = route.clone();
        peer.interface = 6;
        route.implicit = Some(Box::new(peer));
        let mut reads = Vec::new();
        assert_eq!(adc3_live_pair_available(&route, |interface, _| {
            reads.push(interface);
            Ok(interface != route.interface)
        }), Ok(false));
        assert_eq!(reads, vec![5, 6]);
        assert_eq!(adc3_live_pair_available(&route, |interface, _| {
            if interface == 6 { Err("malformed peer bitmap".into()) } else { Ok(false) }
        }), Err("malformed peer bitmap".into()));
        assert!(adc3_live_pair_available(&route, |_, _| Err("failed GET_CUR".into())).is_err());
        assert_eq!(adc3_live_pair_available(&route, |_, _| Ok(true)), Ok(true));
    }

    #[test]
    fn negotiated_identity_rejects_configuration_cluster_and_pin_drift() {
        let route = live_test_route();
        assert_eq!(
            capture_channel_map(&route).unwrap().positions(),
            &[crate::capture::ChannelPosition::Adc3Information {
                purpose: 1,
                relationship: 1,
                group: 0
            }]
        );
        let mut out = route.clone();
        out.count = 3;
        out.clock = Some(wire::ClockSource { id: 9, attributes: 1, controls: wire::Controls(7), reference_terminal: 0, string_id: 0 });
        let mut input = out.clone();
        input.interface = 6;
        input.terminal = 6;
        input.endpoint.0 = 0x81;
        let sink = pcm_candidate();
        let source = implicit_pcm_candidate();
        assert!(adc3_implicit_pair(&sink, &out, &source, &input));
        // Adaptive feedforward reverses the timing direction. Each endpoint
        // keeps its own valid interval and capacity rather than mirroring IN.
        let adaptive_candidate = || IsoStreamCandidate {
            sync_type: SyncType::Adaptive,
            usage_type: UsageType::FeedbackData,
            subslot_size: 2,
            bit_resolution: 16,
            max_packet_bytes: 196,
            ..pcm_candidate()
        };
        let adaptive = adaptive_candidate();
        let mut adaptive_out = out.clone(); adaptive_out.feedback = None;
        adaptive_out.sync_type = SyncType::Adaptive; adaptive_out.usage_type = UsageType::FeedbackData;
        let mut adaptive_in = input.clone(); adaptive_in.implicit_feedforward = true;
        adaptive_in.sync_type = SyncType::Adaptive;
        adaptive_in.endpoint.3 = 125; adaptive_in.packet_bytes = 28;
        assert!(validate_adaptive_duplex(&adaptive, &adaptive_out, 48_000, &adaptive_in, 48_000).is_ok());
        for mutation in 0..11 {
            let mut bad_out = adaptive_candidate(); let mut bad_input = adaptive_in.clone();
            let mut bad_route = adaptive_out.clone();
            match mutation {
                0 => bad_out.usage_type = UsageType::Data,
                1 => bad_out.sync_type = SyncType::Asynchronous,
                2 => bad_out.interface_number = bad_input.interface,
                3 => bad_input.first += 1,
                4 => bad_input.clock.as_mut().unwrap().id += 1,
                5 => bad_input.packet_bytes = 1,
                6 => bad_route.packet_bytes = 1,
                7 => bad_route.feedback = Some((0x82, 4, 4)),
                8 => bad_route.usage_type = UsageType::Data,
                9 => bad_route.sync_type = SyncType::Synchronous,
                _ => bad_input.choice.as_mut().unwrap().path[0].kind = ClockHopKind::Selector { selected_pin: 2, pin_count: 2 },
            }
            assert!(validate_adaptive_duplex(&bad_out, &bad_route, 48_000, &bad_input, 48_000).is_err(), "mutation {mutation}");
        }
        assert!(validate_adaptive_duplex(&adaptive, &adaptive_out, 96_000, &adaptive_in, 48_000).is_err());
        let mut direct = adaptive_in.clone(); direct.feedforward = Some(FeedforwardEndpoint {
            address: 2, interval: 4, service_us: 1000, bytes: 4, full_speed: false });
        assert!(validate_adaptive_duplex(&adaptive, &adaptive_out, 48_000, &direct, 48_000).is_err());
        let mut scaled_out = out.clone();
        scaled_out.choice = Some(ratio_choice(&[(7, 2, 4)]));
        let mut scaled_in = input.clone();
        scaled_in.choice = Some(ratio_choice(&[(8, 1, 2)]));
        assert!(adc3_implicit_pair(&sink, &scaled_out, &source, &scaled_in));
        scaled_in.choice = Some(ratio_choice(&[(8, 1, 1)]));
        assert!(!adc3_implicit_pair(&sink, &scaled_out, &source, &scaled_in));
        // Shared entities must retain raw factors, even for equivalent ratios.
        scaled_in.choice = Some(ratio_choice(&[(7, 1, 2)]));
        assert!(!adc3_implicit_pair(&sink, &scaled_out, &source, &scaled_in));
        let pair = (ImplicitFeedbackSource { interface_number: 6, alt_setting: 1, address: 0x81,
            bytes_per_frame: 8, service_interval_us: 1000, max_packet_bytes: 768 }, input.clone());
        assert!(adc3_unique_implicit(vec![]).is_err());
        assert!(adc3_unique_implicit(vec![pair.clone()]).is_ok());
        assert!(adc3_unique_implicit(vec![pair.clone(), pair.clone()]).is_ok());
        let mut other = pair.clone(); other.0.interface_number = 7;
        assert!(adc3_unique_implicit(vec![pair.clone(), other]).is_err());
        let mut other = pair.clone(); other.0.address = 0x82;
        assert!(adc3_unique_implicit(vec![pair.clone(), other]).is_err());
        let mut narrow = pair.clone(); narrow.0.alt_setting = 2; narrow.0.bytes_per_frame = 4;
        assert_eq!(adc3_unique_implicit(vec![pair.clone(), narrow.clone()]).unwrap()[0].0.alt_setting, 2);
        let mut lean = narrow.clone(); lean.0.alt_setting = 3; lean.0.bytes_per_frame = 2; lean.0.max_packet_bytes = 384;
        assert_eq!(adc3_unique_implicit(vec![pair.clone(), narrow, lean.clone()]).unwrap()[0].0.alt_setting, 3);
        let mut stable = lean.clone(); stable.0.alt_setting = 1;
        assert_eq!(adc3_unique_implicit(vec![lean, stable]).unwrap()[0].0.alt_setting, 1);
        // Same physical pipe with disjoint AS frequency limits: capacity rank
        // alone cannot decide which peer carries the requested wire rate.
        let mut fast = pair.clone();
        fast.0.alt_setting = 4;
        fast.1.alt = 4;
        fast.1.frequency_limits = vec![(88_000, 96_000)];
        let mut slow = pair.clone();
        slow.0.alt_setting = 5;
        slow.0.max_packet_bytes = 384;
        slow.1.alt = 5;
        slow.1.packet_bytes = 384;
        slow.1.endpoint.2 = 384;
        slow.1.frequency_limits = vec![(44_100, 48_100)];
        let peers = adc3_unique_implicit(vec![slow.clone(), fast.clone()]).unwrap();
        assert_eq!(peers.len(), 2);
        assert_eq!(peers[0].0.alt_setting, 4);
        let clock_ranges = [SamplingFrequencySubrange { min: 44_100, max: 192_100, res: 1000 }];
        let pairs = adc3_expand_implicit_pairs(&sink, &out, &clock_ranges, peers);
        let clipped: Vec<_> = pairs[1].2.iter().map(|range| (range.min, range.max, range.res)).collect();
        assert_eq!(clipped, vec![(44_100, 47_100, 1_000)]);
        assert!(sampling_frequency_ranges_support_rate(&pairs[0].2, 89_100));
        assert!(!sampling_frequency_ranges_support_rate(&pairs[1].2, 48_100));
        assert!(!sampling_frequency_ranges_support_rate(&pairs[0].2, 44_100));
        let requested = PlaybackFormat { sample_rate: 44_100, channels: sink.channels,
            bit_depth: sink.bit_resolution, is_dop: false,
            dsd_transport: DsdTransportMode::None, dsd_bit_rate: 0 };
        let chosen = pairs.iter().find(|(candidate, _, ranges)|
            adc3_candidate_matches_ranges(candidate, ranges, requested)).unwrap();
        assert_eq!(chosen.0.implicit_feedback, Some(slow.0));
        // Exact reconstruction retains the lower-ranked selected source even
        // when a higher-ranked alternate is still feasible for another rate.
        let pinned = adc3_pair_for_source(pairs, Some(slow.0)).unwrap();
        assert_eq!(pinned.1.implicit.as_deref(), Some(&slow.1));
        let mut missing = slow.0; missing.alt_setting = 9;
        assert!(adc3_pair_for_source(vec![pinned], Some(missing)).is_none());
        let mut incompatible = slow.clone();
        incompatible.1.endpoint.1 += 1;
        assert!(adc3_unique_implicit(vec![fast.clone(), incompatible]).is_err());
        let mut conflicting = slow.clone(); conflicting.0.max_packet_bytes += 8;
        assert!(adc3_unique_implicit(vec![slow, conflicting]).is_err());
        for sync in [SyncType::Adaptive, SyncType::Synchronous, SyncType::NoSync] {
            let mut bad = implicit_pcm_candidate(); bad.sync_type = sync;
            assert!(!adc3_implicit_pair(&sink, &out, &bad, &input));
        }
        let mut bad = implicit_pcm_candidate(); bad.usage_type = UsageType::Data;
        assert!(!adc3_implicit_pair(&sink, &out, &bad, &input));
        let mut bad = implicit_pcm_candidate(); bad.service_interval_us = 125;
        assert!(!adc3_implicit_pair(&sink, &out, &bad, &input));
        let mut bad = implicit_pcm_candidate(); bad.interface_number = sink.interface_number;
        assert!(!adc3_implicit_pair(&sink, &out, &bad, &input));
        let mut bad = input.clone(); bad.clock.as_mut().unwrap().id = 10;
        assert!(!adc3_shared_clock(&out, &bad));
        let mut bad = input.clone(); bad.first = 8;
        assert!(!adc3_shared_clock(&out, &bad));
        let mut bad = input.clone(); bad.config = 2;
        assert!(!adc3_shared_clock(&out, &bad));
        let mut bad = input.clone();
        bad.choice.as_mut().unwrap().path[0].kind = ClockHopKind::Selector { selected_pin: 2, pin_count: 2 };
        assert!(!adc3_shared_clock(&out, &bad));
        assert!(adc3_route_domain_required(&out, &domain(10)));
        let mut unrelated = domain(12); unrelated.entities = vec![99];
        assert!(!adc3_route_domain_required(&out, &unrelated));
        let mut clock_domain = domain(13); clock_domain.entities = vec![8];
        assert!(adc3_route_domain_required(&out, &clock_domain));
        let mut limited_out = out.clone();
        limited_out.frequency_limits = vec![(44_100, 96_000)];
        let mut limited_in = input.clone();
        limited_in.frequency_limits = vec![(48_000, 48_000)];
        limited_out.implicit = Some(Box::new(limited_in.clone()));
        assert!(validate_adc3_static_rate(&limited_out, 48_000).is_ok());
        assert!(validate_adc3_static_rate(&limited_out, 44_100).is_err());
        assert!(validate_adc3_static_rate(&limited_out, 96_000).is_err());
        let restricted = adc3_route_limit_ranges(&limited_out,
            &[SamplingFrequencySubrange { min: 44_000, max: 96_000, res: 1_000 }]);
        assert_eq!(restricted.iter().map(|r| (r.min, r.max, r.res)).collect::<Vec<_>>(),
            vec![(48_000, 48_000, 0)]);
        let mut impossible = limited_out.clone();
        impossible.implicit.as_mut().unwrap().frequency_limits = vec![(192_000, 192_000)];
        assert!(adc3_route_limit_ranges(&impossible,
            &[SamplingFrequencySubrange { min: 44_000, max: 96_000, res: 1_000 }]).is_empty());
        let mut drift = limited_out.clone();
        drift.frequency_limits = vec![(44_100, 48_000)];
        assert_ne!(limited_out, drift);
        let mut drift = limited_out.clone();
        drift.implicit.as_mut().unwrap().frequency_limits = vec![];
        assert_ne!(limited_out, drift);
        adaptive_in.frequency_limits = vec![(96_000, 96_000)];
        assert!(validate_adaptive_duplex(&adaptive, &adaptive_out, 48_000, &adaptive_in, 48_000).is_err());
        adaptive_in.frequency_limits.clear();
        adaptive_out.frequency_limits = vec![(96_000, 96_000)];
        assert!(validate_adaptive_duplex(&adaptive, &adaptive_out, 48_000, &adaptive_in, 48_000).is_err());
        let mut implicit_route = out.clone(); implicit_route.implicit = Some(Box::new(input.clone()));
        let mut drift = implicit_route.clone(); drift.implicit.as_mut().unwrap().terminal = 7;
        assert_ne!(implicit_route, drift);
        let mut drift = implicit_route.clone(); drift.implicit.as_mut().unwrap().domains[0].recovery_d1 += 1;
        assert_ne!(implicit_route, drift);
        let mut drift = implicit_route.clone(); drift.implicit.as_mut().unwrap().endpoint.1 = 3;
        assert_ne!(implicit_route, drift);
        let map = capture_channel_map(&route).unwrap();
        let format = crate::capture::CaptureFormat::pcm(96_000, 16, 2, map.clone()).unwrap();
        let mut exact = crate::capture::CaptureRequest { sample_rate: 96_000,
            channels: 1, valid_bits: 16, channel_map: Some(map) };
        assert!(format.satisfies(&exact));
        exact.sample_rate = 48_000;
        assert!(!format.satisfies(&exact));
        exact.sample_rate = 96_000;
        exact.channel_map = Some(crate::capture::ChannelMap::uac2(1, 4).unwrap());
        assert!(!format.satisfies(&exact));
        let mut topology = route.clone();
        topology.cluster.as_mut().unwrap().channels = vec![
            wire::Channel::Information {
                purpose: 3,
                relationship: wire::ChannelRelationship::Spatial(
                    wire::SpatialPosition::HeadphoneRight,
                ),
                group: 7,
            },
            wire::Channel::Ambisonic {
                ordering: 1,
                acn: 4,
                normalization: 2,
                group: 8,
            },
        ];
        assert_eq!(
            capture_channel_map(&topology).unwrap().positions(),
            &[
                crate::capture::ChannelPosition::Adc3Information {
                    purpose: 3,
                    relationship: 0xbc,
                    group: 7
                },
                crate::capture::ChannelPosition::Adc3Ambisonic {
                    ordering: 1,
                    acn: 4,
                    normalization: 2,
                    group: 8
                },
            ]
        );
        let mut companion_drift = route.clone();
        companion_drift.companions.push((1, [6, 0x30, 0, 0, 196, 0]));
        assert_ne!(route, companion_drift);
        let mut extended_drift = route.clone();
        extended_drift.extended_companions.push((1, [8, 0x31, 0, 0, 0, 0, 1, 0]));
        assert_ne!(route, extended_drift);
        let mut geometry = route.clone();
        geometry.endpoint.2 += 4;
        assert_ne!(route, geometry);
        let mut geometry = route.clone();
        geometry.endpoint.3 = 2000;
        assert_ne!(route, geometry);
        let mut drift = route.clone();
        drift.config = 2;
        assert_ne!(route, drift);
        let mut drift = route.clone();
        drift.cluster.as_mut().unwrap().id = 2;
        assert_ne!(route, drift);
        let mut drift = route.clone();
        drift.choice.as_mut().unwrap().path[0].kind = ClockHopKind::Selector {
            selected_pin: 2,
            pin_count: 2,
        };
        assert_ne!(route, drift);
    }
}

#[cfg(test)]
mod alternate_frequency_tests {
    use super::*;
    struct Frequency { current: u32, writes: Vec<u32>, failed_readback: bool }
    impl Adc3FrequencyIo for Frequency {
        fn read_frequency(&mut self, _: u8) -> Result<u32, String> { Ok(self.current) }
        fn write_frequency(&mut self, _: u8, frequency: u32) -> Result<(), String> {
            self.writes.push(frequency);
            if !self.failed_readback { self.current = frequency; }
            Ok(())
        }
    }
    fn snapshot() -> FrequencySnapshot { FrequencySnapshot { source: 9, original: 48000, target: 96000 } }
    #[test]
    fn failed_or_retired_alternative_restores_frequency_exactly() {
        let mut io = Frequency { current: 96000, writes: vec![], failed_readback: false };
        restore_adc3_frequency(&mut io, snapshot()).unwrap();
        assert_eq!(io.current, 48000);
        assert_eq!(io.writes, vec![48000]);
        restore_adc3_frequency(&mut io, snapshot()).unwrap();
        assert_eq!(io.writes, vec![48000]);
    }
    #[test]
    fn external_frequency_drift_is_preserved_and_failed_readback_is_reported() {
        let mut io = Frequency { current: 44100, writes: vec![], failed_readback: false };
        assert!(restore_adc3_frequency(&mut io, snapshot()).is_err());
        assert!(io.writes.is_empty());
        io.current = 96000;
        io.failed_readback = true;
        assert!(restore_adc3_frequency(&mut io, snapshot()).is_err());
        assert_eq!(io.writes, vec![48000]);
    }
}

#[cfg(test)]
mod multi_interface_clock_tests {
    use super::*;
    use crate::clock_resolution::ClockPinPlan;
    use std::cell::RefCell;
    use std::rc::Rc;

    struct TransactionIo {
        pin: u8,
        frequency: u32,
        events: Rc<RefCell<Vec<String>>>,
    }
    impl ClockPlanIo for TransactionIo {
        fn read_pin(&mut self, _: u8) -> Result<u8, String> { Ok(self.pin) }
        fn write_pin(&mut self, _: u8, pin: u8) -> Result<(), String> {
            self.events.borrow_mut().push(format!("pin:{pin}"));
            self.pin = pin;
            Ok(())
        }
        fn validate_source(&mut self, _: u8, _: u32) -> Result<(), String> { Ok(()) }
    }
    impl Adc3FrequencyIo for TransactionIo {
        fn read_frequency(&mut self, _: u8) -> Result<u32, String> { Ok(self.frequency) }
        fn write_frequency(&mut self, _: u8, frequency: u32) -> Result<(), String> {
            self.events.borrow_mut().push(format!("frequency:{frequency}"));
            self.frequency = frequency;
            Ok(())
        }
    }
    fn plan() -> ClockPlan {
        ClockPlan { source_id: 9, source_controls: 3, path: vec![7, 9],
            selectors: vec![ClockPinPlan { selector: 7, original: 1, target: 2 }] }
    }
    fn frequency() -> FrequencySnapshot {
        FrequencySnapshot { source: 9, original: 48_000, target: 96_000 }
    }
    #[test]
    fn handback_confirms_every_peer_idle_before_frequency_and_selector_restoration() {
        let events = Rc::new(RefCell::new(Vec::new()));
        let mut io = TransactionIo { pin: 2, frequency: 96_000, events: events.clone() };
        restore_adc3_clock_transaction(&mut io, &plan(), frequency(), &[1, 2], |interface| {
            events.borrow_mut().push(format!("idle:{interface}"));
            verify_idle_reply(1, 0)
        }).unwrap();
        assert_eq!(*events.borrow(), vec!["idle:1", "idle:2", "frequency:48000", "pin:1"]);
    }
    #[test]
    fn active_peer_or_selector_drift_blocks_all_handback_writes() {
        let events = Rc::new(RefCell::new(Vec::new()));
        let mut io = TransactionIo { pin: 2, frequency: 96_000, events: events.clone() };
        assert!(restore_adc3_clock_transaction(&mut io, &plan(), frequency(), &[1, 2], |interface|
            verify_idle_reply(1, u8::from(interface == 2))).is_err());
        assert!(events.borrow().is_empty());
        io.pin = 1;
        assert!(restore_adc3_clock_transaction(&mut io, &plan(), frequency(), &[1, 2], |_|
            verify_idle_reply(1, 0)).is_err());
        assert!(events.borrow().is_empty());
        assert_eq!(io.frequency, 96_000);
        assert!(verify_idle_reply(0, 0).is_err());
        assert!(verify_idle_reply(-1, 0).is_err());
        assert!(verify_idle_reply(2, 0).is_err());
    }
}

#[cfg(test)]
mod alternate_range_tests {
    use super::*;
    fn range(min: u32, max: u32, res: u32) -> SamplingFrequencySubrange {
        SamplingFrequencySubrange { min, max, res }
    }
    fn values(ranges: &[SamplingFrequencySubrange]) -> Vec<(u32, u32, u32)> {
        ranges.iter().map(|r| (r.min, r.max, r.res)).collect()
    }
    #[test]
    fn alternate_bounds_keep_the_projected_progression_anchor() {
        let clipped = adc3_limit_ranges(&[range(44_100, 96_100, 1_000)], &[(44_101, 48_999)]);
        assert_eq!(values(&clipped), vec![(45_100, 48_100, 1_000)]);
        assert!(!sampling_frequency_ranges_support_rate(&clipped, 48_000));
        assert!(sampling_frequency_ranges_support_rate(&clipped, 48_100));
    }
    #[test]
    fn disjoint_limits_singletons_and_u32_edges_remain_exact() {
        let clock = [range(44_100, 96_100, 1_000)];
        assert_eq!(values(&adc3_limit_ranges(&clock, &[(44_100, 44_100), (47_100, 48_000), (96_000, 96_100)])),
            vec![(44_100, 44_100, 0), (47_100, 47_100, 0), (96_100, 96_100, 0)]);
        assert!(adc3_limit_ranges(&clock, &[(48_000, 48_000)]).is_empty());
        assert_eq!(values(&adc3_limit_ranges(&[range(48_000, 48_000, 0)], &[(47_000, 49_000)])),
            vec![(48_000, 48_000, 0)]);
        assert_eq!(values(&adc3_limit_ranges(&clock, &[])), values(&clock));
        assert!(adc3_limit_ranges(&[range(1, u32::MAX, u32::MAX - 1)],
            &[(u32::MAX - 1, u32::MAX - 1)]).is_empty());
        assert_eq!(values(&adc3_limit_ranges(&[range(1, u32::MAX, u32::MAX - 1)],
            &[(u32::MAX, u32::MAX)])), vec![(u32::MAX, u32::MAX, 0)]);
    }
}
