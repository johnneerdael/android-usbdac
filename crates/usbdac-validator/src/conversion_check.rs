//! Independent checks of the production playback stream's accepted target bytes.
use super::*;
use std::collections::VecDeque;

/// A bounded-cost verifier: sample-preserving conversions compare normalized
/// integer values; lossy conversions use an independent sinusoidal projection.
pub(super) struct ConversionCheck {
    source: StreamRequest,
    target: StreamFormat,
    preserve_samples: bool,
    pending: Vec<u8>,
    frames: u64,
    checked: u64,
    mismatches: u64,
    sin: Vec<f64>,
    cos: Vec<f64>,
    power: Vec<f64>,
    stride: u64,
    source_pcm_pending: Vec<u8>,
    source_pcm_frames: u64,
    source_pcm_mismatches: u64,
    expected_samples: VecDeque<f64>,
    observed_samples: VecDeque<f64>,
    queue_overflow: bool,
    expected_amplitude: f64,
    expected_headroom_db: f64,
    source_dsd: DsdDigest,
    target_dsd: DsdDigest,
    /// Per-channel audio-band low-pass for DSD sources, applied before the tone projection.
    audio_band: Option<(f64, Vec<AudioBand>)>,
}

impl ConversionCheck {
    pub(super) fn new(
        source: StreamRequest,
        target: StreamFormat,
        preserve_samples: bool,
        headroom_db: f64,
    ) -> Self {
        let channels = usize::from(target.channels);
        let stride = if preserve_samples {
            1
        } else {
            (u64::from(target.sample_rate) / 48_000).max(1)
        };
        let expected_amplitude = source_fundamental(source) * 10f64.powf(headroom_db / 20.0);
        let expand_mono = source.channels == 1 && target.channels == 2;
        // DSD carries its modulation noise above the audio band by design; the validator's
        // first-order test signal puts most of its power there at DSD64 and DSD128. Judge the
        // tone within the audio band so that noise does not count against a correct conversion.
        let audio_band = (!preserve_samples
            && matches!(source.encoding, Encoding::DsdNative { .. } | Encoding::DoP))
        .then(|| {
            let cutoff = AUDIO_BAND_HZ.min(0.45 * f64::from(target.sample_rate));
            let filters = (0..channels)
                .map(|_| AudioBand::new(cutoff, f64::from(target.sample_rate)))
                .collect();
            (cutoff, filters)
        });
        let target_dsd = DsdDigest::new(
            target.encoding,
            target.channels,
            usize::from(target.subslot_bytes),
            false,
            false,
        );
        Self {
            source,
            target,
            preserve_samples,
            pending: Vec::new(),
            frames: 0,
            checked: 0,
            mismatches: 0,
            sin: vec![0.0; channels],
            cos: vec![0.0; channels],
            power: vec![0.0; channels],
            stride,
            source_pcm_pending: Vec::new(),
            source_pcm_frames: 0,
            source_pcm_mismatches: 0,
            expected_samples: VecDeque::new(),
            observed_samples: VecDeque::new(),
            queue_overflow: false,
            expected_amplitude,
            expected_headroom_db: headroom_db,
            source_dsd: DsdDigest::new(
                source.encoding,
                source.channels,
                if source.bit_depth == 32 { 4 } else { 3 },
                true,
                expand_mono,
            ),
            target_dsd,
            audio_band,
        }
    }
    /// Feed exactly the source bytes accepted by production playback.
    pub(super) fn source(&mut self, data: &[u8]) {
        self.source_dsd.add(data);
        if !self.preserve_samples
            || !matches!(
                self.source.encoding,
                Encoding::Pcm | Encoding::PcmU8 | Encoding::PcmFloat32
            )
        {
            return;
        }
        let slot = match self.source.encoding {
            Encoding::PcmU8 => 1,
            Encoding::PcmFloat32 => 4,
            _ => usize::from(self.source.bit_depth) / 8,
        };
        let channels = usize::from(self.source.channels);
        let frame_len = slot * channels;
        if frame_len == 0 {
            self.source_pcm_mismatches += 1;
            return;
        }
        self.source_pcm_pending.extend_from_slice(data);
        let complete = self.source_pcm_pending.len() / frame_len * frame_len;
        for frame in self.source_pcm_pending[..complete].chunks_exact(frame_len) {
            for target_ch in 0..usize::from(self.target.channels) {
                let ch = if channels == 1 { 0 } else { target_ch };
                if ch >= channels {
                    self.source_pcm_mismatches += 1;
                    continue;
                }
                match decode_pcm(
                    &frame[ch * slot..(ch + 1) * slot],
                    self.source.encoding,
                    self.source.bit_depth,
                ) {
                    Some(value) if self.expected_samples.len() < 65_536 => {
                        self.expected_samples.push_back(value)
                    }
                    _ => {
                        self.source_pcm_mismatches += 1;
                        self.queue_overflow = true;
                    }
                }
            }
            self.source_pcm_frames += 1;
        }
        self.source_pcm_pending.drain(..complete);
        self.match_samples();
    }
    fn match_samples(&mut self) {
        self.mismatches +=
            compare_queued_samples(&mut self.expected_samples, &mut self.observed_samples);
    }
    pub(super) fn observe(&mut self, bytes: &[u8]) {
        if matches!(
            self.target.encoding,
            Encoding::DsdNative { .. } | Encoding::DoP
        ) {
            self.target_dsd.add(bytes);
            return;
        }
        let frame_bytes = self.target.bytes_per_frame();
        if frame_bytes == 0 {
            return;
        }
        self.pending.extend_from_slice(bytes);
        let complete = self.pending.len() / frame_bytes * frame_bytes;
        let slot = usize::from(self.target.subslot_bytes);
        let channels = usize::from(self.target.channels);
        for frame in self.pending[..complete].chunks_exact(frame_bytes) {
            let frame_index = self.frames;
            self.frames += 1;
            // Exclude the start transient from filter-based tone measurements.
            let measured = frame_index.is_multiple_of(self.stride)
                && (self.preserve_samples
                    || frame_index >= u64::from(self.target.sample_rate) / 50);
            if !measured {
                // The audio-band filter runs on every frame so the stride cannot alias noise.
                if let Some((_, filters)) = &mut self.audio_band {
                    for (ch, filter) in filters.iter_mut().enumerate() {
                        if let Some(value) = decode_pcm(
                            &frame[ch * slot..(ch + 1) * slot],
                            self.target.encoding,
                            self.target.valid_bits,
                        ) {
                            filter.process(value);
                        }
                    }
                }
                continue;
            }
            let phase = std::f64::consts::TAU * TONE_HZ * frame_index as f64
                / self.target.sample_rate as f64;
            let (s, c) = phase.sin_cos();
            let mut valid = true;
            for ch in 0..channels {
                let Some(mut value) = decode_pcm(
                    &frame[ch * slot..(ch + 1) * slot],
                    self.target.encoding,
                    self.target.valid_bits,
                ) else {
                    valid = false;
                    continue;
                };
                if let Some((_, filters)) = &mut self.audio_band {
                    value = filters[ch].process(value);
                }
                if self.preserve_samples {
                    if self.observed_samples.len() < 65_536 {
                        self.observed_samples.push_back(value);
                    } else {
                        self.queue_overflow = true;
                    }
                }
                self.sin[ch] += value * s;
                self.cos[ch] += value * c;
                self.power[ch] += value * value;
            }
            if valid {
                self.checked += 1;
            } else {
                self.mismatches += 1;
            }
        }
        self.pending.drain(..complete);
        self.match_samples();
    }
    pub(super) fn result(&self, source_frames: u64, source_rate: u32) -> Value {
        if matches!(
            self.target.encoding,
            Encoding::DsdNative { .. } | Encoding::DoP
        ) {
            let source_digest = format!("{:x}", self.source_dsd.hash.clone().finalize());
            let target_digest = format!("{:x}", self.target_dsd.hash.clone().finalize());
            let expected_bytes = match self.source.encoding {
                Encoding::DsdNative { .. } => source_frames * u64::from(self.target.channels),
                Encoding::DoP => source_frames * 2 * u64::from(self.target.channels),
                _ => 0,
            };
            let passed = expected_bytes > 0
                && self.source_dsd.bytes == expected_bytes
                && self.source_dsd.bytes == self.target_dsd.bytes
                && source_digest == target_digest
                && self.source_dsd.pending.is_empty()
                && self.target_dsd.pending.is_empty()
                && self.source_dsd.invalid_markers == 0;
            return json!({"method":"complete normalized chronological DSD payload SHA-256 comparison", "passed":passed,
                "expected_bytes":expected_bytes, "source_bytes":self.source_dsd.bytes, "target_bytes":self.target_dsd.bytes,
                "source_sha256":source_digest, "target_sha256":target_digest, "source_invalid_markers":self.source_dsd.invalid_markers,
                "source_trailing_bytes":self.source_dsd.pending.len(), "target_trailing_bytes":self.target_dsd.pending.len(), "target_markers":"not_observed_driver_rewrites_after_tap"});
        }
        let expected_frames = (u128::from(source_frames) * u128::from(self.target.sample_rate))
            .div_ceil(u128::from(source_rate.max(1))) as u64;
        let frame_error = self.frames.abs_diff(expected_frames);
        let duration_ok = frame_error <= 16;
        let mut channels = Vec::new();
        let mut tone_ok = self.checked >= 64;
        for ch in 0..self.sin.len() {
            let n = self.checked.max(1) as f64;
            let (amplitude, rms, coherent) =
                tone_metrics(self.sin[ch], self.cos[ch], self.power[ch], n);
            let source_ch = if self.source.channels == 1 { 0 } else { ch };
            let polarity = if source_ch & 1 == 0 { 1.0 } else { -1.0 };
            // Relative channel polarity is phase-independent, including filter delay.
            let sign = if ch == 0 {
                true
            } else {
                let expected_sign = if self.source.channels == 1 || ch & 1 == 0 {
                    1.0
                } else {
                    -1.0
                };
                expected_sign * (self.sin[0] * self.sin[ch] + self.cos[0] * self.cos[ch]) > 0.0
            };
            let level_tolerance = self.expected_amplitude * 0.15 + 0.0001;
            let ok = (amplitude - self.expected_amplitude).abs() <= level_tolerance
                && coherent >= 0.65
                && sign;
            tone_ok &= ok;
            channels.push(json!({"channel":ch, "amplitude":amplitude, "rms":rms, "tone_coherence":coherent, "relative_polarity_ok":sign, "expected_polarity":polarity, "passed":ok}));
        }
        let passed = duration_ok
            && self.pending.is_empty()
            && self.checked >= 64
            && if self.preserve_samples {
                self.mismatches == 0
                    && self.source_pcm_mismatches == 0
                    && self.source_pcm_pending.is_empty()
                    && self.source_pcm_frames == self.frames
                    && self.expected_samples.is_empty()
                    && self.observed_samples.is_empty()
                    && !self.queue_overflow
            } else {
                tone_ok
            };
        json!({"method":if self.preserve_samples {"normalized sample equality"} else {"independent analytic tone projection"},
            "passed":passed, "frames":self.frames, "checked_frames":self.checked, "stride":self.stride,
            "duration_expected_frames":expected_frames, "duration_tolerance_frames":16, "duration_ok":duration_ok,
            "sample_comparison":"strict normalized value equality", "sample_mismatches":self.mismatches, "unmatched_source_samples":self.expected_samples.len(), "unmatched_target_samples":self.observed_samples.len(), "queue_overflow":self.queue_overflow, "source_reference_frames":self.source_pcm_frames, "source_reference_mismatches":self.source_pcm_mismatches, "tone_hz":TONE_HZ,
            "expected_amplitude":self.expected_amplitude, "headroom_db":self.expected_headroom_db, "amplitude_tolerance":"15 percent plus 0.0001", "minimum_tone_coherence":0.65, "tone_band_limit_hz":self.audio_band.as_ref().map(|(cutoff, _)| *cutoff), "channels":channels, "trailing_bytes":self.pending.len()})
    }
}

/// Upper edge of the audio band used to judge DSD-sourced tones.
const AUDIO_BAND_HZ: f64 = 20_000.0;

/// Fourth-order Butterworth low-pass as two cookbook biquads; unity gain at the test tone.
struct AudioBand {
    stages: [Biquad; 2],
}
impl AudioBand {
    fn new(cutoff: f64, rate: f64) -> Self {
        Self {
            stages: [
                Biquad::low_pass(cutoff, rate, 0.541_196_100_146_197),
                Biquad::low_pass(cutoff, rate, 1.306_562_964_876_377),
            ],
        }
    }
    fn process(&mut self, x: f64) -> f64 {
        self.stages.iter_mut().fold(x, |x, stage| stage.process(x))
    }
}
struct Biquad {
    b: [f64; 3],
    a: [f64; 2],
    z: [f64; 2],
}
impl Biquad {
    fn low_pass(cutoff: f64, rate: f64, q: f64) -> Self {
        let (sin, cos) = (std::f64::consts::TAU * cutoff / rate).sin_cos();
        let alpha = sin / (2.0 * q);
        let a0 = 1.0 + alpha;
        let b0 = (1.0 - cos) / 2.0 / a0;
        Self {
            b: [b0, 2.0 * b0, b0],
            a: [-2.0 * cos / a0, (1.0 - alpha) / a0],
            z: [0.0; 2],
        }
    }
    fn process(&mut self, x: f64) -> f64 {
        let y = self.b[0] * x + self.z[0];
        self.z[0] = self.b[1] * x - self.a[0] * y + self.z[1];
        self.z[1] = self.b[2] * x - self.a[1] * y;
        y
    }
}

fn compare_queued_samples(expected: &mut VecDeque<f64>, observed: &mut VecDeque<f64>) -> u64 {
    let mut mismatches = 0;
    while let (Some(a), Some(b)) = (expected.front(), observed.front()) {
        if a != b {
            mismatches += 1;
        }
        expected.pop_front();
        observed.pop_front();
    }
    mismatches
}

fn source_fundamental(source: StreamRequest) -> f64 {
    if matches!(source.encoding, Encoding::DsdNative { .. } | Encoding::DoP) {
        return AMPLITUDE;
    }
    let frames = usize::try_from(source.sample_rate / 10)
        .unwrap_or(4096)
        .clamp(4096, 153_600);
    let (mut sin, mut cos, mut power) = (0.0, 0.0, 0.0);
    for frame in 0..frames {
        let phase =
            std::f64::consts::TAU * TONE_HZ * frame as f64 / f64::from(source.sample_rate.max(1));
        let value = expected_pcm_wave(source, phase.sin(), 0);
        sin += value * phase.sin();
        cos += value * phase.cos();
        power += value * value;
    }
    tone_metrics(sin, cos, power, frames as f64).0
}

fn tone_metrics(sin: f64, cos: f64, power: f64, frames: f64) -> (f64, f64, f64) {
    let amplitude = 2.0 * sin.hypot(cos) / frames;
    let rms = (power / frames).sqrt();
    let coherence = if rms > 0.0 {
        amplitude / (rms * std::f64::consts::SQRT_2)
    } else {
        0.0
    };
    (amplitude, rms, coherence)
}

struct DsdDigest {
    encoding: Encoding,
    channels: usize,
    slot: usize,
    validate_markers: bool,
    phase: bool,
    pending: Vec<u8>,
    hash: Sha256,
    bytes: u64,
    invalid_markers: u64,
    expand_mono: bool,
}
impl DsdDigest {
    fn new(
        encoding: Encoding,
        channels: u16,
        slot: usize,
        validate_markers: bool,
        expand_mono: bool,
    ) -> Self {
        Self {
            encoding,
            channels: usize::from(channels),
            slot,
            validate_markers,
            phase: false,
            pending: Vec::new(),
            hash: Sha256::new(),
            bytes: 0,
            invalid_markers: 0,
            expand_mono,
        }
    }
    fn add(&mut self, input: &[u8]) {
        if matches!(self.encoding, Encoding::DsdNative { .. }) {
            if self.expand_mono {
                for byte in input {
                    self.hash.update([*byte, *byte]);
                }
                self.bytes += input.len() as u64 * 2;
            } else {
                self.hash.update(input);
                self.bytes += input.len() as u64;
            }
            return;
        }
        if self.encoding != Encoding::DoP || self.channels == 0 {
            return;
        }
        let frame_len = self.channels * self.slot;
        self.pending.extend_from_slice(input);
        let complete = self.pending.len() / frame_len * frame_len;
        for frame in self.pending[..complete].chunks_exact(frame_len) {
            let marker = if self.phase { 0xfa } else { 0x05 };
            if self.validate_markers
                && frame
                    .chunks_exact(self.slot)
                    .any(|word| word[self.slot - 1] != marker || (self.slot == 4 && word[0] != 0))
            {
                self.invalid_markers += 1;
            }
            for i in [self.slot - 2, self.slot - 3] {
                for word in frame.chunks_exact(self.slot) {
                    if self.expand_mono {
                        self.hash.update([word[i], word[i]]);
                        self.bytes += 2;
                    } else {
                        self.hash.update([word[i]]);
                        self.bytes += 1;
                    }
                }
            }
            self.phase = !self.phase;
        }
        self.pending.drain(..complete);
    }
}

fn decode_pcm(bytes: &[u8], encoding: Encoding, bits: u8) -> Option<f64> {
    if encoding == Encoding::PcmU8 && bytes.len() == 1 {
        return Some((f64::from(bytes[0]) - 128.0) / 128.0);
    }
    if encoding == Encoding::PcmFloat32 && bytes.len() == 4 {
        let value = f32::from_le_bytes(bytes.try_into().ok()?) as f64;
        return value.is_finite().then_some(value);
    }
    if encoding != Encoding::Pcm
        || bytes.is_empty()
        || bytes.len() > 4
        || bits == 0
        || usize::from(bits) > bytes.len() * 8
    {
        return None;
    }
    let mut raw = [if bytes[bytes.len() - 1] & 0x80 != 0 {
        0xff
    } else {
        0
    }; 8];
    raw[..bytes.len()].copy_from_slice(bytes);
    let value = i64::from_le_bytes(raw) >> (bytes.len() * 8 - usize::from(bits));
    Some(value as f64 / (1u64 << (bits - 1)) as f64)
}
#[cfg(test)]
fn expected_pcm(source: StreamRequest, frame: u64, ch: usize) -> f64 {
    let wave =
        (std::f64::consts::TAU * TONE_HZ * frame as f64 / f64::from(source.sample_rate)).sin();
    expected_pcm_wave(source, wave, ch)
}
fn expected_pcm_wave(source: StreamRequest, wave: f64, ch: usize) -> f64 {
    let polarity = if ch & 1 == 0 { 1.0 } else { -1.0 };
    let x = AMPLITUDE * polarity * wave;
    match source.encoding {
        Encoding::PcmU8 => ((128.0 + x * 127.0).round().clamp(0.0, 255.0) - 128.0) / 128.0,
        Encoding::PcmFloat32 => (x as f32) as f64,
        _ => {
            (x * ((1u64 << (source.bit_depth - 1)) - 1) as f64).round()
                / (1u64 << (source.bit_depth - 1)) as f64
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    #[test]
    fn unsigned_samples_center_and_widen_exactly() {
        for byte in [0u8, 126, 127, 128, 129, 130, 255] {
            let wide = ((i16::from(byte) - 128) << 8).to_le_bytes();
            assert_eq!(
                decode_pcm(&[byte], Encoding::PcmU8, 8),
                decode_pcm(&wide, Encoding::Pcm, 16)
            );
        }
    }
    #[test]
    fn signed_normalization_handles_padded_negative_and_positive_values() {
        assert_eq!(decode_pcm(&[0, 0, 0x80], Encoding::Pcm, 16), Some(-1.0));
        assert_eq!(
            decode_pcm(&[0, 0xff, 0x7f], Encoding::Pcm, 16),
            Some(32767.0 / 32768.0)
        );
    }
    #[test]
    fn generated_u8_reference_preserves_small_quantized_steps() {
        let request = StreamRequest::pcm_u8(48_000, 1);
        let mut signal = Signal::from_spec(Encoding::PcmU8, 48_000, 1, 1, 8);
        let data = signal.frames(1024);
        assert!(data.iter().any(|b| *b != 128));
        for (frame, byte) in data.into_iter().enumerate() {
            assert_eq!(
                expected_pcm(request, frame as u64, 0),
                decode_pcm(&[byte], Encoding::PcmU8, 8).unwrap()
            );
        }
    }
    #[test]
    fn dop_four_byte_slots_normalize_to_chronological_dsd() {
        let mut source = DsdDigest::new(Encoding::DoP, 2, 4, true, false);
        // Four-byte DoP slots can have 24 valid bits. Padding precedes payload.
        source.add(&[0, 0x22, 0x11, 0x05, 0, 0x44, 0x33, 0x05]);
        let mut target = DsdDigest::new(
            Encoding::DsdNative {
                bit_rate: 2_822_400,
            },
            2,
            1,
            false,
            false,
        );
        target.add(&[0x11, 0x33, 0x22, 0x44]);
        assert_eq!(source.invalid_markers, 0);
        assert_eq!(source.bytes, target.bytes);
        assert_eq!(source.hash.finalize()[..], target.hash.finalize()[..]);
    }
    #[test]
    fn malformed_dop_marker_is_not_a_successful_payload_proof() {
        let mut source = DsdDigest::new(Encoding::DoP, 1, 3, true, false);
        source.add(&[0x22, 0x11, 0xfa]);
        assert_eq!(source.invalid_markers, 1);
    }
    #[test]
    fn independent_projection_accepts_tone_and_rejects_wrong_frequency() {
        fn measure(freq: f64) -> (f64, f64, f64) {
            let n = 14_400;
            let mut s = 0.0;
            let mut c = 0.0;
            let mut power = 0.0;
            for i in 0..n {
                let t = i as f64 / 48_000.0;
                let sample = AMPLITUDE * (std::f64::consts::TAU * freq * t).sin();
                let phase = std::f64::consts::TAU * TONE_HZ * t;
                s += sample * phase.sin();
                c += sample * phase.cos();
                power += sample * sample;
            }
            tone_metrics(s, c, power, n as f64)
        }
        let (amplitude, _, coherence) = measure(TONE_HZ);
        assert!((amplitude - AMPLITUDE).abs() < 0.0001 && coherence > 0.99);
        assert!(measure(TONE_HZ * 2.0).2 < 0.1);
    }
    #[test]
    fn source_and_target_queue_compares_even_when_target_arrives_first() {
        let mut source = VecDeque::new();
        let mut target = VecDeque::from([0.5, -0.5]);
        assert_eq!(compare_queued_samples(&mut source, &mut target), 0);
        source.extend([0.5, -0.5]);
        assert_eq!(compare_queued_samples(&mut source, &mut target), 0);
        assert!(source.is_empty() && target.is_empty());
        source.push_back(0.5);
        target.push_back(0.5 + 1.0 / 2_147_483_648.0);
        assert_eq!(compare_queued_samples(&mut source, &mut target), 1);
    }
    #[test]
    fn mono_dsd_digest_compares_to_stereo_duplication() {
        let mut source = DsdDigest::new(
            Encoding::DsdNative {
                bit_rate: 2_822_400,
            },
            1,
            1,
            true,
            true,
        );
        let mut target = DsdDigest::new(
            Encoding::DsdNative {
                bit_rate: 2_822_400,
            },
            2,
            1,
            false,
            false,
        );
        source.add(&[0x11, 0x22, 0x33]);
        target.add(&[0x11, 0x11, 0x22, 0x22, 0x33, 0x33]);
        assert_eq!(source.bytes, target.bytes);
        assert_eq!(source.hash.finalize()[..], target.hash.finalize()[..]);
    }
    /// Runs the validator's own DSD tone through the production DSD-to-PCM converter, as the
    /// hardware fallback legs do, and returns the independent check's verdict.
    fn dsd_to_pcm_verdict(bit_rate: u32, target_rate: u32, tone_hz: Option<f64>) -> Value {
        use usbdac_core::convert::{ConvertOptions, PcmFormat, SampleFormat};
        use usbdac_core::dsd_pcm::DsdPcmConverter;
        use usbdac_core::fake::FakeDac;
        use usbdac_core::{DeviceInfo, PlaybackDevice};
        let source = StreamRequest::dsd_native(bit_rate, 2);
        let wire = StreamRequest::pcm(target_rate, 2, 32);
        let format = FakeDac::new(DeviceInfo::new(1, 2, "fake"), vec![wire], 16)
            .unwrap()
            .negotiate(&wire)
            .unwrap();
        let options = ConvertOptions::default();
        let target = PcmFormat::new(target_rate, 2, SampleFormat::int(32, 4).unwrap());
        let mut converter = DsdPcmConverter::with_options(bit_rate, 2, target, &options).unwrap();
        let mut check = ConversionCheck::new(source, format, false, options.headroom_db);
        let signal_rate = bit_rate / 8;
        let frames = u64::from(signal_rate) * 1500 / 1000;
        let mut signal = Signal::from_spec(source.encoding, signal_rate, 2, 1, 1);
        let mut output = vec![0u8; 1 << 16];
        let mut emitted = 0u64;
        let mut written = 0;
        while written < frames {
            let n = (frames - written).min(4096);
            let input = signal.frames(n as usize);
            written += n;
            check.source(&input);
            let mut offset = 0;
            while offset < input.len() {
                let done = converter.process(&input[offset..], &mut output).unwrap();
                offset += done.consumed;
                let bytes = match tone_hz {
                    // A wrong-frequency tone of the same level, so only the tone check can fail.
                    Some(hz) => wrong_tone(&mut emitted, done.produced / 8, hz, target_rate),
                    None => output[..done.produced].to_vec(),
                };
                check.observe(&bytes);
                if done.consumed == 0 && done.produced == 0 {
                    break;
                }
            }
        }
        loop {
            let produced = converter.finish(&mut output).unwrap();
            if produced == 0 {
                break;
            }
            let bytes = match tone_hz {
                Some(hz) => wrong_tone(&mut emitted, produced / 8, hz, target_rate),
                None => output[..produced].to_vec(),
            };
            check.observe(&bytes);
        }
        check.result(frames, signal_rate)
    }
    fn wrong_tone(emitted: &mut u64, frames: usize, hz: f64, rate: u32) -> Vec<u8> {
        let mut out = Vec::with_capacity(frames * 8);
        for _ in 0..frames {
            let x = AMPLITUDE
                * 10f64.powf(-3.5 / 20.0)
                * (std::f64::consts::TAU * hz * *emitted as f64 / f64::from(rate)).sin();
            for polarity in [1.0, -1.0] {
                let v = (x * polarity * f64::from(i32::MAX)).round() as i32;
                out.extend_from_slice(&v.to_le_bytes());
            }
            *emitted += 1;
        }
        out
    }
    #[test]
    fn low_rate_dsd_tone_survives_production_conversion_to_pcm() {
        // DSD64 and DSD128 in both rate families failed on a DAC without native DSD
        // (tone coherence 0.39 to 0.53) although level, polarity and duration were right.
        for (bit_rate, target_rate) in [
            (2_822_400, 192_000),
            (3_072_000, 176_400),
            (5_644_800, 192_000),
            (6_144_000, 192_000),
            (2_822_400, 44_100),
            (3_072_000, 48_000),
        ] {
            let verdict = dsd_to_pcm_verdict(bit_rate, target_rate, None);
            assert_eq!(
                verdict["passed"], true,
                "DSD {bit_rate} to {target_rate} Hz: {verdict}"
            );
        }
    }
    #[test]
    fn dsd_tone_check_still_rejects_a_wrong_frequency() {
        let verdict = dsd_to_pcm_verdict(2_822_400, 192_000, Some(TONE_HZ * 1.5));
        assert_eq!(verdict["passed"], false, "{verdict}");
    }
}
