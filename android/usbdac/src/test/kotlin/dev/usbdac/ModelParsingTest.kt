package dev.usbdac

import dev.usbdac.internal.deviceInfoJson
import dev.usbdac.internal.parseCapabilities
import dev.usbdac.internal.parseStartupReport
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
    fun pioneerDjmProfilesPreserveSparseChoicesWithoutInferringState() {
        val owner = PioneerDjmRouting(1L) {}
        for (model in PioneerDjmModel.entries) {
            val profile = parsePioneerDjmProfile(owner, JSONObject("""{
                "model":"${model.wireName}","configuration_id":1,"controls":[
                    {"key":{"kind":"capture_level"},"options":[{"id":2,"label":"-10 dB"}]},
                    {"key":{"kind":"capture_source","pair":1},"options":[{"id":7,"label":"CH1 Post Fader"}]},
                    {"key":{"kind":"playback_destination","pair":5},"options":[{"id":3,"label":"AUX"}]}
                ]}"""))
            assertEquals(model, profile.model)
            assertEquals(1, profile.configurationId)
            assertEquals(PioneerDjmControlKey.CaptureLevel, profile.controls[0].key)
            assertEquals(PioneerDjmControlKey.CaptureSource(1), profile.controls[1].key)
            assertEquals(PioneerDjmControlKey.PlaybackDestination(5), profile.controls[2].key)
            assertEquals(listOf(PioneerDjmOption(7, "CH1 Post Fader")), profile.controls[1].options)
        }
        assertEquals(PioneerDjmRouting::class.java, DacRouting::class.java.getMethod("getPioneer").returnType)
        assertEquals(PioneerDjmProfile::class.java, PioneerDjmRouting::class.java.getMethod("discover").returnType)
        assertEquals(Void.TYPE, PioneerDjmRouting::class.java.getMethod("apply", PioneerDjmProfile::class.java,
            PioneerDjmControlKey::class.java, PioneerDjmOption::class.java).returnType)
        assertEquals(String::class.java, dev.usbdac.internal.Native::class.java.getMethod("pioneerDjmDiscover",
            Long::class.javaPrimitiveType).returnType)
        assertEquals(String::class.java, dev.usbdac.internal.Native::class.java.getMethod("pioneerDjmApply",
            Long::class.javaPrimitiveType, String::class.java).returnType)
    }

    @Test
    fun pioneerDjmRejectsForeignProfilesAbsentControlsAndUnadvertisedOptionsBeforeNative() {
        val owner = PioneerDjmRouting(1L) {}
        val foreign = PioneerDjmRouting(2L) {}
        val key = PioneerDjmControlKey.CaptureSource(1)
        val option = PioneerDjmOption(7, "CH1 Post Fader")
        val profile = PioneerDjmProfile(owner, PioneerDjmModel.DJM_450, 1,
            listOf(PioneerDjmControl(key, listOf(option))))
        for (invalid in listOf<() -> Any>(
            { foreign.apply(profile, key, option) },
            { owner.apply(profile, PioneerDjmControlKey.CaptureSource(2), option) },
            { owner.apply(profile, key, PioneerDjmOption(8, "Other")) },
            { owner.apply(profile, key, option.copy(label = "Invented")) },
            { PioneerDjmControlKey.CaptureSource(0) },
            { PioneerDjmControlKey.PlaybackDestination(256) },
            { PioneerDjmOption(-1, "Invalid") },
            { parsePioneerDjmKey(JSONObject("""{"kind":"capture_level","pair":0}""")) },
            { parsePioneerDjmKey(JSONObject("""{"kind":"selector","pair":1}""")) },
        )) {
            assertTrue(runCatching { invalid() }.exceptionOrNull() is IllegalArgumentException)
        }
    }

    @Test
    fun gen1ReadbackPreservesModelSignedSourcesRawGainAndTypedEntryPoints() {
        for (model in Gen1Model.entries) {
            val v = parseGen1Readback(JSONObject().put("model", model.name.lowercase())
                .put("source_offsets", org.json.JSONArray(listOf(0, 4, 6, 6, 6)))
                .put("matrix_sources", 7).put("output_sources", 0)
                .put("controls", org.json.JSONArray("""[{"key":{"kind":"matrix_input","index":1},"value":-1},
                    {"key":{"kind":"gain","input":1,"mix":4},"value":-32767}]""")))
            assertEquals(model, v.model)
            assertEquals(Gen1Control(Gen1Key.MatrixInput(1), -1), v.controls[0])
            assertEquals(Gen1Control(Gen1Key.Gain(1, 4), -32767), v.controls[1])
            assertEquals(listOf(0, 4, 6, 6, 6), v.sourceOffsets)
        }
        assertEquals(Gen1Readback::class.java, ScarlettRouting::class.java.getMethod("readGen1").returnType)
        assertEquals(Gen1Snapshot::class.java, ScarlettRouting::class.java.getMethod("readGen1Snapshot").returnType)
        assertEquals(Gen1Report::class.java, ScarlettRouting::class.java.getMethod("applyGen1", Gen1Snapshot::class.java, List::class.java).returnType)
        for (name in listOf("scarlettGen1Read", "scarlettGen1ReadSnapshot")) {
            assertEquals(String::class.java, dev.usbdac.internal.Native::class.java.getMethod(name, Long::class.javaPrimitiveType).returnType)
        }
        assertEquals(String::class.java, dev.usbdac.internal.Native::class.java.getMethod("scarlettGen1Apply",
            Long::class.javaPrimitiveType, Long::class.javaPrimitiveType, String::class.java).returnType)
    }

    @Test
    fun gen1RejectsForeignSnapshotsDuplicateControlsAndInvalidSignedEdits() {
        val owner = ScarlettRouting(1L) {}
        val foreign = ScarlettRouting(2L) {}
        val snapshot = Gen1Snapshot(owner, 99L, Gen1Readback(Gen1Model.FORTE, emptyList(), emptyList(), 7, 0))
        val edit = Gen1Edit(Gen1Key.MatrixInput(1), -1)
        assertTrue(runCatching { foreign.applyGen1(snapshot, listOf(edit)) }.exceptionOrNull() is IllegalArgumentException)
        assertTrue(runCatching { owner.applyGen1(snapshot, listOf(edit, edit)) }.exceptionOrNull() is IllegalArgumentException)
        for (invalid in listOf<() -> Any>({ Gen1Key.Output(0) }, { Gen1Key.Gain(1, 256) },
            { Gen1Edit(Gen1Key.Capture(1), -2) }, { Gen1Edit(Gen1Key.Gain(1, 1), 32768) })) {
            assertTrue(runCatching { invalid() }.exceptionOrNull() is IllegalArgumentException)
        }
        val failure = parseGen1Failure(JSONObject("""{"kind":"io","detail":"write failed","key":{"kind":"output","index":2}}"""))
        assertEquals(Gen1Key.Output(2), failure.key)
        assertEquals("write failed", failure.detail)
        assertNull(parseGen1Failure(JSONObject("""{"kind":"drift","key":null}""")).key)
    }

    @Test
    fun modernFcpModelsHaveDistinctClaimKeysAndTypedSnapshotEntryPoints() {
        for (key in listOf("scarlett_16i16_gen4", "scarlett_18i16_gen4", "scarlett_18i20_gen4", "isa_c8x")) {
            val capability = parseScarlettCapability(JSONObject()
                .put("model", key).put("name", key).put("configuration_id", 1)
                .put("interface", 4).put("notification_endpoint", 132).put("interface_claimed", false))
            assertEquals(key, capability.model.name.lowercase())
            assertEquals(capability.model.name, VendorRouting.valueOf(key.uppercase()).name)
            assertFalse(capability.interfaceClaimed)
        }
        assertEquals(FcpReadback::class.java, ScarlettRouting::class.java.getMethod("readFcp").returnType)
        assertEquals(String::class.java, dev.usbdac.internal.Native::class.java.getMethod("fcpRead", Long::class.javaPrimitiveType).returnType)
        assertEquals(String::class.java, dev.usbdac.internal.Native::class.java.getMethod("fcpReadSnapshot", Long::class.javaPrimitiveType).returnType)
        assertEquals(FcpSnapshot::class.java, ScarlettRouting::class.java.getMethod("readFcpSnapshot").returnType)
        assertEquals(FcpReport::class.java, ScarlettRouting::class.java.getMethod("applyFcp", FcpSnapshot::class.java, List::class.java, List::class.java).returnType)
        assertEquals(String::class.java, dev.usbdac.internal.Native::class.java.getMethod("fcpApply", Long::class.javaPrimitiveType, Long::class.javaPrimitiveType, String::class.java).returnType)
    }

    @Test
    fun fcpTypedBoundsAndStructuredFailurePreserveTargets() {
        assertEquals(0, FcpRouteEdit(2, 65535, 0).source)
        assertEquals(32613, FcpMixEdit(255, 255, 32613).coefficient)
        for (invalid in listOf<() -> Any>({ FcpRouteEdit(3, 1, 0) }, { FcpRouteEdit(0, 0, 1) },
            { FcpMixEdit(0, 1, 0) }, { FcpMixEdit(1, 1, 32614) })) {
            assertTrue(runCatching { invalid() }.exceptionOrNull() is IllegalArgumentException)
        }
        val failure = parseFcpFailure(JSONObject("""{"kind":"verification","target":{"kind":"mix","index":2}}"""))
        assertEquals(ScarlettFailureKind.VERIFICATION, failure.kind)
        assertEquals(FcpTarget(FcpTargetKind.MIX, 2), failure.target)
        assertNull(failure.detail)
    }

    @Test
    fun fcpSnapshotsRejectForeignOwnersBeforeNativeWrites() {
        val owner = ScarlettRouting(1L) {}
        val foreign = ScarlettRouting(2L) {}
        val readback = FcpReadback(ScarlettModel.ISA_C8X, 1L, 1, 1, emptyList(), emptyList(), emptyList(), emptyList())
        val snapshot = FcpSnapshot(owner, 99L, readback)
        assertTrue(runCatching { foreign.applyFcp(snapshot, listOf(FcpRouteEdit(0, 1, 0)), emptyList()) }
            .exceptionOrNull() is IllegalArgumentException)
        assertTrue(runCatching { owner.applyFcp(snapshot, listOf(FcpRouteEdit(0, 1, 0), FcpRouteEdit(0, 1, 2)), emptyList()) }
            .exceptionOrNull() is IllegalArgumentException)
        assertTrue(runCatching { owner.applyFcp(snapshot, emptyList(), listOf(FcpMixEdit(1, 1, 0), FcpMixEdit(1, 1, 2))) }
            .exceptionOrNull() is IllegalArgumentException)
    }

    @Test
    fun fcpReadbackPreservesFirmwarePinsFixedSourcesNamesMixesAndUnsignedWords() {
        val readback = parseFcpReadback(JSONObject("""{
            "model":"isa_c8x","firmware":4294967295,"mixer_inputs":1,"mixer_outputs":1,
            "sources":[{"pin":65535,"name":"Mic 1","fixed_source":null}],
            "destinations":[{"pin":40000,"name":"Fixed Output","fixed_source":65535}],
            "banks":[{"bank":2,"raw_words":[4294967295,2147483648,0]}],
            "mixes":[{"output":1,"coefficients":[65535]}]
        }"""))
        assertEquals(ScarlettModel.ISA_C8X, readback.model)
        assertEquals(4294967295L, readback.firmware)
        assertEquals(FcpPort(65535, "Mic 1", null), readback.sources.single())
        assertEquals(FcpPort(40000, "Fixed Output", 65535), readback.destinations.single())
        assertEquals(FcpBank(2, listOf(4294967295L, 2147483648L, 0L)), readback.banks.single())
        assertEquals(ScarlettMixRow(1, listOf(65535)), readback.mixes.single())
        assertEquals(1, readback.mixerInputs)
        assertEquals(1, readback.mixerOutputs)
    }

    @Test
    fun clarettCapabilityKeysPreserveUsbAndPlusIdentityAndOptIn() {
        for ((key, name) in listOf(
            "clarett_usb_2pre" to "Clarett USB 2Pre",
            "clarett_usb_4pre" to "Clarett USB 4Pre",
            "clarett_usb_8pre" to "Clarett USB 8Pre",
            "clarett_plus_2pre" to "Clarett+ 2Pre",
            "clarett_plus_4pre" to "Clarett+ 4Pre",
            "clarett_plus_8pre" to "Clarett+ 8Pre",
        )) {
            val capability = parseScarlettCapability(JSONObject()
                .put("model", key).put("name", name).put("configuration_id", 1)
                .put("interface", 4).put("notification_endpoint", 132)
                .put("interface_claimed", false))
            assertEquals(key, capability.model.name.lowercase())
            assertEquals(name, capability.name)
            assertEquals(capability.model.name, VendorRouting.valueOf(key.uppercase()).name)
            assertEquals(4, capability.interfaceId)
            assertFalse(capability.interfaceClaimed)
        }
        assertTrue(ScarlettModel.CLARETT_USB_2PRE != ScarlettModel.CLARETT_PLUS_2PRE)
    }

    @Test
    fun implicitPacingKeepsSourceAndLiveLockSeparate() {
        val json = JSONObject(Fixtures.FORMAT)
            .put("explicit_feedback", false)
            .put("pacing", JSONObject().put("mode", "implicit_feedback").put("implicit_feedback",
                JSONObject().put("interface_number", 2).put("alt_setting", 3).put("endpoint", 130)))
        val format = parseStreamFormat(json.toString())
        assertEquals(Pacing.ImplicitFeedback(2, 3, 130), format.pacing)
        assertFalse(format.explicitFeedback)
        assertTrue(JSONObject(DacConfig(disableFeedback = true).toNativeJson()).getBoolean("disable_feedback"))
        val claims = JSONObject(deviceInfoJson(1, 2, null, null, null, listOf(0, 1, 2), 4, listOf(3)))
        assertEquals(4, claims.getInt("configuration_id"))
        assertEquals(3, claims.getJSONArray("unavailable_feedback_interfaces").getInt(0))
    }

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
        assertTrue(capabilities.hasHardwareVolume)
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
    fun dopOnlyAlternateKeepsItsOwnEncodingAndWireStride() {
        val json = JSONObject(Fixtures.CAPABILITIES)
        json.getJSONArray("alt_settings").getJSONObject(0)
            .put("format", "dop").put("subslot_bytes", 3).put("valid_bits", 24)
        val alt = parseCapabilities(json.toString()).altSettings.single()
        assertEquals(AltFormat.DOP, alt.format)
        assertEquals(3, alt.subslotBytes)
        assertEquals(24, alt.validBits)
    }

    @Test
    fun nullsInCapabilitiesBecomeNulls() {
        val capabilities = parseCapabilities(Fixtures.CAPABILITIES_NO_VOLUME)
        assertNull(capabilities.volume)
        assertFalse(capabilities.hasHardwareVolume)
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
    fun aNegotiatedFormatCarriesTheClockPathAndThePacing() {
        val format = parseStreamFormat(Fixtures.FORMAT)
        assertEquals(Pacing.ExplicitFeedback(0x81), format.pacing)
        assertEquals("explicit feedback (endpoint 0x81)", format.pacing.description)
        val clock = format.clock!!
        assertEquals(0, clock.controlInterface)
        assertEquals(5, clock.sourceId)
        assertEquals(ClockResolution.RESOLVED, clock.resolution)
        // In the order followed: the terminal, then each clock entity down to the source.
        assertEquals(
            listOf(
                ClockHop(1, ClockHopKind.Terminal),
                ClockHop(9, ClockHopKind.Selector(selectedPin = 2, pinCount = 2)),
                ClockHop(8, ClockHopKind.Multiplier),
                ClockHop(5, ClockHopKind.Source),
            ),
            clock.path,
        )
        assertEquals(clock.sourceId, clock.path.last().id)
        assertEquals("terminal 1 > selector 9 (pin 2 of 2) > multiplier 8 > source 5, resolved", clock.description)
    }

    @Test
    fun aClockThatFellBackToTheFirstSourceSaysSo() {
        val format = parseStreamFormat(Fixtures.FORMAT_CLOCK_FALLBACK)
        assertEquals(Pacing.Adaptive, format.pacing)
        val clock = format.clock!!
        assertEquals(ClockResolution.FALLBACK_FIRST_SOURCE, clock.resolution)
        assertEquals(4, clock.sourceId)
        assertEquals(listOf(ClockHop(4, ClockHopKind.Source)), clock.path)
        assertEquals("source 4, fallback to the first source", clock.description)
    }

    @Test
    fun aDeviceWithoutClockEntitiesHasNoClock() {
        val format = parseStreamFormat(Fixtures.FORMAT_NATIVE_DSD)
        assertNull(format.clock)
        assertEquals(Pacing.Adaptive, format.pacing)
    }

    @Test
    fun everyPacingModeParses() {
        fun pacingOf(mode: String, endpoint: String = "null"): Pacing = parseStreamFormat(
            JSONObject(Fixtures.FORMAT)
                .put("pacing", JSONObject("""{ "mode": "$mode", "feedback_endpoint": $endpoint }"""))
                .toString(),
        ).pacing
        assertEquals(Pacing.ExplicitFeedback(131), pacingOf("explicit_feedback", "131"))
        assertEquals(Pacing.Adaptive, pacingOf("adaptive"))
        assertEquals(Pacing.Synchronous, pacingOf("synchronous"))
        assertEquals(Pacing.Unsynchronized, pacingOf("unsynchronized"))
        assertEquals(Pacing.Unknown, pacingOf("something_new"))
    }

    @Test
    fun aFormatFromABuildWithoutTheDiagnosticsStillParses() {
        val json = JSONObject(Fixtures.FORMAT).apply { remove("pacing"); remove("clock") }.toString()
        val format = parseStreamFormat(json)
        assertEquals(Pacing.Unknown, format.pacing)
        assertNull(format.clock)
        assertEquals(44100, format.sampleRate)
    }

    @Test
    fun anUnknownClockResolutionOrHopKindIsUnknownNotAnError() {
        val clock = JSONObject(Fixtures.FORMAT).getJSONObject("clock")
            .put("resolution", "something_new")
        clock.getJSONArray("path").getJSONObject(2).put("kind", "something_new")
        val format = parseStreamFormat(JSONObject(Fixtures.FORMAT).put("clock", clock).toString())
        assertEquals(ClockResolution.UNKNOWN, format.clock!!.resolution)
        assertEquals(ClockHopKind.Unknown, format.clock!!.path[2].kind)
        assertEquals(8, format.clock!!.path[2].id)
    }

    @Test
    fun capabilitiesCarryThePacingOfEachAlternateSetting() {
        assertEquals(
            Pacing.ExplicitFeedback(0x81),
            parseCapabilities(Fixtures.CAPABILITIES).altSettings.single().pacing,
        )
        assertEquals(Pacing.Adaptive, parseCapabilities(Fixtures.CAPABILITIES_NO_VOLUME).altSettings.single().pacing)
    }

    @Test
    fun anAlternateSettingTheLibraryWouldNotStreamHasNoPacing() {
        // null: refused (an asynchronous endpoint without feedback, with allowUnsynchronizedAsync off).
        val refused = JSONObject(Fixtures.CAPABILITIES).apply {
            getJSONArray("alt_settings").getJSONObject(0).put("pacing", JSONObject.NULL)
        }.toString()
        assertNull(parseCapabilities(refused).altSettings.single().pacing)

        // The same for a build that does not report it.
        val absent = JSONObject(Fixtures.CAPABILITIES).apply {
            getJSONArray("alt_settings").getJSONObject(0).remove("pacing")
        }.toString()
        assertNull(parseCapabilities(absent).altSettings.single().pacing)
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
        assertTrue(json.getBoolean("cancel_in_flight_on_flush"))
        // The DAC is protected by default, and a stream needs a volume to bring it down.
        assertEquals("attenuate", json.getString("startup_volume"))
        assertEquals(-40.0, json.getDouble("startup_volume_max_db"), 0.0)
        assertTrue(json.getBoolean("require_hardware_volume"))
        // Unset options are left out, so the native side applies the device quirk.
        for (key in listOf("clock_settle_delay_ms", "dsd_big_endian", "dsd_subslot", "padded_24bit")) {
            assertFalse(key, json.has(key))
        }
        assertEquals(0, json.getJSONArray("quirks").length())
    }

    @Test
    fun keepLeavesTheCeilingOutOfTheNativeConfig() {
        val json = JSONObject(DacConfig(startupVolume = StartupVolume.Keep).toNativeJson())
        assertEquals("keep", json.getString("startup_volume"))
        assertFalse(json.has("startup_volume_max_db"))
    }

    @Test
    fun aStartupReportParsesFromTheDocumentedSchema() {
        val report = parseStartupReport(Fixtures.STARTUP)
        assertEquals(listOf(-10.0, -70.0), report.originalDb)
        assertEquals(listOf(-70.0, -70.0), report.appliedDb)
        assertEquals(-70.0, report.limitDb, 0.0)
        assertTrue(report.attenuated)
        assertFalse(report.clampedToMinimum)
        assertEquals(-40.0, report.maxDb)

        val kept = parseStartupReport(Fixtures.STARTUP_KEEP)
        assertFalse(kept.attenuated)
        assertNull("Keep has no ceiling", kept.maxDb)
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
            cancelInFlightOnFlush = false,
            startupVolume = StartupVolume.Attenuate(-25.5),
            requireHardwareVolume = false,
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
                        DacQuirk.ExplicitFeedbackPerServiceInterval,
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
        assertFalse(json.getBoolean("cancel_in_flight_on_flush"))
        assertEquals("attenuate", json.getString("startup_volume"))
        assertEquals(-25.5, json.getDouble("startup_volume_max_db"), 0.0)
        assertFalse(json.getBoolean("require_hardware_volume"))

        val entry = json.getJSONArray("quirks").getJSONObject(0)
        assertEquals(1, entry.getInt("vendor_id"))
        assertEquals(2, entry.getInt("product_id"))
        assertEquals("Foo", entry.getString("name_contains"))
        val quirks = entry.getJSONArray("quirks")
        assertEquals("skip_clock_validation", quirks.getString(0))
        assertEquals(80, quirks.getJSONObject(1).getInt("settle_delay_ms"))
        assertEquals(4, quirks.getJSONObject(2).getInt("dsd_subslot_size"))
        assertEquals("dsd_bit_reverse", quirks.getString(3))
        assertEquals("explicit_feedback_per_service_interval", quirks.getString(4))
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
    @Test
    fun adaptiveAndSchedulerStatsDecodeAndRemainCompatibleWithOlderNativeReports() {
        val stats = parseStats("""{"worker_gap_over_10ms":3,"worker_gap_over_50ms":1,
            "max_worker_gap_us":65000,"adaptive_queue_changes":2,
            "adaptive_queue_slots":12,"adaptive_queue_us":24000}""")
        assertEquals(3L, stats.workerGapOver10ms)
        assertEquals(1L, stats.workerGapOver50ms)
        assertEquals(65000L, stats.maxWorkerGapUs)
        assertEquals(2L, stats.adaptiveQueueChanges)
        assertEquals(12L, stats.adaptiveQueueSlots)
        assertEquals(24000L, stats.adaptiveQueueUs)
        assertEquals(0L, parseStats("{}").adaptiveQueueChanges)
    }

    @Test
    fun compatibilityPolicyDefaultsAndOptInsReachNativeWithoutBroadeningLegacyOverride() {
        val defaults = JSONObject(DacConfig().toNativeJson())
        assertEquals("full_speed_uac1", defaults.getString("async_fallback_policy"))
        assertEquals("require_verified", defaults.getString("clock_readback_policy"))
        assertTrue(defaults.getBoolean("adaptive_queue"))
        assertFalse(defaults.getBoolean("allow_unsynchronized_async"))
        val explicit = JSONObject(DacConfig(asyncFallbackPolicy = AsyncFallbackPolicy.FORBID,
            clockReadbackPolicy = ClockReadbackPolicy.ALLOW_UNUSABLE_AFTER_SUCCESSFUL_SET,
            adaptiveQueue = false).toNativeJson())
        assertEquals("forbid", explicit.getString("async_fallback_policy"))
        assertEquals("allow_unusable_after_successful_set", explicit.getString("clock_readback_policy"))
        assertFalse(explicit.getBoolean("adaptive_queue"))
        assertFalse(explicit.getBoolean("allow_unsynchronized_async"))
    }

    @Test
    fun minimumMuteDbIsExplicitAndNeverDefaultsMalformedValuesToFullScale() {
        assertEquals(Double.NEGATIVE_INFINITY, parseVolumeState("""{"db":"-Infinity","fraction":0,"muted":true}""").db, 0.0)
        assertEquals(Double.NEGATIVE_INFINITY, parseVolumeRange("""{"min_db":"-Infinity","max_db":0,"res_db":1}""").minDb, 0.0)
        val startup = parseStartupReport("""{"original_db":["-Infinity"],"applied_db":["-Infinity"],
            "limit_db":"-Infinity","attenuated":false,"clamped_to_minimum":false,"max_db":-40}""")
        assertEquals(listOf(Double.NEGATIVE_INFINITY), startup.originalDb)
        assertEquals(listOf(Double.NEGATIVE_INFINITY), startup.appliedDb)
        assertEquals(Double.NEGATIVE_INFINITY, startup.limitDb, 0.0)
        for (invalid in listOf("null", "\"bad\"", "\"Infinity\"", "\"NaN\"")) {
            org.junit.Assert.assertThrows(org.json.JSONException::class.java) {
                parseVolumeState("""{"db":$invalid,"fraction":0,"muted":false}""")
            }
        }
    }

    @Test
    fun unsignedAndFloatPcmKeepDistinctWireEncodings() {
        for ((request, wire) in listOf(StreamRequest.pcmU8(48_000, 2) to "pcm_u8",
                StreamRequest.pcmFloat32(96_000, 2) to "pcm_float32")) {
            val json = JSONObject(request.toJson())
            assertEquals(wire, json.getString("encoding"))
            assertEquals(request.bitDepth, json.getInt("bit_depth"))
            assertEquals(request.sampleRate, json.getInt("sample_rate"))
        }
        assertEquals("source_backed", JSONObject(DacConfig(quirkPolicy = QuirkPolicy.SOURCE_BACKED).toNativeJson()).getString("quirk_policy"))
    }

    @Test
    fun sourceBackedQuirkOverridesPreserveExplicitKindsAndDurations() {
        val quirks = listOf(DacQuirk.ControlMessageDelayUs(50), DacQuirk.InterfaceDelayMs(20),
            DacQuirk.SetInterfaceFirst, DacQuirk.PlaybackUrbFixup, DacQuirk.SkipSampleRateReadback,
            DacQuirk.SkipImplicitFeedback, DacQuirk.MixerPlaybackMinMute, DacQuirk.MixerPlaybackLinearVolume,
            DacQuirk.RawDsd32Be, DacQuirk.FixedRate, DacQuirk.ImplicitSilenceOnEmpty,
            DacQuirk.SkipClockSelectorWrites, DacQuirk.IgnoreOptionalControlErrors)
        val config = DacConfig(quirks = listOf(QuirkOverride(1, 2, quirks = quirks)))
        val encoded = JSONObject(config.toNativeJson()).getJSONArray("quirks").getJSONObject(0).getJSONArray("quirks")
        assertEquals(50L, encoded.getJSONObject(0).getLong("control_message_delay_us"))
        assertEquals(20L, encoded.getJSONObject(1).getLong("interface_delay_ms"))
        assertEquals("set_interface_first", encoded.getString(2))
        assertEquals("ignore_optional_control_errors", encoded.getString(12))
        assertEquals(quirks.size, encoded.length())
    }

}
