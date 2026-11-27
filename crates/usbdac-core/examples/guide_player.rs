#![allow(dead_code)]
//! Playing through an open DAC: the writer thread, the transport, events, volume, the bit-perfect line
//! and a clean close.
//!
//! This is the code of the second half of `docs/sdk/guide-rust.md`. It compiles on every host and needs a
//! DAC on Linux or Android to run. `cargo check --workspace --examples` keeps it compiling.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::Duration;
use usbdac_core::{
    BitPerfectBlocker, CloseVolume, Dac, DacEvent, DacListener, DacStream, Error, Result, StreamFormat,
    VolumeControl,
};

// region rust-source
/// The host's decoder, as the writer thread sees it.
pub trait Source: Send {
    /// Fills `out` with whole frames of PCM as full-scale 32-bit samples, left-justified: a 16-bit sample
    /// is `sample << 16`, a 24-bit one `sample << 8`. Returns how many samples it wrote, a multiple of the
    /// channel count, or 0 at the end of the track. The stream keeps the top `valid_bits` bits.
    fn render(&mut self, out: &mut [i32]) -> usize;

    /// Moves the decoder to `seconds`.
    fn seek(&mut self, seconds: f64);
}

/// A 440 Hz tone at 24-bit precision, for trying the code without a decoder.
pub struct Tone {
    rate: f64,
    channels: usize,
    phase: f64,
}

impl Tone {
    pub fn new(rate: u32, channels: u16) -> Self {
        Self { rate: f64::from(rate), channels: usize::from(channels), phase: 0.0 }
    }
}

impl Source for Tone {
    fn render(&mut self, out: &mut [i32]) -> usize {
        for frame in out.chunks_exact_mut(self.channels) {
            let sample = ((self.phase * std::f64::consts::TAU).sin() * 0.25 * 8_388_607.0) as i32;
            frame.fill(sample << 8); // 24 bits, left-justified in 32
            self.phase = (self.phase + 440.0 / self.rate).fract();
        }
        out.len()
    }

    fn seek(&mut self, _seconds: f64) {
        self.phase = 0.0;
    }
}
// endregion

// region rust-ring
/// The part of `DacStream` that the writer and the transport use. A host that wants to test its writer
/// on a laptop writes a second implementation that records what it is given: `DacStream` cannot be
/// built without a DAC.
pub trait Ring: Send + Sync {
    fn writable_bytes(&self) -> usize;
    fn write_i32(&self, samples: &[i32]) -> Result<usize>;
    fn play(&self) -> Result<()>;
    fn pause(&self) -> Result<()>;
    fn flush(&self) -> Result<()>;
    fn drain(&self) -> Result<()>;
    fn is_drained(&self) -> bool;
    fn played_frames(&self) -> u64;
}

impl Ring for DacStream {
    fn writable_bytes(&self) -> usize {
        DacStream::writable_bytes(self)
    }
    fn write_i32(&self, samples: &[i32]) -> Result<usize> {
        DacStream::write_i32(self, samples)
    }
    fn play(&self) -> Result<()> {
        DacStream::play(self)
    }
    fn pause(&self) -> Result<()> {
        DacStream::pause(self)
    }
    fn flush(&self) -> Result<()> {
        DacStream::flush(self)
    }
    fn drain(&self) -> Result<()> {
        DacStream::drain(self)
    }
    fn is_drained(&self) -> bool {
        DacStream::is_drained(self)
    }
    fn played_frames(&self) -> u64 {
        DacStream::played_frames(self)
    }
}

/// What the writer needs to know about the wire format.
#[derive(Clone, Copy, Debug)]
pub struct Shape {
    pub channels: usize,
    /// Bytes of one sample on the wire (`subslot_bytes`): the unit `writable_bytes` counts in.
    pub bytes_per_sample: usize,
    pub sample_rate: u32,
    /// The rate of `played_frames`.
    pub counter_rate_hz: u32,
}

impl From<&StreamFormat> for Shape {
    fn from(format: &StreamFormat) -> Self {
        Self {
            channels: usize::from(format.channels),
            bytes_per_sample: usize::from(format.subslot_bytes),
            sample_rate: format.sample_rate,
            counter_rate_hz: format.counter_rate_hz(),
        }
    }
}
// endregion

// region rust-player
/// What the writer thread and the control calls share, under one lock so that a seek can never
/// interleave with a half-written chunk.
struct Shared {
    source: Box<dyn Source>,
    /// A rendered chunk that the ring has not fully taken yet.
    chunk: Vec<i32>,
    taken: usize,
    ended: bool,
    /// The source time of the first frame written since the last flush.
    start_seconds: f64,
}

fn lock(shared: &Mutex<Shared>) -> MutexGuard<'_, Shared> {
    shared.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

pub struct Player<R: Ring + 'static> {
    ring: Arc<R>,
    shape: Shape,
    shared: Arc<Mutex<Shared>>,
    stop: Arc<AtomicBool>,
    writer: Option<JoinHandle<()>>,
}

impl<R: Ring + 'static> Player<R> {
    /// Starts the writer thread. The stream starts by itself once the ring holds `start_threshold_ms`
    /// of audio (100 ms by default); `play` is only for after a pause.
    pub fn start(ring: Arc<R>, shape: Shape, source: Box<dyn Source>) -> Self {
        let shared = Arc::new(Mutex::new(Shared { source, chunk: Vec::new(), taken: 0, ended: false, start_seconds: 0.0 }));
        let stop = Arc::new(AtomicBool::new(false));
        let writer = {
            let (ring, shared, stop) = (Arc::clone(&ring), Arc::clone(&shared), Arc::clone(&stop));
            thread::Builder::new()
                .name("host-writer".into())
                .spawn(move || write_loop(&*ring, shape, &shared, &stop))
                .expect("spawn the writer thread")
        };
        Self { ring, shape, shared, stop, writer: Some(writer) }
    }

    /// Keep the writer running while paused: it fills the ring to its target and idles, so a resume
    /// loses nothing.
    pub fn pause(&self) -> Result<()> {
        self.ring.pause()
    }

    pub fn resume(&self) -> Result<()> {
        self.ring.play()
    }

    /// Reposition the decoder, drop what was rendered for the old position, then flush. The old audio
    /// stops within a transfer or two, and `played_frames` starts again from zero.
    pub fn seek(&self, seconds: f64) -> Result<()> {
        let mut state = lock(&self.shared);
        state.source.seek(seconds);
        state.chunk.clear();
        state.taken = 0;
        state.ended = false;
        state.start_seconds = seconds;
        self.ring.flush()
    }

    /// The audible position: where the last flush started plus what the DAC has completed since.
    pub fn position_seconds(&self) -> f64 {
        let start = lock(&self.shared).start_seconds;
        start + self.ring.played_frames() as f64 / f64::from(self.shape.counter_rate_hz)
    }

    /// The decoder reached the end and the DAC has played everything.
    pub fn finished(&self) -> bool {
        lock(&self.shared).ended && self.ring.is_drained()
    }

    pub fn ring(&self) -> &R {
        &self.ring
    }

    /// Stops the writer and returns the ring, so that the caller can close it.
    pub fn stop(mut self) -> Arc<R> {
        self.stop.store(true, Ordering::Release);
        if let Some(writer) = self.writer.take() {
            let _ = writer.join();
        }
        Arc::clone(&self.ring)
    }
}

impl<R: Ring + 'static> Drop for Player<R> {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(writer) = self.writer.take() {
            let _ = writer.join();
        }
    }
}

fn write_loop(ring: &dyn Ring, shape: Shape, shared: &Mutex<Shared>, stop: &AtomicBool) {
    // About 10 ms of audio per render call, in whole frames.
    let mut scratch = vec![0i32; (shape.sample_rate as usize / 100).max(1) * shape.channels];
    while !stop.load(Ordering::Acquire) {
        let mut state = lock(shared);
        if state.ended {
            drop(state);
            thread::sleep(Duration::from_millis(10));
            continue;
        }
        if state.taken == state.chunk.len() {
            // Render only when the ring wants this much, so the decoder is never ahead of the ring's
            // target depth and a seek has little to throw away.
            if ring.writable_bytes() < scratch.len() * shape.bytes_per_sample {
                drop(state);
                thread::sleep(Duration::from_millis(2));
                continue;
            }
            let rendered = state.source.render(&mut scratch);
            if rendered == 0 {
                // The end of the track: let the ring play out, even a tail shorter than the start threshold.
                if ring.drain().is_ok() {
                    state.ended = true;
                }
                continue;
            }
            state.chunk.clear();
            state.chunk.extend_from_slice(&scratch[..rendered]);
            state.taken = 0;
        }
        // Never blocks. A count short of what was offered, or zero, means the ring is full.
        match ring.write_i32(&state.chunk[state.taken..]) {
            Ok(0) => {
                drop(state);
                thread::sleep(Duration::from_millis(2));
            }
            Ok(taken) => state.taken += taken,
            Err(error) => {
                // The stream closed or failed. The event listener has the reason.
                log::warn!("the writer stops: {error}");
                return;
            }
        }
    }
}
// endregion

// region rust-events
/// A listener that hands events to a thread of the host's choice. It runs on the library's event
/// thread, must not block, and must not call `Dac::close`. An unbounded channel's `send` does neither.
pub fn event_channel() -> (Arc<dyn DacListener>, Receiver<DacEvent>) {
    let (tx, rx) = channel();
    let listener: Arc<dyn DacListener> = Arc::new(move |event: DacEvent| {
        let _ = tx.send(event);
    });
    (listener, rx)
}

/// What the host does about each event.
pub enum Reaction {
    Nothing,
    /// Count it; look at the decoder's read-ahead. Playback goes on with silence in the gap.
    Underrun,
    /// A sliver of audio never reached the DAC. The stream is not bit-perfect from here on.
    NotBitPerfect,
    /// Pause, say why, and do not carry on through another output unless the listener asks.
    Pause(&'static str),
    /// The stream failed. Stop the writer, close the stream and decide what to do next.
    Failed(Error),
}

pub fn react(event: DacEvent) -> Reaction {
    match event {
        DacEvent::Underrun => Reaction::Underrun,
        DacEvent::LostFrames { .. } => Reaction::NotBitPerfect,
        DacEvent::FeedbackStale => Reaction::Nothing, // the stream recovers, or fails with its own event
        DacEvent::DeviceLost => Reaction::Pause("the DAC was unplugged"),
        DacEvent::Error(error) => Reaction::Failed(error),
        // The state, the bit-perfect flag and the volume are read when the display needs them.
        _ => Reaction::Nothing,
    }
}
// endregion

// region rust-volume
/// When a DAC connects: the library has already lowered it to at most -40 dB. Restore the level the
/// listener had for this DAC, but never above what the DAC itself had.
pub fn restore_level(volume: &VolumeControl, remembered_db: Option<f64>) -> Result<f64> {
    let ceiling = volume.startup().limit_db;
    match remembered_db {
        Some(db) => volume.set_db(db.min(ceiling)).map(|state| state.db),
        None => volume.get().map(|state| state.db),
    }
}

/// A volume key or a slider. Each call is a USB control transfer (milliseconds, up to a second):
/// call it from a control thread and not from the writer or a UI thread.
pub fn nudge(volume: &VolumeControl, up: bool) -> Result<f64> {
    volume.step(if up { 1 } else { -1 }, 0).map(|state| state.db)
}
// endregion

// region rust-bit-perfect
/// One line for a status display. The library's flag covers the transport; the host adds what it did
/// to the samples and what the source was.
pub fn status_line(stream: &DacStream, engine_untouched: bool, lossy_source: bool) -> String {
    if !engine_untouched {
        return "Not bit-perfect: the player is processing the audio".to_string();
    }
    match stream.bit_perfect_blocker() {
        None if lossy_source => "Direct USB, decoded from a lossy file".to_string(),
        None => "Bit-perfect".to_string(),
        Some(blocker) => format!("Not bit-perfect: {}", blocker_text(blocker)),
    }
}

fn blocker_text(blocker: BitPerfectBlocker) -> &'static str {
    match blocker {
        BitPerfectBlocker::LostFrames => "audio was lost on the USB bus",
        BitPerfectBlocker::FormatChanged => "the DAC runs another rate than the track has",
        BitPerfectBlocker::ClockUnverified => "the DAC did not confirm its sampling rate",
        BitPerfectBlocker::SoftwareGain => "software gain is on",
        BitPerfectBlocker::UnsynchronizedAsync => "the DAC has no feedback, so it plays at the nominal rate",
        BitPerfectBlocker::FeedbackUnlocked => "the DAC's timing has not locked yet",
        BitPerfectBlocker::PayloadUnverified => "waiting for real audio",
        BitPerfectBlocker::NoStream => "no stream is running",
        // `BitPerfectBlocker` is non-exhaustive.
        _ => "the stream is not verified",
    }
}
// endregion

// region rust-close
/// Stops the writer, closes the stream, then the DAC. The host releases its interfaces and its
/// descriptor after this returns, never before.
///
/// `device_lost` is true when the DAC was unplugged: nothing can be written to it, and its recorded
/// level must not outlive the attachment.
pub fn shutdown(player: Player<DacStream>, dac: Dac, device_lost: bool) -> Result<()> {
    let stream = player.stop(); // joins the writer thread
    let stream_result = match Arc::try_unwrap(stream) {
        Ok(stream) => stream.close(),
        Err(_) => Err(Error::InvalidState("the stream is still shared".to_string())),
    };
    let dac_result = if device_lost { dac.close_with(CloseVolume::Forget) } else { dac.close() };
    if matches!(dac_result, Err(Error::Transfer(_))) {
        // The output thread did not stop in time. Keep every claim and the descriptor until it has.
        while !Dac::await_usb_retirement(Duration::from_secs(1)) {}
    }
    stream_result.and(dac_result)
}
// endregion

// region rust-play-one
/// How a host that plays one track fits the pieces together. `format` comes from `negotiate`.
pub fn play_one(dac: Dac, format: &StreamFormat, source: Box<dyn Source>) -> Result<()> {
    let (listener, events) = event_channel();
    let stream = dac.open_stream(format, listener)?;
    let shape = Shape::from(stream.format());
    let player = Player::start(Arc::new(stream), shape, source);
    let mut device_lost = false;
    while !player.finished() {
        match events.recv_timeout(Duration::from_millis(100)).map(react) {
            Ok(Reaction::Pause(reason)) => {
                log::warn!("{reason}");
                device_lost = true;
                break;
            }
            Ok(Reaction::Failed(error)) => {
                device_lost = error == Error::DeviceLost;
                log::warn!("the stream failed: {error}");
                break;
            }
            _ => {}
        }
    }
    log::info!("{}", status_line(player.ring(), true, false));
    shutdown(player, dac, device_lost)
}
// endregion

fn main() {
    println!("guide_player is a compile-checked example of docs/sdk/guide-rust.md; it needs a DAC to run.");
}
