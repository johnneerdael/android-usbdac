//! Source-proven Pioneer fixed-rate PCM. Descriptor-only: no initialization requests.
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
    pub(super) rate: u32,
    pub(super) speed: Speed,
    pub(super) output: VendorEndpoint,
    pub(super) input: VendorEndpoint,
}

fn channels(vendor: u16, product: u16) -> Option<(u16, u16)> {
    match (vendor, product) {
        (0x2b73, 0x0017) => Some((8, 8)),
        (0x2b73, 0x001b) => Some((10, 12)),
        _ => None,
    }
}

#[derive(Clone, Copy)]
struct EndpointShape { address: u8, iso: bool, asynchronous: bool, usage: UsageType, interval: u8, packet: u16 }
struct AlternateShape { number: u8, class: u8, endpoints: Vec<EndpointShape> }

fn prove(vendor: u16, product: u16, speed: Speed, alts: &[AlternateShape]) -> Result<Option<VendorPcm>, String> {
    let Some((outputs, inputs)) = channels(vendor, product) else { return Ok(None); };
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
        let stride = usize::from(channels) * 3;
        if max_packet_bytes < required_max_packet_bytes(48_000, service_us, stride) { return Err(invalid()); }
        Ok(VendorEndpoint { address, interval: e.interval, service_us, max_packet_bytes, channels, subslot_bytes: 3 })
    };
    let output = endpoint(active.endpoints[0], 0x01, outputs)?;
    let input = endpoint(active.endpoints[1], 0x82, inputs)?;
    if output.service_us != input.service_us { return Err(invalid()); }
    Ok(Some(VendorPcm { interface_number: 0, alt_setting: 1, rate: 48_000, speed, output, input }))
}

pub(super) fn discover(handle: &DeviceHandle<Context>, config: &ConfigDescriptor, speed: Speed) -> Result<Option<VendorPcm>, String> {
    let identity = handle.device().device_descriptor().map_err(|e| e.to_string())?;
    if channels(identity.vendor_id(), identity.product_id()).is_none() { return Ok(None); }
    let alts = config.interfaces().filter(|i| i.number() == 0).flat_map(|i| i.descriptors().map(|d| AlternateShape {
        number: d.setting_number(), class: d.class_code(), endpoints: d.endpoint_descriptors().map(|e| EndpointShape {
            address: e.address(), iso: e.transfer_type() == TransferType::Isochronous,
            asynchronous: e.sync_type() == SyncType::Asynchronous, usage: e.usage_type(), interval: e.interval(), packet: e.max_packet_size(),
        }).collect(),
    }).collect::<Vec<_>>()).collect::<Vec<_>>();
    prove(identity.vendor_id(), identity.product_id(), speed, &alts)
}

impl VendorPcm {
    pub(super) fn candidate(self) -> IsoStreamCandidate {
        IsoStreamCandidate { interface_number: self.interface_number, alt_setting: self.alt_setting,
            endpoint_address: self.output.address, endpoint_interval: self.output.interval,
            service_interval_us: self.output.service_us, usb_speed: self.speed, max_packet_bytes: self.output.max_packet_bytes,
            sync_type: SyncType::Asynchronous, usage_type: UsageType::Data, format_tag: FORMAT_TAG_PCM,
            uac2_formats: None, native_dsd: None, channels: self.output.channels, subslot_size: 3, bit_resolution: 24,
            sample_rates: vec![self.rate], refresh: 0, synch_address: 0, feedback_endpoint: None,
            implicit_feedback: Some(ImplicitFeedbackSource { interface_number: self.interface_number, alt_setting: self.alt_setting,
                address: self.input.address, bytes_per_frame: usize::from(self.input.channels) * 3,
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
        || candidate.format_tag != FORMAT_TAG_PCM || candidate.channels != expected.channels || candidate.subslot_size != 3
        || candidate.bit_resolution != 24 || candidate.sample_rates != [profile.rate] || candidate.native_dsd.is_some()
        || candidate.feedback_endpoint.is_some() || candidate.implicit_feedback != expected.implicit_feedback
        || candidate.terminal_link.is_some() || candidate.is_uac1 || candidate.uac2_formats.is_some()
        || candidate.refresh != 0 || candidate.synch_address != 0 {
        return Err("Pioneer vendor PCM candidate changed since descriptor proof".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // Independent fixed wire geometries: seven complete 24-bit frames per
    // microframe, covering nominal six-frame 48 kHz packets with headroom.
    fn geometry(outputs: u16, inputs: u16) -> Vec<AlternateShape> {
        vec![AlternateShape { number: 0, class: 0xff, endpoints: vec![] },
            AlternateShape { number: 1, class: 0xff, endpoints: vec![
                EndpointShape { address: 1, iso: true, asynchronous: true, usage: UsageType::Data, interval: 1, packet: outputs * 21 },
                EndpointShape { address: 0x82, iso: true, asynchronous: true, usage: UsageType::FeedbackData, interval: 1, packet: inputs * 21 },
            ] }]
    }
    #[test]
    fn exact_model_identity_and_asymmetric_format() {
        for (product, outputs, inputs) in [(0x17, 8, 8), (0x1b, 10, 12)] {
            let p = prove(0x2b73, product, Speed::High, &geometry(outputs, inputs)).unwrap().unwrap();
            assert_eq!((p.rate, p.output.channels, p.input.channels, p.output.subslot_bytes), (48_000, outputs, inputs, 3));
            assert_eq!((p.interface_number, p.alt_setting, p.output.address, p.input.address), (0, 1, 1, 0x82));
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
        alts.push(AlternateShape { number: 2, class: 0xff, endpoints: vec![] });
        let p = prove(0x2b73, 0x17, Speed::High, &alts).unwrap().unwrap();
        assert_eq!((p.output.max_packet_bytes, p.input.max_packet_bytes), (169, 170));
        for number in [0, 1] {
            alts.push(AlternateShape { number, class: 0xff, endpoints: vec![] });
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
