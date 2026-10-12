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
# hdiutil fails now and then on CI runners ("Resource busy"): retry, and keep
# its errors visible.
for try in 1 2 3 4 5; do
  hdiutil create -volname "my-browser $VERSION" -srcfolder "$STAGE" -fs HFS+ -format UDZO -ov "$OUT" && break
  [ "$try" = 5 ] && exit 1
  echo "hdiutil failed, retrying ($try)" >&2
  sleep $((try * 5))
done
rm -rf "$STAGE"
echo "built $OUT"
