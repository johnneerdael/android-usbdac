package dev.usbdac

import dev.usbdac.internal.DirectLink
import dev.usbdac.internal.OutputPlatform
import org.junit.Assert.*
import org.junit.Test

/** The Android selector must honor the driver's decision instead of ranking USB targets again. */
class DriverPlaybackSelectorTest {
    @Test fun monoDecisionUsesSourceFramesAndKeepsTheDriverTarget() {
        val fake = FakePlatform()
        val dac = fakeDac()
        val link = fake.attach(dac)
        fake.planKind = "lossless"
        var resolved = 0
        val platform = object : OutputPlatform by fake {
            override fun openDac(dac: DacDeviceInfo, config: DacConfig): DirectLink = object : DirectLink by link {
                override val supportsDriverPlayback = true
                override fun resolvePlayback(request: StreamRequest, policy: OutputPolicy, bypassLimits: Boolean): DriverPlaybackDecision {
                    resolved++
                    assertEquals(1, request.channels)
                    assertTrue(policy.allowConversion)
                    return DriverPlaybackDecision(fakeFormat(48_000, 16, 2, 2), "pcm", "lossless", 2, "unsupported")
                }
                override fun negotiate(request: StreamRequest): StreamFormat = error("Kotlin must not rerank driver candidates")
                override fun negotiateCandidates(requests: List<StreamRequest>): StreamFormat = error("Kotlin must not rerank driver candidates")
            }
        }
        val selected = OutputSelector(platform, OutputPolicy(force = OutputKind.DIRECT_USB)).open(StreamRequest.pcm(44_100, 1, 16))
        assertEquals(1, resolved)
        assertEquals(1, selected.format.channels)
        assertEquals(44_100, selected.format.counterRateHz)
        assertEquals(2, selected.format.bytesPerFrame)
        assertEquals(48_000, selected.decision.wireFormat?.sampleRate)
        assertEquals(2, selected.decision.wireFormat?.channels)
        assertFalse(selected.bitPerfect.value.isVerified)
        selected.close()
    }
    @Test fun nativeDsdAccumulatesByteFramesUntilTheWireWordIsComplete() {
        val recording = RecordingOutput()
        val wire = object : PcmOutput by recording {
            override fun write(buffer: java.nio.ByteBuffer): Int {
                val complete = buffer.remaining() / 8 * 8
                if (complete == 0) return 0
                val view = buffer.duplicate().apply { limit(position() + complete) }
                val count = recording.write(view)
                buffer.position(buffer.position() + count)
                return count
            }
        }
        val output = NativeDsdInputOutput(wire, channels = 2, wordBytes = 4)
        val first = java.nio.ByteBuffer.wrap(byteArrayOf(1, 2))
        assertEquals(2, output.write(first))
        assertEquals(0, recording.received.size)
        assertEquals(1L, output.latencyFrames)
        val rest = java.nio.ByteBuffer.wrap(byteArrayOf(3, 4, 5, 6, 7, 8))
        assertEquals(6, output.write(rest))
        assertArrayEquals(byteArrayOf(1, 2, 3, 4, 5, 6, 7, 8), recording.received.toByteArray())
        output.close()
    }
    @Test fun nativeDsdFailureKeepsStagedBytesAndReturnsSourceProgress() {
        val recorded = RecordingOutput()
        val failure = DacException(DacErrorCode.DEVICE_LOST, "target failed")
        var attempts = 0
        val wire = object : PcmOutput by recorded {
            override fun write(buffer: java.nio.ByteBuffer): Int {
                if (!buffer.hasRemaining()) return 0
                attempts++
                val limit = buffer.limit()
                buffer.limit(buffer.position() + 8)
                try { recorded.write(buffer) } finally { buffer.limit(limit) }
                throw failure
            }
        }
        val output = NativeDsdInputOutput(wire, channels = 2, wordBytes = 4)
        val input = java.nio.ByteBuffer.wrap(byteArrayOf(1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11))
        assertEquals(10, output.write(input))
        assertEquals(10, input.position())
        assertEquals(1, input.remaining())
        assertArrayEquals(byteArrayOf(1, 2, 3, 4, 5, 6, 7, 8), recorded.received.toByteArray())
        assertEquals(1L, output.latencyFrames)
        assertEquals(0L, output.playedFrames)
        assertSame(failure, assertThrows(DacException::class.java) { output.write(input) })
        assertSame(failure, assertThrows(DacException::class.java) { output.flush() })
        assertEquals(1, attempts)
        output.close()
        assertTrue(recorded.closed)
    }

    // PcmOutput.writableBytes is "the room up to the target depth"; the staged partial wire word
    // already uses part of the target's room, as in the other driver adapters.
    @Test fun nativeDsdWritableBytesFollowsTargetRoom() {
        val recording = RecordingOutput()
        val wire = object : PcmOutput by recording {
            override fun write(buffer: java.nio.ByteBuffer): Int {
                val complete = minOf(buffer.remaining(), recording.writable) / 8 * 8
                if (complete == 0) return 0
                val view = buffer.duplicate().apply { limit(position() + complete) }
                val count = recording.write(view)
                buffer.position(buffer.position() + count)
                recording.writable -= count
                return count
            }
        }
        val output = NativeDsdInputOutput(wire, channels = 2, wordBytes = 4)
        recording.writable = 16
        assertEquals(16, output.writableBytes)
        assertEquals(2, output.write(java.nio.ByteBuffer.wrap(byteArrayOf(1, 2))))
        assertEquals(14, output.writableBytes)
        recording.writable = 0
        assertEquals(0, output.writableBytes)
        assertEquals(6, output.write(java.nio.ByteBuffer.wrap(byteArrayOf(3, 4, 5, 6, 7, 8))))
        assertEquals(0, output.writableBytes)
        recording.writable = 8
        assertEquals(0, output.writableBytes)
        assertArrayEquals(byteArrayOf(1, 2, 3, 4, 5, 6, 7, 8), recording.received.toByteArray())
        recording.writable = 100_000
        assertEquals(64 * 1024, output.writableBytes)
        output.close()
    }
}
