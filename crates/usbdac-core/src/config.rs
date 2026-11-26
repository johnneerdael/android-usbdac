//! Configuration of a [`Dac`](crate::Dac) session.

use crate::error::{Error, Result};
use crate::quirk::Quirk;

/// What happens to the DAC's hardware volume when the [`Dac`](crate::Dac) is
/// closed.
///
/// The library records the level the DAC had the first time it read its volume
/// control (before it wrote anything), keyed by the device. This decides what is
/// done with that record on close.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HandBack {
    /// Write the recorded original level back to every channel, verify it, and
    /// end the record. Use this when the DAC goes back to another audio stack
    /// (for example Android's own USB audio driver).
    #[default]
    RestoreOriginal,
    /// Leave the DAC at its current level and keep the record, so a later
    /// [`Dac::open`](crate::Dac::open) of the same device in this process still
    /// knows the true original level. Use this when the host takes the DAC
    /// straight back.
    Keep,
}

/// Extra quirks for one VID/PID, added to the built-in table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuirkOverride {
    pub vendor_id: u16,
    pub product_id: u16,
    /// Only apply when the product name contains this text; `None` matches any
    /// name.
    pub name_contains: Option<String>,
    pub quirks: Vec<Quirk>,
}

/// Every tunable of a session. `DacConfig::default()` is what the library was
/// tuned with.
#[derive(Debug, Clone, PartialEq)]
pub struct DacConfig {
    /// Depth the host should keep the ring at: [`DacStream::writable_bytes`]
    /// reports room up to this much. Absorbs a stall upstream (storage,
    /// network, scheduling) shorter than this. Default 250 ms.
    ///
    /// [`DacStream::writable_bytes`]: crate::DacStream::writable_bytes
    pub buffer_target_ms: u32,
    /// Hard capacity of the ring; `write` never accepts more. Default 500 ms.
    pub buffer_capacity_ms: u32,
    /// How much audio the ring must hold before the stream starts, and before a
    /// flushed ring resumes. Never above the target. Default 100 ms.
    pub start_threshold_ms: u32,
    /// The recommended ring depth while software gain is not 1.0, so a host
    /// that processes audio itself sees changes sooner. Default 100 ms.
    pub processing_buffer_target_ms: u32,
    /// Refuse to stream unless the DAC confirms the sampling frequency it was
    /// told. Default `true`. Turning it off lets a DAC with a broken clock
    /// readback play, but the stream is then never reported bit-perfect.
    pub require_verified_rate: bool,
    /// Wait after programming the clock. `None` uses the device's quirk value,
    /// else 20 ms.
    pub clock_settle_delay_ms: Option<u64>,
    /// Native DSD byte order on the wire. `None` follows the device quirk
    /// (little-endian when there is none).
    pub dsd_big_endian: Option<bool>,
    /// Subslot size (1, 2 or 4) for native DSD alternate settings. `None`
    /// follows the device quirk, else the descriptor.
    pub dsd_subslot: Option<u8>,
    /// Prefer the 4-byte subslot for 24-bit PCM. `None` follows the device
    /// quirk (off when there is none).
    pub padded_24bit: Option<bool>,
    /// Quirks added to the built-in table for this session.
    pub quirks: Vec<QuirkOverride>,
    /// Ask for `SCHED_FIFO` (nice -16 as a fallback) on the output thread.
    /// Android only; ignored elsewhere. Default `true`.
    pub realtime_priority: bool,
    /// What to do with the hardware volume level on close.
    pub hand_back: HandBack,
}

impl Default for DacConfig {
    fn default() -> Self {
        Self {
            buffer_target_ms: 250,
            buffer_capacity_ms: 500,
            start_threshold_ms: 100,
            processing_buffer_target_ms: 100,
            require_verified_rate: true,
            clock_settle_delay_ms: None,
            dsd_big_endian: None,
            dsd_subslot: None,
            padded_24bit: None,
            quirks: Vec::new(),
            realtime_priority: true,
            hand_back: HandBack::default(),
        }
    }
}

impl DacConfig {
    /// Checks the combination of values.
    pub fn validate(&self) -> Result<()> {
        let bad = |message: &str| Err(Error::InvalidArgument(message.to_string()));
        if self.buffer_capacity_ms == 0 || self.buffer_capacity_ms > 10_000 {
            return bad("buffer_capacity_ms must be between 1 and 10000");
        }
        if self.buffer_target_ms == 0 || self.buffer_target_ms > self.buffer_capacity_ms {
            return bad("buffer_target_ms must be between 1 and buffer_capacity_ms");
        }
        if self.start_threshold_ms == 0 {
            return bad("start_threshold_ms must be at least 1");
        }
        if self.processing_buffer_target_ms == 0
            || self.processing_buffer_target_ms > self.buffer_capacity_ms
        {
            return bad("processing_buffer_target_ms must be between 1 and buffer_capacity_ms");
        }
        if let Some(subslot) = self.dsd_subslot {
            if !matches!(subslot, 1 | 2 | 4) {
                return bad("dsd_subslot must be 1, 2 or 4");
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_the_values_the_transport_was_tuned_with() {
        let config = DacConfig::default();
        assert_eq!(config.buffer_target_ms, 250);
        assert_eq!(config.buffer_capacity_ms, 500);
        assert_eq!(config.start_threshold_ms, 100);
        assert_eq!(config.processing_buffer_target_ms, 100);
        assert!(config.require_verified_rate);
        assert!(config.realtime_priority);
        assert_eq!(config.hand_back, HandBack::RestoreOriginal);
        config.validate().unwrap();
    }

    #[test]
    fn validation_rejects_inconsistent_depths() {
        let mut config = DacConfig::default();
        config.buffer_target_ms = 600;
        assert!(matches!(config.validate(), Err(Error::InvalidArgument(_))));
        let mut config = DacConfig::default();
        config.dsd_subslot = Some(3);
        assert!(matches!(config.validate(), Err(Error::InvalidArgument(_))));
        let mut config = DacConfig::default();
        config.buffer_capacity_ms = 0;
        assert!(config.validate().is_err());
    }
}
