use super::*;

fn cluster(channels: u8, segments: &[u8]) -> Vec<u8> {
    let mut d = vec![0, 0, 0x26, 0, 0x34, 0x12, channels];
    d.extend_from_slice(segments);
    let length = (d.len() as u16).to_le_bytes();
    d[..2].copy_from_slice(&length);
    d
}
fn decode(d: &[u8]) -> Result<Cluster, DescriptorError> {
    let h = parse_cluster_header(&d[..6], 0x1234, 4096)?;
    parse_cluster(d, h)
}
#[test]
fn as_header_keeps_all_format_bits_and_control_words() {
    let mut d = vec![23, 0x24, 1, 5, 3, 0, 0, 0x80, 0x34, 0x12];
    d.extend_from_slice(&0x8000_0001_0000_0001u64.to_le_bytes());
    d.extend_from_slice(&[4, 24, 0x12, 0x34, 8]);
    let h = parse_as_header(&d).unwrap();
    assert_eq!(h.formats, 0x8000_0001_0000_0001);
    assert_eq!(h.controls.0, 0x8000_0003);
    assert_eq!(h.auxiliary_protocols, 0x3412);
    assert_eq!(h.control_size, 8);
    assert_eq!(h.controls.access(1), ControlAccess::ReadWrite);
    assert_eq!(h.controls.access(16), ControlAccess::Reserved);
    for length in 0..23 {
        assert!(parse_as_header(&d[..length]).is_err());
    }
    d[0] = 16;
    assert!(parse_as_header(&d).is_err()); // A UAC2 header cannot pass as ADC3.
}
#[test]
fn clusters_preserve_information_and_ambisonic_wire_order() {
    let d = cluster(
        3,
        &[
            5, 0, 1, 9, 0, 3, 0, 0xff, // Optional common description block.
            6, 0, 0x20, 1, 0x80, 2, 3, 0, 0xff, 7, 0, 0x21, 1, 4, 2, 3, 3, 0, 0xff, 6, 0, 0x20,
            0xff, 0xf0, 4, 3, 0, 0xff,
        ],
    );
    assert_eq!(
        decode(&d).unwrap().channels,
        vec![
            Channel::Information {
                purpose: 1,
                relationship: ChannelRelationship::Spatial(SpatialPosition::FrontLeft),
                group: 2
            },
            Channel::Ambisonic {
                ordering: 1,
                acn: 4,
                normalization: 2,
                group: 3
            },
            Channel::Information {
                purpose: 0xff,
                relationship: ChannelRelationship::Unknown(0xf0),
                group: 4
            },
        ]
    );
    assert!(decode(&cluster(0, &[])).unwrap().channels.is_empty());
}
#[test]
fn cluster_transfer_header_is_bounded_and_stable() {
    let d = cluster(1, &[6, 0, 0x20, 1, 1, 0, 3, 0, 0xff]);
    let h = parse_cluster_header(&d[..6], 0x1234, 4096).unwrap();
    assert!(parse_cluster_header(&d[..5], 0x1234, 4096).is_err());
    assert!(parse_cluster_header(&d[..6], 0x1235, 4096).is_err());
    assert!(parse_cluster_header(&d[..6], 0x1234, 7).is_err());
    assert!(parse_cluster_header(&[6, 0, 0x26, 0, 1, 0], 1, 4096).is_err());
    for at in [0, 2, 3, 4, 5] {
        let mut changed = d.clone();
        changed[at] ^= 1;
        assert!(parse_cluster(&changed, h).is_err());
    }
    assert!(parse_cluster(&d[..d.len() - 1], h).is_err());
}
#[test]
fn malformed_channel_blocks_fail_without_partial_layout() {
    for segments in [
        vec![0, 0, 0x20],                                              // zero progress
        vec![255, 255, 0x20],                                          // oversized
        vec![6, 0, 0x20, 1, 1, 0],                                     // no end
        vec![3, 0, 0xff],                                              // identity absent
        vec![6, 0, 0x20, 1, 1, 0, 7, 0, 0x21, 0, 0, 0, 0, 3, 0, 0xff], // two identities
        vec![6, 0, 0x20, 1, 1, 0, 3, 0, 0xff, 5, 0, 0x22, 0, 0], // trailing unterminated block
        vec![5, 0, 1, 0, 0, 6, 0, 0x20, 1, 1, 0, 3, 0, 0xff],    // unterminated common
    ] {
        assert!(decode(&cluster(1, &segments)).is_err(), "{segments:?}");
    }
    assert!(decode(&cluster(2, &[6, 0, 0x20, 1, 1, 0, 3, 0, 0xff])).is_err());
    assert!(decode(&cluster(0, &[3, 0, 0xff])).is_err());
}
#[test]
fn adc3_entities_keep_control_and_string_widths_and_clock_semantics() {
    let input = parse_input_terminal(&[
        20, 0x24, 2, 1, 1, 1, 0, 9, 3, 0, 0, 0x80, 1, 2, 3, 4, 5, 6, 7, 8,
    ])
    .unwrap();
    assert_eq!(
        (input.cluster_id, input.controls.0, input.string_id),
        (0x0201, 0x8000_0003, 0x0807)
    );
    let output =
        parse_output_terminal(&[19, 0x24, 3, 3, 1, 3, 0, 2, 9, 0, 1, 2, 3, 4, 5, 6, 7, 8, 9])
            .unwrap();
    assert_eq!(
        (output.clock_id, output.controls.0, output.string_id),
        (9, 0x0302_0100, 0x0908)
    );
    let source = parse_clock_source(&[12, 0x24, 0x0b, 9, 3, 7, 0, 0, 0x80, 0, 0x34, 0x12]).unwrap();
    assert!(source.is_internal() && source.is_synchronous_to_sof());
    assert_eq!(source.controls.0, 0x8000_0007);
    let selector =
        parse_clock_selector(&[13, 0x24, 0x0c, 10, 2, 9, 8, 3, 0, 0, 0x80, 0x34, 0x12]).unwrap();
    assert_eq!(selector.sources, [9, 8]);
    assert_eq!(selector.controls.0, 0x8000_0003);
    let mult =
        parse_clock_multiplier(&[11, 0x24, 0x0d, 11, 10, 5, 0, 0, 0x80, 0x34, 0x12]).unwrap();
    assert_eq!(
        (mult.source_id, mult.controls.0, mult.string_id),
        (10, 0x8000_0005, 0x1234)
    );
    assert!(parse_clock_selector(&[11, 0x24, 0x0c, 10, 0, 0, 0, 0, 0, 0, 0]).is_err());
    assert!(parse_clock_source(&[12, 0x24, 0x0b, 9, 2, 7, 0, 0, 0, 0, 0, 0]).is_err());
}
#[test]
fn power_domain_members_and_recovery_are_bounded() {
    let d = [13, 0x24, 0x10, 10, 0x58, 2, 0x70, 0x17, 2, 1, 3, 0x34, 0x12];
    let p = parse_power_domain(&d).unwrap();
    assert_eq!(
        (p.recovery_d1_micros(), p.recovery_d2_micros()),
        (30_000, 300_000)
    );
    assert_eq!(p.entities, [1, 3]);
    for length in 0..d.len() {
        assert!(parse_power_domain(&d[..length]).is_err());
    }
    let mut duplicate = d;
    duplicate[10] = 1;
    assert!(parse_power_domain(&duplicate).is_err());
}
fn iad(subclass: u8, count: u8) -> Iad {
    Iad {
        b_first_interface: 0,
        b_interface_count: count,
        b_function_class: 1,
        b_function_sub_class: subclass,
        b_function_protocol: 0x30,
        i_function: 0,
    }
}
#[test]
fn iad_requires_adc3_protocol_and_known_profile() {
    assert_eq!(
        function_profile(&iad(1, 2), 0x30).unwrap(),
        FunctionProfile::Full
    );
    assert_eq!(
        function_profile(&iad(0x24, 3), 0x30).unwrap(),
        FunctionProfile::Badd(BaddProfile::Headset)
    );
    assert!(function_profile(&iad(0x24, 2), 0x30).is_err());
    assert!(function_profile(&iad(0x27, 2), 0x30).is_err());
    assert!(function_profile(&iad(0x20, 2), 0x20).is_err());
    let mut wrong = iad(0x20, 2);
    wrong.b_function_protocol = 0x20;
    assert!(function_profile(&wrong, 0x30).is_err());
}
fn ep(address: u8, sync: u8, capacity: u16, interval: u8) -> [u8; 7] {
    let c = capacity.to_le_bytes();
    [7, 5, address, sync, c[0], c[1], interval]
}
#[test]
fn badd_capacity_sync_speed_and_direction_are_joint_evidence() {
    for (sync, sizes) in [(0x0d, [96, 144, 192, 288]), (0x05, [98, 147, 196, 294])] {
        for (i, capacity) in sizes.into_iter().enumerate() {
            for speed in [BusSpeed::Full, BusSpeed::High, BusSpeed::Super] {
                let interval = if speed == BusSpeed::Full { 1 } else { 4 };
                let f = infer_badd_format(
                    BaddProfile::GenericIo,
                    speed,
                    1,
                    &ep(1, sync, capacity, interval),
                    true,
                )
                .unwrap();
                assert_eq!(
                    (
                        f.channels,
                        f.subslot_size,
                        f.sample_rate,
                        f.clock_id,
                        f.power_domain_id
                    ),
                    (
                        if i < 2 { 1 } else { 2 },
                        if i % 2 == 0 { 2 } else { 3 },
                        48_000,
                        9,
                        10
                    )
                );
            }
        }
    }
    let valid = ep(0x81, 0x05, 98, 4);
    assert_eq!(
        infer_badd_format(BaddProfile::Microphone, BusSpeed::High, 1, &valid, false)
            .unwrap()
            .power_domain_id,
        11
    );
    assert!(infer_badd_format(BaddProfile::Headphone, BusSpeed::High, 1, &valid, false).is_err());
    for endpoint in [
        ep(1, 0x05, 96, 4),
        ep(1, 0x0d, 98, 4),
        ep(1, 0x09, 96, 4),
        ep(1, 0x0d, 96, 1),
        ep(0, 0x0d, 96, 4),
        ep(0x71, 0x0d, 96, 4),
        ep(1, 0x0d, 0x0860, 4),
    ] {
        assert!(
            infer_badd_format(BaddProfile::GenericIo, BusSpeed::High, 1, &endpoint, true).is_err()
        );
    }
    assert!(infer_badd_format(
        BaddProfile::GenericIo,
        BusSpeed::High,
        1,
        &ep(1, 0x05, 98, 4),
        false
    )
    .is_err());
    assert!(infer_badd_format(
        BaddProfile::GenericIo,
        BusSpeed::High,
        2,
        &ep(1, 0x0d, 96, 4),
        true
    )
    .is_err());
    assert!(infer_badd_format(
        BaddProfile::Headset,
        BusSpeed::High,
        1,
        &ep(0x81, 0x0d, 192, 4),
        true
    )
    .is_err());
    assert!(infer_badd_format(
        BaddProfile::Speakerphone,
        BusSpeed::High,
        1,
        &ep(1, 0x0d, 192, 4),
        true
    )
    .is_err());
}

#[test]
fn adc3_feature_unit_keeps_master_channel_controls_and_wide_string_id() {
    let d = [15, 0x24, 7, 2, 1, 3, 0, 0, 0x80, 12, 0, 0, 0x40, 0x34, 0x12];
    let unit = parse_feature_unit(&d).unwrap();
    assert_eq!(
        unit.controls,
        [Controls(0x8000_0003), Controls(0x4000_000c)]
    );
    assert_eq!(unit.string_id, 0x1234);
    for len in 0..d.len() {
        assert!(parse_feature_unit(&d[..len]).is_err());
    }
    let mut invalid = d;
    invalid[0] = 14;
    assert!(parse_feature_unit(&invalid).is_err());
}
