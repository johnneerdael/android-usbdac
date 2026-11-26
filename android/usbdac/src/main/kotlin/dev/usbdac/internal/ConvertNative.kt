package dev.usbdac.internal

import java.nio.ByteBuffer

/**
 * The JNI surface of the conversion helpers in `libusbdac.so`. Each method has the exported symbol
 * `Java_dev_usbdac_internal_ConvertNative_<name>`; the contract is in `docs/API.md` (section 14).
 *
 * Handles are opaque ids. A failure is an [IllegalStateException] whose message starts with
 * `usbdac:<code>:`; `nativeCall` turns it into a `DacException`. A separate object from [Native],
 * so the USB path and the conversion helpers stay independent.
 */
internal object ConvertNative {
    init {
        System.loadLibrary("usbdac")
    }

    @JvmStatic external fun convertPlan(requestJson: String): String
    @JvmStatic external fun convertCreate(requestJson: String): Long
    @JvmStatic external fun convertClose(handle: Long)
    @JvmStatic external fun convertProcess(
        handle: Long,
        input: ByteBuffer,
        inOffset: Int,
        inCount: Int,
        output: ByteBuffer,
        outOffset: Int,
        outCapacity: Int,
    ): Long
    @JvmStatic external fun convertFinish(handle: Long, output: ByteBuffer, outOffset: Int, outCapacity: Int): Int
    @JvmStatic external fun convertReset(handle: Long)
    @JvmStatic external fun convertClipped(handle: Long): Long
}

/**
 * What the conversion classes need from the native layer. The real implementation forwards to
 * [ConvertNative]; the unit tests supply a fake, because the library cannot be loaded on the JVM.
 */
internal interface ConvertBackend {
    fun plan(requestJson: String): String
    fun create(requestJson: String): Long
    fun close(handle: Long)

    /** Returns `(consumed shl 32) or produced`, both in bytes. */
    fun process(handle: Long, input: ByteBuffer, inOffset: Int, inCount: Int, output: ByteBuffer, outOffset: Int, outCapacity: Int): Long
    fun finish(handle: Long, output: ByteBuffer, outOffset: Int, outCapacity: Int): Int
    fun reset(handle: Long)
    fun clipped(handle: Long): Long
}

/** The backend over `libusbdac.so`. */
internal object NativeConvertBackend : ConvertBackend {
    override fun plan(requestJson: String): String = ConvertNative.convertPlan(requestJson)
    override fun create(requestJson: String): Long = ConvertNative.convertCreate(requestJson)
    override fun close(handle: Long) = ConvertNative.convertClose(handle)
    override fun process(handle: Long, input: ByteBuffer, inOffset: Int, inCount: Int, output: ByteBuffer, outOffset: Int, outCapacity: Int): Long =
        ConvertNative.convertProcess(handle, input, inOffset, inCount, output, outOffset, outCapacity)

    override fun finish(handle: Long, output: ByteBuffer, outOffset: Int, outCapacity: Int): Int =
        ConvertNative.convertFinish(handle, output, outOffset, outCapacity)

    override fun reset(handle: Long) = ConvertNative.convertReset(handle)
    override fun clipped(handle: Long): Long = ConvertNative.convertClipped(handle)
}
