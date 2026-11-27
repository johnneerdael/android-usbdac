package dev.usbdac.validator

import android.content.Context
import android.hardware.usb.UsbDevice
import android.os.Binder
import android.os.Bundle
import android.os.IBinder
import android.os.Process

/** Short-lived Shizuku user service; never exported as an Android service. */
class UsbGrantService : IUsbGrantService.Stub {
    private val ownerUid: Int
    constructor() { ownerUid = -1 }
    constructor(context: Context) {
        require(context.packageName == "dev.usbdac.validator")
        ownerUid = context.applicationInfo.uid
    }

    override fun grant(deviceName: String, deviceId: Int, vendorId: Int, productId: Int): String {
        check(ownerUid >= 0 && Binder.getCallingUid() == ownerUid) { "Validator caller required" }
        check(Process.myUid() == 0 || Process.myUid() == 2000) { "Unsupported helper identity" }
        val identity = Binder.clearCallingIdentity()
        try {
            val binder = Class.forName("android.os.ServiceManager").getMethod("getService", String::class.java).invoke(null, "usb") as? IBinder ?: error("USB service unavailable")
            val iface = Class.forName("android.hardware.usb.IUsbManager")
            val stub = Class.forName("android.hardware.usb.IUsbManager\$Stub")
            val usb = stub.getMethod("asInterface", IBinder::class.java).invoke(null, binder) ?: error("USB interface unavailable")
            val devices = Bundle()
            iface.getMethod("getDeviceList", Bundle::class.java).invoke(usb, devices)
            @Suppress("DEPRECATION") val device = devices.getParcelable<UsbDevice>(deviceName) ?: error("Selected device no longer attached")
            check(device.deviceName == deviceName && device.deviceId == deviceId && device.vendorId == vendorId && device.productId == productId) { "Selected device identity changed" }
            iface.getMethod("grantDevicePermission", UsbDevice::class.java, Int::class.javaPrimitiveType).invoke(usb, device, ownerUid)
            return "requested"
        } finally { Binder.restoreCallingIdentity(identity) }
    }
    override fun destroy() {
        check(Binder.getCallingUid() == ownerUid || Binder.getCallingUid() == 0 || Binder.getCallingUid() == 2000)
        kotlin.system.exitProcess(0)
    }
}
