package dev.usbdac

import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Assert.assertSame
import org.junit.Assert.assertTrue
import org.junit.Assert.fail
import org.junit.Test

class DacExceptionTest {
    @Test
    fun everyDocumentedNativeCodeParses() {
        val expected = mapOf(
            "busy" to DacErrorCode.BUSY,
            "platform_unsupported" to DacErrorCode.PLATFORM_UNSUPPORTED,
            "claim" to DacErrorCode.CLAIM,
            "unsupported" to DacErrorCode.UNSUPPORTED,
            "implicit_feedback_unsupported" to DacErrorCode.IMPLICIT_FEEDBACK_UNSUPPORTED,
            "clock_unverified" to DacErrorCode.CLOCK_UNVERIFIED,
            "device_lost" to DacErrorCode.DEVICE_LOST,
            "transfer" to DacErrorCode.TRANSFER,
            "control" to DacErrorCode.CONTROL,
            "invalid_state" to DacErrorCode.INVALID_STATE,
            "invalid_argument" to DacErrorCode.INVALID_ARGUMENT,
            "volume_unavailable" to DacErrorCode.VOLUME_UNAVAILABLE,
        )
        for ((wire, code) in expected) {
            val error = parseNativeError("usbdac:$wire: something went wrong")!!
            assertEquals(wire, code, error.code)
            assertEquals("something went wrong", error.detail)
        }
    }

    @Test
    fun theMessageKeepsTheCodePrefixOfTheNativeDisplayForm() {
        val error = parseNativeError("usbdac:clock_unverified: DAC clock not verified at 44100 Hz (no readback)")!!
        assertEquals("clock_unverified: DAC clock not verified at 44100 Hz (no readback)", error.message)
    }

    @Test
    fun aColonInsideTheMessageIsKept() {
        val error = parseNativeError("usbdac:claim: interface 2: not claimed")!!
        assertEquals(DacErrorCode.CLAIM, error.code)
        assertEquals("interface 2: not claimed", error.detail)
    }

    @Test
    fun anUnknownCodeIsUnknownNotAFailure() {
        val error = parseNativeError("usbdac:from_the_future: hello")!!
        assertEquals(DacErrorCode.UNKNOWN, error.code)
        assertEquals("hello", error.detail)
    }

    @Test
    fun aMessageWithoutTheLibraryPrefixIsNotOurs() {
        assertNull(parseNativeError("something else"))
        assertNull(parseNativeError(null))
    }

    @Test
    fun aPrefixWithoutACodeSeparatorIsUnknown() {
        assertEquals(DacErrorCode.UNKNOWN, parseNativeError("usbdac:oops")!!.code)
    }

    @Test
    fun nativeCallConvertsOurFailuresAndPassesOthersThrough() {
        try {
            nativeCall<Unit> { throw IllegalStateException("usbdac:busy: another DAC session is open") }
            fail("expected a DacException")
        } catch (error: DacException) {
            assertEquals(DacErrorCode.BUSY, error.code)
        }
        val other = IllegalStateException("not ours")
        try {
            nativeCall<Unit> { throw other }
            fail("expected the original exception")
        } catch (error: IllegalStateException) {
            assertSame(other, error)
        }
        assertEquals(5, nativeCall { 5 })
    }

    @Test
    fun kotlinSideCodesHaveNoNativeName() {
        assertTrue(DacException(DacErrorCode.PERMISSION_DENIED, "no permission").message!!.startsWith("permission_denied:"))
    }
}
