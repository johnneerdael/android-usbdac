package dev.usbdac.internal

import dev.usbdac.CaptureChannel
import dev.usbdac.CaptureChannelTopology
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
                val topology = channel.topology
                when (topology) {
                    is CaptureChannelTopology.Information -> {
                        require(channel.position == CaptureChannelPosition.ADC3_INFORMATION)
                        put("topology", JSONObject().put("type", "adc3Information").put("purpose", topology.purpose).put("relationship", topology.relationship).put("group", topology.group))
                    }
                    is CaptureChannelTopology.Ambisonic -> {
                        require(channel.position == CaptureChannelPosition.ADC3_AMBISONIC)
                        put("topology", JSONObject().put("type", "adc3Ambisonic").put("ordering", topology.ordering).put("acn", topology.acn).put("normalization", topology.normalization).put("group", topology.group))
                    }
                    null -> require(channel.position != CaptureChannelPosition.ADC3_INFORMATION && channel.position != CaptureChannelPosition.ADC3_AMBISONIC)
                }
            }) }
        })
    }
}.toString()

private inline fun <reified T : Enum<T>> enumLabel(label: String): T =
    enumValues<T>().firstOrNull { it.name == label.uppercase(Locale.ROOT) }
        ?: throw JSONException("unknown capture label: $label")

/** Shared ordered identity decoder for capture and playback. */
internal fun parseChannelMap(o: JSONObject): List<CaptureChannel> {
    val entries = o.getJSONArray("channel_map")
    return List(entries.length()) { index ->
        val channel = entries.getJSONObject(index)
        if (channel.getInt("index") != index) throw JSONException("capture channel order changed")
        val position = enumLabel<CaptureChannelPosition>(channel.getString("label"))
        val topology = channel.optJSONObject("topology")
        fun byte(name: String): Int = (topology ?: throw JSONException("missing ADC3 topology")).getInt(name).also {
            if (it !in 0..255) throw JSONException("ADC3 topology byte out of range")
        }
        val identity = when (position) {
            CaptureChannelPosition.ADC3_INFORMATION -> {
                if (topology?.getString("type") != "adc3Information") throw JSONException("ADC3 topology type mismatch")
                CaptureChannelTopology.Information(byte("purpose"), byte("relationship"), byte("group"))
            }
            CaptureChannelPosition.ADC3_AMBISONIC -> {
                if (topology?.getString("type") != "adc3Ambisonic") throw JSONException("ADC3 topology type mismatch")
                CaptureChannelTopology.Ambisonic(byte("ordering"), byte("acn"), byte("normalization"), byte("group"))
            }
            else -> {
                if (topology != null) throw JSONException("unexpected ADC3 topology")
                null
            }
        }
        CaptureChannel(index, position, identity)
    }
}

internal fun parseCaptureFormat(json: String): CaptureFormat {
    val o = JSONObject(json)
    if (o.getString("encoding") != "pcm") throw JSONException("capture is not integer PCM")
    val map = parseChannelMap(o)
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
        o.getLong("missing_frames"), o.getLong("reordered_completions"), o.getLong("device_position_frames"), error,
        o.optLong("contention_frames", 0L), o.optLong("proof_suppressed_frames", 0L))
}
