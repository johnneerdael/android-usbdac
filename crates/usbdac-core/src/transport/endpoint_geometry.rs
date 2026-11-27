//! Shared USB SuperSpeed periodic endpoint descriptor geometry.
use rusb::Speed;

/// USB 3.2 section 9.6.7 ordinary periodic companion geometry. Keep this
/// separate from USB 2 high-bandwidth wMaxPacketSize transaction bits. libusb
/// retains the wire companion descriptor in the endpoint's extra bytes.
pub(super) fn ss_periodic_geometry(
    packet: u16,
    interval: u8,
    extra: &[u8],
    isochronous: bool,
) -> Result<(usize, u32, [u8; 6]), String> {
    if !(1..=1024).contains(&packet) || !(1..=16).contains(&interval) {
        return Err("USB invalid SuperSpeed endpoint packet/interval".into());
    }
    if extra.len() < 6 || extra[0] != 6 || extra[1] != 0x30 {
        return Err("USB SuperSpeed endpoint requires an immediate companion".into());
    }
    let companion: [u8; 6] = extra[..6].try_into().unwrap();
    let burst = companion[2];
    let attributes = companion[3];
    let extended = attributes & 0x80 != 0;
    // USB 3.2 rev 1.1 Table 9-28: Mult is ignored in SSP mode.
    if burst > 15
        || attributes & 0x7c != 0
        || (!extended && attributes & 3 == 3)
        || (burst > 0 && packet != 1024)
        || (!extended && burst == 0 && attributes & 3 != 0)
        || (!isochronous && attributes != 0)
    {
        return Err("USB invalid SuperSpeed burst/Mult or reserved attributes".into());
    }
    // USB 3.2 rev 1.1 sections 9.6.8/9.8, Tables 9-29/9-32.
    // rusb's SuperPlus does not retain lane count/rate. Every above-Gen1
    // link supports at least twice Gen1x1 bandwidth: admit strictly below
    // 49152 * 2, never assume the fourfold bound of Gen2x2.
    let extended_capacity = if extended {
        if extra.len() < 14
            || extra[6] != 8
            || extra[7] != 0x31
            || extra[8..10] != [0, 0]
            || companion[4..6] != [1, 0]
        {
            return Err("USB invalid immediate SSP isochronous companion".into());
        }
        let bytes = u32::from_le_bytes(extra[10..14].try_into().unwrap());
        if bytes == 0 {
            return Err("USB SSP capacity must be nonzero".into());
        }
        if bytes >= 96 * 1024 {
            return Err("USB SSP capacity requires negotiated lane/rate evidence".into());
        }
        Some(usize::try_from(bytes).map_err(|_| "USB SSP capacity exceeds host size")?)
    } else {
        None
    };
    let mut at = if extended { 14 } else { 6 };
    while at < extra.len() {
        if extra.len() - at < 2 || extra[at] < 2 || usize::from(extra[at]) > extra.len() - at {
            return Err("USB malformed SuperSpeed endpoint extras".into());
        }
        if matches!(extra[at + 1], 0x30 | 0x31) {
            return Err("USB duplicated or unsupported extended companion".into());
        }
        at += usize::from(extra[at]);
    }
    let capacity = extended_capacity
        .unwrap_or_else(|| usize::from(u16::from_le_bytes([companion[4], companion[5]])));
    let maximum = usize::from(packet)
        .checked_mul(usize::from(burst) + 1)
        .and_then(|n| n.checked_mul(usize::from(attributes & 3) + 1))
        .ok_or("USB SuperSpeed capacity overflow")?;
    if capacity == 0 || (!extended && capacity > maximum) {
        return Err("USB invalid SuperSpeed wBytesPerInterval".into());
    }
    Ok((capacity, 125u32 << (interval - 1), companion))
}

/// Validate link speed before interpreting SuperSpeed endpoint companions.
pub(super) fn ss_endpoint_geometry(
    packet: u16,
    interval: u8,
    extra: &[u8],
    speed: Speed,
    isochronous: bool,
) -> Result<(usize, u32, [u8; 6]), String> {
    if !matches!(speed, Speed::Super | Speed::SuperPlus) {
        return Err("USB SuperSpeed endpoint requires a recognized SuperSpeed link".into());
    }
    if speed != Speed::SuperPlus && extra.len() >= 4 && extra[3] & 0x80 != 0 {
        return Err("USB SSP companion requires a SuperPlus link".into());
    }
    ss_periodic_geometry(packet, interval, extra, isochronous)
}

pub(super) fn ss_feedback_geometry(
    packet: u16,
    interval: u8,
    extra: &[u8],
    isochronous: bool,
) -> Result<(usize, u32, [u8; 6]), String> {
    let geometry = ss_periodic_geometry(packet, interval, extra, isochronous)?;
    // USB 3.2 section 4.4.8.4.1: Enhanced SuperSpeed uses eight-byte Q32.32
    // isochronous feedback. Retain exact four-byte legacy compatibility.
    let valid_bytes = packet == 4 || (isochronous && packet == 8);
    if !valid_bytes || geometry.0 != usize::from(packet) || geometry.2[2] != 0 || geometry.2[3] != 0
    {
        return Err("USB SuperSpeed feedback requires exact four-byte legacy or eight-byte isochronous unburst capacity".into());
    }
    Ok(geometry)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordinary_companions_require_a_recognized_superspeed_link() {
        let companion = [6, 0x30, 0, 0, 4, 0];
        for speed in [Speed::Super, Speed::SuperPlus] {
            assert_eq!(
                ss_endpoint_geometry(4, 4, &companion, speed, true).unwrap(),
                (4, 1000, companion)
            );
        }
        for speed in [Speed::Unknown, Speed::Low, Speed::Full, Speed::High] {
            assert!(ss_endpoint_geometry(4, 4, &companion, speed, true).is_err());
        }
    }

    #[test]
    fn enhanced_feedback_requires_exact_unburst_isochronous_eight_bytes() {
        let companion = [6, 0x30, 0, 0, 8, 0];
        assert_eq!(
            ss_feedback_geometry(8, 4, &companion, true).unwrap(),
            (8, 1000, companion)
        );
        assert!(ss_feedback_geometry(8, 4, &companion, false).is_err());
        assert!(ss_feedback_geometry(8, 4, &[6, 0x30, 0, 0, 4, 0], true).is_err());
        assert!(ss_feedback_geometry(16, 4, &companion, true).is_err());
        assert!(ss_feedback_geometry(8, 4, &[6, 0x30, 1, 0, 8, 0], true).is_err());
        assert!(ss_feedback_geometry(4, 4, &[6, 0x30, 0, 0, 4, 0], false).is_ok());
    }

    #[test]
    fn extended_companions_require_superplus() {
        let companion = [6, 0x30, 0, 0x80, 1, 0, 8, 0x31, 0, 0, 0, 0, 1, 0];
        assert_eq!(
            ss_endpoint_geometry(1024, 1, &companion, Speed::SuperPlus, true)
                .unwrap()
                .0,
            65536
        );
        assert!(ss_endpoint_geometry(1024, 1, &companion, Speed::Super, true).is_err());
    }
}
