#!/bin/sh
# Build target/release/my-browser.app: the browser as a real macOS app.
#
# Why a bundle: macOS only grants camera, microphone and location to apps whose
# Info.plist says why (Messenger/Meet calls need them), and the app gets its own
# identity (Dock icon, "Open with", its own WebKit storage folder).
#
# Note: WebKit keeps site data per app identity, so the bundled app and
# `cargo run` have separate cookie stores. Use one or the other.
set -eu
cd "$(dirname "$0")/.."
cargo build --release
APP=target/release/my-browser.app
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS"
cp target/release/my-browser "$APP/Contents/MacOS/my-browser"
VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleExecutable</key><string>my-browser</string>
  <key>CFBundleIdentifier</key><string>local.my-browser</string>
  <key>CFBundleName</key><string>my-browser</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>$VERSION</string>
  <key>CFBundleVersion</key><string>$VERSION</string>
  <key>LSMinimumSystemVersion</key><string>15.4</string>
  <key>NSPrincipalClass</key><string>NSApplication</string>
  <key>NSHighResolutionCapable</key><true/>
  <key>NSCameraUsageDescription</key><string>Websites you allow can use the camera, for example for video calls.</string>
  <key>NSMicrophoneUsageDescription</key><string>Websites you allow can use the microphone, for example for calls.</string>
  <key>NSLocationUsageDescription</key><string>Websites you allow can ask for your location.</string>
</dict>
</plist>
PLIST
# Hardened runtime: no injected libraries (DYLD_INSERT_LIBRARIES), no
# debugger attaching, no unsigned code. With it on, macOS only lets the app
# use the camera, microphone and location if it says so in its entitlements.
ENT=target/release/entitlements.plist
cat > "$ENT" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>com.apple.security.device.camera</key><true/>
  <key>com.apple.security.device.audio-input</key><true/>
  <key>com.apple.security.personal-information.location</key><true/>
</dict>
</plist>
PLIST
# Ad-hoc signature: required for macOS to remember permission grants.
codesign --force --options runtime --entitlements "$ENT" --sign - "$APP"
echo "built $APP"
