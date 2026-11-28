# Optional ALSA descriptor comparison

After `.github/usb-lifecycle/run.sh build` succeeds, prepare and run the independent comparison:

```sh
.github/usb-lifecycle/alsa-run.sh build
.github/usb-lifecycle/alsa-run.sh run
```

`USBDAC_USBIP_OUTPUT` selects the lifecycle build input and `USBDAC_ALSA_OUTPUT` selects the separate disposable output (defaults: `target/usb-lifecycle` and `target/usb-alsa`). The build verifies the same pinned Alpine ARM64 rootfs and 6.12.119 kernel APK digests as the lifecycle runner, copies its freshly built USB/IP daemon, extracts the ordered sound modules from that kernel package, and records hashes of the resulting kernel, initramfs, daemon and ALSA modules. It does not modify the lifecycle guest. Run records a two-minute stop deadline, disables guest networking apart from loopback, passes through no physical USB port, and requires successful QEMU exit, all expected profile markers, and guest poweroff. Save `manifest.txt`, `acceptance-window.txt`, and `serial.log` as evidence.

The guest loads `snd-usb-audio` and reads its `/proc/asound/card*/stream0` playback descriptor after attaching each synthetic fixture. The host verifier checks stereo, alternate setting, PCM format, advertised rates, playback endpoint and explicit feedback endpoint against the retained comparison: normal and full UAC3 clock expose S16_LE at 48/96 kHz on synchronous OUT 0x01; BADD exposes S16_LE and S24_3LE at 48 kHz on asynchronous OUT 0x01 with IN 0x81 feedback. The captured values describe ALSA's enumeration of these synthetic descriptors; they do not establish actual playback, physical DAC compatibility, or broader ALSA parity.
