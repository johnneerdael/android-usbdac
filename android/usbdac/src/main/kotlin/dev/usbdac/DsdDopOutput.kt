package dev.usbdac

import java.nio.ByteBuffer
import kotlinx.coroutines.flow.StateFlow

/** Exact native DSD to DoP framing for an independently confirmed DoP route.
 * Input is chronological interleaved bytes with the oldest bit in bit 7. One
 * byte-frame can be held across writes; drain completes an incomplete final pair
 * with the DSD idle byte `0x69` as its later byte, under the next marker.
 * Position and latency use DSD byte-frames at the bit rate divided by eight.
 */
public class DsdDopOutput(
    private val target: PcmOutput,
    private val channels: Int,
    private val slotBytes: Int,
    confirmedDop: Boolean,
) : PcmOutput {
    init {
        require(confirmedDop) { "DoP decoding must be independently confirmed" }
        require(channels in 1..8)
        require(slotBytes == 3 || slotBytes == 4)
    }

    private val lock = Any()
    private val carrierFrame = channels * slotBytes
    private val pending = ByteBuffer.allocateDirect((64 * 1024 / carrierFrame) * carrierFrame).apply { limit(0) }
    private val first = ByteArray(channels)
    private var haveFirst = false
    private var marker = 0x05
    private var draining = false
    private var targetDrained = false
    private var closed = false
    // PcmOutput.write returns the position delta, and Media3's AudioSink.handleBuffer must report
    // what it consumed: a target failure after this call accepted input is returned as that count
    // and rethrown, unchanged, by the next call.
    private var targetFailure: DacException? = null

    private inline fun <T> targetCall(block: () -> T): T {
        targetFailure?.let { throw it }
        return try { block() } catch (error: DacException) { targetFailure = error; throw error }
    }
    override val bitPerfect: StateFlow<BitPerfectState> get() = target.bitPerfect

    private fun pump() {
        if (pending.hasRemaining()) targetCall { target.write(pending) }
    }

    /** Frames the held older byte-frame and [later] per channel under the current marker. */
    private inline fun putCarrierFrame(later: (Int) -> Byte) {
        for (ch in 0 until channels) {
            if (slotBytes == 4) pending.put(0)
            pending.put(later(ch))
            pending.put(first[ch])
            pending.put(marker.toByte())
        }
        haveFirst = false
        marker = marker xor 0xff
    }

    override fun write(buffer: ByteBuffer): Int = synchronized(lock) {
        check(!closed)
        targetFailure?.let { throw it }
        check(!draining)
        pump()
        if (pending.hasRemaining()) return 0
        val start = buffer.position()
        pending.clear()
        while (buffer.remaining() >= channels && pending.remaining() >= carrierFrame) {
            if (!haveFirst) {
                buffer.get(first)
                haveFirst = true
                continue
            }
            val p = buffer.position()
            putCarrierFrame { ch -> buffer.get(p + ch) }
            buffer.position(p + channels)
        }
        pending.flip()
        try { pump() } catch (_: DacException) { return@synchronized buffer.position() - start }
        buffer.position() - start
    }

    override val writableBytes: Int get() = synchronized(lock) {
        if (closed) return 0
        targetFailure?.let { throw it }
        if (draining) return 0
        pump()
        if (pending.hasRemaining()) return 0
        val frames = target.writableBytes / carrierFrame
        ((frames.toLong() * 2 + if (haveFirst) 0 else 1) * channels)
            .coerceAtMost(Int.MAX_VALUE.toLong()).toInt()
    }

    override fun play(): Unit = synchronized(lock) {
        check(!closed)
        targetCall { target.play() }
    }

    override fun pause(): Unit = synchronized(lock) {
        check(!closed)
        targetCall { target.pause() }
    }

    override fun flush(): Unit = synchronized(lock) {
        check(!closed)
        targetCall { target.flush() }
        pending.clear()
        pending.limit(0)
        marker = 0x05
        haveFirst = false
        draining = false
        targetDrained = false
    }

    private fun pumpDrain() {
        pump()
        if (!pending.hasRemaining() && !targetDrained) {
            targetCall { target.drain() }
            targetDrained = true
        }
    }

    override fun drain(): Unit = synchronized(lock) {
        check(!closed)
        if (haveFirst) {
            // DoP v1.1 section 2 carries 16 DSD bits per channel under a marker that alternates
            // 0x05/0xFA per sample frame, the same in every channel (ALSA sound/usb/pcm.c
            // fill_playback_urb_dsd_dop). A held older byte-frame is completed in the source
            // domain, before framing, with DSD idle, ALSA's DSD silence byte (sound/core/pcm_misc.c
            // pcm_formats[DSD_U8].silence = 0x69), so the frame takes the next marker in sequence.
            // write() reserved this frame's room when it held the byte-frame.
            pending.compact()
            putCarrierFrame { DSD_IDLE }
            pending.flip()
        }
        draining = true
        pumpDrain()
    }

    override val isDrained: Boolean get() = synchronized(lock) {
        if (closed) return false
        targetFailure?.let { throw it }
        if (!draining) return false
        pumpDrain()
        targetDrained && targetCall { target.isDrained }
    }

    override val playedFrames: Long get() = synchronized(lock) {
        if (closed) 0 else target.playedFrames * 2
    }

    override val latencyFrames: Long get() = synchronized(lock) {
        if (closed) 0 else (target.latencyFrames + pending.remaining() / carrierFrame) * 2 + if (haveFirst) 1 else 0
    }

    override fun close(): Unit = synchronized(lock) {
        if (!closed) {
            closed = true
            target.close()
        }
    }
}
