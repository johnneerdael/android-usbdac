//! A lone host-timing loss: one isochronous transfer the host put on the bus
//! too late, told apart from a DAC or driver failure.
//!
//! An isochronous OUT packet the host completes with zero length was never
//! transmitted: the device cannot refuse one, so the scheduled frame passed
//! before the transfer reached the controller. When the ring still held audio
//! (no underrun), nothing timed out, no packet was short, and the loss fits in
//! one transfer, the evidence points at a late user-space worker or a late
//! completion on the host, not at the DAC, its clock or the payload. The case
//! still records the loss; only a single such incident in a run is accepted
//! (see `crate::required_fallbacks_verified`).

use serde_json::{json, Value};
use usbdac_core::{Encoding, StreamStats};

/// The stream counters the classification reads, in
/// `StreamFormat::counter_rate_hz` frames.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct LossCounters {
    pub lost_frames: u64,
    pub zero_packets: u64,
    pub short_packets: u64,
    pub underruns: u64,
    pub urb_timeouts: u64,
    pub frames_written: u64,
    pub played_frames: u64,
    pub adaptive_queue_slots: u64,
    pub adaptive_queue_us: u64,
    pub adaptive_queue_changes: u64,
    pub max_turnaround_us: u64,
    pub max_worker_gap_us: u64,
    pub worker_gap_over_10ms: u64,
}

impl From<&StreamStats> for LossCounters {
    fn from(stats: &StreamStats) -> Self {
        Self {
            lost_frames: stats.lost_frames,
            zero_packets: stats.zero_packets,
            short_packets: stats.short_packets,
            underruns: stats.underruns,
            urb_timeouts: stats.urb_timeouts,
            frames_written: stats.frames_written,
            played_frames: stats.played_frames,
            adaptive_queue_slots: stats.adaptive_queue_slots,
            adaptive_queue_us: stats.adaptive_queue_us,
            adaptive_queue_changes: stats.adaptive_queue_changes,
            max_turnaround_us: stats.max_turnaround_us,
            max_worker_gap_us: stats.max_worker_gap_us,
            worker_gap_over_10ms: stats.worker_gap_over_10ms,
        }
    }
}

/// Most incidents a run may accept and still pass.
pub(crate) const MAX_ACCEPTED_INCIDENTS: usize = 1;

/// Evidence for a lone host-timing loss in `stats`, or `None` when the loss
/// has any other shape. `sample_rate_hz` is the wire frame rate;
/// `service_interval_us` the data endpoint's packet interval. Native DSD
/// counts ring frames in other units, so it is never classified.
pub(crate) fn lone_host_timing_loss(
    stats: &LossCounters,
    encoding: Encoding,
    sample_rate_hz: u32,
    service_interval_us: u32,
) -> Option<Value> {
    if matches!(encoding, Encoding::DsdNative { .. })
        || stats.lost_frames == 0
        || stats.zero_packets == 0
        || stats.short_packets != 0
        || stats.underruns != 0
        || stats.urb_timeouts != 0
        || stats.played_frames.checked_add(stats.lost_frames) != Some(stats.frames_written)
        || stats.adaptive_queue_slots == 0
        || service_interval_us == 0
        || sample_rate_hz == 0
    {
        return None;
    }
    let transfer_us = stats.adaptive_queue_us / stats.adaptive_queue_slots;
    let packets_per_transfer = transfer_us / u64::from(service_interval_us);
    if transfer_us == 0 || packets_per_transfer == 0 {
        return None;
    }
    // One transfer's frames, plus one per packet for feedback-sized packets.
    let transfer_frames =
        (u64::from(sample_rate_hz) * transfer_us).div_ceil(1_000_000) + packets_per_transfer;
    if stats.zero_packets > packets_per_transfer || stats.lost_frames > transfer_frames {
        return None;
    }
    Some(json!({
        "classification": "lone_host_timing_loss",
        "interpretation": "One transfer completed with zero-length packets while the ring held audio: the host missed its bus slot. Not evidence of a DAC, clock or payload fault; the lost audio is still counted.",
        "lost_frames": stats.lost_frames,
        "zero_packets": stats.zero_packets,
        "transfer_us": transfer_us,
        "packets_per_transfer": packets_per_transfer,
        "transfer_frame_bound": transfer_frames,
        "max_turnaround_us": stats.max_turnaround_us,
        "max_worker_gap_us": stats.max_worker_gap_us,
        "worker_gap_over_10ms": stats.worker_gap_over_10ms,
        "adaptive_queue_changes": stats.adaptive_queue_changes,
        "adaptive_queue_us": stats.adaptive_queue_us,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    // The 176.4 kHz mono-to-stereo fallback on the Topping D10 Balanced, macOS.
    fn d10_leg() -> LossCounters {
        LossCounters {
            lost_frames: 353,
            zero_packets: 16,
            frames_written: 264_600,
            played_frames: 264_247,
            adaptive_queue_slots: 10,
            adaptive_queue_us: 20_000,
            adaptive_queue_changes: 1,
            max_turnaround_us: 26_918,
            max_worker_gap_us: 13_210,
            worker_gap_over_10ms: 3,
            ..LossCounters::default()
        }
    }

    #[test]
    fn one_late_transfer_is_a_host_timing_loss() {
        let evidence = lone_host_timing_loss(&d10_leg(), Encoding::Pcm, 176_400, 125).unwrap();
        assert_eq!(evidence["packets_per_transfer"], 16);
        assert_eq!(evidence["transfer_us"], 2_000);
    }

    #[test]
    fn any_other_loss_shape_is_not() {
        let encodings = [
            Encoding::Pcm,
            Encoding::DsdNative {
                bit_rate: 2_822_400,
            },
        ];
        assert!(lone_host_timing_loss(&d10_leg(), encodings[1], 176_400, 125).is_none());
        let cases: [fn(&mut LossCounters); 7] = [
            |s| s.underruns = 1,
            |s| s.short_packets = 1,
            |s| s.urb_timeouts = 1,
            |s| s.zero_packets = 0,
            |s| s.zero_packets = 17,
            |s| s.played_frames -= 1,
            |s| {
                s.lost_frames = 706;
                s.played_frames = s.frames_written - 706;
            },
        ];
        for (index, change) in cases.iter().enumerate() {
            let mut stats = d10_leg();
            change(&mut stats);
            assert!(
                lone_host_timing_loss(&stats, encodings[0], 176_400, 125).is_none(),
                "case {index}"
            );
        }
        let mut clean = d10_leg();
        clean.lost_frames = 0;
        clean.played_frames = clean.frames_written;
        assert!(lone_host_timing_loss(&clean, Encoding::Pcm, 176_400, 125).is_none());
    }
}
