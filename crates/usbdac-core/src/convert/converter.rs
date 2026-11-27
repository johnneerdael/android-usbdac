//! The streaming executor of a [`ConversionPlan`].
//!
//! Two engines do the work. Plans that only move integers between layouts (widening, repacking,
//! narrowing, mono to stereo) run on the integer engine: left-justified `i32` values, no floating
//! point, so a bit-exact plan is exactly that. Plans that resample, or that involve floats, run on
//! the float engine: samples decoded to `f64` (exact for every integer layout), resampled per
//! channel, duplicated if mono to stereo, and encoded with the plan's narrowing mode.
//!
//! Both work on whole frames, in blocks of at most [`BLOCK_FRAMES`], with scratch buffers made when
//! the converter is: `process`, `finish` and `reset` allocate nothing.

use super::format::{ConvertError, Narrowing, PcmFormat, SampleFormat};
use super::plan::{ConversionPlan, Step};
use super::resample::Resampler;
use super::samples::{self, Rng};

/// The most frames one pass of the float engine handles.
pub const BLOCK_FRAMES: usize = 1024;

/// What one call to [`Converter::process`] did, in bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Processed {
    /// Input bytes used: whole source frames.
    pub consumed: usize,
    /// Output bytes written: whole target frames.
    pub produced: usize,
}

struct IntegerEngine {
    source_bits: u8,
    source_slot: usize,
    target_bits: u8,
    target_slot: usize,
    channels: usize,
    duplicate: bool,
    mode: Narrowing,
    narrow: bool,
    rng: Rng,
}

impl IntegerEngine {
    fn process(&mut self, input: &[u8], output: &mut [u8], clipped: &mut u64) -> (usize, usize) {
        let source_frame = self.channels * self.source_slot;
        let target_channels = if self.duplicate { 2 } else { self.channels };
        let target_frame = target_channels * self.target_slot;
        let frames = (input.len() / source_frame).min(output.len() / target_frame);
        for frame in 0..frames {
            let source = &input[frame * source_frame..(frame + 1) * source_frame];
            let target = &mut output[frame * target_frame..(frame + 1) * target_frame];
            for channel in 0..target_channels {
                let from = if self.duplicate { 0 } else { channel };
                let mut value = samples::read_int(
                    &source[from * self.source_slot..],
                    self.source_slot,
                    self.source_bits,
                );
                if self.narrow {
                    value =
                        samples::narrow(value, self.target_bits, self.mode, &mut self.rng, clipped);
                } else {
                    value &= samples::keep_mask(self.target_bits.min(32));
                }
                samples::write_int(
                    &mut target[channel * self.target_slot..],
                    self.target_slot,
                    value,
                );
            }
        }
        (frames * source_frame, frames * target_frame)
    }
}

struct FloatEngine {
    source: SampleFormat,
    target: SampleFormat,
    channels: usize,
    duplicate: bool,
    resampler: Option<Resampler>,
    gain: f64,
    mode: Narrowing,
    rng: Rng,
    planar_in: Vec<Vec<f64>>,
    planar_out: Vec<Vec<f64>>,
    /// Zeros still to feed to flush the resampler's tail.
    pad_left: usize,
    finishing: bool,
}

impl FloatEngine {
    fn decode(&mut self, input: &[u8], frames: usize) {
        let slot = self.source.bytes_per_sample();
        let frame_bytes = slot * self.channels;
        for frame in 0..frames {
            for channel in 0..self.channels {
                let bytes = &input[frame * frame_bytes + channel * slot..];
                self.planar_in[channel][frame] = match self.source {
                    SampleFormat::Float32 => samples::read_float(bytes),
                    SampleFormat::Int { bits, slot_bytes } => {
                        samples::int_to_f64(samples::read_int(bytes, usize::from(slot_bytes), bits))
                    }
                };
            }
        }
    }

    /// Encodes `frames` frames of `planar` (from `planar_in` or `planar_out`) into `output`.
    // Keep explicit geometry and disjoint RNG/output/counter borrows on this
    // allocation-free codec path rather than bundling them into shared state.
    #[allow(clippy::too_many_arguments)]
    fn encode(
        planar: &[Vec<f64>],
        frames: usize,
        duplicate: bool,
        gain: f64,
        target: SampleFormat,
        mode: Narrowing,
        rng: &mut Rng,
        output: &mut [u8],
        clipped: &mut u64,
    ) {
        let slot = target.bytes_per_sample();
        let target_channels = if duplicate { 2 } else { planar.len() };
        let frame_bytes = slot * target_channels;
        for frame in 0..frames {
            for channel in 0..target_channels {
                let x = planar[if duplicate { 0 } else { channel }][frame] * gain;
                let bytes = &mut output[frame * frame_bytes + channel * slot..];
                match target {
                    SampleFormat::Float32 => bytes[..4].copy_from_slice(&(x as f32).to_le_bytes()),
                    SampleFormat::Int { bits, slot_bytes } => {
                        let value = samples::float_to_int(x, bits, mode, rng, clipped);
                        samples::write_int(bytes, usize::from(slot_bytes), value);
                    }
                }
            }
        }
    }

    fn process(&mut self, input: &[u8], output: &mut [u8], clipped: &mut u64) -> (usize, usize) {
        let source_frame = self.source.bytes_per_sample() * self.channels;
        let target_channels = if self.duplicate { 2 } else { self.channels };
        let target_frame = self.target.bytes_per_sample() * target_channels;
        let in_frames = input.len() / source_frame;
        let out_frames = output.len() / target_frame;
        let (mut consumed, mut produced) = (0usize, 0usize);
        while consumed < in_frames {
            let room = out_frames - produced;
            let mut n = (in_frames - consumed).min(BLOCK_FRAMES);
            n = match &self.resampler {
                Some(resampler) => n.min(resampler.max_input_for_output(room)),
                None => n.min(room),
            };
            if n == 0 {
                break;
            }
            self.decode(&input[consumed * source_frame..], n);
            produced += self.encode_block(n, &mut output[produced * target_frame..], clipped);
            consumed += n;
        }
        (consumed * source_frame, produced * target_frame)
    }

    /// Runs the `n` decoded frames of `planar_in` through the resampler (if any) and encodes the
    /// result into `output`. Returns the frames written.
    fn encode_block(&mut self, n: usize, output: &mut [u8], clipped: &mut u64) -> usize {
        let frames = match &mut self.resampler {
            Some(resampler) => resampler.process(&self.planar_in, n, &mut self.planar_out),
            None => n,
        };
        let planar = if self.resampler.is_some() {
            &self.planar_out
        } else {
            &self.planar_in
        };
        Self::encode(
            planar,
            frames,
            self.duplicate,
            self.gain,
            self.target,
            self.mode,
            &mut self.rng,
            output,
            clipped,
        );
        frames
    }

    /// Feeds the zeros that flush the resampler's tail, as far as `output` has room.
    fn finish(&mut self, output: &mut [u8], clipped: &mut u64) -> usize {
        self.finishing = true;
        if self.resampler.is_none() {
            return 0;
        }
        let target_channels = if self.duplicate { 2 } else { self.channels };
        let target_frame = self.target.bytes_per_sample() * target_channels;
        let out_frames = output.len() / target_frame;
        let mut produced = 0usize;
        while self.pad_left > 0 {
            let room = out_frames - produced;
            let allowed = self
                .resampler
                .as_ref()
                .map_or(0, |r| r.max_input_for_output(room));
            let n = self.pad_left.min(BLOCK_FRAMES).min(allowed);
            if n == 0 {
                break;
            }
            for channel in &mut self.planar_in {
                channel[..n].fill(0.0);
            }
            produced += self.encode_block(n, &mut output[produced * target_frame..], clipped);
            self.pad_left -= n;
        }
        produced * target_frame
    }

    fn reset(&mut self) {
        if let Some(resampler) = &mut self.resampler {
            resampler.reset();
            self.pad_left = resampler.pad_frames();
        }
        self.finishing = false;
    }
}

enum Engine {
    /// The formats are the same: the bytes are copied.
    Copy {
        frame_bytes: usize,
    },
    Integer(IntegerEngine),
    Float(Box<FloatEngine>),
}

/// Turns audio in the plan's source format into the target format, a chunk at a time.
///
/// `process` takes whole frames from the front of the input and writes whole frames to the output,
/// as many as fit in either; the caller keeps what was not consumed. After the last chunk call
/// [`Converter::finish`] until it returns 0 to get the resampler's tail, then [`Converter::reset`]
/// to start another stream (a seek is a reset too: the filter's history belongs to the old position).
pub struct Converter {
    plan: ConversionPlan,
    engine: Engine,
    source_frame: usize,
    target_frame: usize,
    clipped: u64,
}

impl Converter {
    pub fn new(plan: &ConversionPlan) -> Result<Self, ConvertError> {
        let source = plan.source();
        let target = plan.target();
        let options = plan.options();
        let channels = usize::from(source.channels);
        let duplicate = plan
            .steps()
            .iter()
            .any(|step| matches!(step, Step::MonoToStereo));
        let engine = if plan.steps().is_empty() {
            Engine::Copy {
                frame_bytes: source.bytes_per_frame(),
            }
        } else if !plan.resamples()
            && matches!(source.sample, SampleFormat::Int { .. })
            && matches!(target.sample, SampleFormat::Int { .. })
        {
            let (
                SampleFormat::Int {
                    bits: source_bits,
                    slot_bytes: source_slot,
                },
                SampleFormat::Int {
                    bits: target_bits,
                    slot_bytes: target_slot,
                },
            ) = (source.sample, target.sample)
            else {
                unreachable!("both are integers")
            };
            Engine::Integer(IntegerEngine {
                source_bits,
                source_slot: usize::from(source_slot),
                target_bits,
                target_slot: usize::from(target_slot),
                channels,
                duplicate,
                mode: options.narrowing,
                narrow: target_bits < source_bits,
                rng: Rng::new(options.dither_seed),
            })
        } else {
            let resampler = if plan.resamples() {
                Some(Resampler::new(
                    source.sample_rate,
                    target.sample_rate,
                    channels,
                    options.quality,
                    BLOCK_FRAMES,
                )?)
            } else {
                None
            };
            // A block of BLOCK_FRAMES produces at most ceil(BLOCK_FRAMES * L / M) frames.
            let max_out = match &resampler {
                Some(resampler) => {
                    let info = resampler.info();
                    (BLOCK_FRAMES * info.l).div_ceil(info.m) + 2
                }
                None => 0,
            };
            let pad_left = resampler.as_ref().map_or(0, Resampler::pad_frames);
            Engine::Float(Box::new(FloatEngine {
                source: source.sample,
                target: target.sample,
                channels,
                duplicate,
                resampler,
                gain: if plan.resamples() {
                    10f64.powf(options.headroom_db / 20.0)
                } else {
                    1.0
                },
                mode: options.narrowing,
                rng: Rng::new(options.dither_seed),
                planar_in: vec![vec![0.0; BLOCK_FRAMES]; channels],
                planar_out: vec![vec![0.0; max_out]; channels],
                pad_left,
                finishing: false,
            }))
        };
        Ok(Self {
            plan: plan.clone(),
            engine,
            source_frame: source.bytes_per_frame(),
            target_frame: target.bytes_per_frame(),
            clipped: 0,
        })
    }

    pub fn plan(&self) -> &ConversionPlan {
        &self.plan
    }

    pub fn source(&self) -> PcmFormat {
        self.plan.source()
    }

    pub fn target(&self) -> PcmFormat {
        self.plan.target()
    }

    /// Bytes of one source frame: the unit `process` takes.
    pub fn source_frame_bytes(&self) -> usize {
        self.source_frame
    }

    /// Bytes of one target frame: the unit `process` writes.
    pub fn target_frame_bytes(&self) -> usize {
        self.target_frame
    }

    /// Source frames the converter holds back before the output reflects them: the resampler's
    /// algorithmic latency (a few milliseconds), 0 for every other plan. Output frame `n` is
    /// aligned with source time `n * source_rate / target_rate` exactly, so a position needs no
    /// correction; only the delay between feeding audio and hearing it grows by this much.
    pub fn delay_frames(&self) -> usize {
        match &self.engine {
            Engine::Float(engine) => engine.resampler.as_ref().map_or(0, Resampler::delay_frames),
            _ => 0,
        }
    }

    /// The smallest output buffer `process` and `finish` make progress with: room for the frames one
    /// source frame can turn into (`ceil(target_rate / source_rate)` when resampling, 1 otherwise).
    /// With less, `process` can consume nothing and `finish` can return 0 before the end.
    pub fn min_output_bytes(&self) -> usize {
        let frames = match &self.engine {
            Engine::Float(engine) => engine.resampler.as_ref().map_or(1, |resampler| {
                let info = resampler.info();
                info.l.div_ceil(info.m).max(1)
            }),
            _ => 1,
        };
        frames * self.target_frame
    }

    /// Samples that exceeded full scale and were saturated, since the start or the last reset.
    pub fn clipped_samples(&self) -> u64 {
        self.clipped
    }

    /// Converts whole frames. Declared integer padding is checked before any output or state change.
    /// Nonzero padding returns `InvalidArgument`; after `finish`, call `reset` first.
    pub fn process(&mut self, input: &[u8], output: &mut [u8]) -> Result<Processed, ConvertError> {
        if let SampleFormat::Int { bits, slot_bytes } = self.plan.source().sample {
            let padding = u32::from(slot_bytes) * 8 - u32::from(bits);
            if padding > 0 {
                let slot = usize::from(slot_bytes);
                let whole = input.len() / self.source_frame * self.source_frame;
                let mask = (1u32 << padding) - 1;
                for sample in input[..whole].chunks_exact(slot) {
                    let mut word = 0u32;
                    for (i, byte) in sample.iter().enumerate() {
                        word |= u32::from(*byte) << (i * 8);
                    }
                    if word & mask != 0 {
                        return Err(ConvertError::InvalidArgument(
                            "source integer sample has nonzero declared padding bits".into(),
                        ));
                    }
                }
            }
        }
        let (consumed, produced) = match &mut self.engine {
            Engine::Copy { frame_bytes } => {
                let frames = (input.len() / *frame_bytes).min(output.len() / *frame_bytes);
                let bytes = frames * *frame_bytes;
                output[..bytes].copy_from_slice(&input[..bytes]);
                (bytes, bytes)
            }
            Engine::Integer(engine) => engine.process(input, output, &mut self.clipped),
            Engine::Float(engine) => {
                if engine.finishing {
                    return Err(ConvertError::InvalidState(
                        "finish() was called: reset() before converting more".into(),
                    ));
                }
                engine.process(input, output, &mut self.clipped)
            }
        };
        Ok(Processed { consumed, produced })
    }

    /// Writes the end of the stream: the frames the resampler still holds, as far as `output` has
    /// room. Call it until it returns 0. Nothing else follows a `finish`: [`Converter::process`]
    /// fails until [`Converter::reset`].
    pub fn finish(&mut self, output: &mut [u8]) -> Result<usize, ConvertError> {
        Ok(match &mut self.engine {
            Engine::Float(engine) => engine.finish(output, &mut self.clipped),
            _ => 0,
        })
    }

    /// Starts a new stream: the filter's history, the dither state and the counters are cleared.
    pub fn reset(&mut self) {
        match &mut self.engine {
            Engine::Float(engine) => {
                engine.reset();
                engine.rng = Rng::new(self.plan.options().dither_seed);
            }
            Engine::Integer(engine) => engine.rng = Rng::new(self.plan.options().dither_seed),
            Engine::Copy { .. } => {}
        }
        self.clipped = 0;
    }

    /// Output bytes `process` writes for `input_bytes` of input, if the output buffer is large
    /// enough, rounded down to whole frames. Exact for every plan; for a resampling one it depends
    /// on how much has been fed so far.
    pub fn output_len_for(&self, input_bytes: usize) -> usize {
        let frames = input_bytes / self.source_frame;
        let out_frames = match &self.engine {
            Engine::Float(engine) => match &engine.resampler {
                Some(resampler) => resampler.output_len_for(frames),
                None => frames,
            },
            _ => frames,
        };
        out_frames * self.target_frame
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::convert::format::{ConvertOptions, Quality};

    fn converter(source: PcmFormat, target: PcmFormat, options: ConvertOptions) -> Converter {
        Converter::new(&ConversionPlan::pcm(source, target, &options).unwrap()).unwrap()
    }

    fn fmt(rate: u32, channels: u16, sample: SampleFormat) -> PcmFormat {
        PcmFormat::new(rate, channels, sample)
    }

    /// Runs everything through in `chunk`-byte pieces with an output buffer of `room` bytes.
    fn convert_all(c: &mut Converter, input: &[u8], chunk: usize, room: usize) -> Vec<u8> {
        let mut result = Vec::new();
        let mut buffer = vec![0u8; room];
        let mut pending: &[u8] = &[];
        let mut rest = input;
        let chunk = (chunk / c.source_frame_bytes()).max(1) * c.source_frame_bytes();
        loop {
            if pending.is_empty() && !rest.is_empty() {
                let take = chunk.min(rest.len());
                pending = &rest[..take];
                rest = &rest[take..];
            }
            if pending.is_empty() {
                break;
            }
            let done = c.process(pending, &mut buffer).unwrap();
            result.extend_from_slice(&buffer[..done.produced]);
            pending = &pending[done.consumed..];
            if done.consumed == 0 && done.produced == 0 {
                // A partial frame at the end of the stream: nothing more to do.
                assert!(
                    pending.len() < c.source_frame_bytes(),
                    "stuck with {} bytes",
                    pending.len()
                );
                break;
            }
        }
        loop {
            let n = c.finish(&mut buffer).unwrap();
            if n == 0 {
                break;
            }
            result.extend_from_slice(&buffer[..n]);
        }
        result
    }

    fn i16_bytes(values: &[i16]) -> Vec<u8> {
        values.iter().flat_map(|v| v.to_le_bytes()).collect()
    }

    #[test]
    fn identical_formats_copy_the_bytes() {
        let mut c = converter(
            fmt(44_100, 2, SampleFormat::I16),
            fmt(44_100, 2, SampleFormat::I16),
            ConvertOptions::default(),
        );
        let input = i16_bytes(&[1, -2, 3, -4, 5, -6]);
        assert_eq!(convert_all(&mut c, &input, 4, 64), input);
        assert_eq!(c.delay_frames(), 0);
    }

    #[test]
    fn widening_16_to_24_and_32_is_exact_with_zero_filled_low_bits() {
        let input = i16_bytes(&[0x1234, -2, 0, i16::MAX, i16::MIN, 1]);
        let mut to24 = converter(
            fmt(44_100, 1, SampleFormat::I16),
            fmt(44_100, 1, SampleFormat::I24),
            ConvertOptions::default(),
        );
        let out = convert_all(&mut to24, &input, 100, 100);
        assert_eq!(&out[..3], &[0x00, 0x34, 0x12]);
        assert_eq!(&out[3..6], &[0x00, 0xfe, 0xff]);
        assert_eq!(&out[9..12], &[0x00, 0xff, 0x7f]); // i16::MAX
        assert_eq!(&out[12..15], &[0x00, 0x00, 0x80]); // i16::MIN

        let mut to32 = converter(
            fmt(44_100, 1, SampleFormat::I16),
            fmt(44_100, 1, SampleFormat::I32),
            ConvertOptions::default(),
        );
        let out = convert_all(&mut to32, &input, 100, 100);
        assert_eq!(&out[..4], &[0x00, 0x00, 0x34, 0x12]);
        let values: Vec<i32> = out
            .chunks_exact(4)
            .map(|b| i32::from_le_bytes([b[0], b[1], b[2], b[3]]))
            .collect();
        let expected: Vec<i32> = [0x1234i16, -2, 0, i16::MAX, i16::MIN, 1]
            .iter()
            .map(|v| i32::from(*v) << 16)
            .collect();
        assert_eq!(values, expected);
    }

    #[test]
    fn packed_24_and_24_in_4_repack_bit_exactly_both_ways() {
        // Every pattern of a 24-bit sample that matters: walk the 24-bit range in a prime stride.
        let values: Vec<i32> = (0..50_000i64)
            .map(|i| (((i * 99_991) % 16_777_216) - 8_388_608) as i32)
            .collect();
        let packed: Vec<u8> = values
            .iter()
            .flat_map(|v| {
                [
                    (*v & 0xff) as u8,
                    ((*v >> 8) & 0xff) as u8,
                    ((*v >> 16) & 0xff) as u8,
                ]
            })
            .collect();
        let mut up = converter(
            fmt(96_000, 1, SampleFormat::I24),
            fmt(96_000, 1, SampleFormat::I24_IN_32),
            ConvertOptions::default(),
        );
        let wide = convert_all(&mut up, &packed, 3000, 4000);
        assert_eq!(wide.len(), values.len() * 4);
        for (i, v) in values.iter().enumerate() {
            let word = i32::from_le_bytes([
                wide[i * 4],
                wide[i * 4 + 1],
                wide[i * 4 + 2],
                wide[i * 4 + 3],
            ]);
            assert_eq!(word, v << 8, "sample {i}");
        }
        let mut down = converter(
            fmt(96_000, 1, SampleFormat::I24_IN_32),
            fmt(96_000, 1, SampleFormat::I24),
            ConvertOptions::default(),
        );
        assert_eq!(convert_all(&mut down, &wide, 4000, 3000), packed);
    }

    #[test]
    fn mono_to_stereo_duplicates_every_sample() {
        let mut c = converter(
            fmt(48_000, 1, SampleFormat::I16),
            fmt(48_000, 2, SampleFormat::I16),
            ConvertOptions::default(),
        );
        let out = convert_all(&mut c, &i16_bytes(&[10, -20, 30]), 6, 64);
        assert_eq!(out, i16_bytes(&[10, 10, -20, -20, 30, 30]));
        let mut widened = converter(
            fmt(48_000, 1, SampleFormat::I16),
            fmt(48_000, 2, SampleFormat::I24),
            ConvertOptions::default(),
        );
        let out = convert_all(&mut widened, &i16_bytes(&[0x0102]), 2, 64);
        assert_eq!(out, [0x00, 0x02, 0x01, 0x00, 0x02, 0x01]);
    }

    #[test]
    fn narrowing_truncates_or_dithers_as_asked() {
        let input: Vec<u8> = [0x1234_5678i32, -0x0000_ffff, 0x7fff_ffff]
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .collect();
        let truncate = ConvertOptions {
            narrowing: Narrowing::Truncate,
            ..ConvertOptions::default()
        };
        let mut c = converter(
            fmt(44_100, 1, SampleFormat::I32),
            fmt(44_100, 1, SampleFormat::I16),
            truncate,
        );
        let out = convert_all(&mut c, &input, 100, 100);
        let values: Vec<i16> = out
            .chunks_exact(2)
            .map(|b| i16::from_le_bytes([b[0], b[1]]))
            .collect();
        assert_eq!(values, [0x1234, -1, 0x7fff]);

        let mut dithered = converter(
            fmt(44_100, 1, SampleFormat::I32),
            fmt(44_100, 1, SampleFormat::I16),
            ConvertOptions::default(),
        );
        let out = convert_all(&mut dithered, &input, 100, 100);
        let values: Vec<i16> = out
            .chunks_exact(2)
            .map(|b| i16::from_le_bytes([b[0], b[1]]))
            .collect();
        assert!((i32::from(values[0]) - 0x1234).abs() <= 2);
        assert!(i32::from(values[1]).abs() <= 2);
        assert!(i32::from(values[2]) >= 0x7ffe, "{}", values[2]);
    }

    #[test]
    fn integers_become_floats_exactly_and_floats_come_back_with_truncation() {
        let ints: Vec<i16> = (-32768..=32767).step_by(7).map(|v| v as i16).collect();
        let mut up = converter(
            fmt(44_100, 1, SampleFormat::I16),
            fmt(44_100, 1, SampleFormat::Float32),
            ConvertOptions::default(),
        );
        let floats = convert_all(&mut up, &i16_bytes(&ints), 1000, 1000);
        for (i, v) in ints.iter().enumerate() {
            let x = f32::from_le_bytes([
                floats[i * 4],
                floats[i * 4 + 1],
                floats[i * 4 + 2],
                floats[i * 4 + 3],
            ]);
            assert_eq!(f64::from(x), f64::from(*v) / 32768.0, "sample {i}");
        }
        let truncate = ConvertOptions {
            narrowing: Narrowing::Truncate,
            ..ConvertOptions::default()
        };
        let mut down = converter(
            fmt(44_100, 1, SampleFormat::Float32),
            fmt(44_100, 1, SampleFormat::I16),
            truncate,
        );
        assert_eq!(
            convert_all(&mut down, &floats, 1000, 1000),
            i16_bytes(&ints)
        );
    }

    #[test]
    fn a_24_bit_source_survives_a_float_round_trip() {
        let values: Vec<i32> = (0..20_000i64)
            .map(|i| (((i * 99_991) % 16_777_216) - 8_388_608) as i32)
            .collect();
        let packed: Vec<u8> = values
            .iter()
            .flat_map(|v| {
                [
                    (*v & 0xff) as u8,
                    ((*v >> 8) & 0xff) as u8,
                    ((*v >> 16) & 0xff) as u8,
                ]
            })
            .collect();
        let truncate = ConvertOptions {
            narrowing: Narrowing::Truncate,
            ..ConvertOptions::default()
        };
        let mut up = converter(
            fmt(96_000, 1, SampleFormat::I24),
            fmt(96_000, 1, SampleFormat::Float32),
            truncate,
        );
        let floats = convert_all(&mut up, &packed, 3000, 4000);
        let mut down = converter(
            fmt(96_000, 1, SampleFormat::Float32),
            fmt(96_000, 1, SampleFormat::I24),
            truncate,
        );
        assert_eq!(
            convert_all(&mut down, &floats, 4000, 3000),
            packed,
            "a float holds 24 bits exactly"
        );
    }

    #[test]
    fn non_finite_floats_are_silence_and_full_scale_saturates_with_a_count() {
        let input: Vec<u8> = [f32::NAN, f32::INFINITY, 2.0, -2.0, 0.5]
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .collect();
        let mut c = converter(
            fmt(44_100, 1, SampleFormat::Float32),
            fmt(44_100, 1, SampleFormat::I16),
            ConvertOptions::default(),
        );
        let out = convert_all(&mut c, &input, 100, 100);
        let values: Vec<i16> = out
            .chunks_exact(2)
            .map(|b| i16::from_le_bytes([b[0], b[1]]))
            .collect();
        assert!(values[0].abs() <= 1 && values[1].abs() <= 1, "{values:?}");
        assert!(values[2] >= 32766 && values[3] <= -32767, "{values:?}");
        assert!((i32::from(values[4]) - 16384).abs() <= 1);
        assert_eq!(c.clipped_samples(), 2);
        c.reset();
        assert_eq!(c.clipped_samples(), 0);
    }

    #[test]
    fn chunking_and_a_tight_output_buffer_change_nothing() {
        let input: Vec<u8> = (0..40_000i32)
            .flat_map(|i| (((i * 37) % 60_000 - 30_000) as i16).to_le_bytes())
            .collect();
        for (source, target, mono) in [
            (
                fmt(44_100, 2, SampleFormat::I16),
                fmt(48_000, 2, SampleFormat::I24),
                false,
            ),
            (
                fmt(44_100, 1, SampleFormat::I16),
                fmt(48_000, 2, SampleFormat::I16),
                true,
            ),
            (
                fmt(44_100, 2, SampleFormat::I16),
                fmt(44_100, 2, SampleFormat::I24_IN_32),
                false,
            ),
        ] {
            let channels = usize::from(source.channels);
            let usable = input.len() / (2 * channels) * (2 * channels);
            let whole = {
                let mut c = converter(source, target, ConvertOptions::default());
                convert_all(&mut c, &input[..usable], usable, 1 << 20)
            };
            for (chunk, room) in [
                (2 * channels, 64),
                (1000 * channels, 3),
                (4096, 1000),
                (7 * 2 * channels, 123),
            ] {
                let mut c = converter(source, target, ConvertOptions::default());
                let room = room.max(c.min_output_bytes());
                let got = convert_all(&mut c, &input[..usable], chunk, room);
                assert_eq!(
                    got, whole,
                    "{source:?} -> {target:?}, chunk {chunk}, room {room}, mono {mono}"
                );
            }
        }
    }

    #[test]
    fn a_resampling_converter_produces_the_exact_length_and_reports_its_delay() {
        let source = fmt(44_100, 2, SampleFormat::I16);
        let target = fmt(48_000, 2, SampleFormat::I16);
        let mut c = converter(source, target, ConvertOptions::default());
        assert!(c.delay_frames() > 0);
        assert_eq!(
            c.delay_frames(),
            80,
            "158 taps round up to 160: 80 frames held back"
        );
        let frames = 10_000usize;
        let input = vec![0u8; frames * 4];
        let out = convert_all(&mut c, &input, 4 * 333, 4 * 500);
        assert_eq!(out.len(), (frames * 160).div_ceil(147) * 4);
    }

    #[test]
    fn processing_after_finish_needs_a_reset_and_a_reset_gives_the_same_stream_again() {
        let source = fmt(48_000, 1, SampleFormat::I16);
        let target = fmt(44_100, 1, SampleFormat::I16);
        let input: Vec<u8> = (0..6_000i32)
            .flat_map(|i| (((i * 91) % 20_000 - 10_000) as i16).to_le_bytes())
            .collect();
        let mut c = converter(source, target, ConvertOptions::default());
        let first = convert_all(&mut c, &input, 1000, 4096);
        let mut sink = vec![0u8; 64];
        assert_eq!(
            c.process(&input, &mut sink).unwrap_err().code(),
            "invalid_state"
        );
        c.reset();
        let second = convert_all(&mut c, &input, 777, 999);
        assert_eq!(
            first, second,
            "reset restores the filter, the counters and the dither"
        );
    }

    #[test]
    fn output_len_for_matches_what_process_writes() {
        let mut c = converter(
            fmt(44_100, 2, SampleFormat::I16),
            fmt(96_000, 2, SampleFormat::I24),
            ConvertOptions::default(),
        );
        let input = vec![0u8; 4 * 1000];
        let mut buffer = vec![0u8; 1 << 20];
        for _ in 0..5 {
            let predicted = c.output_len_for(input.len());
            let done = c.process(&input, &mut buffer).unwrap();
            assert_eq!(done.produced, predicted);
        }
    }

    #[test]
    fn a_resampled_sine_keeps_its_frequency_level_and_stays_clean() {
        // 1 kHz at -6 dBFS from 44.1 kHz 16-bit to 48 kHz 24-bit.
        let frames = 44_100usize;
        let input: Vec<u8> = (0..frames)
            .flat_map(|n| {
                let v = (0.5
                    * (2.0 * std::f64::consts::PI * 1000.0 * n as f64 / 44_100.0).sin()
                    * 32767.0)
                    .round() as i16;
                v.to_le_bytes()
            })
            .collect();
        let mut c = converter(
            fmt(44_100, 1, SampleFormat::I16),
            fmt(48_000, 1, SampleFormat::I24),
            ConvertOptions {
                headroom_db: 0.0,
                ..ConvertOptions::default()
            },
        );
        let out = convert_all(&mut c, &input, 4096, 8192);
        let samples: Vec<f64> = out
            .chunks_exact(3)
            .map(|b| f64::from(i32::from_le_bytes([0, b[0], b[1], b[2]]) >> 8) / 8_388_608.0)
            .collect();
        assert_eq!(samples.len(), (frames * 160).div_ceil(147));
        // A whole number of 48-sample periods, so the projections below are orthogonal.
        let len = (samples.len() - 4000) / 48 * 48;
        let middle = &samples[2000..2000 + len];
        let peak = middle.iter().fold(0.0f64, |m, v| m.max(v.abs()));
        assert!((peak - 0.5).abs() < 0.001, "peak {peak}");
        // Everything except the sine is below -90 dB (the input's own 16-bit rounding is at about -98 dB).
        let w = 2.0 * std::f64::consts::PI * 1000.0 / 48_000.0;
        let (mut a, mut b) = (0.0, 0.0);
        for (i, y) in middle.iter().enumerate() {
            a += y * (w * i as f64).sin();
            b += y * (w * i as f64).cos();
        }
        let n = middle.len() as f64;
        let (a, b) = (2.0 * a / n, 2.0 * b / n);
        let residual = middle
            .iter()
            .enumerate()
            .map(|(i, y)| y - (a * (w * i as f64).sin() + b * (w * i as f64).cos()))
            .map(|e| e * e)
            .sum::<f64>()
            / n;
        let noise_db = 10.0 * (residual / (0.5f64 * 0.5 / 2.0)).log10();
        assert!(noise_db < -90.0, "noise at {noise_db} dB");
    }

    #[test]
    fn a_full_scale_square_wave_overshoots_and_saturates_without_wrapping_and_headroom_prevents_it()
    {
        let frames = 20_000usize;
        let input: Vec<u8> = (0..frames)
            .flat_map(|n| {
                let v: i16 = if (n / 22) % 2 == 0 {
                    i16::MAX
                } else {
                    i16::MIN
                };
                v.to_le_bytes()
            })
            .collect();
        let source = fmt(44_100, 1, SampleFormat::I16);
        let target = fmt(48_000, 1, SampleFormat::I16);

        let mut plain = converter(
            source,
            target,
            ConvertOptions {
                headroom_db: 0.0,
                ..ConvertOptions::default()
            },
        );
        let out = convert_all(&mut plain, &input, 4096, 8192);
        let values: Vec<i16> = out
            .chunks_exact(2)
            .map(|b| i16::from_le_bytes([b[0], b[1]]))
            .collect();
        assert!(
            plain.clipped_samples() > 0,
            "the Gibbs overshoot of a full-scale square wave exceeds full scale"
        );
        // The same signal as floats is never saturated, so it shows what the integers should be:
        // the float value rounded, and held at the rails where it goes past them. A wrap would
        // put a sample on the other side of zero.
        let mut reference = converter(
            source,
            fmt(48_000, 1, SampleFormat::Float32),
            ConvertOptions {
                headroom_db: 0.0,
                ..ConvertOptions::default()
            },
        );
        let floats = convert_all(&mut reference, &input, 4096, 8192);
        assert_eq!(floats.len() / 4, values.len());
        let mut beyond = 0;
        for (i, (value, float)) in values.iter().zip(floats.chunks_exact(4)).enumerate() {
            let x = f64::from(f32::from_le_bytes([float[0], float[1], float[2], float[3]]));
            if x > 1.0 {
                beyond += 1;
            }
            let expected = (x * 32768.0).round().clamp(-32768.0, 32767.0);
            assert!(
                (f64::from(*value) - expected).abs() <= 2.0,
                "sample {i}: {value} for a float of {x}"
            );
        }
        assert!(beyond > 0, "the float conversion shows the overshoot");

        let overshoot = floats
            .chunks_exact(4)
            .map(|b| f64::from(f32::from_le_bytes([b[0], b[1], b[2], b[3]])).abs())
            .fold(0.0f64, f64::max);
        println!(
            "full-scale square wave, 44.1 -> 48 kHz: peak {overshoot:.4} = {:+.2} dB",
            20.0 * overshoot.log10()
        );
        assert!(overshoot > 1.2 && overshoot < 1.6, "{overshoot}");

        let mut default_guard = converter(source, target, ConvertOptions::default());
        convert_all(&mut default_guard, &input, 4096, 8192);
        assert_eq!(default_guard.clipped_samples(), 0);
        let headroom = ConvertOptions {
            headroom_db: -3.5,
            ..ConvertOptions::default()
        };
        let mut guarded = converter(source, target, headroom);
        convert_all(&mut guarded, &input, 4096, 8192);
        assert_eq!(
            guarded.clipped_samples(),
            0,
            "3.5 dB of headroom covers the overshoot"
        );
    }

    #[test]
    fn a_full_scale_sine_is_not_clipped_with_a_float_or_a_truncating_16_bit_target() {
        // Within the passband the filter's ripple is about 3e-6, less than one 16-bit step at full scale.
        for hz in [100.0, 1_000.0, 10_000.0, 19_000.0] {
            // The tone fades in and out over 2000 frames: a tone cut off abruptly would ring at the cut.
            let tone = |scale: f64| -> Vec<f64> {
                (0..20_000)
                    .map(|n| {
                        let edge = n.min(19_999 - n).min(2_000) as f64 / 2_000.0;
                        let fade = 0.5 - 0.5 * (std::f64::consts::PI * edge).cos();
                        scale
                            * fade
                            * (2.0 * std::f64::consts::PI * hz * f64::from(n) / 44_100.0).sin()
                    })
                    .collect()
            };
            // A float target, at amplitude 1.0.
            let input: Vec<u8> = tone(1.0)
                .iter()
                .flat_map(|v| (*v as f32).to_le_bytes())
                .collect();
            let mut c = converter(
                fmt(44_100, 1, SampleFormat::Float32),
                fmt(96_000, 1, SampleFormat::Float32),
                ConvertOptions {
                    headroom_db: 0.0,
                    ..ConvertOptions::default()
                },
            );
            let out = convert_all(&mut c, &input, 4096, 8192);
            let settled = &out[4 * 1000..out.len() - 4 * 1000];
            let peak = settled
                .chunks_exact(4)
                .map(|b| f64::from(f32::from_le_bytes([b[0], b[1], b[2], b[3]])).abs())
                .fold(0.0f64, f64::max);
            assert!(peak <= 1.00001, "{hz} Hz overshoots to {peak}");
            assert!(peak >= 0.9999, "{hz} Hz is down to {peak}");

            // A 16-bit source at its largest sine, 32767 / 32768, to a 16-bit target.
            let ints: Vec<u8> = tone(32767.0 / 32768.0)
                .iter()
                .flat_map(|v| ((v * 32768.0).round() as i16).to_le_bytes())
                .collect();
            let truncate = ConvertOptions {
                narrowing: Narrowing::Truncate,
                ..ConvertOptions {
                    headroom_db: 0.0,
                    ..ConvertOptions::default()
                }
            };
            let mut c = converter(
                fmt(44_100, 1, SampleFormat::I16),
                fmt(96_000, 1, SampleFormat::I16),
                truncate,
            );
            convert_all(&mut c, &ints, 4096, 8192);
            assert_eq!(c.clipped_samples(), 0, "{hz} Hz clipped");
        }
    }

    #[test]
    fn the_fast_preset_is_cheaper() {
        let plan = |quality| {
            converter(
                fmt(48_000, 2, SampleFormat::I16),
                fmt(44_100, 2, SampleFormat::I16),
                ConvertOptions {
                    quality,
                    ..ConvertOptions::default()
                },
            )
        };
        assert!(plan(Quality::Fast).delay_frames() * 2 < plan(Quality::High).delay_frames());
    }
}
