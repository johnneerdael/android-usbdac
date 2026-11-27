# Evidence and status

This page collects the data points that the [design decisions](decisions.md) rest on, says where each came from, and lists plainly what has been verified and what has not. Read it before relying on usbdac in an app.

The [generated status ledger](status.md) is checked against the compiled shipped-support manifest and preserves historical, virtual, and pending OEM results separately. Physical acceptance is waived for the current software-delivery task; pending hardware rows remain unverified.

## Hardware status {#hardware-status}

Four DACs have streamed audio through usbdac, all on a OnePlus 15 (CPH2747, Android 17): the iBasso DC04U, the Hidizs AP80 Pro-X, an SMSL USB AUDIO and a Schiit Unison Modius E. The first two also played in earlier builds. Direct USB PCM was played with the DC04U in Flick and with the AP80 Pro-X in a development build of the SDK sample. The parity build then played native DSD64, DSD128 and DSD256 followed by 16-, 24- and 32-bit PCM on the AP80 Pro-X, using universal feedback detection. The DC04U also played 44.1 kHz PCM through the SDK sample on an Android host. On 27 November 2026 the Android validator ran its playback matrix on all four ([validator runs](#validator-2026-11-27)). Nothing else has run on a DAC.

The earlier usbdac runs are recorded for commits `2f290a1` and `28dbb29` (AP80 Pro-X) and `97e614b` (DC04U). None of them contains the two queued feedback polls (`1b695de`, released in 0.1.3) or the packet recovery and transfer-period options of 0.1.5. The validator runs used a 0.1.10 build, described below. No physical result for 0.1.11 itself is recorded here.

The DC04U sample record at `97e614b` reports 297,672 delivered frames at 44.1 kHz, zero underruns and lost frames, and explicit feedback at approximately 44,099.3 Hz. The phone and volume readback were not recorded. Earlier Flick fault injection used the OnePlus 15 and DC04U on 8 x 16 transfer geometry; it was not repeated on 16 x 8, and audibility was not judged.

### Validator runs, 27 November 2026 {#validator-2026-11-27}

The Android [DAC validator](https://github.com/johnneerdael/android-usbdac/blob/main/docs/design/dac-validator.md) ran its exact-format playback matrix on four DACs between 18:22 and 19:17 UTC. All runs used a OnePlus 15 (CPH2747, Android 17, SDK 37) and `QuirkPolicy.SOURCE_BACKED`, with the hardware-volume requirement off and the startup volume kept. The app and driver report version 0.1.10, so the build includes `84197a2`, and with it the Direct feedback default from `e26dd2d`. It was a local build of the validator branch before that branch merged as `7047b6b`, and the exact commit was not recorded.

| DAC | VID:PID, bcdDevice | Passed | PCM | Native DSD | DoP carrier |
|---|---|---|---|---|---|
| iBasso DC04U | `0661:0883`, `0x0001` | 44 of 44 | 16, 24, 32-bit, 44.1 to 768 kHz | DSD64 to DSD512, both rate families (interface 2, alternate 4) | DSD64 to DSD256 |
| SMSL USB AUDIO | `152a:85dd`, `0x0315` | 34 of 34 | 24, 32-bit, 44.1 to 768 kHz | DSD64 to DSD512, both rate families (interface 1, alternate 3) | DSD64 to DSD256 |
| Schiit Unison Modius E | `30be:101a`, `0x0100` | 20 of 20 | 16, 24, 32-bit, 44.1 to 192 kHz | No native alternate admitted | DSD64 |
| Hidizs AP80 Pro-X | `c502:0072`, `0xffff` | 34 of 34 | 16, 24, 32-bit, 32 to 384 kHz | DSD64 to DSD256 | DSD64, DSD128 |

Every leg reported `passed_usb`. Each was bit-perfect, with a verified clock and a payload that matched the written bytes, and each closed cleanly. No leg had an underrun, a lost frame, a short packet or a transfer timeout. All four DACs ran at high speed with explicit feedback: the DC04U, SMSL and Modius E at a 125 µs service interval, the AP80 Pro-X at 250 µs. Some AP80 Pro-X feedback polls returned empty reports, without loss.

What these runs do not show:

- **The analog output.** The evidence is the USB transport as the host sees it. No analog output, acoustic result or DAC display was observed.
- **DoP decoding.** The DoP rows prove the carrier only. Decoder lock was not observed.
- **Native DSD under the default policy.** On the DC04U and the SMSL, native DSD came from the source-backed `DSD_RAW` rule, which the default `VerifiedOnly` policy does not apply.
- **Anything beyond exact playback.** Implicit feedback, capture, the admission probes, converted fallback and other host routes were not exercised.

The ledger keeps one summary per run in `docs/evidence/history/validator-*-20261127.json`. Each is derived from the validator ZIP, records that ZIP's SHA-256, and is itself checked by digest (`validator-dc04u`, `validator-smsl`, `validator-modius-e` and `validator-ap80` in the [status ledger](status.md)). The ZIPs are not committed because they contain debug logs. The rows are `historical-pass`, not `passed`, because the source revision and Android build were not recorded.

The [generated capability and evidence table](status.md#implementation-boundaries) is the current summary. It separates historical physical runs, virtual results and pending hardware validation. The narratives below retain the details of the recorded physical runs, including their limits.

**Reporting a problem.** Set the native log level (`UsbDac.setLogLevel`) and send the logcat of the tag `usbdac`, with the Android version, the phone, the DAC, and the source format. [Troubleshooting](troubleshooting.md) has the error codes.

### AP80 Pro-X feedback units

On a OnePlus 15, the Hidizs AP80 Pro-X (`c502:0072`) opened UAC2 interface 1,
alternate setting 2, at 44.1 kHz with 16-bit stereo PCM. The clock readback
matched. Playback used endpoint `0x01` at a 250 µs service interval. The
four-byte feedback endpoint `0x81` was polled every 1 ms.

The failing capture recorded 62 successful feedback transfers and zero
accepted reports before the deadline. One observed report was
`46 09 0b 00`, or about 11.0362 frames in 16.16 format. Per high-speed USB
microframe that means about 88,289.8 Hz. Per OUT service interval it means
about 44,144.9 Hz. The latter fits both the confirmed clock's rate window and
the OUT packet capacity. The captured bytes are a regression vector for
[startup timebase detection](decisions-clock.md#feedback).

With the detection change, the SDK sample inferred OUT service interval
units after three matching completions on the same phone and DAC. It
reported verified direct playback at 44.1 kHz / 16 bit and reached 2,133,448
played frames (48.4 s) with zero underruns and lost frames. The sample
exported the opted-in report after stopping the stream.

The Kotlin SDK later repeated the run on the parity AAR with main
integrated. DSD64 learned the timebase, DSD128 and DSD256 reused it after
fresh validation, and each PCM alternate learned its own timing. All six
modes reported bit-perfect USB transport with zero underruns and lost
frames. The final trace remained available after session close.
[Parity AAR acceptance](https://github.com/johnneerdael/android-usbdac/blob/main/docs/design/alsa-parity.md#universal-detection-on-the-parity-build)

### AP80 Pro-X native DSD transition {#ap80-native-dsd-transition}

The AP80 Pro-X advertises UAC2 Type I RAW_DATA on configuration 1, interface
1, alternate setting 4. Its documented native layout uses four-byte,
big-endian words, with no bit reversal. The driver preserves the full USB
format bitmap and recognizes this layout for the measured device and
endpoint. Native requests require a DSD alternate. [Hidizs specifications](https://www.hidizs.net/pages/ap80pro-x-specifications)

The SDK ran these legs in order on the OnePlus 15 in one DAC session. Each
leg verified its clock and feedback, reached the driver's bit-perfect
verdict, and finished with zero lost frames and underruns.

| Mode | Alternate setting | Wire rate | Feedback units |
|---|---|---|---|
| DSD64 | 4 | 88,200 Hz | Inferred from live feedback |
| DSD128 | 4 | 176,400 Hz | Reused and validated |
| DSD256 | 4 | 352,800 Hz | Reused and validated |
| 16-bit PCM | 2 | 44,100 Hz | Inferred for the PCM alternate |

The later parity AAR acceptance also covered 24- and 32-bit PCM. The linked
[parity ledger](https://github.com/johnneerdael/android-usbdac/blob/main/docs/design/alsa-parity.md#universal-detection-on-the-parity-build)
records that sequence separately.

The probe used balanced DSD data and low-level PCM. This validates USB
delivery, clock changes and the transition on this device.

## Release validation scope {#release-validation-scope}

For v0.1.11, Release run [46059018520](https://github.com/johnneerdael/android-usbdac/actions/runs/46059018520) ran the five release gates on source `66f6fc1` with workflow `7047b6b`: Rust tests and Android check; guide code samples; AAR, unit tests and sample; defensive USB parsers (bounded fuzz and Miri); and QEMU UAC1 playback, reconfigure and two owners. The QEMU gate's log records `QEMU_RATE_POLICY_PASS` (an exact 44.1 kHz request refused, and a `NearestSupported` 44.1 kHz request opened at 48 kHz on the 48 kHz-only virtual device), `QEMU_PCM_PASS` with 998 completed transfers and zero underruns and lost frames, `RECONFIGURE_SAME_OWNER_PASS`, `RECONFIGURE_ROLLBACK_PASS` and `TWO_DACS_PASS`. That log is a CI artifact (`qemu-uac1-evidence`) and is not copied into `docs/evidence/history`. The same run built the Android validator (signed with the CI debug key) and the Apple Silicon validator (ad hoc signed, not notarized) from the same source, sealed the gate receipt, and published and verified the staged crate and Maven files. Its GitHub Release step then failed with HTTP 422. The release page, published at 19:57 UTC, carries the staged assets, both validators and a `receipt.json` naming run 46059018520 and source `66f6fc1`; the workflow runs do not record how that page was created. No gate used a physical device, and shipping the validators is not a hardware result.

For v0.1.10, the five required release gates ran on the reserved release source. Packaging failed before publication; recovery checked the original evidence and rebuilt the same tagged source. That is source-level assurance, not proof that the rebuilt binaries were the exact gate-built bytes. The bounded v0.1.10 defensive gate and the earlier three-hour v0.1.7 fuzz run are separate software evidence. Neither establishes physical-device behavior.

The shared USB Audio implementation includes: UAC1, UAC2 and UAC3 (BADD and full ADC3) playback, PCM capture and duplex, channel layouts, clock selection and restoration, standard routing, explicit and implicit feedback, and native DSD and DoP capability handling. The
[implementation ledger](https://github.com/johnneerdael/android-usbdac/blob/main/docs/design/alsa-parity.md) records software checks
and their artifacts by implementation batch. These include Rust protocol,
control and packet tests, Kotlin tests, Android target checks and four-ABI
AAR and sample builds. Those results apply to their recorded
revisions. [The tests section](#tests) preserves historical counts; the generated ledger tracks current source inventory.

For current shipped boundaries and recorded verification, use the [generated status ledger](status.md#implementation-boundaries). The [source-backed device compatibility page](source-backed-quirks.md) describes the opt-in quirk table and its limits; the [device compatibility candidates](https://github.com/johnneerdael/android-usbdac/blob/main/docs/design/device-compatibility-candidates.md) record possible future work.

QEMU 10.1.3 exposed a full-speed UAC1 interface on emulated xHCI with a
null audio backend. In an Alpine arm64 Linux guest, a probe built from
`ff6d616` opened usbdac through Linux usbfs and played synchronous PCM
at 48 kHz, stereo, 16 bits. The driver reported 95,808 played frames
and 998 completed isochronous transfers over two seconds, with verified
USB payloads and zero underruns or packet losses. Close reset the
streaming alternate to zero, released both interface claims, and
restored volume. The virtual device rounded the startup attenuation
request. Stream opening rediscovered the control at the lower level.
The [implementation ledger](https://github.com/johnneerdael/android-usbdac/blob/main/docs/design/alsa-parity.md)
records the guest setup and validation scope. This was a virtual device on a Linux
guest. It says nothing about a phone, a real DAC or any other transfer mode.

The OnePlus 15, AP80 Pro-X and DC04U results above and the Flick results remain the only
hardware evidence. UAC3 and BADD streaming, capture and duplex, standard routing
controls, implicit feedback, DoP and broader device compatibility still need physical
acceptance. Use the
[device compatibility candidates](https://github.com/johnneerdael/android-usbdac/blob/main/docs/design/device-compatibility-candidates.md)
to choose that follow-up work.

## The geometry sweep {#the-geometry-sweep}

From Flick pull request #400. A test-only Flick build (not part of that pull request or of usbdac) read the transfer geometry from a system property so each shape could be run from adb without a rebuild. OnePlus 15 (CPH2747, Android 17) and iBasso DC04U, 24-bit at 44.1 kHz, 10 to 20 seconds per run, and the shapes marked "x2" or "x3" were repeated with the same result. Shapes are written packets per transfer x transfers in flight.

| Packets in flight | Played | Stalled about 1 s after the start (the table's words) |
|---|---|---|
| 255 or fewer | 8 x 8, 8 x 16, 16 x 8, 32 x 4, 4 x 32, 8 x 20, 8 x 24, 16 x 12, 32 x 6, 8 x 28, 16 x 14, 32 x 7, 8 x 30, 8 x 31, 64 x 2, 64 x 3, 127 x 2, 17 x 15 (x2), 51 x 5, 85 x 3 | none |
| exactly 256 | none | 8 x 32, 16 x 16 (x2), 32 x 8 (x2), 64 x 4, 128 x 2 |
| 257 to 511 | 37 x 7 (x2), 26 x 10, 8 x 33, 8 x 34, 8 x 36, 8 x 38, 8 x 40, 73 x 7 | 43 x 6 (x3) |
| 512 | 32 x 16 (x3), 8 x 64 | 64 x 8 (x3), 16 x 32 |
| 768 and 1024 | 32 x 32 (7 underruns) | 32 x 24 (x2) |

How to read it: 255 packets or fewer always played, exactly 256 never did, and above 256 the result varies with the shape in a way that repeats. The moment of the stall differs by shape (the pull request computes 223 ms for 8 x 32, 478 ms for 16 x 16, 988 ms for 32 x 8, 2008 ms for 64 x 4 and 4048 ms for 128 x 2, and the logs put the stuck transfer's submission 12 to 30 ms before each); with the one-second transfer timeout, the failure is reported about a second after that, which matches the log of the first run (start at 15:05:55.227, timeout at 15:05:57.188). Every working shape was clean (no lost frame, timeout or underrun) except 32 x 32.

**Cost of the shapes that played**, CPU time of the USB output thread over a 10 s window; "load" adds 16 busy loops from adb:

| Shape | Audio in flight | CPU, idle | CPU, load | Lowest queue depth |
|---|---|---|---|---|
| 4 x 32 | 16 ms | 110 ms/s | 53 ms/s | 31 of 32 |
| 8 x 8 | 8 ms | 69 ms/s | 29 ms/s | 7 of 8 |
| 8 x 16 | 16 ms | 66 ms/s | 29 ms/s | 15 of 16 |
| 8 x 24 | 24 ms | 67 ms/s | 30 ms/s | 23 of 24 |
| **16 x 8** (usbdac's default) | 16 ms | 46, 48, 44 ms/s | 19, 18, 18 ms/s | 7 of 8 |
| 32 x 4 (Mornye and BitChord's shape) | 16 ms | 33 ms/s | 13 ms/s | 3 of 4 |
| 32 x 16 | 64 ms | 33, 32 ms/s | 12, 12 ms/s | 15 of 16 |

CPU follows packets per transfer and not the queue depth. The pull request lists what was **not measured**: tolerance to a stalled app thread (the adb load never caused an underrun on any shape, even 8 ms in flight, so it does not separate them), battery, and any rate other than 44.1 kHz.

**What the geometry rests on besides this.** The Linux `snd-usb-audio` driver limits itself to 12 URBs and 18 ms (`MAX_URBS`, `MAX_QUEUE` in `sound/usb/card.h`). BitChord issue #1485 reports that a different phone (Poco F9 Ultra) stalls at 32 x 32 and runs 32 x 16, where the OnePlus 15 runs both and stalls at 256. Both phones run 128 packets in flight. The limit at which a phone stalls is therefore specific to the phone, which is why the default stays well under the one that was measured, and the numbers are configurable.

## The fault-injection runs {#the-fault-injection-runs}

From Flick pull request #406. A healthy DAC produces no lossy transfer, so a test-only build either shortened the timeout of every Nth transfer, so libusb really cancels it in the kernel, or rewrote the result of a completed one. OnePlus 15 and DC04U, 8 packets x 16 transfers (the geometry of #400 at the time), one fault per second unless noted.

| Injected fault | Result |
|---|---|
| One packet one frame short | Playback continues; 1 frame counted lost per hit; the stream-stable flag drops after the first hit |
| One zero-length packet | Playback continues; one packet counted lost per hit |
| A transfer reported timed out, back half undelivered | Playback continues; half a transfer counted lost per hit |
| The same, 4 transfers in a row | The stream stops on the 4th: "4 consecutive isochronous transfers lost audio or timed out (limit 3)" |
| The same, 2 transfers every 20 | The stream stops at the total bound: "4429 audio frames lost since stream start exceeds the limit of 4410" (44.1 kHz) |
| One packet with `LIBUSB_TRANSFER_ERROR`, `actual=0` | The stream stops on the first hit (the case #406 lists as still open) |
| A real 4 ms transfer timeout | Playback continues; the kernel cancels that one transfer before it reaches the wire, all 8 packets `status=error actual=0`; neighbouring transfers are unaffected |
| A real 15 ms transfer timeout | Playback continues; the transfer is cancelled on the wire with 1 to 3 packets delivered; the count is exact (3 packets, 102 bytes, 17 frames delivered of a 44-frame transfer, 27 reported lost) |

Not confirmed on hardware, per the pull request: the lost-frame count as shown in the diagnostics, the bit-perfect indicator turning off (it was already off), and what the loss sounds like. The bounds (3 consecutive, 100 ms in all) are the author's choice and are not derived from a measurement.

## Reports from the other projects {#other-projects}

These are not usbdac's measurements. Each is a record in another project's repository, with the limits its author put on it.

| Source | What it records | The author's own caveat |
|---|---|---|
| Mornye `09ff115ef` (2026-11-19), reverted by `580a9bbfe` the same day | Up to 24 transfers (about 96 ms) in flight against stutters when the worker thread is late; reverted because deeper audio in the kernel delays feedback, and a DAC that steers its own buffer swung past the 2 % band, leading to "USB feedback stopped or invalid" pauses reported by a tester | "Not run on a DAC: the cause is read from the code, not measured"; "the mechanism is read from the code, not measured" |
| Mornye `30ec85296` (2026-11-18) | The queue held 200 ms or 256 KiB, "1.5 s for a CD-rate song but a third of a second at 96 kHz in 32-bit", cutting high-rate songs on a stall; changed to 1.5 s at every rate | None stated |
| Mornye `b98f81046` (2026-11-11) | XMOS-based DACs such as the FiiO BTR15 point the terminal at a clock selector; the 4-byte rate request went to the selector and failed with "Short USB control response" | None stated |
| Mornye `da608a663` (2026-11-18) | A DAC that takes a rate request and then plays silence; the clock's range is now read and a rate it does not list is never set | None stated |
| Mornye `7447af0ee` (2026-11-19) | A direct USB DAC held through every pause left other apps without sound until it was unplugged; released after 5 s idle with the level handed back | "Not run on a DAC" |
| BitChord `06628ed1` (2026-10-28) | The port of Mornye's driver, with a pause that keeps the queue | |
| BitChord `ea2df7e4` (2026-10-28) | The -40 dB start-up level carried over into Android's own driver because it was never restored | |
| BitChord issue #1485 | Poco F9 Ultra, Snapdragon, Android 16, a USB-C dongle DAC, v1.9.1: about 990 failed transfers in 7 minutes with 4 transfers queued, each ending the stream; 91 with 16 transfers; 32 transfers made things worse (all timed out together). A single packet fails with `LIBUSB_TRANSFER_ERROR` and `actual=0`, about 125 us each; the worker loop was not starved (2 gaps over 10 ms in 7 minutes). The DAC was left at the app's attenuation after the fallback | One reporter, one device and a patch; it is a field report and no controlled test was run. "Occasional faint clicks remain during bursts of host-side packet loss: about 13 per minute with 16 transfers, against ~140 per minute before" |
| Flick PRs #414 to #418 | The lag of pause, resume and seek (about 100 ms each); the second libusb context of volume writes; the slider floor; volume keys; idle release | Each says it was "Not run on hardware" |

## Tests {#tests}

Historical run: usbdac `main` at `353e039` (`v0.1.5`), on a Mac. Current source annotations are generated in the [status ledger](status.md#validation-source-inventory); annotation counts are not passing-run evidence:

| Suite | Count | What it covers |
|---|---|---|
| `usbdac-core` unit tests | 805 | Descriptor parsing (UAC1, UAC2, UAC3), clock resolution and selection, packet scheduling and validation, loss accounting, ring and ring targets, position, flush cancellation, explicit and implicit feedback, capture and duplex, standard routing, native DSD, volume curves, start-up attenuation, bit-perfect blockers, conversion, and the transport against a fake DAC (`transport/fake_dac.rs`) |
| `usbdac-core` integration tests | 17 | The public API surface (10), the conversion API (5), and a test that conversion does not allocate in its loop (2) |
| `usbdac-jni` unit tests | 94 | The JNI and JSON layer, the routing bridges, the AAudio engine logic and ring |
| Rust doctests | 2 | The README example and the crate example |
| **Rust total** | **918** | `cargo test --workspace --locked`: 918 passed, 0 failed |
| Kotlin unit tests | 276 | `./gradlew :usbdac:testDebugUnitTest -Pusbdac.skipNative=true`, run without the build cache: 276 passed, 0 failed, the same as the number of `@Test` annotations in `android/usbdac/src/test`. The output selector (57), model parsing (37), AAudio output (33), converting output (18), volume (14), interface claims (14), conversion plans (12), errors, events, the volume write queue (10 each), the output handoff (9), controls (9) and the rest |

The Kotlin count was taken with the native build skipped, because the unit tests do not load `libusbdac.so`. The sample and the AAR build were not rerun for this page.

None of these runs touches a DAC. They test the logic: what is decided, in what order, with what numbers. They cannot say that a DAC will play. The earlier count, at `b309a08` (`v0.1.0-alpha.6`), was 487 Rust tests and 237 Kotlin tests.

To reproduce: `cargo test --workspace --locked` (host), `cargo test -p usbdac-core quality -- --nocapture` (the resampler's measured quality), `cargo run --release -p usbdac-core --example convert_bench` (its cost), and `python3 -I .github/scripts/check_docs_snippets.py` (that the guide's code is the sample's code).

## Numbers computed for these pages {#computed-numbers}

Where a figure is arithmetic and not a measurement, it is worked out here so that it can be checked.

| Figure | Arithmetic |
|---|---|
| Feedback representability at 352.8 kHz, 16.16 | `round(44.1 x 65536) / 65536 - 44.1 = 6.1e-6` frames per microframe, times 8000 microframes a second = **0.049 Hz**. Half of the last bit is 0.5 x 8000 / 65536 = 0.061 Hz |
| The same in 10.14 | `round(44.1 x 16384) / 16384 - 44.1 = -2.4e-5`, times 8000 = **0.195 Hz**. Half of the last bit is 0.244 Hz |
| Flick's two feedback tests | They assert `abs(rate - 352800) < 0.01`, which 0.049 Hz and 0.195 Hz exceed; the pull requests #414 and #415 report the same two tests failing on `versionF` when compiled for a host |
| Ring memory | 500 ms of 384 kHz stereo at 4 bytes a sample (`Vec<i32>`) = 0.5 x 384000 x 2 x 4 = 1.54 MB |
| 16 ms of drift cushion | At 200 ppm, 16 ms / 0.0002 = 80 s. Hundreds of ppm is the figure the scheduler's comment uses for USB clock tolerance. An illustration, not a measurement |
| Queue of the kernel driver | 128 ms (Flick at a 1 ms interval) / 18 ms (`MAX_QUEUE`) = about 7 |
| Resampler | Quality, passband ripple and cost are measured by the repository's own tests and example and are in [the native API document](native-api.html#14-conversion-helpers) |
