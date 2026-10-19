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

Changing a shortcut in Settings rewrites that menu item's key equivalent
and keeps the change in the profile's database, not in macOS's
`NSUserKeyEquivalents` defaults: those are shared by every profile, and
tests would have changed the owner's shortcuts.

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

### Why EasyList's ad servers, and only their domain rules?

Disconnect lists trackers, not ad networks: on fojik.com a click anywhere
opened the site in a new tab and sent the old one to an ad (a Monetag
pop-under from `llvpn.com`). Brave stops it with adblock-rust running
EasyList and uBlock Origin's lists; the part of that doing the work here is
the network rule `||llvpn.com^`. WebKit's content blocker can express
EasyList's plain domain rules (`||domain^`, `$third-party`, `$popup` as the
`popup` resource type, `$document` as a block everywhere) but not its paths,
wildcards or per-site options, so those few hundred are skipped. Domains
Disconnect already lists are skipped too: a plain block would override its
owner exemptions (Facebook's CDN on Messenger).

Measured (2026-10-17): 4,042 → 53,196 rules; compiling takes 0.70 s → 1.39 s
(first launch after an update, or a Shields change); the compiled list grows
12 → 31 MB on disk, mapped by WebKit. Warm A/B, base → new: idle footprint
78.3 → 78.8 MB, shell 32.7 → 33.3 MB, startup 150 → 150 ms, text page
170.4 → 170.8 MB: noise. The binary grows by 460 KB. Cosmetic filters and
scriptlets (Brave's other layers) would need a script in every frame, which
costs memory per frame; not done.

### Why is the rule list compressed, and why with libcompression?

The JSON is 12.9 MB, because each Disconnect rule repeats its owner's site
list. Raw DEFLATE brings it to 720 KB. macOS's built-in `libcompression` inflates it,
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

### How does this compare with Brave, and why use WebKit's switches instead of writing our own?

Brave's Shields: ad and tracker blocking, HTTPS by default, fingerprinting
"farbling" (noise in canvas, audio and WebGL readback, per site and session),
tracking-parameter stripping and debouncing, Global Privacy Control, blocking
sites from localhost, third-party cookie blocking, and per-site Shields down.
Each has a counterpart here:

| Brave | Here |
|---|---|
| Ad/tracker blocking | Disconnect list plus EasyList's ad-server and pop-up domains in WebKit's content blocker (network rules, not cosmetic hiding or scriptlets) |
| HTTPS by default | WebKit's `preferredHTTPSNavigationPolicy`, automatic fallback |
| Farbling | Safari's Advanced Tracking and Fingerprinting Protection, the same idea in WebKit |
| Query filtering | Brave's default parameter list, plus WebKit's link decoration filtering |
| Debouncing (skip bounce trackers) | Not done: needs a maintained list |
| GPC | WebKit's `globalPrivacyControlEnabled` |
| Localhost permission | Local-network shield with per-site allow |
| Shields down per site | Site menu toggle |
| Safe browsing through a proxy | Off by default (no proxy without a server of our own) |

WebKit's versions run in the engine (network process, page process), cost
nothing in our code and get Apple's fixes. Writing farbling ourselves would
mean injecting scripts into every page, which sites can detect and which
costs memory per frame. The private ones follow the usual rule: checked with
`respondsToSelector:` and reported in `TUNE` lines that the e2e test checks.

### Why block the local network, and why with a second rule list?

A website can make your browser send requests to `localhost` or `192.168.1.1`:
attacks on routers, printers, dev servers and apps that listen on localhost.
Brave and Chrome guard against it. A content-blocker list blocks those
requests unless the top page is itself local. It's a separate list because
per-site exemptions use `ignore-previous-rules`, which only reaches rules in
the same list: one list would make "Shields down" also open the local network.
Content-blocker regexes have no `|`, hence one rule per address range.

### Why does the address bar show the committed address?

Tested attack: a page sets `location` to a slow site and keeps showing its own
content. `WKWebView.URL` already shows the new address, so a fake login sat
under the bank's address for seconds. Browsers show the committed address for
page-started navigations and the typed one for the user's own. WebKit's
`_committedURL` gives exactly that; it's private, so it's reported in `TUNE`.

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

### Why is the new tab page native AppKit and not HTML?

Measured with `--bench` (total footprint, browser plus WebKit processes):

| New tab | Total |
|---|---|
| Blank (no web view) | 28.8 MB |
| HTML page (`mybrowser://home`, first version) | 72.5 MB |
| Native page | ~0.5 MB over blank, once per launch |

An HTML page needs a page process per new tab. The native one is a few views.
Its first version used Auto Layout, stack views, a search field and a
monospaced font, and cost +2 MB in the app process (30.1 → 32.1 MB); plain
frames, a text field and drawing the banner as rectangles brought it to
~+0.5 MB. Drawing the banner layer at 1x was tried and was worse (+1 MB: a
layer of its own costs more than it saves).

### Why are extensions off until clicked, and started only on first click?

The owner asked for it, and it's the safest default: an extension with
`<all_urls>` can read every page, including banking. Chrome's "on click" site
access works the same way. Not starting them until the first click also
saves memory: an extension's background page or service worker runs in a
WebKit process.

The page reloads after the first click when the extension has content
scripts for it: WebKit adds content scripts as user scripts, which only run
when a document loads. The popup opens after the reload, so it talks to the
new page. Extensions that must always run (blockers) can be set to "On every
site" (right-click its button, or Settings).

### How do extensions that need `chrome.offscreen` work?

The system WebKit (macOS 27.0.1) has no `offscreen` API: upstream WebKit has
one behind the `WebExtensionOffscreenEnabled` setting, but it isn't in this
build's feature list. Read-aloud extensions (Edge Read Aloud) play their audio
from an offscreen document, so pressing play did nothing.

For an extension whose manifest asks for `offscreen`, the browser changes its
copy in the profile (never the folder it was installed from): the background
service worker becomes a background page that loads
`resources/offscreen.js` first. That stands in for `chrome.offscreen` with an
iframe in the background page. Messages and ports from content scripts reach
the iframe (a port from the background page to its own iframe doesn't).

Audio needed one more step: WebKit only starts media in a web view that's in
a window, and the background page has none, so its `AudioContext` stayed
`interrupted`. The stand-in sends one native message
(`my-browser.offscreen`, the reason for the added `nativeMessaging`
permission) and the browser puts the background web view
(`_backgroundWebView`, private, checked and reported as a `TUNE` line) in a
borderless window that is never shown. Upstream WebKit does the same for its
own offscreen documents. Measured: `AudioContext.state` went from
`interrupted` to `running` (`extensions_can_play_audio_from_an_offscreen_document`).

WebKit also ignores `declarativeNetRequest` rules that set the User-Agent on
an extension's own requests (checked on a WebSocket handshake, even with
access to every site). Read-aloud services want an Edge User-Agent, so the
stand-in reads the extension's own rule files and passes the first "set
User-Agent" value in that native message; the browser sets it as the
background page's `customUserAgent` (printable ASCII, 512 characters at
most). It covers every request from that page, not only the rule's URLs;
it's the extension's own traffic, and no tab is affected. The Origin stays
`webkit-extension://…`, which no browser code can change.

Limits: a worker that uses `importScripts` or `self.registration` would break
as a page. When WebKit ships `offscreen`, drop the stand-in.

### Why does the browser connect for extensions, and why only through a gate?

Some services refuse WebKit's own extension connections: WebKit sends a
`webkit-extension://` Origin and ignores the extension's header rules. Edge
Read Aloud reported "Speech service connection failed". The only way round it
is for the browser's own code to open the connection, which the "nothing
phones home" rule forbade. The owner chose a gate rather than an exception:

- every address is blocked until the owner allows it in Settings → Network;
- blocked requests are listed for the session only, and not answering means
  denied;
- an allowed rule ignores the query (`wss://host/path`), or covers the host
  (`wss://host/*`), so a changing token doesn't need a new answer;
- only `wss:`, only hosts the extension's manifest asks for, never this Mac
  or the local network, no cookies or cache, no Origin.

Memory: nothing exists until the first allowed connection (no URL session,
no objects). The page side is a script only adapted extensions get. Checked
by hand against a local echo server (open, text and binary both ways, clean
close, the rule's User-Agent, no Origin) with a temporary bypass of the
local-network check; the e2e test covers every refusal and the allow flow.

### Why does the home button drop the web view?

The new tab page is native (see above). Going home destroys the page's web
view like closing the tab would, so its memory goes. Back can't return to
the page; Safari's home is a page in history, ours is not one.

### Why does the second dialog get a "don't allow more" checkbox?

An `alert()` in a loop is a sheet that comes straight back, and a window can't
close while a sheet is up: the tab is trapped. Chrome and Safari offer the
same escape.

### Why does a page need permission for a second download?

Otherwise a page can fill the Downloads folder (and try many file names)
without a click. Chrome calls it "automatic downloads". Typed and
user-initiated loads reset the count.

### Why doesn't bench output use `println!`?

`println!` panics when stdout is closed, and release builds abort on panic. A
test that failed and dropped the browser's stdout made the next status line
crash the browser (a crash report on the owner's screen). `say!` ignores
write errors.

### Why are passwords in the Keychain and not Apple's Passwords app?

Apple's Passwords app reaches other browsers through its iCloud Passwords
extension and a native helper, `PasswordManagerBrowserExtensionHelper`. That
helper is signed with launch constraints that only let Chrome, Edge and
Firefox start it ("Missing or empty parent launch constraints" in its own
strings), so a third-party browser can't talk to it, and reading the
Passwords vault directly needs entitlements Apple doesn't give out. The
login keychain is the macOS store any app may use: encrypted by the system,
locked with the Mac, visible in Keychain Access, and guarded per app. Logins
come over from the Passwords app (or Chrome, Firefox…) with its CSV export.

Filling only after the user picks an account in a native menu, never on page
load, means a page can't harvest a password with a hidden form. The form
script lives in its own script world and only reports focus and sign-ins:
two event listeners, no MutationObserver, so pages pay nothing while idle.

### Why are web notifications denied?

The owner doesn't want them. They would also cost memory: notifications only
arrive from awake tabs, so allowed sites would have to skip the sleep timer,
and WebKit doesn't show them for a third-party app by itself (tested: its
permission prompt works with a persistent store, but nothing is ever
displayed), so a page script on every site would have to provide them.

### Why is the reader page a new document with no scripts?

Reader views that restyle the live page keep its scripts running, and those
scripts can undo the cleanup or track reading. The reader page is built
fresh with a `default-src 'none'` policy, so the cleaning script doesn't have
to be perfect: nothing in the article can run.

### Why does "Check for Updates" open a page instead of checking itself?

Rule 4: the browser never connects on its own, and an update check is a
request that says "this person runs my-browser, this version, now". Opening
the latest GitHub release in a tab is a page the user asked for, like any
link: no timer, no wakeups, no memory until clicked, and no network code.
The page shows the newest version; Settings shows this one next to the
button. Automatic checks need a signed appcast (Sparkle, EdDSA) and an
opt-in prompt, and are only worth it with a Developer ID (SECURITY.md
roadmap 1 and 2).

### Why ask for Touch ID before filling a password?

Filling already needed a click in a native menu, but anyone at an unlocked
Mac could fill or copy every saved password. Safari asks the same way.
`LAContext` costs nothing until used, and the owner can turn it off in
Settings → Passwords.

### Why are CRX signatures checked when the download is HTTPS from Google?

HTTPS only proves where the file came from, not that it's the extension the
user asked for or that it wasn't altered on disk before installing. The
CRX3 header signs the zip and proves the id. Checking it is ~200 lines over
Security.framework, with no new crate. CRX2 can't prove an id, and Chrome
refuses it too.

### Why are there developer tools now?

Until iteration 8 there were none (CONTRIBUTING: "no Web Inspector"). The
owner reversed that: they want to inspect pages. WebKit's own Web Inspector
costs nothing until opened (it's another web view, loaded on demand), so
turning it on is two switches: `isInspectable` on every web view and the
private developer-extras preference (Inspect Element in the page menu). A
setting turns both off, for people who'd rather not have them.

### Why are the built-in bangs off by default?

The owner's call: nothing typed in the address bar should go somewhere the
user didn't choose. A word that happens to look like `!w` stays a search
with the user's engine until they turn that bang on in Settings → General
(setting `bangs`). Their own keywords are on because they added them.

### Why spelling on by default?

WebKit's default in a plain `WKWebView` is off, so no word was ever
underlined; Safari turns it on. The browser registers the same default and
leaves the choice to Edit → Spelling and Grammar, where WebKit remembers it.

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
