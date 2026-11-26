# Controls and configuration

Every option and control of the SDK, with its type, its default, what it does and what it does to bit-perfect output. "Bit-perfect impact" says whether the setting can make the library report the stream as not bit-perfect, or change the bytes that reach the DAC. See [bit-perfect output](bit-perfect.md) for the conditions behind the flag.

## Session configuration: `DacConfig`

Pass a `DacConfig` to `UsbDac.open(context, device, config)`. `DacConfig()` is what the transport was tuned with; change a value only for a reason. The native library validates the combination and throws `INVALID_ARGUMENT`.

### Buffering

| Option | Type | Default | Effect | Bit-perfect impact |
|---|---|---|---|---|
| `bufferTargetMs` | `Int` | 250 | The depth `writableBytes` fills the ring to. A deeper ring rides out a longer stall upstream (storage, network, scheduling). Must be between 1 and `bufferCapacityMs` | None |
| `bufferCapacityMs` | `Int` | 500 | Hard capacity of the ring: `write` never accepts more. 1 to 10000 | None |
| `startThresholdMs` | `Int` | 100 | Audio the ring must hold before a new stream starts, and before a flushed ring resumes. At least 1; keep it at or below `bufferTargetMs` | None |
| `processingBufferTargetMs` | `Int` | 100 | The recommended depth while software gain is not 1.0, so a host that processes audio hears its changes sooner. 1 to `bufferCapacityMs` | None |

A deep ring costs nothing in responsiveness if you `flush()` on every seek, stop and track change: the flush drops the ring at once. A shallow ring lowers memory and latency and makes underruns likelier (an underrun pads the wire with silence and sends `DacEvent.Underrun`).

### Clock and format

| Option | Type | Default | Effect | Bit-perfect impact |
|---|---|---|---|---|
| `requireVerifiedRate` | `Boolean` | `true` | Refuse to stream unless the DAC confirms the sampling frequency it was told (`CLOCK_UNVERIFIED`). Off, a DAC with a broken readback plays | Off: the stream is never reported bit-perfect (blocker `CLOCK_UNVERIFIED`) unless a quirk vouches for the DAC |
| `clockSettleDelayMs` | `Long?` | `null` | Wait after programming the clock. `null`: the device's quirk, else 20 ms | None |
| `padded24Bit` | `Boolean?` | `null` | Prefer the 4-byte slot for 24-bit PCM. `null`: the device's quirk, else off | None on the samples. It changes the wire layout (`StreamFormat.subslotBytes` becomes 4 and the 24-bit sample sits in the upper three bytes), and you write what the format says |
| `dsdBigEndian` | `Boolean?` | `null` | Native DSD byte order on the wire. `null`: the device's quirk, else little-endian | None; wrong order is noise from the DAC, which the library cannot detect |
| `dsdSubslot` | `Int?` | `null` | Slot size (1, 2 or 4) for native DSD alternate settings. `null`: the device's quirk, else the descriptor | None |
| `allowUnsynchronizedAsync` | `Boolean` | `false` | Stream an asynchronous endpoint that has no feedback endpoint, at the nominal rate. Off, such a DAC fails with `IMPLICIT_FEEDBACK_UNSUPPORTED` when that endpoint is the only way to carry the request. On, host timing and the DAC's clock drift apart and the DAC ends in underruns or overruns | Never bit-perfect while it is in use (blocker `UNSYNCHRONIZED_ASYNC`) |

### Quirks

`quirks` is a `List<QuirkOverride>` (default empty). Each entry names a vendor id, a product id, optionally a text the product name must contain, and a list of `DacQuirk`s. They are added to the library's built-in table, which holds two entries (MOONDROP Dawn Pro, Fosi Audio DS2). Config fields set explicitly (`clockSettleDelayMs`, `dsdBigEndian`, `dsdSubslot`, `padded24Bit`) win over quirks.

| Quirk | Effect | Bit-perfect impact |
|---|---|---|
| `SkipClockValidation` | The DAC's clock readback is unreliable (it returns 0 or fails). Trust the acknowledgement of the set request for PCM and skip verification | The clock counts as verified, on the DAC's word |
| `IgnoreInvalidSampleRate` | The DAC reports a rate that differs from the one just set (some report 0 Hz) but plays correctly: do not refuse. PCM only | As above |
| `RequireVerifiedRate` | Refuse to stream unless the rate is confirmed, even when `requireVerifiedRate` is off | None |
| `SettleDelayMs(ms)` | Wait this long after programming the clock | None |
| `PreferPadded24BitTransport` | The 4-byte slot for 24-bit PCM, as `padded24Bit` | None on the samples |
| `DsdSubslotSize(bytes)` | Override the slot size of native DSD alternate settings whose descriptors are wrong | None |
| `DsdBigEndian` | Native DSD words go on the wire most-significant byte first | None |
| `DsdBitReverse` | Reverse the bit order within each native DSD byte, for a DAC that expects the other order | None on the flag |

### Threads and ownership

| Option | Type | Default | Effect | Bit-perfect impact |
|---|---|---|---|---|
| `realtimePriority` | `Boolean` | `true` | Ask for real-time scheduling (`SCHED_FIFO`, priority 2, falling back to nice -16) for the USB output thread | None; it protects against underruns |
| `handBack` | `HandBack` | `RESTORE_ORIGINAL` | What `DacSession.close()` does with the DAC's hardware volume. See [hand-back](#hand-back) | None |

## The request: `StreamRequest`

`session.negotiate(request)` settles the format.

| Field | Type | Default | Effect |
|---|---|---|---|
| `sampleRate` | `Int` | none | PCM rate in Hz. Ignored for DSD |
| `channels` | `Int` | none | Channel count |
| `bitDepth` | `Int` | none | PCM bit depth: 16, 24 or 32. Ignored for DSD |
| `encoding` | `StreamEncoding` | `PCM` | `PCM`, `DOP` (DSD in 24-bit PCM words) or `DSD_NATIVE` |
| `dsdBitRate` | `Int?` | `null` | DSD bit rate in Hz (2,822,400 for DSD64). Required for the two DSD encodings |
| `ratePolicy` | `RatePolicy` | `EXACT` | `EXACT`: a rate the DAC cannot run fails with `UNSUPPORTED`. `NEAREST_SUPPORTED`: use the nearest rate; the returned format carries it and the host must resample |

Build one with `StreamRequest.pcm(rate, channels, bits)`, `StreamRequest.dop(dsdBitRate, channels)` or `StreamRequest.dsdNative(dsdBitRate, channels)`. Bit-perfect impact: `NEAREST_SUPPORTED` with a different rate sets `StreamFormat.rateDiffersFromRequest` and the blocker `FORMAT_CHANGED`; the library does not resample, so what you resample is on you.

## The stream: `DacStream` and `PcmOutput`

| Control | Type | Effect | Bit-perfect impact |
|---|---|---|---|
| `write(buffer)` | `ByteBuffer -> Int` | Writes whole frames in the wire layout, advances the position by what it took. Never blocks. A direct buffer is read in place; a heap buffer is copied | The bytes written are the bytes on the wire |
| `writableBytes` | `Int` | Room up to the target depth, a whole number of frames; 0 after `drain()`. A target, not a limit | None |
| `play()` | | Starts or resumes. A new stream starts itself once `startThresholdMs` is buffered | None |
| `pause()` | | Sends silence (zeros for PCM, the idle pattern for DSD, DoP markers keep alternating) so the DAC stays locked. Does not consume the ring. Transfers already queued on the bus (16 to 32 ms on a high-speed link, about 130 ms on full speed) still play | None |
| `flush()` | | Drops the ring, resets `playedFrames` to 0, clears a pending `drain()`, holds silence until the ring has the start threshold again | None |
| `drain()` | | Declares the end of the audio: the ring plays out, including a tail shorter than the start threshold. Writes then fail until `flush()` | None. The padded end is not an underrun |
| `isDrained` | `Boolean` | `true` when everything written has completed on the bus | None |
| `playedFrames` | `Long` | Music frames completed on the bus since the start or the last flush, in `format.counterRateHz` units. Silence is not counted | None |
| `latencyFrames` | `Long` | Music between the host and the DAC: the ring plus what is on the bus | None |
| `stats()` | `DacStreamStats` | Counters: underruns, lost frames, packets, timeouts, buffer depth, effective rate, feedback state, and `bitPerfectBlocker` | None |
| `bitPerfect` | `StateFlow<BitPerfectState>` | `Verified` or `NotVerified(blocker, reason)` | It is the verdict |
| `state` | `StateFlow<StreamState>` | `BUFFERING`, `PLAYING`, `PAUSED`, `DRAINING`, `DRAINED`, `CLOSED`, `FAILED` | None |
| `format` | `StreamFormat` | What goes on the wire: encoding, rate, channels, bits, slot size, interface, alternate setting, synchronisation, whether feedback is used | None |
| `setSoftwareGain(gain)` | `Float` (1.0) | Scales the samples in the library. Refused for DoP and native DSD | Any value other than exactly 1.0 stops the stream being bit-perfect (blocker `SOFTWARE_GAIN`). Prefer the DAC's volume |
| `close()` | | Stops the stream and releases it (about 100 ms). The session and the volume stay | None |

`position = startTime + playedFrames / format.counterRateHz`, with `startTime` set at each flush.

`DacSession.events` carries what happened: `StateChanged`, `Underrun`, `LostFrames`, `BitPerfectChanged`, `VolumeChanged`, `FeedbackStale`, `StreamFailed`, `DeviceLost`, `VolumeWriteFailed`. It is a `SharedFlow` with a buffer of 64; a collector that cannot keep up misses the oldest events. `DacSession.state` is `READY`, `STREAMING`, `LOST` or `CLOSED`.

## Hardware volume: `DacVolume`

`session.volume` is `null` when the DAC has no writable volume control. It works with or without a stream while the session is open. The system volume has no effect on a direct stream; this is the volume.

| Control | Type | Effect | Bit-perfect impact |
|---|---|---|---|
| `range` | `VolumeRange` | The DAC's minimum, maximum and resolution in dB | None |
| `state` | `StateFlow<VolumeState>` | Level in dB, position on the 0..1 curve, mute (`null` when the DAC has no readable mute). Follows this library's writes and `refresh()`; a physical knob is noticed only by `refresh()` | None |
| `setDb(db)` | `Double` | Clamped to the range and snapped to the nearest step the DAC supports (a tie goes to the quieter step). Returns at once | None: the DAC scales the audio, the stream stays bit-perfect |
| `setFraction(f)` | `Double` (0..1) | Position on the curve. 0 is the DAC's minimum; above 0 the curve spans the larger of the minimum and -80 dB to the maximum, linearly in dB, so a DAC whose minimum is -127 dB does not waste most of a slider on inaudible levels. Returns at once | None |
| `step(direction, steps)` | `Int, Int = 16` | Moves by 1/`steps` of the curve and by at least one DAC step; at either end the level stays. `steps` of 0 or less means 16. Returns at once | None |
| `setMute(muted)` | `Boolean` | Mutes or unmutes. Needs `hasMute`; otherwise the write fails and is reported | None |
| `hasMute` | `Boolean` | The DAC has a writable mute | None |
| `refresh()` | `VolumeState` | Reads the level from the DAC (blocking) and updates `state` | None |
| `originalDb` | `Double?` | The level the DAC had before the library first changed it, if every channel agreed | None |
| `handBack()` | `Boolean` | Writes the original level back (blocking) and forgets it. Pending writes are dropped first | None |

The setters queue their writes on a library thread, one at a time. A burst of absolute values (`setDb`, `setFraction`) collapses to the last one, which is always applied; identical `step` calls merge into a count (at most 32 wait); a mute request replaces a pending one. A write that fails is reported as `DacEvent.VolumeWriteFailed` on `session.events`, because the call that asked has already returned. Writes are verified by reading the level back.

### Hand-back

The library records the level the DAC had the first time it read the volume. `DacSession.close()` applies the session's `handBack`; `close(handBack)` overrides it for one call.

| Policy | Effect |
|---|---|
| `RESTORE_ORIGINAL` (default) | Writes the original level back after the stream has stopped, verifies it, and forgets it. Use it when the DAC goes back to Android's audio stack |
| `KEEP` | Leaves the DAC at its current level and remembers the original for the next `UsbDac.open` of the same device in this process. Use it when your app takes the DAC straight back |

The restore runs after the stream has stopped, so a level louder than the current one is never heard. If the device is lost, the restore is skipped.

## Volume keys

`DacVolumeKeys.handle(event, volume, steps = 16)` steps the DAC volume for the phone's volume keys. Call it from `Activity.dispatchKeyEvent`. It consumes volume-up and volume-down, including key repeats and the key release, and returns `false` for everything else and for every key while `volume` is `null`, so the system volume keeps working when there is no DAC volume. A key press is `step(±1, steps)`.

```kotlin
    override fun dispatchKeyEvent(event: KeyEvent): Boolean =
        DacVolumeKeys.handle(event, session?.volume) || super.dispatchKeyEvent(event)
```

It only sees keys while the activity is in front. For the keys with the screen off, and for the system volume panel, use the next section.

## The system volume panel and background keys

`DacVolumeProvider(volume, scope)` is a platform `android.media.VolumeProvider` with an absolute 0..100 scale over the DAC's 0..1 curve. Attach it to your `MediaSession` and the system volume panel and the hardware keys drive the DAC, also from the background. It keeps the panel in step with the DAC by collecting `volume.state` in `scope`; cancel the scope when the session ends.

```kotlin
fun attachDacVolume(session: MediaSession, volume: DacVolume, scope: CoroutineScope) {
    session.setPlaybackToRemote(DacVolumeProvider(volume, scope))
}
```

```kotlin
fun detachDacVolume(session: MediaSession) {
    session.setPlaybackToLocal(
        AudioAttributes.Builder()
            .setUsage(AudioAttributes.USAGE_MEDIA)
            .setContentType(AudioAttributes.CONTENT_TYPE_MUSIC)
            .build(),
    )
}
```

Raise and lower step the DAC by one `step()`; set-to-index writes `setFraction(index / 100)`. Mute, unmute and toggle map to `setMute` when the DAC has a mute and are ignored otherwise. A session has one remote volume: if it also serves a cast target, decide which provider is attached.

With `MediaSessionCompat` from AndroidX Media, use `VolumeProviderCompat` (the library does not depend on `androidx.media`):

<!-- uncompiled -->

```kotlin
val provider = object : VolumeProviderCompat(
    VOLUME_CONTROL_ABSOLUTE, 100, (volume.state.value.fraction * 100).roundToInt(),
) {
    override fun onSetVolumeTo(volumeIndex: Int) = volume.setFraction(volumeIndex / 100.0)
    override fun onAdjustVolume(direction: Int) {
        if (direction > 0) volume.step(1) else if (direction < 0) volume.step(-1)
    }
}
mediaSessionCompat.setPlaybackToRemote(provider)
volume.state.onEach { provider.currentVolume = (it.fraction * 100).roundToInt() }.launchIn(scope)
```

Bit-perfect impact of all of the above: none. They change the DAC's volume, not the samples.

## Idle release

A paused direct stream keeps the output thread running and sends silence so the DAC stays locked. `DacIdleRelease` closes the stream after a continuous pause and reopens it on resume, with a lambda you supply. The session and the DAC volume stay open meanwhile.

| Parameter | Type | Default | Effect |
|---|---|---|---|
| `scope` | `CoroutineScope` | none | Runs the timer; cancel it with the player |
| `timeoutMs` | `Long` | 30,000 | Continuous pause before the stream is closed |
| `reopen` | `suspend () -> T` | none | Opens a new output the way you opened the first. Called from `onResumed()` only when the stream was released |
| `closeContext` | `CoroutineContext` | `Dispatchers.IO` | Where the blocking close runs |

```kotlin
fun idleRelease(scope: CoroutineScope, session: DacSession, request: StreamRequest, first: DacStream): DacIdleRelease<DacStream> {
    val idle = DacIdleRelease(
        scope = scope,
        timeoutMs = 30_000,
        // Runs from onResumed() when the stream was released; the host opens it the way it did the first time.
        reopen = { session.openStream(session.negotiate(request)) },
    )
    idle.attach(first)
    return idle
}
```

Call `idle.onPaused()` when playback pauses and `idle.onResumed()` when it resumes; the latter returns the stream to write to, the same one or a new one. Reopening costs 100 ms or more (negotiate and open), so the first audio after a long pause is late by that much. `idle.isReleased` is a `StateFlow<Boolean>`. Bit-perfect impact: none; a reopened stream is verified again from scratch.

## Library-level

| Setting | Effect |
|---|---|
| `UsbDac.setLogLevel(level)` | Routes the native library's log to logcat under the tag `usbdac`. 0 off, 1 error, 2 warn, 3 info, 4 debug, 5 trace |
| `UsbDac.nativeVersion` | The native library's version string; loads `libusbdac.so` |

The managed transport's controls are in [transports](transports.md).

## The AAudio output: `AAudioConfig`

Pass an `AAudioConfig` to `AAudioExclusiveOutput.open(context, request, device, config)`. The request supplies the rate, the channel count and (with `floatSamples`) the sample layout; nothing is converted. "Bit-perfect impact" says whether the option can change what `bitPerfect` reports: the AAudio output is never `Verified`, so the question is whether it changes the reason.

| Option | Type | Default | Effect | Bit-perfect impact |
|---|---|---|---|---|
| `requireExclusive` | `Boolean` | `true` | Fail `open` with `EXCLUSIVE_UNAVAILABLE` when the platform grants only a shared stream (AAudio falls back to shared on its own when exclusive is not available). Off, the output opens and `grant.sharingMode` is `SHARED` | On: a stream that opens is exclusive. Off: a shared stream reports that the system mixer is in the path |
| `floatSamples` | `Boolean` | `false` | With `bitDepth` 32, carry 32-bit float instead of 32-bit integers. Any other depth with it on fails `INVALID_ARGUMENT` | None; the bytes are not converted either way |
| `performanceMode` | `AAudioPerformanceMode` | `LOW_LATENCY` | How the platform schedules the stream: `NONE`, `POWER_SAVING` or `LOW_LATENCY`. Exclusive (memory-mapped) streams are normally granted only in `LOW_LATENCY` | An exclusive stream is unlikely in the other modes |
| `bufferTargetMs` | `Int` | 250 | The depth `writableBytes` fills the output's ring to. Between `startThresholdMs` and `bufferCapacityMs` | None |
| `bufferCapacityMs` | `Int` | 500 | The ring's capacity: `write` never accepts more. At most 10000, and the ring must fit in 64 MiB | None |
| `startThresholdMs` | `Int` | 100 | Audio the ring must hold before playback starts, and before it resumes after a flush or an underrun. At least 1 | None |
| `deviceBufferBursts` | `Int?` | `null` | The device-side buffer in bursts of `grant.framesPerBurst` frames; `null` is the platform's default (usually 2). A larger buffer rides out scheduling hiccups at the price of latency | None |

A deeper ring costs nothing in responsiveness if you `flush()` on every seek, stop and track change, exactly as for a direct stream.

### Reading the output

| Member | Type | What it is |
|---|---|---|
| `grant` | `AAudioGrant` | What the platform granted, read back after opening: `sharingMode`, `sampleRate`, `channels`, `format`, `framesPerBurst`, `bufferCapacityFrames`, `bufferSizeFrames`, `deviceId`, `performanceMode`, `usesMmap`, `hardware` (Android 14+), `asRequested`, `verdict`, `reason` |
| `bitPerfect` | `StateFlow<BitPerfectState>` | `NotVerified(null, grant.reason)`, never `Verified`. After a disconnect the reason says the route failed and keeps what was known before |
| `state` | `StateFlow<StreamState>` | `BUFFERING`, `PLAYING`, `PAUSED`, `DRAINING`, `DRAINED`, `CLOSED`, `FAILED`. Refreshed by every call you make and by the platform's error callback |
| `events` | `SharedFlow<AAudioEvent>` | `Disconnected`, `Failed`, `Underrun`, `Xrun`, `StateChanged`. A buffer of 64; a collector that cannot keep up misses the oldest |
| `xrunCount` | `Int` | AAudio's count of device-side underruns since the stream opened |
| `underrunCount` | `Long` | How often the output's own ring ran dry because the host wrote too slowly |
| `playedFrames`, `latencyFrames` | `Long` | At the stream's rate; see [transports](transports.md#what-aaudio-guarantees) |

`write`, `writableBytes`, `play`, `pause`, `flush`, `drain`, `isDrained` and `close` behave as in the table of [the stream](#the-stream-dacstream-and-pcmoutput), with these differences: `pause()` plays silence on a stream that stays open, `latencyFrames` counts the ring plus what AAudio holds, and `close()` takes up to about half a second.

