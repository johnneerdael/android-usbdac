package dev.usbdac.sample.player

import dev.usbdac.media3.*

import android.content.Context
import androidx.media3.common.AudioAttributes
import androidx.media3.common.C
import androidx.annotation.OptIn
import androidx.media3.common.util.UnstableApi
import androidx.media3.exoplayer.DefaultRenderersFactory
import androidx.media3.exoplayer.ExoPlayer
import androidx.media3.exoplayer.audio.AudioSink
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob

// region player-module
/**
 * Where a typical player builds its objects: a Hilt module, a service locator or plain code. The
 * controller lives as long as the process, because the settings screen reads it too.
 */
object PlayerModule {
    private val musicAttributes = AudioAttributes.Builder()
        .setUsage(C.USAGE_MEDIA)
        .setContentType(C.AUDIO_CONTENT_TYPE_MUSIC)
        .build()

    @Volatile
    private var controller: DirectOutputController? = null

    fun outputs(context: Context, settings: DirectOutputSettings = DirectOutputSettings()): DirectOutputController =
        controller ?: synchronized(this) {
            controller ?: DirectOutputController(
                context.applicationContext,
                settings,
                CoroutineScope(SupervisorJob() + Dispatchers.Default),
            ).also { controller = it }
        }

    @OptIn(UnstableApi::class)
    fun player(context: Context, outputs: DirectOutputController): ExoPlayer {
        val renderers = object : DefaultRenderersFactory(context) {
            // The one change to a standard player: the audio sink. Everything else stays Media3's.
            override fun buildAudioSink(
                context: Context,
                enableFloatOutput: Boolean,
                enableAudioTrackPlaybackParams: Boolean,
            ): AudioSink = UsbDacAudioSink(outputs)
        }
        return ExoPlayer.Builder(context, renderers)
            // Audio focus stays Media3's job: it pauses on a loss and asks the sink to duck (setVolume).
            .setAudioAttributes(musicAttributes, /* handleAudioFocus = */ true)
            // Pauses for outputs that go through Android when headphones are unplugged. A direct stream
            // reports the loss of its DAC itself (DacEvent.DeviceLost).
            .setHandleAudioBecomingNoisy(true)
            .setWakeMode(C.WAKE_MODE_NETWORK) // a streaming player: keep the CPU and Wi-Fi awake while it plays
            .build()
    }
}
// endregion
