//! Sync by file (Settings → Sync): everything that makes the browser yours,
//! in one JSON file you save wherever you like, and import on another Mac
//! (or another profile). The browser never uploads it: moving the file is up
//! to you (AirDrop, iCloud Drive, a USB stick).
//!
//! In it: settings, bookmarks, history, search keywords, your filters,
//! addresses, site settings, zoom levels and changed keyboard shortcuts. Not
//! in it: passwords and cards (they stay in the keychain; a plain file would
//! expose them), cookies and site data, extensions, downloads.
//!
//! Importing adds to what's there: bookmarks and history that are already
//! there are skipped, and settings, keywords and site settings from the
//! file replace the current ones.

use std::path::Path;

use objc2::rc::Retained;
use objc2_foundation::{NSArray, NSData, NSDictionary, NSJSONReadingOptions, NSJSONSerialization, NSNumber, NSString};

use crate::App;
use crate::store::{SETTINGS, json_str, now};

const FORMAT: &str = "my-browser sync";

/// Settings that belong to this Mac, not to the person: left out.
const LOCAL_SETTINGS: [&str; 1] = ["downloads_dir"];

pub fn export(app: &App) -> String {
    let s = app.store();
    let obj = |fields: &[(&str, String)]| {
        let f: Vec<String> = fields.iter().map(|(k, v)| format!("{}:{v}", json_str(k))).collect();
        format!("{{{}}}", f.join(","))
    };
    let list = |items: Vec<String>| format!("[{}]", items.join(",\n"));
    let settings: Vec<(&str, String)> = SETTINGS
        .iter()
        .filter(|(k, _)| !LOCAL_SETTINGS.contains(k))
        .map(|(k, _)| (*k, json_str(&s.setting(k))))
        .collect();
    let bookmarks = s
        .bookmarks("")
        .iter()
        .map(|b| obj(&[("url", json_str(&b.url)), ("title", json_str(&b.title)), ("folder", json_str(&b.folder))]))
        .collect();
    let history = s
        .history("", i64::MAX)
        .iter()
        .map(|v| obj(&[("url", json_str(&v.url)), ("title", json_str(&v.title)), ("at", v.at.to_string())]))
        .collect();
    let keywords = s.keywords().iter().map(|(k, u)| obj(&[("keyword", json_str(k)), ("url", json_str(u))])).collect();
    let addresses = s
        .autofill(Some("address"))
        .iter()
        .map(|a| {
            let fields: Vec<(&str, String)> = a.fields.iter().map(|(k, v)| (k.as_str(), json_str(v))).collect();
            obj(&fields)
        })
        .collect();
    let permissions = s
        .permissions()
        .iter()
        .map(|(site, kind, value)| {
            obj(&[("site", json_str(site)), ("kind", json_str(kind)), ("value", json_str(value))])
        })
        .collect();
    let zoom =
        s.zooms().iter().map(|(site, level)| obj(&[("site", json_str(site)), ("level", level.to_string())])).collect();
    let shortcuts: Vec<(String, String)> = s.settings_with_prefix("shortcut:");
    let shortcuts: Vec<(&str, String)> =
        shortcuts.iter().map(|(k, v)| (k.strip_prefix("shortcut:").unwrap_or(k), json_str(v))).collect();
    format!(
        "{{\"format\":{},\"version\":1,\"exported\":{},\n\"settings\":{},\n\"keywords\":{},\n\"filters\":{},\n\"shortcuts\":{},\n\"permissions\":{},\n\"zoom\":{},\n\"addresses\":{},\n\"bookmarks\":{},\n\"history\":{}}}\n",
        json_str(FORMAT),
        now(),
        obj(&settings),
        list(keywords),
        json_str(&s.get("filters").unwrap_or_default()),
        obj(&shortcuts),
        list(permissions),
        list(zoom),
        list(addresses),
        list(bookmarks),
        list(history),
    )
}

/// Merge a sync file into this profile. `done` gets what was added, as JSON.
pub fn import(app: &App, path: &Path, done: crate::internal::Reply) {
    let file = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) => return done(Err(format!("Couldn't read the file: {e}"))),
    };
    let parsed = NSJSONSerialization::JSONObjectWithData_options_error(
        &NSData::with_bytes(&file),
        NSJSONReadingOptions::empty(),
    );
    let Some(root) = parsed.ok().and_then(|v| v.downcast::<NSDictionary>().ok()) else {
        return done(Err("That isn't a my-browser sync file.".into()));
    };
    if text(&root, "format") != FORMAT {
        return done(Err("That isn't a my-browser sync file.".into()));
    }
    let s = app.store();
    let mut settings = 0;
    if let Some(d) = dict(&root, "settings") {
        for (key, _) in SETTINGS.iter().filter(|(k, _)| !LOCAL_SETTINGS.contains(k)) {
            let value = text(&d, key);
            if d.objectForKey(&NSString::from_str(key)).is_some()
                && s.setting(key) != value
                && s.set_setting(key, &value).is_ok()
            {
                settings += 1;
            }
        }
    }
    let bookmarks: Vec<(String, String, String)> = items(&root, "bookmarks")
        .iter()
        .map(|b| (text(b, "folder"), text(b, "title"), text(b, "url")))
        .filter(|b| !b.2.is_empty())
        .collect();
    let bookmarks = s.import_bookmarks(&bookmarks);
    let visits: Vec<(String, String, i64)> = items(&root, "history")
        .iter()
        .map(|v| (text(v, "url"), text(v, "title"), number(v, "at") as i64))
        .filter(|v| !v.0.is_empty())
        .collect();
    let history = s.import_visits(&visits);
    let known = s.keywords();
    let keywords = items(&root, "keywords")
        .iter()
        .map(|k| (text(k, "keyword"), text(k, "url")))
        .filter(|k| !known.contains(k) && s.set_keyword(&k.0, &k.1).is_ok())
        .count();
    let permissions = items(&root, "permissions")
        .iter()
        .filter(|p| s.permission(&text(p, "site"), &text(p, "kind")).as_deref() != Some(&*text(p, "value")))
        .filter(|p| s.set_permission(&text(p, "site"), &text(p, "kind"), &text(p, "value")).is_ok())
        .count();
    for z in items(&root, "zoom") {
        let (site, level) = (text(&z, "site"), number(&z, "level"));
        if crate::store::site_of(&format!("https://{site}/")).as_deref() == Some(site.as_str())
            && (0.25..=5.0).contains(&level)
        {
            s.set_zoom(&site, Some(level));
        }
    }
    let have = s.autofill(Some("address"));
    let mut addresses = 0;
    for a in items(&root, "addresses") {
        let fields: Vec<(String, String)> = a
            .allKeys()
            .iter()
            .filter_map(|k| k.downcast::<NSString>().ok().map(|k| k.to_string()))
            .map(|k| (k.clone(), text(&a, &k)))
            .collect();
        let same =
            |e: &crate::store::Autofill| fields.iter().all(|(k, v)| e.get(k) == v) && e.fields.len() == fields.len();
        if !have.iter().any(same) && s.save_autofill(0, "address", &fields).is_ok() {
            addresses += 1;
        }
    }
    if let Some(d) = dict(&root, "shortcuts") {
        for id in d.allKeys().iter().filter_map(|k| k.downcast::<NSString>().ok().map(|k| k.to_string())) {
            let _ = crate::shortcuts::set(app, &id, &text(&d, &id));
        }
    }
    app.apply_theme();
    app.apply_devtools();
    app.apply_gestures();
    app.apply_safe_browsing();
    app.apply_trackers();
    app.refresh_toolbars();
    // The file's filters are added to yours; then the blockers are rebuilt
    // (they also hold Shields and local-network exceptions).
    let mine = s.get("filters").unwrap_or_default();
    let mut lines: Vec<&str> = mine.lines().collect();
    let theirs = text(&root, "filters");
    let new: Vec<&str> = theirs.lines().filter(|l| !l.trim().is_empty() && !lines.contains(l)).collect();
    let filters = new.iter().filter(|l| !l.trim_start().starts_with('!')).count();
    lines.extend(new);
    let summary = format!(
        "{{\"settings\":{settings},\"bookmarks\":{bookmarks},\"history\":{history},\"keywords\":{keywords},\"filters\":{filters},\"addresses\":{addresses},\"permissions\":{permissions}}}"
    );
    let text = if lines.is_empty() { String::new() } else { lines.join("\n") + "\n" };
    app.set_filters(&text, move |r| done(r.map(|_| summary)));
}

fn text(d: &NSDictionary, k: &str) -> String {
    d.objectForKey(&NSString::from_str(k))
        .and_then(|v| v.downcast::<NSString>().ok())
        .map(|s| s.to_string())
        .unwrap_or_default()
}

fn number(d: &NSDictionary, k: &str) -> f64 {
    d.objectForKey(&NSString::from_str(k)).and_then(|v| v.downcast::<NSNumber>().ok()).map_or(0.0, |n| n.doubleValue())
}

fn dict(d: &NSDictionary, k: &str) -> Option<Retained<NSDictionary>> {
    d.objectForKey(&NSString::from_str(k))?.downcast::<NSDictionary>().ok()
}

fn items(d: &NSDictionary, k: &str) -> Vec<Retained<NSDictionary>> {
    let Some(list) = d.objectForKey(&NSString::from_str(k)).and_then(|v| v.downcast::<NSArray>().ok()) else {
        return Vec::new();
    };
    list.iter().filter_map(|i| i.downcast::<NSDictionary>().ok()).collect()
}
