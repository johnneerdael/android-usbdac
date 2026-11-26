//! What a DAC offers: the data types returned by
//! [`Dac::capabilities`](crate::Dac::capabilities).

use crate::volume::VolumeRange;

/// Identity of the USB device and the interfaces the host has claimed.
///
/// On Android the host gets the file descriptor from
/// `UsbDeviceConnection.getFileDescriptor()` after claiming the audio
/// interfaces with `claimInterface`; `claimed_interfaces` lists those interface
/// numbers. The library never claims or releases interfaces itself: it checks
/// that the host claimed exactly the AudioControl interface(s) and the
/// playback AudioStreaming interface(s) of the active configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceInfo {
    pub vendor_id: u16,
    pub product_id: u16,
    /// Product string (`UsbDevice.getProductName()`); used for quirk matching.
    pub product_name: String,
    pub manufacturer_name: Option<String>,
    /// Platform device name, for example `/dev/bus/usb/001/004`. Only used to
    /// key the remembered original volume.
    pub device_name: Option<String>,
    /// Interface numbers the host has claimed.
    pub claimed_interfaces: Vec<u8>,
}

impl DeviceInfo {
    pub fn new(vendor_id: u16, product_id: u16, product_name: impl Into<String>) -> Self {
        Self {
            vendor_id,
            product_id,
            product_name: product_name.into(),
            manufacturer_name: None,
            device_name: None,
            claimed_interfaces: Vec::new(),
        }
    }

    pub fn with_claimed_interfaces(mut self, interfaces: impl Into<Vec<u8>>) -> Self {
        self.claimed_interfaces = interfaces.into();
        self
    }

    pub fn with_device_name(mut self, name: impl Into<String>) -> Self {
        self.device_name = Some(name.into());
        self
    }

    pub fn with_manufacturer_name(mut self, name: impl Into<String>) -> Self {
        self.manufacturer_name = Some(name.into());
        self
    }
}

/// USB Audio Class version of the device's interfaces.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UacVersion {
    V1,
    V2,
    Unknown,
}

/// Bus speed the device is connected at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkSpeed {
    Low,
    Full,
    High,
    Super,
    SuperPlus,
    Unknown,
}

/// Synchronisation type of an isochronous data endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncKind {
    /// The DAC runs its own clock; an explicit feedback endpoint, when
    /// present, tells the host the true rate.
    Asynchronous,
    /// The DAC follows the host's packet timing.
    Adaptive,
    Synchronous,
    None,
}

/// The usage bits of an isochronous endpoint's `bmAttributes`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndpointUsage {
    /// A data endpoint.
    Data,
    /// A feedback endpoint.
    Feedback,
    /// An implicit feedback data endpoint (the feedback is carried by an
    /// associated IN data endpoint).
    FeedbackData,
    Reserved,
}

/// What an alternate setting carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AltFormat {
    /// Linear PCM (also the carrier for DoP).
    Pcm,
    /// Native DSD (format tag DSD).
    Dsd,
    /// Anything else (float, PCM8, ...); never selected.
    Other,
}

/// A continuous or discrete range of sampling frequencies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RateRange {
    pub min_hz: u32,
    pub max_hz: u32,
    /// Step between supported values; 0 for a single discrete rate.
    pub step_hz: u32,
}

/// One isochronous OUT alternate setting of an AudioStreaming interface.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct AltSetting {
    pub interface_number: u8,
    pub alt_setting: u8,
    pub endpoint_address: u8,
    pub format: AltFormat,
    pub channels: u16,
    /// Bytes per sample on the wire (the UAC `bSubslotSize`).
    pub subslot_bytes: u8,
    /// Significant bits per sample (the UAC `bBitResolution`).
    pub valid_bits: u8,
    pub rate_ranges: Vec<RateRange>,
    /// A representative list of the supported rates (the common audio rates
    /// inside `rate_ranges`, or the discrete rates themselves).
    pub sample_rates: Vec<u32>,
    pub sync_type: SyncKind,
    /// Address of the explicit feedback endpoint tied to this alternate
    /// setting, if it has one.
    pub feedback_endpoint: Option<u8>,
    pub max_packet_bytes: usize,
    pub service_interval_us: u32,
    pub uac_version: UacVersion,
    /// The endpoint's `bInterval`.
    pub endpoint_interval: u8,
    /// The endpoint's usage bits.
    pub usage: EndpointUsage,
    /// The endpoint's `bRefresh`.
    pub refresh: u8,
    /// The endpoint's `bSynchAddress`: the feedback endpoint it names, or 0.
    /// [`feedback_endpoint`](Self::feedback_endpoint) can be set while this is 0
    /// (a feedback endpoint found by its usage bits).
    pub synch_address: u8,
}

/// Summary of what the DAC can do.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct Capabilities {
    pub uac_version: UacVersion,
    pub link_speed: LinkSpeed,
    pub alt_settings: Vec<AltSetting>,
    /// Union of the PCM rates over all PCM alternate settings.
    pub pcm_sample_rates: Vec<u32>,
    /// Union of the PCM valid-bit depths.
    pub pcm_bit_depths: Vec<u8>,
    pub channel_counts: Vec<u16>,
    pub supports_pcm: bool,
    /// A PCM alternate setting with at least 24 valid bits that runs at
    /// 176.4 kHz or faster exists, which is what DoP64 needs.
    pub supports_dop: bool,
    /// Highest DoP DSD rate (bits per second per channel, e.g. 2822400 for
    /// DSD64) the PCM carrier can take, if DoP is supported.
    pub dop_max_dsd_bit_rate: Option<u32>,
    pub supports_native_dsd: bool,
    /// DSD rates (bits per second per channel) the native DSD alternate
    /// settings accept, derived from their wire rates and subslot sizes.
    pub native_dsd_bit_rates: Vec<u32>,
    /// At least one asynchronous alternate setting has an explicit feedback
    /// endpoint.
    pub has_explicit_feedback: bool,
    /// The DAC's hardware volume control, if it has a writable one.
    pub volume: Option<VolumeRange>,
    /// The DAC has a usable hardware volume control (`volume.is_some()`). A
    /// DAC that has none is refused by `open_stream` unless
    /// `DacConfig::require_hardware_volume` is off.
    pub has_hardware_volume: bool,
    pub has_mute: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_info_builder_sets_the_optional_fields() {
        let info = DeviceInfo::new(1, 2, "Dac")
            .with_claimed_interfaces(vec![0u8, 1, 2])
            .with_device_name("/dev/bus/usb/001/002")
            .with_manufacturer_name("Acme");
        assert_eq!(info.claimed_interfaces, vec![0, 1, 2]);
        assert_eq!(info.device_name.as_deref(), Some("/dev/bus/usb/001/002"));
        assert_eq!(info.manufacturer_name.as_deref(), Some("Acme"));
    }
}
