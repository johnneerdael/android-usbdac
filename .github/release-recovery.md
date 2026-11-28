# Release recovery and documentation

Automatic releases reserve a version on main before gating. A package-only recovery uses `failed_run_id` and, when necessary, `gate_run_id` to select previously sealed evidence. Use `regate_run_id` to run all gates again on the original reserved source when retained evidence has expired. Keep the sealed payload and receipt; do not replace published assets or reuse a version with different bytes.

A partial Maven publication is an abandoned coordinate, not an ordinary retry. Save the `maven-publication-incident` artifact and sealed evidence, then reserve a new patch and run all gates for its new source. Do not create the release page for the partial coordinate. Cross-service publication is not atomic: external writes can race preflight checks.

The Pages workflow deploys only verified published release source. Documentation-only changes on main deploy when the library build inputs still match the latest published tag and the guide names its published coordinates. A push with unpublished library inputs or coordinates skips deployment until publication. After an externally created release, or to retry a failed documentation deployment, dispatch the workflow on main with its exact published version and peeled source commit:

```sh
gh workflow run docs.yml --repo johnneerdael/usbdac --ref main -f published_version=VERSION -f source_sha=COMMIT_SHA
```

The workflow checks the stable tag, published release, evidence and download coordinates before building. Legacy releases whose receipts do not contain a repository ID cannot use this deployment path; deploy a newer release with current evidence instead.

## Re-pinning the release implementation

The Release workflow refuses to run unless the release implementation at its commit matches a reviewed pin in `.github/scripts/release_adapters.json`. The pinned files are listed in `RELEASE_IMPLEMENTATION` in `.github/scripts/prepare_release.py`: `release.yml`, `ci.yml` and the release scripts. Any edit to one of them, including an unrelated CI change, needs a new pin in the same pull request. CI checks this before merge.

After reviewing the change, pin it and check the result from the repository root:

```sh
python3 -I .github/scripts/pin_release_adapter.py          # add a pin for the working tree
python3 -I .github/scripts/pin_release_adapter.py --check  # what CI runs
python3 -B -m unittest discover -s .github/scripts/tests
```

While a branch is still changing, `--amend` replaces the newest pin instead of adding another. It refuses when the newest pin is the implementation of a released tag, because package-only recovery of runs on that implementation verifies them against the pin. Fetch tags first. Never remove a released pin.
