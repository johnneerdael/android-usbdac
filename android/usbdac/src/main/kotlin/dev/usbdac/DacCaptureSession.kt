package dev.usbdac

import android.hardware.usb.UsbDeviceConnection
import dev.usbdac.internal.ClaimedInterfaces
import dev.usbdac.internal.Native
import dev.usbdac.internal.NativeListener
import dev.usbdac.internal.parseCaptureFormat
import dev.usbdac.internal.toCaptureJson
import dev.usbdac.internal.logWarning
import kotlinx.coroutines.channels.BufferOverflow
import kotlinx.coroutines.flow.MutableSharedFlow
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.SharedFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.update
import java.io.Closeable
import java.util.concurrent.atomic.AtomicBoolean

/**
 * An explicitly opened USB capture connection with only IN and owning AudioControl claims.
 * USB permission remains Android's decision. No output-volume setup or playback stream is opened.
 * Keep this session open until recording ends; close off the main thread.
 */
public class DacCaptureSession internal constructor(
    private val dac: Long,
    public val info: DacDeviceInfo,
    private val connection: UsbDeviceConnection,
    private val claims: ClaimedInterfaces,
    private val onClosed: () -> Unit,
) : Closeable {
    private val closed = AtomicBoolean(false)
    private val mutableState = MutableStateFlow(DacState.READY)
    private val mutableEvents = MutableSharedFlow<DacEvent>(extraBufferCapacity = 64, onBufferOverflow = BufferOverflow.DROP_OLDEST)
    private var stream: DacCaptureStream? = null
    public val state: StateFlow<DacState> get() = mutableState
    public val events: SharedFlow<DacEvent> get() = mutableEvents

    private val listener = object : NativeListener {
        override fun onEvent(type: Int, a: Long, b: Double, message: String?) {
            val event = decodeEvent(type, a, b, message)
            if (event == DacEvent.DeviceLost) mutableState.update { if (it == DacState.CLOSED) it else DacState.LOST }
            mutableEvents.tryEmit(event)
        }
    }

    init { nativeCall { Native.dacSetListener(dac, listener) } }

    /** Negotiate and start exact PCM capture. No rate substitution, conversion or duplex. */
    @Synchronized
    public fun openCapture(request: CaptureRequest, capacityFrames: Int = 8192): DacCaptureStream {
        if (closed.get()) throw DacException(DacErrorCode.INVALID_STATE, "capture session is closed")
        if (mutableState.value == DacState.LOST) throw DacException(DacErrorCode.DEVICE_LOST, "capture device is gone")
        if (stream != null) throw DacException(DacErrorCode.INVALID_STATE, "a capture stream is already open")
        val handle = nativeCall { Native.captureOpen(dac, request.toCaptureJson(), capacityFrames) }
        val format = try {
            nativeCall { parseCaptureFormat(Native.captureFormat(handle)) }
        } catch (error: Throwable) {
            runCatching { Native.captureClose(handle) }
            throw error
        }
        val opened = DacCaptureStream(handle, format) { streamClosed() }
        stream = opened
        mutableState.update { if (it == DacState.READY) DacState.STREAMING else it }
        return opened
    }

    @Synchronized
    private fun streamClosed() {
        stream = null
        mutableState.update { if (it == DacState.STREAMING) DacState.READY else it }
    }

    /**
     * Stop capture and native USB, then release Android claims only after actual retirement.
     * A failed cancellation keeps the host connection owned by a retirement thread.
     */
    override fun close() {
        if (!closed.compareAndSet(false, true)) return
        var failure: Throwable? = null
        try {
            runCatching { synchronized(this) { stream }?.close() }
                .onFailure { logWarning("closing capture stream failed", it) }
            try { nativeCall { Native.dacClose(dac) } } catch (error: Throwable) { failure = error }
        } finally {
            fun release() {
                claims.release()
                runCatching { connection.close() }
                onClosed()
            }
            if (runCatching { Native.dacAwaitRetirement() }.getOrDefault(false)) {
                release()
            } else {
                Thread({
                    while (!runCatching { Native.dacAwaitRetirement() }.getOrDefault(false)) {
                        runCatching { Thread.sleep(50) }
                    }
                    release()
                }, "usbdac-capture-retirement").start()
            }
            mutableState.value = DacState.CLOSED
        }
        failure?.let { throw it }
    }
}
