use super::*;

/// Verify the just-encoded wire payload looks like real DSD transport data.
/// DoP: all channels share a marker and every frame alternates. This is a
/// framing check, NOT proof of source-to-wire byte identity. Native: require
/// at least one non-zero byte (gross
/// pipeline-failure guard; DSD has no in-band markers to check).
pub(super) fn audit_dsd_payload(
    buffer: &[u8],
    dsd_transport: DsdTransportMode,
    subslot_size: u8,
    channels: u16,
) -> bool {
    if buffer.is_empty() {
        return false;
    }
    let ss = subslot_size as usize;
    match dsd_transport {
        DsdTransportMode::DoP => {
            if !(ss == 3 || ss == 4) || channels == 0 {
                return false;
            }
            let ch = channels.max(1) as usize;
            let stride = ss * ch;
            if buffer.len() % stride != 0 || buffer.len() < 2 * stride {
                return false;
            }
            let mut previous: Option<u8> = None;
            for frame in buffer.chunks_exact(stride) {
                let marker = frame[ss - 1];
                if marker != 0x05 && marker != 0xFA {
                    return false;
                }
                if previous == Some(marker) || frame.chunks_exact(ss).any(|word| word[ss - 1] != marker) {
                    return false;
                }
                previous = Some(marker);
            }
            true
        }
        DsdTransportMode::Native => buffer.iter().any(|&b| b != 0),
        DsdTransportMode::None => true,
    }
}

/// Verify the just-encoded PCM wire payload looks like real audio, not a stuck
/// or dead pipeline. Mirrors audit_dsd_payload's gross-guard philosophy: decode
/// channel-0 samples at the negotiated subslot width (little-endian signed, per
/// UAC2 Type I PCM) and require at least two distinct values across the buffer.
/// A real music transfer is effectively never sample-constant; a dead pipeline
/// emits a constant (often all-zero or a fixed DC value).
// Statistical guard, not a bit-exact hash. Upgrade path: per-frame
// hash comparison against decoded source bytes if a stronger claim is needed.
pub(super) fn audit_pcm_payload(buffer: &[u8], subslot_size: u8, channels: u16) -> bool {
    if buffer.is_empty() || subslot_size == 0 {
        return false;
    }
    let ss = subslot_size as usize;
    let ch = channels.max(1) as usize;
    let stride = ss * ch;
    if stride == 0 || buffer.len() < stride * 2 {
        return false;
    }
    let bits = ss * 8;
    let sign_extend = |v: i64| -> i64 {
        if v & (1 << (bits - 1)) != 0 {
            v | (-1i64 << bits)
        } else {
            v
        }
    };
    let decode = |i: usize| -> i64 {
        let off = i * stride;
        let mut v: i64 = 0;
        for b in 0..ss {
            v |= (buffer[off + b] as i64) << (8 * b);
        }
        sign_extend(v)
    };
    let limit = (buffer.len() / stride).min(1024);
    let first = decode(0);
    (1..limit).any(|i| decode(i) != first)
}

/// Scales PCM samples by `gain` with round-to-nearest and saturation.
///
/// This is the only place the library changes sample values, and it is only
/// reached while the host has enabled software gain (`gain != 1.0`); a gain of
/// exactly 1.0 returns without touching the samples. No dither is added: the
/// encoder truncates to the wire width afterwards, exactly as it does for
/// unscaled samples.
pub(super) fn apply_software_gain(samples: &mut [i32], gain: f32) {
    if gain == 1.0 {
        return;
    }
    let gain = f64::from(gain);
    for sample in samples.iter_mut() {
        let scaled = (f64::from(*sample) * gain).round();
        *sample = scaled.clamp(f64::from(i32::MIN), f64::from(i32::MAX)) as i32;
    }
}

/// Map left-justified linear PCM `i32` to 16-bit samples for USB.
///
/// Uses the top 16 bits (arithmetic shift), matching UAC2-style downscale from a 32-bit
/// linear container and avoiding a float round-trip. For values produced by
/// `convert_f32_to_pcm_samples`, `sample >> 16` is always in `i16` range.
#[inline]
pub(super) fn linear_pcm_i32_to_i16(sample: i32) -> i16 {
    (sample >> 16) as i16
}

/// 24-bit packed little-endian (3 bytes per channel), from linear full-scale i32.
#[inline]
pub(super) fn encode_i24(sample: i32) -> [u8; 3] {
    let s = sample >> 8;
    [
        (s & 0xFF) as u8,
        ((s >> 8) & 0xFF) as u8,
        ((s >> 16) & 0xFF) as u8,
    ]
}

#[inline]
pub(super) fn encode_i32_le(sample: i32) -> [u8; 4] {
    sample.to_le_bytes()
}

/// Encode exact Type I U8 from the ring's reversible centered bit representation.
pub(super) fn encode_unsigned8_slots(input: &[i32], output: &mut [u8]) -> Result<(), String> {
    if input.len() != output.len() { return Err("unsigned8 sample/byte count mismatch".into()); }
    for (sample, byte) in input.iter().zip(output) { *byte = sample.to_le_bytes()[3] ^ 0x80; }
    Ok(())
}

/// Pack PCM / DoP / native DSD for the active UAC subslot.
///
/// PCM and DoP: each i32 encodes `subslot_size` wire bytes.
/// Native DSD: each i32 carries one DSD byte; `subslot_size` consecutive
/// same-channel bytes are packed into each wire word (1:1 byte count).
pub(super) fn encode_usb_pcm_slots(
    input: &[i32],
    output: &mut [u8],
    subslot_size: u8,
    bit_resolution: u8,
    dsd_transport: DsdTransportMode,
    channels: u16,
    big_endian: bool,
    bit_reverse: bool,
) -> Result<(), String> {
    let ss = subslot_size as usize;
    if ss == 0 {
        return Err("subslot_size is zero".to_string());
    }

    if dsd_transport == DsdTransportMode::Native {
        let ch = channels as usize;
        if ch == 0 {
            return Err("channels is zero".to_string());
        }
        if !matches!(ss, 1 | 2 | 4 | 8) || input.len() % (ss * ch) != 0 {
            return Err("native DSD requires complete 1-, 2-, 4- or 8-byte channel words".into());
        }
        if output.len() != input.len() {
            return Err(format!(
                "USB native DSD buffer length mismatch: output {} bytes, expected {} (1:1 with {} samples)",
                output.len(),
                input.len(),
                input.len()
            ));
        }
        if ss == 1 {
            for (i, &sample) in input.iter().enumerate() {
                let b = (sample & 0xFF) as u8;
                output[i] = if bit_reverse { b.reverse_bits() } else { b };
            }
        } else {
            let chunk_samples = ss * ch;
            let num_chunks = input.len() / chunk_samples;
            for chunk in 0..num_chunks {
                let in_base = chunk * chunk_samples;
                let out_base = chunk * chunk_samples;
                for chan in 0..ch {
                    for b in 0..ss {
                        let in_idx = in_base + chan + b * ch;
                        let raw = (input[in_idx] & 0xFF) as u8;
                        let dsd_byte = if bit_reverse { raw.reverse_bits() } else { raw };
                        // ADC3 DSD uses LE64: oldest bit D63, newest D0.
                        // The input is chronological MSB-first bytes, so little
                        // endian reverses bytes within a word, never its bits.
                        let out_b = if big_endian { b } else { ss - 1 - b };
                        let out_idx = out_base + chan * ss + out_b;
                        output[out_idx] = dsd_byte;
                    }
                }
            }
        }
        return Ok(());
    }

    let expected = input.len().checked_mul(ss).ok_or_else(|| {
        format!(
            "USB PCM sample count overflow: {} samples × {} subslot",
            input.len(),
            ss
        )
    })?;
    if output.len() != expected {
        return Err(format!(
            "USB PCM buffer length mismatch: output {} bytes, expected {} ({} samples × {} subslot)",
            output.len(),
            expected,
            input.len(),
            ss
        ));
    }

    match dsd_transport {
        DsdTransportMode::DoP => {
            encode_dop_slots(input, output, subslot_size, bit_resolution);
            return Ok(());
        }
        DsdTransportMode::Native => unreachable!("handled above"),
        DsdTransportMode::None => {}
    }

    match subslot_size {
        2 => {
            if bit_resolution == 16 {
                encode_pcm_i16_le(input, output);
            } else {
                encode_pcm_bytes(input, output, subslot_size, bit_resolution);
            }
            Ok(())
        }
        3 => {
            for (index, sample) in input.iter().enumerate() {
                let slot = encode_i24(*sample);
                let offset = index * 3;
                output[offset..offset + 3].copy_from_slice(&slot);
            }
            Ok(())
        }
        4 => {
            if bit_resolution >= 32 {
                for (index, &sample) in input.iter().enumerate() {
                    let offset = index * 4;
                    output[offset..offset + 4].copy_from_slice(&encode_i32_le(sample));
                }
            } else {
                let unused: u32 = 32u32.saturating_sub(bit_resolution as u32);
                let mask = !((1u32.wrapping_shl(unused)).wrapping_sub(1) as i32);
                for (index, &sample) in input.iter().enumerate() {
                    let offset = index * 4;
                    let packed = (sample & mask).to_le_bytes();
                    output[offset..offset + 4].copy_from_slice(&packed);
                }
            }
            Ok(())
        }
        _ => {
            encode_pcm_bytes(input, output, subslot_size, bit_resolution);
            Ok(())
        }
    }
}

/// DoP frames arrive 32-bit left-justified (marker in bits 31:24, 16 data bits
/// in 23:8, low byte zero) — the same canonical word the DAP I32 path uses.
/// Emit the top `subslot_size` bytes little-endian so the marker stays the
/// most-significant wire byte, where the DAC scans for 0x05/0xFA.
pub(super) fn encode_dop_slots(input: &[i32], output: &mut [u8], subslot_size: u8, _bit_resolution: u8) {
    let ss = subslot_size as usize;
    for (index, &sample) in input.iter().enumerate() {
        let w = sample as u32;
        let offset = index * ss;
        for b in 0..ss {
            let shift = 8 * (4usize.saturating_sub(ss) + b);
            output[offset + b] = if shift < 32 { (w >> shift) as u8 } else { 0 };
        }
    }
}

/// 16-bit PCM: linear i32 → i16 (MSB extract) → LE.
pub(super) fn encode_pcm_i16_le(input: &[i32], output: &mut [u8]) {
    debug_assert_eq!(output.len(), input.len() * 2);
    for (i, &sample) in input.iter().enumerate() {
        let i16_val = linear_pcm_i32_to_i16(sample);
        let bytes = i16_val.to_le_bytes();
        output[i * 2] = bytes[0];
        output[i * 2 + 1] = bytes[1];
    }
}

/// Pack i32 PCM samples into the USB subslot byte layout.
///
/// UAC2 Data Formats spec: PCM is signed two's complement, left-justified
/// (sign bit is the MSB of the container). Unused LSBs are zero-padded.
///
/// When `subslot_size * 8 > bit_resolution`, the sample is shifted left
/// so that the sign bit occupies the MSB of the container.
pub(super) fn encode_pcm_bytes(input: &[i32], output: &mut [u8], subslot_size: u8, bit_resolution: u8) {
    let subslot_bytes = usize::from(subslot_size);
    debug_assert_eq!(output.len(), input.len() * subslot_bytes);

    let container_bits = (subslot_size as u32) * 8;
    let right_shift = 32u32.saturating_sub(bit_resolution as u32);
    let left_shift = container_bits.saturating_sub(bit_resolution as u32);

    for (index, &sample) in input.iter().enumerate() {
        let offset = index * subslot_bytes;
        let kept = if right_shift == 0 {
            sample
        } else {
            sample >> right_shift
        };
        let padded = if left_shift == 0 {
            kept
        } else {
            kept << left_shift
        };
        let bytes = padded.to_le_bytes();
        for byte_index in 0..subslot_bytes {
            output[offset + byte_index] = bytes[byte_index];
        }
    }
}

