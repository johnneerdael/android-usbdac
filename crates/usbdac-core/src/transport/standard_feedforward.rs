//! Standard UAC2 adaptive capture's explicit host-to-device rate endpoint.
use super::adc3::FeedforwardEndpoint;
use super::endpoint_geometry::ss_feedback_geometry;
use super::service_interval_micros;
use rusb::Speed;

/// `descriptors` must contain the descriptors of one AS alternate, excluding
/// its interface header. Return the feedback descriptor's index in that slice.
/// UAC1's nine-byte descriptors and bRefresh scheduling require a separate path.
pub(super) fn find_standard_capture_feedforward(
    protocol: u8,
    data: &[u8],
    descriptors: &[&[u8]],
    speed: Speed,
) -> Result<(FeedforwardEndpoint, usize), String> {
    if protocol != 0x20 {
        return Err("standard capture feedforward requires UAC2".into());
    }
    if !matches!(speed, Speed::Full | Speed::High | Speed::Super | Speed::SuperPlus) {
        return Err("UAC2 capture feedforward requires a recognized isochronous USB speed".into());
    }
    // ADC2 4.10.1/4.10.2 use exactly seven-byte standard endpoint descriptors;
    // an ADC1 bRefresh/bSynchAddress tail must never be silently interpreted.
    if !standard_endpoint(data) || data[2] & 0x80 == 0 || data[3] != 0x09 {
        return Err("UAC2 feedforward requires an Adaptive/Data isochronous IN endpoint".into());
    }
    let packet = u16::from_le_bytes([data[4], data[5]]);
    let base = packet & 0x07ff;
    let valid_packet = match speed {
        Speed::Full => packet == base && (1..=1023).contains(&base),
        Speed::High => packet & 0xe000 == 0 && packet & 0x1800 != 0x1800
            && (1..=1024).contains(&base),
        Speed::Super | Speed::SuperPlus => (1..=1024).contains(&packet),
        _ => false,
    };
    if !valid_packet {
        return Err("UAC2 capture data endpoint has invalid packet capacity".into());
    }
    let mut endpoints = Vec::new();
    let mut data_occurrences = 0;
    for (index, descriptor) in descriptors.iter().copied().enumerate() {
        if descriptor.len() < 2 || usize::from(descriptor[0]) != descriptor.len()
            || matches!(descriptor[1], 2 | 4)
        {
            return Err("UAC2 feedforward requires a well-formed single-alternate descriptor slice".into());
        }
        if descriptor[1] == 5 {
            endpoints.push((index, descriptor));
            if descriptor == data { data_occurrences += 1; }
        }
    }
    if data_occurrences != 1 || endpoints.len() != 2 {
        return Err("UAC2 feedforward requires exactly one matching data endpoint and two same-alternate endpoints".into());
    }
    let (index, report) = endpoints.iter().copied().find(|(_, d)| *d != data)
        .ok_or("UAC2 capture feedforward endpoint missing")?;
    // ADC2 3.16.2 adopts USB2 9.6.6's numbering scheme: the first data
    // endpoint and its feedback pipe have the same number, opposite direction.
    // Exactly one data endpoint is admitted here, so that association is unique.
    if !standard_endpoint(report) || report[3] != 0x11 || report[2] != (data[2] ^ 0x80) {
        return Err("UAC2 capture feedforward requires opposite matching Isochronous/NoSync/Feedback OUT".into());
    }
    let bytes = usize::from(u16::from_le_bytes([report[4], report[5]]));
    let service_us = match speed {
        Speed::Full if bytes == 3 => service_interval_micros(speed, report[6]),
        Speed::High if bytes == 4 => service_interval_micros(speed, report[6]),
        Speed::Super | Speed::SuperPlus => {
            // Only descriptors belonging to this endpoint may prove capacity.
            // ss_feedback_geometry requires an immediate, unique unburst
            // companion and exact four-byte legacy/eight-byte enhanced capacity.
            let end = descriptors[index + 1..].iter().position(|d| d[1] == 5)
                .map_or(descriptors.len(), |offset| index + 1 + offset);
            let extra: Vec<u8> = descriptors[index + 1..end].iter()
                .flat_map(|d| d.iter().copied()).collect();
            ss_feedback_geometry(bytes as u16, report[6], &extra, true)?.1
        }
        _ => return Err("UAC2 capture feedforward has invalid fixed-point packet size".into()),
    };
    Ok((FeedforwardEndpoint { address: report[2], interval: report[6], service_us,
        bytes, full_speed: speed == Speed::Full }, index))
}

fn standard_endpoint(d: &[u8]) -> bool {
    d.len() == 7 && d[0] == 7 && d[1] == 5 && d[2] & 0x70 == 0
        && d[2] & 0x0f != 0 && (1..=16).contains(&d[6])
}

#[cfg(test)]
mod tests {
    use super::*;

    const DATA: [u8; 7] = [7, 5, 0x81, 0x09, 192, 0, 1];
    const REPORT: [u8; 7] = [7, 5, 1, 0x11, 3, 0, 4];

    #[test]
    fn uac2_capture_rate_pipe_retains_its_own_interval_and_raw_index() {
        let class_specific = [3, 0x24, 1];
        let descriptors = [&class_specific[..], &DATA, &REPORT];
        let (pipe, index) = find_standard_capture_feedforward(0x20, &DATA, &descriptors, Speed::Full).unwrap();
        assert_eq!(index, 2);
        assert_eq!(pipe, FeedforwardEndpoint { address: 1, interval: 4,
            service_us: 8000, bytes: 3, full_speed: true });
        assert_eq!(pipe.nominal(48_000).unwrap(), [0, 0, 12]);
        let mut report = REPORT; report[4] = 4;
        let (pipe, _) = find_standard_capture_feedforward(0x20, &DATA, &[&report, &DATA], Speed::High).unwrap();
        assert_eq!(pipe.service_us, 1000);
        assert_eq!(pipe.nominal(48_000).unwrap(), [0, 0, 6, 0]);
    }

    #[test]
    fn uac2_capture_requires_exact_proven_same_alternate_endpoint_pair() {
        let parse = |data: &[u8], descriptors: &[&[u8]]| {
            find_standard_capture_feedforward(0x20, data, descriptors, Speed::Full)
        };
        assert!(parse(&DATA, &[&REPORT]).is_err());
        assert!(parse(&DATA, &[&DATA, &DATA, &REPORT]).is_err());
        assert!(parse(&DATA, &[&DATA, &REPORT, &REPORT]).is_err());
        assert!(parse(&DATA, &[&DATA, &[9, 4, 1, 1, 2, 1, 2, 0x20, 0], &REPORT]).is_err());
        assert!(parse(&DATA, &[&DATA, &[3, 0x24], &REPORT]).is_err());
        let mut other = DATA; other[4] += 1;
        assert!(parse(&other, &[&DATA, &REPORT]).is_err());
        for (field, value) in [(0, 8), (1, 0x25), (2, 0), (2, 0x11), (2, 0x81),
            (2, 2), (3, 0x01), (3, 0x15), (4, 4), (5, 8), (6, 0), (6, 17)] {
            let mut bad = REPORT; bad[field] = value;
            assert!(parse(&DATA, &[&DATA, &bad]).is_err(), "{bad:?}");
        }
        for (field, value) in [(0, 8), (2, 1), (2, 0x80), (2, 0x91), (3, 0x05),
            (3, 0x29), (4, 0), (5, 8), (6, 0), (6, 17)] {
            let mut bad = DATA; bad[field] = value;
            assert!(parse(&bad, &[&bad, &REPORT]).is_err(), "{bad:?}");
        }
        let extended = [9, 5, 1, 0x11, 3, 0, 4, 0, 0];
        assert!(parse(&DATA, &[&DATA, &extended]).is_err());
        for protocol in [0, 0x30, 0xff] {
            assert!(find_standard_capture_feedforward(protocol, &DATA, &[&DATA, &REPORT], Speed::Full).is_err());
        }
        for speed in [Speed::Unknown, Speed::Low, Speed::High] {
            assert!(find_standard_capture_feedforward(0x20, &DATA, &[&DATA, &REPORT], speed).is_err());
        }
    }

    #[test]
    fn superspeed_capture_rate_pipe_requires_its_own_exact_unburst_companion() {
        for speed in [Speed::Super, Speed::SuperPlus] {
            for bytes in [4, 8] {
                let mut report = REPORT; report[4] = bytes;
                let companion = [6, 0x30, 0, 0, bytes, 0];
                let (pipe, index) = find_standard_capture_feedforward(0x20, &DATA,
                    &[&DATA, &report, &companion], speed).unwrap();
                assert_eq!((pipe.bytes, pipe.service_us, index), (usize::from(bytes), 1000, 1));
                for descriptors in [vec![&DATA[..], &report],
                    vec![&DATA[..], &companion, &report],
                    vec![&DATA[..], &report, &companion, &companion]] {
                    assert!(find_standard_capture_feedforward(0x20, &DATA, &descriptors, speed).is_err());
                }
                for (field, value) in [(2, 1), (3, 1), (3, 0x80), (4, bytes - 1), (5, 1)] {
                    let mut bad = companion; bad[field] = value;
                    assert!(find_standard_capture_feedforward(0x20, &DATA,
                        &[&DATA, &report, &bad], speed).is_err());
                }
            }
        }
    }
}
