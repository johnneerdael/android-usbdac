package dev.usbdac.internal

import org.junit.Assert.assertEquals
import org.junit.Test

class HeadPositionTrackerTest {
    @Test
    fun countsFramesFromTheBaseline() {
        val tracker = HeadPositionTracker()
        tracker.reset(0)
        assertEquals(0, tracker.update(0))
        assertEquals(44_100, tracker.update(44_100))
    }

    @Test
    fun theValueIsReadAsUnsigned32Bit() {
        val tracker = HeadPositionTracker()
        tracker.reset(0)
        // 0x80000000 is a signed Int of Int.MIN_VALUE but 2^31 frames played.
        assertEquals(1L shl 31, tracker.update(Int.MIN_VALUE))
    }

    @Test
    fun aWrapOfTheCounterKeepsCounting() {
        val tracker = HeadPositionTracker()
        tracker.reset(0)
        tracker.update(Int.MIN_VALUE)
        tracker.update(-1000) // 2^32 - 1000
        assertEquals((1L shl 32) + 500, tracker.update(500))
    }

    @Test
    fun resetMeasuresFromTheNewPosition() {
        val tracker = HeadPositionTracker()
        tracker.reset(0)
        tracker.update(100_000)
        tracker.reset(100_000)
        assertEquals(0, tracker.update(100_000))
        assertEquals(2_000, tracker.update(102_000))
    }

    @Test
    fun aDropToZeroAfterTheResetIsAFlushNotAWrap() {
        val tracker = HeadPositionTracker()
        tracker.reset(0)
        tracker.update(500_000)
        // The platform's flush finished after reset() ran: the head restarts at zero.
        tracker.reset(500_000)
        assertEquals(0, tracker.update(0))
        assertEquals(1_000, tracker.update(1_000))
    }

    @Test
    fun aReadingBelowThePreviousOneIsTheHeadRestartingAtZero() {
        val tracker = HeadPositionTracker()
        tracker.reset(1_000)
        // Not a wrap (the step back is small): the platform reset its counter, so count from zero.
        assertEquals(900, tracker.update(900))
        assertEquals(1_900, tracker.update(1_900))
    }
}
