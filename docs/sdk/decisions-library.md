# Design decisions: outputs and library shape

Three decisions about what makes usbdac a library and not part of a player: who takes the DAC away from Android, how an app gets the best output without knowing the situation, and what the library limits itself to. The other three drivers are written for one app each, so on this page "what they do" is often "the app does it".

The parts of each decision are the question, what the three drivers do, what usbdac does, the evidence, the trade-off and the public references. Revisions are defined on [the drivers page](drivers.md#revisions-studied); the [overview of all decisions](decisions.md) has a summary table.

## 16. Who takes the DAC from Android, and how it is given back {#claiming}

**The question.** Android's own USB audio driver normally owns a USB DAC. To drive it through usbfs, something has to detach that driver and claim the interfaces. Does the library do that, or the host, and how is the DAC returned?

**In plain terms.** If the claim fails, nothing else works, and if the release is wrong, the DAC stays silent for every other app until it is replugged. The phone also holds the USB permission, so the app has to be involved at some point.

| | What it does | Where |
|---|---|---|
| Flick | The Kotlin side force-claims the AudioControl and the playback AudioStreaming interfaces with `UsbDeviceConnection.claimInterface(interface, true)` and tells Rust which are claimed, so Rust does not claim them again. On release it resets a streaming interface to alternate setting 0 and calls `releaseInterface`, with the comment "Unlike close(), releaseInterface also asks Android to rebind the driver" | `android/app/src/main/kotlin/com/mossapps/flick/MainActivity.kt`, `DirectUsbInterfaceClaims.kt` |
| Mornye | libusb does it: the device is wrapped from the file descriptor, `libusb_set_auto_detach_kernel_driver` is on, and the control and streaming interfaces are claimed with `libusb_claim_interface`. The README says interfaces are "released and the kernel driver reattached automatically", and that the worker is joined before Android closes the original descriptor | `usb_audio/transport.rs` (`Session::open`, `configure`, `release`) |
| BitChord | The same: auto-detach and `libusb_claim_interface` in the C++ worker | `usb_transport.cpp` (`Session`) |
| usbdac | The Kotlin SDK claims, as Flick does: `claimInterface(interface, force = true)` for every AudioControl interface and every playback AudioStreaming interface of the active configuration, all or none, control first. The Rust core is told which interfaces are claimed (`DeviceInfo::with_claimed_interfaces`) and refuses a control request on an interface that is not. `DacSession.close()` resets streaming interfaces to alternate setting 0 and releases them, which asks Android to rebind its driver | `android/usbdac/.../internal/UsbInterfaces.kt` (`planClaims`, `ClaimedInterfaces`), `transport/usbfs.rs` |

**What usbdac does.** The library's Rust core stays free of Android: it takes a file descriptor and a list of claimed interfaces and checks that the list covers what it uses. Everything Android-specific (permission, claim, release) is in the Kotlin layer, which does it in the order that was found to work on hardware. A host that uses the Rust crate directly has to do the same, and the README says so.

**Evidence.**

- Flick PR #400 gives the two failures on the OnePlus 15 and iBasso DC04U that led to a Java-side force claim: libusb's `kernel_driver_active()` "reports false on a wrapped Android fd even when `snd-usb-audio` owns the interface", so the detach has to be attempted regardless, and Android's USB audio HAL held the AudioControl interface, so `SET_CUR` returned an input/output error and the clock could not be verified. It also records that a second claim from libusb on an interface already claimed from Java returned `EBUSY`, which is why the claimed list is passed down.
- **A discrepancy that matters.** The description of Flick PR #400 still says to claim only the AudioControl interface and to leave the streaming interface to libusb, because a Java claim of the streaming interface made `set_alt_setting` fail with `ENOENT` on the DC4U. The pull request's last commit (`69cb3c50`, "Finalize USB ownership and reconnect") removed that paragraph from Flick's routing document, and the merged Flick code claims both kinds from Java. usbdac follows the merged code. The hardware checks listed in the pull request (cold start to direct USB, five minutes without a dropout, 48 and 96 kHz, pause, seek, unplug and replug) were made on the earlier `49ceb71e` build. Whether the merged claim of streaming interfaces works on that phone is not recorded, and usbdac's Kotlin claim path has not been run on any phone.
- Mornye's approach is the one libusb documents for Android: `libusb_wrap_sys_device` on the descriptor Android handed over, with `LIBUSB_OPTION_NO_DEVICE_DISCOVERY`. It is simpler, and whether it is enough depends on the phone. This project has no run of Mornye's or BitChord's claim path on the OnePlus 15, and no claim of Mornye's on any other phone.
- Android's documentation covers `claimInterface(intf, force)` ("true to disconnect kernel driver if necessary") and `getFileDescriptor()` ("intended for passing to native code"). It lists isochronous endpoints as "currently not supported" in `UsbConstants`, which is why a driver for isochronous audio has to use usbfs through libusb. The `usbdac` sample app has not been run on a phone (see [status](evidence.md#hardware-status)); the claim code is the same pattern as Flick's.

**Trade-off.** A Java-side force claim needs Android's USB host API and a connection that stays open for the life of the session, and the "Disable USB audio routing" developer option is the fallback that the error message suggests when Android keeps the DAC. Putting the claim in the SDK keeps apps from having to learn that order, and costs the Rust-only user the same learning.

**References.** [Android USB host](references.md#ref-android-usb-host), [`UsbConstants`](references.md#ref-android-usb-host), [libusb `wrap_sys_device`](references.md#ref-libusb-async), [Flick #400](references.md#ref-flick-pr400).

## 17. Three outputs, and one that chooses {#outputs}

**The question.** An app on a phone with a USB DAC, a digital audio player with an internal DAC, or a phone on Bluetooth wants "the best way to play this". Which outputs exist, and who decides?

**In plain terms.** There are three ways to reach a DAC from an app. Direct USB drives the DAC itself and can verify what it sends. On Android 14 and later the app can ask the platform for a bit-perfect mixer mode for a USB device, which Android enforces and the app cannot check. And an exclusive AAudio stream asks the platform to bypass the app mixer, which suits players with a built-in DAC. Each has different guarantees, and a wrong choice either wastes a better option or claims a guarantee that does not exist.

| | What it does | Where |
|---|---|---|
| Flick | Direct USB (an engine mode selected by user settings and by whether a DAC is claimed), and Oboe/AAudio exclusive for players with an internal DAC. After the claim Android stops reporting a USB route, so PR #400 had to make the engine selection follow the claimed DAC | `AGENTS.md`, `docs/UAC2_EXCLUSIVE_ROUTING.md`, PR #400 |
| Mornye | Three modes the listener chooses in settings: Direct USB (opt-in), the Android 14 preferred mixer path, and "DAP hi-res (AAudio exclusive)" through Oboe. Permission denied or no hardware leaves PCM on normal output; a failure after direct-USB preparation never redirects to the speaker | `usb_audio/README.md` |
| BitChord | A routing sink decides per track, at every `configure`: a track the exclusive output can carry unchanged goes there, anything else takes the normal path. If the exclusive output fails the stream continues on the normal path from the next buffer. Hand-over between sinks waits for the outgoing one to finish | `bitperfect/BitPerfectRoutingSink.kt`, `BitPerfectTransport.kt` |
| usbdac | One interface (`PcmOutput`) for the three outputs and an `OutputSelector` that returns the first of direct USB, Android's bit-perfect mixer, exclusive AAudio, and the standard output that works for the request. The result says which `Guarantee` applies (`VERIFIED_BY_LIBRARY`, `REQUESTED_FROM_PLATFORM`, `EXCLUSIVE_GRANTED_UNVERIFIABLE`, `NONE`) and lists every better output that was passed over with a typed reason. It never asks for USB permission, never switches a stream that is playing, and reports changes through `changes` so the app can look again at a track boundary | `android/usbdac/.../OutputSelector.kt`, `OutputSelection.kt`, `PcmOutput.kt`, [automatic output](automatic-output.md) |

**What usbdac does.** The decision that has to be the listener's, whether the app may take the DAC away from other apps, is a policy field (`allowDirectUsb`) and not something the library chooses. A player with an internal DAC is recognised by what the platform grants an exclusive stream for the request, and not by a list of device names. The two outputs that pass through Android never report `Verified`.

**Evidence.**

- All three drivers have the three outputs and let the app or the listener choose, which is the problem the selector turns into one call. BitChord's per-track routing and fall-through, and Mornye's rule that a failed direct stream never goes to the speaker, are both reflected: the selector chooses once per open, and a failed output is the app's to handle, as [the integration guide](integration-app.md#failures-and-fallback) describes.
- Mornye's README states the limit on exclusive streams, "Android may grant Shared when Exclusive was requested", and Android documents the same: the requested sharing mode "may not be available" and the app should query the actual mode. usbdac's AAudio output reads the grant and reports it. The `AudioMixerAttributes` bit-perfect behaviour is Android 14 and later, limited to USB devices and media usage, and optional in the HAL, so the selector asks `getSupportedMixerAttributes` first.
- The selector's decision table is covered by JVM tests against a fake device and a fake native layer (`OutputSelectorTest.kt`, about 900 lines). **Nothing in the selector, the managed output or the AAudio output has run on hardware.** In particular, whether a given digital audio player grants an exclusive stream, and what a `select` with a DAC attached costs on a real DAC (it opens a session, programs the clock, and closes it again), are not known.

**Trade-off.** One interface hides differences that matter: direct USB has hardware volume and DSD, and the others do not, so `PcmOutput` is the common part and the rest is reached through the `SelectedOutput`. The selector looks before it chooses, which costs 100 ms or more with a DAC attached, so it is for the moment before a track and not for a render loop.

**References.** [AAudio sharing mode](references.md#ref-android-aaudio), [`AudioMixerAttributes` and the preferred mixer](references.md#ref-android-mixer), [`AudioFormat` encodings](references.md#ref-android-audioformat), [Mornye README](references.md#ref-mornye-commits).

## 18. What the library limits itself to {#scope}

**The question.** How many DACs at once, which USB Audio Class versions, which direction, and how does the library tell an app what happened without disturbing the audio?

**In plain terms.** A phone has one USB port in practice, and the claimed interfaces, the file descriptor and the output thread each have one owner. Keeping to that makes the code smaller and the rules clearer. The audio thread must also never wait on the app: a slow event handler cannot be allowed to stall the music.

| | What it does | Where |
|---|---|---|
| Flick | One direct USB session in process-wide state; diagnostics are polled by the Dart side | `rust/src/uac2/android_direct.rs`, `lib/services/uac2_service.dart` |
| Mornye | One `Output` with one worker thread owning libusb; a failure sets an error string that the next call returns. UAC 1.0 and 2.0 playback; "multiple configurations fail closed" | `usb_audio/transport.rs`, `README.md` |
| BitChord | One `Output` and worker in C++; "only UAC1/UAC2 Type I mono or stereo output is accepted; ... multiple configurations are rejected rather than guessed at" | `UsbDescriptors.kt` |
| usbdac | One open DAC per process (`Error::Busy` for a second); UAC 1.0 and 2.0, playback only, no UAC 3.0. Every event is delivered by one library thread (`usbdac-events`), in order, through a bounded queue with a non-blocking send, never while the library holds a lock: if the queue overflows, new events are dropped and logged. The output thread asks for `SCHED_FIFO` priority 2 and falls back to nice -16. The Kotlin API turns events into a `SharedFlow` with a buffer of 64 | `transport/mod.rs`, `events.rs`, `priority.rs`, `error.rs` |

**What usbdac does.** The process-wide session lives in `SESSION_STATE` inside the transport and does not leak into the public types, so lifting the limit later does not change the API. Errors have stable codes (`Error::code()`, `DacErrorCode` in Kotlin), configuration has documented defaults and is validated (`DacConfig::validate`), and the native API is a handle-based JNI layer with JSON for the structured values ([native API](native-api.html#10-jni-contract)). The library depends on no player: a host with a render callback, a pull loop or a Media3 sink writes the same way.

**Evidence.** The single-session rule matches what the other three do, and the transport module's own comment gives the reason: "one DAC at a time is the reality on a phone". The event rule comes from a real risk in the drivers that report by polling or by return value: a slow collector must not stall the transfer loop. The tests in `events.rs` check that events arrive in order on `usbdac-events`, that a listener that panics does not stop delivery, and that closing the session from inside a listener does not deadlock. The drop-on-overflow rule is in the code and its documentation; I found no test for it. Real-time scheduling is best-effort and logged either way; whether it is granted depends on the phone. **The behaviour of two DACs on one phone (for example through a hub) was not tested.**

**Trade-off.** No second DAC, no capture, no multichannel beyond what the DAC's alternate settings give, no UAC 3.0 and no implicit feedback. Each is a feature that can be added and none is a design dead end except the process-wide state, which would have to be replaced to allow a second DAC. Dropping events on overflow means a host can miss an event; the state it needs (`StreamState`, `BitPerfectState`, volume) is also available as a value to read, and `LostFrames` counts are in `StreamStats`.

**References.** [UAC 1.0](references.md#ref-uac1), [UAC 2.0](references.md#ref-uac2), [libusb event handling](references.md#ref-libusb-async), [Android USB host](references.md#ref-android-usb-host).
