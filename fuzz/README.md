# Defensive USB parser fuzzing

`cargo fuzz run defensive_parsers -- -max_total_time=60 -max_len=4096 -timeout=10`
exercises the production descriptor parsers, bounded clock graph walks, feedback
decoders, and UAC2/ADC3 control replies without a device or writes. The `fuzzing`
feature exposes only this defensive adapter; default SDK builds omit it.
Committed regression seeds also run in ordinary core unit tests. CI retains
crash artifacts for repair. A bounded clean run is evidence for that corpus and
revision, not a proof that arbitrary device data is safe.

The manually dispatched `Extended defensive USB fuzz evidence` workflow offers
10, 60 or 180 minute ASan runs. It records the source revision, compiler, sanitized
diagnostic signatures/source locations/numeric metrics and, on success, a SHA-256
corpus inventory. Generated input bytes, raw fuzzer output, crash samples and
failed-run corpora are never published by these workflows. Dispatch it on the release
candidate revision for longer evidence; availability of the workflow is not a
claim that it has completed. Its hard per-input limit remains 4096 bytes and
10 seconds; the normal PR gate remains a bounded 60 second run.

The adapter exercises streaming descriptors for UAC1/2/3, UAC1 rate replies, UAC2 volume ranges, and Scarlett reply and mux parsing in addition to descriptor and feedback parsers. These are pure bounded parsing paths; no hardware writes occur.
