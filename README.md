# usbdac

Add direct USB DAC playback to an Android audio app. usbdac sends decoded PCM,
DoP or native DSD to the DAC through USB, bypassing Android's mixer on the
direct path. Your app can control the DAC's hardware volume and show whether
the stream is bit-perfect, with the reason when verification fails.

The shared USB Audio implementation includes UAC3 and BADD, PCM capture and
duplex, channel layouts, verified clock selection and restoration, standard
routing controls and guarded device-specific controls. Native DSD uses a
standard UAC3 layout or a resolved legacy profile. DoP capability describes
a feasible PCM carrier; marker decoding still needs device evidence.
Read the [implementation ledger](docs/design/alsa-parity.md) and
[release validation scope](docs/sdk/evidence.md#release-validation-scope).
For in-app support captures, see [collecting a USB playback support report](docs/sdk/support-diagnostics.md).

| Integrating with | Start here |
|---|---|
| Kotlin on Android 8.0+ | Add `dev.usbdac:usbdac` from GitHub Packages. The AAR bundles the native libraries for all four Android ABIs. [Android quickstart](https://johnneerdael.github.io/android-usbdac/getting-started.html) |
| Rust on Android or Linux | Add [`usbdac-core` from crates.io](https://crates.io/crates/usbdac-core). Your app handles USB access and passes an open file descriptor to the driver. [Rust API reference](https://docs.rs/usbdac-core) |

Use `OutputSelector` when your Android player needs one PCM playback integration
that can use direct USB, Android-managed audio or AAudio exclusive as the device
and permissions allow. [SDK guide](https://johnneerdael.github.io/android-usbdac/)

Download the AAR, native libraries and crate archives from
[GitHub Releases](https://github.com/johnneerdael/android-usbdac/releases).

The same site has the developer documentation: [design decisions](https://johnneerdael.github.io/android-usbdac/decisions.html) (each compared with what Flick, Mornye and BitChord do, with the evidence and public references), an [architecture](https://johnneerdael.github.io/android-usbdac/architecture.html) page, an [integration guide](https://johnneerdael.github.io/android-usbdac/integration.html) and an [evidence and status](https://johnneerdael.github.io/android-usbdac/evidence.html) page.

## Where it came from, and what changed

usbdac started from the USB DAC driver of the [Flick](https://github.com/moss-apps/Flick)
music player. It is no longer that driver. The code was taken out of the app,
given a public API, and improved in ways Flick's own driver has not been.
[What usbdac improves](docs/sdk/improvements.md) explains each change twice:
technically, and in plain terms. In short:

| Area | What usbdac adds | In plain terms |
|---|---|---|
| Clock handling | Resolves selectors and multipliers, verifies rates, and restores owned changes | A compatible alternate clock can serve a rate the current clock cannot |
| USB feedback | Uses the newest valid explicit rate report and supports compatible UAC2 and UAC3 implicit feedback | Playback can follow the device clock through a dedicated rate endpoint or a matching input stream |
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
| Capture and duplex | Explicit PCM input APIs, shared endpoint ownership, channel layouts and separate loss/overrun reporting | Record input alone or alongside playback without stalling USB pacing |
| Hardware routing | Session-bound USB Audio Class controls with readback and rollback | Expose the controls the device can prove it supports |
| Output choice | An `OutputSelector` that detects the DAC, permission, route and what the platform grants, and returns one ready output with the reason and the guarantee | The app asks for the best way to play and is told what it got and why |
| Conversion | Optional resampling, bit-depth and mono-to-stereo, always labelled | A file still plays on a limited DAC, and is plainly marked as not bit-perfect |
| Packaging | Rust core, Kotlin API, sample app, tests, published builds | A dependency and a few dozen lines instead of writing a USB driver |

Several of these came from studying how two other open-source players, Mornye
and BitChord, behave with the same DACs. No code from either was used.

**Status: 0.1.** Direct USB PCM has played on a OnePlus 15 with an iBasso DC04U
in Flick and with a Hidizs AP80 Pro-X in the SDK sample. The AP80 Pro-X also
verified native DSD64 through DSD256 and the transition back to PCM.
See [hardware status](docs/sdk/evidence.md#hardware-status) for the tested
formats and paths. Release preparation uses software validation and PCM playback to a
virtual QEMU USB audio device; it adds no physical streaming checks. Broader hardware
acceptance and additional models are tracked in
[device compatibility candidates](https://github.com/johnneerdael/android-usbdac/blob/main/docs/design/device-compatibility-candidates.md).

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
  (carrier up to what the DAC runs), native DSD (legacy 1-, 2- or 4-byte
  profiles and standard 8-byte UAC3 words). Legacy byte and bit order are
  resolved per alternate, device and applicable firmware.
- Output: a ring buffer (250 ms target, 500 ms capacity by default) feeds eight
  isochronous transfers in flight, 16 ms queued on the bus at every service
  interval and for every encoding, never 256 packets. Explicit rate reports and
  implicit capture packet counts steer asynchronous playback packet sizes.
- Flush: `flush()` drops the ring and cancels what is already on the bus (not
  counted as played, lost or an underrun), so a seek is heard at once.
- Reporting: played frames and latency counted on completed transfers, lost
  frames (timed-out and short transfers) latched, an audit of the wire payload,
  and `is_bit_perfect()` combining them with the clock verification, with
  `bit_perfect_blocker()` saying what stands in the way.
- Pacing: explicit feedback uses the newest valid report in each poll batch
  and checks that the resulting packet fits. A short report gap can recover
  while playback holds the last good rate. Stale feedback clears the
  bit-perfect verdict, and a persistent outage ends the stream. Implicit
  feedback uses frame counts from a compatible UAC2 IN stream that shares the
  playback clock, rate and service interval. An otherwise valid asynchronous
  route without usable feedback requires
  `DacConfig::allow_unsynchronized_async` to play at the nominal rate.
  See [clock and pacing](docs/sdk/decisions-clock.md#feedback).
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
- UAC 1.0/2.0/3.0 playback and PCM capture, including BADD profiles.
  Capture and duplex require explicit input APIs and USB claims.
- Standard routing and supported vendor controls depend on the active
  topology and verified device capabilities. Additional devices need
  evidence-backed compatibility work.
- The host owns USB permission, `claimInterface` and the file descriptor.

## Releases

Library changes merged to `main` start the Release workflow. It advances the
patch version, builds and tests a versioned commit, publishes the Rust crate
and Android AAR, and creates the GitHub Release with its downloads. A merge
that declares a higher version can select the next minor or major release.
For releases from sources containing the validator, the downloads also include
a versioned Android validator APK and Apple Silicon macOS validator ZIP built
from that same commit. The APK uses the CI debug signing key; the macOS app is
ad hoc signed and not notarized. Both are included in `SHA256SUMS.txt`.
Documentation-only changes deploy through the Docs workflow.

Use **Actions → Release → Run workflow** to build an explicit version, or
rerun the original Actions run to retry an automatic release from its
reserved commit.

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

Add the core crate with `cargo add usbdac-core@0.1.10`. The example logs
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
