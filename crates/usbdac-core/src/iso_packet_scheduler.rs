use crate::FeedbackSmoothing;

/// Default time the library keeps queued on the bus
/// ([`DacConfig::bus_queue_ms`](crate::DacConfig::bus_queue_ms)).
pub(crate) const DEFAULT_BUS_QUEUE_MS: u32 = 16;
/// Default number of isochronous transfers in flight
/// ([`DacConfig::transfers_in_flight`](crate::DacConfig::transfers_in_flight)).
pub(crate) const DEFAULT_TRANSFERS_IN_FLIGHT: u32 = 8;
/// Most packets one transfer carries.
pub(crate) const MAX_PACKETS_PER_TRANSFER: usize = 32;
/// Packets in flight never reach this count. On the one phone and DAC this was
/// measured on, 255 or fewer always played and exactly 256 always stalled.
pub(crate) const PACKET_FLIGHT_LIMIT: usize = 256;
/// Completed-transfer time after which a stream counts as stable.
const STABLE_WINDOW_US: u64 = 256_000;
/// Consecutive lossy transfers are bounded in time as well as in number: at
/// least this much audio must be lost in a row before the stream is stopped.
const MAX_CONSECUTIVE_LOSS_US: u64 = 6_000;
/// ...and never fewer transfers than this.
const MIN_CONSECUTIVE_LOSSY_URBS: u32 = 3;

/// How the isochronous queue is shaped: the same rule for every service
/// interval and every encoding, so the time queued on the bus is constant.
///
/// A transfer carries `bus_queue_ms / transfers_in_flight` of audio (2 ms by
/// default), rounded to whole service intervals, at least one packet and at
/// most [`MAX_PACKETS_PER_TRANSFER`]; `transfers_in_flight` of them are on the
/// bus (8 by default), so about `bus_queue_ms` is queued (16 ms by default):
///
/// | service interval | packets x transfers | queued |
/// |---|---|---|
/// | 125 us | 16 x 8 | 16 ms |
/// | 250 us | 8 x 8 | 16 ms |
/// | 500 us | 4 x 8 | 16 ms |
/// | 1000 us | 2 x 8 | 16 ms |
///
/// The packets in flight (`packets_per_transfer * transfer_slots`) never reach
/// [`PACKET_FLIGHT_LIMIT`]: when the configured depth would, the cap wins and
/// the transfers carry fewer packets (and less is queued).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct IsoQueueGeometry {
    pub(crate) packets_per_transfer: usize,
    pub(crate) transfer_slots: usize,
    pub(crate) stable_transfers: usize,
    pub(crate) service_interval_us: u32,
}

impl IsoQueueGeometry {
    /// Apply an optional application period at the negotiated USB cadence.
    /// Keep at least two slots so an ordinary completion leaves work queued.
    pub(crate) fn for_config(service_interval_us: u32, config: &crate::DacConfig) -> Self {
        let Some(period_us) = config.transfer_period_us else {
            return Self::for_stream(
                service_interval_us,
                config.bus_queue_ms,
                config.transfers_in_flight,
            );
        };
        let interval = service_interval_us.max(1);
        let target_packets = (config.bus_queue_ms * 1_000 / interval).max(2) as usize;
        let period_packets = (period_us / interval).max(1) as usize;
        let packets = period_packets
            .min(MAX_PACKETS_PER_TRANSFER)
            .min(target_packets / 2);
        let slots = (target_packets / packets)
            .min(config.transfers_in_flight as usize)
            .min((PACKET_FLIGHT_LIMIT - 1) / packets);
        Self::from_packets(interval, packets, slots)
    }

    pub(crate) fn for_stream(
        service_interval_us: u32,
        bus_queue_ms: u32,
        transfers_in_flight: u32,
    ) -> Self {
        let interval = u64::from(service_interval_us.max(1));
        let slots = u64::from(transfers_in_flight.max(1));
        let transfer_us = u64::from(bus_queue_ms) * 1_000 / slots;
        // Round to the nearest whole packet.
        let wanted = (transfer_us + interval / 2) / interval;
        let flight_cap = (PACKET_FLIGHT_LIMIT as u64 - 1) / slots;
        let packets = wanted
            .clamp(1, MAX_PACKETS_PER_TRANSFER as u64)
            .min(flight_cap.max(1));
        Self::from_packets(interval as u32, packets as usize, slots as usize)
    }

    fn from_packets(service_interval_us: u32, packets: usize, slots: usize) -> Self {
        let completed_us = packets as u64 * u64::from(service_interval_us);
        Self {
            packets_per_transfer: packets,
            transfer_slots: slots,
            stable_transfers: STABLE_WINDOW_US.div_ceil(completed_us).max(1) as usize,
            service_interval_us,
        }
    }

    /// The geometry the library was tuned with, for a service interval.
    #[cfg(test)]
    pub(crate) fn default_for(service_interval_us: u32) -> Self {
        Self::for_stream(
            service_interval_us,
            DEFAULT_BUS_QUEUE_MS,
            DEFAULT_TRANSFERS_IN_FLIGHT,
        )
    }

    /// Packets submitted and not yet completed when every slot is full.
    pub(crate) fn packets_in_flight(&self) -> usize {
        self.packets_per_transfer * self.transfer_slots
    }

    /// Audio one transfer carries, in microseconds.
    pub(crate) fn transfer_us(&self) -> u64 {
        self.packets_per_transfer as u64 * u64::from(self.service_interval_us)
    }

    /// Audio queued on the bus when every slot is full, in microseconds.
    pub(crate) fn queued_us(&self) -> u64 {
        self.packets_in_flight() as u64 * u64::from(self.service_interval_us)
    }

    /// How many lossy transfers in a row stop the stream: the same stretch of
    /// time whatever the geometry (never fewer than three transfers).
    pub(crate) fn max_consecutive_lossy_urbs(&self) -> u32 {
        let by_time = MAX_CONSECUTIVE_LOSS_US.div_ceil(self.transfer_us().max(1)) as u32;
        by_time.max(MIN_CONSECUTIVE_LOSSY_URBS)
    }
}

#[derive(Debug, Clone)]
pub(crate) struct IsoPacketScheduler {
    sample_rate: u32,
    verified_clock: bool,
    service_interval_us: u32,
    nominal_remainder: u64,
    bytes_per_frame: usize,
    packets_per_transfer: usize,
    feedback_frames_per_packet: Option<f64>,
    feedback_smoothing: FeedbackSmoothing,
    feedback_remainder: f64,
    packet_pattern: Option<Vec<usize>>,
}

impl IsoPacketScheduler {
    #[cfg(test)]
    pub(crate) fn new(sample_rate: u32, bytes_per_frame: usize, service_interval_us: u32) -> Self {
        Self::with_geometry(
            sample_rate,
            bytes_per_frame,
            service_interval_us,
            IsoQueueGeometry::default_for(service_interval_us),
        )
    }

    pub(crate) fn with_geometry(
        sample_rate: u32,
        bytes_per_frame: usize,
        service_interval_us: u32,
        geometry: IsoQueueGeometry,
    ) -> Self {
        Self {
            sample_rate,
            verified_clock: true,
            service_interval_us,
            nominal_remainder: 0,
            bytes_per_frame,
            packets_per_transfer: geometry.packets_per_transfer,
            feedback_frames_per_packet: None,
            feedback_smoothing: FeedbackSmoothing::Direct,
            feedback_remainder: 0.0,
            packet_pattern: None,
        }
    }

    pub(crate) fn with_verified_clock(mut self, verified: bool) -> Self {
        self.verified_clock = verified;
        self
    }

    /// Configure the dedicated explicit-feedback path without changing phase.
    /// Implicit timing continues to consume its measured packet vectors.
    pub(crate) fn with_explicit_feedback_smoothing(mut self, policy: FeedbackSmoothing) -> Self {
        self.feedback_smoothing = policy;
        self
    }

    pub(crate) fn explicit_feedback_smoothing(&self) -> FeedbackSmoothing {
        self.feedback_smoothing
    }

    pub(crate) fn applied_feedback_frames_per_packet(&self) -> Option<f64> {
        self.feedback_frames_per_packet
    }

    pub(crate) fn packets_per_transfer(&self) -> usize {
        self.packets_per_transfer
    }

    /// Commit one validated implicit vector; consumed exactly once.
    pub(crate) fn set_packet_pattern(&mut self, pattern: Vec<usize>) {
        assert!(self.packet_pattern.is_none());
        self.packet_pattern = Some(pattern);
    }

    pub(crate) fn next_transfer_packet_bytes(&mut self) -> Vec<usize> {
        if let Some(pattern) = self.packet_pattern.take() {
            return pattern;
        }
        (0..self.packets_per_transfer)
            .map(|_| self.next_packet_bytes())
            .collect()
    }

    /// Update packet timing from a device feedback report.
    ///
    /// Returns false when the report is non-finite, non-positive, or too far
    /// from nominal to be plausible. Permit bounded FIFO recentering (7/8 to
    /// 3/2 nominal); the transport separately enforces endpoint packet capacity.
    pub(crate) fn update_feedback_frames_per_packet(&mut self, frames_per_packet: f64) -> bool {
        if !self.feedback_rate_is_plausible(frames_per_packet) {
            return false;
        }

        self.feedback_frames_per_packet = Some(
            match (self.feedback_smoothing, self.feedback_frames_per_packet) {
                (FeedbackSmoothing::Ewma, Some(previous)) => {
                    previous + (frames_per_packet - previous) * 0.2
                }
                _ => frames_per_packet,
            },
        );
        true
    }

    /// Validate without changing the selected rate or fractional phase while scanning a batch.
    pub(crate) fn feedback_rate_is_plausible(&self, frames_per_packet: f64) -> bool {
        if !frames_per_packet.is_finite() || frames_per_packet <= 0.0 {
            return false;
        }

        let nominal = self.nominal_frames_per_packet().max(0.001);
        let (lower, upper) = if self.verified_clock {
            (0.875, 1.5)
        } else {
            (0.97, 1.03)
        };
        !(frames_per_packet < nominal * lower || frames_per_packet > nominal * upper)
    }

    pub(crate) fn lock_to_nominal_packet_timing(&mut self) {
        self.feedback_frames_per_packet = None;
        self.feedback_remainder = 0.0;
    }

    pub(crate) fn nominal_frames_per_packet(&self) -> f64 {
        self.sample_rate as f64 * self.service_interval_us as f64 / 1_000_000.0
    }

    fn next_packet_bytes(&mut self) -> usize {
        let frames = if let Some(feedback_frames_per_packet) = self.feedback_frames_per_packet {
            self.feedback_remainder += feedback_frames_per_packet;
            let frames = self.feedback_remainder.floor() as usize;
            self.feedback_remainder -= frames as f64;
            frames
        } else {
            let total = self.nominal_remainder
                + (self.sample_rate as u64 * self.service_interval_us as u64);
            let frames = (total / 1_000_000) as usize;
            self.nominal_remainder = total % 1_000_000;
            frames
        };
        frames.saturating_mul(self.bytes_per_frame)
    }
}

/// Change only active slot count: packet cadence and in-flight payload stay intact.
#[derive(Debug)]
pub(crate) struct AdaptiveQueue {
    pub(crate) slots: usize,
    pub(crate) max_slots: usize,
    baseline: usize,
    window_ms: u64,
    losses: u32,
    last_loss_ms: u64,
}
impl AdaptiveQueue {
    pub(crate) fn new(geometry: IsoQueueGeometry, enabled: bool) -> Self {
        let baseline = geometry.transfer_slots;
        let max_slots = if enabled {
            16usize
                .min((PACKET_FLIGHT_LIMIT - 1) / geometry.packets_per_transfer)
                .min((64_000 / geometry.transfer_us().max(1)) as usize)
                .max(baseline)
        } else {
            baseline
        };
        Self {
            slots: baseline,
            max_slots,
            baseline,
            window_ms: 0,
            losses: 0,
            last_loss_ms: 0,
        }
    }
    pub(crate) fn observe(&mut self, now_ms: u64, lossy_completions: u32) -> bool {
        let previous = self.slots;
        if lossy_completions > 0 {
            if now_ms.saturating_sub(self.window_ms) > 1_000 {
                self.losses = 0;
                self.window_ms = now_ms;
            }
            self.last_loss_ms = now_ms;
            self.losses = self.losses.saturating_add(lossy_completions);
            if self.losses >= 1 {
                // Count coalesced losses, but grow only once per worker batch.
                self.slots = (self.slots + 2).min(self.max_slots);
                self.losses = 0;
            }
        } else if self.slots > self.baseline && now_ms.saturating_sub(self.last_loss_ms) >= 30_000 {
            self.slots -= 1;
            self.last_loss_ms = now_ms;
        }
        previous != self.slots
    }
}

#[cfg(test)]
mod adaptive_tests {
    use super::*;
    #[test]
    fn loss_bursts_deepen_and_quiet_periods_shrink_without_changing_packets() {
        let g = IsoQueueGeometry::default_for(125);
        let mut queue = AdaptiveQueue::new(g, true);
        queue.observe(20, 1);
        assert_eq!(queue.slots, 10);
        assert!(!queue.observe(29_999, 0));
        assert!(queue.observe(30_020, 0));
        assert_eq!(queue.slots, 9);
        for at in 0..100 {
            queue.observe(30_021 + at, 1);
        }
        assert_eq!(queue.slots, 15);
        assert!(queue.slots * g.packets_per_transfer < PACKET_FLIGHT_LIMIT);
    }
    #[test]
    fn unverified_clock_rejects_adjacent_rate_family() {
        let scheduler = IsoPacketScheduler::new(44_100, 4, 1000).with_verified_clock(false);
        assert!(scheduler.feedback_rate_is_plausible(44.1));
        assert!(!scheduler.feedback_rate_is_plausible(48.0));
        let verified = scheduler.with_verified_clock(true);
        assert!(verified.feedback_rate_is_plausible(48.0));
    }
    #[test]
    fn fixed_policy_and_capacity_cannot_deepen() {
        let g = IsoQueueGeometry::default_for(125);
        let mut queue = AdaptiveQueue::new(g, false);
        for at in 0..100 {
            assert!(!queue.observe(at, 1));
        }
        assert_eq!(queue.slots, g.transfer_slots);
    }
    #[test]
    fn three_lossy_completions_coalesced_in_one_batch_deepen_once() {
        let geometry = IsoQueueGeometry::default_for(125);
        let mut queue = AdaptiveQueue::new(geometry, true);
        let losses = [true, false, true, false, true]
            .into_iter()
            .fold(0u32, |count, lossy| count.saturating_add(u32::from(lossy)));
        assert!(queue.observe(100, losses));
        assert_eq!(queue.slots, geometry.transfer_slots + 2);
        assert!(!queue.observe(30_099, 0));
        assert!(queue.observe(30_100, 0));
        assert_eq!(queue.slots, geometry.transfer_slots + 1);
        assert!(queue.observe(30_101, u32::MAX));
        assert_eq!(
            queue.slots,
            geometry.transfer_slots + 3,
            "one growth even for a saturated batch count"
        );
        assert!(queue.slots * geometry.packets_per_transfer < PACKET_FLIGHT_LIMIT);
        let mut fixed = AdaptiveQueue::new(geometry, false);
        assert!(!fixed.observe(100, losses));
        assert_eq!(fixed.slots, geometry.transfer_slots);
    }
}

/// Host starvation observation only; a host ring cannot measure the DAC FIFO.
#[derive(Default)]
pub(crate) struct NominalStarvation {
    empty_since_ms: Option<u64>,
    reported: bool,
}
impl NominalStarvation {
    pub(crate) fn observe(&mut self, now_ms: u64, buffered: usize, active: bool) -> Option<u64> {
        if !active || buffered > 0 {
            self.empty_since_ms = None;
            self.reported = false;
            return None;
        }
        let since = *self.empty_since_ms.get_or_insert(now_ms);
        if !self.reported && now_ms.saturating_sub(since) >= 5_000 {
            self.reported = true;
            return Some(now_ms.saturating_sub(since));
        }
        None
    }
}
#[cfg(test)]
mod nominal_starvation_tests {
    use super::*;
    #[test]
    fn topped_up_producer_is_never_called_drift_or_starvation() {
        let mut guard = NominalStarvation::default();
        for ms in (0..120_000).step_by(2) {
            assert!(guard.observe(ms, 48_000, true).is_none());
        }
        assert!(guard.observe(120_001, 0, true).is_none());
        assert!(guard.observe(125_000, 0, true).is_none());
        assert_eq!(guard.observe(125_001, 0, true), Some(5_000));
        assert!(guard.observe(130_001, 0, true).is_none());
        assert!(guard.observe(130_002, 1, true).is_none());
        assert!(guard.observe(130_003, 0, true).is_none());
        assert_eq!(guard.observe(135_003, 0, true), Some(5_000));
        assert!(guard.observe(135_004, 0, false).is_none());
        assert!(guard.observe(140_000, 0, true).is_none());
        assert_eq!(guard.observe(145_001, 0, true), Some(5_001));
    }
}
