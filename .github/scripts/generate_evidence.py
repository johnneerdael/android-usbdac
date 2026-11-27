#!/usr/bin/env python3
"""Validate the results ledger and generate an honest current status page."""
import argparse
import hashlib
import json
import pathlib
import re
import sys

ROOT = pathlib.Path(__file__).resolve().parents[2]
LEDGER = ROOT / "docs/evidence/results.json"
OUTPUT = ROOT / "docs/sdk/status.md"


def generate(capabilities):
    ledger = json.loads(LEDGER.read_text())
    if ledger.get("schema_version") != 1:
        raise ValueError("unsupported evidence schema")
    ids = set()
    lines = ["# Recorded capabilities and evidence", "", "Generated from `docs/evidence/results.json`; edit the ledger and run `python3 -I .github/scripts/generate_evidence.py`.", "", ledger["policy"], "", "## Implementation boundaries", "", f"Shipped boundaries: UAC versions {capabilities['uac_versions']}; playback {capabilities['playback']}; PCM capture {capabilities['pcm_capture']}; duplex {capabilities['duplex']}; simultaneous open devices {capabilities['max_open_devices']}; Type I formats {', '.join(capabilities['type_i_formats'])}; vendor protocols {capabilities['vendor_protocols']}. USB host tests and virtual guests do not establish physical or audible behavior. Source-backed compatibility candidates are distinct from hardware-verified device rules.", "", "| Feature | Device / host | Evidence tier | Result | Revision and limits |", "|---|---|---|---|---|"]
    required = {"id", "feature", "tier", "status", "device", "vendor_id", "product_id", "bcd_device", "phone", "android_build", "oem_profile", "revision", "source", "log_sha256", "notes"}
    for row in ledger["results"]:
        if set(row) != required or row["id"] in ids:
            raise ValueError(f"invalid or duplicate ledger row: {row.get('id')}")
        ids.add(row["id"])
        if row["tier"] not in {"physical", "virtual", "host"} or row["status"] not in {"pending", "historical-pass", "passed", "failed"}:
            raise ValueError(f"invalid evidence classification: {row['id']}")
        for key in ("vendor_id", "product_id", "bcd_device"):
            if row[key] is not None and not re.fullmatch("[0-9a-f]{4}", row[key]):
                raise ValueError(f"invalid {key}: {row['id']}")
        if row["status"] != "pending" and (not row["revision"] or not row["source"]):
            raise ValueError(f"completed result lacks revision/source: {row['id']}")
        if row["status"] == "passed" and not row["log_sha256"]:
            raise ValueError(f"passed result lacks a log digest: {row['id']}")
        digest = row["log_sha256"]
        if digest:
            source = ROOT / row["source"]
            if not re.fullmatch("[0-9a-f]{64}", digest) or not source.is_file() or hashlib.sha256(source.read_bytes()).hexdigest() != digest:
                raise ValueError(f"evidence digest mismatch: {row['id']}")
        fields = [row["feature"], row["device"] + "; " + row["phone"], row["tier"], row["status"], (row["revision"] or "pending") + ": " + row["notes"]]
        lines.append("| " + " | ".join(value.replace("|", "\\|").replace("\n", " ") for value in fields) + " |")
    lines += ["", "Historical passes preserve source records with incomplete metadata; they are not a fresh acceptance of the current tree. Pending OEM rows are prerequisites, not failed tests. Per-device bcdDevice, Android build, OEM profile, exact revision and log SHA-256 must be recorded for new hardware acceptance.", "", "## Validation source inventory", "", "These are annotations in current sources, not counts of passing tests. CI and archived logs establish run results independently.", "", "| Source | Test annotations |", "|---|---|"]
    for label, paths, pattern in [
        ("Rust core", (ROOT / "crates/usbdac-core").rglob("*.rs"), r"#\[test\]"),
        ("Rust JNI", (ROOT / "crates/usbdac-jni/src").rglob("*.rs"), r"#\[test\]"),
        ("Kotlin SDK", (ROOT / "android/usbdac/src/test").rglob("*.kt"), r"@Test\b"),
        ("Media3 library", (ROOT / "android/usbdac-media3/src/test").rglob("*.kt"), r"@Test\b")]:
        count = sum(len(re.findall(pattern, path.read_text())) for path in paths)
        lines.append(f"| {label} | {count} |")
    return "\n".join(lines) + "\n"


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--check", action="store_true")
    parser.add_argument("--capabilities", type=pathlib.Path, help="manifest emitted by the compiled support_manifest example")
    args = parser.parse_args()
    checked = json.loads((ROOT / "docs/evidence/capabilities.json").read_text())
    if args.capabilities:
        actual = json.loads(args.capabilities.read_text())
        if checked != actual:
            raise ValueError("shipped capability drift: regenerate docs/evidence/capabilities.json from support_manifest")
    expected = generate(checked)
    if args.check:
        if not OUTPUT.exists() or OUTPUT.read_text() != expected:
            print("Evidence status drift: run python3 -I .github/scripts/generate_evidence.py", file=sys.stderr)
            return 1
    else:
        OUTPUT.write_text(expected)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
