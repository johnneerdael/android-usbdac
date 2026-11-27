package dev.usbdac.sample

import android.content.Context
import android.media.AudioAttributes
import android.media.session.MediaSession
import dev.usbdac.AAudioConfig
import dev.usbdac.AAudioEvent
import dev.usbdac.AAudioExclusiveOutput
import dev.usbdac.AAudioGrant
import dev.usbdac.AndroidManagedOutput
import dev.usbdac.ConversionOptions
import dev.usbdac.ConversionPlan
import dev.usbdac.ConvertingOutput
import dev.usbdac.PcmConverter
import dev.usbdac.RatePolicy
import dev.usbdac.DacConfig
import dev.usbdac.DacException
import dev.usbdac.DacSession
import dev.usbdac.DacStream
import dev.usbdac.DacVolume
import dev.usbdac.OutputChange
import dev.usbdac.OutputDecision
import dev.usbdac.OutputHandoff
import dev.usbdac.OutputPolicy
import dev.usbdac.OutputSelector
import dev.usbdac.PcmOutput
import dev.usbdac.SelectedOutput
import dev.usbdac.StreamFormat
import dev.usbdac.StreamRequest
import dev.usbdac.UsbQueueProfile
import dev.usbdac.controls.DacIdleRelease
import dev.usbdac.controls.DacVolumeProvider
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.FlowPreview
import kotlinx.coroutines.Job
import kotlinx.coroutines.flow.debounce
import kotlinx.coroutines.flow.launchIn
import kotlinx.coroutines.flow.onEach
import java.nio.ByteBuffer

// The code samples of docs/sdk that are not part of the activity. They live here so the build
// compiles them; the guide quotes them. Nothing calls them.

// region auto-policy
/**
 * What a player offers its listener for automatic output: one switch for taking over the DAC (while
 * it plays, other apps lose it), one for converting what the DAC cannot run, and the fixed-volume
 * permission a DAC without a hardware volume needs.
 */
fun automaticPolicy(takeOverDac: Boolean, allowConversion: Boolean, allowFixedVolume: Boolean): OutputPolicy =
    OutputPolicy(
        allowDirectUsb = takeOverDac,
        allowConversion = allowConversion,
        dacConfig = DacConfig(requireHardwareVolume = !allowFixedVolume),
    )
// endregion

// region auto-open
/**
 * Chooses the best output for [request] and opens it. It blocks, because it has to look at the DAC and the
 * route first: call it off the main thread. The result is a [PcmOutput]; closing it closes everything the
 * selector opened for it, the DAC session included.
 */
fun openAutomatic(selector: OutputSelector, request: StreamRequest): SelectedOutput {
    val output = selector.open(request)
    // output.decision says what was chosen and why, output.format is what to write, and for the
    // direct output output.volume is the DAC's hardware volume.
    return output
}
// endregion

// region auto-track-boundary
/** Whether what [output] takes is the format [request] has. */
fun sameFormat(output: SelectedOutput, request: StreamRequest): Boolean =
    output.format.sampleRate == request.sampleRate &&
        output.format.channels == request.channels &&
        output.format.validBits == request.bitDepth

/**
 * At a track boundary: keep writing to the output in use when nothing changed and the next track has the
 * same format; otherwise close it and open again, which chooses afresh. The old output goes first, because
 * the DAC and an exclusive stream can only be held once. Call it off the main thread.
 */
fun outputForNextTrack(selector: OutputSelector, current: SelectedOutput?, next: StreamRequest, changed: Boolean): SelectedOutput {
    if (current != null && !changed && sameFormat(current, next)) return current
    current?.close()
    return selector.open(next)
}
// endregion

// region output-handoff
fun observeHandoff(scope: CoroutineScope, handoff: OutputHandoff, onFailure: (DacException) -> Unit): Job =
    handoff.changes
        .onEach { change ->
            try {
                handoff.handleChange(change)
            } catch (error: DacException) {
                onFailure(error)
            }
        }
        .launchIn(scope)
// endregion

fun lowCpuUsbPolicy(): OutputPolicy =
    OutputPolicy(dacConfig = DacConfig().withUsbQueueProfile(UsbQueueProfile.LOW_CPU))

fun setLiveEq(output: SelectedOutput, active: Boolean) {
    output.stream?.setHostProcessing(active)
}

// region auto-decision
/** What was chosen, who vouches for it, why, and why the better outputs were passed over. */
fun describeDecision(decision: OutputDecision): String = buildString {
    appendLine("${decision.kind} (${decision.guarantee})")
    appendLine(decision.reason)
    decision.conversion?.let { appendLine("Conversion: ${it.description}") }
    // The selector never asks by itself; the app does, then selects again.
    decision.needsUsbPermission?.let { appendLine("${it.productName} needs USB permission to be used directly.") }
    decision.alternativesRejected.forEach { appendLine("Not ${it.kind}: ${it.why}") }
}.trimEnd()
// endregion

// region auto-changes
/**
 * Calls [onChange] when the best output may have changed: a DAC attached, detached or allowed, or an
 * output route that came or went. One plug raises several, so they are debounced. Nothing is switched
 * here; the player looks again at the next track boundary.
 */
@OptIn(FlowPreview::class)
fun reselectOnChange(scope: CoroutineScope, selector: OutputSelector, onChange: (OutputChange) -> Unit): Job =
    selector.changes
        .debounce(500)
        .onEach(onChange)
        .launchIn(scope)
// endregion

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

// region aaudio-output
/**
 * The AAudio exclusive output: a stream that bypasses the app mixer, for a device with an internal
 * DAC (a digital audio player). It plays PCM in the encoding the request names, nothing converted.
 * With the default `requireExclusive = true`, the call throws if the platform grants only a shared stream.
 */
fun openAAudio(context: Context, sampleRate: Int, bitDepth: Int): AAudioExclusiveOutput =
    AAudioExclusiveOutput.open(
        context,
        StreamRequest.pcm(sampleRate, channels = 2, bitDepth = bitDepth),
        device = null, // the platform's default route; or one of AAudioExclusiveOutput.outputDevices(context)
        config = AAudioConfig(requireExclusive = true),
    )
// endregion

// region aaudio-grant
/** What the platform granted, and what can and cannot be said about it. */
fun describeGrant(grant: AAudioGrant): String = buildString {
    appendLine("${grant.sharingMode} at ${grant.sampleRate} Hz, ${grant.channels} channels, ${grant.format}")
    appendLine("burst ${grant.framesPerBurst} frames, buffer ${grant.bufferSizeFrames}/${grant.bufferCapacityFrames} frames")
    appendLine("device ${grant.deviceId}, MMAP ${grant.usesMmap}, hardware ${grant.hardware ?: "not reported"}")
    append(grant.reason)
}
// endregion

// region aaudio-events
/** Reacts to the route going away: stop writing and close; the output cannot be revived. */
fun watchRoute(scope: CoroutineScope, output: AAudioExclusiveOutput, onLost: (String) -> Unit): Job =
    output.events
        .onEach { event -> if (event is AAudioEvent.Disconnected) onLost(event.detail) }
        .launchIn(scope)
// endregion

// region conversion
/**
 * Plays what the DAC refuses: ask for the nearest rate it runs, plan the conversion from what the host
 * has ([source]) to what the DAC runs, and put a converter in front of the stream. Pass
 * [wireBitDepth] to ask the DAC for another depth than the source's (it must have it). **This is not
 * bit-perfect** unless the plan is only a widening: `bitPerfect` on the result says what was done.
 * Call it off the main thread: negotiating and building a resampler take a moment.
 */
fun openConverting(session: DacSession, source: StreamRequest, wireBitDepth: Int = source.bitDepth): ConvertingOutput {
    val wire = source.copy(bitDepth = wireBitDepth, ratePolicy = RatePolicy.NEAREST_SUPPORTED)
    val format = session.negotiate(wire)
    val plan = ConversionPlan.between(source, format, ConversionOptions())
    return ConvertingOutput(session.openStream(format), PcmConverter.create(plan))
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
