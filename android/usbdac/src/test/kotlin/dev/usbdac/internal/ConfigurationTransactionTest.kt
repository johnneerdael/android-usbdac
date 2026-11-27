package dev.usbdac.internal

import dev.usbdac.StreamRequest
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
    @Test fun fallbackRequiresCompleteUac3AndLegacyDescriptorProof() {
        val active = byteArrayOf(
            9, 2, 35, 0, 2, 9, 0, -128, 50,
            8, 11, 0, 2, 1, 1, 0x30, 0,
            9, 4, 0, 0, 0, 1, 1, 0x30, 0,
            9, 4, 1, 0, 0, 1, 2, 0x30, 0,
        )
        val first = byteArrayOf(
            9, 2, 73, 0, 2, 7, 0, -128, 50,
            8, 11, 0, 2, 1, 0, 0x20, 0,
            9, 4, 0, 0, 0, 1, 1, 0x20, 0,
            9, 4, 1, 0, 0, 1, 2, 0x20, 0,
            9, 4, 1, 1, 1, 1, 2, 0x20, 0,
            16, 0x24, 1, 1, 0, 1, 1, 0, 0, 0, 2, 3, 0, 0, 0, 0,
            6, 0x24, 2, 1, 2, 16,
            7, 5, 1, 1, 64, 0, 1,
        )
        val current = parseFallbackDescriptor(active)!!
        val legacy = parseFallbackDescriptor(first, StreamRequest.pcm(48_000, 2, 16))!!
        assertEquals(7, legacyFallbackId(current, legacy, listOf(7, 9)))
        assertNull(legacyFallbackId(current, legacy, listOf(7, 7, 9)))
        assertNull(legacyFallbackId(current.copy(hasIsoOut = true), legacy, listOf(7, 9)))
        assertNull(legacyFallbackId(current, legacy.copy(legacyPlayback = false), listOf(7, 9)))
        assertNull(parseFallbackDescriptor(first.copyOf(first.size - 1)))
        val malformed = first.clone().apply { this[66] = 2 }
        assertNull(parseFallbackDescriptor(malformed))
        val io = Io(active = 9)
        val selection = ConfigurationTransaction.select(io, legacy.id, listOf(7, 9))
        selection.restore()
        assertEquals(listOf(7, 9), io.writes)
    }
    @Test fun fullUac3FormatExclusionRequiresEveryOutputToBeKnown() {
        val bytes = byteArrayOf(
            9, 2, 74, 0, 2, 9, 0, -128, 50,
            8, 11, 0, 2, 1, 1, 0x30, 0,
            9, 4, 0, 0, 0, 1, 1, 0x30, 0,
            9, 4, 1, 0, 0, 1, 2, 0x30, 0,
            9, 4, 1, 1, 1, 1, 2, 0x30, 0,
            23, 0x24, 1, 1, 0, 0, 0, 0, 1, 0, 1, 0, 0, 0, 0, 0, 0, 0, 3, 24, 0, 0, 0,
            7, 5, 1, 1, 64, 0, 1,
        )
        assertTrue(parseFallbackDescriptor(bytes, StreamRequest.pcm(48_000, 2, 16))!!.formatExcluded)
        assertFalse(parseFallbackDescriptor(bytes, StreamRequest.pcm(48_000, 2, 24))!!.formatExcluded)
        val unknown = bytes.clone().apply { this[52] = 0 }
        assertFalse(parseFallbackDescriptor(unknown, StreamRequest.pcm(48_000, 2, 16))!!.formatExcluded)
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
