//! How deep the host is asked to keep the ring ahead of the USB output.
//!
//! A deep ring absorbs an upstream stall (slow storage, a network source,
//! scheduling) that a shallow one would let through as a dropout. It also delays
//! everything that is applied to the audio *as it is written*: software gain, and
//! whatever processing the host itself does before `write`, is heard only after
//! the audio already queued, which is the ring's fill. A change to audio that is
//! already in the ring is impossible, so the depth is a trade.
//!
//! Bit-perfect passthrough has no live processing to delay, so it keeps the deep
//! ring. While software gain is applied the recommended depth is the shallow
//! `processing_buffer_target_ms`.
//!
//! The depth is re-evaluated on every `writable_bytes()` call, because the mode
//! can change during playback. Nothing is flushed when it does: a ring that is
//! now deeper than its target is simply not refilled until the output has drained
//! it below the target, and one that is shallower than its target is filled on.
//! Either way the audio is continuous. This module is the decision, kept apart
//! from libusb so it can be tested on the host. Sizes are ring samples (frames
//! times channels).

/// The two depths the host is asked to keep the ring at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RingTargets {
    /// Bit-perfect passthrough: nothing is applied live.
    deep_samples: usize,
    /// Something is applied live; never above the deep target.
    live_samples: usize,
}

impl RingTargets {
    pub(crate) fn new(deep_samples: usize, live_samples: usize) -> Self {
        Self {
            deep_samples,
            live_samples: live_samples.min(deep_samples),
        }
    }

    /// The fill the host is asked to keep the ring at right now.
    pub(crate) fn select(self, live_processing: bool) -> usize {
        if live_processing {
            self.live_samples
        } else {
            self.deep_samples
        }
    }
}

/// Whether anything the listener can change live is applied to the audio as it
/// is written.
///
/// Raw DSD and DoP are never processed (software gain is refused for them), so a
/// bitstream is never live. For PCM it is live when the software gain is anything
/// but exactly 1.0, which the output thread then multiplies in.
pub(crate) fn live_processing(raw_bitstream: bool, software_gain: f32) -> bool {
    !raw_bitstream && software_gain != 1.0
}

/// Whether the shallow depth applies: software gain is applied to PCM, or the
/// host says it processes the audio itself before writing it (EQ, DSP,
/// crossfade). A bitstream is never live whatever the host says.
pub(crate) fn live_depth(raw_bitstream: bool, software_gain: f32, host_processing: bool) -> bool {
    live_processing(raw_bitstream, software_gain) || (!raw_bitstream && host_processing)
}

/// Whether the host should write more into a ring holding `buffered_samples`
/// while it is kept at `target_samples`.
pub(crate) fn needs_refill(buffered_samples: usize, target_samples: usize) -> bool {
    buffered_samples < target_samples
}
