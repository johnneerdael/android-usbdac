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
        assertEquals(4, output.write(source))
        assertEquals(2, source.remaining())
        assertEquals(0, output.writableBytes)
        target.room = 8
        assertEquals(0, output.write(source))
        assertEquals(listOf(0,3,1,5,0,4,2,5), target.received.map { it.toInt() and 255 })
    }
}
