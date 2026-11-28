package dev.usbdac.media3

import androidx.media3.common.C
import androidx.media3.common.Format
import androidx.media3.common.MimeTypes
import androidx.media3.exoplayer.audio.AudioSink
import dev.usbdac.*
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertSame
import org.junit.Assert.assertThrows
import org.junit.Assert.assertTrue
import org.junit.Test
import java.nio.ByteBuffer
import java.nio.ByteOrder

/**
 * Counts what one Media3 format change costs through the production [DacSessionOutputSource]: the
 * old stream is stopped (drained) once, one reconfiguration transaction carries every alternate
 * candidate, and exactly one replacement is published. USB alternate-setting writes happen inside
 * that native transaction and are not visible here.
 */
class DacSessionOutputSourceTest {
    private fun alt(bits: Int, slot: Int, rate: Int) = AltSetting(1, bits / 8, 1, AltFormat.PCM, 2, slot, bits, emptyList(),
        listOf(rate), SyncType.ASYNCHRONOUS, 0x81, 1024, 125, UacVersion.V2, Pacing.ExplicitFeedback(0x81))

    private class CountingSession(
        override val altSettings: List<AltSetting>,
        var reconfigureOutcome: (List<StreamRequest>, OpenedRequest) -> DirectReplacement,
    ) : DirectSession {
        class OpenedRequest(val output: FakePcmOutput, val request: StreamRequest)
        val negotiated = mutableListOf<StreamRequest>()
        val opened = mutableListOf<OpenedRequest>()
        val transactions = mutableListOf<List<StreamRequest>>()
        val replacements = mutableListOf<FakePcmOutput>()
        override val pcmBitDepths: List<Int> get() = altSettings.map { it.validBits }.distinct().sorted()

        override fun negotiate(request: StreamRequest): DirectWire {
            negotiated += request
            val alt = altSettings.firstOrNull { it.validBits == request.bitDepth && request.sampleRate in it.sampleRates }
                ?: throw DacException(DacErrorCode.UNSUPPORTED, "no alternate")
            val format = WriteFormat(request.sampleRate, request.channels, alt.subslotBytes, alt.validBits, request.sampleRate)
            return DirectWire(format) {
                FakePcmOutput(format.subslotBytes * format.channels).also { opened += OpenedRequest(it, request) }
            }
        }

        override fun reconfigurable(current: PcmOutput): Boolean = opened.any { it.output === current } || current in replacements

        override fun reconfigureCandidates(current: PcmOutput, requests: List<StreamRequest>): DirectReplacement {
            transactions += requests
            val old = opened.first { it.output === current }
            return reconfigureOutcome(requests, old).also { replacements += it.output as FakePcmOutput }
        }
    }

    private fun pcm(bits: Int, rate: Int): Format = Format.Builder()
        .setSampleMimeType(MimeTypes.AUDIO_RAW)
        .setPcmEncoding(if (bits == 16) C.ENCODING_PCM_16BIT else C.ENCODING_PCM_24BIT)
        .setSampleRate(rate)
        .setChannelCount(2)
        .build()

    private fun audio(frames: Int, frameBytes: Int): ByteBuffer =
        ByteBuffer.allocateDirect(frames * frameBytes).order(ByteOrder.LITTLE_ENDIAN).also {
            repeat(frames * frameBytes) { _ -> it.put(1) }
            it.flip()
        }

    private val alts = listOf(alt(16, 2, 44_100), alt(24, 3, 96_000), alt(32, 4, 96_000))

    private fun replacement(request: StreamRequest, slot: Int, cause: DacException? = null): DirectReplacement {
        val format = WriteFormat(request.sampleRate, request.channels, slot, request.bitDepth, request.sampleRate)
        return DirectReplacement(FakePcmOutput(slot * request.channels), format, cause)
    }

    private fun startedSink(session: CountingSession): Pair<UsbDacAudioSink, FakePcmOutput> {
        val sink = UsbDacAudioSink(DacSessionOutputSource(session, allowWidening = true, volumeMultiplier = {}))
        sink.configure(AudioSink.AudioSinkConfig.Builder(pcm(16, 44_100)).build())
        sink.play()
        assertTrue(sink.handleBuffer(audio(4, 4), 0, 1))
        return sink to session.opened.single().output
    }

    @Test
    fun formatChangeStopsOnceUsesOneTransactionAndPublishesOneReplacement() {
        val session = CountingSession(alts) { requests, _ -> replacement(requests.first(), 3) }
        val (sink, old) = startedSink(session)
        assertEquals(1, session.negotiated.size)

        sink.configure(AudioSink.AudioSinkConfig.Builder(pcm(24, 96_000)).build())
        val next = audio(4, 6)
        assertFalse(sink.handleBuffer(next, 1_000_000, 1))
        assertFalse(sink.handleBuffer(next, 1_000_000, 1))
        assertEquals(0, session.transactions.size) // no transaction while the old stream plays out
        old.drained = true
        assertTrue(sink.handleBuffer(next, 1_000_000, 1))

        assertEquals(1, old.calls.count { it == "drain" })
        assertEquals(0, old.calls.count { it == "close" }) // the native transaction retires it
        assertEquals(1, session.transactions.size)
        assertEquals(listOf(24, 32), session.transactions.single().map { it.bitDepth })
        assertTrue(session.transactions.single().all { it.sampleRate == 96_000 && it.ratePolicy == RatePolicy.EXACT })
        assertEquals(1, session.negotiated.size) // no second negotiate
        assertEquals(1, session.opened.size) // no separate stream open for the replacement
        val published = session.replacements.single()
        assertEquals(24, published.written.size())
        assertEquals("play", published.calls.first())

        assertTrue(sink.handleBuffer(audio(2, 6), 1_000_100, 1))
        assertEquals(1, session.transactions.size)
        sink.reset()
        assertEquals(1, published.calls.count { it == "close" })
        assertEquals(0, old.calls.count { it == "close" })
    }

    @Test
    fun rollbackPublishesTheRestoredOldFormatOnceAndKeepsItPaused() {
        val cause = DacException(DacErrorCode.UNSUPPORTED, "no 96 kHz alternate answered")
        val session = CountingSession(alts) { _, old -> replacement(old.request, 2, cause) }
        val (sink, old) = startedSink(session)
        sink.configure(AudioSink.AudioSinkConfig.Builder(pcm(24, 96_000)).build())
        old.drained = true
        val next = audio(4, 6)
        val error = assertThrows(AudioSink.InitializationException::class.java) { sink.handleBuffer(next, 1_000_000, 1) }
        assertSame(cause, error.cause)
        assertEquals(0, next.position())

        assertEquals(1, old.calls.count { it == "drain" })
        assertEquals(1, session.transactions.size)
        assertEquals(1, session.opened.size)
        val restored = session.replacements.single()
        assertEquals(0, restored.written.size())
        assertFalse(restored.calls.contains("play"))
        sink.reset()
        assertEquals(1, restored.calls.count { it == "close" })
        assertEquals(0, old.calls.count { it == "close" })
    }

    @Test
    fun unusableOutcomePublishesNothingAndOpensNothing() {
        val unusable = DacException(DacErrorCode.RECONFIGURE_UNUSABLE, "rollback failed")
        val session = CountingSession(alts) { _, _ -> throw unusable }
        val (sink, old) = startedSink(session)
        sink.configure(AudioSink.AudioSinkConfig.Builder(pcm(24, 96_000)).build())
        old.drained = true
        val error = assertThrows(AudioSink.InitializationException::class.java) { sink.handleBuffer(audio(4, 6), 1_000_000, 1) }
        assertSame(unusable, error.cause)
        assertEquals(1, old.calls.count { it == "drain" })
        assertEquals(1, session.transactions.size)
        assertTrue(session.replacements.isEmpty())
        assertEquals(1, session.opened.size)
        assertEquals(1, session.negotiated.size)
    }
}
