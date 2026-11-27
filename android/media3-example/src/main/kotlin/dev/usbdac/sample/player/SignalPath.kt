package dev.usbdac.sample.player

import dev.usbdac.media3.*

import dev.usbdac.BitPerfectState
import dev.usbdac.OutputKind
import dev.usbdac.SelectedOutput
import dev.usbdac.UsbDac
import androidx.media3.common.MimeTypes

// region library-version
/** The native library's version, which also proves that libusbdac.so loaded for this device's ABI. */
fun libraryVersion(): String = UsbDac.nativeVersion

/** Debug builds and the support screen: the library's log goes to logcat under the tag `usbdac`. */
fun setSupportLogging(enabled: Boolean) = UsbDac.setLogLevel(if (enabled) 4 else 0)
// endregion

// region indicator
/** Where the audio came from, which only the player knows. */
enum class SourceKind { LOSSLESS, LOSSY }

/** From the compressed format the player reads (`ExoPlayer.audioFormat.sampleMimeType`). */
fun sourceKindOf(sampleMimeType: String?): SourceKind = when (sampleMimeType) {
    MimeTypes.AUDIO_FLAC, MimeTypes.AUDIO_ALAC, MimeTypes.AUDIO_RAW, MimeTypes.AUDIO_WAV -> SourceKind.LOSSLESS
    else -> SourceKind.LOSSY
}

/**
 * One line for the now-playing screen. The library's flag covers the transport only, so the line also
 * says what the player did to the samples and what the source was.
 */
fun indicatorText(state: BitPerfectState, engineUntouched: Boolean, source: SourceKind): String {
    val decodedFromLossy = if (source == SourceKind.LOSSY) ", decoded from a lossy file" else ""
    return when {
        !engineUntouched -> "Not bit-perfect: the player is processing the audio"
        state is BitPerfectState.Verified -> "Bit-perfect$decodedFromLossy"
        state is BitPerfectState.NotVerified -> "Not bit-perfect: ${state.reason}"
        state is BitPerfectState.RequestedViaPlatform -> "Bit-perfect mode requested from Android, not verifiable"
        state is BitPerfectState.Unverifiable -> state.reason
        else -> ""
    }
}

/** "Direct USB, 96000 Hz, 24-bit, stereo" for the signal path view. */
fun outputLine(output: SelectedOutput): String {
    val kind = when (output.kind) {
        OutputKind.DIRECT_USB -> "Direct USB"
        OutputKind.ANDROID_BIT_PERFECT -> "Android bit-perfect mixer"
        OutputKind.AAUDIO_EXCLUSIVE -> "Exclusive AAudio"
        OutputKind.ANDROID_STANDARD -> "Android output"
    }
    val format = output.format
    return "$kind, ${format.sampleRate} Hz, ${format.validBits}-bit, ${if (format.channels == 2) "stereo" else "${format.channels} ch"}"
}
// endregion

// region support-report
/**
 * What a bug report needs from the output in use, as text the listener can copy. The library writes no
 * file and sends nothing: where it goes is the app's decision.
 */
fun supportReport(output: SelectedOutput, appVersion: String): String = buildString {
    appendLine("app $appVersion, usbdac ${UsbDac.nativeVersion}, ${android.os.Build.MODEL}, Android ${android.os.Build.VERSION.SDK_INT}")
    appendLine(describeOutput(output))
    output.decision.dac?.let { appendLine("DAC: ${it.productName}, USB ${"%04x:%04x".format(it.vendorId, it.productId)}") }
    val stream = output.stream
    if (stream != null) {
        appendLine("clock and pacing: ${stream.format.pacing.description}; ${stream.format.clock?.description ?: "no clock entities"}")
        appendLine(stream.stats().toString())
    }
    appendLine("bit-perfect: ${output.bitPerfect.value}")
    output.session?.capabilities?.let { appendLine("DAC: $it") }
}

private fun describeOutput(output: SelectedOutput): String = buildString {
    appendLine(outputLine(output))
    append("${output.decision.guarantee}: ${output.decision.reason}")
    output.decision.alternativesRejected.forEach { append("\nnot ${it.kind}: ${it.why}") }
}
// endregion
