package dev.usbdac

/**
 * Owns deferred hand-back after an opt-in playback open failed. Keep this handle until [finish]
 * completes. While native USB work is retiring the connection, claims and attachment lease stay
 * owned; [isReady] only checks retirement and never changes the USB configuration.
 *
 * Call [finish] explicitly off the main thread. It returns false while retirement is outstanding;
 * after retirement it performs one verified hand-back and closes the retained resources. A failed
 * native teardown or hand-back failure is reported as a typed [DacException] and is not retried.
 */
public class PendingUsbConfigurationRestore internal constructor(
    private val checkReady: () -> Boolean,
    private val finishRetired: () -> Unit,
) {
    private var finished = false
    private var failure: DacException? = null

    /** Whether native retirement has completed. This may wait briefly but does not mutate the device. */
    public val isReady: Boolean
        @Synchronized get() = finished || checkReady()

    /** Return false while retiring; otherwise perform or report the single hand-back attempt. */
    @Synchronized
    public fun finish(): Boolean {
        failure?.let { throw it }
        if (finished) return true
        if (!checkReady()) return false
        // Mark first so even a failing hand-back cannot be attempted a second time.
        finished = true
        try {
            finishRetired()
        } catch (error: Throwable) {
            val typed = if (error is DacException) error else DacException(
                DacErrorCode.CONFIGURATION_RESTORE_FAILED, "USB configuration hand-back failed", error,
            )
            failure = typed
            throw typed
        }
        return true
    }
}
