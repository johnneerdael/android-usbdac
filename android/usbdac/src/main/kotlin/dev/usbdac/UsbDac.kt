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
import dev.usbdac.internal.UsbConfigurationSelection
import dev.usbdac.internal.selectConfiguration
import dev.usbdac.internal.readConfigurationId
import dev.usbdac.internal.readConfigurationDescriptor
import dev.usbdac.internal.parseFallbackDescriptor
import dev.usbdac.internal.legacyFallbackId
import dev.usbdac.internal.logWarning
import dev.usbdac.internal.interfaces
import dev.usbdac.internal.activeConfiguration
import org.json.JSONObject
import dev.usbdac.internal.allInterfaces
import dev.usbdac.internal.deviceInfoJson
import dev.usbdac.internal.hasPlayback
import dev.usbdac.internal.hasCapture
import dev.usbdac.internal.planCaptureClaims
import dev.usbdac.internal.planClaims
import dev.usbdac.internal.toInfo
import dev.usbdac.internal.toNativeJson
import kotlinx.coroutines.channels.awaitClose
import kotlinx.coroutines.flow.Flow
import kotlinx.coroutines.flow.callbackFlow
import dev.usbdac.internal.UsbAttachmentOwners

/**
 * A USB audio device. Playback discovery requires an isochronous OUT interface;
 * [UsbDac.attachedCaptureDevices] also discovers input-only devices.
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
) {
    /** Advertised configurations; audio presence does not imply native format support. */
    public val configurations: List<DacUsbConfiguration> get() = (0 until device.configurationCount).map { index ->
        val configuration = device.getConfiguration(index)
        val interfaces = configuration.interfaces().map { it.toInfo() }
        DacUsbConfiguration(configuration.id, configuration.name, hasPlayback(interfaces), hasCapture(interfaces))
    }
}

/** Whether a request may use the first legacy configuration after a clean descriptor refusal. */
public enum class PlaybackConfigurationFallback { DISABLED, LEGACY_FIRST_IF_DESCRIPTOR_INCOMPATIBLE }

/** A failed fallback open retains its USB resources until explicit hand-back. */
public class DacPlaybackOpenException(
    public val failure: Throwable,
    public val pendingCleanup: PendingUsbConfigurationRestore,
) : Exception("playback open failed; USB configuration hand-back is pending", failure)

/** Owns the connection and its negotiated playback format. */
public class DacPlaybackPreparation internal constructor(
    public val session: DacSession,
    public val format: StreamFormat,
) : AutoCloseable {
    override fun close(): Unit = session.close()
}

/** Explicit configuration choices available before opening a USB connection. */
public data class DacUsbConfiguration(val id: Int, val name: String?, val hasPlayback: Boolean, val hasCapture: Boolean)

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
 * Distinct USB devices can be open concurrently; each attachment has one owner.
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

    /** Attached USB audio inputs, including devices with no playback interface. */
    public fun attachedCaptureDevices(context: Context): List<DacDeviceInfo> =
        usbManager(context).deviceList.values.mapNotNull(::describeCapture).sortedBy { it.deviceName }

    /** Describe an input-capable device; exact PCM support is checked after USB permission. */
    public fun describeCapture(device: UsbDevice): DacDeviceInfo? {
        if (!(0 until device.configurationCount).any { index ->
                hasCapture(device.getConfiguration(index).interfaces().map { it.toInfo() })
            }) return null
        return DacDeviceInfo(device, device.vendorId, device.productId,
            device.productName?.takeIf { it.isNotBlank() } ?: "USB audio input",
            device.manufacturerName?.takeIf { it.isNotBlank() }, device.deviceName)
    }

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

    /** Key actual framework attachment identity before touching claims/configuration. */
    private val sessionOwners = UsbAttachmentOwners()

    /**
     * Open explicit standalone USB input after USB permission through [requestPermission]. Android
     * applies microphone privacy and device permission policy. Declare/request RECORD_AUDIO when
     * the app's other microphone APIs or its platform policy require it; this raw USB path adds no
     * blanket microphone permission gate and requests no permissions automatically.
     * Pass an advertised `configurationId` to select it explicitly before claims; null keeps
     * the active configuration. Selection and restoration require GET_CONFIGURATION readback.
     * Native descriptor preflight claims only supported PCM IN interfaces and their owning
     * AudioControl interfaces in the active configuration. Call off the main thread.
     */
    public fun openCapture(context: Context, device: DacDeviceInfo, config: DacConfig = DacConfig()): DacCaptureSession =
        openCapture(context, device, config, configurationId = null)

    /** Select only [configurationId], or keep the active configuration when null. */
    public fun openCapture(context: Context, device: DacDeviceInfo, config: DacConfig = DacConfig(), configurationId: Int?): DacCaptureSession =
        openCaptureDevice(context, device, config, configurationId, null)

    /** Reserved for Scarlett capture routing; this release returns [DacErrorCode.UNSUPPORTED]. */
    public fun openCaptureWithVendorRouting(context: Context, device: DacDeviceInfo, vendorRouting: VendorRouting,
        config: DacConfig = DacConfig(), configurationId: Int? = null): DacCaptureSession =
        openCaptureDevice(context, device, config, configurationId, vendorRouting)

    private fun openCaptureDevice(context: Context, device: DacDeviceInfo, config: DacConfig, configurationId: Int?, vendorRouting: VendorRouting?): DacCaptureSession {
        val app = context.applicationContext
        val manager = usbManager(app)
        if (!manager.hasPermission(device.device)) {
            throw DacException(DacErrorCode.PERMISSION_DENIED, "no USB permission for ${device.productName}")
        }
        val ownership = sessionOwners.acquire(device.device.deviceId, device.device.deviceName)
            ?: throw DacException(DacErrorCode.BUSY, "this USB attachment already has a session")
        var connection: android.hardware.usb.UsbDeviceConnection? = null
        var claims: ClaimedInterfaces? = null
        var selection: UsbConfigurationSelection? = null
        var nativeDac = 0L
        var nativeRegistrationAttempted = false
        try {
            val opened = manager.openDevice(device.device)
                ?: throw DacException(DacErrorCode.OPEN_FAILED, "Android could not open ${device.productName}")
            connection = opened
            val fd = opened.fileDescriptor
            if (fd < 0) throw DacException(DacErrorCode.OPEN_FAILED, "Android gave no capture file descriptor")
            val selected = device.device.selectConfiguration(opened, configurationId)
            selection = selected
            val active = selected.selected
            val interfaces = active.interfaces()
            fun infoJson(ids: List<Int>, configuration: Int) = deviceInfoJson(
                device.vendorId, device.productId, device.productName, device.manufacturerName,
                device.deviceName, ids, configurationId = configuration,
            )
            val preflight = nativeCall { JSONObject(Native.dacCapturePreflight(fd, infoJson(emptyList(), active.id))) }
            val configuration = preflight.getInt("configuration_id")
            if (configuration != active.id || device.device.activeConfiguration(opened).id != configuration) {
                throw DacException(DacErrorCode.CLAIM, "active configuration changed during capture preflight")
            }
            fun ids(key: String): List<Int> = preflight.getJSONArray(key).let { array ->
                (0 until array.length()).map(array::getInt)
            }
            val plan = planCaptureClaims(interfaces.map { it.toInfo() }, ids("control_interfaces"), ids("input_interfaces"))
                ?: throw DacException(DacErrorCode.CLAIM, "capture preflight does not match Android's active interfaces")
            val vendorInterface = vendorRoutingInterface(vendorRouting, fd, device, active.id, interfaces)
            val owned = ClaimedInterfaces.claim(opened, interfaces,
                dev.usbdac.internal.ClaimPlan(plan.controlIds + listOfNotNull(vendorInterface), plan.streamingIds), device.productName)
            claims = owned
            if (device.device.activeConfiguration(opened).id != configuration) {
                throw DacException(DacErrorCode.CLAIM, "active configuration changed while claiming capture")
            }
            selected.verify()
            nativeRegistrationAttempted = true
            nativeDac = nativeCall { Native.dacOpenCaptureDevice(fd, infoJson(owned.ids, configuration), config.toNativeJson()) }
            return DacCaptureSession(nativeDac, device, opened, owned, selected) { ownership.close() }
        } catch (error: Throwable) {
            if (nativeDac != 0L) runCatching { Native.dacClose(nativeDac) }
            val retainedClaims = claims
            val retainedConnection = connection
            val retainedSelection = selection
            fun release() {
                retainedClaims?.release()
                runCatching { retainedSelection?.restore() }.onFailure {
                    error.addSuppressed(it)
                    logWarning("restoring USB configuration after open failed", it)
                }
                runCatching { retainedConnection?.close() }
                ownership.close()
            }
            if (!nativeRegistrationAttempted || error is LinkageError || runCatching { Native.dacAwaitConnectionRetirement(retainedConnection?.fileDescriptor ?: -1) }.getOrDefault(false)) release()
            else Thread({
                while (!runCatching { Native.dacAwaitConnectionRetirement(retainedConnection?.fileDescriptor ?: -1) }.getOrDefault(false)) runCatching { Thread.sleep(50) }
                release()
            }, "usbdac-capture-open-retirement").start()
            if (error is SecurityException) throw DacException(DacErrorCode.PERMISSION_DENIED, "Android denied USB capture permission", error)
            throw error
        }
    }

    /**
     * Opens [device] for direct output. An explicit `configurationId` selects only that
     * advertised configuration and restores the original after native retirement on close.
     * Null preserves the active configuration and existing claim behavior.
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
    public fun open(context: Context, device: DacDeviceInfo, config: DacConfig = DacConfig()): DacSession =
        open(context, device, config, configurationId = null)

    /**
     * Open and negotiate [request] under one owning session. When [fallback] is enabled,
     * an exact request may use the first legacy configuration only if complete descriptors prove
     * the active full-UAC3 configuration cannot carry the exact request. Explicit configuration
     * IDs remain authoritative. Close the result to retire native work and restore the configuration.
     * Descriptor reads and the selection transaction use one retained connection and file descriptor;
     * the live configuration is checked before selection. The caller's device snapshot may already
     * be stale before opening, and matching framework identity does not prove a detach/replug ABA.
     */
    public fun openPlaybackForRequest(
        context: Context, device: DacDeviceInfo, request: StreamRequest,
        config: DacConfig = DacConfig(), configurationId: Int? = null,
        fallback: PlaybackConfigurationFallback = PlaybackConfigurationFallback.DISABLED,
    ): DacPlaybackPreparation {
        require(request.ratePolicy == RatePolicy.EXACT) { "openPlaybackForRequest requires an exact request" }
        val usedFallback = booleanArrayOf(false)
        val session = openDevice(context, device, config, configurationId, duplex = false,
            fallbackRequest = if (fallback == PlaybackConfigurationFallback.LEGACY_FIRST_IF_DESCRIPTOR_INCOMPATIBLE) request else null, fallbackUsed = usedFallback)
        try {
            return DacPlaybackPreparation(session, session.negotiate(request))
        } catch (error: Throwable) {
            if (usedFallback[0]) {
                error.addSuppressed(DacException(DacErrorCode.UNSUPPORTED, "active full UAC3 descriptor excludes the exact request"))
                val manager = usbManager(context.applicationContext)
                throw DacPlaybackOpenException(error, session.closeForFailedLegacyOpen {
                    val current = manager.deviceList[device.device.deviceName]
                    current != null && current.deviceId == device.device.deviceId && current.vendorId == device.vendorId &&
                        current.productId == device.productId && manager.hasPermission(current)
                })
            }
            try { session.close() } catch (close: Throwable) { error.addSuppressed(close) }
            throw error
        }
    }

    /** Select only [configurationId], or keep the active configuration when null. */
    public fun open(context: Context, device: DacDeviceInfo, config: DacConfig = DacConfig(), configurationId: Int?): DacSession =
        openDevice(context, device, config, configurationId, duplex = false)

    /**
     * Explicitly claim playback plus native-supported PCM input in one configuration transaction.
     * USB capture remains subject to Android's permission and microphone privacy policy.
     * Open audio with [DacSession.openDuplex] after negotiating the output format.
     */
    public fun openDuplex(context: Context, device: DacDeviceInfo, config: DacConfig = DacConfig(), configurationId: Int? = null): DacSession =
        openDevice(context, device, config, configurationId, duplex = true)

    /** Reserved for vendor-aware playback; this release returns [DacErrorCode.UNSUPPORTED]. */
    public fun openWithVendorRouting(context: Context, device: DacDeviceInfo, vendorRouting: VendorRouting,
        config: DacConfig = DacConfig(), configurationId: Int? = null): DacSession =
        openDevice(context, device, config, configurationId, duplex = false, vendorRouting = vendorRouting)

    /** Reserved for vendor-aware duplex; this release returns [DacErrorCode.UNSUPPORTED]. */
    public fun openDuplexWithVendorRouting(context: Context, device: DacDeviceInfo, vendorRouting: VendorRouting,
        config: DacConfig = DacConfig(), configurationId: Int? = null): DacSession =
        openDevice(context, device, config, configurationId, duplex = true, vendorRouting = vendorRouting)

    private fun openDevice(context: Context, device: DacDeviceInfo, config: DacConfig, configurationId: Int?, duplex: Boolean, vendorRouting: VendorRouting? = null, fallbackRequest: StreamRequest? = null, fallbackUsed: BooleanArray? = null): DacSession {
        val app = context.applicationContext
        val manager = usbManager(app)
        if (!manager.hasPermission(device.device)) {
            throw DacException(DacErrorCode.PERMISSION_DENIED, "no USB permission for ${device.productName}")
        }
        val ownership = sessionOwners.acquire(device.device.deviceId, device.device.deviceName)
            ?: throw DacException(DacErrorCode.BUSY, "this USB attachment already has a session")
        var connection: android.hardware.usb.UsbDeviceConnection? = null
        var claims: ClaimedInterfaces? = null
        var selection: UsbConfigurationSelection? = null
        var nativeDac = 0L
        var nativeRegistrationAttempted = false
        try {
            connection = manager.openDevice(device.device)
                ?: throw DacException(DacErrorCode.OPEN_FAILED, "Android could not open ${device.productName}")
            val fd = connection.fileDescriptor
            if (fd < 0) throw DacException(DacErrorCode.OPEN_FAILED, "Android gave no file descriptor for ${device.productName}")

            var requestedConfiguration = configurationId
            if (fallbackRequest != null && configurationId == null) {
                val usb = device.device
                if (usb.configurationCount !in 1..255) throw DacException(DacErrorCode.CONFIGURATION_SELECTION_FAILED, "invalid configuration inventory")
                val attachedBefore = manager.deviceList[usb.deviceName]
                if (attachedBefore == null || attachedBefore.deviceId != usb.deviceId || attachedBefore.vendorId != usb.vendorId || attachedBefore.productId != usb.productId || !manager.hasPermission(attachedBefore)) {
                    throw DacException(DacErrorCode.CONFIGURATION_SELECTION_FAILED, "USB attachment changed before descriptor discovery")
                }
                val advertised = (0 until usb.configurationCount).map { usb.getConfiguration(it).id }
                if (advertised.any { it !in 1..255 } || advertised.distinct().size != advertised.size) {
                    throw DacException(DacErrorCode.CONFIGURATION_SELECTION_FAILED, "configuration values are not uniquely advertised")
                }
                val activeId = usb.readConfigurationId(connection)
                val activeIndex = advertised.indexOf(activeId)
                if (activeIndex < 0) throw DacException(DacErrorCode.CONFIGURATION_SELECTION_FAILED, "active configuration is not advertised")
                val deadline = android.os.SystemClock.elapsedRealtime() + 10_000
                val descriptors = (0 until usb.configurationCount).map { index ->
                    if (android.os.SystemClock.elapsedRealtime() >= deadline) throw DacException(DacErrorCode.CONFIGURATION_SELECTION_FAILED, "configuration discovery deadline expired")
                    val descriptor = parseFallbackDescriptor(readConfigurationDescriptor(connection, index), fallbackRequest)
                    if (descriptor == null || descriptor.id != advertised[index]) throw DacException(DacErrorCode.CONFIGURATION_SELECTION_FAILED, "configuration descriptor identity or completeness could not be verified")
                    descriptor
                }
                if (android.os.SystemClock.elapsedRealtime() >= deadline) throw DacException(DacErrorCode.CONFIGURATION_SELECTION_FAILED, "configuration discovery deadline expired")
                val active = descriptors[activeIndex]
                val first = descriptors[0]
                val candidate = legacyFallbackId(active, first, advertised)
                if (candidate != null) {
                    val attached = manager.deviceList[usb.deviceName]
                    if (attached == null || attached.deviceId != usb.deviceId || attached.vendorId != usb.vendorId || attached.productId != usb.productId || !manager.hasPermission(attached) || usb.readConfigurationId(connection) != activeId) {
                        throw DacException(DacErrorCode.CONFIGURATION_SELECTION_FAILED, "USB attachment or configuration changed before fallback")
                    }
                    requestedConfiguration = candidate
                    fallbackUsed?.set(0, true)
                }
            }
            val selected = device.device.selectConfiguration(connection, requestedConfiguration)
            selection = selected
            val activeConfiguration = selected.selected
            val interfaces = activeConfiguration.interfaces()
            val playbackPlan = planClaims(interfaces.map { it.toInfo() })
                ?: throw DacException(
                    DacErrorCode.UNSUPPORTED,
                    "${device.productName} has no AudioControl interface together with a playback AudioStreaming interface",
                )
            val plan = if (duplex) {
                val preflightInfo = deviceInfoJson(device.vendorId, device.productId, device.productName,
                    device.manufacturerName, device.deviceName, emptyList(), configurationId = activeConfiguration.id)
                val preflight = JSONObject(nativeCall { Native.dacCapturePreflight(fd, preflightInfo) })
                if (preflight.getInt("configuration_id") != activeConfiguration.id) {
                    throw DacException(DacErrorCode.CLAIM, "active configuration changed during duplex preflight")
                }
                fun ids(key: String): List<Int> = preflight.getJSONArray(key).let { values ->
                    (0 until values.length()).map(values::getInt)
                }
                val capture = planCaptureClaims(interfaces.map { it.toInfo() }, ids("control_interfaces"), ids("input_interfaces"))
                    ?: throw DacException(DacErrorCode.CLAIM, "duplex capture proof does not match active interfaces")
                dev.usbdac.internal.ClaimPlan((playbackPlan.controlIds + capture.controlIds).distinct(),
                    (playbackPlan.streamingIds + capture.streamingIds).distinct())
            } else playbackPlan
            val vendorInterface = vendorRoutingInterface(vendorRouting, fd, device, activeConfiguration.id, interfaces)
            val ownedClaims = ClaimedInterfaces.claim(connection, interfaces,
                dev.usbdac.internal.ClaimPlan(plan.controlIds + listOfNotNull(vendorInterface), plan.streamingIds), device.productName)
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
            var nativeConfigurationId: Int? = activeConfiguration.id
            var unavailable: List<Int> = emptyList()
            if (!duplex && !config.disableFeedback) {
                val preflight = JSONObject(nativeCall { Native.dacFeedbackPreflight(fd, infoJson()) })
                nativeConfigurationId = preflight.getInt("configuration_id")
                if (activeConfiguration.id != nativeConfigurationId || device.device.activeConfiguration(connection).id != nativeConfigurationId) {
                    throw DacException(DacErrorCode.CLAIM, "The active USB configuration changed during feedback preflight.")
                }
                val sources = preflight.getJSONArray("interfaces")
                unavailable = ownedClaims.claimOptional(interfaces, (0 until sources.length()).map(sources::getInt))
            }
            selected.verify()
            nativeRegistrationAttempted = true
            val dac = nativeCall {
                if (duplex) Native.dacOpenDuplexDevice(fd, infoJson(nativeConfigurationId), config.toNativeJson())
                else Native.dacOpen(fd, infoJson(nativeConfigurationId, unavailable), config.toNativeJson())
            }
            nativeDac = dac
            return DacSession.create(dac, device, config, connection, ownedClaims, selected, duplexEnabled = duplex) { ownership.close() }
        } catch (error: Throwable) {
            if (fallbackUsed?.get(0) == true) {
                error.addSuppressed(DacException(DacErrorCode.UNSUPPORTED, "active full UAC3 descriptor excludes the exact request"))
            }
            if (nativeDac != 0L) runCatching { Native.dacClose(nativeDac) }
            val retainedClaims = claims
            val retainedConnection = connection
            val retainedSelection = selection
            fun release() {
                retainedClaims?.release()
                runCatching { retainedSelection?.restore() }.onFailure {
                    error.addSuppressed(it)
                    logWarning("restoring USB configuration after open failed", it)
                }
                runCatching { retainedConnection?.close() }
                ownership.close()
            }
            if (fallbackUsed?.get(0) == true && nativeRegistrationAttempted) {
                val fd = retainedConnection?.fileDescriptor ?: -1
                val pending = PendingUsbConfigurationRestore(
                    checkReady = { runCatching { Native.dacAwaitConnectionRetirement(fd) }.getOrDefault(false) },
                    finishRetired = {
                        try {
                            val attached = manager.deviceList[device.device.deviceName]
                            if (attached == null || attached.deviceId != device.device.deviceId || attached.vendorId != device.vendorId || attached.productId != device.productId || !manager.hasPermission(attached)) {
                                throw DacException(DacErrorCode.DEVICE_LOST, "USB attachment lost before configuration hand-back")
                            }
                            retainedClaims?.releaseChecked()
                            val stillAttached = manager.deviceList[device.device.deviceName]
                            if (stillAttached == null || stillAttached.deviceId != device.device.deviceId ||
                                stillAttached.vendorId != device.vendorId || stillAttached.productId != device.productId || !manager.hasPermission(stillAttached)) {
                                throw DacException(DacErrorCode.DEVICE_LOST, "USB attachment lost before configuration restoration")
                            }
                            retainedSelection?.restore()
                        } finally {
                            runCatching { retainedConnection?.close() }
                            ownership.close()
                        }
                    },
                )
                throw DacPlaybackOpenException(error, pending)
            }
            // Open can fail after native registration; the native retirement gate is
            // authoritative even when no public handle was returned.
            if (!nativeRegistrationAttempted || error is LinkageError || runCatching { Native.dacAwaitConnectionRetirement(retainedConnection?.fileDescriptor ?: -1) }.getOrDefault(false)) release()
            else Thread({
                while (!runCatching { Native.dacAwaitConnectionRetirement(retainedConnection?.fileDescriptor ?: -1) }.getOrDefault(false)) runCatching { Thread.sleep(50) }
                release()
            }, "usbdac-open-retirement").start()
            if (error is SecurityException) throw DacException(DacErrorCode.PERMISSION_DENIED, "Android denied USB audio permission", error)
            throw error
        }
    }

    private fun usbManager(context: Context): UsbManager =
        context.getSystemService(Context.USB_SERVICE) as UsbManager

    private fun vendorRoutingInterface(optIn: VendorRouting?, fd: Int, device: DacDeviceInfo,
        configurationId: Int, interfaces: List<android.hardware.usb.UsbInterface>): Int? {
        if (optIn == null) return null
        val info = deviceInfoJson(device.vendorId, device.productId, device.productName,
            device.manufacturerName, device.deviceName, emptyList(), configurationId = configurationId)
        val proof = JSONObject(nativeCall { Native.scarlettPreflight(fd, info) }).optJSONObject("capability")
            ?: throw DacException(DacErrorCode.UNSUPPORTED, "no exact native-proven Scarlett vendor interface")
        val capability = parseScarlettCapability(proof)
        if (capability.configurationId != configurationId || capability.model.name != optIn.name ||
            interfaces.none { it.id == capability.interfaceId && it.alternateSetting == 0 && it.interfaceClass == 255 }) {
            throw DacException(DacErrorCode.CLAIM, "Scarlett preflight differs from the selected Android configuration")
        }
        return capability.interfaceId
    }

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
