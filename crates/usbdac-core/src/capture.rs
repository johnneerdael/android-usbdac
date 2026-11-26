//! PCM capture data contracts and bounded buffering.
//!
//! This module does not open USB interfaces. The session owner must serialize
//! completion delivery and reads (for example under its existing stream lock).
//! No allocations occur on the completion or read paths.

/// Spatial labels in USB Audio channel configurations. These are identities,
/// not instructions to reorder samples.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelPosition {
    FrontLeft,
    FrontRight,
    FrontCenter,
    LowFrequency,
    BackLeft,
    BackRight,
    FrontLeftOfCenter,
    FrontRightOfCenter,
    BackCenter,
    SideLeft,
    SideRight,
    TopCenter,
    TopFrontLeft,
    TopFrontCenter,
    TopFrontRight,
    TopBackLeft,
    TopBackCenter,
    TopBackRight,
    TopFrontLeftOfCenter,
    TopFrontRightOfCenter,
    LeftLowFrequency,
    RightLowFrequency,
    TopSideLeft,
    TopSideRight,
    BottomCenter,
    BackLeftOfCenter,
    BackRightOfCenter,
    /// No descriptor label; retain the zero-based interleaved channel index.
    Unspecified {
        index: u16,
    },
}

/// Ordered channel identities, including unnamed wire channels.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelMap {
    positions: Vec<ChannelPosition>,
}

impl ChannelMap {
    /// Decode UAC1's wChannelConfig (Audio 1.0 section 3.7.2.3).
    pub fn uac1(channels: u16, mask: u16) -> Result<Self, CaptureError> {
        Self::from_mask(channels, u32::from(mask), 12)
    }

    /// Decode UAC2's bmChannelConfig (Audio 2.0 section 3.7.2.1).
    /// Bit 31 indicates non-predefined positions, rather than another speaker.
    pub fn uac2(channels: u16, mask: u32) -> Result<Self, CaptureError> {
        if mask & 0x8000_0000 != 0 {
            if mask != 0x8000_0000 {
                return Err(CaptureError::InvalidChannelMap);
            }
            return Self::from_mask(channels, 0, 27);
        }
        Self::from_mask(channels, mask, 27)
    }

    fn from_mask(channels: u16, mask: u32, bits: u32) -> Result<Self, CaptureError> {
        use ChannelPosition::*;
        // USB bit order is the wire channel order. Keep labels explicit instead
        // of relying on another audio API's numeric speaker enumeration.
        const LABELS: [ChannelPosition; 27] = [
            FrontLeft,
            FrontRight,
            FrontCenter,
            LowFrequency,
            BackLeft,
            BackRight,
            FrontLeftOfCenter,
            FrontRightOfCenter,
            BackCenter,
            SideLeft,
            SideRight,
            TopCenter,
            TopFrontLeft,
            TopFrontCenter,
            TopFrontRight,
            TopBackLeft,
            TopBackCenter,
            TopBackRight,
            TopFrontLeftOfCenter,
            TopFrontRightOfCenter,
            LeftLowFrequency,
            RightLowFrequency,
            TopSideLeft,
            TopSideRight,
            BottomCenter,
            BackLeftOfCenter,
            BackRightOfCenter,
        ];
        if channels == 0 || mask >> bits != 0 || mask.count_ones() > u32::from(channels) {
            return Err(CaptureError::InvalidChannelMap);
        }
        let mut positions = Vec::with_capacity(usize::from(channels));
        for bit in 0..bits {
            if mask & (1 << bit) != 0 {
                positions.push(LABELS[bit as usize]);
            }
        }
        while positions.len() < usize::from(channels) {
            positions.push(Unspecified {
                index: positions.len() as u16,
            });
        }
        Ok(Self { positions })
    }

    pub fn positions(&self) -> &[ChannelPosition] {
        &self.positions
    }
}

/// Exact linear PCM request. No conversion, rate substitution or channel remix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaptureRequest {
    pub sample_rate: u32,
    pub channels: u16,
    pub valid_bits: u8,
    /// Optional required wire order, preserved across negotiation/reopen.
    pub channel_map: Option<ChannelMap>,
}

/// Device PCM geometry, including padding bytes carried unchanged to callers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaptureFormat {
    sample_rate: u32,
    valid_bits: u8,
    subslot_bytes: u8,
    channel_map: ChannelMap,
}

impl CaptureFormat {
    pub fn pcm(
        sample_rate: u32,
        valid_bits: u8,
        subslot_bytes: u8,
        channel_map: ChannelMap,
    ) -> Result<Self, CaptureError> {
        if sample_rate == 0
            || !(1..=4).contains(&subslot_bytes)
            || valid_bits == 0
            || valid_bits > subslot_bytes * 8
        {
            return Err(CaptureError::InvalidFormat);
        }
        Ok(Self {
            sample_rate,
            valid_bits,
            subslot_bytes,
            channel_map,
        })
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }
    pub fn valid_bits(&self) -> u8 {
        self.valid_bits
    }
    pub fn subslot_bytes(&self) -> u8 {
        self.subslot_bytes
    }
    pub fn channel_map(&self) -> &ChannelMap {
        &self.channel_map
    }
    pub fn channels(&self) -> u16 {
        self.channel_map.positions.len() as u16
    }
    pub fn bytes_per_frame(&self) -> usize {
        self.channel_map.positions.len() * usize::from(self.subslot_bytes)
    }

    pub fn satisfies(&self, request: &CaptureRequest) -> bool {
        self.sample_rate == request.sample_rate
            && self.channels() == request.channels
            && self.valid_bits == request.valid_bits
            && request
                .channel_map
                .as_ref()
                .is_none_or(|map| map == &self.channel_map)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureError {
    InvalidFormat,
    InvalidChannelMap,
    InvalidCapacity,
    AllocationFailed,
    /// Caller buffer cannot contain a whole number of wire frames.
    PartialFrame,
    /// Packet count, submitted length or backing buffer bounds changed.
    InvalidCompletion,
    Stopped,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureState {
    Active,
    Stopped,
    Failed,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CaptureStats {
    /// Callback publication observed ahead of an older submitted transfer.
    /// Bytes are still delivered in submission order.
    pub reordered_completions: u64,
    /// Valid full frames received, including any subsequently dropped backlog.
    pub captured_frames: u64,
    pub delivered_frames: u64,
    /// Host backlog only. Never includes transport loss.
    pub overrun_frames: u64,
    pub empty_packets: u64,
    /// Successful nonempty packets shorter than their submitted capacity.
    pub short_packets: u64,
    pub failed_packets: u64,
    pub malformed_packets: u64,
    pub malformed_completions: u64,
    pub failed_transfers: u64,
    /// Independently known lost device frames; never infer this from spare IN
    /// request capacity or an empty/short successful service packet.
    pub missing_frames: u64,
}

impl CaptureStats {
    /// Lower-bound device position since configure. Unknown losses make this
    /// an underestimate; host backlog does not change device position.
    pub fn device_position_frames(&self) -> u64 {
        self.captured_frames.saturating_add(self.missing_frames)
    }
}

/// Completion descriptor detached from libusb. `requested_bytes` must equal
/// the immutable submitted packet length supplied to `ingest_completion`.
#[derive(Debug, Clone, Copy)]
pub struct CapturePacket {
    pub requested_bytes: usize,
    pub actual_bytes: usize,
    pub succeeded: bool,
}

/// Fixed-capacity queue of interleaved wire frames. Configure once before
/// transfer submission; retain newest frames when the reader falls behind.
pub struct CaptureBuffer {
    format: CaptureFormat,
    storage: Vec<u8>,
    head_frame: usize,
    queued_frames: usize,
    capacity_frames: usize,
    stats: CaptureStats,
    state: CaptureState,
}

/// Standalone PCM capture running through the direct USB session. A caller may
/// read from any thread; reads copy complete interleaved wire frames without
/// conversion. Slow readers drop old queued frames and expose that in stats.
pub struct DacCaptureStream {
    dac: std::sync::Arc<crate::dac::DacShared>,
    format: CaptureFormat,
    interface: u8,
    alternate: u8,
    endpoint: u8,
    configuration: u8,
    handle: parking_lot::Mutex<crate::transport::capture_stream::CaptureHandle>,
}

impl DacCaptureStream {
    pub(crate) fn new(
        dac: std::sync::Arc<crate::dac::DacShared>,
        handle: crate::transport::capture_stream::CaptureHandle,
    ) -> Self {
        Self {
            dac,
            format: handle.endpoint.format.clone(),
            interface: handle.endpoint.interface,
            alternate: handle.endpoint.alternate,
            endpoint: handle.endpoint.address,
            configuration: handle.endpoint.configuration,
            handle: parking_lot::Mutex::new(handle),
        }
    }

    pub fn format(&self) -> &CaptureFormat {
        &self.format
    }
    pub fn interface_number(&self) -> u8 {
        self.interface
    }
    pub fn alternate_setting(&self) -> u8 {
        self.alternate
    }
    pub fn endpoint_address(&self) -> u8 {
        self.endpoint
    }
    pub fn configuration_id(&self) -> u8 {
        self.configuration
    }

    /// Return whole wire frames copied. An active empty stream returns zero.
    /// Destination length must be a multiple of `format().bytes_per_frame()`.
    pub fn read(&self, destination: &mut [u8]) -> crate::Result<usize> {
        self.dac.ensure_open()?;
        let handle = self.handle.lock();
        let result = handle.buffer.lock().read(destination);
        result.map_err(|error| match error {
            CaptureError::PartialFrame => crate::Error::InvalidArgument(
                "capture read buffer must contain whole wire frames".into(),
            ),
            CaptureError::Stopped => crate::Error::InvalidState("capture stream is stopped".into()),
            CaptureError::Failed => handle
                .error
                .lock()
                .clone()
                .unwrap_or_else(|| crate::Error::Transfer("capture stream failed".into())),
            other => crate::Error::Transfer(format!("capture read: {other:?}")),
        })
    }

    pub fn available_frames(&self) -> usize {
        self.handle.lock().buffer.lock().available_frames()
    }
    pub fn stats(&self) -> CaptureStats {
        self.handle.lock().buffer.lock().stats()
    }
    pub fn state(&self) -> CaptureState {
        self.handle.lock().buffer.lock().state()
    }
    pub fn last_error(&self) -> Option<crate::Error> {
        self.handle.lock().error.lock().clone()
    }

    /// Stop and retire IN transfers before handing the interface back. A
    /// timeout retains USB ownership in the worker; keep host claims/FD until
    /// `Dac::await_usb_retirement` confirms retirement after closing the DAC.
    pub fn close(&self) -> crate::Result<()> {
        self.handle.lock().stop()
    }
}

impl CaptureBuffer {
    pub(crate) fn note_malformed_completion(&mut self) {
        self.stats.malformed_completions = self.stats.malformed_completions.saturating_add(1);
    }

    pub(crate) fn note_reordered_completion(&mut self) {
        self.stats.reordered_completions = self.stats.reordered_completions.saturating_add(1);
    }
    pub fn new(format: CaptureFormat, capacity_frames: usize) -> Result<Self, CaptureError> {
        let bytes = capacity_frames
            .checked_mul(format.bytes_per_frame())
            .filter(|size| *size != 0)
            .ok_or(CaptureError::InvalidCapacity)?;
        let mut storage = Vec::new();
        storage
            .try_reserve_exact(bytes)
            .map_err(|_| CaptureError::AllocationFailed)?;
        storage.resize(bytes, 0);
        Ok(Self {
            format,
            storage,
            head_frame: 0,
            queued_frames: 0,
            capacity_frames,
            stats: CaptureStats::default(),
            state: CaptureState::Active,
        })
    }

    pub fn format(&self) -> &CaptureFormat {
        &self.format
    }
    pub fn available_frames(&self) -> usize {
        self.queued_frames
    }
    pub fn stats(&self) -> CaptureStats {
        self.stats
    }
    pub fn state(&self) -> CaptureState {
        self.state
    }
    pub fn stop(&mut self) {
        self.state = CaptureState::Stopped;
    }
    pub fn fail(&mut self) {
        self.state = CaptureState::Failed;
    }

    fn require_active(&self) -> Result<(), CaptureError> {
        match self.state {
            CaptureState::Active => Ok(()),
            CaptureState::Stopped => Err(CaptureError::Stopped),
            CaptureState::Failed => Err(CaptureError::Failed),
        }
    }

    /// Validate the entire submitted geometry before accepting any payload.
    /// Buffer length must equal the sum of immutable submitted packet lengths;
    /// packet offsets advance by submitted capacity, never by actual length.
    /// Failed transfers have no trustworthy payload, regardless of packet flags.
    pub fn ingest_completion(
        &mut self,
        data: &[u8],
        submitted: &[usize],
        packets: &[CapturePacket],
        transfer_succeeded: bool,
        known_missing_frames: u64,
    ) -> Result<(), CaptureError> {
        self.require_active()?;
        self.stats.missing_frames = self
            .stats
            .missing_frames
            .saturating_add(known_missing_frames);
        if !transfer_succeeded {
            self.stats.failed_transfers = self.stats.failed_transfers.saturating_add(1);
            return Ok(());
        }
        let total = submitted
            .iter()
            .try_fold(0usize, |total, length| total.checked_add(*length));
        if submitted.len() != packets.len()
            || total != Some(data.len())
            || submitted
                .iter()
                .zip(packets)
                .any(|(length, packet)| *length != packet.requested_bytes)
        {
            self.note_malformed_completion();
            return Err(CaptureError::InvalidCompletion);
        }
        let frame_bytes = self.format.bytes_per_frame();
        let mut offset = 0;
        for (length, packet) in submitted.iter().zip(packets) {
            if !packet.succeeded {
                self.stats.failed_packets = self.stats.failed_packets.saturating_add(1);
            } else if packet.actual_bytes > *length || packet.actual_bytes % frame_bytes != 0 {
                self.stats.malformed_packets = self.stats.malformed_packets.saturating_add(1);
            } else if packet.actual_bytes == 0 {
                self.stats.empty_packets = self.stats.empty_packets.saturating_add(1);
            } else {
                if packet.actual_bytes < *length {
                    self.stats.short_packets = self.stats.short_packets.saturating_add(1);
                }
                self.push_frames(&data[offset..offset + packet.actual_bytes]);
            }
            offset += length;
        }
        Ok(())
    }

    fn push_frames(&mut self, mut data: &[u8]) {
        let frame_bytes = self.format.bytes_per_frame();
        let frames = data.len() / frame_bytes;
        self.stats.captured_frames = self.stats.captured_frames.saturating_add(frames as u64);
        let dropped = self
            .queued_frames
            .saturating_add(frames)
            .saturating_sub(self.capacity_frames);
        self.stats.overrun_frames = self.stats.overrun_frames.saturating_add(dropped as u64);
        if frames >= self.capacity_frames {
            data = &data[(frames - self.capacity_frames) * frame_bytes..];
            self.head_frame = 0;
            self.queued_frames = 0;
        } else if dropped != 0 {
            self.head_frame = (self.head_frame + dropped) % self.capacity_frames;
            self.queued_frames -= dropped;
        }
        let tail = (self.head_frame + self.queued_frames) % self.capacity_frames * frame_bytes;
        let first = data.len().min(self.storage.len() - tail);
        self.storage[tail..tail + first].copy_from_slice(&data[..first]);
        self.storage[..data.len() - first].copy_from_slice(&data[first..]);
        self.queued_frames += data.len() / frame_bytes;
    }

    /// Copy whole frames, returning their count. Zero means active but empty.
    /// Stop/failure are distinct errors even if old frames remain queued.
    pub fn read(&mut self, destination: &mut [u8]) -> Result<usize, CaptureError> {
        self.require_active()?;
        let frame_bytes = self.format.bytes_per_frame();
        if destination.len() % frame_bytes != 0 {
            return Err(CaptureError::PartialFrame);
        }
        let frames = self.queued_frames.min(destination.len() / frame_bytes);
        let bytes = frames * frame_bytes;
        let start = self.head_frame * frame_bytes;
        let first = bytes.min(self.storage.len() - start);
        destination[..first].copy_from_slice(&self.storage[start..start + first]);
        destination[first..bytes].copy_from_slice(&self.storage[..bytes - first]);
        self.head_frame = (self.head_frame + frames) % self.capacity_frames;
        self.queued_frames -= frames;
        self.stats.delivered_frames = self.stats.delivered_frames.saturating_add(frames as u64);
        Ok(frames)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn format(bits: u8, subslot: u8, channels: u16) -> CaptureFormat {
        CaptureFormat::pcm(
            48_000,
            bits,
            subslot,
            ChannelMap::uac2(channels, 0).unwrap(),
        )
        .unwrap()
    }

    fn packet(requested_bytes: usize, actual_bytes: usize, succeeded: bool) -> CapturePacket {
        CapturePacket {
            requested_bytes,
            actual_bytes,
            succeeded,
        }
    }

    fn feed(queue: &mut CaptureBuffer, bytes: &[u8]) {
        queue
            .ingest_completion(
                bytes,
                &[bytes.len()],
                &[packet(bytes.len(), bytes.len(), true)],
                true,
                0,
            )
            .unwrap();
    }

    #[test]
    fn channel_masks_preserve_usb_order_and_unknown_indices() {
        use ChannelPosition::*;
        assert_eq!(
            ChannelMap::uac1(5, (1 << 0) | (1 << 2) | (1 << 11))
                .unwrap()
                .positions(),
            &[
                FrontLeft,
                FrontCenter,
                TopCenter,
                Unspecified { index: 3 },
                Unspecified { index: 4 }
            ]
        );
        assert_eq!(
            ChannelMap::uac2(3, (1 << 26) | (1 << 20) | (1 << 12))
                .unwrap()
                .positions(),
            &[TopFrontLeft, LeftLowFrequency, BackRightOfCenter]
        );
        assert_eq!(
            ChannelMap::uac2(2, 0x8000_0000).unwrap().positions(),
            &[Unspecified { index: 0 }, Unspecified { index: 1 }]
        );
        assert_eq!(ChannelMap::uac1(1, 3), Err(CaptureError::InvalidChannelMap));
        assert_eq!(
            ChannelMap::uac1(1, 1 << 12),
            Err(CaptureError::InvalidChannelMap)
        );
        assert_eq!(
            ChannelMap::uac2(1, 1 << 27),
            Err(CaptureError::InvalidChannelMap)
        );
        assert_eq!(
            ChannelMap::uac2(2, 0x8000_0001),
            Err(CaptureError::InvalidChannelMap)
        );
    }

    #[test]
    fn exact_request_includes_channel_identity_and_wire_padding() {
        let stereo = ChannelMap::uac2(2, 3).unwrap();
        let pcm = CaptureFormat::pcm(48_000, 24, 4, stereo.clone()).unwrap();
        let mut request = CaptureRequest {
            sample_rate: 48_000,
            channels: 2,
            valid_bits: 24,
            channel_map: Some(stereo),
        };
        assert_eq!(pcm.bytes_per_frame(), 8);
        assert!(pcm.satisfies(&request));
        request.channel_map = Some(ChannelMap::uac2(2, 0).unwrap());
        assert!(!pcm.satisfies(&request));
        request.channel_map = None;
        request.sample_rate = 44_100;
        assert!(!pcm.satisfies(&request));
        assert_eq!(
            CaptureFormat::pcm(48_000, 25, 3, ChannelMap::uac2(1, 0).unwrap()),
            Err(CaptureError::InvalidFormat)
        );
    }

    #[test]
    fn multichannel_vectors_keep_all_wire_bytes_across_packet_batches() {
        for (bits, subslot) in [(16, 2), (24, 3), (24, 4), (32, 4)] {
            let mut queue = CaptureBuffer::new(format(bits, subslot, 6), 8).unwrap();
            let frame_bytes = queue.format().bytes_per_frame();
            // Every channel, padding byte, and frame has distinct input bytes.
            let input: Vec<u8> = (0..frame_bytes * 5).map(|n| n as u8).collect();
            let first = frame_bytes * 2;
            feed(&mut queue, &input[..first]);
            feed(&mut queue, &input[first..]);
            let mut output = vec![0; input.len()];
            assert_eq!(queue.read(&mut output).unwrap(), 5);
            assert_eq!(output, input);
            assert_eq!(queue.stats().captured_frames, 5);
            assert_eq!(queue.stats().delivered_frames, 5);
        }
    }

    #[test]
    fn wrap_and_host_backlog_drop_oldest_complete_frames() {
        let mut queue = CaptureBuffer::new(format(16, 2, 1), 3).unwrap();
        feed(&mut queue, &[1, 1, 2, 2, 3, 3]);
        assert_eq!(queue.read(&mut [0; 2]).unwrap(), 1);
        feed(&mut queue, &[4, 4, 5, 5]);
        let mut out = [0; 6];
        assert_eq!(queue.read(&mut out).unwrap(), 3);
        assert_eq!(out, [3, 3, 4, 4, 5, 5]);
        // One incoming packet is larger than the entire configured ring.
        feed(&mut queue, &[6, 6, 7, 7, 8, 8, 9, 9]);
        queue.read(&mut out).unwrap();
        assert_eq!(out, [7, 7, 8, 8, 9, 9]);
        assert_eq!(queue.stats().overrun_frames, 2);
        assert_eq!(queue.stats().missing_frames, 0);
        assert_eq!(queue.stats().device_position_frames(), 9);
    }

    #[test]
    fn short_and_empty_packets_advance_by_submitted_capacity() {
        let mut queue = CaptureBuffer::new(format(16, 2, 1), 8).unwrap();
        queue
            .ingest_completion(
                &[1, 2, 99, 99, 88, 88, 3, 4, 5, 6],
                &[4, 2, 4],
                &[packet(4, 2, true), packet(2, 0, true), packet(4, 4, true)],
                true,
                0,
            )
            .unwrap();
        let mut out = [0; 6];
        assert_eq!(queue.read(&mut out).unwrap(), 3);
        assert_eq!(out, [1, 2, 3, 4, 5, 6]);
        assert_eq!(queue.stats().short_packets, 1);
        assert_eq!(queue.stats().empty_packets, 1);
        assert_eq!(queue.stats().missing_frames, 0);
    }

    #[test]
    fn malformed_failed_and_transport_loss_are_separate_from_overrun() {
        let mut queue = CaptureBuffer::new(format(16, 2, 1), 8).unwrap();
        queue
            .ingest_completion(
                &[0; 10],
                &[2, 2, 2, 2, 2],
                &[
                    packet(2, 1, true),
                    packet(2, 4, true),
                    packet(2, 0, false),
                    packet(2, 0, true),
                    packet(2, 2, true),
                ],
                true,
                7,
            )
            .unwrap();
        queue.ingest_completion(&[], &[], &[], false, 3).unwrap();
        let stats = queue.stats();
        assert_eq!(stats.malformed_packets, 2);
        assert_eq!(stats.failed_packets, 1);
        assert_eq!(stats.empty_packets, 1);
        assert_eq!(stats.failed_transfers, 1);
        assert_eq!(stats.missing_frames, 10);
        assert_eq!(stats.captured_frames, 1);
        assert_eq!(stats.device_position_frames(), 11);
        assert_eq!(stats.overrun_frames, 0);
    }

    #[test]
    fn mutated_request_geometry_is_rejected_before_payload_delivery() {
        let mut queue = CaptureBuffer::new(format(16, 2, 1), 8).unwrap();
        for (data, lengths, packets) in [
            (
                &[0; 4][..],
                vec![2, 2],
                vec![packet(2, 2, true), packet(4, 2, true)],
            ),
            (
                &[0; 2][..],
                vec![2, 2],
                vec![packet(2, 2, true), packet(2, 2, true)],
            ),
            (&[0; 2][..], vec![2], vec![]),
            (
                &[][..],
                vec![usize::MAX, 1],
                vec![packet(usize::MAX, 0, true), packet(1, 0, true)],
            ),
        ] {
            assert_eq!(
                queue.ingest_completion(data, &lengths, &packets, true, 0),
                Err(CaptureError::InvalidCompletion)
            );
        }
        assert_eq!(queue.available_frames(), 0);
        assert_eq!(queue.stats().captured_frames, 0);
        assert_eq!(queue.stats().malformed_completions, 4);
    }

    #[test]
    fn active_empty_partial_read_and_terminal_states_are_distinct() {
        let mut queue = CaptureBuffer::new(format(16, 2, 1), 1).unwrap();
        assert_eq!(queue.read(&mut [0; 2]), Ok(0));
        feed(&mut queue, &[1, 2]);
        assert_eq!(queue.read(&mut [0; 1]), Err(CaptureError::PartialFrame));
        assert_eq!(queue.available_frames(), 1);
        queue.stop();
        assert_eq!(queue.read(&mut [0; 2]), Err(CaptureError::Stopped));
        assert_eq!(
            queue.ingest_completion(&[], &[], &[], true, 0),
            Err(CaptureError::Stopped)
        );
        queue.fail();
        assert_eq!(queue.read(&mut [0; 2]), Err(CaptureError::Failed));
    }
}
