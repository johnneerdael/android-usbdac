# usbdac API

This document describes the public Rust API of `usbdac-core`, the JNI contract
of `libusbdac.so`, the JSON schemas and event codes that contract uses, the
sample layouts the stream accepts, the threading rules, and what "bit-perfect"
means here. Version 0.1; nothing in it has been run on hardware as a standalone
library (see the README).

Contents

1. [Principles](#1-principles)
2. [Lifecycle](#2-lifecycle)
3. [Rust API](#3-rust-api)
4. [Sample layouts](#4-sample-layouts)
5. [Events](#5-events)
6. [Threading rules](#6-threading-rules)
7. [Bit-perfect output](#7-bit-perfect-output)
8. [Volume](#8-volume)
9. [Driving the stream from a render callback](#9-driving-the-stream-from-a-render-callback)
10. [JNI contract](#10-jni-contract)
11. [JSON schemas](#11-json-schemas)
12. [Limits of v0.1](#12-limits-of-v01)

## 1. Principles

- **Bit-perfect by default.** The library never resamples, dithers or scales
  samples. The only operation that changes sample values is software gain, and
  only when the host asks for it (`set_software_gain` with a value other than
  1.0). It is refused for DoP and native DSD.
- **Push model.** The library owns a ring buffer and one USB output thread. The
  host writes audio from its own thread. There is no render thread and no
  callback into the host on the audio path.
- **Handle-based.** `Dac` is the device, `DacStream` the output stream,
  `VolumeControl` the hardware volume. Every process-wide static of the
  implementation sits behind these handles. v0.1 supports one open `Dac` per
  process; a second `Dac::open` returns `Error::Busy`.
- **Honest reporting.** `is_bit_perfect()` is false whenever the library cannot
  vouch for the path, and stays false after audio was lost on the bus.

## 2. Lifecycle

```
Dac::open(fd, DeviceInfo, DacConfig)          scan descriptors, find volume, hold the device
  |- dac.capabilities()                       what the DAC offers
  |- dac.volume()  -> VolumeControl           works with or without a stream
  |- dac.negotiate(&StreamRequest) -> StreamFormat     programs and verifies the clock
  |- dac.open_stream(&StreamFormat, listener) -> DacStream
  |     |- stream.write(bytes) ... pause/play/flush/drain
  |     `- stream.close()                     (or drop)
  `- dac.close()                              (or drop) hand the volume back, release
```

On Android the host does the permission and claim work:

1. `UsbManager.requestPermission` and `openDevice` give a `UsbDeviceConnection`.
2. Claim the AudioControl interface(s) and the playback AudioStreaming
   interface(s) with `claimInterface(iface, true)`.
3. Pass `connection.fileDescriptor` and the claimed interface numbers to
   `Dac::open`. The library duplicates the descriptor; the host keeps its
   connection open until the `Dac` is closed or dropped, then releases the
   interfaces.

`DeviceInfo.claimed_interfaces` must equal the set of AudioControl and
playback AudioStreaming interfaces of the active configuration. A mismatch is
`Error::Claim`: the library does not guess which interfaces it may touch.

## 3. Rust API

All calls take `&self`; the handles are `Send + Sync`.

### `Dac`

```rust
Dac::open(fd: RawFd, info: DeviceInfo, config: DacConfig) -> Result<Dac>
dac.info() -> &DeviceInfo
dac.capabilities() -> Result<Capabilities>
dac.negotiate(&StreamRequest) -> Result<StreamFormat>
dac.open_stream(&StreamFormat, Arc<dyn DacListener>) -> Result<DacStream>
dac.volume() -> Option<VolumeControl>
dac.set_listener(Option<Arc<dyn DacListener>>)     // device-level events
dac.close(self) -> Result<()>                      // also on drop
```

- `open` scans the descriptors, finds the hardware volume control and records
  the DAC's level. It does not touch the clock and starts no audio. Errors:
  `Busy`, `PlatformUnsupported` (not Linux or Android), `Claim`,
  `Unsupported` (no isochronous OUT audio interface), `InvalidArgument` (config).
- `negotiate` settles the wire format. It programs the DAC's sampling
  frequency and reads it back, so it takes 100 ms or more, and it fails with
  `InvalidState` while a stream is open. With `RatePolicy::Exact` a rate the DAC
  cannot run is `Unsupported { offered }`. With `NearestSupported` the returned
  format carries the rate the DAC will run (`rate_differs_from_request()`), and
  the host must resample. Nothing is resampled by the library.
- `open_stream` switches the alternate setting, re-verifies the clock and
  starts the output thread (100 ms or more). Errors include
  `ClockUnverified` (see `DacConfig::require_verified_rate`), `Unsupported`,
  `Claim`, and `InvalidState` if a stream is already open. Only one stream at a
  time.
- `close` stops the stream, hands the volume back as `DacConfig::hand_back`
  says, releases the session and joins the threads. After it returns another
  `Dac::open` can succeed. Do not call it from inside a `DacListener`.

### `DacConfig`

`DacConfig::default()` is what the transport was tuned with.

| Field | Default | Meaning |
|---|---|---|
| `buffer_target_ms` | 250 | Depth `writable_bytes()` fills the ring to. |
| `buffer_capacity_ms` | 500 | Hard capacity; `write` never accepts more. |
| `start_threshold_ms` | 100 | Audio the ring must hold before the stream starts or resumes after `flush()`. Never above the target. |
| `processing_buffer_target_ms` | 100 | Recommended depth while software gain is not 1.0. |
| `require_verified_rate` | `true` | Refuse to stream unless the DAC confirms the sampling frequency. Off: a DAC with a broken readback streams, and the stream is never bit-perfect. |
| `clock_settle_delay_ms` | `None` | Wait after programming the clock. `None`: the device quirk, else 20 ms. |
| `dsd_big_endian` | `None` | Native DSD byte order on the wire. `None`: the device quirk, else little-endian. |
| `dsd_subslot` | `None` | Subslot size (1, 2, 4) for native DSD alternate settings. `None`: the device quirk, else the descriptor. |
| `padded_24bit` | `None` | Prefer the 4-byte subslot for 24-bit PCM. `None`: the device quirk, else off. |
| `quirks` | empty | `QuirkOverride { vendor_id, product_id, name_contains, quirks: Vec<Quirk> }`, added to the built-in table. |
| `realtime_priority` | `true` | `SCHED_FIFO` (nice -16 fallback) for the output thread. Android only. |
| `hand_back` | `RestoreOriginal` | `RestoreOriginal`: write the DAC's original volume back on close. `Keep`: leave it, and remember the original for the next `open` in this process. |

`Quirk` values: `SkipClockValidation`, `IgnoreInvalidSampleRate`,
`RequireVerifiedRate`, `SettleDelayMs(u64)`, `PreferPadded24BitTransport`,
`DsdSubslotSize(u8)`, `DsdBigEndian`, `DsdBitReverse`. Config fields set
explicitly win over quirks. The built-in table has two entries (MOONDROP Dawn
Pro; Fosi Audio DS2), both verified in the project this was extracted from.

### `Capabilities`

```rust
Capabilities {
    uac_version: UacVersion,          // V1 | V2 | Unknown
    link_speed: LinkSpeed,            // Low | Full | High | Super | SuperPlus | Unknown
    alt_settings: Vec<AltSetting>,
    pcm_sample_rates: Vec<u32>,       // union over PCM alternate settings
    pcm_bit_depths: Vec<u8>,
    channel_counts: Vec<u16>,
    supports_pcm: bool,
    supports_dop: bool,               // a PCM alt with >= 24 valid bits that runs at >= 176.4 kHz
    dop_max_dsd_bit_rate: Option<u32>,
    supports_native_dsd: bool,
    native_dsd_bit_rates: Vec<u32>,
    has_explicit_feedback: bool,
    volume: Option<VolumeRange>,
    has_mute: bool,
}
AltSetting { interface_number, alt_setting, endpoint_address, format: AltFormat /*Pcm|Dsd|Other*/,
             channels, subslot_bytes, valid_bits, rate_ranges: Vec<RateRange>, sample_rates,
             sync_type: SyncKind, feedback_endpoint: Option<u8>, max_packet_bytes,
             service_interval_us, uac_version }
```

### `StreamRequest`, `StreamFormat`

```rust
StreamRequest { sample_rate, channels, bit_depth, encoding: Encoding, rate_policy: RatePolicy }
StreamRequest::pcm(rate, channels, bits)
StreamRequest::dop(dsd_bit_rate, channels)          // 24-bit carrier, rate = dsd_bit_rate / 16
StreamRequest::dsd_native(dsd_bit_rate, channels)   // sample_rate and bit_depth ignored
Encoding::{ Pcm, DoP, DsdNative { bit_rate } }
RatePolicy::{ Exact (default), NearestSupported }
```

`StreamFormat` is what will go on the wire: `sample_rate` (PCM rate, DoP
carrier rate, or for native DSD the rate of subslot-sized words, `bit_rate / (8 *
subslot_bytes)`), `channels`, `valid_bits`, `subslot_bytes`, the interface,
alternate setting and endpoint, `sync_type`, `explicit_feedback`, `link_speed`,
`service_interval_us`. Helpers: `bytes_per_frame()` (`subslot_bytes * channels`,
the unit of `write`), `counter_rate_hz()` (the unit of `played_frames` and
`latency_frames`: the wire frame rate, or `bit_rate / 8` for native DSD),
`rate_differs_from_request()`. It cannot be built by hand; pass the one
`negotiate` returned.

### `DacStream`

```rust
stream.format() -> &StreamFormat
stream.write(&[u8]) -> Result<usize>        // wire-layout bytes, see section 4
stream.write_i32(&[i32]) -> Result<usize>   // the ring's native unit
stream.writable_bytes() -> usize
stream.play() / pause() / flush() / drain() -> Result<()>
stream.is_drained() -> bool
stream.close(self) -> Result<()>            // also on drop
stream.state() -> StreamState
stream.played_frames() -> u64
stream.latency_frames() -> u64
stream.stats() -> StreamStats
stream.is_bit_perfect() -> bool
stream.set_software_gain(f32) -> Result<()>
stream.software_gain() -> f32
```

- `write` never blocks. It takes whole wire frames, as many as fit under the
  ring's capacity, and returns the bytes taken. A short count means the ring is
  full: keep the remainder and retry. Input shorter than one frame is not taken.
  After `drain()` (until `flush()`) and once the stream has closed or failed it
  returns `InvalidState`.
- `writable_bytes()` is the room up to the target depth (the shallower
  `processing_buffer_target_ms` while software gain is on), a whole number of
  frames, and 0 after `drain()`. It is the number a pull loop should write; it is
  not a hard limit.
- A new stream plays by itself once the ring holds `start_threshold_ms` of
  audio. `play()` is only needed after `pause()`.
- `pause()` sends wire-correct silence (zero for PCM, the idle pattern for DSD,
  DoP markers keep alternating) so the DAC stays locked, and does not consume
  the ring. Transfers already queued on the bus (eight of them: 16 to 32 ms on
  a high-speed link, about 130 ms on a full-speed one) still play out first.
  `play()` continues exactly where the ring was.
- `flush()` drops everything in the ring, resets `played_frames()` to 0
  immediately, clears a pending `drain()`, and holds silence until the ring has
  the start threshold again. Audio already queued on the bus still plays (it is
  not counted). Use it for seeks and for switching tracks.
- `drain()` declares the end of the audio. The ring plays out, including a tail
  shorter than the start threshold, and the padded end is not an underrun.
  `is_drained()` becomes true when everything written has completed on the bus;
  the state goes `Draining` then `Drained`. For gapless playback do not call it
  between tracks; just keep writing.
- `played_frames()` is the number of music frames completed on the bus since the
  stream started or the last `flush()`. Held silence (pause, the wait after a
  flush), underrun padding and lost frames are not counted. Audible position is
  `start + played_frames() / format().counter_rate_hz()`.
- `latency_frames()` is the music between the host and the DAC: the ring plus
  what is on the bus.
- `write_i32` counts samples and takes multiples of `channels` (of
  `subslot_bytes * channels` for native DSD).

`StreamState`: `Buffering`, `Playing`, `Paused`, `Draining`, `Drained`,
`Closed`, `Failed` (codes 0 to 6).

`StreamStats` carries: `state`, `bit_perfect`, `software_gain`, `underruns`,
`lost_frames`, `short_packets`, `zero_packets`, `urb_timeouts`,
`completed_transfers`, `frames_written`, `frames_completed`, `played_frames`,
`latency_frames`, `latency_ms`, `queued_frames`, `buffer_ms`,
`buffer_target_ms`, `buffer_capacity_ms`, `transfer_queue_depth`,
`last_turnaround_us`, `max_turnaround_us`, `last_submit_gap_us`,
`max_submit_gap_us`, `effective_rate_hz`, `feedback_active`,
`feedback_reports`, `feedback_timeouts`, `feedback_errors`,
`feedback_rejected`, `feedback_age_ms`, `feedback_rate_hz`, `feedback_stale`.
Frame counters are in `counter_rate_hz()` units.

### `Error`

| Variant | `code()` | When |
|---|---|---|
| `Busy` | `busy` | Another `Dac` is open in this process, or one is still shutting down. |
| `PlatformUnsupported` | `platform_unsupported` | Not Linux or Android. |
| `Claim(String)` | `claim` | Bad descriptor, interfaces not claimed as required, libusb could not take the descriptor. |
| `Unsupported { reason, offered }` | `unsupported` | No alternate setting carries the request; `offered` lists what the DAC has. |
| `ClockUnverified { requested_hz, reported_hz }` | `clock_unverified` | The DAC did not confirm the rate and `require_verified_rate` is on, or it reported a different rate. |
| `DeviceLost` | `device_lost` | Unplugged or stopped answering. |
| `Transfer(String)` | `transfer` | An isochronous transfer or the stream machinery failed. |
| `Control(String)` | `control` | A class control request (clock, volume, mute) failed. |
| `InvalidState(String)` | `invalid_state` | The call is not valid now (closed, drained, second stream, ...). |
| `InvalidArgument(String)` | `invalid_argument` | Out of range or malformed argument. |

`Display` is `"<code>: <message>"`.

### Descriptor parsing

`usbdac_core::descriptors` exposes the UAC 1.0/2.0 descriptor parsers the
transport uses (`AudioControlParser`, `AudioStreamingParser`, `DescriptorIter`,
`parse_*` functions and the descriptor types). They work on byte slices and need
no device. They are lower level than the rest and may change before 1.0.

## 4. Sample layouts

`write` takes **wire-layout bytes**: exactly the bytes that go into the USB
packets, in whole wire frames of `bytes_per_frame()` bytes, channels
interleaved in the DAC's channel order, little-endian.

**PCM.** Each sample is `subslot_bytes` bytes, signed two's complement,
*left-justified*: the sign bit is the top bit of the subslot and unused low bits
are zero.

| `subslot_bytes` / `valid_bits` | Sample bytes |
|---|---|
| 2 / 16 | `i16` little-endian |
| 3 / 24 | packed `i24` little-endian |
| 4 / 24 | the `i24` shifted left by 8 (lowest byte zero) |
| 4 / 32 | `i32` little-endian |
| 3 / 16 | the `i16` shifted left by 8 (lowest byte zero) |

The bytes written are the bytes on the wire. The test suite checks this
round trip for the first four rows.

**DoP.** Each sample is the DoP word in the same subslot layout: 16 DSD bits in
the two bytes below the marker. For a 3-byte subslot the sample is
`[dsd_low, dsd_high, marker]`; for a 4-byte subslot `[0, dsd_low, dsd_high,
marker]`. `dsd_high` carries the older 8 DSD bits, most significant bit first.
The marker byte you write is **ignored and overwritten**: the library owns the
`0x05`/`0xFA` alternation (the same marker for every channel of a frame), so the
phase survives pause, flush and underrun. Request DoP with
`StreamRequest::dop(dsd_bit_rate, channels)`: the carrier rate is the DSD rate
divided by 16.

**Native DSD.** Raw DSD bytes, most significant bit first (the oldest bit in bit
7), one byte per channel per step, interleaved: `[ch0, ch1, ch0, ch1, ...]`. A
wire frame is `subslot_bytes * channels` bytes (one wire word per channel); the
library groups the consecutive bytes of a channel into the wire word and orders
them as the DAC needs (big-endian or little-endian per quirk or
`dsd_big_endian`, with optional bit reversal for DACs that want it). DSF files
store the least significant bit first, so a host reading DSF must reverse the
bits of each byte before writing. Request it with
`StreamRequest::dsd_native(bit_rate, channels)`. The counters count DSD
"frames" of one byte per channel (8 DSD bits per channel), at `bit_rate / 8`.

`write_i32` takes the ring's own unit: PCM as full-scale left-justified 32-bit
`i32` (the encoder keeps the top `valid_bits` bits, no dither); DoP as `i32`
words with the DSD bits in bits 23..8 (bits 31..24 are overwritten with the
marker); native DSD as one DSD byte per `i32` in bits 7..0.

## 5. Events

```rust
trait DacListener: Send + Sync { fn on_event(&self, event: DacEvent); }
```

Closures `Fn(DacEvent) + Send + Sync` are listeners.

| `DacEvent` | When |
|---|---|
| `StateChanged(StreamState)` | The stream state changed. |
| `Underrun` | The ring ran dry and the wire was padded. Once per run of underruns. |
| `LostFrames { frames }` | The DAC did not take queued audio (timed-out or short transfer). It cannot be re-sent. |
| `BitPerfectChanged(bool)` | `is_bit_perfect()` changed. |
| `VolumeChanged(VolumeState)` | The hardware volume or mute was changed through this library. |
| `FeedbackStale` | The explicit feedback endpoint stopped producing valid reports; timing falls back to the last good value or the nominal rate. |
| `Error(Error)` | The stream failed. Followed by `StateChanged(Failed)`. |
| `DeviceLost` | The device is gone. Also delivered as `Error(DeviceLost)` for a stream. |

A stream that failed stays open in the `Failed` state (writes return `InvalidState`) until you close it; `open_stream` refuses to start another one before that.

The listener passed to `open_stream` receives the stream's events. Events about
the device itself (`VolumeChanged`, `DeviceLost`) go to the listener set with
`Dac::set_listener` and also to the stream's listener while a stream is open.

Delivery: every `on_event` call comes from one library-owned thread named
`usbdac-events`, in order, and never while the library holds an internal lock.
The audio paths hand events over through a bounded queue with a non-blocking
send, so a slow listener cannot stall the USB loop; if the queue (256 events)
overflows, the newest events are dropped and a warning is logged. The listener
must not block for long, and must not call `Dac::close` (it can call anything
else). A panic in a listener is caught and logged.

## 6. Threading rules

- `Dac`, `DacStream`, `VolumeControl` and `StreamFormat` are `Send + Sync`;
  all methods take `&self`.
- Non-blocking, safe from a real-time-ish thread: `write`, `write_i32`,
  `writable_bytes`, `play`, `pause`, `flush`, `drain`, `is_drained`, `state`,
  `played_frames`, `latency_frames`, `set_software_gain`, `software_gain`. They
  take short internal locks (the ring lock is held for the length of a copy).
- Blocking, call them off the UI thread: `Dac::open` (descriptor and clock
  reads), `negotiate` and `open_stream` (100 ms or more each), `close` (up to about
  130 ms for the last transfers to finish), every `VolumeControl` call (control
  transfers, 1 s timeout each), `stats()` and `is_bit_perfect()` take one
  short lock but also touch session state.
- The library starts two threads: `usbdac-output` (the isochronous loop; with
  `realtime_priority` it asks for `SCHED_FIFO` priority 2 on Android) and
  `usbdac-events`. It never calls into the host from the output thread.
- Do not write from two threads at once: `write` is thread-safe but the byte
  stream you write is one ordered sequence.

## 7. Bit-perfect output

`DacStream::is_bit_perfect()` is true only while all of these hold:

1. the stream is open and its output thread is running;
2. the format on the wire is the one requested (no `NearestSupported` rate
   change);
3. the DAC's sampling frequency was verified (readback equal to the requested
   rate; a device whose quirk is `SkipClockValidation` is trusted on its SET_CUR);
4. software gain is exactly 1.0;
5. no frame was lost on the bus since the stream started (the first lost frame
   clears the flag for the life of the stream);
6. the wire payload audit passes. For PCM the audit needs real audio: channel 0
   must vary within a transfer, with a 1.5 s grace so digital silence or a
   fade does not flip the flag. For DoP the markers must alternate and agree
   across channels. For native DSD the data must not be all zero.

What it does not cover: the claim is about the transport. If the host writes
processed samples (EQ, volume applied in software, resampling), the library
cannot know. Underruns pad the wire with silence and do not clear the flag by
themselves; check `StreamStats::underruns`. The payload audit is a statistical
guard against a dead or stuck pipeline, not a hash comparison against the
source.

**MQA.** MQA is ordinary PCM whose signalling lives in the low bits of the
samples. The library neither decodes nor detects it. The signalling reaches the
DAC intact when the stream is bit-perfect (every sample unmodified), which also
means volume must be the DAC's hardware control, not software gain.

## 8. Volume

`dac.volume()` returns a `VolumeControl` when the DAC's Feature Unit has a
writable volume (UAC 1.0 or 2.0, master control or all channels). It works with
or without a stream, as long as the `Dac` is open. The control is found through
the first PCM playback terminal at open and looked up again if a stream plays
through another terminal.

```rust
volume.range() -> Result<VolumeRange>             // { min_db, max_db, res_db }
volume.get() -> Result<VolumeState>               // { db, fraction, muted: Option<bool> }
volume.set_db(f64) -> Result<VolumeState>
volume.set_fraction(f64) -> Result<VolumeState>
volume.step(direction: i8, steps: u8) -> Result<VolumeState>   // steps == 0 means 16
volume.set_mute(bool) -> Result<VolumeState>
volume.original_db() -> Option<f64>
volume.hand_back() -> Result<bool>
```

- UAC volume units are 1/256 dB. `set_db` clamps to the range and snaps to the
  nearest step the DAC supports (a tie goes to the quieter step). Levels below
  -80 dB are reachable.
- `fraction` maps 0..1 onto the DAC: 0 is the device minimum; anything above 0
  spans `max(min, -80 dB)` to the maximum, linearly in dB. A DAC that reports
  -127 dB as its minimum therefore does not waste most of the slider on inaudible
  levels. A level below -80 dB reads as fraction 0.0 but keeps its exact `db`.
  `set_fraction` is the exact inverse of the read-back mapping.
- `step(direction, steps)` moves by `1/steps` of the 0..1 curve and always by at
  least one DAC step. At either end it returns the current level.
- Writes are verified by reading the level back; a multi-channel write that
  fails restores the channels already written. A readback that differs from the
  write is `Error::Control`.
- `original_db()` is the level the DAC had the first time the library read it
  (before any write), if all channels agreed. `hand_back()` writes it back and
  ends the record. With `HandBack::RestoreOriginal` this happens at close; with
  `Keep` the record survives in the process so a later `open` of the same device
  still knows the true original.
- `VolumeChanged` events are sent for changes made through this library, not
  for a physical knob; call `get()` to poll (it sends the event if the level
  differs from the last one it saw).

## 9. Driving the stream from a render callback

A host with a decoder that renders on request runs a writer thread that asks the
stream how much it wants and renders only that. `examples/pull_adapter.rs`
(compile-checked with `cargo check --examples`) is a complete adapter; the core
of it:

```rust
let frame = stream.format().bytes_per_frame();
let mut scratch = vec![0u8; (stream.format().sample_rate as usize / 100) * frame]; // ~10 ms
while !stop.load(Ordering::Acquire) {
    // Render only when the ring wants more.
    if pending.is_empty() {
        if stream.writable_bytes() < scratch.len() { sleep(2 ms); continue; }
        let n = source.render(&mut scratch);          // wire-layout bytes, whole frames
        if n == 0 { stream.drain()?; break; }         // end of audio: play the tail out
        pending = scratch[..n].to_vec();
    }
    let taken = stream.write(&pending)?;              // never blocks; short count = ring full
    pending.drain(..taken);
    if taken == 0 { sleep(2 ms); }
}
```

How the transport controls map:

| Host action | Call |
|---|---|
| Pause | `stream.pause()`. Keep the writer running; it fills to the target and idles on `writable_bytes() == 0`. |
| Resume | `stream.play()` |
| Seek | Under the same lock the writer uses: reposition the decoder, discard the chunk rendered for the old position, `stream.flush()`, remember the new start time. |
| Stop / next user-chosen track | `stream.flush()`, then write the new audio. |
| Gapless next track | Keep writing; no `drain()` and no `flush()`. |
| End of the last track | `stream.drain()`, then wait for `is_drained()` or the `Drained` state. |
| Position | `start_seconds + played_frames() / counter_rate_hz()`, with `start_seconds` set at each flush. |
| Volume | `dac.volume()` (hardware). Software gain only if you accept losing bit-perfect. |

The flush matters for latency: a deep ring is what makes the stream robust
against stalls (250 ms by default), and anything that must be heard now (seek,
stop, a track the user picked) flushes it so the depth costs no responsiveness.

## 10. JNI contract

`libusbdac.so` (built from `crates/usbdac-jni`) implements the native methods of
the Kotlin object `dev.usbdac.internal.Native`. Declare them `@JvmStatic`.
Handles are opaque `Long` ids into a registry, never pointers; an id that is not
open is an `invalid_argument` error, not a crash. The listener is any object
with the method below.

```kotlin
package dev.usbdac.internal

import java.nio.ByteBuffer

interface NativeListener {
    fun onEvent(type: Int, a: Long, b: Double, message: String?)
}

object Native {
    init { System.loadLibrary("usbdac") }

    @JvmStatic external fun version(): String
    @JvmStatic external fun setLogLevel(level: Int)            // 0 off .. 5 trace; logcat tag "usbdac"

    @JvmStatic external fun dacOpen(fd: Int, deviceInfoJson: String, configJson: String?): Long
    @JvmStatic external fun dacClose(dac: Long)
    @JvmStatic external fun dacCapabilities(dac: Long): String
    @JvmStatic external fun dacNegotiate(dac: Long, requestJson: String): String
    @JvmStatic external fun dacSetListener(dac: Long, listener: Any?)

    @JvmStatic external fun streamOpen(dac: Long, formatId: Long, listener: Any): Long
    @JvmStatic external fun streamClose(stream: Long)
    @JvmStatic external fun streamWrite(stream: Long, buffer: ByteBuffer, offset: Int, count: Int): Int
    @JvmStatic external fun streamWritableBytes(stream: Long): Int
    @JvmStatic external fun streamPlay(stream: Long)
    @JvmStatic external fun streamPause(stream: Long)
    @JvmStatic external fun streamFlush(stream: Long)
    @JvmStatic external fun streamDrain(stream: Long)
    @JvmStatic external fun streamIsDrained(stream: Long): Boolean
    @JvmStatic external fun streamState(stream: Long): Int
    @JvmStatic external fun streamPlayedFrames(stream: Long): Long
    @JvmStatic external fun streamLatencyFrames(stream: Long): Long
    @JvmStatic external fun streamStats(stream: Long): String
    @JvmStatic external fun streamIsBitPerfect(stream: Long): Boolean
    @JvmStatic external fun streamSetSoftwareGain(stream: Long, gain: Float)

    @JvmStatic external fun volumeRange(dac: Long): String?     // null: no hardware volume
    @JvmStatic external fun volumeGet(dac: Long): String
    @JvmStatic external fun volumeSetDb(dac: Long, db: Double): String
    @JvmStatic external fun volumeSetFraction(dac: Long, fraction: Double): String
    @JvmStatic external fun volumeStep(dac: Long, direction: Int, steps: Int): String
    @JvmStatic external fun volumeSetMute(dac: Long, muted: Boolean): String
    @JvmStatic external fun volumeOriginalDb(dac: Long): Double  // NaN: none recorded
    @JvmStatic external fun volumeHandBack(dac: Long): Boolean
}
```

The exported symbols are `Java_dev_usbdac_internal_Native_<name>`.

- `dacNegotiate` returns the negotiated format as JSON including a `format_id`;
  pass that id to `streamOpen`. Formats are kept per DAC (the latest 16) until
  the DAC is closed.
- `streamWrite` takes a **direct** `ByteBuffer` (`ByteBuffer.allocateDirect`),
  reads `count` bytes from `offset` (the buffer's position and limit are
  ignored and not moved) and returns the bytes taken, as in section 3.
- `dacClose` closes the streams opened from the DAC as well. `streamClose` on a
  stream that is already gone is an error; closing is not idempotent at this
  level, because the handle is released.
- The `volume*` functions return the resulting state as JSON, except
  `volumeRange`, `volumeOriginalDb` and `volumeHandBack`. They throw
  `usbdac:control:` when the DAC has no hardware volume.
- Events call `listener.onEvent(type, a, b, message)` on the library's
  `usbdac-events` thread, which the library attaches to the JVM on first use.
  The `listener` of `dacSetListener` receives device-level events; the one of
  `streamOpen` the stream's. An exception thrown by the listener is caught and
  logged. Pass the listener as a global reference-able object (the library
  creates the global reference and releases it with the DAC or stream).

### Errors

Every failure is a `java.lang.IllegalStateException` whose message is
`usbdac:<code>: <message>`, with `<code>` one of `busy`, `platform_unsupported`,
`claim`, `unsupported`, `clock_unverified`, `device_lost`, `transfer`,
`control`, `invalid_state`, `invalid_argument` (section 3). Malformed JSON and
bad handles are `invalid_argument`. A Rust panic is converted to a
`RuntimeException`.

### Event type codes

`onEvent(type, a, b, message)`:

| `type` | Event | `a` | `b` | `message` |
|---|---|---|---|---|
| 0 | StateChanged | state code: 0 buffering, 1 playing, 2 paused, 3 draining, 4 drained, 5 closed, 6 failed | 0 | null |
| 1 | Underrun | 0 | 0 | null |
| 2 | LostFrames | frames lost | 0 | null |
| 3 | BitPerfectChanged | 1 or 0 | 0 | null |
| 4 | VolumeChanged | muted: 1, 0, or -1 unknown | level in dB | the volume state JSON |
| 5 | FeedbackStale | 0 | 0 | null |
| 6 | Error | error code: 1 busy, 2 platform_unsupported, 3 claim, 4 unsupported, 5 clock_unverified, 6 device_lost, 7 transfer, 8 control, 9 invalid_state, 10 invalid_argument | 0 | `usbdac:<code>: <message>` |
| 7 | DeviceLost | 0 | 0 | null |

Unknown future events arrive with `type` -1 and a debug string in `message`.

## 11. JSON schemas

Keys are snake_case. `null` stands for "none". Unknown request keys are
ignored.

**Device info** (`dacOpen`):

```json
{ "vendor_id": 12230, "product_id": 61546, "product_name": "MOONDROP Dawn Pro",
  "manufacturer_name": "MOONDROP", "device_name": "/dev/bus/usb/001/004",
  "claimed_interfaces": [0, 1, 2] }
```

`vendor_id` and `product_id` are required; the rest is optional.

**Config** (`dacOpen`, nullable; every key optional, defaults as in section 3):

```json
{ "buffer_target_ms": 250, "buffer_capacity_ms": 500, "start_threshold_ms": 100,
  "processing_buffer_target_ms": 100, "require_verified_rate": true,
  "clock_settle_delay_ms": 20, "dsd_big_endian": true, "dsd_subslot": 4,
  "padded_24bit": true, "realtime_priority": true,
  "hand_back": "restore_original",
  "quirks": [ { "vendor_id": 1, "product_id": 2, "name_contains": "Foo",
                "quirks": ["skip_clock_validation", {"settle_delay_ms": 80}] } ] }
```

`hand_back` is `restore_original` or `keep`. Flag quirks:
`skip_clock_validation`, `ignore_invalid_sample_rate`, `require_verified_rate`,
`prefer_padded_24bit_transport`, `dsd_big_endian`, `dsd_bit_reverse`; valued
quirks are one-entry objects: `{"settle_delay_ms": n}`, `{"dsd_subslot_size": n}`.

**Stream request** (`dacNegotiate`):

```json
{ "encoding": "pcm", "sample_rate": 44100, "channels": 2, "bit_depth": 16, "rate_policy": "exact" }
{ "encoding": "dop", "dsd_bit_rate": 2822400, "channels": 2 }
{ "encoding": "dsd_native", "dsd_bit_rate": 2822400, "channels": 2, "rate_policy": "nearest" }
```

`encoding` is `pcm` (default), `dop` or `dsd_native`; `rate_policy` is `exact`
(default) or `nearest`. For `dop`, `dsd_bit_rate` derives the carrier rate (rate
/ 16, 24-bit); an explicit `sample_rate` or `bit_depth` overrides it.

**Capabilities** (`dacCapabilities`):

```json
{ "uac_version": "v2", "link_speed": "high",
  "pcm_sample_rates": [44100, 48000, 96000], "pcm_bit_depths": [16, 24, 32],
  "channel_counts": [2],
  "supports_pcm": true, "supports_dop": true, "dop_max_dsd_bit_rate": 11289600,
  "supports_native_dsd": false, "native_dsd_bit_rates": [],
  "has_explicit_feedback": true,
  "volume": { "min_db": -127.0, "max_db": 0.0, "res_db": 1.0 }, "has_mute": true,
  "alt_settings": [ { "interface_number": 1, "alt_setting": 1, "endpoint_address": 1,
      "format": "pcm", "channels": 2, "subslot_bytes": 4, "valid_bits": 32,
      "rate_ranges": [ { "min_hz": 44100, "max_hz": 44100, "step_hz": 0 } ],
      "sample_rates": [44100], "sync_type": "asynchronous", "feedback_endpoint": 129,
      "max_packet_bytes": 1024, "service_interval_us": 125, "uac_version": "v2" } ] }
```

`uac_version`: `v1`, `v2`, `unknown`. `link_speed`: `low`, `full`, `high`,
`super`, `super_plus`, `unknown`. `format`: `pcm`, `dsd`, `other`. `sync_type`:
`asynchronous`, `adaptive`, `synchronous`, `none`.

**Negotiated format** (`dacNegotiate`):

```json
{ "format_id": 7, "encoding": "pcm", "dsd_bit_rate": null, "sample_rate": 44100,
  "channels": 2, "valid_bits": 16, "subslot_bytes": 2, "bytes_per_frame": 4,
  "counter_rate_hz": 44100, "interface_number": 1, "alt_setting": 1,
  "endpoint_address": 1, "sync_type": "asynchronous", "explicit_feedback": true,
  "link_speed": "high", "service_interval_us": 125, "rate_differs_from_request": false }
```

**Stream statistics** (`streamStats`): all `StreamStats` fields of section 3 as
numbers (`state` is the state code; `bit_perfect` boolean; `software_gain`,
`latency_ms`, `effective_rate_hz`, `feedback_rate_hz` floating point; the
`feedback_age_ms` and `feedback_rate_hz` entries are `null` before the first
feedback report).

**Volume range**: `{ "min_db": -127.0, "max_db": 0.0, "res_db": 1.0 }`.
**Volume state**: `{ "db": -40.0, "fraction": 0.5, "muted": false }` (`muted` is
`null` when the DAC has no readable mute).

## 12. Limits of v0.1

- One open `Dac` per process (a second `open` is `Busy`). The session state is a
  process-wide slot behind the handles; moving it into the `Dac` later changes
  no signature.
- Linux and Android only (usbfs through libusb's wrap-by-descriptor call). On
  other hosts the crate builds and its logic is unit-tested, but `Dac::open`
  returns `PlatformUnsupported`.
- Playback only: no capture, no mixer. UAC 1.0 and 2.0 PCM and native DSD;
  not UAC 3.0.
- Not run on hardware as a standalone library. The transport is the code that
  ran in Flick; the layers around it (handle API, push ring, events, JNI) have
  only been exercised by host tests.
