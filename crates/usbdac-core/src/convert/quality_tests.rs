//! Measures the sample-rate converter with tones: passband flatness, stopband attenuation, image and
//! alias rejection. The numbers asserted are the ones `docs/API.md` quotes; run
//! `cargo test -p usbdac-core quality -- --nocapture` to print what was measured.

use super::format::Quality;
use super::resample::tests::{finish, run, tone};
use super::resample::Resampler;
use std::f64::consts::PI;

/// Amplitude of the sine of frequency `hz` that best fits `y` (sampled at `rate`) in the least-squares
/// sense together with a constant, and the RMS of what is left once it is removed.
fn fit_tone(y: &[f64], rate: f64, hz: f64) -> (f64, f64) {
    let n = y.len() as f64;
    let (mut ss, mut cc, mut sc, mut s, mut c) = (0.0, 0.0, 0.0, 0.0, 0.0);
    let (mut ys, mut yc, mut yy_sum) = (0.0, 0.0, 0.0);
    let w = 2.0 * PI * hz / rate;
    for (i, &v) in y.iter().enumerate() {
        let (sin, cos) = (w * i as f64).sin_cos();
        ss += sin * sin;
        cc += cos * cos;
        sc += sin * cos;
        s += sin;
        c += cos;
        ys += v * sin;
        yc += v * cos;
        yy_sum += v;
    }
    // Solve [ss sc s; sc cc c; s c n] * [a b d] = [ys yc sum] by Cramer's rule.
    let det = |m: [[f64; 3]; 3]| {
        m[0][0] * (m[1][1] * m[2][2] - m[1][2] * m[2][1])
            - m[0][1] * (m[1][0] * m[2][2] - m[1][2] * m[2][0])
            + m[0][2] * (m[1][0] * m[2][1] - m[1][1] * m[2][0])
    };
    let a_matrix = [[ss, sc, s], [sc, cc, c], [s, c, n]];
    let d = det(a_matrix);
    let a = det([[ys, sc, s], [yc, cc, c], [yy_sum, c, n]]) / d;
    let b = det([[ss, ys, s], [sc, yc, c], [s, yy_sum, n]]) / d;
    let offset = det([[ss, sc, ys], [sc, cc, yc], [s, c, yy_sum]]) / d;
    let mut residual = 0.0;
    for (i, &v) in y.iter().enumerate() {
        let (sin, cos) = (w * i as f64).sin_cos();
        let e = v - (a * sin + b * cos + offset);
        residual += e * e;
    }
    ((a * a + b * b).sqrt(), (residual / n).sqrt())
}

fn rms(y: &[f64]) -> f64 {
    (y.iter().map(|v| v * v).sum::<f64>() / y.len() as f64).sqrt()
}

fn db(ratio: f64) -> f64 {
    20.0 * ratio.max(1e-30).log10()
}

struct Run {
    output: Vec<f64>,
    rate: f64,
}

/// Resamples a tone and returns the settled middle of the output.
fn resample_tone(from: u32, to: u32, quality: Quality, hz: f64, amplitude: f64) -> Run {
    let mut resampler = Resampler::new(from, to, 1, quality, 4096).unwrap();
    let info = *resampler.info();
    let margin = info.taps * info.l / info.m + 16;
    let window = 4096.max((3.0 * f64::from(to) / hz) as usize);
    let wanted = window + 2 * margin;
    let frames_in = (wanted * info.m).div_ceil(info.l);
    let input = tone(f64::from(from), hz, amplitude, 0, frames_in);
    let mut output = run(&mut resampler, &input, 4096);
    output.extend(finish(&mut resampler, 4096));
    Run {
        output: output[margin..margin + window].to_vec(),
        rate: f64::from(to),
    }
}

/// Gain of the converter at `hz`, in dB, and the level of everything else in the output relative
/// to the tone (the sum of images, aliases, harmonics and numerical noise), in dB.
fn passband_point(from: u32, to: u32, quality: Quality, hz: f64) -> (f64, f64) {
    let measured = resample_tone(from, to, quality, hz, 0.5);
    let (amplitude, residual) = fit_tone(&measured.output, measured.rate, hz);
    (db(amplitude / 0.5), db(residual * 2f64.sqrt() / amplitude))
}

/// Attenuation, in dB, of a tone at `hz` that is above the output's Nyquist frequency: all that
/// comes out of the converter is alias, so it is the whole output.
fn stopband_point(from: u32, to: u32, quality: Quality, hz: f64) -> f64 {
    let measured = resample_tone(from, to, quality, hz, 0.5);
    let mean = measured.output.iter().sum::<f64>() / measured.output.len() as f64;
    let centred: Vec<f64> = measured.output.iter().map(|v| v - mean).collect();
    -db(rms(&centred) * 2f64.sqrt() / 0.5)
}

fn spec_of(quality: Quality) -> (f64, f64) {
    // (passband fraction, stopband fraction) of the lower Nyquist frequency
    match quality {
        Quality::High => (0.91, 1.0),
        Quality::Fast => (0.90, 1.10),
    }
}

/// The rate pairs measured: every direction that matters, from the CD/48 family crossing to the extremes.
const PAIRS: &[(u32, u32)] = &[
    (44_100, 48_000),
    (48_000, 44_100),
    (44_100, 96_000),
    (96_000, 44_100),
    (96_000, 48_000),
    (88_200, 48_000),
    (192_000, 44_100),
    (44_100, 192_000),
    (176_400, 96_000),
    (384_000, 48_000),
    (48_000, 352_800),
];

struct Summary {
    ripple_db: f64,
    spurs_db: f64,
    stopband_db: f64,
}

/// Passband ripple (peak to peak), the strongest spur among the in-band tones, and the weakest stopband
/// attenuation over a sweep above the stopband edge (downsampling only).
fn measure_pair(from: u32, to: u32, quality: Quality) -> Summary {
    measure_pair_points(from, to, quality, 12, 10)
}

/// As [`measure_pair`] with `passband_steps + 1` tones in the passband and `stopband_steps + 1` above it.
fn measure_pair_points(
    from: u32,
    to: u32,
    quality: Quality,
    passband_steps: usize,
    stopband_steps: usize,
) -> Summary {
    let nyquist = f64::from(from.min(to)) / 2.0;
    let (passband, stopband) = spec_of(quality);
    let mut gains = Vec::new();
    let mut spurs: f64 = -400.0;
    for step in 0..=passband_steps {
        let hz = 150.0 + (passband * nyquist - 150.0) * step as f64 / passband_steps as f64;
        let (gain, spur) = passband_point(from, to, quality, hz);
        gains.push(gain);
        spurs = spurs.max(spur);
    }
    let ripple = gains.iter().cloned().fold(f64::MIN, f64::max)
        - gains.iter().cloned().fold(f64::MAX, f64::min);

    let mut weakest = f64::INFINITY;
    let low = stopband * nyquist * 1.001;
    let high = f64::from(from) / 2.0 * 0.999;
    if to < from && low < high {
        for step in 0..=stopband_steps {
            let hz = low + (high - low) * step as f64 / stopband_steps as f64;
            weakest = weakest.min(stopband_point(from, to, quality, hz));
        }
    }
    Summary {
        ripple_db: ripple,
        spurs_db: spurs,
        stopband_db: weakest,
    }
}

#[test]
fn quality_high_meets_its_targets() {
    for &(from, to) in PAIRS {
        let summary = measure_pair(from, to, Quality::High);
        println!(
            "High {from:>6} -> {to:>6}: passband ripple {:.6} dB, worst spur {:.1} dB, stopband {}",
            summary.ripple_db,
            summary.spurs_db,
            if summary.stopband_db.is_finite() {
                format!("{:.1} dB", summary.stopband_db)
            } else {
                "n/a (upsampling)".into()
            },
        );
        assert!(
            summary.ripple_db < 0.0002,
            "{from} -> {to}: ripple {} dB",
            summary.ripple_db
        );
        assert!(
            summary.spurs_db < -110.0,
            "{from} -> {to}: spur {} dB",
            summary.spurs_db
        );
        if summary.stopband_db.is_finite() {
            assert!(
                summary.stopband_db > 105.0,
                "{from} -> {to}: stopband {} dB",
                summary.stopband_db
            );
        }
    }
}

#[test]
fn quality_fast_meets_its_targets() {
    for &(from, to) in PAIRS {
        let summary = measure_pair(from, to, Quality::Fast);
        println!(
            "Fast {from:>6} -> {to:>6}: passband ripple {:.6} dB, worst spur {:.1} dB, stopband {}",
            summary.ripple_db,
            summary.spurs_db,
            if summary.stopband_db.is_finite() {
                format!("{:.1} dB", summary.stopband_db)
            } else {
                "n/a (upsampling)".into()
            },
        );
        assert!(
            summary.ripple_db < 0.002,
            "{from} -> {to}: ripple {} dB",
            summary.ripple_db
        );
        assert!(
            summary.spurs_db < -80.0,
            "{from} -> {to}: spur {} dB",
            summary.spurs_db
        );
        if summary.stopband_db.is_finite() {
            assert!(
                summary.stopband_db > 80.0,
                "{from} -> {to}: stopband {} dB",
                summary.stopband_db
            );
        }
    }
}

#[test]
fn nothing_aliases_into_the_output_band_with_high_quality() {
    // 96 kHz to 44.1 kHz: a tone just above the output's Nyquist frequency (22.05 kHz) would fold to
    // just below it; with the stopband starting at the Nyquist frequency there is nothing to fold.
    for hz in [22_100.0, 23_000.0, 26_000.0, 30_000.0, 40_000.0, 47_900.0] {
        let attenuation = stopband_point(96_000, 44_100, Quality::High, hz);
        assert!(attenuation > 100.0, "{hz} Hz is only {attenuation} dB down");
    }
}

#[test]
fn the_extreme_ratios_meet_the_targets_too() {
    // 44.1 kHz to 768 kHz is the most phases (2560); 768 kHz to 44.1 kHz the longest filter.
    for (from, to) in [(44_100u32, 768_000u32), (768_000, 44_100)] {
        let summary = measure_pair_points(from, to, Quality::High, 3, 3);
        println!(
            "High {from:>6} -> {to:>6}: passband ripple {:.6} dB, worst spur {:.1} dB, stopband {}",
            summary.ripple_db,
            summary.spurs_db,
            if summary.stopband_db.is_finite() {
                format!("{:.1} dB", summary.stopband_db)
            } else {
                "n/a (upsampling)".into()
            },
        );
        assert!(
            summary.ripple_db < 0.0002,
            "{from} -> {to}: ripple {} dB",
            summary.ripple_db
        );
        assert!(
            summary.spurs_db < -110.0,
            "{from} -> {to}: spur {} dB",
            summary.spurs_db
        );
        if summary.stopband_db.is_finite() {
            assert!(
                summary.stopband_db > 105.0,
                "{from} -> {to}: stopband {} dB",
                summary.stopband_db
            );
        }
    }
}
