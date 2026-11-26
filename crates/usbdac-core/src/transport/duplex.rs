//! A capture attachment to the playback worker, never a second USB event owner.
use super::*;
use crate::capture::{CaptureBuffer, CapturePacket, CaptureRequest};
use super::capture_stream::{CaptureEndpoint, CaptureHandle};
use std::collections::VecDeque;

pub(super) struct Attachment {
    pub(super) endpoint: CaptureEndpoint,
    pub(super) shared_implicit: bool,
    buffer: Arc<Mutex<CaptureBuffer>>,
    error: Arc<Mutex<Option<Error>>>,
    retired: Arc<AtomicBool>,
    deferred: Arc<capture_stream::DeferredCapture>,
    original_rate: Mutex<Option<(ClockChoice, u32)>>,
}

/// Equality of rates is insufficient to establish independent clock ownership.
fn clock_pair(playback: Option<&ClockChoice>, capture: Option<&ClockChoice>,
    playback_source_rate: u32, capture_source_rate: u32) -> Result<(), Error> {
    use crate::clock_resolution::ClockResolution;
    match (playback, capture) {
        (Some(out), Some(input)) if out.resolution == ClockResolution::Resolved
            && input.resolution == ClockResolution::Resolved => {
            if out.control_interface == input.control_interface {
                for left in &out.path {
                    for right in &input.path {
                        if left.id == right.id && left.kind != right.kind {
                            return Err(Error::Unsupported { reason: "duplex clock paths disagree about a shared entity or selector pin".into(), offered: vec![] });
                        }
                    }
                }
            }
            if out.control_interface == input.control_interface && out.source_id == input.source_id
                && playback_source_rate != capture_source_rate {
                return Err(Error::Unsupported { reason: "shared duplex clock requires equal source rates".into(), offered: vec![] });
            }
            // Different, terminal-resolved source IDs in the same function (or
            // sources in distinct functions) can be programmed independently.
            Ok(())
        }
        _ => Err(Error::Unsupported { reason: "duplex requires proven terminal clock identities".into(), offered: vec![] }),
    }
}

fn validate_alternates(playback: &IsoStreamCandidate, input: &CaptureEndpoint) -> Result<bool, Error> {
    if playback.interface_number == input.interface && playback.alt_setting != input.alternate {
        return Err(Error::Unsupported { reason: "duplex alternates conflict on one interface".into(), offered: vec![] });
    }
    if input.adc3_route.as_ref().and_then(|route| route.feedforward)
        .is_some_and(|pipe| pipe.address == playback.endpoint_address) {
        return Err(Error::Unsupported { reason: "duplex playback overlaps capture feedforward OUT ownership".into(), offered: vec![] });
    }
    if playback.endpoint_address == input.address {
        return Err(Error::Unsupported { reason: "duplex endpoint directions overlap".into(), offered: vec![] });
    }
    if let Some(source) = playback.implicit_feedback {
        if source.interface_number == input.interface && source.alt_setting != input.alternate {
            return Err(Error::Unsupported { reason: "capture alternate conflicts with implicit feedback".into(), offered: vec![] });
        }
        if source.address == input.address {
            let exact = source.interface_number == input.interface && source.alt_setting == input.alternate
                && source.bytes_per_frame == input.format.bytes_per_frame()
                && source.service_interval_us == input.service_us && source.max_packet_bytes == input.packet_bytes;
            if !exact {
                return Err(Error::Unsupported { reason: "capture must exactly match the owned implicit IN wire format".into(), offered: vec![] });
            }
            return Ok(true);
        }
    }
    Ok(false)
}

fn input_clock(claimed: &ClaimedHandle, endpoint: &CaptureEndpoint) -> Option<ClockChoice> {
    if let Some(route) = endpoint.adc3_route.as_ref() { return route.choice.clone(); }
    let config = claimed.handle.device().active_config_descriptor().ok()?;
    strict_implicit_clock(&config, &claimed.handle, endpoint.control, Some(endpoint.terminal))
}

fn validate_clocks(claimed: &ClaimedHandle, playback: &IsoStreamCandidate,
    expected: &crate::StreamFormat, endpoint: &CaptureEndpoint) -> Result<(), Error> {
    let route = adc3_route_for_candidate(&claimed.handle.device(), &claimed.handle, playback).map_err(Error::Control)?;
    if let (Some(output), Some(input)) = (route.as_ref(), endpoint.adc3_route.as_ref()) {
        // BADD prescribes a common fixed 48 kHz cadence for both directions
        // within one AudioControl function, without programmable Clock Sources.
        if output.clock.is_none() && input.clock.is_none() && output.control == input.control
            && expected.sample_rate == 48_000 && endpoint.format.sample_rate() == 48_000 {
            return Ok(());
        }
    }
    let output = route.as_ref().and_then(|route| route.choice.as_ref()).or(expected.clock.as_ref());
    let output_rate = route.as_ref().map_or(Ok(expected.sample_rate), |route| route.source_rate(expected.sample_rate))
        .map_err(Error::Control)?;
    let input_rate = capture_source_rate(endpoint)?;
    clock_pair(output, input_clock(claimed, endpoint).as_ref(), output_rate, input_rate)
}

fn capture_source_rate(endpoint: &CaptureEndpoint) -> Result<u32, Error> {
    endpoint.adc3_route.as_ref().map_or(Ok(endpoint.format.sample_rate()),
        |route| route.source_rate(endpoint.format.sample_rate())).map_err(Error::Control)
}

pub(super) fn prepare(claimed: &ClaimedHandle, playback: &IsoStreamCandidate,
    expected: &crate::StreamFormat, request: &CaptureRequest, capacity: usize) -> Result<Arc<Attachment>, Error> {
    let config = claimed.handle.device().active_config_descriptor().map_err(|e| Error::Claim(e.to_string()))?;
    let mut endpoints = capture_stream::owned_capture_candidates(claimed, &config, request.sample_rate, true).map_err(Error::Claim)?;
    endpoints.retain(|endpoint| endpoint.format.satisfies(request)
        && claimed.claimed_interfaces.contains(&endpoint.interface)
        && claimed.claimed_interfaces.contains(&endpoint.control));
    endpoints.sort_by_key(|e| (e.format.subslot_bytes(), e.interface, e.alternate, e.address));
    let mut refusal = None;
    for endpoint in endpoints {
        let pair = (|| {
            let shared = validate_alternates(playback, &endpoint)?;
            if !shared {
                let geometry = IsoQueueGeometry::for_stream(endpoint.service_us,
                    settings().config.bus_queue_ms, settings().config.transfers_in_flight);
                capture_stream::capture_queue_bytes(endpoint.packet_bytes, geometry)?;
            }
            validate_clocks(claimed, playback, expected, &endpoint)?;
            let buffer = CaptureBuffer::new(endpoint.format.clone(), capacity)
                .map_err(|e| Error::InvalidArgument(format!("capture buffer: {e:?}")))?;
            Ok((shared, buffer))
        })();
        match pair {
            Ok((shared_implicit, buffer)) => return Ok(Arc::new(Attachment::new(endpoint, shared_implicit, buffer))),
            Err(error) => refusal = Some(error),
        }
    }
    Err(refusal.unwrap_or_else(|| Error::Unsupported { reason: "no claimed PCM capture alternate matches the duplex request".into(), offered: vec![] }))
}

impl Attachment {
    pub(super) fn new(endpoint: CaptureEndpoint, shared_implicit: bool, buffer: CaptureBuffer) -> Self {
        Self { endpoint, shared_implicit, buffer: Arc::new(Mutex::new(buffer)),
            error: Arc::new(Mutex::new(None)), retired: Arc::new(AtomicBool::new(false)),
            deferred: Arc::new(capture_stream::DeferredCapture::default()), original_rate: Mutex::new(None) }
    }
    pub(super) fn configure(&self, claimed: &ClaimedHandle, playback: &IsoStreamCandidate,
        expected: &crate::StreamFormat) -> Result<(), Error> {
        validate_clocks(claimed, playback, expected, &self.endpoint)?;
        if self.shared_implicit {
            // Playback owns both programming and activation of this IN source.
            // Its existing clock transaction has already selected the wire rate.
            let choice = input_clock(claimed, &self.endpoint);
            let reported = if let Some(route) = self.endpoint.adc3_route.as_ref() {
                adc3::verify_adc3_rate_route(&claimed.handle, route).map_err(Error::Control)?;
                if let Some(clock) = choice.as_ref() {
                    get_sampling_frequency(&claimed.handle, clock.control_interface, clock.source_id)
                        .and_then(|source| route.wire_rate(source)).ok()
                } else { Some(48_000) }
            } else {
                let clock = choice.ok_or_else(|| Error::Control("shared capture source disappeared".into()))?;
                get_sampling_frequency(&claimed.handle, clock.control_interface, clock.source_id).ok()
            };
            if reported != Some(self.endpoint.format.sample_rate()) {
                return Err(Error::ClockUnverified { requested_hz: self.endpoint.format.sample_rate(), reported_hz: reported });
            }
        } else {
            if let Some(clock) = input_clock(claimed, &self.endpoint) {
                let original = get_sampling_frequency(&claimed.handle, clock.control_interface, clock.source_id).map_err(Error::Control)?;
                if original != capture_source_rate(&self.endpoint)? {
                    *self.original_rate.lock() = Some((clock, original));
                }
            }
            capture_stream::configure_capture_endpoint(claimed, &self.endpoint)?;
        }
        let current = candidate_audio_control_clock_choice(&claimed.handle.device(), &claimed.handle, playback);
        if current.as_ref().map(|(_, choice)| choice) != expected.clock.as_ref() {
            return Err(Error::InvalidState("playback clock route changed while configuring duplex capture".into()));
        }
        validate_clocks(claimed, playback, expected, &self.endpoint)
    }
    pub(super) fn reader(&self, stop: Arc<AtomicBool>) -> CaptureHandle {
        CaptureHandle { endpoint: self.endpoint.clone(), buffer: self.buffer.clone(), error: self.error.clone(), stop, worker: None, paired: true, retired: Some(self.retired.clone()), deferred: Some(self.deferred.clone()) }
    }
    pub(super) fn ingest(&self, slot: &IsoTransferSlot, status: i32, scratch: &mut [CapturePacket]) -> Result<(), Error> {
        let result = if let Some(mut buffer) = self.buffer.try_lock() {
            self.deferred.merge_into(&mut buffer);
            capture_stream::ingest_slot(slot, status, &mut buffer, scratch)
        } else {
            // No payload is retained while presentation owns the queue. Inspect
            // the same descriptors and count only valid full frames as a host
            // drop; empty/malformed/failed packets remain separate telemetry.
            let mut stats = crate::capture::CaptureStats::default();
            let result = match capture_stream::inspect_slot(slot, status, scratch) {
                Ok((data, detached)) => {
                    let result = stats.record_completion(data.len(), &slot.packet_sizes, scratch,
                        true, 0, self.endpoint.format.bytes_per_frame())
                        .map_err(|e| Error::Transfer(format!("capture completion: {e:?}")));
                    stats.contention_frames = stats.captured_frames;
                    if detached { Err(Error::DeviceLost) } else { result }
                }
                Err(error) => {
                    if status == LIBUSB_TRANSFER_COMPLETED { stats.malformed_completions = 1; }
                    else { stats.failed_transfers = 1; }
                    Err(error)
                }
            };
            self.deferred.record(stats);
            result
        };
        if let Err(error) = &result { *self.error.lock() = Some(error.clone()); }
        result
    }
    pub(super) fn failure(&self) -> Option<Error> { self.error.lock().clone() }
    pub(super) fn retirement_guard(&self) -> Retirement { Retirement(self.retired.clone()) }
    fn restore_rate(&self, claimed: &ClaimedHandle) -> bool {
        let Some((clock, rate)) = self.original_rate.lock().take() else { return true; };
        // Only a distinct source can have a saved rate. Retire callbacks and
        // reset its alternate before restoring that source, while still awake.
        let result = claimed.set_alternate_setting(self.endpoint.interface, 0)
            .and_then(|()| {
                if let Some(route) = self.endpoint.adc3_route.as_ref() {
                    claimed.adc3_power.verify_idle(self.endpoint.interface)?;
                    adc3::verify_adc3_rate_route(&claimed.handle, route)?;
                    let config = claimed.handle.device().active_config_descriptor().map_err(|e| e.to_string())?;
                    let actual = adc3::adc3_capture_route_for_endpoint(claimed, &config,
                        self.endpoint.interface, self.endpoint.alternate, route)?;
                    if &actual != route { return Err("ADC3 capture route changed before rollback".into()); }
                }
                let current = get_sampling_frequency(&claimed.handle, clock.control_interface, clock.source_id)?;
                if current == rate { return Ok(()); }
                if current != capture_source_rate(&self.endpoint).map_err(|e| e.message())? {
                    return Err("capture source drifted; preserving external frequency".into());
                }
                set_sampling_frequency(&claimed.handle, clock.control_interface, clock.source_id, rate)
            })
            .and_then(|()| {
                if get_sampling_frequency(&claimed.handle, clock.control_interface, clock.source_id)? == rate { Ok(()) }
                else { Err("duplex capture clock rollback did not verify".into()) }
            });
        if let Err(error) = result {
            // Keep the owning ADC3 function awake when a distinct capture
            // source could not be handed back after callback retirement.
            claimed.adc3_power.disarm();
            set_last_error(Some(format!("capture clock rollback failed: {error}")));
            return false;
        }
        true
    }
    pub(super) fn reordered(&self) {
        self.deferred.record(crate::capture::CaptureStats {
            reordered_completions: 1, ..Default::default()
        });
    }
    pub(super) fn finish(&self, failure: Option<Error>) {
        let failed = failure.is_some();
        if let Some(error) = failure { *self.error.lock() = Some(error); }
        self.deferred.finish(failed);
        if let Some(mut buffer) = self.buffer.try_lock() { self.deferred.merge_into(&mut buffer); }
    }
}
impl Drop for Attachment {
    fn drop(&mut self) {
        if !self.deferred.is_finished() {
            self.finish(Some(Error::Transfer("duplex worker stopped before capture completed".into())));
        }
    }
}


pub(super) struct Retirement(Arc<AtomicBool>);
impl Drop for Retirement {
    fn drop(&mut self) { self.0.store(true, Ordering::Release); }
}

pub(super) fn cleanup_claimed(claimed: ClaimedHandle, attachment: Option<&Arc<Attachment>>,
    active_interface: Option<u8>, keep_locked: bool) -> bool {
    let capture_restored = attachment.is_none_or(|attachment| attachment.restore_rate(&claimed));
    let owner_restored = cleanup_claimed_handle(claimed, active_interface, keep_locked && capture_restored);
    capture_restored && owner_restored
}

pub(super) struct InputPool {
    pub(super) slots: Vec<IsoTransferSlot>,
    feedforward: Option<capture_stream::FeedforwardPool>,
    attachment: Arc<Attachment>,
    order: VecDeque<usize>,
    reordered: Vec<bool>,
    scratch: Vec<CapturePacket>,
}
impl InputPool {
    pub(super) fn new(attachment: Arc<Attachment>) -> Result<Self, String> {
        let geometry = IsoQueueGeometry::for_stream(attachment.endpoint.service_us,
            settings().config.bus_queue_ms, settings().config.transfers_in_flight);
        let transfer_bytes = capture_stream::capture_queue_bytes(attachment.endpoint.packet_bytes, geometry)
            .map_err(|error| error.message())?;
        let mut slots = Vec::with_capacity(geometry.transfer_slots);
        for index in 0..geometry.transfer_slots {
            slots.push(IsoTransferSlot::new(index, geometry.packets_per_transfer, transfer_bytes)?);
        }
        let feedforward = capture_stream::FeedforwardPool::for_endpoint(&attachment.endpoint)?;
        Ok(Self { slots, feedforward, attachment, order: VecDeque::with_capacity(geometry.transfer_slots),
            reordered: vec![false; geometry.transfer_slots], scratch: vec![CapturePacket {
                requested_bytes: 0, actual_bytes: 0, succeeded: false }; geometry.packets_per_transfer] })
    }
    pub(super) fn start(&mut self, handle: &DeviceHandle<Context>, stop: &AtomicBool) -> Result<(), Error> {
        if let Some(pool) = self.feedforward.as_mut() { pool.start(handle, stop)?; }
        for (index, slot) in self.slots.iter_mut().enumerate() {
            if usb_stop_requested(stop) { break; }
            slot.submit_capture(handle, self.attachment.endpoint.address, self.attachment.endpoint.packet_bytes)
                .map_err(|e| if crate::error::is_device_gone(&e) { Error::DeviceLost } else { Error::Transfer(e) })?;
            self.order.push_back(index);
        }
        Ok(())
    }
    pub(super) fn primed(&self) -> bool { self.feedforward.as_ref().map_or(true, |pool| pool.primed()) }
    pub(super) fn cancel(&mut self) {
        for slot in &mut self.slots { slot.cancel(); }
        if let Some(pool) = self.feedforward.as_mut() { pool.cancel(); }
    }
    pub(super) fn drain(&mut self, context: &Context) {
        self.cancel();
        drain_iso_transfer_slots(context, &mut self.slots);
        if let Some(pool) = self.feedforward.as_mut() { pool.drain(context); }
    }
    pub(super) fn pump(&mut self, handle: &DeviceHandle<Context>, stop: &AtomicBool) -> Result<(), Error> {
        if let Some(pool) = self.feedforward.as_mut() { pool.pump(handle, stop)?; }
        if let Some(&head) = self.order.front() {
            for &index in self.order.iter().skip(1) {
                if !self.slots[head].completion_ready() && self.slots[index].completion_ready() && !self.reordered[index] {
                    self.reordered[index] = true; self.attachment.reordered();
                }
            }
        }
        while let Some(&index) = self.order.front() {
            if usb_stop_requested(stop) { break; }
            let Some((status, completed, _)) = self.slots[index].take_completion() else { break; };
            self.order.pop_front(); self.reordered[index] = false;
            let result = if self.feedforward.as_ref().map_or(true, |pool| pool.accepts_capture(completed)) {
                self.attachment.ingest(&self.slots[index], status, &mut self.scratch)
            } else {
                capture_stream::discard_unprimed_capture(&self.slots[index], status, &mut self.scratch,
                    self.attachment.endpoint.format.bytes_per_frame())
            };
            self.slots[index].mark_completed();
            result?;
            if !usb_stop_requested(stop) {
                self.slots[index].submit_capture(handle, self.attachment.endpoint.address, self.attachment.endpoint.packet_bytes)
                    .map_err(|e| if crate::error::is_device_gone(&e) { Error::DeviceLost } else { Error::Transfer(e) })?;
                self.order.push_back(index);
            }
        }
        Ok(())
    }
}

impl Drop for InputPool {
    fn drop(&mut self) { self.cancel(); }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock_resolution::ClockResolution;
    fn clock(control_interface: u8, source_id: u8) -> ClockChoice {
        ClockChoice { control_interface, source_id, path: vec![], resolution: ClockResolution::Resolved }
    }
    #[test]
    fn independently_resolved_paths_cannot_disagree_about_a_shared_selector() {
        use crate::clock_resolution::{ClockHop, ClockHopKind};
        let mut out = clock(0, 4); let mut input = clock(0, 5);
        out.path.push(ClockHop { id: 8, kind: ClockHopKind::Selector { selected_pin: 1, pin_count: 2 } });
        input.path.push(ClockHop { id: 8, kind: ClockHopKind::Selector { selected_pin: 2, pin_count: 2 } });
        assert!(clock_pair(Some(&out), Some(&input), 48_000, 96_000).is_err());
    }
    #[test]
    fn retirement_acknowledgment_belongs_to_the_pair() {
        let retired = Arc::new(AtomicBool::new(false));
        let guard = Retirement(retired.clone());
        assert!(!retired.load(Ordering::Acquire));
        drop(guard);
        assert!(retired.load(Ordering::Acquire));
    }
    #[test]
    fn shared_sources_accept_different_wire_cadences_only_at_the_same_source_rate() {
        use crate::clock_resolution::{ClockHop, ClockHopKind, RateRatio};
        let mut output = clock(0, 4);
        output.path.push(ClockHop { id: 8, kind: ClockHopKind::RateMultiplier { numerator: 2, denominator: 1 } });
        let input = clock(0, 4);
        let output_source = RateRatio::from_factors(&[(2, 1)]).unwrap().source_rate(96_000).unwrap();
        assert!(clock_pair(Some(&output), Some(&input), output_source, 48_000).is_ok());
        // Equal wire rates can conflict when only one path multiplies them.
        let output_source = RateRatio::from_factors(&[(2, 1)]).unwrap().source_rate(48_000).unwrap();
        assert!(clock_pair(Some(&output), Some(&input), output_source, 48_000).is_err());
    }

    #[test]
    fn shared_clocks_refuse_different_rates_independent_sources_preserve_them() {
        let a = clock(0, 4); let b = clock(0, 5); let c = clock(2, 4);
        assert!(clock_pair(Some(&a), Some(&a), 48_000, 48_000).is_ok());
        assert!(clock_pair(Some(&a), Some(&a), 48_000, 96_000).is_err());
        assert!(clock_pair(Some(&a), Some(&b), 48_000, 96_000).is_ok());
        assert!(clock_pair(Some(&a), Some(&c), 48_000, 96_000).is_ok());
        assert!(clock_pair(None, Some(&a), 48_000, 48_000).is_err());
        let fallback = ClockChoice { resolution: ClockResolution::FallbackFirstSource, ..a.clone() };
        assert!(clock_pair(Some(&fallback), Some(&b), 48_000, 48_000).is_err());
    }
}

/// Pair an already negotiated output with an exact capture request under the
/// one session gate. Refuse stale negotiation instead of changing a clock
/// before the capture endpoint has been selected.
pub(crate) fn open(expected: &crate::StreamFormat, request: &CaptureRequest, capacity: usize,
    events: EventSink) -> Result<(StreamHandle, CaptureHandle), Error> {
    if !api::negotiation_matches(expected) {
        return Err(Error::InvalidState("duplex playback negotiation is stale; negotiate again".into()));
    }
    let mut output = stream_open::start_stream_with_capture(events, expected, Some((request, capacity)))?;
    let capture = output.capture.take().expect("duplex start returns capture attachment");
    Ok((output, capture))
}
