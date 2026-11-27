package dev.usbdac

import android.annotation.SuppressLint
import android.content.Context
import android.media.AudioAttributes
import android.media.AudioDeviceInfo
import android.media.AudioFormat
import android.media.AudioManager
import android.media.AudioTrack
import android.os.Build
import dev.usbdac.internal.BitPerfectMixer
import dev.usbdac.internal.HeadPositionTracker
import dev.usbdac.internal.logWarning
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import java.nio.ByteBuffer

/**
 * Plays through Android's own audio stack with an `AudioTrack`, behind the same [PcmOutput]
 * interface as a direct USB [DacStream].
 *
 * Use it for a device whose DAC is internal (a digital audio player), or when the user does not
 * want the USB DAC taken away from other apps. The audio goes to the platform mixer and the
 * system volume applies. What the platform does to it is **not** under the library's control:
 *
 * - On Android 14 (API 34) and later, for a USB device, [open] asks the platform for the
 *   bit-perfect mixer behavior (`AudioManager.setPreferredMixerAttributes` with
 *   `MIXER_BEHAVIOR_BIT_PERFECT`). If the platform accepts, [bitPerfect] is
 *   [BitPerfectState.RequestedViaPlatform]. That is a request the platform accepted; the library
 *   cannot verify that the samples reach the DAC unchanged, and the platform still decides about
 *   other apps' sounds, volume handling and the DAC's own behaviour.
 * - Otherwise [bitPerfect] is [BitPerfectState.Unverifiable]: the track plays at the source's rate and
 *   encoding, which avoids a resampler only if the route supports that format, and the system may
 *   still mix, scale or resample.
 *
 * Nothing here resamples or converts: the track is created at the request's rate and bit depth, and
 * you write PCM in that encoding (see [open]). For an exclusive AAudio stream see [AAudioExclusiveOutput].
 *
 * Position and latency come from `AudioTrack.getPlaybackHeadPosition`: they are best-effort
 * and exclude the platform's and the DAC's own latency. The behaviour of [flush] and [drain] follows
 * the platform's `AudioTrack` and has not been verified on devices.
 */
public class AndroidManagedOutput private constructor(
    private val track: AudioTrack,
    private val frameBytes: Int,
    private val bufferFrames: Long,
    private val startFrames: Long,
    private val cleanup: () -> Unit,
    initial: BitPerfectState,
) : PcmOutput {

    private val lock = Any()
    private val head = HeadPositionTracker()
    private val mutableBitPerfect = MutableStateFlow(initial)
    private var framesWritten = 0L
    private var running = false
    private var userPaused = false
    private var draining = false
    private var closed = false

    override val bitPerfect: StateFlow<BitPerfectState> get() = mutableBitPerfect

    /**
     * Writes whole frames of the PCM encoding the output was opened with. Nonblocking: it takes what
     * fits in the track's buffer. The buffer must not be written to from another thread meanwhile.
     */
    override fun write(buffer: ByteBuffer): Int = synchronized(lock) {
        checkOpen()
        if (draining) throw DacException(DacErrorCode.INVALID_STATE, "the output is drained; flush before writing")
        val room = roomBytes()
        val size = minOf(buffer.remaining(), room) / frameBytes * frameBytes
        if (size <= 0) return 0
        val written = track.write(buffer, size, AudioTrack.WRITE_NON_BLOCKING)
        if (written < 0) throw writeError(written)
        framesWritten += written / frameBytes
        startIfReady()
        written
    }

    override val writableBytes: Int get() = synchronized(lock) { if (closed || draining) 0 else roomBytes() }

    override fun play() {
        synchronized(lock) {
            checkOpen()
            userPaused = false
            start()
        }
    }

    override fun pause() {
        synchronized(lock) {
            checkOpen()
            userPaused = true
            if (running) track.pause()
            running = false
        }
    }

    override fun flush() {
        synchronized(lock) {
            checkOpen()
            // AudioTrack.flush() only works while paused or stopped.
            track.pause()
            track.flush()
            head.reset(track.playbackHeadPosition)
            framesWritten = 0
            running = false
            draining = false
        }
    }

    override fun drain() {
        synchronized(lock) {
            checkOpen()
            draining = true
            if (!userPaused) start()
        }
    }

    override val isDrained: Boolean
        get() = synchronized(lock) { draining && playedFramesLocked() >= framesWritten }

    override val playedFrames: Long get() = synchronized(lock) { if (closed) 0 else playedFramesLocked() }

    override val latencyFrames: Long
        get() = synchronized(lock) { if (closed) 0 else (framesWritten - playedFramesLocked()).coerceAtLeast(0) }

    override fun close() {
        synchronized(lock) {
            if (closed) return
            closed = true
            runCatching { track.stop() }
            track.release()
            runCatching(cleanup).onFailure { logWarning("clearing the mixer attributes failed", it) }
        }
    }

    // ---- helpers (call with `lock` held) ----

    private fun playedFramesLocked(): Long = head.update(track.playbackHeadPosition)

    private fun roomBytes(): Int {
        val queued = (framesWritten - playedFramesLocked()).coerceAtLeast(0)
        return ((bufferFrames - queued).coerceAtLeast(0) * frameBytes).coerceAtMost(Int.MAX_VALUE.toLong()).toInt()
    }

    private fun startIfReady() {
        if (!running && !userPaused && framesWritten - playedFramesLocked() >= startFrames) start()
    }

    private fun start() {
        if (running) return
        track.play()
        running = true
    }

    private fun checkOpen() {
        if (closed) throw DacException(DacErrorCode.INVALID_STATE, "the output is closed")
    }

    private fun writeError(code: Int): DacException = when (code) {
        AudioTrack.ERROR_DEAD_OBJECT ->
            DacException(DacErrorCode.DEVICE_LOST, "the audio route died (the device may have been unplugged)")
        AudioTrack.ERROR_INVALID_OPERATION ->
            DacException(DacErrorCode.INVALID_STATE, "the AudioTrack is not in a state that accepts audio")
        else -> DacException(DacErrorCode.TRANSFER, "AudioTrack.write failed with code $code")
    }

    public companion object {
        /** Depth of the track's buffer, which is also the writable target. */
        private const val BUFFER_MS = 250

        /** Audio buffered before playback starts by itself, as for a direct stream. */
        private const val START_MS = 100

        /** The USB audio output devices Android currently routes to. */
        public fun usbOutputDevices(context: Context): List<AudioDeviceInfo> =
            (context.getSystemService(Context.AUDIO_SERVICE) as AudioManager)
                .getDevices(AudioManager.GET_DEVICES_OUTPUTS)
                .filter(::isUsb)

        private fun isUsb(device: AudioDeviceInfo): Boolean =
            device.type == AudioDeviceInfo.TYPE_USB_DEVICE || device.type == AudioDeviceInfo.TYPE_USB_HEADSET

        /**
         * Opens an `AudioTrack` for [request], optionally routed to [device].
         *
         * Only PCM is supported, at the request's own rate and channel count, in these encodings,
         * which are what you write:
         *
         * | `bitDepth` | Bytes written per sample | Needs |
         * |---|---|---|
         * | 16 | signed 16-bit little-endian | any API |
         * | 24 | packed signed 24-bit little-endian (3 bytes) | Android 12 (API 31) |
         * | 32 | signed 32-bit little-endian | Android 12 (API 31) |
         *
         * [StreamRequest.ratePolicy] is ignored: the track is created at the requested rate and the
         * platform decides what to do if the route cannot run it.
         *
         * On API 34 and later, with a USB [device] and the `MODIFY_AUDIO_SETTINGS` permission (the
         * library's manifest declares it), this requests the bit-perfect mixer behavior for the
         * track's exact format. The request is cleared again on [close].
         *
         * @param device the route to prefer, such as one of [usbOutputDevices]; `null` for the default route.
         * @throws DacException [DacErrorCode.UNSUPPORTED] for DSD, an unsupported depth or channel
         *   count, or a depth the Android version cannot carry; [DacErrorCode.OPEN_FAILED] if the track
         *   could not be created.
         */
        public fun open(context: Context, request: StreamRequest, device: AudioDeviceInfo?): AndroidManagedOutput {
            if (request.encoding != StreamEncoding.PCM) {
                throw DacException(
                    DacErrorCode.UNSUPPORTED,
                    "the managed output plays PCM only; the platform mixer would destroy DoP and native DSD",
                )
            }
            val problem = carryProblem(request, Build.VERSION.SDK_INT)
            if (problem != null) throw DacException(DacErrorCode.UNSUPPORTED, problem)
            val encoding = requireNotNull(encodingFor(request.bitDepth, Build.VERSION.SDK_INT))
            val channelMask = requireNotNull(channelMaskFor(request.channels))
            val format = try {
                AudioFormat.Builder()
                    .setEncoding(encoding)
                    .setSampleRate(request.sampleRate)
                    .setChannelMask(channelMask)
                    .build()
            } catch (error: IllegalArgumentException) {
                throw DacException(DacErrorCode.UNSUPPORTED, "unsupported AudioTrack format: ${error.message}", error)
            }
            val attributes = AudioAttributes.Builder()
                .setUsage(AudioAttributes.USAGE_MEDIA)
                .setContentType(AudioAttributes.CONTENT_TYPE_MUSIC)
                .build()

            val frameBytes = request.channels * request.bitDepth / 8
            val bufferFrames = request.sampleRate.toLong() * BUFFER_MS / 1000
            val minBytes = AudioTrack.getMinBufferSize(request.sampleRate, channelMask, encoding)
            val bufferBytes = maxOf(bufferFrames * frameBytes, minBytes.toLong() * 2).toInt() / frameBytes * frameBytes

            val audioManager = context.getSystemService(Context.AUDIO_SERVICE) as AudioManager
            val (state, undoMixer) = requestBitPerfect(audioManager, attributes, device, format)

            val track = try {
                AudioTrack.Builder()
                    .setAudioAttributes(attributes)
                    .setAudioFormat(format)
                    .setBufferSizeInBytes(bufferBytes)
                    .setTransferMode(AudioTrack.MODE_STREAM)
                    .setPerformanceMode(AudioTrack.PERFORMANCE_MODE_NONE)
                    .build()
            } catch (error: Exception) {
                runCatching(undoMixer)
                throw DacException(DacErrorCode.OPEN_FAILED, "could not create the AudioTrack: ${error.message}", error)
            }
            if (track.state != AudioTrack.STATE_INITIALIZED) {
                track.release()
                runCatching(undoMixer)
                throw DacException(DacErrorCode.OPEN_FAILED, "the AudioTrack did not initialise")
            }
            device?.let { track.preferredDevice = it }

            return AndroidManagedOutput(
                track = track,
                frameBytes = frameBytes,
                bufferFrames = (bufferBytes / frameBytes).toLong(),
                startFrames = request.sampleRate.toLong() * START_MS / 1000,
                cleanup = undoMixer,
                initial = state,
            )
        }

        /**
         * Why an `AudioTrack` on [apiLevel] cannot carry [request], or `null` if it can. A pure check
         * of the same tables [open] uses, so a caller can ask without creating a track.
         */
        internal fun carryProblem(request: StreamRequest, apiLevel: Int): String? = when {
            request.encoding != StreamEncoding.PCM ->
                "the managed output plays PCM only; the platform mixer would destroy DoP and native DSD"
            request.sampleRate !in 4_000..192_000 ->
                "${request.sampleRate} Hz is outside the AudioTrack PCM range (4000 to 192000 Hz)"
            encodingFor(request.bitDepth, apiLevel) == null ->
                "${request.bitDepth}-bit PCM is not available through AudioTrack on API $apiLevel"
            channelMaskFor(request.channels) == null -> "${request.channels} channels are not supported"
            else -> null
        }

        /** The platform format an `AudioTrack` for [request] has, or `null` if [carryProblem] says it cannot be carried. */
        internal fun audioFormatOf(request: StreamRequest, apiLevel: Int): AudioFormat? {
            if (carryProblem(request, apiLevel) != null) return null
            return try {
                AudioFormat.Builder()
                    .setEncoding(requireNotNull(encodingFor(request.bitDepth, apiLevel)))
                    .setSampleRate(request.sampleRate)
                    .setChannelMask(requireNotNull(channelMaskFor(request.channels)))
                    .build()
            } catch (_: IllegalArgumentException) {
                null
            }
        }

        // The guard is [apiLevel] (callers pass `Build.VERSION.SDK_INT`, or a level under test), which lint
        // cannot see; the constants are compile-time and are only chosen on a release that has them.
        @SuppressLint("InlinedApi")
        private fun encodingFor(bitDepth: Int, apiLevel: Int): Int? = when (bitDepth) {
            16 -> AudioFormat.ENCODING_PCM_16BIT
            24 -> if (apiLevel >= Build.VERSION_CODES.S) AudioFormat.ENCODING_PCM_24BIT_PACKED else null
            32 -> if (apiLevel >= Build.VERSION_CODES.S) AudioFormat.ENCODING_PCM_32BIT else null
            else -> null
        }

        private fun channelMaskFor(channels: Int): Int? = when (channels) {
            1 -> AudioFormat.CHANNEL_OUT_MONO
            2 -> AudioFormat.CHANNEL_OUT_STEREO
            4 -> AudioFormat.CHANNEL_OUT_QUAD
            6 -> AudioFormat.CHANNEL_OUT_5POINT1
            8 -> AudioFormat.CHANNEL_OUT_7POINT1_SURROUND
            else -> null
        }

        /**
         * Asks the platform for the bit-perfect mixer behavior where it exists. Returns what to
         * report, and how to undo what was set.
         */
        private fun requestBitPerfect(
            audioManager: AudioManager,
            attributes: AudioAttributes,
            device: AudioDeviceInfo?,
            format: AudioFormat,
        ): Pair<BitPerfectState, () -> Unit> {
            val none: () -> Unit = {}
            if (device == null) {
                return BitPerfectState.Unverifiable("no output device was chosen; the default route decides") to none
            }
            if (!isUsb(device)) {
                return BitPerfectState.Unverifiable(
                    "only USB devices offer a bit-perfect mixer; what an internal DAC does depends on the device",
                ) to none
            }
            if (Build.VERSION.SDK_INT < Build.VERSION_CODES.UPSIDE_DOWN_CAKE) {
                return BitPerfectState.Unverifiable(
                    "the bit-perfect mixer behavior needs Android 14; this is API ${Build.VERSION.SDK_INT}",
                ) to none
            }
            return BitPerfectMixer.request(audioManager, attributes, device, format)
        }
    }
}
