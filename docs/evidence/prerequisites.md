# Physical acceptance prerequisites

The results ledger preserves pending rows from the 0.1.5 gap report. The current
software-delivery task waives physical device acceptance; this waiver does not
turn any row into verified hardware support. Populate exact DAC VID/PID,
bcdDevice, phone, Android build, OEM profile, source revision, feature outcome,
and SHA-256 of the exported log for each new run. Record a failed outcome when
it fails; keep missing metadata explicit.

Required OEM cases include Samsung Android 17 hardware volume keys on XMOS
20b1:0008, Honor Magic 8 Pro Android 17 with UGREEN CM720, vivo iQOO Neo 10
OriginOS 7 with TempoTec Sonata BHD 22e1:e208, Poco F9 Ultra burst-loss and
screen-off playback, CX31993 readback on MediaTek and Snapdragon hosts, and
Moondrop CDSP 31b2:0112 nominal asynchronous timing on two phones. These are
reports in other projects, not usbdac measurements. See the preserved
[external-report analysis](../sdk/evidence.md#other-projects).

Capture click measurements for start, stop, alt-zero, and rate switching on
three chipsets with a loopback cable. Record through an independent capture
process/device so playback close cannot terminate the observation. Use
`loopback_capture` (packed S16 stereo input), retain its WAV and JSON metadata,
and annotate playback events against the capture clock. Analyze with
`.github/scripts/analyze_loopback.py`, passing the actual reference sample rate,
clock offset, fixed capture gain, and silence threshold measured against the ADC
noise floor. `--clock-aligned` asserts an independently established alignment;
without it the report is explicitly incomparable. Calibrate the same ADC,
gain and signal path before comparing ADC-relative dBFS across runs. A measured
sample jump and a silence gap are separate observations; neither establishes
audibility or a physical pass on its own. The capture probe reports failed,
missing or backlogged input as invalid evidence.

QEMU's `usb-audio` exposes full-speed UAC1. The CI smoke verifies real guest
usbfs transfers, alternate zero, original raw hardware volume restoration, and
interface release. It uses a null audio backend. It does not prove UAC2/UAC3,
feedback, Android, sound quality, click suppression or real DAC timing.

The prepared combined virtual acceptance boots once with two `usb-audio`
devices. It runs the single-device playback/reconfigure/rollback probe, followed
by the concurrent two-owner/duplicate-admission/independent-close probe. Before
launch it records an absolute UTC stop deadline and a hard 60 second host
limit. Guest probe limits are 12 and 15 seconds; the guest powers off after
both. Keep implementation and research outside this owned runtime window.
A launch is authorized only after the final integrated native code compiles;
the script's presence is not a virtual pass record.
