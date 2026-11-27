//! Planning: the smallest chain of steps that turns one PCM format into another, how much damage
//! that chain does, and a sentence that says so.

use super::format::{
    ConversionKind, ConvertError, ConvertOptions, Narrowing, PcmFormat, Quality, SampleFormat,
};
use crate::format::{Encoding, StreamFormat, StreamRequest};

/// The lowest and highest sample rate a plan accepts.
pub const MIN_RATE: u32 = 8_000;
pub const MAX_RATE: u32 = 768_000;

/// One thing a conversion does.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Step {
    /// The sample rate changes: a polyphase windowed-sinc resampler. Lossy.
    Resample {
        from_hz: u32,
        to_hz: u32,
        quality: Quality,
    },
    /// An integer sample gains bits: the old bits keep their place and the new low ones are zero.
    /// A move to a bigger slot size comes with it. Bit-exact.
    Widen {
        from: SampleFormat,
        to: SampleFormat,
    },
    /// An integer sample keeps its bits and changes slot size, as 24-bit packed in 3 bytes into 24-bit in 4. Bit-exact.
    Repack {
        from: SampleFormat,
        to: SampleFormat,
    },
    /// An integer sample loses bits. Lossy.
    Narrow {
        from: SampleFormat,
        to: SampleFormat,
        mode: Narrowing,
    },
    /// An integer sample becomes a float. Lossless up to 24 significant bits (a float holds them
    /// exactly), lossy beyond.
    IntToFloat { from: SampleFormat },
    /// A float sample is rounded to integers (after resampling, the rounding of the resampled values). Lossy.
    Quantise { to: SampleFormat, mode: Narrowing },
    /// One channel becomes two identical ones. Lossless, not identical.
    MonoToStereo,
}

impl Step {
    pub fn kind(&self) -> ConversionKind {
        match self {
            Self::Resample { .. } | Self::Narrow { .. } | Self::Quantise { .. } => {
                ConversionKind::Lossy
            }
            Self::Widen { .. } | Self::Repack { .. } => ConversionKind::BitExact,
            Self::MonoToStereo => ConversionKind::Lossless,
            Self::IntToFloat { from } => {
                if from.valid_bits() <= 24 {
                    ConversionKind::Lossless
                } else {
                    ConversionKind::Lossy
                }
            }
        }
    }

    /// A stable identifier.
    pub fn code(&self) -> &'static str {
        match self {
            Self::Resample { .. } => "resample",
            Self::Widen { .. } => "widen",
            Self::Repack { .. } => "repack",
            Self::Narrow { .. } => "narrow",
            Self::IntToFloat { .. } => "int_to_float",
            Self::Quantise { .. } => "quantise",
            Self::MonoToStereo => "mono_to_stereo",
        }
    }

    /// A phrase for the plan's description.
    pub fn describe(&self) -> String {
        fn mode(mode: Narrowing) -> &'static str {
            match mode {
                Narrowing::Truncate => "by truncation",
                Narrowing::TpdfDither => "with TPDF dither",
            }
        }
        match self {
            Self::Resample {
                from_hz,
                to_hz,
                quality,
            } => {
                let name = match quality {
                    Quality::High => "high quality, stopband about 110 dB",
                    Quality::Fast => "fast quality, stopband about 84 dB",
                };
                format!(
                    "resampled from {from_hz} Hz to {to_hz} Hz (windowed-sinc polyphase, {name})"
                )
            }
            Self::Widen { from, to } => format!(
                "{} widened to {} (the new low bits are zero, every value is unchanged)",
                from.describe(),
                to.describe()
            ),
            Self::Repack { from, to } => format!(
                "{} repacked as {} (every value is unchanged)",
                from.describe(),
                to.describe()
            ),
            Self::Narrow {
                from,
                to,
                mode: narrowing,
            } => format!(
                "{} narrowed to {} {}",
                from.describe(),
                to.describe(),
                mode(*narrowing)
            ),
            Self::IntToFloat { from } => {
                if from.valid_bits() <= 24 {
                    format!("{} converted to 32-bit float (exact)", from.describe())
                } else {
                    format!(
                        "{} converted to 32-bit float (rounded to 24 significant bits)",
                        from.describe()
                    )
                }
            }
            Self::Quantise {
                to,
                mode: narrowing,
            } => {
                format!(
                    "quantised to {} {} (full scale saturates)",
                    to.describe(),
                    mode(*narrowing)
                )
            }
            Self::MonoToStereo => "mono duplicated to stereo".to_string(),
        }
    }
}

/// How to turn `source` into `target`: the steps, what they cost, and a sentence for a status line.
#[derive(Debug, Clone, PartialEq)]
pub struct ConversionPlan {
    source: PcmFormat,
    target: PcmFormat,
    options: ConvertOptions,
    steps: Vec<Step>,
    kind: ConversionKind,
    description: String,
}

impl ConversionPlan {
    /// The plan for what the host wants to play (`source`, a PCM request) and what the DAC runs
    /// (`target`, the negotiated format). The source samples are in the layout the request's bit
    /// depth implies (16-bit, packed 24-bit, 32-bit) unless [`ConvertOptions::source_sample`] says
    /// otherwise. DoP and native DSD are refused: converting them would destroy them.
    pub fn between(
        source: &StreamRequest,
        target: &StreamFormat,
        options: &ConvertOptions,
    ) -> Result<Self, ConvertError> {
        if !matches!(source.encoding, Encoding::Pcm | Encoding::PcmFloat32)
            || target.encoding != Encoding::Pcm
        {
            return Err(ConvertError::Unsupported(
                "DoP and native DSD are never converted: a sample-rate or bit-depth change would destroy them".into(),
            ));
        }
        if source.encoding == Encoding::PcmFloat32
            && options
                .source_sample
                .is_some_and(|sample| sample != SampleFormat::Float32)
        {
            return Err(ConvertError::InvalidArgument(
                "float PCM source requires Float32 layout".into(),
            ));
        }
        let source_sample = match (source.encoding, options.source_sample) {
            (Encoding::PcmFloat32, _) => SampleFormat::Float32,
            (_, Some(sample)) => sample,
            (_, None) => match source.bit_depth {
                16 => SampleFormat::I16,
                24 => SampleFormat::I24,
                32 => SampleFormat::I32,
                other => {
                    return Err(ConvertError::InvalidArgument(format!(
                        "a source of {other} bits is not 16, 24 or 32"
                    )))
                }
            },
        };
        Self::pcm(
            PcmFormat::new(source.sample_rate, source.channels, source_sample),
            PcmFormat::new(
                target.sample_rate,
                target.channels,
                SampleFormat::int(target.valid_bits, target.subslot_bytes)?,
            ),
            options,
        )
    }

    /// The plan between two PCM formats.
    pub fn pcm(
        source: PcmFormat,
        target: PcmFormat,
        options: &ConvertOptions,
    ) -> Result<Self, ConvertError> {
        for (name, format) in [("source", &source), ("target", &target)] {
            if let SampleFormat::Int { bits, slot_bytes } = format.sample {
                SampleFormat::int(bits, slot_bytes)?;
            }
            if !(MIN_RATE..=MAX_RATE).contains(&format.sample_rate) {
                return Err(ConvertError::InvalidArgument(format!(
                    "the {name} rate {} Hz is outside {MIN_RATE} to {MAX_RATE} Hz",
                    format.sample_rate
                )));
            }
            if format.channels == 0 || format.channels > 8 {
                return Err(ConvertError::InvalidArgument(format!(
                    "the {name} has {} channels (1 to 8)",
                    format.channels
                )));
            }
        }
        if !(options.headroom_db.is_finite() && (-60.0..=0.0).contains(&options.headroom_db)) {
            return Err(ConvertError::InvalidArgument(
                "headroom_db must be between -60 and 0".into(),
            ));
        }
        let dup = match (source.channels, target.channels) {
            (a, b) if a == b => false,
            (1, 2) => true,
            (a, b) => {
                return Err(ConvertError::Unsupported(format!(
                    "{a} channels cannot be converted to {b}: only mono to stereo is supported"
                )))
            }
        };

        let mut steps = Vec::new();
        let resample = source.sample_rate != target.sample_rate;
        if resample {
            steps.push(Step::Resample {
                from_hz: source.sample_rate,
                to_hz: target.sample_rate,
                quality: options.quality,
            });
        }
        match (source.sample, target.sample) {
            (a, b) if a == b && !resample => {}
            // After resampling the values are not integers any more; an integer target rounds them.
            (_, SampleFormat::Float32) if resample => {}
            (_, to @ SampleFormat::Int { .. }) if resample => steps.push(Step::Quantise {
                to,
                mode: options.narrowing,
            }),
            (SampleFormat::Float32, SampleFormat::Float32) => {}
            (from @ SampleFormat::Int { .. }, SampleFormat::Float32) => {
                steps.push(Step::IntToFloat { from })
            }
            (SampleFormat::Float32, to @ SampleFormat::Int { .. }) => steps.push(Step::Quantise {
                to,
                mode: options.narrowing,
            }),
            (
                from @ SampleFormat::Int {
                    bits: from_bits, ..
                },
                to @ SampleFormat::Int { bits: to_bits, .. },
            ) => steps.push(if to_bits > from_bits {
                Step::Widen { from, to }
            } else if to_bits == from_bits {
                Step::Repack { from, to }
            } else {
                Step::Narrow {
                    from,
                    to,
                    mode: options.narrowing,
                }
            }),
        }
        if dup {
            steps.push(Step::MonoToStereo);
        }

        let kind = steps
            .iter()
            .map(Step::kind)
            .max()
            .unwrap_or(ConversionKind::None);
        let mut description = describe(kind, &steps, options.headroom_db);
        if let SampleFormat::Int { bits, slot_bytes } = source.sample {
            if u16::from(bits) < u16::from(slot_bytes) * 8 {
                description.push_str(" Declared zero padding is validated on every input buffer.");
            }
        }
        Ok(Self {
            source,
            target,
            options: *options,
            steps,
            kind,
            description,
        })
    }

    pub fn source(&self) -> PcmFormat {
        self.source
    }

    pub fn target(&self) -> PcmFormat {
        self.target
    }

    pub fn options(&self) -> &ConvertOptions {
        &self.options
    }

    pub fn steps(&self) -> &[Step] {
        &self.steps
    }

    /// The worst of the steps: [`ConversionKind::None`] if there are none.
    pub fn kind(&self) -> ConversionKind {
        self.kind
    }

    /// A sentence that says what is done and how much it costs, for a status line or a log.
    pub fn description(&self) -> &str {
        &self.description
    }

    /// Whether the sample rate changes.
    pub fn resamples(&self) -> bool {
        self.steps
            .iter()
            .any(|step| matches!(step, Step::Resample { .. }))
    }

    /// Whether a bit-perfect target stream is still bit-perfect with respect to the source.
    pub fn preserves_bit_perfect(&self) -> bool {
        self.kind.preserves_bit_perfect()
    }
}

fn describe(kind: ConversionKind, steps: &[Step], headroom_db: f64) -> String {
    let lead = match kind {
        ConversionKind::None => {
            return "No conversion: the samples pass through unchanged.".to_string()
        }
        ConversionKind::BitExact => "Bit-exact conversion",
        ConversionKind::Lossless => "Lossless conversion, but the audio is not identical",
        ConversionKind::Lossy => "Lossy conversion, not bit-perfect",
    };
    let mut text = format!(
        "{lead}: {}",
        steps
            .iter()
            .map(Step::describe)
            .collect::<Vec<_>>()
            .join("; ")
    );
    if headroom_db != 0.0
        && steps
            .iter()
            .any(|step| matches!(step, Step::Resample { .. }))
    {
        text.push_str(&format!(
            "; {:.1} dB of headroom after resampling",
            headroom_db
        ));
    }
    text.push('.');
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capabilities::{LinkSpeed, Pacing, SyncKind};

    fn fmt(rate: u32, channels: u16, sample: SampleFormat) -> PcmFormat {
        PcmFormat::new(rate, channels, sample)
    }

    fn plan(source: PcmFormat, target: PcmFormat) -> ConversionPlan {
        ConversionPlan::pcm(source, target, &ConvertOptions::default()).unwrap()
    }

    fn stream_format(
        rate: u32,
        channels: u16,
        valid_bits: u8,
        subslot_bytes: u8,
        encoding: Encoding,
    ) -> StreamFormat {
        StreamFormat {
            request: StreamRequest::pcm(rate, channels, valid_bits),
            encoding,
            sample_rate: rate,
            channels,
            valid_bits,
            subslot_bytes,
            interface_number: 1,
            alt_setting: 1,
            endpoint_address: 1,
            sync_type: SyncKind::Asynchronous,
            explicit_feedback: true,
            link_speed: LinkSpeed::High,
            service_interval_us: 125,
            pacing: Pacing::ExplicitFeedback { endpoint: 0x81 },
            clock: None,
            native_dsd_signature: None,
            channel_map: crate::capture::ChannelMap::uac2(channels, 0).unwrap(),
        }
    }

    #[test]
    fn float_stream_request_uses_float_layout_and_rejects_integer_override() {
        let source = StreamRequest::pcm_float32(44_100, 2);
        let target = stream_format(44_100, 2, 16, 2, Encoding::Pcm);
        let plan = ConversionPlan::between(&source, &target, &ConvertOptions::default()).unwrap();
        assert_eq!(plan.source().sample, SampleFormat::Float32);
        assert_eq!(plan.kind(), ConversionKind::Lossy);
        let options = ConvertOptions {
            source_sample: Some(SampleFormat::I32),
            ..ConvertOptions::default()
        };
        assert!(ConversionPlan::between(&source, &target, &options).is_err());
    }

    #[test]
    fn identical_formats_need_no_conversion() {
        let p = plan(
            fmt(44_100, 2, SampleFormat::I16),
            fmt(44_100, 2, SampleFormat::I16),
        );
        assert_eq!(p.kind(), ConversionKind::None);
        assert!(p.steps().is_empty());
        assert_eq!(
            p.description(),
            "No conversion: the samples pass through unchanged."
        );
        assert!(p.preserves_bit_perfect());
    }

    #[test]
    fn widening_and_repacking_are_bit_exact() {
        let widen = plan(
            fmt(44_100, 2, SampleFormat::I16),
            fmt(44_100, 2, SampleFormat::I24),
        );
        assert_eq!(widen.kind(), ConversionKind::BitExact);
        assert_eq!(widen.steps().len(), 1);
        assert!(matches!(widen.steps()[0], Step::Widen { .. }));
        assert!(widen
            .description()
            .starts_with("Bit-exact conversion: 16-bit widened to 24-bit packed"));
        assert!(widen.preserves_bit_perfect());

        let to32 = plan(
            fmt(48_000, 2, SampleFormat::I16),
            fmt(48_000, 2, SampleFormat::I32),
        );
        assert_eq!(to32.kind(), ConversionKind::BitExact);

        let repack = plan(
            fmt(96_000, 2, SampleFormat::I24),
            fmt(96_000, 2, SampleFormat::I24_IN_32),
        );
        assert_eq!(repack.kind(), ConversionKind::BitExact);
        assert!(matches!(repack.steps()[0], Step::Repack { .. }));
        let back = plan(
            fmt(96_000, 2, SampleFormat::I24_IN_32),
            fmt(96_000, 2, SampleFormat::I24),
        );
        assert_eq!(back.kind(), ConversionKind::BitExact);

        // 16-bit in a 3-byte slot: the sample shifted left by 8.
        let slot = plan(
            fmt(44_100, 2, SampleFormat::I16),
            SampleFormat::int(16, 3).map(|s| fmt(44_100, 2, s)).unwrap(),
        );
        assert_eq!(slot.kind(), ConversionKind::BitExact);
    }

    #[test]
    fn narrowing_is_lossy_with_the_chosen_mode() {
        let p = plan(
            fmt(44_100, 2, SampleFormat::I24),
            fmt(44_100, 2, SampleFormat::I16),
        );
        assert_eq!(p.kind(), ConversionKind::Lossy);
        assert!(
            p.description()
                .contains("narrowed to 16-bit with TPDF dither"),
            "{}",
            p.description()
        );
        let truncate = ConversionPlan::pcm(
            fmt(44_100, 2, SampleFormat::I32),
            fmt(44_100, 2, SampleFormat::I16),
            &ConvertOptions {
                narrowing: Narrowing::Truncate,
                ..ConvertOptions::default()
            },
        )
        .unwrap();
        assert!(
            truncate.description().contains("by truncation"),
            "{}",
            truncate.description()
        );
        assert!(!truncate.preserves_bit_perfect());
    }

    #[test]
    fn mono_to_stereo_is_lossless_but_not_identical() {
        let p = plan(
            fmt(44_100, 1, SampleFormat::I16),
            fmt(44_100, 2, SampleFormat::I16),
        );
        assert_eq!(p.kind(), ConversionKind::Lossless);
        assert_eq!(p.steps(), &[Step::MonoToStereo]);
        assert!(p
            .description()
            .starts_with("Lossless conversion, but the audio is not identical"));
        assert!(!p.preserves_bit_perfect());
    }

    #[test]
    fn other_channel_changes_are_unsupported() {
        for (from, to) in [(2, 1), (6, 2), (2, 6), (1, 4)] {
            let error = ConversionPlan::pcm(
                fmt(44_100, from, SampleFormat::I16),
                fmt(44_100, to, SampleFormat::I16),
                &ConvertOptions::default(),
            )
            .unwrap_err();
            assert_eq!(error.code(), "unsupported", "{from} -> {to}");
            assert!(error.message().contains("only mono to stereo"));
        }
    }

    #[test]
    fn integer_to_float_is_lossless_up_to_24_bits_and_lossy_beyond() {
        for sample in [
            SampleFormat::I16,
            SampleFormat::I24,
            SampleFormat::I24_IN_32,
        ] {
            let p = plan(
                fmt(48_000, 2, sample),
                fmt(48_000, 2, SampleFormat::Float32),
            );
            assert_eq!(p.kind(), ConversionKind::Lossless, "{sample:?}");
            assert!(p.description().contains("(exact)"));
        }
        let p = plan(
            fmt(48_000, 2, SampleFormat::I32),
            fmt(48_000, 2, SampleFormat::Float32),
        );
        assert_eq!(p.kind(), ConversionKind::Lossy);
        assert!(p.description().contains("rounded to 24 significant bits"));
    }

    #[test]
    fn float_to_integer_is_lossy() {
        let p = plan(
            fmt(48_000, 2, SampleFormat::Float32),
            fmt(48_000, 2, SampleFormat::I24),
        );
        assert_eq!(p.kind(), ConversionKind::Lossy);
        assert!(matches!(p.steps()[0], Step::Quantise { .. }));
        let same = plan(
            fmt(48_000, 2, SampleFormat::Float32),
            fmt(48_000, 2, SampleFormat::Float32),
        );
        assert_eq!(same.kind(), ConversionKind::None);
    }

    #[test]
    fn a_rate_change_is_lossy_and_an_integer_target_quantises_the_result() {
        let p = plan(
            fmt(44_100, 2, SampleFormat::I16),
            fmt(48_000, 2, SampleFormat::I16),
        );
        assert_eq!(p.kind(), ConversionKind::Lossy);
        assert!(p.resamples());
        assert!(matches!(
            p.steps()[0],
            Step::Resample {
                from_hz: 44_100,
                to_hz: 48_000,
                quality: Quality::High
            }
        ));
        assert!(matches!(p.steps()[1], Step::Quantise { .. }));
        assert_eq!(p.steps().len(), 2);
        assert!(p
            .description()
            .starts_with("Lossy conversion, not bit-perfect: resampled from 44100 Hz to 48000 Hz"));
        assert!(p
            .description()
            .contains("quantised to 16-bit with TPDF dither"));
        // A float target needs no quantising.
        let float = plan(
            fmt(44_100, 2, SampleFormat::I16),
            fmt(48_000, 2, SampleFormat::Float32),
        );
        assert_eq!(float.steps().len(), 1);
    }

    #[test]
    fn the_steps_of_a_mixed_conversion_are_listed_in_execution_order() {
        let p = plan(
            fmt(44_100, 1, SampleFormat::I16),
            fmt(96_000, 2, SampleFormat::I24),
        );
        let codes: Vec<_> = p.steps().iter().map(Step::code).collect();
        assert_eq!(codes, ["resample", "quantise", "mono_to_stereo"]);
        assert_eq!(p.kind(), ConversionKind::Lossy);
    }

    #[test]
    fn headroom_is_mentioned_when_it_applies() {
        let options = ConvertOptions {
            headroom_db: -1.0,
            ..ConvertOptions::default()
        };
        let resampled = ConversionPlan::pcm(
            fmt(44_100, 2, SampleFormat::I16),
            fmt(48_000, 2, SampleFormat::I16),
            &options,
        )
        .unwrap();
        assert!(resampled
            .description()
            .contains("-1.0 dB of headroom after resampling"));
        let not_resampled = ConversionPlan::pcm(
            fmt(44_100, 2, SampleFormat::I16),
            fmt(44_100, 2, SampleFormat::I24),
            &options,
        )
        .unwrap();
        assert!(!not_resampled.description().contains("headroom"));
    }

    #[test]
    fn bad_formats_and_options_are_invalid_arguments() {
        let ok = fmt(44_100, 2, SampleFormat::I16);
        for bad in [
            fmt(7_999, 2, SampleFormat::I16),
            fmt(768_001, 2, SampleFormat::I16),
            fmt(44_100, 0, SampleFormat::I16),
            fmt(44_100, 9, SampleFormat::I16),
        ] {
            assert_eq!(
                ConversionPlan::pcm(bad, ok, &ConvertOptions::default())
                    .unwrap_err()
                    .code(),
                "invalid_argument"
            );
            assert_eq!(
                ConversionPlan::pcm(ok, bad, &ConvertOptions::default())
                    .unwrap_err()
                    .code(),
                "invalid_argument"
            );
        }
        for headroom in [1.0, f64::NAN, -61.0] {
            let options = ConvertOptions {
                headroom_db: headroom,
                ..ConvertOptions::default()
            };
            assert_eq!(
                ConversionPlan::pcm(ok, ok, &options).unwrap_err().code(),
                "invalid_argument"
            );
        }
        assert!(SampleFormat::int(24, 2).is_err());
        assert!(SampleFormat::int(33, 4).is_err());
        assert!(SampleFormat::int(16, 5).is_err());
        assert!(SampleFormat::int(4, 1).is_err());
    }

    #[test]
    fn between_reads_a_request_and_a_negotiated_format() {
        // The host has 44.1 kHz 16-bit; the DAC runs 48 kHz 24-bit in a 4-byte slot (NearestSupported).
        let request = StreamRequest::pcm(44_100, 2, 16);
        let format = stream_format(48_000, 2, 24, 4, Encoding::Pcm);
        let p = ConversionPlan::between(&request, &format, &ConvertOptions::default()).unwrap();
        assert_eq!(p.source(), fmt(44_100, 2, SampleFormat::I16));
        assert_eq!(p.target(), fmt(48_000, 2, SampleFormat::I24_IN_32));
        assert_eq!(p.kind(), ConversionKind::Lossy);

        // Same rate, the DAC only has 32-bit slots: widening, bit-exact.
        let same = stream_format(44_100, 2, 32, 4, Encoding::Pcm);
        let p = ConversionPlan::between(&request, &same, &ConvertOptions::default()).unwrap();
        assert_eq!(p.kind(), ConversionKind::BitExact);
    }

    #[test]
    fn between_honours_a_float_source() {
        let request = StreamRequest::pcm(48_000, 2, 32);
        let format = stream_format(48_000, 2, 24, 3, Encoding::Pcm);
        let options = ConvertOptions {
            source_sample: Some(SampleFormat::Float32),
            ..ConvertOptions::default()
        };
        let p = ConversionPlan::between(&request, &format, &options).unwrap();
        assert_eq!(p.source().sample, SampleFormat::Float32);
        assert!(matches!(p.steps()[0], Step::Quantise { .. }));
    }

    #[test]
    fn dsd_is_never_converted() {
        let format = stream_format(44_100, 2, 16, 2, Encoding::Pcm);
        let dop = StreamRequest::dop(2_822_400, 2);
        let native = StreamRequest::dsd_native(2_822_400, 2);
        for request in [dop, native] {
            let error =
                ConversionPlan::between(&request, &format, &ConvertOptions::default()).unwrap_err();
            assert_eq!(error.code(), "unsupported");
            assert!(error.message().contains("DoP and native DSD"));
        }
        let wire = stream_format(176_400, 2, 24, 3, Encoding::DoP);
        let error = ConversionPlan::between(
            &StreamRequest::pcm(176_400, 2, 24),
            &wire,
            &ConvertOptions::default(),
        )
        .unwrap_err();
        assert_eq!(error.code(), "unsupported");
    }

    #[test]
    fn a_source_depth_that_is_not_pcm_is_invalid() {
        let format = stream_format(44_100, 2, 16, 2, Encoding::Pcm);
        let error = ConversionPlan::between(
            &StreamRequest::pcm(44_100, 2, 20),
            &format,
            &ConvertOptions::default(),
        )
        .unwrap_err();
        assert_eq!(error.code(), "invalid_argument");
    }
}
