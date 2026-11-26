//! Bounded evidence from a stream's dedicated USB feedback endpoint.

use parking_lot::Mutex;
use std::collections::VecDeque;
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

const STARTUP_ENTRIES: usize = 128;
const RECENT_ENTRIES: usize = 2_048;

/// What happened to a packet while it was examined for explicit feedback.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ExplicitFeedbackOutcome {
    Valid,
    Empty,
    TransferFailed,
    PacketFailed,
    InvalidLength,
    InvalidEncoding,
    RateOutOfRange,
    PacketCapacityExceeded,
    TimebaseCandidate,
}

impl ExplicitFeedbackOutcome {
    pub fn code(self) -> &'static str {
        match self {
            Self::Valid => "valid",
            Self::Empty => "empty",
            Self::TransferFailed => "transfer_failed",
            Self::PacketFailed => "packet_failed",
            Self::InvalidLength => "invalid_length",
            Self::InvalidEncoding => "invalid_encoding",
            Self::RateOutOfRange => "rate_out_of_range",
            Self::PacketCapacityExceeded => "packet_capacity_exceeded",
            Self::TimebaseCandidate => "timebase_candidate",
        }
    }

    fn index(self) -> usize {
        match self {
            Self::Valid => 0,
            Self::Empty => 1,
            Self::TransferFailed => 2,
            Self::PacketFailed => 3,
            Self::InvalidLength => 4,
            Self::InvalidEncoding => 5,
            Self::RateOutOfRange => 6,
            Self::PacketCapacityExceeded => 7,
            Self::TimebaseCandidate => 8,
        }
    }
}

/// The most recent packet observed for one feedback outcome.
#[derive(Debug, Clone, Copy, PartialEq)]
#[non_exhaustive]
pub struct ExplicitFeedbackPacket {
    pub outcome: ExplicitFeedbackOutcome,
    pub transfer_status: i32,
    /// The libusb packet status, or `None` when no packet status was reported.
    pub packet_status: Option<i32>,
    pub requested_bytes: usize,
    pub actual_bytes: usize,
    pub frames_per_out_service: Option<f64>,
    pub decoded_rate_hz: Option<f64>,
    bytes: [u8; 8],
    byte_count: usize,
}

impl ExplicitFeedbackPacket {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        outcome: ExplicitFeedbackOutcome,
        transfer_status: i32,
        packet_status: Option<i32>,
        requested_bytes: usize,
        actual_bytes: usize,
        raw_bytes: &[u8],
        decoded: Option<(f64, f64)>,
    ) -> Self {
        let mut bytes = [0; 8];
        let byte_count = raw_bytes.len().min(bytes.len());
        bytes[..byte_count].copy_from_slice(&raw_bytes[..byte_count]);
        Self {
            outcome,
            transfer_status,
            packet_status,
            requested_bytes,
            actual_bytes,
            frames_per_out_service: decoded.map(|(frames, _)| frames),
            decoded_rate_hz: decoded.map(|(_, rate)| rate),
            bytes,
            byte_count,
        }
    }

    /// Up to eight bytes from the feedback IN endpoint. No audio OUT payload.
    pub fn raw_bytes(&self) -> &[u8] {
        &self.bytes[..self.byte_count]
    }
}

/// Feedback endpoint geometry and observations for one stream. Status counts
/// are indexed by libusb transfer status (0 completed, 1 error, 2 timed out,
/// 3 cancelled, 4 stall, 5 no device, 6 overflow). Index 7 is unknown or
/// unreported. An interrupt transfer has no separate packet status.
#[derive(Debug, Default, Clone, Copy, PartialEq)]
#[non_exhaustive]
pub struct ExplicitFeedbackDiagnostics {
    pub endpoint: u8,
    pub transfer_type: &'static str,
    pub link_speed: &'static str,
    pub interface_number: u8,
    pub alt_setting: u8,
    pub out_endpoint: u8,
    pub nominal_rate_hz: u32,
    pub out_service_interval_us: u32,
    pub out_max_packet_bytes: usize,
    pub out_bytes_per_frame: usize,
    pub feedback_interval: u8,
    pub feedback_refresh: u8,
    pub feedback_service_interval_us: u32,
    pub feedback_max_packet_bytes: usize,
    pub requested_packet_bytes: usize,
    pub packets_per_transfer: usize,
    pub transfer_timeout_ms: u32,
    pub per_service_interval_quirk: bool,
    pub post_alt_readback_hz: Option<u32>,
    pub timebase: Option<&'static str>,
    pub inferred_timebase: bool,
    pub successful_submissions: u64,
    pub submit_failures: u64,
    /// libusb submission error code, separate from completion status.
    pub last_submit_error: Option<i32>,
    pub transfer_status_counts: [u64; 8],
    pub packet_status_counts: [u64; 8],
    pub accepted_reports: u64,
    pub(crate) samples: [Option<ExplicitFeedbackPacket>; 9],
}

impl ExplicitFeedbackDiagnostics {
    /// One latest observation per outcome, including bytes from rejected reports.
    pub fn packet_samples(&self) -> impl Iterator<Item = &ExplicitFeedbackPacket> {
        self.samples.iter().flatten()
    }

    pub(crate) fn record_submit(&mut self, result: i32) {
        if result == 0 {
            self.successful_submissions = self.successful_submissions.saturating_add(1);
        } else {
            self.submit_failures = self.submit_failures.saturating_add(1);
            self.last_submit_error = Some(result);
        }
    }

    fn count_status(counts: &mut [u64; 8], status: Option<i32>) {
        let index = match status {
            Some(code @ 0..=6) => code as usize,
            _ => 7,
        };
        counts[index] = counts[index].saturating_add(1);
    }

    pub(crate) fn record_transfer(&mut self, status: i32) {
        Self::count_status(&mut self.transfer_status_counts, Some(status));
    }

    pub(crate) fn record_packet(&mut self, sample: ExplicitFeedbackPacket, is_iso: bool) {
        if is_iso {
            Self::count_status(&mut self.packet_status_counts, sample.packet_status);
        }
        let slot = &mut self.samples[sample.outcome.index()];
        // On a transfer error preserve any observed payload in preference to
        // a following descriptor with no bytes.
        if sample.outcome != ExplicitFeedbackOutcome::TransferFailed
            || sample.actual_bytes > 0
            || slot.map_or(true, |previous| previous.actual_bytes == 0)
        {
            *slot = Some(sample);
        }
    }
}

/// An observation from the selected USB feedback path.
#[derive(Debug, Clone, Copy, PartialEq)]
#[non_exhaustive]
pub enum FeedbackTraceEvent {
    Submit {
        slot: Option<usize>,
        status: i32,
    },
    Completion {
        slot: Option<usize>,
        status: i32,
        actual_bytes: usize,
    },
    ExplicitPacket {
        index: usize,
        sample: ExplicitFeedbackPacket,
    },
    ImplicitSource {
        interface_number: u8,
        alt_setting: u8,
        endpoint: u8,
        service_interval_us: u32,
        max_packet_bytes: usize,
        bytes_per_frame: usize,
    },
    ImplicitPacket {
        index: usize,
        status: i32,
        requested_bytes: usize,
        actual_bytes: usize,
    },
    Lock {
        locked: bool,
        stale: bool,
    },
    Timebase {
        unit: &'static str,
        inferred: bool,
    },
}

/// A record in the trace, timed from stream creation using a monotonic clock.
#[derive(Debug, Clone, Copy, PartialEq)]
#[non_exhaustive]
pub struct FeedbackTraceEntry {
    pub elapsed_us: u64,
    pub event: FeedbackTraceEvent,
}

/// A shareable support capture for the most recent stream of a DAC session.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct FeedbackTrace {
    pub format: crate::StreamFormat,
    pub explicit: Option<ExplicitFeedbackDiagnostics>,
    pub entries: Vec<FeedbackTraceEntry>,
    /// Observations overwritten at the retention limit or missed during an export.
    pub dropped_entries: u64,
    pub failure: Option<String>,
}

#[derive(Debug)]
pub(crate) struct FeedbackRecorder {
    started_at: Instant,
    dropped: AtomicU64,
    history: Mutex<History>,
}

#[derive(Debug)]
struct History {
    startup: Vec<FeedbackTraceEntry>,
    recent: VecDeque<FeedbackTraceEntry>,
    failure: Option<String>,
}

impl FeedbackRecorder {
    pub(crate) fn new(started_at: Instant) -> Self {
        Self {
            started_at,
            dropped: AtomicU64::new(0),
            history: Mutex::new(History {
                startup: Vec::with_capacity(STARTUP_ENTRIES),
                recent: VecDeque::with_capacity(RECENT_ENTRIES),
                failure: None,
            }),
        }
    }

    pub(crate) fn record(&self, event: FeedbackTraceEvent) {
        self.record_at(Instant::now(), event);
    }

    pub(crate) fn record_at(&self, timestamp: Instant, event: FeedbackTraceEvent) {
        let Some(mut history) = self.history.try_lock() else {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            return;
        };
        let entry = FeedbackTraceEntry {
            elapsed_us: timestamp
                .saturating_duration_since(self.started_at)
                .as_micros()
                .min(u128::from(u64::MAX)) as u64,
            event,
        };
        if history.startup.len() < STARTUP_ENTRIES {
            history.startup.push(entry);
        } else {
            if history.recent.len() == RECENT_ENTRIES {
                history.recent.pop_front();
                self.dropped.fetch_add(1, Ordering::Relaxed);
            }
            history.recent.push_back(entry);
        }
    }

    pub(crate) fn fail(&self, reason: &str) {
        self.history.lock().failure = Some(reason.to_owned());
    }

    pub(crate) fn snapshot(
        &self,
        format: crate::StreamFormat,
        explicit: Option<ExplicitFeedbackDiagnostics>,
    ) -> FeedbackTrace {
        let history = self.history.lock();
        FeedbackTrace {
            format,
            explicit,
            entries: history
                .startup
                .iter()
                .chain(&history.recent)
                .copied()
                .collect(),
            dropped_entries: self.dropped.load(Ordering::Relaxed),
            failure: history.failure.clone(),
        }
    }
}

impl fmt::Display for ExplicitFeedbackDiagnostics {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            concat!(
                "feedback ep=0x{:02x} {} {} if={} alt={} bInterval={} refresh={} poll={}us ",
                "request={}B x{} descriptor={}B timeout={}ms out=0x{:02x} {}us ",
                "capacity={}B stride={}B nominal={}Hz clock_readback={:?} ",
                "timebase={:?} inferred={} per_service_quirk={} ",
                "submits={} submit_failures={} last_submit_error={:?} ",
                "transfer_statuses={:?} packet_statuses={:?} accepted={}"
            ),
            self.endpoint,
            self.transfer_type,
            self.link_speed,
            self.interface_number,
            self.alt_setting,
            self.feedback_interval,
            self.feedback_refresh,
            self.feedback_service_interval_us,
            self.requested_packet_bytes,
            self.packets_per_transfer,
            self.feedback_max_packet_bytes,
            self.transfer_timeout_ms,
            self.out_endpoint,
            self.out_service_interval_us,
            self.out_max_packet_bytes,
            self.out_bytes_per_frame,
            self.nominal_rate_hz,
            self.post_alt_readback_hz,
            self.timebase,
            self.inferred_timebase,
            self.per_service_interval_quirk,
            self.successful_submissions,
            self.submit_failures,
            self.last_submit_error,
            self.transfer_status_counts,
            self.packet_status_counts,
            self.accepted_reports,
        )?;
        for packet in self.packet_samples() {
            write!(
                f,
                " {}(transfer={} packet={:?} bytes={}/{} raw={:02x?} rate={:?} frames={:?})",
                packet.outcome.code(),
                packet.transfer_status,
                packet.packet_status,
                packet.actual_bytes,
                packet.requested_bytes,
                packet.raw_bytes(),
                packet.decoded_rate_hz,
                packet.frames_per_out_service,
            )?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_startup_and_latest_observations_with_bounded_storage() {
        let start = Instant::now();
        let recorder = FeedbackRecorder::new(start);
        let total = STARTUP_ENTRIES + RECENT_ENTRIES + 5;
        for n in 0..total {
            recorder.record_at(
                start + std::time::Duration::from_micros(n as u64),
                FeedbackTraceEvent::Submit {
                    slot: None,
                    status: 0,
                },
            );
        }
        recorder.fail("feedback deadline");
        let history = recorder.history.lock();
        assert_eq!(history.startup.len(), STARTUP_ENTRIES);
        assert_eq!(history.startup[0].elapsed_us, 0);
        assert_eq!(history.recent.len(), RECENT_ENTRIES);
        assert_eq!(
            history.recent.front().unwrap().elapsed_us,
            (STARTUP_ENTRIES + 5) as u64
        );
        assert_eq!(
            history.recent.back().unwrap().elapsed_us,
            (total - 1) as u64
        );
        assert_eq!(recorder.dropped.load(Ordering::Relaxed), 5);
        assert_eq!(history.failure.as_deref(), Some("feedback deadline"));
    }
}
