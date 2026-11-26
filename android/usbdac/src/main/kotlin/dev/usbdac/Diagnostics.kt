package dev.usbdac

/**
 * What keeps the host's packets and the DAC's clock in step on an endpoint. This only names
 * what the library already does for the endpoint; it never selects a mode.
 *
 * Read it from [StreamFormat.pacing] (what a stream uses) or [AltSetting.pacing] (what a stream
 * on that alternate setting would use).
 */
public sealed interface Pacing {
    /** A sentence for a status line. */
    public val description: String

    /**
     * An asynchronous endpoint whose explicit feedback endpoint is polled: the packet sizes follow
     * the rate the DAC reports.
     *
     * @property endpoint The feedback endpoint's address (for example 0x81).
     */
    public data class ExplicitFeedback(val endpoint: Int) : Pacing {
        override val description: String get() = "explicit feedback (endpoint 0x%02x)".format(endpoint)
    }

    /** An adaptive endpoint: the DAC follows the host's packet timing. */
    public data object Adaptive : Pacing {
        override val description: String get() = "adaptive (the DAC follows the host's packets)"
    }

    /** A synchronous endpoint: the DAC and the host share the bus clock. */
    public data object Synchronous : Pacing {
        override val description: String get() = "synchronous (the DAC and the host share the bus clock)"
    }

    /**
     * The host sends at the nominal rate and nothing comes back from the DAC: an asynchronous
     * endpoint without a feedback endpoint, streamed only because
     * [DacConfig.allowUnsynchronizedAsync] is on (never bit-perfect), or an endpoint that
     * declares no synchronisation.
     */
    public data object Unsynchronized : Pacing {
        override val description: String get() = "unsynchronized (the host sends at the nominal rate)"
    }

    /** A pacing this version of the SDK does not know, or one an older native library did not report. */
    public data object Unknown : Pacing {
        override val description: String get() = "unknown"
    }
}

/** How [ClockChoice.sourceId] was found. */
public enum class ClockResolution {
    /**
     * The stream's terminal was traced to the Clock Source through the descriptors (and, at a Clock
     * Selector with several inputs, the pin the device reports as selected).
     */
    RESOLVED,

    /**
     * The topology could not be traced (no terminal link, a malformed or looping entity, a selector
     * that would not say which pin is active), so the first Clock Source in descriptor order was
     * used. On a DAC with more than one clock the rate may then be set on the wrong one.
     */
    FALLBACK_FIRST_SOURCE,

    /** A resolution this version of the SDK does not know. */
    UNKNOWN,
}

/** What a [ClockHop] is. */
public sealed interface ClockHopKind {
    /** The streaming terminal the path starts at. */
    public data object Terminal : ClockHopKind

    /**
     * A Clock Selector. Pins are 1-based. A selector with one input has nothing to choose and is
     * reported as pin 1 of 1; with several inputs, [selectedPin] is what the device reported.
     */
    public data class Selector(val selectedPin: Int, val pinCount: Int) : ClockHopKind

    /** A Clock Multiplier (one clock input). */
    public data object Multiplier : ClockHopKind

    /** A Clock Source: the end of the path. */
    public data object Source : ClockHopKind

    /** A kind this version of the SDK does not know. */
    public data object Unknown : ClockHopKind
}

/** One entity on the path from a streaming terminal to its Clock Source. [id] is its entity ID. */
public data class ClockHop(val id: Int, val kind: ClockHopKind) {
    /** For example `selector 9 (pin 2 of 2)`. */
    public val description: String
        get() = when (kind) {
            ClockHopKind.Terminal -> "terminal $id"
            is ClockHopKind.Selector -> "selector $id (pin ${kind.selectedPin} of ${kind.pinCount})"
            ClockHopKind.Multiplier -> "multiplier $id"
            ClockHopKind.Source -> "source $id"
            ClockHopKind.Unknown -> "entity $id"
        }
}

/**
 * The Clock Source the sample rate was set and verified on, and how it was reached from the
 * stream's terminal, as the library resolved it when it negotiated. Read it from
 * [StreamFormat.clock].
 *
 * A DAC with more than one clock (a Clock Selector between an internal and an external source,
 * say) plays at the wrong pitch, or not at all at one rate, when the rate goes to the wrong
 * source. Look at [path] and [resolution] then.
 *
 * @property controlInterface Interface number of the AudioControl interface the clock requests go to.
 * @property sourceId Entity ID of the Clock Source the rate is set and verified on.
 * @property path The entities followed, in order: the streaming terminal, then each clock entity
 *   down to the Clock Source (the last hop, whose id is [sourceId]). For
 *   [ClockResolution.FALLBACK_FIRST_SOURCE] nothing was followed, so it is the Clock Source alone.
 */
public data class ClockChoice(
    val controlInterface: Int,
    val sourceId: Int,
    val path: List<ClockHop>,
    val resolution: ClockResolution,
) {
    /** For example `terminal 1 > selector 9 (pin 2 of 2) > source 5, resolved`. */
    public val description: String
        get() = path.joinToString(" > ") { it.description } + when (resolution) {
            ClockResolution.RESOLVED -> ", resolved"
            ClockResolution.FALLBACK_FIRST_SOURCE -> ", fallback to the first source"
            ClockResolution.UNKNOWN -> ""
        }
}
