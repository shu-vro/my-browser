//! The browser's own pages (mybrowser://settings: settings, history,
//! downloads, extensions, site settings) and the bridge they use to talk to
//! the browser.
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
use objc2_foundation::{NSArray, NSData, NSDate, NSError, NSString, NSURL};
use objc2_web_kit::{WKScriptMessage, WKURLSchemeTask, WKWebsiteDataStore};

use crate::App;
use crate::store::{SETTINGS, json_str};
use crate::tab::INTERNAL;

static SETTINGS_HTML: &str = include_str!("../resources/settings.html");

pub type Reply = Box<dyn FnOnce(Result<String, String>)>;

/// Answer a mybrowser: request.
pub fn serve(task: &ProtocolObject<dyn WKURLSchemeTask>) {
    let request = unsafe { task.request() };
    let Some(url) = request.URL() else { return };
    let host = url.host().map(|h| h.to_string()).unwrap_or_default();
    let body = match host.as_str() {
        "settings" => SETTINGS_HTML.as_bytes(),
        _ => {
            let err = NSError::new(-1100, &NSString::from_str("NSURLErrorDomain")); // file not found
            unsafe { task.didFailWithError(&err) };
            return;
        }
    };
    // Never framed (clickjacking), never cached, never sniffed.
    let headers = [
        ("Content-Type", "text/html; charset=utf-8"),
        ("Content-Length", &body.len().to_string()),
        ("Content-Security-Policy", "frame-ancestors 'none'"),
        ("X-Frame-Options", "DENY"),
        ("X-Content-Type-Options", "nosniff"),
        ("Cache-Control", "no-store"),
    ];
    let keys: Vec<Retained<NSString>> = headers.iter().map(|(k, _)| NSString::from_str(k)).collect();
    let values: Vec<Retained<NSString>> = headers.iter().map(|(_, v)| NSString::from_str(v)).collect();
    let keys: Vec<&NSString> = keys.iter().map(|k| &**k).collect();
    let fields = objc2_foundation::NSDictionary::from_retained_objects(&keys, &values);
    let response = objc2_foundation::NSHTTPURLResponse::initWithURL_statusCode_HTTPVersion_headerFields(
        objc2_foundation::NSHTTPURLResponse::alloc(),
        &url,
        200,
        Some(&NSString::from_str("HTTP/1.1")),
        Some(&fields),
    );
    let Some(response) = response else { return };
    unsafe {
        task.didReceiveResponse(&response);
        task.didReceiveData(&NSData::with_bytes(body));
        task.didFinish();
    }
}

/// Only the top-level frame of the settings page may use the bridge.
pub fn is_trusted(message: &WKScriptMessage) -> bool {
    let frame = unsafe { message.frameInfo() };
    let origin = unsafe { frame.securityOrigin() };
    let Some(page) = unsafe { message.webView() }.and_then(|w| unsafe { w.URL() }) else { return false };
    let main_frame = unsafe { frame.isMainFrame() };
    main_frame
        && unsafe { origin.protocol() }.to_string() == INTERNAL
        && page.scheme().map(|s| s.to_string()).as_deref() == Some(INTERNAL)
        && page.host().map(|h| h.to_string()).as_deref() == Some("settings")
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
                    "block_trackers" | "block_local_network" => app.apply_trackers(),
                    "safe_browsing" => app.apply_safe_browsing(),
                    "theme" => app.apply_theme(),
                    _ => {}
                }
                "true".to_string()
            })
        }
        "history.list" => {
            let rows = store.history(args.get(1).map_or("", |s| s), 500);
            let items: Vec<String> = rows
                .iter()
                .map(|v| {
                    format!(
                        "{{\"id\":{},\"url\":{},\"title\":{},\"at\":{}}}",
                        v.id,
                        json_str(&v.url),
                        json_str(&v.title),
                        v.at
                    )
                })
                .collect();
            Ok(format!("[{}]", items.join(",")))
        }
        "history.delete" => id_arg(&args, 1).map(|id| {
            store.delete_visit(id);
            "true".into()
        }),
        "bookmarks.list" => {
            let items: Vec<String> = store
                .bookmarks(args.get(1).map_or("", |s| s))
                .iter()
                .map(|b| {
                    format!(
                        "{{\"id\":{},\"url\":{},\"title\":{},\"folder\":{}}}",
                        b.id,
                        json_str(&b.url),
                        json_str(&b.title),
                        json_str(&b.folder)
                    )
                })
                .collect();
            Ok(format!("[{}]", items.join(",")))
        }
        "bookmarks.update" => id_arg(&args, 1).and_then(|id| {
            store.update_bookmark(id, args.get(2).map_or("", |s| s), args.get(3).map_or("", |s| s))?;
            app.refresh_toolbars();
            ok()
        }),
        "bookmarks.delete" => id_arg(&args, 1).map(|id| {
            store.remove_bookmark(id);
            app.refresh_toolbars();
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
            let Some(ext) = args
                .get(1)
                .and_then(|s| s.parse().ok())
                .and_then(|id: i64| store.extensions().into_iter().find(|e| e.id == id))
            else {
                return reply(Err("no such extension".into()));
            };
            let on = args.get(2).map(String::as_str) == Some("1");
            store.set_extension_enabled(ext.id, on);
            if on {
                return app.register_extension(ext.id, &ext.path, Box::new(move |r| reply(r.map(|_| "true".into()))));
            }
            app.unload_extension(ext.id);
            ok()
        }
        "ext.access" => id_arg(&args, 1)
            .and_then(|id| app.set_extension_access(id, args.get(2).map_or("", |s| s)).map(|()| "true".to_string())),
        "ext.remove" => id_arg(&args, 1).map(|id| {
            app.unload_extension(id);
            if let Some(e) = store.extensions().into_iter().find(|e| e.id == id) {
                // Only delete copies the browser made, never the user's own folder.
                if let Some(dir) = extensions_dir(app)
                    && Path::new(&e.path).starts_with(&dir)
                {
                    let _ = std::fs::remove_dir_all(&e.path);
                }
            }
            store.remove_extension(id);
            "true".into()
        }),
        // Settings → Network: the gate's blocked and allowed requests.
        "net.list" => Ok(app.net_json()),
        "net.allow" => {
            id_arg(&args, 1).and_then(|ext| app.net_allow(ext, args.get(2).map_or("", |s| s)).map(|()| "true".into()))
        }
        "net.deny" => id_arg(&args, 1).map(|ext| {
            app.net_deny(ext, args.get(2).map_or("", |s| s));
            "true".into()
        }),
        "net.forget" => id_arg(&args, 1).map(|ext| {
            app.net_forget(ext, args.get(2).map_or("", |s| s));
            "true".into()
        }),
        "data.clear" => return clear_site_data(app.data_store(), reply),
        "storage.get" => Ok(storage_json(app)),
        "storage.clear_cache" => return clear_cache(app.data_store(), reply),
        "downloads.clear" => {
            store.clear_downloads();
            ok()
        }
        "legacy.clear" => {
            let (removed, bytes) = remove_paths(&leftovers(&home()));
            Ok(format!("{{\"removed\":{removed},\"bytes\":{bytes}}}"))
        }
        "perm.list" => {
            let items: Vec<String> = store
                .permissions()
                .iter()
                .map(|(site, kind, value)| {
                    format!("{{\"site\":{},\"kind\":{},\"value\":{}}}", json_str(site), json_str(kind), json_str(value))
                })
                .collect();
            Ok(format!("[{}]", items.join(",")))
        }
        "perm.set" | "perm.forget" => {
            let site = args.get(1).map_or("", |s| s);
            let result = if op == "perm.forget" {
                store.forget_site(site);
                Ok(())
            } else {
                store.set_permission(site, args.get(2).map_or("", |s| s), args.get(3).map_or("", |s| s))
            };
            match result {
                // Shields and local-network exceptions live in the compiled blockers.
                Ok(()) => {
                    return app.load_shields(move |_, t, l| {
                        reply(if t && l { ok() } else { Err("blocker failed to compile".into()) })
                    });
                }
                Err(e) => Err(e),
            }
        }
        "app.uninstall" => return uninstall(app, reply),
        "default.get" => Ok(default_browser_json()),
        "default.set" => return make_default_browser(reply),
        "open" => match args.get(1) {
            Some(url) if url.starts_with("http://") || url.starts_with("https://") => {
                app.open_tab_in(false, url, false);
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
            let ext = app.extension_meta(e.id);
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
            perms.dedup();
            let perms: Vec<String> = perms.iter().map(|p| json_str(p)).collect();
            // What WebKit didn't like about the manifest, or about running it.
            let mut errors: Vec<String> = Vec::new();
            if let Some(x) = &ext {
                errors.extend(unsafe { x.errors() }.iter().map(|e| json_str(&e.localizedDescription().to_string())));
            }
            if let Some(c) = &ctx {
                errors.extend(unsafe { c.errors() }.iter().map(|e| json_str(&e.localizedDescription().to_string())));
            }
            errors.dedup();
            format!(
                "{{\"id\":{},\"path\":{},\"enabled\":{},\"read\":{},\"running\":{},\"access\":{},\"name\":{},\"version\":{},\"description\":{},\"permissions\":[{}],\"errors\":[{}]}}",
                e.id,
                json_str(&e.path),
                e.enabled,
                ext.is_some(),
                ctx.is_some(),
                json_str(&e.access),
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

/// Show what the extension asks for and install only if the user agrees,
/// then copy it into the profile (so it survives the source folder being
/// moved), record it and give it a button. Benchmarks and tests skip the
/// question.
fn install_extension(app: &App, source: PathBuf, reply: Reply) {
    if !source.join("manifest.json").is_file() {
        return reply(Err("That folder has no manifest.json.".into()));
    }
    let this = app.retain();
    let src = source.clone();
    crate::extensions::read_extension(
        app.mtm(),
        &source.to_string_lossy(),
        Box::new(move |result| {
            let ext = match result {
                Ok(ext) => ext,
                Err(e) => return reply(Err(e)),
            };
            if !this.is_bench() {
                let alert = objc2_app_kit::NSAlert::new(this.mtm());
                alert.setMessageText(&NSString::from_str(&format!(
                    "Add “{}”?",
                    crate::extensions::extension_name(&ext)
                )));
                alert.setInformativeText(&NSString::from_str(&crate::extensions::describe_access(&ext)));
                alert.addButtonWithTitle(&NSString::from_str("Add Extension"));
                alert.addButtonWithTitle(&NSString::from_str("Cancel"));
                if alert.runModal() != 1000 {
                    return reply(Err("cancelled".into()));
                }
            }
            copy_and_register(&this, src, reply);
        }),
    );
}

fn copy_and_register(app: &App, source: PathBuf, reply: Reply) {
    let path = match extensions_dir(app) {
        Some(dir) => {
            let name =
                source.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "extension".into());
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
    let this = app.retain();
    app.register_extension(
        id,
        &path.clone(),
        Box::new(move |result| match result {
            Ok(name) => reply(Ok(format!("{{\"id\":{id},\"name\":{}}}", json_str(&name)))),
            Err(e) => {
                // A broken extension isn't kept.
                this.store().remove_extension(id);
                if let Some(dir) = extensions_dir(&this)
                    && Path::new(&path).starts_with(&dir)
                {
                    let _ = std::fs::remove_dir_all(&path);
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

fn home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default()
}

/// WebKit keeps an app's data under its bundle id, or the executable's name
/// when it isn't bundled.
fn webkit_app_id() -> String {
    objc2_foundation::NSBundle::mainBundle()
        .bundleIdentifier()
        .map(|s| s.to_string())
        .or_else(|| std::env::current_exe().ok()?.file_name().map(|n| n.to_string_lossy().into_owned()))
        .unwrap_or_else(|| "my-browser".into())
}

/// Data that older builds left behind: WebKit's shared default store (cookies,
/// site data, cache), from before every profile got its own store, and the
/// `dev.local.my-browser` test bundle. A fixed list: current profiles live in
/// `WebKit/<app>/WebsiteDataStore/<id>` and are never in it.
pub fn leftovers(home: &Path) -> Vec<PathBuf> {
    let lib = home.join("Library");
    let mut paths = vec![
        lib.join("WebKit/my-browser/WebsiteData"),
        lib.join("Caches/my-browser/WebKit"),
        lib.join("WebKit/dev.local.my-browser"),
        lib.join("Caches/dev.local.my-browser"),
    ];
    // The old shared cookie file and its unfinished temp copies.
    if let Ok(dir) = std::fs::read_dir(lib.join("HTTPStorages")) {
        for entry in dir.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let ours = ["my-browser.binarycookies", "dev.local.my-browser"].iter().any(|p| name.starts_with(p));
            if ours {
                paths.push(entry.path());
            }
        }
    }
    paths.retain(|p| p.symlink_metadata().is_ok());
    paths.sort();
    paths
}

/// Bytes on disk, without following symlinks.
fn size_of(path: &Path) -> u64 {
    let Ok(meta) = path.symlink_metadata() else { return 0 };
    if !meta.is_dir() {
        return meta.len();
    }
    std::fs::read_dir(path).map_or(0, |d| d.flatten().map(|e| size_of(&e.path())).sum())
}

/// Delete each path; returns (how many, bytes freed).
fn remove_paths(paths: &[PathBuf]) -> (usize, u64) {
    let mut removed = 0;
    let mut bytes = 0;
    for p in paths {
        let size = size_of(p);
        let gone = if p.symlink_metadata().is_ok_and(|m| m.is_dir()) {
            std::fs::remove_dir_all(p)
        } else {
            std::fs::remove_file(p)
        };
        if gone.is_ok() {
            removed += 1;
            bytes += size;
        }
    }
    (removed, bytes)
}

/// What this browser keeps on disk, for the Storage section.
fn storage_json(app: &App) -> String {
    let store = app.store();
    let site_data = match (app.profile_dir(), store.get("profile_id")) {
        (Some(_), Some(id)) => {
            let dir =
                home().join("Library/WebKit").join(webkit_app_id()).join("WebsiteDataStore").join(id.to_lowercase());
            size_of(&dir)
        }
        _ => 0,
    };
    let profile_files = app.profile_dir().map_or(0, size_of);
    let leftovers: Vec<String> = leftovers(&home())
        .iter()
        .map(|p| format!("{{\"path\":{},\"bytes\":{}}}", json_str(&p.display().to_string()), size_of(p)))
        .collect();
    let uninstall: Vec<String> = if is_main_profile(app) {
        uninstall_paths(&home(), &webkit_app_id(), bundle_path().as_deref())
            .iter()
            .map(|p| json_str(&p.display().to_string()))
            .collect()
    } else {
        Vec::new()
    };
    format!(
        "{{\"site_data_bytes\":{site_data},\"profile_bytes\":{profile_files},\"history\":{},\"downloads\":{},\"leftovers\":[{}],\"uninstall\":[{}]}}",
        store.history_count(),
        store.downloads().len(),
        leftovers.join(","),
        uninstall.join(",")
    )
}

/// Whether this app opens web links from other apps, and whether it can
/// (only the .app can: macOS reads the link types it handles from its Info.plist).
fn default_browser_json() -> String {
    let probe = NSURL::URLWithString(&NSString::from_str("https://example.com/")).unwrap();
    let current = NSWorkspace::sharedWorkspace().URLForApplicationToOpenURL(&probe).and_then(|u| u.path());
    let bundle = bundle_path();
    let is_default = matches!((&current, &bundle), (Some(c), Some(b)) if Path::new(&c.to_string()) == b);
    format!("{{\"default\":{is_default},\"bundled\":{}}}", bundle.is_some())
}

/// Ask macOS to make this app the default browser. It confirms with the
/// user itself ("Use my-browser or keep Safari?"), for https too.
fn make_default_browser(reply: Reply) {
    let Some(bundle) = bundle_path() else {
        return reply(Err("Only the installed app (my-browser.app) can be the default browser.".into()));
    };
    let app = NSURL::fileURLWithPath(&NSString::from_str(&bundle.to_string_lossy()));
    let reply = std::cell::RefCell::new(Some(reply));
    let done = RcBlock::new(move |err: *mut NSError| {
        let result = unsafe { err.as_ref() }.map_or(ok(), |e| Err(e.localizedDescription().to_string()));
        if let Some(r) = reply.take() {
            crate::on_main(move || r(result));
        }
    });
    NSWorkspace::sharedWorkspace().setDefaultApplicationAtURL_toOpenURLsWithScheme_completionHandler(
        &app,
        &NSString::from_str("http"),
        Some(&done),
    );
}

/// The app bundle we run from, if any (not when run straight from `cargo`).
fn bundle_path() -> Option<PathBuf> {
    let path = PathBuf::from(objc2_foundation::NSBundle::mainBundle().bundlePath().to_string());
    path.extension().is_some_and(|e| e == "app").then_some(path)
}

/// Uninstalling only makes sense from the real profile; a `--profile` or
/// throwaway session must never wipe the owner's data.
fn is_main_profile(app: &App) -> bool {
    !app.is_bench() && app.profile_dir().is_some_and(|p| p == home().join("Library/Application Support/my-browser"))
}

/// Everything this browser put on the Mac: the profile, WebKit's site data
/// and caches (bundled and unbundled builds), preferences, saved window
/// state, old leftovers, and the app itself.
pub fn uninstall_paths(home: &Path, app_id: &str, bundle: Option<&Path>) -> Vec<PathBuf> {
    let lib = home.join("Library");
    let mut paths = vec![lib.join("Application Support/my-browser")];
    for id in [app_id, "my-browser"] {
        paths.push(lib.join("WebKit").join(id));
        paths.push(lib.join("Caches").join(id));
        paths.push(lib.join("HTTPStorages").join(id));
        paths.push(lib.join("HTTPStorages").join(format!("{id}.binarycookies")));
        paths.push(lib.join("Preferences").join(format!("{id}.plist")));
        paths.push(lib.join("Saved Application State").join(format!("{id}.savedState")));
    }
    paths.extend(leftovers(home));
    paths.extend(bundle.map(Path::to_path_buf));
    paths.retain(|p| p.symlink_metadata().is_ok());
    paths.sort();
    paths.dedup();
    paths
}

/// Delete everything (site data first, through WebKit, so its helper
/// processes don't write it back), then quit without saving the session.
fn uninstall(app: &App, reply: Reply) {
    if !is_main_profile(app) {
        return reply(Err("Uninstall only runs from the main profile.".into()));
    }
    let paths = uninstall_paths(&home(), &webkit_app_id(), bundle_path().as_deref());
    // Asked natively too: the page's own confirm() is only as trustworthy as the page.
    let alert = objc2_app_kit::NSAlert::new(app.mtm());
    alert.setMessageText(&NSString::from_str("Uninstall my-browser?"));
    alert.setInformativeText(&NSString::from_str(
        "This deletes the browser and all of its data: logins, history, extensions and settings. Downloaded files stay. It can't be undone.",
    ));
    alert.addButtonWithTitle(&NSString::from_str("Uninstall"));
    alert.addButtonWithTitle(&NSString::from_str("Cancel"));
    if alert.runModal() != 1000 {
        return reply(Err("cancelled".into()));
    }
    clear_site_data(
        app.data_store(),
        Box::new(move |_| {
            remove_paths(&paths);
            reply(ok());
            std::process::exit(0);
        }),
    );
}

/// Delete cached files only; logins and site storage stay.
fn clear_cache(store: &WKWebsiteDataStore, reply: Reply) {
    let types = unsafe {
        objc2_foundation::NSSet::from_slice(&[
            objc2_web_kit::WKWebsiteDataTypeDiskCache,
            objc2_web_kit::WKWebsiteDataTypeMemoryCache,
            objc2_web_kit::WKWebsiteDataTypeFetchCache,
            objc2_web_kit::WKWebsiteDataTypeOfflineWebApplicationCache,
        ])
    };
    let reply = std::cell::RefCell::new(Some(reply));
    let done = RcBlock::new(move || {
        if let Some(r) = reply.take() {
            r(ok());
        }
    });
    unsafe { store.removeDataOfTypes_modifiedSince_completionHandler(&types, &NSDate::distantPast(), &done) };
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn leftovers_are_only_the_known_old_paths() {
        let home = std::env::temp_dir().join(format!("mybrowser-leftovers-{}", std::process::id()));
        let lib = home.join("Library");
        let make = |p: &str, file: bool| {
            let p = lib.join(p);
            if file {
                std::fs::create_dir_all(p.parent().unwrap()).unwrap();
                std::fs::write(&p, b"12345").unwrap();
            } else {
                std::fs::create_dir_all(&p).unwrap();
                std::fs::write(p.join("x"), b"123").unwrap();
            }
        };
        make("WebKit/my-browser/WebsiteData", false);
        make("Caches/my-browser/WebKit", false);
        make("WebKit/dev.local.my-browser", false);
        make("HTTPStorages/my-browser.binarycookies", true);
        make("HTTPStorages/my-browser.binarycookies_tmp_1.dat", true);
        // Must survive: a current profile's store, other apps' files.
        make("WebKit/my-browser/WebsiteDataStore/abc", false);
        make("WebKit/my-browser/ContentRuleLists", false);
        make("HTTPStorages/com.other.app.binarycookies", true);
        make("Caches/com.other.app", false);

        let found = leftovers(&home);
        assert_eq!(found.len(), 5, "{found:?}");
        assert!(found.iter().all(|p| !p.to_string_lossy().contains("WebsiteDataStore")));
        assert_eq!(size_of(&lib.join("WebKit/my-browser/WebsiteData")), 3);

        let (removed, bytes) = remove_paths(&found);
        assert_eq!((removed, bytes), (5, 3 + 3 + 3 + 5 + 5));
        assert!(leftovers(&home).is_empty());
        assert!(lib.join("WebKit/my-browser/WebsiteDataStore/abc/x").exists());
        assert!(lib.join("WebKit/my-browser/ContentRuleLists").exists());
        assert!(lib.join("HTTPStorages/com.other.app.binarycookies").exists());
        assert!(lib.join("Caches/com.other.app").exists());
        std::fs::remove_dir_all(&home).unwrap();
    }

    #[test]
    fn uninstall_takes_only_this_browsers_files() {
        let home = std::env::temp_dir().join(format!("mybrowser-uninstall-{}", std::process::id()));
        let lib = home.join("Library");
        for d in [
            "Application Support/my-browser",
            "WebKit/local.my-browser/WebsiteDataStore/abc",
            "WebKit/my-browser",
            "Caches/local.my-browser",
            "Saved Application State/local.my-browser.savedState",
            // Must survive: other apps.
            "Application Support/my-browser-other",
            "WebKit/com.other.app",
            "Caches/com.other.app",
        ] {
            std::fs::create_dir_all(lib.join(d)).unwrap();
        }
        std::fs::create_dir_all(lib.join("Preferences")).unwrap();
        std::fs::write(lib.join("Preferences/local.my-browser.plist"), b"").unwrap();
        let bundle = home.join("Apps/my-browser.app");
        std::fs::create_dir_all(&bundle).unwrap();

        let found = uninstall_paths(&home, "local.my-browser", Some(&bundle));
        assert_eq!(found.len(), 7, "{found:?}");
        remove_paths(&found);
        assert!(uninstall_paths(&home, "local.my-browser", Some(&bundle)).is_empty());
        assert!(lib.join("Application Support/my-browser-other").exists());
        assert!(lib.join("WebKit/com.other.app").exists());
        assert!(lib.join("Caches/com.other.app").exists());
        std::fs::remove_dir_all(&home).unwrap();
    }

    #[test]
    fn sizes_do_not_follow_symlinks() {
        let dir = std::env::temp_dir().join(format!("mybrowser-size-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("inner")).unwrap();
        std::fs::write(dir.join("inner/a"), vec![0u8; 100]).unwrap();
        std::os::unix::fs::symlink("/usr", dir.join("link")).unwrap();
        let link_len = dir.join("link").symlink_metadata().unwrap().len();
        assert_eq!(size_of(&dir), 100 + link_len);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
