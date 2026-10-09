# Design decisions: how and why

Each entry answers one question a contributor will ask, and gives the reason
and the evidence. If you change one of these decisions, update its entry with
the new evidence. Numbers come from the benchmark harness on the development
Mac (Apple Silicon, 24 GB, macOS 27) unless noted.

---

## The foundation

### Why WebKit through WKWebView, and not Chromium, Gecko, Servo or our own engine?

Memory goes to the engine running pages, not to the browser around it. A
blank page costs ~69 MB in total, and a page with 100,000 elements ~430 MB.
The difference is the page's own data, which every engine has to hold. So the
real choices were:

| Option | Why not |
|---|---|
| Chromium / CEF | Several hundred MB of baseline. Its own copy of the engine instead of the one macOS already has loaded and shared. |
| Gecko | Same problem, plus a much harder embedding story |
| Servo, Ladybird | Not ready for everyday sites yet |
| Our own engine | 10+ years of work. It could only use less memory by not supporting parts of the web. It would also be slower: WebKit's JIT and garbage collector have years of tuning behind them. |
| **WKWebView** | Ships with macOS and is already loaded. It supports WebExtensions (15.4+) and has a strong content blocker. Its hidden settings let us turn off memory-hungry features. |

### Why Rust, and how does it talk to AppKit?

Rust gives native speed and no runtime, with memory safety for everything we
write. The `objc2` crates provide typed bindings for AppKit, Foundation and
WebKit. `define_class!` lets us write Objective-C classes in Rust: the app
delegate, tabs, extension windows. That's how we receive delegate callbacks.
Swift would have been equally light; Rust was the owner's choice.

### Why macOS only?

The memory story depends on WKWebView and its private switches. Windows'
WebView2 is Chromium (heavier, weaker extension support), and WebKitGTK has no
extensions. Being cross-platform would mean the lowest common denominator.

---

## Memory

### How do sleeping tabs work, and why destroy the web view instead of suspending it?

A sleeping tab keeps only its URL and title. `Tab::sleep` removes the KVO
observers, then drops the `WKWebView`. WebKit then ends the tab's page
process, and its memory goes back to the system completely. Suspending a
process would keep its memory; destroying it frees everything. The cost is a
reload when you return to the tab. That's why the active tab never sleeps,
and background tabs only sleep after `tab_sleep_minutes` (default 5) or under
memory pressure.

### Why restore sessions asleep?

A restored tab you never click shouldn't cost a page process. With 50 tabs
restored asleep, only the selected one loads; the other 49 cost a window, a
title and a URL each.

### Why is the back/forward cache off?

It keeps whole pages (and their processes) alive after you navigate away, in
case you press Back.

| Measure (cross-site, 3-loop bench session) | Cache on | Cache off |
|---|---|---|
| Session peak | 1816 MB | 299 MB |
| Held after the first loop | 337 MB | 137 MB |

Back was 41 vs 43 ms, but that test page is served `no-store`, which WebKit
never caches. So Back's real cost is still unmeasured (open item).

### Why is the prewarmed process off?

WebKit can start a spare page process so the next site opens faster. Our
custom process pool turned that on, adding a fifth process and ~9 MB at idle.
Off: 4 processes, ~69 MB idle.

### Why is the JavaScript JIT still on?

Turning it off cut memory at the end of a heavy session from ~230 to ~130 MB,
but made JavaScript 2.7× slower (the `compute` page took 266 → 708 ms). The
no-regression rule keeps it on by default. It's a setting ("Fast JavaScript")
for people who prefer memory over speed.

### Why no autoplay?

Video and audio cost memory, CPU and battery the moment they start. With
`mediaTypesRequiringUserActionForPlayback = all`, media plays only after a
click.

### Why does a 30-second timer drive sleeping, and why the 10-second tolerance?

One timer can handle idle tabs, memory pressure and session saving, and it
wakes the CPU about twice a minute. The tolerance lets macOS fold the wakeup
into others, which matters for battery. Event-driven memory-pressure
notifications (a DispatchSource) would react faster but need another
dependency. The timer reads the same kernel pressure level with one `sysctl`.

### How much does the browser's own process cost, and can we shrink it?

~31 MB: 28 MB for the window and frameworks, plus ~3 MB for the toolbar and
tabs. About 9.6 MB of it is live heap owned by Apple's frameworks
(CoreFoundation strings, Objective-C class caches, system icon data). The rest
is freed memory the allocator hasn't returned. Rewriting our UI wouldn't move
it; the savings are in what pages run.

### Why does the release build use LTO, one codegen unit, panic=abort and strip?

The binary went from 548 KB to 362 KB (before the features that came later).
It doesn't change memory, since code is mapped from disk, but it's free,
and "lightest" includes install size.

---

## Tabs and UI

### Why native macOS window tabs instead of our own tab strip?

macOS draws the tab bar and supports dragging tabs into new windows, the "+"
button, "Show All Tabs", accessibility and full-screen behavior. Writing all
that ourselves would be thousands of lines and more memory. Each tab is an
`NSWindow`, which costs little.

### Why is the active tab tracked by hand instead of using the key window?

When the app isn't frontmost, there's no key window, and shortcuts sent
through the responder chain go nowhere. Menu items target the app delegate,
which forwards them to `App.active`. That works whether or not the app is
frontmost, and makes every shortcut testable.

### How are keyboard shortcuts implemented, and why menu items?

Every shortcut is a menu item with a key equivalent (`build_menu`). Users can
find them in the menu bar. macOS handles matching, including Shift implied by
uppercase or symbols. There's no global key monitor to leak keystrokes. Tests
send real `NSEvent`s through `NSMenu.performKeyEquivalent`, the same path a key
press takes.

### Why does the address bar give up focus when a page loads?

While you type, the address bar must not change under you, so updates pause
while it's being edited. After Enter, focus moves to the page, and the bar is
free to show the real URL (including redirects). Without that, the bar showed
stale text forever, a bug found in testing.

---

## Profiles and data

### Why per-profile WebKit data stores and a profile lock?

First, the bug. The browser originally used WebKit's default cookie file,
shared by every copy of the app. The test suite launched up to 9 browsers at
once; each wrote its own cookie jar over the others and erased a real login.

Now each profile has a UUID-keyed WebKit store
(`WKWebsiteDataStore(forIdentifier:)`), and the profile folder is locked
(`File::try_lock`), so a second copy refuses to start. Tests and benchmarks
use in-memory or temporary profiles.

### Why SQLite, and why the system one?

History needs search, and downloads need status updates; flat files would
mean rewriting them. SQLite in WAL mode is crash-safe. Linking the system's
`libsqlite3` instead of bundling it keeps the binary small.

### Why do the `.app` and `cargo run` have separate logins?

WebKit keeps site data under the app's identity: the executable name for a
bare binary, the bundle id for an app. Merging them would mean moving WebKit's
private folders around. It's not worth it; use the `.app` for browsing.

### Why is a bundle needed for video calls?

macOS kills an app that touches the camera or microphone without a usage
string in its `Info.plist`. A bare binary has no Info.plist. `scripts/bundle.sh`
adds the strings and an ad-hoc signature, without which macOS doesn't remember
permission grants.

---

## Privacy

### How does tracker blocking work, and why doesn't it break Messenger?

The browser compiles Disconnect's list (4,042 rules) into WebKit's native
content blocker. It runs in the network process before a request is made, so
there's no JavaScript and almost no cost. Each rule blocks a tracker domain
only when it's loaded as a third party, and **not** on sites owned by the same
company (Disconnect's entity list, the same logic Firefox uses). So
`fbcdn.net` is blocked on a news site but allowed on `messenger.com`.

### Why is the rule list compressed, and why with libcompression?

The JSON is 6.5 MB, because each rule repeats its owner's site list. Raw
DEFLATE brings it to 261 KB. macOS's built-in `libcompression` inflates it,
so there's no new crate. It's inflated once; WebKit caches the compiled
blocker under a hash of the file.

### Why no tab opens until the blocker is ready

Otherwise the first page after launch would load unprotected. On first launch
compiling takes ~0.8 s; after that, the lookup takes milliseconds.

### Why is safe browsing off by default?

It sends hashed URL prefixes of the pages you visit to Apple/Google. That's
protective, but it's a lookup that reveals browsing, and the policy is
nothing leaves without consent. The setting explains the trade.

### Why block all third-party cookies?

This is Safari's policy since 2020, and major sites work with it, including
Messenger. Cross-site cookies are the main tracking mechanism.

---

## Internal pages and extensions

### Why is the settings page HTML instead of native AppKit?

Lists, search and forms take far less code in HTML. The page costs a page
process only while it's open. It's served from inside the binary over a
custom `mybrowser:` scheme, so it can't be loaded from the network.

### How is the settings bridge kept safe from websites?

WebKit makes the message handler reachable from every page, so the handler
checks every message:
- it must come from the top-level frame,
- the frame's origin must be `mybrowser:`,
- the web view's URL must be `mybrowser:`.

Websites also can't navigate to or frame internal pages. Every op validates
its arguments, and the page renders website strings with `textContent`. All
of this is tested end to end (`web_pages_cannot_use_the_bridge_or_open_internal_pages`).

### Why do our tabs implement WebKit's extension tab protocol?

WebKit injects content scripts only into web views it knows as tabs. Without
`WKWebExtensionTab`, an extension loaded and reported no errors, but its
scripts never ran. Implementing it also gives extensions the
`tabs` and `windows` APIs.

### Why don't extensions run in throwaway profiles?

A non-persistent data store is private browsing to WebKit, and extensions
don't run in private windows unless explicitly allowed. That's the correct
default. It's why extension tests use a temporary persistent profile.

### Why copy an extension into the profile on install?

So it keeps working if you move or delete the folder you installed from, and
so removing it can delete exactly what the browser owns. Symlinks are skipped
so an extension can't point outside itself.

### How does find in page work, and why does a new term clear the selection?

It uses WebKit's own public `findString` API, which highlights and scrolls to
the match. That API searches onward from the current selection. Without
clearing it, typing "ba" after "b" would skip a "ba" at the current spot. So
a new term clears the selection first, using the browser's private script
world, which pages can't see. The bar is built the first time you press ⌘F,
so it costs nothing until then (idle memory measured the same with and
without it).

### Why is the leftover cleanup a fixed list of paths?

Deleting files is irreversible, and the request comes from a web page (the
settings page). So the page only says "delete leftovers"; the paths are fixed
in code, resolved under `$HOME`, and shown with sizes before you confirm. The
current profiles' stores aren't in the list, and a unit test checks that they
and other apps' files survive.

---

## Measurement

### How does the benchmark find all of the browser's processes?

WebKit's helper processes are started by launchd, not by the app. Their
"responsible process" is whatever launched the app (VS Code, Terminal). The
harness counts a process as ours if:
- it has the same responsible pid,
- its name is `com.apple.WebKit.*`,
- it started after the app.

Memory is `phys_footprint`, the number Activity Monitor shows.

### Why wait 6 seconds before measuring?

WebKit frees ~12 MB in the page process 2–5 seconds after a load. At 2
seconds, results jumped between two values from run to run.

### Why are runs voided under memory pressure, or when the window is hidden?

Under pressure, WebKit drops its caches and pages measure 30–40% light. A
hidden window isn't painted or animated, so rendering cost disappears. In
both cases the numbers aren't comparable, so the harness refuses them instead
of recording a fake win.

### Why does the benchmark window float but never take keyboard focus?

It floats because macOS often opens a terminal-launched app's window behind a
full-screen app, where WebKit stops painting it. It never takes focus because
it once did: the owner's typing in another app went into the test browser,
opened tabs, and showed up as "idle CPU" (30 ms/s instead of 0). Now it's
visible, never key, and the app never activates. A test enforces this.

### Why can `--save` only lower the bars?

A bar is a promise that the browser won't get worse. If saving accepted
regressions, every slowdown would quietly become the new normal.
`--rebaseline` exists only for when the *measurement* itself changes (like
the hidden-window fix), and the commit must say why.
