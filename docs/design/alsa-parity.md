# ALSA parity design and implementation ledger

Baseline `de8010bb427e3cc9184c2acc6cb084dcfc3ee719` (usbdac 0.1.1). Compare USB-IF protocol requirements, upstream ALSA behavior and the userspace limits of Android usbfs. Build independent MIT code and record hardware compatibility as evidence-backed data. Source baseline for Linux comparisons is v7.0, commit `028ef9c96e96197026887c0f092424679298aae8`.

## Work and acceptance order

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

## Channel layout and hardware routing

Expose descriptor channel labels alongside count and preserve wire order in capture and playback. Distinguish spatial positions from unnamed channels. Treat unsupported conversion requests as typed failures. Model class selectors and mixer crosspoints as typed nodes and writable edges. Probe control readability and writability before exposing them, use protocol-specific CUR/RANGE encodings, verify writes and roll back partially applied route sets where supported. Device topology and vendor routing are distinct capabilities. Start vendor integration with known device/firmware identities and read-only route discovery, then implement verified changes for supported models. ALSA's Scarlett Gen 2–4 families encode generation/model differences, hardware ports, mixer/mux data and control transactions. Plan per-family implementations with exact evidence rather than forwarding arbitrary messages through the public API. [ALSA USB mixer](https://github.com/torvalds/linux/blob/028ef9c96e96197026887c0f092424679298aae8/sound/usb/mixer.c), [ALSA Scarlett controls](https://github.com/torvalds/linux/blob/028ef9c96e96197026887c0f092424679298aae8/sound/usb/mixer_scarlett2.c)

## USB Audio Class 3

Select UAC3 from the interface protocol and the Audio Interface Association profile. Keep explicit protocol dispatch in class parsers and control addressing. BADD is a separate configuration/profile contract with inferred topology and a fixed 48 kHz stream profile. Match the documented endpoint direction, sync and packet capacity before inferring width and channels. Full ADC3 requires parsing its 64-bit format mask and fetching bounded High Capability descriptor bodies by cluster ID through the AudioControl interface. Validate declared lengths and IDs on both reads and derive channel order from cluster segments. Inspect existing mixed-configuration Android ownership before any SET_CONFIGURATION. Recompute claims and capabilities if configuration changes. [USB-IF ADC3 via IEC 62680-1-5](https://www.iteh.eu/catalog/standards/clc/777313ac-7790-430c-8e99-17c611c46dd2/en-iec-62680-1-5-2019), [USB-IF BADD via IEC 62680-1-6](https://cdn.standards.iteh.ai/samples/102576/dceddfdc0b3e48879b48480be81b1805/IEC-62680-1-6-2019.pdf), [ALSA ADC3 stream parsing](https://github.com/torvalds/linux/blob/028ef9c96e96197026887c0f092424679298aae8/sound/usb/stream.c#L872-L1099)

Track the power domain associated with each active entity and reference-count shared domains. Bring a stopped domain to D0, honor specified recovery time, then verify control and streaming state. Keep it awake while a stream or active control holds it. Shut down after the final owner and after retiring transfers, including abort and device hand-back. Preserve UAC1/UAC2 paths through existing CI and verify actual Android configuration and power transitions before stating hardware coverage. [ALSA UAC3 declarations](https://github.com/torvalds/linux/blob/028ef9c96e96197026887c0f092424679298aae8/include/linux/usb/audio-v3.h), [ALSA power-domain control](https://github.com/torvalds/linux/blob/028ef9c96e96197026887c0f092424679298aae8/sound/usb/power.c)

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
