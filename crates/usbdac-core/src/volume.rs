//! Hardware volume: the DAC's Feature Unit.
//!
//! Volume belongs to the DAC, not to the sample path: the library never scales
//! samples for volume unless software gain is explicitly enabled, which ends
//! bit-perfect output. A [`VolumeControl`] reads and writes the Feature Unit's
//! volume and mute controls over the control pipe, so it works with or without
//! a stream while the device is held.

use crate::dac::DacShared;
use crate::error::{Error, Result};
use crate::events::DacEvent;
use crate::transport;
use std::sync::Arc;

/// UAC volume values are signed 1/256 dB units.
pub(crate) const RAW_PER_DB: f64 = 256.0;

/// The DAC's volume range in decibels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VolumeRange {
    pub min_db: f64,
    pub max_db: f64,
    /// The finest step the DAC accepts (the smallest resolution over its
    /// sub-ranges).
    pub res_db: f64,
}

/// The DAC's volume as read back from the device.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VolumeState {
    /// Level in dB.
    pub db: f64,
    /// Position on the 0..1 curve: 0 is the DAC's minimum; anything above 0
    /// spans `max(min, -80 dB)` to the maximum linearly in dB. A level below
    /// -80 dB reads as 0.0 here but keeps its exact `db`.
    pub fraction: f64,
    /// `None` when the DAC has no writable mute, or its readback failed.
    pub muted: Option<bool>,
}

/// What [`Dac::open`](crate::Dac::open) did with the DAC's level before any
/// stream could start (see [`StartupVolume`](crate::StartupVolume)).
///
/// The lists hold one entry per controlled channel, in channel order (a single
/// entry for a DAC with one master control).
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct StartupVolumeReport {
    /// The level each channel had before the library first touched it, in dB.
    /// What [`VolumeControl::hand_back`] restores.
    pub original_db: Vec<f64>,
    /// The level each channel has now that the start-up step is done, in dB.
    /// Equal to `original_db` when nothing needed lowering, and with
    /// `StartupVolume::Keep`.
    pub applied_db: Vec<f64>,
    /// The quietest original channel, in dB. A host that restores a level it
    /// remembered for this DAC should not go above it without the user's
    /// action: that was how loud the DAC itself was when it connected.
    pub limit_db: f64,
    /// `applied_db` is below `original_db` on some channel.
    pub attenuated: bool,
    /// The ceiling was below the DAC's minimum, so the channels went to the
    /// minimum instead.
    pub clamped_to_minimum: bool,
    /// The ceiling that was asked for; `None` for `StartupVolume::Keep`.
    pub max_db: Option<f64>,
}

/// Default number of steps for [`VolumeControl::step`].
pub const DEFAULT_VOLUME_STEPS: u8 = 16;

/// Handle to the DAC's hardware volume control.
///
/// Cheap to clone. Calls serialize with each other and with the library's own
/// volume I/O. Writes are verified by reading the level back; a failed
/// multi-channel write restores the channels already written.
#[derive(Clone)]
pub struct VolumeControl {
    shared: Arc<DacShared>,
}

impl std::fmt::Debug for VolumeControl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VolumeControl").finish_non_exhaustive()
    }
}

impl VolumeControl {
    pub(crate) fn new(shared: Arc<DacShared>) -> Self {
        Self { shared }
    }

    /// Create a bounded, ordered worker for control I/O. Keep one controller per
    /// device; requests never block the submitting or async executor thread.
    /// Dropping a request does not cancel a write already accepted by the worker.
    pub fn nonblocking(&self) -> Result<crate::AsyncVolumeControl> {
        self.ensure_open()?;
        crate::async_volume::AsyncVolumeControl::new(self.clone())
    }

    fn ensure_open(&self) -> Result<()> {
        self.shared.ensure_open()
    }

    /// The writable range, or [`Error::Control`] if the control disappeared
    /// (for example the active stream moved to another terminal).
    pub fn range(&self) -> Result<VolumeRange> {
        let _control = self.shared.control_guard.lock();
        let _scope = self.shared.enter()?;
        self.ensure_open()?;
        transport::volume_hw::hardware_volume_range()
            .ok_or_else(|| Error::Control("the DAC has no usable volume control".into()))
    }

    /// Reads the live level from the device.
    pub fn get(&self) -> Result<VolumeState> {
        let _control = self.shared.control_guard.lock();
        let _scope = self.shared.enter()?;
        self.ensure_open()?;
        let before = transport::volume_hw::cached_volume_state();
        let state = self.finish(transport::volume_hw::read_volume_state())?;
        if before != Some(state) {
            self.shared.events.emit_dac(DacEvent::VolumeChanged(state));
        }
        Ok(state)
    }

    /// Sets the level in dB (`NEG_INFINITY` selects the verified raw minimum).
    /// The value is clamped to the range and snapped to
    /// the nearest step the DAC supports (a tie goes to the quieter one). Returns
    /// the level read back.
    pub fn set_db(&self, db: f64) -> Result<VolumeState> {
        let _control = self.shared.control_guard.lock();
        let _scope = self.shared.enter()?;
        self.ensure_open()?;
        if db.is_nan() || db == f64::INFINITY {
            return Err(Error::InvalidArgument(
                "volume must be finite or negative infinity for silence".into(),
            ));
        }
        let state = self.finish(transport::volume_hw::write_volume_db(db))?;
        self.shared.events.emit_dac(DacEvent::VolumeChanged(state));
        Ok(state)
    }

    /// Sets the level on the 0..1 curve (see [`VolumeState::fraction`]). The
    /// exact inverse of the read-back mapping: feeding a read-back fraction
    /// back in reproduces the same step.
    pub fn set_fraction(&self, fraction: f64) -> Result<VolumeState> {
        let _control = self.shared.control_guard.lock();
        let _scope = self.shared.enter()?;
        self.ensure_open()?;
        if !fraction.is_finite() {
            return Err(Error::InvalidArgument("volume must be finite".into()));
        }
        let state = self.finish(transport::volume_hw::write_volume_fraction(fraction))?;
        self.shared.events.emit_dac(DacEvent::VolumeChanged(state));
        Ok(state)
    }

    /// Moves the level up (`direction > 0`) or down (`direction < 0`) by
    /// `1/steps` of the 0..1 curve, always by at least one DAC step.
    /// `steps == 0` selects [`DEFAULT_VOLUME_STEPS`]. A direction of 0 only
    /// reads the level.
    pub fn step(&self, direction: i8, steps: u8) -> Result<VolumeState> {
        let _control = self.shared.control_guard.lock();
        let _scope = self.shared.enter()?;
        self.ensure_open()?;
        let steps = if steps == 0 {
            DEFAULT_VOLUME_STEPS
        } else {
            steps
        };
        let state = self.finish(transport::volume_hw::step_volume(direction, steps))?;
        self.shared.events.emit_dac(DacEvent::VolumeChanged(state));
        Ok(state)
    }

    /// Sets the master mute. [`Error::Control`] if the DAC has no writable
    /// mute or the readback disagrees.
    pub fn set_mute(&self, muted: bool) -> Result<VolumeState> {
        let _control = self.shared.control_guard.lock();
        let _scope = self.shared.enter()?;
        self.ensure_open()?;
        let state = self.finish(transport::volume_hw::write_mute(muted))?;
        self.shared.events.emit_dac(DacEvent::VolumeChanged(state));
        Ok(state)
    }

    /// What the library did with the DAC's level when it was opened, before any
    /// stream could start: the level each channel had, the level it has now and
    /// the limit a host should not restore above. See [`StartupVolumeReport`].
    ///
    /// On connect a host reads this, then restores the level it remembers for
    /// this DAC capped by `limit_db`, or leaves the attenuated level.
    pub fn startup(&self) -> StartupVolumeReport {
        let _scope = self.shared.context.enter();
        transport::volume_hw::startup_report().unwrap_or_else(|| {
            // Not reachable through `Dac::volume`: a control is only installed
            // together with its start-up step.
            StartupVolumeReport {
                original_db: Vec::new(),
                applied_db: Vec::new(),
                limit_db: 0.0,
                attenuated: false,
                clamped_to_minimum: false,
                max_db: None,
            }
        })
    }

    /// The level the DAC had before the library first wrote to it, in dB, if
    /// it was recorded (all channels agreed).
    pub fn original_db(&self) -> Option<f64> {
        let _scope = self.shared.context.enter();
        transport::volume_hw::original_volume_db()
    }

    /// Writes the original level back now and ends the record. `Ok(false)` if
    /// there was nothing to restore or it was already in place.
    pub fn hand_back(&self) -> Result<bool> {
        let _control = self.shared.control_guard.lock();
        let _scope = self.shared.enter()?;
        self.ensure_open()?;
        transport::volume_hw::hand_back_original_volume().or_else_control()
    }

    fn finish(&self, result: std::result::Result<VolumeState, String>) -> Result<VolumeState> {
        match result {
            Ok(state) => Ok(state),
            Err(message) => {
                let error = if crate::error::is_device_gone(&message) {
                    Error::DeviceLost
                } else {
                    Error::Control(message)
                };
                if error == Error::DeviceLost {
                    self.shared.events.emit_dac(DacEvent::DeviceLost);
                }
                Err(error)
            }
        }
    }
}

trait OrElseControl<T> {
    fn or_else_control(self) -> Result<T>;
}

impl<T> OrElseControl<T> for std::result::Result<T, String> {
    fn or_else_control(self) -> Result<T> {
        crate::error::OrControl::or_control(self)
    }
}

/// dB for a raw UAC volume value.
pub(crate) fn raw_to_db(raw: i16) -> f64 {
    f64::from(raw) / RAW_PER_DB
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_units_are_256ths_of_a_db() {
        assert_eq!(raw_to_db(-256), -1.0);
        assert_eq!(raw_to_db(-12800), -50.0);
        assert_eq!(raw_to_db(i16::MIN), -128.0);
    }
}
