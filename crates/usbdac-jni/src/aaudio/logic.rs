//! The pure logic of the AAudio output: sample formats and the Android release each one needs,
//! what the platform granted against what was asked for, how an AAudio result code maps to the
//! library's error codes, and how the audible position is estimated from AAudio's counters.
//!
//! Nothing here touches the platform, so all of it is unit-tested on the host.

use serde_json::{json, Value};

// ---- AAudio constants (the values of `aaudio/AAudio.h`) -------------------------------------

pub const AAUDIO_OK: i32 = 0;

pub const FORMAT_PCM_I16: i32 = 1;
pub const FORMAT_PCM_FLOAT: i32 = 2;
pub const FORMAT_PCM_I24_PACKED: i32 = 3;
pub const FORMAT_PCM_I32: i32 = 4;

pub const SHARING_EXCLUSIVE: i32 = 0;
#[cfg(test)]
pub const SHARING_SHARED: i32 = 1;

pub const PERFORMANCE_NONE: i32 = 10;
pub const PERFORMANCE_POWER_SAVING: i32 = 11;
pub const PERFORMANCE_LOW_LATENCY: i32 = 12;

pub const STATE_STARTED: i32 = 4;
pub const STATE_STOPPED: i32 = 10;
pub const STATE_DISCONNECTED: i32 = 13;

pub const ERROR_DISCONNECTED: i32 = -899;
pub const ERROR_ILLEGAL_ARGUMENT: i32 = -898;
pub const ERROR_INTERNAL: i32 = -896;
pub const ERROR_INVALID_STATE: i32 = -895;
pub const ERROR_INVALID_HANDLE: i32 = -892;
pub const ERROR_UNIMPLEMENTED: i32 = -890;
pub const ERROR_UNAVAILABLE: i32 = -889;
pub const ERROR_NO_FREE_HANDLES: i32 = -888;
pub const ERROR_NO_MEMORY: i32 = -887;
pub const ERROR_NULL: i32 = -886;
pub const ERROR_TIMEOUT: i32 = -885;
pub const ERROR_WOULD_BLOCK: i32 = -884;
pub const ERROR_INVALID_FORMAT: i32 = -883;
pub const ERROR_OUT_OF_RANGE: i32 = -882;
pub const ERROR_NO_SERVICE: i32 = -881;
pub const ERROR_INVALID_RATE: i32 = -880;

/// The first Android release with AAudio (Android 8.0); the SDK's minimum.
pub const API_AAUDIO: u32 = 26;
/// The first release that has 24-bit packed and 32-bit integer AAudio formats (Android 12).
pub const API_PACKED_24_AND_32: u32 = 31;

/// Event types sent to the Kotlin listener `onEvent(type, a, b, message)`.
pub const EVENT_DISCONNECTED: i32 = 0;
pub const EVENT_FAILED: i32 = 1;

// ---- errors ---------------------------------------------------------------------------------

/// A failure with one of the library's error codes (`unsupported`, `device_lost`, ...), thrown to
/// Java as `usbdac:<code>: <message>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AaudioError {
    pub code: &'static str,
    pub message: String,
}

impl AaudioError {
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    pub fn invalid_argument(message: impl Into<String>) -> Self {
        Self::new("invalid_argument", message)
    }

    pub fn invalid_state(message: impl Into<String>) -> Self {
        Self::new("invalid_state", message)
    }

    pub fn unsupported(message: impl Into<String>) -> Self {
        Self::new("unsupported", message)
    }

    pub fn device_lost(message: impl Into<String>) -> Self {
        Self::new("device_lost", message)
    }

    #[cfg_attr(target_os = "android", allow(dead_code))]
    pub fn platform_unsupported() -> Self {
        Self::new(
            "platform_unsupported",
            "AAudio exists on Android only (API 26 or later)",
        )
    }
}

/// The name of an AAudio result code, for messages.
pub fn result_name(result: i32) -> &'static str {
    match result {
        AAUDIO_OK => "OK",
        ERROR_DISCONNECTED => "DISCONNECTED",
        ERROR_ILLEGAL_ARGUMENT => "ILLEGAL_ARGUMENT",
        ERROR_INTERNAL => "INTERNAL",
        ERROR_INVALID_STATE => "INVALID_STATE",
        ERROR_INVALID_HANDLE => "INVALID_HANDLE",
        ERROR_UNIMPLEMENTED => "UNIMPLEMENTED",
        ERROR_UNAVAILABLE => "UNAVAILABLE",
        ERROR_NO_FREE_HANDLES => "NO_FREE_HANDLES",
        ERROR_NO_MEMORY => "NO_MEMORY",
        ERROR_NULL => "NULL",
        ERROR_TIMEOUT => "TIMEOUT",
        ERROR_WOULD_BLOCK => "WOULD_BLOCK",
        ERROR_INVALID_FORMAT => "INVALID_FORMAT",
        ERROR_OUT_OF_RANGE => "OUT_OF_RANGE",
        ERROR_NO_SERVICE => "NO_SERVICE",
        ERROR_INVALID_RATE => "INVALID_RATE",
        _ => "UNKNOWN",
    }
}

/// Maps a negative AAudio result to the library's error codes. `context` says what was being
/// done ("opening the stream", "starting the stream").
pub fn error_for_result(context: &str, result: i32) -> AaudioError {
    let code = match result {
        ERROR_DISCONNECTED => "device_lost",
        ERROR_ILLEGAL_ARGUMENT | ERROR_OUT_OF_RANGE | ERROR_NULL => "invalid_argument",
        ERROR_INVALID_STATE | ERROR_INVALID_HANDLE => "invalid_state",
        ERROR_UNIMPLEMENTED | ERROR_INVALID_FORMAT | ERROR_INVALID_RATE => "unsupported",
        ERROR_UNAVAILABLE | ERROR_NO_FREE_HANDLES => "busy",
        _ => "transfer",
    };
    AaudioError::new(
        code,
        format!(
            "AAudio failed {context}: {} ({result})",
            result_name(result)
        ),
    )
}

// ---- sample formats ---------------------------------------------------------------------------

/// The sample formats the output carries without converting anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SampleFormat {
    /// Signed 16-bit little-endian.
    I16,
    /// Signed 24-bit little-endian in 3 bytes.
    I24Packed,
    /// Signed 32-bit little-endian.
    I32,
    /// 32-bit IEEE float.
    Float,
}

impl SampleFormat {
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "i16" => Some(Self::I16),
            "i24" => Some(Self::I24Packed),
            "i32" => Some(Self::I32),
            "float" => Some(Self::Float),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::I16 => "i16",
            Self::I24Packed => "i24",
            Self::I32 => "i32",
            Self::Float => "float",
        }
    }

    /// A phrase for messages.
    pub fn describe(self) -> &'static str {
        match self {
            Self::I16 => "16-bit integer",
            Self::I24Packed => "packed 24-bit integer",
            Self::I32 => "32-bit integer",
            Self::Float => "32-bit float",
        }
    }

    pub fn bytes_per_sample(self) -> usize {
        match self {
            Self::I16 => 2,
            Self::I24Packed => 3,
            Self::I32 | Self::Float => 4,
        }
    }

    /// The `aaudio_format_t` value.
    pub fn aaudio_value(self) -> i32 {
        match self {
            Self::I16 => FORMAT_PCM_I16,
            Self::Float => FORMAT_PCM_FLOAT,
            Self::I24Packed => FORMAT_PCM_I24_PACKED,
            Self::I32 => FORMAT_PCM_I32,
        }
    }

    pub fn from_aaudio(value: i32) -> Option<Self> {
        match value {
            FORMAT_PCM_I16 => Some(Self::I16),
            FORMAT_PCM_FLOAT => Some(Self::Float),
            FORMAT_PCM_I24_PACKED => Some(Self::I24Packed),
            FORMAT_PCM_I32 => Some(Self::I32),
            _ => None,
        }
    }

    /// The Android API level that has this format in AAudio.
    pub fn min_api_level(self) -> u32 {
        match self {
            Self::I16 | Self::Float => API_AAUDIO,
            Self::I24Packed | Self::I32 => API_PACKED_24_AND_32,
        }
    }
}

/// Describes a raw `aaudio_format_t` for messages, including ones this output does not carry.
pub fn describe_raw_format(value: i32) -> String {
    match SampleFormat::from_aaudio(value) {
        Some(format) => format.describe().to_string(),
        None => format!("format {value}"),
    }
}

/// Refuses a format the running Android release cannot open, with a typed error.
pub fn check_api_level(format: SampleFormat, api_level: u32) -> Result<(), AaudioError> {
    if api_level < API_AAUDIO {
        return Err(AaudioError::unsupported(format!(
            "AAudio needs Android 8.0 (API {API_AAUDIO}); this is API {api_level}"
        )));
    }
    if api_level < format.min_api_level() {
        return Err(AaudioError::unsupported(format!(
            "{} samples through AAudio need Android 12 (API {}); this is API {api_level}",
            format.describe(),
            format.min_api_level()
        )));
    }
    Ok(())
}

// ---- the request ------------------------------------------------------------------------------

/// How the platform should schedule the stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Performance {
    None,
    PowerSaving,
    LowLatency,
}

impl Performance {
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "none" => Some(Self::None),
            "power_saving" => Some(Self::PowerSaving),
            "low_latency" => Some(Self::LowLatency),
            _ => None,
        }
    }

    pub fn aaudio_value(self) -> i32 {
        match self {
            Self::None => PERFORMANCE_NONE,
            Self::PowerSaving => PERFORMANCE_POWER_SAVING,
            Self::LowLatency => PERFORMANCE_LOW_LATENCY,
        }
    }
}

/// What `aaudioOpen` is asked for.
#[derive(Debug, Clone, PartialEq)]
pub struct OpenRequest {
    pub sample_rate: u32,
    pub channels: u32,
    pub format: SampleFormat,
    /// `AudioDeviceInfo.getId()`, or 0 for the platform's choice.
    pub device_id: i32,
    pub performance: Performance,
    /// The depth `writable_bytes` fills the ring to.
    pub buffer_target_ms: u32,
    /// The ring's capacity.
    pub buffer_capacity_ms: u32,
    /// Audio the ring must hold before playback starts or resumes.
    pub start_threshold_ms: u32,
    /// Size of the device-side buffer in bursts, or `None` for the platform's default.
    pub device_buffer_bursts: Option<u32>,
}

pub const MAX_CHANNELS: u32 = 8;
pub const MAX_SAMPLE_RATE: u32 = 768_000;

impl OpenRequest {
    pub fn from_json(text: &str) -> Result<Self, AaudioError> {
        let value: Value = serde_json::from_str(text).map_err(|error| {
            AaudioError::invalid_argument(format!("malformed request: {error}"))
        })?;
        let number = |key: &str, default: Option<u64>| -> Result<u64, AaudioError> {
            match value.get(key) {
                Some(Value::Null) | None => default.ok_or_else(|| {
                    AaudioError::invalid_argument(format!("the request has no \"{key}\""))
                }),
                Some(other) => other.as_u64().ok_or_else(|| {
                    AaudioError::invalid_argument(format!(
                        "\"{key}\" must be a non-negative integer"
                    ))
                }),
            }
        };
        let text_of = |key: &str, default: &str| -> String {
            value
                .get(key)
                .and_then(Value::as_str)
                .unwrap_or(default)
                .to_string()
        };
        let format_name = text_of("format", "");
        let format = SampleFormat::parse(&format_name).ok_or_else(|| {
            AaudioError::invalid_argument(format!(
                "\"format\" must be i16, i24, i32 or float, not \"{format_name}\""
            ))
        })?;
        let performance_name = text_of("performance_mode", "low_latency");
        let performance = Performance::parse(&performance_name).ok_or_else(|| {
            AaudioError::invalid_argument(format!(
                "\"performance_mode\" must be none, power_saving or low_latency, not \"{performance_name}\""
            ))
        })?;
        let request = Self {
            sample_rate: u32::try_from(number("sample_rate", None)?).unwrap_or(u32::MAX),
            channels: u32::try_from(number("channels", None)?).unwrap_or(u32::MAX),
            format,
            device_id: i32::try_from(number("device_id", Some(0))?).unwrap_or(0),
            performance,
            buffer_target_ms: u32::try_from(number("buffer_target_ms", Some(250))?)
                .unwrap_or(u32::MAX),
            buffer_capacity_ms: u32::try_from(number("buffer_capacity_ms", Some(500))?)
                .unwrap_or(u32::MAX),
            start_threshold_ms: u32::try_from(number("start_threshold_ms", Some(100))?)
                .unwrap_or(u32::MAX),
            device_buffer_bursts: match value.get("device_buffer_bursts") {
                Some(Value::Null) | None => None,
                Some(other) => Some(
                    other
                        .as_u64()
                        .and_then(|bursts| u32::try_from(bursts).ok())
                        .ok_or_else(|| {
                            AaudioError::invalid_argument(
                                "\"device_buffer_bursts\" must be a positive integer",
                            )
                        })?,
                ),
            },
        };
        request.validate()?;
        Ok(request)
    }

    pub fn validate(&self) -> Result<(), AaudioError> {
        if !(8_000..=MAX_SAMPLE_RATE).contains(&self.sample_rate) {
            return Err(AaudioError::invalid_argument(format!(
                "the sample rate {} Hz is outside 8000..={MAX_SAMPLE_RATE}",
                self.sample_rate
            )));
        }
        if !(1..=MAX_CHANNELS).contains(&self.channels) {
            return Err(AaudioError::invalid_argument(format!(
                "{} channels are outside 1..={MAX_CHANNELS}",
                self.channels
            )));
        }
        if self.buffer_target_ms == 0 || self.buffer_target_ms > self.buffer_capacity_ms {
            return Err(AaudioError::invalid_argument(
                "buffer_target_ms must be between 1 and buffer_capacity_ms",
            ));
        }
        if self.buffer_capacity_ms > 10_000 {
            return Err(AaudioError::invalid_argument(
                "buffer_capacity_ms must be at most 10000",
            ));
        }
        if self.start_threshold_ms == 0 || self.start_threshold_ms > self.buffer_target_ms {
            return Err(AaudioError::invalid_argument(
                "start_threshold_ms must be between 1 and buffer_target_ms",
            ));
        }
        if self.device_buffer_bursts == Some(0) {
            return Err(AaudioError::invalid_argument(
                "device_buffer_bursts must be at least 1",
            ));
        }
        Ok(())
    }

    pub fn frame_bytes(&self) -> usize {
        self.format.bytes_per_sample() * self.channels as usize
    }

    pub fn frames_for_ms(&self, ms: u32) -> u64 {
        u64::from(self.sample_rate) * u64::from(ms) / 1000
    }

    pub fn capability(&self) -> Capability {
        Capability {
            rate: self.sample_rate,
            channels: self.channels,
            format: self.format,
        }
    }
}

/// The part of the request the grant is compared with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capability {
    pub rate: u32,
    pub channels: u32,
    pub format: SampleFormat,
}

// ---- the grant --------------------------------------------------------------------------------

/// What the platform reports after opening the stream. `hardware` is `Some` on Android 14 and
/// later, where AAudio says what the device itself runs at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Grant {
    pub sharing: i32,
    pub sample_rate: i32,
    pub channels: i32,
    pub format: i32,
    pub frames_per_burst: i32,
    pub buffer_capacity_frames: i32,
    pub buffer_size_frames: i32,
    pub device_id: i32,
    pub performance_mode: i32,
    /// `Some(true)` when the stream uses the memory-mapped (MMAP) path; `None` below Android 9.
    pub mmap: Option<bool>,
    pub hardware: Option<Hardware>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hardware {
    pub sample_rate: i32,
    pub channels: i32,
    /// `aaudio_format_t`; `-1` (`AAUDIO_FORMAT_INVALID`) when AAudio cannot name it.
    pub format: i32,
}

impl Grant {
    pub fn is_exclusive(&self) -> bool {
        self.sharing == SHARING_EXCLUSIVE
    }
}

/// One way the grant falls short of what was asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Finding {
    /// The stream is shared: the system mixer is in the path.
    Shared,
    RateChanged {
        requested: u32,
        granted: i32,
    },
    FormatChanged {
        requested: SampleFormat,
        granted: i32,
    },
    ChannelsChanged {
        requested: u32,
        granted: i32,
    },
    /// The hardware runs another rate than the stream: AAudio converts.
    HardwareRate {
        stream: i32,
        hardware: i32,
    },
    HardwareFormat {
        stream: i32,
        hardware: i32,
    },
    HardwareChannels {
        stream: i32,
        hardware: i32,
    },
}

impl Finding {
    /// A stable identifier.
    pub fn code(&self) -> &'static str {
        match self {
            Self::Shared => "shared",
            Self::RateChanged { .. } => "rate_changed",
            Self::FormatChanged { .. } => "format_changed",
            Self::ChannelsChanged { .. } => "channels_changed",
            Self::HardwareRate { .. } => "hardware_rate",
            Self::HardwareFormat { .. } => "hardware_format",
            Self::HardwareChannels { .. } => "hardware_channels",
        }
    }

    /// Whether the audio the app writes cannot be played as it is written: the output refuses to
    /// start such a stream.
    pub fn breaks_stream(&self) -> bool {
        matches!(
            self,
            Self::RateChanged { .. } | Self::FormatChanged { .. } | Self::ChannelsChanged { .. }
        )
    }

    pub fn describe(&self) -> String {
        match self {
            Self::Shared => "the platform granted shared mode, not exclusive: the audio goes through \
                the system mixer, which can resample, mix it with other sounds and apply volume and effects"
                .to_string(),
            Self::RateChanged { requested, granted } => format!(
                "the stream runs at {granted} Hz, not the requested {requested} Hz, so it would play at the wrong speed"
            ),
            Self::FormatChanged { requested, granted } => format!(
                "the stream's format is {}, not the requested {}, so the bytes would be misread",
                describe_raw_format(*granted),
                requested.describe()
            ),
            Self::ChannelsChanged { requested, granted } => format!(
                "the stream has {granted} channels, not the requested {requested}"
            ),
            Self::HardwareRate { stream, hardware } => format!(
                "the hardware runs at {hardware} Hz while the stream is at {stream} Hz: AAudio resamples before the device"
            ),
            Self::HardwareFormat { stream, hardware } => format!(
                "the hardware takes {} while the stream carries {}: AAudio converts the samples before the device",
                describe_raw_format(*hardware),
                describe_raw_format(*stream)
            ),
            Self::HardwareChannels { stream, hardware } => format!(
                "the hardware has {hardware} channels while the stream has {stream}: AAudio remaps the channels before the device"
            ),
        }
    }
}

/// The bit-perfect verdict for a grant. It is never "verified": an app cannot see what the
/// device's audio HAL does with the samples.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verdict {
    pub exclusive: bool,
    /// Exclusive, and nothing in [`Verdict::findings`].
    pub as_requested: bool,
    pub findings: Vec<Finding>,
    pub reason: String,
}

impl Verdict {
    /// The code of the most serious finding, or `"exclusive"`.
    pub fn code(&self) -> &'static str {
        self.findings.first().map_or("exclusive", Finding::code)
    }

    pub fn breaks_stream(&self) -> Option<&Finding> {
        self.findings.iter().find(|finding| finding.breaks_stream())
    }
}

/// Compares what the platform granted with what was requested, and says why the result is not
/// verifiable as bit-perfect.
pub fn evaluate(request: &Capability, grant: &Grant) -> Verdict {
    let mut findings = Vec::new();
    if !grant.is_exclusive() {
        findings.push(Finding::Shared);
    }
    if grant.sample_rate != request.rate as i32 {
        findings.push(Finding::RateChanged {
            requested: request.rate,
            granted: grant.sample_rate,
        });
    }
    if grant.format != request.format.aaudio_value() {
        findings.push(Finding::FormatChanged {
            requested: request.format,
            granted: grant.format,
        });
    }
    if grant.channels != request.channels as i32 {
        findings.push(Finding::ChannelsChanged {
            requested: request.channels,
            granted: grant.channels,
        });
    }
    if let Some(hardware) = grant.hardware {
        if hardware.sample_rate > 0 && hardware.sample_rate != grant.sample_rate {
            findings.push(Finding::HardwareRate {
                stream: grant.sample_rate,
                hardware: hardware.sample_rate,
            });
        }
        if hardware.format != -1 && hardware.format != grant.format {
            findings.push(Finding::HardwareFormat {
                stream: grant.format,
                hardware: hardware.format,
            });
        }
        if hardware.channels > 0 && hardware.channels != grant.channels {
            findings.push(Finding::HardwareChannels {
                stream: grant.channels,
                hardware: hardware.channels,
            });
        }
    }
    let as_requested = findings.is_empty();
    let reason = if as_requested {
        let mut reason = String::from(
            "Exclusive mode was granted at the requested rate, format and channel count: the app mixer \
             is bypassed, but the device's audio HAL is not verifiable from an app.",
        );
        if grant.hardware.is_some() {
            reason.push_str(" The hardware reports the same rate, format and channel count.");
        }
        reason
    } else {
        let mut parts = findings.iter().map(Finding::describe);
        let first = parts.next().unwrap_or_default();
        let mut reason = capitalize(&first);
        for part in parts {
            reason.push_str("; ");
            reason.push_str(&part);
        }
        reason.push('.');
        reason
    };
    Verdict {
        exclusive: grant.is_exclusive(),
        as_requested,
        findings,
        reason,
    }
}

fn capitalize(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

/// The grant and its verdict as the JSON `aaudioGrant` returns.
pub fn grant_json(request: &Capability, grant: &Grant) -> Value {
    let verdict = evaluate(request, grant);
    json!({
        "sharing": if grant.is_exclusive() { "exclusive" } else { "shared" },
        "sample_rate": grant.sample_rate,
        "channels": grant.channels,
        "format": SampleFormat::from_aaudio(grant.format).map(SampleFormat::name),
        "format_raw": grant.format,
        "frames_per_burst": grant.frames_per_burst,
        "buffer_capacity_frames": grant.buffer_capacity_frames,
        "buffer_size_frames": grant.buffer_size_frames,
        "device_id": grant.device_id,
        "performance_mode": match grant.performance_mode {
            PERFORMANCE_LOW_LATENCY => "low_latency",
            PERFORMANCE_POWER_SAVING => "power_saving",
            PERFORMANCE_NONE => "none",
            _ => "unknown",
        },
        "mmap": grant.mmap,
        "hardware": grant.hardware.map(|hardware| json!({
            "sample_rate": hardware.sample_rate,
            "channels": hardware.channels,
            "format": SampleFormat::from_aaudio(hardware.format).map(SampleFormat::name),
            "format_raw": hardware.format,
        })),
        "as_requested": verdict.as_requested,
        "verdict": verdict.code(),
        "reason": verdict.reason,
    })
}

// ---- the audible position ---------------------------------------------------------------------

/// A timestamp from `AAudioStream_getTimestamp`: the frame that was presented at `time_ns`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timestamp {
    pub frame: i64,
    pub time_ns: i64,
}

/// How far behind the hardware read position a timestamp may be before it is not believed.
const TIMESTAMP_SLACK_MS: i64 = 1_000;

/// The number of frames the device has presented since the stream started.
///
/// `frames_read` is what the device has taken from the stream's buffer; the audible position is
/// at or behind it. A timestamp gives the audible position at a moment, and is extrapolated to
/// `now_ns` at the stream's rate (only while the stream is running: a stopped stream's timestamp
/// would grow forever). A timestamp that is ahead of the read position, or more than a second
/// behind it, is not believed, and `frames_read` is used.
pub fn presented_position(
    timestamp: Option<Timestamp>,
    now_ns: i64,
    rate: u32,
    frames_read: i64,
    running: bool,
) -> i64 {
    let frames_read = frames_read.max(0);
    let Some(timestamp) = timestamp.filter(|_| running) else {
        return frames_read;
    };
    let elapsed_ns = i128::from(now_ns) - i128::from(timestamp.time_ns);
    let advanced = elapsed_ns * i128::from(rate) / 1_000_000_000;
    let estimate = i128::from(timestamp.frame) + advanced;
    let slack = i128::from(rate) * i128::from(TIMESTAMP_SLACK_MS) / 1000;
    if estimate < 0 || estimate < i128::from(frames_read) - slack {
        return frames_read;
    }
    estimate.min(i128::from(frames_read)) as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn want(rate: u32, channels: u32, format: SampleFormat) -> Capability {
        Capability {
            rate,
            channels,
            format,
        }
    }

    fn exclusive_grant(rate: i32, channels: i32, format: i32) -> Grant {
        Grant {
            sharing: SHARING_EXCLUSIVE,
            sample_rate: rate,
            channels,
            format,
            frames_per_burst: 96,
            buffer_capacity_frames: 960,
            buffer_size_frames: 192,
            device_id: 7,
            performance_mode: PERFORMANCE_LOW_LATENCY,
            mmap: Some(true),
            hardware: None,
        }
    }

    // ---- formats and the API levels they need ----

    #[test]
    fn formats_map_to_the_aaudio_values_and_back() {
        for (format, value) in [
            (SampleFormat::I16, 1),
            (SampleFormat::Float, 2),
            (SampleFormat::I24Packed, 3),
            (SampleFormat::I32, 4),
        ] {
            assert_eq!(format.aaudio_value(), value);
            assert_eq!(SampleFormat::from_aaudio(value), Some(format));
            assert_eq!(SampleFormat::parse(format.name()), Some(format));
        }
        assert_eq!(SampleFormat::from_aaudio(0), None);
        assert_eq!(SampleFormat::from_aaudio(5), None);
        assert_eq!(SampleFormat::parse("i8"), None);
    }

    #[test]
    fn bytes_per_sample_match_the_layouts() {
        assert_eq!(SampleFormat::I16.bytes_per_sample(), 2);
        assert_eq!(SampleFormat::I24Packed.bytes_per_sample(), 3);
        assert_eq!(SampleFormat::I32.bytes_per_sample(), 4);
        assert_eq!(SampleFormat::Float.bytes_per_sample(), 4);
    }

    #[test]
    fn packed_24_and_32_bit_need_android_12() {
        assert!(check_api_level(SampleFormat::I16, 26).is_ok());
        assert!(check_api_level(SampleFormat::Float, 26).is_ok());
        for format in [SampleFormat::I24Packed, SampleFormat::I32] {
            let error = check_api_level(format, 30).unwrap_err();
            assert_eq!(error.code, "unsupported");
            assert!(error.message.contains("API 31"), "{}", error.message);
            assert!(error.message.contains("API 30"), "{}", error.message);
            assert!(check_api_level(format, 31).is_ok());
            assert!(check_api_level(format, 36).is_ok());
        }
    }

    #[test]
    fn below_android_8_nothing_opens() {
        let error = check_api_level(SampleFormat::I16, 25).unwrap_err();
        assert_eq!(error.code, "unsupported");
        assert!(error.message.contains("API 26"));
    }

    // ---- the request ----

    #[test]
    fn a_minimal_request_gets_the_defaults() {
        let request =
            OpenRequest::from_json(r#"{"sample_rate":44100,"channels":2,"format":"i16"}"#).unwrap();
        assert_eq!(request.sample_rate, 44_100);
        assert_eq!(request.device_id, 0);
        assert_eq!(request.performance, Performance::LowLatency);
        assert_eq!(request.buffer_target_ms, 250);
        assert_eq!(request.buffer_capacity_ms, 500);
        assert_eq!(request.start_threshold_ms, 100);
        assert_eq!(request.device_buffer_bursts, None);
        assert_eq!(request.frame_bytes(), 4);
        assert_eq!(request.frames_for_ms(250), 11_025);
    }

    #[test]
    fn a_full_request_is_read() {
        let request = OpenRequest::from_json(
            r#"{"sample_rate":192000,"channels":2,"format":"i24","device_id":12,
                "performance_mode":"none","buffer_target_ms":100,"buffer_capacity_ms":200,
                "start_threshold_ms":50,"device_buffer_bursts":4}"#,
        )
        .unwrap();
        assert_eq!(request.format, SampleFormat::I24Packed);
        assert_eq!(request.device_id, 12);
        assert_eq!(request.performance, Performance::None);
        assert_eq!(request.device_buffer_bursts, Some(4));
        assert_eq!(request.frame_bytes(), 6);
    }

    #[test]
    fn bad_requests_are_invalid_arguments() {
        for text in [
            "not json",
            r#"{"channels":2,"format":"i16"}"#,
            r#"{"sample_rate":44100,"channels":2,"format":"u8"}"#,
            r#"{"sample_rate":44100,"channels":0,"format":"i16"}"#,
            r#"{"sample_rate":44100,"channels":9,"format":"i16"}"#,
            r#"{"sample_rate":100,"channels":2,"format":"i16"}"#,
            r#"{"sample_rate":44100,"channels":2,"format":"i16","buffer_target_ms":600}"#,
            r#"{"sample_rate":44100,"channels":2,"format":"i16","start_threshold_ms":300}"#,
            r#"{"sample_rate":44100,"channels":2,"format":"i16","device_buffer_bursts":0}"#,
            r#"{"sample_rate":44100,"channels":2,"format":"i16","performance_mode":"warp"}"#,
            r#"{"sample_rate":-1,"channels":2,"format":"i16"}"#,
        ] {
            let error = OpenRequest::from_json(text).unwrap_err();
            assert_eq!(error.code, "invalid_argument", "{text}: {}", error.message);
        }
    }

    // ---- the grant ----

    #[test]
    fn exclusive_at_the_requested_format_is_not_verifiable_but_says_so_precisely() {
        let verdict = evaluate(
            &want(96_000, 2, SampleFormat::I24Packed),
            &exclusive_grant(96_000, 2, FORMAT_PCM_I24_PACKED),
        );
        assert!(verdict.exclusive);
        assert!(verdict.as_requested);
        assert!(verdict.findings.is_empty());
        assert_eq!(verdict.code(), "exclusive");
        assert_eq!(
            verdict.reason,
            "Exclusive mode was granted at the requested rate, format and channel count: the app mixer \
             is bypassed, but the device's audio HAL is not verifiable from an app."
        );
    }

    #[test]
    fn a_matching_hardware_report_is_added_to_the_reason() {
        let mut grant = exclusive_grant(48_000, 2, FORMAT_PCM_I16);
        grant.hardware = Some(Hardware {
            sample_rate: 48_000,
            channels: 2,
            format: FORMAT_PCM_I16,
        });
        let verdict = evaluate(&want(48_000, 2, SampleFormat::I16), &grant);
        assert!(verdict.as_requested);
        assert!(verdict
            .reason
            .ends_with("The hardware reports the same rate, format and channel count."));
    }

    #[test]
    fn a_shared_stream_is_named_as_shared() {
        let mut grant = exclusive_grant(44_100, 2, FORMAT_PCM_I16);
        grant.sharing = SHARING_SHARED;
        let verdict = evaluate(&want(44_100, 2, SampleFormat::I16), &grant);
        assert!(!verdict.exclusive);
        assert!(!verdict.as_requested);
        assert_eq!(verdict.code(), "shared");
        assert!(verdict.breaks_stream().is_none());
        assert!(verdict
            .reason
            .starts_with("The platform granted shared mode, not exclusive"));
        assert!(verdict.reason.contains("system mixer"));
        assert!(verdict.reason.ends_with('.'));
    }

    #[test]
    fn a_different_rate_format_or_channel_count_breaks_the_stream() {
        let verdict = evaluate(
            &want(44_100, 2, SampleFormat::I24Packed),
            &exclusive_grant(48_000, 2, FORMAT_PCM_I24_PACKED),
        );
        assert_eq!(verdict.code(), "rate_changed");
        assert!(verdict.breaks_stream().is_some());
        assert!(verdict.reason.contains("48000 Hz"));
        assert!(verdict.reason.contains("44100 Hz"));

        let verdict = evaluate(
            &want(44_100, 2, SampleFormat::I24Packed),
            &exclusive_grant(44_100, 2, FORMAT_PCM_I32),
        );
        assert_eq!(verdict.code(), "format_changed");
        assert!(verdict.breaks_stream().is_some());
        assert!(verdict.reason.contains("32-bit integer"));
        assert!(verdict.reason.contains("packed 24-bit integer"));

        let verdict = evaluate(
            &want(44_100, 2, SampleFormat::I16),
            &exclusive_grant(44_100, 1, FORMAT_PCM_I16),
        );
        assert_eq!(verdict.code(), "channels_changed");
        assert!(verdict.breaks_stream().is_some());
    }

    #[test]
    fn several_findings_are_all_in_the_reason_most_serious_first() {
        let mut grant = exclusive_grant(48_000, 2, FORMAT_PCM_I16);
        grant.sharing = SHARING_SHARED;
        let verdict = evaluate(&want(44_100, 2, SampleFormat::I16), &grant);
        let codes: Vec<_> = verdict.findings.iter().map(Finding::code).collect();
        assert_eq!(codes, ["shared", "rate_changed"]);
        assert!(verdict.reason.contains("; the stream runs at 48000 Hz"));
    }

    #[test]
    fn hardware_that_differs_from_the_stream_is_a_conversion() {
        let mut grant = exclusive_grant(44_100, 2, FORMAT_PCM_I24_PACKED);
        grant.hardware = Some(Hardware {
            sample_rate: 48_000,
            channels: 2,
            format: FORMAT_PCM_I16,
        });
        let verdict = evaluate(&want(44_100, 2, SampleFormat::I24Packed), &grant);
        assert!(verdict.exclusive);
        assert!(!verdict.as_requested);
        let codes: Vec<_> = verdict.findings.iter().map(Finding::code).collect();
        assert_eq!(codes, ["hardware_rate", "hardware_format"]);
        assert!(
            verdict.breaks_stream().is_none(),
            "playable, just converted by the platform"
        );
        assert!(verdict
            .reason
            .contains("AAudio resamples before the device"));
        assert!(verdict
            .reason
            .contains("converts the samples before the device"));
    }

    #[test]
    fn a_hardware_format_aaudio_cannot_name_is_not_a_finding() {
        let mut grant = exclusive_grant(44_100, 2, FORMAT_PCM_I16);
        grant.hardware = Some(Hardware {
            sample_rate: 44_100,
            channels: 2,
            format: -1,
        });
        assert!(evaluate(&want(44_100, 2, SampleFormat::I16), &grant).as_requested);
    }

    #[test]
    fn the_grant_json_carries_the_verdict() {
        let mut grant = exclusive_grant(44_100, 2, FORMAT_PCM_I16);
        grant.sharing = SHARING_SHARED;
        let value = grant_json(&want(44_100, 2, SampleFormat::I16), &grant);
        assert_eq!(value["sharing"], "shared");
        assert_eq!(value["format"], "i16");
        assert_eq!(value["verdict"], "shared");
        assert_eq!(value["as_requested"], false);
        assert_eq!(value["performance_mode"], "low_latency");
        assert_eq!(value["mmap"], true);
        assert!(value["hardware"].is_null());
        assert!(value["reason"].as_str().unwrap().contains("shared mode"));
    }

    // ---- result codes ----

    #[test]
    fn aaudio_results_map_to_the_library_codes() {
        for (result, code) in [
            (ERROR_DISCONNECTED, "device_lost"),
            (ERROR_ILLEGAL_ARGUMENT, "invalid_argument"),
            (ERROR_OUT_OF_RANGE, "invalid_argument"),
            (ERROR_INVALID_STATE, "invalid_state"),
            (ERROR_INVALID_HANDLE, "invalid_state"),
            (ERROR_UNIMPLEMENTED, "unsupported"),
            (ERROR_INVALID_FORMAT, "unsupported"),
            (ERROR_INVALID_RATE, "unsupported"),
            (ERROR_UNAVAILABLE, "busy"),
            (ERROR_NO_FREE_HANDLES, "busy"),
            (ERROR_INTERNAL, "transfer"),
            (ERROR_NO_SERVICE, "transfer"),
            (-12345, "transfer"),
        ] {
            let error = error_for_result("opening the stream", result);
            assert_eq!(error.code, code, "{result}");
            assert!(error.message.contains("opening the stream"));
            assert!(error.message.contains(&format!("({result})")));
        }
        assert!(error_for_result("x", ERROR_DISCONNECTED)
            .message
            .contains("DISCONNECTED"));
    }

    // ---- the audible position ----

    const MS: i64 = 1_000_000;

    #[test]
    fn without_a_timestamp_the_read_position_is_used() {
        assert_eq!(presented_position(None, 0, 48_000, 1234, true), 1234);
        assert_eq!(presented_position(None, 0, 48_000, -5, true), 0);
    }

    #[test]
    fn a_timestamp_is_extrapolated_to_now_but_never_past_the_read_position() {
        let timestamp = Timestamp {
            frame: 4_800,
            time_ns: 100 * MS,
        };
        // 10 ms later at 48 kHz: 480 more frames than the timestamp says.
        assert_eq!(
            presented_position(Some(timestamp), 110 * MS, 48_000, 10_000, true),
            5_280
        );
        // The device cannot have presented what it has not read.
        assert_eq!(
            presented_position(Some(timestamp), 110 * MS, 48_000, 5_000, true),
            5_000
        );
    }

    #[test]
    fn a_timestamp_in_the_future_is_walked_back() {
        let timestamp = Timestamp {
            frame: 4_800,
            time_ns: 120 * MS,
        };
        assert_eq!(
            presented_position(Some(timestamp), 110 * MS, 48_000, 10_000, true),
            4_320
        );
    }

    #[test]
    fn a_stale_or_stopped_timestamp_falls_back_to_the_read_position() {
        let stale = Timestamp {
            frame: 0,
            time_ns: 0,
        };
        // 100 s later the extrapolation says 4.8 M frames while the device read 20 000: the
        // estimate is past the read position, so it is clamped; and a timestamp that lags the read
        // position by more than a second is ignored.
        let lagging = Timestamp {
            frame: 1_000,
            time_ns: 0,
        };
        assert_eq!(
            presented_position(Some(lagging), 0, 48_000, 200_000, true),
            200_000
        );
        assert_eq!(
            presented_position(Some(stale), 100_000 * MS, 48_000, 20_000, true),
            20_000
        );
        // A stopped stream does not extrapolate.
        let timestamp = Timestamp {
            frame: 4_800,
            time_ns: 0,
        };
        assert_eq!(
            presented_position(Some(timestamp), 500 * MS, 48_000, 6_000, false),
            6_000
        );
    }

    #[test]
    fn the_position_arithmetic_does_not_overflow() {
        let timestamp = Timestamp {
            frame: i64::MAX / 2,
            time_ns: i64::MIN / 2,
        };
        let value = presented_position(Some(timestamp), i64::MAX / 2, 768_000, i64::MAX / 4, true);
        assert!(value >= 0);
    }
}
