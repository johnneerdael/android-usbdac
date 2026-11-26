# usbdac

Bit-perfect direct USB DAC output for Android and Linux, as a library.

usbdac drives a USB Audio Class 1.0/2.0 DAC through usbfs without the platform
audio stack: PCM at the DAC's native depth, DSD over PCM (DoP) and native DSD;
isochronous transfers paced by the DAC's explicit feedback endpoint; honest
accounting of lost audio and of whether the output is bit-perfect; and the DAC's
own hardware volume. It never resamples, dithers or scales samples unless the host
turns on software gain.

**Status: v0.1, derived from Flick, not yet run on hardware as a standalone
library.** The USB transport is the driver that runs inside the Flick music
player (see [NOTICE](NOTICE)). What is new, and has only been exercised by host
tests, is everything around it: the handle-based push API, the event thread, the
configuration, the hardware-volume handle and the JNI layer.

## Layout

```text
crates/usbdac-core/   the driver: no JNI, no application types
crates/usbdac-jni/    libusbdac.so, the JNI bindings (package dev.usbdac)
docs/API.md           the Rust API, JNI contract, JSON schemas, sample layouts
```

An Android library (AAR) with the Kotlin side of `dev.usbdac.internal.Native` is
meant to sit on top of the JNI layer; it is not part of this repository yet.

## What it does

- Formats: PCM 16/24/32-bit in the subslot size the DAC advertises, DoP
  (carrier up to what the DAC runs), native DSD (DSD_U8/U16/U32, byte order and
  bit order per device).
- Output: a ring buffer (250 ms target, 500 ms capacity by default) feeds eight
  isochronous transfers in flight; explicit feedback endpoints are polled
  asynchronously and steer the packet sizes.
- Reporting: played frames and latency counted on completed transfers, lost
  frames (timed-out and short transfers) latched, an audit of the wire payload,
  and `is_bit_perfect()` combining them with the clock verification.
- Volume: the Feature Unit's volume and mute, with read-back verification, a dB
  and a 0..1 view, the level the DAC had before, and a hand-back on close.
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
