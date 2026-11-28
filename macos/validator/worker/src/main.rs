use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use serde_json::json;
use std::{
    collections::HashSet,
    env,
    io::{self, BufRead, BufReader, Read, Write},
    os::{
        fd::AsRawFd,
        unix::{ffi::OsStrExt, net::UnixStream},
    },
    path::PathBuf,
    process::Command,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex, OnceLock,
    },
    thread,
    time::{Duration, Instant},
};
use usbdac_core::{darwin_discovery, Dac, DacConfig, HandBack, QuirkPolicy, StartupVolume};
use usbdac_validator::{Capture, RunOptions};

static CAPTURE: OnceLock<Arc<Capture>> = OnceLock::new();
static OUTPUT: OnceLock<Mutex<Box<dyn Write + Send>>> = OnceLock::new();
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
    if let Some(output) = OUTPUT.get() {
        let mut output = output.lock().unwrap_or_else(|e| e.into_inner());
        let _ = writeln!(output, "{value}");
        let _ = output.flush();
    } else {
        println!("{value}");
        let _ = io::stdout().flush();
    }
}

/// The privileged worker never accepts an output path from the caller.
fn private_capture_dir() -> io::Result<PathBuf> {
    let mut template = b"/private/tmp/usbdac-worker.XXXXXX\0".to_vec();
    let path = unsafe { libc::mkdtemp(template.as_mut_ptr().cast()) };
    if path.is_null() {
        return Err(io::Error::last_os_error());
    }
    let path = unsafe { std::ffi::CStr::from_ptr(path) };
    Ok(PathBuf::from(std::ffi::OsStr::from_bytes(path.to_bytes())))
}

fn privileged_channel(args: &[String]) -> io::Result<(UnixStream, PathBuf)> {
    if unsafe { libc::geteuid() } != 0 {
        return Err(io::Error::other("worker is not privileged"));
    }
    let socket = args
        .get(2)
        .ok_or_else(|| io::Error::other("missing socket"))?;
    let expected_uid: libc::uid_t = args
        .get(3)
        .and_then(|s| s.parse().ok())
        .filter(|uid| *uid != 0)
        .ok_or_else(|| io::Error::other("invalid invoking uid"))?;
    let stream = UnixStream::connect(socket)?;
    let no_sigpipe: libc::c_int = 1;
    if unsafe {
        libc::setsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_NOSIGPIPE,
            (&no_sigpipe as *const libc::c_int).cast(),
            std::mem::size_of_val(&no_sigpipe) as libc::socklen_t,
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    let mut peer_uid = 0;
    let mut peer_gid = 0;
    if unsafe { libc::getpeereid(stream.as_raw_fd(), &mut peer_uid, &mut peer_gid) } != 0 {
        return Err(io::Error::last_os_error());
    }
    if peer_uid != expected_uid {
        return Err(io::Error::other("socket peer uid mismatch"));
    }
    Ok((stream, private_capture_dir()?))
}

fn send_archive(path: &std::path::Path) -> io::Result<()> {
    const MAX_ARCHIVE: u64 = 64 * 1024 * 1024;
    let mut file = std::fs::File::open(path)?;
    let length = file.metadata()?.len();
    if length > MAX_ARCHIVE {
        return Err(io::Error::other("archive exceeds transfer limit"));
    }
    emit(json!({"type":"archive_start", "bytes":length}));
    let mut buffer = [0u8; 24 * 1024];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        emit(json!({"type":"archive_chunk", "data":BASE64.encode(&buffer[..count])}));
    }
    emit(json!({"type":"archive_end", "bytes":length}));
    Ok(())
}

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.get(1).is_some_and(|arg| arg == "--render-report") {
        let Some(path) = args.get(2) else {
            eprintln!("Missing capture directory");
            std::process::exit(2);
        };
        if let Err(error) = usbdac_validator::write_html_report(std::path::Path::new(path)) {
            eprintln!("HTML report: {error}");
            std::process::exit(1);
        }
        return;
    }
    let privileged = args.get(1).is_some_and(|v| v == "--privileged-socket");
    let (dir, mut input): (PathBuf, Box<dyn BufRead + Send>) = if privileged {
        match privileged_channel(&args) {
            Ok((stream, dir)) => {
                match stream.try_clone() {
                    Ok(output) => {
                        let _ = OUTPUT.set(Mutex::new(Box::new(output)));
                    }
                    Err(_) => {
                        let _ = std::fs::remove_dir_all(&dir);
                        return;
                    }
                }
                (dir, Box::new(BufReader::new(stream)))
            }
            Err(error) => {
                eprintln!("Privileged channel: {error}");
                return;
            }
        }
    } else {
        let Some(dir) = args.get(1).map(PathBuf::from) else {
            emit(json!({"type":"fatal", "message":"Missing capture directory"}));
            return;
        };
        (dir, Box::new(BufReader::new(io::stdin())))
    };
    let capture = match Capture::new(&dir) {
        Ok(c) => Arc::new(c),
        Err(e) => {
            emit(json!({"type":"fatal", "message":format!("Cannot start capture: {e}")}));
            if privileged {
                let _ = std::fs::remove_dir_all(&dir);
            }
            return;
        }
    };
    let _ = CAPTURE.set(capture.clone());
    let _ = log::set_logger(&LOGGER);
    log::set_max_level(log::LevelFilter::Debug);
    let cancel = Arc::new(AtomicBool::new(false));
    let signal = cancel.clone();
    thread::spawn(move || {
        if privileged {
            // The private protocol has only one client command: any input or
            // EOF means cancel. Never accumulate an unbounded line from a peer.
            let mut command = [0u8; 256];
            let _ = input.read(&mut command);
        } else {
            for line in input.lines() {
                if line.is_err() || matches!(line.as_deref(), Ok("cancel")) {
                    break;
                }
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
    let libusb = rusb::version();
    let libusb_version = json!({
        "major": libusb.major(), "minor": libusb.minor(), "micro": libusb.micro(),
        "nano": libusb.nano(), "rc": libusb.rc(),
    });
    capture.record("host", json!({"libusb_version":libusb_version, "os":"macos", "arch":env::consts::ARCH, "app_version":env!("CARGO_PKG_VERSION"), "driver_version":driver_version(), "driver_version_source":"workspace package version", "os_version":Command::new("/usr/bin/sw_vers").output().ok().map(|v| String::from_utf8_lossy(&v.stdout).into_owned()), "effective_uid":unsafe { libc::geteuid() }, "allow_device_capture":allow_capture}));
    let device_lost = AtomicBool::new(false);
    let result = run(&capture, &cancel, allow_capture, policy, &device_lost);
    capture.record("terminal", json!({"scope": if result.is_ok() {"run"} else {"host_or_driver"}, "result":match &result { Ok(()) => "completed".to_string(), Err(e) => e.clone() }, "cancelled":cancel.load(Ordering::Relaxed), "termination_reason":if device_lost.load(Ordering::Acquire) { Some("device_lost") } else if cancel.load(Ordering::Relaxed) { Some("cancelled") } else { None }}));
    let zip = dir.join("diagnostics.zip");
    match capture.export_zip(&zip) {
        Ok(()) if privileged => match send_archive(&zip) {
            Ok(()) => emit(
                json!({"type":"finished", "message":result.err().unwrap_or_else(|| "Run complete".into())}),
            ),
            Err(error) => {
                emit(json!({"type":"fatal", "message":format!("Archive transfer failed: {error}")}))
            }
        },
        Ok(()) => emit(
            json!({"type":"finished", "path":zip, "message":result.err().unwrap_or_else(|| "Run complete".into())}),
        ),
        Err(e) => {
            emit(json!({"type":"fatal", "message":format!("Could not export diagnostics: {e}")}))
        }
    }
    if privileged {
        let _ = std::fs::remove_dir_all(&dir);
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
    c.record("open_options", json!({"probe_capture_inputs":true,"startup_volume":"keep", "hand_back":"keep", "require_hardware_volume":false, "allow_device_capture":allow_capture, "quirk_policy":format!("{policy:?}")}));
    let dac = Dac::open_darwin_with_capture_probe(attachment, config, allow_capture, true).map_err(|e| {
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
    if !report.fallback_results.is_empty() {
        let count = |status: &str| {
            report
                .fallback_results
                .iter()
                .filter(|item| item.get("status").and_then(|v| v.as_str()) == Some(status))
                .count()
        };
        emit(
            json!({"type":"status", "message":format!("Converted fallbacks: {} total. {} USB passed, {} failed, {} inconclusive, {} unavailable and {} stopped.", report.fallback_results.len(), count("passed_usb"), count("failed"), count("inconclusive"), count("no_fallback"), count("device_lost") + count("cancelled"))}),
        );
        let mut examples: Vec<_> = report.fallback_results.iter().collect();
        examples.sort_by_key(|item| match item.get("status").and_then(|v| v.as_str()) {
            Some("passed_usb" | "failed") => 0,
            Some("inconclusive" | "device_lost" | "cancelled") => 1,
            _ => 2,
        });
        for item in examples.into_iter().take(3) {
            let text = |key: &str| item.get(key).and_then(|v| v.as_str()).unwrap_or("unknown");
            let note = if text("trigger") == "dop_decoder_unconfirmed" {
                " (DoP decoder unobserved. Validator fallback policy.)"
            } else {
                ""
            };
            emit(
                json!({"type":"status", "message":format!("Fallback {}. Result {}. Source fidelity {}. Target USB {}{}", text("source_request"), text("status"), text("source_fidelity"), text("target_wire_integrity"), note)}),
            );
        }
        if report.fallback_results.len() > 3 {
            emit(
                json!({"type":"status", "message":"Further fallback details are in the archive."}),
            );
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
    if report.termination_reason.as_deref() == Some("retirement_failed") {
        return Err("USB retirement failed during testing. Inspect diagnostics.".into());
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
    if report.status == "no_cases" {
        return Err(
            "No admitted exact playback cases were found. Inspect fallback results in diagnostics."
                .into(),
        );
    }
    if report.status != "passed_usb" {
        return Err(format!(
            "Run completed with findings ({}). Inspect diagnostics for exact and fallback results.",
            report.status
        ));
    }
    Ok(())
}
