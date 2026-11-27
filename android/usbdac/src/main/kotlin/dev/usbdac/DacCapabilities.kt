package dev.usbdac

/** USB Audio Class version of a DAC or of one of its alternate settings. */
public enum class UacVersion { V1, V2, V3, UNKNOWN }

/** USB link speed the DAC enumerated at. */
public enum class LinkSpeed { LOW, FULL, HIGH, SUPER, SUPER_PLUS, UNKNOWN }

/** What an alternate setting carries. */
public enum class AltFormat {
    /** Linear PCM. */
    PCM,

    /** DoP carrier without independently advertised linear PCM. */
    DOP,

    /** Native DSD: standard UAC3 8-byte words or a legacy device-specific layout. */
    DSD,

    /** Anything else, such as a compressed format. The library does not use it. */
    OTHER,
}

/** How the DAC's endpoint is synchronised with the host's packets. */
public enum class SyncType { ASYNCHRONOUS, ADAPTIVE, SYNCHRONOUS, NONE }

/** A span of sampling frequencies an alternate setting accepts. [stepHz] is 0 for a single rate. */
public data class RateRange(val minHz: Int, val maxHz: Int, val stepHz: Int)

/**
 * One playback configuration on an isochronous OUT alternate.
 * A UAC2 or UAC3 OUT alternate can have several entries with different implicit-feedback IN
 * alternates. The rates on each entry apply to the input named by [pacing].
 */
public data class AltSetting(
    val interfaceNumber: Int,
    val altSetting: Int,
    val endpointAddress: Int,
    val format: AltFormat,
    val channels: Int,
    /** Bytes per sample on the wire; standard UAC3 native DSD uses 8. */
    val subslotBytes: Int,
    /** Significant bits of each sample; standard UAC3 native DSD uses 64. */
    val validBits: Int,
    val rateRanges: List<RateRange>,
    /** The discrete rates this alternate setting runs. */
    val sampleRates: List<Int>,
    val syncType: SyncType,
    /** Address of the explicit feedback endpoint, if the DAC has one. */
    val feedbackEndpoint: Int?,
    val maxPacketBytes: Int,
    val serviceIntervalUs: Int,
    val uacVersion: UacVersion,
    /**
     * How a stream on this alternate setting would be paced, as [StreamFormat.pacing] will report
     * it. `null` when the library would not stream it with this configuration (an asynchronous
     * endpoint without a feedback endpoint while [DacConfig.allowUnsynchronizedAsync] is off, or
     * one that declares a feedback endpoint it cannot resolve), or from an older native library.
     */
    val pacing: Pacing? = null,
)

/** The range of the DAC's hardware volume control, in dB. */
public data class VolumeRange(val minDb: Double, val maxDb: Double, val resDb: Double)

/** What a DAC can do, read from its descriptors when the session opened. */
public data class DacCapabilities(
    val uacVersion: UacVersion,
    val linkSpeed: LinkSpeed,
    val altSettings: List<AltSetting>,
    /** Union of the rates over all PCM alternate settings. */
    val pcmSampleRates: List<Int>,
    val pcmBitDepths: List<Int>,
    val channelCounts: List<Int>,
    val supportsPcm: Boolean,
    /**
     * A feasible PCM DoP carrier: at least 24 valid bits at 176.4 kHz or more.
     * This does not prove that the device decodes DoP markers.
     */
    val supportsDop: Boolean,
    /** The highest DSD bit rate the PCM carrier can transport; marker decoding is device-dependent. */
    val dopMaxDsdBitRate: Int?,
    val supportsNativeDsd: Boolean,
    val nativeDsdBitRates: List<Int>,
    val hasExplicitFeedback: Boolean,
    /** The hardware volume range; `null` when the DAC has no writable volume control. */
    val volume: VolumeRange?,
    /**
     * The DAC has a usable hardware volume control (`volume != null`). Without one,
     * [DacSession.openStream] fails with [DacErrorCode.VOLUME_UNAVAILABLE] unless
     * [DacConfig.requireHardwareVolume] is off.
     */
    val hasHardwareVolume: Boolean,
    val hasMute: Boolean,
)
