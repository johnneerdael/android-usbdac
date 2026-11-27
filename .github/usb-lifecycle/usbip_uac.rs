//! Loopback-only USB/IP UAC2 device. Build with `rustc --edition 2021 usbip_uac.rs`.
//! Protocol: https://www.kernel.org/doc/html/latest/usb/usbip_protocol.html
//! Descriptor layouts: https://github.com/torvalds/linux/blob/master/include/linux/usb/audio-v2.h
use std::env;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};

const BUSID: &str = "1-1";
const MAX_BYTES: usize = 1 << 20;
const MAX_PACKETS: usize = 1024;
const DEVICE: [u8; 18] = [
    18, 1, 0, 2, 0xef, 2, 1, 64, 0x09, 0x12, 0xca, 0xda, 0, 1, 1, 2, 3, 1,
];
// Two interfaces; programmable SOF-synchronous clock ID 16; USB input terminal 1
// connects to speaker output terminal 2. EP 1 synchronous OUT, 48 bytes per 125 us.
const CONFIG: &[u8] = &[
    9, 2, 0, 0, 2, 1, 0, 0x80, 50, 8, 11, 0, 2, 1, 0, 0x20, 0, 9, 4, 0, 0, 0, 1, 1, 0x20, 0, 9,
    0x24, 1, 0, 2, 1, 46, 0, 0, 8, 0x24, 0x0a, 16, 7, 7, 0, 0, 17, 0x24, 2, 1, 1, 1, 0, 16, 2, 3,
    0, 0, 0, 0, 0, 0, 0, 12, 0x24, 3, 2, 1, 3, 0, 1, 16, 0, 0, 0, 9, 4, 1, 0, 0, 1, 2, 0x20, 0, 9,
    4, 1, 1, 1, 1, 2, 0x20, 0, 16, 0x24, 1, 1, 0, 1, 1, 0, 0, 0, 2, 3, 0, 0, 0, 0, 6, 0x24, 2, 1,
    2, 16, 7, 5, 1, 0x0d, 48, 0, 1, 8, 0x25, 1, 0, 0, 0, 0, 0,
];

#[derive(Clone, Copy, Debug, PartialEq)]
enum Mode {
    Normal,
    BadCur,
    StaleCur,
    Badd,
    FullUac3,
    FullUac3Clock,
    Duplex,
    Retire,
}

fn duplex_configuration() -> Vec<u8> {
    let mut c = CONFIG.to_vec();
    c[4] = 4;
    c.extend([
        8, 11, 2, 2, 1, 0, 0x20, 0, 9, 4, 2, 0, 0, 1, 1, 0x20, 0, 9, 0x24, 1, 0, 2, 3, 46, 0, 0, 8,
        0x24, 0x0a, 17, 7, 7, 0, 0, 17, 0x24, 2, 1, 1, 2, 0, 17, 2, 3, 0, 0, 0, 0, 0, 0, 0, 12,
        0x24, 3, 2, 1, 1, 0, 1, 17, 0, 0, 0, 9, 4, 3, 0, 0, 1, 2, 0x20, 0, 9, 4, 3, 1, 1, 1, 2,
        0x20, 0, 16, 0x24, 1, 2, 0, 1, 1, 0, 0, 0, 2, 3, 0, 0, 0, 0, 6, 0x24, 2, 1, 2, 16, 7, 5,
        0x82, 0x0d, 24, 0, 1, 8, 0x25, 1, 0, 0, 0, 0, 0,
    ]);
    let len = c.len() as u16;
    c[2..4].copy_from_slice(&len.to_le_bytes());
    c
}
impl Mode {
    fn has_uac3(self) -> bool {
        matches!(self, Self::Badd | Self::FullUac3 | Self::FullUac3Clock)
    }
}

// ADC3 AC header, programmable internal clock, USB input and speaker output.
fn full_uac3_clock_configuration() -> Vec<u8> {
    let mut c = vec![
        9, 2, 0, 0, 2, 3, 0, 0x80, 50, 8, 11, 0, 2, 1, 1, 0x30, 0, 9, 4, 0, 0, 0, 1, 1, 0x30, 0,
        10, 0x24, 1, 1, 61, 0, 0, 0, 0, 0, 12, 0x24, 0x0b, 9, 3, 7, 0, 0, 0, 0, 0, 0, 20, 0x24, 2,
        1, 1, 1, 0, 9, 0, 0, 0, 0, 2, 0, 0, 0, 0, 0, 0, 0, 19, 0x24, 3, 3, 1, 3, 0, 1, 9, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 9, 4, 1, 0, 0, 1, 2, 0x30, 0, 9, 4, 1, 1, 1, 1, 2, 0x30, 0, 23, 0x24,
        1, 1, 0, 0, 0, 0, 2, 0, 1, 0, 0, 0, 0, 0, 0, 0, 2, 16, 0, 0, 0, 7, 5, 1, 0x0d, 48, 0, 1,
        10, 0x25, 1, 0, 0, 0, 0, 0, 0, 0,
    ];
    let len = c.len() as u16;
    c[2..4].copy_from_slice(&len.to_le_bytes());
    c
}

fn configuration(index: u8) -> Option<Vec<u8>> {
    let mut c = match index {
        0 => CONFIG.to_vec(),
        1 => vec![
            9, 2, 0, 0, 2, 2, 0, 0x80, 50, 8, 11, 0, 2, 1, 0x22, 0x30, 0, 9, 4, 0, 0, 0, 1, 1,
            0x30, 0, 9, 4, 1, 0, 0, 1, 2, 0x30, 0, 9, 4, 1, 1, 2, 1, 2, 0x30, 0, 7, 5, 1, 0x05,
            196, 0, 4, 7, 5, 0x81, 0x11, 4, 0, 4, 9, 4, 1, 2, 2, 1, 2, 0x30, 0, 7, 5, 1, 0x05, 38,
            1, 4, 7, 5, 0x81, 0x11, 4, 0, 4,
        ],
        2 => vec![
            9, 2, 0, 0, 2, 3, 0, 0x80, 50, 8, 11, 0, 2, 1, 1, 0x30, 0, 9, 4, 0, 0, 0, 1, 1, 0x30,
            0, 10, 0x24, 1, 1, 109, 0, 0, 0, 0, 0, 12, 0x24, 0x0b, 9, 1, 7, 0, 0, 0, 0, 0, 0, 20,
            0x24, 2, 1, 1, 1, 0, 9, 0, 0, 0, 0, 2, 0, 0, 0, 0, 0, 0, 0, 20, 0x24, 2, 4, 1, 2, 0, 9,
            0, 0, 0, 0, 2, 0, 0, 0, 0, 0, 0, 0, 13, 0x24, 6, 8, 2, 1, 4, 3, 0, 0, 0, 0, 0, 19,
            0x24, 3, 3, 1, 3, 0, 8, 9, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 15, 0x24, 0x10, 10, 0x58,
            0x02, 0x70, 0x17, 4, 1, 4, 8, 3, 0, 0, 9, 4, 1, 0, 0, 1, 2, 0x30, 0, 9, 4, 1, 1, 2, 1,
            2, 0x30, 0, 23, 0x24, 1, 1, 0, 0, 0, 0, 2, 0, 1, 0, 0, 0, 0, 0, 0, 0, 2, 16, 0, 0, 0,
            7, 5, 1, 0x05, 196, 0, 4, 10, 0x25, 1, 0, 0, 0, 0, 0, 0, 0, 7, 5, 0x81, 0x11, 4, 0, 4,
        ],
        _ => return None,
    };
    let len = c.len() as u16;
    c[2..4].copy_from_slice(&len.to_le_bytes());
    Some(c)
}
const CLUSTER: [u8; 25] = [
    25, 0, 0x26, 0, 2, 0, 2, 6, 0, 0x20, 0, 2, 0, 3, 0, 0xff, 6, 0, 0x20, 0, 3, 0, 3, 0, 0xff,
];
struct Device {
    mode: Mode,
    rate: u32,
    old_rate: u32,
    config: u8,
    alt: u8,
    power: u8,
    selector: u8,
    mute: u8,
    volume: [i16; 2],
    capture_alt: u8,
    capture_rate: u32,
    shared: Arc<Mutex<Retirement>>,
}
#[derive(Default)]
struct Retirement {
    armed: bool,
    held_submit: Option<u32>,
    held_unlink: Option<u32>,
    capture_fault: bool,
    active: bool,
    release_pending: bool,
    writer: Option<Arc<Mutex<TcpStream>>>,
}
impl Retirement {
    fn status(&self) -> u8 {
        u8::from(self.armed)
            | (u8::from(self.held_submit.is_some()) << 1)
            | (u8::from(self.held_unlink.is_some()) << 2)
            | (u8::from(self.capture_fault) << 3)
            | (u8::from(self.active) << 4)
    }
    fn release(&mut self) -> io::Result<()> {
        let seq = self.held_unlink.ok_or_else(|| invalid("no held unlink"))?;
        let writer = self
            .writer
            .as_ref()
            .ok_or_else(|| invalid("no imported device"))?;
        let mut reply = Vec::with_capacity(48);
        put32(&mut reply, 4);
        put32(&mut reply, seq);
        reply.extend([0; 12]);
        put32(&mut reply, -104i32 as u32);
        reply.extend([0; 24]);
        writer.lock().unwrap().write_all(&reply)?;
        self.armed = false;
        self.held_submit = None;
        self.held_unlink = None;
        Ok(())
    }
}
impl Device {
    #[cfg(test)]
    fn new(mode: Mode) -> Self {
        Self::with_shared(mode, Arc::new(Mutex::new(Retirement::default())))
    }
    fn with_shared(mode: Mode, shared: Arc<Mutex<Retirement>>) -> Self {
        Self {
            mode,
            rate: 48_000,
            old_rate: 48_000,
            config: 0,
            alt: 0,
            power: 0,
            selector: 1,
            mute: 0,
            volume: [0, 0],
            capture_alt: 0,
            capture_rate: 48_000,
            shared,
        }
    }
    fn control(&mut self, s: [u8; 8], data: &[u8]) -> Result<Vec<u8>, ()> {
        let (typ, req) = (s[0], s[1]);
        let value = u16::from_le_bytes([s[2], s[3]]);
        let index = u16::from_le_bytes([s[4], s[5]]);
        let len = u16::from_le_bytes([s[6], s[7]]) as usize;
        let mut result = match (typ, req, value, index) {
            (0x80, 6, 0x0100, _) => {
                let mut d = DEVICE.to_vec();
                if self.mode.has_uac3() {
                    d[17] = 3;
                }
                d
            }
            (0x80, 6, v @ 0x0200..=0x0202, _) if v == 0x0200 || self.mode.has_uac3() => {
                if v == 0x0202 && self.mode == Mode::FullUac3Clock {
                    full_uac3_clock_configuration()
                } else if v == 0x0200 && self.mode == Mode::Duplex {
                    duplex_configuration()
                } else {
                    configuration(v as u8).ok_or(())?
                }
            }
            (0x80, 6, 0x0300, _) => vec![4, 3, 0x09, 0x04],
            (0x80, 6, v, _) if v >> 8 == 3 && (1..=3).contains(&(v as u8)) => {
                let text = match v as u8 {
                    1 => "Synthetic",
                    2 => "USB/IP UAC2",
                    _ => "lifecycle-1",
                };
                let mut v = vec![(2 + text.len() * 2) as u8, 3];
                for c in text.encode_utf16() {
                    v.extend(c.to_le_bytes());
                }
                v
            }
            (0x00, 5, _, 0) => vec![], // SET_ADDRESS is managed by vhci.
            (0x00, 9, 0..=3, 0) if value <= 1 || self.mode.has_uac3() => {
                self.config = value as u8;
                self.alt = 0;
                self.shared.lock().unwrap().active = false;
                self.capture_alt = 0;
                self.power = 0;
                self.selector = 1;
                vec![]
            }
            (0x80, 8, 0, 0) => vec![self.config],
            (0x80, 0, 0, 0) => vec![0, 0],
            (0x81, 0, 0, 0..=1) => vec![0, 0],
            (0x81, 0, 0, 2..=3) if self.mode == Mode::Duplex => vec![0, 0],
            (0x82, 0, 0, 1) => vec![0, 0],
            (0x01, 11, 0..=2, 1) if self.config != 0 && (value <= 1 || self.config == 2) => {
                self.alt = value as u8;
                self.shared.lock().unwrap().active = self.alt == 1;
                eprintln!("SET_INTERFACE alt={}", self.alt);
                vec![]
            }
            (0x81, 10, 0, 1) if self.config != 0 => vec![self.alt],
            (0x01, 11, 0..=1, 3) if self.mode == Mode::Duplex && self.config == 1 => {
                self.capture_alt = value as u8;
                eprintln!("SET_INTERFACE capture_alt={}", self.capture_alt);
                vec![]
            }
            (0x81, 10, 0, 3) if self.mode == Mode::Duplex && self.config == 1 => {
                vec![self.capture_alt]
            }
            (0x40, 0x5a, 1, 0) if matches!(self.mode, Mode::Duplex | Mode::Retire) && len == 0 => {
                let mut state = self.shared.lock().unwrap();
                if state.held_submit.is_some() {
                    return Err(());
                }
                state.armed = true;
                state.capture_fault = false;
                vec![]
            }
            (0x40, 0x5a, 2, 0) if matches!(self.mode, Mode::Duplex | Mode::Retire) && len == 0 => {
                let mut state = self.shared.lock().unwrap();
                if state.held_submit.is_some() && state.held_unlink.is_none() {
                    return Err(());
                }
                state.armed = false;
                state.release_pending = state.held_unlink.is_some();
                vec![]
            }
            (0xc0, 0x5a, 0, 0) if matches!(self.mode, Mode::Duplex | Mode::Retire) && len == 1 => {
                vec![self.shared.lock().unwrap().status() & 0x0f]
            }
            (0xa1, 1, 0x0100, 0x1102) if self.mode == Mode::Duplex => {
                self.capture_rate.to_le_bytes().to_vec()
            }
            (0xa1, 1, 0x0200, 0x1102) if self.mode == Mode::Duplex => vec![1],
            (0xa1, 2, 0x0100, 0x1102) if self.mode == Mode::Duplex => {
                let mut v = vec![1, 0];
                v.extend(48_000u32.to_le_bytes());
                v.extend(48_000u32.to_le_bytes());
                v.extend(0u32.to_le_bytes());
                v
            }
            (0x21, 1, 0x0100, 0x1102)
                if self.mode == Mode::Duplex && data == 48_000u32.to_le_bytes() =>
            {
                self.capture_rate = 48_000;
                vec![]
            }
            (0xa1, 6, 2, 0) if self.config == 3 => CLUSTER.to_vec(),
            (0xa1, 1, 0x0100, 0x0200) if self.config == 2 => vec![self.mute],
            (0x21, 1, 0x0100, 0x0200) if self.config == 2 && data.len() == 1 && data[0] <= 1 => {
                self.mute = data[0];
                vec![]
            }
            (0xa1, 1, v @ 0x0201..=0x0202, 0x0200) if self.config == 2 => self.volume
                [usize::from((v & 0xff) - 1)]
            .to_le_bytes()
            .to_vec(),
            (0x21, 1, v @ 0x0201..=0x0202, 0x0200) if self.config == 2 && data.len() == 2 => {
                self.volume[usize::from((v & 0xff) - 1)] = i16::from_le_bytes([data[0], data[1]]);
                vec![]
            }
            (0xa1, 2, 0x0201..=0x0202, 0x0200) if self.config == 2 => {
                vec![1, 0, 0, 0xa0, 0, 0, 0, 1]
            }
            (0xa1, 1, 0x0500, 0x0100 | 0x0300) if self.config == 2 => 0u32.to_le_bytes().to_vec(),
            (0xa1, 1, 0x1000, 0x0200) if self.config == 2 => 0u32.to_le_bytes().to_vec(),
            (0xa1, 1, 0x0200, 0x0a00)
                if self.config == 2 || (self.config == 3 && self.mode != Mode::FullUac3Clock) =>
            {
                vec![self.power]
            }
            (0x21, 1, 0x0200, 0x0a00)
                if (self.config == 2 || (self.config == 3 && self.mode != Mode::FullUac3Clock))
                    && len == 1
                    && data.len() == 1
                    && data[0] <= 2 =>
            {
                self.power = data[0];
                eprintln!("POWER {}", self.power);
                vec![]
            }
            (0xa1, 1, 0x0100, 0x0800)
                if self.config == 3 && self.mode != Mode::FullUac3Clock && self.power == 0 =>
            {
                vec![self.selector]
            }
            (0x21, 1, 0x0100, 0x0800)
                if self.config == 3
                    && self.mode != Mode::FullUac3Clock
                    && self.power == 0
                    && len == 1
                    && data.len() == 1
                    && (1..=2).contains(&data[0]) =>
            {
                self.selector = data[0];
                eprintln!("SELECTOR {}", self.selector);
                vec![]
            }
            (0xa1, 1, 0x0100, 0x0900) if self.config >= 2 => if self.mode == Mode::FullUac3Clock {
                self.rate
            } else {
                48_000
            }
            .to_le_bytes()
            .to_vec(),
            (0xa1, 1, 0x0200, 0x0900) if self.config >= 2 => vec![1],
            (0xa1, 2, 0x0100, 0x0900) if self.config >= 2 => {
                let mut v = vec![
                    if self.mode == Mode::FullUac3Clock {
                        2
                    } else {
                        1
                    },
                    0,
                ];
                for rate in
                    [48_000u32, 96_000]
                        .into_iter()
                        .take(if self.mode == Mode::FullUac3Clock {
                            2
                        } else {
                            1
                        })
                {
                    v.extend(rate.to_le_bytes());
                    v.extend(rate.to_le_bytes());
                    v.extend(0u32.to_le_bytes());
                }
                v
            }
            (0x21, 1, 0x0100, 0x0900)
                if self.config == 3 && self.mode == Mode::FullUac3Clock && data.len() == 4 =>
            {
                let rate = u32::from_le_bytes(data.try_into().unwrap());
                if ![48_000, 96_000].contains(&rate) {
                    return Err(());
                }
                self.rate = rate;
                vec![]
            }
            (0x21, 1, 0x0100, 0x0900) if self.config >= 2 && data == 48_000u32.to_le_bytes() => {
                vec![]
            }
            (0xa1, 1, 0x0100, 0x1000) => {
                if self.alt == 1 && self.mode == Mode::BadCur {
                    eprintln!("GET_CUR stalled alt=1");
                    return Err(());
                }
                let rate = if self.alt == 1 {
                    match self.mode {
                        Mode::Normal => self.rate,
                        Mode::BadCur => unreachable!(),
                        Mode::StaleCur => self.old_rate,
                        Mode::Badd
                        | Mode::FullUac3
                        | Mode::FullUac3Clock
                        | Mode::Duplex
                        | Mode::Retire => self.rate,
                    }
                } else {
                    self.rate
                };
                eprintln!("GET_CUR rate={rate} alt={}", self.alt);
                rate.to_le_bytes().to_vec()
            }
            (0xa1, 1, 0x0200, 0x1000) => vec![1],
            (0xa1, 2, 0x0100, 0x1000) => {
                let mut v = vec![2, 0];
                for rate in [48_000u32, 96_000] {
                    v.extend(rate.to_le_bytes());
                    v.extend(rate.to_le_bytes());
                    v.extend(0u32.to_le_bytes());
                }
                v
            }
            (0x21, 1, 0x0100, 0x1000) if len == 4 && data.len() == 4 => {
                let rate = u32::from_le_bytes(data.try_into().unwrap());
                if ![48_000, 96_000].contains(&rate) {
                    return Err(());
                }
                if rate != self.rate {
                    self.old_rate = self.rate;
                }
                self.rate = rate;
                eprintln!("SET_CUR rate={rate}");
                vec![]
            }
            _ => {
                eprintln!("STALL setup={s:02x?}");
                return Err(());
            }
        };
        result.truncate(len);
        Ok(result)
    }
}
fn be32(b: &[u8]) -> u32 {
    u32::from_be_bytes(b.try_into().unwrap())
}
fn put32(v: &mut Vec<u8>, n: u32) {
    v.extend(n.to_be_bytes());
}
fn fixed(v: &mut Vec<u8>, s: &str, n: usize) {
    let mut b = vec![0; n];
    b[..s.len()].copy_from_slice(s.as_bytes());
    v.extend(b);
}
fn devinfo(mode: Mode) -> Vec<u8> {
    let mut v = Vec::new();
    fixed(&mut v, "/sys/devices/virtual/usbip/1-1", 256);
    fixed(&mut v, BUSID, 32);
    for n in [1u32, 1, 3] {
        put32(&mut v, n);
    } // USB_SPEED_HIGH
    for n in [0x1209u16, 0xdaca, 0x0100] {
        v.extend(n.to_be_bytes());
    }
    v.extend([
        0xef,
        2,
        1,
        1,
        if mode.has_uac3() { 3 } else { 1 },
        if mode == Mode::Duplex { 4 } else { 2 },
    ]);
    v
}
fn op_reply(code: u16, status: u32) -> Vec<u8> {
    let mut v = vec![1, 0x11];
    v.extend(code.to_be_bytes());
    put32(&mut v, status);
    v
}
fn serve(mut stream: TcpStream, mode: Mode, shared: Arc<Mutex<Retirement>>) -> io::Result<()> {
    stream.set_nodelay(true)?;
    let mut op = [0u8; 8];
    stream.read_exact(&mut op)?;
    if op[..2] != [1, 0x11] || op[4..] != [0; 4] {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "bad USB/IP operation header",
        ));
    }
    match u16::from_be_bytes([op[2], op[3]]) {
        0x8005 => {
            let mut v = op_reply(5, 0);
            put32(&mut v, 1);
            v.extend(devinfo(mode));
            v.extend([1, 1, 0x20, 0, 1, 2, 0x20, 0]);
            if mode == Mode::Duplex {
                v.extend([1, 1, 0x20, 0, 1, 2, 0x20, 0]);
            }
            stream.write_all(&v)
        }
        0x8003 => {
            let mut busid = [0u8; 32];
            stream.read_exact(&mut busid)?;
            if busid.split(|b| *b == 0).next() != Some(BUSID.as_bytes()) {
                return stream.write_all(&op_reply(3, 1));
            }
            let mut v = op_reply(3, 0);
            v.extend(devinfo(mode));
            stream.write_all(&v)?;
            eprintln!("imported {BUSID} mode={mode:?}");
            let result = urb_loop(stream, Device::with_shared(mode, shared.clone()));
            *shared.lock().unwrap() = Retirement::default();
            result
        }
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unknown operation",
        )),
    }
}
fn invalid(msg: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg)
}
fn urb_loop(mut s: TcpStream, mut dev: Device) -> io::Result<()> {
    let writer = Arc::new(Mutex::new(s.try_clone()?));
    dev.shared.lock().unwrap().writer = Some(writer.clone());
    loop {
        let mut h = [0u8; 48];
        match s.read_exact(&mut h) {
            Ok(()) => (),
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(e) => return Err(e),
        }
        let cmd = be32(&h[0..4]);
        let seq = be32(&h[4..8]);
        let mut reply = Vec::with_capacity(48);
        put32(&mut reply, if cmd == 1 { 3 } else { 4 });
        put32(&mut reply, seq);
        reply.extend([0; 12]);
        if cmd == 2 {
            let target = be32(&h[20..24]);
            let mut state = dev.shared.lock().unwrap();
            if state.held_submit == Some(target) && state.held_unlink.is_none() {
                state.held_unlink = Some(seq);
                eprintln!("held unlink seq={seq} target={target}");
                continue;
            }
            drop(state);
            reply.extend([0; 28]);
            writer.lock().unwrap().write_all(&reply)?;
            continue;
        }
        if cmd != 1 {
            return Err(invalid("unknown URB command"));
        }
        let dir = be32(&h[12..16]);
        let ep = be32(&h[16..20]);
        let len = be32(&h[24..28]) as usize;
        let packets_raw = be32(&h[32..36]);
        let packets = if packets_raw == u32::MAX {
            0
        } else {
            packets_raw as usize
        };
        if dir > 1 || len > MAX_BYTES || packets > MAX_PACKETS {
            return Err(invalid("URB exceeds fixture limits"));
        }
        let mut data = vec![0; if dir == 0 { len } else { 0 }];
        s.read_exact(&mut data)?;
        let mut iso = vec![0; packets * 16];
        s.read_exact(&mut iso)?;
        let (status, actual, payload) = if ep == 0 && packets == 0 {
            let setup: [u8; 8] = h[40..48].try_into().unwrap();
            match dev.control(setup, &data) {
                Ok(v) => {
                    let n = if dir == 1 { v.len() } else { data.len() };
                    (0, n, v)
                }
                Err(()) => (-32i32, 0, vec![]),
            }
        } else if ep == 1 && dir == 0 && packets > 0 && dev.config != 0 && dev.alt != 0 {
            let mut total = 0usize;
            let max_packet = match (dev.config, dev.alt) {
                (1, 1) => 48,
                (3, 1) if dev.mode == Mode::FullUac3Clock => 48,
                (2, 1) | (3, 1) => 196,
                (2, 2) => 294,
                _ => return Err(invalid("invalid alternate")),
            };
            for packet in iso.chunks_exact_mut(16) {
                let offset = be32(&packet[..4]) as usize;
                let length = be32(&packet[4..8]) as usize;
                if length > max_packet
                    || offset
                        .checked_add(length)
                        .filter(|end| *end <= len)
                        .is_none()
                {
                    return Err(invalid("invalid ISO packet"));
                }
                packet[8..12].copy_from_slice(&(length as u32).to_be_bytes());
                packet[12..16].copy_from_slice(&0u32.to_be_bytes());
                total += length;
            }
            if dev.mode == Mode::Retire {
                let mut state = dev.shared.lock().unwrap();
                if state.armed {
                    state.armed = false;
                    state.held_submit = Some(seq);
                    eprintln!("held OUT submit seq={seq}");
                    continue;
                }
            }
            (0, total, vec![])
        } else if ep == 2
            && dir == 1
            && packets > 0
            && dev.mode == Mode::Duplex
            && dev.config == 1
            && dev.capture_alt == 1
        {
            let mut state = dev.shared.lock().unwrap();
            let fault = state.armed;
            if fault {
                state.armed = false;
                state.capture_fault = true;
                eprintln!("capture fault seq={seq}");
            }
            drop(state);
            let mut payload = Vec::new();
            for packet in iso.chunks_exact_mut(16) {
                let offset = be32(&packet[..4]) as usize;
                let length = be32(&packet[4..8]) as usize;
                if length > 24
                    || length % 4 != 0
                    || offset
                        .checked_add(length)
                        .filter(|end| *end <= len)
                        .is_none()
                {
                    return Err(invalid("invalid capture packet"));
                }
                let actual = if fault { 0 } else { length };
                packet[8..12].copy_from_slice(&(actual as u32).to_be_bytes());
                packet[12..16].copy_from_slice(&(if fault { -32i32 } else { 0 }).to_be_bytes());
                for _ in 0..actual / 4 {
                    payload.extend([0x34, 0x12, 0xcc, 0xed]);
                }
            }
            (if fault { -32i32 } else { 0 }, payload.len(), payload)
        } else if ep == 1 && dir == 1 && packets > 0 && dev.config >= 2 && dev.alt != 0 {
            let mut payload = Vec::with_capacity(packets * 4);
            for packet in iso.chunks_exact_mut(16) {
                let offset = be32(&packet[..4]) as usize;
                let length = be32(&packet[4..8]) as usize;
                if length < 4
                    || offset
                        .checked_add(length)
                        .filter(|end| *end <= len)
                        .is_none()
                {
                    return Err(invalid("invalid feedback packet"));
                }
                packet[8..12].copy_from_slice(&4u32.to_be_bytes());
                packet[12..16].copy_from_slice(&0u32.to_be_bytes());
                payload.extend([0, 0, 6, 0]);
            }
            (0, payload.len(), payload)
        } else {
            (-32i32, 0, vec![])
        };
        put32(&mut reply, status as u32);
        put32(&mut reply, actual as u32);
        put32(&mut reply, be32(&h[28..32]));
        put32(&mut reply, packets_raw);
        put32(&mut reply, 0);
        reply.extend([0; 8]);
        if dir == 1 {
            reply.extend(payload);
        }
        reply.extend(iso);
        writer.lock().unwrap().write_all(&reply)?;
        let mut state = dev.shared.lock().unwrap();
        if state.release_pending {
            state.release_pending = false;
            state.release()?;
        }
    }
}
#[cfg(target_os = "linux")]
fn attach(port: u16) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    let mut stream = TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, port))?;
    let mut request = vec![1, 0x11, 0x80, 3, 0, 0, 0, 0];
    fixed(&mut request, BUSID, 32);
    stream.write_all(&request)?;
    let mut header = [0; 8];
    stream.read_exact(&mut header)?;
    if header != [1, 0x11, 0, 3, 0, 0, 0, 0] {
        return Err(invalid("USB/IP import rejected"));
    }
    let mut info = vec![0; 312];
    stream.read_exact(&mut info)?;
    if info != devinfo(Mode::Normal) && info != devinfo(Mode::Badd) && info != devinfo(Mode::Duplex)
    {
        return Err(invalid("unexpected imported device identity"));
    }
    // vhci_hcd acquires its own reference to the socket before this write returns.
    std::fs::write(
        "/sys/devices/platform/vhci_hcd.0/attach",
        format!("0 {} 65537 3\n", stream.as_raw_fd()),
    )?;
    eprintln!("attached {BUSID} to vhci port 0");
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn attach(_port: u16) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "VHCI attach requires Linux",
    ))
}

fn control_connection(mut stream: TcpStream, shared: &Arc<Mutex<Retirement>>) -> io::Result<()> {
    stream.set_read_timeout(Some(std::time::Duration::from_secs(2)))?;
    let mut command = String::new();
    BufReader::new(stream.try_clone()?.take(32)).read_line(&mut command)?;
    let response = match command.as_str() {
        "status\n" => format!("{}\n", shared.lock().unwrap().status()),
        "release\n" => match shared.lock().unwrap().release() {
            Ok(()) => "ok\n".to_string(),
            Err(e) => format!("error {e}\n"),
        },
        _ => "error invalid command\n".to_string(),
    };
    stream.write_all(response.as_bytes())
}
fn control_client(port: u16, command: &str) -> io::Result<()> {
    if !matches!(command, "status" | "release") {
        return Err(invalid("control must be status or release"));
    }
    let control_port = port
        .checked_add(100)
        .ok_or_else(|| invalid("control port overflow"))?;
    let mut stream = TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, control_port))?;
    stream.set_read_timeout(Some(std::time::Duration::from_secs(3)))?;
    writeln!(stream, "{command}")?;
    let mut response = String::new();
    BufReader::new(stream.take(128)).read_line(&mut response)?;
    if response.starts_with("error") || response.is_empty() {
        return Err(io::Error::other(response));
    }
    print!("{response}");
    Ok(())
}
fn main() -> io::Result<()> {
    let mut mode = Mode::Normal;
    let mut port = 3240u16;
    let mut attach_only = false;
    let mut control = None;
    let mut args = env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--attach" => attach_only = true,
            "--control" => control = Some(args.next().ok_or_else(|| invalid("missing control command"))?),
            "--mode" => {
                mode = match args.next().as_deref() {
                    Some("normal") => Mode::Normal,
                    Some("bad-cur") => Mode::BadCur,
                    Some("stale-cur") => Mode::StaleCur,
                    Some("badd") => Mode::Badd,
                    Some("full-uac3") => Mode::FullUac3,
                    Some("full-uac3-clock") => Mode::FullUac3Clock,
                    Some("duplex") => Mode::Duplex,
                    Some("retire") => Mode::Retire,
                    _ => return Err(invalid("unknown mode")),
                }
            }
            "--port" => {
                port = args
                    .next()
                    .and_then(|p| p.parse().ok())
                    .filter(|p| *p != 0)
                    .ok_or_else(|| invalid("invalid port"))?;
            }
            _ => {
                return Err(invalid(
                    "usage: usbip_uac [--attach | --control status|release] [--mode normal|bad-cur|stale-cur|badd|full-uac3|full-uac3-clock|duplex|retire] [--port PORT]",
                ))
            }
        }
    }
    if let Some(command) = control {
        return control_client(port, &command);
    }
    if attach_only {
        return attach(port);
    }
    let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port))?;
    let shared = Arc::new(Mutex::new(Retirement::default()));
    if mode == Mode::Retire {
        let control_port = port
            .checked_add(100)
            .ok_or_else(|| invalid("control port overflow"))?;
        let control_listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, control_port))?;
        let control_state = shared.clone();
        std::thread::spawn(move || {
            for stream in control_listener.incoming() {
                match stream {
                    Ok(stream) => {
                        if let Err(e) = control_connection(stream, &control_state) {
                            eprintln!("control: {e}");
                        }
                    }
                    Err(e) => eprintln!("control accept: {e}"),
                }
            }
        });
        eprintln!("control on 127.0.0.1:{control_port}");
    }
    eprintln!("listening on 127.0.0.1:{port}, busid {BUSID}, mode {mode:?}");
    for stream in listener.incoming() {
        match stream {
            Ok(s) => {
                if let Err(e) = serve(s, mode, shared.clone()) {
                    eprintln!("connection ended: {e}");
                }
            }
            Err(e) => eprintln!("accept: {e}"),
        }
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    fn req(t: u8, r: u8, v: u16, i: u16, l: u16) -> [u8; 8] {
        let mut s = [t, r, 0, 0, 0, 0, 0, 0];
        s[2..4].copy_from_slice(&v.to_le_bytes());
        s[4..6].copy_from_slice(&i.to_le_bytes());
        s[6..8].copy_from_slice(&l.to_le_bytes());
        s
    }
    #[test]
    fn descriptors_are_well_formed() {
        let mut d = Device::new(Mode::Normal);
        let c = d.control(req(0x80, 6, 0x0200, 0, 255), &[]).unwrap();
        assert_eq!(u16::from_le_bytes([c[2], c[3]]) as usize, c.len());
        let mut i = 0;
        while i < c.len() {
            let n = c[i] as usize;
            assert!(n >= 2 && i + n <= c.len());
            i += n;
        }
        assert_eq!(i, c.len());
        assert_eq!(devinfo(Mode::Normal).len(), 312);
    }
    #[test]
    fn rate_and_injection() {
        for mode in [Mode::Normal, Mode::BadCur, Mode::StaleCur] {
            let mut d = Device::new(mode);
            let get = req(0xa1, 1, 0x100, 0x1000, 4);
            let set = req(0x21, 1, 0x100, 0x1000, 4);
            assert!(d.control(set, &44_100u32.to_le_bytes()).is_err());
            d.control(set, &96_000u32.to_le_bytes()).unwrap();
            d.control(set, &96_000u32.to_le_bytes()).unwrap();
            assert_eq!(d.control(get, &[]).unwrap(), 96_000u32.to_le_bytes());
            d.control(req(0, 9, 1, 0, 0), &[]).unwrap();
            d.control(req(1, 11, 1, 1, 0), &[]).unwrap();
            match mode {
                Mode::Normal => assert_eq!(d.control(get, &[]).unwrap(), 96_000u32.to_le_bytes()),
                Mode::BadCur => assert!(d.control(get, &[]).is_err()),
                Mode::StaleCur => assert_eq!(d.control(get, &[]).unwrap(), 48_000u32.to_le_bytes()),
                Mode::Badd | Mode::FullUac3 | Mode::FullUac3Clock | Mode::Duplex | Mode::Retire => {
                    unreachable!()
                }
            }
            let range = d.control(req(0xa1, 2, 0x100, 0x1000, 255), &[]).unwrap();
            assert_eq!(range.len(), 26);
            assert_eq!(&range[..2], &[2, 0]);
        }
    }
    #[test]
    fn uac3_configurations_and_powered_selector() {
        for mode in [Mode::Badd, Mode::FullUac3, Mode::FullUac3Clock] {
            let mut d = Device::new(mode);
            assert_eq!(d.control(req(0x80, 6, 0x100, 0, 18), &[]).unwrap()[17], 3);
            for index in 0..3 {
                let c = d.control(req(0x80, 6, 0x200 + index, 0, 255), &[]).unwrap();
                assert_eq!(c[5], index as u8 + 1);
                assert_eq!(u16::from_le_bytes([c[2], c[3]]) as usize, c.len());
                let mut p = 0;
                while p < c.len() {
                    let n = c[p] as usize;
                    assert!(n >= 2 && p + n <= c.len());
                    p += n;
                }
                assert_eq!(p, c.len());
            }
            d.control(req(0, 9, 2, 0, 0), &[]).unwrap();
            let power = req(0xa1, 1, 0x200, 0xa00, 1);
            assert_eq!(d.control(power, &[]).unwrap(), [0]);
            for state in 0..=2 {
                d.control(req(0x21, 1, 0x200, 0xa00, 1), &[state]).unwrap();
                assert_eq!(d.control(power, &[]).unwrap(), [state]);
            }
            assert!(d.control(req(0x21, 1, 0x200, 0xa00, 1), &[3]).is_err());
            d.control(req(0, 9, 2, 0, 0), &[]).unwrap();
            assert_eq!(d.control(power, &[]).unwrap(), [0]);
            if mode == Mode::FullUac3Clock {
                continue;
            }
            d.control(req(0, 9, 3, 0, 0), &[]).unwrap();
            assert_eq!(d.control(req(0xa1, 6, 2, 0, 6), &[]).unwrap(), CLUSTER[..6]);
            assert_eq!(d.control(req(0xa1, 6, 2, 0, 25), &[]).unwrap(), CLUSTER);
            let selector = req(0xa1, 1, 0x100, 0x800, 1);
            assert_eq!(d.control(power, &[]).unwrap(), [0]);
            d.control(req(0x21, 1, 0x200, 0xa00, 1), &[2]).unwrap();
            assert!(d.control(selector, &[]).is_err());
            d.control(req(0x21, 1, 0x200, 0xa00, 1), &[0]).unwrap();
            assert_eq!(d.control(selector, &[]).unwrap(), [1]);
            d.control(req(0x21, 1, 0x100, 0x800, 1), &[2]).unwrap();
            assert_eq!(d.control(selector, &[]).unwrap(), [2]);
            d.control(req(0x21, 1, 0x200, 0xa00, 1), &[2]).unwrap();
            assert!(d.control(selector, &[]).is_err());
        }
        assert_eq!(configuration(1).unwrap().len(), 81);
    }
    #[test]
    fn full_uac3_direct_clock_controls_and_topology() {
        let mut d = Device::new(Mode::FullUac3Clock);
        let c = d.control(req(0x80, 6, 0x202, 0, 512), &[]).unwrap();
        assert_eq!(u16::from_le_bytes([c[2], c[3]]) as usize, c.len());
        let mut pos = 0;
        let mut ac_lengths = Vec::new();
        while pos < c.len() {
            let n = c[pos] as usize;
            assert!(n >= 2 && pos + n <= c.len());
            if c[pos + 1] == 0x24 && pos < 100 {
                ac_lengths.push(n);
            }
            pos += n;
        }
        assert_eq!(ac_lengths, [10, 12, 20, 19]);
        d.control(req(0, 9, 3, 0, 0), &[]).unwrap();
        let get = req(0xa1, 1, 0x100, 0x900, 4);
        let set = req(0x21, 1, 0x100, 0x900, 4);
        assert_eq!(d.control(get, &[]).unwrap(), 48_000u32.to_le_bytes());
        d.control(set, &96_000u32.to_le_bytes()).unwrap();
        assert_eq!(d.control(get, &[]).unwrap(), 96_000u32.to_le_bytes());
        assert!(d.control(set, &44_100u32.to_le_bytes()).is_err());
        assert_eq!(d.control(req(0xa1, 1, 0x200, 0x900, 1), &[]).unwrap(), [1]);
        let range = d.control(req(0xa1, 2, 0x100, 0x900, 26), &[]).unwrap();
        assert_eq!(&range[..2], &[2, 0]);
        assert_eq!(&range[2..6], &48_000u32.to_le_bytes());
        assert_eq!(&range[14..18], &96_000u32.to_le_bytes());
        assert!(d.control(req(0xa1, 1, 0x200, 0xa00, 1), &[]).is_err());
    }
    #[test]
    fn duplex_descriptor_and_independent_controls() {
        let mut d = Device::new(Mode::Duplex);
        let c = d.control(req(0x80, 6, 0x200, 0, 512), &[]).unwrap();
        assert_eq!(c[4], 4);
        assert_eq!(u16::from_le_bytes([c[2], c[3]]) as usize, c.len());
        let mut pos = 0;
        while pos < c.len() {
            let n = c[pos] as usize;
            assert!(n >= 2 && pos + n <= c.len());
            pos += n;
        }
        d.control(req(0, 9, 1, 0, 0), &[]).unwrap();
        d.control(req(1, 11, 1, 3, 0), &[]).unwrap();
        assert_eq!(d.control(req(0x81, 10, 0, 3, 1), &[]).unwrap(), [1]);
        assert_eq!(d.control(req(0x81, 10, 0, 1, 1), &[]).unwrap(), [0]);
        d.control(req(0x21, 1, 0x100, 0x1000, 4), &96_000u32.to_le_bytes())
            .unwrap();
        assert_eq!(
            d.control(req(0xa1, 1, 0x100, 0x1102, 4), &[]).unwrap(),
            48_000u32.to_le_bytes()
        );
        d.control(req(0x40, 0x5a, 1, 0, 0), &[]).unwrap();
        assert_eq!(d.control(req(0xc0, 0x5a, 0, 0, 1), &[]).unwrap(), [1]);
        assert!(Device::new(Mode::Normal)
            .control(req(0x40, 0x5a, 1, 0, 0), &[])
            .is_err());
    }
    #[test]
    fn out_of_band_status_and_release_held_unlink() {
        use std::thread;
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        let mut client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        client
            .set_read_timeout(Some(std::time::Duration::from_secs(2)))
            .unwrap();
        let (server, _) = listener.accept().unwrap();
        let shared = Arc::new(Mutex::new(Retirement::default()));
        let device_state = shared.clone();
        let worker = thread::spawn(move || {
            urb_loop(server, Device::with_shared(Mode::Retire, device_state)).unwrap()
        });
        fn submit(
            s: &mut TcpStream,
            seq: u32,
            dir: u32,
            ep: u32,
            setup: [u8; 8],
            data: &[u8],
            iso: bool,
        ) {
            let mut h = vec![];
            for n in [
                1,
                seq,
                0x10001,
                dir,
                ep,
                0,
                data.len() as u32,
                0,
                if iso { 1 } else { u32::MAX },
                1,
            ] {
                put32(&mut h, n);
            }
            h.extend(setup);
            h.extend(data);
            if iso {
                for n in [0, data.len() as u32, 0, 0] {
                    put32(&mut h, n);
                }
            }
            s.write_all(&h).unwrap();
        }
        fn response(s: &mut TcpStream) -> [u8; 48] {
            let mut b = [0; 48];
            s.read_exact(&mut b).unwrap();
            b
        }
        submit(&mut client, 1, 0, 0, req(0, 9, 1, 0, 0), &[], false);
        assert_eq!(be32(&response(&mut client)[4..8]), 1);
        submit(&mut client, 2, 0, 0, req(1, 11, 1, 1, 0), &[], false);
        response(&mut client);
        submit(&mut client, 3, 0, 0, req(0x40, 0x5a, 1, 0, 0), &[], false);
        response(&mut client);
        submit(&mut client, 4, 0, 1, [0; 8], &[0; 4], true);
        let mut unlink = vec![];
        for n in [2, 5, 0x10001, 0, 0, 4, 0, 0, 0, 0, 0, 0] {
            put32(&mut unlink, n);
        }
        client.write_all(&unlink).unwrap();
        // A later request proves the preceding unlink has reached the server.
        submit(&mut client, 6, 0, 0, req(0, 5, 0, 0, 0), &[], false);
        assert_eq!(be32(&response(&mut client)[4..8]), 6);
        fn control(state: Arc<Mutex<Retirement>>, command: &str) -> String {
            let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
            let mut client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
            let (server, _) = listener.accept().unwrap();
            let worker = thread::spawn(move || control_connection(server, &state).unwrap());
            writeln!(client, "{command}").unwrap();
            let mut answer = String::new();
            BufReader::new(client).read_line(&mut answer).unwrap();
            worker.join().unwrap();
            answer
        }
        assert_eq!(control(shared.clone(), "status"), "22\n");
        assert_eq!(control(shared.clone(), "release"), "ok\n");
        let unlinked = response(&mut client);
        assert_eq!(
            (
                be32(&unlinked[0..4]),
                be32(&unlinked[4..8]),
                be32(&unlinked[20..24]) as i32
            ),
            (4, 5, -104)
        );
        assert_eq!(control(shared, "status"), "16\n");
        drop(client);
        worker.join().unwrap();
    }
}
