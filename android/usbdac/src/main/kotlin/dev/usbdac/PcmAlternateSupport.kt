package dev.usbdac

/** Descriptor-only PCM admission. This does not establish that a clock can be programmed. */
public fun pcmAlternateSupports(alt: AltSetting, request: StreamRequest): Boolean {
    val format = when (request.encoding) {
        StreamEncoding.PCM -> AltFormat.PCM
        StreamEncoding.PCM_U8 -> AltFormat.PCM_U8
        StreamEncoding.PCM_FLOAT32 -> AltFormat.PCM_FLOAT32
        else -> return false
    }
    if (request.encoding == StreamEncoding.PCM_U8 && (request.bitDepth != 8 || alt.subslotBytes != 1)) return false
    if (request.encoding == StreamEncoding.PCM_FLOAT32 && (request.bitDepth != 32 || alt.subslotBytes != 4)) return false
    val rate = request.sampleRate
    val rateSupported = rate in alt.sampleRates || alt.rateRanges.any { range ->
        rate in range.minHz..range.maxHz && if (range.stepHz > 0)
            (rate.toLong() - range.minHz) % range.stepHz == 0L
        else range.stepHz == 0 && range.minHz == range.maxHz
    }
    if (rate <= 0 || alt.format != format || alt.channels != request.channels || alt.channels <= 0 ||
        alt.validBits != request.bitDepth || alt.subslotBytes !in 1..4 ||
        alt.validBits <= 0 || alt.validBits > alt.subslotBytes * 8 || alt.pacing == null ||
        alt.serviceIntervalUs <= 0 || alt.maxPacketBytes <= 0) return false
    val frames = (rate.toLong() * alt.serviceIntervalUs + 999_999L) / 1_000_000L
    val stride = alt.channels.toLong() * alt.subslotBytes
    return rateSupported && frames <= alt.maxPacketBytes.toLong() / stride
}
