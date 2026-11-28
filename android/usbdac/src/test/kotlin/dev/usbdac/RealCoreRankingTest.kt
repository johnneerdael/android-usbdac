package dev.usbdac

import dev.usbdac.internal.NativeRateRanking
import dev.usbdac.internal.OutputPlatform
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Assert.assertNull
import org.junit.Assert.assertArrayEquals
import java.nio.ByteBuffer
import org.junit.Assume.assumeTrue
import org.junit.BeforeClass
import org.junit.Test

/** Runs the production Rust ranking through JNI while the USB hardware is synthetic. */
class RealCoreRankingTest {
    companion object {
        @BeforeClass @JvmStatic fun loadHostJni() {
            val path = System.getenv("USBDAC_TEST_HOST_JNI")
            assumeTrue("named host-JNI gate supplies USBDAC_TEST_HOST_JNI", !path.isNullOrBlank())
            System.load(checkNotNull(path))
        }
    }

    @Test fun selectorUsesCoreRankingForDiscreteAndSteppedAlternates() {
        data class Case(val input: Int, val rates: List<Int>, val range: RateRange?, val expected: Int)
        val cases = listOf(
            Case(192_000, listOf(44_100, 48_000, 88_200, 96_000), null, 96_000),
            Case(176_400, listOf(44_100, 48_000, 88_200, 96_000), null, 88_200),
            Case(44_100, listOf(48_000), null, 48_000),
            Case(192_000, emptyList(), RateRange(8_000, 96_000, 1), 96_000),
        )
        for (case in cases) {
            val fake = FakePlatform()
            val link = fake.attach(fakeDac())
            val alt = AltSetting(1, 1, 1, AltFormat.PCM, 2, 2, 16,
                listOfNotNull(case.range), case.rates, SyncType.ASYNCHRONOUS,
                0x81, 1024, 125, UacVersion.V2, Pacing.ExplicitFeedback(0x81))
            link.capabilities = fakeCapabilities(depths = listOf(16)).copy(altSettings = listOf(alt))
            link.onNegotiate = { request ->
                val rate = request.sampleRate
                val supported = rate in case.rates || case.range?.let { rate in it.minHz..it.maxHz && (rate - it.minHz) % it.stepHz == 0 } == true
                if (!supported) throw unsupported()
                fakeFormat(rate, 16, 2)
            }
            var nativeCalls = 0
            val platform = object : OutputPlatform by fake {
                override fun rankRates(requested: Int, rates: List<Int>): List<Int> {
                    nativeCalls++
                    return NativeRateRanking.rankList(requested, rates)
                }
            }
            val decision = OutputSelector(platform, OutputPolicy()).select(StreamRequest.pcm(case.input, 2, 16))
            assertEquals(case.expected, decision.wireFormat!!.sampleRate)
            assertTrue(nativeCalls > 0)
        }
    }
    @Test fun pcmDescriptorAdmissionMatchesRealCoreShapeRateAndPacketPredicates() {
        val base = AltSetting(1, 1, 1, AltFormat.PCM, 2, 3, 24, listOf(RateRange(48_000, 96_000, 48_000)), emptyList(),
            SyncType.ASYNCHRONOUS, 0x81, 72, 125, UacVersion.V2, Pacing.ExplicitFeedback(0x81))
        val request = StreamRequest.pcm(96_000, 2, 24)
        val cases = listOf(
            base to request,
            base.copy(maxPacketBytes = 71) to request,
            base.copy(pacing = null) to request,
            base to request.copy(channels = 1),
            base to request.copy(bitDepth = 16),
            base to request.copy(sampleRate = 72_000),
            base.copy(rateRanges = listOf(RateRange(48_000, 96_000, 0))) to request,
            base.copy(format = AltFormat.PCM_FLOAT32) to request,
        )
        for ((alt, source) in cases) {
            val range = alt.rateRanges.single()
            val fmt = when (alt.format) { AltFormat.PCM -> 0; AltFormat.PCM_U8 -> 1; AltFormat.PCM_FLOAT32 -> 2; else -> 3 }
            val values = listOf(source.sampleRate, source.channels, source.bitDepth, 0, alt.channels, alt.validBits,
                alt.subslotBytes, fmt, alt.serviceIntervalUs, alt.maxPacketBytes, if (alt.pacing != null) 1 else 0,
                if (source.sampleRate in alt.sampleRates) 1 else 0, range.minHz, range.maxHz, range.stepHz)
            val core = NativeRateRanking.pcmAdmission(org.json.JSONArray(values).toString()).toBooleanStrict()
            assertEquals("$alt / $source", core, pcmAlternateSupports(alt, source))
        }
    }

    @Test fun paddedPcmRejectionLeavesDirectAndHeapWritesRetryable() {
        assumeTrue("host converter gate requires USBDAC_TEST_DSD_PCM=1 and java.library.path",
            System.getenv("USBDAC_TEST_DSD_PCM") == "1")
        fun newOutput(): Pair<ConvertingOutput, RecordingOutput> {
            val target = RecordingOutput()
            val plan = ConversionPlan.between(StreamRequest.pcm(48_000, 1, 16), fakeFormat(48_000, 16, 2, 1),
                ConversionOptions(sourceLayout = PcmLayout.PCM_16_IN_32))
            return ConvertingOutput(target, PcmConverter.create(plan)) to target
        }
        for (direct in listOf(false, true)) {
            val (output, target) = newOutput()
            val (reference, expected) = newOutput()
            try {
                val input = if (direct) ByteBuffer.allocateDirect(4) else ByteBuffer.allocate(4)
                input.put(byteArrayOf(1, 0, 0x34, 0x12)).flip()
                val beforeLatency = output.latencyFrames
                val error = org.junit.Assert.assertThrows(DacException::class.java) { output.write(input) }
                assertEquals(DacErrorCode.INVALID_ARGUMENT, error.code)
                assertEquals(0, input.position())
                assertTrue(target.received.isEmpty())
                assertEquals(beforeLatency, output.latencyFrames)
                input.put(0, 0)
                assertEquals(4, output.write(input))
                assertEquals(4, reference.write(ByteBuffer.wrap(byteArrayOf(0, 0, 0x34, 0x12))))
                assertEquals(expected.received, target.received)
            } finally { output.close(); reference.close() }
        }
    }

    @Test fun largePaddedWritesPreserveProgressAcrossInvalidChunks() {
        assumeTrue("host converter gate requires USBDAC_TEST_DSD_PCM=1", System.getenv("USBDAC_TEST_DSD_PCM") == "1")
        val bytes = 200_000 * 4
        fun output(): Pair<ConvertingOutput, RecordingOutput> {
            val target = RecordingOutput()
            val plan = ConversionPlan.between(StreamRequest.pcm(48_000, 1, 16), fakeFormat(48_000, 16, 2, 1),
                ConversionOptions(sourceLayout = PcmLayout.PCM_16_IN_32))
            return ConvertingOutput(target, PcmConverter.create(plan)) to target
        }
        for (badFrame in listOf(0, 100_000, 199_999)) {
            val traces = mutableListOf<List<Int>>()
            val results = mutableListOf<List<Byte>>()
            for (direct in listOf(false, true)) {
                val input = if (direct) ByteBuffer.allocateDirect(bytes + 16) else ByteBuffer.allocate(bytes + 16)
                input.position(8); input.limit(8 + bytes)
                input.put(8 + badFrame * 4, 1)
                val (converter, target) = output()
                val trace = mutableListOf<Int>()
                var accepted = 0
                var failed = false
                try {
                    while (input.hasRemaining()) {
                        target.room = 8192
                        val before = input.position()
                        try {
                            val count = converter.write(input)
                            assertEquals(count, input.position() - before)
                            accepted += count
                            trace += count
                        } catch (error: DacException) {
                            assertEquals(DacErrorCode.INVALID_ARGUMENT, error.code)
                            assertEquals(before, input.position())
                            assertTrue(!failed)
                            failed = true
                            input.put(8 + badFrame * 4, 0)
                        }
                    }
                    target.room = Int.MAX_VALUE
                    converter.write(input) // pump final pending output
                    assertTrue(failed)
                    assertEquals(bytes, accepted)
                    traces += trace
                    results += target.received.toList()
                } finally { converter.close() }
            }
            assertEquals(traces[0], traces[1])
            assertEquals(results[0], results[1])
            assertEquals(200_000 * 2, results[0].size)
            assertTrue(results[0].all { it == 0.toByte() })
        }
    }

    @Test fun androidDsdFallsBackWhenPreferredManagedPrecisionCannotOpen() {
        assumeTrue("host converter gate requires USBDAC_TEST_DSD_PCM=1 and java.library.path",
            System.getenv("USBDAC_TEST_DSD_PCM") == "1")
        val fake = FakePlatform().apply { apiLevel = 31 }
        val attempts = mutableListOf<Int>()
        val platform = object : OutputPlatform by fake {
            override fun openManaged(request: StreamRequest, route: OutputRoute?): PcmOutput {
                attempts += request.bitDepth
                if (request.bitDepth == 24) throw DacException(DacErrorCode.UNSUPPORTED, "rejected 24-bit format")
                return fake.openManaged(request, route)
            }
        }
        val output = OutputSelector(platform, OutputPolicy(force = OutputKind.ANDROID_STANDARD))
            .open(StreamRequest.dsdNative(2_822_400, 2))
        try {
            assertEquals(listOf(24, 32), attempts)
            assertTrue(output.decision.processing!!.description.contains("32-bit PCM"))
        } finally { output.close() }
    }

    @Test fun dsdTailSurvivesPartialTargetWrites() {
        assumeTrue("host converter gate requires USBDAC_TEST_DSD_PCM=1 and java.library.path",
            System.getenv("USBDAC_TEST_DSD_PCM") == "1")
        fun render(partial: Boolean): List<Byte> {
            val target = RecordingOutput()
            val output = DsdPcmOutput(target, 2_822_400, 2, 176_400, 2, PcmLayout.PCM_16)
            try {
                val input = ByteBuffer.allocateDirect(8192 * 2).apply { repeat(capacity()) { put(0x69.toByte()) }; flip() }
                while (input.hasRemaining()) {
                    target.room = if (partial) 4 else Int.MAX_VALUE
                    output.write(input)
                }
                target.room = if (partial) 4 else Int.MAX_VALUE
                output.drain()
                target.drained = true
                var polls = 0
                while (!output.isDrained) {
                    target.room = if (partial) 4 else Int.MAX_VALUE
                    check(++polls < 10_000)
                }
                assertEquals(8192 / 2 * 2 * 2, target.received.size)
                assertEquals(1, target.calls.count { it == "drain" })
                return target.received.toList()
            } finally { output.close() }
        }
        assertEquals(render(false), render(true))
    }

    @Test fun androidStandardDsdAndDopUseRealConverterAndPreserveSourceCounters() {
        assumeTrue("opt in with USBDAC_TEST_DSD_PCM=1 and put the host library directory on java.library.path",
            System.getenv("USBDAC_TEST_DSD_PCM") == "1")
        val byteFrames = 8_192
        fun render(dop: Boolean): ByteArray {
            val fake = FakePlatform()
            val policy = OutputPolicy(force = OutputKind.ANDROID_STANDARD, allowDopPcmConversion = dop)
            val request = if (dop) StreamRequest.dop(2_822_400, 2) else StreamRequest.dsdNative(2_822_400, 2)
            val output = OutputSelector(fake, policy).open(request)
            try {
                assertEquals(Guarantee.NONE, output.decision.guarantee)
                assertEquals(request, output.decision.source)
                assertNull(output.decision.wireFormat)
                assertEquals(dop, output.resetDopInputMarkerPhase())
                assertEquals(if (dop) StreamEncoding.DOP else StreamEncoding.DSD_NATIVE, output.format.encoding)
                assertEquals(if (dop) 176_400 else 352_800, output.format.counterRateHz)
                assertEquals(if (dop) 6 else 2, output.format.bytesPerFrame)
                assertTrue(output.bitPerfect.value is BitPerfectState.NotVerified)
                val input = if (dop) {
                    ByteBuffer.allocateDirect(byteFrames / 2 * 6).apply {
                        repeat(byteFrames / 2) { frame ->
                            val marker = if (frame % 2 == 0) 5 else 0xfa
                            repeat(2) { put(0x69.toByte()); put(0x69.toByte()); put(marker.toByte()) }
                        }
                        flip()
                    }
                } else ByteBuffer.allocateDirect(byteFrames * 2).apply {
                    repeat(byteFrames * 2) { put(0x69.toByte()) }
                    flip()
                }
                val total = input.remaining()
                var consumed = 0
                while (input.hasRemaining()) {
                    val n = output.write(input)
                    assertTrue("converter must make progress", n > 0)
                    consumed += n
                }
                assertEquals(total, consumed)
                output.drain()
                val target = fake.managed.single().first
                assertTrue(target.calls.contains("drain"))
                assertEquals(byteFrames / 2 * 2 * 3, target.received.size)
                target.played = (byteFrames / 2).toLong()
                target.drained = true
                assertTrue(output.isDrained)
                assertEquals(if (dop) byteFrames / 2L else byteFrames.toLong(), output.playedFrames)
                return target.received.toByteArray()
            } finally {
                output.close()
            }
        }
        assertArrayEquals(render(false), render(true))
    }

}
