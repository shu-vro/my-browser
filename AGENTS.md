# AGENTS.md

Operating manual for AI coding agents working on this repo. Read this, then
the latest file in [previous-iterations/](previous-iterations/), then
[docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) for the area you're changing.
Before reversing a decision, read its entry in
[docs/DESIGN-DECISIONS.md](docs/DESIGN-DECISIONS.md).

## Mission

The lightest production browser on macOS: a Rust + AppKit shell on the system
WKWebView. Memory first, then wakeups, then speed. No telemetry, no tracking,
no devtools; extensions yes. Full vision and rules:
[CONTRIBUTING.md](CONTRIBUTING.md).

## Hard rules

- **Don't regress a benchmark bar, and don't weaken or skip a test.** Fix the
  cause. Waiting for async UI with `state_until` is fine; dropping the check
  isn't.
- **Never point the browser at the real profile** (`~/Library/Application
  Support/my-browser`) from tests, scripts or experiments. Use `--bench`
  (throwaway) or `--profile <temp dir>`. Parallel instances on one profile
  erased the owner's logins once.
- **No network requests from the browser's own code**, except through the
  network gate (`src/netgate.rs`): blocked until the owner allows the address
  in Settings → Network. Never add another path that connects.
- **Private WebKit selectors:** check `respondsToSelector:` first, report in
  `TUNE` lines, keep the e2e test in sync.
- **Ask the owner before:**
  - deleting their data,
  - running long benchmarks (a floating window takes over the screen for
    minutes),
  - `--rebaseline`,
  - committing or pushing.

## Commands

```sh
cargo build --release                  # always test release builds
cargo test --release                   # 103 tests, ~12 s, opens windows (needs GUI session)
git config core.hooksPath .githooks    # once per clone: fmt, clippy, tests before every commit
./target/release/bench --runs 1        # ~2 min quick benchmark (floating window)
./target/release/bench --only startup  # ~20 s: idle CPU, wakeups, footprint only
./target/release/bench                 # ~4 min, compare against benchmark/bars.tsv
BENCH_VERBOSE=1 ./target/release/bench # per-process footprint at every measurement
MYB_TUNE=jit=0,pagecache=1 ./target/release/bench --runs 2   # engine experiment
./scripts/bundle.sh                    # signed .app (UNIVERSAL=1 for both CPUs)
./scripts/dmg.sh                       # DMG from the .app; releases: push to `release`
python3 scripts/update-trackers.py     # regenerate tracker rules (network)
```

Don't run the benchmark while rebuilding: it launches `target/release/my-browser`
for every page, and swapping the binary mid-run mixes versions.

## Driving the browser yourself

```sh
(sleep 2; echo "go https://example.com"; sleep 2; echo state; echo quit) | ./target/release/my-browser --bench
```

The full protocol is in docs/ARCHITECTURE.md ("Bench protocol"). Useful
commands:
- `state`, `tabs`, `key cmd+t`,
- `bridge settings.get`,
- `bridge ext.install\t<dir>` (needs `--profile`; throwaway profiles are
  private and don't run extensions).

## Traps

- WebKit helper processes aren't children of the app. Measure through the
  harness, not `ps` trees.
- **Bench windows must never take keyboard focus.** `--on-top` windows are
  ordered front without becoming key, and the app doesn't activate.
  Otherwise the owner's typing in other apps lands in the test browser (it
  did) and skews results.
- **Hidden windows** are throttled and unpainted by WebKit. The harness uses
  `--on-top` and voids hidden runs. Windows from `--on-top` can't be
  minimized.
- **Measure ≥6 s after load.** WebKit frees ~12 MB a few seconds after a page
  loads.
- **Memory pressure** makes pages read light. The harness voids those runs.
- **The page title can arrive after `LOADED`.** Poll with `state_until`.
- **KVO:** remove observers before dropping a web view.
- **Popups:** `createWebViewWithConfiguration` must use WebKit's
  configuration.
- **Content scripts** need tabs registered as `WKWebExtensionTab` (already
  done; keep `didOpenTab`, `didCloseTab`, `didActivateTab` and
  `didChangeTabProperties` calls when changing tab code).
- **The tracker list must be all lowercase hostnames** in `unless-domain`, or
  WebKit rejects the whole list with "error 6".
- **The owner's shell aliases `ls`,** which can hang scripted commands. Use
  `/bin/ls`.
- **Python from python.org lacks certificates.** Use `curl` for downloads.
- **Compare against a build, not the bars.** `bars.tsv` is stale (recorded
  hidden) and full-run activity numbers swing with what else the Mac does.
  Build the previous commit in a worktree and run both back to back with
  `--only`/`--page` (benchmark/README.md).
- **Tests without `--on-top` run hidden windows**: pages report
  `visibilityState` hidden, and APIs like screen sharing refuse. Use
  `launch_with(&["--on-top"])` for those.
- **Modal menus block the main thread.** Context and password menus print a
  bench line instead of popping up in bench mode.
- **WebKit cancels a download whose destination exists.** Always pick a fresh
  file name.
- **`/script/<js>` test pages run in `<head>`:** touch the body in a
  `DOMContentLoaded` listener.

## Commits

One commit per feature or fix, made when it's done and tested, with the
code, tests and docs for it together. The subject line is imperative and
says what changed for the user ("Add find in page (Cmd+F)"). The body says
why, with the numbers if anything was measured. Commit only when the owner
has asked for commits in this session.

## When you finish a session

Write `previous-iterations/<next number>.md`:
- what you changed,
- measured decisions with the numbers,
- traps you found,
- open items in priority order.

Update the docs you made stale.
