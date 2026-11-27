@file:Suppress("INVISIBLE_MEMBER", "INVISIBLE_REFERENCE", "DEPRECATION")

package dev.usbdac.media3

import androidx.media3.common.DeviceInfo
import androidx.media3.common.Player
import dev.usbdac.*
import dev.usbdac.internal.VolumeBackend
import kotlinx.coroutines.ExperimentalCoroutinesApi
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.test.runCurrent
import kotlinx.coroutines.test.runTest
import org.junit.Assert.*
import org.junit.Test
import java.lang.reflect.Proxy
import java.util.concurrent.Executor

/** Tests the published wrapper against the same volume backend seam as the core controls tests. */
@OptIn(ExperimentalCoroutinesApi::class)
class DacVolumePlayerTest {
    @Test
    fun preTrackVolumeUsesHardwareCurveStepsMuteAndReturnsToLocalOnDetach() = runTest {
        var fraction = 0.5
        var muted = false
        val steps = mutableListOf<Int>()
        fun state() = VolumeState(-80 + 80 * fraction, fraction, muted)
        val backend = object : VolumeBackend {
            override fun get() = state()
            override fun setDb(db: Double): VolumeState { fraction = (db + 80) / 80; return state() }
            override fun setFraction(value: Double): VolumeState { fraction = value; return state() }
            override fun step(direction: Int, count: Int): VolumeState {
                steps += direction; fraction += direction / count.toDouble(); return state()
            }
            override fun setMute(value: Boolean): VolumeState { muted = value; return state() }
            override fun originalDb(): Double? = -20.0
            override fun handBack() = true
        }
        val dac = DacVolume(backend, VolumeRange(-80.0, 0.0, 1.0), true, state(),
            StartupVolumeReport(listOf(-20.0), listOf(-40.0), -20.0, true, false, -40.0),
            Executor { it.run() }, { throw it })
        val volumes = MutableStateFlow<DacVolume?>(dac)
        var localSets = 0
        val local = Proxy.newProxyInstance(Player::class.java.classLoader, arrayOf(Player::class.java)) { _, method, _ ->
            when (method.name) {
                "getApplicationLooper" -> null // Android JVM stubs: no looper exists in this test.
                "getAvailableCommands" -> Player.Commands.EMPTY
                "getDeviceInfo" -> DeviceInfo.UNKNOWN
                "getDeviceVolume" -> 9
                "isDeviceMuted" -> false
                "setDeviceVolume" -> { localSets++; null }
                else -> null
            }
        } as Player
        val wrapper = DacVolumePlayer(local, volumes, backgroundScope)
        assertEquals(DeviceInfo.PLAYBACK_TYPE_REMOTE, wrapper.deviceInfo.playbackType)
        assertEquals(50, wrapper.deviceVolume)
        var aggregateEvents = 0
        wrapper.addListener(object : Player.Listener {
            override fun onEvents(player: Player, events: Player.Events) { aggregateEvents++ }
        })
        runCurrent()
        wrapper.setDeviceVolume(200)
        assertEquals(1.0, fraction, 0.0)
        wrapper.setDeviceVolume(-1, 0)
        assertEquals(0.0, fraction, 0.0)
        wrapper.increaseDeviceVolume()
        wrapper.decreaseDeviceVolume(0)
        assertEquals(listOf(1, -1), steps)
        wrapper.setDeviceMuted(true)
        assertTrue(muted)
        runCurrent()
        assertTrue(aggregateEvents > 0)
        volumes.value = null
        runCurrent()
        assertEquals(9, wrapper.deviceVolume)
        wrapper.setDeviceVolume(4)
        assertEquals(1, localSets)
    }
}
