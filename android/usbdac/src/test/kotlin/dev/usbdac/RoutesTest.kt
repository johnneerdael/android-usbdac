package dev.usbdac

import android.media.AudioDeviceInfo
import dev.usbdac.internal.RouteTracker
import dev.usbdac.internal.assumeActiveRoute
import dev.usbdac.internal.routeKindOf
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test

class RoutesTest {
    @Test
    fun usbDevicesAreUsb() {
        assertEquals(RouteKind.USB, routeKindOf(AudioDeviceInfo.TYPE_USB_DEVICE))
        assertEquals(RouteKind.USB, routeKindOf(AudioDeviceInfo.TYPE_USB_HEADSET))
    }

    @Test
    fun bluetoothHearingAidsAndCastAreWireless() {
        for (type in listOf(
            AudioDeviceInfo.TYPE_BLUETOOTH_A2DP,
            AudioDeviceInfo.TYPE_BLUETOOTH_SCO,
            AudioDeviceInfo.TYPE_BLE_HEADSET,
            AudioDeviceInfo.TYPE_BLE_SPEAKER,
            AudioDeviceInfo.TYPE_BLE_BROADCAST,
            AudioDeviceInfo.TYPE_HEARING_AID,
            AudioDeviceInfo.TYPE_REMOTE_SUBMIX,
            AudioDeviceInfo.TYPE_IP,
        )) {
            assertEquals("type $type", RouteKind.WIRELESS, routeKindOf(type))
        }
    }

    @Test
    fun jacksAndSpeakersAreTheDevicesOwnDac() {
        for (type in listOf(AudioDeviceInfo.TYPE_WIRED_HEADPHONES, AudioDeviceInfo.TYPE_WIRED_HEADSET, AudioDeviceInfo.TYPE_LINE_ANALOG, AudioDeviceInfo.TYPE_AUX_LINE)) {
            assertEquals("type $type", RouteKind.WIRED, routeKindOf(type))
        }
        for (type in listOf(AudioDeviceInfo.TYPE_BUILTIN_SPEAKER, AudioDeviceInfo.TYPE_BUILTIN_EARPIECE)) {
            assertEquals("type $type", RouteKind.BUILT_IN, routeKindOf(type))
        }
    }

    @Test
    fun hdmiDocksAndTheUnknownAreOther() {
        for (type in listOf(AudioDeviceInfo.TYPE_HDMI, AudioDeviceInfo.TYPE_HDMI_ARC, AudioDeviceInfo.TYPE_DOCK, AudioDeviceInfo.TYPE_BUS, AudioDeviceInfo.TYPE_UNKNOWN, 9999)) {
            assertEquals("type $type", RouteKind.OTHER, routeKindOf(type))
        }
    }

    @Test
    fun beforeAndroid13TheRouteIsGuessedFromWhatIsConnected() {
        val all = listOf(SPEAKER_ROUTE, BLUETOOTH_ROUTE, HEADPHONES_ROUTE, USB_ROUTE)
        assertEquals(listOf(USB_ROUTE), assumeActiveRoute(all))
        assertEquals(listOf(HEADPHONES_ROUTE), assumeActiveRoute(all - USB_ROUTE))
        assertEquals(listOf(BLUETOOTH_ROUTE), assumeActiveRoute(listOf(SPEAKER_ROUTE, BLUETOOTH_ROUTE)))
        assertEquals(listOf(SPEAKER_ROUTE), assumeActiveRoute(listOf(SPEAKER_ROUTE)))
        assertTrue(assumeActiveRoute(emptyList()).isEmpty())
    }

    @Test
    fun aFreshlyRegisteredCallbackDoesNotReportWhatWasAlreadyConnected() {
        val tracker = RouteTracker(listOf(SPEAKER_ROUTE.id, USB_ROUTE.id))
        assertTrue("Android reports the connected devices on registration", tracker.added(listOf(SPEAKER_ROUTE, USB_ROUTE)).isEmpty())
        assertEquals(listOf(BLUETOOTH_ROUTE), tracker.added(listOf(BLUETOOTH_ROUTE, SPEAKER_ROUTE)))
        assertTrue("already counted", tracker.added(listOf(BLUETOOTH_ROUTE)).isEmpty())
    }

    @Test
    fun aRemovalIsReportedOnceAndAReturnIsANewAddition() {
        val tracker = RouteTracker(listOf(USB_ROUTE.id))
        assertEquals(listOf(USB_ROUTE), tracker.removed(listOf(USB_ROUTE)))
        assertTrue(tracker.removed(listOf(USB_ROUTE)).isEmpty())
        assertEquals(listOf(USB_ROUTE), tracker.added(listOf(USB_ROUTE)))
    }
}
