use super::*;
use crate::volume::{raw_to_db, VolumeRange as PublicVolumeRange, VolumeState};
use crate::volume_control::{quantize_db, step_target_raw, HardwareVolumeSnapshot};

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
    if protocol == AudioControlProtocol::Uac2 {
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
                if let Some(control) = build_hardware_volume_control_from_feature_unit(
                    device, topology, feature_unit, channels,
                )? { return Ok(Some(control)); }
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
            Ok(snapshot) => {
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
        db: raw_to_db(snapshot.raw),
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
        min_db: raw_to_db(control.min_volume_raw),
        max_db: raw_to_db(control.max_volume_raw),
        res_db: raw_to_db(control.resolution_raw.max(1)),
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
    let target_raw = quantize_db(db, &control.ranges);
    write_volume_raw_locked(&state, &control, target_raw)
}

pub(crate) fn write_volume_fraction(fraction: f64) -> Result<VolumeState, String> {
    let _hold = HARDWARE_VOLUME_MUTEX.lock();
    let (state, control) = volume_route()?;
    let target_raw = quantize_volume(fraction, &control.ranges);
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
    let target_raw = step_target_raw(snapshot.raw, snapshot.volume, direction, steps, &control.ranges);
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

/// Remembers the level a DAC has the first time its volume control is found
/// during an attachment, which is before this library has written to it. Caller
/// holds HARDWARE_VOLUME_MUTEX.
pub(super) fn note_original_hardware_volume(
    device: &UsbDevice,
    control: &HardwareVolumeControl,
) {
    let key = original_volume_device_key(device);
    if ORIGINAL_HARDWARE_VOLUME.lock().is_recorded(&key) {
        return;
    }
    match read_channel_volumes(device, control).map(|raw| common_channel_volume(&raw)) {
        Ok(Some(raw)) => {
            log_info!("[USB] Original hardware volume of '{}': raw={raw}", device.product_name);
            ORIGINAL_HARDWARE_VOLUME.lock().note(&key, control.clone(), raw);
        }
        Ok(None) => log_warn!("[USB] Original hardware volume not recorded: channels disagree"),
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
    ORIGINAL_VOLUME_ACTION.store(action, Ordering::SeqCst);
    true
}

/// The recorded original level in dB.
pub(crate) fn original_volume_db() -> Option<f64> {
    let device = SESSION_STATE.lock().as_ref().map(|state| state.device.clone())?;
    ORIGINAL_HARDWARE_VOLUME
        .lock()
        .peek(&original_volume_device_key(&device))
        .map(|original| raw_to_db(original.raw))
}

/// Writes the recorded original level back now and ends the record. `Ok(false)`
/// when there is none or the DAC already holds it.
pub(crate) fn hand_back_original_volume() -> Result<bool, String> {
    let _hold = HARDWARE_VOLUME_MUTEX.lock();
    let device = SESSION_STATE
        .lock()
        .as_ref()
        .map(|state| state.device.clone())
        .ok_or_else(|| "no DAC is open".to_string())?;
    let Some(original) = ORIGINAL_HARDWARE_VOLUME
        .lock()
        .take(&original_volume_device_key(&device))
    else {
        return Ok(false);
    };
    restore_original_hardware_volume(&device, &original.control, original.raw)
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
            let Some(original) = ORIGINAL_HARDWARE_VOLUME
                .lock()
                .take(&original_volume_device_key(&device))
            else {
                return;
            };
            match restore_original_hardware_volume(&device, &original.control, original.raw) {
                Ok(true) => log_info!("[USB] Restored original hardware volume: raw={}", original.raw),
                Ok(false) => {}
                Err(error) => log_warn!("[USB] Original hardware volume not restored: {error}"),
            }
        }
        _ => {}
    }
}

/// Writes `raw`, exactly as the DAC reported it, back to every channel and reads
/// it back. Ok(false) when it was already there.
pub(super) fn restore_original_hardware_volume(
    device: &UsbDevice,
    control: &HardwareVolumeControl,
    raw: i16,
) -> Result<bool, String> {
    if read_channel_volumes(device, control)?.iter().all(|&current| current == raw) {
        return Ok(false);
    }
    for &channel in &control.volume_channels {
        write_feature_unit_i16_control(
            device,
            control.interface_number,
            control.feature_unit_id,
            channel,
            UAC2_FEATURE_UNIT_VOLUME_CONTROL,
            raw,
        )?;
    }
    let readback = common_channel_volume(&read_channel_volumes(device, control)?);
    if readback != Some(raw) {
        return Err(format!("read back {readback:?} after writing {raw}"));
    }
    Ok(true)
}

