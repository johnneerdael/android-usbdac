# Bit-perfect output

## What the library promises

On a direct USB stream the library never resamples, dithers or scales samples. The bytes you `write` are the bytes that go into the USB packets, in whole frames of the DAC's own format. Three things are not a copy of your bytes, and they are the only ones:

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
| 4 | The DAC's sampling frequency was verified: its read-back equals the requested rate. A DAC whose quirk is `SkipClockValidation` is trusted on its acknowledgement | `CLOCK_UNVERIFIED` |
| 5 | Software gain is exactly 1.0 | `SOFTWARE_GAIN` |
| 6 | The endpoint is paced by the DAC's own feedback, or follows the host's timing. An asynchronous endpoint streamed without feedback (`allowUnsynchronizedAsync`) is never bit-perfect | `UNSYNCHRONIZED_ASYNC` |
| 7 | The wire payload audit passes | `PAYLOAD_UNVERIFIED` |

The payload audit looks at what is actually in the packets. For PCM it needs real audio: channel 0 must vary within a transfer, with a grace of 1.5 s so digital silence or a fade does not flip the flag. For DoP the markers must alternate and agree across channels. For native DSD the data must not be all zero. A new stream, or one that has only been fed silence, can therefore report `NotVerified` with `PAYLOAD_UNVERIFIED` until real audio has passed.

`BitPerfectBlocker.UNKNOWN` appears when a newer native library reports a blocker this version of the SDK does not know.

## What the flag does not cover

The claim is about the transport. It is not a hash of the source.

- If the host writes processed samples (an equaliser, volume applied in software, resampling, dither), the library cannot know. A decoder that outputs float and converts to 24-bit integers has already changed the samples before they reach `write`.
- An **underrun** pads the wire with silence and does **not** clear the flag. Watch `DacEvent.Underrun` and `stats().underruns`.
- The payload audit is a statistical guard against a dead or stuck pipeline.
- The library cannot see what the DAC does with the stream internally: its own filters, its oversampling, its volume.

## PCM

Write the samples as the DAC will receive them: 16-bit as `i16`, 24-bit packed in a 3-byte slot or in the upper three bytes of a 4-byte slot, 32-bit as `i32`, little-endian, channels interleaved. A source with fewer bits than the DAC's slot can be widened losslessly by padding the low bits with zeros. Ask for the depth your source has (`StreamRequest.pcm(rate, channels, bits)`) and let the library choose the alternate setting; `StreamFormat.validBits` and `subslotBytes` tell you the layout. See [the native API document](native-api.html#4-sample-layouts) for the table.

## DSD

Two ways to carry DSD, chosen with the request. Check `capabilities.supportsDop` and `supportsNativeDsd` first; both are read from the DAC's descriptors.

**DoP (DSD over PCM).** `StreamRequest.dop(dsdBitRate, channels)`. The carrier is 24-bit PCM at `dsdBitRate / 16`: DSD64 (2,822,400) needs 176.4 kHz, DSD128 352.8 kHz. `capabilities.dopMaxDsdBitRate` is the highest the DAC can carry. Each sample you write is the DoP word in the format's slot layout: 16 DSD bits in the two bytes below the marker (`dsd_high`, the older 8 bits, most significant bit first). The marker byte is ignored. Any software gain is refused for DoP, because it would destroy the signal. A DoP stream is bit-perfect on the same terms as PCM, and its payload audit checks that the markers alternate.

**Native DSD.** `StreamRequest.dsdNative(dsdBitRate, channels)`, for a DAC with a native DSD alternate setting. You write raw DSD bytes, most significant bit first, one byte per channel per step, interleaved (`[ch0, ch1, ch0, ch1, ...]`). The library groups consecutive bytes of a channel into the DAC's wire word. **DSF files store the least significant bit first**: a host reading DSF must reverse the bits of every byte before writing. DFF is already most significant bit first. The counters (`playedFrames`, `latencyFrames`) count one DSD byte per channel, at `bitRate / 8` (`StreamFormat.counterRateHz`). The byte order and the slot size can depend on the DAC; the built-in quirk table and the `dsdBigEndian`, `dsdSubslot` and `DsdBitReverse` settings are the controls, and a wrong guess sounds like noise, which the library cannot detect.

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

## The states at a glance

| `BitPerfectState` | From | `isVerified` | What it says |
|---|---|---|---|
| `Verified` | `DacStream` | `true` | The library's own checks all pass |
| `NotVerified(blocker, reason)` | `DacStream` | `false` | The first failing condition, with a sentence for a status line |
| `RequestedViaPlatform(detail)` | `AndroidManagedOutput` | `false` | The platform accepted a bit-perfect request |
| `Unverifiable(reason)` | `AndroidManagedOutput` | `false` | Nothing can be said |
