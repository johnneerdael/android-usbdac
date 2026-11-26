package dev.usbdac.sample

import android.app.Activity
import android.content.ClipData
import android.content.ClipboardManager
import android.os.Bundle
import android.view.KeyEvent
import android.view.View
import android.widget.AdapterView
import android.widget.ArrayAdapter
import android.widget.Button
import android.widget.CheckBox
import android.widget.SeekBar
import android.widget.Spinner
import android.widget.TextView
import dev.usbdac.AAudioExclusiveOutput
import dev.usbdac.AltFormat
import dev.usbdac.DacCapabilities
import dev.usbdac.DacConfig
import dev.usbdac.DacDeviceInfo
import dev.usbdac.DacErrorCode
import dev.usbdac.DacException
import dev.usbdac.DacSession
import dev.usbdac.DacState
import dev.usbdac.OutputChange
import dev.usbdac.OutputSelector
import dev.usbdac.SelectedOutput
import dev.usbdac.ConvertingOutput
import dev.usbdac.DacStream
import dev.usbdac.Pacing
import dev.usbdac.PcmOutput
import dev.usbdac.StreamFormat
import dev.usbdac.StreamRequest
import dev.usbdac.StreamState
import dev.usbdac.WriteFormat
import dev.usbdac.UsbDac
import dev.usbdac.controls.DacVolumeKeys
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.cancel
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.launchIn
import kotlinx.coroutines.flow.onEach
import kotlinx.coroutines.launch
import java.util.concurrent.Executors

/**
 * The SDK's worked example: find a USB DAC, get permission, open it, play a sine through it, and
 * drive its hardware volume. Everything blocking (open, negotiate, close) runs on [io]; the UI is
 * updated from flows on the main thread.
 *
 * "Automatic" (the default) lets [OutputSelector] look at the device and choose: it shows what it
 * chose, the guarantee that applies and why the better outputs were passed over. The other entries of the
 * output spinner are the manual override: the Android-managed output or an AAudio exclusive stream (the
 * route of a player with an internal DAC), which need no DAC session, or the direct stream, also behind a
 * converter (a rate the DAC refuses). Pick the output first.
 */
class MainActivity : Activity() {
    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.Main)
    private val io = Executors.newSingleThreadExecutor()

    private var dacs = emptyList<DacDeviceInfo>()
    private var formats = emptyList<Pair<Int, Int>>() // sample rate to bit depth
    private var session: DacSession? = null
    private var stream: DacStream? = null
    private var player: SinePlayer? = null
    private var outputChoice = OutputChoice.AUTOMATIC
    private var automatic: OutputSelector? = null
    private var waitingForPermission: DacDeviceInfo? = null // a DAC the selector would use directly if it were allowed
    private var other: PcmOutput? = null // the output of the automatic, managed or AAudio choice, while one is playing
    private var otherPaused = false
    // Accessed on io, alongside stream and session teardown.
    private var lastFeedbackReport: String? = null
    private var streamJobs = mutableListOf<Job>()
    private val events = ArrayDeque<String>()

    private lateinit var outputSpinner: Spinner
    private lateinit var takeOverBox: CheckBox
    private lateinit var convertBox: CheckBox
    private lateinit var decisionText: TextView
    private lateinit var deviceSpinner: Spinner
    private lateinit var formatSpinner: Spinner
    private lateinit var capabilitiesText: TextView
    private lateinit var volumeText: TextView
    private lateinit var startupText: TextView
    private lateinit var volumeBar: SeekBar
    private lateinit var muteBox: CheckBox
    private lateinit var fixedVolumeBox: CheckBox
    private lateinit var feedbackTraceBox: CheckBox
    private lateinit var statusText: TextView
    private lateinit var eventsText: TextView

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        setContentView(R.layout.activity_main)
        outputSpinner = findViewById(R.id.outputSpinner)
        takeOverBox = findViewById(R.id.takeOverBox)
        convertBox = findViewById(R.id.convertBox)
        decisionText = findViewById(R.id.decisionText)
        deviceSpinner = findViewById(R.id.deviceSpinner)
        formatSpinner = findViewById(R.id.formatSpinner)
        capabilitiesText = findViewById(R.id.capabilitiesText)
        volumeText = findViewById(R.id.volumeText)
        startupText = findViewById(R.id.startupText)
        volumeBar = findViewById(R.id.volumeBar)
        muteBox = findViewById(R.id.muteBox)
        fixedVolumeBox = findViewById(R.id.fixedVolumeBox)
        feedbackTraceBox = findViewById(R.id.feedbackTraceBox)
        statusText = findViewById(R.id.statusText)
        eventsText = findViewById(R.id.eventsText)

        click(R.id.refreshButton) { refreshDevices(); refreshDecision() }
        // In Automatic mode Open asks for the USB permission the selector is waiting for; otherwise it opens a DAC by hand.
        click(R.id.openButton) { if (outputChoice == OutputChoice.AUTOMATIC) askForUsbPermission() else openSelectedDevice() }
        click(R.id.closeButton) { closeSession() }
        click(R.id.playButton) { play() }
        click(R.id.pauseButton) { togglePause() }
        click(R.id.flushButton) {
            stream?.flush()
            other?.flush()
        }
        click(R.id.stopButton) { stopStream() }
        click(R.id.copyFeedbackTraceButton) { copyFeedbackTrace() }

        // Automatic lets the selector choose; the rest are the outputs behind the same PcmOutput
        // interface, chosen by hand. The two direct ones need a DAC session of their own.
        outputSpinner.adapter = ArrayAdapter(this, android.R.layout.simple_spinner_dropdown_item, OutputChoice.entries.map { it.label })
        outputSpinner.onItemSelectedListener = object : AdapterView.OnItemSelectedListener {
            override fun onItemSelected(parent: AdapterView<*>?, view: View?, position: Int, id: Long) {
                outputChoice = OutputChoice.entries[position]
                showFormats()
                refreshDecision()
            }
            override fun onNothingSelected(parent: AdapterView<*>?) = Unit
        }
        formatSpinner.onItemSelectedListener = object : AdapterView.OnItemSelectedListener {
            override fun onItemSelected(parent: AdapterView<*>?, view: View?, position: Int, id: Long) = refreshDecision()
            override fun onNothingSelected(parent: AdapterView<*>?) = Unit
        }
        takeOverBox.setOnCheckedChangeListener { _, _ -> refreshDecision() }
        convertBox.setOnCheckedChangeListener { _, _ -> refreshDecision() }

        // The volume slider writes the DAC's hardware volume. setFraction() returns at once and
        // collapses a burst of moves into the last one, so it is safe to call on every change.
        volumeBar.setOnSeekBarChangeListener(object : SeekBar.OnSeekBarChangeListener {
            override fun onProgressChanged(bar: SeekBar, progress: Int, fromUser: Boolean) {
                if (fromUser) session?.volume?.setFraction(progress / 1000.0)
            }
            override fun onStartTrackingTouch(bar: SeekBar) = Unit
            override fun onStopTrackingTouch(bar: SeekBar) = Unit
        })
        muteBox.setOnCheckedChangeListener { box, checked -> if (box.isPressed) session?.volume?.setMute(checked) }
        // Allowing a fixed volume is part of the session's configuration, so changing it reopens the DAC.
        fixedVolumeBox.setOnCheckedChangeListener { box, _ -> if (box.isPressed) { reopen(); refreshDecision() } }
        feedbackTraceBox.setOnCheckedChangeListener { box, checked ->
            if (box.isPressed) {
                UsbDac.setLogLevel(if (checked) 3 else 2)
                reopen()
                refreshDecision()
            }
        }

        // Plugging and unplugging: refresh the list.
        UsbDac.hotplug(this).onEach { refreshDevices() }.launchIn(scope)
        // A DAC or an output route came or went: look again if nothing plays, else wait for the next track.
        reselectOnChange(scope, selector(), ::onOutputChange)
        refreshDevices()
        scope.launch { statusLoop() }
    }

    override fun onDestroy() {
        scope.cancel()
        player?.stop()
        val open = session
        io.execute { runCatching { open?.close() } }
        io.shutdown()
        super.onDestroy()
    }

    /** The phone's volume keys step the DAC while this screen is in front. */
    override fun dispatchKeyEvent(event: KeyEvent): Boolean =
        DacVolumeKeys.handle(event, session?.volume) || super.dispatchKeyEvent(event)

    // ---- devices and session ----

    private fun refreshDevices() {
        dacs = UsbDac.attachedDacs(this)
        val names = dacs.map { dac ->
            val granted = if (UsbDac.hasPermission(this, dac)) "" else "  (needs permission)"
            "${dac.productName}  ${dac.vendorId.toString(16)}:${dac.productId.toString(16)}$granted"
        }.ifEmpty { listOf("No USB DAC attached") }
        deviceSpinner.adapter = ArrayAdapter(this, android.R.layout.simple_spinner_dropdown_item, names)
    }

    private fun openSelectedDevice() {
        val dac = dacs.getOrNull(deviceSpinner.selectedItemPosition) ?: return
        if (session != null) return log("a session is already open")
        UsbDac.requestPermission(this, dac) { granted ->
            if (!granted) return@requestPermission log("USB permission denied")
            openDevice(dac)
        }
    }

    /** The selector never asks for USB permission by itself: the app does, then selects again. */
    private fun askForUsbPermission() {
        val dac = waitingForPermission ?: return log("no DAC is waiting for USB permission")
        UsbDac.requestPermission(this, dac) { granted ->
            if (granted) refreshDecision() else log("USB permission denied")
        }
    }

    /**
     * Opens [dac] off the main thread. By default a DAC without a usable hardware volume cannot
     * stream (it would play at its own level, with no way to bring it down); the user allows that
     * with the checkbox, and the DAC is opened again.
     */
    private fun openDevice(dac: DacDeviceInfo) {
        val config = DacConfig(
            requireHardwareVolume = !fixedVolumeBox.isChecked,
            captureFeedbackTrace = feedbackTraceBox.isChecked,
        )
        // UsbDac.open() blocks while it claims interfaces and scans descriptors.
        io.execute {
            try {
                val opened = UsbDac.open(this, dac, config)
                runOnUiThread { onOpened(opened) }
            } catch (error: DacException) {
                runOnUiThread { log("open failed: ${error.message}") }
            }
        }
    }

    /** Closes the session and opens the same DAC again with the current setting. */
    private fun reopen() {
        val dac = session?.info ?: return
        closeSession()
        // io runs its tasks in order, so the close has finished before the open starts.
        openDevice(dac)
    }

    private fun onOpened(opened: DacSession) {
        session = opened
        capabilitiesText.text = describe(opened.capabilities)
        formats = pcmFormats(opened.capabilities)
        showFormats()

        opened.events.onEach { log(it.toString()) }.launchIn(scope)
        opened.state.onEach {
            if (it == DacState.LOST && session === opened) {
                log("the DAC was unplugged")
                closeSession()
            }
        }.launchIn(scope)

        val volume = opened.volume
        // What the library did to the DAC's level before any stream could start.
        startupText.text = volume?.startup?.let { startup ->
            "Start-up: %s dB, lowered to %s dB. A remembered level should stay at or below %.1f dB.".format(
                startup.originalDb.joinToString(" / "),
                startup.appliedDb.joinToString(" / "),
                startup.limitDb,
            )
        } ?: "No usable hardware volume: tick \"Allow a fixed volume\" to stream at the DAC's own level."
        volumeBar.isEnabled = volume != null
        muteBox.isEnabled = volume?.hasMute == true
        volume?.state?.onEach { state ->
            volumeText.text = "DAC volume: %.1f dB%s".format(state.db, if (state.muted == true) " (muted)" else "")
            if (!volumeBar.isPressed) volumeBar.progress = (state.fraction * 1000).toInt()
            muteBox.isChecked = state.muted == true
        }?.launchIn(scope)
        log("opened ${opened.info.productName}")
    }

    private fun closeSession() {
        val closing = session ?: return
        if ((other as? SelectedOutput)?.session === closing) {
            stopOther()
            return
        }
        clearDacSession()
        stopStream()
        // close() stops the stream, restores the DAC volume, and gives the DAC back to Android.
        io.execute {
            runCatching { closing.close() }
            retainFeedbackReport(closing)
            runOnUiThread { log("session closed") }
        }
    }

    private fun clearDacSession() {
        session = null
        capabilitiesText.text = ""
        volumeBar.isEnabled = false
        muteBox.isEnabled = false
        volumeText.text = "DAC volume: none"
        startupText.text = ""
    }

    // ---- playback ----

    private fun play() {
        if (outputChoice != OutputChoice.DIRECT_USB) return playOther()
        val open = session ?: return log("open a DAC first")
        if (stream != null) return
        val (rate, bits) = formats.getOrNull(formatSpinner.selectedItemPosition) ?: return
        io.execute {
            try {
                // negotiate() programs the DAC's clock and reads it back; openStream() starts the output thread.
                val format = open.negotiate(StreamRequest.pcm(rate, channels = 2, bitDepth = bits))
                val opened = open.openStream(format)
                val sine = SinePlayer(opened, format.sampleRate, format.channels, format.subslotBytes, format.validBits) {
                    runOnUiThread { log("writer stopped: ${it.message}") }
                }
                runOnUiThread {
                    stream = opened
                    player = sine
                    streamJobs += opened.bitPerfect.onEach { log("bit-perfect: $it") }.launchIn(scope)
                    streamJobs += opened.state.onEach { log("stream: $it") }.launchIn(scope)
                    sine.start()
                }
            } catch (error: DacException) {
                runOnUiThread {
                    // The DAC has no usable hardware volume and the session requires one.
                    if (error.code == DacErrorCode.VOLUME_UNAVAILABLE) {
                        log("no hardware volume: tick \"Allow a fixed volume\" to play at the DAC's own level")
                    } else {
                        log("play failed: ${error.message}")
                    }
                }
            }
        }
    }

    private fun togglePause() {
        other?.let { return toggleOther(it) }
        val current = stream ?: return
        // Keep the writer running: it fills the ring to its target and idles on writableBytes == 0.
        if (current.state.value == StreamState.PAUSED) current.play() else current.pause()
    }

    private fun stopStream() {
        stopOther()
        val closing = stream ?: return
        stream = null
        streamJobs.forEach { it.cancel() }
        streamJobs.clear()
        val writer = player
        player = null
        io.execute {
            writer?.stop()
            runCatching { closing.close() }
        }
    }

    // ---- the managed and AAudio outputs ----

    /** The (rate, bit depth) pairs offered for the selected output: the DAC's own, or a fixed list. */
    private fun currentFormats(): List<Pair<Int, Int>> = if (outputChoice == OutputChoice.DIRECT_USB) formats else OTHER_FORMATS

    private fun showFormats() {
        formatSpinner.adapter = ArrayAdapter(
            this,
            android.R.layout.simple_spinner_dropdown_item,
            currentFormats().map { (rate, bits) -> "$rate Hz / $bits-bit" },
        )
    }

    private fun playOther() {
        if (other != null) return
        val (rate, bits) = currentFormats().getOrNull(formatSpinner.selectedItemPosition) ?: return
        val kind = outputChoice
        val open = session
        if (kind == OutputChoice.DIRECT_USB_CONVERTED && open == null) return log("open a DAC first")
        val chooser = selector()
        io.execute {
            try {
                // All of them are PcmOutputs: the same writer and the same controls drive either.
                val output = when (kind) {
                    OutputChoice.AUTOMATIC -> chooser.open(StreamRequest.pcm(rate, channels = 2, bitDepth = bits))
                    OutputChoice.AAUDIO_EXCLUSIVE -> openAAudio(this, rate, bits)
                    OutputChoice.DIRECT_USB_CONVERTED -> openConverting(requireNotNull(open), StreamRequest.pcm(rate, channels = 2, bitDepth = bits))
                    else -> openManaged(this, rate, bits)
                }
                // What to write is the layout of the output: the DAC's wire layout for a direct stream.
                val layout = (output as? SelectedOutput)?.format ?: WriteFormat(rate, 2, bits / 8, bits, rate)
                val sine = SinePlayer(output, layout.sampleRate, layout.channels, layout.subslotBytes, layout.validBits) {
                    runOnUiThread { log("writer stopped: ${it.message}") }
                }
                runOnUiThread {
                    other = output
                    otherPaused = false
                    player = sine
                    streamJobs += output.bitPerfect.onEach { log("bit-perfect: $it") }.launchIn(scope)
                    if (output is AAudioExclusiveOutput) {
                        streamJobs += watchRoute(scope, output) { detail -> log("route lost: $detail"); stopOther() }
                    }
                    if (output is SelectedOutput) {
                        decisionText.text = describeDecision(output.decision)
                        // A direct output brings its DAC session: the volume controls work as for a session opened by hand.
                        output.session?.let { onOpened(it) }
                    }
                    sine.start()
                }
            } catch (error: DacException) {
                runOnUiThread { log("play failed: ${error.message}") }
            }
        }
    }

    /** The selector for what the two boxes say; the same one is kept while they do not change. */
    private fun selector(): OutputSelector {
        val base = automaticPolicy(takeOverBox.isChecked, convertBox.isChecked, fixedVolumeBox.isChecked)
        val policy = base.copy(dacConfig = base.dacConfig.copy(captureFeedbackTrace = feedbackTraceBox.isChecked))
        return automatic?.takeIf { it.policy == policy } ?: OutputSelector(this, policy).also { automatic = it }
    }

    /**
     * In Automatic mode, shows what the selector would choose now and why. `select` opens the DAC or an
     * AAudio stream to look and closes it again, so it runs on [io] and only while nothing plays.
     */
    private fun refreshDecision() {
        if (outputChoice != OutputChoice.AUTOMATIC) {
            decisionText.text = ""
            return
        }
        if (other != null || stream != null) return
        val (rate, bits) = currentFormats().getOrNull(formatSpinner.selectedItemPosition) ?: return
        val chooser = selector()
        io.execute {
            val decision = try {
                chooser.select(StreamRequest.pcm(rate, channels = 2, bitDepth = bits))
            } catch (error: DacException) {
                if (error.code == DacErrorCode.INVALID_STATE) return@execute // an output of this selector is opening
                runOnUiThread { if (other == null) decisionText.text = "No output can play this: ${error.detail}" }
                return@execute
            }
            runOnUiThread {
                if (other != null) return@runOnUiThread
                waitingForPermission = decision.needsUsbPermission
                decisionText.text = describeDecision(decision) +
                    if (decision.needsUsbPermission != null) "\nTap Open to ask for the USB permission." else ""
            }
        }
    }

    private fun onOutputChange(change: OutputChange) {
        log("output change: ${change::class.simpleName}")
        // The selector never switches a playing stream: look again when idle, else at the next track boundary.
        if (other == null && stream == null) refreshDecision() else log("re-select at the next track boundary")
    }

    private fun toggleOther(output: PcmOutput) {
        otherPaused = !otherPaused
        if (otherPaused) output.pause() else output.play()
    }

    private fun stopOther() {
        val closing = other ?: return
        val closingSession = (closing as? SelectedOutput)?.session
        other = null
        streamJobs.forEach { it.cancel() }
        streamJobs.clear()
        val writer = player
        player = null
        io.execute {
            writer?.stop()
            runCatching { closing.close() }
            closingSession?.let(::retainFeedbackReport)
        }
        // The output closed its DAC session too; clear what the volume controls were showing.
        if (closingSession != null && session === closingSession) clearDacSession()
    }

    private fun describeOther(output: PcmOutput): String = buildString {
        appendLine(output.bitPerfect.value.toString())
        appendLine("played ${output.playedFrames} frames   latency ${output.latencyFrames} frames   writable ${output.writableBytes} bytes")
        if (output is AAudioExclusiveOutput) {
            appendLine("state ${output.state.value}   xruns ${output.xrunCount}   underruns ${output.underrunCount}")
            append(describeGrant(output.grant))
        }
        if (output is ConvertingOutput) append(output.plan.description)
        if (output is SelectedOutput) {
            appendLine("writing ${output.format}")
            output.stream?.let { stream ->
                val stats = runCatching { stream.stats() }.getOrNull()
                if (stats != null) append("buffer ${stats.bufferMs}/${stats.bufferTargetMs} ms   underruns ${stats.underruns}   lost ${stats.lostFrames}")
            }
        }
    }

    // ---- status ----

    private suspend fun statusLoop() {
        while (true) {
            delay(250)
            other?.let { statusText.text = describeOther(it); continue }
            val current = stream
            statusText.text = if (current == null) "" else runCatching {
                val rate = current.format.counterRateHz
                val stats = current.stats()
                val feedback = when {
                    stats.feedbackStale -> "stale"
                    stats.feedbackActive -> "${stats.feedbackReports} reports"
                    current.format.pacing is Pacing.ExplicitFeedback ||
                        current.format.pacing is Pacing.ImplicitFeedback -> "waiting for lock"
                    else -> "off"
                }
                "state ${current.state.value}   ${current.bitPerfect.value}\n" +
                    "played ${current.playedFrames} frames (%.1f s)\n".format(current.playedFrames / rate.toDouble()) +
                    "latency ${current.latencyFrames} frames (%.0f ms)   writable ${current.writableBytes} bytes\n".format(
                        current.latencyFrames * 1000.0 / rate,
                    ) +
                    "buffer ${stats.bufferMs}/${stats.bufferTargetMs} ms   underruns ${stats.underruns}   lost ${stats.lostFrames}\n" +
                    "effective %.1f Hz   feedback $feedback\n".format(
                        stats.effectiveRateHz,
                    ) +
                    describeFormat(current.format)
            }.getOrDefault("")
        }
    }

    private fun retainFeedbackReport(closedSession: DacSession) {
        runCatching { closedSession.feedbackTraceJson() }
            .getOrNull()
            ?.let { lastFeedbackReport = it }
    }

    private fun copyFeedbackTrace() {
        val active = session ?: (other as? SelectedOutput)?.session
        io.execute {
            val result = runCatching { active?.feedbackTraceJson() ?: lastFeedbackReport }
            runOnUiThread {
                result.onSuccess { report ->
                    if (report == null) {
                        log("enable USB feedback capture and play once before exporting")
                    } else {
                        getSystemService(ClipboardManager::class.java)
                            .setPrimaryClip(ClipData.newPlainText("USB feedback report", report))
                        log("feedback report copied")
                    }
                }.onFailure { log("feedback export failed: ${it.message}") }
            }
        }
    }

    private fun log(line: String) {
        events.addFirst(line)
        while (events.size > 12) events.removeLast()
        eventsText.text = events.joinToString("\n")
    }

    // ---- small helpers ----

    private fun click(id: Int, action: () -> Unit) = findViewById<Button>(id).setOnClickListener { _: View -> action() }

    /** The (rate, bit depth) pairs the DAC's stereo PCM alternate settings offer. */
    private fun pcmFormats(capabilities: DacCapabilities): List<Pair<Int, Int>> =
        capabilities.altSettings
            .filter { it.format == AltFormat.PCM && it.channels == 2 }
            .flatMap { alt -> alt.sampleRates.map { rate -> rate to alt.validBits } }
            .distinct()
            .sortedWith(compareBy({ it.second }, { it.first }))

    /**
     * How the stream is paced and which clock the rate was set on: what to read first when a DAC
     * plays at the wrong pitch, or not at all at one rate.
     */
    private fun describeFormat(format: StreamFormat): String = buildString {
        appendLine("pacing: ${format.pacing.description}")
        val clock = format.clock
        if (clock == null) {
            append("clock: none (the DAC describes no clock entities)")
        } else {
            appendLine("clock: source ${clock.sourceId} on AudioControl interface ${clock.controlInterface}")
            append("  ${clock.description}")
        }
    }

    private fun describe(c: DacCapabilities): String = buildString {
        appendLine("UAC ${c.uacVersion}, ${c.linkSpeed} speed, explicit feedback: ${c.hasExplicitFeedback}")
        appendLine("pacing: ${c.altSettings.mapNotNull { it.pacing }.distinct().joinToString("; ") { it.description }.ifEmpty { "none" }}")
        appendLine("PCM ${c.pcmSampleRates} Hz, ${c.pcmBitDepths}-bit")
        appendLine("DoP: ${c.supportsDop} (max DSD ${c.dopMaxDsdBitRate})   native DSD: ${c.supportsNativeDsd}")
        val range = c.volume
        append(if (range == null) "no hardware volume" else "hardware volume ${range.minDb}..${range.maxDb} dB, mute: ${c.hasMute}")
    }
}

/** Where the sample sends its tone: the selector's own choice, or one of the outputs by hand. The manual ones are all behind [PcmOutput]. */
private enum class OutputChoice(val label: String) {
    AUTOMATIC("Automatic (the library chooses)"),
    DIRECT_USB("Direct USB (needs a DAC session)"),
    ANDROID_MANAGED("Android-managed (AudioTrack)"),
    AAUDIO_EXCLUSIVE("AAudio exclusive (internal DAC)"),
    DIRECT_USB_CONVERTED("Direct USB with conversion (not bit-perfect)"),
}

/** The formats offered to the outputs that do not read them from a DAC (the source formats). 24-bit and 32-bit need Android 12 for the platform outputs. */
private val OTHER_FORMATS = listOf(44_100 to 16, 48_000 to 16, 96_000 to 24, 192_000 to 24, 96_000 to 32)
