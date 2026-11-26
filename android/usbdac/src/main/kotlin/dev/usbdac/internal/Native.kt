package dev.usbdac.internal

import java.nio.ByteBuffer

/**
 * Receives native events. The library calls [onEvent] by name through JNI, on its own
 * `usbdac-events` thread, so this interface and its method must keep their names (see
 * `consumer-rules.pro`).
 */
internal interface NativeListener {
    fun onEvent(type: Int, a: Long, b: Double, message: String?)
}

/**
 * The JNI surface of `libusbdac.so`. Each method has the exported symbol
 * `Java_dev_usbdac_internal_Native_<name>`; the contract is in `docs/API.md` (section 10).
 *
 * Handles are opaque ids. A failure is an [IllegalStateException] whose message starts
 * with `usbdac:<code>:`; `nativeCall` turns it into a `DacException`.
 */
internal object Native {
    init {
        System.loadLibrary("usbdac")
    }

    @JvmStatic external fun version(): String
    @JvmStatic external fun setLogLevel(level: Int)

    @JvmStatic external fun dacFeedbackPreflight(fd: Int, deviceInfoJson: String): String
    @JvmStatic external fun dacCapturePreflight(fd: Int, deviceInfoJson: String): String
    @JvmStatic external fun dacOpenCaptureDevice(fd: Int, deviceInfoJson: String, configJson: String?): Long
    @JvmStatic external fun dacOpenDuplexDevice(fd: Int, deviceInfoJson: String, configJson: String?): Long
    @JvmStatic external fun duplexOpen(dac: Long, formatId: Long, requestJson: String, capacityFrames: Int, listener: Any): String
    @JvmStatic external fun captureOpen(dac: Long, requestJson: String, capacityFrames: Int): Long
    @JvmStatic external fun captureFormat(capture: Long): String
    @JvmStatic external fun captureStats(capture: Long): String
    @JvmStatic external fun captureRead(capture: Long, buffer: ByteBuffer, offset: Int, count: Int): Int
    @JvmStatic external fun captureClose(capture: Long)
    @JvmStatic external fun dacOpen(fd: Int, deviceInfoJson: String, configJson: String?): Long
    @JvmStatic external fun dacClose(dac: Long)
    @JvmStatic external fun dacAwaitRetirement(): Boolean
    @JvmStatic external fun scarlettPreflight(fd: Int, deviceInfoJson: String): String
    @JvmStatic external fun scarlettCapability(dac: Long): String
    @JvmStatic external fun scarlettRead(dac: Long): String
    @JvmStatic external fun scarlettMuxRead(dac: Long): String
    @JvmStatic external fun scarlettMuxApply(dac: Long, snapshot: Long, editsJson: String): String
    @JvmStatic external fun scarlettMixApply(dac: Long, editJson: String): String
    @JvmStatic external fun routingDiscoverPowered(dac: Long): String
    @JvmStatic external fun routingReadPowered(dac: Long, snapshot: Long, keyJson: String): String
    @JvmStatic external fun routingApplyPowered(dac: Long, snapshot: Long, editsJson: String): String
    @JvmStatic external fun routingDiscover(dac: Long): String
    @JvmStatic external fun routingRead(dac: Long, snapshot: Long, keyJson: String): String
    @JvmStatic external fun routingApply(dac: Long, snapshot: Long, editsJson: String): String
    @JvmStatic external fun dacCapabilities(dac: Long): String
    @JvmStatic external fun dacNegotiate(dac: Long, requestJson: String): String
    @JvmStatic external fun dacSetListener(dac: Long, listener: Any?)

    @JvmStatic external fun streamOpen(dac: Long, formatId: Long, listener: Any): Long
    @JvmStatic external fun streamClose(stream: Long)
    @JvmStatic external fun streamWrite(stream: Long, buffer: ByteBuffer, offset: Int, count: Int): Int
    @JvmStatic external fun streamWritableBytes(stream: Long): Int
    @JvmStatic external fun streamPlay(stream: Long)
    @JvmStatic external fun streamPause(stream: Long)
    @JvmStatic external fun streamFlush(stream: Long)
    @JvmStatic external fun streamDrain(stream: Long)
    @JvmStatic external fun streamIsDrained(stream: Long): Boolean
    @JvmStatic external fun streamState(stream: Long): Int
    @JvmStatic external fun streamPlayedFrames(stream: Long): Long
    @JvmStatic external fun streamLatencyFrames(stream: Long): Long
    @JvmStatic external fun streamStats(stream: Long): String
    @JvmStatic external fun streamIsBitPerfect(stream: Long): Boolean
    @JvmStatic external fun streamSetSoftwareGain(stream: Long, gain: Float)

    @JvmStatic external fun volumeRange(dac: Long): String?
    @JvmStatic external fun volumeGet(dac: Long): String
    @JvmStatic external fun volumeSetDb(dac: Long, db: Double): String
    @JvmStatic external fun volumeSetFraction(dac: Long, fraction: Double): String
    @JvmStatic external fun volumeStep(dac: Long, direction: Int, steps: Int): String
    @JvmStatic external fun volumeSetMute(dac: Long, muted: Boolean): String
    @JvmStatic external fun volumeOriginalDb(dac: Long): Double
    @JvmStatic external fun volumeHandBack(dac: Long): Boolean
    @JvmStatic external fun volumeStartup(dac: Long): String?
}
