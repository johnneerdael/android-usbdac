# Physical USB DAC validator

The Android and macOS apps run the same direct USB playback tests and produce a local ZIP. A run starts before the USB device is attached. The operator lowers or mutes downstream amplification, removes headphones, selects **Start capture**, then attaches the DAC. On Android, the system may prompt for USB access. The app proceeds automatically after a single DAC is selected and gives the operator a way to save the ZIP at every terminal outcome.

On Android, **Test connected DAC** can select an attachment that was already present when capture started. The app displays permission, interface claim and format progress separately. If the validator regains focus while permission is still unresolved, it retries the Android request once after checking the selected attachment and the current grant. An explicit denial leaves the manual retry available.

## Driver and host responsibilities

Use the existing Android USB host permission and claim path and the same Rust driver on macOS. The Darwin backend uses libusb for device discovery, interface ownership, control requests and isochronous transfers. The applications share the driver’s descriptor parsing, clock programming, negotiation, feedback, packet scheduling, payload audits and playback APIs. They request exact formats and collect the direct transport’s evidence without converting input or routing through the system mixer.

When a compatible Shizuku service is connected, its optional grant action uses that manager's consent flow and checks whether the service has `MANAGE_USB`. It requests a temporary grant for the selected attachment and this validator. The app verifies the grant through Android's USB host API before claiming the audio interfaces.

macOS can already have attached its USB audio driver when the validator sees a device. Record any failed interface claim and device capture separately, with the libusb error and what permission the process had. libusb’s Darwin backend captures the whole USB device when detaching a kernel driver. Prefer an ordinary claim when available and surface explicit authorization requirements for takeover. Verify release of the interface and return of the host route after the run. [libusb macOS FAQ](https://github.com/libusb/libusb/wiki/FAQ#how-can-i-run-libusb-applications-under-macos-if-there-is-already-a-kernel-extension-installed-for-the-device-and-claim-exclusive-access)

## Automatic playback matrix

| Mode | Request and stimulus | Evidence |
| --- | --- | --- |
| Signed PCM | Exercise the advertised supported rate, significant-depth and channel combinations. Generate low-level, channel-identifiable tones and align significant bits in the negotiated signed slot. | Exact rate, channel count and significant depth, chosen interface and alternate, subslot, clock, frames and payload audit. |
| Unsigned and float PCM | Exercise advertised Type I U8 or Float32. Use unsigned midpoint for U8, finite IEEE binary32 for float, and little-endian multibyte samples. | Keep the encoding explicit at negotiation, write and verification. |
| Native DSD | Exercise supported bit rates and channels from the alternates and driver quirks. Generate a low-level delta-sigma test tone as chronological, interleaved MSB-first input. The driver packs it for the negotiated standard or legacy transport. | Record the intended DSD bit rate, actual layout and transport. Check expected bytes and decoded tone independently of the modulator. |
| DoP | Test plausible 24-bit and 32-bit carrier formats at the DSD ratios the driver can negotiate. Put chronological, modulated DSD in the carried bytes. The driver owns the alternating wire markers. | Verify exact carrier and driver payload audit. Report decoder lock only with independent evidence such as the DAC display or digital receiver. |

Use discrete advertised values. For a continuous range, test eligible endpoints and common audio rates and record the sampling rule. Record the original advertised alternatives, the resolved interface and alternate for each request, and competing alternatives that the normal driver selector left unexercised. Preserve unsupported or unattempted modes in the report. Record duration, signal algorithm, channel frequencies, nominal level and a digest of the submitted input for reproducibility. For a PCM-only DAC, a DoP trial may produce marker noise at the analog output even if the intended DSD signal is quiet. Keep the downstream mute or disconnect instruction visible before capture.

The validator enables source-backed compatibility modes by default so it can exercise matching device-specific formats, including the iBasso DC04 Ultra's native DSD alternate. Clear that option to reproduce the library's verified-only policy. The report records the selected policy and separates mode admission, test execution, USB delivery and decoder observation.

After testing the advertised formats, probe exact requests from a finite standard catalog across signed PCM, unsigned PCM, float PCM, native DSD and DoP. Use the advertised channel counts plus mono and stereo, and include standard points that fall inside advertised rate ranges in the playback matrix. Keep the admission results separate from USB playback verdicts. Record a typed unsupported response as an expected rejection and a clock or host error as inconclusive. If a previously unlisted request negotiates exactly, record the discrepancy and test the resolved format through the usual driver and signal path. Retain the probe plan when a run ends early.

## Capture and verdicts

Record a run ID, UTC start and monotonic elapsed time, app build, driver version, host OS and architecture, and the pre-attach USB baseline. Limit device detail to the run’s USB candidates and selected DAC. Include VID/PID, product and firmware revision where available, link speed, port or hub path when exposed, the descriptors and active configuration, and the permission and interface-claim results. Record unavailable data or a platform error explicitly.

Keep the ordered test plan and all phase results. For each attempted format record negotiation and clock selection, requested and verified rates, endpoint and pacing, stream state and failures, written and completed frames, clock/payload verification, underruns, loss and timing statistics, feedback diagnostics and raw feedback trace. Keep the driver debug log, report any bounded capture truncation, and retain teardown information even when the device disconnects. Record the stage and raw typed error when a failure happens before a stream is available.

Report USB delivery for an exact format only when the driver confirms delivery of the stimulus and its clock, timing and payload evidence meet the run’s pass criteria. Format refusal, clock failure, packet loss, a host permission or ownership block, and absence of enough evidence warrant distinct results. A report can include optional observations of DAC lock, audible playback or a physical loopback. Without them the report makes a USB transport claim and leaves final decoding and analog output unverified.

Stop the playback matrix when the host or driver reports loss of the selected USB device. Preserve the case in progress, completed results and original plan in the archive, with a device-loss reason that distinguishes this outcome from cancellation and timeout.

Prefer incremental capture so an error can still leave a useful report. Close the test stream and session, record the result, and then finalize the archive. A test aborted before USB attachment, application permission or negotiation must still produce a useful ZIP. Save locally through the platform’s file picker. Sharing is an explicit operator action.

Finalize an archive before offering it for Save or Share. If flushing an optional diagnostic source fails, preserve the readable evidence and label the missing or incomplete data. Record an interrupted run as such when recovering its capture. A recovered archive can describe the last observed phase without claiming that playback reached a verdict.

## Acceptance

- Build the native driver and both runnable apps. Confirm the Android APK includes the JNI library for the target ABI. Package the Apple Silicon macOS app and its linked direct-driver worker in a transferable ZIP and verify the extracted bundle has no checkout or Homebrew runtime dependencies. Record signing and notarization status and test launch from a location outside the checkout.
- Independently check generated samples for known signed, unsigned, float, padded and packed byte layouts, DoP payload order, and native DSD bit order and decoded frequency. Verify clock, payload and outcome fields map to the matching stream and format.
- On each app, start with no DAC, cancel while waiting, and save a valid ZIP with the start, wait and terminal events. Test denial or loss when available and inspect the archived error and connection stage. Keep tests and live-runtime work in separate batches.
- On each platform, start capture and attach a known DAC. Verify USB identity/configuration, exact PCM playback and drain, healthy transfer counts, result export, and release of the interface. Test an intentional unplug during a run and collect the partial ZIP.
- On hardware with confirmed support, repeat native DSD and DoP through the direct driver. Check the DAC’s indicated mode or an external capture against the run leg. When hardware supports one mode only, verify unsupported modes and failed claims remain individually diagnosable and the capture stays exportable.
- Keep the app artifacts, representative reports and anonymized hardware evidence identifiable by build and platform. Label any hardware-dependent result that remains unverified.

The first physical acceptance target is the TOPPING D10 Balanced. Its USB input lists PCM to 384 kHz, DoP through DSD128 and native DSD through DSD256. The SMSL AO300 adds a higher-rate USB PCM and DSD target on a different device. Run the Schiit Modius and Fosi DAC-Q4 as additional PCM coverage and retain the exact device revision and negotiated capabilities in each report. [TOPPING D10 Balanced manual](https://dl.topping.audio/usermanual/d10b.pdf), [SMSL AO300 manual](https://www.smsl-audio.com/upload/portal/download/AO300Manual.pdf), [Schiit Modius introduction](https://www.schiit.com/news/news/introducing-modius), [Fosi DAC-Q4 manual](https://fosiaudio.com/pages/user-instruction-q4).

Use the iBasso DC04 Ultra for the first Android USB-C run, including native DSD up to the rate recognized by the driver. The Hidizs AP80 Pro-X in USB DAC mode provides a second portable USB target and an independent display for checking the received format. [iBasso DC04 Ultra](https://ibassousa.com/ibasso-dc04ultra/), [Hidizs AP80 Pro-X specifications](https://www.hidizs.net/pages/ap80pro-x-specifications).

### Android hardware evidence

The OnePlus CPH2747 captures on 27 November 2026 reported successful USB delivery and session close for these stereo paths. These captures exercised the playback matrix. Standard admission probes require a fresh run. The DoP results cover the carrier, with decoder lock unobserved.

| USB product | VID:PID and bcdDevice | PCM tested | Native DSD tested | DoP carriers tested |
| --- | --- | --- | --- | --- |
| iBasso DC04 Ultra | `0661:0883`, `0x0001` | 16, 24 and 32 bit through 768 kHz | DSD64 through DSD512 and corresponding 48 kHz-family bit rates | 176.4, 352.8 and 705.6 kHz |
| SMSL USB AUDIO | `152a:85dd`, `0x0315` | 24 and 32 bit through 768 kHz | DSD64 through DSD512 and corresponding 48 kHz-family bit rates | 176.4, 352.8 and 705.6 kHz |
| Schiit Unison Modius E | `30be:101a`, `0x0100` | 16, 24 and 32 bit through 192 kHz | No admitted cases | 176.4 kHz |
| Hidizs AP80 Pro-X | `c502:0072`, `0xffff` | 16, 24 and 32 bit from 32 to 384 kHz | DSD64, DSD128 and DSD256 | 176.4 and 352.8 kHz |
