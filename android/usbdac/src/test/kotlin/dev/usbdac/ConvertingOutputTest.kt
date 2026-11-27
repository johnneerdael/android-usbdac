package dev.usbdac

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Assert.fail
import org.junit.Test
import java.nio.ByteBuffer

private fun expectCode(code: DacErrorCode, block: () -> Unit) {
    try {
        block()
    } catch (error: DacException) {
        assertEquals(error.message, code, error.code)
        return
    }
    fail("expected $code")
}

private fun planFor(backend: FakeConvertBackend): ConversionPlan =
    ConversionPlan.between(
        backend,
        StreamRequest.pcm(24_000, 1, 16),
        StreamFormat(
            formatId = 1, encoding = StreamEncoding.PCM, dsdBitRate = null, sampleRate = 48_000, channels = 1,
            validBits = 16, subslotBytes = 2, bytesPerFrame = 2, counterRateHz = 48_000, interfaceNumber = 1, altSetting = 1,
            endpointAddress = 1, syncType = SyncType.ASYNCHRONOUS, explicitFeedback = true, linkSpeed = LinkSpeed.HIGH,
            serviceIntervalUs = 125, rateDiffersFromRequest = true,
        ),
        ConversionOptions(),
    )

private class Rig(backend: FakeConvertBackend = FakeConvertBackend()) {
    val backend = backend
    val target = RecordingOutput()
    val converter = PcmConverter.create(planFor(backend))
    val output = ConvertingOutput(target, converter)
}

class ConvertingOutputTest {
    // ---- bit-perfect ----

    @Test
    fun aLossyPlanIsNotVerifiedAndSaysWhatWasDoneEvenIfTheTargetIsVerified() {
        val rig = Rig()
        assertEquals(BitPerfectState.Verified, rig.target.bitPerfect.value)
        val state = rig.output.bitPerfect.value
        assertEquals(BitPerfectState.NotVerified(null, rig.output.plan.description), state)
        assertFalse(state.isVerified)
        rig.target.setBitPerfect(BitPerfectState.Verified)
        assertFalse(rig.output.bitPerfect.value.isVerified)
    }

    @Test
    fun aLosslessPlanIsNotVerifiedEither() {
        val backend = FakeConvertBackend(planJson(kind = "lossless", description = "Lossless conversion, but the audio is not identical: mono duplicated to stereo.", resamples = false, delayFrames = 0, targetRate = 24_000))
        val rig = Rig(backend)
        assertEquals(BitPerfectState.NotVerified(null, "Lossless conversion, but the audio is not identical: mono duplicated to stereo."), rig.output.bitPerfect.value)
    }

    @Test
    fun aBitExactPlanPassesTheTargetsStateThrough() {
        val backend = FakeConvertBackend(planJson(kind = "bit_exact", description = "Bit-exact conversion: x.", resamples = false, delayFrames = 0, targetRate = 24_000, steps = "[]"))
        val rig = Rig(backend)
        assertEquals(BitPerfectState.Verified, rig.output.bitPerfect.value)
        val unverified = BitPerfectState.NotVerified(BitPerfectBlocker.LOST_FRAMES, "lost")
        rig.target.setBitPerfect(unverified)
        assertEquals(unverified, rig.output.bitPerfect.value)
    }

    @Test
    fun aPlanWithNoConversionForwardsEverythingAndNeverTouchesTheConverter() {
        val backend = FakeConvertBackend(planJson(kind = "none", description = "No conversion: the samples pass through unchanged.", resamples = false, delayFrames = 0, targetRate = 24_000, steps = "[]"))
        val rig = Rig(backend)
        assertEquals(BitPerfectState.Verified, rig.output.bitPerfect.value)
        rig.target.latency = 77
        rig.target.played = 5
        assertEquals(2, rig.output.write(directOf(1, 2)))
        assertEquals(listOf<Byte>(1, 2), rig.target.received)
        assertEquals(77L, rig.output.latencyFrames)
        assertEquals(5L, rig.output.playedFrames)
        rig.output.flush()
        rig.output.drain()
        assertTrue(backend.calls.none { it == "process" || it == "finish" || it == "reset" })
    }

    @Test
    fun paddedCopyUsesConverterAndRetainsVerifiedVerdict() {
        val backend = FakeConvertBackend(planJson(kind = "none", description = "conditional copy", resamples = false, delayFrames = 0, targetRate = 24_000, steps = "[]"))
        val target = RecordingOutput()
        val base = planFor(backend)
        val json = org.json.JSONObject(base.requestJson)
        json.getJSONObject("source").getJSONObject("sample").put("bits", 16).put("slot_bytes", 4)
        val padded = ConversionPlan(
            backend = backend, requestJson = json.toString(), kind = ConversionKind.NONE,
            description = "conditional copy", steps = emptyList(), sourceSampleRate = 24_000,
            sourceChannels = 1, sourceFrameBytes = 4, targetSampleRate = 24_000,
            targetChannels = 1, targetFrameBytes = 4, resamples = false, delayFrames = 0,
            minOutputBytes = 4, resampler = null,
        )
        val output = ConvertingOutput(target, PcmConverter.create(padded))
        assertTrue(output.bitPerfect.value.isVerified)
        output.write(directOf(0, 0, 1, 2))
        assertTrue(backend.calls.contains("process"))
    }

    @Test
    fun failedConversionNeverForwardsScratchOnLaterCalls() {
        val backend = FakeConvertBackend()
        val target = RecordingOutput()
        val converter = PcmConverter.create(planFor(backend))
        val output = ConvertingOutput(target, converter)
        output.write(directOf(1, 2))
        val before = target.received.toList()
        converter.finish(ByteBuffer.allocateDirect(16))
        expectCode(DacErrorCode.INVALID_STATE) { output.write(directOf(3, 4)) }
        output.writableBytes
        assertEquals(before, target.received)
    }

    // ---- writing ----

    @Test
    fun writeConvertsAndPassesTheResultOn() {
        val rig = Rig()
        val input = directOf(1, 2, 3, 4)
        assertEquals(4, rig.output.write(input))
        assertFalse(input.hasRemaining())
        assertEquals(listOf<Byte>(1, 2, 1, 2, 3, 4, 3, 4), rig.target.received)
    }

    @Test
    fun aHeapBufferIsStagedAndAdvanced() {
        val rig = Rig()
        val input = heapOf(9, 8, 7, 6)
        assertEquals(4, rig.output.write(input))
        assertFalse(input.hasRemaining())
        assertEquals(listOf<Byte>(9, 8, 9, 8, 7, 6, 7, 6), rig.target.received)
    }

    @Test
    fun aPartialFrameStaysWithTheCaller() {
        val rig = Rig()
        val input = directOf(1, 2, 3)
        assertEquals(2, rig.output.write(input))
        assertEquals(1, input.remaining())
        assertEquals(0, rig.output.write(input))
        assertEquals(1, input.remaining())
    }

    @Test
    fun whenTheTargetIsFullTheConvertedAudioWaitsAndNothingIsLostOrReordered() {
        val rig = Rig()
        rig.target.room = 3 // the target takes 3 of the 8 converted bytes
        val input = directOf(1, 2, 3, 4)
        assertEquals("the source bytes are consumed: their audio is held in the output", 4, rig.output.write(input))
        assertEquals(listOf<Byte>(1, 2, 1), rig.target.received)
        // Nothing more is taken while the rest waits.
        assertEquals(0, rig.output.write(directOf(5, 6)))
        rig.target.writable = 5 // a full target says so: all that is left is what waits
        assertEquals(0, rig.output.writableBytes)
        rig.target.writable = 100_000
        rig.target.room = 100
        assertEquals(2, rig.output.write(directOf(5, 6)))
        assertEquals(listOf<Byte>(1, 2, 1, 2, 3, 4, 3, 4, 5, 6, 5, 6), rig.target.received)
    }

    @Test
    fun writableBytesIsInSourceUnits() {
        val rig = Rig()
        rig.target.writable = 4800 // 2400 target frames of 2 bytes = 1200 source frames = 2400 source bytes
        assertEquals(2400, rig.output.writableBytes)
        rig.target.writable = 3
        assertEquals(0, rig.output.writableBytes)
    }

    @Test
    fun writableBytesSubtractsWhatIsStillPending() {
        val rig = Rig()
        rig.target.room = 0
        rig.output.write(directOf(1, 2, 3, 4)) // 8 converted bytes pending
        rig.target.writable = 20
        rig.target.room = 0
        assertEquals((20 - 8) / 2 * 24_000 / 48_000 * 2, rig.output.writableBytes)
    }

    // ---- position ----

    @Test
    fun playedFramesAreSourceFrames() {
        val rig = Rig()
        rig.target.played = 48_000
        assertEquals(24_000L, rig.output.playedFrames)
        rig.target.played = 1
        assertEquals(0L, rig.output.playedFrames)
    }

    @Test
    fun latencyIsTheTargetsPlusWhatIsPendingPlusTheConverterDelayInSourceFrames() {
        val rig = Rig()
        rig.target.latency = 4800 // target frames
        assertEquals(4800L * 24_000 / 48_000 + 3, rig.output.latencyFrames)
        rig.target.room = 0
        rig.output.write(directOf(1, 2, 3, 4)) // 4 target frames pending
        assertEquals((4800L + 4) * 24_000 / 48_000 + 3, rig.output.latencyFrames)
    }

    // ---- controls ----

    @Test
    fun pausePlayAreForwarded() {
        val rig = Rig()
        rig.output.pause()
        rig.output.play()
        assertEquals(listOf("pause", "play"), rig.target.calls)
    }

    @Test
    fun flushResetsTheConverterDropsWhatWaitsAndForwards() {
        val rig = Rig()
        rig.target.room = 0
        rig.output.write(directOf(1, 2, 3, 4))
        rig.target.room = 100
        rig.output.flush()
        assertTrue(rig.backend.calls.contains("reset"))
        assertEquals("flush", rig.target.calls.last())
        assertTrue("the pending audio was dropped, not delivered", rig.target.received.isEmpty())
        assertEquals(2, rig.output.write(directOf(7, 8)))
        assertEquals(listOf<Byte>(7, 8, 7, 8), rig.target.received)
    }

    @Test
    fun drainWritesTheConvertersTailBeforeDrainingTheTarget() {
        val rig = Rig()
        rig.output.write(directOf(1, 2))
        rig.output.drain()
        // The 3 frames of tail (0xEE) went to the target, and only then the target was drained.
        assertEquals(listOf<Byte>(1, 2, 1, 2) + List(6) { 0xEE.toByte() }, rig.target.received)
        val relevant = rig.target.calls.filter { it == "write" || it == "drain" }
        assertEquals("the target is drained last, once", listOf("write", "write", "drain"), relevant)
        assertFalse(rig.output.isDrained)
        rig.target.drained = true
        assertTrue(rig.output.isDrained)
        expectCode(DacErrorCode.INVALID_STATE) { rig.output.write(directOf(1, 2)) }
        assertEquals(0, rig.output.writableBytes)
    }

    @Test
    fun aTailTheTargetCannotTakeYetIsDeliveredWhenItCanAndOnlyThenIsTheTargetDrained() {
        val rig = Rig()
        rig.target.room = 0
        rig.output.drain()
        assertFalse(rig.target.calls.contains("drain"))
        assertFalse(rig.output.isDrained)
        rig.target.room = 4 // two frames of the three-frame tail
        rig.output.isDrained // polling moves the tail along
        assertFalse(rig.target.calls.contains("drain"))
        rig.target.room = 100
        rig.target.drained = true
        assertTrue(rig.output.isDrained)
        assertEquals(List(6) { 0xEE.toByte() }, rig.target.received)
        assertEquals(1, rig.target.calls.count { it == "drain" })
    }

    @Test
    fun flushAfterDrainAcceptsWritesAgain() {
        val rig = Rig()
        rig.output.drain()
        rig.output.flush()
        assertEquals(2, rig.output.write(directOf(1, 2)))
    }

    // ---- closing ----

    @Test
    fun closeClosesTheTargetAndTheConverterOnceAndTheOutputThenRefuses() {
        val rig = Rig()
        rig.output.close()
        rig.output.close()
        assertTrue(rig.target.closed)
        assertEquals(1, rig.target.calls.count { it == "close" })
        assertEquals(1, rig.backend.calls.count { it == "close" })
        expectCode(DacErrorCode.INVALID_STATE) { rig.output.write(directOf(1, 2)) }
        expectCode(DacErrorCode.INVALID_STATE) { rig.output.play() }
        assertEquals(0, rig.output.writableBytes)
        assertEquals(0L, rig.output.playedFrames)
        assertEquals(0L, rig.output.latencyFrames)
        assertFalse(rig.output.isDrained)
    }
}
