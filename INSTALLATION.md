# Installing my-browser

my-browser is installed from its GitHub releases. There is no App Store or
Homebrew version yet.

## Requirements

- macOS 15.4 or later
- Apple Silicon or Intel (each release is a universal build)

## Install

1. Open the [releases page](https://github.com/shu-vro/my-browser/releases)
   and download `my-browser-<version>.dmg` from the latest release.
2. Open the DMG and drag **my-browser** onto the **Applications** shortcut.
3. Eject the DMG.

### First launch: "Apple could not verify…"

The first time you open it, macOS shows **"my-browser.app" Not Opened**:
*Apple could not verify "my-browser.app" is free of malware*. This happens
because the app isn't signed with a paid Apple Developer ID or notarized by
Apple. macOS hasn't found anything wrong with it. Downloaded files carry a
"quarantine" flag, and copying the app to Applications keeps that flag.

Allow it once, using either method:

- **System Settings:** click **Done** (not *Move to Bin*), open
  System Settings → Privacy & Security, scroll down, click **Open Anyway**
  next to my-browser, and confirm.
- **Terminal:**

  ```sh
  xattr -dr com.apple.quarantine /Applications/my-browser.app
  ```

After that, the app opens normally. Each new release you download asks once
again.

## Verify the download (optional)

Each release includes a `SHA256SUMS` file. Download it next to the DMG, then
run:

```sh
shasum -a 256 -c SHA256SUMS
```

It should print `my-browser-<version>.dmg: OK`.

## Make it your default browser (optional)

Settings (⌘,) → General → make my-browser the default browser.

## Update

The browser never checks for updates by itself. Choose **my-browser → Check
for Updates…** (or Settings → General → Check for updates): it opens the
latest release on GitHub. If its version is newer than the one Settings
shows, download the new DMG and drag the app to Applications again, replacing the old
one. Your profile (logins, history, bookmarks, extensions) is stored separately
and is kept.

## Uninstall

Settings → Storage → **Uninstall my-browser…** deletes the app and everything
it keeps (logins, history, extensions, settings), then quits. Files you
downloaded stay. To keep your data, drag `/Applications/my-browser.app` to the
Bin instead.

## Build from source

See [Run it](README.md#run-it) in the README: `./scripts/bundle.sh` builds the
`.app`, and `./scripts/dmg.sh` packs it into a DMG. Builds you make yourself
don't show the first-launch warning.
