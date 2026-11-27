package dev.usbdac.media3

import dev.usbdac.*

/**
 * Writes to an already-open session. Opening [DacSession] discovers and initializes its hardware
 * volume without opening a stream or programming its clock. The host retains session ownership:
 * closing a track output closes only its stream, so volume remains usable between tracks.
 *
 * Only equal-rate/equal-channel integer PCM is accepted. [allowWidening] permits a wider integer
 * depth and zero-padded slots; the sink preserves every input bit. No resampling or narrowing occurs.
 * DAC ducking is deliberately host-owned via [volumeMultiplier], so the host can restore its level.
 */
class DacSessionOutputSource(
    private val session: DacSession,
    private val allowWidening: Boolean = true,
    private val volumeMultiplier: (Float) -> Unit = {},
) : OutputSource {
    override fun open(request: StreamRequest): OpenedOutput {
        require(request.encoding == StreamEncoding.PCM) { "Media3 integration requires integer PCM" }
        val exact = request.copy(ratePolicy = RatePolicy.EXACT)
        var original: DacException? = null
        val depths = listOf(request.bitDepth) + if (allowWidening) {
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
            check(wire.encoding == StreamEncoding.PCM && wire.sampleRate == request.sampleRate && wire.counterRateHz == request.sampleRate &&
                wire.channels == request.channels && wire.validBits >= request.bitDepth &&
                wire.subslotBytes >= request.bitDepth / 8) { "negotiation is not lossless integer widening" }
            val stream = session.openStream(wire)
            return OpenedOutput(stream, WriteFormat(wire.sampleRate, wire.channels, wire.subslotBytes,
                wire.validBits, wire.counterRateHz), request)
        }
        throw checkNotNull(original)
    }

    override fun close(output: OpenedOutput) = output.output.close()
    override fun takeLookAgain(): Boolean = false
    override fun setVolumeMultiplier(multiplier: Float) = volumeMultiplier(multiplier)
}
