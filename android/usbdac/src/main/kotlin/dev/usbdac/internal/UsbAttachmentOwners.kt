package dev.usbdac.internal

/** Actual Android attachment identity, never caller-supplied DAC display metadata. */
internal class UsbAttachmentOwners {
    private data class Key(val deviceId: Int, val systemPath: String)
    private val active = mutableMapOf<Key, Lease>()

    @Synchronized
    fun acquire(deviceId: Int, systemPath: String): Lease? {
        val key = Key(deviceId, systemPath)
        if (active.containsKey(key)) return null
        return Lease { synchronized(this) { active.remove(key, it) } }.also { active[key] = it }
    }

    internal class Lease(private val release: (Lease) -> Unit) : AutoCloseable {
        override fun close() = release(this)
    }
}
