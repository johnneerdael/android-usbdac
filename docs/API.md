# usbdac API

This document describes the public Rust API of `usbdac-core`, the JNI contract
of `libusbdac.so`, the JSON schemas and event codes that contract uses, the
sample layouts, the threading rules and the conditions for bit-perfect output.
For tested PCM and native DSD formats, see [hardware status](sdk/evidence.md#hardware-status).
The [release validation scope](sdk/evidence.md#release-validation-scope) distinguishes
software coverage, QEMU enumeration and physical streaming evidence.

Add [`usbdac-core` from crates.io](https://crates.io/crates/usbdac-core) to use
the USB driver from Rust on Android or Linux. Run `cargo add usbdac-core@0.1.14`,
then follow the lifecycle below to open the device and stream decoded audio.
The [Rust API reference](https://docs.rs/usbdac-core)
covers individual types and methods.

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
13. [AAudio exclusive output](#13-aaudio-exclusive-output)
14. [Conversion helpers](#14-conversion-helpers)

## 1. Principles

- **Bit-perfect by default.** The library never resamples, dithers or scales
  samples. The only operation that changes sample values is software gain, and
  only when the host asks for it (`set_software_gain` with a value other than
  1.0). It is refused for DoP and native DSD. (The opt-in conversion helpers of
  section 14 are a separate module that the direct path never calls.)
- **Push model.** The library owns a ring buffer and one USB output thread. The
  host writes audio from its own thread. There is no render thread and no
  callback into the host on the audio path.
- **Handle-based.** `Dac` is the device, `DacStream` the output stream,
  `VolumeControl` the hardware volume. Every process-wide static of the
  implementation sits behind these handles. One owner is allowed per physical
  attachment; distinct attachments can have independent owners.
- **Honest reporting.** `is_bit_perfect()` is false whenever the library cannot
  vouch for the path, and stays false after audio was lost on the bus.

## 2. Lifecycle

```
Dac::open(fd, DeviceInfo, DacConfig)          scan descriptors, find volume, lower it, hold the device
  |- dac.capabilities()                       what the DAC offers
  |- dac.volume()  -> VolumeControl           works with or without a stream
  |- dac.negotiate(&StreamRequest) -> StreamFormat     programs and verifies the clock
  |- dac.open_stream(&StreamFormat, listener) -> DacStream
  |     |- stream.write(bytes) ... pause/play/flush/drain
  |     `- stream.close()                     (or drop)
  `- dac.close()                              (or drop) hand the volume back, release
```

The Kotlin SDK handles USB permission and claims through `UsbDac`. A raw
Android host using Rust directly does the following:

1. `UsbManager.requestPermission` and `openDevice` give a `UsbDeviceConnection`.
2. Claim the AudioControl interface(s) and the playback AudioStreaming
   interface(s) with `claimInterface(iface, true)`.
3. With feedback enabled, call `Dac::feedback_preflight(fd, &info)` with the
   base claims. It returns the active configuration ID and candidate IN
   interface numbers. Try the additional claims and record their results in
   `DeviceInfo.claimed_interfaces`, `configuration_id` and
   `unavailable_feedback_interfaces`.
4. Pass `connection.fileDescriptor` and the resulting `DeviceInfo` to
   `Dac::open`. The library duplicates the descriptor. The host keeps its
   connection open until the `Dac` is closed or dropped, then releases the
   interfaces.

`DeviceInfo.claimed_interfaces` holds the actual successful claims for the
active configuration. It includes the required control and playback interfaces
and any successfully claimed optional IN sources. An unavailable optional
claim leaves other viable playback routes available.

The Kotlin SDK also offers `UsbDac.openPlaybackForRequest` for a checked exact playback request. Its optional `LEGACY_FIRST_IF_DESCRIPTOR_INCOMPATIBLE` policy defaults off; an explicit configuration ID wins. The fallback selects the actual ID at descriptor index zero only when the first configuration provides legacy playback and complete descriptors prove that the active configuration cannot carry that exact request. Full-UAC3 incompatibility is established by its active descriptors. For exact signed PCM, a same-connection native witness can also prove that a validated microphone-only BADD inventory has no playback, or that an unambiguous BADD output is fixed at 48 kHz and excludes a different requested rate. It requires fresh matching raw and parsed descriptors, stable configuration and supported bus speed; unknown or mixed output leaves normal selection unchanged. This is descriptor evidence, not physical playback proof. The fallback is playback-only and does not fire when the active configuration refuses for any other reason (a clock, claim or transfer failure). Selection and checks use one retained connection, but cannot prove that a caller's earlier device snapshot survived a matching detach/replug. On a failed fallback open, `DacPlaybackOpenException.failure` preserves the error and `pendingCleanup` retains the connection until native retirement; off the main thread, use `isReady` and explicit `finish()` for one verified restoration. See [checked playback open](sdk/integration-app.md#checked-playback-open-and-configuration-hand-back). This Kotlin transaction does not add an automatic second-configuration retry to the Rust API below.

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
dac.close_with(self, CloseVolume) -> Result<()>    // decide the volume hand-back at close
dac.set_dsd_overrides(Option<bool>, Option<u8>) -> Result<()>   // native DSD byte order, subslot
dac.feedback_trace() -> Option<FeedbackTrace>      // the last stream's support capture
Dac::feedback_preflight(fd, &DeviceInfo) -> Result<(u8, Vec<u8>)>   // see section 2
Dac::await_usb_retirement(Duration) -> bool        // after a close that timed out
dac.negotiate_candidates(&[StreamRequest]) -> Result<StreamFormat>  // one to eight exact PCM formats, in order
dac.reconfigure_stream(&DacStream, &StreamRequest, listener, drain_timeout) -> Result<StreamReconfiguration>
dac.reconfigure_stream_candidates(&DacStream, &[StreamRequest], listener, drain_timeout) -> Result<StreamReconfiguration>
dac.retirement() -> UsbRetirement                  // this owner's retirement: wait(timeout), routing_hand_back_report()
Dac::await_connection_retirement(fd, Duration) -> Result<bool>   // one connection, after a failed open or close
Dac::open_darwin(Attachment, DacConfig, allow_capture: bool) -> Result<Dac>   // macOS only
```

Capture, duplex and routing have their own entry points; they are listed under
"Capture, duplex and routing" after the `DacStream` section.

- `open` scans the descriptors, finds the hardware volume control, records the
  DAC's level and lowers it (`DacConfig::startup_volume`, see section 8). It does
  not touch the clock and starts no audio. Errors:
  `Busy`, `PlatformUnsupported` (not Linux or Android, macOS included), `Claim`,
  `Unsupported` (no isochronous OUT audio interface), `InvalidArgument` (config).
- `open_darwin` (macOS only) opens an attachment from
  `usbdac_core::darwin_discovery::snapshot` for playback through libusb. It
  claims the audio interfaces of the active configuration; only when
  `allow_capture` is true and an ordinary claim fails with an access or busy
  error does it detach the system driver from the whole device, which can need
  administrator rights. Its own failures are `HostUsb`. Capture and duplex have
  no macOS path. No macOS hardware result is recorded in the evidence.
- `negotiate` settles the wire format. It programs the DAC's sampling
  frequency and reads it back, so it takes 100 ms or more, and it fails with
  `InvalidState` while a stream is open. With `RatePolicy::Exact` a rate the DAC
  cannot run is `Unsupported { offered }`. With `NearestSupported` the returned
  format carries the rate the DAC will run (`rate_differs_from_request()`), and
  the host must resample. Nothing is resampled by the library on this path; the
  host that wants it done can use the conversion helpers of section 14.
  `NearestSupported` applies to PCM only; DoP and native DSD are always exact.
  The candidates are ranked: the requested rate, then the highest integer
  divisor of it, then the highest lower rate of the same family (44.1 or 48 kHz
  multiples), then the highest other lower rate, then the lowest higher rate.
  An 88.2 kHz request on a DAC with {44.1, 176.4} kHz therefore runs at
  44.1 kHz, halving the bandwidth; a host that prefers 176.4 kHz asks for it
  with `Exact`. Each candidate is tried as an exact request on the retained USB
  handle, at most eight within 15 s. The next one is tried only after a
  certified clean refusal: complete descriptors (standard UAC1 Type I, or UAC2
  PCM) show that no alternate setting carries the candidate, or a UAC2 Valid
  Alternate Settings control excluded it after an acknowledged SET and the
  original clock was restored and read back. Since 0.1.11 a UAC1 refusal can be
  clean in this way. Any other failure (a claim, a stalled clock write, a
  disconnect, an uncertain restoration) ends the call with that error.
- `negotiate_candidates` takes one to eight distinct exact PCM requests with the
  same channel count and tries them in order under the same clean-refusal rule.
- `reconfigure_stream` drains a playback-only stream (playing it if it was
  paused) and replaces it without releasing the USB handle or the host's claims.
  Stop the writer first. The replacement is paused with counters at zero; supply
  audio from the right position and call `play()`. If the new request is refused,
  the previous format is restored and `rollback_cause` carries the refusal.
  `reconfigure_stream_candidates` takes one to four same-rate, same-channel PCM
  requests in ascending bit depth. `ReconfigureDrainTimeout` leaves the original
  stream open and draining; `ReconfigureUnusable` means the DAC must be closed.
  While the drain runs, `negotiate`, `negotiate_candidates`, `open_stream`,
  `open_capture`, `open_duplex`, `set_dsd_overrides` and every routing call fail
  at once with `Busy` instead of waiting.
- `open_stream` switches the alternate setting, re-verifies the clock and
  starts the output thread (100 ms or more). Errors include
  `ClockUnverified` (see `DacConfig::require_verified_rate`), `Unsupported`,
  `ImplicitFeedbackUnsupported` (see "Asynchronous endpoints without feedback"
  below), `VolumeUnavailable` (see `DacConfig::require_hardware_volume`),
  `Claim`, and `InvalidState` if a stream is already open. Only one stream at a
  time.
- `close` stops the stream, hands the volume back as `DacConfig::hand_back`
  says, releases the session and joins the threads. After it returns another
  `Dac::open` can succeed. Do not call it from inside a `DacListener`.
- `close_with(CloseVolume)` is `close` for a host that only learns at the end
  what becomes of the volume: `Restore` writes the DAC's original level back and
  ends the record (another driver gets the DAC), `Keep` leaves the level and the
  record (the host holds on to the DAC or takes it straight back, and a later
  `open` still knows the true original), `Forget` ends the record without
  touching the DAC (it was unplugged, so a later attachment of the same device
  starts fresh). It overrides `DacConfig::hand_back` for this call. A DAC the
  library saw disappear (`DeviceLost`) is forgotten whatever was asked for.
- `set_dsd_overrides(big_endian, subslot)` changes `DacConfig::dsd_big_endian`
  and `DacConfig::dsd_subslot` of the open DAC, for a host whose user can change
  them while the DAC stays open. `None` goes back to the device quirk; a subslot
  other than 1, 2 or 4 is `InvalidArgument`. They are read at `negotiate` and
  `open_stream`: an open stream keeps what it started with, and `capabilities()`
  shows a new subslot size after the next `negotiate`.

### `DacConfig`

`DacConfig::default()` is what the transport was tuned with.

| Field | Default | Meaning |
|---|---|---|
| `buffer_target_ms` | 250 | Depth `writable_bytes()` fills the ring to. |
| `buffer_capacity_ms` | 500 | Hard capacity; `write` never accepts more. |
| `start_threshold_ms` | 100 | Audio the ring must hold before the stream starts or resumes after `flush()`. Never above the target. |
| `processing_buffer_target_ms` | 100 | Recommended depth while software gain is not 1.0 on PCM (DoP and native DSD always keep the deep target). Nothing is flushed when the depth changes: see `writable_bytes`. |
| `require_verified_rate` | `true` | Refuse to stream unless the DAC confirms the sampling frequency. Off: an acknowledged SET whose read-back is missing or zero is admitted on every device (UAC1, UAC2 and UAC3), and the stream is never bit-perfect. On, that admission needs `Quirk::SkipClockValidation`, `ClockReadbackPolicy::AllowUnusableAfterSuccessfulSet` or the source-backed `SkipSampleRateReadback` rule, which is UAC1-only. A multiplied clock, DSD, `Quirk::RequireVerifiedRate` and a positive different rate are refused either way. |
| `clock_readback_policy` | `RequireVerified` | `AllowUnusableAfterSuccessfulSet` admits a missing or zero PCM read-back after a successful SET, as above; the stream stays unverified. |
| `async_fallback_policy` | `FullSpeedUac1` | Host-paced nominal timing for full-speed UAC1 PCM without a feedback pipe; `Forbid` refuses it. See "Asynchronous endpoints without feedback". |
| `quirk_policy` | `VerifiedOnly` | `SourceBacked` also applies the implemented, exact-identity ALSA facts of the source-backed table; that is not hardware verification. |
| `adaptive_queue` | `true` | Deepen the bus queue after lost audio, within the bounds in "The bus queue". Applies only to the default queue geometry without implicit feedback. |
| `clock_settle_delay_ms` | `None` | Wait after programming the clock. `None`: the device quirk, else 20 ms. |
| `dsd_big_endian` | `None` | Legacy native DSD byte order on the wire. `None`: the device quirk, else little-endian. Standard UAC3 DSD stays little-endian. |
| `dsd_subslot` | `None` | Legacy native DSD subslot size (1, 2, 4). `None`: the device quirk, else the descriptor. Standard UAC3 DSD uses 8 bytes. |
| `padded_24bit` | `None` | Prefer the 4-byte subslot for 24-bit PCM. `None`: the device quirk, else off. |
| `quirks` | empty | `QuirkOverride { vendor_id, product_id, name_contains, quirks: Vec<Quirk> }`, added to the built-in table. |
| `feedback_smoothing` | `FeedbackSmoothing::Direct` | Explicit feedback uses each accepted selected fractional report directly. `FeedbackSmoothing::Ewma` retains the legacy alpha-0.2 filter; implicit cadence is unaffected. JSON: `"direct"` or `"ewma"`. |
| `allow_unsynchronized_async` | `false` | Allow an otherwise valid asynchronous endpoint without usable feedback to stream at the nominal rate. |
| `disable_feedback` | `false` | Disable both feedback modes for this session. The asynchronous nominal option still applies. |
| `capture_feedback_trace` | `false` | Enable the bounded USB feedback support capture for streams opened by this session. |
| `realtime_priority` | `true` | `SCHED_FIFO` (nice -16 fallback) for the output thread. Android only. |
| `hand_back` | `RestoreOriginal` | `RestoreOriginal`: write the DAC's original volume back on close. `Keep`: leave it, and remember the original for the next `open` in this process. |
| `startup_volume` | `Attenuate { max_db: -40.0 }` | What `open` does with the hardware volume before any stream can start: `Attenuate { max_db }` lowers every channel to `min(its level, max_db)`, `Keep` leaves it. See section 8. |
| `require_hardware_volume` | `true` | `open_stream` fails with `Error::VolumeUnavailable` when the DAC has no usable hardware volume control. `negotiate` and `capabilities` work either way. |
| `cancel_in_flight_on_flush` | `true` | `flush()` also cancels the transfers already on the bus (see `flush()`). Off: they play out. |
| `bus_queue_ms` | 16 | Audio kept queued on the USB bus, 4 to 64. One rule for every service interval and encoding (see "The bus queue"). |
| `transfers_in_flight` | 8 | Isochronous slots, 2 to 16. A ceiling when `transfer_period_us` is set. |
| `transfer_period_us` | None | Optional target for one USB transfer, 125–32,000 µs. The driver derives the slot count from the queue target and endpoint cadence. |

`DacConfig::with_usb_queue_profile` sets the queue knobs together. `Balanced`
uses 16 ms across eight slots, `LowLatency` targets 8 ms with 1 ms transfers,
and `LowCpu` uses 16 ms across four slots. For a custom period, the driver
rounds down to whole endpoint service intervals and keeps at least two slots.
The 32-packet per transfer and 255-packet in-flight limits still apply.

#### Flush and the bus

A flush empties the ring, but the transfers already submitted would still play:
up to `bus_queue_ms` of the old position after a seek or a track change. With
`cancel_in_flight_on_flush` (default) the output thread notices the flush
within about one transfer, cancels every transfer on the bus that was filled
before it, waits until libusb has handed each one back (the buffers stay alive
until then; if one never returns within a second the stream fails with a
`transfer` error naming how many), and refills the slots with wire-correct
silence (zeros for PCM, the idle pattern for DSD, DoP markers alternating)
until the ring holds the start threshold again.

- Cancelled transfers are not lost frames, not underruns, do not latch
  bit-perfect false and are not counted in `played_frames()` or
  `frames_completed`; `completed_transfers` does not count them either.
- A transfer that had already finished when the cancel reached the controller
  is accounted as played normally (and, being from before the flush, not as
  music of the new position).
- Transfers filled after the flush are never cancelled.
- Nothing is cancelled unless the bus carries music of the old position. A
  flush while paused, or while the stream is still priming, or with only held
  silence queued, leaves the bus alone, and `pause()` itself never cancels: the
  DAC stays locked on silence.
- Explicit feedback polls and implicit IN timing transfers stay active across
  a playback flush.
- A DAC may notice the short gap in the isochronous stream a cancel makes. DoP
  and native DSD DACs re-detect the stream and can click or re-lock; if a DAC
  does, set `cancel_in_flight_on_flush = false`: the queue then plays out, as
  it did before.

#### The bus queue

The output thread keeps `transfers_in_flight` isochronous transfers submitted
at all times. A transfer carries `bus_queue_ms / transfers_in_flight` of audio
(2 ms by default), rounded to whole service intervals, at least one packet and
at most 32. Time on the bus is therefore the same for every DAC and every
encoding (PCM, DoP, native DSD), where the service interval used to decide it:

| Service interval | Packets x transfers | Queued on the bus |
|---|---|---|
| 125 us | 16 x 8 | 16 ms |
| 250 us | 8 x 8 | 16 ms |
| 500 us | 4 x 8 | 16 ms |
| 1000 us | 2 x 8 | 16 ms |

A DAC with a 1 ms service interval used to queue 128 ms, which made a pause or
a seek eight times slower to be heard, and DSD at 125 us queued 32 x 8 = 256
packets. **Packets in flight never reach 256**: on the phone and DAC where this
was measured, 255 or fewer always played and exactly 256 always stalled. The
cap is enforced in code, and it wins over the configuration: with
`bus_queue_ms = 64` and eight transfers at 125 us each transfer would need 32
packets (256 in all), so it carries 31 and 31 ms is queued. A deeper queue
rides out a later output thread; a shallower one makes a pause and a stop
sound sooner. Stability ("stable" in the log) is judged over about 256 ms of
completed transfers for every geometry, and a stream is stopped after about
6 ms of consecutive lossy transfers (never fewer than three), whatever their
size. For an isochronous explicit endpoint, the polling batch targets a window
of `min(16 ms, bus_queue_ms)`, rounded down to whole intervals and clamped to
one to 32 packets. When the window fits two batches, the driver splits it
between two transfers so the endpoint can keep polling while the worker
processes the previous completion. The log line
`[USB] Isochronous queue:` states the resulting geometry.

**Adaptive depth.** With `adaptive_queue` on (the default), the default
geometry (`bus_queue_ms` 16, eight transfers, no `transfer_period_us`) and no
implicit feedback, the worker changes only the number of active transfers; the
packets per transfer stay as above. A completion batch that lost audio adds two
slots; each 30 s without loss removes one, never below the configured eight. The
ceiling is the smallest of 16 slots, fewer than 256 packets in flight and 64 ms.
With the default 2 ms transfers that is 15 slots, 30 ms, at a 125 us service
interval, and 16 slots, 32 ms, at 250 us, 500 us and 1 ms; the 64 ms bound is
not reached. Any other geometry, a queue profile other than `Balanced`, or
implicit feedback keeps the configured depth.

`Quirk` values: `SkipClockValidation`, `IgnoreInvalidSampleRate`,
`RequireVerifiedRate`, `SettleDelayMs(u64)`, `PreferPadded24BitTransport`,
`DsdSubslotSize(u8)`, `DsdBigEndian`, `DsdBitReverse`,
`ExplicitFeedbackPerServiceInterval`, and the source-backed values
`ControlMessageDelayUs(u64)`, `InterfaceDelayMs(u64)`, `SetInterfaceFirst`,
`PlaybackUrbFixup`, `SkipSampleRateReadback`, `SkipImplicitFeedback`,
`MixerPlaybackMinMute`, `MixerPlaybackLinearVolume`, `RawDsd32Be`, `FixedRate`,
`ImplicitSilenceOnEmpty`, `SkipClockSelectorWrites`,
`IgnoreOptionalControlErrors`, `TxLengthPrefix` and `PlaybackFirst`. The last
two act only from the scoped source-backed table: in a Rust `QuirkOverride` they
have no effect, and the JSON config rejects them (section 11). `Quirk`
is `#[non_exhaustive]`. Config fields set explicitly win over quirks. The built-in table has two entries (MOONDROP Dawn
Pro; Fosi Audio DS2), both verified in the project this was extracted from.

### `Capabilities`

```rust
Capabilities {
    uac_version: UacVersion,          // V1 | V2 | V3 | Unknown (the highest version among the alternate settings)
    link_speed: LinkSpeed,            // Low | Full | High | Super | SuperPlus | Unknown
    alt_settings: Vec<AltSetting>,
    pcm_sample_rates: Vec<u32>,       // union over PCM alternate settings
    pcm_bit_depths: Vec<u8>,
    channel_counts: Vec<u16>,
    supports_pcm: bool,
    supports_dop: bool,               // feasible PCM carrier: >= 24 valid bits at >= 176.4 kHz; not proof of marker decoding
    dop_max_dsd_bit_rate: Option<u32>,
    supports_native_dsd: bool,
    native_dsd_bit_rates: Vec<u32>,
    has_explicit_feedback: bool,
    volume: Option<VolumeRange>,
    has_hardware_volume: bool,        // volume.is_some(): a usable control exists
    has_mute: bool,
}
AltSetting { interface_number, alt_setting, endpoint_address, format: AltFormat /*Pcm|Dop|Dsd|Other*/,
             channels, subslot_bytes, valid_bits, rate_ranges: Vec<RateRange>, sample_rates,
             sync_type: SyncKind, feedback_endpoint: Option<u8>, max_packet_bytes,
             service_interval_us, uac_version,
             endpoint_interval /* bInterval */, usage: EndpointUsage /*Data|Feedback|FeedbackData|Reserved*/,
             refresh /* bRefresh */, synch_address /* bSynchAddress, 0 if the feedback endpoint was found by usage */,
             pacing: Option<Pacing> /* how a stream here would be paced; None if it would not be streamed */ }
```

UAC2 clock rates come from the Clock Source's `GET_RANGE`. A UAC2 subrange
with MIN below MAX and a resolution of 0 is ambiguous; the library keeps only
its MIN and logs it. UAC3 range reads are used as returned, so on UAC3 such a
subrange offers no rate.

### `StreamRequest`, `StreamFormat`

```rust
StreamRequest { sample_rate, channels, bit_depth, encoding: Encoding, rate_policy: RatePolicy }
StreamRequest::pcm(rate, channels, bits)
StreamRequest::dop(dsd_bit_rate, channels)          // 24-bit carrier, rate = dsd_bit_rate / 16
StreamRequest::dsd_native(dsd_bit_rate, channels)   // sample_rate and bit_depth ignored
StreamRequest::pcm_u8(rate, channels)                // Type I unsigned 8-bit
StreamRequest::pcm_float32(rate, channels)           // Type I IEEE-754 binary32
Encoding::{ Pcm, PcmU8, PcmFloat32, DoP, DsdNative { bit_rate } }
RatePolicy::{ Exact (default), NearestSupported }
```

`Encoding` is an exhaustive enum: it is not `#[non_exhaustive]`, so a new
variant in a later release is a breaking change for a `match` without a
wildcard arm. A PCM request needs an alternate setting whose valid bits equal
`bit_depth`; the library does not pick a deeper one.

`StreamFormat` is what will go on the wire: `sample_rate` (PCM rate, DoP
carrier rate, or for native DSD the rate of subslot-sized words, `bit_rate / (8 *
subslot_bytes)`), `channels`, `valid_bits`, `subslot_bytes`, the interface,
alternate setting and endpoint, `sync_type`, `explicit_feedback`, `link_speed`,
`service_interval_us`, and two read-only diagnostics, `pacing` and `clock`
(next section). Helpers: `bytes_per_frame()` (`subslot_bytes * channels`,
the unit of `write`), `counter_rate_hz()` (the unit of `played_frames` and
`latency_frames`: the wire frame rate, or `bit_rate / 8` for native DSD),
`rate_differs_from_request()`. It cannot be built by hand; pass the one
`negotiate` returned.

### Clock and pacing diagnostics

Two facts about a stream that explain "wrong pitch" and "clicks" reports, exposed
so a developer can read them instead of reconstructing them from the log. They
describe what the library already does; reading them changes nothing.

```rust
StreamFormat { ..., pacing: Pacing, clock: Option<ClockChoice> }
AltSetting   { ..., pacing: Option<Pacing> }

Pacing::{ ExplicitFeedback { endpoint: u8 },
          ImplicitFeedback { interface_number: u8, alt_setting: u8, endpoint: u8 },
          Adaptive, Synchronous, Unsynchronized }
ClockChoice { control_interface: u8, source_id: u8, path: Vec<ClockHop>, resolution: ClockResolution }
ClockHop { id: u8, kind: ClockHopKind }
ClockHopKind::{ Terminal, Selector { selected_pin: u8, pin_count: u8 }, Multiplier,
                RateMultiplier { numerator: u16, denominator: u16 }, Source }
ClockResolution::{ Resolved, FallbackFirstSource }
```

**`StreamFormat::clock`** is the Clock Source the sample rate was set and verified on,
as resolved by `negotiate`, and how it was reached. `control_interface` is the
AudioControl interface the clock requests go to and `source_id` the Clock Source
entity ID. `path` is the walk in the order followed: the stream's terminal
(`Terminal`, its terminal ID), then each clock entity (a `Selector` with the 1-based
pin that is active out of `pin_count`, a `Multiplier`, or a `RateMultiplier` when the
exact ratio was read back from the device), ending at the `Source`
(its `id` is `source_id`). A selector with a single input reports pin 1 of 1 and is
not asked; with several, the pin is what the device reported. `resolution` is
`Resolved` when the walk succeeded and `FallbackFirstSource` when it could not (no
terminal link, a malformed or looping entity, a selector that would not report its
pin), in which case the first Clock Source in descriptor order was used and `path`
holds that source alone. `None` when the device describes no clock entities (UAC
1.0 sets the rate on the endpoint). The clock is resolved again when the stream
opens, on the same terminal; a DAC whose selector is switched between the two calls
(a front-panel clock switch) can differ from what `negotiate` reported.

**`pacing`** names what the transport does for the chosen endpoint:

| Endpoint | `Pacing` |
|---|---|
| Asynchronous, explicit feedback endpoint resolved | `ExplicitFeedback { endpoint }` (the feedback endpoint's address; it is polled) |
| Asynchronous, shared-clock IN stream resolved | `ImplicitFeedback { interface_number, alt_setting, endpoint }` (the capture source supplies frame counts) |
| Adaptive | `Adaptive` (a declared feedback endpoint is ignored) |
| Synchronous | `Synchronous` (likewise) |
| Asynchronous, no feedback endpoint, admitted by the narrow default policy or the broader `allow_unsynchronized_async` opt-in | `Unsynchronized` (also the `UnsynchronizedAsync` blocker; never bit-perfect) |
| Declares no synchronisation | `Unsynchronized` (nominal rate; not a bit-perfect blocker today) |

It is a label, not a mode selector: no pacing is added or chosen by it. On
`StreamFormat` it is what the stream uses; on `AltSetting` it is what a stream on
that alternate setting would use, and `None` when the library would not stream it
with this configuration (an asynchronous endpoint without usable feedback and without an applicable nominal policy, or a declared feedback endpoint that cannot be resolved).

`Pacing`, `ClockResolution` and `ClockHopKind` are `#[non_exhaustive]`; each has a
`code()` with the stable lower-case identifier the JSON uses.

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
stream.queued_frames() -> u64               // frames in the ring (cheap)
stream.stats() -> StreamStats
stream.is_bit_perfect() -> bool
stream.bit_perfect_blocker() -> Option<BitPerfectBlocker>   // why not, if not
stream.set_software_gain(f32) -> Result<()>
stream.software_gain() -> f32
stream.set_host_processing(bool)            // the host processes the audio: shallow ring
stream.host_processing() -> bool
```

- `write` never blocks. It takes whole wire frames, as many as fit under the
  ring's capacity, and returns the bytes taken. A short count means the ring is
  full: keep the remainder and retry. Input shorter than one frame is not taken.
  After `drain()` (until `flush()`) and once the stream has closed or failed it
  returns `InvalidState`.
- `writable_bytes()` is the room up to the target depth (the shallower
  `processing_buffer_target_ms` while software gain is on), a whole number of
  frames, and 0 after `drain()`. It is the number a pull loop should write; it is
  not a hard limit. When the target drops (software gain turned on), a ring that
  is already deeper than the new target is not flushed: `writable_bytes()` is 0
  until the output has drained it below the target, so the change is heard after
  at most the old depth. When the target rises, the room reappears at once.
- A new stream plays by itself once the ring holds `start_threshold_ms` of
  audio. `play()` is only needed after `pause()`.
- `pause()` sends wire-correct silence (zero for PCM, the idle pattern for DSD,
  DoP markers keep alternating) so the DAC stays locked, and does not consume
  the ring. Submitted transfers may still play first. The default starts at eight slots and about 16 ms; the adaptive queue can deepen it to 30 or 32 ms (see "The bus queue").
  `play()` continues exactly where the ring was.
- `flush()` drops everything in the ring, resets `played_frames()` to 0
  immediately, clears a pending `drain()`, and holds silence until the ring has
  the start threshold again. It returns at once; the output thread then cancels
  the transfers already on the bus (see "Flush and the bus" below). Use it for
  seeks and for switching tracks.
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
- `set_host_processing(true)` tells the stream that the host processes the audio
  (EQ, DSP, crossfade, pitch) before writing it, so a change the host makes is
  heard after the ring's fill. `writable_bytes()` then asks for the shallow
  `processing_buffer_target_ms`, as it does for software gain (and, as for gain,
  nothing is flushed when the target changes). It is only a buffering hint: it
  changes no sample and, since the library cannot see what the host did to the
  audio, it does not touch `is_bit_perfect()`. DoP and native DSD ignore it.

`StreamState`: `Buffering`, `Playing`, `Paused`, `Draining`, `Drained`,
`Closed`, `Failed` (codes 0 to 6).

`StreamStats` carries: `state`, `bit_perfect`, `software_gain`, `underruns`,
`lost_frames`, `short_packets`, `zero_packets`, `urb_timeouts`,
`completed_transfers`, `frames_written`, `frames_completed`, `played_frames`,
`latency_frames`, `latency_ms`, `queued_frames`, `buffer_ms`,
`buffer_target_ms`, `active_target_ms` (the depth `writable_bytes()` asks for
now), `buffer_capacity_ms`, `transfer_queue_depth`,
`last_turnaround_us`, `max_turnaround_us`, `last_submit_gap_us`,
`max_submit_gap_us`, `max_worker_gap_us`, `max_worker_loop_gap_us`, `max_event_pump_elapsed_us`, `max_event_pump_timeout_overrun_us`, `effective_rate_hz`, `feedback_active`,
`feedback_locked`, `feedback_empty`, `feedback_reports`, `feedback_timeouts`, `feedback_errors`,
`feedback_rejected`, `feedback_age_ms`, `feedback_rate_hz`, `feedback_stale`,
`stable` (lossless transfers for about 256 ms, no underrun, no lost audio),
`clock_verified` (the clock has current verified evidence; an acknowledged `SET_CUR` alone is not proof), `payload_verified` (the wire payload audit passes) and
`payload_constant_for_ms` (how long the payload has been failing the audit, while
it does). `max_worker_gap_us` and the 10/50 ms worker-gap counters measure work outside the event pump. `max_worker_loop_gap_us` is observed elapsed loop time; `max_event_pump_elapsed_us` measures event-pump duration, and `max_event_pump_timeout_overrun_us` records time beyond its requested wait. These observations do not identify OS scheduling as the cause. `feedback_locked` and `feedback_active` report live lock for either
feedback mode. A successful empty explicit poll increments `feedback_empty`
and leaves the last accepted rate and timestamp unchanged. Report age keeps
increasing. Frame counters are in `counter_rate_hz()` units, except implicit
`implicit_measured_frames` and `implicit_scheduled_frames`, which count USB
wire frames. Implicit timing stats also report FIFO depth and high-water mark,
waits, empty OUT queue events, overruns, IN completions, out-of-order
completions and invalid IN packets (`implicit_fifo_depth`,
`implicit_fifo_high_water`, `implicit_waits`, `implicit_empty_bus`,
`implicit_overruns`, `implicit_in_transfers`, `implicit_out_of_order`,
`implicit_invalid_packets`).

### `Error`

| Variant | `code()` | When |
|---|---|---|
| `Busy` | `busy` | This attachment already has an owner, or its previous owner is still shutting down; or a `reconfigure_stream` drain is in progress on this DAC. |
| `PlatformUnsupported` | `platform_unsupported` | Not Linux or Android (on macOS, every entry point except `open_darwin`). |
| `Claim(String)` | `claim` | Bad descriptor, interfaces not claimed as required, libusb could not take the descriptor. |
| `HostUsb { operation, kind, detail }` | `host_usb` | macOS only: the libusb open, an interface claim or the whole-device capture of `open_darwin` failed. `kind` is `AccessDenied`, `InUse`, `DeviceLost`, `NotSupported` or `Other`; it does not by itself mean a DAC fault. |
| `Unsupported { reason, offered }` | `unsupported` | No alternate setting carries the request; `offered` lists what the DAC has. |
| `ImplicitFeedbackUnsupported` | `implicit_feedback_unsupported` | The request needs an asynchronous route and no compatible feedback source is available. See below. |
| `ClockUnverified { requested_hz, reported_hz }` | `clock_unverified` | The DAC did not confirm the rate and `require_verified_rate` is on, or it reported a different rate. |
| `DeviceLost` | `device_lost` | Unplugged or stopped answering. |
| `Transfer(String)` | `transfer` | An isochronous transfer or the stream machinery failed. |
| `Control(String)` | `control` | A class control request (clock, volume, mute) failed. |
| `InvalidState(String)` | `invalid_state` | The call is not valid now (closed, drained, second stream, ...). |
| `InvalidArgument(String)` | `invalid_argument` | Out of range or malformed argument. |
| `VolumeUnavailable` | `volume_unavailable` | `open_stream` on a DAC with no usable hardware volume control while `require_hardware_volume` is on. The host asks the user to allow fixed-level output and opens the DAC again with `require_hardware_volume = false`. |
| `ReconfigureDrainTimeout` | `reconfigure_drain_timeout` | `reconfigure_stream` did not drain before its deadline. The original stream stays open and draining; no alternate setting or clock was changed. |
| `ReconfigureUnusable(String)` | `reconfigure_unusable` | Retirement, the retained USB owner or the rollback of a reconfiguration failed. Close the DAC before using it again. |

`Error` is `#[non_exhaustive]`. `Display` is `"<code>: <message>"`.

### Asynchronous endpoints without feedback

An asynchronous OUT endpoint runs on the DAC's own clock. Explicit feedback
reports that rate on a dedicated IN endpoint. For UAC2 implicit feedback, the
driver uses whole-frame counts from an associated IN audio endpoint with the
same resolved clock source, wire rate and service interval (UAC2, or UAC3 with a
marked input). It primes the IN transfer queue before submitting playback.
A gap in the IN timing before the cadence has qualified ends the stream. Once
it is trusted, one missing IN pattern is tolerated while the cadence
requalifies; a second gap before that, or no requalification within 100 ms plus
the queued bus time, ends the stream with an `implicit_xrun` `Transfer` error,
and the stream has to be opened again. A
playback-only session does not give the IN samples to the app; a duplex stream
hands them to its capture reader from the same transfers. On Android, the SDK
returns capture-only claims when the session closes.

By default, full-speed UAC1 PCM without a declared feedback pipe may use host-paced nominal timing. `AsyncFallbackPolicy::Forbid` disables this narrow case; `DacConfig::allow_unsynchronized_async` separately opts into a broader eligible fallback. Other unsupported routes are refused with `Error::ImplicitFeedbackUnsupported`. The format reports
`sync_type == Asynchronous` and `pacing == Pacing::Unsynchronized`, and
`is_bit_perfect()` is false with `bit_perfect_blocker() ==
Some(UnsynchronizedAsync)` for the whole stream. An endpoint that declares a
feedback endpoint the library cannot resolve is malformed and is never streamed,
whatever the setting.

### Explicit report compatibility

Full-speed feedback uses frames per USB frame and high-speed feedback uses
frames per USB microframe. The driver scales the rate to the OUT service
interval and checks that the largest packet needed by fractional scheduling fits
the endpoint. It uses the newest valid report in each batch. Empty or rejected
reports do not extend the feedback deadline. A stale explicit source clears
live lock and emits `FeedbackStale`. Playback can hold the last valid rate
until a timely report restores lock or the stream fails with `Error::Transfer`.
[Timing thresholds](https://johnneerdael.github.io/usbdac/decisions-clock.html#feedback)

For four-byte high-speed reports, the driver first tests the USB microframe
interpretation. If it is outside the nominal-rate tolerance and post-alt
`GET_CUR` independently confirmed the clock, the driver can test the OUT
service interval. Three separate completions must support that interpretation
and fit the OUT packet capacity. The driver remembers learned units for the
same USB route while the DAC session stays open. A later stream still checks
its clock and validates a fresh report. A valid standard report takes
precedence, and feedback lock starts fresh on every stream. The feedback
poll interval defines when to read a report.

`Quirk::ExplicitFeedbackPerServiceInterval` can force the alternate units
when a trace establishes them and clock readback is unavailable. Scope the
override to the affected VID/PID. The [Linux UAC2 gadget implementation](https://github.com/torvalds/linux/blob/028ef9c96e96197026887c0f092424679298aae8/drivers/usb/gadget/function/u_audio.c#L100-L147)
illustrates the alternate units.

### USB feedback support capture

`DacStream::explicit_feedback_diagnostics()` returns the selected endpoint,
poll geometry, libusb status counts, accepted report count and the latest
sample per outcome. Without opt-in, the snapshot excludes raw packet bytes.

Enable `DacConfig::capture_feedback_trace` before opening a session.
`Dac::feedback_trace()` returns the last stream's format, explicit summary,
timestamped observations and terminal failure, even after that stream is
closed. The trace keeps the first 128 and most recent 2,048 observations.
It counts observations discarded due to retention or concurrent export.
Explicit raw bytes are capped at eight per packet. PCM, DSD and implicit
input payloads are excluded. Copy the report before closing the DAC or
opening another stream.

The Android SDK method `DacSession.feedbackTraceJson()` calls
`dacFeedbackTrace` and retains a final report on that object after close.
The JSON object has `schema_version`, `driver_version`, `usb`, `format`,
`explicit`, `entries`, `dropped_entries` and `failure`. Each entry has an
`event` name and `elapsed_us` relative to stream creation. For completions
and packet metadata, libusb status values are 0 completed, 1 error, 2 timed
out, 3 cancelled, 4 stall, 5 no device and 6 overflow. The summary's status
array uses index 7 for unknown or unreported values. Submit entries contain
the return code from `libusb_submit_transfer`. `explicit` is null for other
pacing modes. The `timebase` event and summary identify the selected units
and whether the driver inferred them. The summary's `reused_timebase` field
indicates reuse from the open DAC session. Before a captured stream opens, `dacFeedbackTrace` returns JSON
`null`.

For an in-app capture flow and Kotlin and Rust export examples, see
[collect a USB playback support report](sdk/support-diagnostics.md).

### Descriptor parsing

`usbdac_core::descriptors` exposes the UAC 1.0, 2.0 and 3.0 descriptor parsers the
transport uses (`AudioControlParser`, `AudioStreamingParser`, `DescriptorIter`,
`parse_*` functions and the descriptor types). They work on byte slices and need
no device. They are lower level than the rest and may change before 1.0.

### Capture, duplex and routing

Standalone and paired PCM capture, and standard routing controls, are separate from
`Dac::open` and `open_stream`. Each takes its own claims from the host.

```rust
Dac::capture_preflight(fd, &DeviceInfo) -> Result<(u8, Vec<u8>, Vec<u8>)>   // (configuration, control interfaces, input interfaces)
Dac::open_capture_device(fd, DeviceInfo, DacConfig) -> Result<Dac>          // input only, no playback claim, no volume step
dac.open_capture(&CaptureRequest, capacity_frames) -> Result<DacCaptureStream>

Dac::open_duplex_device(fd, DeviceInfo, DacConfig) -> Result<Dac>           // playback claims plus every input and control interface of capture_preflight
dac.open_duplex(&StreamFormat, &CaptureRequest, capacity_frames, listener) -> Result<(DacStream, DacCaptureStream)>

CaptureRequest { sample_rate, channels, valid_bits, channel_map: Option<ChannelMap> }
CaptureFormat::{ sample_rate(), valid_bits(), subslot_bytes(), channel_map(), channels(), bytes_per_frame() }
capture.read(&mut [u8]) -> Result<usize>     // whole interleaved frames; 0 when active and empty
capture.available_frames() / stats() / state() / last_error() / close()
```

- A capture request is exact linear PCM: the rate, the channel count and the valid
  bits must match what the DAC offers, and an optional `ChannelMap` must match too.
  There is no rate substitution, conversion or remix. The returned format carries the
  device's own slot size, so padding bytes pass through unchanged.
- `ChannelMap` is the ordered channel identity (`ChannelPosition`) from the
  descriptors: the terminal's channel configuration for UAC 1.0, the AS channel
  cluster for UAC 2.0, the high-capability cluster for UAC 3.0. Unnamed channels keep
  their zero-based wire index. Nothing is reordered.
- `CaptureStats` keeps the causes of missing input apart: `missing_frames` and the
  failed, short, empty and malformed packet counters describe the bus,
  `overrun_frames` is audio dropped because the reader was too slow,
  `proof_suppressed_frames` counts valid frames withheld during startup and rate proof,
  and `device_position_frames()` includes those frames as a lower bound from configuration.
  The presentation timeline begins after the suppressed region. Standalone capture drops
  oldest queued frames on overflow; duplex drops newly arriving frames when its bounded
  owned-frame pool is full. `contention_frames` is retained for compatibility.
  `CaptureState` is `Active`, `Stopped` or `Failed`.
- In duplex, one USB worker runs both directions. Closing either direction stops
  both. A capture fault also stops playback, unless capture sits on a separate
  AudioControl interface with its own directly resolved clock; only then does
  playback continue after the capture side fails. A disconnect always stops both. Two directions on one clock must use the same rate; directions on separately
  proven clocks keep their own rates and counters. When the playback uses implicit
  feedback from an IN stream that capture also reads, they share one transfer pool.
- While a standalone capture stream is open, concurrent playback, duplex and routing
  changes are refused.

```rust
dac.routing_discover() -> Result<RoutingSnapshot, RoutingSessionError>
dac.routing_read(&RoutingSnapshot, ControlKey) -> Result<RouteValue, RoutingSessionError>
dac.routing_apply(&RoutingSnapshot, &[RouteEdit]) -> Result<RouteReport, RoutingSessionError>
dac.routing_discover_powered() / routing_read_powered(..) / routing_apply_powered(..)
```

- Routing covers the standard UAC selector units and mixer crosspoints of the claimed
  AudioControl interfaces in the active configuration (`usbdac_core::routing`). Every pin
  and channel is one-based, in wire order. A mixer crosspoint is a gain in 1/256 dB,
  and `i16::MIN` is silence.
- It works only on an idle DAC. Each edit carries the expected current value, so a stale
  edit fails before anything is written; every write is read back and a failed
  transaction restores what it changed. Rediscover after a selector edit. The configured
  `HandBack` policy decides whether routes are restored on close.
- An apply that changed a control, or whose rollback failed, discards the session's
  negotiated format and its hardware volume route: `open_stream` with an earlier
  `StreamFormat` fails with `InvalidState` ("call negotiate first"), and `volume()` is
  `None` until `negotiate` has found the playback route again. Negotiate after every
  routing change.
- The `_powered` variants wake the affected UAC 3.0 power domains for the call, and put
  the checked original power state back afterwards. The plain variants never wake a domain.
- Vendor routing (`djm_routing_*`, `scarlett_*`, `fcp_routing_*`) is in the crate for
  source compatibility. Since 0.1.5, and still in 0.1.11, its calls return
  `RoutingSessionError::UnsupportedTopology` or report no capability. Vendor PCM profiles
  are switched off in the same way.

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
divided by 16. DoP uses PCM framing, including on UAC3; it does not select
the native DSD format. `supports_dop` and `dop_max_dsd_bit_rate` describe a
feasible PCM carrier, not proof that the device decodes the markers. See the
[DoP Open Standard](https://www.dsd-guide.com/sites/default/files/white-papers/DoP_openStandard_1v1.pdf).

**Native DSD.** Raw DSD bytes, most significant bit first (the oldest bit in bit
7), one byte per channel per step, interleaved: `[ch0, ch1, ch0, ch1, ...]`. A
wire frame is `subslot_bytes * channels` bytes (one wire word per channel); the
library groups the consecutive bytes of a channel into the wire word. Standard
[UAC3 DSD](sdk/references.md#ref-uac3-formats) uses 8-byte, 64-bit words per channel at `bit_rate / 64`: the oldest
bit is D63, and the word is emitted little-endian (LE64), so the eight
chronological input bytes of each channel appear in reverse byte order on the
wire. Legacy native DSD uses 1-, 2- or 4-byte words and retains its device
quirks and `dsd_big_endian`/`dsd_subslot` overrides, including optional bit
reversal; those controls do not change the standard UAC3 layout. DSF field `1` is LSB-first and needs per-byte reversal; field `8` is MSB-first and does not. Deblock each channel, interleave channels chronologically and trim to the declared sample count. Raw DSDIFF Channel Bytes are MSB-first in Channels Chunk order; exclude IFF padding and reject unsupported compression. DSF field `1` byte `0x01`, DSF field `8` byte `0x80`, and raw DSDIFF byte `0x80` carry the same chronological bits. Request it with
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
| `FeedbackStale` | The feedback source stopped supplying usable timing. The bit-perfect verdict clears. Explicit pacing can recover during a bounded hold of the last good rate. Persistent failure ends the stream. |
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
  reads), `negotiate` and `open_stream` (100 ms or more each), `close` (the last
  transfers are cancelled and handed back: typically a few milliseconds), every `VolumeControl` call (control
  transfers, 1 s timeout each), `stats()` and `is_bit_perfect()` take one
  short lock but also touch session state.
- The library starts two threads: `usbdac-output` (the isochronous loop; with
  `realtime_priority` it asks for `SCHED_FIFO` priority 2 on Android) and
  `usbdac-events`. It never calls into the host from the output thread.
- Do not write from two threads at once: `write` is thread-safe but the byte
  stream you write is one ordered sequence.

## 7. Bit-perfect output

`DacStream::is_bit_perfect()` is true only while all of these hold:

1. The stream is open and its output thread is running.
2. No frame was lost on the bus since the stream started. The first lost frame
   clears the flag for the life of the stream.
3. The format on the wire matches the request.
4. Live feedback has locked when the stream uses explicit or implicit feedback.
5. The DAC's sampling frequency has current verified readback or another proven clock condition. An acknowledged SET with unusable readback remains unverified, including when a compatibility quirk permits it.
6. Software gain is exactly 1.0.
7. The endpoint uses valid pacing. An asynchronous route opened with the
   nominal-timing opt-in remains unverified.
8. The wire payload audit passes. For PCM the audit needs real audio. Channel 0
   must vary within a transfer, with a 1.5 s grace so digital silence or a
   fade does not flip the flag. For DoP the markers must alternate and agree
   across channels. For native DSD the data must not be all zero.

`bit_perfect_blocker()` returns the first condition that fails, as a
`BitPerfectBlocker`: `NoStream`, `LostFrames`, `FormatChanged`, `FeedbackUnlocked`,
`ClockUnverified`, `SoftwareGain`, `UnsynchronizedAsync`, `PayloadUnverified`
(codes `no_stream`, `lost_frames`, `format_changed`, `feedback_unlocked`, `clock_unverified`,
`software_gain`, `unsynchronized_async`, `payload_unverified`), and `None` when
the stream is bit-perfect. `StreamStats::bit_perfect_blocker` carries the same.

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
writable volume (UAC 1.0, 2.0 or 3.0, master control or all channels). It works with
or without a stream, as long as the `Dac` is open: closing a stream, or a stream
that failed, leaves the control, its ranges and the cached level and mute in
place, and a write that is in flight is not failed by the stream stopping. The
route changes only when a later stream plays through another Feature Unit (the
cached level is then read again) or when the `Dac` closes. The control is found through
the first PCM playback terminal at open and looked up again if a stream plays
through another terminal.

```rust
volume.range() -> Result<VolumeRange>             // { min_db, max_db, res_db }
volume.get() -> Result<VolumeState>               // { db, fraction, muted: Option<bool> }
volume.set_db(f64) -> Result<VolumeState>
volume.set_fraction(f64) -> Result<VolumeState>
volume.step(direction: i8, steps: u8) -> Result<VolumeState>   // steps == 0 means 16
volume.set_mute(bool) -> Result<VolumeState>
volume.startup() -> StartupVolumeReport           // what open did to the level
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
  (before any write, so before the start-up attenuation), if all channels
  agreed. `hand_back()` writes it back, each channel its own original, and ends
  the record. With `HandBack::RestoreOriginal` this happens at close; with
  `Keep` the record survives in the process so a later `open` of the same device
  still knows the true original.
- `VolumeChanged` events are sent for changes made through this library, not
  for a physical knob; call `get()` to poll (it sends the event if the level
  differs from the last one it saw).

### Start-up volume protection

A DAC keeps the level it had, and many default to full scale. `Dac::open`
therefore protects the first audio. With the default
`StartupVolume::Attenuate { max_db: -40.0 }`, once the hardware volume control
is found:

1. each controlled channel's level is read and recorded first, as the original
   (what `hand_back` and a `RestoreOriginal` close restore);
2. each channel is lowered to `min(its level, max_db)` and every channel is read
   back and compared with what was meant, all before `open` returns and so
   before any stream can be opened;
3. the result is the `StartupVolumeReport` of `volume.startup()`, and a
   `VolumeChanged` event follows when something was lowered.

A channel is **never raised**: one already below the ceiling, or at the DAC's
silent value, is left alone. `max_db` is rounded **down** to a step the DAC
supports; if the DAC's minimum is above it, the channels go to the minimum and
`clamped_to_minimum` says so. Channels that end at different levels are brought
to the quietest, because the control reads and writes one level (a channel's
own original is still what hand-back restores). `StartupVolume::Keep` leaves
the level alone. The step runs once per `open` and per volume route: a level the
host sets afterwards is never pulled back, a stream that opens later does not
repeat it, and a stream that plays through another Feature Unit gets the same
protection for that unit, with its own recorded original.

If a write or the read-back fails, the attenuation is not verified: the control
is treated as **unusable**. `dac.volume()` is `None`,
`Capabilities::has_hardware_volume` is false, the failure is logged, and what the
library may have changed stays on record for hand-back.

```rust
StartupVolumeReport {
    original_db: Vec<f64>,   // per channel, before the library touched it
    applied_db: Vec<f64>,    // per channel, now
    limit_db: f64,           // the quietest original channel
    attenuated: bool,        // applied is below original on some channel
    clamped_to_minimum: bool,
    max_db: Option<f64>,     // the ceiling asked for; None for Keep
}
```

`limit_db` is the cap for a host that restores a level it remembered for this
DAC: it should not go above it without the user's action, because that is how
loud the DAC itself was when it connected. What a host does on connect:

1. read `volume.startup()` and show it if it wants to say the level was lowered;
2. restore its remembered per-DAC level capped by `limit_db`
   (`min(remembered, limit_db)`), or leave the attenuated level if it remembers
   none;
3. let the user raise it from there.

`VolumeChanged` (with the attenuated level) goes to the first listener set with
`Dac::set_listener` when the open lowered something: nothing can listen before
`open` returns, so it is held until then. A re-open that finds the DAC already
at the ceiling (after `HandBack::Keep`) lowers nothing and sends nothing.

**Without a usable hardware volume** (none found, or the step above failed),
`open_stream` fails with `Error::VolumeUnavailable` while
`DacConfig::require_hardware_volume` is true, once the stream's own route is
known to have none and before the output thread starts or the stream's clock is
programmed. `open`, `negotiate` and `capabilities` still work, so the host can
show what the DAC offers, ask the user whether to allow fixed-level output, and
open the DAC again with `require_hardware_volume = false`.

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
| Seek | Under the same lock the writer uses: reposition the decoder, discard the chunk rendered for the old position, `stream.flush()`, remember the new start time. The old position stops within a transfer or two: the bus is cancelled too. |
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

    // Library
    @JvmStatic external fun version(): String
    @JvmStatic external fun setLogLevel(level: Int)            // 0 off .. 5 trace; logcat tag "usbdac"

    // Used by the validator app only
    @JvmStatic external fun validatorBegin(directory: String)
    @JvmStatic external fun validatorRun(dac: Long, directory: String): String
    @JvmStatic external fun validatorCancel()
    @JvmStatic external fun validatorDeviceLost()
    @JvmStatic external fun validatorFlush()

    // Device: preflight, open, close, capabilities, negotiation, events, feedback capture
    @JvmStatic external fun baddDescriptorWitness(fd: Int, activeId: Int, snapshot: ByteArray): Int
    @JvmStatic external fun dacFeedbackPreflight(fd: Int, deviceInfoJson: String): String
    @JvmStatic external fun dacCapturePreflight(fd: Int, deviceInfoJson: String): String
    @JvmStatic external fun dacOpenCaptureDevice(fd: Int, deviceInfoJson: String, configJson: String?): Long
    @JvmStatic external fun dacOpenDuplexDevice(fd: Int, deviceInfoJson: String, configJson: String?): Long
    @JvmStatic external fun dacOpen(fd: Int, deviceInfoJson: String, configJson: String?): Long
    @JvmStatic external fun dacClose(dac: Long)
    @JvmStatic external fun dacAwaitRetirement(): Boolean
    @JvmStatic external fun dacRetirementToken(dac: Long): Long
    @JvmStatic external fun dacAwaitRetirementToken(token: Long): Boolean
    @JvmStatic external fun dacRetirementRoutingReport(token: Long): String
    @JvmStatic external fun dacReleaseRetirementToken(token: Long)
    @JvmStatic external fun dacAwaitConnectionRetirement(fd: Int): Boolean
    @JvmStatic external fun dacCapabilities(dac: Long): String
    @JvmStatic external fun dacFeedbackTrace(dac: Long): String
    @JvmStatic external fun dacNegotiate(dac: Long, requestJson: String): String
    @JvmStatic external fun dacNegotiateCandidates(dac: Long, requestsJson: String): String
    @JvmStatic external fun dacSetListener(dac: Long, listener: Any?)

    // Playback stream
    @JvmStatic external fun streamOpen(dac: Long, formatId: Long, listener: Any): Long
    @JvmStatic external fun streamReconfigure(dac: Long, current: Long, requestJson: String, timeoutMs: Long, listener: Any): String
    @JvmStatic external fun streamReconfigureCandidates(dac: Long, current: Long, requestsJson: String, timeoutMs: Long, listener: Any): String
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
    @JvmStatic external fun streamSetHostProcessing(stream: Long, active: Boolean)

    // Capture and duplex
    @JvmStatic external fun duplexOpen(dac: Long, formatId: Long, requestJson: String, capacityFrames: Int, listener: Any): String
    @JvmStatic external fun captureOpen(dac: Long, requestJson: String, capacityFrames: Int): Long
    @JvmStatic external fun captureFormat(capture: Long): String
    @JvmStatic external fun captureStats(capture: Long): String
    @JvmStatic external fun captureRead(capture: Long, buffer: ByteBuffer, offset: Int, count: Int): Int
    @JvmStatic external fun captureClose(capture: Long)

    // Hardware volume
    @JvmStatic external fun volumeRange(dac: Long): String?     // null: no hardware volume
    @JvmStatic external fun volumeGet(dac: Long): String
    @JvmStatic external fun volumeSetDb(dac: Long, db: Double): String
    @JvmStatic external fun volumeSetFraction(dac: Long, fraction: Double): String
    @JvmStatic external fun volumeStep(dac: Long, direction: Int, steps: Int): String
    @JvmStatic external fun volumeSetMute(dac: Long, muted: Boolean): String
    @JvmStatic external fun volumeOriginalDb(dac: Long): Double  // NaN: none recorded
    @JvmStatic external fun volumeHandBack(dac: Long): Boolean
    @JvmStatic external fun volumeStartup(dac: Long): String?   // null: no hardware volume

    // Standard routing
    @JvmStatic external fun routingDiscoverPowered(dac: Long): String
    @JvmStatic external fun routingReadPowered(dac: Long, snapshot: Long, keyJson: String): String
    @JvmStatic external fun routingApplyPowered(dac: Long, snapshot: Long, editsJson: String): String
    @JvmStatic external fun routingDiscover(dac: Long): String
    @JvmStatic external fun routingRead(dac: Long, snapshot: Long, keyJson: String): String
    @JvmStatic external fun routingApply(dac: Long, snapshot: Long, editsJson: String): String

    // Vendor controls (switched off since 0.1.5; still off in 0.1.11)
    @JvmStatic external fun dacVendorPcmPreflight(fd: Int, deviceInfoJson: String): String
    @JvmStatic external fun dacVendorPcmClaimsPreflight(fd: Int, deviceInfoJson: String): String
    @JvmStatic external fun pioneerDjmDiscover(dac: Long): String
    @JvmStatic external fun pioneerDjmApply(dac: Long, editJson: String): String
    @JvmStatic external fun scarlettPreflight(fd: Int, deviceInfoJson: String): String
    @JvmStatic external fun scarlettCapability(dac: Long): String
    @JvmStatic external fun scarlettRead(dac: Long): String
    @JvmStatic external fun scarlettGen1Read(dac: Long): String
    @JvmStatic external fun scarlettGen1ReadSnapshot(dac: Long): String
    @JvmStatic external fun scarlettGen1Apply(dac: Long, snapshot: Long, editsJson: String): String
    @JvmStatic external fun fcpRead(dac: Long): String
    @JvmStatic external fun fcpReadSnapshot(dac: Long): String
    @JvmStatic external fun fcpApply(dac: Long, snapshot: Long, editsJson: String): String
    @JvmStatic external fun scarlettMuxRead(dac: Long): String
    @JvmStatic external fun scarlettMuxApply(dac: Long, snapshot: Long, editsJson: String): String
    @JvmStatic external fun scarlettMixApply(dac: Long, editJson: String): String
}
```

The exported symbols are `Java_dev_usbdac_internal_Native_<name>`.

- `dacNegotiate` returns the negotiated format as JSON including a `format_id`;
  pass that id to `streamOpen`. Formats are kept per DAC (the latest 16) until
  the DAC is closed.
- `dacFeedbackPreflight` returns the active configuration ID and candidate
  implicit feedback interfaces without starting audio. The Kotlin SDK uses
  the result before `dacOpen`.
- `dacCapturePreflight` returns `{ "configuration_id", "control_interfaces",
  "input_interfaces" }` for the active configuration, and `dacOpenCaptureDevice`
  and `dacOpenDuplexDevice` register a device opened for capture only or for
  playback with capture. `captureOpen` returns a capture handle, `duplexOpen`
  returns `{ "playback": <stream handle>, "capture": <capture handle> }`, and
  `captureFormat` and `captureStats` return JSON. `captureRead` takes a direct
  buffer and returns the bytes copied, whole frames only, 0 when the stream is
  active and empty.
- `dacAwaitRetirement` returns true when every USB transfer of the session has
  retired. The host must keep its connection and claims until it does.
  `dacRetirementToken` takes a token for one DAC owner before `dacClose`;
  `dacAwaitRetirementToken` waits on that owner only, so another open DAC does
  not delay it, `dacRetirementRoutingReport` returns its routing hand-back once it
  has retired, and `dacReleaseRetirementToken` frees the token once the owner has
  retired (before that it fails with `invalid_state`).
  `dacAwaitConnectionRetirement` waits on the connection's own descriptor, for
  example after a failed open. Each wait is bounded at about 500 ms; call again
  until it returns true.
- `dacNegotiateCandidates` takes a JSON array of one to eight exact PCM requests
  (section 3, `negotiate_candidates`). `streamReconfigure` and
  `streamReconfigureCandidates` are `reconfigure_stream` and
  `reconfigure_stream_candidates`: they return `{ "stream_id", "format",
  "rolled_back", "error_code", "error_message" }`, and the old stream handle is
  replaced by `stream_id`.
- `baddDescriptorWitness` reads the active configuration and its raw descriptor
  on a duplicate of `fd`, with standard requests only, and returns a nonzero code
  when they prove one of the BADD cases of the checked playback open (section 2),
  else 0. The Kotlin
  SDK calls it before any claim. Never call it, or the Rust
  `usbdac_core::badd_descriptor_witness`, with the descriptor of a DAC that has
  an open session: a duplicate shares the kernel's URB queue, so the witness's
  libusb context could reap the stream's transfers. The function does not check
  this itself.
- `streamSetHostProcessing` is the buffering hint of `set_host_processing`.
- The routing functions take and return JSON; a snapshot is an opaque id that
  belongs to one open DAC, and at most eight are kept. The vendor functions
  (`pioneer*`, `scarlett*`, `fcp*`, `dacVendorPcm*`) are present and fail with an
  unsupported error or report no capability (since 0.1.5; still so in 0.1.11).
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
`claim`, `unsupported`, `implicit_feedback_unsupported`, `clock_unverified`, `device_lost`, `transfer`,
`control`, `invalid_state`, `invalid_argument`, `volume_unavailable`, `reconfigure_drain_timeout`,
`reconfigure_unusable` (section 3). In an `Error` event the two reconfiguration codes, like any
code without a number in the table below, arrive as `a` = 0. Malformed JSON and
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
| 6 | Error | error code: 1 busy, 2 platform_unsupported, 3 claim, 4 unsupported, 5 clock_unverified, 6 device_lost, 7 transfer, 8 control, 9 invalid_state, 10 invalid_argument, 11 implicit_feedback_unsupported, 12 volume_unavailable | 0 | `usbdac:<code>: <message>` |
| 7 | DeviceLost | 0 | 0 | null |
| 8 | CompatibilityFallback | tier code | 0 | reason |
| 9 | NominalAsyncHostStarvation | host ring empty duration in ms | 0 | null |

NominalAsyncHostStarvation occurs once per continuous host-ring starvation episode on nominal asynchronous output; refill, pause, or drain resets the episode. It does not measure the DAC FIFO or its drift. The host chooses whether to pause, re-prime, seek, or select another route.

Unknown future events arrive with `type` -1 and a debug string in `message`.

## 11. JSON schemas

Keys are snake_case. `null` stands for "none". Unknown request keys are
ignored.

**Device info** (`dacOpen`):

```json
{ "vendor_id": 12230, "product_id": 61546, "product_name": "MOONDROP Dawn Pro",
  "manufacturer_name": "MOONDROP", "device_name": "/dev/bus/usb/001/004",
  "claimed_interfaces": [0, 1, 2], "configuration_id": 1,
  "unavailable_feedback_interfaces": [] }
```

`vendor_id` and `product_id` are required; the rest is optional.
The preflight response is `{ "configuration_id": 1, "interfaces": [2] }`.

**Config** (`dacOpen`, nullable; every key optional, defaults as in section 3):

```json
{ "buffer_target_ms": 250, "buffer_capacity_ms": 500, "start_threshold_ms": 100,
  "processing_buffer_target_ms": 100, "require_verified_rate": true,
  "clock_settle_delay_ms": 20, "dsd_big_endian": true, "dsd_subslot": 4,
  "padded_24bit": true, "allow_unsynchronized_async": false, "disable_feedback": false,
  "realtime_priority": true, "capture_feedback_trace": false,
  "feedback_smoothing": "direct", "adaptive_queue": true, "quirk_policy": "verified_only",
  "async_fallback_policy": "full_speed_uac1", "clock_readback_policy": "require_verified",
  "hand_back": "restore_original", "bus_queue_ms": 16, "transfers_in_flight": 8,
  "transfer_period_us": null, "cancel_in_flight_on_flush": true,
  "startup_volume": "attenuate", "startup_volume_max_db": -40.0, "require_hardware_volume": true,
  "quirks": [ { "vendor_id": 1, "product_id": 2, "name_contains": "Foo",
                "quirks": ["skip_clock_validation", {"settle_delay_ms": 80}] } ] }
```

`feedback_smoothing` is `direct` or `ewma`; `quirk_policy` is `verified_only` or `source_backed`. `async_fallback_policy` is `full_speed_uac1` or `forbid`; `clock_readback_policy` is `require_verified` or `allow_unusable_after_successful_set`. Unknown values are rejected. The source-backed tier imports only implemented, scoped facts and does not establish physical verification. `hand_back` is `restore_original` or `keep`. `startup_volume` is `attenuate`
(default; `startup_volume_max_db` is its ceiling, -40.0 when absent) or `keep`
(which ignores the ceiling); a `startup_volume_max_db` alone means `attenuate`. Flag quirks:
`skip_clock_validation`, `ignore_invalid_sample_rate`, `require_verified_rate`,
`prefer_padded_24bit_transport`, `dsd_big_endian`, `dsd_bit_reverse`,
`explicit_feedback_per_service_interval`, `set_interface_first`, `playback_urb_fixup`, `skip_sample_rate_readback`, `skip_implicit_feedback`, `mixer_playback_min_mute`, `mixer_playback_linear_volume`, `raw_dsd32_be`, `fixed_rate`, `implicit_silence_on_empty`, `skip_clock_selector_writes`, and `ignore_optional_control_errors`. Valued quirks are one-entry objects: `{"settle_delay_ms": n}`, `{"control_message_delay_us": n}`, `{"interface_delay_ms": n}`, and `{"dsd_subslot_size": n}`. Generic host overrides `tx_length_prefix` and `playback_first` are rejected; their scoped source-backed profiles have separate admission. Unknown quirks are rejected.

**Stream request** (`dacNegotiate`):

```json
{ "encoding": "pcm", "sample_rate": 44100, "channels": 2, "bit_depth": 16, "rate_policy": "exact" }
{ "encoding": "dop", "dsd_bit_rate": 2822400, "channels": 2 }
{ "encoding": "dsd_native", "dsd_bit_rate": 2822400, "channels": 2, "rate_policy": "nearest" }
```

`encoding` is `pcm` (default), `pcm_u8`, `pcm_float32`, `dop` or `dsd_native`; `rate_policy` is `exact`
(default) or `nearest`. `nearest` has no effect for `dop` and `dsd_native`: they are always exact. For `dop`, `dsd_bit_rate` derives the carrier rate (rate
/ 16, 24-bit); an explicit `sample_rate` or `bit_depth` overrides it.

**Capabilities** (`dacCapabilities`):

```json
{ "uac_version": "v2", "link_speed": "high",
  "pcm_sample_rates": [44100, 48000, 96000], "pcm_bit_depths": [16, 24, 32],
  "channel_counts": [2],
  "supports_pcm": true, "supports_dop": true, "dop_max_dsd_bit_rate": 11289600,
  "supports_native_dsd": false, "native_dsd_bit_rates": [],
  "has_explicit_feedback": true,
  "volume": { "min_db": -127.0, "max_db": 0.0, "res_db": 1.0 },
  "has_hardware_volume": true, "has_mute": true,
  "alt_settings": [ { "interface_number": 1, "alt_setting": 1, "endpoint_address": 1,
      "format": "pcm", "channels": 2, "subslot_bytes": 4, "valid_bits": 32,
      "rate_ranges": [ { "min_hz": 44100, "max_hz": 44100, "step_hz": 0 } ],
      "sample_rates": [44100], "sync_type": "asynchronous", "feedback_endpoint": 129,
      "max_packet_bytes": 1024, "service_interval_us": 125, "uac_version": "v2",
      "pacing": { "mode": "explicit_feedback", "feedback_endpoint": 129, "implicit_feedback": null } } ] }
```

`uac_version`: `v1`, `v2`, `v3`, `unknown`. `link_speed`: `low`, `full`, `high`,
`super`, `super_plus`, `unknown`. `format`: `pcm`, `dop`, `dsd`, `other`. `sync_type`:
`asynchronous`, `adaptive`, `synchronous`, `none`. An alternate setting's `pacing`
is a pacing object (below), or `null` when the library would not stream it with
this configuration. It is an additive key: documents from earlier versions lack it.

**Negotiated format** (`dacNegotiate`):

```json
{ "format_id": 7, "encoding": "pcm", "dsd_bit_rate": null, "sample_rate": 44100,
  "channels": 2, "valid_bits": 16, "subslot_bytes": 2, "bytes_per_frame": 4,
  "counter_rate_hz": 44100, "interface_number": 1, "alt_setting": 1,
  "endpoint_address": 1, "sync_type": "asynchronous", "explicit_feedback": true,
  "link_speed": "high", "service_interval_us": 125, "rate_differs_from_request": false,
  "pacing": { "mode": "explicit_feedback", "feedback_endpoint": 129, "implicit_feedback": null },
  "clock": { "control_interface": 0, "source_id": 5, "resolution": "resolved",
             "path": [ { "id": 1, "kind": "terminal", "selected_pin": null, "pin_count": null },
                       { "id": 9, "kind": "selector", "selected_pin": 2, "pin_count": 2 },
                       { "id": 5, "kind": "source", "selected_pin": null, "pin_count": null } ] } }
```

`pacing` and `clock` are additive keys (documents from earlier versions lack
them). Pacing modes are `explicit_feedback`, `implicit_feedback`, `adaptive`,
`synchronous`, and `unsynchronized`. `feedback_endpoint` gives the endpoint
address for explicit feedback. `implicit_feedback` gives the IN source tuple
for implicit feedback and is `null` in the other modes.

```json
{ "mode": "implicit_feedback", "feedback_endpoint": null,
  "implicit_feedback": { "interface_number": 2, "alt_setting": 3, "endpoint": 130 } }
```

`clock` is `null` when the device describes no clock
entities; otherwise `resolution` is `resolved` or `fallback_first_source`, `path`
lists the hops in the order followed (terminal first, Clock Source last, whose `id`
equals `source_id`), and a hop's `kind` is `terminal`, `selector`, `multiplier` or
`source`. `selected_pin` (1-based) and `pin_count` are set for `selector` hops and
`null` for the others. A `multiplier` hop whose ratio was read back also carries
`numerator` and `denominator`. A reader should treat an unknown `mode`, `resolution` or
`kind` as "unknown", not as an error.

**Stream statistics** (`streamStats`) includes the `StreamStats` fields in
section 3. `state` is a numeric code. `bit_perfect`, `feedback_active`,
`feedback_locked` and `feedback_stale` are booleans. `bit_perfect_blocker` is a
code string or `null`. `software_gain`, `latency_ms`, `effective_rate_hz` and
`feedback_rate_hz` are floating-point numbers. `feedback_age_ms` and
`feedback_rate_hz` are `null` until an age or rate estimate is available.

**Volume range**: `{ "min_db": -127.0, "max_db": 0.0, "res_db": 1.0 }`.
**Start-up report** (`volumeStartup`):
`{ "original_db": [-10.0, -70.0], "applied_db": [-70.0, -70.0], "limit_db": -70.0,
"attenuated": true, "clamped_to_minimum": false, "max_db": -40.0 }` (`max_db` is
`null` for `keep`).
**Volume state**: `{ "db": -40.0, "fraction": 0.5, "muted": false }` (`muted` is
`null` when the DAC has no readable mute).

## 12. Limits of v0.1

- One owner per physical attachment (a second owner is `Busy`); distinct attachments can have independent owners.
- Linux and Android (usbfs through libusb's wrap-by-descriptor call). On macOS
  the crate builds and its logic is unit-tested; `Dac::open`, capture and duplex
  return `PlatformUnsupported`, and only `Dac::open_darwin` plays, through libusb,
  without recorded hardware evidence. Windows is not supported.
- UAC 1.0, 2.0 and 3.0 playback include PCM and native DSD; standard
  UAC3 DSD has a fixed 8-byte transport word. PCM capture and duplex use
  explicit input APIs and claims. Standard routing requires verified
  topology and device capabilities. Additional vendor controls are
  tracked in the [device compatibility candidates](https://github.com/johnneerdael/android-usbdac/blob/main/docs/design/device-compatibility-candidates.md).
  See the
  [integration surface](https://github.com/johnneerdael/android-usbdac/blob/main/docs/design/alsa-parity.md#reference-integration-surface).
- Vendor PCM profiles, vendor routing and device-specific feedback and DSD
  rules are written and tested on the host and switched off (since 0.1.5,
  still in 0.1.11); the device profiles that are active by default are the two
  built-in quirks and the AP80 Pro-X and MOONDROP Dawn Pro native DSD layouts.
  `QuirkPolicy::SourceBacked` adds the implemented source-backed facts on request.
- Direct PCM and legacy native DSD have limited physical coverage on the
  OnePlus 15 / AP80 Pro-X, with additional PCM evidence from the DC04U and
  from Flick. New parity paths remain subject to the documented physical
  acceptance cases; the virtual playback check covers synchronous UAC1 PCM
  through Linux usbfs. See [evidence and status](sdk/evidence.md).
- The AAudio output (section 13) is not part of the Rust API of `usbdac-core`; it
  lives in the JNI crate, is Android only, and has never opened a stream on a
  device.

## 13. AAudio exclusive output

A second output next to the USB transport, for a device whose DAC is internal (a
digital audio player). It is not a USB feature and has nothing to do with
`Dac`: the JNI crate asks Android for an exclusive-mode AAudio stream at the
source's own rate, channel count and sample format, and feeds it the bytes the
host writes, unchanged. It is the object `dev.usbdac.internal.AAudioNative`,
separate from `Native`; the Kotlin class on top is `AAudioExclusiveOutput`.

**What it promises.** No conversion, resampling, dithering or scaling: the
bytes written are the bytes AAudio is handed, and the stream is requested with
`AAUDIO_SHARING_MODE_EXCLUSIVE`, the exact rate and channel count and the
requested format, direction output, usage media, content type music. **What it
cannot promise.** An app cannot see past AAudio, so the output is never
reported bit-perfect-verified. Section 13.4 lists the reasons it reports.

### 13.1 Lifecycle and functions

```
aaudioOpen(requestJson, listener, apiLevel) -> handle    open the stream (not started)
aaudioGrant(handle) -> json                              what the platform granted, and the verdict
  (the host decides: refuse a shared stream, or go on)
aaudioStart(handle)                                      request the start; the stream plays silence
aaudioWrite / WritableBytes / Play / Pause / Flush / Drain / IsDrained
aaudioState / PlayedFrames / LatencyFrames / Xruns / Underruns
aaudioClose(handle)                                      stop and release
```

```kotlin
package dev.usbdac.internal

internal object AAudioNative {
    init { System.loadLibrary("usbdac") }

    @JvmStatic external fun aaudioOpen(requestJson: String, listener: Any, apiLevel: Int): Long
    @JvmStatic external fun aaudioGrant(handle: Long): String
    @JvmStatic external fun aaudioStart(handle: Long)
    @JvmStatic external fun aaudioClose(handle: Long)
    @JvmStatic external fun aaudioWrite(handle: Long, buffer: ByteBuffer, offset: Int, count: Int): Int
    @JvmStatic external fun aaudioWritableBytes(handle: Long): Int
    @JvmStatic external fun aaudioPlay(handle: Long)
    @JvmStatic external fun aaudioPause(handle: Long)
    @JvmStatic external fun aaudioFlush(handle: Long)
    @JvmStatic external fun aaudioDrain(handle: Long)
    @JvmStatic external fun aaudioIsDrained(handle: Long): Boolean
    @JvmStatic external fun aaudioState(handle: Long): Int
    @JvmStatic external fun aaudioPlayedFrames(handle: Long): Long
    @JvmStatic external fun aaudioLatencyFrames(handle: Long): Long
    @JvmStatic external fun aaudioXruns(handle: Long): Int
    @JvmStatic external fun aaudioUnderruns(handle: Long): Long
}
```

The exported symbols are `Java_dev_usbdac_internal_AAudioNative_<name>`. Handles
are opaque ids from the same counter as every other handle. `listener` is any
object with `onEvent(type: Int, a: Long, b: Double, message: String?)` (the
`NativeListener` interface); the library keeps a global reference until the
output is closed. `apiLevel` is `Build.VERSION.SDK_INT`. Failures are
`IllegalStateException("usbdac:<code>: <message>")` as in section 10.

### 13.2 Request

```json
{ "sample_rate": 96000, "channels": 2, "format": "i24", "device_id": 7,
  "performance_mode": "low_latency", "buffer_target_ms": 250,
  "buffer_capacity_ms": 500, "start_threshold_ms": 100, "device_buffer_bursts": null }
```

`sample_rate` (8000 to 768000), `channels` (1 to 8) and `format` are required.
`format` is `i16` (signed 16-bit), `i24` (signed 24-bit packed in 3 bytes),
`i32` (signed 32-bit) or `float` (32-bit IEEE); all little-endian and
interleaved. `device_id` is `AudioDeviceInfo.getId()`, 0 for the platform's
choice. `performance_mode` is `none`, `power_saving` or `low_latency` (the
default). The buffer keys default as shown; `buffer_target_ms` must be at most
`buffer_capacity_ms` and at least `start_threshold_ms`, and the ring must fit in
64 MiB. `device_buffer_bursts` (null: the platform's default) sizes the device
buffer in bursts. A request that fails these checks is `invalid_argument`.

The format needs an Android release: `i16` and `float` need API 26 (AAudio
itself), `i24` and `i32` need API 31. Below that the open fails with
`unsupported` and a message naming both API levels. Nothing is converted to make
a request fit.

### 13.3 Grant

```json
{ "sharing": "exclusive", "sample_rate": 96000, "channels": 2,
  "format": "i24", "format_raw": 3,
  "frames_per_burst": 96, "buffer_capacity_frames": 960, "buffer_size_frames": 192,
  "device_id": 7, "performance_mode": "low_latency", "mmap": true,
  "hardware": null,
  "as_requested": true, "verdict": "exclusive",
  "reason": "Exclusive mode was granted at the requested rate, format and channel count: the app mixer is bypassed, but the device's audio HAL is not verifiable from an app." }
```

Everything but the last three keys is read back from the stream after it opened.
`mmap` (the memory-mapped path exclusive streams use) is `null` below Android 9.
`hardware` is `{ "sample_rate", "channels", "format", "format_raw" }` on Android
14 (API 34) and later, where AAudio reports what the device itself runs at, and
`null` before. `format` is `null` for a format this library does not carry.

### 13.4 The verdict

`as_requested` is true when the stream is exclusive and nothing below differs.
`verdict` names the most serious difference, and `reason` is the sentence the
Kotlin side puts in `NotVerified`. The differences, in the order they are
reported:

| `verdict` | Meaning | `aaudioOpen` |
|---|---|---|
| `exclusive` | exclusive, at the requested rate, format and channel count, and (on API 34+) the hardware agrees | succeeds |
| `shared` | the platform granted a shared stream: the system mixer is in the path | succeeds; the host may refuse |
| `rate_changed` | the stream runs at another rate than requested | fails `unsupported` |
| `format_changed` | the stream's format is another than requested | fails `unsupported` |
| `channels_changed` | the stream has another channel count | fails `unsupported` |
| `hardware_rate`, `hardware_format`, `hardware_channels` | (API 34+) the hardware runs another rate, format or channel count than the stream, so AAudio converts before the device | succeeds |

A stream whose rate, format or channel count differs from the one the ring holds
would play the bytes at the wrong speed or as noise, so it is never started: the
open fails and the stream is closed. A shared stream and a conversion before the
hardware are playable and are reported instead. With several differences the
`reason` lists them all, separated by `;`.

The output is **never** verified. Even when `verdict` is `exclusive`, what the
audio HAL and the device do with the samples (volume, effects, resampling in
firmware) cannot be seen from an app.

### 13.5 Playing

The stream is opened with a data callback. AAudio's own buffer is a few
milliseconds deep, which no host loop that sleeps for a few milliseconds can
keep full, so the output puts a ring of `buffer_capacity_ms` in front of it and
the callback (on AAudio's real-time thread, without locks or allocation) copies
from the ring. The host sees the same push model as the USB stream:

- `aaudioWrite` takes whole frames, never blocks, and returns the bytes taken
  (a short count means the ring is full). It reads `count` bytes from `offset` of
  a direct buffer and does not move the buffer's position. After `aaudioDrain`
  (until `aaudioFlush`) it fails `invalid_state`; after a disconnect
  `device_lost`.
- `aaudioWritableBytes` is the room up to `buffer_target_ms`, a whole number of
  frames, and 0 after drain or failure.
- Nothing plays until the ring holds `start_threshold_ms`; the stream then runs
  and the state goes from buffering to playing. The device is fed silence until
  then, while paused, and after an underrun. An underrun (the ring ran dry
  while playing) is counted by `aaudioUnderruns` and playback waits for the start
  threshold again instead of stuttering.
- `aaudioPause` plays silence and keeps the ring and the stream open
  (and exclusive); `aaudioPlay` continues where the ring was. A long pause holds
  the device: close the output instead.
- `aaudioFlush` drops the ring (the callback applies it within one burst),
  resets `aaudioPlayedFrames` to 0 at once, and re-arms the start threshold. Audio
  the device already holds still plays and is not counted. It also clears a drain.
- `aaudioDrain` declares the end of the audio: the ring plays out, a tail
  shorter than the start threshold included, and the padded end is not an
  underrun. `aaudioIsDrained` is true when everything written since the last
  flush has been presented by the device.
- `aaudioState` is 0 buffering, 1 playing, 2 paused, 3 draining, 4 drained,
  5 closed, 6 failed, as for a stream.

**Position.** `aaudioPlayedFrames` is the number of music frames the device has
presented since the start or the last flush, in the stream's rate. The device is
fed silence as well as music, so the count of music frames among the frames
AAudio has presented is kept separately: the callback counts the music and the
silence it hands over, and the position is `min(music, presented - silence handed
over before the last music frame)`. `presented` is `AAudioStream_getTimestamp`
extrapolated to now while the stream runs (never past `AAudioStream_getFramesRead`;
an implausible timestamp is ignored), else the frames read. The result lags the
audible position by at most the device's own buffer, never leads it, is
monotonic between flushes, and does not move while paused. `aaudioLatencyFrames`
is the ring plus the music the device holds.

`aaudioXruns` is AAudio's own count of device-side underruns
(`AAudioStream_getXRunCount`); `aaudioUnderruns` counts the ring running dry.

### 13.6 Events and errors

`onEvent(type, a, b, message)` from AAudio's error-callback thread:

| `type` | Event | `a` | `message` |
|---|---|---|---|
| 0 | the route was disconnected (device unplugged, routing changed) | the AAudio error code (`-899`) | text |
| 1 | another AAudio error | the AAudio error code | text |

Both mark the output failed (state 6, writes `device_lost`) before the event is
sent. The library never closes the stream from the callback; the host closes it.
No event is delivered after `aaudioClose` returns.

AAudio result codes map to the library's codes: `DISCONNECTED` is `device_lost`;
`ILLEGAL_ARGUMENT`, `OUT_OF_RANGE`, `NULL` are `invalid_argument`;
`INVALID_STATE`, `INVALID_HANDLE` are `invalid_state`; `UNIMPLEMENTED`,
`INVALID_FORMAT`, `INVALID_RATE` are `unsupported`; `UNAVAILABLE`,
`NO_FREE_HANDLES` (another app holds the device) are `busy`; everything else is
`transfer`. Off Android `aaudioOpen` fails with `platform_unsupported`. The Kotlin layer adds one code of its own,
`exclusive_unavailable`, when `requireExclusive` is on and the grant is shared.

### 13.7 Loading

`libaaudio.so` is opened with `dlopen` on the first `aaudioOpen` and every
function is resolved with `dlsym`. The library therefore has no link-time
dependency on AAudio and loads on every Android release the SDK supports; the
functions that exist only on newer releases (usage and content type, API 28; the
MMAP query, API 28; the hardware format, API 34) are optional, and without them
the grant simply lacks that information. If `libaaudio.so` cannot be opened the
open fails with `unsupported`.

### 13.8 Threading

As for a stream (section 6): `aaudioWrite` and the getters never block and are
safe from any thread, with one writer; `aaudioOpen` and `aaudioClose` block (the
close for up to about half a second while the stream stops).

Nothing in this section has run on a device: the logic is covered by host tests
with a simulated device and the Kotlin layer by JVM tests with a fake native
layer, and the AAudio calls themselves have never opened a stream.

## 14. Conversion helpers

The direct USB path never converts anything. For a host that has audio in one
format and a DAC that runs another (a rate refused under
`RatePolicy::NearestSupported`, a depth the DAC lacks), `usbdac_core::convert`
plans, classifies and runs the conversion. It is pure Rust with no platform code,
it is opt-in, and **everything but the first two kinds below is not bit-perfect.**
It is exposed to Kotlin as `ConversionPlan`, `PcmConverter` and `ConvertingOutput`
through `dev.usbdac.internal.ConvertNative`.

### 14.1 Planning

```rust
ConversionPlan::between(&StreamRequest, &StreamFormat, &ConvertOptions) -> Result<ConversionPlan, ConvertError>
ConversionPlan::pcm(source: PcmFormat, target: PcmFormat, &ConvertOptions) -> Result<ConversionPlan, ConvertError>
plan.kind() -> ConversionKind          // None | BitExact | Lossless | Lossy
plan.description() -> &str             // a sentence for a status line
plan.steps() -> &[Step]
plan.preserves_bit_perfect() -> bool   // kind <= BitExact
PcmFormat { sample_rate, channels, sample: SampleFormat }
SampleFormat::{ Int { bits, slot_bytes }, Float32 }     // I16, I24, I24_IN_32, I32 constants
```

`between` takes the request the host made (its source samples are in the layout of
the request's `bit_depth`: 16, packed 24 or 32, unless
`ConvertOptions::source_sample` says float or 24-in-4) and the format `negotiate`
returned (its layout is `valid_bits` left-justified in `subslot_bytes`, section 4).
DoP and native DSD are refused (`Unsupported`): a rate or depth change would
destroy them. The minimal chain is chosen, in the order the converter runs it:
resample, then the sample step, then mono to stereo.

| `ConversionKind` | Steps in it | Bit-perfect |
|---|---|---|
| `None` | no step: the formats are the same, the bytes are copied | stays so |
| `BitExact` | `Widen` (more bits, the new low bits are zero, every value is unchanged) and `Repack` (the same bits in another slot: 24-bit packed to 24-bit in 4 bytes and back) | stays so: the stream is verified as it was |
| `Lossless` | `MonoToStereo` (the sample duplicated); `IntToFloat` of up to 24 bits (a float holds them exactly) | no: the audio is not identical |
| `Lossy` | `Narrow` (fewer bits), `Quantise` (a float, or resampled values, rounded to integers), `IntToFloat` of 32 bits (rounded to 24 significant bits), `Resample` | no |

The plan's kind is the largest of its steps'. Anything else than mono to stereo
is `Unsupported` for channels (no downmix, no upmix); rates are 8000 to 768000 Hz.

`ConvertOptions` (all with defaults): `narrowing` is `TpdfDither` (the default) or
`Truncate`; `quality` is `High` (default) or `Fast`; `headroom_db` (0 to -60,
default -3.5 dB, applied only after resampling); `dither_seed`; `source_sample`.

### 14.2 Running

```rust
let mut converter = Converter::new(&plan)?;
converter.process(input: &[u8], output: &mut [u8]) -> Result<Processed { consumed, produced }, ConvertError>
converter.finish(output: &mut [u8]) -> Result<usize, ConvertError>     // the resampler's tail; call until 0
converter.reset()                                                      // a new stream, or a seek
converter.delay_frames() -> usize          // source frames held back (0 unless resampling)
converter.min_output_bytes() -> usize      // the output size process makes progress with
converter.clipped_samples() -> u64
```

`process` takes whole source frames from the front of `input` and writes whole
target frames to `output`, as many as fit in either; the caller keeps what was not
consumed. The output buffer must hold at least `min_output_bytes()` (the frames one
source frame turns into). `finish` flushes the filter by feeding it silence, so the
total output for `N` input frames is exactly `ceil(N * L / M)` frames (14.4); after
it `process` fails `InvalidState` until `reset`. Chunking changes nothing: the
output is bit-identical whatever the chunk sizes (tested).

For signed integer input with a declared significant width smaller than its slot, every complete input buffer is checked for zero low padding bits before any output or state change. An invalid buffer fails with `InvalidArgument`; declaring 16 significant bits in a 32-bit slot permits exact repacking to 16-bit, while full 32-bit input to 16-bit remains lossy. A plan is exact conditional on that declared input contract; it does not infer precision from a silent prefix. Kotlin `ConvertingOutput` validates before forwarding each block, including plans that otherwise copy unchanged.

Everything the converter needs is allocated in `Converter::new`: `process`,
`finish` and `reset` do not allocate (tested with a counting allocator, for every
kind of plan). Integer-to-integer plans run on an integer engine without floating
point, so a `BitExact` plan is exact by construction; the others decode to `f64`,
which holds every integer layout exactly.

### 14.3 Narrowing, dither and clipping

Where bits are dropped (an integer narrowing, a float to integers, the rounding of
resampled values) `Truncate` rounds toward negative infinity, and `TpdfDither`
adds triangular-PDF noise of one step peak before rounding to nearest: the error
becomes signal-independent, zero-mean, at half a step RMS (-98 dBFS at 16 bits,
tested), at most 1.5 steps. The generator is a xorshift64*, seeded by
`dither_seed`: the same seed gives the same output, and `reset` rewinds it.

Integer outputs **saturate at full scale and never wrap**, and `clipped_samples()`
counts post-dither integer-quantizer saturations, including rounding or dither clamps even without resampling. A non-finite float input sample is replaced by silence.
Whether a resampled signal clips depends on the signal: a band-limited
reconstruction can exceed its input's peak. Measured: a full-scale 1 kHz square
wave resampled from 44.1 to 48 kHz peaks at 1.41 (+3.0 dB), and clips; a
full-scale sine does not (the passband ripple is 0.00004 dB, less than one 16-bit
step at full scale: tested for 100 Hz to 19 kHz at 16 bits with truncation). The
default is -3.5 dB after resampling, which covers the measured square wave but is not a guarantee for every signal: it reduces clipping and does not prevent it. A review probe with an adversarial input measured peaks of +3.18 dBFS (`High`) and +5.24 dBFS (`Fast`) after the default headroom; such samples saturate and are counted by `clipped_samples()`. Set `headroom_db = 0` explicitly for unity gain. Saturation remains counted, and headroom never applies to exact or widening paths.

### 14.4 The resampler

A polyphase windowed-sinc converter for any rational ratio, written from the textbook
design (the module documentation of `convert/resample.rs` has the derivation).
For rates `fi` and `fo` with `g = gcd`, `L = fo / g` and `M = fi / g`: output `n`
sits at input time `n * M / L`, and is the dot product of `T` input samples with
the coefficients of phase `(n * M) mod L`, a Kaiser-windowed sinc evaluated at
the exact fractional position, each phase normalised to sum to 1. Streaming with
per-channel history; chunks of any size.

- **Alignment and latency.** The filter is zero-phase: output frame `n` is aligned
  with source time `n * M / L` exactly, so a position needs no correction:
  `source_position = played_target_frames * fi / fo`. What the converter holds back is
  `T/2` source frames (`delay_frames()`, 1.8 ms at 44.1 kHz for `High`), the
  algorithmic latency, which `ConvertingOutput.latencyFrames` adds. `finish` pads with
  `T/2` zeros, which makes the total output `ceil(N * L / M)`.
- **Design targets.** `High`: passband to 91% of the lower Nyquist frequency
  (20.07 kHz at 44.1 kHz), stopband from the Nyquist frequency itself (so nothing
  aliases into the output), designed for 110 dB. `Fast`: passband to 90%, stopband
  from 110% of the Nyquist frequency (content between them aliases into the top
  10% of the band), designed for 84 dB, about 2.5 times cheaper (2.9 times fewer taps).
- **Limits.** Rates 8000 to 768000 Hz; a ratio is refused (`Unsupported`) if it needs
  more than 4.5 million coefficients (for the standard 44.1 and 48 kHz families
  up to 768 kHz the most is 410 000: 44.1 to 768 kHz is 2560 phases of 160 taps,
  768 to 44.1 kHz 147 phases of 2752 taps). A ratio that does not reduce to small
  integers (44.1 to 47.993 kHz would be 47993 phases) is refused.

**Measured quality** (`cargo test -p usbdac-core quality -- --nocapture` prints it;
tones through the converter, least-squares sine fits, all in `f64`). Passband
ripple is the peak-to-peak gain over 13 tones from 150 Hz to the passband edge;
"spur" is everything in the output except the tone (images, aliases, harmonics
and numerical noise), relative to the tone, the worst of those 13; stopband is
the weakest attenuation over 11 tones from just above the stopband edge to the
input's Nyquist frequency, where the whole output is alias (downsampling only).

| Pair | `High` ripple | `High` spur | `High` stopband | `Fast` ripple | `Fast` spur | `Fast` stopband |
|---|---|---|---|---|---|---|
| 44.1 to 48 kHz | 0.000011 dB | -120.7 dB | (up) | 0.00084 dB | -83.9 dB | (up) |
| 48 to 44.1 kHz | 0.000026 dB | -126.5 dB | 110.0 dB | 0.00045 dB | -94.0 dB | none above Nyquist |
| 44.1 to 96 kHz | 0.000011 dB | -120.7 dB | (up) | 0.00084 dB | -83.8 dB | (up) |
| 96 to 44.1 kHz | 0.000036 dB | -137.8 dB | 112.5 dB | 0.00076 dB | -105.2 dB | 84.0 dB |
| 96 to 48 kHz | 0.000012 dB | -246.5 dB | 109.2 dB | 0.00085 dB | -246.1 dB | 83.8 dB |
| 88.2 to 48 kHz | 0.000020 dB | -141.9 dB | 109.9 dB | 0.00082 dB | -104.7 dB | 84.4 dB |
| 192 to 44.1 kHz | 0.000036 dB | -144.1 dB | 112.5 dB | 0.00028 dB | -117.5 dB | 96.4 dB |
| 44.1 to 192 kHz | 0.000011 dB | -120.8 dB | (up) | 0.00084 dB | -83.8 dB | (up) |
| 384 to 48 kHz | 0.000037 dB | -247.6 dB | 112.3 dB | 0.00045 dB | -247.7 dB | 97.2 dB |
| 768 to 44.1 kHz | 0.000030 dB | -156.4 dB | 112.5 dB | not run | | |
| 44.1 to 768 kHz | 0.000007 dB | -120.8 dB | (up) | not run | | |

The tests assert stopband above 105 dB, spur below -110 dB and ripple below
0.0002 dB for `High`; stopband above 80 dB, spur below -80 dB and ripple below
0.002 dB for `Fast`. The gain at DC is 1 to within 1e-13 in every phase. An
impulse comes back at the same instant, symmetric. A 1 kHz tone at -6 dBFS from
44.1 kHz 16-bit to 48 kHz 24-bit comes out with its peak within 0.001 of 0.5 and
everything else below -90 dB (the 16-bit input's own rounding noise is about
-92 dB relative to that tone).

**Cost** (`cargo run --release -p usbdac-core --example convert_bench`, Apple
silicon, one core, stereo 24-bit): `High` converts 44.1 to 48 kHz at 385 times
real time (0.26% of a core), 96 to 44.1 kHz at 220 times, 44.1 to 192 kHz at 91 times,
44.1 to 768 kHz at 22 times and 768 to 44.1 kHz at 29 times (3.5% of a core). `Fast` is
2.3 to 2.6 times faster. Building a converter takes 0.7 ms (44.1 to 48 kHz), 2.7 ms
(44.1 to 192 kHz) and about 10 ms for the two extremes. Integer re-layouts run at
thousands of times real time; narrowing 32 to 16 bits with dither at 1550 times. A phone core
is several times slower than that machine; nothing here has run on one.

### 14.5 JNI contract

The object `dev.usbdac.internal.ConvertNative`, exported as
`Java_dev_usbdac_internal_ConvertNative_<name>`; handles are opaque ids; failures
are `IllegalStateException("usbdac:<code>: <message>")` with the codes
`unsupported`, `invalid_argument` and `invalid_state`.

```kotlin
internal object ConvertNative {
    init { System.loadLibrary("usbdac") }

    @JvmStatic external fun convertPlan(requestJson: String): String
    @JvmStatic external fun convertCreate(requestJson: String): Long
    @JvmStatic external fun convertClose(handle: Long)
    @JvmStatic external fun convertProcess(handle: Long, input: ByteBuffer, inOffset: Int, inCount: Int,
                                           output: ByteBuffer, outOffset: Int, outCapacity: Int): Long
    @JvmStatic external fun convertFinish(handle: Long, output: ByteBuffer, outOffset: Int, outCapacity: Int): Int
    @JvmStatic external fun convertReset(handle: Long)
    @JvmStatic external fun convertClipped(handle: Long): Long
}
```

`convertProcess` takes two **direct** buffers, which must not overlap, reads `inCount`
bytes from `inOffset` and writes at most `outCapacity` bytes at `outOffset`, moves
neither position, and returns `(consumed shl 32) or produced` in bytes.
`convertFinish` returns the bytes written. `convertCreate` synthesises the filter
(a few milliseconds for a resampling plan): call it off the main thread.
`convertPlan` does no audio work.

**Request** (`convertPlan`, `convertCreate`):

```json
{ "source": { "sample_rate": 44100, "channels": 2, "sample": { "kind": "int", "bits": 16, "slot_bytes": 2 } },
  "target": { "sample_rate": 48000, "channels": 2, "sample": { "kind": "int", "bits": 24, "slot_bytes": 4 } },
  "options": { "narrowing": "tpdf", "quality": "high", "headroom_db": 0.0, "dither_seed": null } }
```

`sample` is `{ "kind": "int", "bits", "slot_bytes" }` (8 to 32 bits in a slot of
1 to 4 bytes that holds them) or `{ "kind": "float" }`. `options` and each key in
it are optional; `narrowing` is `tpdf` or `truncate`, `quality` is `high` or `fast`.

**Plan** (`convertPlan`):

```json
{ "kind": "lossy", "description": "Lossy conversion, not bit-perfect: resampled from 44100 Hz to 48000 Hz (windowed-sinc polyphase, high quality, stopband about 110 dB); quantised to 24-bit in a 4-byte slot with TPDF dither (full scale saturates).",
  "preserves_bit_perfect": false, "resamples": true,
  "steps": [ { "code": "resample", "kind": "lossy", "description": "..." }, { "code": "quantise", "kind": "lossy", "description": "..." } ],
  "source": { "sample_rate": 44100, "channels": 2, "sample": {}, "bytes_per_frame": 4 },
  "target": { "sample_rate": 48000, "channels": 2, "sample": {}, "bytes_per_frame": 8 },
  "source_frame_bytes": 4, "target_frame_bytes": 8, "delay_frames": 80, "min_output_bytes": 16,
  "filter": { "l": 160, "m": 147, "taps": 160, "passband_hz": 20065.5, "stopband_hz": 22050.0, "design_attenuation_db": 110.0 } }
```

`kind` is `none`, `bit_exact`, `lossless` or `lossy`; `step.code` is `resample`,
`widen`, `repack`, `narrow`, `int_to_float`, `quantise` or `mono_to_stereo`;
`filter` is `null` unless the plan resamples.

### 14.6 The Kotlin side

`ConversionPlan.between(request, format, options)` calls `convertPlan`;
`PcmConverter.create(plan)` calls `convertCreate`; `PcmConverter.convert` and
`finish` wrap `convertProcess` and `convertFinish` and move the buffers' positions.
`ConvertingOutput(target, converter)` is a `PcmOutput` that converts on the way in
and reports everything in source units: `playedFrames = target.playedFrames * fi / fo`,
`latencyFrames = (target latency + converted frames the target has not taken) * fi / fo + delay_frames`,
`writableBytes` in source bytes. Converted audio the target cannot take is held (one
chunk) and delivered first on the next call; `drain()` writes the resampler's tail
before it drains the target; `flush()` resets the converter. Its
`bitPerfect` is the target's for `None` and `BitExact` plans and
`NotVerified(null, plan.description)` otherwise.

### 14.7 DSD to PCM

`usbdac_core::dsd_pcm::DsdPcmConverter` turns native DSD into PCM for a DAC
without a DSD path. It is always `Lossy`, and DoP carrier words are not
accepted.

```rust
DsdPcmConverter::new(bit_rate, channels, target: PcmFormat) -> Result<DsdPcmConverter, ConvertError>
DsdPcmConverter::with_options(bit_rate, channels, target, &ConvertOptions) -> Result<...>
converter.process(input: &[u8], output: &mut [u8]) / finish(output) / reset()
converter.clipped_samples() / delay_byte_frames() / min_output_bytes()
```

- Input is the native DSD layout of section 4: chronological bytes, oldest bit
  in bit 7, channels interleaved. DSD64, DSD128 and DSD256 with one to eight
  channels.
- A 704-tap Kaiser-windowed low-pass decimates the one-bit stream by 16, then
  the resampler of 14.4 (`ConvertOptions::quality`, `High` by default) reaches
  the target rate. Its first-stage transition spans 0.020–0.03125 cycles per
  DSD bit. A production-coefficient test bounds passband ripple and stopband
  response; a separate full-chain tone test covers DSD64 to 44.1 kHz. These
  software measurements do not establish a universal alias or audible-noise
  guarantee. See [DSD conversion](sdk/decisions-dsd-conversion.md).
- Amplitude: a one bit is +1 and a zero bit -1, and the filter has unity gain at
  DC, so ±1.0 is 100 % modulation. A 50 % modulated signal comes out at
  -6.02 dBFS before headroom. `headroom_db` (default -3.5 dB) is applied once,
  in the first stage when the target rate needs no resampling and in the
  resampler otherwise, so 50 % modulation becomes -9.52 dBFS with the defaults.
  Integer outputs saturate and `clipped_samples()` counts it.
- The filter history starts filled with the DSD idle pattern `0x69`. `reset()`
  starts a new stream.
