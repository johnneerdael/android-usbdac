# Media3 integration

The optional `dev.usbdac:usbdac-media3` AAR publishes `UsbDacAudioSink`, `OutputSource`, `OpenedOutput`, `DacSessionOutputSource` and `DacVolumePlayer`. Use the same version as the base SDK. See [the Kotlin integration guide](guide-kotlin.md) and the compiled [`media3-example`](https://github.com/johnneerdael/android-usbdac/tree/main/android/media3-example).

The module targets Media3 1.11.1's `AudioSink.AudioSinkConfig` API. It deliberately supplies a dedicated sink. `DefaultAudioSink.setAudioOutputProvider` changes the final output beneath its processor chain, where PCM may already have been converted to 16-bit or float. Replacing only that output does not prove preservation of 24- or 32-bit integer precision.

## Sink contract

| Operation | Behavior |
|---|---|
| Format support | A cached source capability query reports direct support, transcoding or rejection for raw signed integer PCM, unsigned8 and IEEE float32; compressed formats are rejected |
| Configure | Retain track delay/padding metadata; open nothing until PCM arrives |
| Handle buffer | Skip encoder delay, retain trailing padding, write remaining integer bytes with backpressure |
| Same-format track | Discard previous track's held padding and retain the existing stream |
| Different format | Drain, use transactional source reconfigure when supplied, otherwise close/open |
| Position | Initial presentation timestamp plus hardware frames played, bounded by submitted frames |
| Discontinuity | Reconcile the next presentation timestamp against submitted/trimmed input |
| Flush / seek | Discard pending PCM and padding, reset counters; initial delay is not reapplied without configure |
| End of stream | Discard encoder padding, drain hardware, wait for `isDrained` |
| Volume | Forward multiplier to the output owner; PCM bytes stay unchanged |
| Speed / silence | Keep default speed and no silence skipping |

`OpenedOutput.format` must keep sample rate, channel count and counter rate equal to the request, with at least its bit precision and slot size. Wider slots are filled with zero low bytes (24-bit in four bytes becomes `00 LL MM HH`). Narrowing and resampling cannot be hidden behind a changed sink layout. An output owner that intentionally uses a converting `PcmOutput` reports the source write layout and exposes its conversion/guarantee separately.

The sink holds at most one bounded staging buffer plus encoder padding (at most 16 MiB). It does not discard malformed partial frames. It retains a partly accepted input buffer until Media3 retries it and maps DAC failures to Media3 write/initialization exceptions. Initialization errors are nonrecoverable. Write errors are recoverable only when the source opts in for that typed failure and the failed output closes successfully; otherwise playback stops with an error. A write failure's cause is `OutputRecoveryRequiredException`.

## Capability migration {#capability-migration}

`OutputSource.formatSupport(request)` must use a cached, side-effect-free capability snapshot. It returns `OutputSupport` with `UNSUPPORTED`, `DIRECT`, `WITH_TRANSCODING` or `UNKNOWN` and the source and known target. Existing custom `OutputSource` implementations default to `UNKNOWN`, which permits an open attempt without claiming direct decoder output. Override it when a reliable cached capability snapshot is available. Do not negotiate or open a device during this query. The bundled `DacSessionOutputSource` supplies its own snapshot. The sample controller uses the selector’s cached USB capabilities first, including supported rates above 192 kHz, and the managed fallback when appropriate. See the [published 0.1.11 migration history](migration.md) for its earlier managed-only behavior.

Recovery is opt-in per source: `canRecover` defaults to false. A source that can close and recreate its output may opt in for specific typed failures and rely on Media3 1.11.1 to reselect tracks and seek after a recoverable renderer error. The sink closes the failed output before reporting that error; `onRecoveryPosition` remains advisory. A retained session cannot recreate a lost DAC.

## Volume and lifetime

`DacVolumePlayer` takes a `StateFlow<DacVolume?>`, independently of stream selection. Open a `DacSession` before the first track to expose hardware volume immediately; `DacSessionOutputSource` retains that session while its track streams open and close. The core platform `DacVolumeProvider` remains independent of Media3.

Build the base sample without Media3 using `:sample:assembleDebug`. Build the optional foreground-service example using `:media3-example:assembleDebug`. The library has no service manifest declaration and starts no background work by itself.

The integration is covered by JVM tests for integer byte preservation, delay/padding across decoder buffers and equal-format tracks, backpressure, seek position, changed-format drain, unplug failures and final drain. Hardware decoding, USB timing and Android session-panel behavior require the acceptance procedure in the guide.

## Pinned renderer transitions

The [official 1.11.1 sources](https://dl.google.com/dl/android/maven2/androidx/media3/media3-exoplayer/1.11.1/media3-exoplayer-1.11.1-sources.jar) show `MediaCodecAudioRenderer.onOutputFormatChanged` copying encoder delay/padding into its raw sink format before `configure`. `MediaCodecRenderer` sets `needToNotifyOutputFormatChangeAfterStreamChange` when the stream changes, so even codec reuse notifies the audio renderer for the next stream. The last previous-stream buffer triggers `onProcessedStreamChange`/`handleDiscontinuity`; the first next-stream buffer gets the new configuration. `setOutputStreamOffsetUs` updates output metadata in DefaultAudioSink; it does not subtract from the sink's renderer-time position. The custom sink likewise returns renderer-time positions, which the renderer maps into its period. Seek calls the sink's `flush` before the next output timestamp anchors position.

## Additional Type I PCM encodings

`StreamRequest.pcmU8` and `pcmFloat32` are distinct direct USB requests. The sink maps Media3 `ENCODING_PCM_8BIT` to unsigned bytes and `ENCODING_PCM_FLOAT` to IEEE binary32 bits. Delay/padding trimming and backpressure retain those bytes exactly. Float32 NaN payloads, signed zero and subnormals are not converted to integer samples. Unsigned8 silence is `0x80`; float silence is positive zero. These two encodings require matching slots and never use signed-integer widening or software gain. Unsupported Type II/III compressed alternates are refused rather than reinterpreted as PCM.

An alternate advertising signed32 and float32 in the same four-byte geometry does not establish a live numeric interpretation. Such ambiguous UAC2/ADC3 alternates are refused until an active-format control is implemented. A bitmap lists possibilities; it is not proof that arbitrary source bits are decoded as the requested numeric format. See [USB Audio 2.0 section 4.9.2](https://usb.org/sites/default/files/Audio2_with_Errata_and_ECN_through_Sep_14_2026.pdf), which restricts grouped formats to ones handled by the same decoder.
