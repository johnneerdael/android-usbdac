//! Bounded per-leg producer timings. USB worker edges use the same monotonic clock.
use super::*;
use std::{
    collections::{BTreeMap, VecDeque},
    sync::atomic::AtomicU64,
};

const BIN_US: u64 = 20_000;
const MAX_BINS: usize = 1024;
const MAX_INTERVALS: usize = 256;
const MAX_SAMPLES: usize = 512;
const SLOW_US: u64 = 100;
const MAX_PINNED: usize = 512;

#[derive(Default, Clone)]
struct Stage {
    calls: u64,
    total_us: u64,
    max_us: u64,
    input_bytes: u64,
    output_bytes: u64,
    blocked: u64,
}
impl Stage {
    fn add(&mut self, elapsed: u64, input: usize, output: usize, blocked: bool) {
        self.calls += 1;
        self.total_us = self.total_us.saturating_add(elapsed);
        self.max_us = self.max_us.max(elapsed);
        self.input_bytes = self.input_bytes.saturating_add(input as u64);
        self.output_bytes = self.output_bytes.saturating_add(output as u64);
        self.blocked += u64::from(blocked);
    }
    fn json(&self) -> Value {
        json!({"calls":self.calls,"total_us":self.total_us,"max_us":self.max_us,"input_bytes":self.input_bytes,"output_bytes":self.output_bytes,"blocked_calls":self.blocked})
    }
}
#[derive(Clone)]
struct Interval {
    phase: &'static str,
    start_us: u64,
    elapsed_us: u64,
}
#[derive(Clone)]
struct QueueSample {
    at_us: u64,
    queued: u64,
    written: u64,
    played: u64,
    underruns: u64,
}

pub(super) struct ProducerTimingCapture {
    pub(super) origin: Instant,
    totals: BTreeMap<&'static str, Stage>,
    bins: BTreeMap<u64, BTreeMap<&'static str, Stage>>,
    intervals_first: Vec<Interval>,
    intervals: VecDeque<Interval>,
    pinned: Vec<Interval>,
    pinned_dropped: u64,
    last_pinned_us: Option<u64>,
    last_sample_underruns: u64,
    samples_first: Vec<QueueSample>,
    samples: VecDeque<QueueSample>,
    accepted_first: Vec<u64>,
    accepted: VecDeque<u64>,
    dropped_accepted: u64,
    dropped_intervals: u64,
    dropped_samples: u64,
    dropped_bins: u64,
    last_sample_us: Option<u64>,
}
impl ProducerTimingCapture {
    pub(super) fn new(origin: Instant) -> Self {
        Self {
            origin,
            totals: BTreeMap::new(),
            bins: BTreeMap::new(),
            intervals_first: Vec::new(),
            intervals: VecDeque::new(),
            pinned: Vec::new(),
            pinned_dropped: 0,
            last_pinned_us: None,
            last_sample_underruns: 0,
            samples_first: Vec::new(),
            samples: VecDeque::new(),
            accepted_first: Vec::new(),
            accepted: VecDeque::new(),
            dropped_accepted: 0,
            dropped_intervals: 0,
            dropped_samples: 0,
            dropped_bins: 0,
            last_sample_us: None,
        }
    }
    fn us(&self, t: Instant) -> u64 {
        t.saturating_duration_since(self.origin)
            .as_micros()
            .min(u128::from(u64::MAX)) as u64
    }
    pub(super) fn record(
        &mut self,
        phase: &'static str,
        start: Instant,
        elapsed: Duration,
        input: usize,
        output: usize,
        blocked: bool,
    ) {
        let at = self.us(start);
        let duration = elapsed.as_micros().min(u128::from(u64::MAX)) as u64;
        self.totals
            .entry(phase)
            .or_default()
            .add(duration, input, output, blocked);
        let bin = at / BIN_US;
        if self.bins.contains_key(&bin) || self.bins.len() < MAX_BINS {
            self.bins
                .entry(bin)
                .or_default()
                .entry(phase)
                .or_default()
                .add(duration, input, output, blocked);
        } else {
            self.dropped_bins += 1;
        }
        if phase == "ring_enqueue" && output > 0 {
            let accepted_at = at.saturating_add(duration);
            if self.accepted_first.len() < 128 {
                self.accepted_first.push(accepted_at);
            } else {
                if self.accepted.len() == 1024 {
                    self.accepted.pop_front();
                    self.dropped_accepted += 1;
                }
                self.accepted.push_back(accepted_at);
            }
        }
        if phase != "producer_call_inclusive" && duration >= SLOW_US {
            let interval = Interval {
                phase,
                start_us: at,
                elapsed_us: duration,
            };
            if self.intervals_first.len() < 128 {
                self.intervals_first.push(interval);
            } else {
                if self.intervals.len() == MAX_INTERVALS {
                    self.intervals.pop_front();
                    self.dropped_intervals += 1;
                }
                self.intervals.push_back(interval);
            }
        }
    }
    pub(super) fn sample(&mut self, stats: &usbdac_core::StreamStats, force: bool) {
        let at = self.us(Instant::now());
        if !force
            && self
                .last_sample_us
                .is_some_and(|last| at.saturating_sub(last) < BIN_US)
        {
            return;
        }
        self.last_sample_us = Some(at);
        if stats.underruns > self.last_sample_underruns
            && self
                .last_pinned_us
                .is_none_or(|last| at.saturating_sub(last) >= 100_000)
        {
            self.last_pinned_us = Some(at);
            for interval in self
                .intervals_first
                .iter()
                .chain(self.intervals.iter())
                .filter(|item| {
                    item.start_us
                        .saturating_add(item.elapsed_us)
                        .saturating_add(60_000)
                        >= at
                })
            {
                if self.pinned.iter().any(|v| {
                    v.phase == interval.phase
                        && v.start_us == interval.start_us
                        && v.elapsed_us == interval.elapsed_us
                }) {
                    continue;
                }
                if self.pinned.len() < MAX_PINNED {
                    self.pinned.push(interval.clone());
                } else {
                    self.pinned_dropped += 1;
                }
            }
        }
        self.last_sample_underruns = stats.underruns;
        let sample = QueueSample {
            at_us: at,
            queued: stats.queued_frames,
            written: stats.frames_written,
            played: stats.played_frames,
            underruns: stats.underruns,
        };
        if self.samples_first.len() < 128 {
            self.samples_first.push(sample);
        } else {
            if self.samples.len() == MAX_SAMPLES {
                self.samples.pop_front();
                self.dropped_samples += 1;
            }
            self.samples.push_back(sample);
        }
    }
    pub(super) fn finish(
        &self,
        underruns: u64,
        edges: &[IncidentEdge],
        trace_dropped: u64,
        callback_dropped: u64,
    ) -> Value {
        let totals: BTreeMap<_, _> = self.totals.iter().map(|(k, v)| (*k, v.json())).collect();
        let mut incidents = Vec::new();
        if underruns > 0 {
            for edge in edges.iter().filter(|edge| edge.underrun).take(64) {
                let sample = self
                    .samples_first
                    .iter()
                    .chain(self.samples.iter())
                    .min_by_key(|sample| sample.at_us.abs_diff(edge.at_us))
                    .filter(|sample| sample.at_us.abs_diff(edge.at_us) <= 40_000);
                let eligible = |phase: &str| {
                    !matches!(
                        phase,
                        "generation_preopen"
                            | "negotiation"
                            | "drain_wait"
                            | "producer_call_inclusive"
                    )
                };
                let overlapping: Vec<_> = self
                    .intervals_first
                    .iter()
                    .chain(self.intervals.iter())
                    .chain(self.pinned.iter())
                    .filter(|v| {
                        eligible(v.phase)
                            && v.start_us <= edge.at_us
                            && v.start_us.saturating_add(v.elapsed_us) >= edge.at_us
                    })
                    .collect();
                let adjacent: Vec<_> = self.intervals_first.iter().chain(self.intervals.iter()).chain(self.pinned.iter()).filter(|v| eligible(v.phase) && v.start_us <= edge.at_us.saturating_add(20_000) && v.start_us.saturating_add(v.elapsed_us).saturating_add(20_000) >= edge.at_us).map(|v| json!({"phase":v.phase,"start_us":v.start_us,"elapsed_us":v.elapsed_us})).collect();
                let empty = edge.data.get("buffered_samples").and_then(Value::as_u64) == Some(0);
                let distinct: std::collections::BTreeSet<_> =
                    overlapping.iter().map(|v| v.phase).collect();
                let interval_gap = self.dropped_intervals > 0
                    && self
                        .intervals_first
                        .last()
                        .is_some_and(|v| edge.at_us > v.start_us.saturating_add(v.elapsed_us))
                    && self
                        .intervals
                        .front()
                        .is_some_and(|v| edge.at_us < v.start_us);
                let pinned_overlap = self.pinned_dropped == 0
                    && self.pinned.iter().any(|v| {
                        eligible(v.phase)
                            && v.start_us <= edge.at_us
                            && v.start_us.saturating_add(v.elapsed_us) >= edge.at_us
                    });
                let stage = if empty
                    && distinct.len() == 1
                    && (!interval_gap || pinned_overlap)
                    && callback_dropped == 0
                {
                    distinct.iter().next().copied()
                } else {
                    None
                };
                let enqueue_gap = self.dropped_accepted > 0
                    && self
                        .accepted_first
                        .last()
                        .is_some_and(|at| edge.at_us > *at)
                    && self.accepted.front().is_some_and(|at| edge.at_us < *at);
                let last_enqueue = self
                    .accepted_first
                    .iter()
                    .chain(self.accepted.iter())
                    .copied()
                    .filter(|at| *at <= edge.at_us)
                    .max()
                    .filter(|_| !enqueue_gap);
                incidents.push(json!({"edge_us":edge.at_us,"worker":edge.data,
                    "classification":if stage.is_some() {"observed_producer_stage_at_empty_edge"} else {"unresolved"},
                    "observed_stage":stage,
                    "reason":if !empty {"worker snapshot did not establish an empty queue"} else if (interval_gap && !pinned_overlap) || callback_dropped > 0 {"timing history is incomplete at or across this edge"} else if distinct.is_empty() {"no retained timed interval overlapped this edge"} else if distinct.len()>1 {"multiple producer stages overlapped this edge"} else {"one retained producer stage overlapped the worker-observed empty queue"},
                    "last_accepted_enqueue_age_us":last_enqueue.map(|at| edge.at_us-at), "enqueue_history_gap":enqueue_gap,
                    "note":"Exact overlap is an observation, not proof of CPU contention or preemption. Intervals shorter than the retention threshold and overwritten history may be absent. Pre-open generation is excluded.",
                    "nearest_queue_sample":sample.map(|s|json!({"at_us":s.at_us,"queued_frames":s.queued,"frames_written":s.written,"played_frames":s.played,"underruns":s.underruns})),"adjacent_intervals":adjacent}));
            }
            if incidents.is_empty() {
                incidents.push(json!({"classification":"unresolved","reason":"No retained worker-side underrun edge; aggregate counts alone do not identify the cause."}));
            }
        }
        let detail = if underruns > 0 {
            json!({
                "bins":self.bins.iter().map(|(i,v)|json!({"start_us":i*BIN_US,"stages":v.iter().map(|(name,stage)|(*name,stage.json())).collect::<BTreeMap<_,_>>()})).collect::<Vec<_>>(),
                "intervals":self.intervals_first.iter().chain(self.intervals.iter()).map(|v|json!({"phase":v.phase,"start_us":v.start_us,"elapsed_us":v.elapsed_us})).collect::<Vec<_>>(),
                "queue_samples":self.samples_first.iter().chain(self.samples.iter()).map(|s|json!({"at_us":s.at_us,"queued_frames":s.queued,"frames_written":s.written,"played_frames":s.played,"underruns":s.underruns})).collect::<Vec<_>>(),
                "incident_pinned_intervals":self.pinned.iter().map(|v|json!({"phase":v.phase,"start_us":v.start_us,"elapsed_us":v.elapsed_us})).collect::<Vec<_>>(),
                "worker_edges":edges.iter().map(|e|json!({"at_us":e.at_us,"underrun":e.underrun,"evidence":e.data})).collect::<Vec<_>>()
            })
        } else {
            Value::Null
        };
        json!({"clock":"monotonic, relative to validator leg origin", "bin_us":BIN_US,"slow_interval_threshold_us":SLOW_US,"sample_interval_us":BIN_US,"stages":totals,"incident_details":incidents,"detail":detail,"retention":{"max_bins":MAX_BINS,"max_intervals":MAX_INTERVALS,"max_queue_samples":MAX_SAMPLES,"dropped_bins":self.dropped_bins,"dropped_intervals":self.dropped_intervals,"max_pinned_intervals":MAX_PINNED,"dropped_pinned_intervals":self.pinned_dropped,"dropped_queue_samples":self.dropped_samples,"dropped_worker_edges":trace_dropped,"dropped_callbacks":callback_dropped,"max_incident_summaries":64,"omitted_incident_summaries":edges.iter().filter(|edge| edge.underrun).count().saturating_sub(64),"max_accepted_enqueues":1024,"dropped_accepted_enqueues":self.dropped_accepted},"interpretation":"Stage intervals can overlap; totals are not additive. Observer verification and diagnostic sampling are measured separately. Callback collection overhead is not independently measured. Pre-open work cannot starve the stream."})
    }
}
#[derive(Clone)]
pub(super) struct IncidentEdge {
    pub(super) at_us: u64,
    pub(super) underrun: bool,
    pub(super) data: Value,
}

pub(super) fn callback_dropped() -> Arc<AtomicU64> {
    Arc::new(AtomicU64::new(0))
}

/// Convert the worker's independently retained edge clock to the leg origin.
pub(super) fn worker_edges(dac: &Dac, origin: Instant) -> (Vec<IncidentEdge>, u64) {
    let Some(trace) = dac.feedback_trace() else {
        return (Vec::new(), 0);
    };
    let offset: i128 = if trace.started_at >= origin {
        trace.started_at.duration_since(origin).as_micros() as i128
    } else {
        -(origin.duration_since(trace.started_at).as_micros() as i128)
    };
    let edges = trace.underrun_edges.iter().filter_map(|edge| {
        let at = offset + i128::from(edge.elapsed_us);
        (at >= 0).then(|| IncidentEdge { at_us: at.min(i128::from(u64::MAX)) as u64,
            underrun: edge.kind == usbdac_core::UnderrunEdgeKind::Started,
            data: json!({"kind":edge.kind.code(),"elapsed_from_trace_us":edge.elapsed_us,"underrun_transfers":edge.underrun_transfers,"buffered_samples":edge.buffered_samples,"capacity_samples":edge.capacity_samples,"channels":edge.channels,"producer_frames":edge.producer_frames,"played_frames":edge.played_frames,"inflight_frames":edge.inflight_frames,"flush_epoch":edge.flush_epoch,"transfer_queue_depth":edge.transfer_queue_depth}) })
    }).collect();
    (edges, trace.underrun_dropped)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn exact_overlap_is_distinguished_from_preopen_and_ambiguous_activity() {
        let origin = Instant::now();
        let mut capture = ProducerTimingCapture::new(origin);
        capture.record(
            "generation_preopen",
            origin,
            Duration::from_millis(20),
            0,
            0,
            false,
        );
        capture.record(
            "ring_enqueue",
            origin + Duration::from_millis(21),
            Duration::from_millis(1),
            4,
            4,
            false,
        );
        capture.record(
            "generation_on_demand",
            origin + Duration::from_millis(25),
            Duration::from_millis(5),
            0,
            4,
            false,
        );
        let edge = IncidentEdge {
            at_us: 27_000,
            underrun: true,
            data: json!({"buffered_samples":0}),
        };
        let first = capture.finish(1, std::slice::from_ref(&edge), 0, 0);
        assert_eq!(
            first["incident_details"][0]["observed_stage"],
            "generation_on_demand"
        );
        assert_eq!(
            first["incident_details"][0]["last_accepted_enqueue_age_us"],
            5000
        );
        capture.record(
            "observer_verification",
            origin + Duration::from_millis(26),
            Duration::from_millis(3),
            4,
            4,
            false,
        );
        let ambiguous = capture.finish(1, &[edge], 0, 0);
        assert_eq!(
            ambiguous["incident_details"][0]["classification"],
            "unresolved"
        );
    }
    #[test]
    fn aggregate_underruns_without_worker_edge_are_unresolved() {
        let capture = ProducerTimingCapture::new(Instant::now());
        let result = capture.finish(4, &[], 0, 0);
        assert_eq!(
            result["incident_details"][0]["classification"],
            "unresolved"
        );
    }
}
