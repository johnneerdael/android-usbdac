//! The per-session settings the transport consults: the configuration plus the
//! quirks resolved for the open device.
//!
//! v0.1 keeps one process-wide session, so these live in a process-wide slot
//! installed by [`Dac::open`](crate::Dac::open). Nothing outside the crate sees
//! that; moving them into the session object later changes no public API.

use crate::capabilities::DeviceInfo;
use crate::config::DacConfig;
use crate::quirk::{builtin_quirks, Quirk};
use once_cell::sync::Lazy;
use parking_lot::RwLock;
use std::sync::Arc;

/// Default wait after programming the clock.
pub(crate) const DEFAULT_CLOCK_SETTLE_DELAY_MS: u64 = 20;

#[derive(Debug, Clone, Default)]
pub(crate) struct Settings {
    pub(crate) config: DacConfig,
    quirks: Vec<Quirk>,
}

impl Settings {
    /// Merges the built-in table and the config's overrides for this device.
    /// Built-in entries come first, so a later override wins for valued quirks.
    pub(crate) fn for_device(config: DacConfig, info: &DeviceInfo) -> Self {
        let mut quirks = builtin_quirks(info.vendor_id, info.product_id, &info.product_name);
        for entry in &config.quirks {
            if entry.vendor_id == info.vendor_id
                && entry.product_id == info.product_id
                && entry
                    .name_contains
                    .as_deref()
                    .map_or(true, |needle| info.product_name.contains(needle))
            {
                quirks.extend_from_slice(&entry.quirks);
            }
        }
        Self { config, quirks }
    }

    pub(crate) fn has(&self, quirk: Quirk) -> bool {
        self.quirks.contains(&quirk)
    }

    pub(crate) fn skip_clock_validation(&self) -> bool {
        self.has(Quirk::SkipClockValidation)
    }

    pub(crate) fn ignore_invalid_sample_rate(&self) -> bool {
        self.has(Quirk::IgnoreInvalidSampleRate)
    }

    pub(crate) fn allow_unsynchronized_async(&self) -> bool {
        self.config.allow_unsynchronized_async
    }

    pub(crate) fn require_verified_rate(&self) -> bool {
        self.config.require_verified_rate || self.has(Quirk::RequireVerifiedRate)
    }

    pub(crate) fn settle_delay_ms(&self) -> u64 {
        self.config
            .clock_settle_delay_ms
            .or_else(|| {
                self.quirks.iter().rev().find_map(|quirk| match quirk {
                    Quirk::SettleDelayMs(ms) => Some(*ms),
                    _ => None,
                })
            })
            .unwrap_or(DEFAULT_CLOCK_SETTLE_DELAY_MS)
    }

    pub(crate) fn prefer_padded_24bit(&self) -> bool {
        self.config
            .padded_24bit
            .unwrap_or_else(|| self.has(Quirk::PreferPadded24BitTransport))
    }

    pub(crate) fn dsd_subslot_override(&self) -> Option<u8> {
        self.config.dsd_subslot.or_else(|| {
            self.quirks.iter().rev().find_map(|quirk| match quirk {
                Quirk::DsdSubslotSize(size) => Some(*size),
                _ => None,
            })
        })
    }

    pub(crate) fn dsd_big_endian(&self) -> bool {
        self.config
            .dsd_big_endian
            .unwrap_or_else(|| self.has(Quirk::DsdBigEndian))
    }

    pub(crate) fn dsd_bit_reverse(&self) -> bool {
        self.has(Quirk::DsdBitReverse)
    }
}

static SETTINGS: Lazy<RwLock<Arc<Settings>>> =
    Lazy::new(|| RwLock::new(Arc::new(Settings::default())));

/// The settings of the open session, or the defaults when none is open.
pub(crate) fn settings() -> Arc<Settings> {
    SETTINGS.read().clone()
}

pub(crate) fn install(settings: Settings) {
    *SETTINGS.write() = Arc::new(settings);
}

pub(crate) fn reset() {
    *SETTINGS.write() = Arc::new(Settings::default());
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::QuirkOverride;

    fn info(vendor_id: u16, product_id: u16, name: &str) -> DeviceInfo {
        DeviceInfo::new(vendor_id, product_id, name)
    }

    #[test]
    fn builtin_quirks_resolve_for_the_dawn_pro() {
        let settings = Settings::for_device(
            DacConfig::default(),
            &info(12230, 61546, "MOONDROP Dawn Pro"),
        );
        assert!(settings.require_verified_rate());
        assert_eq!(settings.settle_delay_ms(), 50);
        assert!(settings.prefer_padded_24bit());
        assert!(settings.dsd_big_endian());
        assert_eq!(settings.dsd_subslot_override(), Some(4));
        assert!(!settings.skip_clock_validation());
    }

    #[test]
    fn an_unknown_device_gets_the_defaults() {
        let settings = Settings::for_device(DacConfig::default(), &info(1, 2, "Anything"));
        assert_eq!(settings.settle_delay_ms(), DEFAULT_CLOCK_SETTLE_DELAY_MS);
        assert!(!settings.prefer_padded_24bit());
        assert!(!settings.dsd_big_endian());
        assert_eq!(settings.dsd_subslot_override(), None);
        assert!(settings.require_verified_rate());
    }

    #[test]
    fn config_overrides_are_added_for_the_matching_device_only() {
        let config = DacConfig {
            quirks: vec![QuirkOverride {
                vendor_id: 1,
                product_id: 2,
                name_contains: Some("Foo".to_string()),
                quirks: vec![Quirk::DsdBigEndian, Quirk::SettleDelayMs(80)],
            }],
            ..DacConfig::default()
        };
        let matching = Settings::for_device(config.clone(), &info(1, 2, "A Foo DAC"));
        assert!(matching.dsd_big_endian());
        assert_eq!(matching.settle_delay_ms(), 80);
        let other_name = Settings::for_device(config.clone(), &info(1, 2, "Bar"));
        assert!(!other_name.dsd_big_endian());
        let other_id = Settings::for_device(config, &info(3, 2, "A Foo DAC"));
        assert!(!other_id.dsd_big_endian());
    }

    #[test]
    fn explicit_config_beats_the_quirk_table() {
        let config = DacConfig {
            dsd_big_endian: Some(false),
            dsd_subslot: Some(2),
            padded_24bit: Some(false),
            clock_settle_delay_ms: Some(5),
            ..DacConfig::default()
        };
        let settings = Settings::for_device(config, &info(12230, 61546, "MOONDROP Dawn Pro"));
        assert!(!settings.dsd_big_endian());
        assert_eq!(settings.dsd_subslot_override(), Some(2));
        assert!(!settings.prefer_padded_24bit());
        assert_eq!(settings.settle_delay_ms(), 5);
    }

    #[test]
    fn require_verified_rate_is_forced_by_a_quirk_even_when_the_config_is_off() {
        let config = DacConfig {
            require_verified_rate: false,
            ..DacConfig::default()
        };
        let dawn = Settings::for_device(config.clone(), &info(12230, 61546, "MOONDROP Dawn Pro"));
        assert!(dawn.require_verified_rate());
        let other = Settings::for_device(config, &info(1, 2, "x"));
        assert!(!other.require_verified_rate());
    }
}
