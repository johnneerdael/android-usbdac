package dev.usbdac.sample

import android.app.Activity
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
import dev.usbdac.DacDeviceInfo
import dev.usbdac.DacException
import dev.usbdac.DacSession
import dev.usbdac.DacState
import dev.usbdac.ConvertingOutput
import dev.usbdac.DacStream
import dev.usbdac.PcmOutput
import dev.usbdac.StreamRequest
import dev.usbdac.StreamState
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
 * The sine can also go through the Android-managed output or an AAudio exclusive stream (the route
 * of a player with an internal DAC), which need no DAC session, or through the direct stream behind a
 * converter (a rate the DAC refuses): pick the output first.
 */
class MainActivity : Activity() {
    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.Main)
    private val io = Executors.newSingleThreadExecutor()

    private var dacs = emptyList<DacDeviceInfo>()
    private var formats = emptyList<Pair<Int, Int>>() // sample rate to bit depth
    private var session: DacSession? = null
    private var stream: DacStream? = null
    private var player: SinePlayer? = null
    private var outputKind = OutputKind.DIRECT_USB
    private var other: PcmOutput? = null // the managed or AAudio output, while one is playing
    private var otherPaused = false
    private var streamJobs = mutableListOf<Job>()
    private val events = ArrayDeque<String>()

    private lateinit var outputSpinner: Spinner
    private lateinit var deviceSpinner: Spinner
    private lateinit var formatSpinner: Spinner
    private lateinit var capabilitiesText: TextView
    private lateinit var volumeText: TextView
    private lateinit var volumeBar: SeekBar
    private lateinit var muteBox: CheckBox
    private lateinit var statusText: TextView
    private lateinit var eventsText: TextView

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        setContentView(R.layout.activity_main)
        outputSpinner = findViewById(R.id.outputSpinner)
        deviceSpinner = findViewById(R.id.deviceSpinner)
        formatSpinner = findViewById(R.id.formatSpinner)
        capabilitiesText = findViewById(R.id.capabilitiesText)
        volumeText = findViewById(R.id.volumeText)
        volumeBar = findViewById(R.id.volumeBar)
        muteBox = findViewById(R.id.muteBox)
        statusText = findViewById(R.id.statusText)
        eventsText = findViewById(R.id.eventsText)

        click(R.id.refreshButton) { refreshDevices() }
        click(R.id.openButton) { openSelectedDevice() }
        click(R.id.closeButton) { closeSession() }
        click(R.id.playButton) { play() }
        click(R.id.pauseButton) { togglePause() }
        click(R.id.flushButton) {
            stream?.flush()
            other?.flush()
        }
        click(R.id.stopButton) { stopStream() }

        // The outputs behind the same PcmOutput interface. The two direct ones need a DAC session.
        outputSpinner.adapter = ArrayAdapter(this, android.R.layout.simple_spinner_dropdown_item, OutputKind.entries.map { it.label })
        outputSpinner.onItemSelectedListener = object : AdapterView.OnItemSelectedListener {
            override fun onItemSelected(parent: AdapterView<*>?, view: View?, position: Int, id: Long) {
                outputKind = OutputKind.entries[position]
                showFormats()
            }
            override fun onNothingSelected(parent: AdapterView<*>?) = Unit
        }

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

        // Plugging and unplugging: refresh the list.
        UsbDac.hotplug(this).onEach { refreshDevices() }.launchIn(scope)
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
            // UsbDac.open() blocks while it claims interfaces and scans descriptors.
            io.execute {
                try {
                    val opened = UsbDac.open(this, dac)
                    runOnUiThread { onOpened(opened) }
                } catch (error: DacException) {
                    runOnUiThread { log("open failed: ${error.message}") }
                }
            }
        }
    }

    private fun onOpened(opened: DacSession) {
        session = opened
        capabilitiesText.text = describe(opened.capabilities)
        formats = pcmFormats(opened.capabilities)
        showFormats()

        opened.events.onEach { log(it.toString()) }.launchIn(scope)
        opened.state.onEach { if (it == DacState.LOST) { log("the DAC was unplugged"); closeSession() } }.launchIn(scope)

        val volume = opened.volume
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
        stopStream()
        session = null
        capabilitiesText.text = ""
        volumeBar.isEnabled = false
        muteBox.isEnabled = false
        volumeText.text = "DAC volume: none"
        // close() stops the stream, restores the DAC volume, and gives the DAC back to Android.
        io.execute { runCatching { closing.close() }; runOnUiThread { log("session closed") } }
    }

    // ---- playback ----

    private fun play() {
        if (outputKind != OutputKind.DIRECT_USB) return playOther()
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
                runOnUiThread { log("play failed: ${error.message}") }
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
    private fun currentFormats(): List<Pair<Int, Int>> = if (outputKind == OutputKind.DIRECT_USB) formats else OTHER_FORMATS

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
        val kind = outputKind
        val open = session
        if (kind == OutputKind.DIRECT_USB_CONVERTED && open == null) return log("open a DAC first")
        io.execute {
            try {
                // All of them are PcmOutputs: the same writer and the same controls drive either.
                val output = when (kind) {
                    OutputKind.AAUDIO_EXCLUSIVE -> openAAudio(this, rate, bits)
                    OutputKind.DIRECT_USB_CONVERTED -> openConverting(requireNotNull(open), StreamRequest.pcm(rate, channels = 2, bitDepth = bits))
                    else -> openManaged(this, rate, bits)
                }
                val sine = SinePlayer(output, rate, 2, bits / 8, bits) { runOnUiThread { log("writer stopped: ${it.message}") } }
                runOnUiThread {
                    other = output
                    otherPaused = false
                    player = sine
                    streamJobs += output.bitPerfect.onEach { log("bit-perfect: $it") }.launchIn(scope)
                    if (output is AAudioExclusiveOutput) {
                        streamJobs += watchRoute(scope, output) { detail -> log("route lost: $detail"); stopOther() }
                    }
                    sine.start()
                }
            } catch (error: DacException) {
                runOnUiThread { log("play failed: ${error.message}") }
            }
        }
    }

    private fun toggleOther(output: PcmOutput) {
        otherPaused = !otherPaused
        if (otherPaused) output.pause() else output.play()
    }

    private fun stopOther() {
        val closing = other ?: return
        other = null
        streamJobs.forEach { it.cancel() }
        streamJobs.clear()
        val writer = player
        player = null
        io.execute {
            writer?.stop()
            runCatching { closing.close() }
        }
    }

    private fun describeOther(output: PcmOutput): String = buildString {
        appendLine(output.bitPerfect.value.toString())
        appendLine("played ${output.playedFrames} frames   latency ${output.latencyFrames} frames   writable ${output.writableBytes} bytes")
        if (output is AAudioExclusiveOutput) {
            appendLine("state ${output.state.value}   xruns ${output.xrunCount}   underruns ${output.underrunCount}")
            append(describeGrant(output.grant))
        }
        if (output is ConvertingOutput) append(output.plan.description)
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
                "state ${current.state.value}   ${current.bitPerfect.value}\n" +
                    "played ${current.playedFrames} frames (%.1f s)\n".format(current.playedFrames / rate.toDouble()) +
                    "latency ${current.latencyFrames} frames (%.0f ms)   writable ${current.writableBytes} bytes\n".format(
                        current.latencyFrames * 1000.0 / rate,
                    ) +
                    "buffer ${stats.bufferMs}/${stats.bufferTargetMs} ms   underruns ${stats.underruns}   lost ${stats.lostFrames}\n" +
                    "effective %.1f Hz   feedback ${if (stats.feedbackActive) "${stats.feedbackReports} reports" else "off"}".format(
                        stats.effectiveRateHz,
                    )
            }.getOrDefault("")
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

    private fun describe(c: DacCapabilities): String = buildString {
        appendLine("UAC ${c.uacVersion}, ${c.linkSpeed} speed, explicit feedback: ${c.hasExplicitFeedback}")
        appendLine("PCM ${c.pcmSampleRates} Hz, ${c.pcmBitDepths}-bit")
        appendLine("DoP: ${c.supportsDop} (max DSD ${c.dopMaxDsdBitRate})   native DSD: ${c.supportsNativeDsd}")
        val range = c.volume
        append(if (range == null) "no hardware volume" else "hardware volume ${range.minDb}..${range.maxDb} dB, mute: ${c.hasMute}")
    }
}

/** Where the sample sends its tone: the three outputs the SDK has, all behind [PcmOutput], and the direct one behind a converter. */
private enum class OutputKind(val label: String) {
    DIRECT_USB("Direct USB (needs a DAC session)"),
    ANDROID_MANAGED("Android-managed (AudioTrack)"),
    AAUDIO_EXCLUSIVE("AAudio exclusive (internal DAC)"),
    DIRECT_USB_CONVERTED("Direct USB with conversion (not bit-perfect)"),
}

/** The formats offered to the outputs that do not read them from a DAC (the source formats). 24-bit and 32-bit need Android 12 for the platform outputs. */
private val OTHER_FORMATS = listOf(44_100 to 16, 48_000 to 16, 96_000 to 24, 192_000 to 24, 96_000 to 32)
