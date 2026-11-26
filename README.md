# usbdac

Add direct USB DAC playback to an Android audio app. usbdac sends decoded PCM,
DoP or native DSD to the DAC through USB, bypassing Android's mixer on the
direct path. Your app can control the DAC's hardware volume and show whether
the stream is bit-perfect, with the reason when verification fails.

| Integrating with | Start here |
|---|---|
| Kotlin on Android 8.0+ | Add `dev.usbdac:usbdac` from GitHub Packages. The AAR bundles the native libraries for all four Android ABIs. [Android quickstart](https://johnneerdael.github.io/android-usbdac/getting-started.html) |
| Rust on Android or Linux | Add [`usbdac-core` from crates.io](https://crates.io/crates/usbdac-core). Your app handles USB access and passes an open file descriptor to the driver. [Rust API reference](https://docs.rs/usbdac-core) |

Use `OutputSelector` when your Android player needs one PCM playback integration
that can use direct USB, Android-managed audio or AAudio exclusive as the device
and permissions allow. [SDK guide](https://johnneerdael.github.io/android-usbdac/)

Download the AAR, native libraries and crate archives from
[GitHub Releases](https://github.com/johnneerdael/android-usbdac/releases).

## Where it came from, and what changed

usbdac started from the USB DAC driver of the [Flick](https://github.com/moss-apps/Flick)
music player. It is no longer that driver. The code was taken out of the app,
given a public API, and improved in ways Flick's own driver has not been.
[What usbdac improves](docs/sdk/improvements.md) explains each change twice:
technically, and in plain terms. In short:

| Area | What usbdac adds | In plain terms |
|---|---|---|
| Clock handling | Follows clock selectors and multipliers to the clock in use | No wrong pitch or silence on DACs with more than one clock |
| Transfer queue | Always 16 ms on the bus, never 256 packets | Avoids a stall seen on real hardware; controls feel equally quick on every DAC |
| Fault handling | Recovers from timed-out transfers and counts lost audio exactly | A brief USB hiccup no longer stops the music |
| Buffering | 250 ms reserve that shrinks while software volume is active | More protection against gaps without slower controls |
| Pause and seek | Pause holds the audio and sends silence; flush cancels what is on the bus | Pause stops at once, resume loses nothing, seeks are heard immediately |
| Position | Counted from what the DAC confirmed | Progress bar and lyrics match what you hear |
| Start-up safety | Volume lowered to at most -40 dB before the first sample; no volume control means no playback unless allowed | Plugging in a DAC cannot blast your ears |
| Volume | Useful slider range, latest-wins writes, works without a stream, key and system-panel helpers, original level handed back | The volume behaves the way phone volume does, and other apps get the DAC back as it was |
| Power | Stream released after a long pause | Less battery used while paused |
| Honesty | Bit-perfect flag with a named reason when false; chosen clock and pacing reported | An indicator that goes out when the sound is compromised, and says why |
| Outputs | Direct USB, Android-managed, and AAudio exclusive behind one interface | One integration serves phones with dongles and players with built-in DACs |
| Output choice | An `OutputSelector` that detects the DAC, permission, route and what the platform grants, and returns one ready output with the reason and the guarantee | The app asks for the best way to play and is told what it got and why |
| Conversion | Optional resampling, bit-depth and mono-to-stereo, always labelled | A file still plays on a limited DAC, and is plainly marked as not bit-perfect |
| Packaging | Rust core, Kotlin API, sample app, tests, published builds | A dependency and a few dozen lines instead of writing a USB driver |

Several of these came from studying how two other open-source players, Mornye
and BitChord, behave with the same DACs. No code from either was used.

**Status: 0.1.** Direct USB has played
on one phone and one DAC (OnePlus 15, Android 16, iBasso DC04U) through a build
of Flick that runs on this library. Other phones and DACs, DSD, the Kotlin SDK
and the two Android-managed outputs are not yet confirmed on hardware.

## Layout

```text
crates/usbdac-core/   the driver (Rust): no JNI, no application types
crates/usbdac-jni/    libusbdac.so, the JNI bindings
android/usbdac/       the Android library (AAR, Kotlin, package dev.usbdac)
android/sample/       a small app that exercises every feature
docs/sdk/             the SDK guide (published to the site above)
docs/API.md           the Rust API, JNI contract, JSON schemas, sample layouts
```

Besides direct USB, the library carries two outputs that go through Android:
an `AudioTrack`-based one that asks Android 14 and later for its bit-perfect
mode, and an AAudio exclusive-mode stream for devices with an internal DAC.

## What it does

- Formats: PCM 16/24/32-bit in the subslot size the DAC advertises, DoP
  (carrier up to what the DAC runs), native DSD (DSD_U8/U16/U32, byte order and
  bit order per device).
- Output: a ring buffer (250 ms target, 500 ms capacity by default) feeds eight
  isochronous transfers in flight, 16 ms queued on the bus at every service
  interval and for every encoding, never 256 packets; explicit feedback
  endpoints are polled asynchronously and steer the packet sizes.
- Flush: `flush()` drops the ring and cancels what is already on the bus (not
  counted as played, lost or an underrun), so a seek is heard at once.
- Reporting: played frames and latency counted on completed transfers, lost
  frames (timed-out and short transfers) latched, an audit of the wire payload,
  and `is_bit_perfect()` combining them with the clock verification, with
  `bit_perfect_blocker()` saying what stands in the way.
- Pacing: asynchronous endpoints without an explicit feedback endpoint (implicit
  feedback) are refused with `Error::ImplicitFeedbackUnsupported` unless
  `DacConfig::allow_unsynchronized_async` streams them at the nominal rate.
- Volume: the Feature Unit's volume and mute, with read-back verification, a dB
  and a 0..1 view, the level the DAC had before, and a hand-back on close. The
  volume is lowered to at most -40 dB when the DAC opens (never raising a
  channel, verified by reading it back), and a DAC without a usable hardware
  volume refuses to stream unless the host allows a fixed level.
- Opt-in conversion helpers (`usbdac_core::convert`): a polyphase windowed-sinc
  resampler, integer widening and narrowing with TPDF dither, float and mono-to-stereo,
  planned and classified (`None`, `BitExact`, `Lossless`, `Lossy`) so a host can play a
  rate the DAC refuses and say honestly that it is not bit-perfect. The direct path never
  calls them (`docs/API.md` section 14).
- Events through one library thread: state changes, underruns, lost frames,
  bit-perfect changes, volume changes, stale feedback, errors, device loss.

## Limits

- One open DAC per process in v0.1 (`Error::Busy` for a second one).
- Linux/Android usbfs only. On other hosts everything compiles and the logic
  tests run, but `Dac::open` returns `Error::PlatformUnsupported`.
- Playback only; UAC 1.0/2.0; no UAC 3.0.
- The host owns USB permission, `claimInterface` and the file descriptor.

## Build

```sh
# host: build and test everything that does not need a DAC
cargo test --workspace

# Android (needs the NDK; cargo-ndk sets the C toolchain libusb1-sys needs)
export ANDROID_NDK_HOME=$HOME/Library/Android/sdk/ndk/28.2.13676358
cargo ndk -t arm64-v8a check --workspace
cargo ndk -t arm64-v8a build -p usbdac-jni --release     # target/aarch64-linux-android/release/libusbdac.so
```

Plain `cargo check --target aarch64-linux-android` also works once the NDK's
clang is reachable, because libusb is compiled from source:

```sh
NDKBIN=$ANDROID_NDK_HOME/toolchains/llvm/prebuilt/darwin-x86_64/bin   # linux-x86_64 on Linux
PATH=$NDKBIN:$PATH CC_aarch64_linux_android=aarch64-linux-android26-clang \
  AR_aarch64_linux_android=llvm-ar cargo check --workspace --target aarch64-linux-android
```

Add `-t armeabi-v7a`, `x86_64`, ... for other ABIs. `usbdac-core` depends on
`rusb`, `libusb1-sys`, `parking_lot`, `once_cell`, `crossbeam-channel`, `log`
and `libc`; the JNI crate adds `jni`, `serde_json` and, on Android,
`android_logger`.

## Minimal Rust example

Add the core crate with `cargo add usbdac-core@0.1.0`. The example logs
through the `log` crate.

```rust
use std::sync::Arc;
use usbdac_core::{Dac, DacConfig, DacEvent, DeviceInfo, StreamRequest};

fn play(fd: i32) -> usbdac_core::Result<()> {
    // `fd` is UsbDeviceConnection.getFileDescriptor(); the host has already
    // claimed the AudioControl and playback AudioStreaming interfaces.
    let info = DeviceInfo::new(0x2fc6, 0xf09a, "Some DAC").with_claimed_interfaces(vec![0u8, 1, 2]);
    let dac = Dac::open(fd, info, DacConfig::default())?;

    if let Some(volume) = dac.volume() {
        volume.set_fraction(0.5)?;                      // the DAC's own volume
    }

    let format = dac.negotiate(&StreamRequest::pcm(44_100, 2, 16))?;
    let stream = dac.open_stream(&format, Arc::new(|event: DacEvent| {
        log::info!("{event:?}");                        // runs on the library's event thread
    }))?;

    // Wire-layout bytes: 16-bit stereo here, 4 bytes per frame. Keep the ring
    // near its target by writing what `writable_bytes()` asks for.
    let silence = vec![0u8; stream.writable_bytes()];
    let taken = stream.write(&silence)?;                // never blocks; a short count means "full"
    log::info!("queued {taken} bytes, bit-perfect: {}", stream.is_bit_perfect());

    stream.drain()?;                                    // no more audio: play the tail out
    while !stream.is_drained() {
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    stream.close()?;
    dac.close()
}
```

`crates/usbdac-core/examples/pull_adapter.rs` shows a host with a render
callback driving `write()` from its own thread, with pause, seek and position.
The sample layout of each encoding, the event codes, the JSON schemas and the
threading rules are in [docs/API.md](docs/API.md).

## License

MIT. Copyright (c) 2026 Flick Player Contributors and usbdac contributors. See
[LICENSE](LICENSE) and [NOTICE](NOTICE).
