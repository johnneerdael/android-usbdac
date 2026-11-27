#!/usr/bin/env bash
# Boot a disposable virtual UAC1 device; no physical USB passthrough or network.
set -euo pipefail
root=$(git rev-parse --show-toplevel)
output=${1:-"$root/target/qemu-usb-smoke"}
mkdir -p "$output"
output=$(cd "$output" && pwd)
export USBDAC_PROBE_REVISION
USBDAC_PROBE_REVISION=$(git rev-parse HEAD)
export LIBUDEV_NO_PKG_CONFIG=1
export CC_x86_64_unknown_linux_musl=musl-gcc
cargo build --locked --release -p usbdac-core --example qemu_smoke --example two_dac_smoke --target x86_64-unknown-linux-musl --features libusb1-sys/vendored
# Versioned release ISO has a publisher SHA-256 file; netboot checksum URLs do not.
alpine_iso=alpine-virt-3.22.8-x86_64.iso
curl --fail --location --retry 3 "https://dl-cdn.alpinelinux.org/alpine/v3.22/releases/x86_64/$alpine_iso" -o "$output/$alpine_iso"
curl --fail --location --retry 3 "https://dl-cdn.alpinelinux.org/alpine/v3.22/releases/x86_64/$alpine_iso.sha256" -o "$output/$alpine_iso.sha256"
(cd "$output" && sha256sum -c "$alpine_iso.sha256")
mkdir -p "$output/iso"
bsdtar -xf "$output/$alpine_iso" -C "$output/iso" boot/vmlinuz-virt boot/initramfs-virt
cp "$output/iso/boot/vmlinuz-virt" "$output/vmlinuz-virt"
cp "$output/iso/boot/initramfs-virt" "$output/initramfs-virt"
mkdir -p "$output/rootfs"
(cd "$output/rootfs" && gzip -dc ../initramfs-virt | cpio -id --quiet)
cp "$root/target/x86_64-unknown-linux-musl/release/examples/qemu_smoke" "$output/rootfs/qemu_smoke"
cp "$root/target/x86_64-unknown-linux-musl/release/examples/two_dac_smoke" "$output/rootfs/two_dac_smoke"
cp "$root/.github/qemu/init" "$output/rootfs/init"
chmod +x "$output/rootfs/init" "$output/rootfs/qemu_smoke" "$output/rootfs/two_dac_smoke"
(cd "$output/rootfs" && find . -print0 | cpio --null -o --format=newc --quiet | gzip -1 > ../probe-initramfs.gz)
# Record a bounded acceptance window before starting the owned runtime.
python3 - "$output/acceptance-window.txt" <<'DEADLINE'
from datetime import datetime, timedelta, timezone
import sys
start = datetime.now(timezone.utc)
with open(sys.argv[1], "w") as output:
    output.write(f"acceptance_start={start.isoformat()}\nstop_deadline={(start + timedelta(seconds=60)).isoformat()}\nhost_timeout_seconds=60\nsingle_guest_timeout_seconds=12\ntwo_guest_timeout_seconds=15\n")
DEADLINE
set +e
timeout 60s qemu-system-x86_64 -machine q35,accel=tcg -m 256 -nodefaults -no-reboot -nographic \
  -serial stdio -kernel "$output/vmlinuz-virt" -initrd "$output/probe-initramfs.gz" \
  -append 'console=ttyS0 quiet panic=-1' -device qemu-xhci,id=xhci \
  -audiodev none,id=silent -device usb-audio,id=dac1,bus=xhci.0,audiodev=silent \
  -device usb-audio,id=dac2,bus=xhci.0,audiodev=silent \
  > "$output/acceptance.log" 2>&1
qemu_result=$?
set -e
cat "$output/acceptance.log"
test "$qemu_result" -eq 0
grep -q 'QEMU_PCM_PASS' "$output/acceptance.log"
grep -q 'SINGLE_PROBE_EXIT=0' "$output/acceptance.log"
grep -q 'VOLUME_RESTORED' "$output/acceptance.log"
grep -q 'TWO_DACS_PASS' "$output/acceptance.log"
grep -q 'TWO_PROBE_EXIT=0' "$output/acceptance.log"
sha256sum "$output/acceptance.log" "$output/rootfs/qemu_smoke" "$output/rootfs/two_dac_smoke" > "$output/SHA256SUMS"
