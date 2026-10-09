//! Benchmark harness. Drives `my-browser --bench`, measures the app plus every
//! WebKit helper process it spawns, and compares results against the recorded
//! bars in `benchmark/bars.tsv`. See benchmark/README.md.

use std::collections::BTreeMap;
use std::ffi::{c_int, c_void};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

const BARS: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/benchmark/bars.tsv");
/// Pause after a load before measuring. WebKit frees ~12 MB in the content
/// process ~2-5 s after a load; measuring earlier makes footprints bimodal.
const SETTLE: Duration = Duration::from_secs(6);
/// Window for CPU / wakeup sampling.
const IDLE_WINDOW: Duration = Duration::from_secs(5);
const LOAD_TIMEOUT: Duration = Duration::from_secs(30);
/// Navigation cycles over the whole corpus in the leak test.
const LEAK_CYCLES: usize = 3;
/// A metric regresses when it exceeds bar + max(|bar| * TOLERANCE, slack for its unit).
const TOLERANCE: f64 = 0.10;

// ---------------------------------------------------------------------------
// Process measurement (libproc)

#[repr(C)]
struct RusageInfoV4 {
    uuid: [u8; 16],
    user_time: u64,
    system_time: u64,
    pkg_idle_wkups: u64,
    interrupt_wkups: u64,
    pageins: u64,
    wired_size: u64,
    resident_size: u64,
    phys_footprint: u64,
    proc_start_abstime: u64,
    _skip: [u64; 19],
    lifetime_max_phys_footprint: u64,
    _rest: [u64; 6],
}

const RUSAGE_INFO_V4: c_int = 4;

unsafe extern "C" {
    fn proc_listallpids(buf: *mut c_void, size: c_int) -> c_int;
    fn proc_pid_rusage(pid: c_int, flavor: c_int, buf: *mut RusageInfoV4) -> c_int;
    fn proc_name(pid: c_int, buf: *mut c_void, size: u32) -> c_int;
    // Private but stable (Activity Monitor uses it). Maps a process to the app it is charged to.
    fn responsibility_get_pid_responsible_for_pid(pid: c_int) -> c_int;
    fn mach_timebase_info(info: *mut [u32; 2]) -> c_int;
    fn sysctlbyname(name: *const i8, old: *mut c_void, oldlen: *mut usize, new: *mut c_void, newlen: usize) -> c_int;
}

fn rusage(pid: i32) -> Option<RusageInfoV4> {
    let mut info: RusageInfoV4 = unsafe { std::mem::zeroed() };
    (unsafe { proc_pid_rusage(pid, RUSAGE_INFO_V4, &mut info) } == 0).then_some(info)
}

fn name(pid: i32) -> String {
    let mut buf = [0u8; 64];
    let n = unsafe { proc_name(pid, buf.as_mut_ptr().cast(), buf.len() as u32) };
    String::from_utf8_lossy(&buf[..n.max(0) as usize]).into_owned()
}

fn all_pids() -> Vec<i32> {
    let mut pids = vec![0i32; 8192];
    let n = unsafe { proc_listallpids(pids.as_mut_ptr().cast(), (pids.len() * 4) as c_int) };
    pids.truncate(n.max(0) as usize);
    pids
}

fn abs_to_ns(t: u64) -> u64 {
    let mut tb = [0u32; 2];
    unsafe { mach_timebase_info(&mut tb) };
    t * tb[0] as u64 / tb[1] as u64
}

/// Sum over the app and its WebKit helpers.
#[derive(Clone, Copy, Debug, Default)]
struct Usage {
    footprint: u64,
    shell_footprint: u64,
    /// Sum of each process's lifetime peak: an upper bound on the true joint peak.
    peak: u64,
    cpu_ns: u64,
    wakeups: u64,
    processes: usize,
}

/// WebKit helpers are spawned by launchd, not by the app, so they are not its
/// children. They share the app's responsible pid and start after it.
/// Highest system memory-pressure level seen at any measurement (1 = normal,
/// 2 = warning, 4 = critical). Under pressure WebKit purges caches, so
/// footprints come out lower than normal and must not become bars.
static MAX_PRESSURE: AtomicI32 = AtomicI32::new(1);

/// Measurements taken while the browser window was not on screen. WebKit
/// throttles hidden pages and skips painting them, so those numbers miss
/// rendering cost and are not comparable.
static HIDDEN_SAMPLES: AtomicI32 = AtomicI32::new(0);

fn pressure_level() -> i32 {
    let mut level: c_int = 0;
    let mut len = std::mem::size_of::<c_int>();
    let name = c"kern.memorystatus_vm_pressure_level";
    let ok = unsafe { sysctlbyname(name.as_ptr(), (&raw mut level).cast(), &mut len, std::ptr::null_mut(), 0) } == 0;
    if ok { level } else { 1 }
}

fn measure(app: i32) -> Usage {
    MAX_PRESSURE.fetch_max(pressure_level(), Ordering::Relaxed);
    let Some(app_info) = rusage(app) else { return Usage::default() };
    let responsible = unsafe { responsibility_get_pid_responsible_for_pid(app) };
    let mut u = Usage::default();
    for pid in all_pids() {
        let Some(info) = rusage(pid) else { continue };
        let ours = pid == app
            || (info.proc_start_abstime >= app_info.proc_start_abstime
                && name(pid).starts_with("com.apple.WebKit")
                && unsafe { responsibility_get_pid_responsible_for_pid(pid) } == responsible);
        if pid == app {
            u.shell_footprint = info.phys_footprint;
        }
        if ours {
            if std::env::var_os("BENCH_VERBOSE").is_some() {
                eprintln!("    {pid:>6} {:<28} {:>8.1} MB", name(pid), mb(info.phys_footprint));
            }
            u.footprint += info.phys_footprint;
            u.peak += info.lifetime_max_phys_footprint;
            u.cpu_ns += abs_to_ns(info.user_time + info.system_time);
            u.wakeups += info.pkg_idle_wkups + info.interrupt_wkups;
            u.processes += 1;
        }
    }
    u
}

fn mb(bytes: u64) -> f64 {
    bytes as f64 / (1024.0 * 1024.0)
}

// ---------------------------------------------------------------------------
// Browser driver: speaks the `--bench` stdin/stdout protocol (see src/main.rs).

struct Browser {
    child: Child,
    stdin: ChildStdin,
    events: Receiver<String>,
    crashes: usize,
}

impl Browser {
    /// Launch and wait for READY. Returns the browser and startup time in ms.
    fn launch(exe: &Path) -> Result<(Self, f64), String> {
        let start = Instant::now();
        let mut child = Command::new(exe)
            .args(["--bench", "--on-top"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("launch {}: {e}", exe.display()))?;
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let (tx, events) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        let mut b = Browser { child, stdin, events, crashes: 0 };
        b.next_event(&["READY"])?;
        Ok((b, ms(start.elapsed())))
    }

    fn pid(&self) -> i32 {
        self.child.id() as i32
    }

    fn next_event(&mut self, kinds: &[&str]) -> Result<String, String> {
        let deadline = Instant::now() + LOAD_TIMEOUT;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let line = self.events.recv_timeout(left).map_err(|_| format!("timed out waiting for {kinds:?}"))?;
            if line == "CRASHED" {
                self.crashes += 1;
            }
            if kinds.iter().any(|k| line.starts_with(k)) {
                return Ok(line);
            }
        }
    }

    /// Navigate and wait for the load to finish. Returns load time in ms.
    fn go(&mut self, url: &str) -> Result<f64, String> {
        self.command(&format!("go {url}"))
    }

    /// Send a navigation command (`go <url>`, `back`, ...) and wait for its load.
    fn command(&mut self, cmd: &str) -> Result<f64, String> {
        writeln!(self.stdin, "{cmd}").map_err(|e| e.to_string())?;
        let line = self.next_event(&["LOADED", "FAILED"])?;
        parse_event(&line).ok_or_else(|| format!("{cmd}: {line}"))
    }

    fn measure(&mut self) -> Usage {
        let visible = writeln!(self.stdin, "visible").is_ok()
            && self.next_event(&["VISIBLE"]).is_ok_and(|l| l == "VISIBLE 1");
        if !visible {
            HIDDEN_SAMPLES.fetch_add(1, Ordering::Relaxed);
        }
        measure(self.pid())
    }
}

impl Drop for Browser {
    fn drop(&mut self) {
        let _ = writeln!(self.stdin, "quit");
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            if let Ok(Some(_)) = self.child.try_wait() {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// `LOADED 12.3 ...` -> Some(12.3); FAILED or garbage -> None.
fn parse_event(line: &str) -> Option<f64> {
    let mut parts = line.split(' ');
    (parts.next()? == "LOADED").then_some(())?;
    parts.next()?.parse().ok()
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

// ---------------------------------------------------------------------------
// Corpus: deterministic offline pages served over loopback HTTP.

/// Pages each stress one kind of memory. Names become metric prefixes.
const PAGES: [&str; 8] = ["blank", "text", "dom", "heap", "canvas", "images", "anim", "compute"];

/// Fixed JavaScript workload (objects, sort, strings, JSON, integer math). Its
/// load time is JS speed; it exists so JIT and engine changes show their cost.
const COMPUTE_JS: &str = "<script>const a=[];for(let i=0;i<300000;i++)a.push({k:(i*7919)%300000,s:'v'+i});\
a.sort((x,y)=>x.k-y.k);let t=0;for(const o of a)t+=o.s.length;\
const j=JSON.parse(JSON.stringify(a.slice(0,50000)));let h=0;\
for(let r=0;r<20;r++)for(let i=0;i<1e6;i++)h=(h*31+i)|0;document.title=t+':'+h+':'+j.length</script>";

fn page(path: &str) -> Option<(&'static str, Vec<u8>)> {
    let html = |body: String| Some(("text/html", format!("<!doctype html><meta charset=utf-8><body>{body}").into_bytes()));
    match path {
        "/blank" => html(String::new()),
        // ~2 MB of static text: parsing + layout.
        "/text" => html("<p>Lorem ipsum dolor sit amet, consectetur adipiscing elit, sed do eiusmod tempor incididunt ut labore et dolore magna aliqua. Ut enim ad minim veniam, quis nostrud exercitation ullamco laboris.</p>\n".repeat(10_000)),
        // 100k DOM nodes built by script.
        "/dom" => html("<script>const f=document.createDocumentFragment();for(let i=0;i<100000;i++){const d=document.createElement('div');d.textContent='row '+i;f.appendChild(d)}document.body.appendChild(f)</script>".into()),
        // ~1M retained JS objects.
        "/heap" => html("<script>window.keep=[];for(let i=0;i<1e6;i++)keep.push({i,s:'item'+i,a:[i,i*2]})</script>".into()),
        // 8 x 2048x2048 canvases: bitmap / GPU backing stores.
        "/canvas" => html("<script>for(let i=0;i<8;i++){const c=document.createElement('canvas');c.width=c.height=2048;c.style.width='64px';const x=c.getContext('2d');const g=x.createLinearGradient(0,0,2048,2048);g.addColorStop(0,'#'+(i*111111%999999).toString().padStart(6,'0'));g.addColorStop(1,'#000');x.fillStyle=g;x.fillRect(0,0,2048,2048);document.body.appendChild(c)}</script>".into()),
        // 20 distinct 1024x1024 images: decode memory.
        "/images" => html((0..20).map(|i| format!("<img src=/img/{i}.bmp width=64>")).collect()),
        // Endless CSS animation: CPU/wakeups while visible.
        "/compute" => html(COMPUTE_JS.into()),
        "/anim" => html("<style>div{width:80px;height:80px;background:#08f;animation:s 1s linear infinite}@keyframes s{to{transform:rotate(1turn)}}</style><div></div>".into()),
        _ => {
            let n: u32 = path.strip_prefix("/img/")?.strip_suffix(".bmp")?.parse().ok()?;
            Some(("image/bmp", bmp(1024, 1024, n)))
        }
    }
}

/// Uncompressed 24-bit BMP with a seed-dependent gradient (distinct per seed).
fn bmp(w: u32, h: u32, seed: u32) -> Vec<u8> {
    let row = (w * 3).div_ceil(4) * 4;
    let size = 54 + row * h;
    let mut out = Vec::with_capacity(size as usize);
    out.extend_from_slice(b"BM");
    for v in [size, 0, 54, 40] {
        out.extend_from_slice(&v.to_le_bytes());
    }
    out.extend_from_slice(&(w as i32).to_le_bytes());
    out.extend_from_slice(&(h as i32).to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&24u16.to_le_bytes());
    for v in [0u32, row * h, 2835, 2835, 0, 0] {
        out.extend_from_slice(&v.to_le_bytes());
    }
    for y in 0..h {
        for x in 0..w {
            out.extend_from_slice(&[(x + seed * 37) as u8, (y + seed * 91) as u8, ((x ^ y) + seed) as u8]);
        }
        out.resize(out.len() + (row - w * 3) as usize, 0);
    }
    out
}

/// Serve the corpus on an ephemeral loopback port. Returns the base URL.
/// Also listens on [::1] (same port), so `localhost` reaches it on IPv6 too.
fn serve() -> String {
    let v4 = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let port = v4.local_addr().unwrap().port();
    let v6 = TcpListener::bind(("::1", port)).ok();
    for listener in std::iter::once(v4).chain(v6) {
        std::thread::spawn(move || {
            for stream in listener.incoming().map_while(Result::ok) {
                std::thread::spawn(move || handle(stream));
            }
        });
    }
    format!("http://127.0.0.1:{port}")
}

/// Same server under a different site. WebKit gives each site its own page
/// process, so alternating sites exercises process swapping and caching.
fn other_site(base: &str) -> String {
    base.replace("127.0.0.1", "localhost")
}

fn handle(mut stream: TcpStream) {
    // Read the whole request head: closing with unread bytes makes the kernel send RST.
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    while !buf.windows(4).any(|w| w == b"\r\n\r\n") && buf.len() < 64 * 1024 {
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
    }
    let req = String::from_utf8_lossy(&buf);
    let path = req.split(' ').nth(1).unwrap_or("/");
    let (status, ctype, body) = match page(path) {
        Some((ctype, body)) => ("200 OK", ctype, body),
        None => ("404 Not Found", "text/plain", b"not found".to_vec()),
    };
    // no-store: every run and every cycle does the same work.
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(head.as_bytes()).and_then(|_| stream.write_all(&body));
}

// ---------------------------------------------------------------------------
// Benchmarks. Every metric is lower-is-better; the name ends with its unit.

type Samples = BTreeMap<String, Vec<f64>>;

fn record(s: &mut Samples, name: &str, v: f64) {
    s.entry(name.to_string()).or_default().push(v);
}

/// CPU ms per second and wakeups per second over IDLE_WINDOW.
fn sample_activity(b: &mut Browser) -> (f64, f64) {
    let before = b.measure();
    std::thread::sleep(IDLE_WINDOW);
    let after = b.measure();
    let secs = IDLE_WINDOW.as_secs_f64();
    (
        after.cpu_ns.saturating_sub(before.cpu_ns) as f64 / 1e6 / secs,
        after.wakeups.saturating_sub(before.wakeups) as f64 / secs,
    )
}

/// Cold start, empty-page cost, idle CPU.
fn bench_startup(exe: &Path, base: &str, s: &mut Samples) -> Result<(), String> {
    let launched = Instant::now();
    let (mut b, startup) = Browser::launch(exe)?;
    b.go(&format!("{base}/blank"))?;
    record(s, "startup_ms", startup);
    record(s, "first_load_ms", ms(launched.elapsed()));
    std::thread::sleep(SETTLE);
    let u = b.measure();
    record(s, "idle_footprint_mb", mb(u.footprint));
    record(s, "shell_footprint_mb", mb(u.shell_footprint));
    record(s, "idle_processes_count", u.processes as f64);
    let (cpu, wakeups) = sample_activity(&mut b);
    record(s, "idle_cpu_ms_per_s", cpu);
    record(s, "idle_wakeups_per_s", wakeups);
    // Back button: instant if the page is cached, a full reload if not.
    b.go(&format!("{base}/text"))?;
    b.go(&format!("{base}/blank"))?;
    record(s, "back_ms", b.command("back")?);
    record(s, "crashes_count", b.crashes as f64);
    Ok(())
}

/// Each page in a fresh browser: load time, settled footprint, peak.
fn bench_pages(exe: &Path, base: &str, s: &mut Samples) -> Result<(), String> {
    for p in PAGES.iter().filter(|p| **p != "blank") {
        let (mut b, _) = Browser::launch(exe)?;
        record(s, &format!("{p}_load_ms"), b.go(&format!("{base}/{p}"))?);
        std::thread::sleep(SETTLE);
        let u = b.measure();
        record(s, &format!("{p}_footprint_mb"), mb(u.footprint));
        record(s, &format!("{p}_peak_mb"), mb(u.peak));
        if *p == "anim" {
            let (cpu, wakeups) = sample_activity(&mut b);
            record(s, "anim_cpu_ms_per_s", cpu);
            record(s, "anim_wakeups_per_s", wakeups);
        }
        record(s, "crashes_count", b.crashes as f64);
    }
    Ok(())
}

/// One browser cycles through the whole corpus, alternating between two sites,
/// returning to blank each cycle.
/// reclaim = what stays after the first cycle; leak = growth over later cycles.
fn bench_leak(exe: &Path, base: &str, s: &mut Samples) -> Result<(), String> {
    let (mut b, _) = Browser::launch(exe)?;
    let mut at_blank = Vec::new();
    let other = other_site(base);
    for _ in 0..LEAK_CYCLES {
        for (i, p) in PAGES.iter().enumerate() {
            let site = if i % 2 == 0 { base } else { &other };
            b.go(&format!("{site}/{p}"))?;
        }
        b.go(&format!("{base}/blank"))?;
        std::thread::sleep(SETTLE);
        at_blank.push(mb(b.measure().footprint));
    }
    record(s, "reclaim_footprint_mb", at_blank[0]);
    record(s, "leak_growth_mb", at_blank[LEAK_CYCLES - 1] - at_blank[0]);
    record(s, "session_peak_mb", mb(b.measure().peak));
    record(s, "crashes_count", b.crashes as f64);
    Ok(())
}

// ---------------------------------------------------------------------------
// Bars: the recorded numbers to beat.

fn median(v: &[f64]) -> f64 {
    let mut v = v.to_vec();
    v.sort_by(f64::total_cmp);
    let n = v.len();
    if n % 2 == 1 { v[n / 2] } else { (v[n / 2 - 1] + v[n / 2]) / 2.0 }
}

/// Absolute noise floor per unit, so tiny values don't flap.
fn slack(metric: &str) -> f64 {
    match metric.rsplit('_').next() {
        Some("mb") => 2.0,
        Some("ms") => 20.0,
        Some("kb") => 8.0,
        Some("s") => 1.0, // *_per_s
        _ => 0.0,         // counts must not grow
    }
}

#[derive(Debug, PartialEq)]
enum Verdict {
    Beat,
    Ok,
    Regress,
    New,
}

fn judge(metric: &str, bar: Option<f64>, now: f64) -> Verdict {
    let Some(bar) = bar else { return Verdict::New };
    let allowed = (bar.abs() * TOLERANCE).max(slack(metric));
    if now > bar + allowed {
        Verdict::Regress
    } else if now < bar - allowed {
        Verdict::Beat
    } else {
        Verdict::Ok
    }
}

fn parse_bars(text: &str) -> BTreeMap<String, f64> {
    text.lines()
        .filter(|l| !l.starts_with('#'))
        .filter_map(|l| {
            let (k, v) = l.split_once('\t')?;
            Some((k.to_string(), v.trim().parse().ok()?))
        })
        .collect()
}

fn format_bars(results: &BTreeMap<String, f64>) -> String {
    let mut out = String::from("# metric\tvalue (lower is better; unit is the name suffix)\n");
    for (k, v) in results {
        out.push_str(&format!("{k}\t{v:.2}\n"));
    }
    out
}

// ---------------------------------------------------------------------------

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let rebaseline = args.iter().any(|a| a == "--rebaseline");
    let save = rebaseline || args.iter().any(|a| a == "--save");
    let runs: usize = args
        .iter()
        .position(|a| a == "--runs")
        .and_then(|i| args.get(i + 1)?.parse().ok())
        .unwrap_or(3);
    if cfg!(debug_assertions) {
        eprintln!("warning: debug build; bars should come from `cargo build --release`");
    }

    let exe: PathBuf = std::env::current_exe().unwrap().with_file_name("my-browser");
    if !exe.exists() {
        eprintln!("{} not found; run `cargo build --release` first", exe.display());
        std::process::exit(2);
    }

    let base = serve();
    let mut s = Samples::new();
    record(&mut s, "binary_size_kb", std::fs::metadata(&exe).unwrap().len() as f64 / 1024.0);
    for run in 1..=runs {
        eprintln!("run {run}/{runs}");
        type Bench = fn(&Path, &str, &mut Samples) -> Result<(), String>;
        let benches: [(&str, Bench); 3] = [("startup", bench_startup), ("pages", bench_pages), ("leak", bench_leak)];
        for (name, f) in benches {
            eprintln!("  {name}");
            if let Err(e) = f(&exe, &base, &mut s) {
                eprintln!("benchmark {name} failed: {e}");
                std::process::exit(2);
            }
        }
    }

    // Crashes are summed, everything else is the median across runs.
    let results: BTreeMap<String, f64> = s
        .iter()
        .map(|(k, v)| (k.clone(), if k == "crashes_count" { v.iter().sum() } else { median(v) }))
        .collect();
    let bars = parse_bars(&std::fs::read_to_string(BARS).unwrap_or_default());

    println!("{:<26} {:>10} {:>10} {:>8}  status", "metric", "bar", "now", "delta");
    let mut regressed = false;
    for (k, &now) in &results {
        let bar = bars.get(k).copied();
        let verdict = judge(k, bar, now);
        regressed |= verdict == Verdict::Regress;
        let (bar_s, delta) = match bar {
            Some(b) if b != 0.0 => (format!("{b:.2}"), format!("{:+.1}%", (now - b) / b.abs() * 100.0)),
            Some(b) => (format!("{b:.2}"), "-".into()),
            None => ("-".into(), "-".into()),
        };
        println!("{k:<26} {bar_s:>10} {now:>10.2} {delta:>8}  {verdict:?}");
    }

    let pressure = MAX_PRESSURE.load(Ordering::Relaxed);
    if pressure > 1 {
        println!("\nUNRELIABLE: macOS reported memory pressure (level {pressure}) during the run.");
        println!("WebKit purges caches under pressure, so footprints read low. Close other apps and rerun.");
        std::process::exit(2);
    }
    let hidden = HIDDEN_SAMPLES.load(Ordering::Relaxed);
    if hidden > 0 {
        println!("\nUNRELIABLE: the browser window was hidden for {hidden} measurement(s).");
        println!("WebKit throttles hidden pages. Keep the screen unlocked and awake, and rerun.");
        std::process::exit(2);
    }
    if save && regressed && !rebaseline {
        println!("\nNOT SAVED: bars only move when nothing regresses. Fix the regression first.");
        std::process::exit(1);
    }
    if save {
        std::fs::write(BARS, format_bars(&results)).expect("write bars");
        println!("\nbars saved to {BARS}");
    } else if regressed {
        println!("\nREGRESSION: at least one metric is worse than its bar");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rusage_struct_matches_c_layout() {
        // 16-byte uuid + 35 u64 fields (sys/resource.h, rusage_info_v4).
        assert_eq!(std::mem::size_of::<RusageInfoV4>(), 16 + 35 * 8);
    }

    #[test]
    fn measures_own_process() {
        let me = std::process::id() as i32;
        // Burn ~20 ms of CPU so CPU time is certain to be nonzero.
        let t = Instant::now();
        let mut x = 0u64;
        while t.elapsed() < Duration::from_millis(20) {
            x = std::hint::black_box(x.wrapping_add(1));
        }
        let info = rusage(me).expect("rusage on self");
        assert!(info.phys_footprint > 0);
        assert!(info.lifetime_max_phys_footprint >= info.phys_footprint);
        let cpu_ms = abs_to_ns(info.user_time + info.system_time) as f64 / 1e6;
        assert!(cpu_ms >= 10.0, "cpu {cpu_ms} ms after burning 20 ms");
        let u = measure(me);
        assert!(u.processes >= 1 && u.footprint >= u.shell_footprint);
        assert!(!name(me).is_empty());
        assert!(all_pids().contains(&me));
    }

    #[test]
    fn reads_pressure_level() {
        assert!([1, 2, 4].contains(&pressure_level()));
    }

    #[test]
    fn parses_events() {
        assert_eq!(parse_event("LOADED 12.5 "), Some(12.5));
        assert_eq!(parse_event("FAILED 3.0 nope"), None);
        assert_eq!(parse_event("LOADED x"), None);
        assert_eq!(parse_event(""), None);
    }

    #[test]
    fn median_and_judging() {
        assert_eq!(median(&[3.0, 1.0, 2.0]), 2.0);
        assert_eq!(median(&[4.0, 1.0, 2.0, 3.0]), 2.5);
        assert_eq!(judge("x_mb", None, 1.0), Verdict::New);
        assert_eq!(judge("x_mb", Some(100.0), 109.0), Verdict::Ok);
        assert_eq!(judge("x_mb", Some(100.0), 111.0), Verdict::Regress);
        assert_eq!(judge("x_mb", Some(100.0), 89.0), Verdict::Beat);
        // Slack keeps small values from flapping.
        assert_eq!(judge("x_ms", Some(10.0), 25.0), Verdict::Ok);
        // Counts have no slack.
        assert_eq!(judge("crashes_count", Some(0.0), 1.0), Verdict::Regress);
        // Negative bars (leak growth) still work.
        assert_eq!(judge("leak_growth_mb", Some(-1.0), 0.5), Verdict::Ok);
        assert_eq!(judge("leak_growth_mb", Some(-1.0), 5.0), Verdict::Regress);
    }

    #[test]
    fn bars_round_trip() {
        let m = BTreeMap::from([("a_mb".to_string(), 1.5), ("b_ms".to_string(), -2.25)]);
        assert_eq!(parse_bars(&format_bars(&m)), m);
        assert!(parse_bars("garbage\nx\tnot-a-number\n").is_empty());
    }

    #[test]
    fn bmp_is_well_formed_and_distinct() {
        let a = bmp(3, 2, 0);
        // 3px * 3B = 9B rows, padded to 12.
        assert_eq!(a.len(), 54 + 12 * 2);
        assert_eq!(&a[..2], b"BM");
        assert_eq!(u32::from_le_bytes(a[2..6].try_into().unwrap()) as usize, a.len());
        assert_ne!(bmp(8, 8, 1), bmp(8, 8, 2));
    }

    #[test]
    fn corpus_server_serves_pages_and_404() {
        let base = serve();
        let get = |path: &str| {
            let mut s = TcpStream::connect(base.trim_start_matches("http://")).unwrap();
            write!(s, "GET {path} HTTP/1.1\r\nHost: x\r\n\r\n").unwrap();
            let mut out = Vec::new();
            s.read_to_end(&mut out).unwrap();
            out
        };
        for p in PAGES {
            let r = get(&format!("/{p}"));
            assert!(r.starts_with(b"HTTP/1.1 200"), "{p}");
        }
        let img = get("/img/3.bmp");
        assert!(img.starts_with(b"HTTP/1.1 200") && img.len() > 3 * 1024 * 1024);
        assert!(get("/nope").starts_with(b"HTTP/1.1 404"));
        // The second site (localhost) is served on IPv6 too.
        let port = base.rsplit(':').next().unwrap();
        let mut s6 = TcpStream::connect(format!("[::1]:{port}")).unwrap();
        write!(s6, "GET /blank HTTP/1.1\r\n\r\n").unwrap();
        let mut out = Vec::new();
        s6.read_to_end(&mut out).unwrap();
        assert!(out.starts_with(b"HTTP/1.1 200"));
        assert_eq!(other_site(&base), format!("http://localhost:{port}"));
        assert!(get("/img/x.bmp").starts_with(b"HTTP/1.1 404"));
    }
}
