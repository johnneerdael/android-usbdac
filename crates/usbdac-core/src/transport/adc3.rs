//! Active-configuration ADC3 PCM playback and capture. No configuration changes or clock guesses.
use super::*;
use crate::clock_resolution::{ClockHop, ClockHopKind, ClockResolution};
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
    feedback: Option<(u8, u8, usize)>,
    pcm: (u16, u8, u8),
    cluster: Option<wire::Cluster>,
    header: Option<wire::AsHeader>,
    pub(super) clock: Option<wire::ClockSource>,
    pub(super) choice: Option<ClockChoice>,
    domains: Vec<wire::PowerDomain>,
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
        Speed::High => wire::BusSpeed::High,
        _ => return Err("BADD playback requires known full/high USB speed".into()),
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
            if alt.setting_number() == 1 {
                let feedback =
                    find_feedback_endpoint(alt, ep.synch_address(), ep.sync_type(), speed);
                if !capture && explicit_feedback_ambiguous(alt, ep.synch_address(), ep.sync_type())
                {
                    return Err("BADD explicit feedback is ambiguous".into());
                }
                if !capture
                    && asynchronous
                    && !feedback.is_some_and(|f| {
                        f.transfer_type == TransferType::Isochronous
                            && f.max_packet_bytes == if speed == Speed::Full { 3 } else { 4 }
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
                wire::infer_badd_format(profile, speed_wire, 1, &endpoint, feedback.is_some())
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
fn cluster(handle: &DeviceHandle<Context>, control: u8, id: u16) -> Result<wire::Cluster, String> {
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
                read_exact(handle, control, id, 1, 1, &mut pin)?;
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
                return Err("ADC3 clock multiplier requires an implemented ratio contract".into())
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
    adc3_alternate(&config, handle, &alt, candidate.usb_speed, false).map(|(_, r)| Some(r))
}
fn adc3_alternate(
    config: &ConfigDescriptor,
    handle: &DeviceHandle<Context>,
    alt: &rusb::InterfaceDescriptor,
    speed: Speed,
    capture: bool,
) -> Result<(IsoStreamCandidate, Adc3Route), String> {
    let (iad, profile, extra) = owning_function(config, handle, alt.interface_number())?;
    let control = iad.b_first_interface;
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
                && e.usage_type() == UsageType::Data
        })
        .collect();
    let [ep] = endpoints.as_slice() else {
        return Err("ADC3 alternate must have one data endpoint in the requested direction".into());
    };
    let packet = ep.max_packet_size();
    let base = packet & 0x7ff;
    if ep.address() & 0x0f == 0
        || packet & 0xe000 != 0
        || base == 0
        || (speed == Speed::Full && (base > 1023 || packet & 0x1800 != 0))
        || (speed == Speed::High && (base > 1024 || (packet >> 11) & 3 == 3))
    {
        return Err("ADC3 invalid audio endpoint packet geometry".into());
    }
    if !(1..=16).contains(&ep.interval()) || !matches!(speed, Speed::Full | Speed::High) {
        return Err("ADC3 currently supports known FS/HS service geometry".into());
    }
    if !capture && explicit_feedback_ambiguous(alt, ep.synch_address(), ep.sync_type()) {
        return Err("ADC3 explicit feedback ambiguous".into());
    }
    let feedback = if capture {
        None
    } else {
        find_feedback_endpoint(alt, ep.synch_address(), ep.sync_type(), speed)
    };
    if !capture
        && ep.sync_type() == SyncType::Asynchronous
        && (feedback.is_none() || settings().disable_feedback())
    {
        return Err("ADC3 asynchronous playback requires enabled explicit feedback".into());
    }
    if !matches!(
        ep.sync_type(),
        SyncType::Asynchronous | SyncType::Synchronous
    ) {
        return Err("ADC3 synchronization unsupported".into());
    }
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
        feedback: feedback.map(|f| (f.address, f.interval, f.max_packet_bytes)),
        pcm: (0, 0, 0),
        cluster: None,
        header: None,
        clock: None,
        choice: None,
        domains: domains(profile, &extra, capture)?,
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
            let (source, choice) = clock_route(handle, control, h.terminal_link, &extra, capture)?;
            let ranges = get_sampling_frequency_ranges(handle, control, source.id)?;
            if ranges.is_empty() || ranges.iter().any(|r| r.min == 0 || r.min > r.max) {
                return Err("ADC3 invalid clock frequency ranges".into());
            }
            let result = (
                c.channels.len() as u16,
                h.subslot_size,
                h.bit_resolution,
                ranges,
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
        interface_number: alt.interface_number(),
        alt_setting: alt.setting_number(),
        endpoint_address: ep.address(),
        endpoint_interval: ep.interval(),
        service_interval_us: service_interval_micros(speed, ep.interval()),
        usb_speed: speed,
        max_packet_bytes: effective_iso_packet_bytes(ep.max_packet_size(), speed),
        sync_type: ep.sync_type(),
        usage_type: ep.usage_type(),
        format_tag: FORMAT_TAG_PCM,
        uac2_formats: None,
        native_dsd: None,
        channels,
        subslot_size: subslot,
        bit_resolution: bits,
        sample_rates: representative_sample_rates_from_ranges(&rates),
        refresh: ep.refresh(),
        synch_address: ep.synch_address(),
        feedback_endpoint: feedback,
        implicit_feedback: None,
        terminal_link: Some(route.terminal),
        is_uac1: false,
    };
    Ok((candidate, route))
}
pub(super) fn adc3_candidates(
    device: &Device<Context>,
    handle: &DeviceHandle<Context>,
    speed: Speed,
    claimed: &[u8],
) -> Vec<(IsoStreamCandidate, Adc3Route)> {
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
        match adc3_alternate(&config, handle, &alt, speed, false) {
            Ok(pair) => {
                if claimed.contains(&pair.1.control) {
                    result.push(pair);
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
) -> Result<Adc3Route, String> {
    let alt = config
        .interfaces()
        .flat_map(|i| i.descriptors())
        .find(|d| d.interface_number() == interface && d.setting_number() == alternate)
        .ok_or("ADC3 capture interface disappeared")?;
    adc3_alternate(
        config,
        &claimed.handle,
        &alt,
        resolve_device_speed(&claimed.handle.device()),
        true,
    )
    .map(|(_, route)| route)
}

/// Capture discovery runs under the same exclusive USB owner as playback.
/// Wake before HC/clock reads; keep successful candidates' domains leased until
/// the selected stream retires. Failed probes restore their writes immediately.
pub(super) fn adc3_capture_candidates(
    claimed: &ClaimedHandle,
    config: &ConfigDescriptor,
    speed: Speed,
    rate: u32,
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
                    .any(|e| e.direction() == Direction::In && e.usage_type() == UsageType::Data)
        })
    {
        let checkpoint = claimed.adc3_power.pending.lock().len();
        let attempt: Result<Option<(IsoStreamCandidate, Adc3Route)>, String> = (|| {
            prepare_adc3_alternate_power(claimed, config, &alt, true)?;
            let (candidate, route) = adc3_alternate(config, &claimed.handle, &alt, speed, true)?;
            let supports = if let Some(clock) = &route.clock {
                sampling_frequency_ranges_support_rate(
                    &get_sampling_frequency_ranges(&claimed.handle, route.control, clock.id)?,
                    rate,
                )
            } else {
                rate == 48_000
            };
            if supports
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
    if matches!(route.profile, wire::FunctionProfile::Badd(_)) {
        return if rate == 48000 {
            Ok(())
        } else {
            Err("BADD rate must be 48000 Hz".into())
        };
    }
    if let Some(choice) = route.choice.as_ref() {
        for hop in &choice.path {
            if let ClockHopKind::Selector { selected_pin, .. } = hop.kind {
                let mut pin = [0];
                read_exact(handle, route.control, hop.id, 1, 1, &mut pin)?;
                if pin[0] != selected_pin {
                    return Err("ADC3 clock selector changed; renegotiate".into());
                }
            }
        }
    }
    let source = route.clock.as_ref().ok_or("ADC3 clock source missing")?;
    let ranges = get_sampling_frequency_ranges(handle, route.control, source.id)?;
    if !sampling_frequency_ranges_support_rate(&ranges, rate) {
        return Err("ADC3 rate outside source ranges".into());
    }
    let mut current = [0; 4];
    read_exact(handle, route.control, source.id, 1, 1, &mut current)?;
    if u32::from_le_bytes(current) != rate {
        if source.controls.access(1) != wire::ControlAccess::ReadWrite {
            return Err("ADC3 source frequency is read-only and differs".into());
        }
        write_exact(handle, route.control, source.id, 1, &rate.to_le_bytes())?;
        thread::sleep(Duration::from_millis(settings().settle_delay_ms()));
    }
    read_exact(handle, route.control, source.id, 1, 1, &mut current)?;
    if u32::from_le_bytes(current) != rate {
        return Err("ADC3 source frequency verification failed".into());
    }
    match source.controls.access(2) {
        wire::ControlAccess::ReadOnly | wire::ControlAccess::ReadWrite => {
            let mut valid = [0];
            read_exact(handle, route.control, source.id, 2, 1, &mut valid)?;
            if valid != [1] {
                return Err("ADC3 clock source invalid".into());
            }
        }
        wire::ControlAccess::Reserved => return Err("ADC3 clock validity access reserved".into()),
        _ => {}
    }
    Ok(())
}

/// One lease per exclusive USB owner. Repeated domains share its lifetime and
/// are restored in reverse acquisition order only after transfer retirement.
pub(super) struct Adc3PowerLease {
    fd: RawFd,
    pending: Mutex<Vec<(u8, u8, u8)>>,
}
impl Adc3PowerLease {
    pub(super) fn new(fd: RawFd) -> Self {
        Self {
            fd,
            pending: Mutex::new(vec![]),
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
    fn acquire(&self, ac: u8, d: &wire::PowerDomain) -> Result<(), String> {
        acquire_domain(&mut PowerUsb(self), &mut self.pending.lock(), ac, d)
    }
    pub(super) fn restore(&self) -> Result<(), String> {
        let pending = std::mem::take(&mut *self.pending.lock());
        restore_domains(&mut PowerUsb(self), pending)
    }
    pub(super) fn disarm(&self) {
        self.pending.lock().clear();
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
pub(super) fn select_owned_stream_candidate(
    claimed: &ClaimedHandle,
    requested: PlaybackFormat,
    speed: Speed,
) -> Result<IsoStreamCandidate, SelectError> {
    let device = claimed.handle.device();
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
            .and_then(|_| adc3_alternate(&config, &claimed.handle, &alt, speed, false));
        match attempt {
            Ok((candidate, route))
                if adc3_candidate_matches(&claimed.handle, &candidate, &route, requested) =>
            {
                return Ok(candidate)
            }
            Ok(_) => {}
            Err(error) => refusal = Some(error),
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
    if requested.dsd_transport != DsdTransportMode::None
        || requested.channels != candidate.channels
        || requested.bit_depth != candidate.bit_resolution
    {
        return false;
    }
    let supported = if let Some(clock) = route.clock.as_ref() {
        get_sampling_frequency_ranges(handle, route.control, clock.id)
            .map(|r| sampling_frequency_ranges_support_rate(&r, requested.sample_rate))
            .unwrap_or(false)
    } else {
        requested.sample_rate == 48000
    };
    supported
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
    #[test]
    fn negotiated_identity_rejects_configuration_cluster_and_pin_drift() {
        let route = Adc3Route {
            config: 1,
            control: 4,
            first: 4,
            count: 2,
            profile: wire::FunctionProfile::Full,
            interface: 5,
            alt: 1,
            terminal: 1,
            endpoint: (1, 4, 196, 1000),
            feedback: Some((0x81, 4, 4)),
            pcm: (2, 2, 16),
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
        };
        assert_eq!(
            capture_channel_map(&route).unwrap().positions(),
            &[crate::capture::ChannelPosition::Adc3Information {
                purpose: 1,
                relationship: 1,
                group: 0
            }]
        );
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
