#![allow(dead_code)]
//! The host's half of opening a DAC, from a USB file descriptor to a negotiated format.
//!
//! This is the code of the first half of `docs/sdk/guide-rust.md`. It compiles on every host; it only
//! runs on Linux or Android with a DAC. `cargo check --workspace --examples` keeps it compiling.

use std::os::fd::RawFd;
use usbdac_core::{
    AltFormat, Capabilities, Dac, DacConfig, DeviceInfo, Error, RatePolicy, Result, StreamFormat,
    StreamRequest,
};

// region rust-host
/// What the host does with USB itself, before and after the library. The library never claims an
/// interface and never closes the descriptor: on Android the Kotlin side owns the connection, on Linux
/// your own code owns the usbfs file.
pub trait UsbHost {
    /// The usbfs file descriptor of the open device. The library duplicates it.
    fn fd(&self) -> RawFd;

    /// Claims the AudioControl interfaces and the playback AudioStreaming interfaces, taking them from
    /// the system's audio driver, and returns their numbers. Fails with `Error::Claim`.
    fn claim_audio_interfaces(&mut self) -> Result<Vec<u8>>;

    /// Tries to claim `candidates`, the optional interfaces a DAC may use for implicit feedback, and
    /// returns the ones it got. A claim that fails is not an error: the library plays without it.
    fn claim_optional(&mut self, candidates: &[u8]) -> Vec<u8>;
}
// endregion

// region rust-open
/// Claims the interfaces, asks the library which optional ones it can use, claims those, and opens.
///
/// Blocks (descriptor and clock reads): call it from a thread of your own, never from a UI thread.
pub fn open_dac(host: &mut dyn UsbHost, identity: DeviceInfo, config: DacConfig) -> Result<Dac> {
    let mut info = identity.with_claimed_interfaces(host.claim_audio_interfaces()?);
    if !config.disable_feedback {
        // Read-only: it names the active configuration and the IN interfaces a DAC may pace itself with.
        let (configuration, candidates) = Dac::feedback_preflight(host.fd(), &info)?;
        let got = host.claim_optional(&candidates);
        info.configuration_id = Some(configuration);
        info.unavailable_feedback_interfaces = candidates
            .iter()
            .copied()
            .filter(|n| !got.contains(n))
            .collect();
        info.claimed_interfaces.extend(got);
    }
    // Lowers the DAC to at most -40 dB, verifies it, and touches neither the clock nor the audio.
    Dac::open(host.fd(), info, config)
}
// endregion

// region rust-config
/// The listener's choices, as a Rust app keeps them.
pub struct Choices {
    /// Play on a DAC whose volume cannot be lowered, at its own level.
    pub allow_fixed_volume: bool,
    /// Keep a USB feedback trace of the next attempt, for a support report.
    pub capture_support_trace: bool,
}

pub fn config_for(choices: &Choices) -> DacConfig {
    DacConfig {
        require_hardware_volume: !choices.allow_fixed_volume,
        capture_feedback_trace: choices.capture_support_trace,
        ..DacConfig::default()
    }
}
// endregion

// region rust-negotiate
/// What a decoder says about the track it is about to play.
#[derive(Clone, Copy, Debug)]
pub struct TrackFormat {
    pub sample_rate: u32,
    pub channels: u16,
    /// Significant bits of the source: 16, 24 or 32.
    pub bits: u8,
}

/// The wire format for a track, and whether the host must resample first.
pub struct Negotiated {
    pub format: StreamFormat,
    /// `Some(rate)` when the DAC runs another rate than the track has. The stream is then not bit-perfect.
    pub resample_to: Option<u32>,
}

/// Asks for the track's own format. With `allow_nearest` the DAC may run the nearest rate it has, and
/// the result says so. Takes 100 ms or more: it programs the DAC's clock and reads it back.
pub fn negotiate(dac: &Dac, track: TrackFormat, allow_nearest: bool) -> Result<Negotiated> {
    let policy = if allow_nearest {
        RatePolicy::NearestSupported
    } else {
        RatePolicy::Exact
    };
    let request =
        StreamRequest::pcm(track.sample_rate, track.channels, track.bits).with_rate_policy(policy);
    let format = dac.negotiate(&request)?;
    let resample_to = format
        .rate_differs_from_request()
        .then_some(format.sample_rate);
    Ok(Negotiated {
        format,
        resample_to,
    })
}

/// Whether the DAC lists the format among its PCM alternate settings, without touching its clock. A
/// DAC that takes a continuous range of rates lists none here, so a `false` is a hint and `negotiate`
/// has the last word.
pub fn offers(capabilities: &Capabilities, track: TrackFormat) -> bool {
    capabilities.alt_settings.iter().any(|alt| {
        alt.format == AltFormat::Pcm
            && alt.channels == track.channels
            && alt.valid_bits == track.bits
            && alt.sample_rates.contains(&track.sample_rate)
    })
}
// endregion

// region rust-errors
/// What a host does about each failure of the open path.
pub enum Next {
    /// Tell the listener and let them decide: allow a fixed volume, grant permission, replug.
    AskListener(&'static str),
    /// Use another output, or convert if the listener allowed it.
    OtherOutput(&'static str),
    /// A bug in the host's own sequence: close what is open first.
    FixTheHost(&'static str),
    /// Log it with the DAC's ids and the descriptors, and give up on this DAC.
    Report(&'static str),
}

pub fn next_step(error: &Error) -> Next {
    match error {
        Error::VolumeUnavailable => {
            Next::AskListener("the DAC has no volume control: allow a fixed level and open again")
        }
        Error::Claim(_) => Next::AskListener(
            "the system keeps the DAC: replug it and check that nothing else holds it",
        ),
        Error::Busy => Next::FixTheHost(
            "this DAC already has an owner, or a reconfigure is running: close it or retry",
        ),
        Error::Unsupported { .. } => {
            Next::OtherOutput("the DAC has no alternate setting for this format")
        }
        Error::ImplicitFeedbackUnsupported => {
            Next::OtherOutput("an asynchronous endpoint without feedback")
        }
        Error::ClockUnverified { .. } => Next::Report("the DAC did not confirm its sampling rate"),
        Error::DeviceLost => Next::AskListener("the DAC was unplugged"),
        Error::PlatformUnsupported => {
            Next::Report("direct USB needs Linux usbfs: Android or Linux")
        }
        Error::InvalidState(_) | Error::InvalidArgument(_) => {
            Next::FixTheHost("the call was not valid in this state")
        }
        Error::Transfer(_) | Error::Control(_) => {
            Next::Report("a transfer or class request failed")
        }
        // `Error` is non-exhaustive: a newer library may add variants.
        _ => Next::Report("an error this host does not know"),
    }
}
// endregion

// region rust-android
/// The Android side of [`UsbHost`]. Kotlin owns the `UsbDeviceConnection`: it asked for permission,
/// opened the device, claimed the interfaces and holds them until the `Dac` is closed. It hands over the
/// connection's file descriptor and the interface numbers it claimed. How it crosses (JNI, UniFFI, Dart
/// FFI) is your bridge's business.
pub struct AndroidHost {
    fd: RawFd,
    claimed: Vec<u8>,
    /// Calls back into Kotlin: `UsbDeviceConnection.claimInterface(iface, true)` for each candidate.
    claim_more: ClaimMore,
}

type ClaimMore = Box<dyn FnMut(&[u8]) -> Vec<u8> + Send>;

impl AndroidHost {
    pub fn new(fd: RawFd, claimed: Vec<u8>, claim_more: ClaimMore) -> Self {
        Self {
            fd,
            claimed,
            claim_more,
        }
    }
}

impl UsbHost for AndroidHost {
    fn fd(&self) -> RawFd {
        self.fd
    }

    fn claim_audio_interfaces(&mut self) -> Result<Vec<u8>> {
        Ok(self.claimed.clone())
    }

    fn claim_optional(&mut self, candidates: &[u8]) -> Vec<u8> {
        (self.claim_more)(candidates)
    }
}

/// The values Kotlin passes: `UsbDevice.vendorId`, `productId`, `productName`, `manufacturerName` and
/// `deviceName`, with the interfaces it claimed.
pub fn open_from_android(
    host: &mut AndroidHost,
    vendor_id: u16,
    product_id: u16,
    product_name: &str,
    device_name: &str,
    config: DacConfig,
) -> Result<Dac> {
    let identity =
        DeviceInfo::new(vendor_id, product_id, product_name).with_device_name(device_name);
    open_dac(host, identity, config)
}
// endregion

// region rust-usbfs
/// The Linux side of [`UsbHost`]: a usbfs file, interfaces claimed with its ioctls. Compile-checked for
/// Linux and Android targets. It has not been run on a Linux desktop by this project.
#[cfg(any(target_os = "linux", target_os = "android"))]
pub mod usbfs {
    use super::UsbHost;
    use std::fs::{File, OpenOptions};
    use std::io::{self, Read};
    use std::os::fd::{AsRawFd, RawFd};
    use std::path::Path;
    use usbdac_core::{Error, Result};

    #[repr(C)]
    struct DisconnectClaim {
        interface: libc::c_uint,
        flags: libc::c_uint,
        driver: [libc::c_char; 256],
    }

    #[repr(C)]
    struct IoctlTransfer {
        interface: libc::c_int,
        code: libc::c_int,
        data: *mut libc::c_void,
    }

    // The usbfs ioctls of <linux/usbdevice_fs.h>, with the architecture's own number layout.
    const CLAIM_INTERFACE: libc::Ioctl = libc::_IOR::<libc::c_uint>(b'U' as u32, 15);
    const RELEASE_INTERFACE: libc::Ioctl = libc::_IOR::<libc::c_uint>(b'U' as u32, 16);
    const IOCTL: libc::Ioctl = libc::_IOWR::<IoctlTransfer>(b'U' as u32, 18);
    const DISCONNECT_CLAIM: libc::Ioctl = libc::_IOR::<DisconnectClaim>(b'U' as u32, 27);
    /// A code for `IOCTL`: bind the kernel's driver to the interface again.
    const CONNECT: libc::c_int = libc::_IO(b'U' as u32, 23) as libc::c_int;

    /// One interface setting of the device's configuration descriptor.
    #[derive(Debug, Clone, Copy)]
    struct Setting {
        number: u8,
        class: u8,
        subclass: u8,
        isochronous_out: bool,
    }

    pub struct UsbfsHost {
        file: File,
        settings: Vec<Setting>,
        claimed: Vec<u8>,
    }

    impl UsbfsHost {
        /// Opens `/dev/bus/usb/BBB/DDD`. The process needs read and write access to the node, usually
        /// through a udev rule. Reading the node returns the device descriptor and then the
        /// configuration descriptors. This reads the first configuration only: a DAC with several
        /// needs the active one (a GET_CONFIGURATION request) before it is claimed.
        pub fn open(path: &Path) -> io::Result<Self> {
            let mut file = OpenOptions::new().read(true).write(true).open(path)?;
            let mut raw = Vec::new();
            file.read_to_end(&mut raw)?;
            let settings = parse_settings(raw.get(18..).unwrap_or(&[]));
            Ok(Self {
                file,
                settings,
                claimed: Vec::new(),
            })
        }

        fn ioctl<T>(&self, request: libc::Ioctl, argument: *mut T) -> io::Result<()> {
            if unsafe { libc::ioctl(self.file.as_raw_fd(), request, argument) } < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        }

        fn claim(&mut self, interface: u8) -> io::Result<()> {
            // Takes the interface from the kernel's audio driver if it holds it.
            let mut claim = DisconnectClaim {
                interface: interface.into(),
                flags: 0,
                driver: [0; 256],
            };
            self.ioctl(DISCONNECT_CLAIM, &mut claim)?;
            self.claimed.push(interface);
            Ok(())
        }
    }

    impl UsbHost for UsbfsHost {
        fn fd(&self) -> RawFd {
            self.file.as_raw_fd()
        }

        fn claim_audio_interfaces(&mut self) -> Result<Vec<u8>> {
            // AudioControl (class 1, subclass 1) and every AudioStreaming interface (subclass 2) with an
            // isochronous OUT endpoint in some alternate setting. Capture-only interfaces stay with the kernel.
            let mut wanted: Vec<u8> = self
                .settings
                .iter()
                .filter(|s| {
                    s.class == 1 && (s.subclass == 1 || (s.subclass == 2 && s.isochronous_out))
                })
                .map(|s| s.number)
                .collect();
            wanted.sort_unstable();
            wanted.dedup();
            if wanted.is_empty() {
                return Err(Error::Claim(
                    "no AudioControl and playback AudioStreaming interface".into(),
                ));
            }
            for interface in &wanted {
                self.claim(*interface)
                    .map_err(|e| Error::Claim(format!("interface {interface}: {e}")))?;
            }
            Ok(wanted)
        }

        fn claim_optional(&mut self, candidates: &[u8]) -> Vec<u8> {
            candidates
                .iter()
                .copied()
                .filter(|n| self.claim(*n).is_ok())
                .collect()
        }
    }

    impl Drop for UsbfsHost {
        /// Gives every interface back and asks the kernel to bind its audio driver again. Close the `Dac`
        /// first: the interfaces are in use until it has closed.
        fn drop(&mut self) {
            for interface in std::mem::take(&mut self.claimed) {
                let mut number: libc::c_uint = interface.into();
                let _ = self.ioctl(RELEASE_INTERFACE, &mut number);
                let mut connect = IoctlTransfer {
                    interface: interface.into(),
                    code: CONNECT,
                    data: std::ptr::null_mut(),
                };
                let _ = self.ioctl(IOCTL, &mut connect);
            }
        }
    }

    /// Walks a configuration descriptor and lists its interface settings.
    fn parse_settings(descriptors: &[u8]) -> Vec<Setting> {
        let mut settings: Vec<Setting> = Vec::new();
        let mut at = 0;
        while at + 2 <= descriptors.len() {
            let length = usize::from(descriptors[at]);
            if length < 2 || at + length > descriptors.len() {
                break;
            }
            let body = &descriptors[at..at + length];
            match body[1] {
                // Interface: number, alternate setting, endpoints, class, subclass.
                4 if length >= 9 => settings.push(Setting {
                    number: body[2],
                    class: body[5],
                    subclass: body[6],
                    isochronous_out: false,
                }),
                // Endpoint: address (bit 7 is the direction), attributes (bits 0-1 are the transfer type).
                5 if length >= 7 => {
                    if let Some(last) = settings.last_mut() {
                        last.isochronous_out |= body[2] & 0x80 == 0 && body[3] & 0x03 == 1;
                    }
                }
                _ => {}
            }
            at += length;
        }
        // The settings of one interface share its number: keep the flag if any alternate setting has the endpoint.
        let copy = settings.clone();
        for setting in &mut settings {
            setting.isochronous_out = copy
                .iter()
                .any(|s| s.number == setting.number && s.isochronous_out);
        }
        settings
    }
}
// endregion

fn main() {
    println!(
        "guide_open is a compile-checked example of docs/sdk/guide-rust.md; it needs a DAC to run."
    );
}
