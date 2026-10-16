//! Web extensions, off until clicked.
//!
//! Every enabled extension gets a toolbar button. Its manifest is read at
//! launch (no process starts), but the extension itself only starts the first
//! time its button is clicked, so an unused extension costs no memory.
//!
//! Access is per extension:
//! - `click` (the default): the extension can't see or change any site until
//!   its button is clicked on it. That turns it on for that site in that tab;
//!   leaving the site (or closing the tab) turns it off again.
//! - `always`: it runs on every site its manifest asks for, like in Chrome.
//!
//! Optional permissions the extension asks for while running are put to the
//! user as a prompt; nothing is granted silently.

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use std::path::Path;

use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, Message, msg_send, sel};
use objc2_app_kit::{NSBackingStoreType, NSImage, NSWindow, NSWindowStyleMask};
use objc2_foundation::{
    NSArray, NSData, NSError, NSJSONReadingOptions, NSJSONSerialization, NSJSONWritingOptions, NSMutableArray,
    NSMutableDictionary, NSObjectProtocol, NSRect, NSSize, NSString, NSURL,
};
use objc2_web_kit::{
    WKWebExtension, WKWebExtensionContext, WKWebExtensionContextPermissionStatus, WKWebExtensionMatchPattern,
    WKWebExtensionTab, WKWebView,
};

use crate::App;
use crate::store::site_of;
use crate::tab::Tab;

/// What a toolbar button shows.
pub struct ExtButton {
    pub id: i64,
    pub image: Retained<NSImage>,
    pub label: String,
    /// Running on this tab's page (always-on, or clicked on this site).
    pub on: bool,
    pub always: bool,
}

pub type Done = Box<dyn FnOnce(Result<String, String>)>;

impl App {
    /// At launch: read every enabled extension's manifest for its button;
    /// start only the ones set to run on every site.
    pub fn start_extensions(&self) {
        for ext in self.store().extensions().into_iter().filter(|e| e.enabled) {
            self.register_extension(ext.id, &ext.path, Box::new(|_| {}));
        }
    }

    /// Read an installed extension and give it a button (starting it if it's
    /// set to run everywhere). `done` gets its name or an error.
    pub fn register_extension(&self, id: i64, path: &str, done: Done) {
        // Only the profile's own copy is changed, never a folder the user picked.
        if self.profile_dir().is_some_and(|p| Path::new(path).starts_with(p.join("extensions"))) {
            adapt_offscreen(Path::new(path));
        }
        let this = self.retain();
        read_extension(
            self.mtm(),
            path,
            Box::new(move |result| {
                let ext = match result {
                    Ok(ext) => ext,
                    Err(e) => return done(Err(e)),
                };
                let name = extension_name(&ext);
                this.ivars().ext_meta.borrow_mut().insert(id, ext);
                if this.extension_access(id) == "always"
                    && let Err(e) = this.start_extension(id)
                {
                    this.ivars().ext_meta.borrow_mut().remove(&id);
                    return done(Err(e));
                }
                this.refresh_extension_buttons();
                done(Ok(name))
            }),
        );
    }

    pub fn extension_meta(&self, id: i64) -> Option<Retained<WKWebExtension>> {
        self.ivars().ext_meta.borrow().get(&id).cloned()
    }

    pub fn extension_context(&self, id: i64) -> Option<Retained<WKWebExtensionContext>> {
        self.ivars().ext_contexts.borrow().get(&id).cloned()
    }

    fn extension_access(&self, id: i64) -> String {
        self.store().extensions().into_iter().find(|e| e.id == id).map_or_else(|| "click".into(), |e| e.access)
    }

    /// Start the extension (its background page or worker runs from now on).
    /// API permissions from the manifest are granted (the user installed it);
    /// site access only for `always` extensions.
    fn start_extension(&self, id: i64) -> Result<Retained<WKWebExtensionContext>, String> {
        if let Some(ctx) = self.extension_context(id) {
            return Ok(ctx);
        }
        let ext = self.extension_meta(id).ok_or("This extension isn't installed.")?;
        let granted = WKWebExtensionContextPermissionStatus::GrantedExplicitly;
        let ctx = unsafe { WKWebExtensionContext::contextForExtension(&ext) };
        unsafe {
            // Stable id: the extension keeps its storage across launches.
            ctx.setUniqueIdentifier(&NSString::from_str(&format!("ext-{id}")));
            for p in ext.requestedPermissions().iter() {
                ctx.setPermissionStatus_forPermission(granted, &p);
            }
            if self.extension_access(id) == "always" {
                for m in ext.allRequestedMatchPatterns().iter() {
                    ctx.setPermissionStatus_forMatchPattern(granted, &m);
                }
            }
        }
        let controller = self.ivars().extensions.get().unwrap();
        unsafe { controller.loadExtensionContext_error(&ctx) }.map_err(|e| e.localizedDescription().to_string())?;
        self.ivars().ext_contexts.borrow_mut().insert(id, ctx.clone());
        Ok(ctx)
    }

    /// Stop an extension's running part, keeping its button.
    fn stop_extension(&self, id: i64) {
        for tab in self.ivars().tabs.borrow().iter() {
            tab.ivars().ext_on.borrow_mut().retain(|(e, _)| *e != id);
        }
        if let Some(window) = self.ivars().ext_audio_windows.borrow_mut().remove(&id) {
            for view in window.contentView().map(|c| c.subviews()).unwrap_or_default() {
                view.removeFromSuperview();
            }
        }
        if let Some(ctx) = self.ivars().ext_contexts.borrow_mut().remove(&id) {
            let _ = unsafe { self.ivars().extensions.get().unwrap().unloadExtensionContext_error(&ctx) };
        }
    }

    /// Disabled or removed: stop it and drop its button.
    pub fn unload_extension(&self, id: i64) {
        self.stop_extension(id);
        self.ivars().ext_meta.borrow_mut().remove(&id);
        self.refresh_extension_buttons();
    }

    /// Read an extension's folder again (after its files changed), and
    /// start it again if it was running.
    pub fn reload_extension(&self, id: i64, path: &str, done: Done) {
        let running = self.extension_context(id).is_some();
        self.unload_extension(id);
        let this = self.retain();
        self.register_extension(
            id,
            path,
            Box::new(move |r| {
                done(r.and_then(|name| {
                    if running {
                        this.start_extension(id)?;
                    }
                    Ok(name)
                }))
            }),
        );
    }

    /// Update a Web Store extension: download its latest package (through
    /// WebKit, like any download, without leaving the page), check it is
    /// signed as the same extension, ask again if it wants more than it had,
    /// then replace its files and reload it. Its storage stays: it is keyed
    /// by the row id. `test` = (package address, Web Store id), bench only.
    pub fn update_extension(&self, id: i64, test: Option<(String, String)>, reply: crate::internal::Reply) {
        let Some(ext) = self.store().extensions().into_iter().find(|e| e.id == id) else {
            return reply(Err("no such extension".into()));
        };
        let (url, webstore) = match test {
            Some((url, webstore)) => (url, webstore),
            None if !ext.webstore.is_empty() => (webstore_package_url(&ext.webstore), ext.webstore.clone()),
            None => return reply(Err("Only extensions added from the Chrome Web Store can be updated here.".into())),
        };
        let Some(nsurl) = NSURL::URLWithString(&NSString::from_str(&url)) else {
            return reply(Err("bad address".into()));
        };
        // Any web view of the profile can start it; a new one if none is awake.
        let web = self.all_tabs().iter().filter(|t| !t.is_private()).find_map(|t| t.web());
        let web = web.unwrap_or_else(|| unsafe {
            objc2_web_kit::WKWebView::initWithFrame_configuration(
                objc2_web_kit::WKWebView::alloc(self.mtm()),
                objc2_foundation::NSRect::ZERO,
                &self.make_config(false),
            )
        });
        self.expect_package(&webstore);
        self.ivars().pending_update.replace(Some((id, reply)));
        let this = self.retain();
        let keep = web.clone();
        let started = RcBlock::new(move |download: std::ptr::NonNull<objc2_web_kit::WKDownload>| {
            let _ = &keep;
            this.start_download(unsafe { download.as_ref() });
        });
        unsafe {
            web.startDownloadUsingRequest_completionHandler(
                &objc2_foundation::NSURLRequest::requestWithURL(&nsurl),
                &started,
            )
        };
    }

    /// The update's package arrived: unpack it (its signature must match
    /// the id it was asked for), then swap the files in.
    pub fn finish_update(&self, id: i64, file: std::path::PathBuf, reply: crate::internal::Reply) {
        let unpacked = unpack(&file);
        let _ = std::fs::remove_file(&file);
        let (dir, ext) = match (unpacked, self.store().extensions().into_iter().find(|e| e.id == id)) {
            (Ok(dir), Some(ext)) => (dir, ext),
            (Err(e), _) => return reply(Err(e)),
            (Ok(dir), None) => {
                crate::internal::drop_unpacked(&dir);
                return reply(Err("no such extension".into()));
            }
        };
        let had: Vec<String> = self.extension_meta(id).map_or_else(Vec::new, |m| permissions(&m));
        let this = self.retain();
        let path = dir.to_string_lossy().into_owned();
        read_extension(
            self.mtm(),
            &path,
            Box::new(move |result| {
                let new = match result {
                    Ok(new) => new,
                    Err(e) => {
                        crate::internal::drop_unpacked(&dir);
                        return reply(Err(e));
                    }
                };
                let more: Vec<String> = permissions(&new).into_iter().filter(|p| !had.contains(p)).collect();
                if !more.is_empty() && !this.is_bench() {
                    let alert = objc2_app_kit::NSAlert::new(this.mtm());
                    alert.setMessageText(&NSString::from_str(&format!("Update “{}”?", extension_name(&new))));
                    alert.setInformativeText(&NSString::from_str(&format!(
                        "The new version also asks for: {}.",
                        more.join(", ")
                    )));
                    alert.addButtonWithTitle(&NSString::from_str("Update"));
                    alert.addButtonWithTitle(&NSString::from_str("Cancel"));
                    if alert.runModal() != 1000 {
                        crate::internal::drop_unpacked(&dir);
                        return reply(Err("cancelled".into()));
                    }
                }
                let version = unsafe { new.displayVersion() }.map(|v| v.to_string()).unwrap_or_default();
                let copied = crate::internal::recopy(&dir, Path::new(&ext.path));
                crate::internal::drop_unpacked(&dir);
                if let Err(e) = copied {
                    return reply(Err(format!("Couldn't copy the update: {e}")));
                }
                this.reload_extension(
                    id,
                    &ext.path,
                    Box::new(move |r| {
                        reply(r.map(|name| {
                            format!(
                                "{{\"name\":{},\"version\":{}}}",
                                crate::store::json_str(&name),
                                crate::store::json_str(&version)
                            )
                        }))
                    }),
                );
            }),
        );
    }

    /// Switch between `click` and `always`. A running extension restarts
    /// under the new access.
    pub fn set_extension_access(&self, id: i64, access: &str) -> Result<(), String> {
        self.store().set_extension_access(id, access)?;
        let running = self.extension_context(id).is_some();
        self.stop_extension(id);
        if (running || access == "always") && self.extension_meta(id).is_some() {
            self.start_extension(id)?;
        }
        self.refresh_extension_buttons();
        Ok(())
    }

    /// The toolbar button was clicked on `tab`: start the extension if
    /// needed, turn it on for this site, then run its action (open its popup).
    /// If it has content scripts for the page, the page reloads first so
    /// they run, and the popup opens when it's loaded.
    pub fn extension_clicked(&self, id: i64, tab: &Tab) -> Result<(), String> {
        let ctx = self.start_extension(id)?;
        let url = tab.url();
        let mut reload = false;
        let web_page = url.starts_with("http://") || url.starts_with("https://");
        if let (true, "click", Some(site)) = (web_page, self.extension_access(id).as_str(), site_of(&url)) {
            let already = tab.ivars().ext_on.borrow().iter().any(|(e, s)| *e == id && *s == site);
            if !already {
                if let Some(pattern) = site_pattern(&site, self.mtm()) {
                    unsafe {
                        ctx.setPermissionStatus_forMatchPattern(
                            WKWebExtensionContextPermissionStatus::GrantedExplicitly,
                            &pattern,
                        )
                    };
                }
                tab.ivars().ext_on.borrow_mut().push((id, site));
                let page = NSURL::URLWithString(&NSString::from_str(&url));
                reload = tab.web().is_some() && page.is_some_and(|u| unsafe { ctx.hasInjectedContentForURL(&u) });
            }
        }
        if reload {
            tab.ivars().pending_ext_action.set(Some(id));
            tab.reload();
        } else {
            self.perform_extension_action(id, tab);
        }
        self.refresh_extension_buttons_for(tab);
        Ok(())
    }

    /// Run the extension's action for `tab`: WebKit opens its popup (through
    /// `presentPopupForAction`) or fires `action.onClicked`.
    pub fn perform_extension_action(&self, id: i64, tab: &Tab) {
        if let Some(ctx) = self.extension_context(id) {
            unsafe { ctx.performActionForTab(Some(ProtocolObject::from_ref(tab))) };
        }
    }

    /// `tab` is leaving its site (`new_site` is where it's going; None when
    /// it closes). Extensions clicked on the old site turn off there, unless
    /// another tab still has them on for it.
    pub fn extensions_leave_site(&self, tab: &Tab, new_site: Option<&str>) {
        let left: Vec<(i64, String)> = {
            let mut on = tab.ivars().ext_on.borrow_mut();
            let (stay, left) = on.drain(..).partition(|(_, s)| Some(s.as_str()) == new_site);
            *on = stay;
            left
        };
        if left.is_empty() {
            return;
        }
        for (id, site) in left {
            let still_used = self.ivars().tabs.borrow().iter().any(|t| {
                !std::ptr::eq(&**t, tab) && t.ivars().ext_on.borrow().iter().any(|(e, s)| *e == id && *s == site)
            });
            if let (false, Some(ctx), Some(pattern)) =
                (still_used, self.extension_context(id), site_pattern(&site, self.mtm()))
            {
                unsafe {
                    ctx.setPermissionStatus_forMatchPattern(WKWebExtensionContextPermissionStatus::Unknown, &pattern)
                };
            }
        }
        self.refresh_extension_buttons_for(tab);
    }

    pub fn refresh_extension_buttons(&self) {
        for tab in self.ivars().tabs.borrow().clone() {
            self.refresh_extension_buttons_for(&tab);
        }
    }

    pub fn refresh_extension_buttons_for(&self, tab: &Tab) {
        tab.set_extension_buttons(&self.extension_buttons(tab));
    }

    /// The buttons `tab` shows, in install order.
    pub fn extension_buttons(&self, tab: &Tab) -> Vec<ExtButton> {
        if tab.is_private() {
            return Vec::new();
        }
        let meta = self.ivars().ext_meta.borrow();
        let size = NSSize::new(18.0, 18.0);
        self.store()
            .extensions()
            .into_iter()
            .filter_map(|e| {
                let ext = meta.get(&e.id)?;
                let ctx = self.extension_context(e.id);
                let action = ctx.as_ref().and_then(|c| unsafe { c.actionForTab(Some(ProtocolObject::from_ref(tab))) });
                let image = action
                    .as_ref()
                    .and_then(|a| unsafe { a.iconForSize(size) })
                    .or_else(|| unsafe { ext.iconForSize(size) })
                    .or_else(|| {
                        NSImage::imageWithSystemSymbolName_accessibilityDescription(
                            &NSString::from_str("puzzlepiece.extension"),
                            None,
                        )
                    })?;
                let name = extension_name(ext);
                let label =
                    action.as_ref().map(|a| unsafe { a.label() }.to_string()).filter(|l| !l.is_empty()).unwrap_or(name);
                let always = e.access == "always";
                let on = always || tab.ivars().ext_on.borrow().iter().any(|(id, _)| *id == e.id);
                Some(ExtButton { id: e.id, image, label, on, always })
            })
            .collect()
    }

    /// Which installed extension a WebKit context belongs to.
    pub fn extension_id_of(&self, ctx: &WKWebExtensionContext) -> Option<i64> {
        self.ivars().ext_contexts.borrow().iter().find(|(_, c)| std::ptr::eq(&***c, ctx)).map(|(id, _)| *id)
    }

    /// Put the extension's background page in a hidden window. WebKit only
    /// starts media in a web view that's in a window (its own offscreen
    /// documents get one the same way), so without this the offscreen
    /// stand-in's audio stays "interrupted". The window is never shown.
    pub fn host_extension_audio(&self, ctx: &WKWebExtensionContext, user_agent: Option<&str>) -> bool {
        let Some(id) = self.extension_id_of(ctx) else { return false };
        if !ctx.respondsToSelector(sel!(_backgroundWebView)) {
            return false;
        }
        let view: Option<Retained<WKWebView>> = unsafe { msg_send![ctx, _backgroundWebView] };
        let Some(view) = view else { return false };
        // The extension's own "set User-Agent" rule, which WebKit doesn't
        // apply to extension pages. Only on its background page, and only a
        // plain one-line value.
        if let Some(ua) = user_agent.filter(|ua| ua.len() <= 512 && ua.chars().all(|c| (' '..='~').contains(&c))) {
            unsafe { view.setCustomUserAgent(Some(&NSString::from_str(ua))) };
        }
        let mtm = self.mtm();
        let mut windows = self.ivars().ext_audio_windows.borrow_mut();
        let window = windows.entry(id).or_insert_with(|| {
            let w = unsafe {
                NSWindow::initWithContentRect_styleMask_backing_defer(
                    NSWindow::alloc(mtm),
                    NSRect::ZERO,
                    NSWindowStyleMask::Borderless,
                    NSBackingStoreType::Buffered,
                    false,
                )
            };
            unsafe { w.setReleasedWhenClosed(false) };
            w
        });
        let content = window.contentView().unwrap();
        // A background page that was unloaded and loaded again is a new view.
        for old in content.subviews() {
            if !std::ptr::eq(&*old, &**view as &objc2_app_kit::NSView) {
                old.removeFromSuperview();
            }
        }
        if unsafe { view.superview() }.is_none() {
            content.addSubview(&view);
        }
        true
    }

    /// Our tab behind a WebKit extension-tab object.
    pub fn tab_of(&self, tab: &ProtocolObject<dyn WKWebExtensionTab>) -> Option<Retained<Tab>> {
        let ptr = tab as *const _ as *const u8;
        self.ivars().tabs.borrow().iter().find(|t| std::ptr::eq(Retained::as_ptr(t) as *const u8, ptr)).cloned()
    }
}

/// `*://site/*`: http and https on the site's host.
fn site_pattern(site: &str, mtm: MainThreadMarker) -> Option<Retained<WKWebExtensionMatchPattern>> {
    unsafe { WKWebExtensionMatchPattern::matchPatternWithString(&NSString::from_str(&format!("*://{site}/*")), mtm) }
}

pub fn extension_name(ext: &WKWebExtension) -> String {
    unsafe { ext.displayName() }.map(|n| n.to_string()).unwrap_or_else(|| "Extension".into())
}

/// Read an unpacked extension's manifest and resources (nothing runs).
pub fn read_extension(
    mtm: MainThreadMarker,
    path: &str,
    done: Box<dyn FnOnce(Result<Retained<WKWebExtension>, String>)>,
) {
    let url = NSURL::fileURLWithPath_isDirectory(&NSString::from_str(path), true);
    let done = std::cell::RefCell::new(Some(done));
    let block = RcBlock::new(move |ext: *mut WKWebExtension, err: *mut NSError| {
        let Some(done) = done.take() else { return };
        match unsafe { Retained::retain(ext) } {
            Some(ext) => done(Ok(ext)),
            None => done(Err(unsafe { err.as_ref() }
                .map_or("Couldn't read the extension.".into(), |e| e.localizedDescription().to_string()))),
        }
    });
    unsafe { WKWebExtension::extensionWithResourceBaseURL_completionHandler(&url, &block, mtm) };
}

/// The User-Agent the extension's background page uses (its own rule, set by
/// `host_extension_audio`), for the network gate's connections.
pub fn background_user_agent(ctx: &WKWebExtensionContext) -> Option<String> {
    if !ctx.respondsToSelector(sel!(_backgroundWebView)) {
        return None;
    }
    let view: Option<Retained<WKWebView>> = unsafe { msg_send![ctx, _backgroundWebView] };
    unsafe { view?.customUserAgent() }.map(|ua| ua.to_string()).filter(|ua| !ua.is_empty())
}

/// The `runtime.sendNativeMessage` app the offscreen stand-in talks to.
pub const OFFSCREEN_APP: &str = "my-browser.offscreen";
const OFFSCREEN_SHIM: &str = "_my-browser-offscreen.js";
static OFFSCREEN_JS: &str = include_str!("../resources/offscreen.js");
const WEBSOCKET_SHIM: &str = "_my-browser-websocket.js";
static WEBSOCKET_JS: &str = include_str!("../resources/websocket.js");

/// WebKit has no `chrome.offscreen`. For an extension that asks for it, turn
/// its background service worker into a background page that loads
/// resources/offscreen.js first (a stand-in built on an iframe), and let it
/// send the one native message that stand-in needs. Its pages also get
/// resources/websocket.js, which sends their WebSockets through the network
/// gate (netgate.rs). Does nothing to other extensions; on one changed
/// before, only refreshes the stand-ins.
// ponytail: a worker that uses `importScripts` or `self.registration` breaks
// as a page; fine for read-aloud extensions, revisit if WebKit adds offscreen.
pub fn adapt_offscreen(dir: &Path) -> Option<()> {
    let path = dir.join("manifest.json");
    let data = NSData::with_bytes(&std::fs::read(&path).ok()?);
    let manifest =
        NSJSONSerialization::JSONObjectWithData_options_error(&data, NSJSONReadingOptions::MutableContainers).ok()?;
    let manifest = manifest.downcast::<NSMutableDictionary>().ok()?;
    let key = |k: &str| NSString::from_str(k);
    let perms = manifest.objectForKey(&key("permissions"))?.downcast::<NSMutableArray>().ok()?;
    if !perms.containsObject(&key("offscreen")) {
        return None;
    }
    let background = manifest.objectForKey(&key("background"))?.downcast::<NSMutableDictionary>().ok()?;
    let shim = dir.join(OFFSCREEN_SHIM);
    let changed_before = shim.exists();
    // Kept current with this build.
    std::fs::write(&shim, OFFSCREEN_JS).ok()?;
    std::fs::write(dir.join(WEBSOCKET_SHIM), WEBSOCKET_JS).ok()?;
    add_websocket_shim(dir);
    if changed_before {
        return Some(());
    }
    let worker = background.objectForKey(&key("service_worker"))?.downcast::<NSString>().ok()?;
    let scripts = NSArray::from_retained_slice(&[key(OFFSCREEN_SHIM), worker]);
    unsafe {
        let _: () = msg_send![&*background, removeObjectForKey: &*key("service_worker")];
        let _: () = msg_send![&*background, setObject: &*scripts, forKey: &*key("scripts")];
        if !perms.containsObject(&key("nativeMessaging")) {
            let _: () = msg_send![&*perms, addObject: &*key("nativeMessaging")];
        }
        let out = NSJSONSerialization::dataWithJSONObject_options_error(&manifest, NSJSONWritingOptions::PrettyPrinted)
            .ok()?;
        std::fs::write(&path, out.to_vec()).ok()
    }
}

/// Load resources/websocket.js first in every HTML page of the extension
/// (a classic script, so it runs before the page's modules).
fn add_websocket_shim(dir: &Path) {
    let tag = format!("<script src=\"/{WEBSOCKET_SHIM}\"></script>");
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let (path, Ok(kind)) = (entry.path(), entry.file_type()) else { continue };
        if kind.is_dir() {
            add_websocket_shim(&path);
        } else if kind.is_file() && path.extension().is_some_and(|e| e.eq_ignore_ascii_case("html")) {
            let Ok(html) = std::fs::read_to_string(&path) else { continue };
            if !html.contains(&tag) {
                let _ = std::fs::write(&path, with_script(&html, &tag));
            }
        }
    }
}

/// `html` with `tag` at the start of its head (or of the document, after
/// any doctype, so the page keeps its rendering mode).
fn with_script(html: &str, tag: &str) -> String {
    let lower = html.to_ascii_lowercase();
    // The tag itself: `<head>` or `<head …>`, not `<header>`.
    let after = |name: &str| {
        let open = format!("<{name}");
        let i = lower
            .match_indices(&open)
            .map(|(i, _)| i)
            .find(|&i| lower[i + open.len()..].starts_with(|c: char| c == '>' || c.is_ascii_whitespace()))?;
        lower[i..].find('>').map(|j| i + j + 1)
    };
    let at = after("head").or_else(|| after("html")).or_else(|| after("!doctype")).unwrap_or(0);
    format!("{}{tag}{}", &html[..at], &html[at..])
}

/// Where packages are unpacked before they're installed (copied into the
/// profile, after which the unpacked copy is deleted).
pub fn unpack_root() -> std::path::PathBuf {
    std::env::temp_dir().join("my-browser-packages")
}

/// Unpack a Chrome extension package (.crx) or a .zip into a temporary
/// folder, and return the folder holding its manifest.json. `ditto` keeps
/// every entry inside the folder (it drops `..` and absolute paths), and
/// installing copies only regular files, never symlinks.
pub fn unpack(file: &Path) -> Result<std::path::PathBuf, String> {
    let bytes = std::fs::read(file).map_err(|e| format!("Couldn't read {}: {e}", file.display()))?;
    let (zip, id) = package_zip(&bytes)?;
    // A Web Store package is saved as `<id>….crx` (see the download
    // handler): it must be signed by that extension's key.
    if file.starts_with(unpack_root()) {
        let name = file.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        if !id.as_ref().is_some_and(|id| name.starts_with(id.as_str())) {
            return Err("This package can't be trusted: it isn't the extension you asked for.".into());
        }
    }
    let stem = file.file_stem().map_or("extension".into(), |s| s.to_string_lossy().into_owned());
    let root = unpack_root().join(format!("{}-{}", std::process::id(), crate::crash::now_ms()));
    let out = root.join(&stem);
    std::fs::create_dir_all(&root).map_err(|e| e.to_string())?;
    let archive = root.join("package.zip");
    std::fs::write(&archive, zip).map_err(|e| e.to_string())?;
    let unpacked = std::process::Command::new("/usr/bin/ditto").arg("-x").arg("-k").arg(&archive).arg(&out).status();
    let _ = std::fs::remove_file(&archive);
    if !unpacked.is_ok_and(|s| s.success()) {
        let _ = std::fs::remove_dir_all(&root);
        return Err("Couldn't unpack that package.".into());
    }
    // The manifest at the top, or in the one folder the archive holds.
    if out.join("manifest.json").is_file() {
        return Ok(out);
    }
    let dirs: Vec<_> = std::fs::read_dir(&out)
        .map(|d| d.flatten().map(|e| e.path()).filter(|p| p.is_dir() && !p.ends_with("__MACOSX")).collect())
        .unwrap_or_default();
    match dirs.as_slice() {
        [one] if one.join("manifest.json").is_file() => Ok(one.clone()),
        _ => {
            let _ = std::fs::remove_dir_all(&root);
            Err("That package has no manifest.json.".into())
        }
    }
}

/// The zip archive in a package: a .zip as is (the user's own file), or the
/// one inside a .crx once its signatures check out (with the extension id
/// they prove).
fn package_zip(bytes: &[u8]) -> Result<(&[u8], Option<String>), String> {
    if bytes.starts_with(b"PK\x03\x04") {
        return Ok((bytes, None));
    }
    if !bytes.starts_with(b"Cr24") {
        return Err("That isn't an extension package (.crx or .zip).".into());
    }
    crate::crx::verify(bytes).map(|(zip, id)| (zip, Some(id)))
}

/// The extension id on a Chrome Web Store page
/// (`chromewebstore.google.com/detail/<name>/<id>`, or the old
/// `chrome.google.com/webstore/detail/...`).
pub fn webstore_id(url: &str) -> Option<String> {
    let rest = url.strip_prefix("https://")?;
    let (host, path) = rest.split_once('/')?;
    let path = path.split(['?', '#']).next()?;
    let detail = match host {
        "chromewebstore.google.com" => path.strip_prefix("detail/")?,
        "chrome.google.com" => path.strip_prefix("webstore/detail/")?,
        _ => return None,
    };
    let id = detail.trim_end_matches('/').rsplit('/').next()?;
    (id.len() == 32 && id.bytes().all(|b| (b'a'..=b'p').contains(&b))).then(|| id.to_string())
}

/// Google's download address for a Web Store extension's package.
pub fn webstore_package_url(id: &str) -> String {
    format!(
        "https://clients2.google.com/service/update2/crx?response=redirect&prodversion=140.0&acceptformat=crx3&x=id%3D{id}%26uc"
    )
}

/// Everything an extension asks for: API permissions and sites.
fn permissions(ext: &WKWebExtension) -> Vec<String> {
    let mut p: Vec<String> = unsafe { ext.requestedPermissions() }.iter().map(|p| p.to_string()).collect();
    p.extend(unsafe { ext.allRequestedMatchPatterns() }.iter().map(|m| unsafe { m.string() }.to_string()));
    p
}

/// What an extension asks for, in words, for the install prompt.
pub fn describe_access(ext: &WKWebExtension) -> String {
    let mut perms: Vec<String> = unsafe { ext.requestedPermissions() }.iter().map(|p| p.to_string()).collect();
    perms.sort();
    let mut sites: Vec<String> =
        unsafe { ext.allRequestedMatchPatterns() }.iter().map(|m| unsafe { m.string() }.to_string()).collect();
    sites.sort();
    sites.dedup();
    let mut out = String::new();
    if !perms.is_empty() {
        out += &format!("It can use: {}.\n\n", perms.join(", "));
    }
    if !sites.is_empty() {
        out += &format!("It asks to read and change: {}.\n\n", sites.join(", "));
    }
    out += "It stays off on every site until you click its button there, and turns off again when you leave the site. You can let it run everywhere later.";
    out
}

#[cfg(test)]
mod tests {
    use super::{package_zip, webstore_id, with_script};

    #[test]
    fn packages_yield_their_zip() {
        let zip = b"PK\x03\x04rest";
        assert_eq!(package_zip(zip).unwrap(), (&zip[..], None));
        let crx = include_bytes!("../tests/fixtures/title.crx");
        let (inner, id) = package_zip(crx).unwrap();
        assert!(inner.starts_with(b"PK\x03\x04") && id.is_some());
        assert!(package_zip(b"<html>").is_err());
    }

    #[test]
    fn web_store_pages_give_the_extension_id() {
        let id = "cjpalhdlnbpafiamejdnhcphjbkeiagm";
        assert_eq!(
            webstore_id(&format!("https://chromewebstore.google.com/detail/ublock-origin/{id}")).as_deref(),
            Some(id)
        );
        assert_eq!(webstore_id(&format!("https://chromewebstore.google.com/detail/{id}?hl=en")).as_deref(), Some(id));
        assert_eq!(webstore_id(&format!("https://chrome.google.com/webstore/detail/x/{id}/")).as_deref(), Some(id));
        assert_eq!(webstore_id(&format!("https://evil.example/detail/x/{id}")), None);
        assert_eq!(webstore_id(&format!("http://chromewebstore.google.com/detail/x/{id}")), None, "https only");
        assert_eq!(webstore_id("https://chromewebstore.google.com/detail/x/not-an-id"), None);
    }

    #[test]
    fn the_websocket_shim_goes_first_in_the_head() {
        let tag = "<script src=\"/s.js\"></script>";
        assert_eq!(
            with_script("<!DOCTYPE html><HTML><Head lang=x><script type=module src=a.js></script></head>", tag),
            format!("<!DOCTYPE html><HTML><Head lang=x>{tag}<script type=module src=a.js></script></head>")
        );
        assert_eq!(
            with_script("<!doctype html><header>x</header>", tag),
            format!("<!doctype html>{tag}<header>x</header>")
        );
        assert_eq!(with_script("<p>x", tag), format!("{tag}<p>x"));
    }
}
