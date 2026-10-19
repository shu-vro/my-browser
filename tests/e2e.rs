//! End-to-end tests: launch the real browser in `--bench` mode and drive it
//! over its stdin/stdout protocol. Needs a logged-in macOS GUI session.
//!
//! Each test gets a throwaway profile (in-memory, or a temp folder for the
//! persistence tests), so tests never touch your real browsing data and can
//! run in parallel.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

struct Browser {
    child: Child,
    stdin: ChildStdin,
    lines: Receiver<String>,
    /// `TUNE <name>=<0|1> <ok|missing>` lines printed before READY.
    tune: Vec<String>,
}

fn launch() -> Browser {
    launch_with(&[])
}

fn spawn(extra: &[&str]) -> (Child, ChildStdin, Receiver<String>) {
    spawn_env(extra, &[])
}

fn spawn_env(extra: &[&str], env: &[(&str, &str)]) -> (Child, ChildStdin, Receiver<String>) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_my-browser"))
        .arg("--bench")
        .args(extra)
        .envs(env.iter().copied())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    let (tx, lines) = mpsc::channel();
    std::thread::spawn(move || {
        for l in BufReader::new(stdout).lines().map_while(Result::ok) {
            let _ = tx.send(l);
        }
    });
    (child, stdin, lines)
}

fn launch_with(extra: &[&str]) -> Browser {
    launch_env(extra, &[])
}

fn launch_env(extra: &[&str], env: &[(&str, &str)]) -> Browser {
    let (child, stdin, lines) = spawn_env(extra, env);
    let mut b = Browser { child, stdin, lines, tune: Vec::new() };
    loop {
        match b.next() {
            line if line == "READY" => return b,
            line if line.starts_with("TUNE ") => b.tune.push(line),
            line => panic!("unexpected before READY: {line}"),
        }
    }
}

impl Browser {
    fn next(&mut self) -> String {
        self.lines.recv_timeout(Duration::from_secs(30)).expect("browser said nothing for 30s")
    }

    fn send(&mut self, cmd: &str) -> String {
        writeln!(self.stdin, "{cmd}").unwrap();
        self.next()
    }

    /// Wait for lines starting with each prefix, in whatever order they come
    /// (a script's EVAL reply and the events it caused race). Others are skipped.
    fn wait_all(&mut self, prefixes: &[&str]) -> Vec<String> {
        let mut found: Vec<Option<String>> = vec![None; prefixes.len()];
        while found.iter().any(Option::is_none) {
            let line = self.next();
            if let Some(i) = prefixes.iter().position(|p| line.starts_with(p)) {
                found[i].get_or_insert(line);
            }
        }
        found.into_iter().map(Option::unwrap).collect()
    }

    /// Wait for a line starting with `prefix`, skipping others.
    fn wait_for(&mut self, prefix: &str) -> String {
        loop {
            let line = self.next();
            if line.starts_with(prefix) {
                return line;
            }
        }
    }

    /// Navigate (or back/forward/reload) and return the loaded URL.
    fn nav(&mut self, cmd: &str) -> String {
        let line = self.send(cmd);
        let rest = line.strip_prefix("LOADED ").unwrap_or_else(|| panic!("{cmd}: {line}"));
        rest.split_once(' ').unwrap().1.to_string()
    }

    /// Ask for `state` until `ok` holds (pages and scripts update asynchronously).
    /// 20 s: with a browser per core running in parallel, an extension's
    /// first start can take over 10 s.
    fn state_until(&mut self, ok: impl Fn(&str) -> bool) -> String {
        self.until("state", ok)
    }

    /// Send `command` until its reply passes `ok` (or 20 s pass).
    fn until(&mut self, command: &str, ok: impl Fn(&str) -> bool) -> String {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let s = self.send(command);
            if ok(&s) || Instant::now() > deadline {
                return s;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    fn tabs(&mut self) -> String {
        self.send("tabs")
    }

    fn key(&mut self, combo: &str) {
        assert_eq!(self.send(&format!("key {combo}")), "KEY ok", "{combo} matched no menu item");
    }

    /// Run a settings-page bridge op; returns its JSON reply.
    fn bridge(&mut self, args: &[&str]) -> String {
        let line = self.send(&format!("bridge {}", args.join("\t")));
        line.strip_prefix("BRIDGE ok ").unwrap_or_else(|| panic!("{args:?}: {line}")).to_string()
    }

    fn quit(mut self) {
        writeln!(self.stdin, "quit").unwrap();
        assert!(self.exits_within(10), "did not quit");
    }

    fn exits_within(&mut self, secs: u64) -> bool {
        let deadline = Instant::now() + Duration::from_secs(secs);
        while Instant::now() < deadline {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status.success();
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let _ = self.child.kill();
        false
    }
}

/// A tiny local web server for tests. Routes:
/// `/page/<t>` (title t), `/setcookie`, `/echo` (title = Cookie header),
/// `/file.txt` (a download), `/script/<js>` (runs the URL-decoded js),
/// `/slow/<t>` (title t after 2 s), `/crx/<name>` (the test package).
fn serve() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    std::thread::spawn(move || {
        for stream in listener.incoming().map_while(Result::ok) {
            std::thread::spawn(move || handle(stream));
        }
    });
    base
}

fn handle(mut s: TcpStream) {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
        match s.read(&mut chunk) {
            Ok(0) | Err(_) => return,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
    }
    let req = String::from_utf8_lossy(&buf).into_owned();
    let path = req.split(' ').nth(1).unwrap_or("/").to_string();
    let cookie = req
        .lines()
        .find_map(|l| l.strip_prefix("Cookie: ").or_else(|| l.strip_prefix("cookie: ")))
        .unwrap_or("none")
        .to_string();
    let html = |body: String| ("text/html", String::new(), body.into_bytes());
    let (ctype, extra, body) = if let Some(t) = path.strip_prefix("/page/") {
        html(format!("<title>{t}</title><p>{t}</p>"))
    } else if path == "/setcookie" {
        ("text/html", "Set-Cookie: session=abc123; Max-Age=86400; Path=/\r\n".into(), b"<title>set</title>".to_vec())
    } else if path == "/echo" {
        html(format!("<title>cookie:{cookie}</title>"))
    } else if path == "/file.txt" {
        ("text/plain", "Content-Disposition: attachment; filename=\"file.txt\"\r\n".into(), b"hello download".to_vec())
    } else if let Some(t) = path.strip_prefix("/slow/") {
        std::thread::sleep(Duration::from_secs(2));
        html(format!("<title>{t}</title>"))
    } else if let Some(name) = path.strip_prefix("/crx/") {
        // The signed test package, served under any file name (as Google serves `<id>_<version>.crx`).
        let crx = std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/title.crx")).unwrap();
        ("application/x-chrome-extension", format!("Content-Disposition: attachment; filename=\"{name}\"\r\n"), crx)
    } else if let Some(js) = path.strip_prefix("/script/") {
        html(format!("<title>start</title><script>{}</script>", percent_decode(js)))
    } else {
        html("<title>other</title>".into())
    };
    let head = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\n{extra}Cache-Control: no-store\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = s.write_all(head.as_bytes()).and_then(|_| s.write_all(&body));
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && i + 2 < b.len()
            && i + 2 <= b.len() - 1 + 1
            && let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16)
        {
            out.push(v);
            i += 3;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn encode(js: &str) -> String {
    js.bytes().map(|c| if c.is_ascii_alphanumeric() { (c as char).to_string() } else { format!("%{c:02X}") }).collect()
}

/// A temp folder, removed (with the WebKit data of the profile in it) on drop.
struct TempProfile(PathBuf);

impl TempProfile {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("mybrowser-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        TempProfile(dir)
    }

    fn path(&self) -> &str {
        self.0.to_str().unwrap()
    }
}

impl Drop for TempProfile {
    fn drop(&mut self) {
        // WebKit keeps the profile's cookies under ~/Library/WebKit/<app>/WebsiteDataStore/<id>.
        if let Ok(db) = rusqlite::Connection::open(self.0.join("browser.db"))
            && let Ok(id) =
                db.query_row("SELECT value FROM settings WHERE key='profile_id'", [], |r| r.get::<_, String>(0))
            && let Some(home) = std::env::var_os("HOME")
        {
            // Cookies and site data, and extension storage, both keyed by the profile id.
            let webkit = Path::new(&home).join("Library/WebKit/my-browser");
            let _ = std::fs::remove_dir_all(webkit.join("WebsiteDataStore").join(id.to_lowercase()));
            let _ = std::fs::remove_dir_all(webkit.join("WebExtensions").join(id.to_uppercase()));
        }
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

// ---------------------------------------------------------------------------
// Protocol and navigation

#[test]
fn loads_report_timing() {
    let mut b = launch();
    for url in ["about:blank", "data:text/html,<h1>hi</h1>"] {
        let line = b.send(&format!("go {url}"));
        // `LOADED <ms> <url>`
        let ms: f64 = line.strip_prefix("LOADED ").unwrap().split(' ').next().unwrap().parse().unwrap();
        assert!((0.0..30_000.0).contains(&ms), "{line}");
    }
    b.quit();
}

#[test]
fn failed_load_is_reported() {
    let mut b = launch();
    // Port 1 is a restricted port; WebKit refuses it immediately.
    let failed = b.send("go http://127.0.0.1:1/");
    assert!(failed.starts_with("FAILED "), "{failed}");
    // Browser still works afterwards.
    assert!(b.send("go about:blank").starts_with("LOADED "));
}

#[test]
fn a_harness_that_stops_reading_does_not_crash_the_browser() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_my-browser"))
        .arg("--bench")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut out = BufReader::new(child.stdout.take().unwrap());
    let mut line = String::new();
    while line.trim() != "READY" {
        line.clear();
        out.read_line(&mut line).unwrap();
    }
    drop(out); // like a test that failed and went away
    let mut stdin = child.stdin.take().unwrap();
    writeln!(stdin, "go about:blank\nstate\ntabs\nquit").unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        if let Some(s) = child.try_wait().unwrap() {
            break s;
        }
        assert!(Instant::now() < deadline, "did not quit");
        std::thread::sleep(Duration::from_millis(50));
    };
    assert!(status.success(), "{status:?}");
}

#[test]
fn unknown_command_is_rejected() {
    let mut b = launch();
    assert_eq!(b.send("bogus"), "ERR unknown command: bogus");
}

#[test]
fn quits_on_command_and_on_stdin_eof() {
    launch().quit();

    let Browser { mut child, stdin, .. } = launch();
    drop(stdin);
    let deadline = Instant::now() + Duration::from_secs(5);
    while child.try_wait().unwrap().is_none() {
        assert!(Instant::now() < deadline, "did not exit on stdin EOF");
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn back_forward_reload_and_toolbar_state() {
    let mut b = launch();
    let a = "data:text/html,<title>Page A</title>a";
    let bb = "data:text/html,<title>Page B</title>b";
    let a_url = b.nav(&format!("go {a}"));
    assert!(a_url.starts_with("data:text/html,"), "{a_url}");
    {
        let want = format!("STATE back=0 forward=0 url={a_url} title=Page A");
        assert_eq!(b.state_until(|s| s == want), want);
    }

    let b_url = b.nav(&format!("go {bb}"));
    {
        let want = format!("STATE back=1 forward=0 url={b_url} title=Page B");
        assert_eq!(b.state_until(|s| s == want), want);
    }

    assert_eq!(b.nav("back"), a_url);
    {
        let want = format!("STATE back=0 forward=1 url={a_url} title=Page A");
        assert_eq!(b.state_until(|s| s == want), want);
    }

    assert_eq!(b.nav("forward"), b_url);
    {
        let want = format!("STATE back=1 forward=0 url={b_url} title=Page B");
        assert_eq!(b.state_until(|s| s == want), want);
    }

    assert_eq!(b.nav("reload"), b_url);
    {
        let want = format!("STATE back=1 forward=0 url={b_url} title=Page B");
        assert_eq!(b.state_until(|s| s == want), want);
    }
}

#[test]
fn links_and_files_from_other_apps_open_in_new_tabs() {
    let base = serve();
    let mut b = launch();
    assert!(b.send(&format!("openurl {base}/page/external")).starts_with("TABS count=2 active=1"));
    assert!(b.state_until(|s| s.ends_with("title=external")).ends_with("title=external"));
    // A local HTML file, as when opened from Finder.
    let dir = TempProfile::new("openfile");
    std::fs::write(dir.0.join("page.html"), "<title>local file</title><p>hi").unwrap();
    assert!(b.send(&format!("openurl file://{}/page.html", dir.path())).starts_with("TABS count=3"));
    // (Hidden bench windows can get a late "became key" for the previous tab.)
    b.send("switch 2");
    let state = b.state_until(|s| s.ends_with("title=local file"));
    assert!(state.ends_with("title=local file"), "{state}");
    // Other schemes belong to other apps.
    for other in ["javascript:alert(1)", "mybrowser://settings", "mailto:a@b.example"] {
        assert!(b.send(&format!("openurl {other}")).starts_with("TABS count=3"), "{other}");
    }
    // Run from cargo, not the installed app: it can't become the default.
    assert_eq!(b.bridge(&["default.get"]), r#"{"default":false,"bundled":false}"#);
    assert!(b.send("bridge default.set").starts_with("BRIDGE err Only the installed app"));
    b.quit();
}

#[test]
fn a_crash_is_logged_and_offered_for_reporting_once() {
    let profile = TempProfile::new("crash");
    let mut b = launch_with(&["--profile", profile.path()]);
    writeln!(b.stdin, "crash").unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        if let Some(s) = b.child.try_wait().unwrap() {
            break s;
        }
        assert!(Instant::now() < deadline, "a panic must end the process");
        std::thread::sleep(Duration::from_millis(50));
    };
    assert!(!status.success());
    // The panic hook kept the message and where it happened.
    let logs: Vec<_> = std::fs::read_dir(profile.0.join("crashes")).unwrap().flatten().map(|e| e.path()).collect();
    assert_eq!(logs.len(), 1, "{logs:?}");
    let log = std::fs::read_to_string(&logs[0]).unwrap();
    assert!(log.starts_with("my-browser ") && log.contains("crash requested by the bench harness"), "{log}");
    assert!(log.contains("src/main.rs"), "{log}");
    // The next launch asks, once.
    let mut b = launch_with(&["--profile", profile.path()]);
    assert!(b.wait_for("DIALOG").contains("quit unexpectedly"));
    assert_eq!(b.send("dialog cancel"), "ANSWERED 1");
    b.quit();
    let mut b = launch_with(&["--profile", profile.path()]);
    assert!(b.send("tabs").starts_with("TABS"), "no second prompt");
    b.quit();
}

#[test]
fn untitled_page_shows_its_url() {
    let mut b = launch();
    b.nav("go about:blank");
    {
        let want = "STATE back=0 forward=0 url=about:blank title=about:blank";
        assert_eq!(b.state_until(|s| s == want), want);
    }
}

#[test]
fn engine_and_privacy_settings_are_applied() {
    // Every private WebKit setting must exist on this macOS. If an update
    // removes one, this fails instead of the browser silently getting heavier
    // or leakier.
    let b = launch();
    let mut names: Vec<_> = b.tune.iter().map(|l| l.split(['=', ' ']).nth(1).unwrap().to_string()).collect();
    names.sort();
    let mut want = [
        "ad_attribution_off",
        "committed_url",
        "context_menu",
        "cookie_banners",
        "developer_extras",
        "display_capture",
        "ext_background_view",
        "fingerprinting_protection",
        "global_privacy_control",
        "https_first",
        "jit",
        "local_network_shield",
        "my_filters",
        "page_print",
        "pagecache",
        "pdf_data",
        "playing_audio",
        "picture_in_picture",
        "prewarm",
        "proccache",
        "screen_capture",
        "suspend",
        "tab_mute",
        "third_party_cookies_blocked",
        "tracking_prevention",
        "trackers",
        "web_inspector",
        "web_process_id",
    ];
    want.sort();
    assert_eq!(names, want);
    for line in &b.tune {
        assert!(line.ends_with(" ok"), "{line}");
    }
}

#[test]
fn bench_window_is_on_screen() {
    // Hidden windows get throttled and unpainted by WebKit, which would make
    // every benchmark miss rendering cost.
    let mut b = launch_with(&["--on-top"]);
    b.nav("go about:blank");
    // macOS updates occlusion state asynchronously after the window appears.
    let deadline = Instant::now() + Duration::from_secs(5);
    while b.send("visible") != "VISIBLE 1" {
        assert!(Instant::now() < deadline, "bench window never became visible");
        std::thread::sleep(Duration::from_millis(100));
    }
    // Visible but never focused: keystrokes meant for the app the user is in
    // must not land in a benchmark window (they once did, and skewed results).
    b.key("cmd+t"); // opens a tab, which selects it: the riskiest moment
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(b.send("window"), "WINDOW minimized=0 key=0 app_active=0");
}

// ---------------------------------------------------------------------------
// Tabs and keyboard shortcuts

#[test]
fn keyboard_shortcuts_drive_tabs() {
    let base = serve();
    let mut b = launch();
    b.nav(&format!("go {base}/page/one"));
    assert!(b.tabs().starts_with("TABS count=1 active=0"));

    // Cmd+T: new tab, selected.
    b.key("cmd+t");
    assert!(b.tabs().starts_with("TABS count=2 active=1"), "cmd+t");
    b.nav(&format!("go {base}/page/two"));

    // Cmd+1 / Cmd+9 / Ctrl+Tab / Cmd+Shift+[ ] move between tabs.
    b.key("cmd+1");
    assert!(b.tabs().starts_with("TABS count=2 active=0"), "cmd+1");
    b.key("cmd+9");
    assert!(b.tabs().starts_with("TABS count=2 active=1"), "cmd+9");
    b.key("ctrl+tab");
    assert!(b.tabs().starts_with("TABS count=2 active=0"), "ctrl+tab wraps");
    b.key("cmd+shift+]");
    assert!(b.tabs().starts_with("TABS count=2 active=1"), "cmd+shift+]");
    b.key("cmd+shift+[");
    assert!(b.tabs().starts_with("TABS count=2 active=0"), "cmd+shift+[");

    // Cmd+W closes the active tab; Cmd+Shift+T brings it back.
    b.key("cmd+w");
    let after_close = b.tabs();
    assert!(after_close.starts_with("TABS count=1 active=0"), "cmd+w: {after_close}");
    assert!(b.state_until(|s| s.contains("title=two")).contains("title=two"), "the other tab is now active");
    b.key("cmd+shift+t");
    assert!(b.tabs().starts_with("TABS count=2"), "cmd+shift+t");
    assert!(b.state_until(|s| s.contains("title=one")).contains("title=one"), "reopened tab is active and loaded");

    // History shortcuts.
    b.key("cmd+[");
    b.key("cmd+]");
    b.key("cmd+r");
    // Zoom and address bar.
    b.key("cmd+=");
    b.key("cmd+-");
    b.key("cmd+0");
    b.key("cmd+l");
    b.key("cmd+m");
    // A combination nobody handles is not swallowed.
    assert_eq!(b.send("key cmd+alt+ctrl+j"), "KEY none");
}

#[test]
fn search_tabs_finds_and_switches_to_a_tab() {
    let base = serve();
    let mut b = launch();
    b.nav(&format!("go {base}/page/RustBook"));
    b.nav(&format!("newtab {base}/page/Weather"));
    b.nav(&format!("newtab {base}/page/Recipes"));
    assert!(b.send("key cmd+shift+a").starts_with("TABSEARCH "));
    assert_eq!(b.next(), "KEY ok");
    assert_eq!(b.send("dialog cancel"), "ANSWERED 1");
    // Most recently used first, the tab you're on last.
    assert_eq!(b.send("tabsearch "), "TABSEARCH Weather|RustBook|Recipes");
    assert_eq!(b.send("dialog cancel"), "ANSWERED 1");
    assert!(b.tabs().contains("active=2"), "cancel stays put");
    assert_eq!(b.send("tabsearch book"), "TABSEARCH RustBook");
    assert_eq!(b.send("dialog ok"), "ANSWERED 1");
    assert!(b.tabs().starts_with("TABS count=3 active=0"), "switched");
    assert_eq!(b.send("tabsearch nothing like it"), "TABSEARCH ");
    b.send("dialog ok");
    assert!(b.tabs().starts_with("TABS count=3 active=0"), "no match, no switch");
}

#[test]
fn pinned_tabs_go_first_survive_close_others_and_restarts() {
    let base = serve();
    let profile = TempProfile::new("pinned");
    let mut b = launch_with(&["--profile", profile.path()]);
    b.nav(&format!("go {base}/page/one"));
    b.nav(&format!("newtab {base}/page/two"));
    b.nav(&format!("newtab {base}/page/three"));
    // (Hidden bench windows can get a late "became key" for the previous tab.)
    b.send("switch 2");
    assert!(b.state_until(|s| s.ends_with("title=three")).ends_with("title=three"));
    let menu = b.send("menu Window/Pin Tab");
    assert!(menu.contains("|Unpin Tab|"), "{menu}");
    assert!(b.send("tabflags").starts_with("TABFLAGS pinned=1"));
    b.send("switch 0");
    assert!(b.state_until(|s| s.ends_with("title=three")).ends_with("title=three"), "moved to the front");
    b.send("switch 2");
    assert!(b.state_until(|s| s.ends_with("title=two")).ends_with("title=two"));
    b.send("menu File/Close Other Tabs");
    assert!(b.tabs().starts_with("TABS count=2"), "the pinned tab stays");
    b.quit();
    let mut b = launch_with(&["--profile", profile.path()]);
    b.nav("switch 0");
    assert!(b.state_until(|s| s.ends_with("title=three")).ends_with("title=three"));
    assert!(b.send("tabflags").starts_with("TABFLAGS pinned=1"), "pinned after a restart");
    b.quit();
}

#[test]
fn tabs_mute_and_show_when_they_play_sound() {
    let mut b = launch();
    b.nav("go about:blank");
    assert_eq!(b.send("tabflags"), "TABFLAGS pinned=0 muted=0 page_muted=0 audio=0 swipe=1", "swipe to go back");
    // A tone, started with the user gesture `eval` carries.
    b.send("eval (() => { const c = new AudioContext(); const o = c.createOscillator(); const g = c.createGain(); g.gain.value = 0.001; o.connect(g).connect(c.destination); o.start(); return 1 })()");
    let deadline = Instant::now() + Duration::from_secs(10);
    while !b.send("tabflags").contains("audio=1") {
        assert!(Instant::now() < deadline, "playing sound never reported");
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(b.send("menu Window/Mute Tab").contains("|Unmute Tab|"));
    assert!(b.send("tabflags").starts_with("TABFLAGS pinned=0 muted=1 page_muted=1 audio=1"));
    // A sleeping tab comes back muted.
    b.nav("newtab about:blank");
    b.send("sleep");
    b.nav("switch 0");
    assert!(b.send("tabflags").contains("muted=1 page_muted=1"));
    b.send("menu Window/Unmute Tab");
    assert!(b.send("tabflags").contains("muted=0 page_muted=0"));
}

#[test]
fn zoom_is_remembered_per_site() {
    let base = serve();
    let other = base.replace("127.0.0.1", "localhost");
    let mut b = launch();
    b.nav(&format!("go {base}/page/a"));
    b.key("cmd+=");
    b.key("cmd+=");
    assert_eq!(b.send("zoom"), "ZOOM 1.21");
    b.nav(&format!("go {other}/page/b"));
    assert_eq!(b.send("zoom"), "ZOOM 1.00", "another site has its own zoom");
    b.nav(&format!("newtab {base}/page/c"));
    assert_eq!(b.send("zoom"), "ZOOM 1.21", "the site's zoom comes back, in any tab");
    b.key("cmd+0");
    b.nav(&format!("go {base}/page/d"));
    assert_eq!(b.send("zoom"), "ZOOM 1.00", "actual size forgets it");
}

#[test]
fn shortcuts_can_be_changed_reset_and_are_kept() {
    let profile = TempProfile::new("shortcuts");
    let mut b = launch_with(&["--profile", profile.path()]);
    let list = b.bridge(&["keys.list"]);
    assert!(
        list.contains(r#"{"id":"newTab:#0","menu":"File","title":"New Tab","keys":"⌘T","custom":false}"#),
        "{list}"
    );
    assert!(
        list.contains(r#""id":"duplicateTab:#0","menu":"File","title":"Duplicate Tab","keys":"""#),
        "items without keys can get one"
    );
    assert_eq!(b.bridge(&["keys.set", "newTab:#0", "cmd+alt+shift+k"]), "true");
    assert_eq!(b.send("key cmd+t"), "KEY none", "the old keys are free");
    b.key("cmd+alt+shift+k");
    assert!(b.tabs().starts_with("TABS count=2"), "the new keys open a tab");
    let taken = b.send("bridge keys.set\tduplicateTab:#0\tcmd+alt+shift+k");
    assert!(taken.starts_with("BRIDGE err ⌥⇧⌘K is already File → New Tab"), "{taken}");
    assert!(b.send("bridge keys.set\tnewTab:#0\tk").starts_with("BRIDGE err a shortcut needs"));
    b.quit();
    let mut b = launch_with(&["--profile", profile.path()]);
    assert!(b.bridge(&["keys.list"]).contains(r#""title":"New Tab","keys":"⌥⇧⌘K","custom":true"#), "kept");
    assert_eq!(b.bridge(&["keys.reset", "newTab:#0"]), "true");
    b.key("cmd+t");
    assert!(b.tabs().starts_with("TABS count=2"), "back to ⌘T");
    assert_eq!(b.bridge(&["keys.set", "duplicateTab:#0", ""]), "true");
    b.quit();
}

#[test]
fn settings_shortcuts_open_internal_pages() {
    let mut b = launch();
    b.key("cmd+,");
    assert!(b.state_until(|s| s.ends_with("title=Settings")).ends_with("title=Settings"));
    assert!(b.tabs().starts_with("TABS count=2"));
    // Cmd+Y and Opt+Cmd+L reuse the same tab.
    b.key("cmd+y");
    assert!(b.state_until(|s| s.ends_with("title=History")).ends_with("title=History"));
    b.key("cmd+alt+l");
    assert!(b.state_until(|s| s.ends_with("title=Downloads")).ends_with("title=Downloads"));
    assert!(b.tabs().starts_with("TABS count=2"), "settings tab reused");
}

#[test]
fn background_tabs_sleep_and_wake() {
    let base = serve();
    let mut b = launch();
    b.nav(&format!("go {base}/page/first"));
    b.nav(&format!("newtab {base}/page/second"));
    assert!(b.tabs().starts_with("TABS count=2 active=1 asleep=0"));

    // The background tab unloads; the active one never does.
    assert!(b.send("sleep").starts_with("TABS count=2 active=1 asleep=1"));
    // Switching to it reloads its page.
    let url = b.nav("switch 0");
    assert_eq!(url, format!("{base}/page/first"));
    assert!(b.tabs().starts_with("TABS count=2 active=0 asleep=0"));
    assert!(b.state_until(|s| s.contains("title=first")).contains("title=first"));
    // And now the other one can sleep.
    assert!(b.send("sleep").contains("asleep=1"));
}

#[test]
fn tabs_list_their_memory_and_busy_background_tabs_are_marked_heavy() {
    let base = serve();
    let mut b = launch();
    // Spins forever once loaded: 100% CPU in the background.
    let spin = "addEventListener('load', () => setTimeout(() => { for (;;); }, 50))";
    b.nav(&format!("go {base}/script/{}", encode(spin)));
    b.nav(&format!("newtab {base}/page/calm"));
    assert_eq!(b.send("heavy"), "HEAVY 0 0", "no CPU history yet");
    std::thread::sleep(Duration::from_millis(1500));
    assert_eq!(b.send("heavy"), "HEAVY 1 0", "the spinning background tab is heavy, the active one never");
    let json = b.bridge(&["tabs.list"]);
    assert!(json.starts_with(r#"{"shell":{"footprint":"#), "{json}");
    assert!(json.contains(r#""heavy":"Uses "#) && json.contains(r#"% CPU in the background""#), "{json}");
    assert!(json.contains(r#""title":"calm","active":true,"asleep":false"#), "{json}");
    // Sleep the heavy one from the list: its process goes, and so does the badge.
    let id = json.split(r#"{"id":"#).nth(1).unwrap().split(',').next().unwrap().to_string();
    assert_eq!(b.bridge(&["tabs.sleep", &id]), "true");
    assert!(b.tabs().contains("asleep=1"));
    assert_eq!(b.send("heavy"), "HEAVY 0 0");
    assert_eq!(b.bridge(&["tabs.close", &id]), "true");
    assert!(b.tabs().starts_with("TABS count=1"));
    assert!(b.send("bridge tabs.show\t1").starts_with("BRIDGE err no such tab"));
}

// ---------------------------------------------------------------------------
// Profiles: cookies, history, downloads

#[test]
fn cookies_survive_a_restart() {
    let base = serve();
    let profile = TempProfile::new("cookies");
    let mut b = launch_with(&["--profile", profile.path()]);
    b.nav(&format!("go {base}/setcookie"));
    b.quit();

    let mut b = launch_with(&["--profile", profile.path()]);
    b.nav(&format!("go {base}/echo"));
    let s = b.state_until(|s| s.contains("title=cookie:"));
    assert!(s.contains("title=cookie:session=abc123"), "logged-in session must persist: {s}");
    b.quit();
}

#[test]
fn private_windows_keep_nothing_and_share_nothing() {
    let base = serve();
    let profile = TempProfile::new("private");
    let mut b = launch_with(&["--profile", profile.path()]);
    b.nav(&format!("go {base}/page/normal"));
    b.key("cmd+shift+n");
    assert_eq!(b.send("private"), "PRIVATE 1");
    assert!(b.tabs().ends_with("windows=2"), "private tabs get their own window");
    // The profile's cookies aren't visible in private, and private ones don't leak out.
    b.nav(&format!("go {base}/echo"));
    assert!(b.state_until(|s| s.contains("title=cookie:")).ends_with("title=cookie:none"));
    b.nav(&format!("go {base}/setcookie"));
    // Downloaded files stay, but aren't listed.
    let downloads = TempProfile::new("private-downloads");
    b.bridge(&["settings.set", "downloads_dir", downloads.path()]);
    writeln!(b.stdin, "go {base}/file.txt").unwrap();
    // The page load that became a download reports FAILED (interrupted), before or after.
    let line = b.wait_all(&["DOWNLOAD ", "FAILED "]).remove(0);
    assert_eq!(line, format!("DOWNLOAD done {}/file.txt", downloads.path()));
    assert!(downloads.0.join("file.txt").exists());
    assert_eq!(b.bridge(&["downloads.list"]), "[]");
    // A new tab in the private window shares its session.
    b.key("cmd+t");
    assert_eq!(b.send("private"), "PRIVATE 1");
    b.nav(&format!("go {base}/echo"));
    assert!(b.state_until(|s| s.contains("title=cookie:")).ends_with("title=cookie:session=abc123"));
    b.key("cmd+w");
    b.key("cmd+w");
    // Back in the normal window, and Reopen Closed Tab doesn't bring private tabs back.
    assert!(b.tabs().starts_with("TABS count=1 "));
    b.key("cmd+shift+t");
    assert!(b.tabs().starts_with("TABS count=1 "));
    assert_eq!(b.send("private"), "PRIVATE 0");
    // No private cookie here.
    b.nav(&format!("go {base}/echo"));
    assert!(b.state_until(|s| s.contains("title=cookie:")).ends_with("title=cookie:none"));
    // Closing the last private tab ended the private session.
    b.key("cmd+shift+n");
    b.nav(&format!("go {base}/echo"));
    assert!(b.state_until(|s| s.contains("title=cookie:")).ends_with("title=cookie:none"), "private data was dropped");
    // Nothing private reached history.
    let history = b.bridge(&["history.list", ""]);
    assert!(history.contains("/page/normal") && !history.contains("/setcookie"), "{history}");
    b.quit();
    // ...or the restored session.
    let mut b = launch_with(&["--profile", profile.path()]);
    assert!(b.tabs().starts_with("TABS count=1 "), "only the normal tab comes back");
    assert_eq!(b.send("private"), "PRIVATE 0");
    b.quit();
}

#[test]
fn throwaway_profile_keeps_nothing() {
    let base = serve();
    let mut b = launch();
    b.nav(&format!("go {base}/setcookie"));
    b.quit();
    let mut b = launch();
    b.nav(&format!("go {base}/echo"));
    let s = b.state_until(|s| s.contains("title=cookie:"));
    assert!(s.contains("title=cookie:none"), "{s}");
}

#[test]
fn one_browser_per_profile() {
    let profile = TempProfile::new("lock");
    let first = launch_with(&["--profile", profile.path()]);
    let (mut second, _stdin, _lines) = spawn(&["--profile", profile.path()]);
    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        if let Some(s) = second.try_wait().unwrap() {
            break s;
        }
        assert!(Instant::now() < deadline, "second browser didn't refuse the locked profile");
        std::thread::sleep(Duration::from_millis(50));
    };
    assert!(!status.success());
    let mut err = String::new();
    second.stderr.take().unwrap().read_to_string(&mut err).unwrap();
    assert!(err.contains("already running"), "{err}");
    first.quit();
}

#[test]
fn session_is_restored_asleep() {
    let base = serve();
    let profile = TempProfile::new("session");
    let mut b = launch_with(&["--profile", profile.path()]);
    b.nav(&format!("go {base}/page/alpha"));
    b.nav(&format!("newtab {base}/page/beta"));
    b.quit();

    let mut b = launch_with(&["--profile", profile.path()]);
    let tabs = b.tabs();
    // Same tabs, same order, the same one selected (beta); only it has a page loaded.
    assert!(tabs.starts_with("TABS count=2 active=1 asleep=1"), "{tabs}");
    assert!(b.state_until(|s| s.contains("title=beta")).contains("title=beta"));
    b.key("cmd+1");
    assert!(b.state_until(|s| s.contains("title=alpha")).contains("title=alpha"), "first tab wakes on click");
    b.quit();
}

#[test]
fn bookmarks_toggle_with_cmd_d_and_fill_the_menu() {
    let base = serve();
    let mut b = launch();
    b.nav(&format!("go {base}/page/saved"));
    // (The title can arrive after LOADED; the bookmark takes it.)
    b.state_until(|s| s.ends_with("title=saved"));
    assert_eq!(b.send("star"), "STAR on=0");
    b.key("cmd+d");
    assert_eq!(b.send("star"), "STAR on=1");
    let list = b.bridge(&["bookmarks.list", ""]);
    assert!(list.contains(&format!("\"url\":\"{base}/page/saved\",\"title\":\"saved\",\"folder\":\"\"")), "{list}");
    let id: String = list.split("\"id\":").nth(1).unwrap().chars().take_while(char::is_ascii_digit).collect();
    assert_eq!(b.send("bookmarksmenu"), "BOOKMARKSMENU Remove Bookmark|Show All Bookmarks|-|saved");
    // The newest first, wherever they are.
    b.nav(&format!("go {base}/page/second"));
    b.state_until(|s| s.ends_with("title=second"));
    b.key("cmd+d");
    b.bridge(&["bookmarks.update", &id, "Saved page", "Work/Docs"]);
    assert_eq!(b.send("bookmarksmenu"), "BOOKMARKSMENU Remove Bookmark|Show All Bookmarks|-|second|Saved page");
    // The History menu lists the latest pages, each once.
    assert_eq!(b.send("historymenu"), "HISTORYMENU Back|Forward|Show History|Downloads|-|second|saved");
    // Picking a bookmark opens it in the tab.
    b.nav(&format!("go {base}/page/elsewhere"));
    assert_eq!(b.send("star"), "STAR on=0");
    assert!(b.send("bookmarksmenu 3").starts_with("BOOKMARKSMENU Bookmark This Page|"));
    assert!(b.state_until(|s| s.ends_with("title=second")).ends_with("title=second"));
    assert_eq!(b.send("star"), "STAR on=1");
    // ⌘D on a bookmarked page removes it; so does deleting it in Settings.
    b.key("cmd+d");
    assert_eq!(b.send("star"), "STAR on=0");
    b.bridge(&["bookmarks.delete", &id]);
    assert_eq!(b.bridge(&["bookmarks.list", ""]), "[]");
    assert_eq!(b.send("bookmarksmenu"), "BOOKMARKSMENU Bookmark This Page|Show All Bookmarks");
    // Invalid input from the settings page is refused.
    assert!(b.send("bridge bookmarks.update\t1\tx\tA//B").starts_with("BRIDGE err"));
    b.quit();
}

#[test]
fn pages_print_to_pdf_and_window_print_opens_the_panel() {
    let base = serve();
    let dir = TempProfile::new("print");
    let mut b = launch();
    b.nav(&format!("go {base}/page/printable"));
    let pdf = dir.0.join("page.pdf");
    assert_eq!(b.send(&format!("printpdf {}", pdf.display())), "PRINTED 1");
    let bytes = std::fs::read(&pdf).unwrap();
    assert!(bytes.starts_with(b"%PDF") && bytes.len() > 500, "{} bytes", bytes.len());
    // The page's own print() shows the print panel; cancelling it prints nothing.
    b.nav(&format!("go {base}/script/{}", encode("setTimeout(() => print(), 10)")));
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        // The operation's end can be reported before or after the answer.
        match b.send("dialog cancel").as_str() {
            "ANSWERED 0" if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(100)),
            "ANSWERED 1" => break assert_eq!(b.next(), "PRINTED 0"),
            "PRINTED 0" => break assert_eq!(b.next(), "ANSWERED 1"),
            other => panic!("no print panel: {other}"),
        }
    }
    b.quit();
}

#[test]
fn pages_save_as_web_archives_that_open_offline() {
    let base = serve();
    let dir = TempProfile::new("webarchive");
    let mut b = launch();
    b.key("cmd+s"); // the menu item exists (the panel is a sheet; Esc it)
    b.send("dialog cancel");
    b.nav(&format!("go {base}/page/saved"));
    let file = format!("{}/saved.webarchive", dir.path());
    assert_eq!(b.send(&format!("savepage {file}")), "SAVED 1");
    assert!(std::fs::read(&file).unwrap().starts_with(b"bplist00"), "a Web Archive is a binary plist");
    b.nav(&format!("newtab file://{file}"));
    assert!(b.state_until(|s| s.ends_with("title=saved")).ends_with("title=saved"), "opens offline");
}

#[test]
fn link_menu_opens_links_in_tabs_windows_and_private_windows() {
    let base = serve();
    let mut b = launch();
    b.nav(&format!("go {base}/page/start"));
    let link = format!("{base}/page/linked");
    let menu = "LINKMENU Open Link|Open Link in New Tab|Open Link in New Window|Open Link in Private Window|Download Linked File|Copy Link";
    assert_eq!(b.send(&format!("linkmenu {link}")), menu);
    // Not a web link (javascript:, mailto:): WebKit's menu as it was.
    assert!(b.send("linkmenu javascript:alert(1)").contains("|Open Link in New Window|Download"));
    // New tab: in the background, next to this one.
    assert_eq!(b.send(&format!("linkmenu {link} 1")), menu);
    assert!(b.tabs().starts_with("TABS count=2 active=0 asleep=0 windows=1"));
    // New window: a second window, in front.
    b.send(&format!("linkmenu {link} 2"));
    assert!(b.tabs().starts_with("TABS count=1 active=0 asleep=0 windows=2"));
    assert!(b.state_until(|s| s.ends_with("title=linked")).ends_with("title=linked"));
    // Private window: a third, private.
    b.send(&format!("linkmenu {link} 3"));
    assert!(b.tabs().ends_with("windows=3"));
    assert_eq!(b.send("private"), "PRIVATE 1");
    // From a private tab, new tabs and windows stay private (no "Private Window" item).
    assert_eq!(
        b.send(&format!("linkmenu {link} 1")),
        "LINKMENU Open Link|Open Link in New Tab|Open Link in New Window|Download Linked File|Copy Link"
    );
    assert!(b.tabs().starts_with("TABS count=2 active=0"));
    b.quit();
}

#[test]
fn tabs_duplicate_close_others_move_and_merge() {
    let base = serve();
    let mut b = launch();
    b.nav(&format!("go {base}/page/one"));
    assert!(b.send("menu File/Duplicate Tab").starts_with("MENU "));
    assert!(b.tabs().starts_with("TABS count=2 active=1"));
    assert!(b.state_until(|s| s.ends_with("title=one")).ends_with("title=one"));
    b.key("cmd+t");
    b.key("cmd+alt+w");
    assert!(b.tabs().starts_with("TABS count=1 active=0 asleep=0 windows=1"), "Close Other Tabs");
    b.key("cmd+t");
    b.send("menu Window/Move Tab to New Window");
    assert!(b.tabs().starts_with("TABS count=1 active=0 asleep=0 windows=2"));
    b.send("menu Window/Merge All Windows");
    let tabs = b.tabs();
    assert!(tabs.starts_with("TABS count=2 ") && tabs.ends_with("windows=1"), "{tabs}");
    b.quit();
}

#[test]
fn every_window_comes_back_after_a_restart() {
    let base = serve();
    let profile = TempProfile::new("windows");
    let mut b = launch_with(&["--profile", profile.path()]);
    b.nav(&format!("go {base}/page/a"));
    b.send(&format!("newtab {base}/page/b"));
    b.key("cmd+n");
    b.nav(&format!("go {base}/page/c"));
    assert!(b.tabs().ends_with("windows=2"));
    b.quit();
    let mut b = launch_with(&["--profile", profile.path()]);
    // The window that was in front comes back in front, with its one tab awake.
    assert!(b.tabs().starts_with("TABS count=1 active=0 asleep=2 windows=2"), "{}", b.tabs());
    assert!(b.state_until(|s| s.ends_with("title=c")).ends_with("title=c"));
    b.key("cmd+shift+]");
    assert!(b.state_until(|s| s.ends_with("title=c")).ends_with("title=c"), "one tab in this window");
    b.quit();
}

#[test]
fn video_fullscreen_picture_in_picture_and_screen_sharing() {
    let base = serve();
    // Hidden pages can't share their screen: this test needs a visible window.
    let mut b = launch_with(&["--on-top"]);
    b.nav(&format!("go {base}/page/media"));
    // Video sites' fullscreen buttons and picture in picture work.
    assert_eq!(b.send("eval document.fullscreenEnabled"), "EVAL 1");
    assert_eq!(b.send("eval document.pictureInPictureEnabled"), "EVAL 1");
    assert_eq!(
        b.send("menu View/Picture in Picture").split('|').next(),
        Some("MENU Reload Page"),
        "no video: nothing happens"
    );
    // Screen sharing asks first; "Don't Allow" reaches the page as NotAllowedError.
    let share = "eval navigator.mediaDevices.getDisplayMedia({video: true}).then(() => document.title = 'shared', \
                 e => document.title = 'refused ' + e.name), 1";
    // Other tests' floating windows can cover this one, and a covered page
    // counts as hidden: wait until the page is really visible.
    let deadline = Instant::now() + Duration::from_secs(20);
    while b.send("eval document.visibilityState") != "EVAL visible" {
        assert!(Instant::now() < deadline, "the window never became visible");
        std::thread::sleep(Duration::from_millis(100));
    }
    writeln!(b.stdin, "{share}").unwrap();
    assert!(b.wait_for("DIALOG").ends_with("127.0.0.1 wants to share your screen"));
    // (The script's EVAL line can come before or after the prompt.)
    writeln!(b.stdin, "dialog cancel").unwrap();
    assert_eq!(b.wait_for("ANSWERED"), "ANSWERED 1");
    assert!(b.state_until(|s| s.ends_with("title=refused NotAllowedError")).ends_with("refused NotAllowedError"));
    // A site set to Block isn't asked.
    b.bridge(&["perm.set", "127.0.0.1", "screen", "deny"]);
    b.send("eval document.title = 'again', 1");
    assert_eq!(b.send(share), "EVAL 1");
    assert!(b.state_until(|s| s.ends_with("title=refused NotAllowedError")).ends_with("refused NotAllowedError"));
    b.quit();
}

#[test]
fn reader_mode_shows_the_article_alone_without_scripts() {
    let base = serve();
    let mut b = launch();
    let para = "Lightweight browsers keep memory low, and readers want the words, not the clutter around them. ";
    let page = format!(
        "addEventListener('DOMContentLoaded', () => {{ document.title = 'Story page'; document.body.innerHTML = \
         '<nav class=menu><a href=/a>Home</a> <a href=/b>Sections</a></nav>\
          <div class=sidebar-ads><p>Buy things now, buy them today, buy them all, they are on sale here.</p></div>\
          <article><h1>Big story</h1><p>{p}{p}</p><p>{p}</p><img data-src=/page/img src=data:,><p>{p}{p}</p></article>\
          <footer>Copyright</footer>'; setInterval(() => document.title = 'Story page', 50); }});",
        p = para
    );
    b.nav(&format!("go {base}/script/{}", encode(&page)));
    assert_eq!(b.send("reader"), "READER on");
    assert!(b.state_until(|s| s.ends_with("title=Big story")).ends_with("title=Big story"));
    let text = b.send("eval document.body.innerText");
    assert!(text.contains("Big story") && text.contains("readers want the words"), "{text}");
    assert!(!text.contains("Sections") && !text.contains("Buy things") && !text.contains("Copyright"), "{text}");
    // The address stays the page's, relative links and lazy images resolve, and no script runs.
    assert!(b.send("state").contains(&format!("url={base}/script/")));
    assert_eq!(b.send("eval document.querySelector('img').src"), format!("EVAL {base}/page/img"));
    assert_eq!(b.send("eval document.scripts.length"), "EVAL 0");
    // Again: back to the page.
    assert_eq!(b.send("reader"), "READER off");
    assert!(b.state_until(|s| s.ends_with("title=Story page")).ends_with("title=Story page"));
    // A page without an article: nothing to show.
    b.nav(&format!("go {base}/page/short"));
    assert_eq!(b.send("reader"), "READER none");
    b.quit();
}

#[test]
fn passwords_are_offered_saved_and_filled_only_when_picked() {
    let base = serve();
    let mut b = launch();
    let login = format!(
        "go {base}/script/{}",
        encode(
            "addEventListener('DOMContentLoaded', () => { document.title = 'login'; document.body.innerHTML = \
             '<form action=/page/welcome><input name=username id=u><input type=password name=password id=p>\
             <button id=b>Sign in</button></form>' })"
        )
    );
    b.nav(&login);
    // The page can't reach the browser's password handler: it lives in another script world.
    assert_eq!(b.send("eval typeof webkit.messageHandlers.passwords"), "EVAL undefined");
    // Signing in offers to save.
    writeln!(b.stdin, "eval u.value = 'alice'; p.value = 's3cret'; b.click(), 1").unwrap();
    assert_eq!(b.wait_all(&["DIALOG", "EVAL"])[0], "DIALOG Save the password for alice on 127.0.0.1?");
    assert_eq!(b.send("dialog ok"), "ANSWERED 1");
    assert_eq!(b.bridge(&["passwords.list"]), r#"[{"site":"127.0.0.1","user":"alice"}]"#);
    // (The form went on to its welcome page, `welcome?username=…`; let it land before leaving.)
    assert!(b.state_until(|s| s.contains("title=welcome")).contains("title=welcome"));
    // Focusing the login field offers the account; picking it fills the form.
    b.nav(&login);
    writeln!(b.stdin, "eval u.focus(), 1").unwrap();
    assert_eq!(b.wait_all(&["PASSWORDS", "EVAL"])[0], "PASSWORDS alice");
    assert_eq!(b.send("fillpassword 0"), "FILLPASSWORD");
    let deadline = Instant::now() + Duration::from_secs(5);
    while b.send("eval [u.value, p.value].join()") != "EVAL alice,s3cret" {
        assert!(Instant::now() < deadline, "not filled");
        std::thread::sleep(Duration::from_millis(100));
    }
    // The same password again isn't offered for saving.
    assert_eq!(b.send("eval b.click(), 1"), "EVAL 1");
    std::thread::sleep(Duration::from_millis(300));
    assert!(b.tabs().starts_with("TABS"), "no second prompt");
    // "Never for this site" sticks.
    assert!(b.state_until(|s| s.contains("title=welcome")).contains("title=welcome"));
    b.nav(&login);
    writeln!(b.stdin, "eval u.value = 'bob'; p.value = 'x'; b.click(), 1").unwrap();
    assert!(b.wait_all(&["DIALOG", "EVAL"])[0].starts_with("DIALOG Save the password for bob"));
    assert_eq!(b.send("dialog cancel check"), "ANSWERED 1");
    assert!(b.bridge(&["perm.list"]).contains(r#""kind":"passwords","value":"deny""#));
    assert!(b.state_until(|s| s.contains("title=welcome")).contains("title=welcome"));
    b.nav(&login);
    assert_eq!(b.send("eval u.value = 'carol'; p.value = 'y'; b.click(), 1"), "EVAL 1");
    std::thread::sleep(Duration::from_millis(300));
    assert!(b.tabs().starts_with("TABS"), "never asked again");
    // CSV export from another browser: HTTPS logins come in, plain HTTP ones don't.
    let dir = TempProfile::new("passwords-csv");
    let csv = dir.0.join("export.csv");
    std::fs::write(
        &csv,
        "name,url,username,password,note\nx,https://shop.example/login,dave,pw1,\ny,http://plain.example/,erin,pw2,\n",
    )
    .unwrap();
    assert_eq!(b.bridge(&["passwords.import", csv.to_str().unwrap()]), r#"{"imported":1,"skipped":1}"#);
    assert!(b.bridge(&["passwords.list"]).contains(r#"{"site":"shop.example","user":"dave"}"#));
    b.bridge(&["passwords.delete", "shop.example", "dave"]);
    assert!(!b.bridge(&["passwords.list"]).contains("dave"));
    b.quit();
}

#[test]
fn passwords_work_in_frames_for_the_frames_site() {
    let base = serve();
    let mut b = launch();
    let form = encode(
        "addEventListener('DOMContentLoaded', () => { document.body.innerHTML = \
         '<form action=/page/welcome><input name=username id=u><input type=password name=password id=p>\
         <button id=b>Sign in</button></form>' })",
    );
    let page = format!(
        "{base}/script/{}",
        encode(&format!(
            "addEventListener('DOMContentLoaded', () => {{ document.title = 'top'; document.body.innerHTML = '<iframe id=f src=/script/{form}></iframe>' }})"
        ))
    );
    b.nav(&format!("go {page}"));
    let frame = "document.getElementById('f').contentDocument";
    let deadline = Instant::now() + Duration::from_secs(5);
    while b.send(&format!("eval !!{frame}.getElementById('b')")) != "EVAL 1" {
        assert!(Instant::now() < deadline, "frame never loaded");
        std::thread::sleep(Duration::from_millis(100));
    }
    writeln!(b.stdin, "eval (d => {{ d.getElementById('u').value = 'erin'; d.getElementById('p').value = 'pw'; d.getElementById('b').click() }})({frame}), 1").unwrap();
    assert_eq!(
        b.wait_all(&["DIALOG", "EVAL"])[0],
        "DIALOG Save the password for erin on 127.0.0.1?",
        "saved from the frame"
    );
    assert_eq!(b.send("dialog ok"), "ANSWERED 1");
    b.nav(&format!("go {page}"));
    let deadline = Instant::now() + Duration::from_secs(5);
    while b.send(&format!("eval !!{frame}.getElementById('u')")) != "EVAL 1" {
        assert!(Instant::now() < deadline, "frame never loaded");
        std::thread::sleep(Duration::from_millis(100));
    }
    writeln!(b.stdin, "eval {frame}.getElementById('u').focus(), 1").unwrap();
    assert_eq!(b.wait_all(&["PASSWORDS", "EVAL"])[0], "PASSWORDS erin", "offered in the frame");
    assert_eq!(b.send("fillpassword 0"), "FILLPASSWORD");
    let deadline = Instant::now() + Duration::from_secs(5);
    while b.send(&format!("eval (d => [d.getElementById('u').value, d.getElementById('p').value].join())({frame})"))
        != "EVAL erin,pw"
    {
        assert!(Instant::now() < deadline, "not filled in the frame");
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn private_windows_fill_passwords_but_never_save_them() {
    let base = serve();
    let mut b = launch();
    let login = format!(
        "go {base}/script/{}",
        encode(
            "addEventListener('DOMContentLoaded', () => { document.body.innerHTML = \
             '<form action=/page/welcome><input name=email id=u><input type=password id=p><button id=b>Go</button></form>' })"
        )
    );
    b.key("cmd+shift+n");
    b.nav(&login);
    assert_eq!(b.send("eval u.value = 'pat'; p.value = 'z'; b.click(), 1"), "EVAL 1");
    std::thread::sleep(Duration::from_millis(300));
    assert!(b.tabs().starts_with("TABS"), "not offered in a private window");
    assert_eq!(b.bridge(&["passwords.list"]), "[]");
    b.quit();
}

#[test]
fn bookmarks_and_history_import_from_other_browsers() {
    // A made-up home folder with a Brave profile and Safari's files: the
    // browser looks for other browsers under $HOME, never the real ones here.
    let home = TempProfile::new("import-home");
    let lib = home.0.join("Library");
    let brave = lib.join("Application Support/BraveSoftware/Brave-Browser/Default");
    std::fs::create_dir_all(&brave).unwrap();
    std::fs::write(
        brave.join("Bookmarks"),
        r#"{"roots":{"bookmark_bar":{"type":"folder","children":[{"type":"url","name":"Rust","url":"https://rust-lang.org/"},
           {"type":"folder","name":"Work","children":[{"type":"url","name":"Docs","url":"https://docs.example/"}]}]},
           "other":{"type":"folder","children":[]}}}"#,
    )
    .unwrap();
    let db = rusqlite::Connection::open(brave.join("History")).unwrap();
    db.execute_batch(
        "CREATE TABLE urls(id INTEGER PRIMARY KEY, url TEXT, title TEXT);
         CREATE TABLE visits(id INTEGER PRIMARY KEY, url INTEGER, visit_time INTEGER);
         INSERT INTO urls VALUES (1, 'https://visited.example/', 'Visited in Brave'), (2, 'chrome://settings', 'x');
         INSERT INTO visits VALUES (1, 1, 13344473600000000), (2, 2, 13344473600000000);",
    )
    .unwrap();
    drop(db);
    let logins = rusqlite::Connection::open(brave.join("Login Data")).unwrap();
    logins
        .execute_batch("CREATE TABLE logins(origin_url TEXT, username_value TEXT, password_value BLOB, blacklisted_by_user INTEGER);")
        .unwrap();
    drop(logins);
    let safari = lib.join("Safari");
    std::fs::create_dir_all(&safari).unwrap();
    std::fs::write(
        safari.join("Bookmarks.plist"),
        r#"<?xml version="1.0" encoding="UTF-8"?><plist version="1.0"><dict><key>WebBookmarkType</key><string>WebBookmarkTypeList</string>
           <key>Children</key><array><dict><key>Title</key><string>BookmarksBar</string><key>WebBookmarkType</key><string>WebBookmarkTypeList</string>
           <key>Children</key><array><dict><key>WebBookmarkType</key><string>WebBookmarkTypeLeaf</string><key>URLString</key>
           <string>https://safari.example/</string><key>URIDictionary</key><dict><key>title</key><string>From Safari</string></dict></dict>
           </array></dict></array></dict></plist>"#,
    )
    .unwrap();
    let db = rusqlite::Connection::open(safari.join("History.db")).unwrap();
    db.execute_batch(
        "CREATE TABLE history_items(id INTEGER PRIMARY KEY, url TEXT);
         CREATE TABLE history_visits(id INTEGER PRIMARY KEY, history_item INTEGER, visit_time REAL, title TEXT);
         INSERT INTO history_items VALUES (1, 'https://safari-visit.example/');
         INSERT INTO history_visits VALUES (1, 1, 721692800.5, 'Visited in Safari');",
    )
    .unwrap();
    drop(db);

    let mut b = launch_env(&[], &[("HOME", home.path())]);
    assert_eq!(b.bridge(&["import.sources"]), r#"[{"id":"brave","name":"Brave"},{"id":"safari","name":"Safari"}]"#);
    assert_eq!(b.bridge(&["import.run", "brave", "bookmarks"]), r#"{"added":2}"#);
    assert_eq!(b.bridge(&["import.run", "brave", "bookmarks"]), r#"{"added":0}"#, "twice adds nothing twice");
    assert_eq!(b.bridge(&["import.run", "brave", "history"]), r#"{"added":1}"#, "web pages only");
    assert_eq!(b.bridge(&["import.run", "brave", "passwords"]), r#"{"added":0}"#);
    // A file macOS won't let us read (other browsers' data since macOS 15):
    // the message names Full Disk Access, which the page offers to open.
    use std::os::unix::fs::PermissionsExt;
    let history = brave.join("History");
    std::fs::set_permissions(&history, std::fs::Permissions::from_mode(0o000)).unwrap();
    let refused = b.send("bridge import.run\tbrave\thistory");
    std::fs::set_permissions(&history, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert!(refused.starts_with("BRIDGE err ") && refused.contains("Full Disk Access"), "{refused}");
    assert_eq!(b.bridge(&["import.allow"]), "true");
    assert_eq!(b.bridge(&["import.run", "safari", "bookmarks"]), r#"{"added":1}"#);
    assert_eq!(b.bridge(&["import.run", "safari", "history"]), r#"{"added":1}"#);
    let bookmarks = b.bridge(&["bookmarks.list", ""]);
    assert!(
        bookmarks.contains(
            r#""url":"https://docs.example/","title":"Docs","folder":"Imported from Brave/Bookmarks Bar/Work""#
        ),
        "{bookmarks}"
    );
    assert!(bookmarks.contains(r#""title":"From Safari","folder":"Imported from Safari/Favorites""#), "{bookmarks}");
    let history = b.bridge(&["history.list", ""]);
    // Both visits happened at 2023-11-14 22:13:20 UTC, and keep that time.
    assert!(
        history.contains(r#""url":"https://visited.example/","title":"Visited in Brave","at":1700000000"#),
        "{history}"
    );
    assert!(
        history.contains(r#""url":"https://safari-visit.example/","title":"Visited in Safari","at":1700000000"#),
        "{history}"
    );
    // A bookmarks file exported by any browser.
    let file = home.0.join("bookmarks.html");
    std::fs::write(
        &file,
        r#"<DL><p><DT><H3>Saved</H3><DL><p><DT><A HREF="https://file.example/">From a file</A></DL><p></DL>"#,
    )
    .unwrap();
    assert_eq!(b.bridge(&["import.file", file.to_str().unwrap()]), r#"{"added":1}"#);
    assert!(b.bridge(&["bookmarks.list", "file"]).contains(r#""folder":"Imported/Saved""#));
    // Safari's passwords come through the Passwords app's export instead.
    assert!(b.send("bridge import.run\tsafari\tpasswords").starts_with("BRIDGE err Import Safari's passwords"));
    b.quit();
}

#[test]
fn history_is_recorded_searchable_and_clearable() {
    let base = serve();
    let mut b = launch();
    b.nav(&format!("go {base}/page/needle"));
    b.nav(&format!("go {base}/page/haystack"));
    let all = b.bridge(&["history.list", ""]);
    assert!(all.contains(&format!("\"url\":\"{base}/page/needle\"")), "{all}");
    assert!(all.contains("\"title\":\"needle\""), "title filled in: {all}");
    let found = b.bridge(&["history.list", "needle"]);
    assert!(found.contains("needle") && !found.contains("haystack"), "{found}");
    // Internal and data: pages are not history.
    b.nav("go data:text/html,<title>x</title>");
    assert!(!b.bridge(&["history.list", ""]).contains("data:"));
    b.bridge(&["history.clear"]);
    assert_eq!(b.bridge(&["history.list", ""]), "[]");
}

#[test]
fn downloads_go_to_the_chosen_folder() {
    let base = serve();
    let dir = TempProfile::new("downloads");
    let mut b = launch();
    assert_eq!(b.bridge(&["settings.set", "downloads_dir", dir.path()]), "true");
    writeln!(b.stdin, "go {base}/file.txt").unwrap();
    let line = b.wait_for("DOWNLOAD ");
    let path = line.strip_prefix("DOWNLOAD done ").unwrap_or_else(|| panic!("{line}"));
    assert_eq!(Path::new(path), dir.0.join("file.txt"));
    assert_eq!(std::fs::read_to_string(path).unwrap(), "hello download");
    // Marked as downloaded from the internet (WebKit does it), so Gatekeeper
    // checks it before it can run.
    let q = Command::new("/usr/bin/xattr").args(["-p", "com.apple.quarantine", path]).output().unwrap();
    assert!(q.status.success() && q.stdout.starts_with(b"00"), "{q:?}");

    // Same name again: kept apart, not overwritten.
    writeln!(b.stdin, "go {base}/file.txt").unwrap();
    let line = b.wait_for("DOWNLOAD ");
    assert!(line.ends_with("file (2).txt"), "{line}");

    let list = b.bridge(&["downloads.list"]);
    assert!(list.contains("\"status\":\"done\"") && list.contains("\"bytes\":14"), "{list}");
    // Quick Look (reported in bench mode: the panel would take focus).
    let id = list.split("\"id\":").nth(1).unwrap().split(',').next().unwrap().to_string();
    assert!(b.send(&format!("bridge downloads.preview\t{id}")).starts_with("PREVIEW "));
    assert_eq!(b.next(), "BRIDGE ok true");
}

#[test]
fn site_data_is_listed_per_site_and_deleted_one_site_at_a_time() {
    let base = serve();
    let mut b = launch();
    b.nav(&format!("go {base}/setcookie"));
    b.nav(&format!("go {base}/script/{}", encode("localStorage.setItem('k', 'v')")));
    let list = b.bridge(&["sitedata.list"]);
    assert!(list.contains(r#"{"name":"127.0.0.1","kinds":["#) && list.contains("Cookies"), "{list}");
    assert!(b.send("sitedata").starts_with("SITEDATA 127.0.0.1:"), "the site menu's sheet lists it");
    assert_eq!(b.send("dialog ok"), "ANSWERED 1");
    assert_eq!(b.bridge(&["sitedata.delete", "127.0.0.1"]), "true");
    assert!(!b.bridge(&["sitedata.list"]).contains("127.0.0.1"));
    b.nav(&format!("go {base}/echo"));
    assert!(b.state_until(|s| s.contains("title=cookie:")).ends_with("title=cookie:none"), "signed out");
    assert!(b.send("bridge sitedata.delete\tnothing.example").starts_with("BRIDGE err no data"));
}

#[test]
fn clearing_site_data_signs_out() {
    let base = serve();
    let mut b = launch();
    b.nav(&format!("go {base}/setcookie"));
    b.nav(&format!("go {base}/echo"));
    assert!(b.state_until(|s| s.contains("title=cookie:")).contains("cookie:session=abc123"));
    b.bridge(&["data.clear"]);
    b.nav(&format!("go {base}/echo"));
    assert!(b.state_until(|s| s.contains("title=cookie:")).contains("title=cookie:none"));
}

// ---------------------------------------------------------------------------
// Settings page and its bridge

#[test]
fn settings_page_loads_and_its_bridge_works() {
    let mut b = launch();
    b.nav("go mybrowser://settings");
    // The page's own script set the title after reading settings over the bridge.
    assert!(b.state_until(|s| s.ends_with("title=Settings")).ends_with("title=Settings"));
    // The sidebar's #links switch sections without a reload.
    b.send("eval document.querySelector('nav a[href=\"#history\"]').click()");
    assert!(b.state_until(|s| s.ends_with("title=History")).ends_with("title=History"));
    let s = b.bridge(&["settings.get"]);
    assert!(s.contains("\"search\":\"duckduckgo\"") && s.contains("\"block_trackers\":\"1\""), "{s}");
    assert_eq!(b.bridge(&["settings.set", "search", "brave"]), "true");
    assert!(b.bridge(&["settings.get"]).contains("\"search\":\"brave\""));
    let bad = b.send("bridge settings.set\tsearch\tevil.example");
    assert!(bad.starts_with("BRIDGE err "), "{bad}");
    // Settings → About shows the version "Check for updates" compares against.
    assert_eq!(b.bridge(&["app.version"]), format!("\"{}\"", env!("CARGO_PKG_VERSION")));
    // Settings → Shortcuts lists the menu bar's keys.
    let keys = b.bridge(&["keys.list"]);
    assert!(keys.contains("\"menu\":\"File\",\"title\":\"New Tab\",\"keys\":\"⌘T\""), "{keys}");
    assert!(keys.contains("\"title\":\"New Private Window\",\"keys\":\"⇧⌘N\""), "{keys}");
    assert!(keys.contains("\"title\":\"Next Tab\",\"keys\":\"⌃⇥\""), "{keys}");
    let unknown = b.send("bridge nope");
    assert!(unknown.starts_with("BRIDGE err unknown op"), "{unknown}");
}

#[test]
fn web_pages_cannot_use_the_bridge_or_open_internal_pages() {
    let base = serve();
    let mut b = launch();
    // A website calling the bridge is refused.
    let js = "window.webkit.messageHandlers.browser.postMessage(['history.list','']).then(()=>document.title='allowed',()=>document.title='denied')";
    b.nav(&format!("go {base}/script/{}", encode(js)));
    assert!(b.state_until(|s| s.contains("title=denied") || s.contains("title=allowed")).contains("title=denied"));

    // A website navigating itself to an internal page is stopped.
    let js = "location.href='mybrowser://settings'";
    b.nav(&format!("go {base}/script/{}", encode(js)));
    std::thread::sleep(Duration::from_millis(500));
    let s = b.send("state");
    assert!(s.contains(&format!("url={base}/script/")) && s.contains("title=start"), "{s}");

    // Nor can it frame one: the frame isn't trusted even though it's mybrowser:.
    let js = "const f=document.createElement('iframe');f.src='mybrowser://settings';document.body.append(f)";
    b.nav(&format!("go {base}/script/{}", encode(js)));
    std::thread::sleep(Duration::from_millis(500));
    assert!(b.send("state").contains("title=start"));
}

#[test]
fn new_tab_page_shows_most_visited_sites() {
    let base = serve();
    let mut b = launch();
    b.nav(&format!("go {base}/page/one"));
    // A new tab gets the native page: no web view, an empty address bar.
    b.key("cmd+t");
    assert!(b.state_until(|s| s.ends_with("title=New Tab")).contains("url= title=New Tab"));
    assert_eq!(b.send("home"), format!("HOME shown=1 art=1 search=1 sites={base}/"));
    assert_eq!(b.bridge(&["settings.set", "home_top_sites", "0"]), "true");
    assert_eq!(b.bridge(&["settings.set", "home_art", "0"]), "true");
    b.key("cmd+t");
    assert_eq!(b.send("home"), "HOME shown=1 art=0 search=1 sites=");
    // Loading a page replaces it.
    b.nav(&format!("go {base}/page/two"));
    assert_eq!(b.send("home"), "HOME shown=0");
    assert_eq!(b.bridge(&["settings.set", "theme", "dark"]), "true");
    assert!(b.send("bridge settings.set\ttheme\tpurple").starts_with("BRIDGE err "));
}

#[test]
fn home_button_shows_the_new_tab_page() {
    let base = serve();
    let mut b = launch();
    b.nav(&format!("go {base}/page/one"));
    // The page goes (no web view), the new tab page takes its place.
    assert!(b.send("gohome").starts_with("HOME shown=1 "));
    assert_eq!(b.send("state"), "STATE back=0 forward=0 url= title=New Tab");
    assert_eq!(b.send("tabs"), "TABS count=1 active=0 asleep=0 windows=1");
    // Pressing it again changes nothing; loading a page replaces it.
    assert!(b.send("gohome").starts_with("HOME shown=1 "));
    b.nav(&format!("go {base}/page/two"));
    assert_eq!(b.send("home"), "HOME shown=0");
}

// ---------------------------------------------------------------------------
// Attacks and protections

#[test]
fn pages_cannot_trap_the_tab_with_dialogs() {
    let base = serve();
    let mut b = launch();
    let js = "alert('one');alert('two');document.title='after '+confirm('three')";
    writeln!(b.stdin, "go {base}/script/{}", encode(js)).unwrap();
    assert_eq!(b.wait_for("DIALOG "), "DIALOG one");
    assert_eq!(b.send("dialog ok"), "ANSWERED 1");
    // From the second dialog on, the user can block the rest.
    assert_eq!(b.wait_for("DIALOG "), "DIALOG two");
    assert_eq!(b.send("dialog ok check"), "ANSWERED 1");
    // The confirm() never shows; the page gets "cancel" and carries on.
    assert!(b.state_until(|s| s.ends_with("title=after false")).ends_with("title=after false"));
    // A new page may show dialogs again.
    writeln!(b.stdin, "go {base}/script/{}", encode("alert('again')")).unwrap();
    assert_eq!(b.wait_for("DIALOG "), "DIALOG again");
    assert_eq!(b.send("dialog ok"), "ANSWERED 1");
}

#[test]
fn quitting_closes_an_open_prompt_first() {
    let mut b = launch();
    b.nav("go about:blank");
    b.send("eval setTimeout(() => prompt('name?'), 10)");
    assert!(b.wait_for("DIALOG").contains("name?"));
    // AppKit refused to quit while a sheet with a text field was up.
    b.quit();
}

#[test]
fn address_bar_cannot_be_spoofed_by_a_slow_navigation() {
    let base = serve();
    let other = base.replace("127.0.0.1", "localhost");
    let mut b = launch();
    // The page sends itself to a slow "bank" and keeps showing its own content meanwhile.
    let page = format!("{base}/script/{}", encode(&format!("setTimeout(()=>location='{other}/slow/bank',50)")));
    b.nav(&format!("go {page}"));
    std::thread::sleep(Duration::from_millis(600));
    let s = b.send("state");
    assert!(s.contains(&format!("url={page}")) && s.ends_with("title=start"), "the bar shows the page on screen: {s}");
    assert!(b.state_until(|s| s.ends_with("title=bank")).contains(&format!("url={other}/slow/bank")));
    // What the user types shows right away, before it loads.
    writeln!(b.stdin, "go {other}/slow/typed").unwrap();
    std::thread::sleep(Duration::from_millis(300));
    assert!(b.send("state").contains(&format!("url={other}/slow/typed")));
}

#[test]
fn a_page_needs_permission_for_more_than_one_download() {
    let base = serve();
    let dir = TempProfile::new("multi-downloads");
    let mut b = launch();
    assert_eq!(b.bridge(&["settings.set", "downloads_dir", dir.path()]), "true");
    let js = "const d=()=>{const a=document.createElement('a');a.href='/file.txt';a.download='f.txt';a.click()};d();setTimeout(d,700)";
    writeln!(b.stdin, "go {base}/script/{}", encode(js)).unwrap();
    assert!(b.wait_for("DOWNLOAD ").starts_with("DOWNLOAD done"), "the first one just downloads");
    assert_eq!(b.wait_for("DIALOG "), "DIALOG 127.0.0.1 wants to download multiple files");
    assert_eq!(b.send("dialog cancel check"), "ANSWERED 1");
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(std::fs::read_dir(&dir.0).unwrap().count(), 1, "the second was refused");
    assert!(b.bridge(&["perm.list"]).contains("\"kind\":\"downloads\",\"value\":\"deny\""), "and remembered");
}

#[test]
fn failed_loads_show_an_error_page() {
    let mut b = launch();
    let failed = b.send("go http://127.0.0.1:1/");
    assert!(failed.starts_with("FAILED "), "{failed}");
    let s = b.state_until(|s| s.ends_with("title=Can't open this page"));
    assert!(s.contains("url=http://127.0.0.1:1/") && s.ends_with("title=Can't open this page"), "{s}");
    assert!(b.bridge(&["history.list", ""]) == "[]", "an error page isn't a visit");
}

#[test]
fn websites_cannot_reach_the_local_network() {
    let base = serve();
    let mut b = launch();
    // A page that isn't local (a data: URL) tries to reach a server on this Mac.
    let probe = format!(
        "fetch('{base}/page/x',{{mode:'no-cors'}}).then(()=>document.title='reached',()=>document.title='blocked')"
    );
    let remote = format!("go data:text/html,{}", encode(&format!("<title>start</title><script>{probe}</script>")));
    let outcome = |b: &mut Browser| b.state_until(|s| s.contains("title=reached") || s.contains("title=blocked"));
    b.nav(&remote);
    assert!(outcome(&mut b).ends_with("title=blocked"));
    // A local page may talk to local servers.
    b.nav(&format!("go {base}/script/{}", encode(&probe)));
    assert!(outcome(&mut b).ends_with("title=reached"));
    // The switch in Privacy settings turns the shield off.
    assert_eq!(b.bridge(&["settings.set", "block_local_network", "0"]), "true");
    b.nav(&remote);
    assert!(outcome(&mut b).ends_with("title=reached"));
}

#[test]
fn global_privacy_control_is_sent() {
    let base = serve();
    let mut b = launch();
    b.nav(&format!("go {base}/script/{}", encode("document.title='gpc '+navigator.globalPrivacyControl")));
    assert!(b.state_until(|s| s.ends_with("title=gpc true")).ends_with("title=gpc true"));
}

#[test]
fn tracking_parameters_are_removed_unless_shields_are_down() {
    let base = serve();
    let mut b = launch();
    assert_eq!(b.nav(&format!("go {base}/page/x?fbclid=abc&keep=1&gclid=z")), format!("{base}/page/x?keep=1"));
    // Shields down for this site: the address is left alone.
    assert_eq!(b.bridge(&["perm.set", "127.0.0.1", "shields", "off"]), "true");
    // WebKit's own link filtering follows the page being left, so (like the
    // site menu) reload once under the new setting.
    b.nav("reload");
    assert!(b.bridge(&["perm.list"]).contains("\"site\":\"127.0.0.1\",\"kind\":\"shields\",\"value\":\"off\""));
    assert_eq!(b.nav(&format!("go {base}/page/x?fbclid=abc")), format!("{base}/page/x?fbclid=abc"));
    assert_eq!(b.bridge(&["perm.forget", "127.0.0.1"]), "true");
    assert_eq!(b.bridge(&["perm.list"]), "[]");
    let bad = b.send("bridge perm.set\t127.0.0.1\tcamera\tmaybe");
    assert!(bad.starts_with("BRIDGE err "), "{bad}");
}

#[test]
fn cookie_banners_are_hidden_unless_turned_off() {
    let base = serve();
    let mut b = launch();
    let js = "addEventListener('DOMContentLoaded', () => { const d = document.createElement('div'); \
              d.id = 'onetrust-banner-sdk'; d.textContent = 'We use cookies'; document.body.append(d); \
              setTimeout(() => document.title = getComputedStyle(d).display, 100) })";
    let page = format!("go {base}/script/{}", encode(js));
    b.nav(&page);
    assert!(b.state_until(|s| s.ends_with("title=none")).ends_with("title=none"), "hidden");
    assert_eq!(b.bridge(&["settings.set", "hide_cookie_banners", "0"]), "true");
    b.nav(&page);
    assert!(b.state_until(|s| s.ends_with("title=block")).ends_with("title=block"), "shown when turned off");
    assert_eq!(b.bridge(&["settings.set", "hide_cookie_banners", "1"]), "true");
    assert_eq!(b.bridge(&["perm.set", "127.0.0.1", "shields", "off"]), "true");
    b.nav(&page);
    assert!(b.state_until(|s| s.ends_with("title=block")).ends_with("title=block"), "shown with Shields down");
}

#[test]
fn javascript_can_be_turned_off_per_site() {
    let base = serve();
    let mut b = launch();
    let page = format!("go {base}/script/{}", encode("document.title = 'ran'"));
    b.nav(&page);
    assert!(b.state_until(|s| s.ends_with("title=ran")).ends_with("title=ran"));
    assert_eq!(b.bridge(&["perm.set", "127.0.0.1", "javascript", "deny"]), "true");
    b.nav(&page);
    std::thread::sleep(Duration::from_millis(300));
    assert!(b.send("state").ends_with("title=start"), "the page's script must not run");
    // Other sites are unaffected.
    let other = format!("go {}/script/{}", base.replace("127.0.0.1", "localhost"), encode("document.title = 'ran'"));
    b.nav(&other);
    assert!(b.state_until(|s| s.ends_with("title=ran")).ends_with("title=ran"));
    assert_eq!(b.bridge(&["perm.set", "127.0.0.1", "javascript", "ask"]), "true");
    b.nav(&page);
    assert!(b.state_until(|s| s.ends_with("title=ran")).ends_with("title=ran"), "back on");
}

#[test]
fn unknown_internal_page_fails() {
    let mut b = launch();
    assert!(b.send("go mybrowser://nope").starts_with("FAILED "));
}

// ---------------------------------------------------------------------------
// Extensions

#[test]
fn extensions_can_play_audio_from_an_offscreen_document() {
    // WebKit has no `chrome.offscreen`; read-aloud extensions need it. The
    // browser stands in for it, and the player's audio must actually start
    // (a background page outside a window stays "interrupted").
    let base = serve();
    let fixture = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/offscreen-extension");
    let profile = TempProfile::new("offscreen");
    let mut b = launch_with(&["--profile", profile.path()]);
    let installed = b.bridge(&["ext.install", fixture]);
    let id: String = installed.split("\"id\":").nth(1).unwrap().chars().take_while(char::is_ascii_digit).collect();
    b.nav(&format!("go {base}/page/listen"));
    assert_eq!(b.send(&format!("extclick {id}")), "EXTCLICK ok");
    let state = b.state_until(|s| s.contains("title=AUDIO"));
    // Its own rule's User-Agent, which WebKit doesn't apply to extension pages.
    assert!(state.ends_with("title=AUDIO running OffscreenPlayerTest/1.0"), "{state}");
    // Only the profile's copy was changed, not the folder installed from.
    let source = std::fs::read_to_string(format!("{fixture}/manifest.json")).unwrap();
    assert!(source.contains("service_worker") && !source.contains("nativeMessaging"));
    b.quit();
}

#[test]
fn extension_connections_are_blocked_until_allowed() {
    // The browser connects for an extension only to addresses the user
    // allowed: blocked ones are listed for the session, allowed ones kept.
    let base = serve();
    let fixture = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/offscreen-extension");
    let profile = TempProfile::new("netgate");
    let mut b = launch_with(&["--profile", profile.path()]);
    let installed = b.bridge(&["ext.install", fixture]);
    let id: String = installed.split("\"id\":").nth(1).unwrap().chars().take_while(char::is_ascii_digit).collect();
    b.nav(&format!("go {base}/page/listen"));
    assert_eq!(b.send(&format!("extclick {id}")), "EXTCLICK ok");
    b.state_until(|s| s.contains("title=AUDIO"));
    assert_eq!(b.send("netbutton"), "NETBUTTON 0");
    // The page asks the extension's offscreen page to open a WebSocket.
    let socket = |b: &mut Browser, url: &str| {
        b.nav(&format!("go {base}/script/{}", encode(&format!("setTimeout(()=>postMessage({{ws:'{url}'}},'*'),100)"))));
        let s = b.state_until(|s| s.contains("title=WS"));
        s.split("title=WS ").nth(1).unwrap_or_else(|| panic!("{s}")).to_string()
    };
    let blocked = "error close 1006 my-browser: blocked until allowed in Settings → Network";
    assert_eq!(socket(&mut b, "wss://gate.invalid/tts?token=1"), blocked);
    assert_eq!(socket(&mut b, "wss://gate.invalid/tts?token=2"), blocked, "another query, same address");
    assert_eq!(socket(&mut b, "wss://gate.invalid/other"), blocked);
    assert_eq!(b.send("netbutton"), "NETBUTTON 1");
    let list = b.bridge(&["net.list"]);
    assert!(
        list.contains(r#""address":"wss://gate.invalid/tts","site":"wss://gate.invalid/*","denied":false,"count":2"#),
        "{list}"
    );
    // Never offered: hosts the manifest doesn't ask for, this Mac, plain ws:.
    for (url, why) in [
        ("wss://elsewhere.invalid/x", "not a site this extension asks for"),
        ("wss://127.0.0.1/x", "this Mac and the local network are blocked"),
        ("ws://gate.invalid/x", "only wss: addresses"),
    ] {
        assert_eq!(socket(&mut b, url), format!("error close 1006 my-browser: {why}"));
    }
    assert!(!b.bridge(&["net.list"]).contains("elsewhere"));
    // Only a blocked request's address or site can be allowed.
    assert!(b.send(&format!("bridge net.allow\t{id}\twss://gate.invalid/*x")).starts_with("BRIDGE err "));
    assert!(b.send(&format!("bridge net.allow\t{id}\twss://evil.invalid/*")).starts_with("BRIDGE err "));
    // Deny one: still blocked, no longer waiting. Allow the other (any query).
    assert_eq!(b.bridge(&["net.deny", &id, "wss://gate.invalid/other"]), "true");
    assert_eq!(b.bridge(&["net.allow", &id, "wss://gate.invalid/tts"]), "true");
    assert_eq!(b.send("netbutton"), "NETBUTTON 0");
    let list = b.bridge(&["net.list"]);
    assert!(list.contains(r#""allowed":[{"ext":"#) && list.contains(r#""rule":"wss://gate.invalid/tts""#), "{list}");
    assert!(
        list.contains(r#""address":"wss://gate.invalid/other","site":"wss://gate.invalid/*","denied":true"#),
        "{list}"
    );
    // Allowed: the browser connects (the name doesn't exist, so it fails as a
    // connection, not as a refusal).
    assert_eq!(socket(&mut b, "wss://gate.invalid/tts?token=3"), "error close 1006");
    assert_eq!(socket(&mut b, "wss://gate.invalid/other"), blocked);
    b.quit();

    // Next session: the allowed rule is kept, the blocked list is gone.
    let mut b = launch_with(&["--profile", profile.path()]);
    let list = b.bridge(&["net.list"]);
    assert!(list.starts_with(r#"{"blocked":[],"allowed":[{"#) && list.contains("wss://gate.invalid/tts"), "{list}");
    assert_eq!(b.bridge(&["net.forget", &id, "wss://gate.invalid/tts"]), "true");
    assert_eq!(b.bridge(&["net.list"]), r#"{"blocked":[],"allowed":[]}"#);
    b.quit();
}

#[test]
fn extensions_stay_off_until_clicked_then_turn_off_when_leaving_the_site() {
    let base = serve();
    let other = base.replace("127.0.0.1", "localhost");
    let fixture = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/title-extension");
    // Throwaway profiles count as private browsing, where extensions don't run.
    let profile = TempProfile::new("extension");
    let mut b = launch_with(&["--profile", profile.path()]);
    let installed = b.bridge(&["ext.install", fixture]);
    assert!(installed.contains("\"name\":\"Title Marker\""), "{installed}");
    let id: String = installed.split("\"id\":").nth(1).unwrap().chars().take_while(char::is_ascii_digit).collect();
    let list = b.bridge(&["ext.list"]);
    assert!(
        list.contains("\"access\":\"click\"") && list.contains("\"running\":false"),
        "installed, not started: {list}"
    );
    assert!(list.contains("http://127.0.0.1/*"), "{list}");

    // Off until clicked: the page is untouched, and the button shows it's off.
    b.nav(&format!("go {base}/page/hello"));
    std::thread::sleep(Duration::from_millis(300));
    assert!(b.send("state").ends_with("title=hello"), "not running before a click");
    assert_eq!(b.send("extbuttons"), format!("EXTBUTTONS {id}:0"));

    // A click starts it, turns it on for this site (the page reloads so its
    // script runs), and opens its popup under the button.
    assert_eq!(b.send(&format!("extclick {id}")), "EXTCLICK ok");
    assert_eq!(b.wait_for("POPUP "), format!("POPUP {id} shown=1"));
    assert!(b.state_until(|s| s.ends_with("title=EXT hello")).ends_with("title=EXT hello"), "content script ran");
    assert_eq!(b.send("extbuttons"), format!("EXTBUTTONS {id}:1"));
    assert!(b.bridge(&["ext.list"]).contains("\"running\":true"));

    // Same site: still on. Another site: off, and coming back it stays off.
    b.nav(&format!("go {base}/page/again"));
    assert!(b.state_until(|s| s.ends_with("title=EXT again")).ends_with("title=EXT again"));
    b.nav(&format!("go {other}/page/elsewhere"));
    assert_eq!(b.send("extbuttons"), format!("EXTBUTTONS {id}:0"));
    b.nav(&format!("go {base}/page/back"));
    std::thread::sleep(Duration::from_millis(300));
    assert!(b.send("state").ends_with("title=back"), "off again after leaving the site");

    // "On every site": runs without a click.
    assert_eq!(b.bridge(&["ext.access", &id, "always"]), "true");
    b.nav(&format!("go {base}/page/everywhere"));
    assert!(b.state_until(|s| s.ends_with("title=EXT everywhere")).ends_with("title=EXT everywhere"));
    assert!(b.send(&format!("bridge ext.access\t{id}\tsometimes")).starts_with("BRIDGE err "));

    // Disabled: no button, never runs.
    assert_eq!(b.bridge(&["ext.toggle", &id, "0"]), "true");
    assert_eq!(b.send("extbuttons"), "EXTBUTTONS ");
    b.nav(&format!("go {base}/page/plain"));
    std::thread::sleep(Duration::from_millis(300));
    assert!(b.send("state").ends_with("title=plain"), "disabled extension doesn't run");

    assert_eq!(b.bridge(&["ext.remove", &id]), "true");
    assert_eq!(b.bridge(&["ext.list"]), "[]");
    // The copy the browser made inside the profile is gone too.
    let left: Vec<_> =
        std::fs::read_dir(profile.0.join("extensions")).map(|d| d.flatten().collect()).unwrap_or_default();
    assert!(left.iter().all(|e| !e.file_name().to_string_lossy().ends_with("title-extension")), "{left:?}");
    b.quit();
}

#[test]
fn reload_copies_an_unpacked_extension_folder_again() {
    let base = serve();
    let profile = TempProfile::new("reload");
    let src = TempProfile::new("reload-src");
    let fixture = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/title-extension"));
    for f in ["manifest.json", "content.js", "popup.html"] {
        std::fs::copy(fixture.join(f), src.0.join(f)).unwrap();
    }
    let mut b = launch_with(&["--profile", profile.path()]);
    let installed = b.bridge(&["ext.install", src.path()]);
    let id: String = installed.split("\"id\":").nth(1).unwrap().chars().take_while(char::is_ascii_digit).collect();
    assert!(b.bridge(&["ext.list"]).contains("\"local\":true"));
    assert_eq!(b.bridge(&["ext.access", &id, "always"]), "true");
    b.nav(&format!("go {base}/page/before"));
    assert!(b.state_until(|s| s.ends_with("title=EXT before")).ends_with("title=EXT before"));

    // Edit the user's folder: the running copy doesn't change until Reload.
    std::fs::write(src.0.join("content.js"), "document.title = \"NEW \" + document.title;").unwrap();
    assert_eq!(b.bridge(&["ext.reload", &id]), "\"Title Marker\"");
    b.nav(&format!("go {base}/page/after"));
    assert!(b.state_until(|s| s.ends_with("title=NEW after")).ends_with("title=NEW after"));
    b.quit();
}

#[test]
fn chrome_extension_packages_install_from_crx_and_zip() {
    let profile = TempProfile::new("packages");
    let files = TempProfile::new("package-files");
    let fixture = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/title-extension");
    // A .zip with the extension in a folder (as GitHub serves them), and a
    // signed CRX3 of the same folder (as the Chrome Web Store serves them).
    let zip = files.0.join("title.zip");
    let zipped = Command::new("/usr/bin/ditto").args(["-c", "-k", "--keepParent", fixture]).arg(&zip).status();
    assert!(zipped.unwrap().success());
    let crx = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/title.crx")).to_path_buf();
    // The same package with one byte of its contents changed.
    let tampered = files.0.join("tampered.crx");
    let mut bytes = std::fs::read(&crx).unwrap();
    let at = bytes.len() - 30;
    bytes[at] ^= 1;
    std::fs::write(&tampered, bytes).unwrap();
    let junk = files.0.join("junk.crx");
    std::fs::write(&junk, "<html>not a package</html>").unwrap();

    let mut b = launch_with(&["--profile", profile.path()]);
    for package in [&crx, &zip] {
        let installed = b.bridge(&["ext.install", package.to_str().unwrap()]);
        assert!(installed.contains("\"name\":"), "{installed}");
    }
    let list = b.bridge(&["ext.list"]);
    assert_eq!(list.matches("\"read\":true").count(), 2, "{list}");
    // Installed into the profile, not run from the temporary unpacked copy.
    assert_eq!(list.matches(&format!("\"path\":\"{}/extensions/", profile.path())).count(), 2, "{list}");
    let refused = b.send(&format!("bridge ext.install\t{}", junk.display()));
    assert_eq!(refused, "BRIDGE err That isn't an extension package (.crx or .zip).");
    let refused = b.send(&format!("bridge ext.install\t{}", tampered.display()));
    assert!(refused.starts_with("BRIDGE err This package can't be trusted"), "{refused}");
    assert_eq!(b.bridge(&["ext.list"]).matches("\"id\":").count(), 2, "nothing more installed");
    b.quit();
}

#[test]
fn web_store_extensions_update_in_place() {
    let base = serve();
    let profile = TempProfile::new("ext-update");
    let id = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/title.crx.id")).unwrap();
    let id = id.trim();
    let mut b = launch_with(&["--profile", profile.path()]);
    let crx = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/title.crx");
    let installed = b.bridge(&["ext.install", crx]);
    let row = installed.strip_prefix("{\"id\":").unwrap().split(',').next().unwrap().to_string();
    let not_store = b.send(&format!("bridge ext.update\t{row}"));
    assert!(not_store.starts_with("BRIDGE err Only extensions added from the Chrome Web Store"), "{not_store}");
    // The installed copy loses a file; the update puts the package's files back.
    let list = b.bridge(&["ext.list"]);
    let path = list.split("\"path\":\"").nth(1).unwrap().split('"').next().unwrap().to_string();
    std::fs::remove_file(Path::new(&path).join("content.js")).unwrap();
    b.nav("go about:blank");
    writeln!(b.stdin, "bridge ext.update\t{row}\t{base}/crx/{id}_1_0.crx\t{id}").unwrap();
    let done = b.wait_for("BRIDGE");
    assert_eq!(done, r#"BRIDGE ok {"name":"Title Marker","version":"1.0"}"#);
    assert!(Path::new(&path).join("content.js").is_file(), "files replaced in place");
    assert!(b.bridge(&["ext.list"]).contains("\"read\":true"));
    // A package signed by another extension is refused.
    let other = "a".repeat(32);
    writeln!(b.stdin, "bridge ext.update\t{row}\t{base}/crx/{other}_1_0.crx\t{other}").unwrap();
    assert!(b.wait_for("BRIDGE").starts_with("BRIDGE err This package can't be trusted"));
    b.quit();
}

#[test]
fn installing_a_folder_without_manifest_fails() {
    let dir = TempProfile::new("not-an-extension");
    let mut b = launch();
    let r = b.send(&format!("bridge ext.install\t{}", dir.path()));
    assert!(r.starts_with("BRIDGE err ") && r.contains("manifest.json"), "{r}");
    assert_eq!(b.bridge(&["ext.list"]), "[]");
}

// ---------------------------------------------------------------------------
// Storage

#[test]
fn storage_reports_and_clears_this_profile() {
    let base = serve();
    let mut b = launch();
    b.nav(&format!("go {base}/page/one"));
    let s = b.bridge(&["storage.get"]);
    assert!(s.contains("\"history\":1"), "{s}");
    assert_eq!(b.bridge(&["storage.clear_cache"]), "true");
    // Clearing the cache keeps logins.
    b.nav(&format!("go {base}/setcookie"));
    b.bridge(&["storage.clear_cache"]);
    b.nav(&format!("go {base}/echo"));
    assert!(b.state_until(|s| s.contains("title=cookie:")).contains("cookie:session=abc123"));
    assert_eq!(b.bridge(&["downloads.clear"]), "true");
    assert!(b.bridge(&["storage.get"]).contains("\"downloads\":0"));
}

#[test]
fn leftover_data_is_listed_and_deleted_but_profiles_are_kept() {
    // A fake home, so the real ~/Library is never touched.
    let home = TempProfile::new("home");
    let lib = home.0.join("Library");
    let put = |p: &str| {
        let p = lib.join(p);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, b"x").unwrap();
    };
    put("HTTPStorages/my-browser.binarycookies");
    put("HTTPStorages/my-browser.binarycookies_tmp_9.dat");
    put("WebKit/my-browser/WebsiteData/IndexedDB/db");
    put("Caches/my-browser/WebKit/NetworkCache/r");
    put("WebKit/my-browser/WebsiteDataStore/1234/cookies");
    put("HTTPStorages/com.apple.Safari.binarycookies");

    let mut b = launch_env(&[], &[("HOME", home.path())]);
    let s = b.bridge(&["storage.get"]);
    assert_eq!(s.matches("\"path\":").count(), 4, "{s}");
    assert!(!s.contains("WebsiteDataStore") && !s.contains("Safari"), "{s}");
    let r = b.bridge(&["legacy.clear"]);
    assert!(r.starts_with("{\"removed\":4,"), "{r}");
    assert!(b.bridge(&["storage.get"]).contains("\"leftovers\":[]"));
    assert!(lib.join("WebKit/my-browser/WebsiteDataStore/1234/cookies").exists(), "profile data kept");
    assert!(lib.join("HTTPStorages/com.apple.Safari.binarycookies").exists(), "other apps kept");
    assert!(!lib.join("HTTPStorages/my-browser.binarycookies").exists());
}

// ---------------------------------------------------------------------------
// Find in page

#[test]
fn address_bar_suggests_past_searches_and_visited_pages() {
    let base = serve();
    let mut b = launch();
    b.nav(&format!("go {base}/page/Rustacean"));
    assert_eq!(b.send("bartype zzz"), "SUGGEST ", "nothing matches, no list");
    // What Return on a search saves (without going to the search engine).
    assert_eq!(b.send("savesearch rust book"), "SAVEDSEARCH");
    assert!(b.bridge(&["storage.get"]).contains(r#""searches":1"#));
    let typed = b.send("bartype rust");
    assert_eq!(typed, format!("SUGGEST search:rust book|page:{base}/page/Rustacean"));
    // ↓ chooses the first, ↓ the next, Return opens it.
    assert_eq!(b.send("barkey down"), format!("SUGGEST *search:rust book|page:{base}/page/Rustacean"));
    b.send("barkey down");
    let line = b.send("barkey enter");
    assert_eq!(line, "SUGGEST ", "the list closes");
    assert!(b.state_until(|s| s.ends_with("title=Rustacean")).ends_with("title=Rustacean"));
    // Esc closes the list; turning suggestions off hides them and keeps no searches.
    assert!(b.send("bartype rust").starts_with("SUGGEST search:"));
    assert_eq!(b.send("barkey esc"), "SUGGEST ");
    assert_eq!(b.bridge(&["settings.set", "address_suggestions", "0"]), "true");
    assert_eq!(b.send("bartype rust"), "SUGGEST ");
    assert_eq!(b.bridge(&["history.clear"]), "true");
    assert!(b.bridge(&["storage.get"]).contains(r#""searches":0"#), "clearing history clears searches");
}

#[test]
fn address_bar_loads_on_return_only() {
    let mut b = launch();
    b.nav("go data:text/html,<title>one</title>");
    // Return navigates, through the field editor like a real key press.
    assert!(b.nav("barenter data:text/html,<title>two</title>").contains("two"));
    // Return on the unchanged address, or clicking in and out again,
    // doesn't reload the page.
    b.send("eval window.marker = 1");
    writeln!(b.stdin, "barenter data:text/html,<title>two</title>").unwrap();
    assert_eq!(b.send("barblur"), "BLURRED");
    std::thread::sleep(Duration::from_secs(1));
    assert!(b.send("state").starts_with("STATE "), "the address bar loaded the page again");
    assert_eq!(b.send("eval window.marker"), "EVAL 1");
    b.quit();
}

#[test]
fn find_bar_finds_steps_wraps_and_closes() {
    let mut b = launch();
    // "alpha" at offsets 0 and 11.
    b.nav("go data:text/html,<title>t</title><p>alpha beta alpha gamma</p>");
    b.key("cmd+f");
    assert!(b.send("findbar").starts_with("FINDBAR open=1"), "cmd+f opens the bar");

    assert_eq!(b.send("find alpha"), "FIND found=1");
    assert_eq!(b.send("selection"), "SELECTION 0");
    assert_eq!(b.send("findnext"), "FIND found=1");
    assert_eq!(b.send("selection"), "SELECTION 11");
    assert_eq!(b.send("findnext"), "FIND found=1");
    assert_eq!(b.send("selection"), "SELECTION 0", "wraps around");
    assert_eq!(b.send("findprev"), "FIND found=1");
    assert_eq!(b.send("selection"), "SELECTION 11", "previous wraps back");

    // A new term starts from the top again.
    assert_eq!(b.send("find gamma"), "FIND found=1");
    assert_eq!(b.send("selection"), "SELECTION 17");

    assert_eq!(b.send("find zzz"), "FIND found=0");
    assert_eq!(b.send("findbar"), "FINDBAR open=1 text=zzz status=No matches");

    // Esc closes it; Cmd+G reopens it and searches again.
    assert!(b.send("findesc").starts_with("FINDBAR open=0"));
    b.key("cmd+g");
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let bar = b.send("findbar");
        if bar == "FINDBAR open=1 text=zzz status=No matches" {
            break;
        }
        assert!(Instant::now() < deadline, "cmd+g didn't reopen and search: {bar}");
        std::thread::sleep(Duration::from_millis(100));
    }
    // Shift+Cmd+G searches backwards from the same bar.
    b.send("find alpha");
    // (Searches run asynchronously: let the first match be selected first, or
    // the backward search can start from the old selection under load.)
    let deadline = Instant::now() + Duration::from_secs(5);
    while b.send("selection") != "SELECTION 0" {
        assert!(Instant::now() < deadline, "find alpha selects the first match");
        std::thread::sleep(Duration::from_millis(50));
    }
    b.key("cmd+shift+g");
    let deadline = Instant::now() + Duration::from_secs(5);
    while b.send("selection") != "SELECTION 11" {
        assert!(Instant::now() < deadline, "previous from the first match wraps to the last");
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn find_bar_survives_navigation_and_sleep() {
    let base = serve();
    let mut b = launch();
    b.nav(&format!("go {base}/page/needle"));
    assert_eq!(b.send("find needle"), "FIND found=1");
    // The bar stays open across a navigation and searches the new page.
    b.nav(&format!("go {base}/page/other"));
    assert_eq!(b.send("findnext"), "FIND found=0");
    // A tab that slept and woke up still lays out under an open find bar.
    b.nav(&format!("newtab {base}/page/second"));
    b.send("sleep");
    b.nav("switch 0");
    assert!(b.send("findbar").starts_with("FINDBAR open=1"));
    assert_eq!(b.send("find other"), "FIND found=1");
}

// ---------------------------------------------------------------------------
// Iteration 8: translate, filters, keywords, spelling, screenshots, picture in
// picture, extension commands, autofill, profiles, sync file, developer
// tools, local addresses, find counts, folders, PDFs, gestures.

#[test]
fn pages_translate_and_show_the_original_again() {
    // The helper "translates" by upper-casing (no language models needed).
    let mut b = launch_env(&[], &[("MYB_TRANSLATE_FAKE", "1")]);
    b.nav("go data:text/html;charset=utf-8,<title>t</title><p>Bonjour tout le monde, le chat dort sur le canapé.</p><code>garder</code>");
    assert_eq!(b.bridge(&["settings.set", "translate_to", "en"]), "true");
    assert!(b.send("bridge settings.set\ttranslate_to\tEnglish!").starts_with("BRIDGE err"));
    assert_eq!(b.send("translate"), "TRANSLATED fr 1", "one text (code is left alone)");
    assert_eq!(
        b.send("eval document.body.innerText"),
        "EVAL BONJOUR TOUT LE MONDE, LE CHAT DORT SUR LE CANAPÉ.  garder"
    );
    assert!(b.send("menu View").contains("|Show Original|"));
    assert_eq!(b.send("translate"), "TRANSLATE off");
    assert!(b.send("eval document.body.innerText").starts_with("EVAL Bonjour tout"));
    // Already in the target language.
    assert_eq!(b.bridge(&["settings.set", "translate_to", "fr"]), "true");
    assert_eq!(b.send("translate"), "TRANSLATE err same");
}

#[test]
fn elements_hide_with_the_picker_and_your_own_filters() {
    let base = serve();
    let mut b = launch();
    let js = "addEventListener('DOMContentLoaded', () => { document.body.innerHTML = \
              '<div class=\"ad box\">AD</div><p id=keep>text</p><p class=promo>x</p>' })";
    let page = format!("go {base}/script/{}", encode(js));
    b.nav(&page);
    let shown =
        |b: &mut Browser, sel: &str| b.send(&format!("eval getComputedStyle(document.querySelector('{sel}')).display"));
    // Hide Element: pick it, confirm, and it's gone, now and after a reload.
    assert_eq!(b.send("picker"), "PICKER");
    writeln!(b.stdin, "eval document.querySelector('.ad').click(), 1").unwrap();
    assert_eq!(b.wait_for("DIALOG"), "DIALOG Hide this element on 127.0.0.1?");
    writeln!(b.stdin, "dialog ok").unwrap();
    assert_eq!(b.wait_for("HIDDEN"), "HIDDEN 1");
    assert_eq!(b.bridge(&["filters.get"]), r#"{"text":"127.0.0.1##div.ad.box\n","unknown":[]}"#);
    b.nav(&page);
    assert_eq!(shown(&mut b, ".ad"), "EVAL none");
    assert_eq!(shown(&mut b, "#keep"), "EVAL block");
    // Your own list, with a line it doesn't understand.
    assert_eq!(b.bridge(&["filters.set", "##.promo\\n/banner/*"]), r#"{"unknown":["/banner/*"]}"#);
    b.nav(&page);
    assert_eq!(shown(&mut b, ".promo"), "EVAL none");
    assert_eq!(shown(&mut b, ".ad"), "EVAL block", "the picked rule was replaced");
    // Shields down shows everything.
    assert_eq!(b.bridge(&["perm.set", "127.0.0.1", "shields", "off"]), "true");
    b.nav(&page);
    assert_eq!(shown(&mut b, ".promo"), "EVAL block");
}

#[test]
fn search_keywords_open_their_site() {
    let base = serve();
    let mut b = launch();
    assert_eq!(b.bridge(&["keywords.set", "t", &format!("{base}/page/%s")]), "true");
    assert!(b.send("bridge keywords.set\tbad word\thttps://x.example/%s").starts_with("BRIDGE err"));
    let wiki = r#"{"keyword":"w","url":"https://en.wikipedia.org/wiki/Special:Search?search=%s","on":"#;
    assert!(b.bridge(&["keywords.list"]).contains(&format!("{wiki}false}}")), "built-in bangs start off");
    assert_eq!(b.bridge(&["settings.set", "bangs", "w,yt"]), "true");
    assert!(b.bridge(&["keywords.list"]).contains(&format!("{wiki}true}}")));
    assert!(b.send("bridge settings.set\tbangs\tw,nope").starts_with("BRIDGE err"));
    assert!(b.nav("barenter t hello").ends_with("/page/hello"));
    assert!(b.nav("barenter world !t").ends_with("/page/world"));
    b.bridge(&["keywords.delete", "t"]);
    assert!(b.bridge(&["keywords.list"]).starts_with(r#"{"custom":[]"#));
}

#[test]
fn spelling_is_checked_while_typing() {
    let mut b = launch();
    b.nav("go data:text/html,<textarea>helo wrld</textarea>");
    assert_eq!(b.send("spell"), "SPELL continuous=1 grammar=1 correct=0");
    assert!(b.send("menu Edit").contains("|Spelling and Grammar|"));
}

#[test]
fn screenshots_take_the_whole_page_the_visible_part_or_an_area() {
    let dir = TempProfile::new("screenshot");
    let path = |n: &str| dir.0.join(n).display().to_string();
    let size = |line: &str| -> (u32, u32) {
        let (w, h) = line.rsplit(' ').next().and_then(|s| s.split_once('x')).unwrap_or_else(|| panic!("{line}"));
        (w.parse().unwrap(), h.parse().unwrap())
    };
    let mut b = launch();
    b.nav("go data:text/html,<body style=margin:0><div style=height:3000px;background:red>tall</div>");
    let full = b.send(&format!("screenshot full {}", path("full.png")));
    assert!(full.starts_with("SCREENSHOT 1 ") && size(&full).1 >= 3000, "the whole page, not the window: {full}");
    assert!(std::fs::read(path("full.png")).unwrap().starts_with(b"\x89PNG"));
    let visible = b.send(&format!("screenshot visible {}", path("visible.png")));
    assert!(visible.starts_with("SCREENSHOT 1 ") && size(&visible).1 < 3000, "{visible}");
    let area = b.send(&format!("screenshot area 0,0,100,50 {}", path("area.png")));
    let (w, h) = size(&area);
    assert!(area.starts_with("SCREENSHOT 1 ") && w == 2 * h && w >= 100, "{area}");
    // The picker: a drag takes that area; a click without dragging cancels.
    assert_eq!(b.send(&format!("screenshot area {}", path("picked.png"))), "SCREENSHOT picking");
    let picked = b.send("screenshotdrag 10,10,90,50");
    let (w, h) = size(&picked);
    assert!(picked.starts_with("SCREENSHOT 1 ") && w * 40 == h * 80, "{picked}");
    assert_eq!(b.send("screenshot area"), "SCREENSHOT picking");
    assert_eq!(b.send("screenshotdrag 5,5,6,6"), "SCREENSHOT cancelled");
    assert_eq!(b.send("screenshotdrag 5,5,60,60"), "ERR no picker", "gone after cancelling");
    // Copy to Clipboard is remembered (tests copy to a private pasteboard).
    assert_eq!(b.bridge(&["settings.set", "screenshot_to", "clipboard"]), "true");
    assert!(b.send("screenshot visible").starts_with("SCREENSHOT copied=1 "));
}

#[test]
fn a_playing_video_goes_picture_in_picture_when_its_tab_is_left() {
    let base = serve();
    let mut b = launch_with(&["--on-top"]);
    let js = "addEventListener('DOMContentLoaded', () => { document.body.innerHTML = '<video id=v width=320 height=240></video>' }); \
              function start() { const c = document.createElement('canvas'); c.width = 320; c.height = 240; \
              const g = c.getContext('2d'); let i = 0; setInterval(() => { g.fillStyle = `hsl(${i++ % 360} 80% 50%)`; g.fillRect(0, 0, 320, 240) }, 40); \
              const ac = new AudioContext(); const o = ac.createOscillator(); const gain = ac.createGain(); gain.gain.value = 0.001; \
              const d = ac.createMediaStreamDestination(); o.connect(gain).connect(d); o.start(); \
              const s = c.captureStream(25); s.addTrack(d.stream.getAudioTracks()[0]); v.srcObject = s; v.play(); return 1 }";
    b.nav(&format!("go {base}/script/{}", encode(js)));
    assert_eq!(b.send("eval start()"), "EVAL 1");
    let deadline = Instant::now() + Duration::from_secs(10);
    while !b.send("tabflags").contains("audio=1") {
        assert!(Instant::now() < deadline, "never played");
        std::thread::sleep(Duration::from_millis(100));
    }
    writeln!(b.stdin, "newtab").unwrap();
    b.wait_all(&["TABS", "AUTOPIP 1"]);
    b.send("switch 0");
    let deadline = Instant::now() + Duration::from_secs(5);
    while b.send("eval !!document.pictureInPictureElement") != "EVAL 0" {
        assert!(Instant::now() < deadline, "back in the page when the tab is shown again");
        std::thread::sleep(Duration::from_millis(100));
    }
    // Turned off: nothing happens.
    assert_eq!(b.bridge(&["settings.set", "auto_pip", "0"]), "true");
    b.send("switch 1");
    std::thread::sleep(Duration::from_millis(500));
    b.send("switch 0");
    assert_eq!(b.send("eval !!document.pictureInPictureElement"), "EVAL 0");
}

#[test]
fn extension_keyboard_commands_are_menu_items() {
    let base = serve();
    let fixture = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/command-extension");
    let profile = TempProfile::new("commands");
    let mut b = launch_with(&["--profile", profile.path()]);
    assert!(b.bridge(&["ext.install", fixture]).contains("\"name\":\"Command Test\""));
    assert_eq!(b.send("menu Extensions"), "MENU Manage Extensions…|-|Command Test: Mark the page");
    assert!(
        b.bridge(&["keys.list"]).contains(r#""menu":"Extensions","title":"Command Test: Mark the page","keys":"⇧⌘Y""#)
    );
    b.nav(&format!("go {base}/page/plain"));
    b.key("cmd+shift+y");
    assert!(
        b.state_until(|s| s.ends_with("title=CMD mark")).ends_with("title=CMD mark"),
        "the command ran on the page"
    );
    // Its keys can be changed like any shortcut, and stay changed after a restart.
    let id = format!("extensionCommand:#{}", 100);
    assert_eq!(b.bridge(&["keys.set", &id, "cmd+alt+y"]), "true");
    b.quit();
    let mut b = launch_with(&["--profile", profile.path()]);
    let deadline = Instant::now() + Duration::from_secs(10);
    while !b.bridge(&["keys.list"]).contains(r#""title":"Command Test: Mark the page","keys":"⌥⌘Y","custom":true"#)
    {
        assert!(Instant::now() < deadline, "changed keys come back");
        std::thread::sleep(Duration::from_millis(100));
    }
    b.quit();
}

#[test]
fn addresses_and_cards_fill_forms_when_picked() {
    let base = serve();
    let mut b = launch();
    assert_eq!(
        b.bridge(&[
            "autofill.save",
            "0",
            "address",
            "name",
            "Ada Lovelace",
            "email",
            "ada@example.com",
            "postal",
            "NW1",
            "country",
            "United Kingdom"
        ]),
        r#"{"id":1}"#
    );
    assert!(
        b.send("bridge autofill.save\t0\tcard\tnumber\t4242 4242 4242 4241")
            .ends_with("That isn't a valid card number.")
    );
    assert_eq!(
        b.bridge(&[
            "autofill.save",
            "0",
            "card",
            "name",
            "Ada L",
            "number",
            "4242 4242 4242 4242",
            "month",
            "3",
            "year",
            "2031"
        ]),
        r#"{"id":2}"#
    );
    let list = b.bridge(&["autofill.list"]);
    assert!(
        list.contains(r#""label":"Visa •••• 4242 (Ada L)""#) && !list.contains("4242424242424242"),
        "the number stays in the vault: {list}"
    );
    let form = "addEventListener('DOMContentLoaded', () => { document.body.innerHTML = \
                '<form><input name=fullname id=n placeholder=\"Full name\"><input name=email id=e><input name=zip id=z>\
                <select name=country id=c><option value=\"\">-</option><option value=GB>United Kingdom</option></select>\
                <input name=username id=u></form><input type=search id=q>\
                <form><input autocomplete=cc-name id=cn><input autocomplete=cc-number id=cc><input autocomplete=cc-exp id=ce>\
                <input autocomplete=cc-csc id=cv></form>' })";
    b.nav(&format!("go {base}/script/{}", encode(form)));
    // A lone search box isn't an address field.
    assert_eq!(b.send("eval q.focus(), 1"), "EVAL 1");
    writeln!(b.stdin, "eval n.focus(), 1").unwrap();
    assert_eq!(b.wait_all(&["AUTOFILL", "EVAL"])[0], "AUTOFILL Ada Lovelace, ada@example.com");
    assert_eq!(b.send("fillautofill 0"), "FILLAUTOFILL");
    let deadline = Instant::now() + Duration::from_secs(5);
    while b.send("eval [n.value, e.value, z.value, c.value, u.value].join('|')")
        != "EVAL Ada Lovelace|ada@example.com|NW1|GB|"
    {
        assert!(Instant::now() < deadline, "address not filled");
        std::thread::sleep(Duration::from_millis(100));
    }
    writeln!(b.stdin, "eval cc.focus(), 1").unwrap();
    assert_eq!(b.wait_all(&["AUTOFILL", "EVAL"])[0], "AUTOFILL Visa •••• 4242 (Ada L)");
    assert_eq!(b.send("fillautofill 0"), "FILLAUTOFILL");
    let deadline = Instant::now() + Duration::from_secs(5);
    while b.send("eval [cn.value, cc.value, ce.value, cv.value].join('|')") != "EVAL Ada L|4242424242424242|03/31|" {
        assert!(Instant::now() < deadline, "card not filled (and never the security code)");
        std::thread::sleep(Duration::from_millis(100));
    }
    assert_eq!(b.bridge(&["autofill.delete", "2"]), "true");
    assert!(!b.bridge(&["autofill.list"]).contains("card"));
}

#[test]
fn profiles_are_listed_added_opened_and_deleted() {
    let home = TempProfile::new("profiles-home");
    let work = home.0.join("Library/Application Support/my-browser/profiles/Work");
    std::fs::create_dir_all(&work).unwrap();
    let mut b = launch_env(&["--profile", work.to_str().unwrap()], &[("HOME", home.path())]);
    assert_eq!(b.send("profile"), "PROFILE Work Work: search or enter address", "the window says which profile");
    assert_eq!(
        b.bridge(&["profiles.list"]),
        r#"{"current":"Work","profiles":[{"name":"Default","open":false},{"name":"Work","open":true}]}"#
    );
    assert_eq!(b.bridge(&["profiles.create", "Home"]), "true");
    assert!(b.send("bridge profiles.create\thome").starts_with("BRIDGE err"), "names are unique, whatever the case");
    assert!(b.send("bridge profiles.create\t../x").starts_with("BRIDGE err"));
    assert!(b.bridge(&["profiles.open", "Home"]).ends_with("/profiles/Home\""));
    assert!(b.send("bridge profiles.delete\tWork").ends_with("Close that profile's windows first."));
    assert!(b.send("bridge profiles.delete\tDefault").starts_with("BRIDGE err"));
    assert_eq!(b.bridge(&["profiles.delete", "Home"]), "true");
    assert!(!home.0.join("Library/Application Support/my-browser/profiles/Home").exists());
    b.quit();
    // (The profile's WebKit data lives under the real home; clean it up.)
    let id = rusqlite::Connection::open(work.join("browser.db")).and_then(|db| {
        db.query_row("SELECT value FROM settings WHERE key='profile_id'", [], |r| r.get::<_, String>(0))
    });
    if let (Ok(id), Some(real)) = (id, std::env::var_os("HOME")) {
        let _ = std::fs::remove_dir_all(
            Path::new(&real).join("Library/WebKit/my-browser/WebsiteDataStore").join(id.to_lowercase()),
        );
    }
}

#[test]
fn developer_tools_can_be_turned_off() {
    let mut b = launch();
    b.nav("go about:blank");
    assert_eq!(b.send("inspector"), "INSPECTOR 1");
    assert!(b.send("menu View").contains("|Show Web Inspector"));
    assert_eq!(b.bridge(&["settings.set", "devtools", "0"]), "true");
    assert_eq!(b.send("inspector"), "INSPECTOR 0");
}

#[test]
fn local_addresses_load_over_http_as_typed() {
    let base = serve();
    let port = base.rsplit(':').next().unwrap().to_string();
    let mut b = launch();
    assert_eq!(b.nav(&format!("barenter localhost:{port}/page/local")), format!("http://localhost:{port}/page/local"));
    assert!(b.state_until(|s| s.ends_with("title=local")).ends_with("title=local"), "not upgraded to HTTPS first");
}

#[test]
fn find_counts_matches_and_marks_the_current_one() {
    let mut b = launch();
    b.nav("go data:text/html,<p>alpha beta alpha <b>al</b>pha</p><p hidden>alpha</p>");
    assert_eq!(b.send("find alpha"), "FIND found=1");
    assert_eq!(b.send("findbar"), "FINDBAR open=1 text=alpha status=1 of 3", "hidden text isn't counted");
    assert_eq!(b.send("eval String([...CSS.highlights.keys()])"), "EVAL myb-find,myb-find-current");
    assert_eq!(
        b.send("eval CSS.highlights.get('myb-find').size + ' ' + CSS.highlights.get('myb-find-current').size"),
        "EVAL 2 1"
    );
    assert_eq!(b.send("findnext"), "FIND found=1");
    assert_eq!(b.send("findbar"), "FINDBAR open=1 text=alpha status=2 of 3");
    b.send("findnext");
    assert_eq!(b.send("findbar"), "FINDBAR open=1 text=alpha status=3 of 3", "a match across tags counts");
    b.send("findesc");
    let deadline = Instant::now() + Duration::from_secs(5);
    while b.send("eval String([...CSS.highlights.keys()])") != "EVAL " {
        assert!(Instant::now() < deadline, "closing the bar clears the marks");
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(b.send("eval document.querySelectorAll('style').length"), "EVAL 0", "and their style");
}

#[test]
fn folders_show_as_pages_and_files_open_in_the_tab() {
    let dir = TempProfile::new("folder");
    std::fs::create_dir_all(dir.0.join("sub dir")).unwrap();
    std::fs::write(dir.0.join("notes.txt"), "some notes").unwrap();
    std::fs::write(dir.0.join("doc.unknownkind"), "?").unwrap();
    std::fs::write(dir.0.join("\" onmouseover=\"x"), "").unwrap();
    let base = serve();
    let mut b = launch();
    let shown = b.nav(&format!("barenter {}", dir.path()));
    assert!(shown.starts_with("mybrowser://files/") && shown.ends_with('/'), "{shown}");
    assert_eq!(b.send("eval document.querySelectorAll('tbody tr').length"), "EVAL 4");
    let click = |name: &str| {
        format!("eval [...document.querySelectorAll('tbody a')].find(a => a.textContent == '{name}').click(), 1")
    };
    assert_eq!(b.send(&click("sub dir")), "EVAL 1");
    assert!(b.state_until(|s| s.ends_with("title=sub dir")).contains("/sub%20dir/ "));
    b.nav("back");
    assert_eq!(b.send(&click("notes.txt")), "EVAL 1");
    assert!(b.state_until(|s| s.ends_with("/notes.txt")).contains("url=file:///"), "loaded: the title is its address");
    assert_eq!(b.send("eval document.body.innerText.trim()"), "EVAL some notes");
    b.nav("back");
    writeln!(b.stdin, "{}", click("doc.unknownkind")).unwrap();
    assert!(b.wait_for("OPENLOCAL").starts_with("OPENLOCAL app "), "what WebKit can't show opens in its app");
    // (Its cancelled load ends in the page a moment later.)
    std::thread::sleep(Duration::from_millis(500));
    // Websites can't read folder pages.
    b.nav(&format!("go {base}/page/site"));
    let read = format!(
        "eval fetch('{shown}').then(r => r.text()).then(t => document.title = 'read ' + t.length, () => document.title = 'refused'), 1"
    );
    assert_eq!(b.send(&read), "EVAL 1");
    assert!(b.state_until(|s| s.ends_with("title=refused")).ends_with("title=refused"));
}

#[test]
fn pdfs_open_in_the_pdf_viewer() {
    let dir = TempProfile::new("pdf");
    let pdf = dir.0.join("doc.pdf");
    let mut b = launch_with(&["--on-top"]);
    b.nav(
        "go data:text/html,<p style=break-after:page>alpha one</p><p style=break-after:page>beta alpha</p><p>gamma</p>",
    );
    assert_eq!(b.send(&format!("printpdf {}", pdf.display())), "PRINTED 1");
    writeln!(b.stdin, "go file://{}", pdf.display()).unwrap();
    b.wait_all(&["LOADED", "PDFVIEW"]);
    assert_eq!(b.send("pdf"), "PDF pages=3 page=1 sidebar=0 twoup=0 rotation=0 highlights=0");
    assert_eq!(b.send("pdfcmd 2"), "PDF pages=3 page=2 sidebar=0 twoup=0 rotation=0 highlights=0", "next page");
    assert_eq!(b.send("pdfcmd 0"), "PDF pages=3 page=2 sidebar=1 twoup=0 rotation=0 highlights=0", "thumbnails");
    assert_eq!(b.send("pdfcmd 7"), "PDF pages=3 page=2 sidebar=1 twoup=0 rotation=90 highlights=0", "rotate");
    assert_eq!(b.send("pdfcmd 8"), "PDF pages=3 page=2 sidebar=1 twoup=1 rotation=90 highlights=0", "two pages");
    assert_eq!(b.send("find alpha"), "FIND found=1");
    assert_eq!(b.send("findbar"), "FINDBAR open=1 text=alpha status=1 of 2");
    b.send("pdfselect 0");
    assert!(b.send("pdfcmd 9").ends_with("highlights=1"), "highlight the selection");
    let saved = dir.0.join("saved.pdf");
    assert_eq!(b.send(&format!("savepage {}", saved.display())), "SAVED 1");
    assert!(std::fs::read(&saved).unwrap().windows(9).any(|w| w == b"Highlight"), "saved with the highlight");
    // Reading aloud selects each word as it's read (silent in bench mode).
    b.send("pdfcmd 10");
    // (From this page, page 2: "beta alpha".)
    let reading = b.until("pdf", |s| s.ends_with("reading=alpha"));
    assert!(reading.ends_with("reading=alpha"), "{reading}");
    b.send("pdfcmd 10");
    assert!(b.until("pdf", |s| !s.contains("reading=")).ends_with("highlights=1"), "stopped");
    b.nav("go about:blank");
    assert_eq!(b.send("pdf"), "PDF none", "leaving the PDF closes the viewer");
}
