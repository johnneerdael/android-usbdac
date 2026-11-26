# Scarlett routing follow-on

The standard UAC2 Rust routing API remains separate from the Scarlett mixer/mux
protocol. The first vendor batch supplies a narrow read-only implementation;
it does not establish physically validated Scarlett device coverage.

Use Linux v7.0 commit `028ef9c96e96197026887c0f092424679298aae8`,
[`sound/usb/mixer_scarlett2.c`](https://github.com/torvalds/linux/blob/028ef9c96e96197026887c0f092424679298aae8/sound/usb/mixer_scarlett2.c),
as protocol research evidence. Implement independent MIT code from an explicit
protocol contract; do not copy the GPL driver implementation or model tables.

## Evidence and implementation sequence

1. Select one exact product and firmware for the first acceptance target. The
   pinned driver's `scarlett2_devices` entries associate product IDs with
   individual model information, and `scarlett2_device_info` carries port and
   firmware differences. A shared Focusrite vendor ID or a generation name
   cannot authorize a command. Record interface identity and descriptor geometry
   from the intended target before enabling a rule.
2. Document the transaction envelope independently: request/response sizes,
   sequence matching, status handling, initialization and timeout behavior.
   Research `scarlett2_usb`, the protocol initialization path and asynchronous
   notification handling together. A vendor GET operation may require an OUT
   command transfer; classify operations by their actual device effects rather
   than transfer direction. Exclude flash, firmware, erase, reboot and persistent
   configuration operations from the routing adapter.
3. Add exact-model read-only discovery. Research `scarlett2_usb_get_mux`,
   `scarlett2_usb_get_mix` and their port-number conversion paths. Define typed
   hardware ports, sample-rate-dependent mux banks, mixer inputs/outputs and gain
   units. Validate every returned port against the selected model and every
   response against its expected size. Report unknown firmware or unknown ports
   as unsupported. Keep source-backed compatibility separate from physically
   verified routing.
4. Capture the original complete mux/mixer state before enabling edits. Specify
   the allowed source/destination relation, transaction granularity, expected
   current state, readback and restoration for each supported firmware. Account
   for changes made through front-panel controls or another controller. Reuse
   the existing session/Feature Unit serialization and reject edits during any
   stream. Preserve configured volume admission and explicit hand-back policy.
5. Enable writes only after a bounded acceptance session on the selected device
   verifies the physical destination as well as request/readback transcripts.
   Cover valid edits, rejected ports, partial failure, restore failure, external
   drift and unplug. Publish the exact model/firmware evidence and unsupported
   neighboring identities. Add further models as separate evidence-backed rules.

## First source-backed read-only batch

`scarlett.rs` independently implements the 16-byte little-endian envelope, the
initialization handshake, bounded ACK processing, reply command/sequence/size/
status validation, typed mux ports and raw unsigned mixer coefficient reads.
No routing, persistent configuration or maintenance command builder is present.
The request/response path uses class-interface OUT request 2 and IN request 3;
initialization first uses IN request 0, then initialization commands 0 and 2.
Initialization handles the documented sequence-one/reply-zero exception only
for its two startup commands. Normal reads require exact sequence equality.

The sole candidate identity is `1235:8201` (Scarlett 18i20 Gen 2). Firmware
`1083` is the sole admitted firmware because the pinned source explicitly names
that original protocol capture. Other firmware stops after initialization,
before mux/mix queries. These bounds express the evidence currently selected;
they do not imply that adjacent firmware is incompatible.

The selected source-backed shape has 18 mixer inputs, 10 mixer outputs and
56 mux destinations. Mux-zero decoding requires a complete set of distinct,
known destination IDs and known source IDs. Mixer coefficients retain their
wire values; no copied lookup table or inferred dB conversion is used.
GET_MUX's zero argument is preserved as `mux_zero`; this read is not presented
as a snapshot of all three sample-rate banks. It is also not an atomic snapshot
of concurrent front-panel changes.

`Dac::scarlett_routing_capability` performs descriptor-only discovery while
idle. `Dac::scarlett_routing_read` explicitly initializes and reads vendor
state. The host must opt into and already hold the vendor-interface claim.
Admission requires one unambiguous alternate-zero vendor interface with one
interrupt IN endpoint, an eight-byte packet size and nonzero interval. Audio
streams and negotiation are excluded through the existing session gate; volume
controls share the existing control mutex. USB identity and the active
configuration are checked; descriptor drift during the read rejects its result.
All notification transfers retire before the call returns. No claim is acquired
or released by these methods, and existing audio/volume hand-back is unchanged.

Focused inline tests cover identity and firmware exclusions, byte order,
malformed replies, mux IDs, sequence handling, bounded notification floods and
poisoned sessions. Hardware initialization, physical routes, actual firmware
responses and Android execution have not been validated on a Scarlett device.
The shared integration gate must run the new tests before reporting them as
passing. Vendor route writes remain deferred until sample-rate-family state,
verified rollback and physical acceptance evidence are available.

## Independent mixer writes

The mixer and mux have different recovery properties. GET_MIX returns every
coefficient for one named output. SET_MIX sends the same output number followed
by its full coefficient row. That permits a verified transaction for mixer
gains without touching any sample-rate mux bank. Start with the existing
18i20 Gen 2 firmware profile. Accept an expected old row and a requested new
row of the exact model width. Compare a fresh GET_MIX to the expected row,
preserve untouched coefficients byte for byte, and read back the entire row
after SET_MIX. Report the original failure and any restoration uncertainty.
Use the existing sequence, claim and idle-session guards. Expose bounded,
source-backed gain values and preserve the raw original row for restoration.
Run mocked transport cases before trying the transaction on an attenuated
device. [ALSA mixer row operations](https://github.com/torvalds/linux/blob/028ef9c96e96197026887c0f092424679298aae8/sound/usb/mixer_scarlett2.c#L2844-L2900)

Mux writes remain a separate transaction. Upstream GET_MUX sends request
selector zero. SET_MUX sends three individually numbered tables with
different destination widths and padding. A valid selector-zero reply
establishes the current route representation used by this implementation,
while per-bank original values and rollback still need evidence. Capture
each table's observed request and response or document that the firmware
shares one canonical route graph across all banks. Verify restoration for
each table after partial failure before enabling mux edits.
[ALSA mux read and write](https://github.com/torvalds/linux/blob/028ef9c96e96197026887c0f092424679298aae8/sound/usb/mixer_scarlett2.c#L3051-L3144)

Upstream's minimum firmware checks supply useful evidence for broader
readback coverage. Vocaster One and Two use minimum 1769, Scarlett Solo and
2i2 Gen 4 use minimum 2115, and 4i4 Gen 4 uses minimum 2089. Each exact model
has its own port geometry and capability flags. Verify the startup reply
and model-specific port/width rules before using an upstream minimum as an
admission boundary. The presence of a model registration alone supplies no
evidence for a particular token's bank or route behavior.
[ALSA device profiles](https://github.com/torvalds/linux/blob/028ef9c96e96197026887c0f092424679298aae8/sound/usb/mixer_scarlett2.c#L1805-L2052),
[ALSA firmware checks](https://github.com/torvalds/linux/blob/028ef9c96e96197026887c0f092424679298aae8/sound/usb/mixer_scarlett2.c#L8489-L8499)

For new mixer gains, use the pinned driver's documented half-dB interval
from −80 to +12 dB. The wire coefficient is the integer part of
`8192 × 10^(dB/20)`. Derive it from the formula and retain raw wire values
in readback and restoration snapshots. Request verification must compare
the whole row, including unchanged sources. Hand-back can restore a row
only when the current row still matches the last verified write. Keep
front-panel or other-controller edits intact.
[ALSA mixer coefficient scale](https://github.com/torvalds/linux/blob/028ef9c96e96197026887c0f092424679298aae8/sound/usb/mixer_scarlett2.c#L196-L207)

### Per-bank evidence from the FCP userspace driver

The separate FCP userspace driver supplies a concrete bank-read contract.
It queries MUX_INFO for the three bank widths, then issues MUX_READ with
four byte fields for offset, padding, count and bank. MUX_WRITE identifies
the bank in its two-word header. Its routing code reads each bank and
tracks a destination's slot separately at each rate. This evidence is
for the documented FCP device families. The 18i20 Gen 2 firmware already
admitted to usbdac still needs a successful capability probe or a matching
trace before treating these reads as its rollback baseline.

A guarded implementation can probe MUX_INFO and each bounded bank read
before allowing any edit. Validate all reply lengths, port identities,
destination uniqueness and the model's bank geometry, including padding.
Preserve exact bank payloads for rollback. For an edit, require the
current complete snapshot, update only the named destination in the
requested banks, read those banks back, and restore all affected originals
after a partial failure. Gate every write on an observed complete
snapshot. This also permits read-only reporting for firmware that answers
selector zero and rejects the per-bank probes.
[FCP information and bank wire operations](https://github.com/geoffreybennett/fcp-support/blob/02be3c9081658af9c5b1246294a054d5eb24ef60/server/fcp.c#L349-L469),
[FCP destination slots by rate](https://github.com/geoffreybennett/fcp-support/blob/02be3c9081658af9c5b1246294a054d5eb24ef60/server/mux.c#L447-L491)
