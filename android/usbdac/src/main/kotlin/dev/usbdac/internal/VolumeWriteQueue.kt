package dev.usbdac.internal

import java.util.concurrent.Executor

/** One pending change of the DAC's volume controls. */
internal sealed interface VolumeOp {
    data class SetDb(val db: Double) : VolumeOp
    data class SetFraction(val fraction: Double) : VolumeOp

    /** [count] presses of [direction] by 1/[steps] of the curve, merged. */
    data class Step(val direction: Int, val steps: Int, val count: Int) : VolumeOp
    data class SetMute(val muted: Boolean) : VolumeOp
}

/**
 * Serialises writes to the DAC without ever making the caller wait.
 *
 * At most one write is in flight, on [executor]. What arrives meanwhile is merged so the
 * queue stays short and the final value is always applied:
 *
 * - an absolute level (dB or fraction) replaces everything pending that would set the level,
 *   because only its result is observable;
 * - a step merges into a pending identical step (up to [maxPendingSteps] presses), and
 *   otherwise waits behind what is pending, so a held key is not dropped;
 * - a mute request replaces a pending one; mute and level are independent controls.
 */
internal class VolumeWriteQueue(
    private val executor: Executor,
    private val maxPendingSteps: Int = 32,
    private val write: (VolumeOp) -> Unit,
) {
    private val lock = Any()
    private val pending = ArrayDeque<VolumeOp>()
    private var draining = false

    /** Queues [op]; returns at once. */
    fun submit(op: VolumeOp) {
        val start = synchronized(lock) {
            merge(op)
            if (draining) false else { draining = true; true }
        }
        if (start) executor.execute(::drain)
    }

    /** Drops what has not started yet. A write already in flight completes. */
    fun clear() {
        synchronized(lock) { pending.clear() }
    }

    private fun merge(op: VolumeOp) {
        when (op) {
            is VolumeOp.SetDb, is VolumeOp.SetFraction ->
                pending.removeAll { it !is VolumeOp.SetMute }
            is VolumeOp.SetMute ->
                pending.removeAll { it is VolumeOp.SetMute }
            is VolumeOp.Step -> {
                val last = pending.lastOrNull { it !is VolumeOp.SetMute }
                if (last is VolumeOp.Step && last.direction == op.direction && last.steps == op.steps) {
                    val index = pending.lastIndexOf(last)
                    pending[index] = last.copy(count = minOf(last.count + op.count, maxPendingSteps))
                    return
                }
            }
        }
        pending.addLast(op)
    }

    private fun drain() {
        while (true) {
            val next = synchronized(lock) {
                val head = pending.removeFirstOrNull()
                if (head == null) draining = false
                head
            } ?: return
            write(next)
        }
    }
}
