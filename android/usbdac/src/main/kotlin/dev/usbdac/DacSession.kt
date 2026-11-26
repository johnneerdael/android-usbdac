package dev.usbdac

import android.hardware.usb.UsbDeviceConnection
import dev.usbdac.internal.ClaimedInterfaces
import dev.usbdac.internal.Native
import dev.usbdac.internal.NativeListener
import dev.usbdac.internal.NativeVolumeBackend
import dev.usbdac.internal.logWarning
import dev.usbdac.internal.parseCapabilities
import dev.usbdac.internal.parseStartupReport
import dev.usbdac.internal.parseStreamFormat
import dev.usbdac.internal.parseVolumeRange
import dev.usbdac.internal.parseVolumeState
import dev.usbdac.internal.toJson
import kotlinx.coroutines.channels.BufferOverflow
import kotlinx.coroutines.flow.MutableSharedFlow
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.SharedFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.update
import java.io.Closeable
import java.util.concurrent.ExecutorService
import java.util.concurrent.Executors
import java.util.concurrent.atomic.AtomicBoolean

/** The state of a [DacSession]. */
public enum class DacState {
    /** Open, no stream. */
    READY,

    /** A stream is open. */
    STREAMING,

    /** The device is gone. Close the session. */
    LOST,

    /** Closed. */
    CLOSED,
}

/**
 * An open DAC: its capabilities, its volume, and the streams you can open on it.
 *
 * A session holds the USB connection and the claimed interfaces, and keeps Android's own audio
 * stack away from the DAC, until [close]. Close it when playback is over for good; to save power
 * while paused, close only the stream (see `DacIdleRelease`) and keep the session.
 *
 * ### Threading
 * [negotiate], [openStream] and [close] block for 100 ms or more: call them off the main thread.
 * Everything else is safe from any thread. [state] and [events] are updated from a library thread.
 */
public class DacSession private constructor(
    private val dac: Long,
    /** The DAC this session is open on. */
    public val info: DacDeviceInfo,
    /** The configuration the session was opened with. */
    public val config: DacConfig,
    /** What the DAC offers, read from its descriptors. */
    public val capabilities: DacCapabilities,
    private val connection: UsbDeviceConnection,
    private val claims: ClaimedInterfaces,
    private val onClosed: () -> Unit,
) : Closeable {

    private val mutableState = MutableStateFlow(DacState.READY)
    private val mutableEvents = MutableSharedFlow<DacEvent>(
        replay = 0,
        extraBufferCapacity = EVENT_BUFFER,
        onBufferOverflow = BufferOverflow.DROP_OLDEST,
    )
    private val volumeExecutor: ExecutorService = Executors.newSingleThreadExecutor { task ->
        Thread(task, "usbdac-volume").apply { isDaemon = true }
    }
    private val closed = AtomicBoolean(false)
    @Volatile private var closedFeedbackTrace: String? = null
    private var stream: DacStream? = null

    /** State of the session. */
    public val state: StateFlow<DacState> get() = mutableState

    /**
     * What happens to the session and its stream: state changes, underruns, lost frames, volume
     * changes, a failed stream, a lost device. Delivered from a library thread, never the audio
     * path. A collector that cannot keep up misses the oldest events (a buffer of 64).
     */
    public val events: SharedFlow<DacEvent> get() = mutableEvents

    /**
     * The hardware volume, or `null` when the DAC has no usable volume control: it has none, or the
     * start-up attenuation ([DacConfig.startupVolume]) could not be verified. With a volume,
     * [DacVolume.startup] says what the library did to the level when the session opened. Without
     * one, [openStream] fails with [DacErrorCode.VOLUME_UNAVAILABLE] unless
     * [DacConfig.requireHardwareVolume] is off.
     */
    public val volume: DacVolume?

    private val deviceListener = object : NativeListener {
        override fun onEvent(type: Int, a: Long, b: Double, message: String?) {
            val event = decodeEvent(type, a, b, message)
            if (event is DacEvent.DeviceLost) mutableState.update { DacState.LOST }
            mutableEvents.tryEmit(event)
        }
    }

    init {
        nativeCall { Native.dacSetListener(dac, deviceListener) }
        val range = nativeCall { Native.volumeRange(dac)?.let(::parseVolumeRange) }
        volume = range?.let {
            val backend = NativeVolumeBackend(dac)
            DacVolume(
                backend = backend,
                range = it,
                hasMute = capabilities.hasMute,
                initial = nativeCall { parseVolumeState(Native.volumeGet(dac)) },
                startup = nativeCall { parseStartupReport(requireNotNull(Native.volumeStartup(dac))) },
                executor = volumeExecutor,
                onWriteFailed = { error -> mutableEvents.tryEmit(DacEvent.VolumeWriteFailed(error)) },
            )
        }
    }

    /**
     * Settles the wire format for [request]. This programs the DAC's sampling frequency and reads it
     * back, so it takes 100 ms or more, and it fails with [DacErrorCode.INVALID_STATE] while a stream is
     * open. Nothing is resampled: with [RatePolicy.EXACT] a rate the DAC cannot run is
     * [DacErrorCode.UNSUPPORTED]; with [RatePolicy.NEAREST_SUPPORTED] the returned format carries the
     * rate the DAC will run and the host must resample.
     */
    public fun negotiate(request: StreamRequest): StreamFormat {
        checkOpen()
        return nativeCall { parseStreamFormat(Native.dacNegotiate(dac, request.toJson())) }
    }

    /**
     * Export the most recent stream's USB feedback support report as JSON.
     * Available after the stream fails or closes. Closing an opted-in session
     * retains a final copy on this session object.
     * Returns null until an opted-in stream has opened. PCM, DSD, and capture
     * audio are excluded. Export on demand from the app's support UI.
     */
    public fun feedbackTraceJson(): String? {
        if (!config.captureFeedbackTrace) return null
        if (closed.get()) return closedFeedbackTrace
        return readFeedbackTrace()
    }

    private fun readFeedbackTrace(): String? =
        nativeCall { Native.dacFeedbackTrace(dac).takeUnless { it == "null" } }

    /**
     * Opens a stream in [format], the one [negotiate] returned. Switches the alternate setting,
     * re-verifies the clock and starts the output thread (100 ms or more). A new stream plays by
     * itself once the ring holds [DacConfig.startThresholdMs] of audio.
     *
     * @throws DacException [DacErrorCode.INVALID_STATE] if a stream is already open,
     *   [DacErrorCode.CLOCK_UNVERIFIED] if the DAC does not confirm the rate,
     *   [DacErrorCode.VOLUME_UNAVAILABLE] if the DAC has no usable hardware volume and
     *   [DacConfig.requireHardwareVolume] is on, and the native codes.
     */
    @Synchronized
    public fun openStream(format: StreamFormat): DacStream {
        checkOpen()
        if (stream != null) throw DacException(DacErrorCode.INVALID_STATE, "a stream is already open")
        val signals = StreamSignals { event -> mutableEvents.tryEmit(event) }
        val handle = nativeCall { Native.streamOpen(dac, format.formatId, signals) }
        val opened = DacStream(handle, format, signals) { streamClosed() }
        stream = opened
        signals.seedBitPerfect(nativeCall { Native.streamIsBitPerfect(handle) })
        mutableState.update { if (it == DacState.READY) DacState.STREAMING else it }
        return opened
    }

    @Synchronized
    private fun streamClosed() {
        stream = null
        mutableState.update { if (it == DacState.STREAMING) DacState.READY else it }
    }

    /** Closes the session with [DacConfig.handBack]. See [close]. */
    override fun close() {
        close(config.handBack)
    }

    /**
     * Closes the stream if one is open, hands the volume back as [handBack] says, releases the
     * DAC to the native library and then to Android (the interfaces are released and Android's USB
     * audio driver is asked to take the DAC again), and closes the USB connection. Blocks for
     * about 100 ms. Safe to call again. Do not call it from a collector of [events].
     *
     * The volume is restored after the stream has stopped, so a level louder than the current one
     * is never heard.
     *
     * @throws DacException if the native library fails to close. USB ownership remains held until
     *   all native transfers retire, then Android receives the interfaces.
     */
    public fun close(handBack: HandBack) {
        if (!closed.compareAndSet(false, true)) return
        var failure: DacException? = null
        try {
            runCatching { synchronized(this) { stream }?.close() }.onFailure { logWarning("closing the stream failed", it) }
            if (config.captureFeedbackTrace) {
                runCatching { readFeedbackTrace() }
                    .onSuccess { closedFeedbackTrace = it }
                    .onFailure { logWarning("exporting the feedback trace failed", it) }
            }
            if (handBack == HandBack.RESTORE_ORIGINAL && mutableState.value != DacState.LOST) {
                runCatching { volume?.handBack() }.onFailure { logWarning("restoring the volume failed", it) }
            }
            volume?.close()
            volumeExecutor.shutdown()
            try {
                nativeCall { Native.dacClose(dac) }
            } catch (error: DacException) {
                failure = error
            }
        } finally {
            fun releaseHostOwnership() {
                claims.release()
                runCatching { connection.close() }
                onClosed()
            }
            if (runCatching { Native.dacAwaitRetirement() }.getOrDefault(false)) {
                releaseHostOwnership()
            } else {
                Thread({
                    while (!runCatching { Native.dacAwaitRetirement() }.getOrDefault(false)) {
                        runCatching { Thread.sleep(50) }
                    }
                    releaseHostOwnership()
                }, "usbdac-retirement").start()
            }
            mutableState.value = DacState.CLOSED
        }
        failure?.let { throw it }
    }

    private fun checkOpen() {
        if (closed.get()) throw DacException(DacErrorCode.INVALID_STATE, "the session is closed")
    }

    internal companion object {
        private const val EVENT_BUFFER = 64

        fun create(
            dac: Long,
            device: DacDeviceInfo,
            config: DacConfig,
            connection: UsbDeviceConnection,
            claims: ClaimedInterfaces,
            onClosed: () -> Unit,
        ): DacSession {
            val capabilities = nativeCall { parseCapabilities(Native.dacCapabilities(dac)) }
            return DacSession(dac, device, config, capabilities, connection, claims, onClosed)
        }
    }
}
