package dev.usbdac

/** What stands in the way of a bit-perfect direct USB stream, most significant first. */
public enum class BitPerfectBlocker(
    internal val wireName: String,
    /** A sentence for a status line. */
    public val description: String,
) {
    NO_STREAM("no_stream", "The stream is closed or not running."),
    LOST_FRAMES(
        "lost_frames",
        "Audio queued to the DAC was lost on the bus. The flag stays off for the rest of the stream.",
    ),
    FORMAT_CHANGED("format_changed", "The DAC runs another rate or format than the one requested."),
    CLOCK_UNVERIFIED("clock_unverified", "The DAC did not confirm its sampling frequency."),
    SOFTWARE_GAIN("software_gain", "Software gain other than 1.0 is applied."),
    UNSYNCHRONIZED_ASYNC(
        "unsynchronized_async",
        "The DAC's asynchronous endpoint has no feedback endpoint; the stream is paced at the nominal rate.",
    ),
    PAYLOAD_UNVERIFIED("payload_unverified", "The wire payload audit has not seen real audio yet, or lost it."),

    /** A blocker this version of the library does not know. */
    UNKNOWN("unknown", "The stream is not verified bit-perfect.");

    internal companion object {
        fun fromWireName(name: String?): BitPerfectBlocker? =
            if (name == null) null else entries.firstOrNull { it.wireName == name } ?: UNKNOWN
    }
}

/**
 * Whether the output is bit-perfect, and how well anyone can tell.
 *
 * Only [Verified] is a claim the library makes itself, from its own checks of the USB
 * stream. The other states say what is known, and are not an assertion either way.
 */
public sealed interface BitPerfectState {
    /** `true` only for [Verified]. */
    public val isVerified: Boolean

    /**
     * Direct USB: the stream is running in the requested format, the DAC confirmed its clock,
     * no frame was lost, software gain is off and the payload audit passes.
     * This is a statement about the transport; it does not cover samples the host changed
     * before writing them.
     */
    public data object Verified : BitPerfectState {
        override val isVerified: Boolean get() = true
    }

    /** Direct USB: at least one condition is not met. [blocker] is the main one. */
    public data class NotVerified(val blocker: BitPerfectBlocker?, val reason: String) : BitPerfectState {
        override val isVerified: Boolean get() = false
    }

    /**
     * Managed output on Android 14 or later: the platform accepted a request for the bit-perfect
     * mixer behavior for this USB device and format. Android, not this library, enforces it,
     * and nothing here can confirm it.
     */
    public data class RequestedViaPlatform(val detail: String) : BitPerfectState {
        override val isVerified: Boolean get() = false
    }

    /**
     * Managed output without a platform guarantee. Whether the samples reach the DAC unchanged
     * depends on the device, the Android version and the route, and cannot be checked from the app.
     */
    public data class Unverifiable(val reason: String) : BitPerfectState {
        override val isVerified: Boolean get() = false
    }
}

/** Builds the state a direct USB stream reports from the library's own flag and blocker. */
internal fun directBitPerfectState(bitPerfect: Boolean, blocker: BitPerfectBlocker?): BitPerfectState =
    when {
        bitPerfect -> BitPerfectState.Verified
        else -> {
            val main = blocker ?: BitPerfectBlocker.UNKNOWN
            BitPerfectState.NotVerified(main, main.description)
        }
    }
