//! Descriptor-promised startup clock recovery, independent of wall-clock time.
use super::{ClaimedHandle, SyncType, TransferType, UsageType};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum LockDelay {
    Milliseconds(u16),
    /// Decoded PCM samples per channel, hence PCM sample frames on the wire.
    PcmSampleFrames(u16),
}

/// Inspect only libusb's cached active configuration. No control requests or
/// alternate-setting changes belong in this descriptor lookup.
pub(super) fn selected_delay(
    claimed: &ClaimedHandle,
    interface: u8,
    alternate: u8,
    address: u8,
    decoded_pcm: bool,
) -> Result<Option<LockDelay>, String> {
    if !claimed.claimed_interfaces.contains(&interface) {
        return Err("USB endpoint lock-delay interface is not claimed".into());
    }
    let config = claimed
        .handle
        .device()
        .active_config_descriptor()
        .map_err(|e| e.to_string())?;
    for itf in config.interfaces() {
        for alt in itf.descriptors() {
            if alt.interface_number() != interface || alt.setting_number() != alternate {
                continue;
            }
            if alt.class_code() != 1 || alt.sub_class_code() != 2 {
                return Ok(None);
            }
            for ep in alt.endpoint_descriptors() {
                if ep.address() == address {
                    return endpoint_delay(
                        alt.protocol_code(),
                        ep.transfer_type(),
                        ep.sync_type(),
                        ep.usage_type(),
                        ep.extra().unwrap_or(&[]),
                        decoded_pcm,
                    );
                }
            }
            return Ok(None);
        }
    }
    Ok(None)
}

fn endpoint_delay(
    protocol: u8,
    transfer: TransferType,
    sync: SyncType,
    usage: UsageType,
    extra: &[u8],
    decoded_pcm: bool,
) -> Result<Option<LockDelay>, String> {
    // UAC1 4.6.1.2, UAC2 4.10.1.2 and full ADC3 4.8.1.2:
    // clock recovery applies only to synchronous/adaptive audio data endpoints.
    if transfer != TransferType::Isochronous
        || !matches!(sync, SyncType::Adaptive | SyncType::Synchronous)
        || !matches!(usage, UsageType::Data | UsageType::FeedbackData)
    {
        return Ok(None);
    }
    let (length, units_at) = match protocol {
        0x00 => (7, 4),
        0x20 => (8, 5),
        0x30 => (10, 7),
        _ => return Ok(None),
    };
    let mut promise = None;
    let mut at = 0;
    while at < extra.len() {
        let remaining = &extra[at..];
        let general = remaining.len() >= 3 && remaining[1] == 0x25 && remaining[2] == 1;
        let size = usize::from(remaining[0]);
        if general && (size != length || remaining.len() < size) {
            return Err("USB malformed class-specific audio endpoint lock-delay descriptor".into());
        }
        if size < 2 || size > remaining.len() {
            // An unrelated malformed extra does not invent a lock promise or
            // change the legacy no-delay path. Its boundary cannot be trusted.
            break;
        }
        if general {
            let value = (
                remaining[units_at],
                u16::from_le_bytes([remaining[units_at + 1], remaining[units_at + 2]]),
            );
            if promise.is_some_and(|previous| previous != value) {
                return Err("USB conflicting audio endpoint lock-delay descriptors".into());
            }
            promise = Some(value);
        }
        // SS/SSP companion descriptors and other endpoint extras are skipped
        // by their own length; the class-specific descriptor need not be first.
        at += size;
    }
    Ok(match promise {
        Some((1, amount)) if amount != 0 => Some(LockDelay::Milliseconds(amount)),
        Some((2, amount)) if amount != 0 && decoded_pcm => Some(LockDelay::PcmSampleFrames(amount)),
        // Undefined/reserved units, zero delay and native DSD sample units do
        // not carry an applicable promise. Milliseconds also cover encoded data.
        _ => None,
    })
}

/// Count only accepted, scheduled startup data. Callers must discard the
/// packets counted here, including the packet that completes the gate. A
/// failed completion resets pending progress; completion itself is permanent.
/// Callback batches may conservatively count only wholly accepted batches.
pub(super) struct LockDelayGate {
    delay: Option<LockDelay>,
    progress: u64,
}

impl LockDelayGate {
    pub(super) fn new(delay: Option<LockDelay>) -> Self {
        let delay = match delay {
            Some(LockDelay::Milliseconds(0) | LockDelay::PcmSampleFrames(0)) => None,
            other => other,
        };
        Self { delay, progress: 0 }
    }

    pub(super) fn pending(&self) -> bool {
        self.delay.is_some()
    }

    pub(super) fn observe(&mut self, accepted: bool, service_us: u64, sample_frames: u64) {
        let Some(delay) = self.delay else {
            return;
        };
        if !accepted {
            self.progress = 0;
            return;
        }
        let (increment, required) = match delay {
            LockDelay::Milliseconds(ms) => (service_us, u64::from(ms) * 1000),
            LockDelay::PcmSampleFrames(frames) => (sample_frames, u64::from(frames)),
        };
        self.progress = self.progress.saturating_add(increment);
        if self.progress >= required {
            self.delay = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(protocol: u8, bytes: &[u8], pcm: bool) -> Result<Option<LockDelay>, String> {
        endpoint_delay(
            protocol,
            TransferType::Isochronous,
            SyncType::Adaptive,
            UsageType::Data,
            bytes,
            pcm,
        )
    }

    #[test]
    fn exact_layouts_and_little_endian_amounts_include_full_adc3() {
        for (protocol, descriptor) in [
            (0, vec![7, 0x25, 1, 0, 1, 0x34, 0x12]),
            (0x20, vec![8, 0x25, 1, 0, 0, 1, 0x34, 0x12]),
            (0x30, vec![10, 0x25, 1, 0, 0, 0, 0, 1, 0x34, 0x12]),
        ] {
            assert_eq!(
                parse(protocol, &descriptor, true).unwrap(),
                Some(LockDelay::Milliseconds(0x1234))
            );
            let mut extras = vec![6, 0x30, 0, 0x80, 1, 0, 8, 0x31, 0, 0, 0, 1, 0, 0];
            extras.extend_from_slice(&descriptor);
            assert_eq!(
                parse(protocol, &extras, true).unwrap(),
                Some(LockDelay::Milliseconds(0x1234))
            );
        }
    }

    #[test]
    fn no_applicable_promise_preserves_legacy_path() {
        assert_eq!(parse(0, &[], true).unwrap(), None);
        for units in [0, 1, 2, 3, 255] {
            assert_eq!(parse(0, &[7, 0x25, 1, 0, units, 0, 0], true).unwrap(), None);
        }
        for units in [0, 3, 255] {
            assert_eq!(parse(0, &[7, 0x25, 1, 0, units, 1, 0], true).unwrap(), None);
        }
        assert_eq!(parse(0x77, &[7, 0x25, 1, 0, 1, 1, 0], true).unwrap(), None);
        for sync in [SyncType::NoSync, SyncType::Asynchronous] {
            assert_eq!(
                endpoint_delay(
                    0,
                    TransferType::Isochronous,
                    sync,
                    UsageType::Data,
                    &[7, 0x25, 1, 0, 1, 10, 0],
                    true
                )
                .unwrap(),
                None
            );
        }
        for usage in [UsageType::Feedback, UsageType::Reserved] {
            assert_eq!(
                endpoint_delay(
                    0,
                    TransferType::Isochronous,
                    SyncType::Synchronous,
                    usage,
                    &[7, 0x25, 1, 0, 1, 10, 0],
                    true
                )
                .unwrap(),
                None
            );
        }
        assert_eq!(
            endpoint_delay(
                0,
                TransferType::Bulk,
                SyncType::Synchronous,
                UsageType::Data,
                &[7, 0x25, 1, 0, 1, 10, 0],
                true
            )
            .unwrap(),
            None
        );
    }

    #[test]
    fn decoded_samples_mean_pcm_frames_and_milliseconds_cover_encoded_paths() {
        assert_eq!(
            parse(0, &[7, 0x25, 1, 0, 2, 255, 255], true).unwrap(),
            Some(LockDelay::PcmSampleFrames(65535))
        );
        assert_eq!(parse(0, &[7, 0x25, 1, 0, 2, 10, 0], false).unwrap(), None);
        assert_eq!(
            parse(0, &[7, 0x25, 1, 0, 1, 255, 255], false).unwrap(),
            Some(LockDelay::Milliseconds(65535))
        );
        assert_eq!(
            endpoint_delay(
                0,
                TransferType::Isochronous,
                SyncType::Synchronous,
                UsageType::FeedbackData,
                &[7, 0x25, 1, 0, 1, 10, 0],
                true
            )
            .unwrap(),
            Some(LockDelay::Milliseconds(10))
        );
    }

    #[test]
    fn recognizable_malformed_and_conflicting_promises_fail() {
        for descriptor in [
            vec![6, 0x25, 1, 0, 1, 2],
            vec![7, 0x25, 1, 0, 1, 2],
            vec![0, 0x25, 1],
        ] {
            assert!(parse(0, &descriptor, true).is_err());
        }
        let first = [7, 0x25, 1, 0, 1, 10, 0];
        assert_eq!(
            parse(0, &[first, first].concat(), true).unwrap(),
            Some(LockDelay::Milliseconds(10))
        );
        assert!(parse(0, &[first, [7, 0x25, 1, 0, 1, 11, 0]].concat(), true).is_err());
        assert_eq!(parse(0, &[3, 0x25, 2], true).unwrap(), None);
        assert_eq!(parse(0, &[1, 0x30], true).unwrap(), None);
    }

    #[test]
    fn pending_time_requires_contiguous_accepted_scheduled_service() {
        let mut gate = LockDelayGate::new(Some(LockDelay::Milliseconds(2)));
        gate.observe(true, 1000, 9999);
        assert!(gate.pending());
        gate.observe(false, 1000, 9999);
        gate.observe(true, 1000, 9999);
        assert!(gate.pending());
        gate.observe(true, 1000, 0);
        assert!(!gate.pending());
        gate.observe(false, 0, 0);
        assert!(!gate.pending());
    }

    #[test]
    fn sample_gate_counts_frames_without_rate_or_channel_conversion() {
        let mut gate = LockDelayGate::new(Some(LockDelay::PcmSampleFrames(96)));
        gate.observe(true, u64::MAX, 48);
        assert!(gate.pending());
        gate.observe(false, 1000, 48);
        gate.observe(true, 0, 48);
        assert!(gate.pending());
        gate.observe(true, 0, 48);
        assert!(!gate.pending());
        for delay in [
            None,
            Some(LockDelay::Milliseconds(0)),
            Some(LockDelay::PcmSampleFrames(0)),
        ] {
            assert!(!LockDelayGate::new(delay).pending());
        }
        let mut maximum = LockDelayGate::new(Some(LockDelay::Milliseconds(65535)));
        maximum.observe(true, u64::MAX, 0);
        assert!(!maximum.pending());
    }
}
