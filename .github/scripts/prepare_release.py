"""Select immutable source for the release build and reserve automatic patch versions."""

import argparse
import base64
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess


STABLE_VERSION = re.compile(r"(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)")
WORKSPACE_VERSION = re.compile(r'(?m)(^\[workspace\.package\]\n(?:[^\[\n][^\n]*\n)*?^version = ")([^"\n]+)(")')
GRADLE_VERSION = re.compile(r"(?m)(^VERSION_NAME=)([^\n]+)()")
LOCK_VERSION = re.compile(r'(?m)(^name = "usbdac-(?:core|jni|validator)"\nversion = ")([^"\n]+)(")')
WORKER_LOCK_VERSION = re.compile(r'(?m)(^name = "usbdac-(?:core|validator|validator-worker)"\nversion = ")([^"\n]+)(")')
WORKER_VERSION = re.compile(r'(?m)(^version = ")([^"\n]+)(")')
VERSION_FILES = {
    Path("Cargo.toml"): (WORKSPACE_VERSION, 1),
    Path("Cargo.lock"): (LOCK_VERSION, 2),
    Path("android/gradle.properties"): (GRADLE_VERSION, 1),
}
PACKAGE_PATHS = (
    "Cargo.toml", "Cargo.lock", "LICENSE", "NOTICE", "crates",
    "android/usbdac", "android/usbdac-media3", "android/build.gradle.kts", "android/settings.gradle.kts",
    "android/gradle.properties", "android/gradle/wrapper", "android/gradlew",
    "android/gradlew.bat", "android/validator", "macos/validator",
)


def git(*args):
    return subprocess.check_output(["git", *args], text=True).strip()


def version_tuple(version):
    match = STABLE_VERSION.fullmatch(version)
    if match is None:
        raise SystemExit(f"Expected a MAJOR.MINOR.PATCH version, got {version!r}")
    return tuple(int(component) for component in match.groups())


def version_files(has_validator):
    files = dict(VERSION_FILES)
    if has_validator:
        files[Path("Cargo.lock")] = (LOCK_VERSION, 3)
        files[Path("macos/validator/worker/Cargo.toml")] = (WORKER_VERSION, 1)
        files[Path("macos/validator/worker/Cargo.lock")] = (WORKER_LOCK_VERSION, 3)
    return files


def declared_version():
    versions = []
    for path, (pattern, count) in version_files(Path("crates/usbdac-validator/Cargo.toml").exists()).items():
        values = [match[2] for match in pattern.finditer(path.read_text())]
        if len(values) != count:
            raise SystemExit(f"Could not read the usbdac version from {path}")
        versions.extend(values)
    if len(set(versions)) != 1:
        raise SystemExit("Cargo.toml, Cargo.lock and android/gradle.properties must agree")
    version_tuple(versions[0])
    return versions[0]


def stable_tags(*options):
    tags = git("tag", "--list", "v*", *options).splitlines()
    return sorted(
        (tag for tag in tags if STABLE_VERSION.fullmatch(tag[1:])),
        key=lambda tag: version_tuple(tag[1:]),
    )


def set_version(old, new, last_release):
    for path, (pattern, _) in version_files(Path("crates/usbdac-validator/Cargo.toml").exists()).items():
        path.write_text(pattern.sub(lambda match: match[1] + new + match[3], path.read_text()))

    # The sealed package and its source examples must carry the selected version.
    examples = {
        "README.md": ["usbdac-core@{version}"],
        "crates/usbdac-core/README.md": ["usbdac-core@{version}"],
        "docs/API.md": ["usbdac-core@{version}"],
        "docs/sdk/guide-rust.md": ["usbdac-core@{version}"],
        "docs/sdk/index.md": ["This guide covers usbdac {version}."],
        "docs/sdk/getting-started.md": [
            "dev.usbdac:usbdac:{version}", "dev.usbdac:usbdac-media3:{version}", "usbdac-{version}.aar", "v{version})",
            "[{version} release]",
        ],
    }
    for name, literals in examples.items():
        path = Path(name)
        content = path.read_text()
        updated = content
        for template in literals:
            pattern = re.escape(template).replace(re.escape("{version}"), STABLE_VERSION.pattern)
            updated = re.sub(pattern, lambda _: template.format(version=new), updated)
        if updated != content:
            path.write_text(updated)


# Only these reviewed layouts prove that a skipped package job cannot publish.
HISTORICAL_WORKFLOWS = (
    '7047b6b2b143389e13e695c37385c7ecfde943a8',
    '0733689bd30c5e9ffd192947bc4803c380dbdead',
)
RELEASE_IMPLEMENTATION = (
    '.github/workflows/release.yml', '.github/workflows/ci.yml',
    '.github/scripts/prepare_release.py', '.github/scripts/recover_release.py',
    '.github/scripts/release_artifact.py', '.github/scripts/publish_release.py',
    '.github/scripts/release_notes.py',
)
PACKAGE_JOB = 'Publish verified gate payload'


def reservations(head, version):
    """Return first-parent reservations newest first, including empty commits."""
    commits = git('log', '--first-parent', '--format=%H', head,
                  '--fixed-strings', f'--grep=Release usbdac {version}').splitlines()
    found = []
    for commit in commits:
        message = git('show', '-s', '--format=%B', commit).splitlines()
        if not message or message[0] != f'Release usbdac {version}':
            continue
        markers = [line for line in message if line.startswith('Release-Run: ')]
        if len(markers) != 1 or not re.fullmatch(r'Release-Run: [1-9][0-9]*', markers[0]):
            raise ValueError('Ambiguous reservation marker')
        found.append((commit, int(markers[0].split(': ')[1])))
    return found


def require_latest_reservation(head, version, candidate):
    records = reservations(head, version)
    if not records or records[0][0] != candidate:
        raise SystemExit('Reservation has been superseded or cannot be verified')


def matches_adapter(sha, adapter):
    if not isinstance(adapter, dict) or set(adapter) != set(RELEASE_IMPLEMENTATION) or any(not isinstance(value, str) or re.fullmatch(r'[0-9a-f]{64}', value) is None for value in adapter.values()):
        return False
    try:
        return all(hashlib.sha256(subprocess.check_output(['git', 'show', f'{sha}:{path}'], stderr=subprocess.PIPE)).hexdigest() == digest
                   for path, digest in adapter.items())
    except subprocess.CalledProcessError:
        return False


def reviewed_adapters():
    return json.loads(Path(__file__).with_name('release_adapters.json').read_text())


def trusted_workflow(sha):
    for adapter in reviewed_adapters():
        if matches_adapter(sha, adapter['files']):
            return True
    for ref in HISTORICAL_WORKFLOWS:
        try:
            if all(subprocess.check_output(['git', 'show', f'{sha}:{path}'], stderr=subprocess.PIPE)
                   == subprocess.check_output(['git', 'show', f'{ref}:{path}'], stderr=subprocess.PIPE)
                   for path in RELEASE_IMPLEMENTATION):
                return True
        except subprocess.CalledProcessError:
            continue
    return False


def api_json(*args):
    return json.loads(subprocess.check_output(['gh', 'api', *args], text=True, stderr=subprocess.PIPE))


def never_published(repo, version, records):
    """Require every attempt of each reservation to have skipped the publisher."""
    if not records:
        return False
    earliest = None
    repo_id = os.environ.get('GITHUB_REPOSITORY_ID')
    if repo_id:
        if not repo_id.isdigit() or api_json(f'repositories/{repo_id}').get('full_name') != repo:
            return False
        repo_id = int(repo_id)
    for commit, run_id in records:
        run = api_json(f'repos/{repo}/actions/runs/{run_id}')
        parent = git('show', '-s', '--format=%P', commit).split()
        if (len(parent) != 1 or run.get('id') != run_id or run.get('head_sha') != parent[0]
                or run.get('path') != '.github/workflows/release.yml' or run.get('event') != 'push'
                or run.get('head_branch') != 'main' or run.get('status') != 'completed'
                or run.get('conclusion') != 'failure'
                or (repo_id is not None and (run.get('repository', {}).get('id') != repo_id or run.get('head_repository', {}).get('id') != repo_id))
                or (repo_id is None and (run.get('repository', {}).get('full_name') != repo or run.get('head_repository', {}).get('full_name') != repo))
                or not trusted_workflow(parent[0])):
            return False
        created = run.get('created_at')
        if not isinstance(created, str) or not re.fullmatch(r'\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}Z', created):
            return False
        earliest = min(earliest, created) if earliest else created
        attempts = run.get('run_attempt')
        if not isinstance(attempts, int) or isinstance(attempts, bool) or attempts < 1:
            return False
        for attempt in range(1, attempts + 1):
            detail = api_json(f'repos/{repo}/actions/runs/{run_id}/attempts/{attempt}')
            if (detail.get('id') != run_id or detail.get('run_attempt') != attempt
                    or detail.get('status') != 'completed' or detail.get('conclusion') != 'failure'):
                return False
            pages = api_json('--paginate', '--slurp', f'repos/{repo}/actions/runs/{run_id}/attempts/{attempt}/jobs?per_page=100')
            jobs = [job for page in pages for job in page['jobs'] if job.get('name') == PACKAGE_JOB]
            if len(jobs) != 1 or jobs[0].get('status') != 'completed' or jobs[0].get('conclusion') != 'skipped':
                return False
    # A manual recovery can publish the same reservation under a different run
    # ID. Refuse adoption if any dispatch occurred since the first reservation;
    # without proving its inputs and all attempts it is not safe to reuse.
    pages = api_json('--paginate', '--slurp',
        f'repos/{repo}/actions/workflows/release.yml/runs?event=workflow_dispatch&per_page=100')
    for page in pages:
        for run in page['workflow_runs']:
            created = run.get('created_at')
            if not isinstance(created, str) or created >= earliest:
                return False
    return True


def registry_absent(repo, version, reference):
    # A 404 alone can hide an authorization failure. Prove the same credentials
    # can read every object of the latest published Maven version first.
    from publish_release import fetch
    token, actor = os.environ['GH_TOKEN'], os.environ['GITHUB_ACTOR']
    auth = 'Basic ' + base64.b64encode(f'{actor}:{token}'.encode()).decode()
    if not reference or git('ls-remote', '--tags', 'origin', f'refs/tags/v{version}'):
        return False
    # Cargo sparse index: a name of four or more characters lives at
    # {first two}/{next two}/{name}, one JSON line per published version
    # (yanked ones included), keyed by "vers".
    index = fetch('https://index.crates.io/us/bd/usbdac-core')
    if index is None:
        return False
    if any(entry.get('vers') == version for entry in (json.loads(line) for line in index.splitlines())):
        return False
    if fetch(f'https://static.crates.io/crates/usbdac-core/usbdac-core-{version}.crate') is not None:
        return False
    for name in ('usbdac', 'usbdac-media3'):
        for suffix in ('.aar', '.pom', '.module', '-sources.jar'):
            base = f'https://maven.pkg.github.com/{repo}/dev/usbdac/{name}'
            if fetch(f'{base}/{reference}/{name}-{reference}{suffix}', auth) is None:
                return False
            if fetch(f'{base}/{version}/{name}-{version}{suffix}', auth) is not None:
                return False
    return True


def can_adopt(head, version, reference):
    try:
        records = reservations(head, version)
        return (never_published(os.environ['GITHUB_REPOSITORY'], version, records)
                and registry_absent(os.environ['GITHUB_REPOSITORY'], version, reference))
    except (OSError, ValueError, KeyError, TypeError, subprocess.CalledProcessError):
        # API errors, missing permissions and unverifiable state never authorize reuse.
        return False


def select_automatic(run_id, source):
    marker = f"Release-Run: {run_id}"
    for _ in range(4):
        git("fetch", "--quiet", "--tags", "origin", "main")
        head = git("rev-parse", "FETCH_HEAD")
        reserved = git("log", head, "-1", "--format=%H", f"--grep=^{marker}$")
        if not reserved and head != source:
            raise SystemExit("main advanced since this workflow started; the newer push will run its own release")
        git("checkout", "--detach", "--quiet", reserved or head)
        if reserved:
            # A re-run must use the original commit, even if a registry
            # accepted its version before the previous run stopped.
            version = declared_version()
            require_latest_reservation(head, version, reserved)
            return version

        previous = stable_tags("--merged", "HEAD")
        if previous and not git("diff", "--name-only", previous[-1], "HEAD", "--", *PACKAGE_PATHS):
            print("The latest release already includes the library sources on main")
            return None

        current = declared_version()
        numeric = version_tuple(current)
        tags = stable_tags()
        newest = version_tuple(tags[-1][1:]) if tags else (0, 0, 0)
        reference = previous[-1][1:] if previous else None
        records, eligible = None, False
        if numeric > newest:
            try:
                records = reservations(head, current)
                eligible = (can_adopt(head, current, reference) if records else
                            registry_absent(os.environ['GITHUB_REPOSITORY'], current, reference))
            except (OSError, ValueError, KeyError, TypeError, subprocess.CalledProcessError):
                eligible = False
            if not eligible and records == []:
                # Nothing reserved this version, so a maintainer declared it above every
                # tag (SemVer 2.0.0 section 2 orders X.Y.Z numerically). Section 3 forbids
                # reusing a released version, so it needs proof of absence, but bumping to
                # its next patch would replace the declared version (a new minor X.Y.0
                # would ship as X.Y.1). Stop and leave the choice to the maintainer.
                raise SystemExit(f'Declared version {current} is newer than every tag but the registries '
                                 'could not prove it unpublished; re-run when they are reachable, or '
                                 'declare a different version')
        if eligible:
            version = current
        else:
            major, minor, patch = max(numeric, newest)
            version = f"{major}.{minor}.{patch + 1}"

        set_version(current, version, previous[-1][1:] if previous else current)
        git("add", "--update")
        git(
            "-c", "user.name=github-actions[bot]",
            "-c", "user.email=41898282+github-actions[bot]@users.noreply.github.com",
            "commit", "--quiet", "--allow-empty", "-m", f"Release usbdac {version}", "-m", marker,
        )
        push = subprocess.run(["git", "push", "origin", "HEAD:refs/heads/main"], check=False)
        if push.returncode == 0:
            return version
        if git("ls-remote", "origin", "refs/heads/main").split()[0] == head:
            raise SystemExit("Could not reserve the release version on main")
        print("main advanced while reserving the version, retrying from its new head")
    raise SystemExit("main advanced repeatedly while reserving the release version")


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--source", help="triggering commit for automatic reservation")
    mode = parser.add_mutually_exclusive_group(required=True)
    mode.add_argument("--run-id", type=int)
    mode.add_argument("--version")
    args = parser.parse_args()

    if args.run_id is not None:
        if not args.source:
            parser.error("--source is required with --run-id")
        version = select_automatic(args.run_id, args.source)
    else:
        version = args.version
        version_tuple(version)
        tag = f"v{version}"
        if tag in git("tag", "--list", tag).splitlines():
            git("checkout", "--detach", "--quiet", f"refs/tags/{tag}")
        if declared_version() != version:
            raise SystemExit(f"The release source must declare version {version}")

    with args.output.open("a") as output:
        if version is not None:
            commit = git("rev-parse", "HEAD")
            print(f"Release {version} from {commit}")
            print(f"version={version}", f"commit={commit}", sep="\n", file=output)


if __name__ == "__main__":
    main()
