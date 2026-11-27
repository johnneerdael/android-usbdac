//! Streaming native DSD to PCM. Input is chronological channel-interleaved
//! bytes with the oldest bit in bit 7, as accepted by `DacStream::write`.
//! This converter is always lossy; DoP carrier words are not accepted.
//!
//! A 512-tap Blackman-windowed low-pass decimates the one-bit stream by 16,
//! followed by the library's high-quality rational PCM resampler. The first
//! stage is evaluated with byte lookup tables, so no processing allocation is
//! required. The bounded input rates are DSD64, DSD128 and DSD256.
use crate::convert::{
    ConversionPlan, ConvertError, ConvertOptions, Converter, PcmFormat, Processed, SampleFormat,
};

const TAPS: usize = 512;
const HISTORY: usize = TAPS / 8;
const DELAY: usize = 16;

pub struct DsdPcmConverter {
    channels: usize,
    table: Vec<[f64; 256]>,
    history: Vec<[u8; HISTORY]>,
    head: usize,
    first: Option<[u8; 8]>,
    skipped: usize,
    pending: Vec<u8>,
    pcm: Converter,
    finishing: bool,
    tail_left: usize,
    final_partial: bool,
    input_frames: u64,
    output_frames: u64,
    input_rate: u32,
    output_rate: u32,
    output_frame_bytes: usize,
    tail_scratch: Vec<u8>,
    first_stage_gain: f64,
}

impl DsdPcmConverter {
    pub fn new(bit_rate: u32, channels: u16, target: PcmFormat) -> Result<Self, ConvertError> {
        Self::with_options(bit_rate, channels, target, &ConvertOptions::default())
    }
    pub fn with_options(
        bit_rate: u32,
        channels: u16,
        target: PcmFormat,
        options: &ConvertOptions,
    ) -> Result<Self, ConvertError> {
        if options.source_sample.is_some() {
            return Err(ConvertError::InvalidArgument(
                "PCM source layout is not valid for native DSD".into(),
            ));
        }
        if !matches!(bit_rate, 2_822_400 | 5_644_800 | 11_289_600) || !(1..=8).contains(&channels) {
            return Err(ConvertError::Unsupported(
                "native DSD supports DSD64/128/256 and 1 to 8 channels".into(),
            ));
        }
        let source = PcmFormat::new(bit_rate / 16, channels, SampleFormat::Float32);
        let plan = ConversionPlan::pcm(source, target, options)?;
        let first_stage_gain = if plan.resamples() {
            1.0
        } else {
            10f64.powf(options.headroom_db / 20.0)
        };
        let pcm = Converter::new(&plan)?;
        let tail_scratch = vec![0; pcm.min_output_bytes()];
        let mut coefficients = [0.0; TAPS];
        // Transition band is 0.020..0.03125 cycles/bit (the decimated Nyquist).
        let cutoff = (0.020 + 0.03125) / 2.0;
        let mut sum = 0.0;
        for (i, c) in coefficients.iter_mut().enumerate() {
            let x = i as f64 - (TAPS as f64 - 1.0) / 2.0;
            let sinc = (2.0 * std::f64::consts::PI * cutoff * x).sin() / (std::f64::consts::PI * x);
            let phase = 2.0 * std::f64::consts::PI * i as f64 / (TAPS - 1) as f64;
            *c = sinc * (0.42 - 0.5 * phase.cos() + 0.08 * (2.0 * phase).cos());
            sum += *c;
        }
        for c in &mut coefficients {
            *c /= sum;
        }
        let mut table = vec![[0.0; 256]; HISTORY];
        for (group, entries) in table.iter_mut().enumerate() {
            for (byte, entry) in entries.iter_mut().enumerate() {
                *entry = (0..8)
                    .map(|bit| {
                        coefficients[group * 8 + bit]
                            * if byte & (1 << bit) != 0 { 1.0 } else { -1.0 }
                    })
                    .sum();
            }
        }
        Ok(Self {
            channels: channels as usize,
            table,
            history: vec![[0x69; HISTORY]; channels as usize],
            head: 0,
            first: None,
            skipped: 0,
            pending: Vec::with_capacity(channels as usize * 4),
            pcm,
            finishing: false,
            tail_left: DELAY,
            final_partial: false,
            input_frames: 0,
            output_frames: 0,
            input_rate: bit_rate / 8,
            output_rate: target.sample_rate,
            output_frame_bytes: target.bytes_per_frame(),
            tail_scratch,
            first_stage_gain,
        })
    }
    pub fn min_output_bytes(&self) -> usize {
        self.pcm.min_output_bytes()
    }
    pub fn kind(&self) -> crate::convert::ConversionKind {
        crate::convert::ConversionKind::Lossy
    }
    pub fn clipped_samples(&self) -> u64 {
        self.pcm.clipped_samples()
    }
    pub fn delay_byte_frames(&self) -> usize {
        2 * DELAY + self.pcm.delay_frames() * 2
    }
    fn push_frame(&mut self, frame: &[u8]) {
        self.head = (self.head + 1) % HISTORY;
        for (ch, byte) in frame.iter().enumerate().take(self.channels) {
            self.history[ch][self.head] = *byte;
        }
    }
    fn synthesize(&mut self) {
        if self.skipped < DELAY {
            self.skipped += 1;
            return;
        }
        self.pending.clear();
        for channel in &self.history {
            let sample: f64 = (0..HISTORY)
                .map(|i| self.table[i][channel[(self.head + HISTORY - i) % HISTORY] as usize])
                .sum();
            self.pending
                .extend_from_slice(&((sample * self.first_stage_gain) as f32).to_le_bytes());
        }
    }
    fn emit_pending(&mut self, output: &mut [u8]) -> Result<Processed, ConvertError> {
        if self.pending.is_empty() {
            return Ok(Processed {
                consumed: 0,
                produced: 0,
            });
        }
        let done = self.pcm.process(&self.pending, output)?;
        if done.consumed == self.pending.len() {
            self.pending.clear();
        }
        Ok(done)
    }
    pub fn process(&mut self, input: &[u8], output: &mut [u8]) -> Result<Processed, ConvertError> {
        if self.finishing {
            return Err(ConvertError::InvalidState(
                "finish called; reset before processing".into(),
            ));
        }
        let mut consumed = 0;
        let mut produced = 0;
        loop {
            if !self.pending.is_empty() {
                let done = self.emit_pending(&mut output[produced..])?;
                produced += done.produced;
                if !self.pending.is_empty() {
                    break;
                }
            }
            if input.len() - consumed < self.channels {
                break;
            }
            if output.len() - produced < self.min_output_bytes() {
                break;
            }
            let frame = &input[consumed..consumed + self.channels];
            self.push_frame(frame);
            consumed += self.channels;
            if self.first.take().is_some() {
                self.synthesize();
            } else {
                self.first = Some([0; 8]);
            }
        }
        self.input_frames = self
            .input_frames
            .saturating_add((consumed / self.channels) as u64);
        self.output_frames = self
            .output_frames
            .saturating_add((produced / self.output_frame_bytes) as u64);
        Ok(Processed { consumed, produced })
    }
    pub fn finish(&mut self, output: &mut [u8]) -> Result<usize, ConvertError> {
        self.finishing = true;
        let expected = (u128::from(self.input_frames) * u128::from(self.output_rate))
            .div_ceil(u128::from(self.input_rate));
        let remaining = expected.saturating_sub(u128::from(self.output_frames));
        let cap = remaining
            .saturating_mul(self.output_frame_bytes as u128)
            .min(output.len() as u128) as usize;
        if cap < self.output_frame_bytes {
            return Ok(0);
        }
        let made = if cap < self.min_output_bytes() {
            let mut scratch = std::mem::take(&mut self.tail_scratch);
            let result = self.finish_inner(&mut scratch);
            let copied = result.as_ref().copied().unwrap_or(0).min(cap);
            output[..copied].copy_from_slice(&scratch[..copied]);
            self.tail_scratch = scratch;
            result.map(|_| copied)?
        } else {
            self.finish_inner(&mut output[..cap])?
        };
        self.output_frames = self
            .output_frames
            .saturating_add((made / self.output_frame_bytes) as u64);
        Ok(made)
    }
    fn finish_inner(&mut self, output: &mut [u8]) -> Result<usize, ConvertError> {
        let mut produced = 0;
        loop {
            if !self.pending.is_empty() {
                let done = self.emit_pending(&mut output[produced..])?;
                produced += done.produced;
                if !self.pending.is_empty() {
                    return Ok(produced);
                }
            }
            if output.len() - produced < self.min_output_bytes() {
                return Ok(produced);
            }
            if self.first.take().is_some() && !self.final_partial {
                self.push_frame(&[0x69; 8][..self.channels]);
                self.synthesize();
                self.final_partial = true;
                continue;
            }
            if self.tail_left > 0 {
                self.push_frame(&[0x69; 8][..self.channels]);
                self.push_frame(&[0x69; 8][..self.channels]);
                self.synthesize();
                self.tail_left -= 1;
                continue;
            }
            produced += self.pcm.finish(&mut output[produced..])?;
            return Ok(produced);
        }
    }
    pub fn reset(&mut self) {
        for h in &mut self.history {
            h.fill(0x69);
        }
        self.head = 0;
        self.first = None;
        self.skipped = 0;
        self.pending.clear();
        self.pcm.reset();
        self.finishing = false;
        self.tail_left = DELAY;
        self.final_partial = false;
        self.input_frames = 0;
        self.output_frames = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn converter() -> DsdPcmConverter {
        DsdPcmConverter::new(
            2_822_400,
            2,
            PcmFormat::new(44_100, 2, SampleFormat::Float32),
        )
        .unwrap()
    }
    #[test]
    fn filter_rejects_decimation_aliases_and_preserves_passband() {
        let c = converter();
        let response = |f: f64| {
            let mut re = 0.0;
            let mut im = 0.0;
            for j in 0..HISTORY {
                for bit in 0..8 {
                    let coeff = (c.table[j][1 << bit] - c.table[j][0]) / 2.0;
                    let angle = 2.0 * std::f64::consts::PI * f * (j * 8 + bit) as f64;
                    re += coeff * angle.cos();
                    im -= coeff * angle.sin();
                }
            }
            re.hypot(im)
        };
        assert!((response(0.01) - 1.0).abs() < 0.001);
        for i in 0..200 {
            let f = 0.03125 + i as f64 * (0.5 - 0.03125) / 199.0;
            assert!(response(f) < 0.001, "f={f} response={}", response(f));
        }
    }
    fn run(chunks: usize) -> Vec<u8> {
        let mut c = converter();
        let input: Vec<u8> = (0..8192)
            .map(|i| if i % 2 == 0 { 0x69 } else { 0x96 })
            .collect();
        let mut result = Vec::new();
        let mut offset = 0;
        while offset < input.len() {
            let end = (offset + chunks).min(input.len());
            let mut out = [0u8; 512];
            let done = c.process(&input[offset..end], &mut out).unwrap();
            assert!(done.consumed > 0 || done.produced > 0);
            offset += done.consumed;
            result.extend_from_slice(&out[..done.produced]);
        }
        loop {
            let mut out = [0u8; 512];
            let n = c.finish(&mut out).unwrap();
            result.extend_from_slice(&out[..n]);
            if n == 0 {
                break;
            }
        }
        assert_eq!(c.kind(), crate::convert::ConversionKind::Lossy);
        result
    }
    #[test]
    fn streaming_chunks_and_eos_preserve_phase_and_length() {
        let a = run(8192);
        let b = run(18);
        assert_eq!(a, b);
        assert_eq!(a, run(3));
        assert_eq!(a.len(), 512 * 8); // 4096 input byte-frames, DSD64 -> 44.1 kHz
    }
    #[test]
    fn rejects_unsupported_rates_and_recovers_after_reset() {
        assert!(DsdPcmConverter::new(
            22_579_200,
            2,
            PcmFormat::new(44_100, 2, SampleFormat::Float32)
        )
        .is_err());
        let mut c = converter();
        let mut out = [0u8; 128];
        let _ = c.finish(&mut out).unwrap();
        assert!(c.process(&[0x69, 0x69], &mut out).is_err());
        c.reset();
        assert!(c.process(&[0x69, 0x69], &mut out).is_ok());
    }
    #[test]
    fn channel_histories_are_independent_and_msb_is_oldest() {
        let mut c = converter();
        for _ in 0..64 {
            c.push_frame(&[0xff, 0x00]);
        }
        c.skipped = DELAY;
        c.synthesize();
        let left = f32::from_le_bytes(c.pending[0..4].try_into().unwrap());
        let right = f32::from_le_bytes(c.pending[4..8].try_into().unwrap());
        assert!((left - 1.0).abs() < 1e-6 && (right + 1.0).abs() < 1e-6);
        // Within each chronological byte, bit 0 is newest and multiplies tap 0.
        let weight_newest = (c.table[0][1] - c.table[0][0]) / 2.0;
        let weight_oldest = (c.table[0][128] - c.table[0][0]) / 2.0;
        assert!((weight_newest - weight_oldest).abs() > 1e-9);
    }
    #[test]
    fn odd_end_and_upsampling_return_exact_source_duration() {
        for target in [48_000u32, 192_000] {
            let mut c = DsdPcmConverter::new(
                2_822_400,
                1,
                PcmFormat::new(target, 1, SampleFormat::Float32),
            )
            .unwrap();
            let input = [0x69; 137];
            let mut out = [0u8; 4096];
            let done = c.process(&input, &mut out).unwrap();
            assert_eq!(done.consumed, input.len());
            let mut produced = done.produced;
            loop {
                let n = c.finish(&mut out).unwrap();
                produced += n;
                if n == 0 {
                    break;
                }
            }
            let expected = (137u64 * u64::from(target)).div_ceil(2_822_400 / 8) as usize;
            assert_eq!(produced, expected * 4, "target {target}");
        }
    }
    #[test]
    fn full_chain_preserves_audio_tone_and_suppresses_out_of_band_tone() {
        fn level(freq: f64, measured_freq: f64) -> f64 {
            let mut bits = Vec::new();
            let mut acc = 0.0;
            let mut byte = 0u8;
            for n in 0..131_072 {
                let signal =
                    0.5 * (2.0 * std::f64::consts::PI * freq * n as f64 / 2_822_400.0).sin();
                acc += signal;
                let high = acc >= 0.0;
                acc -= if high { 1.0 } else { -1.0 };
                byte = (byte << 1) | u8::from(high);
                if n % 8 == 7 {
                    bits.push(byte);
                    byte = 0;
                }
            }
            let mut c = DsdPcmConverter::new(
                2_822_400,
                1,
                PcmFormat::new(44_100, 1, SampleFormat::Float32),
            )
            .unwrap();
            let mut audio = Vec::new();
            let mut i = 0;
            let mut out = [0u8; 4096];
            while i < bits.len() {
                let done = c.process(&bits[i..], &mut out).unwrap();
                assert!(done.consumed > 0 || done.produced > 0);
                i += done.consumed;
                audio.extend_from_slice(&out[..done.produced]);
            }
            loop {
                let n = c.finish(&mut out).unwrap();
                audio.extend_from_slice(&out[..n]);
                if n == 0 {
                    break;
                }
            }
            let samples: Vec<f64> = audio
                .chunks_exact(4)
                .map(|x| f32::from_le_bytes(x.try_into().unwrap()) as f64)
                .collect();
            let mut re = 0.0;
            let mut im = 0.0;
            let body = &samples[128..samples.len() - 128];
            for (n, value) in body.iter().enumerate() {
                let phase = 2.0 * std::f64::consts::PI * measured_freq * n as f64 / 44_100.0;
                re += value * phase.cos();
                im += value * phase.sin();
            }
            2.0 * re.hypot(im) / body.len() as f64
        }
        let in_band = level(1_378.125, 1_378.125);
        let alias = level(30_000.0, 14_100.0);
        assert!(
            (0.47..0.53).contains(&in_band),
            "in-band amplitude {in_band}"
        );
        assert!(alias < 0.002, "aliased amplitude {alias}");
    }
    #[test]
    fn headroom_is_applied_once_with_and_without_pcm_resampling() {
        fn amplitude(rate: u32, db: f64) -> f64 {
            let options = ConvertOptions {
                headroom_db: db,
                ..ConvertOptions::default()
            };
            let mut c = DsdPcmConverter::with_options(
                2_822_400,
                1,
                PcmFormat::new(rate, 1, SampleFormat::Float32),
                &options,
            )
            .unwrap();
            let input = [0xff; 8192];
            let mut out = vec![0; 128 * 1024];
            let done = c.process(&input, &mut out).unwrap();
            assert_eq!(done.consumed, input.len());
            let samples: Vec<f32> = out[..done.produced]
                .chunks_exact(4)
                .map(|b| f32::from_le_bytes(b.try_into().unwrap()))
                .collect();
            samples[samples.len() / 2] as f64
        }
        let gain = 10f64.powf(-6.0 / 20.0);
        for rate in [44_100, 176_400] {
            let unity = amplitude(rate, 0.0);
            let reduced = amplitude(rate, -6.0);
            assert!(
                (reduced / unity - gain).abs() < 0.001,
                "rate {rate}: {}",
                reduced / unity
            );
        }
        let options = ConvertOptions {
            source_sample: Some(SampleFormat::I16),
            ..ConvertOptions::default()
        };
        assert!(DsdPcmConverter::with_options(
            2_822_400,
            1,
            PcmFormat::new(44_100, 1, SampleFormat::Float32),
            &options
        )
        .is_err());
    }
}
