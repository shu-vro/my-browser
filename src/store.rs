//! Profile storage: settings, history, downloads and installed extensions in
//! one SQLite file. Cookies and site data live in WebKit's own per-profile
//! store; this file only holds what the browser itself keeps. Nothing in it
//! ever leaves the machine.

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{params, Connection, OptionalExtension};

/// Settings the settings page may change, with their defaults.
pub const SETTINGS: [(&str, &str); 8] = [
    // duckduckgo | brave | startpage | google
    ("search", "duckduckgo"),
    // Page for new tabs; empty means a blank page.
    ("homepage", ""),
    // Background tabs unload their page after this many minutes (0 = never).
    ("tab_sleep_minutes", "5"),
    // Reopen last session's tabs (asleep, so they cost almost nothing) at launch.
    ("restore_session", "1"),
    // Built-in tracker blocker (Disconnect list).
    ("block_trackers", "1"),
    // Safe-browsing lookups send hashed URL prefixes to Apple/Google. Off by default.
    ("safe_browsing", "0"),
    // JavaScript JIT: faster JS, more memory. Applies after restart.
    ("jit", "1"),
    // Where downloads go; empty means ~/Downloads.
    ("downloads_dir", ""),
];

pub struct Store {
    db: Connection,
}

pub struct Visit {
    pub id: i64,
    pub url: String,
    pub title: String,
    pub at: i64,
}

pub struct Download {
    pub id: i64,
    pub url: String,
    pub path: String,
    /// active | done | failed | cancelled
    pub status: String,
    pub bytes: i64,
    pub at: i64,
}

pub struct Extension {
    pub id: i64,
    pub path: String,
    pub enabled: bool,
}

pub fn now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
}

impl Store {
    /// `None` opens a throwaway in-memory store (benchmarks and tests).
    pub fn open(path: Option<&Path>) -> rusqlite::Result<Self> {
        let db = match path {
            Some(p) => Connection::open(p)?,
            None => Connection::open_in_memory()?,
        };
        db.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA synchronous=NORMAL;
             CREATE TABLE IF NOT EXISTS settings (key TEXT PRIMARY KEY, value TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS history (
                 id INTEGER PRIMARY KEY, url TEXT NOT NULL, title TEXT NOT NULL DEFAULT '', at INTEGER NOT NULL);
             CREATE INDEX IF NOT EXISTS history_at ON history(at);
             CREATE TABLE IF NOT EXISTS downloads (
                 id INTEGER PRIMARY KEY, url TEXT NOT NULL, path TEXT NOT NULL,
                 status TEXT NOT NULL, bytes INTEGER NOT NULL DEFAULT 0, at INTEGER NOT NULL);
             CREATE TABLE IF NOT EXISTS extensions (
                 id INTEGER PRIMARY KEY, path TEXT NOT NULL UNIQUE, enabled INTEGER NOT NULL DEFAULT 1);
             CREATE TABLE IF NOT EXISTS session (pos INTEGER PRIMARY KEY, url TEXT NOT NULL, title TEXT NOT NULL);",
        )?;
        Ok(Store { db })
    }

    // ---- settings

    /// Raw value, including internal keys like `profile_id`.
    pub fn get(&self, key: &str) -> Option<String> {
        self.db
            .query_row("SELECT value FROM settings WHERE key = ?1", [key], |r| r.get(0))
            .optional()
            .ok()
            .flatten()
    }

    pub fn put(&self, key: &str, value: &str) {
        let _ = self.db.execute(
            "INSERT INTO settings (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            [key, value],
        );
    }

    /// A user setting, falling back to its default.
    pub fn setting(&self, key: &str) -> String {
        let default = SETTINGS.iter().find(|(k, _)| *k == key).map_or("", |(_, d)| d);
        self.get(key).unwrap_or_else(|| default.to_string())
    }

    pub fn flag(&self, key: &str) -> bool {
        self.setting(key) == "1"
    }

    /// Change a user setting. Rejects unknown keys and invalid values, since
    /// the settings page is web content.
    pub fn set_setting(&self, key: &str, value: &str) -> Result<(), String> {
        let valid = match key {
            "search" => ["duckduckgo", "brave", "startpage", "google"].contains(&value),
            "homepage" => value.is_empty() || value.starts_with("https://") || value.starts_with("http://"),
            "tab_sleep_minutes" => value.parse::<u32>().is_ok_and(|m| m <= 24 * 60),
            "restore_session" | "block_trackers" | "safe_browsing" | "jit" => value == "0" || value == "1",
            "downloads_dir" => value.is_empty() || Path::new(value).is_dir(),
            _ => return Err(format!("unknown setting {key}")),
        };
        if !valid {
            return Err(format!("invalid value for {key}"));
        }
        self.put(key, value);
        Ok(())
    }

    // ---- history

    pub fn add_visit(&self, url: &str, title: &str) -> i64 {
        let _ = self.db.execute("INSERT INTO history (url, title, at) VALUES (?1, ?2, ?3)", params![url, title, now()]);
        self.db.last_insert_rowid()
    }

    pub fn set_visit_title(&self, id: i64, title: &str) {
        let _ = self.db.execute("UPDATE history SET title = ?2 WHERE id = ?1", params![id, title]);
    }

    /// Newest first. `query` matches URL or title.
    pub fn history(&self, query: &str, limit: i64) -> Vec<Visit> {
        let like = format!("%{}%", query.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_"));
        let mut stmt = match self.db.prepare(
            "SELECT id, url, title, at FROM history
             WHERE url LIKE ?1 ESCAPE '\\' OR title LIKE ?1 ESCAPE '\\'
             ORDER BY at DESC, id DESC LIMIT ?2",
        ) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        stmt.query_map(params![like, limit], |r| {
            Ok(Visit { id: r.get(0)?, url: r.get(1)?, title: r.get(2)?, at: r.get(3)? })
        })
        .map(|rows| rows.filter_map(Result::ok).collect())
        .unwrap_or_default()
    }

    pub fn history_count(&self) -> i64 {
        self.db.query_row("SELECT COUNT(*) FROM history", [], |r| r.get(0)).unwrap_or(0)
    }

    pub fn delete_visit(&self, id: i64) {
        let _ = self.db.execute("DELETE FROM history WHERE id = ?1", [id]);
    }

    pub fn clear_history(&self) {
        let _ = self.db.execute("DELETE FROM history", []);
    }

    // ---- downloads

    pub fn add_download(&self, url: &str, path: &str) -> i64 {
        let _ = self.db.execute(
            "INSERT INTO downloads (url, path, status, at) VALUES (?1, ?2, 'active', ?3)",
            params![url, path, now()],
        );
        self.db.last_insert_rowid()
    }

    pub fn finish_download(&self, id: i64, status: &str, bytes: i64) {
        let _ = self.db.execute("UPDATE downloads SET status = ?2, bytes = ?3 WHERE id = ?1", params![id, status, bytes]);
    }

    pub fn downloads(&self) -> Vec<Download> {
        let Ok(mut stmt) = self.db.prepare("SELECT id, url, path, status, bytes, at FROM downloads ORDER BY id DESC")
        else {
            return Vec::new();
        };
        stmt.query_map([], |r| {
            Ok(Download { id: r.get(0)?, url: r.get(1)?, path: r.get(2)?, status: r.get(3)?, bytes: r.get(4)?, at: r.get(5)? })
        })
        .map(|rows| rows.filter_map(Result::ok).collect())
        .unwrap_or_default()
    }

    pub fn download(&self, id: i64) -> Option<Download> {
        self.downloads().into_iter().find(|d| d.id == id)
    }

    pub fn remove_download(&self, id: i64) {
        let _ = self.db.execute("DELETE FROM downloads WHERE id = ?1", [id]);
    }

    /// Forget finished downloads (the files stay); in-flight ones are kept.
    pub fn clear_downloads(&self) {
        let _ = self.db.execute("DELETE FROM downloads WHERE status != 'active'", []);
    }

    /// Downloads still marked active belong to a previous run that ended mid-download.
    pub fn fail_stale_downloads(&self) {
        let _ = self.db.execute("UPDATE downloads SET status = 'failed' WHERE status = 'active'", []);
    }

    // ---- extensions

    pub fn add_extension(&self, path: &str) -> i64 {
        let _ = self.db.execute("INSERT OR IGNORE INTO extensions (path) VALUES (?1)", [path]);
        self.db
            .query_row("SELECT id FROM extensions WHERE path = ?1", [path], |r| r.get(0))
            .unwrap_or(0)
    }

    pub fn extensions(&self) -> Vec<Extension> {
        let Ok(mut stmt) = self.db.prepare("SELECT id, path, enabled FROM extensions ORDER BY id") else {
            return Vec::new();
        };
        stmt.query_map([], |r| Ok(Extension { id: r.get(0)?, path: r.get(1)?, enabled: r.get::<_, i64>(2)? == 1 }))
            .map(|rows| rows.filter_map(Result::ok).collect())
            .unwrap_or_default()
    }

    pub fn set_extension_enabled(&self, id: i64, enabled: bool) {
        let _ = self.db.execute("UPDATE extensions SET enabled = ?2 WHERE id = ?1", params![id, enabled as i64]);
    }

    pub fn remove_extension(&self, id: i64) {
        let _ = self.db.execute("DELETE FROM extensions WHERE id = ?1", [id]);
    }

    // ---- session (open tabs, for restore at launch)

    pub fn save_session(&self, tabs: &[(String, String)]) {
        let _ = self.db.execute("DELETE FROM session", []);
        for (pos, (url, title)) in tabs.iter().enumerate() {
            let _ = self.db.execute("INSERT INTO session (pos, url, title) VALUES (?1, ?2, ?3)", params![pos as i64, url, title]);
        }
    }

    pub fn session(&self) -> Vec<(String, String)> {
        let Ok(mut stmt) = self.db.prepare("SELECT url, title FROM session ORDER BY pos") else { return Vec::new() };
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .map(|rows| rows.filter_map(Result::ok).collect())
            .unwrap_or_default()
    }
}

/// Escape a string for a JSON string literal (the settings page bridge speaks JSON).
pub fn json_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            // U+2028/2029 are valid JSON but break JS string literals in older engines.
            c if (c as u32) < 0x20 || c == '\u{2028}' || c == '\u{2029}' => {
                out.push_str(&format!("\\u{:04x}", c as u32))
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_defaults_and_validation() {
        let s = Store::open(None).unwrap();
        assert_eq!(s.setting("search"), "duckduckgo");
        assert!(s.flag("block_trackers"));
        assert!(!s.flag("safe_browsing"));
        assert!(s.set_setting("search", "brave").is_ok());
        assert_eq!(s.setting("search"), "brave");
        assert!(s.set_setting("search", "evil").is_err());
        assert!(s.set_setting("tab_sleep_minutes", "-1").is_err());
        assert!(s.set_setting("tab_sleep_minutes", "30").is_ok());
        assert!(s.set_setting("homepage", "javascript:alert(1)").is_err());
        assert!(s.set_setting("profile_id", "x").is_err(), "internal keys are not settable");
        assert!(s.set_setting("downloads_dir", "/definitely/not/here").is_err());
        assert!(s.set_setting("jit", "2").is_err());
    }

    #[test]
    fn history_search_and_delete() {
        let s = Store::open(None).unwrap();
        let a = s.add_visit("https://a.example/", "Alpha");
        s.add_visit("https://b.example/100%_off", "Beta");
        s.set_visit_title(a, "Alpha page");
        assert_eq!(s.history("", 10).len(), 2);
        assert_eq!(s.history("alpha", 10)[0].title, "Alpha page");
        // LIKE wildcards in the query are literal.
        assert_eq!(s.history("100%_", 10).len(), 1);
        assert_eq!(s.history("%", 10).len(), 1);
        assert_eq!(s.history_count(), 2);
        s.delete_visit(a);
        assert_eq!(s.history("", 10).len(), 1);
        s.clear_history();
        assert!(s.history("", 10).is_empty());
    }

    #[test]
    fn downloads_lifecycle() {
        let s = Store::open(None).unwrap();
        let id = s.add_download("https://x/f.zip", "/tmp/f.zip");
        assert_eq!(s.download(id).unwrap().status, "active");
        s.fail_stale_downloads();
        assert_eq!(s.download(id).unwrap().status, "failed");
        s.finish_download(id, "done", 42);
        let d = s.download(id).unwrap();
        assert_eq!((d.status.as_str(), d.bytes), ("done", 42));
        s.remove_download(id);
        assert!(s.downloads().is_empty());
        let active = s.add_download("https://x/a", "/tmp/a");
        let done = s.add_download("https://x/b", "/tmp/b");
        s.finish_download(done, "done", 1);
        s.clear_downloads();
        let left: Vec<i64> = s.downloads().iter().map(|d| d.id).collect();
        assert_eq!(left, vec![active], "only the in-flight download stays");
    }

    #[test]
    fn extensions_and_session() {
        let s = Store::open(None).unwrap();
        let id = s.add_extension("/x/ext");
        assert_eq!(s.add_extension("/x/ext"), id, "same path, same row");
        s.set_extension_enabled(id, false);
        assert!(!s.extensions()[0].enabled);
        s.remove_extension(id);
        assert!(s.extensions().is_empty());
        s.save_session(&[("https://a/".into(), "A".into()), ("https://b/".into(), "B".into())]);
        assert_eq!(s.session()[1].0, "https://b/");
    }

    #[test]
    fn persists_to_disk() {
        let dir = std::env::temp_dir().join(format!("mybrowser-store-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("browser.db");
        Store::open(Some(&path)).unwrap().put("profile_id", "abc");
        assert_eq!(Store::open(Some(&path)).unwrap().get("profile_id").as_deref(), Some("abc"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn json_escaping() {
        assert_eq!(json_str("a\"b\\c\n\u{1}\u{2028}é"), "\"a\\\"b\\\\c\\n\\u0001\\u2028é\"");
    }
}
