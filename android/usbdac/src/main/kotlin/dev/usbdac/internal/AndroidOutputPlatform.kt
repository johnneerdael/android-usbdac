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

    override fun openManaged(request: StreamRequest, route: OutputRoute?): PcmOutput =
        AndroidManagedOutput.open(app, request, route?.device)

    override fun probeAAudio(request: StreamRequest, config: AAudioConfig): AAudioOffer =
        AAudioExclusiveOutput.probe(NativeAAudioBackend, Build.VERSION.SDK_INT, request, deviceId = 0, config = config)

    override fun plan(source: StreamRequest, target: StreamFormat): ConversionPlan =
        ConversionPlan.between(source, target, ConversionOptions())

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

    override fun openStream(format: StreamFormat): PcmOutput = session.openStream(format).also { stream = it }

    override fun close() {
        session.close()
    }
}
