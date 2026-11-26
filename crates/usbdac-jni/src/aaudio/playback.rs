//! The audio side of an AAudio output: the ring the host writes into, the render function AAudio's
//! data callback runs, the transport state (buffering, playing, paused, draining) and the
//! counters that turn AAudio's position into "music frames played since the last flush".
//!
//! The host calls `write`, `flush`, `pause`, `play`, `drain` and the getters from its threads; the
//! platform calls `render` from its real-time callback thread. `render` takes no lock and does not
//! allocate. Everything here is plain Rust, so the whole transport is tested on the host with a
//! simulated device.
//!
//! ### Position
//!
//! The device is fed continuously: when the ring cannot supply audio (before the start threshold,
//! while paused, after an underrun) `render` supplies silence, so AAudio's frame counters count
//! music and silence together. `Playback` counts both separately:
//!
//! - `music`: music frames handed to the device, in total;
//! - `silence`: silence frames handed to the device, in total;
//! - `silence_before_music`: the silence frames handed over before the last music frame.
//!
//! If the device has presented `p` frames in all, the music among them is
//! `min(music, p - silence_before_music)`: past the last music frame every further frame is
//! silence, and before it the silence frames that came after position `p` are at most the
//! device's own buffer, so the estimate lags by at most that much and never leads. The value
//! reported is made monotonic between flushes.

use super::logic::{AaudioError, OpenRequest};
use super::ring::SpscRing;
use parking_lot::Mutex;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering};

/// The largest ring the output will allocate.
pub const MAX_RING_BYTES: usize = 64 * 1024 * 1024;

/// What the data callback tells AAudio.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Callback {
    Continue,
    Stop,
}

impl Callback {
    pub fn aaudio_value(self) -> i32 {
        match self {
            Self::Continue => 0,
            Self::Stop => 1,
        }
    }
}

/// The transport state, with the codes `StreamState` uses on the Kotlin side.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Buffering = 0,
    Playing = 1,
    Paused = 2,
    Draining = 3,
    Drained = 4,
    Closed = 5,
    Failed = 6,
}

#[derive(Default)]
struct Producer {
    /// Frames accepted since the stream opened.
    written: u64,
    /// `written` at the last flush.
    written_at_flush: u64,
}

pub struct Playback {
    ring: SpscRing,
    frame_bytes: usize,
    target_frames: u64,
    start_frames: u64,
    producer: Mutex<Producer>,

    // Transport flags, set by the host.
    paused: AtomicBool,
    draining: AtomicBool,
    closing: AtomicBool,
    failed: AtomicBool,
    failure: AtomicI32,

    // Flush handshake: the host bumps `flush_requested`; the callback applies it and echoes the
    // number in `flush_applied`, recording the music count at that moment in `flush_mark`.
    flush_requested: AtomicU64,
    flush_applied: AtomicU64,
    flush_mark: AtomicU64,

    // Published by the callback.
    playing: AtomicBool,
    music: AtomicU64,
    silence: AtomicU64,
    silence_before_music: AtomicU64,
    underruns: AtomicU64,

    // Largest value `played_frames` has reported since the last flush.
    reported: AtomicU64,
}

impl Playback {
    pub fn new(request: &OpenRequest) -> Result<Self, AaudioError> {
        let frame_bytes = request.frame_bytes();
        let capacity_frames = request.frames_for_ms(request.buffer_capacity_ms).max(1);
        let capacity_bytes = usize::try_from(capacity_frames)
            .ok()
            .and_then(|frames| frames.checked_mul(frame_bytes))
            .filter(|bytes| *bytes <= MAX_RING_BYTES)
            .ok_or_else(|| {
                AaudioError::invalid_argument(format!(
                    "a {} ms buffer at this rate and format is larger than {} MiB",
                    request.buffer_capacity_ms,
                    MAX_RING_BYTES >> 20
                ))
            })?;
        Ok(Self {
            ring: SpscRing::new(capacity_bytes),
            frame_bytes,
            target_frames: request.frames_for_ms(request.buffer_target_ms).max(1),
            start_frames: request.frames_for_ms(request.start_threshold_ms).max(1),
            producer: Mutex::new(Producer::default()),
            paused: AtomicBool::new(false),
            draining: AtomicBool::new(false),
            closing: AtomicBool::new(false),
            failed: AtomicBool::new(false),
            failure: AtomicI32::new(0),
            flush_requested: AtomicU64::new(0),
            flush_applied: AtomicU64::new(0),
            flush_mark: AtomicU64::new(0),
            playing: AtomicBool::new(false),
            music: AtomicU64::new(0),
            silence: AtomicU64::new(0),
            silence_before_music: AtomicU64::new(0),
            underruns: AtomicU64::new(0),
            reported: AtomicU64::new(0),
        })
    }

    pub fn frame_bytes(&self) -> usize {
        self.frame_bytes
    }

    // ---- the callback side ---------------------------------------------------------------

    /// Fills `out` (whole frames) for the device. Real-time: no lock, no allocation.
    pub fn render(&self, out: &mut [u8]) -> Callback {
        let frame_bytes = self.frame_bytes;
        let whole = out.len() / frame_bytes * frame_bytes;
        let (out, tail) = out.split_at_mut(whole);
        tail.fill(0);

        // A flush the host asked for since the last callback.
        let requested = self.flush_requested.load(Ordering::Acquire);
        if requested != self.flush_applied.load(Ordering::Relaxed) {
            self.ring.apply_discard();
            self.flush_mark
                .store(self.music.load(Ordering::Relaxed), Ordering::Relaxed);
            self.playing.store(false, Ordering::Relaxed);
            self.flush_applied.store(requested, Ordering::Release);
        }

        let frames = (whole / frame_bytes) as u64;
        if self.closing.load(Ordering::Acquire) || self.failed.load(Ordering::Acquire) {
            out.fill(0);
            self.silence.fetch_add(frames, Ordering::Relaxed);
            return Callback::Stop;
        }
        if self.paused.load(Ordering::Acquire) {
            out.fill(0);
            self.silence.fetch_add(frames, Ordering::Relaxed);
            return Callback::Continue;
        }

        let draining = self.draining.load(Ordering::Acquire);
        if !self.playing.load(Ordering::Relaxed) {
            let threshold = if draining { 1 } else { self.start_frames };
            if (self.ring.len() / frame_bytes) as u64 >= threshold {
                self.playing.store(true, Ordering::Relaxed);
            }
        }

        let mut music = 0u64;
        if self.playing.load(Ordering::Relaxed) {
            let taken = self.ring.read(out);
            music = (taken / frame_bytes) as u64;
            out[taken..].fill(0);
            if taken < out.len() && !draining {
                // The ring ran dry: wait for the start threshold again instead of stuttering.
                self.underruns.fetch_add(1, Ordering::Relaxed);
                self.playing.store(false, Ordering::Relaxed);
            }
        } else {
            out.fill(0);
        }

        let silence_so_far = self.silence.load(Ordering::Relaxed);
        if music > 0 {
            self.silence_before_music
                .store(silence_so_far, Ordering::Relaxed);
            self.music.fetch_add(music, Ordering::Release);
        }
        self.silence.fetch_add(frames - music, Ordering::Release);
        Callback::Continue
    }

    // ---- the host side -------------------------------------------------------------------

    /// Takes whole frames, as many as fit, and returns the bytes taken.
    pub fn write(&self, bytes: &[u8]) -> Result<usize, AaudioError> {
        self.check_writable()?;
        let whole = bytes.len() / self.frame_bytes * self.frame_bytes;
        if whole == 0 {
            return Ok(0);
        }
        let mut producer = self.producer.lock();
        let free = self.ring.free() / self.frame_bytes * self.frame_bytes;
        let take = whole.min(free);
        let taken = self.ring.write(&bytes[..take]);
        producer.written += (taken / self.frame_bytes) as u64;
        Ok(taken)
    }

    fn check_writable(&self) -> Result<(), AaudioError> {
        if self.failed.load(Ordering::Acquire) {
            return Err(AaudioError::device_lost(
                "the audio route was disconnected or failed; close the output",
            ));
        }
        if self.closing.load(Ordering::Acquire) {
            return Err(AaudioError::invalid_state("the output is closed"));
        }
        if self.draining.load(Ordering::Acquire) {
            return Err(AaudioError::invalid_state(
                "the output is drained; flush before writing",
            ));
        }
        Ok(())
    }

    /// The room up to the target depth, a whole number of frames; 0 once drained or failed.
    pub fn writable_bytes(&self) -> usize {
        if self.failed.load(Ordering::Acquire)
            || self.closing.load(Ordering::Acquire)
            || self.draining.load(Ordering::Acquire)
        {
            return 0;
        }
        let queued = self.ring.len() / self.frame_bytes;
        let room = (self.target_frames as usize).saturating_sub(queued);
        let free = self.ring.free() / self.frame_bytes;
        room.min(free) * self.frame_bytes
    }

    pub fn pause(&self) {
        self.paused.store(true, Ordering::Release);
    }

    pub fn play(&self) {
        self.paused.store(false, Ordering::Release);
    }

    /// Drops everything buffered and restarts the position at 0. Music the device already holds
    /// still plays, and is not counted.
    pub fn flush(&self) {
        let mut producer = self.producer.lock();
        self.ring.request_discard_all();
        producer.written_at_flush = producer.written;
        self.draining.store(false, Ordering::Release);
        self.reported.store(0, Ordering::Relaxed);
        self.flush_requested.fetch_add(1, Ordering::Release);
    }

    /// Declares the end of the audio: the ring plays out, a tail shorter than the start threshold
    /// included.
    pub fn drain(&self) {
        self.draining.store(true, Ordering::Release);
    }

    pub fn begin_close(&self) {
        self.closing.store(true, Ordering::Release);
    }

    /// The route failed (`code` is the AAudio error); further writes are `device_lost`.
    pub fn fail(&self, code: i32) {
        self.failure.store(code, Ordering::Relaxed);
        self.failed.store(true, Ordering::Release);
    }

    #[cfg(test)]
    pub fn failure_code(&self) -> Option<i32> {
        self.failed
            .load(Ordering::Acquire)
            .then(|| self.failure.load(Ordering::Relaxed))
    }

    pub fn underruns(&self) -> u64 {
        self.underruns.load(Ordering::Relaxed)
    }

    // ---- position -------------------------------------------------------------------------

    /// Whether the callback has applied every flush the host asked for.
    fn flush_settled(&self) -> bool {
        self.flush_applied.load(Ordering::Acquire) == self.flush_requested.load(Ordering::Acquire)
    }

    /// Music frames the device has presented in total, given that it has presented `presented`
    /// frames in all.
    fn music_presented(&self, presented: i64) -> u64 {
        let music = self.music.load(Ordering::Acquire);
        let before = self.silence_before_music.load(Ordering::Relaxed);
        let beyond_silence = u64::try_from(presented).unwrap_or(0).saturating_sub(before);
        music.min(beyond_silence)
    }

    /// Music frames played since the start or the last flush. `presented` is the frame count the
    /// device has presented in all (see `logic::presented_position`).
    pub fn played_frames(&self, presented: i64) -> u64 {
        if !self.flush_settled() {
            return 0;
        }
        let mark = self.flush_mark.load(Ordering::Relaxed);
        let played = self.music_presented(presented).saturating_sub(mark);
        self.reported
            .fetch_max(played, Ordering::Relaxed)
            .max(played)
    }

    /// Music between the host and the device: the ring plus what the device holds.
    pub fn latency_frames(&self, presented: i64) -> u64 {
        let in_ring = (self.ring.len_after_flush() / self.frame_bytes) as u64;
        let delivered = self.music.load(Ordering::Acquire);
        let in_device = delivered.saturating_sub(self.music_presented(presented));
        if self.flush_settled() {
            in_ring + in_device
        } else {
            in_ring
        }
    }

    /// Everything written since the last flush has been presented.
    pub fn is_drained(&self, presented: i64) -> bool {
        if !self.draining.load(Ordering::Acquire) || !self.flush_settled() {
            return false;
        }
        let producer = self.producer.lock();
        let owed = producer.written - producer.written_at_flush;
        drop(producer);
        self.ring.len_after_flush() == 0 && self.played_frames(presented) >= owed
    }

    pub fn state(&self, presented: i64) -> State {
        if self.failed.load(Ordering::Acquire) {
            State::Failed
        } else if self.closing.load(Ordering::Acquire) {
            State::Closed
        } else if self.draining.load(Ordering::Acquire) {
            if self.is_drained(presented) {
                State::Drained
            } else {
                State::Draining
            }
        } else if self.paused.load(Ordering::Acquire) {
            State::Paused
        } else if self.playing.load(Ordering::Relaxed) {
            State::Playing
        } else {
            State::Buffering
        }
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::aaudio::logic::{OpenRequest, Performance, SampleFormat};

    /// 1000 Hz mono 16-bit: one frame is two bytes and a millisecond is one frame, so frame
    /// counts and times read the same in the tests. (Built directly: `validate` would refuse the
    /// rate.)
    pub fn request(target_ms: u32, capacity_ms: u32, start_ms: u32) -> OpenRequest {
        OpenRequest {
            sample_rate: 1_000,
            channels: 1,
            format: SampleFormat::I16,
            device_id: 0,
            performance: Performance::LowLatency,
            buffer_target_ms: target_ms,
            buffer_capacity_ms: capacity_ms,
            start_threshold_ms: start_ms,
            device_buffer_bursts: None,
        }
    }

    /// 8 frames of 16-bit mono are 16 bytes; a frame's two bytes both carry its number so music
    /// can be told from silence (zero) and from other music.
    fn frames(first: u16, count: usize) -> Vec<u8> {
        (0..count)
            .flat_map(|i| {
                let value = (first as usize + i + 1) as u16; // never zero
                value.to_le_bytes()
            })
            .collect()
    }

    fn value_at(bytes: &[u8], frame: usize) -> u16 {
        u16::from_le_bytes([bytes[frame * 2], bytes[frame * 2 + 1]])
    }

    /// A device that asks for audio in bursts and presents it `latency` frames late.
    pub struct SimDevice {
        pub playback: std::sync::Arc<Playback>,
        pub delivered: Vec<u8>,
        pub total_frames: i64,
        pub latency: i64,
    }

    impl SimDevice {
        pub fn new(playback: Playback, latency: i64) -> Self {
            Self {
                playback: std::sync::Arc::new(playback),
                delivered: Vec::new(),
                total_frames: 0,
                latency,
            }
        }

        pub fn callback(&mut self, frames: usize) {
            let mut block = vec![0u8; frames * self.playback.frame_bytes()];
            self.playback.render(&mut block);
            self.delivered.extend_from_slice(&block);
            self.total_frames += frames as i64;
        }

        /// Frames the device has presented: what it was given, minus what is still in its buffer.
        pub fn presented(&self) -> i64 {
            (self.total_frames - self.latency).max(0)
        }

        pub fn played(&self) -> u64 {
            self.playback.played_frames(self.presented())
        }
    }

    fn device(target: u32, capacity: u32, start: u32, latency: i64) -> SimDevice {
        SimDevice::new(
            Playback::new(&request(target, capacity, start)).unwrap(),
            latency,
        )
    }

    #[test]
    fn nothing_plays_until_the_start_threshold_is_buffered() {
        let mut device = device(100, 200, 20, 0);
        assert_eq!(device.playback.write(&frames(0, 10)).unwrap(), 20);
        device.callback(8);
        assert!(
            device.delivered.iter().all(|byte| *byte == 0),
            "silence while buffering"
        );
        assert_eq!(device.playback.state(device.presented()), State::Buffering);
        assert_eq!(device.playback.write(&frames(10, 10)).unwrap(), 20);
        device.callback(8);
        assert_eq!(value_at(&device.delivered[16..], 0), 1);
        assert_eq!(device.playback.state(device.presented()), State::Playing);
    }

    #[test]
    fn the_bytes_come_out_exactly_as_they_went_in() {
        let mut device = device(100, 200, 10, 0);
        let input = frames(0, 60);
        let mut offset = 0;
        let mut produced = Vec::new();
        for _ in 0..30 {
            offset += device.playback.write(&input[offset..]).unwrap();
            device.callback(7);
        }
        produced.extend_from_slice(&device.delivered);
        let first = produced.iter().position(|byte| *byte != 0).unwrap();
        assert_eq!(&produced[first..first + input.len()], &input[..]);
    }

    #[test]
    fn writes_take_whole_frames_only() {
        let device = device(100, 200, 10, 0);
        assert_eq!(device.playback.write(&[1]).unwrap(), 0);
        assert_eq!(device.playback.write(&[1, 0, 2]).unwrap(), 2);
    }

    #[test]
    fn writable_bytes_is_the_room_up_to_the_target_and_never_past_the_capacity() {
        let device = device(100, 200, 10, 0);
        assert_eq!(device.playback.writable_bytes(), 200); // 100 frames
        device.playback.write(&frames(0, 60)).unwrap();
        assert_eq!(device.playback.writable_bytes(), 80);
        device.playback.write(&frames(0, 100)).unwrap(); // the target is not a limit
        assert_eq!(device.playback.writable_bytes(), 0);
        assert_eq!(
            device.playback.write(&frames(0, 100)).unwrap(),
            40 * 2,
            "up to the 200 frame capacity"
        );
        assert_eq!(
            device.playback.write(&frames(0, 1)).unwrap(),
            0,
            "the capacity is a limit"
        );
    }

    #[test]
    fn pause_sends_silence_and_keeps_the_ring() {
        let mut device = device(100, 200, 10, 0);
        device.playback.write(&frames(0, 40)).unwrap();
        device.callback(10);
        device.playback.pause();
        assert_eq!(device.playback.state(device.presented()), State::Paused);
        let before = device.delivered.len();
        device.callback(10);
        assert!(device.delivered[before..].iter().all(|byte| *byte == 0));
        device.playback.play();
        device.callback(10);
        // The music continues where it stopped: frame 11 of the input.
        assert_eq!(value_at(&device.delivered[before + 20..], 0), 11);
        assert_eq!(device.playback.underruns(), 0);
    }

    #[test]
    fn the_position_does_not_move_while_paused_and_continues_after() {
        let mut device = device(100, 200, 10, 6);
        device.playback.write(&frames(0, 60)).unwrap();
        for _ in 0..4 {
            device.callback(5);
        }
        let before = device.played();
        assert_eq!(before, 20 - 6, "the device holds 6 frames");
        device.playback.pause();
        for _ in 0..6 {
            device.callback(5);
        }
        // The 6 frames in the device's buffer at the pause still play out.
        assert_eq!(device.played(), 20);
        for _ in 0..6 {
            device.callback(5);
            assert_eq!(device.played(), 20, "silence is not counted");
        }
        device.playback.play();
        for _ in 0..4 {
            device.callback(5);
        }
        assert_eq!(device.played(), 20 + 20 - 6);
    }

    #[test]
    fn the_position_never_goes_backwards() {
        let mut device = device(200, 400, 10, 4);
        device.playback.write(&frames(0, 200)).unwrap();
        let mut last = 0;
        for round in 0..60 {
            if round == 20 {
                device.playback.pause();
            }
            if round == 40 {
                device.playback.play();
            }
            device.callback(5);
            let now = device.played();
            assert!(now >= last, "round {round}: {now} after {last}");
            last = now;
        }
    }

    #[test]
    fn an_underrun_is_counted_once_and_playback_rebuffers() {
        let mut device = device(100, 200, 10, 0);
        device.playback.write(&frames(0, 12)).unwrap();
        device.callback(8); // plays 8
        device.callback(8); // 4 left: 4 frames of music, then silence, an underrun
        assert_eq!(device.playback.underruns(), 1);
        assert_eq!(device.playback.state(device.presented()), State::Buffering);
        device.callback(8);
        device.callback(8);
        assert_eq!(
            device.playback.underruns(),
            1,
            "one run of silence is one underrun"
        );
        // Below the threshold the audio waits; at the threshold it resumes.
        device.playback.write(&frames(100, 5)).unwrap();
        device.callback(8);
        assert_eq!(device.playback.state(device.presented()), State::Buffering);
        device.playback.write(&frames(105, 5)).unwrap();
        device.callback(8);
        assert_eq!(device.playback.state(device.presented()), State::Playing);
    }

    #[test]
    fn flush_drops_the_buffer_and_restarts_the_position() {
        let mut device = device(200, 400, 10, 5);
        device.playback.write(&frames(0, 100)).unwrap();
        for _ in 0..4 {
            device.callback(10);
        }
        assert_eq!(device.played(), 35);
        device.playback.flush();
        assert_eq!(device.played(), 0, "reset at once");
        device.callback(10);
        // Nothing of the old audio is played after the flush; the new audio is.
        device.playback.write(&frames(1000, 30)).unwrap();
        let before = device.delivered.len();
        for _ in 0..5 {
            device.callback(10);
        }
        let after: Vec<u16> = (0..50)
            .map(|f| value_at(&device.delivered[before..], f))
            .collect();
        assert!(
            after.iter().all(|value| *value == 0 || *value > 1000),
            "{after:?}"
        );
        assert!(after.contains(&1001));
        // The old music still in the device (5 frames) plays and is not counted.
        assert!(device.played() <= 30);
    }

    #[test]
    fn a_flush_while_the_ring_is_full_gives_room_back_after_the_next_callback() {
        let mut device = device(100, 100, 10, 0);
        assert_eq!(device.playback.write(&frames(0, 200)).unwrap(), 200);
        device.playback.flush();
        assert_eq!(
            device.playback.writable_bytes(),
            0,
            "the callback may still be reading"
        );
        device.callback(5);
        assert_eq!(device.playback.writable_bytes(), 200);
    }

    #[test]
    fn drain_plays_a_tail_shorter_than_the_start_threshold_and_reports_drained() {
        let mut device = device(100, 200, 50, 4);
        device.playback.write(&frames(0, 7)).unwrap();
        device.callback(5);
        assert!(
            device.delivered.iter().all(|byte| *byte == 0),
            "7 frames is below the threshold"
        );
        device.playback.drain();
        assert_eq!(device.playback.state(device.presented()), State::Draining);
        assert!(
            device.playback.write(&frames(0, 1)).is_err(),
            "writes fail after drain"
        );
        assert_eq!(device.playback.writable_bytes(), 0);
        device.callback(5);
        assert!(
            !device.playback.is_drained(device.presented()),
            "the device still holds audio"
        );
        for _ in 0..3 {
            device.callback(5);
        }
        assert!(device.playback.is_drained(device.presented()));
        assert_eq!(device.playback.state(device.presented()), State::Drained);
        assert_eq!(device.played(), 7);
        assert_eq!(
            device.playback.underruns(),
            0,
            "the padded end is not an underrun"
        );
    }

    #[test]
    fn flush_after_drain_accepts_writes_again() {
        let device = device(100, 200, 10, 0);
        device.playback.write(&frames(0, 20)).unwrap();
        device.playback.drain();
        device.playback.flush();
        assert_eq!(device.playback.write(&frames(0, 5)).unwrap(), 10);
    }

    #[test]
    fn latency_is_the_ring_plus_what_the_device_holds() {
        let mut device = device(200, 400, 10, 6);
        device.playback.write(&frames(0, 100)).unwrap();
        device.callback(10);
        // 10 frames went to the device; it has presented 4 of them; 90 wait in the ring.
        assert_eq!(device.playback.latency_frames(device.presented()), 90 + 6);
        for _ in 0..3 {
            device.callback(10);
        }
        assert_eq!(device.playback.latency_frames(device.presented()), 60 + 6);
    }

    #[test]
    fn failing_the_output_stops_writes_and_the_callback() {
        let device = device(100, 200, 10, 0);
        device.playback.write(&frames(0, 20)).unwrap();
        device.playback.fail(-899);
        assert_eq!(device.playback.failure_code(), Some(-899));
        let error = device.playback.write(&frames(0, 1)).unwrap_err();
        assert_eq!(error.code, "device_lost");
        assert_eq!(device.playback.state(0), State::Failed);
        let mut block = [0xAAu8; 16];
        assert_eq!(device.playback.render(&mut block), Callback::Stop);
        assert!(block.iter().all(|byte| *byte == 0));
    }

    #[test]
    fn closing_stops_the_callback_with_silence() {
        let device = device(100, 200, 10, 0);
        device.playback.write(&frames(0, 20)).unwrap();
        device.playback.begin_close();
        assert_eq!(device.playback.state(0), State::Closed);
        let mut block = [0xAAu8; 16];
        assert_eq!(device.playback.render(&mut block), Callback::Stop);
        assert!(block.iter().all(|byte| *byte == 0));
        assert_eq!(
            device.playback.write(&frames(0, 1)).unwrap_err().code,
            "invalid_state"
        );
    }

    #[test]
    fn a_ring_larger_than_the_limit_is_refused() {
        let mut request = request(250, 500, 100);
        request.sample_rate = 768_000;
        request.channels = 8;
        request.format = SampleFormat::I32;
        request.buffer_capacity_ms = 10_000;
        let error = Playback::new(&request).err().expect("too big");
        assert_eq!(error.code, "invalid_argument");
    }

    #[test]
    fn a_callback_partial_frame_is_zeroed_and_not_counted() {
        let device = device(100, 200, 10, 0);
        device.playback.write(&frames(0, 20)).unwrap();
        let mut block = [0xAAu8; 7];
        device.playback.render(&mut block);
        assert_eq!(block[6], 0, "the trailing odd byte is silence");
    }

    #[test]
    fn two_threads_one_writing_one_rendering_deliver_the_stream_intact() {
        use std::sync::atomic::AtomicBool;
        use std::sync::Arc;
        let playback = Arc::new(Playback::new(&request(100, 200, 10)).unwrap());
        let done = Arc::new(AtomicBool::new(false));
        let writer = {
            let playback = playback.clone();
            let done = done.clone();
            std::thread::spawn(move || {
                let mut next = 0usize;
                while next < 20_000 {
                    let count = 37.min(20_000 - next);
                    let taken = playback.write(&frames(next as u16, count)).unwrap() / 2;
                    next += taken;
                    if taken == 0 {
                        std::thread::yield_now();
                    }
                }
                playback.drain();
                done.store(true, Ordering::Release);
            })
        };
        let mut heard = Vec::new();
        let mut block = [0u8; 2 * 61];
        let mut idle = 0;
        while idle < 2_000 {
            playback.render(&mut block);
            let music: Vec<u16> = block
                .chunks_exact(2)
                .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
                .filter(|value| *value != 0)
                .collect();
            idle = if music.is_empty() && done.load(Ordering::Acquire) {
                idle + 1
            } else {
                0
            };
            heard.extend(music);
            std::thread::yield_now();
        }
        writer.join().unwrap();
        assert_eq!(heard.len(), 20_000);
        assert!(heard
            .iter()
            .enumerate()
            .all(|(i, value)| *value as usize == (i + 1) % 65536));
    }
}
