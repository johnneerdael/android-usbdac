package dev.usbdac

import dev.usbdac.internal.deviceInfoJson
import dev.usbdac.internal.parseCapabilities
import dev.usbdac.internal.parseStats
import dev.usbdac.internal.parseStreamFormat
import dev.usbdac.internal.parseVolumeRange
import dev.usbdac.internal.parseVolumeState
import dev.usbdac.internal.toJson
import dev.usbdac.internal.toNativeJson
import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

class ModelParsingTest {
    @Test
    fun capabilitiesParseFromTheDocumentedSchema() {
        val capabilities = parseCapabilities(Fixtures.CAPABILITIES)
        assertEquals(UacVersion.V2, capabilities.uacVersion)
        assertEquals(LinkSpeed.HIGH, capabilities.linkSpeed)
        assertEquals(listOf(44100, 48000, 96000), capabilities.pcmSampleRates)
        assertEquals(listOf(16, 24, 32), capabilities.pcmBitDepths)
        assertTrue(capabilities.supportsDop)
        assertEquals(11_289_600, capabilities.dopMaxDsdBitRate)
        assertFalse(capabilities.supportsNativeDsd)
        assertTrue(capabilities.hasExplicitFeedback)
        assertEquals(VolumeRange(-127.0, 0.0, 1.0), capabilities.volume)
        assertTrue(capabilities.hasMute)

        val alt = capabilities.altSettings.single()
        assertEquals(AltFormat.PCM, alt.format)
        assertEquals(SyncType.ASYNCHRONOUS, alt.syncType)
        assertEquals(129, alt.feedbackEndpoint)
        assertEquals(listOf(RateRange(44100, 44100, 0)), alt.rateRanges)
        assertEquals(4, alt.subslotBytes)
        assertEquals(32, alt.validBits)
        assertEquals(125, alt.serviceIntervalUs)
    }

    @Test
    fun nullsInCapabilitiesBecomeNulls() {
        val capabilities = parseCapabilities(Fixtures.CAPABILITIES_NO_VOLUME)
        assertNull(capabilities.volume)
        assertNull(capabilities.dopMaxDsdBitRate)
        assertEquals(UacVersion.V1, capabilities.uacVersion)
        assertEquals(listOf(2_822_400, 5_644_800), capabilities.nativeDsdBitRates)
        val alt = capabilities.altSettings.single()
        assertEquals(AltFormat.DSD, alt.format)
        assertEquals(SyncType.ADAPTIVE, alt.syncType)
        assertNull(alt.feedbackEndpoint)
        assertTrue(alt.rateRanges.isEmpty())
    }

    @Test
    fun aNegotiatedPcmFormatParses() {
        val format = parseStreamFormat(Fixtures.FORMAT)
        assertEquals(7L, format.formatId)
        assertEquals(StreamEncoding.PCM, format.encoding)
        assertNull(format.dsdBitRate)
        assertEquals(44100, format.sampleRate)
        assertEquals(2, format.channels)
        assertEquals(16, format.validBits)
        assertEquals(2, format.subslotBytes)
        assertEquals(4, format.bytesPerFrame)
        assertEquals(44100, format.counterRateHz)
        assertTrue(format.explicitFeedback)
        assertFalse(format.rateDiffersFromRequest)
    }

    @Test
    fun aNativeDsdFormatCarriesItsBitRateAndCounterRate() {
        val format = parseStreamFormat(Fixtures.FORMAT_NATIVE_DSD)
        assertEquals(StreamEncoding.DSD_NATIVE, format.encoding)
        assertEquals(2_822_400, format.dsdBitRate)
        assertEquals(352_800, format.counterRateHz)
        assertEquals(8, format.bytesPerFrame)
        assertTrue(format.rateDiffersFromRequest)
        assertEquals(LinkSpeed.FULL, format.linkSpeed)
    }

    @Test
    fun statsParseIncludingTheNullFeedbackFieldsAndTheBlocker() {
        val stats = parseStats(Fixtures.STATS)
        assertEquals(StreamState.PLAYING, stats.state)
        assertFalse(stats.bitPerfect)
        assertEquals(BitPerfectBlocker.PAYLOAD_UNVERIFIED, stats.bitPerfectBlocker)
        assertEquals(1.0f, stats.softwareGain, 0f)
        assertEquals(2L, stats.underruns)
        assertEquals(441_000L, stats.framesWritten)
        assertEquals(249.5, stats.latencyMs, 0.0)
        assertEquals(204L, stats.bufferMs)
        assertEquals(8, stats.transferQueueDepth)
        assertEquals(44100.2, stats.effectiveRateHz, 0.0)
        assertNull(stats.feedbackAgeMs)
        assertNull(stats.feedbackRateHz)
    }

    @Test
    fun statsFromABuildWithoutABlockerStillParse() {
        val json = JSONObject(Fixtures.STATS).apply { remove("bit_perfect_blocker") }.toString()
        assertNull(parseStats(json).bitPerfectBlocker)
    }

    @Test
    fun anUnknownBlockerIsUnknownNotAnError() {
        val json = JSONObject(Fixtures.STATS).put("bit_perfect_blocker", "something_new").toString()
        assertEquals(BitPerfectBlocker.UNKNOWN, parseStats(json).bitPerfectBlocker)
    }

    @Test
    fun statsAndCapabilitiesToleratePartialDocuments() {
        val stats = parseStats("""{ "state": 2, "bit_perfect": true }""")
        assertEquals(StreamState.PAUSED, stats.state)
        assertTrue(stats.bitPerfect)
        assertEquals(0L, stats.underruns)
        assertNull(stats.feedbackAgeMs)
        val capabilities = parseCapabilities("""{ "uac_version": "v2" }""")
        assertEquals(UacVersion.V2, capabilities.uacVersion)
        assertTrue(capabilities.altSettings.isEmpty())
        assertFalse(capabilities.supportsPcm)
    }

    @Test
    fun anUnreadableNegotiatedFormatIsADacExceptionNotAJsonException() {
        try {
            nativeCall { parseStreamFormat("""{ "format_id": 1 }""") }
            org.junit.Assert.fail("a format without its fields must not parse")
        } catch (error: DacException) {
            assertEquals(DacErrorCode.UNKNOWN, error.code)
        }
    }

    @Test
    fun volumeDocumentsParse() {
        assertEquals(VolumeRange(-127.0, 0.0, 1.0), parseVolumeRange(Fixtures.VOLUME_RANGE))
        assertEquals(VolumeState(-40.0, 0.5, false), parseVolumeState(Fixtures.VOLUME_STATE))
        assertNull(parseVolumeState(Fixtures.VOLUME_STATE_NO_MUTE).muted)
    }

    @Test
    fun aPcmRequestEncodesLikeTheDocumentedSchema() {
        val json = JSONObject(StreamRequest.pcm(96000, 2, 24).toJson())
        assertEquals("pcm", json.getString("encoding"))
        assertEquals(96000, json.getInt("sample_rate"))
        assertEquals(2, json.getInt("channels"))
        assertEquals(24, json.getInt("bit_depth"))
        assertEquals("exact", json.getString("rate_policy"))
    }

    @Test
    fun dsdRequestsCarryTheBitRateAndTheRatePolicy() {
        val dop = JSONObject(StreamRequest.dop(2_822_400, 2).toJson())
        assertEquals("dop", dop.getString("encoding"))
        assertEquals(2_822_400, dop.getInt("dsd_bit_rate"))
        assertFalse(dop.has("sample_rate"))

        val native = JSONObject(StreamRequest.dsdNative(5_644_800, 2, RatePolicy.NEAREST_SUPPORTED).toJson())
        assertEquals("dsd_native", native.getString("encoding"))
        assertEquals("nearest", native.getString("rate_policy"))
    }

    @Test
    fun defaultsGoToTheNativeConfigAsTheDocumentedDefaults() {
        val json = JSONObject(DacConfig().toNativeJson())
        assertEquals(250, json.getInt("buffer_target_ms"))
        assertEquals(500, json.getInt("buffer_capacity_ms"))
        assertEquals(100, json.getInt("start_threshold_ms"))
        assertEquals(100, json.getInt("processing_buffer_target_ms"))
        assertTrue(json.getBoolean("require_verified_rate"))
        assertTrue(json.getBoolean("realtime_priority"))
        assertFalse(json.getBoolean("allow_unsynchronized_async"))
        assertEquals(16, json.getInt("bus_queue_ms"))
        assertEquals(8, json.getInt("transfers_in_flight"))
        // Unset options are left out, so the native side applies the device quirk.
        for (key in listOf("clock_settle_delay_ms", "dsd_big_endian", "dsd_subslot", "padded_24bit")) {
            assertFalse(key, json.has(key))
        }
        assertEquals(0, json.getJSONArray("quirks").length())
    }

    @Test
    fun theSessionAppliesHandBackItselfSoNativeAlwaysKeeps() {
        for (policy in HandBack.entries) {
            assertEquals("keep", JSONObject(DacConfig(handBack = policy).toNativeJson()).getString("hand_back"))
        }
    }

    @Test
    fun everyKnobReachesTheNativeConfig() {
        val config = DacConfig(
            bufferTargetMs = 200,
            bufferCapacityMs = 400,
            startThresholdMs = 50,
            processingBufferTargetMs = 80,
            requireVerifiedRate = false,
            clockSettleDelayMs = 60,
            dsdBigEndian = true,
            dsdSubslot = 2,
            padded24Bit = true,
            allowUnsynchronizedAsync = true,
            realtimePriority = false,
            busQueueMs = 32,
            transfersInFlight = 4,
            quirks = listOf(
                QuirkOverride(
                    vendorId = 1,
                    productId = 2,
                    nameContains = "Foo",
                    quirks = listOf(
                        DacQuirk.SkipClockValidation,
                        DacQuirk.SettleDelayMs(80),
                        DacQuirk.DsdSubslotSize(4),
                        DacQuirk.DsdBitReverse,
                    ),
                ),
            ),
        )
        val json = JSONObject(config.toNativeJson())
        assertEquals(200, json.getInt("buffer_target_ms"))
        assertEquals(400, json.getInt("buffer_capacity_ms"))
        assertEquals(50, json.getInt("start_threshold_ms"))
        assertEquals(80, json.getInt("processing_buffer_target_ms"))
        assertFalse(json.getBoolean("require_verified_rate"))
        assertEquals(60, json.getInt("clock_settle_delay_ms"))
        assertTrue(json.getBoolean("dsd_big_endian"))
        assertEquals(2, json.getInt("dsd_subslot"))
        assertTrue(json.getBoolean("padded_24bit"))
        assertTrue(json.getBoolean("allow_unsynchronized_async"))
        assertFalse(json.getBoolean("realtime_priority"))
        assertEquals(32, json.getInt("bus_queue_ms"))
        assertEquals(4, json.getInt("transfers_in_flight"))

        val entry = json.getJSONArray("quirks").getJSONObject(0)
        assertEquals(1, entry.getInt("vendor_id"))
        assertEquals(2, entry.getInt("product_id"))
        assertEquals("Foo", entry.getString("name_contains"))
        val quirks = entry.getJSONArray("quirks")
        assertEquals("skip_clock_validation", quirks.getString(0))
        assertEquals(80, quirks.getJSONObject(1).getInt("settle_delay_ms"))
        assertEquals(4, quirks.getJSONObject(2).getInt("dsd_subslot_size"))
        assertEquals("dsd_bit_reverse", quirks.getString(3))
    }

    @Test
    fun deviceInfoCarriesTheClaimedInterfaces() {
        val json = JSONObject(deviceInfoJson(12230, 61546, "MOONDROP Dawn Pro", null, "/dev/bus/usb/001/004", listOf(0, 1, 2)))
        assertEquals(12230, json.getInt("vendor_id"))
        assertEquals(61546, json.getInt("product_id"))
        assertEquals("MOONDROP Dawn Pro", json.getString("product_name"))
        assertFalse(json.has("manufacturer_name"))
        assertEquals(3, json.getJSONArray("claimed_interfaces").length())
    }
}
