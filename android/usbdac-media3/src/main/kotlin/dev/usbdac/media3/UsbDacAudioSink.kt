package dev.usbdac.media3

import android.media.AudioDeviceInfo
import androidx.media3.common.AudioAttributes
import androidx.media3.common.AuxEffectInfo
import androidx.media3.common.C
import androidx.media3.common.Format
import androidx.media3.common.MimeTypes
import androidx.media3.common.PlaybackParameters
import androidx.annotation.OptIn
import androidx.media3.common.util.UnstableApi
import androidx.media3.exoplayer.audio.AudioSink
import dev.usbdac.DacException
import dev.usbdac.StreamRequest
import dev.usbdac.StreamEncoding
import java.nio.ByteBuffer
import java.nio.ByteOrder

/** The output was lost after Media3 submitted audio. Seek the decoder before retrying. */
class OutputRecoveryRequiredException(val failure: DacException) : Exception(
    "Output failed; queued audio may be lost. Seek to a confirmed playback position before resuming.", failure
)

// region sink-layout
/**
 * How Media3's PCM maps onto what the output wants. Media3 hands over little-endian PCM in 1, 2, 3 or 4
 * bytes per sample. The output wants the layout of its [dev.usbdac.WriteFormat]: the same, or a wider
 * slot with the sample left-justified (24 bits in a 4-byte slot is the sample shifted left by 8).
 */
class SinkLayout(val inputBytes: Int, val outputBytes: Int, val channels: Int, val rateHz: Int, val counterRateHz: Int) {
    val inputFrameBytes: Int get() = inputBytes * channels
    val outputFrameBytes: Int get() = outputBytes * channels
    val needsRepack: Boolean get() = inputBytes != outputBytes
}

/**
 * Copies [samples] samples from [source] (from its position, which does not move) into [target] and
 * pads each one to [outputBytes] bytes with zero low bytes. Widening keeps every sample value.
 */
fun repack(source: ByteBuffer, inputBytes: Int, outputBytes: Int, samples: Int, target: ByteBuffer) {
    var read = source.position()
    val padding = outputBytes - inputBytes
    repeat(samples) {
        repeat(padding) { target.put(0) }
        repeat(inputBytes) { target.put(source.get(read++)) }
    }
}
// endregion

/**
 * An [AudioSink] that plays Media3's decoded PCM through an [OutputSource]: direct USB when the DAC
 * allows it, otherwise whatever the library's selector chose. It does no processing: no gain, no speed
 * change, no silence skipping. Stereo and mono signed integer PCM, unsigned8 and IEEE float32.
 *
 * Media3 calls it from its playback thread, one call at a time.
 */
@OptIn(UnstableApi::class)
class UsbDacAudioSink(private val outputs: OutputSource) : AudioSink {
    private class Wanted(val format: Format, val request: StreamRequest)

    private var listener: AudioSink.Listener? = null
    private var attributes: AudioAttributes? = null

    // What the renderer asked for last. It becomes the output in use once the old one has played out.
    private var wanted: Wanted? = null
    private var active: OpenedOutput? = null
    private var activeFormat: Format? = null
    private var layout: SinkLayout? = null
    private var staging: ByteBuffer? = null

    // Integer PCM is retained byte for byte, including the look-behind for encoder padding.
    private var trimStartBytes = 0L
    private var trimEndBytes = 0
    private var trimBuffer: ByteBuffer? = null
    private var pendingFormat: Format? = null
    private var receivedFrames = 0L
    private var trimmedStartFrames = 0L

    private var playing = false
    private var endOfStream = false
    private var drainingForSwitch = false

    // Position: where the first frame since the last flush sits on the media timeline.
    private var startMediaTimeUsNeedsInit = true
    private var startMediaTimeUsNeedsSync = false
    private var startMediaTimeUs = 0L
    private var submittedFrames = 0L
    private var inputBuffer: ByteBuffer? = null

    // region sink-support
    override fun supportsFormat(format: Format): Boolean = getFormatSupport(format) != AudioSink.SINK_FORMAT_UNSUPPORTED

    override fun getFormatSupport(format: Format): Int {
        val bits = bitDepthOf(format.pcmEncoding)
        if (format.sampleMimeType != MimeTypes.AUDIO_RAW || bits == null ||
            format.channelCount !in 1..2 || format.sampleRate <= 0) return AudioSink.SINK_FORMAT_UNSUPPORTED
        val request = requestOf(format, bits)
        return when (outputs.formatSupport(request).support) {
            OutputFormatSupport.UNSUPPORTED -> AudioSink.SINK_FORMAT_UNSUPPORTED
            OutputFormatSupport.DIRECT -> AudioSink.SINK_FORMAT_SUPPORTED_DIRECTLY
            OutputFormatSupport.WITH_TRANSCODING, OutputFormatSupport.UNKNOWN -> AudioSink.SINK_FORMAT_SUPPORTED_WITH_TRANSCODING
        }
    }

    private fun requestOf(format: Format, bits: Int): StreamRequest = when (format.pcmEncoding) {
        C.ENCODING_PCM_8BIT -> StreamRequest.pcmU8(format.sampleRate, format.channelCount)
        C.ENCODING_PCM_FLOAT -> StreamRequest.pcmFloat32(format.sampleRate, format.channelCount)
        else -> StreamRequest.pcm(format.sampleRate, format.channelCount, bits)
    }

    private fun bitDepthOf(pcmEncoding: Int): Int? = when (pcmEncoding) {
        C.ENCODING_PCM_8BIT -> 8
        C.ENCODING_PCM_FLOAT -> 32
        C.ENCODING_PCM_16BIT -> 16
        C.ENCODING_PCM_24BIT -> 24
        C.ENCODING_PCM_32BIT -> 32
        else -> null
    }
    // endregion

    // region sink-configure
    override fun configure(audioSinkConfig: AudioSink.AudioSinkConfig) {
        val format = audioSinkConfig.format
        audioSinkConfig.outputChannelMapping?.let { mapping ->
            if (mapping.length() != format.channelCount || (0 until mapping.length()).any { mapping[it] != it }) {
                throw AudioSink.ConfigurationException("channel remapping is not supported", format)
            }
        }
        val bits = bitDepthOf(format.pcmEncoding)
        if (format.sampleMimeType != MimeTypes.AUDIO_RAW || bits == null || format.channelCount !in 1..2) {
            throw AudioSink.ConfigurationException("not supported Type I PCM in one or two channels", format)
        }
        if (format.sampleRate <= 0 || format.encoderDelay < 0 || format.encoderPadding < 0 ||
            format.encoderPadding.toLong() * format.channelCount * (bits / 8) > MAX_PADDING_BYTES) {
            throw AudioSink.ConfigurationException("invalid sample rate or encoder trim", format)
        }
        // Renderer output-format changes configure every stream, including decoder reuse.
        layout?.let { old -> receivedFrames -= (trimBuffer?.position() ?: 0) / old.inputFrameBytes }
        trimBuffer = null
        pendingFormat = format
        // Nothing is opened here. The output opens in handleBuffer, after the one in use has played out.
        wanted = Wanted(format, requestOf(format, bits))
    }
    // endregion

    // region sink-handle-buffer
    override fun handleBuffer(buffer: ByteBuffer, presentationTimeUs: Long, encodedAccessUnitCount: Int): Boolean {
        require(inputBuffer == null || buffer === inputBuffer)
        if (!applyPendingConfiguration()) return false
        val out = active ?: openOutput()
        val frames = layout!!
        pendingFormat?.let { format ->
            // The previous handleBuffer returned true only after all non-padding bytes were written.
            trimStartBytes = format.encoderDelay.toLong() * frames.inputFrameBytes
            trimEndBytes = Math.multiplyExact(format.encoderPadding, frames.inputFrameBytes)
            trimBuffer = ByteBuffer.allocateDirect(maxOf(STAGING_BYTES, trimEndBytes + STAGING_BYTES))
                .order(ByteOrder.LITTLE_ENDIAN)
            pendingFormat = null
            endOfStream = false
        }

        if (startMediaTimeUsNeedsInit) {
            startMediaTimeUs = maxOf(0L, presentationTimeUs)
            startMediaTimeUsNeedsSync = false
            startMediaTimeUsNeedsInit = false
        }
        if (inputBuffer == null) {
            // The first time this buffer is offered. Check that it follows the audio written so far.
            if (!buffer.hasRemaining()) return true
            val expectedUs = startMediaTimeUs + durationUs(receivedFrames - trimmedStartFrames, frames.rateHz)
            if (!startMediaTimeUsNeedsSync && Math.abs(expectedUs - presentationTimeUs) > 200_000) {
                listener?.onAudioSinkError(AudioSink.UnexpectedDiscontinuityException(presentationTimeUs, expectedUs))
                startMediaTimeUsNeedsSync = true
            }
            if (startMediaTimeUsNeedsSync) {
                startMediaTimeUs += presentationTimeUs - expectedUs
                startMediaTimeUsNeedsSync = false
                if (presentationTimeUs != expectedUs) listener?.onPositionDiscontinuity()
            }
            inputBuffer = buffer
        }

        out.failure?.let { failure ->
            // The DAC went away or the stream failed. While playing that is an error; while paused, wait for the
            // host's seek/flush, which discards the failed output and rebases the decoder.
            if (playing) throw writeFailure(failure)
            return false
        }
        val done = try {
            writeTrimmed(out, frames, buffer)
        } catch (error: DacException) {
            out.failure = error
            throw writeFailure(error)
        }
        if (done) inputBuffer = null
        return done
    }
    // endregion

    /** Holds the track's final padding without rounding, gain, or float conversion. */
    private fun writeTrimmed(out: OpenedOutput, frames: SinkLayout, buffer: ByteBuffer): Boolean {
        require(buffer.remaining() % frames.inputFrameBytes == 0) { "partial PCM frame" }
        if (trimStartBytes > 0) {
            val skipped = minOf(trimStartBytes, buffer.remaining().toLong()).toInt()
            buffer.position(buffer.position() + skipped)
            trimStartBytes -= skipped
            receivedFrames += skipped / frames.inputFrameBytes
            trimmedStartFrames += skipped / frames.inputFrameBytes
        }
        val held = checkNotNull(trimBuffer)
        fun pump(): Boolean {
            held.flip()
            val fullLimit = held.limit()
            val playable = (held.remaining() - trimEndBytes).coerceAtLeast(0)
            held.limit(held.position() + playable)
            if (playable > 0) write(out, frames, held)
            val blocked = held.hasRemaining()
            held.limit(fullLimit)
            held.compact()
            return !blocked
        }
        if (!pump()) return false
        if (!buffer.hasRemaining()) return true
        val targetFrames = out.output.writableBytes / frames.outputFrameBytes
        var budget = (targetFrames.toLong() * frames.inputFrameBytes + trimEndBytes - held.position()).coerceAtLeast(0)
        while (buffer.hasRemaining() && budget > 0) {
            val count = minOf(buffer.remaining().toLong(), held.remaining().toLong(), budget)
                .toInt() / frames.inputFrameBytes * frames.inputFrameBytes
            if (count == 0) break
            val oldLimit = buffer.limit()
            buffer.limit(buffer.position() + count)
            held.put(buffer)
            buffer.limit(oldLimit)
            receivedFrames += count / frames.inputFrameBytes
            budget -= count
            if (!pump()) return false
        }
        return !buffer.hasRemaining()
    }

    // region sink-write
    /** Writes as many whole frames as the output wants now. Returns true when [buffer] is used up. */
    private fun write(out: OpenedOutput, frames: SinkLayout, buffer: ByteBuffer): Boolean {
        if (buffer.remaining() < frames.inputFrameBytes) {
            require(!buffer.hasRemaining()) { "partial PCM frame" }
            return true
        }
        val wanted = out.output.writableBytes / frames.outputFrameBytes
        var count = minOf(buffer.remaining() / frames.inputFrameBytes, wanted)
        if (count == 0) return false // the output is as full as it wants to be: try again on the next call
        if (!frames.needsRepack) {
            val limit = buffer.limit()
            buffer.limit(buffer.position() + count * frames.inputFrameBytes)
            try {
                submittedFrames += out.output.write(buffer) / frames.inputFrameBytes
            } finally {
                buffer.limit(limit)
            }
        } else {
            val stage = staging ?: ByteBuffer.allocateDirect(STAGING_BYTES).order(ByteOrder.LITTLE_ENDIAN).also { staging = it }
            count = minOf(count, stage.capacity() / frames.outputFrameBytes)
            stage.clear()
            repack(buffer, frames.inputBytes, frames.outputBytes, count * frames.channels, stage)
            stage.flip()
            val taken = out.output.write(stage) / frames.outputFrameBytes // whole frames the output took
            buffer.position(buffer.position() + taken * frames.inputFrameBytes)
            submittedFrames += taken
        }
        return !buffer.hasRemaining()
    }

    private fun writeFailure(error: DacException): AudioSink.WriteException {
        val format = activeFormat ?: wanted?.format ?: Format.Builder().build()
        var recoverable = outputs.canRecover(error)
        if (recoverable) {
            try {
                closeActive()
            } catch (closeError: Throwable) {
                error.addSuppressed(closeError)
                recoverable = false
            }
        }
        return AudioSink.WriteException(error.code.ordinal, format, recoverable).also {
            it.initCause(OutputRecoveryRequiredException(error))
        }
    }
    // endregion

    // region sink-switch
    /**
     * Applies what configure() asked for. The same format keeps the output (the gapless case). A different
     * one lets the old output play out first, then closes it and opens a new one. Returns false while the
     * old output is still playing.
     */
    private fun applyPendingConfiguration(): Boolean {
        val next = wanted ?: return true
        val out = active ?: return true // handleBuffer opens one
        // A failed output is retained until a seek/flush so queued audio cannot be silently skipped.
        if (out.failure != null || sameFormat(out.request, next.request)) return true
        try {
            if (!drainingForSwitch) {
                out.output.drain()
                drainingForSwitch = true
            }
            if (!out.output.isDrained) return false
        } catch (error: DacException) {
            out.failure = error
            throw writeFailure(error)
        }
        val replacement = try { outputs.reconfigure(out, next.request) } catch (error: DacException) {
            throw AudioSink.InitializationException("USB reconfiguration failed: ${error.detail}", 0, next.format, false, error)
        }
        if (replacement != null) {
            outputs.onRecoveryPosition(null, null)
            val candidate = replacement.opened
            val candidateLayout = try {
                outputLayout(candidate, if (replacement.rollbackCause == null) next.request else out.request, next.format)
            } catch (error: AudioSink.InitializationException) {
                // A returned replacement is authoritative; the previous wrapper may be retired.
                active = null
                activeFormat = null
                layout = null
                try { outputs.close(candidate) } catch (closeError: Throwable) { error.addSuppressed(closeError) }
                throw error
            }
            active = candidate
            layout = candidateLayout
            if (replacement.rollbackCause == null) activeFormat = next.format
            drainingForSwitch = false
            startMediaTimeUsNeedsInit = true
            submittedFrames = 0
            receivedFrames = 0
            trimmedStartFrames = 0
            replacement.rollbackCause?.let { error ->
                throw AudioSink.InitializationException("requested USB format failed; old format restored paused: ${error.detail}",
                    0, next.format, false, error)
            }
            if (playing) replacement.opened.output.play() else replacement.opened.output.pause()
            return true
        }
        outputs.onRecoveryPosition(null, null)
        closeActive()
        startMediaTimeUsNeedsInit = true
        submittedFrames = 0
        receivedFrames = 0
        trimmedStartFrames = 0
        return true // handleBuffer opens the new one
    }

    private fun sameFormat(a: StreamRequest, b: StreamRequest) =
        a.sampleRate == b.sampleRate && a.channels == b.channels && a.bitDepth == b.bitDepth && a.encoding == b.encoding

    private fun openOutput(): OpenedOutput {
        val next = checkNotNull(wanted) { "configure() comes before the first buffer" }
        val out = try {
            outputs.open(next.request) // blocks for 100 ms or more
        } catch (error: DacException) {
            throw AudioSink.InitializationException("no output can play ${next.request}: ${error.message}", 0, next.format, false, error)
        }
        val candidateLayout = try {
            outputLayout(out, next.request, next.format)
        } catch (error: AudioSink.InitializationException) {
            try { outputs.close(out) } catch (closeError: Throwable) { error.addSuppressed(closeError) }
            throw error
        }
        active = out
        activeFormat = next.format
        layout = candidateLayout
        drainingForSwitch = false
        if (!playing) out.output.pause()
        return out
    }

    private fun outputLayout(out: OpenedOutput, request: StreamRequest, format: Format): SinkLayout {
        val write = out.format
        if (!sameFormat(out.request, request) || write.encoding != request.encoding ||
            (write.encoding == StreamEncoding.PCM_U8 && write.subslotBytes != 1) ||
            (write.encoding == StreamEncoding.PCM_FLOAT32 && write.subslotBytes != 4) ||
            write.subslotBytes !in 1..4 || write.subslotBytes < request.bitDepth / 8 ||
            write.validBits !in request.bitDepth..(write.subslotBytes * 8) ||
            write.channels != request.channels || write.sampleRate != request.sampleRate ||
            write.counterRateHz != request.sampleRate) {
            val message = "the output wants ${write}, which is not a widening of ${request}"
            val cause = IllegalArgumentException(message)
            throw AudioSink.InitializationException(message, 0, format, false, cause)
        }
        return SinkLayout(request.bitDepth / 8, write.subslotBytes, write.channels, write.sampleRate, write.counterRateHz)
    }

    private fun closeActive() {
        val out = active
        active = null
        activeFormat = null
        layout = null
        drainingForSwitch = false
        out?.let { outputs.close(it) }
    }
    // endregion

    // region sink-position
    override fun getCurrentPositionUs(sourceEnded: Boolean): Long {
        val out = active ?: return AudioSink.CURRENT_POSITION_NOT_SET
        val frames = layout ?: return AudioSink.CURRENT_POSITION_NOT_SET
        if (startMediaTimeUsNeedsInit) return AudioSink.CURRENT_POSITION_NOT_SET
        // What the DAC has played since the last flush, never more than what was written.
        val playedUs = out.output.playedFrames * 1_000_000L / frames.counterRateHz
        return (startMediaTimeUs + minOf(playedUs, durationUs(submittedFrames, frames.rateHz))).also {
            if (out.failure == null) outputs.onRecoveryPosition(out, it)
        }
    }

    private fun durationUs(frames: Long, rateHz: Int): Long = frames * 1_000_000L / rateHz

    override fun handleDiscontinuity() {
        startMediaTimeUsNeedsSync = true
    }

    override fun hasPendingData(): Boolean =
        (active?.output?.latencyFrames ?: 0L) > 0L || (trimBuffer?.position() ?: 0) > trimEndBytes
    // endregion

    // region sink-transport
    override fun play() {
        playing = true
        val out = active ?: return
        // Keep the failed output and its position until the host seeks and flushes. Reopening here
        // loses queued PCM that Media3 has already consumed and silently advances the decoder.
        if (out.failure == null) out.output.play()
    }

    override fun pause() {
        playing = false
        active?.takeIf { it.failure == null }?.output?.pause()
    }

    override fun playToEndOfStream() {
        val out = active
        out?.failure?.let { throw writeFailure(it) }
        if (endOfStream) return
        if (out != null) {
            try {
                out.output.drain()
            } catch (error: DacException) {
                out.failure = error
                throw writeFailure(error)
            }
        }
        endOfStream = true
    }

    override fun isEnded(): Boolean {
        val out = active ?: return true
        if (!endOfStream || out.failure != null) return false
        return try { out.output.isDrained } catch (error: DacException) {
            out.failure = error
            false
        }
    }

    override fun flush() {
        outputs.onRecoveryPosition(null, null)
        startMediaTimeUsNeedsInit = true
        startMediaTimeUsNeedsSync = false
        submittedFrames = 0
        receivedFrames = 0
        trimmedStartFrames = 0
        trimBuffer?.clear()
        // A seek must not apply the track's encoder delay again. A pending configure still applies it.
        trimStartBytes = 0
        inputBuffer = null
        endOfStream = false
        drainingForSwitch = false
        val out = active ?: return
        // A seek or a skip is where a changed setting or a newly attached DAC takes effect.
        val reopen = out.failure != null || outputs.takeLookAgain() || wanted?.let { !sameFormat(out.request, it.request) } == true
        if (reopen) closeActive() else out.output.flush()
    }

    override fun reset() {
        flush()
        closeActive()
        wanted = null
    }

    override fun release() = reset()
    // endregion

    // region sink-ignored
    // A direct stream has no AudioTrack: ducking becomes the DAC's volume, the rest is not applied.
    override fun setVolume(volume: Float) = outputs.setVolumeMultiplier(volume)

    // Speed and silence skipping change the samples, so they are refused: the parameters stay at their defaults.
    override fun setPlaybackParameters(playbackParameters: PlaybackParameters) = Unit
    override fun getPlaybackParameters(): PlaybackParameters = PlaybackParameters.DEFAULT
    override fun setSkipSilenceEnabled(skipSilenceEnabled: Boolean) = Unit
    override fun getSkipSilenceEnabled(): Boolean = false

    override fun setListener(listener: AudioSink.Listener) {
        this.listener = listener
    }

    override fun setAudioAttributes(audioAttributes: AudioAttributes) {
        attributes = audioAttributes
    }

    override fun getAudioAttributes(): AudioAttributes? = attributes
    override fun getAudioTrackBufferSizeUs(): Long = C.TIME_UNSET
    override fun setAudioSessionId(audioSessionId: Int) = Unit
    override fun setAuxEffectInfo(auxEffectInfo: AuxEffectInfo) = Unit
    override fun setPreferredDevice(audioDeviceInfo: AudioDeviceInfo?) = Unit
    override fun enableTunnelingV21() = Unit
    override fun disableTunneling() = Unit
    // endregion

    private companion object {
        const val STAGING_BYTES = 64 * 1024
        const val MAX_PADDING_BYTES = 16 * 1024 * 1024
    }
}
