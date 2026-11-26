//! Source-proven Pioneer and Roland fixed-rate PCM. Descriptor-only: no initialization requests.
use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct VendorEndpoint {
    pub(super) address: u8,
    pub(super) interval: u8,
    pub(super) service_us: u32,
    pub(super) max_packet_bytes: usize,
    pub(super) channels: u16,
    pub(super) subslot_bytes: u8,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct VendorPcm {
    pub(super) interface_number: u8,
    pub(super) alt_setting: u8,
    pub(super) input_interface_number: u8,
    pub(super) input_alt_setting: u8,
    pub(super) requires_output_for_capture: bool,
    /// False when the IN pipe carries dummy frames solely for output pacing.
    pub(super) input_carries_pcm: bool,
    pub(super) rate: u32,
    pub(super) speed: Speed,
    pub(super) output: VendorEndpoint,
    pub(super) input: VendorEndpoint,
}

#[derive(Clone, Copy)]
struct PioneerModel {
    rate: u32,
    subslot_bytes: u8,
    output_address: u8,
    input_address: u8,
    outputs: u16,
    inputs: u16,
    input_carries_pcm: bool,
}

// Linux snd-usb-audio quirks-table.h fixes the exact rate, format and routing.
// Keep each model's wire specification independent of descriptor capacities.
const PIONEER_MODELS: &[(u16, PioneerModel)] = &[
    (0x0017, PioneerModel { rate: 48_000, subslot_bytes: 3, output_address: 0x01, input_address: 0x82, outputs: 8, inputs: 8, input_carries_pcm: true }), // DJM-250MK2
    (0x001b, PioneerModel { rate: 48_000, subslot_bytes: 3, output_address: 0x01, input_address: 0x82, outputs: 10, inputs: 12, input_carries_pcm: true }), // DJM-750MK2
    (0x0023, PioneerModel { rate: 44_100, subslot_bytes: 4, output_address: 0x05, input_address: 0x86, outputs: 12, inputs: 10, input_carries_pcm: true }), // DDJ-SX3
    (0x000d, PioneerModel { rate: 44_100, subslot_bytes: 3, output_address: 0x01, input_address: 0x82, outputs: 6, inputs: 4, input_carries_pcm: true }), // DDJ-RR
    (0x001e, PioneerModel { rate: 44_100, subslot_bytes: 3, output_address: 0x01, input_address: 0x82, outputs: 4, inputs: 6, input_carries_pcm: true }), // DDJ-SR2
    (0x0029, PioneerModel { rate: 44_100, subslot_bytes: 3, output_address: 0x01, input_address: 0x82, outputs: 6, inputs: 6, input_carries_pcm: true }), // DDJ-800
    (0x000e, PioneerModel { rate: 44_100, subslot_bytes: 3, output_address: 0x01, input_address: 0x82, outputs: 4, inputs: 2, input_carries_pcm: false }), // DDJ-RB: dummy IN is implicit feedback only
];

fn pioneer_model(vendor: u16, product: u16) -> Option<PioneerModel> {
    if vendor != 0x2b73 { return None; }
    PIONEER_MODELS.iter().find_map(|(id, model)| (*id == product).then_some(*model))
}

fn roland_channels(vendor: u16, product: u16) -> Option<(u16, u16)> {
    match (vendor, product) {
        (0x0582, 0x0120) => Some((10, 12)),
        (0x0582, 0x012f) => Some((4, 6)),
        _ => None,
    }
}

#[derive(Clone, Copy)]
struct EndpointShape { address: u8, iso: bool, asynchronous: bool, usage: UsageType, interval: u8, packet: u16 }
struct AlternateShape { interface: u8, number: u8, class: u8, subclass: u8, protocol: u8, endpoints: Vec<EndpointShape> }

fn prove(vendor: u16, product: u16, speed: Speed, alts: &[AlternateShape]) -> Result<Option<VendorPcm>, String> {
    if let Some((outputs, inputs)) = roland_channels(vendor, product) {
        return prove_roland(outputs, inputs, speed, alts);
    }
    let Some(model) = pioneer_model(vendor, product) else { return Ok(None); };
    let interface_alts = alts.iter().filter(|a| a.interface == 0).collect::<Vec<_>>();
    // Some configurations expose ordinary AudioControl/AudioStreaming instead.
    // An exact ID alone is insufficient: require positive class evidence and
    // never hide a partial vendor layout on the intended streaming interface.
    if !interface_alts.is_empty() && interface_alts.iter().all(|a| a.class == USB_CLASS_AUDIO)
        && alts.iter().any(|a| a.class == USB_CLASS_AUDIO && a.subclass == USB_SUBCLASS_AUDIOSTREAMING
            && a.endpoints.iter().any(|e| e.iso && matches!(e.usage, UsageType::Data | UsageType::FeedbackData))) {
        return Ok(None);
    }
    let alts = interface_alts;
    let invalid = || "Pioneer vendor PCM descriptor geometry does not match the proven profile".to_string();
    if speed != Speed::High { return Err(invalid()); }
    let mut zero_alts = alts.iter().filter(|a| a.number == 0);
    let zero = zero_alts.next().ok_or_else(invalid)?;
    let mut active_alts = alts.iter().filter(|a| a.number == 1);
    let active = active_alts.next().ok_or_else(invalid)?;
    if zero_alts.next().is_some() || active_alts.next().is_some() { return Err(invalid()); }
    if zero.class != 0xff || active.class != 0xff || !zero.endpoints.is_empty() || active.endpoints.len() != 2 { return Err(invalid()); }
    let endpoint = |e: EndpointShape, address, channels| -> Result<VendorEndpoint, String> {
        if e.address != address || !e.iso || !e.asynchronous
            || !matches!(e.usage, UsageType::Data | UsageType::FeedbackData)
            || !(1..=16).contains(&e.interval) || !valid_implicit_packet_capacity(e.packet, speed)
            || e.packet & 0x07ff > 1024 { return Err(invalid()); }
        let service_us = service_interval_micros(speed, e.interval);
        let max_packet_bytes = effective_iso_packet_bytes(e.packet, speed);
        let stride = usize::from(channels) * usize::from(model.subslot_bytes);
        if max_packet_bytes < required_max_packet_bytes(model.rate, service_us, stride) { return Err(invalid()); }
        Ok(VendorEndpoint { address, interval: e.interval, service_us, max_packet_bytes, channels, subslot_bytes: model.subslot_bytes })
    };
    let output = endpoint(active.endpoints[0], model.output_address, model.outputs)?;
    let input = endpoint(active.endpoints[1], model.input_address, model.inputs)?;
    if output.service_us != input.service_us { return Err(invalid()); }
    Ok(Some(VendorPcm { interface_number: 0, alt_setting: 1, input_interface_number: 0, input_alt_setting: 1, requires_output_for_capture: false, input_carries_pcm: model.input_carries_pcm, rate: model.rate, speed, output, input }))
}

// Linux snd-usb-audio quirks-table.h fixes these exact models to S32_LE at
// 44.1 kHz. implicit.c requires adjacent vendor streaming interfaces whose
// active subclass OR protocol is 2, and starts playback before capture.
fn prove_roland(outputs: u16, inputs: u16, speed: Speed, alts: &[AlternateShape]) -> Result<Option<VendorPcm>, String> {
    let invalid = || "Roland vendor PCM descriptor geometry does not match the proven profile".to_string();
    // These IDs can also expose an ordinary class-compliant configuration.
    // Defer only when both intended audio interfaces positively identify it;
    // never turn a partial or malformed vendor layout into a generic fallback.
    let standard_audio = [0, 1].iter().all(|interface| {
        let relevant = alts.iter().filter(|a| a.interface == *interface).collect::<Vec<_>>();
        !relevant.is_empty() && relevant.iter().all(|a| a.class == USB_CLASS_AUDIO)
    }) && alts.iter().any(|a| a.class == USB_CLASS_AUDIO
        && a.subclass == USB_SUBCLASS_AUDIOSTREAMING
        && a.endpoints.iter().any(|e| e.iso && matches!(e.usage, UsageType::Data | UsageType::FeedbackData)));
    if standard_audio { return Ok(None); }
    if speed != Speed::High { return Err(invalid()); }
    let endpoint = |interface, address, channels| -> Result<VendorEndpoint, String> {
        let mut zero_alts = alts.iter().filter(|a| a.interface == interface && a.number == 0);
        let zero = zero_alts.next().ok_or_else(invalid)?;
        let mut active_alts = alts.iter().filter(|a| a.interface == interface && a.number == 1);
        let active = active_alts.next().ok_or_else(invalid)?;
        if zero_alts.next().is_some() || active_alts.next().is_some()
            || zero.class != 0xff || !zero.endpoints.is_empty()
            || active.class != 0xff || (active.subclass != 2 && active.protocol != 2)
            || active.endpoints.len() != 1 { return Err(invalid()); }
        let e = active.endpoints[0];
        // ALSA proves the raw asynchronous ISO IN pipe directly, then supplies
        // its 0x25 implicit-feedback usage in the fixed format table. Both raw
        // data usages are valid; a dedicated feedback endpoint is never PCM.
        let data_usage = match address {
            0x85 => matches!(e.usage, UsageType::Data | UsageType::FeedbackData),
            _ => e.usage == UsageType::Data,
        };
        if e.address != address || !e.iso || !e.asynchronous || !data_usage
            || !(1..=16).contains(&e.interval) || !valid_implicit_packet_capacity(e.packet, speed)
            || e.packet & 0x07ff > 1024 { return Err(invalid()); }
        let service_us = service_interval_micros(speed, e.interval);
        let max_packet_bytes = effective_iso_packet_bytes(e.packet, speed);
        if max_packet_bytes < required_max_packet_bytes(44_100, service_us, usize::from(channels) * 4) {
            return Err(invalid());
        }
        Ok(VendorEndpoint { address, interval: e.interval, service_us, max_packet_bytes, channels, subslot_bytes: 4 })
    };
    let output = endpoint(0, 0x05, outputs)?;
    let input = endpoint(1, 0x85, inputs)?;
    if output.service_us != input.service_us { return Err(invalid()); }
    Ok(Some(VendorPcm { interface_number: 0, alt_setting: 1, input_interface_number: 1,
        input_alt_setting: 1, requires_output_for_capture: true, input_carries_pcm: true, rate: 44_100, speed, output, input }))
}

pub(super) fn discover(handle: &DeviceHandle<Context>, config: &ConfigDescriptor, speed: Speed) -> Result<Option<VendorPcm>, String> {
    let identity = handle.device().device_descriptor().map_err(|e| e.to_string())?;
    if pioneer_model(identity.vendor_id(), identity.product_id()).is_none()
        && roland_channels(identity.vendor_id(), identity.product_id()).is_none() { return Ok(None); }
    let alts = config.interfaces().flat_map(|i| i.descriptors().map(|d| AlternateShape {
        interface: d.interface_number(), number: d.setting_number(), class: d.class_code(),
        subclass: d.sub_class_code(), protocol: d.protocol_code(), endpoints: d.endpoint_descriptors().map(|e| EndpointShape {
            address: e.address(), iso: e.transfer_type() == TransferType::Isochronous,
            asynchronous: e.sync_type() == SyncType::Asynchronous, usage: e.usage_type(), interval: e.interval(), packet: e.max_packet_size(),
        }).collect(),
    }).collect::<Vec<_>>()).collect::<Vec<_>>();
    prove(identity.vendor_id(), identity.product_id(), speed, &alts)
}

impl VendorPcm {
    pub(super) fn claimed_interfaces(&self) -> Vec<u8> {
        let mut interfaces = vec![self.interface_number];
        if self.input_interface_number != self.interface_number { interfaces.push(self.input_interface_number); }
        interfaces
    }

    pub(super) fn claims_match(&self, claimed: &[u8]) -> bool {
        let expected = self.claimed_interfaces();
        claimed.len() == expected.len() && expected.iter().all(|id| claimed.contains(id))
    }

    pub(super) fn candidate(self) -> IsoStreamCandidate {
        IsoStreamCandidate { interface_number: self.interface_number, alt_setting: self.alt_setting,
            endpoint_address: self.output.address, endpoint_interval: self.output.interval,
            service_interval_us: self.output.service_us, usb_speed: self.speed, max_packet_bytes: self.output.max_packet_bytes,
            sync_type: SyncType::Asynchronous, usage_type: UsageType::Data, format_tag: FORMAT_TAG_PCM,
            uac2_formats: None, native_dsd: None, channels: self.output.channels, subslot_size: self.output.subslot_bytes, bit_resolution: self.output.subslot_bytes * 8,
            sample_rates: vec![self.rate], refresh: 0, synch_address: 0, feedback_endpoint: None,
            implicit_feedback: Some(ImplicitFeedbackSource { interface_number: self.input_interface_number, alt_setting: self.input_alt_setting,
                address: self.input.address, bytes_per_frame: usize::from(self.input.channels) * usize::from(self.input.subslot_bytes),
                service_interval_us: self.input.service_us, max_packet_bytes: self.input.max_packet_bytes }),
            terminal_link: None, is_uac1: false }
    }
}

pub(super) fn for_candidate(handle: &DeviceHandle<Context>, candidate: &IsoStreamCandidate) -> Result<Option<VendorPcm>, String> {
    let device = handle.device();
    let config = device.active_config_descriptor().map_err(|e| e.to_string())?;
    let Some(profile) = discover(handle, &config, resolve_device_speed(&device))? else { return Ok(None); };
    validate_candidate(profile, candidate)?;
    Ok(Some(profile))
}

fn validate_candidate(profile: VendorPcm, candidate: &IsoStreamCandidate) -> Result<(), String> {
    let expected = profile.candidate();
    if candidate.interface_number != expected.interface_number || candidate.alt_setting != expected.alt_setting
        || candidate.endpoint_address != expected.endpoint_address || candidate.endpoint_interval != expected.endpoint_interval
        || candidate.service_interval_us != expected.service_interval_us || candidate.usb_speed != expected.usb_speed
        || candidate.max_packet_bytes != expected.max_packet_bytes || candidate.sync_type != expected.sync_type
        || !matches!(candidate.usage_type, UsageType::Data | UsageType::FeedbackData)
        || candidate.format_tag != FORMAT_TAG_PCM || candidate.channels != expected.channels || candidate.subslot_size != expected.subslot_size
        || candidate.bit_resolution != expected.bit_resolution || candidate.sample_rates != [profile.rate] || candidate.native_dsd.is_some()
        || candidate.feedback_endpoint.is_some() || candidate.implicit_feedback != expected.implicit_feedback
        || candidate.terminal_link.is_some() || candidate.is_uac1 || candidate.uac2_formats.is_some()
        || candidate.refresh != 0 || candidate.synch_address != 0 {
        return Err("vendor PCM candidate changed since descriptor proof".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // Independent fixed wire geometries: seven complete 24-bit frames per
    // microframe, covering nominal six-frame 48 kHz packets with headroom.
    fn geometry(outputs: u16, inputs: u16) -> Vec<AlternateShape> {
        vec![AlternateShape { interface: 0, subclass: 0, protocol: 0, number: 0, class: 0xff, endpoints: vec![] },
            AlternateShape { interface: 0, subclass: 0, protocol: 0, number: 1, class: 0xff, endpoints: vec![
                EndpointShape { address: 1, iso: true, asynchronous: true, usage: UsageType::Data, interval: 1, packet: outputs * 21 },
                EndpointShape { address: 0x82, iso: true, asynchronous: true, usage: UsageType::FeedbackData, interval: 1, packet: inputs * 21 },
            ] }]
    }
    // Independent controller expectations from the pinned ALSA fixed formats.
    const CONTROLLERS: &[(u16, u16, u16, u8, u8, u8, bool)] = &[
        (0x0023, 12, 10, 4, 0x05, 0x86, true),
        (0x000d, 6, 4, 3, 0x01, 0x82, true),
        (0x001e, 4, 6, 3, 0x01, 0x82, true),
        (0x0029, 6, 6, 3, 0x01, 0x82, true),
        (0x000e, 4, 2, 3, 0x01, 0x82, false),
    ];

    fn controller_geometry(outputs: u16, inputs: u16, width: u8, output: u8, input: u8) -> Vec<AlternateShape> {
        let mut alts = geometry(outputs, inputs);
        alts[1].endpoints[0].address = output;
        alts[1].endpoints[1].address = input;
        alts[1].endpoints[0].packet = outputs * u16::from(width) * 7;
        alts[1].endpoints[1].packet = inputs * u16::from(width) * 7;
        alts
    }

    #[test]
    fn fixed_pioneer_controllers_preserve_format_routing_and_pacing() {
        for &(product, outputs, inputs, width, output, input, carries_pcm) in CONTROLLERS {
            let alts = controller_geometry(outputs, inputs, width, output, input);
            let p = prove(0x2b73, product, Speed::High, &alts).unwrap().unwrap();
            assert_eq!((p.rate, p.output.channels, p.input.channels), (44_100, outputs, inputs));
            assert_eq!((p.output.subslot_bytes, p.input.subslot_bytes), (width, width));
            assert_eq!((p.output.address, p.input.address), (output, input));
            assert_eq!((p.interface_number, p.alt_setting, p.input_interface_number, p.input_alt_setting), (0, 1, 0, 1));
            assert_eq!(p.input_carries_pcm, carries_pcm);
            assert!(!p.requires_output_for_capture);
            assert_eq!(p.claimed_interfaces(), vec![0]);
            let c = p.candidate();
            assert_eq!((c.subslot_size, c.bit_resolution, c.sample_rates.as_slice()), (width, width * 8, &[44_100][..]));
            let feedback = c.implicit_feedback.as_ref().unwrap();
            // RB keeps the dummy IN pipe as pacing, even without input PCM.
            assert_eq!((feedback.address, feedback.bytes_per_frame), (input, usize::from(inputs) * usize::from(width)));
            assert!(validate_candidate(p, &c).is_ok());
            for mutation in 0..5 {
                let mut c = p.candidate();
                match mutation {
                    0 => c.sample_rates = vec![48_000],
                    1 => c.subslot_size = if width == 4 { 3 } else { 4 },
                    2 => c.bit_resolution = 16,
                    3 => c.endpoint_address ^= 1,
                    _ => c.implicit_feedback = None,
                }
                assert!(validate_candidate(p, &c).is_err());
            }
            assert!(prove(0x08e4, product, Speed::High, &alts).unwrap().is_none());
        }
        // Neighboring IDs, models requiring rate controls, and multirate models
        // must not inherit a controller's descriptor-only fixed format.
        for product in [0x000c, 0x0010, 0x0012, 0x001d, 0x0022, 0x0028, 0x002a] {
            assert!(prove(0x2b73, product, Speed::High, &geometry(6, 6)).unwrap().is_none());
        }
    }

    #[test]
    fn fixed_pioneer_controllers_reject_partial_and_malformed_layouts() {
        for &(product, outputs, inputs, width, output, input, _) in CONTROLLERS {
            for mutation in 0..19 {
                let mut alts = controller_geometry(outputs, inputs, width, output, input);
                match mutation {
                    0 => alts[0].class = USB_CLASS_AUDIO,
                    1 => alts[1].class = USB_CLASS_AUDIO,
                    2 => alts[1].endpoints.swap(0, 1),
                    3 => alts[1].endpoints[0].address ^= 1,
                    4 => alts[1].endpoints[1].address ^= 1,
                    5 => alts[1].endpoints[0].iso = false,
                    6 => alts[1].endpoints[1].asynchronous = false,
                    7 => alts[1].endpoints[1].usage = UsageType::Feedback,
                    8 => alts[1].endpoints[0].interval = 0,
                    9 => alts[1].endpoints[1].interval = 2,
                    10 => alts[1].endpoints[0].packet = outputs * u16::from(width) * 6 - 1,
                    11 => alts[1].endpoints[1].packet = inputs * u16::from(width) * 6 - 1,
                    12 => alts[1].endpoints[0].packet |= 0x1800,
                    13 => alts[1].endpoints[1].packet = 1025,
                    14 => { let e = alts[1].endpoints[0]; alts[0].endpoints.push(e); },
                    15 => { alts[1].endpoints.pop(); },
                    16 => alts[1].interface = 1,
                    17 => alts[1].number = 2,
                    _ => alts.push(AlternateShape { interface: 0, number: 1, class: 0xff, subclass: 0, protocol: 0, endpoints: vec![] }),
                }
                assert!(prove(0x2b73, product, Speed::High, &alts).is_err(), "product {product:04x}, mutation {mutation}");
            }
            for speed in [Speed::Unknown, Speed::Full, Speed::Super] {
                assert!(prove(0x2b73, product, speed, &controller_geometry(outputs, inputs, width, output, input)).is_err());
            }
        }
    }

    #[test]
    fn standard_pioneer_configuration_defers_only_with_positive_class_evidence() {
        for &(product, outputs, inputs, width, output, input, _) in CONTROLLERS {
            let mut alts = controller_geometry(outputs, inputs, width, output, input);
            for a in &mut alts { a.class = USB_CLASS_AUDIO; a.subclass = USB_SUBCLASS_AUDIOSTREAMING; }
            assert!(prove(0x2b73, product, Speed::Full, &alts).unwrap().is_none());
            alts[0].class = 0xff;
            assert!(prove(0x2b73, product, Speed::High, &alts).is_err());
            alts[0].class = USB_CLASS_AUDIO;
            alts[1].class = 0xff;
            assert!(prove(0x2b73, product, Speed::High, &alts).is_err());
            alts[1].class = USB_CLASS_AUDIO;
            for a in &mut alts { a.endpoints.clear(); }
            assert!(prove(0x2b73, product, Speed::High, &alts).is_err());
        }
        // A standard AudioControl interface 0 can coexist with streams on 1.
        let mut alts = controller_geometry(6, 4, 3, 1, 0x82);
        for a in &mut alts { a.interface = 1; a.class = USB_CLASS_AUDIO; a.subclass = USB_SUBCLASS_AUDIOSTREAMING; }
        alts.push(AlternateShape { interface: 0, number: 0, class: USB_CLASS_AUDIO,
            subclass: USB_SUBCLASS_AUDIOCONTROL, protocol: 0, endpoints: vec![] });
        assert!(prove(0x2b73, 0x000d, Speed::High, &alts).unwrap().is_none());
        alts.push(AlternateShape { interface: 0, number: 1, class: 0xff, subclass: 0, protocol: 0, endpoints: vec![] });
        assert!(prove(0x2b73, 0x000d, Speed::High, &alts).is_err());
    }

    fn roland_geometry(outputs: u16, inputs: u16) -> Vec<AlternateShape> {
        let mut alts = Vec::new();
        for (interface, address, channels, usage) in [(0, 0x05, outputs, UsageType::Data), (1, 0x85, inputs, UsageType::FeedbackData)] {
            alts.push(AlternateShape { interface, number: 0, class: 0xff, subclass: 0, protocol: 0, endpoints: vec![] });
            alts.push(AlternateShape { interface, number: 1, class: 0xff, subclass: 2, protocol: 0, endpoints: vec![
                EndpointShape { address, iso: true, asynchronous: true, usage, interval: 1, packet: channels * 28 },
            ] });
        }
        alts
    }
    #[test]
    fn exact_roland_models_have_separate_s32_pipes() {
        for (product, outputs, inputs) in [(0x0120, 10, 12), (0x012f, 4, 6)] {
            let p = prove(0x0582, product, Speed::High, &roland_geometry(outputs, inputs)).unwrap().unwrap();
            assert_eq!((p.rate, p.output.channels, p.input.channels), (44_100, outputs, inputs));
            assert_eq!((p.output.subslot_bytes, p.input.subslot_bytes), (4, 4));
            assert_eq!((p.interface_number, p.alt_setting, p.output.address), (0, 1, 0x05));
            assert_eq!((p.input_interface_number, p.input_alt_setting, p.input.address), (1, 1, 0x85));
            assert!(p.requires_output_for_capture);
            assert!(p.input_carries_pcm);
            assert_eq!(p.claimed_interfaces(), vec![0, 1]);
            assert!(p.claims_match(&[0, 1]));
            assert!(p.claims_match(&[1, 0]));
            for mismatch in [&[0][..], &[1][..], &[0, 0][..], &[1, 1][..], &[0, 1, 2][..], &[0, 1, 1][..]] {
                assert!(!p.claims_match(mismatch));
            }
            let c = p.candidate();
            assert_eq!((c.subslot_size, c.bit_resolution), (4, 32));
            assert_eq!(c.implicit_feedback.as_ref().unwrap().interface_number, 1);
            assert_eq!(c.implicit_feedback.as_ref().unwrap().bytes_per_frame, usize::from(inputs) * 4);
            assert!(validate_candidate(p, &c).is_ok());
            for mutation in 0..4 {
                let mut c = p.candidate();
                match mutation {
                    0 => c.subslot_size = 3,
                    1 => c.bit_resolution = 24,
                    2 => c.sample_rates = vec![48_000],
                    _ => c.implicit_feedback.as_mut().unwrap().interface_number = 0,
                }
                assert!(validate_candidate(p, &c).is_err());
            }
        }
        for (vendor, product) in [(0x0582, 0x0121), (0x0582, 0x0130), (0x0583, 0x0120)] {
            assert!(prove(vendor, product, Speed::High, &roland_geometry(10, 12)).unwrap().is_none());
        }
    }
    #[test]
    fn roland_proof_rejects_unproven_geometry() {
        for speed in [Speed::Unknown, Speed::Full, Speed::Super] {
            assert!(prove(0x0582, 0x012f, speed, &roland_geometry(4, 6)).is_err());
        }
        for mutation in 0..17 {
            let mut alts = roland_geometry(4, 6);
            match mutation {
                0 => alts[0].class = 1,
                1 => alts[1].class = 1,
                2 => alts[3].subclass = 0,
                3 => alts[3].interface = 2,
                4 => alts[1].number = 2,
                5 => alts.push(AlternateShape { interface: 1, number: 0, class: 0xff, subclass: 0, protocol: 0, endpoints: vec![] }),
                6 => { let e = alts[1].endpoints[0]; alts[0].endpoints.push(e); },
                7 => alts[1].endpoints[0].address = 1,
                8 => alts[3].endpoints[0].address = 0x82,
                9 => alts[1].endpoints[0].iso = false,
                10 => alts[3].endpoints[0].asynchronous = false,
                11 => alts[3].endpoints[0].usage = UsageType::Feedback,
                12 => alts[1].endpoints[0].interval = 0,
                13 => alts[3].endpoints[0].interval = 2,
                14 => alts[3].endpoints[0].packet = 120,
                15 => alts[1].endpoints[0].packet |= 0x1800,
                _ => { let e = alts[3].endpoints[0]; alts[3].endpoints.push(e); },
            }
            assert!(prove(0x0582, 0x012f, Speed::High, &alts).is_err(), "mutation {mutation}");
        }
    }
    #[test]
    fn standard_roland_configuration_defers_without_hiding_vendor_mismatch() {
        let mut alts = roland_geometry(4, 6);
        for a in &mut alts { a.class = USB_CLASS_AUDIO; a.subclass = USB_SUBCLASS_AUDIOSTREAMING; }
        assert!(prove(0x0582, 0x012f, Speed::High, &alts).unwrap().is_none());
        alts[1].class = 0xff;
        assert!(prove(0x0582, 0x012f, Speed::High, &alts).is_err());
    }
    #[test]
    fn conventional_roland_uac_control_and_streams_defer() {
        let mut alts = roland_geometry(4, 6);
        // A conventional UAC configuration moves the streams to interfaces 1/2
        // and reserves interface 0 alt0 for AudioControl.
        for a in &mut alts {
            a.interface += 1;
            a.class = USB_CLASS_AUDIO;
            a.subclass = USB_SUBCLASS_AUDIOSTREAMING;
        }
        alts.push(AlternateShape { interface: 0, number: 0, class: USB_CLASS_AUDIO,
            subclass: USB_SUBCLASS_AUDIOCONTROL, protocol: 0, endpoints: vec![] });
        assert!(prove(0x0582, 0x0120, Speed::High, &alts).unwrap().is_none());
        for a in &mut alts { a.endpoints.clear(); }
        assert!(prove(0x0582, 0x0120, Speed::High, &alts).is_err());
    }
    #[test]
    fn roland_raw_input_accepts_both_data_usages_only() {
        for usage in [UsageType::Data, UsageType::FeedbackData] {
            let mut alts = roland_geometry(4, 6);
            alts[3].endpoints[0].usage = usage;
            let p = prove(0x0582, 0x012f, Speed::High, &alts).unwrap().unwrap();
            assert_eq!(p.input.address, 0x85);
            assert_eq!(p.input_interface_number, 1);
        }
        let mut alts = roland_geometry(4, 6);
        alts[1].endpoints[0].usage = UsageType::FeedbackData;
        assert!(prove(0x0582, 0x012f, Speed::High, &alts).is_err());
        alts = roland_geometry(4, 6);
        alts[3].endpoints[0].usage = UsageType::Feedback;
        assert!(prove(0x0582, 0x012f, Speed::High, &alts).is_err());
    }
    #[test]
    fn roland_protocol_marker_and_unrelated_interfaces_are_allowed() {
        let mut alts = roland_geometry(4, 6);
        for a in alts.iter_mut().filter(|a| a.number == 1) { a.subclass = 0; a.protocol = 2; }
        alts.push(AlternateShape { interface: 2, number: 0, class: 1, subclass: 3, protocol: 0, endpoints: vec![] });
        alts.push(AlternateShape { interface: 3, number: 1, class: 0xff, subclass: 0, protocol: 0, endpoints: vec![] });
        let p = prove(0x0582, 0x012f, Speed::High, &alts).unwrap().unwrap();
        assert_eq!(p.claimed_interfaces(), vec![0, 1]);
        assert_eq!(p.input.max_packet_bytes, 168);
        alts.push(AlternateShape { interface: 1, number: 1, class: 0xff, subclass: 2, protocol: 0, endpoints: vec![] });
        assert!(prove(0x0582, 0x012f, Speed::High, &alts).is_err());
    }
    #[test]
    fn exact_model_identity_and_asymmetric_format() {
        for (product, outputs, inputs) in [(0x17, 8, 8), (0x1b, 10, 12)] {
            let p = prove(0x2b73, product, Speed::High, &geometry(outputs, inputs)).unwrap().unwrap();
            assert_eq!((p.rate, p.output.channels, p.input.channels, p.output.subslot_bytes), (48_000, outputs, inputs, 3));
            assert_eq!((p.interface_number, p.alt_setting, p.output.address, p.input.address), (0, 1, 1, 0x82));
            assert_eq!(p.claimed_interfaces(), vec![0]);
            assert!(p.claims_match(&[0]));
            assert!(!p.claims_match(&[0, 0]));
            assert!(!p.claims_match(&[0, 1]));
            assert!(!p.requires_output_for_capture);
            assert!(p.input_carries_pcm);
            let c = p.candidate();
            assert!(validate_candidate(p, &c).is_ok());
            assert_eq!(c.implicit_feedback.unwrap().bytes_per_frame, usize::from(inputs) * 3);
        }
        assert!(prove(0x2b73, 0x18, Speed::High, &geometry(8,8)).unwrap().is_none());
        assert!(prove(0x08e4, 0x17, Speed::High, &geometry(8,8)).unwrap().is_none());
    }
    #[test]
    fn rejects_unproven_descriptor_shapes() {
        for speed in [Speed::Unknown, Speed::Full, Speed::Super] {
            assert!(prove(0x2b73, 0x17, speed, &geometry(8,8)).is_err());
        }
        for mutation in 0..15 {
            let mut a = geometry(8,8);
            match mutation {
                0 => a[0].class = 1,
                1 => a[1].class = 1,
                2 => { let e = a[1].endpoints[0]; a[0].endpoints.push(e); },
                3 => a[1].number = 2,
                4 => { a[1].endpoints.swap(0,1); },
                5 => a[1].endpoints[0].address = 2,
                6 => a[1].endpoints[1].address = 0x81,
                7 => a[1].endpoints[0].iso = false,
                8 => a[1].endpoints[1].asynchronous = false,
                9 => a[1].endpoints[1].usage = UsageType::Feedback,
                10 => a[1].endpoints[0].interval = 0,
                11 => a[1].endpoints[1].interval = 2,
                12 => a[1].endpoints[1].packet = 143, // less than six complete frames
                13 => a[1].endpoints[1].interval = 17,
                _ => a[1].endpoints[0].packet |= 0x1800, // reserved transactions
            }
            assert!(prove(0x2b73, 0x17, Speed::High, &a).is_err(), "mutation {mutation}");
        }
    }
    #[test]
    fn packet_capacity_is_an_upper_bound_and_unused_alts_are_allowed() {
        let mut alts = geometry(8, 8);
        alts[1].endpoints[0].packet = 169;
        alts[1].endpoints[1].packet = 170;
        alts.push(AlternateShape { interface: 0, subclass: 0, protocol: 0, number: 2, class: 0xff, endpoints: vec![] });
        let p = prove(0x2b73, 0x17, Speed::High, &alts).unwrap().unwrap();
        assert_eq!((p.output.max_packet_bytes, p.input.max_packet_bytes), (169, 170));
        for number in [0, 1] {
            alts.push(AlternateShape { interface: 0, subclass: 0, protocol: 0, number, class: 0xff, endpoints: vec![] });
            assert!(prove(0x2b73, 0x17, Speed::High, &alts).is_err());
            alts.pop();
        }
    }
    #[test]
    fn fresh_candidate_requires_exact_format_and_feedback() {
        let p = prove(0x2b73, 0x1b, Speed::High, &geometry(10,12)).unwrap().unwrap();
        for mutation in 0..7 {
            let mut c = p.candidate();
            match mutation {
                0 => c.channels = 2,
                1 => c.subslot_size = 4,
                2 => c.bit_resolution = 16,
                3 => c.sample_rates = vec![96_000],
                4 => c.implicit_feedback = None,
                5 => c.max_packet_bytes += 30,
                _ => c.is_uac1 = true,
            }
            assert!(validate_candidate(p, &c).is_err());
        }
    }
}
