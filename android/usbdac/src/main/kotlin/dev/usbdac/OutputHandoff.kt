package dev.usbdac

import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.NonCancellable
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.Flow
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.sync.Mutex
import kotlinx.coroutines.sync.withLock
import kotlinx.coroutines.withContext
import kotlinx.coroutines.withTimeoutOrNull

/** State of one player's output across track boundaries and USB reconnects. */
public sealed interface OutputHandoffState {
    public data object Idle : OutputHandoffState

    /**
     * The writer can use [output] after preparing its source at [positionUs], the media time
     * corresponding to frame zero of the output. The handoff owns and closes the output.
     */
    public data class Ready(val output: SelectedOutput, val positionUs: Long) : OutputHandoffState

    /** Direct output was lost. The player can retain its source at [positionUs]. */
    public data class WaitingForUsb(
        val device: DacDeviceInfo,
        val positionUs: Long,
        /** Whether a serial number was available before the device disappeared. */
        val canMatchAutomatically: Boolean,
    ) : OutputHandoffState
}

/**
 * Opt-in coordination of [OutputSelector] for a single player. Feed [changes] to [handleChange]
 * and report native device loss with [onDeviceLost]. The helper owns the output and serializes
 * open, close and reconnect. The player owns decoding, seeking to the reported position,
 * USB permission prompts and the write loop. Stop feeding the outgoing output before switching.
 */
public class OutputHandoff(private val selector: OutputSelector) {
    public val changes: Flow<OutputChange> get() = selector.changes

    private val mutex = Mutex()
    private val current = MutableStateFlow<OutputHandoffState>(OutputHandoffState.Idle)
    public val state: StateFlow<OutputHandoffState> = current.asStateFlow()

    private data class UsbTarget(val device: DacDeviceInfo, val serial: String?) {
        fun sameModel(candidate: DacDeviceInfo): Boolean =
            candidate.vendorId == device.vendorId && candidate.productId == device.productId

        fun sameAttachment(candidate: DacDeviceInfo): Boolean =
            sameModel(candidate) && candidate.deviceName == device.deviceName
    }

    private var usbTarget: UsbTarget? = null
    private var request: StreamRequest? = null
    private var playing = false

    /**
     * Drain the outgoing track before opening the next output. Pass `null` for [drainTimeoutMs]
     * to cut immediately (for example, when the listener chooses another track). A drain timeout
     * releases the outgoing route and reports [DacErrorCode.TRANSFER] without opening a new one.
     *
     * [playWhenReady] is the player's current intent. A paused output can buffer data for later.
     * Call on track changes after the last write to the outgoing output.
     */
    public suspend fun open(
        request: StreamRequest,
        positionUs: Long = 0,
        playWhenReady: Boolean = true,
        drainTimeoutMs: Long? = 5_000,
    ): OutputHandoffState.Ready = locked {
        require(positionUs >= 0)
        require(drainTimeoutMs == null || drainTimeoutMs > 0)
        val old = current.value as? OutputHandoffState.Ready
        clear()
        if (old != null) retire(old.output, if (playing) drainTimeoutMs else null)
        playing = playWhenReady
        install(request, selector.open(request), positionUs)
    }

    /** Preserve pause/resume intent even while a USB device is absent. */
    public suspend fun setPlaying(play: Boolean): Unit = locked {
        playing = play
        (current.value as? OutputHandoffState.Ready)?.output?.let { output ->
            if (play) output.play() else output.pause()
        }
        if (play) resumeWaiting()
        Unit
    }

    /** Flush queued audio and anchor the next frames to the media time after a seek. */
    public suspend fun seekTo(positionUs: Long): Unit = locked {
        require(positionUs >= 0)
        when (val previous = current.value) {
            is OutputHandoffState.Ready -> {
                previous.output.flush()
                current.value = previous.copy(positionUs = positionUs)
            }
            is OutputHandoffState.WaitingForUsb -> current.value = previous.copy(positionUs = positionUs)
            OutputHandoffState.Idle -> Unit
        }
    }

    /**
     * Report a transport device-loss event. A stale event from a closed output is ignored.
     * A direct stream only reopens on a matching DAC and never falls back during reconnect.
     */
    public suspend fun onDeviceLost(output: SelectedOutput): Unit = locked {
        (current.value as? OutputHandoffState.Ready)?.takeIf { it.output === output }?.let(::lose)
        Unit
    }

    /**
     * Handle selector detach, attach and permission events. Repeated events are harmless.
     * A reconnect attempt that fails leaves the saved position and intent available for a retry.
     */
    public suspend fun handleChange(change: OutputChange): Unit = locked {
        when (change) {
            is OutputChange.DacDetached -> {
                val ready = current.value as? OutputHandoffState.Ready
                val target = usbTarget
                if (ready != null && target != null && target.sameAttachment(change.dac) &&
                    !selector.hasAttachedUsb(target::sameAttachment)) {
                    lose(ready)
                }
            }
            is OutputChange.DacAttached, is OutputChange.PermissionGranted,
            is OutputChange.RouteChanged -> if (playing) resumeWaiting()
        }
        Unit
    }

    /**
     * Explicitly resume on a device selected by the listener, including a DAC without a readable
     * serial. The caller has obtained USB permission and confirmed the physical device.
     */
    public suspend fun reconnectTo(device: DacDeviceInfo): OutputHandoffState.Ready = locked {
        val waiting = current.value as? OutputHandoffState.WaitingForUsb
            ?: throw DacException(DacErrorCode.INVALID_STATE, "no direct USB output is waiting to reconnect")
        val target = checkNotNull(usbTarget)
        require(target.sameModel(device))
        val selected = checkNotNull(request)
        val output = selector.openMatchingUsb(selected) { candidate ->
            candidate.deviceName == device.deviceName && target.sameModel(candidate)
                && (target.serial == null || selector.usbSerial(candidate) == target.serial)
        }
        install(selected, output, waiting.positionUs)
    }

    /**
     * Release a failed output and reopen its request on an app-chosen route, at the last
     * reported media position. Call before [onDeviceLost] when the app wants to switch
     * routes instead of waiting for USB. A failure from an older output is ignored.
     *
     * Leaving direct USB for an output that could play on the phone requires
     * [allowSystemRoute]. Android's USB bit-perfect mixer requires an active USB route.
     */
    public suspend fun fallbackAfterFailure(
        failed: SelectedOutput,
        kind: OutputKind,
        allowSystemRoute: Boolean = false,
    ): OutputHandoffState.Ready? = locked {
        val ready = current.value as? OutputHandoffState.Ready
        if (ready == null || ready.output !== failed) return@locked null
        require(kind != failed.kind && kind != OutputKind.DIRECT_USB)
        require(allowSystemRoute || failed.kind != OutputKind.DIRECT_USB || kind == OutputKind.ANDROID_BIT_PERFECT)
        if (selector.policy.force != null && selector.policy.force != kind) {
            throw DacException(DacErrorCode.INVALID_STATE, "the handoff target conflicts with the forced output policy")
        }
        val selected = checkNotNull(request)
        val position = positionOf(ready)
        // Keep the original USB reconnect intent if Android has yet to
        // reclaim the DAC or the selected fallback cannot open.
        retireLost(ready, position)
        install(selected, selector.openForHandoff(selected, kind), position)
    }

    /** Close any owned output and forget saved reconnect state, including in a cancelled scope. */
    public suspend fun close(): Unit = withContext(NonCancellable) {
        locked {
            val output = (current.value as? OutputHandoffState.Ready)?.output
            clear()
            playing = false
            output?.close()
        }
    }

    private suspend fun <T> locked(op: suspend () -> T): T = mutex.withLock {
        withContext(Dispatchers.IO) { op() }
    }

    private fun clear() {
        current.value = OutputHandoffState.Idle
        request = null
        usbTarget = null
    }

    private fun install(request: StreamRequest, output: SelectedOutput, positionUs: Long): OutputHandoffState.Ready {
        try {
            if (!playing) output.pause()
            usbTarget = output.decision.dac?.let { UsbTarget(it, selector.usbSerial(it)) }
            this.request = request
            return OutputHandoffState.Ready(output, positionUs).also { current.value = it }
        } catch (error: Throwable) {
            try {
                output.close()
            } catch (closeError: Exception) {
                error.addSuppressed(closeError)
            }
            throw error
        }
    }

    private suspend fun retire(output: SelectedOutput, timeoutMs: Long?) {
        try {
            if (timeoutMs != null) {
                output.drain()
                val drained = withTimeoutOrNull(timeoutMs) {
                    while (!output.isDrained) delay(10)
                    true
                }
                if (drained != true) throw DacException(DacErrorCode.TRANSFER, "outgoing route did not drain")
            }
        } finally {
            output.close()
        }
    }

    private fun positionOf(ready: OutputHandoffState.Ready): Long {
        val output = ready.output
        val frames = try {
            output.playedFrames.coerceAtLeast(0)
        } catch (_: DacException) {
            return ready.positionUs
        }
        val rate = output.format.counterRateHz.toLong()
        val remainingUs = Long.MAX_VALUE - ready.positionUs
        val seconds = frames / rate
        if (seconds > remainingUs / 1_000_000) return Long.MAX_VALUE
        val wholeUs = seconds * 1_000_000
        val fractionUs = ((frames % rate) * 1_000_000) / rate
        return ready.positionUs + wholeUs + minOf(fractionUs, remainingUs - wholeUs)
    }

    private fun lose(ready: OutputHandoffState.Ready) = retireLost(ready, positionOf(ready))

    private fun retireLost(ready: OutputHandoffState.Ready, positionUs: Long) {
        val target = usbTarget
        if (target == null) {
            clear()
        } else {
            current.value = OutputHandoffState.WaitingForUsb(target.device, positionUs, target.serial != null)
        }
        ready.output.close()
    }

    private fun resumeWaiting() {
        val waiting = current.value as? OutputHandoffState.WaitingForUsb ?: return
        val target = checkNotNull(usbTarget)
        val serial = target.serial ?: return
        val selected = checkNotNull(request)
        val output = try {
            selector.openMatchingUsb(selected) { target.sameModel(it) && selector.usbSerial(it) == serial }
        } catch (error: DacException) {
            when (error.code) {
                DacErrorCode.DEVICE_LOST, DacErrorCode.PERMISSION_DENIED, DacErrorCode.UNSUPPORTED -> return
                else -> throw error
            }
        }
        install(selected, output, waiting.positionUs)
    }
}
