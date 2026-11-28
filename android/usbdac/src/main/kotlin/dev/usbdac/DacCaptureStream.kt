package dev.usbdac

import dev.usbdac.internal.Native
import dev.usbdac.internal.parseCaptureStats
import java.io.Closeable
import java.nio.ByteBuffer
import java.util.concurrent.atomic.AtomicBoolean

/** USB channel identities in interleaved wire order; no remapping is performed. */
public enum class CaptureChannelPosition {
    FRONT_LEFT, FRONT_RIGHT, FRONT_CENTER, LOW_FREQUENCY, BACK_LEFT, BACK_RIGHT,
    FRONT_LEFT_OF_CENTER, FRONT_RIGHT_OF_CENTER, BACK_CENTER, SIDE_LEFT, SIDE_RIGHT,
    TOP_CENTER, TOP_FRONT_LEFT, TOP_FRONT_CENTER, TOP_FRONT_RIGHT, TOP_BACK_LEFT,
    TOP_BACK_CENTER, TOP_BACK_RIGHT, TOP_FRONT_LEFT_OF_CENTER, TOP_FRONT_RIGHT_OF_CENTER,
    LEFT_LOW_FREQUENCY, RIGHT_LOW_FREQUENCY, TOP_SIDE_LEFT, TOP_SIDE_RIGHT,
    BOTTOM_CENTER, BACK_LEFT_OF_CENTER, BACK_RIGHT_OF_CENTER, UNSPECIFIED,
    ADC3_INFORMATION, ADC3_AMBISONIC,
}

/** An unnamed channel still has a stable zero-based [index] on the wire. */
public data class CaptureChannel(val index: Int, val position: CaptureChannelPosition, val topology: CaptureChannelTopology? = null)

/** Raw ADC3 HC descriptor identity, preserving unknown purpose/relationship semantics. */
public sealed interface CaptureChannelTopology {
    public data class Information(val purpose: Int, val relationship: Int, val group: Int) : CaptureChannelTopology
    public data class Ambisonic(val ordering: Int, val acn: Int, val normalization: Int, val group: Int) : CaptureChannelTopology
}

/** Exact integer PCM request. Set [channelMap] to a previous format's map to retain identities. */
public data class CaptureRequest(
    val sampleRate: Int,
    val channels: Int,
    val validBits: Int,
    val channelMap: List<CaptureChannel>? = null,
)

/** Actual integer PCM bytes, including unmodified padding in each channel's subslot. */
public data class CaptureFormat(
    val sampleRate: Int,
    val channels: Int,
    val validBits: Int,
    val subslotBytes: Int,
    val channelMap: List<CaptureChannel>,
    val configurationId: Int,
    val interfaceNumber: Int,
    val altSetting: Int,
    val endpointAddress: Int,
) {
    public val bytesPerFrame: Int get() = channels * subslotBytes
}

public enum class CaptureState { ACTIVE, STOPPED, FAILED }

/** Transport loss, slow-reader backlog and duplex reader contention are separate counters. */
public data class CaptureStats(
    val state: CaptureState,
    val availableFrames: Long,
    val capturedFrames: Long,
    val deliveredFrames: Long,
    val overrunFrames: Long,
    val emptyPackets: Long,
    val shortPackets: Long,
    val failedPackets: Long,
    val malformedPackets: Long,
    val malformedCompletions: Long,
    val failedTransfers: Long,
    val missingFrames: Long,
    val reorderedCompletions: Long,
    /** Lower-bound device position; unknown transport loss cannot be inferred from IN capacity. */
    val devicePositionFrames: Long,
    val error: DacException?,
    /** Compatibility counter for frames dropped due to reader contention; duplex handoff no longer causes such drops. */
    val contentionFrames: Long = 0,
    /** Valid frames withheld during startup or rate proof. */
    val proofSuppressedFrames: Long = 0,
)

/**
 * Direct USB PCM input. Read whole interleaved frames in [format]; slow readers lose
 * frames and can inspect [CaptureStats.overrunFrames]. Standalone capture drops oldest frames; duplex drops newly arriving frames when its owned buffer pool is full. No conversion occurs.
 * Close off the main thread. In a duplex pair, closing this direction also closes playback.
 */
public class DacCaptureStream internal constructor(
    private val handle: Long,
    public val format: CaptureFormat,
    private val onClosed: () -> Unit,
) : Closeable {
    private val closed = AtomicBoolean(false)
    private var scratch: ByteBuffer? = null

    /**
     * Fill writable remaining bytes and advance position; return bytes, or zero when active and empty.
     * Do not read or write the selected direct memory concurrently, including through aliased
     * duplicate or sliced buffers, until this call returns.
     */
    @Synchronized
    public fun read(destination: ByteBuffer): Int {
        checkOpen()
        if (destination.isReadOnly) throw DacException(DacErrorCode.INVALID_ARGUMENT, "capture destination is read-only")
        val frameBytes = format.bytesPerFrame
        if (destination.remaining() % frameBytes != 0) {
            throw DacException(DacErrorCode.INVALID_ARGUMENT, "capture destination must contain whole wire frames")
        }
        if (destination.isDirect) {
            val copied = nativeCall { Native.captureRead(handle, destination, destination.position(), destination.remaining()) }
            destination.position(destination.position() + copied)
            return copied
        }
        val staging = scratch ?: ByteBuffer.allocateDirect(maxOf(256 * 1024, frameBytes)).also { scratch = it }
        val count = minOf(destination.remaining(), staging.capacity() / frameBytes * frameBytes)
        val copied = nativeCall { Native.captureRead(handle, staging, 0, count) }
        staging.position(0)
        staging.limit(copied)
        destination.put(staging)
        return copied
    }

    public val state: CaptureState get() = if (closed.get()) CaptureState.STOPPED else stats().state
    public val availableFrames: Long get() = if (closed.get()) 0 else stats().availableFrames

    public fun stats(): CaptureStats {
        checkOpen()
        return nativeCall { parseCaptureStats(Native.captureStats(handle)) }
    }

    /** Stop IN and wait for transfer retirement. Session ownership stays held on a timeout. */
    @Synchronized
    override fun close() {
        if (!closed.compareAndSet(false, true)) return
        try {
            nativeCall { Native.captureClose(handle) }
        } finally {
            onClosed()
        }
    }

    private fun checkOpen() {
        if (closed.get()) throw DacException(DacErrorCode.INVALID_STATE, "capture stream is closed")
    }
}
