//! Typed Pioneer DJM routing bridge. No raw USB selectors or inferred current state.
use super::*;
use serde_json::json;
use usbdac_core::pioneer::{DjmControlKey, DjmModel, DjmRoutingProfile};

fn key_json(key: DjmControlKey) -> Value {
    match key {
        DjmControlKey::CaptureLevel => json!({"kind":"capture_level"}),
        DjmControlKey::CaptureSource(pair) => json!({"kind":"capture_source","pair":pair}),
        DjmControlKey::PlaybackDestination(pair) => json!({"kind":"playback_destination","pair":pair}),
    }
}
fn profile_json(profile: &DjmRoutingProfile) -> Value {
    json!({"model":profile.model.key(),"configuration_id":profile.configuration,
        "controls":profile.controls.iter().map(|control|json!({
            "key":key_json(control.key),"options":control.options.iter().map(|option|
                json!({"id":option.id,"label":option.label})).collect::<Vec<_>>()
        })).collect::<Vec<_>>()})
}
fn fields(value: &Value, allowed: &[&str]) -> Outcome<()> {
    if !value.as_object().is_some_and(|v|v.len()==allowed.len() && v.keys().all(|k|allowed.contains(&k.as_str()))) {
        return Err(Failure::Bad("Pioneer DJM request has invalid fields".into()));
    }
    Ok(())
}
fn byte(value: &Value, name: &str) -> Outcome<u8> {
    value[name].as_u64().and_then(|n|u8::try_from(n).ok())
        .ok_or_else(||Failure::Bad(format!("Pioneer DJM {name} must be a byte")))
}
fn parse_key(value: &Value) -> Outcome<DjmControlKey> {
    match value["kind"].as_str() {
        Some("capture_level") => {
            fields(value,&["kind"])?;
            Ok(DjmControlKey::CaptureLevel)
        },
        Some(kind @ ("capture_source" | "playback_destination")) => {
            fields(value,&["kind","pair"])?;
            let pair=byte(value,"pair")?;
            if pair==0 { return Err(Failure::Bad("Pioneer DJM pairs are one-based".into())); }
            Ok(if kind=="capture_source" {DjmControlKey::CaptureSource(pair)} else {DjmControlKey::PlaybackDestination(pair)})
        },
        _ => Err(Failure::Bad("Unknown Pioneer DJM control kind".into())),
    }
}
fn parse_edit(value: &Value) -> Outcome<(DjmModel,u8,DjmControlKey,u8)> {
    fields(value,&["model","configuration_id","key","option_id"])?;
    let model=value["model"].as_str().and_then(DjmModel::from_key)
        .ok_or_else(||Failure::Bad("Unknown Pioneer DJM model".into()))?;
    let configuration=byte(value,"configuration_id")?;
    if configuration==0 { return Err(Failure::Bad("Pioneer DJM configuration is one-based".into())); }
    Ok((model,configuration,parse_key(&value["key"])?,byte(value,"option_id")?))
}
#[jni_mangle("dev.usbdac.internal.Native")]
pub fn pioneer_djm_discover<'l>(mut env:EnvUnowned<'l>,_class:JObject<'l>,dac:i64)->JString<'l> {
    run(&mut env,|env| {
        let entry=registry::dac(dac)?;
        let result=match entry.dac.djm_routing_discover() {
            Ok(profile)=>json!({"value":profile_json(&profile)}),
            Err(error)=>routing::failure(error),
        };
        java_string(env,&result)
    })
}
#[jni_mangle("dev.usbdac.internal.Native")]
pub fn pioneer_djm_apply<'l>(mut env:EnvUnowned<'l>,_class:JObject<'l>,dac:i64,edit:JString<'l>)->JString<'l> {
    run(&mut env,|env| {
        let input=string_of(env,&edit)?;
        if input.len()>1024 { return Err(Failure::Bad("Pioneer DJM edit exceeds 1024 bytes".into())); }
        let value=serde_json::from_str::<Value>(&input).map_err(|e|Failure::Bad(e.to_string()))?;
        let (model,configuration,key,option)=parse_edit(&value)?;
        let entry=registry::dac(dac)?;
        let result=match entry.dac.djm_routing_apply(model,configuration,key,option) {
            Ok(())=>json!({"value":{}}),
            Err(error)=>routing::failure(error),
        };
        java_string(env,&result)
    })
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn typed_keys_preserve_one_based_pairs_and_level_has_no_pair() {
        for key in [DjmControlKey::CaptureLevel,DjmControlKey::CaptureSource(1),DjmControlKey::PlaybackDestination(6)] {
            assert_eq!(parse_key(&key_json(key)).ok(),Some(key));
        }
        for value in [json!({"kind":"capture_level","pair":0}),json!({"kind":"capture_level","pair":null}),
            json!({"kind":"capture_source","pair":0}),json!({"kind":"capture_source","pair":256}),
            json!({"kind":"capture_source","pair":1.5}),json!({"kind":"capture_source","pair":-1}),
            json!({"kind":"selector","pair":1}),json!({"kind":"playback_destination","pair":1,"selector":2})] {
            assert!(parse_key(&value).is_err(),"accepted {value}");
        }
    }
    #[test]
    fn profiles_and_edits_keep_model_scoped_option_ordinals_and_labels() {
        for model in [DjmModel::Djm250Mk2,DjmModel::Djm450,DjmModel::Djm750,DjmModel::Djm850,
            DjmModel::Djm900Nxs2,DjmModel::Djm750Mk2,DjmModel::DjmV10] {
            let profile=DjmRoutingProfile {model,configuration:1,controls:model.controls()};
            let value=profile_json(&profile);
            assert_eq!(value["model"],model.key());
            assert!(value.get("current").is_none() && value.get("default").is_none());
            for (index,control) in profile.controls.iter().enumerate() {
                assert_eq!(value["controls"][index]["key"],key_json(control.key));
                for (option_index,option) in control.options.iter().enumerate() {
                    assert_eq!(value["controls"][index]["options"][option_index],json!({"id":option.id,"label":option.label}));
                    let edit=json!({"model":model.key(),"configuration_id":1,"key":key_json(control.key),"option_id":option.id});
                    assert_eq!(parse_edit(&edit).ok(),Some((model,1,control.key,option.id)));
                }
            }
        }
    }
    #[test]
    fn edit_rejects_unknown_identity_indices_and_raw_request_fields() {
        let edit=json!({"model":"unknown","configuration_id":1,"key":{"kind":"capture_level"},"option_id":0});
        assert!(parse_edit(&edit).is_err());
        // Exact field admission rejects raw transport parameters even before model lookup.
        let mut raw=edit;
        raw["request"]=json!(1);
        assert!(parse_edit(&raw).is_err());
        for value in [json!(-1),json!(256),json!(0.5),json!("1")] {
            assert!(byte(&json!({"option_id":value}),"option_id").is_err());
        }
    }
}
