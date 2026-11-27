//! A capture attachment to the playback worker, never a second USB event owner.
use super::*;
use crate::capture::{CaptureBuffer, CapturePacket, CaptureRequest};
use crate::error::OrControl;
use super::capture_stream::{CaptureEndpoint, CaptureHandle};
use std::collections::VecDeque;

pub(super) struct Attachment {
    pub(super) endpoint: CaptureEndpoint,
    pub(super) shared_implicit: bool,
    buffer: Arc<Mutex<CaptureBuffer>>,
    error: Arc<Mutex<Option<Error>>>,
    retired: Arc<AtomicBool>,
    deferred: Arc<capture_stream::DeferredCapture>,
    original_rate: Mutex<Option<(ClockChoice, u32, u32)>>,
    uac1_rate: Mutex<Option<Uac1DuplexRate>>,
    rate_uncertain: AtomicBool,
    retry_endpoints: Vec<CaptureEndpoint>,
    capacity: usize,
    lock_delay: Option<endpoint_lock::LockDelay>,
}

#[derive(Clone)]
struct Uac1EndpointRate {
    interface: u8,
    address: u8,
    original: u32,
    target: u32,
    observed: Option<u32>,
    may_have_changed: bool,
}

#[derive(Clone)]
struct Uac1DuplexRate {
    playback: IsoStreamCandidate,
    expected: crate::StreamFormat,
    endpoints: Vec<Uac1EndpointRate>,
    // A SET_CUR may change either endpoint in the same function, even when
    // it fails or the other endpoint did not need an explicit write.
    mutated: bool,
    playback_owner: u8,
    shared_function: bool,
}

fn uac1_pair_rates(output_owner: u8, input_owner: u8, output: u32, input: u32) -> Result<(), Error> {
    if output_owner == input_owner && output != input {
        return Err(Error::Unsupported { reason: "UAC1 duplex in one audio function requires equal rates".into(), offered: vec![] });
    }
    Ok(())
}

fn uac1_endpoint_owned(endpoint: &Uac1EndpointRate, current: u32) -> bool {
    uac1_restore_owned(endpoint.original, endpoint.target, current) || endpoint.observed == Some(current)
}

fn uac1_restore_owned(original: u32, target: u32, current: u32) -> bool {
    current == original || current == target
}

fn uac1_pair_profile(claimed: &ClaimedHandle, playback: &IsoStreamCandidate,
    expected: &crate::StreamFormat, endpoint: &CaptureEndpoint) -> Result<(Uac1PlaybackRateProfile, bool, u8), Error> {
    if !playback.is_uac1 || endpoint.protocol != 0 || expected.clock.is_some()
        || expected.encoding != crate::Encoding::Pcm || playback.format_tag != 1
        || playback.native_dsd.is_some() || endpoint.vendor_pcm.is_some() || endpoint.adc3_route.is_some()
        || playback.implicit_feedback.is_some() || validate_alternates(playback, endpoint)? {
        return Err(Error::Unsupported { reason: "duplex UAC1 requires independent standard PCM AudioStreaming alternates".into(), offered: vec![] });
    }
    if endpoint.feedforward().is_some() && settings().disable_feedback() {
        return Err(Error::Unsupported { reason: "adaptive UAC1 duplex capture requires feedforward timing".into(), offered: vec![] });
    }
    let owner = capture_stream::uac1_duplex_owners(claimed, playback, endpoint, expected.sample_rate)?;
    uac1_pair_rates(owner, endpoint.control, expected.sample_rate, endpoint.format.sample_rate())?;
    let profile = uac1_playback_rate_profile(claimed, playback, expected.sample_rate).map_err(Error::Control)?;
    if profile.sampling_control != Some(true)
        && !uac1_playback_rate_verified(expected.sample_rate, None, profile, true) {
        return Err(Error::Unsupported { reason: "UAC1 duplex OUT has neither explicit rate control nor a fixed or host-paced rate contract".into(), offered: vec![] });
    }
    let input_control = capture_stream::uac1_duplex_capture_sampling_control(endpoint)?;
    Ok((profile, input_control, owner))
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
    if (playback.is_uac1 || input.protocol == 0) && playback.interface_number == input.interface {
        return Err(Error::Unsupported { reason: "UAC1 duplex requires separate AudioStreaming interfaces".into(), offered: vec![] });
    }
    if playback.interface_number == input.interface && playback.alt_setting != input.alternate {
        return Err(Error::Unsupported { reason: "duplex alternates conflict on one interface".into(), offered: vec![] });
    }
    if input.feedforward()
        .is_some_and(|pipe| pipe.address == playback.endpoint_address) {
        return Err(Error::Unsupported { reason: "duplex playback overlaps capture feedforward OUT ownership".into(), offered: vec![] });
    }
    if playback.endpoint_address == input.address {
        return Err(Error::Unsupported { reason: "duplex endpoint directions overlap".into(), offered: vec![] });
    }
    if playback.feedback_endpoint.is_some_and(|feedback| feedback.address == input.address) {
        return Err(Error::Unsupported { reason: "capture overlaps playback explicit feedback IN ownership".into(), offered: vec![] });
    }
    if let Some(source) = playback.implicit_feedback {
        if source.interface_number == input.interface && source.alt_setting != input.alternate {
            return Err(Error::Unsupported { reason: "capture alternate conflicts with implicit feedback".into(), offered: vec![] });
        }
        if source.address == input.address {
            // An adaptive source consumes host timing on its feedforward OUT;
            // it cannot also provide the asynchronous clock playback owns.
            if input.feedforward().is_some() {
                return Err(Error::Unsupported { reason: "capture requiring feedforward cannot share the owned implicit IN clock".into(), offered: vec![] });
            }
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
    if playback.is_uac1 || endpoint.protocol == 0 {
        uac1_pair_profile(claimed, playback, expected, endpoint)?;
        return Ok(());
    }
    if let Some(profile) = endpoint.vendor_pcm.as_ref() {
        capture_stream::verify_vendor_capture(claimed, endpoint)?;
        let proven = vendor_pcm::for_candidate(&claimed.handle, playback).map_err(Error::Control)?;
        let exact = proven.as_ref() == Some(profile)
            && profile.supports_rate(expected.sample_rate) && expected.clock.is_none()
            && endpoint.format.sample_rate() == expected.sample_rate
            && validate_alternates(playback, endpoint)?;
        if exact { return Ok(()); }
        return Err(Error::Unsupported { reason: "duplex must match the exact vendor OUT and shared implicit IN profile".into(), offered: vec![] });
    }
    let route = adc3_route_for_candidate(&claimed.handle.device(), &claimed.handle, playback).map_err(Error::Control)?;
    if let (Some(output), Some(input)) = (route.as_ref(), endpoint.adc3_route.as_ref()) {
        adc3::validate_adaptive_duplex(playback, output, expected.sample_rate,
            input, endpoint.format.sample_rate()).map_err(Error::Control)?;
        // BADD prescribes a common fixed 48 kHz cadence for both directions
        // within one AudioControl function, without programmable Clock Sources.
        if output.clock.is_none() && input.clock.is_none() && output.control == input.control
            && expected.sample_rate == 48_000 && endpoint.format.sample_rate() == 48_000 {
            return Ok(());
        }
    }
    let output = route.as_ref().and_then(|route| route.choice.as_ref()).or(expected.clock.as_ref());
    let input = input_clock(claimed, endpoint);
    let output_rate = if let Some(route) = route.as_ref() {
        route.source_rate(expected.sample_rate).map_err(Error::Control)?
    } else {
        uac2_source_rate(output, expected.sample_rate)?
    };
    let input_rate = capture_source_rate(endpoint, input.as_ref())?;
    clock_pair(output, input.as_ref(), output_rate, input_rate)
}

fn capture_source_rate(endpoint: &CaptureEndpoint, choice: Option<&ClockChoice>) -> Result<u32, Error> {
    if let Some(route) = endpoint.adc3_route.as_ref() {
        route.source_rate(endpoint.format.sample_rate()).map_err(Error::Control)
    } else {
        uac2_source_rate(choice, endpoint.format.sample_rate())
    }
}

fn uac2_source_rate(choice: Option<&ClockChoice>, wire_rate: u32) -> Result<u32, Error> {
    let ratio = uac2_clock_ratio(choice).map_err(Error::Control)?;
    ratio.source_rate(wire_rate).ok_or_else(|| Error::Unsupported {
        reason: "duplex wire rate has no exact representable root source rate".into(),
        offered: vec![],
    })
}

fn retryable_capture(endpoint: &CaptureEndpoint) -> bool {
    endpoint.vendor_pcm.is_none() && (endpoint.protocol == 0x20
        || (endpoint.protocol == 0x30 && endpoint.adc3_route.as_ref().is_some_and(|route| route.choice.is_some())))
}

/// Retry only descriptor alternatives of the originally selected input route.
fn same_retry_route(primary: &CaptureEndpoint, candidate: &CaptureEndpoint) -> bool {
    retryable_capture(candidate)
        && (primary.configuration, primary.protocol, primary.interface, primary.control, primary.terminal)
            == (candidate.configuration, candidate.protocol, candidate.interface, candidate.control, candidate.terminal)
}

fn prepare_endpoint(claimed: &ClaimedHandle, playback: &IsoStreamCandidate,
    expected: &crate::StreamFormat, endpoint: CaptureEndpoint, capacity: usize) -> Result<Attachment, Error> {
    let shared = validate_alternates(playback, &endpoint)?;
    if !shared {
        let geometry = IsoQueueGeometry::for_stream(endpoint.service_us,
            settings().config.bus_queue_ms, settings().config.transfers_in_flight);
        capture_stream::capture_queue_bytes(endpoint.packet_bytes, geometry)?;
    }
    validate_clocks(claimed, playback, expected, &endpoint)?;
    let delay = if shared || endpoint.vendor_pcm.is_some() { None } else {
        endpoint_lock::selected_delay(claimed, endpoint.interface, endpoint.alternate,
            endpoint.address, true).or_control()?
    };
    let buffer = CaptureBuffer::new(endpoint.format.clone(), capacity)
        .map_err(|e| Error::InvalidArgument(format!("capture buffer: {e:?}")))?;
    let mut attachment = Attachment::new(endpoint, shared, buffer);
    attachment.lock_delay = delay;
    attachment.capacity = capacity;
    Ok(attachment)
}

pub(super) fn prepare(claimed: &ClaimedHandle, playback: &IsoStreamCandidate,
    expected: &crate::StreamFormat, request: &CaptureRequest, capacity: usize) -> Result<Arc<Attachment>, Error> {
    let config = claimed.handle.device().active_config_descriptor().map_err(|e| Error::Claim(e.to_string()))?;
    let mut endpoints = capture_stream::owned_capture_candidates(claimed, &config, request.sample_rate, true).map_err(Error::Claim)?;
    endpoints.retain(|endpoint| endpoint.format.satisfies(request)
        && claimed.claimed_interfaces.contains(&endpoint.interface)
        && claimed.claimed_interfaces.contains(&endpoint.control));
    endpoints.sort_by_key(|e| (e.format.subslot_bytes(), e.interface, e.alternate, e.address));
    let mut endpoints = endpoints.into_iter();
    let mut refusal = None;
    while let Some(endpoint) = endpoints.next() {
        match prepare_endpoint(claimed, playback, expected, endpoint, capacity) {
            Ok(mut attachment) => {
                if !attachment.shared_implicit && retryable_capture(&attachment.endpoint) {
                    attachment.retry_endpoints = endpoints.filter(|candidate|
                        same_retry_route(&attachment.endpoint, candidate)).collect();
                }
                return Ok(Arc::new(attachment));
            },
            Err(Error::DeviceLost) => return Err(Error::DeviceLost),
            Err(error) => refusal = Some(error),
        }
    }
    Err(refusal.unwrap_or_else(|| Error::Unsupported { reason: "no claimed PCM capture alternate matches the duplex request".into(), offered: vec![] }))
}

/// Publish each attempted attachment before any clock/alternate mutation so
/// stream-open cleanup always owns that attempt's rollback snapshot.
pub(super) fn configure_with_retry(attachment: &mut Arc<Attachment>, claimed: &ClaimedHandle,
    playback: &IsoStreamCandidate, expected: &crate::StreamFormat) -> Result<(), Error> {
    let mut alternatives = None;
    let capacity = attachment.capacity;
    let input = if attachment.retry_endpoints.is_empty() { None } else { input_clock(claimed, &attachment.endpoint) };
    loop {
        if attachment.configure(claimed, playback, expected)? { return Ok(()); }
        if !attachment.restore_rate(claimed) {
            return Err(Error::Control("capture clock rollback failed before alternate retry".into()));
        }
        let endpoint = alternatives.get_or_insert_with(|| attachment.retry_endpoints.clone().into_iter())
            .next().ok_or_else(|| Error::Unsupported {
            reason: "no matching duplex PCM IN alternate is currently available at the verified capture rate".into(),
            offered: vec![],
        })?;
        // Playback and its implicit source remain pinned. Recheck the live
        // pairing before allocating or publishing the next independent IN plan.
        if validate_alternates(playback, &endpoint)? || input_clock(claimed, &endpoint) != input {
            return Err(Error::Unsupported { reason: "duplex retry changed the selected capture clock or implicit endpoint".into(), offered: vec![] });
        }
        let next = prepare_endpoint(claimed, playback, expected, endpoint, capacity)?;
        *attachment = Arc::new(next);
    }
}

impl Attachment {
    pub(super) fn new(endpoint: CaptureEndpoint, shared_implicit: bool, buffer: CaptureBuffer) -> Self {
        Self { endpoint, shared_implicit, buffer: Arc::new(Mutex::new(buffer)),
            error: Arc::new(Mutex::new(None)), retired: Arc::new(AtomicBool::new(false)),
            deferred: Arc::new(capture_stream::DeferredCapture::default()), original_rate: Mutex::new(None), uac1_rate: Mutex::new(None), rate_uncertain: AtomicBool::new(false),
            retry_endpoints: Vec::new(), capacity: 0, lock_delay: None }
    }
    fn verify_uac1_pair(&self, claimed: &ClaimedHandle, playback: &IsoStreamCandidate,
        expected: &crate::StreamFormat) -> Result<(Uac1PlaybackRateProfile, bool, u8), Error> {
        let profile = uac1_pair_profile(claimed, playback, expected, &self.endpoint)?;
        self.verify_uac1_alternates(claimed, playback)?;
        Ok(profile)
    }

    fn verify_uac1_alternates(&self, claimed: &ClaimedHandle, playback: &IsoStreamCandidate) -> Result<(), Error> {
        capture_stream::verify_active_capture_alt(claimed, playback.interface_number, playback.alt_setting)?;
        capture_stream::verify_active_capture_alt(claimed, self.endpoint.interface, self.endpoint.alternate)?;
        Ok(())
    }

    /// Commit both endpoint clocks together, after OUT activation and before
    /// either worker can submit transfers. The attachment owns partial setup
    /// rollback as soon as the first potentially effective request is sent.
    pub(super) fn configure_uac1(&self, claimed: &ClaimedHandle, playback: &IsoStreamCandidate,
        expected: &crate::StreamFormat) -> Result<(bool, bool, Option<u32>), Error> {
        if self.uac1_rate.lock().is_some() {
            return Err(Error::InvalidState("UAC1 duplex rate transaction already configured".into()));
        }
        let (_, _, initial_owner) = uac1_pair_profile(claimed, playback, expected, &self.endpoint)?;
        claimed.set_alternate_setting(self.endpoint.interface, self.endpoint.alternate).map_err(Error::Claim)?;
        let (profile, input_control, owner) = self.verify_uac1_pair(claimed, playback, expected)?;
        if owner != initial_owner {
            return Err(Error::InvalidState("UAC1 duplex playback function changed during activation".into()));
        }
        let mut endpoints = Vec::new();
        for (programmable, interface, address, target) in [
            (profile.sampling_control == Some(true), playback.interface_number, playback.endpoint_address, expected.sample_rate),
            (input_control, self.endpoint.interface, self.endpoint.address, self.endpoint.format.sample_rate()),
        ] {
            if programmable {
                // Both original positive GET_CUR values must exist before any
                // writes; a write-only control cannot be handed back safely.
                let original = get_uac1_sampling_frequency(&claimed.handle, interface, address).or_control()?;
                endpoints.push(Uac1EndpointRate { interface, address, original, target, observed: None, may_have_changed: false });
            }
        }
        *self.uac1_rate.lock() = Some(Uac1DuplexRate {
            playback: playback.clone(), expected: expected.clone(), endpoints: endpoints.clone(), mutated: false,
            playback_owner: owner,
            shared_function: owner == self.endpoint.control,
        });
        let mut output_attempted = false;
        let mut output_written = false;
        for endpoint in &endpoints {
            // A preceding OUT write may already have moved the shared IN root.
            // Recheck both active alternates before every control mutation.
            self.verify_uac1_alternates(claimed, playback)?;
            let current = get_uac1_sampling_frequency(&claimed.handle, endpoint.interface, endpoint.address).or_control()?;
            if current == endpoint.target { continue; }
            let observed = self.uac1_rate.lock().as_ref().expect("saved UAC1 transaction").endpoints.iter()
                .find(|saved| saved.address == endpoint.address).and_then(|saved| saved.observed);
            if current != endpoint.original && Some(current) != observed {
                return Err(Error::InvalidState("UAC1 duplex endpoint frequency changed outside the transaction".into()));
            }
            {
                let mut saved = self.uac1_rate.lock();
                let saved = saved.as_mut().expect("saved UAC1 transaction");
                saved.mutated = true;
                let shared_function = saved.shared_function;
                for other in &mut saved.endpoints {
                    if shared_function || other.address == endpoint.address {
                        other.may_have_changed = true;
                    }
                }
            }
            let output = endpoint.address == playback.endpoint_address;
            if output { output_attempted = true; }
            let write = set_uac1_sampling_frequency(&claimed.handle, endpoint.interface, endpoint.address, endpoint.target).or_control();
            if output { output_written = write.is_ok(); }
            if write.is_ok() {
                // Acknowledged UAC1 SET_CUR can round the request. Snapshot
                // both controls immediately, attributing common-root effects
                // to this transaction so failed setup can restore them too.
                for other in &endpoints {
                    if owner != self.endpoint.control && other.address != endpoint.address { continue; }
                    let reported = get_uac1_sampling_frequency(&claimed.handle, other.interface, other.address).or_control()?;
                    if let Some(saved) = self.uac1_rate.lock().as_mut().expect("saved UAC1 transaction").endpoints
                        .iter_mut().find(|saved| saved.address == other.address) {
                        saved.observed = Some(reported);
                    }
                }
            }
            // A stalled SET can still leave the required active rate. Only a
            // positive final GET_CUR, never the acknowledgement, commits it.
            if let Err(Error::DeviceLost) = write { return Err(Error::DeviceLost); }
        }
        if self.verify_uac1_pair(claimed, playback, expected)?.2 != owner {
            return Err(Error::InvalidState("UAC1 duplex playback function changed during rate setup".into()));
        }
        let output_reported = if profile.sampling_control == Some(true) {
            Some(get_uac1_sampling_frequency(&claimed.handle, playback.interface_number, playback.endpoint_address).or_control()?)
        } else { None };
        let input_reported = if input_control {
            Some(get_uac1_sampling_frequency(&claimed.handle, self.endpoint.interface, self.endpoint.address).or_control()?)
        } else { None };
        if !uac1_playback_rate_verified(expected.sample_rate, output_reported, profile, true) {
            return Err(Error::ClockUnverified { requested_hz: expected.sample_rate, reported_hz: output_reported });
        }
        let input_rate = self.endpoint.format.sample_rate();
        if input_reported.is_some_and(|rate| rate != input_rate) || (input_control && input_reported != Some(input_rate)) {
            return Err(Error::ClockUnverified { requested_hz: input_rate, reported_hz: input_reported });
        }
        Ok((output_attempted, output_written, output_reported))
    }

    fn restore_uac1_rate(&self, claimed: &ClaimedHandle) -> Result<(), Error> {
        let Some(mut transaction) = self.uac1_rate.lock().clone() else { return Ok(()); };
        if transaction.mutated {
            if self.verify_uac1_pair(claimed, &transaction.playback, &transaction.expected)?.2 != transaction.playback_owner {
                return Err(Error::InvalidState("UAC1 duplex playback function changed before rollback".into()));
            }
            let mut safe = vec![true; transaction.endpoints.len()];
            // Preflight the complete pair before changing either root. An
            // independently proven function can still be restored when its
            // sibling has drifted, but a shared function must remain intact.
            for (index, endpoint) in transaction.endpoints.iter().enumerate() {
                match get_uac1_sampling_frequency(&claimed.handle, endpoint.interface, endpoint.address).or_control() {
                    Ok(current) => safe[index] = uac1_endpoint_owned(endpoint, current),
                    Err(Error::DeviceLost) => return Err(Error::DeviceLost),
                    Err(_) => safe[index] = false,
                }
            }
            if transaction.shared_function && safe.contains(&false) {
                return Err(Error::InvalidState("UAC1 duplex rollback preserved an external endpoint frequency".into()));
            }
            for index in 0..transaction.endpoints.len() {
                if !safe[index] || !transaction.endpoints[index].may_have_changed { continue; }
                self.verify_uac1_alternates(claimed, &transaction.playback)?;
                if transaction.shared_function {
                    for other in &transaction.endpoints {
                        let current = get_uac1_sampling_frequency(&claimed.handle, other.interface, other.address).or_control()?;
                        if !uac1_endpoint_owned(other, current) {
                            return Err(Error::InvalidState("UAC1 duplex frequency drifted during rollback".into()));
                        }
                    }
                }
                let endpoint = &transaction.endpoints[index];
                let current = match get_uac1_sampling_frequency(&claimed.handle, endpoint.interface, endpoint.address).or_control() {
                    Ok(current) => current,
                    Err(Error::DeviceLost) => return Err(Error::DeviceLost),
                    Err(_) => { safe[index] = false; continue; },
                };
                if !uac1_endpoint_owned(endpoint, current) { safe[index] = false; continue; }
                if current != endpoint.original {
                    let restoring_address = endpoint.address;
                    let write = set_uac1_sampling_frequency(&claimed.handle, endpoint.interface, endpoint.address, endpoint.original).or_control();
                    if let Err(Error::DeviceLost) = write { return Err(Error::DeviceLost); }
                    if write.is_ok() {
                        for other in &mut transaction.endpoints {
                            if !other.may_have_changed || (!transaction.shared_function && other.address != restoring_address) { continue; }
                            match get_uac1_sampling_frequency(&claimed.handle, other.interface, other.address).or_control() {
                                Ok(reported) => other.observed = Some(reported),
                                Err(Error::DeviceLost) => return Err(Error::DeviceLost),
                                Err(_) => {},
                            }
                        }
                    }
                }
            }
            if self.verify_uac1_pair(claimed, &transaction.playback, &transaction.expected)?.2 != transaction.playback_owner {
                return Err(Error::InvalidState("UAC1 duplex playback function changed during rollback".into()));
            }
            // Verify after all restorations: a later request can affect an
            // earlier endpoint through an undisclosed common UAC1 clock root.
            for (index, endpoint) in transaction.endpoints.iter().enumerate() {
                match get_uac1_sampling_frequency(&claimed.handle, endpoint.interface, endpoint.address).or_control() {
                    Ok(reported) if reported == endpoint.original => {},
                    Err(Error::DeviceLost) => return Err(Error::DeviceLost),
                    _ => safe[index] = false,
                }
            }
            if safe.contains(&false) {
                return Err(Error::InvalidState("UAC1 duplex endpoint restoration did not verify; external frequencies preserved".into()));
            }
        }
        *self.uac1_rate.lock() = None;
        Ok(())
    }
    fn configure(&self, claimed: &ClaimedHandle, playback: &IsoStreamCandidate,
        expected: &crate::StreamFormat) -> Result<bool, Error> {
        validate_clocks(claimed, playback, expected, &self.endpoint)?;
        if self.endpoint.vendor_pcm.is_some() {
            // Playback has activated both exact alternates; capture joins the
            // existing implicit IN pool and never opens another companion OUT.
            let profile = self.endpoint.vendor_pcm.as_ref().expect("vendor profile");
            if profile.interface_number != self.endpoint.interface {
                capture_stream::verify_vendor_interface_alternate(claimed, profile.interface_number, profile.alt_setting)?;
            }
            capture_stream::verify_vendor_capture_alternate(claimed, &self.endpoint, self.endpoint.alternate)?;
            return Ok(true);
        }
        if self.shared_implicit {
            if self.endpoint.adc3_route.is_none() {
                capture_stream::verify_standard_capture(claimed, &self.endpoint)?;
            }
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
                verify_uac2_clock_factors(&claimed.handle, &clock).map_err(Error::Control)?;
                get_wire_frequency(&claimed.handle, AudioControlClock {
                    interface_number: clock.control_interface, clock_id: clock.source_id,
                }, Some(&clock)).ok()
            };
            if reported != Some(self.endpoint.format.sample_rate()) {
                return Err(Error::ClockUnverified { requested_hz: self.endpoint.format.sample_rate(), reported_hz: reported });
            }
        } else {
            let mut expected_source = None;
            if let Some(clock) = input_clock(claimed, &self.endpoint) {
                let original = get_sampling_frequency(&claimed.handle, clock.control_interface, clock.source_id).map_err(Error::Control)?;
                let target = capture_source_rate(&self.endpoint, Some(&clock))?;
                let shared_root = expected.clock.as_ref().is_some_and(|output|
                    (output.control_interface, output.source_id) == (clock.control_interface, clock.source_id));
                if shared_root && original != target {
                    return Err(Error::InvalidState("shared duplex capture source changed from the committed playback rate".into()));
                }
                if self.endpoint.adc3_route.is_none() {
                    verify_uac2_clock_factors(&claimed.handle, &clock).map_err(Error::Control)?;
                }
                if self.endpoint.adc3_route.is_none() || shared_root {
                    expected_source = Some((clock.clone(), original));
                }
                if !shared_root && original != target {
                    *self.original_rate.lock() = Some((clock, original, target));
                }
            }
            if !capture_stream::configure_capture_endpoint_guarded(claimed, &self.endpoint,
                expected_source.as_ref().map(|(choice, original)| (choice, *original)))? {
                return Ok(false);
            }
        }
        let current = candidate_audio_control_clock_choice(&claimed.handle.device(), &claimed.handle, playback)
            .map_err(Error::Control)?;
        if current.as_ref().map(|(_, choice)| choice) != expected.clock.as_ref() {
            return Err(Error::InvalidState("playback clock route changed while configuring duplex capture".into()));
        }
        validate_clocks(claimed, playback, expected, &self.endpoint)?;
        Ok(true)
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
    fn discard_before_timing(&self, slot: &IsoTransferSlot, status: i32,
        scratch: &mut [CapturePacket]) -> Result<(), Error> {
        let mut stats = crate::capture::CaptureStats::default();
        let result = match capture_stream::inspect_slot(slot, status, scratch) {
            Ok((data, detached)) => {
                let result = stats.record_completion(data.len(), &slot.packet_sizes, scratch,
                    true, 0, self.endpoint.format.bytes_per_frame())
                    .map_err(|e| Error::Transfer(format!("capture startup completion: {e:?}")));
                if detached { Err(Error::DeviceLost) } else { result }
            }
            Err(error) => {
                if status == LIBUSB_TRANSFER_COMPLETED { stats.malformed_completions = 1; }
                else { stats.failed_transfers = 1; }
                Err(error)
            }
        };
        self.deferred.record(stats);
        if let Err(error) = &result { *self.error.lock() = Some(error.clone()); }
        result
    }
    pub(super) fn failure(&self) -> Option<Error> { self.error.lock().clone() }
    pub(super) fn retirement_guard(&self) -> Retirement { Retirement(self.retired.clone()) }
    fn restore_rate(&self, claimed: &ClaimedHandle) -> bool {
        if let Err(error) = self.restore_uac1_rate(claimed) {
            self.rate_uncertain.store(true, Ordering::Release);
            set_last_error(Some(format!("UAC1 duplex clock rollback failed: {}", error.message())));
            return false;
        }
        let Some((clock, rate, target)) = self.original_rate.lock().clone() else {
            return !self.rate_uncertain.load(Ordering::Acquire);
        };
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
                } else {
                    if clock.path.iter().any(|hop| matches!(hop.kind,
                        crate::clock_resolution::ClockHopKind::RateMultiplier { .. }))
                        && !clock::stop_complete_capture_clock_function(claimed, self.endpoint.interface)? {
                        return Err("capture multiplier rollback requires complete idle function ownership".into());
                    }
                    verify_uac2_clock_factors(&claimed.handle, &clock)?;
                    if input_clock(claimed, &self.endpoint).as_ref() != Some(&clock) {
                        return Err("capture clock route drifted; preserving external frequency".into());
                    }
                }
                let current = get_sampling_frequency(&claimed.handle, clock.control_interface, clock.source_id)?;
                if current == rate { return Ok(()); }
                if current != target {
                    return Err("capture source drifted; preserving external frequency".into());
                }
                set_sampling_frequency(&claimed.handle, clock.control_interface, clock.source_id, rate)
            })
            .and_then(|()| {
                if self.endpoint.adc3_route.is_none() {
                    verify_uac2_clock_factors(&claimed.handle, &clock)?;
                    if input_clock(claimed, &self.endpoint).as_ref() != Some(&clock) {
                        return Err("capture clock route changed during rollback".into());
                    }
                }
                if get_sampling_frequency(&claimed.handle, clock.control_interface, clock.source_id)? == rate { Ok(()) }
                else { Err("duplex capture clock rollback did not verify".into()) }
            });
        if let Err(error) = result {
            // Keep the owning ADC3 function awake when a distinct capture
            // source could not be handed back after callback retirement.
            self.rate_uncertain.store(true, Ordering::Release);
            claimed.adc3_power.disarm();
            set_last_error(Some(format!("capture clock rollback failed: {error}")));
            return false;
        }
        *self.original_rate.lock() = None;
        !self.rate_uncertain.load(Ordering::Acquire)
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

/// Capture timing is authorized by wholly accepted playback callbacks only.
/// Once a deadline is missed, a late callback cannot restart the same session.
struct ImplicitFeedforwardHealth {
    submitted: Option<Instant>,
    first: Option<Instant>,
    latest: Option<Instant>,
    startup: Duration,
    fresh: Duration,
    failed: bool,
}
impl ImplicitFeedforwardHealth {
    fn new() -> Self {
        Self { submitted: None, first: None, latest: None,
            startup: Duration::from_millis(500), fresh: Duration::from_millis(100), failed: false }
    }
    fn submitted(&mut self, at: Instant, duration: Duration) {
        if self.submitted.is_some() || self.failed { return; }
        self.submitted = Some(at);
        self.startup = Duration::from_millis(500).max(duration.saturating_mul(2))
            .min(Duration::from_secs(1));
        self.fresh = Duration::from_millis(100).max(duration.saturating_mul(2))
            .min(Duration::from_millis(500));
    }
    fn check(&mut self, at: Instant) -> Result<(), Error> {
        if self.latest.map_or_else(|| self.submitted.is_some_and(|start|
            at.saturating_duration_since(start) >= self.startup), |latest|
            at.saturating_duration_since(latest) >= Duration::from_secs(1)) {
            self.failed = true;
        }
        if self.failed { Err(Error::Transfer("ADC3 implicit feedforward playback timing lost".into())) }
        else { Ok(()) }
    }
    fn completed(&mut self, at: Instant) -> Result<(), Error> {
        self.check(at)?;
        if self.submitted.is_some_and(|start| at >= start) {
            if self.latest.is_some_and(|latest| at.saturating_duration_since(latest) >= self.fresh) {
                // Never admit a buffered IN completion from a stale timing gap
                // merely because a later callback has restored OUT freshness.
                self.first = Some(at);
            } else { self.first.get_or_insert(at); }
            self.latest = Some(self.latest.map_or(at, |latest| latest.max(at)));
        }
        Ok(())
    }
    fn accepts(&self, at: Instant) -> bool {
        !self.failed && self.first.is_some_and(|first| at >= first)
            && self.latest.is_some_and(|latest| at.saturating_duration_since(latest) < self.fresh)
    }
}

pub(super) struct InputPool {
    pub(super) slots: Vec<IsoTransferSlot>,
    feedforward: Option<capture_stream::FeedforwardPool>,
    implicit_feedforward: Option<ImplicitFeedforwardHealth>,
    attachment: Arc<Attachment>,
    order: VecDeque<usize>,
    reordered: Vec<bool>,
    scratch: Vec<CapturePacket>,
    lock_delay: endpoint_lock::LockDelayGate,
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
        let implicit_feedforward = attachment.endpoint.adc3_route.as_ref()
            .is_some_and(|route| route.implicit_feedforward).then(ImplicitFeedforwardHealth::new);
        let lock_delay = endpoint_lock::LockDelayGate::new(attachment.lock_delay);
        Ok(Self { slots, feedforward, implicit_feedforward, lock_delay, attachment, order: VecDeque::with_capacity(geometry.transfer_slots),
            reordered: vec![false; geometry.transfer_slots], scratch: vec![CapturePacket {
                requested_bytes: 0, actual_bytes: 0, succeeded: false }; geometry.packets_per_transfer] })
    }
    pub(super) fn start(&mut self, handle: &DeviceHandle<Context>, stop: &AtomicBool) -> Result<(), Error> {
        if let Some(pool) = self.feedforward.as_mut() { pool.start(handle, stop)?; }
        for (index, slot) in self.slots.iter_mut().enumerate() {
            if usb_stop_requested(stop) { break; }
            slot.submit_capture(handle, self.attachment.endpoint.address, self.attachment.endpoint.packet_bytes, None)
                .map_err(|e| if crate::error::is_device_gone(&e) { Error::DeviceLost } else { Error::Transfer(e) })?;
            self.order.push_back(index);
        }
        Ok(())
    }
    pub(super) fn primed(&self) -> bool { self.feedforward.as_ref().map_or(true, |pool| pool.primed()) }
    pub(super) fn uses_implicit_feedforward(&self) -> bool { self.implicit_feedforward.is_some() }
    pub(super) fn note_playback_submitted(&mut self, at: Instant, transfer_duration: Duration) {
        if let Some(health) = self.implicit_feedforward.as_mut() { health.submitted(at, transfer_duration); }
    }
    pub(super) fn note_playback_completed(&mut self, at: Instant) -> Result<(), Error> {
        if let Some(health) = self.implicit_feedforward.as_mut() { health.completed(at)?; }
        Ok(())
    }
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
        if usb_stop_requested(stop) { return Ok(()); }
        if let Some(pool) = self.feedforward.as_mut() { pool.pump(handle, stop)?; }
        if let Some(health) = self.implicit_feedforward.as_mut() { health.check(Instant::now())?; }
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
            let timing_allowed = self.feedforward.as_ref().map_or(true, |pool|
                pool.accepts_capture_transfer(completed, self.slots[index].submitted_at))
                && self.implicit_feedforward.as_ref().map_or(true, |health| health.accepts(completed));
            let result = if self.lock_delay.pending() {
                let whole_urb_allowed = timing_allowed && self.slots[index].submitted_at.is_some_and(|submitted|
                    self.feedforward.as_ref().map_or(true, |pool| pool.accepts_capture(submitted))
                    && self.implicit_feedforward.as_ref().map_or(true, |health| health.accepts(submitted)));
                let (stats, result) = capture_stream::discard_lock_delay_capture(&self.slots[index], status,
                    &mut self.scratch, self.attachment.endpoint.format.bytes_per_frame(),
                    self.attachment.endpoint.service_us, whole_urb_allowed, &mut self.lock_delay);
                self.attachment.deferred.record(stats);
                if let Err(error) = &result { *self.attachment.error.lock() = Some(error.clone()); }
                result
            } else if timing_allowed {
                self.attachment.ingest(&self.slots[index], status, &mut self.scratch)
            } else {
                self.attachment.discard_before_timing(&self.slots[index], status, &mut self.scratch)
            };
            self.slots[index].mark_completed();
            result?;
            if !usb_stop_requested(stop) {
                self.slots[index].submit_capture(handle, self.attachment.endpoint.address, self.attachment.endpoint.packet_bytes, None)
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
    #[test]
    fn uac1_rates_require_a_common_cadence_only_within_the_same_proven_function() {
        assert!(uac1_pair_rates(0, 0, 48_000, 48_000).is_ok());
        assert!(uac1_pair_rates(0, 0, 48_000, 44_100).is_err());
        assert!(uac1_pair_rates(0, 3, 48_000, 44_100).is_ok());
    }
    #[test]
    fn uac1_rollback_preserves_frequency_drift_and_accepts_shared_root_effects() {
        // A sibling endpoint may already have restored a common root, or may
        // have moved it to the committed target without its own SET_CUR.
        assert!(uac1_restore_owned(44_100, 48_000, 44_100));
        assert!(uac1_restore_owned(44_100, 48_000, 48_000));
        assert!(!uac1_restore_owned(44_100, 48_000, 96_000));
        assert!(!uac1_restore_owned(44_100, 48_000, 0));
        let endpoint = Uac1EndpointRate { interface: 1, address: 1, original: 44_100,
            target: 48_000, observed: Some(47_999), may_have_changed: true };
        assert!(uac1_endpoint_owned(&endpoint, 47_999));
        assert!(!uac1_endpoint_owned(&endpoint, 47_998));
    }
    fn clock(control_interface: u8, source_id: u8) -> ClockChoice {
        ClockChoice { control_interface, source_id, path: vec![], resolution: ClockResolution::Resolved }
    }
    #[test]
    fn implicit_feedforward_drops_capture_until_accepted_playback_timing() {
        let at = Instant::now();
        let mut health = ImplicitFeedforwardHealth::new();
        assert!(!health.accepts(at));
        // Polling before OUT launch cannot consume the startup deadline.
        assert!(health.check(at + Duration::from_secs(30)).is_ok());
        health.submitted(at, Duration::from_millis(8));
        assert!(!health.accepts(at + Duration::from_millis(1)));
        let first = at + Duration::from_millis(8);
        health.completed(first).unwrap();
        assert!(!health.accepts(first - Duration::from_millis(1)));
        assert!(health.accepts(first));
        assert!(!health.accepts(first + Duration::from_millis(100)));
        // Out-of-order callbacks never move the freshness anchor backward.
        health.completed(at + Duration::from_millis(24)).unwrap();
        health.completed(at + Duration::from_millis(16)).unwrap();
        assert_eq!(health.latest, Some(at + Duration::from_millis(24)));
        assert!(health.accepts(at + Duration::from_millis(120)));
    }
    #[test]
    fn implicit_feedforward_recovery_drops_the_buffered_stale_gap() {
        let at = Instant::now();
        let mut health = ImplicitFeedforwardHealth::new();
        health.submitted(at, Duration::from_millis(8));
        health.completed(at + Duration::from_millis(8)).unwrap();
        assert!(health.accepts(at + Duration::from_millis(9)));
        health.completed(at + Duration::from_millis(300)).unwrap();
        assert!(!health.accepts(at + Duration::from_millis(200)));
        assert!(!health.accepts(at + Duration::from_millis(299)));
        assert!(health.accepts(at + Duration::from_millis(300)));
        // A late older callback cannot reopen the previous qualification epoch.
        health.completed(at + Duration::from_millis(16)).unwrap();
        assert!(!health.accepts(at + Duration::from_millis(200)));
        assert_eq!(health.latest, Some(at + Duration::from_millis(300)));
    }
    #[test]
    fn implicit_feedforward_startup_and_loss_are_terminal() {
        let at = Instant::now();
        let mut startup = ImplicitFeedforwardHealth::new();
        startup.submitted(at, Duration::from_millis(8));
        // Replenishing silence or a second submit cannot extend startup.
        startup.submitted(at + Duration::from_millis(400), Duration::from_millis(8));
        assert!(startup.check(at + Duration::from_millis(500)).is_err());
        assert!(startup.completed(at + Duration::from_millis(501)).is_err());
        assert!(!startup.accepts(at + Duration::from_millis(502)));
        let mut stale = ImplicitFeedforwardHealth::new();
        stale.submitted(at, Duration::from_millis(8));
        stale.completed(at + Duration::from_millis(8)).unwrap();
        assert!(stale.check(at + Duration::from_millis(1008)).is_err());
        assert!(stale.completed(at + Duration::from_millis(1009)).is_err());
        assert!(!stale.accepts(at + Duration::from_millis(1010)));
    }
    #[test]
    fn implicit_deadlines_account_for_composed_transfer_duration_with_a_bound() {
        let mut health = ImplicitFeedforwardHealth::new();
        health.submitted(Instant::now(), Duration::from_millis(300));
        assert_eq!(health.startup, Duration::from_millis(600));
        assert_eq!(health.fresh, Duration::from_millis(500));
        let mut sparse = ImplicitFeedforwardHealth::new();
        sparse.submitted(Instant::now(), Duration::from_secs(10));
        assert_eq!(sparse.startup, Duration::from_secs(1));
        assert_eq!(sparse.fresh, Duration::from_millis(500));
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
    fn capture_root_conversion_requires_proven_factors() {
        use crate::clock_resolution::{ClockHop, ClockHopKind};
        let mut input = clock(0, 4);
        input.path.push(ClockHop { id: 8, kind: ClockHopKind::Multiplier });
        assert!(uac2_clock_ratio(Some(&input)).is_err());
        input.path[0].kind = ClockHopKind::RateMultiplier { numerator: 2, denominator: 1 };
        assert_eq!(uac2_clock_ratio(Some(&input)).unwrap().source_rate(96_000).unwrap(), 48_000);
        assert!(uac2_clock_ratio(Some(&input)).unwrap().source_rate(44_101).is_none());
    }
    #[test]
    fn shared_factor_snapshots_must_agree_even_at_equal_effective_ratio() {
        use crate::clock_resolution::{ClockHop, ClockHopKind};
        let mut output = clock(0, 4);
        let mut input = clock(0, 4);
        output.path.push(ClockHop { id: 8, kind: ClockHopKind::RateMultiplier { numerator: 2, denominator: 2 } });
        input.path.push(ClockHop { id: 8, kind: ClockHopKind::RateMultiplier { numerator: 1, denominator: 1 } });
        assert!(clock_pair(Some(&output), Some(&input), 48_000, 48_000).is_err());
    }
    #[test]
    fn shared_sources_accept_different_wire_cadences_only_at_the_same_source_rate() {
        use crate::clock_resolution::{ClockHop, ClockHopKind};
        let mut output = clock(0, 4);
        output.path.push(ClockHop { id: 8, kind: ClockHopKind::RateMultiplier { numerator: 2, denominator: 1 } });
        let input = clock(0, 4);
        let output_source = uac2_clock_ratio(Some(&output)).unwrap().source_rate(96_000).unwrap();
        assert!(clock_pair(Some(&output), Some(&input), output_source, 48_000).is_ok());
        // Equal wire rates can conflict when only one path multiplies them.
        let output_source = uac2_clock_ratio(Some(&output)).unwrap().source_rate(48_000).unwrap();
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
