use super::*;

#[derive(Debug)]
pub(super) struct IsoTransferCompletion {
    pub(super) status: StdMutex<Option<i32>>,
    pub(super) completed_at: StdMutex<Option<Instant>>,
    pub(super) actual_length: AtomicUsize,
}

impl IsoTransferCompletion {
    pub(super) fn new() -> Self {
        Self {
            status: StdMutex::new(None),
            completed_at: StdMutex::new(None),
            actual_length: AtomicUsize::new(0),
        }
    }

    pub(super) fn reset(&self) {
        match self.status.lock() {
            Ok(mut guard) => *guard = None,
            Err(poisoned) => *poisoned.into_inner() = None,
        }
        match self.completed_at.lock() {
            Ok(mut guard) => *guard = None,
            Err(poisoned) => *poisoned.into_inner() = None,
        }
        self.actual_length.store(0, Ordering::Release);
    }

    /// libusb has handed the transfer back and nobody has taken the result yet.
    pub(super) fn is_published(&self) -> bool {
        match self.status.lock() {
            Ok(guard) => guard.is_some(),
            Err(poisoned) => poisoned.into_inner().is_some(),
        }
    }

    pub(super) fn take(&self) -> Option<(i32, Instant, usize)> {
        let status = match self.status.lock() {
            Ok(mut guard) => guard.take()?,
            Err(poisoned) => poisoned.into_inner().take()?,
        };
        let completed_at = match self.completed_at.lock() {
            Ok(mut guard) => guard.take(),
            Err(poisoned) => poisoned.into_inner().take(),
        }
        .unwrap_or_else(Instant::now);
        Some((
            status,
            completed_at,
            self.actual_length.load(Ordering::Acquire),
        ))
    }
}

#[derive(Debug)]
pub(super) struct IsoTransferUserData {
    pub(super) completion: Arc<IsoTransferCompletion>,
    pub(super) buffer: Vec<u8>,
}

pub(super) struct IsoTransferSlot {
    pub(super) transfer: std::ptr::NonNull<libusb_transfer>,
    pub(super) user_data: *mut IsoTransferUserData,
    pub(super) slot_index: usize,
    pub(super) generation: u64,
    pub(super) submitted_endpoint: u8,
    pub(super) submitted_bytes: usize,
    pub(super) packet_sizes: Vec<usize>,
    pub(super) queued_frame_count: usize,
    pub(super) queued_underrun: bool,
    /// The queued transfer carries held wire silence, not audio.
    pub(super) queued_held: bool,
    /// Frames of the queued transfer taken from the ring (not padding or hold).
    pub(super) queued_music_frames: usize,
    /// Flush epoch the queued transfer was filled in.
    pub(super) queued_epoch: u64,
    pub(super) submitted_at: Option<Instant>,
    pub(super) in_flight: bool,
    retirement_context: Option<Context>,
    /// The flush asked libusb to cancel this transfer. Cleared when it comes
    /// back, whatever its status: a transfer that finished first is still
    /// accounted as played.
    pub(super) cancelled_for_flush: bool,
}

pub(super) struct IsoTransferPayload {
    pub(super) packet_sizes: Vec<usize>,
    pub(super) packet_samples: Vec<i32>,
    pub(super) transfer_buffer: Vec<u8>,
    pub(super) underrun: bool,
    /// Wire silence sent while paused or while a flushed ring refills. It
    /// consumed nothing from the ring, so it is not an underrun, and a transfer
    /// that carries it cannot have lost audio.
    pub(super) held: bool,
    pub(super) frame_count: usize,
    /// Frames taken from the ring: `frame_count` minus padding and holds.
    pub(super) music_frames: usize,
    /// Flush epoch of the ring when this transfer was filled.
    pub(super) epoch: u64,
}

impl IsoTransferSlot {
    pub(super) fn new(slot_index: usize, packet_count: usize, buffer_capacity: usize) -> Result<Self, String> {
        let completion = Arc::new(IsoTransferCompletion::new());
        let user_data = Box::new(IsoTransferUserData {
            completion,
            buffer: vec![0u8; buffer_capacity.max(1)],
        });
        let user_data_ptr = Box::into_raw(user_data);
        let transfer = unsafe { libusb_alloc_transfer(packet_count as i32) };

        let Some(transfer) = std::ptr::NonNull::new(transfer) else {
            unsafe {
                drop(Box::from_raw(user_data_ptr));
            }
            return Err("libusb_alloc_transfer returned null".to_string());
        };

        Ok(Self {
            transfer,
            user_data: user_data_ptr,
            slot_index,
            generation: 0,
            submitted_endpoint: 0,
            submitted_bytes: 0,
            packet_sizes: vec![0; packet_count],
            queued_frame_count: 0,
            queued_underrun: false,
            queued_held: false,
            queued_music_frames: 0,
            queued_epoch: 0,
            submitted_at: None,
            in_flight: false,
            retirement_context: None,
            cancelled_for_flush: false,
        })
    }

    pub(super) fn submit(
        &mut self,
        handle: &DeviceHandle<Context>,
        endpoint: u8,
        payload: IsoTransferPayload,
    ) -> Result<(), String> {
        if self.in_flight {
            return Err("isochronous transfer slot already has a transfer in flight".to_string());
        }

        if payload.packet_sizes.len() != self.packet_sizes.len() {
            return Err(format!(
                "isochronous packet count mismatch: slot={}, payload={}",
                self.packet_sizes.len(),
                payload.packet_sizes.len()
            ));
        }

        self.retirement_context = Some(handle.context().clone());
        unsafe {
            let user_data = &mut *self.user_data;
            if payload.transfer_buffer.len() > user_data.buffer.len() {
                return Err(format!(
                    "isochronous transfer buffer too small: capacity={}, payload={}",
                    user_data.buffer.len(),
                    payload.transfer_buffer.len()
                ));
            }
            user_data.buffer[..payload.transfer_buffer.len()]
                .copy_from_slice(&payload.transfer_buffer);
            user_data.completion.reset();

            libusb_fill_iso_transfer(
                self.transfer.as_mut(),
                handle.as_raw(),
                endpoint,
                user_data.buffer.as_mut_ptr(),
                payload.transfer_buffer.len() as i32,
                payload.packet_sizes.len() as i32,
                iso_transfer_callback,
                self.user_data as *mut c_void,
                ISO_TRANSFER_TIMEOUT_MS,
            );

            for (index, packet_size) in payload.packet_sizes.iter().enumerate() {
                (*self
                    .transfer
                    .as_mut()
                    .iso_packet_desc
                    .as_mut_ptr()
                    .add(index))
                .length = *packet_size as u32;
            }

            let submit_result = libusb_submit_transfer(self.transfer.as_ptr());
            if submit_result != 0 {
                return Err(format!(
                    "libusb_submit_transfer failed with code {}",
                    submit_result
                ));
            }
        }

        self.generation = self.generation.saturating_add(1);
        self.submitted_endpoint = endpoint;
        self.submitted_bytes = payload.transfer_buffer.len();
        self.queued_frame_count = payload.frame_count;
        self.queued_underrun = payload.underrun;
        self.queued_held = payload.held;
        self.queued_music_frames = payload.music_frames;
        self.queued_epoch = payload.epoch;
        self.packet_sizes.copy_from_slice(&payload.packet_sizes);
        self.submitted_at = Some(Instant::now());
        self.in_flight = true;
        Ok(())
    }

    /// Reuse the capture buffer without copying or exposing captured audio.
    pub(super) fn submit_capture(&mut self, handle: &DeviceHandle<Context>, endpoint: u8, packet_bytes: usize) -> Result<(), String> {
        if self.in_flight { return Err("capture slot already submitted".into()); }
        if endpoint & 0x80 == 0 || packet_bytes == 0 || packet_bytes > u32::MAX as usize
            || self.packet_sizes.len() > i32::MAX as usize {
            return Err("invalid capture request geometry".into());
        }
        self.retirement_context = Some(handle.context().clone());
        unsafe {
            let data = &mut *self.user_data;
            data.completion.reset();
            let bytes = packet_bytes.checked_mul(self.packet_sizes.len()).ok_or("capture size overflow")?;
            if bytes > data.buffer.len() || bytes > i32::MAX as usize { return Err("capture buffer too small or request too large".into()); }
            libusb_fill_iso_transfer(self.transfer.as_mut(), handle.as_raw(), endpoint,
                data.buffer.as_mut_ptr(), bytes as i32, self.packet_sizes.len() as i32,
                iso_transfer_callback, self.user_data as *mut c_void, ISO_TRANSFER_TIMEOUT_MS);
            for index in 0..self.packet_sizes.len() {
                (*self.transfer.as_mut().iso_packet_desc.as_mut_ptr().add(index)).length = packet_bytes as u32;
            }
            let result = libusb_submit_transfer(self.transfer.as_ptr());
            if result != 0 {
                return Err(if result == libusb1_sys::constants::LIBUSB_ERROR_NO_DEVICE {
                    "USB DAC disconnected (implicit capture submit)".into()
                } else { format!("implicit capture submit failed: {}", result) });
            }
            self.packet_sizes.fill(packet_bytes);
            self.generation = self.generation.saturating_add(1);
            self.submitted_endpoint = endpoint;
            self.submitted_bytes = bytes;
            self.in_flight = true;
            self.submitted_at = Some(Instant::now());
        }
        Ok(())
    }

    pub(super) fn take_completion(&mut self) -> Option<(i32, Instant, usize)> {
        let completion = unsafe { &(*self.user_data).completion };
        completion.take()
    }

    pub(super) fn turnaround_us(&self, completed_at: Instant) -> Option<u64> {
        self.submitted_at.map(|submitted_at| {
            completed_at
                .saturating_duration_since(submitted_at)
                .as_micros()
                .min(u128::from(u64::MAX)) as u64
        })
    }

    pub(super) fn validate_packets(&self, acceptance: PacketAcceptance) -> Result<CompletionStats, String> {
        let transfer = unsafe { self.transfer.as_ref() };
        validate_iso_packet_results(transfer, &self.packet_sizes, acceptance).map_err(|error| {
            format!(
                "{}; slot={} generation={} submitted_endpoint=0x{:02x} \
                 completed_endpoint=0x{:02x} submitted_bytes={} transfer_length={}",
                error,
                self.slot_index,
                self.generation,
                self.submitted_endpoint,
                transfer.endpoint,
                self.submitted_bytes,
                transfer.length,
            )
        })
    }

    /// What a timed-out URB cost, from its packet descriptors.
    pub(super) fn account_timeout(&self, acceptance: PacketAcceptance) -> CompletionStats {
        let transfer = unsafe { self.transfer.as_ref() };
        timed_out_packet_results(transfer, &self.packet_sizes, acceptance)
    }

    pub(super) fn mark_completed(&mut self) {
        self.queued_frame_count = 0;
        self.queued_underrun = false;
        self.queued_held = false;
        self.queued_music_frames = 0;
        self.submitted_at = None;
        self.in_flight = false;
        self.cancelled_for_flush = false;
    }

    pub(super) fn cancel(&mut self) {
        if !self.in_flight {
            return;
        }

        let _ = unsafe { libusb_cancel_transfer(self.transfer.as_ptr()) };
    }

    /// Cancels the transfer because the stream was flushed. The buffer stays
    /// owned by the slot until libusb hands the transfer back (see
    /// [`completion_ready`](Self::completion_ready)). A transfer that already
    /// finished cannot be cancelled and comes back with its own status.
    pub(super) fn cancel_for_flush(&mut self) {
        if !self.in_flight {
            return;
        }
        self.cancelled_for_flush = true;
        self.cancel();
    }

    /// libusb has handed this transfer back: its result is waiting to be taken.
    pub(super) fn completion_ready(&self) -> bool {
        unsafe { &(*self.user_data).completion }.is_published()
    }
}

impl Drop for IsoTransferSlot {
    fn drop(&mut self) {
        if self.in_flight {
            // Also protects unwinding: the handle/FD owner outlives the slots.
            self.cancel();
            if let Some(context) = self.retirement_context.clone() {
                let mut warned = false;
                while self.in_flight {
                    if self.take_completion().is_some() { self.mark_completed(); break; }
                    if let Err(error) = context.handle_events(Some(Duration::from_millis(10))) {
                        if !warned { log_warn!("USB unwind cancellation retirement pending: {}", error); warned = true; }
                        thread::sleep(Duration::from_millis(10));
                    }
                }
            }
        }

        unsafe {
            libusb_free_transfer(self.transfer.as_ptr());
            drop(Box::from_raw(self.user_data));
        }
    }
}

/// Built-in rules are bound to the opened descriptor and selected explicit pipe.
#[derive(Default, Debug)]
pub(super) struct FeedbackCompatibility {
    tenor_nominal_q16: Option<u32>,
    startup_packets_remaining: u8,
}

impl FeedbackCompatibility {
    pub(super) fn for_device(
        vendor: u16,
        product: u16,
        speed: Speed,
        endpoint: FeedbackEndpoint,
        sample_rate: u32,
    ) -> Self {
        let explicit_isoc_in = endpoint.transfer_type == TransferType::Isochronous
            && endpoint.address & 0x80 != 0;
        let tenor = explicit_isoc_in
            && endpoint.max_packet_bytes == 4
            && speed == Speed::High
            && matches!((vendor, product), (0x0644, 0x8038) | (0x1852, 0x5034));
        // ALSA rounds samples per 125-us microframe to the nearest Q16.16 value.
        let nominal_q16 = (u64::from(sample_rate) * 1024 + 62) / 125;
        Self {
            tenor_nominal_q16: tenor.then_some(nominal_q16).and_then(|nominal| u32::try_from(nominal).ok()),
            startup_packets_remaining: if explicit_isoc_in && vendor == 0x23ba { 4 } else { 0 },
        }
    }

    fn skip_packet(&mut self) -> bool {
        let skip = self.startup_packets_remaining != 0;
        self.startup_packets_remaining = self.startup_packets_remaining.saturating_sub(1);
        skip
    }
}

// Linux endpoint.c Tenor 8802 rule: correct only values strictly outside the
// nominal half-sample window, before applying this transport's tighter guards.
fn correct_tenor_feedback(raw: u32, nominal: u32) -> Option<u32> {
    if raw == 0 || raw & 0xf000_0000 != 0 {
        return None;
    }
    let lower = nominal.checked_sub(0x8000)?;
    let upper = nominal.checked_add(0x8000)?;
    if raw < lower {
        raw.checked_add(0xf000)
    } else if raw > upper {
        raw.checked_sub(0xf000)
    } else {
        Some(raw)
    }
}

/// Persistent asynchronous feedback IN slot. The output event loop re-arms
/// the transfer after each completion, without blocking the OUT queue.
pub(super) struct FeedbackTransferSlot {
    pub(super) transfer: std::ptr::NonNull<libusb_transfer>,
    pub(super) user_data: *mut IsoTransferUserData,
    pub(super) endpoint: FeedbackEndpoint,
    pub(super) packet_count: usize,
    pub(super) packet_bytes: usize,
    pub(super) timeout_ms: u32,
    pub(super) in_flight: bool,
    retirement_context: Option<Context>,
    pub(super) submit_no_device: bool,
    retry_at: Instant,
    compatibility: FeedbackCompatibility,
}

impl FeedbackTransferSlot {
    pub(super) fn new(
        endpoint: FeedbackEndpoint,
        bus_queue_ms: u32,
        compatibility: FeedbackCompatibility,
    ) -> Result<Self, String> {
        let packet_bytes = endpoint.max_packet_bytes.max(4);
        let packet_count = if endpoint.transfer_type == TransferType::Isochronous {
            let window_packets = (USB_FEEDBACK_POLL_WINDOW_MS.min(bus_queue_ms.max(1)) * 1_000)
                / endpoint.service_interval_us.max(125);
            window_packets.clamp(1, 32) as usize
        } else {
            1
        };

        let poll_window_ms = ((packet_count as u64)
            .saturating_mul(endpoint.service_interval_us as u64)
            / 1_000)
            .max(1) as u32;
        let timeout_ms =
            USB_FEEDBACK_TIMEOUT_MS.max(poll_window_ms.saturating_add(20));

        let completion = Arc::new(IsoTransferCompletion::new());
        let user_data = Box::new(IsoTransferUserData {
            completion,
            buffer: vec![0u8; packet_bytes.saturating_mul(packet_count).max(4)],
        });
        let user_data_ptr = Box::into_raw(user_data);
        let transfer = unsafe { libusb_alloc_transfer(packet_count as i32) };

        let Some(transfer) = std::ptr::NonNull::new(transfer) else {
            unsafe {
                drop(Box::from_raw(user_data_ptr));
            }
            return Err("libusb_alloc_transfer returned null for feedback transfer".to_string());
        };

        Ok(Self {
            transfer,
            user_data: user_data_ptr,
            endpoint,
            packet_count,
            packet_bytes,
            timeout_ms,
            in_flight: false,
            retirement_context: None,
            submit_no_device: false,
            retry_at: Instant::now(),
            compatibility,
        })
    }

    pub(super) fn submit(&mut self, handle: &DeviceHandle<Context>) -> Result<(), String> {
        if self.in_flight {
            return Err("feedback transfer slot already in flight".to_string());
        }

        self.retirement_context = Some(handle.context().clone());
        unsafe {
            let user_data = &mut *self.user_data;
            user_data.completion.reset();
            let buffer_len = self.packet_bytes.saturating_mul(self.packet_count);
            match self.endpoint.transfer_type {
                TransferType::Isochronous => {
                    libusb_fill_iso_transfer(
                        self.transfer.as_mut(),
                        handle.as_raw(),
                        self.endpoint.address,
                        user_data.buffer.as_mut_ptr(),
                        buffer_len as i32,
                        self.packet_count as i32,
                        iso_transfer_callback,
                        self.user_data as *mut c_void,
                        self.timeout_ms,
                    );
                    for index in 0..self.packet_count {
                        (*self
                            .transfer
                            .as_mut()
                            .iso_packet_desc
                            .as_mut_ptr()
                            .add(index))
                        .length = self.packet_bytes as u32;
                    }
                }
                TransferType::Interrupt => {
                    libusb_fill_interrupt_transfer(
                        self.transfer.as_mut(),
                        handle.as_raw(),
                        self.endpoint.address,
                        user_data.buffer.as_mut_ptr(),
                        self.packet_bytes as i32,
                        iso_transfer_callback,
                        self.user_data as *mut c_void,
                        self.timeout_ms,
                    );
                }
                other => {
                    return Err(format!(
                        "Unsupported feedback transfer type {}",
                        transfer_type_label(other)
                    ));
                }
            }

            let submit_result = libusb_submit_transfer(self.transfer.as_ptr());
            self.submit_no_device = submit_result == libusb1_sys::constants::LIBUSB_ERROR_NO_DEVICE;
            if submit_result != 0 {
                return Err(format!(
                    "libusb_submit_transfer failed for feedback endpoint 0x{:02x} with code {}",
                    self.endpoint.address, submit_result
                ));
            }
        }

        self.in_flight = true;
        Ok(())
    }

    pub(super) fn take_completion(&self) -> Option<(i32, Instant, usize)> {
        let completion = unsafe { &(*self.user_data).completion };
        completion.take()
    }

    fn newest_valid_report(&mut self, scheduler: &IsoPacketScheduler, service_us: u32,
        speed: Speed, max_frames: usize, per_service: bool, counters: &mut FeedbackCounters,
        actual_length: usize) -> Option<(f64, f64)> {
        let buffer = unsafe { &(*self.user_data).buffer };
        if self.endpoint.transfer_type == TransferType::Isochronous {
            let transfer = unsafe { self.transfer.as_ref() };
            if transfer.num_iso_packets < 0 || transfer.num_iso_packets as usize != self.packet_count {
                counters.errors = counters.errors.saturating_add(1);
                return None;
            }
            select_feedback_report_with_compatibility(buffer, (0..self.packet_count).map(|index| {
                let packet = unsafe { &*transfer.iso_packet_desc.as_ptr().add(index) };
                (packet.status, packet.length as usize, packet.actual_length as usize)
            }), scheduler, service_us, speed, max_frames, per_service, counters, &mut self.compatibility)
        } else {
            select_feedback_report_with_compatibility(
                buffer, [(LIBUSB_TRANSFER_COMPLETED, self.packet_bytes, actual_length)],
                scheduler, service_us, speed, max_frames, per_service, counters, &mut self.compatibility,
            )
        }
    }

    pub(super) fn mark_completed(&mut self) {
        self.in_flight = false;
    }

    pub(super) fn cancel(&mut self) {
        if self.in_flight {
            let _ = unsafe { libusb_cancel_transfer(self.transfer.as_ptr()) };
        }
    }
}

impl Drop for FeedbackTransferSlot {
    fn drop(&mut self) {
        if self.in_flight {
            self.cancel();
            if let Some(context) = self.retirement_context.clone() {
                let mut warned = false;
                while self.in_flight {
                    if self.take_completion().is_some() { self.mark_completed(); break; }
                    if let Err(error) = context.handle_events(Some(Duration::from_millis(10))) {
                        if !warned { log_warn!("USB unwind cancellation retirement pending: {}", error); warned = true; }
                        thread::sleep(Duration::from_millis(10));
                    }
                }
            }
        }

        unsafe {
            libusb_free_transfer(self.transfer.as_ptr());
            drop(Box::from_raw(self.user_data));
        }
    }
}

#[derive(Default)]
pub(super) struct FeedbackCounters {
    pub(super) reports: u64,
    pub(super) timeouts: u64,
    pub(super) errors: u64,
    pub(super) rejected: u64,
    pub(super) last_report_at: Option<Instant>,
    pub(super) last_frames_per_packet: Option<f64>,
    pub(super) last_estimated_sample_rate: Option<f64>,
    pub(super) empty_polls: u64,
    malformed: u64,
    rate_rejected: u64,
    last_report_bytes: [u8; 4],
    last_report_len: usize,
}

#[cfg(test)]
fn select_feedback_report(
    buffer: &[u8], packets: impl IntoIterator<Item = (i32, usize, usize)>,
    scheduler: &IsoPacketScheduler, service_us: u32, speed: Speed,
    max_frames: usize, per_service: bool, counters: &mut FeedbackCounters,
) -> Option<(f64, f64)> {
    select_feedback_report_with_compatibility(buffer, packets, scheduler, service_us,
        speed, max_frames, per_service, counters, &mut FeedbackCompatibility::default())
}

/// Scan the requested packet spans and update the EWMA only once per transfer.
fn select_feedback_report_with_compatibility(
    buffer: &[u8], packets: impl IntoIterator<Item = (i32, usize, usize)>,
    scheduler: &IsoPacketScheduler, service_us: u32, speed: Speed,
    max_frames: usize, per_service: bool, counters: &mut FeedbackCounters,
    compatibility: &mut FeedbackCompatibility,
) -> Option<(f64, f64)> {
    let mut offset = 0usize;
    let mut newest = None;
    for (status, requested, actual) in packets {
        let start = offset;
        offset = offset.saturating_add(requested);
        // A completed URB contributes each bus opportunity, including failed or empty packets.
        let skip = compatibility.skip_packet();
        if status != LIBUSB_TRANSFER_COMPLETED {
            counters.errors = counters.errors.saturating_add(1);
            continue;
        }
        if actual == 0 {
            counters.empty_polls = counters.empty_polls.saturating_add(1);
            continue;
        }
        let report = (actual <= requested && offset <= buffer.len())
            .then(|| buffer.get(start..start.saturating_add(actual))).flatten();
        let decoded = report.and_then(|bytes| {
            if let (false, Speed::High, Some(nominal), Ok(raw_bytes)) =
                (per_service, speed, compatibility.tenor_nominal_q16, bytes.try_into())
            {
                let corrected = correct_tenor_feedback(u32::from_le_bytes(raw_bytes), nominal)?;
                return decode_feedback_report_with_scale(&corrected.to_le_bytes(), service_us, speed, false);
            }
            decode_feedback_report_with_scale(bytes, service_us, speed, per_service)
        });
        // Discard startup rates before selection: these cannot acquire or renew lock.
        // Malformed packets still contribute the normal diagnostics.
        if skip && decoded.is_some() { continue; }
        if let Some((frames, rate)) = decoded.filter(|(frames, _)|
            scheduler.feedback_rate_is_plausible(*frames) && frames.ceil() <= max_frames as f64) {
            newest = Some((frames, rate));
            let bytes = report.unwrap();
            counters.last_report_bytes = [0; 4];
            counters.last_report_bytes[..bytes.len()].copy_from_slice(bytes);
            counters.last_report_len = bytes.len();
        } else {
            if decoded.is_none() { counters.malformed = counters.malformed.saturating_add(1); }
            else { counters.rate_rejected = counters.rate_rejected.saturating_add(1); }
            counters.rejected = counters.rejected.saturating_add(1);
        }
    }
    newest
}

#[derive(Debug)]
pub(super) enum FeedbackFault { DeviceLost, Transfer(String) }

/// A selected explicit pipe cannot silently become nominal on first-submit failure.
/// Runtime transient submissions may retry only while the health deadline remains live.
pub(super) fn feedback_submit_failure(error: String, no_device: bool, first: bool) -> Option<FeedbackFault> {
    if no_device { Some(FeedbackFault::DeviceLost) }
    else if first { Some(FeedbackFault::Transfer(format!("explicit feedback initial submit failed: {}", error))) }
    else { None }
}

fn feedback_completion_fault(status: i32, stopping: bool, endpoint: u8) -> Option<FeedbackFault> {
    match status {
        LIBUSB_TRANSFER_NO_DEVICE => Some(FeedbackFault::DeviceLost),
        LIBUSB_TRANSFER_CANCELLED if !stopping => Some(FeedbackFault::Transfer("explicit feedback unexpectedly cancelled".into())),
        LIBUSB_TRANSFER_STALL => Some(FeedbackFault::Transfer(format!("explicit feedback endpoint 0x{:02x} stalled", endpoint))),
        _ => None,
    }
}

pub(super) fn pump_feedback_transfer(
    handle: &DeviceHandle<Context>, slot: &mut FeedbackTransferSlot,
    scheduler: &mut IsoPacketScheduler, data_service_interval_us: u32, speed: Speed,
    max_frames: usize, per_service: bool, counters: &mut FeedbackCounters, stop: &AtomicBool,
    health: ExplicitFeedbackHealth, started_at: Instant,
) -> Result<(), FeedbackFault> {
    if let Some((status, completed_at, actual_length)) = slot.take_completion() {
        slot.mark_completed();
        if let Some(fault) = feedback_completion_fault(status, usb_stop_requested(stop), slot.endpoint.address) {
            return Err(fault);
        }
        match status {
            LIBUSB_TRANSFER_COMPLETED => {
                if let Some((frames, rate)) = slot.newest_valid_report(scheduler, data_service_interval_us,
                    speed, max_frames, per_service, counters, actual_length) {
                    // A report delivered beyond the old lease cannot restart it.
                    // Use callback time: a timely report processed slightly late is valid.
                    if !health.report_is_timely(completed_at, started_at, counters.last_report_at) {
                        return Err(FeedbackFault::Transfer("explicit feedback report arrived after its terminal deadline".into()));
                    }
                    if scheduler.update_feedback_frames_per_packet(frames) {
                        counters.reports = counters.reports.saturating_add(1);
                        counters.last_report_at = Some(completed_at);
                        counters.last_frames_per_packet = Some(frames);
                        counters.last_estimated_sample_rate = Some(rate);
                        if counters.reports == 1 || counters.reports % 128 == 0 {
                            dev_eprintln!("[USB] Explicit feedback endpoint=0x{:02x} bytes={:02x?} framesPerOutService={:.6} rateHz={:.3} perServiceQuirk={} empty={} malformed={} rateRejected={}",
                                slot.endpoint.address, &counters.last_report_bytes[..counters.last_report_len],
                                frames, rate, per_service, counters.empty_polls, counters.malformed, counters.rate_rejected);
                        }
                    }
                }
            }
            LIBUSB_TRANSFER_TIMED_OUT => { counters.timeouts = counters.timeouts.saturating_add(1); }
            LIBUSB_TRANSFER_CANCELLED if usb_stop_requested(stop) => return Ok(()),
            LIBUSB_TRANSFER_OVERFLOW => { counters.rejected = counters.rejected.saturating_add(1); }
            _ => { counters.errors = counters.errors.saturating_add(1); }
        }
        // Retry transient completions with a small bounded delay, not a busy resubmit loop.
        if status != LIBUSB_TRANSFER_COMPLETED {
            slot.retry_at = Instant::now() + Duration::from_millis(5);
        }
    }
    if !usb_stop_requested(stop) && !slot.in_flight && Instant::now() >= slot.retry_at {
        if let Err(error) = slot.submit(handle) {
            if let Some(fault) = feedback_submit_failure(error.clone(), slot.submit_no_device, false) { return Err(fault); }
            counters.errors = counters.errors.saturating_add(1);
            slot.retry_at = Instant::now() + Duration::from_millis(5);
            // The health deadline bounds retries; never fall back to nominal timing.
            if counters.errors <= 4 || counters.errors % 64 == 0 {
                log_warn!("explicit feedback resubmit failed: {}", error);
            }
        }
    }
    Ok(())
}

/// Deadlines are measured from the first active transfer or the last actual rate.
/// Empty successful polls and transport retries do not refresh that epoch.
#[derive(Debug, Clone, Copy)]
pub(super) struct ExplicitFeedbackHealth { pub(super) warning: Duration, pub(super) terminal: Duration }
impl ExplicitFeedbackHealth {
    pub(super) fn new(poll_us: u32, refresh: Option<u8>) -> Self {
        // UAC1 bRefresh is log2 milliseconds, valid in the range 1..=9.
        let update_us = refresh.filter(|r| (1..=9).contains(r))
            .map(|r| (1u64 << r) * 1000).unwrap_or(u64::from(poll_us));
        let cadence = Duration::from_micros(update_us.max(u64::from(poll_us)));
        Self { warning: Duration::from_millis(USB_FEEDBACK_STALE_WARN_MS).max(cadence * 3),
            terminal: Duration::from_secs(1).max(cadence * 6) }
    }
    fn report_is_timely(&self, completed: Instant, started: Instant, previous: Option<Instant>) -> bool {
        completed.saturating_duration_since(previous.unwrap_or(started)) < self.terminal
    }
    pub(super) fn state(&self, now: Instant, started: Instant, last: Option<Instant>) -> (bool, bool, bool) {
        let age = now.saturating_duration_since(last.unwrap_or(started));
        (last.is_some() && age < self.warning, age >= self.warning, age >= self.terminal)
    }
}

/// Decode UAC feedback into frames per data-endpoint service interval.
///
/// UAC2 feedback reports samples per microframe on high-speed links and
/// samples per frame on full-speed links. The feedback endpoint's own polling
/// interval is not the data endpoint's packet interval; using it as the
/// timebase previously produced a rate error of up to 8x at 125 us service.
#[cfg(test)]
pub(super) fn decode_feedback_report(
    raw_bytes: &[u8],
    data_service_interval_us: u32,
    speed: Speed,
) -> Option<(f64, f64)> {
    decode_feedback_report_with_scale(raw_bytes, data_service_interval_us, speed, false)
}

fn decode_feedback_report_with_scale(raw_bytes: &[u8], data_service_interval_us: u32,
    speed: Speed, per_service: bool) -> Option<(f64, f64)> {
    if data_service_interval_us == 0 { return None; }
    let high_speed = matches!(speed, Speed::High | Speed::Super | Speed::SuperPlus);
    if !matches!(speed, Speed::Full | Speed::High | Speed::Super | Speed::SuperPlus) { return None; }
    let per_service = per_service && speed == Speed::High && raw_bytes.len() == 4;
    let (frames_per_base_interval, base_interval_us) = match raw_bytes.len() {
        3 => {
            // Existing HS Q10.14 compatibility: only this exact length/scale;
            // the intake nominal window still has to identify a plausible rate.
            let raw = u32::from(raw_bytes[0])
                | (u32::from(raw_bytes[1]) << 8)
                | (u32::from(raw_bytes[2]) << 16);
            let base_interval_us = feedback_base_interval_us(speed, data_service_interval_us);
            (raw as f64 / 16_384.0, base_interval_us)
        }
        4 => {
            let raw = u32::from_le_bytes([raw_bytes[0], raw_bytes[1], raw_bytes[2], raw_bytes[3]]);
            if high_speed && raw & 0xf000_0000 != 0 { return None; }
            let base_interval_us = if per_service { f64::from(data_service_interval_us) } else { feedback_base_interval_us(speed, data_service_interval_us) };
            (raw as f64 / 65_536.0, base_interval_us)
        }
        _ => return None,
    };

    let packet_multiplier = data_service_interval_us as f64 / base_interval_us;
    let frames_per_packet = frames_per_base_interval * packet_multiplier;
    let estimated_sample_rate = frames_per_base_interval * (1_000_000.0 / base_interval_us);
    Some((frames_per_packet, estimated_sample_rate))
}

pub(super) fn feedback_base_interval_us(speed: Speed, data_service_interval_us: u32) -> f64 {
    match speed {
        Speed::Low | Speed::Full => 1_000.0,
        Speed::High | Speed::Super | Speed::SuperPlus => 125.0,
        Speed::Unknown => {
            // Isochronous audio at sub-millisecond service intervals can only
            // exist on a high-speed link. Fall back to the data endpoint when
            // libusb could not identify the bus speed.
            if data_service_interval_us < 1_000 {
                125.0
            } else {
                1_000.0
            }
        }
        _ => {
            if data_service_interval_us < 1_000 {
                125.0
            } else {
                1_000.0
            }
        }
    }
}

pub(super) extern "system" fn iso_transfer_callback(transfer: *mut libusb_transfer) {
    // Panicking across the FFI boundary aborts the process, so contain every
    // panic here; a poisoned lock must not kill the app either.
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let Some(transfer) = std::ptr::NonNull::new(transfer) else {
            return;
        };
        let user_data_ptr = unsafe { transfer.as_ref().user_data as *mut IsoTransferUserData };
        let Some(user_data) = std::ptr::NonNull::new(user_data_ptr) else {
            return;
        };

        let completed_at = Instant::now();
        let status = unsafe { transfer.as_ref().status };
        let actual_length = unsafe { transfer.as_ref().actual_length } as usize;
        // Keep the completion state alive independently of the transfer/user
        // data so the output thread can re-arm or free the slot as soon as the
        // status is published.
        let completion = unsafe { user_data.as_ref() }.completion.clone();
        completion
            .actual_length
            .store(actual_length, Ordering::Release);
        match completion.completed_at.lock() {
            Ok(mut guard) => *guard = Some(completed_at),
            Err(poisoned) => *poisoned.into_inner() = Some(completed_at),
        }
        // Publish status last. Everything above is mine; after this store the
        // slot owner may refill or release the transfer without racing this
        // callback, because the callback no longer touches either object.
        let mut guard = match completion.status.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        *guard = Some(status);
    }));
}

pub(super) fn validate_iso_packet_results(
    transfer: &libusb_transfer,
    packet_sizes: &[usize],
    acceptance: PacketAcceptance,
) -> Result<CompletionStats, String> {
    if transfer.num_iso_packets != packet_sizes.len() as i32 {
        return Err(format!(
            "isochronous transfer completed with packet errors: \
             packet count changed: requested={}, transfer={}",
            packet_sizes.len(),
            transfer.num_iso_packets,
        ));
    }

    // Both callers allocated exactly packet_sizes.len() descriptors and call
    // this only after completion, before resubmitting or freeing the transfer.
    let descriptors = unsafe {
        std::slice::from_raw_parts(transfer.iso_packet_desc.as_ptr(), packet_sizes.len())
    };
    validate_iso_packet_descriptors(packet_sizes, descriptors, acceptance)
}

pub(super) fn timed_out_packet_results(
    transfer: &libusb_transfer,
    packet_sizes: &[usize],
    acceptance: PacketAcceptance,
) -> CompletionStats {
    if transfer.num_iso_packets != packet_sizes.len() as i32 {
        // Not the schedule that was submitted: trust nothing, lose the URB.
        return account_timed_out_transfer(packet_sizes, &[], acceptance);
    }

    // Same contract as validate_iso_packet_results: the slot allocated exactly
    // packet_sizes.len() descriptors and this runs after completion, before the
    // slot is resubmitted.
    let descriptors = unsafe {
        std::slice::from_raw_parts(transfer.iso_packet_desc.as_ptr(), packet_sizes.len())
    };
    account_timed_out_transfer(packet_sizes, descriptors, acceptance)
}


#[cfg(test)]
mod explicit_feedback_tests {
    use super::*;

    fn q16(frames: f64) -> [u8; 4] { ((frames * 65536.0).round() as u32).to_le_bytes() }
    fn scheduler(service_us: u32) -> IsoPacketScheduler { IsoPacketScheduler::new(48_000, 8, service_us) }

    fn feedback_pipe() -> FeedbackEndpoint {
        FeedbackEndpoint { refresh: 0, address: 0x81, transfer_type: TransferType::Isochronous,
            interval: 1, service_interval_us: 125, max_packet_bytes: 4 }
    }

    fn compatible_report(report: &[u8], service_us: u32, max_frames: usize, per_service: bool,
        compatibility: &mut FeedbackCompatibility) -> Option<(f64, f64)> {
        select_feedback_report_with_compatibility(report, [(0, report.len(), report.len())],
            &scheduler(service_us), service_us, Speed::High, max_frames, per_service,
            &mut FeedbackCounters::default(), compatibility)
    }

    #[test]
    fn tenor_correction_is_exact_identity_pipe_and_bus_scoped() {
        let pipe = feedback_pipe();
        for (vendor, product) in [(0x0644, 0x8038), (0x1852, 0x5034)] {
            let mut compatibility = FeedbackCompatibility::for_device(vendor, product, Speed::High, pipe, 48_000);
            for raw in [0x60000 - 0xf000, 0x60000, 0x60000 + 0xf000] {
                assert_eq!(compatible_report(&u32::to_le_bytes(raw), 125, 7, false, &mut compatibility),
                    Some((6.0, 48_000.0)));
            }
        }
        for (vendor, product) in [(0x0644, 0x8037), (0x0644, 0x8039), (0x1852, 0x5035), (0x1853, 0x5034)] {
            let mut compatibility = FeedbackCompatibility::for_device(vendor, product, Speed::High, pipe, 48_000);
            assert!(compatibility.tenor_nominal_q16.is_none());
            assert!(compatible_report(&q16(6.9375), 125, 7, false, &mut compatibility).is_none());
        }
        for speed in [Speed::Full, Speed::Super, Speed::SuperPlus, Speed::Unknown] {
            assert!(FeedbackCompatibility::for_device(0x0644, 0x8038, speed, pipe, 48_000)
                .tenor_nominal_q16.is_none());
        }
        for disabled_pipe in [FeedbackEndpoint { max_packet_bytes: 3, ..pipe },
            FeedbackEndpoint { max_packet_bytes: 8, ..pipe },
            FeedbackEndpoint { address: 1, ..pipe },
            FeedbackEndpoint { transfer_type: TransferType::Interrupt, ..pipe }] {
            let compatibility = FeedbackCompatibility::for_device(0x0644, 0x8038, Speed::High, disabled_pipe, 48_000);
            assert!(compatibility.tenor_nominal_q16.is_none());
        }
    }

    #[test]
    fn tenor_uses_checked_strict_bounds_and_never_bypasses_admission() {
        assert_eq!(correct_tenor_feedback(0, 0x60000), None);
        assert_eq!(correct_tenor_feedback(0x58000, 0x60000), Some(0x58000));
        assert_eq!(correct_tenor_feedback(0x68000, 0x60000), Some(0x68000));
        assert_eq!(correct_tenor_feedback(0x57fff, 0x60000), Some(0x66fff));
        assert_eq!(correct_tenor_feedback(0x68001, 0x60000), Some(0x59001));
        assert_eq!(correct_tenor_feedback(1, 0x7fff), None); // nominal bound underflow
        assert_eq!(correct_tenor_feedback(u32::MAX, u32::MAX), None); // nominal bound overflow
        let mut compatibility = FeedbackCompatibility::for_device(0x0644, 0x8038,
            Speed::High, feedback_pipe(), 48_000);
        for raw in [0, 1, 0xf0060000, 0x80000] {
            assert!(compatible_report(&u32::to_le_bytes(raw), 125, 7, false, &mut compatibility).is_none());
        }
        // Corrected values still need fractional-ceiling capacity and nominal-rate checks.
        let report = q16(6.01 + 0.9375);
        assert!(compatible_report(&report, 1000, 48, false, &mut compatibility).is_none());
        assert!(compatible_report(&report, 1000, 49, false, &mut compatibility).is_some());
        assert!(compatible_report(&q16(7.2), 125, 8, false, &mut compatibility).is_none());
        // The explicit user scale wins and is never followed by the Tenor subtraction.
        assert_eq!(compatible_report(&q16(48.0), 1000, 49, true, &mut compatibility), Some((48.0, 48_000.0)));
        // A three-byte report keeps the existing Q10.14 interpretation.
        assert_eq!(compatible_report(&[0, 0x80, 1], 125, 7, false, &mut compatibility), Some((6.0, 48_000.0)));
    }

    #[test]
    fn playback_designs_skips_four_completed_bus_opportunities_across_batches() {
        let mut compatibility = FeedbackCompatibility::for_device(0x23ba, 0xffff,
            Speed::High, feedback_pipe(), 48_000);
        let mut counters = FeedbackCounters::default();
        let prior_lock = Instant::now();
        counters.last_report_at = Some(prior_lock);
        let report = q16(6.0);
        assert!(select_feedback_report_with_compatibility(&report.repeat(2), [(0, 4, 4); 2],
            &scheduler(125), 125, Speed::High, 7, false, &mut counters, &mut compatibility).is_none());
        // A transfer-level failure never invokes packet selection (and consumes no skip).
        assert!(feedback_completion_fault(LIBUSB_TRANSFER_TIMED_OUT, false, 0x81).is_none());
        assert_eq!(compatibility.startup_packets_remaining, 2);
        assert!(select_feedback_report_with_compatibility(&report.repeat(2),
            [(LIBUSB_TRANSFER_ERROR, 4, 4), (0, 4, 0)], &scheduler(125), 125, Speed::High,
            7, false, &mut counters, &mut compatibility).is_none());
        assert_eq!(counters.errors, 1);
        assert_eq!(counters.empty_polls, 1);
        assert_eq!(counters.last_report_at, Some(prior_lock));
        assert_eq!(counters.reports, 0);
        let mut bytes = report.to_vec();
        bytes.extend_from_slice(&q16(6.01));
        bytes.extend_from_slice(&q16(48.0)); // newest invalid report cannot replace the good one
        let selected = select_feedback_report_with_compatibility(&bytes, [(0, 4, 4); 3],
            &scheduler(125), 125, Speed::High, 7, false, &mut counters, &mut compatibility).unwrap();
        assert!((selected.0 - 6.01).abs() < 1.0 / 65536.0);
        assert_eq!(counters.rejected, 1);
        let fresh = FeedbackCompatibility::for_device(0x23ba, 1, Speed::Full, feedback_pipe(), 48_000);
        assert_eq!(fresh.startup_packets_remaining, 4);
    }

    #[test]
    fn playback_startup_keeps_malformed_safety_and_non_iso_semantics() {
        let mut compatibility = FeedbackCompatibility::for_device(0x23ba, 1,
            Speed::High, feedback_pipe(), 48_000);
        let mut counters = FeedbackCounters::default();
        assert!(select_feedback_report_with_compatibility(&[0; 16],
            [(0, 4, 5), (0, 4, 2), (0, 4, 0), (LIBUSB_TRANSFER_ERROR, 4, 4)],
            &scheduler(125), 125, Speed::High, 7, false, &mut counters, &mut compatibility).is_none());
        assert_eq!(counters.malformed, 2);
        assert_eq!(counters.rejected, 2);
        assert_eq!(counters.empty_polls, 1);
        assert_eq!(counters.errors, 1);
        assert_eq!(compatibility.startup_packets_remaining, 0);
        for (vendor, pipe) in [(0x23bb, feedback_pipe()), (0x23ba,
            FeedbackEndpoint { transfer_type: TransferType::Interrupt, ..feedback_pipe() })] {
            let mut compatibility = FeedbackCompatibility::for_device(vendor, 1, Speed::High, pipe, 48_000);
            assert_eq!(compatible_report(&q16(6.0), 125, 7, false, &mut compatibility), Some((6.0, 48_000.0)));
        }
    }

    #[test]
    fn newest_fully_valid_poll_wins_at_requested_offsets() {
        let first = q16(6.0);
        let later = q16(6.005);
        // Requested spans deliberately vary; actual bytes are never concatenated.
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&first);
        bytes.extend_from_slice(&[0; 6]); // oversized successful poll
        bytes.extend_from_slice(&later); // failed descriptor cannot replace first
        bytes.extend_from_slice(&[0; 4]); // successful empty poll
        bytes.extend_from_slice(&[1, 2]); // malformed trailing poll
        let mut counters = FeedbackCounters::default();
        let packets = [(0, 4, 4), (0, 6, 6), (LIBUSB_TRANSFER_ERROR, 4, 4), (0, 4, 0), (0, 2, 2)];
        assert_eq!(select_feedback_report(&bytes, packets, &scheduler(125), 125, Speed::High,
            7, false, &mut counters), Some((6.0, 48_000.0)));
        assert_eq!(counters.empty_polls, 1);
        assert_eq!(counters.errors, 1);
        assert_eq!(counters.malformed, 2);
        assert_eq!(counters.reports, 0); // scanning never commits the EWMA
        bytes.extend_from_slice(&later);
        let packets = [(0, 4, 4), (0, 6, 6), (LIBUSB_TRANSFER_ERROR, 4, 4), (0, 4, 0), (0, 2, 2), (0, 4, 4)];
        let chosen = select_feedback_report(&bytes, packets, &scheduler(125), 125, Speed::High,
            7, false, &mut counters).unwrap();
        assert!((chosen.0 - 6.005).abs() < 1.0 / 65536.0);
    }

    #[test]
    fn rejects_oversized_truncated_reserved_and_implausible_reports_without_masking_good() {
        let good = q16(6.0);
        let bad_rate = q16(48.0);
        let reserved = [0, 0, 6, 0xf0];
        let mut bytes = good.to_vec();
        bytes.extend_from_slice(&bad_rate);
        bytes.extend_from_slice(&reserved);
        let mut counters = FeedbackCounters::default();
        assert_eq!(select_feedback_report(&bytes, [(0, 4, 4), (0, 4, 4), (0, 4, 4), (0, 4, 5)],
            &scheduler(125), 125, Speed::High, 7, false, &mut counters), Some((6.0, 48_000.0)));
        assert_eq!(counters.rejected, 3);
        assert_eq!(counters.rate_rejected, 1);
        assert_eq!(counters.malformed, 2);
        assert!(decode_feedback_report(&[0, 0, 6, 0, 0], 125, Speed::High).is_none());
        assert!(decode_feedback_report(&good, 1000, Speed::Unknown).is_none());
    }

    #[test]
    fn capacity_checks_fractional_ceiling_after_out_service_scaling() {
        let mut counters = FeedbackCounters::default();
        let report = q16(48.05 / 8.0);
        assert!(select_feedback_report(&report, [(0, 4, 4)], &scheduler(1000), 1000,
            Speed::High, 48, false, &mut counters).is_none());
        let selected = select_feedback_report(&report, [(0, 4, 4)], &scheduler(1000), 1000,
            Speed::High, 49, false, &mut counters).unwrap();
        assert_eq!(selected.0.ceil(), 49.0);
        // Feedback polling interval is deliberately absent from the rate calculation.
        let mut stream = scheduler(1000);
        assert!(stream.update_feedback_frames_per_packet(selected.0));
        let mut total = 0usize;
        let mut packets = 0usize;
        for _ in 0..100 {
            let packet_bytes = stream.next_transfer_packet_bytes();
            packets += packet_bytes.len();
            total += packet_bytes.into_iter().map(|n| n / 8).sum::<usize>();
        }
        assert!((total as f64 - selected.0 * packets as f64).abs() < 1.0);
    }

    #[test]
    fn linux_uac2_gadget_per_service_vector_requires_opt_in() {
        // Linux v7.0 u_audio.c u_audio_set_fback_frequency: a 1 ms HS OUT
        // service can report 48 Q16.16 frames per OUT packet rather than 6
        // per USB microframe. Enable only after the device trace proves units.
        let report = q16(48.0);
        let mut counters = FeedbackCounters::default();
        assert!(select_feedback_report(&report, [(0, 4, 4)], &scheduler(1000), 1000,
            Speed::High, 49, false, &mut counters).is_none());
        assert_eq!(select_feedback_report(&report, [(0, 4, 4)], &scheduler(1000), 1000,
            Speed::High, 49, true, &mut counters), Some((48.0, 48_000.0)));
        assert_eq!(decode_feedback_report_with_scale(&report, 1000, Speed::Super, true),
            decode_feedback_report(&report, 1000, Speed::Super));
        assert_eq!(decode_feedback_report_with_scale(&[0, 0, 12], 1000, Speed::High, true),
            decode_feedback_report(&[0, 0, 12], 1000, Speed::High));
        assert_eq!(decode_feedback_report_with_scale(&[0, 0, 12], 1000, Speed::Full, true),
            Some((48.0, 48_000.0)));
    }

    #[test]
    fn nominal_warmup_stale_recovery_and_terminal_deadline_follow_last_actual_report() {
        let health = ExplicitFeedbackHealth::new(1000, None);
        let start = Instant::now();
        assert_eq!(health.state(start, start, None), (false, false, false));
        assert_eq!(health.state(start + Duration::from_millis(500), start, None), (false, true, false));
        assert_eq!(health.state(start + Duration::from_secs(1), start, None), (false, true, true));
        let good = start + Duration::from_millis(100);
        assert_eq!(health.state(good, start, Some(good)), (true, false, false));
        assert_eq!(health.state(start + Duration::from_millis(600), start, Some(good)), (false, true, false));
        let recovered = start + Duration::from_millis(900);
        assert_eq!(health.state(recovered, start, Some(recovered)), (true, false, false));
        // Empty success produces no report and cannot refresh the clock or last rate.
        let mut counters = FeedbackCounters { last_report_at: Some(good), ..Default::default() };
        assert!(select_feedback_report(&[0; 4], [(0, 4, 0)], &scheduler(125), 125,
            Speed::High, 7, false, &mut counters).is_none());
        assert_eq!(counters.last_report_at, Some(good));
        assert_eq!(health.state(start + Duration::from_millis(1100), start, counters.last_report_at), (false, true, true));
    }

    #[test]
    fn first_submit_failure_is_terminal_and_detach_is_distinct() {
        assert!(matches!(feedback_submit_failure("allocation/submit error".into(), false, true),
            Some(FeedbackFault::Transfer(_))));
        assert!(feedback_submit_failure("transient".into(), false, false).is_none());
        assert!(matches!(feedback_submit_failure("detached".into(), true, true), Some(FeedbackFault::DeviceLost)));
        assert!(matches!(feedback_submit_failure("detached".into(), true, false), Some(FeedbackFault::DeviceLost)));
        assert!(matches!(feedback_completion_fault(LIBUSB_TRANSFER_NO_DEVICE, false, 0x81), Some(FeedbackFault::DeviceLost)));
        assert!(feedback_completion_fault(LIBUSB_TRANSFER_CANCELLED, true, 0x81).is_none());
        assert!(matches!(feedback_completion_fault(LIBUSB_TRANSFER_CANCELLED, false, 0x81), Some(FeedbackFault::Transfer(_))));
        assert!(feedback_completion_fault(LIBUSB_TRANSFER_TIMED_OUT, false, 0x81).is_none());
        assert!(feedback_completion_fault(LIBUSB_TRANSFER_OVERFLOW, false, 0x81).is_none());
    }

    #[test]
    fn a_late_completion_cannot_renew_an_expired_feedback_lease() {
        let health = ExplicitFeedbackHealth::new(1000, None);
        let start = Instant::now();
        let at_deadline = start + health.terminal;
        let before_deadline = at_deadline - Duration::from_micros(1);
        assert!(health.report_is_timely(before_deadline, start, None));
        assert!(!health.report_is_timely(at_deadline, start, None));
        assert!(!health.report_is_timely(at_deadline + Duration::from_micros(1), start, None));
        // Processing after the old deadline is fine if the callback was timely:
        // its new age is measured from the true completion, not processing time.
        let processed = at_deadline + Duration::from_millis(5);
        assert_eq!(health.state(processed, start, Some(before_deadline)), (true, false, false));
        let prior_good = start + Duration::from_millis(200);
        assert!(health.report_is_timely(prior_good + health.terminal - Duration::from_micros(1), start, Some(prior_good)));
        assert!(!health.report_is_timely(prior_good + health.terminal, start, Some(prior_good)));
    }

    #[test]
    fn deadlines_honor_valid_uac1_refresh_and_sparse_polling() {
        let slow = ExplicitFeedbackHealth::new(1000, Some(9));
        assert_eq!(slow.warning, Duration::from_millis(1536));
        assert_eq!(slow.terminal, Duration::from_millis(3072));
        let invalid = ExplicitFeedbackHealth::new(1000, Some(10));
        assert_eq!(invalid.warning, Duration::from_millis(500));
        assert_eq!(ExplicitFeedbackHealth::new(1_024_000, None).terminal, Duration::from_millis(6144));
    }
}
