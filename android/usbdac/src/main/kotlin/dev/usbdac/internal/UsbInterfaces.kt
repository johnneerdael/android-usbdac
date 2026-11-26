package dev.usbdac.internal

import android.hardware.usb.UsbConfiguration
import android.hardware.usb.UsbConstants
import android.hardware.usb.UsbDevice
import android.hardware.usb.UsbDeviceConnection
import android.hardware.usb.UsbInterface
import dev.usbdac.DacErrorCode
import dev.usbdac.DacException

private const val SUBCLASS_AUDIO_CONTROL = 0x01
private const val SUBCLASS_AUDIO_STREAMING = 0x02

/** The facts about one USB interface (one alternate setting of it) that the claim plan needs. */
internal data class InterfaceInfo(
    val id: Int,
    val alternateSetting: Int,
    val interfaceClass: Int,
    val interfaceSubclass: Int,
    /** Has an isochronous OUT endpoint: it can carry playback. */
    val hasIsochronousOut: Boolean,
) {
    val isAudioControl: Boolean
        get() = interfaceClass == UsbConstants.USB_CLASS_AUDIO && interfaceSubclass == SUBCLASS_AUDIO_CONTROL
    val isAudioStreaming: Boolean
        get() = interfaceClass == UsbConstants.USB_CLASS_AUDIO && interfaceSubclass == SUBCLASS_AUDIO_STREAMING
}

/** Which interface numbers the library needs claimed: AudioControl and playback AudioStreaming. */
internal data class ClaimPlan(val controlIds: List<Int>, val streamingIds: List<Int>) {
    /** Control first, then streaming, without repeats: the order they are claimed in. */
    val allIds: List<Int> get() = (controlIds + streamingIds).distinct()
}

/**
 * Whether the interfaces of a configuration can play audio: an AudioStreaming interface with
 * an alternate setting that has an isochronous OUT endpoint.
 */
internal fun hasPlayback(interfaces: List<InterfaceInfo>): Boolean =
    interfaces.any { it.isAudioStreaming && it.hasIsochronousOut }

/**
 * The interfaces to claim for the active configuration, or `null` when it has no AudioControl
 * interface or no playback AudioStreaming interface.
 *
 * Playback AudioStreaming interfaces are those with at least one alternate setting that has an
 * isochronous OUT endpoint; capture-only interfaces are left to Android.
 */
internal fun planClaims(interfaces: List<InterfaceInfo>): ClaimPlan? {
    val control = interfaces.filter { it.isAudioControl }.map { it.id }.distinct()
    val streaming = interfaces.filter { it.isAudioStreaming && it.hasIsochronousOut }.map { it.id }.distinct()
    if (control.isEmpty() || streaming.isEmpty()) return null
    return ClaimPlan(control, streaming)
}

// ---- Android glue ---------------------------------------------------------------

internal fun UsbInterface.toInfo(): InterfaceInfo =
    InterfaceInfo(
        id = id,
        alternateSetting = alternateSetting,
        interfaceClass = interfaceClass,
        interfaceSubclass = interfaceSubclass,
        hasIsochronousOut = (0 until endpointCount).any { index ->
            val endpoint = getEndpoint(index)
            endpoint.direction == UsbConstants.USB_DIR_OUT && endpoint.type == UsbConstants.USB_ENDPOINT_XFER_ISOC
        },
    )

private fun UsbConfiguration.interfaces(): List<UsbInterface> =
    (0 until interfaceCount).map(::getInterface)

/** Every interface of every configuration. Enough to tell whether a device can play audio at all. */
internal fun UsbDevice.allInterfaces(): List<UsbInterface> =
    (0 until configurationCount).flatMap { getConfiguration(it).interfaces() }

/**
 * The interfaces of the configuration the device is in now. A device with one configuration is
 * simple; with several, interface numbers only mean something inside the selected one, so the
 * selected configuration is asked for with the standard GET_CONFIGURATION request.
 */
internal fun UsbDevice.activeInterfaces(connection: UsbDeviceConnection): List<UsbInterface> {
    val configurations = (0 until configurationCount).map(::getConfiguration)
    configurations.singleOrNull()?.let { return it.interfaces() }
    val value = ByteArray(1)
    val read = connection.controlTransfer(UsbConstants.USB_DIR_IN, REQUEST_GET_CONFIGURATION, 0, 0, value, 1, 500)
    val active = configurations.firstOrNull { read == 1 && it.id == (value[0].toInt() and 0xff) }
        ?: throw DacException(DacErrorCode.CLAIM, "Could not read the active USB configuration of $deviceName.")
    return active.interfaces()
}

private const val REQUEST_GET_CONFIGURATION = 0x08

/**
 * Claims the interfaces the library drives, and gives them back.
 *
 * Android's USB audio driver normally owns a USB DAC. `claimInterface(interface, force = true)`
 * detaches the kernel driver from an interface and claims it for this connection, which is what
 * lets the library talk to the DAC through usbfs. That is done for the AudioControl interface
 * (clock and volume requests) and for every playback AudioStreaming interface.
 *
 * Releasing is the reverse and matters as much: a streaming interface is returned to alternate
 * setting 0 (no isochronous stream, the DAC stops), and `releaseInterface`, unlike closing the
 * connection, also asks Android to rebind its driver, so the DAC is usable by the system again.
 */
internal class ClaimedInterfaces private constructor(
    private val connection: UsbDeviceConnection,
    private val claimed: MutableList<UsbInterface>,
) {
    val ids: List<Int> get() = claimed.map { it.id }

    fun release() {
        for (usbInterface in claimed.asReversed()) {
            runCatching {
                if (usbInterface.interfaceSubclass == SUBCLASS_AUDIO_STREAMING && usbInterface.alternateSetting == 0) {
                    connection.setInterface(usbInterface)
                }
            }
            runCatching { connection.releaseInterface(usbInterface) }
        }
        claimed.clear()
    }

    companion object {
        /** Claims [plan] on [connection], all or none. */
        fun claim(
            connection: UsbDeviceConnection,
            interfaces: List<UsbInterface>,
            plan: ClaimPlan,
            deviceLabel: String,
        ): ClaimedInterfaces {
            val claimed = ClaimedInterfaces(connection, mutableListOf())
            try {
                for (id in plan.allIds) {
                    // Any alternate setting names the interface; alternate 0 is the one to reset to on release.
                    val candidates = interfaces.filter { it.id == id }
                    val target = candidates.firstOrNull { it.alternateSetting == 0 } ?: candidates.first()
                    if (!connection.claimInterface(target, true)) {
                        throw DacException(
                            DacErrorCode.CLAIM,
                            "Could not claim USB audio interface $id on $deviceLabel. If Android keeps the DAC, " +
                                "try enabling \"Disable USB audio routing\" in Developer options and reconnect it.",
                        )
                    }
                    claimed.claimed += target
                }
            } catch (error: Throwable) {
                claimed.release()
                throw error
            }
            return claimed
        }
    }
}
