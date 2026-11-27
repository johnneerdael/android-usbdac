package dev.usbdac

import android.media.AudioDeviceInfo

/**
 * The outputs [OutputSelector] chooses between, best first. Each one is a [PcmOutput]; what differs
 * is who owns the DAC and what can be promised about the samples (see [Guarantee]).
 */
public enum class OutputKind {
    /**
     * The library drives a USB DAC itself ([DacStream]): Android's mixer, resampler and volume are
     * out of the path, the DAC's own volume applies, and the library verifies the stream. Android's
     * audio stack cannot use the DAC while this output is open.
     */
    DIRECT_USB,

    /**
     * An `AudioTrack` through Android, with the platform's bit-perfect mixer behavior requested for
     * a USB DAC (Android 14 and later). Other apps keep the DAC; Android enforces the behavior and
     * the library cannot verify it ([AndroidManagedOutput]).
     */
    ANDROID_BIT_PERFECT,

    /**
     * An exclusive AAudio stream at the source's own rate and format ([AAudioExclusiveOutput]): the
     * route of a player with an internal DAC, recognised by what the platform grants rather than by
     * a list of devices. The app mixer is bypassed; what happens past AAudio cannot be seen from an app.
     */
    AAUDIO_EXCLUSIVE,

    /**
     * An ordinary `AudioTrack` through Android ([AndroidManagedOutput]) when nothing better is
     * available: the system mixer may resample, mix and scale, and the system volume applies.
     */
    ANDROID_STANDARD,
}

/**
 * What can be said about the samples on the chosen output, in terms of who guarantees it. It is
 * not a live verdict: read [PcmOutput.bitPerfect] on the opened output for that.
 */
public enum class Guarantee {
    /**
     * The library checks the stream itself: the clock the DAC confirmed, the format on the wire, lost
     * frames and the payload ([BitPerfectState.Verified]). The output's [PcmOutput.bitPerfect] says
     * whether every check holds right now, and names the reason when one does not.
     */
    VERIFIED_BY_LIBRARY,

    /**
     * Android accepted a request for its bit-perfect mixer behavior for this device and format
     * ([BitPerfectState.RequestedViaPlatform]). Android enforces it; the library cannot confirm it.
     */
    REQUESTED_FROM_PLATFORM,

    /**
     * The platform granted an exclusive AAudio stream at exactly the requested rate and format. The
     * app mixer is bypassed; the device's audio HAL and DAC are beyond what an app can see
     * ([BitPerfectState.NotVerified] with the grant's reason).
     */
    EXCLUSIVE_GRANTED_UNVERIFIABLE,

    /**
     * No bit-perfect promise: the standard Android output, a shared AAudio stream, or a stream that
     * is converted before it reaches the DAC.
     */
    NONE,
}

/**
 * What the output is allowed and told to do. The defaults are the cautious ones: bit-perfect first,
 * nothing converted, an exclusive AAudio stream only when it really is exclusive.
 *
 * @property allowDirectUsb Whether the app may take a USB DAC away from the rest of the system.
 *   `false` keeps [OutputKind.DIRECT_USB] out of the choice (and an exclusive AAudio stream on a USB
 *   route too), so other apps keep their sound; the platform's bit-perfect mixer is the way to a DAC
 *   then. It is the one setting a listener should be able to turn on and off ("let this app take over
 *   the DAC"), because the choice changes what every other app can do while it plays.
 * @property force Skips the detection and uses exactly this output. If it cannot be opened,
 *   [OutputSelector.select] and [OutputSelector.open] throw a [DacException] with the typed code
 *   instead of using another output. `force = DIRECT_USB` together with `allowDirectUsb = false`
 *   contradicts itself and is refused with an `IllegalArgumentException`.
 * @property requireExclusiveForAAudio `true` (the default): a shared AAudio grant is not accepted
 *   ([RejectionReason.EXCLUSIVE_NOT_GRANTED]) and the selection moves on. `false`: a shared grant is
 *   accepted as [OutputKind.AAUDIO_EXCLUSIVE] with [Guarantee.NONE]; in the automatic order that puts a
 *   shared stream ahead of the standard output, so it is mostly useful with [force]. It replaces
 *   [AAudioConfig.requireExclusive] of [aaudioConfig], which the selector ignores.
 * @property allowConversion Let the direct output convert (resample, widen or narrow) a request the
 *   DAC cannot run as it is, with the library's own converter. The decision then carries the
 *   [ConversionPlan], and the guarantee drops to [Guarantee.NONE] unless every sample value is kept
 *   (a widening). `false` (the default): a DAC that cannot run the request exactly is rejected.
 * @property dacConfig The session configuration for [OutputKind.DIRECT_USB]. With
 *   [DacConfig.requireHardwareVolume] on (the default) a DAC without a usable hardware volume is
 *   rejected ([RejectionReason.NO_HARDWARE_VOLUME]); turn it off to accept a fixed level.
 * @property aaudioConfig The options of [OutputKind.AAUDIO_EXCLUSIVE]. Its `requireExclusive` is
 *   overridden by [requireExclusiveForAAudio].
 */
public data class OutputPolicy @JvmOverloads constructor(
    val allowDirectUsb: Boolean = true,
    val force: OutputKind? = null,
    val requireExclusiveForAAudio: Boolean = true,
    val allowConversion: Boolean = false,
    val dacConfig: DacConfig = DacConfig(),
    val aaudioConfig: AAudioConfig = AAudioConfig(),
    /** Permit same-rate, same-channel integer widening only, with a lossless conversion plan. */
    val allowWidening: Boolean = false,
) {
    init {
        require(!(force == OutputKind.DIRECT_USB && !allowDirectUsb)) {
            "force = DIRECT_USB contradicts allowDirectUsb = false"
        }
    }
}

/** What kind of device an output route is. */
public enum class RouteKind {
    /** A USB audio device: a DAC or a USB headset. */
    USB,

    /** A wired analog or digital line, headphone or headset jack: driven by the device's own DAC. */
    WIRED,

    /** The phone's speaker or earpiece. */
    BUILT_IN,

    /** Bluetooth, a hearing aid, or audio sent to another screen or speaker (cast, mirroring). */
    WIRELESS,

    /** HDMI, a dock, or anything else. */
    OTHER,
}

/**
 * An audio output device Android knows about.
 *
 * @property kind What sort of device it is.
 * @property name The device's own name, or a generic one when it gives none.
 * @property id `AudioDeviceInfo.getId()`.
 * @property device The Android device, for APIs this SDK does not wrap; `null` where the library
 *   has none to give.
 */
public data class OutputRoute(
    val kind: RouteKind,
    val name: String,
    val id: Int,
    val device: AudioDeviceInfo? = null,
)

/** Why an output was not chosen. */
public enum class RejectionReason {
    /** The policy rules it out: [OutputPolicy.allowDirectUsb] is off. */
    POLICY,

    /** The thing it needs is not there: no USB DAC is attached, no USB audio route is active. */
    NOT_AVAILABLE,

    /** The app does not hold USB permission for the DAC. Ask for it and select again. */
    PERMISSION_MISSING,

    /** This Android release cannot do it (it needs a newer API level). */
    PLATFORM_TOO_OLD,

    /** Audio goes to a Bluetooth or cast route, or to a device this output cannot serve. */
    ROUTE_UNSUITABLE,

    /** The output cannot carry this request: DSD on a mixer, a rate or depth the DAC does not run, a channel count. */
    REQUEST_NOT_SUPPORTED,

    /** The DAC paces an asynchronous endpoint with implicit feedback, which the library does not support. */
    IMPLICIT_FEEDBACK,

    /** The DAC has no usable hardware volume and [DacConfig.requireHardwareVolume] is on. */
    NO_HARDWARE_VOLUME,

    /** Another session or another app holds the device. */
    BUSY,

    /** The platform granted only a shared AAudio stream. */
    EXCLUSIVE_NOT_GRANTED,

    /** The platform granted exclusive mode, but not at what was asked (the hardware runs another rate or format). */
    NOT_AS_REQUESTED,

    /** Opening or negotiating failed for another reason; the [Rejection.error] says which. */
    FAILED,
}

/**
 * An output that was considered before the chosen one and was not used.
 *
 * @property kind The output.
 * @property reason The reason, as a value to branch on.
 * @property why The reason, as a sentence for a status line.
 * @property error The [DacErrorCode] that forcing this output ([OutputPolicy.force]) would fail with.
 */
public data class Rejection(
    val kind: OutputKind,
    val reason: RejectionReason,
    val why: String,
    val error: DacErrorCode,
)

/**
 * The layout of what the app writes to the output, and the unit of its position counters.
 *
 * It is the wire layout of the negotiated [StreamFormat] for the direct output without conversion
 * (a 24-bit sample may sit in a 3-byte or a 4-byte slot there), and the layout of the request for
 * every other output, including a converting one: little-endian, interleaved, 16-bit in 2 bytes,
 * 24-bit packed in 3, 32-bit in 4 (32-bit float with [AAudioConfig.floatSamples]).
 *
 * @property sampleRate The rate of the frames written.
 * @property channels Samples per frame.
 * @property subslotBytes Bytes per sample.
 * @property validBits Significant bits of each sample, left-justified in the slot.
 * @property counterRateHz The rate of [PcmOutput.playedFrames] and [PcmOutput.latencyFrames]. The
 *   audible position is `startPosition + playedFrames / counterRateHz`.
 */
public data class WriteFormat(
    val sampleRate: Int,
    val channels: Int,
    val subslotBytes: Int,
    val validBits: Int,
    val counterRateHz: Int,
    /** Exact source/wire encoding; unsigned8 and float32 must never be interpreted as signed PCM. */
    val encoding: StreamEncoding = StreamEncoding.PCM,
) {
    /** Bytes of one frame: the unit [PcmOutput.write] takes. */
    val bytesPerFrame: Int get() = channels * subslotBytes
}

/**
 * What [OutputSelector] chose, and why.
 *
 * @property kind The chosen output.
 * @property guarantee Who vouches for the samples on it; see [Guarantee].
 * @property reason A sentence saying what was chosen and what that means, for a status line.
 * @property route The route Android sends audio to, if known: the device the decision was judged against.
 * @property dac The DAC the direct output is about, for [OutputKind.DIRECT_USB].
 * @property conversion The conversion that sits in front of the direct output, when
 *   [OutputPolicy.allowConversion] made the DAC carry a request it does not run as it is; `null` otherwise.
 * @property needsUsbPermission A DAC that is attached but whose USB permission the app lacks, when
 *   direct output would have been chosen with it. Ask with [UsbDac.requestPermission] and select
 *   again. The selector never asks by itself. `null` when nothing is waiting for a permission.
 * @property alternativesRejected The outputs tried before this one, best first, and why each was
 *   passed over. Empty when the first choice worked.
 */
public data class OutputDecision(
    val kind: OutputKind,
    val guarantee: Guarantee,
    val reason: String,
    val route: OutputRoute?,
    val dac: DacDeviceInfo?,
    val conversion: ConversionPlan?,
    val needsUsbPermission: DacDeviceInfo?,
    val alternativesRejected: List<Rejection>,
)

/**
 * A change that may make another output the best one. Re-select at the next track boundary; the
 * selector never changes a playing stream. One physical event can raise several of these (plugging in
 * a DAC raises an attach and a route change), so treat each as "look again" and debounce.
 */
public sealed interface OutputChange {
    /** A DAC was plugged in. It has no USB permission yet unless the user granted it permanently. */
    public data class DacAttached(val dac: DacDeviceInfo) : OutputChange

    /** A DAC was unplugged. */
    public data class DacDetached(val dac: DacDeviceInfo) : OutputChange

    /** The user granted USB permission for the DAC: direct output may now be possible. */
    public data class PermissionGranted(val dac: DacDeviceInfo) : OutputChange

    /** Output devices appeared or went away: the route Android uses may have changed. */
    public data class RouteChanged(val added: List<OutputRoute>, val removed: List<OutputRoute>) : OutputChange
}
