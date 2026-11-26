package dev.usbdac

import android.annotation.SuppressLint
import android.app.PendingIntent
import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.content.IntentFilter
import android.hardware.usb.UsbDevice
import android.hardware.usb.UsbManager
import android.os.Build
import dev.usbdac.internal.ClaimedInterfaces
import dev.usbdac.internal.Native
import dev.usbdac.internal.interfaces
import dev.usbdac.internal.activeConfiguration
import org.json.JSONObject
import dev.usbdac.internal.allInterfaces
import dev.usbdac.internal.deviceInfoJson
import dev.usbdac.internal.hasPlayback
import dev.usbdac.internal.planClaims
import dev.usbdac.internal.toInfo
import dev.usbdac.internal.toNativeJson
import kotlinx.coroutines.channels.awaitClose
import kotlinx.coroutines.flow.Flow
import kotlinx.coroutines.flow.callbackFlow
import java.util.concurrent.atomic.AtomicBoolean

/**
 * A USB device that can play audio: it has an Audio Streaming interface with an isochronous
 * OUT endpoint. Get one from [UsbDac.attachedDacs] or [UsbDac.describe].
 */
public data class DacDeviceInfo(
    /** The Android device, for APIs this SDK does not wrap. */
    val device: UsbDevice,
    val vendorId: Int,
    val productId: Int,
    /** The product name, or "USB DAC" when the device reports none. */
    val productName: String,
    val manufacturerName: String?,
    /** The system's name for the device, such as `/dev/bus/usb/001/004`. Unique while attached. */
    val deviceName: String,
)

/** A change among the attached DACs. See [UsbDac.hotplug]. */
public sealed interface DacHotplugEvent {
    /** The DAC the event is about. */
    public val device: DacDeviceInfo

    /** A DAC was plugged in. It has no USB permission yet unless the user granted it permanently. */
    public data class Attached(override val device: DacDeviceInfo) : DacHotplugEvent

    /** A DAC was unplugged. An open [DacSession] on it reports [DacEvent.DeviceLost]. */
    public data class Detached(override val device: DacDeviceInfo) : DacHotplugEvent

    /** The user granted USB permission for the DAC. */
    public data class PermissionGranted(override val device: DacDeviceInfo) : DacHotplugEvent
}

/**
 * Entry point: find a USB DAC, get permission and open it.
 *
 * ```
 * val dac = UsbDac.attachedDacs(context).first()
 * UsbDac.requestPermission(context, dac) { granted ->
 *     if (granted) thread { val session = UsbDac.open(context, dac) ... }
 * }
 * ```
 *
 * Only one [DacSession] can be open in a process (the native library's limit in version 0.1).
 */
public object UsbDac {

    /** The library version, from the native library (`major.minor.patch`). Loads `libusbdac.so`. */
    public val nativeVersion: String get() = Native.version()

    /**
     * Routes the native library's logging to logcat under the tag `usbdac`.
     *
     * @param level 0 off, 1 error, 2 warn, 3 info, 4 debug, 5 trace.
     */
    public fun setLogLevel(level: Int) {
        Native.setLogLevel(level)
    }

    /** The USB devices attached now that can play audio. */
    public fun attachedDacs(context: Context): List<DacDeviceInfo> =
        usbManager(context).deviceList.values.mapNotNull(::describe).sortedBy { it.deviceName }

    /**
     * Describes [device] if it can play audio, otherwise returns `null`. Use it on the device an
     * `ACTION_USB_DEVICE_ATTACHED` intent carries.
     */
    public fun describe(device: UsbDevice): DacDeviceInfo? {
        if (!hasPlayback(device.allInterfaces().map { it.toInfo() })) return null
        return DacDeviceInfo(
            device = device,
            vendorId = device.vendorId,
            productId = device.productId,
            productName = device.productName?.takeIf { it.isNotBlank() } ?: "USB DAC",
            manufacturerName = device.manufacturerName?.takeIf { it.isNotBlank() },
            deviceName = device.deviceName,
        )
    }

    /** Whether the app may open [device] now. */
    public fun hasPermission(context: Context, device: DacDeviceInfo): Boolean =
        usbManager(context).hasPermission(device.device)

    /**
     * Asks the user for USB permission for [device]; the system shows its own dialog.
     * [onResult] runs on the main thread, with `true` at once if the permission is already held.
     */
    public fun requestPermission(context: Context, device: DacDeviceInfo, onResult: (granted: Boolean) -> Unit) {
        val app = context.applicationContext
        val manager = usbManager(app)
        if (manager.hasPermission(device.device)) {
            onResult(true)
            return
        }
        val action = permissionAction(app)
        val receiver = object : BroadcastReceiver() {
            override fun onReceive(context: Context, intent: Intent) {
                if (intent.action != action || intent.usbDevice()?.deviceName != device.deviceName) return
                app.unregisterReceiver(this)
                onResult(intent.getBooleanExtra(UsbManager.EXTRA_PERMISSION_GRANTED, false))
            }
        }
        registerNotExported(app, receiver, IntentFilter(action))
        manager.requestPermission(device.device, permissionIntent(app, action))
    }

    /**
     * DACs coming and going, as a cold flow: collecting registers a receiver, cancelling removes it.
     * Events are delivered on the main thread. [DacHotplugEvent.PermissionGranted] reports a grant
     * from any [requestPermission] call or from the system dialog; the device's `ACTION_USB_DEVICE_ATTACHED`
     * intent filter in your manifest is not needed for this flow.
     */
    public fun hotplug(context: Context): Flow<DacHotplugEvent> = callbackFlow {
        val app = context.applicationContext
        val action = permissionAction(app)
        val receiver = object : BroadcastReceiver() {
            override fun onReceive(context: Context, intent: Intent) {
                val dac = intent.usbDevice()?.let(::describe) ?: return
                when (intent.action) {
                    UsbManager.ACTION_USB_DEVICE_ATTACHED -> trySend(DacHotplugEvent.Attached(dac))
                    UsbManager.ACTION_USB_DEVICE_DETACHED -> trySend(DacHotplugEvent.Detached(dac))
                    action -> if (intent.getBooleanExtra(UsbManager.EXTRA_PERMISSION_GRANTED, false)) {
                        trySend(DacHotplugEvent.PermissionGranted(dac))
                    }
                }
            }
        }
        val filter = IntentFilter().apply {
            addAction(UsbManager.ACTION_USB_DEVICE_ATTACHED)
            addAction(UsbManager.ACTION_USB_DEVICE_DETACHED)
            addAction(action)
        }
        registerNotExported(app, receiver, filter)
        awaitClose { app.unregisterReceiver(receiver) }
    }

    /** One session per process: the native library refuses a second one, and so do we, before touching USB. */
    private val sessionOpen = AtomicBoolean(false)

    /**
     * Opens [device] for direct output.
     *
     * This opens the USB connection, takes the audio interfaces away from Android's own USB audio
     * driver, hands the file descriptor to the native library, which scans the descriptors and
     * finds the volume control. It does not touch the clock and starts no audio. It blocks for
     * tens of milliseconds or more: **call it off the main thread**.
     *
     * Android's system audio stops using the DAC while the session is open. The session keeps the
     * USB connection until [DacSession.close].
     *
     * @throws DacException [DacErrorCode.PERMISSION_DENIED] without USB permission,
     *   [DacErrorCode.BUSY] if a session is already open, [DacErrorCode.CLAIM] if the interfaces
     *   could not be taken, and the native library's other codes.
     */
    public fun open(context: Context, device: DacDeviceInfo, config: DacConfig = DacConfig()): DacSession {
        val app = context.applicationContext
        val manager = usbManager(app)
        if (!manager.hasPermission(device.device)) {
            throw DacException(DacErrorCode.PERMISSION_DENIED, "no USB permission for ${device.productName}")
        }
        if (!sessionOpen.compareAndSet(false, true)) {
            throw DacException(DacErrorCode.BUSY, "another DAC session is open in this process")
        }
        var connection: android.hardware.usb.UsbDeviceConnection? = null
        var claims: ClaimedInterfaces? = null
        try {
            connection = manager.openDevice(device.device)
                ?: throw DacException(DacErrorCode.OPEN_FAILED, "Android could not open ${device.productName}")
            val fd = connection.fileDescriptor
            if (fd < 0) throw DacException(DacErrorCode.OPEN_FAILED, "Android gave no file descriptor for ${device.productName}")

            val activeConfiguration = device.device.activeConfiguration(connection)
            val interfaces = activeConfiguration.interfaces()
            val plan = planClaims(interfaces.map { it.toInfo() })
                ?: throw DacException(
                    DacErrorCode.UNSUPPORTED,
                    "${device.productName} has no AudioControl interface together with a playback AudioStreaming interface",
                )
            val ownedClaims = ClaimedInterfaces.claim(connection, interfaces, plan, device.productName)
            claims = ownedClaims

            fun infoJson(configurationId: Int? = null, unavailable: List<Int> = emptyList()) = deviceInfoJson(
                vendorId = device.vendorId,
                productId = device.productId,
                productName = device.productName,
                manufacturerName = device.manufacturerName,
                deviceName = device.deviceName,
                claimedInterfaces = ownedClaims.ids,
                configurationId = configurationId,
                unavailableFeedbackInterfaces = unavailable,
            )
            var configurationId: Int? = activeConfiguration.id
            var unavailable: List<Int> = emptyList()
            if (!config.disableFeedback) {
                val preflight = JSONObject(nativeCall { Native.dacFeedbackPreflight(fd, infoJson()) })
                configurationId = preflight.getInt("configuration_id")
                if (activeConfiguration.id != configurationId || device.device.activeConfiguration(connection).id != configurationId) {
                    throw DacException(DacErrorCode.CLAIM, "The active USB configuration changed during feedback preflight.")
                }
                val sources = preflight.getJSONArray("interfaces")
                unavailable = ownedClaims.claimOptional(interfaces, (0 until sources.length()).map(sources::getInt))
            }
            val dac = nativeCall { Native.dacOpen(fd, infoJson(configurationId, unavailable), config.toNativeJson()) }
            val session = try {
                DacSession.create(dac, device, config, connection, claims) { sessionOpen.set(false) }
            } catch (error: Throwable) {
                runCatching { Native.dacClose(dac) }
                throw error
            }
            return session
        } catch (error: Throwable) {
            claims?.release()
            connection?.close()
            sessionOpen.set(false)
            throw error
        }
    }

    private fun usbManager(context: Context): UsbManager =
        context.getSystemService(Context.USB_SERVICE) as UsbManager

    private fun permissionAction(context: Context): String = "${context.packageName}.usbdac.USB_PERMISSION"

    private fun permissionIntent(context: Context, action: String): PendingIntent {
        // The system adds the device and the result to this intent, so the PendingIntent must be
        // mutable; the intent is explicit (package set), which Android 14 requires for that.
        val flags = PendingIntent.FLAG_UPDATE_CURRENT or
            (if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) PendingIntent.FLAG_MUTABLE else 0)
        return PendingIntent.getBroadcast(context, 0, Intent(action).setPackage(context.packageName), flags)
    }

    // The export flag exists from Android 13; before that, receivers registered at run time are not
    // exported to other apps unless the intent filter says so, and the system's own broadcasts arrive.
    @SuppressLint("UnspecifiedRegisterReceiverFlag")
    private fun registerNotExported(context: Context, receiver: BroadcastReceiver, filter: IntentFilter) {
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
            context.registerReceiver(receiver, filter, Context.RECEIVER_NOT_EXPORTED)
        } else {
            context.registerReceiver(receiver, filter)
        }
    }

    private fun Intent.usbDevice(): UsbDevice? =
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
            getParcelableExtra(UsbManager.EXTRA_DEVICE, UsbDevice::class.java)
        } else {
            @Suppress("DEPRECATION")
            getParcelableExtra(UsbManager.EXTRA_DEVICE)
        }
}
