"""Verify a failed automatic release before retrying its packaging only."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import io
import zipfile

from release_artifact import receipt_attempt
from prepare_release import (HISTORICAL_WORKFLOWS, RELEASE_IMPLEMENTATION, STABLE_VERSION,
                             version_files, git, require_latest_reservation, matches_adapter, reviewed_adapters)

GATES = {
    "release-gates / Guide code samples",
    "release-gates / QEMU UAC1 playback, reconfigure and two owners",
    "release-gates / Defensive USB parsers (bounded fuzz and Miri)",
    "release-gates / Rust tests and Android check",
    "release-gates / AAR, unit tests and sample",
}
PREPARE = "Select the release version and source"


def fail(message):
    raise SystemExit(message)


def one(items, label):
    if len(items) != 1:
        fail(f"Expected exactly one {label}, found {len(items)}")
    return items[0]


def api(*args):
    return subprocess.check_output(["gh", "api", *args], text=True)




def same_repository(run, repo):
    current = json.loads(api(f'repos/{repo}'))
    repo_id = current.get('id')
    if not isinstance(repo_id, int) or isinstance(repo_id, bool):
        return False
    return all(run.get(key, {}).get('id') == repo_id for key in ('repository', 'head_repository'))


def verify_reservation(run, prepare, log, run_id, version, repo, candidate, parent, declared):
    if not STABLE_VERSION.fullmatch(version):
        fail('Reservation requires a stable version')
    if (run.get('id') != run_id or run.get('path') != '.github/workflows/release.yml'
            or run.get('event') != 'push' or run.get('head_branch') != 'main'
            or run.get('status') != 'completed' or run.get('conclusion') != 'failure'
            or not same_repository(run, repo)
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
    schema_two = any(matches(ref) for ref in ('16e5dc4d2d16304a8feab8ad54f774eda3f04a54', '6255d00b83408d81dacfa3faff615dd99cd08ecd'))
    schema_three = any(a['schema'] == 3 and matches_adapter(run['head_sha'], a['files']) for a in reviewed_adapters())
    historical = any(matches(ref) for ref in HISTORICAL_WORKFLOWS)
    if not schema_two and not schema_three and not historical:
        fail('Unknown release workflow layout')
    for name in ('Seal release gate evidence',):
        job = one([j for j in jobs if j.get('name') == name], name)
        if job.get('status') != 'completed' or job.get('conclusion') != 'success':
            fail(f'Required original job did not succeed: {name}')
    # `attempt` sealed the receipt. Its payload can come from an earlier attempt,
    # because jobs that "Re-run failed jobs" does not re-run keep their outputs
    # from the previous attempt (GitHub Docs, "Re-running workflows and jobs").
    receipt_name = f'release-gate-receipt-{run_id}-{attempt}'
    receipt = one([a for a in artifacts if a.get('name') == receipt_name], 'gate receipt')
    if receipt.get('expired') or receipt.get('workflow_run', {}).get('id') != run_id:
        fail('Gate artifact expired or belongs to another run')
    raw = subprocess.check_output(['gh', 'api', f"repos/{repo}/actions/artifacts/{receipt['id']}/zip"])
    if (not re.fullmatch(r'sha256:[0-9a-f]{64}', receipt.get('digest') or '')
            or 'sha256:' + hashlib.sha256(raw).hexdigest() != receipt['digest']):
        fail('Gate receipt archive digest mismatch')
    with zipfile.ZipFile(io.BytesIO(raw)) as archive:
        if archive.namelist() != ['receipt.json'] or archive.infolist()[0].external_attr >> 16 & 0o170000 == 0o120000:
            fail('Unexpected gate receipt archive')
        evidence = json.loads(archive.read('receipt.json'))
    payload_attempt = evidence.get('run_attempt')
    if (not isinstance(payload_attempt, int) or isinstance(payload_attempt, bool) or not 1 <= payload_attempt <= attempt
            or receipt_attempt(evidence) != attempt):
        fail('Gate receipt attempt mismatch')
    payload_name = f'release-payload-{run_id}-{payload_attempt}-{candidate}'
    payload = one([a for a in artifacts if a.get('name') == payload_name], 'gate payload')
    if payload.get('expired') or payload.get('workflow_run', {}).get('id') != run_id:
        fail('Gate artifact expired or belongs to another run')
    schema = evidence.get('schema_version')
    if schema == 3:
        if not schema_three or evidence.get('gate_identity') != 'usbdac-release-gates/v1':
            fail('Unsupported gate identity or workflow')
        if evidence.get('repository_id') != run.get('repository', {}).get('id'):
            fail('Gate receipt repository ID mismatch')
    elif schema == 2:
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
                    workflow_sha=run['head_sha'], run_id=run_id, run_attempt=payload_attempt,
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
    if not isinstance(attempt, int) or isinstance(attempt, bool) or attempt < 1:
        fail("Prior run has no valid attempt")
    pages = json.loads(api("--paginate", "--slurp", f"repos/{repo}/actions/runs/{run_id}/attempts/1/jobs?per_page=100"))
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
                or not same_repository(run, repo)
                or not isinstance(attempt, int) or isinstance(attempt, bool) or attempt < 1):
            fail('Re-gating run identity mismatch')
        pages = json.loads(api('--paginate', '--slurp', f'repos/{repo}/actions/runs/{run_id}/attempts/1/jobs?per_page=100'))
        jobs = [job for page in pages for job in page['jobs']]
        gate_prepare = one([j for j in jobs if j.get('name') == PREPARE], PREPARE)
        gate_prepare_log = api('--allow-escape-sequences', f"repos/{repo}/actions/jobs/{gate_prepare['id']}/logs")
        expected_line = f'Re-gating reserved release {declared} from {candidate}; all five gates will run'
        if gate_prepare.get('conclusion') != 'success' or gate_prepare_log.count(expected_line) != 1:
            fail('Re-gating run did not verify the requested reservation')
    artifact_pages = json.loads(api('--paginate', '--slurp', f'repos/{repo}/actions/runs/{run_id}/artifacts?per_page=100'))
    artifacts = [item for page in artifact_pages for item in page['artifacts']]
    for evidence_attempt in range(attempt, 0, -1):
        receipt_name = f'release-gate-receipt-{run_id}-{evidence_attempt}'
        if not any(a.get('name') == receipt_name for a in artifacts):
            continue
        pages = json.loads(api('--paginate', '--slurp', f'repos/{repo}/actions/runs/{run_id}/attempts/{evidence_attempt}/jobs?per_page=100'))
        evidence_jobs = [job for page in pages for job in page['jobs']]
        payload, receipt, evidence = verify_staged(run, evidence_jobs, artifacts, repo, args.version, candidate, run_id, evidence_attempt, reservation_run_id)
        if evidence.get('schema_version') == 3:
            subjects = evidence.get('validators')
            if not isinstance(subjects, list) or len(subjects) != (2 if has_validator else 0):
                fail('Invalid validator subject list')
            validators = {}
            if has_validator:
                if {s.get('platform') for s in subjects} != {'android', 'mac'}:
                    fail('Duplicate or unknown validator subject')
                for subject in subjects:
                    platform = subject['platform']
                    producing_attempt = subject.get('run_attempt')
                    if not isinstance(producing_attempt, int) or isinstance(producing_attempt, bool) or not 1 <= producing_attempt <= evidence_attempt:
                        fail('Invalid validator producing attempt')
                    obj = one([a for a in artifacts if a.get('id') == subject.get('artifact_id')], 'validator artifact')
                    name = f'{platform}-validator-{run_id}-{producing_attempt}'
                    if (obj.get('name') != name or obj.get('expired') or obj.get('workflow_run', {}).get('id') != run_id
                            or subject.get('artifact_digest') != obj.get('digest')
                            or not re.fullmatch(r'sha256:[0-9a-f]{64}', obj.get('digest') or '')
                            or subject.get('run_id') != run_id or subject.get('source_sha') != candidate
                            or subject.get('version') != declared or subject.get('repository_id') != run.get('repository', {}).get('id')):
                        fail('Validator subject provenance mismatch')
                    pages = json.loads(api('--paginate', '--slurp', f'repos/{repo}/actions/runs/{run_id}/attempts/{producing_attempt}/jobs?per_page=100'))
                    producing_jobs = [job for page in pages for job in page['jobs']]
                    label = 'Android' if platform == 'android' else 'macOS'
                    job = one([j for j in producing_jobs if j.get('name') == f'Build the {label} validator'], f'{label} validator job')
                    if job.get('status') != 'completed' or job.get('conclusion') != 'success':
                        fail('Validator producing job did not succeed')
                    validators[platform] = obj['id']
        else:
            validators = validator_artifacts(evidence_jobs, artifacts, run_id, evidence_attempt) if has_validator else {}
        with args.output.open('a') as output:
            print(f'version={declared}', f'commit={candidate}', 'recovered=true',
                  f'source_run_id={run_id}', f'source_run_attempt={evidence["run_attempt"]}',
                  f'receipt_run_attempt={evidence_attempt}',
                  f'source_repository={evidence["repository"]}',
                  f'workflow_sha={run["head_sha"]}', f'reservation_run_id={reservation_run_id}', f'artifact_id={payload["id"]}',
                  f'receipt_id={receipt["id"]}', sep='\n', file=output)
            for kind, artifact_id in validators.items():
                print(f'{kind}_validator_id={artifact_id}', file=output)
        return
    fail('No complete retained gate receipt; rerun all gates on reserved source')


if __name__ == "__main__":
    main()
