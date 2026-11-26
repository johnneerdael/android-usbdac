#![allow(dead_code)]
//! A host with a decoder that renders audio on request ("pull") driving the
//! push API from a thread of its own, with seek, pause and position.
//!
//! The library never calls back into the host on the audio path. The adapter
//! keeps the ring near its target depth by asking `writable_bytes()` and writing
//! only then. It cannot run without a DAC; `cargo check --examples` keeps it
//! compiling.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::Duration;
use usbdac_core::{DacStream, Result};

/// The host's decoder, as the adapter sees it.
trait Render: Send {
    /// Fills `out` with wire-layout bytes (see the `DacStream` documentation for
    /// the layout of each encoding) in whole frames and returns how many bytes
    /// it wrote; 0 at the end of the track.
    fn render(&mut self, out: &mut [u8]) -> usize;
    fn seek(&mut self, seconds: f64);
}

/// Everything the writer thread and the control calls share, under one lock so a
/// seek can never interleave with a half-written chunk.
struct State {
    source: Box<dyn Render>,
    /// A rendered chunk the ring has not fully taken yet.
    chunk: Vec<u8>,
    taken: usize,
    ended: bool,
    /// Source time of the first frame written since the last flush.
    base_seconds: f64,
}

struct PullAdapter {
    stream: Arc<DacStream>,
    state: Arc<Mutex<State>>,
    stop: Arc<AtomicBool>,
    writer: Option<JoinHandle<()>>,
}

fn lock(state: &Mutex<State>) -> MutexGuard<'_, State> {
    state.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl PullAdapter {
    fn start(stream: Arc<DacStream>, source: Box<dyn Render>) -> Self {
        let state = Arc::new(Mutex::new(State {
            source,
            chunk: Vec::new(),
            taken: 0,
            ended: false,
            base_seconds: 0.0,
        }));
        let stop = Arc::new(AtomicBool::new(false));
        let writer = {
            let (stream, state, stop) = (Arc::clone(&stream), Arc::clone(&state), Arc::clone(&stop));
            thread::Builder::new()
                .name("host-writer".into())
                .spawn(move || write_loop(&stream, &state, &stop))
                .expect("spawn the writer thread")
        };
        Self {
            stream,
            state,
            stop,
            writer: Some(writer),
        }
    }

    /// Pause keeps the writer running: it fills the ring to its target and then
    /// idles on `writable_bytes() == 0`, so resume is instant.
    fn pause(&self) -> Result<()> {
        self.stream.pause()
    }

    fn resume(&self) -> Result<()> {
        self.stream.play()
    }

    /// Seek: reposition the decoder, drop what was rendered for the old
    /// position, then flush the ring. After `flush()` the stream holds silence
    /// until the start threshold of new audio has arrived, and `played_frames()`
    /// counts from zero again.
    fn seek(&self, seconds: f64) -> Result<()> {
        let mut state = lock(&self.state);
        state.source.seek(seconds);
        state.chunk.clear();
        state.taken = 0;
        state.ended = false;
        state.base_seconds = seconds;
        self.stream.flush()
    }

    /// The position that is audible right now: where the last flush started,
    /// plus the music the DAC has completed since.
    fn position_seconds(&self) -> f64 {
        let base = lock(&self.state).base_seconds;
        base + self.stream.played_frames() as f64 / f64::from(self.stream.format().counter_rate_hz())
    }

    /// The decoder is at the end of the track and the DAC has played it all.
    fn finished(&self) -> bool {
        lock(&self.state).ended && self.stream.is_drained()
    }
}

impl Drop for PullAdapter {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(writer) = self.writer.take() {
            let _ = writer.join();
        }
    }
}

fn write_loop(stream: &DacStream, state: &Mutex<State>, stop: &AtomicBool) {
    let frame = stream.format().bytes_per_frame();
    // About 10 ms per render call.
    let chunk_frames = (stream.format().sample_rate as usize / 100).max(1);
    let mut scratch = vec![0u8; chunk_frames * frame];
    while !stop.load(Ordering::Acquire) {
        let mut state = lock(state);
        if state.ended {
            drop(state);
            thread::sleep(Duration::from_millis(10));
            continue;
        }
        // Render only when the ring wants more, so the decoder is never ahead of
        // the configured buffer depth (and a seek never has much to throw away).
        if state.taken == state.chunk.len() {
            if stream.writable_bytes() < scratch.len() {
                drop(state);
                thread::sleep(Duration::from_millis(2));
                continue;
            }
            let rendered = state.source.render(&mut scratch);
            if rendered == 0 {
                // No more audio: let the ring play out, even a short tail.
                if stream.drain().is_ok() {
                    state.ended = true;
                }
                continue;
            }
            state.chunk.clear();
            state.chunk.extend_from_slice(&scratch[..rendered]);
            state.taken = 0;
        }
        match stream.write(&state.chunk[state.taken..]) {
            Ok(0) => {
                drop(state);
                thread::sleep(Duration::from_millis(2));
            }
            Ok(taken) => state.taken += taken,
            Err(error) => {
                // The stream closed or failed; the listener has the details.
                eprintln!("writer stopping: {error}");
                return;
            }
        }
    }
}

fn main() {
    // Needs a DAC; see the README for how a host obtains the file descriptor.
    println!("pull_adapter is a compile-checked example, see the source.");
}
