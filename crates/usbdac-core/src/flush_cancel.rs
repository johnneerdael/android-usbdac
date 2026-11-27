//! Cancelling what is already on the bus when the stream is flushed.
//!
//! A flush empties the ring at once, but the transfers already submitted would
//! still play: up to `DacConfig::bus_queue_ms` of the old position after a seek
//! or a track change. With `cancel_in_flight_on_flush` the output thread
//! cancels them, waits until libusb has handed every one back (their buffers
//! stay alive until then), and resumes with wire silence until the ring has
//! refilled.
//!
//! This module is the decisions, apart from libusb so the host tests can run the
//! same ones the output loop does: which transfers to cancel, what a returned
//! transfer means, and the bounded wait.

use libusb1_sys::constants::LIBUSB_TRANSFER_CANCELLED;
use std::time::{Duration, Instant};

/// How long the output thread waits for libusb to hand back cancelled
/// transfers. A cancellation is acknowledged within a few milliseconds when the
/// device is healthy; past this the transfer is considered stuck and the stream
/// fails with a clear error (the teardown drain then tries once more).
pub(crate) const FLUSH_CANCEL_TIMEOUT: Duration = Duration::from_millis(1_000);

/// The slots to cancel after a flush, as indexes into the slot list. Each item
/// is `(in_flight, epoch the transfer was filled in, music frames it carries)`.
///
/// Nothing is cancelled unless some transfer on the bus carries music of an
/// earlier epoch: a flush while paused, while the stream is still priming, or
/// while only held silence is queued has nothing to cut, and leaving the bus
/// alone keeps the DAC locked on silence. When there is music to cut, every
/// transfer of an earlier epoch goes with it (the silence queued behind it
/// would otherwise play after a hole); transfers filled after the flush are
/// never touched.
pub(crate) fn flush_cancel_targets(
    current_epoch: u64,
    slots: impl IntoIterator<Item = (bool, u64, usize)>,
) -> Vec<usize> {
    let stale: Vec<(usize, usize)> = slots
        .into_iter()
        .enumerate()
        .filter(|(_, (in_flight, epoch, _))| *in_flight && *epoch != current_epoch)
        .map(|(index, (_, _, music_frames))| (index, music_frames))
        .collect();
    if stale.iter().any(|(_, music_frames)| *music_frames > 0) {
        stale.into_iter().map(|(index, _)| index).collect()
    } else {
        Vec::new()
    }
}

/// What a transfer that libusb handed back stands for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Completion {
    /// An ordinary completion: loss, played-frame and stability accounting
    /// apply. This includes a transfer that finished before its cancellation
    /// reached the controller (it was flagged, but it really played).
    Played,
    /// Cancelled because the stream was flushed. Not lost audio, not an
    /// underrun, never counted as played: the slot is just refilled.
    CancelledByFlush,
    /// Cancelled and nobody asked: a failure, as it always was.
    UnexpectedCancel,
}

pub(crate) fn classify_completion(status: i32, cancelled_for_flush: bool) -> Completion {
    match (status == LIBUSB_TRANSFER_CANCELLED, cancelled_for_flush) {
        (true, true) => Completion::CancelledByFlush,
        (true, false) => Completion::UnexpectedCancel,
        (false, _) => Completion::Played,
    }
}

/// Runs `pump` until `pending` reports no transfer still waiting for its
/// cancellation to be acknowledged, or `timeout` passes. `pending` counts what
/// is outstanding; `pump` lets libusb deliver completions and fails if it
/// cannot. The error names how many transfers never came back.
pub(crate) fn wait_for_acknowledgements(
    mut pending: impl FnMut() -> usize,
    mut pump: impl FnMut() -> Result<(), String>,
    timeout: Duration,
) -> Result<(), String> {
    let started = Instant::now();
    loop {
        let outstanding = pending();
        if outstanding == 0 {
            return Ok(());
        }
        if started.elapsed() >= timeout {
            return Err(format!(
                "flush: {outstanding} isochronous transfer(s) did not come back within {} ms of being cancelled",
                timeout.as_millis()
            ));
        }
        pump()?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use libusb1_sys::constants::{
        LIBUSB_TRANSFER_COMPLETED, LIBUSB_TRANSFER_ERROR, LIBUSB_TRANSFER_TIMED_OUT,
    };

    #[test]
    fn music_of_an_earlier_epoch_on_the_bus_is_cancelled_with_everything_before_the_flush() {
        // (in flight, epoch, music frames): old music, old silence, idle, new music.
        let slots = [(true, 3, 96), (true, 3, 0), (false, 3, 96), (true, 4, 96)];
        assert_eq!(flush_cancel_targets(4, slots), vec![0, 1]);
    }

    #[test]
    fn transfers_filled_after_the_flush_are_never_cancelled() {
        let slots = [(true, 4, 96), (true, 4, 0), (true, 4, 96)];
        assert!(flush_cancel_targets(4, slots).is_empty());
    }

    #[test]
    fn a_flush_while_only_silence_is_queued_leaves_the_bus_alone() {
        // Paused (or primed, or refilling): the bus holds held silence only.
        let silence = [(true, 3, 0); 8];
        assert!(flush_cancel_targets(4, silence).is_empty());
        // Nothing in flight at all, as during priming.
        assert!(flush_cancel_targets(4, [(false, 0, 0); 8]).is_empty());
        assert!(flush_cancel_targets(4, []).is_empty());
    }

    #[test]
    fn an_idle_slot_is_never_a_target_whatever_it_remembers() {
        assert!(flush_cancel_targets(4, [(false, 3, 96)]).is_empty());
    }

    #[test]
    fn a_cancelled_flush_transfer_is_neither_loss_nor_a_failure() {
        assert_eq!(
            classify_completion(LIBUSB_TRANSFER_CANCELLED, true),
            Completion::CancelledByFlush
        );
        // Nobody asked for the cancellation: still the failure it always was.
        assert_eq!(
            classify_completion(LIBUSB_TRANSFER_CANCELLED, false),
            Completion::UnexpectedCancel
        );
    }

    #[test]
    fn a_transfer_that_finished_before_the_cancel_arrived_is_accounted_normally() {
        // It was flagged, but it completed (or timed out, or failed): it played,
        // so the ordinary accounting applies and an error stays an error.
        for status in [
            LIBUSB_TRANSFER_COMPLETED,
            LIBUSB_TRANSFER_TIMED_OUT,
            LIBUSB_TRANSFER_ERROR,
        ] {
            assert_eq!(classify_completion(status, true), Completion::Played);
            assert_eq!(classify_completion(status, false), Completion::Played);
        }
    }

    #[test]
    fn the_wait_ends_as_soon_as_every_transfer_is_back() {
        let outstanding = std::cell::Cell::new(3);
        let pumps = std::cell::Cell::new(0);
        wait_for_acknowledgements(
            || outstanding.get(),
            || {
                pumps.set(pumps.get() + 1);
                outstanding.set(outstanding.get() - 1);
                Ok(())
            },
            Duration::from_secs(5),
        )
        .unwrap();
        assert_eq!(pumps.get(), 3);
        // Nothing to wait for: no pump at all.
        wait_for_acknowledgements(|| 0, || panic!("pumped"), Duration::from_secs(5)).unwrap();
    }

    #[test]
    fn a_transfer_that_never_returns_ends_the_wait_with_a_clear_error() {
        let started = Instant::now();
        let error = wait_for_acknowledgements(
            || 2,
            || {
                std::thread::sleep(Duration::from_millis(2));
                Ok(())
            },
            Duration::from_millis(30),
        )
        .unwrap_err();
        assert!(started.elapsed() < Duration::from_secs(2), "bounded");
        assert!(error.contains("2 isochronous transfer(s)"), "{error}");
        assert!(error.contains("30 ms"), "{error}");
        assert!(error.contains("flush"), "{error}");
    }

    #[test]
    fn a_failing_pump_is_reported_at_once() {
        let error = wait_for_acknowledgements(
            || 1,
            || Err("libusb_handle_events failed".into()),
            Duration::from_secs(5),
        )
        .unwrap_err();
        assert_eq!(error, "libusb_handle_events failed");
    }
}
