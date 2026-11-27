package dev.usbdac

import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Assert.assertSame
import org.junit.Assert.assertTrue
import org.junit.Assert.fail
import org.junit.Test

class DacExceptionTest {
    @Test
    fun routingPowerFailureKeepsTransactionAndRollbackDetails() {
        val response = """{"error":{"kind":"transaction","detail":"write failed","failed_change":2,
            "cause":{"kind":"io","detail":"write"},"rollback_failures":[
                {"key":{"kind":"selector","interface":3,"entity":8},"cause":{"kind":"io","detail":"route restore"}}],
            "power_restore_failure":"power restore"}}"""
        try {
            routingResponse { response }
            fail("expected routing failure")
        } catch (error: RoutingException) {
            assertEquals(RoutingErrorKind.TRANSACTION, error.kind)
            assertEquals(2, error.failedChange)
            assertEquals("write", error.controlFailure!!.detail)
            assertEquals(RouteKey.Selector(3, 8), error.rollbackFailures.single().key)
            assertEquals("route restore", error.rollbackFailures.single().cause.detail)
            assertEquals("power restore", error.powerRestoreFailure)
        }
    }

    @Test
    fun routingErrorsWithoutPowerFailureKeepExistingConstructorAndDecodeBehavior() {
        val legacy = RoutingException(RoutingErrorKind.INVALID, "invalid", null, null, null, emptyList())
        assertNull(legacy.powerRestoreFailure)
        try {
            routingResponse { """{"error":{"kind":"session","detail":"power restore","session_code":"control","power_restore_failure":"power restore"}}""" }
            fail("expected routing failure")
        } catch (error: RoutingException) {
            assertEquals(RoutingErrorKind.SESSION, error.kind)
            assertEquals(DacErrorCode.CONTROL, error.sessionCode)
            assertEquals("power restore", error.powerRestoreFailure)
            assertTrue(error.rollbackFailures.isEmpty())
        }
        try {
            routingResponse { """{"error":{"kind":"invalid","detail":"invalid"}}""" }
            fail("expected routing failure")
        } catch (error: RoutingException) {
            assertNull(error.powerRestoreFailure)
        }
    }

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
