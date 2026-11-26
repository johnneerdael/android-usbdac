//! A polyphase windowed-sinc sample-rate converter for rational ratios.
//!
//! Written from the textbook design, with nothing borrowed from another converter.
//!
//! ### Design
//!
//! For rates `fi` (input) and `fo` (output) let `g = gcd(fi, fo)`, `L = fo / g` and `M = fi / g`.
//! Output sample `n` sits at input time `t = n * M / L`, that is between input samples `k` and
//! `k + 1` with `k = floor(n * M / L)` and fraction `p / L`, `p = (n * M) mod L`. It is
//!
//! ```text
//! y[n] = sum_j x[k - (T/2 - 1) + j] * c[p][j]      c[p][j] = h((T/2 - 1) - j + p / L)
//! ```
//!
//! where `h` is a windowed sinc evaluated at the exact fractional offset: the response of the
//! ideal low-pass filter `2 fc sinc(2 fc t)` (time in input samples, `fc` in cycles per input
//! sample) times a Kaiser window of half-width `T/2` input samples. The coefficients are computed
//! once, for the `L` phases, and each phase is normalised to sum to exactly 1, so the gain at DC is
//! 1 for every phase. There are `L * T` of them (at most 4.5 million), and an output costs `T`
//! multiply-adds.
//!
//! The filter is zero-phase: output `n` is aligned with input time `n * M / L` exactly, with no
//! group delay to compensate. What it needs is the `T/2` input samples after that instant, so the
//! converter holds back `T/2` input frames ([`Resampler::delay_frames`]): they are the algorithmic
//! latency, and they are flushed by feeding `T/2` zeros ([`Resampler::pad_frames`]), which makes
//! the total output length for `N` input frames exactly `ceil(N * L / M)`.
//!
//! ### Quality
//!
//! `fp` is the passband edge and `fs` the stopband edge, `Delta = (fs - fp) / fi`, `A` the stopband
//! attenuation to design for. Kaiser's formulas give the window shape `beta = 0.1102 (A - 8.7)` and the
//! half-width `W = (A - 7.95) / (28.72 * Delta)` input samples, and the cutoff `fc` is the centre of the
//! transition band. [`Quality::High`] designs for `fp = 0.91` and `fs = 1.0` times the lower Nyquist
//! frequency, so nothing above it can alias into the output; [`Quality::Fast`] for `fp = 0.90` and
//! `fs = 1.10` (content between them aliases into the 10% of the band under the Nyquist
//! frequency, which the passband does not use), and a lower `A`; it takes about 2.9 times fewer
//! taps and runs about 2.5 times faster. The tests measure the result and
//! `docs/API.md` quotes the numbers.
//!
//! ### Cost
//!
//! The filter is longer the more it downsamples: `T` is about `158 * fi / min(fi, fo)` taps for
//! [`Quality::High`], so 768 kHz to 44.1 kHz takes about 2800 multiply-adds per output sample
//! (about 120 million per second per channel). The processing loop allocates nothing.

use super::format::{ConvertError, Quality};

/// The longest filter the converter will build, in taps per phase.
pub const MAX_TAPS: usize = 8192;
/// The largest coefficient table the converter will build.
pub const MAX_COEFFICIENTS: usize = 4_500_000;

/// The design targets of a [`Quality`].
#[derive(Debug, Clone, Copy)]
struct Spec {
    /// Stopband attenuation the Kaiser formulas are given.
    attenuation_db: f64,
    /// Passband edge as a fraction of the lower Nyquist frequency.
    passband: f64,
    /// Stopband edge as a fraction of the lower Nyquist frequency.
    stopband: f64,
}

fn spec(quality: Quality) -> Spec {
    match quality {
        Quality::High => Spec {
            attenuation_db: 110.0,
            passband: 0.91,
            stopband: 1.0,
        },
        Quality::Fast => Spec {
            attenuation_db: 84.0,
            passband: 0.90,
            stopband: 1.10,
        },
    }
}

/// What the converter built for a pair of rates.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FilterInfo {
    /// Upsampling factor of the polyphase structure (and number of phases).
    pub l: usize,
    /// Decimation factor.
    pub m: usize,
    /// Taps per output sample.
    pub taps: usize,
    /// Passband edge in Hz.
    pub passband_hz: f64,
    /// Stopband edge in Hz.
    pub stopband_hz: f64,
    /// Attenuation the filter was designed for, in dB.
    pub design_attenuation_db: f64,
}

fn gcd(mut a: u64, mut b: u64) -> u64 {
    while b != 0 {
        (a, b) = (b, a % b);
    }
    a
}

/// Modified Bessel function of the first kind, order 0, by its power series.
fn bessel_i0(x: f64) -> f64 {
    let quarter = x * x / 4.0;
    let mut term = 1.0;
    let mut sum = 1.0;
    for k in 1..400 {
        term *= quarter / (k * k) as f64;
        sum += term;
        if term < sum * 1e-18 {
            break;
        }
    }
    sum
}

fn kaiser_beta(attenuation_db: f64) -> f64 {
    if attenuation_db > 50.0 {
        0.1102 * (attenuation_db - 8.7)
    } else if attenuation_db >= 21.0 {
        0.5842 * (attenuation_db - 21.0).powf(0.4) + 0.07886 * (attenuation_db - 21.0)
    } else {
        0.0
    }
}

fn sinc(x: f64) -> f64 {
    if x.abs() < 1e-12 {
        1.0
    } else {
        let px = std::f64::consts::PI * x;
        px.sin() / px
    }
}

/// Describes the filter for `from_hz` to `to_hz` without synthesising it: the polyphase ratio and
/// the number of taps. Fails for a ratio that needs more than [`MAX_COEFFICIENTS`] coefficients.
pub fn filter_info(from_hz: u32, to_hz: u32, quality: Quality) -> Result<FilterInfo, ConvertError> {
    if from_hz == 0 || to_hz == 0 {
        return Err(ConvertError::InvalidArgument(
            "rates must be positive".into(),
        ));
    }
    let g = gcd(u64::from(from_hz), u64::from(to_hz));
    let m = (u64::from(from_hz) / g) as usize;
    let l = (u64::from(to_hz) / g) as usize;
    let spec = spec(quality);

    let fi = f64::from(from_hz);
    let nyquist = f64::from(from_hz.min(to_hz)) / 2.0;
    let passband_hz = spec.passband * nyquist;
    let stopband_hz = spec.stopband * nyquist;
    let transition = (stopband_hz - passband_hz) / fi;
    let half_width = (spec.attenuation_db - 7.95) / (28.72 * transition);
    let mut taps = 2 * half_width.ceil() as usize;
    taps = taps.div_ceil(8) * 8;
    taps = taps.max(16);
    if taps > MAX_TAPS || l.saturating_mul(taps) > MAX_COEFFICIENTS {
        return Err(ConvertError::Unsupported(format!(
            "converting {from_hz} Hz to {to_hz} Hz needs a filter of {taps} taps in {l} phases, \
             more than the {MAX_COEFFICIENTS} coefficients the converter builds"
        )));
    }
    Ok(FilterInfo {
        l,
        m,
        taps,
        passband_hz,
        stopband_hz,
        design_attenuation_db: spec.attenuation_db,
    })
}

/// The filter for `from_hz` to `to_hz`: its description and the `L * T` coefficients, phase after phase.
fn design(
    from_hz: u32,
    to_hz: u32,
    quality: Quality,
) -> Result<(FilterInfo, Vec<f64>), ConvertError> {
    let info = filter_info(from_hz, to_hz, quality)?;
    let (l, taps) = (info.l, info.taps);
    let cutoff = (info.passband_hz + info.stopband_hz) / 2.0 / f64::from(from_hz); // cycles per input sample
    let beta = kaiser_beta(info.design_attenuation_db);
    let i0_beta = bessel_i0(beta);
    let half = (taps / 2) as f64;
    let mut coef = vec![0.0f64; l * taps];
    for p in 0..l {
        let row = &mut coef[p * taps..(p + 1) * taps];
        let fraction = p as f64 / l as f64;
        for (j, slot) in row.iter_mut().enumerate() {
            let t = (half - 1.0) - j as f64 + fraction;
            let r = t / half;
            let window = if r.abs() >= 1.0 {
                0.0
            } else {
                bessel_i0(beta * (1.0 - r * r).sqrt()) / i0_beta
            };
            *slot = 2.0 * cutoff * sinc(2.0 * cutoff * t) * window;
        }
        let sum: f64 = row.iter().sum();
        for slot in row.iter_mut() {
            *slot /= sum;
        }
    }
    Ok((info, coef))
}

/// A dot product with eight independent accumulators; `a` and `b` have the same length, a multiple of 8.
#[inline]
fn dot(a: &[f64], b: &[f64]) -> f64 {
    debug_assert_eq!(a.len(), b.len());
    debug_assert_eq!(a.len() % 8, 0);
    let mut acc = [0.0f64; 8];
    for (x, y) in a.chunks_exact(8).zip(b.chunks_exact(8)) {
        for i in 0..8 {
            acc[i] += x[i] * y[i];
        }
    }
    ((acc[0] + acc[4]) + (acc[1] + acc[5])) + ((acc[2] + acc[6]) + (acc[3] + acc[7]))
}

/// A streaming rational resampler for any number of channels that advance together.
///
/// Planar buffers (one `Vec<f64>` per channel) are used so the caller can reuse them: nothing in
/// [`Resampler::process`] allocates, provided the input block has at most `max_block` frames and the
/// output buffers are long enough ([`Resampler::output_len_for`]).
pub struct Resampler {
    info: FilterInfo,
    coef: Box<[f64]>,
    half: usize,
    max_block: usize,
    history: Vec<Vec<f64>>,
    /// Absolute index of `history[c][0]` (negative at the start: the leading zeros).
    base: i64,
    /// Absolute index of the input sample at or left of the next output's position.
    k: i64,
    /// Phase of the next output, `0..L`.
    p: usize,
    in_total: u64,
    out_total: u64,
}

impl Resampler {
    /// A converter from `from_hz` to `to_hz` for `channels` channels, taking at most `max_block`
    /// frames per call.
    pub fn new(
        from_hz: u32,
        to_hz: u32,
        channels: usize,
        quality: Quality,
        max_block: usize,
    ) -> Result<Self, ConvertError> {
        if from_hz == 0 || to_hz == 0 || channels == 0 || max_block == 0 {
            return Err(ConvertError::InvalidArgument(
                "rates, channels and block size must be positive".into(),
            ));
        }
        if from_hz == to_hz {
            return Err(ConvertError::InvalidArgument(
                "the rates are equal: there is nothing to resample".into(),
            ));
        }
        let (info, coef) = design(from_hz, to_hz, quality)?;
        let half = info.taps / 2;
        let mut resampler = Self {
            info,
            coef: coef.into_boxed_slice(),
            half,
            max_block,
            history: (0..channels)
                .map(|_| Vec::with_capacity(info.taps + max_block))
                .collect(),
            base: 0,
            k: 0,
            p: 0,
            in_total: 0,
            out_total: 0,
        };
        resampler.reset();
        Ok(resampler)
    }

    pub fn info(&self) -> &FilterInfo {
        &self.info
    }

    pub fn channels(&self) -> usize {
        self.history.len()
    }

    /// Input frames the converter holds back: the algorithmic latency. Output sample `n` is aligned
    /// with input time `n * M / L` and is produced once the input has reached `delay_frames()` frames past it.
    pub fn delay_frames(&self) -> usize {
        self.half
    }

    /// Zero frames to feed after the last input to flush the tail out.
    pub fn pad_frames(&self) -> usize {
        self.half
    }

    /// Input frames fed since the start or the last reset.
    pub fn frames_in(&self) -> u64 {
        self.in_total
    }

    /// Output frames produced since the start or the last reset.
    pub fn frames_out(&self) -> u64 {
        self.out_total
    }

    /// Forgets the signal: the next input is the start of a new stream.
    pub fn reset(&mut self) {
        let lead = self.half - 1;
        for history in &mut self.history {
            history.clear();
            history.resize(lead, 0.0);
        }
        self.base = -(lead as i64);
        self.k = 0;
        self.p = 0;
        self.in_total = 0;
        self.out_total = 0;
    }

    /// Output frames produced in all once `input_total` input frames have been fed.
    fn outputs_after(&self, input_total: u64) -> u64 {
        let usable = input_total.saturating_sub(self.half as u64);
        let numerator = u128::from(usable) * self.info.l as u128;
        numerator.div_ceil(self.info.m as u128) as u64
    }

    /// How many output frames feeding `input_frames` more input produces, exactly.
    pub fn output_len_for(&self, input_frames: usize) -> usize {
        (self.outputs_after(self.in_total + input_frames as u64) - self.out_total) as usize
    }

    /// The most input frames that can be fed so that no more than `output_frames` come out.
    pub fn max_input_for_output(&self, output_frames: usize) -> usize {
        let allowed_total = u128::from(self.out_total) + output_frames as u128;
        let usable = allowed_total * self.info.m as u128 / self.info.l as u128;
        let input_total = usable + self.half as u128;
        input_total
            .saturating_sub(u128::from(self.in_total))
            .min(usize::MAX as u128) as usize
    }

    /// Feeds `n` frames (`input[c][..n]` for each channel) and writes the output frames to the
    /// front of `output[c]`; returns how many ([`Resampler::output_len_for`]). `n` must be at most
    /// `max_block` and every `output[c]` at least that long.
    pub fn process(&mut self, input: &[Vec<f64>], n: usize, output: &mut [Vec<f64>]) -> usize {
        assert_eq!(
            input.len(),
            self.history.len(),
            "one input buffer per channel"
        );
        assert_eq!(
            output.len(),
            self.history.len(),
            "one output buffer per channel"
        );
        assert!(
            n <= self.max_block,
            "a block of {n} frames exceeds the maximum of {}",
            self.max_block
        );
        let taps = self.info.taps;
        let (l, m) = (self.info.l, self.info.m);
        let lead = self.half as i64 - 1;
        let wanted = self.output_len_for(n);

        let mut next_k = self.k;
        let mut next_p = self.p;
        let mut dropped = 0usize;
        for (channel, history) in self.history.iter_mut().enumerate() {
            history.extend_from_slice(&input[channel][..n]);
            let out = &mut output[channel];
            assert!(
                out.len() >= wanted,
                "the output buffer holds {} frames, {wanted} are needed",
                out.len()
            );
            let (mut k, mut p) = (self.k, self.p);
            let mut count = 0usize;
            loop {
                let start = (k - lead - self.base) as usize;
                if start + taps > history.len() {
                    break;
                }
                out[count] = dot(
                    &history[start..start + taps],
                    &self.coef[p * taps..(p + 1) * taps],
                );
                count += 1;
                p += m;
                k += (p / l) as i64;
                p %= l;
            }
            debug_assert_eq!(count, wanted);
            // Drop what no later output needs.
            let next_start = (k - lead - self.base) as usize;
            let drop = next_start.min(history.len());
            history.copy_within(drop.., 0);
            let kept = history.len() - drop;
            history.truncate(kept);
            next_k = k;
            next_p = p;
            dropped = drop;
        }
        self.base += dropped as i64;
        self.k = next_k;
        self.p = next_p;
        self.in_total += n as u64;
        self.out_total += wanted as u64;
        wanted
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::f64::consts::PI;

    /// Sine of `hz` at amplitude `amplitude` sampled at `rate`, `len` frames from `start`.
    pub fn tone(rate: f64, hz: f64, amplitude: f64, start: usize, len: usize) -> Vec<f64> {
        (start..start + len)
            .map(|n| amplitude * (2.0 * PI * hz * n as f64 / rate).sin())
            .collect()
    }

    /// Runs one channel through a resampler in blocks of `block`, returning everything it produced.
    pub fn run(resampler: &mut Resampler, input: &[f64], block: usize) -> Vec<f64> {
        let mut result = Vec::new();
        let mut out = vec![Vec::new()];
        for chunk in input.chunks(block) {
            let wanted = resampler.output_len_for(chunk.len());
            out[0].resize(wanted, 0.0);
            let produced = resampler.process(&[chunk.to_vec()], chunk.len(), &mut out);
            result.extend_from_slice(&out[0][..produced]);
        }
        result
    }

    /// Feeds the tail of zeros and returns what comes out.
    pub fn finish(resampler: &mut Resampler, block: usize) -> Vec<f64> {
        run(resampler, &vec![0.0; resampler.pad_frames()], block)
    }

    fn make(from: u32, to: u32, quality: Quality) -> Resampler {
        Resampler::new(from, to, 1, quality, 4096).unwrap()
    }

    #[test]
    fn the_polyphase_ratio_is_the_reduced_fraction() {
        for (from, to, l, m) in [
            (44_100, 48_000, 160, 147),
            (48_000, 44_100, 147, 160),
            (44_100, 88_200, 2, 1),
            (96_000, 44_100, 147, 320),
            (768_000, 44_100, 147, 2560),
            (44_100, 768_000, 2560, 147),
            (705_600, 48_000, 10, 147),
        ] {
            let resampler = make(from, to, Quality::Fast);
            assert_eq!(
                (resampler.info().l, resampler.info().m),
                (l, m),
                "{from} -> {to}"
            );
        }
    }

    #[test]
    fn equal_rates_and_nonsense_are_refused() {
        assert!(Resampler::new(48_000, 48_000, 2, Quality::High, 64).is_err());
        assert!(Resampler::new(0, 48_000, 2, Quality::High, 64).is_err());
        assert!(Resampler::new(48_000, 44_100, 0, Quality::High, 64).is_err());
    }

    #[test]
    fn a_ratio_that_needs_an_unreasonable_filter_is_unsupported() {
        // 44100 Hz to 47993 Hz does not reduce: 47993 phases of about 160 taps.
        let error = Resampler::new(44_100, 47_993, 2, Quality::High, 64)
            .err()
            .unwrap();
        assert_eq!(error.code(), "unsupported");
    }

    #[test]
    fn every_standard_ratio_builds_within_the_limits() {
        let rates = [
            44_100u32, 88_200, 176_400, 352_800, 705_600, 48_000, 96_000, 192_000, 384_000, 768_000,
        ];
        for &from in &rates {
            for &to in &rates {
                if from == to {
                    continue;
                }
                for quality in [Quality::High, Quality::Fast] {
                    let resampler = Resampler::new(from, to, 1, quality, 64)
                        .unwrap_or_else(|error| panic!("{from} -> {to} {quality:?}: {error}"));
                    let info = resampler.info();
                    assert!(info.taps <= MAX_TAPS);
                    assert!(info.taps * info.l <= MAX_COEFFICIENTS);
                    assert_eq!(info.taps % 8, 0);
                }
            }
        }
    }

    #[test]
    fn the_gain_at_dc_is_exactly_one_in_every_phase() {
        for (from, to) in [
            (44_100, 48_000),
            (48_000, 44_100),
            (96_000, 44_100),
            (44_100, 192_000),
        ] {
            let mut resampler = make(from, to, Quality::High);
            let input = vec![0.3; 20_000];
            let mut output = run(&mut resampler, &input, 777);
            output.extend(finish(&mut resampler, 777));
            // The first outputs see the zeros before the signal; after the filter's length the gain is DC.
            let info = *resampler.info();
            let margin = info.taps * info.l / info.m + 8;
            let settled = &output[margin..output.len() - margin];
            let worst = settled.iter().map(|y| (y - 0.3).abs()).fold(0.0, f64::max);
            assert!(worst < 1e-13, "{from} -> {to}: DC gain off by {worst}");
        }
    }

    #[test]
    fn the_output_length_is_ceil_of_the_input_length_times_l_over_m() {
        for (from, to, frames) in [
            (44_100u32, 48_000u32, 10_000usize),
            (48_000, 44_100, 9_999),
            (96_000, 44_100, 12_345),
            (44_100, 88_200, 777),
        ] {
            let mut resampler = make(from, to, Quality::Fast);
            let mut produced = run(&mut resampler, &vec![0.1; frames], 513).len();
            produced += finish(&mut resampler, 513).len();
            let info = *resampler.info();
            let expected = (frames * info.l).div_ceil(info.m);
            assert_eq!(produced, expected, "{from} -> {to}");
            assert_eq!(resampler.frames_out(), expected as u64);
        }
    }

    #[test]
    fn output_len_for_and_max_input_for_output_are_exact_and_agree() {
        let mut resampler = make(48_000, 44_100, Quality::Fast);
        let mut fed = 0usize;
        let mut made = 0usize;
        for step in [1usize, 7, 160, 3, 1000, 2, 4095] {
            let predicted = resampler.output_len_for(step);
            let produced = run(&mut resampler, &vec![0.0; step], 4096).len();
            assert_eq!(predicted, produced);
            fed += step;
            made += produced;
            // Everything up to the largest allowed feed produces no more than asked for.
            for cap in [0usize, 1, 5, 100] {
                let allowed = resampler.max_input_for_output(cap);
                assert!(resampler.output_len_for(allowed) <= cap, "cap {cap}");
                assert!(resampler.output_len_for(allowed + 1) > cap, "cap {cap}");
            }
        }
        assert_eq!(resampler.frames_in(), fed as u64);
        assert_eq!(resampler.frames_out(), made as u64);
    }

    #[test]
    fn chunking_does_not_change_a_single_bit() {
        let input = tone(44_100.0, 1_234.5, 0.7, 0, 30_000);
        let mut reference = make(44_100, 48_000, Quality::High);
        let mut expected = run(&mut reference, &input, 30_000.min(4096));
        expected.extend(finish(&mut reference, 4096));
        for block in [1usize, 3, 100, 147, 1000, 4096] {
            let mut resampler = make(44_100, 48_000, Quality::High);
            let mut got = run(&mut resampler, &input, block);
            got.extend(finish(&mut resampler, block));
            assert_eq!(got.len(), expected.len(), "block {block}");
            assert!(
                got.iter()
                    .zip(&expected)
                    .all(|(a, b)| a.to_bits() == b.to_bits()),
                "block {block}"
            );
        }
    }

    #[test]
    fn a_reset_forgets_the_past() {
        let input = tone(48_000.0, 997.0, 0.5, 0, 9_000);
        let mut resampler = make(48_000, 96_000, Quality::Fast);
        let first = run(&mut resampler, &input, 1000);
        run(
            &mut resampler,
            &tone(48_000.0, 5_000.0, 0.9, 0, 3_000),
            1000,
        );
        resampler.reset();
        assert_eq!(resampler.frames_in(), 0);
        let again = run(&mut resampler, &input, 1000);
        assert_eq!(first.len(), again.len());
        assert!(first
            .iter()
            .zip(&again)
            .all(|(a, b)| a.to_bits() == b.to_bits()));
    }

    #[test]
    fn an_impulse_comes_back_where_it_went_in() {
        // The filter is zero-phase: an impulse at input n0 peaks at output n0 * L / M.
        for (from, to, n0) in [
            (48_000u32, 96_000u32, 500usize),
            (96_000, 48_000, 800),
            (44_100, 48_000, 640),
        ] {
            let mut resampler = make(from, to, Quality::High);
            let mut input = vec![0.0; 4_000];
            input[n0] = 1.0;
            let mut output = run(&mut resampler, &input, 512);
            output.extend(finish(&mut resampler, 512));
            let (peak, _) = output
                .iter()
                .enumerate()
                .max_by(|a, b| a.1.partial_cmp(b.1).unwrap())
                .unwrap();
            let expected = n0 as f64 * f64::from(to) / f64::from(from);
            assert!(
                (peak as f64 - expected).abs() <= 1.0,
                "{from} -> {to}: peak at {peak}, expected {expected}"
            );
            // When the impulse lands on an output sample the response is symmetric around it.
            if (expected - expected.round()).abs() < 1e-9 {
                let around = |d: usize| (output[peak - d] - output[peak + d]).abs();
                assert!(around(1) < 1e-9 && around(5) < 1e-9, "{from} -> {to}");
            }
        }
    }

    #[test]
    fn channels_are_independent_and_advance_together() {
        let left = tone(44_100.0, 440.0, 0.5, 0, 8_000);
        let right = tone(44_100.0, 3_000.0, 0.25, 0, 8_000);
        let mut stereo = Resampler::new(44_100, 48_000, 2, Quality::Fast, 1_000).unwrap();
        let mut mono_left = Resampler::new(44_100, 48_000, 1, Quality::Fast, 1_000).unwrap();
        let mut mono_right = Resampler::new(44_100, 48_000, 1, Quality::Fast, 1_000).unwrap();
        let mut out = vec![Vec::new(), Vec::new()];
        let mut got_left = Vec::new();
        let mut got_right = Vec::new();
        for (l, r) in left.chunks(1_000).zip(right.chunks(1_000)) {
            let wanted = stereo.output_len_for(l.len());
            out[0].resize(wanted, 0.0);
            out[1].resize(wanted, 0.0);
            let produced = stereo.process(&[l.to_vec(), r.to_vec()], l.len(), &mut out);
            got_left.extend_from_slice(&out[0][..produced]);
            got_right.extend_from_slice(&out[1][..produced]);
        }
        let want_left = run(&mut mono_left, &left, 1_000);
        let want_right = run(&mut mono_right, &right, 1_000);
        assert!(got_left
            .iter()
            .zip(&want_left)
            .all(|(a, b)| a.to_bits() == b.to_bits()));
        assert!(got_right
            .iter()
            .zip(&want_right)
            .all(|(a, b)| a.to_bits() == b.to_bits()));
    }
}
