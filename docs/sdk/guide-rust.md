# Integrate usbdac-core into a Rust app, step by step

This guide is for a Rust app or audio engine that drives a USB DAC through the `usbdac-core` crate: a Rust player on Android, a cross-platform engine with a Kotlin or Flutter shell, or a Linux program. It starts with an empty crate and ends with a player that opens a DAC, plays, seeks, controls the volume, reports whether the output is bit-perfect and closes cleanly. Every step says what to do, why, shows the code and says how to check it.

If you build an Android app in Kotlin on Media3, read [the Kotlin guide](guide-kotlin.md) instead. It uses the same driver through the Android library.

<!-- rust-checked -->

The code is compiled. It is in `crates/usbdac-core/examples` (`guide_open.rs` and `guide_player.rs`) and the documentation build checks that every Rust block on this page is an excerpt of those files or of `crates/usbdac-core/tests/guide_player.rs`. The Kotlin blocks come from the sample app in the same way.

**Status.** The examples compile on the host and for `aarch64-linux-android`, and the writer thread and transport have 5 tests that run against a fake ring, with no DAC. No code on this page has been run with a DAC in front of it, and the Linux part has not been run on a Linux desktop. The driver underneath has played on hardware ([hardware status](evidence.md#hardware-status)). Run the checks of [step 13](#13-test-it) on your own devices before you ship.

## Words used on this page

- **DAC.** The box that turns digital audio into sound. Here it is a USB one.
- **usbfs.** The Linux and Android interface through which a program talks to a USB device with a file descriptor. The crate drives the DAC through it.
- **Claiming an interface.** A USB device has interfaces (audio control, audio streaming). Only one driver can own an interface. Claiming takes it from the system's audio driver so that your app can use it.
- **Ring.** The buffer between your writer thread and the USB output thread. The library owns it.
- **Frame.** One sample for every channel. Stereo 24-bit audio has 2 samples per frame.
- **Bit-perfect.** The samples reach the DAC exactly as your decoder produced them.

## What you build

| Piece | Job | Step |
|---|---|---|
| `UsbHost` | Your side of USB: the descriptor and the claims | 2, 3 |
| `open_dac`, `negotiate` | Open the DAC and settle a format per track | 3, 4 |
| `Source`, `Ring`, `Player` | The writer thread, with seek, pause and position | 6, 7 |
| `event_channel`, `react` | Events from the library, and what to do about each | 5, 9 |
| `restore_level`, `status_line`, `shutdown` | Volume, the bit-perfect line and a clean close | 8, 10, 11 |

The library is a push design. It owns a ring and one USB output thread. You write audio into the ring from a thread of your own. There is no render callback into your code on the audio path.

## 1. Set up the crate

**Do.** Add the crate.

```sh
cargo add usbdac-core@0.1.13
```

The crate has no Cargo features. It needs Rust 1.76 or later. Its dependencies are `libc`, `libusb1-sys`, `rusb`, `log`, `parking_lot`, `once_cell`, `crossbeam-channel`, `serde`, `serde_json`, `base64` and `flate2`. The libusb sources are compiled with your build, so you need a C compiler for the target.

For Android, add the target and make the NDK's clang reachable:

```sh
rustup target add aarch64-linux-android
export ANDROID_NDK_HOME=$HOME/Library/Android/sdk/ndk/28.2.13676358
NDKBIN=$ANDROID_NDK_HOME/toolchains/llvm/prebuilt/darwin-x86_64/bin   # linux-x86_64 on Linux
PATH=$NDKBIN:$PATH CC_aarch64_linux_android=aarch64-linux-android26-clang \
  AR_aarch64_linux_android=llvm-ar cargo check --target aarch64-linux-android
```

`cargo ndk` sets the same variables for you. The project pins NDK 28.2.13676358.

Log through the `log` facade and install a logger before you open a DAC: `android_logger` on Android, `env_logger` on Linux. The library logs through it at the levels of the `log` crate.

**Why.** The core has no JNI and no Android types, so the same crate serves Android and Linux. On macOS it compiles and its logic tests run, but `Dac::open` returns `Error::PlatformUnsupported`; macOS playback goes through the separate `Dac::open_darwin` entry point instead ([the native API document](API.md#dac)). Windows is not a supported host: the crate uses Unix file descriptors.

**Limits to know now.** One owner is allowed per physical attachment; distinct attachments can have independent owners. Opening an already owned attachment returns `Error::Busy`. If the same process also loads the Kotlin library of usbdac, that library has its own copy of the driver and its own state: do not open the same DAC from both.

**Check.** `cargo check --examples` passes. On a Mac a call to `Dac::open` returns `Error::PlatformUnsupported`. That is the expected answer and it shows that the crate is linked.

## 2. Get a file descriptor and permission

The library does not enumerate USB devices, ask for permission or claim interfaces. It takes an open usbfs file descriptor and the list of interfaces you claimed on it. Both platforms follow the same rule: **the descriptor you give the library must be the one that claimed the interfaces.** The kernel ties a claim to the file it was made on.

### On Android

**Do.** Only Kotlin or Java can ask for USB permission and open the device, so a small Kotlin part comes first. It asks for permission from an activity that is in front, opens the device and claims the interfaces. You also need the attach intent filter and device filter of [the Android setup guide](getting-started.md#2-the-manifest).

```kotlin
/** Asks for USB permission. Call it from an activity that is in front: the dialog needs a screen. */
fun requestUsbPermission(context: Context, device: UsbDevice, onResult: (Boolean) -> Unit) {
    val manager = context.getSystemService(UsbManager::class.java)
    if (manager.hasPermission(device)) return onResult(true)
    val action = "${context.packageName}.USB_PERMISSION"
    val receiver = object : BroadcastReceiver() {
        override fun onReceive(received: Context, intent: Intent) {
            received.unregisterReceiver(this)
            onResult(intent.getBooleanExtra(UsbManager.EXTRA_PERMISSION_GRANTED, false))
        }
    }
    if (Build.VERSION.SDK_INT >= 33) {
        context.registerReceiver(receiver, IntentFilter(action), Context.RECEIVER_NOT_EXPORTED)
    } else {
        @Suppress("UnspecifiedRegisterReceiverFlag")
        context.registerReceiver(receiver, IntentFilter(action))
    }
    // The system adds the result to this intent, so the PendingIntent is mutable and the intent explicit.
    val flags = PendingIntent.FLAG_UPDATE_CURRENT or (if (Build.VERSION.SDK_INT >= 31) PendingIntent.FLAG_MUTABLE else 0)
    manager.requestPermission(device, PendingIntent.getBroadcast(context, 0, Intent(action).setPackage(context.packageName), flags))
}
```

```kotlin
/**
 * An open USB device with its audio interfaces claimed, ready to hand to Rust. Rust gets the file
 * descriptor and the claimed interface numbers; Kotlin keeps the connection open until Rust has closed
 * its `Dac`, and only then calls [close].
 */
class UsbHandoff private constructor(
    private val connection: UsbDeviceConnection,
    private val interfaces: List<UsbInterface>,
    private val claimed: MutableList<UsbInterface>,
) : Closeable {
    /** The usbfs file descriptor. Rust duplicates it, so this one stays ours. */
    val fileDescriptor: Int get() = connection.fileDescriptor

    /** The numbers of the interfaces claimed so far, for `DeviceInfo::with_claimed_interfaces`. */
    val claimedNumbers: IntArray get() = claimed.map { it.id }.distinct().toIntArray()

    /** Rust asks for the optional interfaces a DAC may pace itself with. Returns the numbers it got. */
    fun claimMore(candidates: IntArray): IntArray =
        candidates.filter { number ->
            val iface = interfaces.firstOrNull { it.id == number } ?: return@filter false
            connection.claimInterface(iface, true).also { if (it) claimed += iface }
        }.toIntArray()

    /** Gives the interfaces back, which lets Android's audio driver bind again, and closes the connection. */
    override fun close() {
        claimed.forEach { connection.releaseInterface(it) }
        claimed.clear()
        connection.close()
    }

    companion object {
        /**
         * Opens [device] (which needs USB permission) and claims what a playback DAC needs: every
         * AudioControl interface and every AudioStreaming interface with an isochronous OUT endpoint.
         * Capture-only interfaces stay with Android. Returns null when the device has no such pair.
         * Blocks: call it off the main thread.
         */
        fun open(manager: UsbManager, device: UsbDevice): UsbHandoff? {
            val connection = manager.openDevice(device) ?: return null
            val interfaces = activeInterfaces(device, connection)
            val control = interfaces.filter { it.isAudio(SUBCLASS_CONTROL) }
            val streaming = interfaces.filter { it.isAudio(SUBCLASS_STREAMING) && it.hasIsochronousOut() }
            if (control.isEmpty() || streaming.isEmpty()) {
                connection.close()
                return null
            }
            val claimed = mutableListOf<UsbInterface>()
            for (iface in (control + streaming).distinctBy { it.id }) {
                // force = true detaches Android's USB audio driver from the interface.
                if (!connection.claimInterface(iface, true)) {
                    claimed.forEach { connection.releaseInterface(it) }
                    connection.close()
                    return null
                }
                claimed += iface
            }
            return UsbHandoff(connection, interfaces, claimed)
        }

        private const val SUBCLASS_CONTROL = 1
        private const val SUBCLASS_STREAMING = 2

        private fun UsbInterface.isAudio(subclass: Int) =
            interfaceClass == UsbConstants.USB_CLASS_AUDIO && interfaceSubclass == subclass

        private fun UsbInterface.hasIsochronousOut() = (0 until endpointCount).any {
            getEndpoint(it).let { endpoint ->
                endpoint.direction == UsbConstants.USB_DIR_OUT && endpoint.type == UsbConstants.USB_ENDPOINT_XFER_ISOC
            }
        }

        /** The interfaces of the configuration the device is in: the only one, or the one GET_CONFIGURATION names. */
        private fun activeInterfaces(device: UsbDevice, connection: UsbDeviceConnection): List<UsbInterface> {
            val configurations = (0 until device.configurationCount).map(device::getConfiguration)
            val active = configurations.singleOrNull() ?: run {
                val value = ByteArray(1)
                val read = connection.controlTransfer(UsbConstants.USB_DIR_IN, REQUEST_GET_CONFIGURATION, 0, 0, value, 1, 500)
                configurations.firstOrNull { read == 1 && it.id == (value[0].toInt() and 0xff) } ?: configurations.first()
            }
            return (0 until active.interfaceCount).map(active::getInterface)
        }

        private const val REQUEST_GET_CONFIGURATION = 0x08
    }
}
```

The result has the file descriptor, the interface numbers claimed so far and a `claimMore` call for the optional interfaces of [step 3](#3-open-the-dac). Send the descriptor, the numbers, the device's vendor id, product id, product name and device name across your bridge (JNI, UniFFI, Dart FFI or whatever the app uses) and keep the `UsbHandoff` object alive. Closing it releases the interfaces and the connection, so close it only after Rust has closed its `Dac`.

**Why.** `claimInterface(iface, true)` with `force` detaches Android's own USB audio driver from the interface, which is what lets the library own it. Capture-only interfaces stay with Android. The active configuration matters on a DAC that has several: interface numbers only mean something inside one configuration.

**Do not ask for permission.** From a service, with the screen locked or while another app is in front. Android may refuse the dialog, and a listener who sees it over a locked screen misses it. If the listener picks "always" in the dialog Android shows when the DAC is plugged in, the permission is remembered for that DAC.

**Hot-plug.** The crate has no device list. Register a `BroadcastReceiver` for `UsbManager.ACTION_USB_DEVICE_ATTACHED` and `ACTION_USB_DEVICE_DETACHED` yourself. The library also reports an unplugged DAC as `DacEvent::DeviceLost` ([step 9](#9-events-and-errors)), but the broadcast is the way to know that a DAC arrived.

### On Linux

**Do.** Open the usbfs node of the device and claim its interfaces with the node's ioctls. The process needs read and write access to `/dev/bus/usb/BBB/DDD`, which usually means a udev rule such as:

```text
SUBSYSTEM=="usb", ATTRS{idVendor}=="1234", ATTRS{idProduct}=="abcd", MODE="0660", GROUP="audio"
```

`UsbfsHost` below does this: it reads the descriptors from the node, picks the interfaces a playback DAC needs, claims them (taking them from `snd-usb-audio` if it holds them) and, when dropped, gives them back and asks the kernel to bind its driver again.

```rust
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
```

**Check.** On Android, `UsbHandoff.open` returns an object and `claimedNumbers` has at least two numbers: an AudioControl interface and a streaming interface. A `null` means no permission, no such interface pair or a claim that Android refused. On Linux, `ls -l /dev/bus/usb/001/004` shows your user or group with write access and `UsbfsHost::open` returns `Ok`.

## 3. Open the DAC

**Do.** Define one trait for your side of USB, so that the rest of the code does not care which platform it runs on:

```rust
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
```

On Android, the host is a thin wrapper around what Kotlin handed over:

```rust
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
```

Then open. The function claims the base interfaces, asks the library which optional interfaces the DAC might use, claims those and calls `Dac::open`:

```rust
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
```

**What happens.** `Dac::open` duplicates the descriptor, scans the DAC's descriptors and finds its hardware volume. It records the level the DAC has, lowers every channel to at most -40 dB, and reads the level back to check it. It does not touch the clock and starts no audio. The point of the lowering is that a DAC often comes up at full scale, and your first track would otherwise play at that level.

**What the `DeviceInfo` carries.** `vendor_id`, `product_id` and `product_name` identify the DAC and match its quirk entries. `device_name` keys the remembered original volume. `claimed_interfaces` holds the numbers you claimed, the optional ones included. `configuration_id` and `unavailable_feedback_interfaces` say which configuration the claims belong to and which optional interfaces you could not get. An optional claim that failed costs nothing but that one route: the library falls back to another way of pacing the stream.

**The settings.** The three choices a listener owns, as a `DacConfig`:

```rust
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
```

`require_hardware_volume` is on by default. A DAC without a usable volume then refuses to stream (`Error::VolumeUnavailable`) until the listener has agreed to a fixed level and you open again with it off. Leave `DacConfig`'s buffer, bus queue and clock fields at their defaults unless you are fixing a measured problem; [controls](controls.md) says what each one does to the bit-perfect flag.

**Errors.** What a host does about each failure of the open path:

```rust
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
```

`Error` is `#[non_exhaustive]`, so a `match` needs a last arm.

**Check.** After `open_dac` returns, print `dac.capabilities()?` and `dac.volume().map(|v| v.startup())`. You should see the DAC's alternate settings, and, if the DAC had a volume above -40 dB, `attenuated: true`. `RUST_LOG=debug` (with your logger) shows the library's own account.

## 4. Settle a format for each track

**Do.** Ask for the format the track has, and let the DAC say yes or no.

```rust
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
            && alt.valid_bits >= track.bits
            && alt.sample_rates.contains(&track.sample_rate)
    })
}
```

`offers` answers "does the DAC list this format at all" from the descriptors, without touching its clock, which is what a track list wants before anything plays. `negotiate` programs the DAC's sampling frequency, reads it back and picks the alternate setting that carries the request. It takes 100 ms or more, and it fails with `InvalidState` while a stream is open. So the order for each track is: close the old stream, negotiate, open the new stream. For a gapless run of tracks in one format, do none of this: keep writing.

**Why.** By default the library never changes the rate. With `RatePolicy::Exact`, a rate the DAC cannot run fails with `Error::Unsupported`, and that error lists what the DAC offers. `NearestSupported` (PCM only) picks a rate the DAC has and returns it in the format. It prefers lower rates: the requested rate, then the highest integer divisor of it, then the highest lower rate of the same family, then the highest other lower rate, and only then the lowest higher rate. An 88.2 kHz track on a DAC with 44.1 and 176.4 kHz therefore runs at 44.1 kHz, half the bandwidth; ask for 176.4 kHz with `Exact` if you would rather upsample. A candidate is skipped only when the DAC cleanly refused it ([the request](controls.md#the-request-streamrequest)); any other failure ends the call. Then you have to resample, and the stream is not bit-perfect (`BitPerfectBlocker::FormatChanged`). The crate's opt-in `convert` module has a resampler and bit-depth conversion for that, and it labels each conversion as exact, lossless or lossy ([the conversion helpers](API.md#14-conversion-helpers)). The direct path never calls it by itself.

Ask for a depth the DAC has. `negotiate` needs an alternate setting whose valid bits equal `bit_depth`, so a 16-bit request on a DAC that only has 24-bit alternate settings fails with `Error::Unsupported`. Request 24 bits instead and write the 16-bit samples left-justified (`write_i32` with `sample << 16`): the low bits are zero and every sample is kept. `format.valid_bits` and `format.subslot_bytes` tell you the layout.

**Check.** Negotiate 44.1 kHz and then 96 kHz in a row (closing the stream between them). The DAC shows the rate change. `format.clock` and `format.pacing` say which clock was set and how packets are paced; read them first when a DAC plays at the wrong pitch ([diagnostics](controls.md#diagnostics-the-clock-and-the-pacing)).

## 5. Open the stream and listen

**Do.** Open the stream with a listener. The listener runs on the library's event thread, so it only hands events to a thread of yours:

```rust
/// A listener that hands events to a thread of the host's choice. It runs on the library's event
/// thread, must not block, and must not call `Dac::close`. An unbounded channel's `send` does neither.
pub fn event_channel() -> (Arc<dyn DacListener>, Receiver<DacEvent>) {
    let (tx, rx) = channel();
    let listener: Arc<dyn DacListener> = Arc::new(move |event: DacEvent| {
        let _ = tx.send(event);
    });
    (listener, rx)
}

/// What the host does about each event.
pub enum Reaction {
    Nothing,
    /// Count it; look at the decoder's read-ahead. Playback goes on with silence in the gap.
    Underrun,
    /// A sliver of audio never reached the DAC. The stream is not bit-perfect from here on.
    NotBitPerfect,
    /// Pause, say why, and do not carry on through another output unless the listener asks.
    Pause(&'static str),
    /// The stream failed. Stop the writer, close the stream and decide what to do next.
    Failed(Error),
}

pub fn react(event: DacEvent) -> Reaction {
    match event {
        DacEvent::Underrun => Reaction::Underrun,
        DacEvent::LostFrames { .. } => Reaction::NotBitPerfect,
        DacEvent::FeedbackStale => Reaction::Nothing, // the stream recovers, or fails with its own event
        DacEvent::DeviceLost => Reaction::Pause("the DAC was unplugged"),
        DacEvent::Error(error) => Reaction::Failed(error),
        // The state, the bit-perfect flag and the volume are read when the display needs them.
        _ => Reaction::Nothing,
    }
}
```

```rust
let (listener, events) = event_channel();
let stream = dac.open_stream(format, listener)?;
```

Here `format` is the `&StreamFormat` that `negotiate` returned.

**Why.** The library calls the listener from one thread, `usbdac-events`, in order. If your listener blocks, every later event waits, and the library drops the newest events when its queue of 256 fills. The listener must not call `Dac::close`. A panic in it is caught and logged. An unbounded channel's `send` never blocks, which is why the code above uses one.

If the listener calls into Kotlin or Java, attach the thread to the JVM once, in your own code.

`open_stream` switches the alternate setting, checks the clock again and starts the output thread, which takes 100 ms or more. Only one stream can be open at a time. Opening it with the volume protection above in force can fail with `VolumeUnavailable`.

**Check.** `stream.state()` is `Buffering` until the ring holds the start threshold, then `Playing`. Events arrive on your channel.

## 6. Write audio from a thread of your own

**What the writer does.** Your decoder produces samples. A writer thread asks the stream how much it wants, renders that much and writes it. The code below is generic over a small `Ring` trait that `DacStream` implements, so that the same writer can be tested without a DAC ([step 13](#13-test-it)).

First, how the decoder looks to the writer, with a tone source to try it:

```rust
/// The host's decoder, as the writer thread sees it.
pub trait Source: Send {
    /// Fills `out` with whole frames of PCM as full-scale 32-bit samples, left-justified: a 16-bit sample
    /// is `sample << 16`, a 24-bit one `sample << 8`. Returns how many samples it wrote, a multiple of the
    /// channel count, or 0 at the end of the track. The stream keeps the top `valid_bits` bits.
    fn render(&mut self, out: &mut [i32]) -> usize;

    /// Moves the decoder to `seconds`.
    fn seek(&mut self, seconds: f64);
}

/// A 440 Hz tone at 24-bit precision, for trying the code without a decoder.
pub struct Tone {
    rate: f64,
    channels: usize,
    phase: f64,
}

impl Tone {
    pub fn new(rate: u32, channels: u16) -> Self {
        Self {
            rate: f64::from(rate),
            channels: usize::from(channels),
            phase: 0.0,
        }
    }
}

impl Source for Tone {
    fn render(&mut self, out: &mut [i32]) -> usize {
        for frame in out.chunks_exact_mut(self.channels) {
            let sample = ((self.phase * std::f64::consts::TAU).sin() * 0.25 * 8_388_607.0) as i32;
            frame.fill(sample << 8); // 24 bits, left-justified in 32
            self.phase = (self.phase + 440.0 / self.rate).fract();
        }
        out.len()
    }

    fn seek(&mut self, _seconds: f64) {
        self.phase = 0.0;
    }
}
```

Then the ring and the shape of the wire format:

```rust
/// The part of `DacStream` that the writer and the transport use. A host that wants to test its writer
/// on a laptop writes a second implementation that records what it is given: `DacStream` cannot be
/// built without a DAC.
pub trait Ring: Send + Sync {
    fn writable_bytes(&self) -> usize;
    fn write_i32(&self, samples: &[i32]) -> Result<usize>;
    fn play(&self) -> Result<()>;
    fn pause(&self) -> Result<()>;
    fn flush(&self) -> Result<()>;
    fn drain(&self) -> Result<()>;
    fn is_drained(&self) -> bool;
    fn played_frames(&self) -> u64;
}

impl Ring for DacStream {
    fn writable_bytes(&self) -> usize {
        DacStream::writable_bytes(self)
    }
    fn write_i32(&self, samples: &[i32]) -> Result<usize> {
        DacStream::write_i32(self, samples)
    }
    fn play(&self) -> Result<()> {
        DacStream::play(self)
    }
    fn pause(&self) -> Result<()> {
        DacStream::pause(self)
    }
    fn flush(&self) -> Result<()> {
        DacStream::flush(self)
    }
    fn drain(&self) -> Result<()> {
        DacStream::drain(self)
    }
    fn is_drained(&self) -> bool {
        DacStream::is_drained(self)
    }
    fn played_frames(&self) -> u64 {
        DacStream::played_frames(self)
    }
}

/// What the writer needs to know about the wire format.
#[derive(Clone, Copy, Debug)]
pub struct Shape {
    pub channels: usize,
    /// Bytes of one sample on the wire (`subslot_bytes`): the unit `writable_bytes` counts in.
    pub bytes_per_sample: usize,
    pub sample_rate: u32,
    /// The rate of `played_frames`.
    pub counter_rate_hz: u32,
}

impl From<&StreamFormat> for Shape {
    fn from(format: &StreamFormat) -> Self {
        Self {
            channels: usize::from(format.channels),
            bytes_per_sample: usize::from(format.subslot_bytes),
            sample_rate: format.sample_rate,
            counter_rate_hz: format.counter_rate_hz(),
        }
    }
}
```

Then the player, with its writer loop:

```rust
/// What the writer thread and the control calls share, under one lock so that a seek can never
/// interleave with a half-written chunk.
struct Shared {
    source: Box<dyn Source>,
    /// A rendered chunk that the ring has not fully taken yet.
    chunk: Vec<i32>,
    taken: usize,
    ended: bool,
    /// The source time of the first frame written since the last flush.
    start_seconds: f64,
}

fn lock(shared: &Mutex<Shared>) -> MutexGuard<'_, Shared> {
    shared
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

pub struct Player<R: Ring + 'static> {
    ring: Arc<R>,
    shape: Shape,
    shared: Arc<Mutex<Shared>>,
    stop: Arc<AtomicBool>,
    writer: Option<JoinHandle<()>>,
}

impl<R: Ring + 'static> Player<R> {
    /// Starts the writer thread. The stream starts by itself once the ring holds `start_threshold_ms`
    /// of audio (100 ms by default); `play` is only for after a pause.
    pub fn start(ring: Arc<R>, shape: Shape, source: Box<dyn Source>) -> Self {
        let shared = Arc::new(Mutex::new(Shared {
            source,
            chunk: Vec::new(),
            taken: 0,
            ended: false,
            start_seconds: 0.0,
        }));
        let stop = Arc::new(AtomicBool::new(false));
        let writer = {
            let (ring, shared, stop) = (Arc::clone(&ring), Arc::clone(&shared), Arc::clone(&stop));
            thread::Builder::new()
                .name("host-writer".into())
                .spawn(move || write_loop(&*ring, shape, &shared, &stop))
                .expect("spawn the writer thread")
        };
        Self {
            ring,
            shape,
            shared,
            stop,
            writer: Some(writer),
        }
    }

    /// Keep the writer running while paused: it fills the ring to its target and idles, so a resume
    /// loses nothing.
    pub fn pause(&self) -> Result<()> {
        self.ring.pause()
    }

    pub fn resume(&self) -> Result<()> {
        self.ring.play()
    }

    /// Reposition the decoder, drop what was rendered for the old position, then flush. The old audio
    /// stops within a transfer or two, and `played_frames` starts again from zero.
    pub fn seek(&self, seconds: f64) -> Result<()> {
        let mut state = lock(&self.shared);
        state.source.seek(seconds);
        state.chunk.clear();
        state.taken = 0;
        state.ended = false;
        state.start_seconds = seconds;
        self.ring.flush()
    }

    /// The audible position: where the last flush started plus what the DAC has completed since.
    pub fn position_seconds(&self) -> f64 {
        let start = lock(&self.shared).start_seconds;
        start + self.ring.played_frames() as f64 / f64::from(self.shape.counter_rate_hz)
    }

    /// The decoder reached the end and the DAC has played everything.
    pub fn finished(&self) -> bool {
        lock(&self.shared).ended && self.ring.is_drained()
    }

    pub fn ring(&self) -> &R {
        &self.ring
    }

    /// Stops the writer and returns the ring, so that the caller can close it.
    pub fn stop(mut self) -> Arc<R> {
        self.stop.store(true, Ordering::Release);
        if let Some(writer) = self.writer.take() {
            let _ = writer.join();
        }
        Arc::clone(&self.ring)
    }
}

impl<R: Ring + 'static> Drop for Player<R> {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(writer) = self.writer.take() {
            let _ = writer.join();
        }
    }
}

fn write_loop(ring: &dyn Ring, shape: Shape, shared: &Mutex<Shared>, stop: &AtomicBool) {
    // About 10 ms of audio per render call, in whole frames.
    let mut scratch = vec![0i32; (shape.sample_rate as usize / 100).max(1) * shape.channels];
    while !stop.load(Ordering::Acquire) {
        let mut state = lock(shared);
        if state.ended {
            drop(state);
            thread::sleep(Duration::from_millis(10));
            continue;
        }
        if state.taken == state.chunk.len() {
            // Render only when the ring wants this much, so the decoder is never ahead of the ring's
            // target depth and a seek has little to throw away.
            if ring.writable_bytes() < scratch.len() * shape.bytes_per_sample {
                drop(state);
                thread::sleep(Duration::from_millis(2));
                continue;
            }
            let rendered = state.source.render(&mut scratch);
            if rendered == 0 {
                // The end of the track: let the ring play out, even a tail shorter than the start threshold.
                if ring.drain().is_ok() {
                    state.ended = true;
                }
                continue;
            }
            state.chunk.clear();
            state.chunk.extend_from_slice(&scratch[..rendered]);
            state.taken = 0;
        }
        // Never blocks. A count short of what was offered, or zero, means the ring is full.
        match ring.write_i32(&state.chunk[state.taken..]) {
            Ok(0) => {
                drop(state);
                thread::sleep(Duration::from_millis(2));
            }
            Ok(taken) => state.taken += taken,
            Err(error) => {
                // The stream closed or failed. The event listener has the reason.
                log::warn!("the writer stops: {error}");
                return;
            }
        }
    }
}
```

**The layout.** `write_i32` takes PCM as full-scale 32-bit samples, left-justified. A 16-bit sample is `sample << 16`, a 24-bit one `sample << 8`. The library keeps the top `valid_bits` bits and packs them into whatever the DAC wants: 2, 3 or 4 bytes per sample. That is why the code never looks at `subslot_bytes` except to size a chunk. A source with fewer bits than the DAC's alternate setting is widened with zero bits, and nothing is dithered. If you already hold wire-layout bytes, `write` takes them instead; the layouts are in [the native API document](API.md#4-sample-layouts). DoP and native DSD have their own layouts there.

**Buffer sizing.**

- **Chunk.** About 10 ms of audio per render call (`sample_rate / 100` frames), in whole frames, in one reused buffer. Smaller chunks cost more wake-ups. Larger ones make a seek throw away more.
- **The ring.** 250 ms deep by default (`buffer_target_ms`), 500 ms at most (`buffer_capacity_ms`). `writable_bytes()` is the room up to the target, a whole number of frames. `write` accepts more, up to the capacity, so read the target as the depth to aim for.
- **The start.** A new stream plays by itself once the ring holds `start_threshold_ms` (100 ms by default). A writer that writes less than that and stops leaves the stream in `Buffering` and silent. For a clip shorter than the threshold, call `drain()`.
- **Underruns.** If your decoder cannot keep the ring above empty, the library pads the wire with silence and sends `DacEvent::Underrun` once per run. An underrun does not clear the bit-perfect flag, so count them yourself. Fix it with more read-ahead in the decoder before you raise the ring.

**Blocking.** `write`, `write_i32`, `writable_bytes`, `play`, `pause`, `flush`, `drain` and `is_drained` never block. They take short internal locks. `write` returns the number it took, which may be less than you offered or zero when the ring is full: keep the rest and try again. The loop sleeps 2 ms in that case, and renders only when `writable_bytes()` says there is room, so the decoder is never far ahead of the DAC. After `drain()` and until the next `flush()`, `write` returns `InvalidState`, and so does a write after the stream failed or closed.

**One writer.** `write` is thread-safe, but the bytes you write are one ordered sequence. Use one writer thread, and put the seek under the lock that the writer holds while it renders and writes.

**Check.** Start the player with `Tone` and listen: a steady 440 Hz tone with no clicks. Watch `stream.stats()`: `underruns` stays at 0, `frames_written` rises, and `buffer_ms` sits near 250. Then slow the source down with a `sleep` and see `underruns` rise and `DacEvent::Underrun` arrive.

## 7. Pause, seek, skip and stop

**Do.** The `Player` of step 6 maps the transport:

| Host action | Call |
|---|---|
| Pause | `stream.pause()`. Keep the writer running: it fills the ring to the target and then idles on `writable_bytes() == 0` |
| Resume | `stream.play()` |
| Seek | Under the writer's lock: reposition the decoder, drop the rendered chunk, `stream.flush()`, remember the new start time |
| Stop, or a track the listener picked | `flush()`, then write the new audio, or none |
| Next track in a gapless run, same format | Keep writing. No `drain()`, no `flush()` |
| Next track, another format | `drain()` (or `flush()` after a skip), close the stream, negotiate, open a new one |
| End of the last track | `drain()`, then wait for `is_drained()` |
| Position | `start_seconds + played_frames() / counter_rate_hz()`, with `start_seconds` set at each flush |

`pause()` schedules wire-correct silence and holds the ring. Already submitted audio may still play. `flush()` drops the ring, resets `played_frames()` and, by default, requests cancellation of submitted transfers; cancellation is asynchronous and acoustic timing is not established. The cancelled audio is not lost audio and the flag stays bit-perfect. `drain()` declares the end of the audio: the ring plays out, even a tail shorter than the start threshold.

**Why the seek order matters.** The writer may hold a chunk rendered for the old position. If the seek flushed the ring and the writer then wrote that chunk, the old audio would play after the flush. So the seek takes the writer's lock, repositions the decoder, drops the chunk and only then flushes. The test below checks exactly this:

```rust
#[test]
fn a_seek_drops_the_chunk_that_was_rendered_for_the_old_position() {
    // The ring takes everything, so the chunk in hand is whatever the ring has not accepted yet.
    let ring = FakeRing::new(1_000_000);
    let player = Player::start(
        ring.clone(),
        SHAPE,
        Box::new(Counter {
            next: 0,
            frames: 10_000_000,
        }),
    );
    wait_until("some audio", || ring.written.lock().unwrap().len() > 200);

    player.seek(2.0).unwrap();
    assert!(ring.calls().contains(&"flush"));
    let after = ring.written.lock().unwrap().clone();
    // Everything written after the flush comes from the new position.
    wait_until("audio at the new position", || {
        ring.written.lock().unwrap().len() > after.len() + 100
    });
    let written = ring.written.lock().unwrap().clone();
    assert!(
        written.iter().all(|sample| *sample >= 102_000),
        "old audio was written after the flush: {:?}",
        &written[..8]
    );
}
```

**Position.** `played_frames()` counts music the DAC has completed on the bus since the last flush: not held silence, not padding, not lost frames. It is cheap to read, so poll it for a progress bar. The rate of the count is `counter_rate_hz()`: the sample rate for PCM, and the DSD bit rate divided by 8 for native DSD.

**Check.** Exercise seek while playing, pause/resume and seek while paused on the target device. Observe submitted and downstream audio as well as the reported counters; host tests do not establish acoustic timing.

## 8. Volume

**Do.** Use the DAC's hardware volume. `dac.volume()` returns a `VolumeControl` when the DAC has a writable volume, or `None`. It works with or without a stream, as long as the `Dac` is open.

```rust
/// When a DAC connects: the library has already lowered it to at most -40 dB. Restore the level the
/// listener had for this DAC, but never above what the DAC itself had.
pub fn restore_level(volume: &VolumeControl, remembered_db: Option<f64>) -> Result<f64> {
    let ceiling = volume.startup().limit_db;
    match remembered_db {
        Some(db) => volume.set_db(db.min(ceiling)).map(|state| state.db),
        None => volume.get().map(|state| state.db),
    }
}

/// A volume key or a slider. Each call is a USB control transfer (milliseconds, up to a second):
/// call it from a control thread and not from the writer or a UI thread.
pub fn nudge(volume: &VolumeControl, up: bool) -> Result<f64> {
    volume
        .step(if up { 1 } else { -1 }, 0)
        .map(|state| state.db)
}
```

`set_db` clamps to the DAC's range and snaps to the nearest step it has. `set_fraction` uses a 0 to 1 curve whose 0 is the DAC's minimum and whose remaining travel spans the top 80 dB (or the DAC's range, if smaller), so a DAC with a -127 dB minimum does not waste most of a slider. `step(direction, steps)` moves by `1/steps` of that curve, and at least one DAC step. `steps` of 0 means 16.

**Rules.**

- **Every call blocks.** Each is a USB control transfer, with a timeout of a second. The Kotlin SDK queues volume writes; this crate does not. Call volume from a control thread, never from the writer or a UI thread. A slider that fires on every move needs your own coalescing: keep the latest value and write it when the previous write returns.
- **Remember levels in dB, per DAC**, not as slider positions, and cap what you restore at `startup().limit_db`, as `restore_level` does. That is how loud the DAC itself was when it connected.
- **A knob on the DAC** changes its level without telling the library. `get()` reads the live level and sends `VolumeChanged` if it differs from the last one it saw.
- **Software gain** (`stream.set_software_gain`) scales the samples and ends bit-perfect output. Do not use it for a volume control. It exists for hosts that accept that.
- **ReplayGain, an equaliser, crossfade** change samples before `write`. The library cannot see them. Call `stream.set_host_processing(true)` while they run, so the ring is shallower (100 ms) and your changes are heard sooner, and do not claim bit-perfect output while they are on.

**Check.** Raise and lower the level in steps while a tone plays: the level follows each call and `VolumeChanged` arrives. Unplug and replug the DAC: it starts at or below -40 dB again.

## 9. Events and errors

**Do.** Read the channel from a control thread and decide for each event:

```rust
/// How a host that plays one track fits the pieces together. `format` comes from `negotiate`.
pub fn play_one(dac: Dac, format: &StreamFormat, source: Box<dyn Source>) -> Result<()> {
    let (listener, events) = event_channel();
    let stream = dac.open_stream(format, listener)?;
    let shape = Shape::from(stream.format());
    let player = Player::start(Arc::new(stream), shape, source);
    let mut device_lost = false;
    while !player.finished() {
        match events.recv_timeout(Duration::from_millis(100)).map(react) {
            Ok(Reaction::Pause(reason)) => {
                log::warn!("{reason}");
                device_lost = true;
                break;
            }
            Ok(Reaction::Failed(error)) => {
                device_lost = error == Error::DeviceLost;
                log::warn!("the stream failed: {error}");
                break;
            }
            _ => {}
        }
    }
    log::info!("{}", status_line(player.ring(), true, false));
    shutdown(player, dac, device_lost)
}
```

`react` is the table of what each event means for a host:

| Event | What it means | What the host does |
|---|---|---|
| `StateChanged` | The stream state changed | Update the display |
| `Underrun` | The ring ran dry and the wire was padded | Count it. Look at the decoder's read-ahead |
| `LostFrames { frames }` | Queued audio never reached the DAC. It cannot be re-sent | The flag is false for the rest of the stream. Show it. Log once |
| `BitPerfectChanged` | `is_bit_perfect()` changed | Update the indicator |
| `VolumeChanged` | The level or mute changed through this library | Update the slider |
| `FeedbackStale` | The DAC's timing source stopped giving usable reports | Log `format.pacing` and `stats()`. The stream holds its last good rate for a short grace period and fails if reports do not return |
| `Error` | The stream failed. `StateChanged(Failed)` follows | Stop the writer, close the stream, decide what to do next |
| `DeviceLost` | The DAC is gone | Pause and say so. Do not move to another output unless the listener asked |

A stream that failed stays open in the `Failed` state, and writes return `InvalidState`, until you close it. `open_stream` refuses to start another before that. A per-packet transfer error ends the stream with `Error::Transfer`; the message carries the packet diagnostics, so keep it for a report.

**Check.** Pull the cable while a tone plays. `DeviceLost` arrives, `play_one` leaves its loop, and the shutdown of step 11 returns.

## 10. Report whether the output is bit-perfect

**Do.** Show one line, from the stream's own verdict and from what your engine did:

```rust
/// One line for a status display. The library's flag covers the transport; the host adds what it did
/// to the samples and what the source was.
pub fn status_line(stream: &DacStream, engine_untouched: bool, lossy_source: bool) -> String {
    if !engine_untouched {
        return "Not bit-perfect: the player is processing the audio".to_string();
    }
    match stream.bit_perfect_blocker() {
        None if lossy_source => "Direct USB, decoded from a lossy file".to_string(),
        None => "Bit-perfect".to_string(),
        Some(blocker) => format!("Not bit-perfect: {}", blocker_text(blocker)),
    }
}

fn blocker_text(blocker: BitPerfectBlocker) -> &'static str {
    match blocker {
        BitPerfectBlocker::LostFrames => "audio was lost on the USB bus",
        BitPerfectBlocker::FormatChanged => "the DAC runs another rate than the track has",
        BitPerfectBlocker::ClockUnverified => "the DAC did not confirm its sampling rate",
        BitPerfectBlocker::SoftwareGain => "software gain is on",
        BitPerfectBlocker::UnsynchronizedAsync => {
            "the DAC has no feedback, so it plays at the nominal rate"
        }
        BitPerfectBlocker::FeedbackUnlocked => "the DAC's timing has not locked yet",
        BitPerfectBlocker::PayloadUnverified => "waiting for real audio",
        BitPerfectBlocker::NoStream => "no stream is running",
        // `BitPerfectBlocker` is non-exhaustive.
        _ => "the stream is not verified",
    }
}
```

**Why.** `is_bit_perfect()` is true while the stream is open and running, no frame was lost, the format on the wire is the requested one, live feedback has locked (when the DAC uses it), the DAC confirmed its clock, software gain is exactly 1.0, the pacing is valid and the wire payload passed an audit. `bit_perfect_blocker()` returns the first condition that fails, or `None`. The claim covers the transport. It does not hash the source, and it cannot see what your decoder or an equaliser did. So the line combines three things: the library's flag, whether your engine touched the samples, and whether the source was lossy.

`PayloadUnverified` shows for the first moments of a stream, and after a long stretch of digital silence, because the audit needs real audio. Do not alarm the listener over it. An underrun does not clear the flag.

`is_bit_perfect()` and `stats()` take a short lock and touch session state. Poll them a few times a second from a control thread, not on every write.

**Check.** Play a lossless file: the line reads "Bit-perfect" after a moment of real audio. Turn on `set_software_gain(0.5)`: it reads "Not bit-perfect: software gain is on". Play with `NearestSupported` on a rate the DAC lacks: "the DAC runs another rate than the track has".

## 11. Close cleanly and hand the DAC back

**Do.**

```rust
/// Stops the writer, closes the stream, then the DAC. The host releases its interfaces and its
/// descriptor after this returns, never before.
///
/// `device_lost` is true when the DAC was unplugged: nothing can be written to it, and its recorded
/// level must not outlive the attachment.
pub fn shutdown(player: Player<DacStream>, dac: Dac, device_lost: bool) -> Result<()> {
    let stream = player.stop(); // joins the writer thread
    let stream_result = match Arc::try_unwrap(stream) {
        Ok(stream) => stream.close(),
        Err(_) => Err(Error::InvalidState(
            "the stream is still shared".to_string(),
        )),
    };
    let dac_result = if device_lost {
        dac.close_with(CloseVolume::Forget)
    } else {
        dac.close()
    };
    if matches!(dac_result, Err(Error::Transfer(_))) {
        // The output thread did not stop in time. Keep every claim and the descriptor until it has.
        while !Dac::await_usb_retirement(Duration::from_secs(1)) {}
    }
    stream_result.and(dac_result)
}
```

**The order.** Stop the writer, close the stream, close the `Dac`, and only then release the interfaces and the descriptor (Kotlin's `UsbHandoff.close`, or dropping `UsbfsHost`). Dropping a stream or a `Dac` starts the same cleanup, but `drop` cannot report a timeout, so close explicitly.

**What close does.** `Dac::close` stops the stream if one is open, writes the volume back as `DacConfig::hand_back` says and joins the library's threads. The default, `HandBack::RestoreOriginal`, puts the DAC back at the level it had before the library lowered it, which is right when the system's audio driver gets the DAC next. `close_with(CloseVolume::Keep)` leaves the level and keeps the record for your next `open` of the same DAC in this process, for an app that takes the DAC straight back. `close_with(CloseVolume::Forget)` ends the record without touching the DAC, and is what you want after an unplug. A DAC the library saw disappear is forgotten whatever you ask for.

**If close times out.** `Dac::close` returns `Error::Transfer` when the output thread did not stop in time. Then keep every claim and the descriptor, and call `Dac::await_usb_retirement` until it returns `true`. Releasing the interfaces earlier lets the system driver bind while transfers are still in flight.

**Do not** call `close` from inside the listener.

**Check.** After close returns, another `Dac::open` succeeds. On Android, the DAC is an Android audio output again after `UsbHandoff.close`. On Linux, `cat /proc/asound/cards` should list the DAC again once `UsbfsHost` is dropped. If it does not, the kernel driver was not bound again, and the reconnect step in `UsbfsHost::drop` needs your attention: that path has not been run on a Linux desktop.

## 12. Threading and real-time rules

| Call | Blocks? | Where to call it |
|---|---|---|
| `Dac::open`, `negotiate`, `open_stream` | Yes: descriptor and clock reads, 100 ms or more | A thread of your own, never the UI thread |
| `stream.close`, `Dac::close` | Yes: the last transfers are cancelled and handed back, typically a few milliseconds | A control thread |
| Every `VolumeControl` call | Yes: a USB control transfer, up to a second | A control thread. Never the writer |
| `stats()`, `is_bit_perfect()` | A short lock that touches session state | A control thread, a few times a second |
| `write`, `write_i32`, `writable_bytes`, `play`, `pause`, `flush`, `drain`, `is_drained`, `state`, `played_frames`, `latency_frames` | No: short internal locks | The writer and control threads |

The library starts two threads: `usbdac-output`, the isochronous loop (with `realtime_priority` on, it asks for `SCHED_FIFO` priority 2 on Android and falls back to a nice value), and `usbdac-events`. It never calls into your code from the output thread.

Rules for your own threads:

- **One writer.** It owns the sequence of audio bytes.
- **Do not hold your own lock while you wait on the library, or the library's while you wait on yours.** The writer in step 6 holds the seek lock across `write_i32`, which is safe because `write_i32` never blocks.
- **Keep the writer free of allocation and I/O.** It reuses one scratch buffer and one chunk buffer. Decode ahead on another thread if your decoder allocates.
- **Priority for the writer.** Android's own player classes give the audio thread a higher priority. Whether that helps usbdac has not been measured, and the output thread has its own real-time scheduling.
- **If you have a real-time callback of your own** (a `cpal` or `Oboe` callback, say), you can call `write` from it, but prefer a dedicated writer thread and a lock-free queue from the callback side. The ring lock is held for the length of a copy.
- **Sleep while the ring is full.** A busy loop on `writable_bytes()` burns a core. The 2 ms sleep of step 6 costs nothing in latency, because the ring holds 250 ms.

## 13. Test it

**Without a DAC.** `Dac` and `DacStream` cannot be built without a device, so the host's logic gets its own seam: the `Ring` trait of step 6. A fake that records what the writer does tests the rules you depend on:

```rust
/// A ring with a fixed amount of room that records what it is given.
struct FakeRing {
    room_bytes: AtomicUsize,
    written: Mutex<Vec<i32>>,
    calls: Mutex<Vec<&'static str>>,
    drained: AtomicBool,
    played: AtomicU64,
}

impl FakeRing {
    fn new(room_bytes: usize) -> Arc<Self> {
        Arc::new(Self {
            room_bytes: AtomicUsize::new(room_bytes),
            written: Mutex::new(Vec::new()),
            calls: Mutex::new(Vec::new()),
            drained: AtomicBool::new(false),
            played: AtomicU64::new(0),
        })
    }
    fn call(&self, name: &'static str) -> Result<()> {
        self.calls.lock().unwrap().push(name);
        Ok(())
    }
    fn calls(&self) -> Vec<&'static str> {
        self.calls.lock().unwrap().clone()
    }
}

impl Ring for FakeRing {
    fn writable_bytes(&self) -> usize {
        self.room_bytes.load(Ordering::SeqCst)
    }
    fn write_i32(&self, samples: &[i32]) -> Result<usize> {
        self.written.lock().unwrap().extend_from_slice(samples);
        Ok(samples.len())
    }
    fn play(&self) -> Result<()> {
        self.call("play")
    }
    fn pause(&self) -> Result<()> {
        self.call("pause")
    }
    fn flush(&self) -> Result<()> {
        self.written.lock().unwrap().clear();
        self.played.store(0, Ordering::SeqCst);
        self.call("flush")
    }
    fn drain(&self) -> Result<()> {
        self.call("drain")
    }
    fn is_drained(&self) -> bool {
        self.drained.load(Ordering::SeqCst)
    }
    fn played_frames(&self) -> u64 {
        self.played.load(Ordering::SeqCst)
    }
}
```

`crates/usbdac-core/tests/guide_player.rs` runs five tests against it, under `cargo test --workspace`. They check that the writer renders only when the ring has room, that it drains once at the end, that a seek drops the stale chunk, that the position is the last seek plus what was played, and that pause and resume leave the writer running. The transport's own tests use an internal fake DAC that is not public; the public `fake::FakeDac` ([below](#fake-dac-at-the-application-boundary)) substitutes for the application-level playback calls only.

**With a DAC on Linux or Android.** Adapt this to your devices and write down the phone or computer, the DAC, the cable and any hub:

| Check | What to look for |
|---|---|
| Cold start: plug in, open, play a tone | Plays; `is_bit_perfect()` becomes true after real audio; the level is at or below -40 dB |
| 44.1, 48, 96 kHz and the DAC's highest rate; 16 and 24 bit | No `ClockUnverified`; the DAC shows each rate |
| Gapless in one format | No gap, no click, `played_frames` continuous |
| A rate change between tracks | A gap of 100 ms or more; the right rate on the DAC |
| Pause, resume, seek while playing and while paused | Observe acoustic response and old submitted audio; compare the counters |
| Slow the decoder | `Underrun` events and `underruns` rise, then recover |
| Unplug during playback | `DeviceLost`; the writer stops; close returns; replug and open again works |
| A DAC with no volume control | `VolumeUnavailable` until the fixed-level setting is on |
| Close, then play from another app | The DAC is back at its original level and the system plays through it |
| A hub or a long cable | No `LostFrames`, no timeouts in `stats()` |

On a Linux machine without a DAC, QEMU can present a virtual USB audio device. The project used it to play synchronous UAC1 PCM through usbfs ([evidence](evidence.md)); that check does not cover feedback, DSD or asynchronous endpoints.

Send what you find, with the log, to [the issue tracker](https://github.com/johnneerdael/android-usbdac/issues).

## What the crate does not do

Know these before you ship.

- **It does not enumerate devices, ask for permission or claim interfaces.** Every host writes the claim planning itself: the AudioControl interfaces and the AudioStreaming interfaces that have an isochronous OUT endpoint, in the active configuration. The Kotlin library has this logic and does not export it.
- **It does not watch for DACs by itself.** On Android the host feeds its broadcasts into `HotplugHub`; on Linux `LinuxHotplugMonitor` is opt-in ([discovery](#discovery-and-permission-events)).
- **Its public fake covers playback calls only.** `fake::FakeDac` has no clock, feedback, capture, routing or hardware volume; the tests above work through a seam of your own.
- **One owner per attachment.** A second owner for the same attachment returns `Busy`; separate attachments can be owned independently.
- **It does not decode, resample or mix.** Resampling and bit-depth conversion are in the opt-in `convert` module, and the direct path never calls them.
- **Linux is checked less than Android.** The Linux code of this guide compiles for Android's kernel target and has not been run on a Linux desktop. A DAC with several configurations needs the active one before it is claimed: `UsbfsHost` reads only the first configuration.
- **DSD.** The crate carries DoP and native DSD, with layouts and quirks of their own. This guide plays PCM. See [bit-perfect output](bit-perfect.md#dsd) and [the native API document](API.md#4-sample-layouts).

## Common mistakes

- **Handing the library a descriptor that did not make the claims.** Claims belong to the file they were made on.
- **Releasing the interfaces before `Dac::close` returned.** Wait for it, and for `await_usb_retirement` after a timeout.
- **Calling a volume function from the writer thread.** It is a USB control transfer and can take up to a second.
- **Writing from two threads.** `write` is thread-safe, but the audio is one ordered sequence.
- **Closing the `Dac` from the event listener.** The library forbids it.
- **Flushing or draining between gapless tracks.** `drain()` ends the stream until the next `flush()`.
- **Showing "bit-perfect" while your engine processes the audio.** The flag is about the transport.
- **Opening a second `Dac` for the same attachment**, or opening the same DAC from Rust and through the Kotlin library in one process.
- **Running `negotiate` or `open_stream` on a UI thread.** Each takes 100 ms or more.
- **Ignoring `VolumeUnavailable`.** A DAC with no volume control plays at its own level, possibly full scale. Ask the listener first.

## Where to go next

- The write loop, seek and position in general: [the player engine](integration-player.md).
- Permission, lifecycle and settings for any app: [app, service and settings](integration-app.md).
- Every option and its effect on bit-perfect output: [controls and configuration](controls.md).
- Wire layouts, events, threading rules and the JNI contract: [the native API document](API.md).
- A Kotlin app on Media3: [the Kotlin guide](guide-kotlin.md).

## Discovery and permission events

`HotplugHub` is the Rust host boundary for attachment and permission events.
Subscribe before taking action. Each subscription starts with an atomic
`HotplugEvent::Snapshot`; subsequent events describe changes. If a slow consumer
overwhelms its bounded queue, the next receive replaces old hints with a fresh
snapshot. Replace application discovery state on **every** snapshot.

```rust
use usbdac_core::{DeviceInfo, HotplugHub, UsbPermission};
let hub = HotplugHub::new();
let events = hub.subscribe();
let attachment = hub.attached(
    "android-device-name",
    DeviceInfo::new(1, 2, "DAC"),
    UsbPermission::Required,
);
// Pass attachment.id through the host permission request/callback.
assert!(hub.permission_changed(attachment.id, UsbPermission::Granted));
hub.detached(attachment.id);
assert!(!hub.permission_changed(attachment.id, UsbPermission::Granted));
drop(events);
```

On Android register the `UsbManager` attach/detach and application permission
receiver, then seed the hub from `deviceList`. Feed broadcasts and permission
results into the same hub. Query `hasPermission` for each initial/attached
device; report denial as `UsbPermission::Denied`. Use the attachment ID captured
when requesting permission: delayed callbacks for a detached device must not
grant a replacement with the same OS path. Requests, microphone permission for
capture, configuration selection, claims, and the connection FD remain the
Android host's responsibility, as in Kotlin `UsbDac.hotplug` and
`UsbDac.requestPermission`. `Granted` records a host result and never bypasses
`Dac::open` validation. Discovery does not reroute audio or open a DAC.

On Linux `LinuxHotplugMonitor::start()` enumerates audio-class/composite devices
and registers native libusb hotplug callbacks. Retain the monitor and subscribe
through `monitor.hub()`. When libusb lacks hotplug capability it polls cached
descriptors every 250 ms; a complete removal/reinsert between fallback polls can
be missed. Permission is `Unknown` because enumeration proves no access; the
host opens/claims the selected device with its usual OS permissions. Linux
names are descriptor identities (`USB audio VID:PID`), not product strings, so
populate the actual product/manufacturer fields before `Dac::open` if required
for quirk matching. Other platforms return `PlatformUnsupported`; Android uses
the explicit host bridge, not the Linux monitor. On macOS,
`darwin_discovery::snapshot` lists attachments read-only, without opening or
claiming them, and is polled rather than called back.

## Volume without blocking the UI

The existing `VolumeControl` methods remain synchronous. Create one
`volume.nonblocking()?` controller per open device to dispatch verified control
I/O to its dedicated worker. No async runtime dependency is required.

```rust
use usbdac_core::{Result, VolumeControl};
async fn change(volume: VolumeControl) -> Result<()> {
let controls = volume.nonblocking()?;
let applied = controls.set_db(-45.0)?.await?;
assert!(applied.db <= 0.0);
Ok(())
}
```

Submission is nonblocking and returns `Busy` when 32 requests are queued. Retry
or coalesce slider input in the host. Accepted operations run in FIFO order and
use the same hardware readback, multi-channel rollback, and events as blocking
calls. Each result handle supports `.await`, `try_result()`, or blocking `wait()`;
retrieve it once. Dropping a result does **not** cancel an accepted write, so do
not use result cancellation to undo a volume change. Close serializes with
control I/O, and queued requests on a closed device fail without touching a new
session. Startup attenuation and hand-back policy are unchanged.

## Fake DAC at the application boundary

Use `PlaybackDevice` and `PlaybackStream` for the common push-playback subset
implemented by the real `Dac`/`DacStream` and `fake::FakeDac`/`fake::FakeStream`.
The fake is public, has no USB dependency at runtime, and does not occupy the
real process session. It is a deterministic application substitute, not USB
protocol or hardware evidence.

```rust
use std::sync::Arc;
use usbdac_core::{fake::FakeDac, DeviceInfo, PlaybackDevice, PlaybackStream, StreamRequest};
let request = StreamRequest::pcm(48_000, 2, 16);
let dac = FakeDac::new(DeviceInfo::new(1, 2, "test"), vec![request], 128).unwrap();
let format = dac.negotiate(&request).unwrap();
let stream = dac.open_stream(&format, Arc::new(|_| {})).unwrap();
assert_eq!(stream.write(&[0; 16]).unwrap(), 16);
assert_eq!(stream.advance(4).unwrap(), 4);
assert_eq!(stream.played_frames(), 4);
dac.detach();
assert!(stream.write(&[0; 4]).is_err());
```

Profiles are exact PCM formats. Explicit `advance` consumes queued frames;
`lose_frames` injects loss, and `detach` invalidates all existing handles. Test
backpressure, pause, drain, flush, and recovery decisions without timers.
Callbacks run inline after fake locks are released; real callbacks run on the
library event thread. Clock/feedback, capture/duplex, routing, hardware volume,
and bit-perfect verification require their real implementations and dedicated
protocol/hardware checks.
