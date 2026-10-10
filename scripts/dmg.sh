#!/bin/sh
# Pack target/release/my-browser.app into target/release/my-browser-<version>.dmg:
# the app next to an Applications shortcut, compressed. Run scripts/bundle.sh first.
set -eu
cd "$(dirname "$0")/.."
APP=target/release/my-browser.app
VERSION=$(/usr/libexec/PlistBuddy -c "Print CFBundleShortVersionString" "$APP/Contents/Info.plist")
OUT=target/release/my-browser-$VERSION.dmg
STAGE=$(mktemp -d)
cp -R "$APP" "$STAGE/"
ln -s /Applications "$STAGE/Applications"
rm -f "$OUT"
hdiutil create -quiet -volname "my-browser $VERSION" -srcfolder "$STAGE" -fs HFS+ -format UDZO -ov "$OUT"
rm -rf "$STAGE"
echo "built $OUT"
