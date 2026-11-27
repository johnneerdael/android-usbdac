package dev.usbdac.media3

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
        assertEquals(AudioSink.SINK_FORMAT_SUPPORTED_DIRECTLY, sink.getFormatSupport(float))
        assertEquals(AudioSink.SINK_FORMAT_UNSUPPORTED, sink.getFormatSupport(pcm(16, channels = 6)))
        assertEquals(AudioSink.SINK_FORMAT_SUPPORTED_DIRECTLY, sink.getFormatSupport(pcm(24)))
    }
    @Test
    fun trimsDelayAndPaddingWithoutChangingIntegerPrecisionAcrossGaplessTracks() {
        for (bits in listOf(16, 24, 32)) {
            val source = FakeOutputSource()
            val sink = UsbDacAudioSink(source)
            val format = pcm(bits).buildUpon().setEncoderDelay(2).setEncoderPadding(3).build()
            val frameBytes = bits / 8 * 2
            sink.configure(AudioSink.AudioSinkConfig.Builder(format).build())
            sink.play()
            val expected = java.io.ByteArrayOutputStream()
            repeat(2) { track ->
                if (track > 0) sink.configure(AudioSink.AudioSinkConfig.Builder(format).build())
                val bytes = ByteArray(frameBytes * 12) { (it + track * 43).toByte() }
                expected.write(bytes, frameBytes * 2, frameBytes * 7)
                // Split the leading trim and trailing padding across decoder buffers.
                for (range in listOf(0 to 1, 1 to 10, 10 to 12)) {
                    val input = ByteBuffer.allocateDirect((range.second - range.first) * frameBytes)
                    input.put(bytes, range.first * frameBytes, input.capacity()).flip()
                    while (!sink.handleBuffer(input, 0, 1)) { /* output always has room */ }
                }
            }
            assertArrayEquals(expected.toByteArray(), source.opened.single().written.toByteArray())
            sink.playToEndOfStream()
            assertFalse(sink.isEnded())
            source.opened.single().drained = true
            assertTrue(sink.isEnded())
        }
    }

    @Test
    fun paddingBackpressureConsumesEachByteOnceAndSeekDoesNotRepeatEncoderDelay() {
        val source = FakeOutputSource(room = 4)
        val sink = UsbDacAudioSink(source)
        val format = pcm(16).buildUpon().setEncoderDelay(1).setEncoderPadding(2).build()
        sink.configure(AudioSink.AudioSinkConfig.Builder(format).build())
        sink.play()
        val buffer = audio(10, 4, fill = 7)
        var retries = 0
        while (!sink.handleBuffer(buffer, 0, 1)) check(++retries < 20)
        assertEquals(7 * 4, source.opened.single().written.size())
        sink.flush()
        val afterSeek = audio(5, 4, fill = 9)
        while (!sink.handleBuffer(afterSeek, 5_000_000, 1)) check(++retries < 40)
        assertEquals(3 * 4, source.opened.single().written.size())
        source.opened.single().played = 2
        assertEquals(5_000_045, sink.getCurrentPositionUs(false))
    }

    @Test
    fun rawPcmBelowOneFrameIsRejectedRatherThanSilentlyDropped() {
        val sink = configured(FakeOutputSource())
        assertThrows(IllegalArgumentException::class.java) {
            sink.handleBuffer(ByteBuffer.allocateDirect(3), 0, 1)
        }
    }

    @Test
    fun pinnedRendererStreamChangeSequenceKeepsOneStreamAndRendererTimePosition() {
        val source = FakeOutputSource()
        val sink = UsbDacAudioSink(source)
        val format = pcm(24, rate = 48_000).buildUpon().setEncoderDelay(2).setEncoderPadding(3).build()
        // MediaCodecRenderer.setOutputStreamInfo -> renderer forwards offset, then configures raw output.
        sink.setOutputStreamOffsetUs(1_000_000)
        sink.configure(AudioSink.AudioSinkConfig.Builder(format).build())
        sink.play()
        assertTrue(sink.handleBuffer(audio(12, 6, 1), 1_000_000, 1))
        // Last old-stream buffer completes -> processed stream change -> first reused-codec format.
        sink.setOutputStreamOffsetUs(1_000_145)
        sink.handleDiscontinuity()
        sink.configure(AudioSink.AudioSinkConfig.Builder(format).build())
        assertTrue(sink.handleBuffer(audio(12, 6, 2), 1_000_145, 1))
        val out = source.opened.single()
        assertEquals(14 * 6, out.written.size())
        out.played = 14
        assertEquals(1_000_291, sink.getCurrentPositionUs(false))
        assertFalse(out.calls.contains("flush"))
        assertFalse(out.calls.contains("drain"))
    }

    @Test
    fun unsigned8AndFloat32BitsRemainExactThroughTrimAndBackpressure() {
        for (encoding in listOf(C.ENCODING_PCM_8BIT, C.ENCODING_PCM_FLOAT)) {
            val width = if (encoding == C.ENCODING_PCM_8BIT) 1 else 4
            val fake = FakeOutputSource(room = width * 2)
            val source = object : OutputSource by fake {
                override fun open(request: dev.usbdac.StreamRequest): OpenedOutput {
                    val original = fake.open(request)
                    return OpenedOutput(original.output, original.format.copy(encoding = request.encoding), request)
                }
            }
            val sink = UsbDacAudioSink(source)
            val format = pcm(32).buildUpon().setPcmEncoding(encoding).setEncoderDelay(1).setEncoderPadding(1).build()
            sink.configure(AudioSink.AudioSinkConfig.Builder(format).build())
            sink.play()
            val bytes = if (width == 1) byteArrayOf(0x80.toByte(), 0, 0x7f, 0xff.toByte(), 1, 2, 3, 4)
                else byteArrayOf(0, 0, 0, 0x80.toByte(), 0, 0, 0, 0,
                    0x45, 0x23, 0xc1.toByte(), 0x7f, 1, 0, 0, 0,
                    0, 0, 0x80.toByte(), 0x7f, 0, 0, 0x80.toByte(), 0xff.toByte(),
                    0x45, 0x23, 0xa1.toByte(), 0x7f, 0, 0, 0, 0)
            val input = ByteBuffer.allocateDirect(bytes.size).put(bytes).apply { flip() }
            var retries = 0
            while (!sink.handleBuffer(input, 0, 1)) check(++retries < 30)
            assertArrayEquals(bytes.copyOfRange(width * 2, bytes.size - width * 2), fake.opened.single().written.toByteArray())
        }
    }

    @Test
    fun drainedFormatChangeUsesTransactionalSourceAndResetsPosition() {
        val fake = FakeOutputSource()
        var reconfigured = 0
        val source = object : OutputSource by fake {
            override fun reconfigure(output: OpenedOutput, request: dev.usbdac.StreamRequest): OutputReconfiguration {
                assertTrue(output.output.isDrained)
                reconfigured++
                output.output.close() // native transaction retires only the old stream
                return OutputReconfiguration(fake.open(request))
            }
        }
        val sink = UsbDacAudioSink(source)
        sink.configure(AudioSink.AudioSinkConfig.Builder(pcm(16)).build())
        sink.play(); sink.handleBuffer(audio(4, 4), 0, 1)
        sink.configure(AudioSink.AudioSinkConfig.Builder(pcm(24, 96_000)).build())
        val next = audio(4, 6)
        assertFalse(sink.handleBuffer(next, 1_000_000, 1))
        fake.opened.first().drained = true
        assertTrue(sink.handleBuffer(next, 1_000_000, 1))
        assertEquals(1, reconfigured)
        assertTrue(fake.opened.first().closed)
        fake.opened.last().played = 4
        assertEquals(1_000_041, sink.getCurrentPositionUs(false))
    }

    @Test
    fun rolledBackFormatRemainsOwnedAndNewFormatPcmIsNotWritten() {
        val fake = FakeOutputSource()
        val source = object : OutputSource by fake {
            override fun reconfigure(output: OpenedOutput, request: dev.usbdac.StreamRequest): OutputReconfiguration {
                output.output.close()
                val restored = fake.open(output.request)
                restored.output.pause()
                return OutputReconfiguration(restored, DacException(DacErrorCode.UNSUPPORTED, "no new alternate"))
            }
        }
        val sink = UsbDacAudioSink(source)
        sink.configure(AudioSink.AudioSinkConfig.Builder(pcm(16)).build())
        sink.play(); sink.handleBuffer(audio(4, 4), 0, 1)
        fake.opened.first().drained = true
        sink.configure(AudioSink.AudioSinkConfig.Builder(pcm(32, 96_000)).build())
        val next = audio(4, 8)
        assertThrows(AudioSink.InitializationException::class.java) { sink.handleBuffer(next, 1_000_000, 1) }
        assertEquals(0, next.position())
        assertEquals(0, fake.opened.last().written.size())
        assertEquals("pause", fake.opened.last().calls.last())
        sink.reset()
        assertTrue(fake.opened.last().closed)
    }

}
