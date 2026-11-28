package dev.usbdac

import dev.usbdac.internal.ConvertNative
import dev.usbdac.internal.DsdPcmNative
import org.junit.Assert.assertEquals
import org.junit.Assert.assertThrows
import org.junit.Assume.assumeTrue
import org.junit.BeforeClass
import org.junit.Test
import java.nio.ByteBuffer

/**
 * The native converters must refuse a read-only output buffer themselves: JNI hands them the
 * memory of a direct buffer whether or not Java allows writes to it. The Kotlin wrappers check
 * first, so these tests call the JNI entry points directly. Each call is repeated so the cached
 * `ByteBuffer.isReadOnly` method lookup runs more than once.
 */
class HostJniBufferTest {
    companion object {
        private const val READ_ONLY = "usbdac:invalid_argument: the output buffer is read-only"

        @BeforeClass @JvmStatic fun loadHostJni() {
            val path = System.getenv("USBDAC_TEST_HOST_JNI")
            assumeTrue("named host-JNI gate supplies USBDAC_TEST_HOST_JNI", !path.isNullOrBlank())
            assumeTrue("host conversion gate requires library loading", System.getenv("USBDAC_TEST_DSD_PCM") == "1")
            System.load(checkNotNull(path))
        }
    }

    private fun assertReadOnlyRefused(call: () -> Unit) {
        val error = assertThrows(IllegalStateException::class.java) { call() }
        assertEquals(READ_ONLY, error.message)
    }

    @Test fun pcmConverterRefusesReadOnlyOutputEveryTime() {
        val target = StreamFormat(
            formatId = 1, encoding = StreamEncoding.PCM, dsdBitRate = null,
            sampleRate = 48_000, channels = 2, validBits = 24, subslotBytes = 4,
            bytesPerFrame = 8, counterRateHz = 48_000, interfaceNumber = 1, altSetting = 1,
            endpointAddress = 1, syncType = SyncType.SYNCHRONOUS, explicitFeedback = false,
            linkSpeed = LinkSpeed.HIGH, serviceIntervalUs = 125, rateDiffersFromRequest = false,
            pacing = Pacing.Synchronous,
        )
        val plan = ConversionPlan.between(StreamRequest.pcm(48_000, 2, 16), target, ConversionOptions(headroomDb = 0.0))
        val handle = ConvertNative.convertCreate(plan.requestJson)
        try {
            val input = ByteBuffer.allocateDirect(4)
            val readOnly = ByteBuffer.allocateDirect(64).asReadOnlyBuffer()
            repeat(3) {
                assertReadOnlyRefused { ConvertNative.convertProcess(handle, input, 0, 4, readOnly, 0, 64) }
                assertReadOnlyRefused { ConvertNative.convertFinish(handle, readOnly, 0, 64) }
            }
            // A read-only input is fine; only the output is written. The refusals changed nothing.
            val result = ConvertNative.convertProcess(handle, input.asReadOnlyBuffer(), 0, 4, ByteBuffer.allocateDirect(64), 0, 64)
            assertEquals(4L, result ushr 32)
            assertEquals(8L, result and 0xffff_ffffL)
        } finally { ConvertNative.convertClose(handle) }
    }

    @Test fun dsdConverterRefusesReadOnlyOutputEveryTime() {
        val handle = DsdPcmNative.create(2_822_400, 2, 176_400, 2, 16, 2, 1, 1, 0.0, -1)
        try {
            val input = ByteBuffer.allocateDirect(256).apply { repeat(capacity()) { put(0x69.toByte()) }; flip() }
            val readOnly = ByteBuffer.allocateDirect(4096).asReadOnlyBuffer()
            repeat(3) {
                assertReadOnlyRefused { DsdPcmNative.process(handle, input, 0, 256, readOnly, 0, 4096) }
                assertReadOnlyRefused { DsdPcmNative.finish(handle, readOnly, 0, 4096) }
            }
            val result = DsdPcmNative.process(handle, input.asReadOnlyBuffer(), 0, 256, ByteBuffer.allocateDirect(4096), 0, 4096)
            assertEquals(256L, result ushr 32)
        } finally { DsdPcmNative.close(handle) }
    }
}
