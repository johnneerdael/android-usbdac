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
    private var targetFailure: DacException? = null
    private inline fun <T> targetCall(block: () -> T): T {
        targetFailure?.let { throw it }
        return try { block() } catch (error: DacException) { targetFailure = error; throw error }
    }
    override val bitPerfect: StateFlow<BitPerfectState> get() = target.bitPerfect
    private fun pump() {
        pending.flip()
        try { targetCall { target.write(pending) } } finally { pending.compact() }
    }
    override fun write(buffer: ByteBuffer): Int = synchronized(lock) {
        if (closed) throw DacException(DacErrorCode.INVALID_STATE, "output is closed or draining")
        targetFailure?.let { throw it }
        if (draining) throw DacException(DacErrorCode.INVALID_STATE, "output is closed or draining")
        pump()
        val count = minOf(buffer.remaining(), pending.remaining()).let { it - it % channels }
        if (count == 0) return 0
        pending.put(buffer.duplicate().apply { limit(position() + count) })
        buffer.position(buffer.position() + count)
        try { pump() } catch (_: DacException) { return@synchronized count }
        count
    }
    override val writableBytes: Int get() = synchronized(lock) {
        if (closed) return 0
        targetFailure?.let { throw it }
        if (draining) return 0
        // PcmOutput.writableBytes is the room up to the target depth: the staged bytes of an
        // incomplete wire word already use part of the target's room, as in the other adapters.
        pump()
        minOf(pending.remaining(), (targetCall { target.writableBytes } - pending.position()).coerceAtLeast(0)).let { it - it % channels }
    }
    override fun play() = synchronized(lock) { if (closed) throw DacException(DacErrorCode.INVALID_STATE, "output closed"); targetCall { target.play() } }
    override fun pause() = synchronized(lock) { if (closed) throw DacException(DacErrorCode.INVALID_STATE, "output closed"); targetCall { target.pause() } }
    override fun flush() = synchronized(lock) { if (closed) throw DacException(DacErrorCode.INVALID_STATE, "output closed"); targetCall { target.flush() }; pending.clear(); draining = false; targetDrained = false }
    private fun pumpDrain() { pump(); if (pending.position() == 0 && !targetDrained) { targetCall { target.drain() }; targetDrained = true } }
    override fun drain() = synchronized(lock) {
        if (closed) throw DacException(DacErrorCode.INVALID_STATE, "output closed")
        if (pending.position() % wireFrame != 0) throw DacException(DacErrorCode.INVALID_ARGUMENT, "incomplete native DSD wire word")
        draining = true; pumpDrain()
    }
    override val isDrained: Boolean get() = synchronized(lock) { if (closed) false else { targetFailure?.let { throw it }; if (!draining) false else { pumpDrain(); targetDrained && targetCall { target.isDrained } } } }
    override val playedFrames: Long get() = synchronized(lock) { if (closed) 0 else target.playedFrames }
    override val latencyFrames: Long get() = synchronized(lock) { if (closed) 0 else target.latencyFrames + pending.position() / channels }
    override fun close() = synchronized(lock) { if (!closed) { closed = true; target.close() } }
}
