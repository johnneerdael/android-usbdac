# Design decisions: clock and pacing

Three decisions about time. The first is which of the DAC's clocks gets the sample rate and how the driver knows it took. The second is how the host stays in step with a DAC that keeps its own clock. The third is what to do with DACs that cannot say how fast they want the data. [USB audio in a page](usb-audio-primer.md) explains the vocabulary (clock source, selector, feedback endpoint) if it is new.

The parts of each decision are the question, what the three drivers do, what usbdac does, the evidence, the trade-off and the public references. Revisions are defined on [the drivers page](drivers.md#revisions-studied); the [overview of all decisions](decisions.md) has a summary table.

## 6. Which clock gets the sample rate, and how it is verified {#clock-path}

**The question.** A UAC 2.0 DAC describes its clocks as entities: a clock source makes a rate, a selector chooses between several clocks, a multiplier derives one from another. The stream's terminal points at one of them. Where does the driver set the sample rate, and how does it know the DAC accepted it?

**In plain terms.** Some DACs have two clocks, for example one for USB and one for a digital input, with a switch between them, and many bridges built on XMOS chips point the terminal at the switch instead of at a clock. If the driver sends the rate to the wrong entity, the DAC either refuses it, accepts it on a clock that is not feeding the output (wrong pitch or silence), or answers with something that is not a rate at all.

| | What it does | Where |
|---|---|---|
| Flick today | Treats the terminal's `bCSourceID` as a clock source. If it names a selector or multiplier, or the trace fails, it falls back to "the first Clock Source found in the AudioControl descriptor". Flick's own `feat/usb-dac-unified` branch has the walk (commit `1a183a52`); upstream `versionF` does not | `find_audio_control_clock` in `rust/src/uac2/android_direct.rs` at `11395a49` |
| Mornye | Follows multipliers and selectors for up to 8 hops, asks a selector with several inputs for its current pin (class `GET_CUR`, 1 byte, 1-based), and refuses a pin outside the descriptor. Reads the clock's `RANGE` first and refuses a rate it does not list "without touching the clock". Skips `SET_CUR` when a read-only clock already reads the rate. Reads back and fails with "USB clock rate could not be verified" | `usb_audio/descriptors.rs` (`clock_source`, `clock_ranges`, `clock_offers`), `transport.rs` (`Session::configure`) |
| BitChord | Keeps the terminal's `bCSourceID` as the clock id and writes the rate to it. The parser does not read clock selector or multiplier descriptors, although the file's header comment says ambiguous selectors are rejected. Reads back and fails on mismatch | `usb/UsbDescriptors.kt` (the terminal case of the parse loop), `usb_transport.cpp` (`configure`) |
| usbdac | Walks terminal, selector, multiplier, source, at most 16 hops, with loop detection, and reports the path it took. A selector with one input is not asked. A multiplier's numerator and denominator are read, and the rate is converted exactly between the wire rate and the rate of the source. Falls back to the first clock source only when the walk is impossible, with a warning and `resolution = FALLBACK_FIRST_SOURCE`. Reads the clock's `RANGE` and refuses a rate it does not list, but treats a missing or empty answer as "no information". Sets the rate (three attempts, with a settle delay of 20 ms by default), reads it back, and checks the clock validity control when the DAC has one. When the current route cannot run the rate, it can select another input of a writable selector, reads each pin back, and puts the original pins back when the stream ends. Reads the rate again after the alternate setting is active | `crates/usbdac-core/src/clock_resolution.rs`, `transport/clock.rs` (`apply_sampling_frequency`, `select_requested_clock_route`) |

**What usbdac does.** The result of the walk is on every negotiated stream as `StreamFormat.clock` (`ClockChoice`: the control interface, the source id, the list of hops with the selector's chosen pin, and `resolution`). `DacConfig.requireVerifiedRate` (default on) refuses to stream when the read-back differs from the rate that was set. An applicable compatibility policy may admit PCM after an acknowledged SET and unusable readback; that stream remains unverified. A positive mismatched rate is not accepted as proof. A rate that is not verified blocks the bit-perfect flag with `CLOCK_UNVERIFIED`. UAC 1.0 sets the rate on the endpoint (3 bytes), reads it back after the alternate setting is active, and has no clock entities, so `clock` is null. A UAC 1.0 endpoint with no rate control can run at an exact fixed descriptor rate or, when it is host-paced, an eligible rate.

UAC 3.0 devices use the same walk with their own descriptors and 32-bit controls. Capture and duplex use it too: in a duplex stream, two directions on one clock must ask for the same rate, and directions on separately proven clocks keep their own rates and counters.

*Switching an input.* The walk reads the selector's current pin first. If the clock behind it cannot run the requested rate or reports itself invalid, and the selector is writable, the driver tries the other inputs in turn, verifies each pin by reading it back, and restores the original pins if the attempt fails and when the stream ends. A selector that is read-only, a clock shared with an interface the app has not claimed, and a route that cannot be proven are left alone. A selector pin that is valid for the rate is never changed.

*Around the alternate setting and during a stream.* Before it switches the alternate setting, the driver reads the Valid Alternate Settings control when the descriptors advertise it, and after the switch the Active Alternate Setting control and the sampling frequency. If the AudioControl interface has an interrupt endpoint, the output thread watches it. A notification for the active alternate or the clock path triggers a read-back. A read-back that fails or disagrees with the active route stops the stream with `StreamFailed` and clears the verified clock and the feedback lock.

**Evidence.**

- Mornye's commit `b98f81046` (2026-11-11) is a hardware-driven report of the same fault: "XMOS-based DACs such as the FiiO BTR15 point their input terminal at a clock selector, not a clock source. Direct USB sent the 4-byte sample rate request to the selector, which answers with its 1-byte input pin, so playback fell back with 'Short USB control response'." Flick's `1a183a52` describes the other failure: on a DAC with two clocks the rate could be "set and verified on the wrong one". Neither commit was run on a DAC with a selector by this project.
- Mornye's `da608a663` (2026-11-18) is the reason to read the clock's range before setting a rate: "a DAC that takes the request and then plays silence was never converted at all". usbdac makes the same check.
- The UAC 2.0 specification says the Sampling Frequency Control of a Clock Source is "the only way provided by this specification to change the sampling clock", and describes the selector and multiplier entities. The Linux driver walks the same chain: `__uac_clock_find_source` in `sound/usb/clock.c` follows selectors (reading the current pin, one-based) and multipliers (as pass-through) down to the source and sets the rate there.
- Tests: `clock_resolution_tests.rs` covers a direct source, a one-input selector, a multi-input selector on pin 1 and on pin 2, a multiplier, selector into multiplier into source, multiplier into selector, loops, an endless chain, missing entities, bad pins, malformed descriptors and UAC 1.0 descriptors, all on synthetic descriptor bytes. **No DAC with a selector or multiplier has been tried with usbdac.** The DC04U run in the SDK sample resolved terminal 3 to clock source 1 on AudioControl interface 1, a plain source; the Flick log in PR #400 records `SET_CUR verified: 44100 Hz`. Switching a selector input, the multiplier ratio, the live alternate-setting controls and the AudioControl notifications are covered by host tests only.

*Rate ranges.* A UAC 2.0 RANGE triplet with `MIN < MAX` and `RES = 0` is given no meaning by the specification (UAC 2.0 §5.2.1 defines only `MIN = MAX, RES = 0`, a single rate). usbdac reads such a UAC 2.0 triplet as its minimum only (`normalize_uac2_ranges` in `transport/clock.rs`), which is where the Linux driver's loop ends too (`parse_uac2_sample_rate_range` in `sound/usb/format.c` stops after the first rate when the step is zero). Linux uses that parser for UAC 3.0 as well; usbdac does not. UAC 3.0 ranges are used as the device reports them, so the same triplet offers no rate at all, and a UAC 3.0 clock that reports only such triplets has no exact rate to negotiate.

**Trade-off.** The walk and the range check add control transfers when the DAC opens, and each is a place where an unusual DAC can answer wrongly. The range check could refuse a rate that a DAC with a wrong range list would play; usbdac treats an unreadable or empty list as "no information" so that such a DAC is not refused, and keeps the quirks for DACs that read back wrongly. Up to three attempts, each followed by a settle delay, can add time to opening a DAC that is slow to settle; the attempts and the delay were inherited from Flick and have not been re-tuned. Changing a selector pin changes the DAC's state, so the driver does it only with every interface of the function idle and claimed, and the restore at the end can fail on a DAC that has changed its own clock meanwhile. During the search, a restore that cannot be confirmed stops the search before another input is tried, and at the end of the stream the failure is logged as a warning.

**References.** [UAC 2.0 clock entities](references.md#ref-uac2), [UAC 1.0 sampling frequency control](references.md#ref-uac1), [Linux `clock.c`](references.md#ref-linux-clock), [Mornye commits](references.md#ref-mornye-commits).

## 7. How the host stays in step with the DAC's clock {#feedback}

**The question.** In asynchronous mode the DAC runs its own clock, so the DAC knows how many samples it needs per millisecond and the host has to ask. The DAC reports that on a feedback endpoint. How does the driver read the report, how much does it trust it, and what does it do when the reports stop?

**In plain terms.** At 44.1 kHz, a 125 microsecond packet averages 5.5125 frames. The driver alternates between whole packet sizes, typically 5 and 6 frames; the USB Audio 2.0 formats document limits a constant-rate packet to the whole part of the average or one more ([section 2.3.1.1](references.md#ref-uac2-formats)). The DAC reports the rate of its own clock, and the driver adjusts the mix of packet sizes to stay in step.

| | What it does | Where |
|---|---|---|
| Flick | One persistent feedback transfer, polled over a window of up to 16 ms, newest packet used. Reports within 2 % of nominal are accepted and smoothed (new value 20 %, old value 80 %). 500 ms without a report is logged as stale and playback continues on the last value | `ANDROID_USB_FEEDBACK_POLL_WINDOW_MS`, `FeedbackTransferSlot`, `update_feedback_frames_per_packet` in `android_direct.rs` and `iso_packet_scheduler.rs` |
| Mornye | One feedback transfer of one packet, resubmitted each time it completes. Each report within 2 % of nominal replaces the previous one directly, with no averaging. 10.14 in three bytes only on a full-speed link, 16.16 in four bytes. **Two seconds without a valid report ends the stream** ("USB feedback stopped or invalid") | `usb_audio/descriptors.rs` (`PacketClock`), `transport.rs` (`run`) |
| BitChord | The same | `usb_stream.h` (`PacketClock`), `usb_transport.cpp` (`run`) |
| Linux `snd-usb-audio` | Applies each report directly and range-checks it (rejects values outside `freqn - freqn/8 .. freqmax`); detects whether the device uses 10.14 or 16.16 | `sound/usb/endpoint.c` (`snd_usb_handle_sync_urb`) |
| usbdac (current) | Two asynchronous transfers keep an isochronous feedback endpoint polled, so a new poll is already queued while the output thread handles the last completion (an interrupt endpoint uses one single-packet transfer). Uses the newest valid report of each completed transfer, checks 7/8 through 3/2 nominal and the largest required OUT packet, then applies the accepted fractional report directly by default. An explicit EWMA option retains the legacy 0.8 / 0.2 average. A short gap holds the last valid rate. Stale timing clears lock and a persistent outage ends the stream | `transport/iso.rs` (`FeedbackTransfers`, `FeedbackTransferSlot`, `ExplicitFeedbackHealth`), `transport/output.rs`, `iso_packet_scheduler.rs` |

**What usbdac does.** For an isochronous feedback endpoint the driver polls over a window of `min(16 ms, busQueueMs)`, rounded down to whole feedback intervals and held to 1 to 32 packets. When the window holds at least two packets, it is split between two transfers that are both submitted at the start, and each is submitted again as soon as its completion has been handled. The endpoint is therefore never left unpolled while the output thread works through the previous result. (Before 0.1.3 one transfer was in flight at a time.) The AP80 Pro-X, whose feedback endpoint has a 1 ms interval, gets two transfers of eight packets. Completions are handled in the order they were submitted, even if libusb hands a later one back first. From each completed transfer the driver scans every packet and uses the newest report that passes validation. A successful empty poll increments `feedback_empty`. Empty, malformed and rejected reports leave the last valid rate and timestamp in place. The decoder accepts three bytes as 10.14, four as 16.16, and on a SuperSpeed link eight as an unsigned Q32.32 count of frames per 125 microsecond bus interval, and scales the value to the OUT service interval. The capacity check rounds the required frame count up, since some packets carry that larger size. UAC2 does not advertise alternate feedback units. For four-byte high-speed reports outside the standard rate window, the driver can test OUT service interval units against an independent clock readback. The first stream on a route needs three matching transfer completions. Later streams on the same USB configuration, interface, alternate setting and endpoints can reuse the learned units after validating fresh timing. Closing the DAC clears the remembered units. A trace-backed [`ExplicitFeedbackPerServiceInterval` quirk](controls.md#quirks) can force the alternate units when clock readback is unavailable. No device-specific feedback correction is active in the current release: the rules for the TEAC UD-H01, the T+A DAC 8 and Playback Designs startup are written down in the implementation ledger and not enabled. The intake keeps a startup hold-off for such rules: a per-pipe count of feedback bus opportunities to skip, consumed in submission order by each successfully completed feedback transfer (empty or failed packets inside it still count). Release construction (`FeedbackCompatibility::for_device` in `transport/iso.rs`, called when the output thread sets up the feedback transfers in `transport/output.rs`) sets that count to zero for every device, so no report is skipped today. The same construction leaves the Tenor correction off.

`StreamFormat.pacing` identifies `ExplicitFeedback(endpoint)`. Live lock is separate. Before the first valid report the stream primes at the nominal rate with verification withheld. After a valid report, the stale threshold is the larger of 500 ms or three report intervals. The terminal deadline is the larger of 1 s or six report intervals. The driver estimates the interval from the endpoint's polling cadence and, where present, its valid UAC1 `bRefresh`. At stale, it emits `FeedbackStale` and clears bit-perfect verification. A timely valid report can restore lock, and a report that arrives after the deadline cannot renew it. When the deadline expires the app gets a `TRANSFER` failure. If the first feedback transfer cannot be submitted the stream fails at once; a later submission that fails is tried again after 5 ms while the deadline still runs; a stalled endpoint or a device that disappears fails the stream. [Feedback diagnostics](controls.md#diagnostics-the-clock-and-the-pacing)

**The timing policy, exactly.** `FeedbackSmoothing::Direct` (Kotlin `FeedbackSmoothing.DIRECT`) is the default. Each accepted selected report replaces the scheduler's rate directly as a fractional value; the rate is never rounded to an integer before scheduling. Whole-frame packets use the existing fractional accumulator, whose phase survives report changes and rejected reports. A burst processed before another OUT refill therefore leaves its newest accepted rate in effect. The intake still selects the newest valid packet within each completed feedback transfer: earlier packets in that completion cannot change OUT transfers that are already queued. Each completed transfer commits in order, preserving calibration and rediscovery rules.

`FeedbackSmoothing::Ewma` (Kotlin `FeedbackSmoothing.EWMA`, JSON `"ewma"`) retains the previous behavior: the first accepted report sets the rate, then `new = old + 0.2 x (report - old)` once per completed transfer. This retains legacy update timing and alpha. Implicit feedback continues to use its measured packet vectors, and nominal/capture timing does not consult this option.

Opt-in support traces record `explicit_feedback_policy` at startup. Each `explicit_feedback_rate` observation separates the accepted selected report from the fractional rate actually applied by the scheduler and records the policy. Existing raw feedback statistics and `feedbackRateHz` remain observations of the reports, rather than the filtered scheduling rate.

**Evidence.**

- **Fixed-point rounding at 352.8 kHz.** The rate is 44.1 frames per high-speed
  microframe. Rounded to the nearest representable value, it is off by 0.049 Hz
  in 16.16 and 0.195 Hz in 10.14. Both errors are within half a bit of precision
  (0.061 Hz and 0.244 Hz respectively). usbdac's tests use that bound. Flick's
  pull requests #414 and #415 record that the earlier 0.01 Hz assertions fail
  on `versionF`. [Feedback arithmetic](evidence.md#computed-numbers)
- **The formats are in the specifications.** USB 2.0 section 5.12.4.2 says a full-speed endpoint reports an unsigned 10.10 value left-justified in three bytes, so it reads as 10.14, and a high-speed endpoint reports an unsigned 12.13 value in four bytes with the binary point between the second and third byte, so it reads as 16.16. Only 10 and 13 fractional bits are required; the lower 4 and 3 bits are optional extra precision, so a DAC may report coarser values than the decoder can hold (13 fractional bits is a step of about 0.98 Hz at 8000 microframes a second). UAC 1.0 section 3.7.2.2 gives the 10.14 format for the same three bytes, and the Linux driver says in a comment that full-speed devices use 10.14 and high-speed devices 16.16.
- **A deep queue delays the correction.** Mornye's revert `580a9bbfe` (2026-11-19, quoted under [decision 1](decisions-stream.md#queue-geometry)) is the one record of what happens when feedback takes effect late: a DAC that steers its own buffer swung past the accepted band and the stream paused. It says the mechanism was "read from the code, not measured". usbdac's 16 ms bus queue and its poll window follow from the same reasoning.
- **Recovery has a deadline.** Mornye's revert message reports frequent pauses
  from its two-second rule. usbdac's host tests cover initial lock, empty
  polls, recovery before the deadline and failure after it, malformed reports,
  USB timebase conversion, packet capacity, the Q32.32 SuperSpeed report, the
  order of the two queued polls and a failed second submission at start.
- **Hardware.** On the AP80 Pro-X, explicit feedback locked for PCM
  and for native DSD64, DSD128 and DSD256 with zero underruns and lost frames,
  and the DC04U locked at about 44,099 Hz in the SDK sample. Those runs were
  made on builds from before the two queued polls (commit `1b695de`, 0.1.3).
  The two-transfer polling has host tests only.
  [Evidence and status](evidence.md#hardware-status)

**Trade-off and compatibility.** Direct timing, the default, follows accepted
feedback swings immediately, including report jitter, while the queued OUT
audio still imposes its existing bus latency. EWMA reduces report jitter but
adds response lag during real changes such as FIFO recentering. Clients get
Direct without a configuration change; choose EWMA explicitly to keep the old
filter. Capacity, health deadlines and bit-perfect conditions still apply; an
unverified clock has a narrower feedback acceptance window, and unit
rediscovery requires fresh post-alternate proof. Two queued polls keep the
endpoint polled through a slow completion, and cost one more transfer and its
buffer. The release has automated coverage for the validation and recovery
rules. No run compares the two policies on a DAC, so which suits a given DAC
better is not known. The two DACs that locked and played without loss did so
on builds that still smoothed every report (before `e26dd2d` made Direct the
default).

**References.** [UAC 1.0 and USB 2.0 feedback format](references.md#ref-uac1), [USB 2.0 §5.12.4.2](references.md#ref-usb2), [UAC 2.0 formats §2.3.1.1 packet sizes](references.md#ref-uac2-formats), [USB 3.2 feedback format](references.md#ref-usb32), [Linux `endpoint.c`](references.md#ref-linux-endpoint), [Mornye commits](references.md#ref-mornye-commits).

## 8. DACs that cannot say how fast they want the data {#implicit-feedback}

**The question.** Some asynchronous DACs have no feedback endpoint. Instead they expect the host to derive the pacing from the DAC's own recording stream ("implicit feedback"), which is common on USB interfaces that also record. What does the driver do?

**In plain terms.** For a compatible device that uses its own clock for both
directions, the frame counts in its IN packets tell the host how many frames
to send back. That lets playback follow the device's actual clock. With nominal
timing alone, a 200 ppm difference would use up a 16 ms cushion in about
80 seconds. This is an illustration of clock drift.

| | What it does | Where |
|---|---|---|
| Flick | Streams at the nominal rate, host-paced, and logs "Async sink exposes no feedback endpoint; host-paced nominal timing ... Drift beyond that cushion will still drop out" | `rust/src/uac2/android_direct.rs` (the feedback branch of the stream start) |
| Mornye | Refuses: "Implicit feedback is not supported"; the README says implicit feedback, ambiguous clocks and multiple configurations "fail closed" | `usb_audio/transport.rs` (`Session::configure`) |
| BitChord | Refuses, with the comment "Implicit feedback means following the DAC's capture stream: not supported" | `usb/UsbDescriptors.kt` (the endpoint plan) |
| Linux `snd-usb-audio` | Implements it: pairs an asynchronous playback endpoint with an adjacent capture endpoint (or a UAC 2.0 implicit-feedback data endpoint) and copies the packet sizes it receives | `sound/usb/implicit.c`, `endpoint.c` |
| usbdac | Resolves a compatible IN source in the same audio function (UAC2, or UAC3 with a marked input and a shared clock), then uses its packet frame counts. If the request has no usable feedback route, negotiation can choose another suitable alternate setting or return `ImplicitFeedbackUnsupported` | `transport/scan.rs`, `transport/implicit.rs`, `transport/implicit_ranges.rs`, `transport/output.rs` |

**What usbdac does.** A usable pair is in one audio function and shares a resolved clock source (compared by exact ratio when a multiplier is involved), a wire rate and a service interval. The source is the IN endpoint that the playback endpoint names, or an endpoint marked as carrying implicit feedback data, or the adjacent interface at the same alternate setting. Both endpoints need packet capacity for whole frames at the rate. The capability list publishes only the rate ranges that the playback and the IN alternate both carry, and names the IN alternate that exact negotiation will pick. If two physical sources tie at a rate, that rate has no proven implicit route. The driver starts the IN stream first, converts each IN packet's length to whole frames, and schedules the same frame count in the corresponding OUT packet using the playback frame size. It checks every IN completion (status, whole-frame length, capacity, order) before using it, and measures the age of the timing from the USB callback. The app still supplies the playback samples. Android's SDK claims eligible IN interfaces as needed and releases them when the session closes. `Pacing.ImplicitFeedback` identifies the interface, alternate setting and endpoint. Check `feedbackLocked` for the live state. When playback and capture share the IN endpoint (duplex), the capture reader receives the data from the same transfer pool, and a slow reader cannot stall playback.

*Gaps in the IN timing.* One isolated gap (an IN completion with failed or missing timing packets) is tolerated: lock is cleared and the stream keeps the frame counts it already holds. Trust returns only after a full 100 ms window of valid timing. A second gap before that window completes ends the stream (`TimingBudgetExhausted`), and so does a gap that lasts past the recovery deadline (100 ms plus the queued time), an overflow of the pattern FIFO, or an empty IN completion while playing, unless the source-backed `IFB_SILENCE_ON_EMPTY` rule supplies its one measured silence pattern. A failed stream needs a new `openStream`. The Linux driver has no such limit: it keeps queuing OUT patterns through any number of bad IN packets (a failed packet becomes a zero-size entry), and on a FIFO overflow it stops the PCM with an xrun that the application recovers by preparing it again (`snd_usb_handle_sync_urb` and `notify_xrun` in `sound/usb/endpoint.c`).

**Evidence.** Host tests cover source pairing, rate ranges and capacity limits
for UAC2 and UAC3 pairs, whole-frame packet sizing, ordered consumption of frame
counts, shared and independent duplex readers, and Kotlin diagnostics.
**No implicit-feedback stream has run on a DAC.** Linux `snd-usb-audio`
implements the same pairing in `implicit.c`. The exact device rules that the
implementation ledger takes from its tables (fixed timing pipes, and the Roland and
Pioneer vendor PCM profiles that rely on implicit feedback) are written and tested on the
host, and switched off in the current release: only the descriptor-based pairing above is active.
[Evidence and status](evidence.md#hardware-status)

**Trade-off.** Implicit pacing requires an IN stream that stays usable while
playback runs. If no compatible source is available, `OutputSelector` can try
another route. `allowUnsynchronizedAsync` is a separate opt-in for otherwise
valid routes using nominal timing. It carries the `UNSYNCHRONIZED_ASYNC`
blocker for the entire stream.

**References.** [Linux implicit feedback](references.md#ref-linux-implicit), [UAC 2.0 and USB 2.0 synchronisation types](references.md#ref-usb2), [Mornye README](references.md#ref-mornye-commits).

Source-backed `GET_SAMPLE_RATE` suppresses only the UAC1 endpoint readback, matching its source scope. UAC2/3 clock-source reads remain enabled. An inferred adaptive rate is not clock verification; only actual readback evidence can establish that status.
