//! Independent, bounded capture for start/stop/alt-zero discontinuity analysis.
//! Run in a separate process from playback, using an independent USB input.
#[cfg(target_os = "linux")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use std::{
        fs::OpenOptions,
        io::{Seek, SeekFrom, Write},
        os::fd::AsRawFd,
        time::{Duration, Instant},
    };
    use usbdac_core::{CaptureRequest, Dac, DacConfig, DeviceInfo};
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 5 {
        return Err(
            "usage: loopback_capture /dev/bus/usb/BBB/DDD output.wav sample_rate seconds (1..120)"
                .into(),
        );
    }
    let rate: u32 = args[3].parse()?;
    let seconds: u64 = args[4].parse()?;
    if rate == 0 || !(1..=120).contains(&seconds) {
        return Err("invalid capture duration/rate".into());
    }
    let file = OpenOptions::new().read(true).write(true).open(&args[1])?;
    let fd = file.as_raw_fd();
    // GET_DESCRIPTOR device: identity comes from the actual USB device.
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
    let mut descriptor = [0u8; 18];
    let mut control = Control {
        kind: 0x80,
        request: 6,
        value: 0x0100,
        index: 0,
        length: 18,
        timeout: 1000,
        data: descriptor.as_mut_ptr(),
    };
    if unsafe { libc::ioctl(fd, 0xc0185500u32 as _, &mut control) } != 18 {
        return Err(std::io::Error::last_os_error().into());
    }
    let mut info = DeviceInfo::new(
        u16::from_le_bytes([descriptor[8], descriptor[9]]),
        u16::from_le_bytes([descriptor[10], descriptor[11]]),
        "Independent loopback input",
    )
    .with_device_name(&args[1]);
    let (configuration, control_interfaces, input_interfaces) = Dac::capture_preflight(fd, &info)?;
    let mut claims = control_interfaces;
    claims.extend(input_interfaces);
    claims.sort_unstable();
    claims.dedup();
    for &interface in &claims {
        let interface = u32::from(interface);
        if unsafe { libc::ioctl(fd, 0x8004550fu32 as _, &interface) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
    }
    info.configuration_id = Some(configuration);
    info.claimed_interfaces = claims.clone();
    let dac = Dac::open_capture_device(fd, info, DacConfig::default())?;
    let capture = dac.open_capture(
        &CaptureRequest {
            sample_rate: rate,
            channels: 2,
            valid_bits: 16,
            channel_map: None,
        },
        rate as usize,
    )?;
    if capture.format().subslot_bytes() != 2 {
        return Err("probe requires packed 16-bit stereo input for WAV analysis".into());
    }
    let mut output = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&args[2])?;
    output.write_all(&[0u8; 44])?;
    let mut buffer = [0u8; 8192];
    let mut bytes = 0u32;
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(seconds) {
        let frames = capture.read(&mut buffer)?;
        let count = frames * capture.format().bytes_per_frame();
        output.write_all(&buffer[..count])?;
        bytes = bytes.checked_add(count as u32).ok_or("WAV too large")?;
        std::thread::sleep(Duration::from_millis(2));
    }
    let stats = capture.stats();
    capture.close()?;
    dac.close()?;
    if !Dac::await_usb_retirement(Duration::from_secs(3)) {
        return Err("capture did not retire; retain host claims".into());
    }
    for &interface in claims.iter().rev() {
        let interface = u32::from(interface);
        if unsafe { libc::ioctl(fd, 0x80045510u32 as _, &interface) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
    }
    let mut header = Vec::with_capacity(44);
    header.extend_from_slice(b"RIFF");
    header.extend_from_slice(&(36 + bytes).to_le_bytes());
    header.extend_from_slice(b"WAVEfmt ");
    header.extend_from_slice(&16u32.to_le_bytes());
    header.extend_from_slice(&1u16.to_le_bytes());
    header.extend_from_slice(&2u16.to_le_bytes());
    header.extend_from_slice(&rate.to_le_bytes());
    header.extend_from_slice(&(rate * 4).to_le_bytes());
    header.extend_from_slice(&4u16.to_le_bytes());
    header.extend_from_slice(&16u16.to_le_bytes());
    header.extend_from_slice(b"data");
    header.extend_from_slice(&bytes.to_le_bytes());
    output.seek(SeekFrom::Start(0))?;
    output.write_all(&header)?;
    output.flush()?;
    let valid = bytes > 0
        && stats.failed_transfers == 0
        && stats.failed_packets == 0
        && stats.missing_frames == 0
        && stats.overrun_frames == 0
        && stats.contention_frames == 0;
    let metadata = serde_json::json!({"sample_rate":rate,"channels":2,"valid_bits":16,"subslot_bytes":2,"bytes":bytes,"capture_complete":valid,"stats":format!("{stats:?}"),"clock_alignment":"unmeasured","gain_calibration":"unmeasured"});
    std::fs::write(
        format!("{}.json", args[2]),
        serde_json::to_vec_pretty(&metadata)?,
    )?;
    if !valid {
        return Err(
            "capture has missing/failed/backlogged frames; metrics cannot establish discontinuity"
                .into(),
        );
    }
    println!("CAPTURE_COMPLETE {} frames; annotate playback events against the capture clock before analysis", bytes / 4);
    Ok(())
}
#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("loopback_capture requires a Linux/Android USB host");
}
