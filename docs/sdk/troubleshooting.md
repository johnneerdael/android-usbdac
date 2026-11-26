# Troubleshooting

Turn on the native library's log first: `UsbDac.setLogLevel(4)` (debug), then `adb logcat -s usbdac`. Include that log in a bug report, together with the DAC's name and USB ids (`DacDeviceInfo.vendorId`, `productId`), `session.capabilities`, the `StreamFormat` (its `clock` and `pacing` in particular, see [diagnostics](controls.md#diagnostics-the-clock-and-the-pacing)) and `stream.stats()`.

## Error codes

Every failure is a `DacException`. `error.code` is a `DacErrorCode`, `error.detail` the message without the code, and `error.message` is `"<code>: <detail>"`.

| Code | Where from | Meaning and what to do |
|---|---|---|
| `BUSY` | `UsbDac.open` | A `DacSession` is already open in this process (one per process in this version). Close it first. A session that was not closed holds the DAC until the process dies |
| `PLATFORM_UNSUPPORTED` | native | Direct output needs Linux usbfs: Android or Linux. Not an Android problem; you are on another host |
| `CLAIM` | `UsbDac.open` | The audio interfaces could not be taken from Android's USB audio driver, or are not the set the library expected. See [Android keeps the DAC](#android-keeps-the-dac) |
| `UNSUPPORTED` | `UsbDac.open`, `negotiate`, `openStream`, `AndroidManagedOutput.open`, `AAudioExclusiveOutput.open` | At open: the device has no AudioControl interface together with a playback AudioStreaming interface. Later: no alternate setting carries the request (rate, depth, channels, DSD), or a managed request the platform cannot make. The message lists how many alternate settings the DAC offers; read `session.capabilities.altSettings`. With `RatePolicy.EXACT` a rate the DAC cannot run ends here |
| `IMPLICIT_FEEDBACK_UNSUPPORTED` | `negotiate`, `openStream` | See [asynchronous endpoints without feedback](#asynchronous-endpoints-without-feedback) |
| `CLOCK_UNVERIFIED` | `openStream` | The DAC did not confirm the sampling frequency it was told (or reported another), and `DacConfig.requireVerifiedRate` is on. The message gives the requested and reported rate. Some DACs have a broken readback: `DacQuirk.SkipClockValidation` (with `IgnoreInvalidSampleRate`) trusts them, or turn `requireVerifiedRate` off and accept that the stream is then never verified |
| `DEVICE_LOST` | any call | The DAC was unplugged or stopped answering. `DacState.LOST` and `DacEvent.DeviceLost` follow; close the session |
| `TRANSFER` | stream, events | An isochronous transfer or the stream machinery failed. The stream goes to `FAILED` and stays open until closed. A cable, hub or port problem is the usual cause |
| `CONTROL` | volume, clock | A class control request failed (volume, mute, clock). A DAC without a volume control throws this for volume calls; `session.volume` is `null` for it |
| `INVALID_STATE` | many | The call is not valid now: a second stream, `negotiate` while a stream is open, `write` after `drain()` or after the stream closed or failed, any call on a closed session |
| `INVALID_ARGUMENT` | many | A value out of range or inconsistent: buffer depths, `startupVolume` ceiling outside -128..0 dB, `busQueueMs` outside 4..64, `transfersInFlight` outside 2..16, a DSD slot size other than 1, 2 or 4, a malformed request |
| `VOLUME_UNAVAILABLE` | `openStream` | The DAC has no usable hardware volume control (none, or the start-up attenuation could not be verified) and `DacConfig.requireHardwareVolume` is on: a stream would play at the DAC's own level and nothing could bring it down. `negotiate` and `capabilities` still work. Show the user what the DAC offers, ask whether to allow fixed-level output, and open the session again with `requireHardwareVolume = false`. See [start-up volume protection](controls.md#start-up-volume-protection) |
| `EXCLUSIVE_UNAVAILABLE` | `AAudioExclusiveOutput.open` | `AAudioConfig.requireExclusive` is on and the platform granted only a shared AAudio stream. The message carries the grant's reason. Exclusive mode depends on the device and the route; open with `requireExclusive = false` to accept a shared stream, or fall back to `AndroidManagedOutput` |
| `PERMISSION_DENIED` | `UsbDac.open` | The app has no USB permission for the device. See [permission](#permission) |
| `OPEN_FAILED` | `UsbDac.open`, `AndroidManagedOutput.open` | Android refused to open the device or gave no file descriptor, or the `AudioTrack` could not be created |
| `UNKNOWN` | any | A code this version of the SDK does not know, from a newer native library |

## Asynchronous endpoints without feedback

An asynchronous OUT endpoint runs on the DAC's own clock. The driver follows explicit rate reports or derives pacing from a compatible IN data endpoint on the same clock. If neither source is usable, the host and DAC can drift apart until playback under- or overruns. Read `format.pacing` for the selected source. For an implicit route, it identifies the capture interface, alternate setting and endpoint.

By default `negotiate` or `openStream` refuses such an alternate setting with `IMPLICIT_FEEDBACK_UNSUPPORTED`, when it is the only way to carry the request. A DAC that also offers a synchronised alternate setting for the same format gets that one. A malformed endpoint (a feedback endpoint that cannot be resolved) is never streamed.

`DacConfig(allowUnsynchronizedAsync = true)` streams it anyway, at the nominal rate, host-paced. Expect the drift to end in clicks or dropouts after some time, and the stream is never bit-perfect (blocker `UNSYNCHRONIZED_ASYNC`). Use the managed output (`AndroidManagedOutput`) for such a DAC if it works there.

## Feedback lock and dropouts {#feedback-lock-and-dropouts}

If a direct stream has opened but loses feedback, record `format.pacing`,
`stats().feedbackLocked`, `feedbackAgeMs` and the `usbdac` log. Include the
DAC's USB IDs, the requested format, and whether the issue occurs at startup
or after playback begins.

- **Explicit.** A successful empty poll increments `feedbackEmpty` and
  leaves the last valid report timestamp alone. A burst of rejected reports
  shows up in `feedbackRejected`. If the source goes stale, the driver emits
  `FeedbackStale` and holds the last valid rate during the recovery window.
  If valid timing fails to return by the deadline, it emits `StreamFailed`
  with `TRANSFER`.
- **Implicit.** `format.pacing` identifies the IN interface, alternate setting
  and endpoint. Include `implicitFifoDepth`, `implicitWaits` and
  `implicitInvalidPackets` to distinguish a timing-source gap from a writer
  underrun. A transport failure can end the stream with `TRANSFER`.

In both modes, `FEEDBACK_UNLOCKED` can appear while the driver acquires or
reacquires timing. See [all feedback counters](controls.md#diagnostics-the-clock-and-the-pacing).

### Collect a feedback support report

Offer a support toggle that opens a new USB session with
`DacConfig(captureFeedbackTrace = true)`. If you use `OutputSelector`, pass
that configuration through `OutputPolicy.dacConfig`. Ask the listener to
reproduce once and export `DacSession.feedbackTraceJson()`. The report stays
available after the stream fails or closes. A closed session object retains
its final report.
The sample app has **Capture USB feedback for support** and **Copy feedback
report** controls.

The export includes the selected format and clock route, USB IDs and claims,
endpoint geometry, submission errors, libusb transfer and packet statuses,
and timestamped packet lengths. Explicit reports include up to eight raw
bytes per observation, the decoded rate, the selected timebase and any rejection reason. Implicit
feedback records packet metadata only. The capture retains the beginning
and the most recent observations, and counts discarded entries. It stays
in memory until the app exports it. Explicit feedback failures also carry a
short summary. Include the Android version and DAC firmware when sending a report.

## Wrong pitch, or no sound at one sample rate

On a DAC with more than one clock (a Clock Selector choosing between an internal oscillator and another source, or Clock Multipliers in front of a source), the sampling frequency is programmed on one Clock Source. If it goes to the wrong one, the DAC plays at the wrong pitch, or is silent at one rate while others work. Look at `clock.path` and `clock.resolution` of the `StreamFormat` that `negotiate` returned:

- **`clock.resolution == FALLBACK_FIRST_SOURCE`**: the library could not trace the stream's terminal to its clock (no terminal link, a malformed or looping entity, a selector that would not say which pin is active) and used the first Clock Source in descriptor order. On a multi-clock DAC that may be the wrong one. The `usbdac` log (`UsbDac.setLogLevel(2)` or more) has the reason on a warning line. Report it with the log, the DAC's USB ids and its descriptors (`lsusb -v` or the Android USB host dump).
- **`clock.resolution == RESOLVED`**: read `clock.path` from the terminal down. Each `Selector` hop shows the pin the DAC reported as active (`selectedPin` of `pinCount`); the library follows what the DAC says and never switches it. If the source at the end is not the one that clocks the playback terminal on that DAC, the selector is on another input than you expect: a clock switch on the DAC, an external clock input, or the DAC's own default. Compare with the DAC's descriptors.
- **`clock.sourceId` and `clock.controlInterface`** say where the sampling frequency was written and read back. `CLOCK_UNVERIFIED` (see the table above) is the DAC refusing to confirm it on that source.
- **`clock == null`**: the DAC describes no clock entities (UAC 1.0, whose rate is set on the endpoint). The clock topology is not the cause.

`format.pacing` answers the other half, how packets are paced: `Unsynchronized` on an asynchronous endpoint is the [no-feedback case](#asynchronous-endpoints-without-feedback), the usual cause of clicks that appear after some time. See [diagnostics](controls.md#diagnostics-the-clock-and-the-pacing) for the fields.

## Permission

USB permission is per device and asked at run time.

- `UsbDac.requestPermission(context, device) { granted -> ... }` shows the system dialog. The callback runs on the main thread, with `true` at once if the permission is already held.
- `UsbDac.open` without permission throws `PERMISSION_DENIED`; it never shows a dialog itself.
- If the activity declares the `USB_DEVICE_ATTACHED` intent filter with a device filter (see [getting started](getting-started.md#2-the-manifest)) and the user chooses "always" in the dialog Android shows on attach, the permission is remembered for that device and app.
- Permission is not kept across unplugging unless it was granted that way.
- A request needs a foreground app: Android may not show the dialog from the background.

## Android keeps the DAC

`CLAIM` at open usually means Android's USB audio driver still has the interface. The library claims the AudioControl interface and the playback AudioStreaming interfaces with `claimInterface(interface, force = true)`, which detaches the kernel driver. If that fails:

1. Unplug the DAC and plug it in again.
2. Turn on **Disable USB audio routing** in Developer options, then plug it in again. It stops Android from routing audio to USB devices, which is what the force-claim fights.
3. Check that no other app (or an earlier session of yours) holds the DAC.
4. Read the `usbdac` log for the interface number that failed.

On release the library returns the streaming interfaces to alternate setting 0 and calls `releaseInterface`, which also asks Android to take its driver back. If the DAC does not come back as an Android audio output after `close()`, replug it.

## Nothing plays

Work down the list.

1. **`attachedDacs` is empty.** The device has no Audio Streaming interface with an isochronous OUT endpoint: it is not a USB audio playback device, the cable carries power only, or the adapter or hub is the problem. A capture-only device is not listed.
2. **Open fails.** Read the code above. `BUSY`, `PERMISSION_DENIED` and `CLAIM` are the usual ones.
3. **`negotiate` fails.** `UNSUPPORTED`: pick a rate and depth from `capabilities`. `IMPLICIT_FEEDBACK_UNSUPPORTED`, `CLOCK_UNVERIFIED`: see above. Silence or a wrong pitch at one rate on a DAC with several clocks: [look at the clock](#wrong-pitch-or-no-sound-at-one-sample-rate).
4. **The stream opens and is silent.** In this order:
   - Is the DAC's **volume at its minimum, or muted**? Read `session.volume?.state`. `UsbDac.open` lowers the volume to at most -40 dB (`DacConfig.startupVolume`), so a DAC that was at full scale is quiet until the slider goes up; `session.volume?.startup` says what it had and what it was lowered to. If `session.volume` is `null` the DAC has no usable hardware volume, and `openStream` fails with `VOLUME_UNAVAILABLE` unless `requireHardwareVolume` is off.
   - Is the **writer running**? `stats().framesWritten` must rise. A writer that writes less than `startThresholdMs` of audio leaves the stream in `BUFFERING`, and it never starts. Write at least `startThresholdMs` (100 ms), or call `drain()` for a short clip.
   - Is the stream `PAUSED`, `DRAINED` or `FAILED`? `stream.state` says. After `drain()` writes fail until `flush()`.
   - Does `write` return 0 forever? `writableBytes` is 0 while the ring is full (the DAC is not taking audio) or after `drain()`. Check `stats().transferQueueDepth` and `completedTransfers`.
5. **Noise or static.** The bytes are in the wrong layout. Match `StreamFormat.subslotBytes` and `validBits`: 24-bit in a 4-byte slot is the sample shifted left by 8; samples are little-endian and interleaved. For native DSD the byte order (`dsdBigEndian`, `DsdBigEndian`), the bit order (`DsdBitReverse`; DSF is least significant bit first and must be reversed) and the slot size (`dsdSubslot`) are DAC-specific. For DoP, write the DSD bits as the layout says and leave the marker byte alone.
6. **Clicks and dropouts.** `DacEvent.Underrun` means the writer did not keep the ring filled: raise `bufferTargetMs`, make sure the writer thread is not starved (do not render on the main thread, avoid long garbage collections in it), and write when `writableBytes` says there is room. If the ring is full and the stream still underruns, the output thread itself is late: raise `busQueueMs` (16 ms by default) a few milliseconds at a time. The packets in flight never exceed 255, so a very deep queue at a short service interval is capped. `DacEvent.LostFrames` and `stats().urbTimeouts` mean the USB transport lost audio: cable, hub or port. `DacEvent.FeedbackStale` means the feedback source stopped providing usable timing. Use the [feedback diagnostics above](#feedback-lock-and-dropouts) to identify the source and collect the counters.
   A click or a short re-lock of a DoP or native DSD DAC at a seek or a track change is the gap a flush makes when it cancels the transfers on the bus: set `DacConfig(cancelInFlightOnFlush = false)` and the queue plays out instead (up to `busQueueMs` of the old audio is then heard after the seek).
7. **The flag says not bit-perfect.** `stream.bitPerfect.value` carries the blocker and a sentence. See [bit-perfect output](bit-perfect.md#the-verified-flag).
8. **The volume keys do nothing.** `session.volume` is `null` (the DAC has no volume control), or `dispatchKeyEvent` does not call `DacVolumeKeys.handle`, or the screen is off and no `DacVolumeProvider` is attached to a `MediaSession`.
9. **After unplugging.** `DacEvent.DeviceLost` and `DacState.LOST`. Close the session, wait for the next `DacHotplugEvent.Attached`, and open again.

## Conversion

`ConversionPlan.between`, `PcmConverter.create` and `ConvertingOutput` fail with typed codes:

- `UNSUPPORTED` with "DoP and native DSD are never converted": a DSD stream cannot go through a converter. Use the direct path as it is.
- `UNSUPPORTED` with "only mono to stereo is supported": no downmix, no upmix, no channel remapping.
- `UNSUPPORTED` with "needs a filter of ... taps in ... phases": a rate ratio that does not reduce to small integers (44100 to 47993 Hz) would need a filter larger than the converter builds. Rates between 8000 and 768000 Hz in the 44.1 and 48 kHz families are all fine.
- `INVALID_ARGUMENT`: a rate or channel count out of range, a negative `ditherSeed`, a `headroomDb` outside -60 to 0, a heap buffer given to `PcmConverter.convert` (use direct buffers).
- `INVALID_STATE`: `convert` after `finish` without `reset()`, or any call after `close()`.
- Clipping: `PcmConverter.clippedSamples` counts samples saturated at full scale. A resampled signal can exceed its input's peak; set `ConversionOptions.headroomDb` (`-3.5` covers a full-scale square wave).
- A `ConvertingOutput` whose `write` takes fewer bytes than you offered is full: the target is not taking audio, and the converted audio waits in the wrapper. Write when `writableBytes` says there is room.

## AAudio exclusive output

`AAudioExclusiveOutput.open` says why it could not open; the usual ones:

- `UNSUPPORTED` with "need Android 12 (API 31)": packed 24-bit and 32-bit integer samples are not in AAudio before Android 12. Use 16-bit, 32-bit float (`AAudioConfig(floatSamples = true)`) or another output.
- `UNSUPPORTED` with "did not open the stream as requested": the platform opened the stream at another rate, channel count or format than the one you write. The message says which. The output never converts, so it refuses; the route may only run one rate (read `AudioDeviceInfo.sampleRates` of the device).
- `UNSUPPORTED` with "AAudio is not available": `libaaudio.so` could not be opened.
- `BUSY`: another app holds the device exclusively.
- `EXCLUSIVE_UNAVAILABLE`: see the table above.
- `DEVICE_LOST` from `write`, `AAudioEvent.Disconnected` and `state == FAILED`: the route went away. Close the output and open another.
- Clicks: `AAudioEvent.Underrun` (the ring ran dry: raise `bufferTargetMs`, check the writer thread) against `AAudioEvent.Xrun` (the device-side buffer ran dry: raise `deviceBufferBursts`, or check that `performanceMode` is `LOW_LATENCY`).
- Silence: the writer must write at least `startThresholdMs` of audio before playback starts, as for a direct stream; check `state` and `writableBytes`.

The `usbdac` log (`UsbDac.setLogLevel`) also covers this output.

## Automatic output

`OutputSelector.select` and `open` say why the output they chose is not a better one in `decision.alternativesRejected` (see [automatic output](automatic-output.md#reading-the-decision)); read it first. Then:

- **The direct output is never chosen.** Look for its rejection. `POLICY`: `allowDirectUsb` is off. `PERMISSION_MISSING`: ask with `UsbDac.requestPermission` (the decision's `needsUsbPermission` names the DAC) and select again. `ROUTE_UNSUITABLE`: audio is routed to Bluetooth or a cast device. `IMPLICIT_FEEDBACK`, `NO_HARDWARE_VOLUME`, `BUSY` and `FAILED` carry the `DacErrorCode` in `error`: the same codes as above.
- **The exclusive AAudio output is never chosen.** `EXCLUSIVE_NOT_GRANTED` means the platform granted only a shared stream on that route; `NOT_AS_REQUESTED` means the hardware runs another rate or format; `PLATFORM_TOO_OLD` means 24-bit or 32-bit integer samples on a release before Android 12. Whether a device grants exclusive mode is the platform's decision, not the library's.
- **`INVALID_STATE` from `select` or `open`.** An output this selector opened is still open. Close it first: the DAC and an exclusive stream can only be held once.
- **`UNSUPPORTED` listing every output.** No output can carry the request, typically DSD with no DAC, or 24-bit PCM on a release before Android 12. Read the list.
- **A forced output fails.** With `OutputPolicy.force` the typed error of that output is thrown and nothing else is tried; it is the same code the rejection would have had.
- **`select` takes a long time or the DAC glitches.** With a DAC attached it opens a session and negotiates, which programs the clock, and gives the DAC back afterwards. Do not call it while another app is playing to the DAC, and call it off the main thread.
- **The decision lists the wrong route before Android 13.** Android cannot say where media goes before release 13, so the library guesses from the connected outputs. Select again after the route settles, or force an output.

## Build and packaging

- `UnsatisfiedLinkError` for `usbdac`: the AAR has no library for the device's ABI. The AAR carries arm64-v8a, armeabi-v7a, x86_64 and x86; check `abiFilters` in your app and that you did not build the AAR with `-Pusbdac.abis` restricted.
- `UnsatisfiedLinkError` for a `dev.usbdac.internal.Native` or `AAudioNative` method, or a crash on the first event, with R8 enabled: the library ships consumer rules that keep `Native`, `AAudioNative` and `NativeListener`. If your build replaces or ignores consumer rules, keep them.
- A heap `ByteBuffer` passed to `write` works but is copied on every call; use a direct buffer in a real player.
