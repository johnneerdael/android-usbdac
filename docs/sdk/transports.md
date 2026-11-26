# Transports

An app can send audio to a DAC in two ways. Both sit behind `PcmOutput`:

```
PcmOutput  write, writableBytes, play, pause, flush, drain, isDrained,
           playedFrames, latencyFrames, bitPerfect, close
 |- DacStream             direct USB, from DacSession.openStream
 `- AndroidManagedOutput  AudioTrack, from AndroidManagedOutput.open
```

A player written against `PcmOutput` runs on either. What differs is who owns the DAC, who does the volume, and what can be promised about the samples.

## Direct USB: `DacStream`

The library opens the USB device, claims the audio interfaces away from Android's USB audio driver, programs the DAC's clock, and sends isochronous USB transfers itself. Android's mixer, resampler, volume and effects are not in the path.

- **Exclusive.** While a `DacSession` is open the DAC belongs to your app. System sounds, notifications and other apps cannot use it (they fall back to the phone's speaker or another route), and the system volume does not reach it. `DacSession.close()` hands the DAC back to Android and asks it to take its USB audio driver back.
- **Verified.** The library knows the format on the wire, whether the DAC confirmed its clock, whether any frame was lost, and what is in the packets. It reports `BitPerfectState.Verified` or the reason it is not.
- **Hardware volume.** Volume is the DAC's own volume control (`DacVolume`), which scales the audio in the DAC and leaves the stream bit-perfect.
- **DSD.** DoP and native DSD, when the DAC offers them.
- **Costs.** A permission dialog per device. `UsbDac.open`, `negotiate` and `openStream` take 100 ms or more each. One session per process. It depends on the DAC following the USB Audio Class; a DAC with quirks may need an entry in `DacConfig.quirks`. A foreground service is the app's business if it plays in the background.
- **Failure modes.** A DAC that Android will not let go of (`CLAIM`), a DAC with no feedback endpoint on an asynchronous endpoint (`IMPLICIT_FEEDBACK_UNSUPPORTED`), a DAC whose clock readback does not work (`CLOCK_UNVERIFIED`). See [troubleshooting](troubleshooting.md).

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

## Choosing

| If | Use |
|---|---|
| The device has an internal DAC (a digital audio player) | Managed |
| The user must keep hearing system sounds, or other apps must keep the DAC | Managed |
| Bit-perfect output must be verifiable, or a bug report needs to say why it was not | Direct |
| DSD over DoP or native DSD | Direct |
| The DAC's hardware volume should be the only volume | Direct |
| Android 14 or later, a USB DAC, PCM, and a platform guarantee is enough | Either; managed is simpler |
| The DAC cannot be driven directly (it fails `CLAIM`, or is not a USB Audio Class device) | Managed |

An app that offers both can pick at run time: direct while a `DacSession` opens, managed otherwise. Close the `DacSession` first when you switch from direct to managed: while the session is open Android has no audio route to the DAC.

## Roadmap

AAudio exclusive mode, which could give a lower-level route on devices with internal DACs, is not implemented. It is listed as a possible third transport and nothing in this version uses it.
