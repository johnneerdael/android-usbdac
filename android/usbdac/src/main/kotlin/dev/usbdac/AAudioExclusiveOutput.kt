package dev.usbdac

import android.content.Context
import android.media.AudioDeviceInfo
import android.media.AudioManager
import android.os.Build
import dev.usbdac.internal.AAudioBackend
import dev.usbdac.internal.NativeAAudioBackend
import dev.usbdac.internal.NativeListener
import dev.usbdac.internal.aaudioRequestJson
import dev.usbdac.internal.parseAAudioGrant
import kotlinx.coroutines.channels.BufferOverflow
import kotlinx.coroutines.flow.MutableSharedFlow
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.SharedFlow
import kotlinx.coroutines.flow.StateFlow
import java.nio.ByteBuffer
import java.util.concurrent.atomic.AtomicBoolean

/** How the platform schedules an AAudio stream. */
public enum class AAudioPerformanceMode(internal val wireName: String) {
    /** No latency target. The platform may then choose its non-exclusive path. */
    NONE("none"),

    /** Favours battery over latency. */
    POWER_SAVING("power_saving"),

    /** The mode exclusive (MMAP) streams are normally granted in. The default. */
    LOW_LATENCY("low_latency"),
}

/** Whether the stream has the device to itself. */
public enum class AAudioSharingMode {
    /** The app mixer is bypassed; no other app's audio is mixed into the stream. */
    EXCLUSIVE,

    /** The stream goes through the system mixer (volume, effects, mixing, sample rate conversion). */
    SHARED,
}

/** The sample layouts [AAudioExclusiveOutput] carries without converting anything. */
public enum class AAudioSampleFormat(internal val wireName: String, public val bytesPerSample: Int) {
    /** Signed 16-bit little-endian. Any Android release with AAudio. */
    PCM_16("i16", 2),

    /** Signed 24-bit little-endian in 3 bytes. Android 12 (API 31) and later. */
    PCM_24_PACKED("i24", 3),

    /** Signed 32-bit little-endian. Android 12 (API 31) and later. */
    PCM_32("i32", 4),

    /** 32-bit IEEE float. Any Android release with AAudio. */
    FLOAT_32("float", 4),
    ;

    internal companion object {
        fun fromWireName(name: String?): AAudioSampleFormat? = entries.firstOrNull { it.wireName == name }
    }
}

/**
 * Options of [AAudioExclusiveOutput.open]. The defaults are what the library was designed with.
 *
 * @property requireExclusive Fail with [DacErrorCode.EXCLUSIVE_UNAVAILABLE] when the platform grants
 *   only a shared stream. On (the default), a stream that comes back is exclusive; off, it may be
 *   shared and [AAudioExclusiveOutput.bitPerfect] says so.
 * @property floatSamples Carry 32-bit IEEE float instead of 32-bit integers when
 *   [StreamRequest.bitDepth] is 32. Rejected for other depths.
 * @property performanceMode How the platform schedules the stream. Exclusive streams are
 *   normally granted in [AAudioPerformanceMode.LOW_LATENCY] only.
 * @property bufferTargetMs The depth [PcmOutput.writableBytes] fills the output's buffer to.
 * @property bufferCapacityMs The capacity of that buffer: [PcmOutput.write] never accepts more.
 * @property startThresholdMs Audio the buffer must hold before playback starts, and before it
 *   resumes after a flush or an underrun. Between 1 and [bufferTargetMs].
 * @property deviceBufferBursts The size of the device-side buffer in bursts (a burst is
 *   [AAudioGrant.framesPerBurst] frames), or `null` for the platform's default (usually 2).
 *   A larger buffer rides out scheduling hiccups at the price of latency.
 */
public data class AAudioConfig(
    val requireExclusive: Boolean = true,
    val floatSamples: Boolean = false,
    val performanceMode: AAudioPerformanceMode = AAudioPerformanceMode.LOW_LATENCY,
    val bufferTargetMs: Int = 250,
    val bufferCapacityMs: Int = 500,
    val startThresholdMs: Int = 100,
    val deviceBufferBursts: Int? = null,
)

/** What the hardware behind an AAudio stream runs at, reported by Android 14 (API 34) and later. */
public data class AAudioHardware(
    val sampleRate: Int,
    val channels: Int,
    /** `null` when AAudio cannot name the hardware's format. */
    val format: AAudioSampleFormat?,
)

/**
 * What the platform actually granted for an AAudio stream, read back after opening.
 *
 * @property sharingMode Exclusive, or shared (the system mixer is in the path).
 * @property sampleRate The stream's rate in Hz.
 * @property channels The stream's channel count.
 * @property format The stream's sample layout, or `null` for one this library does not carry.
 * @property framesPerBurst The device's burst size in frames.
 * @property bufferCapacityFrames The device-side buffer's capacity in frames.
 * @property bufferSizeFrames The device-side buffer's current size in frames.
 * @property deviceId The `AudioDeviceInfo.getId()` the stream is routed to.
 * @property performanceMode The mode the stream runs in, or `null` if the platform reports none this library knows.
 * @property usesMmap `true` when the stream uses the memory-mapped path that exclusive streams use;
 *   `null` below Android 9.
 * @property hardware What the hardware itself runs at, from Android 14; `null` before.
 * @property asRequested Exclusive, and everything (stream and, where reported, hardware) is as requested.
 * @property verdict A short stable identifier of the most serious difference (`exclusive`, `shared`,
 *   `hardware_rate`, `hardware_format`, ...).
 * @property reason A sentence saying what is and is not known about the audio path. It is the text of
 *   [AAudioExclusiveOutput.bitPerfect].
 */
public data class AAudioGrant(
    val sharingMode: AAudioSharingMode,
    val sampleRate: Int,
    val channels: Int,
    val format: AAudioSampleFormat?,
    val framesPerBurst: Int,
    val bufferCapacityFrames: Int,
    val bufferSizeFrames: Int,
    val deviceId: Int,
    val performanceMode: AAudioPerformanceMode?,
    val usesMmap: Boolean?,
    val hardware: AAudioHardware?,
    val asRequested: Boolean,
    val verdict: String,
    val reason: String,
) {
    /** `true` when the stream has the device to itself. */
    public val isExclusive: Boolean get() = sharingMode == AAudioSharingMode.EXCLUSIVE
}

/** Something that happened to an [AAudioExclusiveOutput]. Delivered from a library thread, never from the audio path. */
public sealed interface AAudioEvent {
    /**
     * The route went away: the device was unplugged or Android re-routed the stream. The output
     * is [StreamState.FAILED]; writes throw [DacErrorCode.DEVICE_LOST]. Close it and open another.
     */
    public data class Disconnected(val detail: String) : AAudioEvent

    /** The platform reported another error for the stream, which is [StreamState.FAILED]. */
    public data class Failed(val detail: String) : AAudioEvent

    /** The output's buffer ran dry: the host did not write fast enough. [total] counts all of them. */
    public data class Underrun(val total: Long) : AAudioEvent

    /** AAudio's own device-side buffer ran dry ([total] since the stream opened). */
    public data class Xrun(val total: Int) : AAudioEvent

    /** The output's state changed. */
    public data class StateChanged(val state: StreamState) : AAudioEvent
}

/** The observable state of one output; it exists before the native handle does (events can arrive during `open`). */
internal class AAudioSignals(private val reasonWhenGranted: () -> String?) : NativeListener {
    val state = MutableStateFlow(StreamState.BUFFERING)
    val bitPerfect = MutableStateFlow<BitPerfectState>(
        BitPerfectState.NotVerified(null, "The output is not open yet."),
    )
    val events = MutableSharedFlow<AAudioEvent>(
        replay = 0,
        extraBufferCapacity = EVENT_BUFFER,
        onBufferOverflow = BufferOverflow.DROP_OLDEST,
    )

    fun publish(event: AAudioEvent) {
        events.tryEmit(event)
    }

    fun setState(next: StreamState) {
        if (state.value == next) return
        // A failed or closed output stays so.
        if (state.value == StreamState.FAILED || state.value == StreamState.CLOSED) return
        state.value = next
        publish(AAudioEvent.StateChanged(next))
    }

    override fun onEvent(type: Int, a: Long, b: Double, message: String?) {
        val detail = message ?: "AAudio error $a"
        when (type) {
            EVENT_DISCONNECTED -> {
                fail(detail)
                publish(AAudioEvent.Disconnected(detail))
            }
            EVENT_FAILED -> {
                fail(detail)
                publish(AAudioEvent.Failed(detail))
            }
        }
    }

    private fun fail(detail: String) {
        if (state.value != StreamState.CLOSED) {
            state.value = StreamState.FAILED
            publish(AAudioEvent.StateChanged(StreamState.FAILED))
        }
        bitPerfect.value = BitPerfectState.NotVerified(
            null,
            "The audio route failed or was disconnected ($detail); nothing more is known about the samples" +
                (reasonWhenGranted()?.let { ". Before that: $it" } ?: "."),
        )
    }

    companion object {
        const val EVENT_DISCONNECTED = 0
        const val EVENT_FAILED = 1
        const val EVENT_BUFFER = 64
    }
}

/**
 * Plays through an AAudio stream in exclusive mode, behind the same [PcmOutput] interface as a
 * direct USB [DacStream] and an [AndroidManagedOutput].
 *
 * This is the route for a device whose DAC is internal (a digital audio player): the stream is
 * opened at the source's own rate, channel count and sample format, and exclusive mode asks the
 * platform to bypass the app mixer, which is what makes it worth using. **The library does not
 * convert, resample, dither or scale anything**: the bytes you write are the bytes handed to
 * AAudio, in the layout of the request (see [open]).
 *
 * ### What can and cannot be said about bit-perfect output
 * An app cannot see past AAudio, so [bitPerfect] is never [BitPerfectState.Verified]. It is
 * [BitPerfectState.NotVerified] with the most precise reason the grant allows:
 * - exclusive, at the requested rate and format: the app mixer is bypassed, but the device's audio HAL
 *   is not verifiable from an app;
 * - the platform granted a shared stream: the system mixer is in the path;
 * - on Android 14 and later, the hardware runs at another rate or format than the stream: AAudio
 *   converts before the device.
 *
 * A stream that would play the bytes wrongly (another rate, channel count or format than the one you
 * write) is never started: [open] fails with [DacErrorCode.UNSUPPORTED].
 *
 * ### Buffering
 * AAudio's own buffer is a few milliseconds deep. This output puts a ring of
 * [AAudioConfig.bufferTargetMs] in front of it (250 ms by default, as for a direct USB stream) and
 * feeds the stream from AAudio's real-time data callback, so the host can pull and write the way it
 * does for the other outputs and [flush] still drops the buffer at once. [latencyFrames] includes the ring.
 * Pause plays silence; the stream stays open (and exclusive) until [close].
 *
 * ### Threading
 * As for [PcmOutput]: [write] never blocks and has one writer thread; the other calls are safe from
 * any thread. [open] and [close] block for up to about half a second: call them off the main thread.
 * [events], [state] and [bitPerfect] are updated from library or platform threads, never from the
 * audio path. Do not call [close] from a collector of [events].
 *
 * Not verified on hardware: see the guide's transports page.
 */
public class AAudioExclusiveOutput internal constructor(
    private val backend: AAudioBackend,
    private val handle: Long,
    /** What the platform granted. */
    public val grant: AAudioGrant,
    private val frameBytes: Int,
    private val signals: AAudioSignals,
    private val nanoTime: () -> Long = System::nanoTime,
) : PcmOutput {

    private val closed = AtomicBoolean(false)

    // Written from one writer thread; only used for heap buffers.
    private var scratch: ByteBuffer? = null

    // Polled counters, so a change can be announced as an event.
    private var lastPolled = 0L
    private var lastXruns = 0
    private var lastUnderruns = 0L

    /** The output's state. Updated on every call and from the platform's error callback. */
    public val state: StateFlow<StreamState> get() = signals.state

    /** Disconnection, failures, underruns and device-side xruns. A buffer of 64; the oldest are dropped. */
    public val events: SharedFlow<AAudioEvent> get() = signals.events

    /** Never [BitPerfectState.Verified]; the reason says exactly what is and is not known. */
    override val bitPerfect: StateFlow<BitPerfectState> get() = signals.bitPerfect

    /** AAudio's count of device-side underruns since the stream opened. */
    public val xrunCount: Int get() = ifOpen(0) { nativeCall { backend.xruns(handle) } }

    /** How often this output's own buffer ran dry because the host wrote too slowly. */
    public val underrunCount: Long get() = ifOpen(0L) { nativeCall { backend.underruns(handle) } }

    override fun write(buffer: ByteBuffer): Int {
        checkOpen()
        if (!buffer.hasRemaining()) return 0
        val taken = if (buffer.isDirect) {
            val count = nativeCall { backend.write(handle, buffer, buffer.position(), buffer.remaining()) }
            buffer.position(buffer.position() + count)
            count
        } else {
            writeHeap(buffer)
        }
        pollCounters()
        return taken
    }

    private fun writeHeap(buffer: ByteBuffer): Int {
        val staging = scratch ?: ByteBuffer.allocateDirect(HEAP_COPY_BYTES).also { scratch = it }
        var total = 0
        while (buffer.hasRemaining()) {
            val chunk = minOf(buffer.remaining(), staging.capacity())
            val source = buffer.duplicate().apply { limit(position() + chunk) }
            staging.clear()
            staging.put(source)
            val taken = nativeCall { backend.write(handle, staging, 0, chunk) }
            buffer.position(buffer.position() + taken)
            total += taken
            if (taken < chunk) break
        }
        return total
    }

    override val writableBytes: Int
        get() = ifOpen(0) {
            pollCounters()
            nativeCall { backend.writableBytes(handle) }
        }

    override fun play() {
        checkOpen()
        nativeCall { backend.play(handle) }
        refreshState()
    }

    override fun pause() {
        checkOpen()
        nativeCall { backend.pause(handle) }
        refreshState()
    }

    override fun flush() {
        checkOpen()
        nativeCall { backend.flush(handle) }
        refreshState()
    }

    override fun drain() {
        checkOpen()
        nativeCall { backend.drain(handle) }
        refreshState()
    }

    override val isDrained: Boolean
        get() = ifOpen(false) {
            val drained = nativeCall { backend.isDrained(handle) }
            refreshState()
            drained
        }

    /** Music frames the device has presented since the start or the last [flush], at the stream's rate. */
    override val playedFrames: Long
        get() = ifOpen(0L) { nativeCall { backend.playedFrames(handle) } }

    /** Music between the host and the device: this output's buffer plus what AAudio holds. */
    override val latencyFrames: Long
        get() = ifOpen(0L) { nativeCall { backend.latencyFrames(handle) } }

    /**
     * Stops the stream and releases it (up to about half a second). Safe to call again. No event
     * is delivered after it returns.
     */
    override fun close() {
        if (!closed.compareAndSet(false, true)) return
        try {
            nativeCall { backend.close(handle) }
        } finally {
            signals.state.value = StreamState.CLOSED
        }
    }

    // ---- helpers ----

    private fun checkOpen() {
        if (closed.get()) throw DacException(DacErrorCode.INVALID_STATE, "the output is closed")
    }

    private inline fun <T> ifOpen(whenClosed: T, block: () -> T): T = if (closed.get()) whenClosed else block()

    private fun refreshState() {
        val next = StreamState.fromCode(nativeCall { backend.state(handle) }) ?: return
        signals.setState(next)
    }

    /** At most every [POLL_INTERVAL_NANOS]: refreshes the state and announces new underruns and xruns. */
    private fun pollCounters() {
        val now = nanoTime()
        if (lastPolled != 0L && now - lastPolled < POLL_INTERVAL_NANOS) return
        lastPolled = now
        refreshState()
        val xruns = nativeCall { backend.xruns(handle) }
        if (xruns > lastXruns) {
            lastXruns = xruns
            signals.publish(AAudioEvent.Xrun(xruns))
        }
        val underruns = nativeCall { backend.underruns(handle) }
        if (underruns > lastUnderruns) {
            lastUnderruns = underruns
            signals.publish(AAudioEvent.Underrun(underruns))
        }
    }

    public companion object {
        /** Staging buffer for heap buffers: about 220 ms of 24-bit stereo at 192 kHz per copy. */
        private const val HEAP_COPY_BYTES = 256 * 1024

        private const val POLL_INTERVAL_NANOS = 50_000_000L

        /**
         * The output devices Android lists, such as the internal DAC of a digital audio player
         * (`TYPE_BUILTIN_SPEAKER`, `TYPE_WIRED_HEADPHONES`, ...) or a USB DAC. Pass one to [open]
         * to route the stream to it; the list is a snapshot.
         */
        public fun outputDevices(context: Context): List<AudioDeviceInfo> =
            (context.getSystemService(Context.AUDIO_SERVICE) as AudioManager)
                .getDevices(AudioManager.GET_DEVICES_OUTPUTS)
                .toList()

        /**
         * Opens an exclusive AAudio stream for [request] and starts it (it plays silence until the
         * buffer holds [AAudioConfig.startThresholdMs] of audio).
         *
         * The stream is requested with `AAUDIO_SHARING_MODE_EXCLUSIVE`, at the request's exact sample
         * rate and channel count, in the sample layout of [StreamRequest.bitDepth], for music
         * (`USAGE_MEDIA`, `CONTENT_TYPE_MUSIC`). You write PCM in that layout, little-endian and
         * interleaved, nothing converted:
         *
         * | `bitDepth` | Bytes per sample | Needs |
         * |---|---|---|
         * | 16 | signed 16-bit | Android 8.0 (API 26) |
         * | 24 | packed signed 24-bit, 3 bytes | Android 12 (API 31) |
         * | 32 | signed 32-bit, or 32-bit float with [AAudioConfig.floatSamples] | integer: Android 12 (API 31); float: Android 8.0 |
         *
         * [StreamRequest.ratePolicy] is ignored: the rate is exactly the request's. DSD and DoP are not
         * supported.
         *
         * @param context used to confirm that [device] is still attached.
         * @param device the output to route to, from [outputDevices]; `null` for the platform's default.
         * @throws DacException [DacErrorCode.UNSUPPORTED] for DSD or DoP, a depth or channel count AAudio cannot carry,
         *   a format the Android release lacks, or a stream the platform opened in another format than requested;
         *   [DacErrorCode.EXCLUSIVE_UNAVAILABLE] when [AAudioConfig.requireExclusive] is on and the platform grants only
         *   a shared stream; [DacErrorCode.BUSY] when another app holds the device exclusively;
         *   [DacErrorCode.DEVICE_LOST] when [device] is no longer attached;
         *   [DacErrorCode.INVALID_ARGUMENT] for a bad option.
         */
        public fun open(
            context: Context,
            request: StreamRequest,
            device: AudioDeviceInfo? = null,
            config: AAudioConfig = AAudioConfig(),
        ): AAudioExclusiveOutput {
            if (device != null && outputDevices(context).none { it.id == device.id }) {
                throw DacException(DacErrorCode.DEVICE_LOST, "the output device ${device.productName} is no longer attached")
            }
            return open(NativeAAudioBackend, Build.VERSION.SDK_INT, request, device?.id ?: 0, config)
        }

        internal fun open(
            backend: AAudioBackend,
            apiLevel: Int,
            request: StreamRequest,
            deviceId: Int,
            config: AAudioConfig,
            nanoTime: () -> Long = System::nanoTime,
        ): AAudioExclusiveOutput {
            if (request.encoding != StreamEncoding.PCM) {
                throw DacException(
                    DacErrorCode.UNSUPPORTED,
                    "the AAudio output plays PCM only; DSD and DoP need the direct USB output",
                )
            }
            val format = sampleFormatFor(request, config)
            val frameBytes = request.channels * format.bytesPerSample

            var granted: AAudioGrant? = null
            val signals = AAudioSignals { granted?.reason }
            val handle = nativeCall { backend.open(aaudioRequestJson(request, format, deviceId, config), signals, apiLevel) }
            try {
                val grant = nativeCall { parseAAudioGrant(backend.grant(handle)) }
                granted = grant
                if (config.requireExclusive && !grant.isExclusive) {
                    throw DacException(
                        DacErrorCode.EXCLUSIVE_UNAVAILABLE,
                        "the platform granted a shared stream, not an exclusive one. ${grant.reason}",
                    )
                }
                nativeCall { backend.start(handle) }
                signals.bitPerfect.value = BitPerfectState.NotVerified(null, grant.reason)
                return AAudioExclusiveOutput(backend, handle, grant, frameBytes, signals, nanoTime)
            } catch (error: Throwable) {
                runCatching { backend.close(handle) }
                throw error
            }
        }

        private fun sampleFormatFor(request: StreamRequest, config: AAudioConfig): AAudioSampleFormat {
            if (config.floatSamples && request.bitDepth != 32) {
                throw DacException(
                    DacErrorCode.INVALID_ARGUMENT,
                    "floatSamples needs bitDepth 32, not ${request.bitDepth}",
                )
            }
            if (request.channels !in 1..MAX_CHANNELS) {
                throw DacException(
                    DacErrorCode.UNSUPPORTED,
                    "${request.channels} channels are not supported (1 to $MAX_CHANNELS)",
                )
            }
            return when (request.bitDepth) {
                16 -> AAudioSampleFormat.PCM_16
                24 -> AAudioSampleFormat.PCM_24_PACKED
                32 -> if (config.floatSamples) AAudioSampleFormat.FLOAT_32 else AAudioSampleFormat.PCM_32
                else -> throw DacException(
                    DacErrorCode.UNSUPPORTED,
                    "${request.bitDepth}-bit PCM is not available through AAudio (16, 24 or 32)",
                )
            }
        }

        private const val MAX_CHANNELS = 8
    }
}
