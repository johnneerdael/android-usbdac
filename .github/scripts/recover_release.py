"""Verify a failed automatic release before retrying its packaging only."""

import argparse
import json
import os
from pathlib import Path
import re
import subprocess

from prepare_release import STABLE_VERSION, VERSION_FILES, git

GATES = {
    "release-gates / Guide code samples",
    "release-gates / QEMU UAC1 playback, reconfigure and two owners",
    "release-gates / Defensive USB parsers (bounded fuzz and Miri)",
    "release-gates / Rust tests and Android check",
    "release-gates / AAR, unit tests and sample",
}
PREPARE = "Select the release version and source"
PACKAGE = "Build, test and publish the package"
BUILD_STEP = "Build the AAR (all four ABIs), run the unit tests, build the sample"
NOT_REACHED = {
    "Check whether the Rust crate version is published",
    "Publish the Rust crate to crates.io",
    "Check each Maven package before uploading",
    "Publish the core to GitHub Packages",
    "Publish the Media3 integration to GitHub Packages",
    "Collect the release assets",
}


def fail(message):
    raise SystemExit(message)


def one(items, label):
    if len(items) != 1:
        fail(f"Expected exactly one {label}, found {len(items)}")
    return items[0]


def verify(run, jobs, log, gate_logs, run_id, version, repo, candidate, parent, declared):
    if not STABLE_VERSION.fullmatch(version):
        fail("Recovery requires a stable version")
    if (run.get("id") != run_id or run.get("path") != ".github/workflows/release.yml"
            or run.get("event") != "push" or run.get("head_branch") != "main"
            or run.get("status") != "completed" or run.get("conclusion") != "failure"
            or run.get("repository", {}).get("full_name") != repo
            or run.get("head_repository", {}).get("full_name") != repo):
        fail("Failed run is not a completed main push release in this repository")
    observed_gates = [job.get("name") for job in jobs if job.get("name", "").startswith("release-gates / ")]
    if len(observed_gates) != len(GATES) or set(observed_gates) != GATES:
        fail("Prior release does not contain exactly the required gates")
    for name in GATES | {PREPARE}:
        job = one([j for j in jobs if j.get("name") == name], name)
        if job.get("status") != "completed" or job.get("conclusion") != "success":
            fail(f"Prior gate did not succeed: {name}")
    for name in GATES:
        checkout = re.findall(r"\[command\][^\n]*git log -1 --format=%H\r?\n[^\n]*Z ([0-9a-f]{40})(?:\r?\n|$)", gate_logs.get(name, ""))
        if checkout != [candidate]:
            fail(f"Prior gate did not check out the reserved commit: {name}")
    package = one([j for j in jobs if j.get("name") == PACKAGE], PACKAGE)
    if package.get("status") != "completed" or package.get("conclusion") != "failure":
        fail("Prior package job did not fail")
    steps = package.get("steps", [])
    if one([s for s in steps if s.get("name") == BUILD_STEP], BUILD_STEP).get("conclusion") != "failure":
        fail("Prior failure was not at the known AAR build step")
    for name in NOT_REACHED:
        if one([s for s in steps if s.get("name") == name], name).get("conclusion") != "skipped":
            fail(f"Prior package may have published: {name}")
    uploads = [s for s in steps if s.get("name", "").startswith("Run actions/upload-artifact@")]
    if not uploads or any(s.get("conclusion") != "skipped" for s in uploads):
        fail("Prior package artifact upload was not skipped")
    if parent != run.get("head_sha") or declared != version:
        fail("Reserved commit parent or declared version does not match")
    matches = re.findall(r"\bRelease ([0-9]+\.[0-9]+\.[0-9]+) from ([0-9a-f]{40})\b", log)
    if matches != [(version, candidate)]:
        fail("Prior prepare log does not uniquely identify this version and commit")


def api(*args):
    return subprocess.check_output(["gh", "api", *args], text=True)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--failed-run-id", required=True, type=int)
    parser.add_argument("--version", required=True)
    parser.add_argument("--output", required=True, type=Path)
    args = parser.parse_args()
    if (args.failed_run_id <= 0 or os.environ.get("GITHUB_EVENT_NAME") != "workflow_dispatch"
            or os.environ.get("GITHUB_REF") != "refs/heads/main"):
        fail("Recovery must be dispatched on main for a positive run ID")
    if os.environ.get("SKIP_PACKAGE") == "true":
        fail("Recovery cannot skip package publication")
    repo = os.environ["GITHUB_REPOSITORY"]
    run = json.loads(api(f"repos/{repo}/actions/runs/{args.failed_run_id}"))
    attempt = run.get("run_attempt")
    if not isinstance(attempt, int) or attempt < 1:
        fail("Prior run has no valid attempt")
    pages = json.loads(api("--paginate", "--slurp", f"repos/{repo}/actions/runs/{args.failed_run_id}/attempts/{attempt}/jobs?per_page=100"))
    jobs = [job for page in pages for job in page["jobs"]]
    prepare = one([j for j in jobs if j.get("name") == PREPARE], PREPARE)
    log = api("--allow-escape-sequences", f"repos/{repo}/actions/jobs/{prepare['id']}/logs")
    git("fetch", "--quiet", "origin", "main")
    marker = f"Release-Run: {args.failed_run_id}"
    candidates = git("log", "origin/main", "--format=%H", "--fixed-strings", f"--grep={marker}").splitlines()
    candidates = [sha for sha in candidates if marker in git("show", "-s", "--format=%B", sha).splitlines()]
    candidate = one(candidates, "reserved commit on main")
    parents = git("show", "-s", "--format=%P", candidate).split()
    parent = one(parents, "reserved commit parent")
    versions = []
    for path, (pattern, count) in VERSION_FILES.items():
        content = git("show", f"{candidate}:{path}")
        values = [match[2] for match in pattern.finditer(content)]
        if len(values) != count:
            fail(f"Cannot read version from {path}")
        versions.extend(values)
    declared = one(list(set(versions)), "declared version")
    gate_logs = {}
    for name in GATES:
        job = one([j for j in jobs if j.get("name") == name], name)
        gate_logs[name] = api("--allow-escape-sequences", f"repos/{repo}/actions/jobs/{job['id']}/logs")
    verify(run, jobs, log, gate_logs, args.failed_run_id, args.version, repo, candidate, parent, declared)
    with args.output.open("a") as output:
        print(f"version={declared}", f"commit={candidate}", "recovered=true", sep="\n", file=output)
    print(f"Verified failed release {args.failed_run_id}: {declared} from {candidate}")


if __name__ == "__main__":
    main()
