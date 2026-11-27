use super::audio_control_parser::AudioControlParser;
use super::parser_trait::DescriptorParser;
use super::types::AudioControlDescriptor;

#[test]
fn standard_uac2_feature_unit_has_no_control_size_byte() {
    let bytes = [
        18, 0x24, 6, 2, 1, 0x0f, 0, 0, 0, 0x0c, 0, 0, 0, 0x0c, 0, 0, 0, 0,
    ];
    let unit = AudioControlParser.parse_feature_unit(&bytes).unwrap();
    assert_eq!(unit.b_unit_id, 2);
    assert_eq!(unit.b_source_id, 1);
    assert_eq!(unit.bma_controls, vec![0x0f, 0x0c, 0x0c]);
    assert!(AudioControlParser.parse_feature_unit(&bytes[..17]).is_err());
}

#[test]
fn uac1_feature_unit_keeps_single_bit_controls() {
    let unit = AudioControlParser
        .parse_feature_unit_v1(&[10, 0x24, 6, 2, 1, 1, 3, 2, 2, 0])
        .unwrap();
    assert_eq!(unit.b_control_size, 1);
    assert_eq!(unit.bma_controls, vec![3, 2, 2]);
}

#[test]
fn nine_byte_uac2_header_does_not_read_past_end_or_guess_uac1() {
    let header = [9, 0x24, 1, 0, 2, 1, 1, 0, 3];
    match AudioControlParser.parse(&header).unwrap() {
        AudioControlDescriptor::HeaderV2(value) => assert_eq!(value.bm_controls, 3),
        _ => panic!("bcdADC determines the version"),
    }
}

#[test]
fn terminal_and_clock_selector_offsets_follow_uac2() {
    let input = [17, 0x24, 2, 1, 1, 1, 0, 5, 2, 3, 0, 0, 0, 0, 0, 0, 7];
    let terminal = AudioControlParser.parse_input_terminal(&input).unwrap();
    assert_eq!(terminal.b_nr_channels, 2);
    assert_eq!(terminal.w_channel_config, 3);
    assert_eq!(terminal.i_terminal, 7);
    let selector = AudioControlParser
        .parse_clock_selector(&[9, 0x24, 0x0b, 6, 2, 4, 5, 3, 7])
        .unwrap();
    assert_eq!(selector.ba_c_source_id, vec![4, 5]);
    assert_eq!(selector.bm_controls, 3);
}

#[test]
fn malformed_declared_lengths_and_variable_spans_return_without_panicking() {
    // Generic malformed-descriptor coverage, independent of generated fuzz
    // inputs. Exercise both dispatch and direct public parser entry points.
    let control = AudioControlParser;
    let streaming = super::AudioStreamingParser;
    for available in 0usize..=32 {
        for declared in [
            0,
            available.saturating_sub(1),
            available,
            available + 1,
            255,
        ] {
            for subtype in 1..=13 {
                let mut data = vec![0u8; available];
                for (index, value) in [
                    (0, declared as u8),
                    (1, 0x24),
                    (2, subtype),
                    (4, 255),
                    (5, 255),
                    (7, 255),
                ] {
                    if let Some(field) = data.get_mut(index) {
                        *field = value;
                    }
                }
                let _ = control.parse(&data);
                let _ = control.parse_ac_header(&data);
                let _ = control.parse_ac_header_v1(&data);
                let _ = control.parse_input_terminal(&data);
                let _ = control.parse_input_terminal_v1(&data);
                let _ = control.parse_output_terminal(&data);
                let _ = control.parse_output_terminal_v1(&data);
                let _ = control.parse_feature_unit(&data);
                let _ = control.parse_feature_unit_v1(&data);
                let _ = control.parse_clock_source(&data);
                let _ = control.parse_clock_selector(&data);
                let _ = control.parse_clock_multiplier(&data);
                let _ = control.parse_mixer_unit(&data);
                let _ = control.parse_selector_unit(&data);
                let _ = control.parse_effect_unit(&data);
                let _ = control.parse_processing_unit(&data);
                let _ = control.parse_extension_unit(&data);
                let _ = streaming.parse(&data);
                let _ = streaming.parse_as_general(&data);
                let _ = streaming.parse_as_general_v2(&data);
                let _ = streaming.parse_format_type_i(&data);
                let _ = streaming.parse_format_type_ii(&data);
                let _ = streaming.parse_format_type_iii(&data);
                let _ = streaming.parse_endpoint_specific(&data);
                let _ = super::parse_iad(&data);
            }
        }
    }
}

#[test]
fn following_bytes_cannot_complete_an_undersized_declared_descriptor() {
    let mut header = [9, 0x24, 1, 0, 2, 1, 1, 0, 3];
    header[0] = 5;
    assert!(AudioControlParser.parse_ac_header(&header).is_err());
    assert!(AudioControlParser.parse(&header).is_err());
    let mut feature = [10, 0x24, 6, 2, 1, 0x0f, 0, 0, 0, 0];
    feature[0] = 6;
    assert!(AudioControlParser.parse_feature_unit(&feature).is_err());
}
