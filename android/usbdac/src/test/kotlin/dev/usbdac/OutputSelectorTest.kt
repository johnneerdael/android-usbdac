package dev.usbdac

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertNull
import org.junit.Assert.assertSame
import org.junit.Assert.assertTrue
import org.junit.Assert.fail
import org.junit.Test

private val PCM_96_24 = StreamRequest.pcm(96_000, channels = 2, bitDepth = 24)
private val PCM_44_16 = StreamRequest.pcm(44_100, channels = 2, bitDepth = 16)

private fun selector(platform: FakePlatform, policy: OutputPolicy = OutputPolicy()) = OutputSelector(platform, policy)

private inline fun expectError(code: DacErrorCode, block: () -> Unit): DacException {
    try {
        block()
    } catch (error: DacException) {
        assertEquals(error.message, code, error.code)
        return error
    }
    fail("expected a DacException with code $code")
    throw AssertionError()
}

private fun OutputDecision.rejected(kind: OutputKind): Rejection =
    alternativesRejected.single { it.kind == kind }

class OutputSelectorTest {

    @Test
    fun prepared_dac_transfers_the_same_session_to_first_track() {
        val platform = FakePlatform()
        val dac = fakeDac()
        val link = platform.attach(dac)
        val selector = selector(platform)
        val prepared = selector.prepareUsb(dac)
        assertSame(dac, prepared.device)
        assertNull(prepared.volume) // fake link has no volume object
        assertEquals(1, link.openCount)
        assertTrue(link.negotiated.isEmpty())
        expectError(DacErrorCode.INVALID_STATE) { selector.select(PCM_96_24) }
        val output = selector.open(PCM_96_24)
        assertEquals(1, link.openCount)
        assertSame(dac, output.decision.dac)
        assertEquals(1, link.streams.size)
        prepared.close() // transferred: cannot close the playing session
        assertEquals(0, link.closeCount)
        output.close()
        assertEquals(1, link.closeCount)
    }

    @Test
    fun preparation_exposes_volume_and_transfers_the_same_control() {
        val platform = FakePlatform()
        val dac = fakeDac()
        val link = platform.attach(dac)
        var state = VolumeState(-40.0, 0.5, false)
        val backend = object : dev.usbdac.internal.VolumeBackend {
            override fun get() = state
            override fun setDb(db: Double): VolumeState = state.copy(db = db).also { state = it }
            override fun setFraction(fraction: Double): VolumeState = state.copy(fraction = fraction).also { state = it }
            override fun step(direction: Int, steps: Int) = state
            override fun setMute(muted: Boolean): VolumeState = state.copy(muted = muted).also { state = it }
            override fun originalDb(): Double? = 0.0
            override fun handBack() = true
        }
        val volume = DacVolume(backend, VolumeRange(-80.0, 0.0, 1.0), true, state,
            StartupVolumeReport(listOf(0.0), listOf(-40.0), 0.0, true, false, -40.0),
            java.util.concurrent.Executor { it.run() }) { throw it }
        val routed = object : dev.usbdac.internal.OutputPlatform by platform {
            override fun openDac(dac: DacDeviceInfo, config: DacConfig): dev.usbdac.internal.DirectLink {
                val opened = platform.openDac(dac, config)
                return object : dev.usbdac.internal.DirectLink by opened { override val volume = volume }
            }
        }
        val selector = OutputSelector(routed, OutputPolicy())
        val prepared = selector.prepareUsb(dac)
        assertSame(volume, prepared.volume)
        prepared.volume!!.setDb(-20.0)
        assertEquals(-20.0, prepared.volume!!.refresh().db, 0.0)
        val output = selector.open(PCM_96_24)
        assertSame(volume, output.volume)
        assertEquals(1, link.openCount)
        output.close()
        assertEquals(1, link.closeCount)
    }

    @Test
    fun prepared_dac_cancel_is_idempotent_and_selector_can_be_reused() {
        val platform = FakePlatform()
        val dac = fakeDac()
        val link = platform.attach(dac)
        val selector = selector(platform)
        val prepared = selector.prepareUsb(dac)
        prepared.close()
        prepared.close()
        assertEquals(1, link.closeCount)
        selector.open(PCM_96_24).close()
        assertEquals(2, link.openCount)
    }

    @Test
    fun prepared_dac_failure_closes_once_without_fallback() {
        val platform = FakePlatform()
        val dac = fakeDac()
        val link = platform.attach(dac)
        val selector = selector(platform)
        val prepared = selector.prepareUsb(dac)
        link.onNegotiate = { throw unsupported() }
        expectError(DacErrorCode.UNSUPPORTED) { selector.open(PCM_96_24) }
        prepared.close()
        assertEquals(1, link.closeCount)
        assertTrue(platform.managed.isEmpty())
        assertTrue(platform.offers.isEmpty())
    }

    @Test
    fun prepared_dac_does_not_follow_a_detached_or_unpermitted_device() {
        val platform = FakePlatform()
        val dac = fakeDac()
        val link = platform.attach(dac)
        val selector = selector(platform)
        val prepared = selector.prepareUsb(dac)
        platform.dacs = emptyList()
        expectError(DacErrorCode.DEVICE_LOST) { selector.open(PCM_96_24) }
        prepared.close()
        assertEquals(1, link.closeCount)
        platform.dacs = listOf(dac)
        platform.permitted.clear()
        expectError(DacErrorCode.PERMISSION_DENIED) { selector.prepareUsb(dac) }
        assertEquals(1, link.openCount)
    }

    @Test
    fun prepared_dac_stream_open_failure_releases_session() {
        val platform = FakePlatform()
        val dac = fakeDac()
        val link = platform.attach(dac)
        val selector = selector(platform)
        val prepared = selector.prepareUsb(dac)
        link.openStreamError = unsupported()
        expectError(DacErrorCode.UNSUPPORTED) { selector.open(PCM_96_24) }
        prepared.close()
        assertEquals(1, link.closeCount)
    }

    @Test
    fun downstream_limit_checks_actual_wire_and_never_uses_an_unverified_platform_route() {
        val platform = FakePlatform()
        val link = platform.attach(fakeDac())
        link.onNegotiate = { fakeFormat(rate = 96_000, bits = 24, subslot = 3) }
        val policy = OutputPolicy(downstreamLimits = DownstreamLimits(48_000, setOf(16)))
        expectError(DacErrorCode.UNSUPPORTED) { selector(platform, policy).select(PCM_96_24) }
        assertTrue(platform.managed.isEmpty())
        assertTrue(platform.offers.isEmpty())
        assertEquals(1, link.closeCount)
    }

    @Test
    fun prepared_output_obeys_downstream_limits_and_does_not_fallback() {
        val platform = FakePlatform()
        val dac = fakeDac()
        val link = platform.attach(dac)
        link.onNegotiate = { fakeFormat(rate = 96_000, bits = 24, subslot = 3) }
        val selector = selector(platform, OutputPolicy(downstreamLimits = DownstreamLimits(48_000, setOf(16))))
        val prepared = selector.prepareUsb(dac)
        expectError(DacErrorCode.UNSUPPORTED) { selector.open(PCM_96_24) }
        prepared.close()
        assertEquals(1, link.closeCount)
        assertTrue(platform.managed.isEmpty())
    }

    @Test
    fun selector_prefers_lowest_rate_above_and_reports_source_wire_and_conversion() {
        val platform = FakePlatform()
        val link = platform.attach(fakeDac())
        link.capabilities = fakeCapabilities(depths = listOf(16)).copy(pcmSampleRates = listOf(48_000, 88_200))
        link.onNegotiate = { request ->
            if (request.sampleRate !in listOf(48_000, 88_200)) throw unsupported()
            fakeFormat(request.sampleRate, 16, 2)
        }
        val decision = selector(platform).select(PCM_44_16)
        assertEquals(48_000, decision.wireFormat!!.sampleRate)
        assertEquals(PCM_44_16, decision.source)
        assertNotNull(decision.conversion)
        assertTrue(link.negotiated.all { it.ratePolicy == RatePolicy.EXACT })
    }

    @Test
    fun selector_prefers_highest_integer_divisor_over_other_supported_rates() {
        for ((sourceRate, rates, expected) in listOf(
            Triple(192_000, listOf(44_100, 48_000, 88_200, 96_000), 96_000),
            Triple(176_400, listOf(44_100, 48_000, 88_200, 96_000), 88_200),
        )) {
            val platform = FakePlatform()
            val link = platform.attach(fakeDac())
            link.capabilities = fakeCapabilities(depths = listOf(16)).copy(pcmSampleRates = rates)
            link.onNegotiate = { request ->
                if (request.sampleRate !in rates) throw unsupported()
                fakeFormat(request.sampleRate, 16, 2)
            }
            val decision = selector(platform).select(StreamRequest.pcm(sourceRate, 2, 16))
            assertEquals(expected, decision.wireFormat!!.sampleRate)
        }
    }

    @Test
    fun per_alternate_ranges_control_candidate_rate_and_width() {
        val platform = FakePlatform()
        val link = platform.attach(fakeDac())
        val alt = AltSetting(1, 1, 1, AltFormat.PCM, 2, 3, 24,
            listOf(RateRange(48_000, 96_000, 1_000)), emptyList(), SyncType.ASYNCHRONOUS,
            0x81, 1024, 125, UacVersion.V2, Pacing.ExplicitFeedback(0x81))
        link.capabilities = fakeCapabilities().copy(altSettings = listOf(alt),
            pcmSampleRates = listOf(44_100), pcmBitDepths = listOf(16))
        link.onNegotiate = { request ->
            if (request.sampleRate != 48_000 || request.bitDepth != 24) throw unsupported()
            fakeFormat(request.sampleRate, request.bitDepth, 3)
        }
        val policy = OutputPolicy(downstreamLimits = DownstreamLimits(50_000, setOf(24)))
        val decision = selector(platform, policy).select(PCM_44_16)
        assertEquals(48_000, decision.wireFormat!!.sampleRate)
        assertEquals(24, decision.wireFormat!!.validBits)
        assertTrue(link.negotiated.drop(1).all { it.bitDepth == 24 && it.sampleRate <= 50_000 })
    }

    @Test
    fun disabling_widening_rejects_lossless_widening_even_if_conversion_is_enabled() {
        val platform = FakePlatform()
        val link = platform.attach(fakeDac())
        link.capabilities = fakeCapabilities(depths = listOf(24)).copy(pcmSampleRates = listOf(44_100))
        link.onNegotiate = { request ->
            if (request.bitDepth != 24) throw unsupported()
            fakeFormat(request.sampleRate, 24, 3)
        }
        platform.planKind = "bit_exact"
        val decision = selector(platform, OutputPolicy(allowConversion = true, allowWidening = false)).select(PCM_44_16)
        assertEquals(OutputKind.AAUDIO_EXCLUSIVE, decision.kind)
    }

    @Test
    fun float_source_is_planned_against_integer_wire() {
        val platform = FakePlatform()
        val link = platform.attach(fakeDac())
        link.capabilities = fakeCapabilities(depths = listOf(24))
        link.onNegotiate = { request ->
            if (request.encoding != StreamEncoding.PCM || request.bitDepth != 24 || request.sampleRate != 44_100) throw unsupported()
            fakeFormat(44_100, 24, 3)
        }
        val source = StreamRequest.pcmFloat32(44_100, 2)
        val decision = selector(platform).select(source)
        assertEquals(source, decision.source)
        assertEquals(StreamEncoding.PCM, decision.wireFormat!!.encoding)
        assertNotNull(decision.conversion)
    }

    @Test
    fun dop_requires_explicit_decoder_confirmation_even_when_carrier_is_advertised() {
        val platform = FakePlatform()
        val link = platform.attach(fakeDac())
        val request = StreamRequest.dop(2_822_400, 2)
        expectError(DacErrorCode.UNSUPPORTED) { selector(platform, OutputPolicy(allowConversion = false)).select(request) }
        assertTrue(link.negotiated.isEmpty())
        link.onNegotiate = { StreamFormat(1, StreamEncoding.DOP, null, 176_400, 2, 24, 3, 6,
            176_400, 1, 1, 1, SyncType.ASYNCHRONOUS, true, LinkSpeed.HIGH, 125, false,
            Pacing.ExplicitFeedback(0x81)) }
        val decision = selector(platform, OutputPolicy(dopSupport = DopSupport.CONFIRMED)).select(request)
        assertEquals(StreamEncoding.DOP, decision.wireFormat!!.encoding)
        assertEquals(request, decision.source)
    }

    @Test
    fun selector_passes_options_and_only_applies_headroom_to_resampling() {
        for (targetRate in listOf(44_100, 48_000)) {
            val platform = FakePlatform()
            val link = platform.attach(fakeDac())
            link.capabilities = fakeCapabilities(depths = listOf(24)).copy(pcmSampleRates = listOf(targetRate))
            link.onNegotiate = { request ->
                if (request.bitDepth != 24 || request.sampleRate != targetRate) throw unsupported()
                fakeFormat(targetRate, 24, 3)
            }
            val seen = mutableListOf<ConversionOptions>()
            val routed = object : dev.usbdac.internal.OutputPlatform by platform {
                override fun plan(source: StreamRequest, target: StreamFormat, options: ConversionOptions): ConversionPlan {
                    seen += options
                    return platform.plan(source, target)
                }
            }
            val options = ConversionOptions(headroomDb = -4.0, quality = ResampleQuality.FAST, ditherSeed = 42)
            OutputSelector(routed, OutputPolicy(conversionOptions = options)).select(PCM_44_16)
            assertEquals(if (targetRate == 44_100) 0.0 else -4.0, seen.single().headroomDb, 0.0)
            assertEquals(ResampleQuality.FAST, seen.single().quality)
            assertEquals(42L, seen.single().ditherSeed)
        }
    }

    @Test
    fun native_dsd_uses_dop_only_for_confirmed_decoder_and_reports_source_units() {
        val platform = FakePlatform()
        val link = platform.attach(fakeDac())
        link.onNegotiate = { request ->
            if (request.encoding != StreamEncoding.DOP) throw unsupported()
            StreamFormat(1, StreamEncoding.DOP, null, 176_400, 2, 24, 3, 6,
                176_400, 1, 1, 1, SyncType.ASYNCHRONOUS, true, LinkSpeed.HIGH, 125, false,
                Pacing.ExplicitFeedback(0x81))
        }
        val policy = OutputPolicy(allowConversion = false, dopSupport = DopSupport.CONFIRMED)
        val source = StreamRequest.dsdNative(2_822_400, 2)
        val output = selector(platform, policy).open(source)
        assertEquals(StreamEncoding.DSD_NATIVE, output.format.encoding)
        assertEquals(352_800, output.format.counterRateHz)
        assertEquals(2, output.format.bytesPerFrame)
        assertEquals(StreamEncoding.DOP, output.decision.wireFormat!!.encoding)
        assertEquals(true, output.decision.processing!!.preservesBitPerfect)
        output.close()
    }

    @Test
    fun unconfirmed_dop_decoder_uses_validating_pcm_fallback() {
        val platform = FakePlatform()
        val link = platform.attach(fakeDac())
        link.onNegotiate = { request ->
            if (request.encoding != StreamEncoding.PCM) throw unsupported()
            fakeFormat(request.sampleRate, request.bitDepth, request.bitDepth / 8)
        }
        val source = StreamRequest.dop(2_822_400, 2)
        val decision = selector(platform, OutputPolicy(allowDopPcmConversion = true)).select(source)
        assertEquals(StreamEncoding.PCM, decision.wireFormat!!.encoding)
        assertEquals(source, decision.source)
        assertTrue(decision.processing!!.description.contains("DoP unpacked"))
        assertEquals(false, decision.processing!!.preservesBitPerfect)
        assertTrue(link.negotiated.none { it.encoding == StreamEncoding.DOP })
    }

    @Test
    fun native_dsd_pcm_fallback_is_explicitly_reported_lossy() {
        val platform = FakePlatform()
        val link = platform.attach(fakeDac())
        link.onNegotiate = { request ->
            if (request.encoding != StreamEncoding.PCM) throw unsupported()
            fakeFormat(request.sampleRate, request.bitDepth, request.bitDepth / 8)
        }
        val decision = selector(platform).select(StreamRequest.dsdNative(2_822_400, 2))
        assertEquals(StreamEncoding.PCM, decision.wireFormat!!.encoding)
        assertEquals(false, decision.processing!!.preservesBitPerfect)
        assertEquals(Guarantee.NONE, decision.guarantee)
        assertTrue(link.negotiated.none { it.encoding == StreamEncoding.DOP })
    }

    @Test
    fun dop_pcm_conversion_requires_both_permissions() {
        for (policy in listOf(OutputPolicy(force = OutputKind.ANDROID_STANDARD),
            OutputPolicy(force = OutputKind.ANDROID_STANDARD, allowConversion = false, allowDopPcmConversion = true))) {
            expectError(DacErrorCode.UNSUPPORTED) { selector(FakePlatform(), policy).select(StreamRequest.dop(2_822_400, 2)) }
        }
        val direct = FakePlatform()
        val link = direct.attach(fakeDac())
        link.onNegotiate = { request ->
            if (request.encoding != StreamEncoding.PCM) throw unsupported()
            fakeFormat(request.sampleRate, request.bitDepth, request.bitDepth / 8)
        }
        expectError(DacErrorCode.UNSUPPORTED) {
            selector(direct, OutputPolicy(force = OutputKind.DIRECT_USB)).select(StreamRequest.dop(2_822_400, 2))
        }
        assertTrue(link.negotiated.none { it.encoding == StreamEncoding.PCM })
        val policy = OutputPolicy(force = OutputKind.ANDROID_STANDARD, allowDopPcmConversion = true)
        val source = StreamRequest.dop(2_822_400, 2)
        val decision = selector(FakePlatform(), policy).select(source)
        assertEquals(source, decision.source)
        assertNull(decision.wireFormat)
        assertEquals(Guarantee.NONE, decision.guarantee)
        assertFalse(decision.processing!!.preservesBitPerfect)
    }

    @Test
    fun android_dsd_pcm_preview_preserves_source_and_finite_managed_rate() {
        val source = StreamRequest.dsdNative(11_289_600, 2)
        val decision = selector(FakePlatform(), OutputPolicy(force = OutputKind.ANDROID_STANDARD)).select(source)
        assertEquals(source, decision.source)
        assertEquals(Guarantee.NONE, decision.guarantee)
        assertNull(decision.wireFormat)
        assertTrue(decision.processing!!.description.contains("176400 Hz"))
        assertNull(AndroidManagedOutput.carryProblem(StreamRequest.pcm(192_000, 2, 16), 34))
        assertNotNull(AndroidManagedOutput.carryProblem(StreamRequest.pcm(192_001, 2, 16), 34))
    }

    @Test
    fun android_dsd_pcm_preview_covers_mixer_and_aaudio_targets() {
        val source = StreamRequest.dsdNative(2_822_400, 2)
        val mixer = FakePlatform().apply { route = listOf(USB_ROUTE) }
        val mixerDecision = selector(mixer, OutputPolicy(force = OutputKind.ANDROID_BIT_PERFECT)).select(source)
        assertEquals(Guarantee.NONE, mixerDecision.guarantee)
        assertNull(mixerDecision.wireFormat)
        assertFalse(mixerDecision.processing!!.preservesBitPerfect)

        val audio = FakePlatform().apply {
            grant = fakeGrant().copy(sampleRate = 176_400, channels = 2, format = AAudioSampleFormat.PCM_16)
        }
        val audioDecision = selector(audio, OutputPolicy(force = OutputKind.AAUDIO_EXCLUSIVE)).select(source)
        assertEquals(Guarantee.NONE, audioDecision.guarantee)
        assertNull(audioDecision.wireFormat)
        assertEquals(source, audioDecision.source)
        assertTrue(audio.offers.single().closed)
    }

    @Test
    fun unsupported_dsd_conversion_is_refused_before_any_android_open() {
        val platform = FakePlatform()
        val policy = OutputPolicy(force = OutputKind.ANDROID_STANDARD)
        expectError(DacErrorCode.UNSUPPORTED) { selector(platform, policy).open(StreamRequest.dsdNative(22_579_200, 2)) }
        assertTrue(platform.managed.isEmpty())
        expectError(DacErrorCode.UNSUPPORTED) { selector(platform, policy).open(StreamRequest.dsdNative(2_822_400, 9)) }
        assertTrue(platform.managed.isEmpty())
    }

    // ---- 1. direct USB ----

    @Test
    fun a_dac_with_permission_is_driven_directly_and_nothing_stays_open_after_a_look() {
        val platform = FakePlatform()
        val dac = fakeDac()
        val link = platform.attach(dac)
        platform.route = listOf(USB_ROUTE)

        val decision = selector(platform).select(PCM_96_24)

        assertEquals(OutputKind.DIRECT_USB, decision.kind)
        assertEquals(Guarantee.VERIFIED_BY_LIBRARY, decision.guarantee)
        assertSame(dac, decision.dac)
        assertNull(decision.conversion)
        assertNull(decision.needsUsbPermission)
        assertTrue(decision.alternativesRejected.isEmpty())
        assertEquals(USB_ROUTE, decision.route)
        assertTrue(decision.reason, decision.reason.contains("Test DAC"))
        // The session was opened to look, negotiated exactly, and handed back; no stream was started.
        assertEquals(1, link.openCount)
        assertEquals(1, link.closeCount)
        assertEquals(listOf(RatePolicy.EXACT), link.negotiated.map { it.ratePolicy })
        assertTrue(link.streams.isEmpty())
    }

    @Test
    fun open_keeps_the_session_and_the_stream_until_the_output_is_closed() {
        val platform = FakePlatform()
        val link = platform.attach(fakeDac())

        val output = selector(platform).open(PCM_96_24)

        assertEquals(OutputKind.DIRECT_USB, output.kind)
        assertEquals(1, link.streams.size)
        assertEquals(0, link.closeCount)
        // The layout to write is the wire layout the DAC negotiated.
        assertEquals(WriteFormat(96_000, 2, 3, 24, 96_000), output.format)
        assertEquals(6, output.format.bytesPerFrame)
        output.play()
        assertTrue(link.streams.single().calls.contains("play"))

        output.close()
        output.close()

        assertTrue(link.streams.single().closed)
        assertEquals(1, link.closeCount)
    }

    @Test
    fun the_wire_layout_is_what_the_dac_negotiated_not_what_was_asked() {
        val platform = FakePlatform()
        platform.attach(fakeDac()).onNegotiate = { fakeFormat(rate = 96_000, bits = 24, subslot = 4) }

        val output = selector(platform).open(PCM_96_24)

        assertEquals(WriteFormat(96_000, 2, 4, 24, 96_000), output.format)
    }

    @Test
    fun without_permission_the_best_other_choice_is_taken_and_the_dac_is_named() {
        val platform = FakePlatform()
        val dac = fakeDac()
        platform.attach(dac, permission = false)
        platform.route = listOf(USB_ROUTE)

        val decision = selector(platform).select(PCM_96_24)

        assertEquals(OutputKind.ANDROID_BIT_PERFECT, decision.kind)
        assertSame(dac, decision.needsUsbPermission)
        assertEquals(RejectionReason.PERMISSION_MISSING, decision.rejected(OutputKind.DIRECT_USB).reason)
        assertFalse("no session may be opened without permission", platform.calls.any { it.startsWith("openDac") })
    }

    @Test
    fun without_permission_and_with_nothing_better_the_standard_output_carries_the_request_to_ask() {
        val platform = FakePlatform()
        val dac = fakeDac()
        platform.attach(dac, permission = false)
        platform.apiLevel = 33
        platform.route = listOf(USB_ROUTE)
        platform.grant = fakeGrant(exclusive = false)

        val decision = selector(platform).select(PCM_96_24)

        assertEquals(OutputKind.ANDROID_STANDARD, decision.kind)
        assertSame(dac, decision.needsUsbPermission)
        assertEquals(
            listOf(OutputKind.DIRECT_USB, OutputKind.ANDROID_BIT_PERFECT, OutputKind.AAUDIO_EXCLUSIVE),
            decision.alternativesRejected.map { it.kind },
        )
    }

    @Test
    fun a_dac_that_needs_implicit_feedback_is_rejected_and_the_next_output_is_tried() {
        val platform = FakePlatform()
        val link = platform.attach(fakeDac())
        link.onNegotiate = { throw DacException(DacErrorCode.IMPLICIT_FEEDBACK_UNSUPPORTED, "no feedback endpoint") }
        platform.route = listOf(USB_ROUTE)

        val decision = selector(platform).select(PCM_96_24)

        assertEquals(OutputKind.ANDROID_BIT_PERFECT, decision.kind)
        val rejection = decision.rejected(OutputKind.DIRECT_USB)
        assertEquals(RejectionReason.IMPLICIT_FEEDBACK, rejection.reason)
        assertEquals(DacErrorCode.IMPLICIT_FEEDBACK_UNSUPPORTED, rejection.error)
        assertTrue(rejection.why, rejection.why.contains("no feedback endpoint"))
        assertTrue("the session of a rejected DAC is given back", link.isClosed)
    }

    @Test
    fun a_dac_without_a_hardware_volume_is_rejected_before_the_clock_is_touched() {
        val platform = FakePlatform()
        val link = platform.attach(fakeDac())
        link.hasUsableVolume = false

        val decision = selector(platform).select(PCM_96_24)

        assertEquals(RejectionReason.NO_HARDWARE_VOLUME, decision.rejected(OutputKind.DIRECT_USB).reason)
        assertEquals(DacErrorCode.VOLUME_UNAVAILABLE, decision.rejected(OutputKind.DIRECT_USB).error)
        assertTrue(link.negotiated.isEmpty())
        assertTrue(link.isClosed)
    }

    @Test
    fun a_fixed_volume_is_accepted_when_the_policy_allows_it() {
        val platform = FakePlatform()
        val link = platform.attach(fakeDac())
        link.hasUsableVolume = false

        val decision = selector(platform, OutputPolicy(dacConfig = DacConfig(requireHardwareVolume = false))).select(PCM_96_24)

        assertEquals(OutputKind.DIRECT_USB, decision.kind)
        assertTrue(decision.reason, decision.reason.contains("no hardware volume"))
    }

    @Test
    fun a_failure_to_open_the_session_is_recorded_with_its_code() {
        val platform = FakePlatform()
        platform.attach(fakeDac())
        platform.openDacError = DacException(DacErrorCode.CLAIM, "Android's driver holds the interfaces")

        val decision = selector(platform).select(PCM_96_24)

        val rejection = decision.rejected(OutputKind.DIRECT_USB)
        assertEquals(RejectionReason.FAILED, rejection.reason)
        assertEquals(DacErrorCode.CLAIM, rejection.error)
    }

    @Test
    fun another_open_session_in_the_process_is_a_busy_rejection() {
        val platform = FakePlatform()
        platform.attach(fakeDac())
        platform.openDacError = DacException(DacErrorCode.BUSY, "another DAC session is open in this process")

        val decision = selector(platform).select(PCM_96_24)

        assertEquals(RejectionReason.BUSY, decision.rejected(OutputKind.DIRECT_USB).reason)
    }

    @Test
    fun a_clock_that_cannot_be_verified_is_a_negotiate_failure_and_is_recorded() {
        val platform = FakePlatform()
        val link = platform.attach(fakeDac())
        link.onNegotiate = { throw DacException(DacErrorCode.CLOCK_UNVERIFIED, "the DAC did not confirm 96000 Hz") }

        val decision = selector(platform).select(PCM_96_24)

        assertEquals(DacErrorCode.CLOCK_UNVERIFIED, decision.rejected(OutputKind.DIRECT_USB).error)
        assertTrue(link.isClosed)
    }

    @Test
    fun a_stream_that_will_not_open_falls_through_to_the_next_output_and_gives_the_session_back() {
        val platform = FakePlatform()
        val link = platform.attach(fakeDac())
        link.openStreamError = DacException(DacErrorCode.CLOCK_UNVERIFIED, "the DAC did not confirm the rate on open")
        platform.route = listOf(USB_ROUTE)

        val output = selector(platform).open(PCM_96_24)

        assertEquals(OutputKind.ANDROID_BIT_PERFECT, output.kind)
        assertEquals(DacErrorCode.CLOCK_UNVERIFIED, output.decision.rejected(OutputKind.DIRECT_USB).error)
        assertEquals(1, link.closeCount)
    }

    @Test
    fun the_first_dac_that_works_is_used_when_several_are_attached() {
        val platform = FakePlatform()
        val first = fakeDac("First", "/dev/bus/usb/001/002")
        val second = fakeDac("Second", "/dev/bus/usb/001/003")
        platform.attach(first).onNegotiate = { throw unsupported() }
        platform.attach(second)

        val decision = selector(platform).select(PCM_96_24)

        assertEquals(OutputKind.DIRECT_USB, decision.kind)
        assertSame(second, decision.dac)
        assertEquals(1, decision.alternativesRejected.size)
        assertTrue(decision.alternativesRejected.single().why.startsWith("First:"))
    }

    @Test
    fun a_dac_that_cannot_run_the_rate_is_rejected_when_conversion_is_not_allowed() {
        val platform = FakePlatform()
        val link = platform.attach(fakeDac())
        link.onNegotiate = { throw unsupported() }

        val decision = selector(platform, OutputPolicy(allowConversion = false, allowWidening = false)).select(PCM_44_16)

        assertEquals(RejectionReason.REQUEST_NOT_SUPPORTED, decision.rejected(OutputKind.DIRECT_USB).reason)
        assertFalse(platform.calls.contains("plan"))
        assertEquals("only the exact format is asked for", 1, link.negotiated.size)
    }

    @Test
    fun a_request_with_a_nearest_rate_policy_is_still_negotiated_exactly_without_conversion() {
        val platform = FakePlatform()
        val link = platform.attach(fakeDac())

        selector(platform).select(StreamRequest.pcm(44_100, 2, 16, RatePolicy.NEAREST_SUPPORTED))

        assertEquals(listOf(RatePolicy.EXACT), link.negotiated.map { it.ratePolicy })
    }

    // ---- conversion ----

    @Test
    fun with_conversion_allowed_the_nearest_format_is_planned_and_the_guarantee_drops() {
        val platform = FakePlatform()
        val link = platform.attach(fakeDac())
        link.capabilities = fakeCapabilities(depths = listOf(16))
        link.onNegotiate = { request ->
            if (request.sampleRate != 48_000) throw unsupported()
            fakeFormat(rate = 48_000, bits = 16, subslot = 2)
        }
        platform.planKind = "lossy"

        val decision = selector(platform, OutputPolicy(allowConversion = true)).select(PCM_44_16)

        assertEquals(OutputKind.DIRECT_USB, decision.kind)
        assertEquals(Guarantee.NONE, decision.guarantee)
        assertNotNull(decision.conversion)
        assertEquals(ConversionKind.LOSSY, decision.conversion!!.kind)
        assertTrue(decision.reason, decision.reason.contains("not bit-perfect"))
        assertTrue(link.negotiated.all { it.ratePolicy == RatePolicy.EXACT })
    }

    @Test
    fun open_puts_a_converter_in_front_of_the_stream_and_the_host_writes_the_source_layout() {
        val platform = FakePlatform()
        val link = platform.attach(fakeDac())
        link.capabilities = fakeCapabilities(depths = listOf(16))
        link.onNegotiate = { request ->
            if (request.sampleRate != 48_000) throw unsupported()
            fakeFormat(rate = 48_000, bits = 16, subslot = 2)
        }

        val output = selector(platform, OutputPolicy(allowConversion = true)).open(PCM_44_16)

        // The host writes 44.1 kHz / 16-bit and counts positions in 44.1 kHz frames, not the DAC's 48 kHz.
        assertEquals(WriteFormat(44_100, 2, 2, 16, 44_100), output.format)
        assertNotNull(output.decision.conversion)
        assertEquals(1, link.streams.size)
    }

    @Test
    fun a_widening_keeps_the_guarantee_because_every_sample_value_is_kept() {
        val platform = FakePlatform()
        val link = platform.attach(fakeDac())
        link.capabilities = fakeCapabilities(depths = listOf(24))
        link.onNegotiate = { request ->
            if (request.bitDepth != 24) throw unsupported()
            fakeFormat(rate = request.sampleRate, bits = 24, subslot = 3)
        }
        platform.planKind = "bit_exact"
        platform.planDescription = "Bit-exact conversion: widened from 16 to 24 bits."

        val decision = selector(platform, OutputPolicy(allowConversion = true)).select(PCM_44_16)

        assertEquals(OutputKind.DIRECT_USB, decision.kind)
        assertEquals(Guarantee.VERIFIED_BY_LIBRARY, decision.guarantee)
        assertEquals(ConversionKind.BIT_EXACT, decision.conversion!!.kind)
        assertTrue(decision.reason, decision.reason.contains("without changing any sample value"))
        // The same rate at the wider depth was tried before any other rate.
        assertEquals(listOf(16 to RatePolicy.EXACT, 24 to RatePolicy.EXACT), link.negotiated.map { it.bitDepth to it.ratePolicy })
    }

    @Test
    fun conversion_tries_the_same_rate_at_another_depth_before_changing_the_rate() {
        val platform = FakePlatform()
        val link = platform.attach(fakeDac())
        link.capabilities = fakeCapabilities(depths = listOf(16, 24))
        link.onNegotiate = { request ->
            when {
                request.ratePolicy == RatePolicy.NEAREST_SUPPORTED -> fakeFormat(48_000, request.bitDepth, request.bitDepth / 8, rateDiffers = true)
                request.bitDepth == 24 -> fakeFormat(request.sampleRate, 24, 3)
                else -> throw unsupported()
            }
        }
        platform.planKind = "bit_exact"

        selector(platform, OutputPolicy(allowConversion = true)).select(PCM_44_16)

        assertTrue(link.negotiated.none { it.ratePolicy == RatePolicy.NEAREST_SUPPORTED })
    }

    @Test
    fun conversion_that_the_dac_cannot_help_with_is_still_a_rejection() {
        val platform = FakePlatform()
        val link = platform.attach(fakeDac())
        link.onNegotiate = { throw unsupported("no alternate setting carries it") }

        val decision = selector(platform, OutputPolicy(allowConversion = true)).select(PCM_44_16)

        assertEquals(RejectionReason.REQUEST_NOT_SUPPORTED, decision.rejected(OutputKind.DIRECT_USB).reason)
        assertTrue(link.isClosed)
    }

    @Test
    fun dop_is_not_converted_when_conversion_is_disabled() {
        val platform = FakePlatform()
        val link = platform.attach(fakeDac())
        link.onNegotiate = { throw unsupported("the DAC has no DoP carrier at that rate") }

        // With conversion disabled, a rejected DoP carrier must not reach PCM fallback.
        val error = expectError(DacErrorCode.UNSUPPORTED) {
            selector(platform, OutputPolicy(allowConversion = false, dopSupport = DopSupport.CONFIRMED)).select(StreamRequest.dop(2_822_400, 2))
        }

        assertTrue(error.message, error.message!!.contains("DIRECT_USB"))
        assertEquals(1, link.negotiated.size)
        assertFalse(platform.calls.contains("plan"))
        assertTrue(link.isClosed)
    }

    @Test
    fun an_endpoint_without_feedback_is_never_called_verified() {
        val platform = FakePlatform()
        platform.attach(fakeDac()).onNegotiate = {
            fakeFormat(syncType = SyncType.ASYNCHRONOUS, explicitFeedback = false, pacing = Pacing.Unsynchronized)
        }

        val decision = selector(platform, OutputPolicy(dacConfig = DacConfig(allowUnsynchronizedAsync = true))).select(PCM_96_24)

        assertEquals(OutputKind.DIRECT_USB, decision.kind)
        assertEquals(Guarantee.NONE, decision.guarantee)
        assertTrue(decision.reason, decision.reason.contains("never verified"))
    }

    // ---- allowDirectUsb ----

    @Test
    fun a_dac_is_left_alone_when_the_app_may_not_take_it_and_the_platform_mixer_is_used() {
        val platform = FakePlatform()
        platform.attach(fakeDac())
        platform.route = listOf(USB_ROUTE)

        val decision = selector(platform, OutputPolicy(allowDirectUsb = false)).select(PCM_96_24)

        assertEquals(OutputKind.ANDROID_BIT_PERFECT, decision.kind)
        assertEquals(RejectionReason.POLICY, decision.rejected(OutputKind.DIRECT_USB).reason)
        assertNull("no permission is asked for what is not allowed", decision.needsUsbPermission)
        assertFalse(platform.calls.any { it.startsWith("openDac") })
    }

    @Test
    fun an_exclusive_stream_on_a_usb_route_is_not_taken_when_the_dac_must_be_shared() {
        val platform = FakePlatform()
        platform.attach(fakeDac())
        platform.route = listOf(USB_ROUTE)
        platform.apiLevel = 33

        val decision = selector(platform, OutputPolicy(allowDirectUsb = false)).select(PCM_96_24)

        assertEquals(OutputKind.ANDROID_STANDARD, decision.kind)
        assertEquals(RejectionReason.POLICY, decision.rejected(OutputKind.AAUDIO_EXCLUSIVE).reason)
        assertFalse(platform.calls.contains("probeAAudio"))
    }

    @Test
    fun an_exclusive_stream_on_a_usb_route_is_tried_when_the_dac_may_be_taken_and_direct_failed() {
        val platform = FakePlatform()
        platform.attach(fakeDac()).onNegotiate = { throw DacException(DacErrorCode.IMPLICIT_FEEDBACK_UNSUPPORTED, "no feedback") }
        platform.route = listOf(USB_ROUTE)
        platform.apiLevel = 33

        val decision = selector(platform).select(PCM_96_24)

        assertEquals(OutputKind.AAUDIO_EXCLUSIVE, decision.kind)
    }

    // ---- 2. the platform's bit-perfect mixer ----

    @Test
    fun a_usb_route_on_android_14_with_the_format_offered_is_bit_perfect_from_the_platform() {
        val platform = FakePlatform()
        platform.route = listOf(USB_ROUTE)

        val decision = selector(platform).select(PCM_96_24)

        assertEquals(OutputKind.ANDROID_BIT_PERFECT, decision.kind)
        assertEquals(Guarantee.REQUESTED_FROM_PLATFORM, decision.guarantee)
        assertEquals(listOf(OutputKind.DIRECT_USB), decision.alternativesRejected.map { it.kind })
        assertEquals(RejectionReason.NOT_AVAILABLE, decision.rejected(OutputKind.DIRECT_USB).reason)
        assertTrue(platform.managed.isEmpty())
    }

    @Test
    fun open_routes_the_track_to_the_usb_device_and_checks_the_platform_kept_the_request() {
        val platform = FakePlatform()
        platform.route = listOf(USB_ROUTE)

        val output = selector(platform).open(PCM_96_24)

        assertEquals(OutputKind.ANDROID_BIT_PERFECT, output.kind)
        assertEquals(USB_ROUTE, platform.managed.single().second)
        assertEquals(WriteFormat(96_000, 2, 3, 24, 96_000), output.format)
        assertNull(output.session)
        assertNull(output.volume)
    }

    @Test
    fun a_platform_that_does_not_keep_the_request_is_a_rejection_and_the_track_is_closed() {
        val platform = FakePlatform()
        platform.route = listOf(USB_ROUTE)
        platform.managedUsbState = BitPerfectState.Unverifiable("the platform refused the bit-perfect request")
        platform.grant = fakeGrant(exclusive = false)

        val output = selector(platform).open(PCM_96_24)

        assertEquals(OutputKind.ANDROID_STANDARD, output.kind)
        val rejection = output.decision.rejected(OutputKind.ANDROID_BIT_PERFECT)
        assertEquals(RejectionReason.FAILED, rejection.reason)
        assertTrue(rejection.why, rejection.why.contains("refused"))
        assertTrue(platform.managed.first().first.closed)
    }

    @Test
    fun a_format_the_platform_has_no_bit_perfect_mixer_for_is_rejected() {
        val platform = FakePlatform()
        platform.route = listOf(USB_ROUTE)
        platform.bitPerfectSupported = false

        val decision = selector(platform).select(PCM_96_24)

        assertNotNull(decision.alternativesRejected.firstOrNull { it.kind == OutputKind.ANDROID_BIT_PERFECT })
        assertEquals(RejectionReason.REQUEST_NOT_SUPPORTED, decision.rejected(OutputKind.ANDROID_BIT_PERFECT).reason)
    }

    @Test
    fun the_bit_perfect_mixer_needs_android_14() {
        for (level in listOf(26, 31, 33)) {
            val platform = FakePlatform()
            platform.apiLevel = level
            platform.route = listOf(USB_ROUTE)

            val decision = selector(platform).select(PCM_44_16)

            assertEquals("API $level", RejectionReason.PLATFORM_TOO_OLD, decision.rejected(OutputKind.ANDROID_BIT_PERFECT).reason)
            assertFalse("API $level", platform.calls.any { it.startsWith("supportsBitPerfect") })
        }
        val platform = FakePlatform()
        platform.apiLevel = 34
        platform.route = listOf(USB_ROUTE)
        assertEquals(OutputKind.ANDROID_BIT_PERFECT, selector(platform).select(PCM_44_16).kind)
    }

    // ---- 3. an exclusive AAudio stream ----

    @Test
    fun an_exclusive_grant_on_the_built_in_route_is_how_a_player_with_an_internal_dac_is_recognised() {
        val platform = FakePlatform()

        val decision = selector(platform).select(PCM_96_24)

        assertEquals(OutputKind.AAUDIO_EXCLUSIVE, decision.kind)
        assertEquals(Guarantee.EXCLUSIVE_GRANTED_UNVERIFIABLE, decision.guarantee)
        assertEquals(
            listOf(OutputKind.DIRECT_USB, OutputKind.ANDROID_BIT_PERFECT),
            decision.alternativesRejected.map { it.kind },
        )
        assertEquals(SPEAKER_ROUTE, decision.route)
    }

    @Test
    fun a_look_at_aaudio_makes_no_sound_and_gives_the_stream_back() {
        val platform = FakePlatform()

        selector(platform).select(PCM_96_24)

        val offer = platform.offers.single()
        assertFalse("the probe must not start the stream", offer.started)
        assertTrue(offer.closed)
    }

    @Test
    fun open_uses_the_stream_that_was_probed_instead_of_opening_another() {
        val platform = FakePlatform()

        val output = selector(platform).open(PCM_96_24)

        val offer = platform.offers.single()
        assertTrue(offer.started)
        assertFalse(offer.closed)
        assertEquals(1, platform.calls.count { it == "probeAAudio" })
        assertEquals(WriteFormat(96_000, 2, 3, 24, 96_000), output.format)
        output.close()
        assertTrue(offer.output.closed)
    }

    @Test
    fun a_shared_grant_is_not_accepted_by_default() {
        val platform = FakePlatform()
        platform.grant = fakeGrant(exclusive = false, reason = "the system mixer is in the path")

        val decision = selector(platform).select(PCM_96_24)

        assertEquals(OutputKind.ANDROID_STANDARD, decision.kind)
        val rejection = decision.rejected(OutputKind.AAUDIO_EXCLUSIVE)
        assertEquals(RejectionReason.EXCLUSIVE_NOT_GRANTED, rejection.reason)
        assertEquals(DacErrorCode.EXCLUSIVE_UNAVAILABLE, rejection.error)
        assertTrue("the probe is closed when it is not used", platform.offers.single().closed)
    }

    @Test
    fun a_shared_grant_can_be_accepted_and_is_then_not_a_guarantee() {
        val platform = FakePlatform()
        platform.grant = fakeGrant(exclusive = false)

        val decision = selector(platform, OutputPolicy(requireExclusiveForAAudio = false)).select(PCM_96_24)

        assertEquals(OutputKind.AAUDIO_EXCLUSIVE, decision.kind)
        assertEquals(Guarantee.NONE, decision.guarantee)
        assertFalse(platform.aaudioConfigs.single().requireExclusive)
    }

    @Test
    fun the_selector_asks_for_exclusive_mode_whatever_the_aaudio_config_says() {
        val platform = FakePlatform()

        selector(platform, OutputPolicy(aaudioConfig = AAudioConfig(requireExclusive = false))).select(PCM_96_24)

        assertTrue(platform.aaudioConfigs.single().requireExclusive)
    }

    @Test
    fun an_exclusive_stream_whose_hardware_runs_another_rate_is_not_what_was_asked() {
        val platform = FakePlatform()
        platform.grant = fakeGrant(exclusive = true, asRequested = false, reason = "the hardware runs 48000 Hz")

        val decision = selector(platform).select(PCM_96_24)

        assertEquals(OutputKind.ANDROID_STANDARD, decision.kind)
        assertEquals(RejectionReason.NOT_AS_REQUESTED, decision.rejected(OutputKind.AAUDIO_EXCLUSIVE).reason)
        assertTrue(platform.offers.single().closed)
    }

    @Test
    fun a_device_held_by_another_app_is_a_busy_rejection() {
        val platform = FakePlatform()
        platform.aaudioError = DacException(DacErrorCode.BUSY, "another app holds the device exclusively")

        val decision = selector(platform).select(PCM_96_24)

        assertEquals(RejectionReason.BUSY, decision.rejected(OutputKind.AAUDIO_EXCLUSIVE).reason)
        assertEquals(OutputKind.ANDROID_STANDARD, decision.kind)
    }

    @Test
    fun aaudio_carries_24_bit_only_from_android_12() {
        val request = PCM_96_24
        for (level in listOf(26, 30)) {
            val platform = FakePlatform()
            platform.apiLevel = level
            val decision = selector(platform).let { runCatching { it.select(request) } }
            // 24-bit is carried by neither AAudio nor AudioTrack before Android 12: nothing can play it.
            val error = decision.exceptionOrNull() as DacException
            assertEquals("API $level", DacErrorCode.UNSUPPORTED, error.code)
            assertFalse("API $level", platform.calls.contains("probeAAudio"))
        }
        val platform = FakePlatform()
        platform.apiLevel = 31
        assertEquals(OutputKind.AAUDIO_EXCLUSIVE, selector(platform).select(request).kind)
    }

    @Test
    fun the_reason_aaudio_is_passed_over_before_android_12_is_the_release() {
        val platform = FakePlatform()
        platform.apiLevel = 30

        val error = expectError(DacErrorCode.UNSUPPORTED) { selector(platform).select(PCM_96_24) }

        assertTrue(error.message, error.message!!.contains("AAUDIO_EXCLUSIVE: AAudio carries 24-bit integer samples from Android 12"))
        assertTrue(error.message, error.message!!.contains("ANDROID_BIT_PERFECT: the bit-perfect mixer behavior needs Android 14"))
    }

    @Test
    fun float_samples_need_no_android_12_in_aaudio() {
        val platform = FakePlatform()
        platform.apiLevel = 26

        val decision = selector(platform, OutputPolicy(aaudioConfig = AAudioConfig(floatSamples = true)))
            .select(StreamRequest.pcm(96_000, 2, 32))

        assertEquals(OutputKind.AAUDIO_EXCLUSIVE, decision.kind)
    }

    @Test
    fun a_channel_count_audiotrack_cannot_carry_is_rejected_by_the_standard_output() {
        val platform = FakePlatform()
        platform.grant = fakeGrant(exclusive = false)

        val error = expectError(DacErrorCode.UNSUPPORTED) { selector(platform).select(StreamRequest.pcm(48_000, 3, 16)) }

        assertTrue(error.message, error.message!!.contains("ANDROID_STANDARD: 3 channels are not supported"))
    }

    @Test
    fun sixteen_bit_works_from_android_8_everywhere() {
        val platform = FakePlatform()
        platform.apiLevel = 26

        val decision = selector(platform).select(PCM_44_16)

        assertEquals(OutputKind.AAUDIO_EXCLUSIVE, decision.kind)
    }

    @Test
    fun api_31_reaches_the_standard_output_for_24_bit_and_api_26_does_not() {
        val failing = FakePlatform()
        failing.apiLevel = 26
        failing.grant = fakeGrant(exclusive = false)
        val error = runCatching { selector(failing).select(PCM_96_24) }.exceptionOrNull() as DacException
        assertEquals(DacErrorCode.UNSUPPORTED, error.code)
        assertTrue(error.message, error.message!!.contains("ANDROID_STANDARD"))
        assertTrue(error.message, error.message!!.contains("API 26"))

        val working = FakePlatform()
        working.apiLevel = 31
        working.grant = fakeGrant(exclusive = false)
        assertEquals(OutputKind.ANDROID_STANDARD, selector(working).select(PCM_96_24).kind)
    }

    // ---- bluetooth and cast ----

    @Test
    fun a_bluetooth_route_goes_straight_to_the_standard_output_and_says_so() {
        val platform = FakePlatform()
        platform.attach(fakeDac())
        platform.route = listOf(BLUETOOTH_ROUTE)

        val output = selector(platform).open(PCM_96_24)

        assertEquals(OutputKind.ANDROID_STANDARD, output.kind)
        assertEquals(Guarantee.NONE, output.decision.guarantee)
        assertTrue(output.decision.reason, output.decision.reason.contains("Pixel Buds"))
        assertEquals(
            listOf(OutputKind.DIRECT_USB, OutputKind.ANDROID_BIT_PERFECT, OutputKind.AAUDIO_EXCLUSIVE),
            output.decision.alternativesRejected.map { it.kind },
        )
        assertTrue(output.decision.alternativesRejected.all { it.reason == RejectionReason.ROUTE_UNSUITABLE })
        // Nothing was opened to find that out.
        assertEquals(listOf("openManaged:default"), platform.calls)
    }

    @Test
    fun a_wireless_route_wins_over_a_usb_one_when_both_are_active() {
        val platform = FakePlatform()
        platform.attach(fakeDac())
        platform.route = listOf(USB_ROUTE, BLUETOOTH_ROUTE)

        val decision = selector(platform).select(PCM_96_24)

        assertEquals(OutputKind.ANDROID_STANDARD, decision.kind)
        assertEquals(BLUETOOTH_ROUTE, decision.route)
    }

    // ---- 4. the standard output, and requests nothing can carry ----

    @Test
    fun dsd_with_no_dac_cannot_be_played_and_the_error_lists_every_rejection() {
        val platform = FakePlatform()

        val error = expectError(DacErrorCode.UNSUPPORTED) { selector(platform).select(StreamRequest.dop(2_822_400, 2)) }

        for (kind in OutputKind.entries) assertTrue(error.message, error.message!!.contains(kind.name))
        assertTrue(platform.calls.isEmpty())
    }

    @Test
    fun an_unknown_route_is_treated_as_the_default_and_the_platform_decides() {
        val platform = FakePlatform()
        platform.route = emptyList()

        val decision = selector(platform).select(PCM_96_24)

        assertEquals(OutputKind.AAUDIO_EXCLUSIVE, decision.kind)
        assertNull(decision.route)
    }

    @Test
    fun the_standard_output_is_opened_on_the_default_route() {
        val platform = FakePlatform()
        platform.grant = fakeGrant(exclusive = false)

        val output = selector(platform).open(PCM_44_16)

        assertEquals(OutputKind.ANDROID_STANDARD, output.kind)
        assertEquals(listOf<OutputRoute?>(null), platform.managed.map { it.second })
        assertEquals(WriteFormat(44_100, 2, 2, 16, 44_100), output.format)
    }

    // ---- force ----

    @Test
    fun a_forced_output_is_used_whatever_would_have_been_chosen() {
        val platform = FakePlatform()
        platform.attach(fakeDac())

        val decision = selector(platform, OutputPolicy(force = OutputKind.ANDROID_STANDARD)).select(PCM_96_24)

        assertEquals(OutputKind.ANDROID_STANDARD, decision.kind)
        assertTrue(decision.alternativesRejected.isEmpty())
        assertTrue("nothing else is looked at", platform.calls.isEmpty())
    }

    @Test
    fun every_kind_can_be_forced() {
        for (kind in OutputKind.entries) {
            val platform = FakePlatform()
            platform.attach(fakeDac())
            platform.route = listOf(USB_ROUTE)

            val output = selector(platform, OutputPolicy(force = kind)).open(PCM_96_24)

            assertEquals(kind, output.kind)
            output.close()
        }
    }

    @Test
    fun a_forced_output_that_cannot_be_opened_fails_with_its_typed_error_and_nothing_else_is_used() {
        val noPermission = FakePlatform()
        noPermission.attach(fakeDac(), permission = false)
        expectError(DacErrorCode.PERMISSION_DENIED) {
            selector(noPermission, OutputPolicy(force = OutputKind.DIRECT_USB)).open(PCM_96_24)
        }
        assertTrue(noPermission.managed.isEmpty())

        val implicit = FakePlatform()
        implicit.attach(fakeDac()).onNegotiate = { throw DacException(DacErrorCode.IMPLICIT_FEEDBACK_UNSUPPORTED, "no feedback") }
        expectError(DacErrorCode.IMPLICIT_FEEDBACK_UNSUPPORTED) {
            selector(implicit, OutputPolicy(force = OutputKind.DIRECT_USB)).select(PCM_96_24)
        }
        assertTrue(implicit.links.values.all { it.isClosed })

        val shared = FakePlatform()
        shared.grant = fakeGrant(exclusive = false)
        expectError(DacErrorCode.EXCLUSIVE_UNAVAILABLE) {
            selector(shared, OutputPolicy(force = OutputKind.AAUDIO_EXCLUSIVE)).open(PCM_96_24)
        }
        assertTrue(shared.offers.single().closed)
        assertTrue(shared.managed.isEmpty())

        val old = FakePlatform()
        old.apiLevel = 33
        old.route = listOf(USB_ROUTE)
        expectError(DacErrorCode.UNSUPPORTED) {
            selector(old, OutputPolicy(force = OutputKind.ANDROID_BIT_PERFECT)).select(PCM_96_24)
        }

        val noDac = FakePlatform()
        expectError(DacErrorCode.DEVICE_LOST) {
            selector(noDac, OutputPolicy(force = OutputKind.DIRECT_USB)).select(PCM_96_24)
        }
    }

    @Test
    fun a_forced_output_that_fails_on_open_does_not_fall_through() {
        val platform = FakePlatform()
        val link = platform.attach(fakeDac())
        link.openStreamError = DacException(DacErrorCode.CLOCK_UNVERIFIED, "the DAC did not confirm the rate")

        expectError(DacErrorCode.CLOCK_UNVERIFIED) { selector(platform, OutputPolicy(force = OutputKind.DIRECT_USB)).open(PCM_96_24) }

        assertTrue(link.isClosed)
        assertTrue(platform.managed.isEmpty())
    }

    @Test
    fun forcing_direct_output_while_forbidding_it_is_a_contradiction() {
        try {
            OutputPolicy(allowDirectUsb = false, force = OutputKind.DIRECT_USB)
            fail("expected an IllegalArgumentException")
        } catch (_: IllegalArgumentException) {
            // as it should
        }
    }

    // ---- one output at a time ----

    @Test
    fun a_second_selection_while_an_output_is_open_is_refused_until_it_is_closed() {
        val platform = FakePlatform()
        platform.attach(fakeDac())
        val selector = selector(platform)

        val output = selector.open(PCM_96_24)
        expectError(DacErrorCode.INVALID_STATE) { selector.select(PCM_96_24) }
        expectError(DacErrorCode.INVALID_STATE) { selector.open(PCM_96_24) }

        output.close()
        assertEquals(OutputKind.DIRECT_USB, selector.select(PCM_96_24).kind)
        selector.open(PCM_44_16).close()
    }

    @Test
    fun a_look_does_not_block_the_next_one() {
        val platform = FakePlatform()
        val selector = selector(platform)

        selector.select(PCM_96_24)
        selector.select(PCM_96_24)

        assertEquals(2, platform.offers.size)
        assertTrue(platform.offers.all { it.closed && !it.started })
    }

    // ---- the pieces of the decision ----

    @Test
    fun the_decision_carries_every_alternative_in_the_order_it_was_tried() {
        val platform = FakePlatform()
        platform.attach(fakeDac(), permission = false)
        platform.route = listOf(USB_ROUTE)
        platform.bitPerfectSupported = false
        platform.grant = fakeGrant(exclusive = false)

        val decision = selector(platform).select(PCM_96_24)

        assertEquals(OutputKind.ANDROID_STANDARD, decision.kind)
        assertEquals(
            listOf(
                OutputKind.DIRECT_USB to RejectionReason.PERMISSION_MISSING,
                OutputKind.ANDROID_BIT_PERFECT to RejectionReason.REQUEST_NOT_SUPPORTED,
                OutputKind.AAUDIO_EXCLUSIVE to RejectionReason.EXCLUSIVE_NOT_GRANTED,
            ),
            decision.alternativesRejected.map { it.kind to it.reason },
        )
    }
}

class SelectedOutputTest {
    @Test
    fun closeClosesTheOutputAndThenTheSessionEvenWhenTheOutputFails() {
        val order = mutableListOf<String>()
        val failing = object : PcmOutput by RecordingOutput() {
            override fun close() {
                order += "output"
                throw DacException(DacErrorCode.TRANSFER, "the stream failed to close")
            }
        }
        var closedCallbacks = 0
        val decision = OutputDecision(OutputKind.DIRECT_USB, Guarantee.VERIFIED_BY_LIBRARY, "r", null, null, null, null, emptyList())
        val output = SelectedOutput(
            failing, decision, WriteFormat(44_100, 2, 2, 16, 44_100), null, null, null,
            releaseSession = { order += "session" },
            onClosed = { closedCallbacks++ },
        )

        val error = try {
            output.close()
            null
        } catch (error: DacException) {
            error
        }

        assertEquals(listOf("output", "session"), order)
        assertEquals(DacErrorCode.TRANSFER, error?.code)
        assertEquals(1, closedCallbacks)
        output.close()
        assertEquals("closing again does nothing", listOf("output", "session"), order)
    }
    @Test
    fun wideningOnlyRequiresLosslessPlanAndNeverTriesAnotherRateOrNarrowerDepth() {
        for (proof in listOf("bit_exact", "lossy")) {
            val platform = FakePlatform()
            val link = platform.attach(fakeDac())
            link.capabilities = fakeCapabilities(depths = listOf(16, 24, 32))
            link.onNegotiate = { request ->
                if (request.bitDepth != 32) throw unsupported()
                fakeFormat(request.sampleRate, 32, 4)
            }
            platform.planKind = proof
            val decision = selector(platform, OutputPolicy(allowConversion = false, allowWidening = true)).select(PCM_44_16)
            assertEquals(if (proof == "bit_exact") OutputKind.DIRECT_USB else OutputKind.AAUDIO_EXCLUSIVE, decision.kind)
            assertTrue(link.negotiated.all { it.ratePolicy == RatePolicy.EXACT })
            assertTrue(link.negotiated.all { it.bitDepth >= 16 })
        }
    }

}
