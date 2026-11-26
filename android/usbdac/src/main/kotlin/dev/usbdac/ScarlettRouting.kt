package dev.usbdac

import dev.usbdac.internal.Native
import org.json.JSONArray
import org.json.JSONObject

/** Explicit opt-in to claim the exact native-proven vendor interface on the same USB connection. */
public enum class VendorRouting {
    SCARLETT_18I20_GEN2, SCARLETT_6I6_GEN2, SCARLETT_18I8_GEN2,
    SCARLETT_4I4_GEN3, SCARLETT_8I6_GEN3, SCARLETT_18I8_GEN3, SCARLETT_18I20_GEN3,
    VOCASTER_ONE, VOCASTER_TWO,
    SCARLETT_SOLO_GEN4, SCARLETT_2I2_GEN4, SCARLETT_4I4_GEN4,
    CLARETT_USB_2PRE, CLARETT_USB_4PRE, CLARETT_USB_8PRE,
    CLARETT_PLUS_2PRE, CLARETT_PLUS_4PRE, CLARETT_PLUS_8PRE,
    SCARLETT_16I16_GEN4, SCARLETT_18I16_GEN4, SCARLETT_18I20_GEN4, ISA_C8X,
}

public enum class ScarlettModel {
    SCARLETT_18I20_GEN2, SCARLETT_6I6_GEN2, SCARLETT_18I8_GEN2,
    SCARLETT_4I4_GEN3, SCARLETT_8I6_GEN3, SCARLETT_18I8_GEN3, SCARLETT_18I20_GEN3,
    VOCASTER_ONE, VOCASTER_TWO,
    SCARLETT_SOLO_GEN4, SCARLETT_2I2_GEN4, SCARLETT_4I4_GEN4,
    CLARETT_USB_2PRE, CLARETT_USB_4PRE, CLARETT_USB_8PRE,
    CLARETT_PLUS_2PRE, CLARETT_PLUS_4PRE, CLARETT_PLUS_8PRE,
    SCARLETT_16I16_GEN4, SCARLETT_18I16_GEN4, SCARLETT_18I20_GEN4, ISA_C8X,
}
public data class ScarlettCapability(val model: ScarlettModel, val name: String, val configurationId: Int,
    val interfaceId: Int, val notificationEndpoint: Int, val interfaceClaimed: Boolean)
public enum class ScarlettPortKind { OFF, ANALOGUE, SPDIF, ADAT, MIX, PCM }
/** Channel is one-based; OFF has no channel. */
public data class ScarlettPort(val kind: ScarlettPortKind, val channel: Int?)
public data class ScarlettMuxRoute(val source: ScarlettPort, val destination: ScarlettPort)
/** Exact unsigned wire coefficients, in one-based input order. */
public data class ScarlettMixRow(val output: Int, val coefficients: List<Int>)
/** GET_MUX argument zero only; this does not prove all sample-rate banks. */
public data class ScarlettReadback(val model: ScarlettModel, val firmware: Long, val mixerInputs: Int,
    val mixerOutputs: Int, val muxZero: List<ScarlettMuxRoute>, val mixes: List<ScarlettMixRow>)
/** Firmware-defined FCP pin; fixedSource is a source pin when routing is fixed. */
public data class FcpPort(val pin: Int, val name: String, val fixedSource: Int?)
/** Exact unsigned mux words, including padding, in firmware order. */
public data class FcpBank(val bank: Int, val rawWords: List<Long>)
/** Modern Focusrite firmware topology and exact unsigned raw readback. */
public data class FcpReadback(val model: ScarlettModel, val firmware: Long, val mixerInputs: Int,
    val mixerOutputs: Int, val sources: List<FcpPort>, val destinations: List<FcpPort>,
    val banks: List<FcpBank>, val mixes: List<ScarlettMixRow>)
/** Opaque native-observed baseline scoped to this session; at most eight are retained. */
public class FcpSnapshot internal constructor(internal val owner: ScarlettRouting, internal val handle: Long,
    public val readback: FcpReadback)
/** Firmware pin IDs; source zero disconnects the destination. */
public data class FcpRouteEdit(val bank: Int, val destination: Int, val source: Int) {
    init { require(bank in 0..2); require(destination in 1..65535); require(source in 0..65535) }
}
/** One-based mixer coordinates and a bounded wire coefficient. */
public data class FcpMixEdit(val output: Int, val input: Int, val coefficient: Int) {
    init { require(output in 1..255); require(input in 1..255); require(coefficient in 0..32613) }
}
public data class FcpReport(val original: FcpReadback, val verified: FcpSnapshot)
public enum class FcpTargetKind { BANK, MIX }
public data class FcpTarget(val kind: FcpTargetKind, val index: Int)
public data class FcpFailure(val kind: ScarlettFailureKind, val detail: String?, val target: FcpTarget?)
public data class FcpRollbackFailure(val target: FcpTarget, val cause: FcpFailure)
public class FcpException(message: String, public val fcpFailure: FcpFailure,
    public val fcpRollbackFailures: List<FcpRollbackFailure>) :
    ScarlettException(ScarlettErrorKind.FCP, message, null, null, null)
/** Half-dB units, -160 (-80 dB) through 24 (+12 dB). */
public data class ScarlettGainEdit(val input: Int, val halfDb: Int) {
    init { require(input in 1..25); require(halfDb in -160..24) }
}
public data class ScarlettMixReport(val original: ScarlettMixRow, val verified: ScarlettMixRow)
public enum class ScarlettErrorKind { READ, MIX, SESSION, DRIFT, UNSUPPORTED, MUX, FCP }
public enum class ScarlettFailureKind { IO, PROTOCOL, INVALID, DRIFT, VERIFICATION }
public data class ScarlettFailure(val kind: ScarlettFailureKind, val detail: String?, val actual: ScarlettMixRow?)
/** Original failure and failed verified restoration are reported independently. */
public open class ScarlettException(
    public val kind: ScarlettErrorKind,
    message: String,
    public val sessionCode: DacErrorCode?,
    public val causeFailure: ScarlettFailure?,
    public val rollbackFailure: ScarlettFailure?,
) : Exception(message)

/** Complete native-observed bank; raw words include exact padding and wire order. */
public data class ScarlettMuxBank(val bank: Int, val rawWords: List<Long>, val routes: List<ScarlettMuxRoute>)
public data class ScarlettMuxReadback(val model: ScarlettModel, val firmware: Long, val banks: List<ScarlettMuxBank>)
/** Opaque native baseline, owned by one Scarlett session. At most eight are retained. */
public class ScarlettMuxSnapshot internal constructor(internal val owner: ScarlettRouting, internal val handle: Long,
    public val readback: ScarlettMuxReadback)
/** Bank 0 = base rate, 1 = double rate, 2 = quad rate. */
public data class ScarlettMuxEdit(val bank: Int, val destination: ScarlettPort, val source: ScarlettPort) {
    init { require(bank in 0..2) }
}
public data class ScarlettMuxReport(val original: ScarlettMuxReadback, val verified: ScarlettMuxSnapshot)
public data class ScarlettMuxFailure(val kind: ScarlettFailureKind, val detail: String?,
    val actual: ScarlettMuxReadback?, val bank: Int?, val actualBank: ScarlettMuxBank?)
public data class ScarlettMuxRollbackFailure(val bank: Int, val cause: ScarlettMuxFailure)
/** Preserves the original mux failure and each failed verified bank restoration. */
public class ScarlettMuxException(message: String, public val muxFailure: ScarlettMuxFailure,
    public val muxRollbackFailures: List<ScarlettMuxRollbackFailure>) :
    ScarlettException(ScarlettErrorKind.MUX, message, null, null, null)

/**
 * Source-backed, physically unvalidated Scarlett controls. Every operation requires an idle
 * session and the explicitly opted-in vendor claim. Rust owns firmware admission and verified
 * transactions; no raw request API exists. Call blocking operations off the main thread.
 */
public class ScarlettRouting internal constructor(private val dac: Long, private val checkOpen: () -> Unit) {
    public fun capability(): ScarlettCapability? {
        checkOpen()
        return response { Native.scarlettCapability(dac) }.optJSONObject("capability")?.let(::parseScarlettCapability)
    }
    public fun read(): ScarlettReadback {
        checkOpen()
        val v = response { Native.scarlettRead(dac) }
        return ScarlettReadback(ScarlettModel.valueOf(v.getString("model").uppercase()), v.getLong("firmware"),
            v.getInt("mixer_inputs"), v.getInt("mixer_outputs"), v.getJSONArray("mux_zero").objects().map {
                ScarlettMuxRoute(port(it.getJSONObject("source")), port(it.getJSONObject("destination")))
            }, v.getJSONArray("mixes").objects().map(::row))
    }
    /** Stateless read of modern firmware-defined FCP routing; preserves retained edit tokens. */
    public fun readFcp(): FcpReadback {
        checkOpen()
        return parseFcpReadback(response { Native.fcpRead(dac) })
    }
    /** Read and retain a bounded opaque baseline for typed edits. */
    public fun readFcpSnapshot(): FcpSnapshot {
        checkOpen()
        return fcpSnapshot(response { Native.fcpReadSnapshot(dac) })
    }
    /** Apply typed edits against native-retained proof; fresh native state is checked before writes. */
    public fun applyFcp(expected: FcpSnapshot, routes: List<FcpRouteEdit>, mixes: List<FcpMixEdit>): FcpReport {
        checkOpen()
        require(expected.owner === this) { "FCP snapshot belongs to another session" }
        require(routes.size <= 3 * 255 && mixes.size <= 4096 && (routes.isNotEmpty() || mixes.isNotEmpty()))
        require(routes.map { it.bank to it.destination }.distinct().size == routes.size)
        require(mixes.map { it.output to it.input }.distinct().size == mixes.size)
        val edits = JSONObject().put("routes", JSONArray(routes.map { JSONObject().put("bank", it.bank)
            .put("destination", it.destination).put("source", it.source) }))
            .put("mixes", JSONArray(mixes.map { JSONObject().put("output", it.output)
                .put("input", it.input).put("coefficient", it.coefficient) }))
        val v = response { Native.fcpApply(dac, expected.handle, edits.toString()) }
        return FcpReport(parseFcpReadback(v.getJSONObject("original")), fcpSnapshot(v.getJSONObject("verified")))
    }
    private fun fcpSnapshot(v: JSONObject): FcpSnapshot = FcpSnapshot(this, v.getLong("snapshot_id"), parseFcpReadback(v))
    /** Probe complete bank geometry. Legacy read().muxZero remains independent. */
    public fun readMux(): ScarlettMuxSnapshot {
        checkOpen()
        return muxSnapshot(response { Native.scarlettMuxRead(dac) })
    }
    /** Native retained raw banks are the baseline; Rust rereads exact proof before any SET. */
    public fun applyMux(expected: ScarlettMuxSnapshot, edits: List<ScarlettMuxEdit>): ScarlettMuxReport {
        checkOpen()
        require(expected.owner === this) { "Scarlett mux snapshot belongs to another session" }
        require(edits.size in 1..256)
        require(edits.map { it.bank to it.destination }.distinct().size == edits.size)
        val json = JSONArray(edits.map { JSONObject().put("bank", it.bank)
            .put("destination", it.destination.json()).put("source", it.source.json()) })
        val v = response { Native.scarlettMuxApply(dac, expected.handle, json.toString()) }
        return ScarlettMuxReport(muxReadback(v.getJSONObject("original")), muxSnapshot(v.getJSONObject("verified")))
    }
    private fun muxSnapshot(v: JSONObject): ScarlettMuxSnapshot = ScarlettMuxSnapshot(this, v.getLong("snapshot_id"), muxReadback(v))
    /** Keep the complete fresh expected row; untouched raw coefficients are preserved exactly. */
    public fun applyMix(expected: ScarlettMixRow, gains: List<ScarlettGainEdit>): ScarlettMixReport {
        checkOpen()
        require(expected.output in 1..12 && expected.coefficients.size in 1..25 && expected.coefficients.all { it in 0..65535 })
        require(gains.size in 1..expected.coefficients.size && gains.map { it.input }.distinct().size == gains.size)
        val edit = JSONObject().put("expected", JSONObject().put("output", expected.output).put("coefficients", JSONArray(expected.coefficients)))
            .put("gains", JSONArray(gains.map { JSONObject().put("input", it.input).put("half_db", it.halfDb) }))
        val v = response { Native.scarlettMixApply(dac, edit.toString()) }
        return ScarlettMixReport(row(v.getJSONObject("original")), row(v.getJSONObject("verified")))
    }
}
internal fun parseScarlettCapability(v: JSONObject): ScarlettCapability = ScarlettCapability(
    ScarlettModel.valueOf(v.getString("model").uppercase()), v.getString("name"), v.getInt("configuration_id"),
    v.getInt("interface"), v.getInt("notification_endpoint"), v.getBoolean("interface_claimed"))
internal fun parseFcpReadback(v: JSONObject): FcpReadback = FcpReadback(
    ScarlettModel.valueOf(v.getString("model").uppercase()), v.getLong("firmware"),
    v.getInt("mixer_inputs"), v.getInt("mixer_outputs"),
    v.getJSONArray("sources").objects().map(::fcpPort),
    v.getJSONArray("destinations").objects().map(::fcpPort),
    v.getJSONArray("banks").objects().map { FcpBank(it.getInt("bank"),
        it.getJSONArray("raw_words").let { a -> (0 until a.length()).map(a::getLong) }) },
    v.getJSONArray("mixes").objects().map(::row))
private fun fcpPort(v: JSONObject): FcpPort = FcpPort(v.getInt("pin"), v.getString("name"),
    if (v.isNull("fixed_source")) null else v.getInt("fixed_source"))
private fun JSONArray.objects(): List<JSONObject> = (0 until length()).map(::getJSONObject)
private fun row(v: JSONObject): ScarlettMixRow = ScarlettMixRow(v.getInt("output"), v.getJSONArray("coefficients").let { a -> (0 until a.length()).map(a::getInt) })
private fun port(v: JSONObject): ScarlettPort = ScarlettPort(ScarlettPortKind.valueOf(v.getString("kind").uppercase()), if (v.has("channel")) v.getInt("channel") else null)
private fun failure(v: JSONObject): ScarlettFailure = ScarlettFailure(ScarlettFailureKind.valueOf(v.getString("kind").uppercase()),
    v.optString("detail").takeIf(String::isNotEmpty), v.optJSONObject("actual")?.let(::row))
private fun ScarlettPort.json(): JSONObject {
    require(if (kind == ScarlettPortKind.OFF) channel == null else channel != null && channel in 1..255)
    return JSONObject().put("kind", kind.name.lowercase()).also { if (channel != null) it.put("channel", channel) }
}
private fun muxBank(v: JSONObject): ScarlettMuxBank = ScarlettMuxBank(v.getInt("bank"),
    v.getJSONArray("raw_words").let { a -> (0 until a.length()).map(a::getLong) },
    v.getJSONArray("routes").objects().map { ScarlettMuxRoute(port(it.getJSONObject("source")), port(it.getJSONObject("destination"))) })
private fun muxReadback(v: JSONObject): ScarlettMuxReadback = ScarlettMuxReadback(
    ScarlettModel.valueOf(v.getString("model").uppercase()), v.getLong("firmware"), v.getJSONArray("banks").objects().map(::muxBank))
private fun muxFailure(v: JSONObject): ScarlettMuxFailure = ScarlettMuxFailure(
    ScarlettFailureKind.valueOf(v.getString("kind").uppercase()), v.optString("detail").takeIf(String::isNotEmpty),
    v.optJSONObject("actual")?.let(::muxReadback), if (v.has("bank")) v.getInt("bank") else null,
    v.optJSONObject("actual_bank")?.let(::muxBank))
private fun fcpTarget(v: JSONObject): FcpTarget = FcpTarget(FcpTargetKind.valueOf(v.getString("kind").uppercase()), v.getInt("index"))
internal fun parseFcpFailure(v: JSONObject): FcpFailure = FcpFailure(
    ScarlettFailureKind.valueOf(v.getString("kind").uppercase()), v.optString("detail").takeIf(String::isNotEmpty),
    v.optJSONObject("target")?.let(::fcpTarget))
private fun response(call: () -> String): JSONObject = nativeCall {
    val result = JSONObject(call())
    result.optJSONObject("error")?.let { e ->
        if (e.getString("kind") == "fcp") {
            throw FcpException(e.getString("detail"), parseFcpFailure(e.getJSONObject("cause")),
                e.getJSONArray("rollback_failures").objects().map {
                    FcpRollbackFailure(fcpTarget(it.getJSONObject("target")), parseFcpFailure(it.getJSONObject("cause"))) })
        }
        if (e.getString("kind") == "mux") {
            throw ScarlettMuxException(e.getString("detail"), muxFailure(e.getJSONObject("cause")),
                e.getJSONArray("rollback_failures").objects().map { ScarlettMuxRollbackFailure(it.getInt("bank"), muxFailure(it.getJSONObject("cause"))) })
        }
        throw ScarlettException(ScarlettErrorKind.valueOf(e.getString("kind").uppercase()), e.getString("detail"),
            e.optString("session_code").takeIf(String::isNotEmpty)?.let { DacErrorCode.fromWireName(it) },
            e.optJSONObject("cause")?.let(::failure), e.optJSONObject("rollback_failure")?.let(::failure))
    }
    result.getJSONObject("value")
}
