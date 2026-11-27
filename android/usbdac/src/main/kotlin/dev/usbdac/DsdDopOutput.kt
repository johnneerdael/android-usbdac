package dev.usbdac

import java.nio.ByteBuffer
import kotlinx.coroutines.flow.StateFlow

/** Exact native DSD to DoP framing for a route independently confirmed to decode DoP.
 * Input is interleaved chronological DSD bytes, oldest bit in bit 7. Two byte-frames
 * form one carrier frame. An incomplete pair remains unconsumed for the next write.
 * Positions and latency are in DSD byte-frames at the bit rate divided by eight.
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
    private val pending = ByteBuffer.allocateDirect(channels * slotBytes).apply { limit(0) }
    private var marker = 0x05
    private var draining = false
    private var targetDrained = false
    private var closed = false
    override val bitPerfect: StateFlow<BitPerfectState> get() = target.bitPerfect

    private fun pump() { if (pending.hasRemaining()) target.write(pending) }
    override fun write(buffer: ByteBuffer): Int = synchronized(lock) {
        check(!closed && !draining)
        pump()
        val start = buffer.position()
        while (!pending.hasRemaining() && buffer.remaining() >= channels * 2) {
            val p = buffer.position()
            pending.clear()
            for (ch in 0 until channels) {
                if (slotBytes == 4) pending.put(0)
                pending.put(buffer.get(p + channels + ch)) // newer byte is low in the word
                pending.put(buffer.get(p + ch))
                pending.put(marker.toByte())
            }
            buffer.position(p + channels * 2)
            marker = marker xor 0xff
            pending.flip()
            pump()
        }
        buffer.position() - start
    }
    override val writableBytes: Int get() = synchronized(lock) {
        if (closed || draining) 0 else {
            pump()
            if (pending.hasRemaining()) 0 else ((target.writableBytes / (channels * slotBytes)).toLong() * channels * 2).coerceAtMost(Int.MAX_VALUE.toLong()).toInt()
        }
    }
    override fun play(): Unit = synchronized(lock) { check(!closed); target.play() }
    override fun pause(): Unit = synchronized(lock) { check(!closed); target.pause() }
    override fun flush(): Unit = synchronized(lock) {
        check(!closed)
        pending.clear(); pending.limit(0); marker = 0x05; draining = false; targetDrained = false; target.flush()
    }
    override fun drain(): Unit = synchronized(lock) { check(!closed); draining = true; pump(); if (!pending.hasRemaining() && !targetDrained) { target.drain(); targetDrained = true } }
    override val isDrained: Boolean get() = synchronized(lock) {
        if (closed || !draining) false else { pump(); if (!pending.hasRemaining()) { if (!targetDrained) { target.drain(); targetDrained = true }; target.isDrained } else false }
    }
    override val playedFrames: Long get() = synchronized(lock) { if (closed) 0 else target.playedFrames * 2 }
    override val latencyFrames: Long get() = synchronized(lock) { if (closed) 0 else (target.latencyFrames + if (pending.hasRemaining()) 1 else 0) * 2 }
    override fun close(): Unit = synchronized(lock) { if (!closed) { closed = true; target.close() } }
}
