package dev.usbdac

import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Assert.assertSame
import org.junit.Assert.assertTrue
import org.junit.Assert.fail
import org.junit.Test

class DacExceptionTest {
    @Test
    fun routingHandBackDoesNotInventSuccessWithoutRetirementToken() {
        val routing = DacRouting(1L) { }
        assertEquals(RoutingHandBackState.NotClosed, routing.handBack)
        routing.beginHandBack(false)
        assertTrue(routing.handBack is RoutingHandBackState.Unavailable)
        routing.publishHandBack(0L)
        assertTrue(routing.handBack is RoutingHandBackState.Unavailable)
        routing.beginHandBack(true)
        assertEquals(RoutingHandBackState.Pending, routing.handBack)
    }

    @Test
    fun routingHandBackParsesUncertaintyAndPreservesUnknownOutcomes() {
        val result = parseRoutingHandBack("""{"status":"complete","controls":[{"key":{"kind":"selector","interface":3,"entity":7},"outcome":"restore_uncertain","detail":"rounded"},{"key":{"kind":"selector","interface":3,"entity":8},"outcome":"future_outcome"}]}""")
        val report = (result as RoutingHandBackState.Complete).report
        assertEquals(RoutingHandBackOutcome.RESTORE_UNCERTAIN, report.controls[0].outcome)
        assertEquals("rounded", report.controls[0].detail)
        assertEquals(RoutingHandBackOutcome.UNKNOWN, report.controls[1].outcome)
        assertEquals("future_outcome", report.controls[1].rawOutcome)
        assertEquals(RoutingHandBackState.NoRecordedChanges, parseRoutingHandBack("""{"status":"no_recorded_changes"}"""))
    }

    @Test
    fun routingDiscoveryDiagnosticsAllowLegacyAndPartialEntries() {
        assertEquals(emptyList<RoutingDiagnostic>(), parseRoutingDiagnostics(org.json.JSONObject("{}")))
        val result = parseRoutingDiagnostics(org.json.JSONObject("""{"diagnostics":[{"interface":3,"entity":7,"detail":"unreadable"},{"interface":4,"entity":null,"detail":"ambiguous"},{"detail":"bad"}]}"""))
        assertEquals(listOf(RoutingDiagnostic(3, 7, "unreadable"), RoutingDiagnostic(4, null, "ambiguous")), result)
    }

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
    fun committedRoutesSurviveAPowerHandBackFailure() {
        // usbdac-jni detailed_apply_failure adds route_report when the transaction committed and only
        // the ADC3 power hand-back failed: the app must see which controls now hold the new value.
        val response = """{"error":{"kind":"session","detail":"power restore","session_code":"control",
            "power_restore_failure":"power restore","route_report":{
                "changed":[{"kind":"selector","interface":3,"entity":8}],
                "unchanged":[{"kind":"crosspoint","interface":3,"entity":9,"input_pin":1,"input_channel":2,"output_channel":1}]}}}"""
        try {
            routingResponse { response }
            fail("expected routing failure")
        } catch (error: RoutingException) {
            assertEquals("power restore", error.powerRestoreFailure)
            assertEquals(RoutingReport(listOf(RouteKey.Selector(3, 8)), listOf(RouteKey.Crosspoint(3, 9, 1, 2, 1))), error.routeReport)
        }
        try {
            routingResponse { """{"error":{"kind":"session","detail":"power restore","session_code":"control","power_restore_failure":"power restore"}}""" }
            fail("expected routing failure")
        } catch (error: RoutingException) {
            assertNull(error.routeReport)
        }
        assertNull(RoutingException(RoutingErrorKind.INVALID, "invalid", null, null, null, emptyList()).routeReport)
        val report = RoutingReport(listOf(RouteKey.Selector(3, 8)), emptyList())
        assertEquals(report, RoutingException(RoutingErrorKind.SESSION, "power restore", DacErrorCode.CONTROL, null, null,
            emptyList(), "power restore", report).routeReport)
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
            "host_usb" to DacErrorCode.HOST_USB,
            "reconfigure_drain_timeout" to DacErrorCode.RECONFIGURE_DRAIN_TIMEOUT,
            "reconfigure_unusable" to DacErrorCode.RECONFIGURE_UNUSABLE,
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
