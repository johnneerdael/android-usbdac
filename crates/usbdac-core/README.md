# usbdac-core

Drive a USB DAC directly from a Rust audio application on Android or Linux.
`usbdac-core` handles USB Audio Class 1.0 and 2.0 playback over usbfs so your
app can feed PCM, DoP or native DSD to the DAC and control its hardware volume.
The stream reports whether it can verify bit-perfect output and identifies
what prevented verification.

## Add it to your project

Find published versions on [crates.io](https://crates.io/crates/usbdac-core).

```sh
cargo add usbdac-core@0.1.1
```

1. Open the USB device, claim its AudioControl and playback AudioStreaming
   interfaces, and pass the device file descriptor and claimed interface numbers
   to `Dac::open`. On Android, request USB permission before opening the device.
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
