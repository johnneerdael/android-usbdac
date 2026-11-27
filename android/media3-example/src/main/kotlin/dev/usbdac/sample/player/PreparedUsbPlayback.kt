package dev.usbdac.sample.player

import android.content.Context
import androidx.media3.common.Player
import androidx.media3.session.MediaSession
import dev.usbdac.DacDeviceInfo
import dev.usbdac.DacSession
import dev.usbdac.DacVolume
import dev.usbdac.UsbDac
import dev.usbdac.media3.DacSessionOutputSource
import dev.usbdac.media3.DacVolumePlayer
import dev.usbdac.media3.UsbDacAudioSink
import androidx.media3.exoplayer.DefaultRenderersFactory
import androidx.media3.exoplayer.ExoPlayer
import androidx.media3.exoplayer.audio.AudioSink
import androidx.media3.common.util.UnstableApi
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.withContext

/** Direct-only opt-in path: call after permission, before any track is selected. */
@androidx.annotation.OptIn(UnstableApi::class)
class PreparedUsbPlayback private constructor(
    private val dac: DacSession,
    val player: ExoPlayer,
    val sessionPlayer: Player,
    /** Available immediately, before setMediaItem/prepare opens a stream. */
    val volumes: MutableStateFlow<DacVolume?>,
) {
    /** Release MediaSession first, then call this on the player's application thread. */
    suspend fun close() {
        player.release()
        volumes.value = null
        withContext(Dispatchers.IO) { dac.close() }
    }

    companion object {
        /** The caller obtains USB permission first. Create on the player/session application thread. */
        suspend fun open(context: Context, device: DacDeviceInfo, scope: CoroutineScope): PreparedUsbPlayback {
            val dac = withContext(Dispatchers.IO) { UsbDac.open(context, device) }
            try {
                val volumes = MutableStateFlow(dac.volume)
                var unduckedDb: Double? = null
                val source = DacSessionOutputSource(dac, volumeMultiplier = { multiplier ->
                    dac.volume?.let { volume ->
                        if (multiplier >= 1f) {
                            unduckedDb?.let(volume::setDb)
                            unduckedDb = null
                        } else {
                            val base = unduckedDb ?: volume.state.value.db
                            unduckedDb = base
                            volume.setDb(base + 20 * kotlin.math.log10(multiplier.toDouble().coerceAtLeast(0.001)))
                        }
                    }
                })
                val renderers = object : DefaultRenderersFactory(context) {
                    override fun buildAudioSink(context: Context, enableFloatOutput: Boolean,
                        enableAudioTrackPlaybackParams: Boolean): AudioSink = UsbDacAudioSink(source)
                }
                val player = ExoPlayer.Builder(context, renderers)
                    .setAudioAttributes(androidx.media3.common.AudioAttributes.DEFAULT, true)
                    .setHandleAudioBecomingNoisy(true)
                    .build()
                return PreparedUsbPlayback(dac, player, DacVolumePlayer(player, volumes, scope), volumes)
            } catch (error: Throwable) {
                withContext(kotlinx.coroutines.NonCancellable + Dispatchers.IO) { dac.close() }
                throw error
            }
        }
    }
}
