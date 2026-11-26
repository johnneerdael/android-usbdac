package dev.usbdac

import java.io.Closeable
import java.util.concurrent.atomic.AtomicBoolean

/** One playback/capture worker. Closing this owner or either direction retires both endpoints. */
public class DacDuplexStream internal constructor(
    public val playback: DacStream,
    public val capture: DacCaptureStream,
    private val onClosed: () -> Unit,
) : Closeable {
    private val closed = AtomicBoolean(false)

    override fun close() {
        if (!closed.compareAndSet(false, true)) return
        var failure: Throwable? = null
        try {
            try { playback.close() } catch (error: Throwable) { failure = error }
            try { capture.close() } catch (error: Throwable) {
                failure?.addSuppressed(error) ?: run { failure = error }
            }
        } finally {
            onClosed()
        }
        failure?.let { throw it }
    }
}
