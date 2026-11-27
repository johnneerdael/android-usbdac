package dev.usbdac

import dev.usbdac.internal.OutputPlatform
import dev.usbdac.internal.toNativeJson
import kotlinx.coroutines.CoroutineStart
import kotlinx.coroutines.awaitCancellation
import kotlinx.coroutines.cancelAndJoin
import kotlinx.coroutines.launch
import kotlinx.coroutines.runBlocking
import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertNull
import org.junit.Assert.assertSame
import org.junit.Assert.assertTrue
import org.junit.Test

class OutputHandoffTest {
    private val request = StreamRequest.pcm(96_000, 2, 24)

    @Test
    fun drainsAndReleasesOldRouteBeforeOpeningNextTrack() = runBlocking {
        val platform = FakePlatform()
        val dac = fakeDac()
        val link = platform.attach(dac)
        val checking = object : OutputPlatform by platform {
            override fun openDac(dac: DacDeviceInfo, config: DacConfig) = platform.openDac(dac, config).also {
                link.streams.lastOrNull()?.let { outgoing ->
                    assertEquals(listOf("drain", "close"), outgoing.calls)
                    assertEquals(1, link.closeCount)
                }
            }
        }
        val handoff = OutputHandoff(OutputSelector(checking, OutputPolicy()))
        val old = handoff.open(request)
        link.streams.single().drained = true
        val replacement = handoff.open(request, positionUs = 1_500_000)
        assertFalse(old.output === replacement.output)
        assertEquals(1_500_000L, replacement.positionUs)
        assertEquals(2, link.openCount)
        handoff.close()
    }

    @Test
    fun drainTimeoutReleasesOutputAndDoesNotClaimAnother() = runBlocking {
        val platform = FakePlatform()
        val link = platform.attach(fakeDac())
        val handoff = OutputHandoff(OutputSelector(platform, OutputPolicy()))
        handoff.open(request)
        val error = runCatching { handoff.open(request, drainTimeoutMs = 10) }.exceptionOrNull()
        assertEquals(DacErrorCode.TRANSFER, (error as DacException).code)
        assertEquals(listOf("drain", "close"), link.streams.single().calls)
        assertEquals(1, link.openCount)
        assertSame(OutputHandoffState.Idle, handoff.state.value)
    }

    @Test
    fun reconnectUsesPlayedPositionAndSameSerialAfterFreshPermission() = runBlocking {
        val platform = FakePlatform()
        val first = fakeDac(deviceName = "/dev/bus/usb/001/002")
        val link = platform.attach(first)
        val other = fakeDac(deviceName = "/dev/bus/usb/001/003")
        val returned = fakeDac(deviceName = "/dev/bus/usb/001/004")
        val serials = mapOf(first.deviceName to "A", returned.deviceName to "A", other.deviceName to "B")
        val platformWithSerial = object : OutputPlatform by platform {
            override fun usbSerial(dac: DacDeviceInfo): String? = serials[dac.deviceName]
        }
        val handoff = OutputHandoff(OutputSelector(platformWithSerial, OutputPolicy()))
        val old = handoff.open(request)
        handoff.seekTo(2_000_000)
        link.streams.single().played = 48_000
        platform.dacs = emptyList()
        handoff.handleChange(OutputChange.DacDetached(first))
        val waiting = handoff.state.value as OutputHandoffState.WaitingForUsb
        assertEquals(2_500_000L, waiting.positionUs)
        assertTrue(waiting.canMatchAutomatically)
        assertTrue(link.streams.single().closed)

        val otherLink = platform.attach(other)
        handoff.handleChange(OutputChange.DacAttached(other))
        assertSame(waiting, handoff.state.value)
        val returnedLink = platform.attach(returned, permission = false)
        handoff.handleChange(OutputChange.DacAttached(returned))
        assertSame(waiting, handoff.state.value)
        handoff.setPlaying(false)
        platform.permitted += returned.deviceName
        handoff.handleChange(OutputChange.PermissionGranted(returned))
        assertSame(waiting, handoff.state.value)
        handoff.setPlaying(true)
        val resumed = handoff.state.value as OutputHandoffState.Ready
        assertEquals(returned, resumed.output.decision.dac)
        assertEquals(waiting.positionUs, resumed.positionUs)
        handoff.handleChange(OutputChange.PermissionGranted(returned))
        handoff.onDeviceLost(old.output)
        assertSame(resumed, handoff.state.value)
        assertEquals(0, otherLink.openCount)
        assertEquals(1, returnedLink.openCount)
        assertTrue(platform.managed.isEmpty())
        assertTrue(platform.offers.isEmpty())
        handoff.close()
    }

    @Test
    fun duplicateUsbIdentitiesDoNotReopenEitherAttachment() = runBlocking {
        val platform = FakePlatform()
        val first = fakeDac()
        platform.attach(first)
        val serial = object : OutputPlatform by platform {
            override fun usbSerial(dac: DacDeviceInfo): String = "duplicated serial"
        }
        val handoff = OutputHandoff(OutputSelector(serial, OutputPolicy()))
        val old = handoff.open(request)
        platform.dacs = emptyList()
        handoff.onDeviceLost(old.output)
        val one = fakeDac(deviceName = "/dev/bus/usb/001/003")
        val two = fakeDac(deviceName = "/dev/bus/usb/001/004")
        val oneLink = platform.attach(one)
        val twoLink = platform.attach(two)
        handoff.handleChange(OutputChange.PermissionGranted(two))
        assertTrue(handoff.state.value is OutputHandoffState.WaitingForUsb)
        assertEquals(0, oneLink.openCount)
        assertEquals(0, twoLink.openCount)
        handoff.close()
    }

    @Test
    fun aSeriallessDacRequiresExplicitConfirmationAndRespectsPause() = runBlocking {
        val platform = FakePlatform()
        val first = fakeDac()
        platform.attach(first)
        val returned = fakeDac(deviceName = "/dev/bus/usb/001/004")
        val noSerial = object : OutputPlatform by platform {
            override fun usbSerial(dac: DacDeviceInfo): String? = null
        }
        val handoff = OutputHandoff(OutputSelector(noSerial, OutputPolicy()))
        val old = handoff.open(request, playWhenReady = false)
        handoff.onDeviceLost(old.output)
        val replacement = platform.attach(returned)
        handoff.handleChange(OutputChange.PermissionGranted(returned))
        assertFalse((handoff.state.value as OutputHandoffState.WaitingForUsb).canMatchAutomatically)
        assertEquals(0, replacement.openCount)
        handoff.reconnectTo(returned)
        assertEquals(listOf("pause"), replacement.streams.single().calls)
        handoff.close()
    }

    @Test
    fun fallbackRequiresAnExplicitChoiceAndUsesTheSavedPosition() = runBlocking {
        val platform = FakePlatform()
        val link = platform.attach(fakeDac())
        val handoff = OutputHandoff(OutputSelector(platform, OutputPolicy()))
        val original = handoff.open(request, positionUs = 4_000_000)
        link.streams.single().played = 96_000
        val error = runCatching { handoff.fallbackAfterFailure(original.output, OutputKind.ANDROID_STANDARD) }
        assertTrue(error.exceptionOrNull() is IllegalArgumentException)
        assertSame(original, handoff.state.value)
        assertTrue(platform.managed.isEmpty())

        // Until the platform has taken the device back, a USB-only fallback refuses to open.
        val unavailable = runCatching {
            handoff.fallbackAfterFailure(original.output, OutputKind.ANDROID_BIT_PERFECT)
        }
        assertNotNull(unavailable.exceptionOrNull())
        assertTrue(platform.managed.isEmpty())
        assertEquals(5_000_000L, (handoff.state.value as OutputHandoffState.WaitingForUsb).positionUs)
        assertNull(handoff.fallbackAfterFailure(original.output, OutputKind.ANDROID_BIT_PERFECT))
        handoff.close()
    }

    @Test
    fun fallbackOpensOnlyTheRequestedKindAfterReleasingDirectUsb() = runBlocking {
        val platform = FakePlatform()
        val link = platform.attach(fakeDac())
        platform.route = listOf(USB_ROUTE)
        val checking = object : OutputPlatform by platform {
            override fun openManaged(request: StreamRequest, route: OutputRoute?): PcmOutput {
                assertTrue(link.streams.single().closed)
                assertEquals(1, link.closeCount)
                return platform.openManaged(request, route)
            }
        }
        val handoff = OutputHandoff(OutputSelector(checking, OutputPolicy()))
        val original = handoff.open(request, positionUs = 4_000_000)
        link.streams.single().played = 96_000
        val fallback = checkNotNull(handoff.fallbackAfterFailure(original.output, OutputKind.ANDROID_BIT_PERFECT))
        assertEquals(5_000_000L, fallback.positionUs)
        assertEquals(OutputKind.ANDROID_BIT_PERFECT, fallback.output.kind)
        assertSame(USB_ROUTE, platform.managed.single().second)
        handoff.close()
    }

    @Test
    fun cancelledPlayerScopeStillReleasesItsOutput() = runBlocking {
        val platform = FakePlatform()
        val link = platform.attach(fakeDac())
        val handoff = OutputHandoff(OutputSelector(platform, OutputPolicy()))
        handoff.open(request)
        val player = launch(start = CoroutineStart.UNDISPATCHED) {
            try {
                awaitCancellation()
            } finally {
                handoff.close()
            }
        }
        player.cancelAndJoin()
        assertTrue(link.streams.single().closed)
        assertEquals(1, link.closeCount)
        assertSame(OutputHandoffState.Idle, handoff.state.value)
    }

    @Test
    fun queueProfilesSerializeThePeriodOnlyWhenSelected() {
        val config = DacConfig(bufferTargetMs = 150)
        val interactive = config.withUsbQueueProfile(UsbQueueProfile.LOW_LATENCY)
        val json = JSONObject(interactive.toNativeJson())
        assertEquals(150, json.getInt("buffer_target_ms"))
        assertEquals(8, json.getInt("bus_queue_ms"))
        assertEquals(8, json.getInt("transfers_in_flight"))
        assertEquals(1_000, json.getInt("transfer_period_us"))
        val lowCpu = JSONObject(interactive.withUsbQueueProfile(UsbQueueProfile.LOW_CPU).toNativeJson())
        assertEquals(16, lowCpu.getInt("bus_queue_ms"))
        assertEquals(4, lowCpu.getInt("transfers_in_flight"))
        assertFalse(lowCpu.has("transfer_period_us"))
    }
}
