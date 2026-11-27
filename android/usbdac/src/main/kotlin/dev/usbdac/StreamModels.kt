package dev.usbdac

/** How audio is encoded on the wire. */
public enum class StreamEncoding {
    /** Linear PCM. */
    PCM,

    /** Type I unsigned 8-bit PCM; one byte per sample and silence 0x80. */
    PCM_U8,

    /** Type I IEEE-754 binary32 PCM, exact little-endian bits including NaN payloads. */
    PCM_FLOAT32,

    /** DSD over PCM: DSD bits carried in 24-bit PCM words with alternating marker bytes. */
    DOP,

    /** Native DSD in standard UAC3 8-byte words or a legacy device-specific layout. */
    DSD_NATIVE,
}

/** What to do when the DAC cannot run the requested rate. */
public enum class RatePolicy {
    /** Fail with [DacErrorCode.UNSUPPORTED]. The default: the library never changes the rate itself. */
    EXACT,

    /**
     * Prefer the highest integer divisor, then the highest lower rate in the same family, then
     * the highest other lower rate, then the lowest higher rate. The returned [StreamFormat] carries that rate
     * ([StreamFormat.rateDiffersFromRequest]) and the host must resample; the stream is not
     * bit-perfect with respect to the source.
     */
    NEAREST_SUPPORTED,
}

/**
 * What the host wants to play. Build one with [pcm], [dop] or [dsdNative].
 *
 * @property sampleRate PCM rate in Hz; ignored for DSD.
 * @property bitDepth PCM bit depth (16, 24 or 32); ignored for DSD.
 * @property dsdBitRate DSD bit rate in Hz (2,822,400 for DSD64); `null` for PCM.
 */
public data class StreamRequest(
    val sampleRate: Int,
    val channels: Int,
    val bitDepth: Int,
    val encoding: StreamEncoding = StreamEncoding.PCM,
    val dsdBitRate: Int? = null,
    val ratePolicy: RatePolicy = RatePolicy.EXACT,
) {
    public companion object {
        /** A PCM stream. */
        public fun pcm(
            sampleRate: Int,
            channels: Int,
            bitDepth: Int,
            ratePolicy: RatePolicy = RatePolicy.EXACT,
        ): StreamRequest = StreamRequest(sampleRate, channels, bitDepth, StreamEncoding.PCM, null, ratePolicy)

        /** Type I unsigned 8-bit PCM. */
        public fun pcmU8(sampleRate: Int, channels: Int, ratePolicy: RatePolicy = RatePolicy.EXACT): StreamRequest =
            StreamRequest(sampleRate, channels, 8, StreamEncoding.PCM_U8, null, ratePolicy)

        /** Type I IEEE-754 binary32 PCM; the exact byte path performs no numeric conversion. */
        public fun pcmFloat32(sampleRate: Int, channels: Int, ratePolicy: RatePolicy = RatePolicy.EXACT): StreamRequest =
            StreamRequest(sampleRate, channels, 32, StreamEncoding.PCM_FLOAT32, null, ratePolicy)

        /**
         * A DoP stream. The carrier is 24-bit PCM at `dsdBitRate / 16`, so DSD64 needs a DAC
         * that runs 176.4 kHz and decodes DoP markers. UAC3 also uses PCM framing for DoP.
         */
        public fun dop(
            dsdBitRate: Int,
            channels: Int,
            ratePolicy: RatePolicy = RatePolicy.EXACT,
        ): StreamRequest = StreamRequest(dsdBitRate / 16, channels, 24, StreamEncoding.DOP, dsdBitRate, ratePolicy)

        /** A native DSD stream; the DAC must advertise a native DSD alternate setting. */
        public fun dsdNative(
            dsdBitRate: Int,
            channels: Int,
            ratePolicy: RatePolicy = RatePolicy.EXACT,
        ): StreamRequest = StreamRequest(0, channels, 0, StreamEncoding.DSD_NATIVE, dsdBitRate, ratePolicy)
    }
}

/**
 * The format that will go on the wire, as settled by [DacSession.negotiate]. It cannot be
 * built by hand: pass the one `negotiate` returned to [DacSession.openStream].
 */
public class StreamFormat internal constructor(
    internal val formatId: Long,
    /** How the audio is encoded. */
    public val encoding: StreamEncoding,
    /** DSD bit rate for [StreamEncoding.DSD_NATIVE]; `null` otherwise. */
    public val dsdBitRate: Int?,
    /**
     * The rate on the wire: the PCM rate, the DoP carrier rate, or for native DSD the rate of
     * subslot-sized words (`dsdBitRate / 64` for standard UAC3 DSD).
     */
    public val sampleRate: Int,
    public val channels: Int,
    /** Significant bits of each sample. */
    public val validBits: Int,
    /** Bytes per sample on the wire; 8 for standard UAC3 native DSD. */
    public val subslotBytes: Int,
    /** Bytes in one frame (`subslotBytes * channels`): the unit [PcmOutput.write] takes. */
    public val bytesPerFrame: Int,
    /**
     * The rate of the counters [PcmOutput.playedFrames] and [PcmOutput.latencyFrames]: the wire
     * frame rate, or `bitRate / 8` for native DSD.
     */
    public val counterRateHz: Int,
    public val interfaceNumber: Int,
    public val altSetting: Int,
    public val endpointAddress: Int,
    public val syncType: SyncType,
    /** Whether playback uses explicit rate reports. See [pacing] for implicit feedback. */
    public val explicitFeedback: Boolean,
    public val linkSpeed: LinkSpeed,
    public val serviceIntervalUs: Int,
    /** `true` when the DAC runs another rate than requested ([RatePolicy.NEAREST_SUPPORTED]). */
    public val rateDiffersFromRequest: Boolean,
    /**
     * How the stream is paced on its endpoint: what the library does, named. Consistent with
     * [syncType] and [explicitFeedback]. [Pacing.Unknown] from a native library that predates it.
     */
    public val pacing: Pacing = Pacing.Unknown,
    /**
     * The Clock Source the sample rate was set and verified on, and how it was reached from the
     * stream's terminal, as resolved by [DacSession.negotiate]. `null` when the DAC has no clock
     * entities to set (UAC 1.0 sets the rate on the endpoint), or from an older native library.
     */
    public val clock: ClockChoice? = null,
    /** Descriptor identities in interleaved USB wire order, shared with capture. */
    public val channelMap: List<CaptureChannel> = List(channels) { CaptureChannel(it, CaptureChannelPosition.UNSPECIFIED) },
) {
    override fun toString(): String =
        "StreamFormat($encoding ${sampleRate}Hz ${validBits}bit x$channels, $bytesPerFrame bytes/frame)"
}

/** State of a stream. */
public enum class StreamState(internal val code: Int) {
    /** Waiting for the ring to hold the start threshold. */
    BUFFERING(0),
    PLAYING(1),
    PAUSED(2),

    /** [DacStream.drain] was called; the buffered audio is playing out. */
    DRAINING(3),

    /** Everything written has played. */
    DRAINED(4),
    CLOSED(5),

    /** The stream failed. It stays open until closed; writes throw [DacErrorCode.INVALID_STATE]. */
    FAILED(6);

    internal companion object {
        fun fromCode(code: Int): StreamState? = entries.firstOrNull { it.code == code }
    }
}

/** Counters of a stream. Frame counts are in [StreamFormat.counterRateHz] units. */
public data class DacStreamStats(
    val state: StreamState?,
    val bitPerfect: Boolean,
    /** Why [bitPerfect] is false, if it is. */
    val bitPerfectBlocker: BitPerfectBlocker?,
    val softwareGain: Float,
    /** Runs of padding because the ring ran dry. */
    val underruns: Long,
    /** Frames queued to the DAC that it never took. */
    val lostFrames: Long,
    val shortPackets: Long,
    val zeroPackets: Long,
    val urbTimeouts: Long,
    val completedTransfers: Long,
    /** Frames the host has written. */
    val framesWritten: Long,
    /** Frames the DAC has taken, music and padding. */
    val framesCompleted: Long,
    /** Music frames completed on the bus since the start or the last flush. */
    val playedFrames: Long,
    /** Music frames between the host and the DAC. */
    val latencyFrames: Long,
    val latencyMs: Double,
    /** Frames in the ring. */
    val queuedFrames: Long,
    val bufferMs: Long,
    val bufferTargetMs: Long,
    val bufferCapacityMs: Long,
    /** Transfers currently submitted to the bus. */
    val transferQueueDepth: Int,
    val lastTurnaroundUs: Long,
    val maxTurnaroundUs: Long,
    val lastSubmitGapUs: Long,
    val maxSubmitGapUs: Long,
    /** The rate the DAC has actually consumed frames at since the start. */
    val effectiveRateHz: Double,
    val feedbackActive: Boolean,
    val feedbackReports: Long,
    val feedbackTimeouts: Long,
    val feedbackErrors: Long,
    val feedbackRejected: Long,
    /** Time since the last valid feedback report; `null` before the first one. */
    val feedbackAgeMs: Long?,
    /** The sample rate the last feedback report implies; `null` before the first one. */
    val feedbackRateHz: Double?,
    val feedbackStale: Boolean,
    /** Live explicit or implicit feedback has locked. See [StreamFormat.pacing] for the source. */
    val feedbackLocked: Boolean = false,
    /** Empty explicit polls or empty implicit IN patterns. Neither supplies a rate update. */
    val feedbackEmpty: Long = 0,
    val implicitFifoDepth: Int = 0,
    val implicitFifoHighWater: Int = 0,
    val implicitWaits: Long = 0,
    val implicitEmptyBus: Long = 0,
    val implicitOverruns: Long = 0,
    val implicitInTransfers: Long = 0,
    val implicitOutOfOrder: Long = 0,
    val implicitInvalidPackets: Long = 0,
    val implicitMeasuredFrames: Long = 0,
    val implicitScheduledFrames: Long = 0,
    /** Checkpoint intervals excluding event-pump time, longer than 10 ms. */
    val workerGapOver10ms: Long = 0,
    /** Checkpoint intervals excluding event-pump time, longer than 50 ms. */
    val workerGapOver50ms: Long = 0,
    /** Longest checkpoint interval excluding event-pump time, in microseconds. */
    val maxWorkerGapUs: Long = 0,
    /** Adaptive USB queue depth changes since the stream started. */
    val adaptiveQueueChanges: Long = 0,
    /** Current adaptive USB transfer slot target. */
    val adaptiveQueueSlots: Long = 0,
    /** Current adaptive USB queue target, in microseconds. */
    val adaptiveQueueUs: Long = 0,
    /** Longest checkpoint interval including event-pump time, in microseconds. */
    val maxWorkerLoopGapUs: Long = 0,
    /** Longest event-pump call including callbacks and scheduling, in microseconds. */
    val maxEventPumpElapsedUs: Long = 0,
    /** Largest event-pump elapsed time beyond its requested timeout, in microseconds. */
    val maxEventPumpTimeoutOverrunUs: Long = 0,
)
