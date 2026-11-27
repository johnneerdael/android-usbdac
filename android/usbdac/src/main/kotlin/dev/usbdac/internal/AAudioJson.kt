package dev.usbdac.internal

import dev.usbdac.AAudioConfig
import dev.usbdac.AAudioGrant
import dev.usbdac.AAudioHardware
import dev.usbdac.AAudioPerformanceMode
import dev.usbdac.AAudioSampleFormat
import dev.usbdac.AAudioSharingMode
import dev.usbdac.StreamRequest
import org.json.JSONObject

// The JSON of the AAudio output; the schemas are in docs/API.md section 13.

/** The request `aaudioOpen` reads. */
internal fun aaudioRequestJson(
    request: StreamRequest,
    format: AAudioSampleFormat,
    deviceId: Int,
    config: AAudioConfig,
): String =
    JSONObject()
        .put("sample_rate", request.sampleRate)
        .put("channels", request.channels)
        .put("format", format.wireName)
        .put("device_id", deviceId)
        .put("performance_mode", config.performanceMode.wireName)
        .put("buffer_target_ms", config.bufferTargetMs)
        .put("buffer_capacity_ms", config.bufferCapacityMs)
        .put("start_threshold_ms", config.startThresholdMs)
        .put("device_buffer_bursts", config.deviceBufferBursts ?: JSONObject.NULL)
        .toString()

private fun JSONObject.booleanOrNull(key: String): Boolean? = if (isNull(key)) null else getBoolean(key)

private fun performanceMode(name: String?): AAudioPerformanceMode? =
    AAudioPerformanceMode.entries.firstOrNull { it.wireName == name }

/** Reads what `aaudioGrant` returns. A missing key throws; `nativeCall` turns that into a `DacException`. */
internal fun parseAAudioGrant(json: String): AAudioGrant {
    val value = JSONObject(json)
    val hardware = if (value.isNull("hardware")) {
        null
    } else {
        value.getJSONObject("hardware").let { hardware ->
            AAudioHardware(
                sampleRate = hardware.getInt("sample_rate"),
                channels = hardware.getInt("channels"),
                format = AAudioSampleFormat.fromWireName(if (hardware.isNull("format")) null else hardware.getString("format")),
            )
        }
    }
    return AAudioGrant(
        sharingMode = if (value.getString("sharing") == "exclusive") AAudioSharingMode.EXCLUSIVE else AAudioSharingMode.SHARED,
        sampleRate = value.getInt("sample_rate"),
        channels = value.getInt("channels"),
        format = AAudioSampleFormat.fromWireName(if (value.isNull("format")) null else value.getString("format")),
        framesPerBurst = value.getInt("frames_per_burst"),
        bufferCapacityFrames = value.getInt("buffer_capacity_frames"),
        bufferSizeFrames = value.getInt("buffer_size_frames"),
        deviceId = value.getInt("device_id"),
        performanceMode = performanceMode(if (value.isNull("performance_mode")) null else value.getString("performance_mode")),
        usesMmap = value.booleanOrNull("mmap"),
        hardware = hardware,
        asRequested = value.getBoolean("as_requested"),
        verdict = value.getString("verdict"),
        reason = value.getString("reason"),
        deviceIds = value.optJSONArray("device_ids")?.let { ids -> List(ids.length()) { ids.getInt(it) } },
    )
}
