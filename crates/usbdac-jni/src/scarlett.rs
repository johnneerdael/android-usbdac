//! Typed Scarlett bridge; no raw requests and no caller-supplied model proof.
use super::*;
use serde_json::json;
use usbdac_core::scarlett::*;

fn capability(v: ReadCapability) -> Value {
    json!({"model":v.candidate.key(),"name":v.candidate.name(),"configuration_id":v.configuration,"interface":v.interface,"notification_endpoint":v.notification_endpoint,"interface_claimed":v.interface_claimed})
}
fn port(v: Port) -> Value {
    match v { Port::Off => json!({"kind":"off"}), Port::Channel {kind,channel} => json!({"kind":match kind {
        PortKind::Off=>"off",PortKind::Analogue=>"analogue",PortKind::Spdif=>"spdif",PortKind::Adat=>"adat",PortKind::Mix=>"mix",PortKind::Pcm=>"pcm"},"channel":channel}) }
}
fn row(v: &MixReadback) -> Value { json!({"output":v.output,"coefficients":v.coefficients}) }
fn fcp_readback(v: &FcpReadback) -> Value {
    let port = |p: &FcpPort| json!({"pin":p.pin,"name":p.name,"fixed_source":p.fixed_source});
    json!({"model":v.model.key(),"firmware":v.firmware,
        "mixer_inputs":v.mixer_inputs,"mixer_outputs":v.mixer_outputs,
        "sources":v.sources.iter().map(port).collect::<Vec<_>>(),
        "destinations":v.destinations.iter().map(port).collect::<Vec<_>>(),
        "banks":v.banks.iter().map(|b|json!({"bank":b.bank,"raw_words":b.raw_words})).collect::<Vec<_>>(),
        "mixes":v.mixes.iter().map(row).collect::<Vec<_>>()})
}
fn read_error(v: &ReadError<String>) -> Value {
    match v { ReadError::Io(detail)=>json!({"kind":"io","detail":detail}),ReadError::Protocol(e)=>json!({"kind":"protocol","detail":format!("{e:?}")}) }
}
fn mix_failure(v: &MixFailure<String>) -> Value {
    match v {MixFailure::Read(e)=>read_error(e),MixFailure::Invalid(e)=>json!({"kind":"invalid","detail":format!("{e:?}")}),MixFailure::Drift{actual}=>json!({"kind":"drift","actual":row(actual)}),MixFailure::Verification{actual}=>json!({"kind":"verification","actual":row(actual)})}
}
fn mux_bank(v: &MuxBankSnapshot) -> Value {
    json!({"bank":v.bank(),"raw_words":v.raw_words(),"routes":v.routes().iter().map(|r| json!({"source":port(r.source),"destination":port(r.destination)})).collect::<Vec<_>>()})
}
fn mux_state(v: &MuxSnapshot) -> Value {
    json!({"model":v.model.candidate().key(),"firmware":v.model.firmware(),"banks":v.banks.iter().map(mux_bank).collect::<Vec<_>>()})
}
fn mux_failure(v: &MuxFailure<String>) -> Value {
    match v {
        MuxFailure::Read(e) => read_error(e),
        MuxFailure::Invalid(e) => json!({"kind":"invalid","detail":format!("{e:?}")}),
        MuxFailure::Drift{actual} => json!({"kind":"drift","actual":mux_state(actual)}),
        MuxFailure::Verification{bank,actual} => json!({"kind":"verification","bank":bank,"actual_bank":mux_bank(actual)}),
    }
}
fn fcp_target(v: &FcpTarget) -> Value {
    match v { FcpTarget::Bank(bank) => json!({"kind":"bank","index":bank}),
        FcpTarget::Mix(output) => json!({"kind":"mix","index":output}) }
}
fn fcp_failure(v: &FcpFailure<String>) -> Value {
    match v {
        FcpFailure::Read(e) => read_error(e),
        FcpFailure::Invalid(e) => json!({"kind":"invalid","detail":format!("{e:?}")}),
        FcpFailure::Drift => json!({"kind":"drift"}),
        FcpFailure::Verification(target) => json!({"kind":"verification","target":fcp_target(target)}),
    }
}
fn gen1_key(key: Gen1Key) -> Value {
    match key {
        Gen1Key::MatrixInput(index) => json!({"kind":"matrix_input","index":index}),
        Gen1Key::Output(index) => json!({"kind":"output","index":index}),
        Gen1Key::Capture(index) => json!({"kind":"capture","index":index}),
        Gen1Key::Gain { input, mix } => json!({"kind":"gain","input":input,"mix":mix}),
    }
}
fn gen1_readback(v: &Gen1Readback) -> Value {
    json!({"model":v.model.key(),"controls":v.controls.iter().map(|c|json!({"key":gen1_key(c.key),"value":c.value})).collect::<Vec<_>>(),
        "source_offsets":v.source_offsets,"matrix_sources":v.matrix_sources,"output_sources":v.output_sources})
}
fn gen1_failure(v: &Gen1Failure<String>) -> Value {
    match v {
        Gen1Failure::Io { key, error } => json!({"kind":"io","key":gen1_key(*key),"detail":error}),
        Gen1Failure::Invalid { key } => json!({"kind":"invalid","key":gen1_key(*key)}),
        Gen1Failure::Drift { key } => json!({"kind":"drift","key":key.map(gen1_key)}),
        Gen1Failure::Verification { key } => json!({"kind":"verification","key":gen1_key(*key)}),
    }
}
fn failure(v: usbdac_core::routing::RoutingSessionError) -> Value {
    use usbdac_core::routing::RoutingSessionError as E;
    let detail=v.to_string();
    let error=match v {
        E::Gen1Read(e)=>json!({"kind":"gen1","cause":gen1_failure(&e),"rollback_failures":[],"detail":detail}),
        E::Gen1(e)=>json!({"kind":"gen1","cause":gen1_failure(&e.failure),"rollback_failures":e.rollback.iter().map(|(key,cause)|json!({"key":gen1_key(*key),"cause":gen1_failure(cause)})).collect::<Vec<_>>(),"detail":detail}),
        E::Fcp(e)=>json!({"kind":"fcp","cause":fcp_failure(&e.failure),"rollback_failures":e.rollback.iter().map(|(target,cause)|json!({"target":fcp_target(target),"cause":fcp_failure(cause)})).collect::<Vec<_>>(),"detail":detail}),
        E::Scarlett(e)=>json!({"kind":"read","cause":read_error(&e),"detail":detail}),
        E::ScarlettMux(e)=>json!({"kind":"mux","cause":mux_failure(&e.failure),"rollback_failures":e.rollback.iter().map(|(bank,cause)|json!({"bank":bank,"cause":mux_failure(cause)})).collect::<Vec<_>>(),"detail":detail}),
        E::ScarlettMix(e)=>json!({"kind":"mix","cause":mix_failure(&e.failure),"rollback_failure":e.rollback.as_ref().map(mix_failure),"detail":detail}),
        E::Session(e)=>json!({"kind":"session","session_code":e.code(),"detail":detail}),
        E::Drift(_)=>json!({"kind":"drift","detail":detail}),
        _=>json!({"kind":"unsupported","detail":detail}),
    };
    json!({"error":error})
}
fn byte(v:&Value, key:&str)->Outcome<u8> { v[key].as_u64().and_then(|n|u8::try_from(n).ok()).ok_or_else(||Failure::Bad(format!("Scarlett {key} must be a byte"))) }
fn parse_edit(v: &Value) -> Outcome<MixRowEdit> {
    let expected=&v["expected"];
    let coefficients=expected["coefficients"].as_array().filter(|a|!a.is_empty() && a.len()<=25).ok_or_else(||Failure::Bad("Scarlett expected row must contain 1 to 25 raw coefficients; native model validation requires the exact width".into()))?
        .iter().map(|n|n.as_u64().and_then(|n|u16::try_from(n).ok()).ok_or_else(||Failure::Bad("Scarlett coefficients must be unsigned 16-bit".into()))).collect::<Outcome<Vec<_>>>()?;
    let gains=v["gains"].as_array().filter(|a|!a.is_empty() && a.len()<=25).ok_or_else(||Failure::Bad("Scarlett requires 1 to 25 gain edits".into()))?
        .iter().map(|g| {let gain=g["half_db"].as_i64().and_then(|n|i16::try_from(n).ok()).and_then(|n|MixGain::from_half_db(n).ok()).ok_or_else(||Failure::Bad("Scarlett gain must be -160 to 24 half-dB".into()))?;Ok(MixGainEdit{input:byte(g,"input")?,gain})}).collect::<Outcome<Vec<_>>>()?;
    Ok(MixRowEdit{expected:MixReadback{output:byte(expected,"output")?,coefficients},gains})
}
fn parse_port(v: &Value) -> Outcome<Port> {
    if !v.as_object().is_some_and(|v| v.keys().all(|k| matches!(k.as_str(), "kind" | "channel"))) {
        return Err(Failure::Bad("Scarlett ports contain only kind and channel".into()));
    }
    let kind = match v["kind"].as_str() {
        Some("off") if v.get("channel").is_none() => return Ok(Port::Off),
        Some("analogue") => PortKind::Analogue,
        Some("spdif") => PortKind::Spdif,
        Some("adat") => PortKind::Adat,
        Some("mix") => PortKind::Mix,
        Some("pcm") => PortKind::Pcm,
        _ => return Err(Failure::Bad("unknown Scarlett port kind or OFF channel".into())),
    };
    let channel = byte(v, "channel")?;
    if channel == 0 { return Err(Failure::Bad("Scarlett channels are one-based".into())); }
    Ok(Port::Channel { kind, channel })
}
fn parse_mux_routes(v: &Value) -> Outcome<Vec<MuxRouteEdit>> {
    let edits = v.as_array().filter(|a| !a.is_empty() && a.len() <= 256)
        .ok_or_else(|| Failure::Bad("Scarlett mux requires 1 to 256 typed route edits".into()))?;
    edits.iter().map(|v| {
        if !v.as_object().is_some_and(|v| v.keys().all(|k| matches!(k.as_str(), "bank" | "destination" | "source"))) {
            return Err(Failure::Bad("Scarlett mux edits contain only bank, destination and source".into()));
        }
        let bank = byte(v, "bank")?;
        if bank > 2 { return Err(Failure::Bad("Scarlett mux bank must be 0 to 2".into())); }
        Ok(MuxRouteEdit { bank, destination:parse_port(&v["destination"])?, source:parse_port(&v["source"])? })
    }).collect()
}
#[jni_mangle("dev.usbdac.internal.Native")]
pub fn scarlett_mux_read<'l>(mut env:EnvUnowned<'l>,_class:JObject<'l>,dac:i64)->JString<'l> {
    run(&mut env,|env| {
        let entry=registry::dac(dac)?;
        let result=match entry.dac.scarlett_mux_read() {
            Ok(snapshot)=>{ let mut v=mux_state(&snapshot); v["snapshot_id"]=json!(entry.remember_scarlett_mux(snapshot)?); json!({"value":v}) },
            Err(e)=>failure(e),
        };
        java_string(env,&result)
    })
}
#[jni_mangle("dev.usbdac.internal.Native")]
pub fn scarlett_mux_apply<'l>(mut env:EnvUnowned<'l>,_class:JObject<'l>,dac:i64,snapshot:i64,edits:JString<'l>)->JString<'l> {
    run(&mut env,|env| {
        let input=string_of(env,&edits)?;
        if input.len()>65536 { return Err(Failure::Bad("Scarlett mux edits exceed 65536 bytes".into())); }
        let routes=parse_mux_routes(&serde_json::from_str::<Value>(&input).map_err(|e|Failure::Bad(e.to_string()))?)?;
        let entry=registry::dac(dac)?;
        let expected=entry.scarlett_mux(snapshot)?;
        let result=match entry.dac.scarlett_mux_apply(&MuxEdit{expected,routes}) {
            Ok(report)=>{
                let original=mux_state(&report.original);
                let mut verified=mux_state(&report.verified);
                verified["snapshot_id"]=json!(entry.remember_scarlett_mux(report.verified)?);
                json!({"value":{"original":original,"verified":verified}})
            },
            Err(e)=>failure(e),
        };
        java_string(env,&result)
    })
}
#[jni_mangle("dev.usbdac.internal.Native")]
pub fn scarlett_preflight<'l>(mut env:EnvUnowned<'l>,_class:JObject<'l>,fd:i32,info:JString<'l>)->JString<'l> {
    run(&mut env,|env| {let info=json::parse_device_info(&string_of(env,&info)?)?;let v=Dac::scarlett_routing_preflight(fd,&info)?;java_string(env,&json!({"capability":v.map(capability)}))})
}
#[jni_mangle("dev.usbdac.internal.Native")]
pub fn scarlett_capability<'l>(mut env:EnvUnowned<'l>,_class:JObject<'l>,dac:i64)->JString<'l> {
    run(&mut env,|env| {let e=registry::dac(dac)?;let v=match e.dac.scarlett_routing_capability(){Ok(v)=>json!({"value":{"capability":v.map(capability)}}),Err(e)=>failure(e)};java_string(env,&v)})
}
#[jni_mangle("dev.usbdac.internal.Native")]
pub fn scarlett_read<'l>(mut env:EnvUnowned<'l>,_class:JObject<'l>,dac:i64)->JString<'l> {
    run(&mut env,|env| {let e=registry::dac(dac)?;let v=match e.dac.scarlett_routing_read(){Ok(v)=>json!({"value":{"model":v.model.candidate().key(),"firmware":v.model.firmware(),"mixer_inputs":v.model.mixer_inputs(),"mixer_outputs":v.model.mixer_outputs(),"mux_zero":v.mux_zero.iter().map(|r|json!({"source":port(r.source),"destination":port(r.destination)})).collect::<Vec<_>>(),"mixes":v.mixes.iter().map(row).collect::<Vec<_>>()}}),Err(e)=>failure(e)};java_string(env,&v)})
}
fn fcp_read_value(dac: i64, remember: bool) -> Outcome<Value> {
    let entry=registry::dac(dac)?;
    Ok(match entry.dac.fcp_routing_read() {
        Ok(v)=>{
            let mut value=fcp_readback(&v);
            if remember { value["snapshot_id"]=json!(entry.remember_fcp(v)?); }
            json!({"value":value})
        }, Err(e)=>failure(e),
    })
}
/// Stateless polling never consumes or evicts native edit baselines.
#[jni_mangle("dev.usbdac.internal.Native")]
pub fn fcp_read<'l>(mut env:EnvUnowned<'l>,_class:JObject<'l>,dac:i64)->JString<'l> {
    run(&mut env,|env| java_string(env,&fcp_read_value(dac,false)?))
}
#[jni_mangle("dev.usbdac.internal.Native")]
pub fn fcp_read_snapshot<'l>(mut env:EnvUnowned<'l>,_class:JObject<'l>,dac:i64)->JString<'l> {
    run(&mut env,|env| java_string(env,&fcp_read_value(dac,true)?))
}
fn parse_gen1_key(v: &Value) -> Outcome<Gen1Key> {
    let fields: &[&str] = if v["kind"] == "gain" { &["kind", "input", "mix"] } else { &["kind", "index"] };
    if !v.as_object().is_some_and(|v|v.len()==fields.len() && v.keys().all(|k|fields.contains(&k.as_str()))) {
        return Err(Failure::Bad("First-generation control has invalid key fields".into()));
    }
    let coordinate = |name| { let value=byte(v,name)?; if value==0 { Err(Failure::Bad("First-generation coordinates are one-based".into())) } else { Ok(value) } };
    Ok(match v["kind"].as_str() {
        Some("matrix_input") => Gen1Key::MatrixInput(coordinate("index")?),
        Some("output") => Gen1Key::Output(coordinate("index")?),
        Some("capture") => Gen1Key::Capture(coordinate("index")?),
        Some("gain") => Gen1Key::Gain {input:coordinate("input")?,mix:coordinate("mix")?},
        _ => return Err(Failure::Bad("Unknown first-generation control kind".into())),
    })
}
fn parse_gen1_edits(v: &Value) -> Outcome<Vec<Gen1Edit>> {
    let edits=v.as_array().filter(|a|!a.is_empty() && a.len()<=4096)
        .ok_or_else(||Failure::Bad("First-generation routing requires 1 to 4096 typed edits".into()))?;
    let mut keys=Vec::new();
    edits.iter().map(|v| {
        if !v.as_object().is_some_and(|v|v.len()==2 && v.keys().all(|k|matches!(k.as_str(),"key"|"value"))) {
            return Err(Failure::Bad("First-generation edits contain only key and value".into()));
        }
        let key=parse_gen1_key(&v["key"])?;
        let value=v["value"].as_i64().and_then(|v|i16::try_from(v).ok())
            .ok_or_else(||Failure::Bad("First-generation value must be signed 16-bit".into()))?;
        if !matches!(key,Gen1Key::Gain {..}) && !(-1..=255).contains(&value) {
            return Err(Failure::Bad("First-generation source must be -1 (Off) to 255".into()));
        }
        if keys.contains(&key) { return Err(Failure::Bad("Duplicate first-generation edit key".into())); }
        keys.push(key);
        Ok(Gen1Edit {key,value})
    }).collect()
}
fn gen1_read_value(dac: i64, remember: bool) -> Outcome<Value> {
    let entry=registry::dac(dac)?;
    Ok(match entry.dac.scarlett_gen1_read() {
        Ok(v)=>{
            let mut value=gen1_readback(&v);
            if remember { value["snapshot_id"]=json!(entry.remember_gen1(v)?); }
            json!({"value":value})
        }, Err(e)=>failure(e),
    })
}
#[jni_mangle("dev.usbdac.internal.Native")]
pub fn scarlett_gen1_read<'l>(mut env:EnvUnowned<'l>,_class:JObject<'l>,dac:i64)->JString<'l> {
    run(&mut env,|env| java_string(env,&gen1_read_value(dac,false)?))
}
#[jni_mangle("dev.usbdac.internal.Native")]
pub fn scarlett_gen1_read_snapshot<'l>(mut env:EnvUnowned<'l>,_class:JObject<'l>,dac:i64)->JString<'l> {
    run(&mut env,|env| java_string(env,&gen1_read_value(dac,true)?))
}
#[jni_mangle("dev.usbdac.internal.Native")]
pub fn scarlett_gen1_apply<'l>(mut env:EnvUnowned<'l>,_class:JObject<'l>,dac:i64,snapshot:i64,edits:JString<'l>)->JString<'l> {
    run(&mut env,|env| {
        let input=string_of(env,&edits)?;
        if input.len()>262144 { return Err(Failure::Bad("First-generation edits exceed 262144 bytes".into())); }
        let edits=parse_gen1_edits(&serde_json::from_str::<Value>(&input).map_err(|e|Failure::Bad(e.to_string()))?)?;
        let entry=registry::dac(dac)?;
        let expected=entry.gen1(snapshot)?;
        let result=match entry.dac.scarlett_gen1_apply(&expected,&edits) {
            Ok(report)=>{
                let original=gen1_readback(&report.original);
                let mut verified=gen1_readback(&report.verified);
                verified["snapshot_id"]=json!(entry.remember_gen1(report.verified)?);
                json!({"value":{"original":original,"verified":verified}})
            }, Err(e)=>failure(e),
        };
        java_string(env,&result)
    })
}
fn word(v: &Value, key: &str) -> Outcome<u16> {
    v[key].as_u64().and_then(|n|u16::try_from(n).ok())
        .ok_or_else(||Failure::Bad(format!("FCP {key} must be unsigned 16-bit")))
}
fn parse_fcp_edits(v: &Value) -> Outcome<(Vec<FcpRouteEdit>, Vec<FcpMixEdit>)> {
    if !v.as_object().is_some_and(|v|v.keys().all(|k|matches!(k.as_str(),"routes"|"mixes"))) {
        return Err(Failure::Bad("FCP edits contain only routes and mixes".into()));
    }
    let max_routes = 3 * usize::from(u8::MAX);
    let routes=v["routes"].as_array().filter(|a|a.len()<=max_routes)
        .ok_or_else(||Failure::Bad(format!("FCP requires at most {max_routes} typed routes")))?;
    let mixes=v["mixes"].as_array().filter(|a|a.len()<=4096)
        .ok_or_else(||Failure::Bad("FCP requires at most 4096 typed coefficients".into()))?;
    if routes.is_empty() && mixes.is_empty() { return Err(Failure::Bad("FCP edits are empty".into())); }
    let routes=routes.iter().map(|v| {
        if !v.as_object().is_some_and(|v|v.keys().all(|k|matches!(k.as_str(),"bank"|"destination"|"source"))) {
            return Err(Failure::Bad("FCP routes contain only bank, destination and source".into()));
        }
        let bank=byte(v,"bank")?;
        if bank>2 { return Err(Failure::Bad("FCP bank must be 0 to 2".into())); }
        Ok(FcpRouteEdit {bank,destination:word(v,"destination")?,source:word(v,"source")?})
    }).collect::<Outcome<Vec<_>>>()?;
    let mixes=mixes.iter().map(|v| {
        if !v.as_object().is_some_and(|v|v.keys().all(|k|matches!(k.as_str(),"output"|"input"|"coefficient"))) {
            return Err(Failure::Bad("FCP mixes contain only output, input and coefficient".into()));
        }
        let output=byte(v,"output")?; let input=byte(v,"input")?; let coefficient=word(v,"coefficient")?;
        if output==0 || input==0 || coefficient>32613 {
            return Err(Failure::Bad("FCP mixes require one-based coordinates and coefficient 0 to 32613".into()));
        }
        Ok(FcpMixEdit {output,input,coefficient})
    }).collect::<Outcome<Vec<_>>>()?;
    Ok((routes,mixes))
}
#[jni_mangle("dev.usbdac.internal.Native")]
pub fn fcp_apply<'l>(mut env:EnvUnowned<'l>,_class:JObject<'l>,dac:i64,snapshot:i64,edits:JString<'l>)->JString<'l> {
    run(&mut env,|env| {
        let input=string_of(env,&edits)?;
        if input.len()>262144 { return Err(Failure::Bad("FCP edits exceed 262144 bytes".into())); }
        let (routes,mixes)=parse_fcp_edits(&serde_json::from_str::<Value>(&input).map_err(|e|Failure::Bad(e.to_string()))?)?;
        let entry=registry::dac(dac)?;
        let expected=entry.fcp(snapshot)?;
        let result=match entry.dac.fcp_routing_apply(&expected,&routes,&mixes) {
            Ok(report)=>{
                let original=fcp_readback(&report.original);
                let mut verified=fcp_readback(&report.verified);
                verified["snapshot_id"]=json!(entry.remember_fcp(report.verified)?);
                json!({"value":{"original":original,"verified":verified}})
            }, Err(e)=>failure(e),
        };
        java_string(env,&result)
    })
}
#[jni_mangle("dev.usbdac.internal.Native")]
pub fn scarlett_mix_apply<'l>(mut env:EnvUnowned<'l>,_class:JObject<'l>,dac:i64,edit:JString<'l>)->JString<'l> {
    run(&mut env,|env| {let input=string_of(env,&edit)?;if input.len()>8192 {return Err(Failure::Bad("Scarlett edit exceeds 8192 bytes".into()));}let edit=parse_edit(&serde_json::from_str::<Value>(&input).map_err(|e|Failure::Bad(e.to_string()))?)?;let e=registry::dac(dac)?;let v=match e.dac.scarlett_mix_apply(&edit){Ok(v)=>json!({"value":{"original":row(&v.original),"verified":row(&v.verified)}}),Err(e)=>failure(e)};java_string(env,&v)})
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn gen1_edits_keep_signed_values_and_reject_raw_proof_or_invalid_keys() {
        let mut v=json!([{"key":{"kind":"matrix_input","index":1},"value":-1},
            {"key":{"kind":"gain","input":18,"mix":8},"value":-32768}]);
        let edits=parse_gen1_edits(&v).unwrap();
        assert_eq!(edits[0],Gen1Edit {key:Gen1Key::MatrixInput(1),value:-1});
        assert_eq!(edits[1].value,-32768);
        v[0]["key"]["index"]=json!(0); assert!(parse_gen1_edits(&v).is_err());
        v[0]["key"]["index"]=json!(1);
        v[0]["expected"]=json!({"controls":[]}); assert!(parse_gen1_edits(&v).is_err());
        v[0].as_object_mut().unwrap().remove("expected");
        v[0]["value"]=json!(-2); assert!(parse_gen1_edits(&v).is_err());
        v[0]["value"]=json!(-1);
        v[1]["value"]=json!(32768); assert!(parse_gen1_edits(&v).is_err());
        assert!(parse_gen1_edits(&json!([])).is_err());
        assert!(parse_gen1_edits(&json!([{"key":{"kind":"output","index":1},"value":0},
            {"key":{"kind":"output","index":1},"value":1}])).is_err());
    }
    #[test]
    fn gen1_readback_is_stateless_and_failures_preserve_restore_keys() {
        let readback=Gen1Readback {model:Gen1Model::Forte,
            controls:vec![Gen1Control {key:Gen1Key::Gain {input:1,mix:1},value:-32767}],
            source_offsets:[0,4,6,6,6],matrix_sources:7,output_sources:0};
        let v=gen1_readback(&readback);
        assert_eq!(v["model"],"forte");
        assert_eq!(v["controls"][0]["value"],-32767);
        assert!(v.get("snapshot_id").is_none());
        let key=Gen1Key::Output(2);
        let v=failure(usbdac_core::routing::RoutingSessionError::Gen1(Gen1WriteError {
            failure:Gen1Failure::Io {key,error:"write".into()},
            rollback:vec![(key,Gen1Failure::Verification {key})],
        }));
        assert_eq!(v["error"]["cause"]["detail"],"write");
        assert_eq!(v["error"]["rollback_failures"][0]["key"]["index"],2);
        assert_eq!(v["error"]["rollback_failures"][0]["cause"]["kind"],"verification");
        let v=gen1_failure(&Gen1Failure::Drift {key:None});
        assert_eq!(v["key"],Value::Null);
    }
    #[test]
    fn fcp_typed_edits_reject_raw_proof_and_out_of_range_coefficients() {
        let mut v=json!({"routes":[{"bank":2,"destination":65535,"source":0}],"mixes":[{"output":1,"input":255,"coefficient":32613}]});
        let (routes,mixes)=parse_fcp_edits(&v).unwrap();
        assert_eq!(routes[0],FcpRouteEdit {bank:2,destination:65535,source:0});
        assert_eq!(mixes[0].coefficient,32613);
        v["expected"]=json!({"banks":[]}); assert!(parse_fcp_edits(&v).is_err());
        v.as_object_mut().unwrap().remove("expected");
        v["mixes"][0]["coefficient"]=json!(32614); assert!(parse_fcp_edits(&v).is_err());
        v["mixes"][0]["coefficient"]=json!(0);
        v["mixes"][0]["input"]=json!(0); assert!(parse_fcp_edits(&v).is_err());
        v["mixes"][0]["input"]=json!(1);
        v["routes"][0]["raw_words"]=json!([1]); assert!(parse_fcp_edits(&v).is_err());
        assert!(parse_fcp_edits(&json!({"routes":[],"mixes":[]})).is_err());
    }
    #[test]
    fn fcp_failures_preserve_bank_and_row_rollback_targets() {
        let v=failure(usbdac_core::routing::RoutingSessionError::Fcp(FcpWriteError {
            failure:FcpFailure::Verification(FcpTarget::Mix(2)),
            rollback:vec![(FcpTarget::Bank(1),FcpFailure::Read(ReadError::Io("restore bank".into()))),
                (FcpTarget::Mix(2),FcpFailure::Verification(FcpTarget::Mix(2)))],
        }));
        assert_eq!(v["error"]["kind"],"fcp");
        assert_eq!(v["error"]["cause"]["target"],json!({"kind":"mix","index":2}));
        assert_eq!(v["error"]["rollback_failures"][0]["target"],json!({"kind":"bank","index":1}));
        assert_eq!(v["error"]["rollback_failures"][0]["cause"]["detail"],"restore bank");
        assert_eq!(v["error"]["rollback_failures"][1]["cause"]["kind"],"verification");
    }
    #[test]
    fn fcp_bridge_preserves_firmware_pins_fixed_routes_and_unsigned_banks() {
        let v=fcp_readback(&FcpReadback {
            model:CandidateModel::Scarlett16i16Gen4,firmware:0xffff_ffff,
            mixer_inputs:1,mixer_outputs:1,
            sources:vec![FcpPort {pin:65535,name:"Mic 1".into(),fixed_source:None}],
            destinations:vec![FcpPort {pin:40000,name:"Fixed Output".into(),fixed_source:Some(65535)}],
            banks:vec![FcpBank {bank:2,raw_words:vec![0xffff_ffff,0x8000_0000,0]}],
            mixes:vec![MixReadback {output:1,coefficients:vec![65535]}],
        });
        assert_eq!(v["model"],"scarlett_16i16_gen4");
        assert!(v.get("snapshot_id").is_none()); // Stateless readback serialization never admits a token.
        assert_eq!(v["sources"][0]["fixed_source"],Value::Null);
        assert_eq!(v["destinations"][0]["fixed_source"],65535);
        assert_eq!(v["destinations"][0]["name"],"Fixed Output");
        assert_eq!(v["banks"][0]["raw_words"][0],0xffff_ffffu32);
        assert_eq!(v["mixes"][0]["coefficients"][0],65535);
    }
    #[test]
    fn mux_edits_accept_only_typed_routes_without_caller_raw_baselines() {
        let mut edit=json!([{"bank":2,"destination":{"kind":"analogue","channel":1},"source":{"kind":"off"}}]);
        let routes=parse_mux_routes(&edit).unwrap();
        assert_eq!(routes[0], MuxRouteEdit {bank:2,destination:Port::Channel{kind:PortKind::Analogue,channel:1},source:Port::Off});
        edit[0]["expected"]=json!({"banks":[]});
        assert!(parse_mux_routes(&edit).is_err());
        edit[0].as_object_mut().unwrap().remove("expected");
        edit[0]["bank"]=json!(3); assert!(parse_mux_routes(&edit).is_err());
        edit[0]["bank"]=json!(0);
        edit[0]["source"]=json!({"kind":"off","channel":1}); assert!(parse_mux_routes(&edit).is_err());
        edit[0]["source"]=json!({"kind":"pcm","channel":0}); assert!(parse_mux_routes(&edit).is_err());
        edit[0]["source"]=json!({"kind":"pcm","channel":256}); assert!(parse_mux_routes(&edit).is_err());
        edit[0]["source"]=json!({"kind":"raw","channel":1}); assert!(parse_mux_routes(&edit).is_err());
        assert!(parse_mux_routes(&json!([])).is_err());
        assert!(parse_mux_routes(&json!(vec![Value::Null;257])).is_err());
    }
    #[test]
    fn mux_failure_preserves_every_bank_rollback_cause() {
        let v=failure(usbdac_core::routing::RoutingSessionError::ScarlettMux(MuxWriteError {
            failure:MuxFailure::Read(ReadError::Io("write".into())),
            rollback:vec![(0,MuxFailure::Read(ReadError::Io("restore zero".into()))),
                (2,MuxFailure::Invalid(ProtocolError::InvalidMuxBank(2)))],
        }));
        assert_eq!(v["error"]["kind"], "mux");
        assert_eq!(v["error"]["cause"]["detail"], "write");
        assert_eq!(v["error"]["rollback_failures"][0]["bank"], 0);
        assert_eq!(v["error"]["rollback_failures"][0]["cause"]["detail"], "restore zero");
        assert_eq!(v["error"]["rollback_failures"][1]["bank"], 2);
        assert_eq!(v["error"]["rollback_failures"][1]["cause"]["kind"], "invalid");
    }
    #[test] fn exact_unsigned_rows_and_bounded_gains() {
        let mut v=json!({"expected":{"output":1,"coefficients":vec![65535;18]},"gains":[{"input":1,"half_db":-160}]});
        assert_eq!(parse_edit(&v).unwrap().expected.coefficients[0],65535);
        v["gains"][0]["half_db"]=json!(25);assert!(parse_edit(&v).is_err());
        v["gains"][0]["half_db"]=json!(0);v["expected"]["coefficients"][0]=json!(-1);assert!(parse_edit(&v).is_err());
    }
    #[test] fn legacy_maximum_row_is_bounded_before_exact_native_validation() {
        let mut v=json!({"expected":{"output":12,"coefficients":vec![65535;25]},"gains":[{"input":25,"half_db":0}]});
        let edit=parse_edit(&v).unwrap();
        assert_eq!(edit.expected.coefficients.len(),25);
        assert_eq!(edit.gains[0].input,25);
        v["expected"]["coefficients"]=json!(vec![0;26]);
        assert!(parse_edit(&v).is_err());
    }
    #[test] fn mix_failure_preserves_actual_and_rollback() {
        let v=failure(usbdac_core::routing::RoutingSessionError::ScarlettMix(MixWriteError{failure:MixFailure::Verification{actual:MixReadback{output:2,coefficients:vec![65535;18]}},rollback:Some(MixFailure::Read(ReadError::Io("restore".into())))}));
        assert_eq!(v["error"]["cause"]["actual"]["coefficients"][0],65535);assert_eq!(v["error"]["rollback_failure"]["detail"],"restore");
    }
}
