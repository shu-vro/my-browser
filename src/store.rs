//! Profile storage: settings, history, downloads and installed extensions in
//! one SQLite file. Cookies and site data live in WebKit's own per-profile
//! store; this file only holds what the browser itself keeps. Nothing in it
//! ever leaves the machine.

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OptionalExtension, params};

/// Settings the settings page may change, with their defaults.
pub const SETTINGS: [(&str, &str); 27] = [
    // duckduckgo | brave | startpage | google
    ("search", "duckduckgo"),
    // Page for new tabs; empty means a blank page.
    ("homepage", ""),
    // Background tabs unload their page after this many minutes (0 = never).
    ("tab_sleep_minutes", "5"),
    // At most this many background tabs stay awake (0 = no limit); the least
    // recently used beyond it sleep, and links opened in the background beyond
    // it open asleep.
    ("awake_tabs_max", "4"),
    // Reopen last session's tabs (asleep, so they cost almost nothing) at launch.
    ("restore_session", "1"),
    // Built-in tracker blocker (Disconnect list).
    ("block_trackers", "1"),
    // Cookie consent banners hidden (privacy.rs, BANNERS); consent is never given.
    ("hide_cookie_banners", "1"),
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
    ("home_stats", "1"),
    // Address bar suggestions from past searches and history; searches are saved only while on.
    ("address_suggestions", "1"),
    // Touch ID (or the Mac's password) before a saved password is filled or copied.
    ("password_auth", "1"),
    // Translate Page into this language code; empty means the Mac's language.
    ("translate_to", ""),
    // Web Inspector: Inspect Element in the page menu, View → Show Web Inspector.
    ("devtools", "1"),
    // Leaving a tab whose video is playing with sound puts it in picture in picture.
    ("auto_pip", "1"),
    // Gestures: pinch (and two-finger double-tap) to zoom, two-finger swipe
    // between pages, ⌘ + scroll to zoom.
    ("pinch_zoom", "1"),
    ("swipe_navigation", "1"),
    ("scroll_zoom", "1"),
    // Page zoom for sites without their own, in percent (one of ZOOM_STEPS).
    ("default_zoom", "100"),
    // Built-in bangs that are on (`!w rust`), comma-separated: none until the
    // user turns them on in Settings → General.
    ("bangs", ""),
    // file | clipboard: where Take Screenshot puts the picture (last chosen).
    ("screenshot_to", "file"),
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

/// An address bar suggestion.
#[derive(Clone, Debug, PartialEq)]
pub enum Suggestion {
    /// A search typed before.
    Search(String),
    /// A visited page: (address, title).
    Page(String, String),
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

/// A saved address or card for forms (Settings → Autofill).
#[derive(Clone, Debug, PartialEq)]
pub struct Autofill {
    pub id: i64,
    /// address | card
    pub kind: String,
    /// (field, value), fields from `AUTOFILL_FIELDS`. A card's number isn't
    /// here: it's in the keychain under its `ref`.
    pub fields: Vec<(String, String)>,
}

impl Autofill {
    pub fn get(&self, field: &str) -> &str {
        self.fields.iter().find(|(k, _)| k == field).map_or("", |(_, v)| v)
    }
}

/// What each kind keeps. `ref` names a card's number in the keychain; `last4`
/// shows which card it is.
pub const AUTOFILL_FIELDS: [(&str, &[&str]); 2] = [
    ("address", &["name", "organization", "street", "city", "region", "postal", "country", "email", "tel"]),
    ("card", &["name", "month", "year", "brand", "last4", "ref"]),
];

/// A tab of the saved session: (window number, url, title, pinned).
pub type SessionTab = (i64, String, String, bool);

pub struct Extension {
    pub id: i64,
    pub path: String,
    pub enabled: bool,
    /// click: off until its button is clicked on a site | always: on every site.
    pub access: String,
    /// The user's folder it was copied from ("" for packages): Reload copies it again.
    pub source: String,
    /// Its Chrome Web Store id if it came from there ("" otherwise): Update downloads it again.
    pub webstore: String,
}

/// Per-site permissions and the values each may hold. A missing row means the
/// default: ask (camera, microphone, location, more than one download per
/// page), blocked (local network),
/// protections on (shields), allowed (JavaScript).
pub const PERMISSIONS: [(&str, [&str; 2]); 9] = [
    ("camera", ["allow", "deny"]),
    // deny = the site's own scripts don't run.
    ("javascript", ["allow", "deny"]),
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
             CREATE INDEX IF NOT EXISTS bookmarks_url ON bookmarks(url);
             CREATE TABLE IF NOT EXISTS zoom (site TEXT PRIMARY KEY, level REAL NOT NULL) WITHOUT ROWID;
             CREATE TABLE IF NOT EXISTS searches (text TEXT PRIMARY KEY, at INTEGER NOT NULL) WITHOUT ROWID;
             CREATE TABLE IF NOT EXISTS keywords (keyword TEXT PRIMARY KEY, url TEXT NOT NULL) WITHOUT ROWID;
             CREATE TABLE IF NOT EXISTS autofill (
                 id INTEGER PRIMARY KEY, kind TEXT NOT NULL, fields TEXT NOT NULL, at INTEGER NOT NULL);",
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
        // Added in 0.3: the Chrome Web Store id of extensions added from there, for updates.
        if db.prepare("SELECT webstore FROM extensions LIMIT 0").is_err() {
            db.execute_batch("ALTER TABLE extensions ADD COLUMN webstore TEXT NOT NULL DEFAULT ''")?;
        }
        // Added in 0.3: pinned tabs.
        if db.prepare("SELECT pinned FROM session LIMIT 0").is_err() {
            db.execute_batch("ALTER TABLE session ADD COLUMN pinned INTEGER NOT NULL DEFAULT 0")?;
        }
        // Added in 0.3: the folder an unpacked extension was copied from, for Reload.
        if db.prepare("SELECT source FROM extensions LIMIT 0").is_err() {
            db.execute_batch("ALTER TABLE extensions ADD COLUMN source TEXT NOT NULL DEFAULT ''")?;
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

    pub fn delete(&self, key: &str) {
        let _ = self.db.execute("DELETE FROM settings WHERE key = ?1", [key]);
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
            "awake_tabs_max" => value.parse::<u32>().is_ok_and(|n| n <= 100),
            "theme" => ["system", "light", "dark"].contains(&value),
            "restore_session"
            | "block_trackers"
            | "block_local_network"
            | "hide_cookie_banners"
            | "safe_browsing"
            | "jit"
            | "home_art"
            | "home_search"
            | "home_top_sites"
            | "home_stats"
            | "address_suggestions"
            | "password_auth"
            | "devtools"
            | "auto_pip"
            | "pinch_zoom"
            | "swipe_navigation"
            | "scroll_zoom" => value == "0" || value == "1",
            "default_zoom" => crate::tab::ZOOM_STEPS.iter().any(|z| (z * 100.0).round().to_string() == value),
            "downloads_dir" => value.is_empty() || Path::new(value).is_dir(),
            "translate_to" => crate::translate::valid_language(value),
            "screenshot_to" => ["file", "clipboard"].contains(&value),
            "bangs" => value.is_empty() || value.split(',').all(|b| crate::BANGS.iter().any(|(k, _)| *k == b)),
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

    /// History and the searches typed in the address bar.
    pub fn clear_history(&self) {
        let _ = self.db.execute("DELETE FROM history", []);
        self.clear_searches();
    }

    // ---- searches typed in the address bar (for its suggestions)

    pub fn add_search(&self, text: &str) {
        let text = text.trim();
        if !text.is_empty() && text.len() <= 500 {
            let _ = self.db.execute(
                "INSERT INTO searches (text, at) VALUES (?1, ?2) ON CONFLICT(text) DO UPDATE SET at = excluded.at",
                params![text, now()],
            );
        }
    }

    pub fn search_count(&self) -> i64 {
        self.db.query_row("SELECT COUNT(*) FROM searches", [], |r| r.get(0)).unwrap_or(0)
    }

    pub fn clear_searches(&self) {
        let _ = self.db.execute("DELETE FROM searches", []);
    }

    /// The latest searches, newest first: what ↑/↓ show in an untouched
    /// address bar or new tab search.
    pub fn recent_searches(&self, limit: usize) -> Vec<Suggestion> {
        let Ok(mut stmt) = self.db.prepare("SELECT text FROM searches ORDER BY at DESC, text LIMIT ?1") else {
            return Vec::new();
        };
        let rows = stmt.query_map(params![limit as i64], |r| r.get::<_, String>(0));
        rows.into_iter().flatten().flatten().map(Suggestion::Search).collect()
    }

    /// What the address bar suggests for `typed`: past searches containing
    /// it (newest first, those starting with it before the rest), then
    /// visited pages whose address or title contains it (most visited
    /// first), at most `limit` in all.
    pub fn suggestions(&self, typed: &str, limit: usize) -> Vec<Suggestion> {
        let typed = typed.trim();
        if typed.is_empty() {
            return Vec::new();
        }
        let like = format!("%{}%", typed.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_"));
        let prefix = format!("{}%", &like[1..]);
        let mut out: Vec<Suggestion> = Vec::new();
        if let Ok(mut stmt) = self.db.prepare(
            "SELECT text FROM searches WHERE text LIKE ?1 ESCAPE '\\'
             ORDER BY text LIKE ?2 ESCAPE '\\' DESC, at DESC LIMIT ?3",
        ) {
            let rows = stmt.query_map(params![like, prefix, (limit / 2).max(1) as i64], |r| r.get::<_, String>(0));
            out.extend(rows.into_iter().flatten().flatten().map(Suggestion::Search));
        }
        if let Ok(mut stmt) = self.db.prepare(
            "SELECT url, MAX(title) FROM history WHERE url LIKE ?1 ESCAPE '\\' OR title LIKE ?1 ESCAPE '\\'
             GROUP BY url ORDER BY COUNT(*) DESC, MAX(at) DESC LIMIT ?2",
        ) {
            let rows = stmt.query_map(params![like, limit as i64 * 2], |r| Ok(Suggestion::Page(r.get(0)?, r.get(1)?)));
            // A search engine's result page repeats a search already listed.
            let results = |u: &str| crate::SEARCH_ENGINES.iter().any(|(_, base)| u.starts_with(base));
            out.extend(
                rows.into_iter().flatten().flatten().filter(|p| !matches!(p, Suggestion::Page(u, _) if results(u))),
            );
        }
        out.truncate(limit);
        out
    }

    // ---- search keywords (`w rust` searches the site set for `w`)

    /// (keyword, address with `%s` for the query), by keyword.
    pub fn keywords(&self) -> Vec<(String, String)> {
        let Ok(mut stmt) = self.db.prepare("SELECT keyword, url FROM keywords ORDER BY keyword") else {
            return Vec::new();
        };
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?))).map(|rows| rows.flatten().collect()).unwrap_or_default()
    }

    /// Add or change a keyword. Validated: the settings page is web content.
    pub fn set_keyword(&self, keyword: &str, url: &str) -> Result<(), String> {
        let keyword = keyword.trim().trim_start_matches('!').to_lowercase();
        let short = !keyword.is_empty() && keyword.len() <= 20;
        if !short || !keyword.chars().all(|c| c.is_alphanumeric() || "._-".contains(c)) {
            return Err("A keyword is one short word (letters, digits, . _ -).".into());
        }
        let web = url.starts_with("https://") || url.starts_with("http://");
        if !web || !url.contains("%s") || url.len() > 2000 || url.chars().any(char::is_whitespace) {
            return Err("The address must start with https:// and have %s where the search goes.".into());
        }
        let _ = self.db.execute(
            "INSERT INTO keywords (keyword, url) VALUES (?1, ?2) ON CONFLICT(keyword) DO UPDATE SET url = excluded.url",
            [&keyword, url],
        );
        Ok(())
    }

    pub fn delete_keyword(&self, keyword: &str) {
        let _ = self.db.execute("DELETE FROM keywords WHERE keyword = ?1", [keyword]);
    }

    // ---- autofill (addresses and cards)

    pub fn autofill(&self, kind: Option<&str>) -> Vec<Autofill> {
        let Ok(mut stmt) = self.db.prepare("SELECT id, kind, fields FROM autofill ORDER BY at DESC, id DESC") else {
            return Vec::new();
        };
        let rows = stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?)));
        rows.map(|rows| rows.flatten().collect::<Vec<_>>())
            .unwrap_or_default()
            .into_iter()
            .filter(|(_, k, _)| kind.is_none_or(|want| want == k))
            .map(|(id, kind, fields)| Autofill { id, kind, fields: parse_fields(&fields) })
            .collect()
    }

    /// Add (id 0) or replace an entry; returns its id. Only the kind's own
    /// fields are kept, each at most 500 characters.
    pub fn save_autofill(&self, id: i64, kind: &str, fields: &[(String, String)]) -> Result<i64, String> {
        let allowed = AUTOFILL_FIELDS.iter().find(|(k, _)| *k == kind).ok_or("unknown kind")?.1;
        if let Some((k, _)) = fields.iter().find(|(k, v)| !allowed.contains(&k.as_str()) || v.chars().count() > 500) {
            return Err(format!("invalid field {k}"));
        }
        let text: String =
            fields.iter().map(|(k, v)| format!("{k}\t{}\n", v.replace(['\t', '\n', '\r'], " "))).collect();
        if id == 0 {
            let _ = self
                .db
                .execute("INSERT INTO autofill (kind, fields, at) VALUES (?1, ?2, ?3)", params![kind, text, now()]);
            return Ok(self.db.last_insert_rowid());
        }
        let changed = self.db.execute(
            "UPDATE autofill SET fields = ?3, at = ?4 WHERE id = ?1 AND kind = ?2",
            params![id, kind, text, now()],
        );
        if changed.unwrap_or(0) == 0 { Err("no such entry".into()) } else { Ok(id) }
    }

    pub fn delete_autofill(&self, id: i64) {
        let _ = self.db.execute("DELETE FROM autofill WHERE id = ?1", [id]);
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

    pub fn add_extension(&self, path: &str, source: &str) -> i64 {
        let _ = self.db.execute("INSERT OR IGNORE INTO extensions (path, source) VALUES (?1, ?2)", [path, source]);
        self.db.query_row("SELECT id FROM extensions WHERE path = ?1", [path], |r| r.get(0)).unwrap_or(0)
    }

    pub fn extensions(&self) -> Vec<Extension> {
        let Ok(mut stmt) =
            self.db.prepare("SELECT id, path, enabled, access, source, webstore FROM extensions ORDER BY id")
        else {
            return Vec::new();
        };
        stmt.query_map([], |r| {
            Ok(Extension {
                id: r.get(0)?,
                path: r.get(1)?,
                enabled: r.get::<_, i64>(2)? == 1,
                access: r.get(3)?,
                source: r.get(4)?,
                webstore: r.get(5)?,
            })
        })
        .map(|rows| rows.filter_map(Result::ok).collect())
        .unwrap_or_default()
    }

    pub fn set_extension_webstore(&self, id: i64, webstore: &str) {
        let _ = self.db.execute("UPDATE extensions SET webstore = ?2 WHERE id = ?1", params![id, webstore]);
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

    // ---- page zoom per site

    /// The zoom the user chose for this site, if any (else the default zoom).
    pub fn zoom(&self, site: &str) -> Option<f64> {
        self.db.query_row("SELECT level FROM zoom WHERE site = ?1", [site], |r| r.get(0)).ok()
    }

    /// Remember a site's zoom; None (back to the default) forgets it.
    pub fn set_zoom(&self, site: &str, level: Option<f64>) {
        let _ = match level {
            None => self.db.execute("DELETE FROM zoom WHERE site = ?1", [site]),
            Some(level) => self.db.execute(
                "INSERT INTO zoom (site, level) VALUES (?1, ?2) ON CONFLICT(site) DO UPDATE SET level = excluded.level",
                params![site, level],
            ),
        };
    }

    /// Every site's zoom: (site, level).
    pub fn zooms(&self) -> Vec<(String, f64)> {
        let Ok(mut stmt) = self.db.prepare("SELECT site, level FROM zoom ORDER BY site") else { return Vec::new() };
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?))).map(|rows| rows.flatten().collect()).unwrap_or_default()
    }

    /// Raw settings rows whose key starts with `prefix` (`shortcut:`).
    pub fn settings_with_prefix(&self, prefix: &str) -> Vec<(String, String)> {
        let Ok(mut stmt) =
            self.db.prepare("SELECT key, value FROM settings WHERE substr(key, 1, ?2) = ?1 ORDER BY key")
        else {
            return Vec::new();
        };
        stmt.query_map(params![prefix, prefix.len() as i64], |r| Ok((r.get(0)?, r.get(1)?)))
            .map(|rows| rows.flatten().collect())
            .unwrap_or_default()
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

    /// Open tabs, in tab-bar order.
    pub fn save_session(&self, tabs: &[SessionTab]) {
        let _ = self.db.execute("DELETE FROM session", []);
        for (pos, (win, url, title, pinned)) in tabs.iter().enumerate() {
            let _ = self.db.execute(
                "INSERT INTO session (pos, win, url, title, pinned) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![pos as i64, win, url, title, *pinned as i64],
            );
        }
    }

    pub fn session(&self) -> Vec<SessionTab> {
        let Ok(mut stmt) = self.db.prepare("SELECT win, url, title, pinned FROM session ORDER BY pos") else {
            return Vec::new();
        };
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get::<_, i64>(3)? == 1)))
            .map(|rows| rows.filter_map(Result::ok).collect())
            .unwrap_or_default()
    }
}

/// `field\tvalue` lines.
fn parse_fields(text: &str) -> Vec<(String, String)> {
    text.lines().filter_map(|l| l.split_once('\t')).map(|(k, v)| (k.to_string(), v.to_string())).collect()
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
        assert_eq!(s.setting("awake_tabs_max"), "4");
        assert!(s.set_setting("awake_tabs_max", "0").is_ok(), "no limit");
        assert!(s.set_setting("awake_tabs_max", "101").is_err());
        assert!(s.set_setting("awake_tabs_max", "two").is_err());
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
        let id = s.add_extension("/x/ext", "");
        assert_eq!(s.add_extension("/x/ext", ""), id, "same path, same row");
        s.set_extension_enabled(id, false);
        assert!(!s.extensions()[0].enabled);
        s.remove_extension(id);
        assert!(s.extensions().is_empty());
        s.save_session(&[(0, "https://a/".into(), "A".into(), true), (1, "https://b/".into(), "B".into(), false)]);
        assert_eq!(s.session()[1], (1, "https://b/".into(), "B".into(), false));
        assert!(s.session()[0].3, "pinned");
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
    fn suggestions_from_searches_and_history() {
        let s = Store::open(None).unwrap();
        assert!(s.suggestions("ru", 8).is_empty());
        s.add_search("learn rust");
        s.add_search("rust book");
        s.add_search("python");
        s.add_search("  rust book ");
        s.add_visit("https://www.rust-lang.org/", "Rust Programming Language");
        s.add_visit("https://www.rust-lang.org/", "Rust Programming Language");
        s.add_visit("https://crates.io/", "crates.io: Rust Package Registry");
        s.add_visit("https://duckduckgo.com/?q=rust+book", "rust book at DuckDuckGo");
        assert_eq!(s.search_count(), 3, "a repeated search is one row");
        let got = s.suggestions("rust", 8);
        assert_eq!(
            got,
            [
                Suggestion::Search("rust book".into()),
                Suggestion::Search("learn rust".into()),
                Suggestion::Page("https://www.rust-lang.org/".into(), "Rust Programming Language".into()),
                Suggestion::Page("https://crates.io/".into(), "crates.io: Rust Package Registry".into()),
            ],
            "searches starting with it first, then pages, most visited first"
        );
        assert_eq!(s.suggestions("100%", 8), [], "LIKE wildcards are literal");
        assert_eq!(s.suggestions("rust", 2).len(), 2);
        // Untouched bar, ↑/↓: the latest searches only, no pages.
        for (text, at) in [("learn rust", 1), ("python", 3), ("rust book", 2)] {
            s.db.execute("UPDATE searches SET at = ?2 WHERE text = ?1", params![text, at]).unwrap();
        }
        assert_eq!(s.recent_searches(2), [Suggestion::Search("python".into()), Suggestion::Search("rust book".into())]);
        s.clear_history();
        assert!(s.recent_searches(8).is_empty());
        assert_eq!(s.search_count(), 0, "clearing history clears searches");
    }

    #[test]
    fn keywords_are_validated() {
        let s = Store::open(None).unwrap();
        s.set_keyword("!W", "https://wiki.example/?q=%s").unwrap();
        s.set_keyword("w", "https://wiki2.example/%s").unwrap();
        assert_eq!(s.keywords(), [("w".to_string(), "https://wiki2.example/%s".to_string())], "lowercase, replaced");
        assert!(s.set_keyword("two words", "https://a.example/%s").is_err());
        assert!(s.set_keyword("x", "javascript:alert('%s')").is_err());
        assert!(s.set_keyword("x", "https://a.example/").is_err(), "needs %s");
        s.delete_keyword("w");
        assert!(s.keywords().is_empty());
    }

    #[test]
    fn autofill_entries_keep_only_their_fields() {
        let s = Store::open(None).unwrap();
        let f = |pairs: &[(&str, &str)]| pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect::<Vec<_>>();
        let a =
            s.save_autofill(0, "address", &f(&[("name", "Ada Lovelace"), ("street", "1 Main St\nFlat 2")])).unwrap();
        let c = s.save_autofill(0, "card", &f(&[("name", "Ada"), ("last4", "4242")])).unwrap();
        assert_eq!(s.autofill(Some("address"))[0].get("street"), "1 Main St Flat 2", "one line");
        assert_eq!(s.autofill(None).len(), 2);
        assert!(s.save_autofill(0, "address", &f(&[("number", "4111")])).is_err(), "no card numbers in the database");
        assert!(s.save_autofill(0, "pet", &[]).is_err());
        s.save_autofill(a, "address", &f(&[("name", "Ada King")])).unwrap();
        assert_eq!(s.autofill(Some("address"))[0].get("name"), "Ada King");
        assert!(s.save_autofill(c, "address", &[]).is_err(), "kinds don't change");
        s.delete_autofill(a);
        assert_eq!(s.autofill(None).len(), 1);
    }

    #[test]
    fn zoom_is_remembered_per_site() {
        let s = Store::open(None).unwrap();
        assert_eq!(s.zoom("a.example"), None);
        s.set_zoom("a.example", Some(1.25));
        assert_eq!(s.zoom("a.example"), Some(1.25));
        assert_eq!(s.zoom("b.example"), None);
        s.set_zoom("a.example", None);
        assert_eq!(s.db.query_row("SELECT COUNT(*) FROM zoom", [], |r| r.get::<_, i64>(0)).unwrap(), 0, "forgotten");
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
