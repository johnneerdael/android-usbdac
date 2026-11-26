# usbdac for Android

usbdac gives any Android app bit-perfect playback to a USB DAC, with the DAC's own volume control, DSD, and an indicator that tells the truth about whether the sound is untouched. It started from the USB driver of the Flick music player and has been reworked well beyond it; [what usbdac improves](improvements.md) lists every change, each with a plain-language explanation.

usbdac plays audio from an Android app to a USB DAC. It comes as an Android library (an AAR, package `dev.usbdac`) with three ways to get audio out, and a selector that picks one for the app (see [automatic output](automatic-output.md)):

- **Direct USB.** The library takes the DAC away from Android's audio stack and drives it itself: it sets the DAC's clock, sends the samples in the DAC's own format over isochronous USB transfers, paces them with the DAC's feedback endpoint, and reports whether the output is bit-perfect. It also controls the DAC's own hardware volume. The USB Audio Class driver is written in Rust and sits behind a small JNI layer (`libusbdac.so`).
- **Android-managed.** An `AudioTrack` at the source's own rate and bit depth, through Android's audio stack, behind the same `PcmOutput` interface. On Android 14 and later the library asks the platform for its bit-perfect mixer behavior for a USB device. This is the route for apps that must not take the DAC away from the rest of the system.
- **AAudio exclusive.** An AAudio stream in exclusive mode at the source's own rate, channel count and sample format, behind the same interface. This is the route for players with an internal DAC: it asks the platform to bypass the app mixer, and reports exactly what was granted. It is never reported bit-perfect-verified, because an app cannot see past AAudio.

## Status

This is a 0.1 pre-release.

- **Automated checks pass.** The driver has several hundred tests that run without a DAC, the Kotlin layer has its own, and every push builds the library and the sample app for four processor types.
- **Nothing has been confirmed on hardware yet.** The driver grew out of code that has played to real DACs inside Flick, but it has changed a great deal since, and the public API, the Kotlin SDK and the two non-USB outputs are new. A build of Flick that runs on this library exists for hardware testing. Expect to find problems, and report them with the logcat of the tag `usbdac`.

## Which transport to use

| | Direct USB (`DacStream`) | Android-managed (`AndroidManagedOutput`) | AAudio exclusive (`AAudioExclusiveOutput`) |
|---|---|---|---|
| Where the audio goes | Straight to the DAC over usbfs | Through the platform mixer to whatever Android routes to | An exclusive AAudio stream to the chosen output, bypassing the app mixer when the platform grants it |
| Bit-perfect | Verified by the library, with a reason when it is not | Not verifiable. Android 14+ with a USB device: requested from the platform. Otherwise unknown | Never verified; `NotVerified` with the grant's exact reason |
| Volume | The DAC's hardware volume (`DacVolume`). The system volume has no effect | The system volume | The system volume |
| DSD | DoP and native DSD | PCM only | PCM only |
| Other apps and system sounds | The DAC is not available to them while the session is open | Unchanged. They share the output | An exclusive stream keeps the device; others may be refused or silenced |
| Needs | USB host mode, USB permission per device, a DAC that follows the USB Audio Class | Any Android 8.0+ device. 24-bit and 32-bit PCM need Android 12 | Any Android 8.0+ device whose route grants exclusive mode. 24-bit and 32-bit integer PCM need Android 12 |
| Buffering | A ring of 250 ms by default, flushed on seek | An `AudioTrack` buffer of 250 ms | A ring of 250 ms by default, flushed on seek, in front of AAudio's own few milliseconds |

Choose **direct USB** when bit-perfect output, DSD or the DAC's hardware volume matters and the app can own the DAC while it plays. Choose **AAudio exclusive** for a device with an internal DAC when bypassing the app mixer matters. Choose **managed** for a DAC that other apps must keep using, or as the fallback when a DAC cannot be driven directly or the platform refuses an exclusive stream. An app can offer all three through the shared `PcmOutput` interface; [transports](transports.md) has the details.

Most apps do not need to choose. `OutputSelector` looks at the attached DAC, the USB permission, where Android sends the sound and what the platform grants, and returns one ready `PcmOutput` with the reason and the guarantee that applies. It tries direct USB, then Android's bit-perfect mixer, then an exclusive AAudio stream, then the standard output; [automatic output](automatic-output.md) gives the exact order and what each guarantee means.

## What is in the SDK

| Type | What it is for |
|---|---|
| `UsbDac` | Finds attached DACs, asks for USB permission, watches hot-plugging, opens a `DacSession` |
| `DacSession` | An open DAC: capabilities, negotiation, `DacStream`, `DacVolume`, state and events |
| `DacStream` | The direct output: `write`, `play`, `pause`, `flush`, `drain`, position, latency, statistics, bit-perfect state |
| `DacVolume` | The hardware volume: level in dB or on a 0..1 curve, steps, mute, hand-back |
| `OutputSelector`, `OutputPolicy`, `OutputDecision`, `SelectedOutput` | Automatic output: looks at the device, chooses one of the outputs below, and returns it ready with the reason, the guarantee and the alternatives it rejected. `changes` says when to look again |
| `PcmOutput` | The interface `DacStream`, `AndroidManagedOutput` and `AAudioExclusiveOutput` share |
| `AndroidManagedOutput` | The managed transport |
| `AAudioExclusiveOutput` | The AAudio exclusive transport: `AAudioConfig` options, the `AAudioGrant` the platform gave, `AAudioEvent`s |
| `ConversionPlan`, `PcmConverter`, `ConvertingOutput` | Opt-in conversion in front of any output: a rate the DAC refuses, a depth it lacks, mono to stereo. Classified `NONE`, `BIT_EXACT`, `LOSSLESS` or `LOSSY`; not bit-perfect unless the first two |
| `DacConfig` | Every tunable of a session: buffer depths, clock policy, DSD options, quirks, hand-back |
| `dev.usbdac.controls` | `DacVolumeKeys` (volume keys), `DacVolumeProvider` (system volume panel), `DacIdleRelease` (close the stream after a long pause) |

The API reference is generated from the KDoc. The Rust API, the JNI contract and the sample layouts are in [the native API document](native-api.html).

## Requirements

- Android 8.0 (API 26) or later. The library is built with Kotlin 2.4 at language level 2.2, and its only dependency is `kotlinx-coroutines-core`.
- Direct USB needs USB host support on the device and a DAC that implements the USB Audio Class 1.0 or 2.0 (not 3.0). Playback only.
- `libusbdac.so` is built for arm64-v8a, armeabi-v7a, x86_64 and x86 (about 1.9 MB uncompressed for arm64-v8a). The arm64-v8a build links only against the system's `liblog`, `libdl`, `libm` and `libc` (AAudio is opened at run time with `dlopen`, so there is no link-time dependency on `libaaudio`), and its segments are aligned for 16 KB pages.
- One open `DacSession` per process in this version.

## The guide

Start with [what usbdac improves](improvements.md) for the overview of every change from the driver it started from, in technical and in plain terms. Then:

1. [Getting started](getting-started.md): the dependency, the manifest, and the quickest way to audio; then permission, open, negotiate, the write loop, pause and seek by hand.
2. [Automatic output](automatic-output.md): what the selector detects, the order it chooses in, what each guarantee means, the one setting that is the listener's, and when to look again.
3. [Controls and configuration](controls.md): every option and control, with its default, its effect and its effect on bit-perfect output.
4. [Bit-perfect output](bit-perfect.md): what the flag means, when it turns false, DSD, MQA.
5. [Transports](transports.md): direct USB, the Android-managed output and exclusive AAudio, side by side.
6. [Media3](media3.md): how to put a `PcmOutput` behind an `AudioSink`. Guidance only.
7. [Troubleshooting](troubleshooting.md): error codes and a checklist for "nothing plays".

The sample app in `android/sample` is the worked example for all of it. The code in this guide is taken from it, except where a block says it is not.
