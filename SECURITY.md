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

### Web content → the local network

Requests from a non-local page to loopback, private, link-local and `.local`
addresses are blocked (a content-blocker list), unless the user allowed the
site. Test: `websites_cannot_reach_the_local_network`.

### Web content → camera, microphone, location

Asked per site with a sheet, remembered unless unticked, and listed in
Settings → Site settings. Notifications and screen sharing are denied
(WebKit's default without a delegate).

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

- Installing is a user action: picking a folder in a native dialog, then
  confirming a native prompt that lists what it asks for.
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
- The local-network shield matches addresses, not DNS answers: a public name
  that resolves to a private address (DNS rebinding) isn't caught.
- Notifications and screen sharing aren't supported (denied).
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
   `notarytool submit --wait` and `stapler staple` in the release workflow,
   with the certificate in an encrypted GitHub secret behind a protected
   `release` environment (required reviewer).
2. **Signed, opt-in updates.** A browser that doesn't update keeps its bugs.
   WebKit updates with macOS; our own code doesn't. Use Sparkle 2 with an
   EdDSA-signed appcast. Ask before the first check (policy 1) and make the
   check go only to the appcast URL. Refresh the tracker list the same way.
3. **App Sandbox.** The app process holds the profile database, the
   extension copies and access to the Keychain. Today a bug in it can reach
   every file the user can. Turn on `com.apple.security.app-sandbox` with
   only `network.client`, `files.user-selected.read-write`,
   `files.downloads.read-write`, camera, microphone and location. This needs
   a profile migration into the container. Importing from Chrome or Safari
   and installing extensions would then go through file pickers.
4. **Protect the release path.** Make `main` and `release` protected
   branches (reviews, no force pushes). Pin GitHub Actions to commit SHAs,
   not tags. Turn on Dependabot for Cargo and Actions, and add
   `cargo audit` (or `cargo deny`) to CI. There are only six direct crates;
   keep it that way.
5. **Verify CRX signatures.** Chrome Web Store packages are installed after
   an HTTPS download from Google's update server, which is trust in the
   transport only. CRX3 carries signatures in a protobuf header: check them
   with `SecKeyVerifySignature` and reject packages whose ID doesn't match
   the signing key.
6. **DNS-rebinding protection.** The local-network shield checks host names.
   Add WebKit's local-network access checks when they ship, or a
   resolve-time check (Network Extension) before that.
7. **Gate password filling with Touch ID.** Filling already needs a click on
   the account in a native menu. Add `LocalAuthentication`
   (`LAContext.evaluatePolicy`) before reading a password, as Safari does
   when "Require Touch ID" is on.
8. **Fuzz the parsers.** They read files other programs wrote: macOS crash
   reports, CSV and HTML imports, Chrome's bookmark JSON, CRX headers, the
   offscreen manifest rewriting. Add `cargo fuzz` targets for each and run
   them in a nightly job.
9. **Review private WebKit API on every macOS release.** Each selector is
   checked with `respondsToSelector:` and reported in `TUNE` lines. Run the
   e2e suite on each beta, and check that a missing selector fails closed
   (protection reported missing) rather than open.
10. **Keep the bridge small.** Every bridge op is an attack surface from a
    compromised settings page. New ops validate every argument in
    `store.rs`, never take paths from the page (use native panels), and get
    an e2e test proving web pages can't call them.

Every pull request that adds a trust boundary documents it in "Trust
boundaries" above and adds an e2e test for the attack it stops.

## Reporting a vulnerability

Report privately through GitHub's private vulnerability reporting
(Security → Report a vulnerability on the repository), not in a public
issue. Include steps to reproduce and the macOS version.
