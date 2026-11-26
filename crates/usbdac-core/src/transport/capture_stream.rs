//! Standalone PCM IN ownership. Duplex and implicit-feedback fan-out must join
//! this pool rather than opening a second owner of the same endpoint.
use super::*;
use crate::capture::{CaptureBuffer, CaptureFormat, CapturePacket, CaptureRequest, ChannelMap};
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
    sampling_control: bool,
    fixed_rate: bool,
    pub(super) adc3_route: Option<adc3::Adc3Route>,
}

#[derive(Clone)]
struct RawInterface<'a> {
    number: u8,
    alternate: u8,
    class: u8,
    subclass: u8,
    protocol: u8,
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
fn uac1_cluster(
    descriptors: &[&[u8]],
    id: u8,
    channels: u16,
    visited: &mut Vec<u8>,
) -> Option<ChannelMap> {
    if id == 0 || visited.contains(&id) || visited.len() >= 32 {
        return None;
    }
    visited.push(id);
    let matches: Vec<_> = descriptors
        .iter()
        .copied()
        .filter(|d| d.len() >= 4 && d[1] == USB_DT_CS_INTERFACE && d[2] != 1 && d[3] == id)
        .collect();
    let result = if matches.len() == 1 {
        let d = matches[0];
        let cluster = |offset: usize| -> Option<ChannelMap> {
            if d.len() < offset + 4 || u16::from(d[offset]) != channels {
                return None;
            }
            ChannelMap::uac1(channels, u16::from_le_bytes([d[offset + 1], d[offset + 2]])).ok()
        };
        match d[2] {
            2 if d.len() >= 12 => cluster(7),
            3 if d.len() >= 9 => uac1_cluster(descriptors, d[7], channels, visited),
            6 if d.len() >= 7 => uac1_cluster(descriptors, d[4], channels, visited),
            4 if d.len() >= 5 => cluster(5 + usize::from(d[4])),
            7 | 8 if d.len() >= 7 => cluster(7 + usize::from(d[6])),
            5 if d.len() >= 6 && d.len() == 6 + usize::from(d[4]) => {
                let mut map = None;
                for source in &d[5..5 + usize::from(d[4])] {
                    let candidate = uac1_cluster(descriptors, *source, channels, visited);
                    if candidate.is_none()
                        || map
                            .as_ref()
                            .is_some_and(|map| Some(map) != candidate.as_ref())
                    {
                        map = None;
                        break;
                    }
                    map = candidate;
                }
                map
            }
            _ => None,
        }
    } else {
        None
    };
    visited.pop();
    result
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
        let map = uac1_cluster(&control.descriptors, general[3], channels, &mut Vec::new())
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

pub(super) fn capture_candidates(
    raw: &[u8],
    configuration: u8,
    speed: Speed,
    rate: u32,
) -> Result<Vec<CaptureEndpoint>, String> {
    if !matches!(speed, Speed::Full | Speed::High) {
        return Err("standalone capture currently requires Full or High Speed USB".into());
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
            if ep.len() < 7
                || ep[1] != 5
                || ep[2] & 0x80 == 0
                || ep[2] & 0x0f == 0
                || ep[2] & 0x70 != 0
                || ep[3] & 3 != 1
                || (ep[3] >> 4) & 3 == 1
                || !matches!((ep[3] >> 4) & 3, 0 | 2)
                || !(1..=16).contains(&ep[6])
            {
                continue;
            }
            let packet = u16::from_le_bytes([ep[4], ep[5]]);
            let base = packet & 0x7ff;
            if packet & 0xe000 != 0
                || base == 0
                || (speed == Speed::Full && (base > 1023 || packet & 0x1800 != 0))
                || (speed == Speed::High && (base > 1024 || (packet >> 11) & 3 == 3))
            {
                continue;
            }
            let service_us = service_interval_micros(speed, ep[6]);
            let packet_bytes = effective_iso_packet_bytes(packet, speed);
            if packet_bytes < required_max_packet_bytes(rate, service_us, format.bytes_per_frame())
            {
                continue;
            }
            let sampling_control = interface.descriptors[index + 1..]
                .iter()
                .take_while(|d| d[1] != 5)
                .find(|d| d.len() >= 7 && d[1] == 0x25 && d[2] == 1)
                .is_some_and(|d| d[3] & 1 != 0);
            candidates.push(CaptureEndpoint {
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
                sampling_control,
                fixed_rate,
                adc3_route: None,
            });
        }
    }
    Ok(candidates)
}

/// Resolve active-configuration candidates while the caller owns USB claims.
pub(super) fn owned_capture_candidates(
    claimed: &ClaimedHandle,
    config: &ConfigDescriptor,
    rate: u32,
    planned_clock: bool,
) -> Result<Vec<CaptureEndpoint>, String> {
    let raw = read_active_configuration_bytes(&claimed.handle, config)
        .ok_or("cannot read active capture configuration")?;
    let speed = resolve_device_speed(&claimed.handle.device());
    // Legacy UAC1/2 admission remains FS/HS; ADC3 resolves its own SS companion.
    let mut candidates = if matches!(speed, Speed::Super | Speed::SuperPlus) {
        Vec::new()
    } else {
        capture_candidates(&raw, config.number(), speed, rate)?
    };
    for (candidate, route) in adc3::adc3_capture_candidates(claimed, config, speed, rate, planned_clock)? {
        let format = CaptureFormat::pcm(
            rate,
            candidate.bit_resolution,
            candidate.subslot_size,
            adc3::capture_channel_map(&route)?,
        )
        .map_err(|e| format!("ADC3 capture format: {e:?}"))?;
        candidates.push(CaptureEndpoint {
            configuration: config.number(),
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
            sampling_control: false,
            fixed_rate: route.clock.is_none(),
            adc3_route: Some(route),
        });
    }
    Ok(candidates)
}

/// Explicit capture registration has its own narrow claim policy. Playback's
/// optional-feedback claims remain unchanged. No OUT interface is required.
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
    let raw = read_active_configuration_bytes(&claimed.handle, &config)
        .ok_or("cannot read active capture configuration")?;
    let interfaces = raw_interfaces(&raw)?;
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
        if interface.descriptors.iter().any(|ep| {
            ep.len() >= 7
                && ep[1] == 5
                && ep[2] & 0x80 != 0
                && ep[3] & 3 == 1
                && matches!((ep[3] >> 4) & 3, 0 | 2)
        }) {
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
    let identity = claimed.handle.device().device_descriptor().map_err(|e| e.to_string())?;
    if identity.vendor_id() != device.vendor_id || identity.product_id() != device.product_id {
        return Err("capture host and actual USB identities differ".into());
    }
    if let Some(interface) = super::routing_hw::scarlett_claim_interface(&config, identity.vendor_id(), identity.product_id()) {
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
    let raw = read_active_configuration_bytes(&claimed.handle, &config)
        .ok_or("cannot read active capture configuration")?;
    let interfaces = raw_interfaces(&raw)?;
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
        if interface.descriptors.iter().any(|ep| {
            ep.len() >= 7
                && ep[1] == 5
                && ep[2] & 0x80 != 0
                && ep[2] & 0x0f != 0
                && ep[3] & 3 == 1
                && matches!((ep[3] >> 4) & 3, 0 | 2)
        }) {
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
                let idle = active.into_iter().try_for_each(|interface| {
                    claimed.set_alternate_setting(interface, 0)
                });
                let restored = idle.and_then(|()| clock::restore_capture_clock_route(&claimed, &route));
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

pub(super) fn configure_capture_endpoint(
    claimed: &ClaimedHandle,
    candidate: &CaptureEndpoint,
) -> Result<(), Error> {
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
            set_uac1_sampling_frequency(
                &claimed.handle,
                candidate.interface,
                candidate.address,
                rate,
            )
            .map_err(Error::Control)?;
            let reported = get_uac1_sampling_frequency(
                &claimed.handle,
                candidate.interface,
                candidate.address,
            )
            .ok();
            if reported != Some(rate) {
                return Err(Error::ClockUnverified {
                    requested_hz: rate,
                    reported_hz: reported,
                });
            }
        } else if !candidate.fixed_rate {
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
        adc3::configure_adc3(&claimed.handle, route, rate).map_err(Error::Control)?;
        claimed
            .set_alternate_setting(candidate.interface, candidate.alternate)
            .map_err(Error::Claim)?;
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
        let reported =
            get_sampling_frequency(&claimed.handle, candidate.control, choice.source_id).ok();
        if reported != Some(rate) {
            if source[5] & 3 != 3 {
                return Err(Error::ClockUnverified {
                    requested_hz: rate,
                    reported_hz: reported,
                });
            }
            let ranges =
                get_sampling_frequency_ranges(&claimed.handle, candidate.control, choice.source_id)
                    .map_err(Error::Control)?;
            if !sampling_frequency_ranges_support_rate(&ranges, rate) {
                return Err(Error::Unsupported {
                    reason: "capture source does not support the exact requested rate".into(),
                    offered: vec![],
                });
            }
            set_sampling_frequency(&claimed.handle, candidate.control, choice.source_id, rate)
                .map_err(Error::Control)?;
        }
        let reported =
            get_sampling_frequency(&claimed.handle, candidate.control, choice.source_id).ok();
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
        claimed
            .set_alternate_setting(candidate.interface, candidate.alternate)
            .map_err(Error::Claim)?;
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
    }
    Ok(())
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
        self.reordered_completions.fetch_add(stats.reordered_completions, Ordering::Relaxed);
        self.captured_frames.fetch_add(stats.captured_frames, Ordering::Relaxed);
        self.delivered_frames.fetch_add(stats.delivered_frames, Ordering::Relaxed);
        self.overrun_frames.fetch_add(stats.overrun_frames, Ordering::Relaxed);
        self.contention_frames.fetch_add(stats.contention_frames, Ordering::Relaxed);
        self.empty_packets.fetch_add(stats.empty_packets, Ordering::Relaxed);
        self.short_packets.fetch_add(stats.short_packets, Ordering::Relaxed);
        self.failed_packets.fetch_add(stats.failed_packets, Ordering::Relaxed);
        self.malformed_packets.fetch_add(stats.malformed_packets, Ordering::Relaxed);
        self.malformed_completions.fetch_add(stats.malformed_completions, Ordering::Relaxed);
        self.failed_transfers.fetch_add(stats.failed_transfers, Ordering::Relaxed);
        self.missing_frames.fetch_add(stats.missing_frames, Ordering::Relaxed);
    }
    pub(super) fn finish(&self, failed: bool) {
        self.terminal.fetch_max(if failed { 2 } else { 1 }, Ordering::Release);
    }
    pub(super) fn is_finished(&self) -> bool { self.terminal.load(Ordering::Acquire) != 0 }
    pub(super) fn merge_into(&self, buffer: &mut CaptureBuffer) {
        let terminal = self.terminal.load(Ordering::Acquire);
        buffer.merge_stats(crate::capture::CaptureStats {
            reordered_completions: self.reordered_completions.swap(0, Ordering::Relaxed),
            captured_frames: self.captured_frames.swap(0, Ordering::Relaxed),
            delivered_frames: self.delivered_frames.swap(0, Ordering::Relaxed),
            overrun_frames: self.overrun_frames.swap(0, Ordering::Relaxed),
            contention_frames: self.contention_frames.swap(0, Ordering::Relaxed),
            empty_packets: self.empty_packets.swap(0, Ordering::Relaxed),
            short_packets: self.short_packets.swap(0, Ordering::Relaxed),
            failed_packets: self.failed_packets.swap(0, Ordering::Relaxed),
            malformed_packets: self.malformed_packets.swap(0, Ordering::Relaxed),
            malformed_completions: self.malformed_completions.swap(0, Ordering::Relaxed),
            failed_transfers: self.failed_transfers.swap(0, Ordering::Relaxed),
            missing_frames: self.missing_frames.swap(0, Ordering::Relaxed),
        });
        match terminal {
            2 => buffer.fail(),
            1 if buffer.state() == crate::capture::CaptureState::Active => buffer.stop(),
            _ => {}
        }
    }
}

pub(crate) struct CaptureHandle {
    pub(crate) endpoint: CaptureEndpoint,
    pub(crate) buffer: Arc<Mutex<CaptureBuffer>>,
    pub(crate) error: Arc<Mutex<Option<Error>>>,
    pub(crate) stop: Arc<AtomicBool>,
    pub(crate) worker: Option<JoinHandle<()>>,
    pub(crate) paired: bool,
    pub(crate) retired: Option<Arc<AtomicBool>>,
    pub(crate) deferred: Option<Arc<DeferredCapture>>,
}
impl CaptureHandle {
    pub(crate) fn lock_buffer(&self) -> parking_lot::MutexGuard<'_, CaptureBuffer> {
        let mut buffer = self.buffer.lock();
        if let Some(deferred) = &self.deferred { deferred.merge_into(&mut buffer); }
        buffer
    }
    pub(crate) fn stop(&mut self) -> Result<(), Error> {
        self.stop.store(true, Ordering::Release);
        let mut buffer = self.lock_buffer();
        if buffer.state() == crate::capture::CaptureState::Active {
            buffer.stop();
        }
        drop(buffer);
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
            let retired = self.retired.as_ref().expect("paired capture retirement signal");
            let deadline = Instant::now() + Duration::from_secs(3);
            while !retired.load(Ordering::Acquire) && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(10));
            }
            if !retired.load(Ordering::Acquire) {
                return Err(Error::Transfer("USB duplex is still retiring; retain host claims and FD".into()));
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
    let mut candidates = owned_capture_candidates(owner.claimed(), &config, request.sample_rate, false)
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
    let endpoint = candidates.into_iter().next().ok_or_else(|| Error::Unsupported {
        reason: "no claimed standard PCM IN alternate matches the exact capture rate, width and channel map".into(), offered: vec![] })?;
    let buffer = Arc::new(Mutex::new(
        CaptureBuffer::new(endpoint.format.clone(), capacity_frames)
            .map_err(|e| Error::InvalidArgument(format!("capture buffer: {e:?}")))?,
    ));
    let geometry = IsoQueueGeometry::for_stream(
        endpoint.service_us,
        settings().config.bus_queue_ms,
        settings().config.transfers_in_flight,
    );
    capture_queue_bytes(endpoint.packet_bytes, geometry)?;
    if endpoint.protocol == 0x20 {
        let mut route = None;
        let result = clock::select_capture_clock_route(
            owner.claimed(), endpoint.interface, endpoint.terminal, endpoint.control,
            endpoint.format.sample_rate(), &mut route,
        );
        owner.clock_route = route;
        result.map_err(Error::Control)?;
    }
    configure_capture_endpoint(owner.claimed(), &endpoint)?;
    let error = Arc::new(Mutex::new(None));
    let stop = Arc::new(AtomicBool::new(false));
    let worker_buffer = buffer.clone();
    let worker_error = error.clone();
    let worker_stop = stop.clone();
    let worker_endpoint = endpoint.clone();
    let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
    let worker = thread::Builder::new()
        .name("usbdac-capture".into())
        .spawn(move || {
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
        buffer,
        error,
        stop,
        worker: Some(worker),
        paired: false,
        retired: None,
        deferred: None,
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
    slot: &'a IsoTransferSlot, status: i32, scratch: &mut [CapturePacket],
) -> Result<(&'a [u8], bool), Error> {
    if status != LIBUSB_TRANSFER_COMPLETED {
        return Err(if status == LIBUSB_TRANSFER_NO_DEVICE { Error::DeviceLost }
            else { Error::Transfer(format!("capture transfer failed: {}", iso_transfer_status_label(status))) });
    }
    let transfer = unsafe { slot.transfer.as_ref() };
    let data = unsafe { &*slot.user_data };
    if transfer.num_iso_packets != scratch.len() as i32
        || transfer.length < 0 || transfer.length as usize != slot.submitted_bytes
        || slot.submitted_bytes != data.buffer.len() || transfer.endpoint != slot.submitted_endpoint
        || transfer.buffer != data.buffer.as_ptr().cast_mut() {
        return Err(Error::Transfer("capture transfer geometry changed".into()));
    }
    let descriptors = unsafe { std::slice::from_raw_parts(transfer.iso_packet_desc.as_ptr(), scratch.len()) };
    for (destination, source) in scratch.iter_mut().zip(descriptors) {
        *destination = CapturePacket { requested_bytes: source.length as usize,
            actual_bytes: source.actual_length as usize, succeeded: source.status == LIBUSB_TRANSFER_COMPLETED };
    }
    Ok((&data.buffer, descriptors.iter().any(|p| p.status == LIBUSB_TRANSFER_NO_DEVICE)))
}

pub(super) fn ingest_slot(slot: &IsoTransferSlot, status: i32, buffer: &mut CaptureBuffer,
    scratch: &mut [CapturePacket]) -> Result<(), Error> {
    let (data, detached) = match inspect_slot(slot, status, scratch) {
        Ok(completion) => completion,
        Err(error) => {
            if status != LIBUSB_TRANSFER_COMPLETED {
                let _ = buffer.ingest_completion(&[], &[], &[], false, 0);
            } else { buffer.note_malformed_completion(); }
            return Err(error);
        }
    };
    let result = buffer.ingest_completion(data, &slot.packet_sizes, scratch, true, 0)
        .map_err(|e| Error::Transfer(format!("capture completion: {e:?}")));
    if detached { Err(Error::DeviceLost) } else { result }
}

fn oldest_ready_capture(order: &VecDeque<usize>, slots: &[IsoTransferSlot]) -> Option<usize> {
    order
        .front()
        .copied()
        .filter(|index| slots[*index].completion_ready())
}

/// Prove libusb integer widths and total backing storage before class writes
/// or allocating a capture queue, including SSP payloads above u16::MAX.
pub(super) fn capture_queue_bytes(packet_bytes: usize, geometry: IsoQueueGeometry) -> Result<usize, Error> {
    let invalid = || Error::Unsupported {
        reason: "capture packet/queue geometry exceeds libusb or host allocation bounds".into(),
        offered: vec![],
    };
    if packet_bytes == 0 || packet_bytes > u32::MAX as usize
        || geometry.packets_per_transfer == 0 || geometry.packets_per_transfer > i32::MAX as usize
        || geometry.transfer_slots == 0 {
        return Err(invalid());
    }
    let per_transfer = packet_bytes.checked_mul(geometry.packets_per_transfer).ok_or_else(invalid)?;
    let total = per_transfer.checked_mul(geometry.transfer_slots).ok_or_else(invalid)?;
    let packets = geometry.packets_per_transfer.checked_mul(geometry.transfer_slots).ok_or_else(invalid)?;
    if packets >= crate::iso_packet_scheduler::PACKET_FLIGHT_LIMIT
        || geometry.packets_per_transfer > crate::iso_packet_scheduler::MAX_PACKETS_PER_TRANSFER
        || per_transfer > i32::MAX as usize || total > isize::MAX as usize {
        return Err(invalid());
    }
    Ok(per_transfer)
}

fn run_capture(
    owner: &CaptureUsbOwner,
    endpoint: &CaptureEndpoint,
    geometry: IsoQueueGeometry,
    buffer: &Mutex<CaptureBuffer>,
    stop: &AtomicBool,
    ready: &std::sync::mpsc::SyncSender<Result<(), Error>>,
) -> Result<(), Error> {
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
            IsoTransferSlot::new(
                index,
                geometry.packets_per_transfer,
                transfer_bytes,
            )
            .map_err(Error::Transfer)?,
        );
    }
    for (index, slot) in slots.iter_mut().enumerate() {
        if usb_stop_requested(stop) {
            return Ok(());
        }
        slot.submit_capture(
            &owner.claimed().handle,
            endpoint.address,
            endpoint.packet_bytes,
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
    let _ = ready.send(Ok(()));
    while !usb_stop_requested(stop) {
        owner
            .claimed()
            .context
            .handle_events(Some(Duration::from_millis(10)))
            .map_err(|e| {
                if e == rusb::Error::NoDevice {
                    Error::DeviceLost
                } else {
                    Error::Transfer(format!("capture event pump: {e}"))
                }
            })?;
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
        while let Some(index) = oldest_ready_capture(&order, &slots) {
            if usb_stop_requested(stop) {
                break;
            }
            let Some((status, _, _)) = slots[index].take_completion() else {
                break;
            };
            order.pop_front();
            reordered_seen[index] = false;
            let mut queue = buffer.lock();
            if usb_stop_requested(stop) {
                slots[index].mark_completed();
                break;
            }
            let result = ingest_slot(&slots[index], status, &mut queue, &mut scratch);
            drop(queue);
            slots[index].mark_completed();
            result?;
            if !usb_stop_requested(stop) {
                slots[index]
                    .submit_capture(
                        &owner.claimed().handle,
                        endpoint.address,
                        endpoint.packet_bytes,
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
    }
    // Cancel the complete pool first; each slot Drop pumps the existing
    // context until libusb relinquishes its buffer, with no unsafe deadline.
    for slot in &mut slots {
        slot.cancel();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
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

    fn publish(slot: &IsoTransferSlot) {
        unsafe {
            let user_data = &*slot.user_data;
            *user_data.completion.status.lock().unwrap() = Some(LIBUSB_TRANSFER_COMPLETED);
        }
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
    fn duplex_reader_contention_drops_only_valid_frames_without_blocking_completion_or_finish() {
        for shared_implicit in [false, true] {
            let endpoint = capture_candidates(&configuration(0), 1, Speed::Full, 48_000)
                .unwrap().remove(0);
            let buffer = CaptureBuffer::new(endpoint.format.clone(), 1).unwrap();
            let attachment = duplex::Attachment::new(endpoint, shared_implicit, buffer);
            let mut reader = attachment.reader(Arc::new(AtomicBool::new(false)));
            let retirement = attachment.retirement_guard();
            let mut slot = completed_slot(0, &[1; 12]); // stereo 24-bit: two frames
            let mut scratch = [CapturePacket { requested_bytes: 0, actual_bytes: 0, succeeded: false }];
            attachment.ingest(&slot, LIBUSB_TRANSFER_COMPLETED, &mut scratch).unwrap();
            assert_eq!(reader.lock_buffer().read(&mut [0; 6]), Ok(1));

            // Deliberately retain the reader lock on this thread. Any blocking
            // worker append or finish would deadlock here, before OUT resumes.
            let held = reader.buffer.lock();
            attachment.ingest(&slot, LIBUSB_TRANSFER_COMPLETED, &mut scratch).unwrap();
            unsafe { (*slot.transfer.as_mut().iso_packet_desc.as_mut_ptr()).actual_length = 6; }
            attachment.ingest(&slot, LIBUSB_TRANSFER_COMPLETED, &mut scratch).unwrap();
            unsafe { (*slot.transfer.as_mut().iso_packet_desc.as_mut_ptr()).actual_length = 0; }
            attachment.ingest(&slot, LIBUSB_TRANSFER_COMPLETED, &mut scratch).unwrap();
            unsafe { (*slot.transfer.as_mut().iso_packet_desc.as_mut_ptr()).actual_length = 5; }
            attachment.ingest(&slot, LIBUSB_TRANSFER_COMPLETED, &mut scratch).unwrap();
            unsafe { (*slot.transfer.as_mut().iso_packet_desc.as_mut_ptr()).status = LIBUSB_TRANSFER_NO_DEVICE; }
            assert_eq!(attachment.ingest(&slot, LIBUSB_TRANSFER_COMPLETED, &mut scratch), Err(Error::DeviceLost));
            assert_eq!(attachment.ingest(&slot, LIBUSB_TRANSFER_NO_DEVICE, &mut scratch), Err(Error::DeviceLost));
            unsafe { slot.transfer.as_mut().endpoint = 0x82; }
            assert!(attachment.ingest(&slot, LIBUSB_TRANSFER_COMPLETED, &mut scratch).is_err());
            attachment.reordered();
            attachment.finish(Some(Error::DeviceLost));
            drop(retirement);
            assert!(reader.retired.as_ref().unwrap().load(Ordering::Acquire));
            drop(held);

            let buffer = reader.lock_buffer();
            let stats = buffer.stats();
            assert_eq!(stats.captured_frames, 5);
            assert_eq!(stats.delivered_frames, 1);
            assert_eq!(stats.overrun_frames, 1);
            assert_eq!(stats.contention_frames, 3);
            assert_eq!(stats.empty_packets, 1);
            assert_eq!(stats.short_packets, 1);
            assert_eq!(stats.malformed_packets, 1);
            assert_eq!(stats.failed_packets, 1);
            assert_eq!(stats.failed_transfers, 1);
            assert_eq!(stats.malformed_completions, 1);
            assert_eq!(stats.reordered_completions, 1);
            assert_eq!(stats.missing_frames, 0);
            assert_eq!(stats.device_position_frames(), 5);
            assert_eq!(stats.captured_frames, stats.delivered_frames + buffer.available_frames() as u64
                + stats.overrun_frames + stats.contention_frames);
            assert_eq!(buffer.state(), CaptureState::Failed);
            drop(buffer);
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
        assert_eq!(capture_queue_bytes(98_303, geometry).unwrap(), 98_303 * geometry.packets_per_transfer);
        assert!(matches!(capture_queue_bytes(0, geometry), Err(Error::Unsupported { .. })));
        assert!(matches!(capture_queue_bytes(usize::MAX, geometry), Err(Error::Unsupported { .. })));
        assert!(matches!(capture_queue_bytes(i32::MAX as usize, geometry), Err(Error::Unsupported { .. })));
        let too_many = IsoQueueGeometry { transfer_slots: 256, ..geometry };
        assert!(matches!(capture_queue_bytes(4, too_many), Err(Error::Unsupported { .. })));
        let overflow = IsoQueueGeometry { transfer_slots: usize::MAX, ..geometry };
        assert!(matches!(capture_queue_bytes(98_303, overflow), Err(Error::Unsupported { .. })));
    }
}
