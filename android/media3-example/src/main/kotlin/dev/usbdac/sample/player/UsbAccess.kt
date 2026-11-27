package dev.usbdac.sample.player

import dev.usbdac.media3.*

import android.app.Activity
import android.app.KeyguardManager
import android.view.KeyEvent
import dev.usbdac.DacDeviceInfo
import dev.usbdac.DacVolume
import dev.usbdac.OutputDecision
import dev.usbdac.UsbDac
import dev.usbdac.controls.DacVolumeKeys

// region usb-permission
/**
 * Whether the system dialog can be shown now. It needs an app in front that the listener can see: not a
 * service, and not a locked screen, where the dialog would be missed or refused.
 */
fun mayAskForUsbPermission(activity: Activity): Boolean {
    val keyguard = activity.getSystemService(KeyguardManager::class.java)
    return activity.hasWindowFocus() && !keyguard.isKeyguardLocked
}

/**
 * The selector never asks. When it found a DAC it could not use for lack of permission, its decision
 * names it; the activity asks, and the output is chosen again at the next seek or track.
 */
fun askForUsbPermissionIfNeeded(activity: Activity, decision: OutputDecision?, onGranted: (DacDeviceInfo) -> Unit) {
    val dac = decision?.needsUsbPermission ?: return
    if (!mayAskForUsbPermission(activity)) return
    UsbDac.requestPermission(activity, dac) { granted -> if (granted) onGranted(dac) }
}
// endregion

// region volume-keys
/** The phone's volume keys, while an activity of the app is in front. The panel and the other keys go through the session. */
fun handleVolumeKey(event: KeyEvent, volume: DacVolume?): Boolean = DacVolumeKeys.handle(event, volume)
// endregion
