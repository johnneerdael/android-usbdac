# Upgrading from 0.1.10 to 0.1.14

What an Android app built against 0.1.10 has to check when upgrading. Change both coordinates, `dev.usbdac:usbdac` and `dev.usbdac:usbdac-media3`, to `0.1.14` together. See the 0.1.14 changes immediately below; the 0.1.11 history is retained afterward. None of the changes below has a new hardware result; [evidence](evidence.md#hardware-status) lists what has run on a DAC.

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

In 0.1.10 the selector could validate and unpack an explicit DoP request (`StreamRequest.dop`) and convert it to PCM whenever `allowConversion` was on and the decoder was not confirmed. In 0.1.11 that also needs `OutputPolicy.allowDopPcmConversion = true`, which is `false` by default. Without it, a DoP request with `dopSupport = DopSupport.UNCONFIRMED` gets no direct output, and the Android outputs reject it with `REQUEST_NOT_SUPPORTED`. Native DSD input (`StreamRequest.dsdNative`) is not affected: its PCM fallback still needs only `allowConversion`.

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
