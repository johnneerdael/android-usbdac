package dev.usbdac.controls

import dev.usbdac.BitPerfectState
import dev.usbdac.PcmOutput
import kotlinx.coroutines.ExperimentalCoroutinesApi
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.test.advanceTimeBy
import kotlinx.coroutines.test.runCurrent
import kotlinx.coroutines.test.runTest
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertSame
import org.junit.Assert.assertTrue
import org.junit.Test
import java.nio.ByteBuffer
import kotlin.coroutines.EmptyCoroutineContext

private class FakeOutput : PcmOutput {
    var closed = false
    override fun write(buffer: ByteBuffer): Int = 0
    override val writableBytes: Int get() = 0
    override fun play() = Unit
    override fun pause() = Unit
    override fun flush() = Unit
    override fun drain() = Unit
    override val isDrained: Boolean get() = false
    override val playedFrames: Long get() = 0
    override val latencyFrames: Long get() = 0
    override val bitPerfect: StateFlow<BitPerfectState> = MutableStateFlow(BitPerfectState.Unverifiable("fake"))
    override fun close() {
        closed = true
    }
}

@OptIn(ExperimentalCoroutinesApi::class)
class DacIdleReleaseTest {
    @Test
    fun releasesAfterTheTimeoutOfContinuousPause() = runTest {
        val first = FakeOutput()
        val idle = DacIdleRelease(this, 30_000, reopen = { FakeOutput() }, closeContext = EmptyCoroutineContext)
        idle.attach(first)

        idle.onPaused()
        advanceTimeBy(29_999)
        runCurrent()
        assertFalse(first.closed)
        assertFalse(idle.isReleased.value)

        advanceTimeBy(1)
        runCurrent()
        assertTrue(first.closed)
        assertTrue(idle.isReleased.value)
        assertNull(idle.output)
    }

    @Test
    fun resumingBeforeTheTimeoutKeepsTheSameOutputAndNothingCloses() = runTest {
        val first = FakeOutput()
        var reopened = 0
        val idle = DacIdleRelease(this, 30_000, reopen = { reopened++; FakeOutput() }, closeContext = EmptyCoroutineContext)
        idle.attach(first)

        idle.onPaused()
        advanceTimeBy(20_000)
        assertSame(first, idle.onResumed())
        advanceTimeBy(60_000)
        runCurrent()

        assertFalse(first.closed)
        assertEquals(0, reopened)
        assertFalse(idle.isReleased.value)
    }

    @Test
    fun resumingAfterTheReleaseReopens() = runTest {
        val first = FakeOutput()
        val second = FakeOutput()
        val idle = DacIdleRelease(this, 1_000, reopen = { second }, closeContext = EmptyCoroutineContext)
        idle.attach(first)
        idle.onPaused()
        advanceTimeBy(1_000)
        runCurrent()
        assertTrue(first.closed)

        assertSame(second, idle.onResumed())
        assertFalse(idle.isReleased.value)
        assertSame(second, idle.output)
        assertFalse(second.closed)
    }

    @Test
    fun theTimeRestartsWithEachPause() = runTest {
        val first = FakeOutput()
        val idle = DacIdleRelease(this, 1_000, reopen = { FakeOutput() }, closeContext = EmptyCoroutineContext)
        idle.attach(first)

        idle.onPaused()
        advanceTimeBy(900)
        idle.onResumed()
        idle.onPaused()
        advanceTimeBy(900)
        runCurrent()
        assertFalse("900 ms into the second pause is not a release", first.closed)

        advanceTimeBy(100)
        runCurrent()
        assertTrue(first.closed)
    }

    @Test
    fun pausingTwiceDoesNotExtendTheTimer() = runTest {
        val first = FakeOutput()
        val idle = DacIdleRelease(this, 1_000, reopen = { FakeOutput() }, closeContext = EmptyCoroutineContext)
        idle.attach(first)
        idle.onPaused()
        advanceTimeBy(600)
        idle.onPaused()
        advanceTimeBy(400)
        runCurrent()
        assertTrue(first.closed)
    }

    @Test
    fun aFailedReopenLeavesTheOutputReleased() = runTest {
        val idle = DacIdleRelease<FakeOutput>(this, 1_000, reopen = { error("no DAC") }, closeContext = EmptyCoroutineContext)
        idle.attach(FakeOutput())
        idle.onPaused()
        advanceTimeBy(1_000)
        runCurrent()
        try {
            idle.onResumed()
            org.junit.Assert.fail("the reopen failure must propagate")
        } catch (error: IllegalStateException) {
            assertEquals("no DAC", error.message)
        }
        assertTrue(idle.isReleased.value)
        assertNull(idle.output)
    }

    @Test
    fun pausingWithNothingAttachedStartsNoTimer() = runTest {
        val idle = DacIdleRelease<FakeOutput>(this, 1_000, reopen = { FakeOutput() }, closeContext = EmptyCoroutineContext)
        idle.onPaused()
        advanceTimeBy(5_000)
        runCurrent()
        assertFalse(idle.isReleased.value)
    }

    @Test
    fun cancelStopsTheTimerWithoutClosing() = runTest {
        val first = FakeOutput()
        val idle = DacIdleRelease(this, 1_000, reopen = { FakeOutput() }, closeContext = EmptyCoroutineContext)
        idle.attach(first)
        idle.onPaused()
        idle.cancel()
        advanceTimeBy(5_000)
        runCurrent()
        assertFalse(first.closed)
    }
    @Test
    fun resumeReportsWhetherTheQueuedAudioWasActuallyDiscarded() = runTest {
        val first = FakeOutput()
        val second = FakeOutput()
        val idle = DacIdleRelease(this, 1_000, reopen = { second }, closeContext = EmptyCoroutineContext)
        idle.attach(first)
        idle.onPaused()
        assertFalse(idle.resume().reopened)
        idle.onPaused()
        advanceTimeBy(1_000)
        runCurrent()
        val result = idle.resume()
        assertTrue(result.reopened)
        assertSame(second, result.output)
    }

    @Test
    fun attachingDuringOldCloseKeepsTheReplacement() = runTest {
        val replacement = FakeOutput()
        lateinit var idle: DacIdleRelease<PcmOutput>
        val old = object : PcmOutput by FakeOutput() {
            override fun close() { idle.attach(replacement) }
        }
        idle = DacIdleRelease(this, 1_000, reopen = { error("replacement already attached") }, closeContext = EmptyCoroutineContext)
        idle.attach(old)
        idle.onPaused()
        advanceTimeBy(1_000)
        runCurrent()
        assertSame(replacement, idle.output)
        assertFalse(idle.isReleased.value)
        assertFalse(idle.resume().reopened)
    }

}
