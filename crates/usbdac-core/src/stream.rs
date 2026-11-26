//! The output stream.
//!
//! The model is *push*: the library owns the ring and the USB output thread,
//! the host writes audio into the ring from its own thread. There is no render
//! thread and no callback into the host on the audio path.

use crate::dac::DacShared;
use crate::error::{Error, Result};
use crate::events::StreamState;
use crate::format::StreamFormat;
use crate::transport::session::{bit_perfect_now, current_bit_perfect_blocker};
use crate::transport::shared::StreamShared;
use crate::transport::StreamHandle;
use parking_lot::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// What stands in the way of [`DacStream::is_bit_perfect`], most significant
/// first. `None` from [`DacStream::bit_perfect_blocker`] means nothing does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum BitPerfectBlocker {
    /// The stream is closed or not running.
    NoStream,
    /// At least one frame queued to the DAC was never taken (latched for the
    /// life of the stream).
    LostFrames,
    /// The DAC runs another rate or format than the one requested
    /// (`RatePolicy::NearestSupported`).
    FormatChanged,
    /// The DAC did not confirm its sampling frequency.
    ClockUnverified,
    /// Software gain other than 1.0 is applied.
    SoftwareGain,
    /// An asynchronous endpoint without a feedback endpoint, streamed at the
    /// nominal rate because `DacConfig::allow_unsynchronized_async` is on.
    UnsynchronizedAsync,
    /// The wire payload audit has not seen real audio (yet), or lost it.
    PayloadUnverified,
}

impl BitPerfectBlocker {
    /// Stable identifier: `no_stream`, `lost_frames`, `format_changed`,
    /// `clock_unverified`, `software_gain`, `unsynchronized_async`,
    /// `payload_unverified`.
    pub fn code(self) -> &'static str {
        match self {
            Self::NoStream => "no_stream",
            Self::LostFrames => "lost_frames",
            Self::FormatChanged => "format_changed",
            Self::ClockUnverified => "clock_unverified",
            Self::SoftwareGain => "software_gain",
            Self::UnsynchronizedAsync => "unsynchronized_async",
            Self::PayloadUnverified => "payload_unverified",
        }
    }

    pub(crate) fn description(self) -> &'static str {
        match self {
            Self::NoStream => "stream inactive",
            Self::LostFrames => "audio lost on the USB transport",
            Self::FormatChanged => "requested/effective format mismatch",
            Self::ClockUnverified => "clock verification reset",
            Self::SoftwareGain => "software gain active",
            Self::UnsynchronizedAsync => {
                "asynchronous endpoint without feedback, host-paced at the nominal rate"
            }
            Self::PayloadUnverified => "wire payload audit failed",
        }
    }
}

/// Counters of a stream. Frames are in the unit
/// [`StreamFormat::counter_rate_hz`] gives the rate of.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct StreamStats {
    pub state: StreamState,
    pub bit_perfect: bool,
    /// Why `bit_perfect` is false, if it is.
    pub bit_perfect_blocker: Option<BitPerfectBlocker>,
    pub software_gain: f32,
    /// Runs of padding because the ring ran dry (counted per transfer).
    pub underruns: u64,
    /// Frames queued to the DAC that it never took.
    pub lost_frames: u64,
    pub short_packets: u64,
    pub zero_packets: u64,
    /// Isochronous transfers libusb reported as timed out.
    pub urb_timeouts: u64,
    pub completed_transfers: u64,
    /// Frames the host has written.
    pub frames_written: u64,
    /// Frames the DAC has taken, music and padding alike.
    pub frames_completed: u64,
    /// Music frames completed on the bus since the start or the last flush.
    pub played_frames: u64,
    /// Music frames between the host and the DAC: ring plus bus.
    pub latency_frames: u64,
    pub latency_ms: f64,
    /// Frames in the ring.
    pub queued_frames: u64,
    pub buffer_ms: u32,
    pub buffer_target_ms: u32,
    pub buffer_capacity_ms: u32,
    /// Transfers currently submitted to the bus.
    pub transfer_queue_depth: u32,
    pub last_turnaround_us: u64,
    pub max_turnaround_us: u64,
    pub last_submit_gap_us: u64,
    pub max_submit_gap_us: u64,
    /// Rate the DAC has actually consumed frames at since the start.
    pub effective_rate_hz: f64,
    /// The DAC has an explicit feedback endpoint that is being polled.
    pub feedback_active: bool,
    pub feedback_reports: u64,
    pub feedback_timeouts: u64,
    pub feedback_errors: u64,
    pub feedback_rejected: u64,
    /// Time since the last valid feedback report.
    pub feedback_age_ms: Option<u64>,
    /// The sample rate the last feedback report implies.
    pub feedback_rate_hz: Option<f64>,
    pub feedback_stale: bool,
}

pub(crate) struct StreamInner {
    dac: Arc<DacShared>,
    handle: Mutex<Option<StreamHandle>>,
    pub(crate) shared: Arc<StreamShared>,
    format: StreamFormat,
    closed: AtomicBool,
}

impl StreamInner {
    pub(crate) fn new(dac: Arc<DacShared>, handle: StreamHandle, format: StreamFormat) -> Arc<Self> {
        Arc::new(Self {
            dac,
            shared: Arc::clone(&handle.shared),
            handle: Mutex::new(Some(handle)),
            format,
            closed: AtomicBool::new(false),
        })
    }

    pub(crate) fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    /// Publishes the initial state once the listener is in place.
    pub(crate) fn start_events(&self) {
        self.shared.refresh_state();
    }

    pub(crate) fn close(&self) -> Result<()> {
        if self.closed.swap(true, Ordering::AcqRel) {
            return Ok(());
        }
        let result = match self.handle.lock().take() {
            Some(mut handle) => handle.stop().map_err(Error::Transfer),
            None => Ok(()),
        };
        self.shared.mark_closed();
        // The Closed event is queued with the listener it belongs to; the slot
        // can be cleared now.
        self.dac.events.set_stream_listener(None);
        let mut slot = self.dac.stream.lock();
        if slot
            .as_ref()
            .and_then(std::sync::Weak::upgrade)
            .is_some_and(|current| std::ptr::eq(Arc::as_ptr(&current), self))
        {
            *slot = None;
        }
        result
    }
}

/// An open output stream. Dropping it closes it.
///
/// All methods take `&self`, so a stream can be shared between the host's
/// writer thread and its control thread behind an `Arc`.
///
/// # Sample layout
///
/// [`write`](Self::write) takes **wire-layout bytes**: the bytes exactly as
/// they go into the USB packets, in whole *wire frames*
/// (`format.bytes_per_frame()` = `subslot_bytes * channels` bytes, channels
/// interleaved in the DAC's channel order). Samples are little-endian.
///
/// * **PCM** — each sample is `subslot_bytes` bytes, a signed two's-complement
///   integer, *left-justified*: the sign bit is the top bit of the subslot and
///   unused low bits are zero. 16-bit audio in a 2-byte subslot is a plain
///   `i16`; 24-bit audio in a 3-byte subslot is a packed `i24`; 24-bit audio
///   in a 4-byte subslot is the `i24` shifted left by 8 (low byte zero);
///   32-bit audio is an `i32`. With the layout the DAC advertises, the bytes
///   written are the bytes on the wire.
/// * **DoP** — each sample is the DoP word in the same subslot layout, the 16
///   DSD bits in the top two bytes below the marker: for a 3-byte subslot
///   `[dsd_low, dsd_high, marker]`, for a 4-byte subslot `[0, dsd_low,
///   dsd_high, marker]`. `dsd_high` holds the older 8 DSD bits (MSB first). The
///   marker byte is **ignored and overwritten**: the library owns the
///   `0x05`/`0xFA` alternation so the phase survives pause, flush and underrun.
/// * **Native DSD** — raw DSD bytes, MSB first (the oldest bit in bit 7),
///   interleaved one byte per channel: `[ch0, ch1, ch0, ch1, ...]` in
///   stream order. A wire frame is `subslot_bytes * channels` such bytes (one
///   wire word per channel); the library groups consecutive bytes of a channel
///   into the wire word and orders them as the DAC needs (big-endian or
///   little-endian, optional bit reversal, per quirk or config).
///
/// [`write_i32`](Self::write_i32) takes the ring's own sample unit instead:
/// PCM as full-scale 32-bit left-justified `i32` (the encoder keeps the top
/// `valid_bits` bits), DoP as `i32` words with the DSD bits in bits 23..8 (bits
/// 31..24 are overwritten with the marker), native DSD as one DSD byte per
/// `i32` in bits 7..0.
pub struct DacStream {
    inner: Arc<StreamInner>,
}

impl std::fmt::Debug for DacStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DacStream")
            .field("format", &self.inner.format)
            .finish_non_exhaustive()
    }
}

impl DacStream {
    pub(crate) fn from_inner(inner: Arc<StreamInner>) -> Self {
        Self { inner }
    }

    fn shared(&self) -> &StreamShared {
        &self.inner.shared
    }

    fn ensure_open(&self) -> Result<()> {
        if self.inner.is_closed() {
            return Err(Error::InvalidState("the stream is closed".to_string()));
        }
        Ok(())
    }

    /// The wire format this stream runs.
    pub fn format(&self) -> &StreamFormat {
        &self.inner.format
    }

    /// Appends wire-layout bytes (see the type-level documentation). Never
    /// blocks. Returns the number of bytes taken: a whole number of wire frames,
    /// as many as fit in the ring; a partial or zero count means the ring is
    /// full, so retry with the remainder later. Input shorter than one wire
    /// frame is not taken.
    ///
    /// [`Error::InvalidState`] after [`drain`](Self::drain) (until
    /// [`flush`](Self::flush)), or once the stream has closed or failed.
    pub fn write(&self, data: &[u8]) -> Result<usize> {
        self.ensure_open()?;
        self.shared().write_bytes(data)
    }

    /// Like [`write`](Self::write) for the ring's native `i32` samples. Counts
    /// and returns samples (a whole number of wire frames: multiples of
    /// `channels`, or of `subslot_bytes * channels` for native DSD).
    pub fn write_i32(&self, samples: &[i32]) -> Result<usize> {
        self.ensure_open()?;
        self.shared().write_samples(samples)
    }

    /// How many bytes the host should write now to bring the ring to its
    /// target depth ([`DacConfig::buffer_target_ms`], or the shallower
    /// `processing_buffer_target_ms` while software gain is on). A whole number
    /// of wire frames. `write` still accepts more, up to the ring's capacity.
    ///
    /// [`DacConfig::buffer_target_ms`]: crate::DacConfig::buffer_target_ms
    pub fn writable_bytes(&self) -> usize {
        if self.inner.is_closed() {
            return 0;
        }
        self.shared().writable_bytes()
    }

    /// Resumes after [`pause`](Self::pause). A new stream plays by itself once
    /// the ring holds the start threshold; this is only needed after a pause.
    pub fn play(&self) -> Result<()> {
        self.ensure_open()?;
        self.shared().set_paused(false);
        Ok(())
    }

    /// Holds the stream: wire-correct silence goes out (zero for PCM, the
    /// idle pattern for DSD, DoP markers keep alternating) so the DAC stays
    /// locked, and the ring is not consumed. Audio already submitted to the bus
    /// (eight transfers: 16 to 32 ms on a high-speed link, about 130 ms on a
    /// full-speed one) still plays out first; the ring resumes exactly where it
    /// was.
    pub fn pause(&self) -> Result<()> {
        self.ensure_open()?;
        self.shared().set_paused(true);
        Ok(())
    }

    /// Drops everything queued in the ring and resets
    /// [`played_frames`](Self::played_frames) to 0. The stream holds wire
    /// silence until the ring holds the start threshold again. Audio already
    /// submitted to the bus still plays (it is not counted as played). Also
    /// clears [`drain`](Self::drain), so the stream can be reused.
    pub fn flush(&self) -> Result<()> {
        self.ensure_open()?;
        self.shared().flush();
        Ok(())
    }

    /// Declares that no more audio will be written: the ring plays out, even a
    /// tail shorter than the start threshold, and the stream then reports
    /// [`is_drained`](Self::is_drained) and the `Drained` state. The padded
    /// tail is not an underrun. Further writes fail until
    /// [`flush`](Self::flush).
    pub fn drain(&self) -> Result<()> {
        self.ensure_open()?;
        self.shared().drain();
        Ok(())
    }

    /// `drain()` was called and everything written has been played.
    pub fn is_drained(&self) -> bool {
        self.shared().is_drained()
    }

    /// Closes the stream and waits for the output thread. The DAC stays open.
    pub fn close(self) -> Result<()> {
        self.inner.close()
    }

    pub fn state(&self) -> StreamState {
        self.shared().current_state()
    }

    /// Music frames the DAC has completed on the bus since the stream started
    /// or the last [`flush`](Self::flush). Padding, held silence and lost
    /// frames are not counted. The unit is `format().counter_rate_hz()`.
    pub fn played_frames(&self) -> u64 {
        self.shared().stats.played_frames()
    }

    /// Music frames between the host and the DAC: in the ring plus on the bus.
    /// What the audible position lags the written position by.
    pub fn latency_frames(&self) -> u64 {
        self.shared().stats.output_latency_frames()
    }

    /// Bit-perfect means: no software gain, a verified clock at the requested
    /// rate, no frame lost on the bus, and the wire payload is real audio.
    /// False as soon as any of that stops being true, and for good after a lost
    /// frame.
    pub fn is_bit_perfect(&self) -> bool {
        !self.inner.is_closed() && self.shared().note_bit_perfect()
    }

    /// Why [`is_bit_perfect`](Self::is_bit_perfect) is false: `None` when it is
    /// true, otherwise the first condition that fails.
    pub fn bit_perfect_blocker(&self) -> Option<BitPerfectBlocker> {
        if self.inner.is_closed() {
            return Some(BitPerfectBlocker::NoStream);
        }
        current_bit_perfect_blocker()
    }

    /// Scales PCM samples by `gain` (0.0 to 16.0) as each transfer is prepared,
    /// so it takes effect within the bus latency rather than the ring depth.
    /// Rounds to nearest and saturates; no dither. Any value other than 1.0
    /// ends bit-perfect output. Refused for DoP and native DSD (it would corrupt
    /// the bitstream).
    pub fn set_software_gain(&self, gain: f32) -> Result<()> {
        self.ensure_open()?;
        self.shared().set_software_gain(gain)
    }

    pub fn software_gain(&self) -> f32 {
        self.shared().stats.software_gain()
    }

    pub fn stats(&self) -> StreamStats {
        use std::sync::atomic::Ordering::Relaxed;
        let shared = self.shared();
        let stats = &shared.stats;
        let channels = stats.channels.max(1) as u64;
        let feedback_last = stats.feedback_last_report_ms.load(Relaxed);
        let elapsed_ms = stats.started_at.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;
        StreamStats {
            state: shared.current_state(),
            bit_perfect: !self.inner.is_closed() && bit_perfect_now(),
            bit_perfect_blocker: self.bit_perfect_blocker(),
            software_gain: stats.software_gain(),
            underruns: stats.underrun_count.load(Relaxed),
            lost_frames: stats.lost_frames.load(Relaxed),
            short_packets: stats.short_packets.load(Relaxed),
            zero_packets: stats.zero_packets.load(Relaxed),
            urb_timeouts: stats.data_urb_timeouts.load(Relaxed),
            completed_transfers: stats.completed_transfers.load(Relaxed),
            frames_written: stats.producer_frames.load(Relaxed),
            frames_completed: stats.consumer_frames.load(Relaxed),
            played_frames: stats.played_frames(),
            latency_frames: stats.output_latency_frames(),
            latency_ms: stats.output_latency_ms(),
            queued_frames: stats.buffered_samples.load(Relaxed) as u64 / channels,
            buffer_ms: stats.buffer_fill_ms(),
            buffer_target_ms: stats.buffer_target_ms(),
            buffer_capacity_ms: stats.buffer_capacity_ms(),
            transfer_queue_depth: stats.transfer_queue_depth.load(Relaxed) as u32,
            last_turnaround_us: stats.last_transfer_turnaround_us.load(Relaxed),
            max_turnaround_us: stats.max_transfer_turnaround_us.load(Relaxed),
            last_submit_gap_us: stats.last_submit_gap_us.load(Relaxed),
            max_submit_gap_us: stats.max_submit_gap_us.load(Relaxed),
            effective_rate_hz: stats.effective_consumer_rate_milli_hz.load(Relaxed) as f64 / 1_000.0,
            feedback_active: self.inner.format.explicit_feedback
                && matches!(self.inner.format.sync_type, crate::SyncKind::Asynchronous),
            feedback_reports: stats.feedback_reports.load(Relaxed),
            feedback_timeouts: stats.feedback_timeouts.load(Relaxed),
            feedback_errors: stats.feedback_errors.load(Relaxed),
            feedback_rejected: stats.feedback_rejected.load(Relaxed),
            feedback_age_ms: (feedback_last != u64::MAX)
                .then(|| elapsed_ms.saturating_sub(feedback_last)),
            feedback_rate_hz: (stats.feedback_reports.load(Relaxed) > 0)
                .then(|| stats.feedback_rate_milli_hz.load(Relaxed) as f64 / 1_000.0),
            feedback_stale: stats.feedback_stale.load(Relaxed),
        }
    }
}

impl Drop for DacStream {
    fn drop(&mut self) {
        let _ = self.inner.close();
    }
}
