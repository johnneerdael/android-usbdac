//! Android bridge for the core's retained-session playback decision.
use crate::{convert, java_string, json, registry, run, string_of, Failure};
use jni::{
    jni_mangle,
    objects::{JByteBuffer, JObject, JString},
    EnvUnowned,
};
use serde_json::{json, Value};
use usbdac_core::playback::{PlaybackFidelity, PlaybackPolicy, PlaybackTransform};

#[jni_mangle("dev.usbdac.internal.Native")]
pub fn playback_resolve<'l>(
    mut env: EnvUnowned<'l>,
    _class: JObject<'l>,
    dac: i64,
    request: JString<'l>,
    policy: JString<'l>,
) -> JString<'l> {
    run(&mut env, |env| {
        let request = json::parse_request(&string_of(env, &request)?)?;
        let value: Value = serde_json::from_str(&string_of(env, &policy)?)
            .map_err(|e| Failure::Bad(e.to_string()))?;
        let boolean = |name: &str, default: bool| -> Result<bool, Failure> {
            match value.get(name) {
                None => Ok(default),
                Some(v) => v
                    .as_bool()
                    .ok_or_else(|| Failure::Bad(format!("{name} must be a boolean"))),
            }
        };
        let mut options =
            convert::options_from_json(&value).map_err(|e| Failure::Bad(e.to_string()))?;
        if let Some(sample) = value.get("options").and_then(|o| o.get("source_sample")) {
            options.source_sample =
                Some(convert::sample_from_json(sample).map_err(|e| Failure::Bad(e.to_string()))?);
        }
        let max_pcm_rate = value
            .get("max_pcm_rate")
            .map(|v| {
                v.as_u64()
                    .and_then(|n| u32::try_from(n).ok())
                    .ok_or_else(|| Failure::Bad("invalid max_pcm_rate".into()))
            })
            .transpose()?;
        let allowed_pcm_depths = value
            .get("allowed_pcm_depths")
            .map(|v| -> Result<Vec<u8>, Failure> {
                v.as_array()
                    .ok_or_else(|| Failure::Bad("allowed_pcm_depths must be an array".into()))?
                    .iter()
                    .map(|n| {
                        n.as_u64()
                            .and_then(|v| u8::try_from(v).ok())
                            .ok_or_else(|| Failure::Bad("invalid allowed depth".into()))
                    })
                    .collect()
            })
            .transpose()?;
        let policy = PlaybackPolicy {
            allow_lossless: boolean("allow_lossless", true)?,
            allow_widening: boolean("allow_widening", true)?,
            allow_lossy: boolean("allow_lossy", true)?,
            allow_native_dsd: boolean("allow_native_dsd", true)?,
            dop_confirmed: boolean("dop_confirmed", false)?,
            allow_dop_pcm: boolean("allow_dop_pcm", true)?,
            max_pcm_rate,
            allowed_pcm_depths,
            conversion_options: options,
        };
        let entry = registry::dac(dac)?;
        let decision = entry.dac.resolve_playback(&request, &policy)?;
        let transform = match &decision.transform {
            PlaybackTransform::Direct => "direct",
            PlaybackTransform::Pcm {
                unsigned_input: false,
                ..
            } => "pcm",
            PlaybackTransform::Pcm {
                unsigned_input: true,
                ..
            } => "pcm_u8",
            PlaybackTransform::DsdToPcm {
                dop_input: false, ..
            } => "dsd_to_pcm",
            PlaybackTransform::DsdToPcm {
                dop_input: true, ..
            } => "dop_to_pcm",
            PlaybackTransform::DopToNative { .. } => "dop_to_native",
            PlaybackTransform::NativeToDop { .. } => "native_to_dop",
            PlaybackTransform::DsdChannelExpansion { .. } => "dsd_expand",
            PlaybackTransform::DopRepack { .. } => "dop_repack",
        };
        let fidelity = match decision.fidelity {
            PlaybackFidelity::Exact => "exact",
            PlaybackFidelity::BitExact => "bit_exact",
            PlaybackFidelity::Lossless => "lossless",
            PlaybackFidelity::Lossy => "lossy",
        };
        let selected_headroom_db = match &decision.transform {
            PlaybackTransform::Pcm { plan, .. } => Some(plan.options().headroom_db),
            PlaybackTransform::DsdToPcm { options, .. } => Some(options.headroom_db),
            _ => None,
        };
        let format = decision.format;
        let format_id = entry.remember_format(format.clone());
        java_string(
            env,
            &json!({"format": json::stream_format(format_id, &format), "transform": transform, "fidelity": fidelity,
            "source_frame_bytes": decision.source_frame_bytes, "selected_headroom_db": selected_headroom_db, "exact_refusal": decision.exact_refusal.map(|e| format!("{}: {}", e.code(), e))}),
        )
    })
}

#[jni_mangle("dev.usbdac.internal.Native")]
pub fn playback_u8_to_i16<'l>(
    mut env: EnvUnowned<'l>,
    _class: JObject<'l>,
    input: JByteBuffer<'l>,
    output: JByteBuffer<'l>,
    count: i32,
) -> i32 {
    run(&mut env, |env| {
        let count =
            usize::try_from(count).map_err(|_| Failure::Bad("negative input length".into()))?;
        let in_capacity = env
            .get_direct_buffer_capacity(&input)
            .map_err(|_| Failure::Bad("input must be direct".into()))?;
        let out_capacity = env
            .get_direct_buffer_capacity(&output)
            .map_err(|_| Failure::Bad("output must be direct".into()))?;
        if count > in_capacity || count.checked_mul(2).is_none_or(|n| n > out_capacity) {
            return Err(Failure::Bad("buffer too small".into()));
        }
        let a = env
            .get_direct_buffer_address(&input)
            .map_err(|_| Failure::Bad("input must be direct".into()))?;
        let b = env
            .get_direct_buffer_address(&output)
            .map_err(|_| Failure::Bad("output must be direct".into()))?;
        let a_start = a as usize;
        let b_start = b as usize;
        if a_start < b_start.saturating_add(count * 2) && b_start < a_start.saturating_add(count) {
            return Err(Failure::Bad("buffers overlap".into()));
        }
        // SAFETY: `count` bytes lie inside the input direct buffer (checked
        // against its capacity above), which the caller keeps alive and
        // unmodified for this call.
        let source = unsafe { std::slice::from_raw_parts(a, count) };
        // SAFETY: `count * 2` bytes lie inside the output direct buffer (checked
        // against its capacity above) and do not overlap the input (checked
        // above); the caller keeps it alive and exclusive for this call.
        let target = unsafe { std::slice::from_raw_parts_mut(b, count * 2) };
        Ok(usbdac_core::playback::pcm_u8_to_i16(source, target) as i32)
    })
}

#[jni_mangle("dev.usbdac.internal.Native")]
pub fn playback_dsd_mono_to_stereo<'l>(
    mut env: EnvUnowned<'l>,
    _class: JObject<'l>,
    input: JByteBuffer<'l>,
    output: JByteBuffer<'l>,
    count: i32,
) -> i32 {
    run(&mut env, |env| {
        let count =
            usize::try_from(count).map_err(|_| Failure::Bad("negative input length".into()))?;
        let in_capacity = env
            .get_direct_buffer_capacity(&input)
            .map_err(|_| Failure::Bad("input must be direct".into()))?;
        let out_capacity = env
            .get_direct_buffer_capacity(&output)
            .map_err(|_| Failure::Bad("output must be direct".into()))?;
        if count > in_capacity || count.checked_mul(2).is_none_or(|n| n > out_capacity) {
            return Err(Failure::Bad("buffer too small".into()));
        }
        let a = env
            .get_direct_buffer_address(&input)
            .map_err(|_| Failure::Bad("input must be direct".into()))?;
        let b = env
            .get_direct_buffer_address(&output)
            .map_err(|_| Failure::Bad("output must be direct".into()))?;
        let a_start = a as usize;
        let b_start = b as usize;
        if a_start < b_start.saturating_add(count * 2) && b_start < a_start.saturating_add(count) {
            return Err(Failure::Bad("buffers overlap".into()));
        }
        // SAFETY: `count` bytes lie inside the input direct buffer (checked
        // against its capacity above), which the caller keeps alive and
        // unmodified for this call.
        let source = unsafe { std::slice::from_raw_parts(a, count) };
        // SAFETY: `count * 2` bytes lie inside the output direct buffer (checked
        // against its capacity above) and do not overlap the input (checked
        // above); the caller keeps it alive and exclusive for this call.
        let target = unsafe { std::slice::from_raw_parts_mut(b, count * 2) };
        Ok(usbdac_core::playback::duplicate_mono_dsd(source, target) as i32)
    })
}
