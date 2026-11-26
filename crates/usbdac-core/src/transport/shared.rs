use super::*;
use crate::error::Error;
use crate::events::{DacEvent, EventSink, StreamState};

/// Everything the host-facing stream handle, the output thread and the event
/// bridge share: the ring, the counters and the lifecycle flags.
pub(crate) struct StreamShared {
    pub(crate) ring: Arc<Mutex<PcmRing>>,
    pub(crate) stats: Arc<RuntimeStats>,
    pub(crate) events: EventSink,
    pub(crate) dsd_transport: DsdTransportMode,
    pub(crate) subslot_bytes: usize,
    pub(crate) channels: usize,
}

impl StreamShared {
    pub(crate) fn new(
        ring: Arc<Mutex<PcmRing>>,
        stats: Arc<RuntimeStats>,
        events: EventSink,
        playback_format: PlaybackFormat,
        subslot_size: u8,
    ) -> Self {
        Self {
            ring,
            channels: stats.channels,
            stats,
            events,
            dsd_transport: playback_format.dsd_transport,
            subslot_bytes: usize::from(subslot_size).max(1),
        }
    }

    pub(crate) fn native_dsd(&self) -> bool {
        self.dsd_transport == DsdTransportMode::Native
    }

    /// Bytes of one wire frame: the granularity of `write`.
    pub(crate) fn wire_frame_bytes(&self) -> usize {
        self.subslot_bytes * self.channels
    }

    /// Ring samples that make one wire frame.
    fn wire_frame_samples(&self) -> usize {
        if self.native_dsd() {
            self.subslot_bytes * self.channels
        } else {
            self.channels
        }
    }

    fn ensure_writable(&self) -> Result<(), Error> {
        if self.stats.closed.load(Ordering::Acquire) {
            return Err(Error::InvalidState("the stream is closed".into()));
        }
        if self.stats.failed.load(Ordering::Acquire) {
            return Err(Error::InvalidState("the stream has failed".into()));
        }
        if self.stats.draining.load(Ordering::Acquire) {
            return Err(Error::InvalidState(
                "drain() was called; flush() before writing more".into(),
            ));
        }
        Ok(())
    }

    /// Appends wire-layout bytes (see `DacStream::write`). Returns the bytes
    /// taken: a whole number of wire frames, as many as fit.
    pub(crate) fn write_bytes(&self, bytes: &[u8]) -> Result<usize, Error> {
        self.ensure_writable()?;
        let taken = {
            let mut ring = self.ring.lock();
            let taken = ring.push_bytes(bytes, self.subslot_bytes, self.native_dsd());
            self.stats
                .buffered_samples
                .store(ring.len_samples(), Ordering::Release);
            taken
        };
        let ring_samples = if self.native_dsd() { taken } else { taken / self.subslot_bytes };
        self.stats
            .producer_frames
            .fetch_add((ring_samples / self.channels.max(1)) as u64, Ordering::Relaxed);
        Ok(taken)
    }

    /// Appends ring-native `i32` samples (see `DacStream::write_i32`). Returns
    /// the samples taken: whole wire frames only.
    pub(crate) fn write_samples(&self, samples: &[i32]) -> Result<usize, Error> {
        self.ensure_writable()?;
        let granule = self.wire_frame_samples();
        let taken = {
            let mut ring = self.ring.lock();
            let aligned = samples.len().min(ring.free_samples()) / granule * granule;
            let taken = ring.push_samples(&samples[..aligned]);
            self.stats
                .buffered_samples
                .store(ring.len_samples(), Ordering::Release);
            taken
        };
        self.stats
            .producer_frames
            .fetch_add((taken / self.channels.max(1)) as u64, Ordering::Relaxed);
        Ok(taken)
    }

    /// Bytes the host can write to bring the ring up to its current target
    /// depth (the shallow processing depth while software gain is on).
    pub(crate) fn writable_bytes(&self) -> usize {
        if self.stats.draining.load(Ordering::Acquire)
            || self.stats.closed.load(Ordering::Acquire)
            || self.stats.failed.load(Ordering::Acquire)
        {
            return 0;
        }
        let queued = self.stats.buffered_samples.load(Ordering::Acquire);
        let room = self
            .stats
            .active_target_samples()
            .min(self.stats.buffer_capacity_samples)
            .saturating_sub(queued);
        let bytes_per_sample = if self.native_dsd() { 1 } else { self.subslot_bytes };
        room * bytes_per_sample / self.wire_frame_bytes() * self.wire_frame_bytes()
    }

    pub(crate) fn set_paused(&self, paused: bool) {
        self.stats.paused.store(paused, Ordering::Release);
        self.refresh_state();
    }

    pub(crate) fn flush(&self) {
        flush_ring(&self.ring, &self.stats);
        self.refresh_state();
    }

    pub(crate) fn drain(&self) {
        self.stats.draining.store(true, Ordering::Release);
        self.refresh_state();
    }

    pub(crate) fn is_drained(&self) -> bool {
        self.stats.draining.load(Ordering::Acquire)
            && self.stats.started.load(Ordering::Acquire)
            && self.stats.is_drained()
    }

    pub(crate) fn set_software_gain(&self, gain: f32) -> Result<(), Error> {
        if !gain.is_finite() || !(0.0..=16.0).contains(&gain) {
            return Err(Error::InvalidArgument(
                "software gain must be a finite value between 0.0 and 16.0".into(),
            ));
        }
        if gain != 1.0 && self.dsd_transport != DsdTransportMode::None {
            return Err(Error::InvalidState(
                "software gain would corrupt a DSD bitstream; it is PCM only".into(),
            ));
        }
        self.stats.gain_bits.store(gain.to_bits(), Ordering::Release);
        set_software_volume_active(gain != 1.0);
        self.note_bit_perfect();
        Ok(())
    }

    /// The state derived from the flags.
    pub(crate) fn current_state(&self) -> StreamState {
        let stats = &self.stats;
        if stats.failed.load(Ordering::Acquire) {
            StreamState::Failed
        } else if stats.closed.load(Ordering::Acquire) {
            StreamState::Closed
        } else if stats.paused.load(Ordering::Acquire) {
            StreamState::Paused
        } else if stats.draining.load(Ordering::Acquire) {
            if self.is_drained() {
                StreamState::Drained
            } else {
                StreamState::Draining
            }
        } else if !stats.started.load(Ordering::Acquire)
            || stats.last_transfer_held.load(Ordering::Acquire)
        {
            StreamState::Buffering
        } else {
            StreamState::Playing
        }
    }

    /// Publishes a `StateChanged` event if the state moved since the last one.
    pub(crate) fn refresh_state(&self) {
        let state = self.current_state();
        let code = state.code() as u32;
        if self.stats.published_state.swap(code, Ordering::AcqRel) != code {
            self.events.emit_stream(DacEvent::StateChanged(state));
        }
    }

    /// Recomputes the bit-perfect verdict and publishes a change.
    pub(crate) fn note_bit_perfect(&self) -> bool {
        let now = bit_perfect_now() && !self.stats.closed.load(Ordering::Acquire);
        if self.stats.published_bit_perfect.swap(now, Ordering::AcqRel) != now {
            self.events.emit_stream(DacEvent::BitPerfectChanged(now));
        }
        now
    }

    /// Marks the stream failed and reports why.
    pub(crate) fn fail(&self, error: Error) {
        let lost = error == Error::DeviceLost;
        self.stats.failed.store(true, Ordering::Release);
        self.events.emit_stream(DacEvent::Error(error));
        if lost {
            self.events.emit_dac(DacEvent::DeviceLost);
        }
        self.note_bit_perfect();
        self.refresh_state();
    }

    pub(crate) fn mark_closed(&self) {
        self.stats.closed.store(true, Ordering::Release);
        self.note_bit_perfect();
        self.refresh_state();
    }
}
