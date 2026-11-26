package dev.usbdac

import dev.usbdac.internal.VolumeBackend
import dev.usbdac.internal.VolumeOp
import dev.usbdac.internal.VolumeWriteQueue
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import java.util.concurrent.Executor

/**
 * The DAC's volume as read back from the device.
 *
 * @property db The level in dB.
 * @property fraction Position on the 0..1 curve. 0 is the DAC's minimum; anything above 0 spans
 *   `max(minimum, -80 dB)` to the maximum, linearly in dB, so a DAC whose minimum is -127 dB does
 *   not spend most of a slider on inaudible levels. A level below -80 dB reads as 0.0 but keeps its exact [db].
 * @property muted `null` when the DAC has no writable mute, or its read-back failed.
 */
public data class VolumeState(val db: Double, val fraction: Double, val muted: Boolean?)

/**
 * What [UsbDac.open] did with the DAC's level before any stream could start
 * (see [DacConfig.startupVolume]). The lists hold one entry per controlled channel, in channel
 * order (a single entry for a DAC with one master control).
 *
 * @property originalDb The level each channel had before the library first touched it, in dB:
 *   what [DacVolume.handBack] restores.
 * @property appliedDb The level each channel has now that the start-up step is done.
 * @property limitDb The quietest original channel. An app that restores a level it remembered for
 *   this DAC should not go above it without the user's action: it is how loud the DAC itself was
 *   when it connected.
 * @property attenuated [appliedDb] is below [originalDb] on some channel.
 * @property clampedToMinimum The ceiling was below the DAC's minimum, so the channels went to the minimum.
 * @property maxDb The ceiling that was asked for; `null` for [StartupVolume.Keep].
 */
public data class StartupVolumeReport(
    val originalDb: List<Double>,
    val appliedDb: List<Double>,
    val limitDb: Double,
    val attenuated: Boolean,
    val clampedToMinimum: Boolean,
    val maxDb: Double?,
)

/**
 * The DAC's hardware volume (the Feature Unit's volume and mute control).
 *
 * This is the volume to use with direct USB output: it changes the level in the DAC and leaves
 * the samples untouched, so the stream stays bit-perfect. Android's system volume does not
 * reach a direct USB stream.
 *
 * It works with or without a stream, for as long as the [DacSession] is open.
 *
 * ### Threading
 * [setDb], [setFraction], [step] and [setMute] return at once from any thread, including the main
 * thread. Each is a USB control transfer (milliseconds, up to a 1 s timeout), so the writes are
 * queued and run one at a time on a library thread. A burst of absolute values collapses to its
 * last value, which is always applied, so a slider can call [setFraction] on every move. A failed
 * write is reported as [DacEvent.VolumeWriteFailed] on [DacSession.events]. After the session is
 * closed these calls do nothing. [refresh] and [handBack] block; call them off the main thread.
 */
public class DacVolume internal constructor(
    private val backend: VolumeBackend,
    /** The range of the control, in dB. */
    public val range: VolumeRange,
    /** The DAC has a writable mute. */
    public val hasMute: Boolean,
    initial: VolumeState,
    /** What the library did with the DAC's level when it opened, before any stream could start. */
    public val startup: StartupVolumeReport,
    executor: Executor,
    private val onWriteFailed: (DacException) -> Unit,
) {
    private val deviceLock = Any()
    private val mutableState = MutableStateFlow(initial)

    @Volatile
    private var closed = false

    private val queue = VolumeWriteQueue(executor) { op ->
        if (!closed) {
            try {
                val result = synchronized(deviceLock) { apply(op) }
                mutableState.value = result
            } catch (error: DacException) {
                onWriteFailed(error)
            } catch (error: RuntimeException) {
                onWriteFailed(DacException(DacErrorCode.UNKNOWN, error.message ?: error.toString(), error))
            }
        }
    }

    /**
     * The last level read back from the DAC. It follows this library's writes and [refresh]; a
     * physical knob on the DAC is only noticed by [refresh].
     */
    public val state: StateFlow<VolumeState> get() = mutableState

    /** The level the DAC had before the library first changed it, if every channel agreed. */
    public val originalDb: Double? get() = backend.originalDb()

    /** Sets the level in dB. Clamped to [range] and snapped to the nearest step the DAC supports. */
    public fun setDb(db: Double) {
        queue.submit(VolumeOp.SetDb(db))
    }

    /** Sets the level on the 0..1 curve described at [VolumeState.fraction]. */
    public fun setFraction(fraction: Double) {
        queue.submit(VolumeOp.SetFraction(fraction.coerceIn(0.0, 1.0)))
    }

    /**
     * Moves the level by 1/[steps] of the 0..1 curve (a [steps] of 0 or less means [DEFAULT_STEPS]),
     * and by at least one DAC step, up (`direction` > 0) or down (`direction` < 0). At either end the level stays. Repeated
     * identical steps are merged, and at most a bounded number wait behind a slow write.
     */
    public fun step(direction: Int, steps: Int = DEFAULT_STEPS) {
        if (direction == 0) return
        val size = if (steps <= 0) DEFAULT_STEPS else steps.coerceAtMost(MAX_STEPS)
        queue.submit(VolumeOp.Step(if (direction > 0) 1 else -1, size, 1))
    }

    /** Mutes or unmutes. Does nothing useful when [hasMute] is false: the write fails and is reported. */
    public fun setMute(muted: Boolean) {
        queue.submit(VolumeOp.SetMute(muted))
    }

    /** Reads the level from the DAC (blocking) and updates [state]. Use it to notice a physical knob. */
    public fun refresh(): VolumeState = synchronized(deviceLock) {
        checkOpen()
        backend.get().also { mutableState.value = it }
    }

    /**
     * Writes the original level back (blocking) and forgets it. Pending writes are dropped first.
     * Returns whether a level was written. [DacSession.close] does this by default.
     */
    public fun handBack(): Boolean {
        queue.clear()
        return synchronized(deviceLock) {
            checkOpen()
            backend.handBack()
        }
    }

    internal fun close() {
        closed = true
        queue.clear()
    }

    private fun checkOpen() {
        if (closed) throw DacException(DacErrorCode.INVALID_STATE, "the session is closed")
    }

    private fun apply(op: VolumeOp): VolumeState = when (op) {
        is VolumeOp.SetDb -> backend.setDb(op.db)
        is VolumeOp.SetFraction -> backend.setFraction(op.fraction)
        is VolumeOp.SetMute -> backend.setMute(op.muted)
        is VolumeOp.Step -> {
            var result = backend.step(op.direction, op.steps)
            repeat(op.count - 1) { result = backend.step(op.direction, op.steps) }
            result
        }
    }

    public companion object {
        /** Default size of a [step]: 1/16 of the curve. */
        public const val DEFAULT_STEPS: Int = 16

        private const val MAX_STEPS = 255
    }
}
