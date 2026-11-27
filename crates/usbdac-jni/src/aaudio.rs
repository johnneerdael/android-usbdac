//! The AAudio exclusive-mode output: the JNI surface of `dev.usbdac.internal.AAudioNative`.
//!
//! An output asks the platform for an exclusive AAudio stream at the source's own rate, channel
//! count and sample format, and feeds it from a ring the host writes into. The audio bytes are
//! copied through unchanged: nothing here converts, dithers or scales.
//!
//! - `logic`: formats and the API level each needs, the grant and its verdict, error mapping, the
//!   audible-position estimate. Pure, host-tested.
//! - `ring`: the lock-free byte ring between the host's writer and AAudio's data callback.
//! - `playback`: the transport state machine, the render function the callback runs, and the
//!   music/silence accounting behind `playedFrames`. Pure, host-tested with a simulated device.
//! - `engine`: an open output (shared state plus a `Device`), host-tested with a fake device.
//! - `ffi` (Android only): the AAudio C API through `dlopen`/`dlsym`, and the callbacks.
//!
//! Handles are opaque ids into a registry, as everywhere in the JNI layer. Failures are
//! `IllegalStateException("usbdac:<code>: <message>")`; the codes are those of `docs/API.md`.
//! `docs/API.md` section 13 has the contract.

// Everything the Android build uses is also compiled on the host for the tests.
#![cfg_attr(not(target_os = "android"), allow(dead_code))]

mod engine;
#[cfg(target_os = "android")]
mod ffi;
mod logic;
mod playback;
mod ring;

use engine::{Event, Output, Shared};
use jni::errors::ThrowRuntimeExAndDefault;
use jni::objects::{JByteBuffer, JObject, JString, JValue};
use jni::{jni_mangle, jni_sig, jni_str, Env, EnvUnowned, JavaVM};
use logic::{
    check_api_level, error_for_result, AaudioError, OpenRequest, ERROR_DISCONNECTED,
    EVENT_DISCONNECTED, EVENT_FAILED,
};
use once_cell::sync::Lazy;
use parking_lot::Mutex;
use playback::Playback;
use std::collections::HashMap;
use std::sync::Arc;

/// What went wrong inside a native call.
enum Failure {
    Aaudio(AaudioError),
    Jni(jni::errors::Error),
}

impl From<AaudioError> for Failure {
    fn from(error: AaudioError) -> Self {
        Failure::Aaudio(error)
    }
}

impl From<jni::errors::Error> for Failure {
    fn from(error: jni::errors::Error) -> Self {
        Failure::Jni(error)
    }
}

type Outcome<T> = Result<T, Failure>;

/// Runs a native method body: a failure becomes the documented exception and the type's default
/// value is returned to Java.
fn run<'l, T: Default>(
    env: &mut EnvUnowned<'l>,
    body: impl FnOnce(&mut Env<'l>) -> Outcome<T>,
) -> T {
    env.with_env(|env| -> jni::errors::Result<T> {
        match body(env) {
            Ok(value) => Ok(value),
            Err(Failure::Aaudio(error)) => {
                crate::throw(env, error.code, &error.message)?;
                Err(jni::errors::Error::JavaException)
            }
            Err(Failure::Jni(error)) => Err(error),
        }
    })
    .resolve::<ThrowRuntimeExAndDefault>()
}

// ---- registry ---------------------------------------------------------------------------------

static OUTPUTS: Lazy<Mutex<HashMap<u64, Arc<Output>>>> = Lazy::new(|| Mutex::new(HashMap::new()));

fn output(id: i64) -> Result<Arc<Output>, AaudioError> {
    u64::try_from(id)
        .ok()
        .and_then(|id| OUTPUTS.lock().get(&id).cloned())
        .ok_or_else(|| {
            AaudioError::invalid_argument(format!("AAudio output handle {id} is not open"))
        })
}

// ---- the listener -----------------------------------------------------------------------------

/// Tells the Java listener (`onEvent(type, a, b, message)`) about an event, from whatever thread
/// the platform called back on.
fn deliver(target: &jni::objects::Global<JObject<'static>>, event: Event) {
    let (kind, code) = match event {
        Event::Disconnected(code) => (EVENT_DISCONNECTED, code),
        Event::Failed(code) => (EVENT_FAILED, code),
    };
    let message = if code == ERROR_DISCONNECTED {
        "the audio route was disconnected (the device was unplugged or the routing changed)"
            .to_string()
    } else {
        error_for_result("while playing", code).message
    };
    let Ok(vm) = JavaVM::singleton() else {
        log::warn!("no JavaVM; dropping AAudio event {event:?}");
        return;
    };
    let result = vm.attach_current_thread(|env| -> jni::errors::Result<()> {
        let message = JString::from_str(env, &message)?;
        env.call_method(
            target,
            jni_str!("onEvent"),
            jni_sig!("(IJDLjava/lang/String;)V"),
            &[
                JValue::Int(kind),
                JValue::Long(i64::from(code)),
                JValue::Double(0.0),
                JValue::Object(&message),
            ],
        )?;
        Ok(())
    });
    if let Err(error) = result {
        log::warn!("the AAudio listener failed on {event:?}: {error}");
    }
}

// ---- opening ----------------------------------------------------------------------------------

#[cfg(target_os = "android")]
fn open_device(
    request: &OpenRequest,
    shared: &Arc<Shared>,
) -> Result<Box<dyn engine::Device>, AaudioError> {
    Ok(Box::new(ffi::AndroidDevice::open(request, shared)?))
}

#[cfg(not(target_os = "android"))]
fn open_device(
    _request: &OpenRequest,
    _shared: &Arc<Shared>,
) -> Result<Box<dyn engine::Device>, AaudioError> {
    Err(AaudioError::platform_unsupported())
}

fn open_output(
    request: &OpenRequest,
    api_level: u32,
    notify: Box<dyn Fn(Event) + Send + Sync>,
) -> Result<Output, AaudioError> {
    check_api_level(request.format, api_level)?;
    let shared = Arc::new(Shared::new(Playback::new(request)?, notify));
    let device = open_device(request, &shared)?;
    Output::new(shared, device, request.capability())
}

// ---- JNI --------------------------------------------------------------------------------------

/// Opens an exclusive-mode output. `requestJson` is documented in `docs/API.md` section 13;
/// `listener` receives `onEvent(type, a, b, message)` for the route's disconnection (type 0) and
/// other failures (type 1); `apiLevel` is `Build.VERSION.SDK_INT`. The stream is open but not
/// started: read `aaudioGrant`, then call `aaudioStart`. Returns the handle.
#[jni_mangle("dev.usbdac.internal.AAudioNative")]
pub fn aaudio_open<'l>(
    mut env: EnvUnowned<'l>,
    _class: JObject<'l>,
    request_json: JString<'l>,
    listener: JObject<'l>,
    api_level: i32,
) -> i64 {
    run(&mut env, |env| {
        if request_json.is_null() {
            return Err(AaudioError::invalid_argument("the request is null").into());
        }
        let request = OpenRequest::from_json(&request_json.try_to_string(env)?)?;
        let notify: Box<dyn Fn(Event) + Send + Sync> = if listener.is_null() {
            Box::new(|_| {})
        } else {
            let target = env.new_global_ref(&listener)?;
            Box::new(move |event| deliver(&target, event))
        };
        let level = u32::try_from(api_level).unwrap_or(0);
        let output = open_output(&request, level, notify)?;
        let id = crate::registry::next_id();
        OUTPUTS.lock().insert(id, Arc::new(output));
        Ok(id as i64)
    })
}

/// The grant and its bit-perfect verdict, as JSON (see `docs/API.md` section 13).
#[jni_mangle("dev.usbdac.internal.AAudioNative")]
pub fn aaudio_grant<'l>(mut env: EnvUnowned<'l>, _class: JObject<'l>, handle: i64) -> JString<'l> {
    run(&mut env, |env| {
        let text = output(handle)?.grant_json();
        Ok(JString::from_str(env, text)?)
    })
}

/// Starts the stream. It plays silence until the ring holds the start threshold.
#[jni_mangle("dev.usbdac.internal.AAudioNative")]
pub fn aaudio_start<'l>(mut env: EnvUnowned<'l>, _class: JObject<'l>, handle: i64) {
    run(&mut env, |_env| Ok(output(handle)?.start()?))
}

/// Marks an invalidated constrained route failed before delivering a disconnect event.
#[jni_mangle("dev.usbdac.internal.AAudioNative")]
pub fn aaudio_invalidate_route<'l>(mut env: EnvUnowned<'l>, _class: JObject<'l>, handle: i64) {
    run(&mut env, |_env| Ok(output(handle)?.invalidate_route()?))
}

/// Stops the stream and releases the handle. Blocks for up to about half a second.
#[jni_mangle("dev.usbdac.internal.AAudioNative")]
pub fn aaudio_close<'l>(mut env: EnvUnowned<'l>, _class: JObject<'l>, handle: i64) {
    run(&mut env, |_env| {
        let removed = u64::try_from(handle)
            .ok()
            .and_then(|id| OUTPUTS.lock().remove(&id))
            .ok_or_else(|| {
                AaudioError::invalid_argument(format!("AAudio output handle {handle} is not open"))
            })?;
        Ok(removed.close()?)
    })
}

/// Writes `count` bytes starting at `offset` of a direct `ByteBuffer`, in whole frames of the
/// opened format. Returns the bytes taken. Does not move the buffer's position.
#[jni_mangle("dev.usbdac.internal.AAudioNative")]
pub fn aaudio_write<'l>(
    mut env: EnvUnowned<'l>,
    _class: JObject<'l>,
    handle: i64,
    buffer: JByteBuffer<'l>,
    offset: i32,
    count: i32,
) -> i32 {
    run(&mut env, |env| {
        let output = output(handle)?;
        if buffer.is_null() {
            return Err(AaudioError::invalid_argument("the buffer is null").into());
        }
        let (offset, count) = match (usize::try_from(offset), usize::try_from(count)) {
            (Ok(offset), Ok(count)) => (offset, count),
            _ => {
                return Err(
                    AaudioError::invalid_argument("offset and count must not be negative").into(),
                )
            }
        };
        let capacity = env
            .get_direct_buffer_capacity(&buffer)
            .map_err(|_| AaudioError::invalid_argument("the buffer is not a direct ByteBuffer"))?;
        if offset.checked_add(count).is_none_or(|end| end > capacity) {
            return Err(AaudioError::invalid_argument(format!(
                "offset {offset} + count {count} exceeds the buffer capacity {capacity}"
            ))
            .into());
        }
        let base = env
            .get_direct_buffer_address(&buffer)
            .map_err(|_| AaudioError::invalid_argument("the buffer is not a direct ByteBuffer"))?;
        // SAFETY: the range was checked against the direct buffer's capacity, and the buffer
        // object is kept alive by the caller for this call.
        let bytes = unsafe { std::slice::from_raw_parts(base.add(offset), count) };
        let taken = output.write(bytes)?;
        Ok(i32::try_from(taken).unwrap_or(i32::MAX))
    })
}

#[jni_mangle("dev.usbdac.internal.AAudioNative")]
pub fn aaudio_writable_bytes<'l>(mut env: EnvUnowned<'l>, _class: JObject<'l>, handle: i64) -> i32 {
    run(&mut env, |_env| {
        Ok(i32::try_from(output(handle)?.writable_bytes()).unwrap_or(i32::MAX))
    })
}

#[jni_mangle("dev.usbdac.internal.AAudioNative")]
pub fn aaudio_play<'l>(mut env: EnvUnowned<'l>, _class: JObject<'l>, handle: i64) {
    run(&mut env, |_env| Ok(output(handle)?.play()?))
}

#[jni_mangle("dev.usbdac.internal.AAudioNative")]
pub fn aaudio_pause<'l>(mut env: EnvUnowned<'l>, _class: JObject<'l>, handle: i64) {
    run(&mut env, |_env| Ok(output(handle)?.pause()?))
}

#[jni_mangle("dev.usbdac.internal.AAudioNative")]
pub fn aaudio_flush<'l>(mut env: EnvUnowned<'l>, _class: JObject<'l>, handle: i64) {
    run(&mut env, |_env| Ok(output(handle)?.flush()?))
}

#[jni_mangle("dev.usbdac.internal.AAudioNative")]
pub fn aaudio_drain<'l>(mut env: EnvUnowned<'l>, _class: JObject<'l>, handle: i64) {
    run(&mut env, |_env| Ok(output(handle)?.drain()?))
}

#[jni_mangle("dev.usbdac.internal.AAudioNative")]
pub fn aaudio_is_drained<'l>(mut env: EnvUnowned<'l>, _class: JObject<'l>, handle: i64) -> bool {
    run(&mut env, |_env| Ok(output(handle)?.is_drained()))
}

/// The state code: 0 buffering, 1 playing, 2 paused, 3 draining, 4 drained, 5 closed, 6 failed.
#[jni_mangle("dev.usbdac.internal.AAudioNative")]
pub fn aaudio_state<'l>(mut env: EnvUnowned<'l>, _class: JObject<'l>, handle: i64) -> i32 {
    run(&mut env, |_env| Ok(output(handle)?.state() as i32))
}

#[jni_mangle("dev.usbdac.internal.AAudioNative")]
pub fn aaudio_played_frames<'l>(mut env: EnvUnowned<'l>, _class: JObject<'l>, handle: i64) -> i64 {
    run(&mut env, |_env| {
        Ok(i64::try_from(output(handle)?.played_frames()).unwrap_or(i64::MAX))
    })
}

#[jni_mangle("dev.usbdac.internal.AAudioNative")]
pub fn aaudio_latency_frames<'l>(mut env: EnvUnowned<'l>, _class: JObject<'l>, handle: i64) -> i64 {
    run(&mut env, |_env| {
        Ok(i64::try_from(output(handle)?.latency_frames()).unwrap_or(i64::MAX))
    })
}

/// AAudio's own count of device-side underruns.
#[jni_mangle("dev.usbdac.internal.AAudioNative")]
pub fn aaudio_xruns<'l>(mut env: EnvUnowned<'l>, _class: JObject<'l>, handle: i64) -> i32 {
    run(&mut env, |_env| {
        Ok(i32::try_from(output(handle)?.xruns()).unwrap_or(i32::MAX))
    })
}

/// How often the ring ran dry (the host did not write fast enough).
#[jni_mangle("dev.usbdac.internal.AAudioNative")]
pub fn aaudio_underruns<'l>(mut env: EnvUnowned<'l>, _class: JObject<'l>, handle: i64) -> i64 {
    run(&mut env, |_env| {
        Ok(i64::try_from(output(handle)?.underruns()).unwrap_or(i64::MAX))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stale_handles_are_invalid_arguments() {
        for id in [0, -1, 12345] {
            assert_eq!(output(id).err().unwrap().code, "invalid_argument");
        }
    }

    #[cfg(not(target_os = "android"))]
    #[test]
    fn off_android_an_open_is_platform_unsupported_after_the_api_gate() {
        let request =
            OpenRequest::from_json(r#"{"sample_rate":44100,"channels":2,"format":"i24"}"#).unwrap();
        let error = open_output(&request, 30, Box::new(|_| {})).err().unwrap();
        assert_eq!(error.code, "unsupported", "the API level is checked first");
        let error = open_output(&request, 34, Box::new(|_| {})).err().unwrap();
        assert_eq!(error.code, "platform_unsupported");
    }
}
