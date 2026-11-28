# Explicit DSD conversion

The standalone native DSD adapters accept chronological bytes interleaved one per channel, with the oldest bit in bit 7. They do not accept DoP carrier bytes or arbitrary PCM directly. The DoP adapter requires independent proof that the downstream route decodes DoP. It packs each pair as `[later, earlier, marker]` (or `[0, later, earlier, marker]` in a four-byte slot), alternates markers per frame, and holds at most one carrier frame. One incomplete source byte-frame is retained across writes; drain completes an incomplete final pair with the DSD idle byte `0x69` (ALSA's DSD silence) under the next marker. The native DSD direct adapter likewise completes a final partial wire word with `0x69` in every channel. A separate DoP input adapter validates common 05/FA markers and alternation, rejects nonzero padding, unpacks to the same native byte contract, and reports positions in carrier frames.

The PCM converter is always lossy. Standalone `DsdPcmOutput` and Rust `DsdPcmConverter` apply configured headroom exactly once: at the first stage when no PCM resampling is needed, otherwise after the PCM resampler. The automatic selector uses unity gain when its PCM target equals the DSD intermediate rate (`dsdBitRate / 16`), and applies configured headroom when those rates differ. Set `headroomDb = 0.0` (Rust `headroom_db`) for standalone unity; this does not make conversion lossless. The -3.5 dB default covers a measured PCM square-wave overshoot fixture, not every signal or intersample peak. The post-dither integer-quantizer saturation counter includes rounding and dither clamps, and cannot guarantee absence of intersample peaks. It accepts one to eight channels at DSD64, DSD128, DSD256 and DSD512 in both the 44.1 kHz family (2,822,400; 5,644,800; 11,289,600; 22,579,200 bits/s) and the 48 kHz family (3,072,000; 6,144,000; 12,288,000; 24,576,000 bits/s), subject to supported target PCM formats and rates. A 704-tap Kaiser-windowed low-pass decimates the 1-bit input by 16, then the existing high-quality PCM resampler and quantizer produce the requested PCM format. The first-stage transition spans 0.020–0.03125 cycles per DSD bit. At 20 kHz, normalized frequency is 0.00709, 0.00354 and 0.00177 for DSD64, DSD128 and DSD256 respectively. A dense production-coefficient test bounds passband ripple and stopband response; a separate full-chain tone test covers DSD64 to 44.1 kHz. These are software measurements, not an audible-noise or universal alias guarantee. The final resampler can reject ultrasonic images when downsampling; an equal-intermediate target retains the first-stage response. MPD and FFmpeg use different filters and passbands, so their published figures do not establish parity. The 704 taps require 88 lookup contributions per intermediate sample per channel, compared with 64 previously, and add 44 rather than 32 source byte-frames of first-stage delay. ±1 corresponds to 100% modulation: 50% is -6.02 dBFS at unity and -9.52 dBFS with standalone default headroom. History and filter phase survive chunk boundaries; flush resets them. At EOS a partial final pair is padded with DSD silence and the finite filter tail is emitted; the position is capped at the actual input count. The adapter exposes filter latency as source byte-frames, keeps playback positions in source time, and reports the outstanding source frames as latency; quantization and filtering are not bit-perfect and the adapter says so. Outputs and input are bounded; output backpressure leaves pending data in the adapter.

## Automatic direct playback

The shared production playback resolver chooses an allowed direct USB route and reports its selected format, planned and attempted candidates, transform and source fidelity. Native DSD and confirmed DoP can be retained or repacked when the target and policy permit; a carrier without independent decoder confirmation does not establish DoP decoding. When lossless transport is unavailable and policy permits conversion, the production source-domain stream converts supported native DSD or validated DoP input to the selected signed integer or Float32 PCM target. It supplies the selected conversion options, including headroom, to the same converter. The equal-intermediate unity rule and the configured headroom for a changed rate remain visible in the decision. A U8-only target does not provide a converted PCM fallback. The Android selector can also choose a platform route under its own policy; direct USB target evidence does not establish a platform-route or analog-output guarantee.

The resolver and converter support the expanded input rates above, but the measurements below cover only their stated original source rates and combinations. Hardware acceptance of a new automatic conversion remains separate from exact-format USB playback evidence.

## Bandwidth design and measured residual

The table gives the **design passband edge**, in kHz, as High / Fast. These are design edges rather than measured −3 dB bandwidths. For source bit rate `B` and target PCM rate `T`, it is `min(0.020 × B, q × T / 2)`, with `q = 0.91` for High and `0.90` for Fast. The first-stage edge is below either quality's edge at the intermediate rate `B / 16`, so this also covers upsampling and the equal-intermediate case, where the PCM resampler is bypassed. For this table only, the source bit rates are the original 44.1 kHz family values: 2,822,400, 5,644,800 and 11,289,600 bits/s for DSD64, DSD128 and DSD256 respectively. The expanded 48 kHz family and DSD512 inputs were not part of these measurements. The ten target rates are the two common 44.1 and 48 kHz families through 16×, also enumerated in the resampler rate tests. These rates are examples and the converter supports other rates.

| PCM target (kHz) | DSD64 High / Fast | DSD128 High / Fast | DSD256 High / Fast |
| ---: | ---: | ---: | ---: |
| 44.1 | 20.066 / 19.845 | 20.066 / 19.845 | 20.066 / 19.845 |
| 48 | 21.840 / 21.600 | 21.840 / 21.600 | 21.840 / 21.600 |
| 88.2 | 40.131 / 39.690 | 40.131 / 39.690 | 40.131 / 39.690 |
| 96 | 43.680 / 43.200 | 43.680 / 43.200 | 43.680 / 43.200 |
| 176.4 | 56.448 / 56.448 | 80.262 / 79.380 | 80.262 / 79.380 |
| 192 | 56.448 / 56.448 | 87.360 / 86.400 | 87.360 / 86.400 |
| 352.8 | 56.448 / 56.448 | 112.896 / 112.896 | 160.524 / 158.760 |
| 384 | 56.448 / 56.448 | 112.896 / 112.896 | 174.720 / 172.800 |
| 705.6 | 56.448 / 56.448 | 112.896 / 112.896 | 225.792 / 225.792 |
| 768 | 56.448 / 56.448 | 112.896 / 112.896 | 225.792 / 225.792 |

A software probe exercised the production mono Float32 converter at 0 dB headroom for the original 60 combinations: those three 44.1 kHz-family source rates, ten target rates, and two quality settings. The values do not measure the later-added 48 kHz-family or DSD512 inputs. The input was a 1 kHz sine at 50% modulation, encoded with a deterministic first-order delta-sigma modulator. Each measurement used the same 100 ms window starting 10 ms after input began. A one-sided FFT with 10 Hz bins excluded DC and the 1 kHz fundamental. The residual RMS values use full-scale amplitude 1 as the reference.

| Source | Residual through 20 kHz (dBFS) | Residual above 20 kHz (dBFS) |
| --- | ---: | ---: |
| DSD64 | −56.14 | −68.90 to −42.71 |
| DSD128 | −64.26 | −74.91 to −43.17 |
| DSD256 | −73.00 to −72.99 | −84.28 to −43.49 |

The 1 kHz peak gain relative to the input ranged from −0.000427 to +0.000035 dB. Summed FFT band powers agreed with independently computed time-domain residual power to within 1.3 × 10⁻¹² relative error. The residual includes harmonics and shaped quantization products from this test signal.

An earlier separate run used a 50% first-order modulated tone at `B / 16 − 2 kHz` and projected the output's 2 kHz bin. Its selected alias-bin **peak amplitude** ranged from −149.32 to −122.13 dBFS across the same 60 combinations. This is one bin for one deterministic stimulus. Both probes used the production converter and a coherent 0.1 s trimmed window. Audible performance and hardware behavior require separate evidence.


The DSD→PCM adapter accepts the caller's conversion quality, headroom, narrowing, and dither seed. PCM source-layout overrides are rejected because DSD has its own byte contract. Standalone conversion applies its configured headroom once whether or not the PCM stage changes rate; the selector’s equal-intermediate-rate unity policy is described above. PCM saturation counts are available through the adapter and through the DoP input wrapper when it owns that adapter.

For DoP converted to PCM, `SelectedOutput.resetDopInputMarkerPhase()` accepts a new input marker phase at a track boundary while preserving audio already queued. For direct DoP output, keep the carrier marker sequence continuous across joins; the reset method returns false.
