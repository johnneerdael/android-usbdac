//! Configuration of a [`Dac`](crate::Dac) session.

/// Policy for asynchronous endpoints without a proven feedback source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AsyncFallbackPolicy {
    /// Admit only full-speed UAC1 PCM with no declared feedback pipe.
    #[default]
    FullSpeedUac1,
    /// Require explicit or proven implicit feedback.
    Forbid,
}

/// Policy for unusable frequency readback after an acknowledged rate write.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ClockReadbackPolicy {
    #[default]
    RequireVerified,
    /// Continue unverified after three unusable reads; never hide a mismatch.
    AllowUnusableAfterSuccessfulSet,
}

/// How validated explicit feedback reports change packet timing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FeedbackSmoothing {
    /// Apply each accepted report directly, without averaging or rate rounding.
    #[default]
    Direct,
    /// Legacy filter: first report directly, then old + 0.2 * (report - old).
    Ewma,
}

impl FeedbackSmoothing {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Direct => "direct",
            Self::Ewma => "ewma",
        }
    }
}

use crate::error::{Error, Result};
use crate::iso_packet_scheduler::{DEFAULT_BUS_QUEUE_MS, DEFAULT_TRANSFERS_IN_FLIGHT};
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

/// What [`Dac::close_with`](crate::Dac::close_with) does with the level the DAC
/// had before the library first changed it, decided at close rather than at
/// open.
///
/// [`HandBack`] is fixed in the [`DacConfig`] when the DAC opens. A host that
/// only learns at the end what happens next (Android's own driver gets the DAC,
/// the host takes it straight back, the DAC was unplugged) closes with this.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseVolume {
    /// Write the recorded original level back, verify it, and end the record.
    Restore,
    /// Leave the DAC at its current level and keep the record, so a later
    /// [`Dac::open`](crate::Dac::open) of the same device in this process still
    /// knows the true original level.
    Keep,
    /// End the record without touching the DAC: it is gone, so a later
    /// attachment of the same device starts with a fresh record.
    Forget,
}

/// What [`Dac::open`](crate::Dac::open) does with the DAC's hardware volume
/// before any stream can start.
///
/// A DAC keeps the level it had, and many default to full scale. The first
/// audio would then play at whatever that is.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum StartupVolume {
    /// Lower every controlled channel to `min(its level, max_db)` and verify it
    /// by reading it back. A channel is never raised, and one that is already
    /// quieter (or at the device's silent value) is left alone. `max_db` is
    /// rounded down to a step the DAC supports; below the DAC's minimum the
    /// channels go to the minimum and the report says so. The level each
    /// channel had is recorded first, so [`HandBack::RestoreOriginal`]
    /// restores it. If the attenuation cannot be verified the volume control is
    /// treated as unusable (see
    /// [`DacConfig::require_hardware_volume`](DacConfig::require_hardware_volume)).
    Attenuate { max_db: f64 },
    /// Leave the DAC's level alone.
    Keep,
}

impl Default for StartupVolume {
    /// `Attenuate { max_db: -40.0 }`.
    fn default() -> Self {
        Self::Attenuate { max_db: -40.0 }
    }
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
    /// Apply supported exact-identity ALSA facts only when explicitly selected.
    pub quirk_policy: crate::quirk::QuirkPolicy,
    /// Allow streaming an asynchronous endpoint that has no explicit feedback
    /// source, beyond the narrow default [`AsyncFallbackPolicy`] tier.
    /// Otherwise `negotiate` and `open_stream` refuse it with
    /// [`Error::ImplicitFeedbackUnsupported`]. Allowed, it streams at the nominal
    /// rate, host-paced, and any drift between the DAC's clock and the host's
    /// timing ends in underruns or overruns on the DAC; the stream is never
    /// bit-perfect (`DacStream::bit_perfect_blocker` says why). Default `false`.
    pub allow_unsynchronized_async: bool,
    /// Narrow default fallback; never bit-perfect.
    pub async_fallback_policy: AsyncFallbackPolicy,
    /// Permit the unverified clock tier only after an acknowledged SET.
    pub clock_readback_policy: ClockReadbackPolicy,
    /// Adapt active URB count to loss, bounded by 64 ms and 255 packets.
    pub adaptive_queue: bool,
    /// Disable explicit and implicit feedback for diagnosis. Default `false`.
    /// Asynchronous nominal playback still requires `allow_unsynchronized_async`.
    pub disable_feedback: bool,
    /// Explicit feedback timing policy. Direct by default; implicit cadence
    /// vectors and nominal pacing are unaffected. Applied when a stream opens.
    pub feedback_smoothing: FeedbackSmoothing,
    /// Retain a bounded USB feedback support trace, including raw explicit
    /// feedback bytes. Ask the listener to enable it before opening a session.
    pub capture_feedback_trace: bool,
    /// Ask for `SCHED_FIFO` (nice -16 as a fallback) on the output thread.
    /// Android only; ignored elsewhere. Default `true`.
    pub realtime_priority: bool,
    /// What to do with the hardware volume level on close.
    pub hand_back: HandBack,
    /// How much audio, in milliseconds, is kept queued on the USB bus: the
    /// transfers in flight together carry about this much. The same for every
    /// service interval and encoding. It is what a pause or a flush still plays
    /// out, and the headroom against a late output thread. 4 to 64, default 16. A transfer carries
    /// `bus_queue_ms / transfers_in_flight` of audio (2 ms by default), at most
    /// 32 packets, and the packets in flight never reach 256 (255 at most):
    /// when the configured depth would, that cap wins and less is queued.
    pub bus_queue_ms: u32,
    /// How many isochronous transfers are in flight at once. 2 to 16,
    /// default 8. See [`bus_queue_ms`](Self::bus_queue_ms).
    pub transfers_in_flight: u32,
    /// Optional target for one transfer in microseconds. 125 to 32,000.
    /// When set, the worker rounds down to whole USB service intervals (at
    /// least one), derives the slot count from the bus queue target and uses
    /// `transfers_in_flight` as a ceiling. Keep two slots even when one service
    /// interval exceeds the requested period. None uses the fixed slot count.
    pub transfer_period_us: Option<u32>,
    /// Cancel the transfers already on the bus when the stream is flushed, so
    /// the audio of the old position is not heard after a seek or a track
    /// change. The output thread cancels them, waits for the host controller to
    /// hand every one back, and resumes with wire-correct silence until the
    /// ring has refilled. Cancelled transfers are not lost audio, not
    /// underruns, do not clear bit-perfect, and their music is not counted as
    /// played. `pause` never cancels. `false` lets the queue play out (up to
    /// `bus_queue_ms`). Default `true`.
    pub cancel_in_flight_on_flush: bool,
    /// What `Dac::open` does with the DAC's hardware volume before any stream
    /// can start. Default: lower it to at most -40 dB.
    pub startup_volume: StartupVolume,
    /// `open_stream` fails with [`Error::VolumeUnavailable`] when the DAC has no
    /// usable hardware volume control, so the host can ask the user to allow
    /// fixed-level output and open the DAC again with this off. `negotiate` and
    /// `capabilities` work either way. Default `true`.
    pub require_hardware_volume: bool,
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
            quirk_policy: crate::quirk::QuirkPolicy::VerifiedOnly,
            allow_unsynchronized_async: false,
            async_fallback_policy: AsyncFallbackPolicy::default(),
            clock_readback_policy: ClockReadbackPolicy::default(),
            adaptive_queue: true,
            disable_feedback: false,
            feedback_smoothing: FeedbackSmoothing::Direct,
            capture_feedback_trace: false,
            realtime_priority: true,
            hand_back: HandBack::default(),
            bus_queue_ms: DEFAULT_BUS_QUEUE_MS,
            transfers_in_flight: DEFAULT_TRANSFERS_IN_FLIGHT,
            transfer_period_us: None,
            cancel_in_flight_on_flush: true,
            startup_volume: StartupVolume::default(),
            require_hardware_volume: true,
        }
    }
}

/// Starting points for USB bus queue tuning. Queue times are approximate and
/// depend on the endpoint's service interval. The software ring is independent.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum UsbQueueProfile {
    /// Shorter USB queue and smaller transfers for interactive playback.
    LowLatency,
    /// The default queue and transfer count.
    #[default]
    Balanced,
    /// Same target queue as Balanced with fewer, larger transfers and wakeups.
    LowCpu,
}

impl DacConfig {
    /// Apply the USB queue profile. All other session settings are retained.
    pub fn with_usb_queue_profile(mut self, profile: UsbQueueProfile) -> Self {
        (
            self.bus_queue_ms,
            self.transfers_in_flight,
            self.transfer_period_us,
        ) = match profile {
            UsbQueueProfile::LowLatency => (8, 8, Some(1_000)),
            UsbQueueProfile::Balanced => (DEFAULT_BUS_QUEUE_MS, DEFAULT_TRANSFERS_IN_FLIGHT, None),
            UsbQueueProfile::LowCpu => (16, 4, None),
        };
        self
    }

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
        if let StartupVolume::Attenuate { max_db } = self.startup_volume {
            if !max_db.is_finite() || !(-128.0..=0.0).contains(&max_db) {
                return bad("startup_volume max_db must be a level between -128 and 0 dB");
            }
        }
        if !(4..=64).contains(&self.bus_queue_ms) {
            return bad("bus_queue_ms must be between 4 and 64");
        }
        if !(2..=16).contains(&self.transfers_in_flight) {
            return bad("transfers_in_flight must be between 2 and 16");
        }
        if self
            .transfer_period_us
            .is_some_and(|period| !(125..=32_000).contains(&period))
        {
            return bad("transfer_period_us must be between 125 and 32000");
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
        assert_eq!(config.feedback_smoothing, FeedbackSmoothing::Direct);
        assert!(!config.allow_unsynchronized_async);
        assert_eq!(config.hand_back, HandBack::RestoreOriginal);
        assert_eq!(config.bus_queue_ms, 16);
        assert_eq!(config.transfers_in_flight, 8);
        assert!(config.cancel_in_flight_on_flush);
        assert_eq!(
            config.startup_volume,
            StartupVolume::Attenuate { max_db: -40.0 }
        );
        assert!(config.require_hardware_volume);
        config.validate().unwrap();
    }

    #[test]
    fn validation_rejects_inconsistent_depths() {
        let config = DacConfig {
            buffer_target_ms: 600,
            ..DacConfig::default()
        };
        assert!(matches!(config.validate(), Err(Error::InvalidArgument(_))));
        let config = DacConfig {
            dsd_subslot: Some(3),
            ..DacConfig::default()
        };
        assert!(matches!(config.validate(), Err(Error::InvalidArgument(_))));
        let config = DacConfig {
            buffer_capacity_ms: 0,
            ..DacConfig::default()
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn the_startup_ceiling_must_be_a_level_a_dac_can_have() {
        for (startup_volume, ok) in [
            (StartupVolume::Keep, true),
            (StartupVolume::Attenuate { max_db: -40.0 }, true),
            (StartupVolume::Attenuate { max_db: 0.0 }, true),
            (StartupVolume::Attenuate { max_db: -128.0 }, true),
            (StartupVolume::Attenuate { max_db: 3.0 }, false),
            (StartupVolume::Attenuate { max_db: -200.0 }, false),
            (StartupVolume::Attenuate { max_db: f64::NAN }, false),
            (
                StartupVolume::Attenuate {
                    max_db: f64::NEG_INFINITY,
                },
                false,
            ),
        ] {
            let config = DacConfig {
                startup_volume,
                ..DacConfig::default()
            };
            assert_eq!(config.validate().is_ok(), ok, "{startup_volume:?}");
        }
        let error = DacConfig {
            startup_volume: StartupVolume::Attenuate { max_db: 6.0 },
            ..DacConfig::default()
        }
        .validate()
        .unwrap_err();
        assert!(matches!(&error, Error::InvalidArgument(message) if message.contains("max_db")));
    }

    #[test]
    fn the_bus_queue_is_validated_at_both_ends_of_its_range() {
        for (bus_queue_ms, ok) in [
            (0, false),
            (3, false),
            (4, true),
            (16, true),
            (64, true),
            (65, false),
        ] {
            let config = DacConfig {
                bus_queue_ms,
                ..DacConfig::default()
            };
            assert_eq!(config.validate().is_ok(), ok, "bus_queue_ms={bus_queue_ms}");
        }
        for (transfers_in_flight, ok) in [
            (0, false),
            (1, false),
            (2, true),
            (8, true),
            (16, true),
            (17, false),
        ] {
            let config = DacConfig {
                transfers_in_flight,
                ..DacConfig::default()
            };
            assert_eq!(
                config.validate().is_ok(),
                ok,
                "transfers_in_flight={transfers_in_flight}"
            );
        }
        let error = DacConfig {
            bus_queue_ms: 3,
            ..DacConfig::default()
        }
        .validate()
        .unwrap_err();
        assert!(
            matches!(&error, Error::InvalidArgument(message) if message.contains("bus_queue_ms"))
        );
    }
}
