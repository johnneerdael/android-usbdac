//! What the DAC has actually played, as opposed to what the decoder has produced.
//!
//! On the direct-USB path the decoder runs ahead of the speakers by everything
//! sitting between them: the PCM ring and the isochronous transfers already
//! submitted to the bus. The decoder-side position therefore reads ahead of
//! what is audible by that much, and the lead grows with the buffer.
//!
//! This module is the accounting, kept apart from libusb so it can be tested on
//! the host. Frames here are ring frames (one sample per channel; for native DSD
//! one byte per channel), the unit `consumer_frames` and the ring itself use.
//!
//! "Music" frames are frames taken from the ring. Wire silence sent while the
//! stream is held (pause, or the wait for a flushed ring to refill) and padding
//! added when the ring ran dry are not music, and neither are frames the DAC did
//! not take (`iso_transport_loss`).

/// Music frames of one completed transfer that the DAC actually took.
///
/// `frames` is everything queued in the transfer, `music_frames` the part of it
/// that came from the ring, and `lost_frames` what the DAC did not take. Padding
/// sits at the end of a transfer, and loss does too (a timed-out URB cancels its
/// tail, a short packet loses the end of the packet), so lost frames are charged
/// to the padding first and only the remainder to music.
pub(crate) fn music_frames_delivered(frames: u64, music_frames: u64, lost_frames: u64) -> u64 {
    let music_frames = music_frames.min(frames);
    let padding = frames - music_frames;
    music_frames.saturating_sub(lost_frames.saturating_sub(padding))
}

/// Running count of music frames the DAC has taken since the stream started or
/// the ring was last flushed.
///
/// Owned by the output thread. A flush is noticed through the epoch the ring
/// carries: a transfer filled before the flush still completes afterwards, but
/// it holds audio from before the seek and is not counted in the new epoch.
#[derive(Debug, Default)]
pub(crate) struct PlayedMusicCounter {
    epoch: u64,
    frames: u64,
}

impl PlayedMusicCounter {
    /// Fold in one completed transfer and return the running total.
    ///
    /// `current_epoch` is the ring's flush epoch now; `payload_epoch` is the
    /// epoch the transfer was filled in. `delivered` comes from
    /// [`music_frames_delivered`].
    pub(crate) fn complete(
        &mut self,
        current_epoch: u64,
        payload_epoch: u64,
        delivered: u64,
    ) -> u64 {
        if current_epoch != self.epoch {
            self.epoch = current_epoch;
            self.frames = 0;
        }
        if payload_epoch == current_epoch {
            self.frames = self.frames.saturating_add(delivered);
        }
        self.frames
    }
}

/// Music frames submitted to the bus and not yet completed, for transfers filled
/// in `current_epoch`. Each item is `(epoch, music_frames)` of one in-flight
/// transfer.
pub(crate) fn inflight_music_frames(
    current_epoch: u64,
    in_flight: impl IntoIterator<Item = (u64, u64)>,
) -> u64 {
    in_flight
        .into_iter()
        .filter(|(epoch, _)| *epoch == current_epoch)
        .fold(0u64, |total, (_, music)| total.saturating_add(music))
}

/// Frames between the decoder and the DAC: music in the ring plus music
/// submitted and not yet completed.
pub(crate) fn output_latency_frames(ring_frames: u64, inflight_music_frames: u64) -> u64 {
    ring_frames.saturating_add(inflight_music_frames)
}
