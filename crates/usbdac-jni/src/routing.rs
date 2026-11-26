//! Typed, session-bound routing. No raw control request surface.
use super::*;
use serde_json::json;
use usbdac_core::routing::{Access, ControlKey, Entity, RouteEdit, RouteValue, RoutingError, RoutingSessionError, RoutingUnit};

fn key_json(key: ControlKey) -> Value {
    match key {
        ControlKey::Selector(entity) => json!({"kind":"selector", "interface":entity.interface,"entity":entity.id}),
        ControlKey::Crosspoint{entity,input_pin,input_channel,output_channel} => json!({"kind":"crosspoint","interface":entity.interface,"entity":entity.id,"input_pin":input_pin,"input_channel":input_channel,"output_channel":output_channel}),
    }
}
fn value_json(value: RouteValue) -> Value {
    match value {
        RouteValue::SelectorPin(pin) => json!({"kind":"selector_pin","value":pin}),
        RouteValue::MixerGain(gain) => json!({"kind":"mixer_gain","value":gain}),
    }
}
fn byte(value: &Value, field: &str) -> Outcome<u8> {
    value[field].as_u64().and_then(|v| u8::try_from(v).ok()).ok_or_else(|| Failure::Bad(format!("routing {field} must be a byte")))
}
fn parse_key(value: &Value) -> Outcome<ControlKey> {
    let entity = Entity {interface:byte(value,"interface")?,id:byte(value,"entity")?};
    match value["kind"].as_str() {
        Some("selector") => Ok(ControlKey::Selector(entity)),
        Some("crosspoint") => Ok(ControlKey::Crosspoint{entity,input_pin:byte(value,"input_pin")?,input_channel:byte(value,"input_channel")?,output_channel:byte(value,"output_channel")?}),
        _ => Err(Failure::Bad("unknown routing key kind".into())),
    }
}
fn parse_value(value: &Value) -> Outcome<RouteValue> {
    match value["kind"].as_str() {
        Some("selector_pin") => Ok(RouteValue::SelectorPin(byte(value,"value")?)),
        Some("mixer_gain") => value["value"].as_i64().and_then(|v| i16::try_from(v).ok()).map(RouteValue::MixerGain).ok_or_else(|| Failure::Bad("mixer gain must be signed 1/256 dB i16".into())),
        _ => Err(Failure::Bad("unknown routing value kind".into())),
    }
}
fn control_error(error: &RoutingError<String>) -> Value {
    match error {
        RoutingError::Invalid(e) => json!({"kind":"invalid", "detail":format!("{e:?}")}),
        RoutingError::Io(e) => json!({"kind":"io", "detail":e}),
        RoutingError::TransferLength{expected,actual} => json!({"kind":"transfer_length","expected":expected,"actual":actual}),
        RoutingError::ReadbackMismatch{expected,actual} => json!({"kind":"readback_mismatch","expected":value_json(*expected),"actual":value_json(*actual)}),
    }
}
fn failure(error: RoutingSessionError) -> Value {
    let detail = error.to_string();
    let mut value = match error {
        RoutingSessionError::Session(e) => json!({"kind":"session","session_code":e.code()}),
        RoutingSessionError::Invalid(_) => json!({"kind":"invalid"}),
        RoutingSessionError::UnsupportedTopology(_) => json!({"kind":"unsupported_topology"}),
        RoutingSessionError::Drift(_) => json!({"kind":"drift"}),
        RoutingSessionError::Control(e) => json!({"kind":"control","cause":control_error(&e)}),
        RoutingSessionError::Transaction(e) => json!({"kind":"transaction","failed_change":e.failed_change,"cause":control_error(&e.cause),"rollback_failures":e.rollback_failures.iter().map(|(k,e)|json!({"key":key_json(*k),"cause":control_error(e)})).collect::<Vec<_>>()}),
        RoutingSessionError::Scarlett(_) | RoutingSessionError::ScarlettMix(_) => {
            json!({"kind":"scarlett"})
        }
    };
    value["detail"] = Value::from(detail);
    json!({"error":value})
}
fn success(value: Value) -> Value { json!({"value":value}) }
fn snapshot_json(snapshot: &usbdac_core::routing::RoutingSnapshot, id: u64) -> Value {
    let mut controls = Vec::new();
    for unit in snapshot.inventory().units() {
        match unit {
            RoutingUnit::Selector(selector) => {
                let key = ControlKey::Selector(selector.entity());
                controls.push(json!({"key":key_json(key),"access":access(selector.access()),"sources":selector.sources()}));
            },
            RoutingUnit::Mixer(mixer) => {
                for (pin,input) in mixer.inputs().iter().enumerate() {
                    for channel in 1..=input.channels {
                        for output in 1..=mixer.output_channels() {
                            if let Ok(control) = mixer.crosspoint((pin+1) as u8,channel,output) {
                                controls.push(json!({"key":key_json(control.key()),"access":access(control.access()),"sources":[]}));
                            }
                        }
                    }
                }
            },
        }
    }
    json!({"snapshot_id":id,"configuration_id":snapshot.configuration(),"controls":controls})
}
fn access(access: Access) -> &'static str {
    match access {Access::Absent=>"absent",Access::ReadOnly=>"read_only",Access::ReadWrite=>"read_write"}
}
#[jni_mangle("dev.usbdac.internal.Native")]
pub fn routing_discover<'l>(mut env: EnvUnowned<'l>, _class:JObject<'l>, dac:i64) -> JString<'l> {
    run(&mut env, |env| {
        let entry = registry::dac(dac)?;
        let result = match entry.dac.routing_discover() {
            Ok(snapshot) => { let description = snapshot.clone(); let id = entry.remember_routing(snapshot)?; success(snapshot_json(&description,id)) },
            Err(e) => failure(e),
        };
        java_string(env,&result)
    })
}
#[jni_mangle("dev.usbdac.internal.Native")]
pub fn routing_read<'l>(mut env: EnvUnowned<'l>, _class:JObject<'l>, dac:i64, snapshot:i64, key:JString<'l>) -> JString<'l> {
    run(&mut env, |env| {
        let key = serde_json::from_str::<Value>(&string_of(env,&key)?).map_err(|e| Failure::Bad(e.to_string()))?;
        let key = parse_key(&key)?;
        let entry = registry::dac(dac)?;
        let snapshot = entry.routing(snapshot)?;
        let result = match entry.dac.routing_read(&snapshot,key) {Ok(v)=>success(value_json(v)),Err(e)=>failure(e)};
        java_string(env,&result)
    })
}
#[jni_mangle("dev.usbdac.internal.Native")]
pub fn routing_apply<'l>(mut env: EnvUnowned<'l>, _class:JObject<'l>, dac:i64, snapshot:i64, edits:JString<'l>) -> JString<'l> {
    run(&mut env, |env| {
        let edits = serde_json::from_str::<Value>(&string_of(env,&edits)?).map_err(|e| Failure::Bad(e.to_string()))?;
        let edits = edits.as_array().filter(|v| v.len()<=usbdac_core::routing::MAX_ROUTE_CHANGES).ok_or_else(||Failure::Bad("routing edits must be an array of at most 32 changes".into()))?;
        let edits = edits.iter().map(|e|Ok(RouteEdit{key:parse_key(&e["key"])?,expected:parse_value(&e["expected"])?,value:parse_value(&e["value"])?})).collect::<Outcome<Vec<_>>>()?;
        let entry = registry::dac(dac)?;
        let snapshot = entry.routing(snapshot)?;
        let result = match entry.dac.routing_apply(&snapshot,&edits) {
            Ok(v)=>success(json!({"changed":v.changed.into_iter().map(key_json).collect::<Vec<_>>(),"unchanged":v.unchanged.into_iter().map(key_json).collect::<Vec<_>>()})),Err(e)=>failure(e)
        };
        java_string(env,&result)
    })
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn transaction_failure_keeps_edit_index_and_each_rollback_cause() {
        let key = ControlKey::Selector(Entity {interface:3,id:8});
        let value = failure(RoutingSessionError::Transaction(usbdac_core::routing::TransactionError {
            failed_change:Some(2),cause:RoutingError::Io("write".into()),
            rollback_failures:vec![(key,RoutingError::Io("restore".into()))],
        }));
        assert_eq!(value["error"]["kind"],"transaction");
        assert_eq!(value["error"]["failed_change"],2);
        assert_eq!(value["error"]["rollback_failures"][0]["key"],key_json(key));
        assert_eq!(value["error"]["rollback_failures"][0]["cause"]["detail"],"restore");
    }
    #[test]
    fn keys_values_and_errors_preserve_types_and_bounds() {
        let key=ControlKey::Crosspoint{entity:Entity{interface:2,id:9},input_pin:1,input_channel:2,output_channel:3};
        assert_eq!(parse_key(&key_json(key)).ok(),Some(key));
        assert!(parse_key(&json!({"kind":"selector","interface":256,"entity":1})).is_err());
        for value in [RouteValue::SelectorPin(2),RouteValue::MixerGain(i16::MIN),RouteValue::MixerGain(-256)] {
            assert_eq!(parse_value(&value_json(value)).ok(),Some(value));
        }
        assert!(parse_value(&json!({"kind":"mixer_gain","value":32768})).is_err());
        let error=control_error(&RoutingError::ReadbackMismatch{expected:RouteValue::SelectorPin(1),actual:RouteValue::SelectorPin(2)});
        assert_eq!(error["expected"]["value"],1);
        assert_eq!(error["actual"]["value"],2);
    }
}
