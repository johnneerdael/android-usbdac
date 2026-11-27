//! The vocabulary of conversion: sample layouts, PCM formats, options, kinds and errors.

use std::fmt;

/// How the samples of a PCM stream are stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SampleFormat {
    /// Signed two's-complement integers, little-endian: `bits` significant bits, left-justified
    /// in a slot of `slot_bytes` bytes (the unused low bits of the slot are zero). This is the
    /// wire layout of `docs/API.md` section 4: 16-bit in 2 bytes, packed 24-bit in 3, 24-bit in 4
    /// (the sample shifted left by 8) and 32-bit in 4.
    Int { bits: u8, slot_bytes: u8 },
    /// 32-bit IEEE-754 floats, little-endian, nominal range -1.0 to 1.0.
    Float32,
}

impl SampleFormat {
    /// 16-bit in 2 bytes.
    pub const I16: Self = Self::Int {
        bits: 16,
        slot_bytes: 2,
    };
    /// Packed 24-bit in 3 bytes.
    pub const I24: Self = Self::Int {
        bits: 24,
        slot_bytes: 3,
    };
    /// 24-bit left-justified in a 4-byte slot (the low byte is zero).
    pub const I24_IN_32: Self = Self::Int {
        bits: 24,
        slot_bytes: 4,
    };
    /// 32-bit in 4 bytes.
    pub const I32: Self = Self::Int {
        bits: 32,
        slot_bytes: 4,
    };

    /// An integer layout, checked: 8 to 32 significant bits in a slot of 1 to 4 bytes that holds them.
    pub fn int(bits: u8, slot_bytes: u8) -> Result<Self, ConvertError> {
        if !(8..=32).contains(&bits)
            || !(1..=4).contains(&slot_bytes)
            || u32::from(bits) > u32::from(slot_bytes) * 8
        {
            return Err(ConvertError::InvalidArgument(format!(
                "{bits} significant bits do not fit a slot of {slot_bytes} bytes (8 to 32 bits, slots of 1 to 4 bytes)"
            )));
        }
        Ok(Self::Int { bits, slot_bytes })
    }

    /// Bytes one sample takes.
    pub fn bytes_per_sample(self) -> usize {
        match self {
            Self::Int { slot_bytes, .. } => usize::from(slot_bytes),
            Self::Float32 => 4,
        }
    }

    /// Significant bits of an integer layout; 32 for floats.
    pub fn valid_bits(self) -> u8 {
        match self {
            Self::Int { bits, .. } => bits,
            Self::Float32 => 32,
        }
    }

    pub fn is_float(self) -> bool {
        matches!(self, Self::Float32)
    }

    /// A phrase for descriptions: "16-bit", "24-bit (3-byte slot)", "32-bit float".
    pub fn describe(self) -> String {
        match self {
            Self::Float32 => "32-bit float".to_string(),
            Self::Int { bits, slot_bytes } => {
                if u32::from(bits) == u32::from(slot_bytes) * 8 && bits != 24 {
                    format!("{bits}-bit")
                } else if bits == 24 && slot_bytes == 3 {
                    "24-bit packed".to_string()
                } else {
                    format!("{bits}-bit in a {slot_bytes}-byte slot")
                }
            }
        }
    }
}

/// A PCM stream: its frame rate, channel count and sample layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PcmFormat {
    pub sample_rate: u32,
    pub channels: u16,
    pub sample: SampleFormat,
}

impl PcmFormat {
    pub fn new(sample_rate: u32, channels: u16, sample: SampleFormat) -> Self {
        Self {
            sample_rate,
            channels,
            sample,
        }
    }

    /// Bytes of one frame (one sample for every channel).
    pub fn bytes_per_frame(&self) -> usize {
        self.sample.bytes_per_sample() * usize::from(self.channels)
    }
}

/// What a conversion does to the audio, from nothing to something irreversible.
/// The order is the order of severity: a plan's kind is the largest of its steps'.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ConversionKind {
    /// Nothing changes; the bytes pass through.
    None,
    /// Every sample value is exactly what it was: widening to more bits (zero-filled low bits) and
    /// moving a sample between slot sizes. A stream that was bit-perfect stays bit-perfect.
    BitExact,
    /// No information is lost and the values can be got back, but the audio is not identical:
    /// mono duplicated to two channels, integers carried in a float that holds them exactly.
    Lossless,
    /// Information is lost or rounded: fewer bits, a float turned into integers, a new sample rate.
    Lossy,
}

impl ConversionKind {
    /// A stable identifier: `none`, `bit_exact`, `lossless`, `lossy`.
    pub fn code(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::BitExact => "bit_exact",
            Self::Lossless => "lossless",
            Self::Lossy => "lossy",
        }
    }

    /// Whether a stream that was bit-perfect at the target is still bit-perfect with respect to the source.
    pub fn preserves_bit_perfect(self) -> bool {
        self <= Self::BitExact
    }
}

/// What to do with the bits an integer narrowing or a float-to-integer step cannot keep.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
pub enum Narrowing {
    /// Drop them: round toward negative infinity (an arithmetic shift). The error is correlated
    /// with the signal and has a mean of half a step.
    Truncate,
    /// Add triangular-PDF noise of one step (peak) before rounding to nearest: the error becomes
    /// signal-independent noise at about -98 dBFS for 16 bits. The default.
    #[default]
    TpdfDither,
}

/// How much work the sample-rate converter does for how clean a result.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
pub enum Quality {
    /// Stopband of about 110 dB and no aliasing of any band into the output (the stopband starts
    /// at the lower Nyquist frequency), passband flat to 91% of it. The default.
    #[default]
    High,
    /// About 2.5 times cheaper: stopband of about 84 dB, passband flat to 90% of the lower
    /// Nyquist frequency, and content above it may alias into the top 10% of the band.
    Fast,
}

/// The knobs of a conversion. `ConvertOptions::default()` is what the tests measured.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ConvertOptions {
    /// Applied wherever bits are dropped (integer narrowing and float to integer).
    pub narrowing: Narrowing,
    /// Used for sample-rate conversion.
    pub quality: Quality,
    /// Gain in dB (zero or negative) applied after sample-rate conversion, to keep the overshoot
    /// of a resampled full-scale signal from clipping. `0.0` leaves the gain at exactly unity at
    /// DC. The default is -3.5 dB; 0.0 requests unity.
    pub headroom_db: f64,
    /// Seed of the dither's pseudo-random generator: the same seed gives the same output.
    pub dither_seed: u64,
    /// The layout of the source samples when it is not the one the request's bit depth implies
    /// (16, 24 packed or 32 integer): 32-bit float, or 24-bit in a 4-byte slot.
    pub source_sample: Option<SampleFormat>,
}

impl Default for ConvertOptions {
    fn default() -> Self {
        Self {
            narrowing: Narrowing::default(),
            quality: Quality::default(),
            headroom_db: -3.5,
            dither_seed: 0x853c_49e6_748f_ea9b,
            source_sample: None,
        }
    }
}

/// Why a plan or a converter could not be made, or a call is not valid now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConvertError {
    /// The conversion is not one this module does: DoP or native DSD, a channel mapping other
    /// than mono to stereo, a sample-rate ratio that needs too large a filter.
    Unsupported(String),
    /// A format or option that is out of range or inconsistent.
    InvalidArgument(String),
    /// The call is not valid now (processing after `finish` without a `reset`).
    InvalidState(String),
}

impl ConvertError {
    /// A stable identifier: `unsupported`, `invalid_argument`, `invalid_state`.
    pub fn code(&self) -> &'static str {
        match self {
            Self::Unsupported(_) => "unsupported",
            Self::InvalidArgument(_) => "invalid_argument",
            Self::InvalidState(_) => "invalid_state",
        }
    }

    pub fn message(&self) -> &str {
        match self {
            Self::Unsupported(message)
            | Self::InvalidArgument(message)
            | Self::InvalidState(message) => message,
        }
    }
}

impl fmt::Display for ConvertError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code(), self.message())
    }
}

impl std::error::Error for ConvertError {}
