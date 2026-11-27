package dev.usbdac.sample.player

import androidx.media3.common.C
import androidx.media3.common.Format
import androidx.media3.common.MimeTypes
import androidx.media3.exoplayer.audio.AudioSink
import dev.usbdac.DacErrorCode
import dev.usbdac.DacException
import org.junit.Assert.assertArrayEquals
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertThrows
import org.junit.Assert.assertTrue
import org.junit.Test
import java.nio.ByteBuffer
import java.nio.ByteOrder

class UsbDacAudioSinkTest {
    private fun pcm(bits: Int, rate: Int = 44_100, channels: Int = 2): Format =
        Format.Builder()
            .setSampleMimeType(MimeTypes.AUDIO_RAW)
            .setPcmEncoding(
                when (bits) {
                    16 -> C.ENCODING_PCM_16BIT
                    24 -> C.ENCODING_PCM_24BIT
                    else -> C.ENCODING_PCM_32BIT
                },
            )
            .setSampleRate(rate)
            .setChannelCount(channels)
            .build()

    private fun audio(frames: Int, frameBytes: Int, fill: Int = 1): ByteBuffer =
        ByteBuffer.allocateDirect(frames * frameBytes).order(ByteOrder.LITTLE_ENDIAN).also {
            repeat(frames * frameBytes) { _ -> it.put(fill.toByte()) }
            it.flip()
        }

    private fun configured(source: FakeOutputSource, bits: Int = 16): UsbDacAudioSink =
        UsbDacAudioSink(source).also { it.configure(AudioSink.AudioSinkConfig.Builder(pcm(bits)).build()) }

    @Test
    fun widensTwentyFourBitsIntoAFourByteSlotWithTheLowByteZero() {
        val target = ByteBuffer.allocate(8)
        val source = ByteBuffer.wrap(byteArrayOf(0x11, 0x22, 0x33, 0x44, 0x55, 0x66))
        repack(source, inputBytes = 3, outputBytes = 4, samples = 2, target = target)
        assertArrayEquals(byteArrayOf(0, 0x11, 0x22, 0x33, 0, 0x44, 0x55, 0x66), target.array())
        assertEquals(0, source.position()) // the source is read in place, not consumed
    }

    @Test
    fun opensTheOutputOnTheFirstBufferAndWritesWhatItWasGiven() {
        val source = FakeOutputSource()
        val sink = configured(source)
        assertTrue(source.opened.isEmpty()) // configure() opens nothing
        sink.play()
        assertTrue(sink.handleBuffer(audio(100, 4), 0, 1))
        assertEquals(1, source.opened.size)
        assertEquals(400, source.opened[0].written.size())
    }

    @Test
    fun widensOnTheWayToAnOutputWithWiderSlots() {
        val source = FakeOutputSource(wireBytes = 4)
        val sink = configured(source, bits = 24)
        sink.play()
        assertTrue(sink.handleBuffer(audio(10, 6, fill = 7), 0, 1))
        val bytes = source.opened[0].written.toByteArray()
        assertEquals(80, bytes.size)
        assertEquals(0, bytes[0].toInt())
        assertEquals(7, bytes[1].toInt())
    }

    @Test
    fun theSameFormatKeepsTheOutputAcrossATrackChange() {
        val source = FakeOutputSource()
        val sink = configured(source)
        sink.play()
        sink.handleBuffer(audio(100, 4), 0, 1)
        sink.handleDiscontinuity()
        sink.configure(AudioSink.AudioSinkConfig.Builder(pcm(16)).build())
        assertTrue(sink.handleBuffer(audio(100, 4), 2_267, 1))
        assertEquals(1, source.opened.size)
        assertFalse(source.opened[0].calls.contains("drain"))
        assertFalse(source.opened[0].calls.contains("flush"))
    }

    @Test
    fun aNewFormatWaitsForTheOldOutputToPlayOutThenOpensAnother() {
        val source = FakeOutputSource()
        val sink = configured(source)
        sink.play()
        sink.handleBuffer(audio(100, 4), 0, 1)
        sink.configure(AudioSink.AudioSinkConfig.Builder(pcm(16, rate = 96_000)).build())

        val next = audio(100, 4)
        assertFalse(sink.handleBuffer(next, 0, 1)) // draining
        assertTrue(source.opened[0].calls.contains("drain"))
        assertEquals(1, source.opened.size)

        source.opened[0].drained = true
        assertTrue(sink.handleBuffer(next, 0, 1))
        assertTrue(source.opened[0].closed)
        assertEquals(2, source.opened.size)
    }

    @Test
    fun aSeekFlushesTheOutputAndTheNextBufferAnchorsThePosition() {
        val source = FakeOutputSource()
        val sink = configured(source)
        sink.play()
        sink.handleBuffer(audio(44_100, 4), 0, 1)
        source.opened[0].played = 22_050
        assertEquals(500_000, sink.getCurrentPositionUs(false))

        sink.flush()
        assertTrue(source.opened[0].calls.contains("flush"))
        assertEquals(AudioSink.CURRENT_POSITION_NOT_SET, sink.getCurrentPositionUs(false))

        sink.handleBuffer(audio(44_100, 4), 30_000_000, 1)
        source.opened[0].played = 44_100
        assertEquals(31_000_000, sink.getCurrentPositionUs(false))
    }

    @Test
    fun anOutputOpenedWhilePausedIsPausedUntilPlay() {
        val source = FakeOutputSource()
        val sink = configured(source)
        sink.handleBuffer(audio(100, 4), 0, 1)
        assertTrue(source.opened[0].calls.contains("pause"))
        sink.play()
        assertEquals("play", source.opened[0].calls.last())
    }

    @Test
    fun aLostDacIsAnErrorWhilePlayingAndAWaitWhilePausedAndPlayOpensANewOutput() {
        val source = FakeOutputSource()
        val sink = configured(source)
        sink.play()
        val buffer = audio(100, 4)
        sink.handleBuffer(audio(100, 4), 0, 1)
        // The DAC is unplugged.
        val lost = DacException(DacErrorCode.DEVICE_LOST, "unplugged")
        source.failure(0, lost)

        sink.pause()
        assertFalse(sink.handleBuffer(buffer, 2_267, 1)) // paused: wait, do not fail

        sink.play() // the listener presses play
        assertTrue(source.opened[0].closed)
        assertTrue(sink.handleBuffer(buffer, 2_267, 1))
        assertEquals(2, source.opened.size)
    }

    @Test
    fun aFailureWhilePlayingIsAWriteException() {
        val source = FakeOutputSource()
        val sink = configured(source)
        sink.play()
        sink.handleBuffer(audio(100, 4), 0, 1)
        source.failure(0, DacException(DacErrorCode.DEVICE_LOST, "unplugged"))
        assertThrows(AudioSink.WriteException::class.java) { sink.handleBuffer(audio(100, 4), 2_267, 1) }
    }

    @Test
    fun aFullOutputTakesWhatItCanAndTheRestIsOfferedAgain() {
        val source = FakeOutputSource(room = 40) // ten frames of 16-bit stereo
        val sink = configured(source)
        sink.play()
        val buffer = audio(100, 4)
        assertFalse(sink.handleBuffer(buffer, 0, 1))
        assertEquals(360, buffer.remaining())
        assertEquals(40, source.opened[0].written.size())
    }

    @Test
    fun duckingBecomesTheVolumeMultiplier() {
        val source = FakeOutputSource()
        val sink = configured(source)
        sink.setVolume(0.2f)
        assertEquals(0.2f, source.multiplier, 0f)
    }

    @Test
    fun declinesWhatItCannotPlayExactly() {
        val sink = UsbDacAudioSink(FakeOutputSource())
        val float = Format.Builder().setSampleMimeType(MimeTypes.AUDIO_RAW).setPcmEncoding(C.ENCODING_PCM_FLOAT)
            .setSampleRate(44_100).setChannelCount(2).build()
        assertEquals(AudioSink.SINK_FORMAT_UNSUPPORTED, sink.getFormatSupport(float))
        assertEquals(AudioSink.SINK_FORMAT_UNSUPPORTED, sink.getFormatSupport(pcm(16, channels = 6)))
        assertEquals(AudioSink.SINK_FORMAT_SUPPORTED_DIRECTLY, sink.getFormatSupport(pcm(24)))
    }
}
