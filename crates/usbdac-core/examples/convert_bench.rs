//! Throughput of the conversion helpers on the machine it runs on.
//!
//! ```sh
//! cargo run --release -p usbdac-core --example convert_bench
//! ```
//!
//! Each case converts ten seconds of stereo audio in chunks of 10 ms and reports how many seconds
//! of audio are converted per second of CPU ("x realtime"), and the share of one core a real-time
//! stream of that conversion would need on this machine.

use std::time::Instant;
use usbdac_core::convert::{
    ConversionPlan, ConvertOptions, Converter, PcmFormat, Quality, SampleFormat,
};

fn run(label: &str, source: PcmFormat, target: PcmFormat, quality: Quality) {
    let options = ConvertOptions {
        quality,
        ..ConvertOptions::default()
    };
    let plan = ConversionPlan::pcm(source, target, &options).unwrap();
    let build = Instant::now();
    let mut converter = Converter::new(&plan).unwrap();
    let build_ms = build.elapsed().as_secs_f64() * 1000.0;

    let chunk_frames = source.sample_rate as usize / 100;
    let chunk: Vec<u8> =
        (0..chunk_frames * usize::from(source.channels) * source.sample.bytes_per_sample())
            .map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8)
            .collect();
    let mut output = vec![0u8; converter.min_output_bytes().max(1 << 20)];
    let seconds = 10;
    let start = Instant::now();
    let mut produced = 0usize;
    for _ in 0..seconds * 100 {
        let done = converter.process(&chunk, &mut output).unwrap();
        produced += done.produced;
    }
    let elapsed = start.elapsed().as_secs_f64();
    std::hint::black_box(produced);
    println!(
        "{label:<46} build {build_ms:6.1} ms   {:6.1}x realtime   {:5.2}% of a core",
        seconds as f64 / elapsed,
        elapsed / seconds as f64 * 100.0
    );
}

fn main() {
    let stereo = |rate, sample| PcmFormat::new(rate, 2, sample);
    println!("{}", plan_line());
    run(
        "copy 44.1k 16-bit",
        stereo(44_100, SampleFormat::I16),
        stereo(44_100, SampleFormat::I16),
        Quality::High,
    );
    run(
        "widen 16 -> 24 bit (integer engine)",
        stereo(44_100, SampleFormat::I16),
        stereo(44_100, SampleFormat::I24),
        Quality::High,
    );
    run(
        "narrow 32 -> 16 bit, TPDF dither",
        stereo(96_000, SampleFormat::I32),
        stereo(96_000, SampleFormat::I16),
        Quality::High,
    );
    for (label, from, to) in [
        ("44.1k -> 48k", 44_100, 48_000),
        ("48k -> 44.1k", 48_000, 44_100),
        ("96k -> 44.1k", 96_000, 44_100),
        ("44.1k -> 96k", 44_100, 96_000),
        ("44.1k -> 192k", 44_100, 192_000),
        ("192k -> 48k", 192_000, 48_000),
        ("44.1k -> 768k (most phases)", 44_100, 768_000),
        ("768k -> 44.1k (longest filter)", 768_000, 44_100),
    ] {
        for quality in [Quality::High, Quality::Fast] {
            run(
                &format!("resample {label}, 24-bit, {quality:?}"),
                stereo(from, SampleFormat::I24),
                stereo(to, SampleFormat::I24),
                quality,
            );
        }
    }
}

fn plan_line() -> String {
    let profile = if cfg!(debug_assertions) {
        "debug (slow: use --release)"
    } else {
        "release"
    };
    format!("usbdac-core conversion throughput, {profile} build, stereo, 10 s of audio per case")
}
