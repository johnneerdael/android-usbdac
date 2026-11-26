use super::*;

#[cfg(test)]
mod dsd_bitperfect_tests {
    use super::*;

    // Native DSD_U32: input is byte-interleaved [c0_b0, c1_b0, c0_b1, c1_b1, ...].
    // c0 bytes = 10,11,12,13 ; c1 bytes = 20,21,22,23.
    const NATIVE_IN: [i32; 8] = [10, 20, 11, 21, 12, 22, 13, 23];

    #[test]
    fn native_dsd_u32_big_endian_groups_channels() {
        let mut out = [0u8; 8];
        encode_usb_pcm_slots(
            &NATIVE_IN, &mut out, 4, 1, DsdTransportMode::Native, 2, true, false,
        )
        .unwrap();
        // BE: each channel word is b0(MSB)..b3(LSB), channel 0 then channel 1.
        assert_eq!(out, [10, 11, 12, 13, 20, 21, 22, 23]);
    }

    #[test]
    fn native_dsd_u32_little_endian_reverses_byte_order() {
        let mut out = [0u8; 8];
        encode_usb_pcm_slots(
            &NATIVE_IN, &mut out, 4, 1, DsdTransportMode::Native, 2, false, false,
        )
        .unwrap();
        assert_eq!(out, [13, 12, 11, 10, 23, 22, 21, 20]);
    }

    #[test]
    fn native_dsd_bit_reverse_flips_each_byte() {
        let mut out = [0u8; 8];
        encode_usb_pcm_slots(
            &NATIVE_IN, &mut out, 4, 1, DsdTransportMode::Native, 2, true, true,
        )
        .unwrap();
        let bytes: [u8; 8] = [10, 11, 12, 13, 20, 21, 22, 23];
        let expect: Vec<u8> = bytes.iter().map(|b| b.reverse_bits()).collect();
        assert_eq!(out.to_vec(), expect);
    }

    #[test]
    fn native_dsd_u8_passes_low_byte() {
        let input = [0x100u32 as i32, 0x205u32 as i32];
        let mut out = [0u8; 2];
        encode_usb_pcm_slots(
            &input, &mut out, 1, 1, DsdTransportMode::Native, 2, false, false,
        )
        .unwrap();
        assert_eq!(out, [0x00, 0x05]);
    }

    #[test]
    fn dop_word_32bit_emits_marker_in_msb() {
        // marker 0x05 in bits 31:24, dsd0=0x11 @16, dsd1=0x22 @8, low byte 0.
        let w = (0x05u32 << 24) | (0x11 << 16) | (0x22 << 8);
        let mut out = [0u8; 4];
        encode_dop_slots(&[w as i32], &mut out, 4, 32);
        assert_eq!(out, [0x00, 0x22, 0x11, 0x05]);
    }

    #[test]
    fn dop_word_24bit_emits_three_bytes_marker_last() {
        let w = (0xFAu32 << 24) | (0x11 << 16) | (0x22 << 8);
        let mut out = [0u8; 3];
        encode_dop_slots(&[w as i32], &mut out, 3, 24);
        assert_eq!(out, [0x22, 0x11, 0xFA]);
    }

    #[test]
    fn the_quirk_table_resolves_the_dsd_fields() {
        let info = crate::capabilities::DeviceInfo::new(12230, 61546, "MOONDROP Dawn Pro");
        let resolved = crate::settings::Settings::for_device(Default::default(), &info);
        assert!(resolved.dsd_big_endian());
        assert_eq!(resolved.dsd_subslot_override(), Some(4));
        assert!(!resolved.dsd_bit_reverse());
    }

    #[test]
    fn an_unknown_device_has_no_dsd_quirk() {
        let info = crate::capabilities::DeviceInfo::new(1, 2, "Mystery DAC");
        let resolved = crate::settings::Settings::for_device(Default::default(), &info);
        assert!(!resolved.dsd_big_endian());
        assert_eq!(resolved.dsd_subslot_override(), None);
    }

    #[test]
    fn audit_dop_valid_alternating_markers() {
        // ss=4 stereo: marker is the last byte of each 4-byte word; channel-0
        // words (L0,L1) alternate 0x05 -> 0xFA.
        let buf: [u8; 16] = [
            0xAA, 0xAA, 0xAA, 0x05, // L0
            0xAA, 0xAA, 0xAA, 0x05, // R0
            0xAA, 0xAA, 0xAA, 0xFA, // L1
            0xAA, 0xAA, 0xAA, 0xFA, // R1
        ];
        assert!(audit_dsd_payload(&buf, DsdTransportMode::DoP, 4, 2));
    }

    #[test]
    fn audit_dop_rejects_bad_marker_byte() {
        let buf: [u8; 8] = [0xAA, 0xAA, 0xAA, 0x05, 0xAA, 0xAA, 0xAA, 0x00];
        assert!(!audit_dsd_payload(&buf, DsdTransportMode::DoP, 4, 2));
    }

    #[test]
    fn dop_ring_preserves_phase_through_starvation_and_resume() {
        let mut ring = PcmRing::new(9, 2);
        assert_eq!(ring.capacity_samples % 2, 0);
        assert_eq!(ring.push_samples(&[0xfa112200u32 as i32, 0xfa334400u32 as i32]), 2);
        let mut first = [0i32; 6];
        assert!(ring.pop_into_or_pad(&mut first, 2, DsdTransportMode::DoP));
        assert_eq!(first.map(|s| s as u32), [
            0x05112200, 0x05334400, 0xfa696900, 0xfa696900, 0x05696900, 0x05696900,
        ]);
        assert_eq!(ring.push_samples(&[0x05556600, 0x05778800, 99]), 2);
        let mut resumed = [0i32; 2];
        assert!(!ring.pop_into_or_pad(&mut resumed, 2, DsdTransportMode::DoP));
        assert_eq!(resumed.map(|s| s as u32), [0xfa556600, 0xfa778800]);
    }

    #[test]
    fn dop_audit_rejects_disagreeing_channel_markers() {
        let payload = [0, 0, 0x05, 0, 0, 0xFA, 0, 0, 0xFA, 0, 0, 0x05];
        assert!(!audit_dsd_payload(&payload, DsdTransportMode::DoP, 3, 2));
    }

    #[test]
    fn audit_dop_rejects_non_alternating_or_short() {
        // two channel-0 words with identical marker -> no alternation
        let flat: [u8; 16] = [
            0xAA, 0xAA, 0xAA, 0x05, 0xAA, 0xAA, 0xAA, 0x05, 0xAA, 0xAA, 0xAA, 0x05, 0xAA, 0xAA,
            0xAA, 0x05,
        ];
        assert!(!audit_dsd_payload(&flat, DsdTransportMode::DoP, 4, 2));
        // fewer than 2 words
        assert!(!audit_dsd_payload(&[0, 0, 0, 0x05], DsdTransportMode::DoP, 4, 2));
    }

    #[test]
    fn audit_dop_24bit_marker_in_last_byte() {
        // ss=3: marker at byte[2]. L0=0x05, L1=0xFA (stereo -> 4 words).
        let buf: [u8; 12] = [
            0xAA, 0xAA, 0x05, 0xAA, 0xAA, 0x05, 0xAA, 0xAA, 0xFA, 0xAA, 0xAA, 0xFA,
        ];
        assert!(audit_dsd_payload(&buf, DsdTransportMode::DoP, 3, 2));
    }

    #[test]
    fn audit_native_requires_nonzero() {
        assert!(audit_dsd_payload(&[0x00, 0x55, 0x00], DsdTransportMode::Native, 1, 2));
        assert!(!audit_dsd_payload(&[0, 0, 0, 0], DsdTransportMode::Native, 1, 2));
        assert!(!audit_dsd_payload(&[], DsdTransportMode::Native, 1, 2));
    }

    #[test]
    fn audit_pcm_accepts_varying_16bit_stereo() {
        // stride = ss(2) * ch(2) = 4; channel-0 words at 0 and 4 differ.
        let buf: [u8; 8] = [
            0x00, 0x00, 0x00, 0x00, // L0 = 0
            0x01, 0x00, 0x00, 0x00, // L1 = 1
        ];
        assert!(audit_pcm_payload(&buf, 2, 2));
    }

    #[test]
    fn audit_pcm_rejects_dead_pipeline_all_zero() {
        let buf = [0u8; 64];
        assert!(!audit_pcm_payload(&buf, 2, 2));
    }

    #[test]
    fn audit_pcm_rejects_stuck_constant_dc() {
        // every 16-bit sample = 0x0100 (DC); channel-0 never varies.
        let buf: [u8; 16] = [
            0x00, 0x01, 0x00, 0x01, 0x00, 0x01, 0x00, 0x01, 0x00, 0x01, 0x00, 0x01, 0x00, 0x01,
            0x00, 0x01,
        ];
        assert!(!audit_pcm_payload(&buf, 2, 2));
    }

    #[test]
    fn audit_pcm_accepts_24bit_packed_subslot() {
        // ss=3 stereo, stride=6; channel-0 samples differ.
        let buf: [u8; 12] = [
            0x00, 0x00, 0x01, 0x00, 0x00, 0x02, // L0
            0xFF, 0xFF, 0x7F, 0x00, 0x00, 0x03, // L1
        ];
        assert!(audit_pcm_payload(&buf, 3, 2));
    }

    #[test]
    fn audit_pcm_rejects_empty_and_short() {
        assert!(!audit_pcm_payload(&[], 2, 2));
        assert!(!audit_pcm_payload(&[0, 0], 2, 2)); // < 2 frames
        assert!(!audit_pcm_payload(&[1, 2, 3, 4], 0, 2)); // zero subslot
    }

    #[test]
    fn effective_ring_rate_uses_byte_rate_for_native() {
        // DSD64: dsd_bit_rate 2_822_400 -> byte rate 352_800, wire rate 88_200.
        let native = PlaybackFormat {
            sample_rate: 88_200,
            bit_depth: 1,
            channels: 2,
            is_dop: false,
            dsd_transport: DsdTransportMode::Native,
            dsd_bit_rate: 2_822_400,
        };
        assert_eq!(effective_ring_rate(&native), 352_800);
        let pcm = PlaybackFormat {
            sample_rate: 96_000,
            bit_depth: 24,
            channels: 2,
            is_dop: false,
            dsd_transport: DsdTransportMode::None,
            dsd_bit_rate: 0,
        };
        assert_eq!(effective_ring_rate(&pcm), 96_000);
    }

    /// Half of the last fixed-point digit of a report, in Hz: the decoder cannot
    /// be closer to the true rate than the report's own resolution allows.
    fn half_lsb_hz(fraction_bits: u32, microframes_per_second: f64) -> f64 {
        0.5 * microframes_per_second / f64::from(1u32 << fraction_bits)
    }

    #[test]
    fn feedback_uses_usb_speed_timebase_not_feedback_poll_interval() {
        // UAC2 high-speed feedback is samples per microframe. A report of 44.1
        // at a 125 us data service interval is 352.8 kHz, regardless of whether
        // the feedback endpoint itself is polled every 1 ms. 44.1 is not
        // representable in 16.16, so the decoded rate is within half an LSB
        // (0.06 Hz at 8000 microframes/s) of 352800, not exact.
        let raw = (44.1f64 * 65_536.0).round() as u32;
        let (frames_per_packet, estimated_rate) =
            decode_feedback_report(&raw.to_le_bytes(), 125, Speed::High).unwrap();
        assert!((frames_per_packet - 44.1).abs() <= 0.5 / 65_536.0 + 1e-12);
        assert!((estimated_rate - 352_800.0).abs() <= half_lsb_hz(16, 8_000.0) + 1e-9);

        // A value that 16.16 holds exactly decodes exactly.
        let exact = ((44.0 + 0.125) * 65_536.0) as u32;
        let (frames, rate) = decode_feedback_report(&exact.to_le_bytes(), 125, Speed::High).unwrap();
        assert_eq!(frames, 44.125);
        assert_eq!(rate, 353_000.0);
    }

    #[test]
    fn feedback_full_speed_uses_one_millisecond_timebase() {
        let raw = (48.0f64 * 16_384.0) as u32;
        let bytes = [raw as u8, (raw >> 8) as u8, (raw >> 16) as u8];
        let (frames_per_packet, estimated_rate) =
            decode_feedback_report(&bytes, 1_000, Speed::Full).unwrap();
        assert!((frames_per_packet - 48.0).abs() < 0.001);
        assert!((estimated_rate - 48_000.0).abs() < 0.01);
    }

    #[test]
    fn feedback_q10_14_uses_high_speed_timebase() {
        // 10.14 feedback is still samples per microframe on a high-speed link.
        // Its resolution is 1/16384 frame per microframe, i.e. 0.49 Hz at
        // 8000 microframes/s, so 44.1 decodes to within 0.25 Hz of 352800.
        let raw = (44.1f64 * 16_384.0).round() as u32;
        let bytes = [raw as u8, (raw >> 8) as u8, (raw >> 16) as u8];
        let (frames_per_packet, estimated_rate) =
            decode_feedback_report(&bytes, 125, Speed::High).unwrap();
        assert!((frames_per_packet - 44.1).abs() <= 0.5 / 16_384.0 + 1e-12);
        assert!((estimated_rate - 352_800.0).abs() <= half_lsb_hz(14, 8_000.0) + 1e-9);

        let exact = ((44.0 + 0.25) * 16_384.0) as u32;
        let bytes = [exact as u8, (exact >> 8) as u8, (exact >> 16) as u8];
        let (frames, rate) = decode_feedback_report(&bytes, 125, Speed::High).unwrap();
        assert_eq!(frames, 44.25);
        assert_eq!(rate, 354_000.0);
    }
}

#[cfg(test)]
mod transport_loss_tests {
    use super::*;

    fn streaming_state(runtime_stats: Arc<RuntimeStats>) -> SessionState {
        test_state(Some(runtime_stats))
    }

    /// The PCM wire audit is process-global. Satisfy it so the only thing that
    /// can change `direct_path_is_bit_perfect` in these tests is the latch.
    fn new_stream_stats() -> Arc<RuntimeStats> {
        USB_PCM_PAYLOAD_LATCH.lock().observe(true, Instant::now());
        Arc::new(RuntimeStats::new(44_100, 2, 200, 100, 100, 100))
    }

    fn lossy_tracker(lost_frames: u64) -> LossTracker {
        let mut tracker = LossTracker::new(LossLimits {
            max_consecutive_lossy_urbs: USB_MAX_CONSECUTIVE_LOSSY_URBS,
            max_lost_frames: u64::MAX,
        });
        tracker.record(
            44,
            UrbLoss {
                lost_frames,
                ..UrbLoss::default()
            },
        );
        tracker
    }

    #[test]
    fn lost_frames_clear_the_bit_perfect_claim_until_a_new_stream() {
        let _latch = PAYLOAD_LATCH_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let stats = new_stream_stats();
        let state = streaming_state(Arc::clone(&stats));
        assert!(direct_path_is_bit_perfect(&state));

        publish_transport_loss(&stats, &lossy_tracker(3));
        assert!(stats.transport_loss_latched());
        assert_eq!(stats.lost_frames.load(Ordering::Relaxed), 3);
        assert!(!direct_path_is_bit_perfect(&state));
        assert_eq!(
            bit_perfect_release_reason(&state),
            "audio lost on the USB transport"
        );

        // A new stream gets fresh runtime stats, so it starts unlatched.
        let next_stream = streaming_state(new_stream_stats());
        assert!(direct_path_is_bit_perfect(&next_stream));
    }

    #[test]
    fn a_lossless_tracker_never_latches_the_stream() {
        let _latch = PAYLOAD_LATCH_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let stats = new_stream_stats();
        let state = streaming_state(Arc::clone(&stats));
        publish_transport_loss(&stats, &lossy_tracker(0));
        assert!(!stats.transport_loss_latched());
        assert_eq!(stats.lost_frames.load(Ordering::Relaxed), 0);
        assert!(direct_path_is_bit_perfect(&state));
    }

    #[test]
    fn publish_mirrors_every_counter() {
        let _latch = PAYLOAD_LATCH_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let stats = new_stream_stats();
        let mut tracker = LossTracker::new(LossLimits {
            max_consecutive_lossy_urbs: USB_MAX_CONSECUTIVE_LOSSY_URBS,
            max_lost_frames: u64::MAX,
        });
        tracker.record(
            44,
            UrbLoss {
                timed_out: true,
                lost_frames: 44,
                short_packets: 0,
                zero_packets: 0,
                held: false,
            },
        );
        tracker.record(
            44,
            UrbLoss {
                timed_out: false,
                lost_frames: 6,
                short_packets: 2,
                zero_packets: 1,
                held: false,
            },
        );
        publish_transport_loss(&stats, &tracker);
        assert_eq!(stats.lost_frames.load(Ordering::Relaxed), 50);
        assert_eq!(stats.short_packets.load(Ordering::Relaxed), 2);
        assert_eq!(stats.zero_packets.load(Ordering::Relaxed), 1);
        assert_eq!(stats.data_urb_timeouts.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn the_lost_audio_budget_is_about_a_tenth_of_a_second() {
        let frames = lost_frame_budget(44_100, USB_MAX_LOST_AUDIO_MS);
        assert_eq!(frames, 4_410);
        assert_eq!(USB_MAX_CONSECUTIVE_LOSSY_URBS, 3);
    }
}

#[cfg(test)]
mod pcm_ring_hold_tests {
    use super::*;

    fn dop_frame(marker_bits: u32, payload: u32) -> [i32; 2] {
        [(marker_bits << 24 | payload) as i32; 2]
    }

    fn markers(output: &[i32], channels: usize) -> Vec<u32> {
        output
            .chunks_exact(channels)
            .map(|frame| {
                assert!(
                    frame
                        .iter()
                        .all(|s| (*s as u32) >> 24 == (frame[0] as u32) >> 24),
                    "channels of one frame must share a marker: {frame:x?}"
                );
                (frame[0] as u32) >> 24
            })
            .collect()
    }

    #[test]
    fn clear_discards_queued_audio_and_waits_for_a_whole_transfer() {
        let mut ring = PcmRing::new(16, 2);
        assert_eq!(ring.push_samples(&[1, 2, 3, 4, 5, 6, 7, 8]), 8);
        ring.clear();
        assert_eq!(ring.len_samples(), 0);

        // Nothing to play yet: hold silence instead of reporting an underrun.
        assert!(ring.awaiting_refill(4));
        assert_eq!(ring.push_samples(&[20, 21]), 2);
        assert!(ring.awaiting_refill(4), "half a transfer is not enough");
        assert_eq!(ring.push_samples(&[22, 23]), 2);
        assert!(!ring.awaiting_refill(4));

        let mut out = [0i32; 4];
        assert!(!ring.pop_into_or_pad(&mut out, 2, DsdTransportMode::None));
        assert_eq!(out, [20, 21, 22, 23], "only post-clear audio is played");

        // The hold is one-shot: a later drain is a genuine underrun again.
        assert!(!ring.awaiting_refill(4));
        assert!(ring.pop_into_or_pad(&mut out, 2, DsdTransportMode::None));
    }

    #[test]
    fn paused_pcm_silence_does_not_consume_the_ring() {
        let mut ring = PcmRing::new(16, 2);
        assert_eq!(ring.push_samples(&[1, 2, 3, 4, 5, 6]), 6);

        let mut first = [0i32; 2];
        assert!(!ring.pop_into_or_pad(&mut first, 2, DsdTransportMode::None));
        assert_eq!(first, [1, 2]);

        let mut paused = [9i32; 8];
        ring.fill_wire_silence(&mut paused, DsdTransportMode::None);
        assert_eq!(paused, [0; 8]);
        assert_eq!(ring.len_samples(), 4, "pause must keep the queued audio");

        let mut resumed = [0i32; 4];
        assert!(!ring.pop_into_or_pad(&mut resumed, 2, DsdTransportMode::None));
        assert_eq!(
            resumed,
            [3, 4, 5, 6],
            "nothing lost or repeated across pause"
        );
    }

    #[test]
    fn paused_native_dsd_silence_is_the_idle_pattern_not_dc() {
        let mut ring = PcmRing::new(16, 2);
        assert_eq!(ring.push_samples(&[0x11, 0x22]), 2);

        let mut paused = [0i32; 4];
        ring.fill_wire_silence(&mut paused, DsdTransportMode::Native);
        assert_eq!(paused, [0x69; 4]);
        assert_eq!(ring.len_samples(), 2);
    }

    #[test]
    fn dop_markers_alternate_across_paused_and_resumed_pops() {
        let mut ring = PcmRing::new(16, 2);
        // Stale marker bits on the queued frames must not leak onto the wire.
        for frame in [
            dop_frame(0xAA, 0x112233),
            dop_frame(0xAA, 0x445566),
            dop_frame(0xAA, 0x778899),
        ] {
            assert_eq!(ring.push_samples(&frame), 2);
        }

        let mut before = [0i32; 2];
        assert!(!ring.pop_into_or_pad(&mut before, 2, DsdTransportMode::DoP));
        let mut paused = [0i32; 6];
        ring.fill_wire_silence(&mut paused, DsdTransportMode::DoP);
        let mut after = [0i32; 4];
        assert!(!ring.pop_into_or_pad(&mut after, 2, DsdTransportMode::DoP));

        let mut wire = Vec::new();
        wire.extend_from_slice(&before);
        wire.extend_from_slice(&paused);
        wire.extend_from_slice(&after);
        assert_eq!(
            markers(&wire, 2),
            [0x05, 0xFA, 0x05, 0xFA, 0x05, 0xFA],
            "one unbroken 05/FA alternation across pause and resume"
        );
        assert_eq!(paused.map(|s| s as u32 & 0x00ff_ffff), [0x696900; 6]);
        assert_eq!(
            after.map(|s| s as u32 & 0x00ff_ffff),
            [0x445566, 0x445566, 0x778899, 0x778899],
            "the retained audio resumes where it stopped"
        );
    }

    #[test]
    fn clear_keeps_the_dop_marker_phase() {
        let mut ring = PcmRing::new(16, 2);
        let mut first = [0i32; 2];
        ring.fill_wire_silence(&mut first, DsdTransportMode::DoP);
        assert_eq!(markers(&first, 2), [0x05]);

        ring.clear();
        let mut next = [0i32; 4];
        ring.fill_wire_silence(&mut next, DsdTransportMode::DoP);
        assert_eq!(markers(&next, 2), [0xFA, 0x05]);
    }

    pub(super) fn pcm_candidate() -> IsoStreamCandidate {
        IsoStreamCandidate {
            interface_number: 1,
            alt_setting: 1,
            endpoint_address: 0x01,
            endpoint_interval: 1,
            service_interval_us: 125,
            usb_speed: Speed::High,
            max_packet_bytes: 1024,
            sync_type: SyncType::Adaptive,
            usage_type: UsageType::Data,
            format_tag: FORMAT_TAG_PCM,
            channels: 2,
            subslot_size: 2,
            bit_resolution: 16,
            sample_rates: vec![48_000],
            refresh: 0,
            synch_address: 0,
            feedback_endpoint: None,
            terminal_link: None,
            is_uac1: false,
        }
    }

    fn prepare(
        ring: &Arc<Mutex<PcmRing>>,
        stats: &RuntimeStats,
        paused: bool,
    ) -> IsoTransferPayload {
        stats.paused.store(paused, Ordering::Release);
        let candidate = pcm_candidate();
        let mut scheduler = IsoPacketScheduler::new(48_000, 4, candidate.service_interval_us);
        prepare_iso_transfer_payload(
            &mut scheduler,
            ring,
            stats,
            &candidate,
            2,
            2,
            4,
            DsdTransportMode::None,
            false,
            false,
        )
        .unwrap()
        .unwrap()
    }

    fn deep_ring_stats() -> RuntimeStats {
        let config = crate::config::DacConfig::default();
        RuntimeStats::new(
            48_000,
            2,
            config.buffer_capacity_ms as usize,
            config.buffer_target_ms as usize,
            config.start_threshold_ms as usize,
            config.processing_buffer_target_ms as usize,
        )
    }

    #[test]
    fn held_transfers_are_silent_unconsumed_and_not_underruns() {
        let _latch = PAYLOAD_LATCH_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let stats = deep_ring_stats();
        let threshold = stats.start_threshold_samples;
        let ring = Arc::new(Mutex::new(PcmRing::new(
            stats.buffer_capacity_samples,
            2,
        )));
        // Distinct 16-bit values so the payload audit sees real audio.
        let music: Vec<i32> = (1..=20_000).map(|i| i << 16).collect();
        assert_eq!(ring.lock().push_samples(&music), 20_000);
        USB_PCM_PAYLOAD_LATCH.lock().reset();

        let paused = prepare(&ring, &stats, true);
        assert!(!paused.underrun);
        assert!(paused.held, "a paused transfer is marked held");
        assert!(paused.transfer_buffer.iter().all(|byte| *byte == 0));
        assert_eq!(ring.lock().len_samples(), 20_000, "pause consumes nothing");
        {
            let latch = USB_PCM_PAYLOAD_LATCH.lock();
            assert!(!latch.is_verified());
            assert_eq!(latch.constant_for(Instant::now()), None, "audit untouched");
        }

        let playing = prepare(&ring, &stats, false);
        assert!(!playing.underrun);
        assert!(!playing.held, "audio from the ring is never held");
        assert!(playing.transfer_buffer.iter().any(|byte| *byte != 0));
        let consumed = 20_000 - ring.lock().len_samples();
        assert_eq!(consumed, playing.packet_samples.len());
        assert_eq!(
            playing.packet_samples[0],
            1 << 16,
            "resume starts at the pause point"
        );
        assert!(USB_PCM_PAYLOAD_LATCH.lock().is_verified());

        // A flushed ring holds silence until it has the start threshold.
        ring.lock().clear();
        let flushed = prepare(&ring, &stats, false);
        assert!(!flushed.underrun, "a flush is not an underrun");
        assert!(flushed.held, "the post-flush refill wait is held silence");
        assert!(flushed.transfer_buffer.iter().all(|byte| *byte == 0));
        assert_eq!(ring.lock().len_samples(), 0);

        // Below the threshold the hold goes on, however much has arrived.
        assert_eq!(ring.lock().push_samples(&music[..threshold - 2]), threshold - 2);
        let waiting = prepare(&ring, &stats, false);
        assert!(waiting.held && !waiting.underrun);
        assert_eq!(ring.lock().len_samples(), threshold - 2, "a hold consumes nothing");

        // At the threshold it ends, and a later drain is a genuine underrun.
        assert_eq!(ring.lock().push_samples(&music[..2]), 2);
        let refilled = prepare(&ring, &stats, false);
        assert!(!refilled.underrun && !refilled.held);
        let queued = ring.lock().len_samples();
        ring.lock()
            .pop_into_or_pad(&mut vec![0; queued], 2, DsdTransportMode::None);
        let drained = prepare(&ring, &stats, false);
        assert!(drained.underrun, "a drain after the refill is an underrun");
        assert!(!drained.held, "an underrun is padding, not a hold");
    }

    #[test]
    fn the_buffer_is_deep_but_the_stream_starts_and_resumes_on_a_small_threshold() {
        let stats = deep_ring_stats();
        let frames_per_ms = 48;
        assert_eq!(stats.buffer_target_samples, frames_per_ms * 250 * 2);
        assert_eq!(stats.buffer_capacity_samples, frames_per_ms * 500 * 2);
        assert_eq!(stats.start_threshold_samples, frames_per_ms * 100 * 2);
        assert_eq!(stats.processing_target_samples, frames_per_ms * 100 * 2);
        assert!(stats.start_threshold_samples < stats.buffer_target_samples);
        assert!(stats.buffer_target_samples < stats.buffer_capacity_samples);

        // A target below the threshold lowers the threshold with it.
        let shallow = RuntimeStats::new(48_000, 2, 100, 50, 100, 50);
        assert_eq!(shallow.start_threshold_samples, shallow.buffer_target_samples);
    }

    #[test]
    fn software_gain_switches_the_recommended_depth_to_the_processing_target() {
        let stats = deep_ring_stats();
        assert_eq!(stats.active_target_samples(), stats.buffer_target_samples);
        stats.gain_bits.store(0.5f32.to_bits(), Ordering::Relaxed);
        assert_eq!(stats.active_target_samples(), stats.processing_target_samples);
        stats.gain_bits.store(1.0f32.to_bits(), Ordering::Relaxed);
        assert_eq!(stats.active_target_samples(), stats.buffer_target_samples);
    }

    #[test]
    fn a_flush_clears_the_ring_and_starts_a_new_epoch() {
        let stats = deep_ring_stats();
        let ring = Mutex::new(PcmRing::new(stats.buffer_capacity_samples, 2));
        let music: Vec<i32> = (1..=4_000).collect();
        assert_eq!(ring.lock().push_samples(&music), 4_000);
        stats.buffered_samples.store(4_000, Ordering::Relaxed);
        stats.inflight_music_frames.store(96, Ordering::Relaxed);
        stats.draining.store(true, Ordering::Relaxed);

        assert_eq!(flush_ring(&ring, &stats), 4_000);
        assert_eq!(ring.lock().len_samples(), 0);
        assert_eq!(stats.buffered_samples.load(Ordering::Relaxed), 0);
        assert_eq!(stats.inflight_music_frames.load(Ordering::Relaxed), 0);
        assert_eq!(stats.flush_epoch.load(Ordering::Relaxed), 1);
        assert!(
            !stats.draining.load(Ordering::Relaxed),
            "a flush ends drain so the stream can be reused"
        );

        // Audio written after the flush is queued normally.
        assert_eq!(ring.lock().push_samples(&music[..960]), 960);
        assert_eq!(ring.lock().len_samples(), 960);
        assert_eq!(flush_ring(&ring, &stats), 960);
        assert_eq!(stats.flush_epoch.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn a_resubmitted_slot_carries_nothing_over_from_the_completed_transfer() {
        // A timed-out URB is accounted from its own queued frame count when it
        // completes, then the slot is resubmitted with a fresh payload. Nothing
        // of the finished transfer may survive into the next one, or the same
        // frames would be counted again.
        let mut slot = IsoTransferSlot::new(0, 16, 1024).unwrap();
        slot.queued_frame_count = 96;
        slot.queued_underrun = true;
        slot.queued_held = true;
        slot.queued_music_frames = 96;
        slot.queued_epoch = 3;
        slot.in_flight = true;

        slot.mark_completed();

        assert_eq!(slot.queued_frame_count, 0);
        assert!(!slot.queued_underrun);
        assert!(!slot.queued_held);
        assert_eq!(slot.queued_music_frames, 0);
        assert!(!slot.in_flight);
    }

    #[test]
    fn ring_pops_report_how_much_of_the_output_was_music() {
        let mut ring = PcmRing::new(16, 2);
        assert_eq!(ring.push_samples(&[1, 2, 3, 4, 5, 6]), 6);

        let mut full = [0i32; 4];
        assert_eq!(
            ring.pop_music_into_or_pad(&mut full, 2, DsdTransportMode::None),
            RingPop {
                underrun: false,
                music_samples: 4
            }
        );

        // Two samples left for an eight-sample transfer: six are padding.
        let mut short = [9i32; 8];
        assert_eq!(
            ring.pop_music_into_or_pad(&mut short, 2, DsdTransportMode::None),
            RingPop {
                underrun: true,
                music_samples: 2
            }
        );
        assert_eq!(short, [5, 6, 0, 0, 0, 0, 0, 0]);

        // Dry: nothing but padding.
        let mut dry = [9i32; 4];
        assert_eq!(
            ring.pop_music_into_or_pad(&mut dry, 2, DsdTransportMode::None),
            RingPop {
                underrun: true,
                music_samples: 0
            }
        );
    }

    #[test]
    fn transfers_report_their_music_frames_and_the_flush_epoch_they_were_filled_in() {
        let _latch = PAYLOAD_LATCH_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let stats = deep_ring_stats();
        let threshold = stats.start_threshold_samples;
        let ring = Arc::new(Mutex::new(PcmRing::new(
            stats.buffer_capacity_samples,
            2,
        )));
        let music: Vec<i32> = (1..=20_000).map(|i| i << 16).collect();
        assert_eq!(ring.lock().push_samples(&music), 20_000);

        // Playing: every frame of the transfer is music.
        let playing = prepare(&ring, &stats, false);
        assert_eq!(playing.music_frames, playing.frame_count);
        assert_eq!(playing.epoch, 0);

        // Paused: a held transfer carries no music.
        let paused = prepare(&ring, &stats, true);
        assert_eq!(paused.music_frames, 0);
        assert!(paused.frame_count > 0);

        // A flush starts a new epoch; the refill wait is held silence.
        ring.lock().clear();
        let refilling = prepare(&ring, &stats, false);
        assert_eq!(refilling.music_frames, 0);
        assert_eq!(refilling.epoch, 1, "filled after the flush");

        // Back at the start threshold the hold ends and music flows again.
        assert_eq!(ring.lock().push_samples(&music[..threshold]), threshold);
        let full = prepare(&ring, &stats, false);
        assert!(!full.held);
        assert_eq!(full.music_frames, full.frame_count);
        assert_eq!(full.epoch, 1);

        // Underrun: only what the ring still had is music, the rest is padding.
        let queued = ring.lock().len_samples();
        ring.lock()
            .pop_into_or_pad(&mut vec![0; queued - 10], 2, DsdTransportMode::None);
        let dry = prepare(&ring, &stats, false);
        assert!(dry.underrun);
        assert_eq!(dry.music_frames, 5, "ten samples are five stereo frames");
        assert!(dry.music_frames < dry.frame_count, "padding is not music");
        assert_eq!(dry.epoch, 1);
    }

    #[test]
    fn output_latency_is_the_ring_plus_the_current_epochs_music_on_the_bus() {
        let stats = RuntimeStats::new(48_000, 2, 200, 100, 100, 100);
        // 4800 samples = 2400 frames = 50 ms in the ring.
        stats.buffered_samples.store(4_800, Ordering::Relaxed);
        assert_eq!(stats.output_latency_frames(), 2_400);

        // Three transfers on the bus: music of this epoch, held silence, and
        // music from before a flush.
        let mut slots: Vec<IsoTransferSlot> =
            (0..3).map(|i| IsoTransferSlot::new(i, 16, 1024).unwrap()).collect();
        for (slot, (epoch, music)) in slots.iter_mut().zip([(1, 96), (1, 0), (0, 96)]) {
            slot.in_flight = true;
            slot.queued_epoch = epoch;
            slot.queued_music_frames = music;
        }
        stats.flush_epoch.store(1, Ordering::Release);
        publish_inflight_music(&slots, &stats);
        assert_eq!(stats.inflight_music_frames.load(Ordering::Relaxed), 96);
        assert_eq!(stats.output_latency_frames(), 2_400 + 96);
        assert!((stats.output_latency_ms() - (2_496.0 * 1_000.0 / 48_000.0)).abs() < 1e-9);

        // A transfer that has completed is no longer in flight.
        slots[0].mark_completed();
        publish_inflight_music(&slots, &stats);
        assert_eq!(stats.inflight_music_frames.load(Ordering::Relaxed), 0);
        assert_eq!(stats.output_latency_frames(), 2_400);
    }
}

/// The push seams of the stream: what the host writes, what the output thread
/// prepares from it, and the counters in between. The output thread's USB I/O is
/// replaced by calling the same `prepare_iso_transfer_payload` and
/// `account_completed_transfer` the loop calls.
#[cfg(test)]
mod push_stream_tests {
    use super::pcm_ring_hold_tests::pcm_candidate;
    use super::*;
    use crate::events::{DacEvent, DacListener, EventHub, StreamState};
    use parking_lot::Mutex as PlMutex;

    #[derive(Default)]
    struct Collector(PlMutex<Vec<DacEvent>>);

    impl DacListener for Collector {
        fn on_event(&self, event: DacEvent) {
            self.0.lock().push(event);
        }
    }

    impl Collector {
        fn wait_for(&self, count: usize) -> Vec<DacEvent> {
            for _ in 0..400 {
                if self.0.lock().len() >= count {
                    break;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            self.0.lock().clone()
        }
    }

    struct Rig {
        shared: Arc<StreamShared>,
        hub: EventSink,
        events: Arc<Collector>,
        candidate: IsoStreamCandidate,
        scheduler: IsoPacketScheduler,
        played: PlayedMusicCounter,
        in_flight: Vec<IsoTransferPayload>,
    }

    impl Rig {
        fn new(subslot: u8, bits: u8, channels: u16, transport: DsdTransportMode) -> Self {
            let config = crate::config::DacConfig::default();
            let rate = 48_000;
            let stats = Arc::new(RuntimeStats::new(
                rate,
                usize::from(channels),
                config.buffer_capacity_ms as usize,
                config.buffer_target_ms as usize,
                config.start_threshold_ms as usize,
                config.processing_buffer_target_ms as usize,
            ));
            let ring = Arc::new(Mutex::new(PcmRing::new(
                stats.buffer_capacity_samples,
                usize::from(channels),
            )));
            let hub = EventHub::start();
            let events = Arc::new(Collector::default());
            hub.set_stream_listener(Some(events.clone()));
            let format = PlaybackFormat {
                sample_rate: rate,
                bit_depth: bits,
                channels,
                is_dop: transport == DsdTransportMode::DoP,
                dsd_transport: transport,
                dsd_bit_rate: 0,
            };
            let shared = Arc::new(StreamShared::new(ring, stats, hub.clone(), format, subslot));
            let mut candidate = pcm_candidate();
            candidate.subslot_size = subslot;
            candidate.bit_resolution = bits;
            candidate.channels = channels;
            candidate.max_packet_bytes = 4096;
            let scheduler = IsoPacketScheduler::new(
                rate,
                usize::from(subslot) * usize::from(channels),
                candidate.service_interval_us,
            );
            Self {
                shared,
                hub,
                events,
                candidate,
                scheduler,
                played: PlayedMusicCounter::default(),
                in_flight: Vec::new(),
            }
        }

        fn pcm(subslot: u8, bits: u8) -> Self {
            Self::new(subslot, bits, 2, DsdTransportMode::None)
        }

        /// What the output loop does to fill one slot.
        fn prepare(&mut self) -> IsoTransferPayload {
            let channels = usize::from(self.candidate.channels);
            let payload = prepare_iso_transfer_payload(
                &mut self.scheduler,
                &self.shared.ring,
                &self.shared.stats,
                &self.candidate,
                usize::from(self.candidate.subslot_size),
                channels,
                usize::from(self.candidate.subslot_size) * channels,
                self.shared.dsd_transport,
                false,
                false,
            )
            .unwrap()
            .unwrap();
            payload
        }

        /// Prepares a transfer and puts it on the simulated bus.
        fn submit(&mut self) -> usize {
            let payload = self.prepare();
            let frames = payload.frame_count;
            self.in_flight.push(payload);
            self.publish_inflight();
            frames
        }

        fn publish_inflight(&self) {
            let epoch = self.shared.stats.flush_epoch.load(Ordering::Acquire);
            self.shared.stats.inflight_music_frames.store(
                inflight_music_frames(
                    epoch,
                    self.in_flight
                        .iter()
                        .map(|payload| (payload.epoch, payload.music_frames as u64)),
                ),
                Ordering::Relaxed,
            );
        }

        /// The oldest transfer completes; the DAC took all but `lost` frames.
        fn complete_oldest(&mut self, lost: u64) {
            let payload = self.in_flight.remove(0);
            let stats = &self.shared.stats;
            stats
                .consumer_frames
                .fetch_add(payload.frame_count as u64 - lost, Ordering::Relaxed);
            account_completed_transfer(
                stats,
                &mut self.played,
                payload.epoch,
                payload.frame_count as u64,
                payload.music_frames as u64,
                lost,
            );
            self.publish_inflight();
        }
    }

    fn lock_globals() -> std::sync::MutexGuard<'static, ()> {
        let guard = PAYLOAD_LATCH_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        USB_PCM_PAYLOAD_LATCH.lock().reset();
        USB_DSD_PAYLOAD_LATCH.lock().reset();
        guard
    }

    /// Deterministic bytes that are not constant, for the payload audit.
    fn noise(len: usize, seed: u32) -> Vec<u8> {
        let mut state = seed | 1;
        (0..len)
            .map(|_| {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                (state >> 24) as u8
            })
            .collect()
    }

    #[test]
    fn pcm_wire_bytes_leave_exactly_as_they_were_written() {
        let _globals = lock_globals();
        // (subslot bytes, valid bits): 16 in 2, 24 in 3, 24 in 4 (left-justified,
        // low byte zero), 32 in 4.
        for (subslot, bits) in [(2u8, 16u8), (3, 24), (4, 24), (4, 32)] {
            let mut rig = Rig::pcm(subslot, bits);
            let frame_bytes = usize::from(subslot) * 2;
            let frames = 96 * 5; // five 125 us transfers at 48 kHz
            let mut input = noise(frames * frame_bytes, 7 + u32::from(subslot) * u32::from(bits));
            if (subslot, bits) == (4, 24) {
                for sample in input.chunks_exact_mut(4) {
                    sample[0] = 0;
                }
            }
            assert_eq!(rig.shared.write_bytes(&input).unwrap(), input.len());

            let mut wire = Vec::new();
            for _ in 0..5 {
                let payload = rig.prepare();
                assert!(!payload.underrun && !payload.held);
                wire.extend_from_slice(&payload.transfer_buffer);
            }
            assert_eq!(
                wire, input,
                "{subslot}-byte subslot, {bits} valid bits: the wire must carry the written bytes"
            );
        }
    }

    #[test]
    fn a_write_takes_whole_wire_frames_and_never_blocks() {
        let rig = Rig::pcm(3, 24); // 6-byte stereo frames
        assert_eq!(rig.shared.write_bytes(&[1, 2, 3, 4, 5]).unwrap(), 0);
        assert_eq!(rig.shared.write_bytes(&[0; 13]).unwrap(), 12);

        // Fill the ring: only whole frames fit, and a full ring takes nothing.
        let capacity_bytes = rig.shared.stats.buffer_capacity_samples * 3;
        let big = vec![1u8; capacity_bytes + 600];
        let taken = rig.shared.write_bytes(&big).unwrap();
        assert!(taken > 0 && taken % 6 == 0);
        assert!(taken <= capacity_bytes);
        assert_eq!(rig.shared.write_bytes(&big).unwrap(), 0);
        assert_eq!(rig.shared.writable_bytes(), 0);
    }

    #[test]
    fn write_i32_takes_whole_frames_of_ring_samples() {
        let rig = Rig::pcm(2, 16);
        assert_eq!(rig.shared.write_samples(&[1, 2, 3]).unwrap(), 2);
        assert_eq!(rig.shared.stats.buffered_samples.load(Ordering::Relaxed), 2);
        assert_eq!(rig.shared.stats.producer_frames.load(Ordering::Relaxed), 1);

        // Native DSD frames are subslot_bytes * channels ring samples.
        let dsd = Rig::new(4, 1, 2, DsdTransportMode::Native);
        assert_eq!(dsd.shared.write_samples(&[0x11; 7]).unwrap(), 0);
        assert_eq!(dsd.shared.write_samples(&[0x11; 9]).unwrap(), 8);
    }

    #[test]
    fn native_dsd_bytes_become_one_ring_sample_each() {
        let rig = Rig::new(4, 1, 2, DsdTransportMode::Native);
        assert_eq!(rig.shared.wire_frame_bytes(), 8);
        let bytes = [10u8, 20, 11, 21, 12, 22, 13, 23];
        assert_eq!(rig.shared.write_bytes(&bytes).unwrap(), 8);
        let mut out = [0i32; 8];
        rig.shared.ring.lock().pop_into_or_pad(&mut out, 2, DsdTransportMode::Native);
        assert_eq!(out, [10, 20, 11, 21, 12, 22, 13, 23]);
        // 0xFF must not turn negative.
        assert_eq!(rig.shared.write_bytes(&[0xFF; 8]).unwrap(), 8);
        rig.shared.ring.lock().pop_into_or_pad(&mut out, 2, DsdTransportMode::Native);
        assert_eq!(out, [255; 8]);
    }

    #[test]
    fn dop_words_keep_their_data_bits_and_the_library_owns_the_marker() {
        let mut rig = Rig::new(3, 24, 2, DsdTransportMode::DoP);
        // Two stereo frames of [dsd_low, dsd_high, marker]; the markers are junk.
        let bytes = [0x22u8, 0x11, 0xEE, 0x44, 0x33, 0xEE, 0x66, 0x55, 0xEE, 0x88, 0x77, 0xEE];
        assert_eq!(rig.shared.write_bytes(&bytes).unwrap(), 12);
        let mut out = [0i32; 4];
        rig.shared
            .ring
            .lock()
            .pop_into_or_pad(&mut out, 2, DsdTransportMode::DoP);
        assert_eq!(
            out.map(|s| s as u32),
            [0x05_11_22_00, 0x05_33_44_00, 0xFA_55_66_00, 0xFA_77_88_00]
        );
        let _ = &mut rig;
    }

    #[test]
    fn writable_bytes_fills_to_the_target_and_shrinks_with_software_gain() {
        let _globals = lock_globals();
        let rig = Rig::pcm(2, 16);
        let frame = 4;
        let target_bytes = rig.shared.stats.buffer_target_samples * 2;
        assert_eq!(rig.shared.writable_bytes(), target_bytes / frame * frame);

        // Half the target is queued: half is still writable.
        let queued = vec![3u8; target_bytes / 2];
        let taken = rig.shared.write_bytes(&queued).unwrap();
        assert_eq!(rig.shared.writable_bytes(), (target_bytes - taken) / frame * frame);

        // With software gain the recommended depth is the (shallower) processing
        // target, which is already exceeded by the queue above.
        let _session = TestSession::install_streaming(Arc::clone(&rig.shared.stats));
        rig.shared.set_software_gain(0.5).unwrap();
        let processing_bytes = rig.shared.stats.processing_target_samples * 2;
        assert_eq!(
            rig.shared.writable_bytes(),
            processing_bytes.saturating_sub(taken) / frame * frame
        );
        rig.shared.set_software_gain(1.0).unwrap();
        assert_eq!(rig.shared.writable_bytes(), (target_bytes - taken) / frame * frame);

        // Nothing is writable after drain().
        rig.shared.drain();
        assert_eq!(rig.shared.writable_bytes(), 0);
        assert!(matches!(
            rig.shared.write_bytes(&[0; 4]),
            Err(crate::Error::InvalidState(_))
        ));
        rig.shared.flush();
        assert!(rig.shared.write_bytes(&[0; 4]).is_ok());
    }

    #[test]
    fn pause_sends_silence_keeps_the_ring_and_resume_continues_exactly() {
        let _globals = lock_globals();
        let mut rig = Rig::pcm(2, 16);
        let ramp: Vec<u8> = (0..96 * 4 * 3).map(|i| (i % 251) as u8 | 1).collect();
        rig.shared.write_bytes(&ramp).unwrap();
        let queued = rig.shared.stats.buffered_samples.load(Ordering::Relaxed);

        rig.shared.set_paused(true);
        let held = rig.prepare();
        assert!(held.held && !held.underrun);
        assert!(held.transfer_buffer.iter().all(|byte| *byte == 0));
        assert_eq!(held.music_frames, 0);
        assert_eq!(
            rig.shared.stats.buffered_samples.load(Ordering::Relaxed),
            queued,
            "pause must not consume the ring"
        );

        rig.shared.set_paused(false);
        let first = rig.prepare();
        assert_eq!(&first.transfer_buffer[..], &ramp[..96 * 4], "resume starts at the pause point");
        let second = rig.prepare();
        assert_eq!(&second.transfer_buffer[..], &ramp[96 * 4..96 * 4 * 2]);
    }

    #[test]
    fn played_frames_count_music_only_and_a_flush_resets_them_at_once() {
        let _globals = lock_globals();
        let mut rig = Rig::pcm(2, 16);
        let music = noise(96 * 4 * 6, 11);
        rig.shared.write_bytes(&music).unwrap();

        assert_eq!(rig.shared.stats.played_frames(), 0);
        rig.submit();
        rig.submit();
        assert_eq!(rig.shared.stats.played_frames(), 0, "submitted is not played");
        assert_eq!(
            rig.shared.stats.output_latency_frames(),
            (96 * 4 + 96 * 2) as u64,
            "latency is the ring (4 transfers left) plus two on the bus"
        );
        rig.complete_oldest(0);
        assert_eq!(rig.shared.stats.played_frames(), 96);
        rig.complete_oldest(0);
        assert_eq!(rig.shared.stats.played_frames(), 192);

        // Held silence is not music.
        rig.shared.set_paused(true);
        rig.submit();
        rig.complete_oldest(0);
        assert_eq!(rig.shared.stats.played_frames(), 192);
        rig.shared.set_paused(false);

        // A transfer the DAC only partly took: loss is charged to the padding
        // first, then to the music. Here the transfer is all music.
        rig.submit();
        rig.complete_oldest(10);
        assert_eq!(rig.shared.stats.played_frames(), 192 + 86);
        // consumer_frames counts every frame the DAC took, held silence included.
        assert_eq!(
            rig.shared.stats.consumer_frames.load(Ordering::Relaxed),
            96 * 3 + 86
        );

        // flush(): the count is 0 immediately, and a transfer filled before the
        // flush that completes afterwards does not come back into the count.
        rig.submit();
        rig.shared.flush();
        assert_eq!(rig.shared.stats.played_frames(), 0);
        assert_eq!(rig.shared.stats.inflight_music_frames.load(Ordering::Relaxed), 0);
        rig.complete_oldest(0);
        assert_eq!(rig.shared.stats.played_frames(), 0, "old-epoch audio is not played audio");

        // Audio after the flush is counted again, from zero. The refill hold
        // waits for the start threshold first.
        let refill = noise(rig.shared.stats.start_threshold_samples * 2, 13);
        let before = rig.shared.stats.buffered_samples.load(Ordering::Relaxed);
        assert_eq!(before, 0);
        let held = rig.prepare();
        assert!(held.held, "after a flush the stream holds silence until the threshold");
        rig.shared.write_bytes(&refill).unwrap();
        rig.submit();
        rig.complete_oldest(0);
        assert_eq!(rig.shared.stats.played_frames(), 96);
    }

    #[test]
    fn a_short_tail_pads_and_only_its_music_counts() {
        let _globals = lock_globals();
        let mut rig = Rig::pcm(2, 16);
        let tail = noise(10 * 4, 5); // 10 frames
        rig.shared.write_bytes(&tail).unwrap();
        let payload = rig.prepare();
        assert!(payload.underrun, "without drain(), padding is an underrun");
        assert_eq!(payload.music_frames, 10);
        rig.in_flight.push(payload);
        rig.complete_oldest(0);
        assert_eq!(rig.shared.stats.played_frames(), 10);
    }

    #[test]
    fn drain_plays_a_short_tail_without_an_underrun_and_then_reports_drained() {
        let _globals = lock_globals();
        let mut rig = Rig::pcm(2, 16);
        let _session = TestSession::install_streaming(Arc::clone(&rig.shared.stats));
        let tail = noise(10 * 4, 9);
        rig.shared.write_bytes(&tail).unwrap();
        rig.shared.stats.started.store(true, Ordering::Release);

        assert!(!rig.shared.is_drained());
        rig.shared.drain();
        assert_eq!(rig.shared.current_state(), StreamState::Draining);

        let payload = rig.prepare();
        assert!(!payload.underrun, "the padded tail after drain() is not an underrun");
        assert!(!payload.held);
        assert_eq!(payload.music_frames, 10);
        assert_eq!(&payload.transfer_buffer[..tail.len()], &tail[..]);
        rig.in_flight.push(payload);
        rig.publish_inflight();
        assert!(!rig.shared.is_drained(), "the tail is still on the bus");
        assert_eq!(rig.shared.current_state(), StreamState::Draining);

        rig.complete_oldest(0);
        assert!(rig.shared.is_drained());
        assert_eq!(rig.shared.current_state(), StreamState::Drained);

        // Past the end only held silence goes out; the audit latch is untouched.
        let after = rig.prepare();
        assert!(after.held && !after.underrun);
    }

    #[test]
    fn software_gain_scales_pcm_and_ends_bit_perfect_until_it_is_back_to_one() {
        let _globals = lock_globals();
        let mut rig = Rig::pcm(2, 16);
        let _session = TestSession::install_streaming(Arc::clone(&rig.shared.stats));
        USB_PCM_PAYLOAD_LATCH.lock().observe(true, Instant::now());
        assert!(rig.shared.note_bit_perfect(), "unity gain on a verified stream is bit-perfect");

        // 16-bit samples 0x4000 and -0x4000, repeated.
        let mut input = Vec::new();
        for _ in 0..96 * 2 {
            input.extend_from_slice(&0x4000i16.to_le_bytes());
            input.extend_from_slice(&(-0x4000i16).to_le_bytes());
        }
        rig.shared.write_bytes(&input).unwrap();
        rig.shared.set_software_gain(0.5).unwrap();
        assert!(!rig.shared.note_bit_perfect(), "any gain other than 1.0 ends bit-perfect");
        assert_eq!(rig.shared.stats.software_gain(), 0.5);

        let payload = rig.prepare();
        let first = i16::from_le_bytes([payload.transfer_buffer[0], payload.transfer_buffer[1]]);
        let second = i16::from_le_bytes([payload.transfer_buffer[2], payload.transfer_buffer[3]]);
        assert_eq!((first, second), (0x2000, -0x2000));

        rig.shared.set_software_gain(1.0).unwrap();
        USB_PCM_PAYLOAD_LATCH.lock().observe(true, Instant::now());
        assert!(rig.shared.note_bit_perfect());
        let unity = rig.prepare();
        let first = i16::from_le_bytes([unity.transfer_buffer[0], unity.transfer_buffer[1]]);
        assert_eq!(first, 0x4000, "back at 1.0 the samples are untouched");

        let events = rig.events.wait_for(3);
        assert_eq!(
            events
                .iter()
                .filter_map(|event| match event {
                    DacEvent::BitPerfectChanged(on) => Some(*on),
                    _ => None,
                })
                .collect::<Vec<_>>(),
            vec![true, false, true]
        );
        rig.hub.shutdown();
    }

    #[test]
    fn software_gain_saturates_and_never_dithers() {
        let mut samples = [i32::MAX, i32::MIN, 1 << 16, -(1 << 16), 3];
        apply_software_gain(&mut samples, 2.0);
        assert_eq!(samples, [i32::MAX, i32::MIN, 2 << 16, -(2 << 16), 6]);
        let mut untouched = [123, -456, i32::MAX];
        apply_software_gain(&mut untouched, 1.0);
        assert_eq!(untouched, [123, -456, i32::MAX]);
        let mut halved = [3, -3, 4];
        apply_software_gain(&mut halved, 0.5);
        assert_eq!(halved, [2, -2, 2], "round to nearest, ties away from zero");
    }

    #[test]
    fn software_gain_is_refused_for_dsd_and_for_nonsense_values() {
        let _globals = lock_globals();
        let dop = Rig::new(3, 24, 2, DsdTransportMode::DoP);
        assert!(matches!(
            dop.shared.set_software_gain(0.5),
            Err(crate::Error::InvalidState(_))
        ));
        assert!(dop.shared.set_software_gain(1.0).is_ok(), "unity is always fine");
        let native = Rig::new(4, 1, 2, DsdTransportMode::Native);
        assert!(native.shared.set_software_gain(0.9).is_err());

        let pcm = Rig::pcm(2, 16);
        for bad in [f32::NAN, f32::INFINITY, -0.5, 17.0] {
            assert!(
                matches!(pcm.shared.set_software_gain(bad), Err(crate::Error::InvalidArgument(_))),
                "{bad}"
            );
        }
    }

    #[test]
    fn state_changes_are_published_once_each() {
        let _globals = lock_globals();
        let rig = Rig::pcm(2, 16);
        let stats = &rig.shared.stats;
        rig.shared.refresh_state();
        rig.shared.refresh_state();
        assert_eq!(rig.shared.current_state(), StreamState::Buffering);

        stats.started.store(true, Ordering::Release);
        rig.shared.refresh_state();
        assert_eq!(rig.shared.current_state(), StreamState::Playing);

        rig.shared.set_paused(true);
        rig.shared.set_paused(true);
        rig.shared.set_paused(false);
        rig.shared.mark_closed();
        rig.shared.fail(crate::Error::DeviceLost);

        let events = rig.events.wait_for(6);
        let states: Vec<_> = events
            .iter()
            .filter_map(|event| match event {
                DacEvent::StateChanged(state) => Some(*state),
                _ => None,
            })
            .collect();
        assert_eq!(
            states,
            vec![
                StreamState::Buffering,
                StreamState::Playing,
                StreamState::Paused,
                StreamState::Playing,
                StreamState::Closed,
                StreamState::Failed,
            ]
        );
        assert!(events.contains(&DacEvent::Error(crate::Error::DeviceLost)));
        rig.hub.shutdown();
    }

    #[test]
    fn a_run_of_underruns_is_one_event() {
        let _globals = lock_globals();
        let rig = Rig::pcm(2, 16);
        let mut was_underrun = false;
        for underrun in [true, true, true, false, true, true] {
            note_underrun_edge(&rig.shared, &mut was_underrun, underrun);
        }
        let events = rig.events.wait_for(2);
        std::thread::sleep(Duration::from_millis(30));
        let underruns = rig
            .events
            .0
            .lock()
            .iter()
            .filter(|event| **event == DacEvent::Underrun)
            .count();
        assert_eq!(underruns, 2, "{events:?}");
        rig.hub.shutdown();
    }
}
