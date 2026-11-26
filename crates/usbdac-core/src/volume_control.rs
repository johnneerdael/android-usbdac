//! Protocol and arithmetic shared by USB volume discovery and host-side tests.
//!
//! Raw values are the UAC signed 1/256 dB units.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AudioControlProtocol {
    Uac1,
    Uac2,
}

impl AudioControlProtocol {
    pub(crate) fn current_request(self) -> u8 {
        match self {
            Self::Uac1 => 0x81,
            Self::Uac2 => 0x01,
        }
    }

    pub(crate) fn writable(self, controls: u32, selector: u8) -> bool {
        match self {
            Self::Uac1 => controls & (1 << (selector - 1)) != 0,
            // 00 absent, 01 read-only, 10 reserved, 11 read/write.
            Self::Uac2 => (controls >> (2 * (selector - 1))) & 3 == 3,
        }
    }

    pub(crate) fn volume_channels(self, controls: &[u32], channels: u16) -> Option<Vec<u16>> {
        if self.writable(*controls.first()?, 2) {
            return Some(vec![0]);
        }
        if channels == 0 || controls.len() <= channels as usize {
            return None;
        }
        (1..=channels).all(|channel| self.writable(controls[channel as usize], 2))
            .then(|| (1..=channels).collect())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RawRange {
    pub(crate) min: i16,
    pub(crate) max: i16,
    pub(crate) resolution: i16,
}

impl RawRange {
    pub(crate) fn validate(self) -> Result<Self, String> {
        if self.min > self.max || self.resolution < 0 {
            return Err("invalid signed USB volume range".to_string());
        }
        Ok(self)
    }

    /// Nearest supported value to `target` inside this range; a tie goes to the
    /// lower value.
    fn nearest_quieter_on_tie(self, target: f64) -> i16 {
        let target = target.clamp(self.min as f64, self.max as f64);
        let step = i32::from(self.resolution).max(1);
        let last_step = (i32::from(self.max) - i32::from(self.min)) / step;
        let index = (((target - self.min as f64) / step as f64) - 0.5)
            .ceil()
            .clamp(0.0, last_step as f64) as i32;
        (i32::from(self.min) + index * step) as i16
    }

    /// Nearest supported value to `target` that lies above `floor`, if this
    /// range has one. A `floor` at or below the range minimum does not restrict it.
    fn nearest_above(self, target: f64, floor: i16) -> Option<i16> {
        let target = target.clamp(self.min as f64, self.max as f64);
        let step = i32::from(self.resolution).max(1);
        // Round inside the supported lattice; MAX need not be step-aligned.
        let last_step = (i32::from(self.max) - i32::from(self.min)) / step;
        let first_step = if floor > self.min {
            (i32::from(floor) - i32::from(self.min)) / step + 1
        } else {
            0
        };
        if first_step > last_step {
            return None;
        }
        let index = (((target - self.min as f64) / step as f64).round() as i32)
            .clamp(first_step, last_step);
        Some((i32::from(self.min) + index * step) as i16)
    }
}

pub(crate) fn parse_uac2_volume_ranges(data: &[u8]) -> Result<Vec<RawRange>, String> {
    if data.len() < 2 {
        return Err("USB volume RANGE has no subrange count".to_string());
    }
    let count = u16::from_le_bytes([data[0], data[1]]) as usize;
    if count == 0 || count > 64 || data.len() != 2 + count * 6 {
        return Err(format!("invalid USB volume RANGE length/count: {}/{count}", data.len()));
    }
    data[2..].chunks_exact(6).map(|bytes| RawRange {
        min: i16::from_le_bytes([bytes[0], bytes[1]]),
        max: i16::from_le_bytes([bytes[2], bytes[3]]),
        resolution: i16::from_le_bytes([bytes[4], bytes[5]]),
    }.validate()).collect()
}

pub(crate) fn volume_bounds(ranges: &[RawRange]) -> (i16, i16) {
    (ranges.iter().map(|r| r.min).min().unwrap_or(0),
     ranges.iter().map(|r| r.max).max().unwrap_or(0))
}

/// Quietest level the 0..1 slider range reaches above silence: -80 dB in
/// UAC's 1/256 dB units. Many DACs report a minimum near -127 dB, so mapping the
/// slider over their whole range leaves much of its travel inaudible.
pub(crate) const VOLUME_FLOOR_RAW: i16 = -80 * 256;

/// Lower end of the slider's 0..1 span. Fraction 0 is still the device minimum
/// (silence); anything above it spans `volume_floor(..)` to the device maximum,
/// so a device whose minimum is above the floor (or that never rises above it)
/// keeps its linear full-range mapping.
fn volume_floor(min: i16, max: i16) -> i16 {
    if max > VOLUME_FLOOR_RAW { min.max(VOLUME_FLOOR_RAW) } else { min }
}

pub(crate) fn normalize_volume(raw: i16, ranges: &[RawRange]) -> f64 {
    let (min, max) = volume_bounds(ranges);
    let floor = volume_floor(min, max);
    let span = i32::from(max) - i32::from(floor);
    if span == 0 {
        return 0.0;
    }
    ((i32::from(raw) - i32::from(floor)) as f64 / span as f64).clamp(0.0, 1.0)
}

pub(crate) fn quantize_volume(volume: f64, ranges: &[RawRange]) -> i16 {
    let (min, max) = volume_bounds(ranges);
    let volume = volume.clamp(0.0, 1.0);
    if volume == 0.0 {
        return min;
    }
    let floor = volume_floor(min, max);
    let target = floor as f64 + volume * (i32::from(max) - i32::from(floor)) as f64;
    // In a gap, use the nearest supported value (quieter on a tie). A nonzero
    // fraction stays above the floor so normalize_volume() maps it back to
    // itself; only a range with nothing above it falls back to its minimum.
    let nearest = |above| ranges.iter().filter_map(|range| range.nearest_above(target, above))
        .min_by(|a, b| {
            ((*a as f64 - target).abs()).total_cmp(&(*b as f64 - target).abs()).then(a.cmp(b))
        });
    nearest(floor).or_else(|| nearest(min)).unwrap_or(min)
}

/// Nearest supported raw value to `db` decibels, clamped to the DAC's range
/// and snapped to its step lattice. A tie goes to the quieter value. Unlike
/// [`quantize_volume`] this has no floor: a level below -80 dB is reachable.
pub(crate) fn quantize_db(db: f64, ranges: &[RawRange]) -> i16 {
    let (min, max) = volume_bounds(ranges);
    let target = (db * 256.0).clamp(f64::from(min), f64::from(max));
    ranges
        .iter()
        .map(|range| range.nearest_quieter_on_tie(target))
        .min_by(|a, b| {
            ((*a as f64 - target).abs())
                .total_cmp(&(*b as f64 - target).abs())
                .then(a.cmp(b))
        })
        .unwrap_or(min)
}

/// The raw value one lattice step above (`direction > 0`) or below
/// (`direction < 0`) `raw`, staying inside the ranges; `raw` itself at the end
/// of the range.
pub(crate) fn adjacent_raw(raw: i16, direction: i8, ranges: &[RawRange]) -> i16 {
    let mut best: Option<i16> = None;
    for range in ranges {
        let step = i32::from(range.resolution).max(1);
        let candidate = if direction > 0 {
            // first lattice value strictly above raw
            let first = if i32::from(raw) < i32::from(range.min) {
                i32::from(range.min)
            } else {
                let k = (i32::from(raw) - i32::from(range.min)) / step + 1;
                i32::from(range.min) + k * step
            };
            (first <= i32::from(range.max)).then_some(first as i16)
        } else {
            // last lattice value strictly below raw
            if i32::from(raw) <= i32::from(range.min) {
                None
            } else {
                let k = (i32::from(raw) - 1 - i32::from(range.min)) / step;
                Some((i32::from(range.min) + k * step).min(i32::from(range.max)) as i16)
            }
        };
        if let Some(candidate) = candidate {
            best = Some(match (best, direction > 0) {
                (None, _) => candidate,
                (Some(current), true) => current.min(candidate),
                (Some(current), false) => current.max(candidate),
            });
        }
    }
    best.unwrap_or(raw)
}

/// The raw level `VolumeControl::step` writes: `1/steps` of the 0..1 curve from
/// the current `fraction`, but always at least one DAC step, and never past
/// either end. Returns `raw` itself at the end of the range.
pub(crate) fn step_target_raw(
    raw: i16,
    fraction: f64,
    direction: i8,
    steps: u8,
    ranges: &[RawRange],
) -> i16 {
    if direction == 0 {
        return raw;
    }
    let direction = direction.signum();
    if direction < 0 && fraction <= 0.0 {
        // At or below the bottom of the slider (which reads as 0.0 for anything
        // under -80 dB): one DAC step down, not a jump to the device minimum.
        return adjacent_raw(raw, direction, ranges);
    }
    let steps = f64::from(steps.max(1));
    let target_fraction = (fraction + f64::from(direction) / steps).clamp(0.0, 1.0);
    let target = quantize_volume(target_fraction, ranges);
    if (direction > 0 && target <= raw) || (direction < 0 && target >= raw) {
        // The slider moved less than one DAC step (or the level is below the
        // slider floor): take the next step the DAC has.
        return adjacent_raw(raw, direction, ranges);
    }
    target
}

/// The level a DAC had before this library first changed it during one attachment,
/// and the control that writes it back. The live volume control is dropped with each
/// stream, so the record keeps its own copy and survives stream restarts and idle
/// releases. It ends when the level is handed back or forgotten, or is replaced
/// by another DAC's.
#[derive(Debug)]
pub(crate) struct OriginalVolume<C> {
    pub(crate) device: String,
    pub(crate) control: C,
    pub(crate) raw: i16,
}

#[derive(Debug)]
pub(crate) struct OriginalVolumeSlot<C>(Option<OriginalVolume<C>>);

impl<C> OriginalVolumeSlot<C> {
    pub(crate) const fn new() -> Self {
        Self(None)
    }

    pub(crate) fn is_recorded(&self, device: &str) -> bool {
        self.0.as_ref().is_some_and(|original| original.device == device)
    }

    /// Records `raw` as the original of `device` unless one is already recorded
    /// for it: later readings are this library's own writes. Another DAC's record goes.
    pub(crate) fn note(&mut self, device: &str, control: C, raw: i16) {
        if !self.is_recorded(device) {
            self.0 = Some(OriginalVolume { device: device.to_string(), control, raw });
        }
    }

    /// The record of `device`, without ending it.
    pub(crate) fn peek(&self, device: &str) -> Option<&OriginalVolume<C>> {
        self.0.as_ref().filter(|original| original.device == device)
    }

    /// The record of `device`, ending it. Another DAC's record is left alone.
    pub(crate) fn take(&mut self, device: &str) -> Option<OriginalVolume<C>> {
        if self.is_recorded(device) { self.0.take() } else { None }
    }

    pub(crate) fn forget(&mut self) {
        self.0 = None;
    }
}

/// The one level every channel reads, or None if they disagree (or there are
/// none): a DAC with split channels has no single level to hand back.
pub(crate) fn common_channel_volume(channels: &[i16]) -> Option<i16> {
    let (&first, rest) = channels.split_first()?;
    rest.iter().all(|&raw| raw == first).then_some(first)
}

#[derive(Debug, PartialEq)]
pub(crate) struct HardwareVolumeSnapshot {
    /// The level every channel reads, exactly as the DAC reported it.
    pub(crate) raw: i16,
    pub(crate) volume: f64,
    pub(crate) muted: Option<bool>,
    pub(crate) mute_error: Option<String>,
}

/// Volume readback is mandatory; an optional mute control has independent health.
pub(crate) fn read_hardware_volume_snapshot(
    channels: &[u16],
    ranges: &[RawRange],
    mut read_volume: impl FnMut(u16) -> Result<i16, String>,
    read_mute: impl FnOnce() -> Result<Option<bool>, String>,
) -> Result<HardwareVolumeSnapshot, String> {
    let (&first, remaining) = channels.split_first()
        .ok_or_else(|| "USB volume has no playback channels".to_string())?;
    let raw = read_volume(first)?;
    for &channel in remaining {
        if read_volume(channel)? != raw {
            return Err("USB volume readback channels disagree".to_string());
        }
    }
    if !ranges.iter().any(|range| raw >= range.min && raw <= range.max) {
        return Err("USB volume readback is out of range".to_string());
    }
    let (muted, mute_error) = match read_mute() {
        Ok(muted) => (muted, None),
        Err(error) => (None, Some(error)),
    };
    Ok(HardwareVolumeSnapshot {
        raw,
        volume: normalize_volume(raw, ranges),
        muted,
        mute_error,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uac2_current_is_not_a_uac1_get_request() {
        assert_eq!(AudioControlProtocol::Uac1.current_request(), 0x81);
        assert_eq!(AudioControlProtocol::Uac2.current_request(), 1);
    }

    #[test]
    fn uac2_volume_requires_read_write_and_all_channels() {
        let protocol = AudioControlProtocol::Uac2;
        assert_eq!(protocol.volume_channels(&[0x0c, 0, 0], 2), Some(vec![0]));
        assert_eq!(protocol.volume_channels(&[0x04, 0x0c, 0x0c], 2), Some(vec![1, 2]));
        assert_eq!(protocol.volume_channels(&[0x08, 0x0c, 0], 2), None);
        assert_eq!(AudioControlProtocol::Uac1.volume_channels(&[2], 2), Some(vec![0]));
    }

    #[test]
    fn range_decode_is_signed_bounded_and_multi_range() {
        let data = [2, 0, 0, 0x80, 0, 0xc0, 0, 1, 0, 0xe0, 0, 0, 0, 1];
        let ranges = parse_uac2_volume_ranges(&data).unwrap();
        assert_eq!(ranges[0].min, i16::MIN);
        assert_eq!(ranges[1].max, 0);
        assert!(parse_uac2_volume_ranges(&data[..13]).is_err());
        assert!(parse_uac2_volume_ranges(&[0, 0]).is_err());
        let raw = quantize_volume(0.5, &ranges);
        assert!(ranges.iter().any(|range| raw >= range.min && raw <= range.max));
    }

    #[test]
    fn full_signed_span_does_not_overflow() {
        let ranges = [RawRange { min: i16::MIN, max: i16::MAX, resolution: 1 }];
        assert_eq!(normalize_volume(i16::MIN, &ranges), 0.0);
        assert_eq!(normalize_volume(i16::MAX, &ranges), 1.0);
        assert_eq!(quantize_volume(1.0, &ranges), i16::MAX);
    }

    #[test]
    fn quantization_never_emits_an_unaligned_maximum() {
        let ranges = [RawRange { min: -1000, max: 0, resolution: 256 }];
        assert_eq!(quantize_volume(1.0, &ranges), -232);
    }

    /// Every raw value the ranges support.
    fn lattice(ranges: &[RawRange]) -> Vec<i16> {
        ranges.iter().flat_map(|range| {
            let step = i32::from(range.resolution).max(1) as usize;
            (i32::from(range.min)..=i32::from(range.max)).step_by(step).map(|raw| raw as i16)
        }).collect()
    }

    fn assert_floored_curve_is_consistent(ranges: &[RawRange]) {
        let (min, max) = volume_bounds(ranges);
        let floor = volume_floor(min, max);
        let span = (i32::from(max) - i32::from(floor)) as f64;
        let supported = lattice(ranges);
        assert_eq!(quantize_volume(0.0, ranges), min);
        assert_eq!(normalize_volume(min, ranges), 0.0);
        // Supported steps above the floor survive normalize -> quantize exactly.
        for &raw in supported.iter().filter(|&&raw| raw > floor) {
            let fraction = normalize_volume(raw, ranges);
            assert!(fraction > 0.0 && fraction <= 1.0, "{raw} -> {fraction}");
            assert_eq!(quantize_volume(fraction, ranges), raw, "fraction {fraction}");
        }
        // Whatever the slider requests, the write is supported, never below the
        // floor, and reads back (1e-6, as the write verification does) unchanged.
        let mut previous = min;
        for step in 0..=1000 {
            let volume = f64::from(step) / 1000.0;
            let raw = quantize_volume(volume, ranges);
            assert!(supported.contains(&raw), "{volume} -> {raw}");
            assert!(raw >= previous, "not monotonic at {volume}");
            previous = raw;
            if volume > 0.0 {
                assert!(raw > floor || floor == min, "{volume} -> {raw} is at or below the floor");
            }
            let fraction = normalize_volume(raw, ranges);
            assert!((normalize_volume(quantize_volume(fraction, ranges), ranges) - fraction).abs() < 1e-6);
            // Within one lattice step of the request (a step, not half of one,
            // because the floor itself is excluded for a nonzero fraction).
            let widest = ranges.iter().map(|r| i32::from(r.resolution).max(1)).max().unwrap();
            if volume > 0.0 && ranges.len() == 1 && max > floor {
                let target = f64::from(floor) + volume * span;
                assert!((f64::from(raw) - target).abs() <= f64::from(widest), "{volume} -> {raw}");
            }
        }
        assert_eq!(quantize_volume(1.0, ranges), *supported.iter().max().unwrap());
    }

    #[test]
    fn wide_device_spends_the_slider_above_minus_80_db() {
        // -127..0 dB in 1 dB steps; the old mapping put 50 % at -63.5 dB.
        let ranges = [RawRange { min: -127 * 256, max: 0, resolution: 256 }];
        assert_eq!(VOLUME_FLOOR_RAW, -20480);
        assert_eq!(quantize_volume(0.0, &ranges), -127 * 256);
        assert_eq!(quantize_volume(0.5, &ranges), -40 * 256);
        assert_eq!(quantize_volume(1.0, &ranges), 0);
        assert_eq!(normalize_volume(-40 * 256, &ranges), 0.5);
        // Anything audible-by-slider is above the floor; the floor step itself
        // and everything below it read back as the bottom of the slider.
        assert_eq!(quantize_volume(0.01, &ranges), -79 * 256);
        assert_eq!(normalize_volume(-80 * 256, &ranges), 0.0);
        assert_eq!(normalize_volume(-127 * 256, &ranges), 0.0);
        assert_eq!(normalize_volume(-100 * 256, &ranges), 0.0);
        assert_floored_curve_is_consistent(&ranges);
    }

    #[test]
    fn floored_curve_round_trips_unaligned_and_fine_lattices() {
        for ranges in [
            // 1/256 dB resolution, minimum -128 dB.
            [RawRange { min: i16::MIN, max: 0, resolution: 1 }],
            // Coarse steps that do not land on the floor, maximum above 0 dB.
            [RawRange { min: -32000, max: 3000, resolution: 384 }],
            // Maximum not aligned to the step.
            [RawRange { min: -25600, max: -500, resolution: 256 }],
        ] {
            assert_floored_curve_is_consistent(&ranges);
        }
    }

    #[test]
    fn device_minimum_above_the_floor_keeps_the_linear_curve() {
        let ranges = [RawRange { min: -60 * 256, max: 0, resolution: 256 }];
        for step in 0..=100 {
            let volume = f64::from(step) / 100.0;
            let linear = (-60.0 + volume * 60.0).round() as i16 * 256;
            assert_eq!(quantize_volume(volume, &ranges), linear, "volume {volume}");
        }
        assert_eq!(normalize_volume(-30 * 256, &ranges), 0.5);
        assert_floored_curve_is_consistent(&ranges);
    }

    #[test]
    fn device_that_never_rises_above_the_floor_keeps_its_full_range() {
        let ranges = [RawRange { min: -120 * 256, max: -90 * 256, resolution: 256 }];
        assert_eq!(quantize_volume(0.0, &ranges), -120 * 256);
        assert_eq!(quantize_volume(0.5, &ranges), -105 * 256);
        assert_eq!(quantize_volume(1.0, &ranges), -90 * 256);
        assert_eq!(normalize_volume(-105 * 256, &ranges), 0.5);
        assert_floored_curve_is_consistent(&ranges);
    }

    #[test]
    fn multi_range_floor_in_a_gap_or_inside_a_range() {
        // The floor (-80 dB) falls in the gap: only the upper range is above it.
        let gap = [
            RawRange { min: -127 * 256, max: -100 * 256, resolution: 256 },
            RawRange { min: -50 * 256, max: 0, resolution: 256 },
        ];
        assert_eq!(quantize_volume(0.0, &gap), -127 * 256);
        assert_eq!(quantize_volume(0.001, &gap), -50 * 256);
        assert_eq!(quantize_volume(1.0, &gap), 0);
        assert_floored_curve_is_consistent(&gap);

        // The floor falls inside the lower range, which the slider may use above it.
        let inside = [
            RawRange { min: -127 * 256, max: -64 * 256, resolution: 256 },
            RawRange { min: -32 * 256, max: 0, resolution: 512 },
        ];
        assert_eq!(quantize_volume(0.01, &inside), -79 * 256);
        assert!(inside.iter().any(|r| {
            let raw = quantize_volume(0.5, &inside);
            raw >= r.min && raw <= r.max
        }));
        assert_floored_curve_is_consistent(&inside);
    }

    #[test]
    fn device_with_nothing_supported_above_the_floor_falls_back_to_its_values() {
        // Maximum -20000 is above the floor, but the 20000-wide steps only
        // support -30000: the slider must still produce a supported value.
        let ranges = [RawRange { min: -30000, max: -20000, resolution: 20000 }];
        assert_eq!(quantize_volume(0.0, &ranges), -30000);
        assert_eq!(quantize_volume(0.5, &ranges), -30000);
        assert_eq!(quantize_volume(1.0, &ranges), -30000);
    }

    #[test]
    fn floored_curve_is_never_quieter_than_the_old_one_for_the_same_fraction() {
        // A fraction saved under the old full-range mapping now lands at the same
        // or a louder level on a device whose minimum is below -80 dB (50 % moved
        // from -63.5 dB to -40 dB), so persisted values must not be replayed as is.
        let ranges = [RawRange { min: -127 * 256, max: 0, resolution: 256 }];
        for step in 1..100 {
            let volume = f64::from(step) / 100.0;
            let old = (-127.0 + volume * 127.0).round() as i32 * 256;
            assert!(i32::from(quantize_volume(volume, &ranges)) >= old, "volume {volume}");
        }
    }

    #[test]
    fn legacy_fraction_keeps_its_attenuation_on_the_floored_curve() {
        // lib/core/utils/volume_curve.dart mirrors this to carry a level saved
        // under the old full-range mapping over: -63.5 dB is 20.625 % now.
        let ranges = [RawRange { min: -127 * 256, max: 0, resolution: 256 }];
        let old_raw = |fraction: f64| (-127.0 * 256.0 * (1.0 - fraction)) as i16;
        assert!((normalize_volume(old_raw(0.5), &ranges) - 0.20625).abs() < 1e-12);
        // 60 % is -50.8 dB, truncated to a whole raw unit here.
        assert!((normalize_volume(old_raw(0.6), &ranges) - 0.365).abs() < 1e-4);
        // Quieter than the floor: the bottom of the slider.
        assert_eq!(normalize_volume(old_raw(0.2), &ranges), 0.0);
    }

    const TEST_RANGES: [RawRange; 1] = [RawRange {
        min: -25600, max: 0, resolution: 256,
    }];

    #[test]
    fn optional_mute_failure_does_not_disable_master_volume() {
        let snapshot = read_hardware_volume_snapshot(
            &[0], &TEST_RANGES,
            |channel| { assert_eq!(channel, 0); Ok(-12800) },
            || Err("mute GET_CUR stalled".to_string()),
        ).unwrap();
        // -50 dB of a -80..0 dB slider span.
        assert_eq!(snapshot.volume, 0.375);
        assert_eq!(snapshot.muted, None);
        assert_eq!(snapshot.mute_error.as_deref(), Some("mute GET_CUR stalled"));
    }

    #[test]
    fn stereo_volume_readback_preserves_optional_mute_state() {
        let mut read_channels = Vec::new();
        let snapshot = read_hardware_volume_snapshot(
            &[1, 2], &TEST_RANGES,
            |channel| { read_channels.push(channel); Ok(-6400) },
            || Ok(Some(true)),
        ).unwrap();
        assert_eq!(read_channels, vec![1, 2]);
        assert_eq!(snapshot.volume, 0.6875);
        assert_eq!(snapshot.muted, Some(true));
        assert_eq!(snapshot.mute_error, None);
    }

    #[test]
    fn volume_without_mute_remains_usable_and_mute_can_recover() {
        for muted in [None, Some(false), Some(true)] {
            let snapshot = read_hardware_volume_snapshot(
                &[0], &TEST_RANGES, |_| Ok(-25600), || Ok(muted),
            ).unwrap();
            assert_eq!(snapshot.volume, 0.0);
            assert_eq!(snapshot.muted, muted);
            assert_eq!(snapshot.mute_error, None);
        }
    }

    #[test]
    fn failed_or_inconsistent_volume_never_probes_mute() {
        for (channels, values) in [
            (vec![], vec![]),
            (vec![0], vec![Err("volume GET_CUR stalled".to_string())]),
            (vec![0], vec![Ok(1)]),
            (vec![1, 2], vec![Ok(-12800), Ok(-6400)]),
            (vec![1, 2], vec![Ok(-12800), Err("right channel stalled".to_string())]),
        ] {
            let mut values = values.into_iter();
            let result = read_hardware_volume_snapshot(
                &channels, &TEST_RANGES, |_| values.next().unwrap(),
                || panic!("mute must not mask a volume failure"),
            );
            assert!(result.is_err());
        }
    }

    #[test]
    fn the_first_level_of_an_attachment_is_the_original() {
        let mut slot = OriginalVolumeSlot::new();
        assert!(!slot.is_recorded("dac"));
        slot.note("dac", "control", -12800);
        // Later readings are the library's own writes (a restart, an idle reopen).
        slot.note("dac", "other", -6400);
        let original = slot.take("dac").unwrap();
        assert_eq!((original.raw, original.control), (-12800, "control"));
        // Handing it back ends the record; the next reading starts a new one.
        assert!(slot.take("dac").is_none());
        slot.note("dac", "control", -3000);
        assert_eq!(slot.take("dac").unwrap().raw, -3000);
    }

    #[test]
    fn a_level_below_the_slider_floor_is_kept_exactly() {
        // Normalised, anything under -80 dB reads 0.0, which would be written
        // back as the device minimum; the raw value is what must survive.
        let ranges = [RawRange { min: -127 * 256, max: 0, resolution: 256 }];
        let original = -100 * 256;
        assert_eq!(normalize_volume(original, &ranges), 0.0);
        assert_ne!(quantize_volume(0.0, &ranges), original);
        let mut slot = OriginalVolumeSlot::new();
        slot.note("dac", (), original);
        assert_eq!(slot.take("dac").unwrap().raw, original);
    }

    #[test]
    fn another_dac_neither_takes_nor_keeps_the_record() {
        let mut slot = OriginalVolumeSlot::new();
        slot.note("first", (), -1000);
        assert!(slot.take("second").is_none());
        assert!(slot.is_recorded("first"));
        // A different DAC's first reading replaces the stale record.
        slot.note("second", (), -2000);
        assert!(!slot.is_recorded("first"));
        assert_eq!(slot.take("second").unwrap().raw, -2000);
    }

    #[test]
    fn forgetting_ends_the_record() {
        let mut slot = OriginalVolumeSlot::new();
        slot.note("dac", (), -1000);
        slot.forget();
        assert!(slot.take("dac").is_none());
        slot.note("dac", (), -500);
        assert_eq!(slot.take("dac").unwrap().raw, -500);
    }

    #[test]
    fn split_channels_have_no_single_level_to_hand_back() {
        assert_eq!(common_channel_volume(&[-300]), Some(-300));
        assert_eq!(common_channel_volume(&[-300, -300]), Some(-300));
        assert_eq!(common_channel_volume(&[-300, -256]), None);
        assert_eq!(common_channel_volume(&[]), None);
    }

    #[test]
    fn readback_in_a_gap_between_volume_ranges_is_rejected() {
        let ranges = [
            RawRange { min: -25600, max: -12800, resolution: 256 },
            RawRange { min: -6400, max: 0, resolution: 256 },
        ];
        assert!(read_hardware_volume_snapshot(
            &[0], &ranges, |_| Ok(-9600), || panic!("invalid volume"),
        ).is_err());
    }

    #[test]
    fn quantize_db_reaches_levels_below_the_slider_floor() {
        let ranges = [RawRange { min: -127 * 256, max: 0, resolution: 256 }];
        assert_eq!(quantize_db(-100.0, &ranges), -100 * 256);
        assert_eq!(quantize_db(-100.4, &ranges), -100 * 256);
        assert_eq!(quantize_db(-100.6, &ranges), -101 * 256);
        assert_eq!(quantize_db(-500.0, &ranges), -127 * 256);
        assert_eq!(quantize_db(12.0, &ranges), 0);
        // The quieter value wins a tie.
        assert_eq!(quantize_db(-100.5, &ranges), -101 * 256);
    }

    #[test]
    fn quantize_db_stays_on_the_lattice_of_multi_range_devices() {
        let ranges = [
            RawRange { min: -127 * 256, max: -100 * 256, resolution: 256 },
            RawRange { min: -50 * 256, max: 0, resolution: 256 },
        ];
        assert_eq!(quantize_db(-75.0, &ranges), -100 * 256);
        assert_eq!(quantize_db(-70.0, &ranges), -50 * 256);
        assert_eq!(quantize_db(-3.2, &ranges), -3 * 256);
    }

    #[test]
    fn adjacent_raw_walks_the_lattice() {
        let ranges = [RawRange { min: -25600, max: 0, resolution: 256 }];
        assert_eq!(adjacent_raw(-256, 1, &ranges), 0);
        assert_eq!(adjacent_raw(0, 1, &ranges), 0);
        assert_eq!(adjacent_raw(-256, -1, &ranges), -512);
        assert_eq!(adjacent_raw(-25600, -1, &ranges), -25600);
        let gap = [
            RawRange { min: -127 * 256, max: -100 * 256, resolution: 256 },
            RawRange { min: -50 * 256, max: 0, resolution: 256 },
        ];
        assert_eq!(adjacent_raw(-100 * 256, 1, &gap), -50 * 256);
        assert_eq!(adjacent_raw(-50 * 256, -1, &gap), -100 * 256);
    }

    #[test]
    fn the_snapshot_keeps_the_exact_raw_level() {
        let ranges = [RawRange { min: -127 * 256, max: 0, resolution: 256 }];
        let snapshot = read_hardware_volume_snapshot(
            &[0], &ranges, |_| Ok(-100 * 256), || Ok(None),
        ).unwrap();
        assert_eq!(snapshot.raw, -100 * 256);
        assert_eq!(snapshot.volume, 0.0);
    }

    #[test]
    fn a_step_moves_a_sixteenth_of_the_curve_and_never_leaves_the_range() {
        let ranges = [RawRange { min: -127 * 256, max: 0, resolution: 256 }];
        // 50 % of the curve is -40 dB; a 1/16 step up lands 6.25 % higher.
        let up = step_target_raw(-40 * 256, 0.5, 1, 16, &ranges);
        assert_eq!(up, quantize_volume(0.5625, &ranges));
        assert!(up > -40 * 256);
        let down = step_target_raw(-40 * 256, 0.5, -1, 16, &ranges);
        assert_eq!(down, quantize_volume(0.4375, &ranges));
        // The ends hold.
        assert_eq!(step_target_raw(0, 1.0, 1, 16, &ranges), 0);
        assert_eq!(step_target_raw(-127 * 256, 0.0, -1, 16, &ranges), -127 * 256);
        // No direction, no change.
        assert_eq!(step_target_raw(-40 * 256, 0.5, 0, 16, &ranges), -40 * 256);
    }

    #[test]
    fn a_step_always_makes_progress_even_below_the_slider_floor_or_in_a_coarse_lattice() {
        let ranges = [RawRange { min: -127 * 256, max: 0, resolution: 256 }];
        // -100 dB reads as fraction 0.0 (below the -80 dB floor): stepping up
        // jumps to the first slider step above the floor, never down.
        assert!(step_target_raw(-100 * 256, 0.0, 1, 16, &ranges) > -100 * 256);
        // Stepping down from below the floor goes one DAC step down.
        assert_eq!(step_target_raw(-100 * 256, 0.0, -1, 16, &ranges), -101 * 256);
        // 255 steps on a 3-step lattice still moves by one lattice step.
        let coarse = [RawRange { min: -768, max: 0, resolution: 256 }];
        assert_eq!(step_target_raw(-512, 1.0 / 3.0, 1, 255, &coarse), -256);
        assert_eq!(step_target_raw(-512, 1.0 / 3.0, -1, 255, &coarse), -768);
    }

    #[test]
    fn db_and_fraction_agree_on_the_same_step() {
        let ranges = [RawRange { min: -127 * 256, max: 0, resolution: 256 }];
        for db in [-79.0f64, -60.0, -40.0, -12.0, -3.0, 0.0] {
            let raw = quantize_db(db, &ranges);
            let fraction = normalize_volume(raw, &ranges);
            assert_eq!(quantize_volume(fraction, &ranges), raw, "{db} dB");
            assert_eq!(f64::from(raw) / 256.0, db);
        }
    }
}
