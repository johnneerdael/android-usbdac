//! JNI entry points for explicit standalone capture.
use crate::{
    capture_json, java_string, json, optional_string_of, registry, run, string_of, Failure,
};
use jni::objects::{JByteBuffer, JObject, JString};
use jni::{jni_mangle, jni_sig, jni_str, EnvUnowned};
use usbdac_core::Dac;

#[jni_mangle("dev.usbdac.internal.Native")]
pub fn dac_capture_preflight<'l>(
    mut env: EnvUnowned<'l>,
    _class: JObject<'l>,
    fd: i32,
    info: JString<'l>,
) -> JString<'l> {
    run(&mut env, |env| {
        let info = json::parse_device_info(&string_of(env, &info)?)?;
        let (configuration_id, control_interfaces, input_interfaces) =
            Dac::capture_preflight(fd, &info)?;
        java_string(
            env,
            &serde_json::json!({ "configuration_id": configuration_id,
            "control_interfaces": control_interfaces, "input_interfaces": input_interfaces }),
        )
    })
}

#[jni_mangle("dev.usbdac.internal.Native")]
pub fn dac_open_capture_device<'l>(
    mut env: EnvUnowned<'l>,
    _class: JObject<'l>,
    fd: i32,
    info: JString<'l>,
    config: JString<'l>,
) -> i64 {
    run(&mut env, |env| {
        let info = json::parse_device_info(&string_of(env, &info)?)?;
        let config = json::parse_config(optional_string_of(env, &config)?.as_deref())?;
        let dac = Dac::open_capture_device(fd, info, config)?;
        Ok(registry::insert_dac(registry::DacEntry::new(dac)) as i64)
    })
}

#[jni_mangle("dev.usbdac.internal.Native")]
pub fn dac_open_duplex_device<'l>(
    mut env: EnvUnowned<'l>,
    _class: JObject<'l>,
    fd: i32,
    info: JString<'l>,
    config: JString<'l>,
) -> i64 {
    run(&mut env, |env| {
        let info = json::parse_device_info(&string_of(env, &info)?)?;
        let config = json::parse_config(optional_string_of(env, &config)?.as_deref())?;
        let dac = Dac::open_duplex_device(fd, info, config)?;
        Ok(registry::insert_dac(registry::DacEntry::new(dac)) as i64)
    })
}

#[jni_mangle("dev.usbdac.internal.Native")]
pub fn duplex_open<'l>(
    mut env: EnvUnowned<'l>,
    _class: JObject<'l>,
    dac: i64,
    format_id: i64,
    request: JString<'l>,
    capacity_frames: i32,
    listener: JObject<'l>,
) -> JString<'l> {
    run(&mut env, |env| {
        let entry = registry::dac(dac)?;
        let format = u64::try_from(format_id)
            .ok()
            .and_then(|id| entry.format(id))
            .ok_or_else(|| Failure::Bad("format was not negotiated on this DAC".into()))?;
        let request = capture_json::parse_request(&string_of(env, &request)?)?;
        let capacity = usize::try_from(capacity_frames)
            .ok()
            .filter(|n| *n != 0)
            .ok_or_else(|| Failure::Bad("capture capacity_frames must be positive".into()))?;
        let listener = crate::listener_of(env, &listener)?
            .ok_or_else(|| Failure::Bad("the stream listener must not be null".into()))?;
        let (playback, capture) = entry
            .dac
            .open_duplex(&format, &request, capacity, listener)?;
        let (playback, capture) = registry::insert_duplex(&entry, playback, capture)?;
        java_string(
            env,
            &serde_json::json!({ "playback": playback, "capture": capture }),
        )
    })
}

#[jni_mangle("dev.usbdac.internal.Native")]
pub fn capture_open<'l>(
    mut env: EnvUnowned<'l>,
    _class: JObject<'l>,
    dac: i64,
    request: JString<'l>,
    capacity_frames: i32,
) -> i64 {
    run(&mut env, |env| {
        let request = capture_json::parse_request(&string_of(env, &request)?)?;
        let capacity = usize::try_from(capacity_frames)
            .ok()
            .filter(|n| *n != 0)
            .ok_or_else(|| Failure::Bad("capture capacity_frames must be positive".into()))?;
        let entry = registry::dac(dac)?;
        let stream = entry.dac.open_capture(&request, capacity)?;
        Ok(registry::insert_capture(&entry, stream)? as i64)
    })
}

#[jni_mangle("dev.usbdac.internal.Native")]
pub fn capture_close<'l>(mut env: EnvUnowned<'l>, _class: JObject<'l>, capture: i64) {
    run(&mut env, |_env| {
        registry::remove_capture(capture)?.close()?;
        Ok(())
    })
}

#[jni_mangle("dev.usbdac.internal.Native")]
pub fn capture_format<'l>(
    mut env: EnvUnowned<'l>,
    _class: JObject<'l>,
    capture: i64,
) -> JString<'l> {
    run(&mut env, |env| {
        java_string(env, &capture_json::format(&*registry::capture(capture)?))
    })
}

#[jni_mangle("dev.usbdac.internal.Native")]
pub fn capture_stats<'l>(
    mut env: EnvUnowned<'l>,
    _class: JObject<'l>,
    capture: i64,
) -> JString<'l> {
    run(&mut env, |env| {
        java_string(env, &capture_json::stats(&*registry::capture(capture)?))
    })
}

/// Fill caller-owned writable direct memory, returning bytes, never partial frames.
#[jni_mangle("dev.usbdac.internal.Native")]
pub fn capture_read<'l>(
    mut env: EnvUnowned<'l>,
    _class: JObject<'l>,
    capture: i64,
    buffer: JByteBuffer<'l>,
    offset: i32,
    count: i32,
) -> i32 {
    run(&mut env, |env| {
        let stream = registry::capture(capture)?;
        if buffer.is_null() {
            return Err(Failure::Bad("capture buffer is null".into()));
        }
        if env
            .call_method(&buffer, jni_str!("isReadOnly"), jni_sig!("()Z"), &[])?
            .z()?
        {
            return Err(Failure::Bad("capture buffer is read-only".into()));
        }
        let offset = usize::try_from(offset)
            .map_err(|_| Failure::Bad("capture offset must be non-negative".into()))?;
        let count = usize::try_from(count)
            .map_err(|_| Failure::Bad("capture count must be non-negative".into()))?;
        let capacity = env
            .get_direct_buffer_capacity(&buffer)
            .map_err(|_| Failure::Bad("capture needs a direct ByteBuffer".into()))?;
        if offset.checked_add(count).is_none_or(|end| end > capacity) {
            return Err(Failure::Bad("capture buffer range exceeds capacity".into()));
        }
        let stride = stream.format().bytes_per_frame();
        if count % stride != 0 {
            return Err(Failure::Bad(
                "capture count must contain whole wire frames".into(),
            ));
        }
        if count == 0 {
            stream.read(&mut [])?;
            return Ok(0);
        }
        let base = env
            .get_direct_buffer_address(&buffer)
            .map_err(|_| Failure::Bad("capture needs a direct ByteBuffer".into()))?;
        if base.is_null() {
            return Err(Failure::Bad("capture buffer has no writable memory".into()));
        }
        // SAFETY: Java keeps this writable direct buffer alive for the call,
        // bounds were checked, and the SDK serializes reads of its stream.
        let bytes = unsafe { std::slice::from_raw_parts_mut(base.add(offset), count) };
        let frames = stream.read(bytes)?;
        Ok((frames * stride) as i32)
    })
}
