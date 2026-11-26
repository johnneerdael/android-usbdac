//! The event bridge: native events reach the registered Java listener's
//! `onEvent(type: Int, a: Long, b: Double, message: String?)` on the library's
//! event thread, which is attached to the JVM on first use.

use crate::json;
use jni::objects::{JObject, JString, JValue};
use jni::{jni_sig, jni_str, JavaVM};
use usbdac_core::{DacEvent, DacListener};

pub struct JavaListener {
    target: jni::objects::Global<JObject<'static>>,
}

impl JavaListener {
    pub fn new(target: jni::objects::Global<JObject<'static>>) -> Self {
        Self { target }
    }
}

impl DacListener for JavaListener {
    fn on_event(&self, event: DacEvent) {
        let (kind, a, b, message) = json::event(&event);
        if kind < 0 {
            return;
        }
        let Ok(vm) = JavaVM::singleton() else {
            log::warn!("no JavaVM; dropping event {event:?}");
            return;
        };
        let result = vm.attach_current_thread(|env| -> jni::errors::Result<()> {
            let message: JObject = match &message {
                Some(text) => JString::from_str(env, text)?.into(),
                None => JObject::null(),
            };
            env.call_method(
                &self.target,
                jni_str!("onEvent"),
                jni_sig!("(IJDLjava/lang/String;)V"),
                &[
                    JValue::Int(kind),
                    JValue::Long(a),
                    JValue::Double(b),
                    JValue::Object(&message),
                ],
            )?;
            Ok(())
        });
        if let Err(error) = result {
            log::warn!("the Java listener failed on {event:?}: {error}");
        }
    }
}
