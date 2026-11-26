package dev.usbdac.internal

import dev.usbdac.DacErrorCode
import dev.usbdac.DacException
import org.junit.Assert.*
import org.junit.Test

class ConfigurationTransactionTest {
    private class Io(var active: Int = 1) : ConfigurationIo {
        val writes = mutableListOf<Int>()
        var reads = 0
        var ignoreSelection = false
        var failAfterSelection = false
        var rejectRestore = false
        override fun current(): Int { reads++; return active }
        override fun select(id: Int): Boolean {
            writes += id
            if (rejectRestore && id == 1) return false
            if (!ignoreSelection) active = id
            if (failAfterSelection && id == 2) throw IllegalStateException("applied then failed")
            return true
        }
    }
    @Test fun requestedConfigurationIsSelectedVerifiedAndRestoredOnlyOnce() {
        val io = Io()
        val selection = ConfigurationTransaction.select(io, 2, listOf(1, 2))
        assertEquals(2, io.active)
        assertEquals(2, io.reads)
        selection.restore()
        selection.restore()
        assertEquals(1, io.active)
        assertEquals(listOf(2, 1), io.writes)
        assertEquals(4, io.reads)
    }
    @Test fun activeChoiceAndUnknownChoiceNeverSelectAnotherConfiguration() {
        val io = Io()
        ConfigurationTransaction.select(io, 1, listOf(1, 2)).restore()
        assertTrue(io.writes.isEmpty())
        val error = runCatching { ConfigurationTransaction.select(io, 3, listOf(1, 2)) }.exceptionOrNull() as DacException
        assertEquals(DacErrorCode.CONFIGURATION_UNAVAILABLE, error.code)
        assertTrue(io.writes.isEmpty())
    }
    @Test fun failedReadbackAndAppliedThenFailedSelectionRestoreOriginal() {
        for (afterWrite in listOf(false, true)) {
            val io = Io().apply { ignoreSelection = !afterWrite; failAfterSelection = afterWrite }
            val error = runCatching { ConfigurationTransaction.select(io, 2, listOf(1, 2)) }.exceptionOrNull() as DacException
            assertEquals(DacErrorCode.CONFIGURATION_SELECTION_FAILED, error.code)
            assertEquals(listOf(2, 1), io.writes)
            assertEquals(1, io.active)
            assertTrue(error.suppressed.isEmpty())
        }
    }
    @Test fun failedRollbackIsTypedAndPreservedOnSelectionFailure() {
        val io = Io().apply { failAfterSelection = true; rejectRestore = true }
        val error = runCatching { ConfigurationTransaction.select(io, 2, listOf(1, 2)) }.exceptionOrNull() as DacException
        assertEquals(DacErrorCode.CONFIGURATION_SELECTION_FAILED, error.code)
        assertEquals(DacErrorCode.CONFIGURATION_RESTORE_FAILED, (error.suppressed.single() as DacException).code)
        assertEquals(2, io.active)
    }
    @Test fun handBackRefusesToOverwriteAnExternalConfigurationChange() {
        val io = Io()
        val transaction = ConfigurationTransaction.select(io, 2, listOf(1, 2, 3))
        io.active = 3
        val error = runCatching { transaction.restore() }.exceptionOrNull() as DacException
        assertEquals(DacErrorCode.CONFIGURATION_RESTORE_FAILED, error.code)
        assertEquals(listOf(2), io.writes)
        assertEquals(3, io.active)
    }
    @Test fun closeRestoreRefusalRemainsRetryableAndDoesNotSelectFallback() {
        val io = Io()
        val transaction = ConfigurationTransaction.select(io, 2, listOf(1, 2, 3))
        io.rejectRestore = true
        val error = runCatching { transaction.restore() }.exceptionOrNull() as DacException
        assertEquals(DacErrorCode.CONFIGURATION_RESTORE_FAILED, error.code)
        io.rejectRestore = false
        transaction.restore()
        assertEquals(listOf(2, 1, 1), io.writes)
        assertEquals(1, io.active)
    }
}
