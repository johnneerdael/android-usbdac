#![cfg_attr(not(target_os = "linux"), allow(unused))]
use std::{
    fs::OpenOptions,
    os::fd::AsRawFd,
    sync::Arc,
    thread,
    time::{Duration, Instant},
};
use usbdac_core::{Dac, DacConfig, DacEvent, DeviceInfo, StreamRequest};
struct Logger;
impl log::Log for Logger {
    fn enabled(&self, _: &log::Metadata) -> bool {
        true
    }
    fn log(&self, r: &log::Record) {
        eprintln!("{} {}", r.level(), r.args())
    }
    fn flush(&self) {}
}
static LOGGER: Logger = Logger;
#[repr(C)]
struct Control {
    kind: u8,
    request: u8,
    value: u16,
    index: u16,
    length: u16,
    timeout: u32,
    data: *mut u8,
}
fn alt(fd: i32) -> i32 {
    let mut b = 255u8;
    let mut c = Control {
        kind: 0x81,
        request: 10,
        value: 0,
        index: 1,
        length: 1,
        timeout: 1000,
        data: &mut b,
    };
    let r = unsafe { libc::ioctl(fd, 0xc0185500u32 as _, &mut c) };
    println!("GET_INTERFACE result={r} alt={b}");
    i32::from(b)
}
fn volume(fd: i32) -> std::io::Result<[i16; 2]> {
    let mut values = [0; 2];
    for channel in 1..=2u16 {
        let mut bytes = [0u8; 2];
        let mut c = Control {
            kind: 0xa1,
            request: 0x81,
            value: 0x0200 | channel,
            index: 0x0200,
            length: 2,
            timeout: 1000,
            data: bytes.as_mut_ptr(),
        };
        let result = unsafe { libc::ioctl(fd, 0xc0185500u32 as _, &mut c) };
        if result != 2 {
            return Err(std::io::Error::last_os_error());
        }
        values[(channel - 1) as usize] = i16::from_le_bytes(bytes);
    }
    Ok(values)
}
#[cfg(target_os = "linux")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    log::set_logger(&LOGGER).unwrap();
    log::set_max_level(log::LevelFilter::Info);
    println!(
        "SOURCE {}; QEMU null audio backend",
        option_env!("USBDAC_PROBE_REVISION").unwrap_or("unrecorded")
    );
    let f = OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/bus/usb/001/002")?;
    let fd = f.as_raw_fd();
    for i in [0u32, 1] {
        let r = unsafe { libc::ioctl(fd, 0x8004550fu32 as _, &i) };
        println!("CLAIM interface={i} result={r}");
        if r != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
    }
    assert_eq!(alt(fd), 0);
    let info = DeviceInfo::new(0x46f4, 0x0002, "QEMU USB Audio")
        .with_claimed_interfaces(vec![0u8, 1])
        .with_device_name("/dev/bus/usb/001/002");
    let before = volume(fd)?;
    let dac = Dac::open(fd, info, DacConfig::default())?;
    let capabilities = dac.capabilities()?;
    assert_eq!(
        capabilities.uac_version,
        usbdac_core::UacVersion::V1,
        "this probe proves UAC1 only"
    );
    println!("CAPABILITIES {capabilities:?}");
    let fmt = dac.negotiate(&StreamRequest::pcm(48_000, 2, 16))?;
    println!("FORMAT {fmt:?}");
    let listener = Arc::new(|e: DacEvent| eprintln!("EVENT {e:?}"));
    let mut stream = dac.open_stream(&fmt, listener.clone())?;
    let mut data = Vec::new();
    for i in 0..4800i32 {
        let v = (((i % 96) - 48) * 20) as i16;
        data.extend_from_slice(&v.to_le_bytes());
        data.extend_from_slice(&(-v).to_le_bytes());
    }
    for _ in 0..3 {
        println!("PRIME {}", stream.write(&data)?);
    }
    stream.play()?;
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(2) {
        let n = stream.writable_bytes().min(data.len());
        if n >= 4 {
            stream.write(&data[..n - n % 4])?;
        }
        thread::sleep(Duration::from_millis(2));
    }
    let st = stream.stats();
    println!("PCM_STATS {st:?}");
    let good = st.played_frames > 20_000
        && st.completed_transfers > 0
        && st.lost_frames == 0
        && st.underruns == 0
        && st.urb_timeouts == 0
        && st.short_packets == 0
        && st.zero_packets == 0;
    stream.pause()?;
    let held_volume = volume(fd)?;
    let replacement = dac.reconfigure_stream(
        &stream,
        &StreamRequest::pcm(48_000, 2, 16),
        listener.clone(),
        Duration::from_secs(3),
    )?;
    assert!(replacement.rollback_cause.is_none());
    assert_eq!(stream.state(), usbdac_core::StreamState::Closed);
    assert_eq!(replacement.stream.state(), usbdac_core::StreamState::Paused);
    assert_eq!(replacement.stream.played_frames(), 0);
    assert_eq!(
        volume(fd)?,
        held_volume,
        "reconfigure changed hardware volume"
    );
    stream = replacement.stream;
    println!("RECONFIGURE_SAME_OWNER_PASS rate=48000 paused=true volume=unchanged");
    // The QEMU descriptor exposes signed16 only: this is a valid request with no matching alternate.
    let rollback = dac.reconfigure_stream(
        &stream,
        &StreamRequest::pcm_float32(48_000, 2),
        listener.clone(),
        Duration::from_secs(3),
    )?;
    assert_eq!(
        rollback.rollback_cause.as_ref().map(|e| e.code()),
        Some("unsupported")
    );
    assert_eq!(stream.state(), usbdac_core::StreamState::Closed);
    assert_eq!(rollback.stream.state(), usbdac_core::StreamState::Paused);
    assert_eq!(
        rollback.stream.format().request,
        StreamRequest::pcm(48_000, 2, 16)
    );
    assert_eq!(volume(fd)?, held_volume, "rollback changed hardware volume");
    stream = rollback.stream;
    for _ in 0..3 {
        stream.write(&data)?;
    }
    stream.play()?;
    thread::sleep(Duration::from_millis(100));
    assert!(
        stream.played_frames() > 0,
        "rolled-back stream did not resume real completions"
    );
    assert_eq!(stream.stats().lost_frames, 0);
    println!("RECONFIGURE_ROLLBACK_PASS rate=48000 cause=unsupported resumed=true");
    stream.pause()?;
    stream.close()?;
    dac.close()?;
    assert!(Dac::await_usb_retirement(Duration::from_secs(2)));
    println!("RETIRED true");
    assert_eq!(alt(fd), 0);
    let after = volume(fd)?;
    assert_eq!(
        after, before,
        "original per-channel volume was not restored"
    );
    println!("VOLUME_RESTORED {after:?}");
    for i in [1u32, 0] {
        let r = unsafe { libc::ioctl(fd, 0x80045510u32 as _, &i) };
        println!("RELEASE interface={i} result={r}");
        assert_eq!(r, 0);
    }
    assert!(good, "no proven lossless media completions");
    println!("QEMU_PCM_PASS");
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("qemu_smoke needs a Linux guest with the QEMU UAC1 virtual device");
}
