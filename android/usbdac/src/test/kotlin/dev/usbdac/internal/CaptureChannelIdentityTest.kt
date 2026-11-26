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
}
