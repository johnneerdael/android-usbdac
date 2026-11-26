package dev.usbdac.sample

import dev.usbdac.DacException
import dev.usbdac.PcmOutput
import java.nio.ByteBuffer
import java.nio.ByteOrder
import kotlin.math.PI
import kotlin.math.roundToLong
import kotlin.math.sin

/**
 * A writer thread that fills a [PcmOutput] with a sine tone.
 *
 * It shows the usual pull loop: render only when the output wants more
 * ([PcmOutput.writableBytes]), write what fits, keep the rest for the next round. The samples are
 * written in the wire layout the stream reports: little-endian, channels interleaved, each sample
 * left-justified in a `subslotBytes` slot.
 */
class SinePlayer(
    private val output: PcmOutput,
    private val sampleRate: Int,
    private val channels: Int,
    private val subslotBytes: Int,
    private val validBits: Int,
    private val onFailure: (DacException) -> Unit,
) {
    @Volatile
    private var running = false
    private var thread: Thread? = null

    fun start() {
        running = true
        thread = Thread(::pump, "sine-writer").also { it.start() }
    }

    fun stop() {
        running = false
        thread?.join()
        thread = null
    }

    private fun pump() {
        val frameBytes = channels * subslotBytes
        // A direct buffer is read in place by the library; a heap buffer would be copied.
        val chunk = ByteBuffer.allocateDirect(sampleRate / 100 * frameBytes).order(ByteOrder.LITTLE_ENDIAN)
        chunk.limit(0) // nothing rendered yet
        var phase = 0.0
        try {
            while (running) {
                if (!chunk.hasRemaining()) {
                    // The ring is full enough; wait instead of rendering audio nobody can take.
                    if (output.writableBytes < chunk.capacity()) {
                        Thread.sleep(2)
                        continue
                    }
                    phase = render(chunk, phase)
                }
                // write() never blocks and advances the position by the bytes it took.
                if (output.write(chunk) == 0) Thread.sleep(2)
            }
        } catch (error: DacException) {
            onFailure(error)
        } catch (_: InterruptedException) {
            // stop() joins; nothing to do.
        }
    }

    /** Fills [chunk] with whole frames of the tone and flips it for writing. Returns the next phase. */
    private fun render(chunk: ByteBuffer, startPhase: Double): Double {
        val fullScale = (1L shl (validBits - 1)) - 1
        val shift = subslotBytes * 8 - validBits // left-justify the sample in its slot
        val step = 2 * PI * TONE_HZ / sampleRate
        var phase = startPhase
        chunk.clear()
        repeat(chunk.capacity() / (channels * subslotBytes)) {
            val sample = (sin(phase) * AMPLITUDE * fullScale).roundToLong().toInt() shl shift
            repeat(channels) {
                for (byte in 0 until subslotBytes) chunk.put((sample shr (8 * byte)).toByte())
            }
            phase += step
            if (phase > 2 * PI) phase -= 2 * PI
        }
        chunk.flip()
        return phase
    }

    private companion object {
        const val TONE_HZ = 1000.0

        /** About -20 dBFS: audible without being loud. */
        const val AMPLITUDE = 0.1
    }
}
