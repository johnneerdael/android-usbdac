package dev.usbdac.internal

import android.util.Log

internal fun logWarning(message: String, error: Throwable? = null) {
    Log.w("usbdac", message, error)
}
