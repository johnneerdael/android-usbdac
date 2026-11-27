# Integrate usbdac into a Media3 player, step by step

This guide takes a streaming music player built on Media3 (ExoPlayer and a `MediaSessionService`) from "plays through Android's audio" to "plays straight to a USB DAC when one is plugged in, and plays as before when none is". Every step says what to do, why, shows the code and says how to check that it worked.

The code is compiled. It lives in `android/sample/src/main/kotlin/dev/usbdac/sample/player`, and the documentation build checks that every Kotlin and XML block on this page is an excerpt of the sample app. The class names (`PlaybackService`, `PlayerModule`) are generic. Use your own.

**Status.** The code compiles against usbdac 0.1.5 and Media3 1.11.1, and the audio sink has 12 JVM tests that run without a DAC. It has not been run with a DAC in front of it yet. The usbdac stream underneath has played on hardware ([hardware status](evidence.md#hardware-status)); this Media3 layer has not. Run the checks of [step 16](#16-test-it) on your own devices before you ship.

## Words used on this page

- **DAC.** The box that turns digital audio into sound. Here it is a USB one: a dongle, a desktop DAC or a headset.
- **Direct USB.** usbdac takes the DAC away from Android and sends it the audio itself, so Android's mixer, resampler and volume are out of the path.
- **Bit-perfect.** The samples reach the DAC exactly as the decoder produced them.
- **Audio sink.** Media3's name for the last stage of the player. It takes decoded PCM and plays it. `DefaultAudioSink` plays through an `AudioTrack`; this guide writes one that plays through usbdac.
- **Ducking.** Android asks an app to lower its volume for a moment while another app speaks, such as a navigation prompt.
- **Gapless.** Two tracks play one after the other with no silence between them.

## What you build

| Class | Job | Step |
|---|---|---|
| `DirectOutputSettings` | The choices the listener owns | 3 |
| `DirectOutputController` | Owns the `OutputSelector` and the output in use | 4 |
| `UsbDacAudioSink` | Media3's `AudioSink`, writing to the output | 5 |
| `PlayerModule`, `PlaybackService` | Builds the player with the sink; owns it in a media service | 6 |
| `DacVolumePlayer` | Makes the system volume panel and the hardware keys drive the DAC | 10 |

The data flows in one direction:

```text
decoder  ->  UsbDacAudioSink  ->  DirectOutputController  ->  OutputSelector  ->  one output:
                                                              direct USB, Android's bit-perfect mixer,
                                                              exclusive AAudio or the standard output
```

The selector is the reason the sink works on every phone. With no DAC attached, or with Bluetooth connected, it hands the sink Android's standard output, and the app plays as it did before.

The SDK has no Media3 module in 0.1.5, so the sink is yours to carry. It is about 250 lines, and most of them are Media3 bookkeeping.

## 1. Add the library

**Do.** Add the GitHub Packages repository and the dependency. The registry needs a GitHub user name and a token with `read:packages`, even for a public package. Put `gpr.user` and `gpr.token` in `~/.gradle/gradle.properties`, or set `GITHUB_ACTOR` and `GITHUB_TOKEN` in the build environment.

```gradle
dependencyResolutionManagement {
    repositories {
        google()
        mavenCentral()
        maven {
            url = uri("https://maven.pkg.github.com/johnneerdael/android-usbdac")
            credentials {
                username = providers.gradleProperty("gpr.user").orNull ?: System.getenv("GITHUB_ACTOR")
                password = providers.gradleProperty("gpr.token").orNull ?: System.getenv("GITHUB_TOKEN")
            }
        }
    }
}
```

```gradle
dependencies {
    implementation("dev.usbdac:usbdac:0.1.5")
    implementation("androidx.media3:media3-exoplayer:1.11.1")
    implementation("androidx.media3:media3-session:1.11.1")
}
```

**Why.** The AAR carries the native driver for all four Android ABIs, so the app needs no Rust and no NDK. The library's only dependency is `kotlinx-coroutines-core`. The Media3 lines are the ones a player already has. `AudioSink` is marked unstable in Media3 (`@UnstableApi`), so expect to touch the sink when you upgrade Media3. Pin the version.

**Check.** Call this once, for example in a debug menu:

```kotlin
/** The native library's version, which also proves that libusbdac.so loaded for this device's ABI. */
fun libraryVersion(): String = UsbDac.nativeVersion

/** Debug builds and the support screen: the library's log goes to logcat under the tag `usbdac`. */
fun setSupportLogging(enabled: Boolean) = UsbDac.setLogLevel(if (enabled) 4 else 0)
```

It returns `0.1.5`. The call loads `libusbdac.so`, so an `UnsatisfiedLinkError` here means the AAR has no library for the device's ABI (look at your `abiFilters`).

## 2. Declare what Android needs

**Do.** Three things go in the manifest of the app module.

First, offer the app when a DAC is plugged in. An intent filter on the main activity plus a device filter makes Android ask "open with this app?", and if the listener chooses "always" it remembers the USB permission for that DAC and app.

```xml
<intent-filter>
    <action android:name="android.hardware.usb.action.USB_DEVICE_ATTACHED" />
</intent-filter>
<meta-data
    android:name="android.hardware.usb.action.USB_DEVICE_ATTACHED"
    android:resource="@xml/device_filter" />
```

```xml
<resources>
    <!-- USB class 1 is Audio. -->
    <usb-device class="1" />
</resources>
```

Second, the media service needs permission to run in the foreground and, from Android 13, permission to show its notification:

```xml
<uses-permission android:name="android.permission.FOREGROUND_SERVICE" />
<uses-permission android:name="android.permission.FOREGROUND_SERVICE_MEDIA_PLAYBACK" />
<uses-permission android:name="android.permission.POST_NOTIFICATIONS" />
```

Third, the service itself, with the `mediaPlayback` foreground service type that Android 14 requires:

```xml
<service
    android:name=".player.PlaybackService"
    android:exported="true"
    android:foregroundServiceType="mediaPlayback">
    <intent-filter>
        <action android:name="androidx.media3.session.MediaSessionService" />
    </intent-filter>
</service>
```

You do not need to declare USB host support or `MODIFY_AUDIO_SETTINGS`. The library's manifest already merges `android.hardware.usb.host` with `required="false"` and the audio-settings permission. The sample repeats the first one, which is harmless:

```xml
<uses-feature
    android:name="android.hardware.usb.host"
    android:required="false" />
```

**Why.** `required="false"` keeps the app installable on phones without USB host mode, where the selector falls back to Android's output. The library needs no `RECORD_AUDIO` and no foreground service of its own. The service is the one your player already has, because the music must go on when the activity goes away. `POST_NOTIFICATIONS` is a runtime permission: ask for it in your UI as you already do for the media notification.

**Check.** Build, then open `app/build/outputs/logs/manifest-merger-debug-report.txt`. `android.hardware.usb.host` appears with `required=false` and the service has `foregroundServiceType`. Then close the app and plug in a DAC. Android should offer the app.

## 3. Decide what the listener controls

**Do.** Keep three settings in the app's settings screen, in the listener's words. They are the choices the library cannot make for the listener.

| Setting | What it changes | Suggested wording | Default |
|---|---|---|---|
| Take over the DAC (`allowDirectUsb`) | While music plays, other apps and system sounds cannot use the DAC | "Use the DAC directly. While music plays, other apps and system sounds cannot use it." | On |
| Convert what the DAC cannot play (`allowConversion`) | A file the DAC cannot run as it is gets resampled or changed in depth. The result is not bit-perfect | "If the DAC cannot play a file as it is, convert it (not bit-perfect)." | Off |
| Allow a DAC with no volume control (`requireHardwareVolume = false`) | The first sound plays at the DAC's own level, possibly full scale | "Play on a DAC whose volume cannot be lowered, at its own level." | Off, with a warning the first time |

A fourth switch, for support only, keeps a USB feedback trace of the next attempt ([step 15](#15-diagnostics-and-support-reports)). Show it under a support screen and leave it off.

```kotlin
/**
 * The choices that belong to the listener, as the settings screen stores them. Persist them wherever
 * the app keeps its settings (DataStore in many players); the library stores nothing.
 */
data class DirectOutputSettings(
    /** "Use the DAC directly." Off keeps the DAC available to other apps. */
    val takeOverDac: Boolean = true,
    /** "If the DAC cannot play a file as it is, convert it (not bit-perfect)." */
    val allowConversion: Boolean = false,
    /** "Play on a DAC whose volume cannot be lowered, at its own level." */
    val allowFixedVolume: Boolean = false,
    /** A support setting, off by default: keep a USB feedback trace of the next attempt. */
    val captureSupportTrace: Boolean = false,
) {
    fun toPolicy(): OutputPolicy =
        OutputPolicy(
            allowDirectUsb = takeOverDac,
            allowConversion = allowConversion,
            dacConfig = DacConfig(
                requireHardwareVolume = !allowFixedVolume,
                captureFeedbackTrace = captureSupportTrace,
            ),
        )
}
```

**Why.** Taking over the DAC costs the listener something. A navigation prompt that has to come through the same DAC is lost while direct USB is on. Conversion changes the audio, and a DAC without a volume control plays its first sound loud. These are the three decisions the library leaves to the app, and the app leaves to the listener.

Do not offer the other options of [controls](controls.md) to everybody. In particular, leave out `OutputPolicy.force` (it disables the fallback that keeps playback working), the quirk list, the buffer and bus queue sizes, `requireVerifiedRate`, `allowUnsynchronizedAsync`, `disableFeedback` and `realtimePriority`. They exist for bug reports and for DACs that need a workaround, and a wrong value makes the stream unverified or silent.

**Check.** After the output opens ([step 4](#4-own-the-output-in-one-place)), the decision says `DIRECT_USB` only while "take over the DAC" is on. With it off and Android 14 or later, a DAC gets Android's bit-perfect mixer. Below Android 14 it gets the standard output.

## 4. Own the output in one place

**Do.** Create one object that owns the `OutputSelector` and the output in use. Everything else asks it. In a Hilt or Koin app this is a singleton.

`OutputSelector.open(request)` looks at the attached DAC, the USB permission, where Android sends sound and what the platform grants. It returns a `SelectedOutput`: a `PcmOutput` you write to, plus a `decision` that says what was chosen and why. Four things to know about it:

- `open` blocks for 100 ms or more while it opens the DAC, programs its clock and reads the result. It must not run on the main thread.
- A selector holds one output at a time. While the output it returned is open, `open` throws `INVALID_STATE`. Close the old output before you open the next.
- The selector never moves a stream that is playing. You look again at a track boundary.
- It never shows a permission dialog.

First, the small interface the sink needs. The sink is written against this and `PcmOutput`, which is why it can be tested with a fake output and no DAC ([step 16](#16-test-it)):

```kotlin
/** An output the sink writes to: what the library returned, and what the sink needs to know about it. */
class OpenedOutput(
    val output: PcmOutput,
    /** The layout to write, and the unit of the position counters. */
    val format: WriteFormat,
    /** What the output was opened for. */
    val request: StreamRequest,
    /** The library's own handle, for the status display; `null` for a fake in a test. */
    val selected: SelectedOutput? = null,
) {
    /** Set when the DAC is lost or the stream failed. A failed output is closed and never reused. */
    @Volatile
    var failure: DacException? = null
}

/**
 * What the audio sink needs from the app. The sink is written against this and [PcmOutput], so a test
 * can give it a fake output and no DAC.
 */
interface OutputSource {
    /** Opens the best output for [request]. Blocks for 100 ms or more: the sink calls it off the main thread. */
    fun open(request: StreamRequest): OpenedOutput

    /** Closes [output] and everything opened for it. Blocks. */
    fun close(output: OpenedOutput)

    /** True once if the settings or the devices changed since the last time the output was opened. */
    fun takeLookAgain(): Boolean

    /** Android's ducking and the app's own player volume, as a multiplier from 0 to 1. */
    fun setVolumeMultiplier(multiplier: Float)
}
```

Then the controller. This part holds the selector, the settings and the flags the rest of the app reads:

```kotlin
/**
 * Owns the selector and the output in use for the whole process, and tells the rest of the app what
 * happened. The service creates it once; the settings screen reads [selected] and [notice].
 */
class DirectOutputController(
    private val context: Context,
    settings: DirectOutputSettings,
    private val scope: CoroutineScope,
) : OutputSource {
    private val lock = Any()
    private val selectors = MutableStateFlow(OutputSelector(context, settings.toPolicy()))
    private var current: OpenedOutput? = null
    private var watching: Job? = null
    private var multiplier = 1f
    private var levelBeforeDucking: Double? = null

    @Volatile
    private var lookAgain = false

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
    fun apply(settings: DirectOutputSettings) {
        selectors.value = OutputSelector(context, settings.toPolicy())
        lookAgain = true
    }

    /** Call when [changes] fires. */
    fun onChanged() {
        lookAgain = true
    }

    override fun takeLookAgain(): Boolean = lookAgain.also { lookAgain = false }
```

And the two calls that open and close:

```kotlin
override fun open(request: StreamRequest): OpenedOutput = synchronized(lock) {
    check(current == null) { "close the output in use before opening another" }
    lookAgain = false
    val selected = selectors.value.open(request)
    val opened = OpenedOutput(selected, selected.format, request, selected)
    current = opened
    mutableSelected.value = selected
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

override fun close(output: OpenedOutput) = synchronized(lock) {
    watching?.cancel()
    watching = null
    levelBeforeDucking = null
    try {
        output.output.close()
    } finally {
        if (current === output) current = null
        mutableSelected.value = null
    }
}
```

`open` also starts watching the session's events. When the DAC is lost or the stream fails it marks the output as failed. The sink reads that mark ([step 5](#5-write-the-audio-sink)), and `events` carries the same news to the service ([step 12](#12-unplug-and-replug)).

**What to do with each result.** The decision is made before the first sample, and the output's own `bitPerfect` flow says what is true while playing.

| You get | Meaning | What the app does |
|---|---|---|
| `DIRECT_USB`, guarantee `VERIFIED_BY_LIBRARY` | The library drives the DAC and checks the stream | Show the bit-perfect indicator from `output.bitPerfect` ([step 13](#13-show-the-signal-path-and-the-bit-perfect-indicator)) |
| `DIRECT_USB`, guarantee `NONE` | A conversion that changes the audio, or a DAC paced without feedback | Play. Show the reason from `bitPerfect`; never "bit-perfect" |
| `ANDROID_BIT_PERFECT` | Android accepted a bit-perfect request for this DAC and format | Play. Say "requested from Android": it cannot be verified |
| `AAUDIO_EXCLUSIVE` | The platform gave an exclusive stream, such as a player with an internal DAC | Play. Not verifiable from an app |
| `ANDROID_STANDARD` | Android's ordinary output | Play. The system volume applies |
| `decision.needsUsbPermission` is set | A DAC is attached but the app lacks permission | Ask ([step 9](#9-usb-permission-and-hot-plug)); the output changes at the next track |
| `decision.alternativesRejected` | The better outputs that were passed over, with a sentence each | Show it in a details view. It is the first thing to read when direct USB is not chosen |
| `DacException` `UNSUPPORTED` | No output can carry the request | The sink reports a failed initialisation; the player shows its normal playback error |
| `DacException` `INVALID_STATE` | You opened a second output while one was open | A bug in your own sequence: close first |

**Why.** The library holds the DAC and the USB connection for as long as the output is open, so one owner avoids the common failure of two parts of the app each opening it. The service, not an activity, should hold this object, because an activity goes away when the listener swipes the app from the recents list.

**Check.** Log `describeDecision(output.decision)` from [automatic output](automatic-output.md#reading-the-decision) the first time you play. With a DAC attached and permission held it says `DIRECT_USB (VERIFIED_BY_LIBRARY)`. With nothing attached it says `ANDROID_STANDARD` and lists why the others were rejected.

## 5. Write the audio sink

**Do.** Implement `AudioSink`. ExoPlayer's audio renderer decodes with `MediaCodec` and hands each decoded buffer to the sink. `DefaultAudioSink` writes it to an `AudioTrack`. Yours writes it to the output of step 4.

The contract you have to meet is Media3's, so the sink follows `DefaultAudioSink` closely where it can: the renderer calls `configure` when the decoder's output format changes, `handleBuffer` with each buffer until it is fully consumed, and `flush` on a seek. Five parts follow.

### 5.1 Say which formats it takes

```kotlin
override fun supportsFormat(format: Format): Boolean = getFormatSupport(format) != AudioSink.SINK_FORMAT_UNSUPPORTED

override fun getFormatSupport(format: Format): Int =
    if (format.sampleMimeType == MimeTypes.AUDIO_RAW && bitDepthOf(format.pcmEncoding) != null && format.channelCount in 1..2) {
        AudioSink.SINK_FORMAT_SUPPORTED_DIRECTLY
    } else {
        AudioSink.SINK_FORMAT_UNSUPPORTED // float, 8-bit, more than two channels, compressed audio
    }

private fun bitDepthOf(pcmEncoding: Int): Int? = when (pcmEncoding) {
    C.ENCODING_PCM_16BIT -> 16
    C.ENCODING_PCM_24BIT -> 24
    C.ENCODING_PCM_32BIT -> 32
    else -> null
}
```

The sink takes integer PCM at 16, 24 and 32 bits in one or two channels. Declining float matters. `MediaCodecAudioRenderer` asks the sink whether it supports float PCM, and if the answer is yes it makes the decoder produce float. Answering no keeps the decoder on integers, which is what a direct stream wants. The price is that a 24-bit source may come out of a platform decoder as 16-bit, because that decoder is no longer asked for float. See [what this does not cover](#what-this-does-not-cover).

### 5.2 Remember the request, open later

```kotlin
override fun configure(audioSinkConfig: AudioSink.AudioSinkConfig) {
    val format = audioSinkConfig.format
    val bits = bitDepthOf(format.pcmEncoding)
    if (format.sampleMimeType != MimeTypes.AUDIO_RAW || bits == null || format.channelCount !in 1..2) {
        throw AudioSink.ConfigurationException("not integer PCM in one or two channels", format)
    }
    // Nothing is opened here. The output opens in handleBuffer, after the one in use has played out.
    wanted = Wanted(format, StreamRequest.pcm(format.sampleRate, format.channelCount, bits))
}
```

`configure` opens nothing. It records what the renderer wants. The output opens in `handleBuffer`, because `configure` can come while the previous track's audio is still playing, and because `handleBuffer` is where Media3 expects an initialisation failure (`InitializationException`).

### 5.3 Take the buffer

```kotlin
override fun handleBuffer(buffer: ByteBuffer, presentationTimeUs: Long, encodedAccessUnitCount: Int): Boolean {
    require(inputBuffer == null || buffer === inputBuffer)
    if (!applyPendingConfiguration()) return false
    val out = active ?: openOutput()
    val frames = layout!!

    if (startMediaTimeUsNeedsInit) {
        startMediaTimeUs = maxOf(0L, presentationTimeUs)
        startMediaTimeUsNeedsSync = false
        startMediaTimeUsNeedsInit = false
    }
    if (inputBuffer == null) {
        // The first time this buffer is offered. Check that it follows the audio written so far.
        if (!buffer.hasRemaining()) return true
        val expectedUs = startMediaTimeUs + durationUs(submittedFrames, frames.rateHz)
        if (!startMediaTimeUsNeedsSync && Math.abs(expectedUs - presentationTimeUs) > 200_000) {
            listener?.onAudioSinkError(AudioSink.UnexpectedDiscontinuityException(presentationTimeUs, expectedUs))
            startMediaTimeUsNeedsSync = true
        }
        if (startMediaTimeUsNeedsSync) {
            startMediaTimeUs += presentationTimeUs - expectedUs
            startMediaTimeUsNeedsSync = false
            if (presentationTimeUs != expectedUs) listener?.onPositionDiscontinuity()
        }
        inputBuffer = buffer
    }

    out.failure?.let { failure ->
        // The DAC went away or the stream failed. While playing that is an error; while paused, wait for the
        // listener's next play, which opens a new output.
        if (playing) throw writeFailure(failure)
        return false
    }
    val done = try {
        write(out, frames, buffer)
    } catch (error: DacException) {
        out.failure = error
        throw writeFailure(error)
    }
    if (done) inputBuffer = null
    return done
}
```

In order, `handleBuffer` does this:

1. It applies a pending format change ([step 7](#7-track-boundaries-gapless-playback-and-sample-rate-changes)).
2. It opens the output if none is open. This call blocks for 100 ms or more. It runs on Media3's playback thread, so the main thread is free.
3. It fixes the position anchor: the presentation time of the first buffer since the last flush. It checks that later buffers follow on from what was written, and reports a discontinuity to Media3 when they do not, as `DefaultAudioSink` does.
4. If the DAC was lost, it throws a `WriteException` while the player is playing. While the player is paused it returns `false` and waits, so a lost DAC pauses cleanly and does not turn into a playback error ([step 12](#12-unplug-and-replug)).
5. It writes, and returns `true` only when the buffer is used up. Media3 offers the same buffer again until then.

### 5.4 Write the frames

```kotlin
/** Writes as many whole frames as the output wants now. Returns true when [buffer] is used up. */
private fun write(out: OpenedOutput, frames: SinkLayout, buffer: ByteBuffer): Boolean {
    if (buffer.remaining() < frames.inputFrameBytes) {
        buffer.position(buffer.limit()) // a partial frame cannot be played
        return true
    }
    val wanted = out.output.writableBytes / frames.outputFrameBytes
    var count = minOf(buffer.remaining() / frames.inputFrameBytes, wanted)
    if (count == 0) return false // the output is as full as it wants to be: try again on the next call
    if (!frames.needsRepack) {
        val limit = buffer.limit()
        buffer.limit(buffer.position() + count * frames.inputFrameBytes)
        try {
            submittedFrames += out.output.write(buffer) / frames.inputFrameBytes
        } finally {
            buffer.limit(limit)
        }
    } else {
        val stage = staging ?: ByteBuffer.allocateDirect(STAGING_BYTES).order(ByteOrder.LITTLE_ENDIAN).also { staging = it }
        count = minOf(count, stage.capacity() / frames.outputFrameBytes)
        stage.clear()
        repack(buffer, frames.inputBytes, frames.outputBytes, count * frames.channels, stage)
        stage.flip()
        val taken = out.output.write(stage) / frames.outputFrameBytes // whole frames the output took
        buffer.position(buffer.position() + taken * frames.inputFrameBytes)
        submittedFrames += taken
    }
    return !buffer.hasRemaining()
}

private fun writeFailure(error: DacException): AudioSink.WriteException =
    AudioSink.WriteException(error.code.ordinal, wanted?.format ?: Format.Builder().build(), false).also { it.initCause(error) }
```

`write` never blocks. It takes whole frames, as many as fit, and the sink asks `writableBytes` first so that the ring keeps to its target depth (250 ms by default). A `false` return means "the ring is as full as it wants to be", and Media3 calls again about 10 ms later.

The layout needs one conversion. Media3 hands over little-endian PCM in 2, 3 or 4 bytes per sample. A DAC may want 24-bit audio in a 4-byte slot, with the sample left-justified:

```kotlin
/**
 * How Media3's PCM maps onto what the output wants. Media3 hands over little-endian PCM in 2, 3 or 4
 * bytes per sample. The output wants the layout of its [dev.usbdac.WriteFormat]: the same, or a wider
 * slot with the sample left-justified (24 bits in a 4-byte slot is the sample shifted left by 8).
 */
class SinkLayout(val inputBytes: Int, val outputBytes: Int, val channels: Int, val rateHz: Int, val counterRateHz: Int) {
    val inputFrameBytes: Int get() = inputBytes * channels
    val outputFrameBytes: Int get() = outputBytes * channels
    val needsRepack: Boolean get() = inputBytes != outputBytes
}

/**
 * Copies [samples] samples from [source] (from its position, which does not move) into [target] and
 * pads each one to [outputBytes] bytes with zero low bytes. Widening keeps every sample value.
 */
fun repack(source: ByteBuffer, inputBytes: Int, outputBytes: Int, samples: Int, target: ByteBuffer) {
    var read = source.position()
    val padding = outputBytes - inputBytes
    repeat(samples) {
        repeat(padding) { target.put(0) }
        repeat(inputBytes) { target.put(source.get(read++)) }
    }
}
```

Widening keeps every sample value, so the stream stays bit-perfect. The slot size the output wants is `WriteFormat.subslotBytes`, which the sink reads when it opens the output (`openOutput` in [step 7](#7-track-boundaries-gapless-playback-and-sample-rate-changes)). Narrowing is never done: the sink refuses an output that wants fewer bytes than the source has.

### 5.5 Position, pending data and the end

```kotlin
override fun getCurrentPositionUs(sourceEnded: Boolean): Long {
    val out = active ?: return AudioSink.CURRENT_POSITION_NOT_SET
    val frames = layout ?: return AudioSink.CURRENT_POSITION_NOT_SET
    if (startMediaTimeUsNeedsInit) return AudioSink.CURRENT_POSITION_NOT_SET
    // What the DAC has played since the last flush, never more than what was written.
    val playedUs = out.output.playedFrames * 1_000_000L / frames.counterRateHz
    return startMediaTimeUs + minOf(playedUs, durationUs(submittedFrames, frames.rateHz))
}

private fun durationUs(frames: Long, rateHz: Int): Long = frames * 1_000_000L / rateHz

override fun handleDiscontinuity() {
    startMediaTimeUsNeedsSync = true
}

override fun hasPendingData(): Boolean = (active?.output?.latencyFrames ?: 0L) > 0L
```

The position is the media time of the first frame since the last flush plus what the DAC has played since. `playedFrames` counts music frames the DAC has taken. It excludes frames that are only written and excludes silence. The library's own docs recommend it ([position](integration-player.md#position)).

```kotlin
override fun play() {
    playing = true
    val out = active ?: return
    // Play after a failure is the listener's decision to try again: the next buffer opens a new output.
    if (out.failure != null) closeActive() else out.output.play()
}

override fun pause() {
    playing = false
    active?.takeIf { it.failure == null }?.output?.pause()
}

override fun playToEndOfStream() {
    if (endOfStream) return
    val out = active
    if (out != null && out.failure == null) {
        try {
            out.output.drain()
        } catch (error: DacException) {
            throw writeFailure(error)
        }
    }
    endOfStream = true
}

override fun isEnded(): Boolean {
    val out = active ?: return true
    return endOfStream && (out.failure != null || out.output.isDrained)
}

override fun flush() {
    startMediaTimeUsNeedsInit = true
    startMediaTimeUsNeedsSync = false
    submittedFrames = 0
    inputBuffer = null
    endOfStream = false
    drainingForSwitch = false
    val out = active ?: return
    // A seek or a skip is where a changed setting or a newly attached DAC takes effect.
    val reopen = out.failure != null || outputs.takeLookAgain() || wanted?.let { !sameFormat(out.request, it.request) } == true
    if (reopen) closeActive() else out.output.flush()
}

override fun reset() {
    flush()
    closeActive()
    wanted = null
}

override fun release() = reset()
```

The transport methods map one to one onto the output: `play`, `pause`, `flush`, and `drain` for the end of the stream. Two details are specific to this sink. A `flush` is also where a changed setting or a newly attached DAC takes effect ([step 8](#8-pause-seek-and-skip)). And `play` after a failure drops the dead output, so the next buffer opens a new one: the listener's play is the decision to try again.

What the sink leaves alone:

```kotlin
// A direct stream has no AudioTrack: ducking becomes the DAC's volume, the rest is not applied.
override fun setVolume(volume: Float) = outputs.setVolumeMultiplier(volume)

// Speed and silence skipping change the samples, so they are refused: the parameters stay at their defaults.
override fun setPlaybackParameters(playbackParameters: PlaybackParameters) = Unit
override fun getPlaybackParameters(): PlaybackParameters = PlaybackParameters.DEFAULT
override fun setSkipSilenceEnabled(skipSilenceEnabled: Boolean) = Unit
override fun getSkipSilenceEnabled(): Boolean = false

override fun setListener(listener: AudioSink.Listener) {
    this.listener = listener
}

override fun setAudioAttributes(audioAttributes: AudioAttributes) {
    attributes = audioAttributes
}

override fun getAudioAttributes(): AudioAttributes? = attributes
override fun getAudioTrackBufferSizeUs(): Long = C.TIME_UNSET
override fun setAudioSessionId(audioSessionId: Int) = Unit
override fun setAuxEffectInfo(auxEffectInfo: AuxEffectInfo) = Unit
override fun setPreferredDevice(audioDeviceInfo: AudioDeviceInfo?) = Unit
override fun enableTunnelingV21() = Unit
override fun disableTunneling() = Unit
```

`setVolume` is the one that matters. See [step 10](#10-volume-keys-and-the-system-panel).

**Why a custom sink.** `DefaultAudioSink` plays through an `AudioTrack`, and nothing on that path can reach the DAC. A direct stream has its own ring buffer, its own clock and its own position counter, so the sink replaces `DefaultAudioSink` instead of wrapping it.

**Check.** Run the tests. They use a fake output and need no device:

```sh
cd android
./gradlew :sample:testDebugUnitTest -Pusbdac.skipNative=true
```

They cover the widening, opening on the first buffer, keeping one output across a track change of the same format, draining then reopening on a new format, seeks and the position anchor, a pause before the first buffer, a lost DAC while playing and while paused, a full output, ducking and the formats it declines.

## 6. Build the player and the service

**Do.** Give ExoPlayer a renderers factory that builds your sink. This is the only change to a standard player.

```kotlin
/**
 * Where a typical player builds its objects: a Hilt module, a service locator or plain code. The
 * controller lives as long as the process, because the settings screen reads it too.
 */
object PlayerModule {
    private val musicAttributes = AudioAttributes.Builder()
        .setUsage(C.USAGE_MEDIA)
        .setContentType(C.AUDIO_CONTENT_TYPE_MUSIC)
        .build()

    @Volatile
    private var controller: DirectOutputController? = null

    fun outputs(context: Context, settings: DirectOutputSettings = DirectOutputSettings()): DirectOutputController =
        controller ?: synchronized(this) {
            controller ?: DirectOutputController(
                context.applicationContext,
                settings,
                CoroutineScope(SupervisorJob() + Dispatchers.Default),
            ).also { controller = it }
        }

    @OptIn(UnstableApi::class)
    fun player(context: Context, outputs: DirectOutputController): ExoPlayer {
        val renderers = object : DefaultRenderersFactory(context) {
            // The one change to a standard player: the audio sink. Everything else stays Media3's.
            override fun buildAudioSink(
                context: Context,
                enableFloatOutput: Boolean,
                enableAudioTrackPlaybackParams: Boolean,
            ): AudioSink = UsbDacAudioSink(outputs)
        }
        return ExoPlayer.Builder(context, renderers)
            // Audio focus stays Media3's job: it pauses on a loss and asks the sink to duck (setVolume).
            .setAudioAttributes(musicAttributes, /* handleAudioFocus = */ true)
            // Pauses for outputs that go through Android when headphones are unplugged. A direct stream
            // reports the loss of its DAC itself (DacEvent.DeviceLost).
            .setHandleAudioBecomingNoisy(true)
            .setWakeMode(C.WAKE_MODE_NETWORK) // a streaming player: keep the CPU and Wi-Fi awake while it plays
            .build()
    }
}
```

Then the media service. It creates the player, wraps it for the volume ([step 10](#10-volume-keys-and-the-system-panel)), listens for DAC changes and for a lost DAC, and gives the DAC back when it is destroyed.

```kotlin
/**
 * The media service of a typical streaming player. The service, not an activity, owns the player, and
 * through the sink the DAC: the music has to go on when the listener swipes the app away.
 */
@OptIn(FlowPreview::class)
class PlaybackService : MediaSessionService() {
    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.Main.immediate)
    private lateinit var outputs: DirectOutputController
    private lateinit var player: ExoPlayer
    private var session: MediaSession? = null

    override fun onCreate() {
        super.onCreate()
        outputs = PlayerModule.outputs(this)
        player = PlayerModule.player(this, outputs)
        session = MediaSession.Builder(this, DacVolumePlayer(player, outputs, scope)).build()

        // A DAC attached, detached or allowed, or a route that came or went. One plug raises several.
        // Nothing moves now: the output is chosen again at the next seek, skip or format change.
        outputs.changes.debounce(500).onEach { outputs.onChanged() }.launchIn(scope)

        // The DAC is gone: pause, say why, and do not carry on through the phone's speaker.
        outputs.events
            .filterIsInstance<DacEvent.DeviceLost>()
            .onEach {
                player.pause()
                outputs.explain("The DAC was unplugged. Playback is paused.")
            }
            .launchIn(scope)
    }

    override fun onGetSession(controllerInfo: MediaSession.ControllerInfo): MediaSession? = session

    override fun onTaskRemoved(rootIntent: Intent?) {
        // Swiped away while paused: nothing is playing, so give the DAC back.
        if (!player.playWhenReady || player.mediaItemCount == 0) stopSelf()
    }

    override fun onDestroy() {
        scope.cancel()
        session?.run {
            release()
            session = null
        }
        player.release() // the playback thread closes the output, which hands the DAC back to Android
        super.onDestroy()
    }
}
```

**Why.**

- `buildAudioSink` is where Media3 lets you replace the sink. Everything else about the player stays Media3's, including audio focus ([step 11](#11-audio-focus)).
- The service owns the player, and through the sink, the DAC. If an activity owned it, swiping the app away would stop the music and leave the DAC claimed until the process died.
- `onDestroy` releases the player. The playback thread closes the output, which takes about 100 ms and hands the DAC back to Android and its volume back to the level it had. ExoPlayer waits 500 ms by default for that release; if your DACs close more slowly, raise `ExoPlayer.Builder.setReleaseTimeoutMs`.
- `DeviceLost` pauses the player. The service does not move playback to the speaker.

**Check.** Play a track with no DAC attached. It should play through the speaker or the headphones, exactly as before, and `outputs.selected.value?.decision?.kind` is `ANDROID_STANDARD`. That proves the sink works on its own. Then plug in a DAC and play again: with permission held, the kind is `DIRECT_USB` and the sound comes from the DAC.

## 7. Track boundaries, gapless playback and sample-rate changes

**Do.** Nothing new. This step explains what the code of step 5 does at a boundary, so you know what to expect.

`configure` is called when the decoder's output format changes, not at every track. The sink compares what it is asked for (rate, channels, bit depth) with what the open output was opened for:

| The next track | What happens | What the listener hears |
|---|---|---|
| Same rate, depth and channels, same or different codec | The output is kept. Nothing is drained or flushed | Gapless, if the files themselves are gapless |
| A different rate, such as 44.1 kHz then 96 kHz | The old output plays out, closes, and a new one opens. The DAC's clock is reprogrammed | A gap of about 100 ms or more, usually a few hundred; the DAC's display changes rate |
| A different bit depth | The same | The same |
| A rate the DAC cannot run | The selector picks the next output, or converts if the listener allowed it | Plays through another route, or at another rate, and the indicator says so |

The code that does this is `applyPendingConfiguration` in the `sink-switch` excerpt:

```kotlin
/**
 * Applies what configure() asked for. The same format keeps the output (the gapless case). A different
 * one lets the old output play out first, then closes it and opens a new one. Returns false while the
 * old output is still playing.
 */
private fun applyPendingConfiguration(): Boolean {
    val next = wanted ?: return true
    val out = active ?: return true // handleBuffer opens one
    // A failed output is not replaced here: handleBuffer reports it, and the listener's play opens a new one.
    if (out.failure != null || sameFormat(out.request, next.request)) return true
    if (!drainingForSwitch) {
        out.output.drain()
        drainingForSwitch = true
    }
    if (!out.output.isDrained) return false
    closeActive()
    return true // handleBuffer opens the new one
}

private fun sameFormat(a: StreamRequest, b: StreamRequest) =
    a.sampleRate == b.sampleRate && a.channels == b.channels && a.bitDepth == b.bitDepth

private fun openOutput(): OpenedOutput {
    val next = checkNotNull(wanted) { "configure() comes before the first buffer" }
    val out = try {
        outputs.open(next.request) // blocks for 100 ms or more
    } catch (error: DacException) {
        throw AudioSink.InitializationException("no output can play ${next.request}: ${error.message}", 0, next.format, false, error)
    }
    val write = out.format
    if (write.subslotBytes < next.request.bitDepth / 8 || write.channels != next.request.channels) {
        outputs.close(out)
        throw AudioSink.InitializationException("the output wants ${write}, which is not a widening of ${next.request}", 0, next.format, false, null)
    }
    active = out
    drainingForSwitch = false
    layout = SinkLayout(next.request.bitDepth / 8, write.subslotBytes, write.channels, write.sampleRate, write.counterRateHz)
    if (!playing) out.output.pause()
    return out
}

private fun closeActive() {
    active?.let { outputs.close(it) }
    active = null
    layout = null
    drainingForSwitch = false
}
```

The old audio plays out before the new output opens, so the end of the previous track is not cut off. If the player is paused during that wait, the drain waits too, and a seek cancels it.

**Why.** A direct stream sets the DAC's clock to the source's rate, so the DAC plays the file's own rate and never has to resample. Programming the clock takes time, and the DAC may click or relock. With an `AudioTrack` the mixer hides this.

**Check.** Queue two 44.1 kHz tracks of the same depth. The output opens once (the controller's `open` runs once), and the tracks play with no gap. Then queue a 44.1 kHz track followed by a 96 kHz one. The output closes and opens once, and the DAC shows the new rate.

## 8. Pause, seek and skip

**Do.** Nothing in the player's own code changes. The sink maps what Media3 asks onto the output:

| Player action | Media3 calls | The sink does |
|---|---|---|
| Pause | `pause()` | `output.pause()`. The ring keeps what it holds and the DAC gets silence, so it stays locked |
| Resume | `play()` | `output.play()`. It continues from exactly where it stopped |
| Seek, or skip by the listener | `flush()`, then new buffers | `output.flush()`. The ring is dropped and the audio already on the bus is cancelled, so the new position is heard within a couple of milliseconds. The position anchor restarts |
| Next track by itself, same format | Nothing, or `handleDiscontinuity()` | Keeps writing |
| End of the playlist | `playToEndOfStream()` | `output.drain()`, then `isEnded()` when `isDrained` |
| Stop, or releasing the player | `reset()`, `release()` | Flushes and closes the output, which hands the DAC back |

`flush()` is also where the output is chosen again, when something changed since it opened: the listener changed a setting, a DAC was plugged in, or its permission was granted. In the controller, `onChanged()` and `apply(settings)` raise a flag, and `takeLookAgain()` hands it to the next `flush`. A seek to the current position is a way to apply a change now:

```kotlin
/**
 * Applies a changed setting or a newly allowed DAC now instead of at the next track: a seek to where the
 * player already is flushes the sink, and the flush chooses the output again. The listener hears a short gap.
 */
fun switchOutputNow(player: Player, outputs: DirectOutputController) {
    outputs.onChanged()
    player.seekTo(player.currentPosition)
}
```

**Why.** The library's rule is "flush on every replacement": a deep ring costs nothing in responsiveness if you flush on each seek and skip. `DefaultAudioSink` releases its `AudioTrack` on every flush because of old `AudioTrack` bugs; a direct stream does not need that, and keeping the DAC open across a seek avoids reprogramming its clock.

**Check.** Seek while playing: the old position stops at once and the new one starts without a click. Pause for a few seconds and resume: the sound continues exactly where it stopped. Seek while paused: no old audio is heard when you resume.

## 9. USB permission and hot-plug

**Do.** Ask for USB permission from an activity, never from the service, and only when the screen can show the dialog.

```kotlin
/**
 * Whether the system dialog can be shown now. It needs an app in front that the listener can see: not a
 * service, and not a locked screen, where the dialog would be missed or refused.
 */
fun mayAskForUsbPermission(activity: Activity): Boolean {
    val keyguard = activity.getSystemService(KeyguardManager::class.java)
    return activity.hasWindowFocus() && !keyguard.isKeyguardLocked
}

/**
 * The selector never asks. When it found a DAC it could not use for lack of permission, its decision
 * names it; the activity asks, and the output is chosen again at the next seek or track.
 */
fun askForUsbPermissionIfNeeded(activity: Activity, decision: OutputDecision?, onGranted: (DacDeviceInfo) -> Unit) {
    val dac = decision?.needsUsbPermission ?: return
    if (!mayAskForUsbPermission(activity)) return
    UsbDac.requestPermission(activity, dac) { granted -> if (granted) onGranted(dac) }
}
```

The selector does not ask. When a DAC is attached and the app lacks permission, the decision still names the best output without it, and `decision.needsUsbPermission` holds the DAC. Show a line in the player ("USB DAC found: allow access to play through it") with a button that calls `askForUsbPermissionIfNeeded`. When the permission is granted, call `switchOutputNow` from step 8, or wait for the next track.

The DACs coming and going arrive through `selector.changes` (the controller's `changes`), which the service already collects and debounces in step 6. One plug raises several changes, one for the USB device and one or more for audio routes, which is why the service waits 500 ms.

**Why not ask in the other cases.** Android may refuse to show the dialog from the background, and a dialog that appears over a locked screen is missed. The listener who chose "always" in the attach dialog of step 2 is not asked again for that DAC. Without it, permission is lost when the DAC is unplugged. Remember a denial per DAC and ask again only when the listener asks you to.

**Check.** Plug in a DAC with the app open and the permission not yet granted: the decision is not `DIRECT_USB` and `needsUsbPermission` names the DAC. Grant it and seek: the decision becomes `DIRECT_USB`.

## 10. Volume keys and the system panel

**Do.** Use the DAC's own volume. On a direct stream Android's volume does not reach the DAC, and software gain would end bit-perfect output.

The controller exposes the output in use as `selected`, and `selected.value?.volume` is the `DacVolume`. It is `null` when there is no direct output or the DAC has no usable volume control. The setters return at once and are safe on every slider event: a burst collapses to its last value.

**The keys, with an activity in front.** Route them to the DAC:

```kotlin
/** The phone's volume keys, while an activity of the app is in front. The panel and the other keys go through the session. */
fun handleVolumeKey(event: KeyEvent, volume: DacVolume?): Boolean = DacVolumeKeys.handle(event, volume)
```

Call `handleVolumeKey(event, outputs.selected.value?.volume)` from your activity's `dispatchKeyEvent`, and fall back to `super` when it returns `false`. The helper consumes the volume keys only while there is a DAC volume, so the phone's volume keeps working otherwise.

**The system panel and the keys with the screen off.** A Media3 session owns the Android media session it creates and offers no way to attach a platform `VolumeProvider` to it, so `DacVolumeProvider` does not fit here. Media3 has its own route: when the session's player reports a remote device volume, Media3 gives Android a volume provider that calls back into the player. This wrapper reports the DAC's volume as that device volume:

```kotlin
/**
 * Puts the DAC's volume where Media3 looks for the device volume. A session whose player reports a
 * remote device volume gives Android a volume provider, so the system volume panel and the hardware
 * keys (also with the screen off) end up in the methods below. While no DAC volume is open the
 * player behaves as the one it wraps, and the keys change the phone's volume.
 *
 * Create it on the application thread, with the same looper as [player].
 */
@Suppress("DEPRECATION") // the device volume methods without flags are still part of Player
@OptIn(UnstableApi::class)
@kotlin.OptIn(ExperimentalCoroutinesApi::class)
class DacVolumePlayer(
    player: Player,
    private val outputs: DirectOutputController,
    scope: CoroutineScope,
) : ForwardingPlayer(player) {
    private val listeners = CopyOnWriteArrayList<Player.Listener>()
    private var volume: DacVolume? = null

    init {
        // The DAC's volume appears when a direct output opens and goes when it closes.
        outputs.selected
            .map { it?.volume }
            .distinctUntilChanged()
            .onEach { next ->
                volume = next
                listeners.forEach {
                    it.onDeviceInfoChanged(getDeviceInfo())
                    it.onAvailableCommandsChanged(getAvailableCommands())
                    it.onDeviceVolumeChanged(getDeviceVolume(), isDeviceMuted())
                }
            }
            .launchIn(scope)
        // The level changes from this app's slider, a key or the panel: tell the session.
        outputs.selected
            .flatMapLatest { it?.volume?.state ?: flowOf(null) }
            .onEach { listeners.forEach { it.onDeviceVolumeChanged(getDeviceVolume(), isDeviceMuted()) } }
            .launchIn(scope)
    }

    override fun addListener(listener: Player.Listener) {
        listeners.add(listener)
        super.addListener(listener)
    }

    override fun removeListener(listener: Player.Listener) {
        listeners.remove(listener)
        super.removeListener(listener)
    }

    override fun getAvailableCommands(): Player.Commands {
        if (volume == null) return super.getAvailableCommands()
        return super.getAvailableCommands().buildUpon()
            .addAll(
                Player.COMMAND_GET_DEVICE_VOLUME,
                Player.COMMAND_SET_DEVICE_VOLUME,
                Player.COMMAND_SET_DEVICE_VOLUME_WITH_FLAGS,
                Player.COMMAND_ADJUST_DEVICE_VOLUME,
                Player.COMMAND_ADJUST_DEVICE_VOLUME_WITH_FLAGS,
            )
            .build()
    }

    override fun getDeviceInfo(): DeviceInfo {
        if (volume == null) return super.getDeviceInfo()
        return DeviceInfo.Builder(DeviceInfo.PLAYBACK_TYPE_REMOTE).setMinVolume(0).setMaxVolume(MAX).build()
    }

    override fun getDeviceVolume(): Int =
        volume?.let { (it.state.value.fraction * MAX).roundToInt() } ?: super.getDeviceVolume()

    override fun isDeviceMuted(): Boolean = volume?.let { it.state.value.muted == true } ?: super.isDeviceMuted()

    override fun setDeviceVolume(volume: Int) {
        this.volume?.setFraction(volume / MAX.toDouble()) ?: super.setDeviceVolume(volume)
    }

    override fun setDeviceVolume(volume: Int, flags: Int) {
        this.volume?.setFraction(volume / MAX.toDouble()) ?: super.setDeviceVolume(volume, flags)
    }

    override fun increaseDeviceVolume() {
        volume?.step(1) ?: super.increaseDeviceVolume()
    }

    override fun increaseDeviceVolume(flags: Int) {
        volume?.step(1) ?: super.increaseDeviceVolume(flags)
    }

    override fun decreaseDeviceVolume() {
        volume?.step(-1) ?: super.decreaseDeviceVolume()
    }

    override fun decreaseDeviceVolume(flags: Int) {
        volume?.step(-1) ?: super.decreaseDeviceVolume(flags)
    }

    override fun setDeviceMuted(muted: Boolean) {
        val dac = volume
        if (dac == null) super.setDeviceMuted(muted) else if (dac.hasMute) dac.setMute(muted)
    }

    override fun setDeviceMuted(muted: Boolean, flags: Int) {
        val dac = volume
        if (dac == null) super.setDeviceMuted(muted, flags) else if (dac.hasMute) dac.setMute(muted)
    }

    private companion object {
        const val MAX = 100
    }
}
```

The service wraps its player in it when it builds the session (step 6). While no DAC volume exists the wrapper changes nothing and the keys control the phone. The DAC's level is reported on a scale of 0 to 100 over its volume curve, not in decibels.

**Ducking.** Media3 handles audio focus, and when another app asks for a short duck, Media3 does it by lowering a volume multiplier that reaches the sink through `setVolume(0.2f)`. Nothing goes through Android's mixer on a direct stream, so the duck would do nothing. The controller maps it onto the DAC's volume and puts the level back when the multiplier returns to 1:

```kotlin
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
```

**Rules for the level.**

- Show the level in dB from `state.db` and remember it per DAC in dB, not as a slider position. The curve is not linear in the DAC's range and a position means something else on another DAC.
- When a DAC connects the library lowers it to at most -40 dB and records what it had. `volume.startup.limitDb` is the loudest level a remembered setting may restore without the listener's action.
- A knob on the DAC changes its level without telling the library. Call `refresh()` off the main thread when you need the true level.
- Volume exists once the first output has opened. Before the first track, show the remembered level, not a live one.

**Check.** Play through a DAC. Press the phone's volume keys with the app in front, with another app in front, and with the screen off. The DAC's level follows each time and the phone's media volume does not change. Pull down the system panel: it shows a slider that moves the DAC.

## 11. Audio focus

**Do.** Nothing beyond what step 6 already has: `setAudioAttributes(attributes, handleAudioFocus = true)` on the player.

**Why.** The library does no audio-focus handling, and with a direct stream nothing flows through Android's mixer, so Android cannot silence your output for you. Media3 can: it requests focus, pauses on a loss and asks the sink to duck. The sink already acts on both. Pause comes through `pause()`. Ducking comes through `setVolume`, handled in step 10. A player that handles focus itself must do both.

Other apps' sounds do not reach a DAC that is open directly. They play through the phone's speaker or other route.

**Check.** Start music through the DAC, then start a call or another media app. The music pauses or ducks and comes back when the other app lets go.

## 12. Unplug and replug

**Do.** The code is already there. The controller marks the output as failed when the session reports `DeviceLost` or `StreamFailed`, and the service pauses the player on `DeviceLost`:

```text
DacEvent.DeviceLost  ->  controller marks the output failed  ->  service: player.pause() + notice
                                                          ->  sink: paused, so handleBuffer returns false
listener presses play ->  sink.play() drops the dead output   ->  next buffer opens a new one
```

What the listener gets is a pause with a message, and no sound on the speaker until they press play. When they press play with the DAC plugged back in, the selector picks direct USB again if the permission is still held. If the DAC is gone, it picks the standard output, which means the phone's speaker or headphones. That choice is the listener's, because they pressed play.

**Why.** A listener who chose a DAC and hears the phone speaker will not forgive it. The library does not decide this for you ([failures and fallback](integration-app.md#failures-and-fallback)).

**Route changes for the outputs that go through Android.** `setHandleAudioBecomingNoisy(true)` in step 6 pauses the player when headphones are unplugged for the standard output and the bit-perfect mixer. For a direct stream use `DeviceLost`. Do not rely on the platform's headphone-unplug broadcast for a DAC that the library holds.

**Check.** Play through the DAC and unplug it. Playback pauses within a moment, the notice appears and the speaker stays silent. Plug the DAC in again and press play: the sound returns on the DAC.

## 13. Show the signal path and the bit-perfect indicator

**Do.** Show what the app is doing in terms the listener can read:

```kotlin
/** Where the audio came from, which only the player knows. */
enum class SourceKind { LOSSLESS, LOSSY }

/** From the compressed format the player reads (`ExoPlayer.audioFormat.sampleMimeType`). */
fun sourceKindOf(sampleMimeType: String?): SourceKind = when (sampleMimeType) {
    MimeTypes.AUDIO_FLAC, MimeTypes.AUDIO_ALAC, MimeTypes.AUDIO_RAW, MimeTypes.AUDIO_WAV -> SourceKind.LOSSLESS
    else -> SourceKind.LOSSY
}

/**
 * One line for the now-playing screen. The library's flag covers the transport only, so the line also
 * says what the player did to the samples and what the source was.
 */
fun indicatorText(state: BitPerfectState, engineUntouched: Boolean, source: SourceKind): String {
    val decodedFromLossy = if (source == SourceKind.LOSSY) ", decoded from a lossy file" else ""
    return when {
        !engineUntouched -> "Not bit-perfect: the player is processing the audio"
        state is BitPerfectState.Verified -> "Bit-perfect$decodedFromLossy"
        state is BitPerfectState.NotVerified -> "Not bit-perfect: ${state.reason}"
        state is BitPerfectState.RequestedViaPlatform -> "Bit-perfect mode requested from Android, not verifiable"
        state is BitPerfectState.Unverifiable -> state.reason
        else -> ""
    }
}

/** "Direct USB, 96000 Hz, 24-bit, stereo" for the signal path view. */
fun outputLine(output: SelectedOutput): String {
    val kind = when (output.kind) {
        OutputKind.DIRECT_USB -> "Direct USB"
        OutputKind.ANDROID_BIT_PERFECT -> "Android bit-perfect mixer"
        OutputKind.AAUDIO_EXCLUSIVE -> "Exclusive AAudio"
        OutputKind.ANDROID_STANDARD -> "Android output"
    }
    val format = output.format
    return "$kind, ${format.sampleRate} Hz, ${format.validBits}-bit, ${if (format.channels == 2) "stereo" else "${format.channels} ch"}"
}
```

**Why.** The library's flag covers the transport. It does not know what the player did to the samples, or what the source was. Combine the three:

- `output.bitPerfect.value` is the library's verdict. Collect the flow and update the line when it changes.
- `engineUntouched` is yours. In this sink it is true unless you add something upstream: an equaliser, ReplayGain, crossfade. The sink itself applies no gain and refuses speed changes.
- The source kind decides the wording. A decoded lossy file through a verified stream is "decoded from a lossy file", never plain "bit-perfect".

Reasons come from `NotVerified.reason`, one sentence for the first condition that fails, from `LOST_FRAMES` ("audio queued to the DAC was lost on the bus") to `FEEDBACK_UNLOCKED`. `PAYLOAD_UNVERIFIED` appears for the first moments of a stream, and after a long stretch of digital silence, because the library checks the wire for real audio. Do not alarm the listener over it: show a neutral state until real audio has played.

The signal path view needs a few more lines, all from the output:

- The route and its format, from `outputLine(output)`.
- The guarantee and the reason, from `decision`.
- Why a better output was not used, from `decision.alternativesRejected`.
- The clock and pacing in a details view: `stream.format.clock` and `stream.format.pacing`, the first things to read when a DAC plays at the wrong pitch.

**Check.** Play a short lossless file through the DAC. The line reads "Bit-perfect" after a moment of real audio. Turn on a software gain or an equaliser in your app: the line changes to say the player is processing. Play an MP3: it says "decoded from a lossy file".

## 14. Settings screen

**Do.** The three settings of step 3, an output line, and a "why not" detail.

Put the output line and the decision reason at the top of the screen, as plain text. When the decision has `needsUsbPermission`, show the button of step 9. When the selected output is not direct USB, say why, using the first entry of `alternativesRejected`. When the listener changes a setting while music plays, say that it applies at the next seek or track, or offer "apply now", which calls `switchOutputNow`.

**What to leave out of the screen.** The options listed in step 3. A "choose the output yourself" setting (`force`) belongs in a debug build, because a forced output that cannot open fails instead of falling back.

**Check.** Change each setting while a DAC is attached and a track plays. The text changes, the output changes at the next seek and the old output is closed first.

## 15. Diagnostics and support reports

**Do.** Give the listener a way to copy a report, and give yourself a log switch.

```kotlin
/**
 * What a bug report needs from the output in use, as text the listener can copy. The library writes no
 * file and sends nothing: where it goes is the app's decision.
 */
fun supportReport(output: SelectedOutput, appVersion: String): String = buildString {
    appendLine("app $appVersion, usbdac ${UsbDac.nativeVersion}, ${android.os.Build.MODEL}, Android ${android.os.Build.VERSION.SDK_INT}")
    appendLine(describeOutput(output))
    output.decision.dac?.let { appendLine("DAC: ${it.productName}, USB ${"%04x:%04x".format(it.vendorId, it.productId)}") }
    val stream = output.stream
    if (stream != null) {
        appendLine("clock and pacing: ${stream.format.pacing.description}; ${stream.format.clock?.description ?: "no clock entities"}")
        appendLine(stream.stats().toString())
    }
    appendLine("bit-perfect: ${output.bitPerfect.value}")
    output.session?.capabilities?.let { appendLine("DAC: $it") }
}

private fun describeOutput(output: SelectedOutput): String = buildString {
    appendLine(outputLine(output))
    append("${output.decision.guarantee}: ${output.decision.reason}")
    output.decision.alternativesRejected.forEach { append("\nnot ${it.kind}: ${it.why}") }
}
```

The report holds the app, library, phone and Android versions, the output and the reasons, the DAC's name and USB ids, the clock and pacing, the stream's counters and the bit-perfect state. It holds no track names, no account details and no USB serial numbers. The library writes no file and sends nothing. Where the text goes is your decision, and the listener should see it before it leaves the phone.

For the debug log, `setSupportLogging(true)` from step 1 sends the native library's messages to logcat under the tag `usbdac`. For a USB feedback problem, set `captureSupportTrace` in the settings of step 3 before opening the output, reproduce the problem once and export `session.feedbackTraceJson()`. [Collect a USB playback support report](support-diagnostics.md) has the whole flow and what to keep out of the file.

Keep the first failure. When playback fails and the app falls back, save the decision and the error before the next output opens, so the report shows what happened first.

Treat `DacEvent.Unknown` and `BitPerfectBlocker.UNKNOWN` as a newer native library than your SDK, and log them.

**Check.** Play through a DAC, copy the report and read it. Does it say what a support engineer would need to know, and nothing about the listener?

## 16. Test it

**Without a DAC.** The sink and everything below it is written against `PcmOutput`, which is an interface, so a fake output tests it on the JVM. The fake records what the player does to it:

```kotlin
/**
 * A [PcmOutput] that records what the player does to it, so the sink, the seek order and the track
 * changes can be tested with no DAC. It lives in main source only so that the guide can quote it; put
 * it in your test source set.
 */
class FakePcmOutput(private val bytesPerFrame: Int, private val room: Int = Int.MAX_VALUE) : PcmOutput {
    /** The calls in order: "write 4800", "pause", "flush", ... */
    val calls = mutableListOf<String>()
    val written = ByteArrayOutputStream()

    /** Set by a test to say how far the DAC has played. */
    @Volatile
    var played = 0L

    var drained = false
    var closed = false
    var failWith: DacException? = null

    override fun write(buffer: ByteBuffer): Int {
        failWith?.let { throw it }
        if (closed) throw DacException(DacErrorCode.INVALID_STATE, "closed")
        val taken = buffer.remaining() / bytesPerFrame * bytesPerFrame
        val bytes = ByteArray(taken)
        buffer.get(bytes)
        written.write(bytes)
        calls += "write $taken"
        return taken
    }

    override val writableBytes: Int get() = room
    override fun play() { calls += "play" }
    override fun pause() { calls += "pause" }
    override fun flush() { calls += "flush"; played = 0; written.reset() }
    override fun drain() { calls += "drain" }
    override val isDrained: Boolean get() = drained
    override val playedFrames: Long get() = played
    override val latencyFrames: Long get() = written.size() / bytesPerFrame - played
    override val bitPerfect: StateFlow<BitPerfectState> = MutableStateFlow(BitPerfectState.Verified)
    override fun close() { closed = true; calls += "close" }
}

/** An [OutputSource] that opens a [FakePcmOutput] in the layout a test chooses. */
class FakeOutputSource(private val wireBytes: Int? = null, private val room: Int = Int.MAX_VALUE) : OutputSource {
    val opened = mutableListOf<FakePcmOutput>()
    private val handles = mutableListOf<OpenedOutput>()
    var lookAgain = false
    var multiplier = 1f

    override fun open(request: StreamRequest): OpenedOutput {
        val sampleBytes = wireBytes ?: (request.bitDepth / 8)
        val fake = FakePcmOutput(sampleBytes * request.channels, room)
        opened += fake
        val format = WriteFormat(request.sampleRate, request.channels, sampleBytes, request.bitDepth, request.sampleRate)
        return OpenedOutput(fake, format, request).also { handles += it }
    }

    /** What the controller does when the session reports `DeviceLost` or `StreamFailed`. */
    fun failure(index: Int, error: DacException) {
        handles[index].failure = error
    }

    override fun close(output: OpenedOutput) = output.output.close()
    override fun takeLookAgain(): Boolean = lookAgain.also { lookAgain = false }
    override fun setVolumeMultiplier(multiplier: Float) { this.multiplier = multiplier }
}
```

The sample's `UsbDacAudioSinkTest` uses these two classes. Copy the idea into your test source set, and add a test for every rule you depend on: no old audio written after a seek, the output kept across a gapless change, the tail played out before a format change, a lost output while paused.

**On a phone, without a DAC.**

| Check | What to look for |
|---|---|
| Play on the speaker | Plays as before; the decision is `ANDROID_STANDARD` |
| Plug in wired headphones | The player behaves as before, and no output errors appear |
| Connect Bluetooth headphones | Standard output; no attempt at direct USB |
| Rotate, background and swipe the app away | Music continues, and stops with the service |
| Run through a playlist of mixed rates | No stuck state at any change |

**On a phone, with a DAC.** Adapt this to your devices and write down the phone, Android version, DAC, cable and any hub.

| Check | What to look for |
|---|---|
| Cold start: DAC plugged in, app launched, first track | The decision is `DIRECT_USB`; the indicator reaches "Bit-perfect" after real audio; the level is at or below -40 dB |
| 44.1, 48 and 96 kHz and the DAC's highest rate; 16 and 24 bit | No clock refusal; the DAC shows each rate |
| Same-format gapless | No gap and no click |
| A rate change between tracks | A gap of 100 ms or more; the right rate on the DAC; no stale audio |
| Pause, resume, seek while playing and while paused | The sound stops and starts at once; no old audio after a seek |
| Skip tracks quickly | No stale audio, no stuck state |
| Volume: keys in the app, keys elsewhere, keys with the screen off, the panel, the knob on the DAC | The level follows each control; the dB shown is right |
| Duck: start a navigation prompt or a call | The level drops and returns |
| Unplug during playback, then replug | A pause with a message; no sound on the speaker; play opens the DAC again |
| Permission: first plug, "always" in the attach dialog, a denial | The text of step 9; no dialog from the service or a locked screen |
| Close the app, then play from another app | The DAC is back at its original level and Android plays through it |
| A DAC with no volume control | The decision rejects direct USB until the setting allows a fixed level |
| Software gain or processing in your app | The indicator turns off with the right reason |

Send what you find, with the log, to [the issue tracker](https://github.com/johnneerdael/android-usbdac/issues).

## What this does not cover

Know these before you ship. Each is a place where this integration is thinner than a standard Media3 player.

- **Formats.** The sink takes integer PCM in one or two channels. More channels, float and compressed passthrough are declined, so the renderer decodes to something the sink accepts. Because float is declined, a platform decoder is likely to give 16-bit for a 24-bit source. A software decoder that outputs 24-bit or 32-bit integers would keep the depth. Neither has been tested here, so check the format your player really delivers before you describe it as hi-res.
- **Gapless trimming of lossy files.** `DefaultAudioSink` trims the encoder delay and padding that MP3 and AAC files carry. This sink does not, so albums of such files have a small silent gap between tracks, tens of milliseconds. Files without that padding (FLAC, WAV) are not affected.
- **Speed, pitch and silence skipping.** They change the samples, so the sink ignores them and reports the default parameters. A speed control in your UI will have no effect through the DAC.
- **Audio processors.** The `AudioProcessor` chain of `DefaultAudioSink` does not run. An equaliser built on it needs another place, or direct mode should bypass it.
- **Choosing again.** The output is chosen again at a flush or a format change, not at an automatic track change of the same format.
- **Idle release.** A paused direct stream keeps the DAC busy. [`DacIdleRelease`](controls.md#idle-release) closes the stream after a long pause, but the ring is dropped with it, so a player must seek back to the position when it resumes. This guide does not wire it.
- **Volume before the first track.** A `SelectedOutput` exists only with a stream, so the DAC's volume is not available until the first output opens.
- **DSD.** The sink plays PCM only. DSD and DoP need the player to decode to the DSD bitstream, which Media3 does not do. See [bit-perfect output](bit-perfect.md#dsd).
- **`OutputHandoff`.** The helper is for players that run their own decoder loop. In a Media3 player the sink's switch (step 7) does its job, and ExoPlayer keeps the position.

## Common mistakes

- **Opening the output on the main thread.** `OutputSelector.open`, `close` and a DAC's `negotiate` take 100 ms or more. The sink opens on Media3's playback thread. Keep the controller's `open` and `close` off the main thread, and do not call them from a click handler.
- **Two owners for the DAC.** A selector refuses to open a second output. Keep one controller, close before you open and never open a `DacSession` of your own beside it: the library allows one open DAC per process.
- **Falling back to the speaker without telling the listener.** Pause and say why. The listener's play is the decision.
- **Applying software volume and still showing "bit-perfect".** Ducking and the volume keys use the DAC's volume. Anything that scales samples ends the claim.
- **Decoding to float.** A decoder that returns float and an engine that converts to integers has changed the samples, unless the conversion is exact. The sink declines float for that reason.
- **Calling `flush` or `drain` between gapless tracks.** `drain` ends the stream until the next `flush`. The sink drains only at the end of the playlist and before a format change.
- **Writing from two threads, or from a thread of your own.** The sink is called on Media3's playback thread. Do not call into it from elsewhere.
- **Closing the session from an event collector.** The library forbids it. The service reacts to events by pausing the player, and the sink closes the output on its own thread.
- **Treating the decision as a live verdict.** The decision is made before the first sample. Read `bitPerfect` for what is true now.
- **Expecting a change to apply mid-track.** The selector never moves a stream that is playing. Say so in the settings screen, or offer "apply now".

## Where to go next

- The write loop, seek, position and processing in general: [the player engine](integration-player.md).
- Permission, lifecycle, settings and testing for any app: [app, service and settings](integration-app.md).
- Every option and its effect on bit-perfect output: [controls and configuration](controls.md).
- The outline this guide replaces: [Media3 integration](media3.md).
- A Rust app or engine that talks to the driver directly: [the Rust guide](guide-rust.md).
