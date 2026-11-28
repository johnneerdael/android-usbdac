package dev.usbdac

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

class EventDecodeTest {
    @Test
    fun stateChangesDecodeByCode() {
        assertEquals(DacEvent.StateChanged(StreamState.PAUSED), decodeEvent(0, 2, 0.0, null))
        assertEquals(DacEvent.StateChanged(StreamState.FAILED), decodeEvent(0, 6, 0.0, null))
        assertTrue(decodeEvent(0, 99, 0.0, null) is DacEvent.Unknown)
    }

    @Test
    fun simpleEventsDecode() {
        assertEquals(DacEvent.Underrun, decodeEvent(1, 0, 0.0, null))
        assertEquals(DacEvent.LostFrames(44), decodeEvent(2, 44, 0.0, null))
        assertEquals(DacEvent.BitPerfectChanged(true), decodeEvent(3, 1, 0.0, null))
        assertEquals(DacEvent.BitPerfectChanged(false), decodeEvent(3, 0, 0.0, null))
        assertEquals(DacEvent.FeedbackStale, decodeEvent(5, 0, 0.0, null))
        assertEquals(DacEvent.NominalAsyncHostStarvation(5001), decodeEvent(9, 5001, 0.0, null))
        assertEquals(DacEvent.DeviceLost, decodeEvent(7, 0, 0.0, null))
    }

    @Test
    fun aVolumeEventReadsItsJsonMessage() {
        val event = decodeEvent(4, 1, -12.5, """{"db":-12.5,"fraction":0.6,"muted":true}""")
        assertEquals(DacEvent.VolumeChanged(VolumeState(-12.5, 0.6, true)), event)
    }

    @Test
    fun aVolumeEventWithoutAMessageFallsBackToTheNumericFields() {
        val unknownMute = decodeEvent(4, -1, -30.0, null) as DacEvent.VolumeChanged
        assertEquals(-30.0, unknownMute.state.db, 0.0)
        assertEquals(null, unknownMute.state.muted)
        val muted = decodeEvent(4, 1, -30.0, "not json") as DacEvent.VolumeChanged
        assertEquals(true, muted.state.muted)
    }

    @Test
    fun anErrorEventCarriesTheTypedException() {
        val event = decodeEvent(6, 7, 0.0, "usbdac:transfer: boom") as DacEvent.StreamFailed
        assertEquals(DacErrorCode.TRANSFER, event.error.code)
        assertEquals("boom", event.error.detail)
    }

    @Test
    fun anErrorEventWithAnUnparsableMessageStillDecodes() {
        val event = decodeEvent(6, 0, 0.0, "not ours") as DacEvent.StreamFailed
        assertEquals(DacErrorCode.UNKNOWN, event.error.code)
        assertEquals("not ours", event.error.detail)
    }

    @Test
    fun anErrorEventWithoutAMessageUsesTheStableNumericCode() {
        // usbdac-jni json.rs error_code: 1 busy .. 12 volume_unavailable, 13 host_usb,
        // 14 reconfigure_drain_timeout, 15 reconfigure_unusable; 0 is a code it does not number.
        val expected = listOf(
            DacErrorCode.BUSY, DacErrorCode.PLATFORM_UNSUPPORTED, DacErrorCode.CLAIM, DacErrorCode.UNSUPPORTED,
            DacErrorCode.CLOCK_UNVERIFIED, DacErrorCode.DEVICE_LOST, DacErrorCode.TRANSFER, DacErrorCode.CONTROL,
            DacErrorCode.INVALID_STATE, DacErrorCode.INVALID_ARGUMENT, DacErrorCode.IMPLICIT_FEEDBACK_UNSUPPORTED,
            DacErrorCode.VOLUME_UNAVAILABLE, DacErrorCode.HOST_USB, DacErrorCode.RECONFIGURE_DRAIN_TIMEOUT,
            DacErrorCode.RECONFIGURE_UNUSABLE,
        )
        expected.forEachIndexed { index, code ->
            val event = decodeEvent(6, index + 1L, 0.0, null) as DacEvent.StreamFailed
            assertEquals("code ${index + 1}", code, event.error.code)
        }
        assertEquals(DacErrorCode.UNKNOWN, (decodeEvent(6, 0, 0.0, null) as DacEvent.StreamFailed).error.code)
        assertEquals(DacErrorCode.UNKNOWN, (decodeEvent(6, 16, 0.0, null) as DacEvent.StreamFailed).error.code)
        val parsed = decodeEvent(6, 15, 0.0, "usbdac:reconfigure_unusable: close the DAC") as DacEvent.StreamFailed
        assertEquals("close the DAC", parsed.error.detail)
    }

    @Test
    fun anUnknownTypeIsKeptAsUnknown() {
        val event = decodeEvent(-1, 0, 0.0, "SomethingNew") as DacEvent.Unknown
        assertEquals("SomethingNew", event.description)
    }

    @Test
    fun theDirectStateFollowsTheFlagAndNamesTheBlocker() {
        assertEquals(BitPerfectState.Verified, directBitPerfectState(true, null))
        val state = directBitPerfectState(false, BitPerfectBlocker.SOFTWARE_GAIN) as BitPerfectState.NotVerified
        assertEquals(BitPerfectBlocker.SOFTWARE_GAIN, state.blocker)
        assertEquals(BitPerfectBlocker.SOFTWARE_GAIN.description, state.reason)
        val unknown = directBitPerfectState(false, null) as BitPerfectState.NotVerified
        assertEquals(BitPerfectBlocker.UNKNOWN, unknown.blocker)
    }

    @Test
    fun onlyTheLibrarysOwnVerdictIsVerified() {
        assertTrue(BitPerfectState.Verified.isVerified)
        assertFalse(BitPerfectState.NotVerified(null, "x").isVerified)
        assertFalse(BitPerfectState.RequestedViaPlatform("x").isVerified)
        assertFalse(BitPerfectState.Unverifiable("x").isVerified)
    }

    @Test
    fun everyBlockerNameRoundTrips() {
        for (blocker in BitPerfectBlocker.entries.filter { it != BitPerfectBlocker.UNKNOWN }) {
            assertEquals(blocker, BitPerfectBlocker.fromWireName(blocker.wireName))
        }
        assertEquals(null, BitPerfectBlocker.fromWireName(null))
    }
    @Test
    fun compatibilityAdmissionDecodesItsTierAndReasonWithoutInventingVerification() {
        assertEquals(DacEvent.CompatibilityFallback(CompatibilityFallbackTier.NOMINAL_ASYNC, "full-speed UAC1 nominal pacing"),
            decodeEvent(8, 0, 0.0, "full-speed UAC1 nominal pacing"))
        assertEquals(DacEvent.CompatibilityFallback(CompatibilityFallbackTier.UNVERIFIED_CLOCK, "SET succeeded; no usable readback"),
            decodeEvent(8, 1, 0.0, "SET succeeded; no usable readback"))
        assertEquals("nominal_async", CompatibilityFallbackTier.NOMINAL_ASYNC.wireName)
        assertEquals("unverified_clock", CompatibilityFallbackTier.UNVERIFIED_CLOCK.wireName)
        assertTrue(decodeEvent(8, 2, 0.0, "future tier") is DacEvent.Unknown)
        assertTrue(decodeEvent(8, 4_294_967_296, 0.0, "overflow must not become tier zero") is DacEvent.Unknown)
        assertEquals(DacEvent.CompatibilityFallback(CompatibilityFallbackTier.NOMINAL_ASYNC, "compatibility fallback nominal_async"),
            decodeEvent(8, 0, 0.0, null))
    }

    @Test
    fun deviceLevelCompatibilityEventDoesNotDuplicateSessionDeliveryOrChangeTheBlocker() {
        val published = mutableListOf<DacEvent>()
        val signals = StreamSignals { published += it }
        for ((tier, blocker) in listOf(0L to BitPerfectBlocker.UNSYNCHRONIZED_ASYNC,
                1L to BitPerfectBlocker.CLOCK_UNVERIFIED)) {
            val existing = directBitPerfectState(false, blocker)
            signals.bitPerfect.value = existing
            signals.onEvent(8, tier, 0.0, "admitted compatibility path")
            assertEquals(existing, signals.bitPerfect.value)
            assertEquals(StreamState.BUFFERING, signals.state.value)
        }
        assertTrue("device listener already publishes this event to session.events", published.isEmpty())
    }

}
