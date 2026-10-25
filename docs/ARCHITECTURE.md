# Architecture

my-browser is a native macOS app written in Rust. It draws its own UI with
AppKit and renders pages with the system's WebKit (`WKWebView`). This document
explains how the pieces fit, why they're built this way, and the rules each
piece must keep.

## Where memory goes, and so where we work

A browser's memory is mostly the engine running pages: JavaScript heaps, DOM
trees, decoded images, GPU buffers. Our measurements, all totals across every
process:

| Situation | Total memory |
|---|---|
| Blank page | ~71 MB (app 28 MB, WebKit page/GPU/network processes ~43 MB) |
| 100,000-element page | ~430 MB |
| 1M-object JavaScript heap | ~260 MB |

The browser's own code is a rounding error. So the work happens in three
places:

1. **Run fewer pages.** Sleeping tabs, asleep session restore, no
   back/forward cache.
2. **Load less.** Tracker blocking, no autoplay.
3. **Start fewer processes.** No prewarmed page process.

We don't write a rendering engine and don't wrap Chromium.

### Why not our own engine

A homemade engine can't hold the same page in much less memory. The DOM, the
JS objects and the decoded images are the page's own data, and any engine has
to keep them. It could only save memory by not supporting parts of the web,
which breaks sites. WebKit's JavaScript JIT and garbage collector have years of
speed work behind them. Servo and Ladybird, both funded teams, aren't ready for
everyday browsing after years of work. The right move is a native shell that
does less work around WebKit, plus tooling to prove each saving.

## Processes

```
my-browser (our process: AppKit UI, Rust logic, SQLite)
  ├─ com.apple.WebKit.WebContent   one per site in use (pages, JS)   ← most memory
  ├─ com.apple.WebKit.GPU          compositing, media, canvas
  └─ com.apple.WebKit.Networking   HTTP, cookies, cache, content blocker
```

launchd starts the WebKit processes as XPC services; they're not children of
our process. The benchmark harness finds them by their "responsible process",
name and start time (see benchmark/README.md).

## Source map

| File | Role |
|---|---|
| `src/main.rs` | `App`, the application delegate and the only long-lived object. It owns the profile store, the shared WebKit objects (data store, preferences, process pool, user content controller, extension controller), the tab list, downloads in flight and the menu bar. It also runs the bench protocol. |
| `src/tab.rs` | `Tab`: one native window tab with its toolbar (back, forward, reload, home, site button, address bar) and (while awake) one `WKWebView`. Navigation policy, JavaScript dialogs, permission prompts, the site menu, error pages, extension buttons, and the extension-tab delegate. |
| `src/tabsearch.rs` | The launcher, Search Tabs and More (⇧⌘A): the panel, its list, the matcher and the menu-bar walk |
| `src/usage.rs` | Memory and CPU of the browser's processes (Settings → Tabs, heavy-tab badge) |
| `src/home.rs` | The native new tab page (banner, search field, most visited sites) |
| `src/extensions.rs` | Extensions: read at launch, started on first click, turned on per site, toolbar buttons |
| `src/extwindow.rs` | `ExtWindow`: a tab group, as the WebExtensions `windows` API sees it |
| `src/internal.rs` | Serves `mybrowser://` pages. Validates and runs settings-page bridge operations. |
| `src/privacy.rs` | Tracking protection switches, the tracker and local-network content blockers, per-page protections, tracking-parameter stripping |
| `src/crx.rs` | CRX3 package signatures (Security.framework), and the extension id they prove |
| `src/netgate.rs` | The network gate: extension WebSockets the browser connects for, each blocked until allowed |
| `src/shortcuts.rs` | Changing keyboard shortcuts (Settings → Shortcuts) |
| `src/sitedata.rs` | Site data per site: list and delete (site menu sheet, Settings → Site data) |
| `src/suggest.rs` | Address bar suggestions: the list under the bar |
| `src/store.rs` | `Store`: SQLite tables for settings, history, bookmarks, downloads, extensions, session, site permissions, allowed network rules |
| `src/passwords.rs` | Saved passwords: the keychain (or memory for throwaway profiles), CSV import |
| `src/import.rs` | Import bookmarks, history and passwords from Chromium browsers and Safari, and bookmarks files |
| `src/crash.rs` | The panic hook, finding crash reports, and the pre-filled GitHub issue |
| `src/translate.rs` | Translate Page: the helper process and its requests |
| `helpers/translate.swift` | `my-browser-translate`, the helper: Apple's Translation and NaturalLanguage frameworks (built by `build.rs`) |
| `src/pdf.rs` | The PDF viewer (PDFKit) over a tab showing a PDF |
| `src/files.rs` | Paths in the address bar, folder pages (`mybrowser://files/…`), opening local files |
| `src/autofill.rs` | Addresses and cards: settings bridge, labels, the values a form gets |
| `src/profiles.rs` | Profiles: listing, adding, opening (another copy of the app), deleting |
| `src/syncfile.rs` | Sync by file: export and import |
| `resources/translate.js`, `find.js`, `picker.js` | Translate Page's text collector, find's highlights and count, Hide Element's picker (all in the browser's own script world, run on demand) |
| `resources/settings.html` | The internal page: settings, privacy, site settings, bookmarks, passwords, import, history, downloads, storage, extensions, network |
| `resources/passwords.js` | The saved-passwords and autofill form script (its own script world) |
| `resources/youtube.js` | Removes YouTube's ad schedule from the player's data (page world, document start, every frame; inert on other sites) |
| `resources/reader.js` | Reader mode's article finder (run on demand, in the browser's script world) |
| `resources/icon.svg`, `AppIcon.icns` | The app icon and its source |
| `resources/trackers.bin` | Content-blocker rules, generated by `scripts/update-trackers.py` |

All AppKit and WebKit objects live on the main thread (`MainThreadOnly`).
Other threads: the bench-mode stdin reader (it forwards each line to the main
thread with `performSelectorOnMainThread`), and a short-lived one per network
gate connection that resolves its name. Results from other threads come back
through `on_main`; values they carry untouched are wrapped in `MainOnly`.

## Startup

1. `main` sets the activation policy, creates `App`, builds the menu and runs
   the app.
2. `App::start`:
   1. Resolves the profile (`--profile`, throwaway for `--bench`, otherwise
      `~/Library/Application Support/my-browser`).
   2. Takes the profile lock and opens `browser.db`.
   3. Creates the profile's `WKWebsiteDataStore`, turns on tracking
      protections, and creates the shared preferences, process pool, user
      content controller (with the bridge handler) and extension controller.
   4. Reads enabled extensions' manifests (for their buttons) and starts only
      the ones set to run on every site.
3. It asks WebKit for the compiled tracker blocker, local-network shield,
   cookie-banner hider and the user's own filters (compiling them on first
   launch or after a change). **No tab opens until they're attached**, so no
   page ever loads unprotected.
4. `App::ready` restores the session asleep (or opens the URLs given on the
   command line, or a new tab with the native home page) and selects the
   previously active tab. In bench mode it then prints `TUNE …` lines and
   `READY`.

### Crash reports (`src/crash.rs`)

Nothing is reported automatically. Two records stay on the Mac: the panic
hook (`crash::install`, set in `App::start`) writes the message, location and
backtrace to `<profile>/crashes/panic-<ms>.log` before `panic = "abort"`
ends the process, and macOS writes its own report to
`~/Library/Logs/DiagnosticReports/my-browser-*.ips`. At the end of `ready`,
`App::offer_crash_report` looks for either written since `crash_seen_ms`
(the first launch of a profile only records the time) and asks once with a
sheet. "Report…" opens a GitHub new-issue link in a tab, with the version,
macOS version and the summary (a panic log as written, or an `.ips`'s
exception and crashed-thread frames), and reveals the full report in Finder
to attach. Bench mode skips the shared DiagnosticReports folder so tests
never see the owner's crashes.

Release builds are stripped; the release workflow publishes each version's
`.dSYM`. To symbolicate a frame like `my-browser +0x1a2b3c`:
`atos -o my-browser.dSYM/Contents/Resources/DWARF/* -arch arm64 -l 0x100000000 0x1001a2b3c`
(load address 0x100000000 plus the offset). Test:
`a_crash_is_logged_and_offered_for_reporting_once` (bench `crash` panics on
purpose; the hook then exits with 101 instead of aborting, `crash::REQUESTED`,
so test runs leave no macOS crash report or "quit unexpectedly" dialog).

### Default browser and links from other apps

The bundle's Info.plist claims `http`/`https` links (`CFBundleURLTypes`) and
HTML files (`CFBundleDocumentTypes`, rank Alternate). macOS delivers both to
`application:openURLs:` (`App::open_external`): only `http`, `https` and
`file` open, each in a new tab, and links that arrive before `ready` wait in
`pending_urls` so nothing loads before the blockers. Local files load with
`loadFileURL:allowingReadAccessToURL:` scoped to the file's folder.
Settings → General → "Make default" calls
`NSWorkspace setDefaultApplicationAtURL:toOpenURLsWithScheme:` for `http`;
macOS asks the user to confirm and sets `https` with it. Only the `.app` can
be the default (an unbundled binary has no Info.plist), so the bridge's
`default.get` reports `bundled`. Test: `links_and_files_from_other_apps_open_in_new_tabs`
(bench `openurl` runs the same path).

## Tabs

Each tab is an `NSWindow` in a native tab group (`tabbingMode = preferred`).
macOS draws the tab bar, handles dragging a tab out into a new window, and
provides the "+" button (which calls `newWindowForTab:` on the app delegate).

A tab has two states:

| State | Web view | Page process | What's kept |
|---|---|---|---|
| Awake | yes | yes | everything |
| Asleep | none | released | URL and title (shown in the toolbar and tab) |

- `Tab::sleep` saves the URL and title, removes the KVO observers, then drops
  the web view. The order matters: KVO on a freed object crashes.
- `Tab::wake` creates a new web view from `App::make_config()` and loads the
  saved URL. Waking happens whenever a tab becomes the active tab.
- The active tab never sleeps. The policies below leave alone tabs for
  which `Tab::keep_awake` holds: pinned, playing sound (which covers
  picture in picture), camera or microphone in use (`cameraCaptureState`,
  `microphoneCaptureState`), a sheet open (`Tab.alert`), or on screen in
  another window (occlusion state). Memory pressure still sleeps them.
- `App::sleep_background_tabs` runs from one 30-second timer (with 10 s
  tolerance, so macOS can batch the wakeup). Under
  `kern.memorystatus_vm_pressure_level` pressure it sleeps every background
  tab. Otherwise `to_sleep` (a pure function, unit-tested) picks from the
  awake, sleepable background tabs, most recently used first
  (`App::warm_tabs`, by `last_active`):
  - idle longer than `tab_sleep_minutes` (default 5);
  - beyond the first `awake_tabs_max` (default 4, 0 = no limit): the warm
    set;
  - heavy: the footprint sampled by the previous tick's `check_heavy` is
    `HEAVY_MB` (256 MB) or more and the tab has been in the background
    `HEAVY_AFTER` (3 min), unless it's the first in the list. Off when
    `tab_sleep_minutes` is 0 ("never" means only the cap and pressure).

  In Low Power Mode, or when `NSProcessInfo.thermalState` is serious or
  critical, the timeout drops to one minute (`sleep_after`; 0 still means
  never) and the cap to two (`App::awake_cap`). The same tick saves the
  session. Nothing else runs on a timer: switching tabs doesn't trim the
  warm set at once, the next tick does (≤ 40 s).
- **Background links open asleep when the warm set is full**
  (`App::open_link_in_background`, for ⌘-click and Open Link in New Tab):
  with `awake_tabs_max` awake sleepable background tabs already, the new tab
  is created asleep (no web view, like a restored tab) and loads when
  clicked. Opening 20 links costs 4 page processes, not 20. Popups
  (`open_tab_with_config`, opener and `window.open` need the live web view)
  and extension tabs (`tabs.create` callers may script the tab right away)
  always load. Waking reloads the URL; mute is a tab ivar and the zoom is per
  site, so both come back (`tabs_mute_and_show_when_they_play_sound`,
  `zoom_is_remembered_per_site`).
- Bench: `tick` runs the sleep policies now (without pressure) and prints
  `ASLEEP <0|1> …` in tab-bar order. Test:
  `background_tabs_beyond_the_cap_sleep_and_links_beyond_it_open_asleep`.

**What tabs cost** (`src/usage.rs`): each awake tab's page process id
comes from the private `_webProcessIdentifier` (`TUNE web_process_id`), and
its memory (`phys_footprint`, Activity Monitor's "Memory"), CPU time and
wakeups from `proc_pid_rusage`. WebKit's other helpers are found the way the
benchmark finds them (started after us, named `com.apple.WebKit…`, charged to
us by `responsibility_get_pid_responsible_for_pid`). Settings → Tabs
(`tabs.list`, `tabs.show`, `tabs.sleep`, `tabs.close`; ids are the tab's
pointer, checked against the open tabs) polls every 2 s while shown and
computes CPU % from the difference; private tabs are listed without address
or title. On the 30 s tick, `Tab::check_heavy` samples each awake background
tab: 500 MB or more, or 10% CPU or more since the previous tick
(`usage::heavy`), puts a gauge icon in the tab (`NSWindowTab.accessoryView`)
with the reason as tooltip. Bench: `heavy` runs the check now and prints
`HEAVY <0|1> …` in tab-bar order. Test:
`tabs_list_their_memory_and_busy_background_tabs_are_marked_heavy`.

**The active tab** is tracked explicitly (`App.active`), not taken from
"key window". Menu actions go to the app delegate, which forwards them to the
active tab. This keeps shortcuts working when the app isn't frontmost; it also
makes them testable.

Tab commands, all native and costing nothing until used: Duplicate Tab,
Close Other Tabs (⌥⌘W), and the tab-group actions AppKit only adds to its own
menus (Show All Tabs ⇧⌘\, Move Tab to New Window, Merge All Windows), sent to
the active tab's window. Private and normal windows have different tabbing
identifiers, so they never merge and tabs can't be dragged between them.
Tabs moved to another window (by menu or by dragging) are reported to
extensions with `didMoveTab:fromIndex:inWindow:` (`App::sync_ext_windows`,
on each activation), and a window left empty with `didCloseWindow:`. 

**Pinned tabs** (Window → Pin Tab): AppKit's tab bar can't pin, so a
pinned tab is an ordinary tab with three rules: it moves to the front of its
window, after the tabs pinned before it (`NSWindowTabGroup` remove + insert
at index; `addTabbedWindow:ordered:` reordered the window list but not the
tab bar), it never sleeps on the idle timer (memory pressure still sleeps
it), and Close Other Tabs leaves it. A pin icon sits in its tab. The session
keeps it (`session.pinned`, added in 0.3); restored pinned tabs come back
asleep like the rest.

**Sound**: Window → Mute Tab, or a click on the speaker icon in the tab,
sets WebKit's page mute (private `_setPageMuted:` with `_WKMediaAudioMuted`;
`TUNE tab_mute`). The tab remembers it and sets it again on every new web
view, so it survives sleep. A tab playing sound shows a speaker icon,
from KVO on the private `_isPlayingAudio` (`TUNE playing_audio`; observed
only where WebKit has it). The tab icons are `NSWindowTab.accessoryView`: an
`NSStackView` of borderless buttons (pin, sound, heavy), rebuilt when one
changes; re-inserting a tab drops it, so it's set again on the next turn.
Bench: `tabflags` → `TABFLAGS pinned= muted= page_muted= audio=` (page_muted
read back from `_mediaMutedState`). The Window menu's titles follow the
active tab (`App::update_window_menu`, its `menuNeedsUpdate:`). Tests:
`pinned_tabs_go_first_survive_close_others_and_restarts`,
`tabs_mute_and_show_when_they_play_sound`.

**The launcher** (Window → Search Tabs and More…, ⇧⌘A, `src/tabsearch.rs`):
one search field over four kinds of rows, each with an SF Symbol:
- **open tabs** of the active tab's kind only (private windows find private
  tabs, normal windows never do), most recently used first;
- **commands**: the menu bar walked when the launcher opens (submenus too,
  each menu brought up to date as when it opens: Pin/Unpin, Translate/Show
  Original, validation), keeping the enabled items that target the app
  delegate, so text editing, the window list and the Bookmarks/History page
  items are left out. Each shows its menu and its current shortcut (a
  changed one included). Choosing one is `performActionForItemAtIndex:` on
  its menu, a click: no other path to any action;
- **bookmarks** (`Store::bookmarks`) and **history** (the page rows of
  `Store::suggestions`, most visited first; not in private windows), queried
  per keystroke with the query's longest word, at most 8 each, an address
  listed once (a tab beats a bookmark beats history).

Every word of the query must appear in the title or address/menu
(`tabsearch::rank`: title start, title, address start, address). Tabs come
first; the rest by rank, then commands, bookmarks, history; at most 8
commands. An empty query lists the tabs. ↑/↓ move, Tab/⇧Tab go round
(`tabsearch::cycle`), Return or double-click opens (a tab is selected, a page
loads in the tab as from the History menu), Esc or a click elsewhere closes.
Page titles and addresses are shown as plain one-line strings (control
characters removed, 300 characters at most), never markup.

It's a titled `NSPanel` with a hidden title bar (rounded corners, shadow,
keyboard focus come with it) over an `NSVisualEffectView`, a 22 pt borderless
field and a cell-based table (inset style; the intercell spacing centers the
text), added as a **child window** of the tab's window, not a sheet: ending a
sheet made the window key and the app active even in `--on-top` mode. With
`--on-top` it's ordered in but never made key. Built when asked for; a
thread-local holds the open one, and it's freed with its panel when it
closes. Bench: `tabsearch <query>` → `TABSEARCH <title>|…` (the tabs),
`launcher <query>` → `LAUNCHER <kind>:<title> [shortcut]|…` (every row), both
answered with `dialog ok|cancel`. Tests: `search_tabs_finds_and_switches_to_a_tab`,
`launcher_finds_bookmarks_history_and_commands_and_keeps_private_tabs_apart`.

**Session restore** saves every window (`session.win`, a column added in
0.2) in tab-bar order, every 30 s and at quit; at launch each window comes
back as a window, asleep, and the previously active tab wakes. Tests:
`tabs_duplicate_close_others_move_and_merge`, `every_window_comes_back_after_a_restart`.

## Web view configuration

Every web view gets a configuration from `App::make_config()`. All tabs of a
profile share:

| Shared object | Purpose |
|---|---|
| `WKWebsiteDataStore` | Cookies, cache, storage. `dataStoreForIdentifier(uuid)` for a profile, `nonPersistentDataStore` for throwaway. |
| `WKUserContentController` | Holds the tracker and local-network rule lists and the bridge message handler. Shared, so toggling blocking applies to every tab. |
| `WKWebpagePreferences` (default) | HTTPS first, Global Privacy Control, fingerprinting protection (`privacy::harden_webpage_preferences`); turned off per navigation on Shields-down sites |
| `applicationNameForUserAgent` | `Version/<Safari's version> Safari/605.1.15`, read from `/Applications/Safari.app` so the browser blends in with Safari |
| `WKPreferences` | Page cache off, ad attribution off, safe browsing per setting |
| `WKProcessPool` (private config) | Prewarm off, JIT per setting |
| `WKWebExtensionController` | Extensions |
| URL scheme handler | `mybrowser:` pages |

Media requires a click to play (`mediaTypesRequiringUserActionForPlayback =
all`).

### Engine tuning (private WebKit API)

`engine_tuning()` in `src/main.rs` applies WebKit settings that Apple doesn't
publish. Rules for these settings:

- **Check before calling.** Always use `respondsToSelector:` first; a missing
  setting is skipped, never a crash.
- **Report everything.** Each setting appears in the bench `TUNE name=value
  ok|missing` lines. The e2e test `engine_and_privacy_settings_are_applied`
  fails if any is missing, so a macOS update that removes one is noticed.
- **Keep only measured wins.** Defaults are kept only with benchmark evidence;
  the numbers are in `previous-iterations/0.md`.
- **`MYB_TUNE=name=0|1,...`** overrides defaults for experiments, without
  rebuilding.

## Navigation policy (`Tab::action_policy`, `decide_action`)

- `mybrowser:` URLs load only in the main frame, and only when the browser
  itself started the load (typed, menu, bench: `allow_internal` holds that
  exact address until it commits or fails) or a `mybrowser:` page navigates
  within itself. Websites and extensions can't navigate to, open or frame an
  internal page; the responses also carry `frame-ancestors 'none'` and
  `X-Frame-Options: DENY`.
- Main-frame GET loads lose known tracking parameters (`fbclid`, `gclid`, …;
  `privacy::strip_tracking_params`) unless Shields are down for the site.
- Leaving a site turns off extensions that were clicked on for it, before the
  next page can load their scripts.
- On a Shields-down site, the navigation's `WKWebpagePreferences` lose the
  fingerprinting protection.
- On a site with JavaScript turned off, the navigation's
  `WKWebpagePreferences.allowsContentJavaScript` is false: neither the page
  nor its frames run scripts. The browser's own scripts (reader mode,
  passwords, find) live in other script worlds and still work.
- `<a download>` and `shouldPerformDownload` become downloads; so does a
  response WebKit can't show, or one with `Content-Disposition: attachment`.
  A page's first download goes ahead, more need the site's "downloads"
  permission (asked once, remembered).
- Cmd-click on a link opens it in a new tab.
- `target=_blank` and `window.open` go through
  `createWebViewWithConfiguration`. A new tab is created from **the
  configuration WebKit passes in** (WebKit requires this).
- A failed load (not a cancel) shows an error page through the private
  `_loadAlternateHTMLString:baseURL:forUnreachableURL:`, so the address bar
  and Reload keep the failed address. Everything on it is HTML-escaped and it
  runs no script. Certificate errors get their own wording and no bypass.
- If the page process dies (crash or memory kill), the tab reloads, at most
  twice within 30 s; a third crash shows an error page instead of looping.

Every web view allows WebKit's back/forward swipe
(`allowsBackForwardNavigationGestures`): two fingers left or right on the
trackpad, with the page sliding as in Safari.

### Zoom and gestures

Zoom is per site: ⌘+/⌘- step `pageZoom` through Chrome's levels
(`tab::ZOOM_STEPS`: 25% … 90%, 100%, 110%, 125% … 500%) and save it in the
`zoom` table (site → level; back at the default zoom deletes the row;
private windows read but never write). Sites without a row get the
`default_zoom` setting. Every main-frame commit sets the committed site's
level (`did_commit_navigation`), so it applies in any tab and doesn't follow
you to the next site. While a page isn't at the default zoom, a "125%"
button sits left of the star (`refresh_zoom_button`, rebuilt only when the
percent changes); clicking it is ⌘0, which also undoes a pinch.

Gestures (Settings → General, each a setting): pinch and two-finger
double-tap smart zoom are WebKit's magnification (`allowsMagnification`),
the swipe is `allowsBackForwardNavigationGestures`; both are set on every
new web view and on open ones when changed (`App::apply_gestures`). ⌘ +
scroll is a local event monitor on scroll-wheel events
(`App::watch_scroll_zoom`): with ⌘ held it takes the event and steps the
zoom of the tab under the pointer, a trackpad's small deltas adding up to
40 points per step; without ⌘ it returns the event untouched. Bench: `zoom`
→ `ZOOM <level> button=<label>`, `tabflags` shows `swipe=` and `pinch=`.
Test: `zoom_is_remembered_per_site`.

### Address bar

What's typed becomes an address in `App::normalize`, in order: a path
(`/…`, `~`, `~/…`: a folder page or a `file://` address, see "Local
files"), a search keyword (`keyword_url`: `!kw query` or `query !kw` with a
built-in bang (`BANGS`) the user turned on (setting `bangs`, comma-separated,
empty by default) or one of the user's `keywords` rows, or `kw query` with
one of the user's), an address (`https://`, but `http://` for local
hosts, `privacy::is_local_host`), else a search. Keywords are checked in
Settings → General (`keywords.list/set/delete`; one short word, an http(s)
address with `%s`).

**Local addresses** (localhost, `*.localhost`, `*.local`, 127/8, 10/8,
172.16/12, 192.168/16, 169.254/16, `[::1]`, `[fc…]`, `[fd…]`, `[fe80…]`)
skip HTTPS-first: `decide_action` sets the navigation's
`preferredHTTPSNavigationPolicy` to keep-as-requested (`privacy::keep_scheme`),
and the site button shows a computer ("Local address") instead of the
not-secure warning. Test: `local_addresses_load_over_http_as_typed`.

The bar shows what the user asked for while it loads, otherwise the
**committed** address (`_committedURL`, private; `_unreachableURL` on error
pages). `WKWebView.URL` switches to a page-started navigation's target before
it commits, so with it a page could send itself to a slow bank address and
keep showing its own fake login under that address (tested:
`address_bar_cannot_be_spoofed_by_a_slow_navigation`). A new popup with
nothing committed shows an empty bar, since its opener can still write into it.

The site button at the left of the bar shows a lock (HTTPS), a warning (plain HTTP)
or an info icon, and opens the site menu: Shields, local network access,
JavaScript, camera, microphone, location and multiple downloads for the site, and a link
to all site settings.

**Toolbar look** (`Tab::build_window`, `set_extension_buttons`): the title
bar is transparent (`titlebarAppearsTransparent`), so title bar, tab bar and
the toolbar row read as one surface. The row (40 pt) holds back, forward,
reload/stop and home, then the **pill**, then extension, blocked-request and
Web Store buttons at the right; plain frames and autoresizing, no Auto
Layout. Buttons are `NSBezelStyle::Toolbar` with
`showsBorderOnlyWhileMouseInside`: flat, with AppKit's own hover and press
look (its tracking areas, nothing of ours). SF Symbols get a
`symbolConfiguration` (14 pt medium in the row, 12 pt in the pill and find
bar); images stay plain, so `STAR` still reads the accessibility label.
The pill is a `Fill` view (a layer's background color and continuous
corner radius set in `updateLayer`: no backing store, redrawn by AppKit on
light/dark changes) with `quaternarySystemFillColor`, or a dark purple in
private windows (which also set the pill's appearance to dark so text and
icons are light in either theme). Inside it: the site button, the address
field (`AddressField`, a borderless `NSTextField` subclass whose
`focusRingMaskBounds`/`drawFocusRingMask` put the system focus ring around
the whole pill), the zoom button and the star; `set_extension_buttons`
sizes the pill and the field. The suggestion list opens under the pill, as
wide as it. A 1 pt `separatorColor` hairline sits under the row (and under
the find bar).

**Loading line**: a 2 pt `Fill` in `controlAccentColor` over that hairline,
`estimatedProgress` wide (at least 10%), shown only while `isLoading`.
`estimatedProgress` is one of the observed keys (`OBSERVED`, removed with
the others before a web view is dropped); its changes only move the line
(`Tab::refresh_progress`), not the whole toolbar. No timer and no
animation: idle, it's a hidden view. Bench: `progress` → `PROGRESS
shown=<0|1> width=<percent>`; checked in
`address_bar_cannot_be_spoofed_by_a_slow_navigation`.

**Suggestions** (`src/suggest.rs`): while the bar is edited
(`controlTextDidChange:`), `Store::suggestions` matches the text against past
searches (`searches` table: text, last time; those starting with it first,
then newest) and visited pages (`history`, grouped by address, most visited
first; search engines' result pages left out as they repeat a search), at
most 8. They show in a borderless non-activating `NSPanel` (a child window,
so typing stays in the bar) with a cell-based table. ↑/↓ choose, Return
opens the choice (else what was typed), Esc or leaving the bar closes it,
a click opens a row. Return on text that isn't an address (`is_address`)
and the new tab page's search save the text, except in private windows or
with the `address_suggestions` setting off. Nothing is sent to a search
engine while typing. Clearing history clears searches; Settings → Storage
shows where both are kept (the profile's `browser.db`) and clears searches
alone (`searches.clear`). Bench: `bartype <text>` / `barkey up|down|enter|esc`
→ `SUGGEST [*]search:<text>|[*]page:<url>|…` (`*` = chosen), `savesearch
<text>` saves one without loading a search engine. Test:
`address_bar_suggests_past_searches_and_visited_pages`.

### JavaScript dialogs and prompts

`alert`/`confirm`/`prompt` are sheets on the tab. From a page's second dialog
on, the sheet has a "Don't allow more dialogs from this page" checkbox; after
that, dialogs return at once (cancel). Without it an `alert()` loop traps the
tab, since a window can't close while a sheet is up. The count resets when a
new page commits.

## Site permissions

`permissions` table: (site = lowercase host, kind, value). A missing row is
the default.

| Kind | Values | Default | Used by |
|---|---|---|---|
| `camera`, `microphone` | allow, deny | ask | `webView:requestMediaCapturePermissionForOrigin:…` |
| `screen` | allow, deny | ask | `_webView:requestDisplayCapturePermissionForOrigin:…` (private); then macOS's picker chooses what to share |
| `location` | allow, deny | ask | `webView:requestGeolocationPermissionForOrigin:…` (implemented by selector; newer than our bindings) |
| `downloads` | allow, deny | ask from the 2nd download per page | `Tab::allow_download` |
| `local_network` | allow | blocked | exemptions in the local-network rule list |
| `shields` | off | on | exemptions in the tracker rule list; no fingerprinting protection or parameter stripping |
| `javascript` | deny | allowed | `allowsContentJavaScript` on the site's main-frame navigations (`Tab::decide_action`); toggling reloads the tab |

A prompt is a sheet "<site> wants to …" with Allow / Don't Allow and a
"Remember for this site" checkbox (on; absent in private windows). Web
notifications are denied, by choice: WebKit's default without a provider,
and the owner doesn't want them. Settings → Site
settings lists every site with a saved choice and changes or forgets it
(`perm.list`, `perm.set`, `perm.forget`). Changing Shields or local network
access recompiles the affected rule list (the tracker list takes ~1 s, the
local list is tiny) and reloads the tab.

## Profiles and storage

```
~/Library/Application Support/my-browser/     (or --profile DIR)
  browser.db      settings, history, searches, zoom, downloads, extensions, session, profile_id (UUID)
  lock            held for the app's lifetime: one browser per profile
  extensions/     copies of installed extensions
~/Library/WebKit/<app id>/WebsiteDataStore/<profile_id>/   cookies, cache, site storage (WebKit's)
```

- `<app id>` is the executable name (`my-browser`) for `cargo run` and the
  bundle id (`local.my-browser`) for the `.app`. That's why they have separate
  logins.
- SQLite runs in WAL mode with `synchronous=NORMAL`: a crash can lose the last
  moments, but never corrupts the file.
- Settings the page can change are listed in `store::SETTINGS`;
  `Store::set_setting` validates every key and value.
- Throwaway profile (`--bench` without `--profile`): in-memory SQLite plus
  WebKit's non-persistent store. Nothing touches disk, and WebKit treats it as
  private browsing, so extensions don't run there.

## Privacy layer (`src/privacy.rs`)

| Protection | How |
|---|---|
| Tracker blocking | `WKContentRuleList` compiled from `resources/trackers.bin`. The rules block known tracker domains when loaded as third parties, except on sites owned by the same company (Disconnect's entity list), so Messenger can still load Facebook's CDN. EasyList's ad-server domains follow as plain third-party blocks, and its pop-up servers as `popup` blocks; domains Disconnect covers are skipped so its owner exemptions hold. Shields-down sites are appended as `ignore-previous-rules` rules (`if-top-url`). Cached by WebKit under a hash of the file and the exemptions; stale compiled lists are deleted. |
| YouTube ads | `resources/youtube.js`, a page-world user script added and removed with `block_trackers` in `App::apply_trackers`, with the Shields-down sites written into it. It traps `ytInitialPlayerResponse` and wraps `JSON.parse` and `Response.json` to delete `adPlacements`, `playerAds` and `adSlots`. Swapped with the private `_removeUserScript:` (`TUNE youtube_ads`); `removeAllUserScripts` would also drop extensions' content scripts. |
| Cookie banners | A third shared `WKContentRuleList` (`privacy::banner_rules`): `css-display-none` rules for the banners of common consent platforms (`BANNERS`, specific ids and classes only). Nothing is clicked or answered, so no consent is given. Setting `hide_cookie_banners` (on); Shields-down sites are exempt the same way. A banner that also locks scrolling stays hidden but leaves the page locked: content blockers can only hide. |
| My filters | A fourth shared list (`privacy::filter_rules`), from the `filters` settings row: `##sel`, `a.com,b.com##sel` (`if-domain`, subdomains too), `~a.com##sel` (`unless-domain`) as `css-display-none`; `||host^` (`$third-party`) as a block of the host and its subdomains. Other syntax is listed as not understood. Never empty (one rule that matches nothing). Shields-down sites exempt. A list WebKit refuses isn't saved (`App::set_filters` puts the old text back). |
| Local network shield | A second rule list: requests to loopback, private IPv4, link-local (incl. 169.254.169.254), IPv6 loopback/ULA/link-local and `.local` names are blocked unless the top page is itself local. Per-site exemptions as above. A separate list, because `ignore-previous-rules` only reaches rules in its own list. DNS names that resolve to private addresses aren't covered. |
| HTTPS first | `WKWebpagePreferences.preferredHTTPSNavigationPolicy = automaticFallbackToHTTP` (public, macOS 15.2) |
| Global Privacy Control | `setGlobalPrivacyControlEnabled:` (public, macOS 27): `Sec-GPC: 1` and `navigator.globalPrivacyControl` |
| Fingerprinting protection | `_setNetworkConnectionIntegrityPolicy:` with Enabled, EnhancedTelemetry (= WebCore `FingerprintingProtections`) and SanitizeLookalikeCharacters (= `LinkDecorationFiltering`): Safari's Advanced Tracking and Fingerprinting Protection. WebKit's link filtering acts on the page being left. |
| Tracking parameters | Our own list (Brave's defaults) stripped in `action_policy` |
| Intelligent Tracking Prevention | `_setResourceLoadStatisticsEnabled:` on the data store |
| Third-party cookies | Blocked everywhere (`_setThirdPartyCookieBlockingMode:…`), Safari's policy |
| Ad-click attribution | Off (`_setPrivateClickMeasurementEnabled:`) |
| User agent | Safari's own suffix, so the browser isn't a rare user agent |
| Safe browsing | Off unless the user turns it on (it sends URL hash prefixes to Apple/Google) |
| Telemetry | None. The browser makes no network requests of its own; the network gate connects only to addresses the user allowed. |

WebKit itself already: limits WebRTC to mDNS host candidates unless the page
has camera/microphone access, blocks active mixed content, gives cross-site
`target=_blank` links `noopener`, and sandboxes page processes.

The rules file is raw DEFLATE with a length header. It's inflated with
macOS's `libcompression` (`compression_decode_buffer`, `COMPRESSION_ZLIB`), so
there's no decompression dependency. To refresh it: `python3
scripts/update-trackers.py`.

## Internal pages and the bridge (`src/internal.rs`)

`mybrowser://settings` is served from `resources/settings.html` by the URL
scheme handler, with `frame-ancestors 'none'`, `X-Frame-Options: DENY`,
`nosniff` and `no-store` headers. Its Content-Security-Policy allows only the
page's own inline code, nothing from the network. The page calls
`webkit.messageHandlers.browser.postMessage([op, ...args])` and gets a JSON
string back.

The handler is registered for every page, so **every message is checked**
(`internal::is_trusted`):
- it must come from the top-level frame,
- the frame's origin must be `mybrowser:`,
- the web view's URL must be `mybrowser://settings`.

Arguments must be an array of strings. Each op validates its input, and
`settings.set` goes through `Store::set_setting`.

The page inserts all data from websites (titles, URLs, file names) with
`textContent`, never as HTML.

**Look.** All internal pages use one set of CSS tokens (`tab::PAGE_CSS`,
repeated at the top of settings.html): the system font, macOS-like grays,
`AccentColor` (with a blue fallback) and `prefers-color-scheme`, which
follows the theme setting through `NSApp.appearance`. In settings.html,
consecutive `.row`s form one rounded card by CSS alone (`:has`), so a new
row needs no wrapper. The `.row` and `.list li` checkboxes are drawn as
switches but stay real checkboxes. Icons are inline SVG paths; nothing is
fetched (the CSP allows only inline code and `data:` images).

Bridge ops: `settings.get/set`, `keywords.list/set/delete`, `filters.get/set`,
`autofill.list/save/delete`, `profiles.list/create/open/delete`,
`sync.export/import`, `history.list/delete/clear`,
`downloads.list/open/reveal/cancel/remove/clear`,
`ext.list/install/toggle/access/remove`, `perm.list/set/forget`,
`net.list/allow/deny/forget`, `data.clear`
(cookies and site data), `storage.get`, `storage.clear_cache`, `legacy.clear`,
`open` (http(s) only), `app.uninstall` (also asks natively).

### New tab page (`src/home.rs`)

New tabs and windows show a native AppKit page unless the user set a
homepage: no web view, so no page process. One centered column (plain frames
and autoresizing, kept centered as the window resizes) on a background view
whose layer is a radial `CAGradientLayer` (`makeBackingLayer`; colors set in
`viewDidMoveToWindow` and `viewDidChangeEffectiveAppearance`, because a
gradient layer never gets `updateLayer`). Top to bottom, each switchable in
Settings:

- the banner (`home_art`): each █ of the art a rectangle in a custom view,
  its ink sweeping blue to violet, over a faint offset echo;
- a search pill (`home_search`, address-bar rules): a borderless field and a
  magnifier symbol on a `Plate`, a view whose fill, border and corner are
  layer properties set in `updateLayer`;
- up to 8 most visited sites (`home_top_sites`; `Store::top_sites`: scheme +
  host, counted over the latest 5000 visits) as `Tile`s: a button whose
  square is a gradient sublayer, with two small labels for the initial and
  the host. The color comes from the site's name (`site_name`: the label
  before the public suffix, `en.wikipedia.org` → wikipedia) hashed into 10
  hand-picked hues. No favicons: fetching them would be a network request of
  the browser's own. The address is the tooltip, which `homeSite:` opens;
- a status line (`home_stats`): open tabs and how many are asleep, the
  footprint of the app plus its WebKit helpers (`usage`, read once when the
  page is built, ~1-2 ms), and the shields settings. One attributed label on
  a plate; nothing is sampled afterwards;
- Customize, which opens Settings → General.

Memory is in layer bitmaps (footprint's "CoreAnimation" category), one per
view that draws, about 35 KB plus its area. So only the banner and the
labels draw; everything else is layer properties, composited without a
bitmap. Measured with `footprint` on the app (8 sites, light): the old page
2640 KB, this one 3248 KB (+0.6 MB: the tiles' 16 labels vs the old 8
buttons, and the status line ~130 KB). Tried and dropped: tiles drawn in
`drawRect` (+4.7 MB), one two-line label per tile (more than two small
ones), three cards of three labels each (~420 KB vs the line's ~130 KB).
No Auto Layout, stack views or second font: measured, those cost ~2 MB more.
Loading a page removes the view.

### Theme

The `theme` setting (system | light | dark) sets `NSApp.appearance`. Web views
inherit it, so the internal pages and any site using `prefers-color-scheme`
follow it.

### Address bar

The bar has no target/action: its delegate handles Return in
`control:textView:doCommandBySelector:` and reports it handled, so AppKit
doesn't beep. Return on the address already showing just gives focus back
to the page (⌘R reloads), as the owner asked.

### Keyboard shortcuts

Every shortcut is a menu item (`build_menu`). Settings → Shortcuts lists
them from the live menu bar (`keys.list`, `src/shortcuts.rs`) and changes
them: click a shortcut, press the new keys (named by `e.code`, so ⌥ doesn't
change the letter), Esc cancels, Delete removes. `keys.set <id> <keys>`
validates (one key with ⌘, ⌥ or ⌃; refused if another item has it),
stores `shortcut:<id>` in the profile's settings and sets the item's key
equivalent at once; `keys.reset <id>` goes back to the built-in keys,
remembered at launch. Ids are action and tag (`newTab:#0`), with `.2` for a
second item with the same action, so they survive title changes (Pin/Unpin).
Saved changes are applied at launch (`shortcuts::apply_saved`). macOS's App
Shortcuts (System Settings → Keyboard, `keys.open`) still work on top. Test:
`shortcuts_can_be_changed_reset_and_are_kept`. About my-browser (app menu) opens Settings → About.

### Updates

There is no update check in the background (CONTRIBUTING rule 4).
"my-browser → Check for Updates…" and Settings → About → "Check for
updates" (`app.updates`) open `RELEASES`
(`github.com/shu-vro/my-browser/releases/latest`) in a normal tab, where
GitHub shows the newest release; Settings shows this build's version
(`app.version`) beside the button. Releases come from the `release` branch
(`.github/workflows/release.yml`), which only the owner can push to.

### Uninstall

Settings > Storage lists every path `internal::uninstall_paths` would delete
(profile, WebKit data and caches for both the bundled and unbundled identity,
preferences, saved window state, old leftovers, and the running `.app`).
`app.uninstall` clears site data through WebKit first, so its helper
processes don't write it back, then deletes the paths and exits without saving
the session. It is refused unless the browser runs on the main profile:
`--bench` and `--profile` sessions can never wipe the owner's data.

### Site data

`src/sitedata.rs` reads WebKit's own records of what sites keep
(`fetchDataRecordsOfTypes:` over all types; a record is a site, usually its
registrable domain, and its data types, named for people: Cookies, Local
storage, Databases, Cache…) and deletes one site's
(`removeDataOfTypes:forDataRecords:`). The site menu's **Site Data…** shows
the records that belong to the page's host (`sitedata::belongs`: the same
host, a parent domain or a subdomain) in a sheet, each with a trash button;
deleting shows the sheet again. **Manage All Site Data…** opens Settings →
Site data (`sitedata.list`, `sitedata.delete <name>`), a searchable list of
every site. Private tabs show the private windows' store. Bench: `sitedata`
→ `SITEDATA <name>:<kinds>|…`. Test:
`site_data_is_listed_per_site_and_deleted_one_site_at_a_time`.

### Storage and leftover cleanup

The Storage section shows what the browser keeps on disk, and clears it:
- **Cookies and site data:** the profile's WebKit store, sized from its
  folder.
- **Cache:** disk, memory, fetch and offline caches only, so logins stay.
- **History** and the **downloads list** (the files are kept).

It also deletes **leftovers from older builds** (`internal::leftovers`):
- WebKit's shared default store, from before profiles had their own,
  under `WebKit/my-browser/WebsiteData`, `Caches/my-browser/WebKit` and
  `HTTPStorages/my-browser.binarycookies*`,
- the `dev.local.my-browser` test bundle's data.

The list is fixed in code and resolved under `$HOME`. Nothing from the page
chooses a path, and current profiles' stores
(`WebKit/<app>/WebsiteDataStore/<id>`) are never in it. Sizes don't follow
symlinks.

## Passwords (`src/passwords.rs`, `resources/passwords.js`)

Logins are internet passwords in the login keychain (`SecItemAdd`,
`SecItemCopyMatching`, `SecItemUpdate`, `SecItemDelete`), marked with the
creator code `mybr` so the browser lists only its own. Throwaway profiles use
`Vault::Memory` and never touch the keychain (the keychain itself is covered
by an opt-in test, `cargo test --release -- --ignored keychain`).

`passwords.js` is a user script at document end in every frame (sign-in
forms are often embedded), in its own `WKContentWorld` ("passwords"), with a message handler that exists
only there, so pages can't post to it or see it. It reports:
- `focus` on a username or password field (once per field): the browser
  shows a native menu of the accounts saved for the site under the field
  (`Tab::offer_passwords`); picking one reads it from the vault and calls
  `__mybFill` in that world with the values as arguments
  (`callAsyncJavaScript`, no string building), after checking the page is
  still on that site. A frame is its own site: its form gets the accounts of
  the frame's origin, not the page's, and `__mybFill` itself checks the
  frame is still on that site (it may have navigated). A frame's menu opens
  at the pointer, since where a cross-site frame sits in the page is unknown.
- `submit` (form submit, Enter, or a button click next to a filled password
  field): the browser offers to save unless the tab is private, the site is
  set to never (`passwords` permission = deny), or the same password is saved
  already. "Never for this site" sets that permission.

Before filling or copying, `passwords::confirm_owner` asks for Touch ID or
the Mac's password (`LAContext`, policy `DeviceOwnerAuthentication`), unless
the `password_auth` setting is off. Bench mode skips it, and a Mac that
can't evaluate the policy fills as before. Checked by hand only: the prompt
needs a person.

Passwords are kept and filled only on HTTPS sites (and plain HTTP on this
Mac: `localhost`, `127.0.0.1`, `[::1]`), by exact host. Edit → AutoFill
Password (⌥⌘A) offers again for the focused field. Settings → Passwords lists
(site, user) pairs, copies a password straight to the pasteboard (it never
goes to the settings page), deletes, and imports CSV exports from Apple's
Passwords app, Safari, Chrome, Edge, Brave and Firefox (`parse_csv`).
Tests: `passwords_are_offered_saved_and_filled_only_when_picked`,
`private_windows_fill_passwords_but_never_save_them`,
`passwords_work_in_frames_for_the_frames_site`.

## Import from other browsers (`src/import.rs`)

Settings → Import lists the browsers found under `$HOME/Library` (Chrome,
Brave, Edge, Vivaldi, Arc and Chromium by their default profile folder, and
Safari) and imports one kind at a time, only when clicked:
- **Bookmarks**: Chromium's `Bookmarks` JSON (`NSJSONSerialization`) or
  Safari's `Bookmarks.plist` (`NSPropertyListSerialization`), into
  "Imported from <browser>/<its folders>"; a `/` in a folder name becomes `-`.
- **History**: the newest 20,000 visits from Chromium's `History` (µs since
  1601) or Safari's `History.db` (s since 2001), web pages only, with their
  original times. The database and its `-wal` are copied to a temporary
  folder first, since the browser may have them open.
- **Passwords** (Chromium): `Login Data` rows are `v10` + AES-128-CBC with a
  key derived by PBKDF2-HMAC-SHA1 (1003 rounds, "saltysalt") from the
  browser's "Safe Storage" keychain secret. macOS asks the user before
  handing that secret over; CommonCrypto (`CCKeyDerivationPBKDF`, `CCCrypt`)
  does the rest. Safari's passwords are in the Passwords app: its CSV export.
- **A bookmarks file** (`.html`, the Netscape format every browser exports).

Imports run in one transaction each and skip what's already there (same
address in the same folder, same visit second), so importing twice adds
nothing twice. Reading another app's data can need the user's permission
(Full Disk Access for Safari's folder); the error says so. Test:
`bookmarks_and_history_import_from_other_browsers` (a made-up `$HOME` with a
Brave profile and Safari's files).

## Hide Element (the picker)

View → Hide Element on Page… (and the site menu) runs `resources/picker.js`
in the browser's own script world with `callAsyncJavaScript`: it outlines
the element under the pointer and resolves with a selector for the clicked
one (an id, else tag and up to three classes, `:nth-of-type` when needed,
up the tree until the selector matches it alone; generated names with long
numbers are skipped). Clicks and mouse presses don't reach the page while
picking; Esc cancels. A sheet shows the selector (editable); OK adds
`<host>##<selector>` to the filters (`App::add_filter`) and hides the
matches now. Bench: `picker` → `PICKER`, then `HIDDEN <0|1>`. Test:
`elements_hide_with_the_picker_and_your_own_filters`.

## Translate Page

View → Translate Page (⌥⌘T, `Tab::toggle_translate`) runs
`resources/translate.js` in the browser's own script world: it collects the
page's text nodes (letters only; not in `script`, `code`, `pre`, form
fields, `translate=no`, `.notranslate` or editable areas; at most 5,000),
and later puts translations in place keeping each node's surrounding
spaces, or the originals back (Show Original). The texts go, 200 at a
time, to `my-browser-translate`, a Swift helper built by `build.rs` next to
the browser binary (and into `Contents/MacOS` by `scripts/bundle.sh`, signed
first). One JSON line per request on its stdin, one per reply on stdout:
NaturalLanguage detects the page's language, Translation
(`TranslationSession(installedSource:target:)`, macOS 26) translates
on-device. It runs as its own process so Swift and the frameworks never load
into the browser (nothing at launch); `App::stop_idle_translator` closes its
input after a minute without work, on the 30 s tick, and it exits. Replies
are matched by id; a tab's replies are dropped if it went to another page
(`translate_run`). Errors come back with a code: `install` (the languages
aren't downloaded: a sheet offers System Settings → Language & Region),
`same`, `unsupported`, `old` (macOS 15), `unknown`. The target is the Mac's
language, or `translate_to`. `MYB_TRANSLATE_FAKE=1` makes the helper
upper-case instead (tests). Bench: `translate` → `TRANSLATED <from> <n>`,
`TRANSLATE off` or `TRANSLATE err <code>`. Test:
`pages_translate_and_show_the_original_again`.

## Reader mode

View → Reader (⇧⌘R, `Tab::toggle_reader`) runs `resources/reader.js` in the
browser's own script world (the page can't see or change it): the element
whose own paragraphs hold the most text wins (`<article>`/`<main>` get a
head start), then everything but text, headings, lists, quotes, code,
tables, links and images is dropped, with every attribute except `href`,
`src`, `srcset` and `alt`, lazy images get their real address, and
`javascript:` links go. Rust builds the page (`reader_page`: escaped title,
large serif type, light/dark) with a policy of `default-src 'none'`, so no
script can run even if the cleaning missed one, and loads it at the page's
address so relative links and images resolve. WebKit replaces the page's
history entry with it, so leaving reader mode (again, or Reload) loads the
page fresh. No article (under ~300 characters of paragraphs): nothing
happens. No visit is recorded for the reader page. Nothing runs on any page
until the user asks. Test: `reader_mode_shows_the_article_alone_without_scripts`.

## Media: fullscreen, picture in picture, screen sharing

- **Element fullscreen** (`setElementFullscreenEnabled:`, public) is off in
  a plain WKWebView, so video sites' fullscreen buttons did nothing; it's on.
- **Picture in picture**: `_setAllowsPictureInPictureMediaPlayback:`
  (private) turns on the standard API and the video context menu's item.
  View → Picture in Picture (⌥⌘P, `Tab::picture_in_picture`) uses WebKit's
  `_togglePictureInPicture` when it has a video to toggle, else asks the
  largest playing video in the page through the browser's own script world.
- **Screen sharing**: `_setScreenCaptureEnabled:` (private) and the private
  UI-delegate method above. A hidden page can't share (WebKit refuses with
  InvalidStateError), so the test runs `--on-top`.

**Automatic picture in picture**: when the active tab changes
(`App::set_active`), the tab left behind, if it plays sound (`_isPlayingAudio`)
and `auto_pip` is on, asks its largest playing video for picture in picture
(`Tab::leave_playing`, in the browser's script world, which carries the user
gesture `requestPictureInPicture` needs). Coming back to the tab exits it
(`return_to_playing`). Silent videos are left alone. Tabs playing sound are
never put to sleep by the idle timer (memory pressure still sleeps them).
Bench: `AUTOPIP 1` when it happens. Test:
`a_playing_video_goes_picture_in_picture_when_its_tab_is_left`.

All of these cost nothing until a page uses them. The private ones are `TUNE`
lines (`picture_in_picture`, `screen_capture`), and so are the private
UI-delegate methods (`context_menu`, `page_print`, `display_capture`): WebKit
only calls delegate methods its `WKUIDelegatePrivate` protocol declares, so
`App::ready` checks the protocol still has them. Test:
`video_fullscreen_picture_in_picture_and_screen_sharing`.

## Link context menu

WebKit builds the right-click menu; the private UI-delegate method
`_webView:getContextMenuFromProposedMenu:forElement:userInfo:completionHandler:`
hands it over just before it opens (`Tab::link_menu`). For an http(s) or
file link, WebKit's "Open Link in New Window" item (found by its identifier,
`WKMenuItemIdentifierOpenLinkInNewWindow`) is replaced with "Open Link in
New Tab" (background), "Open Link in New Window" and "Open Link in Private
Window" (left out in private tabs, whose new tabs and windows are private
anyway). The link comes from the element's `hitTestResult.absoluteLinkURL`.
Nothing runs in the page and nothing is built until a right-click. On a
WebKit without the method, WebKit's own menu shows (its new-window item
still opens a tab). The method's type encoding was checked against the
runtime (`v56@0:8@16@24@32@40@?48`); the menu rewrite and the three actions
are tested with a stand-in menu (`link_menu_opens_links_in_tabs_windows_and_private_windows`,
bench `linkmenu`), since a real right-click opens a modal menu.

## Printing

⌘P (`Tab::print`) runs `WKWebView printOperationWithPrintInfo:` as a sheet;
the system print panel's PDF menu saves a PDF. The operation's view gets the
web view's size first, or WebKit prints blank pages. A page's own
`window.print()` arrives through the private
`_webView:printFrame:pdfFirstPageSize:completionHandler:` and opens the same
panel, one at a time (ignored while any sheet is up, or after the user
blocked the page's dialogs). Test: `pages_print_to_pdf_and_window_print_opens_the_panel`
(bench `printpdf <path>` saves straight to a PDF through the same operation).

### Screenshots

File → Take Screenshot (a submenu, also in the page's right-click menu
through `App::add_page_items`; `Tab::screenshot`, screenshot.rs) takes:

- **Select Area…** (⇧⌘S): `AreaPicker`, a flipped view over the page that
  dims all but the dragged rectangle; Esc or a click without dragging
  cancels. The area goes to WebKit's `takeSnapshotWithConfiguration:` as its
  `rect`.
- **Visible Part**: the same snapshot of the whole view.
- **Full Page**: the page as one tall PDF page (`createPDFWithConfiguration:`
  with no rectangle: the whole page, backgrounds included) drawn into a
  bitmap at 2× (`png_of_pdf`), smaller for very long pages (at most 16,384
  pixels tall and 60 megapixels).

Over the PDF viewer, AppKit draws the area (`cacheDisplayInRect:`). The PNG
then goes where setting `screenshot_to` says (`file`: a save panel;
`clipboard`: the general pasteboard and a "Screenshot copied" note that
fades), chosen with the submenu's Save to File / Copy to Clipboard. Record
Screen… opens macOS's Screenshot app. Bench: `screenshot <visible|full|area
[x,y,w,h]> [path]` → `SCREENSHOT <0|1> <w>x<h>` with a path, else `SCREENSHOT
copied=1 …` (a private pasteboard in bench mode), `SCREENSHOT picking` /
`cancelled` for the picker; `screenshotdrag x1,y1,x2,y2` drags on it. Test:
`screenshots_take_the_whole_page_the_visible_part_or_an_area`.

### Dark mode backdrop

In dark mode a new web view doesn't draw its background (private
`_setDrawsBackground:`) and asks for rendering progress
(`_setObservedRenderingProgressEvents:` with first visually non-empty
layout); until then the window's dark background shows instead of a white
flash. When `_webView:renderingProgressDidChange:` arrives, the view draws
its background again, so pages without one get the usual white canvas.
`TUNE theme_backdrop`.

### Saving pages

Save Page As (⌘S) asks WebKit for the page as a Web Archive
(`createWebArchiveDataWithCompletionHandler:`, a binary plist of the page
and its subresources) and writes it where the save panel says. Offline, it
opens like any local file (`loadFileURL`). Bench: `savepage <path>` →
`SAVED <0|1>`. Test: `pages_save_as_web_archives_that_open_offline`.

## Autofill (`src/autofill.rs`)

Addresses and cards live in the `autofill` table (kind, `field\tvalue`
lines). Addresses keep name, organization, street, city, region, postal
code, country, email and phone; cards keep name, expiry, brand, last four
digits and a random `ref`, under which the number is a generic password in
the keychain (service "my-browser card", our creator code; in memory for
throwaway profiles, never listed as a password). Security codes are never
asked for. The settings page adds, edits (an empty number keeps the saved
one) and deletes them (`autofill.list/save/delete`); numbers must pass the
Luhn check and never go back to the page.

`passwords.js` also names address and card fields: by their
`autocomplete` token, else by name, id, placeholder and label (login,
search, coupon and similar fields excluded, and a guessed field counts only
in a form with another). Focus on one reports `autofill` with the kind; the
browser shows the saved entries in a native menu under the field
(`Tab::offer_autofill`). Picking one sends the values by field name
(`autofill::values`: full, given and family name, street lines, `MM/YY`…)
to `__mybAutofill` in that frame, which fills every recognised field of the
form (selects by value or text) and fires `input`/`change`. Cards are
offered and filled only where passwords are (HTTPS, or this Mac), after
Touch ID (`password_auth`). Bench: `AUTOFILL <label>|…`, `fillautofill <i>`.
Test: `addresses_and_cards_fill_forms_when_picked`.

## Profiles (`src/profiles.rs`)

The main profile ("Default") is `~/Library/Application Support/my-browser`;
others are folders in its `profiles/` (names: letters, digits, spaces, `-`,
`_`; up to 40). Each runs as its own copy of the app (`open_profile`
starts the executable with `--profile <folder>`; the profile lock still
allows one copy per profile; "open" in the list = its lock is held). A
named profile's Dock icon carries its name (`dockTile.badgeLabel`) and its
address bar says "Work: search or enter address". File → Profiles lists
them (filled when it opens) and Settings → Profiles adds, opens and deletes
them (`profiles.list/create/open/delete`). Deleting asks natively, refuses
Default, this profile and open ones, then removes WebKit's data store for
the profile id (`removeDataStoreForIdentifier:`), its extension storage and
its folder. Uninstall removes them all with the main folder. Test:
`profiles_are_listed_added_opened_and_deleted` (a made-up `$HOME`).

## Sync by file (`src/syncfile.rs`)

Settings → Sync exports one JSON file (`"format":"my-browser sync"`):
settings (not `downloads_dir`), keywords, filters, changed shortcuts, site
permissions, zoom levels, addresses, bookmarks and history. Passwords and
cards stay in the keychain and aren't in it. Import merges: bookmarks and
visits that are already there are skipped (as for browser imports),
settings, keywords and permissions are set through their validators,
addresses are added unless identical, filter lines are added, then the
blockers are rebuilt and the settings applied. Nothing is uploaded: where
the file goes is the user's choice. Bench: `bridge sync.export\t<path>`,
`sync.import\t<path>`. Test:
`a_sync_file_carries_settings_bookmarks_and_more_to_another_browser`.

## Developer tools

The `devtools` setting (on) sets every web view's `isInspectable` (Safari's
Develop menu can attach) and the shared preferences' private
`_setDeveloperExtrasEnabled:` (`TUNE developer_extras`: Inspect Element in
the page's menu). View → Show Web Inspector (⌥⌘I) opens the page's
inspector (private `_inspector`, `show`; `TUNE web_inspector`). Off, none of
these. Bench: `inspector` → `INSPECTOR <0|1>`. Test:
`developer_tools_can_be_turned_off`.

## Spelling

WebKit checks spelling as you type only if the app says so: `main`
registers the default `WebContinuousSpellCheckingEnabled = YES` (Safari's
behaviour); Edit → Spelling and Grammar (first-responder actions:
`showGuessPanel:`, `checkSpelling:`, `toggleContinuousSpellChecking:`,
`toggleGrammarChecking:`, `toggleAutomaticSpellingCorrection:`) changes it,
and WebKit keeps the choice in the app's defaults. Bench: `spell` → `SPELL
continuous= grammar= correct=` (the menu checkmarks the web view gives).
Test: `spelling_is_checked_while_typing`.

## Private windows

⇧⌘N opens a window whose tabs (`Tab::is_private`) use a non-persistent
`WKWebsiteDataStore` (`App::private_store`, with the same tracking
protections as the profile's). All private windows share it while any is
open; when the last private tab closes it's dropped, and with it every
cookie, cache entry and storage the private session made. Private tabs:
- get no web extension controller and are never reported to extensions
  (`didOpenTab` and the rest are skipped, `ext_window_for` returns None);
- add no history, aren't saved in the session, aren't offered by Reopen
  Closed Tab, and don't list their downloads (the files stay, in the
  downloads folder);
- don't remember permission answers (no "Remember" checkbox);
- open in their own window group (`tabbingIdentifier` "private"), with a dark
  address bar saying "Private". New tabs, ⌘-clicked links and popups from a
  private tab stay private (`App::open_tab_in`); links from other apps,
  extensions and the settings page always open normal tabs.

Test: `private_windows_keep_nothing_and_share_nothing`.

## Bookmarks

A `bookmarks` table in `browser.db` (url, title, `/`-separated folder).
⌘D (`Tab::toggle_bookmark`) or the star right of the address bar adds the
page, or removes its bookmark. The star follows the page in
`refresh_toolbar` (one indexed lookup per toolbar refresh). The Bookmarks
menu keeps two fixed items (so ⌘D and ⌥⌘B always match) and fills the rest in
`menuNeedsUpdate:`, only when it opens: the 10 newest bookmarks, then, with
more than 10, All Bookmarks, where folders become submenus (at most 500
bookmarks per folder). The History menu likewise adds the 10 latest pages
(each address once) after its four fixed items (`fill_history_menu`). Both
open their page through `openMenuURL:` (the address is the item's
represented object); ⌘ while choosing one opens a new tab. Settings →
Bookmarks lists, searches, renames, moves and deletes them through the
`bookmarks.*` bridge ops, which validate titles and folders. Test:
`bookmarks_toggle_with_cmd_d_and_fill_the_menu`.

## PDFs

A main-frame response of `application/pdf` marks the tab (`pdf_expected`).
When WebKit has finished loading it, the tab asks for the file it loaded
(private `_getMainResourceDataWithCompletionHandler:`, `TUNE pdf_data`) and
covers the page with `pdf::Viewer`: PDFKit's `PDFView` and
`PDFThumbnailView` (the framework is loaded on the first PDF) under a
toolbar: thumbnails, previous/next, page number field and count, zoom out/in
with the percent, fit width (`autoScales`), fit page
(`scaleFactorForSizeToFit`), rotate (every page +90°), two pages side by
side (`kPDFDisplayTwoUpContinuous`, as a book), highlight the selection
(yellow `Highlight` annotations, line by line), read aloud
(`NSSpeechSynthesizer`, the selection's text ranges or from this page on;
`spoken` maps the spoken text back to pages, so the tab, as the voice's
delegate, selects each word as it's read: `speechSynthesizer:willSpeakWord:`
→ `Viewer::speaking`, `scrollSelectionToVisible:`), find, print
(`printOperationForPrintInfo:…`, fit to page) and save (the document's
`dataRepresentation`, with the highlights). WebKit keeps the page, so the
address, history and back/forward work as for any page; the viewer is closed
on the next commit, sleep or close. ⌘F searches the PDF
(`findString:withOptions:`, every match highlighted with
`setHighlightedSelections:`, ⌘G steps), ⌘+/⌘-/⌘0 zoom it, ⌘P prints and ⌘S
saves it. Page and zoom changes update the toolbar through PDFKit's
notifications (`pdfChanged:`). The viewer clips to its bounds: PDFView
draws past its edges otherwise (AppKit stopped clipping by default in macOS
14) and showed through the thumbnails. The PDF is in memory twice while shown
(WebKit's page process and ours). Bench: `pdf` → `PDF pages= page= sidebar=
twoup= rotation= highlights=`, `pdfcmd <i>` (a toolbar button,
`pdf::BUTTONS`), `pdfselect <page>`, and `PDFVIEW` when one opens. Test:
`pdfs_open_in_the_pdf_viewer`; ` reading=<word>` is added while reading
aloud (silent in bench mode).

## Local files and folder pages

A path typed in the address bar (`files::typed_path`), or a `file://`
address of a folder, opens `mybrowser://files/<path>/`: `internal::serve`
reads the folder when the page is asked for (`files::folder_page`: at most
5,000 entries, dot-files with `?hidden=1`) and builds the page: breadcrumbs,
Up, a filter field, Show hidden files, Show in Finder, and a table (name
with the Finder's icon for its kind, as a cached PNG `data:` address; date;
size; kind) the page's own script sorts and filters. Names are escaped
everywhere, attributes included. It's only served as a page of its own: a
request whose main document isn't a `mybrowser:` page (a website's fetch,
image or frame) gets an error. Its links go through the browser
(`Tab::action_policy`, for main-frame link clicks from an internal page):
`mybrowser://open/<path>` opens a file (`Tab::open_local`: WebKit loads it
with `loadFileURL`, allowed to read its folder) and `mybrowser://reveal/…`
shows the folder in Finder; WebKit itself refuses links from there to
`file:`. A local file WebKit can't show isn't downloaded:
`decide_response` cancels it and NSWorkspace opens it in its app, except
apps, scripts, installers and other executables (`files::runs_code`),
which are only shown in Finder. Images, text, audio and video show in
WebKit's own image and media pages. Bench: `OPENLOCAL app|reveal <path>`.
Test: `folders_show_as_pages_and_files_open_in_the_tab`.

## Find in page

`Tab::show_find` adds a find row under the toolbar, built on first use, and
shrinks the web view (`Tab::web_frame` accounts for it, including when a
sleeping tab wakes). Searching uses WebKit's public
`findString:withConfiguration:` with wrap-around, which continues from the
current selection. A *new* term first clears the selection, through the
browser's own script world (`defaultClientWorld`, invisible to the page), so
it starts at the top. The field's delegate maps Enter, Shift+Enter and Esc;
⌘G/⇧⌘G are menu items that open the bar if needed.

After each WebKit find, `resources/find.js` (browser's script world)
collects the page's visible text as one string, with a line break between
blocks as WebKit's find has, so a match across tags (`<b>al</b>pha`)
counts; finds every match (case-insensitive, at most 1,000); works out
which one is WebKit's selection; and marks them with the CSS Custom
Highlight API (`CSS.highlights`, `::highlight(myb-find)` yellow, the current
one orange, from a `<style>` it adds). The find bar shows "3 of 17".
Closing the bar or clearing the term removes the marks and the style. In a
PDF, PDFKit finds instead. Test: `find_counts_matches_and_marks_the_current_one`.

## Downloads

`App` is the `WKDownloadDelegate`.
- **Destination:** the `downloads_dir` setting or `~/Downloads`. The suggested
  name is sanitized: path components, leading dots and direction overrides
  (U+202E and friends, which make `gpj.app` read as `ppa.jpg`) are removed,
  `:` and control characters replaced. Then it's made unique (`name (2).ext`).
- **Quarantine:** WebKit marks the file `com.apple.quarantine`, so Gatekeeper
  checks it before it runs (tested).
- **Tracking:** in flight in `App.downloads` (WKDownload → row id), recorded
  in the `downloads` table, and finished as done, failed or cancelled with the
  size on disk.
- **After a crash:** rows still marked active at launch are marked failed.

**Quick Look** (a download's button, or Space on its row): the app delegate
is Quick Look's panel controller (`acceptsPreviewPanelControl:` and its data
source; it's at the end of the responder chain). Quartz is loaded on first
use (`NSBundle.load`), so it costs nothing until then. In bench mode it
prints `PREVIEW <path>` instead, since the panel would take focus.

## Extensions (`src/extensions.rs`)

- **Install:** the user picks a folder, a `.zip` or a Chrome package
  (`.crx`); the browser reads the manifest and asks first, listing the API
  permissions and sites it wants (skipped in bench mode). Then it copies the
  folder into `<profile>/extensions/<ms>-<n>-<name>` (symlinks skipped),
  records it (`access = click`) and gives it a toolbar button.
- **Packages** (`extensions::unpack`): a `.crx` is a zip after a header
  (CRX3: `Cr24`, 3, header length, protobuf header). `crx::verify` checks it
  first: every RSA/ECDSA proof must verify over
  `"CRX3 SignedData\0" + len + signed data + zip`, and one key's SHA-256
  must start with the signed `crx_id`, which gives the extension id. CRX2 is
  refused. A Web Store package (saved as `<id>….crx` in the temporary
  folder) must have the id of the page it was added from. The zip goes through `/usr/bin/ditto
  -x -k` into `$TMPDIR/my-browser-packages/<pid>-<ms>/`, which keeps every
  entry inside (it strips `..` and absolute paths); the manifest may be at the
  top or in the archive's one folder. The unpacked copy is deleted once
  copied into the profile, or if the install is cancelled or fails. Test
  fixture: `tests/fixtures/title.crx`, made by `make-crx.py` with a key that
  was thrown away.
- **Reload** (unpacked folders only): the `extensions.source` column keeps the
  folder the user picked. `ext.reload` copies it again over the profile's
  copy (new copy first, then swap), re-reads the manifest, and restarts the
  extension if it was running. Settings shows the button for enabled
  extensions with a source. Installs from before the column have none.
- **Chrome Web Store:** on an extension's page
  (`chromewebstore.google.com/detail/<name>/<id>`) a puzzle-piece button
  appears left of the star (not in private windows). Clicking it loads
  Google's package address for that id in the tab (`webstore_package_url`),
  a user navigation like any link. Google redirects it, so the download is
  recognised by its file name (`<ID>_<version>.crx`); it goes to a fresh file
  in the temporary folder instead of Downloads (WebKit cancels a download
  whose file exists), unlisted, and the install prompt follows
  (`App::install_package`). A `.crx` downloaded any other way is an ordinary
  download: nothing installs without the user picking it. Checked by hand
  with uBlock Origin Lite (bench `storeclick` presses the button); the e2e
  tests don't reach the network.
- **Launch:** every enabled extension's manifest is read
  (`WKWebExtension.extensionWithResourceBaseURL`; nothing runs) for its
  button. Only `always` extensions get a `WKWebExtensionContext` now.
- **Start:** a context with a stable `uniqueIdentifier` (`ext-<row id>`, so its
  storage survives restarts), the manifest's API permissions granted, and site
  access granted only for `always`. Started on the first click otherwise, then
  it runs for the session.
- **Click** (`App::extension_clicked`): starts it if needed, grants
  `*://<site>/*` and records `(id, site)` on the tab. If the extension has
  content scripts for the page, the page reloads so they run, and the action
  runs when the reload finishes; otherwise right away. The action is
  `performActionForTab`: WebKit opens the popup through
  `presentPopupForAction`, and the browser shows `action.popupPopover` under
  the button.
- **Leaving the site** (a main-frame navigation to another host, or closing
  the tab) removes the tab's `(id, site)` and revokes the pattern unless
  another tab still uses it.
- **Buttons:** one per enabled extension at the right of every toolbar; the
  address bar shrinks to fit. Dimmed while off on the page. Right-click:
  "Only when clicked" / "On every site" / "Manage Extensions…".
- **Runtime requests** (`permissions.request`, access to more sites) are asked
  as a sheet on the tab; nothing is granted silently.
- **Tabs:** WebKit injects content scripts only into web views it knows as
  tabs. So `Tab` implements `WKWebExtensionTab`, `ExtWindow` implements
  `WKWebExtensionWindow` (one per native tab group), and `App` implements
  `WKWebExtensionControllerDelegate`. `App` reports tab open, close, activate
  and property changes (URL/title/loading) to the controller. Extension tab
  loads (`tabs.create`/`update`) can't open `mybrowser:`, `file:` or
  `javascript:` addresses.
- **Keyboard commands**: `extensions::read_commands` reads the manifest's
  `commands` (with `suggested_key`: `mac`, else `default`; Chrome's Ctrl and
  Command are ⌘ on a Mac, MacCtrl ⌃, Alt ⌥; keys a menu can't take are
  left without). Each becomes an item of the Extensions menu
  (`rebuild_extension_menu`, on register and unload; tag = id × 100 +
  index, so ids are `extensionCommand:#<tag>` in Settings → Shortcuts),
  with the suggested key unless another item has it. Choosing one starts
  the extension and turns it on for the tab's site like a click
  (`turn_on_for_site`), then `performCommand:` (`commands.onCommand`);
  `_execute_action` is a click. The user's keys for them are applied after
  every rebuild (`shortcuts::apply_saved`, which records built-in keys only
  once). Test: `extension_keyboard_commands_are_menu_items`.
- **`chrome.offscreen`** (missing in WebKit): extensions asking for it get
  a stand-in (`resources/offscreen.js`, an iframe in the background page) and
  their background page is put in a hidden window when it creates the
  document, so it can play audio (`App::host_extension_audio`). The same
  message carries the extension's own "set User-Agent" rule, applied to that
  background page (WebKit ignores it for extension pages). The manifest
  in the profile's copy is rewritten for this (`adapt_offscreen`, at install
  and launch). See DESIGN-DECISIONS.
- **Not supported yet:**
  - `tabs.onDetached`/`onAttached` when a tab is dragged to another window,
  - packed `.zip`/`.appex` install,
  - extensions in throwaway (private) profiles.

### Updating Web Store extensions

An extension added from the Chrome Web Store keeps its store id
(`extensions.webstore`, added in 0.3). Its **Update** button
(`ext.update <id>`) asks WebKit to download Google's package for that id
(`startDownloadUsingRequest:` on any awake web view of the profile, so the
page stays put; the download goes through WebKit, like the first install),
expects it as a package (`expected_package`), and `App::finish_update`
unpacks it: the CRX3 signature must prove the same id, or it's refused. If
the new version asks for permissions or sites it didn't have, it asks first.
Then the files replace the installed copy (`internal::recopy`) and the
extension reloads; its storage stays, since its context id is the row id.
There's no automatic checking: an update happens when you press the button.
Bench mode may pass the package address and id (`ext.update <id> <url>
<store id>`). Test: `web_store_extensions_update_in_place`.

## Network gate (`src/netgate.rs`)

The only code that opens a connection. It exists because some extension
services refuse WebKit's own connections (WebKit sends a
`webkit-extension://` Origin and ignores the extension's header rules).

- **Who uses it:** extensions adapted for `offscreen`. `adapt_offscreen`
  writes `resources/websocket.js` into their copy and loads it first in every
  HTML page; it replaces `WebSocket` with one that talks to the browser over
  `runtime.connectNative("my-browser.offscreen")` (a
  `WKWebExtensionMessagePort`). Messages: `open`/`send`/`close` from the page,
  `open`/`message`/`error`/`close` back; binary data as base64.
- **Checks, in order** (`App::net_check`): `wss:` only; not this Mac or the
  local network (names and addresses, including numeric-looking hosts); the
  host is one the manifest asks for (WebKit rejects `wss:` match patterns, so
  the `https:` form is matched); allowed by a stored rule. Refusals before
  the last step are final and not listed. Then, before connecting, the name
  is resolved on another thread (`after_resolving`) and refused if any
  answer is this Mac or the local network (a public name pointing here).
  The system resolves again when connecting, so a zero-TTL name can still
  switch in between (SECURITY.md, known gaps).
- **Blocked list:** session-only (`App.net_blocked`): extension, address,
  count, denied. Quitting forgets it; unanswered means blocked.
- **Rules** (`net_allowed` table, per extension): an address without its
  query (`wss://host/path`) or a whole host (`wss://host/*`). Only a blocked
  request's address or site can be allowed. Removing the extension removes
  its rules.
- **UI:** Settings → Network (allow address, allow site, deny, remove). While
  something waits for an answer, every toolbar shows an orange
  `network.badge.shield.half.filled` button left of the extension buttons
  that opens it.
- **Connections:** one `NSURLSession` (ephemeral, no cookies), created on the
  first allowed connection, delegate callbacks on the main queue. No Origin;
  the User-Agent is the one from the extension's own rule
  (`background_user_agent`).
- **Cost:** nothing until an extension uses it: no session, no objects.

## Keyboard shortcuts

All shortcuts are menu items (see `build_menu` in `src/main.rs`), so they're
discoverable and macOS handles key matching. Items marked "to app" target the
app delegate; text-editing items (undo, copy, paste) go to the first
responder. The bench `key cmd+shift+t` command builds a real `NSEvent` and runs
it through `NSMenu.performKeyEquivalent`, so tests exercise the same matching a
key press does.

| Keys | Action |
|---|---|
| ⌘T / ⌘N / ⌘W | New tab / new window / close tab |
| ⇧⌘N | New private window |
| ⇧⌘A | Launcher: tabs, commands, bookmarks, history |
| ⌘P | Print (the panel's PDF menu saves a PDF) |
| ⌥⌘P | Picture in picture |
| ⌥⌘T | Translate Page / Show Original |
| ⌥⌘I | Show Web Inspector |
| ⇧⌘S | Take Screenshot → Select Area |
| ⌘: / ⌘; | Show Spelling and Grammar / Check Document Now |
| ⌘ + scroll | Zoom (a setting) |
| An extension's keys | Its command (Extensions menu) |
| ⇧⌘R | Reader mode |
| ⌥⌘A | AutoFill password for the focused field |
| ⇧⌘T | Reopen closed tab |
| ⌘L | Address bar |
| ⌘R / ⌘. | Reload / stop |
| ⌘[ / ⌘] | Back / forward |
| ⌘1–⌘8 / ⌘9 | Tab 1–8 / last tab |
| ⌃Tab / ⌃⇧Tab / ⇧⌘] / ⇧⌘[ | Next / previous tab |
| ⌘= ⌘+ / ⌘- / ⌘0 | Zoom in / out / actual size |
| ⌘, / ⌘Y / ⌥⌘L | Settings / history / downloads |
| ⌘D / ⌥⌘B | Bookmark this page (or remove it) / all bookmarks |
| ⌘F / ⌘G / ⇧⌘G | Find in page / next match / previous match (Enter and ⇧Enter in the field too, Esc closes) |
| ⌘M | Minimize |
| ⌥⌘W / ⇧⌘\ | Close other tabs / show all tabs |
| Right-click an extension button | Only when clicked / On every site / Manage |
| ⌘Z ⇧⌘Z ⌘X ⌘C ⌘V ⌘A | Text editing |

## Bench protocol

With `--bench`, the browser reads commands on stdin and prints events on
stdout. The benchmark harness and the e2e tests use it. Commands act on the
active tab.

| Command | Reply |
|---|---|
| *(startup)* | `TUNE <setting>=<0\|1> <ok\|missing>` lines, then `READY` |
| `go <url>`, `back`, `forward`, `reload` | `LOADED <ms> <url>` or `FAILED <ms> <reason>` |
| `newtab [url]` | `LOADED …` if a URL is given, else a `TABS` line |
| `switch <i>` | `LOADED …` if the tab was asleep (it wakes), else a `TABS` line |
| `tabs` / `sleep` | `TABS count=<n> active=<i> asleep=<k> windows=<w>` (`sleep` first puts background tabs to sleep) |
| `heavy` | `HEAVY <0|1> …`: runs the heavy-tab check now, one flag per tab in tab-bar order |
| `tick` | `ASLEEP <0|1> …`: runs the timer's sleep policies now (idle, cap, heavy; no pressure), one flag per tab in tab-bar order |
| `state` | `STATE back=<0\|1> forward=<0\|1> url=<address bar> title=<window title>`, read from the real UI |
| `visible` | `VISIBLE <0\|1>` |
| `key <combo>` | `KEY ok\|none` (whether a menu item took it) |
| `find <text>`, `findnext`, `findprev` | `FIND found=<0\|1>` |
| `findbar` / `findesc` | `FINDBAR open=<0\|1> text=<term> status=<label>` (`findesc` presses Esc in the field first) |
| `barenter <text>` / `barblur` | `barenter` types into the address bar and presses Return through its field editor (`LOADED …`, or nothing if the address is unchanged); `barblur` focuses the bar, then the page: `BLURRED` |
| `selection` | `SELECTION <offset>`: where the current selection (find match) starts |
| `window` | `WINDOW minimized=<0\|1> key=<0\|1> app_active=<0\|1>` |
| `bridge <op>\t<arg>…` | `BRIDGE ok <json>` or `BRIDGE err <message>` |
| `dialog ok\|cancel [check]` | `ANSWERED <0\|1>`: answers the sheet on screen (an alert, or the print panel), ticking its checkbox with `check` |
| `home` | `HOME shown=<0\|1> art=<0\|1> search=<0\|1> sites=<urls>` |
| `extclick <id>` | `EXTCLICK ok\|err <message>`: clicks an extension's button |
| `extbuttons` | `EXTBUTTONS <id>:<on 0\|1> …` for the active tab |
| `gohome` | presses the home button, then answers like `home` |
| `netbutton` | `NETBUTTON <0\|1>`: the blocked-requests button is showing |
| `openurl <url>` | a `TABS` line: opens it like a link from another app (`application:openURLs:`) |
| `star` | `STAR on=<0\|1>`: the star shows the page as bookmarked |
| `progress` | `PROGRESS shown=<0\|1> width=<percent>`: the loading line under the toolbar |
| `storebutton` / `storeclick` | `STOREBUTTON <0\|1>` (the Web Store button is showing) / presses it, `STORECLICK`; the install reports `INSTALLED <json>` |
| `reader` | `READER on\|off\|none`: toggles reader mode (`none`: no article found) |
| `fillpassword <i>` | `FILLPASSWORD`: picks account `i` from the last `PASSWORDS <user>\|…` menu (the bench prints that instead of showing it) |
| `eval <js>` | `EVAL <result>`: runs the script in the page, with a user gesture |
| `menu <Menu>[/<Item>]` | `MENU <item>\|…` (`-` separator) after picking `<Item>` if given, as a click would |
| `private` | `PRIVATE <0\|1>`: the active tab is private (read from its address bar) |
| `printpdf <path>` | `PRINTED <0\|1>` once the page is saved there as a PDF (also printed when any print panel closes) |
| `tabsearch <query>` | `TABSEARCH <title>\|…`: the launcher opened with that query, the matching tabs as listed; answer with `dialog` |
| `launcher <query>` | `LAUNCHER <kind>:<title> [shortcut]\|…` (kind: tab, command, bookmark, history): the launcher with that query, every row as listed; answer with `dialog` |
| `tabflags` | `TABFLAGS pinned=<0\|1> muted=<0\|1> page_muted=<0\|1> audio=<0\|1> swipe=<0\|1> pinch=<0\|1>` for the active tab |
| `zoom` | `ZOOM <level> button=<label>`: the active tab's page zoom and the zoom button's label |
| `bartype <text>` / `barkey up\|down\|enter\|esc` | `SUGGEST [*]search:<text>\|[*]page:<url>\|…`: the address bar's suggestion list after typing or a key (`*` = chosen; empty when closed) |
| `savesearch <text>` | `SAVEDSEARCH`: save a search as Return in the bar would, without loading it |
| `sitedata` | `SITEDATA <name>:<kinds>\|…`: the site data sheet for the active tab's site (answer with `dialog`) |
| `savepage <path>` | `SAVED <0\|1>` once the page is saved there as a Web Archive |
| `linkmenu <url> [i]` | `LINKMENU <title>\|…`: a stand-in for WebKit's link menu after our changes; with `i`, picks item `i` |
| `bookmarksmenu [i]`, `historymenu [i]` | `BOOKMARKSMENU <item>\|…` (or `HISTORYMENU`) as the menu would open (`-` separator, `Name/` submenu); with `i`, picks item `i` first |
| `translate` | `TRANSLATED <from> <n>`, `TRANSLATE off` or `TRANSLATE err <code>`: Translate Page / Show Original |
| `picker` | `PICKER`; then a click in the page picks, a sheet asks (`DIALOG`), and `HIDDEN <0\|1>` |
| `spell` | `SPELL continuous=<0\|1> grammar=<0\|1> correct=<0\|1>` |
| `inspector` | `INSPECTOR <0\|1>`: there's a Web Inspector to show |
| `screenshot <visible\|full\|area [x,y,w,h]> [path]` | `SCREENSHOT <0\|1> <w>x<h>` (a PNG there), `SCREENSHOT copied=1 <w>x<h>`, `SCREENSHOT picking` |
| `screenshotdrag x1,y1,x2,y2` | drags on the area picker: `SCREENSHOT …` or `SCREENSHOT cancelled` |
| `fillautofill <i>` | `FILLAUTOFILL`: picks entry `i` from the last `AUTOFILL <label>\|…` menu |
| `profile` | `PROFILE <name> <address bar placeholder>` |
| `pdf`, `pdfcmd <i>`, `pdfselect <page>` | `PDF pages= page= sidebar= twoup= rotation= highlights=` (or `PDF none`); a viewer button; select a page's start |
| `crash` | panics, then exits 101 without a macOS crash report (tests the crash reporter) |
| `quit` or stdin EOF | exits |

In `bridge` arguments, `\n` stands for a line break.

Unprompted events: `PDFVIEW` (the PDF viewer opened), `AUTOPIP 1` (a video
went to picture in picture), `OPENLOCAL app|reveal <path>` (a local file
WebKit can't show), `CRASHED` (the page process died), `DOWNLOAD
<done|failed|cancelled> <path>`, `DIALOG <message>` (a sheet appeared) and
`POPUP <extension id> shown=<0|1>`. Output goes through `say!`, which ignores
a closed pipe: a harness that stopped reading must not crash the browser.
