package dev.usbdac.internal

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

class UsbInterfacesTest {
    private val audio = 1
    private val hid = 3

    private fun control(id: Int) = InterfaceInfo(id, 0, audio, 0x01, false)
    private fun streamingIdle(id: Int) = InterfaceInfo(id, 0, audio, 0x02, false)
    private fun streamingOut(id: Int, alt: Int = 1) = InterfaceInfo(id, alt, audio, 0x02, true)
    private fun streamingInOnly(id: Int, alt: Int = 1) = InterfaceInfo(id, alt, audio, 0x02, false)
    private fun hid(id: Int) = InterfaceInfo(id, 0, hid, 0, true)

    @Test
    fun aTypicalDacClaimsControlAndPlayback() {
        val plan = planClaims(listOf(control(0), streamingIdle(1), streamingOut(1, 1), streamingOut(1, 2)))!!
        assertEquals(listOf(0), plan.controlIds)
        assertEquals(listOf(1), plan.streamingIds)
        assertEquals(listOf(0, 1), plan.allIds)
    }

    @Test
    fun aCaptureInterfaceIsLeftToAndroid() {
        val plan = planClaims(
            listOf(control(0), streamingIdle(1), streamingOut(1), streamingIdle(2), streamingInOnly(2)),
        )!!
        assertEquals(listOf(1), plan.streamingIds)
    }

    @Test
    fun severalPlaybackInterfacesAreAllClaimedOnceEach() {
        val plan = planClaims(listOf(control(0), streamingOut(1), streamingOut(1, 2), streamingOut(3)))!!
        assertEquals(listOf(1, 3), plan.streamingIds)
        assertEquals(listOf(0, 1, 3), plan.allIds)
    }

    @Test
    fun nonAudioInterfacesAreNotClaimed() {
        val plan = planClaims(listOf(control(0), streamingOut(1), hid(2)))!!
        assertEquals(listOf(0, 1), plan.allIds)
    }

    @Test
    fun withoutControlOrPlaybackThereIsNothingToDrive() {
        assertNull(planClaims(listOf(streamingOut(1))))
        assertNull(planClaims(listOf(control(0), streamingInOnly(1))))
        assertNull(planClaims(emptyList()))
    }

    @Test
    fun aDeviceCanPlayOnlyWithAnIsochronousOutEndpointOnAudioStreaming() {
        assertTrue(hasPlayback(listOf(control(0), streamingOut(1))))
        assertFalse(hasPlayback(listOf(control(0), streamingInOnly(1))))
        assertFalse(hasPlayback(listOf(hid(0))))
    }
}
