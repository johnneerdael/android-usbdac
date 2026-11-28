package dev.usbdac

import org.junit.Assert.*
import org.junit.Test

class DsdDopOutputTest {
    @Test(expected = IllegalArgumentException::class) fun refusesUnconfirmedDop() {
        DsdDopOutput(RecordingOutput(), 2, 3, false)
    }
    @Test fun packsChronologicalBytesAndAlternatesMarkers() {
        val target = RecordingOutput()
        val output = DsdDopOutput(target, 2, 3, true)
        val source = heapOf(0x12, 0x34, 0x56, 0x78, 0x9a, 0xbc, 0xde, 0xf0)
        assertEquals(8, output.write(source))
        assertEquals(listOf(0x56, 0x12, 0x05, 0x78, 0x34, 0x05, 0xde, 0x9a, 0xfa, 0xf0, 0xbc, 0xfa), target.received.map { it.toInt() and 255 })
        output.flush()
        assertEquals(4, output.write(heapOf(1, 2, 3, 4)))
        assertEquals(5, target.received[target.received.size - 1].toInt())
    }
    @Test fun boundedBackpressureRetainsWholeFramesAndPartialInput() {
        val target = RecordingOutput().apply { room = 0 }
        val output = DsdDopOutput(target, 2, 4, true)
        val source = heapOf(1, 2, 3, 4, 5, 6)
        assertEquals(6, output.write(source))
        assertEquals(0, source.remaining())
        assertEquals(0, output.writableBytes)
        target.room = 8
        assertEquals(0, output.write(source))
        assertEquals(listOf(0,3,1,5,0,4,2,5), target.received.map { it.toInt() and 255 })
    }
    @Test fun oneByteFrameSurvivesDisjointWrites() {
        val target = RecordingOutput()
        val output = DsdDopOutput(target, 2, 3, true)
        assertEquals(2, output.write(heapOf(1, 2)))
        assertTrue(target.received.isEmpty())
        assertEquals(2, output.write(heapOf(3, 4)))
        assertEquals(listOf(3,1,5,4,2,5), target.received.map { it.toInt() and 255 })
        output.drain()
        assertEquals(6, target.received.size)
        assertTrue(target.calls.contains("drain"))
    }

    // DoP v1.1 section 2: 16 DSD bits per channel under a marker that alternates 0x05/0xFA per
    // frame. A held older byte-frame is completed with ALSA's DSD silence byte 0x69
    // (sound/core/pcm_misc.c) before framing, so the padded frame takes the next marker.
    @Test fun drainPadsAnIncompleteFinalPairWithDsdIdleUnderTheNextMarker() {
        for (slot in listOf(3, 4)) {
            val target = RecordingOutput()
            val output = DsdDopOutput(target, 2, slot, true)
            assertEquals(6, output.write(heapOf(1, 2, 3, 4, 5, 6)))
            assertEquals(1L, output.latencyFrames)
            output.drain()
            assertTrue(target.calls.contains("drain"))
            val zero = if (slot == 4) listOf(0) else emptyList()
            val expected = zero + listOf(3, 1, 0x05) + zero + listOf(4, 2, 0x05) +
                zero + listOf(0x69, 5, 0xfa) + zero + listOf(0x69, 6, 0xfa)
            assertEquals(expected, target.received.map { it.toInt() and 255 })
            output.flush()
            assertEquals(2, output.write(heapOf(7, 8)))
            output.drain()
            assertEquals(zero + listOf(0x69, 7, 0x05) + zero + listOf(0x69, 8, 0x05),
                target.received.drop(expected.size).map { it.toInt() and 255 })
        }
    }

    @Test fun drainPadAppendsBehindFramesTheTargetHasNotTaken() {
        val target = RecordingOutput().apply { room = 6 }
        val output = DsdDopOutput(target, 2, 3, true)
        assertEquals(10, output.write(heapOf(1, 2, 3, 4, 5, 6, 7, 8, 9, 10)))
        output.drain()
        assertFalse(target.calls.contains("drain"))
        assertEquals(4L, output.latencyFrames)
        target.room = 12
        assertFalse(output.isDrained)
        assertTrue(target.calls.contains("drain"))
        assertEquals(listOf(3,1,5,4,2,5, 7,5,0xfa,8,6,0xfa, 0x69,9,5,0x69,10,5), target.received.map { it.toInt() and 255 })
        target.drained = true
        assertTrue(output.isDrained)
    }

    @Test fun drainAfterTargetFailureRethrowsTheLatchedFailure() {
        val target = FailsAfterPartialWrite(RecordingOutput(), acceptBytes = 3)
        val output = DsdDopOutput(target, 2, 3, true)
        assertEquals(6, output.write(heapOf(1, 2, 3, 4, 5, 6)))
        assertSame(target.failure, assertThrows(DacException::class.java) { output.drain() })
        assertSame(target.failure, assertThrows(DacException::class.java) { output.drain() })
        assertEquals(1, target.attempts)
        assertFalse(target.recorded.calls.contains("drain"))
    }

    @Test fun longWritesUseBoundedBatches() {
        val target = RecordingOutput()
        val output = DsdDopOutput(target, 2, 3, true)
        val source = java.nio.ByteBuffer.wrap(ByteArray(40_000) { it.toByte() })
        while (source.hasRemaining()) assertTrue(output.write(source) > 0)
        assertTrue(target.calls.count { it == "write" } <= 2)
        assertEquals(60_000, target.received.size)
    }

    @Test fun shortTargetWritesRetainBatchAcrossDrainAndFlush() {
        val target = RecordingOutput().apply { room = 6 }
        val output = DsdDopOutput(target, 2, 3, true)
        assertEquals(8, output.write(heapOf(1, 2, 3, 4, 5, 6, 7, 8)))
        assertEquals(2L, output.latencyFrames)
        output.drain()
        assertFalse(target.calls.contains("drain"))
        target.room = 6
        assertFalse(output.isDrained)
        assertTrue(target.calls.contains("drain"))
        assertEquals(listOf(3,1,5,4,2,5,7,5,250,8,6,250), target.received.map { it.toInt() and 255 })
        output.flush()
        target.room = 6
        assertEquals(4, output.write(heapOf(9,10,11,12)))
        assertEquals(5, target.received.last().toInt())
    }

    // PcmOutput.write: the return value equals the position delta, and Media3's
    // AudioSink.handleBuffer must report consumed bytes, so a target failure after the input
    // advanced is returned as the accepted count and rethrown on the next call.
    @Test fun targetFailureAfterAcceptingInputReturnsTheCountAndRethrowsLater() {
        val target = FailsAfterPartialWrite(RecordingOutput(), acceptBytes = 3)
        val output = DsdDopOutput(target, 2, 3, true)
        val input = heapOf(0x12, 0x34, 0x56, 0x78, 0x9a, 0xbc)
        assertEquals(6, output.write(input))
        assertEquals(6, input.position())
        assertEquals(listOf(0x56, 0x12, 0x05), target.recorded.received.map { it.toInt() and 255 })
        assertSame(target.failure, assertThrows(DacException::class.java) { output.write(heapOf(1, 2)) })
        assertSame(target.failure, assertThrows(DacException::class.java) { output.writableBytes })
        assertSame(target.failure, assertThrows(DacException::class.java) { output.flush() })
        assertSame(target.failure, assertThrows(DacException::class.java) { output.play() })
        assertEquals(1, target.attempts)
        output.close()
        assertTrue(target.recorded.closed)
    }
}
