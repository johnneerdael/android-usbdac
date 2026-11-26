package dev.usbdac.internal

import dev.usbdac.CaptureChannel
import dev.usbdac.CaptureChannelPosition
import dev.usbdac.CaptureFormat
import dev.usbdac.CaptureRequest
import dev.usbdac.CaptureState
import dev.usbdac.CaptureStats
import dev.usbdac.DacErrorCode
import dev.usbdac.DacException
import org.json.JSONArray
import org.json.JSONException
import org.json.JSONObject
import java.util.Locale

internal fun CaptureRequest.toCaptureJson(): String = JSONObject().apply {
    put("sample_rate", sampleRate)
    put("channels", channels)
    put("valid_bits", validBits)
    put("encoding", "pcm")
    put("rate_policy", "exact")
    channelMap?.let { channels ->
        put("channel_map", JSONArray().apply {
            channels.forEach { channel -> put(JSONObject().apply {
                put("index", channel.index)
                put("label", channel.position.name.lowercase(Locale.ROOT))
            }) }
        })
    }
}.toString()

private inline fun <reified T : Enum<T>> enumLabel(label: String): T =
    enumValues<T>().firstOrNull { it.name == label.uppercase(Locale.ROOT) }
        ?: throw JSONException("unknown capture label: $label")

internal fun parseCaptureFormat(json: String): CaptureFormat {
    val o = JSONObject(json)
    if (o.getString("encoding") != "pcm") throw JSONException("capture is not integer PCM")
    val entries = o.getJSONArray("channel_map")
    val map = List(entries.length()) { index ->
        val channel = entries.getJSONObject(index)
        if (channel.getInt("index") != index) throw JSONException("capture channel order changed")
        CaptureChannel(index, enumLabel<CaptureChannelPosition>(channel.getString("label")))
    }
    val channels = o.getInt("channels")
    val subslot = o.getInt("subslot_bytes")
    val bits = o.getInt("valid_bits")
    if (channels <= 0 || channels != map.size || subslot !in 1..4 || bits !in 1..(subslot * 8)) {
        throw JSONException("invalid capture wire geometry")
    }
    if (o.getInt("bytes_per_frame") != channels * subslot || o.getInt("sample_rate") <= 0) {
        throw JSONException("invalid capture frame size or rate")
    }
    return CaptureFormat(o.getInt("sample_rate"), channels, bits, subslot, map,
        o.getInt("configuration_id"), o.getInt("interface_number"), o.getInt("alt_setting"), o.getInt("endpoint_address"))
}

internal fun parseCaptureStats(json: String): CaptureStats {
    val o = JSONObject(json)
    val error = o.optJSONObject("error")?.let {
        DacException(DacErrorCode.fromWireName(it.getString("code")), it.getString("message"))
    }
    return CaptureStats(enumLabel<CaptureState>(o.getString("state")), o.getLong("available_frames"),
        o.getLong("captured_frames"), o.getLong("delivered_frames"), o.getLong("overrun_frames"),
        o.getLong("empty_packets"), o.getLong("short_packets"), o.getLong("failed_packets"),
        o.getLong("malformed_packets"), o.getLong("malformed_completions"), o.getLong("failed_transfers"),
        o.getLong("missing_frames"), o.getLong("reordered_completions"), o.getLong("device_position_frames"), error)
}
