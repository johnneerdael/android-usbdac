<p align="center"><img src="https://raw.githubusercontent.com/johnneerdael/usbdac/main/branding/marketing/usbdac-readme-dark.png" alt="usbdac. Direct USB audio. Clear evidence." width="100%"></p>

# usbdac-core

[Rust guide](https://johnneerdael.github.io/usbdac/guide-rust.html) · [API on docs.rs](https://docs.rs/usbdac-core) · [Evidence and status](https://johnneerdael.github.io/usbdac/evidence.html) · [Source](https://github.com/johnneerdael/usbdac)

Drive a USB DAC directly from a Rust audio application on Android or Linux.
`usbdac-core` handles USB Audio Class 1.0, 2.0 and 3.0 playback over usbfs so your
app can feed PCM, DoP or native DSD to the DAC and control its hardware volume.
The stream reports whether it can verify bit-perfect output and identifies
what prevented verification.

The driver supports implicit feedback on compatible UAC2 and UAC3 devices. It uses
whole-frame counts from a matching IN stream to pace playback. Explicit
feedback selects the newest valid report in each batch, checks packet capacity
and gives the device a bounded recovery window when reports stop. Read
`StreamFormat::pacing` for the selected source and
`DacStream::stats().feedback_locked` for its live state.

## Add it to your project

Find published versions on [crates.io](https://crates.io/crates/usbdac-core).

```sh
cargo add usbdac-core@0.1.15
```

1. Open the USB device, claim its AudioControl and playback AudioStreaming
   interfaces, and pass the device file descriptor and claimed interface numbers
   to `Dac::open`. A raw Android host also requests USB permission and claims
   any compatible feedback interface it needs. See the native integration guide
   for the claim sequence.
2. Negotiate a stream format, open the stream and write samples in the returned
   wire layout from your playback thread.
3. Query `DacStream::is_bit_perfect()` and `bit_perfect_blocker()` to report the
   stream's status in your app.

Your app supplies the decoded audio. Optional PCM conversion helpers handle
sample rate, bit depth and mono-to-stereo when your source format needs to change.
The stream itself never converts: `StreamRequest` defaults to `RatePolicy::Exact`,
and `RatePolicy::NearestSupported` returns a different rate for the host to
resample to.

`Dac::open` needs a Linux or Android usbfs file descriptor. On macOS,
`Dac::open_darwin` opens a libusb attachment from `darwin_discovery`, as the
Apple Silicon validator does; no macOS hardware result is recorded yet. Other
hosts compile and run the logic tests, and opening a DAC there returns
`Error::PlatformUnsupported`.
Each physical attachment has one owner; distinct attachments can be owned independently.
Direct USB PCM and native DSD have limited device-specific physical evidence. Test your target devices
and formats before relying on it in production.

- [Rust API reference](https://docs.rs/usbdac-core)
- [Native integration guide and wire layouts](https://johnneerdael.github.io/usbdac/native-api.html)
- [Android SDK quickstart for Kotlin and Gradle](https://johnneerdael.github.io/usbdac/getting-started.html)

Rust applications can subscribe to `HotplugHub` attachment/permission changes;
Linux hosts can retain `LinuxHotplugMonitor` for native libusb hotplug events
(with a documented polling fallback). Android hosts bridge `UsbManager`
broadcasts and permission results; discovery never opens or claims a device.
Each physical attachment receives a new ID, including when its path is reused.

For UI/async callers, `dac.volume().unwrap().nonblocking()?` provides a bounded
FIFO control worker. Its requests return futures with polling/blocking result
alternatives, preserving verified hardware writes and startup/hand-back safety.
Existing blocking calls remain available. Accepted writes are not cancelled by
dropping their future.

`fake::FakeDac` implements the same public `PlaybackDevice`/`PlaybackStream`
application boundary as real playback. It supports deterministic signed PCM, unsigned 8-bit and float32 queue
progress, pause/drain/flush, injected loss, and unplug without hardware; it is
not a protocol emulator or a bit-perfect hardware verification claim. See the
[Rust SDK guide](https://johnneerdael.github.io/usbdac/guide-rust.html) for
examples and platform/permission ownership.
