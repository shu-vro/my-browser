//! Import bookmarks, history and passwords from other browsers on this Mac.
//!
//! - Chromium browsers (Chrome, Brave, Edge, Vivaldi, Arc, Chromium), their
//!   default profile: `Bookmarks` (JSON), `History` and `Login Data`
//!   (SQLite, copied first since the browser may have them open). Passwords
//!   are AES-encrypted with a key kept in the keychain ("<Browser> Safe
//!   Storage"); macOS asks the user before handing it over.
//! - Safari: `Bookmarks.plist` and `History.db`. macOS protects them; without
//!   Full Disk Access the user exports from Safari and imports the file.
//!   Safari's passwords live in the Passwords app: its CSV export goes through
//!   Settings → Passwords.
//! - A bookmarks file (`.html`, the format every browser exports).
//!
//! Everything runs only when the user asks, and reads only.

use std::path::{Path, PathBuf};

use objc2_foundation::{NSArray, NSData, NSDictionary, NSJSONReadingOptions, NSJSONSerialization, NSString};

/// (id, name, profile folder under ~/Library, keychain service for passwords).
const CHROMIUM: [(&str, &str, &str, &str); 6] = [
    ("chrome", "Google Chrome", "Application Support/Google/Chrome/Default", "Chrome Safe Storage"),
    ("brave", "Brave", "Application Support/BraveSoftware/Brave-Browser/Default", "Brave Safe Storage"),
    ("edge", "Microsoft Edge", "Application Support/Microsoft Edge/Default", "Microsoft Edge Safe Storage"),
    ("vivaldi", "Vivaldi", "Application Support/Vivaldi/Default", "Vivaldi Safe Storage"),
    ("arc", "Arc", "Application Support/Arc/User Data/Default", "Arc Safe Storage"),
    ("chromium", "Chromium", "Application Support/Chromium/Default", "Chromium Safe Storage"),
];

/// A bookmark to import: (folder path, title, url).
pub type Bookmark = (String, String, String);
/// A visit to import: (url, title, unix seconds).
pub type Visit = (String, String, i64);

/// Browsers found on this Mac: (id, name).
pub fn sources(home: &Path) -> Vec<(&'static str, &'static str)> {
    let lib = home.join("Library");
    let mut out: Vec<_> =
        CHROMIUM.iter().filter(|(_, _, dir, _)| lib.join(dir).is_dir()).map(|(id, name, ..)| (*id, *name)).collect();
    if lib.join("Safari").is_dir() {
        out.push(("safari", "Safari"));
    }
    out
}

fn chromium(id: &str) -> Option<&'static (&'static str, &'static str, &'static str, &'static str)> {
    CHROMIUM.iter().find(|(i, ..)| *i == id)
}

pub fn name_of(id: &str) -> &'static str {
    if id == "safari" { "Safari" } else { chromium(id).map_or("browser", |c| c.1) }
}

/// A read error in words: macOS keeps other apps' data private until the
/// user allows it.
fn explain(e: std::io::Error, what: &str) -> String {
    if e.kind() == std::io::ErrorKind::PermissionDenied || e.raw_os_error() == Some(1) {
        format!(
            "macOS didn't let my-browser read {what}. Allow it in System Settings → Privacy & Security → Full Disk \
             Access (or answer Allow when asked), then try again."
        )
    } else {
        format!("Couldn't read {what}: {e}")
    }
}

pub fn bookmarks(home: &Path, id: &str) -> Result<Vec<Bookmark>, String> {
    let lib = home.join("Library");
    let top = format!("Imported from {}", name_of(id));
    if id == "safari" {
        let path = lib.join("Safari/Bookmarks.plist");
        std::fs::metadata(&path)
            .and_then(|_| std::fs::File::open(&path))
            .map_err(|e| explain(e, "Safari's bookmarks"))?;
        return safari_bookmarks(&path, &top).ok_or_else(|| "Safari's bookmarks file isn't readable.".into());
    }
    let (_, name, dir, _) = chromium(id).ok_or("unknown browser")?;
    let data =
        std::fs::read(lib.join(dir).join("Bookmarks")).map_err(|e| explain(e, &format!("{name}'s bookmarks")))?;
    chromium_bookmarks(&data, &top).ok_or_else(|| format!("{name}'s bookmarks file isn't readable."))
}

pub fn history(home: &Path, id: &str) -> Result<Vec<Visit>, String> {
    let lib = home.join("Library");
    let (file, query, name) = if id == "safari" {
        // Seconds since 2001 (Core Data's epoch).
        let q = "SELECT i.url, COALESCE(v.title, ''), CAST(v.visit_time AS INTEGER) + 978307200 FROM history_visits v
                 JOIN history_items i ON i.id = v.history_item ORDER BY v.visit_time DESC LIMIT 20000";
        (lib.join("Safari/History.db"), q, "Safari")
    } else {
        let (_, name, dir, _) = chromium(id).ok_or("unknown browser")?;
        // Microseconds since 1601.
        let q = "SELECT u.url, u.title, v.visit_time / 1000000 - 11644473600 FROM visits v
                 JOIN urls u ON u.id = v.url ORDER BY v.visit_time DESC LIMIT 20000";
        (lib.join(dir).join("History"), q, *name)
    };
    let copy = copy_database(&file).map_err(|e| explain(e, &format!("{name}'s history")))?;
    let rows = (|| -> rusqlite::Result<Vec<Visit>> {
        let db = rusqlite::Connection::open_with_flags(copy.join("db"), rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        let mut stmt = db.prepare(query)?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.filter_map(Result::ok);
        Ok(rows.filter(|(u, ..): &Visit| u.starts_with("http://") || u.starts_with("https://")).collect())
    })();
    let _ = std::fs::remove_dir_all(&copy);
    rows.map_err(|e| format!("{name}'s history isn't readable: {e}"))
}

/// Passwords from a Chromium browser: (url, username, password).
pub fn passwords(home: &Path, id: &str) -> Result<Vec<(String, String, String)>, String> {
    let (_, name, dir, service) =
        chromium(id).ok_or("Import Safari's passwords from the Passwords app's CSV export.")?;
    let copy = copy_database(&home.join("Library").join(dir).join("Login Data"))
        .map_err(|e| explain(e, &format!("{name}'s passwords")))?;
    let rows = (|| -> rusqlite::Result<Vec<(String, String, Vec<u8>)>> {
        let db = rusqlite::Connection::open_with_flags(copy.join("db"), rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        let mut stmt =
            db.prepare("SELECT origin_url, username_value, password_value FROM logins WHERE blacklisted_by_user = 0")?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.filter_map(Result::ok).collect();
        Ok(rows)
    })();
    let _ = std::fs::remove_dir_all(&copy);
    let rows = rows.map_err(|e| format!("{name}'s passwords aren't readable: {e}"))?;
    if rows.is_empty() {
        return Ok(Vec::new());
    }
    // macOS asks the user here whether we may use the browser's key.
    let secret = crate::passwords::generic_password(service)
        .ok_or_else(|| format!("{name}'s password key wasn't given (the keychain asked, and it was denied)."))?;
    let key = chromium_key(&secret);
    Ok(rows.into_iter().filter_map(|(url, user, blob)| Some((url, user, chromium_decrypt(&key, &blob)?))).collect())
}

/// Copy a SQLite database (and its write-ahead log) somewhere private: the
/// browser may have it open and locked. Returns the folder; the copy is `db`.
fn copy_database(file: &Path) -> std::io::Result<PathBuf> {
    let dir = std::env::temp_dir().join(format!("my-browser-import-{}-{}", std::process::id(), crate::crash::now_ms()));
    std::fs::create_dir_all(&dir)?;
    let copied = std::fs::copy(file, dir.join("db")).map(|_| ());
    if let Err(e) = copied {
        let _ = std::fs::remove_dir_all(&dir);
        return Err(e);
    }
    let mut wal = file.as_os_str().to_owned();
    wal.push("-wal");
    let _ = std::fs::copy(PathBuf::from(wal), dir.join("db-wal"));
    Ok(dir)
}

/// Chromium's `Bookmarks` JSON: roots (bookmark_bar, other, synced) of
/// nodes, each a "url" or a "folder" with children.
pub fn chromium_bookmarks(data: &[u8], top: &str) -> Option<Vec<Bookmark>> {
    let json =
        NSJSONSerialization::JSONObjectWithData_options_error(&NSData::with_bytes(data), NSJSONReadingOptions::empty())
            .ok()?;
    let roots = json.downcast_ref::<NSDictionary>()?.objectForKey(&NSString::from_str("roots"))?;
    let roots = roots.downcast_ref::<NSDictionary>()?;
    let mut out = Vec::new();
    for (key, label) in
        [("bookmark_bar", "Bookmarks Bar"), ("other", "Other Bookmarks"), ("synced", "Mobile Bookmarks")]
    {
        if let Some(node) = roots.objectForKey(&NSString::from_str(key)).and_then(|n| n.downcast::<NSDictionary>().ok())
        {
            walk_chromium(&node, &format!("{top}/{label}"), &mut out);
        }
    }
    Some(out)
}

fn text(d: &NSDictionary, key: &str) -> String {
    d.objectForKey(&NSString::from_str(key))
        .and_then(|v| v.downcast::<NSString>().ok())
        .map_or(String::new(), |s| s.to_string())
}

fn children(d: &NSDictionary, key: &str) -> Vec<objc2::rc::Retained<NSDictionary>> {
    d.objectForKey(&NSString::from_str(key))
        .and_then(|c| c.downcast::<NSArray>().ok())
        .map(|a| a.iter().filter_map(|c| c.downcast::<NSDictionary>().ok()).collect())
        .unwrap_or_default()
}

fn walk_chromium(node: &NSDictionary, folder: &str, out: &mut Vec<Bookmark>) {
    for child in children(node, "children") {
        match text(&child, "type").as_str() {
            "url" => out.push((folder.to_string(), text(&child, "name"), text(&child, "url"))),
            "folder" => walk_chromium(&child, &format!("{folder}/{}", clean_folder(&text(&child, "name"))), out),
            _ => {}
        }
    }
}

/// Safari's `Bookmarks.plist`: lists (`WebBookmarkTypeList`, with `Title`
/// and `Children`) and leaves (`WebBookmarkTypeLeaf`, with `URLString`).
fn safari_bookmarks(path: &Path, top: &str) -> Option<Vec<Bookmark>> {
    let data = NSData::with_bytes(&std::fs::read(path).ok()?);
    let root = unsafe {
        objc2_foundation::NSPropertyListSerialization::propertyListWithData_options_format_error(
            &data,
            objc2_foundation::NSPropertyListReadOptions::empty(),
            std::ptr::null_mut(),
        )
    }
    .ok()?;
    let mut out = Vec::new();
    walk_safari(root.downcast_ref::<NSDictionary>()?, top, &mut out);
    Some(out)
}

fn walk_safari(node: &NSDictionary, folder: &str, out: &mut Vec<Bookmark>) {
    for child in children(node, "Children") {
        match text(&child, "WebBookmarkType").as_str() {
            "WebBookmarkTypeLeaf" => {
                let title = child
                    .objectForKey(&NSString::from_str("URIDictionary"))
                    .and_then(|d| d.downcast::<NSDictionary>().ok())
                    .map(|d| text(&d, "title"))
                    .unwrap_or_default();
                out.push((folder.to_string(), title, text(&child, "URLString")));
            }
            "WebBookmarkTypeList" => {
                let name = match text(&child, "Title").as_str() {
                    "BookmarksBar" => "Favorites".to_string(),
                    "BookmarksMenu" => "Bookmarks Menu".to_string(),
                    "com.apple.ReadingList" => "Reading List".to_string(),
                    t => clean_folder(t),
                };
                walk_safari(&child, &format!("{folder}/{name}"), out);
            }
            _ => {}
        }
    }
}

/// A folder name can't hold `/` (it separates folders here) or be empty.
fn clean_folder(name: &str) -> String {
    let n = name.replace('/', "-").trim().to_string();
    if n.is_empty() { "Untitled".into() } else { n }
}

/// A bookmarks file in the Netscape format every browser exports: `<H3>`
/// names a folder whose contents follow in a `<DL>`, `<A HREF>` is a bookmark.
pub fn html_bookmarks(html: &str, top: &str) -> Vec<Bookmark> {
    let mut out = Vec::new();
    let mut path: Vec<String> = vec![top.to_string()];
    let mut pending: Option<String> = None;
    let lower = html.to_ascii_lowercase();
    let mut i = 0;
    while let Some(start) = lower[i..].find('<').map(|s| s + i) {
        let end = lower[start..].find('>').map_or(lower.len(), |e| start + e + 1);
        let tag = &lower[start..end];
        if tag.starts_with("<h3") {
            let close = lower[end..].find("</h3").map_or(lower.len(), |c| end + c);
            pending = Some(clean_folder(&unescape(&strip_tags(&html[end..close]))));
            i = close;
        } else if tag.starts_with("<dl") {
            // A list after a folder name is that folder; the file's outer
            // list isn't a folder at all.
            path.push(pending.take().unwrap_or_default());
            i = end;
        } else if tag.starts_with("</dl") {
            if path.len() > 1 {
                path.pop();
            }
            i = end;
        } else if tag.starts_with("<a ") || tag.starts_with("<a\t") {
            let close = lower[end..].find("</a").map_or(lower.len(), |c| end + c);
            if let Some(href) = attribute(&html[start..end], "href") {
                let folder: Vec<&str> = path.iter().map(String::as_str).filter(|p| !p.is_empty()).collect();
                out.push((
                    folder.join("/"),
                    unescape(&strip_tags(&html[end..close])).trim().to_string(),
                    unescape(&href),
                ));
            }
            i = close;
        } else {
            i = end;
        }
    }
    out.retain(|(_, _, url)| url.starts_with("http://") || url.starts_with("https://"));
    out
}

fn attribute(tag: &str, name: &str) -> Option<String> {
    let lower = tag.to_ascii_lowercase();
    let at = lower.find(&format!("{name}="))? + name.len() + 1;
    let rest = &tag[at..];
    let (quote, rest) = match rest.chars().next()? {
        q @ ('"' | '\'') => (q, &rest[1..]),
        _ => (' ', rest),
    };
    Some(rest.split([quote, '>']).next()?.to_string())
}

fn strip_tags(s: &str) -> String {
    let mut out = String::new();
    let mut inside = false;
    for c in s.chars() {
        match c {
            '<' => inside = true,
            '>' => inside = false,
            c if !inside => out.push(c),
            _ => {}
        }
    }
    out
}

fn unescape(s: &str) -> String {
    s.replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&#39;", "'").replace("&amp;", "&")
}

#[link(name = "System", kind = "dylib")]
unsafe extern "C" {
    fn CCKeyDerivationPBKDF(
        algorithm: u32,
        password: *const u8,
        password_len: usize,
        salt: *const u8,
        salt_len: usize,
        prf: u32,
        rounds: u32,
        derived: *mut u8,
        derived_len: usize,
    ) -> i32;
    fn CCCrypt(
        op: u32,
        alg: u32,
        options: u32,
        key: *const u8,
        key_len: usize,
        iv: *const u8,
        data_in: *const u8,
        data_in_len: usize,
        data_out: *mut u8,
        data_out_available: usize,
        data_out_moved: *mut usize,
    ) -> i32;
}

/// Chromium on macOS: AES-128 key = PBKDF2-HMAC-SHA1(keychain secret,
/// "saltysalt", 1003 rounds).
fn chromium_key(secret: &[u8]) -> [u8; 16] {
    let mut key = [0u8; 16];
    // kCCPBKDF2 = 2, kCCPRFHmacAlgSHA1 = 1.
    unsafe {
        CCKeyDerivationPBKDF(2, secret.as_ptr(), secret.len(), b"saltysalt".as_ptr(), 9, 1, 1003, key.as_mut_ptr(), 16)
    };
    key
}

/// `v10` + AES-128-CBC (IV of 16 spaces, PKCS#7 padding).
fn chromium_decrypt(key: &[u8; 16], blob: &[u8]) -> Option<String> {
    let data = blob.strip_prefix(b"v10")?;
    let out = crypt(1, key, data)?;
    String::from_utf8(out).ok()
}

/// kCCEncrypt = 0 / kCCDecrypt = 1, kCCAlgorithmAES = 0, kCCOptionPKCS7Padding = 1.
fn crypt(op: u32, key: &[u8; 16], data: &[u8]) -> Option<Vec<u8>> {
    let mut out = vec![0u8; data.len() + 16];
    let mut moved = 0usize;
    let status = unsafe {
        CCCrypt(
            op,
            0,
            1,
            key.as_ptr(),
            16,
            [b' '; 16].as_ptr(),
            data.as_ptr(),
            data.len(),
            out.as_mut_ptr(),
            out.len(),
            &mut moved,
        )
    };
    (status == 0).then(|| {
        out.truncate(moved);
        out
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chromium_bookmarks_keep_their_folders() {
        let json = br#"{"roots":{"bookmark_bar":{"type":"folder","children":[
            {"type":"url","name":"Rust","url":"https://rust-lang.org/"},
            {"type":"folder","name":"Work/Docs","children":[{"type":"url","name":"Docs","url":"https://docs.example/"}]}]},
            "other":{"type":"folder","children":[]},"synced":{"type":"folder","children":[{"type":"url","name":"M","url":"https://m.example/"}]}}}"#;
        let b = chromium_bookmarks(json, "Imported from Brave").unwrap();
        assert_eq!(b[0], ("Imported from Brave/Bookmarks Bar".into(), "Rust".into(), "https://rust-lang.org/".into()));
        assert_eq!(b[1].0, "Imported from Brave/Bookmarks Bar/Work-Docs", "a / in a name doesn't make a folder");
        assert_eq!(b[2].0, "Imported from Brave/Mobile Bookmarks");
        assert!(chromium_bookmarks(b"not json", "x").is_none());
    }

    #[test]
    fn exported_bookmark_files_parse() {
        let html = r#"<!DOCTYPE NETSCAPE-Bookmark-file-1><META HTTP-EQUIV="Content-Type" CONTENT="text/html; charset=UTF-8">
<TITLE>Bookmarks</TITLE><H1>Bookmarks</H1>
<DL><p>
    <DT><H3 ADD_DATE="1">Favorites</H3>
    <DL><p>
        <DT><A HREF="https://a.example/?x=1&amp;y=2" ADD_DATE="1">A &amp; B</A>
        <DT><H3>Deep</H3>
        <DL><p><DT><A HREF='https://deep.example/'>Deep one</A></DL><p>
    </DL><p>
    <DT><A HREF="https://top.example/">Top</A>
    <DT><A HREF="javascript:alert(1)">Bookmarklet</A>
</DL><p>"#;
        let b = html_bookmarks(html, "Imported");
        assert_eq!(
            b,
            [
                ("Imported/Favorites".into(), "A & B".into(), "https://a.example/?x=1&y=2".into()),
                ("Imported/Favorites/Deep".into(), "Deep one".into(), "https://deep.example/".into()),
                ("Imported".into(), "Top".into(), "https://top.example/".into()),
            ],
            "{b:?}"
        );
    }

    #[test]
    fn chromium_passwords_decrypt() {
        let key = chromium_key(b"peanuts");
        let mut blob = b"v10".to_vec();
        blob.extend(crypt(0, &key, b"hunter2").unwrap());
        assert_eq!(chromium_decrypt(&key, &blob).as_deref(), Some("hunter2"));
        assert_eq!(chromium_decrypt(&chromium_key(b"wrong"), &blob), None);
        assert_eq!(chromium_decrypt(&key, b"v11xxxx"), None, "unknown format");
    }

    #[test]
    fn browsers_are_found_by_their_folders() {
        let home = std::env::temp_dir().join(format!("mybrowser-sources-{}", std::process::id()));
        std::fs::create_dir_all(home.join("Library/Application Support/BraveSoftware/Brave-Browser/Default")).unwrap();
        std::fs::create_dir_all(home.join("Library/Safari")).unwrap();
        assert_eq!(sources(&home), [("brave", "Brave"), ("safari", "Safari")]);
        std::fs::remove_dir_all(&home).unwrap();
    }
}
