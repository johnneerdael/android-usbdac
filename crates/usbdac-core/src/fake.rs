//! Deterministic application tests without USB, permissions, threads or a DAC.
//!
//! This is a playback boundary substitute, not a USB protocol emulator. It
//! accepts explicit PCM profiles only and never claims hardware verification.
//! Drive completion with [`FakeStream::advance`]; time never advances itself.
use crate::{
    Dac, DacEvent, DacListener, DacStream, DeviceInfo, Encoding, Error, LinkSpeed, Pacing, Result,
    StreamFormat, StreamRequest, StreamState, SyncKind,
};
use parking_lot::Mutex;
use std::sync::{Arc, Weak};

/// The playback subset an application can use with a real or fake DAC.
pub trait PlaybackDevice {
    type Stream: PlaybackStream;
    fn negotiate(&self, request: &StreamRequest) -> Result<StreamFormat>;
    fn open_stream(
        &self,
        format: &StreamFormat,
        listener: Arc<dyn DacListener>,
    ) -> Result<Self::Stream>;
}
/// Push playback shared by [`DacStream`] and [`FakeStream`].
pub trait PlaybackStream {
    fn write(&self, data: &[u8]) -> Result<usize>;
    fn writable_bytes(&self) -> usize;
    fn played_frames(&self) -> u64;
    fn latency_frames(&self) -> u64;
    fn play(&self) -> Result<()>;
    fn pause(&self) -> Result<()>;
    fn flush(&self) -> Result<()>;
    fn drain(&self) -> Result<()>;
}
impl PlaybackDevice for Dac {
    type Stream = DacStream;
    fn negotiate(&self, request: &StreamRequest) -> Result<StreamFormat> {
        Dac::negotiate(self, request)
    }
    fn open_stream(
        &self,
        format: &StreamFormat,
        listener: Arc<dyn DacListener>,
    ) -> Result<DacStream> {
        Dac::open_stream(self, format, listener)
    }
}
impl PlaybackStream for DacStream {
    fn write(&self, data: &[u8]) -> Result<usize> {
        DacStream::write(self, data)
    }
    fn writable_bytes(&self) -> usize {
        DacStream::writable_bytes(self)
    }
    fn played_frames(&self) -> u64 {
        DacStream::played_frames(self)
    }
    fn latency_frames(&self) -> u64 {
        DacStream::latency_frames(self)
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
}

struct DeviceState {
    detached: bool,
    closed: bool,
    stream: Weak<Mutex<StreamData>>,
    negotiated: Option<StreamFormat>,
}
/// Independent fake devices do not occupy the process's real USB session.
/// Profiles must be nonzero PCM at 16, 24 or 32 bits; negotiation is exact.
pub struct FakeDac {
    info: DeviceInfo,
    profiles: Vec<StreamRequest>,
    capacity_frames: usize,
    state: Arc<Mutex<DeviceState>>,
}
impl FakeDac {
    pub fn new(
        info: DeviceInfo,
        profiles: Vec<StreamRequest>,
        capacity_frames: usize,
    ) -> Result<Self> {
        if capacity_frames == 0
            || profiles.is_empty()
            || profiles.iter().any(|r| {
                !matches!(r.encoding, Encoding::Pcm | Encoding::PcmU8 | Encoding::PcmFloat32)
                    || r.sample_rate == 0
                    || r.channels == 0
                    || !match r.encoding { Encoding::PcmU8 => r.bit_depth == 8, Encoding::PcmFloat32 => r.bit_depth == 32, _ => matches!(r.bit_depth, 16 | 24 | 32) }
            })
        {
            return Err(Error::InvalidArgument(
                "fake requires PCM profiles and nonzero capacity".into(),
            ));
        }
        Ok(Self {
            info,
            profiles,
            capacity_frames,
            state: Arc::new(Mutex::new(DeviceState {
                detached: false,
                closed: false,
                stream: Weak::new(),
                negotiated: None,
            })),
        })
    }
    pub fn info(&self) -> &DeviceInfo {
        &self.info
    }
    /// Inject unplug. Existing streams fail with DeviceLost; a replug is a new
    /// FakeDac, so old handles cannot operate on the replacement attachment.
    pub fn detach(&self) {
        let stream = {
            let mut state = self.state.lock();
            if state.detached {
                return;
            }
            state.detached = true;
            state.stream.upgrade()
        };
        if let Some(stream) = stream {
            let listener = {
                let mut stream = stream.lock();
                stream.state = StreamState::Failed;
                stream.listener.clone()
            };
            listener.on_event(DacEvent::DeviceLost);
            listener.on_event(DacEvent::StateChanged(StreamState::Failed));
        }
    }
    pub fn close(self) {
        self.state.lock().closed = true;
    }
}
fn check(state: &DeviceState) -> Result<()> {
    if state.detached {
        Err(Error::DeviceLost)
    } else if state.closed {
        Err(Error::InvalidState("fake DAC is closed".into()))
    } else {
        Ok(())
    }
}
impl PlaybackDevice for FakeDac {
    type Stream = FakeStream;
    fn negotiate(&self, request: &StreamRequest) -> Result<StreamFormat> {
        let mut state = self.state.lock();
        check(&state)?;
        if state.stream.upgrade().is_some() {
            return Err(Error::InvalidState("a stream is already open".into()));
        }
        if !self.profiles.iter().any(|r| {
            r.sample_rate == request.sample_rate
                && r.channels == request.channels
                && r.bit_depth == request.bit_depth
                && r.encoding == request.encoding
        }) {
            return Err(Error::Unsupported {
                reason: "fake PCM profile".into(),
                offered: Vec::new(),
            });
        }
        let format = StreamFormat {
            request: *request,
            encoding: request.encoding,
            sample_rate: request.sample_rate,
            channels: request.channels,
            channel_map: crate::capture::ChannelMap::uac2(request.channels, 0)
                .map_err(|e| Error::InvalidArgument(format!("{e:?}")))?,
            valid_bits: request.bit_depth,
            subslot_bytes: request.bit_depth / 8,
            interface_number: 1,
            alt_setting: 1,
            endpoint_address: 1,
            sync_type: SyncKind::Synchronous,
            explicit_feedback: false,
            link_speed: LinkSpeed::Unknown,
            service_interval_us: 1000,
            pacing: Pacing::Synchronous,
            clock: None,
            native_dsd_signature: None,
        };
        state.negotiated = Some(format.clone());
        Ok(format)
    }
    fn open_stream(
        &self,
        format: &StreamFormat,
        listener: Arc<dyn DacListener>,
    ) -> Result<FakeStream> {
        let mut state = self.state.lock();
        check(&state)?;
        if state.stream.upgrade().is_some() {
            return Err(Error::Busy);
        }
        if state.negotiated.as_ref() != Some(format) {
            return Err(Error::InvalidArgument(
                "format was not negotiated on this fake".into(),
            ));
        }
        let stream = Arc::new(Mutex::new(StreamData {
            queued: 0,
            played: 0,
            lost: 0,
            capacity: self.capacity_frames,
            bytes_per_frame: format.bytes_per_frame(),
            state: StreamState::Buffering,
            draining: false,
            listener: listener.clone(),
        }));
        state.stream = Arc::downgrade(&stream);
        drop(state);
        listener.on_event(DacEvent::StateChanged(StreamState::Buffering));
        Ok(FakeStream {
            device: self.state.clone(),
            data: stream,
        })
    }
}
impl Drop for FakeDac {
    fn drop(&mut self) {
        self.state.lock().closed = true;
    }
}
struct StreamData {
    queued: u64,
    played: u64,
    lost: u64,
    capacity: usize,
    bytes_per_frame: usize,
    state: StreamState,
    draining: bool,
    listener: Arc<dyn DacListener>,
}
/// Fake callbacks execute inline after releasing locks, unlike real DAC
/// callbacks. USB timing, feedback, volume safety and bit-perfect verification
/// require the real implementation; this fake tests application decisions.
pub struct FakeStream {
    device: Arc<Mutex<DeviceState>>,
    data: Arc<Mutex<StreamData>>,
}
impl FakeStream {
    /// Complete up to `frames` queued frames. Paused audio is not consumed.
    pub fn advance(&self, frames: u64) -> Result<u64> {
        check(&self.device.lock())?;
        let (taken, event, listener) = {
            let mut data = self.data.lock();
            if data.state == StreamState::Paused {
                return Ok(0);
            }
            let taken = frames.min(data.queued);
            data.queued -= taken;
            data.played += taken;
            let next = if data.draining && data.queued == 0 {
                StreamState::Drained
            } else if taken > 0 {
                StreamState::Playing
            } else {
                StreamState::Buffering
            };
            let event = (next != data.state).then_some(DacEvent::StateChanged(next));
            data.state = next;
            (taken, event, data.listener.clone())
        };
        if let Some(event) = event {
            listener.on_event(event);
        }
        Ok(taken)
    }
    /// Lose queued audio without incrementing played frames.
    pub fn lose_frames(&self, frames: u64) -> Result<u64> {
        check(&self.device.lock())?;
        let (lost, listener) = {
            let mut data = self.data.lock();
            let lost = frames.min(data.queued);
            data.queued -= lost;
            data.lost += lost;
            (lost, data.listener.clone())
        };
        if lost > 0 {
            listener.on_event(DacEvent::LostFrames { frames: lost });
        }
        Ok(lost)
    }
    pub fn lost_frames(&self) -> u64 {
        self.data.lock().lost
    }
    pub fn state(&self) -> StreamState {
        self.data.lock().state
    }
}
impl PlaybackStream for FakeStream {
    fn write(&self, bytes: &[u8]) -> Result<usize> {
        check(&self.device.lock())?;
        let mut data = self.data.lock();
        if data.draining {
            return Err(Error::InvalidState("fake stream is draining".into()));
        }
        if bytes.len() % data.bytes_per_frame != 0 {
            return Err(Error::InvalidArgument(
                "write must contain whole frames".into(),
            ));
        }
        let frames = (bytes.len() / data.bytes_per_frame).min(data.capacity - data.queued as usize);
        data.queued += frames as u64;
        Ok(frames * data.bytes_per_frame)
    }
    fn writable_bytes(&self) -> usize {
        if check(&self.device.lock()).is_err() {
            return 0;
        }
        let data = self.data.lock();
        if data.draining {
            0
        } else {
            (data.capacity - data.queued as usize) * data.bytes_per_frame
        }
    }
    fn played_frames(&self) -> u64 {
        self.data.lock().played
    }
    fn latency_frames(&self) -> u64 {
        self.data.lock().queued
    }
    fn play(&self) -> Result<()> {
        check(&self.device.lock())?;
        self.data.lock().state = StreamState::Buffering;
        Ok(())
    }
    fn pause(&self) -> Result<()> {
        check(&self.device.lock())?;
        self.data.lock().state = StreamState::Paused;
        Ok(())
    }
    fn flush(&self) -> Result<()> {
        check(&self.device.lock())?;
        let mut data = self.data.lock();
        data.queued = 0;
        data.played = 0;
        data.draining = false;
        data.state = StreamState::Buffering;
        Ok(())
    }
    fn drain(&self) -> Result<()> {
        check(&self.device.lock())?;
        let mut data = self.data.lock();
        data.draining = true;
        data.state = if data.queued == 0 {
            StreamState::Drained
        } else {
            StreamState::Draining
        };
        Ok(())
    }
}
impl Drop for FakeStream {
    fn drop(&mut self) {
        let listener = {
            let mut data = self.data.lock();
            data.state = StreamState::Closed;
            data.listener.clone()
        };
        listener.on_event(DacEvent::StateChanged(StreamState::Closed));
    }
}
