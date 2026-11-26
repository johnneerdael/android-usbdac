package dev.usbdac.internal

import dev.usbdac.VolumeState
import dev.usbdac.nativeCall

/** The calls [dev.usbdac.DacVolume] makes on the DAC; a seam so the logic runs without a device. */
internal interface VolumeBackend {
    fun get(): VolumeState
    fun setDb(db: Double): VolumeState
    fun setFraction(fraction: Double): VolumeState
    fun step(direction: Int, steps: Int): VolumeState
    fun setMute(muted: Boolean): VolumeState
    fun originalDb(): Double?
    fun handBack(): Boolean
}

/** [VolumeBackend] over the JNI functions of one DAC handle. */
internal class NativeVolumeBackend(private val dac: Long) : VolumeBackend {
    override fun get() = nativeCall { parseVolumeState(Native.volumeGet(dac)) }
    override fun setDb(db: Double) = nativeCall { parseVolumeState(Native.volumeSetDb(dac, db)) }
    override fun setFraction(fraction: Double) = nativeCall { parseVolumeState(Native.volumeSetFraction(dac, fraction)) }
    override fun step(direction: Int, steps: Int) = nativeCall { parseVolumeState(Native.volumeStep(dac, direction, steps)) }
    override fun setMute(muted: Boolean) = nativeCall { parseVolumeState(Native.volumeSetMute(dac, muted)) }
    override fun originalDb(): Double? = nativeCall { Native.volumeOriginalDb(dac) }.takeUnless { it.isNaN() }
    override fun handBack(): Boolean = nativeCall { Native.volumeHandBack(dac) }
}
