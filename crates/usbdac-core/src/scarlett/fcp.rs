//! FCP topology and transactional mixer/router edits using firmware identities.
//! Labels are firmware identities, not a claim about physical connector wiring.
use super::*;
use base64::{engine::general_purpose::STANDARD, Engine};
use flate2::{Decompress, FlushDecompress, Status};
use serde::Deserialize;
use serde_json::Value;
use std::collections::BTreeSet;

const MAX_JSON: usize = 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FcpPort {
    pub pin: u16,
    pub name: String,
    pub fixed_source: Option<u16>,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FcpBank {
    pub bank: u8,
    pub raw_words: Vec<u32>,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FcpReadback {
    pub model: CandidateModel,
    pub firmware: u32,
    pub mixer_inputs: u8,
    pub mixer_outputs: u8,
    pub sources: Vec<FcpPort>,
    pub destinations: Vec<FcpPort>,
    pub banks: Vec<FcpBank>,
    pub mixes: Vec<MixReadback>,
}

/// Read only the bounded FCP mixer, routing banks and firmware device map.
/// The adapter must own the claimed exact device and notification endpoint.
pub fn read_fcp<I: ReadIo>(
    mut io: I,
    candidate: CandidateModel,
) -> Result<FcpReadback, ReadError<I::Error>> {
    read_state(&mut io, candidate).map(|(state, _)| state)
}

fn read_state<I: ReadIo>(
    io: &mut I,
    candidate: CandidateModel,
) -> Result<(FcpReadback, u16), ReadError<I::Error>> {
    if !candidate.is_fcp() {
        return Err(ReadError::Protocol(ProtocolError::UnsupportedProtocol));
    }
    let mut startup = [0; 24];
    exact(
        io.initial_read(&mut startup).map_err(ReadError::Io)?,
        startup.len(),
    )?;
    io.prepare_notifications().map_err(ReadError::Io)?;
    transact_mode(io, Command::Init, 0, &[], 0, true)?;
    let info = transact_mode(io, Command::Info, 1, &[], 84, true)?;
    let firmware = u32::from_le_bytes(info[8..12].try_into().unwrap());
    if firmware == 0 {
        return Err(ReadError::Protocol(ProtocolError::UnsupportedFirmware(0)));
    }
    let mut sequence = 2u16;
    let mut read = |command, payload: &[u8], size| {
        let result = transact_mode(io, command, sequence, payload, size, true);
        sequence = sequence.wrapping_add(1);
        result
    };
    let mix_info = read(Command::MixInfo, &[], 8)?;
    let mixer_outputs = mix_info[0];
    let mixer_inputs = mix_info[1];
    if (mixer_inputs == 0) != (mixer_outputs == 0) {
        return Err(ReadError::Protocol(ProtocolError::UnsupportedMuxGeometry));
    }
    let mux_info = read(Command::MuxInfo, &[], 12)?;
    let widths: Vec<usize> = mux_info[..6]
        .chunks_exact(2)
        .map(|v| usize::from(u16::from_le_bytes([v[0], v[1]])))
        .collect();
    if widths[0] == 0 || widths.iter().any(|&n| n > usize::from(u8::MAX)) {
        return Err(ReadError::Protocol(ProtocolError::UnsupportedMuxGeometry));
    }
    let map_info = read(Command::DevmapInfo, &[], 4)?;
    let map_size = usize::from(u16::from_le_bytes([map_info[2], map_info[3]]));
    if map_size == 0 {
        return Err(ReadError::Protocol(ProtocolError::InvalidDeviceMap));
    }
    let mut encoded = Vec::with_capacity(map_size);
    for offset in (0..map_size).step_by(FCP_MAX_PAYLOAD) {
        encoded.extend(read(
            Command::DevmapRead,
            &((offset / FCP_MAX_PAYLOAD) as u32).to_le_bytes(),
            (map_size - offset).min(FCP_MAX_PAYLOAD),
        )?);
    }
    let (sources, destinations) = decode_map(&encoded).map_err(ReadError::Protocol)?;
    let mut banks = Vec::new();
    for (bank, &width) in widths.iter().enumerate() {
        if width == 0 {
            continue;
        }
        let bytes = read(Command::Mux, &[0, 0, width as u8, bank as u8], width * 4)?;
        let raw_words = bytes
            .chunks_exact(4)
            .map(|b| u32::from_le_bytes(b.try_into().unwrap()))
            .collect();
        banks.push(FcpBank {
            bank: bank as u8,
            raw_words,
        });
    }
    validate_banks(&sources, &destinations, &banks).map_err(ReadError::Protocol)?;
    let mut mixes = Vec::with_capacity(usize::from(mixer_outputs));
    for output in 0..mixer_outputs {
        let bytes = read(
            Command::Mix,
            &[output, 0, mixer_inputs, 0],
            usize::from(mixer_inputs) * 2,
        )?;
        mixes.push(MixReadback {
            output: output + 1,
            coefficients: bytes
                .chunks_exact(2)
                .map(|b| u16::from_le_bytes([b[0], b[1]]))
                .collect(),
        });
    }
    Ok((
        FcpReadback {
            model: candidate,
            firmware,
            mixer_inputs,
            mixer_outputs,
            sources,
            destinations,
            banks,
            mixes,
        },
        sequence,
    ))
}

/// Router pins are firmware identities; source zero selects Off.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FcpRouteEdit {
    pub bank: u8,
    pub destination: u16,
    pub source: u16,
}
/// Mixer positions are one-based; editable coefficients are raw 0..=32613.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FcpMixEdit {
    pub output: u8,
    pub input: u8,
    pub coefficient: u16,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FcpTarget {
    Bank(u8),
    Mix(u8),
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FcpWriteReport {
    pub original: FcpReadback,
    pub verified: FcpReadback,
}
#[derive(Debug, PartialEq, Eq)]
pub enum FcpFailure<E> {
    Read(ReadError<E>),
    Invalid(ProtocolError),
    Drift,
    Verification(FcpTarget),
}
#[derive(Debug, PartialEq, Eq)]
pub struct FcpWriteError<E> {
    pub failure: FcpFailure<E>,
    pub rollback: Vec<(FcpTarget, FcpFailure<E>)>,
}

fn write_error<E>(failure: FcpFailure<E>) -> FcpWriteError<E> {
    FcpWriteError {
        failure,
        rollback: Vec::new(),
    }
}
pub(crate) fn same_fcp_layout(a: &FcpReadback, b: &FcpReadback) -> bool {
    a.model == b.model
        && a.firmware == b.firmware
        && a.mixer_inputs == b.mixer_inputs
        && a.mixer_outputs == b.mixer_outputs
        && a.sources == b.sources
        && a.destinations == b.destinations
        && a.banks.len() == b.banks.len()
        && a.banks.iter().zip(&b.banks).all(|(a, b)| {
            a.bank == b.bank
                && a.raw_words.len() == b.raw_words.len()
                && a.raw_words
                    .iter()
                    .zip(&b.raw_words)
                    .all(|(a, b)| (a & 0xfff) == (b & 0xfff))
        })
        && a.mixes.len() == b.mixes.len()
        && a.mixes
            .iter()
            .zip(&b.mixes)
            .all(|(a, b)| a.output == b.output && a.coefficients.len() == b.coefficients.len())
}
fn targets(state: &FcpReadback) -> Vec<FcpTarget> {
    state
        .banks
        .iter()
        .map(|b| FcpTarget::Bank(b.bank))
        .chain(state.mixes.iter().map(|m| FcpTarget::Mix(m.output)))
        .collect()
}
fn values(state: &FcpReadback, target: FcpTarget) -> Vec<u8> {
    match target {
        FcpTarget::Bank(n) => state
            .banks
            .iter()
            .find(|b| b.bank == n)
            .unwrap()
            .raw_words
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .collect(),
        FcpTarget::Mix(n) => state
            .mixes
            .iter()
            .find(|m| m.output == n)
            .unwrap()
            .coefficients
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .collect(),
    }
}
fn target_read<I: ReadIo>(
    io: &mut I,
    sequence: &mut u16,
    state: &FcpReadback,
    target: FcpTarget,
) -> Result<Vec<u8>, ReadError<I::Error>> {
    let size = values(state, target).len();
    let (command, request) = match target {
        FcpTarget::Bank(n) => (Command::Mux, [0, 0, (size / 4) as u8, n]),
        FcpTarget::Mix(n) => (Command::Mix, [n - 1, 0, (size / 2) as u8, 0]),
    };
    let result = transact_mode(io, command, *sequence, &request, size, true);
    *sequence = sequence.wrapping_add(1);
    result
}
// Literal upstream fcp.c ABI: uint16 row, or uint16 pad + uint16 bank.
fn target_packet(target: FcpTarget, data: &[u8]) -> (Command, Vec<u8>) {
    let (command, mut payload) = match target {
        FcpTarget::Bank(n) => (Command::SetMux, vec![0, 0, n, 0]),
        FcpTarget::Mix(n) => (Command::SetMix, u16::from(n - 1).to_le_bytes().to_vec()),
    };
    payload.extend(data);
    (command, payload)
}
fn target_write<I: ReadIo>(
    io: &mut I,
    sequence: &mut u16,
    target: FcpTarget,
    data: &[u8],
) -> Result<(), ReadError<I::Error>> {
    let (command, payload) = target_packet(target, data);
    let result = transact_mode(io, command, *sequence, &payload, 0, true).map(|_| ());
    *sequence = sequence.wrapping_add(1);
    result
}
fn edited(
    state: &FcpReadback,
    routes: &[FcpRouteEdit],
    mixes: &[FcpMixEdit],
) -> Result<FcpReadback, ProtocolError> {
    let mut result = state.clone();
    let mut seen_routes = BTreeSet::new();
    for edit in routes {
        if !seen_routes.insert((edit.bank, edit.destination)) {
            return Err(ProtocolError::DuplicateDestination);
        }
        let dest = state
            .destinations
            .iter()
            .find(|d| d.pin == edit.destination)
            .ok_or(ProtocolError::UnknownPort {
                id: edit.destination,
                source: false,
            })?;
        if dest.fixed_source.is_some() {
            return Err(ProtocolError::InvalidMuxEdit);
        }
        if edit.source != 0 && !state.sources.iter().any(|s| s.pin == edit.source) {
            return Err(ProtocolError::UnknownPort {
                id: edit.source,
                source: true,
            });
        }
        let bank = result
            .banks
            .iter_mut()
            .find(|b| b.bank == edit.bank)
            .ok_or(ProtocolError::InvalidMuxBank(edit.bank))?;
        let word = bank
            .raw_words
            .iter_mut()
            .find(|w| **w != 0 && (**w & 0xfff) == u32::from(edit.destination))
            .ok_or(ProtocolError::InvalidMuxEdit)?;
        *word = (u32::from(edit.source) << 12) | u32::from(edit.destination);
    }
    let mut seen_mixes = BTreeSet::new();
    for edit in mixes {
        if !seen_mixes.insert((edit.output, edit.input)) {
            return Err(ProtocolError::DuplicateMixInput(edit.input));
        }
        if edit.coefficient > 32613 {
            return Err(ProtocolError::InvalidMixGain(i16::MAX));
        }
        let row = result
            .mixes
            .iter_mut()
            .find(|m| m.output == edit.output)
            .ok_or(ProtocolError::InvalidMixOutput(edit.output))?;
        let value = edit
            .input
            .checked_sub(1)
            .and_then(|i| row.coefficients.get_mut(usize::from(i)))
            .ok_or(ProtocolError::InvalidMixInput(edit.input))?;
        *value = edit.coefficient;
    }
    Ok(result)
}

/// Apply complete volatile rows/banks, preserving padding and every unedited value.
/// A failed or uncertain write triggers one bounded reinitialization and guarded
/// reverse rollback. No command persists configuration to flash.
pub fn apply_fcp<I: ReadIo>(
    mut io: I,
    candidate: CandidateModel,
    expected: &FcpReadback,
    routes: &[FcpRouteEdit],
    mixes: &[FcpMixEdit],
) -> Result<FcpWriteReport, FcpWriteError<I::Error>> {
    let (live, mut sequence) =
        read_state(&mut io, candidate).map_err(|e| write_error(FcpFailure::Read(e)))?;
    if live != *expected {
        return Err(write_error(FcpFailure::Drift));
    }
    let proposed = edited(&live, routes, mixes).map_err(|e| write_error(FcpFailure::Invalid(e)))?;
    let changed: Vec<_> = targets(&live)
        .into_iter()
        .filter(|&t| values(&live, t) != values(&proposed, t))
        .collect();
    let mut attempted = Vec::new();
    for target in changed {
        let current = match target_read(&mut io, &mut sequence, &live, target) {
            Ok(v) => v,
            Err(e) => {
                return Err(rollback(
                    &mut io,
                    candidate,
                    &live,
                    &proposed,
                    &attempted,
                    FcpFailure::Read(e),
                ))
            }
        };
        if current != values(&live, target) {
            return Err(rollback(
                &mut io,
                candidate,
                &live,
                &proposed,
                &attempted,
                FcpFailure::Drift,
            ));
        }
        attempted.push(target); // Sending may mutate the device even without an ACK.
        if let Err(e) = target_write(&mut io, &mut sequence, target, &values(&proposed, target)) {
            return Err(rollback(
                &mut io,
                candidate,
                &live,
                &proposed,
                &attempted,
                FcpFailure::Read(e),
            ));
        }
        match target_read(&mut io, &mut sequence, &live, target) {
            Ok(v) if v == values(&proposed, target) => (),
            Ok(_) => {
                return Err(rollback(
                    &mut io,
                    candidate,
                    &live,
                    &proposed,
                    &attempted,
                    FcpFailure::Verification(target),
                ))
            }
            Err(e) => {
                return Err(rollback(
                    &mut io,
                    candidate,
                    &live,
                    &proposed,
                    &attempted,
                    FcpFailure::Read(e),
                ))
            }
        }
    }
    // The verified snapshot records exactly the rows read back after each write.
    Ok(FcpWriteReport {
        original: live,
        verified: proposed,
    })
}

fn rollback<I: ReadIo>(
    io: &mut I,
    candidate: CandidateModel,
    original: &FcpReadback,
    proposed: &FcpReadback,
    attempted: &[FcpTarget],
    failure: FcpFailure<I::Error>,
) -> FcpWriteError<I::Error> {
    let mut error = write_error(failure);
    if attempted.is_empty() {
        return error;
    }
    let (live, mut sequence) = match read_state(io, candidate) {
        Ok(v) => v,
        Err(e) => {
            error
                .rollback
                .push((*attempted.last().unwrap(), FcpFailure::Read(e)));
            for &target in attempted.iter().rev().skip(1) {
                error
                    .rollback
                    .push((target, FcpFailure::Invalid(ProtocolError::SessionPoisoned)));
            }
            return error;
        }
    };
    if !same_fcp_layout(&live, original) {
        for &target in attempted.iter().rev() {
            error.rollback.push((target, FcpFailure::Drift));
        }
        return error;
    }
    let reverse: Vec<_> = attempted.iter().rev().copied().collect();
    for (index, &target) in reverse.iter().enumerate() {
        if let Err(failure) = restore_target(
            io,
            candidate,
            &mut sequence,
            original,
            proposed,
            target,
            true,
        ) {
            error.rollback.push((target, failure));
            // A failed transport may have poisoned the sequence. Reinitialize
            // before considering any independent remaining target.
            match read_state(io, candidate) {
                Ok((fresh, seq)) if same_fcp_layout(&fresh, original) => sequence = seq,
                Ok(_) => {
                    for &remaining in &reverse[index + 1..] {
                        error.rollback.push((remaining, FcpFailure::Drift));
                    }
                    break;
                }
                Err(e) => {
                    error.rollback.push((target, FcpFailure::Read(e)));
                    for &remaining in &reverse[index + 1..] {
                        error.rollback.push((
                            remaining,
                            FcpFailure::Invalid(ProtocolError::SessionPoisoned),
                        ));
                    }
                    break;
                }
            }
        }
    }
    error
}
fn restore_target<I: ReadIo>(
    io: &mut I,
    candidate: CandidateModel,
    sequence: &mut u16,
    original: &FcpReadback,
    last: &FcpReadback,
    target: FcpTarget,
    allow_original: bool,
) -> Result<(), FcpFailure<I::Error>> {
    let current = target_read(io, sequence, original, target).map_err(FcpFailure::Read)?;
    let old = values(original, target);
    if allow_original && current == old {
        return Ok(());
    }
    if current != values(last, target) {
        return Err(FcpFailure::Drift);
    }
    let write = target_write(io, sequence, target, &old);
    if write.is_err() {
        // Never reuse a possibly poisoned command stream. Initialization also
        // verifies the attempted target, even when the write ACK was lost.
        let (fresh, seq) = read_state(io, candidate).map_err(FcpFailure::Read)?;
        *sequence = seq;
        if !same_fcp_layout(&fresh, original) {
            return Err(FcpFailure::Drift);
        }
        if values(&fresh, target) != old {
            return Err(FcpFailure::Verification(target));
        }
        return Ok(());
    }
    let verified = target_read(io, sequence, original, target).map_err(FcpFailure::Read)?;
    if verified != old {
        return Err(FcpFailure::Verification(target));
    }
    Ok(())
}

/// Hand back only rows/banks still equal to the application's last readback.
/// Independent targets continue when another target has external drift.
pub(crate) fn restore_fcp<I: ReadIo>(
    mut io: I,
    candidate: CandidateModel,
    last: &FcpReadback,
    original: &FcpReadback,
) -> Result<(), FcpWriteError<I::Error>> {
    if !same_fcp_layout(last, original) {
        return Err(write_error(FcpFailure::Drift));
    }
    for state in [last, original] {
        validate_banks(&state.sources, &state.destinations, &state.banks)
            .map_err(|e| write_error(FcpFailure::Invalid(e)))?;
    }
    let (live, mut sequence) =
        read_state(&mut io, candidate).map_err(|e| write_error(FcpFailure::Read(e)))?;
    if !same_fcp_layout(&live, original) {
        return Err(write_error(FcpFailure::Drift));
    }
    let mut errors = Vec::new();
    let changed: Vec<_> = targets(original)
        .into_iter()
        .rev()
        .filter(|&t| values(original, t) != values(last, t))
        .collect();
    for (index, &target) in changed.iter().enumerate() {
        if let Err(failure) = restore_target(
            &mut io,
            candidate,
            &mut sequence,
            original,
            last,
            target,
            false,
        ) {
            let transport = matches!(&failure, FcpFailure::Read(_));
            errors.push((target, failure));
            if transport {
                match read_state(&mut io, candidate) {
                    Ok((fresh, seq)) if same_fcp_layout(&fresh, original) => sequence = seq,
                    Ok(_) => {
                        errors.push((target, FcpFailure::Drift));
                        for &remaining in &changed[index + 1..] {
                            errors.push((remaining, FcpFailure::Drift));
                        }
                        break;
                    }
                    Err(e) => {
                        errors.push((target, FcpFailure::Read(e)));
                        for &remaining in &changed[index + 1..] {
                            errors.push((
                                remaining,
                                FcpFailure::Invalid(ProtocolError::SessionPoisoned),
                            ));
                        }
                        break;
                    }
                }
            }
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        let (_, failure) = errors.remove(0);
        Err(FcpWriteError {
            failure,
            rollback: errors,
        })
    }
}

fn bad() -> ProtocolError {
    ProtocolError::InvalidDeviceMap
}

// Typed consumed fields reject duplicates; unrelated firmware metadata is ignored.
#[derive(Deserialize)]
struct DeviceMap {
    #[serde(rename = "device-specification")]
    specification: Specification,
}
#[derive(Deserialize)]
struct Specification {
    #[serde(deserialize_with = "bounded_ports")]
    sources: Vec<MapPort>,
    #[serde(deserialize_with = "bounded_ports")]
    destinations: Vec<MapPort>,
}
#[derive(Deserialize)]
struct MapPort {
    name: String,
    #[serde(rename = "router-pin")]
    pin: Value,
    #[serde(rename = "static-source", default, deserialize_with = "static_source")]
    fixed_source: Option<String>,
}

fn static_source<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<String>, D::Error> {
    String::deserialize(deserializer).map(Some)
}

fn bounded_ports<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Vec<MapPort>, D::Error> {
    struct PortsVisitor;
    impl<'de> serde::de::Visitor<'de> for PortsVisitor {
        type Value = Vec<MapPort>;
        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("at most 4095 router ports")
        }
        fn visit_seq<A: serde::de::SeqAccess<'de>>(
            self,
            mut seq: A,
        ) -> Result<Self::Value, A::Error> {
            let mut ports = Vec::new();
            while ports.len() < 0xfff {
                match seq.next_element()? {
                    Some(port) => ports.push(port),
                    None => return Ok(ports),
                }
            }
            if seq.next_element::<serde::de::IgnoredAny>()?.is_some() {
                return Err(serde::de::Error::custom("too many router ports"));
            }
            Ok(ports)
        }
    }
    deserializer.deserialize_seq(PortsVisitor)
}

fn decode_map(encoded: &[u8]) -> Result<(Vec<FcpPort>, Vec<FcpPort>), ProtocolError> {
    // The upstream decoder strips terminal NULs and accepts Base64 whitespace.
    let end = encoded.iter().rposition(|&b| b != 0).map_or(0, |n| n + 1);
    let normalized: Vec<u8> = encoded[..end]
        .iter()
        .copied()
        .filter(|b| !b.is_ascii_whitespace())
        .collect();
    let compressed = STANDARD.decode(normalized).map_err(|_| bad())?;
    let mut inflater = Decompress::new(true);
    // One extra byte distinguishes an exactly bounded document from overflow.
    let mut json = vec![0; MAX_JSON + 1];
    let status = inflater
        .decompress(&compressed, &mut json, FlushDecompress::Finish)
        .map_err(|_| bad())?;
    if status != Status::StreamEnd
        || inflater.total_in() != compressed.len() as u64
        || inflater.total_out() > MAX_JSON as u64
    {
        return Err(bad());
    }
    json.truncate(inflater.total_out() as usize);
    let root: DeviceMap = serde_json::from_slice(&json).map_err(|_| bad())?;
    let sources = ports(&root.specification.sources, None)?;
    let destinations = ports(&root.specification.destinations, Some(&sources))?;
    if sources.is_empty() || destinations.is_empty() {
        return Err(bad());
    }
    Ok((sources, destinations))
}

fn pin(value: &Value) -> Result<u16, ProtocolError> {
    let n = if let Some(s) = value.as_str() {
        if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
            return Err(bad());
        }
        s.parse::<u16>().map_err(|_| bad())?
    } else {
        u16::try_from(value.as_u64().ok_or_else(bad)?).map_err(|_| bad())?
    };
    if n == 0 || n > 0xfff {
        return Err(bad());
    }
    Ok(n)
}

fn ports(rows: &[MapPort], sources: Option<&[FcpPort]>) -> Result<Vec<FcpPort>, ProtocolError> {
    // Router pins occupy a 12-bit domain, excluding the synthetic Off pin.
    if rows.len() > 0xfff {
        return Err(bad());
    }
    let mut pins = BTreeSet::new();
    let mut names = BTreeSet::new();
    let mut result = Vec::with_capacity(rows.len());
    for row in rows {
        let name = &row.name;
        if name.is_empty() || name.chars().any(char::is_control) || !names.insert(name) {
            return Err(bad());
        }
        let pin = pin(&row.pin)?;
        if !pins.insert(pin) {
            return Err(bad());
        }
        let fixed_source = match &row.fixed_source {
            None => None,
            Some(name) => Some(
                sources
                    .ok_or_else(bad)?
                    .iter()
                    .find(|p| &p.name == name)
                    .ok_or_else(bad)?
                    .pin,
            ),
        };
        result.push(FcpPort {
            pin,
            name: name.to_owned(),
            fixed_source,
        });
    }
    Ok(result)
}

fn validate_banks(
    sources: &[FcpPort],
    destinations: &[FcpPort],
    banks: &[FcpBank],
) -> Result<(), ProtocolError> {
    for bank in banks {
        let mut seen = BTreeSet::new();
        for &word in &bank.raw_words {
            if word == 0 {
                continue;
            } // Firmware's unused/padding slot.
            if word & 0xff00_0000 != 0 {
                return Err(ProtocolError::InvalidMuxBits);
            }
            let source = (word >> 12) as u16;
            let dest = (word & 0xfff) as u16;
            let destination =
                destinations
                    .iter()
                    .find(|p| p.pin == dest)
                    .ok_or(ProtocolError::UnknownPort {
                        id: dest,
                        source: false,
                    })?;
            if source != 0 && !sources.iter().any(|p| p.pin == source) {
                return Err(ProtocolError::UnknownPort {
                    id: source,
                    source: true,
                });
            }
            if !seen.insert(dest) {
                return Err(ProtocolError::DuplicateDestination);
            }
            if let Some(fixed) = destination.fixed_source {
                if fixed != source {
                    return Err(ProtocolError::InvalidDeviceMap);
                }
            }
        }
        // The device map also contains pins absent from the mux at a given
        // rate. Upstream selects visible sinks using a separate curated map;
        // absence here means inactive/unobserved, never an inferred route.
        if bank.bank == 0 && seen.is_empty() {
            return Err(ProtocolError::UnsupportedMuxGeometry);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::{write::ZlibEncoder, Compression};
    use std::{collections::VecDeque, io::Write};

    fn compressed(json: &[u8]) -> Vec<u8> {
        let mut z = ZlibEncoder::new(Vec::new(), Compression::default());
        z.write_all(json).unwrap();
        z.finish().unwrap()
    }
    fn map() -> Vec<u8> {
        STANDARD.encode(compressed(br#"{"device-specification":{"sources":[{"name":"Input","router-pin":"128"}],"destinations":[{"name":"Output","router-pin":"256"},{"name":"Fixed","router-pin":"257","static-source":"Input"}]}}"#)).into_bytes()
    }
    fn topology() -> (Vec<FcpPort>, Vec<FcpPort>) {
        decode_map(&map()).unwrap()
    }

    fn snapshot() -> FcpReadback {
        let (sources, destinations) = topology();
        FcpReadback {
            model: CandidateModel::Scarlett18i20Gen4,
            firmware: 4321,
            mixer_inputs: 2,
            mixer_outputs: 1,
            sources,
            destinations,
            banks: vec![FcpBank {
                bank: 0,
                raw_words: vec![128 << 12 | 256, 0, 128 << 12 | 257],
            }],
            mixes: vec![MixReadback {
                output: 1,
                coefficients: vec![0, 65535],
            }],
        }
    }
    struct Device {
        state: FcpReadback,
        writes: Vec<(u32, Vec<u8>)>,
        pending: Option<(u32, u16, Vec<u8>)>,
        fail_write: Vec<usize>,
        fail_ack: bool,
        drift_on_reinit: bool,
        initializations: usize,
    }
    struct DeviceIo(std::rc::Rc<std::cell::RefCell<Device>>);
    fn device() -> (DeviceIo, std::rc::Rc<std::cell::RefCell<Device>>) {
        let d = std::rc::Rc::new(std::cell::RefCell::new(Device {
            state: snapshot(),
            writes: vec![],
            pending: None,
            fail_write: vec![],
            fail_ack: false,
            drift_on_reinit: false,
            initializations: 0,
        }));
        (DeviceIo(d.clone()), d)
    }
    impl ReadIo for DeviceIo {
        type Error = ();
        fn initial_read(&mut self, _: &mut [u8; 24]) -> Result<usize, ()> {
            let mut d = self.0.borrow_mut();
            d.initializations += 1;
            if d.initializations > 1 && d.drift_on_reinit {
                d.state.mixes[0].coefficients[1] = 65534;
            }
            Ok(24)
        }
        fn prepare_notifications(&mut self) -> Result<(), ()> {
            self.0.borrow_mut().pending = None;
            Ok(())
        }
        fn send_request(&mut self, bytes: &[u8]) -> Result<usize, ()> {
            let mut d = self.0.borrow_mut();
            let command = u32::from_le_bytes(bytes[..4].try_into().unwrap());
            let seq = u16::from_le_bytes(bytes[6..8].try_into().unwrap());
            let p = &bytes[16..];
            let response = match command {
                0 => vec![],
                2 => {
                    let mut info = vec![0; 84];
                    info[8..12].copy_from_slice(&d.state.firmware.to_le_bytes());
                    info
                }
                0x2000 => vec![1, 2, 0, 0, 0, 0, 0, 0],
                0x3000 => vec![3, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
                0x80000c => {
                    let mut r = vec![0; 4];
                    r[2..].copy_from_slice(&(map().len() as u16).to_le_bytes());
                    r
                }
                0x80000d => map(),
                0x3001 => values(&d.state, FcpTarget::Bank(p[3])),
                0x2001 => values(&d.state, FcpTarget::Mix(p[0] + 1)),
                0x3002 | 0x2002 => {
                    d.writes.push((command, p.to_vec()));
                    if command == 0x3002 {
                        d.state.banks[0].raw_words = p[4..]
                            .chunks_exact(4)
                            .map(|v| u32::from_le_bytes(v.try_into().unwrap()))
                            .collect();
                    } else {
                        d.state.mixes[0].coefficients = p[2..]
                            .chunks_exact(2)
                            .map(|v| u16::from_le_bytes(v.try_into().unwrap()))
                            .collect();
                    }
                    if d.fail_write.contains(&d.writes.len()) {
                        d.fail_ack = true;
                    }
                    vec![]
                }
                _ => panic!("unexpected opcode {command:x}"),
            };
            d.pending = Some((command, seq, response));
            Ok(bytes.len())
        }
        fn next_notification(&mut self, bytes: &mut [u8; 8], _: Duration) -> Result<usize, ()> {
            let mut d = self.0.borrow_mut();
            if d.fail_ack {
                d.fail_ack = false;
                return Err(());
            }
            bytes[0] = 1;
            Ok(8)
        }
        fn receive_response(&mut self, bytes: &mut [u8]) -> Result<usize, ()> {
            let (cmd, seq, p) = self.0.borrow_mut().pending.take().unwrap();
            bytes.copy_from_slice(&packet(
                match cmd {
                    0 => Command::Init,
                    2 => Command::Info,
                    0x2000 => Command::MixInfo,
                    0x3000 => Command::MuxInfo,
                    0x80000c => Command::DevmapInfo,
                    0x80000d => Command::DevmapRead,
                    0x3001 => Command::Mux,
                    0x2001 => Command::Mix,
                    0x3002 => Command::SetMux,
                    0x2002 => Command::SetMix,
                    _ => unreachable!(),
                },
                seq,
                &p,
            ));
            Ok(bytes.len())
        }
    }
    #[test]
    fn transactions_preserve_raw_neighbors_padding_and_literal_write_abi() {
        let (io, d) = device();
        let report = apply_fcp(
            io,
            snapshot().model,
            &snapshot(),
            &[FcpRouteEdit {
                bank: 0,
                destination: 256,
                source: 0,
            }],
            &[FcpMixEdit {
                output: 1,
                input: 1,
                coefficient: 32613,
            }],
        )
        .unwrap();
        assert_eq!(report.verified.mixes[0].coefficients, vec![32613, 65535]);
        assert_eq!(
            d.borrow().writes,
            vec![
                (0x3002, vec![0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 1, 1, 8, 0]),
                (0x2002, vec![0, 0, 0x65, 0x7f, 0xff, 0xff])
            ]
        );
        let (command, payload) = target_packet(FcpTarget::Bank(2), &vec![0; 255 * 4]);
        assert_eq!(command, Command::SetMux);
        assert_eq!(payload.len(), 1024);
        assert_eq!(&payload[..4], &[0, 0, 2, 0]);
    }
    #[test]
    fn edits_reject_fixed_inactive_duplicate_and_out_of_range() {
        let s = snapshot();
        for e in [
            FcpRouteEdit {
                bank: 0,
                destination: 257,
                source: 128,
            },
            FcpRouteEdit {
                bank: 1,
                destination: 256,
                source: 0,
            },
            FcpRouteEdit {
                bank: 0,
                destination: 258,
                source: 0,
            },
            FcpRouteEdit {
                bank: 0,
                destination: 256,
                source: 129,
            },
        ] {
            assert!(edited(&s, &[e], &[]).is_err());
        }
        let e = FcpRouteEdit {
            bank: 0,
            destination: 256,
            source: 0,
        };
        assert!(edited(&s, &[e, e], &[]).is_err());
        for (output, input, coefficient) in [(0, 1, 0), (1, 0, 0), (1, 3, 0), (1, 1, 32614)] {
            assert!(edited(
                &s,
                &[],
                &[FcpMixEdit {
                    output,
                    input,
                    coefficient
                }]
            )
            .is_err());
        }
        let (io, d) = device();
        let mut stale = s.clone();
        stale.firmware += 1;
        assert!(matches!(
            apply_fcp(io, s.model, &stale, &[], &[]),
            Err(FcpWriteError {
                failure: FcpFailure::Drift,
                ..
            })
        ));
        assert!(d.borrow().writes.is_empty());
        let (io, d) = device();
        apply_fcp(io, s.model, &s, &[], &[]).unwrap();
        assert!(d.borrow().writes.is_empty());
        let (io, d) = device();
        assert!(apply_fcp(io, CandidateModel::IsaC8x, &s, &[], &[]).is_err());
        assert!(d.borrow().writes.is_empty());
    }
    #[test]
    fn lost_ack_rolls_back_attempted_rows_in_reverse() {
        let (io, d) = device();
        d.borrow_mut().fail_write = vec![2];
        let e = apply_fcp(
            io,
            snapshot().model,
            &snapshot(),
            &[FcpRouteEdit {
                bank: 0,
                destination: 256,
                source: 0,
            }],
            &[FcpMixEdit {
                output: 1,
                input: 1,
                coefficient: 1,
            }],
        )
        .unwrap_err();
        assert!(matches!(e.failure, FcpFailure::Read(ReadError::Io(()))));
        assert!(e.rollback.is_empty());
        assert_eq!(d.borrow().state, snapshot());
        assert_eq!(
            d.borrow()
                .writes
                .iter()
                .map(|(c, _)| *c)
                .collect::<Vec<_>>(),
            vec![0x3002, 0x2002, 0x2002, 0x3002]
        );
        assert_eq!(d.borrow().initializations, 2);
    }
    #[test]
    fn lost_rollback_ack_is_success_when_original_bytes_are_proven() {
        let (io, d) = device();
        d.borrow_mut().fail_write = vec![2, 3];
        let e = apply_fcp(
            io,
            snapshot().model,
            &snapshot(),
            &[FcpRouteEdit {
                bank: 0,
                destination: 256,
                source: 0,
            }],
            &[FcpMixEdit {
                output: 1,
                input: 1,
                coefficient: 1,
            }],
        )
        .unwrap_err();
        assert!(matches!(e.failure, FcpFailure::Read(ReadError::Io(()))));
        assert!(e.rollback.is_empty());
        assert_eq!(d.borrow().state, snapshot());
        assert_eq!(d.borrow().initializations, 3);
    }
    #[test]
    fn shared_layout_compares_slot_topology_but_allows_source_and_gain_changes() {
        let original = snapshot();
        let mut changed = original.clone();
        changed.banks[0].raw_words[0] = 256;
        changed.mixes[0].coefficients[0] = 100;
        assert!(same_fcp_layout(&original, &changed));
        changed.banks[0].raw_words.swap(0, 1);
        assert!(!same_fcp_layout(&original, &changed));
    }
    #[test]
    fn rollback_and_handback_preserve_external_drift_and_restore_independent_rows() {
        let (io, d) = device();
        {
            let mut dev = d.borrow_mut();
            dev.fail_write = vec![2];
            dev.drift_on_reinit = true;
        }
        let error = apply_fcp(
            io,
            snapshot().model,
            &snapshot(),
            &[FcpRouteEdit {
                bank: 0,
                destination: 256,
                source: 0,
            }],
            &[FcpMixEdit {
                output: 1,
                input: 1,
                coefficient: 1,
            }],
        )
        .unwrap_err();
        assert_eq!(error.rollback, vec![(FcpTarget::Mix(1), FcpFailure::Drift)]);
        assert_eq!(d.borrow().state.banks, snapshot().banks);
        assert_eq!(d.borrow().state.mixes[0].coefficients, vec![1, 65534]);
        let (io, d) = device();
        let original = snapshot();
        let last = edited(
            &original,
            &[FcpRouteEdit {
                bank: 0,
                destination: 256,
                source: 0,
            }],
            &[FcpMixEdit {
                output: 1,
                input: 1,
                coefficient: 1,
            }],
        )
        .unwrap();
        {
            let mut dev = d.borrow_mut();
            dev.state = last.clone();
            dev.state.mixes[0].coefficients[0] = 2;
        }
        let error = restore_fcp(io, original.model, &last, &original).unwrap_err();
        assert_eq!(error.failure, FcpFailure::Drift);
        assert_eq!(d.borrow().state.banks, original.banks);
        assert_eq!(d.borrow().state.mixes[0].coefficients[0], 2);
    }
    #[test]
    fn map_accepts_upstream_decimal_pins_whitespace_and_terminal_nuls() {
        let mut encoded = b"\n\t".to_vec();
        encoded.extend(map());
        encoded.extend(b" \r\n\0\0");
        let (s, d) = decode_map(&encoded).unwrap();
        assert_eq!(s[0].pin, 128);
        assert_eq!(d[1].fixed_source, Some(128));
        for v in [
            serde_json::json!("0x80"),
            serde_json::json!("128x"),
            serde_json::json!(0),
            serde_json::json!(4096),
            serde_json::json!(128.0),
            serde_json::json!(-1),
        ] {
            assert!(pin(&v).is_err());
        }
        assert_eq!(pin(&serde_json::json!(128)), Ok(128));
    }

    #[test]
    fn map_rejects_trailing_compressed_data_bombs_and_malformed_topology() {
        let mut bytes = compressed(b"{}");
        bytes.push(0);
        assert!(decode_map(STANDARD.encode(bytes).as_bytes()).is_err());
        assert!(decode_map(
            STANDARD
                .encode(compressed(&vec![b' '; MAX_JSON + 1]))
                .as_bytes()
        )
        .is_err());
        assert!(decode_map(b"AA\0AA").is_err());
        for json in [
            br#"{"device-specification":{"sources":[],"sources":[],"destinations":[]}}"#.as_slice(),
            br#"{"device-specification":{"sources":[{"name":"a","name":"b","router-pin":"1"}],"destinations":[]}}"#.as_slice(),
            br#"{"device-specification":{"sources":[{"name":"a","router-pin":"1","router-pin":"2"}],"destinations":[]}}"#.as_slice(),
        ] { assert!(decode_map(STANDARD.encode(compressed(json)).as_bytes()).is_err()); }
        let too_many: Vec<MapPort> = (0..4096)
            .map(|_| MapPort {
                name: "a".into(),
                pin: Value::from(1),
                fixed_source: None,
            })
            .collect();
        assert!(ports(&too_many, None).is_err());
        let (sources, _) = topology();
        for rows in [
            serde_json::json!([{"name":"a","router-pin":"1"},{"name":"b","router-pin":"1"}]),
            serde_json::json!([{"name":"a","router-pin":"1"},{"name":"a","router-pin":"2"}]),
            serde_json::json!([{"name":"a","router-pin":"1","static-source":"missing"}]),
            serde_json::json!([{"name":"a"}]),
        ] {
            let parsed = serde_json::from_value::<Vec<MapPort>>(rows);
            assert!(parsed.map_or(true, |rows| ports(&rows, Some(&sources)).is_err()));
        }
    }

    #[test]
    fn route_words_require_explained_pins_padding_and_populated_base_bank() {
        let (sources, mut destinations) = topology();
        destinations.push(FcpPort {
            pin: 258,
            name: "Inactive map-only destination".into(),
            fixed_source: None,
        });
        let valid = FcpBank {
            bank: 0,
            raw_words: vec![128 << 12 | 256, 0],
        };
        assert_eq!(
            validate_banks(&sources, &destinations, &[valid.clone()]),
            Ok(())
        );
        for words in [
            vec![128 << 12],
            vec![129 << 12 | 256],
            vec![128 << 12 | 259],
            vec![0x1000_0000 | 256],
            vec![256, 256],
            vec![0],
            vec![128 << 12 | 256, 257],
        ] {
            assert!(validate_banks(
                &sources,
                &destinations,
                &[FcpBank {
                    bank: 0,
                    raw_words: words
                }]
            )
            .is_err());
        }
        assert_eq!(
            validate_banks(
                &sources,
                &destinations,
                &[
                    valid,
                    FcpBank {
                        bank: 1,
                        raw_words: vec![0]
                    }
                ]
            ),
            Ok(())
        );
    }

    struct Replay {
        steps: VecDeque<(u32, u16, Vec<u8>, Vec<u8>)>,
        pending: Option<(u32, u16, Vec<u8>)>,
    }
    impl ReadIo for Replay {
        type Error = ();
        fn initial_read(&mut self, _: &mut [u8; 24]) -> Result<usize, ()> {
            Ok(24)
        }
        fn prepare_notifications(&mut self) -> Result<(), ()> {
            Ok(())
        }
        fn send_request(&mut self, bytes: &[u8]) -> Result<usize, ()> {
            let (cmd, sequence, payload, response) = self.steps.pop_front().unwrap();
            assert_eq!(&bytes[..4], &cmd.to_le_bytes());
            assert_eq!(&bytes[4..6], &(payload.len() as u16).to_le_bytes());
            assert_eq!(&bytes[6..8], &sequence.to_le_bytes());
            assert_eq!(&bytes[8..16], &[0; 8]);
            assert_eq!(&bytes[16..], &payload);
            self.pending = Some((cmd, sequence, response));
            Ok(bytes.len())
        }
        fn next_notification(&mut self, bytes: &mut [u8; 8], _: Duration) -> Result<usize, ()> {
            bytes[0] = 1;
            Ok(8)
        }
        fn receive_response(&mut self, bytes: &mut [u8]) -> Result<usize, ()> {
            let (cmd, sequence, response) = self.pending.take().unwrap();
            assert_eq!(bytes.len(), HEADER + response.len());
            bytes[..4].copy_from_slice(&cmd.to_le_bytes());
            bytes[4..6].copy_from_slice(&(response.len() as u16).to_le_bytes());
            bytes[6..8].copy_from_slice(&sequence.to_le_bytes());
            bytes[8..16].fill(0);
            bytes[16..].copy_from_slice(&response);
            Ok(bytes.len())
        }
    }
    #[test]
    fn exact_read_only_wire_replay_and_modern_sequence() {
        let mut encoded = vec![b'\n'; 1024];
        encoded.extend(map());
        let mut info = vec![0; 84];
        info[8..12].copy_from_slice(&4321u32.to_le_bytes());
        let mut mux_info = vec![0; 12];
        mux_info[0] = 2;
        mux_info[2] = 1;
        let mut map_info = vec![0; 4];
        map_info[2..].copy_from_slice(&(encoded.len() as u16).to_le_bytes());
        let steps = VecDeque::from(vec![
            (0x0000, 0, vec![], vec![]),
            (0x0002, 1, vec![], info),
            (0x2000, 2, vec![], vec![1, 2, 0, 0, 0, 0, 0, 0]),
            (0x3000, 3, vec![], mux_info),
            (0x80000c, 4, vec![], map_info),
            (0x80000d, 5, vec![0; 4], encoded[..1024].to_vec()),
            (0x80000d, 6, vec![1, 0, 0, 0], encoded[1024..].to_vec()),
            (
                0x3001,
                7,
                vec![0, 0, 2, 0],
                [((128u32 << 12) | 256).to_le_bytes(), [0; 4]].concat(),
            ),
            (0x3001, 8, vec![0, 0, 1, 1], vec![0; 4]),
            (0x2001, 9, vec![0, 0, 2, 0], vec![0, 0, 0xff, 0xff]),
        ]);
        let readback = read_fcp(
            Replay {
                steps,
                pending: None,
            },
            CandidateModel::Scarlett18i20Gen4,
        )
        .unwrap();
        assert_eq!(readback.firmware, 4321);
        assert_eq!(readback.mixes[0].coefficients, vec![0, 65535]);
        let reply = packet(Command::Info, 0, &[0; 84]);
        assert_eq!(
            decode_reply_mode(Command::Info, 1, 84, &reply, true),
            Err(ProtocolError::InvalidSequence)
        );
        assert!(decode_reply(Command::Info, 1, 84, &reply).is_ok());
        assert_eq!(
            CandidateModel::Scarlett18i20Gen4.validate_firmware(4321),
            Err(ProtocolError::UnsupportedProtocol)
        );
        assert!(matches!(
            ReadSession::initialize(
                Replay {
                    steps: VecDeque::new(),
                    pending: None,
                },
                CandidateModel::IsaC8x
            ),
            Err(ReadError::Protocol(ProtocolError::UnsupportedProtocol))
        ));
    }

    #[test]
    fn modern_reply_retains_all_envelope_guards_and_larger_bound() {
        let mut reply = packet(Command::DevmapRead, 2, &[0; 1024]);
        assert!(decode_reply_mode(Command::DevmapRead, 2, 1024, &reply, true).is_ok());
        assert!(decode_reply(Command::DevmapRead, 2, 1024, &reply).is_err());
        for (index, error) in [
            (0, ProtocolError::InvalidCommand),
            (4, ProtocolError::InvalidPayloadSize),
            (6, ProtocolError::InvalidSequence),
            (8, ProtocolError::DeviceStatus(1)),
            (12, ProtocolError::NonzeroReserved),
        ] {
            let original = reply[index];
            reply[index] ^= 1;
            assert_eq!(
                decode_reply_mode(Command::DevmapRead, 2, 1024, &reply, true),
                Err(error)
            );
            reply[index] = original;
        }
        assert!(decode_reply_mode(
            Command::DevmapRead,
            2,
            1024,
            &reply[..reply.len() - 1],
            true
        )
        .is_err());
    }
}
