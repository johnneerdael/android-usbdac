//! Read-only FCP topology, described by firmware rather than legacy port tables.
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
    if !candidate.is_fcp() {
        return Err(ReadError::Protocol(ProtocolError::UnsupportedProtocol));
    }
    let mut startup = [0; 24];
    exact(
        io.initial_read(&mut startup).map_err(ReadError::Io)?,
        startup.len(),
    )?;
    io.prepare_notifications().map_err(ReadError::Io)?;
    transact_mode(&mut io, Command::Init, 0, &[], 0, true)?;
    let info = transact_mode(&mut io, Command::Info, 1, &[], 84, true)?;
    let firmware = u32::from_le_bytes(info[8..12].try_into().unwrap());
    if firmware == 0 {
        return Err(ReadError::Protocol(ProtocolError::UnsupportedFirmware(0)));
    }
    let mut sequence = 2u16;
    let mut read = |command, payload: &[u8], size| {
        let result = transact_mode(&mut io, command, sequence, payload, size, true);
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
    Ok(FcpReadback {
        model: candidate,
        firmware,
        mixer_inputs,
        mixer_outputs,
        sources,
        destinations,
        banks,
        mixes,
    })
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
