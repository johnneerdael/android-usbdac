"""Select immutable source for the release build and reserve automatic patch versions."""

import argparse
from pathlib import Path
import re
import subprocess


STABLE_VERSION = re.compile(r"(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)")
WORKSPACE_VERSION = re.compile(r'(?m)(^\[workspace\.package\]\n(?:[^\[\n][^\n]*\n)*?^version = ")([^"\n]+)(")')
GRADLE_VERSION = re.compile(r"(?m)(^VERSION_NAME=)([^\n]+)()")
LOCK_VERSION = re.compile(r'(?m)(^name = "usbdac-(?:core|jni)"\nversion = ")([^"\n]+)(")')
VERSION_FILES = {
    Path("Cargo.toml"): (WORKSPACE_VERSION, 1),
    Path("Cargo.lock"): (LOCK_VERSION, 2),
    Path("android/gradle.properties"): (GRADLE_VERSION, 1),
}
PACKAGE_PATHS = (
    "Cargo.toml", "Cargo.lock", "LICENSE", "NOTICE", "crates",
    "android/usbdac", "android/build.gradle.kts", "android/settings.gradle.kts",
    "android/gradle.properties", "android/gradle/wrapper", "android/gradlew",
    "android/gradlew.bat",
)


def git(*args):
    return subprocess.check_output(["git", *args], text=True).strip()


def version_tuple(version):
    match = STABLE_VERSION.fullmatch(version)
    if match is None:
        raise SystemExit(f"Expected a MAJOR.MINOR.PATCH version, got {version!r}")
    return tuple(int(component) for component in match.groups())


def declared_version():
    versions = []
    for path, (pattern, count) in VERSION_FILES.items():
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
    for path, (pattern, _) in VERSION_FILES.items():
        path.write_text(pattern.sub(lambda match: match[1] + new + match[3], path.read_text()))

    # Keep the shipped integration examples in step with the released version.
    examples = {
        "README.md": ["usbdac-core@{version}"],
        "crates/usbdac-core/README.md": ["usbdac-core@{version}"],
        "docs/API.md": ["usbdac-core@{version}"],
        "docs/sdk/index.md": ["This guide covers usbdac {version}."],
        "docs/sdk/getting-started.md": [
            "dev.usbdac:usbdac:{version}", "usbdac-{version}.aar", "v{version})",
            "[{version} release]",
        ],
    }
    for name, literals in examples.items():
        path = Path(name)
        content = path.read_text()
        updated = content
        for template in literals:
            for previous in {old, last_release}:
                updated = updated.replace(template.format(version=previous), template.format(version=new))
        if updated != content:
            path.write_text(updated)


def select_automatic(run_id):
    marker = f"Release-Run: {run_id}"
    for _ in range(4):
        git("fetch", "--quiet", "--tags", "origin", "main")
        head = git("rev-parse", "FETCH_HEAD")
        reserved = git("log", head, "-1", "--format=%H", f"--grep=^{marker}$")
        git("checkout", "--detach", "--quiet", reserved or head)
        if reserved:
            # A re-run must use the original commit, even if a registry
            # accepted its version before the previous run stopped.
            return declared_version()

        previous = stable_tags("--merged", "HEAD")
        if previous and not git("diff", "--name-only", previous[-1], "HEAD", "--", *PACKAGE_PATHS):
            print("The latest release already includes the library sources on main")
            return None

        current = declared_version()
        numeric = version_tuple(current)
        tags = stable_tags()
        newest = version_tuple(tags[-1][1:]) if tags else (0, 0, 0)
        version_commit = git("log", "-1", "--format=%B", "--", "Cargo.toml")
        if numeric > newest and not any(line.startswith("Release-Run: ") for line in version_commit.splitlines()):
            # A maintainer can declare the next major or minor version in a merge.
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
    mode = parser.add_mutually_exclusive_group(required=True)
    mode.add_argument("--run-id", type=int)
    mode.add_argument("--version")
    args = parser.parse_args()

    if args.run_id is not None:
        version = select_automatic(args.run_id)
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
