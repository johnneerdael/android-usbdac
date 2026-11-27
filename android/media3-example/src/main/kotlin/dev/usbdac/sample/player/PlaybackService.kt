package dev.usbdac.sample.player

import dev.usbdac.media3.*

import android.content.Intent
import androidx.media3.common.Player
import androidx.media3.exoplayer.ExoPlayer
import androidx.media3.session.MediaSession
import androidx.media3.session.MediaSessionService
import dev.usbdac.DacEvent
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.FlowPreview
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.cancel
import kotlinx.coroutines.flow.debounce
import kotlinx.coroutines.flow.filterIsInstance
import kotlinx.coroutines.flow.launchIn
import kotlinx.coroutines.flow.onEach

// region playback-service
/**
 * The media service of a typical streaming player. The service, not an activity, owns the player, and
 * through the sink the DAC: the music has to go on when the listener swipes the app away.
 */
@OptIn(FlowPreview::class)
class PlaybackService : MediaSessionService() {
    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.Main.immediate)
    private lateinit var outputs: DirectOutputController
    private lateinit var player: ExoPlayer
    private var session: MediaSession? = null

    override fun onCreate() {
        super.onCreate()
        outputs = PlayerModule.outputs(this)
        player = PlayerModule.player(this, outputs)
        session = MediaSession.Builder(this, DacVolumePlayer(player, outputs.volume, scope)).build()

        // A DAC attached, detached or allowed, or a route that came or went. One plug raises several.
        // Nothing moves now: the output is chosen again at the next seek, skip or format change.
        outputs.changes.debounce(500).onEach { outputs.onChanged() }.launchIn(scope)

        // The DAC is gone: pause, say why, and do not carry on through the phone's speaker.
        outputs.events
            .filterIsInstance<DacEvent.DeviceLost>()
            .onEach {
                player.pause()
                outputs.explain("The DAC was unplugged. Playback is paused; reconnect and seek before resuming.")
            }
            .launchIn(scope)
    }

    override fun onGetSession(controllerInfo: MediaSession.ControllerInfo): MediaSession? = session

    override fun onTaskRemoved(rootIntent: Intent?) {
        // Swiped away while paused: nothing is playing, so give the DAC back.
        if (!player.playWhenReady || player.mediaItemCount == 0) stopSelf()
    }

    override fun onDestroy() {
        scope.cancel()
        session?.run {
            release()
            session = null
        }
        player.release() // the playback thread closes the output, which hands the DAC back to Android
        super.onDestroy()
    }
}
// endregion

// region switch-now
/**
 * Applies a changed setting or a newly allowed DAC now instead of at the next track: a seek to where the
 * player already is flushes the sink, and the flush chooses the output again. The listener hears a short gap.
 */
fun switchOutputNow(player: Player, outputs: DirectOutputController) {
    outputs.onChanged()
    player.seekTo(player.currentPosition)
}
// endregion
