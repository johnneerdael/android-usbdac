package dev.usbdac.controls

import dev.usbdac.PcmOutput
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.NonCancellable
import kotlinx.coroutines.currentCoroutineContext
import kotlinx.coroutines.delay
import kotlinx.coroutines.ensureActive
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.launch
import kotlinx.coroutines.sync.Mutex
import kotlinx.coroutines.sync.withLock
import kotlinx.coroutines.withContext
import kotlin.coroutines.CoroutineContext

/**
 * Closes the output after playback has been paused for a while, and opens it again on resume.
 *
 * A paused direct stream keeps the USB output thread running and sends silence so the DAC stays
 * locked. After a long pause that is wasted power. This class closes the stream after
 * [timeoutMs] of continuous pause and, when playback resumes, calls your [reopen] lambda to
 * open a new one. The [dev.usbdac.DacSession] stays open, so the DAC volume keeps working while the
 * stream is released. Reopening costs 100 ms or more (negotiate and open); the first audio after
 * a long pause is delayed by that.
 *
 * ```
 * val idle = DacIdleRelease(scope, reopen = { session.openStream(session.negotiate(request)) })
 * idle.attach(stream)
 * // when the player pauses:   idle.onPaused()
 * // when the player resumes:  stream = idle.onResumed()   // same stream, or a reopened one
 * ```
 *
 * Closing discards queued audio that the DAC has not played. Before resuming a decoder-backed player,
 * save the audible media position (not the decoder/write position). Use [resume] to learn atomically
 * whether a new output was opened. Only when its [IdleResume.reopened] is true, flush/rebase the
 * player's counters and seek the decoder to that saved position before supplying PCM. A reopened
 * stream starts at zero played frames; continuing with the decoder's next buffer would skip the
 * discarded queue. A short pause returns the original output with its queue intact and requires no
 * reseek. This class does not own the decoder and cannot perform that reseek.
 *
 * It is generic over [PcmOutput], so it works for any output, though only a direct USB stream
 * has anything to gain.
 *
 * @param scope runs the timer; cancel it with the player.
 * @param timeoutMs continuous pause before the output is closed; default [DEFAULT_TIMEOUT_MS].
 * @param reopen opens a new output the way the host opened the first one. Runs from [onResumed].
 * @param closeContext where the blocking close runs; default [Dispatchers.IO].
 */
public class DacIdleRelease<T : PcmOutput>(
    private val scope: CoroutineScope,
    private val timeoutMs: Long = DEFAULT_TIMEOUT_MS,
    private val reopen: suspend () -> T,
    private val closeContext: CoroutineContext = Dispatchers.IO,
) {
    private val lock = Any()
    private val transition = Mutex()
    private var timer: Job? = null
    private var current: T? = null
    private val mutableReleased = MutableStateFlow(false)

    /** `true` while the output has been closed for idleness and not yet reopened. */
    public val isReleased: StateFlow<Boolean> get() = mutableReleased

    /** The output playback uses now, or `null` while it is released. */
    public val output: T? get() = synchronized(lock) { current }

    /** Hands over the output the host opened. Cancels a running timer. */
    public fun attach(output: T) {
        synchronized(lock) {
            timer?.cancel()
            timer = null
            current = output
            mutableReleased.value = false
        }
    }

    /** Playback paused: starts the timer. Does nothing if it is already running or nothing is attached. */
    public fun onPaused() {
        synchronized(lock) {
            if (current == null || timer?.isActive == true) return
            timer = scope.launch {
                delay(timeoutMs)
                transition.withLock {
                    // A resume that arrived while this waited for the lock cancelled the job.
                    currentCoroutineContext().ensureActive()
                    val output = synchronized(lock) { current } ?: return@withLock
                    withContext(NonCancellable + closeContext) {
                        runCatching { output.close() }
                        synchronized(lock) {
                            // attach may replace the output while a blocking close finishes.
                            if (current === output) {
                                current = null
                                mutableReleased.value = true
                            }
                        }
                    }
                }
            }
        }
    }

    /**
     * Playback resumed: cancels the timer and returns the output to write to: the one still open,
     * or, if it was released, a new one from the [reopen] lambda. May take 100 ms or more when it
     * reopens; the reopen failing throws what the lambda throws and leaves the output released.
     */
    public suspend fun onResumed(): T = resume().output

    /** Resume with an atomic indication of whether idleness discarded the old output and queue. */
    public suspend fun resume(): IdleResume<T> {
        synchronized(lock) {
            timer?.cancel()
            timer = null
        }
        return transition.withLock {
            val existing = synchronized(lock) { current }
            if (existing != null) IdleResume(existing, false) else {
                val opened = reopen()
                val attached = synchronized(lock) {
                    val attached = current
                    if (attached == null) {
                        current = opened
                        mutableReleased.value = false
                    }
                    attached
                }
                if (attached != null) {
                    withContext(NonCancellable + closeContext) { opened.close() }
                    IdleResume(attached, false)
                } else IdleResume(opened, true)
            }
        }
    }

    /** Stops the timer without closing anything, for when the host closes the output itself. */
    public fun cancel() {
        synchronized(lock) {
            timer?.cancel()
            timer = null
            current = null
        }
    }

    public companion object {
        /** Continuous pause before the output is released: 30 seconds. */
        public const val DEFAULT_TIMEOUT_MS: Long = 30_000
    }
}

/** A resumed output and whether its old queued audio was discarded by idle release. */
public data class IdleResume<T : PcmOutput>(
    /** The output playback should now use. */
    public val output: T,
    /** True only when this resume opened a replacement output; reseek the decoder in that case. */
    public val reopened: Boolean,
)
