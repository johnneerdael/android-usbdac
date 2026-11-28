//! Offline, self-contained diagnostic report generated from captured evidence.
use serde_json::{json, Value};
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    path::Path,
    sync::atomic::{AtomicU64, Ordering},
};

const TEMPLATE: &str = include_str!("report_html/template.html");
const TOKENS: &str = include_str!("../../../branding/tokens/usbdac.css");
const MARK: &str = include_str!("../../../branding/logos/usbdac-mark-dark.svg");
const MARK_LIGHT: &str = include_str!("../../../branding/logos/usbdac-mark-light.svg");
const MANROPE: &[u8] = include_bytes!("../../../branding/fonts/Manrope.woff");
const MONO: &[u8] = include_bytes!("../../../branding/fonts/IBMPlexMono-Regular.woff");
const LICENSE_MANROPE: &str = include_str!("../../../branding/fonts/manrope-OFL.txt");
const LICENSE_MONO: &str = include_str!("../../../branding/fonts/ibmplexmono-OFL.txt");
const REPORT_LIMIT: u64 = 64 * 1024 * 1024;
const LOG_LIMIT: u64 = 8 * 1024 * 1024;
static ID: AtomicU64 = AtomicU64::new(0);

fn base64(bytes: &[u8]) -> String {
    const TABLE: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for c in bytes.chunks(3) {
        let a = c[0];
        let b = *c.get(1).unwrap_or(&0);
        let d = *c.get(2).unwrap_or(&0);
        out.push(TABLE[(a >> 2) as usize] as char);
        out.push(TABLE[(((a & 3) << 4) | (b >> 4)) as usize] as char);
        out.push(if c.len() > 1 {
            TABLE[(((b & 15) << 2) | (d >> 6)) as usize] as char
        } else {
            '='
        });
        out.push(if c.len() > 2 {
            TABLE[(d & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}
fn html_text(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}
fn escape_script(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    for ch in input.chars() {
        match ch {
            '&' => output.push_str("\\u0026"),
            '<' => output.push_str("\\u003c"),
            '>' => output.push_str("\\u003e"),
            '\u{2028}' => output.push_str("\\u2028"),
            '\u{2029}' => output.push_str("\\u2029"),
            _ => output.push(ch),
        }
    }
    output
}
fn script_json(value: &Value) -> io::Result<String> {
    Ok(escape_script(&serde_json::to_string(value)?))
}

fn read_bounded(path: &Path, cap: u64, warnings: &mut Vec<String>) -> Option<Vec<u8>> {
    let file = match File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return None,
        Err(e) => {
            warnings.push(format!("{} unavailable: {e}", path.display()));
            return None;
        }
    };
    let mut data = Vec::new();
    if let Err(e) = file.take(cap + 1).read_to_end(&mut data) {
        warnings.push(format!("{} could not be read: {e}", path.display()));
        return None;
    }
    if data.len() as u64 > cap {
        data.truncate(cap as usize);
        warnings.push(format!(
            "{} exceeds the report limit; evidence is truncated",
            path.file_name().unwrap_or_default().to_string_lossy()
        ));
    }
    Some(data)
}
fn json_file(dir: &Path, name: &str, limit: u64, warnings: &mut Vec<String>) -> Option<Value> {
    let bytes = read_bounded(&dir.join(name), limit, warnings)?;
    match serde_json::from_slice(&bytes) {
        Ok(v) => Some(v),
        Err(e) => {
            warnings.push(format!("{name} could not be parsed: {e}"));
            None
        }
    }
}
fn lines(dir: &Path, name: &str, warnings: &mut Vec<String>) -> Vec<Value> {
    let Some(bytes) = read_bounded(&dir.join(name), LOG_LIMIT, warnings) else {
        return vec![];
    };
    let mut out = Vec::new();
    for line in bytes.split(|b| *b == b'\n').filter(|l| !l.is_empty()) {
        if out.len() >= 50_000 {
            warnings.push(format!(
                "{name} contains more than 50000 records; display is truncated"
            ));
            break;
        }
        out.push(serde_json::from_slice(line).unwrap_or_else(
            |_| json!({"kind":"unparsed_record", "value":String::from_utf8_lossy(line)}),
        ));
    }
    out
}
/// Render captured evidence into a single offline HTML file, then atomically publish it.
pub fn write_html_report(dir: &Path) -> io::Result<()> {
    let mut warnings = Vec::new();
    let report =
        read_bounded(&dir.join("report.json"), REPORT_LIMIT, &mut warnings).and_then(|bytes| {
            use serde::Deserialize;
            let mut parser = serde_json::Deserializer::from_slice(&bytes);
            if serde::de::IgnoredAny::deserialize(&mut parser)
                .and_then(|_| parser.end())
                .is_err()
            {
                warnings.push(
                    "report.json could not be parsed; raw file remains in the archive".into(),
                );
                return None;
            }
            match String::from_utf8(bytes) {
                Ok(value) => Some(value),
                Err(_) => {
                    warnings.push("report.json is not UTF-8".into());
                    None
                }
            }
        });
    let timeline = lines(dir, "timeline.jsonl", &mut warnings);
    let debug = lines(dir, "debug.jsonl", &mut warnings);
    let android = lines(dir, "android-events.jsonl", &mut warnings);
    let mut aux = serde_json::Map::new();
    for name in [
        "capture-status.json",
        "capture-warning.json",
        "interrupted.json",
        "app-start.json",
        "feedback-trace.json",
        "physical-observation.json",
    ] {
        if let Some(value) = json_file(dir, name, 2 * 1024 * 1024, &mut warnings) {
            aux.insert(name.into(), value);
        }
    }
    if let Some(bytes) = read_bounded(
        &dir.join("worker-output.txt"),
        2 * 1024 * 1024,
        &mut warnings,
    ) {
        aux.insert(
            "worker-output.txt".into(),
            json!(String::from_utf8_lossy(&bytes)),
        );
    }
    if let Ok(meta) = fs::metadata(dir.join("usb-descriptors.bin")) {
        aux.insert(
            "usb-descriptors.bin".into(),
            json!({"bytes":meta.len(), "raw_file":"usb-descriptors.bin"}),
        );
    }
    let data = json!({"timeline":timeline, "debug":debug, "android":android, "aux":aux, "warnings":warnings});
    let mut payload = script_json(&data)?;
    payload.pop();
    payload.push_str(",\"report\":");
    if let Some(report) = report {
        payload.push_str(&escape_script(&report));
    } else {
        payload.push_str("null");
    }
    payload.push('}');
    let licenses = html_text(&format!("Manrope — SIL Open Font License\n\n{LICENSE_MANROPE}\n\nIBM Plex Mono — SIL Open Font License\n\n{LICENSE_MONO}"));
    let document = TEMPLATE
        .replace("@@TOKENS@@", TOKENS)
        .replace("@@MANROPE@@", &base64(MANROPE))
        .replace("@@MONO@@", &base64(MONO))
        .replace("@@MARK@@", MARK)
        .replace("@@MARK_LIGHT@@", MARK_LIGHT)
        .replace("@@LICENSE@@", &licenses)
        .replace("@@PAYLOAD@@", &payload);
    let path = dir.join("report.html");
    let temp = dir.join(format!(
        ".report-{}-{}.tmp",
        std::process::id(),
        ID.fetch_add(1, Ordering::Relaxed)
    ));
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)?;
        file.write_all(document.as_bytes())?;
        file.sync_all()?;
        fs::rename(&temp, &path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(temp);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn hostile_script_metadata_is_escaped() {
        let encoded =
            script_json(&json!({"name":"</script><script>alert(1)</script>&\u{2028}"})).unwrap();
        assert!(!encoded.contains('<'));
        assert!(!encoded.contains('&'));
        assert!(!encoded.contains('\u{2028}'));
        assert_eq!(
            serde_json::from_str::<Value>(&encoded).unwrap()["name"],
            "</script><script>alert(1)</script>&\u{2028}"
        );
    }
    #[test]
    fn base64_is_correct_for_short_inputs() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
    }
    #[test]
    fn renders_partial_capture_and_escapes_untrusted_metadata() {
        let dir = std::env::temp_dir().join(format!(
            "usbdac-html-test-{}-{}",
            std::process::id(),
            ID.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&dir).unwrap();
        fs::write(dir.join("android-events.jsonl"), b"{\"kind\":\"selected\",\"data\":{\"product\":\"</script><script>alert(1)</script>\"}}\n").unwrap();
        write_html_report(&dir).unwrap();
        let html = fs::read_to_string(dir.join("report.html")).unwrap();
        assert!(!html.contains("</script><script>alert(1)"));
        assert!(html.contains("\\u003c/script\\u003e"));
        assert!(html.contains("application/json"));
        assert!(html.contains("Structured playback report unavailable"));
        fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn preserves_representative_report_records() {
        let dir = std::env::temp_dir().join(format!(
            "usbdac-html-test-{}-{}",
            std::process::id(),
            ID.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&dir).unwrap();
        let sample = json!({"legs":[{"status":"passed_usb","request":"pcm"}], "admission_results":[{"admission":"expected_rejected","error_code":"unsupported"}], "fallback_results":[{"status":"passed_usb","selection_verification":{"passed":true},"conversion_verification":{"passed":true},"target_wire_integrity":"verified"}]});
        fs::write(
            dir.join("report.json"),
            serde_json::to_vec(&sample).unwrap(),
        )
        .unwrap();
        write_html_report(&dir).unwrap();
        let html = fs::read_to_string(dir.join("report.html")).unwrap();
        let embedded = html
            .split("<script type=\"application/json\" id=\"capture\">")
            .nth(1)
            .unwrap()
            .split("</script>")
            .next()
            .unwrap();
        let payload: Value = serde_json::from_str(embedded).unwrap();
        assert_eq!(payload["report"], sample);
        fs::remove_dir_all(dir).unwrap();
    }
}
