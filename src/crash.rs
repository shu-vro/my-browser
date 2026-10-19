//! Crash reports, sent only if the user chooses to.
//!
//! The browser has no telemetry, so nothing reports a crash by itself. Two
//! things are kept on this Mac instead:
//! - a panic log in `<profile>/crashes/` (the panic hook writes the message
//!   and where it happened: with `panic = "abort"` it's lost otherwise), and
//! - the report macOS writes for any crash, in `~/Library/Logs/DiagnosticReports`.
//!
//! On the next launch the browser asks whether to report it. "Report" opens a
//! pre-filled GitHub issue in a tab (the user sees exactly what's in it and
//! submits it themselves) and shows the full report in Finder to attach.

use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use objc2_foundation::{NSArray, NSData, NSDictionary, NSJSONReadingOptions, NSJSONSerialization, NSNumber, NSString};

/// Where reports go: the project's issue tracker.
const ISSUES: &str = "https://github.com/shu-vro/my-browser/issues/new";
/// GitHub refuses very long issue URLs; frames past this are dropped.
const MAX_BODY: usize = 6000;

/// The version shown to users: the bundle's, else Cargo's.
pub fn version() -> String {
    objc2_foundation::NSBundle::mainBundle()
        .objectForInfoDictionaryKey(&NSString::from_str("CFBundleShortVersionString"))
        .and_then(|v| v.downcast::<NSString>().ok())
        .map(|v| v.to_string())
        .unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_string())
}

pub fn macos_version() -> String {
    objc2_foundation::NSProcessInfo::processInfo().operatingSystemVersionString().to_string()
}

/// Set by the bench harness's `crash`: after the log is written the process
/// exits instead of aborting, so tests don't leave a macOS crash report and
/// its "quit unexpectedly" dialog behind. Real crashes still abort.
pub static REQUESTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Write a panic's message, place and backtrace to `dir` before the process
/// aborts. No profile folder (throwaway profiles) means stderr only.
pub fn install(dir: Option<PathBuf>) {
    let header = format!("my-browser {} ({})", version(), macos_version());
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if let Some(dir) = &dir {
            let _ = std::fs::create_dir_all(dir);
            let at = now_ms();
            let backtrace = std::backtrace::Backtrace::force_capture();
            let text = format!("{header}\n{info}\n\nBacktrace:\n{backtrace}\n");
            let _ = std::fs::write(dir.join(format!("panic-{at}.log")), text);
        }
        default(info);
        if REQUESTED.load(std::sync::atomic::Ordering::Relaxed) {
            std::process::exit(101);
        }
    }));
}

/// Milliseconds since 1970: crashes right after launch must not tie with it.
pub fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64)
}

/// Reports written after `since` (from `now_ms`): panic logs in `crashes`,
/// and macOS's reports for this app in `system` (None skips them).
pub fn reports_since(crashes: Option<&Path>, system: Option<&Path>, since: i64) -> Vec<PathBuf> {
    let written_since = |p: &Path| {
        let modified = p.metadata().and_then(|m| m.modified()).ok();
        modified.and_then(|t| t.duration_since(UNIX_EPOCH).ok()).is_some_and(|d| d.as_millis() as i64 > since)
    };
    let mut found = Vec::new();
    let mut scan = |dir: &Path, prefix: &str, ext: &str| {
        let Ok(entries) = std::fs::read_dir(dir) else { return };
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.starts_with(prefix) && name.ends_with(ext) && written_since(&e.path()) {
                found.push(e.path());
            }
        }
    };
    if let Some(d) = crashes {
        scan(d, "panic-", ".log");
    }
    if let Some(d) = system {
        scan(d, "my-browser-", ".ips");
    }
    found.sort();
    found
}

/// What a report says, short enough for an issue: a panic log as written,
/// or the exception and the crashed thread's frames from a macOS report.
pub fn summary(path: &Path) -> String {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    if path.extension().is_some_and(|e| e == "ips") { ips_summary(&text).unwrap_or(text) } else { text }
}

/// A macOS crash report (`.ips`): a JSON header line, then the JSON body.
fn ips_summary(text: &str) -> Option<String> {
    let (_, body) = text.split_once('\n')?;
    let json = NSJSONSerialization::JSONObjectWithData_options_error(
        &NSData::with_bytes(body.as_bytes()),
        NSJSONReadingOptions::empty(),
    )
    .ok()?;
    let root = json.downcast_ref::<NSDictionary>()?;
    let get = |d: &NSDictionary, k: &str| d.objectForKey(&NSString::from_str(k));
    let string =
        |d: &NSDictionary, k: &str| get(d, k).and_then(|v| v.downcast::<NSString>().ok()).map(|s| s.to_string());
    let number =
        |d: &NSDictionary, k: &str| get(d, k).and_then(|v| v.downcast::<NSNumber>().ok()).map(|n| n.integerValue());
    let mut out = String::new();
    if let Some(ex) = get(root, "exception").and_then(|e| e.downcast::<NSDictionary>().ok()) {
        let kind = string(&ex, "type").unwrap_or_default();
        let signal = string(&ex, "signal").unwrap_or_default();
        out += &format!("Exception: {kind} {signal}\n");
    }
    let images: Vec<String> = get(root, "usedImages")
        .and_then(|i| i.downcast::<NSArray>().ok())
        .map(|a| {
            a.iter()
                .map(|i| i.downcast_ref::<NSDictionary>().and_then(|d| string(d, "name")).unwrap_or_else(|| "?".into()))
                .collect()
        })
        .unwrap_or_default();
    let faulting = number(root, "faultingThread").unwrap_or(0) as usize;
    let threads = get(root, "threads")?.downcast::<NSArray>().ok()?;
    let thread = threads.iter().nth(faulting)?.downcast::<NSDictionary>().ok()?;
    out += &format!("Crashed thread {faulting}:\n");
    for (i, frame) in get(&thread, "frames")?.downcast::<NSArray>().ok()?.iter().enumerate().take(40) {
        let Some(f) = frame.downcast_ref::<NSDictionary>() else { continue };
        let image = number(f, "imageIndex").and_then(|i| images.get(i as usize).cloned()).unwrap_or_default();
        let symbol = string(f, "symbol").unwrap_or_default();
        out += &format!("{i:2} {image} +0x{:x} {symbol}\n", number(f, "imageOffset").unwrap_or(0));
    }
    Some(out)
}

/// A new-issue link with the report filled in.
pub fn issue_url(summary: &str) -> String {
    let first = summary.lines().find(|l| l.starts_with("Exception:") || l.contains("panicked at")).unwrap_or("crash");
    let mut body = format!(
        "my-browser {} on {}\n\nWhat were you doing when it crashed?\n\n\n```\n{}\n```\n",
        version(),
        macos_version(),
        summary.trim()
    );
    if body.len() > MAX_BODY {
        let cut = (0..=MAX_BODY).rev().find(|&i| body.is_char_boundary(i)).unwrap_or(0);
        body.truncate(cut);
        body += "\n…(truncated; please attach the full report)\n```\n";
    }
    format!("{ISSUES}?title={}&body={}", encode(&format!("Crash: {}", first.trim())), encode(&body))
}

fn encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const IPS: &str = r#"{"app_name":"my-browser","bug_type":"309"}
{"exception":{"type":"EXC_CRASH","signal":"SIGABRT"},"faultingThread":1,
 "usedImages":[{"name":"my-browser"},{"name":"libsystem_kernel.dylib"}],
 "threads":[{"frames":[{"imageIndex":0,"imageOffset":16}]},
            {"frames":[{"imageIndex":1,"imageOffset":4660,"symbol":"__pthread_kill"},{"imageIndex":0,"imageOffset":255}]}]}"#;

    #[test]
    fn macos_reports_are_summarised() {
        let s = ips_summary(IPS).unwrap();
        assert!(s.starts_with("Exception: EXC_CRASH SIGABRT\nCrashed thread 1:\n"), "{s}");
        assert!(s.contains(" 0 libsystem_kernel.dylib +0x1234 __pthread_kill\n"), "{s}");
        assert!(s.contains(" 1 my-browser +0xff \n"), "{s}");
        assert!(ips_summary("not json").is_none());
    }

    #[test]
    fn issue_links_are_encoded_and_bounded() {
        let url = issue_url("my-browser 0.2.0\npanicked at src/tab.rs:1:2:\nboom & <stuff>");
        assert!(
            url.starts_with(&format!("{ISSUES}?title=Crash%3A%20panicked%20at%20src%2Ftab.rs%3A1%3A2%3A&body=")),
            "{url}"
        );
        assert_eq!(url.matches('&').count(), 1, "only the separator: the text is encoded");
        assert!(!url.contains(' '));
        let long = "x".repeat(50_000);
        assert!(issue_url(&long).len() < MAX_BODY * 3 + 300);
    }

    #[test]
    fn only_new_reports_of_this_app_are_found() {
        let dir = std::env::temp_dir().join(format!("mybrowser-crash-{}", std::process::id()));
        let (crashes, system) = (dir.join("crashes"), dir.join("system"));
        std::fs::create_dir_all(&crashes).unwrap();
        std::fs::create_dir_all(&system).unwrap();
        for p in [crashes.join("panic-1.log"), system.join("my-browser-2026.ips"), system.join("Safari-2026.ips")] {
            std::fs::write(p, "x").unwrap();
        }
        let found = reports_since(Some(&crashes), Some(&system), 0);
        assert_eq!(found.len(), 2, "{found:?}");
        assert!(reports_since(Some(&crashes), None, i64::MAX).is_empty(), "older than the last check");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
