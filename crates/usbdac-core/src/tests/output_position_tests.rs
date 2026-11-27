use crate::iso_packet_validation::{account_timed_out_transfer, PacketAcceptance};
use crate::iso_transport_loss::{LossLimits, LossTracker, UrbLoss};
use crate::output_position::{
    inflight_music_frames, music_frames_delivered, output_latency_frames, PlayedMusicCounter,
};
use libusb1_sys::{constants::LIBUSB_TRANSFER_COMPLETED, libusb_iso_packet_descriptor};

// ---- which frames of a transfer count as music that was played

#[test]
fn a_clean_music_transfer_counts_in_full() {
    assert_eq!(music_frames_delivered(32, 32, 0), 32);
}

#[test]
fn held_silence_is_not_music() {
    // A held transfer is filled with silence and takes nothing from the ring.
    assert_eq!(music_frames_delivered(32, 0, 0), 0);
    // Not even when part of it was lost: nothing was music to begin with.
    assert_eq!(music_frames_delivered(32, 0, 10), 0);
}

#[test]
fn underrun_padding_is_not_music() {
    // 20 frames came from the ring and 12 are padding.
    assert_eq!(music_frames_delivered(32, 20, 0), 20);
}

#[test]
fn lost_frames_come_off_the_padding_before_the_music() {
    // Padding and loss both sit at the end of a transfer, so a loss no larger
    // than the padding costs the music nothing.
    assert_eq!(music_frames_delivered(32, 20, 12), 20);
    assert_eq!(music_frames_delivered(32, 20, 5), 20);
    // Beyond the padding the music is lost too.
    assert_eq!(music_frames_delivered(32, 20, 15), 17);
    assert_eq!(music_frames_delivered(32, 32, 7), 25);
    assert_eq!(music_frames_delivered(32, 32, 32), 0);
}

#[test]
fn nonsense_inputs_never_overflow_or_exceed_the_transfer() {
    assert_eq!(
        music_frames_delivered(32, 99, 0),
        32,
        "music cannot exceed the transfer"
    );
    assert_eq!(music_frames_delivered(32, 32, 99), 0);
    assert_eq!(music_frames_delivered(0, 0, 0), 0);
    assert_eq!(music_frames_delivered(u64::MAX, u64::MAX, u64::MAX), 0);
}

// ---- the running count

#[test]
fn the_count_only_grows_between_flushes() {
    let mut played = PlayedMusicCounter::default();
    let mut last = 0;
    for delivered in [32, 32, 0, 0, 20, 32, 0, 32] {
        let total = played.complete(0, 0, delivered);
        assert!(total >= last, "{total} fell below {last}");
        last = total;
    }
    assert_eq!(last, 148);
}

#[test]
fn a_flush_starts_a_new_count() {
    let mut played = PlayedMusicCounter::default();
    assert_eq!(played.complete(0, 0, 32), 32);
    assert_eq!(played.complete(0, 0, 32), 64);

    // The ring is flushed (seek): epoch 1. Held silence completes first.
    assert_eq!(played.complete(1, 1, 0), 0, "reset on flush");
    assert_eq!(played.complete(1, 1, 32), 32);
}

#[test]
fn audio_queued_before_a_flush_does_not_count_after_it() {
    let mut played = PlayedMusicCounter::default();
    assert_eq!(played.complete(0, 0, 32), 32);
    // Seek. Two transfers filled before the flush are still on the bus and
    // complete in the new epoch; they hold audio from before the seek.
    assert_eq!(played.complete(1, 0, 32), 0);
    assert_eq!(played.complete(1, 0, 32), 0);
    // The first transfer filled after the flush does count.
    assert_eq!(played.complete(1, 1, 32), 32);
}

#[test]
fn the_count_starts_at_zero_with_each_new_counter() {
    // A stream gets a new counter, so nothing carries over from the last one.
    let mut first = PlayedMusicCounter::default();
    first.complete(0, 0, 500);
    let mut second = PlayedMusicCounter::default();
    assert_eq!(second.complete(0, 0, 0), 0);
}

// ---- pause, refill and loss against the count

fn descriptors(sizes: &[usize], actual: &[usize]) -> Vec<libusb_iso_packet_descriptor> {
    sizes
        .iter()
        .zip(actual)
        .map(|(length, actual)| libusb_iso_packet_descriptor {
            length: *length as u32,
            actual_length: *actual as u32,
            status: LIBUSB_TRANSFER_COMPLETED,
        })
        .collect()
}

const SCHEDULE: [usize; 8] = [20, 24, 20, 24, 20, 24, 20, 24];
const SCHEDULE_FRAMES: u64 = 44;

/// One completed transfer as the output loop sees it: what was queued, how much
/// of it was music, which epoch, whether it was held, and what the transport
/// lost.
struct Completed {
    music_frames: u64,
    epoch: u64,
    held: bool,
    urb: UrbLoss,
}

impl Completed {
    fn music(music_frames: u64) -> Self {
        Self {
            music_frames,
            epoch: 0,
            held: false,
            urb: UrbLoss::default(),
        }
    }

    fn held() -> Self {
        Self {
            music_frames: 0,
            epoch: 0,
            held: true,
            urb: UrbLoss::default(),
        }
    }
}

/// The output loop's bookkeeping for one completed transfer.
fn complete(
    played: &mut PlayedMusicCounter,
    tracker: &mut LossTracker,
    current_epoch: u64,
    done: &Completed,
) -> u64 {
    let urb = done.urb.with_held(done.held);
    tracker.record(SCHEDULE_FRAMES, urb);
    played.complete(
        current_epoch,
        done.epoch,
        music_frames_delivered(SCHEDULE_FRAMES, done.music_frames, urb.lost_frames),
    )
}

fn tracker() -> LossTracker {
    LossTracker::new(LossLimits {
        max_consecutive_lossy_urbs: 1_000,
        max_lost_frames: 1_000_000,
    })
}

#[test]
fn pause_and_refill_holds_add_nothing_and_playing_resumes_the_count() {
    let mut played = PlayedMusicCounter::default();
    let mut tracker = tracker();

    // Playing, then a pause (held), then a flush with its refill wait (held,
    // new epoch), then playing again.
    let mut totals = Vec::new();
    for (epoch, done) in [
        (0, Completed::music(SCHEDULE_FRAMES)),
        (0, Completed::music(SCHEDULE_FRAMES)),
        (0, Completed::held()),
        (0, Completed::held()),
        (0, Completed::music(SCHEDULE_FRAMES)),
        (1, Completed::held()),
        (1, Completed::held()),
        (
            1,
            Completed {
                epoch: 1,
                ..Completed::music(SCHEDULE_FRAMES)
            },
        ),
    ] {
        totals.push(complete(&mut played, &mut tracker, epoch, &done));
    }

    assert_eq!(
        totals,
        [44, 88, 88, 88, 132, 0, 0, 44],
        "held transfers hold the count; a flush restarts it"
    );
    assert!(!tracker.is_latched(), "holds are not lost audio");
}

#[test]
fn a_timed_out_transfer_counts_only_what_went_out_and_its_resubmission_counts_once() {
    let mut played = PlayedMusicCounter::default();
    let mut tracker = tracker();

    // Packets 0 and 1 went out whole, the rest of the URB timed out.
    let timed_out_descriptors = descriptors(&SCHEDULE, &[20, 24, 0, 0, 0, 0, 0, 0]);
    let stats = account_timed_out_transfer(
        &SCHEDULE,
        &timed_out_descriptors,
        PacketAcceptance::new(44_100, 125, 4),
    );
    let timed_out = Completed {
        urb: UrbLoss::from_completion(&stats, true, 4),
        ..Completed::music(SCHEDULE_FRAMES)
    };
    assert_eq!(timed_out.urb.lost_frames, SCHEDULE_FRAMES - 11);

    let after_clean = complete(
        &mut played,
        &mut tracker,
        0,
        &Completed::music(SCHEDULE_FRAMES),
    );
    assert_eq!(after_clean, 44);
    let after_timeout = complete(&mut played, &mut tracker, 0, &timed_out);
    assert_eq!(after_timeout, 44 + 11, "only the delivered packets count");

    // The slot is resubmitted with the next audio from the ring. Its frames are
    // new frames, so the lost URB's audio is not counted a second time.
    let after_resubmit = complete(
        &mut played,
        &mut tracker,
        0,
        &Completed::music(SCHEDULE_FRAMES),
    );
    assert_eq!(after_resubmit, 44 + 11 + 44);
    assert_eq!(tracker.lost_frames(), SCHEDULE_FRAMES - 11);
}

#[test]
fn a_timed_out_hold_adds_nothing_and_loses_no_music() {
    let mut played = PlayedMusicCounter::default();
    let mut tracker = tracker();
    complete(
        &mut played,
        &mut tracker,
        0,
        &Completed::music(SCHEDULE_FRAMES),
    );

    let held_timeout = Completed {
        urb: UrbLoss {
            timed_out: true,
            lost_frames: SCHEDULE_FRAMES,
            ..UrbLoss::default()
        },
        ..Completed::held()
    };
    assert_eq!(complete(&mut played, &mut tracker, 0, &held_timeout), 44);
    assert_eq!(tracker.lost_frames(), 0);
}

#[test]
fn padding_and_loss_together_are_charged_padding_first() {
    let mut played = PlayedMusicCounter::default();
    let mut tracker = tracker();
    // 30 of 44 frames were music; the DAC took 36 of 44, so 8 were lost. The 14
    // frames of padding absorb all 8.
    let done = Completed {
        urb: UrbLoss {
            lost_frames: 8,
            ..UrbLoss::default()
        },
        ..Completed::music(30)
    };
    assert_eq!(complete(&mut played, &mut tracker, 0, &done), 30);
}

// ---- the latency query

#[test]
fn only_music_of_the_current_epoch_counts_as_in_flight() {
    // Three transfers on the bus: two music (one from before the flush), one
    // held silence.
    let in_flight = [(1, 32), (0, 32), (1, 0)];
    assert_eq!(inflight_music_frames(1, in_flight), 32);
    assert_eq!(inflight_music_frames(0, in_flight), 32);
    assert_eq!(inflight_music_frames(2, in_flight), 0);
    assert_eq!(inflight_music_frames(1, []), 0);
}

#[test]
fn latency_is_the_ring_plus_what_is_on_the_bus() {
    assert_eq!(output_latency_frames(11_025, 256), 11_281);
    assert_eq!(output_latency_frames(0, 0), 0);
    assert_eq!(output_latency_frames(u64::MAX, 1), u64::MAX);
}
