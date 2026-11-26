package dev.usbdac.internal

import dev.usbdac.AltFormat
import dev.usbdac.AltSetting
import dev.usbdac.BitPerfectBlocker
import dev.usbdac.DacCapabilities
import dev.usbdac.DacStreamStats
import dev.usbdac.LinkSpeed
import dev.usbdac.RateRange
import dev.usbdac.StreamEncoding
import dev.usbdac.StreamFormat
import dev.usbdac.StreamState
import dev.usbdac.SyncType
import dev.usbdac.UacVersion
import dev.usbdac.VolumeRange
import dev.usbdac.VolumeState
import org.json.JSONArray
import org.json.JSONObject

// Parsing of the JSON the native library returns; the schemas are in docs/API.md section 11.
// Keys are snake_case, `null` stands for "none", enums are lower-case strings.

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
        supportsPcm = value.getBoolean("supports_pcm"),
        supportsDop = value.getBoolean("supports_dop"),
        dopMaxDsdBitRate = value.intOrNull("dop_max_dsd_bit_rate"),
        supportsNativeDsd = value.getBoolean("supports_native_dsd"),
        nativeDsdBitRates = value.optJSONArray("native_dsd_bit_rates").ints(),
        hasExplicitFeedback = value.getBoolean("has_explicit_feedback"),
        volume = value.objectOrNull("volume")?.let(::parseVolumeRange),
        hasMute = value.getBoolean("has_mute"),
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
    )
}

internal fun parseStats(json: String): DacStreamStats {
    val v = JSONObject(json)
    return DacStreamStats(
        state = StreamState.fromCode(v.getInt("state")),
        bitPerfect = v.getBoolean("bit_perfect"),
        bitPerfectBlocker = BitPerfectBlocker.fromWireName(v.stringOrNull("bit_perfect_blocker")),
        softwareGain = v.getDouble("software_gain").toFloat(),
        underruns = v.getLong("underruns"),
        lostFrames = v.getLong("lost_frames"),
        shortPackets = v.getLong("short_packets"),
        zeroPackets = v.getLong("zero_packets"),
        urbTimeouts = v.getLong("urb_timeouts"),
        completedTransfers = v.getLong("completed_transfers"),
        framesWritten = v.getLong("frames_written"),
        framesCompleted = v.getLong("frames_completed"),
        playedFrames = v.getLong("played_frames"),
        latencyFrames = v.getLong("latency_frames"),
        latencyMs = v.getDouble("latency_ms"),
        queuedFrames = v.getLong("queued_frames"),
        bufferMs = v.getLong("buffer_ms"),
        bufferTargetMs = v.getLong("buffer_target_ms"),
        bufferCapacityMs = v.getLong("buffer_capacity_ms"),
        transferQueueDepth = v.getInt("transfer_queue_depth"),
        lastTurnaroundUs = v.getLong("last_turnaround_us"),
        maxTurnaroundUs = v.getLong("max_turnaround_us"),
        lastSubmitGapUs = v.getLong("last_submit_gap_us"),
        maxSubmitGapUs = v.getLong("max_submit_gap_us"),
        effectiveRateHz = v.getDouble("effective_rate_hz"),
        feedbackActive = v.getBoolean("feedback_active"),
        feedbackReports = v.getLong("feedback_reports"),
        feedbackTimeouts = v.getLong("feedback_timeouts"),
        feedbackErrors = v.getLong("feedback_errors"),
        feedbackRejected = v.getLong("feedback_rejected"),
        feedbackAgeMs = v.longOrNull("feedback_age_ms"),
        feedbackRateHz = v.doubleOrNull("feedback_rate_hz"),
        feedbackStale = v.getBoolean("feedback_stale"),
    )
}
