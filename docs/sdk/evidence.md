# Evidence and status

This page collects the data points that the [design decisions](decisions.md) rest on, says where each came from, and lists plainly what has been verified and what has not. Read it before relying on usbdac in an app.

## Hardware status {#hardware-status}

usbdac has been run on **one phone and one DAC**: a OnePlus 15 (CPH2747, Android 16) with an iBasso DC04U. The Rust driver played PCM on that pair inside a build of Flick that runs on this library. Everything below the first row is covered only by tests that run without a DAC, or has not been run at all.

| Area | Status | Source |
|---|---|---|
| PCM playback through the Rust driver, 24-bit at 44.1 kHz | Played on the OnePlus 15 and DC04U in a Flick build | [index](index.md#status), Flick PR #400 |
| 16 packets x 8 transfers (the default at 125 us) | The geometry sweep and benchmark runs played cleanly at 44.1 kHz, judged from the app log. The listening checks (cold start, five minutes without a dropout, 48 and 96 kHz, pause, seek, unplug and replug, 192 to 384 kHz at up to 32-bit) were made on the earlier 8 x 16 build and "have not been repeated" on 16 x 8 | Flick PR #400 |
| Loss tolerance and exact loss accounting | Fault injection on the OnePlus 15 and DC04U with a test-only build, on 8 x 16. Not repeated on 16 x 8. What loss sounds like was not judged | Flick PR #406 |
| Full-speed (1 ms interval) DACs | Not tested | Flick PR #400 |
| DSD and DoP, native and over PCM | Host tests only. Not run on a DAC | [decision 5](decisions-stream.md#dsd) |
| Clock selectors and multipliers, range check | Host tests on synthetic descriptors only | [decision 6](decisions-clock.md#clock-path) |
| Implicit-feedback refusal | Host tests only. No such DAC tried | [decision 8](decisions-clock.md#implicit-feedback) |
| Hardware volume, start-up attenuation, hand-back, volume keys, system panel, idle release | Host tests only | [decisions 9 to 12](decisions-volume.md) |
| The Kotlin SDK and the sample app | Not run on a phone. CI builds them for four ABIs | [index](index.md#status) |
| Android-managed output (Android 14 bit-perfect mixer request) | Host tests only | [transports](transports.md) |
| AAudio exclusive output on a player with an internal DAC | Host tests only. No digital audio player tried | [automatic output](automatic-output.md#what-is-not-verified) |
| `OutputSelector` | Host tests against a fake device. Nothing on hardware | [automatic output](automatic-output.md#what-is-not-verified) |
| Other phones and DACs | None tried | |

**Reporting a problem.** Set the native log level (`UsbDac.setLogLevel`) and send the logcat of the tag `usbdac`, with the Android version, the phone, the DAC, and the source format. [Troubleshooting](troubleshooting.md) has the error codes.

## The geometry sweep {#the-geometry-sweep}

From Flick pull request #400. A test-only Flick build (not part of that pull request or of usbdac) read the transfer geometry from a system property so each shape could be run from adb without a rebuild. OnePlus 15 (CPH2747, Android 16) and iBasso DC04U, 24-bit at 44.1 kHz, 10 to 20 seconds per run, and the shapes marked "x2" or "x3" were repeated with the same result. Shapes are written packets per transfer x transfers in flight.

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

Counted for usbdac `main` at `b309a08` (`v0.1.0-alpha.6`):

| Suite | Count | What it covers |
|---|---|---|
| `usbdac-core` unit tests | 396 | Descriptor parsing, clock resolution, packet scheduling and validation, loss accounting, ring and ring targets, position, flush cancellation, volume curves, start-up attenuation, bit-perfect blockers, conversion, and the transport against a fake DAC (`transport/fake_dac.rs`) |
| `usbdac-core` integration tests | 17 | The public API surface, the conversion API, and a test that conversion does not allocate in its loop |
| `usbdac-jni` unit tests | 72 | The JNI and JSON layer, the AAudio engine logic and ring |
| Rust doctests | 2 | The README example and the crate example |
| **Rust total** | **487** | `cargo test --workspace`, run on a Mac for this page: 487 passed, 0 failed |
| Kotlin unit tests | 237 | Counted by their `@Test` annotations in `android/usbdac/src/test`: the output selector (58), AAudio output (33), model parsing (26), conversion (38), volume (24), controls (17), events, routes, interfaces. Run in CI with `./gradlew :usbdac:testDebugUnitTest`; not run for this page |

None of these runs touches a DAC. They test the logic: what is decided, in what order, with what numbers. They cannot say that a DAC will play.

To reproduce: `cargo test --workspace` (host), `cargo test -p usbdac-core quality -- --nocapture` (the resampler's measured quality), `cargo run --release -p usbdac-core --example convert_bench` (its cost), and `python3 -I .github/scripts/check_docs_snippets.py` (that the guide's code is the sample's code).

## Numbers computed for these pages {#computed-numbers}

Where a figure is arithmetic and not a measurement, it is worked out here so that it can be checked.

| Figure | Arithmetic |
|---|---|
| Feedback representability at 44.1 kHz, 16.16 | `round(44.1 x 65536) / 65536 - 44.1 = 6.1e-6` samples per microframe, times 8000 microframes a second = **0.049 Hz**. Half of the last bit is 0.5 x 8000 / 65536 = 0.061 Hz |
| The same in 10.14 | `round(44.1 x 16384) / 16384 - 44.1 = -2.4e-5`, times 8000 = **0.195 Hz**. Half of the last bit is 0.244 Hz |
| Flick's two feedback tests | They assert `abs(rate - 352800) < 0.01`, which 0.049 Hz and 0.195 Hz exceed; the pull requests #414 and #415 report the same two tests failing on `versionF` when compiled for a host |
| Ring memory | 500 ms of 384 kHz stereo at 4 bytes a sample (`Vec<i32>`) = 0.5 x 384000 x 2 x 4 = 1.54 MB |
| 16 ms of drift cushion | At 200 ppm, 16 ms / 0.0002 = 80 s. Hundreds of ppm is the figure the scheduler's comment uses for USB clock tolerance. An illustration, not a measurement |
| Queue of the kernel driver | 128 ms (Flick at a 1 ms interval) / 18 ms (`MAX_QUEUE`) = about 7 |
| Resampler | Quality, passband ripple and cost are measured by the repository's own tests and example and are in [the native API document](native-api.html#14-conversion-helpers) |
