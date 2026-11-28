package dev.usbdac

import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import java.nio.ByteBuffer

/**
 * A [PcmOutput] that converts what it is given before passing it on to [target], with a
 * [PcmConverter]: the way to play a source the DAC does not run as it is (a rate it refuses, a depth it
 * lacks).
 *
 * The host writes the **source** format ([ConversionPlan.sourceSampleRate], [ConversionPlan.sourceFrameBytes]) and
 * everything the output reports is in source units: [write] takes source bytes, [writableBytes] says how many
 * source bytes the target has room for, and [playedFrames] and [latencyFrames] are source frames
 * (target frames scaled by the rate ratio, plus, for the latency, the audio held in the converter),
 * so a player's position arithmetic is unchanged: `position = start + playedFrames / sourceRate`.
 *
 * ### Bit-perfect
 * [bitPerfect] is the target's when the plan is [ConversionKind.NONE] or [ConversionKind.BIT_EXACT]
 * (widening and repacking keep every value). Otherwise it is
 * [BitPerfectState.NotVerified] with [ConversionPlan.description]: **the stream is not bit-perfect**, and the
 * flag says what was done instead. For [ConversionKind.NONE] the output forwards to the target unless declared padding needs validation.
 *
 * ### Controls
 * [flush] also resets the converter (a seek moves the source: the filter's history belongs to the old position).
 * [drain] converts the resampler's tail and then drains the target; keep polling [isDrained] (or call [writableBytes]),
 * which also moves the tail along when the target had no room for it at the time.
 * [close] closes the target and the converter.
 *
 * ### Threading
 * As for [PcmOutput]: one writer thread, the other calls from any thread (they take the same lock, and
 * do not block for long). A write converts at most what the target has room for, in chunks, on the
 * calling thread.
 */
public class ConvertingOutput(
    private val target: PcmOutput,
    private val converter: PcmConverter,
) : PcmOutput {
    /** What is being done to the audio. */
    public val plan: ConversionPlan get() = converter.plan

    /** Saturated integer samples since the last reset or flush. */
    public val clippedSamples: Long get() = synchronized(lock) { converter.clippedSamples }

    private val lock = Any()
    private val passthrough = plan.kind == ConversionKind.NONE && !plan.requiresPaddingValidation
    private val sourceRate = plan.sourceSampleRate.toLong()
    private val targetRate = plan.targetSampleRate.toLong()
    private val sourceFrameBytes = plan.sourceFrameBytes
    private val targetFrameBytes = plan.targetFrameBytes

    // Converted audio the target did not take yet: [position, limit) is pending.
    private val pending: ByteBuffer = ByteBuffer.allocateDirect(maxOf(SCRATCH_BYTES, plan.minOutputBytes * 4)).also { it.limit(0) }
    private var stage: ByteBuffer? = null
    private var draining = false
    private var tailDone = false
    private var targetDrained = false
    private var closed = false
    private var targetFailure: DacException? = null

    override val bitPerfect: StateFlow<BitPerfectState> =
        if (plan.preservesBitPerfect) {
            target.bitPerfect
        } else {
            MutableStateFlow(BitPerfectState.NotVerified(null, plan.description))
        }

    override fun write(buffer: ByteBuffer): Int = synchronized(lock) {
        checkOpen()
        if (passthrough) return target.write(buffer)
        if (draining) throw DacException(DacErrorCode.INVALID_STATE, "the output is drained; flush before writing")
        pump()
        if (pending.hasRemaining()) return 0
        var total = 0
        val chunkBytes = maxOf(sourceFrameBytes, STAGE_BYTES / sourceFrameBytes * sourceFrameBytes)
        while (buffer.hasRemaining()) {
            pending.clear()
            val input = buffer.duplicate().apply { limit(position() + minOf(remaining(), chunkBytes)) }
            val consumed = try {
                val source = if (buffer.isDirect) input else stageFrom(input)
                converter.convert(source, pending).consumed
            } catch (error: Throwable) {
                pending.clear()
                pending.limit(0)
                if (total > 0 && error is DacException && error.code == DacErrorCode.INVALID_ARGUMENT) return total
                throw error
            }
            buffer.position(buffer.position() + consumed)
            pending.flip()
            total += consumed
            if (consumed == 0 && !pending.hasRemaining()) break
            try {
                pump()
            } catch (error: DacException) {
                if (total > 0) return total
                throw error
            }
            if (pending.hasRemaining()) break
        }
        total
    }

    /** Copies the next chunk of a heap buffer into a direct staging buffer, ready to be read. */
    private fun stageFrom(buffer: ByteBuffer): ByteBuffer {
        val staged = stage ?: ByteBuffer.allocateDirect(maxOf(STAGE_BYTES, sourceFrameBytes)).also { stage = it }
        staged.clear()
        val count = minOf(buffer.remaining(), staged.capacity())
        staged.put(buffer.duplicate().apply { limit(position() + count) })
        staged.flip()
        return staged
    }

    override val writableBytes: Int
        get() = synchronized(lock) {
            if (closed) return 0
            checkTarget()
            if (passthrough) return target.writableBytes
            if (draining) return 0
            pump()
            val room = target.writableBytes - pending.remaining()
            if (room <= 0) return 0
            val sourceFrames = room / targetFrameBytes * sourceRate / targetRate
            (sourceFrames * sourceFrameBytes).coerceAtMost(Int.MAX_VALUE.toLong()).toInt()
        }

    override fun play() {
        synchronized(lock) {
            checkOpen()
            target.play()
        }
    }

    override fun pause() {
        synchronized(lock) {
            checkOpen()
            target.pause()
        }
    }

    override fun flush() {
        synchronized(lock) {
            checkOpen()
            if (!passthrough) {
                converter.reset()
                pending.clear()
                pending.limit(0)
                draining = false
                tailDone = false
                targetDrained = false
            }
            target.flush()
        }
    }

    override fun drain() {
        synchronized(lock) {
            checkOpen()
            if (passthrough) {
                target.drain()
            } else {
                draining = true
                pump()
            }
        }
    }

    override val isDrained: Boolean
        get() = synchronized(lock) {
            if (closed) return false
            checkTarget()
            if (passthrough) return target.isDrained
            pump()
            draining && tailDone && targetDrained && !pending.hasRemaining() && target.isDrained
        }

    /** Source frames played since the start or the last [flush]: the target's count scaled by the rate ratio. */
    override val playedFrames: Long
        get() = synchronized(lock) { if (closed) 0 else target.playedFrames * sourceRate / targetRate }

    /**
     * Source frames between the host and the speaker: the target's latency scaled by the rate ratio,
     * the converted audio the target has not taken yet, and what the converter holds back
     * ([ConversionPlan.delayFrames]).
     */
    override val latencyFrames: Long
        get() = synchronized(lock) {
            if (closed) return 0
            if (passthrough) return target.latencyFrames
            val pendingFrames = pending.remaining() / targetFrameBytes
            (target.latencyFrames + pendingFrames) * sourceRate / targetRate + plan.delayFrames
        }

    /** Closes the target and the converter. Safe to call again. */
    override fun close() {
        synchronized(lock) {
            if (closed) return
            closed = true
        }
        try {
            target.close()
        } finally {
            converter.close()
        }
    }

    // ---- helpers (call with `lock` held) ----

    private fun checkOpen() {
        if (closed) throw DacException(DacErrorCode.INVALID_STATE, "the output is closed")
        checkTarget()
    }

    private fun checkTarget() { targetFailure?.let { throw it } }

    /** Moves converted audio to the target, then the resampler's tail after a [drain], then drains the target. */
    private fun pump() {
        while (true) {
            if (pending.hasRemaining()) {
                try {
                    target.write(pending)
                } catch (error: DacException) {
                    targetFailure = error
                    throw error
                }
                if (pending.hasRemaining()) return
            }
            if (!draining) return
            if (!tailDone) {
                pending.clear()
                val produced = converter.finish(pending)
                if (produced == 0) {
                    tailDone = true
                    pending.limit(0)
                } else {
                    pending.flip()
                }
                continue
            }
            if (!targetDrained) {
                target.drain()
                targetDrained = true
            }
            return
        }
    }

    private companion object {
        /** Converted audio held between the converter and the target: about 90 ms of 24-bit stereo at 192 kHz. */
        const val SCRATCH_BYTES = 64 * 1024

        /** Staging buffer for heap buffers. */
        const val STAGE_BYTES = 64 * 1024
    }
}
