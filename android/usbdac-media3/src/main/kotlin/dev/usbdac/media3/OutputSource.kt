package dev.usbdac.media3

import dev.usbdac.*

// region output-source
/** An output the sink writes to: what the library returned, and what the sink needs to know about it. */
class OpenedOutput(
    val output: PcmOutput,
    /** The layout to write, and the unit of the position counters. */
    val format: WriteFormat,
    /** What the output was opened for. */
    val request: StreamRequest,
    /** The library's own handle, for the status display; `null` for a fake in a test. */
    val selected: SelectedOutput? = null,
) {
    /** Set when the DAC is lost or the stream failed. A failed output is closed and never reused. */
    @Volatile
    var failure: DacException? = null
}

/** A drained track's replacement, including a usable restored old format on rollback. */
class OutputReconfiguration(val opened: OpenedOutput, val rollbackCause: DacException? = null)

/** Result from a cached, side-effect-free output capability snapshot. */
enum class OutputFormatSupport { UNSUPPORTED, DIRECT, WITH_TRANSCODING }

/** The accepted producer format and, when known, the resulting target format. */
data class OutputSupport(
    val support: OutputFormatSupport,
    val source: StreamRequest,
    val target: StreamRequest? = null,
    val reason: String = "",
)

/**
 * What the audio sink needs from the app. The sink is written against this and [PcmOutput], so a test
 * can give it a fake output and no DAC.
 */
interface OutputSource {
    /**
     * Whether the sink should accept [request], answered from cached capabilities only. Do not open,
     * negotiate, or probe here. [UsbDacAudioSink.getFormatSupport] asks it for every raw PCM format.
     *
     * The default answers [OutputFormatSupport.UNSUPPORTED] for every request. A source written for
     * 0.1.10 that implements only [open], [close], [takeLookAgain] and [setVolumeMultiplier] still
     * compiles, but the sink then rejects every format and nothing plays. Override this method.
     */
    fun formatSupport(request: StreamRequest): OutputSupport =
        OutputSupport(OutputFormatSupport.UNSUPPORTED, request, reason = "no capability snapshot")

    /** Opens the best output for [request]. Blocks for 100 ms or more: the sink calls it off the main thread. */
    fun open(request: StreamRequest): OpenedOutput

    /** Optional transactional format change after drain; null retains the close/open fallback. */
    fun reconfigure(output: OpenedOutput, request: StreamRequest): OutputReconfiguration? = null

    /** Closes [output] and everything opened for it. Blocks. */
    fun close(output: OpenedOutput)

    /** True once if the settings or the devices changed since the last time the output was opened. */
    fun takeLookAgain(): Boolean

    /** Last sink position observed on the playback thread, or null after a flush/reconfiguration.
     * This is an advisory recovery point, not proof of samples audible at the instant of loss. */
    fun onRecoveryPosition(output: OpenedOutput?, positionUs: Long?) = Unit

    /** Android's ducking and the app's own player volume, as a multiplier from 0 to 1. */
    fun setVolumeMultiplier(multiplier: Float)
}
// endregion
