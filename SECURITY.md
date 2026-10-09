# Security and privacy

## Policy

1. **No telemetry, ever.** No usage stats, crash reports, update pings,
   remote config or "anonymous" metrics. The browser's own code makes no
   network requests; only websites the user opens do.
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
(`Tab::action_policy`). Internal pages load only from the browser's own
actions. The settings page has a strict Content-Security-Policy (no network,
no external scripts), and it renders all site-provided strings (history
titles, URLs, file names) with `textContent`, never `innerHTML`.

### Web content → file system (downloads)

The destination file name comes from the server, so it's sanitized
(`sanitize_filename`): only the last path component is kept, leading dots are
removed (no hidden files), `:` and control characters are replaced, and an
empty name becomes "download". The file always goes into the downloads folder,
and an existing file is never overwritten (`unique_path`). Tests:
`download_names_are_safe_and_unique`, `downloads_go_to_the_chosen_folder`.

### Extensions

- Installing is a user action: picking a folder in a native dialog.
- The browser copies the extension into the profile and skips symlinks, so an
  extension can't point outside its folder.
- The permissions and host patterns the manifest requests are granted, and
  shown on the extensions page.
- A broken extension isn't kept. Removing it deletes only the copy the browser
  made.
- Extensions don't run in throwaway (private) profiles.

### Profiles

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

## Known gaps

- Camera/microphone/location permissions use WebKit's default prompt. There's
  no per-site permission manager yet.
- The tracker list is updated by hand (`scripts/update-trackers.py`), not
  automatically. Updating automatically would mean the browser making network
  requests of its own, which conflicts with policy 1 unless the user opts in.
- No sandbox entitlements: the browser runs unsandboxed like most dev builds.
  WebKit's own page processes are sandboxed by macOS.
- Extensions get every permission they request at install; there's no
  per-permission prompt.

## Reporting a vulnerability

This project has no public bug tracker yet. Report privately to the
maintainer, not in a public issue. Include steps to reproduce and the macOS
version.
