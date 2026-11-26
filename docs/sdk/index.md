# usbdac for Android

usbdac plays audio from an Android app to a USB DAC. It comes as an Android library (an AAR, package `dev.usbdac`) with three ways to get audio out:

- **Direct USB.** The library takes the DAC away from Android's audio stack and drives it itself: it sets the DAC's clock, sends the samples in the DAC's own format over isochronous USB transfers, paces them with the DAC's feedback endpoint, and reports whether the output is bit-perfect. It also controls the DAC's own hardware volume. The USB Audio Class driver is written in Rust and sits behind a small JNI layer (`libusbdac.so`).
- **Android-managed.** An `AudioTrack` at the source's own rate and bit depth, through Android's audio stack, behind the same `PcmOutput` interface. On Android 14 and later the library asks the platform for its bit-perfect mixer behavior for a USB device. This is the route for apps that must not take the DAC away from the rest of the system.
- **AAudio exclusive.** An AAudio stream in exclusive mode at the source's own rate, channel count and sample format, behind the same interface. This is the route for players with an internal DAC: it asks the platform to bypass the app mixer, and reports exactly what was granted. It is never reported bit-perfect-verified, because an app cannot see past AAudio.

## Status

This is version 0.1.

- The USB driver is the one that runs inside the Flick music player. It has been used with real DACs there.
- Everything around it is new: the push-model stream API, the event thread, the hardware-volume handle, the JNI layer, this Kotlin SDK, the managed transport and the AAudio transport. It compiles, the pure logic has JVM unit tests, and the build produces the AAR and the sample app. **None of it has been run on a phone or a DAC yet.** Expect to find problems on hardware, and report them with the logcat of the tag `usbdac`.
- The workflows in `.github/workflows` have never run on GitHub.

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

## What is in the SDK

| Type | What it is for |
|---|---|
| `UsbDac` | Finds attached DACs, asks for USB permission, watches hot-plugging, opens a `DacSession` |
| `DacSession` | An open DAC: capabilities, negotiation, `DacStream`, `DacVolume`, state and events |
| `DacStream` | The direct output: `write`, `play`, `pause`, `flush`, `drain`, position, latency, statistics, bit-perfect state |
| `DacVolume` | The hardware volume: level in dB or on a 0..1 curve, steps, mute, hand-back |
| `PcmOutput` | The interface `DacStream`, `AndroidManagedOutput` and `AAudioExclusiveOutput` share |
| `AndroidManagedOutput` | The managed transport |
| `AAudioExclusiveOutput` | The AAudio exclusive transport: `AAudioConfig` options, the `AAudioGrant` the platform gave, `AAudioEvent`s |
| `DacConfig` | Every tunable of a session: buffer depths, clock policy, DSD options, quirks, hand-back |
| `dev.usbdac.controls` | `DacVolumeKeys` (volume keys), `DacVolumeProvider` (system volume panel), `DacIdleRelease` (close the stream after a long pause) |

The API reference is generated from the KDoc. The Rust API, the JNI contract and the sample layouts are in [the native API document](native-api.html).

## Requirements

- Android 8.0 (API 26) or later. The library is built with Kotlin 2.4 at language level 2.2, and its only dependency is `kotlinx-coroutines-core`.
- Direct USB needs USB host support on the device and a DAC that implements the USB Audio Class 1.0 or 2.0 (not 3.0). Playback only.
- `libusbdac.so` is built for arm64-v8a, armeabi-v7a, x86_64 and x86 (about 1.2 MB for arm64-v8a). The arm64-v8a build links only against the system's `liblog`, `libdl` and `libc`, and its segments are aligned for 16 KB pages.
- One open `DacSession` per process in this version.

## The guide

1. [Getting started](getting-started.md): the dependency, the manifest, permission, open, negotiate, the write loop, pause and seek.
2. [Controls and configuration](controls.md): every option and control, with its default, its effect and its effect on bit-perfect output.
3. [Bit-perfect output](bit-perfect.md): what the flag means, when it turns false, DSD, MQA.
4. [Transports](transports.md): direct USB against the Android-managed output.
5. [Media3](media3.md): how to put a `PcmOutput` behind an `AudioSink`. Guidance only.
6. [Troubleshooting](troubleshooting.md): error codes and a checklist for "nothing plays".

The sample app in `android/sample` is the worked example for all of it. The code in this guide is taken from it, except where a block says it is not.
