# Contributing

## The vision

**The lightest production browser on macOS.** It's for people who care about
how much memory their browser uses, not for web developers.

- **Light first.** Memory is the main metric, then CPU wakeups (battery), then
  speed. A feature that costs memory needs a reason, and the benchmark shows
  its price.
- **Production quality.** Logins persist, downloads work, popups, dialogs and
  file uploads work, extensions work. "Light" never means "broken".
- **Private by construction.** No telemetry, no tracking, no data leaves the
  machine unless the user asks for it.
- **No developer tools.** No Web Inspector, no console, no view-source.
  Extensions are the way to extend the browser.

### Non-goals

- Windows or Linux, for now. The design is macOS-native on purpose.
- Our own rendering engine or a Chromium fork (see
  docs/ARCHITECTURE.md, "Why not our own engine").
- Sync, accounts, built-in AI, or any cloud feature.
- Developer features.

Why each major decision was made, with its evidence:
[docs/DESIGN-DECISIONS.md](docs/DESIGN-DECISIONS.md). Read the entry before
changing the decision, and update it after.

## The rules every change follows

1. **Never regress a bar.** Run the benchmark before and after. `bench`
   exits 1 on any regression, and `--save` refuses to record one. If a feature
   costs memory, say so in the change, with the numbers.
2. **Never skip or weaken a test to make it pass.** Fix the cause. A test may
   wait for asynchronous UI (`state_until`); it may not stop checking the
   thing.
3. **Measure before optimizing, and keep only measured wins.** Engine settings
   go through `MYB_TUNE` experiments first. Record the evidence in the code
   comment and in `previous-iterations/`.
4. **Nothing phones home.** The browser never connects on its own. Websites
   and extensions the user opened make the traffic. The one exception is the
   network gate (`src/netgate.rs`): connections made for an extension that
   WebKit can't make itself, each blocked until the user allows its address
   in Settings → Network. No other code may open a connection.
5. **Private WebKit APIs are allowed, carefully.** Check
   `respondsToSelector:` first, skip when missing, report in the bench `TUNE`
   lines, and cover with the `engine_and_privacy_settings_are_applied` test.
6. **Validate everything that crosses a trust boundary:**
   - bridge messages,
   - settings values,
   - download names,
   - extension folders,
   - URLs from pages.

   See SECURITY.md.
7. **Tests never touch a real profile.** Use a throwaway profile (`--bench`)
   or `TempProfile`.
8. **Prefer the platform.** Native window tabs over a custom tab strip, menu
   items over a key-event monitor, WebKit's content blocker over JavaScript,
   libcompression over a crate, system SQLite over a bundled one. Add a
   dependency only when the platform can't do it.

## Setup

- macOS 15.4+, Xcode command line tools, Rust 1.89+.
- `cargo build --release`.
- `git config core.hooksPath .githooks`: before every commit,
  [.githooks/pre-commit](.githooks/pre-commit) runs `cargo fmt --check`,
  `cargo clippy -D warnings` and the whole test suite. CI runs the same checks
  on every push. The hook checks the working tree, so commit everything you
  changed together.
- Always benchmark and run the e2e tests on **release** builds.

## Workflow

```sh
./target/release/bench --runs 1        # 1. quick baseline on your machine
# 2. make the change
cargo test --release                   # 3. all tests green
./target/release/bench                 # 4. nothing says Regress
./target/release/bench --save          # 5. if numbers improved, record them
```

Commit `benchmark/bars.tsv` with the change that moved it. Bars are
machine-specific: compare on the same Mac, and re-record after a macOS update
(`--rebaseline`, and say why in the commit).

The benchmark floats a window over your screen for several minutes. Don't
touch it: a hidden window voids the run (WebKit stops painting hidden pages).

At the end of a working session, add `previous-iterations/<n>.md`: what
changed, measured decisions, traps found, open items.

## Code style

- Write code that reads like the surrounding code: short functions, comments
  that say *why*, no speculative abstractions.
- `cargo fmt` (settings in rustfmt.toml) and no clippy warnings.
- Every `unsafe` block calls an Objective-C or C API; keep them small.
- **Main thread only** for AppKit and WebKit objects. Long-lived objects (the
  app delegate) may be referenced by raw pointer from timers and the stdin
  thread, because they live until exit.
- **KVO:** every `addObserver` has a matching `removeObserver` before the
  observed object goes away.
- **Delegate callbacks with completion handlers:** call the handler exactly
  once on every path, including errors and cancellation.
- Deliberate shortcuts with a known ceiling get a `ponytail:` comment naming
  the ceiling and the upgrade path.

## Tests

`cargo test --release` runs the unit tests in `src/` and `benchmark/` plus the
end-to-end suite in `tests/e2e.rs`. The e2e suite launches the real browser and
needs a logged-in GUI session (not SSH or headless CI). See
[tests/README.md](tests/README.md).

**Adding a feature means adding its e2e test.** Drive it through the bench
protocol: add a command if needed, keep replies one line, read state from the
real UI.

## Benchmarks

See [benchmark/README.md](benchmark/README.md). **Adding a performance-relevant
feature means adding a metric or a test page.** If it can't be measured, the
benchmark has a blind spot: fix that first. The JS-speed page (`compute`) and
`back_ms` were added exactly this way, before deciding on JIT and page cache.

## Changing the tracker list

```sh
python3 scripts/update-trackers.py     # needs curl and network
cargo test --release                   # unit tests check key rules; e2e checks it compiles
```

WebKit rejects the whole list if one rule is invalid, and only says "error 6".
The generator keeps exception lists to lowercase hostnames. The list is CC
BY-NC-SA 4.0 (non-commercial).

## Releasing

Push to the `release` branch (`git push origin main:release`).
[.github/workflows/release.yml](.github/workflows/release.yml) runs the tests,
builds a universal app (`UNIVERSAL=1 scripts/bundle.sh`), packs it with
`scripts/dmg.sh` and publishes a GitHub release with the DMG, its SHA-256 and
the debug symbols (`.dSYM`, for crash reports). Versions are
`<major.minor from Cargo.toml>.<patch>`, the patch counting up with each
release: bump the minor in Cargo.toml to start a new series.

Builds are signed ad hoc, not notarized (that needs a paid Apple Developer
account), so macOS blocks the first launch until the user chooses **Open
Anyway** in System Settings → Privacy & Security. The app icon is
`resources/icon.svg`; `scripts/icon.sh` regenerates `resources/AppIcon.icns`.

## Getting a change reviewed

Include:
- what changed and why,
- benchmark before/after for any metric that moved (or "no change"),
- the tests added,
- any new private API, trust boundary or dependency, with the reason.
