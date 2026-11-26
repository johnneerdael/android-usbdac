package dev.usbdac.sample

import android.content.Context
import android.media.AudioAttributes
import android.media.session.MediaSession
import dev.usbdac.AndroidManagedOutput
import dev.usbdac.DacSession
import dev.usbdac.DacStream
import dev.usbdac.DacVolume
import dev.usbdac.PcmOutput
import dev.usbdac.StreamFormat
import dev.usbdac.StreamRequest
import dev.usbdac.controls.DacIdleRelease
import dev.usbdac.controls.DacVolumeProvider
import kotlinx.coroutines.CoroutineScope
import java.nio.ByteBuffer

// The code samples of docs/sdk that are not part of the activity. They live here so the build
// compiles them; the guide quotes them. Nothing calls them.

// region system-volume
/** Makes the system volume panel and the hardware volume keys drive the DAC, also in the background. */
fun attachDacVolume(session: MediaSession, volume: DacVolume, scope: CoroutineScope) {
    session.setPlaybackToRemote(DacVolumeProvider(volume, scope))
}

/** Back to the system volume, for example when the DAC is released. */
fun detachDacVolume(session: MediaSession) {
    session.setPlaybackToLocal(
        AudioAttributes.Builder()
            .setUsage(AudioAttributes.USAGE_MEDIA)
            .setContentType(AudioAttributes.CONTENT_TYPE_MUSIC)
            .build(),
    )
}
// endregion

// region idle-release
/** Release the stream after 30 s of pause; the session and the DAC volume stay. */
fun idleRelease(scope: CoroutineScope, session: DacSession, request: StreamRequest, first: DacStream): DacIdleRelease<DacStream> {
    val idle = DacIdleRelease(
        scope = scope,
        timeoutMs = 30_000,
        // Runs from onResumed() when the stream was released; the host opens it the way it did the first time.
        reopen = { session.openStream(session.negotiate(request)) },
    )
    idle.attach(first)
    return idle
}
// endregion

// region managed-output
/**
 * The Android-managed transport: an AudioTrack through the platform mixer, behind the same
 * [PcmOutput] interface as a direct stream. Write PCM in the encoding the request names:
 * 16-bit little-endian, or packed 24-bit / 32-bit on Android 12 and later.
 */
fun openManaged(context: Context, sampleRate: Int, bitDepth: Int): PcmOutput {
    val usbRoute = AndroidManagedOutput.usbOutputDevices(context).firstOrNull() // null: the default route
    return AndroidManagedOutput.open(context, StreamRequest.pcm(sampleRate, channels = 2, bitDepth = bitDepth), usbRoute)
}
// endregion

// region transport-mapping
/**
 * How a player's transport maps onto a [PcmOutput]. [Source] is whatever decodes audio for the
 * writer thread; the writer holds [lock] while it reads and writes, so a seek cannot interleave.
 */
class Transport(private val output: PcmOutput, private val format: StreamFormat, private val source: Source) {
    interface Source {
        fun seekTo(positionUs: Long)

        /** Reads whole frames in the stream's wire layout, or returns false at the end of the audio. */
        fun read(into: ByteBuffer): Boolean
    }

    private val lock = Any()
    private var startUs = 0L

    /** Audible position: where the source was when the output was last flushed, plus what has played since. */
    fun positionUs(): Long = startUs + output.playedFrames * 1_000_000L / format.counterRateHz

    fun pause() = output.pause() // keep the writer running; it idles when the ring is full
    fun resume() = output.play()

    fun seek(positionUs: Long) = synchronized(lock) {
        source.seekTo(positionUs) // 1. reposition the decoder (and drop any chunk rendered for the old position)
        output.flush() //            2. drop what is buffered; playedFrames restarts at 0
        startUs = positionUs //      3. remember where "frame 0" is now
    }

    fun stop() = output.flush()

    /** The last track ended: let the buffered tail play, then wait for isDrained. */
    fun endOfAudio() = output.drain()
}
// endregion
