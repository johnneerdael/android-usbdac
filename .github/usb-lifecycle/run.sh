#!/usr/bin/env bash
# Build a digest-pinned disposable guest, then run it only in a separate acceptance window.
set -euo pipefail
root=$(git rev-parse --show-toplevel)
out=${USBDAC_USBIP_OUTPUT:-"$root/target/usb-lifecycle"}
mkdir -p "$out"
out=$(cd "$out" && pwd)
fixture="$root/.github/usb-lifecycle"
tar_cmd=tar
if [ "$(uname -s)" = Darwin ]; then tar_cmd=gtar; fi
base=https://dl-cdn.alpinelinux.org/alpine/v3.22
fetch() {
    local url=$1 hash=$2 file=$3
    if [ ! -f "$file" ]; then curl -fL --retry 3 "$url" -o "$file"; fi
    printf '%s  %s\n' "$hash" "$file" | sha256sum -c -
}
if [ "${1:-}" = build ]; then
    mkdir -p "$out/downloads" "$out/root" "$out/compiled"
    fetch "$base/releases/aarch64/alpine-minirootfs-3.22.1-aarch64.tar.gz" 188416d41f9f0c9a6e9427b75149e43ccf3a89587b2d27c9ad506e7ffca78d1c "$out/downloads/rootfs.tar.gz"
    fetch "$base/main/aarch64/linux-lts-6.12.119-r0.apk" 60374bc0d9b125bc710f952d4508fd3ad4c78523c75ab1be1ec54fc3267cd54f "$out/downloads/kernel.apk"
    fetch "$base/community/aarch64/openjdk17-jre-headless-17.0.19_p10-r0.apk" 3d70540059df8771a6329e1d9a0af665df0c3deee670cf32a049de713d8e289f "$out/downloads/jre.apk"
    fetch "$base/main/aarch64/zlib-1.4-r0.apk" 279972bab4f817182d9617984880bc29471f1d3016f92265be10d71e6f95ca74 "$out/downloads/zlib.apk"
    fetch https://github.com/JetBrains/kotlin/releases/download/v2.4.0/kotlin-compiler-2.4.0.zip ba1b9e6eb6ddc3275079224f2e9ea4a2b02eef7d59ce2d38404f04b22613c20a "$out/downloads/kotlin.zip"
    # Recreate only job-owned outputs; preserve verified downloads and any local baseline.
    rm -rf "$out/root" "$out/compiled"
    mkdir -p "$out/root" "$out/compiled" "$out/root/fixture"
    "$tar_cmd" -xzf "$out/downloads/rootfs.tar.gz" -C "$out/root"
    mkdir -p "$out/compiled/kernel"
    "$tar_cmd" -xzf "$out/downloads/kernel.apk" --ignore-zeros -C "$out/compiled/kernel"
    for apk in jre zlib; do "$tar_cmd" -xzf "$out/downloads/$apk.apk" --ignore-zeros -C "$out/root"; done
    unzip -q "$out/downloads/kotlin.zip" -d "$out/compiled"
    cp "$out/compiled/kernel/boot/vmlinuz-lts" "$out/vmlinuz-lts"
    for module in usbcore usbip-core vhci-hcd; do
        module_path=$(find "$out/compiled/kernel/lib/modules" -name "$module.ko.gz" -print -quit)
        test -n "$module_path"
        gzip -dc "$module_path" > "$out/root/fixture/$module.ko"
    done
    export LIBUDEV_NO_PKG_CONFIG=1
    export RUSTUP_TOOLCHAIN=1.91.1
    toolchain_bin=$(dirname "$(rustup which --toolchain "$RUSTUP_TOOLCHAIN" rustc)")
    export PATH="$toolchain_bin:$PATH"
    rustc --version | grep -q '^rustc 1\.91\.1 '
    cargo --version | grep -q '^cargo 1\.91\.1 '
    target=aarch64-unknown-linux-musl
    if [ "$(uname -s)" = Darwin ]; then
        rustup run "$RUSTUP_TOOLCHAIN" cargo zigbuild --locked --release -p usbdac-core --example usbip_lifecycle --target "$target" --features libusb1-sys/vendored
        RUSTFLAGS='-C target-feature=-crt-static' rustup run "$RUSTUP_TOOLCHAIN" cargo zigbuild --locked --release -p usbdac-jni -p usbdac-core --target "$target" --features libusb1-sys/vendored
        zig cc -target aarch64-linux-musl -static -O2 "$fixture/usbfs_jni_launcher.c" -o "$out/root/fixture/usbfs_jni_launcher"
        printf '#!/bin/sh\nexec zig cc -target aarch64-linux-musl "$@"\n' > "$out/compiled/zig-cc"
        chmod +x "$out/compiled/zig-cc"
        rustup run "$RUSTUP_TOOLCHAIN" rustc --edition 2021 --target "$target" -C linker="$out/compiled/zig-cc" "$fixture/usbip_uac.rs" -o "$out/root/fixture/usbip_uac"
    else
        uapi="$out/compiled/uapi"
        mkdir -p "$uapi"
        ln -s /usr/include/linux "$uapi/linux"
        ln -s /usr/include/asm-generic "$uapi/asm-generic"
        ln -s "/usr/include/$(gcc -print-multiarch)/asm" "$uapi/asm"
        export USBDAC_LINUX_UAPI="$uapi"
        printf '#!/bin/sh\nexec musl-gcc -isystem "$USBDAC_LINUX_UAPI" "$@"\n' > "$out/compiled/musl-cc"
        chmod +x "$out/compiled/musl-cc"
        export CC_aarch64_unknown_linux_musl="$out/compiled/musl-cc"
        export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER="$out/compiled/musl-cc"
        rustup run "$RUSTUP_TOOLCHAIN" cargo build --locked --release -p usbdac-core --example usbip_lifecycle --target "$target" --features libusb1-sys/vendored
        RUSTFLAGS='-C target-feature=-crt-static' rustup run "$RUSTUP_TOOLCHAIN" cargo build --locked --release -p usbdac-jni -p usbdac-core --target "$target" --features libusb1-sys/vendored
        "$out/compiled/musl-cc" -static -O2 "$fixture/usbfs_jni_launcher.c" -o "$out/root/fixture/usbfs_jni_launcher"
        rustup run "$RUSTUP_TOOLCHAIN" rustc --edition 2021 --target "$target" -C linker="$out/compiled/musl-cc" "$fixture/usbip_uac.rs" -o "$out/root/fixture/usbip_uac"
    fi
    cp "$root/target/$target/release/examples/usbip_lifecycle" "$out/root/fixture/usbip_lifecycle"
    cp "$root/target/$target/release/libusbdac.so" "$out/root/fixture/libusbdac.so"
    cp "$out/compiled/kotlinc/lib/kotlin-stdlib.jar" "$out/root/fixture/kotlin-stdlib.jar"
    "$out/compiled/kotlinc/bin/kotlinc" "$root/android/usbdac/src/main/kotlin/dev/usbdac/internal/Native.kt" -d "$out/root/fixture/classes.jar"
    mkdir -p "$out/root/fixture/jni-classes"
    javac --release 17 -cp "$out/root/fixture/classes.jar:$out/root/fixture/kotlin-stdlib.jar" -d "$out/root/fixture/jni-classes" "$fixture/JniRetirementProbe.java"
    cp "$fixture/init" "$out/root/init"
    chmod +x "$out/root/init"
    (cd "$out/root" && find . -print0 | cpio --null -o -H newc | gzip -9 > "$out/initramfs.gz")
    {
        printf 'source_sha=%s\n' "$(git -C "$root" rev-parse HEAD)"
        git -C "$root" status --short
        printf 'source_diff_sha256=%s\n' "$(git -C "$root" diff --binary | sha256sum | cut -d' ' -f1)"
        rustc --version --verbose
        cargo --version
        printf 'rustc_path=%s\n' "$(command -v rustc)"
        printf 'cargo_path=%s\n' "$(command -v cargo)"
        javac -version 2>&1
        qemu-system-aarch64 --version | sed -n '1p'
        if command -v musl-gcc >/dev/null; then musl-gcc --version | sed -n '1p'; else zig version; fi
        sha256sum "$fixture/run.sh" "$fixture/usbip_uac.rs" "$fixture/init" "$fixture/JniRetirementProbe.java" "$fixture/usbfs_jni_launcher.c" "$root/crates/usbdac-core/examples/usbip_lifecycle.rs" "$root/android/usbdac/src/main/kotlin/dev/usbdac/internal/Native.kt"
        sha256sum "$out/downloads/"*
        sha256sum "$root/Cargo.lock" "$out/vmlinuz-lts" "$out/initramfs.gz" "$out/root/fixture/usbip_uac" "$out/root/fixture/usbip_lifecycle" "$out/root/fixture/libusbdac.so" "$out/root/fixture/classes.jar" "$out/root/fixture/kotlin-stdlib.jar" "$out/root/fixture/usbfs_jni_launcher" "$out/root/fixture/"*.ko
    } > "$out/manifest.txt"
    sha256sum "$out/vmlinuz-lts" "$out/initramfs.gz" > "$out/runtime.SHA256"
    echo "USBIP_BUILD_READY $out"
elif [ "${1:-}" = run ]; then
    test -f "$out/manifest.txt" && test -f "$out/runtime.SHA256" && test -f "$out/initramfs.gz" && test -f "$out/vmlinuz-lts"
    sha256sum -c "$out/runtime.SHA256"
    deadline=$(python3 -c 'from datetime import datetime, timedelta, timezone; print((datetime.now(timezone.utc) + timedelta(minutes=10)).strftime("%Y-%m-%dT%H:%M:%SZ"))')
    printf 'stop_deadline_utc=%s\n' "$deadline" | tee "$out/acceptance-window.txt"
    set +e
    timeout --kill-after=5 600 qemu-system-aarch64 -machine virt -cpu cortex-a72 -m 1024 -nographic -no-reboot -nic none -kernel "$out/vmlinuz-lts" -initrd "$out/initramfs.gz" -append 'console=ttyAMA0 rdinit=/init panic=-1' > "$out/serial.log" 2>&1
    status=$?
    set -e
    mkdir -p "$out/modes"
    python3 - "$out/serial.log" "$out/modes" <<'SPLIT'
import pathlib, re, sys
source, output = pathlib.Path(sys.argv[1]), pathlib.Path(sys.argv[2])
current = None
for line in source.read_text(errors="replace").splitlines(keepends=True):
    match = re.match(r"USBIP_MODE_START mode=([a-z0-9-]+) ", line)
    if match:
        current = (output / (match.group(1) + ".log")).open("w")
    if current:
        current.write(line)
        if line.startswith("USBIP_MODE_PASS ") or line.startswith("USBIP_MODE_FAIL "):
            current.close()
            current = None
if current:
    current.close()
SPLIT
    test "$status" -eq 0 || { echo "QEMU failed or timed out: $status"; exit 1; }
    for mode in normal bad-cur stale-cur badd full-uac3 duplex retire jni-retire full-uac3-clock reconfigure selector zero-res full-uac3-clock-bad full-uac3-clock-stale full-uac3-volatile refuse-set; do
        test "$(grep -c "^USBIP_MODE_PASS mode=$mode$" "$out/serial.log")" -eq 1
    done
    test "$(grep -c '^USBIP_AGGREGATE failures=0$' "$out/serial.log")" -eq 1
    ! grep -Eq 'USBIP_(MODE_FAIL|BOOT_FAIL|CLEANUP_FAIL)' "$out/serial.log"
    grep -q 'reboot: Power down' "$out/serial.log"
    echo "USBIP_ACCEPTANCE_PASS $out"
else
    echo "usage: $0 build|run" >&2
    exit 2
fi
