package dev.usbdac.internal

import dev.usbdac.AAudioConfig
import dev.usbdac.AAudioGrant
import dev.usbdac.ConversionOptions
import dev.usbdac.ConversionPlan
import dev.usbdac.DacCapabilities
import dev.usbdac.DacConfig
import dev.usbdac.DacDeviceInfo
import dev.usbdac.DacHotplugEvent
import dev.usbdac.DacSession
import dev.usbdac.DacStream
import dev.usbdac.DacVolume
import dev.usbdac.OutputChange
import dev.usbdac.OutputRoute
import dev.usbdac.PcmOutput
import dev.usbdac.StreamFormat
import dev.usbdac.StreamRequest
import kotlinx.coroutines.flow.Flow
import java.io.Closeable

// What the output selector needs from Android and from the native layer, as small interfaces, so the
// whole decision table runs on the JVM against fakes. The real implementation is AndroidOutputPlatform.

/** A DAC opened for direct output: the session, behind the calls the selector makes. */
internal interface DirectLink : Closeable {
    val dac: DacDeviceInfo
    val capabilities: DacCapabilities

    /** The DAC has a hardware volume the library could verify; without one `openStream` fails when [dev.usbdac.DacConfig.requireHardwareVolume] is on. */
    val hasUsableVolume: Boolean
    val volume: DacVolume?

    /** The session; `null` only for a fake. */
    val session: DacSession?

    /** The stream opened by [openStream]; `null` before that, and for a fake. */
    val stream: DacStream?

    /** Programs the DAC's clock and settles the wire format. 100 ms or more. */
    fun negotiate(request: StreamRequest): StreamFormat

    /** Opens the stream in a format [negotiate] returned. */
    fun openStream(format: StreamFormat): PcmOutput

    /** Closes the stream and the session and hands the DAC back to Android. */
    override fun close()
}

/** An AAudio stream the platform granted for a request and that has not been started: silent, and holding the device until [start] or [close]. */
internal interface AAudioOffer : Closeable {
    val grant: AAudioGrant

    /** Starts the stream and returns it as an output. The offer is spent. */
    fun start(): PcmOutput

    /** Releases the stream if it was not started. */
    override fun close()
}

internal interface OutputPlatform {
    /** `Build.VERSION.SDK_INT`. */
    val apiLevel: Int

    // ---- USB DACs ----

    fun attachedDacs(): List<DacDeviceInfo>
    fun hasPermission(dac: DacDeviceInfo): Boolean

    fun usbSerial(dac: DacDeviceInfo): String? =
        try {
            dac.device.serialNumber?.takeIf { it.isNotBlank() }
        } catch (_: SecurityException) {
            null
        }

    /** Opens a session on [dac]; the failures of `UsbDac.open`. */
    fun openDac(dac: DacDeviceInfo, config: DacConfig): DirectLink

    /** Host test platforms can order synthetic rates without loading Android JNI. */
    fun rankRates(requested: Int, rates: List<Int>): List<Int> {
        fun family(rate: Int): Int = when {
            rate > 0 && rate % 11025 == 0 -> 1
            rate > 0 && rate % 8000 == 0 -> 2
            else -> 0
        }
        return rates.distinct().filter { it > 0 }.sortedWith(compareBy<Int>(
            { rate -> when {
                rate == requested -> 0
                rate < requested && requested % rate == 0 -> 1
                rate < requested && family(rate) != 0 && family(rate) == family(requested) -> 2
                rate < requested -> 3
                else -> 4
            } },
            { rate -> if (rate <= requested) -rate.toLong() else rate.toLong() },
        ))
    }

    // ---- audio routing ----

    /** The output devices Android sends media to now; empty when it cannot tell. */
    fun activeRoute(): List<OutputRoute>

    /** Whether the platform offers its bit-perfect mixer behavior for [route] in exactly [request]'s format. Asks only; sets nothing. */
    fun supportsBitPerfect(route: OutputRoute, request: StreamRequest): Boolean

    /** An `AudioTrack` output, routed to [route] (and asked for the bit-perfect behavior if it is a USB device) or to the default. */
    fun openManaged(request: StreamRequest, route: OutputRoute?): PcmOutput

    /** Opens an exclusive AAudio stream on the default route and reads the grant, without starting it. */
    fun probeAAudio(request: StreamRequest, config: AAudioConfig): AAudioOffer

    // ---- conversion ----

    fun plan(source: StreamRequest, target: StreamFormat): ConversionPlan
    fun plan(source: StreamRequest, target: StreamFormat, options: ConversionOptions): ConversionPlan = plan(source, target)
    fun converting(target: PcmOutput, plan: ConversionPlan): PcmOutput

    // ---- changes ----

    fun hotplug(): Flow<DacHotplugEvent>
    fun routeChanges(): Flow<OutputChange.RouteChanged>
}
