package dev.usbdac

/** A paused replacement output on the same retained USB session, with zero-based stream counters. */
public data class StreamReconfiguration(
    /** The output to use next; the old stream is retired. */
    public val stream: DacStream,
    /** Requested-format error when the old format was restored; null when the requested format opened. */
    public val rollbackCause: DacException?,
) {
    /** True when [stream] uses the restored original format after a failed requested change. */
    public val rolledBack: Boolean get() = rollbackCause != null
}
