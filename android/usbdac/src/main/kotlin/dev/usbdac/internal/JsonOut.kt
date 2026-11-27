package dev.usbdac.internal

import dev.usbdac.DacConfig
import dev.usbdac.DacQuirk
import dev.usbdac.QuirkOverride
import dev.usbdac.StreamEncoding
import dev.usbdac.StreamRequest
import dev.usbdac.RatePolicy
import dev.usbdac.StartupVolume
import org.json.JSONArray
import org.json.JSONObject

// Encoding of the JSON the native library takes; the schemas are in docs/API.md section 11.

private fun JSONObject.putIfNotNull(key: String, value: Any?): JSONObject =
    if (value == null) this else put(key, value)

private fun quirkJson(quirk: DacQuirk): Any = when (quirk) {
    DacQuirk.SkipClockValidation -> "skip_clock_validation"
    DacQuirk.IgnoreInvalidSampleRate -> "ignore_invalid_sample_rate"
    DacQuirk.RequireVerifiedRate -> "require_verified_rate"
    DacQuirk.PreferPadded24BitTransport -> "prefer_padded_24bit_transport"
    DacQuirk.DsdBigEndian -> "dsd_big_endian"
    DacQuirk.DsdBitReverse -> "dsd_bit_reverse"
    DacQuirk.ExplicitFeedbackPerServiceInterval -> "explicit_feedback_per_service_interval"
    DacQuirk.SetInterfaceFirst -> "set_interface_first"
    DacQuirk.PlaybackUrbFixup -> "playback_urb_fixup"
    DacQuirk.SkipSampleRateReadback -> "skip_sample_rate_readback"
    DacQuirk.SkipImplicitFeedback -> "skip_implicit_feedback"
    DacQuirk.MixerPlaybackMinMute -> "mixer_playback_min_mute"
    DacQuirk.MixerPlaybackLinearVolume -> "mixer_playback_linear_volume"
    DacQuirk.RawDsd32Be -> "raw_dsd32_be"
    DacQuirk.FixedRate -> "fixed_rate"
    DacQuirk.ImplicitSilenceOnEmpty -> "implicit_silence_on_empty"
    DacQuirk.SkipClockSelectorWrites -> "skip_clock_selector_writes"
    DacQuirk.IgnoreOptionalControlErrors -> "ignore_optional_control_errors"
    is DacQuirk.ControlMessageDelayUs -> JSONObject().put("control_message_delay_us", quirk.microseconds)
    is DacQuirk.InterfaceDelayMs -> JSONObject().put("interface_delay_ms", quirk.milliseconds)
    is DacQuirk.SettleDelayMs -> JSONObject().put("settle_delay_ms", quirk.milliseconds)
    is DacQuirk.DsdSubslotSize -> JSONObject().put("dsd_subslot_size", quirk.bytes)
}

private fun overrideJson(entry: QuirkOverride): JSONObject =
    JSONObject()
        .put("vendor_id", entry.vendorId)
        .put("product_id", entry.productId)
        .putIfNotNull("name_contains", entry.nameContains)
        .put("quirks", JSONArray(entry.quirks.map(::quirkJson)))

/**
 * The config as the native library reads it. The hand-back policy is applied by the session
 * (it must restore the volume after the stream has stopped, and it can be chosen per `close`),
 * so the native side always keeps the volume.
 */
internal fun DacConfig.toNativeJson(): String =
    JSONObject()
        .put("buffer_target_ms", bufferTargetMs)
        .put("buffer_capacity_ms", bufferCapacityMs)
        .put("start_threshold_ms", startThresholdMs)
        .put("processing_buffer_target_ms", processingBufferTargetMs)
        .put("require_verified_rate", requireVerifiedRate)
        .putIfNotNull("clock_settle_delay_ms", clockSettleDelayMs)
        .putIfNotNull("dsd_big_endian", dsdBigEndian)
        .putIfNotNull("dsd_subslot", dsdSubslot)
        .putIfNotNull("padded_24bit", padded24Bit)
        .put("allow_unsynchronized_async", allowUnsynchronizedAsync)
        .put("async_fallback_policy", asyncFallbackPolicy.wireName)
        .put("clock_readback_policy", clockReadbackPolicy.wireName)
        .put("adaptive_queue", adaptiveQueue)
        .put("quirk_policy", quirkPolicy.wireName)
        .put("disable_feedback", disableFeedback)
        .put("feedback_smoothing", feedbackSmoothing.wireName)
        .put("capture_feedback_trace", captureFeedbackTrace)
        .put("realtime_priority", realtimePriority)
        .put("hand_back", "keep")
        .put("bus_queue_ms", busQueueMs)
        .put("transfers_in_flight", transfersInFlight)
        .putIfNotNull("transfer_period_us", transferPeriodUs)
        .put("cancel_in_flight_on_flush", cancelInFlightOnFlush)
        .put("require_hardware_volume", requireHardwareVolume)
        .let { json ->
            when (val startup = startupVolume) {
                is StartupVolume.Attenuate ->
                    json.put("startup_volume", "attenuate").put("startup_volume_max_db", startup.maxDb)
                StartupVolume.Keep -> json.put("startup_volume", "keep")
            }
        }
        .put("quirks", JSONArray(quirks.map(::overrideJson)))
        .toString()

internal fun StreamRequest.toJson(): String {
    val json = JSONObject().put("channels", channels)
    when (encoding) {
        StreamEncoding.PCM -> json
            .put("encoding", "pcm")
            .put("sample_rate", sampleRate)
            .put("bit_depth", bitDepth)
        StreamEncoding.PCM_U8, StreamEncoding.PCM_FLOAT32 -> json
            .put("encoding", if (encoding == StreamEncoding.PCM_U8) "pcm_u8" else "pcm_float32")
            .put("sample_rate", sampleRate)
            .put("bit_depth", bitDepth)
        StreamEncoding.DOP -> json
            .put("encoding", "dop")
            .put("dsd_bit_rate", requireNotNull(dsdBitRate) { "a DoP request needs dsdBitRate" })
        StreamEncoding.DSD_NATIVE -> json
            .put("encoding", "dsd_native")
            .put("dsd_bit_rate", requireNotNull(dsdBitRate) { "a native DSD request needs dsdBitRate" })
    }
    json.put("rate_policy", if (ratePolicy == RatePolicy.NEAREST_SUPPORTED) "nearest" else "exact")
    return json.toString()
}

internal fun deviceInfoJson(
    vendorId: Int,
    productId: Int,
    productName: String?,
    manufacturerName: String?,
    deviceName: String?,
    claimedInterfaces: List<Int>,
    configurationId: Int? = null,
    unavailableFeedbackInterfaces: List<Int> = emptyList(),
): String =
    JSONObject()
        .put("vendor_id", vendorId)
        .put("product_id", productId)
        .putIfNotNull("product_name", productName)
        .putIfNotNull("manufacturer_name", manufacturerName)
        .putIfNotNull("device_name", deviceName)
        .put("claimed_interfaces", JSONArray(claimedInterfaces))
        .putIfNotNull("configuration_id", configurationId)
        .put("unavailable_feedback_interfaces", JSONArray(unavailableFeedbackInterfaces))
        .toString()
