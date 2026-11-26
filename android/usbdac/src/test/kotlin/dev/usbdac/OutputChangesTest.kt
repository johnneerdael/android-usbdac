package dev.usbdac

import kotlinx.coroutines.ExperimentalCoroutinesApi
import kotlinx.coroutines.Job
import kotlinx.coroutines.launch
import kotlinx.coroutines.flow.toList
import kotlinx.coroutines.test.UnconfinedTestDispatcher
import kotlinx.coroutines.test.runTest
import org.junit.Assert.assertEquals
import org.junit.Assert.assertSame
import org.junit.Assert.assertTrue
import org.junit.Test

@OptIn(ExperimentalCoroutinesApi::class)
class OutputChangesTest {
    @Test
    fun aDacComingAndGoingAndBeingAllowedAreReportedAsChanges() = runTest {
        val platform = FakePlatform()
        val selector = OutputSelector(platform, OutputPolicy())
        val dac = fakeDac()
        val seen = mutableListOf<OutputChange>()
        val job: Job = launch(UnconfinedTestDispatcher(testScheduler)) { selector.changes.toList(seen) }

        platform.hotplugFlow.tryEmit(DacHotplugEvent.Attached(dac))
        platform.hotplugFlow.tryEmit(DacHotplugEvent.PermissionGranted(dac))
        platform.hotplugFlow.tryEmit(DacHotplugEvent.Detached(dac))
        testScheduler.runCurrent()
        job.cancel()

        assertEquals(3, seen.size)
        assertSame(dac, (seen[0] as OutputChange.DacAttached).dac)
        assertSame(dac, (seen[1] as OutputChange.PermissionGranted).dac)
        assertSame(dac, (seen[2] as OutputChange.DacDetached).dac)
    }

    @Test
    fun aRouteThatAppearsOrGoesAwayIsReportedWithTheRoutes() = runTest {
        val platform = FakePlatform()
        val selector = OutputSelector(platform, OutputPolicy())
        val seen = mutableListOf<OutputChange>()
        val job = launch(UnconfinedTestDispatcher(testScheduler)) { selector.changes.toList(seen) }

        platform.routeFlow.tryEmit(OutputChange.RouteChanged(added = listOf(BLUETOOTH_ROUTE), removed = emptyList()))
        platform.routeFlow.tryEmit(OutputChange.RouteChanged(added = emptyList(), removed = listOf(USB_ROUTE)))
        testScheduler.runCurrent()
        job.cancel()

        assertEquals(
            listOf(
                OutputChange.RouteChanged(listOf(BLUETOOTH_ROUTE), emptyList()),
                OutputChange.RouteChanged(emptyList(), listOf(USB_ROUTE)),
            ),
            seen,
        )
    }

    @Test
    fun bothSourcesArriveOnOneFlow() = runTest {
        val platform = FakePlatform()
        val selector = OutputSelector(platform, OutputPolicy())
        val seen = mutableListOf<OutputChange>()
        val job = launch(UnconfinedTestDispatcher(testScheduler)) { selector.changes.toList(seen) }

        platform.hotplugFlow.tryEmit(DacHotplugEvent.Attached(fakeDac()))
        platform.routeFlow.tryEmit(OutputChange.RouteChanged(listOf(USB_ROUTE), emptyList()))
        testScheduler.runCurrent()
        job.cancel()

        assertEquals(2, seen.size)
        assertTrue(seen.any { it is OutputChange.DacAttached })
        assertTrue(seen.any { it is OutputChange.RouteChanged })
    }

    @Test
    fun theSelectorNeverSwitchesAnOutputOnItsOwn() = runTest {
        val platform = FakePlatform()
        platform.attach(fakeDac(), permission = false)
        val selector = OutputSelector(platform, OutputPolicy())
        platform.apiLevel = 33
        platform.grant = fakeGrant(exclusive = false)
        val output = selector.open(StreamRequest.pcm(44_100, 2, 16))
        val before = platform.calls.toList()
        val job = launch(UnconfinedTestDispatcher(testScheduler)) { selector.changes.toList(mutableListOf()) }

        // The DAC is allowed while the standard output plays: that is an event, not a switch.
        platform.permitted += fakeDac().deviceName
        platform.hotplugFlow.tryEmit(DacHotplugEvent.PermissionGranted(fakeDac()))
        testScheduler.runCurrent()
        job.cancel()

        assertEquals(before, platform.calls)
        assertTrue(platform.managed.single().first.calls.none { it == "close" })
        assertEquals(OutputKind.ANDROID_STANDARD, output.kind)
    }
}
