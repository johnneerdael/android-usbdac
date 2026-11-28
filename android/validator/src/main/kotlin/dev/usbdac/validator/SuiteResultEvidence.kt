package dev.usbdac.validator

import org.json.JSONObject

/**
 * Compact `suite_result` event for android-events.jsonl.
 *
 * The native validator already writes the complete result atomically to report.json. Copying that
 * report into the event log made a single line tens of MiB on DACs with many fallbacks, which pushed
 * the later session_close and finished events past the HTML report's event-log limit.
 */
internal object SuiteResultEvidence {
    private val scalars = listOf("schema_version", "started_unix_ms", "status", "termination_reason", "cancelled", "error")
    private val arrays = listOf("plan", "legs", "unattempted_plan", "admission_plan", "admission_results", "unattempted_admission_plan", "fallback_results")

    fun summary(result: JSONObject): JSONObject {
        val summary = JSONObject().put("details", "report.json")
        for (key in scalars) if (result.has(key)) summary.put(key, result.get(key))
        val counts = JSONObject()
        for (key in arrays) result.optJSONArray(key)?.let { counts.put(key, it.length()) }
        result.optJSONArray("legs")?.let { legs ->
            counts.put("legs_passed_usb", (0 until legs.length()).count { legs.optJSONObject(it)?.optString("status") == "passed_usb" })
        }
        summary.put("counts", counts)
        result.optJSONObject("mode_coverage")?.let { summary.put("mode_coverage", it) }
        return summary
    }
}
