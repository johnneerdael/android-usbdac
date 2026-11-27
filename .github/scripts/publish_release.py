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
    results = []
    for suffix in ('.aar', '.pom', '.module', '-sources.jar'):
        remote = fetch(base + suffix, auth)
        local = (folder / f'{name}-{version}{suffix}').read_bytes()
        if remote is not None and hash_bytes(remote) != hash_bytes(local):
            raise SystemExit(f'Conflicting published Maven object: {name}{suffix}')
        results.append(remote is not None)
    if any(results) and not all(results):
        raise SystemExit(f'Partial Maven publication requires manual repair: {name}')
    return all(results)


def classify_crate(root, version):
    data = fetch('https://index.crates.io/us/bd/usbdac-core')
    entries = [] if data is None else [json.loads(line) for line in data.splitlines()]
    matches = [entry for entry in entries if entry['vers'] == version]
    if len(matches) > 1:
        raise SystemExit('Duplicate crate index version')
    if not matches:
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
        if classify_crate(root, version):
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
    if skip and not all(states.values()):
        raise SystemExit('Skipping Maven requires both complete matching publications')

def main():
    env = os.environ
    root = Path('release-payload').resolve()
    version, repo = env['VERSION'], env['GITHUB_REPOSITORY']
    verify(root, dict(repository=repo, version=version, source_sha=env['SOURCE_SHA'], workflow_sha=env['WORKFLOW_SHA'],
        run_id=int(env['SOURCE_RUN_ID']), run_attempt=int(env['SOURCE_RUN_ATTEMPT']),
        reservation_run_id=int(env['RESERVATION_RUN_ID'])))
    auth = 'Basic ' + base64.b64encode(f"{env['GITHUB_ACTOR']}:{env['GITHUB_TOKEN']}".encode()).decode()
    names = ('usbdac', 'usbdac-media3')
    states = {name: classify_maven(root, version, repo, auth, name) for name in names}
    verify_skip(states, env.get('SKIP_PACKAGE') == 'true')
    crate_exists = classify_crate(root, version)
    if not crate_exists:
        publish_crate(root, version)
    with tempfile.TemporaryDirectory() as tmp:
        settings = Path(tmp) / 'settings.xml'
        settings.write_text('<settings><servers><server><id>github</id><username>' +
            xml.sax.saxutils.escape(env['GITHUB_ACTOR']) + '</username><password>' +
            xml.sax.saxutils.escape(env['GITHUB_TOKEN']) + '</password></server></servers></settings>')
        settings.chmod(0o600)
        for name in names:
            if not states[name]:
                publish_maven(root, version, repo, name, settings)
                if not classify_maven(root, version, repo, auth, name):
                    raise SystemExit(f'Maven publication missing after upload: {name}')


if __name__ == '__main__':
    main()
