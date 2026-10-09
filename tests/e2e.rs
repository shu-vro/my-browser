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
    let mut child = Command::new(env!("CARGO_BIN_EXE_my-browser"))
        .arg("--bench")
        .args(extra)
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
    let (child, stdin, lines) = spawn(extra);
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
    fn state_until(&mut self, ok: impl Fn(&str) -> bool) -> String {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let s = self.send("state");
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
/// `/file.txt` (a download), `/script/<js>` (runs the URL-decoded js).
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
        if b[i] == b'%' && i + 2 < b.len() + 0 && i + 2 <= b.len() - 1 + 1 {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn encode(js: &str) -> String {
    js.bytes()
        .map(|c| if c.is_ascii_alphanumeric() { (c as char).to_string() } else { format!("%{c:02X}") })
        .collect()
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
        if let Ok(db) = rusqlite::Connection::open(self.0.join("browser.db")) {
            if let Ok(id) = db.query_row("SELECT value FROM settings WHERE key='profile_id'", [], |r| r.get::<_, String>(0)) {
                if let Some(home) = std::env::var_os("HOME") {
                    let store = Path::new(&home).join("Library/WebKit/my-browser/WebsiteDataStore").join(id.to_lowercase());
                    let _ = std::fs::remove_dir_all(store);
                }
            }
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
    assert!(b.send("go http://127.0.0.1:1/").starts_with("FAILED "));
    // Browser still works afterwards.
    assert!(b.send("go about:blank").starts_with("LOADED "));
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
    { let want = format!("STATE back=0 forward=0 url={a_url} title=Page A"); assert_eq!(b.state_until(|s| s == want), want); }

    let b_url = b.nav(&format!("go {bb}"));
    { let want = format!("STATE back=1 forward=0 url={b_url} title=Page B"); assert_eq!(b.state_until(|s| s == want), want); }

    assert_eq!(b.nav("back"), a_url);
    { let want = format!("STATE back=0 forward=1 url={a_url} title=Page A"); assert_eq!(b.state_until(|s| s == want), want); }

    assert_eq!(b.nav("forward"), b_url);
    { let want = format!("STATE back=1 forward=0 url={b_url} title=Page B"); assert_eq!(b.state_until(|s| s == want), want); }

    assert_eq!(b.nav("reload"), b_url);
    { let want = format!("STATE back=1 forward=0 url={b_url} title=Page B"); assert_eq!(b.state_until(|s| s == want), want); }
}

#[test]
fn untitled_page_shows_its_url() {
    let mut b = launch();
    b.nav("go about:blank");
    { let want = "STATE back=0 forward=0 url=about:blank title=about:blank"; assert_eq!(b.state_until(|s| s == want), want); }
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
        "jit",
        "pagecache",
        "prewarm",
        "proccache",
        "suspend",
        "third_party_cookies_blocked",
        "tracking_prevention",
        "trackers",
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

    // Same name again: kept apart, not overwritten.
    writeln!(b.stdin, "go {base}/file.txt").unwrap();
    let line = b.wait_for("DOWNLOAD ");
    assert!(line.ends_with("file (2).txt"), "{line}");

    let list = b.bridge(&["downloads.list"]);
    assert!(list.contains("\"status\":\"done\"") && list.contains("\"bytes\":14"), "{list}");
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
    let s = b.bridge(&["settings.get"]);
    assert!(s.contains("\"search\":\"duckduckgo\"") && s.contains("\"block_trackers\":\"1\""), "{s}");
    assert_eq!(b.bridge(&["settings.set", "search", "brave"]), "true");
    assert!(b.bridge(&["settings.get"]).contains("\"search\":\"brave\""));
    let bad = b.send("bridge settings.set\tsearch\tevil.example");
    assert!(bad.starts_with("BRIDGE err "), "{bad}");
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
fn unknown_internal_page_fails() {
    let mut b = launch();
    assert!(b.send("go mybrowser://nope").starts_with("FAILED "));
}

// ---------------------------------------------------------------------------
// Extensions

#[test]
fn extension_content_script_runs_and_can_be_disabled() {
    let base = serve();
    let fixture = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/title-extension");
    // Throwaway profiles count as private browsing, where extensions don't run.
    let profile = TempProfile::new("extension");
    let mut b = launch_with(&["--profile", profile.path()]);
    let installed = b.bridge(&["ext.install", fixture]);
    assert!(installed.contains("\"name\":\"Title Marker\""), "{installed}");
    let id: String = installed.split("\"id\":").nth(1).unwrap().chars().take_while(char::is_ascii_digit).collect();

    b.nav(&format!("go {base}/page/hello"));
    assert!(b.state_until(|s| s.ends_with("title=EXT hello")).ends_with("title=EXT hello"), "content script ran");
    let list = b.bridge(&["ext.list"]);
    assert!(list.contains("\"loaded\":true") && list.contains("http://127.0.0.1/*"), "{list}");

    assert_eq!(b.bridge(&["ext.toggle", &id, "0"]), "true");
    b.nav(&format!("go {base}/page/plain"));
    std::thread::sleep(Duration::from_millis(300));
    assert!(b.send("state").ends_with("title=plain"), "disabled extension doesn't run");

    assert_eq!(b.bridge(&["ext.remove", &id]), "true");
    assert_eq!(b.bridge(&["ext.list"]), "[]");
    // The copy the browser made inside the profile is gone too.
    let left: Vec<_> = std::fs::read_dir(profile.0.join("extensions")).map(|d| d.flatten().collect()).unwrap_or_default();
    assert!(left.iter().all(|e| !e.file_name().to_string_lossy().ends_with("title-extension")), "{left:?}");
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
