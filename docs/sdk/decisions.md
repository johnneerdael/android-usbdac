# Design decisions

usbdac is the result of 18 decisions. Each was made after reading how Flick, Mornye and BitChord, three open-source Android players with their own USB DAC drivers, handle the same problem, and after looking at what the USB and Android documents and the Linux kernel driver say. This section writes each decision down: the question, what each of the three drivers does (with file paths), what usbdac does, the evidence, the trade-off, and the public references.

Two things to know before reading them.

- **usbdac copies no code from Mornye or BitChord.** Their behaviour was studied. usbdac began as Flick's driver, which is MIT licensed and credited in [`NOTICE`](https://github.com/johnneerdael/android-usbdac/blob/main/NOTICE), and has been substantially reworked. [The drivers page](drivers.md) says who the three are, which revisions were read and how.
- **The evidence has several sources.** Direct USB PCM has played on a OnePlus 15 with the iBasso DC04U in Flick and the AP80 Pro-X in the SDK sample. Other evidence comes from host tests and public commits or issue reports. [Evidence and status](evidence.md)

## The decisions

The status column says what has been confirmed about the usbdac behaviour: **Hardware** (run on the one phone and DAC, in a Flick build), **Tests** (host tests against a fake DAC or synthetic descriptors only), or a mix.

| # | The question | What usbdac does | Status |
|---|---|---|---|
| 1 | [How the bus queue is shaped](decisions-stream.md#queue-geometry) | 16 ms at every interval and encoding, 8 transfers, never 256 packets in flight | Hardware for PCM at 125 us (16 x 8). Tests for every other case |
| 2 | [How deep the ring is](decisions-stream.md#ring-depth) | 250 ms, shrinking to 100 ms while live processing is on; gain applied as each transfer is built | Tests |
| 3 | [What a late or lost transfer does](decisions-stream.md#loss-handling) | Count it exactly, carry on, bound it, clear the bit-perfect flag for good | Hardware (fault injection in Flick, PCM). Tests in usbdac. A per-packet error still ends the stream |
| 4 | [What pause, flush and position mean](decisions-stream.md#pause-and-flush) | Pause holds the ring and sends silence; flush cancels what is on the bus; position counts what the DAC took | Tests |
| 5 | [How DSD is carried](decisions-stream.md#dsd) | Standard UAC3 DSD packing; legacy quirks; PCM DoP carrier with library-owned markers | AP80 Pro-X legacy native DSD64 through DSD256 and PCM transition verified. UAC3 DSD and DoP covered by host tests |
| 6 | [Which clock gets the rate](decisions-clock.md#clock-path) | Walk selectors and multipliers to the source, check the range, verify by read-back | Hardware for a plain clock. Tests for selectors |
| 7 | [How the host follows the DAC's clock](decisions-clock.md#feedback) | Newest valid explicit report, packet-capacity check and bounded recovery when reports stop | Tests |
| 8 | [DACs that need implicit feedback](decisions-clock.md#implicit-feedback) | Use a compatible shared-clock source or return a typed refusal | Tests |
| 9 | [Who owns the volume](decisions-volume.md#volume-ownership) | The DAC's own control; software gain exists and ends bit-perfect | Tests |
| 10 | [How a slider writes to the DAC](decisions-volume.md#volume-writes) | Floor at -80 dB, latest write wins, control ioctl on the same descriptor, works without a stream | Tests |
| 11 | [The first sound, and what the DAC is left at](decisions-volume.md#startup-volume) | Lower to at most -40 dB, never raise, hand the original back | Tests |
| 12 | [Volume keys, the system panel, idle release](decisions-volume.md#keys-and-idle) | Optional helpers for each; 30 s idle release | Tests |
| 13 | [When the library may say bit-perfect](decisions-reporting.md#bit-perfect-claim) | Named blockers, including loss of feedback lock on direct USB routes that use it | Tests |
| 14 | [MQA](decisions-reporting.md#mqa) | No MQA code; pass the PCM through untouched | None needed. No MQA DAC tried |
| 15 | [When a DAC cannot play the file as it is](decisions-reporting.md#conversion) | Opt-in conversion, classified exact, lossless or lossy | Tests and measured quality on a host |
| 16 | [Who takes the DAC from Android](decisions-library.md#claiming) | Kotlin claims control and playback interfaces, then eligible IN interfaces for implicit feedback | Tests for claim planning |
| 17 | [Three outputs and one that chooses](decisions-library.md#outputs) | `OutputSelector` with a stated guarantee and typed reasons | Tests |
| 18 | [What the library limits itself to](decisions-library.md#scope) | One DAC per process, UAC 1 and 2, playback; events on their own thread | Tests |

## At a glance, driver by driver

Short answers; the decision pages have the file paths and the reasoning. "Flick" is `versionF` at `11395a49`.

| | Flick | Mornye | BitChord | usbdac |
|---|---|---|---|---|
| Bus queue | 16 x 8 for PCM at 125 us; 32 x 8 for DSD and DoP; 128 ms at 1 ms | 4 x about 4 ms = 16 ms | 4 x about 4 ms = 16 ms | 8 x 2 ms = 16 ms, any interval, configurable |
| Ring | 100 ms, capacity 200 ms | 1.5 s | 200 ms | 250 ms, capacity 500 ms, 100 ms while live processing |
| Timed-out transfer | Survives for PCM, counted | Ends the stream | Ends the stream | Survives, counted, any encoding |
| Packet-level error | Ends the stream | Ends the stream | Ends the stream | Ends the stream |
| Pause | Silence, ring held | Flush and re-seek; bus idle | Stop submitting; queue kept | Silence, ring held |
| Seek or stop | Ring cleared, bus plays out | Cancel all in flight | Cancel all in flight | Cancel what carries music, refill with silence |
| Position | Decoder | Completed frames | Completed frames | Completed frames, epoch-tagged |
| Clock | First source if the terminal names a selector | Walks selectors, checks range | Terminal's clock id directly | Walks selectors, checks range, reports the path |
| Explicit feedback | Windowed poll, smoothed | One-packet poll, direct | One-packet poll, direct | Newest valid report per poll batch, capacity checked and smoothed |
| Stale feedback | Warning | Ends the stream after 2 s | Ends the stream after 2 s | Clears lock and verification, emits `FeedbackStale`, fails at the deadline |
| Implicit feedback | Plays at nominal rate | Refuses | Refuses | Follows a compatible UAC2 IN stream on the same clock |
| Volume | DAC, software or system | DAC only | DAC only | DAC; software gain ends bit-perfect |
| Slider | Linear in dB (floor proposed in #416) | Linear in dB | Floor -80 dB, 16 steps | Floor -80 dB, 16 steps |
| Start-up level | App restores a saved level or 0.40 | At most -40 dB | At most -40 dB | At most -40 dB, verified |
| Level on close | App's job | Restored at end of playback or idle, not between songs | Restored (since `ea2df7e4`) | Restored, kept or forgotten |
| Volume keys | System volume only (#417 open) | Not routed (README) | Routed to the DAC | Optional helpers |
| Idle release | #418 open (30 s, claim kept) | 5 s, DAC given back to Android | 30 s | `DacIdleRelease`, 30 s, claim kept |
| Bit-perfect claim | Flag with a payload audit | By construction | By construction | Seven checks, named blocker |
| Conversion | Engine resamples | Automatic, 130 dB design | Opt-in setting, dithered | Opt-in module, classified |
| Interface claim | Kotlin force-claim | libusb auto-detach | libusb auto-detach | Kotlin force-claim |
| Outputs | Direct USB, AAudio | Direct USB, Android 14 mixer, AAudio | Direct USB, Android 14 mixer, AAudio | Same three plus a selector |

## Where the three agree

- **The DAC's own volume, not software gain, on a direct stream.** All four say so, and three lower the level at start (Mornye, BitChord and usbdac).
- **Four transfers of about 4 ms or eight of about 2 ms, 16 ms in all.** The two shapes are in practice the same queue.
- **The 2 % band on feedback.** A report more than 2 % from nominal is ignored by all four, because a broken feedback endpoint must not push packets past the endpoint's capacity.
- **Count what the DAC actually took.** Position and "played" frames come from completed transfers in three of the four.

## Where they differ, and which differences are open

- **Per-packet errors end the stream in all four.** BitChord issue #1485 reports this on a Snapdragon phone, and a third-party patch counts them. usbdac has the same behaviour and no data on how often it happens on other phones. See [decision 3](decisions-stream.md#loss-handling).
- **Feedback smoothing.** usbdac and Flick average, Mornye, BitChord and the Linux kernel do not. It has not been measured on a DAC which is better. See [decision 7](decisions-clock.md#feedback).
- **Implicit feedback and fallback.** Mornye and BitChord refuse it. Flick uses nominal timing. usbdac 0.1.1 follows a compatible UAC2 IN source and lets the app fall back when it cannot use one. See [decision 8](decisions-clock.md#implicit-feedback).
- **The claim path.** usbdac's Kotlin SDK force-claims control and playback interfaces and tries eligible capture interfaces for implicit feedback. Flick's pull request description records a streaming interface claim failure on the DC04U. See [decision 16](decisions-library.md#claiming).
- **The size of the queue that works best.** The two phones with reported data disagree on how deep a queue can be before it fails (see [decision 1](decisions-stream.md#queue-geometry)). The default is the shape measured on the one phone usbdac has run on, and it can be changed.

## How each decision page is laid out

1. **The question**, in one or two sentences, followed by **in plain terms**.
2. A table with **what each driver does** and **what usbdac does**, with file paths. Paths are relative to the repository named on [the drivers page](drivers.md).
3. **What usbdac does**, in a paragraph, with the API names.
4. **Evidence**: hardware data (with the device), commit messages and pull requests (named), the public specifications and source (linked from [the references](references.md)), and tests (named). A claim that rests on a commit message that says "not measured" says so here too.
5. **Trade-off**: what the choice costs and who should choose differently.
6. **References**: links into [the references page](references.md).
