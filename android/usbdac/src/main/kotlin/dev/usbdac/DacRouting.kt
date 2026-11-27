package dev.usbdac

import dev.usbdac.internal.Native
import org.json.JSONArray
import org.json.JSONObject

/** Descriptor-backed UAC2 controls. Every pin/channel uses one-based wire order. */
public sealed interface RouteKey {
    public val audioControlInterface: Int
    public val entityId: Int
    public data class Selector(override val audioControlInterface: Int, override val entityId: Int) : RouteKey
    public data class Crosspoint(override val audioControlInterface: Int, override val entityId: Int,
        val inputPin: Int, val inputChannel: Int, val outputChannel: Int) : RouteKey
}
public sealed interface RouteValue {
    public data class SelectorPin(val pin: Int) : RouteValue
    /** Signed 1/256 dB; -32768 is silence. */
    public data class MixerGain(val raw: Int) : RouteValue
}
public enum class RouteAccess { ABSENT, READ_ONLY, READ_WRITE }
public data class RouteControl(val key: RouteKey, val access: RouteAccess, val sourceEntityIds: List<Int>)
/** Kept by the owning native DAC. Rediscover after a selector edit, drift or eviction. */
public class RoutingSnapshot internal constructor(internal val owner: DacRouting, internal val handle: Long,
    public val configurationId: Int, public val controls: List<RouteControl>)
public data class RouteEdit(val key: RouteKey, val expected: RouteValue, val value: RouteValue)
public data class RoutingReport(val changed: List<RouteKey>, val unchanged: List<RouteKey>)
public enum class RoutingErrorKind { SESSION, INVALID, UNSUPPORTED_TOPOLOGY, DRIFT, CONTROL, TRANSACTION, SCARLETT }
public enum class RoutingControlErrorKind { INVALID, IO, TRANSFER_LENGTH, READBACK_MISMATCH }
public data class RoutingControlFailure(val kind: RoutingControlErrorKind, val detail: String?,
    val expectedLength: Int?, val actualLength: Int?, val expected: RouteValue?, val actual: RouteValue?)
public data class RoutingRollbackFailure(val key: RouteKey, val cause: RoutingControlFailure)
/** A failed transaction preserves the failed edit and every failed verified rollback. */
public class RoutingException @JvmOverloads constructor(public val kind: RoutingErrorKind, message: String,
    public val sessionCode: DacErrorCode?, public val failedChange: Int?,
    public val controlFailure: RoutingControlFailure?, public val rollbackFailures: List<RoutingRollbackFailure>,
    /** Scoped ADC3 power hand-back also failed; the primary error fields remain intact. */
    public val powerRestoreFailure: String? = null) : Exception(message)

/**
 * Typed routing through this session's checked native controls while idle.
 * Native serialization excludes playback, capture and clock negotiation; stopping a caller's
 * stream remains explicit. The configured HandBack policy controls route restoration.
 * Call these blocking methods off the main thread. At most eight snapshots are retained.
 */
public class DacRouting internal constructor(private val dac: Long, private val checkOpen: () -> Unit) {
    public val pioneer: PioneerDjmRouting = PioneerDjmRouting(dac, checkOpen)
    public val scarlett: ScarlettRouting = ScarlettRouting(dac, checkOpen)

    /** Explicit opt-in wakes affected full ADC3 domains temporarily; default never wakes. */
    @JvmOverloads
    public fun discover(powered: Boolean = false): RoutingSnapshot {
        checkOpen()
        val value = routingResponse { if (powered) Native.routingDiscoverPowered(dac) else Native.routingDiscover(dac) }
        val controls = value.getJSONArray("controls").objects().map {
            RouteControl(parseKey(it.getJSONObject("key")), RouteAccess.valueOf(it.getString("access").uppercase()),
                it.getJSONArray("sources").let { array -> (0 until array.length()).map(array::getInt) })
        }
        return RoutingSnapshot(this, value.getLong("snapshot_id"), value.getInt("configuration_id"), controls)
    }
    /** Reacquire a scoped ADC3 lease when explicitly requested; snapshots hold no power. */
    @JvmOverloads
    public fun read(snapshot: RoutingSnapshot, key: RouteKey, powered: Boolean = false): RouteValue {
        check(snapshot)
        return parseValue(routingResponse { if (powered) Native.routingReadPowered(dac, snapshot.handle, key.json().toString()) else Native.routingRead(dac, snapshot.handle, key.json().toString()) })
    }
    /** Preserve verified routing/rollback semantics while temporarily waking ADC3 domains. */
    @JvmOverloads
    public fun apply(snapshot: RoutingSnapshot, edits: List<RouteEdit>, powered: Boolean = false): RoutingReport {
        check(snapshot)
        require(edits.size <= 32) { "at most 32 route edits" }
        val json = JSONArray(edits.map { JSONObject().put("key", it.key.json()).put("expected", it.expected.json()).put("value", it.value.json()) })
        val result = routingResponse { if (powered) Native.routingApplyPowered(dac, snapshot.handle, json.toString()) else Native.routingApply(dac, snapshot.handle, json.toString()) }
        return RoutingReport(result.getJSONArray("changed").objects().map(::parseKey), result.getJSONArray("unchanged").objects().map(::parseKey))
    }
    private fun check(snapshot: RoutingSnapshot) {
        checkOpen()
        require(snapshot.owner === this) { "routing snapshot belongs to another session" }
    }
}
private fun JSONArray.objects(): List<JSONObject> = (0 until length()).map(::getJSONObject)
private fun RouteKey.json(): JSONObject = JSONObject().put("interface", audioControlInterface).put("entity", entityId).also {
    when (this) {
        is RouteKey.Selector -> it.put("kind", "selector")
        is RouteKey.Crosspoint -> it.put("kind", "crosspoint").put("input_pin", inputPin).put("input_channel", inputChannel).put("output_channel", outputChannel)
    }
}
private fun RouteValue.json(): JSONObject = when (this) {
    is RouteValue.SelectorPin -> JSONObject().put("kind", "selector_pin").put("value", pin)
    is RouteValue.MixerGain -> JSONObject().put("kind", "mixer_gain").put("value", raw)
}
private fun parseKey(value: JSONObject): RouteKey = when (value.getString("kind")) {
    "selector" -> RouteKey.Selector(value.getInt("interface"), value.getInt("entity"))
    "crosspoint" -> RouteKey.Crosspoint(value.getInt("interface"), value.getInt("entity"), value.getInt("input_pin"), value.getInt("input_channel"), value.getInt("output_channel"))
    else -> throw org.json.JSONException("unknown route key")
}
private fun parseValue(value: JSONObject): RouteValue = when (value.getString("kind")) {
    "selector_pin" -> RouteValue.SelectorPin(value.getInt("value"))
    "mixer_gain" -> RouteValue.MixerGain(value.getInt("value"))
    else -> throw org.json.JSONException("unknown route value")
}
private fun JSONObject.optionalInt(key: String): Int? = if (isNull(key)) null else getInt(key)
private fun controlFailure(value: JSONObject): RoutingControlFailure = RoutingControlFailure(
    RoutingControlErrorKind.valueOf(value.getString("kind").uppercase()), value.optString("detail").takeIf { it.isNotEmpty() },
    (value.opt("expected") as? Number)?.toInt(), (value.opt("actual") as? Number)?.toInt(),
    value.optJSONObject("expected")?.let(::parseValue), value.optJSONObject("actual")?.let(::parseValue))
internal fun routingResponse(call: () -> String): JSONObject = nativeCall {
    val result = JSONObject(call())
    result.optJSONObject("error")?.let { error ->
        throw RoutingException(RoutingErrorKind.valueOf(error.getString("kind").uppercase()), error.getString("detail"),
            error.optString("session_code").takeIf { it.isNotEmpty() }?.let { DacErrorCode.fromWireName(it) }, error.optionalInt("failed_change"),
            error.optJSONObject("cause")?.let(::controlFailure), error.optJSONArray("rollback_failures")?.objects()?.map {
                RoutingRollbackFailure(parseKey(it.getJSONObject("key")), controlFailure(it.getJSONObject("cause")))
            } ?: emptyList(), error.optString("power_restore_failure").takeIf { it.isNotEmpty() })
    }
    result.getJSONObject("value")
}
