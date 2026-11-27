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
        assertThrows(IllegalArgumentException::class.java) { output.write(bad) }
        assertEquals(0, bad.position()); assertTrue(target.received.isEmpty())
        val wrongChannels = heapOf(0,3,1,5,0,4,2,0xfa)
        assertThrows(IllegalArgumentException::class.java) { output.write(wrongChannels) }
        val wrongPadding = heapOf(1,3,1,5,0,4,2,5)
        assertThrows(IllegalArgumentException::class.java) { output.write(wrongPadding) }
        val good = heapOf(0,3,1,5,0,4,2,5)
        assertEquals(8, output.write(good)); output.flush()
        assertEquals(8, output.write(good.apply { rewind() }))
    }
}
