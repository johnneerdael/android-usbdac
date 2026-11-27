package dev.usbdac

import dev.usbdac.internal.Native
import org.json.JSONObject
import java.util.Collections

/** Exact source-backed Pioneer DJM routing identities. Native USB admission remains authoritative. */
public enum class PioneerDjmModel(internal val wireName: String) {
    DJM_250MK2("djm-250mk2"), DJM_450("djm-450"), DJM_750("djm-750"), DJM_850("djm-850"),
    DJM_900NXS2("djm-900nxs2"), DJM_750MK2("djm-750mk2"), DJM_V10("djm-v10");
}

/** Sparse controls with one-based stereo-pair numbers. */
public sealed interface PioneerDjmControlKey {
    public data object CaptureLevel : PioneerDjmControlKey
    public data class CaptureSource(val pair: Int) : PioneerDjmControlKey {
        init { require(pair in 1..255) }
    }
    public data class PlaybackDestination(val pair: Int) : PioneerDjmControlKey {
        init { require(pair in 1..255) }
    }
}

/** An available choice, with its native label. The id is scoped to its model and control. */
public data class PioneerDjmOption(val id: Int, val label: String) {
    init { require(id in 0..255) }
}
public data class PioneerDjmControl(val key: PioneerDjmControlKey, val options: List<PioneerDjmOption>)

/**
 * Available controls and options bound to the session that discovered them.
 */
public class PioneerDjmProfile internal constructor(
    internal val owner: PioneerDjmRouting,
    public val model: PioneerDjmModel,
    public val configurationId: Int,
    public val controls: List<PioneerDjmControl>,
)

/**
 * Explicit source-backed DJM routing through an already opened session. Call these blocking
 * operations off the main thread while playback and capture are idle. Native serialization
 * excludes streaming and negotiation. Uses the session's existing claims.
 * Hardware behaviour remains physically unvalidated.
 */
public class PioneerDjmRouting internal constructor(private val dac: Long, private val checkOpen: () -> Unit) {
    /** Discover this exact model's available sparse choices without changing routing. */
    public fun discover(): PioneerDjmProfile {
        checkOpen()
        return parsePioneerDjmProfile(this, routingResponse { Native.pioneerDjmDiscover(dac) })
    }

    /**
     * Apply one advertised choice and return on USB acknowledgement. The native session
     * rechecks model, configuration and choice. Hardware state remains unknown.
     */
    public fun apply(profile: PioneerDjmProfile, key: PioneerDjmControlKey, option: PioneerDjmOption) {
        checkOpen()
        require(profile.owner === this) { "Pioneer DJM profile belongs to another session" }
        require(profile.controls.any { it.key == key && option in it.options }) {
            "Pioneer DJM option is not advertised for this control"
        }
        val edit = JSONObject().put("model", profile.model.wireName)
            .put("configuration_id", profile.configurationId).put("key", key.json()).put("option_id", option.id)
        routingResponse { Native.pioneerDjmApply(dac, edit.toString()) }
    }
}

private fun PioneerDjmControlKey.json(): JSONObject = when (this) {
    PioneerDjmControlKey.CaptureLevel -> JSONObject().put("kind", "capture_level")
    is PioneerDjmControlKey.CaptureSource -> JSONObject().put("kind", "capture_source").put("pair", pair)
    is PioneerDjmControlKey.PlaybackDestination -> JSONObject().put("kind", "playback_destination").put("pair", pair)
}
internal fun parsePioneerDjmKey(v: JSONObject): PioneerDjmControlKey {
    val kind = v.getString("kind")
    require(v.length() == if (kind == "capture_level") 1 else 2)
    return when (kind) {
        "capture_level" -> PioneerDjmControlKey.CaptureLevel
        "capture_source" -> PioneerDjmControlKey.CaptureSource(v.getInt("pair"))
        "playback_destination" -> PioneerDjmControlKey.PlaybackDestination(v.getInt("pair"))
        else -> throw IllegalArgumentException("Unknown Pioneer DJM control kind")
    }
}
internal fun parsePioneerDjmProfile(owner: PioneerDjmRouting, v: JSONObject): PioneerDjmProfile {
    val model = PioneerDjmModel.entries.single { it.wireName == v.getString("model") }
    val configuration = v.getInt("configuration_id")
    require(configuration in 1..255)
    val controls = v.getJSONArray("controls").let { a -> (0 until a.length()).map { index ->
        val control = a.getJSONObject(index)
        val options = control.getJSONArray("options").let { o -> (0 until o.length()).map { option ->
            o.getJSONObject(option).let { PioneerDjmOption(it.getInt("id"), it.getString("label")) }
        } }
        require(options.isNotEmpty() && options.map { it.id }.distinct().size == options.size)
        PioneerDjmControl(parsePioneerDjmKey(control.getJSONObject("key")), Collections.unmodifiableList(options))
    } }
    require(controls.isNotEmpty() && controls.map { it.key }.distinct().size == controls.size)
    return PioneerDjmProfile(owner, model, configuration, Collections.unmodifiableList(controls))
}
