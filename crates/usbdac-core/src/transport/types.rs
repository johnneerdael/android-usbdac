use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DsdTransportMode {
    None,
    DoP,
    Native,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PlaybackFormat {
    pub sample_rate: u32,
    pub bit_depth: u8,
    pub channels: u16,
    pub is_dop: bool,
    pub dsd_transport: DsdTransportMode,
    /// DSD 1-bit sample rate (e.g. 2_822_400 for DSD64). Set only when
    /// dsd_transport == Native. The wire rate is computed as
    /// `dsd_bit_rate / (8 * subslot_size)` after the alt-setting is chosen.
    pub dsd_bit_rate: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum DacMode {
    FullControl,
    FixedClock,
    AdaptiveStreaming,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum EngineState {
    Idle,
    UsbInit,
    UsbReady,
    Streaming,
    Error,
    Fallback,
}

#[derive(Debug, Clone)]
pub(super) struct LifecycleState {
    pub(super) engine_state: EngineState,
    pub(super) reason: Option<String>,
}

impl Default for LifecycleState {
    fn default() -> Self {
        Self {
            engine_state: EngineState::Idle,
            reason: None,
        }
    }
}


#[derive(Debug, Clone)]
pub(super) struct SessionState {
    pub(super) device: UsbDevice,
    pub(super) requested_playback_format: Option<PlaybackFormat>,
    pub(super) playback_format: Option<PlaybackFormat>,
    pub(super) lock_requested: bool,
    pub(super) stream_active: bool,
    pub(super) capability_model: Option<CapabilityModel>,
    pub(super) clock_status: Option<ClockStatus>,
    pub(super) clock_control_attempted: bool,
    pub(super) clock_control_succeeded: bool,
    pub(super) clock_verification_passed: bool,
    pub(super) dac_mode: DacMode,
    pub(super) last_error: Option<String>,
    pub(super) direct_mode_refusal_reason: Option<String>,
    pub(super) usb_stream_stable: bool,
    pub(super) bit_perfect_verified: bool,
    pub(super) software_volume_active: bool,
    /// The open stream runs an asynchronous endpoint that has no explicit
    /// feedback endpoint (`DacConfig::allow_unsynchronized_async`): host-paced
    /// at the nominal rate, never bit-perfect.
    pub(super) unsynchronized_async: bool,
    pub(super) hardware_volume_control: Option<HardwareVolumeControl>,
    pub(super) hardware_volume_generation: u64,
    pub(super) hardware_volume_error: Option<String>,
    /// The last level read from or written to the DAC.
    pub(super) hardware_volume_state: Option<crate::volume::VolumeState>,
    /// What the start-up volume step did, for the route it ran on.
    pub(super) startup_volume: Option<StartupVolumeStatus>,
    pub(super) runtime_stats: Option<Arc<RuntimeStats>>,
}

#[derive(Debug, Clone)]
pub(super) struct ClockStatus {
    pub(super) reported_sample_rate: Option<u32>,
}

#[derive(Debug, Clone)]
pub(super) struct ClockApplyOutcome {
    pub(super) clock_ok: bool,
    pub(super) rate_verified: bool,
    pub(super) reported_sample_rate: Option<u32>,
    pub(super) known_mismatch: bool,
    pub(super) set_cur_succeeded: bool,
    pub(super) message: Option<String>,
}

#[derive(Debug, Clone)]
pub(super) struct HardwareVolumeControl {
    pub(super) interface_number: u8,
    pub(super) feature_unit_id: u8,
    pub(super) protocol: AudioControlProtocol,
    pub(super) volume_channels: Vec<u16>,
    pub(super) ranges: Vec<RawRange>,
    pub(super) mute_channel: Option<u16>,
    pub(super) min_volume_raw: i16,
    pub(super) max_volume_raw: i16,
    pub(super) resolution_raw: i16,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct CapabilityModel {
    pub(super) supported_sample_rates: Vec<u32>,
    pub(super) alt_settings: Vec<crate::capabilities::AltSetting>,
    pub(super) link_speed: Option<crate::capabilities::LinkSpeed>,
}

#[derive(Debug)]
pub(super) struct IsoStreamCandidate {
    pub(super) interface_number: u8,
    pub(super) alt_setting: u8,
    pub(super) endpoint_address: u8,
    pub(super) endpoint_interval: u8,
    pub(super) service_interval_us: u32,
    pub(super) usb_speed: Speed,
    pub(super) max_packet_bytes: usize,
    pub(super) sync_type: SyncType,
    pub(super) usage_type: UsageType,
    pub(super) format_tag: u16,
    pub(super) channels: u16,
    pub(super) subslot_size: u8,
    pub(super) bit_resolution: u8,
    pub(super) sample_rates: Vec<u32>,
    pub(super) refresh: u8,
    pub(super) synch_address: u8,
    pub(super) feedback_endpoint: Option<FeedbackEndpoint>,
    /// bTerminalLink from the AS_GENERAL descriptor, used to trace the
    /// UAC2 topology to the correct Clock Entity.
    pub(super) terminal_link: Option<u8>,
    pub(super) is_uac1: bool,
}

#[derive(Debug, Clone, Copy)]
pub(super) struct FeedbackEndpoint {
    pub(super) address: u8,
    pub(super) transfer_type: TransferType,
    pub(super) interval: u8,
    pub(super) service_interval_us: u32,
    pub(super) max_packet_bytes: usize,
}

#[derive(Debug, Clone, Copy)]
pub(super) struct AudioControlClock {
    pub(super) interface_number: u8,
    pub(super) clock_id: u8,
}

#[derive(Debug, Clone, Copy)]
pub(super) struct SamplingFrequencySubrange {
    pub(super) min: u32,
    pub(super) max: u32,
    pub(super) res: u32,
}

#[derive(Debug, Clone)]
pub(super) struct StreamingInterfaceFormat {
    pub(super) format_tag: u16,
    pub(super) channels: u16,
    pub(super) subslot_size: u8,
    pub(super) bit_resolution: u8,
    pub(super) terminal_link: Option<u8>,
    pub(super) is_uac1: bool,
    pub(super) sample_rate_ranges: Vec<SamplingFrequencySubrange>,
}


/// Why no alternate setting could be selected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SelectError {
    /// Nothing carries the requested format.
    NoMatch(String),
    /// The only alternate settings that carry it are asynchronous endpoints with
    /// no explicit feedback endpoint, which would need implicit feedback, and
    /// `allow_unsynchronized_async` is off.
    ImplicitFeedback(String),
}

impl SelectError {
    pub(crate) fn message(&self) -> &str {
        match self {
            Self::NoMatch(message) | Self::ImplicitFeedback(message) => message,
        }
    }

    pub(crate) fn into_error(self, offered: Vec<crate::capabilities::AltSetting>) -> crate::Error {
        match self {
            Self::NoMatch(reason) => crate::Error::Unsupported { reason, offered },
            Self::ImplicitFeedback(_) => crate::Error::ImplicitFeedbackUnsupported,
        }
    }
}

/// What to do with an isochronous OUT endpoint, by how it is synchronised.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EndpointAdmission {
    Admit,
    /// Declares a feedback endpoint that could not be resolved: malformed, never
    /// streamed.
    SkipMalformed,
    /// Asynchronous with no explicit feedback endpoint (it would need implicit
    /// feedback) and the configuration does not allow streaming it at the
    /// nominal rate.
    RefuseUnsynchronized,
}

/// The admission rule for an OUT endpoint. Only an asynchronous endpoint needs a
/// feedback endpoint to be paced correctly: adaptive and synchronous endpoints
/// follow the host's packet timing.
pub(crate) fn admit_endpoint(
    sync_type: SyncType,
    synch_address: u8,
    feedback_resolved: bool,
    allow_unsynchronized_async: bool,
) -> EndpointAdmission {
    if feedback_resolved {
        return EndpointAdmission::Admit;
    }
    if synch_address != 0 {
        return EndpointAdmission::SkipMalformed;
    }
    if sync_type == SyncType::Asynchronous && !allow_unsynchronized_async {
        return EndpointAdmission::RefuseUnsynchronized;
    }
    EndpointAdmission::Admit
}
