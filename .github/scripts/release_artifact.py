"""Create and verify the immutable release-gate payload (schema 1) and seal its receipt (schema 3)."""

import argparse
import hashlib
import io
import re
import json
import os
from pathlib import Path
import shutil
import subprocess
import tarfile
import tomllib
import zipfile


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def required(root, version):
    return {f"maven/dev/usbdac/{name}/{version}/{name}-{version}{suffix}"
            for name in ("usbdac", "usbdac-media3")
            for suffix in (".aar", ".pom", ".module", "-sources.jar")} | {
                f"assets/usbdac-{version}-jniLibs.zip",
                f"crates/usbdac-core-{version}.crate",
                f"crates/usbdac-core-{version}.publish.json",
                "SHA256SUMS.txt",
                "assets/device-compatibility-candidates.md",
            }


def files(root):
    return {str(p.relative_to(root)): {'sha256': digest(p), 'size': p.stat().st_size} for p in sorted(root.rglob('*')) if p.is_file() and p != root / 'manifest.json'}


def command(*args):
    return subprocess.check_output(args, text=True, stderr=subprocess.STDOUT).strip()


def stage(root, version):
    root.mkdir(parents=True, exist_ok=False)
    for name in ("usbdac", "usbdac-media3"):
        src = Path(f"android/build/staging/dev/usbdac/{name}/{version}")
        dst = root / 'maven' / 'dev' / 'usbdac' / name / version
        dst.mkdir(parents=True)
        for suffix in ('.aar', '.pom', '.module', '-sources.jar'):
            shutil.copyfile(src / f'{name}-{version}{suffix}', dst / f'{name}-{version}{suffix}')
    assets = root / 'assets'
    assets.mkdir()
    native = Path('android/usbdac/src/main/jniLibs')
    native_files = sorted(native.rglob('*.so'))
    if {str(p.relative_to(native)) for p in native_files} != {f'{abi}/libusbdac.so' for abi in ('arm64-v8a', 'armeabi-v7a', 'x86_64', 'x86')}:
        raise SystemExit('Native archive must contain exactly the four supported ABIs')
    with zipfile.ZipFile(assets / f'usbdac-{version}-jniLibs.zip', 'w', zipfile.ZIP_DEFLATED) as archive:
        for p in native_files:
            archive.write(p, p.relative_to(native))
    crates = root / 'crates'
    crates.mkdir()
    crate = crates / f'usbdac-core-{version}.crate'
    shutil.copyfile(f'target/package/usbdac-core-{version}.crate', crate)
    shutil.copyfile('docs/design/device-compatibility-candidates.md', assets / 'device-compatibility-candidates.md')
    metadata = json.loads(command('cargo', 'metadata', '--format-version=1', '--no-deps', '--locked'))
    package = next(p for p in metadata['packages'] if p['name'] == 'usbdac-core')
    if package['version'] != version:
        raise SystemExit('Crate version mismatch')
    deps = [{"name": d['name'], "version_req": d['req'], "features": d['features'],
             "optional": d['optional'], "default_features": d['uses_default_features'],
             "target": d['target'], "kind": d['kind'] or 'normal', "registry": d['registry'],
             "explicit_name_in_toml": d['rename']} for d in package['dependencies']]
    m = {k: package.get(k) for k in ('authors', 'description', 'documentation', 'homepage', 'keywords',
        'categories', 'license', 'license_file', 'repository', 'links', 'rust_version', 'features')}
    m.update(name=package['name'], vers=version, deps=deps,
             readme=(Path(package['manifest_path']).parent / package['readme']).read_text() if package.get('readme') else None,
             readme_file=Path(package['readme']).name if package.get('readme') else None)
    with tarfile.open(crate, 'r:gz') as archive:
        normalized = tomllib.loads(archive.extractfile(f'usbdac-core-{version}/Cargo.toml').read().decode())
        if m['readme_file'] and archive.extractfile(f"usbdac-core-{version}/{m['readme_file']}").read().decode() != m['readme']:
            raise SystemExit('Packaged README does not match publish metadata')
    if normalized['package']['name'] != m['name'] or normalized['package']['version'] != version:
        raise SystemExit('Packaged manifest name/version mismatch')
    for dep in deps:
        if dep['target'] is not None or dep['kind'] != 'normal' or dep['explicit_name_in_toml']:
            raise SystemExit('Unsupported dependency shape requires explicit publish metadata validation')
        spec = normalized['dependencies'].get(dep['name'])
        if (not isinstance(spec, dict) or spec.get('version') != dep['version_req'].lstrip('^')
                or sorted(spec.get('features', [])) != sorted(dep['features'])
                or spec.get('optional', False) != dep['optional']
                or spec.get('default-features', True) != dep['default_features']):
            raise SystemExit(f"Packaged dependency mismatch: {dep['name']}")
    if normalized.get('features', {}) != m['features']:
        raise SystemExit('Packaged features mismatch')
    if set(normalized['dependencies']) != {d['name'] for d in deps}:
        raise SystemExit('Packaged dependency set mismatch')
    (crates / f'usbdac-core-{version}.publish.json').write_text(json.dumps(m, sort_keys=True) + '\n')
    payload_files = files(root)
    (root / 'SHA256SUMS.txt').write_text(''.join(f"{entry['sha256']}  {name}\n" for name, entry in payload_files.items()))
    entries = files(root)
    if set(entries) != required(root, version):
        raise SystemExit('Staged payload does not contain exactly the required files')
    env = os.environ
    manifest = {"schema_version": 1, "repository": env['GITHUB_REPOSITORY'], "repository_id": int(env['GITHUB_REPOSITORY_ID']),
                "source_sha": env['SOURCE_SHA'], "workflow_sha": env['GITHUB_SHA'],
                "workflow_file": ".github/workflows/release.yml", "gate_job": "release-gates / AAR, unit tests and sample",
                "run_id": int(env['GITHUB_RUN_ID']), "run_attempt": int(env['GITHUB_RUN_ATTEMPT']),
                "reservation_run_id": int(env['RESERVATION_RUN_ID']),
                "version": version, "rust": command('rustc', '--version', '--verbose'),
                "cargo": command('cargo', '--version', '--verbose'),
                "java": command('java', '-XshowSettings:properties', '-version'),
                "ndk": env['NDK_VERSION'], "cargo_ndk": env['CARGO_NDK_VERSION'],
                "gradle_wrapper_sha256": digest(Path('android/gradle/wrapper/gradle-wrapper.jar')),
                "gradle_properties_sha256": digest(Path('android/gradle/wrapper/gradle-wrapper.properties')),
                "cargo_lock_sha256": digest(Path('Cargo.lock')), "files": entries}
    (root / 'manifest.json').write_text(json.dumps(manifest, sort_keys=True, indent=2) + '\n')


def verify(root, expected):
    if any(path.is_symlink() for path in root.rglob('*')):
        raise SystemExit('Release payload contains a symlink')
    manifest = json.loads((root / 'manifest.json').read_text())
    for key, value in expected.items():
        if manifest.get(key) != value:
            raise SystemExit(f'Manifest {key} mismatch')
    if manifest.get('schema_version') != 1 or set(manifest.get('files', {})) != required(root, expected['version']):
        raise SystemExit('Unknown schema or incomplete payload')
    if files(root) != manifest['files']:
        raise SystemExit('Release payload is missing, changed, or contains unexpected files')
    return manifest


def validator_subject(archive_bytes, platform, artifact_id, archive_digest, run_id, attempt, version, source_sha, repository_id):
    if not re.fullmatch(r'sha256:[0-9a-f]{64}', archive_digest or '') or 'sha256:' + hashlib.sha256(archive_bytes).hexdigest() != archive_digest:
        raise SystemExit('Validator artifact archive digest mismatch')
    filename = f'usbdac-validator-{version}-' + ('android.apk' if platform == 'android' else 'macos-arm64.zip')
    with zipfile.ZipFile(io.BytesIO(archive_bytes)) as archive:
        entries = archive.infolist()
        if len(entries) != 1 or entries[0].filename != filename or entries[0].is_dir() or (entries[0].external_attr >> 16) & 0o170000 == 0o120000:
            raise SystemExit('Unexpected validator artifact contents')
        data = archive.read(entries[0])
    if not data:
        raise SystemExit('Empty validator artifact')
    return dict(platform=platform, artifact_id=int(artifact_id), artifact_digest=archive_digest,
                run_id=int(run_id), run_attempt=int(attempt), filename=filename,
                size=len(data), sha256=hashlib.sha256(data).hexdigest(), version=version,
                source_sha=source_sha, repository_id=int(repository_id))


def verify_validator_subjects(receipt, root, version, source_sha, repository_id, run_id, attempt, ids):
    subjects = receipt.get('validators')
    if not isinstance(subjects, list) or len(subjects) != 2:
        raise SystemExit('Missing validator subjects')
    if {s.get('platform') for s in subjects} != {'android', 'mac'}:
        raise SystemExit('Duplicate or unknown validator subject')
    expected_files = set()
    for subject in subjects:
        platform = subject['platform']
        filename = f'usbdac-validator-{version}-' + ('android.apk' if platform == 'android' else 'macos-arm64.zip')
        expected = dict(filename=filename, version=version, source_sha=source_sha, repository_id=int(repository_id),
                        run_id=int(run_id), artifact_id=int(ids[platform]))
        producing_attempt = subject.get('run_attempt')
        if not isinstance(producing_attempt, int) or isinstance(producing_attempt, bool) or not 1 <= producing_attempt <= int(attempt):
            raise SystemExit('Invalid validator producing attempt')
        if any(subject.get(key) != value for key, value in expected.items()) or not re.fullmatch(r'sha256:[0-9a-f]{64}', subject.get('artifact_digest') or ''):
            raise SystemExit('Validator subject identity mismatch')
        path = root / filename
        if path.is_symlink() or not path.is_file() or path.stat().st_size != subject.get('size') or digest(path) != subject.get('sha256'):
            raise SystemExit('Validator subject bytes mismatch')
        expected_files.add(filename)
    if {p.name for p in root.iterdir()} != expected_files:
        raise SystemExit('Unexpected validator release files')


GATE_IDENTITY = 'usbdac-release-gates/v1'


def attempt_number(value, label):
    try:
        attempt = int(value)
    except (TypeError, ValueError):
        raise SystemExit(f'Missing or invalid {label} attempt') from None
    if attempt < 1:
        raise SystemExit(f'Missing or invalid {label} attempt')
    return attempt


def receipt_attempt(receipt):
    """Return the attempt that sealed a receipt.

    The gate payload and its receipt can come from different attempts: after
    "Re-run failed jobs", jobs that are not re-run keep their outputs from the
    previous attempt (GitHub Docs, "Re-running workflows and jobs"), so a
    re-run receipt seals a payload uploaded by an earlier attempt. Receipts
    sealed before receipt_run_attempt existed (0.1.17 and older) were always
    sealed in the payload's own attempt.
    """
    value = receipt.get('receipt_run_attempt', receipt.get('run_attempt'))
    if not isinstance(value, int) or isinstance(value, bool) or value < 1:
        raise SystemExit('Invalid gate receipt attempt')
    return value


def gh_api(path):
    return subprocess.check_output(['gh', 'api', path])


def seal(root, receipt_path, version, env, api=gh_api):
    payload_attempt = attempt_number(env.get('PAYLOAD_RUN_ATTEMPT'), 'gate payload')
    sealing_attempt = attempt_number(env.get('RECEIPT_RUN_ATTEMPT'), 'gate receipt')
    if payload_attempt > sealing_attempt:
        raise SystemExit('Gate payload attempt is later than the sealing attempt')
    if not env.get('ARTIFACT_ID') or not env.get('ARTIFACT_DIGEST'):
        raise SystemExit('Missing gate payload identity')
    if env.get('GATE_IDENTITY') != GATE_IDENTITY:
        raise SystemExit('Unsupported or missing gate identity')
    repo, run_id, source_sha = env['GITHUB_REPOSITORY'], int(env['SOURCE_RUN_ID']), env['SOURCE_SHA']
    payload_digest = 'sha256:' + env['ARTIFACT_DIGEST'].removeprefix('sha256:')
    info = json.loads(api(f"repos/{repo}/actions/artifacts/{env['ARTIFACT_ID']}"))
    if (info.get('name') != f'release-payload-{run_id}-{payload_attempt}-{source_sha}' or info.get('expired')
            or info.get('workflow_run', {}).get('id') != run_id or info.get('digest') != payload_digest):
        raise SystemExit('Gate payload artifact API identity mismatch')
    verify(root, dict(repository=env.get('SOURCE_REPOSITORY', repo), version=version, source_sha=source_sha,
                      workflow_sha=env['WORKFLOW_SHA'], run_id=run_id, run_attempt=payload_attempt,
                      reservation_run_id=int(env['RESERVATION_RUN_ID'])))
    validators = []
    if env.get('VALIDATOR') == 'true':
        for platform in ('android', 'mac'):
            prefix = platform.upper() + '_VALIDATOR_'
            artifact_id, attempt = env[prefix + 'ID'], attempt_number(env.get(prefix + 'ATTEMPT'), f'{platform} validator')
            if attempt > sealing_attempt:
                raise SystemExit('Validator attempt is later than the sealing attempt')
            info = json.loads(api(f'repos/{repo}/actions/artifacts/{artifact_id}'))
            validator_digest = 'sha256:' + env[prefix + 'DIGEST'].removeprefix('sha256:')
            if (info.get('name') != f'{platform}-validator-{run_id}-{attempt}' or info.get('expired')
                    or info.get('workflow_run', {}).get('id') != run_id or info.get('digest') != validator_digest):
                raise SystemExit('Validator artifact API identity mismatch')
            raw = api(f'repos/{repo}/actions/artifacts/{artifact_id}/zip')
            validators.append(validator_subject(raw, platform, artifact_id, validator_digest, run_id, attempt, version, source_sha, env['GITHUB_REPOSITORY_ID']))
    receipt = dict(schema_version=3, validators=validators, repository_id=int(env['GITHUB_REPOSITORY_ID']), gate_identity=GATE_IDENTITY,
                   repository=repo, version=version, source_sha=source_sha, workflow_sha=env['WORKFLOW_SHA'], run_id=run_id,
                   run_attempt=payload_attempt, receipt_run_attempt=sealing_attempt,
                   reservation_run_id=int(env['RESERVATION_RUN_ID']), artifact_id=int(env['ARTIFACT_ID']),
                   artifact_digest=payload_digest, manifest_sha256=digest(root / 'manifest.json'))
    receipt_path.parent.mkdir(parents=True, exist_ok=True)
    receipt_path.write_text(json.dumps(receipt, sort_keys=True) + '\n')
    return receipt


def check_receipt(receipt, manifest_path, expected, sealing_attempt, repository_id):
    """Bind a gate receipt to the verified payload and the attempt that sealed it."""
    schema = receipt.get('schema_version')
    if schema in (2, 3) and receipt.get('gate_identity') != GATE_IDENTITY:
        raise SystemExit('Unsupported gate identity')
    if schema not in (1, 2, 3):
        raise SystemExit('Unsupported receipt schema')
    if any(receipt.get(k) != v for k, v in expected.items()) or receipt.get('manifest_sha256') != digest(manifest_path):
        raise SystemExit('Gate receipt or manifest mismatch')
    if receipt_attempt(receipt) != sealing_attempt or receipt['run_attempt'] > sealing_attempt:
        raise SystemExit('Gate receipt attempt mismatch')
    if schema == 3:
        manifest = json.loads(manifest_path.read_text())
        if receipt.get('repository_id') != repository_id or manifest.get('repository_id') != repository_id:
            raise SystemExit('Gate evidence repository ID mismatch')


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('action', choices=('stage', 'verify', 'seal', 'check-receipt'))
    parser.add_argument('root', type=Path)
    parser.add_argument('--version', required=True)
    parser.add_argument('--receipt', type=Path, help='gate receipt to write (seal) or check (check-receipt)')
    args = parser.parse_args()
    env = os.environ
    if args.action in ('seal', 'check-receipt') and args.receipt is None:
        parser.error(f'--receipt is required with {args.action}')
    if args.action == 'stage':
        stage(args.root, args.version)
    elif args.action == 'seal':
        seal(args.root, args.receipt, args.version, env)
    elif args.action == 'check-receipt':
        # SOURCE_RUN_ATTEMPT is the payload's attempt; RECEIPT_RUN_ATTEMPT sealed the receipt.
        expected = dict(repository=env['SOURCE_REPOSITORY'], version=args.version,
                        source_sha=env['SOURCE_SHA'], workflow_sha=env['WORKFLOW_SHA'],
                        run_id=int(env['SOURCE_RUN_ID']), run_attempt=int(env['SOURCE_RUN_ATTEMPT']),
                        reservation_run_id=int(env['RESERVATION_RUN_ID']))
        verify(args.root, expected)
        check_receipt(json.loads(args.receipt.read_text()), args.root / 'manifest.json',
                      dict(expected, artifact_id=int(env['PAYLOAD_ID'])),
                      attempt_number(env.get('RECEIPT_RUN_ATTEMPT'), 'gate receipt'), int(env['GITHUB_REPOSITORY_ID']))
    else:
        verify(args.root, dict(repository=env.get('SOURCE_REPOSITORY', env['GITHUB_REPOSITORY']), version=args.version,
               source_sha=env['SOURCE_SHA'], workflow_sha=env['WORKFLOW_SHA'],
               run_id=int(env['SOURCE_RUN_ID']), run_attempt=int(env['SOURCE_RUN_ATTEMPT']),
               reservation_run_id=int(env['RESERVATION_RUN_ID'])))


if __name__ == '__main__':
    main()
