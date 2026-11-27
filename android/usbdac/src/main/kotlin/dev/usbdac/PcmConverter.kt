package dev.usbdac

import dev.usbdac.internal.ConvertBackend
import dev.usbdac.internal.NativeConvertBackend
import org.json.JSONObject
import java.io.Closeable
import java.nio.ByteBuffer
import java.util.concurrent.atomic.AtomicBoolean

/**
 * What a conversion does to the audio, from nothing to something irreversible. Only [NONE] and
 * [BIT_EXACT] keep a bit-perfect stream bit-perfect.
 */
public enum class ConversionKind(internal val wireName: String) {
    /** Nothing changes; the bytes pass through. */
    NONE("none"),

    /**
     * Every sample value is exactly what it was: widening to more bits (the new low bits are zero)
     * and moving a sample between slot sizes (24-bit packed in 3 bytes to 24-bit in 4).
     */
    BIT_EXACT("bit_exact"),

    /**
     * No information is lost, but the audio is not identical: mono duplicated to two channels, or
     * integers carried in a float that holds them exactly.
     */
    LOSSLESS("lossless"),

    /** Information is lost or rounded: fewer bits, floats to integers, a new sample rate. */
    LOSSY("lossy"),
    ;

    /** Whether a bit-perfect target stream is still bit-perfect with respect to the source. */
    public val preservesBitPerfect: Boolean get() = this == NONE || this == BIT_EXACT

    internal companion object {
        fun fromWireName(name: String): ConversionKind =
            entries.firstOrNull { it.wireName == name } ?: LOSSY // an unknown kind is not to be trusted
    }
}

/** What to do with the bits a narrowing cannot keep. */
public enum class NarrowingMode(internal val wireName: String) {
    /** Drop them: round toward negative infinity. The error is correlated with the signal. */
    TRUNCATE("truncate"),

    /**
     * Add triangular-PDF noise of one step (peak) before rounding to nearest: the error becomes
     * signal-independent noise at about -98 dBFS for 16 bits. The default.
     */
    TPDF_DITHER("tpdf"),
}

/** How much work the sample-rate converter does for how clean a result. */
public enum class ResampleQuality(internal val wireName: String) {
    /**
     * Stopband of about 110 dB, no aliasing of any band into the output, passband flat to 91% of the lower
     * Nyquist frequency. The default.
     */
    HIGH("high"),

    /** About 2.5 times cheaper: stopband about 84 dB, passband flat to 90%; content above it may alias into the top 10%. */
    FAST("fast"),
}

/** How samples are laid out in bytes: little-endian, interleaved, left-justified in their slot. */
public enum class PcmLayout(internal val bits: Int, internal val slotBytes: Int) {
    /** Signed 16-bit in 2 bytes. */
    PCM_16(16, 2),

    /** Signed 16-bit left-justified in a 3-byte slot; the low byte must be zero. */
    PCM_16_IN_24(16, 3),

    /** Signed 16-bit left-justified in a 4-byte slot; the low two bytes must be zero. */
    PCM_16_IN_32(16, 4),

    /** Signed 24-bit packed in 3 bytes. */
    PCM_24_PACKED(24, 3),

    /** Signed 24-bit left-justified in a 4-byte slot (the low byte is zero). */
    PCM_24_IN_32(24, 4),

    /** Signed 32-bit. */
    PCM_32(32, 4),

    /** 32-bit IEEE float, nominal range -1.0 to 1.0. */
    FLOAT_32(0, 4),
}

/**
 * The knobs of a conversion. The defaults are what the tests measured.
 *
 * @property narrowing Applied wherever bits are dropped (integer narrowing, float to integer, and
 *   the rounding of resampled values to integers).
 * @property quality Used for sample-rate conversion.
 * @property headroomDb Gain in dB (zero or negative, down to -60) applied after sample-rate
 *   conversion. The default is -3.5 dB, which covers the measured square-wave overshoot but
 *   does not guarantee all signals avoid clipping. Set 0 explicitly for unity gain at DC.
 * @property ditherSeed Seed of the dither's pseudo-random generator (not negative); `null` for the library's own. The same seed
 *   gives the same output.
 * @property sourceLayout The layout of the source samples when it is not the one
 *   [StreamRequest.bitDepth] implies (16: [PcmLayout.PCM_16], 24: [PcmLayout.PCM_24_PACKED], 32: [PcmLayout.PCM_32]).
 */
public data class ConversionOptions(
    val narrowing: NarrowingMode = NarrowingMode.TPDF_DITHER,
    val quality: ResampleQuality = ResampleQuality.HIGH,
    val headroomDb: Double = -3.5,
    val ditherSeed: Long? = null,
    val sourceLayout: PcmLayout? = null,
)

/** One step of a [ConversionPlan]. */
public data class ConversionStep(
    /** A stable identifier: `resample`, `widen`, `repack`, `narrow`, `int_to_float`, `quantise`, `mono_to_stereo`. */
    val code: String,
    val kind: ConversionKind,
    /** A phrase for a log or a status line. */
    val description: String,
)

/** What the sample-rate converter built for a plan. */
public data class ResamplerInfo(
    /** Phases of the polyphase structure (the upsampling factor of the reduced ratio). */
    val phases: Int,
    /** The decimation factor of the reduced ratio. */
    val decimation: Int,
    /** Taps per output sample. */
    val taps: Int,
    val passbandHz: Double,
    val stopbandHz: Double,
    val designAttenuationDb: Double,
)

/**
 * How to turn what the host has into what the DAC runs: the smallest chain of steps, how much damage
 * it does ([kind]), and a sentence for a status line ([description]).
 *
 * A plan is made by [between] for a request and the format a session negotiated, typically after
 * `RatePolicy.NEAREST_SUPPORTED` brought back a rate other than the one asked for. **A plan that is
 * not [ConversionKind.NONE] or [ConversionKind.BIT_EXACT] is not bit-perfect**, and
 * [ConvertingOutput.bitPerfect] says so.
 */
public class ConversionPlan internal constructor(
    internal val backend: ConvertBackend,
    internal val requestJson: String,
    /** The worst of the steps. */
    public val kind: ConversionKind,
    /** A sentence that says what is done and how much it costs. */
    public val description: String,
    public val steps: List<ConversionStep>,
    public val sourceSampleRate: Int,
    public val sourceChannels: Int,
    /** Bytes of one source frame: the unit [PcmConverter.convert] takes. */
    public val sourceFrameBytes: Int,
    public val targetSampleRate: Int,
    public val targetChannels: Int,
    /** Bytes of one target frame: the unit it produces. */
    public val targetFrameBytes: Int,
    /** Whether the sample rate changes. */
    public val resamples: Boolean,
    /**
     * Source frames the converter holds back (the resampler's latency, a few milliseconds), 0 when it does
     * not resample. The output at target time `n / targetSampleRate` is aligned with source time
     * `n / targetSampleRate` exactly, so positions need no correction; only the delay before audio is
     * heard grows by this much.
     */
    public val delayFrames: Int,
    /** The smallest output buffer [PcmConverter.convert] makes progress with. */
    public val minOutputBytes: Int,
    /** The filter, when [resamples]. */
    public val resampler: ResamplerInfo?,
) {
    /** True when every complete input buffer must satisfy the declared zero padding contract. */
    public val requiresPaddingValidation: Boolean = JSONObject(requestJson).getJSONObject("source").getJSONObject("sample").let {
        it.optString("kind") == "int" && it.getInt("bits") < it.getInt("slot_bytes") * 8
    }

    /** Whether a bit-perfect target stream is still bit-perfect with respect to the source. */
    public val preservesBitPerfect: Boolean get() = kind.preservesBitPerfect

    override fun toString(): String = "ConversionPlan($kind: $description)"

    public companion object {
        /**
         * Plans the conversion from what the host plays ([source]) to what the DAC runs ([target], the
         * format `DacSession.negotiate` returned). The source samples are in the layout of
         * [StreamRequest.bitDepth] unless [ConversionOptions.sourceLayout] says otherwise. The target's
         * layout is its [StreamFormat.validBits] left-justified in [StreamFormat.subslotBytes].
         *
         * Does no audio work and is cheap; run it wherever convenient.
         *
         * @throws DacException [DacErrorCode.UNSUPPORTED] for DoP and native DSD (converting them would destroy
         *   them), a channel mapping other than mono to stereo, or a rate ratio that needs too large a filter;
         *   [DacErrorCode.INVALID_ARGUMENT] for a rate or option out of range.
         */
        public fun between(
            source: StreamRequest,
            target: StreamFormat,
            options: ConversionOptions = ConversionOptions(),
        ): ConversionPlan = between(NativeConvertBackend, source, target, options)

        internal fun between(
            backend: ConvertBackend,
            source: StreamRequest,
            target: StreamFormat,
            options: ConversionOptions,
        ): ConversionPlan {
            if (source.encoding !in listOf(StreamEncoding.PCM, StreamEncoding.PCM_FLOAT32) || target.encoding != StreamEncoding.PCM) {
                throw DacException(
                    DacErrorCode.UNSUPPORTED,
                    "DoP and native DSD are never converted: a sample-rate or bit-depth change would destroy them",
                )
            }
            if (source.encoding == StreamEncoding.PCM_FLOAT32 && options.sourceLayout != null && options.sourceLayout != PcmLayout.FLOAT_32) {
                throw DacException(DacErrorCode.INVALID_ARGUMENT, "float PCM source requires FLOAT_32 layout")
            }
            val sourceLayout = if (source.encoding == StreamEncoding.PCM_FLOAT32) PcmLayout.FLOAT_32 else options.sourceLayout ?: when (source.bitDepth) {
                16 -> PcmLayout.PCM_16
                24 -> PcmLayout.PCM_24_PACKED
                32 -> PcmLayout.PCM_32
                else -> throw DacException(DacErrorCode.INVALID_ARGUMENT, "a source of ${source.bitDepth} bits is not 16, 24 or 32")
            }
            val request = JSONObject()
                .put("source", formatJson(source.sampleRate, source.channels, sourceLayout.bits, sourceLayout.slotBytes, sourceLayout == PcmLayout.FLOAT_32))
                .put("target", formatJson(target.sampleRate, target.channels, target.validBits, target.subslotBytes, false))
                .put("options", optionsJson(options))
                .toString()
            return parsePlan(backend, request)
        }

        private fun formatJson(rate: Int, channels: Int, bits: Int, slotBytes: Int, float: Boolean): JSONObject =
            JSONObject()
                .put("sample_rate", rate)
                .put("channels", channels)
                .put(
                    "sample",
                    if (float) {
                        JSONObject().put("kind", "float")
                    } else {
                        JSONObject().put("kind", "int").put("bits", bits).put("slot_bytes", slotBytes)
                    },
                )

        private fun optionsJson(options: ConversionOptions): JSONObject {
            if (options.ditherSeed != null && options.ditherSeed < 0) {
                throw DacException(DacErrorCode.INVALID_ARGUMENT, "ditherSeed must not be negative")
            }
            return JSONObject()
                .put("narrowing", options.narrowing.wireName)
                .put("quality", options.quality.wireName)
                .put("headroom_db", options.headroomDb)
                .put("dither_seed", options.ditherSeed ?: JSONObject.NULL)
        }

        private fun parsePlan(backend: ConvertBackend, request: String): ConversionPlan = nativeCall {
            val json = JSONObject(backend.plan(request))
            val source = json.getJSONObject("source")
            val target = json.getJSONObject("target")
            val steps = json.getJSONArray("steps")
            val filter = if (json.isNull("filter")) null else json.getJSONObject("filter")
            ConversionPlan(
                backend = backend,
                requestJson = request,
                kind = ConversionKind.fromWireName(json.getString("kind")),
                description = json.getString("description"),
                steps = List(steps.length()) { index ->
                    val step = steps.getJSONObject(index)
                    ConversionStep(
                        code = step.getString("code"),
                        kind = ConversionKind.fromWireName(step.getString("kind")),
                        description = step.getString("description"),
                    )
                },
                sourceSampleRate = source.getInt("sample_rate"),
                sourceChannels = source.getInt("channels"),
                sourceFrameBytes = json.getInt("source_frame_bytes"),
                targetSampleRate = target.getInt("sample_rate"),
                targetChannels = target.getInt("channels"),
                targetFrameBytes = json.getInt("target_frame_bytes"),
                resamples = json.getBoolean("resamples"),
                delayFrames = json.getInt("delay_frames"),
                minOutputBytes = json.getInt("min_output_bytes"),
                resampler = filter?.let {
                    ResamplerInfo(
                        phases = it.getInt("l"),
                        decimation = it.getInt("m"),
                        taps = it.getInt("taps"),
                        passbandHz = it.getDouble("passband_hz"),
                        stopbandHz = it.getDouble("stopband_hz"),
                        designAttenuationDb = it.getDouble("design_attenuation_db"),
                    )
                },
            )
        }
    }
}

/** What one call of [PcmConverter.convert] did, in bytes. */
public data class ConvertResult(
    /** Input bytes used: whole source frames. */
    val consumed: Int,
    /** Output bytes written: whole target frames. */
    val produced: Int,
)

/**
 * Converts audio from a plan's source format to its target format, a chunk at a time. Opt-in: the
 * direct USB path never converts anything, and a converter sits in front of an output (see
 * [ConvertingOutput]) or is used on its own.
 *
 * Every conversion but [ConversionKind.NONE] and [ConversionKind.BIT_EXACT] changes the audio; see
 * [ConversionPlan.description]. Nothing is allocated per chunk.
 *
 * ### Using it
 * [convert] takes whole frames from the front of the input and writes whole frames to the output, as
 * many as fit in either; keep what was not consumed. After the last chunk call [finish] until it
 * returns 0, which gives the resampler's tail, then [reset] to start another stream. A seek is a
 * [reset] too: the filter's history belongs to the old position.
 *
 * Output position needs no correction: frame `n` of the output is aligned with source time
 * `n * sourceRate / targetRate` exactly. The audio the resampler holds back is [ConversionPlan.delayFrames].
 *
 * ### Threading
 * Not thread-safe: use one converter from one thread at a time. [create] takes a few milliseconds for a
 * resampling plan (it synthesises the filter): call it off the main thread.
 */
public class PcmConverter private constructor(
    private val backend: ConvertBackend,
    private val handle: Long,
    /** What this converter does. */
    public val plan: ConversionPlan,
) : Closeable {
    private val closed = AtomicBoolean(false)

    /** Samples that exceeded full scale and were saturated, since the start or the last [reset]. */
    public val clippedSamples: Long get() = nativeCall { backend.clipped(handle) }

    /**
     * Converts from the input's position up to its limit into the output's position up to its limit, and
     * advances both positions. Both must be direct buffers, and must not overlap. The input storage must
     * remain immutable and output storage exclusively accessible, including aliases, throughout the call.
     *
     * @throws DacException [DacErrorCode.INVALID_STATE] after [finish] without a [reset], or after [close];
     *   [DacErrorCode.INVALID_ARGUMENT] for a heap buffer or nonzero declared padding bits.
     */
    public fun convert(input: ByteBuffer, output: ByteBuffer): ConvertResult {
        checkOpen()
        requireDirect(input, output)
        requireWritable(output)
        val packed = nativeCall {
            backend.process(handle, input, input.position(), input.remaining(), output, output.position(), output.remaining())
        }
        val consumed = (packed ushr 32).toInt()
        val produced = (packed and 0xffff_ffffL).toInt()
        input.position(input.position() + consumed)
        output.position(output.position() + produced)
        return ConvertResult(consumed, produced)
    }

    /**
     * Writes the end of the stream (the audio the resampler still holds) at the output's position, as
     * far as the output has room, and advances it. Returns the bytes written; call it until it returns 0.
     * After it, [convert] fails until [reset].
     */
    public fun finish(output: ByteBuffer): Int {
        checkOpen()
        requireDirect(output)
        requireWritable(output)
        val produced = nativeCall { backend.finish(handle, output, output.position(), output.remaining()) }
        output.position(output.position() + produced)
        return produced
    }

    /** Starts a new stream: the filter's history, the dither and the counters are cleared. */
    public fun reset() {
        checkOpen()
        nativeCall { backend.reset(handle) }
    }

    /** Releases the converter. Safe to call again. */
    override fun close() {
        if (!closed.compareAndSet(false, true)) return
        nativeCall { backend.close(handle) }
    }

    private fun checkOpen() {
        if (closed.get()) throw DacException(DacErrorCode.INVALID_STATE, "the converter is closed")
    }

    private fun requireWritable(buffer: ByteBuffer) {
        if (buffer.isReadOnly) throw DacException(DacErrorCode.INVALID_ARGUMENT, "output buffer is read-only")
    }

    private fun requireDirect(vararg buffers: ByteBuffer) {
        if (buffers.any { !it.isDirect }) {
            throw DacException(DacErrorCode.INVALID_ARGUMENT, "the converter works on direct ByteBuffers (ByteBuffer.allocateDirect)")
        }
    }

    public companion object {
        /**
         * Builds the converter for [plan]. Takes a few milliseconds for a resampling plan.
         *
         * @throws DacException [DacErrorCode.UNSUPPORTED] or [DacErrorCode.INVALID_ARGUMENT] as for [ConversionPlan.between].
         */
        public fun create(plan: ConversionPlan): PcmConverter {
            val handle = nativeCall { plan.backend.create(plan.requestJson) }
            return PcmConverter(plan.backend, handle, plan)
        }
    }
}
