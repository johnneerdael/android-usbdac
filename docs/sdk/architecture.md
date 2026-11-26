# Architecture

This page explains how usbdac is built: the layers, what each module of the Rust core is for, what happens to a sample between `write` and the DAC, which threads exist, and how a stream moves through its states. It describes `v0.1.0-alpha.6`. The reasons for the choices are on the [design decisions](decisions.md) pages, and the user-level descriptions are in the [guide](index.md); this page is the map between them.

## The layers

```text
Your app
  |   Kotlin API: UsbDac, DacSession, DacStream, DacVolume, OutputSelector, PcmOutput
  v
dev.usbdac (the AAR, android/usbdac)
  |   claims USB interfaces, asks for permission, turns native JSON and events into
  |   Kotlin types and flows; the managed and AAudio outputs; the optional controls
  v
libusbdac.so (crates/usbdac-jni)
  |   JNI: opaque Long handles, JSON for structured values, one event bridge;
  |   the AAudio exclusive engine (AAudio is opened with dlopen at run time);
  |   the conversion helpers' JNI surface
  v
usbdac-core (crates/usbdac-core)
  |   descriptors, capabilities, clock resolution, negotiation, the ring, the
  |   isochronous output thread, loss and position accounting, volume, events,
  |   conversion (opt-in)
  v
usbfs through libusb (rusb) and raw usbfs ioctls
  v
the phone's USB host controller, the cable, the DAC
```

The Rust core knows nothing about Android except usbfs and the file descriptor it is handed. The Kotlin layer knows nothing about USB packets. Between them the JNI layer is deliberately thin: a handle is a number in a registry (a stale number gives an `invalid_argument` error and cannot crash the process), a structured value is JSON with snake_case keys, and an event is one method call, `onEvent(type, a, b, message)`, made on the library's event thread. The full contract is in the [native API document](native-api.html#10-jni-contract).

The core talks to the DAC through usbfs, so it works on Android and Linux. On any other host (the tests were run on a Mac) `Dac::open` returns `PlatformUnsupported`, and everything that does not need a DAC (parsing, scheduling, accounting, conversion, and the transport against a fake DAC) still runs. That is what lets 487 tests run on a laptop.

## What is in the Rust core

| Area | Modules in `crates/usbdac-core/src` | What they do |
|---|---|---|
| Public handles | `dac.rs`, `stream.rs`, `volume.rs`, `format.rs`, `capabilities.rs`, `config.rs`, `error.rs`, `events.rs` | `Dac`, `DacStream`, `VolumeControl`; the request and format types; what the DAC offers; every setting with a default and a validation; typed errors with stable codes; events |
| Descriptors | `descriptors/` | Parse the USB configuration and the Audio Class descriptors (UAC 1.0 and 2.0) into plain types |
| Topology | `clock_resolution.rs`, `endpoint.rs`, `transport/scan.rs` | Walk the clock graph from a terminal to its source; choose the explicit feedback endpoint; build `Capabilities` from the descriptors |
| Negotiation | `transport/negotiate.rs`, `transport/clock.rs`, `transport/api.rs` | Program the sampling frequency on the right entity, read it back, and settle the format the DAC will run |
| Stream setup | `transport/stream_open.rs`, `transport/session.rs`, `transport/shared.rs` | Switch the alternate setting, verify the clock again, start the output thread; the process-wide session and what the host handle, the thread and the events share |
| The ring | `transport/ring.rs`, `ring_target.rs` | The PCM ring buffer, the counters, the recommended depth and when it changes |
| The output thread | `transport/output.rs`, `transport/iso.rs`, `transport/encode.rs` | Build each transfer, submit it, handle completions, poll the feedback endpoint, encode samples to the wire layout, audit the payload |
| Packet arithmetic | `iso_packet_scheduler.rs`, `iso_packet_validation.rs` | How many frames go in each packet (exact over a long time, following feedback); which completed packet sizes are legal |
| Accounting | `iso_transport_loss.rs`, `output_position.rs`, `payload_audit_latch.rs`, `flush_cancel.rs` | Lost audio, played music frames and latency, the grace on the payload audit, which transfers a flush cancels |
| Volume | `volume_control.rs`, `transport/volume_hw.rs`, `transport/usbfs.rs` | Find the Feature Unit, quantise levels, attenuate at open, write and read back, hand back; control transfers on the descriptor |
| Per device | `quirk.rs`, `settings.rs` | The built-in quirk table and the settings the transport reads |
| Conversion | `convert/` | The opt-in resampler, bit-depth and channel conversion, planning and classification. The stream never calls it |
| Support | `priority.rs`, `transport/fake_dac.rs` | Real-time scheduling for the output thread; a fake DAC that answers control requests so the transport can be tested on a host |

The code that decides things is kept separate from the code that touches libusb, so the host tests run the same decisions the output loop does: `iso_packet_scheduler.rs`, `iso_transport_loss.rs`, `output_position.rs`, `ring_target.rs`, `flush_cancel.rs` and `clock_resolution.rs` are plain data and callbacks.

## The life of a sample

1. **The app writes.** `DacStream::write(bytes)` takes whole frames in the stream's wire layout (the layout is in the [native API document](native-api.html#4-sample-layouts)) and copies them into the ring. It never blocks: it takes as many frames as fit and returns the count. `writable_bytes()` says how much the app should write to bring the ring to its target depth, which is deeper while nothing is applied live (see [decision 2](decisions-stream.md#ring-depth)).
2. **The stream primes.** A new stream, or one that was flushed, sends wire silence until the ring holds `start_threshold_ms` (100 ms by default), so a flush never looks like an underrun.
3. **The output thread builds a transfer.** For each free slot (eight by default) the thread asks the `IsoPacketScheduler` for the byte count of every packet in the transfer: 16 packets of 5 or 6 frames each at 44.1 kHz and 125 us, with the fractional remainder carried so the long-run count is exact, adjusted by the DAC's feedback if it has a feedback endpoint. It then pops that many bytes from the ring (`pop_music_into_or_pad`): music while the ring has it, padding after that, held silence while paused or refilling. If software gain is on, PCM samples are scaled here, at the last moment, which is why a gain change is heard within the bus queue. The samples are encoded to the wire layout, and DoP markers or native DSD words are put in place.
4. **The transfer is submitted.** `libusb_submit_transfer` hands the host controller a batch of packets. With the defaults, about 16 ms of audio is queued on the bus at any moment ([decision 1](decisions-stream.md#queue-geometry)).
5. **A transfer completes.** The thread waits in `handle_events` for at most 50 ms. A completed transfer has a status for the transfer and one for each packet. A `COMPLETED` transfer is validated against the legal packet sizes; a timed-out one is accounted for packet by packet ([decision 3](decisions-stream.md#loss-handling)); any other status ends the stream.
6. **The accounting is updated.** The loss tracker records what the DAC did not take. `played_music_frames` grows by the music frames it did take, in the flush epoch the transfer was filled in. The payload audit sees the encoded bytes. The bit-perfect flag and the stream state are re-evaluated, and a change goes out as an event.
7. **The slot is refilled** with the next transfer, and the cycle repeats. The feedback endpoint has one persistent transfer that is re-armed from the same loop.

Everything on this path except the ring lock is in one thread, so there is one reaper for libusb's completions. Control transfers for volume do not use libusb at all: they are synchronous `USBDEVFS_CONTROL` ioctls on the same descriptor, which do not reap transfers ([decision 10](decisions-volume.md#volume-writes)).

## Threads

| Thread | Owner | What it does | Rules |
|---|---|---|---|
| The app's writer | The app | Calls `write`, `pause`, `play`, `flush`, `drain` | One writer. These calls never block. A seek must hold the writer's lock while it repositions the source and flushes ([the player engine](integration-player.md#seek)) |
| `usbdac-output` | The library, per stream | The loop above. Asks for `SCHED_FIFO` priority 2 on Android and falls back to nice -16 | Never calls into the app. Ends with the stream |
| `usbdac-events` | The library, per DAC | Delivers every event, in order, to the listener | A bounded queue with a non-blocking send; new events are dropped and logged if it overflows. Never called while a library lock is held. Do not close the session from a collector of events |
| App threads for blocking calls | The app | `UsbDac.open`, `negotiate`, `openStream`, `close`, `DacVolume.refresh` | These take 100 ms or more and must not run on the main thread |
| Volume writer | The SDK (Kotlin) | `VolumeWriteQueue` runs the one write that is in flight | Callers return at once; the last value always wins |

## States

A `DacSession` is `READY`, `STREAMING`, `LOST` or `CLOSED`. A stream moves through these states, which the stream publishes as events and as a `StateFlow`:

| `StreamState` | What is on the wire | Left by |
|---|---|---|
| `BUFFERING` | Wire silence, while the ring fills to the start threshold (at the start, after `flush()`, after an underrun recovers) | The ring reaching the threshold |
| `PLAYING` | Audio from the ring | `pause()`, `drain()`, a failure |
| `PAUSED` | Wire silence, ring untouched | `play()`, `flush()` |
| `DRAINING` | The ring playing out, including a tail shorter than the threshold | The last transfer completing |
| `DRAINED` | Wire silence | `flush()` or `close()` |
| `CLOSED` | Nothing | |
| `FAILED` | The stream has stopped; the stream object stays until it is closed | `close()` |

Two other pieces of state matter to an app. `BitPerfectState` is `Verified` or `NotVerified(blocker, reason)` and is re-evaluated after every transfer ([decision 13](decisions-reporting.md#bit-perfect-claim)). The volume state (level in dB, position on the 0..1 curve, mute) is read from the DAC after every write.

## Where each decision lives

| If you want to change ... | Look in ... |
|---|---|
| The queue shape, or 255 packets in flight | `iso_packet_scheduler.rs` (`IsoQueueGeometry`), `DacConfig::bus_queue_ms` |
| The ring depths | `config.rs`, `ring_target.rs` |
| What counts as lost audio, and the bounds | `iso_transport_loss.rs`, `transport/output.rs` |
| Pause, flush and position | `transport/ring.rs`, `flush_cancel.rs`, `output_position.rs` |
| Which clock is programmed | `clock_resolution.rs`, `transport/clock.rs` |
| Feedback decoding and smoothing | `transport/iso.rs`, `iso_packet_scheduler.rs` |
| The volume curve and start-up attenuation | `volume_control.rs`, `volume.rs`, `config.rs` |
| What turns the bit-perfect flag off | `stream.rs` (`BitPerfectBlocker`), `transport/session.rs`, `transport/encode.rs` |
| Which output the selector picks | `android/usbdac/.../OutputSelector.kt` |
| Interface claim and release | `android/usbdac/.../internal/UsbInterfaces.kt` |
