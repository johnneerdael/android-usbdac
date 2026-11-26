# App, service and settings

The parts of an integration that are not the audio loop: permission and hot-plug, who owns the session, what the listener can set, the volume interface, what to show about the signal path, what to do when something fails, and how to test and release. [The integration guide](integration.md) is the overview; [the player engine](integration-player.md) covers decoding and the write loop. The Kotlin outlines on this page are not compiled; each is marked, and uses only documented calls.

## Permission and hot-plug {#permission-and-hot-plug}

USB permission is per device and asked at run time. The library never shows a dialog itself, and neither does the output selector.

- **Ask from a foreground app**, with `UsbDac.requestPermission(context, dac) { granted -> ... }`. The callback runs on the main thread, and with `true` at once if the permission is already held. Android may not show the dialog from the background.
- **Make Android offer your app when a DAC is plugged in.** The `USB_DEVICE_ATTACHED` intent filter and device filter in [getting started](getting-started.md#2-the-manifest) make Android ask "open with this app?", and if the listener picks "always", the permission is remembered for that device and app.
- **Watch the DACs coming and going** with `UsbDac.hotplug(context)`, a cold flow of `Attached`, `Detached` and `PermissionGranted`, delivered on the main thread. For an app that uses the selector, `selector.changes` carries the same news plus route changes, and one plug raises several, so debounce it ([when to look again](automatic-output.md#when-to-look-again)).
- **With the selector, a missing permission is not an error.** The decision is the best output without it, and `decision.needsUsbPermission` names the DAC. Show a line that says so, with a button that asks, and select again when it is granted.

## Who owns the session {#lifecycle}

One `DacSession` can be open in a process: a second `UsbDac.open` throws `BUSY`. The session holds the USB connection and the claimed interfaces, which keeps Android's own audio stack away from the DAC, until `close()`.

- **Let the playback service own it, not an activity.** The activity goes away when the listener swipes the app off the recents list and the music should not. Mornye's README says the same of its engine: "the engine, not the Activity, owns the connection".
- **Open the output at the start of a track and keep it across tracks** that have the same format. Closing and opening costs 100 ms or more, programs the DAC's clock again, and puts a gap between gapless tracks.
- **Close it when playback ends for good.** `SelectedOutput.close()` or `DacSession.close()` hands the DAC back to Android and puts the DAC's volume back to what it was ([decision 11](decisions-volume.md#startup-volume)). While paused, to save power, close only the stream and keep the session: `DacIdleRelease`, 30 s by default. A paused session that is never closed holds the DAC, so other apps get no sound through it. A host that wants to give the DAC back sooner closes the session.
- **A foreground media service is yours.** The library needs no foreground service and no `RECORD_AUDIO`. A player that continues in the background needs the usual media service, with its notification, so that Android keeps the process and the writer thread alive.
- **Audio focus is yours.** The library does no audio-focus handling. Request focus as any player does, and pause (`output.pause()`) when you lose it. On a direct stream the focus has no effect on the audio itself, because nothing goes through Android's mixer: you have to act on it.
- **Keep the writer in the process that holds the media service.** If the writer stops, for example because the process is killed, the ring runs dry and the DAC receives silence.

An outline of a service that owns the output, and what it does when the DAC goes away:

<!-- uncompiled -->

```kotlin
class PlaybackService : MediaSessionService() {
    private lateinit var output: DirectOutputController      // from the integration guide
    private var current: SelectedOutput? = null

    private fun watch(opened: SelectedOutput, scope: CoroutineScope) {
        opened.session?.events
            ?.onEach { event ->
                when (event) {
                    is DacEvent.DeviceLost -> pauseAndExplain("The DAC was unplugged")
                    is DacEvent.StreamFailed -> onStreamFailed(event.error)
                    is DacEvent.LostFrames -> showNotVerified(event.frames)
                    else -> Unit
                }
            }
            ?.launchIn(scope)
    }

    override fun onDestroy() {
        current?.close()          // off the main thread in a real service
        super.onDestroy()
    }
}
```

**On unplug, pause and say so.** Mornye's README says "Disconnect pauses playback; it never redirects active USB playback to the speaker", and Flick's hardware checklist for pull request #400 lists the same expectations: unplug pauses, playback does not continue on the speaker, and replugging resumes on the DAC. usbdac gives you `DacEvent.DeviceLost` and `DacState.LOST`; what the player does next is policy. When `DeviceLost` arrives, close the session, wait for the next `DacHotplugEvent.Attached`, and open again when the listener presses play (or by itself, if your settings say so).

## Settings the listener must own {#settings}

Three choices belong in the app's settings screen, in the listener's words:

| Setting | In the library | Worded as | Default to |
|---|---|---|---|
| Let this app take over the DAC | `OutputPolicy.allowDirectUsb` | "Use the DAC directly. While music plays, other apps and system sounds cannot use it." | On, once the listener has understood the cost. The library cannot decide this for them: a navigation prompt that has to come through the same DAC is lost when direct USB is on |
| Convert what the DAC cannot play | `OutputPolicy.allowConversion` | "If the DAC cannot play a file as it is, convert it (not bit-perfect)." | Off. Conversion changes the audio |
| Allow a DAC with no volume control | `DacConfig.requireHardwareVolume = false` | "Play on a DAC whose volume cannot be lowered, at its own level." | Off, with a warning the first time: the first sound plays at the DAC's own level. Mornye and BitChord ask twice |

Optional, for a "choose the output yourself" screen or a bug report: `OutputPolicy.force`, and `DacConfig` fields such as `cancelInFlightOnFlush` or `allowUnsynchronizedAsync` for DACs that need them ([controls](controls.md)). Do not offer them to everybody.

Changing a setting while something plays should not move the stream. Apply it at the next track boundary: build a new selector with the new policy and let the old output be closed when the next track opens.

## Volume {#volume}

On a direct stream the system volume does not reach the DAC. The volume interface is the DAC's own control, `DacVolume` (`session.volume`, or `selectedOutput.volume`), and it is `null` when the DAC has none.

- **The slider.** `setFraction(0.0 to 1.0)` on a curve whose zero is the DAC's minimum and whose remaining travel spans the top 80 dB or the DAC's range, whichever is smaller ([decision 10](decisions-volume.md#volume-writes)). It returns at once and is safe on every slider event: writes are queued, a burst collapses to its last value. Do not hold the thumb to a read-back: let it follow the finger, and correct to `volume.state` when the writes have settled.
- **Show the level in dB** from `state.db`, with mute from `state.muted` (`null` when the DAC has no mute).
- **Remember levels in dB per DAC, not as slider positions.** The curve is not linear in the DAC's range, and a position means something else on another DAC.
- **Cap what you restore.** When a DAC connects, the library lowers it to at most -40 dB (`DacConfig.startupVolume`) and records what it had. `volume.startup.limitDb` is the quietest original channel. A remembered level above it should not be restored without the listener's action: that is how loud the DAC itself was when it connected.
- **Before the first track.** Volume and mute work with no stream open, so the player can show the level and let the listener set it before pressing play.
- **The phone's keys.** `DacVolumeKeys.handle(event, volume)` from `dispatchKeyEvent` while an activity is in front, and `DacVolumeProvider` on the `MediaSession` for the panel and for the keys with the screen off ([controls](controls.md#volume-keys)). Flick PR #417 notes that Android routes the background keys to a session only while it is playing.
- **Hardware knobs.** A knob on the DAC changes its level without telling the library: call `refresh()` (off the main thread) when you need the true level.
- **On close**, the library writes the original level back by default. If your app takes the DAC straight back, use `HandBack.KEEP`; if it was unplugged, the library skips the restore.

## The signal path display {#the-signal-path-display}

A listener who bought a DAC wants to see what the app is doing. The library gives you what is needed for an honest line. Suggested content, and where each piece comes from:

| Shown | From | Notes |
|---|---|---|
| The output | `decision.kind` | Direct USB, Android bit-perfect mixer, AAudio exclusive, standard |
| The format | `output.format` (`WriteFormat`) | Rate, bit depth (`validBits`) and channels. Say "24-bit in a 4-byte slot" only in a details view |
| The guarantee | `decision.guarantee` and the output's `bitPerfect` | The first is made before play; the second is live |
| The indicator | `bitPerfect` | `Verified`: show it. `NotVerified(blocker, reason)`: show the sentence, which names what is wrong. `RequestedViaPlatform`: "requested from Android, not verifiable". `Unverifiable`: say nothing is promised |
| Why not a better output | `decision.alternativesRejected` | Each has a sentence. `describeDecision` in [automatic output](automatic-output.md#reading-the-decision) is a compiled example |
| Conversion | `decision.conversion?.description` | A sentence such as "Lossy conversion, not bit-perfect: resampled from 44100 Hz to 48000 Hz ..." |
| The DAC | `decision.dac` | Name and USB ids |
| Clock and pacing | `StreamFormat.clock`, `StreamFormat.pacing` | In a details view. The first thing to read when a DAC plays at the wrong pitch |
| Counters | `stream.stats()` | Underruns, lost frames, queue depth, feedback state. Poll a few times a second, not per buffer |

Two rules for the indicator. First, combine the library's flag with what your engine did: show "bit-perfect" only if the flag is `Verified` **and** the engine applied nothing ([processing](integration-player.md#processing)). Second, label the source: a decoded lossy file through a verified stream is "direct USB, decoded from AAC".

A mapping from state to one line, as an outline:

<!-- uncompiled -->

```kotlin
fun indicatorText(state: BitPerfectState, engineUntouched: Boolean): String = when {
    !engineUntouched -> "Not bit-perfect: the player is processing the audio"
    state is BitPerfectState.Verified -> "Bit-perfect"
    state is BitPerfectState.NotVerified -> "Not bit-perfect: ${state.reason}"
    state is BitPerfectState.RequestedViaPlatform -> "Bit-perfect mode requested from Android (cannot be verified)"
    state is BitPerfectState.Unverifiable -> state.reason
    else -> ""
}
```

## Failures and fallback {#failures-and-fallback}

Every failure is a `DacException` with a typed `code`; [troubleshooting](troubleshooting.md#error-codes) has the full table. What matters for an integration is what the app does about each kind. A rule for each, as a starting point:

| Failure | When | What the app does |
|---|---|---|
| `PERMISSION_DENIED` | Opening a direct session | Ask ([permission](#permission-and-hot-plug)). If denied, tell the listener once and use the next output. With the selector this is already the decision |
| `BUSY` | Opening a session or a selector output | Something in your own process holds the DAC. Close it. Treat it as a bug in your lifecycle |
| `CLAIM` | Opening a session | Android keeps the DAC. Offer the steps in [Android keeps the DAC](troubleshooting.md#android-keeps-the-dac) and use the next output meanwhile |
| `UNSUPPORTED` | `negotiate`, `openStream` | The DAC has no alternate setting for the request. Offer conversion if the listener allowed it, otherwise the next output, saying why |
| `IMPLICIT_FEEDBACK_UNSUPPORTED` | `negotiate`, `openStream` | The selector already moves to the next output. With the direct calls, use `AndroidManagedOutput`, or `allowUnsynchronizedAsync` if the listener accepts clicks and no bit-perfect claim |
| `CLOCK_UNVERIFIED` | `openStream` | The DAC did not confirm the rate. Report with `format.clock`; as a last resort the listener can turn off `requireVerifiedRate`, and the stream is then never verified |
| `VOLUME_UNAVAILABLE` | `openStream` | Ask whether to allow a fixed level, and reopen with `requireHardwareVolume = false` |
| `TRANSFER`, `StreamFailed` | While playing | The stream is `FAILED` and stays open until closed. Stop the writer, close the stream, keep the position, and **pause with an explanation**. Retry once on the listener's play; if it fails again, report |
| `DEVICE_LOST`, `DeviceLost` | While playing | See [on unplug](#lifecycle). Do not continue on the speaker without the listener's say |
| `INVALID_STATE` | Many | Your lifecycle is wrong: a write after `drain()`, a second stream, a call on a closed session |
| `EXCLUSIVE_UNAVAILABLE` | AAudio open | The platform granted only a shared stream. The selector already moves on |

Two rules behind the table.

- **Do not fall back to the speaker silently.** A listener who chose a DAC and hears the phone speaker will not forgive it. Mornye's README: "A failure after direct-USB preparation never redirects to the speaker." Fall back to another output only when the listener's settings say it is acceptable, and when you do, show the reason.
- **Decide fallback at a track boundary, not in the middle.** The selector cannot move a playing stream. A failure that ends the stream is a pause in which you may choose again.

One more failure is not an exception: **a per-packet transfer error ends the stream** (a transfer that completes with one packet in error), as in the three drivers studied. It is reported as `StreamFailed` with code `TRANSFER`. BitChord issue #1485 describes a phone where it happens often; usbdac has no data on how often it happens elsewhere ([decision 3](decisions-stream.md#loss-handling)). If your listeners hit it, the error message carries the packet diagnostics.

## Diagnostics and bug reports

- Turn on `UsbDac.setLogLevel(4)` in a debug setting and keep logcat of the tag `usbdac`.
- Offer a "copy diagnostics" action that writes: the DAC's name and USB ids, `session.capabilities`, the `StreamFormat` (with `clock` and `pacing`), `decision` and its rejections, `stream.stats()`, the bit-perfect state, the phone model and Android version, and the library version (`UsbDac.nativeVersion`). Everything needed to reproduce a clock or pacing problem is in those.
- Watch `stats().underruns`, `lostFrames`, `urbTimeouts` and the feedback counters during testing. A healthy stream on the one tested phone showed no lost frame, timeout or underrun in the sweep.
- Treat `DacEvent.Unknown` and `BitPerfectBlocker.UNKNOWN` as a newer native library than your SDK and log them.

## Testing your integration {#testing-your-integration}

**Without a DAC.** Code the writer and the transport against `PcmOutput`, which is an interface: a fake that records the bytes, the calls and their order tests the writer loop, the seek order (flush after the decoder moved, no stale chunk written), the end-of-queue drain and the gapless path with no hardware. usbdac's own tests do the same against a fake DAC (`transport/fake_dac.rs`) and a fake native layer. They do not tell you that a DAC will play.

**With a DAC.** usbdac has run on one phone and one DAC, so every other pairing is a test. Adapt this matrix, and record the phone, Android version, DAC, cable and hub:

| Check | What to look for |
|---|---|
| Cold start: DAC plugged in, app launched, direct output chosen | The stream starts, the indicator reaches `Verified` after real audio, the level is at or below -40 dB |
| 44.1, 48, 96 kHz and the highest rate the DAC offers, 16 and 24 bit | No clock refusal, no timeouts, `format.clock` and `pacing` as expected |
| Rate change between tracks | The output reopens, a gap of about 100 ms or more, the right rate on the DAC |
| Gapless in one format | No gap, no click, `playedFrames` continuous |
| Pause, resume, seek while playing and while paused | The sound stops and starts at once, no old audio after a seek, the DAC stays locked on silence |
| Skip tracks quickly | No stale audio, no stuck state |
| A long pause (over 30 s) with `DacIdleRelease` | The stream closes, the session stays, play reopens it |
| Unplug during playback, then replug | Pause with a message, no sound on the speaker, play opens the DAC again |
| Volume: slider, keys, keys with the screen off, the knob on the DAC | The level follows the control, the dB shown is right, no click on a step |
| Close the app, then play from another app | The DAC is back at its original level and Android plays through it |
| A DAC with no volume control | `VOLUME_UNAVAILABLE`, then the fixed-level path after the listener agrees |
| Bluetooth connected | The selector chooses standard output, not direct USB |
| Software gain or processing on | The indicator turns off with the right reason |
| DSD or DoP, if your player offers it, on a DAC that takes it | The right byte order, no noise, markers keep alternating through a pause and a seek. Not run on a DAC by this project |

Send what you find, with the log, to [the issue tracker](https://github.com/johnneerdael/android-usbdac/issues).

## Release checklist {#release}

- minSdk 26. The library needs Android 8.0 or later; 24-bit and 32-bit integer PCM through the managed and AAudio outputs need Android 12; the bit-perfect mixer needs Android 14.
- `abiFilters` and the AAR: the library carries arm64-v8a, armeabi-v7a, x86_64 and x86 (about 1.9 MB uncompressed for arm64-v8a). Do not publish an app built from an AAR made with `-Pusbdac.abis` or `-Pusbdac.skipNative=true`.
- R8: the library ships consumer rules that keep `Native`, `AAudioNative` and `NativeListener`. Keep them if you replace the rules.
- The manifest merges `android.hardware.usb.host` with `required="false"`, so your app is still offered on phones without host mode (the managed output works there).
- Say in your own release notes what is verified. This library has run on one phone and one DAC.
- Credit: usbdac is MIT licensed. The `NOTICE` file records that it began from Flick's USB driver. Include both notices as the licence asks.
