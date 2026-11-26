//! The NDK's AAudio C API, reached through `dlopen("libaaudio.so")` and `dlsym`.
//!
//! The functions are resolved at run time instead of being linked, for two reasons. The build
//! then does not depend on the platform level it targets (`libaaudio.so` is only in the NDK
//! sysroot from API 26 on), and `System.loadLibrary("usbdac")` succeeds on every device the
//! library supports whatever the AAudio release: the symbols added after API 26 (the usage and
//! content-type attributes in 28, the MMAP query in 28, the hardware format in 34) are optional,
//! and a missing one only means the information is not reported.
//!
//! Only this file and the callbacks below know the C ABI.

use super::engine::{Device, Shared};
use super::logic::{
    error_for_result, presented_position, AaudioError, Grant, Hardware, OpenRequest, Timestamp,
    AAUDIO_OK, SHARING_EXCLUSIVE, STATE_DISCONNECTED, STATE_STARTED, STATE_STOPPED,
};
use super::playback::Callback;
use parking_lot::Mutex;
use std::ffi::c_void;
use std::mem::{size_of, transmute_copy};
use std::sync::{Arc, OnceLock};

type RawBuilder = c_void;
type RawStream = c_void;
type DataCallback = unsafe extern "C" fn(*mut RawStream, *mut c_void, *mut c_void, i32) -> i32;
type ErrorCallback = unsafe extern "C" fn(*mut RawStream, *mut c_void, i32);

const AAUDIO_DIRECTION_OUTPUT: i32 = 0;
const AAUDIO_USAGE_MEDIA: i32 = 1;
const AAUDIO_CONTENT_TYPE_MUSIC: i32 = 2;
const CLOCK_MONOTONIC: i32 = 1;

macro_rules! api {
    (
        required { $($name:ident : $symbol:literal => $ty:ty,)* }
        optional { $($opt:ident : $opt_symbol:literal => $opt_ty:ty,)* }
    ) => {
        /// The resolved AAudio functions.
        pub struct Api {
            $(pub $name: $ty,)*
            $(pub $opt: Option<$opt_ty>,)*
        }

        impl Api {
            /// # Safety
            /// `library` is a handle returned by `dlopen`.
            unsafe fn load(library: *mut c_void) -> Result<Self, String> {
                Ok(Self {
                    $($name: resolve::<$ty>(library, $symbol)
                        .ok_or_else(|| format!("libaaudio.so has no {}", $symbol))?,)*
                    $($opt: resolve::<$opt_ty>(library, $opt_symbol),)*
                })
            }
        }
    };
}

api! {
    required {
        create_builder: "AAudio_createStreamBuilder" => unsafe extern "C" fn(*mut *mut RawBuilder) -> i32,
        builder_delete: "AAudioStreamBuilder_delete" => unsafe extern "C" fn(*mut RawBuilder) -> i32,
        builder_set_device_id: "AAudioStreamBuilder_setDeviceId" => unsafe extern "C" fn(*mut RawBuilder, i32),
        builder_set_sample_rate: "AAudioStreamBuilder_setSampleRate" => unsafe extern "C" fn(*mut RawBuilder, i32),
        builder_set_channel_count: "AAudioStreamBuilder_setChannelCount" => unsafe extern "C" fn(*mut RawBuilder, i32),
        builder_set_format: "AAudioStreamBuilder_setFormat" => unsafe extern "C" fn(*mut RawBuilder, i32),
        builder_set_sharing_mode: "AAudioStreamBuilder_setSharingMode" => unsafe extern "C" fn(*mut RawBuilder, i32),
        builder_set_direction: "AAudioStreamBuilder_setDirection" => unsafe extern "C" fn(*mut RawBuilder, i32),
        builder_set_performance_mode: "AAudioStreamBuilder_setPerformanceMode" => unsafe extern "C" fn(*mut RawBuilder, i32),
        builder_set_data_callback: "AAudioStreamBuilder_setDataCallback" => unsafe extern "C" fn(*mut RawBuilder, Option<DataCallback>, *mut c_void),
        builder_set_error_callback: "AAudioStreamBuilder_setErrorCallback" => unsafe extern "C" fn(*mut RawBuilder, Option<ErrorCallback>, *mut c_void),
        builder_open_stream: "AAudioStreamBuilder_openStream" => unsafe extern "C" fn(*mut RawBuilder, *mut *mut RawStream) -> i32,
        stream_close: "AAudioStream_close" => unsafe extern "C" fn(*mut RawStream) -> i32,
        stream_request_start: "AAudioStream_requestStart" => unsafe extern "C" fn(*mut RawStream) -> i32,
        stream_request_stop: "AAudioStream_requestStop" => unsafe extern "C" fn(*mut RawStream) -> i32,
        stream_get_state: "AAudioStream_getState" => unsafe extern "C" fn(*mut RawStream) -> i32,
        stream_wait_for_state_change: "AAudioStream_waitForStateChange" => unsafe extern "C" fn(*mut RawStream, i32, *mut i32, i64) -> i32,
        stream_get_sample_rate: "AAudioStream_getSampleRate" => unsafe extern "C" fn(*mut RawStream) -> i32,
        stream_get_channel_count: "AAudioStream_getChannelCount" => unsafe extern "C" fn(*mut RawStream) -> i32,
        stream_get_format: "AAudioStream_getFormat" => unsafe extern "C" fn(*mut RawStream) -> i32,
        stream_get_sharing_mode: "AAudioStream_getSharingMode" => unsafe extern "C" fn(*mut RawStream) -> i32,
        stream_get_performance_mode: "AAudioStream_getPerformanceMode" => unsafe extern "C" fn(*mut RawStream) -> i32,
        stream_get_frames_per_burst: "AAudioStream_getFramesPerBurst" => unsafe extern "C" fn(*mut RawStream) -> i32,
        stream_get_buffer_capacity: "AAudioStream_getBufferCapacityInFrames" => unsafe extern "C" fn(*mut RawStream) -> i32,
        stream_get_buffer_size: "AAudioStream_getBufferSizeInFrames" => unsafe extern "C" fn(*mut RawStream) -> i32,
        stream_set_buffer_size: "AAudioStream_setBufferSizeInFrames" => unsafe extern "C" fn(*mut RawStream, i32) -> i32,
        stream_get_device_id: "AAudioStream_getDeviceId" => unsafe extern "C" fn(*mut RawStream) -> i32,
        stream_get_xrun_count: "AAudioStream_getXRunCount" => unsafe extern "C" fn(*mut RawStream) -> i32,
        stream_get_frames_read: "AAudioStream_getFramesRead" => unsafe extern "C" fn(*mut RawStream) -> i64,
        stream_get_timestamp: "AAudioStream_getTimestamp" => unsafe extern "C" fn(*mut RawStream, i32, *mut i64, *mut i64) -> i32,
    }
    optional {
        builder_set_usage: "AAudioStreamBuilder_setUsage" => unsafe extern "C" fn(*mut RawBuilder, i32),
        builder_set_content_type: "AAudioStreamBuilder_setContentType" => unsafe extern "C" fn(*mut RawBuilder, i32),
        stream_is_mmap_used: "AAudioStream_isMMapUsed" => unsafe extern "C" fn(*mut RawStream) -> bool,
        stream_get_hardware_sample_rate: "AAudioStream_getHardwareSampleRate" => unsafe extern "C" fn(*mut RawStream) -> i32,
        stream_get_hardware_channel_count: "AAudioStream_getHardwareChannelCount" => unsafe extern "C" fn(*mut RawStream) -> i32,
        stream_get_hardware_format: "AAudioStream_getHardwareFormat" => unsafe extern "C" fn(*mut RawStream) -> i32,
    }
}

/// Looks `name` up in `library` as a function of type `F` (a function pointer).
///
/// # Safety
/// `F` must be the function pointer type that matches the C signature of the symbol.
unsafe fn resolve<F: Copy>(library: *mut c_void, name: &str) -> Option<F> {
    assert_eq!(size_of::<F>(), size_of::<*mut c_void>());
    let name = std::ffi::CString::new(name).ok()?;
    let address = libc::dlsym(library, name.as_ptr());
    if address.is_null() {
        None
    } else {
        Some(transmute_copy::<*mut c_void, F>(&address))
    }
}

static API: OnceLock<Result<Api, String>> = OnceLock::new();

/// The AAudio functions, loaded on first use.
pub fn api() -> Result<&'static Api, AaudioError> {
    API.get_or_init(|| {
        // SAFETY: plain dlopen/dlsym of a public NDK library; the signatures in `api!` are those
        // of AAudio.h.
        unsafe {
            let library = libc::dlopen(c"libaaudio.so".as_ptr(), libc::RTLD_NOW);
            if library.is_null() {
                return Err("libaaudio.so could not be loaded".to_string());
            }
            Api::load(library)
        }
    })
    .as_ref()
    .map_err(|reason| AaudioError::unsupported(format!("AAudio is not available: {reason}")))
}

fn monotonic_ns() -> i64 {
    let mut now = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `now` is a valid timespec.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut now) };
    i64::from(now.tv_sec) * 1_000_000_000 + i64::from(now.tv_nsec)
}

// ---- the callbacks ---------------------------------------------------------------------------

/// AAudio's data callback, on its real-time thread. `user` is the `Shared` the stream was opened
/// with, kept alive until the stream is closed.
unsafe extern "C" fn data_callback(
    _stream: *mut RawStream,
    user: *mut c_void,
    audio: *mut c_void,
    frames: i32,
) -> i32 {
    let shared = &*(user as *const Shared);
    let bytes = usize::try_from(frames).unwrap_or(0) * shared.playback.frame_bytes();
    let out = std::slice::from_raw_parts_mut(audio as *mut u8, bytes);
    // A panic must not unwind into the platform: answer with silence and stop.
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| shared.playback.render(out))) {
        Ok(result) => result.aaudio_value(),
        Err(_) => {
            out.fill(0);
            Callback::Stop.aaudio_value()
        }
    }
}

/// AAudio's error callback, on a thread of its own: the route is gone or failed.
unsafe extern "C" fn error_callback(_stream: *mut RawStream, user: *mut c_void, error: i32) {
    let shared = &*(user as *const Shared);
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| shared.on_error(error)));
}

// ---- the stream -------------------------------------------------------------------------------

struct StreamPtr(*mut RawStream);

// SAFETY: AAudio streams are thread-safe objects (their functions may be called from any thread
// but for read and write, which this output does not use).
unsafe impl Send for StreamPtr {}

pub struct AndroidDevice {
    api: &'static Api,
    stream: Mutex<Option<StreamPtr>>,
    /// The `Arc<Shared>` reference the callbacks use; released once the stream is closed.
    user: *const Shared,
    rate: u32,
    grant: Grant,
}

// SAFETY: `user` is only turned back into an `Arc` in `close`, and the stream behind the mutex is
// thread-safe (see `StreamPtr`).
unsafe impl Send for AndroidDevice {}
unsafe impl Sync for AndroidDevice {}

impl AndroidDevice {
    /// Opens a stream for `request` whose callbacks feed from and report to `shared`.
    pub fn open(request: &OpenRequest, shared: &Arc<Shared>) -> Result<Self, AaudioError> {
        let api = api()?;
        let user = Arc::into_raw(shared.clone());
        match Self::open_stream(api, request, user) {
            Ok(device) => Ok(device),
            Err(error) => {
                // SAFETY: `user` came from `Arc::into_raw` above and no stream holds it.
                drop(unsafe { Arc::from_raw(user) });
                Err(error)
            }
        }
    }

    fn open_stream(
        api: &'static Api,
        request: &OpenRequest,
        user: *const Shared,
    ) -> Result<Self, AaudioError> {
        // SAFETY: every call below follows AAudio.h; the builder is deleted on every path and the
        // stream pointer is only used after a successful open.
        unsafe {
            let mut builder: *mut RawBuilder = std::ptr::null_mut();
            let result = (api.create_builder)(&mut builder);
            if result != AAUDIO_OK || builder.is_null() {
                return Err(error_for_result("creating the stream builder", result));
            }
            if request.device_id != 0 {
                (api.builder_set_device_id)(builder, request.device_id);
            }
            (api.builder_set_direction)(builder, AAUDIO_DIRECTION_OUTPUT);
            (api.builder_set_sharing_mode)(builder, SHARING_EXCLUSIVE);
            (api.builder_set_sample_rate)(builder, request.sample_rate as i32);
            (api.builder_set_channel_count)(builder, request.channels as i32);
            (api.builder_set_format)(builder, request.format.aaudio_value());
            (api.builder_set_performance_mode)(builder, request.performance.aaudio_value());
            if let Some(set_usage) = api.builder_set_usage {
                set_usage(builder, AAUDIO_USAGE_MEDIA);
            }
            if let Some(set_content_type) = api.builder_set_content_type {
                set_content_type(builder, AAUDIO_CONTENT_TYPE_MUSIC);
            }
            (api.builder_set_data_callback)(builder, Some(data_callback), user as *mut c_void);
            (api.builder_set_error_callback)(builder, Some(error_callback), user as *mut c_void);

            let mut stream: *mut RawStream = std::ptr::null_mut();
            let result = (api.builder_open_stream)(builder, &mut stream);
            (api.builder_delete)(builder);
            if result != AAUDIO_OK || stream.is_null() {
                return Err(error_for_result("opening the stream", result));
            }

            if let Some(bursts) = request.device_buffer_bursts {
                let burst = (api.stream_get_frames_per_burst)(stream);
                if burst > 0 {
                    let wanted = burst.saturating_mul(i32::try_from(bursts).unwrap_or(i32::MAX));
                    (api.stream_set_buffer_size)(stream, wanted);
                }
            }
            let grant = read_grant(api, stream);
            Ok(Self {
                api,
                stream: Mutex::new(Some(StreamPtr(stream))),
                user,
                rate: request.sample_rate,
                grant,
            })
        }
    }
}

/// Reads back what the platform granted.
///
/// # Safety
/// `stream` is an open AAudio stream.
unsafe fn read_grant(api: &Api, stream: *mut RawStream) -> Grant {
    let hardware = match (
        api.stream_get_hardware_sample_rate,
        api.stream_get_hardware_channel_count,
        api.stream_get_hardware_format,
    ) {
        (Some(rate), Some(channels), Some(format)) => Some(Hardware {
            sample_rate: rate(stream),
            channels: channels(stream),
            format: format(stream),
        }),
        _ => None,
    };
    Grant {
        sharing: (api.stream_get_sharing_mode)(stream),
        sample_rate: (api.stream_get_sample_rate)(stream),
        channels: (api.stream_get_channel_count)(stream),
        format: (api.stream_get_format)(stream),
        frames_per_burst: (api.stream_get_frames_per_burst)(stream),
        buffer_capacity_frames: (api.stream_get_buffer_capacity)(stream),
        buffer_size_frames: (api.stream_get_buffer_size)(stream),
        device_id: (api.stream_get_device_id)(stream),
        performance_mode: (api.stream_get_performance_mode)(stream),
        mmap: api.stream_is_mmap_used.map(|is_mmap| is_mmap(stream)),
        hardware,
    }
}

impl Device for AndroidDevice {
    fn grant(&self) -> Grant {
        self.grant.clone()
    }

    fn start(&self) -> Result<(), AaudioError> {
        let guard = self.stream.lock();
        let Some(stream) = guard.as_ref() else {
            return Err(AaudioError::invalid_state("the stream is closed"));
        };
        // SAFETY: the stream is open while it is in the mutex.
        let result = unsafe { (self.api.stream_request_start)(stream.0) };
        if result != AAUDIO_OK {
            return Err(error_for_result("starting the stream", result));
        }
        Ok(())
    }

    fn close(&self) -> Result<(), AaudioError> {
        let Some(stream) = self.stream.lock().take() else {
            return Ok(());
        };
        let api = self.api;
        // SAFETY: the pointer left the mutex, so nothing else uses it any more. The callbacks are
        // told to stop (`Playback::begin_close`) before this runs; stopping and then closing makes
        // AAudio wait for a callback that is running.
        let result = unsafe {
            (api.stream_request_stop)(stream.0);
            let mut state = (api.stream_get_state)(stream.0);
            for _ in 0..20 {
                if state == STATE_STOPPED || state == STATE_DISCONNECTED {
                    break;
                }
                let mut next = state;
                (api.stream_wait_for_state_change)(stream.0, state, &mut next, 25_000_000);
                state = (api.stream_get_state)(stream.0);
            }
            (api.stream_close)(stream.0)
        };
        if result == AAUDIO_OK {
            // SAFETY: the stream is closed, so no callback holds `user` any more.
            drop(unsafe { Arc::from_raw(self.user) });
            Ok(())
        } else {
            // A stream that would not close may still call back: keep `Shared` alive.
            Err(error_for_result("closing the stream", result))
        }
    }

    fn presented_frames(&self) -> i64 {
        let guard = self.stream.lock();
        let Some(stream) = guard.as_ref() else {
            return 0;
        };
        // SAFETY: the stream is open while it is in the mutex.
        unsafe {
            let frames_read = (self.api.stream_get_frames_read)(stream.0);
            let running = (self.api.stream_get_state)(stream.0) == STATE_STARTED;
            let mut frame = 0i64;
            let mut time_ns = 0i64;
            let timestamp = (self.api.stream_get_timestamp)(
                stream.0,
                CLOCK_MONOTONIC,
                &mut frame,
                &mut time_ns,
            );
            presented_position(
                (timestamp == AAUDIO_OK).then_some(Timestamp { frame, time_ns }),
                monotonic_ns(),
                self.rate,
                frames_read,
                running,
            )
        }
    }

    fn xruns(&self) -> u32 {
        let guard = self.stream.lock();
        let Some(stream) = guard.as_ref() else {
            return 0;
        };
        // SAFETY: the stream is open while it is in the mutex.
        u32::try_from(unsafe { (self.api.stream_get_xrun_count)(stream.0) }).unwrap_or(0)
    }
}
