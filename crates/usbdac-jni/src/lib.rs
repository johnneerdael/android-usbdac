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
mod convert;
mod capture;
mod capture_json;
mod routing;
mod json;
mod listener;
mod registry;

use jni::errors::ThrowRuntimeExAndDefault;
use jni::objects::{JByteBuffer, JObject, JString};
use jni::strings::JNIString;
use jni::{jni_mangle, jni_str, Env, EnvUnowned};
use listener::JavaListener;
use registry::DacEntry;
use serde_json::Value;
use std::sync::Arc;
use usbdac_core::{Dac, DacListener, Error};

/// What went wrong inside a native call.
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

// ---- library --------------------------------------------------------------

/// The library version, `"<major>.<minor>.<patch>"`.
#[jni_mangle("dev.usbdac.internal.Native")]
pub fn version<'l>(mut env: EnvUnowned<'l>, _class: JObject<'l>) -> JString<'l> {
    run(&mut env, |env| {
        Ok(JString::from_str(env, env!("CARGO_PKG_VERSION"))?)
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
        Ok(registry::insert_stream(&entry, stream) as i64)
    })
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
