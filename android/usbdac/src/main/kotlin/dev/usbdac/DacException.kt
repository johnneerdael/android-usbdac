package dev.usbdac

/**
 * Why a call failed.
 *
 * Most codes come from the native library, where they are the stable identifiers of its
 * errors (`busy`, `claim`, ...). The rest are raised by the Kotlin side before native code
 * is reached.
 */
public enum class DacErrorCode(internal val wireName: String?) {
    /** Another [DacSession] is open in this process. Close it first. */
    BUSY("busy"),

    /** Direct USB output needs Linux usbfs (Android or Linux). */
    PLATFORM_UNSUPPORTED("platform_unsupported"),

    /**
     * The interfaces could not be claimed, or are not the set the library expected.
     * Android's own USB audio driver may still hold them; see the troubleshooting guide.
     */
    CLAIM("claim"),

    /** No alternate setting of the DAC carries the request. */
    UNSUPPORTED("unsupported"),

    /**
     * The DAC would stream the request only through an asynchronous endpoint that has no
     * explicit feedback endpoint and has no compatible input stream to take implicit feedback from.
     * [DacConfig.allowUnsynchronizedAsync] streams it anyway, never bit-perfect.
     */
    IMPLICIT_FEEDBACK_UNSUPPORTED("implicit_feedback_unsupported"),

    /** The DAC did not confirm the sampling frequency and [DacConfig.requireVerifiedRate] is on. */
    CLOCK_UNVERIFIED("clock_unverified"),

    /** The DAC was unplugged or stopped answering. */
    DEVICE_LOST("device_lost"),

    /** An isochronous transfer, or the stream machinery around it, failed. */
    TRANSFER("transfer"),

    /** A class control request (clock, volume, mute) failed. */
    CONTROL("control"),

    /** The call is not valid now: closed handle, writing after drain, a second stream, ... */
    INVALID_STATE("invalid_state"),

    /** An argument was out of range or malformed. */
    INVALID_ARGUMENT("invalid_argument"),

    /**
     * The DAC has no usable hardware volume control (none, or its start-up attenuation could not
     * be verified) and [DacConfig.requireHardwareVolume] is on, so a stream would play at the
     * DAC's own level. Ask the user to allow fixed-level output and open the session again with
     * `requireHardwareVolume = false`.
     */
    VOLUME_UNAVAILABLE("volume_unavailable"),

    /** Drain deadline expired; the original stream is still open and draining. */
    RECONFIGURE_DRAIN_TIMEOUT("reconfigure_drain_timeout"),

    /** Replacement/rollback lost a usable stream or retained USB ownership; close the session. */
    RECONFIGURE_UNUSABLE("reconfigure_unusable"),

    /** The app does not hold USB permission for the device. */
    PERMISSION_DENIED(null),

    /** Android refused to open the USB device or gave no usable file descriptor. */
    OPEN_FAILED(null),

    /**
     * [AAudioConfig.requireExclusive] is on and the platform granted only a shared AAudio stream:
     * the system mixer would be in the path. Open with `requireExclusive = false` to accept it.
     */
    EXCLUSIVE_UNAVAILABLE("exclusive_unavailable"),

    /** The requested USB configuration ID is absent or ambiguous. */
    CONFIGURATION_UNAVAILABLE(null),

    /** Android rejected explicit selection or GET_CONFIGURATION did not confirm it. */
    CONFIGURATION_SELECTION_FAILED(null),

    /** Hand-back could not confirm restoration of the original USB configuration. */
    CONFIGURATION_RESTORE_FAILED(null),

    /** A code this version of the library does not know. */
    UNKNOWN(null);

    internal companion object {
        fun fromWireName(name: String): DacErrorCode =
            entries.firstOrNull { it.wireName == name } ?: UNKNOWN
    }
}

/**
 * A failure of the library, with a typed [code].
 *
 * The native library reports errors as an `IllegalStateException` whose message starts with
 * `usbdac:<code>:`; every call of this SDK converts that into a [DacException].
 */
public class DacException(
    /** What went wrong. */
    public val code: DacErrorCode,
    message: String,
    cause: Throwable? = null,
) : Exception("${code.wireName ?: code.name.lowercase()}: $message", cause) {

    /** The message without the code prefix. */
    public val detail: String = message
}

private const val ERROR_PREFIX = "usbdac:"

/**
 * Reads a native error message, `usbdac:<code>: <message>`. Returns `null` when [message] is
 * not one of ours.
 */
internal fun parseNativeError(message: String?, cause: Throwable? = null): DacException? {
    if (message == null || !message.startsWith(ERROR_PREFIX)) return null
    val rest = message.substring(ERROR_PREFIX.length)
    val separator = rest.indexOf(':')
    if (separator < 0) return DacException(DacErrorCode.UNKNOWN, rest.trim(), cause)
    val code = DacErrorCode.fromWireName(rest.substring(0, separator).trim())
    return DacException(code, rest.substring(separator + 1).trim(), cause)
}

/**
 * Runs a native call, and the parsing of its result, and rethrows its `usbdac:` failures as
 * [DacException]. A response that cannot be read is a [DacErrorCode.UNKNOWN] failure too, so the
 * caller only ever sees one kind of exception.
 */
internal inline fun <T> nativeCall(block: () -> T): T =
    try {
        block()
    } catch (error: IllegalStateException) {
        throw parseNativeError(error.message, error) ?: error
    } catch (error: org.json.JSONException) {
        throw DacException(DacErrorCode.UNKNOWN, "unreadable response from the native library: ${error.message}", error)
    }
