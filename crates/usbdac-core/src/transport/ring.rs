use super::*;
use crate::ring_target::{live_depth, RingTargets};

/// Counters and control flags shared between the API, the output thread and
/// the ring. Feedback evidence has its own lock, separate from the hot counters.
#[derive(Debug)]
pub(crate) struct RuntimeStats {
    pub(crate) sample_rate: u32,
    pub(crate) channels: usize,
    pub(crate) buffer_capacity_samples: usize,
    /// Depth `writable_bytes()` fills the ring to.
    pub(crate) buffer_target_samples: usize,
    /// The shallower depth recommended while software gain is not 1.0.
    pub(crate) processing_target_samples: usize,
    /// Ring fill that starts the stream and ends a post-flush hold; never above
    /// the target.
    pub(crate) start_threshold_samples: usize,
    pub(crate) started_at: Instant,
    pub(crate) transfer_queue_depth: AtomicUsize,
    pub(crate) frames_per_packet: AtomicUsize,
    pub(crate) buffered_samples: AtomicUsize,
    pub(crate) underrun_count: AtomicU64,
    /// Frames the host has written.
    pub(crate) producer_frames: AtomicU64,
    pub(crate) consumer_frames: AtomicU64,
    /// Ring frames queued to the DAC that it never took (timed-out URBs and
    /// short or zero-length ISO packets). Never included in `consumer_frames`.
    pub(crate) lost_frames: AtomicU64,
    pub(crate) short_packets: AtomicU64,
    pub(crate) zero_packets: AtomicU64,
    /// Data-endpoint URBs libusb reported as timed out. Unrelated to the
    /// feedback endpoint's counters.
    pub(crate) data_urb_timeouts: AtomicU64,
    /// Latched by the first lost frame. Stats are created per stream, so a new
    /// stream starts unlatched.
    pub(crate) transport_loss_latched: AtomicBool,
    /// Bumped, with the ring lock held, each time the ring is flushed.
    pub(crate) flush_epoch: AtomicU64,
    /// Music frames the DAC has taken in `played_epoch`.
    pub(crate) played_music_frames: AtomicU64,
    /// The flush epoch `played_music_frames` counts in. When it lags
    /// `flush_epoch` the count is from before the last flush and reads as 0.
    pub(crate) played_epoch: AtomicU64,
    /// Music frames of the current flush epoch submitted and not yet completed.
    pub(crate) inflight_music_frames: AtomicU64,
    pub(crate) completed_transfers: AtomicU64,
    /// Transfers cancelled on the bus by `flush()` (`cancel_in_flight_on_flush`).
    /// Not completions: they are neither lost nor played.
    pub(crate) flush_cancelled_transfers: AtomicU64,
    pub(crate) last_transfer_turnaround_us: AtomicU64,
    pub(crate) max_transfer_turnaround_us: AtomicU64,
    pub(crate) last_submit_gap_us: AtomicU64,
    pub(crate) max_submit_gap_us: AtomicU64,
    pub(crate) worker_gap_over_10ms: AtomicU64,
    pub(crate) worker_gap_over_50ms: AtomicU64,
    pub(crate) max_worker_gap_us: AtomicU64,
    pub(crate) adaptive_queue_changes: AtomicU64,
    pub(crate) adaptive_queue_slots: AtomicU64,
    pub(crate) adaptive_queue_us: AtomicU64,
    pub(crate) effective_consumer_rate_milli_hz: AtomicU64,
    pub(crate) feedback_reports: AtomicU64,
    pub(crate) feedback_timeouts: AtomicU64,
    pub(crate) feedback_errors: AtomicU64,
    pub(crate) feedback_rejected: AtomicU64,
    /// Milliseconds after `started_at` of the last valid feedback report;
    /// `u64::MAX` before the first.
    pub(crate) feedback_last_report_ms: AtomicU64,
    pub(crate) feedback_rate_milli_hz: AtomicU64,
    pub(crate) feedback_stale: AtomicBool,
    pub(crate) feedback_locked: AtomicBool,
    pub(crate) feedback_required: AtomicBool,
    pub(crate) implicit_feedback: AtomicBool,
    pub(crate) feedback_empty: AtomicU64,
    pub(crate) explicit_feedback_diagnostics: Mutex<Option<crate::ExplicitFeedbackDiagnostics>>,
    pub(crate) feedback_trace: Option<Arc<crate::feedback_diagnostics::FeedbackRecorder>>,
    pub(crate) implicit_fifo_depth: AtomicUsize,
    pub(crate) implicit_fifo_high_water: AtomicUsize,
    pub(crate) implicit_waits: AtomicU64,
    pub(crate) implicit_empty_bus: AtomicU64,
    pub(crate) implicit_overruns: AtomicU64,
    pub(crate) implicit_in_transfers: AtomicU64,
    pub(crate) implicit_out_of_order: AtomicU64,
    pub(crate) implicit_invalid_packets: AtomicU64,
    pub(crate) implicit_measured_frames: AtomicU64,
    pub(crate) implicit_scheduled_frames: AtomicU64,

    // --- control and lifecycle ---
    /// `pause()` in effect: transfers carry wire silence and the ring is
    /// untouched.
    pub(crate) paused: AtomicBool,
    /// `drain()` called: no more audio will be written; the tail is padded
    /// without counting an underrun and playback starts below the threshold.
    pub(crate) draining: AtomicBool,
    /// Software gain as `f32` bits (1.0 = off).
    pub(crate) gain_bits: AtomicU32,
    /// The last transfer carried held silence (pause or refill wait).
    pub(crate) last_transfer_held: AtomicBool,
    /// The last transfer was padded for lack of audio.
    pub(crate) last_transfer_underrun: AtomicBool,
    /// The first transfer has been submitted.
    pub(crate) started: AtomicBool,
    pub(crate) failed: AtomicBool,
    pub(crate) closed: AtomicBool,
    /// The state last published as an event (`StreamState::code`).
    pub(crate) published_state: AtomicU32,
    /// The last value published through `BitPerfectChanged`.
    pub(crate) published_bit_perfect: AtomicBool,
    /// The stream carries a DSD bitstream (DoP or native): never processed, so
    /// never "live", whatever the gain value says.
    pub(crate) raw_bitstream: AtomicBool,
    /// The host says it processes the audio before writing it, so a change it
    /// makes is heard after the ring's fill (`DacStream::set_host_processing`).
    pub(crate) host_processing: AtomicBool,
}

#[derive(Debug)]
pub(crate) struct PcmRing {
    pub(super) samples: Vec<i32>,
    pub(super) capacity_samples: usize,
    pub(super) channels: usize,
    pub(super) read_index: usize,
    pub(super) write_index: usize,
    pub(super) len_samples: usize,
    pub(super) dop_marker_is_fa: bool,
    /// Set by `clear`: hold wire silence until the ring can fill a transfer, so
    /// a flush never shows up as an underrun or a half-padded transfer.
    pub(super) refill_pending: bool,
    /// Counts `clear` calls. A transfer remembers the epoch it was filled in, so
    /// audio queued before a flush is never mistaken for audio after it.
    pub(super) flush_epoch: u64,
}

/// What `pop_music_into_or_pad` took from the ring.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RingPop {
    /// The ring ran dry and the tail of the output is padding.
    pub(super) underrun: bool,
    /// Samples taken from the ring; the rest of the output is padding.
    pub(super) music_samples: usize,
}

impl RuntimeStats {
    pub(crate) fn new(
        sample_rate: u32,
        channels: usize,
        capacity_ms: usize,
        target_ms: usize,
        start_threshold_ms: usize,
        processing_target_ms: usize,
    ) -> Self {
        let frames_per_ms = sample_rate as usize / 1_000;
        let samples_for = |ms: usize| frames_per_ms.saturating_mul(ms).saturating_mul(channels);
        let capacity_samples = samples_for(capacity_ms);
        let target_samples = samples_for(target_ms.min(capacity_ms));
        let processing_target_samples = samples_for(processing_target_ms.min(capacity_ms));
        let start_threshold_samples = samples_for(start_threshold_ms.min(target_ms));

        Self {
            sample_rate,
            channels,
            buffer_capacity_samples: capacity_samples,
            buffer_target_samples: target_samples,
            processing_target_samples,
            start_threshold_samples,
            started_at: Instant::now(),
            transfer_queue_depth: AtomicUsize::new(0),
            frames_per_packet: AtomicUsize::new(0),
            buffered_samples: AtomicUsize::new(0),
            underrun_count: AtomicU64::new(0),
            producer_frames: AtomicU64::new(0),
            consumer_frames: AtomicU64::new(0),
            lost_frames: AtomicU64::new(0),
            short_packets: AtomicU64::new(0),
            zero_packets: AtomicU64::new(0),
            data_urb_timeouts: AtomicU64::new(0),
            transport_loss_latched: AtomicBool::new(false),
            flush_epoch: AtomicU64::new(0),
            played_music_frames: AtomicU64::new(0),
            played_epoch: AtomicU64::new(0),
            inflight_music_frames: AtomicU64::new(0),
            completed_transfers: AtomicU64::new(0),
            flush_cancelled_transfers: AtomicU64::new(0),
            last_transfer_turnaround_us: AtomicU64::new(0),
            max_transfer_turnaround_us: AtomicU64::new(0),
            last_submit_gap_us: AtomicU64::new(0),
            max_submit_gap_us: AtomicU64::new(0),
            worker_gap_over_10ms: AtomicU64::new(0),
            worker_gap_over_50ms: AtomicU64::new(0),
            max_worker_gap_us: AtomicU64::new(0),
            adaptive_queue_changes: AtomicU64::new(0),
            adaptive_queue_slots: AtomicU64::new(0),
            adaptive_queue_us: AtomicU64::new(0),
            effective_consumer_rate_milli_hz: AtomicU64::new(0),
            feedback_reports: AtomicU64::new(0),
            feedback_timeouts: AtomicU64::new(0),
            feedback_errors: AtomicU64::new(0),
            feedback_rejected: AtomicU64::new(0),
            feedback_last_report_ms: AtomicU64::new(u64::MAX),
            feedback_rate_milli_hz: AtomicU64::new(0),
            feedback_stale: AtomicBool::new(false),
            feedback_locked: AtomicBool::new(false),
            feedback_required: AtomicBool::new(false),
            implicit_feedback: AtomicBool::new(false),
            feedback_empty: AtomicU64::new(0),
            explicit_feedback_diagnostics: Mutex::new(None),
            feedback_trace: None,
            implicit_fifo_depth: AtomicUsize::new(0),
            implicit_fifo_high_water: AtomicUsize::new(0),
            implicit_waits: AtomicU64::new(0),
            implicit_empty_bus: AtomicU64::new(0),
            implicit_overruns: AtomicU64::new(0),
            implicit_in_transfers: AtomicU64::new(0),
            implicit_out_of_order: AtomicU64::new(0),
            implicit_invalid_packets: AtomicU64::new(0),
            implicit_measured_frames: AtomicU64::new(0),
            implicit_scheduled_frames: AtomicU64::new(0),

            paused: AtomicBool::new(false),
            draining: AtomicBool::new(false),
            gain_bits: AtomicU32::new(1.0f32.to_bits()),
            last_transfer_held: AtomicBool::new(false),
            last_transfer_underrun: AtomicBool::new(false),
            started: AtomicBool::new(false),
            failed: AtomicBool::new(false),
            closed: AtomicBool::new(false),
            published_state: AtomicU32::new(u32::MAX),
            published_bit_perfect: AtomicBool::new(false),
            raw_bitstream: AtomicBool::new(false),
            host_processing: AtomicBool::new(false),
        }
    }

    pub(crate) fn with_feedback_trace(mut self, capture: bool) -> Self {
        if capture {
            self.feedback_trace = Some(Arc::new(
                crate::feedback_diagnostics::FeedbackRecorder::new(self.started_at),
            ));
        }
        self
    }

    pub(crate) fn transport_loss_latched(&self) -> bool {
        self.transport_loss_latched.load(Ordering::Relaxed)
    }

    pub(crate) fn software_gain(&self) -> f32 {
        f32::from_bits(self.gain_bits.load(Ordering::Relaxed))
    }

    /// The ring depth the host is asked to keep right now: the shallow
    /// processing depth while software gain is applied to PCM or the host says
    /// it processes the audio, the deep target otherwise (see `ring_target`).
    pub(crate) fn active_target_samples(&self) -> usize {
        RingTargets::new(self.buffer_target_samples, self.processing_target_samples).select(
            live_depth(
                self.raw_bitstream.load(Ordering::Relaxed),
                self.software_gain(),
                self.host_processing.load(Ordering::Relaxed),
            ),
        )
    }

    /// Frames waiting in the ring, in counter units (one DSD byte per channel
    /// for native DSD).
    pub(crate) fn queued_frames(&self) -> u64 {
        self.buffered_samples.load(Ordering::Relaxed) as u64 / self.channels.max(1) as u64
    }

    /// The depth `active_target_samples` asks for, in milliseconds.
    pub(crate) fn active_target_ms(&self) -> u32 {
        samples_to_millis(
            self.active_target_samples(),
            self.sample_rate,
            self.channels,
        )
    }

    /// Music frames the DAC has taken since the stream started or the ring was
    /// last flushed.
    pub(crate) fn played_frames(&self) -> u64 {
        if self.played_epoch.load(Ordering::Acquire) != self.flush_epoch.load(Ordering::Acquire) {
            return 0;
        }
        self.played_music_frames.load(Ordering::Relaxed)
    }

    /// Music frames between the host and the DAC: queued in the ring plus
    /// submitted and not yet completed. Transfers carrying held silence or
    /// padding are not music and are not counted.
    pub(crate) fn output_latency_frames(&self) -> u64 {
        let ring_frames =
            (self.buffered_samples.load(Ordering::Relaxed) / self.channels.max(1)) as u64;
        output_latency_frames(
            ring_frames,
            self.inflight_music_frames.load(Ordering::Relaxed),
        )
    }

    pub(crate) fn output_latency_ms(&self) -> f64 {
        if self.sample_rate == 0 {
            return 0.0;
        }
        self.output_latency_frames() as f64 * 1_000.0 / f64::from(self.sample_rate)
    }

    pub(crate) fn buffer_fill_ms(&self) -> u32 {
        samples_to_millis(
            self.buffered_samples.load(Ordering::Relaxed),
            self.sample_rate,
            self.channels,
        )
    }

    pub(crate) fn buffer_capacity_ms(&self) -> u32 {
        samples_to_millis(
            self.buffer_capacity_samples,
            self.sample_rate,
            self.channels,
        )
    }

    pub(crate) fn buffer_target_ms(&self) -> u32 {
        samples_to_millis(self.buffer_target_samples, self.sample_rate, self.channels)
    }

    /// Everything the host wrote has been played: the ring is empty and nothing
    /// of this flush epoch is still on the bus.
    pub(crate) fn is_drained(&self) -> bool {
        self.buffered_samples.load(Ordering::Relaxed) == 0
            && self.inflight_music_frames.load(Ordering::Relaxed) == 0
    }
}

impl PcmRing {
    pub(super) fn new(capacity_samples: usize, channels: usize) -> Self {
        let channels = channels.max(1);
        let min_capacity = channels * 4;
        let capacity_samples = capacity_samples.max(min_capacity) / channels * channels;
        Self {
            samples: vec![0; capacity_samples],
            capacity_samples,
            channels,
            read_index: 0,
            write_index: 0,
            len_samples: 0,
            dop_marker_is_fa: false,
            refill_pending: false,
            flush_epoch: 0,
        }
    }

    pub(super) fn flush_epoch(&self) -> u64 {
        self.flush_epoch
    }

    pub(super) fn len_samples(&self) -> usize {
        self.len_samples
    }

    /// Drops every queued sample (seek). The DoP marker phase is wire state and
    /// survives, so the stream stays continuous across the flush.
    pub(super) fn clear(&mut self) {
        self.read_index = 0;
        self.write_index = 0;
        self.len_samples = 0;
        self.refill_pending = true;
        self.flush_epoch = self.flush_epoch.wrapping_add(1);
    }

    /// True while `clear` is waiting for `needed_samples` of new audio. The
    /// caller then sends silence via `fill_wire_silence` instead of popping.
    pub(super) fn awaiting_refill(&mut self, needed_samples: usize) -> bool {
        if self.refill_pending && self.len_samples < needed_samples {
            return true;
        }
        self.refill_pending = false;
        false
    }

    pub(super) fn push_samples(&mut self, input: &[i32]) -> usize {
        if input.is_empty() || self.capacity_samples == 0 {
            return 0;
        }

        let writable = input
            .len()
            .min(self.capacity_samples.saturating_sub(self.len_samples));
        let writable = writable / self.channels * self.channels;
        for sample in input.iter().take(writable) {
            self.samples[self.write_index] = *sample;
            self.write_index = (self.write_index + 1) % self.capacity_samples;
        }
        self.len_samples += writable;
        writable
    }

    /// Appends wire-layout bytes: `subslot` little-endian bytes per sample,
    /// decoded left-justified into the ring's `i32` (so the valid bits stay at
    /// the top, which is what the encoder expects), or for native DSD one byte
    /// per ring sample. Takes whole wire frames (`subslot * channels` bytes)
    /// only and returns the bytes consumed.
    #[cfg(test)]
    pub(super) fn push_bytes(&mut self, bytes: &[u8], subslot: usize, native_dsd: bool) -> usize {
        self.push_bytes_encoding(bytes, subslot, native_dsd, false)
    }

    pub(super) fn push_bytes_encoding(
        &mut self,
        bytes: &[u8],
        subslot: usize,
        native_dsd: bool,
        unsigned8: bool,
    ) -> usize {
        // Native DSD has byte-valued ring entries even for 64-bit words.
        // Keep PCM limited to its i32 container independently.
        let subslot = if native_dsd {
            if !matches!(subslot, 1 | 2 | 4 | 8) {
                return 0;
            }
            subslot
        } else {
            subslot.clamp(1, 4)
        };
        let wire_frame_bytes = subslot * self.channels;
        let bytes_per_sample = if native_dsd { 1 } else { subslot };
        let room_samples = self.capacity_samples.saturating_sub(self.len_samples);
        let take =
            bytes.len().min(room_samples * bytes_per_sample) / wire_frame_bytes * wire_frame_bytes;
        if take == 0 {
            return 0;
        }
        for chunk in bytes[..take].chunks_exact(bytes_per_sample) {
            let sample = if native_dsd {
                i32::from(chunk[0])
            } else {
                let mut word = [0u8; 4];
                word[4 - subslot..].copy_from_slice(chunk);
                // Center U8 around signed zero using bits only; the encoder reverses this mapping.
                if unsigned8 {
                    word[3] ^= 0x80;
                }
                i32::from_le_bytes(word)
            };
            self.samples[self.write_index] = sample;
            self.write_index = (self.write_index + 1) % self.capacity_samples;
        }
        self.len_samples += take / bytes_per_sample;
        take
    }

    /// Room for more samples, in samples.
    pub(super) fn free_samples(&self) -> usize {
        self.capacity_samples.saturating_sub(self.len_samples)
    }

    #[cfg(test)]
    pub(super) fn pop_into_or_pad(
        &mut self,
        output: &mut [i32],
        channels: usize,
        dsd_transport: DsdTransportMode,
    ) -> bool {
        self.pop_music_into_or_pad(output, channels, dsd_transport)
            .underrun
    }

    /// `pop_into_or_pad`, also reporting how much of `output` is real audio.
    pub(super) fn pop_music_into_or_pad(
        &mut self,
        output: &mut [i32],
        _channels: usize,
        dsd_transport: DsdTransportMode,
    ) -> RingPop {
        if output.is_empty() {
            return RingPop {
                underrun: false,
                music_samples: 0,
            };
        }

        let readable = output.len().min(self.len_samples) / self.channels * self.channels;
        for destination in output.iter_mut().take(readable) {
            *destination = self.samples[self.read_index];
            self.read_index = (self.read_index + 1) % self.capacity_samples;
        }
        self.len_samples -= readable;

        let underrun = readable < output.len();
        if underrun {
            // Zero is full-scale DC for native DSD and breaks DoP marker sync;
            // pad with a silent but valid bitstream instead.
            output[readable..].fill(wire_silence_sample(dsd_transport));
        }

        self.stamp_dop_markers(output, dsd_transport);

        RingPop {
            underrun,
            music_samples: readable,
        }
    }

    /// Fills `output` with wire-correct silence and leaves the queue untouched.
    /// Used while paused (or refilling after `clear`) to keep the isochronous
    /// stream running without consuming audio the user has not heard yet. DoP
    /// markers advance exactly as for real audio.
    pub(super) fn fill_wire_silence(
        &mut self,
        output: &mut [i32],
        dsd_transport: DsdTransportMode,
    ) {
        output.fill(wire_silence_sample(dsd_transport));
        self.stamp_dop_markers(output, dsd_transport);
    }

    // The final USB stream owns marker phase. Decoder restarts and inserted
    // silence must not reset it or give L/R different markers.
    pub(super) fn stamp_dop_markers(
        &mut self,
        output: &mut [i32],
        dsd_transport: DsdTransportMode,
    ) {
        if dsd_transport != DsdTransportMode::DoP {
            return;
        }
        for frame in output.chunks_exact_mut(self.channels) {
            let marker = if self.dop_marker_is_fa { 0xFAu32 } else { 0x05 };
            for sample in frame {
                *sample = ((*sample as u32 & 0x00ff_ffff) | marker << 24) as i32;
            }
            self.dop_marker_is_fa = !self.dop_marker_is_fa;
        }
    }
}

pub(super) fn wire_silence_sample(dsd_transport: DsdTransportMode) -> i32 {
    match dsd_transport {
        DsdTransportMode::Native => 0x69,
        DsdTransportMode::DoP => 0x00696900,
        DsdTransportMode::None => 0,
    }
}

pub(super) fn samples_to_millis(samples: usize, sample_rate: u32, channels: usize) -> u32 {
    if sample_rate == 0 || channels == 0 {
        return 0;
    }
    ((samples as u64 * 1_000) / (sample_rate as u64 * channels as u64)) as u32
}

pub(super) fn effective_ring_rate(playback_format: &PlaybackFormat) -> u32 {
    if playback_format.dsd_transport == DsdTransportMode::Native && playback_format.dsd_bit_rate > 0
    {
        playback_format.dsd_bit_rate / 8
    } else {
        playback_format.sample_rate
    }
}

/// Drops everything queued and starts a new flush epoch (the rest of the audio
/// path notices through the epoch). Returns the samples dropped. The DoP marker
/// phase survives, so the stream stays continuous across the flush.
pub(super) fn flush_ring(ring: &Mutex<PcmRing>, stats: &RuntimeStats) -> usize {
    let dropped_samples = {
        let mut guard = ring.lock();
        let dropped_samples = guard.len_samples();
        guard.clear();
        // Under the ring lock, so a transfer filled afterwards carries the new
        // epoch and one filled before it the old one.
        stats
            .flush_epoch
            .store(guard.flush_epoch(), Ordering::Release);
        stats.buffered_samples.store(0, Ordering::Relaxed);
        dropped_samples
    };
    // Music already on the bus belongs to the old epoch and is no longer ahead
    // of the new position; the output thread's next publish agrees.
    stats.inflight_music_frames.store(0, Ordering::Relaxed);
    stats.draining.store(false, Ordering::Release);
    dev_eprintln!(
        "[USB] flush dropped {}ms of queued audio",
        samples_to_millis(dropped_samples, stats.sample_rate, stats.channels)
    );
    dropped_samples
}

/// Folds one completed transfer into the played-frame counters. The output loop
/// calls this for every completion; it is a function of its own so the host
/// tests exercise the same accounting.
pub(super) fn account_completed_transfer(
    stats: &RuntimeStats,
    played: &mut PlayedMusicCounter,
    completed_epoch: u64,
    queued_frames: u64,
    music_frames: u64,
    lost_frames: u64,
) {
    let current_epoch = stats.flush_epoch.load(Ordering::Acquire);
    let total = played.complete(
        current_epoch,
        completed_epoch,
        music_frames_delivered(queued_frames, music_frames, lost_frames),
    );
    stats.played_music_frames.store(total, Ordering::Relaxed);
    stats.played_epoch.store(current_epoch, Ordering::Release);
}
