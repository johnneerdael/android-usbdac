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
    }

    pub(super) fn cancel(&mut self) {
        if !self.in_flight {
            return;
        }

        let _ = unsafe { libusb_cancel_transfer(self.transfer.as_ptr()) };
    }
}

impl Drop for IsoTransferSlot {
    fn drop(&mut self) {
        if self.in_flight {
            // libusb still owns the transfer and its buffer. Freeing either
            // here risks a use-after-free inside libusb's event thread, so
            // leak them instead of crashing.
            dev_eprintln!(
                "[USB] in-flight isochronous transfer survived teardown drain; leaking slot to avoid use-after-free"
            );
            std::mem::forget(unsafe { Box::from_raw(self.user_data) });
            return;
        }

        unsafe {
            libusb_free_transfer(self.transfer.as_ptr());
            drop(Box::from_raw(self.user_data));
        }
    }
}

/// Persistent asynchronous feedback IN slot.
///
/// The previous implementation performed a blocking feedback read from the
/// single output thread. The OUT transfers on the bus only cover about 16 ms
/// (`DacConfig::bus_queue_ms`), so a 50 ms feedback timeout could drain the
/// entire hardware queue even while the software ring looked full. This slot
/// is submitted once and re-armed from the same event loop after completion.
/// It polls over at most the bus queue (and 16 ms), so a report is never older
/// than what is queued for the DAC.
pub(super) struct FeedbackTransferSlot {
    pub(super) transfer: std::ptr::NonNull<libusb_transfer>,
    pub(super) user_data: *mut IsoTransferUserData,
    pub(super) endpoint: FeedbackEndpoint,
    pub(super) packet_count: usize,
    pub(super) packet_bytes: usize,
    pub(super) timeout_ms: u32,
    pub(super) in_flight: bool,
}

impl FeedbackTransferSlot {
    pub(super) fn new(endpoint: FeedbackEndpoint, bus_queue_ms: u32) -> Result<Self, String> {
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
        })
    }

    pub(super) fn submit(&mut self, handle: &DeviceHandle<Context>) -> Result<(), String> {
        if self.in_flight {
            return Err("feedback transfer slot already in flight".to_string());
        }

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

    pub(super) fn report_bytes(&self) -> Option<Vec<u8>> {
        let user_data = unsafe { &(*self.user_data) };
        let buffer = &user_data.buffer;

        // For iso feedback, packet payloads are laid out at fixed offsets based
        // on the requested packet length. Read the newest packet from its own
        // descriptor instead of assuming libusb packed a shortened actual-length
        // prefix contiguously.
        let (offset, available) = if self.endpoint.transfer_type == TransferType::Isochronous {
            let transfer = unsafe { self.transfer.as_ref() };
            (0..self.packet_count).rev().find_map(|index| {
                let descriptor = unsafe { &*transfer.iso_packet_desc.as_ptr().add(index) };
                (descriptor.status == LIBUSB_TRANSFER_COMPLETED && descriptor.actual_length > 0)
                    .then_some((
                        index * self.packet_bytes,
                        (descriptor.actual_length as usize).min(self.packet_bytes),
                    ))
            })?
        } else {
            (
                0,
                user_data
                    .completion
                    .actual_length
                    .load(Ordering::Acquire)
                    .min(buffer.len()),
            )
        };

        let available = available.min(buffer.len().saturating_sub(offset));
        let report_len = if available >= 4 {
            4
        } else if available >= 3 {
            3
        } else {
            return None;
        };
        Some(buffer[offset + available - report_len..offset + available].to_vec())
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
            dev_eprintln!(
                "[USB] in-flight feedback transfer survived teardown drain; leaking slot to avoid use-after-free"
            );
            std::mem::forget(unsafe { Box::from_raw(self.user_data) });
            return;
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
    pub(super) announced_report: bool,
}

pub(super) fn pump_feedback_transfer(
    handle: &DeviceHandle<Context>,
    slot: &mut FeedbackTransferSlot,
    scheduler: &mut IsoPacketScheduler,
    data_service_interval_us: u32,
    speed: Speed,
    counters: &mut FeedbackCounters,
    stop: &AtomicBool,
) {
    let endpoint = slot.endpoint;
    if let Some((status, completed_at, _actual_length)) = slot.take_completion() {
        slot.mark_completed();

        match status {
            LIBUSB_TRANSFER_COMPLETED => {
                if let Some(raw_bytes) = slot.report_bytes() {
                    if let Some((frames_per_packet, estimated_sample_rate)) =
                        decode_feedback_report(&raw_bytes, data_service_interval_us, speed)
                    {
                        if scheduler.update_feedback_frames_per_packet(frames_per_packet) {
                            counters.reports = counters.reports.saturating_add(1);
                            counters.last_report_at = Some(completed_at);
                            counters.last_frames_per_packet = Some(frames_per_packet);
                            counters.last_estimated_sample_rate = Some(estimated_sample_rate);
                            if !counters.announced_report || counters.reports % 128 == 0 {
                                dev_eprintln!(
                                    "[USB] Feedback endpoint 0x{:02x} {} bytes={:02x?} framesPerPacket={:.4} sampleRate≈{:.2}Hz (dataService={}us)",
                                    endpoint.address,
                                    transfer_type_label(endpoint.transfer_type),
                                    raw_bytes,
                                    frames_per_packet,
                                    estimated_sample_rate,
                                    data_service_interval_us,
                                );
                                counters.announced_report = true;
                            }
                        } else {
                            counters.rejected = counters.rejected.saturating_add(1);
                            if counters.rejected <= 4 || counters.rejected % 64 == 0 {
                                dev_eprintln!(
                                    "[USB] Feedback endpoint 0x{:02x} rejected implausible framesPerPacket={:.4} (nominal dataService={}us)",
                                    endpoint.address,
                                    frames_per_packet,
                                    data_service_interval_us,
                                );
                            }
                        }
                    } else {
                        counters.rejected = counters.rejected.saturating_add(1);
                        if counters.rejected <= 4 || counters.rejected % 64 == 0 {
                            dev_eprintln!(
                                "[USB] Feedback endpoint 0x{:02x} rejected malformed report length={}",
                                endpoint.address,
                                raw_bytes.len()
                            );
                        }
                    }
                }
            }
            LIBUSB_TRANSFER_TIMED_OUT | LIBUSB_TRANSFER_CANCELLED => {
                counters.timeouts = counters.timeouts.saturating_add(1);
            }
            LIBUSB_TRANSFER_STALL => {
                counters.errors = counters.errors.saturating_add(1);
                dev_eprintln!(
                    "[USB] Feedback endpoint 0x{:02x} stalled; keeping last feedback-derived packet rate",
                    endpoint.address
                );
            }
            LIBUSB_TRANSFER_NO_DEVICE => {
                counters.errors = counters.errors.saturating_add(1);
            }
            other => {
                counters.errors = counters.errors.saturating_add(1);
                if counters.errors <= 4 || counters.errors % 64 == 0 {
                    dev_eprintln!(
                        "[USB] Feedback endpoint 0x{:02x} transfer status {} ({})",
                        endpoint.address,
                        other,
                        iso_transfer_status_label(other)
                    );
                }
            }
        }
    }

    if !usb_stop_requested(stop) && !slot.in_flight {
        if let Err(error) = slot.submit(handle) {
            counters.errors = counters.errors.saturating_add(1);
            if counters.errors <= 4 || counters.errors % 64 == 0 {
                dev_eprintln!(
                    "[USB] Feedback endpoint 0x{:02x} resubmit failed: {}",
                    endpoint.address,
                    error
                );
            }
        }
    }
}

/// Decode UAC feedback into frames per data-endpoint service interval.
///
/// UAC2 feedback reports samples per microframe on high-speed links and
/// samples per frame on full-speed links. The feedback endpoint's own polling
/// interval is not the data endpoint's packet interval; using it as the
/// timebase previously produced a rate error of up to 8x at 125 us service.
pub(super) fn decode_feedback_report(
    raw_bytes: &[u8],
    data_service_interval_us: u32,
    speed: Speed,
) -> Option<(f64, f64)> {
    let (frames_per_base_interval, base_interval_us) = match raw_bytes.len() {
        3 => {
            let raw = u32::from(raw_bytes[0])
                | (u32::from(raw_bytes[1]) << 8)
                | (u32::from(raw_bytes[2]) << 16);
            let base_interval_us = feedback_base_interval_us(speed, data_service_interval_us);
            (raw as f64 / 16_384.0, base_interval_us)
        }
        4.. => {
            let raw = u32::from_le_bytes([raw_bytes[0], raw_bytes[1], raw_bytes[2], raw_bytes[3]]);
            let base_interval_us = feedback_base_interval_us(speed, data_service_interval_us);
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

