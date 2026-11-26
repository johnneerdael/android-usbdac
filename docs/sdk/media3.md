# Media3 integration

**This page is guidance, not shipped code.** The SDK has no dependency on Media3 and ships no `AudioSink`. Nothing below has been compiled or run; the Kotlin is an outline of how the pieces map. The method names follow the `AudioSink` interface of Media3 1.x as it stands on the `release` branch at the time of writing (1.12.0-rc01); the interface has changed between versions (for example `configure(AudioSinkConfig)` replaces the deprecated `configure(Format, int, int[])`), so check the version you build with.

## Why a custom sink

Media3's `DefaultAudioSink` creates an `AudioTrack` and runs an `AudioProcessor` chain in front of it: channel mapping, speed change, silence skipping, float conversion and more. That is the Android-managed path, and it is what you want if you are happy with it. To send decoded PCM straight to a DAC through usbdac, implement `AudioSink` yourself and hand it to the renderer (`DefaultRenderersFactory.buildAudioSink` or your own `AudioRendererFactory`). Because `DacStream` and `AndroidManagedOutput` are both a `PcmOutput`, the same sink can drive either.

## The mapping

`handleBuffer` is called on the playback thread with a buffer that must be consumed fully before the next one is offered; it returns whether it did. That fits `PcmOutput.write`, which never blocks and returns how many bytes it took.

| `AudioSink` | `PcmOutput` and the session |
|---|---|
| `supportsFormat`, `getFormatSupport` | Directly supported for the PCM encodings you can map to a layout the DAC offers (`capabilities.altSettings`: 16-bit, 24-bit, 32-bit at the format's rate and channel count). Unsupported otherwise, so the renderer decodes to PCM. Float is a conversion the library does not do: unsupported, or accepted knowingly as not bit-perfect |
| `configure` | Close the previous output. `session.negotiate(StreamRequest.pcm(rate, channels, bits))`, `session.openStream(format)`. Both block for 100 ms or more, so open the `DacSession` ahead of time and keep it. Map a `DacException` to `ConfigurationException`. Build the converter from Media3's PCM layout to the stream's wire layout (see below) |
| `play` | `output.play()` |
| `handleBuffer(buffer, presentationTimeUs, count)` | Convert whole frames into a direct staging buffer, `output.write(staging)`, advance `buffer` by the source frames the write consumed, return `!buffer.hasRemaining()`. Remember the first `presentationTimeUs` after `configure` or `flush` as the position anchor |
| `handleDiscontinuity` | Nothing to do for PCM |
| `getCurrentPositionUs(sourceEnded)` | `anchorUs + output.playedFrames * 1_000_000 / counterRateHz`; `CURRENT_POSITION_NOT_SET` before the first buffer. `playedFrames` is reset by `flush()`, so set the anchor again after each flush |
| `playToEndOfStream` | `output.drain()` and remember that it was called |
| `isEnded` | The drain was requested and `output.isDrained` |
| `hasPendingData` | `output.latencyFrames > 0`, or not drained |
| `pause` | `output.pause()` |
| `flush` | `output.flush()`, forget the anchor and any partly converted data |
| `reset` | Close the output. Keep the session unless the player is going away |
| `setVolume` | Do not apply it. Software gain breaks bit-perfect output. Offer the DAC's hardware volume through `DacVolume`, `DacVolumeKeys` and `DacVolumeProvider` instead |
| `setPlaybackParameters`, `setSkipSilenceEnabled` | Speed and silence skipping change the samples. Keep the parameters at their defaults and say so in `getPlaybackParameters` |
| `setAudioAttributes`, `setAudioSessionId`, `setAuxEffectInfo`, `enableTunnelingV21`, `disableTunneling` | No `AudioTrack` is involved; accept and ignore |
| `getAudioTrackBufferSizeUs` | `DacConfig.bufferTargetMs * 1000` |
| `setListener` | Call `onUnderrun` on `DacEvent.Underrun`, and map `DacEvent.StreamFailed` to the sink's error reporting |

### Layout conversion

Media3 hands over little-endian PCM: `C.ENCODING_PCM_16BIT` as `i16`, `C.ENCODING_PCM_24BIT` as packed 3-byte samples, `C.ENCODING_PCM_32BIT` as `i32`. The stream wants the slot layout in `StreamFormat.subslotBytes` and `validBits`:

- 16-bit into a 2-byte slot, 24-bit packed into a 3-byte slot, and 32-bit into a 4-byte slot are plain copies.
- 24-bit into a 4-byte slot is the sample shifted left by 8 (low byte zero). 16-bit into a 3-byte or 4-byte slot is shifted left to the top of the slot. Both are lossless.
- Narrowing (32-bit source into a 24-bit slot) drops bits and is not bit-perfect with respect to the source.

Ask for the depth the source has and let `negotiate` choose the alternate setting.

## Outline

<!-- uncompiled -->

```kotlin
/** An outline of an AudioSink over a PcmOutput. Not compiled. Not complete. */
class UsbDacAudioSink(
    private val session: DacSession,
    private val openOutput: (StreamRequest) -> PcmOutput,   // DacStream or AndroidManagedOutput
) : AudioSink {
    private var output: PcmOutput? = null
    private var counterRateHz = 0
    private var anchorUs = C.TIME_UNSET
    private var drainRequested = false
    private var staging: ByteBuffer? = null

    override fun configure(config: AudioSinkConfig) {
        output?.close()
        val format = config.format
        val request = StreamRequest.pcm(format.sampleRate, format.channelCount, bitsOf(format.pcmEncoding))
        try {
            output = openOutput(request)
        } catch (error: DacException) {
            throw AudioSink.ConfigurationException(error, format)
        }
        counterRateHz = format.sampleRate
        anchorUs = C.TIME_UNSET
        drainRequested = false
    }

    override fun handleBuffer(buffer: ByteBuffer, presentationTimeUs: Long, encodedAccessUnitCount: Int): Boolean {
        val out = output ?: return false
        if (anchorUs == C.TIME_UNSET) anchorUs = presentationTimeUs
        // Convert whole frames of `buffer` to the wire layout into `staging`, write, and advance `buffer`
        // by the source frames that were taken. Return true when nothing remains in `buffer`.
        return convertAndWrite(out, buffer)
    }

    override fun getCurrentPositionUs(sourceEnded: Boolean): Long {
        val out = output ?: return AudioSink.CURRENT_POSITION_NOT_SET
        if (anchorUs == C.TIME_UNSET) return AudioSink.CURRENT_POSITION_NOT_SET
        return anchorUs + out.playedFrames * 1_000_000L / counterRateHz
    }

    override fun play() { output?.play() }
    override fun pause() { output?.pause() }
    override fun playToEndOfStream() { drainRequested = true; output?.drain() }
    override fun isEnded() = drainRequested && output?.isDrained == true
    override fun hasPendingData() = (output?.latencyFrames ?: 0) > 0
    override fun flush() { output?.flush(); anchorUs = C.TIME_UNSET; drainRequested = false }
    override fun reset() { output?.close(); output = null }
    override fun setVolume(volume: Float) = Unit   // the DAC's hardware volume is the volume
    // ... the remaining AudioSink methods: format support, listener, parameters, tunneling, attributes.
}
```

Two things to watch. First, `configure` and the first `handleBuffer` run on the playback thread; a `negotiate` and `openStream` that take 100 ms or more there delay the first audio, so keep the `DacSession` open between tracks and only reopen the stream when the format changes (gapless playback of equal formats needs no new stream: keep writing). Second, `AudioSink.flush` can be called while `handleBuffer` holds a partly consumed buffer; drop the partial conversion state with it.
