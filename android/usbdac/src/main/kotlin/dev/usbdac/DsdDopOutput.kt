package dev.usbdac

import java.nio.ByteBuffer
import kotlinx.coroutines.flow.StateFlow

/** Exact native DSD to DoP framing for an independently confirmed DoP route.
 * Input is chronological interleaved bytes with the oldest bit in bit 7. One
 * byte-frame can be held across writes; drain rejects an incomplete final pair.
 * Position and latency use DSD byte-frames at the bit rate divided by eight.
 */
public class DsdDopOutput(
    private val target: PcmOutput,
    private val channels: Int,
    private val slotBytes: Int,
    confirmedDop: Boolean,
) : PcmOutput {
    init {
        require(confirmedDop) { "DoP decoding must be independently confirmed" }
        require(channels in 1..8)
        require(slotBytes == 3 || slotBytes == 4)
    }

    private val lock = Any()
    private val carrierFrame = channels * slotBytes
    private val pending = ByteBuffer.allocateDirect((64 * 1024 / carrierFrame) * carrierFrame).apply { limit(0) }
    private val first = ByteArray(channels)
    private var haveFirst = false
    private var marker = 0x05
    private var draining = false
    private var targetDrained = false
    private var closed = false
    override val bitPerfect: StateFlow<BitPerfectState> get() = target.bitPerfect

    private fun pump() {
        if (pending.hasRemaining()) target.write(pending)
    }

    override fun write(buffer: ByteBuffer): Int = synchronized(lock) {
        check(!closed && !draining)
        pump()
        if (pending.hasRemaining()) return 0
        val start = buffer.position()
        pending.clear()
        while (buffer.remaining() >= channels && pending.remaining() >= carrierFrame) {
            if (!haveFirst) {
                buffer.get(first)
                haveFirst = true
                continue
            }
            val p = buffer.position()
            for (ch in 0 until channels) {
                if (slotBytes == 4) pending.put(0)
                pending.put(buffer.get(p + ch))
                pending.put(first[ch])
                pending.put(marker.toByte())
            }
            buffer.position(p + channels)
            haveFirst = false
            marker = marker xor 0xff
        }
        pending.flip()
        pump()
        buffer.position() - start
    }

    override val writableBytes: Int get() = synchronized(lock) {
        if (closed || draining) return 0
        pump()
        if (pending.hasRemaining()) return 0
        val frames = target.writableBytes / carrierFrame
        ((frames.toLong() * 2 + if (haveFirst) 0 else 1) * channels)
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

    override fun flush(): Unit = synchronized(lock) {
        check(!closed)
        target.flush()
        pending.clear()
        pending.limit(0)
        marker = 0x05
        haveFirst = false
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
        check(!haveFirst) { "incomplete final native DSD byte-frame pair" }
        draining = true
        pumpDrain()
    }

    override val isDrained: Boolean get() = synchronized(lock) {
        if (closed || !draining) return false
        pumpDrain()
        targetDrained && target.isDrained
    }

    override val playedFrames: Long get() = synchronized(lock) {
        if (closed) 0 else target.playedFrames * 2
    }

    override val latencyFrames: Long get() = synchronized(lock) {
        if (closed) 0 else (target.latencyFrames + pending.remaining() / carrierFrame) * 2 + if (haveFirst) 1 else 0
    }

    override fun close(): Unit = synchronized(lock) {
        if (!closed) {
            closed = true
            target.close()
        }
    }
}
