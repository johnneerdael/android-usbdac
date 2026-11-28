"""Render release notes from the reserved source and verified staged payload."""
import argparse
from pathlib import Path
import subprocess

from release_artifact import verify


SITE = 'https://johnneerdael.github.io/usbdac'
BANNER = 'branding/marketing/usbdac-readme-dark.png'


def render(version, source, changes, repository, validator=False, banner=False):
    lines = []
    if banner:
        # Pinned to the released commit so the image matches the release.
        lines += [f'![usbdac. Direct USB audio. Clear evidence.](https://raw.githubusercontent.com/{repository}/{source}/{BANNER})', '']
    lines += [
        f'**usbdac {version}** · Direct USB audio. Clear evidence.',
        '',
        f'[Quickstart]({SITE}/getting-started.html) · [SDK guide]({SITE}/) · '
        f'[Evidence and status]({SITE}/evidence.html) · [Upgrading]({SITE}/migration.html)',
        '',
        '## Changes', '', *changes, '',
        '## Install', '',
        '```kotlin',
        f'// From GitHub Packages; the quickstart shows the repository setup.',
        f'implementation("dev.usbdac:usbdac:{version}")',
        f'implementation("dev.usbdac:usbdac-media3:{version}") // optional Media3 integration',
        '```',
        '',
        f'```sh\ncargo add usbdac-core@{version}\n```',
        '',
        '## Files', '',
    ]
    for artifact, description in (('usbdac', 'Android library'), ('usbdac-media3', 'optional Media3 integration')):
        lines.append(f'- `{artifact}-{version}.aar`, `-sources.jar`, `.pom`, `.module`: {description}; '
                     f'also published as `dev.usbdac:{artifact}:{version}` on GitHub Packages.')
    lines.extend([
        f'- `usbdac-{version}-jniLibs.zip`: native libraries for arm64-v8a, armeabi-v7a, x86_64 and x86.',
        f'- `usbdac-core-{version}.crate`: Rust driver, also available on [crates.io](https://crates.io/crates/usbdac-core/{version}).',
        '- `device-compatibility-candidates.md`: source-backed device decision list.',
        '- `SHA256SUMS.txt`: checksums of the release assets.',
    ])
    if validator:
        lines.extend([
            '', '## Validators', '',
            'Test your own DAC: start a capture, connect one DAC, and save the report ZIP with device details, '
            'test outcomes and debug logs. Remove headphones and mute the amplifier first.',
            '',
            f'- `usbdac-validator-{version}-android.apk`: Android validator, signed with the pinned validator key (certificate SHA-256 2989c5a4...3a46588e), so it installs over the previous release.',
            f'- `usbdac-validator-{version}-macos-arm64.dmg`: Apple Silicon validator, Developer ID signed and notarized.',
        ])
    lines.extend([
        '', '## Provenance', '',
        f'The five release gates ran on [reserved source `{source}`](https://github.com/{repository}/commit/{source}). '
        f'[manifest.json](https://github.com/{repository}/releases/download/v{version}/manifest.json) records staged file digests and build identity; '
        f'[receipt.json](https://github.com/{repository}/releases/download/v{version}/receipt.json) binds the payload to its Actions run and attempt.',
        'Host JNI and Kotlin tests do not execute the Android ABI binaries.',
    ])
    return '\n'.join(lines) + '\n'


def main():
    import os
    parser = argparse.ArgumentParser()
    parser.add_argument('--output', required=True, type=Path)
    args = parser.parse_args()
    e = os.environ
    version, source = e['VERSION'], e['SOURCE_SHA']
    verify(Path('release-payload'), dict(repository=e['SOURCE_REPOSITORY'], version=version,
        source_sha=source, workflow_sha=e['WORKFLOW_SHA'], run_id=int(e['SOURCE_RUN_ID']),
        run_attempt=int(e['SOURCE_RUN_ATTEMPT']), reservation_run_id=int(e['RESERVATION_RUN_ID'])))
    previous = subprocess.run(['git', 'describe', '--tags', '--abbrev=0', f'{source}^'],
        text=True, capture_output=True, check=False)
    revision = f'{previous.stdout.strip()}..{source}' if previous.returncode == 0 else source
    changes = subprocess.check_output(['git', 'log', '--no-merges', '--pretty=- %s (%h)', revision], text=True).splitlines()
    banner = subprocess.run(['git', 'cat-file', '-e', f'{source}:{BANNER}'], capture_output=True, check=False).returncode == 0
    args.output.write_text(render(version, source, changes, e['PUBLISH_REPOSITORY'], e.get('VALIDATOR') == 'true', banner))


if __name__ == '__main__':
    main()
