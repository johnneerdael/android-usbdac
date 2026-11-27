//! Bounded normal-path two-device probe for QEMU's two UAC1 usb-audio devices.
#[cfg(target_os = "linux")]
mod linux {
    use std::{
        fs::{File, OpenOptions},
        os::fd::AsRawFd,
        sync::Arc,
        time::{Duration, Instant},
    };
    use usbdac_core::{Dac, DacConfig, DacStream, DeviceInfo, Error, StreamRequest};
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
    fn read(
        fd: i32,
        kind: u8,
        request: u8,
        value: u16,
        index: u16,
        bytes: &mut [u8],
    ) -> std::io::Result<()> {
        let mut control = Control {
            kind,
            request,
            value,
            index,
            length: bytes.len() as u16,
            timeout: 1000,
            data: bytes.as_mut_ptr(),
        };
        let result = unsafe { libc::ioctl(fd, 0xc0185500u32 as _, &mut control) };
        if result != bytes.len() as i32 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
    fn volume(fd: i32) -> std::io::Result<[i16; 2]> {
        let mut values = [0; 2];
        for channel in 1..=2u16 {
            let mut bytes = [0; 2];
            read(fd, 0xa1, 0x81, 0x0200 | channel, 0x0200, &mut bytes)?;
            values[(channel - 1) as usize] = i16::from_le_bytes(bytes);
        }
        Ok(values)
    }
    fn alternate(fd: i32) -> std::io::Result<u8> {
        let mut value = [255];
        read(fd, 0x81, 10, 0, 1, &mut value)?;
        Ok(value[0])
    }
    fn open(path: &str) -> Result<(File, Dac, DacStream, [i16; 2]), Box<dyn std::error::Error>> {
        let file = OpenOptions::new().read(true).write(true).open(path)?;
        let fd = file.as_raw_fd();
        let mut descriptor = [0; 18];
        read(fd, 0x80, 6, 0x0100, 0, &mut descriptor)?;
        assert_eq!(
            &descriptor[8..12],
            &[0xf4, 0x46, 0x02, 0x00],
            "probe is restricted to QEMU USB Audio"
        );
        for interface in [0u32, 1] {
            if unsafe { libc::ioctl(fd, 0x8004550fu32 as _, &interface) } != 0 {
                return Err(std::io::Error::last_os_error().into());
            }
        }
        let before = volume(fd)?;
        let info = DeviceInfo::new(0x46f4, 0x0002, "QEMU USB Audio")
            .with_device_name(path)
            .with_claimed_interfaces(vec![0u8, 1]);
        let dac = Dac::open(fd, info.clone(), DacConfig::default())?;
        assert!(
            matches!(Dac::open(fd, info, DacConfig::default()), Err(Error::Busy)),
            "duplicate attachment was admitted"
        );
        let format = dac.negotiate(&StreamRequest::pcm(48_000, 2, 16))?;
        let stream = dac.open_stream(&format, Arc::new(|event| eprintln!("{event:?}")))?;
        Ok((file, dac, stream, before))
    }
    fn feed(streams: &[&DacStream], duration: Duration) -> Result<(), Error> {
        let mut audio = Vec::new();
        for sample in 0..4800i16 {
            let value = (sample % 96 - 48) * 20;
            audio.extend_from_slice(&value.to_le_bytes());
            audio.extend_from_slice(&(-value).to_le_bytes());
        }
        for stream in streams {
            for _ in 0..3 {
                stream.write(&audio)?;
            }
        }
        let start = Instant::now();
        while start.elapsed() < duration {
            for stream in streams {
                let bytes = stream.writable_bytes().min(audio.len()) / 4 * 4;
                if bytes > 0 {
                    stream.write(&audio[..bytes])?;
                }
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        Ok(())
    }
    fn release(file: &File) -> std::io::Result<()> {
        for interface in [1u32, 0] {
            if unsafe { libc::ioctl(file.as_raw_fd(), 0x80045510u32 as _, &interface) } != 0 {
                return Err(std::io::Error::last_os_error());
            }
        }
        Ok(())
    }
    pub fn run() -> Result<(), Box<dyn std::error::Error>> {
        let (first_fd, first, first_stream, first_original) = open("/dev/bus/usb/001/002")?;
        let (second_fd, second, second_stream, second_original) = open("/dev/bus/usb/001/003")?;
        feed(&[&first_stream, &second_stream], Duration::from_secs(2))?;
        let a = first_stream.stats();
        let b = second_stream.stats();
        assert!(
            a.played_frames > 20_000
                && b.played_frames > 20_000
                && a.lost_frames == 0
                && b.lost_frames == 0
        );
        let second_level = volume(second_fd.as_raw_fd())?;
        let first_retirement = first.retirement();
        first_stream.close()?;
        first.close()?;
        assert!(first_retirement.wait(Duration::from_secs(2)));
        assert_eq!(alternate(first_fd.as_raw_fd())?, 0);
        assert_eq!(volume(first_fd.as_raw_fd())?, first_original);
        assert_eq!(alternate(second_fd.as_raw_fd())?, 1);
        assert_eq!(
            volume(second_fd.as_raw_fd())?,
            second_level,
            "first close changed second volume"
        );
        feed(&[&second_stream], Duration::from_secs(1))?;
        let after = second_stream.stats();
        assert!(after.played_frames > b.played_frames && after.lost_frames == 0);
        let second_retirement = second.retirement();
        second_stream.close()?;
        second.close()?;
        assert!(second_retirement.wait(Duration::from_secs(2)));
        assert_eq!(alternate(second_fd.as_raw_fd())?, 0);
        assert_eq!(volume(second_fd.as_raw_fd())?, second_original);
        release(&first_fd)?;
        release(&second_fd)?;
        println!("TWO_DACS_PASS first_frames={} second_frames={} independent volume/alt-zero/retirement/duplicate protection",a.played_frames,after.played_frames);
        Ok(())
    }
}
#[cfg(target_os = "linux")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    linux::run()
}
#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("two_dac_smoke requires a Linux guest with two QEMU UAC1 devices");
}
