//! The conversion helpers of `usbdac-core::convert`, as the JNI surface of
//! `dev.usbdac.internal.ConvertNative`.
//!
//! Nothing here touches a DAC or the platform: a converter is a pure function of the bytes it is
//! given, so it can sit in front of any `PcmOutput`. Handles are opaque ids as everywhere in the JNI
//! layer, failures are `IllegalStateException("usbdac:<code>: <message>")` with the codes
//! `unsupported`, `invalid_argument` and `invalid_state`. The JSON schemas are in `docs/API.md`
//! section 14.

use jni::errors::ThrowRuntimeExAndDefault;
use jni::ids::JMethodID;
use jni::objects::{JByteBuffer, JObject, JString};
use jni::signature::{Primitive, ReturnType};
use jni::{jni_mangle, jni_sig, jni_str, Env, EnvUnowned};
use once_cell::sync::Lazy;
use parking_lot::Mutex;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Arc, OnceLock};
use usbdac_core::convert::{
    filter_info, ConversionPlan, ConvertError, ConvertOptions, Converter, Narrowing, PcmFormat,
    Quality, SampleFormat,
};

/// What went wrong inside a native call.
pub(crate) enum Failure {
    Convert(ConvertError),
    Jni(jni::errors::Error),
}

impl From<ConvertError> for Failure {
    fn from(error: ConvertError) -> Self {
        Failure::Convert(error)
    }
}

impl From<jni::errors::Error> for Failure {
    fn from(error: jni::errors::Error) -> Self {
        Failure::Jni(error)
    }
}

pub(crate) type Outcome<T> = Result<T, Failure>;

pub(crate) fn run<'l, T: Default>(
    env: &mut EnvUnowned<'l>,
    body: impl FnOnce(&mut Env<'l>) -> Outcome<T>,
) -> T {
    env.with_env(|env| -> jni::errors::Result<T> {
        match body(env) {
            Ok(value) => Ok(value),
            Err(Failure::Convert(error)) => {
                crate::throw(env, error.code(), error.message())?;
                Err(jni::errors::Error::JavaException)
            }
            Err(Failure::Jni(error)) => Err(error),
        }
    })
    .resolve::<ThrowRuntimeExAndDefault>()
}

fn bad(message: impl Into<String>) -> ConvertError {
    ConvertError::InvalidArgument(message.into())
}

// ---- JSON -------------------------------------------------------------------------------------

pub(crate) fn sample_from_json(value: &Value) -> Result<SampleFormat, ConvertError> {
    match value.get("kind").and_then(Value::as_str) {
        Some("float") => Ok(SampleFormat::Float32),
        Some("int") => {
            let field = |key: &str| {
                value
                    .get(key)
                    .and_then(Value::as_u64)
                    .and_then(|n| u8::try_from(n).ok())
                    .ok_or_else(|| bad(format!("the sample needs an integer \"{key}\"")))
            };
            SampleFormat::int(field("bits")?, field("slot_bytes")?)
        }
        _ => Err(bad("the sample's \"kind\" must be \"int\" or \"float\"")),
    }
}

fn sample_to_json(sample: SampleFormat) -> Value {
    match sample {
        SampleFormat::Float32 => json!({ "kind": "float" }),
        SampleFormat::Int { bits, slot_bytes } => {
            json!({ "kind": "int", "bits": bits, "slot_bytes": slot_bytes })
        }
    }
}

fn format_from_json(value: &Value, what: &str) -> Result<PcmFormat, ConvertError> {
    let section = value
        .get(what)
        .ok_or_else(|| bad(format!("the request has no \"{what}\"")))?;
    let number = |key: &str| {
        section
            .get(key)
            .and_then(Value::as_u64)
            .ok_or_else(|| bad(format!("\"{what}\" needs an integer \"{key}\"")))
    };
    let rate = u32::try_from(number("sample_rate")?)
        .map_err(|_| bad("the sample rate is out of range"))?;
    let channels =
        u16::try_from(number("channels")?).map_err(|_| bad("the channel count is out of range"))?;
    let sample = sample_from_json(
        section
            .get("sample")
            .ok_or_else(|| bad(format!("\"{what}\" has no \"sample\"")))?,
    )?;
    Ok(PcmFormat::new(rate, channels, sample))
}

fn format_to_json(format: PcmFormat) -> Value {
    json!({
        "sample_rate": format.sample_rate,
        "channels": format.channels,
        "sample": sample_to_json(format.sample),
        "bytes_per_frame": format.bytes_per_frame(),
    })
}

pub(crate) fn options_from_json(value: &Value) -> Result<ConvertOptions, ConvertError> {
    let mut options = ConvertOptions::default();
    let Some(section) = value.get("options").filter(|options| !options.is_null()) else {
        return Ok(options);
    };
    if let Some(name) = section.get("narrowing").and_then(Value::as_str) {
        options.narrowing = match name {
            "truncate" => Narrowing::Truncate,
            "tpdf" => Narrowing::TpdfDither,
            other => {
                return Err(bad(format!(
                    "\"narrowing\" must be truncate or tpdf, not \"{other}\""
                )))
            }
        };
    }
    if let Some(name) = section.get("quality").and_then(Value::as_str) {
        options.quality = match name {
            "high" => Quality::High,
            "fast" => Quality::Fast,
            other => {
                return Err(bad(format!(
                    "\"quality\" must be high or fast, not \"{other}\""
                )))
            }
        };
    }
    if let Some(headroom) = section.get("headroom_db") {
        options.headroom_db = headroom
            .as_f64()
            .ok_or_else(|| bad("\"headroom_db\" must be a number"))?;
    }
    if let Some(seed) = section.get("dither_seed").filter(|seed| !seed.is_null()) {
        options.dither_seed = seed
            .as_u64()
            .ok_or_else(|| bad("\"dither_seed\" must be a non-negative integer"))?;
    }
    Ok(options)
}

/// Reads a request: `{ "source": {...}, "target": {...}, "options": {...} }`.
fn plan_from_json(text: &str) -> Result<ConversionPlan, ConvertError> {
    let value: Value =
        serde_json::from_str(text).map_err(|error| bad(format!("malformed request: {error}")))?;
    let source = format_from_json(&value, "source")?;
    let target = format_from_json(&value, "target")?;
    ConversionPlan::pcm(source, target, &options_from_json(&value)?)
}

/// The plan as JSON.
fn plan_to_json(plan: &ConversionPlan) -> Result<Value, ConvertError> {
    let filter = if plan.resamples() {
        let info = filter_info(
            plan.source().sample_rate,
            plan.target().sample_rate,
            plan.options().quality,
        )?;
        Some(info)
    } else {
        None
    };
    let target_frame = plan.target().bytes_per_frame();
    Ok(json!({
        "kind": plan.kind().code(),
        "description": plan.description(),
        "preserves_bit_perfect": plan.preserves_bit_perfect(),
        "resamples": plan.resamples(),
        "steps": plan.steps().iter().map(|step| json!({
            "code": step.code(),
            "kind": step.kind().code(),
            "description": step.describe(),
        })).collect::<Vec<_>>(),
        "source": format_to_json(plan.source()),
        "target": format_to_json(plan.target()),
        "source_frame_bytes": plan.source().bytes_per_frame(),
        "target_frame_bytes": target_frame,
        "delay_frames": filter.map_or(0, |info| info.taps / 2),
        "min_output_bytes": filter.map_or(1, |info| info.l.div_ceil(info.m).max(1)) * target_frame,
        "filter": filter.map(|info| json!({
            "l": info.l,
            "m": info.m,
            "taps": info.taps,
            "passband_hz": info.passband_hz,
            "stopband_hz": info.stopband_hz,
            "design_attenuation_db": info.design_attenuation_db,
        })),
    }))
}

// ---- registry ---------------------------------------------------------------------------------

static CONVERTERS: Lazy<Mutex<HashMap<u64, Arc<Mutex<Converter>>>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

fn converter(id: i64) -> Result<Arc<Mutex<Converter>>, ConvertError> {
    u64::try_from(id)
        .ok()
        .and_then(|id| CONVERTERS.lock().get(&id).cloned())
        .ok_or_else(|| bad(format!("converter handle {id} is not open")))
}

/// The byte range of a direct buffer, checked against its capacity.
fn direct_range(
    env: &Env<'_>,
    buffer: &JByteBuffer<'_>,
    offset: i32,
    count: i32,
    what: &str,
) -> Result<(*mut u8, usize), ConvertError> {
    if buffer.is_null() {
        return Err(bad(format!("the {what} buffer is null")));
    }
    let (offset, count) = match (usize::try_from(offset), usize::try_from(count)) {
        (Ok(offset), Ok(count)) => (offset, count),
        _ => return Err(bad("offset and count must not be negative")),
    };
    let capacity = env
        .get_direct_buffer_capacity(buffer)
        .map_err(|_| bad(format!("the {what} buffer is not a direct ByteBuffer")))?;
    if offset.checked_add(count).is_none_or(|end| end > capacity) {
        return Err(bad(format!(
            "{what}: offset {offset} + count {count} exceeds the buffer capacity {capacity}"
        )));
    }
    let base = env
        .get_direct_buffer_address(buffer)
        .map_err(|_| bad(format!("the {what} buffer is not a direct ByteBuffer")))?;
    // SAFETY: offset <= capacity, checked above.
    Ok((unsafe { base.add(offset) }, count))
}

/// `java.nio.ByteBuffer.isReadOnly()Z`, resolved on first use.
static IS_READ_ONLY: OnceLock<JMethodID> = OnceLock::new();

/// `buffer.isReadOnly()`, with the method ID cached instead of looked up on
/// every call. The same virtual call as `call_method`, which resolves the name
/// on the receiver's class each time; a null receiver fails as it does there.
pub(crate) fn is_read_only(
    env: &mut Env<'_>,
    buffer: &JByteBuffer<'_>,
) -> jni::errors::Result<bool> {
    if buffer.is_null() {
        return Err(jni::errors::Error::NullPtr("call_method obj argument"));
    }
    let method = match IS_READ_ONLY.get() {
        Some(&method) => method,
        None => {
            let method = env.get_method_id(
                jni_str!("java/nio/ByteBuffer"),
                jni_str!("isReadOnly"),
                jni_sig!("()Z"),
            )?;
            // A racing thread resolves the same ID; keep whichever won.
            *IS_READ_ONLY.get_or_init(|| method)
        }
    };
    // SAFETY: `method` is ByteBuffer's `isReadOnly()Z`: no arguments, boolean
    // return, matching the empty args and `Primitive::Boolean`. The receiver is
    // a non-null `java.nio.ByteBuffer` (checked above; the JVM enforces the
    // declared parameter type), and Call<Type>MethodA dispatches virtually, so
    // every ByteBuffer subclass runs its own override. java.nio.ByteBuffer is a
    // bootstrap class: a class unloads only with its defining loader, and the
    // bootstrap loader never goes away, so the cached ID stays valid.
    unsafe {
        env.call_method_unchecked(
            buffer,
            method,
            ReturnType::Primitive(Primitive::Boolean),
            &[],
        )
    }?
    .z()
}

pub(crate) fn require_writable(env: &mut Env<'_>, buffer: &JByteBuffer<'_>) -> Outcome<()> {
    if is_read_only(env, buffer)? {
        return Err(bad("the output buffer is read-only").into());
    }
    Ok(())
}

// ---- JNI --------------------------------------------------------------------------------------

/// Plans a conversion without building a converter: the request is
/// `{ "source": {...}, "target": {...}, "options": {...} }` (docs/API.md section 14); the answer is
/// the plan's JSON.
#[jni_mangle("dev.usbdac.internal.ConvertNative")]
pub fn convert_plan<'l>(
    mut env: EnvUnowned<'l>,
    _class: JObject<'l>,
    request_json: JString<'l>,
) -> JString<'l> {
    run(&mut env, |env| {
        if request_json.is_null() {
            return Err(bad("the request is null").into());
        }
        let plan = plan_from_json(&request_json.try_to_string(env)?)?;
        Ok(JString::from_str(env, plan_to_json(&plan)?.to_string())?)
    })
}

/// Builds a converter for the same request. Takes a few milliseconds for a resampling plan (the
/// filter is synthesised), so call it off the main thread.
#[jni_mangle("dev.usbdac.internal.ConvertNative")]
pub fn convert_create<'l>(
    mut env: EnvUnowned<'l>,
    _class: JObject<'l>,
    request_json: JString<'l>,
) -> i64 {
    run(&mut env, |env| {
        if request_json.is_null() {
            return Err(bad("the request is null").into());
        }
        let plan = plan_from_json(&request_json.try_to_string(env)?)?;
        let converter = Converter::new(&plan)?;
        let id = crate::registry::next_id();
        CONVERTERS
            .lock()
            .insert(id, Arc::new(Mutex::new(converter)));
        Ok(id as i64)
    })
}

#[jni_mangle("dev.usbdac.internal.ConvertNative")]
pub fn convert_close<'l>(mut env: EnvUnowned<'l>, _class: JObject<'l>, handle: i64) {
    run(&mut env, |_env| {
        u64::try_from(handle)
            .ok()
            .and_then(|id| CONVERTERS.lock().remove(&id))
            .map(|_| ())
            .ok_or_else(|| bad(format!("converter handle {handle} is not open")).into())
    })
}

/// Converts whole frames from `inCount` bytes at `inOffset` of one direct buffer into at most
/// `outCapacity` bytes at `outOffset` of another. Returns `(consumed << 32) | produced`, both in
/// bytes. Neither buffer's position is moved. The buffers must not overlap.
#[jni_mangle("dev.usbdac.internal.ConvertNative")]
pub fn convert_process<'l>(
    mut env: EnvUnowned<'l>,
    _class: JObject<'l>,
    handle: i64,
    input: JByteBuffer<'l>,
    in_offset: i32,
    in_count: i32,
    output: JByteBuffer<'l>,
    out_offset: i32,
    out_capacity: i32,
) -> i64 {
    run(&mut env, |env| {
        let converter = converter(handle)?;
        let (in_ptr, in_len) = direct_range(env, &input, in_offset, in_count, "input")?;
        let (out_ptr, out_len) = direct_range(env, &output, out_offset, out_capacity, "output")?;
        require_writable(env, &output)?;
        let (a, b) = (in_ptr as usize, out_ptr as usize);
        if a < b + out_len && b < a + in_len {
            return Err(bad("the input and output ranges overlap").into());
        }
        // SAFETY: both ranges lie inside direct buffers the caller keeps alive for this call, and they do not overlap; the caller keeps input immutable and output exclusive, including aliases.
        let (input, output) = unsafe {
            (
                std::slice::from_raw_parts(in_ptr as *const u8, in_len),
                std::slice::from_raw_parts_mut(out_ptr, out_len),
            )
        };
        let done = converter.lock().process(input, output)?;
        Ok(((done.consumed as i64) << 32) | done.produced as i64)
    })
}

/// Writes the resampler's tail into `outCapacity` bytes at `outOffset`. Call until it returns 0.
#[jni_mangle("dev.usbdac.internal.ConvertNative")]
pub fn convert_finish<'l>(
    mut env: EnvUnowned<'l>,
    _class: JObject<'l>,
    handle: i64,
    output: JByteBuffer<'l>,
    out_offset: i32,
    out_capacity: i32,
) -> i32 {
    run(&mut env, |env| {
        let converter = converter(handle)?;
        let (out_ptr, out_len) = direct_range(env, &output, out_offset, out_capacity, "output")?;
        require_writable(env, &output)?;
        // SAFETY: the range lies inside a writable direct buffer kept alive and exclusively accessible for this call.
        let output = unsafe { std::slice::from_raw_parts_mut(out_ptr, out_len) };
        let produced = converter.lock().finish(output)?;
        Ok(i32::try_from(produced).unwrap_or(i32::MAX))
    })
}

/// Starts a new stream (a seek, a track change): clears the history, the dither and the counters.
#[jni_mangle("dev.usbdac.internal.ConvertNative")]
pub fn convert_reset<'l>(mut env: EnvUnowned<'l>, _class: JObject<'l>, handle: i64) {
    run(&mut env, |_env| {
        converter(handle)?.lock().reset();
        Ok(())
    })
}

/// Samples that were saturated at full scale since the start or the last reset.
#[jni_mangle("dev.usbdac.internal.ConvertNative")]
pub fn convert_clipped<'l>(mut env: EnvUnowned<'l>, _class: JObject<'l>, handle: i64) -> i64 {
    run(&mut env, |_env| {
        Ok(i64::try_from(converter(handle)?.lock().clipped_samples()).unwrap_or(i64::MAX))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const REQUEST: &str = r#"{
        "source": { "sample_rate": 44100, "channels": 2, "sample": { "kind": "int", "bits": 16, "slot_bytes": 2 } },
        "target": { "sample_rate": 48000, "channels": 2, "sample": { "kind": "int", "bits": 24, "slot_bytes": 4 } },
        "options": { "narrowing": "truncate", "quality": "fast", "headroom_db": -3.0, "dither_seed": 7 }
    }"#;

    #[test]
    fn a_request_becomes_a_plan_with_its_options() {
        let plan = plan_from_json(REQUEST).unwrap();
        assert_eq!(plan.source(), PcmFormat::new(44_100, 2, SampleFormat::I16));
        assert_eq!(
            plan.target(),
            PcmFormat::new(48_000, 2, SampleFormat::I24_IN_32)
        );
        assert_eq!(plan.options().narrowing, Narrowing::Truncate);
        assert_eq!(plan.options().quality, Quality::Fast);
        assert_eq!(plan.options().headroom_db, -3.0);
        assert_eq!(plan.options().dither_seed, 7);
    }

    #[test]
    fn the_plan_json_carries_what_the_kotlin_side_reads() {
        let value = plan_to_json(&plan_from_json(REQUEST).unwrap()).unwrap();
        assert_eq!(value["kind"], "lossy");
        assert_eq!(value["resamples"], true);
        assert_eq!(value["preserves_bit_perfect"], false);
        assert!(value["description"]
            .as_str()
            .unwrap()
            .starts_with("Lossy conversion, not bit-perfect"));
        assert_eq!(value["source_frame_bytes"], 4);
        assert_eq!(value["target_frame_bytes"], 8);
        assert_eq!(value["steps"][0]["code"], "resample");
        assert_eq!(value["steps"][1]["code"], "quantise");
        assert_eq!(value["filter"]["l"], 160);
        assert_eq!(value["filter"]["m"], 147);
        let taps = value["filter"]["taps"].as_u64().unwrap();
        assert_eq!(value["delay_frames"].as_u64().unwrap(), taps / 2);
        assert_eq!(
            value["min_output_bytes"], 16,
            "ceil(160 / 147) = 2 frames of 8 bytes"
        );
    }

    #[test]
    fn a_plan_without_resampling_has_no_filter_and_no_delay() {
        let request = r#"{ "source": { "sample_rate": 96000, "channels": 2, "sample": { "kind": "int", "bits": 16, "slot_bytes": 2 } },
                           "target": { "sample_rate": 96000, "channels": 2, "sample": { "kind": "int", "bits": 32, "slot_bytes": 4 } } }"#;
        let value = plan_to_json(&plan_from_json(request).unwrap()).unwrap();
        assert_eq!(value["kind"], "bit_exact");
        assert_eq!(value["preserves_bit_perfect"], true);
        assert!(value["filter"].is_null());
        assert_eq!(value["delay_frames"], 0);
        assert_eq!(value["min_output_bytes"], 8);
    }

    #[test]
    fn float_samples_and_defaults_are_read() {
        let request = r#"{ "source": { "sample_rate": 48000, "channels": 1, "sample": { "kind": "float" } },
                           "target": { "sample_rate": 48000, "channels": 2, "sample": { "kind": "int", "bits": 24, "slot_bytes": 3 } },
                           "options": null }"#;
        let plan = plan_from_json(request).unwrap();
        assert_eq!(plan.source().sample, SampleFormat::Float32);
        assert_eq!(plan.options(), &ConvertOptions::default());
    }

    #[test]
    fn bad_requests_say_what_is_wrong() {
        for (text, code) in [
            ("not json", "invalid_argument"),
            (r#"{ "target": {} }"#, "invalid_argument"),
            (
                r#"{ "source": { "sample_rate": 44100, "channels": 2, "sample": { "kind": "int", "bits": 16 } }, "target": {} }"#,
                "invalid_argument",
            ),
            (
                r#"{ "source": { "sample_rate": 44100, "channels": 2, "sample": { "kind": "pcm" } }, "target": {} }"#,
                "invalid_argument",
            ),
            (
                r#"{ "source": { "sample_rate": 44100, "channels": 6, "sample": { "kind": "float" } },
                    "target": { "sample_rate": 44100, "channels": 2, "sample": { "kind": "float" } } }"#,
                "unsupported",
            ),
            (
                r#"{ "source": { "sample_rate": 44100, "channels": 2, "sample": { "kind": "float" } },
                    "target": { "sample_rate": 44100, "channels": 2, "sample": { "kind": "float" } },
                    "options": { "quality": "ultra" } }"#,
                "invalid_argument",
            ),
        ] {
            assert_eq!(plan_from_json(text).unwrap_err().code(), code, "{text}");
        }
    }

    #[test]
    fn a_converter_runs_through_the_registry_functions_it_uses() {
        let plan = plan_from_json(
            r#"{ "source": { "sample_rate": 44100, "channels": 1, "sample": { "kind": "int", "bits": 16, "slot_bytes": 2 } },
                 "target": { "sample_rate": 44100, "channels": 2, "sample": { "kind": "int", "bits": 16, "slot_bytes": 2 } } }"#,
        )
        .unwrap();
        let mut converter = Converter::new(&plan).unwrap();
        let mut out = [0u8; 8];
        let done = converter.process(&[1, 0, 2, 0], &mut out).unwrap();
        assert_eq!((done.consumed, done.produced), (4, 8));
        assert_eq!(out, [1, 0, 1, 0, 2, 0, 2, 0]);
        assert!(super::converter(0).is_err());
        assert!(super::converter(-3).is_err());
    }
}
