//! Local files: a path typed or pasted in the address bar opens here. A
//! folder shows as a file-manager page (`mybrowser://files/<path>`); a file
//! opens in the tab if WebKit can show it (pages, text, images, audio, video,
//! PDFs), otherwise in its default app (`Tab::open_local`). Folders list
//! names, sizes and dates read from disk when the page is asked for; the page
//! sorts and filters them itself. Nothing is sent anywhere.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use objc2::AnyThread;
use objc2_foundation::{NSData, NSString};

use crate::tab::escape_html;

/// Folder pages show at most this many entries (the rest are counted).
const MAX_ENTRIES: usize = 5000;

/// The internal address of a folder: `mybrowser://files/Users/me/My%20Docs/`.
pub fn folder_url(path: &Path) -> String {
    let mut s = String::from("mybrowser://files");
    for part in path.components().filter_map(|c| c.as_os_str().to_str()).filter(|c| *c != "/") {
        s.push('/');
        s.push_str(&encode_path_part(part));
    }
    s.push('/');
    s
}

/// A file's `file://` address.
pub fn file_url(path: &Path) -> String {
    let parts: Vec<String> =
        path.components().filter_map(|c| c.as_os_str().to_str()).filter(|c| *c != "/").map(encode_path_part).collect();
    format!("file:///{}", parts.join("/"))
}

fn encode_path_part(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'+' | b',' | b'@' | b'=' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// A path typed in the address bar (`/Users/me`, `~/Downloads`, `~`), as a
/// path; None if it isn't one.
pub fn typed_path(input: &str) -> Option<PathBuf> {
    let s = input.trim();
    let home = || std::env::var_os("HOME").map(PathBuf::from);
    if s == "~" {
        return home();
    }
    if let Some(rest) = s.strip_prefix("~/") {
        return Some(home()?.join(rest));
    }
    s.starts_with('/').then(|| PathBuf::from(s))
}

thread_local! {
    /// File-type icons as PNG data addresses, by extension ("/" for folders).
    static ICONS: RefCell<HashMap<String, String>> = RefCell::new(HashMap::new());
}

/// The Finder's icon for a kind of file, as a small PNG `data:` address.
fn icon(ext: &str, is_dir: bool) -> String {
    let key = if is_dir { "/".to_string() } else { ext.to_ascii_lowercase() };
    if let Some(i) = ICONS.with(|c| c.borrow().get(&key).cloned()) {
        return i;
    }
    let made = make_icon(&key).unwrap_or_default();
    ICONS.with(|c| c.borrow_mut().insert(key, made.clone()));
    made
}

fn make_icon(key: &str) -> Option<String> {
    let ws = objc2_app_kit::NSWorkspace::sharedWorkspace();
    #[allow(deprecated)]
    let image = if key == "/" {
        ws.iconForFile(&NSString::from_str("/System"))
    } else {
        ws.iconForFileType(&NSString::from_str(key))
    };
    let mut rect = objc2_foundation::NSRect::new(
        objc2_foundation::NSPoint::new(0.0, 0.0),
        objc2_foundation::NSSize::new(32.0, 32.0),
    );
    let cg = unsafe { image.CGImageForProposedRect_context_hints(&mut rect, None, None) }?;
    let rep = objc2_app_kit::NSBitmapImageRep::initWithCGImage(objc2_app_kit::NSBitmapImageRep::alloc(), &cg);
    let png = unsafe {
        rep.representationUsingType_properties(
            objc2_app_kit::NSBitmapImageFileType::PNG,
            &objc2_foundation::NSDictionary::new(),
        )
    }?;
    Some(format!("data:image/png;base64,{}", base64(&png)))
}

fn base64(data: &NSData) -> String {
    data.base64EncodedStringWithOptions(objc2_foundation::NSDataBase64EncodingOptions::empty()).to_string()
}

struct Entry {
    name: String,
    is_dir: bool,
    size: u64,
    modified: i64,
}

fn read(dir: &Path, hidden: bool) -> std::io::Result<(Vec<Entry>, usize)> {
    let mut out = Vec::new();
    let mut skipped = 0;
    for e in std::fs::read_dir(dir)?.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if !hidden && name.starts_with('.') {
            continue;
        }
        if out.len() >= MAX_ENTRIES {
            skipped += 1;
            continue;
        }
        // Symlinks show as what they point to.
        let meta = std::fs::metadata(e.path()).or_else(|_| e.metadata());
        let Ok(meta) = meta else { continue };
        let modified = meta
            .modified()
            .ok()
            .and_then(|m| m.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(0, |d| d.as_secs() as i64);
        out.push(Entry { name, is_dir: meta.is_dir(), size: meta.len(), modified });
    }
    out.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase())));
    Ok((out, skipped))
}

/// The folder page for `dir`. `hidden`: show dot-files too.
pub fn folder_page(dir: &Path, hidden: bool) -> String {
    let title = dir.file_name().map_or("/".into(), |n| n.to_string_lossy().into_owned());
    let mut crumbs = vec![format!("<a href=\"{}\">{}</a>", folder_url(Path::new("/")), "Macintosh HD")];
    let mut at = PathBuf::from("/");
    for part in dir.components().filter_map(|c| c.as_os_str().to_str()).filter(|c| *c != "/") {
        at.push(part);
        crumbs.push(format!("<a href=\"{}\">{}</a>", folder_url(&at), escape_html(part)));
    }
    let hidden_link = format!(
        "<a class=btn href=\"{}{}\">{}</a>",
        folder_url(dir),
        if hidden { "" } else { "?hidden=1" },
        if hidden { "Hide hidden files" } else { "Show hidden files" }
    );
    let up = dir.parent().map_or(String::new(), |p| format!("<a class=btn href=\"{}\">↑ Up</a>", folder_url(p)));
    let reveal = format!(
        "<a class=btn href=\"mybrowser://reveal{}\">Show in Finder</a>",
        &folder_url(dir)["mybrowser://files".len()..]
    );
    let body = match read(dir, hidden) {
        Err(e) => format!("<p class=empty>Can't open this folder: {}</p>", escape_html(&e.to_string())),
        Ok((entries, skipped)) if entries.is_empty() && skipped == 0 => {
            "<p class=empty>This folder is empty.</p>".into()
        }
        Ok((entries, skipped)) => {
            let rows: Vec<String> = entries
                .iter()
                .map(|e| {
                    let path = dir.join(&e.name);
                    let ext = Path::new(&e.name).extension().map_or(String::new(), |x| x.to_string_lossy().to_lowercase());
                    // WebKit won't follow a link from here to a file: the
                    // browser opens `mybrowser://open/…` itself (Tab::action_policy).
                    let href = if e.is_dir { folder_url(&path) } else { format!("mybrowser://open{}", folder_url(&path)["mybrowser://files".len()..].trim_end_matches('/')) };
                    let kind = if e.is_dir { "Folder".to_string() } else if ext.is_empty() { "File".into() } else { ext.to_uppercase() };
                    format!(
                        "<tr data-name=\"{}\" data-size={} data-date={} data-kind=\"{}\"><td><a href=\"{}\"><img src=\"{}\" alt=\"\">{}</a></td><td class=date></td><td class=size>{}</td><td>{}</td></tr>",
                        escape_html(&e.name.to_lowercase()),
                        if e.is_dir { -1 } else { e.size as i64 },
                        e.modified,
                        escape_html(&kind),
                        escape_html(&href),
                        icon(&ext, e.is_dir),
                        escape_html(&e.name),
                        if e.is_dir { "—".into() } else { size_text(e.size) },
                        escape_html(&kind),
                    )
                })
                .collect();
            let more =
                if skipped > 0 { format!("<p class=empty>And {skipped} more, not shown.</p>") } else { String::new() };
            format!(
                "<table><thead><tr><th data-key=name>Name</th><th data-key=date>Date Modified</th><th data-key=size>Size</th><th data-key=kind>Kind</th></tr></thead><tbody>{}</tbody></table>{more}",
                rows.join("")
            )
        }
    };
    format!(
        r#"<!doctype html><html><head><meta charset=utf-8>
<meta http-equiv=Content-Security-Policy content="default-src 'none'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; img-src data:">
<title>{t}</title><style>
:root{{color-scheme:light dark;--line:#e3e3e6;--muted:#6e6e73;--hover:#f2f2f4;--accent:#0a66d8}}
@media(prefers-color-scheme:dark){{:root{{--line:#3a3a3d;--muted:#a1a1a6;--hover:#2c2c2e;--accent:#4d9bff}}}}
body{{font:13px -apple-system,sans-serif;margin:0;padding:18px 26px}}
nav{{color:var(--muted);margin-bottom:10px;overflow-wrap:anywhere}} nav a{{color:inherit;text-decoration:none}} nav a:hover{{color:var(--accent)}}
nav a+a::before{{content:" › ";color:var(--muted)}}
h1{{font-size:22px;margin:4px 0 12px}}
.bar{{display:flex;gap:8px;align-items:center;margin-bottom:12px;flex-wrap:wrap}}
.btn{{border:1px solid var(--line);border-radius:6px;padding:4px 10px;color:inherit;text-decoration:none}} .btn:hover{{background:var(--hover)}}
input{{font:inherit;flex:1;min-width:160px;max-width:320px;padding:4px 8px;border:1px solid var(--line);border-radius:6px;background:transparent;color:inherit}}
table{{border-collapse:collapse;width:100%}} th{{text-align:left;font-weight:600;color:var(--muted);border-bottom:1px solid var(--line);padding:6px 8px;cursor:pointer;user-select:none;white-space:nowrap}}
th.on::after{{content:" ▲"}} th.on.down::after{{content:" ▼"}}
td{{padding:4px 8px;border-bottom:1px solid var(--line);white-space:nowrap}} tr:hover td{{background:var(--hover)}}
td:first-child{{width:100%;white-space:normal}} td a{{color:inherit;text-decoration:none;display:flex;align-items:center;gap:8px}}
td a:hover{{color:var(--accent)}} img{{width:18px;height:18px;flex-shrink:0}} .date,.size{{color:var(--muted);font-variant-numeric:tabular-nums}}
.empty{{color:var(--muted)}}
</style></head><body>
<nav>{crumbs}</nav><h1>{t}</h1>
<div class=bar>{up}<input id=q type=search placeholder="Filter" autofocus>{hidden_link}{reveal}</div>
{body}
<script>
"use strict";
const rows = () => [...document.querySelectorAll("tbody tr")];
for (const r of rows()) r.querySelector(".date").textContent = new Date(r.dataset.date * 1000).toLocaleString(undefined, {{dateStyle: "medium", timeStyle: "short"}});
document.getElementById("q").addEventListener("input", e => {{
  const q = e.target.value.toLowerCase();
  for (const r of rows()) r.hidden = !r.dataset.name.includes(q);
}});
let key = "name", down = false;
for (const th of document.querySelectorAll("th")) th.onclick = () => {{
  down = key === th.dataset.key ? !down : th.dataset.key === "date";
  key = th.dataset.key;
  document.querySelectorAll("th").forEach(h => h.className = h === th ? (down ? "on down" : "on") : "");
  const num = key === "size" || key === "date";
  const v = r => num ? +r.dataset[key] : r.dataset[key];
  const sorted = rows().sort((a, b) => (v(a) < v(b) ? -1 : v(a) > v(b) ? 1 : 0) * (down ? -1 : 1));
  document.querySelector("tbody").append(...sorted);
}};
document.querySelector("th[data-key=name]")?.classList.add("on");
</script></body></html>"#,
        t = escape_html(&title),
        crumbs = crumbs.join(""),
    )
}

fn size_text(b: u64) -> String {
    match b {
        b if b < 1000 => format!("{b} bytes"),
        b if b < 1_000_000 => format!("{:.0} KB", b as f64 / 1e3),
        b if b < 1_000_000_000 => format!("{:.1} MB", b as f64 / 1e6),
        b => format!("{:.2} GB", b as f64 / 1e9),
    }
}

/// Files that run something when opened: shown in Finder instead.
pub fn runs_code(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    let ext = path.extension().map_or(String::new(), |e| e.to_string_lossy().to_lowercase());
    let risky = [
        "app",
        "command",
        "sh",
        "tool",
        "pkg",
        "mpkg",
        "terminal",
        "workflow",
        "scpt",
        "applescript",
        "action",
        "jar",
        "dmg",
    ];
    risky.contains(&ext.as_str())
        || std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_become_addresses() {
        assert_eq!(folder_url(Path::new("/Users/me/My Docs")), "mybrowser://files/Users/me/My%20Docs/");
        assert_eq!(folder_url(Path::new("/")), "mybrowser://files/");
        assert_eq!(file_url(Path::new("/tmp/a b#1.png")), "file:///tmp/a%20b%231.png");
        assert_eq!(typed_path("/etc/hosts"), Some(PathBuf::from("/etc/hosts")));
        assert!(typed_path("~/Downloads").is_some_and(|p| p.ends_with("Downloads") && p.is_absolute()));
        assert_eq!(typed_path("example.com/x"), None);
        assert_eq!(typed_path("//cdn"), Some(PathBuf::from("//cdn")));
    }

    #[test]
    fn folder_pages_escape_names_and_list_entries() {
        let dir = std::env::temp_dir().join(format!("mybrowser-files-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("<b>x.txt"), b"12345").unwrap();
        std::fs::write(dir.join(".secret"), b"").unwrap();
        std::fs::write(dir.join("\" onmouseover=\"x"), b"").unwrap();
        let page = folder_page(&dir, false);
        assert!(page.contains("&lt;b&gt;x.txt") && !page.contains("<b>x.txt"), "escaped");
        assert!(
            page.contains("mybrowser://open/") && page.contains("%3Cb%3Ex.txt\""),
            "files open through the browser"
        );
        assert!(page.contains("/sub/\""), "folders link to their folder page");
        assert!(!page.contains(".secret") && folder_page(&dir, true).contains(".secret"));
        assert!(page.contains("5 bytes"));
        assert!(!page.contains("\" onmouseover"), "quotes in names can't end an attribute");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
