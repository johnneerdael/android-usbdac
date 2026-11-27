package dev.usbdac

import android.content.Context
import dev.usbdac.internal.AAudioOffer
import dev.usbdac.internal.AndroidOutputPlatform
import dev.usbdac.internal.DirectLink
import dev.usbdac.internal.OutputPlatform
import kotlinx.coroutines.flow.Flow
import kotlinx.coroutines.flow.map
import kotlinx.coroutines.flow.merge
import java.util.concurrent.atomic.AtomicBoolean
import java.util.concurrent.atomic.AtomicReference

/**
 * The output [OutputSelector.open] chose and opened: a [PcmOutput] ready to take audio, with the
 * [decision] that explains it. Write to it and control it as any [PcmOutput]; [close] closes
 * everything the selector opened for it, the DAC session included.
 *
 * What to write is [format]. For the direct output the [session], [stream] and [volume] are there so
 * the hardware volume, the volume keys and the statistics keep working; they are `null` for every other
 * kind (the system volume applies then).
 */
public class SelectedOutput internal constructor(
    private val output: PcmOutput,
    /** What was chosen and why. */
    public val decision: OutputDecision,
    /** The layout to write and the unit of the position counters. */
    public val format: WriteFormat,
    /** The open DAC session, for [OutputKind.DIRECT_USB]; `null` otherwise. Closed by [close]. */
    public val session: DacSession?,
    /** The direct stream, for [OutputKind.DIRECT_USB]; `null` otherwise. Behind a converter it is the converter's target. */
    public val stream: DacStream?,
    /** The DAC's hardware volume, for [OutputKind.DIRECT_USB] when the DAC has one; `null` otherwise. */
    public val volume: DacVolume?,
    private val releaseSession: () -> Unit,
    private val onClosed: (SelectedOutput) -> Unit,
) : PcmOutput by output {

    private val closed = AtomicBoolean(false)

    /** What was chosen: shorthand for `decision.kind`. */
    public val kind: OutputKind get() = decision.kind

    /** Samples clipped by the selector's PCM converter, or null without one. */
    public val clippedSamples: Long? get() = when (output) {
        is ConvertingOutput -> output.clippedSamples
        is DsdPcmOutput -> output.clippedSamples
        is DopInputOutput -> output.clippedSamples
        else -> null
    }

    /**
     * Stops the output and releases everything the selector opened for it: the stream, the converter,
     * and for the direct output the DAC session, which hands the DAC back to Android. Safe to call again.
     * It may take a few hundred milliseconds: call it off the main thread.
     */
    override fun close() {
        if (!closed.compareAndSet(false, true)) return
        var failure: Throwable? = null
        try {
            output.close()
        } catch (error: Throwable) {
            failure = error
        }
        try {
            releaseSession()
        } catch (error: Throwable) {
            if (failure == null) failure = error else failure.addSuppressed(error)
        } finally {
            onClosed(this)
        }
        failure?.let { throw it }
    }
}

/** A pre-track USB session. Close it to release the DAC unless ownership has moved to an output.
 * [volume] is the session's hardware volume, or null when unavailable. Once transferred, use
 * [SelectedOutput.volume]; closing this handle no longer closes the output.
 */
public class PreparedUsbOutput internal constructor(
    public val device: DacDeviceInfo,
    public val volume: DacVolume?,
    private val release: () -> Unit,
) : java.io.Closeable {
    override fun close(): Unit = release()
}

/**
 * Looks at the device and gives the app one ready [PcmOutput], with the reason for it and the
 * guarantee that applies, so the app does not have to choose between [DacStream],
 * [AndroidManagedOutput] and [AAudioExclusiveOutput] itself.
 *
 * ### The order
 * The first output in this list that works for the request is chosen. Each one that is passed over
 * is recorded in [OutputDecision.alternativesRejected] with the reason.
 *
 * 1. **[OutputKind.DIRECT_USB]**, if [OutputPolicy.allowDirectUsb] is on, a USB DAC is attached
 *    ([UsbDac.attachedDacs]), the app holds USB permission for it, the audio is not routed to Bluetooth
 *    or a cast device, and the DAC can carry the request: a session opens, the DAC has a usable hardware
 *    volume (unless [DacConfig.requireHardwareVolume] is off), and [DacSession.negotiate] settles the
 *    format. With [OutputPolicy.allowConversion] a request the DAC does not run as it is is carried
 *    through a [ConversionPlan] to a ranked supported format; the decision says so and the
 *    guarantee drops unless every sample value is kept. If the permission is missing the DAC is not
 *    used and the decision carries [OutputDecision.needsUsbPermission]; the selector never asks.
 *    An implicit-feedback-only DAC, a DAC without hardware volume, a busy or unclaimable DAC and any
 *    other failure to open or negotiate are rejected with their typed reason.
 * 2. **[OutputKind.ANDROID_BIT_PERFECT]**, if audio is routed to a USB device, the Android release is 14
 *    (API 34) or later, and the platform offers its bit-perfect mixer behavior for exactly the request's
 *    format on that device.
 * 3. **[OutputKind.AAUDIO_EXCLUSIVE]**, if audio is not routed to Bluetooth or a cast device and an
 *    exclusive AAudio stream at exactly the request's rate, channel count and sample format is granted
 *    (24-bit and 32-bit integer samples need Android 12). This is how a player with an internal DAC
 *    is recognised: by what the platform grants, not by a list of devices. The stream is opened to read
 *    the grant, not started, and kept for [open]. On a USB route it is tried only when
 *    [OutputPolicy.allowDirectUsb] is on, because an exclusive stream keeps the DAC from other apps.
 * 4. **[OutputKind.ANDROID_STANDARD]**, an ordinary `AudioTrack`, for PCM that Android can carry. A
 *    Bluetooth or cast route goes straight here.
 *
 * Nothing carries DSD except the direct output. If no output can carry the request, [select] and
 * [open] throw [DacErrorCode.UNSUPPORTED] with every rejection in the message.
 * [OutputPolicy.force] skips all this and uses one output or fails with its typed error.
 *
 * ### What `select` and `open` cost
 * Both **block**: call them off the main thread. They have to look before they decide, so with a DAC
 * attached they open a session and negotiate (100 ms or more, and the DAC's clock is programmed), and on
 * a route that may be exclusive they open an AAudio stream (up to about half a second to close). [select]
 * closes whatever it opened again and makes no sound; [open] keeps the winner. Call [select] when
 * nothing is playing (a settings screen, before the first track); call [open] to play.
 *
 * ### Re-selecting
 * A selector holds at most one output: the DAC and an exclusive stream can only be held once, so
 * while the output [open] returned is not closed, [select] and [open] fail with
 * [DacErrorCode.INVALID_STATE]. The selector **never switches a stream that is playing**. Watch
 * [changes]; at the next track boundary, if one has arrived or the next track's format differs, drain or
 * close the output and call [open] again. Otherwise keep writing to the one you have.
 *
 * Not verified on hardware: nothing here has run on a device, in particular how real players answer the
 * AAudio probe.
 */
public class OutputSelector internal constructor(
    private val platform: OutputPlatform,
    /** What the selector may do. */
    public val policy: OutputPolicy,
) {
    /** A selector for the device [context] runs on. */
    @JvmOverloads
    public constructor(context: Context, policy: OutputPolicy = OutputPolicy()) :
        this(AndroidOutputPlatform(context), policy)

    private val lock = Any()
    private val active = AtomicReference<SelectedOutput?>(null)
    private class Preparation(val link: DirectLink, val handle: PreparedUsbOutput)
    private var preparation: Preparation? = null

    /**
     * Open an explicitly chosen, already permitted DAC before the first track, to control its
     * hardware volume. Blocks and claims the device; no permission prompt is shown. The next [open]
     * uses this exact session for direct playback. A failed negotiation or open closes the session
     * and reports the error; it never falls back to another output. Close the returned handle to
     * cancel, or close the resulting [SelectedOutput] after ownership transfers.
     */
    public fun prepareUsb(dac: DacDeviceInfo): PreparedUsbOutput = synchronized(lock) {
        requireIdle()
        if (!policy.allowDirectUsb || (policy.force != null && policy.force != OutputKind.DIRECT_USB)) {
            throw DacException(DacErrorCode.INVALID_STATE, "direct USB conflicts with output policy")
        }
        if (platform.activeRoute().any { it.kind == RouteKind.WIRELESS }) {
            throw DacException(DacErrorCode.INVALID_STATE, "direct USB conflicts with the wireless route")
        }
        if (platform.attachedDacs().none { (it.device === dac.device || it.device == dac.device) && it.deviceName == dac.deviceName &&
                it.vendorId == dac.vendorId && it.productId == dac.productId }) {
            throw DacException(DacErrorCode.DEVICE_LOST, "the selected DAC is no longer attached")
        }
        if (!platform.hasPermission(dac)) {
            throw DacException(DacErrorCode.PERMISSION_DENIED, "no USB permission for ${dac.productName}")
        }
        val link = platform.openDac(dac, policy.dacConfig)
        try {
            if (policy.dacConfig.requireHardwareVolume && !link.hasUsableVolume) {
                throw DacException(DacErrorCode.VOLUME_UNAVAILABLE, "the selected DAC has no usable hardware volume")
            }
            lateinit var handle: PreparedUsbOutput
            handle = PreparedUsbOutput(dac, link.volume) {
                synchronized(lock) {
                    if (preparation?.handle === handle) {
                        preparation = null
                        link.close()
                    }
                }
            }
            preparation = Preparation(link, handle)
            handle
        } catch (error: Throwable) {
            try { link.close() } catch (close: Throwable) { error.addSuppressed(close) }
            throw error
        }
    }

    /**
     * Changes that may make another output the best one: a DAC attached, detached or given its USB
     * permission, or an audio output device that appeared or went away. A cold flow: collecting
     * registers the receivers, cancelling removes them. Events from the USB receiver arrive on the
     * main thread. One physical event can raise several (plugging in a DAC raises an attach and a route
     * change): treat each as "look again", and debounce.
     *
     * The selector never acts on these itself. A playing stream stays as it is; re-select at the next
     * track boundary (see the class documentation).
     */
    public val changes: Flow<OutputChange> = merge<OutputChange>(
        platform.hotplug().map<DacHotplugEvent, OutputChange> { event ->
            when (event) {
                is DacHotplugEvent.Attached -> OutputChange.DacAttached(event.device)
                is DacHotplugEvent.Detached -> OutputChange.DacDetached(event.device)
                is DacHotplugEvent.PermissionGranted -> OutputChange.PermissionGranted(event.device)
            }
        },
        platform.routeChanges(),
    )

    /**
     * Decides which output [open] would use for [request], and opens nothing that stays open: a DAC
     * session or an AAudio stream opened for the look is closed before this returns, and no sound is made.
     * Blocks (see the class documentation).
     *
     * [StreamRequest.ratePolicy] is ignored: whether a rate the DAC does not run is converted is
     * [OutputPolicy.allowConversion]'s decision.
     *
     * @throws DacException [DacErrorCode.UNSUPPORTED] when no output can carry the request;
     *   with [OutputPolicy.force], the typed error of that output; [DacErrorCode.INVALID_STATE]
     *   while an output of this selector is open.
     */
    public fun select(request: StreamRequest): OutputDecision = synchronized(lock) {
        requireIdle()
        resolve(request, keep = false).decision
    }

    /**
     * Decides as [select] does and opens the winner. The output that comes back is ready to take audio
     * in [SelectedOutput.format]; closing it closes everything that was opened. If the winner fails at
     * the last step (the stream will not open), it is recorded as rejected and the next output is tried, unless
     * [OutputPolicy.force] is set. Blocks (see the class documentation).
     *
     * @throws DacException as [select].
     */
    public fun open(request: StreamRequest): SelectedOutput = synchronized(lock) {
        val prepared = preparation
        if (prepared == null) {
            requireIdle()
            openResolved(resolve(request, keep = true))
        } else {
            // The selector owns the link until commit succeeds; then SelectedOutput owns it.
            try {
                val dac = prepared.handle.device
                if (platform.activeRoute().any { it.kind == RouteKind.WIRELESS } ||
                    platform.attachedDacs().none { (it.device === dac.device || it.device == dac.device) && it.deviceName == dac.deviceName &&
                        it.vendorId == dac.vendorId && it.productId == dac.productId } ||
                    !platform.hasPermission(dac) || prepared.link.session?.state?.value?.let { it != DacState.READY } == true) {
                    throw DacException(DacErrorCode.DEVICE_LOST, "the prepared DAC is no longer available for direct playback")
                }
                val pass = Pass(request, RouteView(platform.activeRoute()), null)
                val candidate = directCandidate(request, dac, prepared.link, negotiate(prepared.link, request))
                val resolved = Resolution(decisionOf(candidate, pass, emptyList()), candidate.commit())
                preparation = null
                openResolved(resolved)
            } catch (error: Throwable) {
                preparation = null
                try { prepared.link.close() } catch (close: Throwable) { error.addSuppressed(close) }
                throw error
            }
        }
    }

    internal fun openMatchingUsb(request: StreamRequest, matches: (DacDeviceInfo) -> Boolean): SelectedOutput =
        synchronized(lock) {
            requireIdle()
            openResolved(resolve(request, keep = true, matchesUsb = matches))
        }

    internal fun openForHandoff(request: StreamRequest, kind: OutputKind): SelectedOutput = synchronized(lock) {
        requireIdle()
        if (policy.force != null && policy.force != kind) {
            throw DacException(DacErrorCode.INVALID_STATE, "the handoff target conflicts with the forced output policy")
        }
        openResolved(resolve(request, keep = true, forcedKind = kind))
    }

    internal fun hasAttachedUsb(matches: (DacDeviceInfo) -> Boolean): Boolean =
        platform.attachedDacs().any(matches)

    internal fun usbSerial(dac: DacDeviceInfo): String? =
        if (platform.hasPermission(dac)) platform.usbSerial(dac) else null

    private fun openResolved(resolved: Resolution): SelectedOutput {
        val opened = checkNotNull(resolved.opened)
        return SelectedOutput(
            output = opened.output,
            decision = resolved.decision,
            format = opened.format,
            session = opened.session,
            stream = opened.stream,
            volume = opened.volume,
            releaseSession = opened.releaseSession,
            onClosed = { done -> active.compareAndSet(done, null) },
        ).also { active.set(it) }
    }

    private fun requireIdle() {
        if (active.get() != null || preparation != null) {
            throw DacException(
                DacErrorCode.INVALID_STATE,
                "close the output or prepared DAC this selector owns before selecting again",
            )
        }
    }

    // ---- the decision ----

    private class Opened(
        val output: PcmOutput,
        val format: WriteFormat,
        val session: DacSession? = null,
        val stream: DacStream? = null,
        val volume: DacVolume? = null,
        /** Gives back what the output sits on: the DAC session. */
        val releaseSession: () -> Unit = {},
    )

    private class Resolution(val decision: OutputDecision, val opened: Opened?)

    /** An output that can be used, looked at but not yet opened for good. */
    private class Candidate(
        val kind: OutputKind,
        val guarantee: Guarantee,
        val reason: String,
        val dac: DacDeviceInfo? = null,
        val conversion: ConversionPlan? = null,
        val wireFormat: StreamFormat? = null,
        val processing: OutputConversion? = null,
        /** Opens it for good. */
        val commit: () -> Opened,
        /** Gives back what the look opened; for a candidate that is committed, nothing more to do. */
        val release: () -> Unit = {},
    )

    private sealed interface Attempt {
        class Ready(val candidate: Candidate) : Attempt

        class Rejected(val rejection: Rejection, val cause: DacException?) : Attempt
    }

    /** The routes media is sent to, sorted into what matters here. */
    private class RouteView(val devices: List<OutputRoute>) {
        val wireless: OutputRoute? = devices.firstOrNull { it.kind == RouteKind.WIRELESS }
        val usb: OutputRoute? = devices.firstOrNull { it.kind == RouteKind.USB }
        val primary: OutputRoute? = wireless ?: usb ?: devices.firstOrNull()
    }

    /** What one pass over the candidates learns on the way. */
    private class Pass(
        val request: StreamRequest,
        val route: RouteView,
        val matchesUsb: ((DacDeviceInfo) -> Boolean)?,
    ) {
        var needsPermission: DacDeviceInfo? = null
    }

    private fun resolve(
        request: StreamRequest,
        keep: Boolean,
        matchesUsb: ((DacDeviceInfo) -> Boolean)? = null,
        forcedKind: OutputKind? = null,
    ): Resolution {
        val pass = Pass(request, RouteView(platform.activeRoute()), matchesUsb)
        val rejected = mutableListOf<Rejection>()
        val forced = if (matchesUsb != null) OutputKind.DIRECT_USB else forcedKind ?: policy.force
        var firstFailure: DacException? = null
        for (kind in if (forced != null) listOf(forced) else ORDER) {
            for (attempt in attempts(kind, pass)) {
                when (attempt) {
                    is Attempt.Rejected -> {
                        rejected += attempt.rejection
                        if (firstFailure == null) firstFailure = attempt.cause ?: failureOf(attempt.rejection)
                    }
                    is Attempt.Ready -> {
                        val candidate = attempt.candidate
                        val decision = decisionOf(candidate, pass, rejected)
                        if (!keep) {
                            candidate.release.quietly()
                            return Resolution(decision, null)
                        }
                        try {
                            return Resolution(decision, candidate.commit())
                        } catch (error: DacException) {
                            candidate.release.quietly()
                            rejected += rejectionOf(kind, error, null)
                            if (firstFailure == null) firstFailure = error
                        } catch (error: Throwable) {
                            candidate.release.quietly()
                            throw error
                        }
                    }
                }
            }
        }
        // Nothing worked. A forced output reports its own failure; otherwise every rejection is listed.
        if (forced != null) throw checkNotNull(firstFailure)
        throw DacException(
            DacErrorCode.UNSUPPORTED,
            "no output can play ${describe(request)}: " + rejected.joinToString("; ") { "${it.kind}: ${it.why}" },
        )
    }

    private fun decisionOf(candidate: Candidate, pass: Pass, rejected: List<Rejection>): OutputDecision =
        OutputDecision(
            kind = candidate.kind,
            guarantee = candidate.guarantee,
            reason = candidate.reason,
            route = pass.route.primary,
            dac = candidate.dac,
            conversion = candidate.conversion,
            needsUsbPermission = if (candidate.kind == OutputKind.DIRECT_USB) null else pass.needsPermission,
            alternativesRejected = rejected.toList(),
            source = pass.request,
            wireFormat = candidate.wireFormat,
            processing = candidate.processing,
        )

    private fun attempts(kind: OutputKind, pass: Pass): Sequence<Attempt> {
        if (kind != OutputKind.DIRECT_USB && policy.downstreamLimits != null) {
            return sequenceOf(reject(kind, RejectionReason.POLICY,
                "the platform output cannot verify the configured downstream decoder limits"))
        }
        if (kind != OutputKind.DIRECT_USB && pass.request.encoding in listOf(StreamEncoding.DSD_NATIVE, StreamEncoding.DOP)) {
            return androidDsdAttempts(kind, pass)
        }
        return when (kind) {
        OutputKind.DIRECT_USB -> directAttempts(pass)
        OutputKind.ANDROID_BIT_PERFECT -> sequence { yield(bitPerfectAttempt(pass)) }
        OutputKind.AAUDIO_EXCLUSIVE -> sequence { yield(aaudioAttempt(pass)) }
        OutputKind.ANDROID_STANDARD -> sequence { yield(standardAttempt(pass)) }
        }
    }

    /** PCM targets are finite and lower-first around the DSD intermediate rate. */
    private fun androidDsdAttempts(kind: OutputKind, pass: Pass): Sequence<Attempt> = sequence {
        val source = pass.request
        val bitRate = source.dsdBitRate
        if (!canConvertDsd(source)) {
            yield(reject(kind, RejectionReason.REQUEST_NOT_SUPPORTED,
                "DSD-to-PCM conversion requires supported source format and explicit conversion consent"))
            return@sequence
        }
        val rate = checkNotNull(bitRate)
        val rates = listOf(44_100, 48_000, 88_200, 96_000, 176_400, 192_000, 352_800, 384_000, 705_600)
        for (targetRate in platform.rankRates(rate / 16, rates)) {
            val floatTarget = kind == OutputKind.AAUDIO_EXCLUSIVE && policy.aaudioConfig.floatSamples
            val target = StreamRequest.pcm(targetRate, source.channels, if (floatTarget) 32 else 16)
            val targetPass = Pass(target, pass.route, pass.matchesUsb)
            val attempt = when (kind) {
                OutputKind.ANDROID_BIT_PERFECT -> bitPerfectAttempt(targetPass)
                OutputKind.AAUDIO_EXCLUSIVE -> aaudioAttempt(targetPass, requireStreamMatch = true)
                OutputKind.ANDROID_STANDARD -> standardAttempt(targetPass)
                OutputKind.DIRECT_USB -> error("direct output has its own negotiation")
            }
            if (attempt !is Attempt.Ready) {
                yield(attempt)
                continue
            }
            val candidate = attempt.candidate
            val gain = if (targetRate == rate / 16) 0.0 else policy.conversionOptions.headroomDb
            val description = "${if (source.encoding == StreamEncoding.DOP) "DoP unpacked" else "Native DSD"} converted to PCM at $targetRate Hz; resampling gain $gain dB"
            val sourceRate = if (source.encoding == StreamEncoding.DOP) rate / 16 else rate / 8
            val written = if (source.encoding == StreamEncoding.DOP)
                WriteFormat(sourceRate, source.channels, 3, 24, sourceRate, StreamEncoding.DOP)
            else WriteFormat(sourceRate, source.channels, 1, 8, sourceRate, StreamEncoding.DSD_NATIVE)
            yield(Attempt.Ready(Candidate(
                kind = kind,
                guarantee = Guarantee.NONE,
                reason = "${candidate.reason} $description; the result is not bit-perfect.",
                processing = OutputConversion(description, false),
                commit = {
                    val opened = candidate.commit()
                    try {
                        val options = policy.conversionOptions.copy(headroomDb = gain, sourceLayout = null)
                        val converted = DsdPcmOutput(opened.output, rate, source.channels, targetRate,
                            source.channels, if (floatTarget) PcmLayout.FLOAT_32 else PcmLayout.PCM_16, options)
                        val output = if (source.encoding == StreamEncoding.DOP) DopInputOutput(converted, source.channels, 3) else converted
                        Opened(output, written, opened.session, opened.stream, opened.volume, opened.releaseSession)
                    } catch (error: Throwable) {
                        try { opened.output.close() } catch (closeError: Throwable) { error.addSuppressed(closeError) }
                        throw error
                    }
                },
                release = candidate.release,
            )))
        }
    }

    private fun canConvertDsd(source: StreamRequest): Boolean {
        val rate = source.dsdBitRate ?: return false
        if (!policy.allowConversion || source.channels !in 1..8 ||
            rate !in listOf(2_822_400, 5_644_800, 11_289_600)) return false
        return when (source.encoding) {
            StreamEncoding.DSD_NATIVE -> source.sampleRate == 0 && source.bitDepth == 0
            StreamEncoding.DOP -> policy.allowDopPcmConversion && source.sampleRate == rate / 16 && source.bitDepth == 24
            else -> false
        }
    }

    // ---- 1. direct USB ----

    private fun directAttempts(pass: Pass): Sequence<Attempt> = sequence {
        val kind = OutputKind.DIRECT_USB
        if (!policy.allowDirectUsb) {
            yield(reject(kind, RejectionReason.POLICY, "the app asked not to take the DAC away from other apps (allowDirectUsb is off)"))
            return@sequence
        }
        pass.route.wireless?.let { wireless ->
            yield(
                reject(
                    kind,
                    RejectionReason.ROUTE_UNSUITABLE,
                    "audio is routed to ${wireless.name}, a Bluetooth or cast output: a USB DAC would not be where the listener is",
                ),
            )
            return@sequence
        }
        val attached = platform.attachedDacs()
        val dacs = pass.matchesUsb?.let { attached.filter(it) } ?: attached
        if (pass.matchesUsb != null && dacs.size != 1) {
            yield(reject(kind, RejectionReason.NOT_AVAILABLE, "the USB target is absent or ambiguous"))
            return@sequence
        }
        if (dacs.isEmpty()) {
            yield(reject(kind, RejectionReason.NOT_AVAILABLE, "no USB DAC is attached"))
            return@sequence
        }
        for (dac in dacs) {
            if (!platform.hasPermission(dac)) {
                if (pass.needsPermission == null) pass.needsPermission = dac
                yield(
                    reject(
                        kind,
                        RejectionReason.PERMISSION_MISSING,
                        "the app has no USB permission for ${dac.productName}: ask with UsbDac.requestPermission and select again",
                    ),
                )
                continue
            }
            yield(directAttempt(pass.request, dac))
        }
    }

    private enum class DsdFallback { NONE, DOP, PCM, DOP_PCM }
    private class Negotiated(val format: StreamFormat, val plan: ConversionPlan?, val dsdFallback: DsdFallback = DsdFallback.NONE)

    private fun directAttempt(request: StreamRequest, dac: DacDeviceInfo): Attempt {
        val kind = OutputKind.DIRECT_USB
        val link = try {
            platform.openDac(dac, policy.dacConfig)
        } catch (error: DacException) {
            return Attempt.Rejected(rejectionOf(kind, error, dac.productName), error)
        }
        try {
            if (policy.dacConfig.requireHardwareVolume && !link.hasUsableVolume) {
                link.closeQuietly()
                return reject(
                    kind,
                    RejectionReason.NO_HARDWARE_VOLUME,
                    "${dac.productName} has no usable hardware volume, so a stream would play at the DAC's own level " +
                        "(allow it with DacConfig.requireHardwareVolume = false)",
                    DacErrorCode.VOLUME_UNAVAILABLE,
                )
            }
            val negotiated = negotiate(link, request)
            return Attempt.Ready(directCandidate(request, dac, link, negotiated))
        } catch (error: DacException) {
            link.closeQuietly()
            return Attempt.Rejected(rejectionOf(kind, error, dac.productName), error)
        } catch (error: Throwable) {
            link.closeQuietly()
            throw error
        }
    }

    private fun permitsWire(format: StreamFormat): Boolean {
        if (format.encoding == StreamEncoding.DOP && policy.dopSupport != DopSupport.CONFIRMED) return false
        val limit = policy.downstreamLimits ?: return true
        // Native DSD is not PCM; the supplied PCM/DoP carrier limits cannot prove its compatibility.
        if (format.encoding == StreamEncoding.DSD_NATIVE) return false
        return format.sampleRate <= limit.maxSampleRateHz && format.validBits in limit.allowedValidBits
    }

    /** Settle [request] exactly first, then try permitted conversions in ranked order. */
    private fun negotiate(link: DirectLink, request: StreamRequest): Negotiated {
        val exact = request.copy(ratePolicy = RatePolicy.EXACT)
        val first = if (request.encoding == StreamEncoding.DOP && policy.dopSupport != DopSupport.CONFIRMED) {
            DacException(DacErrorCode.UNSUPPORTED, "DoP decoder support has not been confirmed")
        } else try {
            val format = link.negotiate(exact)
            if (permitsWire(format) && format.encoding == request.encoding && format.channels == request.channels &&
                (request.encoding == StreamEncoding.DSD_NATIVE || format.sampleRate == request.sampleRate)) {
                return Negotiated(format, null)
            }
            DacException(DacErrorCode.UNSUPPORTED, "negotiated wire format exceeds downstream limits")
        } catch (error: DacException) {
            error
        }
        if (first.code != DacErrorCode.UNSUPPORTED) throw first
        if (request.encoding == StreamEncoding.DSD_NATIVE || request.encoding == StreamEncoding.DOP) {
            val bitRate = request.dsdBitRate ?: throw first
            if (request.encoding == StreamEncoding.DSD_NATIVE && policy.dopSupport == DopSupport.CONFIRMED) {
                try {
                    val dop = link.negotiate(StreamRequest.dop(bitRate, request.channels))
                    if (dop.encoding == StreamEncoding.DOP && dop.sampleRate == bitRate / 16 &&
                        dop.channels == request.channels && dop.subslotBytes in 3..4 && permitsWire(dop)) {
                        return Negotiated(dop, null, DsdFallback.DOP)
                    }
                } catch (error: DacException) {
                    if (error.code != DacErrorCode.UNSUPPORTED) throw error
                }
            }
            if (canConvertDsd(request)) {
                try {
                    val pcm = negotiate(link, StreamRequest.pcm(bitRate / 16, request.channels, 24))
                    if (pcm.format.encoding == StreamEncoding.PCM) return Negotiated(pcm.format, null, if (request.encoding == StreamEncoding.DOP) DsdFallback.DOP_PCM else DsdFallback.PCM)
                } catch (error: DacException) {
                    if (error.code != DacErrorCode.UNSUPPORTED) throw error
                }
            }
            throw first
        }
        if ((!policy.allowConversion && !policy.allowWidening) || request.encoding !in listOf(StreamEncoding.PCM, StreamEncoding.PCM_FLOAT32)) throw first

        val alts = link.capabilities.altSettings.filter { it.format == AltFormat.PCM && it.channels == request.channels }
        val pairs = mutableSetOf<Pair<Int, Int>>()
        val standardRates = listOf(8000, 11025, 16000, 22050, 32000, 44100, 48000, 88200, 96000,
            176400, 192000, 352800, 384000, 705600, 768000)
        fun addRange(depth: Int, range: RateRange) {
            if (range.minHz <= 0 || range.maxHz < range.minHz || range.stepHz < 0) return
            fun add(rate: Int) {
                if (rate in range.minHz..range.maxHz && (range.stepHz == 0 ||
                        (rate.toLong() - range.minHz) % range.stepHz == 0L)) pairs += rate to depth
            }
            if (range.stepHz == 0) {
                if (range.minHz == range.maxHz) add(range.minHz)
            } else {
                val step = range.stepHz.toLong()
                val n = ((request.sampleRate.toLong() - range.minHz) / step).coerceAtLeast(0)
                val ceiling = minOf(range.maxHz, policy.downstreamLimits?.maxSampleRateHz ?: range.maxHz)
                val limitIndex = ((ceiling.toLong() - range.minHz) / step).coerceAtLeast(0)
                for (index in listOf(0L, n, n + 1, limitIndex, (range.maxHz.toLong() - range.minHz) / step)) {
                    val value = range.minHz.toLong() + index * step
                    if (value <= Int.MAX_VALUE) add(value.toInt())
                }
                standardRates.forEach(::add)
                for (divisor in 1..request.sampleRate.coerceAtLeast(1)) {
                    if (divisor.toLong() * divisor > request.sampleRate) break
                    if (request.sampleRate % divisor == 0) {
                        add(divisor)
                        add(request.sampleRate / divisor)
                    }
                }
                val familyStep = if (request.sampleRate % 11025 == 0) 11025 else if (request.sampleRate % 8000 == 0) 8000 else 0
                if (familyStep > 0) {
                    val maximum = minOf(range.maxHz, policy.downstreamLimits?.maxSampleRateHz ?: range.maxHz, 768_000)
                    for (rate in familyStep..maximum step familyStep) add(rate)
                }
            }
        }
        for (alt in alts) {
            if (alt.validBits !in listOf(16, 24, 32)) continue
            alt.sampleRates.forEach { if (it > 0) pairs += it to alt.validBits }
            alt.rateRanges.forEach { addRange(alt.validBits, it) }
        }
        // Older native libraries may only expose the capability unions, not per-alt entries.
        if (link.capabilities.altSettings.isEmpty()) {
            for (depth in link.capabilities.pcmBitDepths) {
                for (rate in link.capabilities.pcmSampleRates + request.sampleRate) pairs += rate to depth
            }
        }
        val eligible = pairs.filter { (rate, depth) ->
            rate > 0 && depth in listOf(16, 24, 32) &&
                (policy.downstreamLimits?.let { rate <= it.maxSampleRateHz && depth in it.allowedValidBits } ?: true) &&
                (policy.allowConversion || (request.encoding == StreamEncoding.PCM && rate == request.sampleRate && depth > request.bitDepth))
        }
        val rank = platform.rankRates(request.sampleRate, eligible.map { it.first }).withIndex().associate { it.value to it.index }
        val ordered = eligible.sortedWith(compareBy<Pair<Int, Int>>(
            { rank[it.first] ?: Int.MAX_VALUE },
            { if (it.second >= request.bitDepth) 0 else 1 },
            { kotlin.math.abs(it.second - request.bitDepth) },
        ))
        for ((rate, depth) in ordered) {
            if (request.encoding == StreamEncoding.PCM && rate == request.sampleRate && depth == request.bitDepth) continue
            val target = exact.copy(sampleRate = rate, bitDepth = depth, encoding = StreamEncoding.PCM)
            try {
                val format = link.negotiate(target)
                if (!permitsWire(format) || format.encoding != StreamEncoding.PCM || format.sampleRate != rate || format.channels != request.channels) continue
                val options = if (request.sampleRate == format.sampleRate) policy.conversionOptions.copy(headroomDb = 0.0)
                    else policy.conversionOptions
                val plan = platform.plan(request, format, options)
                if (plan.preservesBitPerfect && !policy.allowWidening) continue
                if (!plan.preservesBitPerfect && !policy.allowConversion) continue
                if (plan.kind == ConversionKind.NONE) continue
                return Negotiated(format, plan)
            } catch (error: DacException) {
                if (error.code != DacErrorCode.UNSUPPORTED) throw error
            }
        }
        throw first
    }

    private fun directCandidate(request: StreamRequest, dac: DacDeviceInfo, link: DirectLink, negotiated: Negotiated): Candidate {
        val format = negotiated.format
        val plan = negotiated.plan
        val unsynchronized = format.pacing == Pacing.Unsynchronized ||
            (format.pacing == Pacing.Unknown && format.syncType == SyncType.ASYNCHRONOUS && !format.explicitFeedback)
        val guarantee = if (negotiated.dsdFallback !in listOf(DsdFallback.PCM, DsdFallback.DOP_PCM) && (plan?.preservesBitPerfect ?: true) && !unsynchronized) Guarantee.VERIFIED_BY_LIBRARY else Guarantee.NONE
        val reason = buildString {
            append("The library drives ${dac.productName} directly over USB (${describe(format)}): ")
            append("Android's mixer, resampler and volume are out of the path. Live stream diagnostics report whether clock and feedback are verified.")
            if (negotiated.dsdFallback == DsdFallback.DOP) append(" Native DSD is framed as DoP for the confirmed decoder.")
            if (negotiated.dsdFallback in listOf(DsdFallback.PCM, DsdFallback.DOP_PCM)) {
                val gain = if (format.sampleRate != checkNotNull(request.dsdBitRate) / 16) policy.conversionOptions.headroomDb else 0.0
                append(" ${if (negotiated.dsdFallback == DsdFallback.DOP_PCM) "DoP is validated, unpacked and converted" else "Native DSD is converted"} to PCM and is not bit-perfect; resampling gain $gain dB.")
            }
            if (plan != null) {
                if (plan.preservesBitPerfect) {
                    append(" The DAC does not run the request as it is, so the audio is first converted without changing any sample value: ${plan.description}")
                } else {
                    append(" The DAC cannot run the request as it is, so the audio is converted first and the result is not bit-perfect: ${plan.description}")
                }
            }
            if (unsynchronized) {
                append(" The DAC's endpoint has no feedback endpoint, so the stream is paced at the nominal rate and is never verified.")
            }
            if (!link.hasUsableVolume) append(" The DAC has no hardware volume: the app cannot control the level.")
        }
        // What the host writes: the wire layout, or, behind a converter, the request's own.
        val written = if (negotiated.dsdFallback == DsdFallback.DOP_PCM) {
            WriteFormat(request.sampleRate, request.channels, 3, 24, request.sampleRate, StreamEncoding.DOP)
        } else if (negotiated.dsdFallback != DsdFallback.NONE) {
            val byteRate = checkNotNull(request.dsdBitRate) / 8
            WriteFormat(byteRate, request.channels, 1, 8, byteRate, StreamEncoding.DSD_NATIVE)
        } else if (plan == null) {
            WriteFormat(format.sampleRate, format.channels, format.subslotBytes, format.validBits, format.counterRateHz, format.encoding)
        } else {
            layoutOf(request)
        }
        return Candidate(
            kind = OutputKind.DIRECT_USB,
            guarantee = guarantee,
            reason = reason,
            dac = dac,
            conversion = plan,
            wireFormat = format,
            processing = when (negotiated.dsdFallback) {
                DsdFallback.DOP -> OutputConversion("Native DSD framed as DoP for confirmed decoder", true)
                DsdFallback.PCM, DsdFallback.DOP_PCM -> {
                    val gain = if (format.sampleRate != checkNotNull(request.dsdBitRate) / 16) policy.conversionOptions.headroomDb else 0.0
                    OutputConversion("${if (negotiated.dsdFallback == DsdFallback.DOP_PCM) "DoP unpacked" else "Native DSD"} converted to PCM; resampling gain $gain dB", false)
                }
                DsdFallback.NONE -> plan?.let { OutputConversion(it.description, it.preservesBitPerfect) }
            },
            commit = {
                val stream = link.openStream(format)
                val output = try {
                    when (negotiated.dsdFallback) {
                        DsdFallback.DOP -> DsdDopOutput(stream, request.channels, format.subslotBytes, true)
                        DsdFallback.PCM, DsdFallback.DOP_PCM -> {
                            val layout = when (format.validBits to format.subslotBytes) {
                                16 to 2 -> PcmLayout.PCM_16
                                16 to 3 -> PcmLayout.PCM_16_IN_24
                                16 to 4 -> PcmLayout.PCM_16_IN_32
                                24 to 3 -> PcmLayout.PCM_24_PACKED
                                24 to 4 -> PcmLayout.PCM_24_IN_32
                                32 to 4 -> PcmLayout.PCM_32
                                else -> throw DacException(DacErrorCode.UNSUPPORTED, "unsupported DSD-to-PCM wire layout")
                            }
                            val bitRate = checkNotNull(request.dsdBitRate)
                            val options = if (format.sampleRate == bitRate / 16) policy.conversionOptions.copy(headroomDb = 0.0, sourceLayout = null)
                                else policy.conversionOptions.copy(sourceLayout = null)
                            val converted = DsdPcmOutput(stream, bitRate, request.channels, format.sampleRate, format.channels, layout, options)
                            if (negotiated.dsdFallback == DsdFallback.DOP_PCM) DopInputOutput(converted, request.channels, 3) else converted
                        }
                        DsdFallback.NONE -> if (plan == null) stream else platform.converting(stream, plan)
                    }
                } catch (error: Throwable) {
                    runCatching { stream.close() }
                    throw error
                }
                Opened(output, written, session = link.session, stream = link.stream, volume = link.volume, releaseSession = { link.close() })
            },
            release = { link.close() },
        )
    }

    // ---- 2. the platform's bit-perfect mixer ----

    private fun bitPerfectAttempt(pass: Pass): Attempt {
        val kind = OutputKind.ANDROID_BIT_PERFECT
        val request = pass.request
        if (request.encoding != StreamEncoding.PCM) {
            return reject(kind, RejectionReason.REQUEST_NOT_SUPPORTED, "the platform's bit-perfect mixer carries PCM only")
        }
        if (platform.apiLevel < API_BIT_PERFECT) {
            return reject(
                kind,
                RejectionReason.PLATFORM_TOO_OLD,
                "the bit-perfect mixer behavior needs Android 14 (API 34); this is API ${platform.apiLevel}",
            )
        }
        AndroidManagedOutput.carryProblem(request, platform.apiLevel)?.let {
            return reject(kind, RejectionReason.REQUEST_NOT_SUPPORTED, it)
        }
        pass.route.wireless?.let { wireless ->
            return reject(
                kind,
                RejectionReason.ROUTE_UNSUITABLE,
                "audio is routed to ${wireless.name}; the bit-perfect mixer exists for USB devices only",
            )
        }
        val usb = pass.route.usb ?: return reject(kind, RejectionReason.NOT_AVAILABLE, "audio is not routed to a USB device")
        if (!platform.supportsBitPerfect(usb, request)) {
            return reject(
                kind,
                RejectionReason.REQUEST_NOT_SUPPORTED,
                "the platform offers no bit-perfect mixer attributes for ${describe(request)} on ${usb.name}",
            )
        }
        return Attempt.Ready(
            Candidate(
                kind = kind,
                guarantee = Guarantee.REQUESTED_FROM_PLATFORM,
                reason = "Android's bit-perfect mixer is offered for ${usb.name} at ${describe(request)}. " +
                    "The platform enforces it and other apps keep the DAC; the library cannot verify it.",
                commit = {
                    val output = platform.openManaged(request, usb)
                    val state = output.bitPerfect.value
                    if (state !is BitPerfectState.RequestedViaPlatform) {
                        runCatching { output.close() }
                        throw DacException(
                            DacErrorCode.OPEN_FAILED,
                            "the platform did not keep the bit-perfect request" +
                                ((state as? BitPerfectState.Unverifiable)?.let { ": ${it.reason}" } ?: ""),
                        )
                    }
                    Opened(output, layoutOf(request))
                },
            ),
        )
    }

    // ---- 3. an exclusive AAudio stream ----

    private fun aaudioAttempt(pass: Pass, requireStreamMatch: Boolean = false): Attempt {
        val kind = OutputKind.AAUDIO_EXCLUSIVE
        val request = pass.request
        val config = policy.aaudioConfig.copy(requireExclusive = policy.requireExclusiveForAAudio)
        if (request.encoding != StreamEncoding.PCM) {
            return reject(kind, RejectionReason.REQUEST_NOT_SUPPORTED, "AAudio plays PCM only; DSD and DoP need the direct USB output")
        }
        if (request.channels !in 1..AAUDIO_MAX_CHANNELS) {
            return reject(kind, RejectionReason.REQUEST_NOT_SUPPORTED, "${request.channels} channels are not supported by AAudio (1 to $AAUDIO_MAX_CHANNELS)")
        }
        when (request.bitDepth) {
            16 -> Unit
            24, 32 -> if (platform.apiLevel < API_AAUDIO_WIDE && !(request.bitDepth == 32 && config.floatSamples)) {
                return reject(
                    kind,
                    RejectionReason.PLATFORM_TOO_OLD,
                    "AAudio carries ${request.bitDepth}-bit integer samples from Android 12 (API 31); this is API ${platform.apiLevel}",
                )
            }
            else -> return reject(kind, RejectionReason.REQUEST_NOT_SUPPORTED, "${request.bitDepth}-bit PCM is not available through AAudio (16, 24 or 32)")
        }
        pass.route.wireless?.let { wireless ->
            return reject(
                kind,
                RejectionReason.ROUTE_UNSUITABLE,
                "audio is routed to ${wireless.name}, a Bluetooth or cast output: an exclusive stream is not possible there",
            )
        }
        pass.route.usb?.let { usb ->
            if (!policy.allowDirectUsb) {
                return reject(
                    kind,
                    RejectionReason.POLICY,
                    "audio is routed to ${usb.name} and an exclusive stream would keep it from other apps (allowDirectUsb is off)",
                )
            }
        }
        val offer: AAudioOffer = try {
            platform.probeAAudio(request, config)
        } catch (error: DacException) {
            return Attempt.Rejected(rejectionOf(kind, error, null), error)
        }
        val grant = offer.grant
        val exclusive = grant.isExclusive
        val expectedFormat = when {
            request.bitDepth == 16 -> AAudioSampleFormat.PCM_16
            request.bitDepth == 24 -> AAudioSampleFormat.PCM_24_PACKED
            config.floatSamples -> AAudioSampleFormat.FLOAT_32
            else -> AAudioSampleFormat.PCM_32
        }
        if (requireStreamMatch && (grant.sampleRate != request.sampleRate || grant.channels != request.channels || grant.format != expectedFormat)) {
            offer.closeQuietly()
            return reject(kind, RejectionReason.NOT_AS_REQUESTED, "AAudio granted a different PCM input layout")
        }
        if (!exclusive && policy.requireExclusiveForAAudio) {
            offer.closeQuietly()
            return reject(kind, RejectionReason.EXCLUSIVE_NOT_GRANTED, "the platform granted a shared stream, not an exclusive one. ${grant.reason}")
        }
        if (exclusive && !grant.asRequested) {
            offer.closeQuietly()
            return reject(kind, RejectionReason.NOT_AS_REQUESTED, grant.reason)
        }
        return Attempt.Ready(
            Candidate(
                kind = kind,
                guarantee = if (exclusive) Guarantee.EXCLUSIVE_GRANTED_UNVERIFIABLE else Guarantee.NONE,
                reason = if (exclusive) {
                    "The platform granted an exclusive AAudio stream at ${describe(request)}: the app mixer is bypassed. " +
                        "What the audio path does beyond that cannot be verified from an app."
                } else {
                    "The platform granted a shared AAudio stream at ${describe(request)}, so the system mixer is in the path."
                },
                commit = { Opened(offer.start(), layoutOf(request)) },
                release = { offer.close() },
            ),
        )
    }

    // ---- 4. the standard output ----

    private fun standardAttempt(pass: Pass): Attempt {
        val kind = OutputKind.ANDROID_STANDARD
        val request = pass.request
        AndroidManagedOutput.carryProblem(request, platform.apiLevel)?.let { problem ->
            val tooOld = request.encoding == StreamEncoding.PCM &&
                request.bitDepth in 24..32 && platform.apiLevel < API_AAUDIO_WIDE
            return reject(kind, if (tooOld) RejectionReason.PLATFORM_TOO_OLD else RejectionReason.REQUEST_NOT_SUPPORTED, problem)
        }
        val wireless = pass.route.wireless
        return Attempt.Ready(
            Candidate(
                kind = kind,
                guarantee = Guarantee.NONE,
                reason = if (wireless != null) {
                    "Audio is routed to ${wireless.name}, a Bluetooth or cast output: there is no bit-perfect path to it, " +
                        "so Android's standard output is used and the system volume applies."
                } else {
                    "No bit-perfect route was available (the rejected alternatives say why), so Android's standard output is used: " +
                        "the system mixer may resample, mix and scale the audio, and the system volume applies."
                },
                commit = { Opened(platform.openManaged(request, null), layoutOf(request)) },
            ),
        )
    }

    // ---- helpers ----

    private fun reject(
        kind: OutputKind,
        reason: RejectionReason,
        why: String,
        error: DacErrorCode = reason.defaultCode(),
    ): Attempt.Rejected = Attempt.Rejected(Rejection(kind, reason, why, error), null)

    private fun rejectionOf(kind: OutputKind, error: DacException, subject: String?): Rejection =
        Rejection(kind, reasonOf(error.code), if (subject == null) error.detail else "$subject: ${error.detail}", error.code)

    private fun failureOf(rejection: Rejection): DacException = DacException(rejection.error, rejection.why)

    private fun layoutOf(request: StreamRequest): WriteFormat =
        WriteFormat(request.sampleRate, request.channels, request.bitDepth / 8, request.bitDepth, request.sampleRate, request.encoding)

    private fun describe(request: StreamRequest): String = when (request.encoding) {
        StreamEncoding.PCM -> "${request.sampleRate} Hz, ${request.bitDepth}-bit, ${request.channels} channels"
        StreamEncoding.PCM_U8 -> "${request.sampleRate} Hz, unsigned 8-bit PCM, ${request.channels} channels"
        StreamEncoding.PCM_FLOAT32 -> "${request.sampleRate} Hz, IEEE float32 PCM, ${request.channels} channels"
        StreamEncoding.DOP -> "DoP at DSD ${request.dsdBitRate} Hz, ${request.channels} channels"
        StreamEncoding.DSD_NATIVE -> "native DSD at ${request.dsdBitRate} Hz, ${request.channels} channels"
    }

    private fun describe(format: StreamFormat): String = when (format.encoding) {
        StreamEncoding.PCM -> "${format.sampleRate} Hz, ${format.validBits}-bit, ${format.channels} channels"
        StreamEncoding.PCM_U8 -> "${format.sampleRate} Hz, unsigned 8-bit PCM, ${format.channels} channels"
        StreamEncoding.PCM_FLOAT32 -> "${format.sampleRate} Hz, IEEE float32 PCM, ${format.channels} channels"
        StreamEncoding.DOP -> "DoP, ${format.sampleRate} Hz carrier, ${format.channels} channels"
        StreamEncoding.DSD_NATIVE -> "native DSD at ${format.dsdBitRate} Hz, ${format.channels} channels"
    }

    private fun (() -> Unit).quietly() {
        runCatching { this() }
    }

    private fun DirectLink.closeQuietly() {
        runCatching { close() }
    }

    private fun AAudioOffer.closeQuietly() {
        runCatching { close() }
    }

    private companion object {
        val ORDER = listOf(
            OutputKind.DIRECT_USB,
            OutputKind.ANDROID_BIT_PERFECT,
            OutputKind.AAUDIO_EXCLUSIVE,
            OutputKind.ANDROID_STANDARD,
        )

        /** Android 14: the bit-perfect mixer behavior. */
        const val API_BIT_PERFECT = 34

        /** Android 12: 24-bit packed and 32-bit integer samples in AAudio and `AudioTrack`. */
        const val API_AAUDIO_WIDE = 31

        const val AAUDIO_MAX_CHANNELS = 8

        fun reasonOf(code: DacErrorCode): RejectionReason = when (code) {
            DacErrorCode.IMPLICIT_FEEDBACK_UNSUPPORTED -> RejectionReason.IMPLICIT_FEEDBACK
            DacErrorCode.VOLUME_UNAVAILABLE -> RejectionReason.NO_HARDWARE_VOLUME
            DacErrorCode.BUSY -> RejectionReason.BUSY
            DacErrorCode.EXCLUSIVE_UNAVAILABLE -> RejectionReason.EXCLUSIVE_NOT_GRANTED
            DacErrorCode.UNSUPPORTED -> RejectionReason.REQUEST_NOT_SUPPORTED
            DacErrorCode.PERMISSION_DENIED -> RejectionReason.PERMISSION_MISSING
            DacErrorCode.DEVICE_LOST -> RejectionReason.NOT_AVAILABLE
            else -> RejectionReason.FAILED
        }

        /** The code a forced output fails with when the rejection did not come from a failure that has one. */
        fun RejectionReason.defaultCode(): DacErrorCode = when (this) {
            RejectionReason.POLICY,
            RejectionReason.PLATFORM_TOO_OLD,
            RejectionReason.ROUTE_UNSUITABLE,
            RejectionReason.REQUEST_NOT_SUPPORTED,
            RejectionReason.NOT_AS_REQUESTED,
            -> DacErrorCode.UNSUPPORTED
            RejectionReason.NOT_AVAILABLE -> DacErrorCode.DEVICE_LOST
            RejectionReason.PERMISSION_MISSING -> DacErrorCode.PERMISSION_DENIED
            RejectionReason.IMPLICIT_FEEDBACK -> DacErrorCode.IMPLICIT_FEEDBACK_UNSUPPORTED
            RejectionReason.NO_HARDWARE_VOLUME -> DacErrorCode.VOLUME_UNAVAILABLE
            RejectionReason.BUSY -> DacErrorCode.BUSY
            RejectionReason.EXCLUSIVE_NOT_GRANTED -> DacErrorCode.EXCLUSIVE_UNAVAILABLE
            RejectionReason.FAILED -> DacErrorCode.UNKNOWN
        }
    }
}
