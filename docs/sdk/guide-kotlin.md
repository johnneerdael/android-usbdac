# Kotlin player integration

Use the base `dev.usbdac:usbdac` AAR for PCM output without a player framework. Use the separately published `dev.usbdac:usbdac-media3` AAR, at the same release version, for an ExoPlayer audio sink and Media3 session volume. The integration depends on Media3 1.11.1; the base AAR has no Media3 dependency.

The compiled integration is in [`android/usbdac-media3`](https://github.com/johnneerdael/android-usbdac/tree/main/android/usbdac-media3). A service and output-selection example lives separately in [`android/media3-example`](https://github.com/johnneerdael/android-usbdac/tree/main/android/media3-example). The base [`android/sample`](https://github.com/johnneerdael/android-usbdac/tree/main/android/sample) app remains a framework-independent sine player. Build the optional service example with `:media3-example:assembleDebug`.

## Connect the renderer

Add `dev.usbdac:usbdac-media3` using the release version shown in [installation](getting-started.md). Override `DefaultRenderersFactory.buildAudioSink` to return `dev.usbdac.media3.UsbDacAudioSink(outputSource)`. See the compiled [`PlayerModule`](https://github.com/johnneerdael/android-usbdac/blob/main/android/media3-example/src/main/kotlin/dev/usbdac/sample/player/PlayerModule.kt) for audio focus, becoming-noisy handling and renderer construction.

`OutputSource` opens an `OpenedOutput`, which contains a `PcmOutput`, its `WriteFormat` and the original `StreamRequest`. The sink opens lazily on the playback thread when PCM arrives. Opening a session/stream blocks; do not perform it on the UI thread. Implement `close`, `takeLookAgain` and `setVolumeMultiplier` for the app's lifetime and ducking policy. The example's `DirectOutputController` uses `OutputSelector` and observes device loss. A route change marks selection dirty; a seek/flush applies it. A currently playing track never silently changes to the phone speaker.

Use `OutputPolicy(allowWidening = true)` for direct output that accepts wider integer formats while keeping the source rate, channels and clock units. The selector requires a lossless conversion plan. `allowConversion = true` is a separate, broader opt-in that also permits narrowing and resampling; its decision reports the conversion and the resulting guarantee.

## Open hardware volume before selecting a track

After USB permission is granted, call `UsbDac.open` off the main thread. This opens a `DacSession`, discovers its hardware volume and applies the configured startup limit. It does not negotiate a rate, program the clock or open an audio stream. Expose `session.volume` immediately to the UI and session wrapper. Volume is available even when the player has no media item.

The compiled [`PreparedUsbPlayback.open`](https://github.com/johnneerdael/android-usbdac/blob/main/android/media3-example/src/main/kotlin/dev/usbdac/sample/player/PreparedUsbPlayback.kt) demonstrates the direct-only path: permission comes first, session volume is exposed before `setMediaItem`, and the host releases the MediaSession/player before closing the session. It fails on direct-USB failure rather than taking over an automatically selected fallback. The selector example instead owns each selected output and does not retain pre-track volume.

Pass that session to `DacSessionOutputSource` and give the source to `UsbDacAudioSink`. Track output closes release only the stream; the host retains the session and its volume between tracks. Close the session when the user gives the DAC back or the player is destroyed. A session claims the DAC from Android, so make direct USB an explicit user choice and explain that other apps cannot play through that DAC while it is claimed.

`DacSessionOutputSource` accepts exact integer PCM or same-rate/same-channel integer widening. The sink zero-pads low bytes in wider slots, preserving the original signed sample value. Supply its `volumeMultiplier` callback if the host wants focus ducking to drive hardware volume. Manage the original hardware level and restore it when the multiplier returns to one; the integration never multiplies PCM samples.

## Preserve precision and gapless playback

The dedicated sink handles little-endian signed 16-, 24- and 32-bit integer PCM, unsigned8 and IEEE float32 in one or two channels. It rejects compressed and unsupported layouts. It does not run Media3's resampling, integer-to-float, gain, speed or silence-skipping processors. A decoder's own output format remains the precision limit; the sink cannot restore bits a decoder has already discarded.

For each configured track, `Format.encoderDelay` frames are skipped at the start and `Format.encoderPadding` frames are retained until the track ends, then discarded. Trimming copies integer bytes without rounding. Padding storage is bounded to 16 MiB; excessive/negative metadata is rejected. Equal formats retain one stream across track changes. A changed rate/depth/channel layout drains the old stream before opening another. `handleBuffer` observes backpressure and advances only the bytes it accepted; partial PCM frames are rejected.

Seek calls `flush`, discards the held padding and pending input, and anchors position to the next presentation timestamp. A seek into an already configured track does not apply its initial encoder delay again. A fresh `configure` applies that track's trim metadata. End-of-stream drains the hardware output and `isEnded` waits until that drain completes. [Media3 details](media3.md) describes the integration boundaries.

## Route session volume

Wrap the ExoPlayer in `DacVolumePlayer(player, volumes, scope)` before constructing a Media3 `MediaSession`. `volumes` is a `StateFlow<DacVolume?>`, so it can expose the pre-track session volume and continue to expose it while a stream is idle-released. Collect on the player's application looper and cancel that scope with the session.

The wrapper reports remote device volume on a 0..100 scale, maps absolute requests to the hardware curve, steps relative requests by one hardware step and honors mute only when the DAC supports it. With `null` it delegates to the wrapped player's local volume. Its behavior matches the core `DacVolumeProvider`, which remains available for platform `android.media.session.MediaSession`; the base AAR does not require Media3. Foreground activity keys can use `DacVolumeKeys`.

## Release idle streams without skipping queued audio

A paused stream keeps its clock locked by sending silence. `DacIdleRelease` can close it after continuous pause while the `DacSession` remains open for volume. Record the audible position when pausing. On resume call `resume()` and inspect its atomic `IdleResume.reopened` result. A short pause returns the original stream and its queue; continue normally. If it reopened, discard decoder/sink state, rebase position counters and reseek the decoder to the saved audible position before writing more PCM. Continuing from the decoder's already-submitted position would skip audio discarded with the old queue.

On a Media3 output failure, the sink retains the failed output and reports a `WriteException` whose cause is `OutputRecoveryRequiredException`. It cannot replay audio already queued in that output. Pause and, after reconnecting, seek the same seekable media item to a previously observed sink position before resuming; the `OutputSource.onRecoveryPosition` callback supplies the last sink-reported position and receives null on flush. An older observation can repeat audio, and transport counters do not prove precisely what was audible. If the item is not seekable or no position was observed, leave playback paused and ask the listener to choose a restart point. The host must bind the observed point to the failed output and the current media item, confirm its chosen DAC is available, and retain that identity when applying the seek. The example does not automate reconnection.

This helper owns a PCM output, not ExoPlayer's decoder. For Media3, coordinate release and the reseek on the player/playback thread and have the output owner observe the replacement. Do not close an output from a timer while the sink continues using its old reference. The example does not install an idle timer automatically.

## Acceptance on a real DAC

Verify USB permission and pre-track volume, including a session with no media item. Play integer PCM at 16, 24 and 32 bits; inspect the selected layout and conversion decision. Test gapless tracks carrying nonzero encoder delay/padding, seek with queued audio, backpressure, final drain, route changes, unplug while playing and while paused, and the system volume panel with the app backgrounded. Test idle release by saving the audible position and confirming that only a reopened stream triggers the reseek. Unit tests cover sink byte preservation and transport ordering; no physical-device result is implied by those tests.

## Permission and service setup

Follow [getting started](getting-started.md) for the USB-host manifest feature, device filter, `UsbDac.attachedDacs` and `requestPermission`. Request permission in a user-visible activity; a playback service observes the result and opens only after the grant. A permission denial leaves Android playback available under the automatic selector policy. Direct-only preparation reports the denial/failure to the host instead of choosing a fallback.

Declare a `MediaSessionService` with `foregroundServiceType="mediaPlayback"`, the `androidx.media3.session.MediaSessionService` action, and foreground-service/media-playback permissions. The optional example includes its own manifest. Notification permission and starting a foreground service must follow the host app's target-SDK/lifecycle rules. A service owns the player, output controller and MediaSession; release the session, player and owned USB session when the service ends. Cancel event/volume collection with the session. The base sample has no Media3 service or foreground playback permissions.

## Reconfigure a retained direct stream

`DacSession.reconfigure(current, request, drainTimeoutMs)` first validates ownership and drains the existing playback-only output. The native transport retains and checks the exact libusb owner across retirement, new clock/alternate negotiation and replacement. The Android connection, claimed interfaces and `DacVolume` stay owned by the session. Both successful and rolled-back replacements are paused with zero-based counters; use the returned stream, rebase/reseek for its actual format, supply PCM, then play.

A recovered requested-format failure returns `StreamReconfiguration.rollbackCause` with a usable old-format replacement. `RECONFIGURE_DRAIN_TIMEOUT` leaves the original stream open/draining and its token valid; the host can keep waiting, explicitly flush or close. There is no hidden queue discard on timeout. `RECONFIGURE_UNUSABLE` means ownership/retirement/rollback could not be proved: the old token may still refer to a retired stream, and must not be used for playback. Close the session. An overflowing/negative deadline and a foreign/duplex stream are refused before a format change.

`DacSessionOutputSource` uses this transaction after the sink observes drain completion. Equal formats still retain one gapless stream. A rolled-back change is surfaced as a Media3 initialization error before any new-format PCM is written; the restored stream remains owned and paused until the host chooses recovery or releases the player. An automatic selector source can retain its close/open policy by leaving `OutputSource.reconfigure` at its null default.
