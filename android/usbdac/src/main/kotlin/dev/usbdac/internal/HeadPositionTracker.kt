package dev.usbdac.internal

/**
 * Turns `AudioTrack.getPlaybackHeadPosition()` into a monotonic frame count.
 *
 * The platform reports the position as an `int` to be read as an unsigned 32-bit counter that
 * wraps (after about 6 hours at 192 kHz), and resets it to zero on `flush()`. This class adds
 * the wraps and measures from a baseline set at [reset].
 *
 * A reading that is *lower* than the previous one by less than half the counter range is taken
 * as the platform's reset to zero (a flush that completed after [reset] ran), not as a wrap.
 * That is a heuristic about device behaviour, and the reason the managed output's position is
 * documented as best-effort.
 */
internal class HeadPositionTracker {
    private var last = 0L
    private var wraps = 0L
    private var baseline = 0L

    /** Frames played since [reset], given the raw head position the platform just reported. */
    @Synchronized
    fun update(raw: Int): Long {
        val value = raw.toLong() and MASK
        if (value < last) {
            if (last - value > HALF_RANGE) {
                wraps++
            } else {
                wraps = 0
                baseline = 0
            }
        }
        last = value
        return ((wraps shl 32) + value - baseline).coerceAtLeast(0)
    }

    /** Starts counting from [raw]: the position the platform reports right now is zero frames played. */
    @Synchronized
    fun reset(raw: Int) {
        last = raw.toLong() and MASK
        wraps = 0
        baseline = last
    }

    private companion object {
        const val MASK = 0xFFFFFFFFL
        const val HALF_RANGE = 1L shl 31
    }
}
