package dev.usbdac.internal

import dev.usbdac.AltFormat
import dev.usbdac.AltSetting
import dev.usbdac.BitPerfectBlocker
import dev.usbdac.ClockChoice
import dev.usbdac.ClockHop
import dev.usbdac.ClockHopKind
import dev.usbdac.ClockResolution
import dev.usbdac.DacCapabilities
import dev.usbdac.DacStreamStats
import dev.usbdac.LinkSpeed
import dev.usbdac.Pacing
import dev.usbdac.RateRange
import dev.usbdac.StreamEncoding
import dev.usbdac.StreamFormat
import dev.usbdac.StreamState
import dev.usbdac.SyncType
import dev.usbdac.UacVersion
import dev.usbdac.StartupVolumeReport
import dev.usbdac.VolumeRange
import dev.usbdac.VolumeState
import org.json.JSONArray
import org.json.JSONObject

// Parsing of the JSON the native library returns; the schemas are in docs/API.md section 11.
// Keys are snake_case, `null` stands for "none", enums are lower-case strings.
//
// What the host acts on (the negotiated format, the volume) is read strictly: a missing key throws,
// and `nativeCall` turns that into a DacException. Capabilities and statistics are descriptive, so a key
// a newer or older native library lacks reads as its zero value instead of failing the call.

private fun JSONObject.intOrNull(key: String): Int? = if (isNull(key)) null else getInt(key)
private fun JSONObject.longOrNull(key: String): Long? = if (isNull(key)) null else getLong(key)
private fun JSONObject.doubleOrNull(key: String): Double? = if (isNull(key)) null else getDouble(key)
private fun JSONObject.stringOrNull(key: String): String? = if (isNull(key)) null else getString(key)
private fun JSONObject.objectOrNull(key: String): JSONObject? = if (isNull(key)) null else getJSONObject(key)

private fun JSONArray?.ints(): List<Int> = if (this == null) emptyList() else List(length()) { getInt(it) }

private fun uacVersion(name: String?): UacVersion = when (name) {
    "v1" -> UacVersion.V1
    "v2" -> UacVersion.V2
    else -> UacVersion.UNKNOWN
}

private fun linkSpeed(name: String?): LinkSpeed = when (name) {
    "low" -> LinkSpeed.LOW
    "full" -> LinkSpeed.FULL
    "high" -> LinkSpeed.HIGH
    "super" -> LinkSpeed.SUPER
    "super_plus" -> LinkSpeed.SUPER_PLUS
    else -> LinkSpeed.UNKNOWN
}

private fun syncType(name: String?): SyncType = when (name) {
    "asynchronous" -> SyncType.ASYNCHRONOUS
    "adaptive" -> SyncType.ADAPTIVE
    "synchronous" -> SyncType.SYNCHRONOUS
    else -> SyncType.NONE
}

// The clock and pacing diagnostics describe, they are not acted on: an absent, null or unknown value
// reads as "not reported" instead of failing the negotiated format.

private fun pacing(json: JSONObject?): Pacing? = json?.let {
    when (it.stringOrNull("mode")) {
        "explicit_feedback" -> Pacing.ExplicitFeedback(it.optInt("feedback_endpoint"))
        "implicit_feedback" -> it.optJSONObject("implicit_feedback")?.let { source ->
            Pacing.ImplicitFeedback(source.getInt("interface_number"), source.getInt("alt_setting"), source.getInt("endpoint"))
        } ?: Pacing.Unknown
        "adaptive" -> Pacing.Adaptive
        "synchronous" -> Pacing.Synchronous
        "unsynchronized" -> Pacing.Unsynchronized
        else -> Pacing.Unknown
    }
}

private fun clockHop(json: JSONObject) = ClockHop(
    id = json.optInt("id"),
    kind = when (json.stringOrNull("kind")) {
        "terminal" -> ClockHopKind.Terminal
        "selector" -> ClockHopKind.Selector(json.optInt("selected_pin"), json.optInt("pin_count"))
        "multiplier" -> ClockHopKind.Multiplier
        "source" -> ClockHopKind.Source
        else -> ClockHopKind.Unknown
    },
)

private fun clockChoice(json: JSONObject?): ClockChoice? = json?.let {
    val hops = it.optJSONArray("path")
    ClockChoice(
        controlInterface = it.optInt("control_interface"),
        sourceId = it.optInt("source_id"),
        path = if (hops == null) emptyList() else List(hops.length()) { index -> clockHop(hops.getJSONObject(index)) },
        resolution = when (it.stringOrNull("resolution")) {
            "resolved" -> ClockResolution.RESOLVED
            "fallback_first_source" -> ClockResolution.FALLBACK_FIRST_SOURCE
            else -> ClockResolution.UNKNOWN
        },
    )
}

private fun altFormat(name: String?): AltFormat = when (name) {
    "pcm" -> AltFormat.PCM
    "dsd" -> AltFormat.DSD
    else -> AltFormat.OTHER
}

internal fun parseVolumeRange(json: String): VolumeRange = parseVolumeRange(JSONObject(json))

private fun parseVolumeRange(json: JSONObject) = VolumeRange(
    minDb = json.getDouble("min_db"),
    maxDb = json.getDouble("max_db"),
    resDb = json.getDouble("res_db"),
)

internal fun parseVolumeState(json: String): VolumeState {
    val value = JSONObject(json)
    return VolumeState(
        db = value.getDouble("db"),
        fraction = value.getDouble("fraction"),
        muted = if (value.isNull("muted")) null else value.getBoolean("muted"),
    )
}

internal fun parseStartupReport(json: String): StartupVolumeReport {
    val value = JSONObject(json)
    fun doubles(key: String): List<Double> =
        value.getJSONArray(key).let { array -> List(array.length()) { array.getDouble(it) } }
    return StartupVolumeReport(
        originalDb = doubles("original_db"),
        appliedDb = doubles("applied_db"),
        limitDb = value.getDouble("limit_db"),
        attenuated = value.getBoolean("attenuated"),
        clampedToMinimum = value.getBoolean("clamped_to_minimum"),
        maxDb = value.doubleOrNull("max_db"),
    )
}

private fun parseAltSetting(json: JSONObject) = AltSetting(
    interfaceNumber = json.getInt("interface_number"),
    altSetting = json.getInt("alt_setting"),
    endpointAddress = json.getInt("endpoint_address"),
    format = altFormat(json.stringOrNull("format")),
    channels = json.getInt("channels"),
    subslotBytes = json.getInt("subslot_bytes"),
    validBits = json.getInt("valid_bits"),
    rateRanges = json.optJSONArray("rate_ranges").let { ranges ->
        if (ranges == null) emptyList() else List(ranges.length()) {
            val range = ranges.getJSONObject(it)
            RateRange(range.getInt("min_hz"), range.getInt("max_hz"), range.getInt("step_hz"))
        }
    },
    sampleRates = json.optJSONArray("sample_rates").ints(),
    syncType = syncType(json.stringOrNull("sync_type")),
    feedbackEndpoint = json.intOrNull("feedback_endpoint"),
    maxPacketBytes = json.getInt("max_packet_bytes"),
    serviceIntervalUs = json.getInt("service_interval_us"),
    uacVersion = uacVersion(json.stringOrNull("uac_version")),
    pacing = pacing(json.optJSONObject("pacing")),
)

internal fun parseCapabilities(json: String): DacCapabilities {
    val value = JSONObject(json)
    val alts = value.optJSONArray("alt_settings")
    return DacCapabilities(
        uacVersion = uacVersion(value.stringOrNull("uac_version")),
        linkSpeed = linkSpeed(value.stringOrNull("link_speed")),
        altSettings = if (alts == null) emptyList() else List(alts.length()) { parseAltSetting(alts.getJSONObject(it)) },
        pcmSampleRates = value.optJSONArray("pcm_sample_rates").ints(),
        pcmBitDepths = value.optJSONArray("pcm_bit_depths").ints(),
        channelCounts = value.optJSONArray("channel_counts").ints(),
        supportsPcm = value.optBoolean("supports_pcm"),
        supportsDop = value.optBoolean("supports_dop"),
        dopMaxDsdBitRate = value.intOrNull("dop_max_dsd_bit_rate"),
        supportsNativeDsd = value.optBoolean("supports_native_dsd"),
        nativeDsdBitRates = value.optJSONArray("native_dsd_bit_rates").ints(),
        hasExplicitFeedback = value.optBoolean("has_explicit_feedback"),
        volume = value.objectOrNull("volume")?.let(::parseVolumeRange),
        hasHardwareVolume = value.optBoolean("has_hardware_volume", !value.isNull("volume")),
        hasMute = value.optBoolean("has_mute"),
    )
}

internal fun parseStreamFormat(json: String): StreamFormat {
    val value = JSONObject(json)
    return StreamFormat(
        formatId = value.getLong("format_id"),
        encoding = when (value.stringOrNull("encoding")) {
            "dop" -> StreamEncoding.DOP
            "dsd_native" -> StreamEncoding.DSD_NATIVE
            else -> StreamEncoding.PCM
        },
        dsdBitRate = value.intOrNull("dsd_bit_rate"),
        sampleRate = value.getInt("sample_rate"),
        channels = value.getInt("channels"),
        validBits = value.getInt("valid_bits"),
        subslotBytes = value.getInt("subslot_bytes"),
        bytesPerFrame = value.getInt("bytes_per_frame"),
        counterRateHz = value.getInt("counter_rate_hz"),
        interfaceNumber = value.getInt("interface_number"),
        altSetting = value.getInt("alt_setting"),
        endpointAddress = value.getInt("endpoint_address"),
        syncType = syncType(value.stringOrNull("sync_type")),
        explicitFeedback = value.getBoolean("explicit_feedback"),
        linkSpeed = linkSpeed(value.stringOrNull("link_speed")),
        serviceIntervalUs = value.getInt("service_interval_us"),
        rateDiffersFromRequest = value.getBoolean("rate_differs_from_request"),
        pacing = pacing(value.optJSONObject("pacing")) ?: Pacing.Unknown,
        clock = clockChoice(value.optJSONObject("clock")),
    )
}

internal fun parseStats(json: String): DacStreamStats {
    val v = JSONObject(json)
    return DacStreamStats(
        state = StreamState.fromCode(v.optInt("state", -1)),
        bitPerfect = v.optBoolean("bit_perfect"),
        bitPerfectBlocker = BitPerfectBlocker.fromWireName(v.stringOrNull("bit_perfect_blocker")),
        softwareGain = v.optDouble("software_gain", 0.0).toFloat(),
        underruns = v.optLong("underruns"),
        lostFrames = v.optLong("lost_frames"),
        shortPackets = v.optLong("short_packets"),
        zeroPackets = v.optLong("zero_packets"),
        urbTimeouts = v.optLong("urb_timeouts"),
        completedTransfers = v.optLong("completed_transfers"),
        framesWritten = v.optLong("frames_written"),
        framesCompleted = v.optLong("frames_completed"),
        playedFrames = v.optLong("played_frames"),
        latencyFrames = v.optLong("latency_frames"),
        latencyMs = v.optDouble("latency_ms", 0.0),
        queuedFrames = v.optLong("queued_frames"),
        bufferMs = v.optLong("buffer_ms"),
        bufferTargetMs = v.optLong("buffer_target_ms"),
        bufferCapacityMs = v.optLong("buffer_capacity_ms"),
        transferQueueDepth = v.optInt("transfer_queue_depth"),
        lastTurnaroundUs = v.optLong("last_turnaround_us"),
        maxTurnaroundUs = v.optLong("max_turnaround_us"),
        lastSubmitGapUs = v.optLong("last_submit_gap_us"),
        maxSubmitGapUs = v.optLong("max_submit_gap_us"),
        effectiveRateHz = v.optDouble("effective_rate_hz", 0.0),
        feedbackActive = v.optBoolean("feedback_active"),
        feedbackLocked = v.optBoolean("feedback_locked"),
        feedbackEmpty = v.optLong("feedback_empty"),
        implicitFifoDepth = v.optInt("implicit_fifo_depth"),
        implicitFifoHighWater = v.optInt("implicit_fifo_high_water"),
        implicitWaits = v.optLong("implicit_waits"),
        implicitEmptyBus = v.optLong("implicit_empty_bus"),
        implicitOverruns = v.optLong("implicit_overruns"),
        implicitInTransfers = v.optLong("implicit_in_transfers"),
        implicitOutOfOrder = v.optLong("implicit_out_of_order"),
        implicitInvalidPackets = v.optLong("implicit_invalid_packets"),
        implicitMeasuredFrames = v.optLong("implicit_measured_frames"),
        implicitScheduledFrames = v.optLong("implicit_scheduled_frames"),

        feedbackReports = v.optLong("feedback_reports"),
        feedbackTimeouts = v.optLong("feedback_timeouts"),
        feedbackErrors = v.optLong("feedback_errors"),
        feedbackRejected = v.optLong("feedback_rejected"),
        feedbackAgeMs = v.longOrNull("feedback_age_ms"),
        feedbackRateHz = v.doubleOrNull("feedback_rate_hz"),
        feedbackStale = v.optBoolean("feedback_stale"),
    )
}
