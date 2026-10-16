#!/bin/sh
# Pack target/release/my-browser.app into target/release/my-browser-<version>.dmg:
# the app next to an Applications shortcut, compressed. Run scripts/bundle.sh first.
# NOTARY_PROFILE=… (with SIGN_IDENTITY=…) also notarizes and staples it.
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
# NOTARY_PROFILE=<notarytool keychain profile> (made once with
# `xcrun notarytool store-credentials`) has Apple notarize the DMG and
# staples the ticket, so Gatekeeper opens it without "Open Anyway". Needs an
# app signed with SIGN_IDENTITY.
if [ -n "${NOTARY_PROFILE:-}" ]; then
  codesign --force --timestamp --sign "${SIGN_IDENTITY:?SIGN_IDENTITY is needed to notarize}" "$OUT"
  xcrun notarytool submit "$OUT" --keychain-profile "$NOTARY_PROFILE" --wait
  xcrun stapler staple "$OUT"
fi
echo "built $OUT"
