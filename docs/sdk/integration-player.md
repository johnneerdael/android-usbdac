# The player engine

What the part of your app that decodes and schedules audio has to do to feed usbdac correctly. [The integration guide](integration.md) says where this fits; [getting started](getting-started.md) has the compiled tutorial for the first stream, and this page goes further into each piece. The Kotlin outlines here are not compiled (each is marked), and use only calls that are documented elsewhere in this guide.

## What the engine must hand over {#what-the-engine-must-hand-over}

A direct stream sends the bytes you write. For the output to be what the file contains, the engine has to produce **integer PCM at the source's own rate and bit depth, unprocessed**, and put it in the layout the DAC wants.

| Source | Ask for | Notes |
|---|---|---|
| 16-bit PCM or FLAC | `StreamRequest.pcm(rate, channels, 16)` | If the DAC only has a 24-bit alternate setting, ask for 24 and widen the samples (shift left by 8), or let a `ConversionPlan` do it: the new low bits are zero and the plan is `BIT_EXACT` |
| 24-bit | `StreamRequest.pcm(rate, channels, 24)` | The DAC may carry it in a 3-byte or 4-byte slot. `format.subslotBytes` says which; you write what it says |
| 32-bit integer | `StreamRequest.pcm(rate, channels, 32)` | |
| A lossy source (AAC, MP3, Opus, Vorbis) | The decoder's output rate and depth | The transport is bit-perfect with respect to the decoded PCM. The audio is not the file's. Label it by source, see below |
| DSD | `StreamRequest.dop(...)` or `dsdNative(...)` | [Bit-perfect output](bit-perfect.md#dsd). DSF files store the least significant bit first, so reverse every byte before writing |

**Ask for the depth the source has and let `negotiate` choose the alternate setting.** `StreamFormat.validBits` and `subslotBytes` then tell you the layout. A source with fewer bits than the slot is widened by padding the low bits with zero.

### The wire layout

Samples are little-endian, channels interleaved, and left-justified in a slot of `subslotBytes` bytes (the [layout table](native-api.html#4-sample-layouts)). Most integer decoders hand over 32-bit words with the sample left-justified, which makes the conversion a copy of the top bytes:

<!-- uncompiled -->

```kotlin
/** Packs left-justified 32-bit samples into the stream's slot layout. Lossless when validBits <= subslotBytes * 8. */
fun packToWire(source: IntBuffer, wire: ByteBuffer, subslotBytes: Int) {
    while (source.hasRemaining()) {
        val sample = source.get()
        for (byte in 4 - subslotBytes until 4) wire.put((sample ushr (8 * byte)).toByte())
    }
}
```

This function is a sketch of the arithmetic and not part of the library. The compiled version of the same idea, shifting a sample left by `subslotBytes * 8 - validBits` and writing its low `subslotBytes` bytes, is in [getting started](getting-started.md#6-write-audio).

### What the decoder must not do

- **Float output.** A decoder that returns float and an app that converts it to integers has changed the samples, unless the conversion is exact. Integers up to 24 bits are exactly representable in a 32-bit float, so a float decoder for a 24-bit source can be acceptable if you convert back and check every sample. Mornye does exactly that for 17 to 24-bit sources through Android's decoders and refuses 32-bit lossless sources on that path (commit `4b7edf89c`). usbdac has no such check, so it is yours to make.
- **A platform decoder that gives you less than the file has.** Mornye's README says some vendor extractors expose 24-bit FLAC as "already-decoded 16-bit `audio/raw`" and that it decodes FLAC with libFLAC directly for that reason. Check the depth the decoder reports against the file's header.
- **A fixed output rate.** An engine that resamples everything to 48 kHz cannot be bit-perfect. The direct path needs the engine to follow the source's rate per track.
- **Dither or noise shaping**, and anything that changes level ([processing](#processing)).

### Lossy and converted sources

For a decoded lossy file, the honest label is "direct USB, decoded from AAC" and not "bit-perfect". Mornye's status label marks a decoded lossy source for this reason ("so a direct transport is not shown as a bit-perfect lossless chain"). usbdac's flag is about the transport only. The player knows what the source was, so the player labels it. The same goes for a stream behind a `ConvertingOutput`: its `bitPerfect` already says what was done, so show that.

## The writer {#the-writer}

The library owns a ring and one USB output thread. Nothing pulls audio from you, so you need a **writer thread** that decodes ahead and writes whenever the ring has room. The pattern is a pull loop: ask `writableBytes`, sleep if it is small, otherwise render a chunk and write it.

The compiled version, from the sample's `SinePlayer`, is in [getting started](getting-started.md#6-write-audio). The things to get right when a real decoder replaces the sine:

- **One writer thread, with its own lock.** Seeks and track changes happen on other threads. The lock the writer holds while it reads the decoder and writes is the one `seek` takes ([seek](#seek)).
- **A direct `ByteBuffer`** of about 10 ms, little-endian, reused. `write` reads a direct buffer in place and copies a heap buffer.
- **`write` returns the bytes taken.** A short count means the ring is full: keep the rest and try later. Zero for a partial frame means your chunk is not whole frames.
- **Wait on `writableBytes`, not on a timer alone.** It is the room up to the target depth. Sleep 2 to 5 ms when it is below your chunk size.
- **Keep the decoder ahead of the writer.** The ring is 250 ms by default. If decoding is slow (network, a heavy format), the output will underrun ([DacEvent.Underrun](integration-app.md#failures-and-fallback)). Read ahead in the decoder rather than raising the ring.
- **Give the thread audio priority.** `Process.setThreadPriority(Process.THREAD_PRIORITY_URGENT_AUDIO)` is the platform's setting for this. It has not been measured with usbdac; the USB output thread has its own real-time scheduling.
- **Never write after `drain()`** until a `flush()`; writes fail with `INVALID_STATE`.

At the end of the queue, call `drain()` once and wait for `isDrained` (or the `DRAINED` state). Between tracks of a gapless sequence in one format, do not drain and do not flush: keep writing.

## Transport mapping {#seek}

| Player action | Call | Notes |
|---|---|---|
| Play | `play()` | A new stream starts by itself when the ring holds `startThresholdMs` (100 ms). `play()` is for after a pause |
| Pause | `pause()` | Keep the writer running. The ring fills to its target and the writer idles when `writableBytes` is 0. What is on the bus (up to 16 ms) plays out first |
| Resume | `play()` | Continues from exactly the held audio |
| Seek | Under the writer's lock: reposition the decoder, discard the chunk rendered for the old position, `flush()`, set the new anchor | The order matters, see below |
| A track the listener picked, or Stop | `flush()`, then write the new audio or nothing | |
| Skip to the next track, same format | Keep writing the new track, or `flush()` first if the listener skipped | A skip is a replacement of what should be heard, so flush it |
| Skip, different format | `flush()` or `drain()`, close the output, open a new one for the new format | The DAC's clock is reprogrammed: expect a gap of 100 ms or more |
| End of the last track | `drain()`, then wait for `isDrained` | |

The compiled `Transport` class that holds these rules is in [getting started](getting-started.md#7-pause-resume-seek-stop).

**The seek order.** The writer may be holding a chunk it rendered for the old position. If the seek flushes the ring and the writer then writes that chunk, the old audio plays after the flush. So the seek takes the writer's lock, repositions the decoder and drops the pending chunk, flushes, and only then lets the writer continue. Flush while paused is fine and cancels nothing on the bus; the ring refills when the writer resumes.

**Why flush on every replacement.** The ring holds up to 250 ms and the bus 16 ms. A flush clears the ring and, by default, cancels the music-carrying transfers on the bus, so the new position is heard within a couple of milliseconds. A deep buffer costs nothing in responsiveness if you flush ([decision 2](decisions-stream.md#ring-depth)).

## Position {#position}

The audible position is the source position at the last flush plus what the DAC has played since:

<!-- uncompiled -->

```kotlin
fun audiblePositionUs(output: PcmOutput, counterRateHz: Int, anchorUs: Long): Long =
    anchorUs + output.playedFrames * 1_000_000L / counterRateHz
```

- `anchorUs` is the source position you flushed to (zero at the start of a track, the seek target after a seek). Set it under the same lock as the flush.
- `playedFrames` counts music frames the DAC has taken since the last flush: not padding, not held silence, not lost frames, and not audio that was already on the bus when you flushed. It restarts at 0 at the flush, so between a seek and the first new audio the position is the anchor.
- `counterRateHz` is `format.counterRateHz` (`SelectedOutput.format` for the selector). For PCM it is the sample rate. For native DSD it is the bit rate over 8, because the counters count one byte per channel. Behind a `ConvertingOutput` the counters are in source frames, so the arithmetic does not change.
- `latencyFrames` is what is between you and the DAC (ring plus bus). It is how far ahead of the sound the decoder is. You do not need it for the position, and it is useful for lining up visuals that you render from the decoded stream.
- It is cheap to read: poll it for the progress bar and for synced lyrics, tens of times a second.

Across a gapless track change in the same format nothing is flushed, so `playedFrames` keeps counting: convert the running count to a track position by subtracting the frames of the tracks that have finished (remember the count at each track boundary).

## Processing {#processing}

Anything the engine does to the samples before `write` ends the claim that the stream is bit-perfect, and the library cannot see it. The choices for each feature:

| Feature | If you keep it | What the player should show |
|---|---|---|
| Equaliser, DSP, convolution, bass boost | It runs before `write`, so every change is heard after the ring's fill (250 ms by default) | Not bit-perfect. Consider bypassing it in direct mode, as Mornye does |
| ReplayGain, volume normalisation, preamp | A gain on the samples | Not bit-perfect. The DAC's own volume is the alternative |
| Crossfade, fade in and out | A mix of two decoded streams | Not bit-perfect while it runs. A pause fade is not needed: pause holds the ring and sends silence |
| Speed or pitch change | Changes the samples | Not bit-perfect |
| Software volume | Use `DacStream.setSoftwareGain` if you must. It is applied as each transfer is built, so it is heard within the bus queue. It sets the blocker `SOFTWARE_GAIN` | Not bit-perfect. Refused for DSD |
| Resampling to a rate the DAC runs | Opt-in through `ConversionPlan` and `ConvertingOutput` | The plan's own description, which says exact, lossless or lossy |

The library's flag is not changed by your processing. Show your own indicator as "bit-perfect" only when **both** the output's flag is `Verified` **and** your engine did nothing to the samples.

With live processing, a shallower ring makes changes audible sooner. Use
`DacStream.setHostProcessing(true)` in Kotlin or `set_host_processing(true)` in
Rust when applying EQ or DSP upstream. The target changes to
`processingBufferTargetMs` (100 ms by default) as existing audio plays out.
Pass `false` to return to the passthrough target. The hint controls buffering.
The app remains responsible for describing any changes it made to the source audio.

```kotlin
fun setLiveEq(output: SelectedOutput, active: Boolean) {
    output.stream?.setHostProcessing(active)
}
```

## DSD sources {#dsd-sources}

Native DSD64 through DSD256 and the transition to PCM were verified on the AP80 Pro-X. [Hardware evidence](evidence.md#ap80-native-dsd-transition)

The DSD input and wire-layout rules are in [bit-perfect output](bit-perfect.md#dsd). For the engine: DSF stores bits least significant first and DFF most significant first, so bit-reverse DSF bytes before writing; DoP wants 16 DSD bits per sample in the slot layout with the marker byte left for the library to overwrite; native DSD wants one byte per channel in stream order; the position counters count bytes, so `counterRateHz` is the bit rate over 8; and no gain, no conversion and no equaliser can touch the stream. Use `capabilities.supportsDop` to identify a feasible PCM carrier, then establish that the device decodes DoP markers. `supportsNativeDsd` covers standard UAC3 DSD and supported legacy layouts. Standard UAC3 packing is fixed; retain per-DAC overrides for legacy native DSD, because a wrong legacy byte order sounds like noise and the library cannot detect it.

## Events the engine should react to

| Event | What the engine does |
|---|---|
| `Underrun` | The writer was late. Count it, log it, and look at the decoder's read-ahead. Playback continues with silence in the gap, and the bit-perfect flag does not change |
| `LostFrames(frames)` | A sliver of audio never reached the DAC. The flag is now `NotVerified(LOST_FRAMES)` for the rest of the stream. Show it. Optionally treat the first one as a reason to log a diagnostic |
| `FeedbackStale` | The stream lost usable feedback timing. Log `format.pacing` and `stats()`, and update the bit-perfect indicator. Explicit feedback can recover during a short hold of the last rate. Handle `StreamFailed` if timing fails to recover or the feedback transport fails |
| `StreamFailed` | The stream is `FAILED`. Stop the writer, close the output, and decide ([failures](integration-app.md#failures-and-fallback)) |
| `DeviceLost` | The DAC is gone. Pause, say so, and do not continue on another output unless the listener asked for that |
| `BitPerfectChanged`, `StateChanged`, `VolumeChanged` | Update the display ([the signal path](integration-app.md#the-signal-path-display)) |

Collect `session.events` on a dispatcher you choose; the library delivers them from its own thread and never from the audio path. For the selector, the same events come from `output.session?.events`.
