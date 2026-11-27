package dev.usbdac.internal

import java.nio.ByteBuffer

internal object DsdPcmNative {
    init { System.loadLibrary("usbdac") }
    @JvmStatic external fun create(rate: Int, channels: Int, targetRate: Int, targetChannels: Int, bits: Int, slot: Int): Long
    @JvmStatic external fun process(handle: Long, input: ByteBuffer, offset: Int, count: Int, output: ByteBuffer, outOffset: Int, outCount: Int): Long
    @JvmStatic external fun finish(handle: Long, output: ByteBuffer, outOffset: Int, outCount: Int): Int
    @JvmStatic external fun delay(handle: Long): Int
    @JvmStatic external fun reset(handle: Long)
    @JvmStatic external fun close(handle: Long)
}
