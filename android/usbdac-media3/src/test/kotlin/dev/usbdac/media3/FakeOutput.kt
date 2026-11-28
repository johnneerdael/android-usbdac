package dev.usbdac.media3

import dev.usbdac.media3.*

import dev.usbdac.BitPerfectState
import dev.usbdac.DacErrorCode
import dev.usbdac.DacException
import dev.usbdac.PcmOutput
import dev.usbdac.StreamRequest
import dev.usbdac.WriteFormat
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import java.io.ByteArrayOutputStream
import java.nio.ByteBuffer

// region fake-output
/**
 * A [PcmOutput] that records what the player does to it, so the sink, the seek order and the track
 * changes can be tested with no DAC. It lives in test source only so that the guide can quote it; put
 * it in your test source set.
 */
class FakePcmOutput(private val bytesPerFrame: Int, private val room: Int = Int.MAX_VALUE) : PcmOutput {
    /** The calls in order: "write 4800", "pause", "flush", ... */
    val calls = mutableListOf<String>()
    val written = ByteArrayOutputStream()

    /** Set by a test to say how far the DAC has played. */
    @Volatile
    var played = 0L

    var drained = false
    var closed = false
    var failWith: DacException? = null
    var failAfterFrames: Int? = null

    override fun write(buffer: ByteBuffer): Int {
        if (failAfterFrames == null) failWith?.let { throw it }
        if (closed) throw DacException(DacErrorCode.INVALID_STATE, "closed")
        val taken = minOf(buffer.remaining() / bytesPerFrame, failAfterFrames ?: Int.MAX_VALUE) * bytesPerFrame
        val bytes = ByteArray(taken)
        buffer.get(bytes)
        written.write(bytes)
        calls += "write $taken"
        failWith?.let { throw it }
        return taken
    }

    override val writableBytes: Int get() = room
    override fun play() { calls += "play" }
    override fun pause() { calls += "pause" }
    override fun flush() { calls += "flush"; played = 0; written.reset() }
    override fun drain() { calls += "drain" }
    override val isDrained: Boolean get() = drained
    override val playedFrames: Long get() = played
    override val latencyFrames: Long get() = written.size() / bytesPerFrame - played
    override val bitPerfect: StateFlow<BitPerfectState> = MutableStateFlow(BitPerfectState.Verified)
    override fun close() { closed = true; calls += "close" }
}

/** An [OutputSource] that opens a [FakePcmOutput] in the layout a test chooses. */
class FakeOutputSource(private val wireBytes: Int? = null, private val room: Int = Int.MAX_VALUE) : OutputSource {
    val opened = mutableListOf<FakePcmOutput>()
    private val handles = mutableListOf<OpenedOutput>()
    var lookAgain = false
    var multiplier = 1f

    override fun open(request: StreamRequest): OpenedOutput {
        val sampleBytes = wireBytes ?: (request.bitDepth / 8)
        val fake = FakePcmOutput(sampleBytes * request.channels, room)
        opened += fake
        val format = WriteFormat(request.sampleRate, request.channels, sampleBytes, request.bitDepth, request.sampleRate)
        return OpenedOutput(fake, format, request).also { handles += it }
    }

    /** What the controller does when the session reports `DeviceLost` or `StreamFailed`. */
    fun failure(index: Int, error: DacException) {
        handles[index].failure = error
    }

    override fun close(output: OpenedOutput) = output.output.close()
    override fun takeLookAgain(): Boolean = lookAgain.also { lookAgain = false }
    override fun setVolumeMultiplier(multiplier: Float) { this.multiplier = multiplier }
}
// endregion
