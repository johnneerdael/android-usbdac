//! Opt-in conversion helpers.
//!
//! The direct USB path never converts anything. This module is for a host that has audio in one
//! format and a DAC that runs another (a rate the DAC refuses under
//! [`RatePolicy::NearestSupported`](crate::RatePolicy), a depth it lacks): it plans the smallest
//! chain of steps that bridges them, says honestly what that costs, and runs it.
//!
//! - [`ConversionPlan::between`] plans a conversion from a request to a negotiated format and
//!   classifies it as [`ConversionKind::None`], [`ConversionKind::BitExact`] (widening and
//!   repacking: every sample value is unchanged), [`ConversionKind::Lossless`] (mono duplicated to
//!   stereo, integers carried in a float) or [`ConversionKind::Lossy`] (fewer bits, floats to
//!   integers, a new sample rate). Only the first two keep a bit-perfect stream bit-perfect.
//! - [`Converter`] runs a plan on chunks of bytes, without allocating after it is built.
//! - [`Resampler`] is the polyphase windowed-sinc converter underneath, for any rational ratio
//!   between the standard rates; its design and measured quality are in its module documentation.
//!
//! ### Clipping and headroom
//!
//! Integer outputs saturate at full scale and never wrap, and [`Converter::clipped_samples`] counts
//! how often. Resampling defaults to -3.5 dB gain after the filter. This covers the measured
//! full-scale square-wave overshoot; other signals can still clip, so monitor the saturation count.
//! Set 0 dB explicitly for unity. Exact and widening paths never apply this gain.
//!
//! ### What is not converted
//!
//! DoP and native DSD are never converted ([`ConvertError::Unsupported`]), nor is any channel
//! layout other than mono to stereo.

mod converter;
mod format;
mod plan;
mod resample;
mod samples;

#[cfg(test)]
mod quality_tests;

pub use converter::{Converter, Processed, BLOCK_FRAMES};
pub use format::{
    ConversionKind, ConvertError, ConvertOptions, Narrowing, PcmFormat, Quality, SampleFormat,
};
pub use plan::{ConversionPlan, Step, MAX_RATE, MIN_RATE};
pub use resample::{filter_info, FilterInfo, Resampler, MAX_COEFFICIENTS, MAX_TAPS};
