//! A successful UAC1 rate-word OUT proves delivery, not the ADC's clock.
//! Measure only IN transfers submitted after that delivery and clock recovery.
use super::*;
use crate::capture::CapturePacket;
use capture_stream::CaptureEndpoint;

const WINDOW_US: u64 = 100_000;

pub(super) struct CaptureRateProof {
    rate: u32,
    packets: usize,
    service_us: u32,
    started: Instant,
    latest: Option<Instant>,
    last_proof: Option<Instant>,
    startup: Duration,
    recovery: Duration,
    stale: Duration,
    frames: u64,
    elapsed_us: u64,
    qualified: bool,
    ever_qualified: bool,
    bad_windows: u8,
}

impl CaptureRateProof {
    pub(super) fn for_endpoint(
        endpoint: &CaptureEndpoint,
        geometry: IsoQueueGeometry,
        delay: Option<endpoint_lock::LockDelay>,
    ) -> Option<Self> {
        endpoint.requires_host_rate_proof().then(|| {
            Self::new(endpoint.format.sample_rate(), geometry, delay)
                .with_initial_refresh(endpoint.standard_feedforward)
        })
    }

    fn new(rate: u32, geometry: IsoQueueGeometry, delay: Option<endpoint_lock::LockDelay>) -> Self {
        let queue_us = geometry.queued_us().saturating_mul(4);
        let delay_us = match delay {
            Some(endpoint_lock::LockDelay::Milliseconds(ms)) => u64::from(ms) * 1_000,
            Some(endpoint_lock::LockDelay::PcmSampleFrames(frames)) => {
                (u64::from(frames) * 1_000_000).div_ceil(u64::from(rate.max(1)))
            }
            None => 0,
        };
        Self {
            rate,
            packets: geometry.packets_per_transfer,
            service_us: geometry.service_interval_us,
            started: Instant::now(),
            latest: None,
            last_proof: None,
            startup: Duration::from_micros(500_000u64.max(queue_us).saturating_add(delay_us)),
            recovery: Duration::from_micros(500_000u64.max(queue_us)),
            stale: Duration::from_micros(WINDOW_US.max(queue_us)),
            frames: 0,
            elapsed_us: 0,
            qualified: false,
            ever_qualified: false,
            bad_windows: 0,
        }
    }

    fn with_initial_refresh(mut self, pipe: Option<adc3::FeedforwardEndpoint>) -> Self {
        if let Some(pipe) = pipe {
            // The validated UAC1 pipe sends a word every 2^bRefresh service
            // slots. Allow one such update period for initial convergence.
            let period = pipe
                .refresh
                .filter(|refresh| (1..=9).contains(refresh))
                .map_or(1, |refresh| 1u64 << refresh);
            self.startup += Duration::from_micros(u64::from(pipe.service_us) * period);
        }
        self
    }

    pub(super) fn qualified(&self) -> bool {
        self.qualified
    }

    pub(super) fn check(&self, now: Instant) -> Result<(), Error> {
        let proof_age = self.last_proof.unwrap_or(self.started);
        let allowance = if self.ever_qualified {
            self.recovery
        } else {
            self.startup
        };
        if now.saturating_duration_since(proof_age) >= allowance
            || self
                .latest
                .is_some_and(|latest| now.saturating_duration_since(latest) >= self.stale)
        {
            return Err(self.unverified());
        }
        Ok(())
    }

    fn unverified(&self) -> Error {
        Error::ClockUnverified {
            requested_hz: self.rate,
            reported_hz: None,
        }
    }

    fn suppress(&mut self) {
        self.frames = 0;
        self.elapsed_us = 0;
        self.qualified = false;
    }

    pub(super) fn observe(
        &mut self,
        endpoint: &CaptureEndpoint,
        slot: &IsoTransferSlot,
        status: i32,
        completed: Instant,
        scratch: &mut [CapturePacket],
    ) -> Result<bool, Error> {
        // Inspect removal before expiry so an unplug remains DeviceLost.
        let (_, detached) = match capture_stream::inspect_slot(slot, status, scratch) {
            Ok(value) => value,
            Err(error) => {
                self.suppress();
                return Err(error);
            }
        };
        if detached {
            self.suppress();
            return Err(Error::DeviceLost);
        }
        self.check(completed)?;
        let stride = endpoint.format.bytes_per_frame();
        if slot.submitted_endpoint != endpoint.address
            || endpoint.address & 0x80 == 0
            || endpoint.format.sample_rate() != self.rate
            || endpoint.service_us != self.service_us
            || scratch.len() != self.packets
            || slot.packet_sizes.len() != self.packets
            || stride == 0
            || endpoint.packet_bytes == 0
            || slot.submitted_at.map_or(true, |submitted| {
                submitted > completed
                    || completed.saturating_duration_since(submitted) >= self.stale
            })
            || slot
                .packet_sizes
                .iter()
                .any(|size| *size != endpoint.packet_bytes)
            || slot
                .packet_sizes
                .iter()
                .try_fold(0usize, |sum, size| sum.checked_add(*size))
                != Some(slot.submitted_bytes)
        {
            self.suppress();
            return Err(Error::Transfer(
                "UAC1 capture rate proof geometry changed".into(),
            ));
        }
        self.observe_packets(scratch, stride, endpoint.packet_bytes, completed)
    }

    fn observe_packets(
        &mut self,
        packets: &[CapturePacket],
        stride: usize,
        capacity: usize,
        completed: Instant,
    ) -> Result<bool, Error> {
        // Empty service intervals are real elapsed USB time at low rates.
        // Reject the entire completion before counting or publishing any part.
        if packets.iter().any(|packet| {
            !packet.succeeded
                || packet.requested_bytes != capacity
                || packet.actual_bytes > packet.requested_bytes
                || packet.actual_bytes % stride != 0
        }) {
            self.suppress();
            return Ok(false);
        }
        self.latest = Some(
            self.latest
                .map_or(completed, |latest| latest.max(completed)),
        );
        let publish = self.qualified;
        self.frames += packets
            .iter()
            .map(|packet| (packet.actual_bytes / stride) as u64)
            .sum::<u64>();
        self.elapsed_us += u64::from(self.service_us) * packets.len() as u64;
        if self.elapsed_us >= WINDOW_US {
            let measured = u128::from(self.frames) * 1_000_000;
            let expected = u128::from(self.rate) * u128::from(self.elapsed_us);
            let valid = self.frames != 0 && measured.abs_diff(expected) <= 2_000_000;
            self.frames = 0;
            self.elapsed_us = 0;
            if !valid {
                self.qualified = false;
                self.bad_windows = self.bad_windows.saturating_add(1);
                if self.ever_qualified && self.bad_windows >= 2 {
                    return Err(self.unverified());
                }
                return Ok(false);
            }
            self.bad_windows = 0;
            self.qualified = true;
            self.ever_qualified = true;
            self.last_proof = Some(completed);
        }
        // The qualifying window itself must never enter the capture buffer.
        Ok(publish)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn packet(frames: usize) -> CapturePacket {
        CapturePacket {
            requested_bytes: 400,
            actual_bytes: frames * 4,
            succeeded: true,
        }
    }

    fn window(proof: &mut CaptureRateProof, frames: usize, now: Instant) -> Result<bool, Error> {
        let mut publish = false;
        for _ in 0..100 {
            publish = proof.observe_packets(&[packet(frames)], 4, 400, now)?;
        }
        Ok(publish)
    }

    #[test]
    fn confirms_measured_rate_and_discards_the_qualifying_window() {
        let mut proof = CaptureRateProof::new(48_000, IsoQueueGeometry::default_for(1_000), None);
        let now = proof.started;
        assert!(!window(&mut proof, 48, now).unwrap());
        assert!(proof.qualified());
        assert!(window(&mut proof, 48, now).unwrap());
        assert!(!window(&mut proof, 44, now).unwrap());
        assert!(!proof.qualified());
        assert!(!window(&mut proof, 48, now).unwrap());
        assert!(proof.qualified());
    }

    #[test]
    fn wrong_and_empty_windows_allow_startup_but_never_prime() {
        for frames in [0, 44] {
            let mut proof =
                CaptureRateProof::new(48_000, IsoQueueGeometry::default_for(1_000), None);
            let now = proof.started;
            assert!(!window(&mut proof, frames, now).unwrap());
            assert!(!proof.qualified());
            assert!(!window(&mut proof, frames, now).unwrap());
            assert!(matches!(
                proof.check(now + proof.startup),
                Err(Error::ClockUnverified { .. })
            ));
        }
    }

    #[test]
    fn two_wrong_windows_after_qualification_terminate() {
        let mut proof = CaptureRateProof::new(48_000, IsoQueueGeometry::default_for(1_000), None);
        let now = proof.started;
        window(&mut proof, 48, now).unwrap();
        assert!(!window(&mut proof, 44, now).unwrap());
        assert!(matches!(
            window(&mut proof, 44, now),
            Err(Error::ClockUnverified { .. })
        ));
    }

    #[test]
    fn low_rates_count_empty_intervals_and_malformed_completion_requires_new_proof() {
        let mut proof = CaptureRateProof::new(500, IsoQueueGeometry::default_for(1_000), None);
        let now = proof.started;
        for _ in 0..50 {
            assert!(!proof
                .observe_packets(&[packet(0), packet(1)], 4, 400, now)
                .unwrap());
        }
        assert!(proof.qualified());
        let invalid = CapturePacket {
            actual_bytes: 3,
            ..packet(1)
        };
        assert!(!proof.observe_packets(&[invalid], 4, 400, now).unwrap());
        assert!(!proof.qualified());
        for _ in 0..50 {
            proof
                .observe_packets(&[packet(0), packet(1)], 4, 400, now)
                .unwrap();
        }
        assert!(proof.qualified());
    }

    #[test]
    fn startup_includes_declared_delay_and_extreme_queue_and_proof_expires() {
        let geometry = IsoQueueGeometry::for_stream(16_000, 4_000, 1);
        let proof = CaptureRateProof::new(
            500,
            geometry,
            Some(endpoint_lock::LockDelay::PcmSampleFrames(501)),
        );
        assert_eq!(
            proof.startup,
            Duration::from_micros(geometry.queued_us() * 4 + 1_002_000)
        );
        assert!(proof
            .check(proof.started + proof.startup - Duration::from_micros(1))
            .is_ok());
        assert!(proof.check(proof.started + proof.startup).is_err());
        let mut proof = CaptureRateProof::new(48_000, IsoQueueGeometry::default_for(1_000), None);
        let now = proof.started;
        window(&mut proof, 48, now).unwrap();
        assert!(proof.check(now + proof.stale).is_err());
        let millis = CaptureRateProof::new(
            48_000,
            IsoQueueGeometry::default_for(1_000),
            Some(endpoint_lock::LockDelay::Milliseconds(600)),
        );
        assert_eq!(millis.startup, Duration::from_millis(1_100));
        let mut refresh = CaptureRateProof::new(
            48_000,
            IsoQueueGeometry::default_for(1_000),
            Some(endpoint_lock::LockDelay::Milliseconds(600)),
        )
        .with_initial_refresh(Some(adc3::FeedforwardEndpoint {
            address: 1,
            interval: 1,
            service_us: 1_000,
            bytes: 3,
            full_speed: true,
            refresh: Some(9),
        }));
        assert_eq!(refresh.startup, Duration::from_millis(1_612));
        assert_eq!(refresh.recovery, Duration::from_millis(500));
        let now = refresh.started;
        window(&mut refresh, 48, now).unwrap();
        // Keep callbacks recent to isolate proof expiry from cadence expiry.
        refresh.latest = Some(now + refresh.recovery);
        assert!(refresh.check(now + refresh.recovery).is_err());
    }
}
