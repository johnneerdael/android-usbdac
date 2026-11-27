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
            &NATIVE_IN,
            &mut out,
            4,
            1,
            DsdTransportMode::Native,
            2,
            true,
            false,
        )
        .unwrap();
        // BE: each channel word is b0(MSB)..b3(LSB), channel 0 then channel 1.
        assert_eq!(out, [10, 11, 12, 13, 20, 21, 22, 23]);
    }

    #[test]
    fn native_dsd_u32_little_endian_reverses_byte_order() {
        let mut out = [0u8; 8];
        encode_usb_pcm_slots(
            &NATIVE_IN,
            &mut out,
            4,
            1,
            DsdTransportMode::Native,
            2,
            false,
            false,
        )
        .unwrap();
        assert_eq!(out, [13, 12, 11, 10, 23, 22, 21, 20]);
    }

    #[test]
    fn standard_adc3_dsd_le64_reverses_each_channel_word_without_reversing_bits() {
        // Chronological MSB-first bytes, alternating left and right channels.
        let input = [
            0x01, 0x81, 0x02, 0x82, 0x03, 0x83, 0x04, 0x84, 0x05, 0x85, 0x06, 0x86, 0x07, 0x87,
            0x08, 0x88,
        ];
        let mut out = [0; 16];
        encode_usb_pcm_slots(
            &input,
            &mut out,
            8,
            64,
            DsdTransportMode::Native,
            2,
            false,
            false,
        )
        .unwrap();
        assert_eq!(
            out,
            [
                0x08, 0x07, 0x06, 0x05, 0x04, 0x03, 0x02, 0x01, 0x88, 0x87, 0x86, 0x85, 0x84, 0x83,
                0x82, 0x81
            ]
        );
        let mut partial = [0xaa; 14];
        assert!(encode_usb_pcm_slots(
            &input[..14],
            &mut partial,
            8,
            64,
            DsdTransportMode::Native,
            2,
            false,
            false,
        )
        .is_err());
        assert_eq!(partial, [0xaa; 14]);
    }

    #[test]
    fn native_dsd_bit_reverse_flips_each_byte() {
        let mut out = [0u8; 8];
        encode_usb_pcm_slots(
            &NATIVE_IN,
            &mut out,
            4,
            1,
            DsdTransportMode::Native,
            2,
            true,
            true,
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
            &input,
            &mut out,
            1,
            1,
            DsdTransportMode::Native,
            2,
            false,
            false,
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
    fn endpoint_usage_bits_map_to_the_public_enum() {
        use crate::capabilities::EndpointUsage;
        assert_eq!(endpoint_usage(UsageType::Data), EndpointUsage::Data);
        assert_eq!(endpoint_usage(UsageType::Feedback), EndpointUsage::Feedback);
        assert_eq!(
            endpoint_usage(UsageType::FeedbackData),
            EndpointUsage::FeedbackData
        );
        assert_eq!(endpoint_usage(UsageType::Reserved), EndpointUsage::Reserved);
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
        assert_eq!(
            ring.push_samples(&[0xfa112200u32 as i32, 0xfa334400u32 as i32]),
            2
        );
        let mut first = [0i32; 6];
        assert!(ring.pop_into_or_pad(&mut first, 2, DsdTransportMode::DoP));
        assert_eq!(
            first.map(|s| s as u32),
            [0x05112200, 0x05334400, 0xfa696900, 0xfa696900, 0x05696900, 0x05696900,]
        );
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
        assert!(!audit_dsd_payload(
            &[0, 0, 0, 0x05],
            DsdTransportMode::DoP,
            4,
            2
        ));
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
        assert!(audit_dsd_payload(
            &[0x00, 0x55, 0x00],
            DsdTransportMode::Native,
            1,
            2
        ));
        assert!(!audit_dsd_payload(
            &[0, 0, 0, 0],
            DsdTransportMode::Native,
            1,
            2
        ));
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
            pcm_encoding: crate::transport::PcmEncoding::Signed,
            sample_rate: 88_200,
            bit_depth: 1,
            channels: 2,
            is_dop: false,
            dsd_transport: DsdTransportMode::Native,
            dsd_bit_rate: 2_822_400,
        };
        assert_eq!(effective_ring_rate(&native), 352_800);
        let pcm = PlaybackFormat {
            pcm_encoding: crate::transport::PcmEncoding::Signed,
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
        let (frames, rate) =
            decode_feedback_report(&exact.to_le_bytes(), 125, Speed::High).unwrap();
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
            max_consecutive_lossy_urbs: IsoQueueGeometry::default_for(125)
                .max_consecutive_lossy_urbs(),
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
    fn explicit_feedback_live_lock_gates_bit_perfect() {
        let _latch = PAYLOAD_LATCH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let stats = new_stream_stats();
        let state = streaming_state(Arc::clone(&stats));
        stats.feedback_required.store(true, Ordering::Release);
        assert_eq!(
            bit_perfect_blocker(&state),
            Some(BitPerfectBlocker::FeedbackUnlocked)
        );
        stats.feedback_locked.store(true, Ordering::Release);
        assert_eq!(bit_perfect_blocker(&state), None);
        stats.feedback_locked.store(false, Ordering::Release);
        assert_eq!(
            bit_perfect_blocker(&state),
            Some(BitPerfectBlocker::FeedbackUnlocked)
        );
        stats.feedback_required.store(false, Ordering::Release);
        assert_eq!(bit_perfect_blocker(&state), None);
    }

    #[test]
    fn lost_frames_clear_the_bit_perfect_claim_until_a_new_stream() {
        let _latch = PAYLOAD_LATCH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
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
        let _latch = PAYLOAD_LATCH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let stats = new_stream_stats();
        let state = streaming_state(Arc::clone(&stats));
        publish_transport_loss(&stats, &lossy_tracker(0));
        assert!(!stats.transport_loss_latched());
        assert_eq!(stats.lost_frames.load(Ordering::Relaxed), 0);
        assert!(direct_path_is_bit_perfect(&state));
    }

    #[test]
    fn publish_mirrors_every_counter() {
        let _latch = PAYLOAD_LATCH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let stats = new_stream_stats();
        let mut tracker = LossTracker::new(LossLimits {
            max_consecutive_lossy_urbs: IsoQueueGeometry::default_for(125)
                .max_consecutive_lossy_urbs(),
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
                transport_failed: false,
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
                transport_failed: false,
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
        // Three of the default 2 ms transfers, at every service interval.
        for interval in [125, 250, 500, 1_000] {
            assert_eq!(
                IsoQueueGeometry::default_for(interval).max_consecutive_lossy_urbs(),
                3,
                "{interval} us"
            );
        }
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
            configuration_number: 1,
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
            uac2_formats: Some(1),
            native_dsd: None,
            channels: 2,
            subslot_size: 2,
            bit_resolution: 16,
            sample_rates: vec![48_000],
            refresh: 0,
            synch_address: 0,
            feedback_endpoint: None,
            implicit_feedback: None,
            terminal_link: None,
            is_uac1: false,
        }
    }

    #[test]
    fn feedback_timebase_hint_is_scoped_to_the_session_and_usb_route() {
        let mut candidate = pcm_candidate();
        let feedback = FeedbackEndpoint {
            refresh: 0,
            address: 0x81,
            transfer_type: TransferType::Isochronous,
            interval: 4,
            service_interval_us: 1_000,
            max_packet_bytes: 4,
        };
        let route = FeedbackTimebaseRoute::new(&candidate, feedback);
        let session = test_state(None);
        session
            .service_interval_feedback_routes
            .lock()
            .insert(route);
        assert!(session
            .clone()
            .service_interval_feedback_routes
            .lock()
            .contains(&route));
        assert!(!test_state(None)
            .service_interval_feedback_routes
            .lock()
            .contains(&route));
        candidate.alt_setting += 1;
        let other_alt = FeedbackTimebaseRoute::new(&candidate, feedback);
        assert!(!session
            .service_interval_feedback_routes
            .lock()
            .contains(&other_alt));
    }

    #[test]
    fn native_dsd_cannot_negotiate_or_open_a_pcm_alternate() {
        let mut candidate = pcm_candidate();
        candidate.subslot_size = 4;
        candidate.bit_resolution = 32;
        let requested = PlaybackFormat {
            pcm_encoding: crate::transport::PcmEncoding::Signed,
            sample_rate: 88_200,
            bit_depth: 1,
            channels: 2,
            is_dop: false,
            dsd_transport: DsdTransportMode::Native,
            dsd_bit_rate: 2_822_400,
        };
        assert_eq!(
            transport_compatibility_penalty_for_candidate(&candidate, requested),
            u32::MAX
        );
        assert!(validate_transport_against_playback_format(&candidate, requested).is_err());
        candidate.format_tag = FORMAT_TAG_DSD;
        candidate.bit_resolution = 1;
        // A format tag alone cannot authorize native wire packing.
        assert_eq!(
            transport_compatibility_penalty_for_candidate(&candidate, requested),
            u32::MAX
        );
        assert!(validate_transport_against_playback_format(&candidate, requested).is_err());
        candidate.native_dsd = Some(native_dsd::NativeDsdProfile {
            word_bytes: 4,
            big_endian: false,
            bit_reverse: false,
            origin: native_dsd::NativeDsdOrigin::HostPolicy,
        });
        assert_eq!(
            transport_compatibility_penalty_for_candidate(&candidate, requested),
            0
        );
        assert!(validate_transport_against_playback_format(&candidate, requested).is_ok());
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
        let _latch = PAYLOAD_LATCH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let stats = deep_ring_stats();
        let threshold = stats.start_threshold_samples;
        let ring = Arc::new(Mutex::new(PcmRing::new(stats.buffer_capacity_samples, 2)));
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
        assert_eq!(
            ring.lock().push_samples(&music[..threshold - 2]),
            threshold - 2
        );
        let waiting = prepare(&ring, &stats, false);
        assert!(waiting.held && !waiting.underrun);
        assert_eq!(
            ring.lock().len_samples(),
            threshold - 2,
            "a hold consumes nothing"
        );

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
        assert_eq!(
            shallow.start_threshold_samples,
            shallow.buffer_target_samples
        );
    }

    #[test]
    fn software_gain_switches_the_recommended_depth_to_the_processing_target() {
        let stats = deep_ring_stats();
        assert_eq!(stats.active_target_samples(), stats.buffer_target_samples);
        stats.gain_bits.store(0.5f32.to_bits(), Ordering::Relaxed);
        assert_eq!(
            stats.active_target_samples(),
            stats.processing_target_samples
        );
        stats.gain_bits.store(1.0f32.to_bits(), Ordering::Relaxed);
        assert_eq!(stats.active_target_samples(), stats.buffer_target_samples);
    }

    #[test]
    fn the_host_processing_hint_switches_the_recommended_depth_like_software_gain() {
        let stats = deep_ring_stats();
        assert_eq!(stats.active_target_samples(), stats.buffer_target_samples);
        assert_eq!(stats.active_target_ms(), 250);
        stats.host_processing.store(true, Ordering::Relaxed);
        assert_eq!(
            stats.active_target_samples(),
            stats.processing_target_samples
        );
        assert_eq!(stats.active_target_ms(), 100);
        stats.host_processing.store(false, Ordering::Relaxed);
        assert_eq!(stats.active_target_samples(), stats.buffer_target_samples);
        // A bitstream is never live.
        stats.raw_bitstream.store(true, Ordering::Relaxed);
        stats.host_processing.store(true, Ordering::Relaxed);
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
        let _latch = PAYLOAD_LATCH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let stats = deep_ring_stats();
        let threshold = stats.start_threshold_samples;
        let ring = Arc::new(Mutex::new(PcmRing::new(stats.buffer_capacity_samples, 2)));
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
        let mut slots: Vec<IsoTransferSlot> = (0..3)
            .map(|i| IsoTransferSlot::new(i, 16, 1024).unwrap())
            .collect();
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
                pcm_encoding: crate::transport::PcmEncoding::Signed,
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

            prepare_iso_transfer_payload(
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
            .unwrap()
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
            self.account(&payload, lost);
            self.publish_inflight();
        }

        /// What the loop does for a transfer that really played.
        fn account(&mut self, payload: &IsoTransferPayload, lost: u64) {
            let stats = &self.shared.stats;
            stats
                .consumer_frames
                .fetch_add(payload.frame_count as u64 - lost, Ordering::Relaxed);
            stats.completed_transfers.fetch_add(1, Ordering::Relaxed);
            account_completed_transfer(
                stats,
                &mut self.played,
                payload.epoch,
                payload.frame_count as u64,
                payload.music_frames as u64,
                lost,
            );
        }

        /// What the output loop does when it sees the flush epoch move: cancels
        /// the transfers filled before the flush (`flush_cancel_targets`), and
        /// then takes each one back as libusb returns it. `status_of` is what
        /// libusb reports for the transfer at that index of the bus. Returns
        /// (cancelled, raced): the transfers dropped as `CancelledByFlush` and
        /// the ones that had already finished and were accounted as played.
        fn cancel_for_flush(&mut self, status_of: impl Fn(usize) -> i32) -> (usize, usize) {
            let epoch = self.shared.stats.flush_epoch.load(Ordering::Acquire);
            let targets = flush_cancel_targets(
                epoch,
                self.in_flight
                    .iter()
                    .map(|payload| (true, payload.epoch, payload.music_frames)),
            );
            let (mut cancelled, mut raced) = (0, 0);
            for (offset, &index) in targets.iter().enumerate() {
                let payload = self.in_flight.remove(index - offset);
                match classify_completion(status_of(index), true) {
                    Completion::CancelledByFlush => {
                        cancelled += 1;
                        self.shared
                            .stats
                            .flush_cancelled_transfers
                            .fetch_add(1, Ordering::Relaxed);
                    }
                    Completion::Played => {
                        raced += 1;
                        self.account(&payload, 0);
                    }
                    Completion::UnexpectedCancel => unreachable!("the flush asked for it"),
                }
            }
            self.publish_inflight();
            (cancelled, raced)
        }
    }

    fn lock_globals() -> std::sync::MutexGuard<'static, ()> {
        let guard = PAYLOAD_LATCH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
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
            let mut input = noise(
                frames * frame_bytes,
                7 + u32::from(subslot) * u32::from(bits),
            );
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
        rig.shared
            .ring
            .lock()
            .pop_into_or_pad(&mut out, 2, DsdTransportMode::Native);
        assert_eq!(out, [10, 20, 11, 21, 12, 22, 13, 23]);
        // 0xFF must not turn negative.
        assert_eq!(rig.shared.write_bytes(&[0xFF; 8]).unwrap(), 8);
        rig.shared
            .ring
            .lock()
            .pop_into_or_pad(&mut out, 2, DsdTransportMode::Native);
        assert_eq!(out, [255; 8]);
    }

    #[test]
    fn native_dsd_u64_admission_counts_bytes_and_rejects_partial_words() {
        let rig = Rig::new(8, 64, 2, DsdTransportMode::Native);
        assert_eq!(rig.shared.wire_frame_bytes(), 16);
        assert_eq!(rig.shared.write_bytes(&[0xff; 15]).unwrap(), 0);
        assert_eq!(rig.shared.write_samples(&[0x81; 15]).unwrap(), 0);
        assert_eq!(rig.shared.write_bytes(&[0xff; 31]).unwrap(), 16);
        assert_eq!(rig.shared.write_samples(&[0x81; 17]).unwrap(), 16);
        assert_eq!(
            rig.shared.stats.buffered_samples.load(Ordering::Relaxed),
            32
        );
        assert_eq!(rig.shared.stats.producer_frames.load(Ordering::Relaxed), 16);
        let mut out = [0; 32];
        rig.shared
            .ring
            .lock()
            .pop_into_or_pad(&mut out, 2, DsdTransportMode::Native);
        assert_eq!(&out[..16], &[255; 16]);
        assert_eq!(&out[16..], &[0x81; 16]);

        // Free capacity smaller than a wire frame must not admit half a word.
        let mut ring = PcmRing::new(24, 2);
        assert_eq!(ring.push_bytes(&[1; 32], 8, true), 16);
        assert_eq!(ring.push_bytes(&[2; 16], 8, true), 0);
        ring.clear();
        assert_eq!(ring.len_samples(), 0);
        assert_eq!(ring.push_bytes(&[3; 16], 8, true), 16);
    }

    #[test]
    fn native_dsd_u64_drain_padding_and_flush_keep_byte_frame_counters() {
        let mut rig = Rig::new(8, 64, 2, DsdTransportMode::Native);
        assert_eq!(rig.shared.write_bytes(&[0x81; 16]).unwrap(), 16);
        rig.shared.drain();
        let payload = rig.prepare();
        assert_eq!(payload.music_frames, 8); // byte frames, not 64-bit wire words
        assert_eq!(payload.frame_count, payload.transfer_buffer.len() / 2);
        assert_eq!(&payload.transfer_buffer[..16], &[0x81; 16]);
        assert!(payload.transfer_buffer[16..]
            .iter()
            .all(|byte| *byte == 0x69));
        assert_eq!(
            rig.shared
                .stats
                .inflight_music_frames
                .load(Ordering::Relaxed),
            8
        );
        rig.shared.flush();
        assert_eq!(
            rig.shared
                .stats
                .inflight_music_frames
                .load(Ordering::Relaxed),
            0
        );
        assert_eq!(rig.shared.stats.buffered_samples.load(Ordering::Relaxed), 0);
        assert_eq!(rig.shared.write_bytes(&[0x02; 16]).unwrap(), 16);
        rig.shared.drain();
        let fresh = rig.prepare();
        assert_eq!(fresh.epoch, payload.epoch + 1);
        assert_eq!(&fresh.transfer_buffer[..16], &[0x02; 16]);
        assert!(fresh.transfer_buffer[16..].iter().all(|byte| *byte == 0x69));
    }

    #[test]
    fn dop_words_keep_their_data_bits_and_the_library_owns_the_marker() {
        let mut rig = Rig::new(3, 24, 2, DsdTransportMode::DoP);
        // Two stereo frames of [dsd_low, dsd_high, marker]; the markers are junk.
        let bytes = [
            0x22u8, 0x11, 0xEE, 0x44, 0x33, 0xEE, 0x66, 0x55, 0xEE, 0x88, 0x77, 0xEE,
        ];
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
    fn a_gain_change_moves_the_target_without_flushing_the_ring() {
        let _globals = lock_globals();
        let mut rig = Rig::pcm(2, 16);
        let _session = TestSession::install_streaming(Arc::clone(&rig.shared.stats));
        let stats = Arc::clone(&rig.shared.stats);
        let deep = stats.buffer_target_samples;
        let live = stats.processing_target_samples;

        // Fill to the deep target at unity gain: nothing more is wanted.
        let chunk = vec![5u8; rig.shared.writable_bytes()];
        rig.shared.write_bytes(&chunk).unwrap();
        assert_eq!(stats.buffered_samples.load(Ordering::Relaxed), deep);
        assert_eq!(rig.shared.writable_bytes(), 0);
        let epoch = stats.flush_epoch.load(Ordering::Relaxed);

        // Gain on: the shallow target applies. The ring is above it, so nothing
        // is wanted, and nothing is dropped: it simply drains.
        rig.shared.set_software_gain(0.5).unwrap();
        assert_eq!(rig.shared.writable_bytes(), 0);
        assert_eq!(stats.buffered_samples.load(Ordering::Relaxed), deep);
        assert_eq!(stats.flush_epoch.load(Ordering::Relaxed), epoch, "no flush");

        // The output drains the ring (prepare pops it) down to the shallow
        // target; only below it does the host get room again.
        while stats.buffered_samples.load(Ordering::Relaxed) > live {
            assert_eq!(rig.shared.writable_bytes(), 0);
            rig.prepare();
        }
        assert_eq!(rig.shared.writable_bytes(), 0);
        rig.prepare();
        let queued = stats.buffered_samples.load(Ordering::Relaxed);
        assert!(queued < live);
        assert_eq!(rig.shared.writable_bytes(), (live - queued) * 2 / 4 * 4);

        // Gain off again: the deep target applies and the room grows to it.
        rig.shared.set_software_gain(1.0).unwrap();
        assert_eq!(rig.shared.writable_bytes(), (deep - queued) * 2 / 4 * 4);
        assert_eq!(stats.flush_epoch.load(Ordering::Relaxed), epoch);
    }

    #[test]
    fn the_queued_frames_are_the_ring_fill_in_counter_units() {
        let _globals = lock_globals();
        let rig = Rig::pcm(2, 16);
        assert_eq!(rig.shared.stats.queued_frames(), 0);
        let taken = rig.shared.write_bytes(&vec![9u8; 4_000]).unwrap();
        assert_eq!(rig.shared.stats.queued_frames(), (taken / 4) as u64);
        // Native DSD counts DSD bytes per channel.
        let dsd = Rig::new(4, 1, 2, DsdTransportMode::Native);
        dsd.shared.write_bytes(&[0x69u8; 64]).unwrap();
        assert_eq!(
            dsd.shared.stats.queued_frames(),
            32,
            "one DSD byte per channel is one frame"
        );
    }

    #[test]
    fn host_processing_moves_the_target_without_flushing_the_ring() {
        let _globals = lock_globals();
        let rig = Rig::pcm(2, 16);
        let stats = Arc::clone(&rig.shared.stats);
        let deep = stats.buffer_target_samples;
        let live = stats.processing_target_samples;
        let chunk = vec![5u8; rig.shared.writable_bytes()];
        rig.shared.write_bytes(&chunk).unwrap();
        assert_eq!(stats.buffered_samples.load(Ordering::Relaxed), deep);
        assert_eq!(rig.shared.writable_bytes(), 0);
        let epoch = stats.flush_epoch.load(Ordering::Relaxed);

        // The host starts processing: the shallow target applies, the ring is
        // above it, so nothing is wanted and nothing is dropped.
        stats.host_processing.store(true, Ordering::Relaxed);
        assert_eq!(rig.shared.writable_bytes(), 0);
        assert_eq!(stats.buffered_samples.load(Ordering::Relaxed), deep);
        assert_eq!(stats.flush_epoch.load(Ordering::Relaxed), epoch, "no flush");
        assert!(live < deep);

        // The host stops processing: the deep target's room reappears at once.
        stats.host_processing.store(false, Ordering::Relaxed);
        stats.buffered_samples.store(live, Ordering::Relaxed);
        assert_eq!(rig.shared.writable_bytes(), (deep - live) * 2 / 4 * 4);
    }

    #[test]
    fn the_stream_verdicts_follow_the_session_and_the_payload_audit() {
        let _globals = lock_globals();
        // No session: nothing is true.
        assert!(!stream_verdicts().stable && !stream_verdicts().clock_verified);

        let rig = Rig::pcm(2, 16);
        let _session = TestSession::install_streaming(Arc::clone(&rig.shared.stats));
        {
            // The test session has a verified clock and no stability yet.
            let verdicts = stream_verdicts();
            assert!(verdicts.clock_verified);
            assert!(!verdicts.stable);
            assert!(!verdicts.payload_verified, "no audio has been audited");
        }
        set_usb_stream_stable(true);
        assert!(stream_verdicts().stable);
        set_clock_verification(true, true, false, None);
        assert!(!stream_verdicts().clock_verified);
        assert_eq!(
            current_bit_perfect_blocker(),
            Some(crate::stream::BitPerfectBlocker::ClockUnverified)
        );

        // Real audio through the PCM audit verifies the payload.
        let music: Vec<i32> = (1..=2_000).map(|i| i << 16).collect();
        let mut samples = vec![0i32; 2_000];
        let mut ring = PcmRing::new(8_000, 2);
        assert_eq!(ring.push_samples(&music), 2_000);
        let pop = ring.pop_music_into_or_pad(&mut samples, 2, DsdTransportMode::None);
        assert_eq!(pop.music_samples, 2_000);
        let mut wire = vec![0u8; samples.len() * 2];
        encode_usb_pcm_slots(
            &samples,
            &mut wire,
            2,
            16,
            DsdTransportMode::None,
            2,
            false,
            false,
        )
        .unwrap();
        observe_pcm_payload_audit(audit_pcm_payload(&wire, 2, 2));
        assert!(stream_verdicts().payload_verified);
        assert_eq!(stream_verdicts().payload_constant_for_ms, None);
    }

    #[test]
    fn a_dsd_bitstream_always_keeps_the_deep_target() {
        let dop = Rig::new(3, 24, 2, DsdTransportMode::DoP);
        let native = Rig::new(4, 1, 2, DsdTransportMode::Native);
        for rig in [&dop, &native] {
            let stats = &rig.shared.stats;
            // Even a gain value that could only come from a bug is not "live".
            stats.gain_bits.store(0.5f32.to_bits(), Ordering::Relaxed);
            assert_eq!(stats.active_target_samples(), stats.buffer_target_samples);
            stats.host_processing.store(true, Ordering::Relaxed);
            assert_eq!(stats.active_target_samples(), stats.buffer_target_samples);
        }
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
        assert_eq!(
            rig.shared.writable_bytes(),
            (target_bytes - taken) / frame * frame
        );

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
        assert_eq!(
            rig.shared.writable_bytes(),
            (target_bytes - taken) / frame * frame
        );

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
        assert_eq!(
            &first.transfer_buffer[..],
            &ramp[..96 * 4],
            "resume starts at the pause point"
        );
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
        assert_eq!(
            rig.shared.stats.played_frames(),
            0,
            "submitted is not played"
        );
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
        assert_eq!(
            rig.shared
                .stats
                .inflight_music_frames
                .load(Ordering::Relaxed),
            0
        );
        rig.complete_oldest(0);
        assert_eq!(
            rig.shared.stats.played_frames(),
            0,
            "old-epoch audio is not played audio"
        );

        // Audio after the flush is counted again, from zero. The refill hold
        // waits for the start threshold first.
        let refill = noise(rig.shared.stats.start_threshold_samples * 2, 13);
        let before = rig.shared.stats.buffered_samples.load(Ordering::Relaxed);
        assert_eq!(before, 0);
        let held = rig.prepare();
        assert!(
            held.held,
            "after a flush the stream holds silence until the threshold"
        );
        rig.shared.write_bytes(&refill).unwrap();
        rig.submit();
        rig.complete_oldest(0);
        assert_eq!(rig.shared.stats.played_frames(), 96);
    }

    use libusb1_sys::constants::{LIBUSB_TRANSFER_CANCELLED, LIBUSB_TRANSFER_COMPLETED};

    fn loss_and_underruns(stats: &RuntimeStats) -> (u64, u64, u64, bool) {
        (
            stats.lost_frames.load(Ordering::Relaxed),
            stats.underrun_count.load(Ordering::Relaxed),
            stats.data_urb_timeouts.load(Ordering::Relaxed),
            stats.transport_loss_latched(),
        )
    }

    #[test]
    fn a_flush_cancels_the_music_on_the_bus_and_none_of_it_is_counted() {
        let _globals = lock_globals();
        let mut rig = Rig::pcm(2, 16);
        let _session = TestSession::install_streaming(Arc::clone(&rig.shared.stats));
        USB_PCM_PAYLOAD_LATCH.lock().observe(true, Instant::now());
        assert!(rig.shared.note_bit_perfect());
        rig.shared.write_bytes(&noise(96 * 4 * 12, 21)).unwrap();

        // Eight transfers of music on the bus; two have played.
        for _ in 0..8 {
            rig.submit();
        }
        rig.complete_oldest(0);
        rig.complete_oldest(0);
        let stats = Arc::clone(&rig.shared.stats);
        assert_eq!(stats.played_frames(), 192);
        let consumed = stats.consumer_frames.load(Ordering::Relaxed);
        let completed = stats.completed_transfers.load(Ordering::Relaxed);
        assert_eq!(rig.in_flight.len(), 6);

        // The seek: the ring is dropped at once, then the output thread cancels
        // what is on the bus and libusb hands every transfer back as cancelled.
        rig.shared.flush();
        assert_eq!(stats.played_frames(), 0);
        let (cancelled, raced) = rig.cancel_for_flush(|_| LIBUSB_TRANSFER_CANCELLED);
        assert_eq!((cancelled, raced), (6, 0));
        assert!(
            rig.in_flight.is_empty(),
            "nothing of the old position stays on the bus"
        );

        // Nothing was lost, nothing underran, nothing counted as taken or played.
        assert_eq!(loss_and_underruns(&stats), (0, 0, 0, false));
        assert_eq!(stats.consumer_frames.load(Ordering::Relaxed), consumed);
        assert_eq!(stats.completed_transfers.load(Ordering::Relaxed), completed);
        assert_eq!(stats.played_frames(), 0);
        assert_eq!(stats.inflight_music_frames.load(Ordering::Relaxed), 0);
        assert_eq!(stats.flush_cancelled_transfers.load(Ordering::Relaxed), 6);
        assert_eq!(
            stats.output_latency_frames(),
            0,
            "the ring is empty and so is the bus"
        );

        // The slots are refilled with wire silence while the ring refills: held,
        // not an underrun, and the audit and the verdict are untouched.
        for _ in 0..8 {
            let payload = rig.prepare();
            assert!(
                payload.held && !payload.underrun,
                "held silence after a flush"
            );
            assert!(payload.transfer_buffer.iter().all(|byte| *byte == 0));
            assert_eq!(payload.music_frames, 0);
            rig.in_flight.push(payload);
        }
        for _ in 0..8 {
            rig.complete_oldest(0);
        }
        assert_eq!(loss_and_underruns(&stats), (0, 0, 0, false));
        assert!(
            rig.shared.note_bit_perfect(),
            "cancelled transfers do not clear bit-perfect"
        );

        // The refill hold ends at the start threshold, and from then on the
        // new position counts from zero.
        rig.shared
            .write_bytes(&noise(stats.start_threshold_samples * 2, 22))
            .unwrap();
        rig.submit();
        rig.complete_oldest(0);
        assert_eq!(stats.played_frames(), 96);
        assert_eq!(loss_and_underruns(&stats), (0, 0, 0, false));
        assert!(rig.shared.note_bit_perfect());
        rig.hub.shutdown();
    }

    #[test]
    fn a_transfer_that_finished_before_the_cancel_arrived_is_accounted_as_old_audio() {
        let _globals = lock_globals();
        let mut rig = Rig::pcm(2, 16);
        rig.shared.write_bytes(&noise(96 * 4 * 12, 23)).unwrap();
        for _ in 0..8 {
            rig.submit();
        }
        let stats = Arc::clone(&rig.shared.stats);
        let consumed = stats.consumer_frames.load(Ordering::Relaxed);
        rig.shared.flush();

        // The cancel reaches the controller too late for the first two: they
        // complete (libusb reports COMPLETED for a flagged transfer), the other
        // six come back cancelled.
        let (cancelled, raced) = rig.cancel_for_flush(|index| {
            if index < 2 {
                LIBUSB_TRANSFER_COMPLETED
            } else {
                LIBUSB_TRANSFER_CANCELLED
            }
        });
        assert_eq!((cancelled, raced), (6, 2));
        // The two that played took the DAC's time: counted as taken (they were),
        // not as lost, and not as played in the new position.
        assert_eq!(
            stats.consumer_frames.load(Ordering::Relaxed),
            consumed + 192
        );
        assert_eq!(
            stats.played_frames(),
            0,
            "audio of the old position is not played audio"
        );
        assert_eq!(loss_and_underruns(&stats), (0, 0, 0, false));
        assert!(rig.in_flight.is_empty());
    }

    #[test]
    fn a_flush_while_paused_leaves_the_bus_alone() {
        let _globals = lock_globals();
        let mut rig = Rig::pcm(2, 16);
        rig.shared.write_bytes(&noise(96 * 4 * 12, 24)).unwrap();
        rig.shared.set_paused(true);
        for _ in 0..8 {
            let payload = rig.prepare();
            assert!(payload.held, "paused: wire silence");
            rig.in_flight.push(payload);
        }
        let queued_before = rig.shared.stats.buffered_samples.load(Ordering::Relaxed);
        assert!(queued_before > 0, "the ring still holds the paused audio");

        // pause() never cancels, and a flush while paused finds no music on the
        // bus to cut: the DAC stays locked on the silence it is already playing.
        rig.shared.flush();
        let (cancelled, raced) = rig.cancel_for_flush(|_| LIBUSB_TRANSFER_CANCELLED);
        assert_eq!((cancelled, raced), (0, 0));
        assert_eq!(rig.in_flight.len(), 8);
        assert_eq!(
            rig.shared
                .stats
                .flush_cancelled_transfers
                .load(Ordering::Relaxed),
            0
        );

        // The flush emptied the ring and the stream stays paused; the next
        // transfers are still silence, and the accounting is untouched.
        assert_eq!(rig.shared.stats.buffered_samples.load(Ordering::Relaxed), 0);
        assert_eq!(
            rig.shared.current_state(),
            crate::events::StreamState::Paused
        );
        for _ in 0..8 {
            rig.complete_oldest(0);
        }
        assert_eq!(rig.shared.stats.played_frames(), 0);
        assert_eq!(loss_and_underruns(&rig.shared.stats), (0, 0, 0, false));
    }

    #[test]
    fn a_flush_with_music_queued_before_a_pause_cuts_only_what_was_music() {
        let _globals = lock_globals();
        let mut rig = Rig::pcm(2, 16);
        rig.shared.write_bytes(&noise(96 * 4 * 12, 25)).unwrap();
        // Two transfers of music, then the pause puts silence behind them.
        rig.submit();
        rig.submit();
        rig.shared.set_paused(true);
        for _ in 0..6 {
            rig.submit();
        }
        rig.shared.flush();
        let (cancelled, raced) = rig.cancel_for_flush(|_| LIBUSB_TRANSFER_CANCELLED);
        // Music is on the bus, so the old queue goes with it, silence included
        // (it would otherwise play after a hole).
        assert_eq!((cancelled, raced), (8, 0));
        assert!(rig.in_flight.is_empty());
        assert_eq!(loss_and_underruns(&rig.shared.stats), (0, 0, 0, false));
    }

    #[test]
    fn a_flush_during_priming_has_nothing_to_cancel_and_the_stream_still_starts() {
        let _globals = lock_globals();
        let mut rig = Rig::pcm(2, 16);
        let stats = Arc::clone(&rig.shared.stats);
        // Priming: audio is arriving, nothing has been submitted, and a flush
        // drops it.
        rig.shared.write_bytes(&noise(96 * 4 * 3, 26)).unwrap();
        rig.shared.flush();
        let (cancelled, raced) = rig.cancel_for_flush(|_| LIBUSB_TRANSFER_CANCELLED);
        assert_eq!((cancelled, raced), (0, 0));
        assert_eq!(stats.flush_epoch.load(Ordering::Relaxed), 1);
        assert_eq!(stats.buffered_samples.load(Ordering::Relaxed), 0);

        // The priming wait goes on until the threshold, then the first
        // transfers carry the new audio: the epoch is the new one and nothing
        // was cut.
        let fresh = noise(stats.start_threshold_samples * 2 + 96 * 4 * 4, 27);
        rig.shared.write_bytes(&fresh).unwrap();
        let first = rig.prepare();
        assert!(!first.held, "the threshold is met, the music flows");
        assert_eq!(first.epoch, 1);
        assert_eq!(first.music_frames, 96);
        rig.in_flight.push(first);
        rig.complete_oldest(0);
        assert_eq!(stats.played_frames(), 96);
        assert_eq!(loss_and_underruns(&stats), (0, 0, 0, false));
    }

    #[test]
    fn transfers_filled_after_the_flush_are_never_cancelled() {
        let _globals = lock_globals();
        let mut rig = Rig::pcm(2, 16);
        let stats = Arc::clone(&rig.shared.stats);
        rig.shared.write_bytes(&noise(96 * 4 * 4, 28)).unwrap();
        rig.submit();
        rig.submit();
        rig.shared.flush();

        // The new audio is written and two transfers of it are queued before the
        // output thread gets to look at the flush.
        rig.shared
            .write_bytes(&noise(stats.start_threshold_samples * 2 + 96 * 4 * 4, 29))
            .unwrap();
        rig.submit();
        rig.submit();
        assert_eq!(
            rig.in_flight.iter().map(|p| p.epoch).collect::<Vec<_>>(),
            [0, 0, 1, 1]
        );

        let (cancelled, _) = rig.cancel_for_flush(|_| LIBUSB_TRANSFER_CANCELLED);
        assert_eq!(cancelled, 2, "only the old position's transfers");
        assert_eq!(rig.in_flight.len(), 2);
        rig.complete_oldest(0);
        rig.complete_oldest(0);
        assert_eq!(stats.played_frames(), 192, "the new audio plays, from zero");
    }

    #[test]
    fn a_second_flush_cancels_again_and_the_counters_add_up() {
        let _globals = lock_globals();
        let mut rig = Rig::pcm(2, 16);
        let stats = Arc::clone(&rig.shared.stats);
        let refill = stats.start_threshold_samples * 2 + 96 * 4 * 8;
        for seed in 30..32 {
            rig.shared.write_bytes(&noise(refill, seed)).unwrap();
            for _ in 0..4 {
                rig.submit();
            }
            rig.shared.flush();
            let (cancelled, raced) = rig.cancel_for_flush(|_| LIBUSB_TRANSFER_CANCELLED);
            assert_eq!((cancelled, raced), (4, 0));
        }
        assert_eq!(stats.flush_cancelled_transfers.load(Ordering::Relaxed), 8);
        assert_eq!(loss_and_underruns(&stats), (0, 0, 0, false));
    }

    #[test]
    fn a_slot_remembers_the_cancellation_until_the_transfer_comes_back() {
        let mut slot = IsoTransferSlot::new(0, 16, 1024).unwrap();
        // Not on the bus: nothing to cancel, nothing flagged.
        slot.cancel_for_flush();
        assert!(!slot.cancelled_for_flush);

        slot.in_flight = true;
        slot.cancelled_for_flush = true;
        assert!(
            !slot.completion_ready(),
            "libusb has not handed it back yet"
        );
        unsafe { &(*slot.user_data).completion }
            .status
            .lock()
            .unwrap()
            .replace(LIBUSB_TRANSFER_CANCELLED);
        assert!(slot.completion_ready());
        assert_eq!(
            slot.take_completion().map(|(status, _, _)| status),
            Some(LIBUSB_TRANSFER_CANCELLED)
        );
        assert!(!slot.completion_ready(), "taken");

        // The slot is reused: the flag does not leak into the next transfer.
        slot.mark_completed();
        assert!(!slot.cancelled_for_flush);
        assert!(!slot.in_flight);
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
        assert!(
            !payload.underrun,
            "the padded tail after drain() is not an underrun"
        );
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
        assert!(
            rig.shared.note_bit_perfect(),
            "unity gain on a verified stream is bit-perfect"
        );

        // 16-bit samples 0x4000 and -0x4000, repeated.
        let mut input = Vec::new();
        for _ in 0..96 * 2 {
            input.extend_from_slice(&0x4000i16.to_le_bytes());
            input.extend_from_slice(&(-0x4000i16).to_le_bytes());
        }
        rig.shared.write_bytes(&input).unwrap();
        rig.shared.set_software_gain(0.5).unwrap();
        assert!(
            !rig.shared.note_bit_perfect(),
            "any gain other than 1.0 ends bit-perfect"
        );
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
        assert!(
            dop.shared.set_software_gain(1.0).is_ok(),
            "unity is always fine"
        );
        let native = Rig::new(4, 1, 2, DsdTransportMode::Native);
        assert!(native.shared.set_software_gain(0.9).is_err());

        let pcm = Rig::pcm(2, 16);
        for bad in [f32::NAN, f32::INFINITY, -0.5, 17.0] {
            assert!(
                matches!(
                    pcm.shared.set_software_gain(bad),
                    Err(crate::Error::InvalidArgument(_))
                ),
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

        // Error and device notifications may arrive before the final state.
        // Observe terminal delivery rather than an intermediate callback count.
        for _ in 0..400 {
            let terminal = rig
                .events
                .0
                .lock()
                .iter()
                .any(|event| matches!(event, DacEvent::StateChanged(StreamState::Failed)));
            if terminal {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        let events = rig.events.0.lock().clone();
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

/// The DAC's volume control, level and mute are the registration's, not the
/// stream's: they stay usable while the idle lock holds a DAC whose stream has
/// closed, and they go with the registration.
#[cfg(test)]
mod hardware_volume_without_a_stream_tests {
    use super::fake_dac::{control, device, state, Registered, ORIGINAL_RAW};
    use super::*;

    #[test]
    fn a_stopped_stream_keeps_the_control_while_the_idle_lock_holds_the_dac() {
        let mut stopped = state(true, Some(control()));
        let (generation, level) = (
            stopped.hardware_volume_generation,
            stopped.hardware_volume_state,
        );

        stopped.set_stream_active(false, true);

        assert!(!stopped.stream_active);
        assert!(stopped.hardware_volume_control.is_some());
        assert_eq!(stopped.hardware_volume_state, level);
        assert_eq!(stopped.hardware_volume_generation, generation);
    }

    #[test]
    fn a_stopped_stream_without_the_idle_lock_drops_the_control() {
        let mut stopped = state(true, Some(control()));
        let generation = stopped.hardware_volume_generation;

        stopped.set_stream_active(false, false);

        assert!(!stopped.stream_active);
        assert!(stopped.hardware_volume_control.is_none());
        assert_eq!(stopped.hardware_volume_state, None);
        assert_ne!(stopped.hardware_volume_generation, generation);
    }

    #[test]
    fn a_starting_stream_changes_nothing() {
        let mut starting = state(false, Some(control()));
        let generation = starting.hardware_volume_generation;

        starting.set_stream_active(true, false);

        assert!(starting.stream_active);
        assert!(starting.hardware_volume_control.is_some());
        assert_eq!(starting.hardware_volume_generation, generation);
    }

    #[test]
    fn level_mute_and_readback_work_with_the_stream_stopped() {
        let dac = Registered::new(true, true);
        dac.stop_stream_holding_the_dac();
        let generation = dac.generation();
        assert!(has_hardware_volume());

        let written = write_volume_fraction(0.5).unwrap();
        let wrote = quantize_volume(0.5, &control().ranges);
        assert_eq!(dac.raw(), vec![wrote, wrote]);
        assert_eq!(written.db, f64::from(wrote) / 256.0);
        assert_eq!(cached_volume_state(), Some(written));

        let muted = write_mute(true).unwrap();
        assert!(dac.dac.lock().unwrap().muted);
        assert_eq!(muted.muted, Some(true));
        assert_eq!(cached_volume_state().unwrap().muted, Some(true));
        assert_eq!(cached_volume_state().unwrap().db, written.db);

        // A knob turned on the DAC is seen by the readback.
        dac.dac.lock().unwrap().volume = [ORIGINAL_RAW; 3];
        let seen = read_volume_state().unwrap();
        assert_eq!(seen.db, f64::from(ORIGINAL_RAW) / 256.0);
        assert_eq!(cached_volume_state(), Some(seen));

        // None of it was a change of route.
        assert_eq!(dac.generation(), generation);
        assert!(!SESSION_STATE.lock().as_ref().unwrap().stream_active);
    }

    #[test]
    fn db_and_step_writes_work_with_the_stream_stopped_and_are_verified() {
        let dac = Registered::new(true, true);
        dac.stop_stream_holding_the_dac();

        let state = write_volume_db(-30.4).unwrap();
        assert_eq!(state.db, -30.0, "snapped to the DAC's 1 dB steps");
        assert_eq!(dac.raw(), vec![-30 * 256, -30 * 256]);
        let up = step_volume(1, 16).unwrap();
        assert!(up.db > -30.0);
        let down = step_volume(-1, 16).unwrap();
        assert!(down.db < up.db);
        // Reading is a step of zero.
        assert_eq!(step_volume(0, 16).unwrap(), down);
    }

    #[test]
    fn a_dac_that_cannot_be_reached_fails_the_write_without_losing_the_control() {
        let dac = Registered::new(true, true);
        dac.stop_stream_holding_the_dac();
        dac.dac.lock().unwrap().unreachable = true;

        assert!(write_volume_fraction(0.5).is_err());
        assert!(read_volume_state().is_err());

        // The error is the DAC's; the control is still the registration's.
        assert!(has_hardware_volume());
        dac.dac.lock().unwrap().unreachable = false;
        write_volume_fraction(0.5).unwrap();
    }

    #[test]
    fn a_write_in_flight_is_not_failed_by_the_stream_stopping() {
        let dac = Registered::new(true, true);
        let (arrived_tx, arrived) = crossbeam_channel::bounded(1);
        let (release, release_rx) = crossbeam_channel::bounded(1);
        dac.dac.lock().unwrap().hold_next_volume_write = Some((arrived_tx, release_rx));

        let write = thread::spawn(|| write_volume_fraction(0.25));
        arrived
            .recv_timeout(Duration::from_secs(5))
            .expect("the write reached the DAC");
        let generation = dac.generation();
        dac.stop_stream_holding_the_dac();
        assert_eq!(
            dac.generation(),
            generation,
            "a stopped stream is not a new route"
        );
        release.send(()).unwrap();

        let state = write.join().unwrap().unwrap();
        let wrote = quantize_volume(0.25, &control().ranges);
        assert_eq!(dac.raw(), vec![wrote, wrote]);
        assert_eq!(cached_volume_state(), Some(state));
    }

    #[test]
    fn the_stream_that_follows_keeps_the_same_route_and_replaces_a_different_one() {
        let dac = Registered::new(true, true);
        dac.stop_stream_holding_the_dac();
        let generation = dac.generation();
        let level = cached_volume_state();

        // Discovery at the next stream start finds the same Feature Unit: the
        // route is the same route, so a write that is in flight stays valid.
        install_volume_control(Some(control()));
        assert_eq!(dac.generation(), generation);
        assert_eq!(cached_volume_state(), level);
        assert!(has_hardware_volume());

        // A stream on another terminal finds another unit: a new route, read again.
        let mut other = control();
        other.feature_unit_id += 1;
        install_volume_control(Some(other));
        assert_ne!(dac.generation(), generation);
        assert_eq!(cached_volume_state(), None, "read again, not carried over");
        assert!(has_hardware_volume());
    }

    #[test]
    fn clearing_the_registration_ends_the_control() {
        let dac = Registered::new(true, true);
        dac.stop_stream_holding_the_dac();

        clear_session_state();

        assert!(!has_hardware_volume());
        assert!(write_volume_fraction(0.5).is_err());
        assert!(write_mute(true).is_err());
        assert!(read_volume_state().is_err());
        assert!(dac.dac.lock().unwrap().volume_writes.is_empty());
    }

    #[test]
    fn the_original_level_is_handed_back_after_the_stream_closed() {
        let dac = Registered::new(true, true);
        // Found with the first stream, before the library wrote anything.
        dac.dac.lock().unwrap().volume = [ORIGINAL_RAW; 3];
        {
            let _hold = HARDWARE_VOLUME_MUTEX.lock();
            note_original_hardware_volume(&device(), &control());
        }
        write_volume_fraction(0.2).unwrap();
        assert_ne!(dac.raw(), vec![ORIGINAL_RAW; 2]);
        dac.stop_stream_holding_the_dac();
        assert_eq!(original_volume_db(), Some(-20.0));

        // The host hands the DAC to another driver: close with the restore action.
        assert!(set_original_volume_action(ORIGINAL_VOLUME_RESTORE));
        clear_session_state();

        assert_eq!(dac.raw(), vec![ORIGINAL_RAW; 2]);
        assert!(SESSION_STATE.lock().is_none());
    }

    #[test]
    fn hand_back_restores_the_original_with_the_stream_stopped_and_ends_the_record() {
        let dac = Registered::new(true, true);
        dac.dac.lock().unwrap().volume = [ORIGINAL_RAW; 3];
        {
            let _hold = HARDWARE_VOLUME_MUTEX.lock();
            note_original_hardware_volume(&device(), &control());
        }
        write_volume_fraction(0.9).unwrap();
        dac.stop_stream_holding_the_dac();

        assert_eq!(hand_back_original_volume(), Ok(true));
        assert_eq!(dac.raw(), vec![ORIGINAL_RAW; 2]);
        assert_eq!(
            original_volume_db(),
            None,
            "the record ends once handed back"
        );
        assert_eq!(hand_back_original_volume(), Ok(false));
    }
}

/// What `Dac::open` does with the DAC's level before any stream can start: the
/// start-up attenuation, its verification, the original level it must preserve,
/// and what happens to a DAC whose level cannot be made safe. The fake Feature
/// Unit answers the control transfers in-process.
#[cfg(test)]
mod startup_volume_tests {
    use super::fake_dac::{control, device, Registered};
    use super::*;
    use crate::config::{DacConfig, StartupVolume};

    fn db(db: i32) -> i16 {
        (db * 256) as i16
    }

    fn attenuate(max_db: f64) -> DacConfig {
        DacConfig {
            startup_volume: StartupVolume::Attenuate { max_db },
            ..DacConfig::default()
        }
    }

    /// What the library does once discovery has found `control`.
    fn open_with(dac: &Registered, config: DacConfig, control: HardwareVolumeControl) {
        dac.configure(config);
        let _hold = HARDWARE_VOLUME_MUTEX.lock();
        adopt_volume_control(&device(), Some(control));
    }

    fn report() -> crate::volume::StartupVolumeReport {
        startup_report().expect("a control with a start-up report")
    }

    #[test]
    fn a_loud_dac_comes_up_at_the_ceiling_with_the_original_recorded_first() {
        let dac = Registered::new(false, false);
        dac.set_levels(db(0), db(0));
        open_with(&dac, attenuate(-40.0), control());

        assert_eq!(dac.raw(), vec![db(-40), db(-40)]);
        let report = report();
        assert_eq!(report.original_db, vec![0.0, 0.0]);
        assert_eq!(report.applied_db, vec![-40.0, -40.0]);
        assert_eq!(report.limit_db, 0.0, "the quietest original channel");
        assert!(report.attenuated && !report.clamped_to_minimum);
        assert_eq!(report.max_db, Some(-40.0));
        // The record holds the pre-attenuation level, not the attenuated one.
        assert_eq!(original_volume_db(), Some(0.0));
        // The control works and its cached level is the attenuated one.
        assert!(has_hardware_volume());
        assert_eq!(cached_volume_state().unwrap().db, -40.0);
        assert_eq!(
            startup_lowered_volume().unwrap().db,
            -40.0,
            "the open changed the level"
        );
    }

    #[test]
    fn a_channel_is_never_raised() {
        let dac = Registered::new(false, false);
        dac.set_levels(db(-64), db(-64));
        open_with(&dac, attenuate(-40.0), control());

        assert_eq!(dac.raw(), vec![db(-64), db(-64)]);
        assert!(
            dac.dac.lock().unwrap().volume_writes.is_empty(),
            "nothing was written"
        );
        let report = report();
        assert_eq!(report.applied_db, report.original_db);
        assert_eq!(report.limit_db, -64.0);
        assert!(!report.attenuated);
        assert_eq!(startup_lowered_volume(), None, "no change, no event");
        assert!(has_hardware_volume());
    }

    #[test]
    fn the_ceiling_rounds_down_to_a_step_the_dac_supports() {
        let dac = Registered::new(false, false);
        dac.set_levels(db(0), db(0));
        let mut coarse = control();
        coarse.ranges = vec![RawRange {
            min: db(-80),
            max: 0,
            resolution: 3 * 256,
        }];
        coarse.resolution_raw = 3 * 256;
        open_with(&dac, attenuate(-40.0), coarse);

        // -80, -77, ... -41, -38: the ceiling is the step below it.
        assert_eq!(dac.raw(), vec![db(-41), db(-41)]);
        assert_eq!(report().applied_db, vec![-41.0, -41.0]);
    }

    #[test]
    fn a_ceiling_below_the_dacs_minimum_clamps_to_it_and_reports_that() {
        let dac = Registered::new(false, false);
        dac.set_levels(db(0), db(-10));
        let mut shallow = control();
        shallow.ranges = vec![RawRange {
            min: db(-30),
            max: 0,
            resolution: 256,
        }];
        shallow.min_volume_raw = db(-30);
        open_with(&dac, attenuate(-40.0), shallow);

        assert_eq!(dac.raw(), vec![db(-30), db(-30)]);
        let report = report();
        assert!(report.clamped_to_minimum);
        assert_eq!(report.applied_db, vec![-30.0, -30.0]);
        assert_eq!(
            report.max_db,
            Some(-40.0),
            "what was asked is still reported"
        );
    }

    #[test]
    fn a_channel_already_at_the_silent_value_is_kept() {
        let dac = Registered::new(false, false);
        // The left channel sits at the DAC's minimum (silence); the right is loud.
        dac.set_levels(db(-80), db(0));
        open_with(&dac, attenuate(-40.0), control());

        let writes = dac.dac.lock().unwrap().volume_writes.clone();
        assert!(
            writes.iter().all(|(channel, _)| *channel == 2),
            "the silent channel is never written: {writes:?}"
        );
        // One level for the control: the loud channel follows the silent one.
        assert_eq!(dac.raw(), vec![db(-80), db(-80)]);
        let report = report();
        assert_eq!(report.original_db, vec![-80.0, 0.0]);
        assert_eq!(report.limit_db, -80.0);
    }

    #[test]
    fn channels_are_lowered_independently_and_each_original_is_kept_for_hand_back() {
        let dac = Registered::new(false, false);
        dac.set_levels(db(-10), db(-70));
        open_with(&dac, attenuate(-40.0), control());

        // Left was loud and comes down; right was already quiet and is not written.
        let writes = dac.dac.lock().unwrap().volume_writes.clone();
        assert_eq!(
            writes,
            vec![(1, db(-70))],
            "only the loud channel, and nothing is raised"
        );
        assert_eq!(dac.raw(), vec![db(-70), db(-70)]);
        let report = report();
        assert_eq!(report.original_db, vec![-10.0, -70.0]);
        assert_eq!(report.applied_db, vec![-70.0, -70.0]);
        assert_eq!(report.limit_db, -70.0);
        // Different originals have no single level...
        assert_eq!(original_volume_db(), None);

        // ...but each one is restored.
        assert_eq!(hand_back_original_volume(), Ok(true));
        assert_eq!(dac.raw(), vec![db(-10), db(-70)]);
    }

    #[test]
    fn keep_leaves_the_levels_alone() {
        let dac = Registered::new(false, false);
        dac.set_levels(db(0), db(0));
        let keep = DacConfig {
            startup_volume: StartupVolume::Keep,
            ..DacConfig::default()
        };
        open_with(&dac, keep, control());

        assert_eq!(dac.raw(), vec![db(0), db(0)]);
        assert!(dac.dac.lock().unwrap().volume_writes.is_empty());
        let report = report();
        assert_eq!(report.applied_db, report.original_db);
        assert!(!report.attenuated);
        assert_eq!(report.max_db, None);
        assert_eq!(report.limit_db, 0.0);
        assert!(
            has_hardware_volume(),
            "the control is there, just not lowered"
        );
        assert_eq!(startup_lowered_volume(), None);
    }

    #[test]
    fn a_dac_that_does_not_take_the_attenuation_is_unusable_and_refused() {
        let dac = Registered::new(false, false);
        dac.set_levels(db(0), db(0));
        dac.dac.lock().unwrap().ignore_volume_writes = true;
        open_with(&dac, attenuate(-40.0), control());

        // The read-back still says 0 dB: the attenuation cannot be verified.
        assert!(!has_hardware_volume(), "no control is offered");
        assert!(startup_report().is_none());
        let reason = SESSION_STATE
            .lock()
            .as_ref()
            .and_then(|state| state.hardware_volume_error.clone())
            .unwrap();
        assert!(
            reason.contains("start-up volume protection failed"),
            "{reason}"
        );
        assert!(reason.contains("could not be verified"), "{reason}");

        // With a control required, a stream is refused; otherwise it may go ahead.
        assert_eq!(
            ensure_volume_if_required(true),
            Err(crate::Error::VolumeUnavailable)
        );
        assert_eq!(ensure_volume_if_required(false), Ok(()));
        // What the library may have changed is still on record for hand-back.
        assert!(ORIGINAL_HARDWARE_VOLUME
            .lock()
            .is_recorded(&original_volume_device_key(&device())));
    }

    #[test]
    fn a_dac_that_cannot_be_read_is_unusable_too() {
        let dac = Registered::new(false, false);
        dac.dac.lock().unwrap().unreachable = true;
        open_with(&dac, attenuate(-40.0), control());
        assert!(!has_hardware_volume());
        assert_eq!(
            ensure_volume_if_required(true),
            Err(crate::Error::VolumeUnavailable)
        );
    }

    #[test]
    fn no_control_at_all_is_refused_only_when_one_is_required() {
        let none = Registered::new(false, false);
        assert!(!has_hardware_volume());
        assert_eq!(
            ensure_volume_if_required(true),
            Err(crate::Error::VolumeUnavailable)
        );
        assert_eq!(ensure_volume_if_required(false), Ok(()));
        drop(none); // the fake registration is process-wide: one at a time
        let dac = Registered::new(false, true);
        assert_eq!(
            ensure_volume_if_required(true),
            Ok(()),
            "a DAC with a control is never refused"
        );
        drop(dac);
    }

    #[test]
    fn a_stream_whose_own_route_has_no_volume_is_refused_even_if_open_found_one() {
        // Open found a control through the first playback stream; the stream that
        // is being opened plays through a terminal that has none.
        let dac = Registered::new(false, false);
        dac.set_levels(db(0), db(0));
        open_with(&dac, attenuate(-40.0), control());
        assert_eq!(ensure_volume_if_required(true), Ok(()));
        {
            let _hold = HARDWARE_VOLUME_MUTEX.lock();
            adopt_volume_control(&device(), None);
        }
        assert!(!has_hardware_volume());
        assert_eq!(
            ensure_volume_if_required(true),
            Err(crate::Error::VolumeUnavailable)
        );
        assert_eq!(
            ensure_volume_if_required(false),
            Ok(()),
            "a fixed level the user allowed"
        );
    }

    #[test]
    fn hand_back_restores_the_true_original_and_it_survives_a_reopen() {
        let dac = Registered::new(false, false);
        dac.set_levels(db(-5), db(-5));
        open_with(&dac, attenuate(-40.0), control());
        assert_eq!(dac.raw(), vec![db(-40), db(-40)]);

        // The session ends with the level kept (the host takes the DAC straight
        // back) and a new one opens the same DAC: it is already at the ceiling,
        // so nothing is written, and the report still names the true original.
        *SESSION_STATE.lock() = Some(fake_dac::state(false, None));
        dac.dac.lock().unwrap().volume_writes.clear();
        open_with(&dac, attenuate(-40.0), control());
        assert!(dac.dac.lock().unwrap().volume_writes.is_empty());
        let report = report();
        assert_eq!(report.original_db, vec![-5.0, -5.0]);
        assert_eq!(report.applied_db, vec![-40.0, -40.0]);
        assert!(
            report.attenuated,
            "below the level it had before the library touched it"
        );
        assert_eq!(report.limit_db, -5.0);
        assert_eq!(startup_lowered_volume(), None, "this open wrote nothing");

        // Closing for the restore action puts the true original back.
        assert!(set_original_volume_action(ORIGINAL_VOLUME_RESTORE));
        clear_session_state();
        assert_eq!(dac.raw(), vec![db(-5), db(-5)]);
    }

    #[test]
    fn a_louder_level_left_by_a_kept_session_is_brought_down_again() {
        let dac = Registered::new(false, false);
        dac.set_levels(db(-5), db(-5));
        open_with(&dac, attenuate(-40.0), control());
        write_volume_db(-10.0).unwrap();

        *SESSION_STATE.lock() = Some(fake_dac::state(false, None));
        open_with(&dac, attenuate(-40.0), control());
        assert_eq!(dac.raw(), vec![db(-40), db(-40)]);
        assert_eq!(
            report().original_db,
            vec![-5.0, -5.0],
            "still the first original"
        );
    }

    #[test]
    fn a_route_that_comes_back_keeps_the_level_the_host_set_since() {
        let dac = Registered::new(false, false);
        dac.set_levels(db(0), db(0));
        open_with(&dac, attenuate(-40.0), control());
        write_volume_db(-20.0).unwrap();
        let writes = dac.dac.lock().unwrap().volume_writes.len();

        // The next stream's discovery finds the same Feature Unit: the start-up
        // step has run for it, so the host's -20 dB is not pulled back to -40.
        {
            let _hold = HARDWARE_VOLUME_MUTEX.lock();
            adopt_volume_control(&device(), Some(control()));
        }
        assert_eq!(dac.raw(), vec![db(-20), db(-20)]);
        assert_eq!(dac.dac.lock().unwrap().volume_writes.len(), writes);
        assert_eq!(cached_volume_state().unwrap().db, -20.0);
        // The original is still the first one.
        assert_eq!(original_volume_db(), Some(0.0));
    }

    #[test]
    fn another_feature_unit_is_a_new_route_that_still_needs_its_start_up_step() {
        let dac = Registered::new(false, false);
        dac.set_levels(db(0), db(0));
        open_with(&dac, attenuate(-40.0), control());
        assert!(!startup_volume_pending(&control()));
        let mut other = control();
        other.feature_unit_id += 1;
        assert!(
            startup_volume_pending(&other),
            "its level was never lowered"
        );
    }

    #[test]
    fn volume_works_without_a_stream_after_the_start_up_step() {
        let dac = Registered::new(false, false);
        dac.set_levels(db(0), db(0));
        open_with(&dac, attenuate(-40.0), control());
        assert!(!SESSION_STATE.lock().as_ref().unwrap().stream_active);

        let state = write_volume_fraction(0.5).unwrap();
        assert!(state.db > -80.0 && state.db < 0.0);
        assert_eq!(write_mute(true).unwrap().muted, Some(true));
        assert_eq!(read_volume_state().unwrap().db, state.db);
        // A level above the ceiling is the host's choice once the DAC is open.
        assert_eq!(write_volume_db(-3.0).unwrap().db, -3.0);
    }

    #[test]
    fn capabilities_say_whether_a_hardware_volume_exists() {
        let with = Registered::new(false, true);
        SESSION_STATE.lock().as_mut().unwrap().capability_model = Some(CapabilityModel::default());
        let capabilities = crate::transport::api::capabilities().unwrap();
        assert!(capabilities.has_hardware_volume && capabilities.volume.is_some());
        drop(with);

        let _without = Registered::new(false, false);
        SESSION_STATE.lock().as_mut().unwrap().capability_model = Some(CapabilityModel::default());
        let capabilities = crate::transport::api::capabilities().unwrap();
        assert!(!capabilities.has_hardware_volume && capabilities.volume.is_none());
    }
}

/// Asynchronous endpoints without an explicit feedback endpoint or a compatible
/// implicit feedback source are refused unless the configuration
/// allows streaming them at the nominal rate, and then never bit-perfect.
#[cfg(test)]
mod unsynchronized_async_tests {
    use super::fake_dac::{control, state};
    use super::*;
    use crate::capabilities::AltSetting;

    #[test]
    fn an_async_endpoint_with_feedback_is_admitted() {
        for allow in [false, true] {
            assert_eq!(
                admit_endpoint(SyncType::Asynchronous, 0x81, true, allow),
                EndpointAdmission::Admit
            );
        }
    }

    #[test]
    fn an_async_endpoint_without_feedback_is_refused_unless_allowed() {
        assert_eq!(
            admit_endpoint(SyncType::Asynchronous, 0, false, false),
            EndpointAdmission::RefuseUnsynchronized
        );
        assert_eq!(
            admit_endpoint(SyncType::Asynchronous, 0, false, true),
            EndpointAdmission::Admit
        );
    }

    #[test]
    fn a_declared_but_unresolvable_feedback_endpoint_is_never_streamed() {
        for allow in [false, true] {
            assert_eq!(
                admit_endpoint(SyncType::Asynchronous, 0x81, false, allow),
                EndpointAdmission::SkipMalformed
            );
        }
    }

    #[test]
    fn adaptive_and_synchronous_endpoints_need_no_feedback() {
        for sync in [SyncType::Adaptive, SyncType::Synchronous, SyncType::NoSync] {
            for allow in [false, true] {
                assert_eq!(
                    admit_endpoint(sync, 0, false, allow),
                    EndpointAdmission::Admit
                );
            }
        }
    }

    #[test]
    fn the_refusal_is_its_own_error_and_a_plain_mismatch_stays_unsupported() {
        let refusal = SelectError::ImplicitFeedback("async, no feedback".into());
        assert_eq!(
            refusal.into_error(Vec::<AltSetting>::new()),
            crate::Error::ImplicitFeedbackUnsupported
        );
        let mismatch = SelectError::NoMatch("no 7.1".into());
        assert!(matches!(
            mismatch.into_error(Vec::new()),
            crate::Error::Unsupported { ref reason, .. } if reason == "no 7.1"
        ));
    }

    #[test]
    fn a_nominal_rate_async_stream_is_never_bit_perfect_and_says_why() {
        let _globals = PAYLOAD_LATCH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        USB_PCM_PAYLOAD_LATCH.lock().reset();
        USB_PCM_PAYLOAD_LATCH.lock().observe(true, Instant::now());
        let mut stream = state(true, Some(control()));
        assert_eq!(bit_perfect_blocker(&stream), None);
        assert!(direct_path_is_bit_perfect(&stream));

        stream.unsynchronized_async = true;
        assert_eq!(
            bit_perfect_blocker(&stream),
            Some(BitPerfectBlocker::UnsynchronizedAsync)
        );
        assert!(!direct_path_is_bit_perfect(&stream));
        assert!(
            bit_perfect_release_reason(&stream).contains("asynchronous endpoint without feedback")
        );

        // The flag belongs to the stream: it ends with it.
        stream.set_stream_active(false, true);
        assert!(!stream.unsynchronized_async);
    }

    #[test]
    fn the_blockers_are_reported_in_order_of_precedence() {
        let _globals = PAYLOAD_LATCH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        USB_PCM_PAYLOAD_LATCH.lock().reset();
        let mut stream = state(true, None);
        // Nothing audited yet.
        assert_eq!(
            bit_perfect_blocker(&stream),
            Some(BitPerfectBlocker::PayloadUnverified)
        );
        USB_PCM_PAYLOAD_LATCH.lock().observe(true, Instant::now());
        assert_eq!(bit_perfect_blocker(&stream), None);

        stream.software_volume_active = true;
        assert_eq!(
            bit_perfect_blocker(&stream),
            Some(BitPerfectBlocker::SoftwareGain)
        );
        stream.clock_verification_passed = false;
        assert_eq!(
            bit_perfect_blocker(&stream),
            Some(BitPerfectBlocker::ClockUnverified)
        );
        stream
            .requested_playback_format
            .as_mut()
            .unwrap()
            .sample_rate = 96_000;
        assert_eq!(
            bit_perfect_blocker(&stream),
            Some(BitPerfectBlocker::FormatChanged)
        );
        stream.stream_active = false;
        assert_eq!(
            bit_perfect_blocker(&stream),
            Some(BitPerfectBlocker::NoStream)
        );
        assert_eq!(
            BitPerfectBlocker::UnsynchronizedAsync.code(),
            "unsynchronized_async"
        );
    }
}

/// The pacing label: what the transport does for an admitted endpoint, and
/// `None` for the ones it would never stream.
#[cfg(test)]
mod pacing_tests {
    use super::*;
    use crate::capabilities::Pacing;

    const FEEDBACK: u8 = 0x81;

    /// (sync type, bSynchAddress, resolved feedback endpoint, allow unsynchronized async,
    /// expected pacing).
    type Row = (SyncType, u8, Option<u8>, bool, Option<Pacing>);

    fn table() -> Vec<Row> {
        let explicit = Some(Pacing::ExplicitFeedback { endpoint: FEEDBACK });
        let mut rows: Vec<Row> = Vec::new();
        for allow in [false, true] {
            rows.extend([
                // Asynchronous with its feedback endpoint: polled, whatever the config says.
                (
                    SyncType::Asynchronous,
                    FEEDBACK,
                    Some(FEEDBACK),
                    allow,
                    explicit,
                ),
                // The feedback endpoint found by its usage bits rather than bSynchAddress.
                (SyncType::Asynchronous, 0, Some(FEEDBACK), allow, explicit),
                // A declared feedback endpoint that cannot be resolved is never streamed.
                (SyncType::Asynchronous, FEEDBACK, None, allow, None),
                (SyncType::Adaptive, FEEDBACK, None, allow, None),
                // Adaptive and synchronous follow the host's packet timing.
                (SyncType::Adaptive, 0, None, allow, Some(Pacing::Adaptive)),
                (
                    SyncType::Synchronous,
                    0,
                    None,
                    allow,
                    Some(Pacing::Synchronous),
                ),
                // The transport only polls feedback for an asynchronous endpoint.
                (
                    SyncType::Adaptive,
                    FEEDBACK,
                    Some(FEEDBACK),
                    allow,
                    Some(Pacing::Adaptive),
                ),
                (
                    SyncType::Synchronous,
                    FEEDBACK,
                    Some(FEEDBACK),
                    allow,
                    Some(Pacing::Synchronous),
                ),
                // No synchronisation declared: nothing comes back, nominal rate.
                (
                    SyncType::NoSync,
                    0,
                    None,
                    allow,
                    Some(Pacing::Unsynchronized),
                ),
            ]);
        }
        rows.extend([
            // Asynchronous without feedback: refused unless the config allows the nominal rate.
            (SyncType::Asynchronous, 0, None, false, None),
            (
                SyncType::Asynchronous,
                0,
                None,
                true,
                Some(Pacing::Unsynchronized),
            ),
        ]);
        rows
    }

    #[test]
    fn each_endpoint_shape_has_the_pacing_the_transport_gives_it() {
        for (sync, synch_address, feedback, allow, expected) in table() {
            assert_eq!(
                endpoint_pacing(sync, synch_address, feedback, allow),
                expected,
                "{sync:?} synch=0x{synch_address:02x} feedback={feedback:?} allow={allow}"
            );
        }
    }

    #[test]
    fn an_endpoint_has_a_pacing_exactly_when_the_scan_admits_it() {
        for (sync, synch_address, feedback, allow, _) in table() {
            let admitted = admit_endpoint(sync, synch_address, feedback.is_some(), allow)
                == EndpointAdmission::Admit;
            assert_eq!(
                endpoint_pacing(sync, synch_address, feedback, allow).is_some(),
                admitted,
                "{sync:?} synch=0x{synch_address:02x} feedback={feedback:?} allow={allow}"
            );
        }
    }

    #[test]
    fn the_negotiated_label_matches_the_capability_label_of_the_same_endpoint() {
        // `negotiate` labels the admitted candidate with `admitted_pacing`, the
        // capability scan with `endpoint_pacing`: the same answer for the same endpoint.
        for (sync, synch_address, feedback, allow, expected) in table() {
            if let Some(pacing) = expected {
                assert_eq!(admitted_pacing(sync, feedback), pacing);
                assert_eq!(
                    endpoint_pacing(sync, synch_address, feedback, allow),
                    Some(pacing)
                );
            }
        }
    }

    #[test]
    fn unsynchronized_asynchronous_pacing_is_the_streams_unsynchronized_async_flag() {
        // stream_open sets `unsynchronized_async` (the UnsynchronizedAsync bit-perfect
        // blocker) when an asynchronous endpoint has no feedback endpoint.
        for sync in [
            SyncType::Asynchronous,
            SyncType::Adaptive,
            SyncType::Synchronous,
            SyncType::NoSync,
        ] {
            for feedback in [None, Some(FEEDBACK)] {
                let flag = sync == SyncType::Asynchronous && feedback.is_none();
                let labelled = sync == SyncType::Asynchronous
                    && admitted_pacing(sync, feedback) == Pacing::Unsynchronized;
                assert_eq!(flag, labelled, "{sync:?} feedback={feedback:?}");
            }
        }
    }

    #[test]
    fn pacing_codes_are_stable() {
        assert_eq!(
            Pacing::ExplicitFeedback { endpoint: 0x83 }.code(),
            "explicit_feedback"
        );
        assert_eq!(
            Pacing::ExplicitFeedback { endpoint: 0x83 }.feedback_endpoint(),
            Some(0x83)
        );
        assert_eq!(Pacing::Adaptive.code(), "adaptive");
        assert_eq!(Pacing::Synchronous.code(), "synchronous");
        assert_eq!(Pacing::Unsynchronized.code(), "unsynchronized");
        assert_eq!(Pacing::Adaptive.feedback_endpoint(), None);
    }
}

#[cfg(test)]
mod type_i_exact_tests {
    use super::*;

    #[test]
    fn unsigned8_and_float32_use_the_real_ring_and_encoder_without_changing_source_bits() {
        let _guard = PAYLOAD_LATCH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        for encoding in [PcmEncoding::Unsigned8, PcmEncoding::Float32] {
            let slot = if encoding == PcmEncoding::Unsigned8 {
                1
            } else {
                4
            };
            let stats = Arc::new(RuntimeStats::new(48_000, 2, 40, 20, 1, 10));
            let ring = Arc::new(Mutex::new(PcmRing::new(stats.buffer_capacity_samples, 2)));
            let mut format = test_pcm_format();
            format.pcm_encoding = encoding;
            format.bit_depth = slot * 8;
            let hub = crate::events::EventHub::start();
            let shared = StreamShared::new(ring.clone(), stats.clone(), hub, format, slot);
            let mut candidate = super::pcm_ring_hold_tests::pcm_candidate();
            candidate.format_tag = encoding.format_tag();
            candidate.subslot_size = slot;
            candidate.bit_resolution = slot * 8;
            let source: Vec<u8> = if encoding == PcmEncoding::Unsigned8 {
                (0..=255u8).cycle().take(1024).collect()
            } else {
                // Exact IEEE bit patterns: signed zero, infinities, quiet/signaling NaNs, subnormals.
                [
                    0u32, 0x80000000, 0x7f800000, 0xff800000, 0x7fc12345, 0x7fa12345, 1, 0x80000001,
                ]
                .into_iter()
                .cycle()
                .take(256)
                .flat_map(u32::to_le_bytes)
                .collect()
            };
            assert_eq!(shared.write_bytes(&source).unwrap(), source.len());
            assert!(shared.write_samples(&[0, 0]).is_err());
            assert!(shared.set_software_gain(0.5).is_err());
            let mut scheduler = IsoPacketScheduler::new(48_000, usize::from(slot) * 2, 125);
            let silence = if encoding == PcmEncoding::Unsigned8 {
                0x80
            } else {
                0
            };
            shared.set_paused(true);
            let prepare = |scheduler: &mut IsoPacketScheduler| {
                prepare_iso_transfer_payload(
                    scheduler,
                    &ring,
                    &stats,
                    &candidate,
                    usize::from(slot),
                    2,
                    usize::from(slot) * 2,
                    DsdTransportMode::None,
                    false,
                    false,
                )
                .unwrap()
                .unwrap()
            };
            let held = prepare(&mut scheduler);
            assert!(held.transfer_buffer.iter().all(|byte| *byte == silence));
            assert_eq!(ring.lock().len_samples() * usize::from(slot), source.len());
            shared.set_paused(false);
            shared.drain();
            let mut restored = Vec::new();
            while ring.lock().len_samples() > 0 {
                let payload = prepare(&mut scheduler);
                let music_bytes = payload.music_frames * 2 * usize::from(slot);
                restored.extend_from_slice(&payload.transfer_buffer[..music_bytes]);
                assert!(payload.transfer_buffer[music_bytes..]
                    .iter()
                    .all(|byte| *byte == silence));
            }
            assert_eq!(restored, source);
            shared.flush();
            let primed = prepare(&mut scheduler);
            assert!(primed.held);
            assert!(primed.transfer_buffer.iter().all(|byte| *byte == silence));
        }
    }

    #[test]
    fn unsigned_and_float_requests_are_distinct_and_type_ii_iii_never_become_pcm() {
        for request in [
            crate::StreamRequest::pcm_u8(48_000, 2),
            crate::StreamRequest::pcm_float32(48_000, 2),
        ] {
            let format = api::playback_format_from_request(&request).unwrap();
            assert_ne!(format.pcm_encoding, PcmEncoding::Signed);
            let mut candidate = super::pcm_ring_hold_tests::pcm_candidate();
            candidate.subslot_size = request.bit_depth / 8;
            candidate.bit_resolution = request.bit_depth;
            assert!(validate_transport_against_playback_format(&candidate, format).is_err());
            candidate.format_tag = format.pcm_encoding.format_tag();
            assert!(validate_transport_against_playback_format(&candidate, format).is_ok());
        }
        for kind in [2, 3] {
            let bytes = [
                16, 0x24, 1, 1, 0, kind, 1, 0, 0, 0, 2, 3, 0, 0, 0, 0, 6, 0x24, 2, kind, 4, 32,
            ];
            assert!(parse_streaming_extra(0x20, &bytes).is_none());
        }
    }
}

#[cfg(test)]
mod ambiguous_pcm_format_tests {
    use super::*;
    #[test]
    fn equal_geometry_signed32_float32_bitmap_never_selects_a_numeric_interpretation() {
        let mut bytes = [
            16, 0x24, 1, 1, 0, 1, 5, 0, 0, 0, 2, 3, 0, 0, 0, 0, 6, 0x24, 2, 1, 4, 32,
        ];
        assert!(parse_streaming_extra(0x20, &bytes).is_none());
        bytes[6] = 4;
        assert_eq!(
            parse_streaming_extra(0x20, &bytes).unwrap().format_tag,
            FORMAT_TAG_IEEE_FLOAT
        );
        bytes[6] = 1;
        assert_eq!(
            parse_streaming_extra(0x20, &bytes).unwrap().format_tag,
            FORMAT_TAG_PCM
        );
    }
}

#[cfg(test)]
mod empty_reconfigure_drain_tests {
    use super::*;
    #[test]
    fn unprimed_empty_stream_drains_without_waiting_for_audio_that_will_never_arrive() {
        let stats = Arc::new(RuntimeStats::new(48_000, 2, 40, 20, 10, 10));
        let ring = Arc::new(Mutex::new(PcmRing::new(stats.buffer_capacity_samples, 2)));
        let shared = StreamShared::new(
            ring,
            stats.clone(),
            crate::events::EventHub::start(),
            test_pcm_format(),
            2,
        );
        assert!(!stats.started.load(Ordering::Acquire));
        shared.drain();
        assert!(shared.is_drained());
        shared.flush();
        assert!(!shared.is_drained());
        shared.write_bytes(&[1, 2, 3, 4]).unwrap();
        shared.drain();
        assert!(!shared.is_drained());
    }
}
