# Design decisions: reporting and conversion

Three decisions about what the library says is happening to the audio: when it may call a stream "bit-perfect", what it does about MQA, and what it does when a DAC cannot play a file as it is. They have one thing in common. A driver can make a wrong claim in a way a listener cannot hear, so each decision is about how much the library is allowed to say and how it finds out.

The parts of each decision are the question, what the three drivers do, what usbdac does, the evidence, the trade-off and the public references. Revisions are defined on [the drivers page](drivers.md#revisions-studied); the [overview of all decisions](decisions.md) has a summary table. The user-facing description of the flag is [bit-perfect output](bit-perfect.md).

## 13. When the library may say "bit-perfect" {#bit-perfect-claim}

**The question.** An app wants to show a "bit-perfect" indicator. What does the library check before it says yes, and what does it say when it says no?

**In plain terms.** "Bit-perfect" means the DAC receives exactly the numbers in the file. A player can make that true only by not touching the samples, and it can make it false by accident in many ways: a lost packet, a rate the DAC did not accept, a software volume left on, a stream that is silent because something upstream broke. An indicator that is simply switched on whenever direct USB is open is correct most of the time and wrong exactly when it matters.

| | What it does | Where |
|---|---|---|
| Flick | Reports `bit_perfect_verified` and `pcm_payload_verified` in its diagnostics, with a log line "verified ... no mixer, no resample, no DSP". The payload check has a 1.5 s grace so a fade or a silent passage does not flip it. Since PR #406 the first lost frame clears the flag for the rest of that stream (PCM only; DSD timeouts are still fatal) | `rust/src/uac2/android_direct.rs` (`ANDROID_USB_PAYLOAD_FLICKER_GRACE_MS`, `bit_perfect_verified`), `iso_transport_loss.rs` |
| Mornye | The transport changes no sample, and the transport code has no run-time check of what is on the wire. What the player shows rests on the path it chose: no resampler, no gain, no DSP in USB mode. For AAudio it says to verify the granted API, sharing mode, rate, channels and format before reporting the output as active, and to close it if no exclusive stream matches | `usb_audio/README.md` |
| BitChord | The same design. Its converter for DACs that cannot take a track says that "a stream through here is never reported as bit-perfect" | `bitperfect/DacFormatConverter.kt` |
| usbdac | `DacStream.is_bit_perfect()` is true only while all of these hold, and `bit_perfect_blocker()` names the first that fails: the stream is open and running (`NO_STREAM`); no frame queued to the DAC was lost (`LOST_FRAMES`, latched for the life of the stream); the format on the wire is the one requested (`FORMAT_CHANGED`); the DAC confirmed its sampling frequency (`CLOCK_UNVERIFIED`); software gain is exactly 1.0 (`SOFTWARE_GAIN`); the endpoint is paced by feedback or by the host's own timing (`UNSYNCHRONIZED_ASYNC`); and the wire payload audit passes (`PAYLOAD_UNVERIFIED`) | `stream.rs` (`BitPerfectBlocker`), `transport/session.rs`, `transport/encode.rs` (`audit_pcm_payload`, `audit_dsd_payload`), `payload_audit_latch.rs` |

**What usbdac does.** The flag is a `StateFlow` with an event on every change and a reason in words. The two outputs that go through Android (the managed output and AAudio) never report `Verified`, because an app cannot see past Android's audio system; they report what was requested and what was granted. The output selector states the guarantee that applies before the first sample, and the stream's own flag is what is true while it plays. See [automatic output](automatic-output.md#what-each-guarantee-means).

**Evidence.**

- The lost-audio rule comes from Flick's own history. Commit `4d23976a` on Flick's own branch is titled "stop reporting lost ISO audio as consumed and bit-perfect", and the same change is in Flick PR #406 (`e39794ac`). The fault-injection runs in that pull request show the flag clearing on the first lost frame (see [the evidence page](evidence.md#the-fault-injection-runs)); the pull request notes that the indicator turning off was not observed in those runs because it was already off.
- The payload audit is described in the code as "a statistical guard, not a bit-exact hash": it decodes channel 0 and requires at least two distinct values in the transfer for PCM, alternating `0x05` / `0xFA` markers that agree across channels for DoP, and a non-zero buffer for native DSD (`audit_pcm_payload`, `audit_dsd_payload`). It catches a dead or stuck pipeline. It cannot tell a processed signal from an unprocessed one.
- Two limits are written into the documentation because the library cannot know them. An **underrun** pads the wire with silence and does not clear the flag (watch `DacEvent.Underrun`), and **processing before `write`** is invisible: a decoder that outputs float and converts to integers has already changed the samples. The host is responsible for not claiming more than it did.

**Trade-off.** The flag can say "not verified" at the start of a track that opens with digital silence, until real audio has passed the audit, and for 1.5 s after the audit last failed. That is deliberate: the library prefers to under-claim. A host that wants a steadier indicator can wait for the first `Verified` after the first non-silent second.

**References.** [UAC 2.0 sampling frequency control](references.md#ref-uac2), [USB 2.0: no retry on isochronous](references.md#ref-usb2), [Flick #406](references.md#ref-flick-pr406), [Mornye README](references.md#ref-mornye-commits), [Android AAudio sharing mode](references.md#ref-android-aaudio).

## 14. MQA {#mqa}

**The question.** MQA files play as ordinary PCM on any DAC. Should the library detect, decode or treat MQA specially?

**In plain terms.** An MQA file is a normal PCM file with extra signalling hidden in its least significant bits. A DAC with an MQA decoder finds the signalling and unfolds the audio. Everything else plays it as PCM. If the samples reach the DAC untouched, the DAC can do the rest.

| | What it does |
|---|---|
| Flick, Mornye, BitChord | None of the three USB transports has any MQA code. The only mention in Flick is in a streaming-service source definition (`lib/services/sources/tidal_service.dart`) |
| usbdac | None. The library does not detect MQA and does not decode it. When the stream is `Verified` the PCM reaches the DAC unmodified, which is what a DAC's own decoder needs. The host has to decode the file to PCM without changing it (no gain, no resampling, no dither) and must use the DAC's hardware volume. Any lossy conversion destroys the signalling, and conversion is never applied to a direct stream unless the host asks for it |

**Evidence.** Wikipedia describes MQA as distributed in a 24-bit stream whose most significant bits are PCM that non-MQA equipment plays, with the lower-order bits rendered as noise by equipment without a decoder. The article does not say where the signalling sits, so usbdac makes no claim about the layout. Android's documentation says its bit-perfect mixer attribute enables playback of MQA and DSD formats through the audio framework. No MQA DAC has been used with usbdac, and nothing here has been checked against a decoder.

**Trade-off.** Doing nothing is the only option that cannot break the signalling, and it puts the work on the host (a lossless decode, no software volume) and on the DAC (a full decoder; a DAC that does only the first unfold, or none, plays the file as ordinary PCM).

**References.** [MQA in Wikipedia](references.md#ref-mqa), [Android bit-perfect mixer behaviour](references.md#ref-android-mixer).

## 15. When a DAC cannot play the file as it is {#conversion}

**The question.** A file at 96 kHz, or 24-bit, or mono, meets a DAC that offers only some rates, some depths or two channels. Does the driver convert, when, and how loudly does it say so?

**In plain terms.** The alternatives are to refuse the file, to let Android's mixer play it (which converts at a quality nobody chose, and does other things as well), or to convert it in the player with a good filter. Whatever is chosen, the result is no longer the file's own samples, and the listener should be told.

| | What it does | Where |
|---|---|---|
| Flick | The engine can prepare a playback format whose rate differs from the requested one and reports `resampling_active` when the rates differ | `rust/src/uac2/android_direct.rs` |
| Mornye | Converts automatically a song whose rate the DAC refuses (commit `208bd7569`, 2026-11-18): a Kaiser-windowed sinc applied as a polyphase rational resampler, 130 dB stopband, passband to 0.907 of the lower Nyquist frequency, dither (plain or high-pass, chosen in `4b47a0745`) when the output is 16 bit or less. "The transport itself never changes a sample." It also tries a DAC's clock range first so that it does not ask for a rate the DAC will not take, and has a "Highest sample rate" setting | `usb_audio/resample.rs`, commits `208bd7569`, `da608a663`, `30ec85296` |
| BitChord | `DacFormatConverter`, on only when the user turns on "convert for limited DACs": a windowed-sinc rate conversion and TPDF dither when rounding to the DAC's depth, digital silence preserved. "This is the one place on the exclusive path where samples change, so a stream through here is never reported as bit-perfect." The comment gives the alternative: "not an untouched track but Android's mixer, which does the same conversion at a quality nobody chose" | `bitperfect/DacFormatConverter.kt` |
| usbdac | The direct stream never converts. A separate opt-in module (`usbdac_core::convert`, Kotlin `PcmConverter`, `ConvertingOutput`) does: a Kaiser-windowed sinc polyphase resampler for any rational ratio, widening and narrowing of integer depth with TPDF dither, float, and mono to stereo. Every plan is classified `NONE`, `BIT_EXACT`, `LOSSLESS` or `LOSSY`, and anything other than the first two turns the bit-perfect flag off with the plan's description as the reason. DSD is never converted. The output selector only converts when its policy says `allowConversion` | `crates/usbdac-core/src/convert/`, `android/usbdac/.../ConvertingOutput.kt`, `PcmConverter.kt` |

**What usbdac does.** The decision is the host's, per request: `RatePolicy.NEAREST_SUPPORTED` makes `negotiate` return the rate the DAC runs, `ConversionPlan.between(source, format)` works out the smallest chain (a widening before a rate change, the same rate at another depth before the nearest rate) and its classification, and `ConvertingOutput` puts the converter in front of the stream while keeping the position arithmetic in source frames. `BIT_EXACT` covers the conversions that cannot change a value, such as 16 bits to 24 or 24 bits in 3 bytes to 24 in 4, and leaves the stream `Verified`.

**Evidence.**

- **Quality is measured, and the numbers are in the repository.** The `High` setting is designed for 110 dB of stopband attenuation with the passband to 91 % of the lower Nyquist frequency. The test `quality_tests.rs` measures 109.2 to 112.5 dB for the downsampling pairs it covers, spurs of -120 dB or lower, and passband ripple of at most 0.000037 dB; the tables are in [the native API document](native-api.html#14-conversion-helpers) and are reproduced by `cargo test -p usbdac-core quality -- --nocapture`. They were measured in `f64` on a host computer; no phone has run the converter.
- **The three conversion designs are the same family.** All three use a windowed-sinc filter and triangular (TPDF-type) dither when narrowing, and Mornye and usbdac use a Kaiser window in a polyphase rational resampler. That is the textbook design (Julius O. Smith's resampling notes and the Lipshitz, Wannamaker and Vanderkooy survey of dither are the public references). usbdac's module documentation derives it from scratch. Mornye's design target is 130 dB where usbdac's is 110 dB; neither has been compared against the other.
- **All three decided the conversion must be labelled.** BitChord's comment is the clearest statement; Mornye's README says "There is no resampling or gain" for the transport

**Trade-off.** Mornye converts automatically so that a file always plays. usbdac converts only when told, because a library that converts by itself could report a stream as direct and verified while the samples have changed, and because the right answer to "the DAC refuses 96 kHz" depends on the app (convert, ask the listener, or hand the track to Android). The cost is that a player that wants Mornye's behaviour has to turn on `allowConversion` and show the description. A resampler is a CPU cost (about 0.26 % of one Mac core for 44.1 to 48 kHz) and adds `T/2` frames of latency (1.8 ms at 44.1 kHz for `High`); `ConvertingOutput.latencyFrames` includes it.

**References.** [Julius O. Smith, Digital Audio Resampling](references.md#ref-jos-resampling), [Lipshitz, Wannamaker, Vanderkooy on dither](references.md#ref-dither), [Mornye commits](references.md#ref-mornye-commits).
