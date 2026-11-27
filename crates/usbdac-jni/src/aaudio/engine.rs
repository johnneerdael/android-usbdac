//! An open AAudio output: the shared audio state, the device behind it, and the operations the
//! JNI layer exposes. The device is a trait so the whole life cycle (open, start, play, flush,
//! disconnect, close) is tested on the host against a simulated device; on Android it is the real
//! AAudio stream (`ffi.rs`).

use super::logic::{evaluate, grant_json, AaudioError, Capability, Grant, ERROR_DISCONNECTED};
use super::playback::{Playback, State};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// Something the platform told the output about the route, delivered to the host's listener.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    /// The route went away (device unplugged, routing changed): AAudio's `DISCONNECTED`.
    Disconnected(i32),
    /// Any other AAudio error callback.
    Failed(i32),
}

/// What the platform's callbacks reach: the audio state, and the way to tell the host.
pub struct Shared {
    pub playback: Playback,
    notify: Box<dyn Fn(Event) + Send + Sync>,
}

impl Shared {
    pub fn new(playback: Playback, notify: Box<dyn Fn(Event) + Send + Sync>) -> Self {
        Self { playback, notify }
    }

    /// AAudio's error callback: the stream is unusable. Marks the output failed, then tells the
    /// host (which may take a moment: it is a call into the JVM).
    pub fn on_error(&self, code: i32) {
        self.playback.fail(code);
        (self.notify)(if code == ERROR_DISCONNECTED {
            Event::Disconnected(code)
        } else {
            Event::Failed(code)
        });
    }
}

/// An AAudio stream, as far as the output needs to know.
pub trait Device: Send + Sync {
    /// What the platform granted.
    fn grant(&self) -> Grant;
    /// Starts the stream; the data callback begins to run.
    fn start(&self) -> Result<(), AaudioError>;
    /// Stops and closes the stream. When this returns no callback runs any more. Safe to repeat.
    fn close(&self) -> Result<(), AaudioError>;
    /// Frames the device has presented since the stream started (see
    /// `logic::presented_position`).
    fn presented_frames(&self) -> i64;
    /// AAudio's own count of device-side underruns.
    fn xruns(&self) -> u32;
}

pub struct Output {
    shared: Arc<Shared>,
    device: Box<dyn Device>,
    grant: Grant,
    capability: Capability,
    closed: AtomicBool,
}

impl Output {
    /// Wraps an opened device. Refuses a grant that could not be played as written: a stream
    /// that runs at another rate, in another format or with another channel count than the ring
    /// holds would play the bytes as noise or at the wrong speed. (A shared stream, or a
    /// platform conversion before the hardware, is playable and is only reported.)
    pub fn new(
        shared: Arc<Shared>,
        device: Box<dyn Device>,
        capability: Capability,
    ) -> Result<Self, AaudioError> {
        let grant = device.grant();
        let verdict = evaluate(&capability, &grant);
        if verdict.breaks_stream().is_some() {
            let _ = device.close();
            return Err(AaudioError::unsupported(format!(
                "the platform did not open the stream as requested: {}",
                verdict.reason
            )));
        }
        Ok(Self {
            shared,
            device,
            grant,
            capability,
            closed: AtomicBool::new(false),
        })
    }

    #[cfg(test)]
    pub fn grant(&self) -> &Grant {
        &self.grant
    }

    /// Invalidate a constrained route before notifying observers. The device is closed by its owner.
    pub fn invalidate_route(&self) -> Result<(), AaudioError> {
        self.check_open()?;
        self.shared.on_error(ERROR_DISCONNECTED);
        Ok(())
    }

    pub fn grant_json(&self) -> String {
        grant_json(&self.capability, &self.grant).to_string()
    }

    fn playback(&self) -> &Playback {
        &self.shared.playback
    }

    fn check_open(&self) -> Result<(), AaudioError> {
        if self.closed.load(Ordering::Acquire) {
            return Err(AaudioError::invalid_state("the output is closed"));
        }
        Ok(())
    }

    pub fn start(&self) -> Result<(), AaudioError> {
        self.check_open()?;
        if self.playback().has_failed() {
            return Err(AaudioError::device_lost(
                "the constrained audio route was invalidated",
            ));
        }
        self.device.start()
    }

    pub fn write(&self, bytes: &[u8]) -> Result<usize, AaudioError> {
        self.check_open()?;
        self.playback().write(bytes)
    }

    pub fn writable_bytes(&self) -> usize {
        if self.closed.load(Ordering::Acquire) {
            return 0;
        }
        self.playback().writable_bytes()
    }

    pub fn play(&self) -> Result<(), AaudioError> {
        self.check_open()?;
        self.playback().play();
        Ok(())
    }

    pub fn pause(&self) -> Result<(), AaudioError> {
        self.check_open()?;
        self.playback().pause();
        Ok(())
    }

    pub fn flush(&self) -> Result<(), AaudioError> {
        self.check_open()?;
        self.playback().flush();
        Ok(())
    }

    pub fn drain(&self) -> Result<(), AaudioError> {
        self.check_open()?;
        self.playback().drain();
        Ok(())
    }

    pub fn is_drained(&self) -> bool {
        !self.closed.load(Ordering::Acquire)
            && self.playback().is_drained(self.device.presented_frames())
    }

    pub fn played_frames(&self) -> u64 {
        if self.closed.load(Ordering::Acquire) {
            return 0;
        }
        self.playback()
            .played_frames(self.device.presented_frames())
    }

    pub fn latency_frames(&self) -> u64 {
        if self.closed.load(Ordering::Acquire) {
            return 0;
        }
        self.playback()
            .latency_frames(self.device.presented_frames())
    }

    pub fn state(&self) -> State {
        if self.closed.load(Ordering::Acquire) {
            return State::Closed;
        }
        self.playback().state(self.device.presented_frames())
    }

    pub fn xruns(&self) -> u32 {
        if self.closed.load(Ordering::Acquire) {
            return 0;
        }
        self.device.xruns()
    }

    pub fn underruns(&self) -> u64 {
        self.playback().underruns()
    }

    /// Stops the callbacks and releases the stream. Safe to repeat.
    pub fn close(&self) -> Result<(), AaudioError> {
        if self.closed.swap(true, Ordering::AcqRel) {
            return Ok(());
        }
        self.playback().begin_close();
        self.device.close()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aaudio::logic::{
        Hardware, OpenRequest, Performance, SampleFormat, FORMAT_PCM_I16, FORMAT_PCM_I32,
        PERFORMANCE_LOW_LATENCY, SHARING_EXCLUSIVE, SHARING_SHARED,
    };
    use crate::aaudio::playback::Callback;
    use parking_lot::Mutex;
    use std::sync::atomic::{AtomicI64, AtomicU32};

    fn request() -> OpenRequest {
        OpenRequest {
            sample_rate: 1_000,
            channels: 1,
            format: SampleFormat::I16,
            device_id: 0,
            performance: Performance::LowLatency,
            buffer_target_ms: 100,
            buffer_capacity_ms: 200,
            start_threshold_ms: 10,
            device_buffer_bursts: None,
        }
    }

    fn grant(sharing: i32, rate: i32, format: i32) -> Grant {
        Grant {
            sharing,
            sample_rate: rate,
            channels: 1,
            format,
            frames_per_burst: 8,
            buffer_capacity_frames: 64,
            buffer_size_frames: 16,
            device_id: 3,
            device_ids: None,
            performance_mode: PERFORMANCE_LOW_LATENCY,
            mmap: Some(true),
            hardware: None,
        }
    }

    struct FakeDevice {
        grant: Grant,
        started: AtomicBool,
        closes: AtomicU32,
        presented: AtomicI64,
        xruns: AtomicU32,
    }

    impl FakeDevice {
        fn new(grant: Grant) -> Arc<Self> {
            Arc::new(Self {
                grant,
                started: AtomicBool::new(false),
                closes: AtomicU32::new(0),
                presented: AtomicI64::new(0),
                xruns: AtomicU32::new(0),
            })
        }
    }

    impl Device for Arc<FakeDevice> {
        fn grant(&self) -> Grant {
            self.grant.clone()
        }
        fn start(&self) -> Result<(), AaudioError> {
            self.started.store(true, Ordering::SeqCst);
            Ok(())
        }
        fn close(&self) -> Result<(), AaudioError> {
            self.closes.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        fn presented_frames(&self) -> i64 {
            self.presented.load(Ordering::SeqCst)
        }
        fn xruns(&self) -> u32 {
            self.xruns.load(Ordering::SeqCst)
        }
    }

    type Opened = (
        Result<Output, AaudioError>,
        Arc<FakeDevice>,
        Arc<Mutex<Vec<Event>>>,
    );
    fn open(grant: Grant) -> Opened {
        let events = Arc::new(Mutex::new(Vec::new()));
        let sink = events.clone();
        let shared = Arc::new(Shared::new(
            Playback::new(&request()).unwrap(),
            Box::new(move |event| sink.lock().push(event)),
        ));
        let device = FakeDevice::new(grant);
        let output = Output::new(shared, Box::new(device.clone()), request().capability());
        (output, device, events)
    }

    #[test]
    fn an_exclusive_grant_as_requested_opens_and_reports_the_grant() {
        let (output, device, _) = open(grant(SHARING_EXCLUSIVE, 1_000, FORMAT_PCM_I16));
        let output = output.unwrap();
        output.start().unwrap();
        assert!(device.started.load(Ordering::SeqCst));
        let json: serde_json::Value = serde_json::from_str(&output.grant_json()).unwrap();
        assert_eq!(json["sharing"], "exclusive");
        assert_eq!(json["verdict"], "exclusive");
        assert_eq!(json["as_requested"], true);
        assert_eq!(json["device_id"], 3);
        assert_eq!(json["frames_per_burst"], 8);
    }

    #[test]
    fn a_shared_grant_opens_and_says_it_is_shared() {
        let (output, _, _) = open(grant(SHARING_SHARED, 1_000, FORMAT_PCM_I16));
        let output = output.unwrap();
        assert!(!output.grant().is_exclusive());
        let json: serde_json::Value = serde_json::from_str(&output.grant_json()).unwrap();
        assert_eq!(json["verdict"], "shared");
        assert!(json["reason"].as_str().unwrap().contains("shared mode"));
    }

    #[test]
    fn a_stream_that_would_play_the_bytes_wrongly_is_refused_and_closed() {
        for bad in [
            grant(SHARING_EXCLUSIVE, 1_200, FORMAT_PCM_I16),
            grant(SHARING_EXCLUSIVE, 1_000, FORMAT_PCM_I32),
        ] {
            let (output, device, _) = open(bad);
            let error = output.err().expect("refused");
            assert_eq!(error.code, "unsupported");
            assert!(error
                .message
                .contains("did not open the stream as requested"));
            assert_eq!(
                device.closes.load(Ordering::SeqCst),
                1,
                "the stream is released"
            );
        }
    }

    #[test]
    fn a_hardware_conversion_is_playable_and_reported() {
        let mut converted = grant(SHARING_EXCLUSIVE, 1_000, FORMAT_PCM_I16);
        converted.hardware = Some(Hardware {
            sample_rate: 1_000,
            channels: 1,
            format: FORMAT_PCM_I32,
        });
        let (output, _, _) = open(converted);
        let json: serde_json::Value = serde_json::from_str(&output.unwrap().grant_json()).unwrap();
        assert_eq!(json["verdict"], "hardware_format");
        assert_eq!(json["as_requested"], false);
    }

    #[test]
    fn the_transport_calls_reach_the_playback_and_the_position_follows_the_device() {
        let (output, device, _) = open(grant(SHARING_EXCLUSIVE, 1_000, FORMAT_PCM_I16));
        let output = output.unwrap();
        output.start().unwrap();
        assert_eq!(output.state(), State::Buffering);
        let audio: Vec<u8> = (1..=40u16).flat_map(u16::to_le_bytes).collect();
        assert_eq!(output.write(&audio).unwrap(), 80);
        let mut block = [0u8; 40];
        assert_eq!(
            output.shared.playback.render(&mut block),
            Callback::Continue
        );
        assert_eq!(output.state(), State::Playing);
        device.presented.store(12, Ordering::SeqCst);
        assert_eq!(output.played_frames(), 12);
        assert_eq!(
            output.latency_frames(),
            20 + 8,
            "ring plus what the device holds"
        );
        output.pause().unwrap();
        assert_eq!(output.state(), State::Paused);
        output.play().unwrap();
        output.flush().unwrap();
        assert_eq!(output.played_frames(), 0);
        output.drain().unwrap();
        assert_eq!(output.state(), State::Draining);
        assert!(output.write(&audio).is_err());
    }

    #[test]
    fn xruns_come_from_the_device_and_underruns_from_the_ring() {
        let (output, device, _) = open(grant(SHARING_EXCLUSIVE, 1_000, FORMAT_PCM_I16));
        let output = output.unwrap();
        device.xruns.store(3, Ordering::SeqCst);
        assert_eq!(output.xruns(), 3);
        assert_eq!(output.underruns(), 0);
    }

    #[test]
    fn a_disconnect_fails_the_output_and_tells_the_listener() {
        let (output, _, events) = open(grant(SHARING_EXCLUSIVE, 1_000, FORMAT_PCM_I16));
        let output = output.unwrap();
        output.shared.on_error(ERROR_DISCONNECTED);
        assert_eq!(
            *events.lock(),
            vec![Event::Disconnected(ERROR_DISCONNECTED)]
        );
        assert_eq!(output.state(), State::Failed);
        assert_eq!(output.write(&[1, 0]).unwrap_err().code, "device_lost");
        assert_eq!(output.writable_bytes(), 0);
        output.shared.on_error(-896);
        assert_eq!(events.lock().last(), Some(&Event::Failed(-896)));
        output.close().unwrap();
    }

    #[test]
    fn explicit_route_invalidation_fails_before_later_writes_and_survives_flush() {
        let (output, _, events) = open(grant(SHARING_EXCLUSIVE, 1_000, FORMAT_PCM_I16));
        let output = output.unwrap();
        output.invalidate_route().unwrap();
        assert_eq!(output.start().unwrap_err().code, "device_lost");
        assert_eq!(
            *events.lock(),
            vec![Event::Disconnected(ERROR_DISCONNECTED)]
        );
        assert_eq!(output.writable_bytes(), 0);
        assert_eq!(output.write(&[0, 0]).unwrap_err().code, "device_lost");
        output.flush().unwrap();
        assert_eq!(output.write(&[0, 0]).unwrap_err().code, "device_lost");
    }

    #[test]
    fn close_releases_the_device_once_and_the_output_then_refuses_calls() {
        let (output, device, _) = open(grant(SHARING_EXCLUSIVE, 1_000, FORMAT_PCM_I16));
        let output = output.unwrap();
        output.close().unwrap();
        output.close().unwrap();
        assert_eq!(device.closes.load(Ordering::SeqCst), 1);
        assert_eq!(output.state(), State::Closed);
        assert_eq!(output.write(&[1, 0]).unwrap_err().code, "invalid_state");
        assert_eq!(output.start().unwrap_err().code, "invalid_state");
        assert_eq!(output.played_frames(), 0);
        assert_eq!(output.writable_bytes(), 0);
        let mut block = [0xAAu8; 8];
        assert_eq!(output.shared.playback.render(&mut block), Callback::Stop);
    }
}
