package dev.usbdac

import dev.usbdac.internal.DsdPcmNative
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import java.nio.ByteBuffer

/** Lossy native DSD to PCM fallback. Input must be channel-interleaved chronological
 * DSD bytes, oldest bit first; DoP and arbitrary PCM are not accepted. DSD64/128/256
 * are supported. Source positions are DSD byte-frames at [dsdBitRate] / 8.
 * Construction synthesizes filters and belongs off the audio thread.
 */
public class DsdPcmOutput(
    private val target: PcmOutput,
    private val dsdBitRate: Int,
    private val channels: Int,
    private val targetRate: Int,
    private val targetChannels: Int,
    private val targetLayout: PcmLayout,
    options: ConversionOptions = ConversionOptions(),
) : PcmOutput {
    private val lock = Any()
    private val handle = run {
        require(options.sourceLayout == null) { "PCM source layout is not valid for native DSD" }
        require(options.ditherSeed == null || options.ditherSeed >= 0) { "dither seed must not be negative" }
        nativeCall { DsdPcmNative.create(dsdBitRate, channels, targetRate, targetChannels, targetLayout.bits, targetLayout.slotBytes,
            if (options.quality == ResampleQuality.HIGH) 0 else 1,
            if (options.narrowing == NarrowingMode.TPDF_DITHER) 0 else 1,
            options.headroomDb, options.ditherSeed ?: -1) }
    }
    init {
        try { if (target is DacStream) target.setHostProcessing(true) }
        catch (error: Throwable) { nativeCall { DsdPcmNative.close(handle) }; throw error }
    }
    /** Number of samples saturated by PCM quantization since open or flush. */
    public val clippedSamples: Long get() = synchronized(lock) { if (closed) 0 else nativeCall { DsdPcmNative.clipped(handle) } }
    /** Algorithmic filter latency, in source byte-frames; this is not a position offset. */
    public val filterDelayByteFrames: Int = nativeCall { DsdPcmNative.delay(handle) }
    private val pending = ByteBuffer.allocateDirect(64 * 1024).apply { limit(0) }
    private val stage = ByteBuffer.allocateDirect(64 * 1024)
    private var draining = false
    private var tailDone = false
    private var targetDrained = false
    private var closed = false
    private var consumedFrames = 0L
    // PcmOutput.write returns the position delta, and Media3's AudioSink.handleBuffer must report
    // what it consumed: a target failure after this call consumed source is returned as that count
    // and rethrown, unchanged, by the next call.
    private var targetFailure: DacException? = null
    private inline fun <T> targetCall(block: () -> T): T {
        targetFailure?.let { throw it }
        return try { block() } catch (error: DacException) { targetFailure = error; throw error }
    }
    override val bitPerfect: StateFlow<BitPerfectState> = MutableStateFlow(BitPerfectState.NotVerified(null, "Native DSD converted to PCM"))
    private fun checkOpen() { check(!closed); targetFailure?.let { throw it } }
    private fun pump() {
        while (true) {
            if (pending.hasRemaining()) { targetCall { target.write(pending) }; if (pending.hasRemaining()) return }
            if (!draining) return
            if (!tailDone) {
                pending.clear()
                val n = try { nativeCall { DsdPcmNative.finish(handle, pending, 0, pending.capacity()) } } catch (error: Throwable) { pending.limit(0); throw error }
                if (n == 0) { pending.limit(0); tailDone = true } else { pending.position(0); pending.limit(n) }
                continue
            }
            if (!targetDrained) { targetCall { target.drain() }; targetDrained = true }
            return
        }
    }
    override fun write(buffer: ByteBuffer): Int = synchronized(lock) {
        checkOpen(); check(!draining); pump()
        if (pending.hasRemaining()) return 0
        var total = 0
        while (buffer.remaining() >= channels) {
            val input = if (buffer.isDirect) buffer else {
                stage.clear(); val n = minOf(buffer.remaining(), stage.capacity()); stage.put(buffer.duplicate().apply { limit(position() + n) }); stage.flip(); stage
            }
            pending.clear()
            val result = try { nativeCall { DsdPcmNative.process(handle, input, input.position(), input.remaining(), pending, 0, pending.capacity()) } } catch (error: Throwable) { pending.limit(0); throw error }
            val used = (result ushr 32).toInt(); val made = result.toInt()
            buffer.position(buffer.position() + used); total += used; consumedFrames += used / channels
            pending.position(0); pending.limit(made)
            try { pump() } catch (error: DacException) { if (total > 0) return total; throw error }
            if (pending.hasRemaining() || (used == 0 && made == 0)) break
        }
        total
    }
    override val writableBytes: Int get() = synchronized(lock) {
        if (closed) 0 else { targetFailure?.let { throw it }; if (draining) 0 else { pump(); if (pending.hasRemaining()) 0 else {
            val frames = target.writableBytes.toLong() / (targetChannels * targetLayout.slotBytes)
            (frames * (dsdBitRate / 8L) / targetRate * channels).coerceAtMost(Int.MAX_VALUE.toLong()).toInt()
        } } }
    }
    override fun play(): Unit = synchronized(lock) { checkOpen(); targetCall { target.play() } }
    override fun pause(): Unit = synchronized(lock) { checkOpen(); targetCall { target.pause() } }
    override fun flush(): Unit = synchronized(lock) {
        checkOpen(); targetCall { target.flush() }; nativeCall { DsdPcmNative.reset(handle) }; pending.clear(); pending.limit(0)
        consumedFrames = 0; draining = false; tailDone = false; targetDrained = false
    }
    override fun drain(): Unit = synchronized(lock) { checkOpen(); draining = true; pump() }
    override val isDrained: Boolean get() = synchronized(lock) { if (closed) false else { targetFailure?.let { throw it }; if (!draining) false else { pump(); targetDrained && targetCall { target.isDrained } } } }
    override val playedFrames: Long get() = synchronized(lock) { if (closed) 0 else (target.playedFrames * (dsdBitRate / 8L) / targetRate).coerceAtMost(consumedFrames) }
    override val latencyFrames: Long get() = synchronized(lock) { if (closed) 0 else (consumedFrames - playedFrames).coerceAtLeast(0) }
    override fun close(): Unit = synchronized(lock) { if (!closed) { closed = true; try { target.close() } finally { nativeCall { DsdPcmNative.close(handle) } } } }
}
