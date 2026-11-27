package dev.usbdac

import java.nio.ByteBuffer
import kotlinx.coroutines.flow.StateFlow

/** Validate and unpack DoP carrier frames to native chronological DSD byte-frames.
 * Input is packed LE 24-bit or left-justified 24-in-32; every channel must carry
 * the same 05/FA marker, alternating per frame. Four-byte padding must be zero.
 * A partial carrier frame is not consumed. Counters use carrier frames at bitRate/16.
 */
public class DopInputOutput(private val target: PcmOutput, private val channels: Int, private val slotBytes: Int) : PcmOutput {
    init { require(channels in 1..8); require(slotBytes == 3 || slotBytes == 4) }
    private val lock = Any()
    private val pending = ByteBuffer.allocateDirect(channels * 2).apply { limit(0) }
    private var expected: Int? = null
    private var draining = false
    private var targetDrained = false
    private var closed = false
    /** PCM quantizer clipping when the downstream output exposes it. */
    public val clippedSamples: Long? get() = (target as? DsdPcmOutput)?.clippedSamples
    override val bitPerfect: StateFlow<BitPerfectState> get() = target.bitPerfect
    private fun pump() { if (pending.hasRemaining()) target.write(pending) }
    override fun write(buffer: ByteBuffer): Int = synchronized(lock) {
        check(!closed && !draining); pump(); val start = buffer.position()
        // Validate the whole available prefix before consuming any of it.
        val checkedLimit = start + minOf(buffer.remaining(), 64 * 1024)
        var checkAt = start
        var next = expected
        while (checkAt + channels * slotBytes <= checkedLimit) {
            val pad = slotBytes - 3
            val marker = buffer.get(checkAt + pad + 2).toInt() and 255
            require(marker == 0x05 || marker == 0xfa) { "invalid DoP marker" }
            require(next == null || marker == next) { "DoP marker did not alternate" }
            for (ch in 0 until channels) {
                val at = checkAt + ch * slotBytes
                require((buffer.get(at + pad + 2).toInt() and 255) == marker) { "DoP channel markers differ" }
                if (pad != 0) require(buffer.get(at).toInt() == 0) { "DoP padding must be zero" }
            }
            next = marker xor 0xff
            checkAt += channels * slotBytes
        }
        while (!pending.hasRemaining() && buffer.position() + channels * slotBytes <= checkAt) {
            val p = buffer.position(); val pad = slotBytes - 3
            val marker = buffer.get(p + pad + 2).toInt() and 255
            require(marker == 0x05 || marker == 0xfa) { "invalid DoP marker" }
            require(expected == null || marker == expected) { "DoP marker did not alternate" }
            for (ch in 0 until channels) {
                val at = p + ch * slotBytes
                require((buffer.get(at + pad + 2).toInt() and 255) == marker) { "DoP channel markers differ" }
                if (pad != 0) require(buffer.get(at).toInt() == 0) { "DoP padding must be zero" }
            }
            pending.clear()
            for (ch in 0 until channels) pending.put(buffer.get(p + ch * slotBytes + pad + 1))
            for (ch in 0 until channels) pending.put(buffer.get(p + ch * slotBytes + pad))
            buffer.position(p + channels * slotBytes); expected = marker xor 0xff
            pending.flip(); pump()
        }
        buffer.position() - start
    }
    override val writableBytes: Int get() = synchronized(lock) {
        if (closed || draining) 0 else { pump(); if (pending.hasRemaining()) 0 else
            ((target.writableBytes / (channels * 2)).toLong() * channels * slotBytes).coerceAtMost(Int.MAX_VALUE.toLong()).toInt() }
    }
    override fun play(): Unit = synchronized(lock) { check(!closed); target.play() }
    override fun pause(): Unit = synchronized(lock) { check(!closed); target.pause() }
    override fun flush(): Unit = synchronized(lock) { check(!closed); target.flush(); pending.clear(); pending.limit(0); expected = null; draining = false; targetDrained = false }
    override fun drain(): Unit = synchronized(lock) { check(!closed); draining = true; pump(); if (!pending.hasRemaining() && !targetDrained) { target.drain(); targetDrained = true } }
    override val isDrained: Boolean get() = synchronized(lock) { if (closed || !draining) false else { pump(); if (!pending.hasRemaining() && !targetDrained) { target.drain(); targetDrained = true }; targetDrained && target.isDrained } }
    override val playedFrames: Long get() = synchronized(lock) { if (closed) 0 else target.playedFrames / 2 }
    override val latencyFrames: Long get() = synchronized(lock) { if (closed) 0 else (target.latencyFrames + pending.remaining() / channels + 1) / 2 }
    override fun close(): Unit = synchronized(lock) { if (!closed) { closed = true; target.close() } }
}
