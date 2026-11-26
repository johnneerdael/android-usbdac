package dev.usbdac

import android.hardware.usb.UsbDevice
import dev.usbdac.internal.AAudioOffer
import dev.usbdac.internal.DirectLink
import dev.usbdac.internal.OutputPlatform
import kotlinx.coroutines.flow.Flow
import kotlinx.coroutines.flow.MutableSharedFlow

// The device and the native layer, faked, for the tests of the output selector.

/** An Android `UsbDevice` has no public constructor; the mockable android.jar lets the hidden one be called. */
private fun usbDevice(): UsbDevice {
    val constructor = UsbDevice::class.java.getDeclaredConstructor()
    constructor.isAccessible = true
    return constructor.newInstance()
}

internal fun fakeDac(name: String = "Test DAC", deviceName: String = "/dev/bus/usb/001/002"): DacDeviceInfo =
    DacDeviceInfo(usbDevice(), 0x1234, 0x5678, name, null, deviceName)

internal val USB_ROUTE = OutputRoute(RouteKind.USB, "USB DAC", 11)
internal val SPEAKER_ROUTE = OutputRoute(RouteKind.BUILT_IN, "speaker", 2)
internal val HEADPHONES_ROUTE = OutputRoute(RouteKind.WIRED, "headphones", 4)
internal val BLUETOOTH_ROUTE = OutputRoute(RouteKind.WIRELESS, "Pixel Buds", 8)

internal fun fakeFormat(
    rate: Int = 96_000,
    bits: Int = 24,
    subslot: Int = 3,
    channels: Int = 2,
    syncType: SyncType = SyncType.ASYNCHRONOUS,
    explicitFeedback: Boolean = true,
    pacing: Pacing = Pacing.ExplicitFeedback(0x81),
    rateDiffers: Boolean = false,
): StreamFormat = StreamFormat(
    formatId = 1,
    encoding = StreamEncoding.PCM,
    dsdBitRate = null,
    sampleRate = rate,
    channels = channels,
    validBits = bits,
    subslotBytes = subslot,
    bytesPerFrame = subslot * channels,
    counterRateHz = rate,
    interfaceNumber = 1,
    altSetting = 1,
    endpointAddress = 1,
    syncType = syncType,
    explicitFeedback = explicitFeedback,
    linkSpeed = LinkSpeed.HIGH,
    serviceIntervalUs = 125,
    rateDiffersFromRequest = rateDiffers,
    pacing = pacing,
)

internal fun fakeCapabilities(depths: List<Int> = listOf(16, 24, 32)): DacCapabilities = DacCapabilities(
    uacVersion = UacVersion.V2,
    linkSpeed = LinkSpeed.HIGH,
    altSettings = emptyList(),
    pcmSampleRates = listOf(44_100, 48_000, 96_000),
    pcmBitDepths = depths,
    channelCounts = listOf(2),
    supportsPcm = true,
    supportsDop = false,
    dopMaxDsdBitRate = null,
    supportsNativeDsd = false,
    nativeDsdBitRates = emptyList(),
    hasExplicitFeedback = true,
    volume = VolumeRange(-127.0, 0.0, 1.0),
    hasHardwareVolume = true,
    hasMute = true,
)

internal fun fakeGrant(
    exclusive: Boolean = true,
    asRequested: Boolean = true,
    reason: String = "granted",
): AAudioGrant = AAudioGrant(
    sharingMode = if (exclusive) AAudioSharingMode.EXCLUSIVE else AAudioSharingMode.SHARED,
    sampleRate = 96_000,
    channels = 2,
    format = AAudioSampleFormat.PCM_24_PACKED,
    framesPerBurst = 96,
    bufferCapacityFrames = 960,
    bufferSizeFrames = 192,
    deviceId = 7,
    performanceMode = AAudioPerformanceMode.LOW_LATENCY,
    usesMmap = true,
    hardware = null,
    asRequested = asRequested,
    verdict = if (exclusive) "exclusive" else "shared",
    reason = reason,
)

/** A DAC opened for direct output. What it answers is set by the test. */
internal class FakeLink(override val dac: DacDeviceInfo) : DirectLink {
    override var capabilities: DacCapabilities = fakeCapabilities()
    override var hasUsableVolume: Boolean = true
    override val volume: DacVolume? = null
    override val session: DacSession? = null
    override var stream: DacStream? = null

    /** What `negotiate` returns for a request; throw a [DacException] to fail. */
    var onNegotiate: (StreamRequest) -> StreamFormat = { request -> fakeFormat(request.sampleRate, request.bitDepth, request.bitDepth / 8) }
    var openStreamError: DacException? = null
    val negotiated = mutableListOf<StreamRequest>()
    val streams = mutableListOf<RecordingOutput>()
    var closeCount = 0
    var openCount = 0

    override fun negotiate(request: StreamRequest): StreamFormat {
        negotiated += request
        return onNegotiate(request)
    }

    override fun openStream(format: StreamFormat): PcmOutput {
        openStreamError?.let { throw it }
        return RecordingOutput().also { streams += it }
    }

    override fun close() {
        closeCount++
    }

    val isClosed: Boolean get() = closeCount > 0
}

/** An AAudio stream the platform granted and that is not started yet. */
internal class FakeOffer(override val grant: AAudioGrant) : AAudioOffer {
    var started = false
    var closed = false
    val output = RecordingOutput()

    override fun start(): PcmOutput {
        started = true
        return output
    }

    override fun close() {
        closed = true
    }
}

internal class FakePlatform : OutputPlatform {
    override var apiLevel: Int = 34
    var dacs: List<DacDeviceInfo> = emptyList()
    val permitted = mutableSetOf<String>()
    var route: List<OutputRoute> = listOf(SPEAKER_ROUTE)
    var bitPerfectSupported = true

    /** What the output made by `openManaged` reports when it is given a USB route. */
    var managedUsbState: BitPerfectState = BitPerfectState.RequestedViaPlatform("accepted")

    val links = mutableMapOf<String, FakeLink>()
    var openDacError: DacException? = null
    var aaudioError: DacException? = null
    var grant: AAudioGrant = fakeGrant()
    var planKind = "lossy"
    var planDescription = "Lossy conversion, not bit-perfect: resampled."

    val calls = mutableListOf<String>()
    val offers = mutableListOf<FakeOffer>()
    val managed = mutableListOf<Pair<RecordingOutput, OutputRoute?>>()
    val aaudioConfigs = mutableListOf<AAudioConfig>()

    val hotplugFlow = MutableSharedFlow<DacHotplugEvent>(extraBufferCapacity = 16)
    val routeFlow = MutableSharedFlow<OutputChange.RouteChanged>(extraBufferCapacity = 16)

    fun attach(dac: DacDeviceInfo, permission: Boolean = true): FakeLink {
        dacs = dacs + dac
        if (permission) permitted += dac.deviceName
        return links.getOrPut(dac.deviceName) { FakeLink(dac) }
    }

    override fun attachedDacs(): List<DacDeviceInfo> = dacs

    override fun hasPermission(dac: DacDeviceInfo): Boolean = dac.deviceName in permitted

    override fun openDac(dac: DacDeviceInfo, config: DacConfig): DirectLink {
        calls += "openDac:${dac.deviceName}"
        openDacError?.let { throw it }
        return links.getOrPut(dac.deviceName) { FakeLink(dac) }.also { it.openCount++ }
    }

    override fun activeRoute(): List<OutputRoute> = route

    override fun supportsBitPerfect(route: OutputRoute, request: StreamRequest): Boolean {
        calls += "supportsBitPerfect:${route.name}"
        return bitPerfectSupported
    }

    override fun openManaged(request: StreamRequest, route: OutputRoute?): PcmOutput {
        calls += "openManaged:${route?.name ?: "default"}"
        val output = RecordingOutput()
        if (route?.kind == RouteKind.USB) output.setBitPerfect(managedUsbState)
        else output.setBitPerfect(BitPerfectState.Unverifiable("no bit-perfect route"))
        managed += output to route
        return output
    }

    override fun probeAAudio(request: StreamRequest, config: AAudioConfig): AAudioOffer {
        calls += "probeAAudio"
        aaudioConfigs += config
        aaudioError?.let { throw it }
        return FakeOffer(grant).also { offers += it }
    }

    override fun plan(source: StreamRequest, target: StreamFormat): ConversionPlan {
        calls += "plan"
        return ConversionPlan.between(
            FakeConvertBackend(planJson(kind = planKind, description = planDescription)),
            source,
            target,
            ConversionOptions(),
        )
    }

    override fun converting(target: PcmOutput, plan: ConversionPlan): PcmOutput =
        ConvertingOutput(target, PcmConverter.create(plan))

    override fun hotplug(): Flow<DacHotplugEvent> = hotplugFlow

    override fun routeChanges(): Flow<OutputChange.RouteChanged> = routeFlow
}

internal fun unsupported(message: String = "no alternate setting carries the request"): DacException =
    DacException(DacErrorCode.UNSUPPORTED, message)
