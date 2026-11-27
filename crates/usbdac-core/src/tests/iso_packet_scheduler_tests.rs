use crate::iso_packet_scheduler::{IsoPacketScheduler, IsoQueueGeometry};

/// The geometry of one service interval with the defaults.
fn default_geometry(interval: u32) -> IsoQueueGeometry {
    IsoQueueGeometry::default_for(interval)
}

#[test]
fn every_service_interval_queues_the_same_16_ms() {
    // (service interval, packets per transfer): a transfer carries 2 ms.
    for (interval, packets) in [(125u32, 16usize), (250, 8), (500, 4), (1_000, 2)] {
        let geometry = default_geometry(interval);
        assert_eq!(geometry.packets_per_transfer, packets, "{interval} us");
        assert_eq!(geometry.transfer_slots, 8, "{interval} us");
        assert_eq!(geometry.transfer_us(), 2_000, "{interval} us");
        assert_eq!(geometry.queued_us(), 16_000, "{interval} us");
        assert_eq!(geometry.packets_in_flight(), packets * 8, "{interval} us");
        // The observation window is 256 ms of completed transfers for every one.
        assert_eq!(geometry.stable_transfers, 128, "{interval} us");
        assert_eq!(
            geometry.stable_transfers as u64 * geometry.transfer_us(),
            256_000,
            "{interval} us"
        );
    }
}

#[test]
fn slower_service_intervals_still_carry_at_least_one_packet() {
    // A transfer is never empty, however long the interval.
    for (interval, packets) in [(2_000u32, 1usize), (4_000, 1), (8_000, 1)] {
        let geometry = default_geometry(interval);
        assert_eq!(geometry.packets_per_transfer, packets, "{interval} us");
        assert_eq!(geometry.transfer_slots, 8);
        assert!(geometry.stable_transfers >= 1);
    }
    // A zero interval from a bad descriptor does not divide by zero.
    assert!(default_geometry(0).packets_per_transfer <= 32);
}

#[test]
fn a_transfer_rounds_to_the_nearest_whole_service_interval() {
    // 2000 us / 300 us = 6.67 -> 7; 2000 / 900 = 2.2 -> 2; 2000 / 3000 -> 1 (never 0).
    assert_eq!(default_geometry(300).packets_per_transfer, 7);
    assert_eq!(default_geometry(900).packets_per_transfer, 2);
    assert_eq!(default_geometry(3_000).packets_per_transfer, 1);
}

#[test]
fn the_configured_queue_sets_the_packets_and_a_transfer_stays_within_32() {
    // 8 ms transfers at 125 us would be 64 packets: capped at 32 per transfer.
    let deep = IsoQueueGeometry::for_stream(125, 32, 4);
    assert_eq!(deep.packets_per_transfer, 32);
    assert_eq!(deep.transfer_slots, 4);
    assert_eq!(
        deep.queued_us(),
        16_000,
        "the 32-packet cap wins over 32 ms"
    );

    let shallow = IsoQueueGeometry::for_stream(125, 4, 8);
    assert_eq!(shallow.packets_per_transfer, 4);
    assert_eq!(shallow.queued_us(), 4_000);

    let many = IsoQueueGeometry::for_stream(500, 16, 16);
    assert_eq!(many.packets_per_transfer, 2);
    assert_eq!(many.transfer_slots, 16);
    assert_eq!(many.queued_us(), 16_000);
}

#[test]
fn profiles_tune_queue_time_and_wakeups_independently() {
    use crate::{DacConfig, UsbQueueProfile};

    for interval in [125, 250, 500, 1_000] {
        let balanced = IsoQueueGeometry::for_config(interval, &DacConfig::default());
        assert_eq!(balanced, default_geometry(interval));

        let low_cpu = DacConfig::default().with_usb_queue_profile(UsbQueueProfile::LowCpu);
        let low_cpu = IsoQueueGeometry::for_config(interval, &low_cpu);
        assert_eq!(low_cpu.queued_us(), balanced.queued_us());
        assert_eq!(low_cpu.transfer_slots, 4);
        assert_eq!(low_cpu.transfer_us(), balanced.transfer_us() * 2);

        let low_latency = DacConfig::default().with_usb_queue_profile(UsbQueueProfile::LowLatency);
        let low_latency = IsoQueueGeometry::for_config(interval, &low_latency);
        assert_eq!(low_latency.transfer_us(), 1_000);
        assert_eq!(low_latency.queued_us(), 8_000);
    }
}

#[test]
fn application_period_rounds_at_the_endpoint_and_derives_the_slot_count() {
    use crate::DacConfig;

    let config = DacConfig {
        bus_queue_ms: 16,
        transfers_in_flight: 12,
        transfer_period_us: Some(3_200),
        ..DacConfig::default()
    };
    let geometry = IsoQueueGeometry::for_config(250, &config);
    assert_eq!(geometry.packets_per_transfer, 12);
    assert_eq!(geometry.transfer_slots, 5);
    assert_eq!(geometry.queued_us(), 15_000);

    let slow = IsoQueueGeometry::for_config(32_000, &config);
    assert_eq!((slow.packets_per_transfer, slow.transfer_slots), (1, 2));

    let capped = IsoQueueGeometry::for_config(
        125,
        &DacConfig {
            bus_queue_ms: 64,
            transfers_in_flight: 16,
            transfer_period_us: Some(32_000),
            ..config
        },
    );
    assert_eq!(
        (capped.packets_per_transfer, capped.transfer_slots),
        (32, 7)
    );
    assert!(capped.packets_in_flight() < 256);
}

#[test]
fn packets_in_flight_never_reach_256_whatever_the_configuration() {
    // The hard cap wins over bus_queue_ms: 64 ms in 8 transfers at 125 us is
    // 32 x 8 = 256 packets, which stalled on the measured phone and DAC.
    let capped = IsoQueueGeometry::for_stream(125, 64, 8);
    assert_eq!(capped.packets_per_transfer, 31);
    assert_eq!(capped.packets_in_flight(), 248);

    for interval in [1u32, 62, 125, 250, 500, 1_000, 2_000, 8_000] {
        for bus_queue_ms in 4..=64u32 {
            for transfers in 2..=16u32 {
                let geometry = IsoQueueGeometry::for_stream(interval, bus_queue_ms, transfers);
                assert!(
                    geometry.packets_in_flight() <= 255,
                    "{interval} us, {bus_queue_ms} ms, {transfers} transfers: {} packets in flight",
                    geometry.packets_in_flight()
                );
                assert!(geometry.packets_per_transfer >= 1);
                assert!(geometry.packets_per_transfer <= 32);
                assert_eq!(geometry.transfer_slots, transfers as usize);
                assert!(geometry.stable_transfers >= 1);
            }
        }
    }
}

#[test]
fn the_stable_window_is_about_256_ms_of_completed_transfers() {
    for interval in [125u32, 250, 500, 1_000] {
        for (bus_queue_ms, transfers) in [(4u32, 8u32), (16, 8), (32, 8), (16, 4), (48, 16)] {
            let geometry = IsoQueueGeometry::for_stream(interval, bus_queue_ms, transfers);
            let window_us = geometry.stable_transfers as u64 * geometry.transfer_us();
            assert!(
                (256_000..256_000 + geometry.transfer_us()).contains(&window_us),
                "{interval} us {bus_queue_ms} ms x {transfers}: {window_us} us"
            );
        }
    }
}

#[test]
fn consecutive_loss_is_bounded_in_time_as_well_as_in_transfers() {
    // The default geometry keeps the three-transfer bound the transport had.
    assert_eq!(default_geometry(125).max_consecutive_lossy_urbs(), 3);
    // Small transfers need more of them for the same stretch of audio...
    assert_eq!(
        IsoQueueGeometry::for_stream(125, 4, 8).max_consecutive_lossy_urbs(),
        12
    );
    // ...and long ones never go below three.
    assert_eq!(
        IsoQueueGeometry::for_stream(125, 64, 4).max_consecutive_lossy_urbs(),
        3
    );
}

/// What a transfer carries for each encoding the transport runs, as the output
/// loop would build it: (name, sample rate, bytes per wire frame).
const ENCODINGS: [(&str, u32, usize); 3] = [
    // 44.1 kHz stereo 16-bit PCM in a 2-byte subslot.
    ("pcm", 44_100, 4),
    // DSD64 over PCM: 176.4 kHz carrier, 24 bits in a 3-byte subslot, stereo.
    ("dop", 176_400, 6),
    // Native DSD64 in 4-byte words: 2.8224 MHz / 32 = 88.2 kHz, stereo.
    ("dsd_native", 88_200, 8),
];

#[test]
fn pcm_dop_and_native_dsd_get_the_same_queue_at_every_interval() {
    for interval in [125u32, 250, 500, 1_000] {
        let expected = default_geometry(interval);
        for (name, rate, bytes_per_frame) in ENCODINGS {
            // The endpoint's maximum packet: the most one interval can carry.
            let max_packet = (u64::from(rate) * u64::from(interval)).div_ceil(1_000_000) as usize
                * bytes_per_frame;
            // The geometry has no encoding input: the output loop derives it from
            // the interval and the configuration alone.
            let geometry = IsoQueueGeometry::for_stream(interval, 16, 8);
            assert_eq!(geometry, expected, "{name} at {interval} us");

            let mut scheduler =
                IsoPacketScheduler::with_geometry(rate, bytes_per_frame, interval, geometry);
            let mut in_flight_packets = 0;
            let mut queued_us = 0;
            for _ in 0..geometry.transfer_slots {
                let packets = scheduler.next_transfer_packet_bytes();
                assert_eq!(packets.len(), geometry.packets_per_transfer);
                assert!(
                    packets.iter().all(|bytes| *bytes <= max_packet),
                    "{name} at {interval} us: a packet exceeds the endpoint maximum"
                );
                // The transfer buffer is sized packets x max packet.
                let capacity = geometry.packets_per_transfer * max_packet;
                assert!(packets.iter().sum::<usize>() <= capacity);
                in_flight_packets += packets.len();
                queued_us += packets.len() as u64 * u64::from(interval);
            }
            assert!(
                in_flight_packets < 256,
                "{name} at {interval} us: {in_flight_packets}"
            );
            assert_eq!(in_flight_packets, geometry.packets_in_flight());
            assert_eq!(queued_us, 16_000, "{name} at {interval} us");
        }
    }
}

#[test]
fn regrouping_a_queue_preserves_packet_sizes_in_order() {
    // The packets the DAC sees depend on the rate and the interval, never on how
    // they are grouped into transfers.
    let current = default_geometry(125);
    let regrouped = [
        IsoQueueGeometry::for_stream(125, 8, 8),
        IsoQueueGeometry::for_stream(125, 32, 4),
        IsoQueueGeometry::for_stream(125, 16, 2),
    ];
    for rate in [44_100, 96_000, 384_000] {
        let schedule = |geometry: IsoQueueGeometry| {
            let mut scheduler = IsoPacketScheduler::with_geometry(rate, 6, 125, geometry);
            (0..(8_000 / geometry.packets_per_transfer))
                .flat_map(|_| scheduler.next_transfer_packet_bytes())
                .take(8_000)
                .collect::<Vec<_>>()
        };
        for other in regrouped {
            assert_eq!(schedule(current), schedule(other), "{rate} Hz vs {other:?}");
        }
    }
}

fn transfer_frames(packet_bytes: &[usize], bytes_per_frame: usize) -> Vec<usize> {
    packet_bytes
        .iter()
        .map(|packet| packet / bytes_per_frame)
        .collect()
}

#[test]
fn test_scheduler_44100_hz_microframes_alternate_five_and_six_frames() {
    let bytes_per_frame = 4usize;
    let mut scheduler = IsoPacketScheduler::new(44_100, bytes_per_frame, 125);
    let frames = transfer_frames(&scheduler.next_transfer_packet_bytes(), bytes_per_frame);

    assert_eq!(frames.len(), scheduler.packets_per_transfer());
    assert_eq!(&frames[..8], &[5, 6, 5, 6, 5, 6, 5, 6]);
}

#[test]
fn test_scheduler_44100_hz_microframes_accumulate_exact_second() {
    let bytes_per_frame = 4usize;
    let mut scheduler = IsoPacketScheduler::new(44_100, bytes_per_frame, 125);
    let total_frames: usize = (0..(8_000 / scheduler.packets_per_transfer()))
        .flat_map(|_| scheduler.next_transfer_packet_bytes())
        .map(|packet| packet / bytes_per_frame)
        .sum();

    assert_eq!(total_frames, 44_100);
}

#[test]
fn test_scheduler_48000_hz_microframes_accumulate_exact_second() {
    let bytes_per_frame = 4usize;
    let mut scheduler = IsoPacketScheduler::new(48_000, bytes_per_frame, 125);
    let total_frames: usize = (0..(8_000 / scheduler.packets_per_transfer()))
        .flat_map(|_| scheduler.next_transfer_packet_bytes())
        .map(|packet| packet / bytes_per_frame)
        .sum();

    assert_eq!(total_frames, 48_000);
}

#[test]
fn test_scheduler_96000_hz_microframes_accumulate_exact_second() {
    let bytes_per_frame = 6usize;
    let mut scheduler = IsoPacketScheduler::new(96_000, bytes_per_frame, 125);
    let total_frames: usize = (0..(8_000 / scheduler.packets_per_transfer()))
        .flat_map(|_| scheduler.next_transfer_packet_bytes())
        .map(|packet| packet / bytes_per_frame)
        .sum();

    assert_eq!(total_frames, 96_000);
}

#[test]
fn test_scheduler_96000_hz_microframes_use_twelve_frames_per_packet() {
    let bytes_per_frame = 6usize;
    let mut scheduler = IsoPacketScheduler::new(96_000, bytes_per_frame, 125);
    let frames = transfer_frames(&scheduler.next_transfer_packet_bytes(), bytes_per_frame);

    assert_eq!(frames.len(), scheduler.packets_per_transfer());
    assert!(frames.iter().all(|frames| *frames == 12));
}

#[test]
fn test_scheduler_88200_hz_microframes_accumulate_exact_second() {
    let bytes_per_frame = 6usize;
    let mut scheduler = IsoPacketScheduler::new(88_200, bytes_per_frame, 125);
    let total_frames: usize = (0..(8_000 / scheduler.packets_per_transfer()))
        .flat_map(|_| scheduler.next_transfer_packet_bytes())
        .map(|packet| packet / bytes_per_frame)
        .sum();

    assert_eq!(total_frames, 88_200);
}

#[test]
fn test_scheduler_176400_hz_microframes_accumulate_exact_second() {
    let bytes_per_frame = 6usize;
    let mut scheduler = IsoPacketScheduler::new(176_400, bytes_per_frame, 125);
    let total_frames: usize = (0..(8_000 / scheduler.packets_per_transfer()))
        .flat_map(|_| scheduler.next_transfer_packet_bytes())
        .map(|packet| packet / bytes_per_frame)
        .sum();

    assert_eq!(total_frames, 176_400);
}

#[test]
fn test_scheduler_192000_hz_microframes_accumulate_exact_second() {
    let bytes_per_frame = 6usize;
    let mut scheduler = IsoPacketScheduler::new(192_000, bytes_per_frame, 125);
    let total_frames: usize = (0..(8_000 / scheduler.packets_per_transfer()))
        .flat_map(|_| scheduler.next_transfer_packet_bytes())
        .map(|packet| packet / bytes_per_frame)
        .sum();

    assert_eq!(total_frames, 192_000);
}

#[test]
fn test_scheduler_96000_hz_full_speed_frames_accumulate_exact_second() {
    let bytes_per_frame = 6usize;
    let mut scheduler = IsoPacketScheduler::new(96_000, bytes_per_frame, 1_000);
    let mut total_frames = 0usize;
    let mut packet_count = 0usize;
    while packet_count < 1_000 {
        for packet in scheduler.next_transfer_packet_bytes() {
            total_frames += packet / bytes_per_frame;
            packet_count += 1;
            if packet_count == 1_000 {
                break;
            }
        }
    }

    assert_eq!(total_frames, 96_000);
}

#[test]
fn test_scheduler_rejects_implausible_feedback() {
    let bytes_per_frame = 6usize;
    let mut scheduler = IsoPacketScheduler::new(96_000, bytes_per_frame, 125);
    assert!(!scheduler.update_feedback_frames_per_packet(10_000.0));

    let frames = transfer_frames(&scheduler.next_transfer_packet_bytes(), bytes_per_frame);
    assert!(frames.iter().all(|frames| *frames == 12));
}

#[test]
fn test_scheduler_352800_hz_dop_carrier_accumulates_exact_second() {
    let bytes_per_frame = 6usize;
    let mut scheduler =
        IsoPacketScheduler::with_geometry(352_800, bytes_per_frame, 125, default_geometry(125));
    let mut frames = Vec::new();
    for _ in 0..(8_000 / scheduler.packets_per_transfer()) {
        frames.extend(transfer_frames(
            &scheduler.next_transfer_packet_bytes(),
            bytes_per_frame,
        ));
    }

    assert_eq!(frames.len(), 8_000);
    assert_eq!(frames.iter().sum::<usize>(), 352_800);
    assert!(frames.iter().all(|frames| *frames == 44 || *frames == 45));
    assert_eq!(frames.iter().filter(|frames| **frames == 45).count(), 800);
}

#[test]
fn test_scheduler_feedback_tracks_small_dac_clock_skew() {
    let bytes_per_frame = 6usize;
    let mut scheduler = IsoPacketScheduler::new(352_800, bytes_per_frame, 125);
    let nominal = 352_800.0 * 125.0 / 1_000_000.0;
    let skewed = nominal * 1.000_2;
    assert!(scheduler.update_feedback_frames_per_packet(skewed));

    let total_frames: usize = (0..8_000)
        .map(|_| {
            scheduler
                .next_transfer_packet_bytes()
                .first()
                .copied()
                .unwrap()
                / bytes_per_frame
        })
        .sum();
    let expected = (skewed * 8_000.0) as usize;
    assert!(total_frames.abs_diff(expected) <= 2);
    assert!(total_frames > 352_800);
}

#[test]
fn test_scheduler_feedback_rejects_and_recovers_from_outlier() {
    let bytes_per_frame = 6usize;
    let mut scheduler = IsoPacketScheduler::new(352_800, bytes_per_frame, 125);
    assert!(!scheduler.update_feedback_frames_per_packet(0.0));
    assert!(!scheduler.update_feedback_frames_per_packet(f64::NAN));
    assert!(!scheduler.update_feedback_frames_per_packet(44.1 * 1.501));
    assert!(scheduler.update_feedback_frames_per_packet(44.1 * 1.000_1));

    let total_frames: usize = (0..8_000)
        .map(|_| {
            scheduler
                .next_transfer_packet_bytes()
                .first()
                .copied()
                .unwrap()
                / bytes_per_frame
        })
        .sum();
    assert!(total_frames > 352_800);
}

#[test]
fn explicit_feedback_validation_preserves_phase_and_updates_ewma_once() {
    let mut scheduler = IsoPacketScheduler::new(48_000, 8, 1000);
    assert!(scheduler.update_feedback_frames_per_packet(48.25));
    let first = scheduler.next_transfer_packet_bytes();
    let first_frames: usize = first.iter().map(|bytes| bytes / 8).sum();
    // Candidate scans validate without mutating the current rate or phase.
    for _ in 0..10 {
        assert!(scheduler.feedback_rate_is_plausible(48.75));
    }
    assert!(scheduler.update_feedback_frames_per_packet(48.75)); // 48.25 * .8 + 48.75 * .2 = 48.35
    let mut total = first_frames;
    let mut remaining_packets = 0usize;
    for _ in 0..100 {
        let packets = scheduler.next_transfer_packet_bytes();
        remaining_packets += packets.len();
        total += packets.iter().map(|bytes| bytes / 8).sum::<usize>();
    }
    let integrated = first.len() as f64 * 48.25 + remaining_packets as f64 * 48.35;
    assert!((total as f64 - integrated).abs() < 1.0);
}

#[test]
fn feedback_recentering_window_is_bounded_and_ewma_retained() {
    let mut scheduler = IsoPacketScheduler::new(48_000, 4, 125);
    assert!(scheduler.update_feedback_frames_per_packet(6.0));
    for factor in [0.875, 1.5, 1.10, 0.90] {
        assert!(scheduler.update_feedback_frames_per_packet(6.0 * factor));
    }
    for factor in [0.874, 1.501, 0.0, f64::INFINITY, f64::NAN] {
        assert!(!scheduler.update_feedback_frames_per_packet(6.0 * factor));
    }
}
