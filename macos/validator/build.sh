#!/bin/bash
set -euo pipefail
cd "$(dirname "$0")"
export MACOSX_DEPLOYMENT_TARGET=12.0
if command -v rustup >/dev/null; then
  RUSTC="$(rustup which rustc)"
  export RUSTC
fi
version=$(sed -n '/^\[workspace.package\]/,/^\[/s/^version = "\([0-9][0-9.]*\)"$/\1/p' ../../Cargo.toml)
[[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || { echo "Invalid workspace version"; exit 1; }
app="build/USB DAC Validator.app"
mkdir -p "$app/Contents/MacOS"
cargo build --release --locked --manifest-path worker/Cargo.toml --target aarch64-apple-darwin
swiftc -O -target arm64-apple-macos12.0 app/main.swift app/ValidatorModel.swift app/Brand.swift app/Views.swift -o "$app/Contents/MacOS/USB DAC Validator"
cp worker/target/aarch64-apple-darwin/release/usbdac-validator-worker "$app/Contents/MacOS/usbdac-validator-worker"
mkdir -p "$app/Contents/Resources"
cp branding/usbdac.icns "$app/Contents/Resources/usbdac.icns"
for font in Manrope.ttf manrope-OFL.txt IBMPlexMono-Regular.ttf ibmplexmono-OFL.txt; do
  cp "../../branding/fonts/$font" "$app/Contents/Resources/$font"
done
cat > "$app/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>CFBundleExecutable</key><string>USB DAC Validator</string>
<key>CFBundleIconFile</key><string>usbdac</string>
<key>CFBundleIdentifier</key><string>dev.usbdac.validator</string>
<key>CFBundleName</key><string>USB DAC Validator</string>
<key>CFBundlePackageType</key><string>APPL</string>
<key>CFBundleShortVersionString</key><string>$version</string>
<key>LSMinimumSystemVersion</key><string>12.0</string>
<key>NSHighResolutionCapable</key><true/>
</dict></plist>
PLIST
if [[ -n "${SIGNING_IDENTITY:-}" ]]; then
  codesign --force --timestamp --options runtime --sign "$SIGNING_IDENTITY" "$app/Contents/MacOS/usbdac-validator-worker"
  codesign --force --timestamp --options runtime --sign "$SIGNING_IDENTITY" "$app"
else
  codesign --force --sign - "$app/Contents/MacOS/usbdac-validator-worker"
  codesign --force --sign - "$app"
fi
/usr/bin/ditto -c -k --sequesterRsrc --keepParent "$app" "build/USB-DAC-Validator-macOS.zip"
printf 'Built %s and distribution ZIP\n' "$app"
