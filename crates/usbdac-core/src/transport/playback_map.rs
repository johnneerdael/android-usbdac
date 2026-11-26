//! Descriptor-backed playback identities in interleaved USB wire order.
use super::*;
use crate::capture::ChannelMap;

pub(super) fn for_candidate(
    device: &Device<Context>, handle: &DeviceHandle<Context>, candidate: &IsoStreamCandidate,
) -> Result<ChannelMap, String> {
    // RAW_DATA has transport channels, without a PCM speaker assignment.
    if candidate.format_tag == FORMAT_TAG_DSD {
        return unnamed(candidate.channels);
    }
    if let Some(route) = adc3_route_for_candidate(device, handle, candidate)? {
        return adc3::capture_channel_map(&route);
    }
    let config = device.active_config_descriptor().map_err(|e| e.to_string())?;
    let raw = read_active_configuration_bytes(handle, &config)
        .ok_or("cannot read playback channel descriptors")?;
    from_raw(&raw, candidate.interface_number, candidate.alt_setting, candidate.channels)
}

fn unnamed(channels: u16) -> Result<ChannelMap, String> {
    ChannelMap::uac2(channels, 0).map_err(|e| format!("playback channel map: {e:?}"))
}

struct Interface<'a> { number: u8, alt: u8, class: u8, subclass: u8, protocol: u8, descriptors: Vec<&'a [u8]> }

/// Missing or ambiguous identities stay unnamed. Malformed descriptor chains
/// refuse negotiation rather than letting the map drift at stream open.
fn from_raw(raw: &[u8], number: u8, alternate: u8, channels: u16) -> Result<ChannelMap, String> {
    let mut interfaces: Vec<Interface<'_>> = Vec::new();
    let mut iads = Vec::new();
    let mut at = 0;
    while at < raw.len() {
        let length = usize::from(raw[at]);
        if length < 2 || length > raw.len() - at { return Err("malformed playback descriptor chain".into()); }
        let d = &raw[at..at + length];
        match d[1] {
            4 => {
                if d.len() < 9 { return Err("short playback interface descriptor".into()); }
                interfaces.push(Interface { number: d[2], alt: d[3], class: d[5], subclass: d[6], protocol: d[7], descriptors: Vec::new() });
            }
            0x0b => iads.push(d),
            _ => if let Some(interface) = interfaces.last_mut() { interface.descriptors.push(d); },
        }
        at += length;
    }
    let selected: Vec<_> = interfaces.iter().filter(|d| d.number == number && d.alt == alternate && d.class == 1 && d.subclass == 2).collect();
    let [stream] = selected.as_slice() else { return Err("playback interface disappeared or duplicated".into()); };
    let general: Vec<_> = stream.descriptors.iter().copied().filter(|d| d.len() >= 3 && d[1..3] == [0x24, 1]).collect();
    let [general] = general.as_slice() else { return unnamed(channels); };
    let (terminal, as_mask) = match stream.protocol {
        0 if general.len() == 7 => (general[3], 0),
        0x20 if general.len() == 16 && u16::from(general[10]) == channels =>
            (general[3], u32::from_le_bytes(general[11..15].try_into().unwrap())),
        _ => return unnamed(channels),
    };
    if as_mask != 0 {
        return Ok(ChannelMap::uac2(channels, as_mask).unwrap_or(unnamed(channels)?));
    }
    // UAC2 needs one unambiguous owning IAD. UAC1 uses the AC header's
    // baInterfaceNr list; never search another audio function for a terminal ID.
    let owners: Vec<_> = interfaces.iter().filter(|ac| {
        if ac.class != 1 || ac.subclass != 1 || ac.alt != 0 || ac.protocol != stream.protocol { return false; }
        if stream.protocol == 0 {
            let headers: Vec<_> = ac.descriptors.iter().filter(|d| d.len() >= 8 && d[1..3] == [0x24, 1]).collect();
            matches!(headers.as_slice(), [h] if h.len() == 8 + usize::from(h[7]) && h[8..].contains(&number))
        } else {
            let associations: Vec<_> = iads.iter().filter(|d| d.len() >= 8 && number >= d[2] && u16::from(number) < u16::from(d[2]) + u16::from(d[3])).collect();
            matches!(associations.as_slice(), [iad] if iad[4..7] == [1, 0, 0x20] && iad[2] == ac.number)
        }
    }).collect();
    let [owner] = owners.as_slice() else { return unnamed(channels); };
    // Duplicate entity IDs make even an otherwise valid Input Terminal ambiguous.
    let terminals: Vec<_> = owner.descriptors.iter().copied().filter(|d| d.len() >= 4 && d[1] == 0x24 && d[2] != 1 && d[3] == terminal).collect();
    let [input] = terminals.as_slice() else { return unnamed(channels); };
    if input[2] != 2 || input.get(4..6) != Some(&[1, 1][..]) { return unnamed(channels); }
    let map = if stream.protocol == 0 && input.len() >= 12 && u16::from(input[7]) == channels {
        ChannelMap::uac1(channels, u16::from_le_bytes([input[8], input[9]])).ok()
    } else if stream.protocol == 0x20 && input.len() >= 17 && u16::from(input[8]) == channels {
        ChannelMap::uac2(channels, u32::from_le_bytes(input[9..13].try_into().unwrap())).ok()
    } else { None };
    map.map_or_else(|| unnamed(channels), Ok)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::ChannelPosition::*;
    fn interface(number: u8, alt: u8, subclass: u8, protocol: u8) -> Vec<u8> { vec![9,4,number,alt,0,1,subclass,protocol,0] }
    fn uac2(mask: u32, terminal_channels: u8, terminal_mask: u32) -> Vec<u8> {
        let mut raw = vec![8,11,0,2,1,0,0x20,0];
        raw.extend(interface(0,0,1,0x20));
        let mut terminal = vec![17,0x24,2,7,1,1,0,9,terminal_channels];
        terminal.extend(terminal_mask.to_le_bytes()); terminal.extend([0,0,0,0]); raw.extend(terminal);
        raw.extend(interface(1,1,2,0x20));
        let mut general = vec![16,0x24,1,7,0,1,1,0,0,0,2];
        general.extend(mask.to_le_bytes()); general.push(0); raw.extend(general); raw
    }
    #[test] fn uac2_prefers_as_and_falls_back_only_for_equal_counts() {
        assert_eq!(from_raw(&uac2(0x30,2,3),1,1,2).unwrap().positions(), &[BackLeft, BackRight]);
        assert_eq!(from_raw(&uac2(0,2,3),1,1,2).unwrap().positions(), &[FrontLeft, FrontRight]);
        assert_eq!(from_raw(&uac2(0,1,3),1,1,2).unwrap().positions(), &[Unspecified { index: 0 }, Unspecified { index: 1 }]);
    }
    #[test] fn absent_or_overlapping_association_never_borrows_terminal() {
        let raw = uac2(0,2,3);
        assert_eq!(from_raw(&raw[8..],1,1,2).unwrap(), unnamed(2).unwrap());
        let mut overlap = vec![8,11,0,2,1,0,0x20,0]; overlap.extend(raw);
        assert_eq!(from_raw(&overlap,1,1,2).unwrap(), unnamed(2).unwrap());
    }
    #[test] fn uac1_uses_linked_terminal_only_at_matching_count() {
        let mut raw = interface(0,0,1,0); raw.extend([9,0x24,1,0,1,0,0,1,1]);
        raw.extend([12,0x24,2,7,1,1,0,2,0x30,0,0,0]);
        raw.extend(interface(1,1,2,0)); raw.extend([7,0x24,1,7,0,1,0]);
        assert_eq!(from_raw(&raw,1,1,2).unwrap().positions(), &[BackLeft, BackRight]);
        assert_eq!(from_raw(&raw,1,1,1).unwrap(), unnamed(1).unwrap());
    }
    #[test] fn malformed_and_unknown_masks_do_not_invent_speakers() {
        assert!(from_raw(&[1],1,1,2).is_err());
        assert_eq!(from_raw(&uac2(1 << 30,2,3),1,1,2).unwrap(), unnamed(2).unwrap());
        assert_eq!(from_raw(&uac2(0x8000_0000,2,3),1,1,2).unwrap(), unnamed(2).unwrap());
    }
}
