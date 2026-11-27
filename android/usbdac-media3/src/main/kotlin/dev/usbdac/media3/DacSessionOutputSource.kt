package dev.usbdac.media3

import dev.usbdac.*

/**
 * Writes to an already-open session. Opening [DacSession] discovers and initializes its hardware
 * volume without opening a stream or programming its clock. The host retains session ownership:
 * closing a track output closes only its stream, so volume remains usable between tracks.
 *
 * Only equal-rate/equal-channel Type I PCM is accepted. Unsigned8 and float32 retain exact bits. [allowWidening] permits a wider integer
 * depth and zero-padded slots; the sink preserves every input bit. No resampling or narrowing occurs.
 * DAC ducking is deliberately host-owned via [volumeMultiplier], so the host can restore its level.
 */
class DacSessionOutputSource(
    private val session: DacSession,
    private val allowWidening: Boolean = true,
    private val volumeMultiplier: (Float) -> Unit = {},
) : OutputSource {
    private val supportSnapshot = session.capabilities.altSettings.map { alt ->
        alt.copy(sampleRates = alt.sampleRates.toList(), rateRanges = alt.rateRanges.toList())
    }

    override fun formatSupport(request: StreamRequest): OutputSupport =
        directSessionSupport(supportSnapshot, request, allowWidening)

    override fun open(request: StreamRequest): OpenedOutput {
        require(request.encoding in listOf(StreamEncoding.PCM, StreamEncoding.PCM_U8, StreamEncoding.PCM_FLOAT32)) { "Media3 integration requires integer PCM" }
        val exact = request.copy(ratePolicy = RatePolicy.EXACT)
        var original: DacException? = null
        val depths = listOf(request.bitDepth) + if (allowWidening && request.encoding == StreamEncoding.PCM) {
            session.capabilities.pcmBitDepths.filter { it > request.bitDepth }.distinct().sorted()
        } else emptyList()
        for (depth in depths) {
            val wire = try {
                session.negotiate(exact.copy(bitDepth = depth))
            } catch (error: DacException) {
                if (error.code != DacErrorCode.UNSUPPORTED) throw error
                if (original == null) original = error
                continue
            }
            check(wire.encoding == request.encoding && wire.sampleRate == request.sampleRate && wire.counterRateHz == request.sampleRate &&
                wire.channels == request.channels && wire.validBits >= request.bitDepth &&
                wire.subslotBytes >= request.bitDepth / 8) { "negotiation is not lossless integer widening" }
            val stream = session.openStream(wire)
            return OpenedOutput(stream, WriteFormat(wire.sampleRate, wire.channels, wire.subslotBytes,
                wire.validBits, wire.counterRateHz, wire.encoding), request)
        }
        throw checkNotNull(original)
    }

    override fun reconfigure(output: OpenedOutput, request: StreamRequest): OutputReconfiguration? {
        val current = output.output as? DacStream ?: return null
        val candidates = reconfigurationCandidates(supportSnapshot, request, allowWidening)
        if (candidates.isEmpty()) throw DacException(DacErrorCode.UNSUPPORTED, "no cached same-rate output alternate")
        val report = session.reconfigureCandidates(current, candidates)
        val wire = report.stream.format
        val actualRequest = if (report.rollbackCause == null) request else output.request
        return OutputReconfiguration(OpenedOutput(report.stream, WriteFormat(wire.sampleRate, wire.channels,
            wire.subslotBytes, wire.validBits, wire.counterRateHz, wire.encoding), actualRequest), report.rollbackCause)
    }

    override fun close(output: OpenedOutput) = output.output.close()
    override fun takeLookAgain(): Boolean = false
    override fun setVolumeMultiplier(multiplier: Float) = volumeMultiplier(multiplier)
}

internal fun reconfigurationCandidates(alts: List<AltSetting>, request: StreamRequest, allowWidening: Boolean): List<StreamRequest> {
    val depths = (listOf(request.bitDepth) + if (allowWidening && request.encoding == StreamEncoding.PCM) {
        alts.filter { it.format == AltFormat.PCM }.map { it.validBits }
            .filter { it > request.bitDepth }.distinct().sorted()
    } else emptyList()).distinct()
    return depths.map { request.copy(bitDepth = it, ratePolicy = RatePolicy.EXACT) }
        .filter { directSessionSupport(alts, it, false).support == OutputFormatSupport.DIRECT }
}

/** Conservative descriptor-only check; no clock programming or stream open occurs. */
internal fun directSessionSupport(alts: List<AltSetting>, request: StreamRequest, allowWidening: Boolean): OutputSupport {
    when (request.encoding) {
        StreamEncoding.PCM, StreamEncoding.PCM_U8, StreamEncoding.PCM_FLOAT32 -> Unit
        else -> return OutputSupport(OutputFormatSupport.UNSUPPORTED, request, reason = "unsupported source encoding")
    }
    val candidates = alts.filter { alt ->
        val exact = pcmAlternateSupports(alt, request)
        val widened = allowWidening && request.encoding == StreamEncoding.PCM &&
            alt.validBits > request.bitDepth && pcmAlternateSupports(alt, request.copy(bitDepth = alt.validBits))
        exact || widened
    }
    val alt = candidates.minByOrNull { it.validBits } ?:
        return OutputSupport(OutputFormatSupport.UNSUPPORTED, request, reason = "no matching cached alternate")
    val processing = if (alt.validBits == request.bitDepth) OutputFormatSupport.DIRECT else OutputFormatSupport.WITH_TRANSCODING
    return OutputSupport(processing, request, request.copy(bitDepth = alt.validBits),
        if (processing == OutputFormatSupport.DIRECT) "cached exact format" else "integer widening")
}
