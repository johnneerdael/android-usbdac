# Recorded capabilities and evidence

Generated from `docs/evidence/results.json`; edit the ledger and run `python3 -I .github/scripts/generate_evidence.py`.

Software delivery may proceed with physical acceptance waived; pending rows must never be rendered as physical passes.

## Implementation boundaries

Shipped boundaries: UAC versions [1, 2, 3]; playback True; PCM capture True; duplex True; simultaneous open devices no process-wide limit; one owner per attachment; Type I formats signed_pcm, unsigned_pcm8, float32; vendor protocols False. USB host tests and virtual guests do not establish physical or audible behavior. Source-backed compatibility candidates are distinct from hardware-verified device rules.

| Feature | Device / host | Evidence tier | Result | Revision and limits |
|---|---|---|---|---|
| PCM 16/24/32 and explicit feedback | Hidizs AP80 Pro-X; OnePlus 15 CPH2747 Android 17 | physical | historical-pass | 28dbb29: Historical parity AAR trace; source records zero loss. Original log hash not available in this ledger. |
| Native DSD64/128/256 transitions | Hidizs AP80 Pro-X; OnePlus 15 CPH2747 Android 17 | physical | historical-pass | 28dbb29: Balanced test payloads, not music. No UAC3 or other native DSD device claim. |
| 44.1 kHz PCM and explicit feedback | iBasso DC04U; Android host not named in SDK record | physical | historical-pass | 97e614b: 297672 delivered frames; phone identity and volume readback not recorded. |
| UAC1 full-speed synchronous PCM and hand-back | QEMU USB Audio; Alpine Linux arm64 guest | virtual | passed | ff6d616: 95808 played frames; zero loss/underrun; alt-zero and original raw volume restored. Not UAC2/3 or audible evidence. |
| Full-speed UAC1 PCM, alt-zero and volume hand-back | QEMU USB Audio (null audio backend); QEMU virtual USB host | virtual | passed | 5bec145aeeec2b1ff1836096f938b4ad09cb6175: 48 kHz stereo signed16; 95520 played frames, zero recorded loss/underruns; final alt-zero and original raw volume [0, 0] restored. Initial attenuation mismatch is retained in the log; subsequent stream-start readback verified -40.53515625 dB. Null backend; no audible, physical, UAC2/UAC3 or Android claim. |
| Retained USB handle reconfiguration and rollback | QEMU USB Audio (null audio backend); QEMU virtual USB host | virtual | passed | 5bec145aeeec2b1ff1836096f938b4ad09cb6175: RECONFIGURE_SAME_OWNER_PASS: same 48 kHz owner retained, paused state and volume unchanged. RECONFIGURE_ROLLBACK_PASS: unsupported format rolled back and resumed 48 kHz. Virtual UAC1 single-device evidence only. |
| Two simultaneous USB owners and independent close | QEMU USB Audio (null audio backend); QEMU virtual USB host | virtual | passed | 5bec145aeeec2b1ff1836096f938b4ad09cb6175: TWO_DACS_PASS: first owner 95904 frames, second owner 144480 frames; independent volume, alt-zero, retirement and duplicate protection. TWO_PROBE_EXIT=0. Two virtual QEMU USB Audio devices; no two-physical-DAC or Android claim. |
| Volume persistence across track change | Portable DAC; Android 17; OEM unspecified | physical | pending | pending: Mornye #864; reported elsewhere, no usbdac physical acceptance claimed. |
| Hardware volume keys | XMOS DAC; Samsung Android 17 | physical | pending | pending: BitChord #1990; reported elsewhere, no usbdac physical acceptance claimed. |
| Long playback/feedback | UGREEN CM720; Honor Magic 8 Pro Android 17 | physical | pending | pending: Flick #393; reported elsewhere, no usbdac physical acceptance claimed. |
| Startup silence and long playback | TempoTec Sonata BHD; vivo iQOO Neo 10 OriginOS 7 SDK 37 | physical | pending | pending: Mornye #975 and #904; reported elsewhere, no usbdac physical acceptance claimed. |
| 30 minute burst-loss and background playback | USB-C DAC; Poco F9 Ultra Android 16 Snapdragon | physical | pending | pending: BitChord #1485; reported elsewhere, no usbdac physical acceptance claimed. |
| Clock compatibility with strict and fallback policy | CX31993 + MAX97220; MediaTek and Snapdragon hosts | physical | pending | pending: Flick #157; reported elsewhere, no usbdac physical acceptance claimed. |
| 30 minute nominal async drift | Moondrop CDSP; Two distinct Android hosts | physical | pending | pending: BitChord #1251; reported elsewhere, no usbdac physical acceptance claimed. |
| DoP | Required target device; Required matching host | physical | pending | pending: Host software coverage does not satisfy this physical prerequisite; waived for current delivery, remains unverified. |
| UAC2/UAC3 implicit feedback | Required target device; Required matching host | physical | pending | pending: Host software coverage does not satisfy this physical prerequisite; waived for current delivery, remains unverified. |
| UAC3/BADD playback capture duplex | Required target device; Required matching host | physical | pending | pending: Host software coverage does not satisfy this physical prerequisite; waived for current delivery, remains unverified. |
| Clock selectors and multipliers | Required target device; Required matching host | physical | pending | pending: Host software coverage does not satisfy this physical prerequisite; waived for current delivery, remains unverified. |
| Startup attenuation hand-back mute | Required target device; Required matching host | physical | pending | pending: Host software coverage does not satisfy this physical prerequisite; waived for current delivery, remains unverified. |
| Android managed/AAudio/OutputSelector/OutputHandoff | Required target device; Required matching host | physical | pending | pending: Host software coverage does not satisfy this physical prerequisite; waived for current delivery, remains unverified. |
| Start stop alt-zero and rate-switch click/latency on three chipsets | Required target device; Required matching host | physical | pending | pending: Host software coverage does not satisfy this physical prerequisite; waived for current delivery, remains unverified. |

Historical passes preserve source records with incomplete metadata; they are not a fresh acceptance of the current tree. Pending OEM rows are prerequisites, not failed tests. Per-device bcdDevice, Android build, OEM profile, exact revision and log SHA-256 must be recorded for new hardware acceptance.

## Validation source inventory

These are annotations in current sources, not counts of passing tests. CI and archived logs establish run results independently.

| Source | Test annotations |
|---|---|
| Rust core | 887 |
| Rust JNI | 102 |
| Kotlin SDK | 290 |
| Media3 library | 20 |
