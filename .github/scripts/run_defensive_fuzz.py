#!/usr/bin/env python3
"""Run defensive ASan fuzzing without publishing generated input bytes.

Cargo-fuzz output may contain crashing input bytes. Never echo it or retain a
raw transcript. Emit only fixed diagnostic categories, existing Rust source
locations and numeric libFuzzer metrics; propagate the child exit status.
"""
import argparse
import json
import pathlib
import re
import subprocess

ROOT = pathlib.Path(__file__).resolve().parents[2]
SOURCE = re.compile(r"(?:^|[/\s(])((?:crates|fuzz)/[A-Za-z0-9_./-]+\.rs):(\d+)(?::(\d+))?")
SANITIZER_KINDS = {"heap-buffer-overflow", "stack-buffer-overflow", "global-buffer-overflow", "heap-use-after-free", "stack-use-after-return", "stack-use-after-scope", "use-after-poison", "double-free", "alloc-dealloc-mismatch", "new-delete-type-mismatch", "bad-free", "container-overflow", "initialization-order-fiasco", "stack-overflow", "negative-size-param", "use-of-uninitialized-value"}
SANITIZER = re.compile(r"(?:ERROR|SUMMARY): (AddressSanitizer|UndefinedBehaviorSanitizer|MemorySanitizer): ([a-z][a-z-]+)")
METRIC = re.compile(r"\b(cov|ft|corp|lim|exec/s|rss):\s*(\d{1,20})(?:/(\d{1,20}))?")
STAT = re.compile(r"^stat::(number_of_executed_units|average_exec_per_sec|new_units_added|slowest_unit_time_sec|peak_rss_mb):\s*(\d{1,20})\s*$")


def diagnostic(line):
    """Whitelist structured facts; no arbitrary child text reaches the report."""
    sanitizer = SANITIZER.search(line)
    if sanitizer:
        return {"sanitizer": sanitizer[1], "signature": sanitizer[2] if sanitizer[2] in SANITIZER_KINDS else "sanitizer failure"}
    locations = []
    for match in SOURCE.finditer(line):
        source = pathlib.Path(match[1])
        resolved = (ROOT / source).resolve()
        if resolved.is_relative_to(ROOT) and resolved.is_file():
            locations.append({"source": str(source), "line": int(match[2]), "column": int(match[3] or 0)})
    if locations:
        return {"source_locations": locations}
    if re.search(r"index out of bounds: the len is \d+ but the index is \d+", line):
        return {"signature": "Rust bounds panic"}
    if "libFuzzer: deadly signal" in line:
        return {"signature": "libFuzzer deadly signal"}
    if re.match(r"^#\d+\s+(INITED|NEW|REDUCE|DONE)\b", line):
        return {"metrics": {match[1]: [int(match[2]), int(match[3])] if match[3] else int(match[2]) for match in METRIC.finditer(line)}}
    stat = STAT.match(line)
    if stat:
        return {"metrics": {stat[1]: int(stat[2])}}
    return None


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--seconds", type=int, required=True)
    parser.add_argument("--log", type=pathlib.Path, required=True)
    parser.add_argument("--corpus", type=pathlib.Path, help="optional task-owned corpus directory")
    args = parser.parse_args()
    if not 1 <= args.seconds <= 10800:
        parser.error("duration must be between 1 and 10800 seconds")
    args.log.parent.mkdir(parents=True, exist_ok=True)
    command = ["cargo", "fuzz", "run", "defensive_parsers", "--sanitizer", "address", "--", f"-max_total_time={args.seconds}", "-max_len=4096", "-timeout=10"]
    if args.corpus is not None:
        command.insert(command.index("--"), str(args.corpus.resolve()))
    with args.log.open("w") as report:
        def emit(value):
            encoded = json.dumps(value, sort_keys=True)
            print(encoded, flush=True)
            report.write(encoded + "\n")
            report.flush()
        emit({"scope": "defensive parser ASan fuzz", "duration_seconds": args.seconds, "raw_payload_logging": False})
        try:
            child = subprocess.Popen(command, cwd=ROOT, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True, encoding="utf-8", errors="replace")
        except OSError:
            emit({"signature": "unable to start cargo-fuzz", "exit_status": 127})
            return 127
        assert child.stdout is not None
        for raw_line in child.stdout:
            safe = diagnostic(raw_line)
            if safe is not None:
                emit(safe)
        status = child.wait()
        status = status if status >= 0 else 128 - status
        emit({"exit_status": status})
        return status


if __name__ == "__main__":
    raise SystemExit(main())
