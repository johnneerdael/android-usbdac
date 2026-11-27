package dev.usbdac.internal

import android.content.Context
import android.media.AudioAttributes
import android.media.AudioDeviceCallback
import android.media.AudioDeviceInfo
import android.media.AudioManager
import android.os.Build
import android.os.Handler
import android.os.Looper
import dev.usbdac.AAudioConfig
import dev.usbdac.AAudioExclusiveOutput
import dev.usbdac.AndroidManagedOutput
import dev.usbdac.ConversionOptions
import dev.usbdac.ConversionPlan
import dev.usbdac.ConvertingOutput
import dev.usbdac.DacCapabilities
import dev.usbdac.DacConfig
import dev.usbdac.DacDeviceInfo
import dev.usbdac.DacHotplugEvent
import dev.usbdac.DacSession
import dev.usbdac.DacStream
import dev.usbdac.DacVolume
import dev.usbdac.DownstreamBinding
import dev.usbdac.DacException
import dev.usbdac.DacErrorCode
import dev.usbdac.RouteKind
import dev.usbdac.OutputChange
import dev.usbdac.OutputRoute
import dev.usbdac.PcmConverter
import dev.usbdac.PcmOutput
import dev.usbdac.StreamFormat
import dev.usbdac.StreamRequest
import dev.usbdac.UsbDac
import kotlinx.coroutines.channels.awaitClose
import kotlinx.coroutines.flow.Flow
import kotlinx.coroutines.flow.callbackFlow

/** The output selector's view of the real device: the library's own entry points and the platform's `AudioManager`. */
internal class AndroidOutputPlatform(context: Context) : OutputPlatform {
    private val app: Context = context.applicationContext

    private val audioManager: AudioManager get() = app.getSystemService(Context.AUDIO_SERVICE) as AudioManager

    override val apiLevel: Int get() = Build.VERSION.SDK_INT

    override fun attachedDacs(): List<DacDeviceInfo> = UsbDac.attachedDacs(app)

    override fun hasPermission(dac: DacDeviceInfo): Boolean = UsbDac.hasPermission(app, dac)

    override fun openDac(dac: DacDeviceInfo, config: DacConfig): DirectLink = SessionLink(UsbDac.open(app, dac, config))

    override fun activeRoute(): List<OutputRoute> {
        val manager = audioManager
        // Android 13 can say which devices media would go to; earlier releases cannot.
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
            val routed = runCatching { manager.getAudioDevicesForAttributes(MEDIA) }.getOrNull().orEmpty()
            if (routed.isNotEmpty()) return routed.map(::routeOf)
        }
        return assumeActiveRoute(manager.getDevices(AudioManager.GET_DEVICES_OUTPUTS).map(::routeOf))
    }

    override fun supportsBitPerfect(route: OutputRoute, request: StreamRequest): Boolean {
        if (Build.VERSION.SDK_INT < Build.VERSION_CODES.UPSIDE_DOWN_CAKE) return false
        val device = route.device ?: return false
        val format = AndroidManagedOutput.audioFormatOf(request, Build.VERSION.SDK_INT) ?: return false
        return runCatching { BitPerfectMixer.supports(audioManager, device, format) }.getOrDefault(false)
    }

    override fun rankRates(requested: Int, rates: List<Int>): List<Int> {
        Native.version() // Ensure libusbdac is loaded before the pure JNI entry point.
        return NativeRateRanking.rankList(requested, rates)
    }

    override fun openManaged(request: StreamRequest, route: OutputRoute?): PcmOutput =
        AndroidManagedOutput.open(app, request, route?.device)

    override fun probeAAudio(request: StreamRequest, config: AAudioConfig): AAudioOffer =
        AAudioExclusiveOutput.probe(NativeAAudioBackend, Build.VERSION.SDK_INT, request, deviceId = 0, config = config)

    override fun probeAAudio(request: StreamRequest, config: AAudioConfig, route: OutputRoute): AAudioOffer =
        AAudioExclusiveOutput.probe(NativeAAudioBackend, Build.VERSION.SDK_INT, request, route.id, config)

    override fun bindDownstream(dac: DacDeviceInfo, protectedRoute: OutputRoute?, independentRoute: OutputRoute, independentDacs: List<DacDeviceInfo>): DownstreamBinding {
        if (Build.VERSION.SDK_INT < 28 || independentRoute.kind != RouteKind.BUILT_IN) {
            throw DacException(DacErrorCode.UNSUPPORTED, "a built-in output on API 28 or later is required")
        }
        val device = independentRoute.device ?: throw DacException(DacErrorCode.INVALID_ARGUMENT, "route lacks platform identity")
        if (!device.isSink || device.id != independentRoute.id || device.type !in listOf(AudioDeviceInfo.TYPE_BUILTIN_SPEAKER, AudioDeviceInfo.TYPE_BUILTIN_EARPIECE)) {
            throw DacException(DacErrorCode.INVALID_ARGUMENT, "independent route must be a built-in sink")
        }
        if (protectedRoute != null) {
            val protectedDevice = protectedRoute.device
            if (protectedRoute.kind != RouteKind.USB || protectedDevice == null || !protectedDevice.isSink ||
                protectedDevice.id != protectedRoute.id || protectedDevice.type !in listOf(
                    AudioDeviceInfo.TYPE_USB_DEVICE, AudioDeviceInfo.TYPE_USB_HEADSET, AudioDeviceInfo.TYPE_USB_ACCESSORY) ||
                audioManager.getDevices(AudioManager.GET_DEVICES_OUTPUTS).none {
                    it.id == protectedDevice.id && it.type == protectedDevice.type && it.address == protectedDevice.address && it.isSink
                }) throw DacException(DacErrorCode.INVALID_ARGUMENT, "protected route is not a current USB sink")
        }
        if (attachedDacs().none { it.deviceName == dac.deviceName && it.device.deviceId == dac.device.deviceId &&
                it.vendorId == dac.vendorId && it.productId == dac.productId }) {
            throw DacException(DacErrorCode.DEVICE_LOST, "protected USB attachment is absent")
        }
        val attached = attachedDacs()
        fun same(a: DacDeviceInfo, b: DacDeviceInfo): Boolean = a.deviceName == b.deviceName &&
            a.device.deviceId == b.device.deviceId && a.vendorId == b.vendorId && a.productId == b.productId
        if (independentDacs.any { declared -> same(declared, dac) || attached.none { same(it, declared) } }) {
            throw DacException(DacErrorCode.INVALID_ARGUMENT, "independent DACs must be distinct current USB attachments")
        }
        return DownstreamBinding(app, dac, independentRoute, device, independentDacs)
    }

    override fun plan(source: StreamRequest, target: StreamFormat): ConversionPlan =
        ConversionPlan.between(source, target, ConversionOptions())

    override fun plan(source: StreamRequest, target: StreamFormat, options: ConversionOptions): ConversionPlan =
        ConversionPlan.between(source, target, options)

    override fun converting(target: PcmOutput, plan: ConversionPlan): PcmOutput =
        ConvertingOutput(target, PcmConverter.create(plan))

    override fun hotplug(): Flow<DacHotplugEvent> = UsbDac.hotplug(app)

    override fun routeChanges(): Flow<OutputChange.RouteChanged> = callbackFlow {
        val manager = audioManager
        // Registering reports every device that is connected already; only real changes are passed on.
        val tracker = RouteTracker(manager.getDevices(AudioManager.GET_DEVICES_OUTPUTS).map { it.id })
        val callback = object : AudioDeviceCallback() {
            override fun onAudioDevicesAdded(addedDevices: Array<out AudioDeviceInfo>) {
                val fresh = tracker.added(addedDevices.filter { it.isSink }.map(::routeOf))
                if (fresh.isNotEmpty()) trySend(OutputChange.RouteChanged(added = fresh, removed = emptyList()))
            }

            override fun onAudioDevicesRemoved(removedDevices: Array<out AudioDeviceInfo>) {
                val gone = tracker.removed(removedDevices.filter { it.isSink }.map(::routeOf))
                if (gone.isNotEmpty()) trySend(OutputChange.RouteChanged(added = emptyList(), removed = gone))
            }
        }
        manager.registerAudioDeviceCallback(callback, Handler(Looper.getMainLooper()))
        awaitClose { manager.unregisterAudioDeviceCallback(callback) }
    }

    private companion object {
        val MEDIA: AudioAttributes = AudioAttributes.Builder()
            .setUsage(AudioAttributes.USAGE_MEDIA)
            .setContentType(AudioAttributes.CONTENT_TYPE_MUSIC)
            .build()
    }
}

/** A real [DacSession] behind [DirectLink]. */
internal class SessionLink(override val session: DacSession) : DirectLink {
    override val dac: DacDeviceInfo get() = session.info
    override val capabilities: DacCapabilities get() = session.capabilities
    override val hasUsableVolume: Boolean get() = session.volume != null
    override val volume: DacVolume? get() = session.volume

    override var stream: DacStream? = null
        private set

    override fun negotiate(request: StreamRequest): StreamFormat = session.negotiate(request)

    override fun negotiateCandidates(requests: List<StreamRequest>): StreamFormat = session.negotiateCandidates(requests)

    override fun openStream(format: StreamFormat): PcmOutput = session.openStream(format).also { stream = it }

    override fun close() {
        session.close()
    }
}
