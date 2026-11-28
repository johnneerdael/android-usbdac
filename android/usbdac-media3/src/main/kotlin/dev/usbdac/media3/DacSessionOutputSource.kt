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
class DacSessionOutputSource internal constructor(
    private val session: DirectSession,
    private val allowWidening: Boolean,
    private val volumeMultiplier: (Float) -> Unit,
) : OutputSource {
    constructor(
        session: DacSession,
        allowWidening: Boolean = true,
        volumeMultiplier: (Float) -> Unit = {},
    ) : this(SessionDirect(session), allowWidening, volumeMultiplier)

    private val supportSnapshot = session.altSettings.map { alt ->
        alt.copy(sampleRates = alt.sampleRates.toList(), rateRanges = alt.rateRanges.toList())
    }

    override fun formatSupport(request: StreamRequest): OutputSupport =
        directSessionSupport(supportSnapshot, request, allowWidening)

    override fun open(request: StreamRequest): OpenedOutput {
        require(request.encoding in listOf(StreamEncoding.PCM, StreamEncoding.PCM_U8, StreamEncoding.PCM_FLOAT32)) { "Media3 integration requires integer PCM" }
        val exact = request.copy(ratePolicy = RatePolicy.EXACT)
        var original: DacException? = null
        val depths = listOf(request.bitDepth) + if (allowWidening && request.encoding == StreamEncoding.PCM) {
            session.pcmBitDepths.filter { it > request.bitDepth }.distinct().sorted()
        } else emptyList()
        for (depth in depths) {
            val negotiated = try {
                session.negotiate(exact.copy(bitDepth = depth))
            } catch (error: DacException) {
                if (error.code != DacErrorCode.UNSUPPORTED) throw error
                if (original == null) original = error
                continue
            }
            val wire = negotiated.format
            check(wire.encoding == request.encoding && wire.sampleRate == request.sampleRate && wire.counterRateHz == request.sampleRate &&
                wire.channels == request.channels && wire.validBits >= request.bitDepth &&
                wire.subslotBytes >= request.bitDepth / 8) { "negotiation is not lossless integer widening" }
            return OpenedOutput(negotiated.open(), wire, request)
        }
        throw checkNotNull(original)
    }

    override fun reconfigure(output: OpenedOutput, request: StreamRequest): OutputReconfiguration? {
        if (!session.reconfigurable(output.output)) return null
        val candidates = reconfigurationCandidates(supportSnapshot, request, allowWidening)
        if (candidates.isEmpty()) throw DacException(DacErrorCode.UNSUPPORTED, "no cached same-rate output alternate")
        // One native transaction for every candidate: the old stream is drained and retired once,
        // and exactly one replacement (or the restored old format) comes back.
        val report = session.reconfigureCandidates(output.output, candidates)
        val actualRequest = if (report.rollbackCause == null) request else output.request
        return OutputReconfiguration(OpenedOutput(report.output, report.format, actualRequest), report.rollbackCause)
    }

    override fun close(output: OpenedOutput) = output.output.close()
    override fun takeLookAgain(): Boolean = false
    override fun setVolumeMultiplier(multiplier: Float) = volumeMultiplier(multiplier)
}

/** A negotiated wire format and the call that opens a stream in it. */
internal class DirectWire(val format: WriteFormat, val open: () -> PcmOutput)

/** The stream a reconfiguration left in place: the requested format, or the old one restored. */
internal class DirectReplacement(val output: PcmOutput, val format: WriteFormat, val rollbackCause: DacException?)

/** The session calls [DacSessionOutputSource] makes, so a test can count them without a DAC. */
internal interface DirectSession {
    val altSettings: List<AltSetting>
    val pcmBitDepths: List<Int>
    fun negotiate(request: StreamRequest): DirectWire
    /** Whether [current] is this session's stream, which [reconfigureCandidates] can replace. */
    fun reconfigurable(current: PcmOutput): Boolean
    /** One transaction over [requests], for a [current] that is [reconfigurable]. */
    fun reconfigureCandidates(current: PcmOutput, requests: List<StreamRequest>): DirectReplacement
}

private class SessionDirect(private val session: DacSession) : DirectSession {
    override val altSettings: List<AltSetting> get() = session.capabilities.altSettings
    override val pcmBitDepths: List<Int> get() = session.capabilities.pcmBitDepths
    override fun negotiate(request: StreamRequest): DirectWire {
        val wire = session.negotiate(request)
        return DirectWire(wire.writeFormat()) { session.openStream(wire) }
    }
    override fun reconfigurable(current: PcmOutput): Boolean = current is DacStream
    override fun reconfigureCandidates(current: PcmOutput, requests: List<StreamRequest>): DirectReplacement {
        val report = session.reconfigureCandidates(current as DacStream, requests)
        return DirectReplacement(report.stream, report.stream.format.writeFormat(), report.rollbackCause)
    }
    private fun StreamFormat.writeFormat() = WriteFormat(sampleRate, channels, subslotBytes, validBits, counterRateHz, encoding)
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
