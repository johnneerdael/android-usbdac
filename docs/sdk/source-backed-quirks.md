# Source-backed device compatibility

`DacConfig.quirk_policy` defaults to `VerifiedOnly`, preserving the existing shipped compatibility tier. `SourceBacked` additionally selects implemented factual rules from the pinned Linux v7.3 source. These rules are **not hardware verified by this project**. Library bit-perfect checks describe the current sample/clock path, not a hardware qualification result.

The transport reads VID, PID and `bcdDevice` through the opened USB fd. Caller product names and claimed revisions cannot activate imported source rules. Exact identity rows with no upstream revision bounds apply to all revisions; no revision interval is invented. Native Amanero word-order rules additionally require their exact documented revision. Vendor-wide and manufacturer-string-disambiguated flag rows are retained as reference data and do not activate automatically.

The [flag facts](../alsa-quirk-flags-v7.3.json) contain 168 exact device rows, 30 vendor rows and two string rows, with a pinned source link for every row. Typed public `SOURCE_QUIRKS` metadata exposes flags and their applicability. An exact rule can have implemented and open flags; selecting it applies only the implemented subset and logs each flag's status. No flag silently authorizes a vendor mode write or fabricates clock/volume evidence.

The [descriptor facts](../alsa-descriptor-quirks-v7.3.json) account for 162 literal USB identity match sites in `quirks-table.h`, with original match scope, quirk categories and source lines. Macro-generated identities remain explicitly linked to the pinned source. Those registration/descriptor plans are reference-only; their MIDI, composite, endpoint and fixed-format behavior is not claimed implemented. Independently justified standard descriptor corrections are implemented separately: UAC1 AudioTrak Optoplay `0a92:0053` omits Sampling Frequency Control, while Creative Audigy 2 NX `041e:3020` and M-Audio Audiophile USB `0763:2003` require that standard control despite the absent descriptor bit. The correction preserves exact selected endpoint/rate checks and never turns SET acknowledgment into readback proof. [Pinned source](https://github.com/torvalds/linux/blob/c03c1a14507dfaf8b856c2608f9ac216021fc222/sound/usb/quirks.c#L2117-L2131).

Exact native-DSD alternate/word-order facts already represented in the driver are selectable in the source-backed tier after complete descriptor checks. The special Amanero revisions remain exact. A known native-only alternate is excluded from PCM/DoP admission even when its original descriptor spells the format PCM. Exact RAW_DATA flag rows require an unambiguous stereo UAC2 RAW_DATA alternate with four-byte words and use the source-defined big-endian word order. Vendor-wide RAW_DATA matches, legacy Playback Designs carrier inference and ITF mode transactions remain inactive. ALSA adds a native DSD format to the last alternate of flagged ITF devices; that alone does not prove the alternate is invalid for PCM. ALSA also switches the vendor mode for PCM or DoP as well as native DSD. This library does not perform those mode writes or establish their state and restoration, so the flag does not guarantee mode selection on those devices. [Upstream format rule](https://github.com/torvalds/linux/blob/c03c1a14507dfaf8b856c2608f9ac216021fc222/sound/usb/quirks.c#L1875-L1879), [alternate rule](https://github.com/torvalds/linux/blob/c03c1a14507dfaf8b856c2608f9ac216021fc222/sound/usb/quirks.c#L1958-L1968), [mode switch](https://github.com/torvalds/linux/blob/c03c1a14507dfaf8b856c2608f9ac216021fc222/sound/usb/quirks.c#L1795-L1830). [Pinned layout and RAW_DATA facts](https://github.com/torvalds/linux/blob/c03c1a14507dfaf8b856c2608f9ac216021fc222/sound/usb/quirks.c#L2010-L2114).

Playback minimum-is-mute and linear-amplitude quirks retain raw range lattices, exact readback and raw hand-back. Linear amplitude maps the verified minimum to silence and maximum to unity; startup protection rounds down on the actual lattice so it cannot exceed its ceiling. Silence is represented as negative infinity in Rust and the explicit string `"-Infinity"` on the JSON bridge. Capture volume controls do not currently have an equivalent public control surface.

## Flag coverage

| Source flag | Behavior or remaining scope |
|---|---|
| `GET_SAMPLE_RATE` | UAC1-only source-backed opt-in; an unusable reply may permit an explicitly unverified tier, not proof of rate |
| `SHARE_MEDIA_DEVICE` | Linux kernel media-controller ownership; no equivalent in this userspace driver |
| `ALIGN_TRANSFER` | complete-frame alignment is enforced; capture fragments are rejected rather than silently aligned |
| `TX_LENGTH` | implemented for exact Zoom standard UAC1 PCM playback interface1: four-byte LE audio-length prefix, capacity and loss exclude header |
| `PLAYBACK_FIRST` | implemented only with a proven implicit PCM IN/OUT pair; bounded existing nominal-silence bootstrap |
| `SKIP_CLOCK_SELECTOR` | implemented: preserve selector reads and current route; refuse any selector-changing write |
| `IGNORE_CLOCK_SOURCE` | Denon154e:500e requires the fixed8ch packed24/48k composite descriptor replacement in quirks-table.h2447-2499; no matching replacement owner, so arbitrary broken-clock bypass is forbidden |
| `ITF_USB_DSD_DAC` | DSD alternate interpretation and PCM/DSD vendor mode writes are unsupported; restoration ownership required |
| `CTL_MSG_DELAY` | implemented; source-backed opt-in, hardware unverified |
| `CTL_MSG_DELAY_1M` | implemented; source-backed opt-in, hardware unverified |
| `CTL_MSG_DELAY_5M` | implemented; source-backed opt-in, hardware unverified |
| `IFACE_DELAY` | implemented; source-backed opt-in, hardware unverified |
| `VALIDATE_RATES` | descriptor/range checks and exact readback admission apply; explicit unusable-readback policy remains unverified |
| `DISABLE_AUTOSUSPEND` | Linux kernel runtime-PM policy; app cannot disable kernel autosuspend through audio-class controls |
| `IGNORE_CTL_ERROR` | implemented only for optional mixer discovery; mandatory attenuation, rate, SET and live readback failures remain visible |
| `DSD_RAW` | implemented only for exact identity, unambiguous stereo UAC2 RAW_DATA and 4-byte words; no vendor-wide activation |
| `SET_IFACE_FIRST` | implemented; source-backed opt-in, hardware unverified |
| `GENERIC_IMPLICIT_FB` | already supported only when topology and cadence prove a source |
| `SKIP_IMPLICIT_FB` | implemented; source-backed opt-in, hardware unverified |
| `IFACE_SKIP_CLOSE` | not implemented: alternate hand-back requires a device-specific lease contract |
| `FORCE_IFACE_RESET` | already enforced by alt-zero reset before each stream |
| `FIXED_RATE` | implemented only with fixed descriptor and exact live frequency/validity proof; never an ACK substitute |
| `MIC_RES_16` | Mic Capture Volume mixer step=16 raw 1/256dB units (mixer.c1079-1085); this driver exposes no capture mixer volume route |
| `MIC_RES_384` | Mic Capture Volume mixer step=384 raw 1/256dB units (mixer.c1073-1078); this driver exposes no capture mixer volume route |
| `MIXER_PLAYBACK_MIN_MUTE` | implemented: raw minimum reports silence; raw hand-back preserved |
| `MIXER_CAPTURE_MIN_MUTE` | capture mixer minimum means silence (mixer_quirks.c4791-4796); no public capture mixer control owner exists, and playback must not inherit this rule |
| `SKIP_IFACE_SETUP` | already enforced: capability discovery does not activate alternates or program clocks; stream activation remains mandatory |
| `MIXER_PLAYBACK_LINEAR_VOL` | implemented: verified raw lattice mapped as linear amplitude, safety ceiling rounded down |
| `MIXER_CAPTURE_LINEAR_VOL` | capture mixer linear-amplitude TLV (mixer_quirks.c4805-4811); no public capture mixer control owner exists, and playback must not inherit this rule |
| `IFB_SILENCE_ON_EMPTY` | implemented for at most one measured-cadence silence pattern; no startup proof or feedback lease renewal |
| `MIXER_GET_CUR_OK` | already enforced: no invasive volume calibration probes; verified live GET_CUR always used |
| `PLAYBACK_URB_FIXUP` | implemented; source-backed opt-in, hardware unverified |
| `ALWAYS_SET_RATE` | already enforced for programmable playback clocks; SET is never skipped for cached rate |

## Transport evidence and limits

Supported ERROR/TIMEOUT/OVERFLOW/STALL packet statuses on completed OUT transfers can be charged in full when submitted geometry is valid. Reported bytes from failed packets are unconfirmed and never credited; a neighboring legal packet cannot erase the charge. Malformed descriptors, removal, cancellation and unsupported statuses remain terminal. Whole-transfer timeout accounting retains its existing separate conservative contract. Existing loss budgets and first-loss bit-perfect clearing remain unchanged.

Adaptive queuing changes the active URB count after a loss-bearing OUT completion (including coalesced callbacks, with at most one growth per worker batch) and lowers it after thirty quiet seconds. It preserves packets per URB, the 255-packet ceiling, sixteen-slot ceiling and 64 ms queue ceiling. Paired implicit-feedback pools retain their proven fixed geometry. The PLAYBACK_URB_FIXUP source rule requests a deeper twelve-URB starting geometry while retaining all caps. TX_LENGTH is limited to the exact Zoom standard UAC1 PCM playback profile on interface 1: each packet has a four-byte little-endian audio-byte-count prefix, reserved in wire capacity and excluded from payload audit, played frames and loss budgets. One bounded implicit silence pattern may reuse a previously measured, locked cadence; it consumes no application frames, contributes no rate evidence and never renews source health. Fresh source proof is required before another concealment. Source-backed PLAYBACK_FIRST uses the existing bounded OUT-silence bootstrap only for a proven implicit PCM source pair.

Feedback accepts 7/8 through 3/2 nominal before endpoint capacity checks. Direct fractional timing is the default; explicit `FeedbackSmoothing::Ewma` retains the legacy alpha-0.2 filter. Unit rediscovery requires an exact post-alt clock cross-check and three new completions. Diagnostics retain unsmoothed min/max/mean/population variance; opt-in trace observations separately record the policy and actual applied scheduler rate. The audible smoothing trade-off has not been measured.

A narrow default nominal tier admits full-speed UAC1 PCM without a feedback pipe. The stream always reports UNSYNCHRONIZED_ASYNC. A topped-up host ring is normal and never causes failure. Sustained empty-ring observations report producer starvation without claiming DAC FIFO drift: the host ring cannot measure that FIFO. No inserted/dropped samples conceal drift.

Clock fallback requires a successful SET and unusable readback under host policy. Positive mismatches, failed SET, multiplied routes and DSD retain strict proof. Three unusable replies are learned only for the current opened device session and exact control interface/clock entity; later attempts still read GET_CUR and readable values clear the suggestion. No learned rule survives close.

Worker 10 ms and 50 ms counters and `maxWorkerGapUs` record gaps outside the event pump. `maxWorkerLoopGapUs` records observed loop elapsed time, `maxEventPumpElapsedUs` measures pump duration, and `maxEventPumpTimeoutOverrunUs` measures pump time beyond the requested wait. None alone attributes a delay to OS scheduling. No screen-off, Doze or OEM hardware claim follows from host tests.


Successful compatibility admission emits typed `CompatibilityFallback`
notifications after the first healthy OUT submission: `nominal_async` for a
feedback-free asynchronous pipe and `unverified_clock` for acknowledged SET
with unusable readback. The event reaches DAC and stream listeners through
the existing deduplicating path; strict failures emit no admission event.
Opt-in support traces retain this admission and a bounded, exact
interface/entity three-strike clock suggestion. See
[compatibility admission diagnostics](support-diagnostics.md#compatibility-admission).
