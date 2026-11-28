# Getting started

Add the Android AAR and play your first audio through a USB DAC. The examples
come from the sample app in `android/sample`. For a Rust integration that owns
USB access, use [`usbdac-core` from crates.io](https://crates.io/crates/usbdac-core)
and follow the [native API guide](native-api.html#2-lifecycle).

The quickest way is to let the library choose the output. Add the library (step 1) and the manifest (step 2), then follow [the quick path](#the-quick-path-let-the-library-choose): one `OutputSelector`, one call, and the app has a ready `PcmOutput` with the reason for it and the guarantee that applies. Steps 3 to 10 do the same thing by hand, with every decision in the app's hands: finding the DAC, asking for permission, negotiating, opening the stream. They are what the selector does for you, and the way to go when an app needs control the selector does not give.

## 1. Add the library

Add `dev.usbdac:usbdac` from the project's GitHub Packages Maven registry. It
requires your GitHub username and a classic personal access token with
`read:packages`, including for public packages. Set `gpr.user` and `gpr.token`
in `~/.gradle/gradle.properties`, or supply `GITHUB_ACTOR` and `GITHUB_TOKEN`
in the build environment.

Add the Maven repository in `settings.gradle.kts`.

```gradle
dependencyResolutionManagement {
    repositories {
        google()
        mavenCentral()
        maven {
            url = uri("https://maven.pkg.github.com/johnneerdael/usbdac")
            credentials {
                username = providers.gradleProperty("gpr.user").orNull ?: System.getenv("GITHUB_ACTOR")
                password = providers.gradleProperty("gpr.token").orNull ?: System.getenv("GITHUB_TOKEN")
            }
        }
    }
}
```

Add the dependency in your app module's `build.gradle.kts`.

```gradle
dependencies {
    implementation("dev.usbdac:usbdac:0.1.15")
}
```

For ExoPlayer/Media3, add the optional integration at the same version. It brings the base SDK transitively; plain PCM apps need only `usbdac`.

```gradle
dependencies {
    implementation("dev.usbdac:usbdac-media3:0.1.15")
}
```

See [the Kotlin player guide](guide-kotlin.md) for the integer-precision sink, gapless trimming and session volume. The base AAR and sample app do not depend on Media3. When downloading AARs directly, include both core and integration AARs plus their declared Maven dependencies; a standalone AAR does not resolve dependencies automatically.

For a download without registry credentials, put `usbdac-0.1.15.aar`
from the [0.1.15 release](https://github.com/johnneerdael/android-usbdac/releases/tag/v0.1.15)
in your app module's `libs/` directory. Use these dependencies in place of
the registry coordinate.

```gradle
dependencies {
    implementation(files("libs/usbdac-0.1.15.aar"))
    implementation("org.jetbrains.kotlinx:kotlinx-coroutines-core:1.11.0")
}
```

If you build from source, run `./gradlew :usbdac:publishToMavenLocal` in
`android/`, add `mavenLocal()` to your repositories and depend on
`dev.usbdac:usbdac:0.1.15`. The Maven metadata brings in coroutines.

The library needs Android 8.0 (API 26) or later. An app that uses the published AAR needs no Rust and no NDK; [building from source](#building-from-source) does.

## 2. The manifest

The library's manifest already declares what it needs and merges into yours:

- `android.hardware.usb.host` with `required="false"`, so your app is not hidden from devices without USB host mode (the managed transport works there).
- `android.permission.MODIFY_AUDIO_SETTINGS`, which only the managed output uses.

USB permission is asked at run time, per device. If you want Android to offer your app when a DAC is plugged in (and remember the permission when the user chooses "always"), add an intent filter to your activity with a device filter, as the sample does:

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

Nothing else is needed. In particular the library does not need `RECORD_AUDIO` or a foreground service by itself; a player that continues in the background needs the usual media foreground service of its own.

## The quick path: let the library choose

`OutputSelector` detects what the device offers and returns one ready `PcmOutput`: direct USB when a DAC is attached and the app may take it, Android's bit-perfect mixer on Android 14 and later, an exclusive AAudio stream on a player with an internal DAC, or the standard output otherwise. [Automatic output](automatic-output.md) gives the exact order and what each result guarantees.

```kotlin
fun automaticPolicy(takeOverDac: Boolean, allowConversion: Boolean, allowFixedVolume: Boolean): OutputPolicy =
    OutputPolicy(
        allowDirectUsb = takeOverDac,
        allowConversion = allowConversion,
        dacConfig = DacConfig(requireHardwareVolume = !allowFixedVolume),
    )
```

```kotlin
fun openAutomatic(selector: OutputSelector, request: StreamRequest): SelectedOutput {
    val output = selector.open(request)
    // output.decision says what was chosen and why, output.format is what to write, and for the
    // direct output output.volume is the DAC's hardware volume.
    return output
}
```

`open` blocks (it looks at the DAC and the route before it decides), so call it off the main thread. What comes back is a `PcmOutput` like the others: the write loop, the transport calls and the position arithmetic of steps 6 and 7 work on it unchanged. `output.format` says what to write and `output.decision` says why this output was chosen. For the direct output `output.volume` is the DAC's hardware volume, and `output.session` is its `DacSession`, so step 8 works as written. If the app lacks USB permission for an attached DAC the selector does not ask; `decision.needsUsbPermission` names it, and the app asks and selects again ([permission](automatic-output.md#permission)).

The selector never moves a playing stream. At a track boundary the app looks again when `selector.changes` has fired or the next track's format differs ([when to look again](automatic-output.md#when-to-look-again)).

## 3. Find the DAC, ask for permission, open it

`UsbDac.attachedDacs` lists the USB devices that can play audio. `UsbDac.hotplug` is a flow of attach, detach and permission events. `requestPermission` shows the system dialog and calls back on the main thread. `UsbDac.open` blocks while it opens the device, takes the audio interfaces from Android's USB audio driver and reads the descriptors, so call it off the main thread.

```kotlin
dacs = UsbDac.attachedDacs(this)
```

```kotlin
// Plugging and unplugging: refresh the list.
UsbDac.hotplug(this).onEach { refreshDevices() }.launchIn(scope)
```

```kotlin
    private fun openSelectedDevice() {
        val dac = dacs.getOrNull(deviceSpinner.selectedItemPosition) ?: return
        if (session != null) return log("a session is already open")
        UsbDac.requestPermission(this, dac) { granted ->
            if (!granted) return@requestPermission log("USB permission denied")
            openDevice(dac)
        }
    }
```

```kotlin
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
```

Opening also protects the ears: a DAC keeps the level it had, and many default to full scale, so `UsbDac.open` lowers every channel of the DAC's hardware volume to at most -40 dB (`DacConfig.startupVolume`), never raising one, and verifies it by reading it back. The level the DAC had is recorded first and `close()` restores it. If the DAC has no usable hardware volume, or the attenuation cannot be verified, `session.volume` is `null` and `openStream` fails with `VOLUME_UNAVAILABLE` until the app opens the session again with `DacConfig(requireHardwareVolume = false)`: show what the DAC offers, ask the user to allow a fixed volume, and reopen (the sample does it with a checkbox). See [volume](controls.md#start-up-volume-protection).

Every failure is a `DacException` with a typed `code` (see [troubleshooting](troubleshooting.md)). A `DacSession` keeps the USB connection and the claimed interfaces until you call `close()`. The SDK also handles eligible IN interface claims for implicit feedback. Closing hands the DAC back to Android.

## 4. Read what the DAC can do

`session.capabilities` is read from the DAC's descriptors. Each alternate setting is one combination of channels, bit depth and rates. The sample builds its format list from the stereo PCM alternate settings:

```kotlin
    /** The (rate, bit depth) pairs the DAC's stereo PCM alternate settings offer. */
    private fun pcmFormats(capabilities: DacCapabilities): List<Pair<Int, Int>> =
        capabilities.altSettings
            .filter { it.format == AltFormat.PCM && it.channels == 2 }
            .flatMap { alt -> alt.sampleRates.map { rate -> rate to alt.validBits } }
            .distinct()
            .sortedWith(compareBy({ it.second }, { it.first }))
```

`capabilities.supportsDop`, `dopMaxDsdBitRate`, `supportsNativeDsd` and `nativeDsdBitRates` say whether and how far DSD works. `capabilities.volume` is `null` (and `capabilities.hasHardwareVolume` false) when the DAC has no usable hardware volume.

## 5. Negotiate and open a stream

`negotiate` settles the format on the wire: it programs the DAC's sampling frequency, reads it back and picks the alternate setting. `openStream` switches to that alternate setting, verifies the clock again and starts the USB output thread. Both take 100 ms or more. Only one stream can be open on a session at a time.

```kotlin
                // negotiate() programs the DAC's clock and reads it back; openStream() starts the output thread.
                val format = open.negotiate(StreamRequest.pcm(rate, channels = 2, bitDepth = bits))
                val opened = open.openStream(format)
```

The returned `StreamFormat` also says which clock the rate was set on (`format.clock`) and how the stream is paced (`format.pacing`); see [diagnostics](controls.md#diagnostics-the-clock-and-the-pacing).

`StreamRequest.pcm(rate, channels, bitDepth)` asks for exactly that. By default a rate the DAC cannot run fails with `UNSUPPORTED`; the library never resamples. With `RatePolicy.NEAREST_SUPPORTED` the returned `StreamFormat` carries the rate the DAC will run (`rateDiffersFromRequest`), and you have to resample. `StreamRequest.dop(...)` and `StreamRequest.dsdNative(...)` ask for DSD; see [bit-perfect output](bit-perfect.md).

### When the DAC refuses what you have

`RatePolicy.EXACT` fails with `UNSUPPORTED` for a rate the DAC cannot run, and the library never resamples. If the app must play it anyway, it can ask for the nearest rate and convert before writing. The conversion is opt-in, it sits in front of the stream, and **it is not bit-perfect**:

```kotlin
fun openConverting(session: DacSession, source: StreamRequest, wireBitDepth: Int = source.bitDepth): ConvertingOutput {
    val wire = source.copy(bitDepth = wireBitDepth, ratePolicy = RatePolicy.NEAREST_SUPPORTED)
    val format = session.negotiate(wire)
    val plan = ConversionPlan.between(source, format, ConversionOptions())
    return ConvertingOutput(session.openStream(format), PcmConverter.create(plan))
}
```

`negotiate` with `RatePolicy.NEAREST_SUPPORTED` returns the rate the DAC runs (`StreamFormat.rateDiffersFromRequest`). `ConversionPlan.between(source, format)` works out the smallest chain from what the host has to what the DAC runs, and says how much damage it does (`plan.kind`, `plan.description`): a changed rate is `LOSSY`, and so are fewer bits; mono to stereo is `LOSSLESS` but not identical; widening to more bits (the new low bits are zero) and moving a sample between slot sizes are `BIT_EXACT` and keep the stream bit-perfect. The `ConvertingOutput` is a `PcmOutput`: write the **source** format to it, and its `playedFrames`, `latencyFrames` and `writableBytes` are in source units, so the position arithmetic below is unchanged. Its `bitPerfect` is `NotVerified` with the plan's description whenever the plan is not `NONE` or `BIT_EXACT`. [Bit-perfect output](bit-perfect.md#conversion-is-not-bit-perfect) and [controls](controls.md#conversion-conversionoptions) have the details; This PCM helper does not accept DoP or native DSD; the selector has a separate, lossy DSD-to-PCM path with explicit consent to unpack DoP. Build the plan and the converter off the main thread (negotiating takes 100 ms or more, and a resampler synthesises its filter).

## 6. Write audio

`write` takes bytes in the **wire layout** of the stream's `StreamFormat`: whole frames of `bytesPerFrame` bytes, channels interleaved, little-endian, each sample left-justified in a slot of `subslotBytes` bytes. The layouts are tabled in [the native API document](native-api.html#4-sample-layouts); in short, 16-bit PCM is `i16`, 24-bit in a 3-byte slot is packed, and 24-bit in a 4-byte slot is the sample shifted left by 8.

The sample's `SinePlayer` renders a tone into that layout and runs the usual pull loop on its own thread. `write` never blocks: it takes whole frames, as many as fit, advances the buffer's position by what it took, and returns the count. `writableBytes` is how much the buffer wants now.

```kotlin
    private fun pump() {
        val frameBytes = channels * subslotBytes
        // A direct buffer is read in place by the library; a heap buffer would be copied.
        val chunk = ByteBuffer.allocateDirect(sampleRate / 100 * frameBytes).order(ByteOrder.LITTLE_ENDIAN)
        chunk.limit(0) // nothing rendered yet
        var phase = 0.0
        try {
            while (running) {
                if (!chunk.hasRemaining()) {
                    // The ring is full enough; wait instead of rendering audio nobody can take.
                    if (output.writableBytes < chunk.capacity()) {
                        Thread.sleep(2)
                        continue
                    }
                    phase = render(chunk, phase)
                }
                // write() never blocks and advances the position by the bytes it took.
                if (output.write(chunk) == 0) Thread.sleep(2)
            }
        } catch (error: DacException) {
            onFailure(error)
        } catch (_: InterruptedException) {
            // stop() joins; nothing to do.
        }
    }
```

How the samples are put into the layout:

```kotlin
        val fullScale = (1L shl (validBits - 1)) - 1
        val shift = subslotBytes * 8 - validBits // left-justify the sample in its slot
```

```kotlin
            val sample = (sin(phase) * AMPLITUDE * fullScale).roundToLong().toInt() shl shift
            repeat(channels) {
                for (byte in 0 until subslotBytes) chunk.put((sample shr (8 * byte)).toByte())
            }
```

Notes:

- A **direct** `ByteBuffer` is read in place. A heap buffer works too, but is copied through a staging buffer on every call.
- Use one writer thread. The bytes you write are one ordered sequence.
- The stream starts by itself once the buffer holds `DacConfig.startThresholdMs` of audio (100 ms by default). `play()` is only needed after `pause()`.
- If the stream fails (the DAC is unplugged, a transfer fails), `write` throws `DacException` with `INVALID_STATE`, and the stream stays open in state `FAILED` until you `close()` it.

## 7. Pause, resume, seek, stop

The buffer is deep (250 ms by default) so that a stall upstream does not become a gap. That is why the transport calls below are not all the same:

| Player action | Call |
|---|---|
| Pause | `pause()`. Keep the writer running; it fills the buffer to its target and idles when `writableBytes` is 0 |
| Resume | `play()` |
| Seek | Under the lock the writer uses: reposition the decoder, discard the chunk rendered for the old position, `flush()`, remember the new start time |
| Stop, or a track the user picked | `flush()`, then write the new audio |
| Next track in a gapless sequence | Keep writing. No `drain()`, no `flush()` |
| End of the last track | `drain()`, then wait for `isDrained` or the stream state `DRAINED` |
| Position | `startTime + playedFrames / counterRateHz`, with `startTime` set at each flush |
| Volume | `DacSession.volume` (hardware). Software gain only if you accept losing bit-perfect output |

`flush()` drops the ring and resets `playedFrames` to 0; by default it requests cancellation of submitted audio (`DacConfig.cancelInFlightOnFlush`). Cancellation is asynchronous, and downstream or already submitted audio affects audible timing. The cancelled audio is not lost audio and the flag stays bit-perfect. `pause()` sends silence so the DAC stays locked. The sample's buttons (the same calls drive the other two outputs, which the sample reaches through `other`):

```kotlin
        click(R.id.flushButton) {
            stream?.flush()
            other?.flush()
        }
```

```kotlin
        val current = stream ?: return
        // Keep the writer running: it fills the ring to its target and idles on writableBytes == 0.
        if (current.state.value == StreamState.PAUSED) current.play() else current.pause()
```

The same mapping as a small class, from `Snippets.kt`:

```kotlin
class Transport(private val output: PcmOutput, private val format: StreamFormat, private val source: Source) {
    interface Source {
        fun seekTo(positionUs: Long)

        /** Reads whole frames in the stream's wire layout, or returns false at the end of the audio. */
        fun read(into: ByteBuffer): Boolean
    }

    private val lock = Any()
    private var startUs = 0L

    /** Audible position: where the source was when the output was last flushed, plus what has played since. */
    fun positionUs(): Long = startUs + output.playedFrames * 1_000_000L / format.counterRateHz

    fun pause() = output.pause() // keep the writer running; it idles when the ring is full
    fun resume() = output.play()

    fun seek(positionUs: Long) = synchronized(lock) {
        source.seekTo(positionUs) // 1. reposition the decoder (and drop any chunk rendered for the old position)
        output.flush() //            2. drop what is buffered; playedFrames restarts at 0
        startUs = positionUs //      3. remember where "frame 0" is now
    }

    fun stop() = output.flush()

    /** The last track ended: let the buffered tail play, then wait for isDrained. */
    fun endOfAudio() = output.drain()
}
```

### Other outputs

Everything above is written against `PcmOutput`, so the same writer, transport and position code runs on the other two outputs: `AndroidManagedOutput` (an `AudioTrack`) and `AAudioExclusiveOutput` (an exclusive AAudio stream, the route for a player with an internal DAC). They need no `DacSession`: open one, write the PCM in the encoding of the request, and control it as above. [Automatic output](automatic-output.md) chooses between them and the direct stream for you. The sample's output spinner starts on Automatic and has the others as a manual override; [transports](transports.md) says what differs.

## 8. Volume

A direct stream is not touched by Android's volume. Use the DAC's hardware volume, `session.volume` (with automatic output, `output.volume` or `output.session.volume`), which is `null` when the DAC has none (or its start-up attenuation could not be verified). Its setters return at once and are safe on every slider event: writes are queued, a burst collapses to its last value, and the last value is always applied.

```kotlin
        volumeBar.setOnSeekBarChangeListener(object : SeekBar.OnSeekBarChangeListener {
            override fun onProgressChanged(bar: SeekBar, progress: Int, fromUser: Boolean) {
                if (fromUser) session?.volume?.setFraction(progress / 1000.0)
            }
```

`volume.startup` says what the library did to the level when the session opened: the level each channel had (`originalDb`), the level it has now (`appliedDb`) and `limitDb`, the quietest original channel. A player that remembers a level per DAC restores it on connect, capped by `limitDb`; the sample shows the report:

```kotlin
        startupText.text = volume?.startup?.let { startup ->
            "Start-up: %s dB, lowered to %s dB. A remembered level should stay at or below %.1f dB.".format(
                startup.originalDb.joinToString(" / "),
                startup.appliedDb.joinToString(" / "),
                startup.limitDb,
            )
        } ?: "No usable hardware volume: tick \"Allow a fixed volume\" to stream at the DAC's own level."
```

`volume.state` is a `StateFlow` of the level in dB, the 0..1 position and the mute state. Route the phone's volume keys to the DAC while an activity is in front:

```kotlin
    override fun dispatchKeyEvent(event: KeyEvent): Boolean =
        DacVolumeKeys.handle(event, session?.volume) || super.dispatchKeyEvent(event)
```

To make the system volume panel and the background volume keys drive the DAC through a `MediaSession`, see [controls](controls.md#the-system-volume-panel-and-background-keys).

## 9. Close

```kotlin
        // close() stops the stream, restores the DAC volume, and gives the DAC back to Android.
        io.execute {
            runCatching { closing.close() }
            retainFeedbackReport(closing)
            runOnUiThread { log("session closed") }
        }
```

`DacStream.close()` releases only the stream. The session, the volume and the claim stay, and you can open another stream. `DacSession.close()` also hands the volume back (by default it writes back the level the DAC had before the library first changed it), releases the interfaces, asks Android to rebind its USB audio driver, and closes the USB connection. Closing takes about 100 ms or more.

## 10. Threading

| Call | Blocks? | Thread |
|---|---|---|
| `UsbDac.open`, `DacSession.negotiate`, `openStream`, `close`, `DacStream.close` | Yes, 100 ms or more | Not the main thread |
| `DacVolume.refresh`, `handBack` | Yes, a USB control transfer | Not the main thread |
| `DacVolume.setDb`, `setFraction`, `step`, `setMute` | No, queued | Any |
| `DacStream.write`, `writableBytes`, `play`, `pause`, `flush`, `drain`, `playedFrames`, `latencyFrames` | No | Any; one writer |
| `DacStream.stats` | Short lock | A few times a second, not per buffer |
| `session.events`, `session.state`, `stream.state`, `stream.bitPerfect`, `volume.state` | Flows | Updated from library threads, never from the audio path; collect on the dispatcher you want |

The library runs two threads of its own: `usbdac-output` (the isochronous loop, with real-time scheduling by default) and `usbdac-events`. It never calls into your code from the output thread. Do not call `DacSession.close` from a collector of `events`.

## Building from source

The Gradle project is in `android/`. Building the library compiles the Rust driver for each ABI with `cargo ndk`, before anything else (the task `buildUsbdacNative`, wired before `preBuild`). It needs:

- JDK 17 or later, and an Android SDK (`ANDROID_HOME`) with platform 36 and build tools. Gradle installs missing pieces it has a licence for.
- NDK 28.2.13676358 (the version Gradle is pinned to).
- Rust with the four Android targets: `rustup target add aarch64-linux-android armv7-linux-androideabi x86_64-linux-android i686-linux-android`.
- `cargo-ndk` 4.1.2: `cargo install cargo-ndk --version 4.1.2 --locked`.

```sh
cd android
# the AAR, the JVM unit tests and the sample app; -Pusbdac.abis restricts the native build for speed
./gradlew :usbdac:assembleRelease :usbdac:testDebugUnitTest :sample:assembleDebug -Pusbdac.abis=arm64-v8a

# the AAR with all four ABIs, into ~/.m2 for use with mavenLocal()
./gradlew :usbdac:publishToMavenLocal

# the API reference (Dokka HTML, in usbdac/build/dokka/html); -Pusbdac.skipNative=true leaves the Rust build out
./gradlew :usbdac:dokkaGeneratePublicationHtml
```

The native libraries are build output (`usbdac/src/main/jniLibs`, ignored by git). An AAR built with `-Pusbdac.abis` or `-Pusbdac.skipNative=true` lacks ABIs or the library altogether: do not publish it.

## Migrating 0.1.10 callers

`OutputPolicy.allowConversion` defaults to `true`; set it to `false` to disallow lossy PCM conversion; lossless widening remains separately enabled by default and can be disabled with `allowWidening = false`. DoP-to-PCM requires both conversion and the separate `allowDopPcmConversion` opt-in, which defaults to `false`. Review application policy explicitly when upgrading. Kotlin `OutputSelector.PcmSupport` includes `UNKNOWN`: handle it as permission to attempt opening, not a guarantee of direct support. Custom Media3 `OutputSource` implementations that do not override `formatSupport` return an unknown verdict that permits an open attempt.
