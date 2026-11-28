//! Explicit source-domain playback resolution and streaming.
//! Existing strict negotiation and direct stream APIs remain unchanged.
use crate::convert::{
    ConversionKind, ConversionPlan, ConvertOptions, Converter, PcmFormat, SampleFormat,
};
use crate::dsd_pcm::DsdPcmConverter;
use crate::{
    AltFormat, Dac, DacListener, DacStream, Encoding, Error, RatePolicy, Result, StreamFormat,
    StreamRequest, StreamStats,
};
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, PartialEq)]
pub struct PlaybackPolicy {
    pub allow_lossless: bool,
    pub allow_widening: bool,
    pub allow_lossy: bool,
    pub allow_native_dsd: bool,
    pub dop_confirmed: bool,
    pub allow_dop_pcm: bool,
    pub max_pcm_rate: Option<u32>,
    pub allowed_pcm_depths: Option<Vec<u8>>,
    pub conversion_options: ConvertOptions,
}
impl Default for PlaybackPolicy {
    fn default() -> Self {
        Self {
            allow_lossless: true,
            allow_widening: true,
            allow_lossy: true,
            allow_native_dsd: true,
            dop_confirmed: false,
            allow_dop_pcm: true,
            max_pcm_rate: None,
            allowed_pcm_depths: None,
            conversion_options: ConvertOptions::default(),
        }
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum PlaybackFidelity {
    Exact,
    BitExact,
    Lossless,
    Lossy,
}
#[derive(Debug, Clone, PartialEq)]
pub enum PlaybackTransform {
    Direct,
    Pcm {
        plan: ConversionPlan,
        unsigned_input: bool,
    },
    DsdToPcm {
        bit_rate: u32,
        dop_input: bool,
        options: ConvertOptions,
    },
    DopToNative {
        target_channels: u16,
    },
    DopRepack {
        target_channels: u16,
    },
    NativeToDop {
        target_channels: u16,
    },
    DsdChannelExpansion {
        target_channels: u16,
    },
}
/// Auditable ranking inputs for an individual planned request. Bandwidth is
/// the conservative passband edge of the production conversion path.
#[derive(Debug, Clone)]
pub struct PlaybackCandidateQuality {
    pub request: StreamRequest,
    pub fidelity: PlaybackFidelity,
    pub retained_bandwidth_hz: Option<f64>,
    pub meaningful_depth_bits: Option<u8>,
    pub resampling: bool,
    pub resampler_passband_hz: Option<f64>,
    pub resampler_attenuation_db: Option<f64>,
    pub dsd_first_stage_passband_hz: Option<f64>,
    pub dsd_first_stage_attenuation_db: Option<f64>,
    pub reason: String,
}
#[derive(Debug, Clone)]
pub struct PlaybackDecision {
    pub source: StreamRequest,
    pub format: StreamFormat,
    /// Ordered planned requests; a success may stop before all were attempted.
    pub candidates: Vec<StreamRequest>,
    /// Quality records aligned to `candidates`; planned, not necessarily attempted.
    pub candidate_quality: Vec<PlaybackCandidateQuality>,
    pub selected_quality: PlaybackCandidateQuality,
    pub attempted_candidates: usize,
    /// Typed, clean refusals for the attempted prefix before the selected request.
    pub candidate_refusals: Vec<Error>,
    pub exact_refusal: Option<Error>,
    /// Descriptor-backed source exclusion before any candidate USB mutation.
    pub source_preflight_refusal: Option<String>,
    pub transform: PlaybackTransform,
    pub fidelity: PlaybackFidelity,
    /// Input framing at `PlaybackStream::write`. Native DSD is one byte/channel.
    pub source_frame_bytes: usize,
}
#[derive(Debug, Clone, Default)]
pub struct PlaybackConversionStats {
    pub source_bytes: u64,
    pub target_bytes: u64,
    pub clipped_samples: u64,
}

fn conversion_error(e: crate::convert::ConvertError) -> Error {
    match e {
        crate::convert::ConvertError::Unsupported(reason) => Error::Unsupported {
            reason,
            offered: vec![],
        },
        crate::convert::ConvertError::InvalidArgument(reason) => Error::InvalidArgument(reason),
        crate::convert::ConvertError::InvalidState(reason) => Error::InvalidState(reason),
    }
}
fn input_sample(source: &StreamRequest, options: &ConvertOptions) -> Result<SampleFormat> {
    match source.encoding {
        Encoding::PcmU8 => {
            if options.source_sample.is_some() {
                return Err(Error::InvalidArgument(
                    "U8 source does not accept a source_sample override".into(),
                ));
            }
            Ok(SampleFormat::I16)
        }
        Encoding::PcmFloat32 => {
            if options
                .source_sample
                .is_some_and(|s| s != SampleFormat::Float32)
            {
                return Err(Error::InvalidArgument(
                    "float input requires Float32 source layout".into(),
                ));
            }
            Ok(SampleFormat::Float32)
        }
        Encoding::Pcm => Ok(options.source_sample.unwrap_or(match source.bit_depth {
            16 => SampleFormat::I16,
            24 => SampleFormat::I24,
            32 => SampleFormat::I32,
            _ => return Err(Error::InvalidArgument("unsupported PCM depth".into())),
        })),
        _ => Err(Error::InvalidArgument("not PCM".into())),
    }
}
fn input_frame(source: &StreamRequest, options: &ConvertOptions) -> Result<usize> {
    let slot = match source.encoding {
        Encoding::PcmU8 => 1,
        Encoding::Pcm | Encoding::PcmFloat32 => input_sample(source, options)?.bytes_per_sample(),
        Encoding::DoP => match source.bit_depth {
            24 => 3,
            32 => 4,
            _ => {
                return Err(Error::InvalidArgument(
                    "DoP requires 24 or 32-bit slots".into(),
                ))
            }
        },
        Encoding::DsdNative { .. } => 1,
    };
    Ok(slot * usize::from(source.channels))
}
fn exact_matches(request: &StreamRequest, f: &StreamFormat) -> bool {
    f.encoding == request.encoding
        && f.channels == request.channels
        && (matches!(request.encoding, Encoding::DsdNative { .. })
            || (f.sample_rate == request.sample_rate && f.valid_bits == request.bit_depth))
}
fn permitted(policy: &PlaybackPolicy, f: &StreamFormat) -> bool {
    !matches!(
        f.encoding,
        Encoding::Pcm | Encoding::PcmU8 | Encoding::PcmFloat32
    ) || (policy.max_pcm_rate.map_or(true, |max| f.sample_rate <= max)
        && policy
            .allowed_pcm_depths
            .as_ref()
            .map_or(true, |depths| depths.contains(&f.valid_bits)))
}
fn dsd_rate(source: &StreamRequest) -> Option<u32> {
    match source.encoding {
        Encoding::DsdNative { bit_rate } => Some(bit_rate),
        Encoding::DoP => source.sample_rate.checked_mul(16),
        _ => None,
    }
}
fn supported_dsd(rate: u32) -> bool {
    matches!(
        rate,
        2_822_400
            | 3_072_000
            | 5_644_800
            | 6_144_000
            | 11_289_600
            | 12_288_000
            | 22_579_200
            | 24_576_000
    )
}
fn fidelity(kind: ConversionKind, unsigned: bool) -> PlaybackFidelity {
    match kind {
        ConversionKind::None if !unsigned => PlaybackFidelity::Exact,
        ConversionKind::None | ConversionKind::BitExact if !unsigned => PlaybackFidelity::BitExact,
        ConversionKind::None | ConversionKind::BitExact | ConversionKind::Lossless => {
            PlaybackFidelity::Lossless
        }
        ConversionKind::Lossy => PlaybackFidelity::Lossy,
    }
}
fn enabled(policy: &PlaybackPolicy, f: PlaybackFidelity) -> bool {
    match f {
        PlaybackFidelity::Exact => true,
        PlaybackFidelity::BitExact | PlaybackFidelity::Lossless => policy.allow_lossless,
        PlaybackFidelity::Lossy => policy.allow_lossy,
    }
}
#[derive(Clone)]
struct Candidate {
    request: StreamRequest,
    fidelity: PlaybackFidelity,
    rate_rank: usize,
    channels_rank: u8,
    depth_rank: (u8, u8),
    quality: PlaybackCandidateQuality,
    upsample_excess: u32,
}
fn supported_rate(alt: &crate::AltSetting, rate: u32) -> bool {
    alt.sample_rates.contains(&rate)
        || alt.rate_ranges.iter().any(|r| {
            rate >= r.min_hz
                && rate <= r.max_hz
                && if r.step_hz == 0 {
                    r.min_hz == r.max_hz
                } else {
                    (rate - r.min_hz).checked_rem(r.step_hz) == Some(0)
                }
        })
}
fn candidates(
    caps: &crate::Capabilities,
    source: &StreamRequest,
    policy: &PlaybackPolicy,
) -> Result<Vec<Candidate>> {
    let dsd = dsd_rate(source);
    if let Some(rate) = dsd {
        if !supported_dsd(rate) || !(1..=8).contains(&source.channels) {
            return Ok(vec![]);
        }
    }
    let source_rate = dsd.map_or(source.sample_rate, |r| r / 16);
    let sample = if dsd.is_some() {
        SampleFormat::Float32
    } else {
        input_sample(source, &policy.conversion_options)?
    };
    let source_depth = if dsd.is_some() { 24 } else { source.bit_depth };
    let mut found = Vec::<(StreamRequest, PlaybackFidelity)>::new();
    const STANDARD: [u32; 19] = [
        8_000, 11_025, 12_000, 16_000, 22_050, 24_000, 32_000, 44_100, 48_000, 88_200, 96_000,
        176_400, 192_000, 352_800, 384_000, 705_600, 768_000, 1_411_200, 1_536_000,
    ];
    for alt in caps.alt_settings.iter().filter(|a| {
        matches!(a.format, AltFormat::Pcm | AltFormat::PcmFloat32)
            && (a.channels == source.channels || (source.channels == 1 && a.channels == 2))
            && [16, 24, 32].contains(&a.valid_bits)
            && a.pacing.is_some()
            && a.service_interval_us > 0
            && (1..=4).contains(&a.subslot_bytes)
            && u16::from(a.valid_bits) <= u16::from(a.subslot_bytes) * 8
    }) {
        if policy
            .allowed_pcm_depths
            .as_ref()
            .is_some_and(|d| !d.contains(&alt.valid_bits))
        {
            continue;
        }
        let mut rates = alt.sample_rates.clone();
        for range in &alt.rate_ranges {
            for rate in STANDARD
                .into_iter()
                .chain([range.min_hz, range.max_hz, source_rate])
            {
                if supported_rate(alt, rate) {
                    rates.push(rate);
                }
            }
            if range.step_hz > 0 && source_rate >= range.min_hz {
                let n = (source_rate - range.min_hz) / range.step_hz;
                for i in [n, n.saturating_add(1)] {
                    if let Some(rate) = i
                        .checked_mul(range.step_hz)
                        .and_then(|v| range.min_hz.checked_add(v))
                    {
                        if supported_rate(alt, rate) {
                            rates.push(rate)
                        }
                    }
                }
            }
        }
        rates.sort_unstable();
        rates.dedup();
        for rate in rates {
            if !(crate::convert::MIN_RATE..=crate::convert::MAX_RATE).contains(&rate)
                || policy.max_pcm_rate.is_some_and(|max| rate > max)
            {
                continue;
            }
            let packet = (u64::from(rate) * u64::from(alt.service_interval_us)).div_ceil(1_000_000)
                * u64::from(alt.channels)
                * u64::from(alt.subslot_bytes);
            if packet > alt.max_packet_bytes as u64 {
                continue;
            }
            if source_rate != rate
                && crate::convert::filter_info(source_rate, rate, policy.conversion_options.quality)
                    .is_err()
            {
                continue;
            }
            let target_sample = if alt.format == AltFormat::PcmFloat32 {
                if alt.valid_bits != 32 || alt.subslot_bytes != 4 {
                    continue;
                }
                SampleFormat::Float32
            } else {
                match SampleFormat::int(alt.valid_bits, alt.subslot_bytes) {
                    Ok(v) => v,
                    Err(_) => continue,
                }
            };
            let mut opts = policy.conversion_options;
            if dsd.is_some() {
                opts.source_sample = None;
            }
            let plan = match ConversionPlan::pcm(
                PcmFormat::new(source_rate, source.channels, sample),
                PcmFormat::new(rate, alt.channels, target_sample),
                &opts,
            ) {
                Ok(v) => v,
                Err(_) => continue,
            };
            if source.encoding == Encoding::PcmU8 && !policy.allow_widening {
                continue;
            }
            if !policy.allow_widening
                && plan
                    .steps()
                    .iter()
                    .any(|step| matches!(step, crate::convert::Step::Widen { .. }))
            {
                continue;
            }
            if dsd.is_some()
                && (!policy.allow_lossy
                    || policy.conversion_options.source_sample.is_some()
                    || (matches!(source.encoding, Encoding::DoP) && !policy.allow_dop_pcm))
            {
                continue;
            }
            let kind = if dsd.is_some() {
                PlaybackFidelity::Lossy
            } else {
                fidelity(plan.kind(), source.encoding == Encoding::PcmU8)
            };
            if !enabled(policy, kind) {
                continue;
            }
            let req = if alt.format == AltFormat::PcmFloat32 {
                StreamRequest::pcm_float32(rate, alt.channels)
            } else {
                StreamRequest::pcm(rate, alt.channels, alt.valid_bits)
            };
            if source.encoding == Encoding::Pcm && req == *source {
                continue;
            }
            if !found.iter().any(|(r, _)| *r == req) {
                found.push((req, kind));
            }
        }
    }
    if let Some(bit_rate) = dsd {
        if policy.allow_lossless {
            for alt in &caps.alt_settings {
                if alt.channels != source.channels && !(source.channels == 1 && alt.channels == 2) {
                    continue;
                }
                if alt.pacing.is_none() || alt.service_interval_us == 0 {
                    continue;
                }
                let channel_fidelity = if alt.channels == source.channels {
                    PlaybackFidelity::BitExact
                } else {
                    PlaybackFidelity::Lossless
                };
                let request = if policy.allow_native_dsd
                    && alt.format == AltFormat::Dsd
                    && alt.subslot_bytes > 0
                {
                    let bits = u32::from(alt.subslot_bytes) * 8;
                    if bit_rate % bits != 0 {
                        continue;
                    }
                    let wire_rate = bit_rate / bits;
                    if !supported_rate(alt, wire_rate) {
                        continue;
                    }
                    let required = (u64::from(wire_rate) * u64::from(alt.service_interval_us))
                        .div_ceil(1_000_000)
                        * u64::from(alt.channels)
                        * u64::from(alt.subslot_bytes);
                    if required > alt.max_packet_bytes as u64 {
                        continue;
                    }
                    StreamRequest::dsd_native(bit_rate, alt.channels)
                } else if policy.dop_confirmed
                    && matches!(source.encoding, Encoding::DsdNative { .. })
                    && matches!(alt.format, AltFormat::Pcm | AltFormat::Dop)
                    && [24, 32].contains(&alt.valid_bits)
                {
                    let carrier = bit_rate / 16;
                    if bit_rate % 16 != 0
                        || !supported_rate(alt, carrier)
                        || policy.max_pcm_rate.is_some_and(|max| carrier > max)
                        || policy
                            .allowed_pcm_depths
                            .as_ref()
                            .is_some_and(|depths| !depths.contains(&alt.valid_bits))
                    {
                        continue;
                    }
                    let required = (u64::from(carrier) * u64::from(alt.service_interval_us))
                        .div_ceil(1_000_000)
                        * u64::from(alt.channels)
                        * u64::from(alt.subslot_bytes);
                    if required > alt.max_packet_bytes as u64 {
                        continue;
                    }
                    let mut req = StreamRequest::dop(bit_rate, alt.channels);
                    req.bit_depth = alt.valid_bits;
                    req
                } else {
                    continue;
                };
                if request == *source || found.iter().any(|(r, _)| *r == request) {
                    continue;
                }
                found.push((request, channel_fidelity));
            }
        }
    }
    let mut rates: Vec<u32> = found.iter().map(|(r, _)| r.sample_rate).collect();
    crate::rank_sample_rates(&mut rates, source_rate);
    let mut out: Vec<_> = found
        .into_iter()
        .map(|(request, fidelity)| Candidate {
            quality: candidate_quality(source, &request, fidelity, &policy.conversion_options),
            upsample_excess: request.sample_rate.saturating_sub(source_rate),
            rate_rank: if matches!(request.encoding, Encoding::DsdNative { .. }) {
                0
            } else {
                rates
                    .iter()
                    .position(|r| *r == request.sample_rate)
                    .unwrap_or(usize::MAX)
            },
            channels_rank: u8::from(request.channels != source.channels),
            depth_rank: (
                u8::from(request.bit_depth < source_depth),
                request.bit_depth.abs_diff(source_depth),
            ),
            request,
            fidelity,
        })
        .collect();
    sort_candidates(&mut out);
    Ok(out)
}
fn sort_candidates(candidates: &mut [Candidate]) {
    candidates.sort_by(|a, b| {
        a.fidelity
            .cmp(&b.fidelity)
            .then_with(|| {
                if a.fidelity == PlaybackFidelity::Lossy {
                    b.quality
                        .retained_bandwidth_hz
                        .unwrap_or(0.0)
                        .total_cmp(&a.quality.retained_bandwidth_hz.unwrap_or(0.0))
                        .then_with(|| {
                            b.quality
                                .meaningful_depth_bits
                                .cmp(&a.quality.meaningful_depth_bits)
                        })
                        .then_with(|| a.upsample_excess.cmp(&b.upsample_excess))
                } else {
                    std::cmp::Ordering::Equal
                }
            })
            .then_with(|| {
                (a.rate_rank, a.channels_rank, a.depth_rank).cmp(&(
                    b.rate_rank,
                    b.channels_rank,
                    b.depth_rank,
                ))
            })
    });
}
fn candidate_quality(
    source: &StreamRequest,
    target: &StreamRequest,
    fidelity: PlaybackFidelity,
    options: &ConvertOptions,
) -> PlaybackCandidateQuality {
    let dsd = dsd_rate(source);
    let source_rate = dsd.map_or(source.sample_rate, |rate| rate / 16);
    let target_pcm = matches!(
        target.encoding,
        Encoding::Pcm | Encoding::PcmU8 | Encoding::PcmFloat32
    );
    let resampling = target_pcm && target.sample_rate != source_rate;
    let filter = if resampling {
        crate::convert::filter_info(source_rate, target.sample_rate, options.quality).ok()
    } else {
        None
    };
    let first_stage = if target_pcm {
        dsd.map(|rate| f64::from(rate) * 0.020)
    } else {
        None
    };
    let retained_bandwidth_hz = if target_pcm {
        let bandwidth = if resampling {
            filter.as_ref().map(|f| f.passband_hz)
        } else {
            Some(f64::from(source_rate) / 2.0)
        };
        bandwidth.map(|bandwidth| first_stage.map_or(bandwidth, |first| first.min(bandwidth)))
    } else {
        None
    };
    let source_bits = match source.encoding {
        Encoding::PcmU8 => 8,
        Encoding::PcmFloat32 | Encoding::DoP | Encoding::DsdNative { .. } => 24,
        Encoding::Pcm => match options.source_sample {
            Some(SampleFormat::Float32) => 24,
            Some(SampleFormat::Int { bits, .. }) => bits,
            None => source.bit_depth,
        },
    };
    let target_bits = match target.encoding {
        Encoding::PcmFloat32 => 24,
        Encoding::PcmU8 => 8,
        Encoding::Pcm => target.bit_depth,
        _ => 0,
    };
    let meaningful_depth_bits = target_pcm.then_some(if resampling {
        target_bits
    } else {
        source_bits.min(target_bits)
    });
    PlaybackCandidateQuality {
        request: *target,
        fidelity,
        retained_bandwidth_hz,
        meaningful_depth_bits,
        resampling,
        resampler_passband_hz: filter.as_ref().map(|f| f.passband_hz),
        resampler_attenuation_db: filter.as_ref().map(|f| f.design_attenuation_db),
        dsd_first_stage_passband_hz: first_stage,
        dsd_first_stage_attenuation_db: first_stage.map(|_| 120.0),
        reason: if target_pcm {
            "Ranked by fidelity, retained conversion passband, meaningful depth, and excess upsampling; rate-family preference breaks ties".into()
        } else {
            "Bitstream-preserving transport; no PCM conversion passband applies".into()
        },
    }
}
/// Returns a conservative source exclusion only for a complete claimed descriptor scan.
/// Unknown/opaque alternatives and unknown rates remain eligible for real negotiation.
fn descriptor_exclusion(
    complete: bool,
    alts: &[crate::AltSetting],
    native_only_aliases: &[(u8, u8, u8)],
    complete_uac1_rate_alternates: &[(u8, u8, u8)],
    source: &StreamRequest,
) -> Option<String> {
    if !complete || alts.is_empty() || matches!(source.encoding, Encoding::DoP) {
        return None;
    }
    let desired = match source.encoding {
        Encoding::Pcm => AltFormat::Pcm,
        Encoding::PcmU8 => AltFormat::PcmU8,
        Encoding::PcmFloat32 => AltFormat::PcmFloat32,
        Encoding::DsdNative { .. } => AltFormat::Dsd,
        Encoding::DoP => unreachable!(),
    };
    if alts.iter().any(|a| {
        a.channels == source.channels
            && a.format == AltFormat::Other
            && !native_only_aliases.contains(&(
                a.interface_number,
                a.alt_setting,
                a.endpoint_address,
            ))
    }) {
        return None;
    }
    let matching: Vec<_> = alts
        .iter()
        .filter(|a| {
            a.channels == source.channels
                && a.format == desired
                && (desired == AltFormat::Dsd || a.valid_bits == source.bit_depth)
        })
        .collect();
    if matching.is_empty() {
        return Some(
            "complete descriptor snapshot has no matching encoding/channel/depth shape".into(),
        );
    }
    let possibly_supported = matching.iter().any(|alt| {
        // UAC1 rates are trusted only with the strict raw Type-I completeness witness.
        let uac1_proven = alt.uac_version == crate::UacVersion::V1
            && !matches!(source.encoding, Encoding::DsdNative { .. })
            && complete_uac1_rate_alternates.contains(&(
                alt.interface_number,
                alt.alt_setting,
                alt.endpoint_address,
            ));
        if (alt.uac_version != crate::UacVersion::V2 && !uac1_proven)
            || alt.rate_ranges.is_empty()
            || alt
                .rate_ranges
                .iter()
                .any(|r| r.min_hz == 0 || r.min_hz > r.max_hz)
        {
            return true;
        }
        let rate = match source.encoding {
            Encoding::DsdNative { bit_rate } => {
                let divisor = u32::from(alt.subslot_bytes) * 8;
                if divisor == 0 || bit_rate.checked_rem(divisor) != Some(0) {
                    return true;
                }
                bit_rate / divisor
            }
            _ => source.sample_rate,
        };
        alt.sample_rates.contains(&rate)
            || alt.rate_ranges.iter().any(|r| {
                rate >= r.min_hz
                    && rate <= r.max_hz
                    && (r.step_hz == 0 || (rate - r.min_hz).checked_rem(r.step_hz) == Some(0))
            })
    });
    if possibly_supported {
        None
    } else {
        Some(
            if matching.iter().all(|alt| alt.uac_version == crate::UacVersion::V1) {
                "complete UAC1 Type-I descriptor rate list/range excludes the exact source rate"
            } else {
                "complete UAC1 Type-I/UAC2 descriptor snapshot and rate ranges exclude the exact source rate"
            }.into(),
        )
    }
}
fn exact_policy_allows(source: &StreamRequest, policy: &PlaybackPolicy) -> bool {
    let pcm_limit_ok = !matches!(
        source.encoding,
        Encoding::Pcm | Encoding::PcmU8 | Encoding::PcmFloat32 | Encoding::DoP
    ) || (policy
        .max_pcm_rate
        .map_or(true, |max| source.sample_rate <= max)
        && policy
            .allowed_pcm_depths
            .as_ref()
            .map_or(true, |depths| depths.contains(&source.bit_depth)));
    let native_allowed =
        !matches!(source.encoding, Encoding::DsdNative { .. }) || policy.allow_native_dsd;
    pcm_limit_ok
        && native_allowed
        && (!matches!(source.encoding, Encoding::DoP) || policy.dop_confirmed)
}
impl Dac {
    pub fn resolve_playback(
        &self,
        source: &StreamRequest,
        policy: &PlaybackPolicy,
    ) -> Result<PlaybackDecision> {
        if source.rate_policy != RatePolicy::Exact {
            return Err(Error::InvalidArgument(
                "playback source requires exact rate policy".into(),
            ));
        }
        if !(1..=32).contains(&source.channels) {
            return Err(Error::InvalidArgument(
                "source channels must be between one and 32".into(),
            ));
        }
        // Validate the source independently of descriptor or policy filtering.
        crate::transport::api::playback_format_from_request(source)?;
        let frame = input_frame(source, &policy.conversion_options)?;
        let policy_allows_exact = exact_policy_allows(source, policy);
        let policy_refusal = (!policy_allows_exact).then(|| Error::Unsupported {
            reason: if matches!(source.encoding, Encoding::DoP) && !policy.dop_confirmed {
                "DoP decoder has not been confirmed"
            } else {
                "exact source is disallowed by downstream policy"
            }
            .into(),
            offered: vec![],
        });
        let caps = self.capabilities()?;
        let source_preflight_refusal = if policy_allows_exact {
            descriptor_exclusion(
                caps.exclusion_snapshot_complete,
                &caps.alt_settings,
                &caps.native_only_aliases,
                &caps.complete_uac1_rate_alternates,
                source,
            )
        } else {
            None
        };
        let allow_exact = policy_allows_exact && source_preflight_refusal.is_none();
        let all = candidates(&caps, source, policy)?;
        // A strict prefix preserves the global fidelity/rate/channel/depth ranking.
        // The transport may attempt only a bounded prefix under one owner reservation.
        let capacity = if allow_exact { 7 } else { 8 };
        let chosen: Vec<_> = all
            .iter()
            .take(capacity)
            .map(|candidate| candidate.request)
            .collect();
        let mut selected = Vec::new();
        if allow_exact {
            selected.push(*source);
        }
        selected.extend(chosen);
        if selected.is_empty() {
            return Err(policy_refusal.unwrap_or(Error::Unsupported {
                reason: source_preflight_refusal
                    .clone()
                    .unwrap_or_else(|| "no permitted playback format".into()),
                offered: vec![],
            }));
        }
        let (format, attempted_candidates, first_refusal, candidate_refusals) =
            self.negotiate_playback_candidates(&selected)?;
        if !selected.iter().any(|r| exact_matches(r, &format)) || !permitted(policy, &format) {
            return Err(Error::InvalidState(
                "candidate negotiation returned an unplanned format".into(),
            ));
        }
        let mut candidate_quality: Vec<_> = selected
            .iter()
            .map(|request| {
                all.iter()
                    .find(|candidate| candidate.request == *request)
                    .map(|candidate| candidate.quality.clone())
                    .unwrap_or_else(|| {
                        candidate_quality(
                            source,
                            request,
                            PlaybackFidelity::Exact,
                            &policy.conversion_options,
                        )
                    })
            })
            .collect();
        let mut selected_quality = candidate_quality
            .iter()
            .find(|quality| quality.request == format.request)
            .cloned()
            .ok_or_else(|| {
                Error::InvalidState("selected candidate has no quality record".into())
            })?;
        let exact_selected = allow_exact && exact_matches(source, &format);
        let refusal = if exact_selected {
            None
        } else {
            policy_refusal.or_else(|| source_preflight_refusal.as_ref().map(|reason| Error::Unsupported { reason: format!("source descriptor preflight: {reason}; no source USB negotiation was attempted"), offered: vec![] })).or(first_refusal)
        };
        if exact_selected {
            if source.encoding == Encoding::Pcm {
                let source_sample = input_sample(source, &policy.conversion_options)?;
                let wire_sample = SampleFormat::int(format.valid_bits, format.subslot_bytes)
                    .map_err(conversion_error)?;
                if source_sample == wire_sample {
                    return Ok(PlaybackDecision {
                        source: *source,
                        format,
                        candidates: selected,
                        candidate_quality,
                        selected_quality,
                        attempted_candidates,
                        candidate_refusals,
                        exact_refusal: None,
                        source_preflight_refusal: None,
                        transform: PlaybackTransform::Direct,
                        fidelity: PlaybackFidelity::Exact,
                        source_frame_bytes: frame,
                    });
                }
                let plan = ConversionPlan::between(source, &format, &policy.conversion_options)
                    .map_err(conversion_error)?;
                let f = fidelity(plan.kind(), false);
                selected_quality.fidelity = f;
                if let Some(q) = candidate_quality
                    .iter_mut()
                    .find(|q| q.request == format.request)
                {
                    q.fidelity = f;
                }
                if !enabled(policy, f)
                    || (!policy.allow_widening
                        && plan
                            .steps()
                            .iter()
                            .any(|step| matches!(step, crate::convert::Step::Widen { .. })))
                {
                    return Err(Error::Unsupported {
                        reason: "source layout conversion is disallowed by policy".into(),
                        offered: vec![],
                    });
                }
                let transform = if plan.kind() == ConversionKind::None {
                    PlaybackTransform::Direct
                } else {
                    PlaybackTransform::Pcm {
                        plan,
                        unsigned_input: false,
                    }
                };
                return Ok(PlaybackDecision {
                    source: *source,
                    format,
                    candidates: selected,
                    candidate_quality,
                    selected_quality,
                    attempted_candidates,
                    candidate_refusals,
                    exact_refusal: None,
                    source_preflight_refusal: None,
                    transform,
                    fidelity: f,
                    source_frame_bytes: frame,
                });
            }
            let repack = source.encoding == Encoding::DoP
                && usize::from(format.subslot_bytes) != frame / usize::from(source.channels);
            if repack && !policy.allow_lossless {
                return Err(Error::Unsupported {
                    reason: "DoP slot repacking is disallowed by policy".into(),
                    offered: vec![],
                });
            }
            let channels = format.channels;
            if repack {
                selected_quality.fidelity = PlaybackFidelity::BitExact;
                if let Some(q) = candidate_quality
                    .iter_mut()
                    .find(|q| q.request == format.request)
                {
                    q.fidelity = PlaybackFidelity::BitExact;
                }
            }
            return Ok(PlaybackDecision {
                source: *source,
                format,
                candidates: selected,
                candidate_quality,
                selected_quality,
                attempted_candidates,
                candidate_refusals,
                exact_refusal: None,
                source_preflight_refusal: None,
                transform: if repack {
                    PlaybackTransform::DopRepack {
                        target_channels: channels,
                    }
                } else {
                    PlaybackTransform::Direct
                },
                fidelity: if repack {
                    PlaybackFidelity::BitExact
                } else {
                    PlaybackFidelity::Exact
                },
                source_frame_bytes: frame,
            });
        }
        let (transform, fidelity) = if matches!(source.encoding, Encoding::DoP)
            && matches!(format.encoding, Encoding::DsdNative { .. })
        {
            (
                PlaybackTransform::DopToNative {
                    target_channels: format.channels,
                },
                if source.channels == format.channels {
                    PlaybackFidelity::BitExact
                } else {
                    PlaybackFidelity::Lossless
                },
            )
        } else if matches!(source.encoding, Encoding::DsdNative { .. })
            && matches!(format.encoding, Encoding::DoP)
        {
            (
                PlaybackTransform::NativeToDop {
                    target_channels: format.channels,
                },
                if source.channels == format.channels {
                    PlaybackFidelity::BitExact
                } else {
                    PlaybackFidelity::Lossless
                },
            )
        } else if matches!(source.encoding, Encoding::DsdNative { .. })
            && matches!(format.encoding, Encoding::DsdNative { .. })
        {
            (
                PlaybackTransform::DsdChannelExpansion {
                    target_channels: format.channels,
                },
                PlaybackFidelity::Lossless,
            )
        } else if let Some(bit_rate) = dsd_rate(source) {
            let mut options = policy.conversion_options;
            if format.sample_rate == bit_rate / 16 {
                options.headroom_db = 0.0;
            }
            (
                PlaybackTransform::DsdToPcm {
                    bit_rate,
                    dop_input: matches!(source.encoding, Encoding::DoP),
                    options,
                },
                PlaybackFidelity::Lossy,
            )
        } else {
            let mut pcm_source = *source;
            let unsigned = source.encoding == Encoding::PcmU8;
            if unsigned {
                pcm_source = StreamRequest::pcm(source.sample_rate, source.channels, 16);
            }
            let plan = ConversionPlan::between(&pcm_source, &format, &policy.conversion_options)
                .map_err(conversion_error)?;
            let fidelity = fidelity(plan.kind(), unsigned);
            if !enabled(policy, fidelity) {
                return Err(Error::InvalidState(
                    "negotiated conversion violates playback policy".into(),
                ));
            }
            (
                PlaybackTransform::Pcm {
                    plan,
                    unsigned_input: unsigned,
                },
                fidelity,
            )
        };
        Ok(PlaybackDecision {
            source: *source,
            format,
            candidates: selected,
            candidate_quality,
            selected_quality,
            attempted_candidates,
            candidate_refusals,
            exact_refusal: refusal,
            source_preflight_refusal,
            transform,
            fidelity,
            source_frame_bytes: frame,
        })
    }
    pub fn open_playback(
        &self,
        decision: &PlaybackDecision,
        listener: Arc<dyn DacListener>,
    ) -> Result<PlaybackStream> {
        let engine = match &decision.transform {
            PlaybackTransform::Direct => Engine::Direct,
            PlaybackTransform::Pcm {
                plan,
                unsigned_input,
            } => Engine::Pcm {
                converter: Converter::new(plan).map_err(conversion_error)?,
                unsigned: *unsigned_input,
            },
            PlaybackTransform::DopToNative { .. }
            | PlaybackTransform::DopRepack { .. }
            | PlaybackTransform::NativeToDop { .. }
            | PlaybackTransform::DsdChannelExpansion { .. } => Engine::DsdRepack,
            PlaybackTransform::DsdToPcm {
                bit_rate,
                dop_input,
                options,
            } => {
                let sample = if decision.format.encoding == Encoding::PcmFloat32 {
                    SampleFormat::Float32
                } else {
                    SampleFormat::int(decision.format.valid_bits, decision.format.subslot_bytes)
                        .map_err(conversion_error)?
                };
                let target = PcmFormat::new(
                    decision.format.sample_rate,
                    decision.format.channels,
                    sample,
                );
                let mut opts = *options;
                opts.source_sample = None;
                Engine::Dsd {
                    converter: DsdPcmConverter::with_options(
                        *bit_rate,
                        decision.source.channels,
                        target,
                        &opts,
                    )
                    .map_err(conversion_error)?,
                    dop: *dop_input,
                }
            }
        };
        let stream = self.open_stream(&decision.format, listener)?;
        if !matches!(&decision.transform, PlaybackTransform::Direct) {
            stream.set_host_processing(true);
        }
        Ok(PlaybackStream {
            stream,
            decision: decision.clone(),
            engine,
            input: Vec::with_capacity(65536),
            input_offset: 0,
            output: Vec::with_capacity(65536),
            output_offset: 0,
            marker: false,
            finishing: false,
            drain_called: false,
            source_bytes: 0,
            target_bytes: 0,
            observer: None,
            timing_observer: None,
        })
    }
}

enum Engine {
    Direct,
    DsdRepack,
    Pcm {
        converter: Converter,
        unsigned: bool,
    },
    Dsd {
        converter: DsdPcmConverter,
        dop: bool,
    },
}
fn emit_producer_timing(
    observer: Option<&ProducerTimingObserver>,
    started: Option<Instant>,
    phase: ProducerTimingPhase,
    input_bytes: usize,
    output_bytes: usize,
    blocked: bool,
) {
    if let (Some(started), Some(observer)) = (started, observer) {
        observer(ProducerTiming {
            phase,
            started,
            elapsed: started.elapsed(),
            input_bytes,
            output_bytes,
            blocked,
        });
    }
}
type Observer = Arc<dyn Fn(&[u8]) + Send + Sync>;
/// Independently timed source-side operation. No wall-clock time is inferred.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProducerTimingPhase {
    SourceFormat,
    ConverterProcess,
    ConverterFinish,
    RingEnqueue,
    Observer,
}
#[derive(Debug, Clone, Copy)]
pub struct ProducerTiming {
    pub phase: ProducerTimingPhase,
    pub started: Instant,
    pub elapsed: Duration,
    pub input_bytes: usize,
    pub output_bytes: usize,
    /// True when an enqueue accepts fewer bytes than requested.
    pub blocked: bool,
}
pub type ProducerTimingObserver = Arc<dyn Fn(ProducerTiming) + Send + Sync>;

/// One source-domain stream. Conversion and USB writes are nonblocking; a short
/// input count means the caller must retry the remainder. Observers see bytes
/// accepted by the core stream, before any native DSD wire packing/DoP marker rewrite.
pub struct PlaybackStream {
    stream: DacStream,
    decision: PlaybackDecision,
    engine: Engine,
    input: Vec<u8>,
    input_offset: usize,
    output: Vec<u8>,
    output_offset: usize,
    marker: bool,
    finishing: bool,
    drain_called: bool,
    source_bytes: u64,
    target_bytes: u64,
    observer: Option<Observer>,
    timing_observer: Option<ProducerTimingObserver>,
}
impl PlaybackStream {
    pub fn decision(&self) -> &PlaybackDecision {
        &self.decision
    }
    pub fn format(&self) -> &StreamFormat {
        &self.decision.format
    }
    pub fn set_observer(&mut self, observer: Option<Observer>) {
        self.observer = observer;
    }
    /// Install an optional synchronous, nonblocking instrumentation callback.
    /// `started` uses the process monotonic clock and can be aligned with the
    /// transport feedback trace's `started_at`. Callbacks must not block.
    pub fn set_timing_observer(&mut self, observer: Option<ProducerTimingObserver>) {
        self.timing_observer = observer;
    }
    fn timing_start(&self) -> Option<Instant> {
        self.timing_observer.as_ref().map(|_| Instant::now())
    }
    fn timing_end(
        &self,
        started: Option<Instant>,
        phase: ProducerTimingPhase,
        input_bytes: usize,
        output_bytes: usize,
        blocked: bool,
    ) {
        emit_producer_timing(
            self.timing_observer.as_ref(),
            started,
            phase,
            input_bytes,
            output_bytes,
            blocked,
        );
    }
    fn notify_observer(&self, bytes: &[u8]) {
        if let Some(observer) = &self.observer {
            let start = self.timing_start();
            observer(bytes);
            self.timing_end(
                start,
                ProducerTimingPhase::Observer,
                bytes.len(),
                bytes.len(),
                false,
            );
        }
    }
    fn flush_output(&mut self) -> Result<bool> {
        while self.output_offset < self.output.len() {
            let wanted = self.output.len() - self.output_offset;
            let start = self.timing_start();
            let write = self.stream.write(&self.output[self.output_offset..]);
            self.timing_end(
                start,
                ProducerTimingPhase::RingEnqueue,
                wanted,
                write.as_ref().copied().unwrap_or(0),
                write.as_ref().is_ok_and(|n| *n < wanted),
            );
            let n = write?;
            if n == 0 {
                return Ok(false);
            }
            self.notify_observer(&self.output[self.output_offset..self.output_offset + n]);
            self.output_offset += n;
            self.target_bytes += n as u64;
        }
        self.output.clear();
        self.output_offset = 0;
        Ok(true)
    }
    fn pump(&mut self) -> Result<bool> {
        if !self.flush_output()? {
            return Ok(false);
        }
        while self.input_offset < self.input.len() {
            self.output.resize(65536, 0);
            let start = self.timing_start();
            let processed = match &mut self.engine {
                Engine::Pcm { converter, .. } => {
                    converter.process(&self.input[self.input_offset..], &mut self.output)
                }
                Engine::Dsd { converter, .. } => {
                    converter.process(&self.input[self.input_offset..], &mut self.output)
                }
                Engine::Direct | Engine::DsdRepack => {
                    return Err(Error::InvalidState("direct stream has staged input".into()))
                }
            };
            self.timing_end(
                start,
                ProducerTimingPhase::ConverterProcess,
                processed.as_ref().map_or(0, |p| p.consumed),
                processed.as_ref().map_or(0, |p| p.produced),
                false,
            );
            let p = processed.map_err(conversion_error)?;
            self.input_offset += p.consumed;
            self.output.truncate(p.produced);
            if p.consumed == 0 && p.produced == 0 {
                return Err(Error::InvalidState("converter made no progress".into()));
            }
            if !self.flush_output()? {
                return Ok(false);
            }
        }
        self.input.clear();
        self.input_offset = 0;
        Ok(true)
    }
    /// Accepts whole source frames and returns bytes accepted, never target bytes.
    pub fn write(&mut self, data: &[u8]) -> Result<usize> {
        if self.finishing {
            return Err(Error::InvalidState("write after drain".into()));
        }
        let frame = self.decision.source_frame_bytes;
        if frame == 0 {
            return Err(Error::InvalidState("zero source frame".into()));
        }
        let n = (data.len() / frame).min(256) * frame;
        if n == 0 {
            return Ok(0);
        }
        if matches!(self.engine, Engine::Direct) {
            let mut phase = self.marker;
            if self.decision.source.encoding == Encoding::DoP {
                let start = self.timing_start();
                let checked = validate_dop(
                    &data[..n],
                    usize::from(self.decision.source.channels),
                    frame / usize::from(self.decision.source.channels),
                    &mut phase,
                );
                self.timing_end(start, ProducerTimingPhase::SourceFormat, n, n, false);
                checked?;
            }
            let start = self.timing_start();
            let write = self.stream.write(&data[..n]);
            self.timing_end(
                start,
                ProducerTimingPhase::RingEnqueue,
                n,
                write.as_ref().copied().unwrap_or(0),
                write.as_ref().is_ok_and(|taken| *taken < n),
            );
            let taken = write?;
            if self.decision.source.encoding == Encoding::DoP {
                let mut accepted = self.marker;
                let start = self.timing_start();
                let checked = validate_dop(
                    &data[..taken],
                    usize::from(self.decision.source.channels),
                    frame / usize::from(self.decision.source.channels),
                    &mut accepted,
                );
                self.timing_end(
                    start,
                    ProducerTimingPhase::SourceFormat,
                    taken,
                    taken,
                    false,
                );
                checked?;
                self.marker = accepted;
            }
            if taken > 0 {
                self.notify_observer(&data[..taken]);
            }
            self.source_bytes += taken as u64;
            self.target_bytes += taken as u64;
            return Ok(taken);
        }
        if matches!(self.engine, Engine::DsdRepack) {
            if !self.flush_output()? {
                return Ok(0);
            }
            return self.write_dsd_repack(data);
        }
        if !self.pump()? {
            return Ok(0);
        }
        let start = self.timing_start();
        self.input.clear();
        let formatted: Result<()> = (|| {
            match &self.engine {
                Engine::Pcm { unsigned: true, .. } => {
                    self.input.resize(n * 2, 0);
                    pcm_u8_to_i16(&data[..n], &mut self.input);
                }
                Engine::Pcm { .. } | Engine::Dsd { dop: false, .. } => {
                    self.input.extend_from_slice(&data[..n])
                }
                Engine::Dsd { dop: true, .. } => unpack_dop_into(
                    &data[..n],
                    usize::from(self.decision.source.channels),
                    frame / usize::from(self.decision.source.channels),
                    &mut self.marker,
                    &mut self.input,
                )?,
                Engine::Direct | Engine::DsdRepack => unreachable!(),
            };
            Ok(())
        })();
        self.timing_end(
            start,
            ProducerTimingPhase::SourceFormat,
            n,
            self.input.len(),
            false,
        );
        formatted?;
        self.input_offset = 0;
        self.source_bytes += n as u64;
        let _ = self.pump()?;
        Ok(n)
    }
    fn write_dsd_repack(&mut self, data: &[u8]) -> Result<usize> {
        let src_ch = usize::from(self.decision.source.channels);
        let dst_ch = usize::from(self.decision.format.channels);
        let source_dop = self.decision.source.encoding == Encoding::DoP;
        let target_dop = self.decision.format.encoding == Encoding::DoP;
        let source_frame = self.decision.source_frame_bytes;
        let target_slot = usize::from(self.decision.format.subslot_bytes);
        let group = if source_dop {
            if target_dop {
                1
            } else {
                target_slot.div_ceil(2)
            }
        } else if target_dop {
            2
        } else {
            target_slot
        };
        let frames = (data.len() / source_frame).min(256) / group * group;
        let taken = frames * source_frame;
        if taken == 0 {
            return Ok(0);
        }
        let start = self.timing_start();
        self.input.clear();
        if source_dop {
            unpack_dop_into(
                &data[..taken],
                src_ch,
                source_frame / src_ch,
                &mut self.marker,
                &mut self.input,
            )?;
        } else {
            self.input.extend_from_slice(&data[..taken]);
        }
        self.output.clear();
        self.output_offset = 0;
        if target_dop {
            for pair in self.input.chunks_exact(src_ch * 2) {
                for ch in 0..dst_ch {
                    let input_ch = if src_ch == 1 { 0 } else { ch };
                    if target_slot == 4 {
                        self.output.push(0);
                    }
                    self.output.push(pair[src_ch + input_ch]);
                    self.output.push(pair[input_ch]);
                    self.output.push(0);
                }
            }
        } else if src_ch == 1 && dst_ch == 2 {
            self.output.resize(self.input.len() * 2, 0);
            duplicate_mono_dsd(&self.input, &mut self.output);
        } else {
            self.output.extend_from_slice(&self.input);
        }
        self.timing_end(
            start,
            ProducerTimingPhase::SourceFormat,
            taken,
            self.output.len(),
            false,
        );
        self.input.clear();
        self.input_offset = 0;
        self.source_bytes += taken as u64;
        let _ = self.flush_output()?;
        Ok(taken)
    }
    /// Begin or progress drain. Returns false when the caller must retry after
    /// the USB ring makes space; `is_drained` reports actual USB completion.
    pub fn drain(&mut self) -> Result<bool> {
        self.finishing = true;
        if self.drain_called {
            return Ok(true);
        }
        if !self.pump()? {
            return Ok(false);
        }
        if !matches!(self.engine, Engine::Direct | Engine::DsdRepack) {
            loop {
                self.output.resize(65536, 0);
                let start = self.timing_start();
                let finished = match &mut self.engine {
                    Engine::Pcm { converter, .. } => converter.finish(&mut self.output),
                    Engine::Dsd { converter, .. } => converter.finish(&mut self.output),
                    Engine::Direct | Engine::DsdRepack => unreachable!(),
                };
                self.timing_end(
                    start,
                    ProducerTimingPhase::ConverterFinish,
                    0,
                    finished.as_ref().copied().unwrap_or(0),
                    false,
                );
                let n = finished.map_err(conversion_error)?;
                if n == 0 {
                    break;
                }
                self.output.truncate(n);
                if !self.flush_output()? {
                    return Ok(false);
                }
            }
        }
        self.stream.drain()?;
        self.drain_called = true;
        Ok(true)
    }
    pub fn play(&self) -> Result<()> {
        self.stream.play()
    }
    pub fn pause(&self) -> Result<()> {
        self.stream.pause()
    }
    pub fn flush(&mut self) -> Result<()> {
        self.stream.flush()?;
        match &mut self.engine {
            Engine::Pcm { converter, .. } => converter.reset(),
            Engine::Dsd { converter, .. } => converter.reset(),
            Engine::Direct | Engine::DsdRepack => {}
        }
        self.input.clear();
        self.output.clear();
        self.input_offset = 0;
        self.output_offset = 0;
        self.marker = false;
        self.finishing = false;
        self.drain_called = false;
        self.source_bytes = 0;
        self.target_bytes = 0;
        Ok(())
    }
    pub fn writable_bytes(&self) -> usize {
        if self.finishing
            || self.output_offset < self.output.len()
            || self.input_offset < self.input.len()
        {
            0
        } else if matches!(self.engine, Engine::Direct) {
            self.stream
                .writable_bytes()
                .min(256 * self.decision.source_frame_bytes)
        } else {
            256 * self.decision.source_frame_bytes
        }
    }
    pub fn is_drained(&self) -> bool {
        self.drain_called && self.stream.is_drained()
    }
    /// Source-relative statistics. A lossy or channel-expanding transform can
    /// never report source bit-perfect even if its target USB payload is intact.
    pub fn stats(&self) -> StreamStats {
        let mut stats = self.stream.stats();
        if matches!(
            self.decision.fidelity,
            PlaybackFidelity::Lossless | PlaybackFidelity::Lossy
        ) {
            stats.bit_perfect = false;
            if stats.bit_perfect_blocker.is_none() {
                stats.bit_perfect_blocker = Some(crate::BitPerfectBlocker::FormatChanged);
            }
        }
        stats
    }
    /// Raw target transport statistics; not a claim of source fidelity.
    pub fn target_stats(&self) -> StreamStats {
        self.stream.stats()
    }
    pub fn conversion_stats(&self) -> PlaybackConversionStats {
        let clipped_samples = match &self.engine {
            Engine::Pcm { converter, .. } => converter.clipped_samples(),
            Engine::Dsd { converter, .. } => converter.clipped_samples(),
            Engine::Direct | Engine::DsdRepack => 0,
        };
        PlaybackConversionStats {
            source_bytes: self.source_bytes,
            target_bytes: self.target_bytes,
            clipped_samples,
        }
    }
    pub fn played_frames(&self) -> u64 {
        let wire = self.stream.played_frames();
        let source_rate = match self.decision.source.encoding {
            Encoding::DsdNative { bit_rate } => bit_rate / 8,
            Encoding::DoP => self.decision.source.sample_rate,
            _ => self.decision.source.sample_rate,
        };
        let target_rate = self.decision.format.counter_rate_hz();
        if target_rate == 0 {
            return 0;
        }
        ((u128::from(wire) * u128::from(source_rate) / u128::from(target_rate)) as u64)
            .min(self.source_bytes / self.decision.source_frame_bytes as u64)
    }
    pub fn latency_frames(&self) -> u64 {
        (self.source_bytes / self.decision.source_frame_bytes as u64)
            .saturating_sub(self.played_frames())
    }
    pub fn close(self) -> Result<()> {
        self.stream.close()
    }
}
/// Duplicate chronological mono DSD bytes into stereo channel-interleaved bytes.
/// Returns the source bytes consumed, bounded by output capacity.
pub fn duplicate_mono_dsd(input: &[u8], output: &mut [u8]) -> usize {
    let count = input.len().min(output.len() / 2);
    for (sample, pair) in input[..count].iter().zip(output.chunks_exact_mut(2)) {
        pair[0] = *sample;
        pair[1] = *sample;
    }
    count
}

/// Convert unsigned 8-bit PCM to signed 16-bit PCM, with silence at 0x80.
/// Returns input samples consumed, bounded by output capacity.
pub fn pcm_u8_to_i16(input: &[u8], output: &mut [u8]) -> usize {
    let count = input.len().min(output.len() / 2);
    for (sample, slot) in input[..count].iter().zip(output.chunks_exact_mut(2)) {
        slot.copy_from_slice(&((i16::from(*sample) - 128) << 8).to_le_bytes());
    }
    count
}
fn validate_dop(data: &[u8], channels: usize, slot: usize, phase: &mut bool) -> Result<()> {
    if channels == 0 || !matches!(slot, 3 | 4) || data.len() % (channels * slot) != 0 {
        return Err(Error::InvalidArgument("invalid DoP frame layout".into()));
    }
    for frame in data.chunks_exact(channels * slot) {
        let marker = if *phase { 0xfa } else { 0x05 };
        if frame
            .chunks_exact(slot)
            .any(|w| w[slot - 1] != marker || (slot == 4 && w[0] != 0))
        {
            return Err(Error::InvalidArgument(
                "invalid DoP markers or padding".into(),
            ));
        }
        *phase = !*phase;
    }
    Ok(())
}
pub fn unpack_dop(data: &[u8], channels: usize, slot: usize, phase: &mut bool) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(data.len());
    unpack_dop_into(data, channels, slot, phase, &mut out)?;
    Ok(out)
}
fn unpack_dop_into(
    data: &[u8],
    channels: usize,
    slot: usize,
    phase: &mut bool,
    out: &mut Vec<u8>,
) -> Result<()> {
    let mut next = *phase;
    validate_dop(data, channels, slot, &mut next)?;
    for frame in data.chunks_exact(channels * slot) {
        for word in frame.chunks_exact(slot) {
            out.push(word[slot - 2]);
        }
        for word in frame.chunks_exact(slot) {
            out.push(word[slot - 3]);
        }
    }
    *phase = next;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn descriptor_alt(format: AltFormat, channels: u16, bits: u8) -> crate::AltSetting {
        crate::AltSetting {
            interface_number: 1,
            alt_setting: 1,
            endpoint_address: 1,
            format,
            channels,
            subslot_bytes: 4,
            valid_bits: bits,
            rate_ranges: vec![crate::RateRange {
                min_hz: 44_100,
                max_hz: 48_000,
                step_hz: 3_900,
            }],
            sample_rates: vec![44_100, 48_000],
            sync_type: crate::SyncKind::Asynchronous,
            feedback_endpoint: Some(0x81),
            max_packet_bytes: 776,
            service_interval_us: 125,
            uac_version: crate::UacVersion::V2,
            endpoint_interval: 1,
            usage: crate::EndpointUsage::Data,
            refresh: 0,
            synch_address: 0,
            pacing: Some(crate::Pacing::ExplicitFeedback { endpoint: 0x81 }),
        }
    }
    #[test]
    fn descriptor_preflight_only_excludes_proven_shapes_and_rates() {
        let pcm = descriptor_alt(AltFormat::Pcm, 2, 32);
        let native = descriptor_alt(AltFormat::Dsd, 2, 1);
        let alts = vec![pcm.clone(), native.clone()];
        assert!(
            descriptor_exclusion(false, &alts, &[], &[], &StreamRequest::pcm(8_000, 1, 16))
                .is_none()
        );
        assert!(
            descriptor_exclusion(true, &alts, &[], &[], &StreamRequest::pcm(8_000, 1, 16))
                .is_some()
        );
        assert!(
            descriptor_exclusion(true, &alts, &[], &[], &StreamRequest::pcm(8_000, 2, 32))
                .is_some()
        );
        assert!(
            descriptor_exclusion(true, &alts, &[], &[], &StreamRequest::pcm(44_100, 2, 32))
                .is_none()
        );
        assert!(descriptor_exclusion(
            true,
            &alts,
            &[],
            &[],
            &StreamRequest::dsd_native(2_822_400, 1)
        )
        .is_some());
        assert!(descriptor_exclusion(
            true,
            &alts,
            &[],
            &[],
            &StreamRequest::dsd_native(2_822_400, 2)
        )
        .is_some());
        let mut unknown = native.clone();
        unknown.rate_ranges.clear();
        assert!(descriptor_exclusion(
            true,
            &[pcm.clone(), unknown],
            &[],
            &[],
            &StreamRequest::dsd_native(2_822_400, 2)
        )
        .is_none());
        let mut opaque = pcm.clone();
        opaque.format = AltFormat::Other;
        assert!(descriptor_exclusion(
            true,
            &[pcm.clone(), opaque.clone()],
            &[],
            &[],
            &StreamRequest::pcm_u8(44_100, 2)
        )
        .is_none());
        assert!(descriptor_exclusion(
            true,
            &[pcm, opaque],
            &[(1, 1, 1)],
            &[],
            &StreamRequest::pcm_u8(44_100, 2)
        )
        .is_some());
    }
    #[test]
    fn uac1_exact_rate_exclusion_requires_complete_descriptor_witness() {
        let mut alt = descriptor_alt(AltFormat::Pcm, 2, 16);
        alt.uac_version = crate::UacVersion::V1;
        alt.subslot_bytes = 2;
        alt.rate_ranges = vec![
            crate::RateRange {
                min_hz: 44_100,
                max_hz: 44_100,
                step_hz: 0,
            },
            crate::RateRange {
                min_hz: 48_000,
                max_hz: 48_000,
                step_hz: 0,
            },
        ];
        alt.sample_rates = vec![44_100, 48_000];
        let source = StreamRequest::pcm(8_000, 2, 16);
        assert!(
            descriptor_exclusion(true, std::slice::from_ref(&alt), &[], &[], &source).is_none()
        );
        let excluded =
            descriptor_exclusion(true, std::slice::from_ref(&alt), &[], &[(1, 1, 1)], &source)
                .unwrap();
        assert!(excluded.contains("UAC1 Type-I"));
        assert!(descriptor_exclusion(
            true,
            std::slice::from_ref(&alt),
            &[],
            &[(1, 1, 1)],
            &StreamRequest::pcm(44_100, 2, 16)
        )
        .is_none());
        alt.rate_ranges.clear();
        alt.sample_rates.clear();
        assert!(descriptor_exclusion(true, &[alt], &[], &[(1, 1, 1)], &source).is_none());
    }
    #[test]
    fn policy_blocks_exact_before_negotiation() {
        let policy = PlaybackPolicy {
            max_pcm_rate: Some(48_000),
            ..PlaybackPolicy::default()
        };
        assert!(!exact_policy_allows(
            &StreamRequest::pcm(96_000, 2, 24),
            &policy
        ));
        assert!(exact_policy_allows(
            &StreamRequest::pcm(48_000, 2, 24),
            &policy
        ));
        assert!(!exact_policy_allows(
            &StreamRequest::dop(2_822_400, 2),
            &policy
        ));
        let native_disabled = PlaybackPolicy {
            allow_native_dsd: false,
            ..policy
        };
        assert!(!exact_policy_allows(
            &StreamRequest::dsd_native(2_822_400, 2),
            &native_disabled
        ));
    }
    #[test]
    fn rank_prefers_fidelity_over_rate() {
        let candidate = |fidelity, rate_rank| Candidate {
            request: StreamRequest::pcm(48_000, 2, 16),
            fidelity,
            rate_rank,
            channels_rank: 0,
            depth_rank: (0, 0),
            quality: candidate_quality(
                &StreamRequest::pcm(48_000, 2, 16),
                &StreamRequest::pcm(48_000, 2, 16),
                fidelity,
                &ConvertOptions::default(),
            ),
            upsample_excess: 0,
        };
        let mut items = vec![
            candidate(PlaybackFidelity::Lossy, 0),
            candidate(PlaybackFidelity::Lossless, 1),
            candidate(PlaybackFidelity::BitExact, 3),
        ];
        sort_candidates(&mut items);
        assert_eq!(
            items.iter().map(|v| v.fidelity).collect::<Vec<_>>(),
            [
                PlaybackFidelity::BitExact,
                PlaybackFidelity::Lossless,
                PlaybackFidelity::Lossy
            ]
        );
    }
    #[test]
    fn lossy_rank_maximizes_bandwidth_before_rate_family_and_avoids_extra_upsampling() {
        for source_rate in [88_200, 176_400, 352_800, 705_600] {
            let source = StreamRequest::pcm(source_rate, 2, 16);
            let candidate = |rate, rank| {
                let request = StreamRequest::pcm(rate, 2, 16);
                Candidate {
                    request,
                    fidelity: PlaybackFidelity::Lossy,
                    rate_rank: rank,
                    channels_rank: 0,
                    depth_rank: (0, 0),
                    quality: candidate_quality(
                        &source,
                        &request,
                        PlaybackFidelity::Lossy,
                        &ConvertOptions::default(),
                    ),
                    upsample_excess: rate.saturating_sub(source.sample_rate),
                }
            };
            let mut list = vec![candidate(44_100, 0), candidate(48_000, 1)];
            sort_candidates(&mut list);
            assert_eq!(list[0].request.sample_rate, 48_000);
        }
        let dsd = StreamRequest::dsd_native(2_822_400, 2);
        let a = candidate_quality(
            &dsd,
            &StreamRequest::pcm(44_100, 2, 16),
            PlaybackFidelity::Lossy,
            &ConvertOptions::default(),
        );
        let b = candidate_quality(
            &dsd,
            &StreamRequest::pcm(48_000, 2, 16),
            PlaybackFidelity::Lossy,
            &ConvertOptions::default(),
        );
        assert!(b.retained_bandwidth_hz.unwrap() > a.retained_bandwidth_hz.unwrap());
        let small = StreamRequest::pcm(8_000, 2, 16);
        let q1 = candidate_quality(
            &small,
            &StreamRequest::pcm(44_100, 2, 16),
            PlaybackFidelity::Lossy,
            &ConvertOptions::default(),
        );
        let q2 = candidate_quality(
            &small,
            &StreamRequest::pcm(48_000, 2, 16),
            PlaybackFidelity::Lossy,
            &ConvertOptions::default(),
        );
        assert_eq!(q1.retained_bandwidth_hz, q2.retained_bandwidth_hz);
        let make = |request, quality, rate_rank| Candidate {
            request,
            fidelity: PlaybackFidelity::Lossy,
            rate_rank,
            channels_rank: 0,
            depth_rank: (0, 0),
            upsample_excess: request.sample_rate - small.sample_rate,
            quality,
        };
        let mut same = vec![
            make(StreamRequest::pcm(48_000, 2, 16), q2, 0),
            make(StreamRequest::pcm(44_100, 2, 16), q1, 1),
        ];
        sort_candidates(&mut same);
        assert_eq!(same[0].request.sample_rate, 44_100);
    }
    #[test]
    fn producer_timing_records_observer_and_conversion_separately() {
        let records = Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = records.clone();
        let callback: ProducerTimingObserver =
            Arc::new(move |event| sink.lock().unwrap().push(event));
        let t = Instant::now();
        emit_producer_timing(
            Some(&callback),
            Some(t),
            ProducerTimingPhase::ConverterProcess,
            16,
            32,
            false,
        );
        let t = Instant::now();
        emit_producer_timing(
            Some(&callback),
            Some(t),
            ProducerTimingPhase::Observer,
            32,
            32,
            false,
        );
        let t = Instant::now();
        emit_producer_timing(
            Some(&callback),
            Some(t),
            ProducerTimingPhase::RingEnqueue,
            32,
            0,
            true,
        );
        let records = records.lock().unwrap();
        assert_eq!(
            records.iter().map(|v| v.phase).collect::<Vec<_>>(),
            [
                ProducerTimingPhase::ConverterProcess,
                ProducerTimingPhase::Observer,
                ProducerTimingPhase::RingEnqueue
            ]
        );
        assert!(records[2].blocked);
        assert_eq!(records[0].output_bytes, 32);
    }
    #[test]
    fn converter_supports_high_rate_and_dsd_512_families() {
        let options = ConvertOptions::default();
        let plan = ConversionPlan::pcm(
            PcmFormat::new(1_536_000, 2, SampleFormat::I16),
            PcmFormat::new(768_000, 2, SampleFormat::I24),
            &options,
        )
        .unwrap();
        assert_eq!(plan.kind(), ConversionKind::Lossy);
        for bit_rate in [22_579_200, 24_576_000] {
            let target = PcmFormat::new(bit_rate / 16, 2, SampleFormat::I24);
            assert!(DsdPcmConverter::with_options(bit_rate, 2, target, &options).is_ok());
        }
    }
    #[test]
    fn source_byte_transforms_are_exact_and_bounded() {
        let mut output = [0u8; 6];
        assert_eq!(pcm_u8_to_i16(&[0, 128, 255, 1], &mut output), 3);
        assert_eq!(output, [0, 128, 0, 0, 0, 127]);
        let mut stereo = [0u8; 4];
        assert_eq!(duplicate_mono_dsd(&[0x69, 0x96, 0xff], &mut stereo), 2);
        assert_eq!(stereo, [0x69, 0x69, 0x96, 0x96]);
        let mut phase = false;
        assert_eq!(
            unpack_dop(&[0x12, 0x34, 0x05, 0x56, 0x78, 0x05], 2, 3, &mut phase).unwrap(),
            [0x34, 0x78, 0x12, 0x56]
        );
        assert!(phase);
        assert!(unpack_dop(&[0x12, 0x34, 0x05, 0x56, 0x78, 0x05], 2, 3, &mut phase).is_err());
        assert!(phase);
        let mut phase = false;
        assert_eq!(
            unpack_dop(
                &[0, 0x12, 0x34, 0x05, 0, 0x56, 0x78, 0x05],
                2,
                4,
                &mut phase
            )
            .unwrap(),
            [0x34, 0x78, 0x12, 0x56]
        );
        let mut bad_phase = false;
        assert!(unpack_dop(&[1, 0x12, 0x34, 0x05], 1, 4, &mut bad_phase).is_err());
        assert!(!bad_phase);
    }
}
