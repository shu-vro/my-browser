# Benchmarks

The benchmark records **bars**: numbers the browser has to beat. Every change is
measured against them. A change that makes any number worse than its bar fails
the run.

## Quick start

```sh
cargo build --release            # build the browser and the harness
./target/release/bench           # compare against the bars (about 4 min)
./target/release/bench --save    # record the current numbers as the new bars
```

| Flag | Meaning |
|---|---|
| `--runs N` | Repeat every benchmark N times and take the median (default 3). Use `--runs 1` for a quick look. |
| `--only startup,pages,leak` | Run only these parts (startup: idle CPU, wakeups and footprint, about 20 s per run). Can't be saved. |
| `--page anim,text` | With `pages`, measure only these pages. Can't be saved. |
| `--save` | Write the results to `benchmark/bars.tsv`. Commit that file. Refused if any metric regressed, so bars can only get better. |
| `--rebaseline` | Save even if something regressed. Only for when **the harness's way of measuring** changed, so old bars aren't comparable. Never use it to accept a browser change that made things worse. Say why in the commit. |
| `MYB_TUNE` (env) | Override the browser's engine settings for an experiment, for example `MYB_TUNE=jit=1 ./target/release/bench`. See [Engine settings](#engine-settings). |
| `BENCH_VERBOSE=1` (env) | Print the footprint of every process at every measurement. Use it when a number moves and you need to know which process moved. |

Exit codes: `0` means nothing regressed, `1` means at least one metric regressed,
`2` means the run can't be trusted: the harness failed (browser missing, page
timed out), macOS reported memory pressure during the run, or the browser
window was hidden.

Why a hidden window voids a run: WebKit throttles pages that aren't on screen
and skips painting them. The first bars were all measured hidden by mistake:
macOS opened the window behind a full-screen app. Those bars missed all
rendering cost. In bench mode the browser now floats its window above every
app and Space, and the harness checks that the window is visible at every
measurement. A window will appear on top of your screen while the benchmark
runs. Leave it alone and keep the screen unlocked. It never takes keyboard
focus: your typing stays in the app you're using. That used to fail: typing
went into the test browser and showed up as fake "idle CPU".

Why memory pressure voids a run: when the system is short on memory, WebKit
throws away its caches, so pages measure much lighter than they really are. In
one early run this made the `text` page read 115 MB instead of its usual
160 MB. The harness checks the pressure level at every measurement. If pressure
ever rises above normal, it prints `UNRELIABLE` and refuses to save.

Always benchmark a **release** build. Close heavy apps first, because memory
pressure from other apps changes what WebKit keeps.

## Reading the output

```
metric                            bar        now    delta  status
idle_footprint_mb               70.86      70.90    +0.1%  Ok
```

- **Lower is better** for every metric. The unit is the name's suffix (`_mb`,
  `_ms`, `_kb`, `_per_s`, `_count`).
- **Beat**: better than the bar by more than the tolerance. Run `--save` to lock it in.
- **Ok**: within the tolerance (noise).
- **Regress**: worse than the bar by more than the tolerance. The run fails.
- **New**: no bar exists yet.

Tolerance is the larger of 10% of the bar and a fixed noise floor per unit:
2 MB, 20 ms, 8 KB, 1 per second. `_count` metrics have no tolerance, so even
one extra crash or process is a regression.

## How the browser is measured

The harness launches `my-browser --bench` and drives it over stdin and stdout:

| Harness sends | Browser replies |
|---|---|
| *(launch)* | one `TUNE <setting>=<0\|1> <ok\|missing>` line per engine setting, then `READY` |
| `go <url>` | `LOADED <ms> <url>` or `FAILED <ms> <reason>` |
| `back`, `forward`, `reload` | `LOADED <ms> <url>` or `FAILED <ms> <reason>` |
| `state` | `STATE back=<0\|1> forward=<0\|1> url=<address bar> title=<window title>`, read from the real toolbar |
| `quit` (or closing stdin) | exits |

The browser also prints `CRASHED` if its page process dies.

Memory is the **physical footprint**, the same number Activity Monitor shows
in its Memory column. It's summed over the app **and** every WebKit helper
process (page, GPU, networking). Those helpers are started by `launchd`, not by
the app, so a plain child-process search misses them. The harness matches
helpers by three checks: same "responsible process" as the app, a
`com.apple.WebKit.*` name, and a start time after the app started.

Every page is measured 6 seconds after it loads. WebKit frees about 12 MB in
the page process 2–5 seconds after a load. Measuring earlier gave numbers that
jumped between two values from run to run.

## Test pages

Pages are generated in memory and served from `127.0.0.1`, so results don't
depend on the network. Each page stresses one kind of memory:

| Page | What it stresses |
|---|---|
| `blank` | Baseline: the browser with an empty page |
| `text` | About 2 MB of static paragraphs (parsing and layout) |
| `dom` | 100,000 elements created by script |
| `heap` | About 1 million JavaScript objects kept alive |
| `canvas` | 8 canvases of 2048×2048 (bitmap and GPU memory) |
| `images` | 20 different 1024×1024 images (image decoding) |
| `anim` | An endless CSS animation (CPU and wakeups while visible) |
| `compute` | A fixed JavaScript workload: building, sorting and serializing 300,000 objects, plus integer math. Its load time measures JavaScript speed. |

The session test also serves the same pages under a second site
(`localhost` instead of `127.0.0.1`) and alternates between the two. That
way, process swapping between sites, and the caches around it, show up in the
numbers.

## Metrics

| Metric | What it measures |
|---|---|
| `binary_size_kb` | Size of the browser executable |
| `startup_ms` | Launch until the window is ready |
| `first_load_ms` | Launch until the first page has loaded (includes starting WebKit's processes) |
| `idle_footprint_mb` | Total memory with a blank page open |
| `shell_footprint_mb` | Memory of the app process alone, the part we control directly |
| `idle_processes_count` | Number of processes with a blank page open |
| `idle_cpu_ms_per_s` | CPU time per second while idle on a blank page |
| `idle_wakeups_per_s` | CPU wakeups per second while idle. Battery cost. |
| `back_ms` | Pressing Back to a page you just left: instant if it's cached, a reload if not |
| `<page>_load_ms` | Time to load that page in a fresh browser |
| `<page>_footprint_mb` | Total memory 6 s after loading that page in a fresh browser |
| `<page>_peak_mb` | Highest memory reached while loading that page. This is the sum of each process's own peak, so it's an upper bound. |
| `anim_cpu_ms_per_s`, `anim_wakeups_per_s` | Cost of keeping an animated page on screen |
| `reclaim_footprint_mb` | One browser loads every page (alternating between two sites), then returns to blank. This is the memory still held afterwards. |
| `leak_growth_mb` | The same loop 3 times. This is how much more is held after the 3rd loop than after the 1st. Ideally 0. |
| `session_peak_mb` | Highest memory reached during those 3 loops |
| `crashes_count` | Page-process crashes across all runs. Must stay 0. |

## Engine settings

The browser applies a few hidden WebKit settings that trade unused features for
memory (`engine_tuning` in `src/main.rs`). `MYB_TUNE` overrides them without
rebuilding, so a setting can be judged by measurement:

```sh
MYB_TUNE=pagecache=1 ./target/release/bench --runs 2     # what does the page cache cost?
MYB_TUNE=jit=0,prewarm=1 ./target/release/bench --runs 2
```

| Setting | Default | What it does | Evidence |
|---|---|---|---|
| `pagecache` | off | Back/forward cache: keeps pages alive after you leave | Session peak 1816 → 299 MB with it off |
| `prewarm` | off | Starts a spare page process in advance | One fewer process, −9 MB idle with it off |
| `proccache` | on | Keeps exited pages' processes for reuse | No measured effect yet; needs the cross-site re-test |
| `jit` | on (user setting) | JavaScript JIT compiler | Off: ~100 MB less in a heavy session, but JS 2.7× slower (`compute_load_ms` 266 → 708) |
| `suspend` | off | Suspends background processes under memory pressure | Can't be measured: runs under pressure are voided |
| `fullscreen`, `picture_in_picture`, `screen_capture` | on | Element fullscreen (public), picture in picture and screen capture (private) | No effect on `anim` or idle numbers in an A/B against the build without them (2026-10-11) |
| `passwords_script` | on | The saved-passwords form script on every page | 0.1–0.5 MB idle, within noise (2026-10-11) |

**Compare against a build, not only the bars.** The full benchmark's
activity numbers (`*_cpu_ms_per_s`, `*_wakeups_per_s`) move a lot with what
else the Mac is doing: one full run read `anim_wakeups_per_s` 85 and the next
morning's A/B read 8 for the same build. To judge a change, build the commit
before it in a worktree (`git worktree add ../base <commit>`, then
`cargo build --release` there), copy this `bench` next to its binary, and run
both back to back with `--only`/`--page`.

Keep a new default only if the benchmark shows it beats the bars without
regressing anything. Record the numbers in the code comment, in
[docs/DESIGN-DECISIONS.md](../docs/DESIGN-DECISIONS.md), and in the session's
`previous-iterations/` file.

## Workflow for breaking a bar

1. `./target/release/bench --runs 1` to get a quick baseline on your machine.
2. Make the change.
3. `./target/release/bench`. Look for `Beat` rows and make sure nothing says `Regress`.
4. If the result is better, run `./target/release/bench --save` and commit
   `benchmark/bars.tsv` together with the change. The save is refused if
   anything regressed.

Bars depend on the machine and macOS version. Compare runs on the same Mac.
Re-record the bars after a macOS update before judging any change.
