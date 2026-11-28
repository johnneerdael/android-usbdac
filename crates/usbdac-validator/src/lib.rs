//! Bounded, append-on-disk evidence and exact-format USB DAC acceptance runs.
//! The host must warn users to turn down the amplifier before starting. This
//! crate never changes hardware volume. USB delivery does not prove audibility.
mod conversion_check;
mod fallback;
mod report_html;
use fallback::run_fallback;
pub use report_html::write_html_report;
use serde::Serialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{self, SyncSender},
        Arc, Mutex,
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use usbdac_core::{AltFormat, Capabilities, Dac, DacEvent, Encoding, StreamFormat, StreamRequest};

const LOG_LIMIT: u64 = 8 * 1024 * 1024;
const TONE_HZ: f64 = 997.0;
const AMPLITUDE: f64 = 0.01;

#[derive(Debug, Clone, Serialize)]
pub struct RunOptions {
    pub duration_ms: u64,
    pub leg_timeout_ms: u64,
}
impl Default for RunOptions {
    fn default() -> Self {
        Self {
            duration_ms: 1500,
            leg_timeout_ms: 10000,
        }
    }
}
#[derive(Debug, Clone, Serialize)]
pub struct Progress {
    pub index: usize,
    pub total: usize,
    pub phase: String,
    pub leg: String,
}
#[derive(Debug, Clone, Serialize)]
pub struct LegResult {
    pub request: String,
    pub advertised_alt: String,
    pub negotiated: Option<String>,
    pub status: String,
    pub admission: Option<String>,
    pub error: Option<String>,
    pub termination_reason: Option<String>,
    pub error_stage: Option<String>,
    pub error_code: Option<String>,
    pub close_outcome: Value,
    pub payload_sha256: Option<String>,
    pub payload_bytes: u64,
    pub evidence: Value,
}
#[derive(Debug, Clone, Serialize)]
pub struct Report {
    pub schema_version: u32,
    pub started_unix_ms: u128,
    pub coverage: String,
    pub signal: Value,
    pub capabilities: Value,
    pub mode_coverage: Value,
    pub status: String,
    pub plan: Vec<Value>,
    pub unattempted_plan: Vec<Value>,
    pub admission_plan: Vec<Value>,
    pub unattempted_admission_plan: Vec<Value>,
    pub admission_results: Vec<LegResult>,
    pub fallback_results: Vec<Value>,
    pub termination_reason: Option<String>,
    pub legs: Vec<LegResult>,
    pub cancelled: bool,
    pub error: Option<String>,
}

pub struct Capture {
    dir: PathBuf,
    started: Instant,
    file: Mutex<File>,
    debug: SyncSender<DebugMessage>,
    dropped: Arc<AtomicU64>,
    io_error: Arc<Mutex<Option<String>>>,
}
enum DebugMessage {
    Row(Value),
    Flush(mpsc::Sender<()>),
}
impl Capture {
    pub fn new(path: impl AsRef<Path>) -> io::Result<Self> {
        fs::create_dir_all(path.as_ref())?;
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path.as_ref().join("timeline.jsonl"))?;
        let debug_file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path.as_ref().join("debug.jsonl"))?;
        let (debug, rx) = mpsc::sync_channel::<DebugMessage>(1024);
        let dropped = Arc::new(AtomicU64::new(0));
        let io_error = Arc::new(Mutex::new(None));
        let worker_error = io_error.clone();
        std::thread::Builder::new()
            .name("validator-debug-writer".into())
            .spawn(move || {
                let mut file = debug_file;
                let mut size = file.metadata().map(|m| m.len()).unwrap_or(0);
                let mut truncated = false;
                while let Ok(message) = rx.recv() {
                    match message {
                        DebugMessage::Row(value) => {
                            if truncated {
                                continue;
                            }
                            let mut bytes = serde_json::to_vec(&value).unwrap_or_default();
                            bytes.push(b'\n');
                            if bytes.len() > 65536 || size + bytes.len() as u64 > LOG_LIMIT {
                                truncated = true;
                                bytes = b"{\"kind\":\"debug_truncated\"}\n".to_vec();
                            }
                            if let Err(e) = file.write_all(&bytes) {
                                *worker_error.lock().unwrap() = Some(format!("debug log: {e}"));
                            }
                            size += bytes.len() as u64;
                        }
                        DebugMessage::Flush(ack) => {
                            if let Err(e) = file.flush() {
                                *worker_error.lock().unwrap() = Some(format!("debug flush: {e}"));
                            }
                            let _ = ack.send(());
                        }
                    }
                }
            })?;
        let capture = Self {
            dir: path.as_ref().to_owned(),
            started: Instant::now(),
            file: Mutex::new(file),
            debug,
            dropped,
            io_error,
        };
        capture.record("capture_started", json!({"unix_ms": unix_ms()}));
        Ok(capture)
    }
    /// Thread-safe hook for host diagnostics, including forwarded Rust DEBUG logs.
    /// The timeline is capped; host values must not include credentials or secrets.
    pub fn record(&self, kind: &str, value: Value) {
        let row =
            json!({"elapsed_ms": self.started.elapsed().as_millis(), "kind": kind, "value": value});
        if kind == "driver_log" || kind == "rust_log" {
            if self.debug.try_send(DebugMessage::Row(row)).is_err() {
                self.dropped.fetch_add(1, Ordering::Relaxed);
            }
            return;
        }
        if let Ok(mut file) = self.file.lock() {
            let mut bytes = serde_json::to_vec(&row).unwrap_or_default();
            if bytes.len() > 65536 {
                bytes = b"{\"kind\":\"oversized_event_omitted\"}".to_vec();
            }
            bytes.push(b'\n');
            if let Err(e) = file.write_all(&bytes).and_then(|_| file.flush()) {
                *self.io_error.lock().unwrap() = Some(format!("timeline: {e}"));
            }
        }
    }
    /// Flush bounded debug output and report any capture write failure.
    pub fn flush_debug(&self) -> io::Result<()> {
        let (tx, rx) = mpsc::channel();
        self.debug
            .send(DebugMessage::Flush(tx))
            .map_err(|_| io::Error::other("debug writer stopped"))?;
        rx.recv()
            .map_err(|_| io::Error::other("debug writer stopped"))?;
        self.record(
            "debug_queue",
            json!({"dropped": self.dropped.load(Ordering::Relaxed)}),
        );
        if let Some(error) = self.io_error.lock().unwrap().clone() {
            return Err(io::Error::other(error));
        }
        Ok(())
    }

    pub fn run(
        &self,
        dac: &Dac,
        options: RunOptions,
        cancel: &AtomicBool,
        progress: impl Fn(&Progress),
    ) -> Report {
        self.run_with_device_loss(dac, options, cancel, &AtomicBool::new(false), progress)
    }

    pub fn run_with_device_loss(
        &self,
        dac: &Dac,
        options: RunOptions,
        cancel: &AtomicBool,
        device_lost: &AtomicBool,
        progress: impl Fn(&Progress),
    ) -> Report {
        let mut report = Report { schema_version: 1, started_unix_ms: unix_ms(), coverage: "Each advertised alternate: all discrete rates and standard points within ranges. Separate standard admission probes challenge unrepresented exact requests; unexpected admission is not evidence of decoding. DoP transport is not proof of DAC decoding.".into(), signal: json!({"frequency_hz": TONE_HZ, "amplitude_peak": AMPLITUDE, "channel_polarity": "even positive, odd inverted", "dsd": "first-order delta-sigma, initial integrator zero, chronological MSB-first", "digest": "SHA-256 of bytes accepted by stream.write; DoP marker supplied by driver"}), capabilities: Value::Null, mode_coverage: Value::Null, status: "running".into(), plan: vec![], unattempted_plan: vec![], admission_plan: vec![], unattempted_admission_plan: vec![], admission_results: vec![], fallback_results: vec![], termination_reason: None, legs: vec![], cancelled: false, error: None };
        self.record("run_started", json!({"options": options, "info": format!("{:?}", dac.info()), "volume": format!("{:?}", dac.volume().map(|v| v.get()))}));
        let caps = match dac.capabilities() {
            Ok(c) => c,
            Err(e) => {
                report.status = if is_device_lost(&e) || device_lost.load(Ordering::Acquire) {
                    report.termination_reason = Some("device_lost".into());
                    "device_lost"
                } else {
                    "failed"
                }
                .into();
                report.error = Some(format!("capabilities [{}]: {}", e.code(), e));
                self.save(&report);
                return report;
            }
        };
        self.record("capabilities", json!({"debug": format!("{caps:#?}")}));
        report.capabilities = json!({"supports_pcm": caps.supports_pcm, "pcm_sample_rates": caps.pcm_sample_rates, "pcm_bit_depths": caps.pcm_bit_depths, "supports_dop": caps.supports_dop, "dop_max_dsd_bit_rate": caps.dop_max_dsd_bit_rate, "supports_native_dsd": caps.supports_native_dsd, "native_dsd_bit_rates": caps.native_dsd_bit_rates});
        let plans = matrix(&caps);
        let probes = admission_matrix(&caps, &plans);
        report.admission_plan = probes.iter().map(|p| json!({"mode":mode(&p.request.encoding), "rate_hz":request_rate(&p.request), "request":format!("{:?}",p.request)})).collect();
        report.unattempted_admission_plan = report.admission_plan.clone();
        self.record(
            "admission_plan",
            json!({"cases": report.admission_plan.len(), "details": "report.json"}),
        );
        report.plan = plans.iter().map(|p| json!({"mode": mode(&p.request.encoding), "rate_hz": request_rate(&p.request), "request": format!("{:?}", p.request), "advertised_alt": p.alt, "identity": p.identity})).collect();
        report.unattempted_plan = report.plan.clone();
        report.mode_coverage = mode_coverage(&plans, &report.legs);
        if plans.is_empty() {
            report.status = "no_cases".into();
        }
        self.record("plan", json!(report.plan));
        self.save(&report);
        let mut dop_fallback_requests: Vec<StreamRequest> = Vec::new();
        for (index, plan) in plans.iter().enumerate() {
            if let Some(reason) = stop_reason(cancel, device_lost) {
                report.termination_reason = Some(reason.into());
                break;
            }
            let p = Progress {
                index,
                total: plans.len(),
                phase: "running".into(),
                leg: format!("{:?}", plan.request),
            };
            progress(&p);
            self.record("progress", json!(p));
            self.record("mode_transition", json!({"index": index, "request": format!("{:?}", plan.request), "advertised_alt": plan.alt}));
            let result = self.run_leg(dac, plan, &options, cancel, device_lost);
            self.record("leg_finished", leg_summary(&result, plan.identity));
            let mut terminal = result.termination_reason.clone();
            let is_dop = matches!(plan.request.encoding, Encoding::DoP);
            let dop_unconfirmed = is_dop
                && result.negotiated.is_some()
                && matches!(result.status.as_str(), "passed_usb" | "not_covered")
                && result.close_outcome["status"] != "failed";
            let exact_rejected = result.status == "unsupported"
                && result.error_stage.as_deref() == Some("negotiate");
            let should_fallback = (exact_rejected || dop_unconfirmed)
                && terminal.is_none()
                && (!is_dop || !dop_fallback_requests.contains(&plan.request));
            report.legs.push(result);
            report.unattempted_plan = unattempted_plan(&report.plan, report.legs.len());
            report.mode_coverage = mode_coverage(&plans, &report.legs);
            self.save(&report);
            if should_fallback {
                if is_dop {
                    dop_fallback_requests.push(plan.request);
                }
                let p = Progress {
                    index,
                    total: plans.len(),
                    phase: "fallback".into(),
                    leg: format!("{:?}", plan.request),
                };
                progress(&p);
                self.record("progress", json!(p));
                let mut fallback =
                    run_fallback(dac, &caps, &plan.request, &options, cancel, device_lost);
                fallback["trigger"] = json!(if exact_rejected {
                    "exact_unsupported"
                } else {
                    "dop_decoder_unconfirmed"
                });
                self.record("fallback_finished", fallback_summary(&fallback));
                if let Some(reason) = fallback.get("termination_reason").and_then(Value::as_str) {
                    terminal = Some(reason.into());
                }
                report.fallback_results.push(fallback);
                self.save(&report);
            }
            if terminal.is_some() {
                report.termination_reason = terminal;
                break;
            }
        }
        if report.termination_reason.is_none() {
            for (index, plan) in probes.iter().enumerate() {
                if let Some(reason) = stop_reason(cancel, device_lost) {
                    report.termination_reason = Some(reason.into());
                    break;
                }
                let p = Progress {
                    index,
                    total: probes.len(),
                    phase: "probing".into(),
                    leg: format!("{:?}", plan.request),
                };
                progress(&p);
                self.record("progress", json!(p));
                let result = self.run_leg(dac, plan, &options, cancel, device_lost);
                self.record("admission_finished", leg_summary(&result, plan.identity));
                let mut terminal = result.termination_reason.clone();
                let is_dop = matches!(plan.request.encoding, Encoding::DoP);
                let expected_rejected = result.admission.as_deref() == Some("expected_rejected");
                let dop_unconfirmed = is_dop
                    && result.admission.as_deref() == Some("unexpected_admitted")
                    && result.status == "passed_usb"
                    && result.close_outcome["status"] != "failed";
                let should_fallback = (expected_rejected || dop_unconfirmed)
                    && terminal.is_none()
                    && (!is_dop || !dop_fallback_requests.contains(&plan.request));
                let checkpoint = result.admission.as_deref() == Some("unexpected_admitted")
                    || terminal.is_some()
                    || (index + 1) % 32 == 0
                    || should_fallback;
                report.admission_results.push(result);
                report.unattempted_admission_plan =
                    unattempted_plan(&report.admission_plan, report.admission_results.len());
                if checkpoint {
                    self.save(&report);
                }
                if should_fallback {
                    if is_dop {
                        dop_fallback_requests.push(plan.request);
                    }
                    let p = Progress {
                        index,
                        total: probes.len(),
                        phase: "fallback".into(),
                        leg: format!("{:?}", plan.request),
                    };
                    progress(&p);
                    self.record("progress", json!(p));
                    let mut fallback =
                        run_fallback(dac, &caps, &plan.request, &options, cancel, device_lost);
                    fallback["trigger"] = json!(if expected_rejected {
                        "exact_unsupported"
                    } else {
                        "dop_decoder_unconfirmed"
                    });
                    self.record("fallback_finished", fallback_summary(&fallback));
                    if let Some(reason) = fallback.get("termination_reason").and_then(Value::as_str)
                    {
                        terminal = Some(reason.into());
                    }
                    report.fallback_results.push(fallback);
                    self.save(&report);
                }
                if terminal.is_some() {
                    report.termination_reason = terminal;
                    break;
                }
            }
        }
        if report.termination_reason.as_deref() != Some("device_lost") {
            report.termination_reason = stop_reason(cancel, device_lost)
                .map(str::to_owned)
                .or(report.termination_reason);
        }
        report.unattempted_plan = unattempted_plan(&report.plan, report.legs.len());
        report.cancelled = report.termination_reason.as_deref() == Some("cancelled");
        if report.termination_reason.as_deref() == Some("device_lost") {
            report.status = "device_lost".into();
        } else if report.cancelled {
            report.status = "cancelled".into();
        } else if report.termination_reason.as_deref() == Some("retirement_failed")
            || !required_fallbacks_verified(&report)
            || report.legs.len() != plans.len()
            || report.admission_results.len() != probes.len()
        {
            report.status = "failed".into();
        } else if plans.is_empty()
            && report
                .admission_results
                .iter()
                .all(|r| r.admission.as_deref() == Some("expected_rejected"))
            && report
                .fallback_results
                .iter()
                .all(|r| !matches!(r["status"].as_str(), Some("failed" | "inconclusive")))
        {
            report.status = "no_cases".into();
        } else if !plans.is_empty() || !probes.is_empty() {
            report.status = if report.legs.iter().all(|l| l.status == "passed_usb")
                && report
                    .admission_results
                    .iter()
                    .all(|r| r.admission.as_deref() == Some("expected_rejected"))
                && report
                    .fallback_results
                    .iter()
                    .all(|r| !matches!(r["status"].as_str(), Some("failed" | "inconclusive")))
            {
                "passed_usb"
            } else {
                "completed_with_findings"
            }
            .into();
        }
        progress(&Progress {
            index: report.legs.len(),
            total: plans.len(),
            phase: "finished".into(),
            leg: String::new(),
        });
        self.save(&report);
        report
    }
    fn save(&self, report: &Report) {
        let result = (|| -> io::Result<()> {
            let data = serde_json::to_vec_pretty(report)?;
            let tmp = self.dir.join("report.json.tmp");
            fs::write(&tmp, data)?;
            fs::rename(tmp, self.dir.join("report.json"))
        })();
        if let Err(e) = result {
            *self.io_error.lock().unwrap() = Some(format!("report: {e}"));
        }
    }
    pub fn export_zip(
        &self,
        path: impl AsRef<Path>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.record("export_requested", json!({}));
        let mut warnings = Vec::new();
        if let Err(error) = self.flush_debug() {
            warnings.push(format!("capture flush: {error}"));
            self.record(
                "capture_warning",
                json!({"message":format!("capture flush: {error}")}),
            );
        }
        if let Err(error) = write_html_report(&self.dir) {
            let _ = fs::remove_file(self.dir.join("report.html"));
            warnings.push(format!("HTML report unavailable: {error}"));
        }
        let destination = path.as_ref();
        let parent = destination
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        static EXPORT_ID: AtomicU64 = AtomicU64::new(0);
        let staging = parent.join(format!(
            ".usbdac-export-{}-{}.tmp",
            std::process::id(),
            EXPORT_ID.fetch_add(1, Ordering::Relaxed)
        ));
        // Do not remove a pre-existing path if create_new fails.
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&staging)?;
        let write_result = (|| -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
            let mut zip = zip::ZipWriter::new(file);
            let options = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated);
            let mut buffer = [0u8; 32 * 1024];
            for name in [
                "timeline.jsonl",
                "debug.jsonl",
                "report.json",
                "report.html",
            ] {
                let mut source = match File::open(self.dir.join(name)) {
                    Ok(file) => file,
                    Err(error)
                        if (name == "report.json" || name == "report.html")
                            && error.kind() == io::ErrorKind::NotFound =>
                    {
                        continue
                    }
                    Err(error) => {
                        warnings.push(format!("{name} unavailable: {error}"));
                        continue;
                    }
                };
                zip.start_file(name, options)?;
                loop {
                    match source.read(&mut buffer) {
                        Ok(0) => break,
                        Ok(count) => zip.write_all(&buffer[..count])?,
                        Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                        Err(error) => {
                            warnings.push(format!("{name} read incomplete: {error}"));
                            break;
                        }
                    }
                }
            }
            zip.start_file("capture-status.json", options)?;
            serde_json::to_writer_pretty(
                &mut zip,
                &json!({"partial": !warnings.is_empty(), "warnings": warnings}),
            )?;
            zip.start_file("README.txt", options)?;
            zip.write_all(b"USB transport evidence is not proof of audible output or DoP decoding. Timeline and report may contain device identifiers. See capture-status.json for missing or incomplete evidence.\n")?;
            zip.finish()?.sync_all()?;
            fs::rename(&staging, destination)?;
            Ok(())
        })();
        if write_result.is_err() {
            let _ = fs::remove_file(&staging);
        }
        write_result
    }

    fn run_leg(
        &self,
        dac: &Dac,
        plan: &Plan,
        options: &RunOptions,
        cancel: &AtomicBool,
        device_lost: &AtomicBool,
    ) -> LegResult {
        let mut result = LegResult {
            request: format!("{:?}", plan.request),
            advertised_alt: plan.alt.clone(),
            negotiated: None,
            status: "failed".into(),
            admission: None,
            error: None,
            termination_reason: None,
            error_stage: None,
            error_code: None,
            close_outcome: json!({"status":"not_opened"}),
            payload_sha256: None,
            payload_bytes: 0,
            evidence: json!({}),
        };
        let format = match dac.negotiate(&plan.request) {
            Ok(f) => f,
            Err(e) => {
                if is_device_lost(&e) {
                    mark_stop(&mut result, "device_lost", "negotiate");
                    return result;
                }
                if plan.probe {
                    let classification = admission_error(&e);
                    result.admission = Some(classification.into());
                    result.status = classification.into();
                    result.error_stage = Some("negotiate".into());
                    result.error_code = Some(e.code().into());
                    result.error = Some(e.to_string());
                    return result;
                }
                result.status = if e.code() == "unsupported" {
                    "unsupported"
                } else if e.code() == "clock_unverified" {
                    "unverified"
                } else {
                    "failed"
                }
                .into();
                result.error_stage = Some("negotiate".into());
                result.error_code = Some(e.code().into());
                result.error = Some(e.to_string());
                return result;
            }
        };
        result.negotiated = Some(format!("{format:#?}"));
        result.evidence["format"] = format_json(&format);
        if !exact_format(&plan.request, &format) {
            if plan.probe {
                result.admission = Some("inconclusive".into());
                result.status = "inconclusive".into();
            } else {
                result.status = "failed".into();
            }
            result.error_stage = Some("negotiate".into());
            result.error_code = Some("format_drift".into());
            result.error = Some(
                "Negotiated format differs from exact request; playback was not attempted".into(),
            );
            return result;
        }
        if let Some(reason) = stop_reason(cancel, device_lost) {
            mark_stop(&mut result, reason, "negotiate");
            return result;
        }
        if plan.probe {
            result.admission = Some("unexpected_admitted".into());
        }
        if !plan.probe
            && (
                format.interface_number,
                format.alt_setting,
                format.subslot_bytes,
                format.valid_bits,
            ) != plan.identity
        {
            result.status = "not_covered".into();
            result.error = Some("Exact request selected a different alternate; this advertised alternate was not exercised".into());
            return result;
        }
        let events = Arc::new(Mutex::new(Vec::<String>::new()));
        let sink = events.clone();
        let event_lost = Arc::new(AtomicBool::new(false));
        let lost_sink = event_lost.clone();
        let stream = match dac.open_stream(
            &format,
            Arc::new(move |e: DacEvent| {
                if matches!(&e, DacEvent::DeviceLost)
                    || matches!(&e, DacEvent::Error(error) if is_device_lost(error))
                {
                    lost_sink.store(true, Ordering::Release);
                }
                if let Ok(mut events) = sink.try_lock() {
                    if events.len() < 1024 {
                        events.push(format!("{e:?}"));
                    }
                }
            }),
        ) {
            Ok(s) => s,
            Err(e) => {
                if is_device_lost(&e) {
                    mark_stop(&mut result, "device_lost", "open");
                    return result;
                }
                result.error_stage = Some("open".into());
                result.error_code = Some(e.code().into());
                result.status = if e.code() == "unsupported" {
                    "unsupported"
                } else if e.code() == "clock_unverified" {
                    "unverified"
                } else {
                    "failed"
                }
                .into();
                result.error = Some(e.to_string());
                return result;
            }
        };
        let deadline = Instant::now() + Duration::from_millis(options.leg_timeout_ms.max(100));
        let mut generator = Signal::new(&format);
        let mut hash = Sha256::new();
        let frames = u64::from(format.sample_rate) * options.duration_ms.clamp(300, 10000) / 1000;
        let mut remaining = frames;
        let mut pending = Vec::new();
        let mut offset = 0;
        let mut prefix = Vec::new();
        while remaining > 0 || offset < pending.len() {
            if event_lost.load(Ordering::Acquire) {
                mark_stop(&mut result, "device_lost", "stream");
                break;
            }
            if let Some(reason) = stop_reason(cancel, device_lost) {
                mark_stop(&mut result, reason, "stream");
                break;
            }
            if Instant::now() >= deadline {
                result.error_stage = Some("write".into());
                result.error_code = Some("timeout".into());
                result.error = Some("write deadline exceeded".into());
                break;
            }
            if offset == pending.len() {
                let count = remaining.min(256) as usize;
                pending = generator.frames(count);
                offset = 0;
                remaining -= count as u64;
            }
            match stream.write(&pending[offset..]) {
                Ok(0) => std::thread::sleep(Duration::from_millis(2)),
                Ok(n) => {
                    if prefix.len() < 64 {
                        let count = n.min(64 - prefix.len());
                        prefix.extend_from_slice(&pending[offset..offset + count]);
                    }
                    hash.update(&pending[offset..offset + n]);
                    result.payload_bytes += n as u64;
                    offset += n;
                }
                Err(e) => {
                    if is_device_lost(&e) {
                        mark_stop(&mut result, "device_lost", "write");
                        break;
                    }
                    result.error_stage = Some("write".into());
                    result.error_code = Some(e.code().into());
                    result.error = Some(e.to_string());
                    break;
                }
            }
        }
        if result.error.is_none() {
            if let Err(e) = stream.drain() {
                if is_device_lost(&e) {
                    mark_stop(&mut result, "device_lost", "drain");
                } else {
                    result.error_stage = Some("drain".into());
                    result.error_code = Some(e.code().into());
                    result.error = Some(e.to_string());
                }
            }
        }
        while result.error.is_none() && !stream.is_drained() {
            if event_lost.load(Ordering::Acquire) {
                mark_stop(&mut result, "device_lost", "stream");
                break;
            }
            if let Some(reason) = stop_reason(cancel, device_lost) {
                mark_stop(&mut result, reason, "stream");
                break;
            }
            if Instant::now() >= deadline {
                result.error_stage = Some("drain".into());
                result.error_code = Some("timeout".into());
                result.error = Some("drain deadline exceeded".into());
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        let stats = stream.stats();
        let feedback = stream.explicit_feedback_diagnostics();
        result.payload_sha256 = Some(format!("{:x}", hash.finalize()));
        let prefix_hex: String = prefix.iter().map(|b| format!("{b:02x}")).collect();
        result.evidence = json!({"format": format_json(&format), "payload_prefix_hex": prefix_hex, "stats": {"bit_perfect": stats.bit_perfect, "payload_verified": stats.payload_verified, "clock_verified": stats.clock_verified, "underruns": stats.underruns, "lost_frames": stats.lost_frames, "short_packets": stats.short_packets, "zero_packets": stats.zero_packets, "urb_timeouts": stats.urb_timeouts, "frames_written": stats.frames_written, "played_frames": stats.played_frames, "completed_transfers": stats.completed_transfers, "feedback_reports": stats.feedback_reports, "feedback_rate_hz": stats.feedback_rate_hz, "feedback_locked": stats.feedback_locked, "debug": format!("{stats:#?}")}, "feedback": format!("{feedback:#?}"), "dac_feedback_trace": format!("{:?}", dac.feedback_trace()), "events": events.lock().map(|v| v.clone()).unwrap_or_default(), "drained": stream.is_drained(), "counter_units": "PCM and DoP: wire frames; native DSD: one byte per channel", "host_enqueue_is_delivery": false, "usb_transport_only": true, "dop_decoder_lock": "not_observed"});
        if result.error.is_none()
            && stats.bit_perfect
            && stats.payload_verified
            && stats.clock_verified
            && stats.lost_frames == 0
            && stats.underruns == 0
            && stats.short_packets == 0
            && stats.zero_packets == 0
            && stats.urb_timeouts == 0
            && stats.played_frames == stats.frames_written
        {
            result.status = "passed_usb".into();
        } else if result.error.is_none() {
            result.status = "unverified".into();
            result.error_stage = Some("evidence".into());
            result.error_code = Some("unverified".into());
            result.error = Some(
                "Strict lossless, payload, clock or bit-perfect evidence was not established"
                    .into(),
            );
        }
        match stream.close() {
            Ok(()) => result.close_outcome = json!({"status": "closed"}),
            Err(e) => {
                result.close_outcome =
                    json!({"status": "failed", "code": e.code(), "message": e.to_string()});
                if result.termination_reason.is_none() {
                    result.status = "failed".into();
                }
                if is_device_lost(&e) {
                    mark_stop(&mut result, "device_lost", "close");
                }
            }
        }
        result.evidence["cleanup"] = result.close_outcome["status"].clone();
        if event_lost.load(Ordering::Acquire) || device_lost.load(Ordering::Acquire) {
            mark_stop(&mut result, "device_lost", "stream");
        }
        result.evidence["dac_feedback_trace"] = dac.feedback_trace().map(|trace| json!({
            "format": format_json(&trace.format), "dropped_entries": trace.dropped_entries,
            "failure": trace.failure, "explicit": format!("{:?}", trace.explicit),
            "entries": trace.entries.iter().map(|entry| json!({"elapsed_us": entry.elapsed_us, "event": format!("{:?}", entry.event)})).collect::<Vec<_>>()
        })).unwrap_or(Value::Null);

        result
    }
}
/// Exact refusals remain independently recorded; overall success additionally
/// requires every requested fallback to be present and independently verified.
fn required_fallbacks_verified(report: &Report) -> bool {
    let verified = |fallback: &Value| {
        fallback.get("status").and_then(Value::as_str) == Some("passed_usb")
            && fallback
                .pointer("/selection_verification/passed")
                .and_then(Value::as_bool)
                == Some(true)
            && fallback
                .pointer("/conversion_verification/passed")
                .and_then(Value::as_bool)
                == Some(true)
            && fallback
                .get("target_wire_integrity")
                .and_then(Value::as_str)
                == Some("verified")
    };
    if !report.fallback_results.iter().all(verified) {
        return false;
    }
    let required = report
        .legs
        .iter()
        .filter(|leg| {
            leg.status == "unsupported" && leg.error_stage.as_deref() == Some("negotiate")
        })
        .chain(
            report
                .admission_results
                .iter()
                .filter(|leg| leg.admission.as_deref() == Some("expected_rejected")),
        );
    required.into_iter().all(|leg| {
        report.fallback_results.iter().any(|fallback| {
            fallback.get("source_request").and_then(Value::as_str) == Some(leg.request.as_str())
                && verified(fallback)
        })
    })
}
fn fallback_summary(value: &Value) -> Value {
    json!({"trigger": value.get("trigger"), "source_request": value.get("source_request"), "target_request": value.get("target_request"), "wire_format": value.get("wire_format"), "status": value.get("status"), "termination_reason": value.get("termination_reason"), "conversion": value.get("conversion"), "conversion_verification": value.get("conversion_verification"), "selection_verification": value.get("selection_verification"), "source_fidelity": value.get("source_fidelity"), "target_wire_integrity": value.get("target_wire_integrity"), "error": value.get("error"), "reason": value.get("reason")})
}
fn leg_summary(leg: &LegResult, advertised_identity: (u8, u8, u8, u8)) -> Value {
    let stats = &leg.evidence["stats"];
    let trace = &leg.evidence["dac_feedback_trace"];
    json!({
        "request": leg.request,
        "advertised_identity": {"interface_number": advertised_identity.0, "alt_setting": advertised_identity.1, "subslot_bytes": advertised_identity.2, "valid_bits": advertised_identity.3},
        "format": leg.evidence.get("format"),
        "status": leg.status,
        "admission": leg.admission,
        "termination_reason": leg.termination_reason,
        "error": leg.error,
        "error_stage": leg.error_stage,
        "error_code": leg.error_code,
        "close_outcome": leg.close_outcome,
        "payload_bytes": leg.payload_bytes,
        "payload_sha256": leg.payload_sha256,
        "stats": {
            "bit_perfect": stats.get("bit_perfect"),
            "payload_verified": stats.get("payload_verified"),
            "clock_verified": stats.get("clock_verified"),
            "underruns": stats.get("underruns"),
            "lost_frames": stats.get("lost_frames"),
            "short_packets": stats.get("short_packets"),
            "zero_packets": stats.get("zero_packets"),
            "urb_timeouts": stats.get("urb_timeouts"),
            "frames_written": stats.get("frames_written"),
            "played_frames": stats.get("played_frames"),
            "completed_transfers": stats.get("completed_transfers"),
            "feedback_reports": stats.get("feedback_reports"),
            "feedback_rate_hz": stats.get("feedback_rate_hz"),
            "feedback_locked": stats.get("feedback_locked"),
        },
        "feedback_trace": {
            "entry_count": trace.get("entries").and_then(Value::as_array).map(Vec::len),
            "dropped_entries": trace.get("dropped_entries"),
            "failure": trace.get("failure"),
        }
    })
}
fn unattempted_plan(plan: &[Value], attempted: usize) -> Vec<Value> {
    plan.iter().skip(attempted).cloned().collect()
}
fn is_device_lost(error: &usbdac_core::Error) -> bool {
    matches!(
        error,
        usbdac_core::Error::DeviceLost
            | usbdac_core::Error::HostUsb {
                kind: usbdac_core::HostUsbFailureKind::DeviceLost,
                ..
            }
    )
}
fn stop_reason(cancel: &AtomicBool, device_lost: &AtomicBool) -> Option<&'static str> {
    if device_lost.load(Ordering::Acquire) {
        Some("device_lost")
    } else if cancel.load(Ordering::Acquire) {
        Some("cancelled")
    } else {
        None
    }
}
fn mark_stop(result: &mut LegResult, reason: &str, stage: &str) {
    result.status = reason.into();
    result.termination_reason = Some(reason.into());
    result.error_stage = Some(stage.into());
    result.error_code = Some(reason.into());
    result.error = Some(
        if reason == "device_lost" {
            "Selected device was lost"
        } else {
            "cancelled"
        }
        .into(),
    );
}
fn format_json(f: &StreamFormat) -> Value {
    json!({"encoding": format!("{:?}", f.encoding), "sample_rate": f.sample_rate, "counter_rate_hz": f.counter_rate_hz(), "channels": f.channels, "valid_bits": f.valid_bits, "subslot_bytes": f.subslot_bytes, "interface_number": f.interface_number, "alt_setting": f.alt_setting, "endpoint_address": f.endpoint_address, "service_interval_us": f.service_interval_us, "explicit_feedback": f.explicit_feedback, "pacing": format!("{:?}", f.pacing), "clock": f.clock.as_ref().map(|clock| json!({"control_interface":clock.control_interface, "source_id":clock.source_id, "resolution":clock.resolution.code(), "path":clock.path.iter().map(|hop| json!({"id":hop.id, "kind":hop.kind.code(), "detail":format!("{:?}",hop.kind)})).collect::<Vec<_>>() }))})
}

fn unix_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}
#[derive(Clone)]
struct Plan {
    request: StreamRequest,
    alt: String,
    identity: (u8, u8, u8, u8),
    probe: bool,
}
fn native_dsd_rate(wire_rate: u32, word_bytes: u8) -> Option<u32> {
    if word_bytes == 0 {
        return None;
    }
    wire_rate
        .checked_mul(u32::from(word_bytes) * 8)
        .filter(|rate| *rate >= 2_822_400 && rate & 31 == 0)
}
fn mode(encoding: &Encoding) -> &'static str {
    match encoding {
        Encoding::DoP => "dop",
        Encoding::DsdNative { .. } => "native_dsd",
        _ => "pcm",
    }
}
fn request_rate(request: &StreamRequest) -> u32 {
    match request.encoding {
        Encoding::DsdNative { bit_rate } => bit_rate,
        Encoding::DoP => request.sample_rate.saturating_mul(16),
        _ => request.sample_rate,
    }
}
fn mode_coverage(plans: &[Plan], legs: &[LegResult]) -> Value {
    let mut result = serde_json::Map::new();
    for name in ["pcm", "dop", "native_dsd"] {
        let matching: Vec<_> = plans
            .iter()
            .enumerate()
            .filter(|(_, p)| mode(&p.request.encoding) == name)
            .collect();
        let planned = matching.len();
        let attempted = matching
            .iter()
            .filter(|(index, _)| *index < legs.len())
            .count();
        let exercised = matching
            .iter()
            .filter(|(index, _)| {
                legs.get(*index)
                    .is_some_and(|leg| leg.close_outcome["status"] != "not_opened")
            })
            .count();
        let passed = matching
            .iter()
            .filter(|(index, _)| {
                legs.get(*index)
                    .is_some_and(|leg| leg.status == "passed_usb")
            })
            .count();
        let rates: std::collections::BTreeSet<_> = matching
            .iter()
            .map(|(_, p)| request_rate(&p.request))
            .collect();
        let state = if planned == 0 {
            "no_admitted_cases"
        } else if exercised == 0 {
            "not_exercised"
        } else if exercised < planned {
            "partially_exercised"
        } else {
            "exercised"
        };
        let mut entry = json!({"planned": planned, "attempted": attempted, "exercised": exercised, "passed": passed, "status": state, "rates_hz": rates, "rate_unit": if name == "pcm" { "pcm_samples_per_second" } else { "dsd_bits_per_second" }});
        if name == "dop" {
            entry["decoder"] = json!("not_observed");
        }
        result.insert(name.into(), entry);
    }
    Value::Object(result)
}
fn matrix(caps: &Capabilities) -> Vec<Plan> {
    let mut plans = Vec::new();
    for alt in &caps.alt_settings {
        let mut rates = alt.sample_rates.clone();
        for r in &alt.rate_ranges {
            rates.push(r.min_hz);
            rates.push(r.max_hz);
        }
        if alt.format == AltFormat::Dsd && rates.is_empty() && alt.subslot_bytes > 0 {
            for bit_rate in [2_822_400u32, 5_644_800, 11_289_600, 22_579_200] {
                let bits = u32::from(alt.subslot_bytes) * 8;
                if bit_rate % bits == 0 {
                    rates.push(bit_rate / bits);
                }
            }
        }
        let standards: Vec<u32> = if alt.format == AltFormat::Dsd {
            DSD_RATES
                .iter()
                .filter_map(|rate| {
                    let bits = u32::from(alt.subslot_bytes) * 8;
                    if bits > 0 && rate % bits == 0 {
                        Some(rate / bits)
                    } else {
                        None
                    }
                })
                .collect()
        } else {
            PCM_RATES.to_vec()
        };
        for rate in standards {
            if alt.rate_ranges.iter().any(|r| range_contains(r, rate)) {
                rates.push(rate);
            }
        }
        rates.sort_unstable();
        rates.dedup();
        for rate in rates {
            if rate == 0 || alt.channels == 0 {
                continue;
            }
            let req = match alt.format {
                AltFormat::Pcm if [16, 24, 32].contains(&alt.valid_bits) => {
                    Some(StreamRequest::pcm(rate, alt.channels, alt.valid_bits))
                }
                AltFormat::PcmU8 => Some(StreamRequest::pcm_u8(rate, alt.channels)),
                AltFormat::PcmFloat32 => Some(StreamRequest::pcm_float32(rate, alt.channels)),
                AltFormat::Dsd => native_dsd_rate(rate, alt.subslot_bytes)
                    .map(|r| StreamRequest::dsd_native(r, alt.channels)),
                _ => None,
            };
            let identity = (
                alt.interface_number,
                alt.alt_setting,
                alt.subslot_bytes,
                alt.valid_bits,
            );
            if let Some(request) = req {
                plans.push(Plan {
                    request,
                    alt: format!("{alt:?}"),
                    identity,
                    probe: false,
                });
            }
            if matches!(alt.format, AltFormat::Pcm | AltFormat::Dop)
                && [24, 32].contains(&alt.valid_bits)
                && DSD_RATES.iter().any(|dsd| *dsd / 16 == rate)
                && (u64::from(rate) * u64::from(alt.service_interval_us)).div_ceil(1_000_000)
                    * u64::from(alt.channels)
                    * u64::from(alt.subslot_bytes)
                    <= alt.max_packet_bytes as u64
            {
                if let Some(dsd) = rate.checked_mul(16) {
                    let mut request = StreamRequest::dop(dsd, alt.channels);
                    request.bit_depth = alt.valid_bits;
                    plans.push(Plan {
                        request,
                        alt: format!("{alt:?}"),
                        identity,
                        probe: false,
                    });
                }
            }
        }
    }
    plans
}
const PCM_RATES: &[u32] = &[
    8_000, 11_025, 12_000, 16_000, 22_050, 24_000, 32_000, 44_100, 48_000, 88_200, 96_000, 176_400,
    192_000, 352_800, 384_000, 705_600, 768_000, 1_411_200, 1_536_000,
];
const DSD_RATES: &[u32] = &[
    2_822_400, 3_072_000, 5_644_800, 6_144_000, 11_289_600, 12_288_000, 22_579_200, 24_576_000,
];
fn range_contains(r: &usbdac_core::RateRange, rate: u32) -> bool {
    rate >= r.min_hz
        && rate <= r.max_hz
        && (r.step_hz == 0 || (rate - r.min_hz).checked_rem(r.step_hz) == Some(0))
}
fn admission_error(error: &usbdac_core::Error) -> &'static str {
    if matches!(error, usbdac_core::Error::Unsupported { .. }) {
        "expected_rejected"
    } else {
        "inconclusive"
    }
}
fn exact_format(request: &StreamRequest, format: &StreamFormat) -> bool {
    request.encoding == format.encoding
        && request.channels == format.channels
        && (matches!(request.encoding, Encoding::DsdNative { .. })
            || (request.sample_rate == format.sample_rate
                && request.bit_depth == format.valid_bits))
}
fn admission_matrix(caps: &Capabilities, advertised: &[Plan]) -> Vec<Plan> {
    let mut channels = caps.channel_counts.clone();
    channels.extend(caps.alt_settings.iter().map(|alt| alt.channels));
    channels.extend([1, 2]);
    channels.retain(|c| (1..=32).contains(c));
    channels.sort_unstable();
    channels.dedup();
    admission_requests(&channels, advertised)
}
fn admission_requests(channels: &[u16], advertised: &[Plan]) -> Vec<Plan> {
    let mut requests = Vec::new();
    for &channel in channels {
        for &rate in PCM_RATES {
            for depth in [16, 24, 32] {
                requests.push(StreamRequest::pcm(rate, channel, depth));
            }
            requests.push(StreamRequest::pcm_u8(rate, channel));
            requests.push(StreamRequest::pcm_float32(rate, channel));
        }
        for &rate in DSD_RATES {
            requests.push(StreamRequest::dsd_native(rate, channel));
            for depth in [24, 32] {
                let mut request = StreamRequest::dop(rate, channel);
                request.bit_depth = depth;
                requests.push(request);
            }
        }
    }
    requests
        .into_iter()
        .filter(|request| !advertised.iter().any(|plan| plan.request == *request))
        .map(|request| Plan {
            request,
            alt: "standard admission probe (not represented in advertised plan)".into(),
            identity: (0, 0, 0, 0),
            probe: true,
        })
        .collect()
}
struct Signal {
    encoding: Encoding,
    sample_rate: u32,
    channels: u16,
    subslot_bytes: u8,
    valid_bits: u8,
    frame: u64,
    integrator: Vec<f64>,
    phase: Vec<(f64, f64)>,
    step: f64,
    sin_step: f64,
    cos_step: f64,
}
impl Signal {
    fn new(format: &StreamFormat) -> Self {
        Self::from_spec(
            format.encoding,
            format.sample_rate,
            format.channels,
            format.subslot_bytes,
            format.valid_bits,
        )
    }
    fn from_spec(
        encoding: Encoding,
        sample_rate: u32,
        channels: u16,
        subslot_bytes: u8,
        valid_bits: u8,
    ) -> Self {
        let rate = match encoding {
            Encoding::DsdNative { bit_rate } => bit_rate,
            Encoding::DoP => sample_rate * 16,
            _ => sample_rate,
        };
        let step = std::f64::consts::TAU * TONE_HZ / rate as f64;
        let (sin_step, cos_step) = step.sin_cos();
        Self {
            step,
            sin_step,
            cos_step,
            encoding,
            sample_rate,
            channels,
            subslot_bytes,
            valid_bits,
            frame: 0,
            integrator: vec![0.0; channels as usize],
            phase: vec![(0.0, 1.0); channels as usize],
        }
    }
    fn bit(&mut self, channel: usize, tick: u64, _rate: u32) -> bool {
        let polarity = if channel & 1 == 0 { 1.0 } else { -1.0 };
        // Rebase periodically to avoid accumulated floating-point phase drift.
        if tick & 8191 == 0 {
            self.phase[channel] = (self.step * tick as f64).sin_cos();
        }
        let (sin, cos) = self.phase[channel];
        self.phase[channel] = (
            sin * self.cos_step + cos * self.sin_step,
            cos * self.cos_step - sin * self.sin_step,
        );
        let x = AMPLITUDE * polarity * sin;
        modulate(&mut self.integrator[channel], x)
    }
    fn dsd_byte(&mut self, channel: usize, start: u64, rate: u32) -> u8 {
        let mut b = 0;
        for i in 0..8 {
            b = (b << 1) | u8::from(self.bit(channel, start + i, rate));
        }
        b
    }
    fn frames(&mut self, frames: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(
            frames * usize::from(self.subslot_bytes) * usize::from(self.channels),
        );
        let channels = self.channels as usize;
        let sub = self.subslot_bytes as usize;
        for _ in 0..frames {
            match self.encoding {
                Encoding::DsdNative { bit_rate } => {
                    for byte in 0..sub {
                        for ch in 0..channels {
                            out.push(self.dsd_byte(
                                ch,
                                (self.frame * sub as u64 + byte as u64) * 8,
                                bit_rate,
                            ));
                        }
                    }
                }
                Encoding::DoP => {
                    for ch in 0..channels {
                        let hi = self.dsd_byte(ch, self.frame * 16, self.sample_rate * 16);
                        let lo = self.dsd_byte(ch, self.frame * 16 + 8, self.sample_rate * 16);
                        if sub == 4 {
                            out.push(0);
                        }
                        out.extend_from_slice(&[lo, hi, 0]);
                    }
                }
                enc => {
                    for ch in 0..channels {
                        let polarity = if ch % 2 == 0 { 1.0 } else { -1.0 };
                        let x = AMPLITUDE
                            * polarity
                            * (std::f64::consts::TAU * TONE_HZ * self.frame as f64
                                / self.sample_rate as f64)
                                .sin();
                        match enc {
                            Encoding::PcmU8 => {
                                out.push((128.0 + x * 127.0).round().clamp(0.0, 255.0) as u8)
                            }
                            Encoding::PcmFloat32 => {
                                out.extend_from_slice(&(x as f32).to_le_bytes())
                            }
                            _ => {
                                let bits = self.valid_bits;
                                let value = (x * ((1u64 << (bits - 1)) - 1) as f64).round() as i64;
                                let shifted = value << (sub * 8 - bits as usize);
                                out.extend_from_slice(&shifted.to_le_bytes()[..sub]);
                            }
                        }
                    }
                }
            }
            self.frame += 1;
        }
        out
    }
}

fn modulate(integrator: &mut f64, input: f64) -> bool {
    let bit = *integrator >= 0.0;
    *integrator += input - if bit { 1.0 } else { -1.0 };
    bit
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn delta_sigma_known_zero_vector_is_msb_first() {
        let mut state = 0.0;
        let mut byte = 0u8;
        for _ in 0..8 {
            byte = (byte << 1) | u8::from(modulate(&mut state, 0.0));
        }
        assert_eq!(byte, 0xaa);
    }
    #[test]
    fn independent_correlator_recovers_frequency_and_polarity() {
        let rate = 2_822_400.0;
        let n = 282_240;
        let mut states = [0.0, 0.0];
        let mut sums = [0.0, 0.0];
        for i in 0..n {
            let reference = (std::f64::consts::TAU * TONE_HZ * i as f64 / rate).sin();
            for ch in 0..2 {
                let input = AMPLITUDE * reference * if ch == 0 { 1.0 } else { -1.0 };
                let decoded = if modulate(&mut states[ch], input) {
                    1.0
                } else {
                    -1.0
                };
                sums[ch] += decoded * reference;
            }
        }
        let amplitudes = sums.map(|sum| 2.0 * sum / n as f64);
        assert!((amplitudes[0] - AMPLITUDE).abs() < 0.001, "{amplitudes:?}");
        assert!((amplitudes[1] + AMPLITUDE).abs() < 0.001, "{amplitudes:?}");
    }
    fn signal(encoding: Encoding, rate: u32, sub: u8, bits: u8) -> Signal {
        let bit_rate = match encoding {
            Encoding::DsdNative { bit_rate } => bit_rate,
            Encoding::DoP => rate * 16,
            _ => rate,
        };
        let step = std::f64::consts::TAU * TONE_HZ / bit_rate as f64;
        let (sin_step, cos_step) = step.sin_cos();
        Signal {
            encoding,
            sample_rate: rate,
            channels: 2,
            subslot_bytes: sub,
            valid_bits: bits,
            frame: 0,
            integrator: vec![0.0; 2],
            phase: vec![(0.0, 1.0); 2],
            step,
            sin_step,
            cos_step,
        }
    }
    #[test]
    fn generated_pcm_samples_decode_in_wire_order() {
        for (encoding, sub, bits) in [
            (Encoding::PcmU8, 1, 8),
            (Encoding::PcmFloat32, 4, 32),
            (Encoding::Pcm, 2, 16),
            (Encoding::Pcm, 3, 24),
            (Encoding::Pcm, 4, 24),
            (Encoding::Pcm, 4, 32),
        ] {
            let bytes = signal(encoding, 48000, sub, bits).frames(100);
            assert_eq!(bytes.len(), 100 * 2 * sub as usize);
            for frame in 0..100 {
                for channel in 0..2 {
                    let start = (frame * 2 + channel) * sub as usize;
                    let b = &bytes[start..start + sub as usize];
                    let actual = match encoding {
                        Encoding::PcmU8 => (f64::from(b[0]) - 128.0) / 127.0,
                        Encoding::PcmFloat32 => f32::from_le_bytes(b.try_into().unwrap()) as f64,
                        _ => {
                            let mut a = [if b[sub as usize - 1] & 128 != 0 {
                                255
                            } else {
                                0
                            }; 8];
                            a[..sub as usize].copy_from_slice(b);
                            (i64::from_le_bytes(a) >> (sub * 8 - bits)) as f64
                                / ((1u64 << (bits - 1)) - 1) as f64
                        }
                    };
                    let expected = AMPLITUDE
                        * if channel == 0 { 1.0 } else { -1.0 }
                        * (std::f64::consts::TAU * TONE_HZ * frame as f64 / 48000.0).sin();
                    assert!((actual - expected).abs() < if bits == 8 { 0.0041 } else { 0.00004 });
                    if sub == 4 && bits == 24 {
                        assert_eq!(b[0], 0);
                    }
                }
            }
        }
    }
    #[test]
    fn generated_dsd_and_dop_decode_chronological_bits_per_channel() {
        for (encoding, rate, sub) in [
            (Encoding::DsdNative { bit_rate: 2822400 }, 176400, 2),
            (Encoding::DoP, 176400, 3),
            (Encoding::DoP, 176400, 4),
        ] {
            let bytes = signal(encoding, rate, sub, if sub == 4 { 32 } else { 24 }).frames(64);
            let mut states = [0.0, 0.0];
            for frame in 0..64 {
                for ch in 0..2 {
                    let mut expected = [0u8; 2];
                    for bit in 0..16 {
                        let tick = frame * 16 + bit;
                        let x = AMPLITUDE
                            * if ch == 0 { 1.0 } else { -1.0 }
                            * (std::f64::consts::TAU * TONE_HZ * tick as f64 / 2822400.0).sin();
                        expected[bit / 8] =
                            (expected[bit / 8] << 1) | u8::from(modulate(&mut states[ch], x));
                    }
                    match encoding {
                        Encoding::DsdNative { .. } => {
                            assert_eq!(bytes[frame * 4 + ch], expected[0]);
                            assert_eq!(bytes[frame * 4 + 2 + ch], expected[1]);
                        }
                        _ => {
                            let off = (frame * 2 + ch) * sub as usize;
                            let pad = usize::from(sub == 4);
                            if pad == 1 {
                                assert_eq!(bytes[off], 0);
                            }
                            assert_eq!(
                                &bytes[off + pad..off + pad + 3],
                                &[expected[1], expected[0], 0]
                            );
                        }
                    }
                }
            }
        }
    }
    #[test]
    fn native_word_rates_filter_invalid_values_and_include_dsd256() {
        assert_eq!(native_dsd_rate(44_100, 4), None);
        assert_eq!(native_dsd_rate(352_801, 1), None);
        assert_eq!(native_dsd_rate(88_200, 4), Some(2_822_400));
        assert_eq!(native_dsd_rate(705_600, 4), Some(22_579_200));
        assert_eq!(native_dsd_rate(u32::MAX, 4), None);
        assert_eq!(native_dsd_rate(88_200, 0), None);
    }
    #[test]
    fn required_fallback_requires_selection_conversion_and_usb_proof() {
        let rejected = LegResult {
            request: "source".into(),
            advertised_alt: String::new(),
            negotiated: None,
            status: "expected_rejected".into(),
            admission: Some("expected_rejected".into()),
            error: None,
            termination_reason: None,
            error_stage: Some("negotiate".into()),
            error_code: Some("unsupported".into()),
            close_outcome: json!({"status":"not_opened"}),
            payload_sha256: None,
            payload_bytes: 0,
            evidence: json!({}),
        };
        let mut report = Report {
            schema_version: 1,
            started_unix_ms: 0,
            coverage: String::new(),
            signal: Value::Null,
            capabilities: Value::Null,
            mode_coverage: Value::Null,
            status: "running".into(),
            plan: vec![],
            unattempted_plan: vec![],
            admission_plan: vec![],
            unattempted_admission_plan: vec![],
            admission_results: vec![rejected],
            fallback_results: vec![],
            termination_reason: None,
            legs: vec![],
            cancelled: false,
            error: None,
        };
        assert!(!required_fallbacks_verified(&report));
        report
            .fallback_results
            .push(json!({"source_request":"source", "status":"no_fallback"}));
        assert!(!required_fallbacks_verified(&report));
        report.fallback_results[0] = json!({"source_request":"source", "status":"passed_usb", "selection_verification":{"passed":true}, "conversion_verification":{"passed":true}, "target_wire_integrity":"verified"});
        assert!(required_fallbacks_verified(&report));
        report.fallback_results[0]["selection_verification"]["passed"] = json!(false);
        assert!(!required_fallbacks_verified(&report));
        report.fallback_results[0]["selection_verification"]["passed"] = json!(true);
        report.fallback_results[0]["conversion_verification"]["passed"] = json!(false);
        assert!(!required_fallbacks_verified(&report));
        report.fallback_results[0]["conversion_verification"]["passed"] = json!(true);
        report.fallback_results[0]["target_wire_integrity"] = json!("unverified");
        assert!(!required_fallbacks_verified(&report));
    }
    #[test]
    fn mode_coverage_reports_admission_and_decoder_boundary() {
        let plans = vec![Plan {
            request: StreamRequest::dsd_native(22_579_200, 2),
            alt: String::new(),
            identity: (2, 4, 4, 1),
            probe: false,
        }];
        let value = mode_coverage(&plans, &[]);
        assert_eq!(value["pcm"]["status"], "no_admitted_cases");
        assert_eq!(value["dop"]["decoder"], "not_observed");
        assert_eq!(value["native_dsd"]["status"], "not_exercised");
        assert_eq!(value["native_dsd"]["rates_hz"][0], 22_579_200);
    }
    #[test]
    fn export_preserves_available_evidence_after_capture_failures() {
        let dir = std::env::temp_dir().join(format!(
            "usbdac-export-failure-{}-{}",
            std::process::id(),
            unix_ms()
        ));
        let capture = Capture::new(&dir).unwrap();
        capture.record("host_error", json!({"message": "device unavailable"}));
        capture.flush_debug().unwrap();
        fs::remove_file(dir.join("debug.jsonl")).unwrap();
        *capture.io_error.lock().unwrap() = Some("simulated debug flush failure".into());
        let path = dir.join("diagnostics.zip");
        capture.export_zip(&path).unwrap();
        let mut archive = zip::ZipArchive::new(File::open(path).unwrap()).unwrap();
        let mut timeline = String::new();
        std::io::Read::read_to_string(
            &mut archive.by_name("timeline.jsonl").unwrap(),
            &mut timeline,
        )
        .unwrap();
        assert!(timeline.contains("device unavailable"));
        let mut status = String::new();
        std::io::Read::read_to_string(
            &mut archive.by_name("capture-status.json").unwrap(),
            &mut status,
        )
        .unwrap();
        let status: Value = serde_json::from_str(&status).unwrap();
        assert_eq!(status["partial"], true);
        let warnings = status["warnings"].to_string();
        assert!(warnings.contains("simulated debug flush failure"));
        assert!(warnings.contains("debug.jsonl unavailable"));
        drop(archive);
        drop(capture);
        fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn standard_probes_deduplicate_and_classify_typed_results() {
        let advertised = vec![Plan {
            request: StreamRequest::pcm(44_100, 2, 16),
            alt: String::new(),
            identity: (1, 1, 2, 16),
            probe: false,
        }];
        let probes = admission_requests(&[1, 2], &advertised);
        assert!(!probes.iter().any(|p| p.request == advertised[0].request));
        assert!(probes
            .iter()
            .any(|p| p.request == StreamRequest::pcm_u8(48_000, 2)));
        for depth in [24, 32] {
            let mut request = StreamRequest::dop(24_576_000, 2);
            request.bit_depth = depth;
            assert!(probes.iter().any(|p| p.request == request));
        }
        assert_eq!(
            admission_error(&usbdac_core::Error::Unsupported {
                reason: String::new(),
                offered: vec![]
            }),
            "expected_rejected"
        );
        assert_eq!(
            admission_error(&usbdac_core::Error::InvalidArgument("x".into())),
            "inconclusive"
        );
    }
    #[test]
    fn continuous_and_stepped_ranges_admit_only_valid_standard_points() {
        let continuous = usbdac_core::RateRange {
            min_hz: 44_100,
            max_hz: 192_000,
            step_hz: 0,
        };
        assert!(range_contains(&continuous, 48_000));
        assert!(!range_contains(&continuous, 384_000));
        let stepped = usbdac_core::RateRange {
            min_hz: 48_000,
            max_hz: 192_000,
            step_hz: 48_000,
        };
        assert!(range_contains(&stepped, 96_000));
        assert!(!range_contains(&stepped, 88_200));
    }
    #[test]
    fn leg_timeline_summary_excludes_large_raw_trace() {
        let trace = vec![json!({"event": "feedback", "elapsed_us": 123}); 10_000];
        let leg = LegResult {
            request: "PCM 44100".into(),
            advertised_alt: "large descriptor".repeat(100),
            negotiated: None,
            status: "device_lost".into(),
            admission: None,
            error: Some("device lost".into()),
            termination_reason: Some("device_lost".into()),
            error_stage: Some("drain".into()),
            error_code: Some("device_lost".into()),
            close_outcome: json!({"status":"closed"}),
            payload_sha256: None,
            payload_bytes: 2048,
            evidence: json!({"format":{"sample_rate":44100,"interface_number":2,"alt_setting":1}, "stats":{"frames_written":512,"debug":"x".repeat(100_000)}, "dac_feedback_trace":{"entries":trace,"dropped_entries":4}}),
        };
        let summary = leg_summary(&leg, (2, 1, 3, 24));
        assert_eq!(summary["feedback_trace"]["entry_count"], 10_000);
        assert_eq!(summary["feedback_trace"]["dropped_entries"], 4);
        assert_eq!(summary["error_stage"], "drain");
        assert_eq!(summary["format"]["sample_rate"], 44100);
        assert!(serde_json::to_vec(&summary).unwrap().len() < 4096);
    }
    #[test]
    fn device_loss_is_terminal_and_takes_precedence_over_cancel() {
        let cancel = AtomicBool::new(false);
        let lost = AtomicBool::new(false);
        assert_eq!(stop_reason(&cancel, &lost), None);
        cancel.store(true, Ordering::Release);
        assert_eq!(stop_reason(&cancel, &lost), Some("cancelled"));
        lost.store(true, Ordering::Release);
        assert_eq!(stop_reason(&cancel, &lost), Some("device_lost"));
        assert!(is_device_lost(&usbdac_core::Error::DeviceLost));
        let mut leg = LegResult {
            request: "first".into(),
            advertised_alt: String::new(),
            negotiated: None,
            status: "failed".into(),
            admission: None,
            error: None,
            termination_reason: None,
            error_stage: None,
            error_code: None,
            close_outcome: json!({}),
            payload_sha256: None,
            payload_bytes: 0,
            evidence: json!({}),
        };
        mark_stop(&mut leg, "device_lost", "drain");
        assert_eq!(leg.termination_reason.as_deref(), Some("device_lost"));
        assert_eq!(leg.error_stage.as_deref(), Some("drain"));
        let plan = [json!({"rate_hz":44100}), json!({"rate_hz":96000})];
        let completed = [leg];
        let remaining = unattempted_plan(&plan, completed.len());
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0]["rate_hz"], 96000);
    }
    #[test]
    fn capture_exports_before_device() {
        let dir = std::env::temp_dir().join(format!(
            "usbdac-validator-test-{}-{}",
            std::process::id(),
            unix_ms()
        ));
        let capture = Capture::new(&dir).unwrap();
        capture.record("host_error", json!({"scope":"host", "message":"no device"}));
        let path = dir.join("diagnostics.zip");
        capture.export_zip(&path).unwrap();
        let mut archive = zip::ZipArchive::new(File::open(path).unwrap()).unwrap();
        assert!(archive.by_name("timeline.jsonl").is_ok());
        let mut status = String::new();
        archive
            .by_name("capture-status.json")
            .unwrap()
            .read_to_string(&mut status)
            .unwrap();
        let status: Value = serde_json::from_str(&status).unwrap();
        assert_eq!(status["partial"], false);
        assert_eq!(status["warnings"].as_array().unwrap().len(), 0);
        drop(archive);
        drop(capture);
        fs::remove_dir_all(dir).unwrap();
    }
}
