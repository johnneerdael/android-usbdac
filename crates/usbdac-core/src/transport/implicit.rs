//! Whole-frame timing patterns, kept in capture submission order by the USB worker.
use super::*;
use std::collections::VecDeque;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ImplicitObservation {
    Complete {
        frames: u64,
        empty_slots: usize,
    },
    MissingTiming {
        bad_packets: usize,
        valid_frames: u64,
    },
    EmptyCompletion,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum ImplicitFault {
    Removed,
    Geometry,
    FifoOverrun,
    TimingBudgetExhausted,
    TimingUntrusted,
    TimingStale,
    CadenceMismatch,
    Transport(String),
}
impl std::fmt::Display for ImplicitFault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Removed => write!(f, "USB DAC disconnected (implicit feedback)"),
            Self::Geometry => write!(f, "implicit_xrun: invalid transfer geometry"),
            Self::FifoOverrun => write!(f, "implicit_xrun: timing FIFO overrun"),
            Self::TimingBudgetExhausted => {
                write!(f, "implicit_xrun: timing recovery budget exhausted")
            }
            Self::TimingUntrusted => write!(
                f,
                "implicit_xrun: missing timing before cadence qualification"
            ),
            Self::TimingStale => {
                write!(f, "implicit_xrun: timing requalification deadline expired")
            }
            Self::CadenceMismatch => write!(f, "implicit_xrun: cadence outside negotiated window"),
            Self::Transport(error) => write!(f, "implicit feedback: {error}"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TimingHealth {
    Priming,
    Trusted,
    Recovering { since: Instant },
}
impl TimingHealth {
    fn missing(self, completed_at: Instant, running: bool) -> Result<Self, ImplicitFault> {
        match self {
            Self::Priming if running => Err(ImplicitFault::TimingUntrusted),
            Self::Priming => Ok(Self::Priming),
            Self::Trusted => Ok(Self::Recovering {
                since: completed_at,
            }),
            Self::Recovering { .. } => Err(ImplicitFault::TimingBudgetExhausted),
        }
    }
    fn check_deadline(self, now: Instant, budget: Duration) -> Result<(), ImplicitFault> {
        if let Self::Recovering { since } = self {
            if now.saturating_duration_since(since) > budget {
                return Err(ImplicitFault::TimingStale);
            }
        }
        Ok(())
    }
}

struct PatternFifo {
    storage: Vec<Vec<usize>>,
    read: usize,
    depth: usize,
    silence: Vec<bool>,
    last_pop_silence: bool,
}
impl PatternFifo {
    fn new(capacity: usize, packets: usize) -> Self {
        Self {
            storage: vec![vec![0; packets]; capacity],
            read: 0,
            depth: 0,
            silence: vec![false; capacity],
            last_pop_silence: false,
        }
    }
    fn clear(&mut self) {
        self.read = 0;
        self.depth = 0;
        self.last_pop_silence = false;
    }
    fn push(&mut self, pattern: &[usize]) -> Result<(), String> {
        if self.depth == self.storage.len() {
            return Err("implicit feedback FIFO overflow".into());
        }
        let index = (self.read + self.depth) % self.storage.len();
        if pattern.len() != self.storage[index].len() {
            return Err("incomplete implicit feedback pattern".into());
        }
        self.storage[index].copy_from_slice(pattern);
        self.silence[index] = false;
        self.depth += 1;
        Ok(())
    }
    fn push_silence(&mut self, pattern: &[usize]) -> Result<(), String> {
        self.push(pattern)?;
        let index = (self.read + self.depth - 1) % self.storage.len();
        self.silence[index] = true;
        Ok(())
    }
    fn pop(&mut self) -> Option<Vec<usize>> {
        if self.depth == 0 {
            return None;
        }
        let value = self.storage[self.read].clone();
        self.last_pop_silence = self.silence[self.read];
        self.read = (self.read + 1) % self.storage.len();
        self.depth -= 1;
        Some(value)
    }
}

/// One fallback vector, copied only from a validated non-empty source pattern.
/// It never contributes rate samples or renews the source health deadline.
#[derive(Default)]
struct MeasuredSilence {
    previous: Option<Vec<usize>>,
    used: bool,
}
impl MeasuredSilence {
    fn observed(&mut self, pattern: &[usize]) {
        self.previous = Some(pattern.to_vec());
        self.used = false;
    }
    fn take(&mut self, enabled: bool, running: bool, locked: bool) -> Option<Vec<usize>> {
        if !enabled || !running || !locked || self.used {
            return None;
        }
        let pattern = self.previous.clone()?;
        self.used = true;
        Some(pattern)
    }
}

// Synthetic nominal cadence advances on every source packet. Its phase is a
// continuous local timeline, not a claim about the device's accumulator.
struct GapCadence {
    increment: u64,
    remainder: u64,
    stride: usize,
    max_out: usize,
}
impl GapCadence {
    fn new(rate: u32, interval_us: u32, stride: usize, max_out: usize) -> Self {
        Self {
            increment: u64::from(rate) * u64::from(interval_us),
            remainder: 0,
            stride,
            max_out,
        }
    }
    fn advance(&mut self, packets: usize, emit: bool) -> Option<Vec<usize>> {
        if self.increment == 0 || self.stride == 0 {
            return None;
        }
        let mut remainder = self.remainder;
        let mut result = Vec::with_capacity(if emit { packets } else { 0 });
        for _ in 0..packets {
            let total = remainder.checked_add(self.increment)?;
            remainder = total % 1_000_000;
            let bytes = usize::try_from(total / 1_000_000)
                .ok()?
                .checked_mul(self.stride)?;
            if emit && bytes > self.max_out {
                return None;
            }
            if emit {
                result.push(bytes);
            }
        }
        self.remainder = remainder;
        Some(result)
    }
}

pub(super) fn capture_pattern(
    packets: impl Iterator<Item = (i32, usize, usize)>,
    in_stride: usize,
    out_stride: usize,
    max_out: usize,
    pattern: &mut [usize],
) -> Result<bool, String> {
    if in_stride == 0 || out_stride == 0 {
        return Err("zero implicit frame stride".into());
    }
    let mut count = 0;
    let mut empty = true;
    for (index, (status, actual, requested)) in packets.enumerate() {
        if index >= pattern.len()
            || status != LIBUSB_TRANSFER_COMPLETED
            || actual > requested
            || actual % in_stride != 0
        {
            return Err("invalid implicit feedback packet".into());
        }
        let bytes = (actual / in_stride)
            .checked_mul(out_stride)
            .ok_or("implicit size overflow")?;
        if bytes > max_out {
            return Err("implicit feedback exceeds OUT capacity".into());
        }
        pattern[index] = bytes;
        empty &= actual == 0;
        count += 1;
    }
    if count != pattern.len() {
        return Err("incomplete implicit feedback pattern".into());
    }
    if !empty && pattern.contains(&0) {
        return Err("implicit feedback packet gap".into());
    }
    Ok(!empty)
}

#[cfg(test)]
fn known_empty_implicit_completion(
    slot: &IsoTransferSlot,
    status: i32,
    source: &ImplicitFeedbackSource,
    packets: &mut [crate::capture::CapturePacket],
) -> bool {
    let Ok((data, detached)) = capture_stream::inspect_slot(slot, status, packets) else {
        return false;
    };
    if detached
        || packets.is_empty()
        || slot.submitted_endpoint != source.address
        || slot.packet_sizes.len() != packets.len()
        || slot
            .packet_sizes
            .iter()
            .any(|size| *size != source.max_packet_bytes)
        || packets.iter().any(|packet| {
            packet.actual_bytes != 0 || packet.requested_bytes != source.max_packet_bytes
        })
        || slot.submitted_bytes != packets.len().saturating_mul(source.max_packet_bytes)
        || slot.submitted_bytes != data.len()
    {
        return false;
    }
    let transfer = unsafe { slot.transfer.as_ref() };
    let descriptors =
        unsafe { std::slice::from_raw_parts(transfer.iso_packet_desc.as_ptr(), packets.len()) };
    descriptors.iter().all(|packet| {
        matches!(
            packet.status,
            LIBUSB_TRANSFER_COMPLETED
                | LIBUSB_TRANSFER_ERROR
                | LIBUSB_TRANSFER_TIMED_OUT
                | LIBUSB_TRANSFER_OVERFLOW
        )
    })
}

/// Admit the complete source schedule before using its lengths for OUT timing
/// or publishing the shared IN payload to a duplex reader.
fn inspect_implicit_pattern(
    slot: &IsoTransferSlot,
    status: i32,
    source: &ImplicitFeedbackSource,
    rate: u32,
    out_stride: usize,
    max_out: usize,
    packets: &mut [crate::capture::CapturePacket],
    pattern: &mut [usize],
) -> Result<ImplicitObservation, ImplicitFault> {
    let transfer = unsafe { slot.transfer.as_ref() };
    let data = unsafe { &*slot.user_data };
    if status == LIBUSB_TRANSFER_NO_DEVICE {
        return Err(ImplicitFault::Removed);
    }
    if transfer.num_iso_packets != packets.len() as i32
        || packets.len() != pattern.len()
        || slot.packet_sizes.len() != packets.len()
        || source.bytes_per_frame == 0
        || out_stride == 0
        || slot.submitted_endpoint != source.address
        || transfer.endpoint != slot.submitted_endpoint
        || transfer.length < 0
        || transfer.length as usize != slot.submitted_bytes
        || slot.submitted_bytes != data.buffer.len()
        || transfer.buffer != data.buffer.as_ptr().cast_mut()
    {
        return Err(ImplicitFault::Geometry);
    }
    let descriptors =
        unsafe { std::slice::from_raw_parts(transfer.iso_packet_desc.as_ptr(), packets.len()) };
    if descriptors
        .iter()
        .any(|packet| packet.status == LIBUSB_TRANSFER_NO_DEVICE)
    {
        return Err(ImplicitFault::Removed);
    }
    let mut span = 0usize;
    let mut bad_packets = 0usize;
    let mut valid_frames = 0u64;
    let mut empty_slots = 0usize;
    for (index, (descriptor, &submitted)) in descriptors.iter().zip(&slot.packet_sizes).enumerate()
    {
        let requested = descriptor.length as usize;
        let actual = descriptor.actual_length as usize;
        if submitted != source.max_packet_bytes || requested != submitted || actual > requested {
            return Err(ImplicitFault::Geometry);
        }
        span = span.checked_add(submitted).ok_or(ImplicitFault::Geometry)?;
        if span > data.buffer.len() {
            return Err(ImplicitFault::Geometry);
        }
        let succeeded =
            status == LIBUSB_TRANSFER_COMPLETED && descriptor.status == LIBUSB_TRANSFER_COMPLETED;
        packets[index] = crate::capture::CapturePacket {
            requested_bytes: requested,
            actual_bytes: actual,
            succeeded,
        };
        if !succeeded || actual % source.bytes_per_frame != 0 {
            bad_packets += 1;
            continue;
        }
        let frames = actual / source.bytes_per_frame;
        let bytes = frames
            .checked_mul(out_stride)
            .ok_or(ImplicitFault::Geometry)?;
        if bytes > max_out {
            return Err(ImplicitFault::Geometry);
        }
        pattern[index] = bytes;
        valid_frames = valid_frames.saturating_add(frames as u64);
        empty_slots += usize::from(actual == 0);
    }
    if span != slot.submitted_bytes {
        return Err(ImplicitFault::Geometry);
    }
    if bad_packets != 0 {
        return Ok(ImplicitObservation::MissingTiming {
            bad_packets,
            valid_frames,
        });
    }
    if empty_slots == packets.len() {
        return Ok(ImplicitObservation::EmptyCompletion);
    }
    // Type I permits floor/ceiling slots, with +/- one only for an integral
    // average. Zero is therefore possible through exactly one frame/service.
    if empty_slots > 0 && u64::from(rate) * u64::from(source.service_interval_us) > 1_000_000 {
        return Ok(ImplicitObservation::MissingTiming {
            bad_packets: empty_slots,
            valid_frames,
        });
    }
    Ok(ImplicitObservation::Complete {
        frames: valid_frames,
        empty_slots,
    })
}

fn rate_window_valid(frames: u64, service_us: u64, rate: u32) -> bool {
    let expected = f64::from(rate) * service_us as f64 / 1_000_000.0;
    (frames as f64 - expected).abs() <= expected * 0.02 + 1.0
}

fn record_valid_completion(
    last_valid: &mut Instant,
    completed_at: Instant,
    started_at: Instant,
) -> u64 {
    // Submission order can differ from callback order. Preserve the newest
    // observed completion without making delayed callbacks fresh at pump time.
    *last_valid = (*last_valid).max(completed_at);
    last_valid.saturating_duration_since(started_at).as_millis() as u64
}

/// Count USB service intervals, never callback elapsed time, for a contiguous
/// whole-frame vendor clock proof. Callers reset on every empty/failed pattern.
pub(super) struct VendorRateWindow {
    rate: u32,
    frames: u64,
    service_us: u64,
}
impl VendorRateWindow {
    pub(super) fn new(rate: u32) -> Self {
        Self {
            rate,
            frames: 0,
            service_us: 0,
        }
    }
    pub(super) fn reset(&mut self) {
        self.frames = 0;
        self.service_us = 0;
    }
    pub(super) fn observe(&mut self, frames: u64, service_us: u64) -> Option<bool> {
        if frames == 0 || service_us == 0 {
            self.reset();
            return None;
        }
        self.frames = self.frames.saturating_add(frames);
        self.service_us = self.service_us.saturating_add(service_us);
        if self.service_us < 100_000 {
            return None;
        }
        let valid = rate_window_valid(self.frames, self.service_us, self.rate);
        self.reset();
        Some(valid)
    }
}

pub(super) struct ImplicitFeedback {
    pub(super) slots: Vec<IsoTransferSlot>,
    order: VecDeque<usize>,
    fifo: PatternFifo,
    measured_silence: MeasuredSilence,
    gap_cadence: GapCadence,
    scratch: Vec<usize>,
    source: ImplicitFeedbackSource,
    out_stride: usize,
    max_out: usize,
    rate: u32,
    startup_rate_proof: Option<VendorRateWindow>,
    startup_rate_proved: bool,
    window_frames: u64,
    window_us: u64,
    bad_windows: u8,
    timing_health: TimingHealth,
    recovery_budget: Duration,
    prime_count: usize,
    prime_target: usize,
    pub(super) running: bool,
    started: Instant,
    startup_budget: Duration,
    pub(super) last_valid: Instant,
    out_of_order_seen: Vec<bool>,
    capture: Option<Arc<duplex::Attachment>>,
    capture_scratch: Vec<crate::capture::CapturePacket>,
}
impl ImplicitFeedback {
    pub(super) fn new(
        source: ImplicitFeedbackSource,
        geometry: IsoQueueGeometry,
        out_stride: usize,
        max_out: usize,
        rate: u32,
    ) -> Result<Self, String> {
        let mut slots = Vec::with_capacity(geometry.transfer_slots);
        for index in 0..geometry.transfer_slots {
            slots.push(IsoTransferSlot::new(
                index,
                geometry.packets_per_transfer,
                geometry.packets_per_transfer * source.max_packet_bytes,
            )?);
        }
        Ok(Self {
            slots,
            order: VecDeque::with_capacity(geometry.transfer_slots),
            fifo: PatternFifo::new(2 * geometry.transfer_slots, geometry.packets_per_transfer),
            measured_silence: MeasuredSilence::default(),
            gap_cadence: GapCadence::new(rate, source.service_interval_us, out_stride, max_out),
            scratch: vec![0; geometry.packets_per_transfer],
            source,
            out_stride,
            max_out,
            rate,
            startup_rate_proof: None,
            startup_rate_proved: false,
            window_frames: 0,
            window_us: 0,
            bad_windows: 0,
            timing_health: TimingHealth::Priming,
            recovery_budget: Duration::from_micros(100_000u64.saturating_add(geometry.queued_us())),
            prime_count: 0,
            prime_target: geometry.transfer_slots,
            running: false,
            started: Instant::now(),
            startup_budget: Duration::from_micros(500_000.max(4 * geometry.transfer_us())),
            last_valid: Instant::now(),
            out_of_order_seen: vec![false; geometry.transfer_slots],
            capture: None,
            capture_scratch: vec![
                crate::capture::CapturePacket {
                    requested_bytes: 0,
                    actual_bytes: 0,
                    succeeded: false
                };
                geometry.packets_per_transfer
            ],
        })
    }
    pub(super) fn require_vendor_rate_proof(&mut self) {
        self.startup_rate_proof = Some(VendorRateWindow::new(self.rate));
        self.startup_rate_proved = false;
    }
    pub(super) fn promote_vendor_rate_proof(&mut self) {
        // Keep the gate restartable through the complete fresh FIFO prime.
        // Output calls this only after primed readiness with no stop or failure.
        if self.primed() {
            self.startup_rate_proof = None;
        }
    }
    pub(super) fn attach_capture(&mut self, capture: Option<Arc<duplex::Attachment>>) {
        self.capture = capture;
    }
    pub(super) fn start(
        &mut self,
        claimed: &ClaimedHandle,
        stats: &RuntimeStats,
        stop: &AtomicBool,
    ) -> Result<(), String> {
        let owner = claimed.transfer_owner();
        for slot in &mut self.slots {
            slot.retain_owner(owner.clone());
        }
        let handle = &claimed.handle;
        self.started = Instant::now();
        self.last_valid = self.started;
        for (index, slot) in self.slots.iter_mut().enumerate() {
            if usb_stop_requested(stop) {
                break;
            }
            slot.submit_capture(
                handle,
                self.source.address,
                self.source.max_packet_bytes,
                stats.feedback_trace.as_deref(),
            )?;
            self.order.push_back(index);
        }
        Ok(())
    }
    pub(super) fn pump(
        &mut self,
        handle: &DeviceHandle<Context>,
        stats: &RuntimeStats,
        stop: &AtomicBool,
    ) -> Result<(), ImplicitFault> {
        self.timing_health
            .check_deadline(Instant::now(), self.recovery_budget)?;
        if let Some(&head) = self.order.front() {
            for &index in self.order.iter().skip(1) {
                if !self.slots[head].completion_ready()
                    && self.slots[index].completion_ready()
                    && !self.out_of_order_seen[index]
                {
                    self.out_of_order_seen[index] = true;
                    stats.implicit_out_of_order.fetch_add(1, Ordering::Relaxed);
                    if let Some(capture) = self.capture.as_ref() {
                        capture.reordered();
                    }
                }
            }
        }
        while let Some(&index) = self.order.front() {
            if usb_stop_requested(stop) {
                break;
            }
            let Some((status, completed_at, actual_bytes)) = self.slots[index].take_completion()
            else {
                break;
            };
            if let Some(trace) = &stats.feedback_trace {
                trace.record_at(
                    completed_at,
                    crate::FeedbackTraceEvent::Completion {
                        slot: Some(index),
                        status,
                        actual_bytes,
                    },
                );
            }
            // Retire every consumed callback before any validation can fail.
            self.slots[index].mark_completed();
            self.order.pop_front();
            self.out_of_order_seen[index] = false;
            stats.implicit_in_transfers.fetch_add(1, Ordering::Relaxed);
            if status == LIBUSB_TRANSFER_COMPLETED {
                let transfer = unsafe { self.slots[index].transfer.as_ref() };
                // A changed count cannot safely be used to read descriptors.
                if transfer.num_iso_packets == self.scratch.len() as i32 {
                    let descriptors = unsafe {
                        std::slice::from_raw_parts(
                            transfer.iso_packet_desc.as_ptr(),
                            self.scratch.len(),
                        )
                    };
                    if let Some(trace) = &stats.feedback_trace {
                        for (index, descriptor) in descriptors.iter().enumerate() {
                            trace.record(crate::FeedbackTraceEvent::ImplicitPacket {
                                index,
                                status: descriptor.status,
                                requested_bytes: descriptor.length as usize,
                                actual_bytes: descriptor.actual_length as usize,
                            });
                        }
                    }
                }
            }
            if status == LIBUSB_TRANSFER_TIMED_OUT {
                stats.feedback_timeouts.fetch_add(1, Ordering::Relaxed);
            }
            let result = inspect_implicit_pattern(
                &self.slots[index],
                status,
                &self.source,
                self.rate,
                self.out_stride,
                self.max_out,
                &mut self.capture_scratch,
                &mut self.scratch,
            );
            // Failed transfers contain no presentable payload. Completed URBs
            // must pass the entire timing contract first, including packet gaps.
            let capture_result = if status == LIBUSB_TRANSFER_COMPLETED && result.is_ok() {
                self.capture
                    .as_ref()
                    .map(|capture| {
                        capture.ingest(&self.slots[index], status, &mut self.capture_scratch)
                    })
                    .transpose()
            } else {
                Ok(None)
            };
            match result {
                Ok(ImplicitObservation::Complete { frames, .. }) => {
                    self.measured_silence.observed(&self.scratch);
                    self.gap_cadence
                        .advance(self.scratch.len(), false)
                        .ok_or(ImplicitFault::Geometry)?;
                    let last_report_ms = record_valid_completion(
                        &mut self.last_valid,
                        completed_at,
                        stats.started_at,
                    );
                    stats.feedback_reports.fetch_add(1, Ordering::Relaxed);
                    stats
                        .implicit_measured_frames
                        .fetch_add(frames, Ordering::Relaxed);
                    stats
                        .feedback_last_report_ms
                        .store(last_report_ms, Ordering::Relaxed);
                    if let Some(proof) = self
                        .startup_rate_proof
                        .as_mut()
                        .filter(|_| !self.startup_rate_proved)
                    {
                        let service_us =
                            self.scratch.len() as u64 * u64::from(self.source.service_interval_us);
                        let verdict = proof.observe(frames, service_us);
                        if verdict == Some(false) {
                            return Err(ImplicitFault::CadenceMismatch);
                        }
                        if verdict == Some(true) {
                            self.startup_rate_proved = true;
                        }
                        // Discard the proof completion as well: prime a fresh,
                        // bounded FIFO only after proving the programmed clock.
                    } else {
                        if let Err(error) = self.fifo.push(&self.scratch) {
                            stats.implicit_overruns.fetch_add(1, Ordering::Relaxed);
                            let _ = error;
                            return Err(ImplicitFault::FifoOverrun);
                        }
                        self.prime_count = (self.prime_count + 1).min(self.prime_target);
                    }
                    self.window_frames += frames;
                    self.window_us +=
                        self.scratch.len() as u64 * u64::from(self.source.service_interval_us);
                    if self.window_us >= 100_000 {
                        let valid =
                            rate_window_valid(self.window_frames, self.window_us, self.rate);
                        self.bad_windows = if valid { 0 } else { self.bad_windows + 1 };
                        if valid && (self.startup_rate_proof.is_none() || self.startup_rate_proved)
                        {
                            self.timing_health = TimingHealth::Trusted;
                        }
                        let previous = stats.feedback_locked.swap(
                            valid && self.timing_health == TimingHealth::Trusted,
                            Ordering::AcqRel,
                        );
                        if previous != valid {
                            if let Some(trace) = &stats.feedback_trace {
                                trace.record(crate::FeedbackTraceEvent::Lock {
                                    locked: valid,
                                    stale: false,
                                });
                            }
                        }
                        stats.feedback_rate_milli_hz.store(
                            self.window_frames * 1_000_000_000 / self.window_us,
                            Ordering::Relaxed,
                        );
                        self.window_frames = 0;
                        self.window_us = 0;
                        if self.bad_windows >= 2 {
                            return Err(ImplicitFault::CadenceMismatch);
                        }
                    }
                }
                Ok(ImplicitObservation::EmptyCompletion) => {
                    self.gap_cadence
                        .advance(self.scratch.len(), false)
                        .ok_or(ImplicitFault::Geometry)?;
                    stats.feedback_empty.fetch_add(1, Ordering::Relaxed);
                    if self.running {
                        if let Some(pattern) = self.measured_silence.take(
                            settings().has(crate::Quirk::ImplicitSilenceOnEmpty),
                            self.running,
                            stats.feedback_locked.load(Ordering::Acquire),
                        ) {
                            self.fifo
                                .push_silence(&pattern)
                                .map_err(|_| ImplicitFault::FifoOverrun)?;
                            stats.feedback_locked.store(false, Ordering::Release);
                            self.window_frames = 0;
                            self.window_us = 0;
                            self.timing_health = TimingHealth::Recovering {
                                since: completed_at,
                            };
                            if let Some(trace) = &stats.feedback_trace {
                                trace.record(crate::FeedbackTraceEvent::Lock {
                                    locked: false,
                                    stale: true,
                                });
                            }
                            log_warn!("[USB] Empty implicit completion: one measured-cadence wire-silence pattern; source health lease unchanged");
                        } else {
                            return Err(ImplicitFault::Transport("empty implicit feedback pattern while running (no bounded proven silence available)".into()));
                        }
                    } else {
                        self.prime_count = 0;
                        self.fifo.clear();
                        self.window_frames = 0;
                        self.window_us = 0;
                        if let Some(proof) = self.startup_rate_proof.as_mut() {
                            proof.reset();
                            self.startup_rate_proved = false;
                            let previous = stats.feedback_locked.swap(false, Ordering::AcqRel);
                            if previous {
                                if let Some(trace) = &stats.feedback_trace {
                                    trace.record(crate::FeedbackTraceEvent::Lock {
                                        locked: false,
                                        stale: false,
                                    });
                                }
                            }
                        }
                    }
                }
                Ok(ImplicitObservation::MissingTiming {
                    bad_packets,
                    valid_frames,
                }) => {
                    if status != LIBUSB_TRANSFER_COMPLETED {
                        if let Some(capture) = &self.capture {
                            capture.note_recoverable_transfer_failure();
                        }
                    }
                    stats
                        .implicit_invalid_packets
                        .fetch_add(bad_packets as u64, Ordering::Relaxed);
                    stats.feedback_rejected.fetch_add(1, Ordering::Relaxed);
                    log_warn!(
                        "implicit_timing_gap: bad_packets={} valid_frames={}",
                        bad_packets,
                        valid_frames
                    );
                    self.window_frames = 0;
                    self.window_us = 0;
                    let previous_health = self.timing_health;
                    if self.running && previous_health == TimingHealth::Priming {
                        stats.feedback_locked.store(false, Ordering::Release);
                    }
                    self.timing_health = previous_health.missing(completed_at, self.running)?;
                    if previous_health == TimingHealth::Priming {
                        self.gap_cadence
                            .advance(self.scratch.len(), false)
                            .ok_or(ImplicitFault::Geometry)?;
                        self.fifo.clear();
                        self.prime_count = 0;
                        if let Some(proof) = self.startup_rate_proof.as_mut() {
                            proof.reset();
                            self.startup_rate_proved = false;
                        }
                    } else {
                        let pattern = self
                            .gap_cadence
                            .advance(self.scratch.len(), true)
                            .ok_or(ImplicitFault::TimingUntrusted)?;
                        self.fifo
                            .push_silence(&pattern)
                            .map_err(|_| ImplicitFault::FifoOverrun)?;
                        stats.feedback_locked.store(false, Ordering::Release);
                    }
                }
                Err(error) => return Err(error),
            }
            capture_result.map_err(|error| {
                if error == Error::DeviceLost {
                    ImplicitFault::Removed
                } else {
                    ImplicitFault::Transport(error.to_string())
                }
            })?;
            if !usb_stop_requested(stop) {
                self.slots[index]
                    .submit_capture(
                        handle,
                        self.source.address,
                        self.source.max_packet_bytes,
                        stats.feedback_trace.as_deref(),
                    )
                    .map_err(ImplicitFault::Transport)?;
                self.order.push_back(index);
            }
        }
        stats
            .implicit_fifo_depth
            .store(self.fifo.depth, Ordering::Relaxed);
        stats
            .implicit_fifo_high_water
            .fetch_max(self.fifo.depth, Ordering::Relaxed);
        if !self.running && self.started.elapsed() >= self.startup_budget {
            return Err(ImplicitFault::Transport("startup deadline expired".into()));
        }
        Ok(())
    }
    pub(super) fn primed(&self) -> bool {
        (self.startup_rate_proof.is_none() || self.startup_rate_proved)
            && self.prime_count >= self.prime_target
    }
    pub(super) fn pattern_is_silence(&self) -> bool {
        self.fifo.last_pop_silence
    }
    pub(super) fn take_pattern(&mut self, stats: &RuntimeStats) -> Option<Vec<usize>> {
        let value = self.fifo.pop()?;
        stats
            .implicit_fifo_depth
            .store(self.fifo.depth, Ordering::Relaxed);
        Some(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn source_slot() -> (ImplicitFeedbackSource, IsoTransferSlot) {
        let source = ImplicitFeedbackSource {
            interface_number: 1,
            alt_setting: 1,
            address: 0x81,
            bytes_per_frame: 4,
            service_interval_us: 1_000,
            max_packet_bytes: 12,
        };
        let mut slot = IsoTransferSlot::new(0, 2, 24).unwrap();
        slot.packet_sizes.fill(12);
        slot.submitted_bytes = 24;
        slot.submitted_endpoint = source.address;
        unsafe {
            let transfer = slot.transfer.as_mut();
            transfer.endpoint = source.address;
            transfer.length = 24;
            transfer.buffer = (*slot.user_data).buffer.as_mut_ptr();
            transfer.num_iso_packets = 2;
            for descriptor in
                std::slice::from_raw_parts_mut(transfer.iso_packet_desc.as_mut_ptr(), 2)
            {
                descriptor.length = 12;
                descriptor.actual_length = 8;
                descriptor.status = LIBUSB_TRANSFER_COMPLETED;
            }
        }
        (source, slot)
    }
    fn inspect_source(
        source: &ImplicitFeedbackSource,
        slot: &IsoTransferSlot,
        status: i32,
    ) -> Result<bool, String> {
        let mut packets = [crate::capture::CapturePacket {
            requested_bytes: 0,
            actual_bytes: 0,
            succeeded: false,
        }; 2];
        inspect_implicit_pattern(
            slot,
            status,
            source,
            48_000,
            8,
            24,
            &mut packets,
            &mut [0; 2],
        )
        .map(|result| matches!(result, ImplicitObservation::Complete { .. }))
        .map_err(|error| error.to_string())
    }
    #[test]
    fn implicit_source_admission_uses_immutable_submission_geometry() {
        let (source, mut slot) = source_slot();
        assert_eq!(
            inspect_source(&source, &slot, LIBUSB_TRANSFER_COMPLETED),
            Ok(true)
        );
        unsafe {
            let descriptor = &mut *slot.transfer.as_mut().iso_packet_desc.as_mut_ptr();
            descriptor.length = 16;
            descriptor.actual_length = 16;
        }
        assert!(inspect_source(&source, &slot, LIBUSB_TRANSFER_COMPLETED).is_err());
        unsafe {
            let descriptor = &mut *slot.transfer.as_mut().iso_packet_desc.as_mut_ptr();
            descriptor.length = 12;
            descriptor.actual_length = 8;
            slot.transfer.as_mut().endpoint = 0x82;
        }
        assert!(inspect_source(&source, &slot, LIBUSB_TRANSFER_COMPLETED).is_err());
        unsafe {
            slot.transfer.as_mut().endpoint = source.address;
            slot.transfer.as_mut().length = 28;
        }
        assert!(inspect_source(&source, &slot, LIBUSB_TRANSFER_COMPLETED).is_err());
        unsafe {
            slot.transfer.as_mut().length = 24;
            slot.transfer.as_mut().buffer = std::ptr::null_mut();
        }
        assert!(inspect_source(&source, &slot, LIBUSB_TRANSFER_COMPLETED).is_err());
        unsafe {
            slot.transfer.as_mut().buffer = (*slot.user_data).buffer.as_mut_ptr();
        }
        slot.packet_sizes[1] = 8;
        assert!(inspect_source(&source, &slot, LIBUSB_TRANSFER_COMPLETED).is_err());
    }
    #[test]
    fn implicit_source_packet_disconnect_needs_no_duplex_attachment() {
        let (source, mut slot) = source_slot();
        unsafe {
            (*slot.transfer.as_mut().iso_packet_desc.as_mut_ptr().add(1)).status =
                LIBUSB_TRANSFER_NO_DEVICE;
        }
        assert!(inspect_source(&source, &slot, LIBUSB_TRANSFER_COMPLETED)
            .unwrap_err()
            .contains("disconnected"));
        assert!(inspect_source(&source, &slot, LIBUSB_TRANSFER_NO_DEVICE)
            .unwrap_err()
            .contains("disconnected"));
        assert!(inspect_source(&source, &slot, LIBUSB_TRANSFER_TIMED_OUT)
            .unwrap_err()
            .contains("disconnected"));
    }
    #[test]
    fn timing_gap_preserves_real_fifo_and_requires_new_qualification() {
        let now = Instant::now();
        let mut fifo = PatternFifo::new(2, 2);
        assert_eq!(
            TimingHealth::Priming.missing(now, false),
            Ok(TimingHealth::Priming)
        );
        assert_eq!(
            TimingHealth::Priming.missing(now, true),
            Err(ImplicitFault::TimingUntrusted)
        );
        fifo.push(&[8, 16]).unwrap();
        let health = TimingHealth::Trusted.missing(now, true).unwrap();
        assert_eq!(fifo.depth, 1);
        assert_eq!(
            health.missing(now, true),
            Err(ImplicitFault::TimingBudgetExhausted)
        );
        assert_eq!(
            health.check_deadline(now + Duration::from_millis(121), Duration::from_millis(120)),
            Err(ImplicitFault::TimingStale)
        );
        fifo.push(&[16, 8]).unwrap();
        assert!(fifo.push(&[24, 0]).is_err());
        assert_eq!(fifo.pop(), Some(vec![8, 16]));
        assert_eq!(fifo.pop(), Some(vec![16, 8]));
        assert_eq!(fifo.pop(), None);
        // Only the caller's completed, valid cadence window promotes to Trusted.
        assert!(TimingHealth::Trusted.missing(now, true).is_ok());
    }

    #[test]
    fn failed_timing_packet_preserves_valid_sibling_without_a_schedule() {
        let (source, mut slot) = source_slot();
        unsafe {
            (*slot.transfer.as_mut().iso_packet_desc.as_mut_ptr().add(1)).status =
                LIBUSB_TRANSFER_ERROR;
        }
        let mut packets = [crate::capture::CapturePacket {
            requested_bytes: 0,
            actual_bytes: 0,
            succeeded: false,
        }; 2];
        let mut pattern = [99; 2];
        assert_eq!(
            inspect_implicit_pattern(
                &slot,
                LIBUSB_TRANSFER_COMPLETED,
                &source,
                48_000,
                8,
                24,
                &mut packets,
                &mut pattern
            ),
            Ok(ImplicitObservation::MissingTiming {
                bad_packets: 1,
                valid_frames: 2
            })
        );
        assert!(packets[0].succeeded);
        assert!(!packets[1].succeeded);
        assert_eq!(pattern[1], 99);
        unsafe {
            (*slot.transfer.as_mut().iso_packet_desc.as_mut_ptr().add(1)).status =
                LIBUSB_TRANSFER_COMPLETED;
            (*slot.transfer.as_mut().iso_packet_desc.as_mut_ptr().add(1)).actual_length = 0;
        }
        assert_eq!(
            inspect_implicit_pattern(
                &slot,
                LIBUSB_TRANSFER_COMPLETED,
                &source,
                48_000,
                8,
                24,
                &mut packets,
                &mut pattern
            ),
            Ok(ImplicitObservation::MissingTiming {
                bad_packets: 1,
                valid_frames: 2
            })
        );
        assert_eq!(packets[0].actual_bytes, 8);
        assert!(packets[1].succeeded);
        assert_eq!(
            inspect_implicit_pattern(
                &slot,
                LIBUSB_TRANSFER_COMPLETED,
                &source,
                1000,
                8,
                24,
                &mut packets,
                &mut pattern
            ),
            Ok(ImplicitObservation::Complete {
                frames: 2,
                empty_slots: 1
            })
        );
        assert_eq!(pattern, [16, 0]);
        assert_eq!(
            inspect_implicit_pattern(
                &slot,
                LIBUSB_TRANSFER_COMPLETED,
                &source,
                1001,
                8,
                24,
                &mut packets,
                &mut pattern
            ),
            Ok(ImplicitObservation::MissingTiming {
                bad_packets: 1,
                valid_frames: 2
            })
        );
    }

    #[test]
    fn delayed_callbacks_preserve_observed_freshness_and_monotonicity() {
        let started_at = Instant::now();
        let mut last_valid = started_at;
        let completed_at = started_at + Duration::from_millis(25);
        let pumped_at = started_at + Duration::from_millis(250);
        assert_eq!(
            record_valid_completion(&mut last_valid, completed_at, started_at),
            25
        );
        assert_eq!(
            pumped_at.duration_since(last_valid),
            Duration::from_millis(225)
        );
        assert_eq!(
            record_valid_completion(
                &mut last_valid,
                started_at + Duration::from_millis(10),
                started_at
            ),
            25
        );
        assert_eq!(last_valid, completed_at);
        assert_eq!(
            record_valid_completion(
                &mut last_valid,
                started_at + Duration::from_millis(40),
                started_at
            ),
            40
        );
        assert_eq!(
            record_valid_completion(&mut last_valid, completed_at, pumped_at),
            0
        );
        assert_eq!(last_valid, started_at + Duration::from_millis(40));
    }
    #[test]
    fn vendor_rate_proof_requires_contiguous_service_time() {
        let mut window = VendorRateWindow::new(96_000);
        assert_eq!(window.observe(9_504, 99_000), None);
        window.reset();
        assert_eq!(window.observe(96, 1_000), None);
        assert_eq!(window.observe(9_504, 99_000), Some(true));
        assert_eq!(window.observe(4_800, 100_000), Some(false));
        assert_eq!(window.observe(9_504, 99_000), None);
        assert_eq!(window.observe(0, 1_000), None);
        assert_eq!(window.observe(96, 1_000), None);
    }
    #[test]
    fn vendor_rate_proof_counts_fractional_service_packets() {
        let mut window = VendorRateWindow::new(44_100);
        for _ in 0..79 {
            assert_eq!(window.observe(55, 1_250), None);
        }
        assert_eq!(window.observe(65, 1_250), Some(true));
    }
    #[test]
    fn whole_frames_and_mixed_strides() {
        let mut pattern = [0; 2];
        assert_eq!(
            capture_pattern(
                [(0, 288, 294), (0, 294, 294)].into_iter(),
                6,
                8,
                392,
                &mut pattern
            ),
            Ok(true)
        );
        assert_eq!(pattern, [384, 392]);
        assert!(capture_pattern(
            [(0, 289, 294), (0, 294, 294)].into_iter(),
            6,
            8,
            392,
            &mut pattern
        )
        .is_err());
        assert!(capture_pattern(
            [(0, 300, 300), (0, 294, 294)].into_iter(),
            6,
            8,
            392,
            &mut pattern
        )
        .is_err());
    }
    #[test]
    fn fifo_full_wrap_and_no_replay() {
        let mut fifo = PatternFifo::new(2, 2);
        fifo.push(&[44, 45]).unwrap();
        fifo.push(&[45, 44]).unwrap();
        assert!(fifo.push(&[99, 99]).is_err());
        assert_eq!(fifo.pop(), Some(vec![44, 45]));
        fifo.push(&[46, 44]).unwrap();
        assert_eq!(fifo.pop(), Some(vec![45, 44]));
        assert_eq!(fifo.pop(), Some(vec![46, 44]));
        assert_eq!(fifo.pop(), None);
        fifo.push(&[1, 2]).unwrap();
        fifo.clear();
        assert_eq!(fifo.pop(), None);
    }
    #[test]
    fn fractional_patterns_conserve_frames_and_reset_on_restart() {
        let mut fifo = PatternFifo::new(16, 10);
        let mut measured = 0;
        let mut scheduled = 0;
        let mut phase = 0;
        for _ in 0..100 {
            let mut pattern = [0; 10];
            for packet in &mut pattern {
                phase += 44_100;
                let frames = phase / 1000;
                phase %= 1000;
                *packet = frames * 8;
                measured += frames;
            }
            fifo.push(&pattern).unwrap();
            scheduled += fifo.pop().unwrap().iter().sum::<usize>() / 8;
        }
        assert_eq!(measured, 44_100);
        assert_eq!(scheduled, measured);
        assert_eq!(fifo.pop(), None);
        let restart = PatternFifo::new(16, 10);
        assert_eq!(restart.depth, 0);
    }
    #[test]
    fn rate_windows_allow_quantization_but_reject_persistent_wrong_rate() {
        assert!(rate_window_valid(4410, 100_000, 44_100));
        assert!(rate_window_valid(4411, 100_000, 44_100));
        assert!(!rate_window_valid(4800, 100_000, 44_100));
        assert!(!rate_window_valid(4000, 100_000, 44_100));
    }
    #[test]
    fn empty_incomplete_failed_and_partial_patterns() {
        let mut pattern = [0; 2];
        assert_eq!(
            capture_pattern(
                [(0, 0, 100), (0, 0, 100)].into_iter(),
                4,
                8,
                200,
                &mut pattern
            ),
            Ok(false)
        );
        for input in [
            vec![(0, 4, 100)],
            vec![(0, 4, 100), (1, 4, 100)],
            vec![(0, 4, 100), (0, 0, 100)],
            vec![(0, 104, 100), (0, 4, 100)],
        ] {
            assert!(capture_pattern(input.into_iter(), 4, 8, 200, &mut pattern).is_err());
        }
    }
}

/// Exercise the production cadence and FIFO without a libusb device.
#[cfg(test)]
pub(super) fn separated_gap_patterns_for_test(mut consume: impl FnMut(Vec<usize>, bool)) {
    let mut cadence = GapCadence::new(48_000, 125, 4, 24);
    let mut fifo = PatternFifo::new(4, 2);
    let mut health = TimingHealth::Trusted;
    let now = Instant::now();
    for _ in 0..3 {
        health = health.missing(now, true).unwrap();
        assert!(matches!(health, TimingHealth::Recovering { .. }));
        let gap = cadence.advance(2, true).unwrap();
        fifo.push_silence(&gap).unwrap();
        let pattern = fifo.pop().unwrap();
        consume(pattern, fifo.last_pop_silence);
        assert_eq!(fifo.depth, 0);
        health = TimingHealth::Trusted; // valid timing requalifies before the next gap
    }
    let _ = health;
}

#[cfg(test)]
mod measured_silence_tests {
    #[test]
    fn separated_gaps_keep_fifo_slots_and_fractional_synthetic_phase() {
        use super::*;
        let mut cadence = GapCadence::new(44_100, 125, 4, 24);
        let mut fifo = PatternFifo::new(4, 3);
        let mut total = 0;
        let mut health = TimingHealth::Trusted;
        let now = Instant::now();
        for gap in 0..80 {
            health = health.missing(now, true).unwrap();
            let pattern = cadence.advance(3, true).unwrap();
            total += pattern.iter().sum::<usize>() / 4;
            fifo.push_silence(&pattern).unwrap();
            assert_eq!(fifo.depth, 1);
            assert_eq!(fifo.pop().unwrap(), pattern);
            assert!(fifo.last_pop_silence);
            assert_eq!(fifo.depth, 0);
            if gap == 0 {
                assert_eq!(
                    health.missing(now, true),
                    Err(ImplicitFault::TimingBudgetExhausted)
                );
            }
            health = TimingHealth::Trusted; // a valid window requalified the source
        }
        assert_eq!(total, 1323);
        assert!(GapCadence::new(44_100, 125, 4, 19)
            .advance(1, true)
            .is_none());
    }

    use super::*;
    #[test]
    fn silence_requires_previous_proof_and_is_bounded_to_one_pattern() {
        let mut fallback = MeasuredSilence::default();
        assert!(fallback.take(true, true, true).is_none());
        fallback.observed(&[20, 24, 20, 24]);
        assert!(fallback.take(false, true, true).is_none());
        assert!(fallback.take(true, false, true).is_none());
        assert!(fallback.take(true, true, false).is_none());
        assert_eq!(fallback.take(true, true, true), Some(vec![20, 24, 20, 24]));
        assert!(fallback.take(true, true, true).is_none());
        fallback.observed(&[24, 20, 24, 20]);
        assert_eq!(fallback.take(true, true, true), Some(vec![24, 20, 24, 20]));
    }
    #[test]
    fn empty_error_recovery_rejects_detach_halt_partial_payload_and_changed_schedule() {
        let source = ImplicitFeedbackSource {
            interface_number: 1,
            alt_setting: 1,
            address: 0x81,
            bytes_per_frame: 4,
            service_interval_us: 1000,
            max_packet_bytes: 12,
        };
        let mut slot = IsoTransferSlot::new(0, 2, 24).unwrap();
        slot.packet_sizes = vec![12, 12];
        slot.submitted_endpoint = 0x81;
        slot.submitted_bytes = 24;
        unsafe {
            let buffer = &mut (*slot.user_data).buffer;
            let transfer = slot.transfer.as_mut();
            transfer.endpoint = 0x81;
            transfer.length = 24;
            transfer.num_iso_packets = 2;
            transfer.buffer = buffer.as_mut_ptr();
            let descriptors =
                std::slice::from_raw_parts_mut(transfer.iso_packet_desc.as_mut_ptr(), 2);
            for descriptor in descriptors {
                descriptor.length = 12;
                descriptor.actual_length = 0;
                descriptor.status = LIBUSB_TRANSFER_ERROR;
            }
        }
        let mut packets = vec![
            crate::capture::CapturePacket {
                requested_bytes: 0,
                actual_bytes: 0,
                succeeded: false
            };
            2
        ];
        assert!(known_empty_implicit_completion(
            &slot,
            LIBUSB_TRANSFER_COMPLETED,
            &source,
            &mut packets
        ));
        assert!(!known_empty_implicit_completion(
            &slot,
            LIBUSB_TRANSFER_ERROR,
            &source,
            &mut packets
        ));
        for status in [
            LIBUSB_TRANSFER_NO_DEVICE,
            LIBUSB_TRANSFER_STALL,
            LIBUSB_TRANSFER_CANCELLED,
        ] {
            unsafe {
                (*slot.transfer.as_ptr())
                    .iso_packet_desc
                    .as_mut_ptr()
                    .as_mut()
                    .unwrap()
                    .status = status;
            }
            assert!(!known_empty_implicit_completion(
                &slot,
                LIBUSB_TRANSFER_COMPLETED,
                &source,
                &mut packets
            ));
        }
        unsafe {
            let descriptor = (*slot.transfer.as_ptr())
                .iso_packet_desc
                .as_mut_ptr()
                .as_mut()
                .unwrap();
            descriptor.status = LIBUSB_TRANSFER_ERROR;
            descriptor.actual_length = 4;
        }
        assert!(!known_empty_implicit_completion(
            &slot,
            LIBUSB_TRANSFER_COMPLETED,
            &source,
            &mut packets
        ));
        unsafe {
            (*slot.transfer.as_ptr())
                .iso_packet_desc
                .as_mut_ptr()
                .as_mut()
                .unwrap()
                .actual_length = 0;
        }
        slot.packet_sizes[0] = 8;
        assert!(!known_empty_implicit_completion(
            &slot,
            LIBUSB_TRANSFER_COMPLETED,
            &source,
            &mut packets
        ));
    }

    #[test]
    fn queued_silence_keeps_its_tag_through_fifo_wrap_and_clear() {
        let mut fifo = PatternFifo::new(2, 2);
        fifo.push(&[20, 24]).unwrap();
        fifo.push_silence(&[24, 20]).unwrap();
        assert_eq!(fifo.pop(), Some(vec![20, 24]));
        assert!(!fifo.last_pop_silence);
        fifo.push(&[20, 24]).unwrap();
        assert_eq!(fifo.pop(), Some(vec![24, 20]));
        assert!(fifo.last_pop_silence);
        assert_eq!(fifo.pop(), Some(vec![20, 24]));
        assert!(!fifo.last_pop_silence);
        fifo.push_silence(&[20, 24]).unwrap();
        fifo.clear();
        assert!(!fifo.last_pop_silence);
        assert!(fifo.pop().is_none());
    }
}
