package dev.usbdac

import dev.usbdac.internal.Native

/** In-repository diagnostic hook; available only in validator-enabled native builds. */
public object ValidatorBridge {
    /** Start recording before USB attachment and opening. */
    public fun begin(directory: String) { Native.validatorBegin(directory) }
    /** Flush diagnostic output before exporting. */
    public fun flush() { Native.validatorFlush() }
    /** Generate an offline HTML report from a capture directory, including recovered runs. */
    public fun writeHtml(directory: String) { Native.validatorWriteHtml(directory) }
    /** Request cancellation of the active diagnostic suite. */
    public fun cancel() { Native.validatorCancel() }
    /** Report that the selected USB attachment was detached. */
    public fun deviceLost() { Native.validatorDeviceLost() }
}
