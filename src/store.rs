//! Profile storage: settings, history, downloads and installed extensions in
//! one SQLite file. Cookies and site data live in WebKit's own per-profile
//! store; this file only holds what the browser itself keeps. Nothing in it
//! ever leaves the machine.

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OptionalExtension, params};

/// Settings the settings page may change, with their defaults.
pub const SETTINGS: [(&str, &str); 13] = [
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
    // Websites can't reach localhost or the local network (sites can be allowed one by one).
    ("block_local_network", "1"),
    // Safe-browsing lookups send hashed URL prefixes to Apple/Google. Off by default.
    ("safe_browsing", "0"),
    // JavaScript JIT: faster JS, more memory. Applies after restart.
    ("jit", "1"),
    // Where downloads go; empty means ~/Downloads.
    ("downloads_dir", ""),
    // system | light | dark: the browser's look, which pages see as prefers-color-scheme.
    ("theme", "system"),
    // What the built-in new tab page shows.
    ("home_art", "1"),
    ("home_search", "1"),
    ("home_top_sites", "1"),
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

pub struct Bookmark {
    pub id: i64,
    pub url: String,
    pub title: String,
    /// `/`-separated path, e.g. `Imported from Chrome/Work`; empty = top level.
    pub folder: String,
}

pub struct Extension {
    pub id: i64,
    pub path: String,
    pub enabled: bool,
    /// click: off until its button is clicked on a site | always: on every site.
    pub access: String,
}

/// Per-site permissions and the values each may hold. A missing row means the
/// default: ask (camera, microphone, location, more than one download per
/// page), blocked (local network),
/// protections on (shields).
pub const PERMISSIONS: [(&str, [&str; 2]); 8] = [
    ("camera", ["allow", "deny"]),
    // deny = never offer to save passwords on the site.
    ("passwords", ["allow", "deny"]),
    ("screen", ["allow", "deny"]),
    ("downloads", ["allow", "deny"]),
    ("microphone", ["allow", "deny"]),
    ("location", ["allow", "deny"]),
    ("local_network", ["allow", "deny"]),
    ("shields", ["on", "off"]),
];

/// A site as permissions see it: the lowercase host name of an address.
pub fn site_of(url: &str) -> Option<String> {
    let rest = url.split_once("://")?.1;
    let authority = rest.split(['/', '?', '#']).next()?;
    let host = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    let host = if host.starts_with('[') { host.split_inclusive(']').next()? } else { host.split(':').next()? };
    (!host.is_empty()).then(|| host.to_ascii_lowercase())
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
             CREATE TABLE IF NOT EXISTS session (pos INTEGER PRIMARY KEY, url TEXT NOT NULL, title TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS permissions (
                 site TEXT NOT NULL, kind TEXT NOT NULL, value TEXT NOT NULL, PRIMARY KEY (site, kind)) WITHOUT ROWID;
             CREATE TABLE IF NOT EXISTS net_allowed (
                 ext INTEGER NOT NULL, rule TEXT NOT NULL, UNIQUE (ext, rule));
             CREATE TABLE IF NOT EXISTS bookmarks (
                 id INTEGER PRIMARY KEY, url TEXT NOT NULL, title TEXT NOT NULL DEFAULT '',
                 folder TEXT NOT NULL DEFAULT '', at INTEGER NOT NULL);
             CREATE INDEX IF NOT EXISTS bookmarks_url ON bookmarks(url);",
        )?;
        // Added after the first release: extensions run only where clicked unless set to "always".
        let has_access: bool = db.prepare("SELECT access FROM extensions LIMIT 0").is_ok();
        if !has_access {
            db.execute_batch("ALTER TABLE extensions ADD COLUMN access TEXT NOT NULL DEFAULT 'click'")?;
        }
        // Added in 0.2: which window each restored tab goes back to.
        if db.prepare("SELECT win FROM session LIMIT 0").is_err() {
            db.execute_batch("ALTER TABLE session ADD COLUMN win INTEGER NOT NULL DEFAULT 0")?;
        }
        Ok(Store { db })
    }

    // ---- settings

    /// Raw value, including internal keys like `profile_id`.
    pub fn get(&self, key: &str) -> Option<String> {
        self.db.query_row("SELECT value FROM settings WHERE key = ?1", [key], |r| r.get(0)).optional().ok().flatten()
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
            "theme" => ["system", "light", "dark"].contains(&value),
            "restore_session"
            | "block_trackers"
            | "block_local_network"
            | "safe_browsing"
            | "jit"
            | "home_art"
            | "home_search"
            | "home_top_sites" => value == "0" || value == "1",
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

    /// The most visited sites (scheme and host), most visited first, counted
    /// over the latest 5000 visits so it stays fast and follows current habits.
    pub fn top_sites(&self, limit: usize) -> Vec<String> {
        let Ok(mut stmt) = self.db.prepare("SELECT url FROM history ORDER BY id DESC LIMIT 5000") else {
            return Vec::new();
        };
        // site -> (visits, rank of its newest visit)
        let mut counts: std::collections::HashMap<String, (usize, usize)> = std::collections::HashMap::new();
        let urls =
            stmt.query_map([], |r| r.get::<_, String>(0)).map(|rows| rows.filter_map(Result::ok).collect::<Vec<_>>());
        for (i, url) in urls.unwrap_or_default().iter().enumerate() {
            let Some((scheme, rest)) = url.split_once("://") else { continue };
            let host = rest.split(['/', '?', '#']).next().unwrap_or("");
            if !host.is_empty() {
                counts.entry(format!("{scheme}://{host}/")).or_insert((0, i)).0 += 1;
            }
        }
        // Equal counts: the most recently visited first.
        let mut counts: Vec<_> = counts.into_iter().collect();
        counts.sort_by_key(|(_, (n, newest))| (std::cmp::Reverse(*n), *newest));
        counts.into_iter().take(limit).map(|(s, _)| s).collect()
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
        let _ =
            self.db.execute("UPDATE downloads SET status = ?2, bytes = ?3 WHERE id = ?1", params![id, status, bytes]);
    }

    pub fn downloads(&self) -> Vec<Download> {
        let Ok(mut stmt) = self.db.prepare("SELECT id, url, path, status, bytes, at FROM downloads ORDER BY id DESC")
        else {
            return Vec::new();
        };
        stmt.query_map([], |r| {
            Ok(Download {
                id: r.get(0)?,
                url: r.get(1)?,
                path: r.get(2)?,
                status: r.get(3)?,
                bytes: r.get(4)?,
                at: r.get(5)?,
            })
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
        self.db.query_row("SELECT id FROM extensions WHERE path = ?1", [path], |r| r.get(0)).unwrap_or(0)
    }

    pub fn extensions(&self) -> Vec<Extension> {
        let Ok(mut stmt) = self.db.prepare("SELECT id, path, enabled, access FROM extensions ORDER BY id") else {
            return Vec::new();
        };
        stmt.query_map([], |r| {
            Ok(Extension { id: r.get(0)?, path: r.get(1)?, enabled: r.get::<_, i64>(2)? == 1, access: r.get(3)? })
        })
        .map(|rows| rows.filter_map(Result::ok).collect())
        .unwrap_or_default()
    }

    pub fn set_extension_enabled(&self, id: i64, enabled: bool) {
        let _ = self.db.execute("UPDATE extensions SET enabled = ?2 WHERE id = ?1", params![id, enabled as i64]);
    }

    pub fn set_extension_access(&self, id: i64, access: &str) -> Result<(), String> {
        if !["click", "always"].contains(&access) {
            return Err("access must be click or always".into());
        }
        let _ = self.db.execute("UPDATE extensions SET access = ?2 WHERE id = ?1", params![id, access]);
        Ok(())
    }

    pub fn remove_extension(&self, id: i64) {
        let _ = self.db.execute("DELETE FROM extensions WHERE id = ?1", [id]);
        let _ = self.db.execute("DELETE FROM net_allowed WHERE ext = ?1", [id]);
    }

    // ---- network requests the user allowed (see netgate.rs)

    /// Every allowed (extension id, rule), oldest first.
    pub fn net_allowed(&self) -> Vec<(i64, String)> {
        let Ok(mut stmt) = self.db.prepare("SELECT ext, rule FROM net_allowed ORDER BY rowid") else {
            return Vec::new();
        };
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?))).map(|rows| rows.flatten().collect()).unwrap_or_default()
    }

    pub fn allow_net(&self, ext: i64, rule: &str) {
        let _ = self
            .db
            .execute("INSERT OR IGNORE INTO net_allowed (ext, rule) VALUES (?1, ?2)", rusqlite::params![ext, rule]);
    }

    pub fn forget_net(&self, ext: i64, rule: &str) {
        let _ = self.db.execute("DELETE FROM net_allowed WHERE ext = ?1 AND rule = ?2", rusqlite::params![ext, rule]);
    }

    // ---- site permissions

    pub fn permission(&self, site: &str, kind: &str) -> Option<String> {
        self.db
            .query_row("SELECT value FROM permissions WHERE site = ?1 AND kind = ?2", [site, kind], |r| r.get(0))
            .optional()
            .ok()
            .flatten()
    }

    /// Set a site's permission; "ask" (or "default") forgets it. Validated,
    /// since the settings page is web content.
    pub fn set_permission(&self, site: &str, kind: &str, value: &str) -> Result<(), String> {
        let values =
            PERMISSIONS.iter().find(|(k, _)| *k == kind).map(|(_, v)| v).ok_or(format!("unknown permission {kind}"))?;
        let valid_site = !site.is_empty()
            && site.len() <= 253
            && site.chars().all(|c| c.is_ascii_alphanumeric() || "-.:[]_".contains(c))
            && site == site.to_ascii_lowercase();
        if !valid_site {
            return Err("invalid site".into());
        }
        if value == "ask" || value == "default" {
            let _ = self.db.execute("DELETE FROM permissions WHERE site = ?1 AND kind = ?2", [site, kind]);
        } else if values.contains(&value) {
            let _ = self.db.execute(
                "INSERT INTO permissions (site, kind, value) VALUES (?1, ?2, ?3)
                 ON CONFLICT(site, kind) DO UPDATE SET value = excluded.value",
                [site, kind, value],
            );
        } else {
            return Err(format!("invalid value for {kind}"));
        }
        Ok(())
    }

    /// (site, kind, value), by site.
    pub fn permissions(&self) -> Vec<(String, String, String)> {
        let Ok(mut stmt) = self.db.prepare("SELECT site, kind, value FROM permissions ORDER BY site, kind") else {
            return Vec::new();
        };
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .map(|rows| rows.filter_map(Result::ok).collect())
            .unwrap_or_default()
    }

    /// Sites where `kind` is set to `value`, sorted.
    pub fn sites_with(&self, kind: &str, value: &str) -> Vec<String> {
        self.permissions().into_iter().filter(|(_, k, v)| k == kind && v == value).map(|(s, ..)| s).collect()
    }

    pub fn forget_site(&self, site: &str) {
        let _ = self.db.execute("DELETE FROM permissions WHERE site = ?1", [site]);
    }

    // ---- bookmarks

    /// Bookmark `url` in `folder`; an existing bookmark of it there is kept.
    pub fn add_bookmark(&self, url: &str, title: &str, folder: &str) -> i64 {
        let existing: Option<i64> = self
            .db
            .query_row("SELECT id FROM bookmarks WHERE url = ?1 AND folder = ?2", [url, folder], |r| r.get(0))
            .optional()
            .ok()
            .flatten();
        if let Some(id) = existing {
            return id;
        }
        let _ = self.db.execute(
            "INSERT INTO bookmarks (url, title, folder, at) VALUES (?1, ?2, ?3, ?4)",
            params![url, title, folder, now()],
        );
        self.db.last_insert_rowid()
    }

    /// A bookmark of exactly this address, if any.
    pub fn bookmark_of(&self, url: &str) -> Option<i64> {
        self.db
            .query_row("SELECT id FROM bookmarks WHERE url = ?1 LIMIT 1", [url], |r| r.get(0))
            .optional()
            .ok()
            .flatten()
    }

    pub fn bookmark(&self, id: i64) -> Option<Bookmark> {
        self.db
            .query_row("SELECT id, url, title, folder FROM bookmarks WHERE id = ?1", [id], |r| {
                Ok(Bookmark { id: r.get(0)?, url: r.get(1)?, title: r.get(2)?, folder: r.get(3)? })
            })
            .optional()
            .ok()
            .flatten()
    }

    /// By folder, then in the order they were added. `query` matches title,
    /// address or folder.
    pub fn bookmarks(&self, query: &str) -> Vec<Bookmark> {
        let like = format!("%{}%", query.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_"));
        let Ok(mut stmt) = self.db.prepare(
            "SELECT id, url, title, folder FROM bookmarks
             WHERE url LIKE ?1 ESCAPE '\\' OR title LIKE ?1 ESCAPE '\\' OR folder LIKE ?1 ESCAPE '\\'
             ORDER BY folder, id",
        ) else {
            return Vec::new();
        };
        stmt.query_map([like], |r| Ok(Bookmark { id: r.get(0)?, url: r.get(1)?, title: r.get(2)?, folder: r.get(3)? }))
            .map(|rows| rows.filter_map(Result::ok).collect())
            .unwrap_or_default()
    }

    /// Rename or move a bookmark. Validated: the settings page is web content.
    pub fn update_bookmark(&self, id: i64, title: &str, folder: &str) -> Result<(), String> {
        let empty_part = !folder.is_empty() && folder.split('/').any(|p| p.trim().is_empty());
        if title.len() > 1000 || folder.len() > 1000 || empty_part {
            return Err("invalid title or folder".into());
        }
        let _ = self.db.execute(
            "UPDATE bookmarks SET title = ?2, folder = ?3 WHERE id = ?1",
            params![id, title.trim(), folder.trim()],
        );
        Ok(())
    }

    pub fn remove_bookmark(&self, id: i64) {
        let _ = self.db.execute("DELETE FROM bookmarks WHERE id = ?1", [id]);
    }

    /// Add imported (folder, title, url) bookmarks in one transaction; ones
    /// already in that folder are skipped. Returns how many were added.
    pub fn import_bookmarks(&self, items: &[(String, String, String)]) -> usize {
        let _ = self.db.execute_batch("BEGIN");
        let added = items
            .iter()
            .filter(|(folder, title, url)| {
                self.db
                    .execute(
                        "INSERT INTO bookmarks (url, title, folder, at) SELECT ?1, ?2, ?3, ?4
                         WHERE NOT EXISTS (SELECT 1 FROM bookmarks WHERE url = ?1 AND folder = ?3)",
                        params![url, title, folder, now()],
                    )
                    .unwrap_or(0)
                    > 0
            })
            .count();
        let _ = self.db.execute_batch("COMMIT");
        added
    }

    /// Add imported (url, title, unix time) visits in one transaction; a
    /// visit to the same address at the same second is there already.
    pub fn import_visits(&self, visits: &[(String, String, i64)]) -> usize {
        let _ = self.db.execute_batch("BEGIN");
        let added = visits
            .iter()
            .filter(|(url, title, at)| {
                self.db
                    .execute(
                        "INSERT INTO history (url, title, at) SELECT ?1, ?2, ?3
                         WHERE NOT EXISTS (SELECT 1 FROM history WHERE at = ?3 AND url = ?1)",
                        params![url, title, at],
                    )
                    .unwrap_or(0)
                    > 0
            })
            .count();
        let _ = self.db.execute_batch("COMMIT");
        added
    }

    // ---- session (open tabs, for restore at launch)

    /// Open tabs as (window number, url, title), in tab-bar order.
    pub fn save_session(&self, tabs: &[(i64, String, String)]) {
        let _ = self.db.execute("DELETE FROM session", []);
        for (pos, (win, url, title)) in tabs.iter().enumerate() {
            let _ = self.db.execute(
                "INSERT INTO session (pos, win, url, title) VALUES (?1, ?2, ?3, ?4)",
                params![pos as i64, win, url, title],
            );
        }
    }

    pub fn session(&self) -> Vec<(i64, String, String)> {
        let Ok(mut stmt) = self.db.prepare("SELECT win, url, title FROM session ORDER BY pos") else {
            return Vec::new();
        };
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
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
        assert_eq!(s.setting("theme"), "system");
        assert!(s.set_setting("theme", "dark").is_ok());
        assert!(s.set_setting("theme", "purple").is_err());
        assert!(s.set_setting("home_top_sites", "0").is_ok());
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
    fn top_sites_by_visits() {
        let s = Store::open(None).unwrap();
        for u in [
            "https://a.example/1",
            "https://b.example/",
            "https://a.example/2?x",
            "http://b.example/",
            "https://a.example",
        ] {
            s.add_visit(u, "");
        }
        s.add_visit("https://c.example/", "");
        s.add_visit("https://b.example/x", "");
        assert_eq!(s.top_sites(3), ["https://a.example/", "https://b.example/", "https://c.example/"]);
        assert_eq!(s.top_sites(1).len(), 1);
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
        s.save_session(&[(0, "https://a/".into(), "A".into()), (1, "https://b/".into(), "B".into())]);
        assert_eq!(s.session()[1], (1, "https://b/".into(), "B".into()));
    }

    #[test]
    fn bookmarks_add_find_update_and_remove() {
        let s = Store::open(None).unwrap();
        let a = s.add_bookmark("https://a.example/", "A", "");
        assert_eq!(s.add_bookmark("https://a.example/", "A again", ""), a, "no duplicates in a folder");
        let b = s.add_bookmark("https://b.example/", "B", "Work/Docs");
        assert_eq!(s.bookmark_of("https://a.example/"), Some(a));
        assert_eq!(s.bookmark_of("https://c.example/"), None);
        assert_eq!(s.bookmarks("").iter().map(|b| b.id).collect::<Vec<_>>(), [a, b], "top level first");
        assert_eq!(s.bookmarks("work").len(), 1, "folders are searched");
        s.update_bookmark(b, "Docs", "Work").unwrap();
        assert_eq!(s.bookmark(b).unwrap().folder, "Work");
        assert!(s.update_bookmark(b, "x", "Work//Docs").is_err(), "no empty folder names");
        s.remove_bookmark(a);
        assert_eq!(s.bookmarks("").len(), 1);
    }

    #[test]
    fn imports_skip_what_is_already_there() {
        let s = Store::open(None).unwrap();
        let b = [("F".to_string(), "A".to_string(), "https://a/".to_string())];
        assert_eq!(s.import_bookmarks(&b), 1);
        assert_eq!(s.import_bookmarks(&b), 0, "importing twice adds nothing");
        let v = [("https://a/".to_string(), "A".to_string(), 1_700_000_000)];
        assert_eq!(s.import_visits(&v), 1);
        assert_eq!(s.import_visits(&v), 0);
        assert_eq!(s.history("", 10)[0].at, 1_700_000_000, "keeps the original time");
    }

    #[test]
    fn site_permissions() {
        let s = Store::open(None).unwrap();
        assert_eq!(s.permission("a.example", "camera"), None, "ask by default");
        s.set_permission("a.example", "camera", "allow").unwrap();
        s.set_permission("a.example", "shields", "off").unwrap();
        s.set_permission("b.example", "shields", "off").unwrap();
        assert_eq!(s.permission("a.example", "camera").as_deref(), Some("allow"));
        assert_eq!(s.sites_with("shields", "off"), ["a.example", "b.example"]);
        s.set_permission("a.example", "camera", "ask").unwrap();
        assert_eq!(s.permission("a.example", "camera"), None);
        assert!(s.set_permission("a.example", "camera", "maybe").is_err());
        assert!(s.set_permission("a.example", "notifications", "allow").is_err());
        assert!(s.set_permission("A.example", "camera", "allow").is_err(), "sites are lowercase");
        assert!(s.set_permission("a.example/\"", "camera", "allow").is_err());
        s.forget_site("a.example");
        assert_eq!(s.permissions().len(), 1);
    }

    #[test]
    fn sites_from_addresses() {
        assert_eq!(site_of("https://WWW.Example.com:8443/a?b#c").as_deref(), Some("www.example.com"));
        assert_eq!(site_of("http://user:pw@host.example/").as_deref(), Some("host.example"));
        assert_eq!(site_of("http://[::1]:8080/").as_deref(), Some("[::1]"));
        assert_eq!(site_of("about:blank"), None);
        assert_eq!(site_of("https:///x"), None);
    }

    #[test]
    fn old_profiles_get_the_extension_access_column() {
        let dir = std::env::temp_dir().join(format!("mybrowser-migrate-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("browser.db");
        let old = Connection::open(&path).unwrap();
        old.execute_batch("CREATE TABLE extensions (id INTEGER PRIMARY KEY, path TEXT NOT NULL UNIQUE, enabled INTEGER NOT NULL DEFAULT 1); INSERT INTO extensions (path) VALUES ('/x');").unwrap();
        drop(old);
        let s = Store::open(Some(&path)).unwrap();
        assert_eq!(s.extensions()[0].access, "click", "existing extensions become click-to-run");
        s.set_extension_access(1, "always").unwrap();
        assert!(s.set_extension_access(1, "sometimes").is_err());
        drop(s);
        assert_eq!(Store::open(Some(&path)).unwrap().extensions()[0].access, "always");
        std::fs::remove_dir_all(&dir).unwrap();
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
