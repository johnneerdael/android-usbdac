package dev.usbdac.controls

import android.media.AudioManager
import android.media.VolumeProvider
import dev.usbdac.DacVolume
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.flow.launchIn
import kotlinx.coroutines.flow.onEach
import kotlin.math.roundToInt

/** What a system volume request means for the DAC. */
internal sealed interface RemoteVolumeAction {
    data class Step(val direction: Int) : RemoteVolumeAction
    data class Mute(val muted: Boolean) : RemoteVolumeAction
}

internal const val REMOTE_VOLUME_MAX = 100

internal fun fractionToIndex(fraction: Double): Int =
    if (fraction.isNaN()) 0 else (fraction.coerceIn(0.0, 1.0) * REMOTE_VOLUME_MAX).roundToInt()

internal fun indexToFraction(index: Int): Double = index.coerceIn(0, REMOTE_VOLUME_MAX) / REMOTE_VOLUME_MAX.toDouble()

/**
 * Maps `VolumeProvider.onAdjustVolume`'s direction. Raise and lower step the level; the mute
 * requests apply only when the DAC has a mute; "same" (a panel being shown) does nothing.
 */
internal fun remoteVolumeAction(direction: Int, hasMute: Boolean, muted: Boolean?): RemoteVolumeAction? =
    when (direction) {
        AudioManager.ADJUST_RAISE -> RemoteVolumeAction.Step(1)
        AudioManager.ADJUST_LOWER -> RemoteVolumeAction.Step(-1)
        AudioManager.ADJUST_MUTE -> if (hasMute) RemoteVolumeAction.Mute(true) else null
        AudioManager.ADJUST_UNMUTE -> if (hasMute) RemoteVolumeAction.Mute(false) else null
        AudioManager.ADJUST_TOGGLE_MUTE -> if (hasMute) RemoteVolumeAction.Mute(muted != true) else null
        else -> null
    }

/**
 * A platform [VolumeProvider] that makes the system volume panel and the hardware volume keys
 * drive the DAC while your app plays, even from the background.
 *
 * Attach it to the app's `MediaSession`:
 *
 * ```
 * val provider = DacVolumeProvider(volume, scope)
 * mediaSession.setPlaybackToRemote(provider)
 * // when the DAC is released, go back to the system volume:
 * mediaSession.setPlaybackToLocal(audioAttributes)
 * ```
 *
 * A session has one remote volume; if it also serves something else (a cast target), choose which
 * provider is attached. With `MediaSessionCompat`, subclass `VolumeProviderCompat` the same way
 * (`onSetVolumeTo(index)` calls `volume.setFraction(index / 100.0)`, `onAdjustVolume(direction)` steps
 * by one); the guide has the snippet.
 *
 * The provider uses a 0..100 scale over the DAC's 0..1 curve ([dev.usbdac.VolumeState.fraction]) and keeps
 * the panel in step with the DAC: it collects [DacVolume.state] in [scope], so cancel the scope (or
 * detach the provider) when the session ends.
 */
public class DacVolumeProvider(
    private val volume: DacVolume,
    scope: CoroutineScope,
) : VolumeProvider(VOLUME_CONTROL_ABSOLUTE, REMOTE_VOLUME_MAX, fractionToIndex(volume.state.value.fraction)) {

    init {
        volume.state.onEach { currentVolume = fractionToIndex(it.fraction) }.launchIn(scope)
    }

    override fun onSetVolumeTo(index: Int) {
        volume.setFraction(indexToFraction(index))
    }

    override fun onAdjustVolume(direction: Int) {
        when (val action = remoteVolumeAction(direction, volume.hasMute, volume.state.value.muted)) {
            is RemoteVolumeAction.Step -> volume.step(action.direction)
            is RemoteVolumeAction.Mute -> volume.setMute(action.muted)
            null -> Unit
        }
    }
}
