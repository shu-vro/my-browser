//! The browser's own pages (mybrowser://settings: settings, history,
//! downloads, extensions) and the bridge they use to talk to the browser.
//!
//! The bridge is a script message handler, and any page can post to it, so
//! every message is checked: only the top-level frame of a mybrowser: page
//! is allowed.

use std::path::{Path, PathBuf};

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{AnyThread, MainThreadOnly, Message};
use objc2_app_kit::{NSModalResponseOK, NSOpenPanel, NSWorkspace};
use objc2_foundation::{NSArray, NSData, NSDate, NSError, NSString, NSURLResponse, NSURL};
use objc2_web_kit::{WKScriptMessage, WKURLSchemeTask, WKWebsiteDataStore};

use crate::store::{json_str, SETTINGS};
use crate::tab::INTERNAL;
use crate::App;

static SETTINGS_HTML: &str = include_str!("../resources/settings.html");

pub type Reply = Box<dyn FnOnce(Result<String, String>)>;

/// Answer a mybrowser: request.
pub fn serve(task: &ProtocolObject<dyn WKURLSchemeTask>) {
    let request = unsafe { task.request() };
    let Some(url) = request.URL() else { return };
    let host = url.host().map(|h| h.to_string()).unwrap_or_default();
    if host != "settings" {
        let err = NSError::new(-1100, &NSString::from_str("NSURLErrorDomain")); // file not found
        unsafe { task.didFailWithError(&err) };
        return;
    }
    let body = SETTINGS_HTML.as_bytes();
    let response = NSURLResponse::initWithURL_MIMEType_expectedContentLength_textEncodingName(
        NSURLResponse::alloc(),
        &url,
        Some(&NSString::from_str("text/html")),
        body.len() as isize,
        Some(&NSString::from_str("utf-8")),
    );
    unsafe {
        task.didReceiveResponse(&response);
        task.didReceiveData(&NSData::with_bytes(body));
        task.didFinish();
    }
}

/// Only the top-level frame of the browser's own pages may use the bridge.
pub fn is_trusted(message: &WKScriptMessage) -> bool {
    let frame = unsafe { message.frameInfo() };
    let origin = unsafe { frame.securityOrigin() };
    let page_scheme = unsafe { message.webView() }
        .and_then(|w| unsafe { w.URL() })
        .and_then(|u| u.scheme())
        .map(|s| s.to_string());
    let main_frame = unsafe { frame.isMainFrame() };
    main_frame && unsafe { origin.protocol() }.to_string() == INTERNAL && page_scheme.as_deref() == Some(INTERNAL)
}

/// The message body must be an array of strings: [op, ...args].
pub fn message_args(message: &WKScriptMessage) -> Option<Vec<String>> {
    let body = unsafe { message.body() };
    let array = body.downcast_ref::<NSArray>()?;
    array.iter().map(|item| item.downcast_ref::<NSString>().map(|s| s.to_string())).collect()
}

fn id_arg(args: &[String], i: usize) -> Result<i64, String> {
    args.get(i).and_then(|s| s.parse().ok()).ok_or_else(|| "missing id".to_string())
}

fn ok() -> Result<String, String> {
    Ok("true".into())
}

/// Run one bridge operation. Replies with JSON.
pub fn handle(app: &App, args: Vec<String>, reply: Reply) {
    let op = args.first().map(String::as_str).unwrap_or("");
    let store = app.store();
    let result = match op {
        "settings.get" => {
            let mut fields: Vec<String> =
                SETTINGS.iter().map(|(k, _)| format!("{}:{}", json_str(k), json_str(&store.setting(k)))).collect();
            fields.push(format!("\"downloads_default\":{}", json_str(&app.default_downloads_dir())));
            let profile = app.profile_dir().map_or("temporary (nothing is saved)".into(), |p| p.display().to_string());
            fields.push(format!("\"profile\":{}", json_str(&profile)));
            Ok(format!("{{{}}}", fields.join(",")))
        }
        "settings.set" => {
            let (key, value) = (args.get(1).map_or("", |s| s), args.get(2).map_or("", |s| s));
            store.set_setting(key, value).map(|()| {
                match key {
                    "block_trackers" => app.apply_trackers(),
                    "safe_browsing" => app.apply_safe_browsing(),
                    _ => {}
                }
                "true".to_string()
            })
        }
        "history.list" => {
            let rows = store.history(args.get(1).map_or("", |s| s), 500);
            let items: Vec<String> = rows
                .iter()
                .map(|v| format!("{{\"id\":{},\"url\":{},\"title\":{},\"at\":{}}}", v.id, json_str(&v.url), json_str(&v.title), v.at))
                .collect();
            Ok(format!("[{}]", items.join(",")))
        }
        "history.delete" => id_arg(&args, 1).map(|id| {
            store.delete_visit(id);
            "true".into()
        }),
        "history.clear" => {
            store.clear_history();
            ok()
        }
        "downloads.list" => {
            let progress = app.download_progress();
            let items: Vec<String> = store
                .downloads()
                .iter()
                .map(|d| {
                    let (done, total) =
                        progress.iter().find(|(id, ..)| *id == d.id).map_or((d.bytes, d.bytes), |(_, c, t)| (*c, *t));
                    format!(
                        "{{\"id\":{},\"url\":{},\"path\":{},\"status\":{},\"bytes\":{done},\"total\":{total},\"at\":{},\"exists\":{}}}",
                        d.id,
                        json_str(&d.url),
                        json_str(&d.path),
                        json_str(&d.status),
                        d.at,
                        Path::new(&d.path).exists()
                    )
                })
                .collect();
            Ok(format!("[{}]", items.join(",")))
        }
        "downloads.open" | "downloads.reveal" => id_arg(&args, 1).and_then(|id| {
            let d = store.download(id).ok_or("no such download")?;
            let url = NSURL::fileURLWithPath(&NSString::from_str(&d.path));
            let ws = NSWorkspace::sharedWorkspace();
            if op == "downloads.open" {
                ws.openURL(&url);
            } else {
                ws.activateFileViewerSelectingURLs(&NSArray::from_retained_slice(&[url]));
            }
            ok()
        }),
        "downloads.cancel" => id_arg(&args, 1).map(|id| {
            app.cancel_download(id);
            "true".into()
        }),
        "downloads.remove" => id_arg(&args, 1).map(|id| {
            store.remove_download(id);
            "true".into()
        }),
        "ext.list" => Ok(extensions_json(app)),
        "ext.install" => match args.get(1) {
            Some(path) => return install_extension(app, PathBuf::from(path), reply),
            None => return pick_and_install_extension(app, reply),
        },
        "ext.toggle" => {
            let Some(ext) = args.get(1).and_then(|s| s.parse().ok()).and_then(|id: i64| store.extensions().into_iter().find(|e| e.id == id)) else {
                return reply(Err("no such extension".into()));
            };
            let on = args.get(2).map(String::as_str) == Some("1");
            store.set_extension_enabled(ext.id, on);
            if on {
                return app.load_extension(ext.id, &ext.path, Box::new(move |r| reply(r.map(|_| "true".into()))));
            }
            app.unload_extension(ext.id);
            ok()
        }
        "ext.remove" => id_arg(&args, 1).map(|id| {
            app.unload_extension(id);
            if let Some(e) = store.extensions().into_iter().find(|e| e.id == id) {
                // Only delete copies the browser made, never the user's own folder.
                if let Some(dir) = extensions_dir(app) {
                    if Path::new(&e.path).starts_with(&dir) {
                        let _ = std::fs::remove_dir_all(&e.path);
                    }
                }
            }
            store.remove_extension(id);
            "true".into()
        }),
        "data.clear" => return clear_site_data(app.data_store(), reply),
        "open" => match args.get(1) {
            Some(url) if url.starts_with("http://") || url.starts_with("https://") => {
                app.open_tab(url, false);
                ok()
            }
            _ => Err("only http(s) URLs".into()),
        },
        _ => Err(format!("unknown op {op}")),
    };
    reply(result);
}

fn extensions_dir(app: &App) -> Option<PathBuf> {
    app.profile_dir().map(|p| p.join("extensions"))
}

fn extensions_json(app: &App) -> String {
    let items: Vec<String> = app
        .store()
        .extensions()
        .iter()
        .map(|e| {
            let ctx = app.extension_context(e.id);
            let ext = ctx.as_ref().map(|c| unsafe { c.webExtension() });
            let text = |f: fn(&objc2_web_kit::WKWebExtension) -> Option<Retained<NSString>>| {
                ext.as_deref().and_then(f).map(|s| s.to_string()).unwrap_or_default()
            };
            let name = text(|x| unsafe { x.displayName() });
            let version = text(|x| unsafe { x.displayVersion() });
            let description = text(|x| unsafe { x.displayDescription() });
            let mut perms: Vec<String> = ext.as_deref().map_or_else(Vec::new, |x| unsafe {
                let mut p: Vec<String> = x.requestedPermissions().iter().map(|p| p.to_string()).collect();
                p.extend(x.allRequestedMatchPatterns().iter().map(|m| m.string().to_string()));
                p
            });
            perms.sort();
            let perms: Vec<String> = perms.iter().map(|p| json_str(p)).collect();
            // What WebKit didn't like about the manifest, or about loading it.
            let mut errors: Vec<String> = Vec::new();
            if let Some(c) = &ctx {
                errors.extend(unsafe { c.errors() }.iter().map(|e| json_str(&e.localizedDescription().to_string())));
            }
            format!(
                "{{\"id\":{},\"path\":{},\"enabled\":{},\"loaded\":{},\"name\":{},\"version\":{},\"description\":{},\"permissions\":[{}],\"errors\":[{}]}}",
                e.id,
                json_str(&e.path),
                e.enabled,
                ctx.is_some(),
                json_str(&name),
                json_str(&version),
                json_str(&description),
                perms.join(","),
                errors.join(",")
            )
        })
        .collect();
    format!("[{}]", items.join(","))
}

fn pick_and_install_extension(app: &App, reply: Reply) {
    let panel = NSOpenPanel::openPanel(app.mtm());
    panel.setCanChooseFiles(false);
    panel.setCanChooseDirectories(true);
    panel.setAllowsMultipleSelection(false);
    panel.setMessage(Some(&NSString::from_str("Choose an unpacked web extension (the folder with manifest.json)")));
    let reply = std::cell::RefCell::new(Some(reply));
    let this = app.retain();
    let p = panel.clone();
    let handler = RcBlock::new(move |response: isize| {
        let Some(reply) = reply.take() else { return };
        let path = (response == NSModalResponseOK).then(|| p.URL()).flatten().and_then(|u| u.path());
        match path {
            Some(path) => install_extension(&this, PathBuf::from(path.to_string()), reply),
            None => reply(Err("cancelled".into())),
        }
    });
    panel.beginWithCompletionHandler(&handler);
}

/// Copy the extension into the profile (so it survives the source folder
/// being moved), record it, load it.
fn install_extension(app: &App, source: PathBuf, reply: Reply) {
    if !source.join("manifest.json").is_file() {
        return reply(Err("That folder has no manifest.json.".into()));
    }
    let path = match extensions_dir(app) {
        Some(dir) => {
            let name = source.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "extension".into());
            let dest = dir.join(format!("{}-{name}", crate::store::now()));
            if let Err(e) = copy_dir(&source, &dest) {
                return reply(Err(format!("Couldn't copy the extension: {e}")));
            }
            dest
        }
        None => source,
    };
    let path = path.to_string_lossy().into_owned();
    let id = app.store().add_extension(&path);
    let store_path = path.clone();
    let this = app.retain();
    app.load_extension(
        id,
        &store_path,
        Box::new(move |result| match result {
            Ok(name) => reply(Ok(format!("{{\"id\":{id},\"name\":{}}}", json_str(&name)))),
            Err(e) => {
                // A broken extension isn't kept.
                this.store().remove_extension(id);
                if let Some(dir) = extensions_dir(&this) {
                    if Path::new(&path).starts_with(&dir) {
                        let _ = std::fs::remove_dir_all(&path);
                    }
                }
                reply(Err(e))
            }
        }),
    );
}

fn copy_dir(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        let kind = entry.file_type()?;
        if kind.is_dir() {
            copy_dir(&entry.path(), &target)?;
        } else if kind.is_file() {
            std::fs::copy(entry.path(), target)?;
        }
        // Symlinks are skipped: an extension must not point outside itself.
    }
    Ok(())
}

/// Delete all cookies, caches and site storage for this profile.
fn clear_site_data(store: &WKWebsiteDataStore, reply: Reply) {
    let types = unsafe { WKWebsiteDataStore::allWebsiteDataTypes(store.mtm()) };
    let reply = std::cell::RefCell::new(Some(reply));
    let done = RcBlock::new(move || {
        if let Some(r) = reply.take() {
            r(ok());
        }
    });
    unsafe { store.removeDataOfTypes_modifiedSince_completionHandler(&types, &NSDate::distantPast(), &done) };
}
