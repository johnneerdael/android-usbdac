package dev.usbdac

import java.nio.ByteBuffer
import kotlinx.coroutines.flow.StateFlow

/** Validate and unpack DoP frames to chronological native DSD byte-frames.
 * Input is packed LE 24-bit or left-justified 24-in-32. All channels must
 * share the alternating 05/FA marker; four-byte padding must be zero. Partial
 * carrier frames are not consumed. Counters use carrier frames at bitRate/16.
 */
public class DopInputOutput(
    private val target: PcmOutput,
    private val channels: Int,
    private val slotBytes: Int,
) : PcmOutput {
    init {
        require(channels in 1..8)
        require(slotBytes == 3 || slotBytes == 4)
    }

    private val lock = Any()
    private val carrierFrame = channels * slotBytes
    private val nativePair = channels * 2
    private val pending = ByteBuffer.allocateDirect((64 * 1024 / nativePair) * nativePair).apply { limit(0) }
    private var expected: Int? = null
    private var draining = false
    private var targetDrained = false
    private var closed = false

    /** PCM quantizer clipping when the downstream output exposes it. */
    public val clippedSamples: Long? get() = (target as? DsdPcmOutput)?.clippedSamples
    override val bitPerfect: StateFlow<BitPerfectState> get() = target.bitPerfect

    private fun pump() {
        if (pending.hasRemaining()) target.write(pending)
    }

    private fun invalid(message: String): Nothing = throw DacException(DacErrorCode.INVALID_ARGUMENT, message)

    override fun write(buffer: ByteBuffer): Int = synchronized(lock) {
        check(!closed && !draining)
        pump()
        if (pending.hasRemaining()) return 0
        val start = buffer.position()
        val submittedFrames = buffer.remaining() / carrierFrame
        val frames = minOf(submittedFrames, pending.capacity() / nativePair)
        val pad = slotBytes - 3
        var next = expected
        // Validate only the bounded batch that can be accepted in this call.
        for (frame in 0 until frames) {
            val p = start + frame * carrierFrame
            val marker = buffer.get(p + pad + 2).toInt() and 255
            if (marker != 0x05 && marker != 0xfa) invalid("invalid DoP marker")
            if (next != null && marker != next) invalid("DoP marker did not alternate")
            for (ch in 0 until channels) {
                val at = p + ch * slotBytes
                if ((buffer.get(at + pad + 2).toInt() and 255) != marker) invalid("DoP channel markers differ")
                if (pad != 0 && buffer.get(at).toInt() != 0) invalid("DoP padding must be zero")
            }
            next = marker xor 0xff
        }
        pending.clear()
        for (frame in 0 until frames) {
            val p = start + frame * carrierFrame
            for (ch in 0 until channels) pending.put(buffer.get(p + ch * slotBytes + pad + 1))
            for (ch in 0 until channels) pending.put(buffer.get(p + ch * slotBytes + pad))
        }
        buffer.position(start + frames * carrierFrame)
        expected = next
        pending.flip()
        pump()
        buffer.position() - start
    }

    override val writableBytes: Int get() = synchronized(lock) {
        if (closed || draining) return 0
        pump()
        if (pending.hasRemaining()) return 0
        ((target.writableBytes / nativePair).toLong() * carrierFrame)
            .coerceAtMost(Int.MAX_VALUE.toLong()).toInt()
    }

    override fun play(): Unit = synchronized(lock) {
        check(!closed)
        target.play()
    }

    override fun pause(): Unit = synchronized(lock) {
        check(!closed)
        target.pause()
    }

    /** Start a new input marker sequence without discarding already accepted downstream data. */
    public fun resetInputMarkerPhase(): Unit = synchronized(lock) {
        check(!closed && !draining)
        expected = null
    }

    override fun flush(): Unit = synchronized(lock) {
        check(!closed)
        target.flush()
        pending.clear()
        pending.limit(0)
        expected = null
        draining = false
        targetDrained = false
    }

    private fun pumpDrain() {
        pump()
        if (!pending.hasRemaining() && !targetDrained) {
            target.drain()
            targetDrained = true
        }
    }

    override fun drain(): Unit = synchronized(lock) {
        check(!closed)
        draining = true
        pumpDrain()
    }

    override val isDrained: Boolean get() = synchronized(lock) {
        if (closed || !draining) return false
        pumpDrain()
        targetDrained && target.isDrained
    }

    override val playedFrames: Long get() = synchronized(lock) {
        if (closed) 0 else target.playedFrames / 2
    }

    override val latencyFrames: Long get() = synchronized(lock) {
        if (closed) 0 else (target.latencyFrames + pending.remaining() / channels + 1) / 2
    }

    override fun close(): Unit = synchronized(lock) {
        if (!closed) {
            closed = true
            target.close()
        }
    }
}
