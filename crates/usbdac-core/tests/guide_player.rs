//! Tests the writer thread and the transport of `examples/guide_player.rs` (the code of
//! `docs/sdk/guide-rust.md`) against a fake ring, so the rules the guide states are checked with no DAC.

#[path = "../examples/guide_player.rs"]
mod guide;

use guide::{Player, Ring, Shape, Source};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use usbdac_core::Result;

// region rust-fake-ring
/// A ring with a fixed amount of room that records what it is given.
struct FakeRing {
    room_bytes: AtomicUsize,
    written: Mutex<Vec<i32>>,
    calls: Mutex<Vec<&'static str>>,
    drained: AtomicBool,
    played: AtomicU64,
}

impl FakeRing {
    fn new(room_bytes: usize) -> Arc<Self> {
        Arc::new(Self {
            room_bytes: AtomicUsize::new(room_bytes),
            written: Mutex::new(Vec::new()),
            calls: Mutex::new(Vec::new()),
            drained: AtomicBool::new(false),
            played: AtomicU64::new(0),
        })
    }
    fn call(&self, name: &'static str) -> Result<()> {
        self.calls.lock().unwrap().push(name);
        Ok(())
    }
    fn calls(&self) -> Vec<&'static str> {
        self.calls.lock().unwrap().clone()
    }
}

impl Ring for FakeRing {
    fn writable_bytes(&self) -> usize {
        self.room_bytes.load(Ordering::SeqCst)
    }
    fn write_i32(&self, samples: &[i32]) -> Result<usize> {
        self.written.lock().unwrap().extend_from_slice(samples);
        Ok(samples.len())
    }
    fn play(&self) -> Result<()> {
        self.call("play")
    }
    fn pause(&self) -> Result<()> {
        self.call("pause")
    }
    fn flush(&self) -> Result<()> {
        self.written.lock().unwrap().clear();
        self.played.store(0, Ordering::SeqCst);
        self.call("flush")
    }
    fn drain(&self) -> Result<()> {
        self.call("drain")
    }
    fn is_drained(&self) -> bool {
        self.drained.load(Ordering::SeqCst)
    }
    fn played_frames(&self) -> u64 {
        self.played.load(Ordering::SeqCst)
    }
}
// endregion

/// Stereo, 4 bytes per sample, 1 kHz: a render call is 10 frames, 20 samples, 80 bytes.
const SHAPE: Shape = Shape {
    channels: 2,
    bytes_per_sample: 4,
    sample_rate: 1_000,
    counter_rate_hz: 1_000,
};

/// A source whose samples say where it is: every sample is the index of its frame, counting from the
/// last seek. It ends after `frames` frames.
struct Counter {
    next: i32,
    frames: i32,
}

impl Source for Counter {
    fn render(&mut self, out: &mut [i32]) -> usize {
        let mut written = 0;
        for frame in out.chunks_exact_mut(2) {
            if self.next >= self.frames {
                break;
            }
            frame.fill(self.next);
            self.next += 1;
            written += 2;
        }
        written
    }
    fn seek(&mut self, seconds: f64) {
        self.next = (seconds * 1_000.0) as i32 + 100_000; // frames of the new position are told apart from the old
    }
}

fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(2));
    }
}

#[test]
fn renders_only_when_the_ring_asks_for_audio() {
    let ring = FakeRing::new(0);
    let player = Player::start(
        ring.clone(),
        SHAPE,
        Box::new(Counter {
            next: 0,
            frames: 1_000,
        }),
    );
    std::thread::sleep(Duration::from_millis(50));
    assert!(
        ring.written.lock().unwrap().is_empty(),
        "nothing is rendered while the ring has no room"
    );

    ring.room_bytes.store(10_000, Ordering::SeqCst);
    wait_until("the first audio", || {
        !ring.written.lock().unwrap().is_empty()
    });
    drop(player);
}

#[test]
fn drains_once_at_the_end_and_finishes_when_the_ring_has_played_out() {
    let ring = FakeRing::new(1_000_000);
    let player = Player::start(
        ring.clone(),
        SHAPE,
        Box::new(Counter {
            next: 0,
            frames: 50,
        }),
    );
    wait_until("the drain", || ring.calls().contains(&"drain"));
    assert_eq!(ring.written.lock().unwrap().len(), 100); // 50 frames of two samples
    assert!(
        !player.finished(),
        "not finished until the ring has played the tail"
    );
    ring.drained.store(true, Ordering::SeqCst);
    assert!(player.finished());
    assert_eq!(ring.calls().iter().filter(|c| **c == "drain").count(), 1);
}

// region rust-seek-test
#[test]
fn a_seek_drops_the_chunk_that_was_rendered_for_the_old_position() {
    // The ring takes everything, so the chunk in hand is whatever the ring has not accepted yet.
    let ring = FakeRing::new(1_000_000);
    let player = Player::start(
        ring.clone(),
        SHAPE,
        Box::new(Counter {
            next: 0,
            frames: 10_000_000,
        }),
    );
    wait_until("some audio", || ring.written.lock().unwrap().len() > 200);

    player.seek(2.0).unwrap();
    assert!(ring.calls().contains(&"flush"));
    let after = ring.written.lock().unwrap().clone();
    // Everything written after the flush comes from the new position.
    wait_until("audio at the new position", || {
        ring.written.lock().unwrap().len() > after.len() + 100
    });
    let written = ring.written.lock().unwrap().clone();
    assert!(
        written.iter().all(|sample| *sample >= 102_000),
        "old audio was written after the flush: {:?}",
        &written[..8]
    );
}
// endregion

#[test]
fn the_position_is_the_last_seek_plus_what_the_dac_has_played() {
    let ring = FakeRing::new(0);
    let player = Player::start(
        ring.clone(),
        SHAPE,
        Box::new(Counter {
            next: 0,
            frames: 1_000,
        }),
    );
    player.seek(30.0).unwrap();
    ring.played.store(2_500, Ordering::SeqCst); // 2.5 s at 1 kHz
    assert!((player.position_seconds() - 32.5).abs() < 1e-9);
}

#[test]
fn pause_and_resume_reach_the_ring_and_the_writer_keeps_running() {
    let ring = FakeRing::new(1_000_000);
    let player = Player::start(
        ring.clone(),
        SHAPE,
        Box::new(Counter {
            next: 0,
            frames: 100_000,
        }),
    );
    player.pause().unwrap();
    player.resume().unwrap();
    assert_eq!(ring.calls(), vec!["pause", "play"]);
    wait_until("audio while paused and resumed", || {
        ring.written.lock().unwrap().len() > 100
    });
}
