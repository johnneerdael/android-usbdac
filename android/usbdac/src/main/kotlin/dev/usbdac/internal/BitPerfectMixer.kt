package dev.usbdac.internal

import android.annotation.SuppressLint
import android.annotation.TargetApi
import android.media.AudioAttributes
import android.media.AudioDeviceInfo
import android.media.AudioFormat
import android.media.AudioManager
import android.media.AudioMixerAttributes
import dev.usbdac.BitPerfectState

/**
 * The Android 14 bit-perfect mixer request, kept in a class of its own so that nothing on older
 * Android versions has to resolve `AudioMixerAttributes`. Call it only on API 34 or later, with a
 * USB [AudioDeviceInfo].
 */
@SuppressLint("UseRequiresApi") // androidx.annotation is not a dependency; callers check SDK_INT
@TargetApi(34)
internal object BitPerfectMixer {
    /**
     * Looks for the bit-perfect mixer attributes the platform offers for [device] in exactly
     * [format], sets them as the preferred attributes for [attributes], and reads them back.
     * Returns what to report, and how to undo what was set.
     */
    fun request(
        audioManager: AudioManager,
        attributes: AudioAttributes,
        device: AudioDeviceInfo,
        format: AudioFormat,
    ): Pair<BitPerfectState, () -> Unit> {
        val none: () -> Unit = {}
        return try {
            val wanted = find(audioManager, device, format) ?: return BitPerfectState.Unverifiable(
                "the platform offers no bit-perfect mixer attributes for this format on this device",
            ) to none
            if (!audioManager.setPreferredMixerAttributes(attributes, device, wanted)) {
                return BitPerfectState.Unverifiable("the platform refused the bit-perfect request") to none
            }
            val undo: () -> Unit = { audioManager.clearPreferredMixerAttributes(attributes, device) }
            val active = audioManager.getPreferredMixerAttributes(attributes, device)
            if (active?.mixerBehavior == AudioMixerAttributes.MIXER_BEHAVIOR_BIT_PERFECT) {
                BitPerfectState.RequestedViaPlatform(
                    "Android accepted the bit-perfect mixer behavior for this device and format; " +
                        "the library cannot verify it",
                ) to undo
            } else {
                runCatching(undo)
                BitPerfectState.Unverifiable("the platform did not keep the bit-perfect request") to none
            }
        } catch (error: SecurityException) {
            BitPerfectState.Unverifiable("the app lacks the MODIFY_AUDIO_SETTINGS permission") to none
        }
    }

    /**
     * Whether the platform offers the bit-perfect mixer behavior for [device] in exactly [format].
     * It only asks: nothing is set, so it is safe to call to decide between outputs.
     */
    fun supports(audioManager: AudioManager, device: AudioDeviceInfo, format: AudioFormat): Boolean =
        find(audioManager, device, format) != null

    private fun find(audioManager: AudioManager, device: AudioDeviceInfo, format: AudioFormat): AudioMixerAttributes? =
        audioManager.getSupportedMixerAttributes(device).firstOrNull {
            it.mixerBehavior == AudioMixerAttributes.MIXER_BEHAVIOR_BIT_PERFECT &&
                it.format.encoding == format.encoding &&
                it.format.sampleRate == format.sampleRate &&
                it.format.channelMask == format.channelMask
        }
}
