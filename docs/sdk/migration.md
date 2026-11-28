# Upgrading from 0.1.10

What an Android app built against 0.1.10 or later has to check when upgrading. Change both coordinates, `dev.usbdac:usbdac` and `dev.usbdac:usbdac-media3`, to the same version together. The newest changes come first; read every section from your current version up. None of the changes below has a new hardware result; [evidence](evidence.md#hardware-status) lists what has run on a DAC.

## Unreleased

These changes are on the development branch and in no published release yet.

- **No accepted bytes are lost when a DSD or DoP adapter's target fails.** The DoP unpacker, the DoP framer and the DSD-to-PCM converter that the selector puts in front of a stream used to advance the buffer and then throw when the stream below them failed. They now return the bytes they took and throw the stored failure, the same exception object, on the next call, as the PCM converter and the unsigned8, mono-DSD and native-DSD adapters already did in 0.1.17. A writer that counts its position from return values, as `UsbDacAudioSink` does, no longer loses those frames. The failure is permanent for that output: `flush`, `play` and `drain` rethrow it too, so close the output.
- **`writableBytes` of a native DSD direct stream follows the stream's room.** It reported the adapter's own 64 KiB staging space; it now reports the stream's room less the bytes of an incomplete wire word, like the other adapters, so a pull loop no longer writes far past the target depth.
- **`RoutingException.routeReport`** lists the controls a powered `apply` committed when only the ADC3 power hand-back failed afterwards. It is `null` when the transaction did not commit. Existing constructors are unchanged.
- **`DacErrorCode.HOST_USB`** is new; it decoded as `UNKNOWN` before. It comes from the macOS libusb host and is not raised on Android. An `Error` event whose message cannot be read now takes its code from the event's stable number, including 14 `RECONFIGURE_DRAIN_TIMEOUT` and 15 `RECONFIGURE_UNUSABLE`, instead of `UNKNOWN`. An exhaustive `when` over `DacErrorCode` without an `else` branch needs the new value.

## Changed in 0.1.17

- **An unplug keeps the example player on direct USB.** In the Media3 example, once a track has opened on direct USB, `DirectOutputController` forces direct USB until the user selects Android output (turns off `takeOverDac`). An unplugged DAC ends the track with `DEVICE_LOST` instead of continuing on the phone speaker; `canRecover` now recovers only `TRANSFER`. Copy that rule if your controller recovers write failures by reopening.
- **Padded PCM validation consumes the valid prefix.** `ConvertingOutput.write` checks the padding bits only of the frames it can convert in this call, so a sample with non-zero padding no longer blocks the audio in front of it: the call returns the bytes it took, and a later call throws `INVALID_ARGUMENT` at the chunk that holds the bad sample, which stays in the buffer. Heap and direct buffers make the same progress. Correct the layout or the padding before retrying; the library never truncates or masks samples.
- **Accepted counts before a stream failure.** `ConvertingOutput`, the unsigned8 expander and the mono-DSD expander return what they took when the stream below them fails, and rethrow that failure on the next call. `PcmOutput.write` now documents that the return value equals the position delta, and `UsbDacAudioSink` checks it and counts frames from the buffer's movement.
- **`RoutingException.powerRestoreFailure`.** A powered routing call whose ADC3 power hand-back also failed keeps its original error kind, failed change and rollback failures, and names the power failure in this new field.

## Changed in 0.1.16

- **DoP input is converted to PCM by default.** `OutputPolicy.allowDopPcmConversion` now defaults to `true` (it was `false` from 0.1.11, described below). With `allowConversion` on and the final decoder not confirmed for DoP, a `StreamRequest.dop` stream is validated, unpacked and converted to PCM; the decision's guarantee is `NONE`. Pass `allowDopPcmConversion = false` to keep the 0.1.11 to 0.1.15 behaviour, in which such a request gets no direct output and the Android outputs reject it.
- **Direct USB follows the driver's playback decision.** On direct USB the selector now asks the native driver for one playback plan (format, transform and fidelity) and opens exactly that format; it no longer ranks the DAC's alternates again in Kotlin. The decision's `processing` and `reason` name the transform (for example "DoP validated and unpacked to native DSD" or "Driver-selected PCM conversion") and, when the exact format was refused, why. A converted source is reported as not bit-perfect even when the wire path is.
- **More formats reach direct USB.** The driver path can expand unsigned8 to signed PCM, duplicate mono native DSD to stereo, unpack DoP to native DSD, frame native DSD as DoP for a confirmed decoder, and convert DSD or DoP to PCM, including a float32 wire target (`ConversionPlan.between` accepts a float target).

## Changed in 0.1.15

No library API or behaviour changed. The release rebranded the site, the README and both validator apps.

## Changed in 0.1.14

Version 0.1.14 supersedes the 0.1.11 behavior described below. Custom `OutputSource.formatSupport` implementations now default to `UNKNOWN`, allowing an open attempt; the sink reports it as supported with transcoding rather than promising direct output. The sample controller consults cached USB capabilities first, including rates above 192 kHz, before a managed fallback. Capability queries remain side-effect-free.

For Android DSD-to-PCM fallback, API 31 and later try 24-bit and 32-bit integer targets before 16-bit when the route supports them; exclusive AAudio with `floatSamples` uses float. Older Android versions retain 16-bit integer targets. These are candidate preferences, not a promise that any particular output accepts them.

Media3 write recovery is opt-in through `OutputSource.canRecover` for a typed failure and requires successful output closure. Sources unable to recreate the output keep the default nonrecoverable behavior. Downstream limits still require a suitable bound exclusive AAudio route for Android output; the selector may bypass limits for a direct DAC only when an explicit binding establishes a distinct route.


## Changes in 0.1.11

## Changes that need code

### Media3: override `OutputSource.formatSupport`

0.1.11 adds `OutputSource.formatSupport(request): OutputSupport`. `UsbDacAudioSink.getFormatSupport` and `supportsFormat` now ask it for every raw PCM format; in 0.1.10 the sink reported every mono or stereo PCM encoding it knew as supported directly. The 0.1.11 default answered `OutputFormatSupport.UNSUPPORTED` for every request; this changed to `UNKNOWN` in 0.1.14.

**A custom `OutputSource` that implements only `open`, `close`, `takeLookAgain` and `setVolumeMultiplier` still compiles, and then plays nothing:** the renderer is told that no format is supported. Override `formatSupport` with an answer from cached capabilities: `DIRECT`, `WITH_TRANSCODING` or `UNSUPPORTED`. Do not open, negotiate or probe a device in it. `DacSessionOutputSource` already implements it from the session's alternate settings.

In 0.1.11 the example's `DirectOutputController` answered from `OutputSelector.managedPcmFallbackSnapshot()` only; 0.1.14 uses cached USB capabilities. The 0.1.11 snapshot never reported `DIRECT`, and reported `UNSUPPORTED` for formats the Android-managed output could not carry (above 192 kHz, unsigned8, float32) even when the attached DAC could play them. See [Media3 integration](media3.md#capability-migration) before copying it.

### DoP input converted to PCM needs `allowDopPcmConversion`

0.1.16 changed this default back to `true`; see [changed in 0.1.16](#changed-in-0116). In 0.1.10 the selector could validate and unpack an explicit DoP request (`StreamRequest.dop`) and convert it to PCM whenever `allowConversion` was on and the decoder was not confirmed. From 0.1.11 to 0.1.15 that also needed `OutputPolicy.allowDopPcmConversion = true`, which was `false` by default. Without it, a DoP request with `dopSupport = DopSupport.UNCONFIRMED` gets no direct output, and the Android outputs reject it with `REQUEST_NOT_SUPPORTED`. Native DSD input (`StreamRequest.dsdNative`) is not affected: its PCM fallback still needs only `allowConversion`.

### `BUSY` while a reconfigure runs

While `DacSession.reconfigure` or `reconfigureCandidates` runs, including the drain of the old stream, `DacSession.negotiate` and the `DacSession.routing` calls on that session fail at once with `DacErrorCode.BUSY`. In 0.1.10 `negotiate` waited for the reconfigure to finish. `openStream` and `openDuplex` still wait for the reconfigure to return. Call again after it returns, or keep these calls on the thread that reconfigures.

## Behaviour that changed

| Area | 0.1.11 |
|---|---|
| DSD on Android outputs | With `allowConversion`, native DSD (and DoP with `allowDopPcmConversion`) can now be converted to PCM for the bit-perfect mixer, exclusive AAudio and standard outputs. 0.1.10 rejected DSD on all three. In 0.1.11 the PCM target was 16-bit (changed in 0.1.14), or 32-bit float for AAudio with `AAudioConfig.floatSamples`, at a standard rate ranked from the DSD bit rate over 16. The decision's guarantee is `NONE` |
| DSD on direct USB | Unchanged in kind: DoP framing for native DSD needs `dopSupport = CONFIRMED`; otherwise PCM conversion is negotiated starting at 24-bit and the DSD bit rate over 16 (176.4 kHz for DSD64), through the PCM ranking below |
| Downstream limits | `DownstreamLimits` still refuse every Android output, except that exclusive AAudio is now allowed on a built-in speaker or earpiece bound with `OutputSelector.bindDownstream(...)` and passed as `OutputPolicy.downstreamBinding` (API 28 and later). Without a binding, limits and no usable DAC, such as after an unplug, leave no output: `select` and `open` throw `UNSUPPORTED` |
| Managed output rates | `AndroidManagedOutput.open` refuses a rate outside 4,000 to 192,000 Hz, and a format `AudioFormat.Builder` rejects, with `UNSUPPORTED` before creating a track. The selector skips the managed outputs for such a request |
| Direct USB PCM candidates | The selector now ranks candidates from the DAC's descriptors first, keeps up to eight it can carry, and negotiates them in one call on the open session. A later candidate is tried only after a clean refusal of the earlier one; other failures end the attempt with their error. The ranking itself is unchanged (below) |
| Media3 format changes | `DacSessionOutputSource.reconfigure` makes one `DacSession.reconfigureCandidates` call with up to four same-rate widening candidates, with at most one rollback, in place of a retry per depth |
| Stream statistics | `workerGapOver10ms`, `workerGapOver50ms` and `maxWorkerGapUs` now exclude time in the libusb event pump. The new `maxWorkerLoopGapUs`, `maxEventPumpElapsedUs` and `maxEventPumpTimeoutOverrunUs` report it ([queue and timing evidence](support-diagnostics.md#queue-and-timing-evidence)) |

New API, all optional: `OutputSelector.managedPcmFallbackSnapshot`, `OutputSelector.bindDownstream` and `DownstreamBinding`, `OutputPolicy.downstreamBinding`, `DacSession.reconfigureCandidates`, `DacSession.routingHandBack`, `UsbDac.openPlaybackForRequest` with `PlaybackConfigurationFallback` ([checked playback open](integration-app.md#checked-playback-open-and-configuration-hand-back)), and `AAudioGrant.deviceIds`.

## Defaults that carry over from 0.1.10

These were already the defaults in 0.1.10. They are listed because an app moving from 0.1.9 or earlier, where conversion and widening were off and the headroom was 0 dB, or one that wants exact output only, has to set them.

| Setting | Default | Effect |
|---|---|---|
| `OutputPolicy.allowConversion` | `true` | The selector may resample, narrow or convert DSD to PCM for a request the output cannot run. The guarantee becomes `NONE` |
| `OutputPolicy.allowWidening` | `true` | Same-rate, same-channel integer widening (16 to 24 bits, 24 in 3 bytes to 24 in 4). Every sample value is kept and the guarantee stays `VERIFIED_BY_LIBRARY` |
| `OutputPolicy.allowDopPcmConversion` | `true` | With `allowConversion`, a DoP request whose final decoder is not confirmed is validated, unpacked and converted to PCM, as in 0.1.10. 0.1.11 to 0.1.15 defaulted it to `false`; set `false` to refuse it |
| `OutputPolicy.dopSupport` | `UNCONFIRMED` | DoP is never put on the wire by the selector until the host confirms that the final decoder reads DoP markers. A capable USB PCM carrier is not confirmation. `DacSession.negotiate` does not check it |
| `OutputPolicy.conversionOptions`, `ConversionOptions()` | `headroomDb = -3.5` | Applied after resampling, in the selector and in direct `PcmConverter` use. The selector uses 0 dB when the rate is kept, and when a DSD conversion's target equals the DSD bit rate over 16. The default covers a measured PCM square-wave fixture; it does not guarantee that every signal avoids clipping |
| `SelectedOutput.clippedSamples` | — | Counts integer-quantizer saturations of the selector's converter, or `null` when it has none. It is also `null`, converter or not, for exclusive AAudio on a bound route and for direct output to a DAC declared independent in a `DownstreamBinding` |
| `StreamRequest.ratePolicy` for `DacSession.negotiate` | `RatePolicy.EXACT` | Direct calls stay exact; only the selector converts |

The selector's rate order, unchanged: the requested rate, then the highest lower rate that divides it, then the highest lower rate of the same 44.1 kHz or 48 kHz family, then the highest other lower rate, then the lowest higher rate. Within a rate, a depth at least as wide as the source comes first. On a DAC that lacks the source rate, 192 kHz goes to 96 kHz and 176.4 kHz to 88.2 kHz when the DAC has them. An 88.2 kHz source on a DAC with only 44.1 and 176.4 kHz gets 44.1 kHz, which halves the bandwidth.

## Keeping exact output only

An app that wants the selector to play only what the DAC runs as it is:

<!-- uncompiled -->

```kotlin
val exactOnly = OutputPolicy(
    allowConversion = false,
    allowWidening = false,
    dopSupport = DopSupport.UNCONFIRMED,   // CONFIRMED only with real knowledge of the final decoder
)
```

With both flags off, a request the DAC cannot run exactly is passed to the next output, or fails with `UNSUPPORTED` when it is forced. For direct `PcmConverter` or `ConvertingOutput` use, pass `ConversionOptions(headroomDb = 0.0)` to keep unity gain; in Rust, set `ConvertOptions::headroom_db` to `0.0`.
