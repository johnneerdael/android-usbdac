use libusb1_sys::{
    constants::{
        LIBUSB_TRANSFER_CANCELLED, LIBUSB_TRANSFER_COMPLETED, LIBUSB_TRANSFER_ERROR,
        LIBUSB_TRANSFER_NO_DEVICE, LIBUSB_TRANSFER_OVERFLOW, LIBUSB_TRANSFER_STALL,
        LIBUSB_TRANSFER_TIMED_OUT,
    },
    libusb_iso_packet_descriptor,
};

const MAX_DIAGNOSTIC_PACKETS: usize = 32;

/// Legal packet-size envelope for one isochronous service interval.
///
/// At 44.1 kHz on a 125 µs microframe the average packet is 5.5125 frames, so
/// the host must alternate 5-frame and 6-frame packets. A completion may carry
/// either size (or exactly what this transfer requested, when feedback pacing
/// moved the schedule). Demanding the exact submitted length turns normal
/// repacketisation and FIFO backpressure into a fatal stream error.
#[derive(Debug, Clone, Copy)]
pub(crate) struct PacketAcceptance {
    legal_sizes: [usize; 2],
    bytes_per_frame: usize,
    prefix_bytes: usize,
    /// Only the exact submitted length is acceptable.
    exact: bool,
}

impl PacketAcceptance {
    pub(crate) fn new(sample_rate: u32, service_interval_us: u32, bytes_per_frame: usize) -> Self {
        let bytes_per_frame = bytes_per_frame.max(1);
        let interval = service_interval_us.max(1) as u64;
        let scaled = u64::from(sample_rate) * interval;
        let lo_frames = scaled / 1_000_000;
        let hi_frames = scaled.div_ceil(1_000_000);
        let legal_sizes = [
            lo_frames as usize * bytes_per_frame,
            hi_frames as usize * bytes_per_frame,
        ];
        Self {
            legal_sizes,
            bytes_per_frame,
            prefix_bytes: 0,
            exact: false,
        }
    }

    /// Accept only the exact submitted length (for tests that have no stream
    /// rate to derive an envelope from).
    #[cfg(test)]
    pub(crate) fn exact() -> Self {
        Self {
            legal_sizes: [0, 0],
            bytes_per_frame: 1,
            prefix_bytes: 0,
            exact: true,
        }
    }

    /// Prefix bytes are transport metadata, never samples or lost audio.
    pub(crate) fn with_packet_prefix(mut self, prefix_bytes: usize) -> Self {
        self.prefix_bytes = prefix_bytes;
        self.legal_sizes = self
            .legal_sizes
            .map(|size| size.saturating_add(prefix_bytes));
        self
    }
    fn audio_bytes(&self, wire_bytes: usize) -> usize {
        wire_bytes.saturating_sub(self.prefix_bytes)
    }

    pub(crate) fn legal_sizes(&self) -> [usize; 2] {
        self.legal_sizes
    }

    pub(crate) fn bytes_per_frame(&self) -> usize {
        self.bytes_per_frame
    }

    fn is_legal_size(&self, actual_length: usize, requested: usize) -> bool {
        actual_length == requested
            || (!self.exact
                && (actual_length == self.legal_sizes[0] || actual_length == self.legal_sizes[1]))
    }

    fn is_frame_aligned(&self, actual_length: usize) -> bool {
        actual_length >= self.prefix_bytes
            && self.audio_bytes(actual_length) % self.bytes_per_frame == 0
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct CompletionStats {
    pub short_packets: usize,
    pub zero_packets: usize,
    pub descriptor_rewrites: usize,
    pub shortfall_frames: u64,
    /// Submitted audio not confirmed delivered. Failed requests are charged
    /// in full because their reported partial byte count is not reliable.
    pub lost_bytes: u64,
}

impl CompletionStats {
    /// Lost audio in ring frames (the unit of `consumer_frames`). Rounds up so
    /// a stray partial frame is never reported as no loss.
    pub(crate) fn lost_ring_frames(&self, ring_bytes_per_frame: usize) -> u64 {
        self.lost_bytes.div_ceil(ring_bytes_per_frame.max(1) as u64)
    }
}

/// Validate ISO OUT completions against the legal packet envelope for the
/// stream's service interval.
///
/// A completed packet shorter (or, after repacketisation, longer) than the
/// submitted length can be legal after repacketisation. Bounded generic,
/// timeout and overflow packet errors are charged as full-request loss.
/// The caller applies the stream loss budget. Removal, halt, cancellation and
/// malformed descriptors remain fatal.
pub(crate) fn validate_iso_packet_descriptors(
    packet_sizes: &[usize],
    descriptors: &[libusb_iso_packet_descriptor],
    acceptance: PacketAcceptance,
) -> Result<CompletionStats, String> {
    if packet_sizes.len() != descriptors.len() {
        return Err(format_diagnostics(
            packet_sizes,
            descriptors,
            acceptance,
            "packet count changed",
        ));
    }

    let mut stats = CompletionStats::default();
    let mut failed_packet_bytes = 0;
    for (&requested, packet) in packet_sizes.iter().zip(descriptors) {
        let actual = packet.actual_length as usize;
        if packet.status != LIBUSB_TRANSFER_COMPLETED {
            if matches!(
                packet.status,
                LIBUSB_TRANSFER_ERROR
                    | LIBUSB_TRANSFER_TIMED_OUT
                    | LIBUSB_TRANSFER_OVERFLOW
                    | LIBUSB_TRANSFER_STALL
            ) && !acceptance.exact
                && requested > 0
                && acceptance.is_frame_aligned(requested)
                && packet.length as usize == requested
                && actual <= requested
            {
                failed_packet_bytes += acceptance.audio_bytes(requested);
                stats.zero_packets +=
                    usize::from(actual == 0 && acceptance.audio_bytes(requested) > 0);
                continue;
            }
            return Err(format_diagnostics(
                packet_sizes,
                descriptors,
                acceptance,
                "packet status is not completed",
            ));
        }

        if packet.length as usize != requested {
            stats.descriptor_rewrites = stats.descriptor_rewrites.saturating_add(1);
        }

        if actual == 0 || actual == acceptance.prefix_bytes {
            if acceptance.audio_bytes(requested) == 0 {
                // Nothing was asked of this packet, so nothing was dropped.
                continue;
            }
            // A zero-length completion of a real request is loss, and only an
            // envelope-based stream tolerates it (the caller accounts for it).
            if acceptance.exact {
                return Err(format_diagnostics(
                    packet_sizes,
                    descriptors,
                    acceptance,
                    "zero-length completion of a non-empty packet",
                ));
            }
            stats.zero_packets = stats.zero_packets.saturating_add(1);
            continue;
        }

        if !acceptance.is_frame_aligned(actual) {
            return Err(format_diagnostics(
                packet_sizes,
                descriptors,
                acceptance,
                "packet is not frame-aligned",
            ));
        }

        // A repacketising host controller may move a packet's bytes into a
        // neighbouring microframe of the same URB, so `actual` can be the other
        // legal size instead of exactly `requested`. The transfer total is
        // checked below and can never exceed what was submitted.
        if !acceptance.is_legal_size(actual, requested) {
            return Err(format_diagnostics(
                packet_sizes,
                descriptors,
                acceptance,
                "packet length is outside the legal envelope",
            ));
        }

        if actual != requested {
            stats.short_packets = stats.short_packets.saturating_add(1);
        }
    }

    // Repacketisation may shuffle bytes between packets of one URB, but the
    // whole transfer can never deliver more than it submitted.
    let requested_total: usize = packet_sizes.iter().copied().sum();
    let actual_total: usize = descriptors
        .iter()
        .map(|packet| packet.actual_length as usize)
        .sum();
    if actual_total > requested_total {
        return Err(format_diagnostics(
            packet_sizes,
            descriptors,
            acceptance,
            "transfer exceeds the submitted byte total",
        ));
    }

    // Never credit neighbouring packets for a known failed request. Their
    // larger legal sizes can reflect repacketisation, not delivery of the
    // specific audio that was scheduled in the failed packet.
    let requested_audio: usize = packet_sizes
        .iter()
        .map(|bytes| acceptance.audio_bytes(*bytes))
        .sum();
    let actual_audio: usize = descriptors
        .iter()
        .filter(|packet| packet.status == LIBUSB_TRANSFER_COMPLETED)
        .map(|packet| acceptance.audio_bytes(packet.actual_length as usize))
        .sum();
    stats.lost_bytes = requested_audio
        .saturating_sub(actual_audio)
        .max(failed_packet_bytes) as u64;
    stats.shortfall_frames = stats.lost_bytes / acceptance.bytes_per_frame.max(1) as u64;

    Ok(stats)
}

/// Account for a URB that libusb reported as timed out.
///
/// libusb retires every URB of the transfer and copies each packet's
/// `actual_length` back before it reports a timeout (`handle_iso_completion` in
/// `linux_usbfs.c`), so the descriptors describe what really went out. How a
/// cancelled packet is reported is up to the kernel: a OnePlus 15 reports
/// `LIBUSB_TRANSFER_ERROR` with `actual_length == 0`, and a host may equally
/// report it completed with zero bytes, so `status` alone proves nothing. A
/// packet counts as delivered only if it completed with its full requested
/// length. Everything
/// else is lost, including a short fragment (not credited) and a swap of the
/// two legal sizes (which the completed-URB path would not call loss). That
/// over-counts a little on a path that is already abnormal, never under-counts.
///
/// If the descriptors do not match the submitted schedule nothing can be
/// trusted and the whole URB is lost.
pub(crate) fn account_timed_out_transfer(
    packet_sizes: &[usize],
    descriptors: &[libusb_iso_packet_descriptor],
    acceptance: PacketAcceptance,
) -> CompletionStats {
    let requested_total: u64 = packet_sizes
        .iter()
        .map(|length| acceptance.audio_bytes(*length) as u64)
        .sum();
    let delivered_total: u64 = if packet_sizes.len() == descriptors.len() {
        packet_sizes
            .iter()
            .zip(descriptors)
            .filter(|(requested, packet)| {
                packet.status == LIBUSB_TRANSFER_COMPLETED
                    && packet.actual_length as usize == **requested
            })
            .map(|(requested, _)| acceptance.audio_bytes(*requested) as u64)
            .sum()
    } else {
        0
    };

    let lost_bytes = requested_total - delivered_total;
    CompletionStats {
        shortfall_frames: lost_bytes / acceptance.bytes_per_frame.max(1) as u64,
        lost_bytes,
        ..CompletionStats::default()
    }
}

fn format_diagnostics(
    packet_sizes: &[usize],
    descriptors: &[libusb_iso_packet_descriptor],
    acceptance: PacketAcceptance,
    reason: &str,
) -> String {
    // Only allocate/format diagnostics on failure. These are sums of individual
    // packet lengths: libusb_transfer.actual_length is not meaningful for ISO.
    let requested_total: u64 = packet_sizes.iter().map(|length| *length as u64).sum();
    let descriptor_total: u64 = descriptors
        .iter()
        .map(|packet| u64::from(packet.length))
        .sum();
    let actual_total: u64 = descriptors
        .iter()
        .map(|packet| u64::from(packet.actual_length))
        .sum();
    let status_errors = descriptors
        .iter()
        .filter(|packet| packet.status != LIBUSB_TRANSFER_COMPLETED)
        .count();
    let descriptor_changes = packet_sizes
        .iter()
        .zip(descriptors)
        .filter(|(requested, packet)| **requested != packet.length as usize)
        .count();
    let length_mismatches = packet_sizes
        .iter()
        .zip(descriptors)
        .filter(|(requested, packet)| packet.actual_length as usize != **requested)
        .count();
    let packet_count = packet_sizes.len().max(descriptors.len());
    let packet_results = (0..packet_count.min(MAX_DIAGNOSTIC_PACKETS))
        .map(|index| {
            let requested = packet_sizes
                .get(index)
                .map(|length| length.to_string())
                .unwrap_or_else(|| "missing".to_string());
            match descriptors.get(index) {
                Some(packet) => format!(
                    "#{}:requested={}/descriptor={}/actual={}/status={}({})",
                    index,
                    requested,
                    packet.length,
                    packet.actual_length,
                    iso_transfer_status_label(packet.status),
                    packet.status,
                ),
                None => format!("#{}:requested={}/descriptor=missing", index, requested),
            }
        })
        .collect::<Vec<_>>();

    format!(
        "isochronous transfer completed with packet errors: {}; \
         packet_counts=requested:{},descriptors:{} legal={}/{} frame={} prefix={} \
         requested_total={} descriptor_total={} actual_total={} actual_minus_requested={} \
         status_errors={} descriptor_changes={} length_mismatches={}; \
         packet_results=[{}] omitted_packets={}",
        reason,
        packet_sizes.len(),
        descriptors.len(),
        acceptance.legal_sizes[0],
        acceptance.legal_sizes[1],
        acceptance.bytes_per_frame,
        acceptance.prefix_bytes,
        requested_total,
        descriptor_total,
        actual_total,
        i128::from(actual_total) - i128::from(requested_total),
        status_errors,
        descriptor_changes,
        length_mismatches,
        packet_results.join(","),
        packet_count.saturating_sub(MAX_DIAGNOSTIC_PACKETS),
    )
}

pub(crate) fn iso_transfer_status_label(status: i32) -> &'static str {
    match status {
        LIBUSB_TRANSFER_COMPLETED => "completed",
        LIBUSB_TRANSFER_TIMED_OUT => "timed_out",
        LIBUSB_TRANSFER_STALL => "stall",
        LIBUSB_TRANSFER_NO_DEVICE => "no_device",
        LIBUSB_TRANSFER_OVERFLOW => "overflow",
        LIBUSB_TRANSFER_CANCELLED => "cancelled",
        LIBUSB_TRANSFER_ERROR => "error",
        _ => "unknown",
    }
}
