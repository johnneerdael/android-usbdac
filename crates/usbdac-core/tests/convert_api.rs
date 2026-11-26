//! The conversion helpers used the way a host uses them, from outside the crate.

use usbdac_core::convert::{
    ConversionKind, ConversionPlan, ConvertError, ConvertOptions, Converter, Narrowing, PcmFormat,
    Quality, SampleFormat, Step,
};

fn assert_send<T: Send>() {}

#[test]
fn a_converter_can_move_to_the_writer_thread() {
    assert_send::<Converter>();
    assert_send::<ConversionPlan>();
}

#[test]
fn a_host_plans_converts_and_flushes_a_stream() {
    // The DAC refused 44.1 kHz and runs 48 kHz at 24 bits in 3 bytes; the host has 44.1 kHz 16-bit stereo.
    let source = PcmFormat::new(44_100, 2, SampleFormat::I16);
    let target = PcmFormat::new(48_000, 2, SampleFormat::I24);
    let plan = ConversionPlan::pcm(source, target, &ConvertOptions::default()).unwrap();
    assert_eq!(plan.kind(), ConversionKind::Lossy);
    assert!(!plan.preserves_bit_perfect());
    assert!(plan
        .steps()
        .iter()
        .any(|step| matches!(step, Step::Resample { .. })));
    println!("{}", plan.description());

    let mut converter = Converter::new(&plan).unwrap();
    let frames = 4_410usize; // 100 ms
    let input: Vec<u8> = (0..frames * 2)
        .flat_map(|i| (((i as f64 * 0.05).sin() * 20_000.0) as i16).to_le_bytes())
        .collect();
    let mut output = vec![0u8; converter.min_output_bytes().max(64 * 1024)];
    let mut total_out = 0usize;
    let mut offset = 0usize;
    while offset < input.len() {
        let done = converter.process(&input[offset..], &mut output).unwrap();
        offset += done.consumed;
        total_out += done.produced;
        assert!(done.consumed > 0 || done.produced > 0, "no progress");
    }
    loop {
        let n = converter.finish(&mut output).unwrap();
        if n == 0 {
            break;
        }
        total_out += n;
    }
    // ceil(frames * 160 / 147) frames of 6 bytes.
    assert_eq!(total_out, (frames * 160).div_ceil(147) * 6);
    assert!(converter.delay_frames() > 0);
    converter.reset();
}

#[test]
fn a_bit_exact_plan_keeps_a_bit_perfect_stream_bit_perfect_and_is_reported_so() {
    let plan = ConversionPlan::pcm(
        PcmFormat::new(96_000, 2, SampleFormat::I16),
        PcmFormat::new(96_000, 2, SampleFormat::I32),
        &ConvertOptions::default(),
    )
    .unwrap();
    assert_eq!(plan.kind(), ConversionKind::BitExact);
    assert!(plan.preserves_bit_perfect());
}

#[test]
fn what_cannot_be_converted_says_unsupported() {
    let stereo = PcmFormat::new(44_100, 2, SampleFormat::I16);
    let surround = PcmFormat::new(44_100, 6, SampleFormat::I16);
    let error = ConversionPlan::pcm(surround, stereo, &ConvertOptions::default()).unwrap_err();
    assert!(matches!(error, ConvertError::Unsupported(_)));
    assert_eq!(error.code(), "unsupported");
}

#[test]
fn the_options_are_plain_data() {
    let options = ConvertOptions {
        narrowing: Narrowing::Truncate,
        quality: Quality::Fast,
        headroom_db: -3.0,
        ..ConvertOptions::default()
    };
    let plan = ConversionPlan::pcm(
        PcmFormat::new(96_000, 2, SampleFormat::I24),
        PcmFormat::new(44_100, 2, SampleFormat::I16),
        &options,
    )
    .unwrap();
    assert_eq!(plan.options(), &options);
    assert!(plan.description().contains("fast quality"));
    assert!(plan.description().contains("-3.0 dB of headroom"));
}
