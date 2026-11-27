//! Independently implemented compatibility data from Linux v7.0 quirks.c,
//! snd_usb_interface_dsd_format_quirks (028ef9c96e96197026887c0f092424679298aae8).
//! Alt-only upstream records require a unique eligible playback layout here.
//! Exact ITF mode transactions run only under the exclusive stream owner.
//! Source evidence is not a claim
//! that these devices have been physically tested with this transport.
use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum NativeDsdOrigin {
    StandardAdc3,
    DeviceCompatibility,
    HostPolicy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct NativeDsdProfile {
    pub(super) word_bytes: u8,
    pub(super) big_endian: bool,
    pub(super) bit_reverse: bool,
    pub(super) origin: NativeDsdOrigin,
}

impl NativeDsdProfile {
    pub(super) fn signature(self) -> (u8, bool, bool) {
        (self.word_bytes, self.big_endian, self.bit_reverse)
    }
}

#[derive(Debug, Clone, Copy)]
struct UsbIdentity {
    vendor: u16,
    product: u16,
    revision: u16,
}

// Facts only: exact product IDs, alternate numbers and word order. No chipset
// matching, product substring guesses, or implicit vendor mode transitions.
fn builtin_layout(id: UsbIdentity) -> Option<(u8, bool)> {
    match (id.vendor, id.product) {
        (0x16d0, 0x071a) => match id.revision {
            0x0199 => Some((2, false)),
            0x019b | 0x0203 => Some((2, true)),
            _ => None,
        },
        (0x16d0, 0x0a23) => Some((2, true)),
        (0x139f, 0x5504) | (0x20b1, 0x3089) | (0x2522, 0x0007)
        | (0x2522, 0x0009) | (0x2522, 0x0012) | (0x2772, 0x0230) => Some((2, true)),
        (0x0d8c, 0x0316) | (0x10cb, 0x0103) | (0x16d0, 0x06b2)
        | (0x16d0, 0x06b4) | (0x16d0, 0x0733) | (0x16d0, 0x09d8)
        | (0x16d0, 0x09db) | (0x16d0, 0x09dd) | (0x16d0, 0x0ab1)
        | (0x16d0, 0xeca1) | (0x1db5, 0x0003) | (0x20a0, 0x4143)
        | (0x22e1, 0xca01) | (0x249c, 0x9326) | (0x2616, 0x0106)
        | (0x2622, 0x0041) | (0x2622, 0x0061) | (0x278b, 0x5100)
        | (0x27f7, 0x3002) | (0x29a2, 0x0086) | (0x6b42, 0x0042) => Some((3, true)),
        _ => None,
    }
}

// DEVICE_FLG and VENDOR_FLG RAW_DATA facts from Linux v7.3 quirks.c,
// c03c1a14507dfaf8b856c2608f9ac216021fc222, lines 2426-2646.
// Upstream stops at the first exact row, even when that row omits RAW_DATA.
// These defaults interpret descriptors; explicit host policy remains separate.
fn raw_data_device(id: UsbIdentity) -> bool {
    match (id.vendor, id.product) {
        (0x152a, 0x880a) // NeuralDSP Quad Cortex
        | (0x2772, 0x0502) // Musical Fidelity M6s DAC
        | (0x2fc6, 0xf06b) // MOONDROP Moonriver2 Ti
        | (0x2fc6, 0xf0b5) // iBasso DC-Elite
        | (0x2fc6, 0xf0b7) => return false, // iBasso DC07 Pro
        (0x0661, 0x0883) // iBasso DC04 Ultra
        | (0x152a, 0x85dd) // SMSL USB DAC
        | (0x154e, 0x300b) // Marantz SA-KI RUBY / SA-12
        | (0x16d0, 0x0ab1) // PureAudio APA DAC
        | (0x16d0, 0xeca1) // PureAudio Lotus DAC5 family
        | (0x20b1, 0x2009) // DIYINHK USB Audio 2.0
        | (0x21b4, 0x0230) // Ayre QB-9 Twenty
        | (0x21b4, 0x0232) // Ayre QX-5 Twenty
        | (0x22e8, 0xca10) // Cambridge Audio streamers
        | (0x262a, 0x9302) // ddHiFi TC44C
        | (0x32bb, 0x0004) => return true, // HiBy FC4
        _ => {}
    }
    // Vendor membership alone neither guesses a chipset nor authorizes writes.
    matches!(id.vendor,
        0x1511 | 0x152a | 0x18d1 | 0x20b1 | 0x21ed | 0x22d9 | 0x23ba
        | 0x25ce | 0x2622 | 0x2772 | 0x278b | 0x292b | 0x2972 | 0x2ab6
        | 0x2afd | 0x2d87 | 0x2fc6 | 0x3336 | 0x3353 | 0x35f4 | 0x3842
        | 0xc502)
}

fn playback_designs_layout(id: UsbIdentity, alt: u8) -> Option<u8> {
    if id.vendor != 0x23ba || id.product >= 0x0110 { return None; }
    match alt {
        // Alt 1 is DSD-over-PCM (DoP), never direct one-bit transport.
        2 => Some(1),
        3 => Some(2),
        _ => None,
    }
}

fn playback_designs_profile(id: UsbIdentity, alt: u8, format: &StreamingInterfaceFormat,
                            eligible_alts: usize) -> Option<NativeDsdProfile> {
    let width = playback_designs_layout(id, alt)?;
    if eligible_alts != 1 || format.channels != 2 || format.subslot_size != width
        || format.bit_resolution == 0 || format.bit_resolution > width * 8 { return None; }
    Some(NativeDsdProfile { word_bytes: width, big_endian: false, bit_reverse: true,
        origin: NativeDsdOrigin::DeviceCompatibility })
}

// Legacy alt 1 has two DSD payload bytes plus one DoP marker on the wire.
// Linux quirks.c:1988-2000, endpoint.c:1111-1121 and pcm.c's DoP writer.
fn playback_designs_dop_profile(id: UsbIdentity, alt: u8,
                                format: &StreamingInterfaceFormat, count: usize) -> bool {
    id.vendor == 0x23ba && id.product < 0x0110 && alt == 1 && count == 1
        && format.channels == 2
        && matches!((format.subslot_size, format.bit_resolution), (2, 16) | (3, 24))
}

pub(super) fn resolve_playback_designs_dop(device: &Device<Context>, config: &ConfigDescriptor,
                                          alt: &rusb::InterfaceDescriptor,
                                          format: &StreamingInterfaceFormat) -> bool {
    let Some(id) = identity(device) else { return false; };
    if !playback_designs_dop_profile(id, alt.setting_number(), format, 1) { return false; }
    let eligible = |d: &rusb::InterfaceDescriptor| {
        if d.class_code() != USB_CLASS_AUDIO || d.sub_class_code() != USB_SUBCLASS_AUDIOSTREAMING
            || d.setting_number() != 1 || !matches!(d.protocol_code(), 0 | 0x20) { return false; }
        let Some(f) = parse_android_streaming_interface_format(d) else { return false; };
        if !playback_designs_dop_profile(id, 1, &f, 1) { return false; }
        let mut outs = 0;
        for ep in d.endpoint_descriptors() {
            if ep.direction() == Direction::Out {
                if ep.transfer_type() != TransferType::Isochronous
                    || !matches!(ep.usage_type(), UsageType::Data | UsageType::FeedbackData)
                    || !(1..=16).contains(&ep.interval())
                    || !itf_packet_geometry(ep.max_packet_size(), device.speed())
                    || ep.max_packet_size() & 0x07ff < 6 { return false; }
                outs += 1;
            }
        }
        outs == 1
    };
    eligible(alt) && config.interfaces().flat_map(|i| i.descriptors()).filter(eligible).count() == 1
}

pub(super) fn playback_designs_dop_capacity(max_bytes: usize, interval_us: u32, rate: u32) -> bool {
    interval_us > 0 && rate > 0 && max_bytes >= required_max_packet_bytes(rate, interval_us, 6)
}

/// Derive a wire carrier only for the explicit DoP request. Descriptor PCM
/// eligibility remains unchanged for independent PCM requests and discovery.
pub(super) fn apply_playback_designs_dop(format: &mut StreamingInterfaceFormat,
                                       playback: PlaybackFormat, resolved: bool) {
    if resolved && playback.is_dop && playback.dsd_transport != DsdTransportMode::Native {
        format.format_tag = FORMAT_TAG_PCM;
        format.subslot_size = 3;
        format.bit_resolution = 24;
        format.native_dsd = None;
    }
}

fn raw_data_profile(id: UsbIdentity, format: &StreamingInterfaceFormat,
                    eligible_alts: usize) -> Option<NativeDsdProfile> {
    // Exact compatibility rules remain authoritative even when their layout
    // or firmware does not match. Legacy Playback Designs alt 1 is DoP only.
    if builtin_layout(id).is_some() || (id.vendor, id.product) == (0x16d0, 0x071a)
        || (id.vendor == 0x23ba && id.product < 0x0110) || itf::model(id).is_some()
        || !raw_data_device(id) || !eligible(format)
        || eligible_alts != 1
        || !format.uac2_formats.is_some_and(|mask| mask & 0x8000_0000 != 0) { return None; }
    Some(NativeDsdProfile { word_bytes: 4, big_endian: true, bit_reverse: false,
        origin: NativeDsdOrigin::DeviceCompatibility })
}

// Exact model data and mockable mode transactions. No mode readback request is
// documented by the pinned source; hand-back therefore restores PCM-safe mode.
mod itf {
    use super::*;
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(super) struct ModelConditions {
        pub(super) interface_delay: bool,
        pub(super) force_interface_reset: bool,
    }
    pub(super) fn model(id: UsbIdentity) -> Option<ModelConditions> {
        let (interface_delay, force_interface_reset) = match (id.vendor, id.product) {
            (0x0644, 0x8043) | (0x0644, 0x804a) => (true, false), // TEAC UD-501/503/NT-503, UD-301
            (0x0644, 0x8044) | (0x0644, 0x806b) | (0x0644, 0x807d)
                | (0x0644, 0x806c) => (true, true), // Esoteric D-05X/XD, TEAC UD-701/507
            (0x154e, 0x1002) | (0x154e, 0x1003) // Denon DCD-1500RE / DA-300USB
                | (0x154e, 0x3005) | (0x154e, 0x3006) // Marantz HD-DAC1 / SA-14S1
                | (0x1852, 0x5062) | (0x1852, 0x5065) // Luxman D-08u / DA-06
                | (0x3255, 0x0000) => (false, false), // Luxman D-10X
            _ => return None,
        };
        Some(ModelConditions { interface_delay, force_interface_reset })
    }
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(super) enum Mode { PcmOrDop, NativeDsd }
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(super) struct VendorWrite {
        pub(super) request_type: u8,
        pub(super) request: u8,
        pub(super) value: u16,
        pub(super) index: u16,
        pub(super) payload_bytes: u16,
    }
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(super) struct Recipe {
        pub(super) reset_interface_to_alt_zero: u8,
        pub(super) delay_after_alt_zero_ms: u16,
        pub(super) command: VendorWrite,
        pub(super) delay_after_command_ms: u16,
        pub(super) restore_pcm: VendorWrite,
        pub(super) conditions: ModelConditions,
        pub(super) native_profile: NativeDsdProfile,
    }
    pub(super) fn recipe(id: UsbIdentity, interface: u8, selected_alt: u8, alternatives: &[u8],
              format: &StreamingInterfaceFormat, mode: Mode) -> Option<Recipe> {
        let conditions = model(id)?;
        // Source mode command addresses wIndex=1. Do not guess a different
        // control recipient on composite devices or remap arbitrary interfaces.
        if interface != 1 || !matches!(alternatives, [0, 1, 2] | [0, 1, 2, 3])
            || selected_alt == 0 || !alternatives.contains(&selected_alt)
            || format.channels != 2 || !matches!(format.subslot_size, 2 | 4)
            || format.bit_resolution == 0 || format.bit_resolution > format.subslot_size * 8
            || (mode == Mode::NativeDsd && (alternatives.last().copied() != Some(selected_alt)
                || format.subslot_size != 4)) { return None; }
        let restore_pcm = VendorWrite { request_type: 0x41, request: 0,
            value: 0, index: 1, payload_bytes: 0 };
        let command = VendorWrite { value: if mode == Mode::NativeDsd { 1 } else { 0 }, ..restore_pcm };
        Some(Recipe { reset_interface_to_alt_zero: interface, delay_after_alt_zero_ms: 20,
            command, delay_after_command_ms: 20, restore_pcm,
            conditions, native_profile: NativeDsdProfile { word_bytes: 4, big_endian: true,
                bit_reverse: false, origin: NativeDsdOrigin::DeviceCompatibility } })
    }
    #[cfg(test)]
    mod tests {
        use super::*;
        #[test]
        fn exact_itf_model_shape_and_mode_values() {
            let id = UsbIdentity { vendor: 0x0644, product: 0x8044, revision: 0 };
            let f = super::super::tests::format();
            let native = recipe(id, 1, 3, &[0, 1, 2, 3], &f, Mode::NativeDsd).unwrap();
            assert_eq!(native.command.value, 1);
            assert_eq!(native.restore_pcm.value, 0);
            assert_eq!(native.command.index, 1);
            assert!(native.conditions.force_interface_reset);
            assert!(native.conditions.interface_delay);
            let pcm = recipe(id, 1, 2, &[0, 1, 2], &f, Mode::PcmOrDop).unwrap();
            assert_eq!(pcm.command, pcm.restore_pcm);
            assert!(recipe(id, 2, 3, &[0, 1, 2, 3], &f, Mode::NativeDsd).is_none());
            assert!(recipe(id, 1, 2, &[0, 1, 2, 3], &f, Mode::NativeDsd).is_none());
            assert!(recipe(id, 1, 3, &[0, 2, 3], &f, Mode::NativeDsd).is_none());
            assert!(model(UsbIdentity { product: 0x8045, ..id }).is_none());
            // RAW-only Marantz must never inherit the ITF mode recipe.
            assert!(model(UsbIdentity { vendor: 0x154e, product: 0x300b, revision: 0 }).is_none());
        }
    }
}

/// The descriptor plan is read-only. Executing it additionally requires the
/// claimed-handle transaction path, which is never called from discovery.
fn itf_descriptor_recipe(device: &Device<Context>, config: &ConfigDescriptor,
                         descriptor: &rusb::InterfaceDescriptor,
                         format: &StreamingInterfaceFormat, mode: itf::Mode) -> Option<itf::Recipe> {
    let id = identity(device)?;
    itf::model(id)?;
    if descriptor.interface_number() != 1 || descriptor.protocol_code() != 0x20 { return None; }
    let alternatives: Vec<_> = config.interfaces().flat_map(|i| i.descriptors())
        .filter(|d| d.interface_number() == 1).collect();
    let mut numbers: Vec<_> = alternatives.iter().map(|d| d.setting_number()).collect();
    numbers.sort_unstable();
    if !matches!(numbers.as_slice(), [0, 1, 2] | [0, 1, 2, 3]) { return None; }
    for alt in &alternatives {
        if alt.class_code() != USB_CLASS_AUDIO || alt.sub_class_code() != USB_SUBCLASS_AUDIOSTREAMING
            || alt.protocol_code() != 0x20 { return None; }
        if alt.setting_number() == 0 {
            if alt.num_endpoints() != 0 { return None; }
            continue;
        }
        let f = parse_android_streaming_interface_format(alt)?;
        if f.channels != 2 || !matches!(f.subslot_size, 2 | 4)
            || f.bit_resolution == 0 || f.bit_resolution > f.subslot_size * 8 { return None; }
        let mut out = 0;
        for ep in alt.endpoint_descriptors() {
            if ep.transfer_type() != TransferType::Isochronous || !(1..=16).contains(&ep.interval())
                || !itf_packet_geometry(ep.max_packet_size(), device.speed()) { return None; }
            if ep.direction() == Direction::Out && ep.usage_type() == UsageType::Data {
                if usize::from(ep.max_packet_size() & 0x07ff) < usize::from(f.channels) * usize::from(f.subslot_size) { return None; }
                out += 1;
            } else if ep.direction() != Direction::In || ep.usage_type() != UsageType::Feedback {
                return None;
            }
        }
        if out != 1 { return None; }
    }
    itf::recipe(id, descriptor.interface_number(), descriptor.setting_number(), &numbers, format, mode)
}

fn itf_packet_geometry(raw: u16, speed: Speed) -> bool {
    let base = raw & 0x07ff;
    if raw & 0xe000 != 0 || base == 0 || base > 1024 || (raw >> 11) & 3 == 3 { return false; }
    if speed == Speed::Full && (raw & 0x1800 != 0 || base > 1023) { return false; }
    // These pinned mode recipes describe USB 2.0 Audio devices. Unknown is
    // allowed here for Android-wrapped descriptors; selection still performs
    // its existing effective-speed and per-service-interval capacity checks.
    matches!(speed, Speed::Full | Speed::High | Speed::Unknown)
}

fn raw_packet_geometry(raw: u16, speed: Speed) -> bool {
    itf_packet_geometry(raw, speed) && raw & 0x07ff >= 8
}

/// Every operation is explicit so ordering and rollback can be proved without
/// USB hardware. A successful zero-alt is mandatory before every vendor write.
trait ItfModeIo {
    fn zero_alt(&mut self, interface: u8) -> Result<(), String>;
    fn write_mode(&mut self, command: itf::VendorWrite) -> Result<(), String>;
    fn delay(&mut self, milliseconds: u16);
}

fn itf_sequence(io: &mut impl ItfModeIo, recipe: itf::Recipe, restore: bool) -> Result<(), String> {
    io.zero_alt(recipe.reset_interface_to_alt_zero)?;
    if recipe.conditions.interface_delay { io.delay(50); }
    io.delay(recipe.delay_after_alt_zero_ms);
    io.write_mode(if restore { recipe.restore_pcm } else { recipe.command })?;
    io.delay(recipe.delay_after_command_ms);
    Ok(())
}

fn itf_switch(io: &mut impl ItfModeIo, recipe: itf::Recipe) -> Result<(), String> {
    if let Err(error) = itf_sequence(io, recipe, false) {
        // SET_INTERFACE failure means no mode write is permitted. The restore
        // sequence independently proves zero-alt before sending PCM recovery.
        return match itf_sequence(io, recipe, true) {
            Ok(()) => Err(format!("ITF mode transaction failed; PCM restored: {error}")),
            Err(restore) => Err(format!("ITF mode transaction failed ({error}); PCM recovery failed ({restore})")),
        };
    }
    Ok(())
}

struct UsbfsItfIo { fd: RawFd }
impl ItfModeIo for UsbfsItfIo {
    fn zero_alt(&mut self, interface: u8) -> Result<(), String> {
        let setting = UsbfsSetInterface { interface: u32::from(interface), altsetting: 0 };
        if unsafe { libc::ioctl(self.fd, USBDEVFS_SETINTERFACE, &setting) } < 0 {
            return Err(std::io::Error::last_os_error().to_string());
        }
        Ok(())
    }
    fn write_mode(&mut self, command: itf::VendorWrite) -> Result<(), String> {
        let control = UsbfsControlTransfer { request_type: command.request_type, request: command.request,
            value: command.value, index: command.index, length: command.payload_bytes,
            timeout_ms: 1000, data: std::ptr::null_mut() };
        let result = unsafe { libc::ioctl(self.fd, USBDEVFS_CONTROL, &control) };
        if result < 0 { return Err(std::io::Error::last_os_error().to_string()); }
        if result != 0 { return Err("ITF mode command returned unexpected data length".into()); }
        Ok(())
    }
    fn delay(&mut self, milliseconds: u16) {
        thread::sleep(Duration::from_millis(u64::from(milliseconds)));
    }
}

/// Owned before libusb handle fields. Normal teardown calls restore only after
/// OUT/feedback transfers retire. Drop covers failed setup/unwinding and always
/// confirms zero-alt first, so detach/reset failures suppress the vendor write.
pub(super) struct ItfModeLease {
    fd: RawFd,
    pending: Mutex<Option<itf::Recipe>>,
}
impl ItfModeLease {
    pub(super) fn new(fd: RawFd) -> Self { Self { fd, pending: Mutex::new(None) } }
    pub(super) fn disarm(&self) { *self.pending.lock() = None; }
    pub(super) fn restore(&self) -> Result<(), String> {
        let mut pending = self.pending.lock();
        let Some(recipe) = *pending else { return Ok(()); };
        // Consume before I/O: a failed zero-alt or detach must never cause a
        // second, uncontrolled vendor-write attempt from Drop.
        *pending = None;
        itf_sequence(&mut UsbfsItfIo { fd: self.fd }, recipe, true)
    }
}
impl Drop for ItfModeLease {
    fn drop(&mut self) {
        if let Err(error) = self.restore() {
            log_warn!("[USB] ITF PCM recovery failed during hand-back: {error}");
        }
    }
}

/// Called only inside an exclusively owned, transfer-free negotiation/open.
/// Read-only capability and selection paths never reach vendor controls.
pub(super) fn apply_itf_mode(claimed: &ClaimedHandle, candidate: &IsoStreamCandidate,
                             mode: DsdTransportMode) -> Result<(), String> {
    // Standard ADC3 DSD is descriptor-defined and never needs a legacy
    // VID-based ITF mode transaction, even on a matching vendor/product.
    if candidate.native_dsd.is_some_and(|profile| profile.origin == NativeDsdOrigin::StandardAdc3) {
        return Ok(());
    }
    let device = claimed.handle.device();
    let Some(id) = identity(&device) else { return Err("cannot read actual device identity for mode selection".into()); };
    if itf::model(id).is_none() { return Ok(()); }
    if candidate.interface_number != 1 { return Ok(()); }
    if SESSION_STATE.lock().as_ref().is_some_and(|state| state.device.fd == claimed.device_fd && state.stream_active) {
        return Err("ITF mode change refused while a stream is active".into());
    }
    claimed.ensure_interface_claimed(1)?;
    if !claimed.active_interfaces.lock().is_empty() {
        return Err("ITF mode change refused while an alternate remains active".into());
    }
    let config = device.active_config_descriptor().map_err(|e| e.to_string())?;
    let descriptor = config.interfaces().flat_map(|i| i.descriptors()).find(|d|
        d.interface_number() == candidate.interface_number && d.setting_number() == candidate.alt_setting)
        .ok_or_else(|| "ITF selected alternate disappeared".to_string())?;
    let format = parse_android_streaming_interface_format(&descriptor)
        .ok_or_else(|| "ITF selected format is malformed".to_string())?;
    let requested_mode = if mode == DsdTransportMode::Native { itf::Mode::NativeDsd } else { itf::Mode::PcmOrDop };
    let Some(recipe) = itf_descriptor_recipe(&device, &config, &descriptor, &format, requested_mode) else {
        // Unknown/sibling layouts retain the existing PCM path, but do not
        // receive a guessed vendor command or newly enabled native support.
        if mode != DsdTransportMode::Native { return Ok(()); }
        return Err("ITF mode change requires the exact supported interface/alternate layout".into());
    };
    let matching_endpoint = descriptor.endpoint_descriptors().any(|ep| ep.direction() == Direction::Out
        && ep.address() == candidate.endpoint_address && ep.interval() == candidate.endpoint_interval
        && effective_iso_packet_bytes(ep.max_packet_size(), candidate.usb_speed) == candidate.max_packet_bytes);
    if !matching_endpoint { return Err("ITF selected endpoint changed".into()); }
    if mode == DsdTransportMode::Native && candidate.native_dsd.map(|p| p.signature()) != Some(recipe.native_profile.signature()) {
        return Err("ITF native mode requires the proven U32 big-endian wire profile".into());
    }
    log::debug!("[USB] ITF mode transaction interface=1 alt={} native={} forceReset={}",
        candidate.alt_setting, mode == DsdTransportMode::Native, recipe.conditions.force_interface_reset);
    // Every transaction issues SET_INTERFACE, including models requiring a
    // forced re-prepare after stop; no cached-alt fast path can suppress it.
    // Arm before the write: a control error can follow a device-side change.
    *claimed.itf_mode.pending.lock() = Some(recipe);
    match itf_switch(&mut UsbfsItfIo { fd: claimed.device_fd }, recipe) {
        Ok(()) => Ok(()),
        Err(error) => {
            // itf_switch already attempted bounded rollback. Prevent duplicate
            // writes from cleanup/Drop if the device detached during rollback.
            *claimed.itf_mode.pending.lock() = None;
            Err(error)
        }
    }
}

/// Device-specific class-control settle guard used by the clock/control layer.
pub(super) fn itf_class_control_delay(handle: &DeviceHandle<Context>) {
    if identity(&handle.device()).is_some_and(|id| itf::model(id).is_some()) {
        thread::sleep(Duration::from_millis(20));
    }
}

pub(super) fn itf_interface_delay(device: &Device<Context>, interface: u8) {
    if interface == 1 && identity(device).and_then(itf::model).is_some_and(|m| m.interface_delay) {
        thread::sleep(Duration::from_millis(50));
    }
}

fn identity(device: &Device<Context>) -> Option<UsbIdentity> {
    let d = device.device_descriptor().ok()?;
    let v = d.device_version();
    Some(UsbIdentity {
        vendor: d.vendor_id(), product: d.product_id(),
        revision: (u16::from(v.major() / 10) << 12)
            | (u16::from(v.major() % 10) << 8)
            | (u16::from(v.minor()) << 4) | u16::from(v.sub_minor()),
    })
}

fn eligible(format: &StreamingInterfaceFormat) -> bool {
    !format.is_uac1 && format.channels == 2 && format.subslot_size == 4
        && format.bit_resolution > 0 && format.bit_resolution <= 32
}

fn compatibility_profile(id: UsbIdentity, alt: u8, format: &StreamingInterfaceFormat,
                         eligible_alts: usize) -> Option<NativeDsdProfile> {
    let (target_alt, big_endian) = builtin_layout(id)?;
    if alt != target_alt || !eligible(format) || eligible_alts != 1 { return None; }
    Some(NativeDsdProfile { word_bytes: 4, big_endian, bit_reverse: false,
        origin: NativeDsdOrigin::DeviceCompatibility })
}

/// Resolve once per alternate, identically for capability reporting and selection.
/// Standard RAW_DATA (bit 31) alone is deliberately insufficient evidence.
pub(super) fn resolve_native_dsd(device: &Device<Context>, config: &ConfigDescriptor,
                                alt: &rusb::InterfaceDescriptor,
                                format: &StreamingInterfaceFormat) -> Option<NativeDsdProfile> {
    let id = identity(device)?;
    if itf::model(id).is_some() {
        // A host hint cannot bypass this family's required mode protocol.
        return itf_descriptor_recipe(device, config, alt, format, itf::Mode::NativeDsd)
            .map(|recipe| recipe.native_profile);
    }
    if (format.is_uac1 && playback_designs_layout(id, alt.setting_number()).is_none())
        || alt.setting_number() == 0 || format.channels == 0
        || format.bit_resolution == 0 || format.subslot_size == 0
        || format.bit_resolution > format.subslot_size.saturating_mul(8) { return None; }
    let out_count = |d: &rusb::InterfaceDescriptor| d.endpoint_descriptors().filter(|ep|
        ep.direction() == Direction::Out && ep.transfer_type() == TransferType::Isochronous
        && matches!(ep.usage_type(), UsageType::Data | UsageType::FeedbackData)
        && (1..=16).contains(&ep.interval()) && ep.max_packet_size() & 0x07ff > 0).count();
    if out_count(alt) != 1 { return None; }
    // A host native hint cannot reinterpret this source-backed DoP alternate.
    if id.vendor == 0x23ba && id.product < 0x0110 && alt.setting_number() == 1 {
        return None;
    }
    let settings = settings();
    let playback_designs = playback_designs_layout(id, alt.setting_number()).and_then(|width| {
        let count = config.interfaces().flat_map(|i| i.descriptors()).filter(|d|
            d.class_code() == USB_CLASS_AUDIO && d.sub_class_code() == USB_SUBCLASS_AUDIOSTREAMING
            && d.setting_number() == alt.setting_number() && out_count(d) == 1
            && parse_android_streaming_interface_format(d).is_some_and(|f|
                f.channels == 2 && f.subslot_size == width && f.bit_resolution > 0
                && f.bit_resolution <= width * 8)).count();
        playback_designs_profile(id, alt.setting_number(), format, count)
    });
    if let Some(mut profile) = playback_designs {
        if let Some((width, _, _)) = settings.native_dsd_policy(id.vendor, id.product, profile.word_bytes) {
            if !matches!(width, 1 | 2 | 4) { return None; }
            profile.word_bytes = width;
            profile.origin = NativeDsdOrigin::HostPolicy;
        }
        let (big_endian, bit_reverse) = settings.native_dsd_order(id.vendor, id.product, profile.big_endian);
        profile.big_endian = big_endian;
        profile.bit_reverse |= bit_reverse;
        return Some(profile);
    }
    if let Some((target_alt, _)) = builtin_layout(id) {
        let count = config.interfaces().flat_map(|i| i.descriptors()).filter(|d|
            d.class_code() == USB_CLASS_AUDIO && d.sub_class_code() == USB_SUBCLASS_AUDIOSTREAMING
            && d.setting_number() == target_alt && out_count(d) == 1
            && parse_android_streaming_interface_format(d).is_some_and(|f| eligible(&f))).count();
        if let Some(mut profile) = compatibility_profile(id, alt.setting_number(), format, count) {
            if let Some((width, _, _)) = settings.native_dsd_policy(id.vendor, id.product, format.subslot_size) {
                if !matches!(width, 1 | 2 | 4) { return None; }
                profile.word_bytes = width;
                profile.origin = NativeDsdOrigin::HostPolicy;
            }
            (profile.big_endian, profile.bit_reverse) = settings.native_dsd_order(id.vendor, id.product, profile.big_endian);
            return Some(profile);
        }
    }
    // A vendor RAW_DATA rule supplies no interface identity. Require a unique
    // UAC2 playback layout at this alternate and capacity for a whole stereo
    // U32 frame. Maximum packet size is a ceiling, not a frame-length promise.
    let raw_out_count = |d: &rusb::InterfaceDescriptor| d.endpoint_descriptors().filter(|ep|
        ep.direction() == Direction::Out && ep.transfer_type() == TransferType::Isochronous
        && matches!(ep.usage_type(), UsageType::Data | UsageType::FeedbackData)
        && (1..=16).contains(&ep.interval())
        && raw_packet_geometry(ep.max_packet_size(), device.speed())).count();
    if alt.protocol_code() == 0x20 && raw_out_count(alt) == 1 {
        let count = config.interfaces().flat_map(|i| i.descriptors()).filter(|d|
            d.class_code() == USB_CLASS_AUDIO && d.sub_class_code() == USB_SUBCLASS_AUDIOSTREAMING
            && d.protocol_code() == 0x20 && d.setting_number() == alt.setting_number()
            && raw_out_count(d) == 1
            && parse_android_streaming_interface_format(d).is_some_and(|f|
                raw_data_profile(id, &f, 1).is_some())).count();
        if let Some(mut profile) = raw_data_profile(id, format, count) {
            if let Some((width, _, _)) = settings.native_dsd_policy(id.vendor, id.product, profile.word_bytes) {
                if !matches!(width, 1 | 2 | 4) { return None; }
                profile.word_bytes = width;
                profile.origin = NativeDsdOrigin::HostPolicy;
            }
            (profile.big_endian, profile.bit_reverse) = settings.native_dsd_order(id.vendor, id.product, profile.big_endian);
            return Some(profile);
        }
    }
    // Existing Dawn Pro and explicit host policy retain their private dialect.
    // Bit 3 is A-law in standard UAC2; only scoped policy may reinterpret it.
    let (word_bytes, big_endian, bit_reverse) = settings.native_dsd_policy(id.vendor, id.product, format.subslot_size)?;
    let raw_or_private = format.uac2_formats.is_some_and(|bits| bits & (0x8000_0000 | 8) != 0)
        || format.format_tag == FORMAT_TAG_DSD;
    if !raw_or_private || !matches!(word_bytes, 1 | 2 | 4) { return None; }
    Some(NativeDsdProfile { word_bytes, big_endian, bit_reverse,
        origin: NativeDsdOrigin::HostPolicy })
}

pub(super) fn native_wire_rate(format: PlaybackFormat, word_bytes: u8) -> Option<u32> {
    if format.dsd_transport != DsdTransportMode::Native { return Some(format.sample_rate); }
    if !matches!(word_bytes, 1 | 2 | 4 | 8) { return None; }
    let bits = 8 * u32::from(word_bytes);
    (format.dsd_bit_rate > 0 && format.dsd_bit_rate % bits == 0)
        .then_some(format.dsd_bit_rate / bits)
}

/// UAC2 bmFormats is a bitmap, unlike UAC1 wFormatTag. A-law bit 3 is
/// intentionally not the historical private numeric DSD tag.
pub(super) fn uac2_primary_format(bits: u32) -> u16 {
    if bits & 1 != 0 { FORMAT_TAG_PCM }
    else if bits & 2 != 0 { FORMAT_TAG_PCM8 }
    else if bits & 4 != 0 { FORMAT_TAG_IEEE_FLOAT }
    else { 0 }
}

#[cfg(test)]
mod tests {
    use super::*;
    pub(super) fn format() -> StreamingInterfaceFormat {
        StreamingInterfaceFormat { format_tag: FORMAT_TAG_PCM, uac2_formats: Some(1), native_dsd: None,
            channels: 2, subslot_size: 4, bit_resolution: 32, terminal_link: None,
            is_uac1: false, sample_rate_ranges: vec![] }
    }
    #[test]
    fn v73_raw_exact_rows_override_vendor_defaults() {
        let mut raw = format(); raw.uac2_formats = Some(0x8000_0000);
        for (vendor, product) in [(0x22e8, 0xca10), (0x32bb, 0x0004), (0x152a, 0x85dd)] {
            let id = UsbIdentity { vendor, product, revision: 0 };
            assert!(raw_data_device(id));
            assert_eq!(raw_data_profile(id, &raw, 1).unwrap().signature(), (4, true, false));
            let mut pcm = raw.clone(); pcm.uac2_formats = Some(1);
            assert!(raw_data_profile(id, &pcm, 1).is_none());
        }
        for (vendor, product) in [(0x152a, 0x880a), (0x2772, 0x0502),
            (0x2fc6, 0xf06b), (0x2fc6, 0xf0b5), (0x2fc6, 0xf0b7)] {
            let id = UsbIdentity { vendor, product, revision: 0 };
            assert!(!raw_data_device(id));
            assert!(raw_data_profile(id, &raw, 1).is_none());
            // The exact veto must not disable an otherwise admitted sibling.
            assert!(raw_data_device(UsbIdentity { product: 0xffff, ..id }));
        }
        for (vendor, product) in [(0x22e8, 0xca11), (0x32bb, 0x0005)] {
            assert!(!raw_data_device(UsbIdentity { vendor, product, revision: 0 }));
        }
    }
    #[test]
    fn legacy_dop_is_exact_and_keeps_pcm_and_native_independent() {
        let id = UsbIdentity { vendor: 0x23ba, product: 0x010f, revision: 0 };
        let mut f = format();
        f.format_tag = 0; f.uac2_formats = Some(0x8000_0000);
        f.subslot_size = 2; f.bit_resolution = 16;
        assert!(playback_designs_dop_profile(id, 1, &f, 1));
        for bad in [UsbIdentity { product: 0x0110, ..id }, UsbIdentity { vendor: 0x20b1, ..id }] {
            assert!(!playback_designs_dop_profile(bad, 1, &f, 1));
        }
        assert!(!playback_designs_dop_profile(id, 2, &f, 1));
        assert!(!playback_designs_dop_profile(id, 1, &f, 2));
        assert!(playback_designs_profile(id, 1, &f, 1).is_none());
        let mut wrong = f.clone(); wrong.subslot_size = 4; wrong.bit_resolution = 32;
        assert!(!playback_designs_dop_profile(id, 1, &wrong, 1));
        wrong = f.clone(); wrong.channels = 4;
        assert!(!playback_designs_dop_profile(id, 1, &wrong, 1));
        wrong = f.clone(); wrong.bit_resolution = 0;
        assert!(!playback_designs_dop_profile(id, 1, &wrong, 1));
        let mut carrier = f.clone(); carrier.subslot_size = 3; carrier.bit_resolution = 24;
        assert!(playback_designs_dop_profile(id, 1, &carrier, 1));
        let dop = PlaybackFormat { sample_rate:176_400, bit_depth:24, channels:2,
            is_dop:true, dsd_transport:DsdTransportMode::DoP, dsd_bit_rate:2_822_400 };
        let mut wire = f.clone();
        apply_playback_designs_dop(&mut wire, dop, true);
        assert_eq!((wire.subslot_size, wire.bit_resolution), (3, 24));
        assert_eq!(transport_compatibility_penalty(&wire, dop), 0);
        assert_eq!(native_wire_rate(dop, wire.subslot_size), Some(176_400));
        assert_eq!(transport_compatibility_penalty(&wire,
            PlaybackFormat { bit_depth:32, ..dop }), u32::MAX);
        assert_eq!(required_max_packet_bytes(176_400, 125, 6), 138);
        assert!(!playback_designs_dop_capacity(137, 125, 176_400));
        assert!(playback_designs_dop_capacity(138, 125, 176_400));
        apply_playback_designs_dop(&mut f, PlaybackFormat { is_dop:false, ..dop }, true);
        assert_eq!(f.format_tag, 0);
        f.format_tag = FORMAT_TAG_PCM;
        assert_eq!(transport_compatibility_penalty(&f,
            PlaybackFormat { is_dop:false, bit_depth:16, ..dop }), 0);
    }
    #[test]
    fn legacy_dop_uses_existing_marker_phase_and_three_byte_encoder() {
        let mut ring = PcmRing::new(8, 2);
        assert_eq!(ring.push_samples(&[0xfa112200u32 as i32, 0xfa334400u32 as i32]), 2);
        let mut words = [0; 4];
        assert!(ring.pop_into_or_pad(&mut words, 2, DsdTransportMode::DoP));
        let mut bytes = [0; 12];
        encode_usb_pcm_slots(&words, &mut bytes, 3, 24, DsdTransportMode::DoP,
            2, false, false).unwrap();
        assert_eq!(bytes, [0x22,0x11,0x05,0x44,0x33,0x05,
            0x69,0x69,0xfa,0x69,0x69,0xfa]);
        // A later USB batch continues phase from the shared ring owner.
        assert_eq!(ring.push_samples(&[0x05556600, 0x05778800]), 2);
        let mut resumed = [0; 2];
        assert!(!ring.pop_into_or_pad(&mut resumed, 2, DsdTransportMode::DoP));
        let mut next = [0; 6];
        encode_usb_pcm_slots(&resumed, &mut next, 3, 24, DsdTransportMode::DoP,
            2, false, false).unwrap();
        assert_eq!(next, [0x66,0x55,0x05,0x88,0x77,0x05]);
    }
    #[test]
    fn amanero_revisions_are_exact_and_byte_order_is_firmware_scoped() {
        for (revision, order) in [(0x0199, Some(false)), (0x019b, Some(true)),
                                  (0x0203, Some(true)), (0x019a, None), (0x0204, None)] {
            let id = UsbIdentity { vendor: 0x16d0, product: 0x071a, revision };
            assert_eq!(compatibility_profile(id, 2, &format(), 1).map(|p| p.big_endian), order);
            assert!(compatibility_profile(id, 1, &format(), 1).is_none());
            assert!(compatibility_profile(id, 2, &format(), 2).is_none());
        }
    }
    #[test]
    fn exact_alt_and_geometry_are_required() {
        let id = UsbIdentity { vendor: 0x2622, product: 0x0061, revision: 0 };
        assert!(compatibility_profile(id, 3, &format(), 1).is_some());
        assert!(compatibility_profile(id, 2, &format(), 1).is_none());
        let mut f = format(); f.subslot_size = 2;
        assert!(compatibility_profile(id, 3, &f, 1).is_none());
        f = format(); f.channels = 4;
        assert!(compatibility_profile(id, 3, &f, 1).is_none());
    }
    #[test]
    fn standard_alaw_and_raw_are_never_native_by_themselves() {
        assert_eq!(uac2_primary_format(8), 0);
        assert_eq!(uac2_primary_format(0x8000_0000), 0);
        assert_eq!(uac2_primary_format(0x8000_0001), FORMAT_TAG_PCM);
        assert_eq!(uac2_primary_format(4), FORMAT_TAG_IEEE_FLOAT);
        assert_eq!(uac2_primary_format(3), FORMAT_TAG_PCM);
    }
    #[test]
    fn wire_clock_uses_word_width_and_refuses_fractional_words() {
        let mut f = PlaybackFormat { sample_rate: 88_200, bit_depth: 1, channels: 2,
            is_dop: false, dsd_transport: DsdTransportMode::Native, dsd_bit_rate: 2_822_400 };
        for (width, rate) in [(1, 352_800), (2, 176_400), (4, 88_200), (8, 44_100)] {
            assert_eq!(native_wire_rate(f, width), Some(rate));
        }
        f.dsd_bit_rate += 1;
        assert_eq!(native_wire_rate(f, 4), None);
        assert_eq!(native_wire_rate(f, 8), None);
    }
    #[test]
    fn parser_retains_raw_mask_and_separates_private_short_dialect() {
        let mut descriptors = vec![16, 0x24, 1, 1, 0, 1, 1, 0, 0, 0x80, 2, 0, 0, 0, 0, 0,
                                   6, 0x24, 2, 1, 4, 32];
        let parsed = parse_streaming_extra(0x20, &descriptors).unwrap();
        assert_eq!(parsed.uac2_formats, Some(0x8000_0001));
        assert_eq!(parsed.format_tag, FORMAT_TAG_PCM);
        assert!(parsed.native_dsd.is_none());
        descriptors[6] = 8; descriptors[9] = 0;
        let alaw = parse_streaming_extra(0x20, &descriptors).unwrap();
        assert_eq!(alaw.uac2_formats, Some(8));
        assert_eq!(alaw.format_tag, 0);
        assert!(parse_streaming_extra(0x30, &descriptors).is_none());
        let private = parse_streaming_extra(0x20, &[7, 0x24, 1, 1, 0, 8, 0,
                                                   6, 0x24, 2, 1, 4, 1]).unwrap();
        assert_eq!(private.uac2_formats, None);
        assert_eq!(private.format_tag, FORMAT_TAG_DSD);
    }
    #[test]
    fn native_encoding_orders_words_once_across_transfer_boundaries() {
        let input: Vec<i32> = (1..=16).collect();
        for width in [1u8, 2, 4] {
            for be in [false, true] {
                for reverse in [false, true] {
                    let mut whole = vec![0; input.len()];
                    encode_usb_pcm_slots(&input, &mut whole, width, 1, DsdTransportMode::Native, 2, be, reverse).unwrap();
                    let mut split = vec![0; input.len()];
                    for (src, dst) in input.chunks(8).zip(split.chunks_mut(8)) {
                        encode_usb_pcm_slots(src, dst, width, 1, DsdTransportMode::Native, 2, be, reverse).unwrap();
                    }
                    assert_eq!(whole, split);
                    let w = usize::from(width);
                    let expected: Vec<u8> = input.chunks(w * 2).flat_map(|group| {
                        let mut out = Vec::new();
                        for channel in 0..2 {
                            for wire_byte in 0..w {
                                let source_byte = if be { wire_byte } else { w - 1 - wire_byte };
                                let byte = group[channel + 2 * source_byte] as u8;
                                out.push(if reverse { byte.reverse_bits() } else { byte });
                            }
                        }
                        out
                    }).collect();
                    assert_eq!(whole, expected);
                }
            }
        }
        assert!(encode_usb_pcm_slots(&input[..6], &mut [0; 6], 4, 1, DsdTransportMode::Native, 2, true, false).is_err());
    }
    #[test]
    fn overlapping_pcm_keeps_pcm_and_dop_while_native_requires_profile() {
        let mut f = format();
        let pcm = PlaybackFormat { sample_rate: 176_400, bit_depth: 32, channels: 2,
            is_dop: false, dsd_transport: DsdTransportMode::None, dsd_bit_rate: 0 };
        let native = PlaybackFormat { bit_depth: 1, dsd_transport: DsdTransportMode::Native,
            dsd_bit_rate: 2_822_400, ..pcm };
        assert_eq!(transport_compatibility_penalty(&f, native), u32::MAX);
        f.native_dsd = compatibility_profile(UsbIdentity { vendor: 0x16d0, product: 0x071a,
            revision: 0x0199 }, 2, &f, 1);
        assert_eq!(transport_compatibility_penalty(&f, pcm), 0);
        assert_eq!(transport_compatibility_penalty(&f, PlaybackFormat { is_dop: true,
            dsd_transport: DsdTransportMode::DoP, ..pcm }), 0);
        assert_eq!(transport_compatibility_penalty(&f, native), 0);
    }

    #[test]
    fn exact_raw_data_products_require_standard_raw_mask_and_u32_geometry() {
        for (vendor, product) in [(0x0661, 0x0883), (0x154e, 0x300b),
            (0x20b1, 0x2009),
            (0x21b4, 0x0230), (0x21b4, 0x0232), (0x262a, 0x9302)] {
            let id = UsbIdentity { vendor, product, revision: 0x0100 };
            let mut f = format();
            assert!(raw_data_profile(id, &f, 1).is_none());
            f.uac2_formats = Some(0x8000_0001);
            assert_eq!(raw_data_profile(id, &f, 1).unwrap().signature(), (4, true, false));
            // The PCM bit survives; compatibility recognition never changes it.
            assert_eq!(f.format_tag, FORMAT_TAG_PCM);
            assert_eq!(f.uac2_formats, Some(0x8000_0001));
            f.uac2_formats = Some(8);
            assert!(raw_data_profile(id, &f, 1).is_none());
            f.uac2_formats = Some(0x8000_0000); f.subslot_size = 2;
            assert!(raw_data_profile(id, &f, 1).is_none());
            f.subslot_size = 4; f.is_uac1 = true;
            assert!(raw_data_profile(id, &f, 1).is_none());
        }
        let mut raw = format(); raw.uac2_formats = Some(0x8000_0000);
        for (vendor, product) in [(0x0661, 0x0884), (0x154e, 0x300c),
            (0x21b4, 0x0231), (0x262a, 0x9303)] {
            assert!(raw_data_profile(UsbIdentity { vendor, product, revision: 0 }, &raw, 1).is_none());
        }
    }
    #[test]
    fn vendor_raw_rules_require_real_raw_stereo_u32_and_unique_layout() {
        for vendor in [0x1511, 0x152a, 0x18d1, 0x20b1, 0x21ed, 0x22d9,
            0x23ba, 0x25ce, 0x2622, 0x2772, 0x278b, 0x292b, 0x2972, 0x2ab6,
            0x2afd, 0x2d87, 0x2fc6, 0x3336, 0x3353, 0x35f4, 0x3842, 0xc502] {
            let id = UsbIdentity { vendor, product: 0xffff, revision: 0 };
            let mut f = format();
            for mask in [None, Some(1), Some(8), Some(0x0000_8001)] {
                f.uac2_formats = mask;
                assert!(raw_data_profile(id, &f, 1).is_none());
            }
            f.uac2_formats = Some(0x8000_0001);
            assert_eq!(raw_data_profile(id, &f, 1).unwrap().signature(), (4, true, false));
            assert!(raw_data_profile(id, &f, 0).is_none());
            assert!(raw_data_profile(id, &f, 2).is_none());
            assert_eq!(uac2_primary_format(f.uac2_formats.unwrap()), FORMAT_TAG_PCM);
            for width in [1, 2, 3, 8] {
                f.subslot_size = width;
                assert!(raw_data_profile(id, &f, 1).is_none());
            }
            f.subslot_size = 4;
            for bits in [0, 33] {
                f.bit_resolution = bits;
                assert!(raw_data_profile(id, &f, 1).is_none());
            }
            // RAW describes a U32 container, not 32 significant PCM bits.
            f.bit_resolution = 1;
            assert!(raw_data_profile(id, &f, 1).is_some());
            for channels in [0, 1, 4] {
                f.channels = channels;
                assert!(raw_data_profile(id, &f, 1).is_none());
            }
            f.channels = 2; f.is_uac1 = true;
            assert!(raw_data_profile(id, &f, 1).is_none());
        }
    }
    #[test]
    fn raw_packet_capacity_allows_unused_bytes_but_requires_whole_frame_room() {
        assert!(raw_packet_geometry(9, Speed::High));
        assert!(raw_packet_geometry(1024 | (2 << 11), Speed::High));
        assert!(!raw_packet_geometry(7, Speed::High));
        assert!(!raw_packet_geometry(1025, Speed::High));
        assert!(!raw_packet_geometry(1024 | (3 << 11), Speed::High));
        assert!(!raw_packet_geometry(8 | (1 << 11), Speed::Full));
        assert!(!raw_packet_geometry(8, Speed::Super));
    }
    #[test]
    fn raw_fallback_never_supersedes_firmware_alt_or_mode_rules() {
        let mut f = format(); f.uac2_formats = Some(0x8000_0001);
        for (vendor, product, revision) in [(0x16d0, 0x071a, 0x019a),
            (0x16d0, 0x071a, 0x0199), (0x16d0, 0x0ab1, 0), (0x16d0, 0xeca1, 0),
            (0x20b1, 0x3089, 0), (0x2622, 0x0061, 0), (0x23ba, 0x010f, 0),
            (0x0644, 0x8044, 0)] {
            assert!(raw_data_profile(UsbIdentity { vendor, product, revision }, &f, 1).is_none());
        }
        let exact = UsbIdentity { vendor: 0x20b1, product: 0x3089, revision: 0 };
        assert_eq!(compatibility_profile(exact, 2, &f, 1).unwrap().signature(), (4, true, false));
        assert!(compatibility_profile(exact, 3, &f, 1).is_none());
        let legacy = UsbIdentity { vendor: 0x23ba, product: 0x010f, revision: 0 };
        assert!(playback_designs_profile(legacy, 1, &f, 1).is_none());
        assert!(raw_data_profile(UsbIdentity { vendor: 0x23bb, product: 0xffff, revision: 0 }, &f, 1).is_none());
    }
    #[test]
    fn playback_designs_native_alts_are_width_specific_bit_reversed_and_not_dop() {
        let id = UsbIdentity { vendor: 0x23ba, product: 0x010f, revision: 0x0100 };
        let mut f = format();
        f.is_uac1 = true; f.uac2_formats = None;
        f.subslot_size = 1; f.bit_resolution = 8;
        assert_eq!(playback_designs_profile(id, 2, &f, 1).unwrap().signature(), (1, false, true));
        assert!(playback_designs_profile(id, 1, &f, 1).is_none());
        assert!(playback_designs_profile(id, 3, &f, 1).is_none());
        assert!(playback_designs_profile(id, 2, &f, 2).is_none());
        f.subslot_size = 2; f.bit_resolution = 16;
        assert_eq!(playback_designs_profile(id, 3, &f, 1).unwrap().signature(), (2, false, true));
        assert!(playback_designs_profile(id, 2, &f, 1).is_none());
        assert!(playback_designs_profile(UsbIdentity { product: 0x0110, ..id }, 3, &f, 1).is_none());
        assert!(playback_designs_profile(UsbIdentity { vendor: 0x23bb, ..id }, 3, &f, 1).is_none());
        f.bit_resolution = 0;
        assert!(playback_designs_profile(id, 3, &f, 1).is_none());
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    enum ItfOp { Zero(u8), Write(u16), Delay(u16) }
    #[derive(Default)]
    struct MockItfIo {
        transcript: Vec<ItfOp>,
        fail_zero_calls: Vec<usize>,
        fail_write_calls: Vec<usize>,
        zero_calls: usize,
        write_calls: usize,
    }
    impl ItfModeIo for MockItfIo {
        fn zero_alt(&mut self, interface: u8) -> Result<(), String> {
            self.transcript.push(ItfOp::Zero(interface));
            self.zero_calls += 1;
            if self.fail_zero_calls.contains(&self.zero_calls) { Err("reset/detach".into()) } else { Ok(()) }
        }
        fn write_mode(&mut self, command: itf::VendorWrite) -> Result<(), String> {
            assert_eq!((command.request_type, command.request, command.index, command.payload_bytes), (0x41, 0, 1, 0));
            self.transcript.push(ItfOp::Write(command.value));
            self.write_calls += 1;
            if self.fail_write_calls.contains(&self.write_calls) { Err("control failure".into()) } else { Ok(()) }
        }
        fn delay(&mut self, milliseconds: u16) { self.transcript.push(ItfOp::Delay(milliseconds)); }
    }
    fn teac_recipe(mode: itf::Mode) -> itf::Recipe {
        itf::recipe(UsbIdentity { vendor: 0x0644, product: 0x8044, revision: 0 },
                    1, 3, &[0, 1, 2, 3], &format(), mode).unwrap()
    }
    #[test]
    fn itf_native_then_pcm_has_zero_alt_and_documented_settle_order() {
        let mut io = MockItfIo::default();
        let recipe = teac_recipe(itf::Mode::NativeDsd);
        itf_switch(&mut io, recipe).unwrap();
        assert_eq!(io.transcript, [ItfOp::Zero(1), ItfOp::Delay(50), ItfOp::Delay(20),
            ItfOp::Write(1), ItfOp::Delay(20)]);
        io.transcript.clear();
        itf_sequence(&mut io, recipe, true).unwrap();
        assert_eq!(io.transcript, [ItfOp::Zero(1), ItfOp::Delay(50), ItfOp::Delay(20),
            ItfOp::Write(0), ItfOp::Delay(20)]);
        io.transcript.clear();
        itf_switch(&mut io, teac_recipe(itf::Mode::PcmOrDop)).unwrap();
        assert_eq!(io.transcript[3], ItfOp::Write(0));
    }
    #[test]
    fn failed_itf_native_write_recovers_pcm_and_preserves_original_failure() {
        let mut io = MockItfIo { fail_write_calls: vec![1], ..MockItfIo::default() };
        let error = itf_switch(&mut io, teac_recipe(itf::Mode::NativeDsd)).unwrap_err();
        assert!(error.contains("control failure"));
        assert!(error.contains("PCM restored"));
        assert_eq!(io.transcript, [ItfOp::Zero(1), ItfOp::Delay(50), ItfOp::Delay(20), ItfOp::Write(1),
            ItfOp::Zero(1), ItfOp::Delay(50), ItfOp::Delay(20), ItfOp::Write(0), ItfOp::Delay(20)]);
    }
    #[test]
    fn detach_or_unconfirmed_zero_alt_prevents_vendor_recovery_write() {
        let mut io = MockItfIo { fail_zero_calls: vec![1, 2], ..MockItfIo::default() };
        let error = itf_switch(&mut io, teac_recipe(itf::Mode::NativeDsd)).unwrap_err();
        assert!(error.contains("PCM recovery failed"));
        assert_eq!(io.transcript, [ItfOp::Zero(1), ItfOp::Zero(1)]);
        let mut io = MockItfIo { fail_zero_calls: vec![2], fail_write_calls: vec![1], ..MockItfIo::default() };
        assert!(itf_switch(&mut io, teac_recipe(itf::Mode::NativeDsd)).unwrap_err().contains("PCM recovery failed"));
        assert_eq!(io.write_calls, 1);
        // Failure during hand-back likewise sends no mode command.
        let mut io = MockItfIo { fail_zero_calls: vec![1], ..MockItfIo::default() };
        assert!(itf_sequence(&mut io, teac_recipe(itf::Mode::NativeDsd), true).is_err());
        assert_eq!(io.write_calls, 0);
    }
    #[test]
    fn pcm_and_dop_use_pcm_mode_on_their_real_alternate() {
        let id = UsbIdentity { vendor: 0x154e, product: 0x3005, revision: 0 };
        let mut f = format(); f.subslot_size = 2; f.bit_resolution = 16;
        let recipe = itf::recipe(id, 1, 1, &[0, 1, 2], &f, itf::Mode::PcmOrDop).unwrap();
        let mut io = MockItfIo::default();
        itf_switch(&mut io, recipe).unwrap();
        assert_eq!(io.transcript, [ItfOp::Zero(1), ItfOp::Delay(20), ItfOp::Write(0), ItfOp::Delay(20)]);
        assert!(itf::recipe(id, 1, 1, &[0, 1, 2], &f, itf::Mode::NativeDsd).is_none());
    }

    #[test]
    fn itf_packet_geometry_rejects_reserved_and_impossible_transactions() {
        assert!(itf_packet_geometry(1024, Speed::High));
        assert!(itf_packet_geometry(1024 | (2 << 11), Speed::High));
        assert!(!itf_packet_geometry(1024 | (3 << 11), Speed::High));
        assert!(!itf_packet_geometry(1025, Speed::High));
        assert!(!itf_packet_geometry(0xe008, Speed::High));
        assert!(!itf_packet_geometry(0, Speed::High));
        assert!(!itf_packet_geometry(1024, Speed::Full));
        assert!(!itf_packet_geometry(512 | (1 << 11), Speed::Full));
        assert!(!itf_packet_geometry(8, Speed::Low));
    }

}
