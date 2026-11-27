# The three drivers studied

usbdac was designed by reading three open-source Android USB DAC drivers side by side: Flick, Mornye and BitChord. All three send audio to a USB Audio Class DAC with isochronous transfers over usbfs and libusb, all three control the DAC's own volume, and all three have been used on real DACs. They also disagree in many details, and each disagreement is a design decision usbdac had to make. The [design decisions](decisions.md) are written down one by one; this page says who the three are, how they were studied, and what usbdac did and did not take from them.

## Who they are

| | Flick | Mornye | BitChord |
|---|---|---|---|
| Repository | [moss-apps/Flick](https://github.com/moss-apps/Flick) | [mornyeapp/Mornye](https://github.com/mornyeapp/Mornye) | [kushagrasinghx/BitChord](https://github.com/kushagrasinghx/BitChord) |
| Licence of the repository | MIT | LGPL-3.0 | GPL-3.0 (the USB transport files carry an LGPL-3.0-only header) |
| App | Android music player, Flutter UI | Music player, Flutter UI | Music player for Android phones and TV, Kotlin |
| USB code | Rust, one large module: `rust/src/uac2/` (`android_direct.rs` is 8,848 lines, with descriptor parsing under `descriptors/`) | Rust: `rust_backend/crates/mobile/src/usb_audio/` (`descriptors.rs`, `framing.rs`, `resample.rs`, `transport.rs`, `volume.rs`, about 2,550 lines), with the player-side logic in Kotlin (`UsbDirectPlayback.kt`) | C++ transport (`app/src/main/cpp/bitperfect/usb_transport.cpp`, `usb_stream.h`) and Kotlin logic (`.../bitperfect/usb/UsbDescriptors.kt`, `UsbVolume.kt`, `UsbDirectTransport.kt`) |
| USB library | libusb through the Rust `rusb` and `libusb1-sys` crates | libusb through `libusb1-sys` | libusb 1.0.27, built from a pinned release |
| Other outputs | Oboe/AAudio exclusive for players with an internal DAC (`AGENTS.md`) | Android 14 preferred mixer path, and an AAudio exclusive path through Oboe (`usb_audio/README.md`) | Android bit-perfect mixer and AAudio exclusive (`BitPerfectTransport.kt` lists the three drivers) |
| Built as | An app | An app | An app |

None of the three is a library. Each driver is written for one player's engine, and its settings, its volume memory and its fallback rules live in that player.

### Flick

Flick is the origin of usbdac. The direct USB driver in `rust/src/uac2/` (descriptor parsers, clock handling, isochronous packet scheduling and validation, loss accounting, output position accounting, payload audits and hardware volume controls) was extracted from Flick's commit `86ffa87f` and given a public API. Flick is MIT licensed, and the repository's [`NOTICE`](https://github.com/johnneerdael/android-usbdac/blob/main/NOTICE) records the origin. usbdac has since changed a great deal; [what usbdac improves](improvements.md) lists it.

"Flick today" in the decision pages means `moss-apps/Flick` branch `versionF` at commit `11395a49` (2026-11-26), which includes pull requests #400, #406 and #414. "Flick before #400" means `main` at `90a539c2`. Where a Flick pull request is still open (#415 to #418), the pages say so.

### Mornye

Mornye's driver lives in one small Rust module and is documented in `usb_audio/README.md`, which also lists its verification and says plainly what was not tested on hardware. Its commit history is a record of reasoning: several commits describe a problem, a change, and in two cases a revert with the reason. The decision pages quote those messages where they bear on a decision. Where the README and the code disagree, the pages follow the code and say so (the README says the output queue holds about 200 ms; the code holds 1.5 s, see [the ring decision](decisions-stream.md#ring-depth)).

Studied at commit `af9a4d642` (2026-11-24).

### BitChord

BitChord's direct USB driver is a port of Mornye's. The commit that added it (`06628ed1`, 2026-10-28) says "Phase 3 of porting Mornye's bit-perfect output paths: its Rust USB Audio Class driver, ported to C++ (transport) and Kotlin (logic)", and the header of `usb_transport.cpp` and `usb_stream.h` says "Adapted from Mornye". Because it is a port made at one moment, BitChord shows what Mornye's driver was on 2026-10-28, and BitChord's later changes show what its users hit. A public issue on BitChord, [#1485](https://github.com/kushagrasinghx/BitChord/issues/1485), is one of the data points used for the transfer-queue decision.

Studied at commit `21356206` (2026-11-21).

## How they were studied

1. **Code.** Each driver's transport, descriptor parsing, clock handling, volume code and tests were read. The decision pages cite file paths, and function names where line numbers would drift.
2. **History.** Commit messages and pull requests were read for the reasoning behind a change. This is where most of the "why" comes from, and it is quoted or summarised, with the commit or pull request named.
3. **Hardware.** The maintainer of usbdac used Flick and BitChord on the same phone and DAC (a OnePlus 15 and an iBasso DC04U); the Flick pull requests #414 to #418 say so. Geometry sweeps and fault-injection runs were made on that pair with test-only Flick builds and are recorded in Flick pull requests #400 and #406. No hardware comparison of Mornye is recorded; its behaviour here comes from its source and history.
4. **Public sources.** The USB and Android documents, and the Linux `snd-usb-audio` source, that back each claim are listed on [the references page](references.md).

Every claim that rests on a hardware run says so and names the device. A claim that rests on reading code says that. Where the only evidence is a commit message that states the mechanism was "read from the code, not measured", this documentation repeats that.

## What usbdac took, and what it did not

- **From Flick:** the code, under the MIT licence, as recorded in `NOTICE`. This is the only project whose code is in usbdac.
- **From Mornye and BitChord:** nothing in code. Their behaviour was studied, and several usbdac decisions were made after seeing what they do and why. Mornye is LGPL-3.0 and BitChord GPL-3.0; usbdac is MIT. `NOTICE` says: "No code from any other project was copied into this repository." The test suites, the data structures and the names are usbdac's own.
- **Not a fork.** usbdac is a library with a public API, a Kotlin SDK, three output paths and an output selector. It started from Flick's driver and has been substantially reworked; it is not Flick's library and is not maintained as a branch of Flick.

## Revisions studied

| Project | Revision | Date | Used for |
|---|---|---|---|
| Flick, `versionF` | `11395a49` | 2026-11-26 | "Flick today" |
| Flick, `main` before pull request #400 | `90a539c2` | 2026-11-25 | "Flick before #400" (32 packets x 8 transfers) |
| Flick, `feat/usb-dac-unified` | `86ffa87f` | 2026-11-26 | The extraction point named in `NOTICE` |
| Mornye | `af9a4d642` | 2026-11-24 | Source and commit history |
| BitChord | `21356206` | 2026-11-21 | Source, the port commit `06628ed1`, issue #1485 |
| usbdac, `main` | `353e039` (`v0.1.5`) | 2026-11-27 | What usbdac does. The decision pages were first written at `b309a08` (`v0.1.0-alpha.6`, 2026-11-26) |

These are all young codebases that change quickly. A statement here is true of the revision named; check the current source before relying on it. The statements about Flick, Mornye and BitChord were not read again for usbdac 0.1.5. They describe the revisions in this table, and later changes to those projects are not reflected.
