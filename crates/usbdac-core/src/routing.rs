//! Pure USB Audio 1/2/3 routing inventory and control transactions.
//!
//! Independent implementation of USB-IF Audio 2.0 sections 4.7.2.6–7,
//! 5.2.2–3 and 5.2.5.5–6 (September 2026 errata). Mixer gains use signed
//! 1/256 dB units; `i16::MIN` means silence. A mixer crosspoint is a gain,
//! not an on/off switch. Fixed crosspoints remain readable.
//!
//! This module does not discover devices, claim interfaces, stop streams, or
//! implement vendor protocols (including Scarlett). The caller must resolve
//! upstream channel clusters, serialize control I/O and exclude concurrent
//! topology changes before using an inventory or applying a transaction.

use std::collections::BTreeMap;

pub use crate::transport::routing_hw::{
    RouteEdit, RoutingApplyError, RoutingDiagnostic, RoutingHandBackOutcome, RoutingHandBackReport,
    RoutingSessionError, RoutingSnapshot,
};

/// AudioControl interface and entity identity, scoped to the active configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Entity {
    pub interface: u8,
    pub id: u8,
}

/// Every pin and channel here is one-based, in descriptor/wire order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ControlKey {
    Selector(Entity),
    Crosspoint {
        entity: Entity,
        input_pin: u8,
        input_channel: u8,
        output_channel: u8,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    Absent,
    ReadOnly,
    ReadWrite,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InvalidRoute {
    MalformedDescriptor,
    ReservedCapability,
    UnknownInputChannels,
    TooManyCrosspoints,
    DuplicateEntity,
    UnknownControl,
    NotReadable,
    NotWritable,
    WrongValueType,
    InvalidPin,
    InvalidRange,
    GainOutsideRange,
    DuplicateChange,
    TooManyChanges,
}

/// A source cluster already resolved under this AudioControl interface.
/// Supply exactly one entry per mixer input pin; never infer these counts from
/// the bitmap length or substitute a default for an unresolved upstream entity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InputCluster {
    pub source_id: u8,
    pub channels: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoutingProtocol {
    Uac1,
    Uac2,
    Adc3,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectorUnit {
    entity: Entity,
    sources: Vec<u8>,
    access: Access,
    protocol: RoutingProtocol,
}

impl SelectorUnit {
    /// Parse exactly one UAC2 Selector Unit descriptor (subtype 0x05).
    /// This reports descriptor capabilities, not empirically tested hardware.
    pub fn parse(interface: u8, bytes: &[u8]) -> Result<Self, InvalidRoute> {
        let pins = header(bytes, 0x05)?;
        if bytes.len() != 7 + pins || bytes[5..5 + pins].contains(&0) {
            return Err(InvalidRoute::MalformedDescriptor);
        }
        let flags = bytes[5 + pins];
        if flags & !3 != 0 {
            return Err(InvalidRoute::ReservedCapability);
        }
        Ok(Self {
            entity: Entity {
                interface,
                id: bytes[3],
            },
            sources: bytes[5..5 + pins].to_vec(),
            access: access(flags)?,
            protocol: RoutingProtocol::Uac2,
        })
    }

    /// UAC1 selectors have mandatory GET_CUR/SET_CUR and no access bitmap.
    pub fn parse_uac1(interface: u8, bytes: &[u8]) -> Result<Self, InvalidRoute> {
        let pins = header(bytes, 5)?;
        if bytes.len() != 6 + pins || bytes[5..5 + pins].contains(&0) {
            return Err(InvalidRoute::MalformedDescriptor);
        }
        Ok(Self {
            entity: Entity {
                interface,
                id: bytes[3],
            },
            sources: bytes[5..5 + pins].to_vec(),
            access: Access::ReadWrite,
            protocol: RoutingProtocol::Uac1,
        })
    }

    /// Full ADC3 Selector Unit, including its 32-bit access field.
    pub fn parse_adc3(interface: u8, bytes: &[u8]) -> Result<Self, InvalidRoute> {
        let pins = header(bytes, 6)?;
        if bytes.len() != 11 + pins || bytes[5..5 + pins].contains(&0) {
            return Err(InvalidRoute::MalformedDescriptor);
        }
        let flags = u32::from_le_bytes(bytes[5 + pins..9 + pins].try_into().unwrap());
        if flags & !3 != 0 {
            return Err(InvalidRoute::ReservedCapability);
        }
        let access = access(flags as u8)?;
        if access == Access::Absent {
            return Err(InvalidRoute::ReservedCapability);
        }
        Ok(Self {
            entity: Entity {
                interface,
                id: bytes[3],
            },
            sources: bytes[5..5 + pins].to_vec(),
            access,
            protocol: RoutingProtocol::Adc3,
        })
    }

    pub fn entity(&self) -> Entity {
        self.entity
    }
    pub fn sources(&self) -> &[u8] {
        &self.sources
    }
    pub fn access(&self) -> Access {
        self.access
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MixerUnit {
    entity: Entity,
    inputs: Vec<InputCluster>,
    outputs: u8,
    programmable: Vec<u8>,
    protocol: RoutingProtocol,
}

impl MixerUnit {
    /// Parse exactly one UAC2 Mixer Unit descriptor (subtype 0x04), only after
    /// resolving the channel count of every source in `baSourceID`.
    pub fn parse(
        interface: u8,
        bytes: &[u8],
        inputs: &[InputCluster],
    ) -> Result<Self, InvalidRoute> {
        let pins = header(bytes, 0x04)?;
        if bytes.len() < 13 + pins {
            return Err(InvalidRoute::MalformedDescriptor);
        }
        if inputs.len() != pins
            || inputs
                .iter()
                .enumerate()
                .any(|(i, c)| c.channels == 0 || c.source_id == 0 || c.source_id != bytes[5 + i])
        {
            return Err(InvalidRoute::UnknownInputChannels);
        }
        let outputs = bytes[5 + pins];
        if outputs == 0 {
            return Err(InvalidRoute::MalformedDescriptor);
        }
        let channels: usize = inputs.iter().map(|c| usize::from(c.channels)).sum();
        let cells = channels * usize::from(outputs);
        // MCN is an unsigned byte, numbered from zero (not an input channel).
        if cells > 256 {
            return Err(InvalidRoute::TooManyCrosspoints);
        }
        let bitmap_len = cells.div_ceil(8);
        if bytes.len() != 13 + pins + bitmap_len {
            return Err(InvalidRoute::MalformedDescriptor);
        }
        let start = 11 + pins;
        let flags = bytes[start + bitmap_len];
        if flags & 0xc0 != 0 {
            return Err(InvalidRoute::ReservedCapability);
        }
        for shift in [0, 2, 4] {
            access((flags >> shift) & 3)?;
        }
        let bitmap = &bytes[start..start + bitmap_len];
        if cells % 8 != 0 && bitmap[bitmap_len - 1] & ((1 << (8 - cells % 8)) - 1) != 0 {
            return Err(InvalidRoute::MalformedDescriptor);
        }
        Ok(Self {
            entity: Entity {
                interface,
                id: bytes[3],
            },
            inputs: inputs.to_vec(),
            outputs,
            programmable: bitmap.to_vec(),
            protocol: RoutingProtocol::Uac2,
        })
    }

    /// UAC1 uses the descriptor's explicit four-byte output cluster.
    pub fn parse_uac1(
        interface: u8,
        bytes: &[u8],
        inputs: &[InputCluster],
    ) -> Result<Self, InvalidRoute> {
        Self::parse_other(interface, bytes, inputs, None, RoutingProtocol::Uac1)
    }

    /// Full ADC3 output count must come from the referenced, queried HC Cluster.
    pub fn parse_adc3(
        interface: u8,
        bytes: &[u8],
        inputs: &[InputCluster],
        outputs: u8,
    ) -> Result<Self, InvalidRoute> {
        Self::parse_other(
            interface,
            bytes,
            inputs,
            Some(outputs),
            RoutingProtocol::Adc3,
        )
    }

    fn parse_other(
        interface: u8,
        bytes: &[u8],
        inputs: &[InputCluster],
        outputs: Option<u8>,
        protocol: RoutingProtocol,
    ) -> Result<Self, InvalidRoute> {
        let pins = header(
            bytes,
            if protocol == RoutingProtocol::Adc3 {
                5
            } else {
                4
            },
        )?;
        if bytes.len() < 5 + pins {
            return Err(InvalidRoute::MalformedDescriptor);
        }
        if inputs.len() != pins
            || inputs
                .iter()
                .enumerate()
                .any(|(i, c)| c.channels == 0 || c.source_id == 0 || c.source_id != bytes[5 + i])
        {
            return Err(InvalidRoute::UnknownInputChannels);
        }
        let outputs = outputs
            .or_else(|| bytes.get(5 + pins).copied())
            .ok_or(InvalidRoute::MalformedDescriptor)?;
        if outputs == 0 {
            return Err(InvalidRoute::UnknownInputChannels);
        }
        let channels: usize = inputs.iter().map(|c| usize::from(c.channels)).sum();
        let cells = channels * usize::from(outputs);
        if (protocol != RoutingProtocol::Uac1 && cells > 256)
            || (protocol == RoutingProtocol::Uac1 && channels > 254)
        {
            return Err(InvalidRoute::TooManyCrosspoints);
        }
        let count = cells.div_ceil(8);
        let (base, start) = if protocol == RoutingProtocol::Adc3 {
            (13, 7)
        } else {
            (10, 9)
        };
        if bytes.len() != base + pins + count {
            return Err(InvalidRoute::MalformedDescriptor);
        }
        if protocol == RoutingProtocol::Adc3 {
            if u16::from_le_bytes([bytes[5 + pins], bytes[6 + pins]]) == 0 {
                return Err(InvalidRoute::MalformedDescriptor);
            }
            let at = start + pins + count;
            let flags = u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap());
            if flags & !15 != 0 {
                return Err(InvalidRoute::ReservedCapability);
            }
            access((flags & 3) as u8)?;
            access(((flags >> 2) & 3) as u8)?;
        }
        let bitmap = &bytes[start + pins..start + pins + count];
        if cells % 8 != 0 && bitmap[count - 1] & ((1 << (8 - cells % 8)) - 1) != 0 {
            return Err(InvalidRoute::MalformedDescriptor);
        }
        Ok(Self {
            entity: Entity {
                interface,
                id: bytes[3],
            },
            inputs: inputs.to_vec(),
            outputs,
            programmable: bitmap.to_vec(),
            protocol,
        })
    }

    pub fn entity(&self) -> Entity {
        self.entity
    }
    pub fn inputs(&self) -> &[InputCluster] {
        &self.inputs
    }
    pub fn output_channels(&self) -> u8 {
        self.outputs
    }

    pub fn crosspoint(
        &self,
        pin: u8,
        channel: u8,
        output: u8,
    ) -> Result<RouteControl, InvalidRoute> {
        let input = pin
            .checked_sub(1)
            .and_then(|p| self.inputs.get(usize::from(p)))
            .ok_or(InvalidRoute::UnknownControl)?;
        if channel == 0 || channel > input.channels || output == 0 || output > self.outputs {
            return Err(InvalidRoute::UnknownControl);
        }
        let previous: usize = self.inputs[..usize::from(pin - 1)]
            .iter()
            .map(|c| usize::from(c.channels))
            .sum();
        let mcn = (previous + usize::from(channel - 1)) * usize::from(self.outputs)
            + usize::from(output - 1);
        let writable = self.programmable[mcn / 8] & (0x80 >> (mcn % 8)) != 0;
        Ok(RouteControl {
            key: ControlKey::Crosspoint {
                entity: self.entity,
                input_pin: pin,
                input_channel: channel,
                output_channel: output,
            },
            access: if writable {
                Access::ReadWrite
            } else {
                Access::ReadOnly
            },
            mcn: mcn as u8,
            input_number: (previous + usize::from(channel)) as u8,
            protocol: self.protocol,
            pins: None,
        })
    }
}

fn header(bytes: &[u8], subtype: u8) -> Result<usize, InvalidRoute> {
    if bytes.len() < 5
        || usize::from(bytes[0]) != bytes.len()
        || bytes[1] != 0x24
        || bytes[2] != subtype
        || bytes[3] == 0
        || bytes[4] == 0
    {
        return Err(InvalidRoute::MalformedDescriptor);
    }
    Ok(usize::from(bytes[4]))
}

fn access(bits: u8) -> Result<Access, InvalidRoute> {
    match bits {
        0 => Ok(Access::Absent),
        1 => Ok(Access::ReadOnly),
        3 => Ok(Access::ReadWrite),
        _ => Err(InvalidRoute::ReservedCapability),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RoutingUnit {
    Selector(SelectorUnit),
    Mixer(MixerUnit),
}

/// Descriptor-backed inventory. Entity IDs are never matched across AC interfaces.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RoutingInventory {
    units: BTreeMap<Entity, RoutingUnit>,
}

impl RoutingInventory {
    pub fn insert(&mut self, unit: RoutingUnit) -> Result<(), InvalidRoute> {
        let entity = match &unit {
            RoutingUnit::Selector(s) => s.entity,
            RoutingUnit::Mixer(m) => m.entity,
        };
        if self.units.contains_key(&entity) {
            return Err(InvalidRoute::DuplicateEntity);
        }
        self.units.insert(entity, unit);
        Ok(())
    }

    pub fn units(&self) -> impl Iterator<Item = &RoutingUnit> {
        self.units.values()
    }

    pub fn control(&self, key: ControlKey) -> Result<RouteControl, InvalidRoute> {
        match key {
            ControlKey::Selector(entity) => match self.units.get(&entity) {
                Some(RoutingUnit::Selector(s)) => Ok(RouteControl {
                    key,
                    access: s.access,
                    mcn: 0,
                    input_number: 0,
                    protocol: s.protocol,
                    pins: Some(s.sources.len() as u8),
                }),
                _ => Err(InvalidRoute::UnknownControl),
            },
            ControlKey::Crosspoint {
                entity,
                input_pin,
                input_channel,
                output_channel,
            } => match self.units.get(&entity) {
                Some(RoutingUnit::Mixer(m)) => {
                    m.crosspoint(input_pin, input_channel, output_channel)
                }
                _ => Err(InvalidRoute::UnknownControl),
            },
        }
    }
}

/// USB setup fields, excluding the data stage. UAC2 CUR is 0x01 in both
/// directions; RANGE is 0x02 and is read only here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ControlRequest {
    pub request_type: u8,
    pub request: u8,
    pub value: u16,
    pub index: u16,
    pub length: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RouteValue {
    SelectorPin(u8),
    MixerGain(i16),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RouteChange {
    pub key: ControlKey,
    pub value: RouteValue,
}

/// Only constructors backed by a parsed unit can create this control.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RouteControl {
    key: ControlKey,
    access: Access,
    mcn: u8,
    input_number: u8,
    protocol: RoutingProtocol,
    pins: Option<u8>,
}

impl RouteControl {
    pub fn key(&self) -> ControlKey {
        self.key
    }
    pub fn access(&self) -> Access {
        self.access
    }

    fn setup(&self, input: bool, request: u8, length: u16) -> ControlRequest {
        let entity = match self.key {
            ControlKey::Selector(e) => e,
            ControlKey::Crosspoint { entity, .. } => entity,
        };
        ControlRequest {
            request_type: if input { 0xa1 } else { 0x21 },
            request: if input && self.protocol == RoutingProtocol::Uac1 {
                request | 0x80
            } else {
                request
            },
            value: if self.protocol == RoutingProtocol::Uac1 {
                match self.key {
                    ControlKey::Selector(_) => 0,
                    ControlKey::Crosspoint { output_channel, .. } => {
                        (u16::from(self.input_number) << 8) | u16::from(output_channel)
                    }
                }
            } else {
                0x0100 | u16::from(self.mcn)
            },
            index: (u16::from(entity.id) << 8) | u16::from(entity.interface),
            length,
        }
    }

    pub fn current_request(&self) -> Result<ControlRequest, InvalidRoute> {
        if self.access == Access::Absent {
            return Err(InvalidRoute::NotReadable);
        }
        Ok(self.setup(true, 1, if self.pins.is_some() { 1 } else { 2 }))
    }

    /// Validate selector pins and value type; mixer values additionally require
    /// a live RANGE check, performed by `apply_routes` before any SET_CUR.
    pub fn write_request(
        &self,
        value: RouteValue,
    ) -> Result<(ControlRequest, Vec<u8>), InvalidRoute> {
        if self.access != Access::ReadWrite {
            return Err(InvalidRoute::NotWritable);
        }
        self.validate(value)?;
        let bytes = match value {
            RouteValue::SelectorPin(p) => vec![p],
            RouteValue::MixerGain(g) => g.to_le_bytes().to_vec(),
        };
        Ok((self.setup(false, 1, bytes.len() as u16), bytes))
    }

    fn validate(&self, value: RouteValue) -> Result<(), InvalidRoute> {
        match (self.pins, value) {
            (Some(pins), RouteValue::SelectorPin(p)) if p > 0 && p <= pins => Ok(()),
            (Some(_), RouteValue::SelectorPin(_)) => Err(InvalidRoute::InvalidPin),
            (None, RouteValue::MixerGain(_)) => Ok(()),
            _ => Err(InvalidRoute::WrongValueType),
        }
    }

    pub fn read_current<I: RoutingIo>(
        &self,
        io: &mut I,
    ) -> Result<RouteValue, RoutingError<I::Error>> {
        let request = self.current_request().map_err(RoutingError::Invalid)?;
        let mut data = vec![0; usize::from(request.length)];
        read_exact(io, request, &mut data)?;
        let value = if self.pins.is_some() {
            RouteValue::SelectorPin(data[0])
        } else {
            RouteValue::MixerGain(i16::from_le_bytes([data[0], data[1]]))
        };
        self.validate(value).map_err(RoutingError::Invalid)?;
        Ok(value)
    }

    /// Fetch a bounded UAC2 Layout 2 RANGE using the count header first.
    pub fn read_gain_ranges<I: RoutingIo>(
        &self,
        io: &mut I,
    ) -> Result<Vec<GainRange>, RoutingError<I::Error>> {
        if self.pins.is_some() {
            return Err(RoutingError::Invalid(InvalidRoute::WrongValueType));
        }
        if self.protocol == RoutingProtocol::Uac1 {
            let mut values = [0i16; 3];
            for (slot, request) in values.iter_mut().zip([2, 3, 4]) {
                let mut bytes = [0; 2];
                read_exact(io, self.setup(true, request, 2), &mut bytes)?;
                *slot = i16::from_le_bytes(bytes);
            }
            let range = GainRange {
                minimum: values[0],
                maximum: values[1],
                resolution: values[2],
            };
            return if range.valid() {
                Ok(vec![range])
            } else {
                Err(RoutingError::Invalid(InvalidRoute::InvalidRange))
            };
        }
        let mut header = [0; 2];
        read_exact(io, self.setup(true, 2, 2), &mut header)?;
        let count = usize::from(u16::from_le_bytes(header));
        if count == 0 || count > MAX_GAIN_RANGES {
            return Err(RoutingError::Invalid(InvalidRoute::InvalidRange));
        }
        let length = 2 + count * 6;
        let mut data = vec![0; length];
        read_exact(io, self.setup(true, 2, length as u16), &mut data)?;
        decode_gain_ranges(&data).map_err(RoutingError::Invalid)
    }
}

pub const MAX_GAIN_RANGES: usize = 64;
pub const MAX_ROUTE_CHANGES: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GainRange {
    pub minimum: i16,
    pub maximum: i16,
    pub resolution: i16,
}

impl GainRange {
    fn valid(&self) -> bool {
        self.minimum != i16::MIN && self.minimum <= self.maximum && self.resolution > 0
    }
    pub fn contains(&self, gain: i16) -> bool {
        self.valid()
            && (gain == i16::MIN
                || (gain >= self.minimum
                    && gain <= self.maximum
                    && (i32::from(gain) - i32::from(self.minimum)) % i32::from(self.resolution)
                        == 0))
    }
}

pub fn decode_gain_ranges(bytes: &[u8]) -> Result<Vec<GainRange>, InvalidRoute> {
    if bytes.len() < 2 {
        return Err(InvalidRoute::InvalidRange);
    }
    let count = usize::from(u16::from_le_bytes([bytes[0], bytes[1]]));
    if count == 0 || count > MAX_GAIN_RANGES || bytes.len() != 2 + count * 6 {
        return Err(InvalidRoute::InvalidRange);
    }
    bytes[2..]
        .chunks_exact(6)
        .map(|b| {
            let range = GainRange {
                minimum: i16::from_le_bytes([b[0], b[1]]),
                maximum: i16::from_le_bytes([b[2], b[3]]),
                resolution: i16::from_le_bytes([b[4], b[5]]),
            };
            if range.valid() {
                Ok(range)
            } else {
                Err(InvalidRoute::InvalidRange)
            }
        })
        .collect()
}

/// An adapter must return actual transferred lengths, including on short I/O.
/// A failed write may have changed hardware; transactions restore that control too.
pub trait RoutingIo {
    type Error;
    fn read(&mut self, request: ControlRequest, data: &mut [u8]) -> Result<usize, Self::Error>;
    fn write(&mut self, request: ControlRequest, data: &[u8]) -> Result<usize, Self::Error>;
    /// Admit the full set of changed controls before the first SET.
    fn admit_plan(&mut self, _changed: usize) -> Result<(), Self::Error> {
        Ok(())
    }
    /// Reserve enough time for a complete SET and its verifying read.
    fn admit_write(&mut self) -> Result<(), Self::Error> {
        Ok(())
    }
    fn begin_recovery(&mut self) {}
    fn begin_recovery_for(&mut self, _attempted: usize) {
        self.begin_recovery();
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum RoutingError<E> {
    Invalid(InvalidRoute),
    Io(E),
    TransferLength {
        expected: usize,
        actual: usize,
    },
    ReadbackMismatch {
        expected: RouteValue,
        actual: RouteValue,
    },
}

#[derive(Debug)]
pub struct TransactionError<E> {
    pub failed_change: Option<usize>,
    pub cause: RoutingError<E>,
    /// Empty means every attempted write was restored and verified, or no write
    /// was attempted. Nonempty means hardware state is uncertain for these keys.
    pub rollback_failures: Vec<(ControlKey, RoutingError<E>)>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RouteReport {
    pub changed: Vec<ControlKey>,
    pub unchanged: Vec<ControlKey>,
}

fn read_exact<I: RoutingIo>(
    io: &mut I,
    request: ControlRequest,
    data: &mut [u8],
) -> Result<(), RoutingError<I::Error>> {
    let actual = io.read(request, data).map_err(RoutingError::Io)?;
    if actual != data.len() {
        return Err(RoutingError::TransferLength {
            expected: data.len(),
            actual,
        });
    }
    Ok(())
}

pub(crate) fn write_verified<I: RoutingIo>(
    io: &mut I,
    control: RouteControl,
    value: RouteValue,
) -> Result<(), RoutingError<I::Error>> {
    let (request, bytes) = control
        .write_request(value)
        .map_err(RoutingError::Invalid)?;
    let actual = io.write(request, &bytes).map_err(RoutingError::Io)?;
    if actual != bytes.len() {
        return Err(RoutingError::TransferLength {
            expected: bytes.len(),
            actual,
        });
    }
    let readback = control.read_current(io)?;
    if readback != value {
        return Err(RoutingError::ReadbackMismatch {
            expected: value,
            actual: readback,
        });
    }
    Ok(())
}

/// Preflight all keys, permissions, original values and mixer ranges before any
/// write. Apply changes in order, verify every write, and on failure restore all
/// attempted controls in reverse order with readback. This is best-effort USB
/// rollback, not device-side atomicity. Stop/reconfigure affected streams first.
pub fn apply_routes<I: RoutingIo>(
    inventory: &RoutingInventory,
    io: &mut I,
    changes: &[RouteChange],
) -> Result<RouteReport, TransactionError<I::Error>> {
    apply_routes_traced(inventory, io, changes, &mut Vec::new())
}

/// Actual CUR and attempted target for each SET, including ambiguous failures.
pub(crate) fn apply_routes_traced<I: RoutingIo>(
    inventory: &RoutingInventory,
    io: &mut I,
    changes: &[RouteChange],
    attempted_values: &mut Vec<(ControlKey, RouteValue, RouteValue)>,
) -> Result<RouteReport, TransactionError<I::Error>> {
    let error = |index, cause| TransactionError {
        failed_change: index,
        cause,
        rollback_failures: Vec::new(),
    };
    if changes.len() > MAX_ROUTE_CHANGES {
        return Err(error(
            None,
            RoutingError::Invalid(InvalidRoute::TooManyChanges),
        ));
    }
    let mut controls = Vec::with_capacity(changes.len());
    // Validate the entire request before touching USB, even for read-only I/O.
    for (i, change) in changes.iter().enumerate() {
        if changes[..i].iter().any(|prior| prior.key == change.key) {
            return Err(error(
                Some(i),
                RoutingError::Invalid(InvalidRoute::DuplicateChange),
            ));
        }
        let control = inventory
            .control(change.key)
            .map_err(|e| error(Some(i), RoutingError::Invalid(e)))?;
        control
            .write_request(change.value)
            .map_err(|e| error(Some(i), RoutingError::Invalid(e)))?;
        controls.push(control);
    }
    let mut originals = Vec::with_capacity(changes.len());
    for (i, (control, change)) in controls.iter().zip(changes).enumerate() {
        let original = control.read_current(io).map_err(|e| error(Some(i), e))?;
        if let RouteValue::MixerGain(gain) = change.value {
            let ranges = control
                .read_gain_ranges(io)
                .map_err(|e| error(Some(i), e))?;
            if !ranges.iter().any(|range| range.contains(gain)) {
                return Err(error(
                    Some(i),
                    RoutingError::Invalid(InvalidRoute::GainOutsideRange),
                ));
            }
        }
        originals.push(original);
    }
    let changed = changes
        .iter()
        .zip(&originals)
        .filter(|(change, original)| change.value != **original)
        .count();
    io.admit_plan(changed)
        .map_err(|cause| error(None, RoutingError::Io(cause)))?;
    let mut report = RouteReport::default();
    let mut attempted: Vec<usize> = Vec::new();
    for (i, ((control, change), original)) in
        controls.iter().zip(changes).zip(&originals).enumerate()
    {
        if change.value == *original {
            report.unchanged.push(change.key);
            continue;
        }
        if let Err(cause) = io.admit_write() {
            let mut failure = error(Some(i), RoutingError::Io(cause));
            if !attempted.is_empty() {
                io.begin_recovery_for(attempted.len());
                for &j in attempted.iter().rev() {
                    if let Err(e) = write_verified(io, controls[j], originals[j]) {
                        failure.rollback_failures.push((changes[j].key, e));
                    }
                }
            }
            return Err(failure);
        }
        attempted.push(i);
        attempted_values.push((change.key, *original, change.value));
        if let Err(cause) = write_verified(io, *control, change.value) {
            let mut failure = error(Some(i), cause);
            io.begin_recovery_for(attempted.len());
            for &j in attempted.iter().rev() {
                if let Err(e) = write_verified(io, controls[j], originals[j]) {
                    failure.rollback_failures.push((changes[j].key, e));
                }
            }
            return Err(failure);
        }
        report.changed.push(change.key);
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uac1_selector_and_crosspoint_use_legacy_setup_and_separate_ranges() {
        let selector = SelectorUnit::parse_uac1(3, &[8, 0x24, 5, 7, 2, 1, 2, 0]).unwrap();
        let mut inventory = RoutingInventory::default();
        inventory.insert(RoutingUnit::Selector(selector)).unwrap();
        let control = inventory
            .control(ControlKey::Selector(Entity {
                interface: 3,
                id: 7,
            }))
            .unwrap();
        let current = control.current_request().unwrap();
        assert_eq!(
            (current.request, current.value, current.length),
            (0x81, 0, 1)
        );
        assert_eq!(
            control
                .write_request(RouteValue::SelectorPin(2))
                .unwrap()
                .0
                .value,
            0
        );
        let inputs = [
            InputCluster {
                source_id: 1,
                channels: 1,
            },
            InputCluster {
                source_id: 2,
                channels: 1,
            },
        ];
        let mixer =
            MixerUnit::parse_uac1(3, &[13, 0x24, 4, 9, 2, 1, 2, 2, 3, 0, 0, 0xf0, 0], &inputs)
                .unwrap();
        let point = mixer.crosspoint(2, 1, 2).unwrap();
        assert_eq!(point.current_request().unwrap().value, 0x0202);
        struct LegacyIo {
            requests: Vec<u8>,
        }
        impl RoutingIo for LegacyIo {
            type Error = ();
            fn read(&mut self, request: ControlRequest, bytes: &mut [u8]) -> Result<usize, ()> {
                self.requests.push(request.request);
                assert_eq!(request.value, 0x0202);
                let value: i16 = match request.request {
                    0x82 => -1024,
                    0x83 => 0,
                    0x84 => 256,
                    _ => panic!("unexpected legacy request"),
                };
                bytes.copy_from_slice(&value.to_le_bytes());
                Ok(bytes.len())
            }
            fn write(&mut self, _: ControlRequest, _: &[u8]) -> Result<usize, ()> {
                panic!("range query wrote hardware")
            }
        }
        let mut io = LegacyIo { requests: vec![] };
        let ranges = point.read_gain_ranges(&mut io).unwrap();
        assert_eq!(io.requests, [0x82, 0x83, 0x84]);
        assert!(ranges[0].contains(-512));
        assert!(!ranges[0].contains(-500));
    }

    #[test]
    fn adc3_routes_use_full_access_words_mcn_and_layout_widths() {
        let mut inventory = RoutingInventory::default();
        inventory
            .insert(RoutingUnit::Selector(
                SelectorUnit::parse_adc3(3, &[13, 0x24, 6, 7, 2, 1, 2, 3, 0, 0, 0, 0, 0]).unwrap(),
            ))
            .unwrap();
        let control = inventory
            .control(ControlKey::Selector(Entity {
                interface: 3,
                id: 7,
            }))
            .unwrap();
        assert_eq!(
            (
                control.current_request().unwrap().value,
                control.current_request().unwrap().length
            ),
            (0x0100, 1)
        );
        assert!(SelectorUnit::parse_adc3(3, &[12, 0x24, 6, 7, 1, 1, 3, 0, 0, 1, 0, 0]).is_err());
        let inputs = [InputCluster {
            source_id: 1,
            channels: 2,
        }];
        let mut bytes = vec![15, 0x24, 5, 9, 1, 1, 42, 0, 0x90, 0, 0, 0, 0, 0, 0];
        let mixer = MixerUnit::parse_adc3(3, &bytes, &inputs, 2).unwrap();
        let point = mixer.crosspoint(1, 2, 2).unwrap();
        assert_eq!(
            (
                point.current_request().unwrap().value,
                point.current_request().unwrap().length
            ),
            (0x0103, 2)
        );
        assert_eq!(
            point.write_request(RouteValue::MixerGain(-512)).unwrap().1,
            (-512i16).to_le_bytes()
        );
        assert_eq!(
            mixer.crosspoint(1, 1, 2).unwrap().access(),
            Access::ReadOnly
        );
        bytes[12] = 1; // reserved bit in the high byte of bmControls
        assert!(MixerUnit::parse_adc3(3, &bytes, &inputs, 2).is_err());
        assert!(MixerUnit::parse_adc3(3, &bytes[..7], &inputs, 2).is_err());
    }

    fn selector(interface: u8, id: u8, flags: u8) -> SelectorUnit {
        SelectorUnit::parse(interface, &[9, 0x24, 5, id, 2, 1, 2, flags, 0]).unwrap()
    }

    fn mixer(
        inputs: &[InputCluster],
        outputs: u8,
        bitmap: &[u8],
    ) -> Result<MixerUnit, InvalidRoute> {
        let mut bytes = vec![
            (13 + inputs.len() + bitmap.len()) as u8,
            0x24,
            4,
            9,
            inputs.len() as u8,
        ];
        bytes.extend(inputs.iter().map(|c| c.source_id));
        bytes.push(outputs);
        bytes.extend([0, 0, 0, 0, 0]); // channel config and names
        bytes.extend(bitmap);
        bytes.extend([0, 0]); // bmControls and iMixer
        MixerUnit::parse(3, &bytes, inputs)
    }

    fn inventory() -> RoutingInventory {
        let mut result = RoutingInventory::default();
        for id in [7, 8] {
            result
                .insert(RoutingUnit::Selector(selector(3, id, 3)))
                .unwrap();
        }
        result
            .insert(RoutingUnit::Mixer(
                mixer(
                    &[InputCluster {
                        source_id: 1,
                        channels: 2,
                    }],
                    2,
                    &[0x90],
                )
                .unwrap(),
            ))
            .unwrap();
        result
    }

    fn select(id: u8, pin: u8) -> RouteChange {
        RouteChange {
            key: ControlKey::Selector(Entity { interface: 3, id }),
            value: RouteValue::SelectorPin(pin),
        }
    }

    #[derive(Default)]
    struct MockIo {
        current: BTreeMap<(u16, u16), Vec<u8>>,
        reads: Vec<ControlRequest>,
        writes: Vec<(ControlRequest, Vec<u8>)>,
        range: Vec<u8>,
        fail_writes: Vec<usize>,
        ignore_writes: Vec<usize>,
        short_reads: bool,
    }

    impl MockIo {
        fn selectors() -> Self {
            Self {
                current: BTreeMap::from([((0x0703, 0x0100), vec![1]), ((0x0803, 0x0100), vec![1])]),
                ..Self::default()
            }
        }
    }

    impl RoutingIo for MockIo {
        type Error = &'static str;
        fn read(&mut self, request: ControlRequest, data: &mut [u8]) -> Result<usize, Self::Error> {
            self.reads.push(request);
            assert_eq!(request.request_type, 0xa1);
            assert_eq!(usize::from(request.length), data.len());
            if self.short_reads {
                return Ok(0);
            }
            let value = if request.request == 2 {
                &self.range
            } else {
                self.current
                    .get(&(request.index, request.value))
                    .ok_or("missing")?
            };
            let len = data.len().min(value.len());
            data[..len].copy_from_slice(&value[..len]);
            Ok(len)
        }
        fn write(&mut self, request: ControlRequest, data: &[u8]) -> Result<usize, Self::Error> {
            assert_eq!(request.request_type, 0x21);
            assert_eq!(request.request, 1);
            self.writes.push((request, data.to_vec()));
            let ordinal = self.writes.len();
            if !self.ignore_writes.contains(&ordinal) {
                self.current
                    .insert((request.index, request.value), data.to_vec());
            }
            // Deliberately fail after applying the hardware write.
            if self.fail_writes.contains(&ordinal) {
                return Err("write failed");
            }
            Ok(data.len())
        }
    }

    #[test]
    fn selector_uses_one_based_pin_and_ac_interface_in_request() {
        let control = inventory().control(select(7, 2).key).unwrap();
        assert_eq!(
            control.current_request().unwrap(),
            ControlRequest {
                request_type: 0xa1,
                request: 1,
                value: 0x0100,
                index: 0x0703,
                length: 1
            }
        );
        let (request, bytes) = control.write_request(RouteValue::SelectorPin(2)).unwrap();
        assert_eq!(request.request_type, 0x21);
        assert_eq!(bytes, vec![2]);
        for pin in [0, 3, 255] {
            assert_eq!(
                control.write_request(RouteValue::SelectorPin(pin)),
                Err(InvalidRoute::InvalidPin)
            );
        }
        assert_eq!(
            control.write_request(RouteValue::MixerGain(0)),
            Err(InvalidRoute::WrongValueType)
        );
        assert_eq!(
            inventory().control(ControlKey::Selector(Entity {
                interface: 4,
                id: 7
            })),
            Err(InvalidRoute::UnknownControl)
        );
    }

    #[test]
    fn selector_permissions_and_descriptor_bounds() {
        for flags in [2, 4, 255] {
            assert_eq!(
                SelectorUnit::parse(0, &[9, 0x24, 5, 7, 2, 1, 2, flags, 0]),
                Err(InvalidRoute::ReservedCapability)
            );
        }
        for bytes in [
            &[5, 0x24, 5, 7, 255][..],
            &[8, 0x24, 5, 7, 1, 0, 3, 0][..],
            &[8, 0x24, 5, 0, 1, 1, 3, 0][..],
            &[7, 0x24, 5, 7, 0, 3, 0][..],
        ] {
            assert_eq!(
                SelectorUnit::parse(0, bytes),
                Err(InvalidRoute::MalformedDescriptor)
            );
        }
        for (flags, expected) in [
            (0, Access::Absent),
            (1, Access::ReadOnly),
            (3, Access::ReadWrite),
        ] {
            let mut inv = RoutingInventory::default();
            inv.insert(RoutingUnit::Selector(selector(3, 7, flags)))
                .unwrap();
            let control = inv.control(select(7, 1).key).unwrap();
            assert_eq!(control.access(), expected);
            if flags != 3 {
                assert_eq!(
                    control.write_request(RouteValue::SelectorPin(1)),
                    Err(InvalidRoute::NotWritable)
                );
            }
            if flags == 0 {
                assert_eq!(control.current_request(), Err(InvalidRoute::NotReadable));
            }
        }
    }

    #[test]
    fn mixer_uses_msb_bitmap_and_flattened_zero_based_mcn() {
        let inputs = [
            InputCluster {
                source_id: 1,
                channels: 2,
            },
            InputCluster {
                source_id: 2,
                channels: 1,
            },
        ];
        let unit = mixer(&inputs, 3, &[0x80, 0x80]).unwrap();
        let first = unit.crosspoint(1, 1, 1).unwrap();
        assert_eq!(first.access(), Access::ReadWrite);
        assert_eq!(first.current_request().unwrap().value, 0x0100);
        let last = unit.crosspoint(2, 1, 3).unwrap();
        assert_eq!(last.access(), Access::ReadWrite);
        assert_eq!(last.current_request().unwrap().value, 0x0108);
        assert_eq!(last.current_request().unwrap().index, 0x0903);
        let fixed = unit.crosspoint(1, 1, 2).unwrap();
        assert_eq!(fixed.access(), Access::ReadOnly);
        assert!(fixed.current_request().is_ok());
        assert_eq!(
            fixed.write_request(RouteValue::MixerGain(0)),
            Err(InvalidRoute::NotWritable)
        );
        assert_eq!(
            last.write_request(RouteValue::MixerGain(-256)).unwrap().1,
            vec![0, 255]
        );
        assert_eq!(
            last.write_request(RouteValue::MixerGain(i16::MIN))
                .unwrap()
                .1,
            vec![0, 128]
        );
        for (pin, channel, output) in [
            (0, 1, 1),
            (3, 1, 1),
            (2, 2, 1),
            (1, 0, 1),
            (1, 1, 0),
            (1, 1, 4),
        ] {
            assert_eq!(
                unit.crosspoint(pin, channel, output),
                Err(InvalidRoute::UnknownControl)
            );
        }
    }

    #[test]
    fn mixer_refuses_unproven_geometry_padding_and_overflow() {
        let inputs = [InputCluster {
            source_id: 1,
            channels: 2,
        }];
        assert_eq!(
            mixer(&inputs, 2, &[]),
            Err(InvalidRoute::MalformedDescriptor)
        );
        assert_eq!(
            mixer(&inputs, 2, &[0xff]),
            Err(InvalidRoute::MalformedDescriptor)
        );
        assert_eq!(
            mixer(
                &[InputCluster {
                    source_id: 1,
                    channels: 0
                }],
                2,
                &[0]
            ),
            Err(InvalidRoute::UnknownInputChannels)
        );
        assert_eq!(
            mixer(
                &[InputCluster {
                    source_id: 1,
                    channels: 129
                }],
                2,
                &[0; 33]
            ),
            Err(InvalidRoute::TooManyCrosspoints)
        );
        let max = mixer(
            &[InputCluster {
                source_id: 1,
                channels: 16,
            }],
            16,
            &[255; 32],
        )
        .unwrap();
        assert_eq!(
            max.crosspoint(1, 16, 16)
                .unwrap()
                .current_request()
                .unwrap()
                .value,
            0x01ff
        );
        let bytes = [15, 0x24, 4, 9, 1, 1, 2, 0, 0, 0, 0, 0, 0xf0, 0, 0];
        assert_eq!(
            MixerUnit::parse(
                3,
                &bytes,
                &[InputCluster {
                    source_id: 2,
                    channels: 2
                }]
            ),
            Err(InvalidRoute::UnknownInputChannels)
        );
        let mut reserved = bytes;
        reserved[13] = 2;
        assert_eq!(
            MixerUnit::parse(3, &reserved, &inputs),
            Err(InvalidRoute::ReservedCapability)
        );
    }

    #[test]
    fn gain_ranges_are_signed_bounded_and_validate_steps() {
        let ranges = decode_gain_ranges(&[1, 0, 0, 254, 0, 0, 0, 1]).unwrap();
        assert!(ranges[0].contains(-256));
        assert!(ranges[0].contains(i16::MIN));
        assert!(!ranges[0].contains(-255));
        assert!(!ranges[0].contains(256));
        for bytes in [
            &[0, 0][..],
            &[65, 0][..],
            &[1, 0, 0, 128, 0, 0, 1, 0][..],
            &[1, 0, 0, 0, 0, 0, 0, 0][..],
            &[1, 0, 0, 0, 0, 255, 1, 0][..],
            &[1, 0][..],
        ] {
            assert_eq!(decode_gain_ranges(bytes), Err(InvalidRoute::InvalidRange));
        }
    }

    #[test]
    fn grouped_changes_verify_readback_and_leave_current_route_untouched() {
        let mut io = MockIo::selectors();
        let report = apply_routes(&inventory(), &mut io, &[select(7, 1), select(8, 2)]).unwrap();
        assert_eq!(report.unchanged, vec![select(7, 1).key]);
        assert_eq!(report.changed, vec![select(8, 2).key]);
        assert_eq!(io.writes.len(), 1);
        assert_eq!(io.reads.len(), 3); // two originals and verified write
    }

    #[test]
    fn rejected_plan_does_not_write_any_control() {
        struct PlanProbe(MockIo);
        impl RoutingIo for PlanProbe {
            type Error = &'static str;
            fn read(
                &mut self,
                request: ControlRequest,
                data: &mut [u8],
            ) -> Result<usize, Self::Error> {
                self.0.read(request, data)
            }
            fn write(
                &mut self,
                request: ControlRequest,
                data: &[u8],
            ) -> Result<usize, Self::Error> {
                self.0.write(request, data)
            }
            fn admit_plan(&mut self, changed: usize) -> Result<(), Self::Error> {
                assert_eq!(changed, 2);
                Err("plan deadline")
            }
        }
        let mut io = PlanProbe(MockIo::selectors());
        let result =
            apply_routes(&inventory(), &mut io, &[select(7, 2), select(8, 2)]).unwrap_err();
        assert_eq!(result.cause, RoutingError::Io("plan deadline"));
        assert!(io.0.writes.is_empty());
    }

    #[test]
    fn admission_failure_restores_previous_writes_and_preserves_cause() {
        struct AdmissionProbe {
            inner: MockIo,
            admitted: usize,
            recovered: usize,
        }
        impl RoutingIo for AdmissionProbe {
            type Error = &'static str;
            fn read(
                &mut self,
                request: ControlRequest,
                data: &mut [u8],
            ) -> Result<usize, Self::Error> {
                self.inner.read(request, data)
            }
            fn write(
                &mut self,
                request: ControlRequest,
                data: &[u8],
            ) -> Result<usize, Self::Error> {
                self.inner.write(request, data)
            }
            fn admit_write(&mut self) -> Result<(), Self::Error> {
                self.admitted += 1;
                if self.admitted == 2 {
                    Err("deadline")
                } else {
                    Ok(())
                }
            }
            fn begin_recovery_for(&mut self, attempted: usize) {
                self.recovered = attempted;
            }
        }
        let mut io = AdmissionProbe {
            inner: MockIo::selectors(),
            admitted: 0,
            recovered: 0,
        };
        let failure =
            apply_routes(&inventory(), &mut io, &[select(7, 2), select(8, 2)]).unwrap_err();
        assert_eq!(failure.cause, RoutingError::Io("deadline"));
        assert!(failure.rollback_failures.is_empty());
        assert_eq!(io.recovered, 1);
        assert_eq!(
            io.inner
                .writes
                .iter()
                .map(|(r, b)| (r.index, b[0]))
                .collect::<Vec<_>>(),
            vec![(0x0703, 2), (0x0703, 1)]
        );
    }

    #[test]
    fn stalled_set_and_expired_rollback_report_every_attempt_without_more_writes() {
        struct DeadlineIo {
            inner: MockIo,
            recovering: bool,
        }
        impl RoutingIo for DeadlineIo {
            type Error = &'static str;
            fn read(
                &mut self,
                request: ControlRequest,
                data: &mut [u8],
            ) -> Result<usize, Self::Error> {
                if self.recovering {
                    return Err("deadline");
                }
                self.inner.read(request, data)
            }
            fn write(
                &mut self,
                request: ControlRequest,
                data: &[u8],
            ) -> Result<usize, Self::Error> {
                if self.recovering {
                    return Err("deadline");
                }
                self.inner.write(request, data)
            }
            fn begin_recovery_for(&mut self, _: usize) {
                self.recovering = true;
            }
        }
        let mut inner = MockIo::selectors();
        inner.fail_writes = vec![2];
        let mut io = DeadlineIo {
            inner,
            recovering: false,
        };
        let changes = [select(7, 2), select(8, 2)];
        let failure = apply_routes(&inventory(), &mut io, &changes).unwrap_err();
        assert_eq!(io.inner.writes.len(), 2);
        assert_eq!(
            failure
                .rollback_failures
                .iter()
                .map(|(key, _)| *key)
                .collect::<Vec<_>>(),
            vec![changes[1].key, changes[0].key]
        );
        assert!(failure
            .rollback_failures
            .iter()
            .all(|(_, error)| *error == RoutingError::Io("deadline")));
    }

    #[test]
    fn failed_write_is_included_in_reverse_verified_rollback() {
        let mut io = MockIo::selectors();
        io.fail_writes = vec![2];
        let failure =
            apply_routes(&inventory(), &mut io, &[select(7, 2), select(8, 2)]).unwrap_err();
        assert_eq!(failure.failed_change, Some(1));
        assert_eq!(failure.cause, RoutingError::Io("write failed"));
        assert!(failure.rollback_failures.is_empty());
        assert_eq!(
            io.writes
                .iter()
                .map(|(r, b)| (r.index, b[0]))
                .collect::<Vec<_>>(),
            vec![(0x0703, 2), (0x0803, 2), (0x0803, 1), (0x0703, 1)]
        );
        assert_eq!(io.current[&(0x0703, 0x0100)], vec![1]);
        assert_eq!(io.current[&(0x0803, 0x0100)], vec![1]);
    }

    #[test]
    fn trace_retains_actual_prewrite_values_for_failed_and_uncertain_sets() {
        let mut io = MockIo::selectors();
        io.fail_writes = vec![2, 3];
        let edits = [select(7, 2), select(8, 2)];
        let mut trace = Vec::new();
        let error = apply_routes_traced(&inventory(), &mut io, &edits, &mut trace).unwrap_err();
        assert_eq!(
            trace,
            vec![
                (
                    edits[0].key,
                    RouteValue::SelectorPin(1),
                    RouteValue::SelectorPin(2)
                ),
                (
                    edits[1].key,
                    RouteValue::SelectorPin(1),
                    RouteValue::SelectorPin(2)
                ),
            ]
        );
        assert_eq!(error.rollback_failures[0].0, edits[1].key);
    }

    #[test]
    fn mismatched_readback_restores_attempted_control() {
        let mut io = MockIo::selectors();
        io.ignore_writes = vec![1];
        let failure = apply_routes(&inventory(), &mut io, &[select(7, 2)]).unwrap_err();
        assert_eq!(
            failure.cause,
            RoutingError::ReadbackMismatch {
                expected: RouteValue::SelectorPin(2),
                actual: RouteValue::SelectorPin(1)
            }
        );
        assert!(failure.rollback_failures.is_empty());
        assert_eq!(io.writes.len(), 2);
    }

    #[test]
    fn rollback_failures_are_reported_and_remaining_restores_continue() {
        let mut io = MockIo::selectors();
        io.fail_writes = vec![2, 3];
        let failure =
            apply_routes(&inventory(), &mut io, &[select(7, 2), select(8, 2)]).unwrap_err();
        assert_eq!(
            failure.rollback_failures,
            vec![(select(8, 2).key, RoutingError::Io("write failed"))]
        );
        assert_eq!(io.writes.len(), 4);
    }

    #[test]
    fn invalid_batches_and_short_original_reads_never_write() {
        let mut io = MockIo::selectors();
        let inv = inventory();
        for changes in [
            vec![select(7, 2), select(8, 0)],
            vec![select(7, 2), select(7, 1)],
            vec![select(7, 2); MAX_ROUTE_CHANGES + 1],
        ] {
            assert!(apply_routes(&inv, &mut io, &changes).is_err());
            assert!(io.reads.is_empty());
            assert!(io.writes.is_empty());
        }
        io.short_reads = true;
        let failure = apply_routes(&inv, &mut io, &[select(7, 2)]).unwrap_err();
        assert_eq!(
            failure.cause,
            RoutingError::TransferLength {
                expected: 1,
                actual: 0
            }
        );
        assert!(io.writes.is_empty());
    }

    #[test]
    fn raw_restore_replays_off_grid_cur_and_detects_mismatch() {
        let inv = inventory();
        let key = ControlKey::Crosspoint {
            entity: Entity {
                interface: 3,
                id: 9,
            },
            input_pin: 1,
            input_channel: 1,
            output_channel: 1,
        };
        let mut io = MockIo {
            current: BTreeMap::from([((0x0903, 0x0100), (-256i16).to_le_bytes().to_vec())]),
            range: vec![1, 0, 0, 254, 0, 0, 0, 1],
            ..MockIo::default()
        };
        let control = inv.control(key).unwrap();
        write_verified(&mut io, control, RouteValue::MixerGain(-255)).unwrap();
        assert_eq!(io.current[&(0x0903, 0x0100)], (-255i16).to_le_bytes());
        io.ignore_writes = vec![2];
        assert!(matches!(
            write_verified(&mut io, control, RouteValue::MixerGain(-256)),
            Err(RoutingError::ReadbackMismatch { .. })
        ));
    }

    #[test]
    fn mixer_transaction_checks_live_range_before_writing() {
        let inv = inventory();
        let key = ControlKey::Crosspoint {
            entity: Entity {
                interface: 3,
                id: 9,
            },
            input_pin: 1,
            input_channel: 1,
            output_channel: 1,
        };
        let mut io = MockIo {
            current: BTreeMap::from([((0x0903, 0x0100), vec![0, 0])]),
            range: vec![1, 0, 0, 254, 0, 0, 0, 1],
            ..MockIo::default()
        };
        let failure = apply_routes(
            &inv,
            &mut io,
            &[RouteChange {
                key,
                value: RouteValue::MixerGain(-255),
            }],
        )
        .unwrap_err();
        assert_eq!(
            failure.cause,
            RoutingError::Invalid(InvalidRoute::GainOutsideRange)
        );
        assert!(io.writes.is_empty());
        apply_routes(
            &inv,
            &mut io,
            &[RouteChange {
                key,
                value: RouteValue::MixerGain(-256),
            }],
        )
        .unwrap();
        assert_eq!(io.writes[0].1, vec![0, 255]);
        assert!(io.reads.iter().any(|r| r.request == 2 && r.length == 2));
        assert!(io.reads.iter().any(|r| r.request == 2 && r.length == 8));
    }
}
