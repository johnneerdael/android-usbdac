package dev.usbdac.internal

import android.hardware.usb.UsbConfiguration
import android.hardware.usb.UsbDevice
import android.hardware.usb.UsbDeviceConnection
import dev.usbdac.StreamRequest
import dev.usbdac.StreamEncoding
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

/** Read a complete configuration by descriptor index, never by configuration value. */
internal fun readConfigurationDescriptor(connection: UsbDeviceConnection, index: Int): ByteArray {
    fun read(length: Int): ByteArray {
        val bytes = ByteArray(length)
        if (connection.controlTransfer(0x80, 0x06, 0x0200 or index, 0, bytes, length, 500) != length) {
            throw DacException(DacErrorCode.CONFIGURATION_SELECTION_FAILED, "could not read complete configuration descriptor index $index")
        }
        return bytes
    }
    val header = read(9)
    val length = (header[2].toInt() and 255) or ((header[3].toInt() and 255) shl 8)
    if ((header[0].toInt() and 255) != 9 || (header[1].toInt() and 255) != 2 || length !in 9..16384) {
        throw DacException(DacErrorCode.CONFIGURATION_SELECTION_FAILED, "invalid configuration descriptor header")
    }
    val body = read(length)
    if (!body.copyOfRange(0, 9).contentEquals(header)) {
        throw DacException(DacErrorCode.CONFIGURATION_SELECTION_FAILED, "configuration descriptor changed during read")
    }
    return body
}

/** Conservative structural proof: unknown, incomplete or vendor output geometry never selects fallback. */
internal data class FallbackDescriptor(val id: Int, val fullUac3: Boolean, val legacyPlayback: Boolean, val hasIsoOut: Boolean, val formatExcluded: Boolean = false)
internal fun parseFallbackDescriptor(bytes: ByteArray, request: StreamRequest? = null): FallbackDescriptor? {
    fun u(at: Int) = bytes[at].toInt() and 255
    if (bytes.size < 9 || u(0) != 9 || u(1) != 2 || (u(2) or (u(3) shl 8)) != bytes.size || u(5) == 0) return null
    data class Interface(val id: Int, val alt: Int, val cls: Int, val sub: Int, val protocol: Int, val endpoints: Int, var seen: Int = 0, var out: Boolean = false, var outputCapacity: Int = 0, val addresses: MutableSet<Int> = mutableSetOf(), val extras: MutableList<ByteArray> = mutableListOf())
    data class Association(val first: Int, val count: Int, val cls: Int, val sub: Int, val protocol: Int)
    val interfaces = mutableListOf<Interface>()
    val associations = mutableListOf<Association>()
    var current: Interface? = null
    var at = 9
    var anyIsoOut = false
    while (at < bytes.size) {
        if (at + 2 > bytes.size) return null
        val size = u(at)
        if (size < 2 || at + size > bytes.size) return null
        when (u(at + 1)) {
            2 -> return null
            11 -> {
                if (size != 8) return null
                val first = u(at + 2); val count = u(at + 3)
                if (count == 0 || first + count > 256) return null
                associations.add(Association(first, count, u(at + 4), u(at + 5), u(at + 6)))
                current = null
            }
            4 -> {
                if (size < 9) return null
                val item = Interface(u(at + 2), u(at + 3), u(at + 5), u(at + 6), u(at + 7), u(at + 4))
                if (interfaces.any { it.id == item.id && it.alt == item.alt }) return null
                interfaces.add(item); current = item
            }
            5 -> {
                if (size < 7) return null
                val item = current ?: return null
                val address = u(at + 2)
                if ((address and 15) == 0 || (address and 0x70) != 0 || !item.addresses.add(address)) return null
                item.seen++
                if ((u(at + 2) and 0x80) == 0 && (u(at + 3) and 3) == 1) {
                    item.out = true; anyIsoOut = true
                    item.outputCapacity = maxOf(item.outputCapacity, (u(at + 4) or (u(at + 5) shl 8)) and 0x7ff)
                }
            }
            0x24 -> {
                if (size < 3) return null
                current?.extras?.add(bytes.copyOfRange(at, at + size))
            }
        }
        at += size
    }
    if (interfaces.map { it.id }.distinct().size != u(4) || interfaces.any { it.seen != it.endpoints } ||
        interfaces.map { it.id }.distinct().any { id -> interfaces.none { it.id == id && it.alt == 0 } }) return null
    if (associations.indices.any { i -> associations.indices.any { j -> i < j && associations[i].first < associations[j].first + associations[j].count && associations[j].first < associations[i].first + associations[i].count } }) return null
    fun associationHas(a: Association, protocol: Int, requireOutput: Boolean): Boolean {
        val members = interfaces.filter { it.id in a.first until a.first + a.count }
        if ((a.first until a.first + a.count).any { id -> members.none { it.id == id && it.alt == 0 } }) return false
        return members.any { it.cls == 1 && it.sub == 1 && it.protocol == protocol } &&
            members.any { it.cls == 1 && it.sub == 2 && it.protocol == protocol && (!requireOutput || it.out) }
    }
    val full = associations.any { it.cls == 1 && it.sub == 1 && it.protocol == 0x30 && associationHas(it, 0x30, false) }
    fun legacyPossible(a: Association, item: Interface): Boolean {
        val req = request ?: return false
        if (!item.out || item.alt == 0 || item.cls != 1 || item.sub != 2 || item.protocol != a.protocol) return false
        val general = item.extras.singleOrNull { it.size >= 3 && (it[2].toInt() and 255) == 1 } ?: return false
        val format = item.extras.singleOrNull { it.size >= 3 && (it[2].toInt() and 255) == 2 } ?: return false
        fun v(d: ByteArray, i: Int) = d[i].toInt() and 255
        if (general.size < 4 || v(general, 3) == 0 || format.size < 6 || v(format, 3) != 1) return false
        val bit = when (req.encoding) { StreamEncoding.PCM -> 1; StreamEncoding.PCM_U8 -> 2; StreamEncoding.PCM_FLOAT32 -> 4; else -> return false }
        val channels: Int
        val slot: Int
        val depth: Int
        if (a.protocol == 0x20) {
            if (general.size != 16 || format.size != 6 || v(general, 5) != 1) return false
            val bitmap = (6..9).fold(0L) { value, i -> value or (v(general, i).toLong() shl (8 * (i - 6))) }
            if ((bitmap and bit.toLong()) == 0L) return false
            channels = v(general, 10); slot = v(format, 4); depth = v(format, 5)
        } else {
            if (general.size != 7 || format.size < 8 || (v(general, 5) or (v(general, 6) shl 8)) != bit) return false
            channels = v(format, 4); slot = v(format, 5); depth = v(format, 6)
            val count = v(format, 7)
            val required = if (count == 0) 14 else 8 + count * 3
            if (format.size != required) return false
            fun rate(at: Int) = v(format, at) or (v(format, at + 1) shl 8) or (v(format, at + 2) shl 16)
            if (count == 0) {
                val min = rate(8); val max = rate(11)
                if (min == 0 || max < min || req.sampleRate !in min..max) return false
            } else if ((0 until count).none { rate(8 + it * 3) == req.sampleRate }) return false
        }
        return channels == req.channels && slot in 1..4 && depth == req.bitDepth && depth in 1..slot * 8 &&
            item.outputCapacity >= channels * slot
    }
    val legacy = associations.any { a -> a.cls == 1 && a.sub == 0 && a.protocol in listOf(0, 0x20) &&
        associationHas(a, a.protocol, true) && interfaces.any { it.id in a.first until a.first + a.count && legacyPossible(a, it) } }
    // A negative requires every OUT path to belong to a validated Full UAC3
    // association and have an independently valid AS header with a fixed mismatch.
    fun excludes(item: Interface): Boolean {
        val req = request ?: return false
        val owner = associations.singleOrNull { item.id in it.first until it.first + it.count } ?: return false
        if (owner.cls != 1 || owner.sub != 1 || owner.protocol != 0x30 || !associationHas(owner, 0x30, false) ||
            item.cls != 1 || item.sub != 2 || item.protocol != 0x30 || item.alt == 0) return false
        val headers = item.extras.filter { it.size >= 3 && (it[2].toInt() and 255) == 1 }
        val h = headers.singleOrNull() ?: return false
        fun b(i: Int) = h[i].toInt() and 255
        if (h.size != 23 || b(3) == 0 || (b(8) or b(9)) == 0 || b(18) !in 1..4 || b(19) == 0 || b(19) > b(18) * 8 ||
            ((b(4) shr 4) and 3) !in 0..1 || b(20) != 0 || b(21) != 0 || b(22) != 0 ||
            (11..17).any { b(it) != 0 } || b(10) == 0 || (b(10) and 0xf8) != 0) return false
        val formatBit = when (req.encoding) {
            StreamEncoding.PCM -> 1
            StreamEncoding.PCM_U8 -> 2
            StreamEncoding.PCM_FLOAT32 -> 4
            else -> return false
        }
        val ranges = item.extras.filter { it.size >= 3 && (it[2].toInt() and 255) == 2 }
        fun word(d: ByteArray, start: Int): Long = (0..3).fold(0L) { v, n -> v or ((d[start + n].toLong() and 255) shl (8 * n)) }
        if (ranges.any { it.size != 11 || word(it, 3) == 0L || word(it, 7) < word(it, 3) }) return false
        val outside = ranges.isNotEmpty() && ranges.none { req.sampleRate.toLong() in word(it, 3)..word(it, 7) }
        return (b(10) and formatBit) == 0 || b(19) != req.bitDepth || outside
    }
    val outputAlts = interfaces.filter { it.out }
    val excluded = outputAlts.isNotEmpty() && outputAlts.all(::excludes)
    return FallbackDescriptor(u(5), full, legacy, anyIsoOut, excluded)
}

/** Return only the uniquely advertised index-zero value when complete descriptors prove no OUT. */
internal fun legacyFallbackId(active: FallbackDescriptor, first: FallbackDescriptor, advertised: List<Int>): Int? =
    first.id.takeIf { active.fullUac3 && (!active.hasIsoOut || active.formatExcluded) && first.legacyPlayback && active.id != first.id && advertised.count { value -> value == first.id } == 1 }
