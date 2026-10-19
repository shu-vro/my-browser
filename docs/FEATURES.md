# Features

Every user-facing feature of my-browser, grouped by what it's for. When you
add or change a feature, add or update its line here in the same commit.
How each one works: [ARCHITECTURE.md](ARCHITECTURE.md). Why:
[DESIGN-DECISIONS.md](DESIGN-DECISIONS.md).

## Light on memory

- **Sleeping tabs**: a background tab unloads its page and process after a
  few minutes (setting, default 5), or at once when macOS runs low on memory.
  Clicking it reloads it.
- **Settings → Tabs**: memory and CPU of every tab's page process, the
  browser itself and WebKit's network and graphics helpers, refreshed every
  2 s; sleep or close any tab from there.
- **Heavy tab badge**: a background tab using 500 MB or more, or 10% CPU or
  more, gets a gauge icon in its tab, with the reason as its tooltip.
- **Battery aware**: in Low Power Mode, or when the Mac runs hot, background
  tabs sleep after one minute.
- **Restored sessions open asleep**: only the selected tab loads, so 50
  restored tabs cost about the same as one.
- **Native new tab page**: drawn with AppKit, no web view and no page process.
- **Extensions start on first click**: an extension uses no memory until you
  first click its button.
- **Engine tuning**: no prewarmed page process, no back/forward page cache.
- **Fast JavaScript** setting: turn the JIT off to save memory at the cost of
  slower scripts.

## Browsing

- Back, forward, reload, stop and a Home button; **swipe** left or right
  with two fingers on the trackpad to go back or forward.
- **Address bar**: type an address or a search; shows the committed address
  (can't be spoofed by a slow navigation); Return on the current address only
  focuses the page.
- **Local addresses** (`localhost:3000`, `192.168.1.1`, `printer.local`)
  open over plain HTTP as typed, never upgraded to HTTPS first; the site
  button shows a computer and says the page is local.
- **Search keywords and bangs**: your own keywords from Settings → General
  work with or without the `!` (`w rust`); 17 built-in bangs (`!w`, `!g`,
  `!yt`, `!gh`, `!mdn`, `!rs`…) are each **off until you turn them on**
  there, then `!w rust` or `rust !w` searches Wikipedia. Done in the
  browser: the search engine never sees them.
- **Suggestions while typing** in the address bar: your past searches and
  pages from your history, from this Mac only (nothing is sent to a search
  engine as you type). Settings → Storage shows the file they're kept in and
  clears them; a setting turns them off.
- **Search engine** and **homepage** settings.
- **Error page** for failed loads (no certificate bypass).
- **Zoom** in, out and actual size, **remembered per site** (actual size
  forgets it; private windows don't save it).
- **Spelling** checked as you type (Edit → Spelling and Grammar: check now,
  grammar, correct automatically).
- **Developer tools**: Inspect Element and View → Show Web Inspector (⌥⌘I);
  a setting turns them off.
- **Find in page** (⌘F, ⌘G, ⇧⌘G): "3 of 17" in the find bar, every match
  highlighted and the current one in another colour.
- **Reader mode** (⇧⌘R): the article alone, in large type, without scripts.
- **Translate Page** (⌥⌘T, View menu): the page's text translated on this
  Mac by Apple's Translation framework (macOS 26 or later, with the
  languages downloaded in System Settings); View → Show Original puts it
  back. Into your Mac's language, or the one set in Settings → General.
- **Print and save as PDF** (⌘P, and `window.print()`).
- **Save Page As** (⌘S): the page with its images and styles in one Web
  Archive file, readable offline here, in Safari or with Quick Look.
- **Link menu**: right-click a link to open it in a new tab, window or
  private window.
- **Popups open as tabs** (`target=_blank`, `window.open`).
- **File upload** with the native picker.
- **JavaScript dialogs** (alert, confirm, prompt) as sheets; a page looping
  dialogs can be stopped with "Don't allow more".
- **Light, dark or system appearance**, followed by sites with a dark mode.

## Tabs and windows

- Native macOS window tabs.
- New, close, reopen closed tab (⇧⌘T), duplicate, close others, show all
  tabs, move to new window, merge windows.
- **Pinned tabs** (Window → Pin Tab): first in their window, never put to
  sleep, kept by Close Other Tabs and by session restore.
- **Mute a tab** (Window → Mute Tab, or click the speaker in the tab); tabs
  playing sound show a speaker.
- **Search Tabs** (⇧⌘A): type part of a title or address, Return switches;
  most recently used first. ↑/↓ or Tab/⇧Tab (going round) choose a tab.
- Switch tabs with ⌘1–⌘9, ⌃Tab / ⌃⇧Tab, ⇧⌘[ / ⇧⌘].
- **Session restore**: every window and tab, in order, with the same tab
  selected.
- **Private windows** (⇧⌘N): cookies and site data only in memory, gone when
  the last private tab closes; no history, no session, no extensions.
- A crashed page reloads, at most twice in 30 seconds.

## Privacy and security

- **Tracker blocking** with Disconnect's list (the one Firefox uses).
- **Ad blocking** with EasyList's ad-server and pop-up lists (the network
  part of what Brave and uBlock Origin block): ad scripts never load, and
  click-hijacking pop-ups and pop-unders are stopped.
- **Cookie banners hidden** (OneTrust, Cookiebot, Quantcast, Didomi and
  other common consent pop-ups), without consenting to anything; a setting
  in Privacy.
- All **third-party cookies blocked**; Intelligent Tracking Prevention on.
- **HTTPS first**, with automatic fallback.
- **Global Privacy Control**.
- **Safari's fingerprinting protection** (Advanced Tracking and
  Fingerprinting Protection).
- **Tracking parameters removed** from links (fbclid, gclid…).
- **Local network shield**: websites can't reach this Mac's servers or
  devices on the network unless allowed per site.
- **Shields per site**: turn protections down for one site from its site menu.
- **Hide Element on Page…** (View menu, site menu): point at an element,
  click, and it stays hidden on that site.
- **My filters** (Settings → Privacy): your own rules in uBlock Origin's
  syntax: `##.ad`, `example.com##.banner`, `||ads.example^`
  (`$third-party`). Lines it can't do (scriptlets, exceptions) are listed.
- **JavaScript off per site** (site menu → Allow JavaScript): news and
  documentation sites get lighter and quieter; reader mode still works.
- No ad-click attribution; safe-browsing lookups off unless turned on.
- Safari user-agent, to blend in.
- **No telemetry**: the browser itself never sends anything.
- **Network gate** (Settings → Network): connections the browser or an
  extension's adapted pages make are blocked until you allow the address; an
  orange toolbar button shows when something is waiting. Names that resolve
  to this Mac or the local network are refused.
- Internal pages can't be framed or opened by websites.
- Download protections: a second automatic download from a page asks first;
  right-to-left-override tricks stripped from file names.
- Profile folder readable only by you; one running browser per profile.

## Site permissions

- Camera, microphone, screen sharing, location and multiple downloads are
  asked per site and remembered.
- Change them for one site from the button left of the address bar, or all
  of them in Settings → Site settings.
- Web notifications are always denied.

## Media

- Video fullscreen.
- **Picture in picture** (⌥⌘P or the video's menu).
- **Screen sharing** for video calls, asked per site.

## Passwords

- Saved in your Mac's **keychain**; offered when you sign in.
- Filled when you pick an account under a login field (⌥⌘A), also in
  embedded sign-in frames, with the frame's own site's accounts.
- **Touch ID or your Mac's password** before filling or copying (setting, on
  by default).
- Manage them in Settings → Passwords; import a CSV from the Passwords app,
  Safari, Chrome, Edge, Brave or Firefox.

## Bookmarks, history and downloads

- **Bookmarks** (⌘D or the star): Bookmarks menu with folders, and Settings →
  Bookmarks (⌥⌘B).
- **History** (⌘Y): searchable, delete one entry or clear all.
- **Most visited sites** on the new tab page.
- **Downloads** (⌥⌘L): to ~/Downloads or a chosen folder, unique and
  sanitized file names, a list with progress, open, **Quick Look** (or Space),
  show in Finder, cancel and remove.
- **Import** bookmarks, history and passwords from Chrome, Brave, Edge,
  Vivaldi, Arc, Chromium or Safari (Settings → Import), or a bookmarks file.

## Extensions

- **Chrome extensions** from the Chrome Web Store (a button appears on an
  extension's page), or a `.crx`, `.zip` or unpacked folder.
- **CRX3 signatures verified**; Web Store packages must match the page they
  came from.
- **Off on every site until clicked** there; right-click the button to run it
  everywhere.
- Toolbar buttons and popups; install and runtime permission requests ask
  first.
- Enable, disable, remove, **Reload** for unpacked folders, and **Update**
  for Web Store extensions (the new package must be signed as the same
  extension; new permissions are asked for; its data stays). No background
  update checks.
- `chrome.offscreen` stand-in so extensions like Edge Read Aloud can play
  audio.

## Settings and app

- **Settings** at `mybrowser://settings` (⌘,): General, Tabs, Privacy, Site
  settings, Network, History, Downloads, Bookmarks, Passwords, Import,
  Extensions, Storage, Shortcuts, About.
- **New tab page**: banner, search bar and most visited sites, each can be
  turned off.
- **Site data per site**: the site menu's Site Data… lists what the site and
  its subdomains keep (cookies, local storage, databases, cache) with a
  delete button each; Settings → Site data lists and deletes every site's.
- **Storage controls**: see and clear cookies, cache (logins kept), history
  and the downloads list; delete data left behind by older builds.
- **Uninstall** button that lists every path before deleting it.
- **Keyboard shortcuts** for everything, **changed in Settings →
  Shortcuts** (click one, press the new keys; conflicts are refused, Reset
  goes back), kept per profile; macOS App Shortcuts work too.
- **About** page with the version and "Check for updates" (opens the GitHub
  releases page; no background checks).
- **Default browser** (Settings → General): links from other apps and HTML
  files from Finder open in new tabs.
- **Crash reports**: after a crash, asks once whether to report it as a
  GitHub issue you read first. Nothing is sent automatically.

## Distribution

- Signed `.app` (Apple Silicon and Intel) and a DMG, published for every push
  to the `release` branch.
- Command-line flags: `--profile DIR`, `--bench`, `--on-top`.
