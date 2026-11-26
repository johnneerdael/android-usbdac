//! Scarlett 18i20 Gen 2 protocol primitives, independently implemented
//! from Linux v7.0 `mixer_scarlett2.c` protocol evidence. The initial trace
//! described there identifies firmware 1083. No other firmware is admitted.
//!
//! Model ports and packet decoding are source-backed; physical routing has not
//! been validated on Scarlett hardware. Legacy GET_MUX selector zero remains
//! independent of the optional, runtime-probed complete per-bank transaction.
//! Original coefficients remain unsigned wire values for exact restoration.

use std::time::{Duration, Instant};

const HEADER: usize = 16;
pub(crate) const MAX_PAYLOAD: usize = 260;
const ACK_TIMEOUT: Duration = Duration::from_secs(1);
const MAX_NOTIFICATIONS: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CandidateModel {
    Scarlett18i20Gen2,
}

impl CandidateModel {
    pub fn recognize(vendor: u16, product: u16) -> Option<Self> {
        (vendor == 0x1235 && product == 0x8201).then_some(Self::Scarlett18i20Gen2)
    }
    pub fn name(self) -> &'static str {
        "Scarlett 18i20 Gen 2"
    }
    pub fn validate_firmware(self, firmware: u32) -> Result<ReadModel, ProtocolError> {
        if firmware != 1083 {
            return Err(ProtocolError::UnsupportedFirmware(firmware));
        }
        Ok(ReadModel {
            candidate: self,
            firmware,
        })
    }
}

/// A model whose firmware has passed the narrow source-evidence guard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReadModel {
    candidate: CandidateModel,
    firmware: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum PortKind {
    Off,
    Analogue,
    Spdif,
    Adat,
    Mix,
    Pcm,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Port {
    Off,
    /// One-based channel identity within this port kind.
    Channel {
        kind: PortKind,
        channel: u8,
    },
}

impl ReadModel {
    pub fn candidate(self) -> CandidateModel {
        self.candidate
    }
    pub fn firmware(self) -> u32 {
        self.firmware
    }
    pub fn mixer_inputs(self) -> u8 {
        18
    }
    pub fn mixer_outputs(self) -> u8 {
        10
    }
    pub fn mux_destinations(self) -> usize {
        56
    }

    fn count(self, kind: PortKind, source: bool) -> u8 {
        match (kind, source) {
            (PortKind::Off, true) => 1,
            (PortKind::Off, false) => 0,
            (PortKind::Analogue, true) => 8,
            (PortKind::Analogue, false) => 10,
            (PortKind::Spdif, _) => 2,
            (PortKind::Adat, _) => 8,
            (PortKind::Mix, true) => 10,
            (PortKind::Mix, false) => 18,
            (PortKind::Pcm, true) => 20,
            (PortKind::Pcm, false) => 18,
        }
    }

    fn port(self, id: u16, source: bool) -> Result<Port, ProtocolError> {
        if id == 0 && source {
            return Ok(Port::Off);
        }
        for (kind, base) in [
            (PortKind::Analogue, 0x080),
            (PortKind::Spdif, 0x180),
            (PortKind::Adat, 0x200),
            (PortKind::Mix, 0x300),
            (PortKind::Pcm, 0x600),
        ] {
            if id >= base && id - base < u16::from(self.count(kind, source)) {
                return Ok(Port::Channel {
                    kind,
                    channel: (id - base + 1) as u8,
                });
            }
        }
        Err(ProtocolError::UnknownPort { id, source })
    }
}

/// Descriptor-only capability. Firmware admission happens during the explicit
/// read operation; this object alone authorizes no protocol transfer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReadCapability {
    pub candidate: CandidateModel,
    pub configuration: u8,
    pub interface: u8,
    pub notification_endpoint: u8,
    pub interface_claimed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MuxRoute {
    pub source: Port,
    pub destination: Port,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MixReadback {
    /// One-based mixer output. Coefficients preserve input order 1 through 18.
    pub output: u8,
    pub coefficients: Vec<u16>,
}

/// Bounded half-dB gain; -160 means -80 dB, 24 means +12 dB.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MixGain(i16);
impl MixGain {
    pub fn from_half_db(value: i16) -> Result<Self, ProtocolError> {
        if !(-160..=24).contains(&value) {
            return Err(ProtocolError::InvalidMixGain(value));
        }
        Ok(Self(value))
    }
    pub fn half_db(self) -> i16 {
        self.0
    }
    pub fn coefficient(self) -> u16 {
        (8192.0 * 10.0_f64.powf(f64::from(self.0) / 40.0)).floor() as u16
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MixGainEdit {
    /// One-based input in the selected output row.
    pub input: u8,
    pub gain: MixGain,
}

/// Expected complete raw row and bounded gain replacements. Untouched values
/// are carried through exactly, even when they are outside the editable scale.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MixRowEdit {
    pub expected: MixReadback,
    pub gains: Vec<MixGainEdit>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MixWriteReport {
    pub original: MixReadback,
    pub verified: MixReadback,
}

#[derive(Debug, PartialEq, Eq)]
pub enum MixFailure<E> {
    Read(ReadError<E>),
    Invalid(ProtocolError),
    Drift { actual: MixReadback },
    Verification { actual: MixReadback },
}

/// `rollback` is present only when the attempted row could not be restored and
/// verified. The original failure is always retained, including transport errors.
#[derive(Debug, PartialEq, Eq)]
pub struct MixWriteError<E> {
    pub failure: MixFailure<E>,
    pub rollback: Option<MixFailure<E>>,
}

/// Full source-backed per-bank payload. Private words prevent callers from
/// inventing a rollback baseline; every transaction also freshly rereads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MuxBankSnapshot {
    bank: u8,
    words: Vec<u32>,
}
impl MuxBankSnapshot {
    pub fn bank(&self) -> u8 { self.bank }
    pub fn raw_words(&self) -> &[u32] { &self.words }
    pub fn routes(&self) -> Vec<MuxRoute> {
        let model = CandidateModel::Scarlett18i20Gen2.validate_firmware(1083).unwrap();
        self.words.iter().copied().filter(|word| *word != 0).map(|word| MuxRoute {
            source: model.port(((word >> 12) & 0xfff) as u16, true).unwrap(),
            destination: model.port((word & 0xfff) as u16, false).unwrap(),
        }).collect()
    }
}

/// Runtime-observed complete bank geometry for the admitted firmware only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MuxSnapshot {
    pub model: ReadModel,
    pub banks: Vec<MuxBankSnapshot>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MuxRouteEdit {
    /// 0 = base rate, 1 = double rate, 2 = quad rate.
    pub bank: u8,
    pub destination: Port,
    pub source: Port,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MuxEdit {
    pub expected: MuxSnapshot,
    pub routes: Vec<MuxRouteEdit>,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MuxWriteReport {
    pub original: MuxSnapshot,
    pub verified: MuxSnapshot,
}
#[derive(Debug, PartialEq, Eq)]
pub enum MuxFailure<E> {
    Read(ReadError<E>),
    Invalid(ProtocolError),
    Drift { actual: MuxSnapshot },
    Verification { bank: u8, actual: MuxBankSnapshot },
}
#[derive(Debug, PartialEq, Eq)]
pub struct MuxWriteError<E> {
    pub failure: MuxFailure<E>,
    /// Every attempted bank is restored separately, including an uncertain SET.
    /// Empty means all attempted banks were restored and verified (or no SET).
    pub rollback: Vec<(u8, MuxFailure<E>)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoutingReadback {
    pub model: ReadModel,
    /// GET_MUX with wire argument zero; not a snapshot of every rate bank.
    pub mux_zero: Vec<MuxRoute>,
    pub mixes: Vec<MixReadback>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProtocolError {
    UnsupportedFirmware(u32),
    InvalidMixOutput(u8),
    InvalidMixInput(u8),
    InvalidMixGain(i16),
    DuplicateMixInput(u8),
    InvalidLength { expected: usize, actual: usize },
    InvalidCommand,
    InvalidSequence,
    InvalidPayloadSize,
    DeviceStatus(u32),
    NonzeroReserved,
    UnknownPort { id: u16, source: bool },
    DuplicateDestination,
    InvalidMuxBits,
    UnsupportedMuxGeometry,
    InvalidMuxBank(u8),
    InvalidMuxEdit,
    AcknowledgementTimeout,
    InvalidNotification,
    SessionPoisoned,
}

#[derive(Debug, PartialEq, Eq)]
pub enum ReadError<E> {
    Protocol(ProtocolError),
    Io(E),
}

/// Adapter ownership must cover the whole initialization/read sequence. It must
/// hold the exact device's already claimed vendor interface and interrupt IN
/// endpoint, with all audio streams stopped. No generic command API is exposed.
pub trait ReadIo {
    type Error;
    fn initial_read(&mut self, bytes: &mut [u8; 24]) -> Result<usize, Self::Error>;
    /// Start owning notifications and discard stale packets for the documented
    /// 20 ms initialization settling window, with a bounded drain.
    fn prepare_notifications(&mut self) -> Result<(), Self::Error>;
    fn send_request(&mut self, bytes: &[u8]) -> Result<usize, Self::Error>;
    /// Return actual length; respect the supplied total remaining timeout.
    fn next_notification(
        &mut self,
        bytes: &mut [u8; 8],
        timeout: Duration,
    ) -> Result<usize, Self::Error>;
    fn receive_response(&mut self, bytes: &mut [u8]) -> Result<usize, Self::Error>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Command {
    Init,
    Info,
    Mux,
    Mix,
    SetMix,
    MuxInfo,
    SetMux,
}
impl Command {
    fn code(self) -> u32 {
        match self {
            Self::Init => 0,
            Self::Info => 2,
            Self::Mux => 0x3001,
            Self::Mix => 0x2001,
            Self::SetMix => 0x2002,
            Self::MuxInfo => 0x3000,
            Self::SetMux => 0x3002,
        }
    }
    fn initializing(self) -> bool {
        matches!(self, Self::Init | Self::Info)
    }
}

fn packet(command: Command, sequence: u16, payload: &[u8]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(HEADER + payload.len());
    bytes.extend(command.code().to_le_bytes());
    bytes.extend((payload.len() as u16).to_le_bytes());
    bytes.extend(sequence.to_le_bytes());
    bytes.extend([0; 8]);
    bytes.extend(payload);
    bytes
}

fn decode_reply<'a>(
    command: Command,
    sequence: u16,
    size: usize,
    bytes: &'a [u8],
) -> Result<&'a [u8], ProtocolError> {
    let expected = HEADER + size;
    if size > MAX_PAYLOAD || bytes.len() != expected {
        return Err(ProtocolError::InvalidLength {
            expected,
            actual: bytes.len(),
        });
    }
    if u32::from_le_bytes(bytes[0..4].try_into().unwrap()) != command.code() {
        return Err(ProtocolError::InvalidCommand);
    }
    if usize::from(u16::from_le_bytes(bytes[4..6].try_into().unwrap())) != size {
        return Err(ProtocolError::InvalidPayloadSize);
    }
    let reply_sequence = u16::from_le_bytes(bytes[6..8].try_into().unwrap());
    if reply_sequence != sequence
        && !(command.initializing() && sequence == 1 && reply_sequence == 0)
    {
        return Err(ProtocolError::InvalidSequence);
    }
    let status = u32::from_le_bytes(bytes[8..12].try_into().unwrap());
    if status != 0 {
        return Err(ProtocolError::DeviceStatus(status));
    }
    if bytes[12..16] != [0; 4] {
        return Err(ProtocolError::NonzeroReserved);
    }
    Ok(&bytes[HEADER..])
}

fn transact<I: ReadIo>(
    io: &mut I,
    command: Command,
    sequence: u16,
    payload: &[u8],
    size: usize,
) -> Result<Vec<u8>, ReadError<I::Error>> {
    let request = packet(command, sequence, payload);
    exact(
        io.send_request(&request).map_err(ReadError::Io)?,
        request.len(),
    )?;
    let deadline = Instant::now() + ACK_TIMEOUT;
    let mut acknowledged = false;
    for _ in 0..MAX_NOTIFICATIONS {
        let timeout = deadline.saturating_duration_since(Instant::now());
        if timeout.is_zero() {
            break;
        }
        let mut notification = [0; 8];
        let n = io
            .next_notification(&mut notification, timeout)
            .map_err(ReadError::Io)?;
        if n != notification.len() {
            return Err(ReadError::Protocol(ProtocolError::InvalidNotification));
        }
        if u32::from_le_bytes(notification[..4].try_into().unwrap()) & 1 != 0 {
            acknowledged = true;
            break;
        }
    }
    if !acknowledged {
        return Err(ReadError::Protocol(ProtocolError::AcknowledgementTimeout));
    }
    let mut reply = vec![0; HEADER + size];
    exact(
        io.receive_response(&mut reply).map_err(ReadError::Io)?,
        reply.len(),
    )?;
    decode_reply(command, sequence, size, &reply)
        .map_err(ReadError::Protocol)
        .map(Vec::from)
}

fn exact<E>(actual: usize, expected: usize) -> Result<(), ReadError<E>> {
    if actual == expected {
        Ok(())
    } else {
        Err(ReadError::Protocol(ProtocolError::InvalidLength {
            expected,
            actual,
        }))
    }
}

/// Stateful, bounded protocol. Transfer/protocol failures poison normal calls;
/// only failed mixer transactions may reinitialize to attempt exact restoration.
pub struct ReadSession<I> {
    io: I,
    model: ReadModel,
    sequence: u16,
    poisoned: bool,
}

impl<I: ReadIo> ReadSession<I> {
    pub fn initialize(mut io: I, candidate: CandidateModel) -> Result<Self, ReadError<I::Error>> {
        let mut startup = [0; 24];
        exact(
            io.initial_read(&mut startup).map_err(ReadError::Io)?,
            startup.len(),
        )?;
        io.prepare_notifications().map_err(ReadError::Io)?;
        transact(&mut io, Command::Init, 1, &[], 0)?;
        // The documented handshake resets sequence to one for its second command.
        let info = transact(&mut io, Command::Info, 1, &[], 84)?;
        let firmware = u32::from_le_bytes(info[8..12].try_into().unwrap());
        let model = candidate
            .validate_firmware(firmware)
            .map_err(ReadError::Protocol)?;
        Ok(Self {
            io,
            model,
            sequence: 2,
            poisoned: false,
        })
    }

    pub fn model(&self) -> ReadModel {
        self.model
    }

    fn request(
        &mut self,
        command: Command,
        payload: &[u8],
        size: usize,
    ) -> Result<Vec<u8>, ReadError<I::Error>> {
        if self.poisoned {
            return Err(ReadError::Protocol(ProtocolError::SessionPoisoned));
        }
        let sequence = self.sequence;
        self.sequence = self.sequence.wrapping_add(1);
        let result = transact(&mut self.io, command, sequence, payload, size);
        if result.is_err() {
            self.poisoned = true;
        }
        result
    }

    pub fn read_mux_zero(&mut self) -> Result<Vec<MuxRoute>, ReadError<I::Error>> {
        let count = self.model.mux_destinations();
        let request = [0, 0, count as u8, 0];
        let bytes = self.request(Command::Mux, &request, count * 4)?;
        let result = decode_mux(self.model, &bytes).map_err(ReadError::Protocol);
        if result.is_err() {
            self.poisoned = true;
        }
        result
    }

    pub fn read_mix(&mut self, output: u8) -> Result<MixReadback, ReadError<I::Error>> {
        if output == 0 || output > self.model.mixer_outputs() {
            return Err(ReadError::Protocol(ProtocolError::InvalidMixOutput(output)));
        }
        let request = [output - 1, 0, self.model.mixer_inputs(), 0];
        let bytes = self.request(
            Command::Mix,
            &request,
            usize::from(self.model.mixer_inputs()) * 2,
        )?;
        Ok(MixReadback {
            output,
            coefficients: bytes
                .chunks_exact(2)
                .map(|b| u16::from_le_bytes([b[0], b[1]]))
                .collect(),
        })
    }

    fn validate_row(&self, row: &MixReadback) -> Result<(), ProtocolError> {
        if row.output == 0 || row.output > self.model.mixer_outputs() {
            return Err(ProtocolError::InvalidMixOutput(row.output));
        }
        let expected = usize::from(self.model.mixer_inputs());
        if row.coefficients.len() != expected {
            return Err(ProtocolError::InvalidLength {
                expected,
                actual: row.coefficients.len(),
            });
        }
        Ok(())
    }

    fn set_mix(&mut self, row: &MixReadback) -> Result<(), ReadError<I::Error>> {
        let mut payload = u16::from(row.output - 1).to_le_bytes().to_vec();
        for value in &row.coefficients {
            payload.extend(value.to_le_bytes());
        }
        self.request(Command::SetMix, &payload, 0).map(|_| ())
    }

    // Recovery must re-establish notification/sequence state rather than reuse a
    // poisoned request stream after an uncertain write or malformed response.
    fn recover(&mut self) -> Result<(), ReadError<I::Error>> {
        let mut startup = [0; 24];
        exact(
            self.io.initial_read(&mut startup).map_err(ReadError::Io)?,
            startup.len(),
        )?;
        self.io.prepare_notifications().map_err(ReadError::Io)?;
        transact(&mut self.io, Command::Init, 1, &[], 0)?;
        let info = transact(&mut self.io, Command::Info, 1, &[], 84)?;
        let firmware = u32::from_le_bytes(info[8..12].try_into().unwrap());
        self.model
            .candidate
            .validate_firmware(firmware)
            .map_err(ReadError::Protocol)?;
        self.sequence = 2;
        self.poisoned = false;
        Ok(())
    }

    /// Fresh-check, set and verify an entire row. A failed attempted SET always
    /// triggers bounded restoration, including when the SET response is lost.
    pub fn apply_mix(
        &mut self,
        edit: &MixRowEdit,
    ) -> Result<MixWriteReport, MixWriteError<I::Error>> {
        let invalid = |error| MixWriteError {
            failure: MixFailure::Invalid(error),
            rollback: None,
        };
        self.validate_row(&edit.expected).map_err(invalid)?;
        let mut target = edit.expected.clone();
        let mut seen = Vec::new();
        for gain in &edit.gains {
            if gain.input == 0 || gain.input > self.model.mixer_inputs() {
                return Err(invalid(ProtocolError::InvalidMixInput(gain.input)));
            }
            if seen.contains(&gain.input) {
                return Err(invalid(ProtocolError::DuplicateMixInput(gain.input)));
            }
            seen.push(gain.input);
            target.coefficients[usize::from(gain.input - 1)] = gain.gain.coefficient();
        }
        self.replace_mix(&edit.expected, &target)
    }

    pub(crate) fn replace_mix(
        &mut self,
        expected: &MixReadback,
        target: &MixReadback,
    ) -> Result<MixWriteReport, MixWriteError<I::Error>> {
        let fail = |failure| MixWriteError {
            failure,
            rollback: None,
        };
        self.validate_row(expected)
            .map_err(|e| fail(MixFailure::Invalid(e)))?;
        self.validate_row(target)
            .map_err(|e| fail(MixFailure::Invalid(e)))?;
        if expected.output != target.output {
            return Err(fail(MixFailure::Invalid(ProtocolError::InvalidMixOutput(
                target.output,
            ))));
        }
        let actual = self
            .read_mix(expected.output)
            .map_err(|e| fail(MixFailure::Read(e)))?;
        if actual != *expected {
            return Err(fail(MixFailure::Drift { actual }));
        }
        if expected == target {
            return Ok(MixWriteReport {
                original: actual.clone(),
                verified: actual,
            });
        }
        let attempt = self
            .set_mix(target)
            .map_err(MixFailure::Read)
            .and_then(|_| {
                let actual = self.read_mix(target.output).map_err(MixFailure::Read)?;
                if actual != *target {
                    return Err(MixFailure::Verification { actual });
                }
                Ok(actual)
            });
        match attempt {
            Ok(verified) => Ok(MixWriteReport {
                original: expected.clone(),
                verified,
            }),
            Err(failure) => {
                let rollback = (|| {
                    if self.poisoned {
                        self.recover().map_err(MixFailure::Read)?;
                    }
                    self.set_mix(expected).map_err(MixFailure::Read)?;
                    let actual = self.read_mix(expected.output).map_err(MixFailure::Read)?;
                    if actual != *expected {
                        return Err(MixFailure::Verification { actual });
                    }
                    Ok(())
                })()
                .err();
                if rollback.is_some() {
                    self.poisoned = true;
                }
                Err(MixWriteError { failure, rollback })
            }
        }
    }

    /// Probe FCP's info/read contract on this exact old firmware. Failure does
    /// not establish write capability and does not alter legacy mux-zero/mix APIs.
    pub fn read_mux_banks(&mut self) -> Result<MuxSnapshot, ReadError<I::Error>> {
        let info = self.request(Command::MuxInfo, &[], 12)?;
        let widths: Vec<_> = info[..6].chunks_exact(2)
            .map(|v| u16::from_le_bytes([v[0], v[1]])).collect();
        if widths != [64, 56, 46] {
            return Err(ReadError::Protocol(ProtocolError::UnsupportedMuxGeometry));
        }
        let mut banks = Vec::new();
        for bank in 0..3 { banks.push(self.read_mux_bank(bank)?); }
        Ok(MuxSnapshot { model: self.model, banks })
    }

    fn read_mux_bank(&mut self, bank: u8) -> Result<MuxBankSnapshot, ReadError<I::Error>> {
        let width = *[64u8, 56, 46].get(usize::from(bank))
            .ok_or(ReadError::Protocol(ProtocolError::InvalidMuxBank(bank)))?;
        let bytes = self.request(Command::Mux, &[0, 0, width, bank], usize::from(width) * 4)?;
        let snapshot = MuxBankSnapshot { bank, words: bytes.chunks_exact(4)
            .map(|v| u32::from_le_bytes(v.try_into().unwrap())).collect() };
        validate_bank(self.model, &snapshot).map_err(ReadError::Protocol)?;
        Ok(snapshot)
    }

    fn set_mux_bank(&mut self, bank: &MuxBankSnapshot) -> Result<(), ReadError<I::Error>> {
        let mut bytes = vec![0, 0, bank.bank, 0];
        for word in &bank.words { bytes.extend(word.to_le_bytes()); }
        self.request(Command::SetMux, &bytes, 0).map(|_| ())
    }

    pub fn apply_mux(&mut self, edit: &MuxEdit) -> Result<MuxWriteReport, MuxWriteError<I::Error>> {
        let invalid = |e| MuxWriteError { failure: MuxFailure::Invalid(e), rollback: Vec::new() };
        validate_snapshot(self.model, &edit.expected).map_err(invalid)?;
        let mut target = edit.expected.clone();
        let mut seen = Vec::new();
        for route in &edit.routes {
            if seen.contains(&(route.bank, route.destination)) {
                return Err(invalid(ProtocolError::InvalidMuxEdit));
            }
            seen.push((route.bank, route.destination));
            let source = port_id(self.model, route.source, true).map_err(invalid)?;
            let destination = port_id(self.model, route.destination, false).map_err(invalid)?;
            let bank = target.banks.get_mut(usize::from(route.bank))
                .ok_or_else(|| invalid(ProtocolError::InvalidMuxBank(route.bank)))?;
            let word = bank.words.iter_mut().find(|word| **word != 0 && (**word & 0xfff) == u32::from(destination))
                .ok_or_else(|| invalid(ProtocolError::InvalidMuxEdit))?;
            *word = u32::from(destination) | (u32::from(source) << 12);
        }
        self.replace_mux(&edit.expected, &target)
    }

    pub(crate) fn replace_mux(&mut self, expected: &MuxSnapshot, target: &MuxSnapshot)
        -> Result<MuxWriteReport, MuxWriteError<I::Error>> {
        let fail = |failure| MuxWriteError { failure, rollback: Vec::new() };
        validate_snapshot(self.model, expected).map_err(|e| fail(MuxFailure::Invalid(e)))?;
        validate_snapshot(self.model, target).map_err(|e| fail(MuxFailure::Invalid(e)))?;
        // Includes info and every bank, so no write can rely on selector zero or
        // a snapshot captured before another controller changed the graph.
        let actual = self.read_mux_banks().map_err(|e| fail(MuxFailure::Read(e)))?;
        if actual != *expected { return Err(fail(MuxFailure::Drift { actual })); }
        let mut attempted = Vec::new();
        let mut verified = actual.clone();
        for (old, new) in expected.banks.iter().zip(&target.banks) {
            if old == new { continue; }
            attempted.push(old);
            let result = self.set_mux_bank(new).map_err(MuxFailure::Read).and_then(|_| {
                let actual = self.read_mux_bank(new.bank).map_err(MuxFailure::Read)?;
                if actual != *new { return Err(MuxFailure::Verification { bank: new.bank, actual }); }
                Ok(actual)
            });
            match result {
                Ok(bank) => { let index = usize::from(bank.bank); verified.banks[index] = bank; },
                Err(failure) => {
                    let mut rollback = Vec::new();
                    for original in attempted.iter().rev() {
                        let restore = (|| {
                            if self.poisoned { self.recover().map_err(MuxFailure::Read)?; }
                            self.set_mux_bank(original).map_err(MuxFailure::Read)?;
                            let actual = self.read_mux_bank(original.bank).map_err(MuxFailure::Read)?;
                            if actual != **original {
                                return Err(MuxFailure::Verification { bank: original.bank, actual });
                            }
                            Ok(())
                        })();
                        if let Err(error) = restore { rollback.push((original.bank, error)); }
                    }
                    if !rollback.is_empty() { self.poisoned = true; }
                    return Err(MuxWriteError { failure, rollback });
                }
            }
        }
        Ok(MuxWriteReport { original: actual, verified })
    }

    pub fn read_all(&mut self) -> Result<RoutingReadback, ReadError<I::Error>> {
        let mux_zero = self.read_mux_zero()?;
        let mixes = (1..=self.model.mixer_outputs())
            .map(|output| self.read_mix(output))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(RoutingReadback {
            model: self.model,
            mux_zero,
            mixes,
        })
    }
}

// Independent geometry contract from the pinned ALSA model: destination
// groups and trailing empty slots, never inferred from a successful zero read.
fn bank_destinations(bank: u8) -> Result<Vec<u16>, ProtocolError> {
    let (pcm, adat, padding) = match bank {
        0 => (18, 8, 8), 1 => (14, 4, 8), 2 => (10, 0, 6),
        _ => return Err(ProtocolError::InvalidMuxBank(bank)),
    };
    let mut ids = Vec::new();
    for (base, count) in [(0x600, pcm), (0x080, 10), (0x180, 2), (0x200, adat), (0x300, 18)] {
        ids.extend((0..count).map(|channel| base + channel));
    }
    ids.extend(std::iter::repeat(0).take(padding));
    Ok(ids)
}
fn validate_bank(model: ReadModel, bank: &MuxBankSnapshot) -> Result<(), ProtocolError> {
    let destinations = bank_destinations(bank.bank)?;
    if destinations.len() != bank.words.len() { return Err(ProtocolError::UnsupportedMuxGeometry); }
    for (&destination, &word) in destinations.iter().zip(&bank.words) {
        if destination == 0 {
            if word != 0 { return Err(ProtocolError::UnsupportedMuxGeometry); }
        } else {
            if word >> 24 != 0 || word & 0xfff != u32::from(destination) {
                return Err(ProtocolError::UnsupportedMuxGeometry);
            }
            model.port(destination, false)?;
            model.port(((word >> 12) & 0xfff) as u16, true)?;
        }
    }
    Ok(())
}
fn validate_snapshot(model: ReadModel, snapshot: &MuxSnapshot) -> Result<(), ProtocolError> {
    if snapshot.model != model || snapshot.banks.len() != 3 {
        return Err(ProtocolError::UnsupportedMuxGeometry);
    }
    for (index, bank) in snapshot.banks.iter().enumerate() {
        if usize::from(bank.bank) != index { return Err(ProtocolError::UnsupportedMuxGeometry); }
        validate_bank(model, bank)?;
    }
    Ok(())
}
fn port_id(model: ReadModel, port: Port, source: bool) -> Result<u16, ProtocolError> {
    if port == Port::Off && source { return Ok(0); }
    if let Port::Channel { kind, channel } = port {
        let base = match kind {
            PortKind::Analogue => 0x080, PortKind::Spdif => 0x180,
            PortKind::Adat => 0x200, PortKind::Mix => 0x300, PortKind::Pcm => 0x600,
            PortKind::Off => return Err(ProtocolError::InvalidMuxEdit),
        };
        if channel != 0 && channel <= model.count(kind, source) {
            return Ok(base + u16::from(channel - 1));
        }
    }
    Err(ProtocolError::InvalidMuxEdit)
}

pub fn decode_mux(model: ReadModel, bytes: &[u8]) -> Result<Vec<MuxRoute>, ProtocolError> {
    let expected = model.mux_destinations() * 4;
    if bytes.len() != expected {
        return Err(ProtocolError::InvalidLength {
            expected,
            actual: bytes.len(),
        });
    }
    let mut routes = Vec::with_capacity(model.mux_destinations());
    for part in bytes.chunks_exact(4) {
        let word = u32::from_le_bytes(part.try_into().unwrap());
        if word >> 24 != 0 {
            return Err(ProtocolError::InvalidMuxBits);
        }
        let source = model.port(((word >> 12) & 0xfff) as u16, true)?;
        let destination = model.port((word & 0xfff) as u16, false)?;
        if routes
            .iter()
            .any(|route: &MuxRoute| route.destination == destination)
        {
            return Err(ProtocolError::DuplicateDestination);
        }
        routes.push(MuxRoute {
            source,
            destination,
        });
    }
    Ok(routes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model() -> ReadModel {
        CandidateModel::Scarlett18i20Gen2
            .validate_firmware(1083)
            .unwrap()
    }

    fn mux_bytes() -> Vec<u8> {
        let mut bytes = Vec::new();
        for (base, count) in [
            (0x080u32, 10),
            (0x180, 2),
            (0x200, 8),
            (0x300, 18),
            (0x600, 18),
        ] {
            for channel in 0..count {
                bytes.extend((base + channel).to_le_bytes());
            }
        }
        bytes
    }

    #[test]
    fn identity_and_firmware_guards_do_not_match_sibling_products() {
        assert_eq!(
            CandidateModel::recognize(0x1235, 0x8201),
            Some(CandidateModel::Scarlett18i20Gen2)
        );
        for (vendor, product) in [
            (0x1234, 0x8201),
            (0x1235, 0x8203),
            (0x1235, 0x8215),
            (0x1235, 0x8218),
        ] {
            assert_eq!(CandidateModel::recognize(vendor, product), None);
        }
        for firmware in [0, 1082, 1084, u32::MAX] {
            assert_eq!(
                CandidateModel::Scarlett18i20Gen2.validate_firmware(firmware),
                Err(ProtocolError::UnsupportedFirmware(firmware))
            );
        }
        assert_eq!(model().mixer_inputs(), 18);
        assert_eq!(model().mixer_outputs(), 10);
    }

    #[test]
    fn envelope_uses_exact_little_endian_header_and_payload_length() {
        assert_eq!(
            packet(Command::Mix, 0x1234, &[9, 0, 18, 0]),
            vec![1, 32, 0, 0, 4, 0, 0x34, 0x12, 0, 0, 0, 0, 0, 0, 0, 0, 9, 0, 18, 0]
        );
        let reply = packet(Command::Mix, 0x1234, &[1, 2]);
        assert_eq!(
            decode_reply(Command::Mix, 0x1234, 2, &reply),
            Ok(&[1, 2][..])
        );
    }

    #[test]
    fn reply_rejects_command_sequence_status_padding_and_short_or_extra_bytes() {
        let good = packet(Command::Mix, 7, &[0, 1]);
        for (offset, value, expected) in [
            (0, 2, ProtocolError::InvalidCommand),
            (4, 1, ProtocolError::InvalidPayloadSize),
            (6, 8, ProtocolError::InvalidSequence),
            (8, 5, ProtocolError::DeviceStatus(5)),
            (12, 1, ProtocolError::NonzeroReserved),
        ] {
            let mut bad = good.clone();
            bad[offset] = value;
            assert_eq!(decode_reply(Command::Mix, 7, 2, &bad), Err(expected));
        }
        for len in [0, 15, 17, 19] {
            let mut bad = good.clone();
            bad.resize(len, 0);
            assert!(matches!(
                decode_reply(Command::Mix, 7, 2, &bad),
                Err(ProtocolError::InvalidLength { .. })
            ));
        }
        assert!(decode_reply(Command::Init, 1, 0, &packet(Command::Init, 0, &[])).is_ok());
        assert!(decode_reply(Command::Info, 1, 84, &packet(Command::Info, 0, &[0; 84])).is_ok());
        assert_eq!(
            decode_reply(Command::Mux, 1, 0, &packet(Command::Mux, 0, &[])),
            Err(ProtocolError::InvalidSequence)
        );
    }

    #[test]
    fn mux_decodes_typed_ports_and_rejects_unknown_duplicate_and_reserved_bits() {
        let bytes = mux_bytes();
        let routes = decode_mux(model(), &bytes).unwrap();
        assert_eq!(routes.len(), 56);
        assert_eq!(
            routes[0],
            MuxRoute {
                source: Port::Off,
                destination: Port::Channel {
                    kind: PortKind::Analogue,
                    channel: 1
                }
            }
        );
        assert_eq!(
            routes[55].destination,
            Port::Channel {
                kind: PortKind::Pcm,
                channel: 18
            }
        );
        let mut source = bytes.clone();
        source[..4].copy_from_slice(&(0x613u32 << 12 | 0x080).to_le_bytes());
        assert_eq!(
            decode_mux(model(), &source).unwrap()[0].source,
            Port::Channel {
                kind: PortKind::Pcm,
                channel: 20
            }
        );
        source[..4].copy_from_slice(&(0x614u32 << 12 | 0x080).to_le_bytes());
        assert!(matches!(
            decode_mux(model(), &source),
            Err(ProtocolError::UnknownPort { source: true, .. })
        ));
        let mut duplicate = bytes.clone();
        duplicate[4..8].copy_from_slice(&bytes[..4]);
        assert_eq!(
            decode_mux(model(), &duplicate),
            Err(ProtocolError::DuplicateDestination)
        );
        let mut reserved = bytes.clone();
        reserved[3] = 1;
        assert_eq!(
            decode_mux(model(), &reserved),
            Err(ProtocolError::InvalidMuxBits)
        );
        let mut unknown = bytes.clone();
        unknown[..4].copy_from_slice(&0u32.to_le_bytes());
        assert!(matches!(
            decode_mux(model(), &unknown),
            Err(ProtocolError::UnknownPort { source: false, .. })
        ));
    }

    struct MockIo {
        sent: Vec<Vec<u8>>,
        last: Vec<u8>,
        firmware: u32,
        notifications: usize,
        ack: bool,
        wrong_sequence: bool,
        short_send: bool,
        prepared: bool,
        row: Vec<u16>,
        writes: usize,
        fail_writes: Vec<usize>,
        corrupt_writes: Vec<usize>,
    }
    impl Default for MockIo {
        fn default() -> Self {
            Self {
                sent: Vec::new(),
                last: Vec::new(),
                firmware: 1083,
                notifications: 0,
                ack: true,
                wrong_sequence: false,
                short_send: false,
                prepared: false,
                row: vec![0x1234; 18],
                writes: 0,
                fail_writes: Vec::new(),
                corrupt_writes: Vec::new(),
            }
        }
    }
    impl ReadIo for MockIo {
        type Error = &'static str;
        fn initial_read(&mut self, _: &mut [u8; 24]) -> Result<usize, Self::Error> {
            Ok(24)
        }
        fn prepare_notifications(&mut self) -> Result<(), Self::Error> {
            self.prepared = true;
            Ok(())
        }
        fn send_request(&mut self, bytes: &[u8]) -> Result<usize, Self::Error> {
            assert!(self.prepared);
            self.sent.push(bytes.to_vec());
            self.last = bytes.to_vec();
            if u32::from_le_bytes(bytes[..4].try_into().unwrap()) == 0x2002 {
                self.writes += 1;
                self.row = bytes[HEADER + 2..]
                    .chunks_exact(2)
                    .map(|part| u16::from_le_bytes([part[0], part[1]]))
                    .collect();
                if self.corrupt_writes.contains(&self.writes) {
                    self.row[17] ^= 1;
                }
                if self.fail_writes.contains(&self.writes) {
                    return Err("lost SET response");
                }
            }
            Ok(if self.short_send { 0 } else { bytes.len() })
        }
        fn next_notification(
            &mut self,
            bytes: &mut [u8; 8],
            timeout: Duration,
        ) -> Result<usize, Self::Error> {
            assert!(timeout <= ACK_TIMEOUT && !timeout.is_zero());
            self.notifications += 1;
            bytes[0] = if self.ack { 1 } else { 2 };
            Ok(8)
        }
        fn receive_response(&mut self, bytes: &mut [u8]) -> Result<usize, Self::Error> {
            bytes[..HEADER].copy_from_slice(&self.last[..HEADER]);
            let command = u32::from_le_bytes(bytes[..4].try_into().unwrap());
            let payload_size = (bytes.len() - HEADER) as u16;
            bytes[4..6].copy_from_slice(&payload_size.to_le_bytes());
            match command {
                0 => bytes[6..8].copy_from_slice(&0u16.to_le_bytes()),
                2 => {
                    bytes[6..8].copy_from_slice(&0u16.to_le_bytes());
                    bytes[24..28].copy_from_slice(&self.firmware.to_le_bytes());
                }
                0x3001 => bytes[HEADER..].copy_from_slice(&mux_bytes()),
                0x2001 => {
                    for (part, value) in bytes[HEADER..].chunks_exact_mut(2).zip(&self.row) {
                        part.copy_from_slice(&value.to_le_bytes());
                    }
                }
                0x2002 => {}
                _ => panic!("unexpected command"),
            }
            if self.wrong_sequence {
                bytes[6] = 255;
            }
            Ok(bytes.len())
        }
    }

    #[test]
    fn handshake_resets_sequence_then_reads_mux_and_mix_without_set_commands() {
        let mut session =
            ReadSession::initialize(MockIo::default(), CandidateModel::Scarlett18i20Gen2).unwrap();
        let result = session.read_all().unwrap();
        assert_eq!(result.mixes.len(), 10);
        assert_eq!(result.mixes[0].coefficients, vec![0x1234; 18]);
        assert_eq!(session.io.sent.len(), 13);
        let commands = session
            .io
            .sent
            .iter()
            .map(|packet| u32::from_le_bytes(packet[..4].try_into().unwrap()))
            .collect::<Vec<_>>();
        assert_eq!(&commands[..3], &[0, 2, 0x3001]);
        assert!(commands[3..].iter().all(|&code| code == 0x2001));
        let sequences = session
            .io
            .sent
            .iter()
            .map(|packet| u16::from_le_bytes(packet[6..8].try_into().unwrap()))
            .collect::<Vec<_>>();
        assert_eq!(&sequences[..4], &[1, 1, 2, 3]);
        assert_eq!(&session.io.sent[2][HEADER..], &[0, 0, 56, 0]);
        assert_eq!(&session.io.sent[12][HEADER..], &[9, 0, 18, 0]);
    }

    #[test]
    fn unsupported_firmware_stops_before_any_routing_query() {
        let result = ReadSession::initialize(
            MockIo {
                firmware: 1084,
                ..MockIo::default()
            },
            CandidateModel::Scarlett18i20Gen2,
        );
        assert!(matches!(
            result,
            Err(ReadError::Protocol(ProtocolError::UnsupportedFirmware(
                1084
            )))
        ));
    }

    #[test]
    fn ack_budget_invalid_mix_and_poisoned_sequence_are_bounded() {
        let mut io = MockIo {
            ack: false,
            ..MockIo::default()
        };
        io.prepare_notifications().unwrap();
        assert_eq!(
            transact(&mut io, Command::Init, 1, &[], 0),
            Err(ReadError::Protocol(ProtocolError::AcknowledgementTimeout))
        );
        assert_eq!(io.notifications, MAX_NOTIFICATIONS);
        let mut session =
            ReadSession::initialize(MockIo::default(), CandidateModel::Scarlett18i20Gen2).unwrap();
        for output in [0, 11, 255] {
            assert_eq!(
                session.read_mix(output),
                Err(ReadError::Protocol(ProtocolError::InvalidMixOutput(output)))
            );
        }
        assert_eq!(session.io.sent.len(), 2);
        session.io.wrong_sequence = true;
        assert_eq!(
            session.read_mix(1),
            Err(ReadError::Protocol(ProtocolError::InvalidSequence))
        );
        let sent = session.io.sent.len();
        assert_eq!(
            session.read_mix(1),
            Err(ReadError::Protocol(ProtocolError::SessionPoisoned))
        );
        assert_eq!(session.io.sent.len(), sent);
    }

    #[test]
    fn short_out_transfer_does_not_wait_for_ack_or_attempt_response() {
        let mut io = MockIo {
            short_send: true,
            ..MockIo::default()
        };
        io.prepare_notifications().unwrap();
        assert!(matches!(
            transact(&mut io, Command::Init, 1, &[], 0),
            Err(ReadError::Protocol(ProtocolError::InvalidLength { .. }))
        ));
        assert_eq!(io.notifications, 0);
    }
    fn row_edit() -> MixRowEdit {
        MixRowEdit {
            expected: MixReadback {
                output: 2,
                coefficients: vec![0x1234; 18],
            },
            gains: vec![MixGainEdit {
                input: 3,
                gain: MixGain::from_half_db(-12).unwrap(),
            }],
        }
    }

    #[test]
    fn bounded_half_db_formula_and_whole_row_preservation() {
        assert!(MixGain::from_half_db(-161).is_err());
        assert!(MixGain::from_half_db(25).is_err());
        assert_eq!(MixGain::from_half_db(-160).unwrap().coefficient(), 0);
        assert_eq!(MixGain::from_half_db(0).unwrap().coefficient(), 8192);
        assert_eq!(MixGain::from_half_db(24).unwrap().coefficient(), 32612);
        let mut edit = row_edit();
        edit.expected.coefficients[17] = u16::MAX;
        let mut session = ReadSession::initialize(
            MockIo {
                row: edit.expected.coefficients.clone(),
                ..MockIo::default()
            },
            CandidateModel::Scarlett18i20Gen2,
        )
        .unwrap();
        let report = session.apply_mix(&edit).unwrap();
        assert_eq!(report.verified.coefficients[17], u16::MAX);
        assert_eq!(
            report.verified.coefficients[2],
            edit.gains[0].gain.coefficient()
        );
        let set = session
            .io
            .sent
            .iter()
            .find(|p| p[..4] == 0x2002u32.to_le_bytes())
            .unwrap();
        assert_eq!(set.len(), HEADER + 38);
        assert_eq!(&set[HEADER..HEADER + 2], &[1, 0]);
        assert_eq!(session.io.writes, 1);
    }

    #[test]
    fn stale_and_invalid_rows_never_write() {
        let mut session =
            ReadSession::initialize(MockIo::default(), CandidateModel::Scarlett18i20Gen2).unwrap();
        let mut edit = row_edit();
        edit.expected.coefficients[0] ^= 1;
        assert!(matches!(
            session.apply_mix(&edit).unwrap_err().failure,
            MixFailure::Drift { .. }
        ));
        edit.expected.coefficients.pop();
        assert!(matches!(
            session.apply_mix(&edit).unwrap_err().failure,
            MixFailure::Invalid(ProtocolError::InvalidLength { .. })
        ));
        edit = row_edit();
        edit.gains.push(edit.gains[0]);
        assert!(matches!(
            session.apply_mix(&edit).unwrap_err().failure,
            MixFailure::Invalid(ProtocolError::DuplicateMixInput(3))
        ));
        assert_eq!(session.io.writes, 0);
    }

    #[test]
    fn failed_write_and_bad_readback_restore_exact_original() {
        for io in [
            MockIo {
                fail_writes: vec![1],
                ..MockIo::default()
            },
            MockIo {
                corrupt_writes: vec![1],
                ..MockIo::default()
            },
        ] {
            let mut session =
                ReadSession::initialize(io, CandidateModel::Scarlett18i20Gen2).unwrap();
            let error = session.apply_mix(&row_edit()).unwrap_err();
            assert!(error.rollback.is_none());
            assert_eq!(session.io.row, row_edit().expected.coefficients);
            assert_eq!(session.io.writes, 2);
        }
    }

    #[test]
    fn rollback_uncertainty_retains_original_failure_and_blocks_reuse() {
        let io = MockIo {
            fail_writes: vec![1, 2],
            ..MockIo::default()
        };
        let mut session = ReadSession::initialize(io, CandidateModel::Scarlett18i20Gen2).unwrap();
        let error = session.apply_mix(&row_edit()).unwrap_err();
        assert_eq!(
            error.failure,
            MixFailure::Read(ReadError::Io("lost SET response"))
        );
        assert_eq!(
            error.rollback,
            Some(MixFailure::Read(ReadError::Io("lost SET response")))
        );
        assert_eq!(
            session.read_mix(2),
            Err(ReadError::Protocol(ProtocolError::SessionPoisoned))
        );
    }

    #[test]
    fn hand_back_row_check_preserves_external_coefficient_drift() {
        let mut session =
            ReadSession::initialize(MockIo::default(), CandidateModel::Scarlett18i20Gen2).unwrap();
        let report = session.apply_mix(&row_edit()).unwrap();
        session.io.row[17] ^= 1;
        assert!(matches!(
            session
                .replace_mix(&report.verified, &report.original)
                .unwrap_err()
                .failure,
            MixFailure::Drift { .. }
        ));
        assert_eq!(session.io.writes, 1);
    }
    struct BankIo {
        base: MockIo,
        banks: Vec<MuxBankSnapshot>,
        writes: Vec<u8>,
        lost: Vec<usize>,
        corrupt: Vec<usize>,
        unsupported: bool,
        bad_width: bool,
    }
    impl Default for BankIo {
        fn default() -> Self {
            Self {
                base: MockIo::default(),
                banks: (0..3).map(|bank| MuxBankSnapshot {
                    bank, words: bank_destinations(bank).unwrap().into_iter().map(u32::from).collect(),
                }).collect(),
                writes: Vec::new(), lost: Vec::new(), corrupt: Vec::new(),
                unsupported: false, bad_width: false,
            }
        }
    }
    impl ReadIo for BankIo {
        type Error = &'static str;
        fn initial_read(&mut self, bytes: &mut [u8; 24]) -> Result<usize, Self::Error> {
            self.base.initial_read(bytes)
        }
        fn prepare_notifications(&mut self) -> Result<(), Self::Error> {
            self.base.prepare_notifications()
        }
        fn send_request(&mut self, bytes: &[u8]) -> Result<usize, Self::Error> {
            let n = self.base.send_request(bytes)?;
            if bytes[..4] == 0x3002u32.to_le_bytes() {
                let bank = bytes[HEADER + 2];
                self.writes.push(bank);
                self.banks[usize::from(bank)].words = bytes[HEADER + 4..].chunks_exact(4)
                    .map(|word| u32::from_le_bytes(word.try_into().unwrap())).collect();
                if self.corrupt.contains(&self.writes.len()) {
                    self.banks[usize::from(bank)].words[0] ^= 1 << 12;
                }
            }
            Ok(n)
        }
        fn next_notification(&mut self, bytes: &mut [u8; 8], timeout: Duration) -> Result<usize, Self::Error> {
            self.base.next_notification(bytes, timeout)
        }
        fn receive_response(&mut self, bytes: &mut [u8]) -> Result<usize, Self::Error> {
            let command = u32::from_le_bytes(self.base.last[..4].try_into().unwrap());
            if command != 0x3000 && command != 0x3001 && command != 0x3002 {
                return self.base.receive_response(bytes);
            }
            if command == 0x3002 && self.lost.contains(&self.writes.len()) {
                return Err("lost mux response");
            }
            bytes[..HEADER].copy_from_slice(&self.base.last[..HEADER]);
            let payload_size = (bytes.len() - HEADER) as u16;
            bytes[4..6].copy_from_slice(&payload_size.to_le_bytes());
            match command {
                0x3000 => {
                    for (part, width) in bytes[HEADER..].chunks_exact_mut(2).zip([64u16, 56, 46, 0, 0, 0]) {
                        part.copy_from_slice(&width.to_le_bytes());
                    }
                    if self.bad_width { bytes[HEADER] = 63; }
                    if self.unsupported { bytes[8..12].copy_from_slice(&1u32.to_le_bytes()); }
                }
                0x3001 => {
                    let bank = self.base.last[HEADER + 3];
                    assert_eq!(self.base.last[HEADER], 0);
                    assert_eq!(self.base.last[HEADER + 1], 0);
                    assert_eq!(usize::from(self.base.last[HEADER + 2]) * 4, bytes.len() - HEADER);
                    for (part, word) in bytes[HEADER..].chunks_exact_mut(4).zip(&self.banks[usize::from(bank)].words) {
                        part.copy_from_slice(&word.to_le_bytes());
                    }
                }
                0x3002 => {},
                _ => unreachable!(),
            }
            Ok(bytes.len())
        }
    }
    fn bank_session(io: BankIo) -> ReadSession<BankIo> {
        ReadSession::initialize(io, CandidateModel::Scarlett18i20Gen2).unwrap()
    }
    fn bank_edit(session: &mut ReadSession<BankIo>) -> MuxEdit {
        MuxEdit {
            expected: session.read_mux_banks().unwrap(),
            routes: vec![0, 1].into_iter().map(|bank| MuxRouteEdit {
                bank, destination: Port::Channel { kind: PortKind::Analogue, channel: 1 },
                source: Port::Channel { kind: PortKind::Pcm, channel: 2 },
            }).collect(),
        }
    }
    #[test]
    fn mux_probe_requires_exact_complete_geometry_and_preserves_raw_banks() {
        for io in [BankIo { unsupported: true, ..BankIo::default() }, BankIo { bad_width: true, ..BankIo::default() }] {
            let mut session = bank_session(io);
            assert!(session.read_mux_banks().is_err());
            assert!(session.io.writes.is_empty());
        }
        let mut session = bank_session(BankIo::default());
        let edit = bank_edit(&mut session);
        let report = session.apply_mux(&edit).unwrap();
        assert_eq!(session.io.writes, vec![0, 1]);
        assert_eq!(report.original, edit.expected);
        assert_eq!(report.verified.banks[2], edit.expected.banks[2]);
        assert_eq!(&report.verified.banks[0].raw_words()[56..], &[0; 8]);
        let mut restored = session.replace_mux(&report.verified, &report.original).unwrap();
        assert_eq!(restored.verified, edit.expected);
        restored.verified.banks.swap(0, 1);
        assert!(validate_snapshot(model(), &restored.verified).is_err());
    }
    #[test]
    fn mux_drift_invalid_destination_and_padding_never_write() {
        let mut session = bank_session(BankIo::default());
        let mut edit = bank_edit(&mut session);
        let source = port_id(model(), Port::Channel { kind: PortKind::Pcm, channel: 2 }, true).unwrap();
        session.io.banks[2].words[0] |= u32::from(source) << 12;
        assert!(matches!(session.apply_mux(&edit).unwrap_err().failure, MuxFailure::Drift { .. }));
        edit.routes[0].destination = Port::Channel { kind: PortKind::Adat, channel: 8 };
        edit.routes[0].bank = 2;
        assert!(matches!(session.apply_mux(&edit).unwrap_err().failure, MuxFailure::Invalid(_)));
        edit.expected.banks[0].words[63] = 1;
        assert!(matches!(session.apply_mux(&edit).unwrap_err().failure, MuxFailure::Invalid(_)));
        assert!(session.io.writes.is_empty());
    }
    #[test]
    fn mux_partial_lost_set_restores_each_attempted_bank_and_retains_failures() {
        for (lost, corrupt) in [(vec![2], vec![]), (vec![], vec![2])] {
            let mut session = bank_session(BankIo { lost, corrupt, ..BankIo::default() });
            let edit = bank_edit(&mut session);
            let error = session.apply_mux(&edit).unwrap_err();
            assert!(error.rollback.is_empty());
            assert_eq!(session.io.writes, vec![0, 1, 1, 0]);
            assert_eq!(session.io.banks, edit.expected.banks);
        }
        let mut session = bank_session(BankIo { lost: vec![2, 3], ..BankIo::default() });
        let edit = bank_edit(&mut session);
        let error = session.apply_mux(&edit).unwrap_err();
        assert_eq!(error.rollback.len(), 1);
        assert_eq!(error.rollback[0].0, 1);
        assert_eq!(session.io.writes, vec![0, 1, 1, 0]);
        assert!(session.poisoned);
    }

}
