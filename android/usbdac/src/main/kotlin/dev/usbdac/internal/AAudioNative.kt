package dev.usbdac.internal

import java.nio.ByteBuffer

/**
 * The JNI surface of the AAudio output in `libusbdac.so`. Each method has the exported symbol
 * `Java_dev_usbdac_internal_AAudioNative_<name>`; the contract is in `docs/API.md` (section 13).
 *
 * Handles are opaque ids. A failure is an [IllegalStateException] whose message starts with
 * `usbdac:<code>:`; `nativeCall` turns it into a `DacException`. This is a separate object from
 * [Native] so the USB path and the AAudio path stay independent.
 */
internal object AAudioNative {
    init {
        System.loadLibrary("usbdac")
    }

    @JvmStatic external fun aaudioOpen(requestJson: String, listener: Any, apiLevel: Int): Long
    @JvmStatic external fun aaudioGrant(handle: Long): String
    @JvmStatic external fun aaudioStart(handle: Long)
    @JvmStatic external fun aaudioInvalidateRoute(handle: Long)
    @JvmStatic external fun aaudioClose(handle: Long)
    @JvmStatic external fun aaudioWrite(handle: Long, buffer: ByteBuffer, offset: Int, count: Int): Int
    @JvmStatic external fun aaudioWritableBytes(handle: Long): Int
    @JvmStatic external fun aaudioPlay(handle: Long)
    @JvmStatic external fun aaudioPause(handle: Long)
    @JvmStatic external fun aaudioFlush(handle: Long)
    @JvmStatic external fun aaudioDrain(handle: Long)
    @JvmStatic external fun aaudioIsDrained(handle: Long): Boolean
    @JvmStatic external fun aaudioState(handle: Long): Int
    @JvmStatic external fun aaudioPlayedFrames(handle: Long): Long
    @JvmStatic external fun aaudioLatencyFrames(handle: Long): Long
    @JvmStatic external fun aaudioXruns(handle: Long): Int
    @JvmStatic external fun aaudioUnderruns(handle: Long): Long
}

/**
 * What [dev.usbdac.AAudioExclusiveOutput] needs from the native layer. The real implementation
 * forwards to [AAudioNative]; the unit tests supply a fake, because the library cannot be loaded
 * on the JVM.
 */
internal interface AAudioBackend {
    fun open(requestJson: String, listener: NativeListener, apiLevel: Int): Long
    fun grant(handle: Long): String
    fun start(handle: Long)
    fun invalidateRoute(handle: Long) { throw UnsupportedOperationException("route invalidation unavailable") }
    fun close(handle: Long)
    fun write(handle: Long, buffer: ByteBuffer, offset: Int, count: Int): Int
    fun writableBytes(handle: Long): Int
    fun play(handle: Long)
    fun pause(handle: Long)
    fun flush(handle: Long)
    fun drain(handle: Long)
    fun isDrained(handle: Long): Boolean
    fun state(handle: Long): Int
    fun playedFrames(handle: Long): Long
    fun latencyFrames(handle: Long): Long
    fun xruns(handle: Long): Int
    fun underruns(handle: Long): Long
}

/** The backend over `libusbdac.so`. */
internal object NativeAAudioBackend : AAudioBackend {
    override fun open(requestJson: String, listener: NativeListener, apiLevel: Int): Long =
        AAudioNative.aaudioOpen(requestJson, listener, apiLevel)

    override fun grant(handle: Long): String = AAudioNative.aaudioGrant(handle)
    override fun start(handle: Long) = AAudioNative.aaudioStart(handle)
    override fun invalidateRoute(handle: Long) = AAudioNative.aaudioInvalidateRoute(handle)
    override fun close(handle: Long) = AAudioNative.aaudioClose(handle)
    override fun write(handle: Long, buffer: ByteBuffer, offset: Int, count: Int): Int =
        AAudioNative.aaudioWrite(handle, buffer, offset, count)

    override fun writableBytes(handle: Long): Int = AAudioNative.aaudioWritableBytes(handle)
    override fun play(handle: Long) = AAudioNative.aaudioPlay(handle)
    override fun pause(handle: Long) = AAudioNative.aaudioPause(handle)
    override fun flush(handle: Long) = AAudioNative.aaudioFlush(handle)
    override fun drain(handle: Long) = AAudioNative.aaudioDrain(handle)
    override fun isDrained(handle: Long): Boolean = AAudioNative.aaudioIsDrained(handle)
    override fun state(handle: Long): Int = AAudioNative.aaudioState(handle)
    override fun playedFrames(handle: Long): Long = AAudioNative.aaudioPlayedFrames(handle)
    override fun latencyFrames(handle: Long): Long = AAudioNative.aaudioLatencyFrames(handle)
    override fun xruns(handle: Long): Int = AAudioNative.aaudioXruns(handle)
    override fun underruns(handle: Long): Long = AAudioNative.aaudioUnderruns(handle)
}
