package dev.usbdac.sample.rust

import android.app.PendingIntent
import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.content.IntentFilter
import android.hardware.usb.UsbConstants
import android.hardware.usb.UsbDevice
import android.hardware.usb.UsbDeviceConnection
import android.hardware.usb.UsbInterface
import android.hardware.usb.UsbManager
import android.os.Build
import java.io.Closeable

// The Kotlin half of a Rust app that uses usbdac-core directly (docs/sdk/guide-rust.md). It uses
// Android's own USB classes only: nothing here calls the usbdac library. A Rust app on Android needs
// this much Kotlin, or the same in Java, because only Java can ask for USB permission and open the device.

// region usb-handoff-permission
/** Asks for USB permission. Call it from an activity that is in front: the dialog needs a screen. */
fun requestUsbPermission(context: Context, device: UsbDevice, onResult: (Boolean) -> Unit) {
    val manager = context.getSystemService(UsbManager::class.java)
    if (manager.hasPermission(device)) return onResult(true)
    val action = "${context.packageName}.USB_PERMISSION"
    val receiver = object : BroadcastReceiver() {
        override fun onReceive(received: Context, intent: Intent) {
            received.unregisterReceiver(this)
            onResult(intent.getBooleanExtra(UsbManager.EXTRA_PERMISSION_GRANTED, false))
        }
    }
    if (Build.VERSION.SDK_INT >= 33) {
        context.registerReceiver(receiver, IntentFilter(action), Context.RECEIVER_NOT_EXPORTED)
    } else {
        @Suppress("UnspecifiedRegisterReceiverFlag")
        context.registerReceiver(receiver, IntentFilter(action))
    }
    // The system adds the result to this intent, so the PendingIntent is mutable and the intent explicit.
    val flags = PendingIntent.FLAG_UPDATE_CURRENT or (if (Build.VERSION.SDK_INT >= 31) PendingIntent.FLAG_MUTABLE else 0)
    manager.requestPermission(device, PendingIntent.getBroadcast(context, 0, Intent(action).setPackage(context.packageName), flags))
}
// endregion

// region usb-handoff
/**
 * An open USB device with its audio interfaces claimed, ready to hand to Rust. Rust gets the file
 * descriptor and the claimed interface numbers; Kotlin keeps the connection open until Rust has closed
 * its `Dac`, and only then calls [close].
 */
class UsbHandoff private constructor(
    private val connection: UsbDeviceConnection,
    private val interfaces: List<UsbInterface>,
    private val claimed: MutableList<UsbInterface>,
) : Closeable {
    /** The usbfs file descriptor. Rust duplicates it, so this one stays ours. */
    val fileDescriptor: Int get() = connection.fileDescriptor

    /** The numbers of the interfaces claimed so far, for `DeviceInfo::with_claimed_interfaces`. */
    val claimedNumbers: IntArray get() = claimed.map { it.id }.distinct().toIntArray()

    /** Rust asks for the optional interfaces a DAC may pace itself with. Returns the numbers it got. */
    fun claimMore(candidates: IntArray): IntArray =
        candidates.filter { number ->
            val iface = interfaces.firstOrNull { it.id == number } ?: return@filter false
            connection.claimInterface(iface, true).also { if (it) claimed += iface }
        }.toIntArray()

    /** Gives the interfaces back, which lets Android's audio driver bind again, and closes the connection. */
    override fun close() {
        claimed.forEach { connection.releaseInterface(it) }
        claimed.clear()
        connection.close()
    }

    companion object {
        /**
         * Opens [device] (which needs USB permission) and claims what a playback DAC needs: every
         * AudioControl interface and every AudioStreaming interface with an isochronous OUT endpoint.
         * Capture-only interfaces stay with Android. Returns null when the device has no such pair.
         * Blocks: call it off the main thread.
         */
        fun open(manager: UsbManager, device: UsbDevice): UsbHandoff? {
            val connection = manager.openDevice(device) ?: return null
            val interfaces = activeInterfaces(device, connection)
            val control = interfaces.filter { it.isAudio(SUBCLASS_CONTROL) }
            val streaming = interfaces.filter { it.isAudio(SUBCLASS_STREAMING) && it.hasIsochronousOut() }
            if (control.isEmpty() || streaming.isEmpty()) {
                connection.close()
                return null
            }
            val claimed = mutableListOf<UsbInterface>()
            for (iface in (control + streaming).distinctBy { it.id }) {
                // force = true detaches Android's USB audio driver from the interface.
                if (!connection.claimInterface(iface, true)) {
                    claimed.forEach { connection.releaseInterface(it) }
                    connection.close()
                    return null
                }
                claimed += iface
            }
            return UsbHandoff(connection, interfaces, claimed)
        }

        private const val SUBCLASS_CONTROL = 1
        private const val SUBCLASS_STREAMING = 2

        private fun UsbInterface.isAudio(subclass: Int) =
            interfaceClass == UsbConstants.USB_CLASS_AUDIO && interfaceSubclass == subclass

        private fun UsbInterface.hasIsochronousOut() = (0 until endpointCount).any {
            getEndpoint(it).let { endpoint ->
                endpoint.direction == UsbConstants.USB_DIR_OUT && endpoint.type == UsbConstants.USB_ENDPOINT_XFER_ISOC
            }
        }

        /** The interfaces of the configuration the device is in: the only one, or the one GET_CONFIGURATION names. */
        private fun activeInterfaces(device: UsbDevice, connection: UsbDeviceConnection): List<UsbInterface> {
            val configurations = (0 until device.configurationCount).map(device::getConfiguration)
            val active = configurations.singleOrNull() ?: run {
                val value = ByteArray(1)
                val read = connection.controlTransfer(UsbConstants.USB_DIR_IN, REQUEST_GET_CONFIGURATION, 0, 0, value, 1, 500)
                configurations.firstOrNull { read == 1 && it.id == (value[0].toInt() and 0xff) } ?: configurations.first()
            }
            return (0 until active.interfaceCount).map(active::getInterface)
        }

        private const val REQUEST_GET_CONFIGURATION = 0x08
    }
}
// endregion
