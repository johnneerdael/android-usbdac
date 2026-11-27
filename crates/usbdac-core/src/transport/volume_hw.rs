use super::*;
use crate::volume::{raw_to_db, VolumeRange as PublicVolumeRange, VolumeState};
use crate::config::StartupVolume;
use crate::volume::StartupVolumeReport;
use crate::volume_control::{
    plan_startup_attenuation, single_level, step_target_raw, HardwareVolumeSnapshot,
    SameRoute, VolumeCurve,
};

fn playback_volume_curve() -> VolumeCurve {
    if settings().has(crate::Quirk::MixerPlaybackLinearVolume) { VolumeCurve::LinearAmplitude }
    else { VolumeCurve::Decibels { minimum_is_mute: settings().has(crate::Quirk::MixerPlaybackMinMute) } }
}

#[derive(Debug, Clone)]
pub(super) struct AudioControlTopology {
    pub(super) interface_number: u8,
    pub(super) protocol: AudioControlProtocol,
    pub(super) streaming_interfaces: Vec<u8>,
    pub(super) input_terminals: Vec<InputTerminal>,
    pub(super) feature_units: Vec<FeatureUnit>,
    pub(super) output_terminals: Vec<OutputTerminal>,
    pub(super) sources: Vec<(u8, Vec<u8>)>,
}

pub(super) fn parse_audio_control_topologies(
    device: &Device<Context>,
) -> Result<Vec<AudioControlTopology>, String> {
    let config_descriptor = device
        .active_config_descriptor()
        .map_err(|error| format!("Failed to read active USB config descriptor: {}", error))?;
    let parser = AudioControlParser;
    let mut topologies = Vec::new();

    for interface in config_descriptor.interfaces() {
        for descriptor in interface.descriptors() {
            if descriptor.class_code() != USB_CLASS_AUDIO
                || descriptor.sub_class_code() != USB_SUBCLASS_AUDIOCONTROL
            {
                continue;
            }

            if descriptor.protocol_code() == 0x30 {
                let mut topology = AudioControlTopology {
                    interface_number: descriptor.interface_number(), protocol: AudioControlProtocol::Uac3,
                    streaming_interfaces: Vec::new(), input_terminals: Vec::new(), feature_units: Vec::new(),
                    output_terminals: Vec::new(), sources: Vec::new(),
                };
                for d in DescriptorIter::new(descriptor.extra()).filter(|d|d.get(1)==Some(&0x24)) {
                    match d.get(2).copied() {
                        Some(2) => {
                            let t=crate::descriptors::uac3::parse_input_terminal(d).map_err(|e|e.to_string())?;
                            topology.input_terminals.push(InputTerminal{b_terminal_id:t.id,w_terminal_type:t.terminal_type,
                                b_assoc_terminal:t.associated_terminal,b_c_source_id:t.clock_id,b_nr_channels:0,w_channel_config:0,i_terminal:0});
                        }
                        Some(3) => {
                            let t=crate::descriptors::uac3::parse_output_terminal(d).map_err(|e|e.to_string())?;
                            topology.sources.push((t.id,vec![t.source_id]));
                            topology.output_terminals.push(OutputTerminal{b_terminal_id:t.id,w_terminal_type:t.terminal_type,
                                b_assoc_terminal:t.associated_terminal,b_source_id:t.source_id,i_terminal:0});
                        }
                        Some(7) => {
                            let f=crate::descriptors::uac3::parse_feature_unit(d).map_err(|e|e.to_string())?;
                            topology.sources.push((f.id,vec![f.source_id]));
                            topology.feature_units.push(FeatureUnit{b_unit_id:f.id,b_source_id:f.source_id,b_control_size:4,
                                bma_controls:f.controls.iter().map(|c|c.0).collect()});
                        }
                        _=>{}
                    }
                }
                if !topology.input_terminals.is_empty() { topologies.push(topology); }
                continue;
            }

            let mut topology = AudioControlTopology {
                interface_number: descriptor.interface_number(),
                protocol: match descriptor.protocol_code() {
                    0x00 => AudioControlProtocol::Uac1,
                    0x20 => AudioControlProtocol::Uac2,
                    _ => continue,
                },
                streaming_interfaces: Vec::new(),
                input_terminals: Vec::new(),
                feature_units: Vec::new(),
                output_terminals: Vec::new(),
                sources: Vec::new(),
            };

            for extra in DescriptorIter::new(descriptor.extra()) {
                if extra.get(1) != Some(&USB_DT_CS_INTERFACE) {
                    continue;
                }
                let is_v1 = topology.protocol == AudioControlProtocol::Uac1;
                match extra.get(2).copied() {
                    Some(1) if is_v1 => {
                        let header = parser.parse_ac_header_v1(extra).map_err(|e| e.to_string())?;
                        topology.streaming_interfaces = header.ba_interface_nr;
                    }
                    Some(UAC2_INPUT_TERMINAL) => {
                        let terminal = if is_v1 { parser.parse_input_terminal_v1(extra) }
                            else { parser.parse_input_terminal(extra) }.map_err(|e| e.to_string())?;
                        topology.input_terminals.push(terminal);
                    }
                    Some(UAC2_OUTPUT_TERMINAL) => {
                        let terminal = if is_v1 { parser.parse_output_terminal_v1(extra) }
                            else { parser.parse_output_terminal(extra) }.map_err(|e| e.to_string())?;
                        topology.sources.push((terminal.b_terminal_id, vec![terminal.b_source_id]));
                        topology.output_terminals.push(terminal);
                    }
                    Some(UAC2_FEATURE_UNIT) => {
                        let unit = if is_v1 { parser.parse_feature_unit_v1(extra) }
                            else { parser.parse_feature_unit(extra) }.map_err(|e| e.to_string())?;
                        topology.sources.push((unit.b_unit_id, vec![unit.b_source_id]));
                        topology.feature_units.push(unit);
                    }
                    Some(4 | 5) => {
                        let count = extra.get(4).copied().unwrap_or(0) as usize;
                        if count == 0 || extra.len() < 6 + count {
                            return Err("malformed USB mixer/selector sources".to_string());
                        }
                        // Do not guess the active branch of a multi-input selector.
                        if extra[2] == 4 || count == 1 {
                            topology.sources.push((extra[3], extra[5..5 + count].to_vec()));
                        }
                    }
                    _ => {}
                }
            }

            if !topology.feature_units.is_empty() || !topology.output_terminals.is_empty() {
                topologies.push(topology);
            }
        }
    }

    Ok(topologies)
}

pub(super) fn read_feature_unit_i16_control(
    device: &UsbDevice,
    interface_number: u8,
    feature_unit_id: u8,
    channel: u16,
    request: u8,
    control_selector: u16,
) -> Result<i16, String> {
    let request_type = USB_DIR_IN | USB_TYPE_CLASS | USB_RECIP_INTERFACE;
    let value = control_selector | (channel & 0xff);
    let index = (interface_number as u16) | ((feature_unit_id as u16) << 8);
    let mut data = [0u8; 2];
    let transferred = device
        .feature_unit_control(request_type, request, value, index, &mut data)
        .map_err(|error| format!(
            "USB volume read request=0x{request:02x} interface={interface_number} unit={feature_unit_id} channel={channel} selector=0x{control_selector:04x}: {error}"
        ))?;
    if transferred < data.len() {
        return Err(format!(
            "USB volume read request=0x{request:02x} interface={interface_number} unit={feature_unit_id} channel={channel} returned {} bytes, expected {}",
            transferred,
            data.len()
        ));
    }
    Ok(i16::from_le_bytes(data))
}

pub(super) fn write_feature_unit_i16_control(
    device: &UsbDevice,
    interface_number: u8,
    feature_unit_id: u8,
    channel: u16,
    control_selector: u16,
    value_raw: i16,
) -> Result<(), String> {
    let request_type = USB_DIR_OUT | USB_TYPE_CLASS | USB_RECIP_INTERFACE;
    let value = control_selector | (channel & 0xff);
    let index = (interface_number as u16) | ((feature_unit_id as u16) << 8);
    let mut data = value_raw.to_le_bytes();
    let transferred = device
        .feature_unit_control(request_type, UAC2_REQUEST_SET_CUR, value, index, &mut data)
        .map_err(|error| format!(
            "USB volume SET_CUR interface={interface_number} unit={feature_unit_id} channel={channel} selector=0x{control_selector:04x}: {error}"
        ))?;
    if transferred != data.len() {
        return Err(format!(
            "short USB volume SET_CUR interface={interface_number} unit={feature_unit_id} channel={channel}: {transferred}/{} bytes", data.len()
        ));
    }
    Ok(())
}

pub(super) fn read_feature_unit_bool_control(
    device: &UsbDevice,
    interface_number: u8,
    feature_unit_id: u8,
    channel: u16,
    protocol: AudioControlProtocol,
    control_selector: u16,
) -> Result<bool, String> {
    let request_type = USB_DIR_IN | USB_TYPE_CLASS | USB_RECIP_INTERFACE;
    let value = control_selector | (channel & 0xff);
    let index = (interface_number as u16) | ((feature_unit_id as u16) << 8);
    let mut data = [0u8; 1];
    let transferred = device
        .feature_unit_control(
            request_type,
            protocol.current_request(),
            value,
            index,
            &mut data,
        )
        .map_err(|error| format!(
            "USB mute {protocol:?} GET_CUR interface={interface_number} unit={feature_unit_id} channel={channel}: {error}"
        ))?;
    if transferred < data.len() {
        return Err(format!(
            "USB mute {protocol:?} GET_CUR interface={interface_number} unit={feature_unit_id} channel={channel} returned {} bytes, expected {}",
            transferred,
            data.len()
        ));
    }
    Ok(data[0] != 0)
}

pub(super) fn write_feature_unit_bool_control(
    device: &UsbDevice,
    interface_number: u8,
    feature_unit_id: u8,
    channel: u16,
    control_selector: u16,
    enabled: bool,
) -> Result<(), String> {
    let request_type = USB_DIR_OUT | USB_TYPE_CLASS | USB_RECIP_INTERFACE;
    let value = control_selector | (channel & 0xff);
    let index = (interface_number as u16) | ((feature_unit_id as u16) << 8);
    let mut data = [u8::from(enabled)];
    let transferred = device
        .feature_unit_control(request_type, UAC2_REQUEST_SET_CUR, value, index, &mut data)
        .map_err(|error| format!(
            "USB mute SET_CUR interface={interface_number} unit={feature_unit_id} channel={channel}: {error}"
        ))?;
    if transferred != data.len() {
        return Err(format!(
            "short USB mute SET_CUR interface={interface_number} unit={feature_unit_id} channel={channel}: {transferred}/{} bytes", data.len()
        ));
    }
    Ok(())
}

pub(super) fn read_feature_unit_volume_range(
    device: &UsbDevice,
    interface_number: u8,
    feature_unit_id: u8,
    channel: u16,
    protocol: AudioControlProtocol,
) -> Result<Vec<RawRange>, String> {
    if protocol != AudioControlProtocol::Uac1 {
        // Read the count first, then request exactly the advertised response.
        // Some DAC firmware rejects an oversized RANGE request.
        let request_type = USB_DIR_IN | USB_TYPE_CLASS | USB_RECIP_INTERFACE;
        let value = UAC2_FEATURE_UNIT_VOLUME_CONTROL | (channel & 0xff);
        let index = interface_number as u16 | (feature_unit_id as u16) << 8;
        let mut count_bytes = [0u8; 2];
        let transferred = device.feature_unit_control(
            request_type, UAC2_REQUEST_GET_RANGE, value, index,
            &mut count_bytes,
        ).map_err(|e| format!("USB volume RANGE count interface={interface_number} unit={feature_unit_id} channel={channel}: {e}"))?;
        if transferred != count_bytes.len() {
            return Err(format!("short USB volume RANGE count interface={interface_number} unit={feature_unit_id} channel={channel}: {transferred}/2 bytes"));
        }
        let count = u16::from_le_bytes(count_bytes) as usize;
        if !(1..=64).contains(&count) {
            return Err(format!("invalid USB volume RANGE count interface={interface_number} unit={feature_unit_id} channel={channel}: {count}"));
        }
        let mut data = [0u8; 386];
        let expected = 2 + count * 6;
        let transferred = device.feature_unit_control(
            request_type, UAC2_REQUEST_GET_RANGE, value, index,
            &mut data[..expected],
        ).map_err(|e| format!("USB volume RANGE interface={interface_number} unit={feature_unit_id} channel={channel}: {e}"))?;
        if transferred != expected {
            return Err(format!("short USB volume RANGE interface={interface_number} unit={feature_unit_id} channel={channel}: {transferred}/{expected} bytes"));
        }
        return parse_uac2_volume_ranges(&data[..transferred]);
    }
    Ok(vec![RawRange {
        min:
        read_feature_unit_i16_control(
            device,
            interface_number,
            feature_unit_id,
            channel,
            UAC1_REQUEST_GET_MIN,
            UAC2_FEATURE_UNIT_VOLUME_CONTROL,
        )?,
        max:
        read_feature_unit_i16_control(
            device,
            interface_number,
            feature_unit_id,
            channel,
            UAC1_REQUEST_GET_MAX,
            UAC2_FEATURE_UNIT_VOLUME_CONTROL,
        )?,
        resolution:
        read_feature_unit_i16_control(
            device,
            interface_number,
            feature_unit_id,
            channel,
            UAC1_REQUEST_GET_RES,
            UAC2_FEATURE_UNIT_VOLUME_CONTROL,
        )?,
    }.validate()?])
}

pub(super) fn build_hardware_volume_control_from_feature_unit(
    device: &UsbDevice,
    topology: &AudioControlTopology,
    feature_unit: &FeatureUnit,
    channels: u16,
) -> Result<Option<HardwareVolumeControl>, String> {
    let Some(volume_channels) = topology.protocol.volume_channels(&feature_unit.bma_controls, channels)
        else { return Ok(None); };
    let mut ranges = Vec::new();
    for &channel in &volume_channels {
        let channel_ranges = read_feature_unit_volume_range(
            device, topology.interface_number, feature_unit.b_unit_id, channel, topology.protocol,
        )?;
        if ranges.is_empty() { ranges = channel_ranges; }
        else if ranges != channel_ranges {
            return Err("per-channel USB volume ranges differ; unified control refused".to_string());
        }
    }
    let (min_volume_raw, max_volume_raw) = volume_bounds(&ranges);
    if min_volume_raw == max_volume_raw {
        return Ok(None);
    }
    // Master mute only: muting one channel would not mute playback.
    let mute_channel = topology.protocol.writable(feature_unit.bma_controls[0], 1).then_some(0);
    let resolution_raw = ranges.iter().map(|range| range.resolution).min().unwrap_or(0);
    Ok(Some(HardwareVolumeControl {
        interface_number: topology.interface_number,
        feature_unit_id: feature_unit.b_unit_id,
        protocol: topology.protocol,
        volume_channels,
        ranges,
        mute_channel,
        min_volume_raw,
        max_volume_raw,
        resolution_raw,
    }))
}

pub(super) fn discover_hardware_volume_control_for_terminal(
    device: &UsbDevice,
    topology: &AudioControlTopology,
    terminal_link: u8,
    channels: u16,
) -> Result<Option<HardwareVolumeControl>, String> {
    // An OUT streaming terminal must be a USB input terminal, not capture.
    if !topology.input_terminals.iter().any(|terminal|
        terminal.b_terminal_id == terminal_link && terminal.w_terminal_type == 0x0101
    ) { return Ok(None); }
    let mut ancestors = HashSet::new();
    let mut pending: Vec<u8> = topology.output_terminals.iter()
        .filter(|terminal| terminal.w_terminal_type != 0x0101)
        .map(|terminal| terminal.b_terminal_id).collect();
    while let Some(entity) = pending.pop() {
        if ancestors.insert(entity) {
            for (_, sources) in topology.sources.iter().filter(|(id, _)| *id == entity) {
                pending.extend(sources.iter().copied());
            }
        }
    }
    let mut pending = vec![terminal_link];
    let mut visited = HashSet::new();

    while let Some(entity_id) = pending.pop() {
        if !visited.insert(entity_id) {
            continue;
        }

        if let Some(feature_unit) = topology
            .feature_units
            .iter()
            .find(|feature_unit| feature_unit.b_unit_id == entity_id)
        {
            if ancestors.contains(&entity_id) {
                match build_hardware_volume_control_from_feature_unit(device,topology,feature_unit,channels) {
                    Ok(Some(control)) => return Ok(Some(control)),
                    Ok(None) => (),
                    Err(error) if settings().has(crate::Quirk::IgnoreOptionalControlErrors)
                        && !crate::error::is_device_gone(&error) => {
                        log_warn!("[USB] Optional mixer unit {} unavailable: {}; startup attenuation and mandatory control failures remain strict",
                            feature_unit.b_unit_id,error);
                    }
                    Err(error) => return Err(error),
                }
            }
        }

        for (id, sources) in &topology.sources {
            if sources.contains(&entity_id) && ancestors.contains(id) {
                pending.push(*id);
            }
        }
    }

    Ok(None)
}

pub(super) fn discover_android_usb_hardware_volume_control(
    descriptors: &Device<Context>,
    device: &UsbDevice,
    terminal_link: Option<u8>,
    interface_number: u8,
    channels: u16,
    clock_interface: Option<u8>,
) -> Result<Option<HardwareVolumeControl>, String> {
    let terminal = terminal_link.ok_or_else(|| "no playback terminal link for USB volume".to_string())?;
    let config = descriptors.active_config_descriptor().map_err(|e|e.to_string())?;
    let is_adc3 = config.interfaces().flat_map(|i|i.descriptors()).any(|d|d.interface_number()==interface_number&&d.protocol_code()==0x30);
    if is_adc3 {
        let claimed=wrap_android_usb_handle(device)?;
        if let Some(topology)=adc3_badd_volume_topology(descriptors,&claimed.handle,interface_number,terminal,channels)? {
            return discover_hardware_volume_control_for_terminal(device,&topology,terminal,channels);
        }
    }
    let topologies = parse_audio_control_topologies(descriptors)?;
    let matches: Vec<_> = topologies.iter().filter(|topology| {
        clock_interface.map_or(true, |interface| topology.interface_number == interface)
            && (topology.protocol != AudioControlProtocol::Uac1
                || topology.streaming_interfaces.contains(&interface_number))
            && topology.input_terminals.iter().any(|input| input.b_terminal_id == terminal)
    }).collect();
    if matches.len() != 1 {
        return Err("USB playback AudioControl topology is missing or ambiguous".to_string());
    }
    discover_hardware_volume_control_for_terminal(device, matches[0], terminal, channels)
}

pub(super) fn refresh_android_usb_hardware_volume_snapshot(
    device: &UsbDevice,
    control: &HardwareVolumeControl,
) -> Result<HardwareVolumeSnapshot, String> {
    const GET_CUR_RETRIES: u32 = 3;
    const RETRY_DELAY_MS: u64 = 10;

    let mut last_err = None;
    for attempt in 0..GET_CUR_RETRIES {
        let read = read_hardware_volume_snapshot(
            &control.volume_channels,
            &control.ranges,
            |channel| read_feature_unit_i16_control(
                device, control.interface_number, control.feature_unit_id, channel,
                control.protocol.current_request(), UAC2_FEATURE_UNIT_VOLUME_CONTROL,
            ),
            || control.mute_channel.map(|channel| read_feature_unit_bool_control(
                device, control.interface_number, control.feature_unit_id, channel,
                control.protocol, UAC2_FEATURE_UNIT_MUTE_CONTROL,
            )).transpose(),
        );
        match read {
            Ok(mut snapshot) => {
                let curve = playback_volume_curve();
                snapshot.db = curve.db(snapshot.raw,&control.ranges);
                snapshot.volume = curve.fraction(snapshot.raw,&control.ranges);
                if let Some(error) = &snapshot.mute_error {
                    log_warn!("[USB] Hardware volume remains available; optional mute unavailable: {error}");
                }
                return Ok(snapshot);
            }
            Err(e) => {
                dev_eprintln!("[VolFlow] GET_CUR attempt {} failed: {e}", attempt + 1);
                // A vanished device will not answer a retry.
                if crate::error::is_device_gone(&e) {
                    return Err(e);
                }
                last_err = Some(e);
                if attempt < GET_CUR_RETRIES - 1 {
                    std::thread::sleep(Duration::from_millis(RETRY_DELAY_MS));
                }
            }
        }
    }
    Err(last_err.unwrap_or_else(|| "GET_CUR failed with no error recorded".to_string()))
}

impl SameRoute for HardwareVolumeControl {
    fn same_route(&self, other: &Self) -> bool {
        HardwareVolumeControl::same_route(self, other)
    }
}

impl HardwareVolumeControl {
    /// Whether `other` drives the same Feature Unit the same way.
    pub(super) fn same_route(&self, other: &HardwareVolumeControl) -> bool {
        self.interface_number == other.interface_number
            && self.feature_unit_id == other.feature_unit_id
            && self.protocol == other.protocol
            && self.volume_channels == other.volume_channels
            && self.ranges == other.ranges
            && self.mute_channel == other.mute_channel
    }
}

pub(super) fn volume_state_of(snapshot: &HardwareVolumeSnapshot) -> VolumeState {
    VolumeState {
        db: snapshot.db,
        fraction: snapshot.volume,
        muted: snapshot.muted,
    }
}

/// Reads the level every channel holds and writes `target_raw` to each, with
/// best-effort rollback of the channels already written if one write fails, then
/// verifies by reading the level back. Caller holds HARDWARE_VOLUME_MUTEX.
pub(super) fn apply_android_usb_hardware_volume(
    device: &UsbDevice,
    control: &HardwareVolumeControl,
    generation: u64,
    target_raw: i16,
) -> Result<HardwareVolumeSnapshot, String> {
    dev_eprintln!(
        "[VolFlow] min={} max={} res={} -> raw={target_raw}",
        control.min_volume_raw, control.max_volume_raw, control.resolution_raw
    );
    // Snapshot every channel before a grouped write, for best-effort rollback.
    let previous = control.volume_channels.iter().map(|&channel| read_feature_unit_i16_control(
        device, control.interface_number, control.feature_unit_id, channel,
        control.protocol.current_request(), UAC2_FEATURE_UNIT_VOLUME_CONTROL,
    )).collect::<Result<Vec<_>, _>>()?;
    for (index, &channel) in control.volume_channels.iter().enumerate() {
        let unchanged = SESSION_STATE.lock().as_ref()
            .is_some_and(|current| current.hardware_volume_generation == generation);
        if !unchanged { return Err("USB volume route changed before write".to_string()); }
        if let Err(error) = write_feature_unit_i16_control(
            device, control.interface_number, control.feature_unit_id, channel,
            UAC2_FEATURE_UNIT_VOLUME_CONTROL, target_raw,
        ) {
            for (&channel, &raw) in control.volume_channels[..=index].iter().zip(&previous) {
                let _ = write_feature_unit_i16_control(
                    device, control.interface_number, control.feature_unit_id, channel,
                    UAC2_FEATURE_UNIT_VOLUME_CONTROL, raw,
                );
            }
            return Err(error);
        }
    }

    let snapshot = refresh_android_usb_hardware_volume_snapshot(device, control)?;
    if snapshot.raw != target_raw {
        dev_eprintln!(
            "[VolFlow] post-SET_CUR volume mismatch: expected raw {target_raw} got {}",
            snapshot.raw
        );
        return Err(format!(
            "volume verification failed: wrote raw {target_raw}, the DAC reads {}",
            snapshot.raw
        ));
    }
    Ok(snapshot)
}

/// The route the next write or read applies to.
fn volume_route() -> Result<(SessionState, HardwareVolumeControl), String> {
    let state = SESSION_STATE
        .lock()
        .as_ref()
        .cloned()
        .ok_or_else(|| "no DAC is open".to_string())?;
    let control = state
        .hardware_volume_control
        .clone()
        .ok_or_else(|| "the DAC has no usable hardware volume control".to_string())?;
    Ok((state, control))
}

/// Whether the DAC has a usable hardware volume control right now.
pub(crate) fn has_hardware_volume() -> bool {
    SESSION_STATE
        .lock()
        .as_ref()
        .is_some_and(|state| state.hardware_volume_control.is_some())
}

pub(crate) fn hardware_volume_range() -> Option<PublicVolumeRange> {
    let guard = SESSION_STATE.lock();
    let control = guard.as_ref()?.hardware_volume_control.as_ref()?;
    Some(PublicVolumeRange {
        min_db: playback_volume_curve().db(control.min_volume_raw,&control.ranges),
        max_db: playback_volume_curve().db(control.max_volume_raw,&control.ranges),
        res_db: if playback_volume_curve() == VolumeCurve::LinearAmplitude { 0.0 }
            else { raw_to_db(control.resolution_raw.max(1)) },
    })
}

/// The last level this library read or wrote.
pub(crate) fn cached_volume_state() -> Option<VolumeState> {
    SESSION_STATE.lock().as_ref()?.hardware_volume_state
}

/// Reads the live level from the DAC.
pub(crate) fn read_volume_state() -> Result<VolumeState, String> {
    let _hold = HARDWARE_VOLUME_MUTEX.lock();
    let (state, control) = volume_route()?;
    let snapshot = refresh_android_usb_hardware_volume_snapshot(&state.device, &control)
        .inspect_err(|e| set_hardware_volume_error(Some(e.clone())))?;
    let volume = volume_state_of(&snapshot);
    publish_hardware_volume_snapshot(state.hardware_volume_generation, volume)?;
    Ok(volume)
}

/// Writes a raw level. Caller holds HARDWARE_VOLUME_MUTEX.
fn write_volume_raw_locked(
    state: &SessionState,
    control: &HardwareVolumeControl,
    target_raw: i16,
) -> Result<VolumeState, String> {
    let snapshot = apply_android_usb_hardware_volume(
        &state.device,
        control,
        state.hardware_volume_generation,
        target_raw,
    )
    .inspect_err(|e| {
        dev_eprintln!("[VolFlow] SET_CUR failed: {e}");
        set_hardware_volume_error(Some(e.clone()));
    })?;
    let volume = volume_state_of(&snapshot);
    publish_hardware_volume_snapshot(state.hardware_volume_generation, volume)?;
    dev_eprintln!("[VolFlow] SET_CUR OK: {:.2} dB", volume.db);
    Ok(volume)
}

pub(crate) fn write_volume_db(db: f64) -> Result<VolumeState, String> {
    let _hold = HARDWARE_VOLUME_MUTEX.lock();
    let (state, control) = volume_route()?;
    let target_raw = playback_volume_curve().target_db(db, &control.ranges);
    write_volume_raw_locked(&state, &control, target_raw)
}

pub(crate) fn write_volume_fraction(fraction: f64) -> Result<VolumeState, String> {
    let _hold = HARDWARE_VOLUME_MUTEX.lock();
    let (state, control) = volume_route()?;
    let target_raw = playback_volume_curve().target_fraction(fraction, &control.ranges);
    write_volume_raw_locked(&state, &control, target_raw)
}

/// Moves the level by `1/steps` of the 0..1 curve, and by at least one DAC
/// step, in the direction of `direction`'s sign. At the end of the range this
/// reads and returns the current level.
pub(crate) fn step_volume(direction: i8, steps: u8) -> Result<VolumeState, String> {
    let _hold = HARDWARE_VOLUME_MUTEX.lock();
    let (state, control) = volume_route()?;
    let snapshot = refresh_android_usb_hardware_volume_snapshot(&state.device, &control)
        .inspect_err(|e| set_hardware_volume_error(Some(e.clone())))?;
    if direction == 0 {
        let volume = volume_state_of(&snapshot);
        publish_hardware_volume_snapshot(state.hardware_volume_generation, volume)?;
        return Ok(volume);
    }
    let target_raw = if playback_volume_curve() == VolumeCurve::LinearAmplitude {
        let target = playback_volume_curve().target_fraction(snapshot.volume + f64::from(direction.signum()) / f64::from(steps.max(1)),&control.ranges);
        if target == snapshot.raw { crate::volume_control::adjacent_raw(snapshot.raw,direction,&control.ranges) } else {target}
    } else { step_target_raw(snapshot.raw, snapshot.volume, direction, steps, &control.ranges) };
    if target_raw == snapshot.raw {
        let volume = volume_state_of(&snapshot);
        publish_hardware_volume_snapshot(state.hardware_volume_generation, volume)?;
        return Ok(volume);
    }
    write_volume_raw_locked(&state, &control, target_raw)
}

/// Sets the master mute and verifies it by reading it back.
pub(crate) fn write_mute(muted: bool) -> Result<VolumeState, String> {
    let _hold = HARDWARE_VOLUME_MUTEX.lock();
    let (state, control) = volume_route()?;
    let mute_channel = control
        .mute_channel
        .ok_or_else(|| "the DAC has no writable mute".to_string())?;
    if !SESSION_STATE.lock().as_ref()
        .is_some_and(|current| current.hardware_volume_generation == state.hardware_volume_generation) {
        return Err("USB mute route changed before write".to_string());
    }
    write_feature_unit_bool_control(
        &state.device,
        control.interface_number,
        control.feature_unit_id,
        mute_channel,
        UAC2_FEATURE_UNIT_MUTE_CONTROL,
        muted,
    )?;
    let snapshot = refresh_android_usb_hardware_volume_snapshot(&state.device, &control)?;
    // Publish the observed state even when mute readback fails or disagrees.
    // Healthy volume must remain usable, and an unknown mute must not stay cached.
    let volume = volume_state_of(&snapshot);
    publish_hardware_volume_snapshot(state.hardware_volume_generation, volume)?;
    if snapshot.muted != Some(muted) {
        return Err("USB mute write did not match readback".to_string());
    }
    Ok(volume)
}

/// Installs `control` as the route volume calls use, unless it is the route
/// already installed (then nothing changes, in-flight writes stay valid).
pub(super) fn install_volume_control(control: Option<HardwareVolumeControl>) {
    let mut guard = SESSION_STATE.lock();
    let Some(state) = guard.as_mut() else {
        return;
    };
    let same = match (&state.hardware_volume_control, &control) {
        (Some(current), Some(new)) => current.same_route(new),
        (None, None) => true,
        _ => false,
    };
    if same {
        return;
    }
    state.hardware_volume_control = control;
    state.hardware_volume_generation = HARDWARE_VOLUME_GENERATION.fetch_add(1, Ordering::Relaxed);
    state.hardware_volume_state = None;
}

/// Finds the hardware volume control of the terminal a stream (or, at open, the
/// first playback stream) feeds, records the DAC's original level the first
/// time, and caches the current level. Failure only means "no hardware volume":
/// it is logged and never stops a stream.
pub(super) fn refresh_stream_volume_control(
    device: &UsbDevice,
    descriptors: &Device<Context>,
    terminal_link: Option<u8>,
    interface_number: u8,
    channels: u16,
    clock_interface: Option<u8>,
) {
    let _hold = HARDWARE_VOLUME_MUTEX.lock();
    let discovered = discover_android_usb_hardware_volume_control(
        descriptors,
        device,
        terminal_link,
        interface_number,
        channels,
        clock_interface,
    );
    let control = match discovered {
        Ok(control) => {
            set_hardware_volume_error(if control.is_none() {
                Some("no writable volume on the active USB playback topology".to_string())
            } else {
                None
            });
            control
        }
        Err(error) => {
            log_error!("[USB] Hardware volume discovery failed: {}", error);
            set_hardware_volume_error(Some(error));
            // Keep whatever route was found before; a failed rediscovery is not
            // proof that the volume went away.
            return;
        }
    };
    adopt_volume_control(device, control);
}

/// Installs what discovery found as the route volume calls use, records its
/// original level and runs the start-up step before anything can play through
/// it, then caches the level. A control whose start-up step fails is not
/// offered. Caller holds HARDWARE_VOLUME_MUTEX.
pub(super) fn adopt_volume_control(device: &UsbDevice, control: Option<HardwareVolumeControl>) {
    install_volume_control(control.clone());
    let Some(control) = control else {
        log_info!("[USB] No hardware volume control discovered");
        return;
    };
    // Before any write of this library's own.
    note_original_hardware_volume(device, &control);
    log_info!(
        "[USB] Hardware volume control found: feature_unit={}, interface={}, min={}, max={}, res={}, protocol={:?}, channels={:?}, mute_channel={:?}",
        control.feature_unit_id,
        control.interface_number,
        control.min_volume_raw,
        control.max_volume_raw,
        control.resolution_raw,
        control.protocol,
        control.volume_channels,
        control.mute_channel
    );
    // The start-up step, once per route and session, before anything can be
    // played through it. A route that comes back (the stream's terminal is the
    // one found at open) keeps the level the host has set since.
    if startup_volume_pending(&control) {
        match run_startup_volume(device, &control) {
            Ok(status) => {
                log_info!(
                    "[USB] Start-up volume: original {:?} dB, now {:?} dB, limit {:.2} dB{}",
                    status.report.original_db,
                    status.report.applied_db,
                    status.report.limit_db,
                    if status.report.clamped_to_minimum { " (clamped to the DAC's minimum)" } else { "" },
                );
                set_startup_volume(status);
            }
            Err(error) => {
                // Without a verified, safe level the control is not offered:
                // a stream through it would play at whatever the DAC had.
                log_error!("[USB] Start-up volume protection failed, the volume control is unusable: {error}");
                install_volume_control(None);
                set_hardware_volume_error(Some(format!("start-up volume protection failed: {error}")));
                return;
            }
        }
    }
    let generation = SESSION_STATE
        .lock()
        .as_ref()
        .map(|state| state.hardware_volume_generation)
        .unwrap_or_default();
    match refresh_android_usb_hardware_volume_snapshot(device, &control) {
        Ok(snapshot) => {
            let volume = volume_state_of(&snapshot);
            log_info!(
                "[USB] Hardware volume initial: {:.2} dB (raw {}), muted={:?}",
                volume.db,
                snapshot.raw,
                volume.muted
            );
            let _ = publish_hardware_volume_snapshot(generation, volume);
        }
        Err(error) => {
            log_error!(
                "[USB] Failed to read the hardware volume of feature unit {} on interface {}: {}",
                control.feature_unit_id,
                control.interface_number,
                error
            );
            install_volume_control(None);
            set_hardware_volume_error(Some(error));
        }
    }
}

/// At open, before any stream: finds the volume control through the first PCM
/// playback stream so volume works without one.
pub(crate) fn discover_idle_volume_control() {
    let Some(device) = SESSION_STATE.lock().as_ref().map(|state| state.device.clone()) else {
        return;
    };
    let usb_device = {
        let guard = IDLE_LOCK.lock();
        match guard.as_ref() {
            Some(lock) => lock.handle.device(),
            None => return,
        }
    };
    let Some((interface_number, terminal_link, channels)) = first_playback_stream(&usb_device)
    else {
        return;
    };
    refresh_stream_volume_control(
        &device,
        &usb_device,
        Some(terminal_link),
        interface_number,
        channels,
        None,
    );
}

pub(super) fn original_volume_device_key(device: &UsbDevice) -> String {
    if let Some(key) = context::current().volume_key() { return key; }
    format!(
        "{}:{:04x}:{:04x}",
        device.device_name.as_deref().unwrap_or_default(),
        device.vendor_id,
        device.product_id
    )
}

pub(super) fn read_channel_volumes(
    device: &UsbDevice,
    control: &HardwareVolumeControl,
) -> Result<Vec<i16>, String> {
    control
        .volume_channels
        .iter()
        .map(|&channel| {
            read_feature_unit_i16_control(
                device,
                control.interface_number,
                control.feature_unit_id,
                channel,
                control.protocol.current_request(),
                UAC2_FEATURE_UNIT_VOLUME_CONTROL,
            )
        })
        .collect()
}

/// Remembers the level every channel of a DAC has the first time its volume
/// control is found during an attachment, which is before this library has
/// written to it. Caller holds HARDWARE_VOLUME_MUTEX.
pub(super) fn note_original_hardware_volume(
    device: &UsbDevice,
    control: &HardwareVolumeControl,
) {
    let key = original_volume_device_key(device);
    if ORIGINAL_HARDWARE_VOLUME.lock().is_route_recorded(&key, control) {
        return;
    }
    match read_channel_volumes(device, control) {
        Ok(channels) => {
            log_info!("[USB] Original hardware volume of '{}': raw={channels:?}", device.product_name);
            ORIGINAL_HARDWARE_VOLUME.lock().note(&key, control.clone(), channels);
        }
        Err(error) => log_warn!("[USB] Original hardware volume not recorded: {error}"),
    }
}

/// What the device clear that follows does with the recorded original level:
/// write it back (the host hands the DAC to another driver), forget it (the DAC
/// is gone) or keep it (the host holds on or takes the DAC straight back).
pub(crate) fn set_original_volume_action(action: u32) -> bool {
    if action > ORIGINAL_VOLUME_FORGET {
        return false;
    }
    context::current().keep_original(action == ORIGINAL_VOLUME_KEEP);
    ORIGINAL_VOLUME_ACTION.store(action, Ordering::SeqCst);
    true
}

/// The recorded original level in dB, if every channel of the current control
/// had the same one.
pub(crate) fn original_volume_db() -> Option<f64> {
    let (device, control) = {
        let guard = SESSION_STATE.lock();
        let state = guard.as_ref()?;
        (state.device.clone(), state.hardware_volume_control.clone()?)
    };
    ORIGINAL_HARDWARE_VOLUME
        .lock()
        .peek(&original_volume_device_key(&device), &control)
        .and_then(|original| original.common())
        .map(|raw| playback_volume_curve().db(raw,&control.ranges))
}

/// Writes the recorded original level of every channel back now and ends the
/// record. `Ok(false)` when there is none or the DAC already holds it.
pub(crate) fn hand_back_original_volume() -> Result<bool, String> {
    let _hold = HARDWARE_VOLUME_MUTEX.lock();
    let device = SESSION_STATE
        .lock()
        .as_ref()
        .map(|state| state.device.clone())
        .ok_or_else(|| "no DAC is open".to_string())?;
    let originals = ORIGINAL_HARDWARE_VOLUME
        .lock()
        .take_all(&original_volume_device_key(&device));
    let mut restored = false;
    let mut failure = None;
    for original in originals {
        match restore_original_hardware_volume(&device, &original.control, &original.channels) {
            Ok(wrote) => restored |= wrote,
            Err(error) => failure = failure.or(Some(error)),
        }
    }
    match failure {
        Some(error) => Err(error),
        None => Ok(restored),
    }
}

/// Settles the original level as the device is cleared, which is when another
/// driver gets the DAC back. The stream has stopped by then; the control
/// transfers need only the registered device. Best effort: a failure is logged,
/// and the record ends either way. Caller holds HARDWARE_VOLUME_MUTEX.
pub(super) fn finish_original_hardware_volume() {
    match ORIGINAL_VOLUME_ACTION.swap(ORIGINAL_VOLUME_KEEP, Ordering::SeqCst) {
        ORIGINAL_VOLUME_FORGET => ORIGINAL_HARDWARE_VOLUME.lock().forget(),
        ORIGINAL_VOLUME_RESTORE => {
            let Some(device) = SESSION_STATE.lock().as_ref().map(|state| state.device.clone()) else {
                return;
            };
            let originals = ORIGINAL_HARDWARE_VOLUME
                .lock()
                .take_all(&original_volume_device_key(&device));
            for original in originals {
                match restore_original_hardware_volume(&device, &original.control, &original.channels) {
                    Ok(true) => log_info!("[USB] Restored original hardware volume: raw={:?}", original.channels),
                    Ok(false) => {}
                    Err(error) => log_warn!("[USB] Original hardware volume not restored: {error}"),
                }
            }
        }
        _ => {}
    }
}

/// Writes each channel's `channels` level, exactly as the DAC reported it, back
/// and reads them back. Ok(false) when they were already there.
pub(super) fn restore_original_hardware_volume(
    device: &UsbDevice,
    control: &HardwareVolumeControl,
    channels: &[i16],
) -> Result<bool, String> {
    if read_channel_volumes(device, control)? == channels {
        return Ok(false);
    }
    for (&channel, &raw) in control.volume_channels.iter().zip(channels) {
        write_feature_unit_i16_control(
            device,
            control.interface_number,
            control.feature_unit_id,
            channel,
            UAC2_FEATURE_UNIT_VOLUME_CONTROL,
            raw,
        )?;
    }
    let readback = read_channel_volumes(device, control)?;
    if readback != channels {
        return Err(format!("read back {readback:?} after writing {channels:?}"));
    }
    Ok(true)
}

/// What the start-up step left behind for one route.
#[derive(Debug, Clone)]
pub(super) struct StartupVolumeStatus {
    pub(super) route: HardwareVolumeControl,
    pub(super) report: StartupVolumeReport,
    /// This open wrote to the DAC (an earlier attenuation that a `Keep` hand-back
    /// left in place is not this open's change).
    pub(super) lowered: bool,
}

/// Whether the start-up step has yet to run for `control` in this session.
pub(super) fn startup_volume_pending(control: &HardwareVolumeControl) -> bool {
    SESSION_STATE.lock().as_ref().is_some_and(|state| {
        !state
            .startup_volume
            .as_ref()
            .is_some_and(|status| status.route.same_route(control))
    })
}

fn set_startup_volume(status: StartupVolumeStatus) {
    if let Some(state) = SESSION_STATE.lock().as_mut() {
        state.startup_volume = Some(status);
    }
}

/// Runs the configured start-up step on `control`: reads every channel, lowers
/// what is louder than the ceiling and verifies the result by reading it back.
/// The originals must be recorded first (they are the report's `original_db` and
/// what hand-back restores). Caller holds HARDWARE_VOLUME_MUTEX.
pub(super) fn run_startup_volume(
    device: &UsbDevice,
    control: &HardwareVolumeControl,
) -> Result<StartupVolumeStatus, String> {
    let startup = settings().config.startup_volume;
    let current = read_channel_volumes(device, control)?;
    let originals = ORIGINAL_HARDWARE_VOLUME
        .lock()
        .peek(&original_volume_device_key(device), control)
        .map(|original| original.channels.clone())
        .unwrap_or_else(|| current.clone());
    apply_startup_volume(
        startup,
        &originals,
        current,
        &control.ranges,
        playback_volume_curve(),
        |channel_index, raw| {
            write_feature_unit_i16_control(
                device,
                control.interface_number,
                control.feature_unit_id,
                control.volume_channels[channel_index],
                UAC2_FEATURE_UNIT_VOLUME_CONTROL,
                raw,
            )
        },
        || read_channel_volumes(device, control),
    )
    .map(|(report, lowered)| StartupVolumeStatus { route: control.clone(), report, lowered })
}

/// The start-up step against any DAC that can write and read a channel. Returns
/// the report and whether anything was written.
///
/// `originals` are the levels before the library first touched the DAC, `current`
/// the levels now (the same on a first open). Each channel is lowered to
/// `min(its level, max_db)`; if that leaves channels at different levels, the
/// louder ones follow the quietest so the control, which holds one level, stays
/// usable. Only channels that change are written, and every channel is read back
/// and compared with what was meant: any difference is an error and the caller
/// treats the control as unusable.
pub(super) fn apply_startup_volume(
    startup: StartupVolume,
    originals: &[i16],
    current: Vec<i16>,
    ranges: &[RawRange],
    curve: VolumeCurve,
    mut write: impl FnMut(usize, i16) -> Result<(), String>,
    mut read_back: impl FnMut() -> Result<Vec<i16>, String>,
) -> Result<(StartupVolumeReport, bool), String> {
    let (targets, clamped_to_minimum, max_db) = match startup {
        StartupVolume::Keep => (current.clone(), false, None),
        StartupVolume::Attenuate { max_db } => {
            if matches!(curve,VolumeCurve::Decibels { .. }) {
                let plan = plan_startup_attenuation(&current,ranges,max_db);
                (single_level(plan.targets),plan.clamped_to_minimum,Some(max_db))
            } else {
                let min = volume_bounds(ranges).0;
                let ceiling = curve.floor_db(max_db,ranges);
                let targets = current.iter().map(|raw| (*raw).min(ceiling.unwrap_or(min))).collect();
                (single_level(targets), ceiling.is_none(), Some(max_db))
            }
        }
    };
    let mut lowered = false;
    for (index, (&target, &now)) in targets.iter().zip(&current).enumerate() {
        if target != now {
            write(index, target)?;
            lowered = true;
        }
    }
    if lowered {
        let after = read_back()?;
        if after != targets {
            return Err(format!(
                "the attenuation could not be verified: wrote {targets:?}, the DAC reads {after:?}"
            ));
        }
    }
    let db = |raws: &[i16]| raws.iter().map(|&raw| curve.db(raw,ranges)).collect::<Vec<_>>();
    let original_db = db(originals);
    let limit_db = original_db
        .iter()
        .copied()
        .fold(f64::INFINITY, f64::min);
    let report = StartupVolumeReport {
        attenuated: targets.iter().zip(originals).any(|(&applied, &original)| applied < original),
        limit_db: if limit_db.is_finite() { limit_db } else { db(&targets).first().copied().unwrap_or(0.0) },
        original_db,
        applied_db: db(&targets),
        clamped_to_minimum,
        max_db,
    };
    Ok((report, lowered))
}

/// What the start-up step did for the route in use now.
pub(crate) fn startup_report() -> Option<StartupVolumeReport> {
    let guard = SESSION_STATE.lock();
    let state = guard.as_ref()?;
    let control = state.hardware_volume_control.as_ref()?;
    state
        .startup_volume
        .as_ref()
        .filter(|status| status.route.same_route(control))
        .map(|status| status.report.clone())
}

/// Whether the start-up step lowered the level while this DAC was opened, with
/// the level it left.
pub(crate) fn startup_lowered_volume() -> Option<VolumeState> {
    let guard = SESSION_STATE.lock();
    let state = guard.as_ref()?;
    state.startup_volume.as_ref().filter(|status| status.lowered)?;
    state.hardware_volume_state
}

/// `Err(VolumeUnavailable)` when a hardware volume control is required and the
/// DAC has no usable one.
pub(crate) fn ensure_volume_if_required(required: bool) -> Result<(), crate::Error> {
    if !required || has_hardware_volume() {
        return Ok(());
    }
    let reason = SESSION_STATE
        .lock()
        .as_ref()
        .and_then(|state| state.hardware_volume_error.clone())
        .unwrap_or_else(|| "no volume control was found".to_string());
    log_warn!("[USB] No usable hardware volume and one is required: {reason}");
    Err(crate::Error::VolumeUnavailable)
}
