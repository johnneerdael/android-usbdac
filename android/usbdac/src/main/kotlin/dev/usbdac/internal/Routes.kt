package dev.usbdac.internal

import android.media.AudioDeviceInfo
import dev.usbdac.OutputRoute
import dev.usbdac.RouteKind

// The Android audio device types, sorted into the few kinds the output selector cares about. Pure,
// so the table can be tested without Android.

/** The kind of route an `AudioDeviceInfo.TYPE_*` value is. */
internal fun routeKindOf(type: Int): RouteKind = when (type) {
    AudioDeviceInfo.TYPE_USB_DEVICE, AudioDeviceInfo.TYPE_USB_HEADSET -> RouteKind.USB

    AudioDeviceInfo.TYPE_WIRED_HEADSET,
    AudioDeviceInfo.TYPE_WIRED_HEADPHONES,
    AudioDeviceInfo.TYPE_LINE_ANALOG,
    AudioDeviceInfo.TYPE_LINE_DIGITAL,
    AudioDeviceInfo.TYPE_AUX_LINE,
    AudioDeviceInfo.TYPE_DOCK_ANALOG,
    -> RouteKind.WIRED

    AudioDeviceInfo.TYPE_BUILTIN_EARPIECE,
    AudioDeviceInfo.TYPE_BUILTIN_SPEAKER,
    AudioDeviceInfo.TYPE_BUILTIN_SPEAKER_SAFE,
    -> RouteKind.BUILT_IN

    // Bluetooth, hearing aids, and audio sent elsewhere (cast, screen mirroring, a network sink).
    AudioDeviceInfo.TYPE_BLUETOOTH_SCO,
    AudioDeviceInfo.TYPE_BLUETOOTH_A2DP,
    AudioDeviceInfo.TYPE_BLE_HEADSET,
    AudioDeviceInfo.TYPE_BLE_SPEAKER,
    AudioDeviceInfo.TYPE_BLE_BROADCAST,
    AudioDeviceInfo.TYPE_HEARING_AID,
    AudioDeviceInfo.TYPE_REMOTE_SUBMIX,
    AudioDeviceInfo.TYPE_IP,
    -> RouteKind.WIRELESS

    else -> RouteKind.OTHER
}

/** The route for [device], named as the device names itself. */
internal fun routeOf(device: AudioDeviceInfo): OutputRoute {
    val kind = routeKindOf(device.type)
    val name = device.productName?.toString()?.takeIf { it.isNotBlank() } ?: defaultName(kind)
    return OutputRoute(kind, name, device.id, device)
}

private fun defaultName(kind: RouteKind): String = when (kind) {
    RouteKind.USB -> "USB audio device"
    RouteKind.WIRED -> "wired output"
    RouteKind.BUILT_IN -> "built-in speaker"
    RouteKind.WIRELESS -> "wireless output"
    RouteKind.OTHER -> "audio output"
}

/** The order Android has usually preferred when several outputs are connected, most preferred first. */
private val ASSUMED_PRIORITY = listOf(RouteKind.USB, RouteKind.WIRED, RouteKind.WIRELESS, RouteKind.OTHER, RouteKind.BUILT_IN)

/**
 * Where media goes, guessed from the connected outputs, for Android releases that cannot say
 * (before Android 13): a USB device, else a wired one, else a wireless one, else anything else, else
 * the speaker. Android actually lets the last device connected win in some cases, so this can be wrong
 * when a USB DAC and Bluetooth headphones are both connected.
 */
internal fun assumeActiveRoute(connected: List<OutputRoute>): List<OutputRoute> {
    for (kind in ASSUMED_PRIORITY) {
        val first = connected.firstOrNull { it.kind == kind }
        if (first != null) return listOf(first)
    }
    return emptyList()
}

/**
 * Tells real changes from what a freshly registered `AudioDeviceCallback` reports: Android calls
 * `onAudioDevicesAdded` with the devices that are connected already. Seeded with those, it lets
 * through only devices that were not known (an add) or that were (a removal).
 */
internal class RouteTracker(initial: Collection<Int>) {
    private val known = initial.toMutableSet()

    /** The routes among [routes] that were not connected before. */
    fun added(routes: List<OutputRoute>): List<OutputRoute> = routes.filter { known.add(it.id) }

    /** The routes among [routes] that were connected until now. */
    fun removed(routes: List<OutputRoute>): List<OutputRoute> = routes.filter { known.remove(it.id) }
}
