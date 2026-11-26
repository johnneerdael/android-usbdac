//! Read-only Scarlett 18i20 Gen 2 protocol primitives, independently implemented
//! from Linux v7.0 `mixer_scarlett2.c` protocol evidence. The initial trace
//! described there identifies firmware 1083. No other firmware is admitted.
//!
//! Model ports and packet decoding are source-backed; physical routing has not
//! been validated on Scarlett hardware. GET_MUX uses only its documented zero
//! argument here; its result does not establish all three sample-rate banks.
//! Coefficients remain unsigned wire values, without a guessed dB conversion.

use std::time::{Duration, Instant};

const HEADER: usize = 16;
pub(crate) const MAX_PAYLOAD: usize = 224;
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
    InvalidLength { expected: usize, actual: usize },
    InvalidCommand,
    InvalidSequence,
    InvalidPayloadSize,
    DeviceStatus(u32),
    NonzeroReserved,
    UnknownPort { id: u16, source: bool },
    DuplicateDestination,
    InvalidMuxBits,
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
}
impl Command {
    fn code(self) -> u32 {
        match self {
            Self::Init => 0,
            Self::Info => 2,
            Self::Mux => 0x3001,
            Self::Mix => 0x2001,
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

/// Stateful, bounded read protocol. Any transfer/protocol failure poisons the
/// session; there are no automatic retries of commands with uncertain results.
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
                    for part in bytes[HEADER..].chunks_exact_mut(2) {
                        part.copy_from_slice(&0x1234u16.to_le_bytes());
                    }
                }
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
}
