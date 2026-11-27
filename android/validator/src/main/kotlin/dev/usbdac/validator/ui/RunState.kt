package dev.usbdac.validator.ui

import androidx.compose.runtime.Immutable
import dev.usbdac.DacDeviceInfo

/** What the UI reads from Capture on each 500 ms refresh. Capture itself is never written here. */
@Immutable
data class CaptureSnapshot(
    val status: String,
    val progress: Progress?,
    val active: Boolean,
    val waitingPermission: Boolean,
    val shizukuSupported: Boolean,
    val shizukuBusy: Boolean,
    val connected: List<DacDeviceInfo>,
    val canSelect: Boolean,
    val hasArchive: Boolean,
) {
    data class Progress(val completed: Int, val total: Int, val message: String)
}

enum class Tone { Neutral, Brand, Success, Warning, Danger }

/** A short label for the status chip. Always paired with a word, never color alone. */
data class Phase(val label: String, val tone: Tone, val verdict: Boolean = false)

/**
 * Derives the chip from Capture's own status texts. Only prefixes Capture writes verbatim are
 * matched; anything unrecognised falls back to a neutral label so the chip never overstates.
 */
fun phaseOf(s: CaptureSnapshot): Phase {
    val text = s.status.trim()
    fun starts(vararg prefixes: String) = prefixes.any { text.startsWith(it) }
    if (s.active) return when {
        s.waitingPermission -> Phase("Waiting for permission", Tone.Warning)
        s.progress != null -> Phase("Running", Tone.Brand)
        starts("Recovering") -> Phase("Recovering", Tone.Warning)
        starts("Preparing archive", "Adding observation") -> Phase("Preparing archive", Tone.Neutral)
        starts("Saving archive") -> Phase("Saving", Tone.Neutral)
        starts("Cancelling") -> Phase("Cancelling", Tone.Neutral)
        starts("Selected DAC disconnected") -> Phase("Disconnected", Tone.Warning)
        starts("Capture started") -> Phase("Waiting for DAC", Tone.Brand)
        starts("Found") -> Phase("Connecting", Tone.Brand)
        else -> Phase("Running", Tone.Brand)
    }
    return when {
        starts("USB validation passed") -> Phase("USB passed", Tone.Success, verdict = true)
        starts("Validation completed with findings") -> Phase("Findings", Tone.Warning, verdict = true)
        starts("No admitted exact playback cases") -> Phase("No cases", Tone.Warning, verdict = true)
        starts("USB retirement failed", "Validation failed", "Validation cleanup failed") -> Phase("Failed", Tone.Danger, verdict = true)
        starts("Cancelled") -> Phase("Cancelled", Tone.Neutral, verdict = true)
        starts("Selected DAC disconnected", "Selected DAC lost") -> Phase("Disconnected", Tone.Warning, verdict = true)
        starts("USB permission timed out") -> Phase("Blocked", Tone.Warning, verdict = true)
        starts("Timed out waiting") -> Phase("Not attempted", Tone.Neutral, verdict = true)
        starts("Cannot start", "USB receiver failed") -> Phase("Not started", Tone.Danger, verdict = true)
        starts("Interrupted capture recovered") -> Phase("Interrupted", Tone.Warning, verdict = true)
        starts("Interrupted capture recovery failed", "Archive creation failed") -> Phase("Archive failed", Tone.Danger)
        starts("Save failed") -> Phase("Save failed", Tone.Danger)
        starts("Archive saved") -> Phase("Saved", Tone.Neutral)
        starts("Archive ready") -> Phase("Archive ready", Tone.Neutral)
        starts("Previous archive available") -> Phase("Archive available", Tone.Neutral)
        starts("Ready") -> Phase("Ready", Tone.Neutral)
        else -> Phase("Idle", Tone.Neutral)
    }
}

/** Status texts after a finished run carry a headline line followed by count and format lines. */
fun splitOutcome(status: String): Pair<String, List<String>> {
    val lines = status.trim().lines().map { it.trim() }.filter { it.isNotEmpty() }
    return (lines.firstOrNull() ?: "") to lines.drop(1)
}
