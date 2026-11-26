# Scarlett routing follow-on

The standard UAC2 Rust routing API does not implement the Scarlett mixer/mux
protocol or establish Scarlett device coverage. Keep vendor discovery and writes
behind a separate, exact-model protocol implementation.

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

This plan adds no vendor command bytes or runtime probes to standard UAC2
routing. The next implementation batch begins with the selected target's
protocol/identity contract and read-only discovery.
