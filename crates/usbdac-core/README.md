# usbdac-core

Drive a USB DAC directly from a Rust audio application on Android or Linux.
`usbdac-core` handles USB Audio Class 1.0 and 2.0 playback over usbfs so your
app can feed PCM, DoP or native DSD to the DAC and control its hardware volume.
The stream reports whether it can verify bit-perfect output and identifies
what prevented verification.

Version 0.1.1 supports implicit feedback on compatible UAC2 devices. It uses
whole-frame counts from a matching IN stream to pace playback. Explicit
feedback selects the newest valid report in each batch, checks packet capacity
and gives the device a bounded recovery window when reports stop. Read
`StreamFormat::pacing` for the selected source and
`DacStream::stats().feedback_locked` for its live state.

## Add it to your project

Find published versions on [crates.io](https://crates.io/crates/usbdac-core).

```sh
cargo add usbdac-core@0.1.6
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
The direct USB stream supports one open DAC per process in 0.1. Direct USB
PCM has played on one phone and DAC combination. Test your target devices
and formats before relying on it in production.

- [Rust API reference](https://docs.rs/usbdac-core)
- [Native integration guide and wire layouts](https://johnneerdael.github.io/android-usbdac/native-api.html)
- [Android SDK quickstart for Kotlin and Gradle](https://johnneerdael.github.io/android-usbdac/getting-started.html)

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
application boundary as real playback. It supports deterministic PCM queue
progress, pause/drain/flush, injected loss, and unplug without hardware; it is
not a protocol emulator or a bit-perfect hardware verification claim. See the
[Rust SDK guide](https://johnneerdael.github.io/android-usbdac/sdk/guide-rust/) for
examples and platform/permission ownership.
