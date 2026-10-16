# Security and privacy

## Policy

1. **No telemetry, ever.** No usage stats, automatic crash reports, update
   pings, remote config or "anonymous" metrics. After a crash the browser
   asks whether to report it; "Report" opens a pre-filled GitHub issue in a
   tab, which the user reads and submits (or not) themselves. The browser's own code makes no
   network requests; only websites and extensions the user opens do. The
   one path that connects for an extension (the network gate) is blocked
   until the user allows each address, only `wss:` to a host the extension
   asks for, never this Mac or the local network; no cookies, cache or
   Origin.
2. **No tracking accepted.** Tracking protections are on by default and can't
   be weakened silently:
   - third-party cookies blocked,
   - Intelligent Tracking Prevention on,
   - ad-click attribution off,
   - the tracker blocker on.

   The tracker blocker is the only one with a user switch, on the settings
   page.
3. **Lookups that reveal browsing are opt-in.** Safe browsing (URL hash
   prefixes sent to Apple/Google) is off by default, and the setting explains
   the trade.
4. **User data stays local.** History, downloads, settings and session live in
   the profile's SQLite file. Cookies and site data live in WebKit's
   per-profile store. There's no sync.
5. **No developer tools** (no Web Inspector), so there's no remote-debug
   surface.

## Threat model

| Adversary | What they control | What we protect |
|---|---|---|
| A malicious website | Page HTML/JS, titles, URLs, download names, popups | The browser's own pages and bridge, the file system, other sites' data |
| A tracker embedded in sites | Third-party requests and cookies | Cross-site identity |
| A malicious or buggy extension | Code the user chose to install | Limited to the permissions its manifest declares |
| Another local process | Nothing special | Out of scope: same-user processes can read the profile. FileVault protects data at rest. |

## Trust boundaries and how each is enforced

### Web content → settings bridge

The `browser` script-message handler is reachable from every page, so every
message is checked in `internal::is_trusted`:
- it must come from the **top-level frame**,
- the frame's security origin must be `mybrowser:`,
- the web view's URL must be `mybrowser:`.

Anything else is refused. The body must be an array of strings. Each op
validates its arguments, and settings go through `Store::set_setting`, which
rejects unknown keys and invalid values. Internal keys like `profile_id` can't
be set. `homepage` accepts only http(s) URLs, `downloads_dir` only existing
folders.

Tests: `web_pages_cannot_use_the_bridge_or_open_internal_pages`, and
`settings_defaults_and_validation` in `store.rs`.

### Web content → internal pages

A website can't navigate a tab to `mybrowser:` or frame one
(`Tab::action_policy`: main frame only, browser-started only; the response
also says `frame-ancestors 'none'` and `X-Frame-Options: DENY`, so a site
can't overlay the settings page to trick clicks into "Uninstall"). Only
`mybrowser://settings` may use the bridge, and uninstalling also asks natively. Internal pages load only from the browser's own
actions. The settings page has a strict Content-Security-Policy (no network,
no external scripts), and it renders all site-provided strings (history
titles, URLs, file names) with `textContent`, never `innerHTML`.

### Web content → the user (spoofing and nuisance)

- **Address bar:** shows the committed address, not a page-started pending
  one (`Tab::shown_url`). Test: `address_bar_cannot_be_spoofed_by_a_slow_navigation`.
- **Dialogs:** from a page's second `alert`/`confirm`/`prompt` the user can
  block the rest. Test: `pages_cannot_trap_the_tab_with_dialogs`.
- **Downloads:** the second download from one page needs the site's
  permission. Test: `a_page_needs_permission_for_more_than_one_download`.
- **Crash loops:** a page that crashes three times in 30 s isn't reloaded again.
- **Error pages** escape everything they show and run no script; certificate
  errors can't be bypassed.

### Web content → saved passwords

- The form script runs in its own script world; its message handler exists
  only there, so pages can't send it fake sign-ins or focus events, or call
  its fill function (tested: `webkit.messageHandlers.passwords` is undefined
  in the page).
- Nothing is filled on load. A password is read from the keychain and put in
  the form only after the user picks the account in a native menu, only in
  the top frame, and only if the page is still on the exact site it was saved
  for. A page can't harvest passwords with hidden forms.
- Filling or copying a password first asks for Touch ID (or the Mac's
  password), through `LAContext` with the device-owner policy, unless the
  owner turned "Ask for Touch ID" off in Settings → Passwords. It's on by
  default.
- Saving is always asked, never on plain-HTTP sites (except this Mac's own),
  never in private windows.
- Passwords live in the login keychain, encrypted by macOS. The settings page
  sees only sites and usernames; "Copy Password" goes from the keychain to
  the pasteboard natively.

### Other browsers' data (import)

- Read only when the user clicks an import button, one kind at a time, and
  never written. Databases are read from a private temporary copy, which is
  deleted afterwards.
- Chromium passwords need that browser's keychain secret; macOS asks the user
  before giving it, and the decrypted passwords go straight into our
  keychain items (HTTPS sites only).

### Web content → the local network

Requests from a non-local page to loopback, private, link-local and `.local`
addresses are blocked (a content-blocker list), unless the user allowed the
site. Test: `websites_cannot_reach_the_local_network`.

### Web content → camera, microphone, location

Asked per site with a sheet, remembered unless unticked (never in private
windows), and listed in Settings → Site settings. Screen sharing is asked the
same way, and then macOS's own picker decides what is shared. Web
notifications are always denied.

### Web content → file system (downloads)

The destination file name comes from the server, so it's sanitized
(`sanitize_filename`): only the last path component is kept, leading dots are
removed (no hidden files), right-to-left and other direction overrides are
removed (no `gpj.app` posing as `ppa.jpg`), `:` and control characters are
replaced, and an empty name becomes "download". WebKit quarantines the file,
so Gatekeeper checks it before it can run (tested). The file always goes into the downloads folder,
and an existing file is never overwritten (`unique_path`). Tests:
`download_names_are_safe_and_unique`, `downloads_go_to_the_chosen_folder`.

### Settings page → file system (storage cleanup)

"Delete leftovers" can only remove a fixed list of old paths under `$HOME`
(WebKit's shared default store from before profiles, and an old test bundle).
The page never sends a path. Current profiles' stores aren't in the list.
Sizes and deletion don't follow symlinks. Tests:
`leftovers_are_only_the_known_old_paths`, and
`leftover_data_is_listed_and_deleted_but_profiles_are_kept` (run in a fake home
folder).

### Extensions

- Installing is a user action: picking a folder or package in a native
  dialog, or clicking the Web Store button on that extension's page, then
  confirming a native prompt that lists what it asks for. A site can't
  start an install: only the package the user asked the Web Store button for
  is installed when it downloads.
- `.crx` packages must be CRX3 (`src/crx.rs`): every signature in the header
  must verify (`SecKeyVerifySignature`), and one must be by the key the
  package's id is derived from. A Web Store package must also carry the id
  of the extension whose page the user clicked "Add" on. CRX2 has no signed
  id and is refused, as in Chrome. `.zip` files and folders are the user's
  own and unsigned.
- Packages are unpacked with `ditto` into a temporary folder (entries can't
  escape it) and copied without symlinks.
- **Off on every site until clicked there**, and off again when the tab
  leaves the site. Only extensions set to "On every site" get site access at
  start. Runtime permission requests are asked, never granted silently.
- Extensions can't point tabs at `mybrowser:`, `file:` or `javascript:`
  addresses.
- The browser copies the extension into the profile and skips symlinks, so an
  extension can't point outside its folder.
- The API permissions the manifest requests are granted at start (the user
  saw them at install); site access as above.
- A broken extension isn't kept. Removing it deletes only the copy the browser
  made.
- Extensions don't run in throwaway (private) profiles.

### Profiles

- The profile folder is `0700`: other users on the Mac can't read history.
- Only one running browser per profile (`lock` file, `File::try_lock`).
  Two processes writing the same cookie and history files corrupt them; this
  happened during development.
- Tests and benchmarks never use the real profile.

### Private windows

- Their cookies and site data live in a memory-only WebKit store, separate
  from the profile's both ways, and dropped when the last private tab
  closes.
- Nothing about them reaches `browser.db`: no history, session, downloads
  list, Reopen Closed Tab or remembered permission. Downloaded files and
  bookmarks the user makes do stay; both are deliberate user actions.
- Extensions can't see or run in them. Links from other apps and from
  extensions never open in them.
- Not protection from the network or the sites themselves: the IP address
  and fingerprinting surface are the same as a normal window.

### Private WebKit APIs

We call undocumented WebKit methods for memory and privacy.
- Each is checked with `respondsToSelector:` before use, so a macOS update
  can't crash the browser, only remove a protection.
- The e2e test `engine_and_privacy_settings_are_applied` fails if any is
  missing, so a lost protection is noticed rather than silent.

### The app

`scripts/bundle.sh` signs with the hardened runtime (no injected libraries,
no debugger attach), with only the camera, microphone and location
entitlements.

## Known gaps

- No ad *hiding* (cosmetic filtering) and no debouncing of bounce trackers.
- The local-network shield for websites matches addresses, not DNS answers:
  a public name that resolves to a private address (DNS rebinding) isn't
  caught. The network gate checks DNS answers before connecting, but the
  system resolves again when it connects, so a name with a zero TTL can
  still switch in between.
- Updates aren't automatic: "Check for Updates…" opens the latest GitHub
  release in a tab, and the user downloads it. Releases aren't signed with a
  Developer ID (roadmap 1), so a download is trusted as far as GitHub's HTTPS
  and the `SHA256SUMS` beside it.
- Web notifications aren't supported (denied), by choice.
- The tracker list is updated by hand (`scripts/update-trackers.py`), not
  automatically. Updating automatically would mean the browser making network
  requests of its own, which conflicts with policy 1 unless the user opts in.
- No sandbox entitlements: the browser runs unsandboxed like most dev builds.
  WebKit's own page processes are sandboxed by macOS.
- An extension's API permissions are all-or-nothing at install.

## Hardening roadmap

What would make the browser harder to attack, most valuable first. Each item
says what it protects against and what it costs.

1. **Developer ID signing and notarization.** Builds are signed ad hoc today,
   so the code's identity changes with every build. That means Gatekeeper
   blocks the first launch, users learn to click "Open Anyway" (which also
   works for malware posing as an update), macOS asks again for camera and
   screen access after each update, and Keychain items the browser saved
   prompt again. A Developer ID (Apple Developer Program, paid) gives a
   stable identity. Then: sign with `--options runtime --timestamp`, run
   `notarytool submit --wait` and `stapler staple` (both scripts support it
   now: `SIGN_IDENTITY`, `NOTARY_PROFILE`) in the release workflow,
   with the certificate in an encrypted GitHub secret behind a protected
   `release` environment (required reviewer).
2. **Signed, opt-in updates.** A browser that doesn't update keeps its bugs.
   WebKit updates with macOS; our own code doesn't. Today "Check for
   Updates…" opens the latest GitHub release and the user installs it by
   hand. Next: Sparkle 2 with an EdDSA-signed appcast (after roadmap 1). Ask before the first check (policy 1) and make the
   check go only to the appcast URL. Refresh the tracker list the same way.
3. **App Sandbox.** The app process holds the profile database, the
   extension copies and access to the Keychain. Today a bug in it can reach
   every file the user can. Turn on `com.apple.security.app-sandbox` with
   only `network.client`, `files.user-selected.read-write`,
   `files.downloads.read-write`, camera, microphone and location. This needs
   a profile migration into the container. Importing from Chrome or Safari
   and installing extensions would then go through file pickers.
4. **Protect `main`.** Done for `release`: the repository ruleset "release:
   owner only" lets only the owner create, push to or delete it, and the
   release job also checks `github.actor`. Actions are pinned to commit
   SHAs, Dependabot opens weekly PRs for crates and Actions, and Dependabot
   security alerts (RustSec advisories) are on. Left: a ruleset for `main`
   (no force pushes or deletion) once there are other contributors.
5. **DNS-rebinding protection for websites.** The network gate now checks
   DNS answers; the websites' local-network shield still checks host names.
   Add WebKit's local-network access checks when they ship, or a
   resolve-time check (Network Extension) before that.
6. **Fuzz the parsers.** They read files other programs wrote: macOS crash
   reports, CSV and HTML imports, Chrome's bookmark JSON, CRX headers, the
   offscreen manifest rewriting. Add `cargo fuzz` targets for each and run
   them in a nightly job.
7. **Review private WebKit API on every macOS release.** Each selector is
   checked with `respondsToSelector:` and reported in `TUNE` lines. Run the
   e2e suite on each beta, and check that a missing selector fails closed
   (protection reported missing) rather than open.
8. **Keep the bridge small.** Every bridge op is an attack surface from a
   compromised settings page. New ops validate every argument in
   `store.rs`, never take paths from the page (use native panels), and get
   an e2e test proving web pages can't call them.

Every pull request that adds a trust boundary documents it in "Trust
boundaries" above and adds an e2e test for the attack it stops.

## Reporting a vulnerability

Report privately through GitHub's private vulnerability reporting
(Security → Report a vulnerability on the repository), not in a public
issue. Include steps to reproduce and the macOS version.
