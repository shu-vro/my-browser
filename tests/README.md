# Tests

```sh
cargo test --release
```

This runs four groups of tests:

| Where | What it checks | Needs a GUI? |
|---|---|---|
| `src/main.rs` (unit tests) | Address-bar input becomes the right URL or search; `MYB_TUNE` settings are parsed correctly; only web pages and files open from other apps | No |
| `src/passwords.rs`, `src/import.rs`, `src/crash.rs`, `src/extensions.rs` (unit tests) | Passwords only for HTTPS (and this Mac), the memory vault, CSV exports of every browser; Chromium bookmarks JSON, bookmarks HTML files, Chromium password decryption, finding browsers; crash report summaries, issue links, new reports only; CRX/zip packages, Web Store ids. An opt-in keychain round trip (`-- --ignored keychain`) writes to the real login keychain | No |
| `benchmark/bench.rs` (unit tests) | Process measurement on itself, the C struct layout, event parsing, median and pass/fail logic, reading and writing the bars file, image generation, and the local page server (all pages, 404s) | No |
| `src/store.rs`, `src/privacy.rs`, `src/internal.rs` (unit tests) | Settings validation (unknown keys and bad values refused), history search with literal `%`/`_`, downloads and extensions tables, persistence on disk, JSON escaping, tracker list unpacks and has the key rules (including the Messenger exemption), leftover cleanup only touches the known old paths, sizes don't follow symlinks | No |
| `tests/e2e.rs` | Launches the real browser and drives it (see the list below) | Yes |

The end-to-end tests (53) cover:

- **Protocol and navigation:** loads report timing, failed loads, unknown commands, quitting, back/forward/reload with toolbar state, untitled pages
- **Tabs and shortcuts:** ⌘T, ⌘W, ⇧⌘T, ⌘1/⌘9, ⌃Tab, ⇧⌘[ ], history and zoom keys, ⌘, ⌘Y ⌥⌘L open and reuse the settings tab, background tabs sleep and wake
- **Profiles:** cookies survive a restart, throwaway profiles keep nothing, one browser per profile, the session comes back asleep with the same tab selected
- **Data:** history recorded, searchable and clearable, downloads land in the chosen folder without overwriting, clearing site data signs you out
- **Settings page:** it loads and its bridge works, websites can't use the bridge or open or frame internal pages, unknown internal pages fail
- **Extensions:** a content script runs, can be disabled and removed, and a folder without a manifest is refused
- **Find in page:** ⌘F opens the bar, next/previous move the selection between matches and wrap both ways, a new term starts at the top, "No matches", Esc closes, ⌘G and ⇧⌘G reopen and search, and the bar survives navigation and a tab sleeping
- **Storage:** sizes and counts are reported, clearing the cache keeps logins, the downloads list clears, and leftovers are deleted while profiles and other apps' files are kept (in a fake home folder)
- **Engine and privacy:** every private WebKit setting exists on this macOS, and the tracker blocker compiles
- **Benchmark window:** it's visible, and it never takes keyboard focus
- **Links from other apps:** web pages and local files open in new tabs, other schemes don't; only the installed app can be the default browser
- **Crashes:** a panic is logged in the profile and offered for reporting once
- **Bookmarks:** ⌘D and the star, the menu with folders, opening one, editing and deleting from Settings
- **Private windows:** cookies isolated both ways and gone with the last private tab, no history, session, Reopen Closed Tab or downloads list; passwords filled but never saved
- **Printing:** a page prints to a real PDF, and `window.print()` opens the print panel
- **Link menu:** new tab, new window and private window from a link (with a stand-in menu)
- **Tabs:** duplicate, close others, move to a new window, merge, and every window coming back after a restart
- **Media:** fullscreen and picture in picture available, screen sharing asks (and a blocked site isn't asked)
- **Reader mode:** the article alone, at the page's address, with no scripts, and back
- **Chrome extensions:** `.crx` and `.zip` packages install into the profile; junk is refused
- **Passwords:** the page can't reach the handler, sign-in offers to save, focus offers the account, picking fills it, "Never" sticks, CSV import
- **Import:** bookmarks and history from a made-up Brave profile and Safari's files (in a fake home folder), and a bookmarks file

Every test uses a throwaway profile, or a temporary one that's deleted
afterwards (including WebKit's cookie folder for it), so tests never touch
your real browsing data and can run in parallel.

The end-to-end tests open real browser windows. They need a logged-in macOS
session and won't work over plain SSH; GitHub's macOS runners have one, so CI
runs them. Each test takes about a second; the whole suite about 12 s with a
browser per core.

Run one group:

```sh
cargo test --release --bin my-browser   # browser unit tests
cargo test --release --bin bench        # harness tests
cargo test --release --test e2e         # end-to-end tests
```

If `engine_and_privacy_settings_are_applied` fails after a macOS update, Apple removed or
renamed one of the private WebKit settings. The browser still runs, but it
loses that memory saving or privacy protection. Rerun the benchmark to see what it costs.

Performance is measured separately. See [benchmark/README.md](../benchmark/README.md).
