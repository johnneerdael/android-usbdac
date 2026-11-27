package dev.usbdac

import org.junit.Assert.*
import org.junit.Test

class DopInputOutputTest {
    @Test fun unpacksStereoAndCountsCarrierFrames() {
        val target = RecordingOutput().apply { played = 8; latency = 6 }
        val output = DopInputOutput(target, 2, 3)
        val input = heapOf(3,1,5,4,2,5, 7,5,0xfa,8,6,0xfa)
        assertEquals(12, output.write(input))
        assertEquals(listOf(1,2,3,4,5,6,7,8), target.received.map { it.toInt() and 255 })
        assertEquals(4L, output.playedFrames); assertEquals(3L, output.latencyFrames)
    }
    @Test fun validatesMarkersAndPaddingBeforeAnyConsumption() {
        val target = RecordingOutput(); val output = DopInputOutput(target, 2, 4)
        val bad = heapOf(0,3,1,5,0,4,2,5, 0,7,5,5,0,8,6,5)
        assertThrows(DacException::class.java) { output.write(bad) }
        assertEquals(0, bad.position()); assertTrue(target.received.isEmpty())
        val wrongChannels = heapOf(0,3,1,5,0,4,2,0xfa)
        assertThrows(DacException::class.java) { output.write(wrongChannels) }
        val wrongPadding = heapOf(1,3,1,5,0,4,2,5)
        assertThrows(DacException::class.java) { output.write(wrongPadding) }
        val good = heapOf(0,3,1,5,0,4,2,5)
        assertEquals(8, output.write(good)); output.flush()
        assertEquals(8, output.write(good.apply { rewind() }))
    }
    @Test fun longCarrierInputUsesBatchesAndPreservesMarkers() {
        val target = RecordingOutput()
        val output = DopInputOutput(target, 2, 3)
        val source = java.nio.ByteBuffer.allocate(60_000)
        repeat(10_000) { frame ->
            val marker = if (frame % 2 == 0) 5 else 0xfa
            source.put(3).put(1).put(marker.toByte()).put(4).put(2).put(marker.toByte())
        }
        source.flip()
        while (source.hasRemaining()) assertTrue(output.write(source) > 0)
        assertTrue(target.calls.count { it == "write" } <= 2)
        assertEquals(40_000, target.received.size)
        assertEquals(listOf(1,2,3,4), target.received.take(4).map { it.toInt() and 255 })
    }

    @Test fun shortWritesRetainUnpackedFramesUntilDrain() {
        val target = RecordingOutput().apply { room = 4 }
        val output = DopInputOutput(target, 2, 3)
        assertEquals(12, output.write(heapOf(3,1,5,4,2,5, 7,5,250,8,6,250)))
        assertEquals(1L, output.latencyFrames)
        output.drain()
        assertFalse(target.calls.contains("drain"))
        target.room = 4
        assertFalse(output.isDrained)
        assertTrue(target.calls.contains("drain"))
        assertEquals(listOf(1,2,3,4,5,6,7,8), target.received.map { it.toInt() and 255 })
    }

    @Test fun malformedFrameBeyondFirstBatchDoesNotConsumeInputOrAdvancePhase() {
        val target = RecordingOutput()
        val output = DopInputOutput(target, 2, 3)
        val input = java.nio.ByteBuffer.allocate(100_000)
        repeat(16_000) { frame ->
            val marker = if (frame % 2 == 0) 5 else 0xfa
            input.put(3).put(1).put(marker.toByte()).put(4).put(2).put(marker.toByte())
        }
        input.put(3).put(1).put(0).put(4).put(2).put(0)
        input.flip()
        val error = assertThrows(DacException::class.java) { output.write(input) }
        assertEquals(DacErrorCode.INVALID_ARGUMENT, error.code)
        assertEquals(0, input.position())
        assertTrue(target.received.isEmpty())
        assertEquals(6, output.write(heapOf(3, 1, 5, 4, 2, 5)))
    }

}
