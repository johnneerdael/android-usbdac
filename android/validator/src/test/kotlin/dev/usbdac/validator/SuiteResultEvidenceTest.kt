package dev.usbdac.validator

import org.json.JSONArray
import org.json.JSONObject
import org.junit.Assert.*
import org.junit.Test

class SuiteResultEvidenceTest {
    private fun bulky(count: Int) = JSONArray((0 until count).map { JSONObject().put("status", "passed_usb").put("stats", "x".repeat(10_000)) })

    @Test fun summaryStaysSmallAndPointsToReportJson() {
        val result = JSONObject().put("schema_version", 1).put("started_unix_ms", 42L).put("status", "passed_usb")
            .put("termination_reason", JSONObject.NULL).put("cancelled", false).put("error", JSONObject.NULL)
            .put("plan", bulky(50)).put("legs", bulky(50)).put("unattempted_plan", JSONArray())
            .put("admission_plan", bulky(188)).put("admission_results", bulky(188)).put("unattempted_admission_plan", JSONArray())
            .put("fallback_results", bulky(200))
            .put("mode_coverage", JSONObject().put("pcm", JSONObject().put("planned", 30).put("exercised", 30).put("passed", 30)))
        val summary = SuiteResultEvidence.summary(result)
        assertTrue("summary must stay far below the 8 MiB event-log limit", summary.toString().length < 4_096)
        assertEquals("report.json", summary.getString("details"))
        assertEquals("passed_usb", summary.getString("status"))
        assertEquals(1, summary.getInt("schema_version"))
        assertEquals(42L, summary.getLong("started_unix_ms"))
        assertFalse(summary.getBoolean("cancelled"))
        assertTrue(summary.isNull("termination_reason"))
        assertTrue(summary.isNull("error"))
        val counts = summary.getJSONObject("counts")
        assertEquals(50, counts.getInt("plan")); assertEquals(50, counts.getInt("legs")); assertEquals(50, counts.getInt("legs_passed_usb"))
        assertEquals(0, counts.getInt("unattempted_plan"))
        assertEquals(188, counts.getInt("admission_plan")); assertEquals(188, counts.getInt("admission_results"))
        assertEquals(200, counts.getInt("fallback_results"))
        assertEquals(30, summary.getJSONObject("mode_coverage").getJSONObject("pcm").getInt("passed"))
    }

    @Test fun failureFieldsSurviveAndMissingArraysAreAbsentNotZero() {
        val summary = SuiteResultEvidence.summary(JSONObject().put("status", "failed").put("error", "boom")
            .put("termination_reason", "retirement_failed").put("cancelled", true))
        assertEquals("failed", summary.getString("status"))
        assertEquals("boom", summary.getString("error"))
        assertEquals("retirement_failed", summary.getString("termination_reason"))
        assertTrue(summary.getBoolean("cancelled"))
        assertFalse(summary.getJSONObject("counts").has("legs"))
    }
}
