package dev.usbdac.sample.player

import dev.usbdac.media3.*

import dev.usbdac.DacConfig
import dev.usbdac.OutputPolicy

// The code of docs/sdk/guide-kotlin.md, which plays through a Media3 player. This part is the settings.

// region settings
/**
 * The choices that belong to the listener, as the settings screen stores them. Persist them wherever
 * the app keeps its settings (DataStore in many players); the library stores nothing.
 */
data class DirectOutputSettings(
    /** "Use the DAC directly." Off keeps the DAC available to other apps. */
    val takeOverDac: Boolean = true,
    /** "If the DAC cannot play a file as it is, convert it (not bit-perfect)." */
    val allowConversion: Boolean = false,
    /** "Play on a DAC whose volume cannot be lowered, at its own level." */
    val allowFixedVolume: Boolean = false,
    /** A support setting, off by default: keep a USB feedback trace of the next attempt. */
    val captureSupportTrace: Boolean = false,
) {
    fun toPolicy(): OutputPolicy =
        OutputPolicy(
            allowDirectUsb = takeOverDac,
            allowConversion = allowConversion,
            dacConfig = DacConfig(
                requireHardwareVolume = !allowFixedVolume,
                captureFeedbackTrace = captureSupportTrace,
            ),
        )
}
// endregion
