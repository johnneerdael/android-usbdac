package dev.usbdac.sample.player

import dev.usbdac.media3.*

import android.content.Context
import dev.usbdac.DacErrorCode
import dev.usbdac.DacEvent
import dev.usbdac.DacException
import dev.usbdac.DacVolume
import dev.usbdac.OutputChange
import dev.usbdac.OutputKind
import dev.usbdac.OutputPolicy
import dev.usbdac.OutputSelector
import dev.usbdac.PcmOutput
import dev.usbdac.SelectedOutput
import dev.usbdac.StreamRequest
import dev.usbdac.WriteFormat
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.ExperimentalCoroutinesApi
import kotlinx.coroutines.Job
import kotlinx.coroutines.flow.Flow
import kotlinx.coroutines.flow.MutableSharedFlow
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.SharedFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.flatMapLatest
import kotlinx.coroutines.flow.launchIn
import kotlinx.coroutines.flow.onEach
import kotlin.math.log10

// region controller-head
/**
 * Owns the selector and the output in use for the whole process, and tells the rest of the app what
 * happened. The service creates it once; the settings screen reads [selected] and [notice].
 */
class DirectOutputController internal constructor(
    settings: DirectOutputSettings,
    private val scope: CoroutineScope,
    private val selectorFactory: (OutputPolicy) -> OutputSelector,
) : OutputSource {
    constructor(context: Context, settings: DirectOutputSettings, scope: CoroutineScope) :
        this(settings, scope, { policy -> OutputSelector(context, policy) })
    private val lock = Any()
    private var settings = settings
    private var directRouteRequired = false
    private fun selector() = selectorFactory(settings.toPolicy().let {
        if (directRouteRequired) it.copy(force = OutputKind.DIRECT_USB) else it
    })
    private val selectors = MutableStateFlow(selector())
    @Volatile
    private var supportSnapshot = selectors.value.pcmSupportSnapshot()
    private var current: OpenedOutput? = null
    private var watching: Job? = null
    private var multiplier = 1f
    private var levelBeforeDucking: Double? = null

    @Volatile
    private var lookAgain = false

    val volume: StateFlow<DacVolume?> = MutableStateFlow(null)

    private val mutableSelected = MutableStateFlow<SelectedOutput?>(null)

    /** The library's output while one is open: its decision, its volume and its bit-perfect state. */
    val selected: StateFlow<SelectedOutput?> get() = mutableSelected

    private val mutableNotice = MutableStateFlow<String?>(null)

    /** A sentence for the listener, such as why playback paused. */
    val notice: StateFlow<String?> get() = mutableNotice

    private val mutableEvents = MutableSharedFlow<DacEvent>(extraBufferCapacity = 16)

    /** The events of the open DAC session: `DeviceLost`, `StreamFailed`, `LostFrames`, `Underrun`. */
    val events: SharedFlow<DacEvent> get() = mutableEvents

    /** A DAC attached, detached or allowed, or a route that came or went. Debounce it. */
    @OptIn(ExperimentalCoroutinesApi::class)
    val changes: Flow<OutputChange> = selectors.flatMapLatest { it.changes }

    /** Takes effect at the next flush or format change, never in the middle of a track. */
    fun apply(settings: DirectOutputSettings) = synchronized(lock) {
        this.settings = settings
        if (!settings.takeOverDac) directRouteRequired = false
        val selector = selector()
        selectors.value = selector
        supportSnapshot = selector.pcmSupportSnapshot()
        lookAgain = true
    }

    /** Call when [changes] fires. */
    fun onChanged() {
        selectors.value.invalidatePcmSupport()
        supportSnapshot = selectors.value.pcmSupportSnapshot()
        lookAgain = true
    }

    override fun takeLookAgain(): Boolean = lookAgain.also { lookAgain = false }

    override fun formatSupport(request: StreamRequest): OutputSupport = when (supportSnapshot.support(request)) {
        OutputSelector.PcmSupport.DIRECT -> OutputSupport(OutputFormatSupport.DIRECT, request, request, "cached USB alternate")
        OutputSelector.PcmSupport.WITH_PROCESSING -> OutputSupport(OutputFormatSupport.WITH_TRANSCODING, request)
        OutputSelector.PcmSupport.UNKNOWN -> OutputSupport(OutputFormatSupport.UNKNOWN, request)
        OutputSelector.PcmSupport.UNSUPPORTED -> OutputSupport(OutputFormatSupport.UNSUPPORTED, request)
    }
    // endregion

    // region controller-open
    override fun open(request: StreamRequest): OpenedOutput = synchronized(lock) {
        check(current == null) { "close the output in use before opening another" }
        lookAgain = false
        val selected = selectors.value.open(request)
        supportSnapshot = selectors.value.pcmSupportSnapshot()
        if (selected.decision.kind == OutputKind.DIRECT_USB && !directRouteRequired) {
            directRouteRequired = true
            selectors.value = selector()
            supportSnapshot = selectors.value.pcmSupportSnapshot()
        }
        val opened = OpenedOutput(selected, selected.format, request, selected)
        current = opened
        mutableSelected.value = selected
        (volume as MutableStateFlow).value = selected.volume
        mutableNotice.value = null
        applyMultiplier(selected.volume)
        watching = selected.session?.events
            ?.onEach { event ->
                when (event) {
                    is DacEvent.DeviceLost ->
                        opened.failure = DacException(DacErrorCode.DEVICE_LOST, "the DAC was unplugged")
                    is DacEvent.StreamFailed -> opened.failure = event.error
                    else -> Unit
                }
                mutableEvents.tryEmit(event)
            }
            ?.launchIn(scope)
        opened
    }

    override fun canRecover(error: DacException): Boolean =
        error.code == DacErrorCode.TRANSFER

    override fun close(output: OpenedOutput) = synchronized(lock) {
        watching?.cancel()
        watching = null
        levelBeforeDucking = null
        try {
            output.output.close()
        } finally {
            if (current === output) current = null
            mutableSelected.value = null
            (volume as MutableStateFlow).value = null
        }
    }
    // endregion

    // region controller-volume
    override fun setVolumeMultiplier(multiplier: Float) {
        synchronized(lock) {
            this.multiplier = multiplier
            applyMultiplier(current?.selected?.volume)
        }
    }

    /** Pauses are the player's job; this is what the listener is told. */
    fun explain(message: String) {
        mutableNotice.value = message
    }

    // Ducking on a direct stream: Android's mixer is not in the path, so the DAC's own volume does it.
    private fun applyMultiplier(volume: DacVolume?) {
        volume ?: return
        val base = levelBeforeDucking
        if (multiplier >= 1f) {
            if (base != null) volume.setDb(base)
            levelBeforeDucking = null
        } else {
            val restore = base ?: volume.state.value.db
            levelBeforeDucking = restore
            volume.setDb(restore + 20 * log10(multiplier.toDouble().coerceAtLeast(0.001)))
        }
    }
    // endregion
}
