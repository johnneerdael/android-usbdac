//! JNI bindings for `usbdac-core`: `libusbdac.so`.
//!
//! The Java/Kotlin side is the object `dev.usbdac.internal.Native`; each
//! `external fun` below has the Rust function of the same name in
//! `snake_case`. Declare them `@JvmStatic`. Handles are opaque `Long` ids into
//! a registry, never pointers. Failures are `IllegalStateException`s whose
//! message starts with `usbdac:<code>: `; see `docs/API.md` for the codes, the
//! JSON schemas and the event type codes.
//!
//! Everything that talks to the DAC (`dacOpen`, `dacNegotiate`, `streamOpen`,
//! the volume calls, closing) blocks for tens to hundreds of milliseconds:
//! call them off the main thread. `streamWrite` and the cheap getters never
//! block.

mod aaudio;
mod capture;
mod capture_json;
mod convert;
mod dsd_pcm;
mod json;
mod listener;
mod pioneer;
mod registry;
mod routing;
mod scarlett;

use jni::errors::ThrowRuntimeExAndDefault;
use jni::objects::{JByteArray, JByteBuffer, JObject, JString};
use jni::strings::JNIString;
use jni::{jni_mangle, jni_str, Env, EnvUnowned};
use listener::JavaListener;
use registry::DacEntry;
use serde_json::Value;
use std::sync::Arc;
use usbdac_core::{Dac, DacListener, Error};

/// What went wrong inside a native call.
#[cfg_attr(test, derive(Debug))]
enum Failure {
    Dac(Error),
    Bad(String),
    Jni(jni::errors::Error),
}

impl From<Error> for Failure {
    fn from(error: Error) -> Self {
        Failure::Dac(error)
    }
}

impl From<jni::errors::Error> for Failure {
    fn from(error: jni::errors::Error) -> Self {
        Failure::Jni(error)
    }
}

impl From<json::BadRequest> for Failure {
    fn from(error: json::BadRequest) -> Self {
        Failure::Bad(error.0)
    }
}

type Outcome<T> = Result<T, Failure>;

/// Throws `IllegalStateException("usbdac:<code>: <message>")`.
fn throw(env: &mut Env<'_>, code: &str, message: &str) -> jni::errors::Result<()> {
    env.throw_new(
        jni_str!("java/lang/IllegalStateException"),
        JNIString::new(format!("usbdac:{code}: {message}")),
    )
}

/// Runs a native method body: a failure becomes the documented exception and
/// the type's default value is returned to Java.
fn run<'l, T: Default>(
    env: &mut EnvUnowned<'l>,
    body: impl FnOnce(&mut Env<'l>) -> Outcome<T>,
) -> T {
    env.with_env(|env| -> jni::errors::Result<T> {
        match body(env) {
            Ok(value) => Ok(value),
            Err(Failure::Dac(error)) => {
                throw(env, error.code(), &error.message())?;
                Err(jni::errors::Error::JavaException)
            }
            Err(Failure::Bad(message)) => {
                throw(env, "invalid_argument", &message)?;
                Err(jni::errors::Error::JavaException)
            }
            Err(Failure::Jni(error)) => Err(error),
        }
    })
    .resolve::<ThrowRuntimeExAndDefault>()
}

fn string_of(env: &Env<'_>, text: &JString<'_>) -> Outcome<String> {
    if text.is_null() {
        return Err(Failure::Bad("a required string argument is null".into()));
    }
    Ok(text.try_to_string(env)?)
}

fn optional_string_of(env: &Env<'_>, text: &JString<'_>) -> Outcome<Option<String>> {
    if text.is_null() {
        return Ok(None);
    }
    Ok(Some(text.try_to_string(env)?))
}

fn java_string<'l>(env: &mut Env<'l>, value: &Value) -> Outcome<JString<'l>> {
    Ok(JString::from_str(env, value.to_string())?)
}

fn listener_of(env: &Env<'_>, target: &JObject<'_>) -> Outcome<Option<Arc<dyn DacListener>>> {
    if target.is_null() {
        return Ok(None);
    }
    let global = env.new_global_ref(target)?;
    Ok(Some(Arc::new(JavaListener::new(global))))
}

/// Rank positive rate candidates using the same policy as USB negotiation.
#[jni_mangle("dev.usbdac.internal.NativeRateRanking")]
pub fn rank<'l>(
    mut env: EnvUnowned<'l>,
    _class: JObject<'l>,
    requested: i32,
    values: JString<'l>,
) -> JString<'l> {
    run(&mut env, |env| {
        if requested <= 0 {
            return Err(Failure::Bad("requested rate must be positive".into()));
        }
        let input = string_of(env, &values)?;
        let mut rates: Vec<u32> = serde_json::from_str(&input)
            .map_err(|_| Failure::Bad("rates must be a JSON array of positive integers".into()))?;
        if rates.len() > 8192
            || rates.contains(&0)
            || rates.iter().any(|&rate| rate > i32::MAX as u32)
        {
            return Err(Failure::Bad("rate candidates out of range".into()));
        }
        usbdac_core::rank_sample_rates(&mut rates, requested as u32);
        Ok(JString::from_str(
            env,
            serde_json::to_string(&rates).expect("rate array serialization"),
        )?)
    })
}

// ---- library --------------------------------------------------------------

/// The library version, `"<major>.<minor>.<patch>"`.
#[jni_mangle("dev.usbdac.internal.Native")]
pub fn version<'l>(mut env: EnvUnowned<'l>, _class: JObject<'l>) -> JString<'l> {
    run(&mut env, |env| {
        Ok(JString::from_str(env, env!("CARGO_PKG_VERSION"))?)
    })
}

/// Read-only BADD proof before interface claims and native registration.
#[jni_mangle("dev.usbdac.internal.Native")]
pub fn badd_descriptor_witness<'l>(
    mut env: EnvUnowned<'l>,
    _class: JObject<'l>,
    fd: i32,
    active_id: i32,
    snapshot: JByteArray<'l>,
) -> i32 {
    run(&mut env, |env| {
        if !(1..=255).contains(&active_id) {
            return Ok(0);
        }
        let len = snapshot.len(env)?;
        if !(9..=16384).contains(&len) {
            return Ok(0);
        }
        let mut bytes = vec![0i8; len];
        snapshot.get_region(env, 0, &mut bytes)?;
        let raw: Vec<u8> = bytes.into_iter().map(|b| b as u8).collect();
        Ok(i32::from(usbdac_core::badd_descriptor_witness(
            fd,
            active_id as u8,
            &raw,
        )))
    })
}

/// Routes the library's logging to logcat (tag `usbdac`). `level` is 0 off,
/// 1 error, 2 warn, 3 info, 4 debug, 5 trace. Safe to call again to change the
/// level. A no-op off Android.
#[jni_mangle("dev.usbdac.internal.Native")]
pub fn set_log_level<'l>(mut env: EnvUnowned<'l>, _class: JObject<'l>, level: i32) {
    run(&mut env, |_env| {
        init_logging(level);
        Ok(())
    })
}

fn level_filter(level: i32) -> log::LevelFilter {
    match level {
        i32::MIN..=0 => log::LevelFilter::Off,
        1 => log::LevelFilter::Error,
        2 => log::LevelFilter::Warn,
        3 => log::LevelFilter::Info,
        4 => log::LevelFilter::Debug,
        _ => log::LevelFilter::Trace,
    }
}

#[cfg(target_os = "android")]
fn init_logging(level: i32) {
    let filter = level_filter(level);
    android_logger::init_once(
        android_logger::Config::default()
            .with_max_level(log::LevelFilter::Trace)
            .with_tag("usbdac"),
    );
    log::set_max_level(filter);
}

#[cfg(not(target_os = "android"))]
fn init_logging(level: i32) {
    log::set_max_level(level_filter(level));
}

// ---- DAC ------------------------------------------------------------------

/// Read-only exact vendor PCM proof before Android detaches its dedicated interface.
#[jni_mangle("dev.usbdac.internal.Native")]
pub fn dac_vendor_pcm_preflight<'l>(
    mut env: EnvUnowned<'l>,
    _class: JObject<'l>,
    fd: i32,
    device_info_json: JString<'l>,
) -> JString<'l> {
    run(&mut env, |env| {
        let info = json::parse_device_info(&string_of(env, &device_info_json)?)?;
        let profile = Dac::vendor_pcm_preflight(fd, &info)?;
        java_string(
            env,
            &serde_json::json!(profile.map(|(configuration, interface)| serde_json::json!({
                "configuration_id": configuration, "interface_id": interface,
            }))),
        )
    })
}

/// Read-only exact vendor PCM configuration and complete interface ownership proof.
#[jni_mangle("dev.usbdac.internal.Native")]
pub fn dac_vendor_pcm_claims_preflight<'l>(
    mut env: EnvUnowned<'l>,
    _class: JObject<'l>,
    fd: i32,
    device_info_json: JString<'l>,
) -> JString<'l> {
    run(&mut env, |env| {
        let info = json::parse_device_info(&string_of(env, &device_info_json)?)?;
        let profile = Dac::vendor_pcm_claims_preflight(fd, &info)?;
        java_string(
            env,
            &serde_json::json!(
                profile.map(|(configuration, interfaces)| serde_json::json!({
                    "configuration_id": configuration, "interfaces": interfaces,
                }))
            ),
        )
    })
}

#[jni_mangle("dev.usbdac.internal.Native")]
pub fn dac_feedback_preflight<'l>(
    mut env: EnvUnowned<'l>,
    _class: JObject<'l>,
    fd: i32,
    device_info_json: JString<'l>,
) -> JString<'l> {
    run(&mut env, |env| {
        let info = json::parse_device_info(&string_of(env, &device_info_json)?)?;
        let (configuration_id, interfaces) = Dac::feedback_preflight(fd, &info)?;
        java_string(
            env,
            &serde_json::json!({"configuration_id": configuration_id, "interfaces": interfaces}),
        )
    })
}

/// Opens the DAC behind `fd`. `deviceInfoJson` and `configJson` (nullable) are
/// documented in `docs/API.md`. Returns the DAC handle.
#[jni_mangle("dev.usbdac.internal.Native")]
pub fn dac_open<'l>(
    mut env: EnvUnowned<'l>,
    _class: JObject<'l>,
    fd: i32,
    device_info_json: JString<'l>,
    config_json: JString<'l>,
) -> i64 {
    run(&mut env, |env| {
        let info = json::parse_device_info(&string_of(env, &device_info_json)?)?;
        let config = json::parse_config(optional_string_of(env, &config_json)?.as_deref())?;
        let dac = Dac::open(fd, info, config)?;
        Ok(registry::insert_dac(DacEntry::new(dac)) as i64)
    })
}

#[jni_mangle("dev.usbdac.internal.Native")]
pub fn dac_await_retirement<'l>(mut env: EnvUnowned<'l>, _class: JObject<'l>) -> bool {
    run(&mut env, |_env| {
        Ok(Dac::await_usb_retirement(std::time::Duration::from_millis(
            500,
        )))
    })
}

/// Retains an owner-specific retirement token across removal of its DAC handle.
#[jni_mangle("dev.usbdac.internal.Native")]
pub fn dac_retirement_token<'l>(mut env: EnvUnowned<'l>, _class: JObject<'l>, dac: i64) -> i64 {
    run(&mut env, |_env| Ok(registry::retirement_token(dac)? as i64))
}
#[jni_mangle("dev.usbdac.internal.Native")]
pub fn dac_await_retirement_token<'l>(
    mut env: EnvUnowned<'l>,
    _class: JObject<'l>,
    token: i64,
) -> bool {
    run(&mut env, |_env| {
        registry::await_retirement(token).map_err(Into::into)
    })
}
#[jni_mangle("dev.usbdac.internal.Native")]
pub fn dac_release_retirement_token<'l>(mut env: EnvUnowned<'l>, _class: JObject<'l>, token: i64) {
    run(&mut env, |_env| {
        registry::release_retirement(token).map_err(Into::into)
    })
}
#[jni_mangle("dev.usbdac.internal.Native")]
pub fn dac_await_connection_retirement<'l>(
    mut env: EnvUnowned<'l>,
    _class: JObject<'l>,
    fd: i32,
) -> bool {
    run(&mut env, |_env| {
        Dac::await_connection_retirement(fd, std::time::Duration::from_millis(500))
            .map_err(Into::into)
    })
}

/// Closes the DAC and every stream opened from it. The handle is invalid
/// afterwards. A close that fails still releases the handle.
#[jni_mangle("dev.usbdac.internal.Native")]
pub fn dac_close<'l>(mut env: EnvUnowned<'l>, _class: JObject<'l>, dac: i64) {
    run(&mut env, |_env| {
        let (entry, streams) = registry::remove_dac(dac)?;
        for capture in registry::remove_dac_captures(dac) {
            let _ = capture.close();
        }
        for stream in streams {
            if let Ok(stream) = Arc::try_unwrap(stream) {
                let _ = stream.close();
            }
        }
        // If another call still holds the DAC, the last holder closes it.
        if let Ok(entry) = Arc::try_unwrap(entry) {
            entry.dac.close()?;
        }
        Ok(())
    })
}

#[jni_mangle("dev.usbdac.internal.Native")]
pub fn dac_capabilities<'l>(mut env: EnvUnowned<'l>, _class: JObject<'l>, dac: i64) -> JString<'l> {
    run(&mut env, |env| {
        let entry = registry::dac(dac)?;
        let capabilities = entry.dac.capabilities()?;
        java_string(env, &json::capabilities(&capabilities))
    })
}

#[jni_mangle("dev.usbdac.internal.Native")]
pub fn dac_feedback_trace<'l>(
    mut env: EnvUnowned<'l>,
    _class: JObject<'l>,
    dac: i64,
) -> JString<'l> {
    run(&mut env, |env| {
        let entry = registry::dac(dac)?;
        let trace = entry
            .dac
            .feedback_trace()
            .map(|trace| json::feedback_trace(&trace, entry.dac.info()))
            .unwrap_or(serde_json::Value::Null);
        java_string(env, &trace)
    })
}

/// Negotiates the wire format for `requestJson`. The result carries a
/// `format_id` to pass to `streamOpen`.
#[jni_mangle("dev.usbdac.internal.Native")]
pub fn dac_negotiate<'l>(
    mut env: EnvUnowned<'l>,
    _class: JObject<'l>,
    dac: i64,
    request_json: JString<'l>,
) -> JString<'l> {
    run(&mut env, |env| {
        let request = json::parse_request(&string_of(env, &request_json)?)?;
        let entry = registry::dac(dac)?;
        let format = entry.dac.negotiate(&request)?;
        let description = json::stream_format(0, &format);
        let id = entry.remember_format(format);
        let mut description = description;
        description["format_id"] = Value::from(id);
        java_string(env, &description)
    })
}

/// Negotiate a bounded ordered candidate list using one retained-owner reservation.
#[jni_mangle("dev.usbdac.internal.Native")]
pub fn dac_negotiate_candidates<'l>(
    mut env: EnvUnowned<'l>,
    _class: JObject<'l>,
    dac: i64,
    requests_json: JString<'l>,
) -> JString<'l> {
    run(&mut env, |env| {
        let value: Value = serde_json::from_str(&string_of(env, &requests_json)?)
            .map_err(|error| Failure::Bad(format!("invalid candidate list: {error}")))?;
        let array = value
            .as_array()
            .ok_or_else(|| Failure::Bad("candidate list must be an array".into()))?;
        if array.is_empty() || array.len() > 8 {
            return Err(Failure::Bad("expected one to eight PCM candidates".into()));
        }
        let requests = array
            .iter()
            .map(|item| json::parse_request(&item.to_string()))
            .collect::<Result<Vec<_>, _>>()?;
        let entry = registry::dac(dac)?;
        let format = entry.dac.negotiate_candidates(&requests)?;
        let mut description = json::stream_format(0, &format);
        let id = entry.remember_format(format);
        description["format_id"] = Value::from(id);
        java_string(env, &description)
    })
}

/// Sets the listener for device-level events (volume, device loss); `null`
/// clears it.
#[jni_mangle("dev.usbdac.internal.Native")]
pub fn dac_set_listener<'l>(
    mut env: EnvUnowned<'l>,
    _class: JObject<'l>,
    dac: i64,
    listener: JObject<'l>,
) {
    run(&mut env, |env| {
        let entry = registry::dac(dac)?;
        entry.dac.set_listener(listener_of(env, &listener)?);
        Ok(())
    })
}

// ---- stream ---------------------------------------------------------------

/// Opens the stream for a negotiated `formatId`. `listener` receives the
/// stream's events and must have `onEvent(Int, Long, Double, String?)`.
#[jni_mangle("dev.usbdac.internal.Native")]
pub fn stream_open<'l>(
    mut env: EnvUnowned<'l>,
    _class: JObject<'l>,
    dac: i64,
    format_id: i64,
    listener: JObject<'l>,
) -> i64 {
    run(&mut env, |env| {
        let entry = registry::dac(dac)?;
        let format = u64::try_from(format_id)
            .ok()
            .and_then(|id| entry.format(id))
            .ok_or_else(|| {
                Failure::Dac(Error::InvalidArgument(format!(
                    "format {format_id} was not negotiated on this DAC"
                )))
            })?;
        let listener = listener_of(env, &listener)?
            .ok_or_else(|| Failure::Bad("the stream listener must not be null".into()))?;
        let stream = entry.dac.open_stream(&format, listener)?;
        Ok(registry::insert_stream(&entry, stream)? as i64)
    })
}

/// Drains and replaces a stream, retaining its registration on timeout. The replacement
/// JSON is allocated before atomic registration so an allocation failure cannot leak a handle.
#[jni_mangle("dev.usbdac.internal.Native")]
pub fn stream_reconfigure<'l>(
    mut env: EnvUnowned<'l>,
    _class: JObject<'l>,
    dac: i64,
    current: i64,
    request_json: JString<'l>,
    timeout_ms: i64,
    listener: JObject<'l>,
) -> JString<'l> {
    run(&mut env, |env| {
        let request = json::parse_request(&string_of(env, &request_json)?)?;
        reconfigure_registered(env, dac, current, &[request], timeout_ms, &listener)
    })
}

/// Reconfigure from a bounded list of equivalent source-layout candidates.
#[jni_mangle("dev.usbdac.internal.Native")]
pub fn stream_reconfigure_candidates<'l>(
    mut env: EnvUnowned<'l>,
    _class: JObject<'l>,
    dac: i64,
    current: i64,
    requests_json: JString<'l>,
    timeout_ms: i64,
    listener: JObject<'l>,
) -> JString<'l> {
    run(&mut env, |env| {
        let value: Value = serde_json::from_str(&string_of(env, &requests_json)?)
            .map_err(|error| Failure::Bad(format!("invalid candidate list: {error}")))?;
        let array = value
            .as_array()
            .ok_or_else(|| Failure::Bad("candidate list must be an array".into()))?;
        if array.is_empty() || array.len() > 4 {
            return Err(Failure::Bad(
                "candidate list must contain one to four entries".into(),
            ));
        }
        let requests = array
            .iter()
            .map(|item| json::parse_request(&item.to_string()))
            .collect::<Result<Vec<_>, _>>()?;
        reconfigure_registered(env, dac, current, &requests, timeout_ms, &listener)
    })
}

fn reconfigure_registered<'l>(
    env: &mut Env<'l>,
    dac: i64,
    current: i64,
    requests: &[usbdac_core::StreamRequest],
    timeout_ms: i64,
    listener: &JObject<'l>,
) -> Outcome<JString<'l>> {
    let timeout =
        u64::try_from(timeout_ms).map_err(|_| Failure::Bad("negative drain timeout".into()))?;
    let entry = registry::dac(dac)?;
    let old = registry::stream_for_dac(&entry, current)?;
    let listener = listener_of(env, listener)?
        .ok_or_else(|| Failure::Bad("stream listener is required".into()))?;
    let report = entry.dac.reconfigure_stream_candidates(
        &old,
        requests,
        listener,
        std::time::Duration::from_millis(timeout),
    )?;
    let id = registry::next_id();
    let format_id = entry.remember_format(report.stream.format().clone());
    let response = serde_json::json!({"stream_id": id,
        "format": json::stream_format(format_id, report.stream.format()),
        "rolled_back": report.rollback_cause.is_some(),
        "error_code": report.rollback_cause.as_ref().map(|e| e.code()),
        "error_message": report.rollback_cause.as_ref().map(|e| e.message())});
    let result = java_string(env, &response)?;
    registry::replace_stream(&entry, current, id, report.stream)?;
    Ok(result)
}

#[jni_mangle("dev.usbdac.internal.Native")]
pub fn stream_close<'l>(mut env: EnvUnowned<'l>, _class: JObject<'l>, stream: i64) {
    run(&mut env, |_env| {
        let stream = registry::remove_stream(stream)?;
        // If a call is still using it, it closes when that call returns.
        if let Ok(stream) = Arc::try_unwrap(stream) {
            stream.close()?;
        }
        Ok(())
    })
}

/// Writes `count` wire-layout bytes starting at `offset` of a direct
/// `ByteBuffer`. Returns the bytes taken (see the sample layouts in
/// `docs/API.md`). Does not move the buffer's position.
#[jni_mangle("dev.usbdac.internal.Native")]
pub fn stream_write<'l>(
    mut env: EnvUnowned<'l>,
    _class: JObject<'l>,
    stream: i64,
    buffer: JByteBuffer<'l>,
    offset: i32,
    count: i32,
) -> i32 {
    run(&mut env, |env| {
        let stream = registry::stream(stream)?;
        if buffer.is_null() {
            return Err(Failure::Bad("the buffer is null".into()));
        }
        let (offset, count) = match (usize::try_from(offset), usize::try_from(count)) {
            (Ok(offset), Ok(count)) => (offset, count),
            _ => return Err(Failure::Bad("offset and count must not be negative".into())),
        };
        let capacity = env
            .get_direct_buffer_capacity(&buffer)
            .map_err(|_| Failure::Bad("the buffer is not a direct ByteBuffer".into()))?;
        if offset.checked_add(count).is_none_or(|end| end > capacity) {
            return Err(Failure::Bad(format!(
                "offset {offset} + count {count} exceeds the buffer capacity {capacity}"
            )));
        }
        let base = env
            .get_direct_buffer_address(&buffer)
            .map_err(|_| Failure::Bad("the buffer is not a direct ByteBuffer".into()))?;
        // SAFETY: the range was checked against the direct buffer's capacity,
        // and the buffer object is kept alive by the caller for this call.
        let bytes = unsafe { std::slice::from_raw_parts(base.add(offset), count) };
        let taken = stream.write(bytes)?;
        Ok(i32::try_from(taken).unwrap_or(i32::MAX))
    })
}

#[jni_mangle("dev.usbdac.internal.Native")]
pub fn stream_writable_bytes<'l>(mut env: EnvUnowned<'l>, _class: JObject<'l>, stream: i64) -> i32 {
    run(&mut env, |_env| {
        Ok(i32::try_from(registry::stream(stream)?.writable_bytes()).unwrap_or(i32::MAX))
    })
}

#[jni_mangle("dev.usbdac.internal.Native")]
pub fn stream_play<'l>(mut env: EnvUnowned<'l>, _class: JObject<'l>, stream: i64) {
    run(&mut env, |_env| Ok(registry::stream(stream)?.play()?))
}

#[jni_mangle("dev.usbdac.internal.Native")]
pub fn stream_pause<'l>(mut env: EnvUnowned<'l>, _class: JObject<'l>, stream: i64) {
    run(&mut env, |_env| Ok(registry::stream(stream)?.pause()?))
}

#[jni_mangle("dev.usbdac.internal.Native")]
pub fn stream_flush<'l>(mut env: EnvUnowned<'l>, _class: JObject<'l>, stream: i64) {
    run(&mut env, |_env| Ok(registry::stream(stream)?.flush()?))
}

#[jni_mangle("dev.usbdac.internal.Native")]
pub fn stream_drain<'l>(mut env: EnvUnowned<'l>, _class: JObject<'l>, stream: i64) {
    run(&mut env, |_env| Ok(registry::stream(stream)?.drain()?))
}

#[jni_mangle("dev.usbdac.internal.Native")]
pub fn stream_is_drained<'l>(mut env: EnvUnowned<'l>, _class: JObject<'l>, stream: i64) -> bool {
    run(&mut env, |_env| Ok(registry::stream(stream)?.is_drained()))
}

/// The stream state code: 0 buffering, 1 playing, 2 paused, 3 draining,
/// 4 drained, 5 closed, 6 failed.
#[jni_mangle("dev.usbdac.internal.Native")]
pub fn stream_state<'l>(mut env: EnvUnowned<'l>, _class: JObject<'l>, stream: i64) -> i32 {
    run(&mut env, |_env| {
        Ok(registry::stream(stream)?.state().code())
    })
}

#[jni_mangle("dev.usbdac.internal.Native")]
pub fn stream_played_frames<'l>(mut env: EnvUnowned<'l>, _class: JObject<'l>, stream: i64) -> i64 {
    run(&mut env, |_env| {
        Ok(i64::try_from(registry::stream(stream)?.played_frames()).unwrap_or(i64::MAX))
    })
}

#[jni_mangle("dev.usbdac.internal.Native")]
pub fn stream_latency_frames<'l>(mut env: EnvUnowned<'l>, _class: JObject<'l>, stream: i64) -> i64 {
    run(&mut env, |_env| {
        Ok(i64::try_from(registry::stream(stream)?.latency_frames()).unwrap_or(i64::MAX))
    })
}

#[jni_mangle("dev.usbdac.internal.Native")]
pub fn stream_stats<'l>(mut env: EnvUnowned<'l>, _class: JObject<'l>, stream: i64) -> JString<'l> {
    run(&mut env, |env| {
        let stats = registry::stream(stream)?.stats();
        java_string(env, &json::stream_stats(&stats))
    })
}

#[jni_mangle("dev.usbdac.internal.Native")]
pub fn stream_is_bit_perfect<'l>(
    mut env: EnvUnowned<'l>,
    _class: JObject<'l>,
    stream: i64,
) -> bool {
    run(&mut env, |_env| {
        Ok(registry::stream(stream)?.is_bit_perfect())
    })
}

#[jni_mangle("dev.usbdac.internal.Native")]
pub fn stream_set_software_gain<'l>(
    mut env: EnvUnowned<'l>,
    _class: JObject<'l>,
    stream: i64,
    gain: f32,
) {
    run(&mut env, |_env| {
        Ok(registry::stream(stream)?.set_software_gain(gain)?)
    })
}

#[jni_mangle("dev.usbdac.internal.Native")]
pub fn stream_set_host_processing<'l>(
    mut env: EnvUnowned<'l>,
    _class: JObject<'l>,
    stream: i64,
    active: bool,
) {
    run(&mut env, |_env| {
        registry::stream(stream)?.set_host_processing(active);
        Ok(())
    })
}

// ---- volume ---------------------------------------------------------------

fn volume_of(dac: i64) -> Outcome<usbdac_core::VolumeControl> {
    registry::dac(dac)?.dac.volume().ok_or_else(|| {
        Failure::Dac(Error::Control(
            "the DAC has no hardware volume control".into(),
        ))
    })
}

/// The volume range as JSON, or `null` if the DAC has no hardware volume.
#[jni_mangle("dev.usbdac.internal.Native")]
pub fn volume_range<'l>(mut env: EnvUnowned<'l>, _class: JObject<'l>, dac: i64) -> JString<'l> {
    run(&mut env, |env| match registry::dac(dac)?.dac.volume() {
        Some(volume) => java_string(env, &json::volume_range(&volume.range()?)),
        None => Ok(JString::null()),
    })
}

#[jni_mangle("dev.usbdac.internal.Native")]
pub fn volume_get<'l>(mut env: EnvUnowned<'l>, _class: JObject<'l>, dac: i64) -> JString<'l> {
    run(&mut env, |env| {
        java_string(env, &json::volume_state(&volume_of(dac)?.get()?))
    })
}

#[jni_mangle("dev.usbdac.internal.Native")]
pub fn volume_set_db<'l>(
    mut env: EnvUnowned<'l>,
    _class: JObject<'l>,
    dac: i64,
    db: f64,
) -> JString<'l> {
    run(&mut env, |env| {
        java_string(env, &json::volume_state(&volume_of(dac)?.set_db(db)?))
    })
}

#[jni_mangle("dev.usbdac.internal.Native")]
pub fn volume_set_fraction<'l>(
    mut env: EnvUnowned<'l>,
    _class: JObject<'l>,
    dac: i64,
    fraction: f64,
) -> JString<'l> {
    run(&mut env, |env| {
        java_string(
            env,
            &json::volume_state(&volume_of(dac)?.set_fraction(fraction)?),
        )
    })
}

/// `direction` > 0 up, < 0 down; `steps` 0 selects the default of 16.
#[jni_mangle("dev.usbdac.internal.Native")]
pub fn volume_step<'l>(
    mut env: EnvUnowned<'l>,
    _class: JObject<'l>,
    dac: i64,
    direction: i32,
    steps: i32,
) -> JString<'l> {
    run(&mut env, |env| {
        let direction = direction.clamp(-1, 1) as i8;
        let steps = steps.clamp(0, 255) as u8;
        java_string(
            env,
            &json::volume_state(&volume_of(dac)?.step(direction, steps)?),
        )
    })
}

#[jni_mangle("dev.usbdac.internal.Native")]
pub fn volume_set_mute<'l>(
    mut env: EnvUnowned<'l>,
    _class: JObject<'l>,
    dac: i64,
    muted: bool,
) -> JString<'l> {
    run(&mut env, |env| {
        java_string(env, &json::volume_state(&volume_of(dac)?.set_mute(muted)?))
    })
}

/// What the library did with the DAC's level when it was opened, as JSON
/// (`docs/API.md` section 11), or `null` if the DAC has no hardware volume.
#[jni_mangle("dev.usbdac.internal.Native")]
pub fn volume_startup<'l>(mut env: EnvUnowned<'l>, _class: JObject<'l>, dac: i64) -> JString<'l> {
    run(&mut env, |env| match registry::dac(dac)?.dac.volume() {
        Some(volume) => java_string(env, &json::startup_report(&volume.startup())),
        None => Ok(JString::null()),
    })
}

/// The recorded original level in dB, or NaN if there is none.
#[jni_mangle("dev.usbdac.internal.Native")]
pub fn volume_original_db<'l>(mut env: EnvUnowned<'l>, _class: JObject<'l>, dac: i64) -> f64 {
    run(&mut env, |_env| {
        Ok(volume_of(dac)?.original_db().unwrap_or(f64::NAN))
    })
}

/// Writes the original level back now. True if something was restored.
#[jni_mangle("dev.usbdac.internal.Native")]
pub fn volume_hand_back<'l>(mut env: EnvUnowned<'l>, _class: JObject<'l>, dac: i64) -> bool {
    run(&mut env, |_env| Ok(volume_of(dac)?.hand_back()?))
}
