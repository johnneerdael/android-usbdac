package dev.usbdac

import dev.usbdac.internal.NativeRateRanking
import dev.usbdac.internal.OutputPlatform
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
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
}
