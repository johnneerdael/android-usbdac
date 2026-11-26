package dev.usbdac.internal

import dev.usbdac.ManualExecutor
import org.junit.Assert.assertEquals
import org.junit.Test

class VolumeWriteQueueTest {
    private val executor = ManualExecutor()
    private val written = mutableListOf<VolumeOp>()
    private val queue = VolumeWriteQueue(executor, maxPendingSteps = 4) { written += it }

    @Test
    fun aBurstOfLevelsCollapsesToTheLastOne() {
        queue.submit(VolumeOp.SetFraction(0.1))
        queue.submit(VolumeOp.SetFraction(0.2))
        queue.submit(VolumeOp.SetDb(-30.0))
        queue.submit(VolumeOp.SetFraction(0.9))
        assertEquals("one task drains everything", 1, executor.pendingTasks)
        executor.runAll()
        assertEquals(listOf<VolumeOp>(VolumeOp.SetFraction(0.9)), written)
    }

    @Test
    fun oneWriteIsInFlightAndTheFinalValueIsStillApplied() {
        val inFlight = mutableListOf<VolumeOp>()
        lateinit var slow: VolumeWriteQueue
        slow = VolumeWriteQueue(executor) { op ->
            inFlight += op
            if (inFlight.size == 1) {
                // While the first write runs, the slider keeps moving.
                slow.submit(VolumeOp.SetFraction(0.5))
                slow.submit(VolumeOp.SetFraction(0.6))
                slow.submit(VolumeOp.SetFraction(0.7))
            }
        }
        slow.submit(VolumeOp.SetFraction(0.1))
        executor.runAll()
        assertEquals(listOf<VolumeOp>(VolumeOp.SetFraction(0.1), VolumeOp.SetFraction(0.7)), inFlight)
        assertEquals("no further task is left running", 0, executor.pendingTasks)
    }

    @Test
    fun theQueueRestartsAfterItDrained() {
        queue.submit(VolumeOp.SetFraction(0.1))
        executor.runAll()
        queue.submit(VolumeOp.SetFraction(0.2))
        assertEquals(1, executor.pendingTasks)
        executor.runAll()
        assertEquals(listOf<VolumeOp>(VolumeOp.SetFraction(0.1), VolumeOp.SetFraction(0.2)), written)
    }

    @Test
    fun identicalStepsMergeIntoACount() {
        repeat(3) { queue.submit(VolumeOp.Step(1, 16, 1)) }
        executor.runAll()
        assertEquals(listOf<VolumeOp>(VolumeOp.Step(1, 16, 3)), written)
    }

    @Test
    fun theMergedCountIsBounded() {
        repeat(10) { queue.submit(VolumeOp.Step(-1, 16, 1)) }
        executor.runAll()
        assertEquals(listOf<VolumeOp>(VolumeOp.Step(-1, 16, 4)), written)
    }

    @Test
    fun aStepInTheOtherDirectionOrSizeWaitsBehindThePendingOne() {
        queue.submit(VolumeOp.Step(1, 16, 1))
        queue.submit(VolumeOp.Step(-1, 16, 1))
        queue.submit(VolumeOp.Step(-1, 32, 1))
        executor.runAll()
        assertEquals(
            listOf<VolumeOp>(VolumeOp.Step(1, 16, 1), VolumeOp.Step(-1, 16, 1), VolumeOp.Step(-1, 32, 1)),
            written,
        )
    }

    @Test
    fun anAbsoluteLevelSupersedesEarlierStepsButNotLaterOnes() {
        queue.submit(VolumeOp.Step(1, 16, 1))
        queue.submit(VolumeOp.SetFraction(0.4))
        queue.submit(VolumeOp.Step(1, 16, 1))
        executor.runAll()
        assertEquals(listOf<VolumeOp>(VolumeOp.SetFraction(0.4), VolumeOp.Step(1, 16, 1)), written)
    }

    @Test
    fun muteIsIndependentOfTheLevelAndLatestWins() {
        queue.submit(VolumeOp.SetMute(true))
        queue.submit(VolumeOp.SetFraction(0.3))
        queue.submit(VolumeOp.SetMute(false))
        executor.runAll()
        assertEquals(listOf<VolumeOp>(VolumeOp.SetFraction(0.3), VolumeOp.SetMute(false)), written)
    }

    @Test
    fun aStepMergesAcrossAnInterleavedMute() {
        queue.submit(VolumeOp.Step(1, 16, 1))
        queue.submit(VolumeOp.SetMute(true))
        queue.submit(VolumeOp.Step(1, 16, 1))
        executor.runAll()
        assertEquals(1, written.count { it is VolumeOp.Step })
        assertEquals(VolumeOp.Step(1, 16, 2), written.single { it is VolumeOp.Step })
    }

    @Test
    fun clearDropsWhatHasNotStarted() {
        queue.submit(VolumeOp.SetFraction(0.8))
        queue.clear()
        executor.runAll()
        assertEquals(emptyList<VolumeOp>(), written)
    }
}
