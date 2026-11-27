package dev.usbdac

import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.content.IntentFilter
import android.hardware.usb.UsbDevice
import android.hardware.usb.UsbManager
import android.media.AudioDeviceCallback
import android.media.AudioDeviceInfo
import android.media.AudioManager
import android.os.Build
import android.os.Handler
import android.os.Looper
import java.io.Closeable
import java.util.concurrent.CopyOnWriteArrayList
import java.util.concurrent.atomic.AtomicBoolean

/** A process-local, explicitly owned association between a protected USB attachment and a built-in output.
 * Close when no longer needed; closing invalidates all streams that use this binding. */
public class DownstreamBinding internal constructor(
    private val context: Context,
    private val protectedDac: DacDeviceInfo,
    internal val independent: OutputRoute,
    private val independentDevice: AudioDeviceInfo,
    independentDacs: List<DacDeviceInfo>,
) : Closeable {
    private val manager = context.getSystemService(Context.AUDIO_SERVICE) as AudioManager
    private val usb = context.getSystemService(Context.USB_SERVICE) as UsbManager
    private val live = AtomicBoolean(true)
    private val closed = AtomicBoolean(false)
    private val listeners = CopyOnWriteArrayList<BindingOnceListener>()
    private val usbListeners = CopyOnWriteArrayList<() -> Unit>()
    private val endpoint = RouteKey.of(independentDevice)
    private val identities = UsbBindingIdentities(UsbKey.of(protectedDac.device),
        independentDacs.map { UsbKey.of(it.device) }.toSet())
    private val callback = object : AudioDeviceCallback() {
        override fun onAudioDevicesRemoved(removedDevices: Array<out AudioDeviceInfo>) {
            if (removedDevices.any { RouteKey.of(it) == endpoint }) invalidate()
        }
    }
    private val receiver = object : BroadcastReceiver() {
        @Suppress("DEPRECATION")
        override fun onReceive(context: Context, intent: Intent) {
            if (intent.action != UsbManager.ACTION_USB_DEVICE_DETACHED && intent.action != UsbManager.ACTION_USB_DEVICE_ATTACHED) return
            val device = intent.getParcelableExtra<UsbDevice>(UsbManager.EXTRA_DEVICE) ?: return
            identities.changed(UsbKey.of(device), intent.action == UsbManager.ACTION_USB_DEVICE_DETACHED)
            usbListeners.forEach { runCatching { it() } }
        }
    }

    init {
        manager.registerAudioDeviceCallback(callback, Handler(Looper.getMainLooper()))
        try {
            val filter = IntentFilter(UsbManager.ACTION_USB_DEVICE_DETACHED).apply { addAction(UsbManager.ACTION_USB_DEVICE_ATTACHED) }
            if (Build.VERSION.SDK_INT >= 33) context.registerReceiver(receiver, filter, Context.RECEIVER_EXPORTED)
            else {
                @Suppress("DEPRECATION")
                context.registerReceiver(receiver, filter)
            }
            if (!identities.initialPresent(usb.deviceList.values.map { UsbKey.of(it) }.toSet()) || !isLive()) {
                throw DacException(DacErrorCode.DEVICE_LOST, "binding endpoint is no longer attached")
            }
        } catch (error: Throwable) {
            manager.unregisterAudioDeviceCallback(callback)
            runCatching { context.unregisterReceiver(receiver) }
            throw error
        }
    }

    internal fun isLive(): Boolean {
        if (!live.get()) return false
        val present = runCatching {
            manager.getDevices(AudioManager.GET_DEVICES_OUTPUTS).any { RouteKey.of(it) == endpoint }
        }.getOrDefault(false)
        return present && live.get()
    }

    internal fun classify(dac: DacDeviceInfo): UsbBindingClassification {
        if (!live.get()) return UsbBindingClassification.PROTECTED_ABSENT
        val present = runCatching { usb.deviceList.values.map { UsbKey.of(it) }.toSet() }
            .getOrElse { return UsbBindingClassification.PROTECTED_ABSENT }
        val result = identities.classify(UsbKey.of(dac.device), present)
        return if (live.get()) result else UsbBindingClassification.PROTECTED_ABSENT
    }

    internal fun ensureLive() {
        if (!isLive()) { invalidate(); throw DacException(DacErrorCode.DEVICE_LOST, "bound output route was invalidated") }
    }

    internal fun observe(listener: () -> Unit): Closeable {
        val once = BindingOnceListener(listener)
        listeners.add(once)
        if (!isLive()) invalidate()
        if (!live.get()) once.notifyOnce()
        return Closeable { listeners.remove(once) }
    }

    internal fun observeUsb(listener: () -> Unit): Closeable {
        usbListeners.add(listener)
        listener()
        return Closeable { usbListeners.remove(listener) }
    }

    private fun invalidate() {
        if (live.compareAndSet(true, false)) listeners.forEach { runCatching { it.notifyOnce() } }
    }

    override fun close() {
        if (!closed.compareAndSet(false, true)) return
        invalidate()
        manager.unregisterAudioDeviceCallback(callback)
        runCatching { context.unregisterReceiver(receiver) }
    }

    internal data class RouteKey(val id: Int, val type: Int, val address: String, val sink: Boolean) {
        companion object { fun of(device: AudioDeviceInfo) = RouteKey(device.id, device.type, device.address, device.isSink) }
    }
    internal data class UsbKey(val name: String, val id: Int, val vendor: Int, val product: Int) {
        companion object { fun of(device: UsbDevice) = UsbKey(device.deviceName, device.deviceId, device.vendorId, device.productId) }
    }
}

internal enum class UsbBindingClassification { PROTECTED, DISTINCT, UNKNOWN, PROTECTED_ABSENT }

/** A detach or reattach permanently retires the recorded generation. */
internal class UsbBindingIdentities(
    private val protectedKey: DownstreamBinding.UsbKey,
    private val declaredIndependent: Set<DownstreamBinding.UsbKey>,
) {
    private var protectedGone = false
    private val stale = mutableSetOf<DownstreamBinding.UsbKey>()

    @Synchronized fun changed(key: DownstreamBinding.UsbKey, detached: Boolean) {
        if (key == protectedKey && detached) protectedGone = true
        if (key in declaredIndependent) stale += key
    }

    @Synchronized fun initialPresent(present: Set<DownstreamBinding.UsbKey>): Boolean =
        !protectedGone && protectedKey in present && declaredIndependent.all { it in present && it !in stale }

    @Synchronized fun classify(key: DownstreamBinding.UsbKey, present: Set<DownstreamBinding.UsbKey>): UsbBindingClassification {
        if (protectedKey !in present) protectedGone = true
        stale.addAll(declaredIndependent.filterNot { it in present })
        if (protectedGone) return UsbBindingClassification.PROTECTED_ABSENT
        if (key == protectedKey && key in present) return UsbBindingClassification.PROTECTED
        if (key in declaredIndependent && key !in stale && key in present) return UsbBindingClassification.DISTINCT
        return UsbBindingClassification.UNKNOWN
    }
}

internal class BindingOnceListener(private val listener: () -> Unit) {
    private val invoked = AtomicBoolean(false)
    fun notifyOnce() { if (invoked.compareAndSet(false, true)) listener() }
}
