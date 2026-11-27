package dev.usbdac

import android.hardware.usb.UsbDeviceConnection
import dev.usbdac.internal.UsbConfigurationSelection
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
import dev.usbdac.internal.toCaptureJson
import dev.usbdac.internal.parseCaptureFormat
import org.json.JSONObject
import org.json.JSONArray
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
    private val configurationSelection: UsbConfigurationSelection,
    private val duplexEnabled: Boolean,
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
    private var duplex: DacDuplexStream? = null

    public val routing: DacRouting = DacRouting(dac, ::checkOpen)
    /** Routing hand-back state, including delayed retirement after close. */
    public val routingHandBack: RoutingHandBackState get() = routing.handBack

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
            if (event is DacEvent.DeviceLost) mutableState.update { if (it == DacState.CLOSED) it else DacState.LOST }
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

    internal fun negotiateCandidates(requests: List<StreamRequest>): StreamFormat {
        checkOpen()
        val json = JSONArray().apply { requests.forEach { put(JSONObject(it.toJson())) } }
        return nativeCall { parseStreamFormat(Native.dacNegotiateCandidates(dac, json.toString())) }
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
        lateinit var opened: DacStream
        opened = DacStream(handle, format, signals) { streamClosed(opened) }
        stream = opened
        try {
            signals.seedBitPerfect(nativeCall { Native.streamIsBitPerfect(handle) })
            mutableState.update { if (it == DacState.READY) DacState.STREAMING else it }
            return opened
        } catch (error: Throwable) {
            runCatching { opened.close() }.onFailure(error::addSuppressed)
            throw error
        }
    }

    /**
     * Drain and replace [current] on the same retained physical USB handle and host claims.
     * Stop the writer first; call off the main thread. Hardware volume remains available unchanged.
     *
     * Success and recovered rollback return a paused stream with zero-based counters. Rebase/reseek
     * the decoder for its actual [DacStream.format], supply PCM, then play. A timeout throws
     * [DacErrorCode.RECONFIGURE_DRAIN_TIMEOUT] while [current] remains open/draining (no hidden flush).
     * [DacErrorCode.RECONFIGURE_UNUSABLE] requires closing this session. Duplex is refused.
     * While it runs, including the drain, [negotiate] and [routing] calls on this session fail with
     * [DacErrorCode.BUSY] instead of waiting; [openStream] waits for it to return.
     */
    public fun reconfigure(current: DacStream, request: StreamRequest, drainTimeoutMs: Long = 5_000): StreamReconfiguration =
        reconfigureCandidates(current, listOf(request), drainTimeoutMs)

    /**
     * Reconfigure once with up to four ascending same-rate/channel PCM widening candidates.
     * A runtime refusal is rolled back unless the native backend can prove retry is safe.
     */
    @Synchronized
    public fun reconfigureCandidates(current: DacStream, requests: List<StreamRequest>, drainTimeoutMs: Long = 5_000): StreamReconfiguration {
        checkOpen()
        require(requests.isNotEmpty() && requests.size <= 4) { "expected one to four candidates" }
        require(drainTimeoutMs >= 0) { "negative drain timeout" }
        if (stream !== current || duplex != null) {
            throw DacException(DacErrorCode.INVALID_ARGUMENT, "current must be this session's playback-only stream")
        }
        val signals = StreamSignals { event -> mutableEvents.tryEmit(event) }
        val candidates = JSONArray().apply { requests.forEach { put(JSONObject(it.toJson())) } }
        val response = nativeCall { JSONObject(Native.streamReconfigureCandidates(dac, current.handleForReconfigure(),
            candidates.toString(), drainTimeoutMs, signals)) }
        val handle = nativeCall { response.getLong("stream_id") }
        current.retiredByReconfigure()
        try {
            val format = nativeCall { parseStreamFormat(response.getJSONObject("format").toString()) }
            val cause = if (response.getBoolean("rolled_back")) DacException(
                DacErrorCode.fromWireName(response.getString("error_code")), response.getString("error_message")) else null
            lateinit var opened: DacStream
            opened = DacStream(handle, format, signals) { streamClosed(opened) }
            signals.seedBitPerfect(nativeCall { Native.streamIsBitPerfect(handle) })
            stream = opened
            mutableState.update { if (it == DacState.READY) DacState.STREAMING else it }
            return StreamReconfiguration(opened, cause)
        } catch (error: Throwable) {
            runCatching { Native.streamClose(handle) }.onFailure(error::addSuppressed)
            throw error
        }
    }

    /**
     * Open both directions atomically on one USB worker after [UsbDac.openDuplex].
     * Shared clocks require equal rates; proven independent clocks retain their own counters.
     * Closing either public direction closes the pair. Capture backlog never blocks playback.
     */
    @Synchronized
    public fun openDuplex(format: StreamFormat, request: CaptureRequest, capacityFrames: Int = 48_000): DacDuplexStream {
        checkOpen()
        if (!duplexEnabled) throw DacException(DacErrorCode.INVALID_STATE, "open the device with UsbDac.openDuplex first")
        if (stream != null) throw DacException(DacErrorCode.INVALID_STATE, "a stream is already open")
        val signals = StreamSignals { event -> mutableEvents.tryEmit(event) }
        val handles = nativeCall { JSONObject(Native.duplexOpen(dac, format.formatId, request.toCaptureJson(), capacityFrames, signals)) }
        val playbackHandle = handles.getLong("playback")
        val captureHandle = handles.getLong("capture")
        try {
            val captureFormat = nativeCall { parseCaptureFormat(Native.captureFormat(captureHandle)) }
            lateinit var pair: DacDuplexStream
            val output = DacStream(playbackHandle, format, signals) { pair.close() }
            val input = DacCaptureStream(captureHandle, captureFormat) { pair.close() }
            pair = DacDuplexStream(output, input) { streamClosed() }
            signals.seedBitPerfect(nativeCall { Native.streamIsBitPerfect(playbackHandle) })
            stream = output
            duplex = pair
            mutableState.update { if (it == DacState.READY) DacState.STREAMING else it }
            return pair
        } catch (error: Throwable) {
            runCatching { Native.streamClose(playbackHandle) }.onFailure(error::addSuppressed)
            runCatching { Native.captureClose(captureHandle) }.onFailure(error::addSuppressed)
            throw error
        }
    }

    @Synchronized
    private fun streamClosed(expected: DacStream? = null) {
        if (expected != null && stream !== expected) return
        stream = null
        duplex = null
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
     * When restoration is requested, native cleanup restores the volume after USB transfers
     * retire and before Android's interface claims are released. This may happen after close returns.
     *
     * @throws DacException if the native library fails to close. USB ownership remains held until
     *   all native transfers retire, then Android receives the interfaces.
     */
    @Synchronized
    public fun close(handBack: HandBack) {
        if (!closed.compareAndSet(false, true)) return
        val retirement = runCatching { Native.dacRetirementToken(dac) }.getOrDefault(0L)
        routing.beginHandBack(retirement != 0L)
        fun retired() = runCatching { if (retirement != 0L) Native.dacAwaitRetirementToken(retirement) else Native.dacAwaitConnectionRetirement(connection.fileDescriptor) }.getOrDefault(false)
        var failure: DacException? = null
        try {
            runCatching { synchronized(this) { duplex ?: stream }?.close() }.onFailure { logWarning("closing the stream failed", it) }
            if (config.captureFeedbackTrace) {
                runCatching { readFeedbackTrace() }
                    .onSuccess { closedFeedbackTrace = it }
                    .onFailure { logWarning("exporting the feedback trace failed", it) }
            }
            volume?.close()
            volumeExecutor.shutdown()
            try {
                nativeCall { Native.dacSetCloseHandBack(
                    dac,
                    volume != null && mutableState.value != DacState.LOST && handBack == HandBack.RESTORE_ORIGINAL,
                    handBack == HandBack.RESTORE_ORIGINAL,
                ) }
            } catch (error: DacException) {
                failure = error
            }
            try {
                nativeCall { Native.dacClose(dac) }
            } catch (error: DacException) {
                if (failure == null) failure = error else failure?.addSuppressed(error)
            }
        } finally {
            fun releaseHostOwnership() {
                routing.publishHandBack(retirement)
                if (retirement != 0L) runCatching { Native.dacReleaseRetirementToken(retirement) }
                claims.release()
                runCatching { configurationSelection.restore() }.onFailure {
                    logWarning("restoring USB configuration failed", it)
                    if (it is DacException && failure == null) failure = it
                }
                runCatching { connection.close() }
                onClosed()
            }
            if (retired()) {
                releaseHostOwnership()
            } else {
                Thread({
                    while (!retired()) {
                        runCatching { Thread.sleep(50) }
                    }
                    releaseHostOwnership()
                }, "usbdac-retirement").start()
            }
            mutableState.value = DacState.CLOSED
        }
        failure?.let { throw it }
    }

    /** USB ownership evidence for the in-repository validator. */
    @Synchronized
    public fun validatorClaimEvidenceJson(): String {
        checkOpen()
        return JSONObject().put("configuration_id", configurationSelection.selected.id)
            .put("claimed_interface_ids", org.json.JSONArray(claims.ids))
            .put("claim_force", true).toString()
    }

    /** Internal integration point for the in-repository validator. */
    @Synchronized
    public fun runValidator(captureDirectory: String): String {
        checkOpen()
        check(stream == null && duplex == null) { "validator requires an idle session" }
        return Native.validatorRun(dac, captureDirectory)
    }

    /** Retire native work after the explicit legacy-open path fails; hand-back is caller-owned. */
    @Synchronized
    internal fun closeForFailedLegacyOpen(isAttached: () -> Boolean): PendingUsbConfigurationRestore {
        if (!closed.compareAndSet(false, true)) {
            throw DacException(DacErrorCode.INVALID_STATE, "the session is already closed")
        }
        var preliminaryFailure: Throwable? = null
        fun recordPreliminary(error: Throwable) {
            if (preliminaryFailure == null) preliminaryFailure = error else preliminaryFailure?.addSuppressed(error)
        }
        val retirementResult = runCatching { Native.dacRetirementToken(dac) }
        retirementResult.exceptionOrNull()?.let(::recordPreliminary)
        val retirement = retirementResult.getOrDefault(0L)
        routing.beginHandBack(retirement != 0L)
        val fdResult = runCatching { connection.fileDescriptor }
        fdResult.exceptionOrNull()?.let(::recordPreliminary)
        val fd = fdResult.getOrNull()
        fun retired(): Boolean = runCatching {
            if (retirement != 0L) Native.dacAwaitRetirementToken(retirement)
            else fd?.let { Native.dacAwaitConnectionRetirement(it) } ?: false
        }.getOrDefault(false)
        // Closing native state starts retirement; no Android configuration restoration occurs here.
        runCatching { (duplex ?: stream)?.close() }.onFailure(::recordPreliminary)
        if (config.captureFeedbackTrace) runCatching { readFeedbackTrace() }
            .onSuccess { closedFeedbackTrace = it }.onFailure(::recordPreliminary)
        runCatching { volume?.close() }.onFailure(::recordPreliminary)
        runCatching { volumeExecutor.shutdown() }.onFailure(::recordPreliminary)
        runCatching { nativeCall { Native.dacClose(dac) } }.onFailure(::recordPreliminary)
        runCatching { mutableState.value = DacState.CLOSED }.onFailure(::recordPreliminary)
        return PendingUsbConfigurationRestore(::retired) {
            var failure: Throwable? = preliminaryFailure
            fun record(error: Throwable) {
                if (failure == null) failure = error else failure?.addSuppressed(error)
            }
            try {
                if (runCatching { isAttached() }.getOrDefault(false)) {
                    val released = runCatching { claims.releaseChecked() }.onFailure(::record).isSuccess
                    // Recheck after releasing claims; a new attachment must never receive a restore.
                    if (released && runCatching { isAttached() }.getOrDefault(false)) {
                        runCatching { configurationSelection.restore() }.onFailure(::record)
                    } else if (released) record(DacException(DacErrorCode.DEVICE_LOST, "USB attachment changed before configuration restoration"))
                } else record(DacException(DacErrorCode.DEVICE_LOST, "USB attachment is no longer present; configuration was not restored"))
            } finally {
                routing.publishHandBack(retirement)
                if (retirement != 0L) runCatching { Native.dacReleaseRetirementToken(retirement) }.onFailure(::record)
                runCatching { connection.close() }.onFailure(::record)
                runCatching { onClosed() }.onFailure(::record)
            }
            failure?.let { throw it }
        }
    }

    internal fun driverHandle(): Long { checkOpen(); return dac }

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
            configurationSelection: UsbConfigurationSelection,
            duplexEnabled: Boolean = false,
            onClosed: () -> Unit,
        ): DacSession {
            val capabilities = nativeCall { parseCapabilities(Native.dacCapabilities(dac)) }
            return DacSession(dac, device, config, capabilities, connection, claims, configurationSelection, duplexEnabled, onClosed)
        }
    }
}
