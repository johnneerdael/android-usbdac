use super::*;
use std::collections::VecDeque;

#[derive(Debug)]
pub(super) struct IsoTransferCompletion {
    pub(super) status: StdMutex<Option<i32>>,
    pub(super) completed_at: StdMutex<Option<Instant>>,
    pub(super) actual_length: AtomicUsize,
    generation: std::sync::atomic::AtomicU64,
    returned: std::sync::atomic::AtomicU64,
    consumed: std::sync::atomic::AtomicBool,
}

impl IsoTransferCompletion {
    pub(super) fn new() -> Self {
        Self {
            status: StdMutex::new(None),
            completed_at: StdMutex::new(None),
            actual_length: AtomicUsize::new(0),
            generation: std::sync::atomic::AtomicU64::new(0),
            returned: std::sync::atomic::AtomicU64::new(0),
            consumed: std::sync::atomic::AtomicBool::new(false),
        }
    }

    pub(super) fn reset(&self) {
        let generation = self
            .generation
            .load(Ordering::Relaxed)
            .checked_add(1)
            .expect("transfer generation exhausted");
        self.consumed.store(false, Ordering::Relaxed);
        self.generation.store(generation, Ordering::Release);
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
        let generation = self.generation.load(Ordering::Acquire);
        generation != 0 && self.returned.load(Ordering::Acquire) == generation
    }

    #[cfg(test)]
    pub(super) fn publish_for_test(&self) {
        self.returned
            .store(self.generation.load(Ordering::Acquire), Ordering::Release);
    }

    pub(super) fn take(&self) -> Option<(i32, Option<Instant>, usize)> {
        if !self.is_published() || self.consumed.swap(true, Ordering::AcqRel) {
            return None;
        }
        let status = match self.status.lock() {
            Ok(mut guard) => guard.take(),
            Err(poisoned) => poisoned.into_inner().take(),
        }
        .unwrap_or(LIBUSB_TRANSFER_ERROR);
        let completed_at = match self.completed_at.lock() {
            Ok(mut guard) => guard.take(),
            Err(poisoned) => poisoned.into_inner().take(),
        };
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
    retirement_pending: bool,
    retirement_context: Option<Context>,
    retirement_owner: Option<Arc<TransferOwner>>,
    /// The flush asked libusb to cancel this transfer. Cleared when it comes
    /// back, whatever its status: a transfer that finished first is still
    /// accounted as played.
    pub(super) cancelled_for_flush: bool,
}

pub(super) fn data_packet_prefix_bytes() -> usize {
    if settings().source_has(crate::Quirk::TxLengthPrefix) {
        4
    } else {
        0
    }
}

fn add_tx_length_prefix(payload: &mut IsoTransferPayload) -> Result<(), String> {
    let expected = payload
        .packet_sizes
        .iter()
        .try_fold(0usize, |sum, size| sum.checked_add(*size))
        .ok_or("ISO payload size overflow")?;
    if expected != payload.transfer_buffer.len() {
        return Err("TX_LENGTH payload schedule changed".into());
    }
    let capacity = expected
        .checked_add(
            payload
                .packet_sizes
                .len()
                .checked_mul(4)
                .ok_or("TX_LENGTH header count overflow")?,
        )
        .ok_or("TX_LENGTH wire capacity overflow")?;
    let mut wire = Vec::with_capacity(capacity);
    let mut offset = 0;
    for size in &mut payload.packet_sizes {
        let length = u32::try_from(*size).map_err(|_| "TX_LENGTH audio span too large")?;
        wire.extend_from_slice(&length.to_le_bytes());
        wire.extend_from_slice(&payload.transfer_buffer[offset..offset + *size]);
        offset += *size;
        *size += 4;
    }
    payload.transfer_buffer = wire;
    Ok(())
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

/// Validate the complete descriptor schedule before exposing it to libusb.
fn validate_submission_geometry(packet_sizes: &[usize], buffer_len: usize) -> Result<(), String> {
    if packet_sizes.len() > i32::MAX as usize || buffer_len > i32::MAX as usize {
        return Err("isochronous request exceeds libusb limits".into());
    }
    let total = packet_sizes
        .iter()
        .try_fold(0usize, |sum, &size| {
            if size > u32::MAX as usize {
                return None;
            }
            sum.checked_add(size)
        })
        .ok_or("invalid isochronous packet length or sum")?;
    if total != buffer_len {
        return Err("isochronous packet lengths do not match transfer buffer".into());
    }
    Ok(())
}

impl IsoTransferSlot {
    pub(super) fn retain_owner(&mut self, owner: Arc<TransferOwner>) {
        assert!(!self.in_flight && !self.retirement_pending);
        self.retirement_owner = Some(owner);
    }
    fn require_owner(&self, handle: &DeviceHandle<Context>) -> Result<(), String> {
        if self
            .retirement_owner
            .as_ref()
            .is_some_and(|owner| owner.matches(handle))
        {
            Ok(())
        } else {
            Err("transfer has no matching native owner".into())
        }
    }
    pub(super) fn new(
        slot_index: usize,
        packet_count: usize,
        buffer_capacity: usize,
    ) -> Result<Self, String> {
        if packet_count > i32::MAX as usize || buffer_capacity > i32::MAX as usize {
            return Err("invalid isochronous slot geometry".into());
        }
        let completion = Arc::new(IsoTransferCompletion::new());
        let user_data = Box::new(IsoTransferUserData {
            completion,
            buffer: vec![0u8; buffer_capacity.max(1)],
        });
        let user_data_ptr = Box::into_raw(user_data);
        // SAFETY: packet_count fits libusb's signed count; allocation is freed in Drop.
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
            retirement_pending: false,
            retirement_context: None,
            retirement_owner: None,
            cancelled_for_flush: false,
        })
    }

    pub(super) fn submit(
        &mut self,
        handle: &DeviceHandle<Context>,
        endpoint: u8,
        payload: IsoTransferPayload,
    ) -> Result<(), String> {
        self.submit_with_timeout(handle, endpoint, payload, ISO_TRANSFER_TIMEOUT_MS)
    }

    pub(super) fn submit_with_timeout(
        &mut self,
        handle: &DeviceHandle<Context>,
        endpoint: u8,
        mut payload: IsoTransferPayload,
        timeout_ms: u32,
    ) -> Result<(), String> {
        if self.in_flight || self.retirement_pending {
            return Err("isochronous transfer slot is not retired".to_string());
        }

        if payload.packet_sizes.len() != self.packet_sizes.len() {
            return Err(format!(
                "isochronous packet count mismatch: slot={}, payload={}",
                self.packet_sizes.len(),
                payload.packet_sizes.len()
            ));
        }

        // Payload verification ran before framing. The extra bytes carry no
        // samples and never enter queued frame/music counts.
        if endpoint & 0x80 == 0 && settings().source_has(crate::Quirk::TxLengthPrefix) {
            add_tx_length_prefix(&mut payload)?;
        }
        validate_submission_geometry(&payload.packet_sizes, payload.transfer_buffer.len())?;
        self.require_owner(handle)?;
        self.retirement_context = Some(handle.context().clone());
        // SAFETY: this idle slot owns its transfer and buffer; the matching owner retains
        // the handle and fd until the callback returns ownership, including during unwind.
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
                timeout_ms,
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
            if submit_result == 0 {
                self.in_flight = true;
            }
            if submit_result != 0 {
                if submit_result == libusb1_sys::constants::LIBUSB_ERROR_NO_DEVICE {
                    return Err("USB DAC disconnected (isochronous submit)".into());
                }
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
    pub(super) fn submit_capture(
        &mut self,
        handle: &DeviceHandle<Context>,
        endpoint: u8,
        packet_bytes: usize,
        trace: Option<&crate::feedback_diagnostics::FeedbackRecorder>,
    ) -> Result<(), String> {
        if self.in_flight || self.retirement_pending {
            return Err("capture slot is not retired".into());
        }
        if endpoint & 0x80 == 0
            || packet_bytes == 0
            || packet_bytes > u32::MAX as usize
            || self.packet_sizes.len() > i32::MAX as usize
        {
            return Err("invalid capture request geometry".into());
        }
        self.require_owner(handle)?;
        self.retirement_context = Some(handle.context().clone());
        // SAFETY: the slot is idle and exclusively owns the native transfer and buffer;
        // the retained matching owner outlives all libusb access.
        unsafe {
            let data = &mut *self.user_data;
            data.completion.reset();
            let bytes = packet_bytes
                .checked_mul(self.packet_sizes.len())
                .ok_or("capture size overflow")?;
            if bytes > data.buffer.len() || bytes > i32::MAX as usize {
                return Err("capture buffer too small or request too large".into());
            }
            libusb_fill_iso_transfer(
                self.transfer.as_mut(),
                handle.as_raw(),
                endpoint,
                data.buffer.as_mut_ptr(),
                bytes as i32,
                self.packet_sizes.len() as i32,
                iso_transfer_callback,
                self.user_data as *mut c_void,
                ISO_TRANSFER_TIMEOUT_MS,
            );
            for index in 0..self.packet_sizes.len() {
                (*self
                    .transfer
                    .as_mut()
                    .iso_packet_desc
                    .as_mut_ptr()
                    .add(index))
                .length = packet_bytes as u32;
            }
            let result = libusb_submit_transfer(self.transfer.as_ptr());
            if result == 0 {
                self.in_flight = true;
                self.packet_sizes.fill(packet_bytes);
                self.generation = self.generation.saturating_add(1);
                self.submitted_endpoint = endpoint;
                self.submitted_bytes = bytes;
                self.in_flight = true;
                self.submitted_at = Some(Instant::now());
            }
            if let Some(trace) = trace {
                trace.record(crate::FeedbackTraceEvent::Submit {
                    slot: Some(self.slot_index),
                    status: result,
                });
            }
            if result != 0 {
                return Err(
                    if result == libusb1_sys::constants::LIBUSB_ERROR_NO_DEVICE {
                        "USB DAC disconnected (implicit capture submit)".into()
                    } else {
                        format!("implicit capture submit failed: {}", result)
                    },
                );
            }
        }
        Ok(())
    }

    #[cfg(test)]
    pub(super) fn publish_completion_for_test(&mut self) {
        // SAFETY: test slot owns the allocated user data, and no native transfer was submitted.
        let completion = unsafe { &(*self.user_data).completion };
        completion.reset();
        *completion.status.lock().unwrap() = Some(LIBUSB_TRANSFER_COMPLETED);
        *completion.completed_at.lock().unwrap() = Some(Instant::now());
        completion.publish_for_test();
    }

    pub(super) fn take_completion(&mut self) -> Option<(i32, Option<Instant>, usize)> {
        let completion = unsafe { &(*self.user_data).completion };
        if !completion.is_published() {
            return None;
        }
        // Retire native ownership before any consumer bookkeeping can unwind.
        self.in_flight = false;
        let result = completion.take();
        if result.is_some() {
            self.retirement_pending = true;
        }
        result
    }

    pub(super) fn turnaround_us(&self, completed_at: Instant) -> Option<u64> {
        self.submitted_at.map(|submitted_at| {
            completed_at
                .saturating_duration_since(submitted_at)
                .as_micros()
                .min(u128::from(u64::MAX)) as u64
        })
    }

    pub(super) fn validate_packets(
        &self,
        acceptance: PacketAcceptance,
    ) -> Result<CompletionStats, String> {
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

    /// Validate descriptor geometry before any recovery accounting.
    fn failed_transfer_geometry(&self) -> Result<(), String> {
        let transfer = unsafe { self.transfer.as_ref() };
        if transfer.num_iso_packets != self.packet_sizes.len() as i32 {
            return Err("failed transfer packet count changed".into());
        }
        let descriptors = unsafe {
            std::slice::from_raw_parts(transfer.iso_packet_desc.as_ptr(), self.packet_sizes.len())
        };
        if descriptors
            .iter()
            .zip(&self.packet_sizes)
            .any(|(packet, requested)| {
                packet.length as usize != *requested || packet.actual_length as usize > *requested
            })
        {
            return Err("failed transfer packet geometry changed".into());
        }
        if descriptors
            .iter()
            .any(|packet| packet.status == LIBUSB_TRANSFER_NO_DEVICE)
        {
            return Err("USB DAC disconnected".into());
        }
        Ok(())
    }

    /// A timed-out URB retains individually completed packet evidence.
    pub(super) fn account_timeout(
        &self,
        acceptance: PacketAcceptance,
    ) -> Result<CompletionStats, String> {
        self.failed_transfer_geometry()?;
        let transfer = unsafe { self.transfer.as_ref() };
        Ok(timed_out_packet_results(
            transfer,
            &self.packet_sizes,
            acceptance,
        ))
    }

    /// A failed transfer does not establish delivery; charge its full payload.
    pub(super) fn account_failed(
        &self,
        acceptance: PacketAcceptance,
    ) -> Result<CompletionStats, String> {
        self.failed_transfer_geometry()?;
        Ok(account_timed_out_transfer(
            &self.packet_sizes,
            &[],
            acceptance,
        ))
    }

    pub(super) fn mark_completed(&mut self) {
        // A stale retirement must never erase a newly submitted generation.
        if self.in_flight || !self.retirement_pending {
            return;
        }
        self.retirement_pending = false;
        self.queued_frame_count = 0;
        self.queued_underrun = false;
        self.queued_held = false;
        self.queued_music_frames = 0;
        self.submitted_at = None;
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
        let completion = unsafe { &(*self.user_data).completion };
        completion.is_published() && !completion.consumed.load(Ordering::Acquire)
    }
}

impl Drop for IsoTransferSlot {
    fn drop(&mut self) {
        if self.in_flight {
            // Also protects unwinding: the handle/FD owner outlives the slots.
            self.cancel();
            if self.retirement_context.is_none() {
                if let Some(owner) = self.retirement_owner.take() {
                    std::mem::forget(owner);
                }
                return; // Keep native transfer and callback buffer alive too.
            }
            if let Some(context) = self.retirement_context.clone() {
                let mut warned = false;
                while self.in_flight {
                    if self.take_completion().is_some() {
                        self.mark_completed();
                        break;
                    }
                    if let Err(error) = context.handle_events(Some(Duration::from_millis(10))) {
                        if !warned {
                            log_warn!("USB unwind cancellation retirement pending: {}", error);
                            warned = true;
                        }
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

/// AudioControl status IN transfer, retired by the stream's existing event owner.
pub(super) struct InterruptTransferSlot {
    slot: IsoTransferSlot,
    packet_bytes: usize,
}

impl InterruptTransferSlot {
    pub(super) fn new(packet_bytes: usize) -> Result<Self, String> {
        if packet_bytes == 0 || packet_bytes > i32::MAX as usize {
            return Err("invalid interrupt transfer capacity".into());
        }
        Ok(Self {
            slot: IsoTransferSlot::new(0, 0, packet_bytes)?,
            packet_bytes,
        })
    }

    pub(super) fn retain_owner(&mut self, owner: Arc<TransferOwner>) {
        self.slot.retain_owner(owner);
    }

    pub(super) fn submit(
        &mut self,
        handle: &DeviceHandle<Context>,
        endpoint: u8,
    ) -> Result<(), Error> {
        self.slot
            .require_owner(handle)
            .map_err(Error::InvalidState)?;
        if self.slot.in_flight || self.slot.retirement_pending {
            return Err(Error::InvalidState(
                "interrupt transfer already in flight".into(),
            ));
        }
        if endpoint & 0x80 == 0 || endpoint & 0x70 != 0 || endpoint & 0x0f == 0 {
            return Err(Error::InvalidArgument(
                "invalid interrupt IN endpoint".into(),
            ));
        }

        unsafe {
            let data = &mut *self.slot.user_data;
            if self.packet_bytes == 0
                || self.packet_bytes > i32::MAX as usize
                || self.packet_bytes > data.buffer.len()
            {
                return Err(Error::InvalidArgument(
                    "invalid interrupt transfer capacity".into(),
                ));
            }
            data.completion.reset();
            self.slot.retirement_context = Some(handle.context().clone());
            libusb_fill_interrupt_transfer(
                self.slot.transfer.as_mut(),
                handle.as_raw(),
                endpoint,
                data.buffer.as_mut_ptr(),
                self.packet_bytes as i32,
                iso_transfer_callback,
                self.slot.user_data as *mut c_void,
                0, // Status endpoints may NAK indefinitely until a change occurs.
            );
            let result = libusb_submit_transfer(self.slot.transfer.as_ptr());
            if result == 0 {
                self.slot.in_flight = true;
            }
            if result != 0 {
                return Err(
                    if result == libusb1_sys::constants::LIBUSB_ERROR_NO_DEVICE {
                        Error::DeviceLost
                    } else {
                        Error::Transfer(format!("interrupt submit failed with code {}", result))
                    },
                );
            }
        }
        self.slot.submitted_endpoint = endpoint;
        self.slot.submitted_bytes = self.packet_bytes;
        self.slot.in_flight = true;
        Ok(())
    }

    pub(super) fn take_completion(&mut self) -> Option<(i32, &[u8])> {
        if !self.slot.in_flight {
            return None;
        }
        let (mut status, _, actual_length) = self.slot.take_completion()?;
        self.slot.mark_completed();
        let buffer = unsafe { &(*self.slot.user_data).buffer };
        let length = if status == LIBUSB_TRANSFER_COMPLETED {
            if actual_length > self.packet_bytes || actual_length > buffer.len() {
                status = libusb1_sys::constants::LIBUSB_TRANSFER_OVERFLOW;
                0
            } else {
                actual_length
            }
        } else {
            0
        };
        Some((status, &buffer[..length]))
    }
}

/// Private deferred compatibility state; release construction is neutral.
#[derive(Default, Debug)]
pub(super) struct FeedbackCompatibility {
    tenor_nominal_q16: Option<u32>,
    startup_packets_remaining: u8,
}

impl FeedbackCompatibility {
    pub(super) fn for_device(
        _vendor: u16,
        _product: u16,
        _speed: Speed,
        _endpoint: FeedbackEndpoint,
        _sample_rate: u32,
    ) -> Self {
        // Shared-parity release keeps universal feedback/timebase handling.
        // Tenor correction and Playback Designs startup skipping are deferred.
        Self::default()
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

const NO_FEEDBACK_PACKET_STATUS: i32 = i32::MIN;

/// Persistent asynchronous feedback IN transfers owned by the output event loop.
/// Keep a second batch submitted while the output thread handles the first.
/// Divide the existing polling window between slots rather than growing it:
/// blocking reads can otherwise drain the queued OUT audio while awaiting IN.
pub(super) struct FeedbackTransfers {
    pub(super) slots: Vec<FeedbackTransferSlot>,
    pending: VecDeque<usize>,
    // Startup skips and device corrections belong to the ordered pipe, not each slot.
    compatibility: FeedbackCompatibility,
}

fn feedback_poll_geometry(endpoint: FeedbackEndpoint, bus_queue_ms: u32) -> (usize, usize) {
    if endpoint.transfer_type != TransferType::Isochronous {
        return (1, 1);
    }
    let window_packets = (USB_FEEDBACK_POLL_WINDOW_MS.min(bus_queue_ms.max(1)) * 1_000)
        / endpoint.service_interval_us.max(125);
    let packets = window_packets.clamp(1, 32) as usize;
    let slots = packets.min(2);
    (slots, packets / slots)
}

impl FeedbackTransfers {
    pub(super) fn new(
        endpoint: FeedbackEndpoint,
        bus_queue_ms: u32,
        compatibility: FeedbackCompatibility,
    ) -> Result<Self, String> {
        let (count, packets_per_slot) = feedback_poll_geometry(endpoint, bus_queue_ms);
        let slots = (0..count)
            .map(|_| FeedbackTransferSlot::new(endpoint, packets_per_slot))
            .collect::<Result<_, _>>()?;
        Ok(Self {
            slots,
            pending: VecDeque::with_capacity(count),
            compatibility,
        })
    }

    pub(super) fn start(
        &mut self,
        claimed: &ClaimedHandle,
        counters: &mut FeedbackCounters,
    ) -> Result<(), FeedbackFault> {
        let owner = claimed.transfer_owner();
        for slot in &mut self.slots {
            slot.retirement_owner = Some(owner.clone());
        }
        let handle = &claimed.handle;
        for index in 0..self.slots.len() {
            self.submit(index, handle, counters, index == 0)?;
        }
        Ok(())
    }

    fn take_next_completion(&mut self) -> Option<(usize, i32, Option<Instant>, usize)> {
        let index = *self.pending.front()?;
        let slot = &mut self.slots[index];
        let (status, completed_at, actual_length) = slot.take_completion()?;
        self.pending.pop_front();
        slot.mark_completed();
        Some((index, status, completed_at, actual_length))
    }

    fn submit(
        &mut self,
        index: usize,
        handle: &DeviceHandle<Context>,
        counters: &mut FeedbackCounters,
        first: bool,
    ) -> Result<(), FeedbackFault> {
        let slot = &mut self.slots[index];
        let result = slot.submit(handle);
        self.record_submission(index, result, counters, first)
    }

    fn record_submission(
        &mut self,
        index: usize,
        result: Result<(), String>,
        counters: &mut FeedbackCounters,
        first: bool,
    ) -> Result<(), FeedbackFault> {
        let slot = &mut self.slots[index];
        counters.record_submit(index, slot.last_submit_status);
        match result {
            Ok(()) => self.pending.push_back(index),
            Err(error) => {
                counters.errors = counters.errors.saturating_add(1);
                if let Some(fault) =
                    feedback_submit_failure(error.clone(), slot.submit_no_device, first)
                {
                    return Err(fault);
                }
                slot.retry_at = Instant::now() + Duration::from_millis(5);
                if counters.errors <= 4 || counters.errors % 64 == 0 {
                    log_warn!("explicit feedback resubmit failed: {}", error);
                }
            }
        }
        Ok(())
    }
}

/// Persistent asynchronous feedback IN transfer and its buffer.
pub(super) struct FeedbackTransferSlot {
    pub(super) transfer: std::ptr::NonNull<libusb_transfer>,
    pub(super) user_data: *mut IsoTransferUserData,
    pub(super) endpoint: FeedbackEndpoint,
    pub(super) packet_count: usize,
    pub(super) packet_bytes: usize,
    pub(super) timeout_ms: u32,
    pub(super) in_flight: bool,
    retirement_pending: bool,
    retirement_context: Option<Context>,
    retirement_owner: Option<Arc<TransferOwner>>,
    pub(super) submit_no_device: bool,
    pub(super) last_submit_status: Option<i32>,
    retry_at: Instant,
}

impl FeedbackTransferSlot {
    fn new(endpoint: FeedbackEndpoint, packet_count: usize) -> Result<Self, String> {
        let packet_bytes = endpoint.max_packet_bytes.max(4);
        let buffer_len = packet_bytes
            .checked_mul(packet_count)
            .ok_or("feedback size overflow")?;
        if packet_count == 0
            || packet_count > i32::MAX as usize
            || packet_bytes > u32::MAX as usize
            || buffer_len > i32::MAX as usize
        {
            return Err("invalid feedback request geometry".into());
        }

        let poll_window_ms =
            ((packet_count as u64).saturating_mul(endpoint.service_interval_us as u64) / 1_000)
                .max(1) as u32;
        let timeout_ms = USB_FEEDBACK_TIMEOUT_MS.max(poll_window_ms.saturating_add(20));

        let completion = Arc::new(IsoTransferCompletion::new());
        let user_data = Box::new(IsoTransferUserData {
            completion,
            buffer: vec![0u8; buffer_len.max(4)],
        });
        let user_data_ptr = Box::into_raw(user_data);
        // SAFETY: packet_count fits libusb's signed count; allocation is freed in Drop.
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
            retirement_owner: None,
            retirement_pending: false,
            submit_no_device: false,
            last_submit_status: None,
            retry_at: Instant::now(),
        })
    }

    pub(super) fn submit(&mut self, handle: &DeviceHandle<Context>) -> Result<(), String> {
        self.last_submit_status = None;
        if self.in_flight || self.retirement_pending {
            return Err("feedback transfer slot is not retired".to_string());
        }

        if !self
            .retirement_owner
            .as_ref()
            .is_some_and(|owner| owner.matches(handle))
        {
            return Err("feedback transfer has no matching native owner".into());
        }
        self.retirement_context = Some(handle.context().clone());
        // SAFETY: this idle slot owns its transfer and buffer; the matching owner retains
        // the handle and fd until the callback returns ownership, including during unwind.
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
                        let packet = &mut *self
                            .transfer
                            .as_mut()
                            .iso_packet_desc
                            .as_mut_ptr()
                            .add(index);
                        packet.length = self.packet_bytes as u32;
                        // A transfer can fail before every packet is reported.
                        // Do not reuse lengths or statuses from its last poll.
                        packet.actual_length = 0;
                        packet.status = NO_FEEDBACK_PACKET_STATUS;
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
            if submit_result == 0 {
                self.in_flight = true;
            }
            self.last_submit_status = Some(submit_result);
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

    pub(super) fn take_completion(&mut self) -> Option<(i32, Option<Instant>, usize)> {
        let completion = unsafe { &(*self.user_data).completion };
        if !completion.is_published() {
            return None;
        }
        self.in_flight = false;
        let result = completion.take();
        if result.is_some() {
            self.retirement_pending = true;
        }
        result
    }

    fn newest_valid_report(
        &mut self,
        scheduler: &IsoPacketScheduler,
        service_us: u32,
        speed: Speed,
        max_frames: usize,
        per_service: bool,
        counters: &mut FeedbackCounters,
        actual_length: usize,
        transfer_status: i32,
        compatibility: &mut FeedbackCompatibility,
    ) -> Option<(f64, f64)> {
        let buffer = unsafe { &(*self.user_data).buffer };
        if self.endpoint.transfer_type == TransferType::Isochronous {
            let transfer = unsafe { self.transfer.as_ref() };
            if transfer.num_iso_packets < 0
                || transfer.num_iso_packets as usize != self.packet_count
            {
                if transfer_status == LIBUSB_TRANSFER_COMPLETED {
                    counters.errors = counters.errors.saturating_add(1);
                }
                return None;
            }
            inspect_feedback_packets_with_compatibility(
                buffer,
                (0..self.packet_count).map(|index| {
                    let packet = unsafe { &*transfer.iso_packet_desc.as_ptr().add(index) };
                    (
                        packet.status,
                        packet.length as usize,
                        packet.actual_length as usize,
                    )
                }),
                scheduler,
                service_us,
                speed,
                max_frames,
                per_service,
                counters,
                transfer_status,
                true,
                compatibility,
            )
        } else {
            inspect_feedback_packets_with_compatibility(
                buffer,
                [(transfer_status, self.packet_bytes, actual_length)],
                scheduler,
                service_us,
                speed,
                max_frames,
                per_service,
                counters,
                transfer_status,
                false,
                compatibility,
            )
        }
    }

    pub(super) fn mark_completed(&mut self) {
        if !self.in_flight {
            self.retirement_pending = false;
        }
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
            if self.retirement_context.is_none() {
                if let Some(owner) = self.retirement_owner.take() {
                    std::mem::forget(owner);
                }
                return; // Keep native transfer and callback buffer alive too.
            }
            if let Some(context) = self.retirement_context.clone() {
                let mut warned = false;
                while self.in_flight {
                    if self.take_completion().is_some() {
                        self.mark_completed();
                        break;
                    }
                    if let Err(error) = context.handle_events(Some(Duration::from_millis(10))) {
                        if !warned {
                            log_warn!("USB unwind cancellation retirement pending: {}", error);
                            warned = true;
                        }
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
    last_report_bytes: [u8; 8],
    last_report_len: usize,
    pub(super) diagnostics: Option<crate::ExplicitFeedbackDiagnostics>,
    pub(super) diagnostics_dirty: bool,
    pub(super) trace: Option<Arc<crate::feedback_diagnostics::FeedbackRecorder>>,
    pub(super) infer_service_interval: bool,
    pub(super) cached_service_interval: bool,
    selected_timebase: Option<FeedbackTimebase>,
    service_interval_confirmations: u8,
    redetecting: bool,
    /// Locked power-of-two shift for four-byte bus-timebase reports, as ALSA's
    /// `ep->freqshift`: `None` until the first report, then 0 (Q16.16) or
    /// `PADDED_Q10_14_SHIFT`. Cleared by an out-of-window report.
    feedback_shift: Option<i32>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FeedbackTimebase {
    Bus,
    OutService,
}

const SERVICE_INTERVAL_CONFIRMATIONS: u8 = 3;

/// Q10.14 carried in a four-byte Q16.16 slot reads 4x low (USB 2.0 section
/// 5.12.4.2 defines 10.14 in 3 bytes for full speed, 16.16 in 4 bytes for high
/// speed; ALSA endpoint.c:1855-1863 notes devices that mix them up).
const PADDED_Q10_14_SHIFT: i32 = 2;

/// ALSA endpoint.c snd_usb_handle_sync_urb:1889-1905: shift the first report
/// left or right until it lies within -25%..+50% of nominal, and return that
/// shift. The caller still applies the admission window afterwards.
fn alsa_feedback_shift(frames: f64, nominal: f64) -> Option<i32> {
    if !(frames.is_finite() && frames > 0.0 && nominal.is_finite() && nominal > 0.0) {
        return None;
    }
    let (mut f, mut shift) = (frames, 0i32);
    while f < nominal - nominal / 4.0 {
        f *= 2.0;
        shift += 1;
    }
    while f > nominal + nominal / 2.0 {
        f /= 2.0;
        shift -= 1;
    }
    Some(shift)
}

fn shift_feedback((frames, rate): (f64, f64), shift: i32) -> (f64, f64) {
    let scale = 2f64.powi(shift);
    (frames * scale, rate * scale)
}

impl FeedbackCounters {
    pub(super) fn selected_service_interval(&self) -> bool {
        self.selected_timebase == Some(FeedbackTimebase::OutService)
    }

    fn record_submit(&mut self, index: usize, status: Option<i32>) {
        if let Some(status) = status {
            if let Some(diagnostics) = self.diagnostics.as_mut() {
                diagnostics.record_submit(status);
                self.diagnostics_dirty = true;
            }
            if let Some(trace) = &self.trace {
                trace.record(crate::FeedbackTraceEvent::Submit {
                    slot: Some(index),
                    status,
                });
            }
        }
    }

    fn select_timebase(&mut self, timebase: FeedbackTimebase, speed: Speed, inferred: bool) {
        self.selected_timebase = Some(timebase);
        let unit = match (timebase, speed) {
            (FeedbackTimebase::OutService, _) => "out_service_interval",
            (_, Speed::Full) => "usb_frame",
            _ => "usb_microframe",
        };
        if let Some(diagnostics) = self.diagnostics.as_mut() {
            diagnostics.timebase = Some(unit);
            diagnostics.inferred_timebase = inferred;
            diagnostics.reused_timebase = inferred && self.cached_service_interval;
        }
        if let Some(trace) = &self.trace {
            trace.record(crate::FeedbackTraceEvent::Timebase { unit, inferred });
        }
        if inferred && self.cached_service_interval {
            log_info!(
                "[USB] Explicit feedback timebase reused for DAC session: {}",
                unit
            );
        } else if inferred {
            log_info!(
                "[USB] Explicit feedback timebase inferred: {} ({} matching completions)",
                unit,
                SERVICE_INTERVAL_CONFIRMATIONS
            );
        }
    }

    fn record_valid_bytes(&mut self, bytes: &[u8]) {
        self.last_report_bytes = [0; 8];
        self.last_report_bytes[..bytes.len()].copy_from_slice(bytes);
        self.last_report_len = bytes.len();
    }
}

/// Scan requested packet spans, never a shortened/concatenated actual-length prefix.
/// Validate all candidates before selecting one; commit the selected timing rate only once per transfer.
#[cfg(test)]
fn select_feedback_report(
    buffer: &[u8],
    packets: impl IntoIterator<Item = (i32, usize, usize)>,
    scheduler: &IsoPacketScheduler,
    service_us: u32,
    speed: Speed,
    max_frames: usize,
    per_service: bool,
    counters: &mut FeedbackCounters,
) -> Option<(f64, f64)> {
    inspect_feedback_packets(
        buffer,
        packets,
        scheduler,
        service_us,
        speed,
        max_frames,
        per_service,
        counters,
        LIBUSB_TRANSFER_COMPLETED,
        true,
    )
}

#[cfg(test)]
fn select_feedback_report_with_compatibility(
    buffer: &[u8],
    packets: impl IntoIterator<Item = (i32, usize, usize)>,
    scheduler: &IsoPacketScheduler,
    service_us: u32,
    speed: Speed,
    max_frames: usize,
    per_service: bool,
    counters: &mut FeedbackCounters,
    compatibility: &mut FeedbackCompatibility,
) -> Option<(f64, f64)> {
    inspect_feedback_packets_with_compatibility(
        buffer,
        packets,
        scheduler,
        service_us,
        speed,
        max_frames,
        per_service,
        counters,
        LIBUSB_TRANSFER_COMPLETED,
        true,
        compatibility,
    )
}

#[cfg(test)]
fn inspect_feedback_packets(
    buffer: &[u8],
    packets: impl IntoIterator<Item = (i32, usize, usize)>,
    scheduler: &IsoPacketScheduler,
    service_us: u32,
    speed: Speed,
    max_frames: usize,
    per_service: bool,
    counters: &mut FeedbackCounters,
    transfer_status: i32,
    is_iso: bool,
) -> Option<(f64, f64)> {
    inspect_feedback_packets_with_compatibility(
        buffer,
        packets,
        scheduler,
        service_us,
        speed,
        max_frames,
        per_service,
        counters,
        transfer_status,
        is_iso,
        &mut FeedbackCompatibility::default(),
    )
}

fn inspect_feedback_packets_with_compatibility(
    buffer: &[u8],
    packets: impl IntoIterator<Item = (i32, usize, usize)>,
    scheduler: &IsoPacketScheduler,
    service_us: u32,
    speed: Speed,
    max_frames: usize,
    per_service: bool,
    counters: &mut FeedbackCounters,
    transfer_status: i32,
    is_iso: bool,
    compatibility: &mut FeedbackCompatibility,
) -> Option<(f64, f64)> {
    use crate::ExplicitFeedbackOutcome as Outcome;
    let mut offset = 0usize;
    let mut newest = None;
    let mut newest_service_interval = None;
    let mut conflicting_report = false;
    // Tenor's bounded correction defines its bus units; inference must not
    // reinterpret a report rejected by that correction as a different timebase.
    let detecting = !per_service
        && compatibility.tenor_nominal_q16.is_none()
        && counters.selected_timebase.is_none()
        && counters.infer_service_interval
        && speed == Speed::High
        && service_us > 125;
    let inferred_service_interval =
        !per_service && counters.selected_timebase == Some(FeedbackTimebase::OutService);
    for (index, (status, requested, actual)) in packets.into_iter().enumerate() {
        let start = offset;
        offset = offset.saturating_add(requested);
        // Only successful transfer completions consume startup bus opportunities.
        // Failed/empty packet descriptors within such a transfer still count.
        let skip = transfer_status == LIBUSB_TRANSFER_COMPLETED && compatibility.skip_packet();
        let report = (actual <= requested && offset <= buffer.len())
            .then(|| buffer.get(start..start.saturating_add(actual)))
            .flatten();
        let mut decoded = None;
        let outcome = if transfer_status != LIBUSB_TRANSFER_COMPLETED {
            Outcome::TransferFailed
        } else if status != LIBUSB_TRANSFER_COMPLETED {
            Outcome::PacketFailed
        } else if actual == 0 {
            Outcome::Empty
        } else if let Some(bytes) = report.filter(|bytes| {
            if requested == 8 && matches!(speed, Speed::Super | Speed::SuperPlus) {
                bytes.len() == 8
            } else {
                matches!(bytes.len(), 3 | 4)
            }
        }) {
            // ALSA's feedback-format autodetection (endpoint.c:1889-1924) for
            // four-byte bus-timebase reports at full and high speed. Tenor
            // correction and the confirmed OUT-service timebase keep their own
            // units and are never re-shifted.
            let shift_eligible = bytes.len() == 4
                && !per_service
                && !inferred_service_interval
                && compatibility.tenor_nominal_q16.is_none()
                && matches!(speed, Speed::Full | Speed::High);
            let locked_shift = counters.feedback_shift.filter(|_| shift_eligible);
            if !inferred_service_interval || matches!(bytes.len(), 4 | 8) {
                decoded = match compatibility.tenor_nominal_q16 {
                    Some(nominal)
                        if !per_service
                            && !inferred_service_interval
                            && speed == Speed::High
                            && bytes.len() == 4 =>
                    {
                        let raw = u32::from_le_bytes(bytes.try_into().unwrap());
                        correct_tenor_feedback(raw, nominal).and_then(|corrected| {
                            decode_feedback_report_with_scale(
                                &corrected.to_le_bytes(),
                                service_us,
                                speed,
                                false,
                            )
                        })
                    }
                    _ => decode_feedback_report_with_scale(
                        bytes,
                        service_us,
                        speed,
                        per_service || inferred_service_interval,
                    )
                    .map(|native| shift_feedback(native, locked_shift.unwrap_or(0))),
                };
            }
            // Discard a selected timebase only when this report positively fits
            // the alternative. An isolated corrupt report must not reset lock.
            if !per_service
                && compatibility.tenor_nominal_q16.is_none()
                && counters.infer_service_interval
                && counters.selected_timebase.is_some()
                && decoded.is_some_and(|(frames, _)| !scheduler.feedback_rate_is_plausible(frames))
            {
                let alternate = decode_feedback_report_with_scale(
                    bytes,
                    service_us,
                    speed,
                    !inferred_service_interval,
                )
                .filter(|(frames, _)| {
                    scheduler.feedback_rate_is_plausible(*frames)
                        && frames.ceil() <= max_frames as f64
                });
                if alternate.is_some() {
                    counters.selected_timebase = None;
                    counters.service_interval_confirmations = 0;
                    counters.cached_service_interval = false;
                    counters.redetecting = true;
                    decoded = if inferred_service_interval {
                        alternate
                    } else {
                        decode_feedback_report_with_scale(bytes, service_us, speed, false)
                    };
                }
            }
            // Startup reports cannot select units, acquire lock or renew its deadline.
            // Retain ordinary malformed/empty/failed diagnostics below.
            if skip && decoded.is_some() {
                continue;
            }
            match decoded {
                None => Outcome::InvalidEncoding,
                Some((frames, rate)) if !scheduler.feedback_rate_is_plausible(frames) => {
                    let compatible = ((detecting
                        || (counters.selected_timebase.is_none()
                            && counters.infer_service_interval
                            && !per_service
                            && speed == Speed::High
                            && service_us > 125
                            && compatibility.tenor_nominal_q16.is_none()))
                        && bytes.len() == 4)
                        .then(|| decode_feedback_report_with_scale(bytes, service_us, speed, true))
                        .flatten()
                        .filter(|(frames, _)| {
                            scheduler.feedback_rate_is_plausible(*frames)
                                && frames.ceil() <= max_frames as f64
                        });
                    // First report: adopt the shift ALSA's search finds only
                    // when it is the padded-Q10.14 one. A per-frame high-speed
                    // scale (shift -3) belongs to the confirmed timebase
                    // inference above; other shifts name no defined encoding,
                    // so one corrupt report cannot rescale the clock.
                    let padded = (shift_eligible && locked_shift.is_none())
                        .then(|| alsa_feedback_shift(frames, scheduler.nominal_frames_per_packet()))
                        .flatten()
                        .filter(|&shift| shift == PADDED_Q10_14_SHIFT)
                        .map(|shift| shift_feedback((frames, rate), shift))
                        .filter(|(frames, _)| {
                            scheduler.feedback_rate_is_plausible(*frames)
                                && frames.ceil() <= max_frames as f64
                        });
                    if let Some(rate) = compatible {
                        newest_service_interval = Some((rate, bytes));
                        decoded = Some(rate);
                        Outcome::TimebaseCandidate
                    } else if let Some(shifted) = padded {
                        counters.feedback_shift = Some(PADDED_Q10_14_SHIFT);
                        decoded = Some(shifted);
                        newest = decoded;
                        counters.record_valid_bytes(bytes);
                        Outcome::Valid
                    } else {
                        if shift_eligible {
                            // endpoint.c:1918-1923: out of range, maybe the
                            // shift is wrong; detect again on the next report.
                            counters.feedback_shift = None;
                        }
                        conflicting_report = true;
                        Outcome::RateOutOfRange
                    }
                }
                Some((frames, _)) if frames.ceil() > max_frames as f64 => {
                    if shift_eligible {
                        counters.feedback_shift = None;
                    }
                    Outcome::PacketCapacityExceeded
                }
                Some(_) => {
                    if shift_eligible && locked_shift.is_none() {
                        counters.feedback_shift = Some(0);
                    }
                    newest = decoded;
                    counters.record_valid_bytes(bytes);
                    Outcome::Valid
                }
            }
        } else {
            Outcome::InvalidLength
        };
        match outcome {
            Outcome::Valid | Outcome::TransferFailed | Outcome::TimebaseCandidate => {}
            Outcome::Empty => counters.empty_polls = counters.empty_polls.saturating_add(1),
            Outcome::PacketFailed => counters.errors = counters.errors.saturating_add(1),
            Outcome::InvalidLength | Outcome::InvalidEncoding => {
                counters.malformed = counters.malformed.saturating_add(1);
                counters.rejected = counters.rejected.saturating_add(1);
            }
            Outcome::RateOutOfRange | Outcome::PacketCapacityExceeded => {
                counters.rate_rejected = counters.rate_rejected.saturating_add(1);
                counters.rejected = counters.rejected.saturating_add(1);
            }
        }
        if counters.diagnostics.is_some() || counters.trace.is_some() {
            let packet_status = (is_iso && status != NO_FEEDBACK_PACKET_STATUS).then_some(status);
            let raw = if counters.trace.is_some() {
                // Copy only bytes inside this packet's submitted span. A
                // failed transfer can still report a partial IN packet.
                buffer
                    .get(start..offset)
                    .map(|span| &span[..actual.min(span.len())])
                    .unwrap_or_default()
            } else {
                &[]
            };
            let sample = crate::ExplicitFeedbackPacket::new(
                outcome,
                transfer_status,
                packet_status,
                requested,
                actual,
                raw,
                decoded,
            );
            if let Some(diagnostics) = counters.diagnostics.as_mut() {
                diagnostics.record_packet(sample, is_iso);
            }
            if let Some(trace) = &counters.trace {
                trace.record(crate::FeedbackTraceEvent::ExplicitPacket { index, sample });
            }
        }
    }
    if counters.selected_timebase.is_none() {
        if newest.is_some() {
            counters.redetecting = false;
            // A conforming report always wins, even if an earlier packet in
            // the same completion resembled a different timebase.
            let unit = if per_service && speed == Speed::High && counters.last_report_len == 4 {
                FeedbackTimebase::OutService
            } else {
                FeedbackTimebase::Bus
            };
            counters.select_timebase(unit, speed, false);
        } else if let Some((decoded, bytes)) =
            newest_service_interval.filter(|_| !conflicting_report)
        {
            // Count once per successful completion. A batch containing many
            // identical packets is only one observation. Empty polls do not
            // count or extend the existing feedback deadline.
            counters.service_interval_confirmations += 1;
            if counters.cached_service_interval
                || counters.service_interval_confirmations == SERVICE_INTERVAL_CONFIRMATIONS
            {
                counters.record_valid_bytes(bytes);
                counters.select_timebase(FeedbackTimebase::OutService, speed, true);
                counters.redetecting = false;
                newest = Some(decoded);
            }
        } else if conflicting_report {
            counters.service_interval_confirmations = 0;
            counters.cached_service_interval = false;
        }
    }
    if let (Some((frames, _)), Some(diagnostics)) = (newest, counters.diagnostics.as_mut()) {
        diagnostics.rate_samples = diagnostics.rate_samples.saturating_add(1);
        let n = diagnostics.rate_samples as f64;
        if diagnostics.rate_samples == 1 {
            diagnostics.rate_min_frames = frames;
            diagnostics.rate_max_frames = frames;
            diagnostics.rate_mean_frames = frames;
        } else {
            diagnostics.rate_min_frames = diagnostics.rate_min_frames.min(frames);
            diagnostics.rate_max_frames = diagnostics.rate_max_frames.max(frames);
            let delta = frames - diagnostics.rate_mean_frames;
            diagnostics.rate_mean_frames += delta / n;
            diagnostics.rate_variance_frames = (diagnostics.rate_variance_frames * (n - 1.0)
                + delta * (frames - diagnostics.rate_mean_frames))
                / n;
        }
        counters.diagnostics_dirty = true;
    }
    newest
}

#[derive(Debug)]
pub(super) enum FeedbackFault {
    DeviceLost,
    Transfer(String),
}

/// A selected explicit pipe cannot silently become nominal on first-submit failure.
/// Runtime transient submissions may retry only while the health deadline remains live.
pub(super) fn feedback_submit_failure(
    error: String,
    no_device: bool,
    first: bool,
) -> Option<FeedbackFault> {
    if no_device {
        Some(FeedbackFault::DeviceLost)
    } else if first {
        Some(FeedbackFault::Transfer(format!(
            "explicit feedback initial submit failed: {}",
            error
        )))
    } else {
        None
    }
}

fn feedback_completion_fault(status: i32, stopping: bool, endpoint: u8) -> Option<FeedbackFault> {
    match status {
        LIBUSB_TRANSFER_NO_DEVICE => Some(FeedbackFault::DeviceLost),
        LIBUSB_TRANSFER_CANCELLED if !stopping => Some(FeedbackFault::Transfer(
            "explicit feedback unexpectedly cancelled".into(),
        )),
        LIBUSB_TRANSFER_STALL => Some(FeedbackFault::Transfer(format!(
            "explicit feedback endpoint 0x{:02x} stalled",
            endpoint
        ))),
        _ => None,
    }
}

pub(super) fn pump_feedback_transfers(
    handle: &DeviceHandle<Context>,
    transfers: &mut FeedbackTransfers,
    scheduler: &mut IsoPacketScheduler,
    data_service_interval_us: u32,
    speed: Speed,
    max_frames: usize,
    per_service: bool,
    counters: &mut FeedbackCounters,
    stop: &AtomicBool,
    health: ExplicitFeedbackHealth,
    started_at: Instant,
) -> Result<(), FeedbackFault> {
    // Finished batches belong to earlier USB intervals even if libusb hands
    // a later slot back first. Process them in submission order.
    while let Some((index, status, completed_at, actual_length)) = transfers.take_next_completion()
    {
        let Some(completed_at) = completed_at else {
            return Err(FeedbackFault::Transfer(
                "feedback callback timestamp missing".into(),
            ));
        };
        let slot = &mut transfers.slots[index];
        if let Some(diagnostics) = counters.diagnostics.as_mut() {
            diagnostics.record_transfer(status);
            counters.diagnostics_dirty = true;
        }
        if let Some(trace) = &counters.trace {
            trace.record_at(
                completed_at,
                crate::FeedbackTraceEvent::Completion {
                    slot: Some(index),
                    status,
                    actual_bytes: actual_length,
                },
            );
        }
        let report = slot.newest_valid_report(
            scheduler,
            data_service_interval_us,
            speed,
            max_frames,
            per_service,
            counters,
            actual_length,
            status,
            &mut transfers.compatibility,
        );
        if let Some(fault) =
            feedback_completion_fault(status, usb_stop_requested(stop), slot.endpoint.address)
        {
            return Err(fault);
        }
        match status {
            LIBUSB_TRANSFER_COMPLETED => {
                if let Some((frames, rate)) = report {
                    // A report delivered beyond the old lease cannot restart it.
                    // Use callback time: a timely report processed slightly late is valid.
                    if !health.report_is_timely(completed_at, started_at, counters.last_report_at) {
                        return Err(FeedbackFault::Transfer(
                            "explicit feedback report arrived after its terminal deadline".into(),
                        ));
                    }
                    if scheduler.update_feedback_frames_per_packet(frames) {
                        if let Some(trace) = &counters.trace {
                            if let Some(event) =
                                applied_feedback_trace(scheduler, frames, data_service_interval_us)
                            {
                                trace.record(event);
                            }
                        }
                        counters.reports = counters.reports.saturating_add(1);
                        // Completion callbacks can run out of order. Never
                        // regress the last-report timestamp in that case.
                        counters.last_report_at = Some(
                            counters
                                .last_report_at
                                .map_or(completed_at, |last| last.max(completed_at)),
                        );
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
            LIBUSB_TRANSFER_TIMED_OUT => {
                counters.timeouts = counters.timeouts.saturating_add(1);
            }
            LIBUSB_TRANSFER_CANCELLED if usb_stop_requested(stop) => return Ok(()),
            LIBUSB_TRANSFER_OVERFLOW => {
                counters.rejected = counters.rejected.saturating_add(1);
            }
            _ => {
                counters.errors = counters.errors.saturating_add(1);
            }
        }
        // Retry transient completions with a small bounded delay, not a busy resubmit loop.
        if status != LIBUSB_TRANSFER_COMPLETED {
            slot.retry_at = Instant::now() + Duration::from_millis(5);
        }
        if !usb_stop_requested(stop) && Instant::now() >= slot.retry_at {
            transfers.submit(index, handle, counters, false)?;
        }
    }
    for index in 0..transfers.slots.len() {
        let slot = &transfers.slots[index];
        if !usb_stop_requested(stop) && !slot.in_flight && Instant::now() >= slot.retry_at {
            transfers.submit(index, handle, counters, false)?;
        }
    }
    Ok(())
}

/// Deadlines are measured from the first active transfer or the last actual rate.
/// Empty successful polls and transport retries do not refresh that epoch.
#[derive(Debug, Clone, Copy)]
pub(super) struct ExplicitFeedbackHealth {
    pub(super) warning: Duration,
    pub(super) terminal: Duration,
}
impl ExplicitFeedbackHealth {
    pub(super) fn new(poll_us: u32, refresh: Option<u8>) -> Self {
        // UAC1 bRefresh is log2 milliseconds, valid in the range 1..=9.
        let update_us = refresh
            .filter(|r| (1..=9).contains(r))
            .map(|r| (1u64 << r) * 1000)
            .unwrap_or(u64::from(poll_us));
        let cadence = Duration::from_micros(update_us.max(u64::from(poll_us)));
        Self {
            warning: Duration::from_millis(USB_FEEDBACK_STALE_WARN_MS).max(cadence * 3),
            terminal: Duration::from_secs(1).max(cadence * 6),
        }
    }
    fn report_is_timely(
        &self,
        completed: Instant,
        started: Instant,
        previous: Option<Instant>,
    ) -> bool {
        completed.saturating_duration_since(previous.unwrap_or(started)) < self.terminal
    }
    pub(super) fn state(
        &self,
        now: Instant,
        started: Instant,
        last: Option<Instant>,
    ) -> (bool, bool, bool) {
        let age = now.saturating_duration_since(last.unwrap_or(started));
        (
            last.is_some() && age < self.warning,
            age >= self.warning,
            age >= self.terminal,
        )
    }
}

/// Record the applied fractional scheduler rate separately from the selected report.
fn applied_feedback_trace(
    scheduler: &IsoPacketScheduler,
    reported: f64,
    service_us: u32,
) -> Option<crate::FeedbackTraceEvent> {
    if service_us == 0 {
        return None;
    }
    let applied = scheduler.applied_feedback_frames_per_packet()?;
    Some(crate::FeedbackTraceEvent::ExplicitFeedbackRate {
        smoothing: scheduler.explicit_feedback_smoothing(),
        reported_frames_per_out_service: reported,
        applied_frames_per_out_service: applied,
        applied_rate_hz: applied * 1_000_000.0 / f64::from(service_us),
    })
}

/// Decode UAC feedback into frames per data-endpoint service interval.
///
/// UAC2 feedback reports samples per microframe on high-speed links and
/// samples per frame on full-speed links. Enhanced SuperSpeed reports unsigned
/// Q32.32 samples per 125 us bus interval (USB 3.2 section 4.4.8.4.1).
/// The feedback endpoint's own polling
/// interval is not the data endpoint's packet interval; using it as the
/// timebase previously produced a rate error of up to 8x at 125 us service.
#[cfg(any(test, feature = "fuzzing"))]
pub(super) fn decode_feedback_report(
    raw_bytes: &[u8],
    data_service_interval_us: u32,
    speed: Speed,
) -> Option<(f64, f64)> {
    decode_feedback_report_with_scale(raw_bytes, data_service_interval_us, speed, false)
}

fn decode_feedback_report_with_scale(
    raw_bytes: &[u8],
    data_service_interval_us: u32,
    speed: Speed,
    per_service: bool,
) -> Option<(f64, f64)> {
    if data_service_interval_us == 0 {
        return None;
    }
    let high_speed = matches!(speed, Speed::High | Speed::Super | Speed::SuperPlus);
    if !matches!(
        speed,
        Speed::Full | Speed::High | Speed::Super | Speed::SuperPlus
    ) {
        return None;
    }
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
            if high_speed && raw & 0xf000_0000 != 0 {
                return None;
            }
            let base_interval_us = if per_service {
                f64::from(data_service_interval_us)
            } else {
                feedback_base_interval_us(speed, data_service_interval_us)
            };
            (raw as f64 / 65_536.0, base_interval_us)
        }
        8 if matches!(speed, Speed::Super | Speed::SuperPlus) => {
            let raw = u64::from_le_bytes(raw_bytes.try_into().ok()?);
            (raw as f64 / 4_294_967_296.0, 125.0)
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

// Publication happens in Drop even if optional completion metadata panics. This
// guard is created only after the callback's final transfer/user_data access.
struct ReturnPublication {
    completion: Arc<IsoTransferCompletion>,
    generation: u64,
}
impl Drop for ReturnPublication {
    fn drop(&mut self) {
        if self.completion.generation.load(Ordering::Acquire) == self.generation {
            self.completion
                .returned
                .store(self.generation, Ordering::Release);
        }
    }
}

pub(super) extern "system" fn iso_transfer_callback(transfer: *mut libusb_transfer) {
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let Some(transfer) = std::ptr::NonNull::new(transfer) else {
            return;
        };
        // SAFETY: libusb invokes this only for a submitted transfer. Its slot retains
        // both the transfer and user_data until the return publication below.
        let user_data_ptr = unsafe { transfer.as_ref().user_data as *mut IsoTransferUserData };
        let Some(user_data) = std::ptr::NonNull::new(user_data_ptr) else {
            return;
        };
        let status = unsafe { transfer.as_ref().status };
        let actual_length = unsafe { transfer.as_ref().actual_length }.max(0) as usize;
        let completion = unsafe { user_data.as_ref() }.completion.clone();
        let generation = completion.generation.load(Ordering::Acquire);
        // No transfer or user_data access is permitted beyond this point.
        let publication = ReturnPublication {
            completion,
            generation,
        };
        let completion = &publication.completion;
        completion
            .actual_length
            .store(actual_length, Ordering::Relaxed);
        // Install a usable result before optional timestamp collection.
        *completion.status.lock().unwrap_or_else(|p| p.into_inner()) = Some(status);
        if let Ok(now) = std::panic::catch_unwind(Instant::now) {
            *completion
                .completed_at
                .lock()
                .unwrap_or_else(|p| p.into_inner()) = Some(now);
        }
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

    fn q16(frames: f64) -> [u8; 4] {
        ((frames * 65536.0).round() as u32).to_le_bytes()
    }
    fn q32(frames: f64) -> [u8; 8] {
        ((frames * 4_294_967_296.0).round() as u64).to_le_bytes()
    }
    fn scheduler(service_us: u32) -> IsoPacketScheduler {
        IsoPacketScheduler::new(48_000, 8, service_us)
    }

    #[test]
    fn swinging_feedback_keeps_capacity_guard_and_reports_unsmoothed_variance() {
        let scheduler = scheduler(125);
        let mut counters = FeedbackCounters {
            diagnostics: Some(Default::default()),
            ..Default::default()
        };
        for value in [6.0, 5.4, 6.6, 6.0] {
            assert!(select_feedback_report(
                &q16(value),
                [(0, 4, 4)],
                &scheduler,
                125,
                Speed::High,
                8,
                false,
                &mut counters
            )
            .is_some());
        }
        let stats = counters.diagnostics.unwrap();
        assert_eq!(stats.rate_samples, 4);
        assert!((stats.rate_min_frames - 5.4).abs() < 0.0001);
        assert!((stats.rate_max_frames - 6.6).abs() < 0.0001);
        assert!((stats.rate_mean_frames - 6.0).abs() < 0.0001);
        assert!((stats.rate_variance_frames - 0.18).abs() < 0.0001);
        assert!(select_feedback_report(
            &q16(9.0),
            [(0, 4, 4)],
            &scheduler,
            125,
            Speed::High,
            8,
            false,
            &mut counters
        )
        .is_none());
    }

    #[test]
    fn stale_inferred_units_are_redetected_against_proven_clock() {
        let scheduler = scheduler(1000);
        let mut counters = FeedbackCounters {
            infer_service_interval: true,
            ..Default::default()
        };
        for _ in 0..3 {
            select_feedback_report(
                &q16(48.0),
                [(0, 4, 4)],
                &scheduler,
                1000,
                Speed::High,
                64,
                false,
                &mut counters,
            );
        }
        assert!(counters.selected_service_interval());
        // Corrupt reports do not discard the locked service timebase.
        assert!(select_feedback_report(
            &q16(999.0),
            [(0, 4, 4)],
            &scheduler,
            1000,
            Speed::High,
            64,
            false,
            &mut counters
        )
        .is_none());
        assert!(counters.selected_service_interval());
        assert!(select_feedback_report(
            &q16(48.0),
            [(0, 4, 4)],
            &scheduler,
            1000,
            Speed::High,
            64,
            false,
            &mut counters
        )
        .is_some());
        // A valid standard report proves a change of units immediately.
        assert!(select_feedback_report(
            &q16(6.0),
            [(0, 4, 4)],
            &scheduler,
            1000,
            Speed::High,
            64,
            false,
            &mut counters
        )
        .is_some());
        assert!(!counters.selected_service_interval());
    }

    #[test]
    fn enhanced_superspeed_feedback_uses_q32_bus_units_at_every_output_interval() {
        for speed in [Speed::Super, Speed::SuperPlus] {
            for rate in [48_000, 44_100] {
                let report = q32(f64::from(rate) / 8000.0);
                for service_us in [125, 250, 1000] {
                    let scheduler = IsoPacketScheduler::new(rate, 8, service_us);
                    let mut counters = FeedbackCounters {
                        infer_service_interval: true,
                        ..FeedbackCounters::default()
                    };
                    let (frames, decoded_rate) = select_feedback_report(
                        &report,
                        [(0, 8, 8)],
                        &scheduler,
                        service_us,
                        speed,
                        128,
                        true,
                        &mut counters,
                    )
                    .unwrap();
                    assert!((decoded_rate - f64::from(rate)).abs() < 0.00001);
                    let expected_frames = f64::from(rate) * f64::from(service_us) / 1_000_000.0;
                    assert!((frames - expected_frames).abs() < 0.00001);
                    assert_eq!(counters.last_report_bytes, report);
                    assert_eq!(counters.last_report_len, 8);
                    assert_eq!(counters.selected_timebase, Some(FeedbackTimebase::Bus));
                    assert!(!counters.selected_service_interval());
                }
            }
        }
    }

    fn padded_q10_14(frames: f64) -> [u8; 4] {
        ((frames * 16_384.0).round() as u32).to_le_bytes()
    }

    #[test]
    fn padded_q10_14_feedback_is_autodetected_once_and_locked() {
        // USB 2.0 section 5.12.4.2: Q10.14 in 3 bytes at full speed, Q16.16 in
        // 4 bytes at high speed. Some devices pad Q10.14 into 4 bytes; ALSA's
        // snd_usb_handle_sync_urb finds the shift (+2) on the first report.
        for (rate, service, speed, frames) in [
            (44_100, 1000, Speed::Full, 44.1),
            (48_000, 125, Speed::High, 6.0),
        ] {
            let scheduler = IsoPacketScheduler::new(rate, 8, service);
            let mut counters = FeedbackCounters::default();
            let report = |bytes: &[u8], counters: &mut FeedbackCounters| {
                select_feedback_report(
                    bytes,
                    [(0, 4, 4)],
                    &scheduler,
                    service,
                    speed,
                    64,
                    false,
                    counters,
                )
            };
            for _ in 0..3 {
                let (decoded, estimated) = report(&padded_q10_14(frames), &mut counters).unwrap();
                assert!((decoded - frames).abs() <= 0.5 / 16_384.0, "{decoded}");
                assert!((estimated - f64::from(rate)).abs() < 0.5, "{estimated}");
            }
            assert_eq!(counters.feedback_shift, Some(PADDED_Q10_14_SHIFT));
            assert_eq!(counters.rate_rejected, 0);
            // A report the locked shift puts out of the window is rejected and
            // clears the lock (endpoint.c:1918-1923); the next one redetects.
            assert!(report(&q16(frames), &mut counters).is_none());
            assert_eq!(counters.rate_rejected, 1);
            assert_eq!(counters.feedback_shift, None);
            let (native, _) = report(&q16(frames), &mut counters).unwrap();
            assert!((native - frames).abs() <= 0.5 / 65_536.0, "{native}");
            assert_eq!(counters.feedback_shift, Some(0));
            // Locked native: a padded report is rejected first, as in ALSA.
            assert!(report(&padded_q10_14(frames), &mut counters).is_none());
            assert_eq!(counters.feedback_shift, None);
            assert!(report(&padded_q10_14(frames), &mut counters).is_some());
        }
    }

    #[test]
    fn feedback_shift_detection_keeps_native_formats_and_rejects_unplaceable_values() {
        let scheduler = scheduler(125);
        let native = |bytes: &[u8]| {
            let mut counters = FeedbackCounters::default();
            let decoded = select_feedback_report(
                bytes,
                [(0, bytes.len(), bytes.len())],
                &scheduler,
                125,
                Speed::High,
                16,
                false,
                &mut counters,
            );
            (decoded, counters)
        };
        // Three-byte Q10.14 and four-byte Q16.16 keep their exact decode.
        let (decoded, counters) = native(&[0, 0x80, 1]);
        assert_eq!(decoded, Some((6.0, 48_000.0)));
        assert_eq!(counters.feedback_shift, None);
        let (decoded, counters) = native(&q16(6.0));
        assert_eq!(decoded, Some((6.0, 48_000.0)));
        assert_eq!(counters.feedback_shift, Some(0));
        // 0.8 x nominal: every power-of-two shift lands outside 7/8..3/2, as
        // does its padded form (1.2 frames shifted +2 is 4.8).
        for bytes in [q16(4.8), q16(1.2), padded_q10_14(1.2)] {
            let (decoded, counters) = native(&bytes);
            assert!(decoded.is_none());
            assert_eq!(counters.rate_rejected, 1);
            assert_eq!(counters.feedback_shift, None);
        }
        assert_eq!(alsa_feedback_shift(1.5, 6.0), Some(2));
        assert_eq!(alsa_feedback_shift(48.0, 6.0), Some(-3));
        assert_eq!(alsa_feedback_shift(4.8, 6.0), Some(0));
        assert_eq!(alsa_feedback_shift(0.0, 6.0), None);
    }

    #[test]
    fn enhanced_superspeed_keeps_valid_report_before_newer_short_report() {
        let report = q32(6.0);
        let mut batch = report.to_vec();
        batch.extend_from_slice(&q16(6.0));
        batch.extend_from_slice(&[0; 4]);
        let mut counters = FeedbackCounters::default();
        assert_eq!(
            select_feedback_report(
                &batch,
                [(0, 8, 8), (0, 8, 4)],
                &scheduler(250),
                250,
                Speed::Super,
                16,
                false,
                &mut counters
            ),
            Some((12.0, 48_000.0))
        );
        assert_eq!(counters.last_report_bytes, report);
        assert_eq!(counters.last_report_len, 8);
        assert_eq!(counters.malformed, 1);
        assert_eq!(counters.rejected, 1);
    }

    #[test]
    fn enhanced_superspeed_feedback_retains_capacity_and_rate_rejection() {
        for (frames, max_frames) in [(6.0, 47), (48.0, 1024)] {
            let mut counters = FeedbackCounters::default();
            assert!(select_feedback_report(
                &q32(frames),
                [(0, 8, 8)],
                &scheduler(1000),
                1000,
                Speed::SuperPlus,
                max_frames,
                false,
                &mut counters
            )
            .is_none());
            assert_eq!(counters.rate_rejected, 1);
            assert_eq!(counters.rejected, 1);
            assert_eq!(counters.last_report_len, 0);
            assert!(counters.last_report_at.is_none());
            assert!(counters.selected_timebase.is_none());
        }
    }

    #[test]
    fn eight_byte_feedback_requires_superspeed_and_exact_selected_request_span() {
        let report = q32(6.0);
        for speed in [Speed::Low, Speed::Full, Speed::High, Speed::Unknown] {
            assert!(decode_feedback_report(&report, 125, speed).is_none());
            let mut counters = FeedbackCounters::default();
            assert!(select_feedback_report(
                &report,
                [(0, 8, 8)],
                &scheduler(125),
                125,
                speed,
                16,
                false,
                &mut counters
            )
            .is_none());
            assert_eq!(counters.malformed, 1);
        }
        for speed in [Speed::Super, Speed::SuperPlus] {
            for actual in [3, 4, 7] {
                let mut counters = FeedbackCounters::default();
                assert!(select_feedback_report(
                    &report,
                    [(0, 8, actual)],
                    &scheduler(125),
                    125,
                    speed,
                    16,
                    false,
                    &mut counters
                )
                .is_none());
                assert_eq!(counters.malformed, 1);
            }
            let mut counters = FeedbackCounters::default();
            assert!(select_feedback_report(
                &report.repeat(2),
                [(0, 16, 8)],
                &scheduler(125),
                125,
                speed,
                16,
                false,
                &mut counters
            )
            .is_none());
            assert_eq!(
                select_feedback_report(
                    &q16(6.0),
                    [(0, 4, 4)],
                    &scheduler(125),
                    125,
                    speed,
                    16,
                    false,
                    &mut counters
                ),
                Some((6.0, 48_000.0))
            );
        }
    }

    fn feedback_pipe() -> FeedbackEndpoint {
        FeedbackEndpoint {
            refresh: 0,
            address: 0x81,
            transfer_type: TransferType::Isochronous,
            interval: 1,
            service_interval_us: 125,
            max_packet_bytes: 4,
        }
    }

    #[test]
    fn physical_pcm_and_three_native_dsd_vectors_use_proven_universal_timebase() {
        // AP80 Pro-X physical captures: PCM44.1, native DSD64/128/256.
        // Raw DSD has an eight-byte wire frame; PCM16 stereo has four.
        for (nominal_rate, stride, raw, expected_rate) in [
            (44_100, 4, [0x16, 0x09, 0x0b, 0x00], 44_141.9677734375),
            (88_200, 8, [0xa2, 0x10, 0x16, 0x00], 88_259.8876953125),
            (176_400, 8, [0x2e, 0x1d, 0x2c, 0x00], 176_455.9326171875),
            (352_800, 8, [0x0a, 0x37, 0x58, 0x00], 352_859.9853515625),
        ] {
            let scheduler = IsoPacketScheduler::new(nominal_rate, stride, 250);
            let max_frames = 1024 / stride;
            let mut unproven = FeedbackCounters::default();
            for _ in 0..4 {
                assert!(select_feedback_report(
                    &raw,
                    [(0, 4, 4)],
                    &scheduler,
                    250,
                    Speed::High,
                    max_frames,
                    false,
                    &mut unproven
                )
                .is_none());
            }
            assert!(unproven.selected_timebase.is_none());
            let mut proven = FeedbackCounters {
                infer_service_interval: true,
                ..Default::default()
            };
            for _ in 0..2 {
                assert!(select_feedback_report(
                    &raw,
                    [(0, 4, 4)],
                    &scheduler,
                    250,
                    Speed::High,
                    max_frames,
                    false,
                    &mut proven
                )
                .is_none());
            }
            let selected = select_feedback_report(
                &raw,
                [(0, 4, 4)],
                &scheduler,
                250,
                Speed::High,
                max_frames,
                false,
                &mut proven,
            )
            .unwrap();
            assert!((selected.1 - expected_rate).abs() < 0.000001);
            assert!((selected.0 - expected_rate / 4000.0).abs() < 0.000001);
            assert!(proven.selected_service_interval());
            // Saved units never waive the fresh stream's capacity guard.
            let mut capacity_limited = FeedbackCounters {
                infer_service_interval: true,
                cached_service_interval: true,
                ..Default::default()
            };
            assert!(select_feedback_report(
                &raw,
                [(0, 4, 4)],
                &scheduler,
                250,
                Speed::High,
                1,
                false,
                &mut capacity_limited
            )
            .is_none());
            assert!(!capacity_limited.selected_service_interval());
        }
    }

    fn compatible_report(
        report: &[u8],
        service_us: u32,
        max_frames: usize,
        per_service: bool,
        compatibility: &mut FeedbackCompatibility,
    ) -> Option<(f64, f64)> {
        select_feedback_report_with_compatibility(
            report,
            [(0, report.len(), report.len())],
            &scheduler(service_us),
            service_us,
            Speed::High,
            max_frames,
            per_service,
            &mut FeedbackCounters::default(),
            compatibility,
        )
    }

    #[test]
    fn shared_release_keeps_device_specific_feedback_profiles_inactive() {
        for (vendor, product) in [(0x0644, 0x8038), (0x1852, 0x5034), (0x23ba, 1)] {
            let mut compatibility = FeedbackCompatibility::for_device(
                vendor,
                product,
                Speed::High,
                feedback_pipe(),
                48_000,
            );
            assert!(compatibility.tenor_nominal_q16.is_none());
            assert_eq!(compatibility.startup_packets_remaining, 0);
            assert_eq!(
                compatible_report(&q16(6.0), 125, 7, false, &mut compatibility),
                Some((6.0, 48_000.0))
            );
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
        let mut compatibility = FeedbackCompatibility {
            tenor_nominal_q16: Some(0x60000),
            startup_packets_remaining: 0,
        };
        for raw in [0, 1, 0xf0060000, 0x80000] {
            assert!(
                compatible_report(&u32::to_le_bytes(raw), 125, 7, false, &mut compatibility)
                    .is_none()
            );
        }
        // Corrected values still need fractional-ceiling capacity and nominal-rate checks.
        let report = q16(6.01 + 0.9375);
        assert!(compatible_report(&report, 1000, 48, false, &mut compatibility).is_none());
        assert!(compatible_report(&report, 1000, 49, false, &mut compatibility).is_some());
        assert!(compatible_report(&q16(11.0), 125, 16, false, &mut compatibility).is_none());
        // The explicit user scale wins and is never followed by the Tenor subtraction.
        assert_eq!(
            compatible_report(&q16(48.0), 1000, 49, true, &mut compatibility),
            Some((48.0, 48_000.0))
        );
        // A three-byte report keeps the existing Q10.14 interpretation.
        assert_eq!(
            compatible_report(&[0, 0x80, 1], 125, 7, false, &mut compatibility),
            Some((6.0, 48_000.0))
        );
    }

    #[test]
    fn universal_inference_cannot_bypass_tenor_correction_or_playback_startup() {
        let mut tenor = FeedbackCompatibility {
            tenor_nominal_q16: Some(0x60000),
            startup_packets_remaining: 0,
        };
        let mut counters = FeedbackCounters {
            infer_service_interval: true,
            cached_service_interval: true,
            ..Default::default()
        };
        // Twelve raw frames would fit OUT-service units but violate the
        // Tenor bus-unit correction and must never acquire an inferred lock.
        assert!(select_feedback_report_with_compatibility(
            &q16(12.0),
            [(0, 4, 4)],
            &scheduler(250),
            250,
            Speed::High,
            128,
            false,
            &mut counters,
            &mut tenor
        )
        .is_none());
        assert!(counters.selected_timebase.is_none());

        let mut playback = FeedbackCompatibility {
            tenor_nominal_q16: None,
            startup_packets_remaining: 4,
        };
        let mut counters = FeedbackCounters {
            infer_service_interval: true,
            ..Default::default()
        };
        let report = q16(12.0);
        assert!(select_feedback_report_with_compatibility(
            &report.repeat(4),
            [(0, 4, 4); 4],
            &scheduler(250),
            250,
            Speed::High,
            128,
            false,
            &mut counters,
            &mut playback
        )
        .is_none());
        assert_eq!(counters.service_interval_confirmations, 0);
        assert!(counters.selected_timebase.is_none());
        for _ in 0..2 {
            assert!(select_feedback_report_with_compatibility(
                &report,
                [(0, 4, 4)],
                &scheduler(250),
                250,
                Speed::High,
                128,
                false,
                &mut counters,
                &mut playback
            )
            .is_none());
        }
        assert_eq!(
            select_feedback_report_with_compatibility(
                &report,
                [(0, 4, 4)],
                &scheduler(250),
                250,
                Speed::High,
                128,
                false,
                &mut counters,
                &mut playback
            ),
            Some((12.0, 48_000.0))
        );
    }

    #[test]
    fn playback_designs_skips_four_completed_bus_opportunities_across_batches() {
        let mut compatibility = FeedbackCompatibility {
            tenor_nominal_q16: None,
            startup_packets_remaining: 4,
        };
        let mut counters = FeedbackCounters::default();
        let prior_lock = Instant::now();
        counters.last_report_at = Some(prior_lock);
        let report = q16(6.0);
        assert!(select_feedback_report_with_compatibility(
            &report.repeat(2),
            [(0, 4, 4); 2],
            &scheduler(125),
            125,
            Speed::High,
            7,
            false,
            &mut counters,
            &mut compatibility
        )
        .is_none());
        // Failed transfers are inspected for trace evidence but consume no skip.
        assert!(inspect_feedback_packets_with_compatibility(
            &report.repeat(2),
            [(0, 4, 4); 2],
            &scheduler(125),
            125,
            Speed::High,
            7,
            false,
            &mut counters,
            LIBUSB_TRANSFER_TIMED_OUT,
            true,
            &mut compatibility
        )
        .is_none());
        assert!(feedback_completion_fault(LIBUSB_TRANSFER_TIMED_OUT, false, 0x81).is_none());
        assert_eq!(compatibility.startup_packets_remaining, 2);
        assert!(counters.selected_timebase.is_none());
        assert!(select_feedback_report_with_compatibility(
            &report.repeat(2),
            [(LIBUSB_TRANSFER_ERROR, 4, 4), (0, 4, 0)],
            &scheduler(125),
            125,
            Speed::High,
            7,
            false,
            &mut counters,
            &mut compatibility
        )
        .is_none());
        assert_eq!(counters.errors, 1);
        assert_eq!(counters.empty_polls, 1);
        assert_eq!(counters.last_report_at, Some(prior_lock));
        assert_eq!(counters.reports, 0);
        let mut bytes = report.to_vec();
        bytes.extend_from_slice(&q16(6.01));
        bytes.extend_from_slice(&q16(48.0)); // newest invalid report cannot replace the good one
        let selected = select_feedback_report_with_compatibility(
            &bytes,
            [(0, 4, 4); 3],
            &scheduler(125),
            125,
            Speed::High,
            7,
            false,
            &mut counters,
            &mut compatibility,
        )
        .unwrap();
        assert!((selected.0 - 6.01).abs() < 1.0 / 65536.0);
        assert_eq!(counters.rejected, 1);
        let fresh = FeedbackCompatibility {
            tenor_nominal_q16: None,
            startup_packets_remaining: 4,
        };
        assert_eq!(fresh.startup_packets_remaining, 4);
    }

    #[test]
    fn playback_startup_keeps_malformed_safety_and_non_iso_semantics() {
        let mut compatibility = FeedbackCompatibility {
            tenor_nominal_q16: None,
            startup_packets_remaining: 4,
        };
        let mut counters = FeedbackCounters::default();
        assert!(select_feedback_report_with_compatibility(
            &[0; 16],
            [
                (0, 4, 5),
                (0, 4, 2),
                (0, 4, 0),
                (LIBUSB_TRANSFER_ERROR, 4, 4)
            ],
            &scheduler(125),
            125,
            Speed::High,
            7,
            false,
            &mut counters,
            &mut compatibility
        )
        .is_none());
        assert_eq!(counters.malformed, 2);
        assert_eq!(counters.rejected, 2);
        assert_eq!(counters.empty_polls, 1);
        assert_eq!(counters.errors, 1);
        assert_eq!(compatibility.startup_packets_remaining, 0);
        for (vendor, pipe) in [
            (0x23bb, feedback_pipe()),
            (
                0x23ba,
                FeedbackEndpoint {
                    transfer_type: TransferType::Interrupt,
                    ..feedback_pipe()
                },
            ),
        ] {
            let mut compatibility =
                FeedbackCompatibility::for_device(vendor, 1, Speed::High, pipe, 48_000);
            assert_eq!(
                compatible_report(&q16(6.0), 125, 7, false, &mut compatibility),
                Some((6.0, 48_000.0))
            );
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
        let packets = [
            (0, 4, 4),
            (0, 6, 6),
            (LIBUSB_TRANSFER_ERROR, 4, 4),
            (0, 4, 0),
            (0, 2, 2),
        ];
        assert_eq!(
            select_feedback_report(
                &bytes,
                packets,
                &scheduler(125),
                125,
                Speed::High,
                7,
                false,
                &mut counters
            ),
            Some((6.0, 48_000.0))
        );
        assert_eq!(counters.empty_polls, 1);
        assert_eq!(counters.errors, 1);
        assert_eq!(counters.malformed, 2);
        assert_eq!(counters.reports, 0); // scanning never commits packet timing
        bytes.extend_from_slice(&later);
        let packets = [
            (0, 4, 4),
            (0, 6, 6),
            (LIBUSB_TRANSFER_ERROR, 4, 4),
            (0, 4, 0),
            (0, 2, 2),
            (0, 4, 4),
        ];
        let chosen = select_feedback_report(
            &bytes,
            packets,
            &scheduler(125),
            125,
            Speed::High,
            7,
            false,
            &mut counters,
        )
        .unwrap();
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
        assert_eq!(
            select_feedback_report(
                &bytes,
                [(0, 4, 4), (0, 4, 4), (0, 4, 4), (0, 4, 5)],
                &scheduler(125),
                125,
                Speed::High,
                7,
                false,
                &mut counters
            ),
            Some((6.0, 48_000.0))
        );
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
        assert!(select_feedback_report(
            &report,
            [(0, 4, 4)],
            &scheduler(1000),
            1000,
            Speed::High,
            48,
            false,
            &mut counters
        )
        .is_none());
        let selected = select_feedback_report(
            &report,
            [(0, 4, 4)],
            &scheduler(1000),
            1000,
            Speed::High,
            49,
            false,
            &mut counters,
        )
        .unwrap();
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
    fn pipelined_iso_feedback_stays_within_the_poll_packet_budget() {
        let mut endpoint = FeedbackEndpoint {
            refresh: 0,
            address: 0x81,
            transfer_type: TransferType::Isochronous,
            interval: 4,
            service_interval_us: 1000,
            max_packet_bytes: 4,
        };
        assert_eq!(feedback_poll_geometry(endpoint, 16), (2, 8));
        assert_eq!(feedback_poll_geometry(endpoint, 5), (2, 2));
        endpoint.service_interval_us = 125;
        assert_eq!(feedback_poll_geometry(endpoint, 64), (2, 16));
        endpoint.service_interval_us = 32_000;
        assert_eq!(feedback_poll_geometry(endpoint, 16), (1, 1));
        endpoint.transfer_type = TransferType::Interrupt;
        assert_eq!(feedback_poll_geometry(endpoint, 16), (1, 1));
    }

    #[test]
    fn second_poll_submit_failure_retries_without_losing_the_active_poll() {
        let mut transfers =
            FeedbackTransfers::new(feedback_pipe(), 4, FeedbackCompatibility::default()).unwrap();
        assert_eq!(transfers.slots.len(), 2);
        let mut counters = FeedbackCounters::default();
        transfers.slots[0].last_submit_status = Some(0);
        transfers
            .record_submission(0, Ok(()), &mut counters, true)
            .unwrap();
        transfers.slots[1].last_submit_status = Some(libusb1_sys::constants::LIBUSB_ERROR_IO);
        let before_retry = Instant::now();
        transfers
            .record_submission(1, Err("transient second poll".into()), &mut counters, false)
            .unwrap();
        assert_eq!(
            transfers.pending.iter().copied().collect::<Vec<_>>(),
            vec![0]
        );
        assert!(!transfers.slots[1].in_flight);
        assert!(transfers.slots[1].retry_at >= before_retry + Duration::from_millis(5));
        assert_eq!(counters.errors, 1);
        transfers.slots[1].last_submit_status = Some(0);
        transfers
            .record_submission(1, Ok(()), &mut counters, false)
            .unwrap();
        assert_eq!(
            transfers.pending.iter().copied().collect::<Vec<_>>(),
            vec![0, 1]
        );
        transfers.slots[1].submit_no_device = true;
        assert!(matches!(
            transfers.record_submission(1, Err("detached".into()), &mut counters, false),
            Err(FeedbackFault::DeviceLost)
        ));
    }

    #[test]
    fn submitted_geometry_rejects_mismatches_and_unrepresentable_lengths() {
        assert!(validate_submission_geometry(&[4, 0, 8], 12).is_ok());
        assert!(validate_submission_geometry(&[4, 8], 11).is_err());
        assert!(validate_submission_geometry(&[4, 8], 13).is_err());
        assert!(validate_submission_geometry(&[usize::MAX, 1], 0).is_err());
        assert!(
            validate_submission_geometry(&[i32::MAX as usize + 1], i32::MAX as usize + 1).is_err()
        );
    }

    #[test]
    fn stale_retirement_does_not_clear_a_live_slot() {
        let mut slot = IsoTransferSlot::new(0, 1, 4).unwrap();
        slot.in_flight = true;
        slot.queued_frame_count = 7;
        slot.mark_completed();
        assert!(slot.in_flight);
        assert_eq!(slot.queued_frame_count, 7);
        // No actual transfer was submitted in this pure state test.
        slot.in_flight = false;
    }

    #[test]
    fn timeout_rejects_mutated_requested_geometry_but_accounts_actual_lengths() {
        let mut slot = IsoTransferSlot::new(0, 2, 16).unwrap();
        slot.packet_sizes.copy_from_slice(&[8, 8]);
        // SAFETY: the slot allocated exactly two packet descriptors and no URB is submitted.
        unsafe {
            slot.transfer.as_mut().num_iso_packets = 2;
        }
        let descriptors = unsafe {
            std::slice::from_raw_parts_mut(slot.transfer.as_mut().iso_packet_desc.as_mut_ptr(), 2)
        };
        for packet in descriptors.iter_mut() {
            packet.length = 8;
            packet.actual_length = 8;
            packet.status = LIBUSB_TRANSFER_COMPLETED;
        }
        assert_eq!(
            slot.account_timeout(PacketAcceptance::exact())
                .unwrap()
                .lost_bytes,
            0
        );
        descriptors[0].actual_length = 4;
        // A short timed-out packet is not credited as delivered, even in part.
        assert_eq!(
            slot.account_timeout(PacketAcceptance::exact())
                .unwrap()
                .lost_bytes,
            8
        );
        descriptors[0].length = 4;
        assert!(slot.account_timeout(PacketAcceptance::exact()).is_err());
        descriptors[0].length = 8;
        descriptors[0].actual_length = 9;
        assert!(slot.account_timeout(PacketAcceptance::exact()).is_err());
        descriptors[0].actual_length = 8;
        descriptors[1].status = LIBUSB_TRANSFER_NO_DEVICE;
        assert!(slot
            .account_timeout(PacketAcceptance::exact())
            .unwrap_err()
            .contains("disconnected"));
    }

    #[test]
    fn returned_ownership_survives_missing_metadata_and_stale_publication() {
        let completion = Arc::new(IsoTransferCompletion::new());
        completion.reset();
        let first = completion.generation.load(Ordering::Acquire);
        let publication = ReturnPublication {
            completion: completion.clone(),
            generation: first,
        };
        assert!(!completion.is_published()); // cancellation alone returns nothing
        drop(publication); // metadata failure still publishes ownership
        assert!(completion.is_published());
        assert_eq!(completion.take().unwrap().0, LIBUSB_TRANSFER_ERROR);
        assert!(completion.take().is_none());
        assert!(completion.is_published()); // taking the result does not undo return
        completion.reset();
        let second = completion.generation.load(Ordering::Acquire);
        drop(ReturnPublication {
            completion: completion.clone(),
            generation: first,
        });
        assert!(!completion.is_published());
        *completion.status.lock().unwrap() = Some(LIBUSB_TRANSFER_NO_DEVICE);
        drop(ReturnPublication {
            completion: completion.clone(),
            generation: second,
        });
        assert_eq!(completion.take().unwrap().0, LIBUSB_TRANSFER_NO_DEVICE);
    }

    #[test]
    fn missing_callback_stamp_is_not_a_zero_age_completion() {
        let completion = IsoTransferCompletion::new();
        completion.reset();
        *completion.status.lock().unwrap() = Some(LIBUSB_TRANSFER_COMPLETED);
        completion.publish_for_test();
        assert!(completion.take().unwrap().1.is_none());
    }

    #[test]
    fn ordered_dual_polls_share_one_playback_startup_skip_budget() {
        let endpoint = FeedbackEndpoint {
            service_interval_us: 1000,
            ..feedback_pipe()
        };
        let compatibility = FeedbackCompatibility {
            tenor_nominal_q16: None,
            startup_packets_remaining: 4,
        };
        let mut transfers = FeedbackTransfers::new(endpoint, 4, compatibility).unwrap();
        assert_eq!(transfers.slots.len(), 2);
        assert_eq!(transfers.slots[0].packet_count, 2);
        transfers.pending.extend([0, 1]);
        let first = Instant::now();
        // The later poll may publish first, but cannot consume startup skips first.
        for index in [1, 0] {
            let slot = &mut transfers.slots[index];
            unsafe {
                let data = &mut *slot.user_data;
                data.buffer.copy_from_slice(&q16(6.0).repeat(2));
                for packet_index in 0..2 {
                    let packet = &mut *slot
                        .transfer
                        .as_mut()
                        .iso_packet_desc
                        .as_mut_ptr()
                        .add(packet_index);
                    packet.length = 4;
                    packet.actual_length = 4;
                    packet.status = LIBUSB_TRANSFER_COMPLETED;
                }
                slot.transfer.as_mut().num_iso_packets = 2;
                data.completion.reset();
                *data.completion.completed_at.lock().unwrap() =
                    Some(first + Duration::from_millis(index as u64));
                *data.completion.status.lock().unwrap() = Some(LIBUSB_TRANSFER_COMPLETED);
                let generation = data.completion.generation.load(Ordering::Acquire);
                data.completion
                    .returned
                    .store(generation, Ordering::Release);
            }
            if index == 1 {
                assert!(transfers.take_next_completion().is_none());
                assert_eq!(transfers.compatibility.startup_packets_remaining, 4);
            }
        }
        let mut counters = FeedbackCounters::default();
        let scheduler = scheduler(1000);
        for expected in 0..2 {
            let (index, status, _, actual) = transfers.take_next_completion().unwrap();
            assert_eq!(index, expected);
            assert!(transfers.slots[index]
                .newest_valid_report(
                    &scheduler,
                    1000,
                    Speed::High,
                    49,
                    false,
                    &mut counters,
                    actual,
                    status,
                    &mut transfers.compatibility
                )
                .is_none());
        }
        assert_eq!(transfers.compatibility.startup_packets_remaining, 0);
        assert!(counters.selected_timebase.is_none());
        // The next poll accepts the newest plausible report and rejects its later bad packet.
        unsafe {
            (&mut *transfers.slots[0].user_data).buffer[4..8].copy_from_slice(&q16(48.0));
        }
        assert_eq!(
            transfers.slots[0].newest_valid_report(
                &scheduler,
                1000,
                Speed::High,
                49,
                false,
                &mut counters,
                8,
                LIBUSB_TRANSFER_COMPLETED,
                &mut transfers.compatibility
            ),
            Some((48.0, 48_000.0))
        );
        assert_eq!(counters.rejected, 1);
        assert!(transfers.take_next_completion().is_none());
    }

    #[test]
    fn dual_poll_enhanced_feedback_keeps_exact_eight_byte_reports() {
        let endpoint = FeedbackEndpoint {
            max_packet_bytes: 8,
            service_interval_us: 1000,
            ..feedback_pipe()
        };
        let mut transfers =
            FeedbackTransfers::new(endpoint, 4, FeedbackCompatibility::default()).unwrap();
        let mut counters = FeedbackCounters::default();
        let report = (6u64 << 32).to_le_bytes();
        let slot = &mut transfers.slots[1];
        unsafe {
            (&mut *slot.user_data)
                .buffer
                .copy_from_slice(&report.repeat(2));
            slot.transfer.as_mut().num_iso_packets = 2;
            for (index, actual) in [8, 4].into_iter().enumerate() {
                let packet = &mut *slot
                    .transfer
                    .as_mut()
                    .iso_packet_desc
                    .as_mut_ptr()
                    .add(index);
                packet.length = 8;
                packet.actual_length = actual;
                packet.status = LIBUSB_TRANSFER_COMPLETED;
            }
        }
        assert_eq!(
            slot.newest_valid_report(
                &scheduler(1000),
                1000,
                Speed::Super,
                49,
                false,
                &mut counters,
                12,
                LIBUSB_TRANSFER_COMPLETED,
                &mut transfers.compatibility
            ),
            Some((48.0, 48_000.0))
        );
        assert_eq!(counters.last_report_len, 8);
        assert_eq!(counters.rejected, 1);
    }

    #[test]
    fn trace_distinguishes_rejected_rate_and_capacity_from_transport_failures() {
        use crate::ExplicitFeedbackOutcome as Outcome;
        let recorder = Arc::new(crate::feedback_diagnostics::FeedbackRecorder::new(
            Instant::now(),
        ));
        let mut counters = FeedbackCounters {
            trace: Some(recorder),
            diagnostics: Some(crate::ExplicitFeedbackDiagnostics::default()),
            ..Default::default()
        };
        let rate = q16(48.0);
        assert!(select_feedback_report(
            &rate,
            [(0, 4, 4)],
            &scheduler(250),
            250,
            Speed::High,
            128,
            false,
            &mut counters
        )
        .is_none());
        let capacity = q16(6.005);
        assert!(select_feedback_report(
            &capacity,
            [(0, 4, 4)],
            &scheduler(125),
            125,
            Speed::High,
            6,
            false,
            &mut counters
        )
        .is_none());
        let mut partial = q16(6.0).to_vec();
        partial.extend_from_slice(&[0; 4]);
        assert!(inspect_feedback_packets(
            &partial,
            [(0, 4, 4), (NO_FEEDBACK_PACKET_STATUS, 4, 0)],
            &scheduler(125),
            125,
            Speed::High,
            7,
            false,
            &mut counters,
            LIBUSB_TRANSFER_TIMED_OUT,
            true
        )
        .is_none());
        let diagnostics = counters.diagnostics.unwrap();
        let sample = |outcome| {
            diagnostics
                .packet_samples()
                .find(|sample| sample.outcome == outcome)
                .unwrap()
        };
        assert_eq!(
            sample(Outcome::RateOutOfRange).decoded_rate_hz,
            Some(384_000.0)
        );
        assert_eq!(sample(Outcome::RateOutOfRange).raw_bytes(), rate);
        assert_eq!(
            sample(Outcome::PacketCapacityExceeded).raw_bytes(),
            capacity
        );
        assert_eq!(
            sample(Outcome::TransferFailed).transfer_status,
            LIBUSB_TRANSFER_TIMED_OUT
        );
        assert_eq!(sample(Outcome::TransferFailed).packet_status, Some(0));
        assert_eq!(sample(Outcome::TransferFailed).raw_bytes(), &partial[..4]);
        assert_eq!(diagnostics.packet_status_counts[7], 1);
    }

    #[test]
    fn summary_has_no_raw_bytes_with_trace_disabled() {
        let mut counters = FeedbackCounters {
            diagnostics: Some(crate::ExplicitFeedbackDiagnostics::default()),
            ..Default::default()
        };
        let report = q16(48.0);
        select_feedback_report(
            &report,
            [(0, 4, 4)],
            &scheduler(125),
            125,
            Speed::High,
            7,
            false,
            &mut counters,
        );
        let diagnostics = counters.diagnostics.unwrap();
        let sample = diagnostics.packet_samples().next().unwrap();
        assert_eq!(
            sample.outcome,
            crate::ExplicitFeedbackOutcome::RateOutOfRange
        );
        assert_eq!(sample.actual_bytes, 4);
        assert!(sample.raw_bytes().is_empty());
    }

    #[test]
    fn ap80_pcm_report_calibrates_from_three_completions_and_keeps_that_timebase() {
        // AP80 Pro-X c502:0072, 16-bit stereo PCM, alt 2. OUT is serviced
        // every 250 us and the IN feedback is polled every 1 ms.
        let report = [0x46, 0x09, 0x0b, 0x00];
        let standard = q16(44_100.0 / 8000.0);
        let scheduler = IsoPacketScheduler::new(44_100, 4, 250);
        let mut counters = FeedbackCounters {
            infer_service_interval: true,
            diagnostics: Some(crate::ExplicitFeedbackDiagnostics::default()),
            ..Default::default()
        };
        // Two reports in a single completion still count as one observation.
        let batch = [report, report].concat();
        assert!(select_feedback_report(
            &batch,
            [(0, 4, 4), (0, 4, 4)],
            &scheduler,
            250,
            Speed::High,
            256,
            false,
            &mut counters
        )
        .is_none());
        assert!(select_feedback_report(
            &report,
            [(0, 4, 4)],
            &scheduler,
            250,
            Speed::High,
            256,
            false,
            &mut counters
        )
        .is_none());
        let (frames, rate) = select_feedback_report(
            &report,
            [(0, 4, 4)],
            &scheduler,
            250,
            Speed::High,
            256,
            false,
            &mut counters,
        )
        .unwrap();
        assert!((frames - 11.036_224).abs() < 0.000_001);
        assert!((rate - 44_144.897).abs() < 0.001);
        let diagnostics = counters.diagnostics.unwrap();
        assert_eq!(diagnostics.timebase, Some("out_service_interval"));
        assert!(diagnostics.inferred_timebase);
        // A later valid standard-unit report is accepted immediately.
        assert!(select_feedback_report(
            &standard,
            [(0, 4, 4)],
            &scheduler,
            250,
            Speed::High,
            256,
            false,
            &mut counters
        )
        .is_some());
    }

    #[test]
    fn ap80_native_dsd_feedback_calibrates_at_all_measured_wire_rates() {
        for (rate, report) in [
            (88_200, [0xa2, 0x10, 0x16, 0]),
            (176_400, [0x2e, 0x1d, 0x2c, 0]),
            (352_800, [0x0a, 0x37, 0x58, 0]),
        ] {
            let scheduler = IsoPacketScheduler::new(rate, 8, 250);
            let mut counters = FeedbackCounters {
                infer_service_interval: true,
                ..Default::default()
            };
            for _ in 0..2 {
                assert!(select_feedback_report(
                    &report,
                    [(0, 4, 4)],
                    &scheduler,
                    250,
                    Speed::High,
                    128,
                    false,
                    &mut counters
                )
                .is_none());
            }
            let (_, decoded) = select_feedback_report(
                &report,
                [(0, 4, 4)],
                &scheduler,
                250,
                Speed::High,
                128,
                false,
                &mut counters,
            )
            .unwrap();
            assert!((decoded - f64::from(rate)).abs() < 65.0);
            assert!(counters.selected_service_interval());
        }
    }

    #[test]
    fn calibration_requires_verified_rate_and_prefers_a_valid_standard_report() {
        let alternate = q16(11.025);
        let standard = q16(5.5125);
        let scheduler = IsoPacketScheduler::new(44_100, 4, 250);
        let mut counters = FeedbackCounters::default();
        for _ in 0..4 {
            assert!(select_feedback_report(
                &alternate,
                [(0, 4, 4)],
                &scheduler,
                250,
                Speed::High,
                256,
                false,
                &mut counters
            )
            .is_none());
        }
        assert!(counters.selected_timebase.is_none());
        counters.infer_service_interval = true;
        // The alternate interpretation needs at least 12 frames of capacity.
        for _ in 0..4 {
            assert!(select_feedback_report(
                &alternate,
                [(0, 4, 4)],
                &scheduler,
                250,
                Speed::High,
                11,
                false,
                &mut counters
            )
            .is_none());
        }
        assert!(counters.selected_timebase.is_none());
        let batch = [alternate, standard, alternate].concat();
        let (_, rate) = select_feedback_report(
            &batch,
            [(0, 4, 4); 3],
            &scheduler,
            250,
            Speed::High,
            256,
            false,
            &mut counters,
        )
        .unwrap();
        assert!((rate - 44_100.0).abs() < 0.1);
        assert_eq!(counters.selected_timebase, Some(FeedbackTimebase::Bus));
        assert!(select_feedback_report(
            &alternate,
            [(0, 4, 4)],
            &scheduler,
            250,
            Speed::High,
            256,
            false,
            &mut counters
        )
        .is_none());
    }

    #[test]
    fn saved_units_still_require_a_fresh_report_and_clock() {
        let alternate = q16(12.0);
        let scheduler = IsoPacketScheduler::new(48_000, 4, 250);
        let mut counters = FeedbackCounters {
            cached_service_interval: true,
            diagnostics: Some(crate::ExplicitFeedbackDiagnostics::default()),
            ..Default::default()
        };
        assert!(select_feedback_report(
            &alternate,
            [(0, 4, 0)],
            &scheduler,
            250,
            Speed::High,
            256,
            false,
            &mut counters
        )
        .is_none());
        assert!(counters.selected_timebase.is_none());
        assert!(select_feedback_report(
            &alternate,
            [(0, 4, 4)],
            &scheduler,
            250,
            Speed::High,
            256,
            false,
            &mut counters
        )
        .is_none());

        // A second stream at a different rate can reuse the saved units after
        // checking them against its own readback and packet capacity.
        counters.infer_service_interval = true;
        counters.cached_service_interval = true;
        assert_eq!(
            select_feedback_report(
                &alternate,
                [(0, 4, 4)],
                &scheduler,
                250,
                Speed::High,
                256,
                false,
                &mut counters
            ),
            Some((12.0, 48_000.0))
        );
        assert!(counters.selected_service_interval());
        assert!(counters.diagnostics.unwrap().reused_timebase);
    }

    #[test]
    fn standard_timing_supersedes_saved_service_interval_units() {
        let scheduler = IsoPacketScheduler::new(48_000, 4, 250);
        let mut counters = FeedbackCounters {
            infer_service_interval: true,
            cached_service_interval: true,
            ..Default::default()
        };
        let batch = [q16(12.0), q16(6.0)].concat();
        assert_eq!(
            select_feedback_report(
                &batch,
                [(0, 4, 4); 2],
                &scheduler,
                250,
                Speed::High,
                256,
                false,
                &mut counters
            ),
            Some((12.0, 48_000.0))
        );
        assert_eq!(counters.selected_timebase, Some(FeedbackTimebase::Bus));
    }

    #[test]
    fn linux_uac2_gadget_per_service_vector_requires_opt_in() {
        // Linux v7.0 u_audio.c u_audio_set_fback_frequency: a 1 ms HS OUT
        // service can report 48 Q16.16 frames per OUT packet rather than 6
        // per USB microframe. Enable only after the device trace proves units.
        let report = q16(48.0);
        let mut counters = FeedbackCounters::default();
        assert!(select_feedback_report(
            &report,
            [(0, 4, 4)],
            &scheduler(1000),
            1000,
            Speed::High,
            49,
            false,
            &mut counters
        )
        .is_none());
        assert_eq!(
            select_feedback_report(
                &report,
                [(0, 4, 4)],
                &scheduler(1000),
                1000,
                Speed::High,
                49,
                true,
                &mut counters
            ),
            Some((48.0, 48_000.0))
        );
        assert_eq!(
            decode_feedback_report_with_scale(&report, 1000, Speed::Super, true),
            decode_feedback_report(&report, 1000, Speed::Super)
        );
        assert_eq!(
            decode_feedback_report_with_scale(&[0, 0, 12], 1000, Speed::High, true),
            decode_feedback_report(&[0, 0, 12], 1000, Speed::High)
        );
        assert_eq!(
            decode_feedback_report_with_scale(&[0, 0, 12], 1000, Speed::Full, true),
            Some((48.0, 48_000.0))
        );
    }

    #[test]
    fn nominal_warmup_stale_recovery_and_terminal_deadline_follow_last_actual_report() {
        let health = ExplicitFeedbackHealth::new(1000, None);
        let start = Instant::now();
        assert_eq!(health.state(start, start, None), (false, false, false));
        assert_eq!(
            health.state(start + Duration::from_millis(500), start, None),
            (false, true, false)
        );
        assert_eq!(
            health.state(start + Duration::from_secs(1), start, None),
            (false, true, true)
        );
        let good = start + Duration::from_millis(100);
        assert_eq!(health.state(good, start, Some(good)), (true, false, false));
        assert_eq!(
            health.state(start + Duration::from_millis(600), start, Some(good)),
            (false, true, false)
        );
        let recovered = start + Duration::from_millis(900);
        assert_eq!(
            health.state(recovered, start, Some(recovered)),
            (true, false, false)
        );
        // Empty success produces no report and cannot refresh the clock or last rate.
        let mut counters = FeedbackCounters {
            last_report_at: Some(good),
            ..Default::default()
        };
        assert!(select_feedback_report(
            &[0; 4],
            [(0, 4, 0)],
            &scheduler(125),
            125,
            Speed::High,
            7,
            false,
            &mut counters
        )
        .is_none());
        assert_eq!(counters.last_report_at, Some(good));
        assert_eq!(
            health.state(
                start + Duration::from_millis(1100),
                start,
                counters.last_report_at
            ),
            (false, true, true)
        );
    }

    #[test]
    fn first_submit_failure_is_terminal_and_detach_is_distinct() {
        assert!(matches!(
            feedback_submit_failure("allocation/submit error".into(), false, true),
            Some(FeedbackFault::Transfer(_))
        ));
        assert!(feedback_submit_failure("transient".into(), false, false).is_none());
        assert!(matches!(
            feedback_submit_failure("detached".into(), true, true),
            Some(FeedbackFault::DeviceLost)
        ));
        assert!(matches!(
            feedback_submit_failure("detached".into(), true, false),
            Some(FeedbackFault::DeviceLost)
        ));
        assert!(matches!(
            feedback_completion_fault(LIBUSB_TRANSFER_NO_DEVICE, false, 0x81),
            Some(FeedbackFault::DeviceLost)
        ));
        assert!(feedback_completion_fault(LIBUSB_TRANSFER_CANCELLED, true, 0x81).is_none());
        assert!(matches!(
            feedback_completion_fault(LIBUSB_TRANSFER_CANCELLED, false, 0x81),
            Some(FeedbackFault::Transfer(_))
        ));
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
        assert_eq!(
            health.state(processed, start, Some(before_deadline)),
            (true, false, false)
        );
        let prior_good = start + Duration::from_millis(200);
        assert!(health.report_is_timely(
            prior_good + health.terminal - Duration::from_micros(1),
            start,
            Some(prior_good)
        ));
        assert!(!health.report_is_timely(prior_good + health.terminal, start, Some(prior_good)));
    }

    #[test]
    fn deadlines_honor_valid_uac1_refresh_and_sparse_polling() {
        let slow = ExplicitFeedbackHealth::new(1000, Some(9));
        assert_eq!(slow.warning, Duration::from_millis(1536));
        assert_eq!(slow.terminal, Duration::from_millis(3072));
        let invalid = ExplicitFeedbackHealth::new(1000, Some(10));
        assert_eq!(invalid.warning, Duration::from_millis(500));
        assert_eq!(
            ExplicitFeedbackHealth::new(1_024_000, None).terminal,
            Duration::from_millis(6144)
        );
    }
}

#[cfg(test)]
mod tx_length_prefix_tests {
    use super::*;
    #[test]
    fn headers_preserve_pcm_and_silence_payload_and_music_counts() {
        for held in [false, true] {
            let bytes = if held { vec![0; 12] } else { (0..12).collect() };
            let mut payload = IsoTransferPayload {
                packet_sizes: vec![6, 6],
                packet_samples: vec![],
                transfer_buffer: bytes.clone(),
                underrun: false,
                held,
                frame_count: 2,
                music_frames: if held { 0 } else { 2 },
                epoch: 7,
            };
            add_tx_length_prefix(&mut payload).unwrap();
            assert_eq!(payload.packet_sizes, vec![10, 10]);
            assert_eq!(&payload.transfer_buffer[0..4], &6u32.to_le_bytes());
            assert_eq!(&payload.transfer_buffer[4..10], &bytes[..6]);
            assert_eq!(&payload.transfer_buffer[10..14], &6u32.to_le_bytes());
            assert_eq!(&payload.transfer_buffer[14..20], &bytes[6..]);
            assert_eq!(payload.frame_count, 2);
            assert_eq!(payload.music_frames, if held { 0 } else { 2 });
            assert_eq!(payload.epoch, 7);
        }
    }
}

#[cfg(test)]
mod source_wire_owner_policy_tests {
    use super::*;
    use crate::{DacConfig, DeviceInfo, QuirkOverride, QuirkPolicy};
    #[test]
    fn host_flag_names_cannot_enable_wire_framing_or_playback_bootstrap() {
        let a = context::SessionContext::unregistered();
        let b = context::SessionContext::unregistered();
        let _a = a.enter();
        crate::settings::install(crate::settings::Settings::for_device(
            DacConfig {
                quirk_policy: QuirkPolicy::SourceBacked,
                quirks: vec![QuirkOverride {
                    vendor_id: 1,
                    product_id: 2,
                    name_contains: None,
                    quirks: vec![crate::Quirk::TxLengthPrefix, crate::Quirk::PlaybackFirst],
                }],
                ..DacConfig::default()
            },
            &DeviceInfo::new(1, 2, "caller metadata"),
        ));
        assert!(settings().has(crate::Quirk::TxLengthPrefix));
        assert!(settings().has(crate::Quirk::PlaybackFirst));
        assert_eq!(data_packet_prefix_bytes(), 0);
        assert!(!settings().source_has(crate::Quirk::PlaybackFirst));
        // The internal descriptor path, rather than caller names/overrides,
        // resolves one exact source fact under its owning context.
        crate::settings::install_descriptor_identity(0x1686, 0x00dd, 0x0100);
        assert_eq!(data_packet_prefix_bytes(), 4);
        assert!(!settings().source_has(crate::Quirk::PlaybackFirst));
        {
            let _b = b.enter();
            assert_eq!(data_packet_prefix_bytes(), 0);
            crate::settings::install(crate::settings::Settings::for_device(
                DacConfig {
                    quirk_policy: QuirkPolicy::SourceBacked,
                    ..DacConfig::default()
                },
                &DeviceInfo::new(9, 9, "caller metadata"),
            ));
            crate::settings::install_descriptor_identity(0x1397, 0x0507, 0x0100);
            assert!(settings().source_has(crate::Quirk::PlaybackFirst));
            assert_eq!(data_packet_prefix_bytes(), 0);
        }
        assert_eq!(data_packet_prefix_bytes(), 4);
        assert!(!settings().source_has(crate::Quirk::PlaybackFirst));
    }
}

#[cfg(test)]
mod feedback_smoothing_trace_tests {
    use super::*;
    #[test]
    fn support_trace_distinguishes_accepted_report_and_actual_scheduler_rate() {
        for policy in [
            crate::FeedbackSmoothing::Direct,
            crate::FeedbackSmoothing::Ewma,
        ] {
            let mut scheduler =
                IsoPacketScheduler::new(48_000, 4, 125).with_explicit_feedback_smoothing(policy);
            assert!(scheduler.update_feedback_frames_per_packet(6.0));
            assert!(scheduler.update_feedback_frames_per_packet(9.0));
            let crate::FeedbackTraceEvent::ExplicitFeedbackRate {
                smoothing,
                reported_frames_per_out_service,
                applied_frames_per_out_service,
                applied_rate_hz,
            } = applied_feedback_trace(&scheduler, 9.0, 125).unwrap()
            else {
                panic!("scheduler rate observation expected");
            };
            assert_eq!(smoothing, policy);
            assert_eq!(reported_frames_per_out_service, 9.0);
            let expected = if policy == crate::FeedbackSmoothing::Direct {
                9.0
            } else {
                6.6
            };
            assert!((applied_frames_per_out_service - expected).abs() < 1e-12);
            assert!((applied_rate_hz - expected * 8000.0).abs() < 1e-8);
            assert!(applied_feedback_trace(&scheduler, 9.0, 0).is_none());
        }
    }
}
