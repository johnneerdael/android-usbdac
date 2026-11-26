//! Stream formats: what the host asks for and what goes on the wire.

use crate::capabilities::{LinkSpeed, Pacing, SyncKind};
use crate::clock_resolution::ClockChoice;

/// How the audio is carried.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Encoding {
    /// Linear PCM.
    Pcm,
    /// DSD over PCM: 16 DSD bits per channel per PCM frame, with the 0x05/0xFA
    /// marker byte owned by the library. `sample_rate` of the request is the
    /// carrier rate (DSD bit rate / 16: 176400 for DSD64).
    DoP,
    /// Native DSD alternate setting; `bit_rate` is the 1-bit rate per channel
    /// (2822400 for DSD64).
    DsdNative { bit_rate: u32 },
}

/// What to do when the DAC cannot run the requested rate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RatePolicy {
    /// Fail with [`Error::Unsupported`](crate::Error::Unsupported). Nothing is
    /// ever resampled by the library, so a host that cannot resample itself
    /// wants this.
    #[default]
    Exact,
    /// Use the closest rate the DAC runs. [`StreamFormat::sample_rate`] then
    /// differs from the request; the host must resample, and the stream is not
    /// reported bit-perfect.
    NearestSupported,
}

/// A format the host would like.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamRequest {
    /// PCM rate, or the DoP carrier rate. Ignored for native DSD (the wire
    /// rate follows from `bit_rate` and the subslot size).
    pub sample_rate: u32,
    pub channels: u16,
    /// Significant bits per sample: 16, 24 or 32 for PCM; 24 or 32 for DoP;
    /// ignored for native DSD.
    pub bit_depth: u8,
    pub encoding: Encoding,
    pub rate_policy: RatePolicy,
}

impl StreamRequest {
    /// PCM at `sample_rate` with `bit_depth` significant bits.
    pub fn pcm(sample_rate: u32, channels: u16, bit_depth: u8) -> Self {
        Self {
            sample_rate,
            channels,
            bit_depth,
            encoding: Encoding::Pcm,
            rate_policy: RatePolicy::Exact,
        }
    }

    /// DoP for a DSD stream of `dsd_bit_rate` (2822400 for DSD64) in a 24-bit
    /// carrier.
    pub fn dop(dsd_bit_rate: u32, channels: u16) -> Self {
        Self {
            sample_rate: dsd_bit_rate / 16,
            channels,
            bit_depth: 24,
            encoding: Encoding::DoP,
            rate_policy: RatePolicy::Exact,
        }
    }

    /// Native DSD of `dsd_bit_rate` (2822400 for DSD64).
    pub fn dsd_native(dsd_bit_rate: u32, channels: u16) -> Self {
        Self {
            sample_rate: 0,
            channels,
            bit_depth: 0,
            encoding: Encoding::DsdNative {
                bit_rate: dsd_bit_rate,
            },
            rate_policy: RatePolicy::Exact,
        }
    }

    pub fn with_rate_policy(mut self, rate_policy: RatePolicy) -> Self {
        self.rate_policy = rate_policy;
        self
    }
}

/// What will go on the wire, as chosen by
/// [`Dac::negotiate`](crate::Dac::negotiate).
///
/// A `StreamFormat` cannot be built by hand; pass the one `negotiate` returned
/// to [`Dac::open_stream`](crate::Dac::open_stream).
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct StreamFormat {
    /// The request this format answers.
    pub request: StreamRequest,
    pub encoding: Encoding,
    /// Frame rate on the wire: the PCM rate, the DoP carrier rate, or for
    /// native DSD the rate of subslot-sized words (`bit_rate / (8 *
    /// subslot_bytes)`).
    pub sample_rate: u32,
    pub channels: u16,
    /// Significant bits of the alternate setting (PCM and DoP carrier). 1 for
    /// native DSD.
    pub valid_bits: u8,
    /// Bytes per sample on the wire.
    pub subslot_bytes: u8,
    pub interface_number: u8,
    pub alt_setting: u8,
    pub endpoint_address: u8,
    pub sync_type: SyncKind,
    pub explicit_feedback: bool,
    pub link_speed: LinkSpeed,
    pub service_interval_us: u32,
    /// How the stream is paced on the endpoint: what the transport does, named
    /// (see [`Pacing`]). Consistent with `sync_type` and `explicit_feedback`.
    pub pacing: Pacing,
    /// The Clock Source the sample rate was set and verified on, and how it was
    /// reached from the stream's terminal (see [`ClockChoice`]), as resolved by
    /// [`Dac::negotiate`](crate::Dac::negotiate). `None` when the DAC has no
    /// clock entities to set (UAC 1.0 sets the rate on the endpoint).
    pub clock: Option<ClockChoice>,
}

impl StreamFormat {
    /// Bytes of one wire frame (one sample for every channel): the unit
    /// [`DacStream::write`](crate::DacStream::write) counts in. Also the
    /// granularity of everything the stream accepts.
    pub fn bytes_per_frame(&self) -> usize {
        usize::from(self.subslot_bytes) * usize::from(self.channels)
    }

    /// Rate of the unit that `played_frames()`, `latency_frames()` and the
    /// stream statistics count in. For PCM and DoP it is the wire frame rate.
    /// For native DSD it is `bit_rate / 8`: one frame is one DSD byte per
    /// channel (8 DSD bits per channel).
    pub fn counter_rate_hz(&self) -> u32 {
        match self.encoding {
            Encoding::DsdNative { bit_rate } => bit_rate / 8,
            _ => self.sample_rate,
        }
    }

    /// Whether the DAC runs a different rate than requested (only possible
    /// with [`RatePolicy::NearestSupported`]).
    pub fn rate_differs_from_request(&self) -> bool {
        match self.encoding {
            Encoding::DsdNative { .. } => false,
            _ => self.sample_rate != self.request.sample_rate,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_constructors_derive_the_carrier_rate() {
        assert_eq!(StreamRequest::dop(2_822_400, 2).sample_rate, 176_400);
        assert_eq!(StreamRequest::dop(5_644_800, 2).sample_rate, 352_800);
        let native = StreamRequest::dsd_native(2_822_400, 2);
        assert_eq!(native.sample_rate, 0);
        assert_eq!(
            native.encoding,
            Encoding::DsdNative {
                bit_rate: 2_822_400
            }
        );
        assert_eq!(StreamRequest::pcm(44_100, 2, 16).rate_policy, RatePolicy::Exact);
    }
}
