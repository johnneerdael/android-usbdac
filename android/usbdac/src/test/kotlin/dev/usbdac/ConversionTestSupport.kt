package dev.usbdac

import dev.usbdac.internal.ConvertBackend
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import java.nio.ByteBuffer

/** The JSON `convertPlan` returns for a plan, assembled for a test. */
internal fun planJson(
    kind: String = "lossy",
    description: String = "Lossy conversion, not bit-perfect: resampled from 24000 Hz to 48000 Hz.",
    sourceRate: Int = 24_000,
    targetRate: Int = 48_000,
    sourceFrameBytes: Int = 2,
    targetFrameBytes: Int = 2,
    resamples: Boolean = true,
    delayFrames: Int = 3,
    minOutputBytes: Int = 4,
    steps: String = """[ { "code": "resample", "kind": "lossy", "description": "resampled from 24000 Hz to 48000 Hz" } ]""",
    filter: String = """{ "l": 2, "m": 1, "taps": 6, "passband_hz": 10920.0, "stopband_hz": 12000.0, "design_attenuation_db": 110.0 }""",
): String =
    """
    { "kind": "$kind", "description": "$description", "preserves_bit_perfect": ${kind == "none" || kind == "bit_exact"},
      "resamples": $resamples, "steps": $steps,
      "source": { "sample_rate": $sourceRate, "channels": 1, "sample": { "kind": "int", "bits": 16, "slot_bytes": 2 }, "bytes_per_frame": $sourceFrameBytes },
      "target": { "sample_rate": $targetRate, "channels": 1, "sample": { "kind": "int", "bits": 16, "slot_bytes": 2 }, "bytes_per_frame": $targetFrameBytes },
      "source_frame_bytes": $sourceFrameBytes, "target_frame_bytes": $targetFrameBytes,
      "delay_frames": $delayFrames, "min_output_bytes": $minOutputBytes,
      "filter": ${if (resamples) filter else "null"} }
    """.trimIndent()

/**
 * The native conversion layer, faked. It does one easy conversion that is easy to check: every source
 * frame becomes [factor] copies of itself (so the target rate is [factor] times the source rate), and
 * `finish` writes [tail] frames of the byte 0xEE.
 */
internal class FakeConvertBackend(
    var plan: String = planJson(),
    private val factor: Int = 2,
    private val frameBytes: Int = 2,
    private val tail: Int = 3,
) : ConvertBackend {
    val calls = mutableListOf<String>()
    var requestJson: String? = null
    var planRequest: String? = null
    var failPlan: Throwable? = null
    var failCreate: Throwable? = null
    var clipped = 0L
    private var tailLeft = tail
    private var finished = false
    var lastInOffset = -1
    var lastOutOffset = -1

    override fun plan(requestJson: String): String {
        calls += "plan"
        planRequest = requestJson
        failPlan?.let { throw it }
        return plan
    }

    override fun create(requestJson: String): Long {
        calls += "create"
        failCreate?.let { throw it }
        this.requestJson = requestJson
        return 9
    }

    override fun close(handle: Long) {
        calls += "close"
    }

    override fun process(handle: Long, input: ByteBuffer, inOffset: Int, inCount: Int, output: ByteBuffer, outOffset: Int, outCapacity: Int): Long {
        calls += "process"
        if (finished) throw IllegalStateException("usbdac:invalid_state: finish() was called: reset() before converting more")
        lastInOffset = inOffset
        lastOutOffset = outOffset
        val frames = minOf(inCount / frameBytes, outCapacity / (frameBytes * factor) )
        var out = outOffset
        for (frame in 0 until frames) {
            repeat(factor) {
                for (i in 0 until frameBytes) output.put(out++, input.get(inOffset + frame * frameBytes + i))
            }
        }
        val consumed = frames * frameBytes
        val produced = frames * frameBytes * factor
        return (consumed.toLong() shl 32) or produced.toLong()
    }

    override fun finish(handle: Long, output: ByteBuffer, outOffset: Int, outCapacity: Int): Int {
        calls += "finish"
        finished = true
        val frames = minOf(tailLeft, outCapacity / frameBytes)
        for (i in 0 until frames * frameBytes) output.put(outOffset + i, 0xEE.toByte())
        tailLeft -= frames
        return frames * frameBytes
    }

    override fun reset(handle: Long) {
        calls += "reset"
        tailLeft = tail
        finished = false
    }

    override fun clipped(handle: Long): Long = clipped
}

/** A target output that records what it is given and takes what [room] still has: a budget that each write uses up. */
internal class RecordingOutput : PcmOutput {
    val calls = mutableListOf<String>()
    val received = ArrayList<Byte>()
    var room = Int.MAX_VALUE
    var writable = 100_000
    var played = 0L
    var latency = 0L
    var drained = false
    var closed = false
    var verified: BitPerfectState = BitPerfectState.Verified
    private val state = MutableStateFlow<BitPerfectState>(BitPerfectState.Verified)
    override val bitPerfect: StateFlow<BitPerfectState> get() = state

    fun setBitPerfect(value: BitPerfectState) {
        state.value = value
    }

    override fun write(buffer: ByteBuffer): Int {
        calls += "write"
        val taken = minOf(buffer.remaining(), room)
        for (i in 0 until taken) received += buffer.get()
        room -= taken
        return taken
    }

    override val writableBytes: Int get() = writable
    override fun play() {
        calls += "play"
    }

    override fun pause() {
        calls += "pause"
    }

    override fun flush() {
        calls += "flush"
    }

    override fun drain() {
        calls += "drain"
    }

    override val isDrained: Boolean get() = drained
    override val playedFrames: Long get() = played
    override val latencyFrames: Long get() = latency
    override fun close() {
        calls += "close"
        closed = true
    }
}

internal fun directOf(vararg bytes: Int): ByteBuffer =
    ByteBuffer.allocateDirect(bytes.size).also { buffer ->
        bytes.forEach { buffer.put(it.toByte()) }
        buffer.flip()
    }

internal fun heapOf(vararg bytes: Int): ByteBuffer = ByteBuffer.wrap(ByteArray(bytes.size) { bytes[it].toByte() })
