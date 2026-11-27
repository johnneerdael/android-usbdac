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
    source_quirks: Vec<Quirk>,
    descriptor_identity: Option<(u16,u16,u16)>,
    device_identity: Option<(u16, u16)>,
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
        Self { config, quirks, device_identity: Some((info.vendor_id, info.product_id)), source_quirks: Vec::new(), descriptor_identity: None }
    }

    pub(crate) fn has(&self, quirk: Quirk) -> bool {
        self.quirks.contains(&quirk) || self.source_quirks.contains(&quirk)
    }

    pub(crate) fn control_delay_us(&self) -> u64 {
        self.quirks.iter().chain(self.source_quirks.iter()).filter_map(|quirk| match quirk {
            Quirk::ControlMessageDelayUs(us) => Some(*us), _ => None,
        }).max().unwrap_or(0)
    }
    pub(crate) fn interface_delay_ms(&self) -> u64 {
        self.quirks.iter().chain(self.source_quirks.iter()).filter_map(|quirk| match quirk {
            Quirk::InterfaceDelayMs(ms) => Some(*ms), _ => None,
        }).max().unwrap_or(0)
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

    pub(crate) fn allow_nominal_uac1(&self, is_uac1: bool, full_speed: bool, pcm: bool, synch_address: u8) -> bool {
        self.allow_unsynchronized_async() || (!self.disable_feedback() && is_uac1 && full_speed && pcm
            && synch_address == 0
            && self.config.async_fallback_policy == crate::config::AsyncFallbackPolicy::FullSpeedUac1)
    }

    pub(crate) fn allow_unusable_clock_readback(&self, set_attempted: bool, set_succeeded: bool,
        reported: Option<u32>, multiplied: bool, dsd: bool) -> bool {
        set_attempted && set_succeeded && reported.map_or(true, |rate| rate == 0)
            && !multiplied && !dsd && !self.has(Quirk::RequireVerifiedRate)
            && (self.skip_clock_validation() || self.has(Quirk::SkipSampleRateReadback) || self.config.clock_readback_policy
                == crate::config::ClockReadbackPolicy::AllowUnusableAfterSuccessfulSet)
    }

    pub(crate) fn disable_feedback(&self) -> bool {
        self.config.disable_feedback
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

    /// Device-scoped policy is usable only for the identity read from USB.
    pub(crate) fn native_dsd_policy(&self, vendor: u16, product: u16, descriptor_width: u8) -> Option<(u8, bool, bool)> {
        let scoped = self.device_identity == Some((vendor, product));
        let width = if scoped { self.dsd_subslot_override() } else { self.config.dsd_subslot };
        let explicit_order = self.config.dsd_big_endian.is_some()
            || (scoped && (self.has(Quirk::DsdBigEndian) || self.has(Quirk::DsdBitReverse)));
        if width.is_none() && !explicit_order { return None; }
        Some((width.unwrap_or(descriptor_width),
            self.config.dsd_big_endian.unwrap_or(scoped && self.has(Quirk::DsdBigEndian)),
            scoped && self.dsd_bit_reverse()))
    }

    pub(crate) fn native_dsd_order(&self, vendor: u16, product: u16, big_endian: bool) -> (bool, bool) {
        let scoped = self.device_identity == Some((vendor, product));
        (self.config.dsd_big_endian.unwrap_or(big_endian || (scoped && self.has(Quirk::DsdBigEndian))),
         scoped && self.dsd_bit_reverse())
    }

    pub(crate) fn dsd_subslot_override(&self) -> Option<u8> {
        self.config.dsd_subslot.or_else(|| {
            self.quirks.iter().rev().find_map(|quirk| match quirk {
                Quirk::DsdSubslotSize(size) => Some(*size),
                _ => None,
            })
        })
    }

    #[cfg(test)]
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

/// Resolve source facts only from the descriptor read through the opened fd.
pub(crate) fn install_descriptor_identity(vendor: u16, product: u16, revision: u16) {
    let mut guard = SETTINGS.write();
    let mut next = (**guard).clone();
    let first = next.descriptor_identity != Some((vendor,product,revision));
    next.descriptor_identity = Some((vendor,product,revision));
    next.source_quirks = if next.config.quirk_policy == crate::quirk::QuirkPolicy::SourceBacked {
        crate::quirk::source_quirks(vendor,product,revision)
    } else { Vec::new() };
    if first && next.config.quirk_policy == crate::quirk::QuirkPolicy::SourceBacked {
        for row in crate::quirk::SOURCE_QUIRKS.iter().filter(|row| row.matches(vendor,product,revision)) {
            for flag in row.flags {
                log::info!("[USB] source-backed quirk {:04x}:{:04x} bcdDevice={:04x} {:?}: {}; hardware unverified; {}",
                    vendor,product,revision,flag,flag.applicability(),row.source_url());
            }
        }
    }
    *guard = Arc::new(next);
}

pub(crate) fn reset() {
    *SETTINGS.write() = Arc::new(Settings::default());
}

/// Changes the configuration of the installed settings in place, keeping the
/// quirks resolved for the open device.
pub(crate) fn update(change: impl FnOnce(&mut DacConfig)) {
    let mut guard = SETTINGS.write();
    let mut settings = (**guard).clone();
    change(&mut settings.config);
    *guard = Arc::new(settings);
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
    #[test]
    fn native_policy_uses_actual_usb_identity_and_keeps_order_only_tuning() {
        let dawn = Settings::for_device(DacConfig::default(), &info(12230, 61546, "MOONDROP Dawn Pro"));
        assert_eq!(dawn.native_dsd_policy(12230, 61546, 4), Some((4, true, false)));
        assert_eq!(dawn.native_dsd_policy(1, 2, 4), None);
        let config = DacConfig { dsd_big_endian: Some(true), ..DacConfig::default() };
        let host = Settings::for_device(config, &info(1, 2, "x"));
        assert_eq!(host.native_dsd_policy(1, 2, 2), Some((2, true, false)));
        let config = DacConfig { dsd_big_endian: Some(false), dsd_subslot: Some(1), ..DacConfig::default() };
        let host = Settings::for_device(config, &info(1, 2, "x"));
        assert_eq!(host.native_dsd_order(1, 2, true), (false, false));
        assert_eq!(host.native_dsd_policy(1, 2, 4), Some((1, false, false)));
    }

}

#[cfg(test)]
mod fallback_policy_tests {
    use super::*;
    #[test]
    fn nominal_default_is_only_full_speed_uac1_pcm_without_pipe() {
        let mut settings = Settings::default();
        assert!(settings.allow_nominal_uac1(true, true, true, 0));
        for args in [(false,true,true,0),(true,false,true,0),(true,true,false,0),(true,true,true,0x81)] {
            assert!(!settings.allow_nominal_uac1(args.0,args.1,args.2,args.3));
        }
        settings.config.async_fallback_policy = crate::config::AsyncFallbackPolicy::Forbid;
        assert!(!settings.allow_nominal_uac1(true,true,true,0));
    }
}

#[cfg(test)]
mod clock_readback_policy_tests {
    use super::*;
    #[test]
    fn only_acknowledged_unreadable_pcm_clock_can_use_fallback() {
        let mut settings = Settings::default();
        assert!(!settings.allow_unusable_clock_readback(true,true,None,false,false));
        settings.config.clock_readback_policy = crate::config::ClockReadbackPolicy::AllowUnusableAfterSuccessfulSet;
        assert!(settings.allow_unusable_clock_readback(true,true,None,false,false));
        assert!(settings.allow_unusable_clock_readback(true,true,Some(0),false,false));
        for (attempted, succeeded, reported, multiplied, dsd) in [
            (false,true,None,false,false), (true,false,None,false,false),
            (true,true,Some(44_100),false,false), (true,true,None,true,false),
            (true,true,None,false,true)] {
            assert!(!settings.allow_unusable_clock_readback(attempted,succeeded,reported,multiplied,dsd));
        }
        settings.quirks.push(Quirk::RequireVerifiedRate);
        assert!(!settings.allow_unusable_clock_readback(true,true,None,false,false));
    }
}

#[cfg(test)]
mod source_delay_tests {
    use super::*;
    use crate::config::QuirkOverride;
    #[test]
    fn host_delay_overrides_have_bounded_identity_and_deterministic_resolution() {
        let config = DacConfig { quirks: vec![QuirkOverride {vendor_id:1,product_id:2,name_contains:None,
            quirks:vec![Quirk::ControlMessageDelayUs(1_000),Quirk::ControlMessageDelayUs(5_000),Quirk::InterfaceDelayMs(50)]}], ..DacConfig::default() };
        let selected = Settings::for_device(config.clone(),&DeviceInfo::new(1,2,"DAC"));
        assert_eq!(selected.control_delay_us(),5_000);
        assert_eq!(selected.interface_delay_ms(),50);
        let other = Settings::for_device(config,&DeviceInfo::new(1,3,"DAC"));
        assert_eq!(other.control_delay_us(),0);
        assert_eq!(other.interface_delay_ms(),0);
    }
}
