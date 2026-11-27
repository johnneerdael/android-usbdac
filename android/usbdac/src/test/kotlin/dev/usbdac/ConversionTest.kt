package dev.usbdac

import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Assert.fail
import org.junit.Test
import java.nio.ByteBuffer

private fun streamFormat(rate: Int = 48_000, channels: Int = 2, validBits: Int = 24, subslotBytes: Int = 3, encoding: StreamEncoding = StreamEncoding.PCM) =
    StreamFormat(
        formatId = 1,
        encoding = encoding,
        dsdBitRate = null,
        sampleRate = rate,
        channels = channels,
        validBits = validBits,
        subslotBytes = subslotBytes,
        bytesPerFrame = subslotBytes * channels,
        counterRateHz = rate,
        interfaceNumber = 1,
        altSetting = 1,
        endpointAddress = 1,
        syncType = SyncType.ASYNCHRONOUS,
        explicitFeedback = true,
        linkSpeed = LinkSpeed.HIGH,
        serviceIntervalUs = 125,
        rateDiffersFromRequest = true,
    )

private fun failsWith(code: DacErrorCode, block: () -> Unit): DacException {
    try {
        block()
    } catch (error: DacException) {
        assertEquals(error.message, code, error.code)
        return error
    }
    fail("expected $code")
    throw AssertionError()
}

class ConversionPlanTest {
    @Test
    fun theRequestCarriesTheSourceAndTheNegotiatedFormatsLayouts() {
        val backend = FakeConvertBackend()
        ConversionPlan.between(backend, StreamRequest.pcm(44_100, 2, 16), streamFormat(48_000, 2, 24, 4), ConversionOptions())
        val json = JSONObject(backend.planRequest!!)
        val source = json.getJSONObject("source")
        assertEquals(44_100, source.getInt("sample_rate"))
        assertEquals(2, source.getInt("channels"))
        assertEquals("int", source.getJSONObject("sample").getString("kind"))
        assertEquals(16, source.getJSONObject("sample").getInt("bits"))
        assertEquals(2, source.getJSONObject("sample").getInt("slot_bytes"))
        val target = json.getJSONObject("target")
        assertEquals(48_000, target.getInt("sample_rate"))
        assertEquals(24, target.getJSONObject("sample").getInt("bits"))
        assertEquals(4, target.getJSONObject("sample").getInt("slot_bytes"))
    }

    @Test
    fun theBitDepthPicksTheSourceLayout() {
        for ((depth, bits, slot) in listOf(Triple(16, 16, 2), Triple(24, 24, 3), Triple(32, 32, 4))) {
            val backend = FakeConvertBackend()
            ConversionPlan.between(backend, StreamRequest.pcm(44_100, 2, depth), streamFormat(), ConversionOptions())
            val sample = JSONObject(backend.planRequest!!).getJSONObject("source").getJSONObject("sample")
            assertEquals(bits, sample.getInt("bits"))
            assertEquals(slot, sample.getInt("slot_bytes"))
        }
    }

    @Test
    fun aFloatOrPaddedSourceLayoutOverridesTheDepth() {
        val backend = FakeConvertBackend()
        ConversionPlan.between(backend, StreamRequest.pcm(44_100, 2, 32), streamFormat(), ConversionOptions(sourceLayout = PcmLayout.FLOAT_32))
        assertEquals("float", JSONObject(backend.planRequest!!).getJSONObject("source").getJSONObject("sample").getString("kind"))
        ConversionPlan.between(backend, StreamRequest.pcm(44_100, 2, 24), streamFormat(), ConversionOptions(sourceLayout = PcmLayout.PCM_24_IN_32))
        val sample = JSONObject(backend.planRequest!!).getJSONObject("source").getJSONObject("sample")
        assertEquals(24, sample.getInt("bits"))
        assertEquals(4, sample.getInt("slot_bytes"))
        for ((layout, slot) in listOf(PcmLayout.PCM_16_IN_24 to 3, PcmLayout.PCM_16_IN_32 to 4)) {
            ConversionPlan.between(backend, StreamRequest.pcm(44_100, 2, 32), streamFormat(), ConversionOptions(sourceLayout = layout))
            val padded = JSONObject(backend.planRequest!!).getJSONObject("source").getJSONObject("sample")
            assertEquals(16, padded.getInt("bits"))
            assertEquals(slot, padded.getInt("slot_bytes"))
        }
    }

    @Test
    fun floatEncodingUsesFloatLayoutAndRejectsContradictions() {
        val backend = FakeConvertBackend()
        val request = StreamRequest.pcmFloat32(44_100, 2)
        ConversionPlan.between(backend, request, streamFormat(), ConversionOptions())
        val sample = JSONObject(backend.planRequest!!).getJSONObject("source").getJSONObject("sample")
        assertEquals("float", sample.getString("kind"))
        try {
            ConversionPlan.between(backend, request, streamFormat(), ConversionOptions(sourceLayout = PcmLayout.PCM_32))
            throw AssertionError("contradictory layout accepted")
        } catch (error: DacException) {
            assertEquals(DacErrorCode.INVALID_ARGUMENT, error.code)
        }
    }

    @Test
    fun theOptionsAreSent() {
        val backend = FakeConvertBackend()
        val options = ConversionOptions(NarrowingMode.TRUNCATE, ResampleQuality.FAST, headroomDb = -3.5, ditherSeed = 99)
        ConversionPlan.between(backend, StreamRequest.pcm(44_100, 2, 16), streamFormat(), options)
        val sent = JSONObject(backend.planRequest!!).getJSONObject("options")
        assertEquals("truncate", sent.getString("narrowing"))
        assertEquals("fast", sent.getString("quality"))
        assertEquals(-3.5, sent.getDouble("headroom_db"), 0.0)
        assertEquals(99L, sent.getLong("dither_seed"))
        ConversionPlan.between(backend, StreamRequest.pcm(44_100, 2, 16), streamFormat(), ConversionOptions())
        assertTrue(JSONObject(backend.planRequest!!).getJSONObject("options").isNull("dither_seed"))
        assertEquals(-3.5, JSONObject(backend.planRequest!!).getJSONObject("options").getDouble("headroom_db"), 0.0)
    }

    @Test
    fun theNativePlanIsReadIntoTheModel() {
        val plan = ConversionPlan.between(FakeConvertBackend(), StreamRequest.pcm(24_000, 1, 16), streamFormat(48_000, 1, 16, 2), ConversionOptions())
        assertEquals(ConversionKind.LOSSY, plan.kind)
        assertFalse(plan.preservesBitPerfect)
        assertTrue(plan.description.startsWith("Lossy conversion, not bit-perfect"))
        assertEquals(listOf("resample"), plan.steps.map { it.code })
        assertEquals(ConversionKind.LOSSY, plan.steps[0].kind)
        assertEquals(24_000, plan.sourceSampleRate)
        assertEquals(48_000, plan.targetSampleRate)
        assertEquals(2, plan.sourceFrameBytes)
        assertEquals(2, plan.targetFrameBytes)
        assertTrue(plan.resamples)
        assertEquals(3, plan.delayFrames)
        assertEquals(4, plan.minOutputBytes)
        assertEquals(ResamplerInfo(2, 1, 6, 10_920.0, 12_000.0, 110.0), plan.resampler)
    }

    @Test
    fun aPlanWithoutResamplingHasNoFilter() {
        val backend = FakeConvertBackend(planJson(kind = "bit_exact", description = "Bit-exact conversion: x.", resamples = false, delayFrames = 0, steps = "[]"))
        val plan = ConversionPlan.between(backend, StreamRequest.pcm(44_100, 2, 16), streamFormat(), ConversionOptions())
        assertEquals(ConversionKind.BIT_EXACT, plan.kind)
        assertTrue(plan.preservesBitPerfect)
        assertNull(plan.resampler)
        assertEquals(0, plan.delayFrames)
    }

    @Test
    fun theKindsKnowWhichKeepBitPerfect() {
        assertTrue(ConversionKind.NONE.preservesBitPerfect)
        assertTrue(ConversionKind.BIT_EXACT.preservesBitPerfect)
        assertFalse(ConversionKind.LOSSLESS.preservesBitPerfect)
        assertFalse(ConversionKind.LOSSY.preservesBitPerfect)
    }

    @Test
    fun dsdIsNeverConvertedAndNothingNativeIsCalled() {
        val backend = FakeConvertBackend()
        failsWith(DacErrorCode.UNSUPPORTED) {
            ConversionPlan.between(backend, StreamRequest.dop(2_822_400, 2), streamFormat(), ConversionOptions())
        }
        failsWith(DacErrorCode.UNSUPPORTED) {
            ConversionPlan.between(backend, StreamRequest.pcm(176_400, 2, 24), streamFormat(176_400, 2, 24, 3, StreamEncoding.DOP), ConversionOptions())
        }
        failsWith(DacErrorCode.UNSUPPORTED) {
            ConversionPlan.between(backend, StreamRequest.dsdNative(2_822_400, 2), streamFormat(), ConversionOptions())
        }
        assertTrue(backend.calls.isEmpty())
    }

    @Test
    fun badOptionsAndDepthsAreInvalidArguments() {
        val backend = FakeConvertBackend()
        failsWith(DacErrorCode.INVALID_ARGUMENT) {
            ConversionPlan.between(backend, StreamRequest.pcm(44_100, 2, 20), streamFormat(), ConversionOptions())
        }
        failsWith(DacErrorCode.INVALID_ARGUMENT) {
            ConversionPlan.between(backend, StreamRequest.pcm(44_100, 2, 16), streamFormat(), ConversionOptions(ditherSeed = -1))
        }
        assertTrue(backend.calls.isEmpty())
    }

    @Test
    fun aNativeRefusalIsTyped() {
        val backend = FakeConvertBackend().apply {
            failPlan = IllegalStateException("usbdac:unsupported: 6 channels cannot be converted to 2: only mono to stereo is supported")
        }
        val error = failsWith(DacErrorCode.UNSUPPORTED) {
            ConversionPlan.between(backend, StreamRequest.pcm(44_100, 6, 16), streamFormat(), ConversionOptions())
        }
        assertTrue(error.detail, error.detail.contains("only mono to stereo"))
    }

    @Test
    fun aPlanTheLibraryCannotReadIsATypedFailure() {
        val backend = FakeConvertBackend("""{ "kind": "lossy" }""")
        failsWith(DacErrorCode.UNKNOWN) {
            ConversionPlan.between(backend, StreamRequest.pcm(44_100, 2, 16), streamFormat(), ConversionOptions())
        }
    }

    @Test
    fun anUnknownKindIsNotTrusted() {
        assertEquals(ConversionKind.LOSSY, ConversionKind.fromWireName("from_the_future"))
    }
}

class PcmConverterTest {
    private fun converter(backend: FakeConvertBackend = FakeConvertBackend()): PcmConverter =
        PcmConverter.create(ConversionPlan.between(backend, StreamRequest.pcm(24_000, 1, 16), streamFormat(48_000, 1, 16, 2), ConversionOptions()))

    @Test
    fun createSendsTheSameRequestTheNativePlanWasGiven() {
        val backend = FakeConvertBackend()
        converter(backend)
        assertEquals(backend.planRequest, backend.requestJson)
        assertEquals(listOf("plan", "create"), backend.calls)
    }

    @Test
    fun convertMovesBothPositionsByWhatTheNativeLayerReports() {
        val backend = FakeConvertBackend()
        val converter = converter(backend)
        val input = directOf(1, 2, 3, 4, 5, 6)
        input.position(2)
        val output = ByteBuffer.allocateDirect(8)
        output.position(1)
        output.limit(7) // room for 6 bytes: three frames of two; each source frame becomes two
        val result = converter.convert(input, output)
        // Four bytes are left in the input (two frames), room for one frame's two copies in the output.
        assertEquals(ConvertResult(consumed = 2, produced = 4), result)
        assertEquals(4, input.position())
        assertEquals(5, output.position())
        assertEquals(2, backend.lastInOffset)
        assertEquals(1, backend.lastOutOffset)
    }

    @Test
    fun convertDuplicatesAsTheFakeDoesAndStopsWhenTheOutputIsFull() {
        val converter = converter()
        val input = directOf(1, 2, 3, 4)
        val output = ByteBuffer.allocateDirect(4)
        val result = converter.convert(input, output)
        assertEquals(ConvertResult(consumed = 2, produced = 4), result)
        assertEquals(2, input.remaining())
        assertEquals(listOf<Byte>(1, 2, 1, 2), List(4) { output.get(it) })
    }

    @Test
    fun aHeapBufferIsRefused() {
        val converter = converter()
        failsWith(DacErrorCode.INVALID_ARGUMENT) { converter.convert(heapOf(1, 2), ByteBuffer.allocateDirect(8)) }
        failsWith(DacErrorCode.INVALID_ARGUMENT) { converter.convert(directOf(1, 2), ByteBuffer.allocate(8)) }
        failsWith(DacErrorCode.INVALID_ARGUMENT) { converter.finish(ByteBuffer.allocate(8)) }
    }

    @Test
    fun readOnlyOutputIsRejectedWithoutChangingPositionsOrState() {
        val converter = converter()
        val input = directOf(1, 2).asReadOnlyBuffer()
        val output = ByteBuffer.allocateDirect(8).asReadOnlyBuffer()
        failsWith(DacErrorCode.INVALID_ARGUMENT) { converter.convert(input, output) }
        failsWith(DacErrorCode.INVALID_ARGUMENT) { converter.finish(output) }
        assertEquals(0, input.position())
        assertEquals(0, output.position())
        assertEquals(ConvertResult(2, 4), converter.convert(input, ByteBuffer.allocateDirect(8)))
    }

    @Test
    fun finishWritesTheTailAndThenZeroAndConvertThenFailsUntilReset() {
        val converter = converter()
        val output = ByteBuffer.allocateDirect(64)
        assertEquals(6, converter.finish(output)) // 3 frames of 2 bytes
        assertEquals(6, output.position())
        assertEquals(0, converter.finish(output))
        failsWith(DacErrorCode.INVALID_STATE) { converter.convert(directOf(1, 2), output) }
        converter.reset()
        assertEquals(ConvertResult(2, 4), converter.convert(directOf(1, 2), output))
    }

    @Test
    fun clippedSamplesComeFromTheNativeLayer() {
        val backend = FakeConvertBackend()
        val converter = converter(backend)
        backend.clipped = 12
        assertEquals(12L, converter.clippedSamples)
    }

    @Test
    fun closeReleasesOnceAndTheConverterThenRefuses() {
        val backend = FakeConvertBackend()
        val converter = converter(backend)
        converter.close()
        converter.close()
        assertEquals(1, backend.calls.count { it == "close" })
        failsWith(DacErrorCode.INVALID_STATE) { converter.convert(directOf(1, 2), ByteBuffer.allocateDirect(8)) }
        failsWith(DacErrorCode.INVALID_STATE) { converter.reset() }
    }

    @Test
    fun aFailureToCreateIsTyped() {
        val backend = FakeConvertBackend().apply {
            failCreate = IllegalStateException("usbdac:unsupported: converting 44100 Hz to 47993 Hz needs a filter of 160 taps in 47993 phases")
        }
        val plan = ConversionPlan.between(backend, StreamRequest.pcm(24_000, 1, 16), streamFormat(48_000, 1, 16, 2), ConversionOptions())
        failsWith(DacErrorCode.UNSUPPORTED) { PcmConverter.create(plan) }
        assertNotNull(plan)
    }
}
