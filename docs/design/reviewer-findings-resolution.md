# Resolution design for the usbdac 0.1.10 review

The review baseline is `84197a2`, with release-fix changes through `45a3f2c`. The grounding handover identifies each finding, source reference and evidence boundary. This design follows those dispositions on the development branch.

## Clock and playback

- Q01–Q02 and K02 separate capability hints, acknowledged SET and live active-alternate frequency evidence. Normalize an ambiguous UAC2 zero-resolution RANGE to its positive minimum for admission only. Scope the legacy ALSA sampling-rate flag to UAC1. Preserve application opt-in for an acknowledged SET with unusable readback, with an unverified guarantee. A known positive active mismatch stays material until an exact readback clears it.
- Q03 preserves the lower-first rate order. Q04 and A03 retry only a candidate whose clean refusal or complete rollback is proven. Bound the distinct candidates and the elapsed transaction. Keep device removal, claim failure and uncertain configuration terminal.
- T01–T03 and T05–T06 retain the documented packet and feedback checks, sparse-loss budget and actual queue geometry. A change to feedback encoding or larger queue depth needs evidence from the exact behavior it addresses. T04 exposes pump elapsed time and timeout overrun separately from the work loop gap so intentional waits remain interpretable.
- K01, K03–K05 retain source-backed flags as explicit, scoped compatibility. Match provenance, admitted behavior and device-verified behavior each have their own meaning. Imported rows alone establish neither native DSD layout nor the right to perform vendor writes.
- X01 keeps nominal UAC1 asynchronous playback in its unverified tier. Host starvation and DAC FIFO drift require different evidence.

## Capture and USB ownership

- C01 can tolerate one isolated timing gap after trusted cadence. Keep only genuine measured patterns, invalidate live feedback lock, preserve valid IN audio separately and count failed transfers without inferring their payload. Real timing requalification replenishes the recovery allowance. FIFO exhaustion or expiry reports a discontinuity.
- C02 uses a bounded single-producer ring for the duplex reader. The USB worker publishes complete frames without waiting for the consumer. The ring drops newly arriving frames only when all configured capacity is occupied. C06 accounts for frames withheld during rate or startup proof on the captured timeline and distinguishes delivery, transport loss and overflow.
- C03 resolves shared UAC1 nodes with bounded memoization. C04 tests clock IDs against addressable UAC3 entities. C05 classifies interrupt errors and capture faults separately. A proven independent clock and owned retirement path are prerequisites for keeping playback alive when capture fails.
- C07–C09 and C11–C15 preserve supported timing, clock, channel and stopped-stream contracts. C10 permits representable signed PCM where capture layout is proven. A legacy USB configuration fallback is opt-in and uses an idle device-wide owner, the advertised configuration value, and verified restoration.
- N01–N04 retain transfer buffers and handles until native ownership returns. The return signal survives optional metadata failure and is independent of one-shot result consumption. Mutable JNI destinations require writable, exclusive storage. Keep ioctl encodings tied to the platform UAPI.

## Routing and reconfiguration

- R01 preserves USB-IF mixer addresses. R02–R03 retain actual pre-write raw values, bounded current ownership evidence and a per-control restoration report. Require live identity and dependency checks before writing back a value.
- R04–R05 isolate independently addressable units, retain mixer indices only when input widths are known, and apply shared deadlines to control requests, acquisition and recovery. An uncertain power hand-back fences further operations that depend on it.
- R06–R11 retain explicit route edits, conservative cache invalidation, standard protocol encodings and the vendor release gates. Firmware persistence and device-specific routing require hardware evidence.
- O01–O06 preserve physical ownership during a transition. A reservation fences competing control changes while volume remains available during drain. Revalidate the owner, route and stream before retirement. Keep pre-track volume and decoder reseek responsibilities explicit. A different attached device at a reused host key gets a fresh attachment ID.

## Player-facing conversion

- D01–D03 document headroom, dither saturation, scaling and measured filter behavior. Exact padding stays verified only when the declared source data passes validation. Preserve the supported conversion limits and report the selected processing.
- D04 conserves whole tail frames under backpressure. D05 distinguishes DoP transport confirmation from permission to unpack explicit DoP into PCM. D06 follows the file's DSF bit-order field and keeps legacy native admission bounded to proven wire formats.
- A01–A03 keep automatic PCM conversion and widening defaults and report their effect. Android PCM backends can accept converted native DSD and consented DoP while retaining source counters. Downstream optical limits follow a provable bound route. The SDK retains conservative protection when route identity or continuity is unknown.
- A04–A06 use cached Media3 format capabilities, an atomic output installation, one retained-owner transition, and typed failures in the renderer's error paths. A07 and X02–X03 align guides, diagnostics and evidence with the implemented contracts.

## Publication and evidence

P01–P04 distinguish the historical source-gated release from publication of immutable gate-staged bytes. Keep the AAR, Maven metadata, sources, native archive and crate digest-bound through staging and publication. Recovery verifies the original reservation and gate receipt, reuses only the immutable payload of a proven attempt, and rejects conflicting registry objects. Re-gating expired evidence is an explicit operation on the reserved source.

Each implementation batch uses the existing Rust and Kotlin tests, actual host JNI where the entry point matters, and static workflow checks for release changes. Physical DAC behavior, audible transients and firmware quirks belong in the dedicated hardware ledger.
