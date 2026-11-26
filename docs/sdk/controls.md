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

A deep ring costs nothing in responsiveness if you `flush()` on every seek, stop and track change: the flush drops the ring at once, and the transfers already on the bus are cancelled (`cancelInFlightOnFlush`), so the old position stops within a couple of milliseconds. A shallow ring lowers memory and latency and makes underruns likelier (an underrun pads the wire with silence and sends `DacEvent.Underrun`).

### The bus queue

| Option | Type | Default | Effect | Bit-perfect impact |
|---|---|---|---|---|
| `busQueueMs` | `Int` | 16 | Audio kept queued on the USB bus, 4 to 64: what a `pause()` still plays out and the headroom against a late output thread. The same for every service interval and encoding | None |
| `transfersInFlight` | `Int` | 8 | Isochronous transfers in flight at once, 2 to 16. A transfer carries `busQueueMs / transfersInFlight` of audio (2 ms by default) | None |
| `cancelInFlightOnFlush` | `Boolean` | `true` | `flush()` also cancels the transfers already on the bus, so a seek is heard at once instead of after `busQueueMs`. Off: they play out. See [flush](#the-stream-dacstream-and-pcmoutput) | None: cancelled transfers are not lost audio |

The packets in flight never reach 256, and at most 32 go in a transfer. Where the configured depth would break either limit the cap wins and less is queued: `busQueueMs = 64` with eight transfers at a 125 us service interval queues 31 ms, not 64. A DAC with a 1 ms service interval used to queue 128 ms and DSD at 125 us 256 packets (one more than the measured stall point); both now queue 16 ms like every other case. A shorter queue makes a pause or a stop sound sooner and leaves less headroom for a late output thread; leave the defaults unless a DAC underruns (deepen) or a host needs the shortest pause (shorten). See [the bus queue](../API.md#the-bus-queue).

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

### Volume protection

| Option | Type | Default | Effect | Bit-perfect impact |
|---|---|---|---|---|
| `startupVolume` | `StartupVolume` | `Attenuate(-40.0)` | What `UsbDac.open` does with the DAC's hardware volume before any stream can start: lower every channel to at most `maxDb`, or `Keep` it. See [start-up volume protection](#start-up-volume-protection) | None: the DAC scales the audio |
| `requireHardwareVolume` | `Boolean` | `true` | `openStream` fails with `VOLUME_UNAVAILABLE` when the DAC has no usable hardware volume, so the app can ask the user to allow fixed-level output and reopen with `false` | None |

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
| `pause()` | | Sends silence (zeros for PCM, the idle pattern for DSD, DoP markers keep alternating) so the DAC stays locked. Does not consume the ring. Transfers already queued on the bus (`busQueueMs`, 16 ms by default, at every service interval) still play | None |
| `flush()` | | Drops the ring, resets `playedFrames` to 0, clears a pending `drain()`, holds silence until the ring has the start threshold again. Returns at once; with `cancelInFlightOnFlush` (default) the output thread then cancels the transfers already on the bus, waits until every one is handed back and refills with silence. Cancelled transfers are not lost frames or underruns, do not clear bit-perfect and are not counted as played. A flush while paused, or with only silence queued, cancels nothing, and `pause()` never cancels | None |
| `drain()` | | Declares the end of the audio: the ring plays out, including a tail shorter than the start threshold. Writes then fail until `flush()` | None. The padded end is not an underrun |
| `isDrained` | `Boolean` | `true` when everything written has completed on the bus | None |
| `playedFrames` | `Long` | Music frames completed on the bus since the start or the last flush, in `format.counterRateHz` units. Silence is not counted | None |
| `latencyFrames` | `Long` | Music between the host and the DAC: the ring plus what is on the bus | None |
| `stats()` | `DacStreamStats` | Counters: underruns, lost frames, packets, timeouts, buffer depth, effective rate, feedback state, and `bitPerfectBlocker` | None |
| `bitPerfect` | `StateFlow<BitPerfectState>` | `Verified` or `NotVerified(blocker, reason)` | It is the verdict |
| `state` | `StateFlow<StreamState>` | `BUFFERING`, `PLAYING`, `PAUSED`, `DRAINING`, `DRAINED`, `CLOSED`, `FAILED` | None |
| `format` | `StreamFormat` | What goes on the wire: encoding, rate, channels, bits, slot size, interface, alternate setting, synchronisation, whether feedback is used, and the clock and pacing diagnostics ([below](#diagnostics-the-clock-and-the-pacing)) | None |
| `setSoftwareGain(gain)` | `Float` (1.0) | Scales the samples in the library. Refused for DoP and native DSD | Any value other than exactly 1.0 stops the stream being bit-perfect (blocker `SOFTWARE_GAIN`). Prefer the DAC's volume |
| `close()` | | Stops the stream and releases it (about 100 ms). The session and the volume stay | None |

`position = startTime + playedFrames / format.counterRateHz`, with `startTime` set at each flush.

`DacSession.events` carries what happened: `StateChanged`, `Underrun`, `LostFrames`, `BitPerfectChanged`, `VolumeChanged`, `FeedbackStale`, `StreamFailed`, `DeviceLost`, `VolumeWriteFailed`. It is a `SharedFlow` with a buffer of 64; a collector that cannot keep up misses the oldest events. `DacSession.state` is `READY`, `STREAMING`, `LOST` or `CLOSED`.

## Diagnostics: the clock and the pacing

Two read-only facts about a stream, on the `StreamFormat` that `negotiate` returned. They say what the library did; reading them changes nothing, and they cost nothing to read. Start here when a DAC plays at the wrong pitch, is silent at one rate, or clicks.

| Field | Type | What it says |
|---|---|---|
| `StreamFormat.clock` | `ClockChoice?` | The Clock Source the sample rate was set and verified on, and how it was reached from the stream's terminal. `null` when the DAC describes no clock entities (UAC 1.0 sets the rate on the endpoint) |
| `StreamFormat.pacing` | `Pacing` | What keeps the host's packets and the DAC's clock in step on the endpoint the stream uses |
| `AltSetting.pacing` | `Pacing?` | The same for each alternate setting of `session.capabilities`: what a stream on it would use. `null` when the library would not stream it with this configuration |

A `ClockChoice` has `controlInterface` (the AudioControl interface the clock requests go to), `sourceId` (the Clock Source entity ID) and `path`, a list of `ClockHop`s in the order followed: the stream's terminal first, then each clock entity, ending at the source. A hop's `kind` is `Terminal`, `Selector(selectedPin, pinCount)` (pins are 1-based; a selector with one input is reported as pin 1 of 1 without being asked, and with several, the pin is what the DAC reported), `Multiplier` or `Source`. `resolution` is:

| `ClockResolution` | Meaning |
|---|---|
| `RESOLVED` | The terminal was traced to the Clock Source through the descriptors. This is the normal case |
| `FALLBACK_FIRST_SOURCE` | The topology could not be traced (no terminal link, a malformed or looping entity, a selector that would not say which pin is active), so the first Clock Source in descriptor order was used and `path` is that source alone. On a DAC with more than one clock the rate may then have gone to the wrong one. The `usbdac` log says why at warning level |

`Pacing` names what the library does for the endpoint. It is a label, not a switch; the library adds no mode of its own.

| `Pacing` | The endpoint | Notes |
|---|---|---|
| `ExplicitFeedback(endpoint)` | Asynchronous, with a feedback endpoint (`endpoint` is its address) | The feedback endpoint is polled and the packets follow the rate the DAC reports |
| `Adaptive` | Adaptive | The DAC follows the host's packet timing |
| `Synchronous` | Synchronous | The DAC and the host share the bus clock |
| `Unsynchronized` | Asynchronous without a feedback endpoint (streamed only with `allowUnsynchronizedAsync`), or one that declares no synchronisation | The host sends at the nominal rate and nothing comes back. For the asynchronous case this is the `UNSYNCHRONIZED_ASYNC` blocker: never bit-perfect |
| `Unknown` | | A value this SDK version does not know, or a native library that predates the field |

`ClockChoice.description` and `Pacing.description` are one-line texts for a status line. The sample shows both next to the stream's statistics:

```kotlin
    /**
     * How the stream is paced and which clock the rate was set on: what to read first when a DAC
     * plays at the wrong pitch, or not at all at one rate.
     */
    private fun describeFormat(format: StreamFormat): String = buildString {
        appendLine("pacing: ${format.pacing.description}")
        val clock = format.clock
        if (clock == null) {
            append("clock: none (the DAC describes no clock entities)")
        } else {
            appendLine("clock: source ${clock.sourceId} on AudioControl interface ${clock.controlInterface}")
            append("  ${clock.description}")
        }
    }
```

For a DAC whose Clock Selector has the second of two inputs active, that prints `terminal 1 > selector 9 (pin 2 of 2) > source 5, resolved`. The clock is resolved when the format is negotiated and again when the stream opens, on the same terminal; a DAC whose selector is switched between the two calls (a clock switch on its front panel) can differ from what `negotiate` reported. See [troubleshooting](troubleshooting.md#wrong-pitch-or-no-sound-at-one-sample-rate) for what to do with a wrong clock.

## Hardware volume: `DacVolume`

`session.volume` is `null` when the DAC has no usable hardware volume control: it has none, or its start-up attenuation could not be verified. It works with or without a stream while the session is open. The system volume has no effect on a direct stream; this is the volume.

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
| `startup` | `StartupVolumeReport` | What the library did to the level when the session opened: `originalDb` and `appliedDb` per channel, `limitDb`, `attenuated`, `clampedToMinimum`, `maxDb`. See [start-up volume protection](#start-up-volume-protection) | None |
| `originalDb` | `Double?` | The level the DAC had before the library first changed it, if every channel agreed | None |
| `handBack()` | `Boolean` | Writes the original level back (blocking) and forgets it. Pending writes are dropped first | None |

The setters queue their writes on a library thread, one at a time. A burst of absolute values (`setDb`, `setFraction`) collapses to the last one, which is always applied; identical `step` calls merge into a count (at most 32 wait); a mute request replaces a pending one. A write that fails is reported as `DacEvent.VolumeWriteFailed` on `session.events`, because the call that asked has already returned. Writes are verified by reading the level back.

### Start-up volume protection

A DAC keeps the level it had, and many come up at full scale, so the first audio of a session would play at that level. `UsbDac.open` therefore finds the hardware volume control, records the level of each channel, and with the default `StartupVolume.Attenuate(-40.0)` lowers every controlled channel to `min(its level, maxDb)` before it returns, so before any stream can open. It then reads every channel back to verify it.

- A channel is **never raised**. One that is already quieter, or at the DAC's silent value, is left alone.
- `maxDb` is rounded **down** to a step the DAC supports. If the DAC's minimum is above `maxDb` the channels go to the minimum and `clampedToMinimum` says so.
- Channels that come out at different levels (one was already below the ceiling) are brought to the quietest, because the control reads and writes one level. Each channel's own original is still what hand-back restores.
- If a write or the read-back fails, the attenuation is not verified and the control is treated as **unusable**: `session.volume` is `null`, `capabilities.hasHardwareVolume` is false, and `openStream` fails as described below.
- `StartupVolume.Keep` leaves the level alone (the original is still recorded for hand-back).
- The level the DAC had is recorded **first**, so [hand-back](#hand-back) with `RESTORE_ORIGINAL` restores the true original, not the attenuated level. If the DAC is opened again in the same process after a session that kept the level, the record is the first one, and `startup.originalDb` still names it.
- The step runs once per `UsbDac.open`. A level the app sets afterwards is never pulled back, and a stream that opens later does not repeat it.

`DacVolume.startup` is the report. `limitDb` is the quietest original channel: **a host that restores a level it remembered for this DAC should not go above `limitDb` without the user's action**, because that is how loud the DAC itself was when it connected. On connect an app should:

1. read `session.volume?.startup`, and show it if it wants to say that the level was lowered;
2. restore its remembered per-DAC level, capped by `limitDb` (`min(remembered, limitDb)`), or leave the attenuated level if it remembers none;
3. let the user raise it from there (`setDb`, `setFraction`, the volume keys).

`DacEvent.VolumeChanged` follows the attenuation, once, to the first collector of `session.events` (an app that attaches later reads `volume.state` and `volume.startup`, which are already current).

**A DAC without a usable hardware volume.** With `requireHardwareVolume = true` (the default) `openStream` fails with `DacErrorCode.VOLUME_UNAVAILABLE`, because the stream would play at the DAC's own level and the app could not bring it down. `UsbDac.open`, `negotiate` and `capabilities` still work, so the app can show what the DAC offers, explain, and let the user allow fixed-level output: close the session and open it again with `DacConfig(requireHardwareVolume = false)`. It is the user's decision, per DAC; the library does not remember it. Set it to `false` up front for an app whose output always goes through its own software gain.

### Hand-back

The library records the level each channel of the DAC had the first time it read the volume, which is before the start-up attenuation. `DacSession.close()` applies the session's `handBack`; `close(handBack)` overrides it for one call.

| Policy | Effect |
|---|---|
| `RESTORE_ORIGINAL` (default) | Writes the original level of every channel back after the stream has stopped, verifies it, and forgets it. That is the level before the start-up attenuation, not the attenuated one. Use it when the DAC goes back to Android's audio stack |
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

## Conversion: `ConversionOptions`

The conversion helpers are opt-in and never touch the direct path by themselves: `ConversionPlan.between(request, format, options)` plans, `PcmConverter.create(plan)` builds the converter, `ConvertingOutput(target, converter)` puts it in front of any `PcmOutput`. Every plan but `NONE` and `BIT_EXACT` changes the audio; "bit-perfect impact" says what the option does to the verdict, which is `NotVerified` with the plan's description for those.

| Option | Type | Default | Effect | Bit-perfect impact |
|---|---|---|---|---|
| `narrowing` | `NarrowingMode` | `TPDF_DITHER` | Where bits are dropped (narrowing, floats to integers, the rounding of resampled values): `TRUNCATE` rounds toward negative infinity; `TPDF_DITHER` adds triangular noise of one step before rounding to nearest, so the error is signal-independent noise at half a step RMS (-98 dBFS at 16 bits) | Both are lossy; neither is bit-perfect |
| `quality` | `ResampleQuality` | `HIGH` | `HIGH`: stopband about 110 dB, no aliasing into the output, passband flat to 91% of the lower Nyquist frequency (20.07 kHz at 44.1 kHz). `FAST`: stopband about 84 dB, passband to 90%, about 2.5 times cheaper. Measured numbers are in [the native API document](native-api.html#14-conversion-helpers) | A rate change is lossy either way |
| `headroomDb` | `Double` | 0.0 | Gain after resampling, 0 to -60 dB. A resampled signal can exceed its input's peak (a full-scale square wave by 3 dB); integer outputs saturate at full scale, never wrap, and `PcmConverter.clippedSamples` counts them. `-3.5` keeps even that from clipping. At 0 the gain at DC is exactly 1 | Any value other than 0 changes the level; the plan's description says so |
| `ditherSeed` | `Long?` | `null` | Seed of the dither's generator: the same seed gives the same output. Not negative | None |
| `sourceLayout` | `PcmLayout?` | `null` | The layout of the source samples when it is not the one `StreamRequest.bitDepth` implies (16: `PCM_16`, 24: `PCM_24_PACKED`, 32: `PCM_32`): `FLOAT_32` or `PCM_24_IN_32` | A float source is converted to integers: lossy |

What the plan can contain, and how each step is classified:

| Step (`ConversionStep.code`) | `kind` | Notes |
|---|---|---|
| `widen` | `BIT_EXACT` | More bits; the new low bits are zero; every value is unchanged. 16 to 24 or 32, 24 to 32 |
| `repack` | `BIT_EXACT` | The same bits in another slot: 24-bit packed in 3 bytes to 24-bit in 4 and back |
| `mono_to_stereo` | `LOSSLESS` | The sample duplicated. The only channel change there is |
| `int_to_float` | `LOSSLESS` up to 24 bits, `LOSSY` for 32 | A float holds 24 bits exactly |
| `narrow` | `LOSSY` | Fewer bits, truncated or dithered |
| `quantise` | `LOSSY` | A float, or resampled values, rounded to integers; full scale saturates |
| `resample` | `LOSSY` | The polyphase windowed-sinc converter |

`PcmConverter` (not thread-safe: one thread at a time) has `convert(input, output)` and `finish(output)` on direct buffers, `reset()`, `clippedSamples` and `plan`; `plan.delayFrames` is the audio the resampler holds back and `plan.minOutputBytes` the smallest output buffer `convert` makes progress with. `ConvertingOutput` takes care of both. Its controls are those of [`PcmOutput`](#the-stream-dacstream-and-pcmoutput) with these differences: `write`, `writableBytes`, `playedFrames` and `latencyFrames` are in the **source** format and rate; `flush()` also resets the converter; `drain()` writes the resampler's tail and then drains the target (poll `isDrained`); `close()` closes the target and the converter.

