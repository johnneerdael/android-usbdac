package dev.usbdac.controls

import android.view.KeyEvent
import dev.usbdac.DacVolume

/** What to do with one key event. [direction] is +1 or -1 for a step, 0 to only swallow the event. */
internal data class KeyDecision(val direction: Int)

/**
 * Decides about one key event without touching the Android classes, so it can be tested.
 * Returns `null` when the event is not for the DAC and the system should have it.
 */
internal fun volumeKeyDecision(keyCode: Int, action: Int, hasVolume: Boolean): KeyDecision? {
    if (!hasVolume) return null
    val direction = when (keyCode) {
        KeyEvent.KEYCODE_VOLUME_UP -> 1
        KeyEvent.KEYCODE_VOLUME_DOWN -> -1
        else -> return null
    }
    // A held key arrives as repeated ACTION_DOWN events; each steps. The release is swallowed too,
    // or the system would see half a key press.
    return KeyDecision(if (action == KeyEvent.ACTION_DOWN) direction else 0)
}

/**
 * Routes the phone's volume keys to the DAC's hardware volume while the activity is in front.
 *
 * Android's volume keys change the system volume, which does not reach a direct USB stream. Call
 * [handle] from `Activity.dispatchKeyEvent`:
 *
 * ```
 * override fun dispatchKeyEvent(event: KeyEvent): Boolean =
 *     DacVolumeKeys.handle(event, session?.volume) || super.dispatchKeyEvent(event)
 * ```
 *
 * For the keys while the app is in the background, or the system volume panel, use [DacVolumeProvider].
 */
public object DacVolumeKeys {
    /**
     * Steps the DAC volume for a volume key and returns `true` (the event is consumed, including
     * key repeats and the release). Returns `false` for any other key, and for every key while
     * [volume] is `null` (no DAC volume), so the system volume keeps working then.
     *
     * @param steps size of one key press: 1/[steps] of the volume curve.
     */
    public fun handle(event: KeyEvent, volume: DacVolume?, steps: Int = DacVolume.DEFAULT_STEPS): Boolean {
        val decision = volumeKeyDecision(event.keyCode, event.action, volume != null) ?: return false
        if (decision.direction != 0) volume?.step(decision.direction, steps)
        return true
    }
}
