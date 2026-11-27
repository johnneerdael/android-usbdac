# Collect a USB playback support report

Give listeners a way to record one failed playback attempt and share a file
from your app. A useful report connects what the listener tried with the
first stage that failed and the USB activity around it. The feedback capture
API is available in usbdac 0.1.2 for Kotlin and Rust.

## What the listener does {#listener-flow}

Put the flow under **Settings → Support → USB playback diagnostics**. Use the
same playback controls and output settings the listener would normally use.
Keep capture off by default.

1. **Start capture.** Show what the report includes and how to remove it.
   Start recording app events and enable feedback capture before opening the
   DAC. If playback is already active, offer to restart the output with
   capture enabled.
2. **Reproduce once.** Ask the listener to repeat the action that fails with
   the same DAC, cable, and output settings. Give examples such as tapping
   Play, seeking, or switching from native DSD to PCM. For a transition,
   start recording before the first format plays.
3. **Save the attempt.** Let the listener tap **Stop capture**. If playback
   fails or the DAC disconnects, preserve the failed attempt before
   reopening the output or falling back. Show **Report ready** with the
   failing stage or the last route used.
4. **Preview and share.** Ask the listener for the expected result, what
   happened, and the shortest steps to repeat it. Offer **Save report** and
   **Share report**. Keep the saved report available after Stop, disconnect,
   or a new playback attempt. Turn off capture after the attempt.

## What your app collects {#report-contents}

Start an attempt ID and a monotonic elapsed clock when the listener starts
capture. Use them to tie the SDK trace, app events, and written observations
to the same attempt. Record the wall-clock start time with its timezone for
matching any platform logs.

| Include | Capture this evidence |
|---|---|
| App and host | App version and build, usbdac version, host model, OS version and architecture. Kotlin can read the loaded native version from `UsbDac.nativeVersion`. For Rust, record the linked `usbdac-core` version from the app build |
| USB setup | DAC model, vendor and product IDs, firmware if known, and whether the DAC connects through a hub, adapter, or dock. Record USB permission and claim results |
| Requested and actual playback | Requested encoding, sample rate or DSD bit rate, bit depth, channel count and output mode. Include the negotiated `StreamFormat`, its clock and pacing, and the actual route. For format switches, record both formats in order |
| Output decision and options | For `OutputSelector`, save the chosen output, guarantee, typed rejections and any later fallback. Record effective direct USB, conversion, clock verification, feedback, buffer and device-override settings |
| App timeline | Record attach, permission, open, negotiate, Play, seek or flush, format switch, failure, fallback and close as they happen. Include the first typed error code and full detail, plus the exact step that produced them |
| Stream diagnostics | Capture the terminal event and available stream stats before discarding the stream. Include feedback lock, age, decoded rate, empty and rejected reports, underruns, lost frames, and the bit-perfect blocker. The exported feedback trace carries the underlying USB observations |

Ship the app context and timeline alongside the SDK capture. Save Android's
trace as returned in `feedback.json`. Rust hosts can include a readable
`feedback.txt` or serialize the typed fields with their own schema. Keep
the clock, numeric statuses, packet lengths, diagnostic keys and raw explicit
feedback bytes intact. A screenshot helps show what the listener saw, and
the exported files let support inspect the underlying values.

For a format switch, save a snapshot for each leg before opening its
replacement. The SDK retains the latest stream in the session. Label each
attachment with the attempt and leg so support can compare the last working
stream with the first failing one.

Save the bundle locally first and let the listener choose where to send it.
The SDK capture contains USB timing information and explicit feedback bytes.
Keep media payloads, track names, playlists, account information, USB serial numbers,
and unrelated system logs out of the bundle. Scrub private paths from app
errors while preserving their technical details.

## Compatibility admission and clock suggestions {#compatibility-admission}

Subscribe before starting playback. A successful direct stream can raise
`CompatibilityFallback` with tier `nominal_async` or `unverified_clock` and a
plain reason. The event is sent only after the first healthy OUT queue has
submitted, including wire silence for a paused stream; opening a handle or
negotiating a format alone does not establish admission. A strict refusal,
failed SET, readable rate mismatch or failed initial submission does not
raise it. Vendor-specific rate programming and its independent cadence proof
are separate from standard acknowledged frequency SET/GET_CUR fallback. Both tiers can apply to one stream, producing one event per tier.

Rust exposes `DacEvent::CompatibilityFallback` and
`CompatibilityFallbackTier`; a DAC listener and a distinct stream listener each
receive it once. The same listener used in both roles receives one copy.
Kotlin exposes `DacEvent.CompatibilityFallback` through the existing session
and stream event flows. These events explain compatibility admission; they do
not remove `UNSYNCHRONIZED_ASYNC` or `CLOCK_UNVERIFIED` bit-perfect blockers.

Enable the opt-in feedback trace before opening the session. A fallback is
retained as `compatibility_fallback` with `tier` and `reason`. After three
unusable acknowledged-clock readbacks, the driver also records a bounded
`clock_readback_suggestion` startup observation with the exact
`control_interface`, `clock_entity` and `strikes` (at most sixteen suggestions
per stream). Include that observation and the corresponding `[USB] Clock
interface=... entity=...: three unusable GET_CUR replies` log line in the
support bundle. It suggests investigation of that exact interface/entity;
it does not install a quirk or authorize a device write.

The observation is copied from the current opened owner's session into the
stream's startup trace. Closing the owner clears learned behavior, while an
already captured/exported old trace retains the historical suggestion.
Readable clock replies clear learning; a known mismatch and a failed SET stay
failures. Save each leg before replacing a stream, as described above.

## Kotlin and Android {#kotlin-android}

Apply `captureFeedbackTrace` to the configuration that actually opens the
session. Preserve the listener's other settings. The sample passes the flag
into the same `DacConfig` it uses for direct playback.

```kotlin
        val config = DacConfig(
            requireHardwareVolume = !fixedVolumeBox.isChecked,
            captureFeedbackTrace = feedbackTraceBox.isChecked,
        )
```

If you use `OutputSelector`, put that configuration in
`OutputPolicy.dacConfig` before creating the selector. Save the
`SelectedOutput.decision` and hold on to its `session` for trace export.
Managed Android and AAudio outputs can still produce app-side context and
route or grant details. The direct USB session produces the USB feedback
trace.

Run export and teardown on the playback service's control worker. When a
session fails, save its events and stats while the handles are available.
After `close()` finishes, the opted-in `DacSession` retains its final
`feedbackTraceJson()` on the session object. If `SelectedOutput` owns the
session, save that reference before closing the wrapper and read it after
close. Copy the JSON out before dropping those references. Keep a previous
nonempty report when a later session has no capture. The sample's retention
helper runs on that same worker.

```kotlin
    private fun retainFeedbackReport(closedSession: DacSession) {
        runCatching { closedSession.feedbackTraceJson() }
            .getOrNull()
            ?.let { lastFeedbackReport = it }
    }
```

Collect `DacEvent.StreamFailed`, `DacEvent.DeviceLost`,
`DacEvent.FeedbackStale` and meaningful state changes in the app's timeline.
Queue teardown on the control worker when an event arrives. This keeps the
event collector and audio writer free to return. During a focused support
attempt, `UsbDac.setLogLevel(4)` enables native debug messages under the
`usbdac` logcat tag. Restore the app's normal logging level afterwards.

## Rust hosts {#rust-hosts}

On Android or Linux with direct USB output, configure your app logger before
opening the DAC. The core emits messages through Rust's `log` facade. Route
usbdac messages to a bounded local log for the capture window. Enable feedback
capture on the same `DacConfig` passed to `Dac::open`.

```rust
use std::{fs, io, path::Path};
use usbdac_core::{Dac, DacConfig};

fn for_support_capture(mut config: DacConfig) -> DacConfig {
    config.capture_feedback_trace = true;
    config
}

fn save_feedback_trace(dac: &Dac, path: &Path) -> io::Result<bool> {
    let Some(trace) = dac.feedback_trace() else {
        return Ok(false);
    };
    fs::write(path, format!("{trace:#?}"))?;
    Ok(true)
}
```

Read `DacStream::stats()` and, for explicit feedback, the optional
`DacStream::explicit_feedback_diagnostics()` while the stream handle is
available. Keep the full trace with `Dac::feedback_trace()` after closing the
stream and before consuming the DAC with `Dac::close(self)`. A successfully
opened replacement stream takes over the session's capture, so keep the
failed attempt in your own report before opening another stream. Save
`DacEvent` values and error codes from the host control path. If the host
does permission and claims itself, record their results, including the USB
configuration and actual claimed interfaces.

`Debug` output is a readable attachment. If your support tooling needs
structured Rust exports, map the public trace fields into your app's own
versioned format.

## When a report has no feedback trace {#before-feedback}

A trace belongs to a direct USB stream that opened with capture enabled.
Match each snapshot to its playback attempt. Label an earlier stream's
report as separate context if the current attempt failed during negotiation.

| Earliest failure | Evidence that still helps |
|---|---|
| Discovery, USB permission, or open | Attached USB IDs, the permission prompt outcome, claim result, and the exact open error |
| Negotiation or stream open | The requested format, available capabilities and alternate settings, the negotiated format if returned, and the exact error. For a clock error, include source, requested rate and readback if available |
| Fallback or unexpected routing | The selector's output decision and typed reasons, Android route or AAudio grant, and the route actually used after the failure |
| Disconnect or unexpected stop | The last good state and stats, detach or failure event, time since Play or switch, and the order in which the session closed or fell back |

If support needs native Android logs for an early failure, turn on the
`usbdac` debug level before reproducing and collect that tag over the attempt
window. Use the app event log and export for the regular in-app flow. An
engineer can collect native logcat over ADB when needed. Share the relevant
interval and keep the timestamps.

```sh
adb logcat -d -v threadtime -s usbdac > usbdac-log.txt
```

## How the report narrows the cause {#triage}

| What the report shows | Where to investigate |
|---|---|
| Submission errors, stalled transfers or timeouts | USB endpoint and transport. Read the submit result separately from the libusb transfer and packet statuses |
| Successful completions with repeated empty feedback | Look at packet lengths and timing. A completed transfer can carry zero feedback bytes |
| Feedback bytes arrived but the report was rejected | Inspect the exact bytes, rejection reason, selected timebase, decoded value, verified clock, and OUT packet capacity |
| Implicit feedback stopped delivering usable patterns | Inspect the selected IN interface and alternate, packet statuses and lengths, and the implicit FIFO, waits and invalid packet counters |
| Feedback stayed locked but playback dropped out | Compare app writes, buffer fill, underruns, lost frames and OUT transfer timeouts |
| Clock or route changes fail during a format switch | Compare the last working and first failing requests, alternates, clock readbacks and feedback lock transitions in the same timeline |

The trace keeps the first 128 and most recent 2,048 observations and reports discarded
entries in `dropped_entries`. Save the report promptly after the failure to
keep the relevant window. If the first report leaves the failing layer
unclear, support can ask for a targeted USB descriptor or bus capture.
[Trace fields and status codes](../API.md#usb-feedback-support-capture)

For example, the AP80 Pro-X report showed successful completions and
four-byte feedback packets rejected as out of range. The raw bytes, verified
clock and OUT interval exposed a timebase mismatch. [AP80 Pro-X diagnosis](evidence.md#ap80-pro-x-feedback-units)

## Check your integration {#check-your-integration}

Run the flow for a direct USB failure, a failure before stream open, and a
Stop or disconnect during playback. Verify that a user can share a readable
file, that it includes the first failure even after fallback or a new
session, and that capture turns off when the attempt ends.

## Queue and timing evidence

Stream statistics expose worker gaps over 10 and 50 ms, maximum worker gap in
microseconds, adaptive queue changes, active slot count, and active queue time.
These distinguish a late host worker from packet failures; they do not infer an
OEM cause from a device name. Opt-in explicit feedback reports include accepted
unsmoothed sample count, minimum, maximum, mean and population variance in frames
per OUT service interval, alongside the existing packet outcomes. These values
and `feedbackRateHz` describe accepted raw reports, not a filtered scheduling rate.
The trace's `explicit_feedback_policy` startup entry records `feedback_smoothing`
(`direct` by default, or legacy `ewma`). Each `explicit_feedback_rate` observation
records the selected policy, `reported_frames_per_out_service`,
`applied_frames_per_out_service` and `applied_rate_hz`, preserving fractional
precision. Direct applies the accepted selected report unchanged; EWMA retains
alpha 0.2. The intake selects the newest valid report in each completed transfer,
then commits each completion in order; only the most recent committed value can
influence the next OUT refill. No physical or audible comparison follows from
these statistics alone.

For an attachment or permission issue, retain the current attachment ID as well
as VID/PID and device path. Paths can be reused after unplug; an old permission
result must not reopen a replacement. For an audible click, keep an independent
loopback WAV, capture-loss counters and clock-aligned start/stop/alt-zero event
timeline. The [acceptance prerequisites](https://github.com/johnneerdael/android-usbdac/blob/main/docs/evidence/prerequisites.md) describe
the software probe and the limits of its ADC-relative metrics.
