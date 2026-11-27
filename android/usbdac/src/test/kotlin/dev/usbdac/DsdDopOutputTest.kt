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
    @Test fun oneByteFrameSurvivesDisjointWritesAndIncompleteEosFails() {
        val target = RecordingOutput()
        val output = DsdDopOutput(target, 2, 3, true)
        assertEquals(2, output.write(heapOf(1, 2)))
        assertThrows(IllegalStateException::class.java) { output.drain() }
        assertEquals(2, output.write(heapOf(3, 4)))
        assertEquals(listOf(3,1,5,4,2,5), target.received.map { it.toInt() and 255 })
        output.drain()
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

}
