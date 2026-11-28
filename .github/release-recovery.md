# Release recovery and documentation

Automatic releases reserve a version on main before gating. A package-only recovery uses `failed_run_id` and, when necessary, `gate_run_id` to select previously sealed evidence. Use `regate_run_id` to run all gates again on the original reserved source when retained evidence has expired. Keep the sealed payload and receipt; do not replace published assets or reuse a version with different bytes.

A partial Maven publication is an abandoned coordinate, not an ordinary retry. Save the `maven-publication-incident` artifact and sealed evidence, then reserve a new patch and run all gates for its new source. Do not create the release page for the partial coordinate. Cross-service publication is not atomic: external writes can race preflight checks.

The Pages workflow deploys only verified published release source. Documentation-only changes on main deploy when the library build inputs still match the latest published tag and the guide names its published coordinates. A push with unpublished library inputs or coordinates skips deployment until publication. After an externally created release, or to retry a failed documentation deployment, dispatch the workflow on main with its exact published version and peeled source commit:

```sh
gh workflow run docs.yml --repo johnneerdael/usbdac --ref main -f published_version=VERSION -f source_sha=COMMIT_SHA
```

The workflow checks the stable tag, published release, evidence and download coordinates before building. Legacy releases whose receipts do not contain a repository ID cannot use this deployment path; deploy a newer release with current evidence instead.
