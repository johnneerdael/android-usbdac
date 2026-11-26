//! Per-device behaviour switches.
//!
//! The built-in table only holds devices that were verified on hardware in the
//! project this library was extracted from. A host can add its own entries
//! through [`DacConfig::quirks`](crate::DacConfig::quirks).

/// One behaviour switch for a device. Only switches the transport actually
/// honours are listed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Quirk {
    /// The clock readback is unreliable (GET_CUR/GET_RANGE return 0 or
    /// fail). Trust SET_CUR for PCM and skip verification.
    SkipClockValidation,
    /// The DAC reports a rate that differs from the one just set (some report
    /// 0 Hz) while playing correctly; do not refuse over the mismatch (PCM).
    IgnoreInvalidSampleRate,
    /// Refuse to stream unless the DAC confirms the sampling frequency, even
    /// if [`DacConfig::require_verified_rate`](crate::DacConfig) is off.
    RequireVerifiedRate,
    /// Wait this many milliseconds after programming the clock.
    SettleDelayMs(u64),
    /// Prefer the 4-byte subslot over the 3-byte one for 24-bit PCM.
    PreferPadded24BitTransport,
    /// Override the subslot size (1, 2 or 4 bytes) of native DSD alternate
    /// settings, for devices whose descriptors are wrong or ambiguous.
    DsdSubslotSize(u8),
    /// Native DSD words go on the wire most-significant byte first.
    DsdBigEndian,
    /// Reverse the bit order within each native DSD byte.
    DsdBitReverse,
}

struct BuiltinEntry {
    vendor_id: u16,
    product_id: u16,
    /// Empty matches any product name with this VID/PID.
    product_name_contains: &'static str,
    quirks: &'static [Quirk],
}

const BUILTIN: &[BuiltinEntry] = &[
    // MOONDROP Dawn Pro: dual CS43131 behind an XMOS-style bridge, DSD_U32_BE.
    BuiltinEntry {
        vendor_id: 12230, // 0x2FC6
        product_id: 61546, // 0xF09A
        product_name_contains: "MOONDROP Dawn Pro",
        quirks: &[
            Quirk::RequireVerifiedRate,
            Quirk::SettleDelayMs(50),
            Quirk::PreferPadded24BitTransport,
            Quirk::DsdSubslotSize(4),
            Quirk::DsdBigEndian,
        ],
    },
    // Fosi Audio DS2 (Savitech bridge): GET_RANGE returns no subranges and
    // GET_CUR reads 0 Hz even after a successful SET_CUR, yet the DAC plays at
    // the requested rate. Trust SET_CUR and skip readback validation.
    BuiltinEntry {
        vendor_id: 9770, // 0x262A
        product_id: 1,
        product_name_contains: "",
        quirks: &[Quirk::SkipClockValidation, Quirk::IgnoreInvalidSampleRate],
    },
];

/// The built-in quirks for a device, in table order.
pub(crate) fn builtin_quirks(vendor_id: u16, product_id: u16, product_name: &str) -> Vec<Quirk> {
    let mut result = Vec::new();
    for entry in BUILTIN {
        if entry.vendor_id == vendor_id
            && entry.product_id == product_id
            && (entry.product_name_contains.is_empty()
                || product_name.contains(entry.product_name_contains))
        {
            result.extend_from_slice(entry.quirks);
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dawn_pro_quirks_found_by_vid_pid() {
        let quirks = builtin_quirks(12230, 61546, "MOONDROP Dawn Pro");
        assert!(quirks.contains(&Quirk::RequireVerifiedRate));
        assert!(quirks.contains(&Quirk::PreferPadded24BitTransport));
        assert!(quirks.contains(&Quirk::DsdBigEndian));
        assert!(quirks.contains(&Quirk::SettleDelayMs(50)));
    }

    #[test]
    fn the_product_name_must_match_when_the_entry_names_one() {
        assert!(builtin_quirks(12230, 61546, "Some Other Product").is_empty());
    }

    #[test]
    fn unknown_device_returns_empty() {
        assert!(builtin_quirks(0x0000, 0x0000, "Unknown").is_empty());
    }

    #[test]
    fn fosi_ds2_quirks_registered_for_any_name() {
        let quirks = builtin_quirks(9770, 1, "Fosi Audio DS2");
        assert!(quirks.contains(&Quirk::SkipClockValidation));
        assert!(quirks.contains(&Quirk::IgnoreInvalidSampleRate));
        assert!(!builtin_quirks(9770, 1, "").is_empty());
    }
}
