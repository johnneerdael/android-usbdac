//! Whole-frame timing patterns, kept in capture submission order by the USB worker.
use super::*;
use std::collections::VecDeque;

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
    out_stride: usize,
    max_out: usize,
    packets: &mut [crate::capture::CapturePacket],
    pattern: &mut [usize],
) -> Result<bool, String> {
    // Removal metadata takes priority over other transfer failures. Only read
    // the descriptor allocation when its count matches the owned schedule.
    let transfer = unsafe { slot.transfer.as_ref() };
    if transfer.num_iso_packets == packets.len() as i32 {
        let descriptors =
            unsafe { std::slice::from_raw_parts(transfer.iso_packet_desc.as_ptr(), packets.len()) };
        if descriptors
            .iter()
            .any(|packet| packet.status == LIBUSB_TRANSFER_NO_DEVICE)
        {
            return Err("USB DAC disconnected (implicit feedback)".into());
        }
    }
    let (data, detached) =
        capture_stream::inspect_slot(slot, status, packets).map_err(|error| {
            if error == Error::DeviceLost {
                "USB DAC disconnected (implicit feedback)".into()
            } else {
                error.to_string()
            }
        })?;
    if detached {
        return Err("USB DAC disconnected (implicit feedback)".into());
    }
    if slot.submitted_endpoint != source.address || slot.packet_sizes.len() != packets.len() {
        return Err("implicit capture source geometry changed".into());
    }
    let mut span = 0usize;
    for (packet, &submitted) in packets.iter().zip(&slot.packet_sizes) {
        if submitted != source.max_packet_bytes || packet.requested_bytes != submitted {
            return Err("implicit capture packet geometry changed".into());
        }
        span = span
            .checked_add(submitted)
            .ok_or("implicit capture packet span overflow")?;
        if span > data.len() {
            return Err("implicit capture packet exceeds submitted buffer".into());
        }
    }
    if span != slot.submitted_bytes {
        return Err("implicit capture packet span changed".into());
    }
    capture_pattern(
        packets.iter().map(|packet| {
            (
                if packet.succeeded {
                    LIBUSB_TRANSFER_COMPLETED
                } else {
                    LIBUSB_TRANSFER_ERROR
                },
                packet.actual_bytes,
                packet.requested_bytes,
            )
        }),
        source.bytes_per_frame,
        out_stride,
        max_out,
        pattern,
    )
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
        handle: &DeviceHandle<Context>,
        stats: &RuntimeStats,
        stop: &AtomicBool,
    ) -> Result<(), String> {
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
    ) -> Result<(), String> {
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
                self.out_stride,
                self.max_out,
                &mut self.capture_scratch,
                &mut self.scratch,
            );
            let result = if result.is_err()
                && settings().has(crate::Quirk::ImplicitSilenceOnEmpty)
                && known_empty_implicit_completion(
                    &self.slots[index],
                    status,
                    &self.source,
                    &mut self.capture_scratch,
                ) {
                Ok(false)
            } else {
                result
            };
            // Failed transfers contain no presentable payload. Completed URBs
            // must pass the entire timing contract first, including packet gaps.
            let capture_result = if result.is_ok() || status != LIBUSB_TRANSFER_COMPLETED {
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
                Ok(true) => {
                    self.measured_silence.observed(&self.scratch);
                    let last_report_ms = record_valid_completion(
                        &mut self.last_valid,
                        completed_at,
                        stats.started_at,
                    );
                    let frames = self.scratch.iter().sum::<usize>() / self.out_stride;
                    stats.feedback_reports.fetch_add(1, Ordering::Relaxed);
                    stats
                        .implicit_measured_frames
                        .fetch_add(frames as u64, Ordering::Relaxed);
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
                        let verdict = proof.observe(frames as u64, service_us);
                        if verdict == Some(false) {
                            return Err(
                                "programmed vendor PCM cadence does not match requested rate"
                                    .into(),
                            );
                        }
                        if verdict == Some(true) {
                            self.startup_rate_proved = true;
                        }
                        // Discard the proof completion as well: prime a fresh,
                        // bounded FIFO only after proving the programmed clock.
                    } else {
                        if let Err(error) = self.fifo.push(&self.scratch) {
                            stats.implicit_overruns.fetch_add(1, Ordering::Relaxed);
                            return Err(error);
                        }
                        self.prime_count = (self.prime_count + 1).min(self.prime_target);
                    }
                    self.window_frames += frames as u64;
                    self.window_us +=
                        self.scratch.len() as u64 * u64::from(self.source.service_interval_us);
                    if self.window_us >= 100_000 {
                        let valid =
                            rate_window_valid(self.window_frames, self.window_us, self.rate);
                        self.bad_windows = if valid { 0 } else { self.bad_windows + 1 };
                        let previous = stats.feedback_locked.swap(valid, Ordering::AcqRel);
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
                            return Err("implicit feedback rate outside negotiated window".into());
                        }
                    }
                }
                Ok(false) => {
                    stats.feedback_empty.fetch_add(1, Ordering::Relaxed);
                    if self.running {
                        if let Some(pattern) = self.measured_silence.take(
                            settings().has(crate::Quirk::ImplicitSilenceOnEmpty),
                            self.running,
                            stats.feedback_locked.load(Ordering::Acquire),
                        ) {
                            self.fifo.push_silence(&pattern)?;
                            stats.feedback_locked.store(false, Ordering::Release);
                            if let Some(trace) = &stats.feedback_trace {
                                trace.record(crate::FeedbackTraceEvent::Lock {
                                    locked: false,
                                    stale: true,
                                });
                            }
                            log_warn!("[USB] Empty implicit completion: one measured-cadence wire-silence pattern; source health lease unchanged");
                        } else {
                            return Err("empty implicit feedback pattern while running (no bounded proven silence available)".into());
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
                Err(error) => {
                    stats
                        .implicit_invalid_packets
                        .fetch_add(1, Ordering::Relaxed);
                    stats.feedback_rejected.fetch_add(1, Ordering::Relaxed);
                    return Err(if matches!(&capture_result, Err(Error::DeviceLost)) {
                        "USB DAC disconnected (duplex implicit capture)".into()
                    } else {
                        error
                    });
                }
            }
            capture_result.map_err(|error| {
                if error == Error::DeviceLost {
                    "USB DAC disconnected (duplex implicit capture)".into()
                } else {
                    error.to_string()
                }
            })?;
            if !usb_stop_requested(stop) {
                self.slots[index].submit_capture(
                    handle,
                    self.source.address,
                    self.source.max_packet_bytes,
                    stats.feedback_trace.as_deref(),
                )?;
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
            return Err("implicit feedback startup deadline expired".into());
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
        inspect_implicit_pattern(slot, status, source, 8, 24, &mut packets, &mut [0; 2])
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

#[cfg(test)]
mod measured_silence_tests {
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
