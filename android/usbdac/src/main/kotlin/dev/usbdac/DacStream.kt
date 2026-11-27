package dev.usbdac

import dev.usbdac.internal.Native
import dev.usbdac.internal.NativeListener
import dev.usbdac.internal.parseStats
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import java.nio.ByteBuffer
import java.util.concurrent.atomic.AtomicBoolean

/**
 * The native event listener of one stream. It exists before the stream handle does (the library
 * may deliver the first events while `streamOpen` is still returning), so the stream's observable
 * state lives here.
 */
internal class StreamSignals(private val publish: (DacEvent) -> Unit) : NativeListener {
    val state = MutableStateFlow(StreamState.BUFFERING)
    val bitPerfect = MutableStateFlow<BitPerfectState>(directBitPerfectState(false, BitPerfectBlocker.NO_STREAM))

    @Volatile
    var statsSource: (() -> DacStreamStats)? = null
    private val bitPerfectEventSeen = AtomicBoolean(false)

    /** Sets the first bit-perfect value, unless an event already did. */
    fun seedBitPerfect(isBitPerfect: Boolean) {
        if (bitPerfectEventSeen.get()) return
        bitPerfect.value = directBitPerfectState(isBitPerfect, if (isBitPerfect) null else blocker())
    }

    override fun onEvent(type: Int, a: Long, b: Double, message: String?) {
        val event = decodeEvent(type, a, b, message)
        when (event) {
            is DacEvent.StateChanged -> state.value = event.state
            is DacEvent.BitPerfectChanged -> {
                bitPerfectEventSeen.set(true)
                bitPerfect.value = directBitPerfectState(event.bitPerfect, if (event.bitPerfect) null else blocker())
            }
            // Device-level events reach the session's own listener; do not publish them twice.
            is DacEvent.VolumeChanged, DacEvent.DeviceLost -> return
            else -> Unit
        }
        publish(event)
    }

    private fun blocker(): BitPerfectBlocker? =
        runCatching { statsSource?.invoke()?.bitPerfectBlocker }.getOrNull()
}

/**
 * An open output stream to the DAC. Get one from [DacSession.openStream]. Only one stream can be
 * open at a time on a session.
 *
 * Write audio with [write] in the wire layout of [format] (see `docs/API.md` section 4 and the
 * guide), and control playback with [play], [pause], [flush] and [drain]. The library keeps a ring
 * buffer of [DacConfig.bufferTargetMs] and a USB output thread; nothing here calls back into your
 * code on the audio path.
 *
 * Do not use a stream after [close]; calls that read state then return 0, and [write] throws.
 */
public class DacStream internal constructor(
    private val handle: Long,
    /** The format on the wire. */
    public val format: StreamFormat,
    private val signals: StreamSignals,
    private val onClosed: () -> Unit,
) : PcmOutput {

    private val closed = AtomicBoolean(false)

    // Written from one writer thread; only used for heap buffers.
    private var scratch: ByteBuffer? = null

    init {
        signals.statsSource = ::stats
    }

    /** The stream's state. Updated from a library thread. */
    public val state: StateFlow<StreamState> get() = signals.state

    override val bitPerfect: StateFlow<BitPerfectState> get() = signals.bitPerfect

    override fun write(buffer: ByteBuffer): Int {
        checkOpen()
        if (!buffer.hasRemaining()) return 0
        if (buffer.isDirect) {
            val taken = nativeCall { Native.streamWrite(handle, buffer, buffer.position(), buffer.remaining()) }
            buffer.position(buffer.position() + taken)
            return taken
        }
        return writeHeap(buffer)
    }

    private fun writeHeap(buffer: ByteBuffer): Int {
        val staging = scratch ?: ByteBuffer.allocateDirect(HEAP_COPY_BYTES).also { scratch = it }
        var total = 0
        while (buffer.hasRemaining()) {
            val chunk = minOf(buffer.remaining(), staging.capacity())
            val source = buffer.duplicate().apply { limit(position() + chunk) }
            staging.clear()
            staging.put(source)
            val taken = nativeCall { Native.streamWrite(handle, staging, 0, chunk) }
            buffer.position(buffer.position() + taken)
            total += taken
            if (taken < chunk) break
        }
        return total
    }

    override val writableBytes: Int
        get() = ifOpen(0) { nativeCall { Native.streamWritableBytes(handle) } }

    override fun play() {
        checkOpen()
        nativeCall { Native.streamPlay(handle) }
    }

    override fun pause() {
        checkOpen()
        nativeCall { Native.streamPause(handle) }
    }

    override fun flush() {
        checkOpen()
        nativeCall { Native.streamFlush(handle) }
    }

    override fun drain() {
        checkOpen()
        nativeCall { Native.streamDrain(handle) }
    }

    override val isDrained: Boolean
        get() = ifOpen(false) { nativeCall { Native.streamIsDrained(handle) } }

    override val playedFrames: Long
        get() = ifOpen(0L) { nativeCall { Native.streamPlayedFrames(handle) } }

    override val latencyFrames: Long
        get() = ifOpen(0L) { nativeCall { Native.streamLatencyFrames(handle) } }

    /** The stream's counters. Takes a short lock and touches session state; poll it a few times a second, not per buffer. */
    public fun stats(): DacStreamStats {
        checkOpen()
        return nativeCall { parseStats(Native.streamStats(handle)) }
    }

    /**
     * Applies a gain to the samples before they are sent. 1.0 is exactly unchanged. Any other value
     * means the library changes sample values, so the stream stops being bit-perfect; use the DAC's
     * [DacVolume] instead. Refused for DoP and native DSD, where it would destroy the signal.
     */
    public fun setSoftwareGain(gain: Float) {
        checkOpen()
        nativeCall { Native.streamSetSoftwareGain(handle, gain) }
    }

    /**
     * Use the shallower [DacConfig.processingBufferTargetMs] while the app applies EQ or
     * other live PCM processing before [write]. Buffered audio plays through at the old
     * depth before the new target takes effect. Restore the deeper target with `false`.
     * This is a buffering hint and does not report what the app did to the samples.
     * DoP and native DSD ignore it.
     */
    public fun setHostProcessing(active: Boolean) {
        checkOpen()
        nativeCall { Native.streamSetHostProcessing(handle, active) }
    }

    /**
     * Stops the stream and releases it (about 100 ms). The session stays open and the volume still
     * works; open another stream to play again. Safe to call again.
     */
    override fun close() {
        if (!closed.compareAndSet(false, true)) return
        try {
            nativeCall { Native.streamClose(handle) }
        } finally {
            signals.state.value = StreamState.CLOSED
            signals.statsSource = null
            onClosed()
        }
    }

    internal fun handleForReconfigure(): Long { checkOpen(); return handle }

    /** Native replacement already removed the old registration; do not close its stale token. */
    internal fun retiredByReconfigure() {
        if (!closed.compareAndSet(false, true)) return
        signals.state.value = StreamState.CLOSED
        signals.statsSource = null
        onClosed()
    }

    private fun checkOpen() {
        if (closed.get()) throw DacException(DacErrorCode.INVALID_STATE, "the stream is closed")
    }

    private inline fun <T> ifOpen(whenClosed: T, block: () -> T): T = if (closed.get()) whenClosed else block()

    private companion object {
        /** Staging buffer for heap buffers: about 220 ms of 24-bit stereo at 192 kHz per copy. */
        const val HEAP_COPY_BYTES = 256 * 1024
    }
}
