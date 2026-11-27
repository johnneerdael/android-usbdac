//! A DAC's Feature Unit, answered in-process instead of through usbfs, for the
//! control transfers `UsbDevice::feature_unit_control` sends. Test only.

use super::*;
use crate::volume_control::normalize_volume;
use crossbeam_channel::{Receiver, Sender};
use std::collections::HashMap;

pub(crate) const INTERFACE: u8 = 0;
pub(crate) const UNIT: u8 = 5;
const MUTE_SELECTOR: u16 = 0x0100;
const VOLUME_SELECTOR: u16 = 0x0200;

#[derive(Default)]
pub(crate) struct FakeDac {
    /// Raw level per channel; index 0 is the master channel.
    pub volume: [i16; 3],
    pub muted: bool,
    /// Every volume SET_CUR, in order, as (channel, raw).
    pub volume_writes: Vec<(u16, i16)>,
    pub mute_writes: Vec<bool>,
    pub reads: usize,
    /// Makes every transfer fail, as an unreachable DAC would.
    pub unreachable: bool,
    /// Accepts volume writes (and logs them) without taking them: the level
    /// reads back as it was, as a DAC that ignores SET_CUR does.
    pub ignore_volume_writes: bool,
    /// Holds the next volume SET_CUR: reports on the sender that it has
    /// arrived, then waits for the receiver before it takes effect.
    pub hold_next_volume_write: Option<(Sender<()>, Receiver<()>)>,
}

pub(crate) type Dac = Arc<StdMutex<FakeDac>>;
static DACS: Lazy<StdMutex<HashMap<RawFd, Dac>>> = Lazy::new(|| StdMutex::new(HashMap::new()));

pub(crate) fn install(fd: RawFd, dac: Dac) {
    DACS.lock().unwrap().insert(fd, dac);
}

pub(crate) fn remove(fd: RawFd) {
    DACS.lock().unwrap().remove(&fd);
}

/// None when no fake DAC answers on `fd`, so the real transfer goes ahead.
pub(super) fn dispatch(
    fd: RawFd,
    request_type: u8,
    request: u8,
    value: u16,
    index: u16,
    data: &mut [u8],
) -> Option<std::io::Result<usize>> {
    let dac = DACS.lock().unwrap().get(&fd).cloned()?;
    Some(answer(&dac, request_type, request, value, index, data))
}

fn answer(
    dac: &Dac,
    request_type: u8,
    request: u8,
    value: u16,
    index: u16,
    data: &mut [u8],
) -> std::io::Result<usize> {
    if dac.lock().unwrap().unreachable {
        return Err(std::io::Error::other("DAC unreachable"));
    }
    let addressed = (index & 0xff, index >> 8);
    // UAC2 GET_CUR and SET_CUR are both request 0x01.
    if addressed != (u16::from(INTERFACE), u16::from(UNIT)) || request != UAC2_REQUEST_SET_CUR {
        return Err(std::io::Error::other(format!(
            "unexpected request 0x{request:02x} to {addressed:?}"
        )));
    }
    let (selector, channel) = (value & 0xff00, usize::from(value & 0xff));
    let reading = request_type & USB_DIR_IN != 0;
    match (selector, reading) {
        (VOLUME_SELECTOR, true) => {
            let mut dac = dac.lock().unwrap();
            dac.reads += 1;
            data.copy_from_slice(&dac.volume[channel].to_le_bytes());
        }
        (VOLUME_SELECTOR, false) => {
            let gate = dac.lock().unwrap().hold_next_volume_write.take();
            if let Some((arrived, release)) = gate {
                arrived.send(()).unwrap();
                release.recv().unwrap();
            }
            let raw = i16::from_le_bytes([data[0], data[1]]);
            let mut dac = dac.lock().unwrap();
            if !dac.ignore_volume_writes {
                dac.volume[channel] = raw;
            }
            dac.volume_writes.push((channel as u16, raw));
        }
        (MUTE_SELECTOR, true) => {
            let mut dac = dac.lock().unwrap();
            dac.reads += 1;
            data[0] = u8::from(dac.muted);
        }
        (MUTE_SELECTOR, false) => {
            let mut dac = dac.lock().unwrap();
            dac.muted = data[0] != 0;
            let muted = dac.muted;
            dac.mute_writes.push(muted);
        }
        _ => return Err(std::io::Error::other("unknown control selector")),
    }
    Ok(data.len())
}

pub(crate) const FD: RawFd = -9_001;
pub(crate) const START_RAW: i16 = -64 * 256;
pub(crate) const ORIGINAL_RAW: i16 = -20 * 256;

pub(super) fn control() -> HardwareVolumeControl {
    HardwareVolumeControl {
        interface_number: INTERFACE,
        feature_unit_id: UNIT,
        protocol: AudioControlProtocol::Uac2,
        volume_channels: vec![1, 2],
        ranges: vec![RawRange {
            min: -80 * 256,
            max: 0,
            resolution: 256,
        }],
        mute_channel: Some(0),
        min_volume_raw: -80 * 256,
        max_volume_raw: 0,
        resolution_raw: 256,
    }
}

pub(crate) fn device() -> UsbDevice {
    UsbDevice {
        fd: DeviceKey::Usbfs(FD),
        #[cfg(target_os = "macos")]
        darwin: None,
        _fd_owner: Some(Arc::new(OwnedUsbFd(FD))),
        claimed_interfaces: Arc::from(vec![INTERFACE]),
        vendor_id: 0x1234,
        product_id: 0x5678,
        product_name: "test DAC".to_string(),
        device_name: Some("/dev/bus/usb/001/002".to_string()),
    }
}

pub(super) fn state(stream_active: bool, control: Option<HardwareVolumeControl>) -> SessionState {
    let mut state = test_state(None);
    state.device = device();
    state.stream_active = stream_active;
    state.hardware_volume_state = control.as_ref().map(|control| crate::volume::VolumeState {
        db: crate::volume::raw_to_db(START_RAW),
        fraction: normalize_volume(START_RAW, &control.ranges),
        muted: Some(false),
    });
    state.hardware_volume_control = control;
    state.hardware_volume_generation = HARDWARE_VOLUME_GENERATION.fetch_add(1, Ordering::Relaxed);
    state
}

/// One registered fake DAC for one test. Serialised: the registration, the
/// original-level record and the fake transport are process-wide.
pub(crate) struct Registered {
    _serial: std::sync::MutexGuard<'static, ()>,
    pub dac: Dac,
}

impl Registered {
    pub(crate) fn new(stream_active: bool, with_control: bool) -> Self {
        let serial = PAYLOAD_LATCH_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        reset_globals();
        let dac = Arc::new(StdMutex::new(FakeDac {
            volume: [START_RAW; 3],
            ..FakeDac::default()
        }));
        install(FD, dac.clone());
        *SESSION_STATE.lock() = Some(state(stream_active, with_control.then(control)));
        Self {
            _serial: serial,
            dac,
        }
    }

    /// Installs the settings a session opened with `config` would have.
    pub(crate) fn configure(&self, config: crate::config::DacConfig) {
        crate::settings::install(crate::settings::Settings::for_device(
            config,
            &crate::capabilities::DeviceInfo::new(0x1234, 0x5678, "test DAC"),
        ));
    }

    /// Sets the stereo channels' levels (raw) and forgets the writes so far.
    pub(crate) fn set_levels(&self, left: i16, right: i16) {
        let mut dac = self.dac.lock().unwrap();
        dac.volume = [left, left, right];
        dac.volume_writes.clear();
    }

    pub(crate) fn generation(&self) -> u64 {
        SESSION_STATE
            .lock()
            .as_ref()
            .unwrap()
            .hardware_volume_generation
    }

    /// What the stream stopping does while the idle lock holds the DAC. The
    /// lock itself needs a live libusb handle, so its presence is stated.
    pub(crate) fn stop_stream_holding_the_dac(&self) {
        SESSION_STATE
            .lock()
            .as_mut()
            .unwrap()
            .set_stream_active(false, true);
    }

    /// Whether the level the DAC had before the library first changed it is on
    /// record for the fake device.
    pub(crate) fn original_recorded(&self) -> bool {
        ORIGINAL_HARDWARE_VOLUME
            .lock()
            .is_recorded(&original_volume_device_key(&device()))
    }

    /// Records what the fake DAC reads now as the level it had before the
    /// library touched it, as the discovery of the first stream does.
    pub(crate) fn record_original(&self) {
        let _hold = HARDWARE_VOLUME_MUTEX.lock();
        note_original_hardware_volume(&device(), &control());
    }

    /// The raw level of the stereo channels.
    pub(crate) fn raw(&self) -> Vec<i16> {
        self.dac.lock().unwrap().volume[1..].to_vec()
    }
}

impl Drop for Registered {
    fn drop(&mut self) {
        remove(FD);
        reset_globals();
    }
}

fn reset_globals() {
    crate::settings::reset();
    *SESSION_STATE.lock() = None;
    ORIGINAL_HARDWARE_VOLUME.lock().forget();
    ORIGINAL_VOLUME_ACTION.store(ORIGINAL_VOLUME_KEEP, Ordering::SeqCst);
}
