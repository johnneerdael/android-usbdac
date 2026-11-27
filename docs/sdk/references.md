# Public references

Every source the [design decisions](decisions.md) cite, with what it was used for and a short excerpt from the page. The original links were fetched on 2026-11-26; the UAC3 format reference was added on 2026-11-27, and the UAC 2.0 formats document and the USB 2.0 pipe clause on 2026-11-27 from local text copies of the USB-IF PDFs. The USB 3.2 entry was added for 0.1.5 without fetching the document and has no excerpt. Every other excerpt was found in the fetched text; a source that did not say what a claim needed is listed under [what the sources do not say](#gaps). The Linux files are cited on the `master` branch of the mainline tree, which moves; the function names are the stable part.

## Standards

### USB 2.0 specification {#ref-usb2}

[usb.org document library](https://www.usb.org/document-library/usb-20-specification) (the archive `usb_20_20250603.zip` holds `usb_20.pdf`).

- **Feedback format, section 5.12.4.2.** A full-speed endpoint reports "an unsigned 10.10 (K=10) format" left-justified in three bytes "so that it has a 10.14 format"; a high-speed endpoint "an unsigned 12.13 (K=13) format which fits into four bytes", aligned "so that it has a 16.16 format". The lower 4 (full speed) and 3 (high speed) bits are optional extra precision.
- **Pipes, section 4.7 (p. 20).** "In general, data movement though one pipe is independent from the data flow in any other pipe", while control transfers can be used for "control of other pipes on the device". This is general independence. It promises nothing about host scheduling or error handling for streams that share a clock, a transfer owner or a feedback dependency.
- **No retry, section 5.6 and 8.5.5.** "In the case of a delivery failure due to error, no retrying of the attempt to deliver the data"; "Isochronous transactions do not support a handshake phase or retry capability."
- Used in [decisions 1, 3, 7 and 8](decisions.md).

### USB 3.2 specification {#ref-usb32}

[usb.org document library, USB 3.2 Revision 1.1](https://www.usb.org/document-library/usb-32-revision-11-june-2022).

- **Enhanced SuperSpeed feedback, section 4.4.8.4.1.** An eight-byte report, read by usbdac as an unsigned Q32.32 count of samples per 125 microsecond bus interval. The endpoint companion descriptor (section 9.6.7) gives the bytes per service interval that usbdac uses as packet capacity.
- These section numbers are the ones the code comments and the implementation ledger give. This page's text was not fetched for this guide.
- Used in [decision 7](decisions-clock.md#feedback).

### USB Audio Class 1.0 {#ref-uac1}

[`audio10.pdf`](https://www.usb.org/sites/default/files/audio10.pdf).

- **Feedback, section 3.7.2.2.** "These three bytes contain the Ff value in a 10.14 format."
- **Sampling frequency on the endpoint, section 5.2.3.2.3.1**, with the control selector `SAMPLING_FREQ_CONTROL` (3 bytes).
- **Volume, section 5.2.2.4.3.2.** CUR, MIN, MAX and RES attributes in steps of 1/256 dB.
- Used in [decisions 6, 7, 9 and 18](decisions.md).

### USB Audio Class 2.0 {#ref-uac2}

[`Audio2_with_Errata_and_ECN_through_Sep_14_2026.pdf`](https://www.usb.org/sites/default/files/Audio2_with_Errata_and_ECN_through_Sep_14_2026.pdf).

- **Clock entities.** "A Clock Selector Entity represents a multi-input source selector, capable of selecting among a number of clock signals" (section 5.2.5.2.1); the Sampling Frequency Control of a Clock Source "is the only way provided by this specification to change the sampling clock" (section 3.13.11.1); the control selector is `CS_SAM_FREQ_CONTROL`.
- **Volume, section 5.2.5.7.2.** The volume attribute can "range from +127.9961 dB (0x7FFF) down to -127.9961 dB (0x8001) in steps of 1/256 dB", with CUR and RANGE requests.
- The document refers the feedback format to the USB 2.0 specification, which is why that is the source above.
- Used in [decisions 6, 9, 11 and 18](decisions.md).

### USB Audio Class 2.0 audio data formats {#ref-uac2-formats}

`Frmts20 final.pdf`, Release 2.0, May 31, 2006, in the [USB-IF Audio 2.0 archive](https://www.usb.org/sites/default/files/Audio2.0_final.zip).

- **Packet sizes, section 2.3.1.1 (pp. 13-14).** "If the sampling rate is a constant, the allowable variation on ni is limited to one audio slot", so every packet carries "INT(nav) audio slots (small VFP) or INT(nav) + 1 (large VFP)". When the average is a whole number, "ni may vary between INT(nav) - 1 (small VFP), INT(nav) (medium VFP) and INT(nav) + 1 (large VFP)". "The sink must be capable to accept a large VFP at all times." This, not the USB 2.0 specification, is the source of the one-frame packet rule.
- **Type I Raw Data, section 2.3.1.7.5 (p. 17).** Data "in the form of PCM-like audio slots when the actual format or even the meaning of the transported data is unknown"; it "can never be interpreted inside the audio function". The format bit therefore names a candidate, not a DSD layout.
- Used in [decisions 5 and 7](decisions.md) and [what usbdac improves](improvements.md).

### USB Audio Class 3.0 audio data formats {#ref-uac3-formats}

[`Frmts30.pdf`, USB-IF Release 3.0, mirrored copy](https://git.nefarius.at/nefarius/USB-Bluetooth-Specs/media/commit/4762eed5fd4d612bef1a5a43ee87f4f4047bdbe5/Frmts30.pdf).

- **DSD format, section 2.3.1.6.5.** Native DSD uses 64-bit subslots (`bBitResolution = 64`, `bSubslotSize = 8`) at one sixty-fourth of the DSD sampling rate. This is a standard format, distinct from legacy device-specific DSD layouts.
- **Type I format bitmap, Table A-2.** PCM and DSD have distinct format bits. A PCM DoP carrier therefore uses PCM framing, including on UAC3.
- The current [USB-IF audio specification archive](https://www.usb.org/document-library/usb-audio-devices-release-40-and-adopters-agreement) makes the little-endian 64-bit DSD word convention explicit: D0 is newest, hence D63 is oldest. The SDK accepts chronological, MSB-first interleaved bytes and packs them accordingly.
- Section numbers are those of the linked initial release. In the April 2019 errata release (`Frmts30-Errata`), DSD Format is section 2.3.1.7.5 (p. 14) and PCM Format section 2.3.1.7.1 (p. 13).
- Used in [decision 5](decisions-stream.md#dsd) and [bit-perfect output](bit-perfect.md#dsd).

### DSD over PCM (DoP) open standard 1.1 {#ref-dop}

[`DoP_openStandard_1v1.pdf`](https://www.dsd-guide.com/sites/default/files/white-papers/DoP_openStandard_1v1.pdf) from dsd-guide.com.

- "The 8 most significant bits are used for the DSD marker and alternate with each sample between 0x05 and 0xFA"; the carrier is 24-bit PCM at 176.4 kHz for a 64fs (2.8224 MHz) DSD stream.
- Used in [decision 5](decisions-stream.md#dsd).

## The Linux kernel's USB audio driver

These are the mainline `snd-usb-audio` sources. They show what a mature driver does for the same problems.

### Feedback handling {#ref-linux-endpoint}

[`sound/usb/endpoint.c`](https://linux.googlesource.com/linux/kernel/git/torvalds/linux/+/4c7d8eb9a79ae5400eac19c4f6f0815bff674452/sound/usb/endpoint.c#1852).
`snd_usb_handle_sync_urb` describes the standard full-speed and high-speed
units, then infers a power-of-two shift by comparing the first usable report
with the nominal rate. It applies the shift to subsequent reports and
range-checks the result before storing it. Used in [decision 7](decisions-clock.md#feedback).

### Clock walk {#ref-linux-clock}

[`sound/usb/clock.c`](https://github.com/torvalds/linux/blob/master/sound/usb/clock.c). `__uac_clock_find_source` follows selectors (it reads the current pin, one-based) and multipliers to the clock source; a comment says "FIXME: multipliers only act as pass-thru element for now". The sample rate is then set on that source. Used in [decision 6](decisions-clock.md#clock-path).

### Queue limits {#ref-linux-card}

[`sound/usb/card.h`](https://github.com/torvalds/linux/blob/master/sound/usb/card.h): `MAX_PACKS 6` per URB, `MAX_PACKS_HS (MAX_PACKS * 8)`, `MAX_URBS 12`, and `MAX_QUEUE 18 /* try not to exceed this queue length, in ms */`. `MAX_QUEUE` is a soft limit measured in milliseconds; the limit on URBs is `MAX_URBS`. Used in [decisions 1 and 2](decisions-stream.md#queue-geometry).

### Implicit feedback {#ref-linux-implicit}

[`sound/usb/implicit.c`](https://github.com/torvalds/linux/blob/master/sound/usb/implicit.c) ("Special handling for implicit feedback mode") pairs an asynchronous playback endpoint with an adjacent capture endpoint or a UAC 2.0 implicit-feedback data endpoint. The [ALSA configuration guide](https://docs.kernel.org/sound/alsa-configuration.html) documents the `implicit_fb` option of `snd-usb-audio`: "When this is set and the playback stream sync mode is ASYNC, the driver tries to tie an adjacent ASYNC capture stream as the implicit feedback source." Used in [decision 8](decisions-clock.md#implicit-feedback).

### Native DSD quirks {#ref-linux-quirks}

[`sound/usb/quirks.c`](https://github.com/torvalds/linux/blob/master/sound/usb/quirks.c), `snd_usb_interface_dsd_format_quirks`, identifies legacy native DSD devices and their byte order (`DSD_U32_BE` or `DSD_U32_LE`). Its comment about the absence of a designated format bit applies to those legacy paths; UAC3 defines a standard DSD format separately. Used in [decision 5](decisions-stream.md#dsd).

### xHCI ring segments {#ref-linux-xhci}

[`drivers/usb/host/xhci.h`](https://github.com/torvalds/linux/blob/master/drivers/usb/host/xhci.h) sets `TRBS_PER_SEGMENT 256`; [`xhci-mem.c`](https://github.com/torvalds/linux/blob/master/drivers/usb/host/xhci-mem.c) (`xhci_set_link_trb`) makes the last TRB of a segment a Link TRB, which leaves 255 usable. The 2015 [patch that raised the size from 64 to 256](https://marc.info/?l=linux-usb&m=143040325106650&w=2) gives its reason as keeping the event ring from filling. **These establish the numbers only.** They say nothing about audio queue depth, and the connection to the 256-packet stall in [decision 1](decisions-stream.md#queue-geometry) is a hypothesis of Flick PR #400, not something the source shows.

## Android

### USB host {#ref-android-usb-host}

[USB host overview](https://developer.android.com/develop/connectivity/usb/host) (permission, `requestPermission`, `claimInterface`), [`UsbDeviceConnection`](https://developer.android.com/reference/android/hardware/usb/UsbDeviceConnection) (`claimInterface(intf, force)`: "true to disconnect kernel driver if necessary"; `getFileDescriptor()`: "intended for passing to native code") and [`UsbConstants`](https://developer.android.com/reference/android/hardware/usb/UsbConstants), where `USB_ENDPOINT_XFER_ISOC` is "Isochronous endpoint type (currently not supported)". That is why isochronous audio needs usbfs through libusb. Used in [decisions 16 and 18](decisions-library.md#claiming).

### Bit-perfect mixer and preferred mixer attributes {#ref-android-mixer}

[`AudioMixerAttributes`](https://developer.android.com/reference/android/media/AudioMixerAttributes) (API 34; `MIXER_BEHAVIOR_BIT_PERFECT`), [`AudioManager`](https://developer.android.com/reference/android/media/AudioManager) (`setPreferredMixerAttributes`, `getSupportedMixerAttributes`), and the AOSP page [preferred mixer attributes](https://source.android.com/docs/core/audio/preferred-mixer-attr): "The bit-perfect mode enables playback for master quality authenticated (MQA) and direct stream digital (DSD) formats on compatible digital to analog converters (DACs)." The attribute is optional in the audio HAL, and the platform currently allows it for USB devices and media usage only. Used in [decisions 14 and 17](decisions-library.md#outputs).

### AAudio {#ref-android-aaudio}

[AAudio guide](https://developer.android.com/ndk/guides/audio/aaudio/aaudio): `AAUDIO_SHARING_MODE_EXCLUSIVE` "has exclusive access to its audio device"; "it might not be possible for the stream to have exclusive access". The [NDK reference](https://developer.android.com/ndk/reference/group/audio) says for `AAudioStreamBuilder_setSharingMode` that the requested mode "may not be available" and the app can query the actual mode. Used in [decisions 13 and 17](decisions-library.md#outputs).

### PCM encodings {#ref-android-audioformat}

[`AudioFormat`](https://developer.android.com/reference/android/media/AudioFormat): `ENCODING_PCM_24BIT_PACKED` and `ENCODING_PCM_32BIT` are API 31 and "not guaranteed to be supported by devices". Used in [decision 17](decisions-library.md#outputs) and [the managed output](transports.md).

### Remote volume {#ref-android-volumeprovider}

[`VolumeProvider`](https://developer.android.com/reference/android/media/VolumeProvider), [`MediaSession.setPlaybackToRemote`](https://developer.android.com/reference/android/media/session/MediaSession) and the AndroidX [`VolumeProviderCompat`](https://developer.android.com/reference/androidx/media/VolumeProviderCompat) (marked deprecated since androidx.media 1.8.0). `VOLUME_CONTROL_ABSOLUTE` lets the system panel set the DAC's level directly. Used in [decision 12](decisions-volume.md#keys-and-idle).

### Media3 `AudioSink` {#ref-android-media3-audiosink}

[`AudioSink`](https://developer.android.com/reference/androidx/media3/exoplayer/audio/AudioSink): `configure`, `handleBuffer`, `play`, `pause`, `flush`, `getCurrentPositionUs`. Used in [decision 4](decisions-stream.md#pause-and-flush) and [the Media3 page](media3.md).

## libusb {#ref-libusb-async}

[Asynchronous device I/O](https://libusb.sourceforge.io/api-1.0/group__libusb__asyncio.html): for isochronous transfers "the actual_length field of the transfer is meaningless" and each packet's own `actual_length` and `status` must be read; `libusb_cancel_transfer` is asynchronous and "freeing the transfer after it has been cancelled but before cancellation has completed will result in undefined behaviour". [`libusb_wrap_sys_device`](https://libusb.sourceforge.io/api-1.0/group__libusb__dev.html) wraps a file descriptor, and [`LIBUSB_OPTION_NO_DEVICE_DISCOVERY`](https://libusb.sourceforge.io/api-1.0/group__libusb__lib.html) is "typically needed on Android". Used in [decisions 1, 3, 4, 16 and 18](decisions.md).

## Signal processing

### Resampling {#ref-jos-resampling}

Julius O. Smith, [The Digital Audio Resampling Home Page](https://ccrma.stanford.edu/~jos/resample/) (CCRMA, Stanford): band-limited interpolation with a windowed sinc, as a table of filter phases. The word "polyphase" is used in the same author's [Spectral Audio Signal Processing](https://ccrma.stanford.edu/~jos/sasp/Filtering_Downsampling.html), which says "the downsampler outputs ... are called polyphase signals". Used in [decision 15](decisions-reporting.md#conversion).

### Dither {#ref-dither}

Lipshitz, Wannamaker and Vanderkooy, [Quantization and Dither: A Theoretical Survey](https://hajim.rochester.edu/ece/sites/zduan/teaching/ece472/reading/Lipshitz_1992.pdf), J. Audio Eng. Soc. 40(5), 1992 (a university copy): "triangular-pdf dither of 2-LSB peak-to-peak amplitude is unique and optimal in the sense that it renders the first and second moments of the total error input independent". It does not make the error independent of the input in every moment. Used in [decision 15](decisions-reporting.md#conversion).

### MQA {#ref-mqa}

[Master Quality Authenticated](https://en.wikipedia.org/wiki/Master_Quality_Authenticated) (Wikipedia; there is no primary source here). "distributed as a single 24-bit stream, with the most significant bits occupied by PCM audio compatible with non-MQA playback equipment ... with the lower-order bits rendered as noise by equipment without an MQA decoder." Used in [decision 14](decisions-reporting.md#mqa).

## Records in the three drivers' repositories {#projects}

These are the project records the decisions quote. They are evidence of what the authors saw and said, with the authors' own caveats, and not independent measurements.

### Flick pull request #400 {#ref-flick-pr400}

[moss-apps/Flick#400](https://github.com/moss-apps/Flick/pull/400): the interface claim, and the geometry sweep with its CPU cost table. Its commits include [`49ceb71e`](https://github.com/moss-apps/Flick/commit/49ceb71e8), [`8e722749`](https://github.com/moss-apps/Flick/commit/8e7227493) and [`69cb3c50`](https://github.com/moss-apps/Flick/commit/69cb3c50d).

### Flick pull request #406 {#ref-flick-pr406}

[moss-apps/Flick#406](https://github.com/moss-apps/Flick/pull/406): legal packet sizes, timeout resubmission, exact loss accounting, and the fault-injection table. Its loss-accounting commit is [`e39794ac`](https://github.com/moss-apps/Flick/commit/e39794ac7).

### Flick pull requests #414 to #418 {#ref-flick-pr414-418}

[#414](https://github.com/moss-apps/Flick/pull/414) (pause, resume and seek; merged), [#415](https://github.com/moss-apps/Flick/pull/415) (volume on the stream's handle), [#416](https://github.com/moss-apps/Flick/pull/416) (live slider with a floor), [#417](https://github.com/moss-apps/Flick/pull/417) (volume keys) and [#418](https://github.com/moss-apps/Flick/pull/418) (idle release). The last four were open when this page was written.

### Commits on Flick's own branch {#ref-flick-commits}

On `feat/usb-dac-unified` of [johnneerdael/Flick](https://github.com/johnneerdael/Flick): [`1a183a52`](https://github.com/johnneerdael/Flick/commit/1a183a525) (follow clock selectors and multipliers), [`3d864cea`](https://github.com/johnneerdael/Flick/commit/3d864cea5) (a quarter second of buffer), [`0fbfbfe9`](https://github.com/johnneerdael/Flick/commit/0fbfbfe9b) (live DSP stays responsive), [`ce5f4a7d`](https://github.com/johnneerdael/Flick/commit/ce5f4a7d6) (report what the DAC played), [`4d23976a`](https://github.com/johnneerdael/Flick/commit/4d23976a3) (lost audio is not bit-perfect) and [`86ffa87f`](https://github.com/johnneerdael/Flick/commit/86ffa87f1), the extraction point named in `NOTICE`.

### Mornye commits and README {#ref-mornye-commits}

[mornyeapp/Mornye](https://github.com/mornyeapp/Mornye) at `af9a4d642`: the USB code is in `rust_backend/crates/mobile/src/usb_audio/` with its [README](https://github.com/mornyeapp/Mornye/blob/af9a4d642/rust_backend/crates/mobile/src/usb_audio/README.md). Commits quoted: [`09ff115ef`](https://github.com/mornyeapp/Mornye/commit/09ff115ef) and its revert [`580a9bbfe`](https://github.com/mornyeapp/Mornye/commit/580a9bbfe) (transfers in flight), [`30ec85296`](https://github.com/mornyeapp/Mornye/commit/30ec85296) (queue length), [`b98f81046`](https://github.com/mornyeapp/Mornye/commit/b98f81046) (clock selector), [`da608a663`](https://github.com/mornyeapp/Mornye/commit/da608a663) (clock range), [`208bd7569`](https://github.com/mornyeapp/Mornye/commit/208bd7569) and [`4b47a0745`](https://github.com/mornyeapp/Mornye/commit/4b47a0745) (resampling and dither), and [`7447af0ee`](https://github.com/mornyeapp/Mornye/commit/7447af0ee) (idle release).

### BitChord issue #1485 {#ref-bitchord-1485}

[kushagrasinghx/BitChord#1485](https://github.com/kushagrasinghx/BitChord/issues/1485): a user's report and patch on a Poco F9 Ultra, with the transfer counts and the attenuation left after a fallback.

### BitChord port commits {#ref-bitchord-port}

[`06628ed1`](https://github.com/kushagrasinghx/BitChord/commit/06628ed17) (the direct USB driver, "Phase 3 of porting Mornye's bit-perfect output paths") and [`ea2df7e4`](https://github.com/kushagrasinghx/BitChord/commit/ea2df7e47) (restore the DAC's level on close). The transport and volume code is under `app/src/main/cpp/bitperfect/` and `app/src/main/java/com/music/bitchord/playback/audio/bitperfect/`.

### usbdac {#ref-usbdac}

The usbdac commits behind the decisions, such as [`eada7d7`](https://github.com/johnneerdael/android-usbdac/commit/eada7d7bc) (the same 16 ms on the bus at every interval), are in [the repository](https://github.com/johnneerdael/android-usbdac).

## What the sources do not say {#gaps}

- **The Android docs do not say "fall back to shared".** They say an exclusive stream may not be available and that the app should query the mode it got. The fall-back is usbdac's reading.
- **The USB Audio 2.0 text does not state the feedback format.** It sends the reader to the USB 2.0 specification (section 5.12.4.2).
- **The MQA article does not say where the signalling is.** usbdac makes no claim about it.
- **The xHCI sources do not explain the 256-packet stall.** The explanation is a hypothesis.
- **The USB 3.2 section numbers are not checked against the PDF.** They come from the code and the ledger.
- **The Dither paper supports "first and second moments independent", not "fully independent".**
- **The UAC 2.0 RAW_DATA format does not say what the data is.** [The formats document](#ref-uac2-formats) defines it as uninterpreted PCM-like slots, so legacy native DSD layouts are cited through the Linux quirks; standard UAC3 DSD is cited through `Frmts30.pdf` above.
- **USB 2.0 section 4.7 does not make streams on one device independent in failure.** It describes pipes in general; whether one direction survives a fault in the other is usbdac's own rule.
- A third-party mirror of Alan Stern's `snd-usb-audio` URB rework, cited in Flick PR #400, redirected to a sign-in page and was not used.

The UAC1 §4.3.2.3 source spans pp. 40–41. The 2019 UAC3 errata numbers PCM Format as §2.3.1.7.1, p. 13. The DSF source is Version 1.01; BS.1770-5 inter-sample figures appear in Attachment 1 to Annex 2, p. 20.
