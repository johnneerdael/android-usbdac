//! Independent ADC 3.0 / Basic Audio Device Definition 3.0 descriptor decoding.
//!
//! Layouts follow USB-IF ADC3 sections 4.3, 4.5 and 4.7 and BADD sections 4 and 8.
//! This module performs no USB requests. In particular, a cluster's six-byte HC
//! header must be checked before allocating its body and checked again on receipt.
use super::{DescriptorError, Iad};

fn invalid(message: &str) -> DescriptorError {
    DescriptorError::Invalid(format!("UAC3: {message}"))
}
fn le16(d: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([d[at], d[at + 1]])
}
fn le32(d: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([d[at], d[at + 1], d[at + 2], d[at + 3]])
}
fn descriptor(d: &[u8], subtype: u8, length: usize) -> Result<&[u8], DescriptorError> {
    if d.len() < length || d[0] as usize != length || d[1] != 0x24 || d[2] != subtype {
        return Err(invalid("wrong descriptor length, type or subtype"));
    }
    Ok(&d[..length])
}
fn entity(id: u8) -> Result<u8, DescriptorError> {
    if id == 0 {
        Err(invalid("zero entity ID"))
    } else {
        Ok(id)
    }
}

/// ADC3 controls use two bits per selector and retain their full 32-bit width.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Controls(pub u32);
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlAccess {
    Absent,
    ReadOnly,
    ReadWrite,
    Reserved,
}
impl Controls {
    pub fn access(self, selector: u8) -> ControlAccess {
        if !(1..=16).contains(&selector) {
            return ControlAccess::Absent;
        }
        match (self.0 >> (2 * (selector - 1))) & 3 {
            0 => ControlAccess::Absent,
            1 => ControlAccess::ReadOnly,
            3 => ControlAccess::ReadWrite,
            _ => ControlAccess::Reserved,
        }
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AsHeader {
    pub terminal_link: u8,
    pub controls: Controls,
    pub cluster_id: u16,
    pub formats: u64,
    pub subslot_size: u8,
    pub bit_resolution: u8,
    pub auxiliary_protocols: u16,
    pub control_size: u8,
}
pub fn parse_as_header(data: &[u8]) -> Result<AsHeader, DescriptorError> {
    let d = descriptor(data, 1, 23)?;
    entity(d[3])?;
    if le16(d, 8) == 0 {
        return Err(invalid("zero AS cluster ID"));
    }
    let formats = u64::from_le_bytes(d[10..18].try_into().unwrap());
    if formats == 0 || d[18] == 0 || d[19] == 0 || u16::from(d[19]) > u16::from(d[18]) * 8 {
        return Err(invalid("invalid AS format geometry"));
    }
    Ok(AsHeader {
        terminal_link: d[3],
        controls: Controls(le32(d, 4)),
        cluster_id: le16(d, 8),
        formats,
        subslot_size: d[18],
        bit_resolution: d[19],
        auxiliary_protocols: le16(d, 20),
        control_size: d[22],
    })
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcHeader {
    pub category: u8,
    pub total_length: u16,
    pub controls: Controls,
}
pub fn parse_ac_header(data: &[u8]) -> Result<AcHeader, DescriptorError> {
    let d = descriptor(data, 1, 10)?;
    if le16(d, 4) < 10 {
        return Err(invalid("AC total length shorter than header"));
    }
    Ok(AcHeader {
        category: d[3],
        total_length: le16(d, 4),
        controls: Controls(le32(d, 6)),
    })
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputTerminal {
    pub id: u8,
    pub terminal_type: u16,
    pub associated_terminal: u8,
    pub clock_id: u8,
    pub controls: Controls,
    pub cluster_id: u16,
    pub extended_terminal_id: u16,
    pub connectors_id: u16,
    pub string_id: u16,
}
pub fn parse_input_terminal(data: &[u8]) -> Result<InputTerminal, DescriptorError> {
    let d = descriptor(data, 2, 20)?;
    entity(d[3])?;
    entity(d[7])?;
    if le16(d, 12) == 0 {
        return Err(invalid("zero terminal cluster ID"));
    }
    Ok(InputTerminal {
        id: d[3],
        terminal_type: le16(d, 4),
        associated_terminal: d[6],
        clock_id: d[7],
        controls: Controls(le32(d, 8)),
        cluster_id: le16(d, 12),
        extended_terminal_id: le16(d, 14),
        connectors_id: le16(d, 16),
        string_id: le16(d, 18),
    })
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputTerminal {
    pub id: u8,
    pub terminal_type: u16,
    pub associated_terminal: u8,
    pub source_id: u8,
    pub clock_id: u8,
    pub controls: Controls,
    pub extended_terminal_id: u16,
    pub connectors_id: u16,
    pub string_id: u16,
}
pub fn parse_output_terminal(data: &[u8]) -> Result<OutputTerminal, DescriptorError> {
    let d = descriptor(data, 3, 19)?;
    entity(d[3])?;
    entity(d[7])?;
    entity(d[8])?;
    Ok(OutputTerminal {
        id: d[3],
        terminal_type: le16(d, 4),
        associated_terminal: d[6],
        source_id: d[7],
        clock_id: d[8],
        controls: Controls(le32(d, 9)),
        extended_terminal_id: le16(d, 13),
        connectors_id: le16(d, 15),
        string_id: le16(d, 17),
    })
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClockSource {
    pub id: u8,
    pub attributes: u8,
    pub controls: Controls,
    pub reference_terminal: u8,
    pub string_id: u16,
}
impl ClockSource {
    pub fn is_internal(&self) -> bool {
        self.attributes & 1 != 0
    }
    pub fn is_synchronous_to_sof(&self) -> bool {
        self.attributes & 2 != 0
    }
}
pub fn parse_clock_source(data: &[u8]) -> Result<ClockSource, DescriptorError> {
    let d = descriptor(data, 0x0b, 12)?;
    entity(d[3])?;
    if d[4] & !3 != 0 || d[4] == 2 {
        return Err(invalid("invalid ADC3 clock attributes"));
    }
    Ok(ClockSource {
        id: d[3],
        attributes: d[4],
        controls: Controls(le32(d, 5)),
        reference_terminal: d[9],
        string_id: le16(d, 10),
    })
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClockSelector {
    pub id: u8,
    pub sources: Vec<u8>,
    pub controls: Controls,
    pub string_id: u16,
}
pub fn parse_clock_selector(data: &[u8]) -> Result<ClockSelector, DescriptorError> {
    if data.len() < 5 || data[4] == 0 {
        return Err(invalid("missing clock selector pins"));
    }
    let n = data[4] as usize;
    let d = descriptor(data, 0x0c, 11 + n)?;
    entity(d[3])?;
    for &id in &d[5..5 + n] {
        entity(id)?;
    }
    Ok(ClockSelector {
        id: d[3],
        sources: d[5..5 + n].to_vec(),
        controls: Controls(le32(d, 5 + n)),
        string_id: le16(d, 9 + n),
    })
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClockMultiplier {
    pub id: u8,
    pub source_id: u8,
    pub controls: Controls,
    pub string_id: u16,
}
pub fn parse_clock_multiplier(data: &[u8]) -> Result<ClockMultiplier, DescriptorError> {
    let d = descriptor(data, 0x0d, 11)?;
    entity(d[3])?;
    entity(d[4])?;
    Ok(ClockMultiplier {
        id: d[3],
        source_id: d[4],
        controls: Controls(le32(d, 5)),
        string_id: le16(d, 9),
    })
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PowerDomain {
    pub id: u8,
    pub recovery_d1: u16,
    pub recovery_d2: u16,
    pub entities: Vec<u8>,
    pub string_id: u16,
}
impl PowerDomain {
    pub fn recovery_d1_micros(&self) -> u32 {
        u32::from(self.recovery_d1) * 50
    }
    pub fn recovery_d2_micros(&self) -> u32 {
        u32::from(self.recovery_d2) * 50
    }
}
pub fn parse_power_domain(data: &[u8]) -> Result<PowerDomain, DescriptorError> {
    if data.len() < 9 {
        return Err(invalid("truncated power domain"));
    }
    let n = data[8] as usize;
    let d = descriptor(data, 0x10, 11 + n)?;
    entity(d[3])?;
    for (i, &id) in d[9..9 + n].iter().enumerate() {
        entity(id)?;
        if id == d[3] || d[9..9 + i].contains(&id) {
            return Err(invalid("duplicate or self power-domain member"));
        }
    }
    Ok(PowerDomain {
        id: d[3],
        recovery_d1: le16(d, 4),
        recovery_d2: le16(d, 6),
        entities: d[9..9 + n].to_vec(),
        string_id: le16(d, 9 + n),
    })
}

/// Feature Unit controls include master channel 0 and one word per channel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeatureUnit {
    pub id: u8,
    pub source_id: u8,
    pub controls: Vec<Controls>,
    pub string_id: u16,
}
pub fn parse_feature_unit(data: &[u8]) -> Result<FeatureUnit, DescriptorError> {
    if data.len() < 11 {
        return Err(invalid("truncated ADC3 feature unit"));
    }
    let length = usize::from(data[0]);
    if length < 11 || (length - 7) % 4 != 0 {
        return Err(invalid("invalid ADC3 feature control words"));
    }
    let d = descriptor(data, 7, length)?;
    entity(d[3])?;
    entity(d[4])?;
    Ok(FeatureUnit {
        id: d[3],
        source_id: d[4],
        controls: (5..length - 2)
            .step_by(4)
            .map(|at| Controls(le32(d, at)))
            .collect(),
        string_id: le16(d, length - 2),
    })
}

/// Allocation bound for one USB control transfer. Callers may impose a lower bound.
pub const MAX_CLUSTER_LENGTH: usize = u16::MAX as usize;
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClusterHeader {
    pub length: u16,
    pub id: u16,
}
/// Validate exactly the six bytes returned by the first HIGH_CAPABILITY request.
pub fn parse_cluster_header(
    data: &[u8],
    requested_id: u16,
    max_length: usize,
) -> Result<ClusterHeader, DescriptorError> {
    if data.len() != 6 || requested_id == 0 {
        return Err(invalid("invalid HC header transfer or ID"));
    }
    let length = le16(data, 0);
    if data[2] != 0x26
        || data[3] != 0
        || le16(data, 4) != requested_id
        || length < 7
        || usize::from(length) > max_length.min(MAX_CLUSTER_LENGTH)
    {
        return Err(invalid("invalid HC cluster type, ID or allocation length"));
    }
    Ok(ClusterHeader {
        length,
        id: requested_id,
    })
}
/// Spatial channel identities in ADC3 wire order, Appendix A.12.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum SpatialPosition {
    FrontLeft = 0x80,
    FrontRight = 0x81,
    FrontCenter = 0x82,
    FrontLeftOfCenter = 0x83,
    FrontRightOfCenter = 0x84,
    FrontWideLeft = 0x85,
    FrontWideRight = 0x86,
    SideLeft = 0x87,
    SideRight = 0x88,
    SurroundArrayLeft = 0x89,
    SurroundArrayRight = 0x8a,
    BackLeft = 0x8b,
    BackRight = 0x8c,
    BackCenter = 0x8d,
    BackLeftOfCenter = 0x8e,
    BackRightOfCenter = 0x8f,
    BackWideLeft = 0x90,
    BackWideRight = 0x91,
    TopCenter = 0x92,
    TopFrontLeft = 0x93,
    TopFrontRight = 0x94,
    TopFrontCenter = 0x95,
    TopFrontLeftOfCenter = 0x96,
    TopFrontRightOfCenter = 0x97,
    TopFrontWideLeft = 0x98,
    TopFrontWideRight = 0x99,
    TopSideLeft = 0x9a,
    TopSideRight = 0x9b,
    TopSurroundArrayLeft = 0x9c,
    TopSurroundArrayRight = 0x9d,
    TopBackLeft = 0x9e,
    TopBackRight = 0x9f,
    TopBackCenter = 0xa0,
    TopBackLeftOfCenter = 0xa1,
    TopBackRightOfCenter = 0xa2,
    TopBackWideLeft = 0xa3,
    TopBackWideRight = 0xa4,
    BottomCenter = 0xa5,
    BottomFrontLeft = 0xa6,
    BottomFrontRight = 0xa7,
    BottomFrontCenter = 0xa8,
    BottomFrontLeftOfCenter = 0xa9,
    BottomFrontRightOfCenter = 0xaa,
    BottomFrontWideLeft = 0xab,
    BottomFrontWideRight = 0xac,
    BottomSideLeft = 0xad,
    BottomSideRight = 0xae,
    BottomSurroundArrayLeft = 0xaf,
    BottomSurroundArrayRight = 0xb0,
    BottomBackLeft = 0xb1,
    BottomBackRight = 0xb2,
    BottomBackCenter = 0xb3,
    BottomBackLeftOfCenter = 0xb4,
    BottomBackRightOfCenter = 0xb5,
    BottomBackWideLeft = 0xb6,
    BottomBackWideRight = 0xb7,
    LowFrequencyEffects = 0xb8,
    LfeLeft = 0xb9,
    LfeRight = 0xba,
    HeadphoneLeft = 0xbb,
    HeadphoneRight = 0xbc,
}
impl SpatialPosition {
    pub fn from_code(code: u8) -> Option<Self> {
        match code {
            0x80 => Some(Self::FrontLeft),
            0x81 => Some(Self::FrontRight),
            0x82 => Some(Self::FrontCenter),
            0x83 => Some(Self::FrontLeftOfCenter),
            0x84 => Some(Self::FrontRightOfCenter),
            0x85 => Some(Self::FrontWideLeft),
            0x86 => Some(Self::FrontWideRight),
            0x87 => Some(Self::SideLeft),
            0x88 => Some(Self::SideRight),
            0x89 => Some(Self::SurroundArrayLeft),
            0x8a => Some(Self::SurroundArrayRight),
            0x8b => Some(Self::BackLeft),
            0x8c => Some(Self::BackRight),
            0x8d => Some(Self::BackCenter),
            0x8e => Some(Self::BackLeftOfCenter),
            0x8f => Some(Self::BackRightOfCenter),
            0x90 => Some(Self::BackWideLeft),
            0x91 => Some(Self::BackWideRight),
            0x92 => Some(Self::TopCenter),
            0x93 => Some(Self::TopFrontLeft),
            0x94 => Some(Self::TopFrontRight),
            0x95 => Some(Self::TopFrontCenter),
            0x96 => Some(Self::TopFrontLeftOfCenter),
            0x97 => Some(Self::TopFrontRightOfCenter),
            0x98 => Some(Self::TopFrontWideLeft),
            0x99 => Some(Self::TopFrontWideRight),
            0x9a => Some(Self::TopSideLeft),
            0x9b => Some(Self::TopSideRight),
            0x9c => Some(Self::TopSurroundArrayLeft),
            0x9d => Some(Self::TopSurroundArrayRight),
            0x9e => Some(Self::TopBackLeft),
            0x9f => Some(Self::TopBackRight),
            0xa0 => Some(Self::TopBackCenter),
            0xa1 => Some(Self::TopBackLeftOfCenter),
            0xa2 => Some(Self::TopBackRightOfCenter),
            0xa3 => Some(Self::TopBackWideLeft),
            0xa4 => Some(Self::TopBackWideRight),
            0xa5 => Some(Self::BottomCenter),
            0xa6 => Some(Self::BottomFrontLeft),
            0xa7 => Some(Self::BottomFrontRight),
            0xa8 => Some(Self::BottomFrontCenter),
            0xa9 => Some(Self::BottomFrontLeftOfCenter),
            0xaa => Some(Self::BottomFrontRightOfCenter),
            0xab => Some(Self::BottomFrontWideLeft),
            0xac => Some(Self::BottomFrontWideRight),
            0xad => Some(Self::BottomSideLeft),
            0xae => Some(Self::BottomSideRight),
            0xaf => Some(Self::BottomSurroundArrayLeft),
            0xb0 => Some(Self::BottomSurroundArrayRight),
            0xb1 => Some(Self::BottomBackLeft),
            0xb2 => Some(Self::BottomBackRight),
            0xb3 => Some(Self::BottomBackCenter),
            0xb4 => Some(Self::BottomBackLeftOfCenter),
            0xb5 => Some(Self::BottomBackRightOfCenter),
            0xb6 => Some(Self::BottomBackWideLeft),
            0xb7 => Some(Self::BottomBackWideRight),
            0xb8 => Some(Self::LowFrequencyEffects),
            0xb9 => Some(Self::LfeLeft),
            0xba => Some(Self::LfeRight),
            0xbb => Some(Self::HeadphoneLeft),
            0xbc => Some(Self::HeadphoneRight),
            _ => None,
        }
    }
}

/// ADC3 relationships are not a UAC1/2 speaker-position bitmap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelRelationship {
    Undefined,
    Mono,
    Left,
    Right,
    Array,
    Pattern(u8),
    /// Defined spatial position code in ADC3 Appendix A.12 (0x80 through 0xbc).
    Spatial(SpatialPosition),
    Unknown(u8),
}
impl From<u8> for ChannelRelationship {
    fn from(value: u8) -> Self {
        match value {
            0 => Self::Undefined,
            1 => Self::Mono,
            2 => Self::Left,
            3 => Self::Right,
            4 => Self::Array,
            0x20..=0x25 => Self::Pattern(value),
            0x80..=0xbc => Self::Spatial(SpatialPosition::from_code(value).unwrap()),
            _ => Self::Unknown(value),
        }
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Channel {
    Information {
        purpose: u8,
        relationship: ChannelRelationship,
        group: u8,
    },
    Ambisonic {
        ordering: u8,
        acn: u8,
        normalization: u8,
        group: u8,
    },
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cluster {
    pub id: u16,
    pub channels: Vec<Channel>,
}
/// Validate body length and ID against the previously validated first read.
/// Optional unknown segments are bounded and skipped, never interpreted as speakers.
pub fn parse_cluster(data: &[u8], expected: ClusterHeader) -> Result<Cluster, DescriptorError> {
    if data.len() != usize::from(expected.length) || data.len() < 7 {
        return Err(invalid("HC body transfer length changed"));
    }
    if parse_cluster_header(&data[..6], expected.id, usize::from(expected.length))? != expected {
        return Err(invalid("HC body header changed"));
    }
    let count = data[6] as usize;
    if count == 0 {
        return if data.len() == 7 {
            Ok(Cluster {
                id: expected.id,
                channels: vec![],
            })
        } else {
            Err(invalid("empty cluster has trailing blocks"))
        };
    }
    let mut channels = Vec::with_capacity(count);
    let mut current = None;
    let mut common = false;
    let mut common_done = false;
    let mut block_open = false;
    let mut at = 7;
    while at < data.len() {
        if data.len() - at < 3 {
            return Err(invalid("truncated cluster segment"));
        }
        let length = usize::from(le16(data, at));
        if length < 3 || length > data.len() - at {
            return Err(invalid("invalid cluster segment length"));
        }
        let d = &data[at..at + length];
        match d[2] {
            0xff => {
                if length != 3 {
                    return Err(invalid("invalid end segment"));
                }
                block_open = false;
                if common {
                    common = false;
                    common_done = true;
                } else {
                    channels.push(current.take().ok_or_else(|| {
                        invalid("channel has no information or ambisonic segment")
                    })?);
                }
            }
            0x01 | 0x1f => {
                if common_done || current.is_some() || !channels.is_empty() {
                    return Err(invalid("common segment inside channel block"));
                }
                if d[2] == 1 && length != 5 {
                    return Err(invalid("invalid cluster description length"));
                }
                common = true;
                block_open = true;
            }
            0x20 | 0x21 => {
                if common || current.is_some() {
                    return Err(invalid(
                        "unterminated common block or duplicate channel identity",
                    ));
                }
                common_done = true;
                block_open = true;
                current = Some(if d[2] == 0x20 {
                    if length != 6 {
                        return Err(invalid("invalid channel information length"));
                    }
                    Channel::Information {
                        purpose: d[3],
                        relationship: d[4].into(),
                        group: d[5],
                    }
                } else {
                    if length != 7 {
                        return Err(invalid("invalid ambisonic segment length"));
                    }
                    Channel::Ambisonic {
                        ordering: d[3],
                        acn: d[4],
                        normalization: d[5],
                        group: d[6],
                    }
                });
            }
            // Channel description and vendor-specific data do not change wire order.
            0x22 | 0xfe => {
                if common {
                    return Err(invalid("channel segment inside common block"));
                }
                if d[2] == 0x22 && length != 5 {
                    return Err(invalid("invalid channel description length"));
                }
                common_done = true;
                block_open = true;
            }
            _ => return Err(invalid("reserved cluster segment type")),
        }
        if channels.len() > count {
            return Err(invalid("too many cluster channel blocks"));
        }
        at += length;
    }
    if block_open || common || current.is_some() || channels.len() != count {
        return Err(invalid("unterminated or missing cluster blocks"));
    }
    Ok(Cluster {
        id: expected.id,
        channels,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FunctionProfile {
    Full,
    Badd(BaddProfile),
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BaddProfile {
    GenericIo,
    Headphone,
    Speaker,
    Microphone,
    Headset,
    HeadsetAdapter,
    Speakerphone,
}
/// Require protocol agreement before interpreting the IAD profile.
pub fn function_profile(
    iad: &Iad,
    interface_protocol: u8,
) -> Result<FunctionProfile, DescriptorError> {
    if iad.b_function_class != 1
        || iad.b_function_protocol != 0x30
        || interface_protocol != 0x30
        || iad.b_interface_count == 0
        || u16::from(iad.b_first_interface) + u16::from(iad.b_interface_count) > 256
    {
        return Err(invalid("not an ADC3 audio function"));
    }
    let p = match iad.b_function_sub_class {
        1 => return Ok(FunctionProfile::Full),
        0x20 => BaddProfile::GenericIo,
        0x21 => BaddProfile::Headphone,
        0x22 => BaddProfile::Speaker,
        0x23 => BaddProfile::Microphone,
        0x24 => BaddProfile::Headset,
        0x25 => BaddProfile::HeadsetAdapter,
        0x26 => BaddProfile::Speakerphone,
        _ => return Err(invalid("reserved ADC3 function profile")),
    };
    let n = iad.b_interface_count;
    if match p {
        BaddProfile::GenericIo => !(2..=3).contains(&n),
        BaddProfile::Headphone | BaddProfile::Speaker | BaddProfile::Microphone => n != 2,
        _ => n != 3,
    } {
        return Err(invalid("BADD profile interface count mismatch"));
    }
    Ok(FunctionProfile::Badd(p))
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BusSpeed {
    Full,
    High,
    Super,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Playback,
    Capture,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Synchronization {
    Synchronous,
    Asynchronous,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BaddFormat {
    pub direction: Direction,
    pub synchronization: Synchronization,
    pub sample_rate: u32,
    pub channels: u8,
    pub subslot_size: u8,
    pub bit_resolution: u8,
    pub clock_id: u8,
    pub power_domain_id: u8,
}
/// Infer only mandatory Continuous Mode alternate #1. Bursting alternates need
/// their own bandwidth interpretation and must not reuse this packet-size table.
pub fn infer_badd_format(
    profile: BaddProfile,
    speed: BusSpeed,
    alternate: u8,
    endpoint: &[u8],
    explicit_feedback: bool,
) -> Result<BaddFormat, DescriptorError> {
    if endpoint.len() < 7 || endpoint[0] != 7 || endpoint[1] != 5 || alternate != 1 {
        return Err(invalid(
            "BADD requires a standard endpoint in continuous alternate 1",
        ));
    }
    let address = endpoint[2];
    if address & 0x0f == 0 || address & 0x70 != 0 {
        return Err(invalid("invalid BADD endpoint address"));
    }
    let direction = if address & 0x80 == 0 {
        Direction::Playback
    } else {
        Direction::Capture
    };
    let synchronization = match endpoint[3] {
        0x05 => Synchronization::Asynchronous,
        0x0d => Synchronization::Synchronous,
        _ => {
            return Err(invalid(
                "BADD requires synchronous or asynchronous audio data endpoint",
            ))
        }
    };
    let required_interval = if speed == BusSpeed::Full { 1 } else { 4 };
    if endpoint[6] != required_interval {
        return Err(invalid("BADD continuous service interval must be 1 ms"));
    }
    let capacity = le16(endpoint, 4);
    let asynchronous = synchronization == Synchronization::Asynchronous;
    let (channels, subslot_size) = match (capacity, asynchronous) {
        (96, false) | (98, true) => (1, 2),
        (144, false) | (147, true) => (1, 3),
        (192, false) | (196, true) => (2, 2),
        (288, false) | (294, true) => (2, 3),
        _ => return Err(invalid("BADD capacity does not match synchronization type")),
    };
    let capture = direction == Direction::Capture;
    if (capture && matches!(profile, BaddProfile::Headphone | BaddProfile::Speaker))
        || (!capture && profile == BaddProfile::Microphone)
        || (capture
            && matches!(profile, BaddProfile::Headset | BaddProfile::HeadsetAdapter)
            && channels != 1)
        || (profile == BaddProfile::Speakerphone && channels != 1)
    {
        return Err(invalid(
            "BADD endpoint direction or channels conflict with profile",
        ));
    }
    if !capture && asynchronous && !explicit_feedback {
        return Err(invalid("asynchronous BADD sink needs explicit feedback"));
    }
    Ok(BaddFormat {
        direction,
        synchronization,
        sample_rate: 48_000,
        channels,
        subslot_size,
        bit_resolution: subslot_size * 8,
        clock_id: 9,
        power_domain_id: if capture { 11 } else { 10 },
    })
}

#[cfg(test)]
mod tests;
