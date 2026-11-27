# Bit-perfect output

## What the library promises

On a direct USB stream the library never resamples, dithers or scales samples. PCM samples you `write` use the DAC's wire layout. DoP markers and native DSD packing are handled by the library, so byte-for-byte identity between the write buffer and USB packets is not a promise for every encoding. These are the transformations:

- **Software gain**, when you ask for it with `setSoftwareGain` and a value other than exactly 1.0.
- **The DoP marker byte.** The library owns the `0x05`/`0xFA` alternation and overwrites the marker you write, so the phase survives pause, flush and underrun.
- **The grouping of native DSD bytes** into the DAC's wire words, in the byte order the DAC wants (and with the bits reversed for a DAC that expects the other order). The DSD data is not changed.

Silence inserted while paused, flushed or underrun is zeros (or the DSD idle pattern), not a processed version of your audio.

## The verified flag

`DacStream.bitPerfect` is a `StateFlow<BitPerfectState>`. For a direct stream it is `Verified`, or `NotVerified(blocker, reason)`, where `blocker` is the first condition that fails. `DacEvent.BitPerfectChanged` announces a change, and `DacStream.stats().bitPerfectBlocker` carries the same blocker.

`Verified` holds only while all of these are true:

| # | Condition | Blocker when it fails |
|---|---|---|
| 1 | The stream is open and its output thread is running | `NO_STREAM` |
| 2 | No frame queued to the DAC was lost on the bus since the stream started. The first lost frame clears the flag for the life of the stream | `LOST_FRAMES` |
| 3 | The format on the wire is the one requested: no `RatePolicy.NEAREST_SUPPORTED` rate change | `FORMAT_CHANGED` |
| 4 | Live feedback has locked when the route uses explicit or implicit feedback | `FEEDBACK_UNLOCKED` |
| 5 | The DAC's sampling frequency was verified: its read-back after the alternate setting switch equals the requested rate. A stream admitted on an acknowledged SET with unusable read-back (`SkipClockValidation`, `requireVerifiedRate = false`, `ALLOW_UNUSABLE_AFTER_SUCCESSFUL_SET`) plays but stays unverified | `CLOCK_UNVERIFIED` |
| 6 | Software gain is exactly 1.0 | `SOFTWARE_GAIN` |
| 7 | The route has valid pacing. An asynchronous endpoint using the nominal-timing opt-in cannot be verified | `UNSYNCHRONIZED_ASYNC` |
| 8 | The wire payload audit passes | `PAYLOAD_UNVERIFIED` |

On a feedback-paced route, use `format.pacing` to identify the selected source
and `stats().feedbackLocked` to check its live state. The clock readback and
feedback lock answer different questions. A successful rate readback says
what rate the DAC is set to. Feedback shows that the host can follow it while
streaming. [Feedback diagnostics](controls.md#diagnostics-the-clock-and-the-pacing)

The payload audit looks at what is actually in the packets. For PCM it needs real audio: channel 0 must vary within a transfer, with a grace of 1.5 s so digital silence or a fade does not flip the flag. For DoP the markers must alternate and agree across channels. For native DSD the data must not be all zero. A new stream, or one that has only been fed silence, can therefore report `NotVerified` with `PAYLOAD_UNVERIFIED` until real audio has passed.

`BitPerfectBlocker.UNKNOWN` appears when a newer native library reports a blocker this version of the SDK does not know.

## What the flag does not cover

The claim is about the transport. It is not a hash of the source.

- If the host writes processed samples (an equaliser, volume applied in software, resampling, dither), the library cannot know. A decoder that outputs float and converts to 24-bit integers has already changed the samples before they reach `write`.
- An **underrun** pads the wire with silence and does **not** clear the flag. Watch `DacEvent.Underrun` and `stats().underruns`.
- The payload audit is a statistical guard against a dead or stuck pipeline.
- The library cannot see what the DAC does with the stream internally: its own filters, its oversampling, its volume.

## PCM

Write the samples as the DAC will receive them: 16-bit as `i16`, 24-bit packed in a 3-byte slot or in the upper three bytes of a 4-byte slot, 32-bit as `i32`, little-endian, channels interleaved. `negotiate` needs an alternate setting whose valid bits equal the requested depth (`StreamRequest.pcm(rate, channels, bits)`); the library chooses among those, and `StreamFormat.validBits` and `subslotBytes` tell you the layout. A 16-bit request on a DAC that has only 24-bit alternate settings fails with `UNSUPPORTED`. Request 24 bits and pad the low bits of each sample with zeros, which keeps every value, or let `OutputSelector` do it (`allowWidening`, on by default). See [the native API document](native-api.html#4-sample-layouts) for the table.

## DSD

Two ways to carry DSD, chosen with the request. Check `capabilities.supportsDop` and `supportsNativeDsd` first; the former describes a feasible PCM carrier, while the latter comes from native DSD alternate settings and legacy quirks.

**DoP (DSD over PCM).** `StreamRequest.dop(dsdBitRate, channels)`. The carrier is 24-bit PCM at `dsdBitRate / 16`: DSD64 (2,822,400) needs 176.4 kHz, DSD128 352.8 kHz. `capabilities.dopMaxDsdBitRate` is the highest rate the PCM carrier can transport. DoP retains PCM framing on UAC3; neither capability field proves that the device decodes the markers. See the [DoP Open Standard](references.md#ref-dop). Each sample you write is the DoP word in the format's slot layout: 16 DSD bits in the two bytes below the marker (`dsd_high`, the older 8 bits, most significant bit first). The marker byte is ignored. The marker phase belongs to the stream, not to the track: it continues across a gapless track join, pause, flush and underrun, and only a new stream starts again at `0x05`; there is no call to reset it. Pause, priming and underrun fill carries the DSD idle pattern `0x69` under markers that keep alternating. Any software gain is refused for DoP, because it would destroy the signal. A DoP stream is bit-perfect on the same terms as PCM, and its payload audit checks that the markers alternate.

**Native DSD.** `StreamRequest.dsdNative(dsdBitRate, channels)`, for a DAC with a native DSD alternate setting. You write raw DSD bytes, most significant bit first, one byte per channel per step, interleaved (`[ch0, ch1, ch0, ch1, ...]`). The library groups consecutive bytes of a channel into the DAC's wire word. For DSF, the format field value `1` means least-significant-bit first and needs per-byte reversal; value `8` means most-significant-bit first and needs no reversal. Deblock each channel, interleave channels chronologically, and trim to the declared sample count. Raw DSDIFF Channel Bytes are MSB-first and interleaved in Channels Chunk order; exclude IFF padding and reject unsupported compression. For example, DSF field `1` byte `0x01`, DSF field `8` byte `0x80`, and raw DSDIFF byte `0x80` all represent the same chronological byte `0x80` after the DSF field `1` byte is bit-reversed. For a two-channel pair, DSF field `1` channel bytes `[0x01, 0x02]` and `[0x80, 0x40]` become the same interleaved USB input as DSF field `8` and raw DSDIFF bytes `[0x80, 0x01, 0x40, 0x02]`. The host performs file decoding and deblocking. The counters (`playedFrames`, `latencyFrames`) count one DSD byte per channel, at `bitRate / 8` (`StreamFormat.counterRateHz`). Standard [UAC3 DSD](references.md#ref-uac3-formats) uses 8-byte, 64-bit words per channel at `bitRate / 64`. The library puts the oldest bit in D63 and emits the word little-endian (LE64), reversing each channel's eight chronological input bytes on the wire. Legacy native DSD keeps its 1-, 2- or 4-byte layouts and the `dsdBigEndian`, `dsdSubslot` and `DsdBitReverse` controls; these do not override the standard UAC3 layout. In 0.1.11, as since 0.1.5, a legacy layout is admitted only for the AP80 Pro-X, the MOONDROP Dawn Pro, or when the host sets those controls ([decision 5](decisions-stream.md#dsd)). A wrong legacy layout can corrupt playback, and the library cannot detect it.

DoP and native DSD are not available through [`AndroidManagedOutput`](transports.md), which plays PCM only.

## MQA

MQA is ordinary PCM whose signalling is carried in the low bits of the samples. The library does no MQA decoding and does not detect MQA. On a direct stream with `Verified` state the PCM reaches the DAC unmodified, so the signalling arrives intact, provided the host decodes the file to PCM without changing it (no gain, no resampling, no dither) and uses the DAC's **hardware volume**, never software gain. A DAC with a full MQA decoder can then do the unfolding and rendering; **a full-decoder DAC is required**. A DAC without one plays the stream as ordinary PCM and does not render the signalling. The same holds for any other signalling carried in the low bits.

Nothing in this library decodes MQA, and it does not offer unfolding in software.

## The managed output

`AndroidManagedOutput` reports its own states, because the library cannot verify what the platform does:

| State | Meaning |
|---|---|
| `RequestedViaPlatform` | Android 14 or later, a USB device and a supported format: the platform accepted the request for the bit-perfect mixer behavior (`AudioMixerAttributes.MIXER_BEHAVIOR_BIT_PERFECT`) and kept it. Android enforces it. The library cannot confirm that the samples reach the DAC unchanged |
| `Unverifiable` | Anything else: an older Android, no device chosen, a device that is not USB, a format the platform offers no bit-perfect mixer for, the platform refusing the request, or the app lacking `MODIFY_AUDIO_SETTINGS`. The reason says which. The track plays at the source's rate and encoding; whether the system mixes, scales or resamples depends on the device and the route |

Neither state is `isVerified`. See [transports](transports.md).

## The AAudio output

`AAudioExclusiveOutput` asks the platform for an exclusive AAudio stream at the source's own rate, channel count and format, and writes your bytes into it unchanged. It reads back what the platform granted, and its `bitPerfect` is **always** `NotVerified(blocker = null, reason)`: an app cannot see what the audio HAL and the device do with the samples, so the library never claims `Verified`. The reason is as precise as the grant allows:

| `grant.verdict` | The reason says |
|---|---|
| `exclusive` | Exclusive mode was granted at the requested rate, format and channel count: the app mixer is bypassed, but the device's audio HAL is not verifiable from an app. On Android 14 and later it adds that the hardware reports the same rate, format and channel count |
| `shared` | The platform granted shared mode, not exclusive: the audio goes through the system mixer, which can resample, mix it with other sounds and apply volume and effects. With `requireExclusive` on (the default) the open fails instead |
| `hardware_rate`, `hardware_format`, `hardware_channels` | (Android 14 and later) The hardware runs another rate, format or channel count than the stream: AAudio converts before the device |

A stream that would play the bytes wrongly (another rate, channel count or format than the one you write) is not reported but refused: `open` fails with `UNSUPPORTED`. Nothing is dithered, scaled or resampled by the library on this path, and a decoder that outputs float and converts to integers has already changed the samples before they reach `write`, as for every output. See [transports](transports.md#aaudio-exclusive-aaudioexclusiveoutput).

## Conversion is not bit-perfect

The direct path never converts. If the app converts before writing (a rate the DAC refuses, a depth it lacks), the stream is only as bit-perfect as the conversion is exact. `ConversionPlan.kind` says which it is:

| `ConversionKind` | Examples | `ConvertingOutput.bitPerfect` |
|---|---|---|
| `NONE` | The formats are the same | The target's state |
| `BIT_EXACT` | 16-bit to 24 or 32 (the new low bits are zero), 24-bit packed to 24-bit in 4 bytes and back: every sample value is unchanged | The target's state: still `Verified` if the stream is |
| `LOSSLESS` | Mono duplicated to stereo; 16 or 24-bit integers carried in a float | `NotVerified(null, plan.description)`: the audio is not identical |
| `LOSSY` | A new sample rate; fewer bits; floats to integers | `NotVerified(null, plan.description)`: **not bit-perfect** |

So `RatePolicy.NEAREST_SUPPORTED` plus a `ConversionPlan` lets an app play a rate the DAC refuses, and the library says plainly that this is not bit-perfect: the direct stream reports `FORMAT_CHANGED` because its rate differs from the request, and `ConvertingOutput` reports the plan's description. A lossy plan in front of a stream negotiated `EXACT` at the target format is the case where the DAC's stream can be `Verified` (the bytes reach it unmodified) while the *audio* is not the source's, which is why `ConvertingOutput` reports the plan, not the transport. The description is a sentence such as "Lossy conversion, not bit-perfect: resampled from 44100 Hz to 48000 Hz (windowed-sinc polyphase, high quality, stopband about 110 dB); quantised to 24-bit in a 4-byte slot with TPDF dither (full scale saturates)." The measured quality of the resampler (passband flat to 0.00004 dB, stopband 109 dB or better, nothing aliased into the output with `HIGH`) is in [the native API document](native-api.html#14-conversion-helpers): a good resampler is still a resampler. MQA signalling in the low bits does not survive any lossy step, and the PCM conversion helper refuses bitstreams; the separate selector DSD-to-PCM path is lossy and explicit DoP unpacking requires its own consent. That converter maps DSD to PCM with ±1.0 at 100 % modulation, so a 50 % modulated signal comes out at -6.02 dBFS, and -9.52 dBFS with the default -3.5 dB headroom (section 14.7 of [the native API document](native-api.html#14-conversion-helpers)).

## The guarantee the selector reports

`OutputSelector` states, before anything plays, who vouches for the samples on the output it chose (`OutputDecision.guarantee`). It maps onto the states of this page, and the output's `bitPerfect` is still what is true while playing:

| `Guarantee` | Output | Its `bitPerfect` while playing |
|---|---|---|
| `VERIFIED_BY_LIBRARY` | Direct USB, exact or with a bit-exact widening | `Verified`, or `NotVerified` with the blocker |
| `REQUESTED_FROM_PLATFORM` | Android's bit-perfect mixer on a USB DAC | `RequestedViaPlatform` |
| `EXCLUSIVE_GRANTED_UNVERIFIABLE` | Exclusive AAudio, as requested | `NotVerified(null, reason)` |
| `NONE` | Standard output, shared AAudio, direct USB behind a conversion that changes the audio, direct USB paced without feedback | `Unverifiable` or `NotVerified` |

See [automatic output](automatic-output.md#what-each-guarantee-means).

## The states at a glance

| `BitPerfectState` | From | `isVerified` | What it says |
|---|---|---|---|
| `Verified` | `DacStream` | `true` | The library's own checks all pass |
| `NotVerified(blocker, reason)` | `DacStream` | `false` | The first failing condition, with a sentence for a status line |
| `RequestedViaPlatform(detail)` | `AndroidManagedOutput` | `false` | The platform accepted a bit-perfect request |
| `Unverifiable(reason)` | `AndroidManagedOutput` | `false` | Nothing can be said |
| `NotVerified(null, reason)` | `AAudioExclusiveOutput` | `false` | The grant's exact reason: exclusive but not verifiable, shared, or converted before the hardware |
| `NotVerified(null, description)` | `ConvertingOutput` | `false` | The plan's description: what was done to the audio before it reached the output |
