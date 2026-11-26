# Design decisions: volume

Four decisions about the DAC's volume. On a direct USB stream Android's own volume does not reach the DAC, so the driver has to decide who owns the level, how the slider maps onto it, how to keep the first sound from being loud, and how the phone's buttons and the idle DAC fit in. Volume is where the three drivers agree most and where they still differ in the details.

The parts of each decision are the question, what the three drivers do, what usbdac does, the evidence, the trade-off and the public references. Revisions are defined on [the drivers page](drivers.md#revisions-studied); the [overview of all decisions](decisions.md) has a summary table. The Kotlin and Rust API for volume is in [controls](controls.md#hardware-volume-dacvolume).

## 9. Who owns the volume {#volume-ownership}

**The question.** With the DAC taken away from Android's mixer, the system volume slider does nothing. Does the player scale the samples itself, use the DAC's own volume control, or both?

**In plain terms.** Scaling the samples in software changes the numbers sent to the DAC, so the stream is no longer bit-perfect, and at low levels it throws away resolution. Most USB DACs have a volume control of their own that turns the level down after the audio has arrived, so the audio the DAC receives stays as it was. Some DACs have no such control.

| | What it does | Where |
|---|---|---|
| Flick | Routes volume to one of three authorities: the DAC's hardware control, the engine's software volume, or the system. The hardware control is used when the DAC has one and direct USB is active | `docs/uac2/internals.md` (`volumeMode`), `rust/src/uac2/volume_control.rs`; PRs #416 and #417 call it "the volume authority" |
| Mornye | The DAC only. ReplayGain, AutoMix, software volume and playback-rate processing "are bypassed while USB mode is selected". A DAC without a verified hardware volume stays paused unless the user enables fixed-volume output after a second warning | `usb_audio/README.md` ("Hardware volume"), `usb_audio/volume.rs` |
| BitChord | The DAC's level "is the app's volume" while direct USB is open, and "nothing upstream may scale a sample". With no DAC volume the player uses normal output, unless the user accepts a fixed level after a second warning | `bitperfect/DacVolume.kt`, commit `06628ed1` |
| usbdac | The DAC's Feature Unit volume and mute, with a read-back after every write. Software gain exists on a PCM stream (`set_software_gain`, 0.0 to 16.0, rounded, saturated, no dither) and ends the bit-perfect flag (blocker `SOFTWARE_GAIN`); it is refused for DoP and native DSD. A DAC with no usable hardware volume fails `open_stream` with `VOLUME_UNAVAILABLE` unless the host has set `requireHardwareVolume = false` | `volume.rs`, `volume_control.rs`, `transport/volume_hw.rs`, `DacConfig::require_hardware_volume` |

**What usbdac does.** The library offers all three outcomes and makes the cost of each visible. The default is the DAC's own control. A host that wants software gain can have it and loses the bit-perfect claim, with the blocker named. A host that accepts a fixed level says so in `DacConfig` and the library no longer refuses the stream; the sample app shows this as a checkbox. The DAC's volume is also usable before any stream is open (see [decision 10](#volume-writes)).

**Evidence.** The three drivers agree on the main point, and Mornye's README states it as a rule: no software gain on this path. The UAC specifications define the Volume Control of a Feature Unit as a signed 16-bit value in steps of 1/256 dB, readable with CUR and RANGE (UAC 1.0 uses MIN, MAX and RES). usbdac's tests (`volume_control.rs`, `DacVolumeTest.kt`) cover the encoding, range arithmetic, read-back verification and channel rollback on a fake DAC. The tests are host tests against a fake DAC. The recorded hardware result of the library is PCM playback on one phone and one DAC; the DAC's volume control has not been separately confirmed there.

**Trade-off.** Requiring hardware volume protects the bit-perfect claim and means a DAC with no control needs a deliberate choice by the listener. Offering a fixed level means the first sound plays at whatever the DAC's own level is, which is why [decision 11](#startup-volume) exists.

**References.** [UAC 2.0 Volume Control](references.md#ref-uac2), [UAC 1.0 Volume Control](references.md#ref-uac1), [Mornye README](references.md#ref-mornye-commits).

## 10. How a volume slider writes to the DAC {#volume-writes}

**The question.** A listener drags a slider. How are the writes sent, what does the slider position mean, and what happens while a stream is playing or before one exists?

**In plain terms.** A DAC's volume range can run from +0 dB down to minus 127 dB, and the bottom third of that is inaudible, so a slider that is linear in decibels wastes much of its travel. Writes are control transfers that take a few milliseconds, and a drag produces many of them. And while the music plays on the same USB connection, a volume write must not disturb it.

| | What it does | Where |
|---|---|---|
| Flick | Linear in dB over the DAC's full range; the level is written only when the drag ends (PR #416, open, changes both). Each volume step, mute and the 5-second health check opened a second libusb context on the stream's file descriptor, which PR #415 (open) replaces with requests queued to the streaming thread | PRs #415 and #416; `lib/services/uac2_preferences_service.dart` (`defaultUsbDacVolume = 0.40`) |
| Mornye | Linear in dB over the DAC's full range (`minDb + fraction * (maxDb - minDb)`, default minimum -90 dB when the DAC does not say). Requests are queued to the one thread that owns libusb, at most 8 pending, each with a 2 s deadline | `lib/services/music_playback_deck.dart`; `usb_audio/transport.rs` (`set_volume`) |
| BitChord | A floor of -80 dB: zero is the DAC's minimum, anything above spans `max(min, -80 dB)` to the maximum, in 16 steps for keys. Requests are queued to the libusb worker with a deadline. The file's comment says Mornye's linear mapping leaves "the bottom half of that travel inaudible" | `bitperfect/DacVolume.kt` (`FLOOR_DB`, `STEPS`), `usb_transport.cpp` (`ControlRequest`) |
| usbdac | The same curve as BitChord: 0 is the DAC's minimum, above 0 it spans `max(min, -80 dB)` to the maximum, linear in dB. A level below -80 dB is reachable in dB and reads back as 0.0. The Kotlin `VolumeWriteQueue` keeps one write in flight, replaces a pending absolute level with the newest, merges repeated steps (at most 32 presses), and always applies the last value. Writes go to the DAC with a synchronous usbfs control ioctl on the same file descriptor, so no second libusb context exists to collect the stream's completions | `volume_control.rs` (`volume_floor`), `android/usbdac/.../internal/VolumeWriteQueue.kt`, `transport/usbfs.rs` (`feature_unit_control`) |

**What usbdac does.** `DacVolume.setFraction`, `setDb`, `step` and `setMute` return at once and are safe on every slider event. Volume works with no stream: the library finds the Feature Unit through the first PCM playback stream when the DAC opens and keeps a handle for it, so a player can show and set the level before the first track. Every write is read back, and a multi-channel write that fails part-way restores the channels already written.

**Evidence.**

- **Flick PR #416** gives the reason for the floor: many DACs report a minimum near -127 dB, so on those "roughly the bottom third of the slider is inaudible". BitChord's `DacVolume.kt` has the same floor and the same reasoning for Mornye's mapping. The -80 dB figure is a choice that both BitChord and Flick PR #416 make; it was not derived from a measurement of audibility on any DAC.
- **Flick PR #415** is the report of the second-context problem. It says libusb's usbfs backend "reaps completed URBs per file descriptor and does not check which context owns them", so a volume thread with its own context can reap the stream's transfers and the other way round, and that it "works by accident". That description of libusb's behaviour was not checked independently here. usbdac's code comment says the same thing and avoids the problem by not creating a second context for control transfers.
- Mornye and BitChord avoid the problem differently, by putting every control request through the one worker thread that owns libusb. That keeps one reaper, at the cost of a volume write waiting for the worker's loop.
- Tests: `VolumeWriteQueueTest.kt` (a burst of levels collapses to the last one, one write in flight, identical steps merge into a bounded count, an absolute level supersedes earlier steps), `volume_control.rs` (curve round trips, DACs whose minimum is above -80 dB, multi-range DACs). All are host tests.

**Trade-off.** The usbfs control ioctl is synchronous and has a one second timeout, so a DAC that answers slowly can hold one write, and the writes behind it, for that long without ever stalling the stream. Flick PR #415 chose a 20 ms timeout instead and notes the cost: a DAC that takes longer than 20 ms to acknowledge a request then fails the change. Neither value has been measured against a range of DACs. The curve means a slider position is not a fixed number of dB: a host that remembers levels should save dB (the `VolumeState.db` value) and not the 0..1 position, which Flick PR #416 had to convert when it changed the curve.

**References.** [Linux usbfs and libusb isochronous and control I/O](references.md#ref-libusb-async), [Flick #415 and #416](references.md#ref-flick-pr414-418), [BitChord `DacVolume.kt`](references.md#ref-bitchord-port).

## 11. The first sound, and what the DAC is left at {#startup-volume}

**The question.** A DAC keeps the level it had, and many default to full scale. What does the driver do before the first sample, what does it require, and what does it do with the DAC's level when it lets go?

**In plain terms.** Plug in a DAC that was left at maximum, press play, and the first note reaches the headphones at full level. And if the app lowers the DAC's level and never puts it back, every other app that uses the DAC afterwards plays quietly until it is unplugged.

| | What it does | Where |
|---|---|---|
| Flick | The driver does not lower the level at open. The app restores a remembered level or uses a default of 0.40 on its slider curve, which PR #416 puts at about -76 dB on a -127 to 0 dB DAC before its change | `lib/services/uac2_preferences_service.dart`, PR #416 |
| Mornye | Before the first transfer, each channel is lowered to at most -40 dB; a quieter or silent channel is left alone; the read-back must confirm it. A level the user chose is reused between songs on the same attached DAC and forgotten on detach. The README adds: "This is attenuation, not a guarantee of safe acoustic output". When playback ends or has been idle, the DAC goes back to the level it connected with, "never between two songs, where the next one could play through Android's mixer at that higher level"; only an original level above the current one is written, so the app never turns a DAC down further | `usb_audio/README.md`, `usb_audio/volume.rs`, `UsbDirectPlayback.kt` (`handBack`), `UsbDirectFormats.kt` (`levelToHandBack`) |
| BitChord | The same -40 dB at start. The first version never put the level back, so it "carried over into Android's own USB driver: switching to the Android 14 mixer mode or normal output played very quietly" (commit `ea2df7e4`, 2026-10-28), which a restore on release fixed. Issue #1485 then reported the restore being skipped after a failed stream | commits `06628ed1`, `ea2df7e4`; issue #1485 |
| usbdac | At open, every channel is recorded, then lowered to `min(its level, -40 dB)` and verified by read-back, before any stream can start; a channel is never raised. If the write or the verification fails the control counts as unusable. `StartupVolumeReport` returns the original and applied levels and `limit_db` (the quietest original channel), which a host that restores a remembered level should not exceed. On close the original level is written back and verified (`RESTORE_ORIGINAL`, the default), kept (`Keep`) or forgotten (`CloseVolume::Forget`, for a DAC that is gone). The record is per device and per process, so a reopen still knows the true original | `config.rs` (`StartupVolume`, `HandBack`, `CloseVolume`), `volume_control.rs` (`plan_startup_attenuation`), `dac.rs` |

**What usbdac does.** The default `StartupVolume::Attenuate { max_db: -40.0 }` runs once per open and never again: a level the host sets later is never pulled back. `DacConfig.requireHardwareVolume` is the second half. A DAC with no usable control cannot play until the host says it accepts a fixed level, because nothing then lowers the first sound.

**Evidence.** Mornye's README gives the number -40 dB and BitChord inherited it in its port; usbdac uses the same value. Flick's driver does not lower the level at open. BitChord's commit `ea2df7e4` is a direct record of what skipping the hand-back does, and issue #1485 reports it again after a failed stream: "the DAC stays at the app's attenuation (-25 dB in my case, -40 dB at startup) after the fallback. Android's driver and every other app then play through that attenuation until the dongle is power-cycled". usbdac's host tests cover attenuation without raising, quiet and silent channels left alone, independent channels, rounding down to a supported step, a ceiling below the DAC's minimum, failed read-back, and restore after close. **Start-up attenuation and hand-back are covered by host tests only. They have not been confirmed on a DAC, and the Kotlin path in the sample app has not run on a phone.**

**Trade-off.** The ceiling of -40 dB is the number Mornye uses and is not derived from a headphone sensitivity (Mornye says so). A DAC whose volume is already below -40 dB is not raised, so a listener who wants it louder has to raise it. Hand-back means the DAC's volume changes twice per session, and a crash between the lowering and the hand-back leaves it lowered; the library cannot fix that.

**References.** [UAC 2.0 Volume Control](references.md#ref-uac2), [Mornye README](references.md#ref-mornye-commits), [BitChord issue #1485](references.md#ref-bitchord-1485), [BitChord port commit](references.md#ref-bitchord-port).

## 12. The phone's volume buttons, and a DAC that idles {#keys-and-idle}

**The question.** With the system volume disconnected, what do the phone's volume keys do, and what happens to a DAC while the music is paused for a long time?

**In plain terms.** The volume buttons are what people reach for without looking, with the screen off. If they move a system slider that changes nothing, direct USB feels broken even though it is working. And a paused player that keeps streaming silence keeps the USB bus busy and the DAC awake.

| | What it does | Where |
|---|---|---|
| Flick | Today the keys still move the system volume and change nothing audible. PR #417 (open) routes them to the DAC in the foreground and reports the DAC as remote playback with a volume provider in the background. PR #418 (open) closes the stream after 30 s of pause and keeps the DAC claimed | PRs #417, #418 |
| Mornye | "Phone volume buttons may not affect direct USB output". After 5 s without playing, the player ends the native session: the DAC goes back at the level it connected with and is released to Android, and play reloads the song and opens the DAC again. The reason in the commit is not power: "a paused Mornye left every other app without sound through that DAC until it was unplugged" | `usb_audio/README.md`, `lib/services/music_playback_deck.dart` (`usbIdleRelease`), commit `7447af0ee` (2026-11-19, "Not run on a DAC") |
| BitChord | The DAC's level is the player's device volume: the foreground keys go through `dispatchKeyEvent`, and the media session reports a remote playback type with a 0 to 100 range, so the system panel and the background keys drive the DAC. A pause longer than 30 s hands a direct USB or AAudio exclusive device back, and play resumes from the frame that was held | `MainActivity.kt` (`handleDacVolumeKey`), `PlaybackService.kt` (`DAC_DEVICE`), `BitPerfectAudioSink.kt` (`IDLE_RELEASE_MS = 30_000`) |
| usbdac | Three optional helpers in `dev.usbdac.controls`. `DacVolumeKeys.handle(event, volume)` steps the DAC by 1/16 of the curve in a key handler, with key repeat. `DacVolumeProvider` is a `VolumeProvider` for a `MediaSession`, so the system panel and the background keys drive the DAC. `DacIdleRelease` closes the stream after 30 s of pause while the session and the volume stay, and reopens it on play | `android/usbdac/.../controls/*.kt` |

**What usbdac does.** The library does nothing by itself: a player turns each helper on. The keys, the panel and the idle release all go through the same `DacVolume` queue as the slider, so the safety rules of [decision 10](#volume-writes) apply to them.

**Evidence.** The behaviour comes from BitChord and from Flick's PRs #417 and #418, whose descriptions say they were **not run on hardware** ("the background path rests on documented media-session behaviour, not on observation"; "that the dongle actually idles, and how long resume takes after a release, are unmeasured"). Android documents the remote volume provider mechanism, and says a session must call `setPlaybackToRemote` to receive volume button events. usbdac's tests (`ControlsLogicTest.kt`, `DacIdleReleaseTest.kt`) cover the key mapping and the timer on the JVM. **None of the three helpers has been tried on a phone with usbdac.**

**Trade-off.** Android routes the background keys to a session only while it is playing, so with the music paused and the screen off the keys still move the system volume (Flick #417 notes the same). Releasing the stream after 30 s saves power and costs a reopen on resume, which takes at least 100 ms and which nobody has timed on hardware. The 30 s is BitChord's number and Flick's, and Mornye uses 5 s; all of them are judgements. The three differ in what they let go of: Mornye hands the DAC back to Android, so other apps can play through it, while Flick's #418 and usbdac close only the stream and keep the DAC claimed, so that Android's own driver does not take it back and play system sounds through it. A host that wants Mornye's behaviour closes the `DacSession` (which hands the volume back and releases the interfaces) instead of using `DacIdleRelease`.

**References.** [VolumeProvider and MediaSession](references.md#ref-android-volumeprovider), [Flick #417 and #418](references.md#ref-flick-pr414-418), [BitChord port commit](references.md#ref-bitchord-port).
