package dev.usbdac

import kotlinx.coroutines.flow.StateFlow
import java.io.Closeable
import java.nio.ByteBuffer

/**
 * Where a player sends audio: either a direct USB stream ([DacStream]) or Android's own audio
 * stack ([AndroidManagedOutput]). The two share this interface so a player can use either.
 *
 * The audio is bytes in the layout the output reports: for a [DacStream], the wire layout of
 * its [StreamFormat] (little-endian, interleaved, left-justified in the subslot); for an
 * [AndroidManagedOutput], the PCM encoding of its `AudioTrack`.
 *
 * ### Threading
 * [write] never blocks; it takes what fits and returns the count. Use one writer thread: write
 * is thread-safe, but the bytes form one ordered sequence. The control calls and getters are
 * safe from any thread and do not block for long. [close] may take about a tenth of a second.
 */
public interface PcmOutput : Closeable {
    /**
     * Writes whole frames from the buffer's position and advances the position by the bytes taken.
     * Direct buffers are read in place; a heap buffer is copied. Returns the bytes taken, which can
     * be less than `buffer.remaining()` (the output is full: keep the rest and try again), and 0
     * for a partial frame.
     *
     * @throws DacException with [DacErrorCode.INVALID_STATE] after [drain] (until [flush]), after
     *   the output closed, or after it failed.
     */
    public fun write(buffer: ByteBuffer): Int

    /**
     * How many bytes a writer should write now: the room up to the target depth, a whole number of
     * frames. A pull loop writes this much and sleeps when it is 0. It is a target, not a hard limit.
     */
    public val writableBytes: Int

    /** Starts or resumes output. A new stream starts by itself once enough audio is buffered. */
    public fun play()

    /**
     * Pauses output without discarding what is buffered. [play] continues exactly where it stopped.
     * Audio already handed to the platform or the bus still plays out first (milliseconds).
     */
    public fun pause()

    /**
     * Discards everything buffered and resets [playedFrames] to 0. Use it for a seek, a stop, or a
     * track the user picked; it keeps a deep buffer from costing responsiveness. On the direct USB
     * output with [DacConfig.cancelInFlightOnFlush] (on by default), the output thread also asks the
     * host controller to cancel the submitted transfers that carry music from before the flush, waits
     * up to a second for them to be handed back (the stream fails if they are not), and sends silence
     * until the ring refills. A transfer that completes before its cancellation takes effect still
     * plays, and no time to silence is guaranteed. With the flag off, or on the managed output, audio
     * already on the bus or in the platform's last buffer may still be heard.
     */
    public fun flush()

    /**
     * Declares the end of the audio: what is buffered plays out, including a tail shorter than the
     * start threshold. Do not call it between tracks of a gapless sequence; keep writing instead.
     * Writes fail until [flush].
     */
    public fun drain()

    /** `true` once everything written before [drain] has played. */
    public val isDrained: Boolean

    /**
     * Frames that have actually played since the start or the last [flush], in the output's own
     * frame rate. Silence inserted while paused or underrun does not count. Audible position is
     * `startPosition + playedFrames / frameRate`.
     */
    public val playedFrames: Long

    /** Frames between the host and the DAC: what is buffered plus what is in flight. */
    public val latencyFrames: Long

    /** Whether the output is bit-perfect, and how well that can be known; see [BitPerfectState]. */
    public val bitPerfect: StateFlow<BitPerfectState>

    /** Stops the output and releases it. Safe to call again. */
    override fun close()
}
