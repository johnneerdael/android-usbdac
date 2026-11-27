# ALSA parity design and implementation ledger

Baseline `de8010bb427e3cc9184c2acc6cb084dcfc3ee719` (usbdac 0.1.1). Compare USB-IF protocol requirements, upstream ALSA behavior and the userspace limits of Android usbfs. Build independent MIT code and record hardware compatibility as evidence-backed data. Source baseline for Linux comparisons is v7.0, commit `028ef9c96e96197026887c0f092424679298aae8`.

## Work and acceptance order

Finish the shared USB Audio work before adding more device-specific
compatibility. Use the following order for the remaining work.

1. Verify shared explicit and implicit feedback, including startup,
   packet capacity, delayed completions, flush, detach and diagnostics.
2. Audit standard clock and routing controls, capture, duplex and
   channel layout across UAC1, UAC2 and UAC3. Verify readback, alternate
   changes and restoration on failures.
3. Verify full UAC3 and BADD transport, including configuration and
   power-domain hand-back. Resolve optional burst admission against
   descriptor or hardware evidence.
4. Check the shared native DSD and DoP wire paths and PCM hand-back.
5. Resume model-specific formats, clock commands, routing and firmware
   rules after the shared implementation and automated checks pass.
6. Run the physical acceptance matrix after the software work is complete,
   including the additional desktop DACs.

| Area | usbdac 0.1.1 | Implementation target | Acceptance evidence |
|---|---|---|---|
| Native DSD | Native and DoP packetization exist. Discovery truncates UAC2 format bits to 16 and applies current byte/bit order quirks globally. | Exact built-in device and firmware rules, endpoint-local native layouts, raw Type I detection, and typed known-device mode transitions. Preserve independently usable PCM and DoP alternatives. | Compare format negotiation and emitted wire bytes across DSD word and URB boundaries. Verify matching and nonmatching revisions, hardware mode transitions and PCM restoration. |
| Clock selection | Walks current selector pin with a fallback clock for incomplete topology. Implicit feedback already requires proven shared topology. | Explicit source policy and bounded ALSA-style alternative search. Keep a valid selected route. Probe only writable, verified selectors and revalidate rate, source and duplex relation. | Control transcript for unchanged valid path, invalid current source with valid alternative, read-only and nested selectors, rollback, selector change between negotiate and open. |
| Multichannel playback and capture | Negotiates multichannel playback by count. Capture metadata exists for implicit pacing and has no public input stream. | Channel identity and layout, an independently negotiated capture API, and duplex ownership of a shared clock and capture endpoint. | Channel-specific PCM vectors, independent and duplex rates, pause/flush/stop/unplug and loss/latency diagnostics. |
| Hardware routing | Feature Unit volume and mute. | UAC entity graph and class-specific selector/mixer controls first. Follow with exact-model hardware mixer drivers for protocol families with corroborated control and firmware evidence. | Readback and rollback of routes, unsupported controls typed, and per-device routing matrix and physical behavior on tested hardware. |
| UAC3 | UAC1/UAC2 parsers. Other protocols can currently encounter UAC1 fallback logic. | Protocol-aware discovery, BADD profiles and full ADC3. Implement High Capability clusters, control width, power domains, configuration handling, USB descriptor safety and the shared playback/capture runtime. | FS/HS and profile descriptor/control validation. Real BADD and full ADC3 playback and capture across suspend, resume and detach. |

Maintain an evidence list for each built-in quirk with device/revision range, interface or alternate evidence, affected direction and encoding, source and tested state. Built-ins carry defaults for qualifying devices. Explicit host overrides remain available for new revisions and tests. A shared chipset or a product-name substring alone has insufficient specificity to enable a vendor write.

## Native DSD compatibility

ALSA separates three concerns. A raw transport flag indicates a possible wire format, a per-alternate compatibility result assigns an actual DSD width and byte/bit order, and a small set of device families need a mode command before the alternate activates. ALSA also separates DoP framing from direct one-bit transport. Current usbdac reads a shortened format value and equates a private DSD tag with that value. In standard UAC2 Type I, the raw flag is in the high bit and low bit 3 denotes A-law. Keep raw flags and explicit format representations internally. Treat raw capability as evidence requiring a known mapping or explicit host policy, and reserve low numeric tags for their documented protocol meanings. [USB Audio 2.0](https://usb.org/sites/default/files/Audio2_with_Errata_and_ECN_through_Sep_14_2026.pdf), [ALSA format parser](https://github.com/torvalds/linux/blob/028ef9c96e96197026887c0f092424679298aae8/sound/usb/format.c#L36-L82), [ALSA DSD compatibility](https://github.com/torvalds/linux/blob/028ef9c96e96197026887c0f092424679298aae8/sound/usb/quirks.c#L1976-L2082)

**First batch.** Add an internal native wire profile carrying bytes per word, byte order, bit order and the descriptor/quirk origin. Resolve with actual active device VID/PID and USB `bcdDevice` from libusb, plus the chosen playback alternate and geometry. The first built-in records include Amanero `16d0:071a` alt 2 with revision `0199` LE and `019b/0203` BE, `16d0:0a23` alt 2 BE, and the upstream exact VID/PID entries which specify alt 2 or 3 and 32-bit BE. Use a unique eligible playback alternate if a source record supplies the alt number but omits the interface. Preserve the existing Dawn Pro rule and explicit host overrides. Do not assign a guessed Amanero layout to an unlisted revision. Infer capabilities and select playback through the same native profile resolver. Keep advertised PCM usable where the alternate genuinely supports both. Apply transforms exactly once in the encoder. Keep profile decisions immutable for the opened stream and use the same wire sample cadence for clock requests and explicit feedback capacity. [ALSA DSD compatibility](https://github.com/torvalds/linux/blob/028ef9c96e96197026887c0f092424679298aae8/sound/usb/quirks.c#L1982-L2082)

**Follow-on built-ins.** Audit Playback Designs per-alt DoP, bit-reversed eight- and sixteen-bit native formats, ALSA's device-scoped RAW_DATA profiles and ITF vendor mode families. Use typed mode recipes with explicit target shape, zero-bandwidth precondition, verified result when a readback exists and PCM recovery on failure/close. Keep mode recipes inactive on sibling firmware and unrelated interfaces. Exercise DSD-to-PCM, PCM-to-DSD, failed open and detach. [ALSA mode switch](https://github.com/torvalds/linux/blob/028ef9c96e96197026887c0f092424679298aae8/sound/usb/quirks.c#L1888-L1920)

## Clock selection and shared ownership

Resolve a terminal's graph under the selected AudioControl function. A reachable source path includes the selector ID and pin at each branch, multiplier ratio where known and source identity. Extend planning to enumerate alternatives with path-local cycle detection and reject invalid IDs, excessive depth and unsupported ratios. Prefer the current route. Probe its validity and sampling rate through the actual source. For alternative selection, honor the selector's readable/writable flags and verify each 1-based selector value through GET_CUR after SET_CUR. Leave valid paths untouched and keep changes transactional. Save original pins, validate after committing a path, restore on failures and at hand-back where safe, and invalidate negotiation if the path changes unexpectedly. Keep separate support for the existing malformed-descriptor fallback, which cannot prove a selectable source or duplex clock equality. [UAC2](https://usb.org/sites/default/files/Audio2_with_Errata_and_ECN_through_Sep_14_2026.pdf), [ALSA clock selection](https://github.com/torvalds/linux/blob/028ef9c96e96197026887c0f092424679298aae8/sound/usb/clock.c#L329-L435)

Capture and implicit feedback can share one IN endpoint only when they use the same alternate and wire format. The session owns one event pump and one transfer pool per active endpoint. Fan out packet metadata to feedback and actual bytes to an explicitly opened capture stream. Give the reader a bounded whole-frame buffer, capture position and overrun telemetry. A paused or slow capture caller cannot stall feedback or playback. A requested duplex plan must declare whether it shares a clock. Accept independent clock domains if the app independently clocks and consumes both streams. Refuse concurrent requests that demand conflicting alts or clock settings. Unit contracts must distinguish zero/short successful capture packets, transport loss and dropped host-side backlog. Keep Android permissions and optional capture claims with the existing connection owner.

### Live AudioStreaming alternate admission

After clock setup, read the standard Valid Alternate Settings control
when the selected UAC2 or full ADC3 AudioStreaming descriptor advertises
it. Check the bitmap against that USB interface and require the selected
alternate to be valid before sending SET_INTERFACE. If the interface
also advertises Active Alternate Setting, read it back before starting
transfers. Alternate zero remains available for cleanup after control
failure. Negotiation still selects from static candidates. Reselecting an
alternate after a live rejection would require renegotiating the exposed
format and endpoint geometry. [USB-IF UAC2 §5.2.6.1](https://www.usb.org/sites/default/files/Audio2_with_Errata_and_ECN_through_Sep_14_2026.pdf), [USB-IF ADC3 §5.2.2.1 via IEC](https://www.iteh.eu/catalog/standards/clc/777313ac-7790-430c-8e99-17c611c46dd2/en-iec-62680-1-5-2019)

For hardware acceptance, capture the Valid Alternate Settings read after
clock selection and the Active Alternate Setting read after activation
on a device that advertises both. Exercise an unavailable alternate and
a failed readback, then verify that the stream submits no transfers and
cleanup returns the interface to alternate zero.

Rust workspace tests, the Android arm64 target, Kotlin unit tests, and
the four-ABI AAR and sample builds passed. Both packages contain the
same native library per ABI. The AAR SHA-256 is
`c81fef47bdda8dc448169f755d82ed50ac10d6a5456308d2f34c51dd53272453`.
Physical alternate-control validation remains pending.

## Channel layout and hardware routing

Expose descriptor channel labels alongside count and preserve wire order in capture and playback. Distinguish spatial positions from unnamed channels. Treat unsupported conversion requests as typed failures. Model class selectors and mixer crosspoints as typed nodes and writable edges. Probe control readability and writability before exposing them, use protocol-specific CUR/RANGE encodings, verify writes and roll back partially applied route sets where supported. Device topology and vendor routing are distinct capabilities. Start vendor integration with known device/firmware identities and read-only route discovery, then implement verified changes for supported models. ALSA's Scarlett Gen 2–4 families encode generation/model differences, hardware ports, mixer/mux data and control transactions. Plan per-family implementations with exact evidence rather than forwarding arbitrary messages through the public API. [ALSA USB mixer](https://github.com/torvalds/linux/blob/028ef9c96e96197026887c0f092424679298aae8/sound/usb/mixer.c), [ALSA Scarlett controls](https://github.com/torvalds/linux/blob/028ef9c96e96197026887c0f092424679298aae8/sound/usb/mixer_scarlett2.c)

## USB Audio Class 3

Select UAC3 from the interface protocol and the Audio Interface Association profile. Keep explicit protocol dispatch in class parsers and control addressing. BADD is a separate configuration/profile contract with inferred topology and a fixed 48 kHz stream profile. Match the documented endpoint direction, sync and packet capacity before inferring width and channels. Full ADC3 requires parsing its 64-bit format mask and fetching bounded High Capability descriptor bodies by cluster ID through the AudioControl interface. Validate declared lengths and IDs on both reads and derive channel order from cluster segments. Inspect existing mixed-configuration Android ownership before any SET_CONFIGURATION. Recompute claims and capabilities if configuration changes. [USB-IF ADC3 via IEC 62680-1-5](https://www.iteh.eu/catalog/standards/clc/777313ac-7790-430c-8e99-17c611c46dd2/en-iec-62680-1-5-2019), [USB-IF BADD via IEC 62680-1-6](https://cdn.standards.iteh.ai/samples/102576/dceddfdc0b3e48879b48480be81b1805/IEC-62680-1-6-2019.pdf), [ALSA ADC3 stream parsing](https://github.com/torvalds/linux/blob/028ef9c96e96197026887c0f092424679298aae8/sound/usb/stream.c#L872-L1099)

Track the power domain associated with each active entity and reference-count shared domains. Bring a stopped domain to D0, honor specified recovery time, then verify control and streaming state. Keep it awake while a stream or active control holds it. Shut down after the final owner and after retiring transfers, including abort and device hand-back. Preserve UAC1/UAC2 paths through existing CI and verify actual Android configuration and power transitions before stating hardware coverage. [ALSA UAC3 declarations](https://github.com/torvalds/linux/blob/028ef9c96e96197026887c0f092424679298aae8/include/linux/usb/audio-v3.h), [ALSA power-domain control](https://github.com/torvalds/linux/blob/028ef9c96e96197026887c0f092424679298aae8/sound/usb/power.c)

### Full ADC3 alternate sample-rate limits

An AS Valid Frequency Range descriptor limits one streaming alternate
in wire Hz. Intersect its inclusive bounds with the clock source's
supported rates after applying any clock multiplier. Preserve the
clock's rate steps and gaps between descriptor ranges. Several
descriptors form a union, and omission leaves the source rates
unrestricted by the alternate. Malformed bounds make the alternate
ineligible. BADD uses its profile's fixed rate. [USB-IF ADC3 §4.7.3 via IEC](https://webstore.iec.ch/en/publication/63921)

Playback and capture use these limits for negotiation and clock
configuration. Implicit feedback also requires the selected IN
alternate to support the output wire rate. Each independent duplex
direction checks its own alternate. The live Valid Alternate Settings
control provides a further check at activation when advertised.
When one feedback endpoint offers several IN alternates, the current
driver retains one compatible IN alternate per playback route. If
those IN alternates support different rates, negotiation can still
miss a rate that a different IN alternate would support.

For physical acceptance, use two alternates on one clock with
different declared ranges. Check that discovery lists the correct
rates and a request selects an eligible alternate. Repeat for capture
and duplex, including a rate the playback alternate supports that its
implicit feedback alternate excludes.

Rust workspace tests, the Android arm64 target check and the four-ABI
AAR and sample builds passed. Each ABI has identical native bytes in
the AAR and sample. The AAR SHA-256 is
`cec2cea497dd9d665df8b0a44ed367ed1635aa0e3b8713ad59595dd2f59423eb`.

## Transport integration sequence

1. **DSD wire formats and identity.** Establish one resolver shared by capabilities and negotiation. Record a transport representation independently of the USB encoding bitmap. Raise `Unsupported` before opening an alternate if a raw layout is ambiguous. Install reviewed built-in profiles first. Validate existing nominal/feedback paths and separate exact protocol verification from physical playability.
2. **Clock planning and commit.** Keep graph planning pure and transport control callbacks serialized with the USB session. Key the selected route by active configuration, AudioControl interface, terminal, and every selector pin. Planning during capability discovery issues read-only operations. Negotiation can try a route after holding the required claims and quiescing owned streaming alternates. Open either confirms the negotiated route or reports renegotiation. For duplex, validate both terminal routes against the intended pair before changing a shared selector. Restore the previous configuration on failed attempts. Do not let a fallback-first source authorize a selector change.
3. **ADC3 parser and control integration.** Land bounded descriptor and BADD decoding independently. Integrate protocol classification before extending clocks and volumes. Introduce one internal protocol-aware request builder with explicit recipient, entity and control width, then route UAC3 discovery and negotiated formats through it. Let the host take an explicit configuration choice before claiming interfaces. At baseline Android selects the current configuration only, so report a selectable BADD profile separately from a ready direct route.
4. **Capture and channel maps.** Extend the negotiated plan with direction and ordered channel layout. Keep the existing `Dac::open_stream` and Android `DacSession.openStream` as playback entry points. Add a capture request/format, `DacCaptureStream` and a paired duplex-open operation. A duplex open selects both sides before activating either alt. Device positions and loss are directional. Keep one worker holding the USB handle so a callback from one direction cannot outlive buffers owned by the other.
5. **Routing.** Add a Rust route descriptor and typed control operation, then transport class-specific reads, and then explicit writes. Surface route control through JNI and Kotlin once the USB transaction differs correctly by protocol. Add built-in Scarlett families with verified route discovery first. Ship writable model rules only where expected request/response geometry, valid node IDs, and hardware rollback are known.

### Capture contract

- Capture negotiates linear PCM in the device's actual format, with channel order and labels available whenever descriptors provide them. UAC1 obtains its map from the terminal channel configuration, UAC2 from the AS channel cluster, and ADC3 from the High Capability cluster. Expose unknown labels with their stable channel index. Initial capture requests use exact rates and preserve the device's wire layout. Exposing float, raw DSD, or converter behavior needs a distinct implemented encoding contract.
- `read` fills a caller-owned buffer with a whole number of interleaved frames. Return zero when the stream is active and no data is ready. Offer available-frame and captured/delivered/overrun counters and a device-position estimate. Distinguish stopped from failed, and partial samples from a genuine empty service packet.
- One existing IN pool supplies implicit pacing and callers when the plans match. A capture read stall drops the oldest full frames in the capture queue and reports overrun. It must not stall OUT or implicit timing. An unrequested capture stream consumes packet metadata only. A capture-only open must be explicit and must not depend on discovery of a playback interface. Claim its input interface within the existing USB permission flow, leaving the playback-oriented Android output selector unchanged.
- Independent duplex clocks expose separate rates and positions. Never convert an independent capture clock into playback pacing. Keep capture-only stream state separate from playback bit-perfect assessment. Route and clock operations which can move an active stream to a different clock or channel map require a stop/reconfigure transition.
- Preserve requested channel identities across a format close/open. Assert known vectors channel by channel across 16/24/32-bit PCM, padded subslots and packet batches. Add independent loss and position accounting at the transport boundary. Test full-size, short valid, malformed, failed and detached capture packets using the same documented lifecycle as output.

### Built-in coverage and evidence

Use the common identity matcher for exact VID/PID, optional product constraint and firmware range, with per-rule interface/alternate geometry. A known shared family can carry multiple records, each with a separately auditable match condition. Record the ALSA source reference and any device report or descriptor capture for each family. Tests cover adjacent firmware, wrong interface, wrong alternate, ambiguous descriptors and installation of an override. Write mode or hardware-routing requests only for active rules with a typed recipe.

Useful initial families include XMOS 32-bit native profiles, firmware-specific Amanero Combo384, ITF mode switching for upstream-listed TEAC/Esoteric, Denon/Marantz and Luxman devices, and Playback Designs raw/DoP profiles. Scarlett controls form a distinct device protocol family and require generation- and model-specific routing facts. Review device and protocol additions as connected batches. Release claims should identify which models are empirically confirmed and which rely on source-backed compatibility. [ALSA DSD layouts and mode families](https://github.com/torvalds/linux/blob/028ef9c96e96197026887c0f092424679298aae8/sound/usb/quirks.c#L1888-L2082), [ALSA Scarlett model map](https://github.com/torvalds/linux/blob/028ef9c96e96197026887c0f092424679298aae8/sound/usb/mixer_scarlett2.c#L2233-L2269)

### Mode changes and Android capture permissions

Run a native vendor mode change only after the selected streaming interface is at its stop alternate and the stream's claims have been verified. Capture no USB audio during discovery. A profile describes the command recipient, supported format/alternate combinations and required settle interval. In the ALSA ITF family, `snd_usb_select_mode_quirk` switches the interface to alt 0 and waits before applying a vendor-interface request. Its PCM and native DSD values differ. Copying the alternate inference without the control transition would advertise unusable DSD. Capture the resulting status and restore the previous or known PCM-safe mode before hand-back where the identified recipe supports it. A return value that only acknowledges the request supplies weaker evidence than actual mode readback or a measured waveform. [ALSA ITF mode transition](https://github.com/torvalds/linux/blob/028ef9c96e96197026887c0f092424679298aae8/sound/usb/quirks.c#L1888-L1920)

Android's USB permission gate also considers microphone privacy for devices that report audio capture. Treat a platform denial or revoked connection as unavailable and retire capture and duplex state through the common teardown path. Direct USB raw input and Android-managed microphone input use different APIs. Keep microphone capture explicit in the app and respect the platform's USB permission decision. [Android USB host permissions](https://developer.android.com/develop/connectivity/usb/host), [AOSP USB permission checks](https://android.googlesource.com/platform/frameworks/base/+/6acb1cfda8d2/services/usb/java/com/android/server/usb/UsbUserPermissionManager.java)

## First validation checkpoint

Native DSD negotiation and encoding now use an endpoint-local wire profile, including the exact native layouts recorded in Linux and the guarded Playback Designs and RAW_DATA families. UAC2 playback negotiation can switch a verified, writable selector when the current source cannot serve the request and the session owns the affected interfaces. Normal open revalidates the selected route. The ADC3 parser, capture frame buffer, and UAC2 routing transaction primitives are independently testable foundations.

Rust workspace tests, the Android target check, four-ABI AAR and sample build, Kotlin unit tests and SDK snippets passed. Device playback, automatic selector writes, capture, ADC3 power transitions and hardware routing still require real-device acceptance. Capture and routing primitives need an owning USB session and public Android entry points. ADC3 needs its descriptor/control and power paths connected to transport. ITF recipes remain inactive until their mode transitions and PCM restoration run through stream ownership.

### ADC3 transport contract for the next integration

- Identify the AudioControl interface from the same ADC3 IAD as the selected AudioStreaming alternate. Profile `0x01` requires the declared AS General, terminal and HC cluster. BADD profiles `0x20..=0x26` infer the mandatory profile layout from endpoint geometry. Unknown profile, malformed descriptors, conflicting association and unsupported format are distinct refusal reasons. Reject ADC3 at the legacy UAC1 fallback boundary.
- For full ADC3, send class/interface IN request `0x06` with the 16-bit HC descriptor ID in `wValue` and owning AudioControl interface in `wIndex`. Retrieve and validate the fixed header, then request the bounded advertised length. Match response type, descriptor ID and length in the body before using its ordered channels. Keep the route shape and configuration identity with the negotiated format.
- Validate clock and volume capabilities from ADC3 control bitmaps at their correct width. Frequency CUR/RANGE for a full ADC3 Clock Source uses the class/interface entity address and sample-frequency selector. BADD is a fixed 48 kHz path and should skip frequency SET_CUR. Parse full ADC3 Feature Unit channel controls as 32-bit words with a trailing 16-bit string ID. For BADD, infer Feature Units 2 and 5 only for profile-conforming playback and capture paths, and account for the distinct sidetone unit on relevant profiles. Preserve PCM eligibility on explicit or synchronous routes independently of future vendor raw-format profiles.
- Power-domain control is one-byte CUR. Address the domain entity in the high byte of `wIndex` and the owning AudioControl interface in the low byte. Read and validate the current D0/D1/D2 value before writing. Wake to D0 before controls and stream activation. For D1→D0 and D2→D0, wait the corresponding descriptor recovery count in 50 µs units, then read back. Track shared domains by configuration/AudioControl/domain ID. Restore the prior state after the last owned endpoint retires. An unknown state or failed wake refuses start.
- Cover an ADC3 active configuration without changing configuration first. Then integrate explicit configuration selection and Android claim rollback, then test BADD and full ADC3 hardware. Do not report a route ready solely because another configuration exposes a valid ADC3 profile.

[ALSA ADC3 stream discovery](https://github.com/torvalds/linux/blob/028ef9c96e96197026887c0f092424679298aae8/sound/usb/stream.c#L872-L1099), [ALSA ADC3 frequency handling](https://github.com/torvalds/linux/blob/028ef9c96e96197026887c0f092424679298aae8/sound/usb/clock.c#L660-L688), [ALSA ADC3 power handling](https://github.com/torvalds/linux/blob/028ef9c96e96197026887c0f092424679298aae8/sound/usb/power.c#L53-L108)

### Scarlett routing compatibility

See the [Scarlett implementation handover](scarlett-routing-follow-on.md) for the adapter phases. Keep vendor routing behind exact supported identities and firmware-capability checks. ALSA's v7.0 table distinguishes Scarlett Gen 2, 3 and 4, Vocaster, and Clarett USB/Clarett+. Their port counts, source IDs and mux assignments differ. The protocol adapter owns a serial command sequence, bounded response lengths and the one documented initialization sequence exception. Verify command, sequence, payload size and device error before interpreting a reply. [ALSA model data and packet validation](https://github.com/torvalds/linux/blob/028ef9c96e96197026887c0f092424679298aae8/sound/usb/mixer_scarlett2.c#L2233-L2470)

Treat the mux as ordered destination/source pairs. Build rate-family table layouts from the model profile. Upstream reads the route map and writes model-specific assignments for each rate-family table. A generic class mixer API cannot stand in for these operations. Before enabling a vendor route write, prove that the observed original state is sufficient to restore any table touched by it. Enable read-only discovery when the firmware or table mapping is unverified. A Gen 4 device map can corroborate the port model after bounded decode and exact identity match. Expose typed routing only through the DAC connection and leave unrelated device maintenance to dedicated tools. [ALSA mux operations](https://github.com/torvalds/linux/blob/028ef9c96e96197026887c0f092424679298aae8/sound/usb/mixer_scarlett2.c#L3045-L3150)

## Second validation checkpoint

Exact ITF models now take the guarded native/PCM transition through normal playback negotiation and open. The owner unwinds the mode after confirmed alt reset and callback retirement. Rust can explicitly register a capture device and open a standalone, multichannel UAC1/UAC2 PCM IN stream. Rust UAC2 routing discovery and verified transactions use the active USB session and reject writes while another stream owns it.

Rust workspace tests and the Android build and target checks passed. The capture and routing entry points are currently Rust-only. Remaining connected work includes Android claim and JNI surfaces, duplex sharing, ADC3 transport and configuration selection, and the Scarlett vendor adapter. Physical validation remains separate for the fixed-format, alternate and firmware cases.

### Shared worker and configuration integration

For duplex, extend the playback worker with an owned capture attachment. Atomically negotiate both endpoints before acquiring or changing a streaming alternate. Reject different alternates of the same interface, and require matching supported rates when two terminals share a clock. Prove independence before programming different rates on one device. Preserve separate position and loss counters when the clocks are independent.

When capture requests exactly match the implicit source, give the user capture buffer validated bytes from the same completed IN packets that supply feedback lengths. A second pool on that endpoint would duplicate ownership and decouple capture timing from playback. For a distinct IN endpoint, allocate its own geometry and slot pool on the same USB event pump. Capture buffer overrun affects the reader and its counters. A broken shared timing source affects playback through the established implicit feedback failure path. Stop both paths through the existing callback retirement guard, and reset alternates only after ownership returns.

On Android, explicit alternate USB configuration selection belongs to the USB connection owner before native registration. Expose the supported configuration ID to the caller, record the original active ID, select the requested configuration, verify it by GET_CONFIGURATION, then build and claim only its current interfaces. Roll back after a failed selection or claim and after the final native retirement on normal hand-back. A caller who accepted the active configuration keeps the existing direct-open behavior. Treat Android kernel or driver rejection as a typed refusal. [Android UsbDeviceConnection configuration API](https://developer.android.com/reference/android/hardware/usb/UsbDeviceConnection#setConfiguration(android.hardware.usb.UsbConfiguration))

## Third validation checkpoint

Android now has a separate capture-session API with descriptor-only preflight, an explicit USB permission/claim flow, whole-frame reads and stats. UAC3 playback uses the selected active ADC3 function, exact BADD or full-profile PCM layout, and verified power-domain ownership. BADD admission validates the whole function's mandatory endpoint directions and sync type. Full ADC3 retains verified selected clock pins and rejects unimplemented multiplier ratios.

The Rust Scarlett read adapter admits the source-backed 18i20 Gen 2 model and firmware only, with an explicit claimed vendor interface and validated command replies. The standard UAC2 routing path has verified writes. Scarlett mux and mixer writes, broader Scarlett profile coverage, Android routing access, duplex, ADC3 capture and explicit configuration selection are separate delivery gates.

Ran the Rust suite and Android builds after the integrated reviews. The target phones and audio devices still need physical acceptance for new capture, native mode, clock and hardware-routing paths.

## Physical acceptance matrix

Record the USB VID/PID, `bcdDevice`, active configuration, interface and alternate numbers, endpoint addresses, USB speed, device firmware and the tested commit for each result. Capture descriptor and control metadata separately from audio. A passing Rust or Android build establishes software coverage for its tested inputs. Physical results must establish that the device accepted the remedy and that sound reached the intended port or capture buffer.

| Area | Minimum observable acceptance |
| --- | --- |
| Native DSD | Compare known byte patterns at the USB boundary for each width/order family. Play a native track, switch back to PCM, and verify restoration after a failed open and a stop. For an ITF device, confirm alt zero, the mode command and cadence in that order. Use separate Amanero revisions for little- and big-endian evidence. |
| Automatic clock source | Leave a working current source selected. Disconnect or invalidate a switchable external clock, reopen and record verified fallback to a valid source. Check read-only selector refusal, external selector drift and original selector restoration. |
| Multichannel duplex | Send distinguishable channel patterns through a physical loopback or known inputs. Verify ordered channel labels, separate counters on independent clock domains, and bounded capture backlog while playback continues. For implicit feedback, confirm one IN transfer pool and reconcile measured frames with scheduled and delivered OUT frames. |
| ADC3 | Run fixed 48 kHz BADD and full-ADC3 PCM for each integrated direction. Observe the active configuration, correct channel cluster for full ADC3, and domain wake/restore order around start, stop and unplug. Verify a caller-selected configuration and return to the original configuration after close. |
| Hardware routing | Start attenuated. Route one known source to one intended destination, confirm the physical result and device readback, then restore. On the admitted Scarlett firmware, verify whole-row mixer readback independently from tests that require all mux-bank originals. |

No task-owned test runtime was started during the inspection of the connected
Android host. Its USB host inventory had no attached test interface. This
observation does not verify any of the physical cases above.

### Expanding the descriptor-gated native DSD rules

Linux v7.0 supplies two additional sources of built-in coverage. Its
`QUIRK_FLAG_DSD_RAW` is enabled for exact products and certain USB vendor
families, and contributes DSD_U32_BE only when the format descriptor advertises
RAW_DATA. This is evidence for a guarded, read-only layout interpretation.
Carry the full descriptor mask into the decision, require the matching audio
protocol, endpoint geometry and U32 word width, and leave PCM capabilities
derived from their own format bits. Put exact layout/revision rules ahead of
a vendor RAW_DATA fallback. Never let a family layout supersede a nonmatching
Amanero revision, the documented Playback Designs DoP-only alternate, or the
separate ITF native-mode transaction.
[ALSA RAW_DATA conditional](https://github.com/torvalds/linux/blob/028ef9c96e96197026887c0f092424679298aae8/sound/usb/quirks.c#L2071-L2080),
[ALSA vendor quirk flags](https://github.com/torvalds/linux/blob/028ef9c96e96197026887c0f092424679298aae8/sound/usb/quirks.c#L2439-L2490)

### Clock validity capability follow-through

The UAC2 fallback currently asks every candidate for CLOCK_VALID and rejects
a failed request. ALSA inspects Clock Source controls first and treats a
source without readable validity as usable. Keep this distinction in the
route plan. A source with an advertised readable validity control must
reply valid. A source with no such control must pass an exact frequency
readback before commit, and the diagnostic must distinguish unreported
validity from a valid device response. A reserved control encoding and a
failed advertised read both remain errors. Exercise a source with no
validity control, a source explicitly invalid, and the existing route
restore tests. Apply the same semantics to the future shared ADC3 resolver
using its 32-bit controls and ADC3 descriptor offsets.
[ALSA clock-validity check](https://github.com/torvalds/linux/blob/028ef9c96e96197026887c0f092424679298aae8/sound/usb/clock.c#L243-L287)

### Duplex claim and SDK handoff

The Rust pair worker can consume a distinct IN endpoint only if it belongs to
the registered session. The playback registration allowlist currently admits
the implicit source and specific vendor controls. Add an explicit duplex
registration entry point that proves capture claim IDs against the existing
capture preflight for the same configuration. It must retain all base
playback claims and authorize only capture IN interfaces and their owning
AudioControl functions. Keep the existing playback registration policy for
callers who request only playback.

The Android pair entry point should perform one permission check, one config
transaction and one combined claim plan. Publish the Rust pair atomically in
JNI after rechecking that the DAC registry entry is still current. Return the
existing playback and capture handles under one public duplex owner. Closing
either direction retires the shared worker, and device hand-back waits for
both handle lifetimes and the callback retirement guard. Avoid claiming every
IN interface during ordinary playback discovery.

## Fourth validation checkpoint

The Rust duplex worker now shares the implicit capture endpoint when the
requested wire format and endpoint match, or uses a separate input pool
on the same event pump when the clocks and claims permit it. Pair
retirement, capture-rate rollback and propagated device loss use the
same ownership transaction. The distinct-input path still needs explicit
duplex registration and Android pair opening.

ADC3 capture supports the active BADD or full-profile PCM configuration
with verified input-terminal clock topology, scoped power domains and
ordered HC channel identities. Android can explicitly select an advertised
USB configuration, verify it and restore the prior configuration after
retirement. Its standard routing API preserves typed readback, snapshot
scope and rollback errors. The driver also applies the pinned upstream
vendor RAW_DATA layout rules when the descriptor actually advertises the
guarded transport, including the existing exact-revision exceptions.

The Rust workspace, Android cross-target check, Kotlin tests, all four
native ABIs and sample build passed after review fixes. The built AAR
retains the original JVM open methods and Kotlin default bridges.

### Ordered playback channel maps

Capture exposes wire identities today. Extend output formats with the same
ordered channel model without changing wire ordering. For UAC1, resolve the
playback AS terminal through its owning AudioControl association, using the
input terminal's channel mask only when its count matches the selected
alternate. For UAC2, prefer the AS General channel mask. A zero mask can
fall back to the matching input terminal of that same function with equal
channel count, following the upstream comparison. Full ADC3 uses the parsed
HC cluster and fixed BADD layouts derive from the verified endpoint profile.
Retain stable unnamed indices when descriptions are unavailable or ambiguous.
Apply no speaker inference to native RAW_DATA and expose its transport
channels as unnamed when no source-backed identity exists. Compare maps on
stream open to catch changed descriptors, and serialize through the same
Kotlin channel identity decoder as capture.
[USB-IF UAC2 logical channel order](https://usb.org/sites/default/files/Audio2_with_Errata_and_ECN_through_Sep_14_2026.pdf),
[ALSA UAC1/UAC2 terminal and AS channels](https://github.com/torvalds/linux/blob/028ef9c96e96197026887c0f092424679298aae8/sound/usb/stream.c#L715-L865)

### Full ADC3 alternate-clock mutation

A full ADC3 clock plan spans selector pins and an owning power lease.
Discovery may enumerate readable paths without selecting a pin. During
negotiation, acquire only the required D0 domains, validate and program
the chosen path, record its original and target pins, and restore the
original pins before releasing temporary D0. Opening reapplies that exact
plan while owned and awake, after checking that the original pins still
match the negotiated snapshot. A running owner restores the pins after
callbacks retire and the alternate is confirmed zero, before restoring
power. Any selector readback or rollback uncertainty is a typed failure.
Existing BADD fixed-rate behavior bypasses this plan. Keep external
selector changes intact if the snapshot drifted.

## Fifth validation checkpoint

Explicit duplex registration now combines the approved playback and
capture claim sets on one configuration. Android publishes both JNI
handles as one pair. Closing either direction retires the common worker,
and the ordinary playback open methods keep their established claims
and signatures. Negotiated playback and capture mappings carry ordered
channel identities, and playback open checks for a changed map.

UAC2 clock selection distinguishes unreported validity from an advertised
invalid response, with frequency verification for sources lacking a
validity control. ADC3 exact-rate configuration shares that control
policy. Alternate ADC3 pin mutation still needs its power-lifecycle
transaction. Scarlett 18i20 Gen 2 firmware 1083 now has typed
mixer-row edits with fresh state comparison, whole-row readback,
rollback and drift-preserving hand-back. Vendor mux-bank edits and
broader device profiles remain separate implementation work.

Rust and JNI tests, Android cross-target and four-ABI builds, Kotlin
tests and the sample build passed. Physical clock switching, multichannel
wire behavior and hardware routing have yet to be verified on target
devices.

### BADD alternate coverage

BADD section 4.2 requires an empty alternate zero, a continuous
1 ms alternate one, and support for both 16-bit and 24-bit samples.
Table 8-26 provides eight exact continuous endpoint capacities combining
mono/stereo, width and synchronous/asynchronous timing. ALSA applies those
capacity cases to operational alternates without requiring alt one.
The current transport only admits alt one. A direct follow-through is to
recognize additional 1 ms alts with the same exact capacity and feedback
evidence, then test a device that exposes the other sample width on a
different alternate. Optional bursting has different intervals and needs
its own evidence. Do not infer its width by applying the continuous
capacity table to a different interval. SuperSpeed requires validating
the endpoint companion service capacity before admission.
[BADD specification via IEC](https://cdn.standards.iteh.ai/samples/102576/dceddfdc0b3e48879b48480be81b1805/IEC-62680-1-6-2019.pdf),
[ALSA BADD format cases](https://github.com/torvalds/linux/blob/028ef9c96e96197026887c0f092424679298aae8/sound/usb/stream.c#L897-L943)

### Duplex reader contention

Capture presentation is separate from USB completion timing. Both shared
implicit and distinct duplex capture paths use a nonblocking append.
While the reader holds the buffer, the worker continues USB and OUT
processing and accounts for valid capture frames dropped by host
contention separately from device packet loss. Detach classification
works even when the buffer is unavailable. The contention regression
holds the reader lock during completion and verifies deferred accounting
and retirement. JNI and Kotlin expose the resulting contention counter.

### Shared implicit feedback completion

Measure feedback age from the newest valid USB callback timestamp.
Process packet patterns in submission order and keep the freshness
timestamp monotonic across reordered callbacks. Apply the existing
asynchronous cancellation deadline to implicit-only flushes. A terminal
feedback error clears the lock and records the stale transition in the
support trace.

Retire the current OUT callback batch, then check coverage from the
remaining submissions before refilling any implicit-paced OUT slot.
This includes promotion from startup silence. A new submission cannot
extend the coverage estimate for a queue that already ran out. Accept
IN timing only after verifying the original submission and packet spans.
An IN packet that reports device loss follows the device-loss path.

Rust tests, the Android arm64 NDK check, Kotlin unit tests, and the
four-ABI AAR and sample builds passed. The AAR SHA-256 is
`a346493d93fc2a53d921d432d6ce1753687499a6822d03fa96466a104e6b92e2`.
Live implicit feedback and flush checks are scheduled for final hardware
acceptance.

### SuperSpeed capacity evidence

For ordinary SuperSpeed ADC3 endpoints, derive the ISO service-byte
ceiling from the endpoint companion's `wBytesPerInterval`, checked
against the packet, burst and Mult limits. Apply it to both directions
and the dedicated feedback endpoint before using the existing worker.
For BADD, the 1 ms interval and continuous capacity table still define
the inferred format. Record the underlying endpoint and companion
geometry in route verification. Extended SuperSpeedPlus high-bandwidth
descriptors require a separate validated contract.
[libusb SuperSpeed endpoint companion fields](https://libusb.sourceforge.io/api-1.0/structlibusb__ss__endpoint__companion__descriptor.html),
[USB 3.2 specification §9.6.7](https://e2e.ti.com/cfs-file/__key/communityserver-discussions-components-files/138/USB-3.2-Revision-1.1.pdf)

### Shared SuperSpeed transport and feedback

Use the validated periodic endpoint companion on both standard USB Audio
and ADC3 paths. Playback rates, capture and duplex endpoint admission,
and implicit-feedback claims use the actual bytes supported per service
interval. Asynchronous implicit pairing still requires matching IN and
OUT cadence and proven clock topology.

Select the Enhanced SuperSpeed explicit feedback format from the
validated eight-byte endpoint. Decode little-endian unsigned Q32.32
as frames per 125 µs USB bus interval and convert to frames per OUT
service interval. Full ADC3 adaptive capture sends its nominal feedback
using the same representation on a verified eight-byte OUT pipe.
The established three- and four-byte paths retain their existing
decoding. An advertised malformed feedback endpoint prevents timing
fallback. [USB 3.2 §4.4.8.4.1 and §9.6.6–9.6.8](https://www.usb.org/document-library/usb-32-revision-11-june-2022)

Rust workspace tests, Android arm64 NDK, Kotlin unit tests and the Android
AAR and sample builds passed. All four native libraries in the AAR match
the sample. The AAR SHA-256 is
`3531dcd8dd6b5f66968a7e4e080288b2ab0f9050bb2f2db6ee90fe5a75d019e3`.
Live SuperSpeed playback and capture checks remain in final hardware
acceptance.

## Sixth validation checkpoint

Full ADC3 playback can fall back to a verified alternative clock on a
fully claimed single-stream function. Negotiation restores prior
settings before releasing temporary power. Open reapplies the recorded
plan after drift checks, and teardown restores the source frequency
and selector pins before the domain state. A failed restoration remains
visible and does not trigger speculative power writes. ADC3 playback
and capture also accept checked ordinary SuperSpeed companions.
Continuous BADD alternate admission covers source-described 16-bit and
24-bit layouts beyond alt one.

Android can opt into the exact vendor claim and perform Scarlett MIX
and per-bank mux transactions. Mux editing requires a successful
runtime geometry probe and complete bank reads, an opaque native
snapshot, fresh comparison, readback and per-bank rollback. Duplex
capture append uses a nonblocking worker boundary and exposes host
contention separately from transport loss.

Rust/JNI tests, Android cross-target and Kotlin tests, the four-ABI
AAR and sample build passed. The built Kotlin native methods and
corresponding exported JNI symbols were checked for all four ABIs.
The final ADB check found the phone offline. No physical acceptance
was run. ADC3 alternate clock mutation across multiple streaming
interfaces, optional BADD bursting, extended SSP geometry and broader
Scarlett model/firmware evidence remain open.

### Playback Designs alternate-one DoP compatibility

Upstream distinguishes three wire modes for Playback Designs vendor
`23ba`, product below `0110`. Alternate one is logical DSD_U16_LE
expanded to three on-wire bytes per channel by adding the DoP marker.
Alternates two and three are the direct bit-reversed native layouts.
For alternate one, admit a source-backed, DoP-only capability even
where the device does not claim linear PCM for that alternate. Require
the exact identity, one unambiguous matching stereo OUT alternate,
valid descriptor geometry, and capacity for three bytes per channel at
the requested carrier rate. Use the existing DoP marker path. Keep
native, DoP-only and actual advertised PCM eligibility separate in
negotiation and capabilities. Check the alternate and whole-frame
pacing at open and test that an unrelated RAW_DATA product cannot match
the rule.
[ALSA Playback Designs formats](https://github.com/torvalds/linux/blob/028ef9c96e96197026887c0f092424679298aae8/sound/usb/quirks.c#L1982-L2004),
[ALSA DoP endpoint stride](https://github.com/torvalds/linux/blob/028ef9c96e96197026887c0f092424679298aae8/sound/usb/endpoint.c#L1104-L1121)

### Standard routing across class versions

UAC1 addresses a mixer crosspoint with one-based input and output channel
numbers in the two bytes of `wValue`. Query programmable crosspoint
limits through separate GET_MIN, GET_MAX and GET_RES responses before
writing. A Selector Unit uses a zero `wValue` and a one-byte current
pin. UAC2 and full ADC3 instead address a mixer by its control selector
and zero-based MCN, with a RANGE response for limits. ADC3 mixer layouts
use a High Capability output cluster, a programmable crosspoint bitmap,
32-bit unit controls and a 16-bit string ID; its Selector Unit also has
32-bit access flags. Apply the existing verified snapshot transaction
after the protocol-specific parser has proved the active function and
upstream cluster widths. For full ADC3, distinguish equal channel
counts from equal selector input cluster identities. Require a known
owned power state for control operations that cannot complete while a
domain sleeps.
[USB-IF UAC1 audio controls §5.2.2](https://www.usb.org/sites/default/files/audio10.pdf),
[USB-IF ADC3 via IEC 62680-1-5](https://www.iteh.eu/catalog/standards/clc/777313ac-7790-430c-8e99-17c611c46dd2/en-iec-62680-1-5-2019),
[ALSA mixer discovery](https://github.com/torvalds/linux/blob/028ef9c96e96197026887c0f092424679298aae8/sound/usb/mixer.c#L2137-L2317)

### Capture clock fallback follow-through

Standalone capture currently verifies the selected source. Extend it to
enumerate alternate sources on a writable selector when the current
source cannot serve the exact capture format. Reuse the existing
descriptor validity policy and require complete active-function claims.
The capture worker's owner should retain the original pin and frequency
snapshot. Restore it after IN callbacks retire and the altered
interfaces reach alt zero, before releasing ADC3 power ownership.
Keep the pending playback negotiation's saved plan separate from a
standalone capture transaction. For duplex, resolve capture and
playback together against the proposed selector plan and verify the
shared-clock equality or independent frequencies before either stream
starts. Do not apply a fallback validated for only one direction to
an existing pair.
[ALSA clock source validation and alternatives](https://github.com/torvalds/linux/blob/028ef9c96e96197026887c0f092424679298aae8/sound/usb/clock.c#L243-L445)

### Stable ALSA compatibility refresh

The Linux v7.3 native DSD format decision is unchanged from the v7.0
reference. Its quirk table adds exact RAW_DATA entries for Cambridge
Audio streamers and HiBy FC4. Its lookup takes the first exact record
before a vendor fallback, including the explicit Musical Fidelity M6s
DAC RAW_DATA exclusion. Keep these source-backed exceptions ahead of
vendor matching while retaining explicit user overrides. Linux v7.3
also extends its slow CLOCK_VALID retry from the MOTU MicroBook IIc
to the exact older AVB/hybrid USB identity. The retry belongs after
rate or source programming and remains bounded. Read-only discovery
does not need to wait for a clock it has not changed.
[Linux v7.3 quirk table](https://github.com/torvalds/linux/blob/c03c1a14507dfaf8b856c2608f9ac216021fc222/sound/usb/quirks.c#L2423-L2553),
[Linux v7.3 MOTU validity settling](https://github.com/torvalds/linux/blob/c03c1a14507dfaf8b856c2608f9ac216021fc222/sound/usb/clock.c#L208-L246)

### Owned ADC3 routing wake

Existing ADC3 routing discovery verifies known D0 state without sending
power writes. An explicit powered routing operation can reuse
`Adc3PowerLease` on the idle USB owner. Scope wake to the affected
claimed AudioControl function and its routing entities before querying
HC or class controls. Restore the prior domain state after discovery,
read or verified transaction, including failure. Keep the snapshot's
configuration, identity and topology proof independent of temporary
power state. A later powered read or edit must reacquire its own lease
and reject drift. Preserve the existing read-only discovery entry point.

## Seventh validation checkpoint

The exact legacy Playback Designs profile now exposes its alternate-one
DoP capability independently of advertised PCM and direct native DSD.
Linux v7.3 exact RAW flags and vetoes take precedence over the pinned
vendor defaults, and the two known MOTU identities get bounded
post-programming validity settling. Standalone UAC2 capture can try a
verified alternate source and retain its own hand-back snapshot.

Full ADC3 playback and duplex fallback can prove a fully claimed
multi-interface function, quiesce every AS interface, verify the
affected original and intended clock paths, and restore frequencies
and selectors before power hand-back. Extended SSP isochronous
payloads use validated companion lengths and the conservative ceiling
that is valid for every SuperPlus link exposed by the USB API. Unknown
link rate beyond that ceiling and ambiguous BADD bursting remain
explicit refusals.

UAC1 and full ADC3 selectors and mixer crosspoints now use
protocol-specific controls behind the standard routing transaction.
ADC3 routing currently requires known D0. Exact Vocaster One/Two and
Scarlett Solo/2i2/4i4 Gen 4 profiles validate their firmware minima and
complete live mixer/mux geometry before exposing vendor operations.

Rust workspace tests, Android target and four-ABI builds, Kotlin
tests, the sample build and the SDK snippet check passed. No physical
acceptance was performed. The connected Android host was offline.

### Reference integration surface

- Use `UsbDac.openDuplex` to claim playback and input together. After
  negotiating output, `DacSession.openDuplex` opens a pair with an
  exact PCM capture request. The returned playback and capture share
  retirement. Playback and capture formats expose channel order
  and carry no implicit speaker remapping.
- Use `UsbDac.openCapture` when the application requires input only.
  Sample-rate, width and channel-map requests are exact. Capture
  stats distinguish device packet loss, a full reader queue and
  reader-lock contention. These are different causes of missing input.
- Supply an explicit advertised configuration ID when the device's
  active USB configuration is unsuitable. The native stream owner
  retires before configuration restoration. The original open
  signatures continue to use the active configuration.
- Standard routing discovery returns a session-bound snapshot.
  Reads and expected-value edits verify it against the current
  active topology. Vendor opt-in additionally proves the model,
  firmware and complete relevant readback shape. Scarlett mux edits
  require native-observed bank snapshots, and separate failure
  details indicate which rollback operations could be confirmed.
- Playback Designs DoP-only alternates appear as DoP. Negotiating
  PCM or native DSD still requires the corresponding independent
  capability. Exact manufacturer and firmware rules narrow only
  the device behavior they actually describe.

## Eighth validation checkpoint

Standalone full-ADC3 capture can select a verified alternate clock
after the current route fails. It uses its own retained plan, preserves
the pending playback negotiation, and restores prior rates, selectors
and domains after retirement. Opt-in powered ADC3 routing uses a
scoped lease after proving function claims and idle interfaces.
Discovery, reads, transactions and hand-back restore the checked
prior power state while preserving external drift.

The Scarlett vendor adapter now includes source-backed legacy
Scarlett 6i6/18i8 Gen 2 and 4i4/8i6/18i8/18i20 Gen 3 profiles.
Read and mixer admission require live model-specific proof.
Bank editing remains conditional on complete proven bank geometry.
The integration test caught and the source confirmed a 42-word
bank size on 8i6 Gen 3. That value is verified in model tests.

Rust workspace tests, the Android target check, Kotlin tests, the
four-ABI AAR and sample build passed. Kotlin native methods and
corresponding JNI exports for duplex, powered routing and Scarlett
mux were checked in the AAR. Its SHA-256 is
`f01b73ae1e2edcd3560d4d86c3ea31fff65e0009a7180ebdd0ab53909296dd1e`.

The Android phone came online with the iBasso DC04U (`0661:0883`)
attached. Its audio service reported an active media track. No
test sample was installed, no DAC claim was taken, and no physical
acceptance session was started while the phone was in use.

## Built-in device compatibility

The Focusrite Clarett 2Pre, 4Pre and 8Pre each have separate USB and
Clarett+ product IDs under VID `1235`. Use their shared per-model mixer
and mux geometry while keeping the identities separate in Rust and
Android. Confirm firmware and complete live mixer and selector-zero
readback before exposing the routing session. Per-rate mux edits still
require independent readback of all banks, including the smaller
high-rate bank and its padding. [ALSA Clarett geometry and USB IDs](https://github.com/torvalds/linux/blob/028ef9c96e96197026887c0f092424679298aae8/sound/usb/mixer_scarlett2.c#L2053-L2262)

Resolve explicit-feedback rules from the opened USB device and selected
feedback pipe. On the TEAC UD-H01 `0644:8038` and T+A DAC 8 `1852:5034`,
correct the documented deviation in high-speed four-byte reports before
the normal plausibility and packet-ceiling checks. For Playback Designs
VID `23ba`, disregard the first four explicit isochronous feedback
packet opportunities. A discarded report cannot establish or extend
feedback lock. [ALSA start rules](https://github.com/torvalds/linux/blob/028ef9c96e96197026887c0f092424679298aae8/sound/usb/quirks.c#L1930-L1956), [ALSA report correction](https://github.com/torvalds/linux/blob/028ef9c96e96197026887c0f092424679298aae8/sound/usb/endpoint.c#L1825-L1864)

The Rust workspace, Android arm64 NDK check, Kotlin unit tests,
sample build and SDK snippet check passed. The local AAR contains the
native library for all four ABIs. Its SHA-256 is
`2385559d2cefebb6d698c49d53e8ab149ede3e216bfd408ac92b75d55c7fb1d3`.
The Clarett profiles and feedback workarounds still need physical
verification on the matching devices.

## Full ADC3 synchronization

Full ADC3 permits host-paced adaptive playback. An asynchronous sink
can also use implicit feedback from a marked asynchronous IN stream
in the same Audio Function. The two streams must share the device
clock and each needs its own AudioStreaming interface. Require a
unique source, compatible service intervals, and whole PCM frames
within the capacity of both endpoints at the selected rate. Keep the
source and sink available while audio flows. A duplex reader sharing
the same IN endpoint receives data from the feedback transfer pool.
[USB Audio 3.0 §§3.11.4 and 3.14.2](https://git.nefarius.at/nefarius/USB-Bluetooth-Specs/media/branch/master/Audio30.pdf#page=25),
[ALSA synchronization endpoint selection](https://github.com/torvalds/linux/blob/028ef9c96e96197026887c0f092424679298aae8/sound/usb/pcm.c#L331-L411)

For physical acceptance, use a full ADC3 asynchronous pair with a
known channel map and packet trace. Confirm the selected clock and
USB claims, then reconcile completed IN frame counts with scheduled
OUT counts across the lock window. Open the same IN endpoint for
capture and verify the playback/capture byte sequences and one shared
transfer pool. Exercise stop and detach and confirm alternate and
power hand-back. Test adaptive playback separately with a trace of
the nominal OUT frame pattern. BADD has a separate profile contract.

For standalone adaptive capture, select the same alternate's unique
isochronous OUT feedback endpoint with the class-defined No
Synchronization and Feedback usage fields. Validate its interval and
capacity independently of the audio IN pipe. ALSA sends a nominal
rate word in each feedback packet. It sends three bytes of 10.14 at
full speed and four bytes of 16.16 at high speed and above. Use bus
frame or microframe units for the word. The capture owner can keep a
small periodic OUT transfer pool running alongside data IN and retire
both before setting alt zero. A stall, detach or missed send deadline
must become a capture error. Wait for a complete valid OUT send
before delivering adaptive IN data, including when capture is attached
to a duplex stream. A paired adaptive device where audio OUT supplies
implicit feedforward still requires proof of the common clock and
ownership of both audio streams. [USB Audio 3.0 §§3.11.4.2–3 and 4.8.2](https://git.nefarius.at/nefarius/USB-Bluetooth-Specs/media/branch/master/Audio30.pdf#page=101), [ALSA outbound sync packets](https://github.com/torvalds/linux/blob/028ef9c96e96197026887c0f092424679298aae8/sound/usb/endpoint.c#L324-L370)

## Synchronization and FCP validation checkpoint

Playback-only Android discovery can prove a marked UAC3 timing
source using either the current shared clock or a bounded writable
selector plan. The open path rechecks the selected source and its
power state before activating either stream. Rust tests exercised
ambiguous sources, selector and route drift, and both endpoints'
rate capacity. The FCP adapter exercised the exact command sequence,
multi-block firmware-map reads and rejection of malformed maps.

The Rust workspace, Android arm64 NDK check, Kotlin unit tests,
sample build and SDK snippet check passed. The Android release AAR
declares `fcpRead` and exports its JNI symbol on all four ABIs.
The local AAR SHA-256 is
`8ad2f6ea78adb1c1724e1a815150b5cdf3fcf0fd7d4aa964d1773a33adb45aac`.
Physical UAC3 and FCP acceptance is pending. The Android host was
online with the iBasso DC04U detached.

### Direct USB PCM on iBasso DC04U

On November 26, 2026, the Android sample built from `97e614b`
opened a physical DC04U (`0661:0883`) and played stereo 44.1 kHz PCM
through Direct USB. At 297,672 delivered frames the sample showed
`PLAYING`, `Verified`, zero underruns and zero lost frames. It showed
explicit feedback on endpoint `0x83` with an estimated 44,099.3 Hz,
and resolved terminal 3 to clock source 1 on AudioControl interface 1.
The event history recorded the transition from `FEEDBACK_UNLOCKED` to
`Verified` and showed the stream closed after the stop action. The
sample was removed and Flick returned to foreground. Hardware volume
readback after closing the sample was outside the captured evidence.

The tested sample APK SHA-256 was
`4dd21ec28b10fbe6276a7bff598d16dfab89a598d7cf055068b5db9aeb9184b0`.
This observation covers the DC04U explicit-feedback PCM path on that
build. Other devices and newer protocol paths require their own checks.

### Adaptive capture and FCP edits validation

Rust workspace tests, the arm64 Android NDK check, Kotlin unit tests,
sample build and SDK snippet check passed. The Android AAR exports the
FCP read, snapshot and apply calls on all four ABIs. Its SHA-256 is
`1b12574cf4f36c9937643db3149ca79c12080122309a6076c8797fc6ccfc2ae6`.
The adapter still needs hardware acceptance on an adaptive UAC3
capture source and a modern Focusrite interface.

### ADC3 clock multipliers

An ADC3 Clock Multiplier exposes its current numerator and denominator
as two-byte class controls. Read both on the owning AudioControl
interface and retain the raw values with the selector path. Compose
ratios exactly and convert the requested wire rate to a representable
source rate before testing ranges or programming the source. A source
range of 32,000–32,005 Hz with one-Hz steps and a 3/2 ratio yields wire
rates of 48,000, 48,003, and 48,006 Hz. Verify source readback and the
derived wire rate. Keep multiplier controls read-only for automatic
selection. Re-read them before activation and rollback, and report
drift if either value changes.

Implicit feedback requires the same effective ratio and wire rate
because IN frame counts directly pace OUT. Independent duplex audio
can use different derived rates on a common root when both requests
map to the same source frequency. Use the wire rate for packet
capacity, startup reports, and the SDK's negotiated stream format.
Use the source rate for frequency control transfers and hand-back.
[USB Audio 3.0 §§3.13.11.3 and 5.2.1.15](https://git.nefarius.at/nefarius/USB-Bluetooth-Specs/media/branch/master/Audio30.pdf#page=128)

For physical acceptance, choose an ADC3 device with a non-unit
multiplier. Check the reported numerator and denominator, negotiate
a wire rate that maps to a supported exact source frequency, and
observe the source control exchange and packet cadence. Check that
an unrepresentable request is refused before a rate write. Switch
between two clock paths with different ratios, then confirm the
original selector and source frequency are restored when requested.

Rust workspace tests covered exact conversion, clipped range grids,
raw factor drift, and shared-source duplex with different wire rates.
The Android arm64 NDK check, Kotlin tests, sample build and SDK snippet
check passed. The local AAR contains all four native ABIs. Its SHA-256
is `af94d8ed4dc40873c2ee9f8e63284ab2f0c7f03800492b0a96756a1a480d89cd`.
Physical verification of an ADC3 multiplier path is pending.

### Paired adaptive UAC3 capture

For devices with two adaptive endpoints, the device can derive
capture timing from the audio sent to its playback sink. During an
explicit duplex open, match the marked adaptive OUT endpoint to the
adaptive IN alternate on a separate AudioStreaming interface of the
same full ADC3 function. Prove the common source, effective multiplier
ratio and wire sample rate, plus each pipe's own packet capacity. The
two service intervals can differ because each pipe follows its own
schedule. A genuine dedicated feedforward endpoint uses the explicit
path. [USB Audio 3.0 §§3.11.4.3 and 4.8.1.1](https://git.nefarius.at/nefarius/USB-Bluetooth-Specs/media/branch/master/Audio30.pdf#page=26)

The worker can arm capture while the application fills the playback
buffer. Begin the timing deadline with the first playback submission.
Deliver capture only after a valid OUT completion and while that
timing remains current. Complete, frame-aligned wire silence during
pause or flush still provides timing. Retire both pipes before handing
back their clock and power. Physical acceptance should trace first
delivery, pause, flush, loss of OUT timing and stop.

### Device-specific UAC2 timing pipes

Use the ALSA fixed endpoint and interface table as a device-specific
association for standard UAC2 PCM. Match actual USB identity and
keep the existing function, clock, rate, and packet-geometry proof.
A usable explicit endpoint or an exact device-specific implicit
exclusion takes precedence over the fixed association. Proprietary
interfaces and device startup commands require separate proof.
[ALSA fixed endpoint table](https://github.com/torvalds/linux/blob/028ef9c96e96197026887c0f092424679298aae8/sound/usb/implicit.c#L42-L70),
[ALSA exact DIYINHK exclusion](https://github.com/torvalds/linux/blob/c03c1a14507dfaf8b856c2608f9ac216021fc222/sound/usb/quirks.c#L2496-L2508)

The Rust workspace and Android checks passed, including the adaptive
pair, timing-gap recovery, and exact fixed-pipe and exclusion cases.
The local four-ABI AAR SHA-256 is
`439c6eddf25a5002b6ca577405252c9ddded25adbcbe5907ccac2d685ad233e9`.
Both additions still need target-hardware validation.

### UAC2 clock multipliers

The UAC2 multiplier reports the numerator and denominator through
two read-only, two-byte CUR controls. Retain the raw values alongside
the terminal's resolved selector path. Reuse exact rational conversion
to project the source's supported range to endpoint rates. Program only
the verified root source when a change is needed and the whole affected
function is owned and idle. Preserve the existing handling of ordinary
UAC2 clocks and device-specific clock controls. [USB Audio 2.0 §§4.7.2.3 and 5.2.5.3](https://usb.org/sites/default/files/Audio2_with_Errata_and_ECN_through_Sep_14_2026.pdf#page=99)

Playback, native DSD word cadence, and capture packet sizes use the
wire rate. Program and restore source frequencies in the source's
rate domain. A verified multiplier route requires exact readback after
alternate activation. Compare effective ratios and overlapping raw
clock hops before pairing an implicit feedback source. Independent
duplex streams with different wire rates can share a root only if
their requested rates resolve to the same source frequency.

Acceptance needs a UAC2 device with a readable multiplier and a
non-unit ratio. Trace the requested endpoint rate, the derived root
SET_CUR, and the rate read back after alternate activation. Verify the
selected pins and raw factors again before capture or playback starts.
Exercise a direct clock and a multiplier path on the same selector
when the hardware offers both. Confirm that hand-back restores the
saved source rate while preserving external clock changes.

Rust workspace tests, Android arm64 NDK checks, the Android sample
build and SDK snippet checks passed. Tests distinguish ordinary clock
fallback from a looping multiplier path and cover exact rates, raw
factor drift and implicit-clock pairing. The four-ABI local AAR
SHA-256 is
`4157a1cc315e45d03a22cb5685c3937951ba1375ec1ed42c58e180f64c2e9580`.
Physical testing of a UAC2 multiplier path is pending.

### Archived Hidizs AP80 Pro-X evidence

Android's USB host service retained the descriptor from the last
Hidizs AP80 Pro-X connection. The 361-byte record identifies
`c502:0072`, `bcdDevice=ffff`, configuration 1, and UAC2 AS interface
1. Alts 1–3 advertise stereo PCM at 24, 16 and 32 valid bits.
Alt 4 advertises stereo Type I RAW_DATA (`0x80000000`) in four-byte
subslots with 32 valid bits. The data OUT endpoint is `0x01` at
`bInterval=2`; the explicit four-byte IN feedback endpoint is `0x81`
at `bInterval=4`. The descriptor contains the format and geometry.
ALSA's `c502` RAW_DATA rule supplies the native DSD layout. Hidizs
lists USB DAC decoding for DSD64, DSD128 and DSD256.
[Android USB archive](https://android.googlesource.com/platform/frameworks/base.git/+/master/services/usb/java/com/android/server/usb/UsbHostManager.java),
[ALSA RAW_DATA interpretation](https://github.com/torvalds/linux/blob/c03c1a14507dfaf8b856c2608f9ac216021fc222/sound/usb/quirks.c#L2103-L2108),
[ALSA `c502` family rule](https://github.com/torvalds/linux/blob/c03c1a14507dfaf8b856c2608f9ac216021fc222/sound/usb/quirks.c#L2641-L2646),
[Hidizs specifications](https://www.hidizs.net/pages/ap80pro-x-specifications)

Descriptor SHA-256 is
`a767fe00fe6a2ae3e4e442362bbcb030fa69b7a4db5c350a9ad8f6342891d33b`.
The player was detached when the archive was read. Native DSD sample
delivery and its clock were subsequently checked on a connected player.

### Hidizs AP80 Pro-X explicit feedback timebase

On an Android USB host, the AP80 Pro-X opened native DSD on alternate 4
at 88.2 kHz wire rate and completed audio transfers. The ordinary
125 µs decode rejected every feedback report, so the stream retired at
the startup deadline with feedback unlocked.

A bounded diagnostic captured 12 successful four-byte reports in each
mode. The player reports Q16.16 frames per 250 µs data service interval.
The values below are the first report from each mode.

| Mode | Nominal wire rate | Raw bytes | Rate at 125 µs | Rate at 250 µs |
| --- | --- | --- | --- | --- |
| DSD64 | 88,200 Hz | `a2 10 16 00` | 176,519.775 Hz | 88,259.888 Hz |
| DSD128 | 176,400 Hz | `2e 1d 2c 00` | 352,911.865 Hz | 176,455.933 Hz |
| DSD256 | 352,800 Hz | `0a 37 58 00` | 705,719.971 Hz | 352,859.985 Hz |
| 16-bit PCM | 44,100 Hz | `16 09 0b 00` | 88,283.936 Hz | 44,141.968 Hz |

All four alternates report the same endpoint geometry. The initial
device-specific rule matched `c502:0072` with `bcdDevice=0xffff`,
configuration 1, UAC2 interface 1, the PCM and RAW_DATA alternates, and
the exact high-speed OUT and feedback pipe. The live 361-byte descriptor
has SHA-256
`65839151d497e59fe99b76a27855f84ce22d237cd198f522c035da14d3f21a0f`.
[USB 2.0 §5.12.4.2](https://www.usb.org/document-library/usb-20-specification),
[ALSA feedback unit correction](https://github.com/torvalds/linux/blob/028ef9c96e96197026887c0f092424679298aae8/sound/usb/endpoint.c#L1824-L1898)

The initial parity Android AAR passed one bounded session on the OnePlus
host with the attached AP80 Pro-X. The session played balanced native
DSD data through alternate 4, then switched to PCM at 44.1 kHz on
alternates 2, 1 and 3. Each leg had live feedback lock, zero rejected
reports, zero underruns and zero lost frames. The session closed
successfully with the existing device volume preserved.

| Mode | Last feedback | Completed media frames | Transport verdict |
| --- | --- | --- | --- |
| DSD64 | 88,255.981 Hz | 528,760 | Bit-perfect |
| DSD128 | 176,459.960 Hz | 1,054,444 | Bit-perfect |
| DSD256 | 352,855.957 Hz | 2,108,612 | Bit-perfect |
| 16-bit PCM | 44,144.897 Hz | 57,286 | Feedback locked |
| 24-bit PCM | 44,141.967 Hz | 57,203 | Feedback locked |
| 32-bit PCM | 44,141.967 Hz | 57,291 | Feedback locked |

DSD counters count one byte per channel. The PCM legs used silence and
reported `PAYLOAD_UNVERIFIED`. The probe checked USB transport and PCM
hand-back. Rust workspace tests, Android arm64 NDK and the four-ABI
AAR and sample builds passed. The AAR SHA-256 is
`3693c3ea6b7de6b523902060f433cc0ad22ded4d4fb057084d683f92ad095e8f`.

#### Universal detection on the parity build

After integrating `main`, the Kotlin SDK repeated the DSD-to-PCM run
with automatic timing detection. Each exported trace recorded a verified
post-alt clock readback and applied `out_service_interval` without a
timebase override. The first DSD stream and each PCM alternate needed
three matching completions. DSD128 and DSD256 reused the session's timing
hint after one fresh completion each on alternate 4. The final PCM32
trace was still available after session close.

| Mode | Last feedback | Completed media frames | Timebase |
| --- | --- | --- | --- |
| DSD64 | 88,259.888 Hz | 528,752 | Inferred |
| DSD128 | 176,455.933 Hz | 1,057,272 | Reused |
| DSD256 | 352,859.985 Hz | 2,111,436 | Reused |
| 16-bit PCM | 44,144.897 Hz | 66,111 | Inferred |
| 24-bit PCM | 44,141.968 Hz | 66,118 | Inferred |
| 32-bit PCM | 44,141.968 Hz | 66,206 | Inferred |

All six legs reported bit-perfect USB transport with locked feedback,
zero rejected reports, zero underruns and zero lost frames. The PCM
payload varied by one least-significant bit in each direction. The
probe preserved the existing volume. Rust tests, Android arm64 NDK,
Kotlin unit tests, and the four-ABI AAR and sample builds passed.
The combined AAR SHA-256 is
`d606224b29df133ee266678fad783449ae5ba5b69ef3dfc2645fb4dbb7b8a836`.

### Fixed-rate Pioneer USB mixers

The DJM-250MK2 and DJM-750MK2 have fixed stereo-pair channel arrays
on one vendor interface. The driver matches exact USB IDs and the
verified active high-speed configuration. Both use 48 kHz packed
24-bit PCM on interface 0, alternate 1, with `0x01` OUT and `0x82` IN.
The two models carry 8 out and 8 in, and 10 out and 12 in respectively.
Prove endpoint cadence and capacity before claiming the interface.
Channel order is USB wire order. The physical mixer controls output
volume, so playback uses the existing fixed-level opt-in policy.
[ALSA DJM-250MK2](https://github.com/torvalds/linux/blob/028ef9c96e96197026887c0f092424679298aae8/sound/usb/quirks-table.h#L2799-L2875),
[ALSA DJM-750MK2](https://github.com/torvalds/linux/blob/028ef9c96e96197026887c0f092424679298aae8/sound/usb/quirks-table.h#L3373-L3421)

Post valid playback silence before arming the capture pipe. Acquire
implicit feedback within the existing startup window, then schedule
application frames from validated IN packet lengths. A duplex reader
shares those same IN transfers. For capture alone, use the exact
fixed-rate IN endpoint. The ALSA Pioneer capture quirk specifies
independent capture timing. Retire all transfers before releasing the
alternate. [ALSA Pioneer timing](https://github.com/torvalds/linux/blob/028ef9c96e96197026887c0f092424679298aae8/sound/usb/implicit.c#L200-L235)

Rust workspace tests, Android NDK, Android unit tests, sample build
and SDK snippet checks passed. The AAR exports the vendor preflight
on all four ABIs. Its SHA-256 is
`eba6356be9eaa7b6ad0a227604282b30326ef5ed9d07e8c6826b36caeb6a5a8c`.
Physical acceptance needs each target mixer at its exact channel
count, with an OUT-start trace, measured feedback, independent capture
and shared duplex capture.

### Fixed-rate Roland multichannel interfaces

The ALSA table gives the OCTO-CAPTURE (`0582:0120`) and QUAD-CAPTURE
(`0582:012f`) an exact 44.1 kHz signed 32-bit little-endian format.
The OCTO-CAPTURE has 10 USB playback channels and 12 capture channels.
The QUAD-CAPTURE has 4 playback channels and 6 capture channels.
Playback uses interface 0, alternate 1, endpoint `0x05`. Capture uses
interface 1, alternate 1, endpoint `0x85`. Verify both vendor interfaces,
their zero-bandwidth alternates and the live endpoint geometry, then
claim only those two interfaces. The exact profile accepts an asynchronous
audio IN pipe with either normal or implicit-feedback data usage. A standard
USB Audio configuration uses the class parser. [ALSA fixed Roland formats](https://github.com/torvalds/linux/blob/028ef9c96e96197026887c0f092424679298aae8/sound/usb/quirks-table.h#L1214-L1311),
[ALSA Roland timing](https://github.com/torvalds/linux/blob/028ef9c96e96197026887c0f092424679298aae8/sound/usb/implicit.c#L135-L198)

Playback and duplex post valid silence before acquiring timing from IN.
Duplex capture reads the same IN transfers used for that timing.
Standalone capture keeps its companion OUT endpoint running at the
fixed nominal cadence with digital silence. A valid OUT completion
qualifies the initial capture data. Both transfer pools retire before
alternate and claim hand-back. [ALSA companion endpoint startup](https://github.com/torvalds/linux/blob/028ef9c96e96197026887c0f092424679298aae8/sound/usb/pcm.c#L283-L310),
[ALSA silent playback packets](https://github.com/torvalds/linux/blob/028ef9c96e96197026887c0f092424679298aae8/sound/usb/endpoint.c#L279-L318)

Exercise both profiles with exact channel counts. During capture alone,
check the OUT endpoint's zero-filled frame schedule and the captured
channel order. During duplex, check acquisition and continued pacing,
then confirm both alternates return to zero after stop or detach.

Rust workspace tests, the Android arm64 NDK check, Kotlin unit tests,
the AAR and sample builds, and the SDK snippet check passed. The AAR
and sample APK package identical native libraries on all four ABIs and
retain both the single-interface and full-set JNI preflight exports.
The AAR SHA-256 is
`4bccece983076c7b17acc33e2e3c9ba1f6cb727b25ccf94317ef98ffde72e1e2`.
Live playback, standalone capture, duplex, and alternate hand-back remain
to be verified on both models.

### Fixed-rate Pioneer DJ controllers

The pinned ALSA device table gives these controllers a 44.1 kHz vendor
PCM profile on interface 0, alternate 1. Channel counts are USB wire
counts.

| Device | USB ID | PCM | OUT channels and endpoint | IN channels and endpoint |
| --- | --- | --- | --- | --- |
| DDJ-SX3 | `2b73:0023` | Signed 32-bit LE | 12, `0x05` | 10, `0x86` |
| DDJ-RR | `2b73:000d` | Packed signed 24-bit LE | 6, `0x01` | 4, `0x82` |
| DDJ-SR2 | `2b73:001e` | Packed signed 24-bit LE | 4, `0x01` | 6, `0x82` |
| DDJ-800 | `2b73:0029` | Packed signed 24-bit LE | 6, `0x01` | 6, `0x82` |
| DDJ-RB | `2b73:000e` | Packed signed 24-bit LE | 4, `0x01` | 2 timing channels, `0x82` |

The driver verifies the idle alternate, both asynchronous isochronous
endpoints and their matching service cadence before using the vendor
profile. Android claims interface 0 after native proof. A standard USB
Audio configuration uses the class parser. [ALSA DDJ-SX3](https://github.com/torvalds/linux/blob/028ef9c96e96197026887c0f092424679298aae8/sound/usb/quirks-table.h#L2749-L2796),
[ALSA DDJ-RB/RR/SR2](https://github.com/torvalds/linux/blob/028ef9c96e96197026887c0f092424679298aae8/sound/usb/quirks-table.h#L2880-L3026),
[ALSA DDJ-800](https://github.com/torvalds/linux/blob/028ef9c96e96197026887c0f092424679298aae8/sound/usb/quirks-table.h#L3086-L3133)

Start playback with silence and use validated IN packet lengths for
implicit feedback. Share the IN transfers with a duplex reader where
the profile carries real audio. The DDJ-RB's two dummy IN channels
provide pacing only, so its vendor profile exposes playback and rejects
capture and duplex. [ALSA Pioneer timing](https://github.com/torvalds/linux/blob/028ef9c96e96197026887c0f092424679298aae8/sound/usb/implicit.c#L200-L235)

For each controller, verify the exact alternate, channel count and
sample layout on hardware. Play across several fractional packet cycles
and verify implicit lock and release. Verify capture-only and duplex
channel order on the four audio-capable models, and confirm the DDJ-RB
rejects both requests while normal playback continues to use its IN
timing.

Rust workspace tests, the Android arm64 NDK check, Kotlin unit tests,
the AAR and sample builds, and the SDK snippet check passed. Both JNI
preflight methods are present on all four ABIs. The AAR SHA-256 is
`cf030282b9c4697b764e3a87e9711696cbef8f3d37156f4c96f497ffe67a9123`.
Physical acceptance on the five Pioneer controllers remains pending.

### Pioneer DJM rate programming

ALSA programs the listed DJM mixers through the capture endpoint on
vendor interface 0, alternate 1. The format in both directions is
packed signed 24-bit little-endian PCM. Treat the listed rates as
discrete options and intersect them with the capacity of both endpoints
at their common USB service interval.

| Mixer | USB ID | OUT channels and endpoint | IN channels and endpoint | Wire rates |
| --- | --- | --- | --- | --- |
| DJM-900NXS2 | `2b73:000a` | 10, `0x01` | 12, `0x82` | 44.1, 48, 96 kHz |
| DJM-V10 | `2b73:0034` | 12, `0x01` | 12, `0x82` | 44.1, 48, 96 kHz |
| DJM-750 | `08e4:017f` | 8, `0x05` | 8, `0x86` | 44.1, 48, 96 kHz |
| DJM-850 | `08e4:0163` | 8, `0x05` | 8, `0x86` | 44.1, 48, 96 kHz |
| DJM-450 | `2b73:0013` | 8, `0x01` | 8, `0x82` | 48 kHz |

[ALSA DJM format entries](https://github.com/torvalds/linux/blob/028ef9c96e96197026887c0f092424679298aae8/sound/usb/quirks-table.h#L3028-L3243),
[ALSA legacy DJM format entries](https://github.com/torvalds/linux/blob/028ef9c96e96197026887c0f092424679298aae8/sound/usb/quirks-table.h#L3320-L3528)

Verify the active configuration, exact interface claim, and alternate 1
before sending the endpoint request `0x22/0x01` with value `0x0100`, index
equal to the IN endpoint address, and a three-byte little-endian rate.
Discovery and negotiation only read descriptors. A complete control
write authorizes startup measurement. Whole-frame IN packet lengths
must agree with the selected rate over a 100 ms service window before
the driver submits application audio or releases capture frames. Playback
primes with silence and duplex shares its timing IN stream. Standalone
capture measures the same cadence directly from its independent IN
transfers. Playback and duplex require the SDK's fixed-output allowance
when volume is controlled on the mixer. The pinned ALSA request documents
rate selection. Restoring a previous mixer rate requires independent
evidence for a readback or hand-back transaction. [ALSA endpoint rate request](https://github.com/torvalds/linux/blob/028ef9c96e96197026887c0f092424679298aae8/sound/usb/quirks.c#L1766-L1782),
[ALSA model dispatch](https://github.com/torvalds/linux/blob/028ef9c96e96197026887c0f092424679298aae8/sound/usb/quirks.c#L1869-L1877)

Physical acceptance should capture the command after alternate 1
activation, followed by the measured IN cadence at each offered rate.
Exercise playback, standalone capture and duplex, and confirm the
mixer uses one rate command per open. Verify that failed or short
commands, wrong cadence and detach stop the affected stream and that
alternate zero is restored on an attached device.

Rust workspace tests, Android arm64 NDK, Kotlin unit tests, the
four-ABI AAR and sample builds, and the SDK snippet check passed.
The AAR and sample contain identical libraries on all four ABIs.
The AAR SHA-256 is
`9115ae24dc3336023e3fdc90c1ee3acdf3720da23c7bde054386a622a09262ba`.
Target DJM hardware acceptance remains pending.

### Pioneer DJM hardware routing

The five rate-programmed DJMs, the DJM-250MK2 and the DJM-750MK2
expose their source-backed routing choices through the existing idle USB
session. The profile lists capture sources by USB stereo pair and the
mixer's capture level steps. The DJM-250MK2, DJM-450 and DJM-750MK2 also
list playback destinations. Each option belongs to one model and one
control. The DJM-850 has its own labels for the LINE and CD/LINE values,
and the V10 uses a different capture level scale.
[ALSA DJM options](https://github.com/torvalds/linux/blob/028ef9c96e96197026887c0f092424679298aae8/sound/usb/mixer_quirks.c#L3888-L4211)

Rust exposes `Dac::djm_routing_discover` and `Dac::djm_routing_apply`.
Kotlin exposes the same operations through `session.routing.pioneer`
from a playback, capture or duplex connection. Close streams before
using these operations. Opening the session and discovering choices
preserve the mixer's existing settings.

An explicit edit revalidates the open session, exact device and active
configuration, vendor transport geometry, and owned interface. The
streaming interface must still be at alternate zero. A single
zero-payload device request `0x40/0x03` carries the selected model's
value and index. A successful call means USB acknowledged the write.
ALSA's getter reads its software value, so hardware state and route
rollback remain unavailable through this protocol.
[ALSA DJM request and getter](https://github.com/torvalds/linux/blob/028ef9c96e96197026887c0f092424679298aae8/sound/usb/mixer_quirks.c#L4245-L4332)

Hardware acceptance should route a distinguishable input to each USB
pair and change a playback destination on one supported model. Confirm
the device receives one request for each edit and retains its prior
settings on open and discovery. Verify that the driver rejects edits
while a stream is open.

An independent comparison matched all 311 option labels, indexes and wire
values to the pinned ALSA table. Rust workspace tests, Android arm64 NDK,
Kotlin unit tests and the sample build passed. The AAR and sample contain
identical native libraries with both routing JNI methods on all four
ABIs. The AAR SHA-256 is
`4ef77f2c2bda998a50ed6382413803e8e9427e19749b06e0e6e75256cc1fa366`.
Physical DJM routing acceptance remains pending.

The DJM-A9 has separate PCM and routing entries in ALSA. Verify its USB
wire cadence before adding it to the supported sessions for this API.
[ALSA DJM-A9 addition](https://github.com/torvalds/linux/commit/5289d00696396224507ee083f5aee6119569c8d3)
