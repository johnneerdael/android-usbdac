# Transports

An app can send audio to a DAC in three ways. All three sit behind `PcmOutput`:

```
PcmOutput  write, writableBytes, play, pause, flush, drain, isDrained,
           playedFrames, latencyFrames, bitPerfect, close
 |- DacStream               direct USB, from DacSession.openStream
 |- AndroidManagedOutput    AudioTrack, from AndroidManagedOutput.open
 `- AAudioExclusiveOutput   AAudio exclusive mode, from AAudioExclusiveOutput.open
```

A player written against `PcmOutput` runs on any of them. What differs is who owns the DAC, who does the volume, and what can be promised about the samples.

An app does not have to pick: [`OutputSelector`](automatic-output.md) detects the situation and returns one of these ready, as `ANDROID_BIT_PERFECT` (the managed output with the Android 14 mixer request), `ANDROID_STANDARD` (the managed output without it), `AAUDIO_EXCLUSIVE` or `DIRECT_USB`. This page is what the outputs are, whichever way they are opened.

## Direct USB: `DacStream`

The library opens the USB device, claims the audio interfaces away from Android's USB audio driver, programs the DAC's clock, and sends isochronous USB transfers itself. Android's mixer, resampler, volume and effects are not in the path.

- **Exclusive.** While a `DacSession` is open the DAC belongs to your app. System sounds, notifications and other apps cannot use it (they fall back to the phone's speaker or another route), and the system volume does not reach it. `DacSession.close()` hands the DAC back to Android and asks it to take its USB audio driver back.
- **Verified.** The library knows the format on the wire, whether the DAC confirmed its clock, whether any frame was lost, and what is in the packets. It reports `BitPerfectState.Verified` or the reason it is not.
- **Feedback.** A dedicated endpoint can report the playback rate. Version
  0.1.1 validates each report and bounds recovery if reports stop. For
  compatible UAC2 implicit feedback, packet frame counts from an IN stream
  on the same clock and service interval pace playback. The SDK handles
  eligible IN interface claims and releases them on session close. Inspect
  `format.pacing` and `stats().feedbackLocked`.
  [Clock and pacing](decisions-clock.md#feedback)
- **Hardware volume.** Volume is the DAC's own volume control (`DacVolume`), which scales the audio in the DAC and leaves the stream bit-perfect.
- **DSD.** DoP and native DSD, when the DAC offers them.
- **Costs.** A permission dialog per device. `UsbDac.open`, `negotiate` and `openStream` take 100 ms or more each. One session per process. It depends on the DAC following the USB Audio Class; a DAC with quirks may need an entry in `DacConfig.quirks`. A foreground service is the app's business if it plays in the background.
- **Failure modes.** A DAC that Android will not let go of (`CLAIM`), an asynchronous route without usable feedback (`IMPLICIT_FEEDBACK_UNSUPPORTED`), a DAC whose clock readback does not work (`CLOCK_UNVERIFIED`). See [troubleshooting](troubleshooting.md).

## Android-managed: `AndroidManagedOutput`

An `AudioTrack` created at the source's own sample rate and bit depth, optionally routed to a chosen output device.

```kotlin
fun openManaged(context: Context, sampleRate: Int, bitDepth: Int): PcmOutput {
    val usbRoute = AndroidManagedOutput.usbOutputDevices(context).firstOrNull() // null: the default route
    return AndroidManagedOutput.open(context, StreamRequest.pcm(sampleRate, channels = 2, bitDepth = bitDepth), usbRoute)
}
```

What you write is PCM in the encoding of the request, little-endian and interleaved:

| `bitDepth` | Bytes per sample | Needs |
|---|---|---|
| 16 | signed 16-bit | Android 8.0 |
| 24 | packed signed 24-bit, 3 bytes | Android 12 (API 31) |
| 32 | signed 32-bit | Android 12 (API 31) |

DSD, DoP and other encodings fail with `UNSUPPORTED`: the platform mixer would destroy them. `StreamRequest.ratePolicy` is ignored; the track is created at the requested rate and Android decides what to do if the route cannot run it. There is no software gain; the system volume applies.

### What is and is not guaranteed

- The library creates the track at the exact rate, depth and channel count you asked for. It never converts samples.
- **Android 14 (API 34) and later, with a USB `AudioDeviceInfo`:** `open` looks for the bit-perfect mixer attributes the platform offers for that device in exactly your format and, if there are any, sets them as the preferred mixer attributes for `USAGE_MEDIA` (`AudioManager.setPreferredMixerAttributes`, which needs `MODIFY_AUDIO_SETTINGS`; the library's manifest declares it). If the platform accepts and keeps the request, `bitPerfect` is `RequestedViaPlatform`. The request is cleared again on `close()`.
- That is a request the platform accepted. **The library cannot verify the result.** What the platform does with other apps' sounds, with volume, and what the DAC does, is the platform's and the DAC's.
- **Otherwise** (an older Android, no device, a device that is not USB such as a DAP's internal DAC, a format the platform offers no bit-perfect mixer for, or a refused request): `bitPerfect` is `Unverifiable` with the reason. Whether the samples are mixed, scaled or resampled depends on the device and the route.
- Android reports bit-perfect mixer attributes for USB devices only. For an internal DAC there is nothing to request.
- `playedFrames` and `latencyFrames` come from `AudioTrack.getPlaybackHeadPosition`. They are best-effort, and `latencyFrames` is only the audio queued in the track: Android's and the DAC's own latency are not included.
- The behaviour of `flush()` and `drain()` follows the platform's `AudioTrack`, and has not been checked on devices.

### Writing and controls

`write` is nonblocking and takes whole frames up to the room in the track's 250 ms buffer. Playback starts by itself once about 100 ms is buffered. `pause()` and `play()` pause and resume the track; `flush()` pauses, flushes and resets the position; `drain()` lets the buffered audio play out, and `isDrained` becomes `true` when the head position has reached the last frame written. After `close()` the track is released.

## AAudio exclusive: `AAudioExclusiveOutput`

An AAudio stream in exclusive mode, at the source's own rate, channel count and sample format. It is the route for a digital audio player with an internal DAC: exclusive mode asks the platform to bypass the app mixer, which is what the managed `AudioTrack` path cannot do. It needs no `DacSession` and no USB.

```kotlin
fun openAAudio(context: Context, sampleRate: Int, bitDepth: Int): AAudioExclusiveOutput =
    AAudioExclusiveOutput.open(
        context,
        StreamRequest.pcm(sampleRate, channels = 2, bitDepth = bitDepth),
        device = null, // the platform's default route; or one of AAudioExclusiveOutput.outputDevices(context)
        config = AAudioConfig(requireExclusive = true),
    )
```

What you write is PCM in the encoding of the request, little-endian and interleaved, and **nothing is converted**:

| `bitDepth` | Bytes per sample | Needs |
|---|---|---|
| 16 | signed 16-bit | Android 8.0 |
| 24 | packed signed 24-bit, 3 bytes | Android 12 (API 31) |
| 32 | signed 32-bit | Android 12 (API 31) |
| 32 with `AAudioConfig(floatSamples = true)` | 32-bit float | Android 8.0 |

Below the release a format needs, `open` fails with `UNSUPPORTED` and a message naming the API levels. DSD and DoP fail with `UNSUPPORTED` as well. `StreamRequest.ratePolicy` is ignored: the stream runs at exactly the rate you ask for, or the open fails. [Controls](controls.md#the-aaudio-output-aaudioconfig) lists every option.

### The grant

After opening, the platform reports what it actually granted, and the library exposes it as `output.grant`: the sharing mode, rate, channel count and format, the burst and buffer sizes in frames, the device id, whether the memory-mapped path is in use, and on Android 14 and later what the hardware itself runs at.

```kotlin
fun describeGrant(grant: AAudioGrant): String = buildString {
    appendLine("${grant.sharingMode} at ${grant.sampleRate} Hz, ${grant.channels} channels, ${grant.format}")
    appendLine("burst ${grant.framesPerBurst} frames, buffer ${grant.bufferSizeFrames}/${grant.bufferCapacityFrames} frames")
    appendLine("device ${grant.deviceId}, MMAP ${grant.usesMmap}, hardware ${grant.hardware ?: "not reported"}")
    append(grant.reason)
}
```

`AAudioConfig.requireExclusive` (on by default) makes `open` fail with `EXCLUSIVE_UNAVAILABLE` when the platform grants only a shared stream: AAudio falls back to shared silently when exclusive is not available, which would put the system mixer back in the path. Turn it off to accept a shared stream; `grant.sharingMode` and `bitPerfect` then say so. A stream that came back at another rate, channel count or format than the one you write is never started (`UNSUPPORTED`), because the bytes would play at the wrong speed or as noise.

### What AAudio guarantees

- The library asks for exclusive mode, the exact rate, channel count and format, direction output, usage media and content type music. It never converts, resamples, dithers or scales.
- **`bitPerfect` is never `Verified`.** An app cannot see past AAudio. It is `NotVerified(blocker = null, reason)` and the reason is as precise as the grant allows: exclusive at the requested rate and format (the app mixer is bypassed, but the device's audio HAL is not verifiable from an app); shared (the system mixer is in the path); or, on Android 14 and later, a hardware rate or format that differs from the stream's (AAudio converts before the device). See [bit-perfect output](bit-perfect.md#the-aaudio-output).
- Exclusive streams are available on some devices and routes and not on others. Whether yours grants one is what `requireExclusive` and `grant` tell you; the library cannot make a platform grant it.
- `playedFrames` is the music the device has presented since the start or the last flush, measured from AAudio's timestamps and counters. It lags the audible position by at most the device's own buffer and never leads it. `latencyFrames` is the ring plus what the device holds; the platform's and the DAC's own latency beyond that are not included.
- A stream that was disconnected (the device unplugged, or Android re-routed the audio) fails: `AAudioEvent.Disconnected` is sent, `state` becomes `FAILED` and writes throw `DEVICE_LOST`. The output cannot be revived: close it and open another.

```kotlin
fun watchRoute(scope: CoroutineScope, output: AAudioExclusiveOutput, onLost: (String) -> Unit): Job =
    output.events
        .onEach { event -> if (event is AAudioEvent.Disconnected) onLost(event.detail) }
        .launchIn(scope)
```

`output.xrunCount` is AAudio's own count of device-side underruns, `output.underrunCount` how often the output's own buffer ran dry because the host wrote too slowly; both are also announced as `AAudioEvent.Xrun` and `AAudioEvent.Underrun` (polled a few times a second from the calls you make).

### Writing and controls

AAudio's own buffer is a few milliseconds deep, which a writer thread that sleeps for a few milliseconds cannot keep full. The output therefore puts a ring of `bufferTargetMs` (250 ms) in front of it and feeds the stream from AAudio's real-time data callback: `write` is nonblocking and takes whole frames up to that depth, `writableBytes` is the room, and `flush()` drops the ring at once, as for a direct stream. Playback starts by itself once `startThresholdMs` (100 ms) is buffered. `pause()` plays silence and keeps the stream open and exclusive (close it instead for a long pause); `play()` continues; `drain()` plays the buffered tail out and `isDrained` turns `true` when the device has presented it. An underrun waits for the start threshold again instead of stuttering. The usual rules of [threading](getting-started.md#10-threading) apply: one writer, `open` and `close` off the main thread (the close takes up to about half a second).

**Not verified on hardware.** The logic is covered by host tests with a simulated device and by JVM tests with a fake native layer. The AAudio calls themselves have never opened a stream.

## Converting in front of any output: `ConvertingOutput`

Not a transport, but a wrapper that goes in front of one: `ConvertingOutput(target, converter)` is a `PcmOutput` that converts what it is given (a new sample rate, more or fewer bits, mono to stereo) before it reaches `target`, which can be any of the three. It is how an app plays a source the DAC does not run as it is. It is opt-in, the direct USB path never converts by itself, and **a plan that is not `NONE` or `BIT_EXACT` is not bit-perfect**. See [getting started](getting-started.md#when-the-dac-refuses-what-you-have) for the flow with `RatePolicy.NEAREST_SUPPORTED`, [bit-perfect output](bit-perfect.md#conversion-is-not-bit-perfect) for what it reports and [controls](controls.md#conversion-conversionoptions) for the options.

## Choosing

| If | Use |
|---|---|
| The device has an internal DAC (a digital audio player) and an exclusive stream matters | AAudio exclusive, with `requireExclusive`; managed when it is refused |
| The device has an internal DAC (a digital audio player) | Managed |
| The user must keep hearing system sounds, or other apps must keep the DAC | Managed |
| Bit-perfect output must be verifiable, or a bug report needs to say why it was not | Direct |
| DSD over DoP or native DSD | Direct |
| The DAC's hardware volume should be the only volume | Direct |
| Android 14 or later, a USB DAC, PCM, and a platform guarantee is enough | Either; managed is simpler |
| The DAC cannot be driven directly (it fails `CLAIM`, or is not a USB Audio Class device) | Managed |
| The DAC refuses the source's rate or depth and the app accepts that conversion is not bit-perfect | Any of them, behind a `ConvertingOutput` |

An app can offer all three and pick at run time: direct while a `DacSession` opens, AAudio exclusive or managed otherwise. Close the `DacSession` first when you switch from direct to managed or AAudio: while the session is open Android has no audio route to the DAC.

Or leave the choice to the library. `OutputSelector` applies this table in a fixed order (direct USB, Android's bit-perfect mixer, exclusive AAudio, standard), skips what the device or the listener rules out, and reports the guarantee that applies and why the better outputs were passed over. The one thing it cannot decide for the app is whether the app may take the DAC away from other apps: that is `OutputPolicy.allowDirectUsb`, and a player should offer it to the listener. See [automatic output](automatic-output.md).
