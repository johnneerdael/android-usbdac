//! Bounded evidence from a stream's dedicated USB feedback endpoint.

use parking_lot::Mutex;
use std::collections::VecDeque;
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

const STARTUP_ENTRIES: usize = 128;
const RECENT_ENTRIES: usize = 2_048;
const UNDERRUN_STARTUP_ENTRIES: usize = 16;
const UNDERRUN_RECENT_ENTRIES: usize = 64;

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
    pub transfer_slots: usize,
    pub packets_per_transfer: usize,
    pub transfer_timeout_ms: u32,
    pub per_service_interval_quirk: bool,
    pub post_alt_readback_hz: Option<u32>,
    pub timebase: Option<&'static str>,
    pub inferred_timebase: bool,
    /// The current stream validated units already learned on this DAC session.
    pub reused_timebase: bool,
    pub successful_submissions: u64,
    pub submit_failures: u64,
    /// libusb submission error code, separate from completion status.
    pub last_submit_error: Option<i32>,
    pub transfer_status_counts: [u64; 8],
    pub packet_status_counts: [u64; 8],
    pub accepted_reports: u64,
    /// Accepted unsmoothed observations, in frames per OUT service interval.
    pub rate_samples: u64,
    pub rate_min_frames: f64,
    pub rate_max_frames: f64,
    pub rate_mean_frames: f64,
    /// Population variance before the configured timing policy. No audible
    /// smoothing tradeoff is inferred.
    pub rate_variance_frames: f64,
    pub empty_polls: u64,
    pub rejected_reports: u64,
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
    /// Selected policy, retained before the first explicit report arrives.
    ExplicitFeedbackPolicy {
        smoothing: crate::FeedbackSmoothing,
    },
    /// The validated selected report and the actual fractional scheduling rate.
    /// Raw feedback statistics remain independent of the configured filter.
    ExplicitFeedbackRate {
        smoothing: crate::FeedbackSmoothing,
        reported_frames_per_out_service: f64,
        applied_frames_per_out_service: f64,
        applied_rate_hz: f64,
    },
    CompatibilityFallback {
        tier: crate::CompatibilityFallbackTier,
        reason: &'static str,
    },
    /// Session-local observation only; never a persisted device rule.
    ClockReadbackSuggestion {
        control_interface: u8,
        clock_entity: u8,
        strikes: u8,
    },
}

/// A record in the trace, timed from stream creation using a monotonic clock.
#[derive(Debug, Clone, Copy, PartialEq)]
#[non_exhaustive]
pub struct FeedbackTraceEntry {
    pub elapsed_us: u64,
    pub event: FeedbackTraceEvent,
}

/// A worker-observed transition into or out of an underrun episode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum UnderrunEdgeKind {
    /// A prepared transfer first required underrun padding.
    Started,
    /// A later prepared transfer no longer required underrun padding.
    Recovered,
}
impl UnderrunEdgeKind {
    pub fn code(self) -> &'static str {
        match self {
            Self::Started => "started",
            Self::Recovered => "recovered",
        }
    }
}

/// Relaxed atomic snapshots taken at the observed worker edge, after preparing the transfer.
/// `elapsed_us` shares the exact origin of the enclosing feedback trace. Sample counts
/// are interleaved ring samples; frame counters use the stream's native counter rate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnderrunEdge {
    pub elapsed_us: u64,
    pub kind: UnderrunEdgeKind,
    pub underrun_transfers: u64,
    pub buffered_samples: usize,
    pub capacity_samples: usize,
    pub channels: usize,
    pub producer_frames: u64,
    pub played_frames: u64,
    pub inflight_frames: u64,
    pub flush_epoch: u64,
    pub transfer_queue_depth: usize,
}

/// Evidence of an isochronous OUT submit failure or its one retry. The slot
/// and generation pair joins both attempts within the same stream.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[non_exhaustive]
pub struct IsoSubmitIncident {
    pub elapsed_us: u64,
    pub interface_number: u8,
    pub alt_setting: u8,
    pub endpoint_address: u8,
    pub slot: usize,
    pub generation: u64,
    pub attempt: u8,
    pub libusb_status: Option<i32>,
    /// Unmodified backend diagnostic, if emitted for this exact submit.
    pub native_error: Option<String>,
    pub outcome: &'static str,
    pub packets: usize,
    pub service_interval_us: u32,
    pub queued_slots: usize,
    pub adaptive_queue_us: u64,
    pub max_event_pump_elapsed_us: u64,
    pub max_worker_loop_gap_us: u64,
    pub lost_frames: u64,
}

const ISO_SUBMIT_INCIDENTS: usize = 32;

/// A shareable support capture for the most recent stream of a DAC session.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct FeedbackTrace {
    /// In-process monotonic origin shared by feedback entries and underrun edges.
    /// This instant has no portable UTC representation; compute offsets in-process.
    pub started_at: Instant,
    pub format: crate::StreamFormat,
    pub explicit: Option<ExplicitFeedbackDiagnostics>,
    pub entries: Vec<FeedbackTraceEntry>,
    /// Observations overwritten at the retention limit or missed during an export.
    pub dropped_entries: u64,
    /// First and recent underrun edges, independently retained from feedback events.
    pub underrun_edges: Vec<UnderrunEdge>,
    /// Underrun edges overwritten or missed due to a concurrent snapshot.
    pub underrun_dropped: u64,
    pub iso_submit_incidents: Vec<IsoSubmitIncident>,
    pub iso_submit_dropped: u64,
    pub failure: Option<String>,
}

#[derive(Debug)]
pub(crate) struct FeedbackRecorder {
    started_at: Instant,
    dropped: AtomicU64,
    history: Mutex<History>,
    underrun_dropped: AtomicU64,
    underruns: Mutex<UnderrunHistory>,
    iso_submit_dropped: AtomicU64,
    iso_submits: Mutex<VecDeque<IsoSubmitIncident>>,
}

#[derive(Debug)]
struct History {
    startup: Vec<FeedbackTraceEntry>,
    recent: VecDeque<FeedbackTraceEntry>,
    failure: Option<String>,
}

#[derive(Debug)]
struct UnderrunHistory {
    startup: Vec<UnderrunEdge>,
    recent: VecDeque<UnderrunEdge>,
}

impl FeedbackRecorder {
    pub(crate) fn new(started_at: Instant) -> Self {
        Self {
            started_at,
            dropped: AtomicU64::new(0),
            underrun_dropped: AtomicU64::new(0),
            iso_submit_dropped: AtomicU64::new(0),
            iso_submits: Mutex::new(VecDeque::with_capacity(ISO_SUBMIT_INCIDENTS)),
            underruns: Mutex::new(UnderrunHistory {
                startup: Vec::with_capacity(UNDERRUN_STARTUP_ENTRIES),
                recent: VecDeque::with_capacity(UNDERRUN_RECENT_ENTRIES),
            }),
            history: Mutex::new(History {
                startup: Vec::with_capacity(STARTUP_ENTRIES),
                recent: VecDeque::with_capacity(RECENT_ENTRIES),
                failure: None,
            }),
        }
    }

    /// Preserve a bounded, deterministic startup snapshot before recent traffic
    /// can overwrite it. A closed stream's exported trace retains these facts.
    pub(crate) fn record_clock_readback_suggestions(
        &self,
        suggestions: impl IntoIterator<Item = ((u8, u8), u8)>,
    ) {
        let mut suggestions: Vec<_> = suggestions
            .into_iter()
            .filter(|(_, strikes)| *strikes >= 3)
            .collect();
        suggestions.sort_unstable_by_key(|(identity, _)| *identity);
        for ((control_interface, clock_entity), strikes) in suggestions.into_iter().take(16) {
            self.record(FeedbackTraceEvent::ClockReadbackSuggestion {
                control_interface,
                clock_entity,
                strikes,
            });
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

    /// Try to retain an edge without blocking or allocating on the USB worker.
    pub(crate) fn record_underrun_at(&self, timestamp: Instant, mut edge: UnderrunEdge) {
        edge.elapsed_us = timestamp
            .saturating_duration_since(self.started_at)
            .as_micros()
            .min(u128::from(u64::MAX)) as u64;
        let Some(mut history) = self.underruns.try_lock() else {
            self.underrun_dropped.fetch_add(1, Ordering::Relaxed);
            return;
        };
        if history.startup.len() < UNDERRUN_STARTUP_ENTRIES {
            history.startup.push(edge);
        } else {
            if history.recent.len() == UNDERRUN_RECENT_ENTRIES {
                history.recent.pop_front();
                self.underrun_dropped.fetch_add(1, Ordering::Relaxed);
            }
            history.recent.push_back(edge);
        }
    }

    pub(crate) fn underrun_snapshot(&self) -> (Vec<UnderrunEdge>, u64) {
        let history = self.underruns.lock();
        (
            history
                .startup
                .iter()
                .chain(&history.recent)
                .copied()
                .collect(),
            self.underrun_dropped.load(Ordering::Relaxed),
        )
    }

    pub(crate) fn record_iso_submit(&self, mut incident: IsoSubmitIncident) {
        incident.elapsed_us = Instant::now()
            .saturating_duration_since(self.started_at)
            .as_micros()
            .min(u128::from(u64::MAX)) as u64;
        let Some(mut entries) = self.iso_submits.try_lock() else {
            self.iso_submit_dropped.fetch_add(1, Ordering::Relaxed);
            return;
        };
        if entries.len() == ISO_SUBMIT_INCIDENTS {
            entries.pop_front();
            self.iso_submit_dropped.fetch_add(1, Ordering::Relaxed);
        }
        entries.push_back(incident);
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
        let (underrun_edges, underrun_dropped) = self.underrun_snapshot();
        FeedbackTrace {
            started_at: self.started_at,
            format,
            explicit,
            entries: history
                .startup
                .iter()
                .chain(&history.recent)
                .copied()
                .collect(),
            dropped_entries: self.dropped.load(Ordering::Relaxed),
            underrun_edges,
            underrun_dropped,
            iso_submit_incidents: self.iso_submits.lock().iter().cloned().collect(),
            iso_submit_dropped: self.iso_submit_dropped.load(Ordering::Relaxed),
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
                "request={}B x{} slots={} descriptor={}B timeout={}ms out=0x{:02x} {}us ",
                "capacity={}B stride={}B nominal={}Hz clock_readback={:?} ",
                "timebase={:?} inferred={} reused={} per_service_quirk={} ",
                "submits={} submit_failures={} last_submit_error={:?} ",
                "transfer_statuses={:?} packet_statuses={:?} accepted={} empty={} rejected={}"
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
            self.transfer_slots,
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
            self.reused_timebase,
            self.per_service_interval_quirk,
            self.successful_submissions,
            self.submit_failures,
            self.last_submit_error,
            self.transfer_status_counts,
            self.packet_status_counts,
            self.accepted_reports,
            self.empty_polls,
            self.rejected_reports,
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

    fn edge() -> UnderrunEdge {
        UnderrunEdge {
            elapsed_us: 0,
            kind: UnderrunEdgeKind::Started,
            underrun_transfers: 1,
            buffered_samples: 4,
            capacity_samples: 100,
            channels: 2,
            producer_frames: 8,
            played_frames: 4,
            inflight_frames: 2,
            flush_epoch: 0,
            transfer_queue_depth: 3,
        }
    }
    #[test]
    fn underrun_history_has_independent_bounded_first_and_recent_edges() {
        let start = Instant::now();
        let recorder = FeedbackRecorder::new(start);
        for n in 0..(STARTUP_ENTRIES + RECENT_ENTRIES + 5) {
            recorder.record_at(
                start + std::time::Duration::from_micros(n as u64),
                FeedbackTraceEvent::Submit {
                    slot: None,
                    status: 0,
                },
            );
        }
        let count = UNDERRUN_STARTUP_ENTRIES + UNDERRUN_RECENT_ENTRIES + 3;
        for n in 0..count {
            recorder.record_underrun_at(start + std::time::Duration::from_micros(n as u64), edge());
        }
        let (events, dropped) = recorder.underrun_snapshot();
        assert_eq!(
            events.len(),
            UNDERRUN_STARTUP_ENTRIES + UNDERRUN_RECENT_ENTRIES
        );
        assert_eq!(events[0].elapsed_us, 0);
        assert_eq!(
            events[UNDERRUN_STARTUP_ENTRIES].elapsed_us,
            (UNDERRUN_STARTUP_ENTRIES + 3) as u64
        );
        assert_eq!(events.last().unwrap().elapsed_us, (count - 1) as u64);
        assert_eq!(dropped, 3);
        assert_eq!(recorder.dropped.load(Ordering::Relaxed), 5);
        let held = recorder.underruns.lock();
        recorder.record_underrun_at(start, edge());
        drop(held);
        assert_eq!(recorder.underrun_snapshot().1, 4);
    }

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

#[cfg(test)]
mod compatibility_suggestion_trace_tests {
    use super::*;
    #[test]
    fn three_strike_suggestions_stay_bounded_and_survive_recent_history_overwrite() {
        let recorder = FeedbackRecorder::new(Instant::now());
        let events = crate::events::EventHub::start();
        crate::events::CompatibilityAdmissions::new(false, true, true, false, None).publish(
            true,
            &events,
            Some(&recorder),
        );
        events.shutdown();
        let suggestions = (0..32u8).map(|interface| ((interface, 7), 3));
        recorder.record_clock_readback_suggestions(suggestions.chain([((99, 1), 2)]));
        for _ in 0..(STARTUP_ENTRIES + RECENT_ENTRIES + 8) {
            recorder.record(FeedbackTraceEvent::Lock {
                locked: true,
                stale: false,
            });
        }
        let history = recorder.history.lock();
        let suggestions: Vec<_> = history
            .startup
            .iter()
            .filter_map(|entry| match entry.event {
                FeedbackTraceEvent::ClockReadbackSuggestion {
                    control_interface,
                    clock_entity,
                    strikes,
                } => Some((control_interface, clock_entity, strikes)),
                _ => None,
            })
            .collect();
        assert_eq!(
            suggestions,
            (0..16u8)
                .map(|interface| (interface, 7, 3))
                .collect::<Vec<_>>()
        );
        assert!(history.startup.iter().any(|entry|matches!(entry.event,
            FeedbackTraceEvent::CompatibilityFallback {tier:crate::CompatibilityFallbackTier::UnverifiedClock,reason} if !reason.is_empty())));
    }
}
