package dev.usbdac

import org.junit.Assert.*
import org.junit.Test
import java.nio.ByteBuffer

class StreamReconfigurationTest {
    @Test
    fun locallyRetiredStreamNeverClosesItsStaleNativeTokenAgain() {
        var closedCallbacks = 0
        val signals = StreamSignals {}
        val stream = DacStream(0, dev.usbdac.internal.parseStreamFormat(Fixtures.FORMAT), signals) { closedCallbacks++ }
        // JNI replacement already removed the old registration. No native library is available here.
        stream.retiredByReconfigure()
        stream.retiredByReconfigure()
        stream.close()
        assertEquals(1, closedCallbacks)
        assertEquals(StreamState.CLOSED, stream.state.value)
        assertEquals(0, stream.writableBytes)
        val error = assertThrows(DacException::class.java) { stream.write(ByteBuffer.allocate(4)) }
        assertEquals(DacErrorCode.INVALID_STATE, error.code)
    }

    @Test
    fun reconfigurationErrorsRetainTheirMachineReadableCodes() {
        assertEquals(DacErrorCode.RECONFIGURE_DRAIN_TIMEOUT,
            parseNativeError("usbdac:reconfigure_drain_timeout: original still draining")!!.code)
        assertEquals(DacErrorCode.RECONFIGURE_UNUSABLE,
            parseNativeError("usbdac:reconfigure_unusable: close this owner")!!.code)
    }
    @Test
    fun pendingConfigurationCleanupIsExplicitAndWaitsForRetirement() {
        var ready = false
        var handBacks = 0
        val pending = PendingUsbConfigurationRestore({ ready }) { handBacks++ }
        assertFalse(pending.isReady)
        assertFalse(pending.finish())
        assertEquals(0, handBacks)
        ready = true
        assertTrue(pending.isReady)
        assertEquals(0, handBacks)
        assertTrue(pending.finish())
        assertTrue(pending.finish())
        assertEquals(1, handBacks)
    }

    @Test
    fun failedConfigurationHandBackIsReportedOnceWithoutRetry() {
        var handBacks = 0
        val cause = DacException(DacErrorCode.CONFIGURATION_RESTORE_FAILED, "external change")
        val pending = PendingUsbConfigurationRestore({ true }) { handBacks++; throw cause }
        assertSame(cause, assertThrows(DacException::class.java) { pending.finish() })
        assertSame(cause, assertThrows(DacException::class.java) { pending.finish() })
        assertEquals(1, handBacks)
    }

}
