# Device compatibility candidates

Use this list to choose future device work. Each candidate describes the
additional behavior, the applicable USB identity, and the evidence still
needed for implementation or hardware acceptance.

The audit used release 0.1.3 (`b516d75`) and the parity source at `52c1100`.
“Branch coverage” describes implementation in that parity source. Existing
software and hardware evidence is recorded in [the parity ledger](alsa-parity.md)
and [the Scarlett handover](scarlett-routing-follow-on.md).

The main Linux reference is v7.0 commit
`028ef9c96e96197026887c0f092424679298aae8`. The existing stable refresh uses
`c03c1a14507dfaf8b856c2608f9ac216021fc222` for RAW_DATA and MOTU settling.
The list covers the device families identified in the format, feedback, clock,
and routing audit. Upstream registration establishes a candidate for research.
Treat safe vendor writes and Android compatibility as separate validation goals.

## Decisions with an exact additional transport target

| Candidate | Current coverage | Additional scope | Evidence still needed before enabling |
| --- | --- | --- | --- |
| Pioneer / AlphaTheta DJM-A9, `2b73:003c` | Absent from the active vendor PCM and DJM routing model tables. A parked local stash contains an earlier attempt; it is not accepted implementation. | High-speed vendor interface 0, alt 1; packed 24-bit LE PCM; 10 OUT channels on `0x01`, 12 IN on `0x82`; 44.1/48/96 kHz where both endpoint capacities permit. Playback, capture and duplex; model-specific capture routing and levels. | Live descriptors and IN cadence; prove which rate currently drives the device. The pinned rate-programming dispatch omits A9, so the other DJMs’ endpoint rate command cannot be assumed. Verify rate mismatch refusal before application audio, startup, hand-back and physical routes. |
| MOTU MicroBook II vendor configuration, `07fd:0004` | Branch already has bounded UAC clock-valid settling for this ID. That does not admit its proprietary PCM interface. Standard-class MicroBook IIc behavior must stay separate. | Exact vendor interface 0, alt 1, packed 24-bit **BE**, fixed 96 kHz; 8 OUT channels on `0x03`, 6 IN on `0x84`; same-interface implicit timing and the separate boot protocol. | Identify the actual configuration and firmware. Audit the HID-like boot exchange and endpoint ownership before adapting it to Android. Prove BE sample bytes, timing, capture and duplex; do not apply this shape to an Audio-class interface or MicroBook IIc’s unrelated vendor interface. |
| Edirol advanced-driver modes: UA-700 `0582:002b`, UA-25 `0582:0074`, UA-4FX `0582:00a3`, UA-25EX `0582:00e6` | Outside the branch’s two fixed Roland vendor profiles. Ordinary class-compliant configurations can already use the shared parser. | Model-scoped admission and interpretation of the documented `EDIROL_UAXX` streaming interfaces; derive format/rate from the device’s mode and descriptor evidence. | Audit the dynamic quirk resolver and each advanced-mode descriptor. Do not copy the existing OCTO/QUAD fixed 44.1 kHz profile. Confirm switch-dependent format, capture relationship and supported rates on each selected model. |

The A9 transport and routing facts come from the
[original A9 addition](https://github.com/torvalds/linux/commit/5289d00696396224507ee083f5aee6119569c8d3),
the [capture-level correction](https://github.com/torvalds/linux/commit/38e94cefbf45c1edc5b751ab90a3088f7c6fac1a),
and [the rate dispatch](https://github.com/torvalds/linux/blob/028ef9c96e96197026887c0f092424679298aae8/sound/usb/quirks.c#L1766-L1877).
The parked stash proposes measured-rate startup and acknowledges that no A9 rate
write is proven; retain that as a proposal. The source-backed A9 controls cover
capture level and capture sources, not a new general playback destination API.

MicroBook facts are in the
[fixed format table](https://github.com/torvalds/linux/blob/028ef9c96e96197026887c0f092424679298aae8/sound/usb/quirks-table.h#L2692-L2746),
[class-sensitive feedback rule](https://github.com/torvalds/linux/blob/028ef9c96e96197026887c0f092424679298aae8/sound/usb/implicit.c#L72-L80),
and [boot protocol and dispatch](https://github.com/torvalds/linux/blob/028ef9c96e96197026887c0f092424679298aae8/sound/usb/quirks.c#L1311-L1410).
The [Edirol table](https://github.com/torvalds/linux/blob/028ef9c96e96197026887c0f092424679298aae8/sound/usb/quirks-table.h#L841-L1188)
separates advanced and ordinary IDs. UA-25 `0582:0073` and UA-25EX
`0582:00e7`, for example, are ordinary 16-bit/44.1 kHz configurations;
they are not new proprietary-format targets.

## Additional clock and startup behavior

These are narrow follow-ons, independent of whether normal PCM already works.
None is an instruction to replay Linux initialization blindly.

| Candidate | Proposed feature | Boundary and missing evidence |
| --- | --- | --- |
| TEAC ITF identities already admitted by the branch: `0644:8043`, `0644:804a`, `0644:8044`, `0644:806b`, `0644:807d`, `0644:806c` | Audit immediate streaming-alt activation when changing between 44.1 and 48 kHz clock families. Native DSD mode switching already exists. | Linux applies the rate-family step by TEAC VID. A future rule here needs an exact model/firmware transcript proving the extra ordering is required and safe. Do not conflate it with the branch’s ITF zero-alt mode transaction. |
| Denon DJ single-rate devices, exact product ID unresolved in this evidence | Investigate invalid CLOCK_VALID replies for a directly connected internal source with one supported rate. | Linux documents this descriptor-shaped exception, but names no exact product in that function. No new named-device support or global invalid-clock bypass is justified. Acquire identity, rate readback and descriptor proof first. |
| Fractal Audio Axe-Fx III `2466:8010` | Bounded, cancellable readiness before stream activation. | Fixed UAC2 feedback association already exists. Linux allows a long SET_INTERFACE readiness transaction and returns to alt zero. Prove Android cancellation, timeout and restoration without weakening ordinary open deadlines. |
| MOTU M Series first hardware version `07fd:0008` | First-open settling policy. | Upstream’s once-only quirk is a four-second delay, not a newly documented clock command. Establish actual need and detach cancellation on that revision; do not assign an unproven M2/M4 product name to this shared identity. |
| M-Audio Fast Track Pro `0763:2012`; Audiophile USB `0763:2003`; Quattro USB `0763:2001` | Explicit model-specific mode/alternate policy, including digital-versus-analogue and rate/width combinations. | Upstream `device_setup` policies are not automatically a suitable SDK policy. Document supported combinations, zero-alt mutations, rate verification and recovery for the selected device. Fast Track Pro also has a boot quirk. |
| PreSonus Studio 1810c `194f:010c` | Expose the intended analogue/S/PDIF/ADAT alternate layout explicitly. | The upstream alternate filter distinguishes channel layouts. Existing generic format negotiation is not evidence of physical routing or optical mode. Pair with the vendor mixer audit below. |

Sources: [TEAC rate-family activation](https://github.com/torvalds/linux/blob/028ef9c96e96197026887c0f092424679298aae8/sound/usb/clock.c#L598-L607),
[Denon validity conditions](https://github.com/torvalds/linux/blob/028ef9c96e96197026887c0f092424679298aae8/sound/usb/clock.c#L180-L192),
[Axe-Fx readiness](https://github.com/torvalds/linux/blob/028ef9c96e96197026887c0f092424679298aae8/sound/usb/quirks.c#L974-L1002),
[MOTU first-version delay](https://github.com/torvalds/linux/blob/028ef9c96e96197026887c0f092424679298aae8/sound/usb/quirks.c#L1411-L1417),
[mode filters](https://github.com/torvalds/linux/blob/028ef9c96e96197026887c0f092424679298aae8/sound/usb/quirks.c#L1444-L1603).

Other exact boot registrations provide a lower-priority research list:
Creative Extigy `041e:3000`, Audigy 2 NX `041e:3020`; C-Media/Turtle Beach
`10f5:0200`, CM6206-family `0d8c:0102`, Terratec Aureon 7.1 `0ccd:00b1`;
Digidesign Mbox 2 `0dba:3000`, Mbox 3 `0dba:5000`; Focusrite Novation
Saffire 6 USB `1235:0010`, Twitch `1235:0018`; Access Music VirusTI Desktop
`133e:0815`; Native Instruments Komplete Audio 6 `17cc:1000`, Traktor Audio 6
`17cc:1010`, Traktor Audio 10 `17cc:1020`; Plantronics Gamecom 780
`047f:c010`. These identify startup investigations, not verified additions.
Some sequences reset, re-enumerate, select configurations or write defaults;
the Android ownership, preserved settings and recovery contract must be
established before implementation. Keep boot IDs separate from the NI control
IDs below. [Exact boot dispatch](https://github.com/torvalds/linux/blob/028ef9c96e96197026887c0f092424679298aae8/sound/usb/quirks.c#L1605-L1687)

## Additional hardware control families

These vendor adapters are absent from the audited branch. Existing standard
PCM or standard entity controls are not being declared unsupported. Start each
chosen adapter with exact identity and readback, then add only the listed
controls whose transaction and restoration semantics can be proved.

| Target and USB identity | Candidate feature | Evidence boundary |
| --- | --- | --- |
| M-Audio Fast Track C400 `0763:2030`, C600 `0763:2031`; Fast Track Ultra `0763:2080`, Ultra 8R `0763:2081` | Model-specific monitor mixer, effect and routing controls. | Their fixed feedback associations already exist; this is additional control support. Audit complete rows, gain units, cached getters and physical output mapping. |
| Native Instruments Traktor Audio 6 `17cc:1011`, Audio 10 `17cc:1021` | Direct-thru and phono-input switches per supported channel. | Exact vendor read/write operations exist upstream. These are not the boot IDs `1010/1020`. |
| Digidesign Mbox 1 `0dba:1000` | Internal/S/PDIF clock source and input source. | Special endpoint clock encoding differs from a standard UAC2 selector. Prove supported rates and readback before writing. |
| RME ADI-2 Pro `2a39:3fd2`, ADI-2 DAC `2a39:3fd3`, unnamed RME entry `2a39:3fd4` | Read-only AES/S/PDIF rates, sync and current clock information. | The source establishes status reads, not TotalMix or general route writes. Retain the unnamed ID until independent product evidence exists. |
| RME Babyface Pro FS `2a39:3fb0` | Model-specific crosspoint gains, PCM remapping, output gains and documented clock/DSP switches. | Requires register-width, gain and complete readback/restore audit. Do not extend to other Babyface identities by name. |
| RME Digiface USB `2a39:3f8c`, alternate ID `2a39:3fa0` | Input sync/format/rate, output ADAT/S/PDIF format, sync source and clock status. | Upstream boot writes defaults, loopback and gains. Preserve the user's configuration; control support alone does not establish a full mixer or safe boot recipe. |
| PreSonus Studio 1810c `194f:010c`, Studio 1824c `194f:010d` | Vendor mixer adapter research. | Dispatch points to a separate implementation; audit its full protocol before claiming route or DSP scope. |
| Tascam US-16x08 `0644:8047` | Vendor mixer/DSP adapter research. | Separate `mixer_us16x08.c` requires its own control and firmware contract; no new coverage is implied here. |
| E-MU 0204 `041e:3f19` | Front-jack channel selection. | A narrow switch, not a new multichannel router. |
| ASUS Xonar U1 `0b05:1739`, `0b05:1743`; U3 `0b05:17a0` | Digital playback switch. | Establish acknowledged write versus actual state readback. |
| Audio Advantage Micro II `0d8c:0103` | S/PDIF status and playback switch. | Audit status-word masks and cached controls separately from PCM. |
| Creative Sound Blaster E1 `041e:323b` | Input-source switch. | The upstream getter is software state; a USB ACK is not physical route readback. |
| Electrix Ebox-44 `200c:1018` | Corrected mono headphone/output mix controls. | Upstream bypasses corrupt mixer descriptors. Use the exact model control map, not a parser-wide reinterpretation. |

The exact registrations and adapters are in
[ALSA mixer quirks](https://github.com/torvalds/linux/blob/028ef9c96e96197026887c0f092424679298aae8/sound/usb/mixer_quirks.c#L4340-L4510).
Relevant protocol sections are
[M-Audio](https://github.com/torvalds/linux/blob/028ef9c96e96197026887c0f092424679298aae8/sound/usb/mixer_quirks.c#L1256-L1759),
[NI and Mbox](https://github.com/torvalds/linux/blob/028ef9c96e96197026887c0f092424679298aae8/sound/usb/mixer_quirks.c#L849-L1255),
[RME status](https://github.com/torvalds/linux/blob/028ef9c96e96197026887c0f092424679298aae8/sound/usb/mixer_quirks.c#L2460-L2829),
[Babyface](https://github.com/torvalds/linux/blob/028ef9c96e96197026887c0f092424679298aae8/sound/usb/mixer_quirks.c#L2830-L3327),
and [Digiface](https://github.com/torvalds/linux/blob/028ef9c96e96197026887c0f092424679298aae8/sound/usb/mixer_quirks.c#L3328-L3798).

Scarlett Solo Gen 3 `1235:8211` and 2i2 Gen 3 `1235:8210` also have upstream
control registrations but are absent from `CandidateModel`. Audit their actual
control capabilities before calling this a routing addition: a registration
alone does not establish a mux/mixer. Broader controls on the already admitted
Focusrite families—Air, phantom power, impedance, direct monitor, clock-source
selection, DSP or persistence—are separate feature decisions, not implied by
existing route APIs. Persistent saves, flash and firmware updates remain outside
this routing menu. [Focusrite dispatch](https://github.com/torvalds/linux/blob/028ef9c96e96197026887c0f092424679298aae8/sound/usb/mixer_quirks.c#L4415-L4453)

Dock jack detection (Dell WD15 `0bda:4014`, WD19 `0bda:402e`, HP TB Dock G2
`03f0:0269`) and Sony DualSense `054c:0ce6` / Edge `054c:0df2` jack reporting
are additional source-backed investigations. They do not add PCM formats or
routes. DualSense’s Linux path depends on `hid-playstation` input events, so
that path is not directly portable to an Android USB audio session.
[Jack-detection registrations](https://github.com/torvalds/linux/blob/028ef9c96e96197026887c0f092424679298aae8/sound/usb/mixer_quirks.c#L4363-L4515)

## Coverage already present; keep it out of the new-device backlog

Released 0.1.3 already contains standard USB PCM/DoP/native transport machinery,
explicit feedback handling, and the existing MOONDROP Dawn Pro
`2fc6:f06a` and Fosi DS2 `262a:0001` built-ins. The parity branch adds the
following model-specific coverage. A physical evidence gap below is a later
acceptance task, not an unimplemented device profile.

| Branch coverage | Exact identities / boundary | Remaining physical evidence |
| --- | --- | --- |
| Native DSD layouts | Amanero `16d0:071a` only revisions `0199`, `019b`, `0203`; `16d0:0a23`; the exact 32-bit BE alt-2/alt-3 identities listed below. | Other Amanero revisions need their own layout evidence. Do not use guessed firmware profiles. Verify mode, wire order and PCM recovery on selected hardware. |
| ITF native mode commands | TEAC/Esoteric IDs above; Denon `154e:1002`, `154e:1003`; Marantz `154e:3005`, `154e:3006`; Luxman `1852:5062`, `1852:5065`, `3255:0000`. | Exact model/interface shape, DSD-to-PCM transitions, failure and detach recovery. Commands already exist; they are not proposed new support. |
| Playback Designs | VID `23ba`, legacy PID below `0110`: alt 1 DoP, alt 2/3 native byte/word profiles; guarded stereo geometry. | Hardware format and bit order, feedback startup, PCM hand-back. No individual product name is proven by this range. |
| RAW_DATA interpretation | Exact and vendor rules with exclusions already exist, including iBasso DC04 Ultra `0661:0883`, Cambridge streamers `22e8:ca10`, HiBy FC4 `32bb:0004` and Hidizs-family VID `c502`. | Descriptor RAW_DATA and eligible format geometry remain required. RAW_DATA interpretation never authorizes vendor mode writes. Existing exact exclusions remain authoritative. |
| Explicit feedback deviations | TEAC UD-H01 `0644:8038`, T+A DAC 8 `1852:5034`; Playback Designs startup discard. | Target-device timing and lock. AP80 Pro-X `c502:0072` already has recorded DSD64/128/256 and PCM16/24/32 transport acceptance with universal timing inference. Do not re-add its retired special timebase rule. |
| MOTU clock settling | `07fd:0004`, `07fd:0005`. | Exact rate/source transition acceptance; no claim of proprietary PCM or a product name for `0005` follows from the settling rule. |
| Fixed Pioneer PCM | DJM-250MK2 `2b73:0017`, DJM-750MK2 `2b73:001b`; DDJ-SX3 `2b73:0023`, RR `2b73:000d`, SR2 `2b73:001e`, 800 `2b73:0029`, RB `2b73:000e`. | Playback and hand-back on each. Capture and duplex on audio-IN models; RB has dummy timing IN and intentionally refuses capture/duplex. |
| Programmed Pioneer PCM and DJM routing | DJM-900NXS2 `2b73:000a`, V10 `2b73:0034`, 750 `08e4:017f`, 850 `08e4:0163`, 450 `2b73:0013`; routing also covers the two fixed DJMs above. | Each rate/cadence, capture/duplex, physical routes. Routing reports acknowledged writes; no hardware getter or general rollback is proven. |
| Fixed Roland PCM | OCTO-CAPTURE `0582:0120`, QUAD-CAPTURE `0582:012f`: fixed 44.1 kHz S32 LE, exact vendor interfaces. | Playback-first acquisition, standalone capture’s silent companion OUT, duplex, channel order and release. These profiles already exist. |
| Focusrite Gen 1 / Forte routing | VID `1235`: 6i6 `8012`, 8i6 `8002`, 18i6 `8004`, 18i8 `8014`, 18i20 `800c`, Forte `8010`. | Model-specific route/gain readback, physical destinations, transactional restore and externally changed controls. |
| Legacy Scarlett routing | VID `1235`: Gen 2 18i20 `8201`, 6i6 `8203`, 18i8 `8204`; Gen 3 4i4 `8212`, 8i6 `8213`, 18i8 `8214`, 18i20 `8215`. | Exact firmware and complete live shape. Gen 2 18i20 admission remains firmware 1083. Mux edits require complete observed bank originals; mixer edits require complete row originals. |
| Clarett routing | VID `1235`: USB 2Pre/4Pre/8Pre `8206/8207/8208`; Clarett+ 2Pre/4Pre/8Pre `820a/820b/820c`. | Firmware, live geometry, all rate banks and physical route/gain restore. USB and Plus retain distinct identities. |
| Vocaster / small Gen 4 routing | VID `1235`: Vocaster One/Two `8216/8217`, Solo/2i2/4i4 Gen 4 `8218/8219/821a`. | Live geometry and physical routes. Firmware minima are 1769, 2115/2115/2089 respectively. Do not invent mixer rows for DSP-only endpoints. |
| Firmware-described FCP routing | VID `1235`: Scarlett 16i16/18i16/18i20 Gen 4 `821b/821c/821d`, ISA C8X `821e`. | Firmware map, fixed pins, per-bank topology, physical edits and ownership-aware restore. Read/snapshot/apply already exist in Rust and Android. |

The remaining explicit native layout IDs in `native_dsd.rs` are already
implemented. Alt 2: `139f:5504`, `20b1:3089`, `2522:0007`, `2522:0009`,
`2522:0012`, `2772:0230`. Alt 3: `0d8c:0316`, `10cb:0103`, `16d0:06b2`,
`16d0:06b4`, `16d0:0733`, `16d0:09d8`, `16d0:09db`, `16d0:09dd`,
`16d0:0ab1`, `16d0:eca1`, `1db5:0003`, `20a0:4143`, `22e1:ca01`,
`249c:9326`, `2616:0106`, `2622:0041`, `2622:0061`, `278b:5100`,
`27f7:3002`, `29a2:0086`, `6b42:0042`. A unique eligible playback layout
and matching geometry remain required; these IDs do not establish physical
acceptance or permission for a mode command.
[Upstream native layout rules](https://github.com/torvalds/linux/blob/028ef9c96e96197026887c0f092424679298aae8/sound/usb/quirks.c#L1976-L2082)

The fixed **standard UAC2** implicit-feedback associations already cover the
following exact IDs. Additional hardware work should validate their existing
class/clock/rate/capacity guards; it should not be presented as adding the table.
A proprietary configuration or a startup requirement is a separate decision.

| Device | USB ID | Timing IN endpoint / interface |
| --- | --- | --- |
| M-Audio Fast Track C400 / C600 | `0763:2030` / `0763:2031` | `0x81` / 3 |
| M-Audio Fast Track Ultra / Ultra 8R | `0763:2080` / `0763:2081` | `0x81` / 2 |
| Fractal Axe-Fx III | `2466:8010` | `0x81` / 2 |
| Solid State Logic SSL2 / SSL2+ | `31e9:0001` / `31e9:0002` | `0x81` / 2 |
| Steinberg UR22C | `0499:172f` | `0x81` / 2 |
| RTX6001 | `0d9a:00df` | `0x81` / 2 |
| RODE AI-1 | `19f7:000a` | `0x84` / 3 |
| Allen & Heath Qu-16 | `22f0:0006` | `0x81` / 3 |
| Zoom UAC-2 | `1686:f029` | `0x82` / 2 |
| Fractal Axe-Fx II | `2466:8003` | `0x86` / 2 |
| Yamaha MODX | `0499:172a` | `0x86` / 2 |

Source: [fixed timing associations](https://github.com/torvalds/linux/blob/028ef9c96e96197026887c0f092424679298aae8/sound/usb/implicit.c#L42-L80).
The branch keeps exact implicit exclusions and usable explicit feedback ahead
of these associations.

## Selecting a later batch

The most concrete new vendor transport is A9; the most direct new monitor
control candidates are M-Audio and NI. RME status can be considered as a
read-only feature before any RME route or boot writes. These are engineering
sequencing suggestions, not selected implementation commitments.

For each selected batch, record the exact VID/PID, revision/firmware,
configuration, interface/alternate and wire shape; the feature being added;
existing evidence that remains valid; and the bounded physical acceptance
steps. Complete connected implementation first, build once, then execute the
prepared acceptance session. Preserve the result and stop the owned runtime
before further research or changes.

Shared UAC3, BADD, clock multipliers, standard routing, DSD capability reporting
and DoP carrier negotiation remain class-level work. No model name or USB ID
should be invented to turn their pending physical acceptance into a device
addition. No new device-specific code or hardware activity is part of this
bundle.
