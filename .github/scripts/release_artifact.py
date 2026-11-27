"""Create and verify the immutable release-gate payload (schema 1)."""

import argparse
import hashlib
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
    manifest = {"schema_version": 1, "repository": env['GITHUB_REPOSITORY'],
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


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('action', choices=('stage', 'verify'))
    parser.add_argument('root', type=Path)
    parser.add_argument('--version', required=True)
    args = parser.parse_args()
    if args.action == 'stage':
        stage(args.root, args.version)
    else:
        env = os.environ
        verify(args.root, dict(repository=env['GITHUB_REPOSITORY'], version=args.version,
               source_sha=env['SOURCE_SHA'], workflow_sha=env['WORKFLOW_SHA'],
               run_id=int(env['SOURCE_RUN_ID']), run_attempt=int(env['SOURCE_RUN_ATTEMPT']),
               reservation_run_id=int(env['RESERVATION_RUN_ID'])))


if __name__ == '__main__':
    main()
