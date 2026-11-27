use serde_json::json;
use std::{
    collections::HashSet,
    env,
    io::{self, BufRead, Write},
    path::PathBuf,
    process::Command,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, OnceLock,
    },
    thread,
    time::{Duration, Instant},
};
use usbdac_core::{darwin_discovery, Dac, DacConfig, HandBack, QuirkPolicy, StartupVolume};
use usbdac_validator::{Capture, RunOptions};

static CAPTURE: OnceLock<Arc<Capture>> = OnceLock::new();
struct Logger;
impl log::Log for Logger {
    fn enabled(&self, m: &log::Metadata<'_>) -> bool {
        m.level() <= log::Level::Debug
    }
    fn log(&self, r: &log::Record<'_>) {
        if self.enabled(r.metadata()) {
            if let Some(c) = CAPTURE.get() {
                c.record("rust_log", json!({"level": r.level().to_string(), "target": r.target(), "message": r.args().to_string()}));
            }
        }
    }
    fn flush(&self) {}
}
static LOGGER: Logger = Logger;
fn driver_version() -> &'static str {
    let workspace = include_str!("../../../../Cargo.toml");
    workspace
        .split("[workspace.package]")
        .nth(1)
        .and_then(|s| {
            s.lines()
                .find_map(|line| line.trim().strip_prefix("version = \""))
        })
        .and_then(|s| s.split('"').next())
        .unwrap_or("unknown")
}
fn emit(value: serde_json::Value) {
    println!("{}", value);
    let _ = io::stdout().flush();
}
fn main() {
    let args: Vec<String> = env::args().collect();
    let Some(dir) = args.get(1).map(PathBuf::from) else {
        emit(json!({"type":"fatal", "message":"Missing capture directory"}));
        return;
    };
    let capture = match Capture::new(&dir) {
        Ok(c) => Arc::new(c),
        Err(e) => {
            emit(json!({"type":"fatal", "message":format!("Cannot start capture: {e}")}));
            return;
        }
    };
    let _ = CAPTURE.set(capture.clone());
    let _ = log::set_logger(&LOGGER);
    log::set_max_level(log::LevelFilter::Debug);
    let cancel = Arc::new(AtomicBool::new(false));
    let signal = cancel.clone();
    thread::spawn(move || {
        for line in io::stdin().lock().lines() {
            if line.is_err() || matches!(line.as_deref(), Ok("cancel")) {
                break;
            }
        }
        signal.store(true, Ordering::Relaxed);
    });
    let allow_capture = args.iter().any(|a| a == "--allow-capture");
    let policy = if args.iter().any(|a| a == "--verified-only") {
        QuirkPolicy::VerifiedOnly
    } else {
        QuirkPolicy::SourceBacked
    };
    let policy_label = match policy {
        QuirkPolicy::VerifiedOnly => "verified_only",
        QuirkPolicy::SourceBacked => "source_backed",
    };
    capture.record("compatibility_policy", json!({"quirk_policy":policy_label}));
    capture.record("host", json!({"os":"macos", "arch":env::consts::ARCH, "app_version":env!("CARGO_PKG_VERSION"), "driver_version":driver_version(), "driver_version_source":"workspace package version", "os_version":Command::new("sw_vers").output().ok().map(|v| String::from_utf8_lossy(&v.stdout).into_owned()), "effective_uid":unsafe { libc::geteuid() }, "allow_device_capture":allow_capture}));
    let device_lost = AtomicBool::new(false);
    let result = run(&capture, &cancel, allow_capture, policy, &device_lost);
    capture.record("terminal", json!({"scope": if result.is_ok() {"run"} else {"host_or_driver"}, "result":match &result { Ok(()) => "completed".to_string(), Err(e) => e.clone() }, "cancelled":cancel.load(Ordering::Relaxed), "termination_reason":if device_lost.load(Ordering::Acquire) { Some("device_lost") } else if cancel.load(Ordering::Relaxed) { Some("cancelled") } else { None }}));
    let zip = dir.join("diagnostics.zip");
    match capture.export_zip(&zip) {
        Ok(()) => emit(
            json!({"type":"finished", "path":zip, "message":result.err().unwrap_or_else(|| "Run complete".into())}),
        ),
        Err(e) => {
            emit(json!({"type":"fatal", "message":format!("Could not export diagnostics: {e}")}))
        }
    }
}
fn run(
    c: &Capture,
    cancel: &AtomicBool,
    allow_capture: bool,
    policy: QuirkPolicy,
    device_lost: &AtomicBool,
) -> Result<(), String> {
    let context = rusb::Context::new().map_err(|e| format!("USB initialization: {e}"))?;
    let baseline =
        darwin_discovery::snapshot(&context).map_err(|e| format!("USB baseline: {e}"))?;
    let mut keys: HashSet<_> = baseline.iter().map(|a| a.evidence.key.clone()).collect();
    c.record(
        "baseline",
        json!({"device_count":keys.len(), "selection":"new USB audio playback attachment only"}),
    );
    emit(json!({"type":"waiting", "message":"Attach one USB DAC now"}));
    let deadline = Instant::now() + Duration::from_secs(120);
    let mut noted = HashSet::new();
    let attachment = loop {
        if cancel.load(Ordering::Relaxed) {
            return Err("Cancelled while waiting for a DAC".into());
        }
        if Instant::now() >= deadline {
            return Err("Timed out waiting for a DAC".into());
        }
        let devices =
            darwin_discovery::snapshot(&context).map_err(|e| format!("USB discovery: {e}"))?;
        let present: HashSet<_> = devices.iter().map(|a| a.evidence.key.clone()).collect();
        keys.retain(|key| present.contains(key));
        for device in &devices {
            if !keys.contains(&device.evidence.key)
                && device.evidence.audio_playback_interfaces.is_empty()
                && noted.insert(device.evidence.key.clone())
            {
                c.record("unclassified_new_usb_candidate", json!({
                    "summary":darwin_discovery::unselected_candidate_summary(device),
                    "decision":"not opened; no advertised standard audio playback interface",
                    "vendor_pcm":"vendor-specific PCM profiles are not released by this driver and are not selected"
                }));
            }
        }
        let mut candidates: Vec<_> = devices
            .into_iter()
            .filter(|a| {
                !keys.contains(&a.evidence.key) && !a.evidence.audio_playback_interfaces.is_empty()
            })
            .collect();
        if candidates.len() > 1 {
            c.record(
                "candidates",
                json!(candidates.iter().map(|a| &a.evidence).collect::<Vec<_>>()),
            );
            return Err(
                "Multiple new USB audio devices found; reconnect one DAC and start again".into(),
            );
        }
        if let Some(a) = candidates.pop() {
            break a;
        }
        thread::sleep(Duration::from_millis(300));
    };
    c.record("attachment", json!(attachment.evidence));
    emit(json!({"type":"status", "message":"Reading USB descriptors"}));
    for record in darwin_discovery::descriptor_evidence(&attachment) {
        c.record(record.kind, record.value);
    }
    if cancel.load(Ordering::Relaxed) {
        return Err("Cancelled after descriptor capture".into());
    }
    emit(json!({"type":"status", "message":"Opening the attached DAC"}));
    let config = DacConfig {
        startup_volume: StartupVolume::Keep,
        quirk_policy: policy,
        hand_back: HandBack::Keep,
        require_hardware_volume: false,
        capture_feedback_trace: true,
        ..DacConfig::default()
    };
    c.record("open_options", json!({"startup_volume":"keep", "hand_back":"keep", "require_hardware_volume":false, "allow_device_capture":allow_capture, "quirk_policy":format!("{policy:?}")}));
    let dac = Dac::open_darwin(attachment, config, allow_capture).map_err(|e| {
        if matches!(&e, usbdac_core::Error::DeviceLost | usbdac_core::Error::HostUsb { kind: usbdac_core::HostUsbFailureKind::DeviceLost, .. }) { device_lost.store(true, Ordering::Release); }
        let host_usb = match &e {
            usbdac_core::Error::HostUsb { operation, kind, detail } => json!({"operation":operation, "kind":format!("{kind:?}"), "detail":detail}),
            _ => serde_json::Value::Null,
        };
        c.record("open_error", json!({"scope":"driver", "error":format!("{e:?}"), "code":e.code(), "host_usb":host_usb}));
        format!("USB driver open or claim failed: {e}")
    })?;
    let report = c.run(&dac, RunOptions::default(), cancel, |p| {
        emit(json!({"type":"progress", "progress":p}))
    });
    // Keep the GUI and ZIP aligned with the shared suite's coverage accounting.
    // A mode with no admitted cases is distinct from one that was planned but
    // never exercised; neither is evidence of an audible result.
    if let Ok(value) = serde_json::to_value(&report) {
        if let Some(coverage) = value.get("mode_coverage") {
            c.record("mode_coverage", coverage.clone());
            for (key, label) in [("pcm", "PCM"), ("dop", "DoP"), ("native_dsd", "Native DSD")] {
                if let Some(mode) = coverage.get(key) {
                    let status = mode
                        .get("status")
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown");
                    let display = match status {
                        "no_admitted_cases" => "no admitted cases",
                        "not_exercised" => "planned, not exercised",
                        "partially_exercised" => "partially exercised",
                        "exercised" => "exercised",
                        _ => "coverage unavailable",
                    };
                    emit(
                        json!({"type":"status", "message":format!("{label}: {display} (planned {}, exercised {}, passed {})", mode["planned"], mode["exercised"], mode["passed"])}),
                    );
                }
            }
        }
    }
    let unexpected = report
        .admission_results
        .iter()
        .filter(|leg| leg.admission.as_deref() == Some("unexpected_admitted"))
        .count();
    let inconclusive = report
        .admission_results
        .iter()
        .filter(|leg| leg.admission.as_deref() == Some("inconclusive"))
        .count();
    emit(
        json!({"type":"status", "message":format!("Admission checks: {} attempted, {unexpected} unexpectedly admitted, {inconclusive} inconclusive.", report.admission_results.len())}),
    );
    let close = dac.close();
    c.record("dac_close", json!({"result":format!("{close:?}")}));
    if report.termination_reason.as_deref() == Some("device_lost") {
        device_lost.store(true, Ordering::Release);
        c.record(
            "run_termination",
            json!({"reason":"device_lost", "source":"driver", "close":format!("{close:?}")}),
        );
        return Err("Selected DAC was lost during testing".into());
    }
    if let Err(e) = close {
        return Err(format!("DAC teardown: {e}"));
    }
    if report.cancelled {
        return Err("Cancelled during testing".into());
    }
    if let Some(e) = report.error {
        return Err(e);
    }
    let failures = report
        .legs
        .iter()
        .filter(|leg| leg.status != "passed_usb")
        .count();
    if failures > 0 {
        return Err(format!(
            "Run finished with {failures} failed or unverified legs; admission: {unexpected} unexpectedly admitted, {inconclusive} inconclusive; inspect diagnostics"
        ));
    }
    if unexpected > 0 || inconclusive > 0 {
        return Err(format!(
            "Admission findings: {unexpected} unexpectedly admitted, {inconclusive} inconclusive"
        ));
    }
    if report.legs.is_empty() {
        return Err("No supported test cases were found".into());
    }
    Ok(())
}
