package dev.usbdac

import dev.usbdac.internal.parseVolumeState

/** The compatibility admission a successfully started stream used, separate from its verdict. */
public enum class CompatibilityFallbackTier(public val wireName: String) {
    /** Host-paced asynchronous output without usable feedback; never bit-perfect while used. */
    NOMINAL_ASYNC("nominal_async"),

    /** A successful clock SET was accepted with unusable readback; the clock remains unverified. */
    UNVERIFIED_CLOCK("unverified_clock");

    internal companion object {
        fun fromCode(code: Long): CompatibilityFallbackTier? = when (code) {
            0L -> NOMINAL_ASYNC
            1L -> UNVERIFIED_CLOCK
            else -> null
        }
    }
}

/**
 * Something that happened to a session or its stream. Events are delivered from a library
 * thread, never from the audio path, in order; see [DacSession.events].
 */
public sealed interface DacEvent {
    /** The stream state changed. */
    public data class StateChanged(val state: StreamState) : DacEvent

    /** The ring ran dry and the wire was padded with silence. Once per run of underruns. */
    public data object Underrun : DacEvent

    /** The DAC did not take queued audio (a timed-out or short transfer). It cannot be re-sent. */
    public data class LostFrames(val frames: Long) : DacEvent

    /** The library's bit-perfect verdict changed. */
    public data class BitPerfectChanged(val bitPerfect: Boolean) : DacEvent

    /** The hardware volume or mute changed through this library. A physical knob does not send this. */
    public data class VolumeChanged(val state: VolumeState) : DacEvent

    /** The explicit feedback endpoint stopped producing valid reports. */
    public data object FeedbackStale : DacEvent

    /** The producer ring was empty for [emptyMs] on nominal asynchronous output.
     * Emitted once per episode; refill, pause, or drain resets it. This does not measure DAC FIFO drift.
     */
    public data class NominalAsyncHostStarvation(val emptyMs: Long) : DacEvent

    /** The stream failed. [StateChanged] with [StreamState.FAILED] follows; close the stream. */
    public data class StreamFailed(val error: DacException) : DacEvent

    /** The device is gone. The session stays open until [DacSession.close]. */
    public data object DeviceLost : DacEvent

    /**
     * Successful stream startup used an explicitly allowed compatibility tier. Emitted after the
     * first healthy OUT submission, never for a strict refusal or failed worker startup.
     * [reason] explains admission; it does not verify the stream. Existing
     * [BitPerfectBlocker.UNSYNCHRONIZED_ASYNC] and [BitPerfectBlocker.CLOCK_UNVERIFIED] verdicts remain.
     * Device and stream listeners may both receive the native event; [DacSession.events] publishes
     * it once through the device listener.
     */
    public data class CompatibilityFallback(
        val tier: CompatibilityFallbackTier,
        val reason: String,
    ) : DacEvent

    /** A volume write requested through [DacVolume] failed; the call that asked had already returned. */
    public data class VolumeWriteFailed(val error: DacException) : DacEvent

    /** An event this version of the library does not know. */
    public data class Unknown(val description: String) : DacEvent
}

/** Event type codes of `onEvent(type, a, b, message)`; see `docs/API.md` section 10. */
internal object EventType {
    const val STATE_CHANGED = 0
    const val UNDERRUN = 1
    const val LOST_FRAMES = 2
    const val BIT_PERFECT_CHANGED = 3
    const val VOLUME_CHANGED = 4
    const val FEEDBACK_STALE = 5
    const val ERROR = 6
    const val DEVICE_LOST = 7
    const val COMPATIBILITY_FALLBACK = 8
    const val NOMINAL_ASYNC_HOST_STARVATION = 9
}

/** Converts one native `onEvent` call into a [DacEvent]. */
internal fun decodeEvent(type: Int, a: Long, b: Double, message: String?): DacEvent =
    when (type) {
        EventType.STATE_CHANGED ->
            StreamState.fromCode(a.toInt())?.let { DacEvent.StateChanged(it) }
                ?: DacEvent.Unknown("state code $a")
        EventType.UNDERRUN -> DacEvent.Underrun
        EventType.LOST_FRAMES -> DacEvent.LostFrames(a)
        EventType.BIT_PERFECT_CHANGED -> DacEvent.BitPerfectChanged(a != 0L)
        EventType.VOLUME_CHANGED -> DacEvent.VolumeChanged(
            message?.let { runCatching { parseVolumeState(it) }.getOrNull() }
                ?: VolumeState(db = b, fraction = Double.NaN, muted = if (a < 0) null else a != 0L),
        )
        EventType.FEEDBACK_STALE -> DacEvent.FeedbackStale
        EventType.NOMINAL_ASYNC_HOST_STARVATION -> DacEvent.NominalAsyncHostStarvation(a)
        EventType.ERROR -> DacEvent.StreamFailed(
            parseNativeError(message) ?: DacException(DacErrorCode.fromNativeNumber(a), message ?: "error code $a"),
        )
        EventType.DEVICE_LOST -> DacEvent.DeviceLost
        EventType.COMPATIBILITY_FALLBACK -> CompatibilityFallbackTier.fromCode(a)?.let { tier ->
            DacEvent.CompatibilityFallback(tier, message ?: "compatibility fallback ${tier.wireName}")
        } ?: DacEvent.Unknown("compatibility fallback tier $a: ${message.orEmpty()}")
        else -> DacEvent.Unknown(message ?: "event type $type")
    }
