package dev.usbdac

import dev.usbdac.internal.VolumeBackend
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Assert.fail
import org.junit.Test
import java.util.concurrent.Executor

/** A DAC whose volume is a fraction; records every call. */
private class FakeBackend : VolumeBackend {
    val calls = mutableListOf<String>()
    var fraction = 0.5
    var muted: Boolean? = false
    var failNext: DacException? = null
    var original: Double? = -20.0

    private fun state() = VolumeState(db = -80 + 80 * fraction, fraction = fraction, muted = muted)
    private fun check() {
        failNext?.let { failNext = null; throw it }
    }

    override fun get(): VolumeState { calls += "get"; return state() }
    override fun setDb(db: Double): VolumeState {
        calls += "setDb($db)"; check(); fraction = (db + 80) / 80; return state()
    }
    override fun setFraction(fraction: Double): VolumeState {
        calls += "setFraction($fraction)"; check(); this.fraction = fraction; return state()
    }
    override fun step(direction: Int, steps: Int): VolumeState {
        calls += "step($direction,$steps)"; check(); fraction = (fraction + direction / steps.toDouble()).coerceIn(0.0, 1.0); return state()
    }
    override fun setMute(muted: Boolean): VolumeState {
        calls += "setMute($muted)"; check(); this.muted = muted; return state()
    }
    override fun originalDb() = original
    override fun handBack(): Boolean { calls += "handBack"; check(); return true }
}

class DacVolumeTest {
    private val backend = FakeBackend()
    private val errors = mutableListOf<DacException>()
    private val inline = Executor { it.run() }
    private val startup = StartupVolumeReport(
        originalDb = listOf(0.0), appliedDb = listOf(-40.0), limitDb = 0.0,
        attenuated = true, clampedToMinimum = false, maxDb = -40.0,
    )

    private fun volume(executor: Executor = inline, hasMute: Boolean = true) = DacVolume(
        backend = backend,
        range = VolumeRange(-80.0, 0.0, 1.0),
        hasMute = hasMute,
        initial = VolumeState(-40.0, 0.5, false),
        startup = startup,
        executor = executor,
        onWriteFailed = { errors += it },
    )

    @Test
    fun theStartupReportIsWhatTheLibraryDidBeforeAnyStream() {
        val volume = volume()
        assertEquals(listOf(0.0), volume.startup.originalDb)
        assertEquals(listOf(-40.0), volume.startup.appliedDb)
        assertEquals("a host restoring a remembered level stays at or below this", 0.0, volume.startup.limitDb, 0.0)
        assertTrue(volume.startup.attenuated)
        // The state a UI starts from is the attenuated level, not the original.
        assertEquals(-40.0, volume.state.value.db, 0.0)
    }

    @Test
    fun theStateFollowsWhatTheDacReadsBack() {
        val volume = volume()
        assertEquals(0.5, volume.state.value.fraction, 0.0)
        volume.setFraction(0.25)
        assertEquals(0.25, volume.state.value.fraction, 0.0)
        volume.setDb(-8.0)
        assertEquals(0.9, volume.state.value.fraction, 1e-9)
    }

    @Test
    fun aFractionOutsideTheCurveIsClampedBeforeItIsWritten() {
        val volume = volume()
        volume.setFraction(1.7)
        volume.setFraction(-3.0)
        assertEquals(listOf("setFraction(1.0)", "setFraction(0.0)"), backend.calls)
    }

    @Test
    fun aSliderBurstWritesOnlyTheLastValue() {
        val executor = ManualExecutor()
        val volume = volume(executor)
        (1..50).forEach { volume.setFraction(it / 100.0) }
        executor.runAll()
        assertEquals(listOf("setFraction(0.5)"), backend.calls)
        assertEquals(0.5, volume.state.value.fraction, 0.0)
    }

    @Test
    fun stepsKeepTheirDirectionAndSize() {
        val volume = volume()
        volume.step(+7)
        volume.step(-3, steps = 8)
        assertEquals(listOf("step(1,16)", "step(-1,8)"), backend.calls)
    }

    @Test
    fun aDirectionOfZeroDoesNothingAndTheSizeIsKeptInRange() {
        val volume = volume()
        volume.step(0)
        volume.step(1, steps = 0)
        volume.step(1, steps = 9999)
        assertEquals(listOf("step(1,16)", "step(1,255)"), backend.calls)
    }

    @Test
    fun heldKeyPressesAreMergedAndAllApplied() {
        val executor = ManualExecutor()
        val volume = volume(executor)
        repeat(3) { volume.step(1) }
        executor.runAll()
        assertEquals(listOf("step(1,16)", "step(1,16)", "step(1,16)"), backend.calls)
        assertEquals(0.5 + 3 / 16.0, volume.state.value.fraction, 1e-9)
    }

    @Test
    fun aFailedWriteIsReportedAndTheStateIsKept() {
        val volume = volume()
        backend.failNext = DacException(DacErrorCode.CONTROL, "stall")
        volume.setFraction(0.9)
        assertEquals(1, errors.size)
        assertEquals(DacErrorCode.CONTROL, errors.single().code)
        assertEquals(0.5, volume.state.value.fraction, 0.0)
        volume.setFraction(0.8)
        assertEquals("the queue keeps working after a failure", 0.8, volume.state.value.fraction, 0.0)
    }

    @Test
    fun anUnexpectedFailureIsReportedAsUnknown() {
        val broken = object : VolumeBackend by backend {
            override fun setFraction(fraction: Double): VolumeState = throw RuntimeException("boom")
        }
        val failing = DacVolume(broken, VolumeRange(-80.0, 0.0, 1.0), true, VolumeState(-40.0, 0.5, false), startup, inline) { errors += it }
        failing.setFraction(0.1)
        assertEquals(DacErrorCode.UNKNOWN, errors.single().code)
        assertEquals(0.5, failing.state.value.fraction, 0.0)
    }

    @Test
    fun refreshReadsTheDacAndUpdatesTheState() {
        val volume = volume()
        backend.fraction = 0.75 // a physical knob
        assertEquals(0.5, volume.state.value.fraction, 0.0)
        assertEquals(0.75, volume.refresh().fraction, 0.0)
        assertEquals(0.75, volume.state.value.fraction, 0.0)
    }

    @Test
    fun handBackDropsPendingWritesFirst() {
        val executor = ManualExecutor()
        val volume = volume(executor)
        volume.setFraction(0.9)
        assertTrue(volume.handBack())
        executor.runAll()
        assertEquals(listOf("handBack"), backend.calls)
    }

    @Test
    fun theOriginalLevelComesFromTheDac() {
        val volume = volume()
        assertEquals(-20.0, volume.originalDb!!, 0.0)
        backend.original = null
        assertNull(volume.originalDb)
    }

    @Test
    fun afterCloseWritesAreIgnoredAndBlockingCallsRefuse() {
        val volume = volume()
        volume.close()
        volume.setFraction(0.2)
        volume.setMute(true)
        assertEquals(emptyList<String>(), backend.calls)
        try {
            volume.refresh()
            fail("refresh must refuse on a closed session")
        } catch (error: DacException) {
            assertEquals(DacErrorCode.INVALID_STATE, error.code)
        }
        try {
            volume.handBack()
            fail("handBack must refuse on a closed session")
        } catch (error: DacException) {
            assertEquals(DacErrorCode.INVALID_STATE, error.code)
        }
    }

    @Test
    fun muteIsWritten() {
        val volume = volume()
        volume.setMute(true)
        assertEquals(listOf("setMute(true)"), backend.calls)
        assertEquals(true, volume.state.value.muted)
        assertFalse(volume(hasMute = false).hasMute)
    }
}
