package dev.usbdac.validator

import android.content.ComponentName
import android.content.Context
import android.content.ServiceConnection
import android.content.pm.PackageManager
import android.hardware.usb.UsbDevice
import android.hardware.usb.UsbManager
import android.os.Handler
import android.os.IBinder
import android.os.Looper
import rikka.shizuku.Shizuku
import java.util.concurrent.Executors
import java.util.concurrent.atomic.AtomicInteger

/** One explicitly requested grant, scoped to the selected attachment and validator UID. */
internal class ShizukuUsbGrant(
    private val app: Context,
    private val device: UsbDevice,
    private val stillSelected: () -> Boolean,
    private val event: (String, String) -> Unit,
    private val completed: (Boolean, String) -> Unit,
) {
    private val main = Handler(Looper.getMainLooper())
    private val executor = Executors.newSingleThreadExecutor()
    private val args = Shizuku.UserServiceArgs(ComponentName(app, UsbGrantService::class.java))
        .daemon(false).tag("validator-usb-grant").processNameSuffix("usb_grant").version(1)
    private var done = false
    private var bound = false
    private val consentCode = nextConsent.incrementAndGet()
    private val timeout = Runnable { finish(false, "Shizuku timed out; retry Android permission or save the archive.", "timed_out") }
    private val listener = object : Shizuku.OnRequestPermissionResultListener {
        override fun onRequestPermissionResult(request: Int, result: Int) {
            if (request == consentCode && !done) {
                Shizuku.removeRequestPermissionResultListener(this)
                if (result == PackageManager.PERMISSION_GRANTED) { event("consent_accepted", "Shizuku"); bind() }
                else finish(false, "Shizuku consent was refused. Retry Android permission or save the archive.", "consent_refused")
            }
        }
    }
    private val connection = object : ServiceConnection {
        override fun onServiceConnected(name: ComponentName, binder: IBinder) {
            if (done) return
            event("service_connected", "Shizuku")
            executor.execute {
                val result = runCatching {
                    check(stillSelected()) { "Selected DAC is no longer active" }
                    val manager = app.getSystemService(Context.USB_SERVICE) as UsbManager
                    val now = manager.deviceList[device.deviceName]
                    check(now != null && now.deviceId == device.deviceId && now.vendorId == device.vendorId && now.productId == device.productId) { "DAC was disconnected or replaced" }
                    IUsbGrantService.Stub.asInterface(binder).grant(device.deviceName, device.deviceId, device.vendorId, device.productId)
                }
                main.post {
                    if (!done) {
                        val granted = result.isSuccess && stillSelected() && (app.getSystemService(Context.USB_SERVICE) as UsbManager).hasPermission(device)
                        if (granted) finish(true, "USB access confirmed through Shizuku.", "os_grant_confirmed")
                        else finish(false, "Shizuku USB grant failed: ${result.exceptionOrNull()?.cause?.message ?: result.exceptionOrNull()?.message ?: "Android did not confirm permission"}. Retry Android permission or save the archive.", "os_grant_failed")
                    }
                }
            }
        }
        override fun onServiceDisconnected(name: ComponentName) { if (!done) finish(false, "Shizuku user service disconnected.", "service_disconnected") }
    }
    fun start() {
        event("requested", "Shizuku")
        main.postDelayed(timeout, 30_000)
        try {
            if (!Shizuku.pingBinder()) { finish(false, "Shizuku service is not connected.", "binder_unavailable"); return }
            event("binder_connected", "Shizuku")
            if (Shizuku.isPreV11() || Shizuku.getVersion() < 13) { finish(false, "Shizuku user service API 13 or newer is required.", "service_api_unsupported"); return }
            if (Shizuku.checkSelfPermission() == PackageManager.PERMISSION_GRANTED) { event("consent_already_granted", "Shizuku"); bind() }
            else if (Shizuku.shouldShowRequestPermissionRationale()) finish(false, "Shizuku access is denied in its settings.", "consent_refused")
            else { event("consent_requested", "Shizuku"); Shizuku.addRequestPermissionResultListener(listener); Shizuku.requestPermission(consentCode) }
        } catch (e: Throwable) { finish(false, "Shizuku connection failed: ${e.message}", "connection_error") }
    }
    private fun bind() {
        try {
            if (!stillSelected()) { finish(false, "The selected DAC is no longer active.", "attachment_changed"); return }
            val uid = Shizuku.getUid()
            event("helper_identity", "uid=$uid")
            if (uid != 0 && uid != 2000) { finish(false, "Shizuku helper UID $uid is unsupported.", "identity_unsupported"); return }
            if (Shizuku.checkRemotePermission("android.permission.MANAGE_USB") != PackageManager.PERMISSION_GRANTED) { finish(false, "Shizuku does not have MANAGE_USB on this phone.", "manage_usb_denied"); return }
            event("manage_usb_granted", "Shizuku")
            bound = true; Shizuku.bindUserService(args, connection)
            event("service_bind_requested", "Shizuku")
        } catch (e: Throwable) { finish(false, "Shizuku user service failed: ${e.message}", "service_error") }
    }
    fun cancel() { if (!done) finish(false, "Shizuku grant cancelled.", "cancelled", false) }
    private fun finish(ok: Boolean, message: String, stage: String, notify: Boolean = true) {
        if (done) return
        done = true; main.removeCallbacks(timeout)
        runCatching { Shizuku.removeRequestPermissionResultListener(listener) }
        if (bound) runCatching { Shizuku.unbindUserService(args, connection, true) }
        executor.shutdownNow(); event(stage, message)
        if (notify) completed(ok, message)
    }
    companion object {
        private val nextConsent = AtomicInteger(46192)
        fun isSupported(): Boolean = runCatching { Shizuku.pingBinder() && !Shizuku.isPreV11() && Shizuku.getVersion() >= 13 }.getOrDefault(false)
    }
}
