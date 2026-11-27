package dev.usbdac.sample.player

import androidx.media3.common.DeviceInfo
import androidx.media3.common.ForwardingPlayer
import androidx.media3.common.Player
import androidx.annotation.OptIn
import androidx.media3.common.util.UnstableApi
import dev.usbdac.DacVolume
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.flow.launchIn
import kotlinx.coroutines.flow.map
import kotlinx.coroutines.flow.onEach
import kotlinx.coroutines.flow.flatMapLatest
import kotlinx.coroutines.flow.flowOf
import kotlinx.coroutines.flow.distinctUntilChanged
import kotlinx.coroutines.ExperimentalCoroutinesApi
import java.util.concurrent.CopyOnWriteArrayList
import kotlin.math.roundToInt

// region volume-player
/**
 * Puts the DAC's volume where Media3 looks for the device volume. A session whose player reports a
 * remote device volume gives Android a volume provider, so the system volume panel and the hardware
 * keys (also with the screen off) end up in the methods below. While no DAC volume is open the
 * player behaves as the one it wraps, and the keys change the phone's volume.
 *
 * Create it on the application thread, with the same looper as [player].
 */
@Suppress("DEPRECATION") // the device volume methods without flags are still part of Player
@OptIn(UnstableApi::class)
@kotlin.OptIn(ExperimentalCoroutinesApi::class)
class DacVolumePlayer(
    player: Player,
    private val outputs: DirectOutputController,
    scope: CoroutineScope,
) : ForwardingPlayer(player) {
    private val listeners = CopyOnWriteArrayList<Player.Listener>()
    private var volume: DacVolume? = null

    init {
        // The DAC's volume appears when a direct output opens and goes when it closes.
        outputs.selected
            .map { it?.volume }
            .distinctUntilChanged()
            .onEach { next ->
                volume = next
                listeners.forEach {
                    it.onDeviceInfoChanged(getDeviceInfo())
                    it.onAvailableCommandsChanged(getAvailableCommands())
                    it.onDeviceVolumeChanged(getDeviceVolume(), isDeviceMuted())
                }
            }
            .launchIn(scope)
        // The level changes from this app's slider, a key or the panel: tell the session.
        outputs.selected
            .flatMapLatest { it?.volume?.state ?: flowOf(null) }
            .onEach { listeners.forEach { it.onDeviceVolumeChanged(getDeviceVolume(), isDeviceMuted()) } }
            .launchIn(scope)
    }

    override fun addListener(listener: Player.Listener) {
        listeners.add(listener)
        super.addListener(listener)
    }

    override fun removeListener(listener: Player.Listener) {
        listeners.remove(listener)
        super.removeListener(listener)
    }

    override fun getAvailableCommands(): Player.Commands {
        if (volume == null) return super.getAvailableCommands()
        return super.getAvailableCommands().buildUpon()
            .addAll(
                Player.COMMAND_GET_DEVICE_VOLUME,
                Player.COMMAND_SET_DEVICE_VOLUME,
                Player.COMMAND_SET_DEVICE_VOLUME_WITH_FLAGS,
                Player.COMMAND_ADJUST_DEVICE_VOLUME,
                Player.COMMAND_ADJUST_DEVICE_VOLUME_WITH_FLAGS,
            )
            .build()
    }

    override fun getDeviceInfo(): DeviceInfo {
        if (volume == null) return super.getDeviceInfo()
        return DeviceInfo.Builder(DeviceInfo.PLAYBACK_TYPE_REMOTE).setMinVolume(0).setMaxVolume(MAX).build()
    }

    override fun getDeviceVolume(): Int =
        volume?.let { (it.state.value.fraction * MAX).roundToInt() } ?: super.getDeviceVolume()

    override fun isDeviceMuted(): Boolean = volume?.let { it.state.value.muted == true } ?: super.isDeviceMuted()

    override fun setDeviceVolume(volume: Int) {
        this.volume?.setFraction(volume / MAX.toDouble()) ?: super.setDeviceVolume(volume)
    }

    override fun setDeviceVolume(volume: Int, flags: Int) {
        this.volume?.setFraction(volume / MAX.toDouble()) ?: super.setDeviceVolume(volume, flags)
    }

    override fun increaseDeviceVolume() {
        volume?.step(1) ?: super.increaseDeviceVolume()
    }

    override fun increaseDeviceVolume(flags: Int) {
        volume?.step(1) ?: super.increaseDeviceVolume(flags)
    }

    override fun decreaseDeviceVolume() {
        volume?.step(-1) ?: super.decreaseDeviceVolume()
    }

    override fun decreaseDeviceVolume(flags: Int) {
        volume?.step(-1) ?: super.decreaseDeviceVolume(flags)
    }

    override fun setDeviceMuted(muted: Boolean) {
        val dac = volume
        if (dac == null) super.setDeviceMuted(muted) else if (dac.hasMute) dac.setMute(muted)
    }

    override fun setDeviceMuted(muted: Boolean, flags: Int) {
        val dac = volume
        if (dac == null) super.setDeviceMuted(muted, flags) else if (dac.hasMute) dac.setMute(muted)
    }

    private companion object {
        const val MAX = 100
    }
}
// endregion
