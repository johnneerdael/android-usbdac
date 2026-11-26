package dev.usbdac

/** What happens to the DAC's hardware volume when the session closes. */
public enum class HandBack {
    /**
     * Write the level the DAC had before the library first touched its volume back, verify it,
     * and forget it. Use it when the DAC goes back to another audio stack, such as Android's own.
     */
    RESTORE_ORIGINAL,

    /**
     * Leave the DAC at its current level and remember the original for the next [UsbDac.open] of
     * the same device in this process. Use it when the app takes the DAC straight back.
     */
    KEEP,
}

/** A workaround for a DAC that deviates from the USB Audio Class. */
public sealed interface DacQuirk {
    /**
     * The DAC's clock readback is unreliable (it returns 0 or fails). Trust the acknowledgement of the
     * set request for PCM and skip the verification.
     */
    public data object SkipClockValidation : DacQuirk

    /**
     * The DAC reports a rate that differs from the one just set (some report 0 Hz) but plays correctly:
     * do not refuse over the mismatch. PCM only.
     */
    public data object IgnoreInvalidSampleRate : DacQuirk

    /** Refuse to stream unless the rate is verified, even if [DacConfig.requireVerifiedRate] is off. */
    public data object RequireVerifiedRate : DacQuirk

    /** Prefer the 4-byte subslot for 24-bit PCM. */
    public data object PreferPadded24BitTransport : DacQuirk

    /** Native DSD words go on the wire most-significant byte first. */
    public data object DsdBigEndian : DacQuirk

    /** Reverse the bit order within each native DSD byte. */
    public data object DsdBitReverse : DacQuirk

    /** Wait this long after programming the clock. */
    public data class SettleDelayMs(val milliseconds: Long) : DacQuirk

    /** Override the subslot size (1, 2 or 4 bytes) of native DSD alternate settings whose descriptors are wrong or ambiguous. */
    public data class DsdSubslotSize(val bytes: Int) : DacQuirk
}

/**
 * Extra [quirks] for one vendor and product id, added to the library's built-in table.
 * [nameContains] restricts the match to products whose name contains that text.
 */
public data class QuirkOverride(
    val vendorId: Int,
    val productId: Int,
    val nameContains: String? = null,
    val quirks: List<DacQuirk>,
)

/**
 * Every tunable of a session. The defaults are what the transport was tuned with; change a
 * value only for a reason. Values left `null` follow the DAC's quirk, then a fixed default.
 * The native library validates the combination and throws [DacErrorCode.INVALID_ARGUMENT].
 *
 * The guide's `controls.md` describes each option, its default and its effect on bit-perfect output.
 */
public data class DacConfig @JvmOverloads constructor(
    /**
     * Depth the ring is filled to: [PcmOutput.writableBytes] reports room up to this much.
     * A deeper ring rides out longer stalls upstream; [PcmOutput.flush] keeps it from costing
     * responsiveness. Default 250 ms.
     */
    val bufferTargetMs: Int = 250,

    /** Hard capacity of the ring; `write` never accepts more. Default 500 ms. */
    val bufferCapacityMs: Int = 500,

    /**
     * Audio the ring must hold before the stream starts, and before it resumes after
     * [PcmOutput.flush]. Never above [bufferTargetMs]. Default 100 ms.
     */
    val startThresholdMs: Int = 100,

    /** The recommended ring depth while software gain is not 1.0, so changes are heard sooner. Default 100 ms. */
    val processingBufferTargetMs: Int = 100,

    /**
     * Refuse to stream unless the DAC confirms the sampling frequency it was told. Default `true`.
     * Off, a DAC with a broken readback plays, and the stream is never reported bit-perfect.
     */
    val requireVerifiedRate: Boolean = true,

    /** Wait after programming the clock, in milliseconds. `null`: the device's quirk, else 20 ms. */
    val clockSettleDelayMs: Long? = null,

    /** Native DSD byte order on the wire. `null`: the device's quirk, else little-endian. */
    val dsdBigEndian: Boolean? = null,

    /** Subslot size (1, 2 or 4) for native DSD alternate settings. `null`: the device's quirk, else the descriptor. */
    val dsdSubslot: Int? = null,

    /** Prefer the 4-byte subslot for 24-bit PCM. `null`: the device's quirk, else off. */
    val padded24Bit: Boolean? = null,

    /** Quirks added to the built-in table for this session. */
    val quirks: List<QuirkOverride> = emptyList(),

    /**
     * Stream an asynchronous endpoint that has no explicit feedback endpoint. Default `false`:
     * [UsbDac.open] succeeds but [DacSession.negotiate] (or [DacSession.openStream]) fails with
     * [DacErrorCode.IMPLICIT_FEEDBACK_UNSUPPORTED] when that endpoint is the only way to carry the request.
     * On, the stream runs host-paced at the nominal rate; drift between the DAC's clock and the host's
     * timing ends in underruns or overruns at the DAC, and the stream is never bit-perfect.
     */
    val allowUnsynchronizedAsync: Boolean = false,

    /** Ask for real-time scheduling (`SCHED_FIFO`, falling back to nice -16) for the USB output thread. Default `true`. */
    val realtimePriority: Boolean = true,

    /** What [DacSession.close] does with the hardware volume unless told otherwise. Default [HandBack.RESTORE_ORIGINAL]. */
    val handBack: HandBack = HandBack.RESTORE_ORIGINAL,

    /**
     * How much audio, in milliseconds, is kept queued on the USB bus: the transfers in flight
     * together carry about this much, at every service interval and for every encoding. It is
     * what a pause (or a flush with [cancelInFlightOnFlush] off) still plays out, and the headroom
     * against a late output thread. 4 to 64, default 16. A transfer carries
     * `busQueueMs / transfersInFlight` of audio (2 ms by default), at most 32 packets, and the
     * packets in flight never reach 256: when the configured depth would, that cap wins and less
     * is queued.
     */
    val busQueueMs: Int = 16,

    /** How many isochronous transfers are in flight at once. 2 to 16, default 8. See [busQueueMs]. */
    val transfersInFlight: Int = 8,

    /**
     * Cancel the transfers already on the bus when [PcmOutput.flush] is called, so the audio of the
     * old position is not heard after a seek or a track change. The output thread cancels them,
     * waits until every one is handed back, and resumes with wire-correct silence until the ring has
     * refilled. Cancelled transfers are not lost audio, not underruns, do not clear the bit-perfect
     * flag and are not counted in [PcmOutput.playedFrames]. `pause` never cancels. `false` lets the
     * queue ([busQueueMs]) play out, as before. Default `true`.
     */
    val cancelInFlightOnFlush: Boolean = true,
)
