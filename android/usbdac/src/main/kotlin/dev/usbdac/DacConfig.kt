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

/**
 * What [UsbDac.open] does with the DAC's hardware volume before any stream can start.
 *
 * A DAC keeps the level it had, and many default to full scale, so the first audio would play at
 * that level. The default [Attenuate] lowers it. The level the DAC had is recorded first and
 * [HandBack.RESTORE_ORIGINAL] restores it. [DacVolume.startup] says what happened.
 */
public sealed interface StartupVolume {
    /**
     * Lower every controlled channel to `min(its level, maxDb)`, rounded down to a step the DAC
     * supports, and verify it by reading it back. A channel is never raised: one that is already
     * quieter, or at the DAC's silent value, is left alone. If [maxDb] is below the DAC's minimum
     * the channels go to the minimum and [StartupVolumeReport.clampedToMinimum] says so. If the
     * attenuation cannot be verified the hardware volume is treated as unusable: [DacSession.volume]
     * is `null` and, with [DacConfig.requireHardwareVolume], streaming fails with
     * [DacErrorCode.VOLUME_UNAVAILABLE]. [maxDb] must be between -128 and 0.
     */
    public data class Attenuate @JvmOverloads constructor(val maxDb: Double = -40.0) : StartupVolume

    /** Leave the DAC's level alone. The first audio plays at whatever the DAC had. */
    public data object Keep : StartupVolume
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

    /** Legacy native DSD words go on the wire most-significant byte first; standard UAC3 stays LE64. */
    public data object DsdBigEndian : DacQuirk

    /** Reverse the bit order within each legacy native DSD byte; standard UAC3 stays MSB first. */
    public data object DsdBitReverse : DacQuirk

    /** Trace-backed high-speed four-byte feedback reports express frames per OUT service interval. */
    public data object ExplicitFeedbackPerServiceInterval : DacQuirk

    /** Delay each class control message by this many microseconds. */
    public data class ControlMessageDelayUs(val microseconds: Long) : DacQuirk

    /** Delay interface transitions by this many milliseconds. */
    public data class InterfaceDelayMs(val milliseconds: Long) : DacQuirk

    /** Activate the selected alternate before programming its rate. */
    public data object SetInterfaceFirst : DacQuirk

    /** Apply the exact device playback transfer fixup. */
    public data object PlaybackUrbFixup : DacQuirk

    /** Skip endpoint sample-rate readback only for an explicitly selected workaround. */
    public data object SkipSampleRateReadback : DacQuirk

    /** Disable implicit feedback discovery for the selected device policy. */
    public data object SkipImplicitFeedback : DacQuirk

    /** Treat the hardware minimum playback level as mute. */
    public data object MixerPlaybackMinMute : DacQuirk

    /** Use the source-backed linear playback-volume interpretation. */
    public data object MixerPlaybackLinearVolume : DacQuirk

    /** Interpret the proven raw DSD32 layout as big-endian words. */
    public data object RawDsd32Be : DacQuirk

    /** Use the proven fixed-rate endpoint policy. */
    public data object FixedRate : DacQuirk

    /** Allow bounded measured-cadence silence after an empty implicit feedback pattern. */
    public data object ImplicitSilenceOnEmpty : DacQuirk

    /** Keep existing clock-selector choices instead of writing selectors. */
    public data object SkipClockSelectorWrites : DacQuirk

    /** Ignore only controls designated optional by the source-backed policy. */
    public data object IgnoreOptionalControlErrors : DacQuirk

    /** Wait this long after programming the clock. */
    public data class SettleDelayMs(val milliseconds: Long) : DacQuirk

    /** Override legacy native DSD subslots (1, 2 or 4 bytes) whose descriptors are wrong or ambiguous; standard UAC3 stays 8 bytes. */
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

/** Starting points for USB bus queue tuning. The software ring is independent. */
public enum class UsbQueueProfile {
    /** Shorter USB queue and smaller transfers for interactive playback. */
    LOW_LATENCY,
    /** The SDK defaults. */
    BALANCED,
    /** Same target queue as [BALANCED] with fewer, larger transfers and wakeups. */
    LOW_CPU,
}

/** Which device-specific compatibility facts may affect transport behavior. */
public enum class QuirkPolicy(internal val wireName: String) {
    /** Keep the previously shipped device admission policy. */
    VERIFIED_ONLY("verified_only"),
    /** Also admit exact source-backed identity/layout rules; this does not claim physical testing. */
    SOURCE_BACKED("source_backed"),
}

/** Narrow compatibility fallback when an asynchronous endpoint has no usable feedback. */
public enum class AsyncFallbackPolicy(internal val wireName: String) {
    /** Permit nominal pacing only for full-speed UAC1 PCM; this never verifies bit-perfect output. */
    FULL_SPEED_UAC1("full_speed_uac1"),
    /** Require feedback unless the separate broad [DacConfig.allowUnsynchronizedAsync] opt-in is set. */
    FORBID("forbid"),
}

/** How PCM negotiation treats unusable clock readback after a successful SET request. */
public enum class ClockReadbackPolicy(internal val wireName: String) {
    /** Keep strict readback verification. */
    REQUIRE_VERIFIED("require_verified"),
    /** Accept zero/unavailable readback after successful SET; positive mismatches still fail. */
    ALLOW_UNUSABLE_AFTER_SUCCESSFUL_SET("allow_unusable_after_successful_set"),
}

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

    /** Legacy native DSD byte order on the wire. `null`: the device's quirk, else little-endian. Standard UAC3 stays LE64. */
    val dsdBigEndian: Boolean? = null,

    /** Legacy native DSD subslot size (1, 2 or 4). `null`: the device's quirk, else the descriptor. Standard UAC3 stays 8 bytes. */
    val dsdSubslot: Int? = null,

    /** Prefer the 4-byte subslot for 24-bit PCM. `null`: the device's quirk, else off. */
    val padded24Bit: Boolean? = null,

    /** Quirks added to the built-in table for this session. */
    val quirks: List<QuirkOverride> = emptyList(),

    /**
     * Stream an asynchronous endpoint that has no explicit feedback endpoint. Default `false`:
     * the separate [asyncFallbackPolicy] permits only its narrow compatibility case; other
     * feedback-less endpoints fail with [DacErrorCode.IMPLICIT_FEEDBACK_UNSUPPORTED].
     * On, the stream runs host-paced at the nominal rate; drift between the DAC's clock and the host's
     * timing ends in underruns or overruns at the DAC, and the stream is never bit-perfect.
     */
    val allowUnsynchronizedAsync: Boolean = false,
    /** Disable both feedback modes for diagnosis; nominal async still requires opt-in. */
    val disableFeedback: Boolean = false,

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

    /**
     * What [UsbDac.open] does with the DAC's hardware volume before any stream can start. Default
     * [StartupVolume.Attenuate] at -40 dB: a DAC that comes up at full scale does not play its first
     * audio there. See [StartupVolume] and [DacVolume.startup].
     */
    val startupVolume: StartupVolume = StartupVolume.Attenuate(),

    /**
     * [DacSession.openStream] fails with [DacErrorCode.VOLUME_UNAVAILABLE] when the DAC has no usable
     * hardware volume control (none, or its start-up attenuation could not be verified): a stream
     * would play at the DAC's own level with no way to bring it down. The session itself opens, and
     * [DacSession.negotiate] and [DacSession.capabilities] work, so the app can show what the DAC
     * offers, ask the user to allow fixed-level output, and open the session again with this
     * `false`. Default `true`.
     */
    val requireHardwareVolume: Boolean = true,
    /**
     * Capture a bounded report of USB feedback timing and raw explicit reports.
     * Enable before opening a support session and export it with [DacSession.feedbackTraceJson].
     */
    val captureFeedbackTrace: Boolean = false,

    /**
     * Optional target for one USB transfer in microseconds, from 125 to 32,000. The driver
     * rounds down to whole service intervals (at least one) and derives the slot count from
     * [busQueueMs], using [transfersInFlight] as a ceiling. It keeps at least two slots.
     * A null period uses the fixed slot count.
     */
    val transferPeriodUs: Int? = null,

    /** Narrow feedback-less PCM fallback; independent of the broad diagnostic override. */
    val asyncFallbackPolicy: AsyncFallbackPolicy = AsyncFallbackPolicy.FULL_SPEED_UAC1,

    /** An unverified clock remains reported unverified when the compatibility policy allows it. */
    val clockReadbackPolicy: ClockReadbackPolicy = ClockReadbackPolicy.REQUIRE_VERIFIED,

    /** Adapt USB queue depth within the native bounds after measured worker stalls. */
    val adaptiveQueue: Boolean = true,

    /** Device-specific transport rules; source-backed rules require this explicit opt-in. */
    val quirkPolicy: QuirkPolicy = QuirkPolicy.VERIFIED_ONLY,
) {
    /** Apply a USB queue profile and keep this config's other session settings. */
    public fun withUsbQueueProfile(profile: UsbQueueProfile): DacConfig = when (profile) {
        UsbQueueProfile.LOW_LATENCY -> copy(busQueueMs = 8, transfersInFlight = 8, transferPeriodUs = 1_000)
        UsbQueueProfile.BALANCED -> copy(busQueueMs = 16, transfersInFlight = 8, transferPeriodUs = null)
        UsbQueueProfile.LOW_CPU -> copy(busQueueMs = 16, transfersInFlight = 4, transferPeriodUs = null)
    }
}
