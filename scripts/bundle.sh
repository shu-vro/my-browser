#!/bin/sh
# Build target/release/my-browser.app: the browser as a real macOS app.
#
# Why a bundle: macOS only grants camera, microphone and location to apps whose
# Info.plist says why (Messenger/Meet calls need them), and the app gets its own
# identity (Dock icon, "Open with", its own WebKit storage folder).
#
# Note: WebKit keeps site data per app identity, so the bundled app and
# `cargo run` have separate cookie stores. Use one or the other.
#
# VERSION=x.y.z overrides Cargo.toml's version (the release workflow sets it).
# UNIVERSAL=1 builds for Apple Silicon and Intel and joins them (releases).
# SIGN_IDENTITY=<Developer ID Application identity> signs for distribution.
set -eu
cd "$(dirname "$0")/.."
APP=target/release/my-browser.app
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
if [ "${UNIVERSAL:-0}" = 1 ]; then
  for t in aarch64-apple-darwin x86_64-apple-darwin; do
    cargo build --release --bin my-browser --target $t
  done
  lipo -create -output "$APP/Contents/MacOS/my-browser" \
    target/aarch64-apple-darwin/release/my-browser target/x86_64-apple-darwin/release/my-browser
  # Translate Page's helper (build.rs builds it next to the browser).
  lipo -create -output "$APP/Contents/MacOS/my-browser-translate" \
    target/aarch64-apple-darwin/release/my-browser-translate target/x86_64-apple-darwin/release/my-browser-translate
else
  cargo build --release --bin my-browser
  cp target/release/my-browser target/release/my-browser-translate "$APP/Contents/MacOS/"
fi
cp resources/AppIcon.icns "$APP/Contents/Resources/AppIcon.icns"
VERSION=${VERSION:-$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)}
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
  <key>CFBundleIconFile</key><string>AppIcon</string>
  <key>CFBundleURLTypes</key>
  <array><dict>
    <key>CFBundleURLName</key><string>Web site URL</string>
    <key>CFBundleURLSchemes</key><array><string>http</string><string>https</string></array>
  </dict></array>
  <key>CFBundleDocumentTypes</key>
  <array><dict>
    <key>CFBundleTypeName</key><string>Web page</string>
    <key>CFBundleTypeRole</key><string>Viewer</string>
    <key>LSHandlerRank</key><string>Alternate</string>
    <key>LSItemContentTypes</key><array><string>public.html</string><string>public.xhtml</string></array>
  </dict></array>
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
# Ad-hoc signature by default: required for macOS to remember permission
# grants. SIGN_IDENTITY="Developer ID Application: …" signs with a Developer
# ID instead (with a secure timestamp, as notarization requires).
# The helper is signed first: the app's signature covers it.
HELPER="$APP/Contents/MacOS/my-browser-translate"
if [ -n "${SIGN_IDENTITY:-}" ]; then
  codesign --force --options runtime --timestamp --sign "$SIGN_IDENTITY" "$HELPER"
  codesign --force --options runtime --timestamp --entitlements "$ENT" --sign "$SIGN_IDENTITY" "$APP"
else
  codesign --force --options runtime --sign - "$HELPER"
  codesign --force --options runtime --entitlements "$ENT" --sign - "$APP"
fi
echo "built $APP"
