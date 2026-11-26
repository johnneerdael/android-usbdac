package dev.usbdac.internal

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

class UsbInterfacesTest {
    @Test
    fun vendorPcmDiscoveryIsExactAndClaimsOnlyNativeProvenInterface() {
        assertTrue(isVendorPcmCandidate(0x2b73, 0x0017))
        assertTrue(isVendorPcmCandidate(0x2b73, 0x001b))
        assertFalse(isVendorPcmCandidate(0x2b73, 0x0018))
        assertFalse(isVendorPcmCandidate(0x2b74, 0x0017))
        val idle = InterfaceInfo(0, 0, 0xff, 0, false)
        val active = InterfaceInfo(0, 1, 0xff, 0, true, true)
        fun plan(interfaces: List<InterfaceInfo>, ids: List<Int> = listOf(0)) =
            planVendorPcmClaims(interfaces, 0x2b73, 0x0017, ids)
        assertEquals(listOf(0), plan(listOf(idle, active, control(2)))!!.allIds)
        assertEquals(listOf(0), planVendorPcmClaims(listOf(idle, active), 0x2b73, 0x001b)!!.allIds)
        assertNull(plan(listOf(idle, active.copy(hasIsochronousIn = false))))
        assertNull(plan(listOf(idle, active.copy(interfaceClass = 1))))
        assertNull(plan(listOf(idle, active, active.copy(alternateSetting = 2))))
        assertNull(plan(listOf(idle, active), listOf(1)))
        assertNull(plan(listOf(idle, active), listOf(0, 0)))
        assertNull(plan(listOf(idle, active), listOf(0, 2)))
        assertNull(planVendorPcmClaims(listOf(idle, active), 0x2b73, 0x0018, listOf(0)))
        assertNull(planClaims(listOf(idle, active)))
    }

    @Test
    fun rolandDiscoveryAndClaimsRequireBothExactDirectionalInterfaces() {
        val outputIdle = InterfaceInfo(0, 0, 0xff, 0, false)
        val output = InterfaceInfo(0, 1, 0xff, 2, true)
        val inputIdle = InterfaceInfo(1, 0, 0xff, 0, false)
        val input = InterfaceInfo(1, 1, 0xff, 2, false, true)
        val midi = InterfaceInfo(2, 0, 0xff, 3, false)
        val interfaces = listOf(outputIdle, output, inputIdle, input, midi)
        for (product in listOf(0x0120, 0x012f)) {
            assertTrue(isVendorPcmCandidate(0x0582, product))
            val plan = planVendorPcmClaims(interfaces, 0x0582, product)!!
            assertEquals(emptyList<Int>(), plan.controlIds)
            assertEquals(listOf(0, 1), plan.streamingIds)
            assertEquals(listOf(0, 1), plan.allIds)
        }
        assertFalse(isVendorPcmCandidate(0x0582, 0x0121))
        assertFalse(isVendorPcmCandidate(0x0583, 0x0120))
        fun plan(values: List<InterfaceInfo> = interfaces, ids: List<Int> = listOf(0, 1)) =
            planVendorPcmClaims(values, 0x0582, 0x0120, ids)
        for (ids in listOf(emptyList(), listOf(0), listOf(1), listOf(0, 0, 1), listOf(0, 1, 2), listOf(0, 2))) {
            assertNull(plan(ids = ids))
        }
        assertEquals(listOf(0, 1), plan(ids = listOf(1, 0))!!.allIds)
        assertNull(plan(interfaces.filterNot { it.id == 1 }))
        assertNull(plan(interfaces + input))
        assertEquals(listOf(0, 1), plan(interfaces + input.copy(alternateSetting = 2))!!.allIds)
        assertNull(plan(interfaces + inputIdle))
        assertNull(plan(listOf(outputIdle, output.copy(hasIsochronousIn = true), inputIdle, input)))
        assertNull(plan(listOf(outputIdle, output, inputIdle, input.copy(hasIsochronousOut = true))))
        assertNull(plan(listOf(outputIdle, output, inputIdle, input.copy(interfaceClass = 1))))
        assertNull(plan(listOf(outputIdle, output, inputIdle.copy(hasIsochronousIn = true), input)))
        assertNull(planClaims(interfaces))
    }

    @Test
    fun explicitCaptureClaimsUseOnlyPreflightInputsAndTheirControl() {
        val interfaces = listOf(control(0), streamingOut(1), control(4),
            streamingInOnly(5).copy(hasIsochronousIn = true),
            streamingInOnly(6).copy(hasIsochronousIn = true))
        val plan = planCaptureClaims(interfaces, listOf(4), listOf(5))!!
        assertEquals(listOf(4, 5), plan.allIds)
        assertEquals(listOf(0, 4, 1), planClaims(interfaces)!!.allIds)
        assertTrue(hasCapture(interfaces))
        assertNull(planCaptureClaims(interfaces, listOf(4), listOf(1)))
        assertNull(planCaptureClaims(interfaces, listOf(7), listOf(5)))
        assertNull(planCaptureClaims(interfaces, listOf(4), listOf(5, 5)))
    }

    @Test
    fun captureDiscoveryDoesNotRequirePlaybackOrAlterPlaybackPlan() {
        val interfaces = listOf(control(0), streamingInOnly(1).copy(hasIsochronousIn = true))
        assertTrue(hasCapture(interfaces))
        assertFalse(hasPlayback(interfaces))
        assertNull(planClaims(interfaces))
        assertEquals(listOf(0, 1), planCaptureClaims(interfaces, listOf(0), listOf(1))!!.allIds)
    }

    @Test
    fun optionalClaimRefusalKeepsOtherSourcesAndSkipsBaseClaims() {
        val attempted = mutableListOf<Int>()
        val owned = mutableListOf(0, 1)
        val unavailable = claimOptionalSources(listOf(1, 2, 3, 2, 4), owned.toList()) { id ->
            attempted += id
            (id != 3).also { if (it) owned += id }
        }
        assertEquals(listOf(2, 3, 4), attempted)
        assertEquals(listOf(3), unavailable)
        assertEquals(listOf(0, 1, 2, 4), owned)
    }

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
