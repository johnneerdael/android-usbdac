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

    @Test fun unsignedInputUsesProductionCoreExpansionAndKeepsSourceFrames() {
        assumeTrue("host conversion gate requires library loading", System.getenv("USBDAC_TEST_DSD_PCM") == "1")
        val target = RecordingOutput()
        val output = UnsignedPcmOutput(target, 2)
        val input = ByteBuffer.wrap(byteArrayOf(0, 128.toByte(), 255.toByte(), 64))
        assertEquals(4, output.write(input))
        assertArrayEquals(byteArrayOf(0, 128.toByte(), 0, 0, 0, 127, 0, 192.toByte()), target.received.toByteArray())
        target.played = 7; target.latency = 3
        assertEquals(7L, output.playedFrames)
        assertEquals(3L, output.latencyFrames)
        output.close()
    }

    @Test fun nativeDsdChannelExpansionUsesProductionCorePrimitive() {
        assumeTrue("host conversion gate requires library loading", System.getenv("USBDAC_TEST_DSD_PCM") == "1")
        val target = RecordingOutput()
        val output = DsdMonoExpansionOutput(target)
        val source = ByteBuffer.wrap(byteArrayOf(0x12, 0x80.toByte(), 0x5a))
        assertEquals(3, output.write(source))
        assertArrayEquals(byteArrayOf(0x12, 0x12, 0x80.toByte(), 0x80.toByte(), 0x5a, 0x5a), target.received.toByteArray())
        output.close()
    }

    // The mono expander holds no partial group, so drain reaches the stereo adapter below it, which
    // completes the tail with ALSA's DSD silence byte 0x69 (sound/core/pcm_misc.c); the DoP framer
    // frames that pad under the next DoP v1.1 marker.
    @Test fun monoDsdExpansionDrainsAnIncompleteFinalGroupWithDsdIdle() {
        assumeTrue("host conversion gate requires library loading", System.getenv("USBDAC_TEST_DSD_PCM") == "1")
        val native = RecordingOutput()
        val nativeWire = object : PcmOutput by native {
            override fun write(buffer: ByteBuffer): Int {
                val complete = buffer.remaining() / 8 * 8
                if (complete == 0) return 0
                val view = buffer.duplicate().apply { limit(position() + complete) }
                val count = native.write(view)
                buffer.position(buffer.position() + count)
                return count
            }
        }
        val toNative = DsdMonoExpansionOutput(NativeDsdInputOutput(nativeWire, channels = 2, wordBytes = 4))
        assertEquals(3, toNative.write(ByteBuffer.wrap(byteArrayOf(0x12, 0x34, 0x56))))
        assertTrue(native.received.isEmpty())
        toNative.drain()
        assertTrue(native.calls.contains("drain"))
        assertArrayEquals(byteArrayOf(0x12, 0x12, 0x34, 0x34, 0x56, 0x56, 0x69, 0x69), native.received.toByteArray())
        toNative.close()

        val dop = RecordingOutput()
        val toDop = DsdMonoExpansionOutput(DsdDopOutput(dop, 2, 3, true))
        assertEquals(3, toDop.write(ByteBuffer.wrap(byteArrayOf(0x12, 0x34, 0x56))))
        toDop.drain()
        assertTrue(dop.calls.contains("drain"))
        assertEquals(listOf(0x34, 0x12, 0x05, 0x34, 0x12, 0x05, 0x69, 0x56, 0xfa, 0x69, 0x56, 0xfa),
            dop.received.map { it.toInt() and 255 })
        toDop.close()
    }

    @Test fun expandedOutputsReturnAcceptedFramesBeforeTargetFailure() {
        assumeTrue("host conversion gate requires library loading", System.getenv("USBDAC_TEST_DSD_PCM") == "1")
        for (unsigned in listOf(true, false)) {
            val recorded = RecordingOutput()
            val failure = DacException(DacErrorCode.TRANSFER, "target failed")
            var attempts = 0
            val target = object : PcmOutput by recorded {
                override fun write(buffer: ByteBuffer): Int {
                    attempts++
                    val limit = buffer.limit()
                    buffer.limit(buffer.position() + if (unsigned) 4 else 2)
                    try { recorded.write(buffer) } finally { buffer.limit(limit) }
                    throw failure
                }
            }
            val output: PcmOutput = if (unsigned) UnsignedPcmOutput(target, 2) else DsdMonoExpansionOutput(target)
            val input = if (unsigned) ByteBuffer.wrap(byteArrayOf(0, 0, 0, 0, 7)) else ByteBuffer.wrap(byteArrayOf(1, 2, 3))
            val accepted = if (unsigned) 4 else 3
            assertEquals(accepted, output.write(input))
            assertEquals(accepted, input.position())
            assertEquals(if (unsigned) 1 else 0, input.remaining())
            assertEquals(if (unsigned) 4 else 2, recorded.received.size)
            assertEquals(if (unsigned) 1L else 2L, output.latencyFrames)
            assertEquals(0L, output.playedFrames)
            assertTrue(org.junit.Assert.assertThrows(DacException::class.java) { output.write(input) } === failure)
            assertTrue(org.junit.Assert.assertThrows(DacException::class.java) { output.flush() } === failure)
            assertEquals(1, attempts)
            output.close()
            assertTrue(recorded.closed)
        }
    }

    // PcmOutput.write: the return value equals the position delta, and Media3's
    // AudioSink.handleBuffer must report consumed bytes. The converter consumed source bytes
    // before the target failed, so the count is returned and the failure is rethrown later.
    @Test fun dsdPcmReturnsConsumedSourceBeforeTargetFailure() {
        assumeTrue("host conversion gate requires library loading", System.getenv("USBDAC_TEST_DSD_PCM") == "1")
        val target = FailsAfterPartialWrite(RecordingOutput(), acceptBytes = 4)
        val output = DsdPcmOutput(target, 2_822_400, 2, 176_400, 2, PcmLayout.PCM_16)
        try {
            val input = ByteBuffer.allocateDirect(8192 * 2).apply { repeat(capacity()) { put(0x69.toByte()) }; flip() }
            val accepted = output.write(input)
            assertTrue(accepted > 0)
            assertEquals(accepted, input.position())
            assertEquals(4, target.recorded.received.size)
            assertTrue(org.junit.Assert.assertThrows(DacException::class.java) { output.write(input) } === target.failure)
            assertTrue(org.junit.Assert.assertThrows(DacException::class.java) { output.writableBytes } === target.failure)
            assertTrue(org.junit.Assert.assertThrows(DacException::class.java) { output.drain() } === target.failure)
            assertEquals(accepted, input.position())
            assertEquals(1, target.attempts)
        } finally { output.close() }
        assertTrue(target.recorded.closed)
    }

    @Test fun productionConverterCanTargetFloat32Wire() {
        assumeTrue("host conversion gate requires library loading", System.getenv("USBDAC_TEST_DSD_PCM") == "1")
        val target = StreamFormat(
            formatId = 1, encoding = StreamEncoding.PCM_FLOAT32, dsdBitRate = null,
            sampleRate = 48_000, channels = 2, validBits = 32, subslotBytes = 4,
            bytesPerFrame = 8, counterRateHz = 48_000, interfaceNumber = 1, altSetting = 1,
            endpointAddress = 1, syncType = SyncType.SYNCHRONOUS, explicitFeedback = false,
            linkSpeed = LinkSpeed.HIGH, serviceIntervalUs = 125, rateDiffersFromRequest = false,
            pacing = Pacing.Synchronous,
        )
        val plan = ConversionPlan.between(StreamRequest.pcm(48_000, 2, 16), target, ConversionOptions(headroomDb = 0.0))
        val sink = RecordingOutput()
        val output = ConvertingOutput(sink, PcmConverter.create(plan))
        assertEquals(4, output.write(ByteBuffer.wrap(byteArrayOf(0, 0, 0, 64))))
        val values = ByteBuffer.wrap(sink.received.toByteArray()).order(java.nio.ByteOrder.LITTLE_ENDIAN)
        assertEquals(0.0f, values.float, 0.00001f)
        assertEquals(0.5f, values.float, 0.00001f)
        output.close()
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
            val policy = OutputPolicy(force = OutputKind.ANDROID_STANDARD)
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
