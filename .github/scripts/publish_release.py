"""Publish a previously verified gate payload without rebuilding it."""
import base64
import hashlib
import json
import os
from pathlib import Path
import struct
import subprocess
import tempfile
import time
import urllib.error
import urllib.request
import xml.sax.saxutils

from release_artifact import verify


def fetch(url, auth=None):
    request = urllib.request.Request(url, headers={'User-Agent': 'usbdac-release/1'})
    if auth:
        request.add_unredirected_header('Authorization', auth)
    try:
        with urllib.request.urlopen(request, timeout=30) as response:
            return response.read()
    except urllib.error.HTTPError as error:
        if error.code == 404:
            return None
        raise


def hash_bytes(data):
    return hashlib.sha256(data).hexdigest()


def classify_maven(root, version, repo, auth, name):
    folder = root / 'maven' / 'dev' / 'usbdac' / name / version
    base = f'https://maven.pkg.github.com/{repo}/dev/usbdac/{name}/{version}/{name}-{version}'
    present, missing, conflicts = {}, {}, {}
    for suffix in ('.aar', '.pom', '.module', '-sources.jar'):
        filename = f'{name}-{version}{suffix}'
        expected = hash_bytes((folder / filename).read_bytes())
        remote = fetch(base + suffix, auth)
        if remote is None:
            missing[filename] = {'expected': expected, 'observed': None}
        else:
            observed = hash_bytes(remote)
            present[filename] = {'expected': expected, 'observed': observed}
            if observed != expected:
                conflicts[filename] = present[filename]
    state = 'conflict' if conflicts else ('absent' if not present else ('partial_match' if missing else 'complete_match'))
    return {'module': name, 'state': state, 'present': present, 'missing': missing, 'conflicts': conflicts}


def require_publishable(states):
    for state in states.values():
        if state['state'] in ('partial_match', 'conflict'):
            Path('maven-publication-incident.json').write_text(json.dumps(states, sort_keys=True, indent=2) + '\n')
            raise SystemExit(f"Maven coordinate {state['module']} is {state['state']}; retain the sealed payload and incident report, abandon this version and reserve a new patch")


def preflight_release(repo, version, commit, assets, notes, token):
    def get(path):
        raw = fetch('https://api.github.com/' + path, 'Bearer ' + token)
        return json.loads(raw) if raw is not None else None
    tag = get(f'repos/{repo}/git/ref/tags/v{version}')
    release = get(f'repos/{repo}/releases/tags/v{version}')
    if tag is not None:
        if tag.get('ref') != f'refs/tags/v{version}':
            raise SystemExit('Unexpected tag reference')
        obj, seen = tag.get('object') or {}, set()
        for _ in range(16):
            if obj.get('type') != 'tag':
                break
            sha = obj.get('sha')
            if not sha or sha in seen:
                raise SystemExit('Annotated tag cycle or missing identity')
            seen.add(sha)
            annotated = get(f'repos/{repo}/git/tags/{sha}')
            if annotated is None:
                raise SystemExit('Missing annotated tag object')
            obj = annotated.get('object') or {}
        if obj.get('type') != 'commit' or obj.get('sha') != commit:
            raise SystemExit('Existing tag does not resolve to expected source')
    if release is not None:
        entries = release.get('assets', [])
        actual = {a['name']: a.get('digest') for a in entries}
        expected = {p.name: 'sha256:' + hash_bytes(p.read_bytes()) for p in assets.iterdir() if p.is_file()}
        if (release.get('draft') or release.get('prerelease') or tag is None or len(actual) != len(entries)
                or actual != expected or (release.get('body') or '').strip() != notes.read_text().strip()):
            raise SystemExit('Existing release, tag, notes or assets conflict')


def classify_crate(root, version, post_upload=False):
    data = fetch('https://index.crates.io/us/bd/usbdac-core')
    if data is None:
        raise SystemExit('Crate index is unavailable; publication state is uncertain')
    entries = [json.loads(line) for line in data.splitlines()]
    matches = [entry for entry in entries if entry['vers'] == version]
    if len(matches) > 1:
        raise SystemExit('Duplicate crate index version')
    if not matches:
        if not post_upload and fetch(f'https://static.crates.io/crates/usbdac-core/usbdac-core-{version}.crate') is not None:
            raise SystemExit('Crate download exists without an index entry; publication state is uncertain')
        return False
    entry = matches[0]
    expected = hash_bytes((root / 'crates' / f'usbdac-core-{version}.crate').read_bytes())
    if entry.get('yanked') or entry.get('cksum') != expected:
        raise SystemExit('Published crate is yanked or has a different checksum')
    downloaded = fetch(f'https://static.crates.io/crates/usbdac-core/usbdac-core-{version}.crate')
    if downloaded is None or hash_bytes(downloaded) != expected:
        raise SystemExit('Published crate download is missing or differs')
    return True


def crate_request(metadata, crate):
    meta = metadata.read_bytes()
    data = crate.read_bytes()
    if max(len(meta), len(data)) >= 2**32:
        raise SystemExit('Registry request exceeds protocol limits')
    return struct.pack('<I', len(meta)) + meta + struct.pack('<I', len(data)) + data


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        raise urllib.error.HTTPError(req.full_url, code, 'Authenticated upload redirect rejected', headers, fp)


def publish_crate(root, version):
    payload = crate_request(root / 'crates' / f'usbdac-core-{version}.publish.json', root / 'crates' / f'usbdac-core-{version}.crate')
    request = urllib.request.Request('https://crates.io/api/v1/crates/new', data=payload, method='PUT', headers={
        'Content-Type': 'application/octet-stream', 'Accept': 'application/json', 'User-Agent': 'usbdac-release/1'})
    request.add_unredirected_header('Authorization', os.environ['CARGO_REGISTRY_TOKEN'])
    with urllib.request.build_opener(NoRedirect).open(request, timeout=60) as response:
        result = json.load(response)
    if result.get('errors'):
        raise SystemExit('Registry rejected crate publication')
    for _ in range(30):
        if classify_crate(root, version, post_upload=True):
            return
        time.sleep(10)
    raise SystemExit('Crate publication is pending; do not retry until registry state is known')


def publish_maven(root, version, repo, name, settings):
    base = root / 'maven' / 'dev' / 'usbdac' / name / version / f'{name}-{version}'
    subprocess.run(['mvn', '--batch-mode', '--settings', str(settings),
        'org.apache.maven.plugins:maven-deploy-plugin:3.2.0:deploy-file',
        '-DrepositoryId=github', f'-Durl=https://maven.pkg.github.com/{repo}',
        f'-Dfile={base}.aar', f'-DpomFile={base}.pom', '-Dpackaging=aar', '-Dextension=aar',
        '-DgeneratePom=false', f'-Dfiles={base}-sources.jar,{base}.module',
        '-Dtypes=jar,module', '-Dclassifiers=sources,'], check=True)



def verify_skip(states, skip):
    if skip and not all(state['state'] == 'complete_match' for state in states.values()):
        raise SystemExit('Skipping Maven requires both complete matching publications')

def main():
    env = os.environ
    root = Path('release-payload').resolve()
    version, repo = env['VERSION'], env['PUBLISH_REPOSITORY']
    verify(root, dict(repository=env['SOURCE_REPOSITORY'], version=version, source_sha=env['SOURCE_SHA'], workflow_sha=env['WORKFLOW_SHA'],
        run_id=int(env['SOURCE_RUN_ID']), run_attempt=int(env['SOURCE_RUN_ATTEMPT']),
        reservation_run_id=int(env['RESERVATION_RUN_ID'])))
    auth = 'Basic ' + base64.b64encode(f"{env['GITHUB_ACTOR']}:{env['GITHUB_TOKEN']}".encode()).decode()
    names = ('usbdac', 'usbdac-media3')
    states = {name: classify_maven(root, version, repo, auth, name) for name in names}
    require_publishable(states)
    verify_skip(states, env.get('SKIP_PACKAGE') == 'true')
    crate_exists = classify_crate(root, version)
    preflight_release(repo, version, env['SOURCE_SHA'], Path('release-assets'), Path('release-notes.md'), env['GITHUB_TOKEN'])
    from prepare_release import git, reservations, require_latest_reservation
    git('fetch', '--quiet', 'origin', 'main')
    if reservations('origin/main', version):
        require_latest_reservation('origin/main', version, env['SOURCE_SHA'])
    if not crate_exists:
        publish_crate(root, version)
    with tempfile.TemporaryDirectory() as tmp:
        settings = Path(tmp) / 'settings.xml'
        settings.write_text('<settings><servers><server><id>github</id><username>' +
            xml.sax.saxutils.escape(env['GITHUB_ACTOR']) + '</username><password>' +
            xml.sax.saxutils.escape(env['GITHUB_TOKEN']) + '</password></server></servers></settings>')
        settings.chmod(0o600)
        for name in names:
            if states[name]['state'] == 'absent':
                try:
                    publish_maven(root, version, repo, name, settings)
                except subprocess.CalledProcessError:
                    pass
                for retry in range(3):
                    try:
                        states[name] = classify_maven(root, version, repo, auth, name)
                        break
                    except (OSError, urllib.error.URLError):
                        if retry == 2:
                            raise
                        time.sleep(5)
                require_publishable(states)
                if states[name]['state'] != 'complete_match':
                    raise SystemExit(f'Maven publication unavailable after upload: {name}; retain payload and investigate before retry')


if __name__ == '__main__':
    main()
