//! Whole-frame timing patterns, kept in capture submission order by the USB worker.
use super::*;
use std::collections::VecDeque;

struct PatternFifo {
    storage: Vec<Vec<usize>>,
    read: usize,
    depth: usize,
}
impl PatternFifo {
    fn new(capacity: usize, packets: usize) -> Self {
        Self { storage: vec![vec![0; packets]; capacity], read: 0, depth: 0 }
    }
    fn clear(&mut self) { self.read = 0; self.depth = 0; }
    fn push(&mut self, pattern: &[usize]) -> Result<(), String> {
        if self.depth == self.storage.len() { return Err("implicit feedback FIFO overflow".into()); }
        let index = (self.read + self.depth) % self.storage.len();
        if pattern.len() != self.storage[index].len() { return Err("incomplete implicit feedback pattern".into()); }
        self.storage[index].copy_from_slice(pattern);
        self.depth += 1;
        Ok(())
    }
    fn pop(&mut self) -> Option<Vec<usize>> {
        if self.depth == 0 { return None; }
        let value = self.storage[self.read].clone();
        self.read = (self.read + 1) % self.storage.len();
        self.depth -= 1;
        Some(value)
    }
}

fn capture_pattern(packets: impl Iterator<Item = (i32, usize, usize)>, in_stride: usize,
    out_stride: usize, max_out: usize, pattern: &mut [usize]) -> Result<bool, String> {
    if in_stride == 0 || out_stride == 0 { return Err("zero implicit frame stride".into()); }
    let mut count = 0;
    let mut empty = true;
    for (index, (status, actual, requested)) in packets.enumerate() {
        if index >= pattern.len() || status != LIBUSB_TRANSFER_COMPLETED || actual > requested || actual % in_stride != 0 {
            return Err("invalid implicit feedback packet".into());
        }
        let bytes = (actual / in_stride).checked_mul(out_stride).ok_or("implicit size overflow")?;
        if bytes > max_out { return Err("implicit feedback exceeds OUT capacity".into()); }
        pattern[index] = bytes;
        empty &= actual == 0;
        count += 1;
    }
    if count != pattern.len() { return Err("incomplete implicit feedback pattern".into()); }
    if !empty && pattern.contains(&0) { return Err("implicit feedback packet gap".into()); }
    Ok(!empty)
}

fn rate_window_valid(frames: u64, service_us: u64, rate: u32) -> bool {
    let expected = f64::from(rate) * service_us as f64 / 1_000_000.0;
    (frames as f64 - expected).abs() <= expected * 0.02 + 1.0
}

pub(super) struct ImplicitFeedback {
    pub(super) slots: Vec<IsoTransferSlot>,
    order: VecDeque<usize>,
    fifo: PatternFifo,
    scratch: Vec<usize>,
    source: ImplicitFeedbackSource,
    out_stride: usize,
    max_out: usize,
    rate: u32,
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
    pub(super) fn new(source: ImplicitFeedbackSource, geometry: IsoQueueGeometry, out_stride: usize,
        max_out: usize, rate: u32) -> Result<Self, String> {
        let mut slots = Vec::with_capacity(geometry.transfer_slots);
        for index in 0..geometry.transfer_slots {
            slots.push(IsoTransferSlot::new(index, geometry.packets_per_transfer,
                geometry.packets_per_transfer * source.max_packet_bytes)?);
        }
        Ok(Self { slots, order: VecDeque::with_capacity(geometry.transfer_slots),
            fifo: PatternFifo::new(2 * geometry.transfer_slots, geometry.packets_per_transfer),
            scratch: vec![0; geometry.packets_per_transfer], source, out_stride, max_out, rate,
            window_frames: 0, window_us: 0, bad_windows: 0, prime_count: 0,
            prime_target: geometry.transfer_slots, running: false, started: Instant::now(),
            startup_budget: Duration::from_micros(500_000.max(4 * geometry.transfer_us())),
            last_valid: Instant::now(), out_of_order_seen: vec![false; geometry.transfer_slots],
            capture: None, capture_scratch: vec![crate::capture::CapturePacket {
                requested_bytes: 0, actual_bytes: 0, succeeded: false }; geometry.packets_per_transfer] })
    }
    pub(super) fn attach_capture(&mut self, capture: Option<Arc<duplex::Attachment>>) { self.capture = capture; }
    pub(super) fn start(&mut self, handle: &DeviceHandle<Context>, stop: &AtomicBool) -> Result<(), String> {
        self.started = Instant::now(); self.last_valid = self.started;
        for (index, slot) in self.slots.iter_mut().enumerate() {
            if usb_stop_requested(stop) { break; }
            slot.submit_capture(handle, self.source.address, self.source.max_packet_bytes)?;
            self.order.push_back(index);
        }
        Ok(())
    }
    pub(super) fn pump(&mut self, handle: &DeviceHandle<Context>, stats: &RuntimeStats, stop: &AtomicBool) -> Result<(), String> {
        if let Some(&head) = self.order.front() {
            for &index in self.order.iter().skip(1) {
                if !self.slots[head].completion_ready() && self.slots[index].completion_ready() && !self.out_of_order_seen[index] {
                    self.out_of_order_seen[index] = true;
                    stats.implicit_out_of_order.fetch_add(1, Ordering::Relaxed);
                    if let Some(capture) = self.capture.as_ref() { capture.reordered(); }
                }
            }
        }
        while let Some(&index) = self.order.front() {
            if usb_stop_requested(stop) { break; }
            let Some((status, _, _)) = self.slots[index].take_completion() else { break; };
            self.order.pop_front(); self.out_of_order_seen[index] = false;
            stats.implicit_in_transfers.fetch_add(1, Ordering::Relaxed);
            let capture_result = self.capture.as_ref().map(|capture|
                capture.ingest(&self.slots[index], status, &mut self.capture_scratch)).transpose();
            let result = if status == LIBUSB_TRANSFER_COMPLETED {
                let transfer = unsafe { self.slots[index].transfer.as_ref() };
                let descriptors = if transfer.num_iso_packets != self.scratch.len() as i32 {
                    self.slots[index].mark_completed();
                    return Err("implicit capture packet descriptor count changed".into());
                } else { unsafe { std::slice::from_raw_parts(transfer.iso_packet_desc.as_ptr(), self.scratch.len()) } };
                capture_pattern(descriptors.iter().map(|p| (p.status, p.actual_length as usize, p.length as usize)),
                    self.source.bytes_per_frame, self.out_stride, self.max_out, &mut self.scratch)
            } else {
                if status == LIBUSB_TRANSFER_NO_DEVICE {
                    self.slots[index].mark_completed();
                    return Err("USB DAC disconnected (implicit feedback)".into());
                }
                if status == LIBUSB_TRANSFER_TIMED_OUT { stats.feedback_timeouts.fetch_add(1, Ordering::Relaxed); }
                Err(format!("implicit capture transfer status {}", status))
            };
            self.slots[index].mark_completed();
            match result {
                Ok(true) => {
                    if let Err(error) = self.fifo.push(&self.scratch) {
                        stats.implicit_overruns.fetch_add(1, Ordering::Relaxed); return Err(error);
                    }
                    self.prime_count = (self.prime_count + 1).min(self.prime_target);
                    self.last_valid = Instant::now();
                    let frames = self.scratch.iter().sum::<usize>() / self.out_stride;
                    stats.feedback_reports.fetch_add(1, Ordering::Relaxed);
                    stats.implicit_measured_frames.fetch_add(frames as u64, Ordering::Relaxed);
                    stats.feedback_last_report_ms.store(stats.started_at.elapsed().as_millis() as u64, Ordering::Relaxed);
                    self.window_frames += frames as u64;
                    self.window_us += self.scratch.len() as u64 * u64::from(self.source.service_interval_us);
                    if self.window_us >= 100_000 {
                        let valid = rate_window_valid(self.window_frames, self.window_us, self.rate);
                        self.bad_windows = if valid { 0 } else { self.bad_windows + 1 };
                        stats.feedback_locked.store(valid, Ordering::Release);
                        stats.feedback_rate_milli_hz.store(self.window_frames * 1_000_000_000 / self.window_us, Ordering::Relaxed);
                        self.window_frames = 0; self.window_us = 0;
                        if self.bad_windows >= 2 { return Err("implicit feedback rate outside negotiated window".into()); }
                    }
                }
                Ok(false) => {
                    stats.feedback_empty.fetch_add(1, Ordering::Relaxed);
                    if self.running { return Err("empty implicit feedback pattern while running".into()); }
                    self.prime_count = 0; self.fifo.clear(); self.window_frames = 0; self.window_us = 0;
                }
                Err(error) => {
                    stats.implicit_invalid_packets.fetch_add(1, Ordering::Relaxed);
                    stats.feedback_rejected.fetch_add(1, Ordering::Relaxed);
                    return Err(if matches!(&capture_result, Err(Error::DeviceLost)) {
                        "USB DAC disconnected (duplex implicit capture)".into()
                    } else { error });
                }
            }
            capture_result.map_err(|error| if error == Error::DeviceLost {
                "USB DAC disconnected (duplex implicit capture)".into()
            } else { error.to_string() })?;
            if !usb_stop_requested(stop) {
                self.slots[index].submit_capture(handle, self.source.address, self.source.max_packet_bytes)?;
                self.order.push_back(index);
            }
        }
        stats.implicit_fifo_depth.store(self.fifo.depth, Ordering::Relaxed);
        stats.implicit_fifo_high_water.fetch_max(self.fifo.depth, Ordering::Relaxed);
        if !self.running && self.started.elapsed() >= self.startup_budget { return Err("implicit feedback startup deadline expired".into()); }
        Ok(())
    }
    pub(super) fn primed(&self) -> bool { self.prime_count >= self.prime_target }
    pub(super) fn take_pattern(&mut self, stats: &RuntimeStats) -> Option<Vec<usize>> {
        let value = self.fifo.pop()?;
        stats.implicit_fifo_depth.store(self.fifo.depth, Ordering::Relaxed);
        Some(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test] fn whole_frames_and_mixed_strides() {
        let mut pattern = [0; 2];
        assert_eq!(capture_pattern([(0,288,294),(0,294,294)].into_iter(),6,8,392,&mut pattern),Ok(true));
        assert_eq!(pattern,[384,392]);
        assert!(capture_pattern([(0,289,294),(0,294,294)].into_iter(),6,8,392,&mut pattern).is_err());
        assert!(capture_pattern([(0,300,300),(0,294,294)].into_iter(),6,8,392,&mut pattern).is_err());
    }
    #[test] fn fifo_full_wrap_and_no_replay() {
        let mut fifo=PatternFifo::new(2,2);
        fifo.push(&[44,45]).unwrap(); fifo.push(&[45,44]).unwrap();
        assert!(fifo.push(&[99,99]).is_err());
        assert_eq!(fifo.pop(),Some(vec![44,45]));
        fifo.push(&[46,44]).unwrap();
        assert_eq!(fifo.pop(),Some(vec![45,44])); assert_eq!(fifo.pop(),Some(vec![46,44])); assert_eq!(fifo.pop(),None);
        fifo.push(&[1,2]).unwrap(); fifo.clear(); assert_eq!(fifo.pop(),None);
    }
    #[test] fn fractional_patterns_conserve_frames_and_reset_on_restart() {
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
        let restart = PatternFifo::new(16,10);
        assert_eq!(restart.depth, 0);
    }
    #[test] fn rate_windows_allow_quantization_but_reject_persistent_wrong_rate() {
        assert!(rate_window_valid(4410, 100_000, 44_100));
        assert!(rate_window_valid(4411, 100_000, 44_100));
        assert!(!rate_window_valid(4800, 100_000, 44_100));
        assert!(!rate_window_valid(4000, 100_000, 44_100));
    }
    #[test] fn empty_incomplete_failed_and_partial_patterns() {
        let mut pattern=[0;2];
        assert_eq!(capture_pattern([(0,0,100),(0,0,100)].into_iter(),4,8,200,&mut pattern),Ok(false));
        for input in [vec![(0,4,100)],vec![(0,4,100),(1,4,100)],vec![(0,4,100),(0,0,100)],vec![(0,104,100),(0,4,100)]] {
            assert!(capture_pattern(input.into_iter(),4,8,200,&mut pattern).is_err());
        }
    }
}
