package dev.usbdac.internal

import android.hardware.usb.UsbConfiguration
import android.hardware.usb.UsbDevice
import android.hardware.usb.UsbDeviceConnection
import dev.usbdac.DacErrorCode
import dev.usbdac.DacException

/** Kept by the connection owner until every native transfer/control has retired. */
internal class UsbConfigurationSelection(val selected: UsbConfiguration, private val transaction: ConfigurationTransaction?, private val verifySelection: (() -> Unit)? = null) {
    fun verify() { verifySelection?.invoke() }
    fun restore() { transaction?.restore() }
}

internal interface ConfigurationIo {
    fun current(): Int
    fun select(id: Int): Boolean
}

/** Selection is explicit, bounded to an advertised ID and read back after every write. */
internal class ConfigurationTransaction private constructor(
    private val io: ConfigurationIo, private val original: Int, private val requested: Int, private var changed: Boolean,
) {
    fun restore(force: Boolean = false) {
        if (!changed) return
        try {
            if (!force) {
                when (io.current()) {
                    original -> { changed = false; return }
                    requested -> Unit
                    else -> throw DacException(DacErrorCode.CONFIGURATION_RESTORE_FAILED, "USB configuration changed externally; refusing to overwrite it on hand-back")
                }
            }
            val accepted = io.select(original)
            val observed = io.current()
            if (!accepted || observed != original) {
                throw DacException(DacErrorCode.CONFIGURATION_RESTORE_FAILED, "original USB configuration $original was not restored")
            }
            changed = false
        } catch (error: Throwable) {
            if (error is DacException && error.code == DacErrorCode.CONFIGURATION_RESTORE_FAILED) throw error
            throw DacException(DacErrorCode.CONFIGURATION_RESTORE_FAILED, "could not verify original USB configuration $original", error)
        }
    }
    companion object {
        fun select(io: ConfigurationIo, requested: Int, advertisedIds: List<Int>): ConfigurationTransaction {
            if (requested !in 1..255 || advertisedIds.count { it == requested } != 1) {
                throw DacException(DacErrorCode.CONFIGURATION_UNAVAILABLE, "USB configuration $requested is not uniquely advertised")
            }
            val original = io.current()
            if (original !in advertisedIds) throw DacException(DacErrorCode.CONFIGURATION_SELECTION_FAILED, "active USB configuration $original is not advertised")
            val transaction = ConfigurationTransaction(io, original, requested, original != requested)
            if (original == requested) return transaction
            try {
                val accepted = io.select(requested)
                val observed = io.current()
                if (!accepted || observed != requested) {
                    throw DacException(DacErrorCode.CONFIGURATION_SELECTION_FAILED, "USB configuration $requested was rejected or failed readback")
                }
                return transaction
            } catch (error: Throwable) {
                val failure = if (error is DacException && error.code == DacErrorCode.CONFIGURATION_SELECTION_FAILED) error
                    else DacException(DacErrorCode.CONFIGURATION_SELECTION_FAILED, "could not select USB configuration $requested", error)
                try { transaction.restore(force = true) } catch (restore: Throwable) { failure.addSuppressed(restore) }
                throw failure
            }
        }
    }
}

internal fun UsbDevice.selectConfiguration(connection: UsbDeviceConnection, requestedId: Int?): UsbConfigurationSelection {
    if (requestedId == null) return UsbConfigurationSelection(activeConfiguration(connection), null)
    val configurations = (0 until configurationCount).map(::getConfiguration)
    val io = object : ConfigurationIo {
        override fun current(): Int = readConfigurationId(connection)
        override fun select(id: Int): Boolean = connection.setConfiguration(configurations.single { it.id == id })
    }
    val transaction = ConfigurationTransaction.select(io, requestedId, configurations.map { it.id })
    return UsbConfigurationSelection(configurations.single { it.id == requestedId }, transaction) {
        if (readConfigurationId(connection) != requestedId) {
            throw DacException(DacErrorCode.CONFIGURATION_SELECTION_FAILED, "selected USB configuration changed before native registration")
        }
    }
}

/** Always issue GET_CONFIGURATION, including a device with one advertised configuration. */
internal fun UsbDevice.readConfigurationId(connection: UsbDeviceConnection): Int {
    val data = ByteArray(1)
    if (connection.controlTransfer(0x80, 0x08, 0, 0, data, 1, 500) != 1) {
        throw DacException(DacErrorCode.CONFIGURATION_SELECTION_FAILED, "could not read USB configuration of $deviceName")
    }
    return data[0].toInt() and 0xff
}
