#!/usr/bin/env bash
# Optional independent snd-usb-audio descriptor replay, isolated from lifecycle guest.
set -euo pipefail
root=$(git rev-parse --show-toplevel)
base=${USBDAC_USBIP_OUTPUT:-"$root/target/usb-lifecycle"}
out=${USBDAC_ALSA_OUTPUT:-"$root/target/usb-alsa"}
fixture="$root/.github/usb-lifecycle"
tar_cmd=tar
if [ "$(uname -s)" = Darwin ]; then tar_cmd=gtar; fi
case ${1:-} in
build)
    test -f "$base/downloads/rootfs.tar.gz"
    test -f "$base/downloads/kernel.apk"
    test -x "$base/root/fixture/usbip_uac"
    printf '%s  %s\n' 188416d41f9f0c9a6e9427b75149e43ccf3a89587b2d27c9ad506e7ffca78d1c "$base/downloads/rootfs.tar.gz" | sha256sum -c -
    printf '%s  %s\n' 60374bc0d9b125bc710f952d4508fd3ad4c78523c75ab1be1ec54fc3267cd54f "$base/downloads/kernel.apk" | sha256sum -c -
    mkdir -p "$out"
    rm -rf "$out/root"
    mkdir -p "$out/root/fixture/alsa"
    "$tar_cmd" -xzf "$base/downloads/rootfs.tar.gz" -C "$out/root"
    "$tar_cmd" -xzf "$base/downloads/kernel.apk" --ignore-zeros -C "$out/root"
    cp "$out/root/boot/vmlinuz-lts" "$out/vmlinuz-lts"
    cp "$base/root/fixture/usbip_uac" "$out/root/fixture/usbip_uac"
    for module in usbcore usbip-core vhci-hcd; do
        path=$(find "$out/root/lib/modules" -name "$module.ko.gz" -print -quit)
        test -n "$path"
        gzip -dc "$path" > "$out/root/fixture/$module.ko"
    done
    for module in soundcore snd snd-timer snd-pcm snd-hwdep snd-seq-device snd-rawmidi mc snd-usbmidi-lib snd-usb-audio; do
        path=$(find "$out/root/lib/modules" -name "$module.ko.gz" -print -quit)
        test -n "$path"
        gzip -dc "$path" > "$out/root/fixture/alsa/$module.ko"
        printf '%s.ko\n' "$module" >> "$out/root/fixture/alsa/order"
    done
    cp "$fixture/alsa-init" "$out/root/init"
    chmod +x "$out/root/init"
    (cd "$out/root" && find . -print0 | cpio --null -o -H newc | gzip -9 > "$out/initramfs.gz")
    sha256sum "$base/downloads/kernel.apk" "$base/downloads/rootfs.tar.gz" "$out/vmlinuz-lts" "$out/initramfs.gz" "$out/root/fixture/usbip_uac" "$out/root/fixture/alsa/"*.ko > "$out/manifest.txt"
    echo "ALSA_BUILD_READY $out"
    ;;
run)
    test -f "$out/manifest.txt" && test -f "$out/initramfs.gz" && test -f "$out/vmlinuz-lts"
    sha256sum -c "$out/manifest.txt"
    deadline=$(python3 -c 'from datetime import datetime, timedelta, timezone; print((datetime.now(timezone.utc) + timedelta(minutes=2)).strftime("%Y-%m-%dT%H:%M:%SZ"))')
    printf 'stop_deadline_utc=%s\n' "$deadline" | tee "$out/acceptance-window.txt"
    timeout 120 qemu-system-aarch64 -machine virt -cpu cortex-a72 -m 1024 -nographic -no-reboot -nic none -kernel "$out/vmlinuz-lts" -initrd "$out/initramfs.gz" -append 'console=ttyAMA0 rdinit=/init panic=-1' > "$out/serial.log" 2>&1
    python3 "$fixture/alsa-verify.py" "$out/serial.log"
    echo "ALSA_ACCEPTANCE_PASS $out"
    ;;
*) echo "usage: $0 build|run" >&2; exit 2;;
esac
