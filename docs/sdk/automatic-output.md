# Automatic output

An app can send audio to a DAC in four ways: directly over USB, through Android's bit-perfect mixer, through an exclusive AAudio stream, or through Android's ordinary output. Which one is best depends on the phone, the Android version, what is plugged in, where Android sends the sound and what the listener allows. `OutputSelector` looks at all of that and hands the app one ready `PcmOutput`, with the reason for the choice and the guarantee that applies.

Most apps should start here. Everything the other pages describe (the stream, the volume, the controls) works on the output you get; this page is about getting it.

The direct USB playback API also exists independently of Android route selection. A Rust host can ask `Dac::resolve_playback` for a `PlaybackDecision` under a `playback::PlaybackPolicy` and open a source-domain `PlaybackStream` with `Dac::open_playback`. Its write input is the original source layout; the decision describes the USB target and any exact, sample-preserving or lossy transform. Strict `Dac::negotiate` and `Dac::open_stream` remain available. Target USB verification is separate from source fidelity, and does not prove DoP decoding or analog output. The driver-owned automatic path requires separate hardware acceptance; results for the existing exact path do not establish it. See [the Rust guide](guide-rust.md#automatic-direct-usb-playback).

## The short version

Build a selector once, with the policy the listener's settings give you:

```kotlin
fun automaticPolicy(takeOverDac: Boolean, allowConversion: Boolean, allowFixedVolume: Boolean): OutputPolicy =
    OutputPolicy(
        allowDirectUsb = takeOverDac,
        allowConversion = allowConversion,
        dacConfig = DacConfig(requireHardwareVolume = !allowFixedVolume),
    )
```

Then open an output for the track about to play:

```kotlin
fun openAutomatic(selector: OutputSelector, request: StreamRequest): SelectedOutput {
    val output = selector.open(request)
    // output.decision says what was chosen and why, output.format is what to write, and for the
    // direct output output.volume is the DAC's hardware volume.
    return output
}
```

`open` blocks while it looks (see [what it costs](#what-it-costs)), so call it off the main thread. The `SelectedOutput` is a `PcmOutput`: the writer, the transport calls and the position arithmetic of [getting started](getting-started.md#6-write-audio) work on it unchanged. Two things differ between the outputs, and the result tells you both:

- **What to write.** `output.format` is the layout of the frames: the DAC's own wire layout for a direct stream (a 24-bit sample may sit in a 3-byte or a 4-byte slot), and the request's layout for every other output, 16-bit in 2 bytes, 24-bit packed in 3, 32-bit in 4. `format.counterRateHz` is the rate of `playedFrames` and `latencyFrames`. The sample hands both to its writer:

```kotlin
                val layout = (output as? SelectedOutput)?.format ?: WriteFormat(rate, 2, bits / 8, bits, rate)
                val sine = SinePlayer(output, layout.sampleRate, layout.channels, layout.subslotBytes, layout.validBits) {
```

- **What it means.** `output.decision` is the `OutputDecision`: the `kind`, the `guarantee`, a sentence in `reason`, the route and the DAC it was judged against, the `conversion` if there is one, and `alternativesRejected`, the better outputs that were passed over and why.

`output.close()` closes everything that was opened for it, the DAC session included, and hands the DAC back to Android.

## What it looks at

| What | How |
|---|---|
| USB DACs that are attached, and whether the app holds permission for each | `UsbDac.attachedDacs`, `UsbDac.hasPermission` |
| Where Android sends media right now | From Android 13, `AudioManager.getAudioDevicesForAttributes` for music. Before that Android cannot say, so the library guesses from the connected outputs: a USB device first, then a wired one, a wireless one, anything else, the speaker. When a USB DAC and Bluetooth headphones are both connected the guess can be wrong |
| The Android version | `Build.VERSION.SDK_INT`: the bit-perfect mixer needs 34, 24-bit and 32-bit integer samples need 31 |
| Whether the DAC can carry the request | By trying: a session is opened and the format negotiated |
| Whether the platform offers its bit-perfect mixer for the request on the USB route | `AudioManager.getSupportedMixerAttributes`, asked only |
| What the platform grants an exclusive AAudio stream at the request's rate and format | By trying: a stream is opened and its grant read back, and not started |

A player with an internal DAC is recognised by that last row: by what the platform grants, not by a list of device names.

## The order

The first output in this list that works for the request is chosen.

1. **Direct USB** (`DIRECT_USB`), when all of these hold:
   - the policy allows it (`allowDirectUsb`);
   - audio is not routed to Bluetooth or a cast device;
   - a USB DAC is attached and the app holds permission for it;
   - the DAC opens, has a usable hardware volume (unless `DacConfig.requireHardwareVolume` is off) and negotiates the request.

   With `allowConversion`, a request the DAC cannot run as it is goes through a `ConversionPlan` to a ranked supported format. The decision then says so, and the guarantee drops (see [conversion](#conversion)). If the requested asynchronous route has no usable feedback, the DAC lacks the required hardware volume, another session holds the DAC, or open or negotiation fails, the selection records the typed reason and moves to the next output.
2. **Android's bit-perfect mixer** (`ANDROID_BIT_PERFECT`), when audio is routed to a USB device, the Android version is 14 or later, and the platform offers its bit-perfect mixer behavior for exactly the request's format on that device. After opening, the library checks that the platform kept the request, and falls through if it did not.
3. **Exclusive AAudio** (`AAUDIO_EXCLUSIVE`), when audio is not routed to Bluetooth or a cast device and the platform grants an exclusive stream at exactly the request's rate, channel count and sample format, with the hardware agreeing (Android 14 and later report it). The stream is opened to read the grant and not started, so nothing is heard and nothing is converted; `open` keeps that very stream. When audio is routed to a USB device this step is tried only if `allowDirectUsb` is on, because an exclusive stream keeps the DAC from other apps.
4. **Standard output** (`ANDROID_STANDARD`), an ordinary `AudioTrack`, for PCM that Android can carry. A Bluetooth or cast route goes straight here, and the decision says so.

Raw DSD and DoP require a direct bitstream path. When policy permits, eligible DSD can be converted to PCM for the direct output or any of the three Android outputs; explicit DoP input also needs `allowDopPcmConversion`. For Android outputs on API 31 and later, the selector tries 24-, 32- and then 16-bit integer PCM targets at each standard rate ranked lower-first from the DSD bit rate over 16; earlier Android versions try 16-bit. Exclusive AAudio configured with `AAudioConfig.floatSamples` instead tries 32-bit float. Each backend must actually support and accept the candidate, so the selected precision depends on the route and device. If no output can carry the request, `select` and `open` throw a `DacException` with code `UNSUPPORTED` that lists every rejection.

Some examples of what that gives, on devices as they are usually met:

| Situation | Chosen |
|---|---|
| Phone with a USB DAC, the listener allows taking over the DAC, permission held | Direct USB |
| The same, permission not given yet | The next best (bit-perfect mixer on Android 14, otherwise AAudio or standard), and `needsUsbPermission` names the DAC |
| The same, the listener does not allow taking over the DAC | Bit-perfect mixer on Android 14 and later; otherwise the standard output |
| A digital audio player with an internal DAC and nothing plugged in | Exclusive AAudio, if the platform grants it; otherwise standard |
| A DAC with a compatible implicit feedback source | Direct USB if the other requirements hold |
| An asynchronous DAC without usable feedback on Android 13 | Exclusive AAudio if the platform grants it on that route, otherwise standard |
| Bluetooth headphones connected | Standard output, whatever else is plugged in |

These are what the order produces. None of it has been confirmed on devices; see [what is not verified](#what-is-not-verified).

## What each guarantee means

`decision.guarantee` says who vouches for the samples. It is not a live verdict: the output's own `bitPerfect` flow is that, and it can still say no.

| `Guarantee` | In plain terms | `bitPerfect` shows |
|---|---|---|
| `VERIFIED_BY_LIBRARY` | The library drives the DAC itself and checks the stream: the DAC confirmed its clock, nothing was lost, the packets are what was written. This is the only guarantee the library makes on its own account | `Verified`, or `NotVerified` with the reason when a check fails |
| `REQUESTED_FROM_PLATFORM` | Android accepted a request for its bit-perfect mixer for this DAC and format. Android enforces it; the library cannot see inside | `RequestedViaPlatform` |
| `EXCLUSIVE_GRANTED_UNVERIFIABLE` | The platform granted an exclusive stream at exactly this rate and format, so the app mixer is bypassed. What the audio path does after that cannot be seen from an app | `NotVerified` with the grant's reason |
| `NONE` | No promise. The standard output, a shared AAudio stream, a stream that is converted first, or a direct stream paced without feedback | `Unverifiable`, or `NotVerified` with the reason |

The decision is made before the first sample plays, from what the library can know then. Read `bitPerfect` while playing for what is true now.

## One setting has to be the listener's

`allowDirectUsb` is the one decision the library cannot make for the app, and the app should not make silently for the listener. Direct USB takes the DAC away from Android: while it plays, system sounds, other apps and the system volume do not reach it. For a listener who wants only the music, that is what bit-perfect playback is. For one who needs a navigation prompt or a call to come through the same DAC, it is a loss.

So a player should offer it as a setting, worded as the listener would say it ("let this app take over the DAC"), and pass it as `allowDirectUsb`. With it off, the best a USB DAC gets is the platform's bit-perfect mixer on Android 14 and later, which other apps share, and which the library cannot verify. The sample has it as a checkbox.

## Permission

The selector never shows a dialog. When a DAC is attached but the app lacks its USB permission, the decision is the best choice without it, and `needsUsbPermission` names the DAC. Ask, and select again:

```kotlin
    /** The selector never asks for USB permission by itself: the app does, then selects again. */
    private fun askForUsbPermission() {
        val dac = waitingForPermission ?: return log("no DAC is waiting for USB permission")
        UsbDac.requestPermission(this, dac) { granted ->
            if (granted) refreshDecision() else log("USB permission denied")
        }
    }
```

The intent filter in [getting started](getting-started.md#2-the-manifest) makes Android offer the app when a DAC is plugged in and remember the permission, which avoids the dialog on later connections.

## Conversion

**Default behavior since 0.1.10:** automatic output permits PCM conversion and bit-exact widening by default. Unpacking DoP input for PCM conversion is off by default since 0.1.11 (`allowDopPcmConversion`); [upgrading from 0.1.10](migration.md) lists the changes. A player must show `decision.source`, `decision.wireFormat` and `decision.processing` in its signal path. Resampling applies -3.5 dB headroom by default; the selector uses unity when the target equals the DSD intermediate PCM rate. This default covers a measured PCM square-wave fixture, not every signal; `output.clippedSamples` counts post-dither integer-quantizer saturation, including rounding or dither clamps without resampling. Direct `DacSession` negotiation remains exact by default.

By default the direct output may convert when a DAC cannot run the request exactly, using the library's own converter. The decision carries the `ConversionPlan`; set `allowConversion = false` to refuse lossy conversion and `allowWidening = false` to refuse widening:

- Exact first. The same rate at another depth is next, then the highest available integer divisor, the highest lower same-family rate, the highest other lower rate, and finally the lowest higher rate.
- A **widening** (16-bit to 24-bit, 24 in 3 bytes to 24 in 4) keeps every sample value: the guarantee stays `VERIFIED_BY_LIBRARY`.
- Anything else changes the audio (a new rate, fewer bits, mono to stereo): the guarantee is `NONE` and the output's `bitPerfect` says what was done. [Bit-perfect output](bit-perfect.md#conversion-is-not-bit-perfect) has the classification.
- Native DSD can be framed as DoP only when the downstream decoder is confirmed to support it. Native DSD can be converted to PCM when `allowConversion` permits it; unpacking explicit DoP additionally requires `allowDopPcmConversion = true` and valid markers. Both conversions are reported as lossy. See [DSD conversion](decisions-dsd-conversion.md).

The converting output takes the **source** layout and reports position in source units. `decision.wireFormat` describes a negotiated direct USB target when known; Android platform output does not expose its downstream wire format. DSD-to-PCM accepts DSD64, DSD128 and DSD256 with one to eight channels. The host owns downstream limits and the process-local `DownstreamBinding` association. With `DownstreamLimits` set, only direct USB and exclusive AAudio on a bound built-in route are eligible, so without a binding an unplugged DAC leaves no output ([downstream limits](guide-kotlin.md#automatic-conversion-and-downstream-limits)). Closing or invalidating its bound endpoint retires streams that depend on it; route/attachment changes require re-selection and renewed proof. No whole-service or physical-route acceptance follows from host tests. See [DSD conversion](decisions-dsd-conversion.md).

## When to look again

The selector does not move a stream that is playing. It cannot: the DAC and an exclusive stream can only be held once, so while an output it opened is still open, `select` and `open` fail with `INVALID_STATE`. The app decides when to look again, and the right moment is the next track boundary.

`selector.changes` tells the app that the best choice may have changed: a DAC attached, detached or given its permission, or an output device that appeared or went away (an `AudioDeviceCallback`). One plug raises several, so debounce them:

```kotlin
@OptIn(FlowPreview::class)
fun reselectOnChange(scope: CoroutineScope, selector: OutputSelector, onChange: (OutputChange) -> Unit): Job =
    selector.changes
        .debounce(500)
        .onEach(onChange)
        .launchIn(scope)
```

Remember that something changed, and at the next boundary keep the output when nothing did and the next track has the same format, otherwise close it and open again:

```kotlin
fun sameFormat(output: SelectedOutput, request: StreamRequest): Boolean =
    output.format.sampleRate == request.sampleRate &&
        output.format.channels == request.channels &&
        output.format.validBits == request.bitDepth
```

```kotlin
fun outputForNextTrack(selector: OutputSelector, current: SelectedOutput?, next: StreamRequest, changed: Boolean): SelectedOutput {
    if (current != null && !changed && sameFormat(current, next)) return current
    current?.close()
    return selector.open(next)
}
```

A gapless sequence of tracks in one format keeps writing to the same output; the gap only appears when the output changes, which is when the format changes or the device did.

## Route handoff and USB reconnect

Use `OutputHandoff(selector)` when a player needs to coordinate track changes
and USB reconnect. Call its suspending `open` after the last write to the
outgoing output. It drains that output, closes the old session and opens the
next one. Pass `drainTimeoutMs = null` to cut immediately on a user-selected
track change. For gapless tracks at the same format, keep the output and
continue writing.

Observe `handoff.state`. When `Ready` offers a new output, prepare the source at
`positionUs` and write using that output's `format`. Send user play and pause commands to
`setPlaying`, and call `seekTo` after repositioning the source under the
player's writer lock. Close the helper with the player.

```kotlin
fun observeHandoff(scope: CoroutineScope, handoff: OutputHandoff, onFailure: (DacException) -> Unit): Job =
    handoff.changes
        .onEach { change ->
            try {
                handoff.handleChange(change)
            } catch (error: DacException) {
                onFailure(error)
            }
        }
        .launchIn(scope)
```

Feed attach and permission events in order. On native `DeviceLost`, call
`onDeviceLost` with the output that raised it. `WaitingForUsb` holds the
last reported media position. The helper automatically reopens only a
uniquely identified USB device with the same VID, PID and serial, with USB
permission and play intent. For a DAC without a readable serial, ask the
listener to confirm the device and call `reconnectTo`. The player handles
permission prompts.

Use `fallbackAfterFailure` instead of `onDeviceLost` when the player has
chosen a different output. A failed fallback leaves the direct USB reconnect
position available. An Android bit-perfect fallback needs an active USB
route. Switching from direct USB to a route that could play on the phone
requires explicit `allowSystemRoute`.

## What it costs

`select` and `open` block, and they look before they decide:

- With a DAC attached they open a session and negotiate. That takes 100 ms or more and **programs the DAC's clock**; `select` then closes the session again and hands the DAC back, as `close` does.
- Where an exclusive stream may be possible they open an AAudio stream to read the grant. It makes no sound. `select` closes it again (up to half a second); `open` keeps it and starts it.
- Everything else is a lookup.

So use `select` when nothing plays, to show the user what will happen (a settings screen, a "now playing on" label), and `open` to play. Do not call either on the main thread.

## Reading the decision

The sample shows all of it:

```kotlin
fun describeDecision(decision: OutputDecision): String = buildString {
    appendLine("${decision.kind} (${decision.guarantee})")
    appendLine(decision.reason)
    decision.conversion?.let { appendLine("Conversion: ${it.description}") }
    // The selector never asks by itself; the app does, then selects again.
    decision.needsUsbPermission?.let { appendLine("${it.productName} needs USB permission to be used directly.") }
    decision.alternativesRejected.forEach { appendLine("Not ${it.kind}: ${it.why}") }
}.trimEnd()
```

`alternativesRejected` lists the outputs tried before the chosen one, best first. Each `Rejection` has the `kind`, a `reason` to branch on, a sentence in `why`, and `error`, the `DacErrorCode` that forcing that output would fail with.

| `RejectionReason` | Means |
|---|---|
| `POLICY` | `allowDirectUsb` is off |
| `NOT_AVAILABLE` | Nothing to use: no DAC attached, no USB route |
| `PERMISSION_MISSING` | The app has no USB permission for the DAC |
| `PLATFORM_TOO_OLD` | This Android release cannot do it |
| `ROUTE_UNSUITABLE` | Audio goes to Bluetooth or a cast device |
| `REQUEST_NOT_SUPPORTED` | The output cannot carry this request: DSD on a mixer, a rate or depth the DAC does not run |
| `IMPLICIT_FEEDBACK` | The requested asynchronous route has no compatible feedback source |
| `NO_HARDWARE_VOLUME` | The DAC has no usable hardware volume and `requireHardwareVolume` is on |
| `BUSY` | Another session or app holds the device |
| `EXCLUSIVE_NOT_GRANTED` | The platform granted a shared AAudio stream |
| `NOT_AS_REQUESTED` | Exclusive, but the hardware runs another rate or format |
| `FAILED` | Opening or negotiating failed; `error` says how |

## Forcing an output

`OutputPolicy(force = OutputKind.X)` skips the detection and uses exactly that output. If it cannot be opened, `select` and `open` throw a `DacException` with the typed code (`PERMISSION_DENIED`, `IMPLICIT_FEEDBACK_UNSUPPORTED`, `EXCLUSIVE_UNAVAILABLE`, ...), and no other output is used in its place. It is for a "choose the output yourself" setting, or for a bug report. `force = DIRECT_USB` together with `allowDirectUsb = false` contradicts itself and throws `IllegalArgumentException`.

`requireExclusiveForAAudio` (default on) decides whether a shared AAudio grant is accepted; off, it is used with `Guarantee.NONE`. It takes the place of `AAudioConfig.requireExclusive` inside the selector.

## What is not verified

The decision table, every branch above, is covered by JVM tests against a fake device and a fake native layer. Nothing has run on hardware, and these are the places it is most likely to differ:

- **How real players answer the AAudio probe.** Whether a given digital audio player grants an exclusive stream, at which rates and formats, and whether the open and close take as long as assumed, is not known yet.
- **The route before Android 13** is a guess (see above).
- **The Android 14 bit-perfect mixer** is asked for the exact format of the request. Which DACs and which formats the platform offers it for is up to Android.
- **Taking a DAC for a look and giving it back** (`select` with a DAC attached) has been tested only against a fake session. On a real DAC it reprograms the clock and rebinds Android's driver, which can take longer than the numbers above.
