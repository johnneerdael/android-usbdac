package dev.usbdac

import dev.usbdac.internal.Native
import dev.usbdac.internal.parseStreamFormat
import dev.usbdac.internal.toJson
import org.json.JSONArray
import org.json.JSONObject

/** The core's decision is scoped to the retained USB session. */
internal data class DriverPlaybackDecision(
    val format: StreamFormat,
    val transform: String,
    val fidelity: String,
    val sourceFrameBytes: Int,
    val exactRefusal: String?,
    val selectedHeadroomDb: Double? = null,
)

internal fun DacSession.resolveDriverPlayback(request: StreamRequest, policy: OutputPolicy, bypassLimits: Boolean): DriverPlaybackDecision {
    val options = policy.conversionOptions
    val limits = if (bypassLimits) null else policy.downstreamLimits
    val encodedOptions = JSONObject().put("narrowing", options.narrowing.wireName)
        .put("quality", options.quality.wireName).put("headroom_db", options.headroomDb)
    options.ditherSeed?.let { encodedOptions.put("dither_seed", it) }
    options.sourceLayout?.let { layout ->
        encodedOptions.put("source_sample", if (layout == PcmLayout.FLOAT_32) JSONObject().put("kind", "float")
            else JSONObject().put("kind", "int").put("bits", layout.bits).put("slot_bytes", layout.slotBytes))
    }
    val encodedPolicy = JSONObject().put("allow_lossless", policy.allowWidening || policy.allowConversion)
        .put("allow_widening", policy.allowWidening).put("allow_lossy", policy.allowConversion)
        .put("allow_native_dsd", limits == null)
        .put("dop_confirmed", policy.dopSupport == DopSupport.CONFIRMED)
        .put("allow_dop_pcm", policy.allowDopPcmConversion && policy.allowConversion)
        .put("options", encodedOptions)
    if (limits != null) encodedPolicy.put("max_pcm_rate", limits.maxSampleRateHz)
        .put("allowed_pcm_depths", JSONArray(limits.allowedValidBits.toList()))
    val value = JSONObject(nativeCall { Native.playbackResolve(driverHandle(), request.toJson(), encodedPolicy.toString()) })
    return DriverPlaybackDecision(
        format = parseStreamFormat(value.getJSONObject("format").toString()),
        transform = value.getString("transform"),
        fidelity = value.getString("fidelity"),
        sourceFrameBytes = value.getInt("source_frame_bytes"),
        exactRefusal = value.optString("exact_refusal").takeIf { it.isNotBlank() && it != "null" },
        selectedHeadroomDb = if (value.isNull("selected_headroom_db")) null else value.getDouble("selected_headroom_db"),
    )
}
