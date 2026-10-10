#!/bin/sh
# Regenerate resources/AppIcon.icns from resources/icon.svg (macOS renders SVG
# natively through sips; iconutil packs the sizes).
set -eu
cd "$(dirname "$0")/.."
SET=$(mktemp -d)/AppIcon.iconset
mkdir -p "$SET"
sips -s format png resources/icon.svg --out "$SET/icon_512x512@2x.png" >/dev/null
for s in 16 32 128 256 512; do
  sips -z $s $s "$SET/icon_512x512@2x.png" --out "$SET/icon_${s}x${s}.png" >/dev/null
  d=$((s * 2))
  [ $s = 512 ] || sips -z $d $d "$SET/icon_512x512@2x.png" --out "$SET/icon_${s}x${s}@2x.png" >/dev/null
done
iconutil -c icns "$SET" -o resources/AppIcon.icns
echo "wrote resources/AppIcon.icns"
