# Troubleshooting

Turn on the native library's log first: `UsbDac.setLogLevel(4)` (debug), then `adb logcat -s usbdac`. Include that log in a bug report, together with the DAC's name and USB ids (`DacDeviceInfo.vendorId`, `productId`), `session.capabilities`, the `StreamFormat` and `stream.stats()`.

## Error codes

Every failure is a `DacException`. `error.code` is a `DacErrorCode`, `error.detail` the message without the code, and `error.message` is `"<code>: <detail>"`.

| Code | Where from | Meaning and what to do |
|---|---|---|
| `BUSY` | `UsbDac.open` | A `DacSession` is already open in this process (one per process in this version). Close it first. A session that was not closed holds the DAC until the process dies |
| `PLATFORM_UNSUPPORTED` | native | Direct output needs Linux usbfs: Android or Linux. Not an Android problem; you are on another host |
| `CLAIM` | `UsbDac.open` | The audio interfaces could not be taken from Android's USB audio driver, or are not the set the library expected. See [Android keeps the DAC](#android-keeps-the-dac) |
| `UNSUPPORTED` | `UsbDac.open`, `negotiate`, `openStream`, `AndroidManagedOutput.open` | At open: the device has no AudioControl interface together with a playback AudioStreaming interface. Later: no alternate setting carries the request (rate, depth, channels, DSD), or a managed request the platform cannot make. The message lists how many alternate settings the DAC offers; read `session.capabilities.altSettings`. With `RatePolicy.EXACT` a rate the DAC cannot run ends here |
| `IMPLICIT_FEEDBACK_UNSUPPORTED` | `negotiate`, `openStream` | See [asynchronous endpoints without feedback](#asynchronous-endpoints-without-feedback) |
| `CLOCK_UNVERIFIED` | `openStream` | The DAC did not confirm the sampling frequency it was told (or reported another), and `DacConfig.requireVerifiedRate` is on. The message gives the requested and reported rate. Some DACs have a broken readback: `DacQuirk.SkipClockValidation` (with `IgnoreInvalidSampleRate`) trusts them, or turn `requireVerifiedRate` off and accept that the stream is then never verified |
| `DEVICE_LOST` | any call | The DAC was unplugged or stopped answering. `DacState.LOST` and `DacEvent.DeviceLost` follow; close the session |
| `TRANSFER` | stream, events | An isochronous transfer or the stream machinery failed. The stream goes to `FAILED` and stays open until closed. A cable, hub or port problem is the usual cause |
| `CONTROL` | volume, clock | A class control request failed (volume, mute, clock). A DAC without a volume control throws this for volume calls; `session.volume` is `null` for it |
| `INVALID_STATE` | many | The call is not valid now: a second stream, `negotiate` while a stream is open, `write` after `drain()` or after the stream closed or failed, any call on a closed session |
| `INVALID_ARGUMENT` | many | A value out of range or inconsistent: buffer depths, `busQueueMs` outside 4..64, `transfersInFlight` outside 2..16, a DSD slot size other than 1, 2 or 4, a malformed request |
| `PERMISSION_DENIED` | `UsbDac.open` | The app has no USB permission for the device. See [permission](#permission) |
| `OPEN_FAILED` | `UsbDac.open`, `AndroidManagedOutput.open` | Android refused to open the device or gave no file descriptor, or the `AudioTrack` could not be created |
| `UNKNOWN` | any | A code this version of the SDK does not know, from a newer native library |

## Asynchronous endpoints without feedback

An asynchronous OUT endpoint runs on the DAC's own clock and tells the host the true rate through a separate feedback endpoint. A few DACs declare an asynchronous endpoint with **no** feedback endpoint. They would need *implicit* feedback (rate information on an associated IN data endpoint), which this library does not do; without it the host's packet timing and the DAC's clock drift apart until the DAC under- or overruns.

By default `negotiate` or `openStream` refuses such an alternate setting with `IMPLICIT_FEEDBACK_UNSUPPORTED`, when it is the only way to carry the request. A DAC that also offers a synchronised alternate setting for the same format gets that one. A malformed endpoint (a feedback endpoint that cannot be resolved) is never streamed.

`DacConfig(allowUnsynchronizedAsync = true)` streams it anyway, at the nominal rate, host-paced. Expect the drift to end in clicks or dropouts after some time, and the stream is never bit-perfect (blocker `UNSYNCHRONIZED_ASYNC`). Use the managed output (`AndroidManagedOutput`) for such a DAC if it works there.

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
3. **`negotiate` fails.** `UNSUPPORTED`: pick a rate and depth from `capabilities`. `IMPLICIT_FEEDBACK_UNSUPPORTED`, `CLOCK_UNVERIFIED`: see above.
4. **The stream opens and is silent.** In this order:
   - Is the DAC's **volume at its minimum, or muted**? Read `session.volume?.state`; the library does not change the volume when it opens, so the DAC plays at whatever level it had. If `session.volume` is `null` the DAC has no hardware volume.
   - Is the **writer running**? `stats().framesWritten` must rise. A writer that writes less than `startThresholdMs` of audio leaves the stream in `BUFFERING`, and it never starts. Write at least `startThresholdMs` (100 ms), or call `drain()` for a short clip.
   - Is the stream `PAUSED`, `DRAINED` or `FAILED`? `stream.state` says. After `drain()` writes fail until `flush()`.
   - Does `write` return 0 forever? `writableBytes` is 0 while the ring is full (the DAC is not taking audio) or after `drain()`. Check `stats().transferQueueDepth` and `completedTransfers`.
5. **Noise or static.** The bytes are in the wrong layout. Match `StreamFormat.subslotBytes` and `validBits`: 24-bit in a 4-byte slot is the sample shifted left by 8; samples are little-endian and interleaved. For native DSD the byte order (`dsdBigEndian`, `DsdBigEndian`), the bit order (`DsdBitReverse`; DSF is least significant bit first and must be reversed) and the slot size (`dsdSubslot`) are DAC-specific. For DoP, write the DSD bits as the layout says and leave the marker byte alone.
6. **Clicks and dropouts.** `DacEvent.Underrun` means the writer did not keep the ring filled: raise `bufferTargetMs`, make sure the writer thread is not starved (do not render on the main thread, avoid long garbage collections in it), and write when `writableBytes` says there is room. If the ring is full and the stream still underruns, the output thread itself is late: raise `busQueueMs` (16 ms by default) a few milliseconds at a time. The packets in flight never exceed 255, so a very deep queue at a short service interval is capped. `DacEvent.LostFrames` and `stats().urbTimeouts` mean the USB transport lost audio: cable, hub or port. `DacEvent.FeedbackStale` means the feedback endpoint stopped answering.
   A click or a short re-lock of a DoP or native DSD DAC at a seek or a track change is the gap a flush makes when it cancels the transfers on the bus: set `DacConfig(cancelInFlightOnFlush = false)` and the queue plays out instead (up to `busQueueMs` of the old audio is then heard after the seek).
7. **The flag says not bit-perfect.** `stream.bitPerfect.value` carries the blocker and a sentence. See [bit-perfect output](bit-perfect.md#the-verified-flag).
8. **The volume keys do nothing.** `session.volume` is `null` (the DAC has no volume control), or `dispatchKeyEvent` does not call `DacVolumeKeys.handle`, or the screen is off and no `DacVolumeProvider` is attached to a `MediaSession`.
9. **After unplugging.** `DacEvent.DeviceLost` and `DacState.LOST`. Close the session, wait for the next `DacHotplugEvent.Attached`, and open again.

## Build and packaging

- `UnsatisfiedLinkError` for `usbdac`: the AAR has no library for the device's ABI. The AAR carries arm64-v8a, armeabi-v7a, x86_64 and x86; check `abiFilters` in your app and that you did not build the AAR with `-Pusbdac.abis` restricted.
- `UnsatisfiedLinkError` for a `dev.usbdac.internal.Native` method, or a crash on the first event, with R8 enabled: the library ships consumer rules that keep `Native` and `NativeListener`. If your build replaces or ignores consumer rules, keep them.
- A heap `ByteBuffer` passed to `write` works but is copied on every call; use a direct buffer in a real player.
