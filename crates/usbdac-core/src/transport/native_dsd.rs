//! Independently implemented compatibility data from Linux v7.0 quirks.c,
//! snd_usb_interface_dsd_format_quirks (028ef9c96e96197026887c0f092424679298aae8).
//! Alt-only upstream records require a unique eligible playback layout here.
//! This module issues no vendor mode requests. Source evidence is not a claim
//! that these devices have been physically tested with this transport.
use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum NativeDsdOrigin {
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

// Exact DEVICE_FLG(..., QUIRK_FLAG_DSD_RAW) records in the pinned source.
// Deliberately do not expand vendor-wide entries into speculative product IDs.
fn raw_data_device(id: UsbIdentity) -> bool {
    matches!((id.vendor, id.product),
        (0x0661, 0x0883) // iBasso DC04 Ultra
        | (0x154e, 0x300b) // Marantz SA-KI RUBY / SA-12
        | (0x16d0, 0x0ab1) // PureAudio APA DAC
        | (0x16d0, 0xeca1) // PureAudio Lotus DAC5 family
        | (0x20b1, 0x2009) // DIYINHK USB Audio 2.0
        | (0x21b4, 0x0230) // Ayre QB-9 Twenty
        | (0x21b4, 0x0232) // Ayre QX-5 Twenty
        | (0x262a, 0x9302)) // ddHiFi TC44C
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

fn raw_data_profile(id: UsbIdentity, format: &StreamingInterfaceFormat) -> Option<NativeDsdProfile> {
    if !raw_data_device(id) || !eligible(format)
        || !format.uac2_formats.is_some_and(|mask| mask & 0x8000_0000 != 0) { return None; }
    Some(NativeDsdProfile { word_bytes: 4, big_endian: true, bit_reverse: false,
        origin: NativeDsdOrigin::DeviceCompatibility })
}

// Compatibility evidence only. Nothing outside this module can retrieve this
// recipe, and resolve_native_dsd never consults it. Enable only after a real
// transaction implements alt-zero ownership, control errors, restoration and
// device-loss handling. No readback command is documented by the pinned source.
#[allow(dead_code)]
mod pending_itf {
    use super::*;
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    struct ModelConditions {
        interface_delay: bool,
        force_interface_reset: bool,
    }
    fn model(id: UsbIdentity) -> Option<ModelConditions> {
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
    enum Mode { PcmOrDop, NativeDsd }
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    struct VendorWrite {
        request_type: u8,
        request: u8,
        value: u16,
        index: u16,
        payload_bytes: u16,
    }
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    struct Recipe {
        reset_interface_to_alt_zero: u8,
        delay_after_alt_zero_ms: u16,
        command: VendorWrite,
        delay_after_command_ms: u16,
        restore_pcm: VendorWrite,
        class_control_delay_ms: u16,
        conditions: ModelConditions,
        native_profile: NativeDsdProfile,
    }
    fn recipe(id: UsbIdentity, interface: u8, selected_alt: u8, alternatives: &[u8],
              format: &StreamingInterfaceFormat, mode: Mode) -> Option<Recipe> {
        let conditions = model(id)?;
        // Source mode command addresses wIndex=1. Do not guess a different
        // control recipient on composite devices or remap arbitrary interfaces.
        if interface != 1 || !matches!(alternatives, [0, 1, 2] | [0, 1, 2, 3])
            || alternatives.last().copied() != Some(selected_alt)
            || format.channels != 2 || format.subslot_size != 4
            || format.bit_resolution == 0 || format.bit_resolution > 32 { return None; }
        let restore_pcm = VendorWrite { request_type: 0x41, request: 0,
            value: 0, index: 1, payload_bytes: 0 };
        let command = VendorWrite { value: if mode == Mode::NativeDsd { 1 } else { 0 }, ..restore_pcm };
        Some(Recipe { reset_interface_to_alt_zero: interface, delay_after_alt_zero_ms: 20,
            command, delay_after_command_ms: 20, restore_pcm, class_control_delay_ms: 20,
            conditions, native_profile: NativeDsdProfile { word_bytes: 4, big_endian: true,
                bit_reverse: false, origin: NativeDsdOrigin::DeviceCompatibility } })
    }
    #[cfg(test)]
    mod tests {
        use super::*;
        #[test]
        fn exact_itf_model_shape_and_mode_values_remain_data_only() {
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
    if (format.is_uac1 && playback_designs_layout(id, alt.setting_number()).is_none())
        || alt.setting_number() == 0 || format.channels == 0
        || format.bit_resolution == 0 || format.subslot_size == 0
        || format.bit_resolution > format.subslot_size.saturating_mul(8) { return None; }
    let out_count = |d: &rusb::InterfaceDescriptor| d.endpoint_descriptors().filter(|ep|
        ep.direction() == Direction::Out && ep.transfer_type() == TransferType::Isochronous
        && matches!(ep.usage_type(), UsageType::Data | UsageType::FeedbackData)
        && (1..=16).contains(&ep.interval()) && ep.max_packet_size() & 0x07ff > 0).count();
    if out_count(alt) != 1 { return None; }
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
    if let Some(mut profile) = playback_designs.or_else(|| raw_data_profile(id, format)) {
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
    if !matches!(word_bytes, 1 | 2 | 4) { return None; }
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
        for (width, rate) in [(1, 352_800), (2, 176_400), (4, 88_200)] {
            assert_eq!(native_wire_rate(f, width), Some(rate));
        }
        f.dsd_bit_rate += 1;
        assert_eq!(native_wire_rate(f, 4), None);
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
            (0x16d0, 0x0ab1), (0x16d0, 0xeca1), (0x20b1, 0x2009),
            (0x21b4, 0x0230), (0x21b4, 0x0232), (0x262a, 0x9302)] {
            let id = UsbIdentity { vendor, product, revision: 0x0100 };
            let mut f = format();
            assert!(raw_data_profile(id, &f).is_none());
            f.uac2_formats = Some(0x8000_0001);
            assert_eq!(raw_data_profile(id, &f).unwrap().signature(), (4, true, false));
            // The PCM bit survives; compatibility recognition never changes it.
            assert_eq!(f.format_tag, FORMAT_TAG_PCM);
            assert_eq!(f.uac2_formats, Some(0x8000_0001));
            f.uac2_formats = Some(8);
            assert!(raw_data_profile(id, &f).is_none());
            f.uac2_formats = Some(0x8000_0000); f.subslot_size = 2;
            assert!(raw_data_profile(id, &f).is_none());
            f.subslot_size = 4; f.is_uac1 = true;
            assert!(raw_data_profile(id, &f).is_none());
        }
        let mut raw = format(); raw.uac2_formats = Some(0x8000_0000);
        for (vendor, product) in [(0x0661, 0x0884), (0x154e, 0x300c),
            (0x20b1, 0xffff), (0x21b4, 0x0231), (0x262a, 0x9303)] {
            assert!(raw_data_profile(UsbIdentity { vendor, product, revision: 0 }, &raw).is_none());
        }
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

}
