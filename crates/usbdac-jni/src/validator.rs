//! Opt-in bridge used only by the in-repository Android validator.
use crate::{registry, run, string_of, Failure};
use jni::{
    jni_mangle,
    objects::{JObject, JString},
    EnvUnowned,
};
use once_cell::sync::Lazy;
use parking_lot::Mutex;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use usbdac_validator::{Capture, RunOptions};

static CANCEL: AtomicBool = AtomicBool::new(false);
static DEVICE_LOST: AtomicBool = AtomicBool::new(false);
static CAPTURE: Lazy<Mutex<Option<Arc<Capture>>>> = Lazy::new(|| Mutex::new(None));
struct CaptureLogger;
static LOGGER: CaptureLogger = CaptureLogger;
impl log::Log for CaptureLogger {
    fn enabled(&self, metadata: &log::Metadata<'_>) -> bool {
        metadata.level() <= log::Level::Debug
    }
    fn log(&self, record: &log::Record<'_>) {
        if self.enabled(record.metadata()) {
            if let Some(capture) = CAPTURE.lock().as_ref() {
                capture.record("driver_log", serde_json::json!({"level": record.level().to_string(), "target": record.target(), "message": record.args().to_string()}));
            }
        }
    }
    fn flush(&self) {}
}
#[jni_mangle("dev.usbdac.internal.Native")]
pub fn validator_begin<'l>(mut env: EnvUnowned<'l>, _class: JObject<'l>, directory: JString<'l>) {
    run(&mut env, |env| {
        let dir = string_of(env, &directory)?;
        let capture = Capture::new(dir).map_err(|e| Failure::Bad(e.to_string()))?;
        *CAPTURE.lock() = Some(Arc::new(capture));
        CANCEL.store(false, Ordering::SeqCst);
        DEVICE_LOST.store(false, Ordering::SeqCst);
        let _ = log::set_logger(&LOGGER);
        log::set_max_level(log::LevelFilter::Debug);
        Ok(())
    })
}
#[jni_mangle("dev.usbdac.internal.Native")]
pub fn validator_cancel<'l>(_env: EnvUnowned<'l>, _class: JObject<'l>) {
    CANCEL.store(true, Ordering::SeqCst);
}
#[jni_mangle("dev.usbdac.internal.Native")]
pub fn validator_device_lost<'l>(_env: EnvUnowned<'l>, _class: JObject<'l>) {
    DEVICE_LOST.store(true, Ordering::SeqCst);
}
#[jni_mangle("dev.usbdac.internal.Native")]
pub fn validator_run<'l>(
    mut env: EnvUnowned<'l>,
    _class: JObject<'l>,
    dac: i64,
    directory: JString<'l>,
) -> JString<'l> {
    run(&mut env, |env| {
        let dir = string_of(env, &directory)?;
        let capture = CAPTURE
            .lock()
            .clone()
            .ok_or_else(|| Failure::Bad("capture not started".into()))?;
        let entry = registry::dac(dac)?;
        let report = capture.run_with_device_loss(
            &entry.dac,
            RunOptions::default(),
            &CANCEL,
            &DEVICE_LOST,
            |progress| {
                if let Ok(json) = serde_json::to_vec(progress) {
                    let _ = std::fs::write(std::path::Path::new(&dir).join("progress.json"), json);
                }
            },
        );
        let json = serde_json::to_string(&report).map_err(|e| Failure::Bad(e.to_string()))?;
        Ok(JString::from_str(env, json)?)
    })
}

#[jni_mangle("dev.usbdac.internal.Native")]
pub fn validator_flush<'l>(mut env: EnvUnowned<'l>, _class: JObject<'l>) {
    run(&mut env, |_env| {
        let capture = CAPTURE
            .lock()
            .clone()
            .ok_or_else(|| Failure::Bad("capture not started".into()))?;
        capture
            .flush_debug()
            .map_err(|e| Failure::Bad(e.to_string()))?;
        Ok(())
    })
}
