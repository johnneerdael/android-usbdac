# Integration guide

How to add bit-perfect USB output to an audio app with usbdac. This page is the map: what you are taking on, the ways to integrate, the order of the work, and the mistakes that are easy to make. The two pages after it go deep: [the player engine](integration-player.md) (decoding, the write loop, seek, gapless, processing) and [app, service and settings](integration-app.md) (permission, lifecycle, volume interface, failures, release).

The [getting started](getting-started.md) page is the tutorial for the first hour: dependency, manifest, a working stream. This guide is for the days after: making it part of a real player.

**Status.** usbdac is a 0.1 pre-release. Its Rust driver has played PCM on one phone and one DAC; the Kotlin SDK, the selector, the managed and AAudio outputs and everything in this guide have been tested without hardware only. Plan your own device testing ([the test plan](integration-app.md#testing-your-integration)), and read [the evidence](evidence.md#hardware-status) before promising anything to listeners.

## What you are taking on

usbdac drives the DAC. Your app still owns these:

| Your app's job | Why the library cannot do it |
|---|---|
| Decoding to PCM at the source's own rate and depth, with no processing in between | The library sees bytes, not music. It cannot know that you applied a gain or converted from float |
| The writer thread: keep the ring near its target | The ring is a buffer, not a clock. Audio only moves if someone writes it |
| Mapping the player's transport (pause, seek, skip, end of queue) onto `play`, `pause`, `flush`, `drain` | Only the player knows what a seek means |
| The settings the listener must own: take over the DAC, allow conversion, allow a fixed volume | They are choices about the listener's other apps and ears |
| USB permission, and what to show while it is missing | Android asks the app, with the app's context |
| The volume interface | The library gives the DAC's control, and the keys and panel helpers; where they go in the UI is yours |
| What to do when the output fails | Falling back to the speaker is a policy, and some listeners would rather hear nothing |
| Not claiming more than happened | The library's flag covers the transport. If you change the samples before `write`, you are the only one who knows |

## Three ways to integrate

| Approach | You call | You get | Choose it when |
|---|---|---|---|
| **The selector** | `OutputSelector.open(request)` | One `PcmOutput` chosen from direct USB, Android's bit-perfect mixer, exclusive AAudio and the standard output, with the reason and the guarantee | You want one code path for phones with a DAC dongle, digital audio players and Bluetooth. This is the default recommendation |
| **Direct USB only** | `UsbDac.open`, `session.negotiate`, `session.openStream` | A `DacStream`, `DacVolume` and every diagnostic | Your product is a DAC player and a missing DAC is an error to explain, not a case to handle |
| **The Rust crate** | `usbdac_core::Dac` | The same driver without the Android layer | Your app is native or cross-platform and does its own permission and claim (the README explains the order). Everything on this page about the writer and the transport applies |

Whichever you pick, your player writes to something that is a `PcmOutput`. Code against that interface, and the same writer, transport and position code works on every output.

Apps built with Flutter, React Native or another cross-platform toolkit should keep the writer loop in Kotlin or native code and send commands (play, pause, seek, volume) across the bridge, not audio. Audio bytes written from another runtime need a direct `ByteBuffer` to avoid a copy per call, and a bridge adds latency to the thing that has to be on time. This is guidance from the structure of the API, and has not been tried.

## The order of the work

1. **Add the library and the manifest** ([getting started, steps 1 and 2](getting-started.md#1-add-the-library)). Nothing else in the manifest is needed; there is no `RECORD_AUDIO` and no foreground service of the library's own.
2. **Decide what the player will tell the listener**: an output line ("Direct USB, 24-bit 96 kHz, bit-perfect"), a reason when it is not, and three settings. Draw it before you build it, because the library's states decide what can be shown ([the status display](integration-app.md#the-signal-path-display)).
3. **Make the engine able to hand over unprocessed integer PCM** at the source's rate and depth, and to say when it is not doing that ([the player engine](integration-player.md#what-the-engine-must-hand-over)). If the engine decodes to float or always resamples to 48 kHz, this is the largest piece of work.
4. **Open the output at the start of a track**, on a background thread, with `OutputSelector.open` (or the direct calls), and keep the result for the track ([automatic output](automatic-output.md)).
5. **Write the pull loop** and the transport mapping ([the player engine](integration-player.md#the-writer)).
6. **Wire the position** from `playedFrames` and the anchor at the last flush ([position](integration-player.md#position)).
7. **Add the volume interface**: the DAC's slider, the keys, the system panel if you have a media session ([volume](integration-app.md#volume)).
8. **Handle the lifecycle**: permission and hot-plug, one session per process, audio focus, a foreground media service, unplug ([app, service and settings](integration-app.md)).
9. **Handle failure** with a rule for each error code ([failures and fallback](integration-app.md#failures-and-fallback)).
10. **Test on hardware** with the matrix in [testing your integration](integration-app.md#testing-your-integration), and send what you find upstream.

## The smallest complete integration

The pieces of a first integration, as an outline. It is not compiled; every call in it is from the API documented in [getting started](getting-started.md) and [automatic output](automatic-output.md), where compiled versions of each piece are quoted from the sample.

<!-- uncompiled -->

```kotlin
/** Owns the player's current output. Every method that opens or closes blocks: call it off the main thread. */
class DirectOutputController(private val context: Context) {
    private var selector: OutputSelector? = null
    private var current: SelectedOutput? = null
    private var lookAgain = false                  // set when OutputSelector.changes fired (debounced)

    /** At the start of each track, and again when the next track's format differs. */
    fun outputFor(request: StreamRequest, settings: PlayerSettings): SelectedOutput {
        val policy = OutputPolicy(
            allowDirectUsb = settings.takeOverDac,
            allowConversion = settings.allowConversion,
            dacConfig = DacConfig(requireHardwareVolume = !settings.allowFixedVolume),
        )
        current?.let { open ->
            // sameFormat is the function in the automatic output guide.
            if (!lookAgain && selector?.policy == policy && sameFormat(open, request)) return open
            open.close()                            // the DAC and an exclusive stream can only be held once
            current = null
        }
        val chosen = selector?.takeIf { it.policy == policy } ?: OutputSelector(context, policy).also { selector = it }
        lookAgain = false
        return chosen.open(request).also { current = it }
    }

    fun onSelectorChanged() { lookAgain = true }   // from selector.changes
    fun close() { current?.close(); current = null }
}
```

Three details in it are worth copying. The output is kept while nothing changed and the next track has the same format, because reopening costs 100 ms or more and puts a gap between gapless tracks. A changed policy builds a new selector, because a selector's policy is fixed. And the old output is closed before the next opens, because the library will refuse a second open.

## What not to do

- **Do not apply software volume, ReplayGain, an equaliser, crossfade or speed change and still show "bit-perfect".** The flag covers the transport. Either bypass your processing chain in direct mode (Mornye does, and says so in its README) or show something else.
- **Do not decode to float.** A decoder that outputs float and converts to integer has changed the samples before `write`. Ask the decoder for the source's integer depth. If a platform decoder gives you less than the file has (Mornye notes that some vendor extractors present 24-bit FLAC as 16-bit decoded audio), decode in software.
- **Do not write from two threads**, or write on the main thread. Writes never block, but the bytes are one ordered sequence.
- **Do not call `negotiate`, `openStream`, `close`, `OutputSelector.open` or `select` on the main thread.** They take 100 ms or more.
- **Do not call `select` or `open` while an output is playing.** The selector cannot move a playing stream; it will fail with `INVALID_STATE`.
- **Do not close the session from inside an `events` collector.**
- **Do not treat the selector's decision as a live verdict.** `decision.guarantee` is made before the first sample. Read the output's `bitPerfect` flow for what is true now.
- **Do not hide a failure by playing on the speaker.** A listener who chose a DAC and hears the phone speaker will not forgive it. Pause and say why ([failures and fallback](integration-app.md#failures-and-fallback)).

## Where to go next

- The write loop, seek, gapless, processing, DSD and position: [the player engine](integration-player.md).
- Permission, the media service, audio focus, volume interface, status display, errors, release: [app, service and settings](integration-app.md).
- Media3 apps: [Media3 integration](media3.md) is an outline of an `AudioSink` over `PcmOutput`.
- Every option and its effect on the flag: [controls and configuration](controls.md).
- Why the library behaves as it does: [design decisions](decisions.md).
