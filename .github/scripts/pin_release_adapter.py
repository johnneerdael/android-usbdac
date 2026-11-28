"""Pin, re-pin or check the reviewed release implementation (release_adapters.json)."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import sys

SCRIPTS = Path(__file__).resolve().parent
REPO = SCRIPTS.parent.parent
# `python3 -I` leaves the script directory off sys.path.
sys.path.insert(0, str(SCRIPTS))
from prepare_release import RELEASE_IMPLEMENTATION, matches_adapter, stable_tags  # noqa: E402

ADAPTERS = SCRIPTS / 'release_adapters.json'
GATE_IDENTITY = 'usbdac-release-gates/v1'
SCHEMA = 3


def current_files():
    return {path: hashlib.sha256((REPO / path).read_bytes()).hexdigest() for path in RELEASE_IMPLEMENTATION}


def pinned(adapters, files):
    return any(a.get('schema') == SCHEMA and a.get('files') == files for a in adapters)


def released_tag(adapter):
    """Return the stable tag whose release implementation is this pin, if any."""
    for tag in reversed(stable_tags()):
        if matches_adapter(tag, adapter.get('files')):
            return tag
    return None


def write(adapters):
    ADAPTERS.write_text(json.dumps(adapters, indent=2, sort_keys=True) + '\n')


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    mode = parser.add_mutually_exclusive_group()
    mode.add_argument('--check', action='store_true', help='fail unless the working tree is pinned')
    mode.add_argument('--amend', action='store_true', help='replace the newest pin if no release used it')
    args = parser.parse_args(argv)
    # Tags and `git show` resolve against this repository wherever the script runs from.
    previous = os.getcwd()
    os.chdir(REPO)
    try:
        pin(args)
    finally:
        os.chdir(previous)


def pin(args):
    adapters = json.loads(ADAPTERS.read_text())
    files = current_files()
    if pinned(adapters, files):
        print('The release implementation is pinned')
        return
    if args.check:
        raise SystemExit('The release implementation differs from every reviewed pin. Review the change, then run '
                         '`python3 -I .github/scripts/pin_release_adapter.py`, with --amend if the newest pin is '
                         'unreleased (see .github/release-recovery.md)')
    entry = {'files': files, 'gate_identity': GATE_IDENTITY, 'schema': SCHEMA}
    if args.amend:
        if not stable_tags():
            raise SystemExit('No release tags are available to prove the newest pin unreleased; fetch tags first')
        tag = released_tag(adapters[-1])
        if tag is not None:
            # Recovery verifies runs of a released implementation against its pin, so keep it.
            raise SystemExit(f'The newest pin is the release implementation of {tag}; '
                             'run without --amend to add a pin')
        adapters[-1] = entry
        print('Replaced the newest unreleased pin')
    else:
        adapters.append(entry)
        print('Added a pin for the release implementation')
    write(adapters)


if __name__ == '__main__':
    main()
