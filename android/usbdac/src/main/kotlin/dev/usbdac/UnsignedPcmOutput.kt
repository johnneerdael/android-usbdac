package dev.usbdac

import dev.usbdac.internal.Native
import kotlinx.coroutines.flow.StateFlow
import java.nio.ByteBuffer

/** Expands unsigned PCM8 to exact signed PCM16 before the driver's selected conversion. */
internal class UnsignedPcmOutput(private val target: PcmOutput, private val channels: Int) : PcmOutput {
    private val expand: (ByteBuffer, ByteBuffer, Int) -> Int = { input, output, count -> Native.playbackU8ToI16(input, output, count) }
    private val lock = Any()
    private val input = ByteBuffer.allocateDirect(32 * 1024)
    private val pending = ByteBuffer.allocateDirect(64 * 1024).apply { limit(0) }
    private var closed = false
    private var draining = false
    private var targetDrained = false
    val clippedSamples: Long? get() = (target as? ConvertingOutput)?.clippedSamples
    override val bitPerfect: StateFlow<BitPerfectState> get() = target.bitPerfect
    private fun pump() { if (pending.hasRemaining()) target.write(pending) }
    override fun write(buffer: ByteBuffer): Int = synchronized(lock) {
        if (closed || draining) throw DacException(DacErrorCode.INVALID_STATE, "output is closed or draining")
        pump()
        if (pending.hasRemaining()) return 0
        val count = minOf(buffer.remaining(), input.capacity()).let { it - it % channels }
        if (count == 0) return 0
        input.clear()
        input.put(buffer.duplicate().apply { limit(position() + count) })
        pending.clear()
        val consumed = nativeCall { expand(input, pending, count) }
        check(consumed == count) { "unsigned PCM conversion did not consume whole frames" }
        pending.position(0); pending.limit(consumed * 2)
        buffer.position(buffer.position() + consumed)
        pump()
        consumed
    }
    override val writableBytes: Int get() = synchronized(lock) {
        if (closed || draining) return 0
        pump()
        if (pending.hasRemaining()) 0 else (target.writableBytes / 2).let { it - it % channels }
    }
    override fun play() = synchronized(lock) { if (closed) throw DacException(DacErrorCode.INVALID_STATE, "output closed"); target.play() }
    override fun pause() = synchronized(lock) { if (closed) throw DacException(DacErrorCode.INVALID_STATE, "output closed"); target.pause() }
    override fun flush() = synchronized(lock) {
        if (closed) throw DacException(DacErrorCode.INVALID_STATE, "output closed")
        target.flush(); pending.clear(); pending.limit(0); draining = false; targetDrained = false
    }
    private fun pumpDrain() { pump(); if (!pending.hasRemaining() && !targetDrained) { target.drain(); targetDrained = true } }
    override fun drain() = synchronized(lock) { if (closed) throw DacException(DacErrorCode.INVALID_STATE, "output closed"); draining = true; pumpDrain() }
    override val isDrained: Boolean get() = synchronized(lock) { if (closed || !draining) false else { pumpDrain(); targetDrained && target.isDrained } }
    override val playedFrames: Long get() = synchronized(lock) { if (closed) 0 else target.playedFrames }
    override val latencyFrames: Long get() = synchronized(lock) { if (closed) 0 else target.latencyFrames + pending.remaining() / (channels * 2) }
    override fun close() = synchronized(lock) { if (!closed) { closed = true; target.close() } }
}
