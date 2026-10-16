#!/bin/sh
# Rebuild the .app and replace the copy in /Applications with it.
set -eu
cd "$(dirname "$0")/.."
./scripts/bundle.sh
osascript -e 'quit app "my-browser"' || true
while pgrep -xq my-browser; do sleep 0.2; done
rm -rf /Applications/my-browser.app
ditto target/release/my-browser.app /Applications/my-browser.app
open /Applications/my-browser.app
