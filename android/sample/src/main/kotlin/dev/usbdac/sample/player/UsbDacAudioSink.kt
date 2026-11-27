package dev.usbdac.sample.player

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
import java.nio.ByteBuffer
import java.nio.ByteOrder

// region sink-layout
/**
 * How Media3's PCM maps onto what the output wants. Media3 hands over little-endian PCM in 2, 3 or 4
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
 * change, no silence skipping. Stereo and mono integer PCM only.
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
    private var layout: SinkLayout? = null
    private var staging: ByteBuffer? = null

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

    override fun getFormatSupport(format: Format): Int =
        if (format.sampleMimeType == MimeTypes.AUDIO_RAW && bitDepthOf(format.pcmEncoding) != null && format.channelCount in 1..2) {
            AudioSink.SINK_FORMAT_SUPPORTED_DIRECTLY
        } else {
            AudioSink.SINK_FORMAT_UNSUPPORTED // float, 8-bit, more than two channels, compressed audio
        }

    private fun bitDepthOf(pcmEncoding: Int): Int? = when (pcmEncoding) {
        C.ENCODING_PCM_16BIT -> 16
        C.ENCODING_PCM_24BIT -> 24
        C.ENCODING_PCM_32BIT -> 32
        else -> null
    }
    // endregion

    // region sink-configure
    override fun configure(audioSinkConfig: AudioSink.AudioSinkConfig) {
        val format = audioSinkConfig.format
        val bits = bitDepthOf(format.pcmEncoding)
        if (format.sampleMimeType != MimeTypes.AUDIO_RAW || bits == null || format.channelCount !in 1..2) {
            throw AudioSink.ConfigurationException("not integer PCM in one or two channels", format)
        }
        // Nothing is opened here. The output opens in handleBuffer, after the one in use has played out.
        wanted = Wanted(format, StreamRequest.pcm(format.sampleRate, format.channelCount, bits))
    }
    // endregion

    // region sink-handle-buffer
    override fun handleBuffer(buffer: ByteBuffer, presentationTimeUs: Long, encodedAccessUnitCount: Int): Boolean {
        require(inputBuffer == null || buffer === inputBuffer)
        if (!applyPendingConfiguration()) return false
        val out = active ?: openOutput()
        val frames = layout!!

        if (startMediaTimeUsNeedsInit) {
            startMediaTimeUs = maxOf(0L, presentationTimeUs)
            startMediaTimeUsNeedsSync = false
            startMediaTimeUsNeedsInit = false
        }
        if (inputBuffer == null) {
            // The first time this buffer is offered. Check that it follows the audio written so far.
            if (!buffer.hasRemaining()) return true
            val expectedUs = startMediaTimeUs + durationUs(submittedFrames, frames.rateHz)
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
            // listener's next play, which opens a new output.
            if (playing) throw writeFailure(failure)
            return false
        }
        val done = try {
            write(out, frames, buffer)
        } catch (error: DacException) {
            out.failure = error
            throw writeFailure(error)
        }
        if (done) inputBuffer = null
        return done
    }
    // endregion

    // region sink-write
    /** Writes as many whole frames as the output wants now. Returns true when [buffer] is used up. */
    private fun write(out: OpenedOutput, frames: SinkLayout, buffer: ByteBuffer): Boolean {
        if (buffer.remaining() < frames.inputFrameBytes) {
            buffer.position(buffer.limit()) // a partial frame cannot be played
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

    private fun writeFailure(error: DacException): AudioSink.WriteException =
        AudioSink.WriteException(error.code.ordinal, wanted?.format ?: Format.Builder().build(), false).also { it.initCause(error) }
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
        // A failed output is not replaced here: handleBuffer reports it, and the listener's play opens a new one.
        if (out.failure != null || sameFormat(out.request, next.request)) return true
        if (!drainingForSwitch) {
            out.output.drain()
            drainingForSwitch = true
        }
        if (!out.output.isDrained) return false
        closeActive()
        return true // handleBuffer opens the new one
    }

    private fun sameFormat(a: StreamRequest, b: StreamRequest) =
        a.sampleRate == b.sampleRate && a.channels == b.channels && a.bitDepth == b.bitDepth

    private fun openOutput(): OpenedOutput {
        val next = checkNotNull(wanted) { "configure() comes before the first buffer" }
        val out = try {
            outputs.open(next.request) // blocks for 100 ms or more
        } catch (error: DacException) {
            throw AudioSink.InitializationException("no output can play ${next.request}: ${error.message}", 0, next.format, false, error)
        }
        val write = out.format
        if (write.subslotBytes < next.request.bitDepth / 8 || write.channels != next.request.channels) {
            outputs.close(out)
            throw AudioSink.InitializationException("the output wants ${write}, which is not a widening of ${next.request}", 0, next.format, false, null)
        }
        active = out
        drainingForSwitch = false
        layout = SinkLayout(next.request.bitDepth / 8, write.subslotBytes, write.channels, write.sampleRate, write.counterRateHz)
        if (!playing) out.output.pause()
        return out
    }

    private fun closeActive() {
        active?.let { outputs.close(it) }
        active = null
        layout = null
        drainingForSwitch = false
    }
    // endregion

    // region sink-position
    override fun getCurrentPositionUs(sourceEnded: Boolean): Long {
        val out = active ?: return AudioSink.CURRENT_POSITION_NOT_SET
        val frames = layout ?: return AudioSink.CURRENT_POSITION_NOT_SET
        if (startMediaTimeUsNeedsInit) return AudioSink.CURRENT_POSITION_NOT_SET
        // What the DAC has played since the last flush, never more than what was written.
        val playedUs = out.output.playedFrames * 1_000_000L / frames.counterRateHz
        return startMediaTimeUs + minOf(playedUs, durationUs(submittedFrames, frames.rateHz))
    }

    private fun durationUs(frames: Long, rateHz: Int): Long = frames * 1_000_000L / rateHz

    override fun handleDiscontinuity() {
        startMediaTimeUsNeedsSync = true
    }

    override fun hasPendingData(): Boolean = (active?.output?.latencyFrames ?: 0L) > 0L
    // endregion

    // region sink-transport
    override fun play() {
        playing = true
        val out = active ?: return
        // Play after a failure is the listener's decision to try again: the next buffer opens a new output.
        if (out.failure != null) closeActive() else out.output.play()
    }

    override fun pause() {
        playing = false
        active?.takeIf { it.failure == null }?.output?.pause()
    }

    override fun playToEndOfStream() {
        if (endOfStream) return
        val out = active
        if (out != null && out.failure == null) {
            try {
                out.output.drain()
            } catch (error: DacException) {
                throw writeFailure(error)
            }
        }
        endOfStream = true
    }

    override fun isEnded(): Boolean {
        val out = active ?: return true
        return endOfStream && (out.failure != null || out.output.isDrained)
    }

    override fun flush() {
        startMediaTimeUsNeedsInit = true
        startMediaTimeUsNeedsSync = false
        submittedFrames = 0
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
    }
}
