//! Standard AudioStreaming alternate-setting controls (UAC2 and ADC3).

use crate::descriptors::DescriptorIter;

pub(super) const AS_ACTIVE_ALT_SETTING_CONTROL: u16 = 0x0100;
pub(super) const AS_VALID_ALT_SETTINGS_CONTROL: u16 = 0x0200;
pub(super) const MAX_VALID_ALTERNATES_REPLY_LEN: usize = 33;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct AlternateControls {
    pub(super) active: bool,
    pub(super) valid: bool,
}

pub(super) fn advertised_controls(protocol: u8, extra: &[u8]) -> Result<AlternateControls, String> {
    let standard_length = match protocol {
        0x20 => 16,
        0x30 => 23,
        _ => return Ok(AlternateControls::default()),
    };
    let mut controls = None;
    for descriptor in DescriptorIter::new(extra) {
        // Short private UAC2 descriptors and absent BADD descriptors have no
        // standard bmControls field; retain their existing transport behavior.
        if descriptor.len() != standard_length || descriptor[1] != 0x24 || descriptor[2] != 1 {
            continue;
        }
        if controls.is_some() {
            return Err("duplicate standard AudioStreaming AS_GENERAL descriptor".into());
        }
        let bitmap = if protocol == 0x30 {
            u32::from_le_bytes(descriptor[4..8].try_into().unwrap())
        } else {
            u32::from(descriptor[4])
        };
        let active = read_only_control(bitmap & 3, "Active Alternate Setting")?;
        let valid = read_only_control((bitmap >> 2) & 3, "Valid Alternate Settings")?;
        if active && !valid {
            return Err(
                "Active Alternate Setting requires Valid Alternate Settings control".into(),
            );
        }
        controls = Some(AlternateControls { active, valid });
    }
    Ok(controls.unwrap_or_default())
}

fn read_only_control(value: u32, name: &str) -> Result<bool, String> {
    match value {
        0 => Ok(false),
        1 => Ok(true),
        _ => Err(format!("{name} control must be absent or read-only")),
    }
}

// A clear candidate bit is meaningful only after validating the entire reply
// against the owning interface's declared alternate settings.
pub(super) fn valid_alternate_is_available(
    response: &[u8],
    declared: &[u8],
    requested: u8,
) -> Result<bool, String> {
    let size = response.first().copied().unwrap_or(0) as usize;
    if !(1..=32).contains(&size) || response.len() != 1 + size {
        return Err("Valid Alternate Settings reply must contain bSize (1..32) and exactly that many bitmap bytes".into());
    }
    let bitmap = &response[1..];
    if declared
        .iter()
        .any(|&alternate| usize::from(alternate) / 8 >= size)
    {
        return Err(
            "Valid Alternate Settings bitmap does not cover all declared alternates".into(),
        );
    }
    if bitmap[0] & 1 == 0 {
        return Err("Valid Alternate Settings reply does not mark alternate zero valid".into());
    }
    for (byte_index, byte) in bitmap.iter().copied().enumerate() {
        for bit in 0..8 {
            if byte & (1 << bit) != 0 {
                let alternate = (byte_index * 8 + bit) as u8;
                if !declared.contains(&alternate) {
                    return Err(format!("Valid Alternate Settings reply marks undeclared alternate {alternate} valid"));
                }
            }
        }
    }
    if !declared.contains(&requested) {
        return Err(format!("requested alternate {requested} is not declared"));
    }
    let requested_index = usize::from(requested);
    Ok(bitmap[requested_index / 8] & (1 << (requested_index % 8)) != 0)
}

pub(super) fn validate_active_alt(response: &[u8], requested: u8) -> Result<(), String> {
    if response != [requested] {
        return Err(format!("Active Alternate Setting reply must be one byte matching requested alternate {requested}"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn general(protocol: u8, controls: u32) -> Vec<u8> {
        let mut descriptor = vec![0; if protocol == 0x20 { 16 } else { 23 }];
        descriptor[0] = descriptor.len() as u8;
        descriptor[1] = 0x24;
        descriptor[2] = 1;
        descriptor[4..8].copy_from_slice(&controls.to_le_bytes());
        descriptor
    }

    #[test]
    fn standard_controls_are_read_only_and_active_requires_valid() {
        for protocol in [0x20, 0x30] {
            for (bitmap, expected) in [(0, (false, false)), (4, (false, true)), (5, (true, true))] {
                let result = advertised_controls(protocol, &general(protocol, bitmap)).unwrap();
                assert_eq!((result.active, result.valid), expected);
            }
            for bitmap in [1, 2, 3, 6, 7, 8, 12, 13] {
                assert!(advertised_controls(protocol, &general(protocol, bitmap)).is_err());
            }
            let mut duplicate = general(protocol, 5);
            duplicate.extend(general(protocol, 5));
            assert!(advertised_controls(protocol, &duplicate).is_err());
        }
    }

    #[test]
    fn absent_and_private_descriptors_do_not_advertise_standard_controls() {
        for protocol in [0, 0x20, 0x30] {
            assert_eq!(
                advertised_controls(protocol, &[]).unwrap(),
                AlternateControls::default()
            );
        }
        assert_eq!(
            advertised_controls(0x20, &[7, 0x24, 1, 1, 0xff, 0xff, 0xff]).unwrap(),
            AlternateControls::default()
        );
    }

    #[test]
    fn valid_alternates_enforce_exact_size_zero_and_sparse_membership() {
        assert_eq!(
            valid_alternate_is_available(&[1, 0b1001], &[0, 3], 3),
            Ok(true)
        );
        for response in [
            vec![],
            vec![0],
            vec![33],
            vec![1],
            vec![1, 9, 0],
            vec![2, 9],
            vec![1, 8],
            vec![1, 11],
        ] {
            assert!(valid_alternate_is_available(&response, &[0, 3], 3).is_err());
        }
        assert_eq!(valid_alternate_is_available(&[1, 1], &[0, 3], 3), Ok(false));
        assert!(valid_alternate_is_available(&[1, 1], &[0, 8], 8).is_err());
        assert!(valid_alternate_is_available(&[1, 1], &[3], 0).is_err());
        assert!(valid_alternate_is_available(&[1, 1], &[0, 3], 2).is_err());
        assert!(valid_alternate_is_available(&[1, 1], &[0, 3], 255).is_err());
    }

    #[test]
    fn bitmap_covers_alternate_255_without_wrapping() {
        let mut response = vec![0; MAX_VALID_ALTERNATES_REPLY_LEN];
        response[0] = 32;
        response[1] = 1;
        response[32] = 0x80;
        assert_eq!(
            valid_alternate_is_available(&response, &[0, 255], 255),
            Ok(true)
        );
        assert!(valid_alternate_is_available(&response, &[0, 254], 254).is_err());
        assert!(valid_alternate_is_available(&[1, 3], &[0, 1, 255], 1).is_err());
        response[32] = 0;
        assert_eq!(
            valid_alternate_is_available(&response, &[0, 255], 255),
            Ok(false)
        );
        assert_eq!(
            valid_alternate_is_available(&response, &[0, 255], 0),
            Ok(true)
        );
    }

    #[test]
    fn active_reply_is_exactly_the_requested_byte() {
        assert!(validate_active_alt(&[255], 255).is_ok());
        for response in [vec![], vec![0], vec![255, 0]] {
            assert!(validate_active_alt(&response, 255).is_err());
        }
    }
}
