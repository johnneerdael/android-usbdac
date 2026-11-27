package dev.usbdac

import kotlinx.coroutines.flow.StateFlow
import java.nio.ByteBuffer

/** Accumulate chronological DSD byte-frames until the native wire word is complete. */
internal class NativeDsdInputOutput(private val target: PcmOutput, private val channels: Int, wordBytes: Int) : PcmOutput {
    private val lock = Any()
    private val wireFrame = channels * wordBytes
    private val pending = ByteBuffer.allocateDirect(maxOf(wireFrame, (64 * 1024 / wireFrame) * wireFrame))
    private var closed = false
    private var draining = false
    private var targetDrained = false
    override val bitPerfect: StateFlow<BitPerfectState> get() = target.bitPerfect
    private fun pump() { pending.flip(); target.write(pending); pending.compact() }
    override fun write(buffer: ByteBuffer): Int = synchronized(lock) {
        if (closed || draining) throw DacException(DacErrorCode.INVALID_STATE, "output is closed or draining")
        pump()
        val count = minOf(buffer.remaining(), pending.remaining()).let { it - it % channels }
        if (count == 0) return 0
        pending.put(buffer.duplicate().apply { limit(position() + count) })
        buffer.position(buffer.position() + count)
        pump()
        count
    }
    override val writableBytes: Int get() = synchronized(lock) {
        if (closed || draining) return 0
        pump(); pending.remaining().let { it - it % channels }
    }
    override fun play() = synchronized(lock) { if (closed) throw DacException(DacErrorCode.INVALID_STATE, "output closed"); target.play() }
    override fun pause() = synchronized(lock) { if (closed) throw DacException(DacErrorCode.INVALID_STATE, "output closed"); target.pause() }
    override fun flush() = synchronized(lock) { if (closed) throw DacException(DacErrorCode.INVALID_STATE, "output closed"); target.flush(); pending.clear(); draining = false; targetDrained = false }
    private fun pumpDrain() { pump(); if (pending.position() == 0 && !targetDrained) { target.drain(); targetDrained = true } }
    override fun drain() = synchronized(lock) {
        if (closed) throw DacException(DacErrorCode.INVALID_STATE, "output closed")
        if (pending.position() % wireFrame != 0) throw DacException(DacErrorCode.INVALID_ARGUMENT, "incomplete native DSD wire word")
        draining = true; pumpDrain()
    }
    override val isDrained: Boolean get() = synchronized(lock) { if (closed || !draining) false else { pumpDrain(); targetDrained && target.isDrained } }
    override val playedFrames: Long get() = synchronized(lock) { if (closed) 0 else target.playedFrames }
    override val latencyFrames: Long get() = synchronized(lock) { if (closed) 0 else target.latencyFrames + pending.position() / channels }
    override fun close() = synchronized(lock) { if (!closed) { closed = true; target.close() } }
}
