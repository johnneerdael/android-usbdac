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

# Drag-to-install disk image: the app and a link to /Applications.
dmg="build/USB-DAC-Validator-macOS.dmg"
stage="build/dmg"
rm -rf "$stage" "$dmg"
mkdir -p "$stage"
/usr/bin/ditto "$app" "$stage/$(basename "$app")"
ln -s /Applications "$stage/Applications"
# hdiutil on CI runners sometimes reports a busy resource; retry briefly.
for attempt in 1 2 3; do
  hdiutil create -volname "USB DAC Validator" -srcfolder "$stage" -fs HFS+ -format UDZO -ov "$dmg" && break
  [[ $attempt -lt 3 ]] || exit 1
  sleep 5
done
rm -rf "$stage"
if [[ -n "${SIGNING_IDENTITY:-}" ]]; then
  codesign --force --timestamp --sign "$SIGNING_IDENTITY" "$dmg"
fi

# Notarize and staple when App Store Connect API credentials are given.
if [[ -n "${NOTARY_KEY_PATH:-}" ]]; then
  [[ -n "${SIGNING_IDENTITY:-}" && -n "${NOTARY_KEY_ID:-}" && -n "${NOTARY_ISSUER_ID:-}" ]] || {
    echo "Notarization needs SIGNING_IDENTITY, NOTARY_KEY_ID and NOTARY_ISSUER_ID"; exit 1; }
  result="build/notarization.json"
  xcrun notarytool submit "$dmg" --key "$NOTARY_KEY_PATH" --key-id "$NOTARY_KEY_ID" \
    --issuer "$NOTARY_ISSUER_ID" --wait --timeout 30m --output-format json > "$result"
  status=$(/usr/bin/plutil -extract status raw -o - "$result" 2>/dev/null || true)
  if [[ "$status" != Accepted ]]; then
    cat "$result"
    id=$(/usr/bin/plutil -extract id raw -o - "$result" 2>/dev/null || true)
    [[ -z "$id" ]] || xcrun notarytool log "$id" --key "$NOTARY_KEY_PATH" --key-id "$NOTARY_KEY_ID" --issuer "$NOTARY_ISSUER_ID"
    exit 1
  fi
  xcrun stapler staple "$dmg"
  xcrun stapler validate "$dmg"
fi
printf 'Built %s and %s\n' "$app" "$dmg"
