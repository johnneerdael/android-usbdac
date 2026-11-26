package dev.usbdac.internal

import dev.usbdac.CaptureChannel
import dev.usbdac.CaptureChannelPosition
import dev.usbdac.CaptureChannelTopology
import dev.usbdac.CaptureRequest
import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test

class CaptureChannelIdentityTest {
    @Test fun adc3IdentitiesPreserveRawDescriptorFieldsAcrossCloseAndReopenRequests() {
        val channels = listOf(
            CaptureChannel(0, CaptureChannelPosition.ADC3_INFORMATION, CaptureChannelTopology.Information(145, 131, 7)),
            CaptureChannel(1, CaptureChannelPosition.ADC3_AMBISONIC, CaptureChannelTopology.Ambisonic(2, 3, 1, 9)))
        val request = JSONObject(CaptureRequest(48000, 2, 24, channels).toCaptureJson())
        val format = request.put("subslot_bytes", 4).put("bytes_per_frame", 8).put("configuration_id", 2)
            .put("interface_number", 1).put("alt_setting", 1).put("endpoint_address", 129)
        assertEquals(channels, parseCaptureFormat(format.toString()).channelMap)
        val topology = format.getJSONArray("channel_map").getJSONObject(0).getJSONObject("topology")
        topology.put("purpose", 256)
        assertTrue(runCatching { parseCaptureFormat(format.toString()) }.isFailure)
    }
    @Test fun adc3LabelsRequireMatchingTopologyInsteadOfGuessingSpatialMeaning() {
        val channel = CaptureChannel(0, CaptureChannelPosition.ADC3_INFORMATION)
        assertTrue(runCatching { CaptureRequest(48000, 1, 24, listOf(channel)).toCaptureJson() }.isFailure)
    }
    @Test fun playbackUsesTheCaptureWireIdentityDecoder() {
        val channels = listOf(
            CaptureChannel(0, CaptureChannelPosition.BACK_LEFT),
            CaptureChannel(1, CaptureChannelPosition.UNSPECIFIED))
        val value = JSONObject(CaptureRequest(48000, 2, 24, channels).toCaptureJson())
            .put("format_id", 1).put("subslot_bytes", 3).put("bytes_per_frame", 6)
            .put("counter_rate_hz", 48000).put("interface_number", 1).put("alt_setting", 1)
            .put("endpoint_address", 1).put("sync_type", "synchronous").put("explicit_feedback", false)
            .put("link_speed", "high").put("service_interval_us", 125).put("rate_differs_from_request", false)
        assertEquals(channels, parseStreamFormat(value.toString()).channelMap)
        value.getJSONArray("channel_map").getJSONObject(1).put("index", 0)
        assertTrue(runCatching { parseStreamFormat(value.toString()) }.isFailure)
        value.remove("channel_map")
        assertEquals(listOf(CaptureChannel(0, CaptureChannelPosition.UNSPECIFIED),
            CaptureChannel(1, CaptureChannelPosition.UNSPECIFIED)), parseStreamFormat(value.toString()).channelMap)
    }
}
