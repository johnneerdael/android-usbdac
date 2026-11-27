"""Verify a failed automatic release before retrying its packaging only."""

import argparse
import json
import os
from pathlib import Path
import re
import subprocess
import io
import zipfile

from prepare_release import (HISTORICAL_WORKFLOWS, RELEASE_IMPLEMENTATION, STABLE_VERSION,
                             version_files, git, require_latest_reservation)

GATES = {
    "release-gates / Guide code samples",
    "release-gates / QEMU UAC1 playback, reconfigure and two owners",
    "release-gates / Defensive USB parsers (bounded fuzz and Miri)",
    "release-gates / Rust tests and Android check",
    "release-gates / AAR, unit tests and sample",
}
PREPARE = "Select the release version and source"
PACKAGE = "Build, test and publish the package"
# Historical workflow layouts before and after the host-JNI input fix.  These
# are legacy source-only recoveries, not evidence of identical binaries.
LEGACY_BUILD_STEPS = {
    "Build the AAR (all four ABIs), run the unit tests, build the sample",
    "Build the AAR (all four ABIs) and sample",
}
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
    build = one([s for s in steps if s.get("name") in LEGACY_BUILD_STEPS], "known legacy AAR build step")
    if build.get("conclusion") != "failure":
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




def verify_reservation(run, prepare, log, run_id, version, repo, candidate, parent, declared):
    if not STABLE_VERSION.fullmatch(version):
        fail('Reservation requires a stable version')
    if (run.get('id') != run_id or run.get('path') != '.github/workflows/release.yml'
            or run.get('event') != 'push' or run.get('head_branch') != 'main'
            or run.get('status') != 'completed' or run.get('conclusion') != 'failure'
            or run.get('repository', {}).get('full_name') != repo
            or run.get('head_repository', {}).get('full_name') != repo
            or parent != run.get('head_sha') or declared != version
            or prepare.get('status') != 'completed' or prepare.get('conclusion') != 'success'):
        fail('Original release reservation or run identity mismatch')
    matches = re.findall(r"\bRelease ([0-9]+\.[0-9]+\.[0-9]+) from ([0-9a-f]{40})\b", log)
    if matches != [(version, candidate)]:
        fail('Original reservation log mismatch')

def verify_staged(run, jobs, artifacts, repo, version, candidate, run_id, attempt, reservation_run_id):
    # Trust exact reviewed implementations, including the version that
    # sealed schema-2 receipts before repository rename recovery.
    def matches(ref):
        try:
            return all(subprocess.check_output(['git', 'show', f"{run['head_sha']}:{path}"])
                       == (Path(path).read_bytes() if ref is None else
                           subprocess.check_output(['git', 'show', f'{ref}:{path}']))
                       for path in RELEASE_IMPLEMENTATION)
        except subprocess.CalledProcessError:
            return False
    schema_two = matches(None) or matches('16e5dc4d2d16304a8feab8ad54f774eda3f04a54')
    historical = any(matches(ref) for ref in HISTORICAL_WORKFLOWS)
    if not schema_two and not historical:
        fail('Unknown release workflow layout')
    for name in (PREPARE, 'Seal release gate evidence'):
        job = one([j for j in jobs if j.get('name') == name], name)
        if job.get('status') != 'completed' or job.get('conclusion') != 'success':
            fail(f'Required original job did not succeed: {name}')
    payload_name = f'release-payload-{run_id}-{attempt}-{candidate}'
    receipt_name = f'release-gate-receipt-{run_id}-{attempt}'
    payload = one([a for a in artifacts if a.get('name') == payload_name], 'gate payload')
    receipt = one([a for a in artifacts if a.get('name') == receipt_name], 'gate receipt')
    for artifact in (payload, receipt):
        if artifact.get('expired') or artifact.get('workflow_run', {}).get('id') != run_id:
            fail('Gate artifact expired or belongs to another run')
    raw = subprocess.check_output(['gh', 'api', f"repos/{repo}/actions/artifacts/{receipt['id']}/zip"])
    with zipfile.ZipFile(io.BytesIO(raw)) as archive:
        if archive.namelist() != ['receipt.json']:
            fail('Unexpected gate receipt archive')
        evidence = json.loads(archive.read('receipt.json'))
    schema = evidence.get('schema_version')
    if schema == 2:
        if not schema_two or evidence.get('gate_identity') != 'usbdac-release-gates/v1':
            fail('Unsupported gate identity or workflow')
    elif schema == 1:
        if not historical:
            fail('Untrusted historical receipt workflow')
        observed = [j.get('name') for j in jobs if j.get('name', '').startswith('release-gates / ')]
        if len(observed) != len(GATES) or set(observed) != GATES:
            fail('Historical run does not have exactly the required gates')
        for name in GATES:
            job = one([j for j in jobs if j.get('name') == name], name)
            if job.get('status') != 'completed' or job.get('conclusion') != 'success':
                fail(f'Historical gate did not succeed: {name}')
            gate_log = api('--allow-escape-sequences', f"repos/{repo}/actions/jobs/{job['id']}/logs")
            checkout = re.findall(r'\[command\][^\n]*git log -1 --format=%H\r?\n[^\n]*Z ([0-9a-f]{40})(?:\r?\n|$)', gate_log)
            if checkout != [candidate]:
                fail(f'Historical gate source mismatch: {name}')
    else:
        fail('Unsupported receipt schema')
    if (not re.fullmatch(r'sha256:[0-9a-f]{64}', payload.get('digest') or '')
            or not re.fullmatch(r'[0-9a-f]{64}', evidence.get('manifest_sha256') or '')):
        fail('Missing or malformed artifact or manifest digest')
    source_repo = evidence.get('repository')
    if source_repo != repo:
        repo_id = run.get('repository', {}).get('id')
        if (not isinstance(repo_id, int) or not isinstance(source_repo, str)
                or re.fullmatch(r'[a-zA-Z0-9_.-]+/[a-zA-Z0-9_.-]+', source_repo) is None
                or json.loads(api(f'repos/{source_repo}')).get('id') != repo_id):
            fail('Gate receipt belongs to another repository')
    expected = dict(version=version, source_sha=candidate,
                    workflow_sha=run['head_sha'], run_id=run_id, run_attempt=attempt,
                    artifact_id=payload['id'], artifact_digest=payload.get('digest'),
                    reservation_run_id=reservation_run_id)
    if any(evidence.get(k) != v for k, v in expected.items()):
        fail('Gate receipt does not bind the original payload and reservation')
    return payload, receipt, evidence

def validator_artifacts(jobs, artifacts, run_id, attempt):
    result = {}
    for kind, label in (('android', 'Android'), ('mac', 'macOS')):
        job = one([j for j in jobs if j.get('name') == f'Build the {label} validator'], f'{label} validator job')
        if job.get('status') != 'completed' or job.get('conclusion') != 'success':
            fail(f'Original {label} validator build did not succeed')
        name = f'{kind}-validator-{run_id}-{attempt}'
        artifact = one([a for a in artifacts if a.get('name') == name], name)
        if (artifact.get('expired') or artifact.get('workflow_run', {}).get('id') != run_id
                or not re.fullmatch(r'sha256:[0-9a-f]{64}', artifact.get('digest') or '')):
            fail(f'{label} validator artifact is unavailable or has invalid provenance')
        result[kind] = artifact['id']
    return result


def main():
    parser = argparse.ArgumentParser()
    group = parser.add_mutually_exclusive_group(required=True)
    group.add_argument("--failed-run-id", type=int)
    group.add_argument("--regate-run-id", type=int)
    parser.add_argument("--gate-run-id", type=int, help="failed re-gating run whose payload should be reused")
    parser.add_argument("--version", required=True)
    parser.add_argument("--output", required=True, type=Path)
    args = parser.parse_args()
    run_id = args.failed_run_id if args.failed_run_id is not None else args.regate_run_id
    reservation_run_id = run_id
    if (run_id <= 0 or os.environ.get("GITHUB_EVENT_NAME") != "workflow_dispatch"
            or os.environ.get("GITHUB_REF") != "refs/heads/main"):
        fail("Recovery must be dispatched on main for a positive run ID")
    if args.gate_run_id is not None and (args.gate_run_id <= 0 or args.regate_run_id is not None):
        fail('A positive gate run ID requires package-only recovery')
    if args.failed_run_id and os.environ.get("SKIP_PACKAGE") == "true":
        fail("Recovery cannot skip package publication")
    repo = os.environ["GITHUB_REPOSITORY"]
    run = json.loads(api(f"repos/{repo}/actions/runs/{run_id}"))
    attempt = run.get("run_attempt")
    if not isinstance(attempt, int) or attempt < 1:
        fail("Prior run has no valid attempt")
    pages = json.loads(api("--paginate", "--slurp", f"repos/{repo}/actions/runs/{run_id}/attempts/{attempt}/jobs?per_page=100"))
    jobs = [job for page in pages for job in page["jobs"]]
    prepare = one([j for j in jobs if j.get("name") == PREPARE], PREPARE)
    log = api("--allow-escape-sequences", f"repos/{repo}/actions/jobs/{prepare['id']}/logs")
    git("fetch", "--quiet", "origin", "main")
    marker = f"Release-Run: {run_id}"
    candidates = git("log", "origin/main", "--format=%H", "--fixed-strings", f"--grep={marker}").splitlines()
    candidates = [sha for sha in candidates if marker in git("show", "-s", "--format=%B", sha).splitlines()]
    candidate = one(candidates, "reserved commit on main")
    require_latest_reservation("origin/main", args.version, candidate)
    parents = git("show", "-s", "--format=%P", candidate).split()
    parent = one(parents, "reserved commit parent")
    versions = []
    has_validator = subprocess.run(["git", "cat-file", "-e", f"{candidate}:crates/usbdac-validator/Cargo.toml"], check=False).returncode == 0
    for path, (pattern, count) in version_files(has_validator).items():
        content = git("show", f"{candidate}:{path}")
        values = [match[2] for match in pattern.finditer(content)]
        if len(values) != count:
            fail(f"Cannot read version from {path}")
        versions.extend(values)
    declared = one(list(set(versions)), "declared version")
    verify_reservation(run, prepare, log, run_id, args.version, repo, candidate, parent, declared)
    if args.regate_run_id:
        with args.output.open('a') as output:
            print(f'version={declared}', f'commit={candidate}', f'reservation_run_id={reservation_run_id}', sep='\n', file=output)
        print(f'Re-gating reserved release {declared} from {candidate}; all five gates will run')
        return
    if args.gate_run_id is not None:
        # The reservation is from the failed automatic push; the payload can
        # instead come from a later, independently gated dispatch of that SHA.
        run_id = args.gate_run_id
        run = json.loads(api(f'repos/{repo}/actions/runs/{run_id}'))
        attempt = run.get('run_attempt')
        if (run.get('id') != run_id or run.get('path') != '.github/workflows/release.yml'
                or run.get('event') != 'workflow_dispatch' or run.get('head_branch') != 'main'
                or run.get('status') != 'completed' or run.get('conclusion') != 'failure'
                or run.get('repository', {}).get('full_name') != repo
                or run.get('head_repository', {}).get('full_name') != repo
                or not isinstance(attempt, int) or attempt < 1):
            fail('Re-gating run identity mismatch')
        pages = json.loads(api('--paginate', '--slurp', f'repos/{repo}/actions/runs/{run_id}/attempts/{attempt}/jobs?per_page=100'))
        jobs = [job for page in pages for job in page['jobs']]
        gate_prepare = one([j for j in jobs if j.get('name') == PREPARE], PREPARE)
        gate_prepare_log = api('--allow-escape-sequences', f"repos/{repo}/actions/jobs/{gate_prepare['id']}/logs")
        expected_line = f'Re-gating reserved release {declared} from {candidate}; all five gates will run'
        if gate_prepare.get('conclusion') != 'success' or gate_prepare_log.count(expected_line) != 1:
            fail('Re-gating run did not verify the requested reservation')
    artifact_pages = json.loads(api('--paginate', '--slurp', f'repos/{repo}/actions/runs/{run_id}/artifacts?per_page=100'))
    artifacts = [item for page in artifact_pages for item in page['artifacts']]
    if any(a.get('name', '').startswith(f'release-gate-receipt-{run_id}-{attempt}') for a in artifacts):
        payload, receipt, evidence = verify_staged(run, jobs, artifacts, repo, args.version, candidate, run_id, attempt, reservation_run_id)
        validators = validator_artifacts(jobs, artifacts, run_id, attempt) if has_validator else {}
        with args.output.open('a') as output:
            print(f'version={declared}', f'commit={candidate}', 'recovered=true',
                  f'source_run_id={run_id}', f'source_run_attempt={attempt}',
                  f'source_repository={evidence["repository"]}',
                  f'workflow_sha={run["head_sha"]}', f'reservation_run_id={reservation_run_id}', f'artifact_id={payload["id"]}',
                  f'receipt_id={receipt["id"]}', sep='\n', file=output)
            for kind, artifact_id in validators.items():
                print(f'{kind}_validator_id={artifact_id}', file=output)
        return
    # Legacy adapter is limited to the historical source-only release.  It does
    # not turn a rebuild into digest-bound provenance.
    if args.gate_run_id is not None or args.version != '0.1.10':
        fail('No complete retained gate receipt; rerun all gates on reserved source')
    fail('Historical source-only recovery cannot use the staged-artifact publisher')


if __name__ == "__main__":
    main()
