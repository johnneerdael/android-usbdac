package dev.usbdac

import dev.usbdac.internal.AAudioBackend
import dev.usbdac.internal.NativeListener
import kotlinx.coroutines.ExperimentalCoroutinesApi
import kotlinx.coroutines.launch
import kotlinx.coroutines.test.UnconfinedTestDispatcher
import kotlinx.coroutines.test.runTest
import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Assert.fail
import org.junit.Test
import java.nio.ByteBuffer

private const val EXCLUSIVE_REASON =
    "Exclusive mode was granted at the requested rate, format and channel count: the app mixer is " +
        "bypassed, but the device's audio HAL is not verifiable from an app."

private fun grantJson(
    sharing: String = "exclusive",
    rate: Int = 96_000,
    format: String? = "i24",
    verdict: String = "exclusive",
    asRequested: Boolean = true,
    reason: String = EXCLUSIVE_REASON,
    hardware: String = "null",
): String =
    """
    { "sharing": "$sharing", "sample_rate": $rate, "channels": 2,
      "format": ${if (format == null) "null" else "\"$format\""}, "format_raw": 3,
      "frames_per_burst": 96, "buffer_capacity_frames": 960, "buffer_size_frames": 192,
      "device_id": 7, "performance_mode": "low_latency", "mmap": true, "hardware": $hardware,
      "as_requested": $asRequested, "verdict": "$verdict", "reason": "$reason" }
    """.trimIndent()

/** The native layer, faked: records every call and answers from fields the test sets. */
private class FakeAAudioBackend(var grant: String = grantJson()) : AAudioBackend {
    val calls = mutableListOf<String>()
    var requestJson: String? = null
    var apiLevel = -1
    var listener: NativeListener? = null
    var failOpen: Throwable? = null
    var failStart: Throwable? = null
    var failWrite: Throwable? = null
    var acceptLimit = Int.MAX_VALUE
    val written = ArrayList<Byte>()
    var lastWriteOffset = -1
    var lastWriteDirect = false
    var state = 0
    var writable = 8192
    var drained = false
    var played = 0L
    var latency = 0L
    var xruns = 0
    var underruns = 0L

    override fun open(requestJson: String, listener: NativeListener, apiLevel: Int): Long {
        calls += "open"
        failOpen?.let { throw it }
        this.requestJson = requestJson
        this.listener = listener
        this.apiLevel = apiLevel
        return 42
    }

    override fun grant(handle: Long): String = grant
    override fun start(handle: Long) {
        calls += "start"
        failStart?.let { throw it }
    }

    override fun close(handle: Long) {
        calls += "close"
    }

    override fun write(handle: Long, buffer: ByteBuffer, offset: Int, count: Int): Int {
        failWrite?.let { throw it }
        lastWriteOffset = offset
        lastWriteDirect = buffer.isDirect
        val taken = minOf(count, acceptLimit)
        for (i in 0 until taken) written += buffer.get(offset + i)
        return taken
    }

    override fun writableBytes(handle: Long): Int = writable
    override fun play(handle: Long) {
        calls += "play"
    }

    override fun pause(handle: Long) {
        calls += "pause"
    }

    override fun flush(handle: Long) {
        calls += "flush"
    }

    override fun drain(handle: Long) {
        calls += "drain"
    }

    override fun isDrained(handle: Long): Boolean = drained
    override fun state(handle: Long): Int = state
    override fun playedFrames(handle: Long): Long = played
    override fun latencyFrames(handle: Long): Long = latency
    override fun xruns(handle: Long): Int = xruns
    override fun underruns(handle: Long): Long = underruns
}

private val PCM24 = StreamRequest.pcm(96_000, channels = 2, bitDepth = 24)

private fun open(
    backend: FakeAAudioBackend,
    request: StreamRequest = PCM24,
    config: AAudioConfig = AAudioConfig(),
    deviceId: Int = 7,
    clock: () -> Long = { 0L },
): AAudioExclusiveOutput = AAudioExclusiveOutput.open(backend, 34, request, deviceId, config, clock)

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

@OptIn(ExperimentalCoroutinesApi::class)
class AAudioExclusiveOutputTest {
    // ---- the request ----

    @Test
    fun theRequestCarriesTheSourcesRateChannelsFormatAndTheDevice() {
        val backend = FakeAAudioBackend()
        open(backend, config = AAudioConfig(bufferTargetMs = 200, bufferCapacityMs = 400, startThresholdMs = 50, deviceBufferBursts = 4))
        val json = JSONObject(backend.requestJson!!)
        assertEquals(96_000, json.getInt("sample_rate"))
        assertEquals(2, json.getInt("channels"))
        assertEquals("i24", json.getString("format"))
        assertEquals(7, json.getInt("device_id"))
        assertEquals("low_latency", json.getString("performance_mode"))
        assertEquals(200, json.getInt("buffer_target_ms"))
        assertEquals(400, json.getInt("buffer_capacity_ms"))
        assertEquals(50, json.getInt("start_threshold_ms"))
        assertEquals(4, json.getInt("device_buffer_bursts"))
        assertEquals(34, backend.apiLevel)
    }

    @Test
    fun theDepthsMapToTheFormatsWithoutConversion() {
        for ((bits, float, format) in listOf(
            Triple(16, false, "i16"),
            Triple(24, false, "i24"),
            Triple(32, false, "i32"),
            Triple(32, true, "float"),
        )) {
            val backend = FakeAAudioBackend()
            open(backend, StreamRequest.pcm(44_100, 2, bits), AAudioConfig(floatSamples = float))
            assertEquals(format, JSONObject(backend.requestJson!!).getString("format"))
        }
    }

    @Test
    fun noDeviceBufferBurstsIsSentAsNull() {
        val backend = FakeAAudioBackend()
        open(backend)
        assertTrue(JSONObject(backend.requestJson!!).isNull("device_buffer_bursts"))
    }

    @Test
    fun floatSamplesNeed32Bits() {
        val backend = FakeAAudioBackend()
        failsWith(DacErrorCode.INVALID_ARGUMENT) { open(backend, StreamRequest.pcm(44_100, 2, 16), AAudioConfig(floatSamples = true)) }
        assertTrue(backend.calls.isEmpty())
    }

    @Test
    fun dsdAndDopAreRefusedBeforeAnyNativeCall() {
        val backend = FakeAAudioBackend()
        failsWith(DacErrorCode.UNSUPPORTED) { open(backend, StreamRequest.dop(2_822_400, 2)) }
        failsWith(DacErrorCode.UNSUPPORTED) { open(backend, StreamRequest.dsdNative(2_822_400, 2)) }
        assertTrue(backend.calls.isEmpty())
    }

    @Test
    fun anUnsupportedDepthOrChannelCountIsRefusedBeforeAnyNativeCall() {
        val backend = FakeAAudioBackend()
        failsWith(DacErrorCode.UNSUPPORTED) { open(backend, StreamRequest.pcm(44_100, 2, 8)) }
        failsWith(DacErrorCode.UNSUPPORTED) { open(backend, StreamRequest.pcm(44_100, 9, 16)) }
        failsWith(DacErrorCode.UNSUPPORTED) { open(backend, StreamRequest.pcm(44_100, 0, 16)) }
        assertTrue(backend.calls.isEmpty())
    }

    @Test
    fun aNativeFailureIsATypedException() {
        val backend = FakeAAudioBackend().apply {
            failOpen = IllegalStateException("usbdac:unsupported: packed 24-bit samples through AAudio need Android 12 (API 31); this is API 30")
        }
        val error = failsWith(DacErrorCode.UNSUPPORTED) { open(backend) }
        assertTrue(error.detail, error.detail.contains("API 31"))
        assertEquals("the handle never existed, so there is nothing to close", listOf("open"), backend.calls)
    }

    @Test
    fun aBusyDeviceIsTyped() {
        val backend = FakeAAudioBackend().apply { failOpen = IllegalStateException("usbdac:busy: AAudio failed opening the stream: UNAVAILABLE (-889)") }
        failsWith(DacErrorCode.BUSY) { open(backend) }
    }

    // ---- the grant ----

    @Test
    fun anExclusiveGrantAsRequestedIsStartedAndNotVerifiedWithThePreciseReason() {
        val backend = FakeAAudioBackend()
        val output = open(backend)
        assertEquals(listOf("open", "start"), backend.calls)
        assertTrue(output.grant.isExclusive)
        assertEquals(AAudioSharingMode.EXCLUSIVE, output.grant.sharingMode)
        assertEquals(96_000, output.grant.sampleRate)
        assertEquals(AAudioSampleFormat.PCM_24_PACKED, output.grant.format)
        assertEquals(96, output.grant.framesPerBurst)
        assertEquals(960, output.grant.bufferCapacityFrames)
        assertEquals(192, output.grant.bufferSizeFrames)
        assertEquals(7, output.grant.deviceId)
        assertEquals(AAudioPerformanceMode.LOW_LATENCY, output.grant.performanceMode)
        assertEquals(true, output.grant.usesMmap)
        assertNull(output.grant.hardware)
        assertTrue(output.grant.asRequested)
        val state = output.bitPerfect.value
        assertFalse("never Verified", state.isVerified)
        assertEquals(BitPerfectState.NotVerified(null, EXCLUSIVE_REASON), state)
    }

    @Test
    fun aSharedGrantFailsOpenByDefaultAndReleasesTheStreamWithoutStartingIt() {
        val reason = "The platform granted shared mode, not exclusive: the audio goes through the system mixer."
        val backend = FakeAAudioBackend(grantJson(sharing = "shared", verdict = "shared", asRequested = false, reason = reason))
        val error = failsWith(DacErrorCode.EXCLUSIVE_UNAVAILABLE) { open(backend) }
        assertTrue(error.detail, error.detail.contains("system mixer"))
        assertEquals(listOf("open", "close"), backend.calls)
    }

    @Test
    fun withRequireExclusiveOffASharedGrantOpensAndSaysItIsShared() {
        val reason = "The platform granted shared mode, not exclusive: the audio goes through the system mixer."
        val backend = FakeAAudioBackend(grantJson(sharing = "shared", verdict = "shared", asRequested = false, reason = reason))
        val output = open(backend, config = AAudioConfig(requireExclusive = false))
        assertEquals(listOf("open", "start"), backend.calls)
        assertFalse(output.grant.isExclusive)
        assertEquals("shared", output.grant.verdict)
        assertEquals(BitPerfectState.NotVerified(null, reason), output.bitPerfect.value)
        assertFalse(output.bitPerfect.value.isVerified)
    }

    @Test
    fun theHardwareReportOfAndroid14IsParsedAndAConversionIsNamed() {
        val reason = "The hardware takes 16-bit integer while the stream carries packed 24-bit integer: AAudio converts the samples before the device."
        val backend = FakeAAudioBackend(
            grantJson(
                verdict = "hardware_format",
                asRequested = false,
                reason = reason,
                hardware = """{ "sample_rate": 96000, "channels": 2, "format": "i16", "format_raw": 1 }""",
            ),
        )
        val output = open(backend)
        assertEquals(AAudioHardware(96_000, 2, AAudioSampleFormat.PCM_16), output.grant.hardware)
        assertFalse(output.grant.asRequested)
        assertEquals(BitPerfectState.NotVerified(null, reason), output.bitPerfect.value)
    }

    @Test
    fun aFormatThisLibraryDoesNotCarryReadsAsNull() {
        val backend = FakeAAudioBackend(grantJson(format = null))
        assertNull(open(backend).grant.format)
    }

    @Test
    fun aFailureWhileStartingReleasesTheStream() {
        val backend = FakeAAudioBackend().apply { failStart = IllegalStateException("usbdac:invalid_state: AAudio failed starting the stream: INVALID_STATE (-895)") }
        failsWith(DacErrorCode.INVALID_STATE) { open(backend) }
        assertEquals(listOf("open", "start", "close"), backend.calls)
    }

    @Test
    fun aGrantTheLibraryCannotReadIsATypedFailureAndReleasesTheStream() {
        val backend = FakeAAudioBackend("""{ "sharing": "exclusive" }""")
        failsWith(DacErrorCode.UNKNOWN) { open(backend) }
        assertEquals(listOf("open", "close"), backend.calls)
    }

    // ---- writing ----

    @Test
    fun aDirectBufferIsReadInPlaceAndItsPositionAdvancesByWhatWasTaken() {
        val backend = FakeAAudioBackend()
        val output = open(backend)
        val buffer = ByteBuffer.allocateDirect(12)
        for (i in 0 until 12) buffer.put(i.toByte())
        buffer.flip()
        buffer.position(2)
        backend.acceptLimit = 6
        assertEquals(6, output.write(buffer))
        assertEquals(8, buffer.position())
        assertEquals(2, backend.lastWriteOffset)
        assertTrue(backend.lastWriteDirect)
        assertEquals(listOf<Byte>(2, 3, 4, 5, 6, 7), backend.written)
    }

    @Test
    fun aHeapBufferIsStagedAndStopsAtTheFirstShortWrite() {
        val backend = FakeAAudioBackend()
        val output = open(backend)
        val buffer = ByteBuffer.wrap(ByteArray(10) { it.toByte() })
        backend.acceptLimit = 4
        assertEquals(4, output.write(buffer))
        assertEquals(4, buffer.position())
        assertTrue("copied through a direct staging buffer", backend.lastWriteDirect)
        assertEquals(0, backend.lastWriteOffset)
        backend.acceptLimit = Int.MAX_VALUE
        assertEquals(6, output.write(buffer))
        assertFalse(buffer.hasRemaining())
        assertEquals((0..9).map { it.toByte() }, backend.written)
    }

    @Test
    fun anEmptyBufferIsZeroWithoutANativeCall() {
        val backend = FakeAAudioBackend()
        val output = open(backend)
        assertEquals(0, output.write(ByteBuffer.allocate(0)))
        assertEquals(-1, backend.lastWriteOffset)
    }

    @Test
    fun aWriteOnADisconnectedRouteIsDeviceLost() {
        val backend = FakeAAudioBackend()
        val output = open(backend)
        backend.failWrite = IllegalStateException("usbdac:device_lost: the audio route was disconnected or failed; close the output")
        failsWith(DacErrorCode.DEVICE_LOST) { output.write(ByteBuffer.allocateDirect(8).apply { limit(8) }) }
    }

    @Test
    fun writableBytesComesFromTheNativeLayer() {
        val backend = FakeAAudioBackend()
        val output = open(backend)
        backend.writable = 4410
        assertEquals(4410, output.writableBytes)
    }

    // ---- controls and position ----

    @Test
    fun theControlsReachTheNativeLayer() {
        val backend = FakeAAudioBackend()
        val output = open(backend)
        output.play()
        output.pause()
        output.flush()
        output.drain()
        assertEquals(listOf("open", "start", "play", "pause", "flush", "drain"), backend.calls)
        backend.drained = true
        backend.played = 96_000
        backend.latency = 24_000
        assertTrue(output.isDrained)
        assertEquals(96_000L, output.playedFrames)
        assertEquals(24_000L, output.latencyFrames)
    }

    @Test
    fun theStateFollowsTheNativeState() = runTest {
        val backend = FakeAAudioBackend()
        val output = open(backend)
        val seen = ArrayList<StreamState>()
        val dispatcher = UnconfinedTestDispatcher(testScheduler)
        backgroundScope.launch(dispatcher) { output.events.collect { if (it is AAudioEvent.StateChanged) seen += it.state } }
        assertEquals(StreamState.BUFFERING, output.state.value)
        backend.state = 1
        output.play()
        assertEquals(StreamState.PLAYING, output.state.value)
        backend.state = 2
        output.pause()
        assertEquals(StreamState.PAUSED, output.state.value)
        assertEquals(listOf(StreamState.PLAYING, StreamState.PAUSED), seen)
    }

    // ---- disconnection and counters ----

    @Test
    fun aDisconnectFailsTheOutputTellsTheListenerAndKeepsNoClaimOnBitPerfect() = runTest {
        val backend = FakeAAudioBackend()
        val output = open(backend)
        val events = ArrayList<AAudioEvent>()
        backgroundScope.launch(UnconfinedTestDispatcher(testScheduler)) { output.events.collect { events += it } }
        backend.listener!!.onEvent(0, -899, 0.0, "the audio route was disconnected")
        assertEquals(StreamState.FAILED, output.state.value)
        assertTrue(events.contains(AAudioEvent.Disconnected("the audio route was disconnected")))
        assertTrue(events.contains(AAudioEvent.StateChanged(StreamState.FAILED)))
        val state = output.bitPerfect.value as BitPerfectState.NotVerified
        assertTrue(state.reason, state.reason.contains("disconnected"))
        assertTrue("what was known before is kept", state.reason.contains(EXCLUSIVE_REASON))
        assertFalse(state.isVerified)
        // A later poll does not bring a failed output back.
        backend.state = 1
        output.play()
        assertEquals(StreamState.FAILED, output.state.value)
    }

    @Test
    fun anotherPlatformErrorIsAFailureEvent() = runTest {
        val backend = FakeAAudioBackend()
        val output = open(backend)
        val events = ArrayList<AAudioEvent>()
        backgroundScope.launch(UnconfinedTestDispatcher(testScheduler)) { output.events.collect { events += it } }
        backend.listener!!.onEvent(1, -896, 0.0, "AAudio failed while playing: INTERNAL (-896)")
        assertEquals(StreamState.FAILED, output.state.value)
        assertTrue(events.any { it is AAudioEvent.Failed })
    }

    @Test
    fun underrunsAndXrunsAreAnnouncedWhenTheyGrowAndNotRepeated() = runTest {
        var now = 1_000_000_000L
        val backend = FakeAAudioBackend()
        val output = open(backend, clock = { now })
        val events = ArrayList<AAudioEvent>()
        backgroundScope.launch(UnconfinedTestDispatcher(testScheduler)) { output.events.collect { events += it } }
        val chunk = ByteBuffer.allocateDirect(8).apply { limit(8) }

        backend.xruns = 2
        backend.underruns = 1
        output.write(chunk.duplicate())
        assertEquals(listOf(AAudioEvent.Xrun(2), AAudioEvent.Underrun(1)), events.filter { it !is AAudioEvent.StateChanged })
        assertEquals(2, output.xrunCount)
        assertEquals(1L, output.underrunCount)

        output.write(chunk.duplicate()) // same counters, and inside the poll interval
        now += 100_000_000
        output.write(chunk.duplicate()) // outside the interval but nothing grew
        assertEquals(2, events.filter { it !is AAudioEvent.StateChanged }.size)

        backend.underruns = 4
        now += 100_000_000
        output.write(chunk.duplicate())
        assertEquals(AAudioEvent.Underrun(4), events.last { it !is AAudioEvent.StateChanged })
    }

    // ---- closing ----

    @Test
    fun closeReleasesOnceAndTheOutputThenRefusesWrites() {
        val backend = FakeAAudioBackend()
        val output = open(backend)
        output.close()
        output.close()
        assertEquals(1, backend.calls.count { it == "close" })
        assertEquals(StreamState.CLOSED, output.state.value)
        failsWith(DacErrorCode.INVALID_STATE) { output.write(ByteBuffer.allocateDirect(8).apply { limit(8) }) }
        failsWith(DacErrorCode.INVALID_STATE) { output.play() }
        assertEquals(0, output.writableBytes)
        assertEquals(0L, output.playedFrames)
        assertEquals(0L, output.latencyFrames)
        assertFalse(output.isDrained)
        assertEquals(0, output.xrunCount)
    }

    @Test
    fun theOutputIsNeverReportedVerifiedWhateverTheGrant() {
        for (grant in listOf(
            grantJson(),
            grantJson(sharing = "shared", verdict = "shared", asRequested = false, reason = "shared"),
            grantJson(verdict = "hardware_rate", asRequested = false, reason = "converted"),
        )) {
            val output = open(FakeAAudioBackend(grant), config = AAudioConfig(requireExclusive = false))
            assertFalse(output.bitPerfect.value.isVerified)
            assertTrue(output.bitPerfect.value is BitPerfectState.NotVerified)
        }
    }

    @Test
    fun theNewErrorCodeParsesFromTheNativeName() {
        assertEquals(DacErrorCode.EXCLUSIVE_UNAVAILABLE, DacErrorCode.fromWireName("exclusive_unavailable"))
    }

    // ---- the probe: what the platform grants, without a sound ----

    @Test
    fun aProbeOpensAndReadsTheGrantButDoesNotStartTheStream() {
        val backend = FakeAAudioBackend()
        val probe = AAudioExclusiveOutput.probe(backend, 34, PCM24, 0, AAudioConfig())
        assertTrue(probe.grant.isExclusive)
        assertEquals(listOf("open"), backend.calls)
        probe.close()
        assertEquals(listOf("open", "close"), backend.calls)
        probe.close()
        assertEquals("closing twice releases once", 1, backend.calls.count { it == "close" })
    }

    @Test
    fun aProbeThatIsStartedIsTheOutputAndClosingTheProbeAfterwardsDoesNothing() {
        val backend = FakeAAudioBackend()
        val probe = AAudioExclusiveOutput.probe(backend, 34, PCM24, 0, AAudioConfig())
        val output = probe.start()
        assertEquals(listOf("open", "start"), backend.calls)
        assertEquals(probe.grant, output.grant)
        assertTrue(output.bitPerfect.value is BitPerfectState.NotVerified)
        probe.close()
        assertEquals("the output owns the stream now", 0, backend.calls.count { it == "close" })
        output.close()
        assertEquals(1, backend.calls.count { it == "close" })
    }

    @Test
    fun aProbeReportsASharedGrantInsteadOfRefusingIt() {
        val backend = FakeAAudioBackend(grantJson(sharing = "shared", verdict = "shared", asRequested = false, reason = "shared"))
        val probe = AAudioExclusiveOutput.probe(backend, 34, PCM24, 0, AAudioConfig())
        assertFalse(probe.grant.isExclusive)
        probe.close()
    }

    @Test
    fun aProbeRefusesWhatOpenRefusesBeforeAnyNativeCall() {
        val backend = FakeAAudioBackend()
        failsWith(DacErrorCode.UNSUPPORTED) { AAudioExclusiveOutput.probe(backend, 34, StreamRequest.dop(2_822_400, 2), 0, AAudioConfig()) }
        assertTrue(backend.calls.isEmpty())
    }

    @Test
    fun aProbeThatFailsToStartCanStillBeClosed() {
        val backend = FakeAAudioBackend().apply { failStart = IllegalStateException("usbdac:transfer: could not start") }
        val probe = AAudioExclusiveOutput.probe(backend, 34, PCM24, 0, AAudioConfig())
        failsWith(DacErrorCode.TRANSFER) { probe.start() }
        probe.close()
        assertEquals(1, backend.calls.count { it == "close" })
    }
}
