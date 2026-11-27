use jni::errors::ThrowRuntimeExAndDefault;
use jni::objects::{JByteBuffer, JObject};
use jni::{jni_mangle, Env, EnvUnowned};
use once_cell::sync::Lazy;
use parking_lot::Mutex;
use std::{collections::HashMap, sync::Arc};
use usbdac_core::{
    convert::{ConvertError, ConvertOptions, Narrowing, PcmFormat, Quality, SampleFormat},
    dsd_pcm::DsdPcmConverter,
};

static CONVERTERS: Lazy<Mutex<HashMap<u64, Arc<Mutex<DsdPcmConverter>>>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));
fn run<'l, T: Default>(
    env: &mut EnvUnowned<'l>,
    f: impl FnOnce(&mut Env<'l>) -> Result<T, ConvertError>,
) -> T {
    env.with_env(|env| match f(env) {
        Ok(v) => Ok(v),
        Err(e) => {
            crate::throw(env, e.code(), e.message())?;
            Err(jni::errors::Error::JavaException)
        }
    })
    .resolve::<ThrowRuntimeExAndDefault>()
}
fn bad(s: &str) -> ConvertError {
    ConvertError::InvalidArgument(s.into())
}
fn get(id: i64) -> Result<Arc<Mutex<DsdPcmConverter>>, ConvertError> {
    u64::try_from(id)
        .ok()
        .and_then(|id| CONVERTERS.lock().get(&id).cloned())
        .ok_or_else(|| bad("invalid DSD converter handle"))
}
fn range(
    env: &Env<'_>,
    b: &JByteBuffer<'_>,
    offset: i32,
    len: i32,
) -> Result<(*mut u8, usize), ConvertError> {
    let (o, n) = (
        usize::try_from(offset).map_err(|_| bad("negative offset"))?,
        usize::try_from(len).map_err(|_| bad("negative length"))?,
    );
    let cap = env
        .get_direct_buffer_capacity(b)
        .map_err(|_| bad("direct buffer required"))?;
    if o.checked_add(n).is_none_or(|end| end > cap) {
        return Err(bad("buffer range out of bounds"));
    }
    let p = env
        .get_direct_buffer_address(b)
        .map_err(|_| bad("direct buffer required"))?;
    Ok((unsafe { p.add(o) }, n))
}
#[jni_mangle("dev.usbdac.internal.DsdPcmNative")]
pub fn create<'l>(
    mut env: EnvUnowned<'l>,
    _: JObject<'l>,
    rate: i32,
    channels: i32,
    target_rate: i32,
    target_channels: i32,
    bits: i32,
    slot: i32,
    quality: i32,
    narrowing: i32,
    headroom: f64,
    seed: i64,
) -> i64 {
    run(&mut env, |_| {
        let rate = u32::try_from(rate).map_err(|_| bad("invalid rate"))?;
        let channels = u16::try_from(channels).map_err(|_| bad("invalid channels"))?;
        let target_rate = u32::try_from(target_rate).map_err(|_| bad("invalid target rate"))?;
        let target_channels =
            u16::try_from(target_channels).map_err(|_| bad("invalid target channels"))?;
        let sample = if bits == 0 && slot == 4 {
            SampleFormat::Float32
        } else {
            SampleFormat::int(
                u8::try_from(bits).map_err(|_| bad("invalid bits"))?,
                u8::try_from(slot).map_err(|_| bad("invalid slot"))?,
            )?
        };
        let mut options = ConvertOptions {
            quality: match quality {
                0 => Quality::High,
                1 => Quality::Fast,
                _ => return Err(bad("invalid quality")),
            },
            narrowing: match narrowing {
                0 => Narrowing::TpdfDither,
                1 => Narrowing::Truncate,
                _ => return Err(bad("invalid narrowing")),
            },
            headroom_db: headroom,
            ..ConvertOptions::default()
        };
        if seed >= 0 {
            options.dither_seed = seed as u64;
        }
        let c = DsdPcmConverter::with_options(
            rate,
            channels,
            PcmFormat::new(target_rate, target_channels, sample),
            &options,
        )?;
        let id = crate::registry::next_id();
        CONVERTERS.lock().insert(id, Arc::new(Mutex::new(c)));
        Ok(id as i64)
    })
}
#[jni_mangle("dev.usbdac.internal.DsdPcmNative")]
pub fn process<'l>(
    mut env: EnvUnowned<'l>,
    _: JObject<'l>,
    id: i64,
    input: JByteBuffer<'l>,
    off: i32,
    len: i32,
    output: JByteBuffer<'l>,
    outoff: i32,
    outlen: i32,
) -> i64 {
    run(&mut env, |env| {
        let (a, n) = range(env, &input, off, len)?;
        let (b, m) = range(env, &output, outoff, outlen)?;
        if (a as usize) < (b as usize).saturating_add(m)
            && (b as usize) < (a as usize).saturating_add(n)
        {
            return Err(bad("overlapping buffers"));
        }
        let done = get(id)?
            .lock()
            .process(unsafe { std::slice::from_raw_parts(a, n) }, unsafe {
                std::slice::from_raw_parts_mut(b, m)
            })?;
        Ok(((done.consumed as i64) << 32) | done.produced as i64)
    })
}
#[jni_mangle("dev.usbdac.internal.DsdPcmNative")]
pub fn finish<'l>(
    mut env: EnvUnowned<'l>,
    _: JObject<'l>,
    id: i64,
    output: JByteBuffer<'l>,
    off: i32,
    len: i32,
) -> i32 {
    run(&mut env, |env| {
        let (p, n) = range(env, &output, off, len)?;
        Ok(get(id)?
            .lock()
            .finish(unsafe { std::slice::from_raw_parts_mut(p, n) })? as i32)
    })
}
#[jni_mangle("dev.usbdac.internal.DsdPcmNative")]
pub fn reset<'l>(mut env: EnvUnowned<'l>, _: JObject<'l>, id: i64) {
    run(&mut env, |_| {
        get(id)?.lock().reset();
        Ok(())
    })
}
#[jni_mangle("dev.usbdac.internal.DsdPcmNative")]
pub fn close<'l>(mut env: EnvUnowned<'l>, _: JObject<'l>, id: i64) {
    run(&mut env, |_| {
        u64::try_from(id)
            .ok()
            .and_then(|id| CONVERTERS.lock().remove(&id))
            .ok_or_else(|| bad("invalid DSD converter handle"))?;
        Ok(())
    })
}

#[jni_mangle("dev.usbdac.internal.DsdPcmNative")]
pub fn delay<'l>(mut env: EnvUnowned<'l>, _: JObject<'l>, id: i64) -> i32 {
    run(&mut env, |_| {
        Ok(get(id)?.lock().delay_byte_frames().min(i32::MAX as usize) as i32)
    })
}

#[jni_mangle("dev.usbdac.internal.DsdPcmNative")]
pub fn clipped<'l>(mut env: EnvUnowned<'l>, _: JObject<'l>, id: i64) -> i64 {
    run(&mut env, |_| {
        Ok(get(id)?.lock().clipped_samples().min(i64::MAX as u64) as i64)
    })
}
