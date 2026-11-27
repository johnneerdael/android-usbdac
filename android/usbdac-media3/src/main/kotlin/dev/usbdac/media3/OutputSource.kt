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

/**
 * What the audio sink needs from the app. The sink is written against this and [PcmOutput], so a test
 * can give it a fake output and no DAC.
 */
interface OutputSource {
    /** Opens the best output for [request]. Blocks for 100 ms or more: the sink calls it off the main thread. */
    fun open(request: StreamRequest): OpenedOutput

    /** Closes [output] and everything opened for it. Blocks. */
    fun close(output: OpenedOutput)

    /** True once if the settings or the devices changed since the last time the output was opened. */
    fun takeLookAgain(): Boolean

    /** Android's ducking and the app's own player volume, as a multiplier from 0 to 1. */
    fun setVolumeMultiplier(multiplier: Float)
}
// endregion
