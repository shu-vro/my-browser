# my-browser

A web browser for macOS built to use as little memory as possible. It has no
developer tools and no telemetry; extensions are supported. It's a thin native
Rust shell around the WebKit engine that ships with macOS. Every change is
measured against recorded benchmarks, and a change that makes memory or speed
worse is rejected.

## What it does

- **Tabs that sleep.** A background tab unloads its page (and the process
  behind it) after a few minutes, or right away when macOS runs low on memory.
  It reloads when you click it. Restored sessions open asleep, so 50 restored
  tabs cost about the same as one.
- **Private by default.**
  - Built-in tracker blocking: Disconnect's list, the one Firefox uses.
  - All third-party cookies blocked, and WebKit's Intelligent Tracking
    Prevention on.
  - HTTPS first, Global Privacy Control, Safari's fingerprinting protection,
    and tracking parameters (fbclid, gclid…) removed from links.
  - Websites can't reach your Mac's local servers or devices on your network.
  - Protections can be turned down for one site from its site menu.
  - No ad-click attribution, and no safe-browsing lookups unless you turn them
    on.
  - The browser itself never sends anything anywhere. After a crash it asks
    whether you want to report it, as a GitHub issue you read first.
- **The basics, done properly:**
  - persistent logins (per-profile cookie store),
  - back, forward and reload,
  - history with search,
  - downloads,
  - session restore,
  - keyboard shortcuts for everything,
  - popups open as tabs,
  - file upload,
  - JavaScript dialogs.
- **Private windows** (⇧⌘N): cookies and site data only in memory, gone when
  the last private tab closes; no history, no session, no extensions.
  Downloaded files stay.
- **Passwords**, saved in your Mac's keychain: offered when you sign in,
  filled when you pick an account under a login field (⌥⌘A). Import a CSV
  from the Passwords app, Safari, Chrome, Edge, Brave or Firefox.
- **Import** bookmarks, history and passwords from Chrome, Brave, Edge,
  Vivaldi, Arc or Safari (Settings → Import), or a bookmarks file.
- **Bookmarks** (⌘D or the star): in the Bookmarks menu, with folders, and in
  Settings → Bookmarks.
- **Reader mode** (⇧⌘R): the article alone, in large type, without scripts.
- **Print and save as PDF** (⌘P).
- **Link menu**: right-click a link to open it in a new tab, window or
  private window.
- **Find in page** (⌘F, ⌘G, ⇧⌘G).
- **Can be your default browser** (Settings → General): links from other apps
  and HTML files from Finder open in new tabs.
- **Chrome extensions**: add them from the Chrome Web Store (a button appears
  on an extension's page), or install a `.crx`, `.zip` or unpacked folder.
  Unpacked folders get a Reload button in Settings to pick up your edits.
  Toolbar buttons and popups. Each one is off on every site until you click its button there,
  and doesn't even start (no memory) until its first click. Right-click a
  button to let it run everywhere.
- **Video**: fullscreen and picture in picture (⌥⌘P or the video's menu).
  **Screen sharing** for video calls, asked per site.
- **Site permissions:** camera, microphone, screen sharing, location and multiple downloads
  are asked per site and remembered; see and change them all in Settings →
  Site settings, or for one site from the button left of the address bar.
- **Storage controls:** see and clear cookies, cache, history and the
  downloads list, and delete data left behind by older builds.
- **New tab page** drawn natively (no page process): a banner, a search bar
  and your most visited sites, each can be turned off.
- **Light, dark or system appearance**, which sites with a dark mode follow.
- **Settings** at `mybrowser://settings`, opened with ⌘, : search engine, tab
  sleep, privacy, history, downloads, storage, extensions, and an uninstall
  button.

## Requirements

- macOS 15.4 or later (needed for WebKit's extension support). Apple Silicon or
  Intel.
- Rust 1.89 or later (`File::try_lock`). Edition 2024.

## Install

Download the DMG from the [releases](https://github.com/shu-vro/my-browser/releases),
open it and drag my-browser to Applications. It isn't notarized by Apple, so
the first launch is blocked: open it once, then choose **Open Anyway** in
System Settings → Privacy & Security.

## Run it

```sh
cargo run --release                   # opens with your profile
cargo run --release -- example.com    # opens a URL
./scripts/bundle.sh                   # builds target/release/my-browser.app
./scripts/dmg.sh                      # packs it into target/release/my-browser-<version>.dmg
```

Use the `.app` for daily browsing. Camera and microphone (video calls) only
work from the bundle, because macOS requires the permission text in its
Info.plist. WebKit keeps site data per app identity, so the `.app` and
`cargo run` have **separate logins**.

Command-line flags:

| Flag | Meaning |
|---|---|
| `--profile DIR` | Use another profile folder (default `~/Library/Application Support/my-browser`) |
| `--bench` | Driven over stdin/stdout by the benchmark and tests. Uses a throwaway profile unless `--profile` is given. |
| `--on-top` | Float the window above every app and Space (benchmarks need a visible window) |

## Test and measure

```sh
cargo test --release                  # 103 tests, opens real browser windows briefly
./target/release/bench                # benchmark against the recorded bars (~4 min)
```

- [tests/README.md](tests/README.md): what the tests cover and how to run them.
- [benchmark/README.md](benchmark/README.md): every metric, how runs are
  judged, how to beat a bar.

## Documentation

| Document | For |
|---|---|
| [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) | How it's built: processes, tabs, profiles, privacy, internal pages, extensions |
| [docs/DESIGN-DECISIONS.md](docs/DESIGN-DECISIONS.md) | How and why: every major decision as a question, with its evidence |
| [CONTRIBUTING.md](CONTRIBUTING.md) | The vision, the rules every change follows, the workflow |
| [SECURITY.md](SECURITY.md) | Threat model, trust boundaries, the privacy policy |
| [AGENTS.md](AGENTS.md) | Short operating manual for AI coding agents |
| [previous-iterations/](previous-iterations/) | What each development session did and left open |

## Layout

```
src/
  main.rs          app delegate: startup, profile, engine setup, tabs list, menu/shortcuts,
                   downloads, extension loading, bench protocol
  tab.rs           one tab: native window tab, toolbar, web view, sleep/wake, navigation policy,
                   printing, link menu, reader mode, password prompts
  home.rs          the native new tab page
  extensions.rs    extensions: click to run, toolbar buttons, .crx/.zip packages, the Web Store
  extwindow.rs     a window (tab group) as web extensions see it
  internal.rs      mybrowser:// pages and the settings-page bridge
  privacy.rs       tracking protection and the tracker blocker
  netgate.rs       the network gate for extension connections
  passwords.rs     saved passwords in the keychain, CSV import
  import.rs        import from Chrome, Brave, Edge, Vivaldi, Arc, Safari
  crash.rs         panic log and crash report prompt
  store.rs         SQLite: settings, history, bookmarks, downloads, extensions, session
resources/
  settings.html    the settings / history / downloads / extensions page
  passwords.js     the saved-passwords form script
  reader.js        reader mode's article finder
  icon.svg         the app icon (AppIcon.icns is generated from it)
  trackers.bin     compressed tracker blocker rules (generated)
scripts/
  bundle.sh        build the .app (UNIVERSAL=1 for both CPUs)
  dmg.sh           pack the .app into a DMG
  icon.sh          regenerate AppIcon.icns from icon.svg
  update-trackers.py   regenerate resources/trackers.bin
.githooks/         pre-commit checks (git config core.hooksPath .githooks)
.github/workflows/ CI on every push; a release for every push to `release`
benchmark/         benchmark harness and recorded bars
tests/             end-to-end tests and fixtures
```

## Licenses

The tracker list comes from Disconnect and is licensed **CC BY-NC-SA 4.0**:
non-commercial use only. Replace it before any commercial distribution.
