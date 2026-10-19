/// A line on stdout for the bench harness. A harness that has stopped
/// reading (a failed test) must not crash the browser, as `println!` would.
macro_rules! say {
    ($($arg:tt)*) => {{
        use std::io::Write;
        let _ = writeln!(std::io::stdout(), $($arg)*);
    }};
}

mod autofill;
mod crash;
mod crx;
mod extensions;
mod extwindow;
mod files;
mod home;
mod import;
mod internal;
mod netgate;
mod passwords;
mod pdf;
mod privacy;
mod profiles;
mod screenshot;
mod shortcuts;
mod sitedata;
mod store;
mod suggest;
mod syncfile;
mod tab;
mod tabsearch;
mod translate;
mod usage;

use std::cell::{Cell, OnceCell, RefCell};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use block2::{DynBlock, RcBlock};
use objc2::rc::Retained;
use objc2::runtime::{AnyClass, AnyObject, Bool, ProtocolObject, Sel};
use objc2::{
    AnyThread, ClassType, DefinedClass, MainThreadMarker, MainThreadOnly, Message, define_class, msg_send, sel,
};
use objc2_app_kit::{
    NSAlert, NSApplication, NSApplicationActivationPolicy, NSApplicationDelegate, NSEvent, NSEventModifierFlags,
    NSEventType, NSMenu, NSMenuDelegate, NSMenuItem, NSWindow, NSWindowOcclusionState, NSWindowOrderingMode,
};
use objc2_foundation::{
    NSArray, NSData, NSError, NSNotification, NSObject, NSObjectProtocol, NSPoint, NSString, NSTimer, NSURL,
    NSURLResponse, NSUUID,
};
#[allow(deprecated)]
use objc2_web_kit::WKProcessPool;
use objc2_web_kit::{
    WKAudiovisualMediaTypes, WKContentRuleList, WKContentWorld, WKDownload, WKDownloadDelegate, WKPreferences,
    WKScriptMessage, WKScriptMessageHandler, WKScriptMessageHandlerWithReply, WKURLSchemeHandler, WKURLSchemeTask,
    WKUserContentController, WKUserScript, WKUserScriptInjectionTime, WKWebExtension, WKWebExtensionAction,
    WKWebExtensionContext, WKWebExtensionController, WKWebExtensionControllerConfiguration,
    WKWebExtensionControllerDelegate, WKWebExtensionMatchPattern, WKWebExtensionTab,
    WKWebExtensionTabChangedProperties, WKWebExtensionTabConfiguration, WKWebExtensionWindow, WKWebView,
    WKWebViewConfiguration, WKWebpagePreferences, WKWebsiteDataStore,
};

use extwindow::ExtWindow;
use store::Store;
use tab::{INTERNAL, RELEASES, Tab};

/// A download in flight: (downloads row, download, file). Private windows'
/// downloads have no row (0).
type Download = (i64, Retained<WKDownload>, String);

#[derive(Default)]
pub struct AppIvars {
    store: OnceCell<Store>,
    /// Persistent profile directory; None = throwaway (benchmarks and tests).
    profile: OnceCell<Option<PathBuf>>,
    /// Held for the app's lifetime: one running browser per profile.
    _lock: OnceCell<Option<std::fs::File>>,
    data_store: OnceCell<Retained<WKWebsiteDataStore>>,
    /// Shared by every tab: tracker blocker and the settings-page bridge.
    content: OnceCell<Retained<WKUserContentController>>,
    prefs: OnceCell<Retained<WKPreferences>>,
    #[allow(deprecated)]
    pool: OnceCell<Option<Retained<WKProcessPool>>>,
    extensions: OnceCell<Retained<WKWebExtensionController>>,
    /// Running extensions (started on first click, or at launch if "always").
    ext_contexts: RefCell<HashMap<i64, Retained<WKWebExtensionContext>>>,
    /// Every enabled extension's manifest and icons, read at launch.
    ext_meta: RefCell<HashMap<i64, Retained<WKWebExtension>>>,
    /// Their keyboard commands (the Extensions menu).
    ext_commands: RefCell<HashMap<i64, Vec<extensions::Command>>>,
    /// Hidden windows holding background pages that play audio (see
    /// `host_extension_audio`).
    ext_audio_windows: RefCell<HashMap<i64, Retained<NSWindow>>>,
    /// Requests the gate blocked this session (Settings → Network).
    net_blocked: RefCell<Vec<netgate::Blocked>>,
    /// Extension-facing window objects, one per native tab group (keyed by its pointer).
    ext_windows: RefCell<HashMap<usize, Retained<ExtWindow>>>,
    /// The shared content blockers, in `SHIELD_SETTINGS` order: trackers,
    /// the local-network shield, cookie banners, the user's own filters.
    shields: RefCell<[Option<Retained<WKContentRuleList>>; 4]>,
    /// `Version/<n> Safari/605.1.15`, so the browser blends in with Safari.
    user_agent: OnceCell<Option<String>>,
    tabs: RefCell<Vec<Retained<Tab>>>,
    active: RefCell<Option<Retained<Tab>>>,
    /// URLs of closed tabs, for Reopen Closed Tab.
    closed: RefCell<Vec<String>>,
    /// In-flight downloads: WKDownload pointer -> (downloads row, download).
    /// In-flight downloads, by WKDownload pointer.
    downloads: RefCell<HashMap<usize, Download>>,
    /// Cookies and site data of private windows: memory only, shared by
    /// every private window, dropped with the last private tab.
    private_store: RefCell<Option<Retained<WKWebsiteDataStore>>>,
    bench: Cell<bool>,
    on_top: Cell<bool>,
    /// `TUNE ...` lines printed in bench mode.
    report: RefCell<Vec<String>>,
    timer: OnceCell<Retained<NSTimer>>,
    /// Startup finished (the blockers are in place); tabs may open.
    ready: Cell<bool>,
    /// Links and files macOS asked us to open before then.
    pending_urls: RefCell<Vec<String>>,
    /// Saved passwords: the keychain, or memory for throwaway profiles.
    vault: OnceCell<passwords::Vault>,
    /// Every menu item's built-in shortcut, by `shortcuts` id, for Reset.
    default_keys: RefCell<HashMap<String, (String, NSEventModifierFlags)>>,
    /// The file Quick Look shows (a download).
    preview: RefCell<Option<Retained<NSURL>>>,
    /// The Web Store extension whose package the user asked to add.
    expected_package: RefCell<Option<String>>,
    /// The installed extension that package updates, and who to tell.
    pending_update: RefCell<Option<(i64, internal::Reply)>>,
    /// ⌘ + scroll zoom's event monitor.
    scroll_monitor: OnceCell<Option<Retained<AnyObject>>>,
    /// Translate Page's helper process and its requests (translate.rs).
    translate: RefCell<translate::State>,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[ivars = AppIvars]
    pub struct App;

    unsafe impl NSObjectProtocol for App {}

    /// The Bookmarks and History menus are filled when they open: no cost
    /// until then. The Window menu's pin and mute items follow the active tab.
    unsafe impl NSMenuDelegate for App {
        #[unsafe(method(menuNeedsUpdate:))]
        fn menu_needs_update(&self, menu: &NSMenu) {
            match menu.title().to_string().as_str() {
                "Window" => self.update_window_menu(menu),
                "View" => self.update_view_menu(menu),
                "Bookmarks" => self.fill_bookmarks_menu(menu),
                "History" => self.fill_history_menu(menu),
                "Profiles" => self.fill_profiles_menu(menu),
                "Take Screenshot" => self.update_screenshot_menu(menu),
                _ => {}
            }
        }
    }

    unsafe impl NSApplicationDelegate for App {
        #[unsafe(method(applicationDidFinishLaunching:))]
        fn did_finish_launching(&self, _n: &NSNotification) {
            self.start();
        }

        #[unsafe(method(applicationShouldTerminateAfterLastWindowClosed:))]
        fn should_terminate_after_last_window_closed(&self, _app: &NSApplication) -> bool {
            true
        }

        #[unsafe(method(applicationWillTerminate:))]
        fn will_terminate(&self, _n: &NSNotification) {
            self.save_session();
        }

        /// A link clicked in another app (we're the default browser) or an
        /// HTML file opened with us.
        #[unsafe(method(application:openURLs:))]
        fn open_urls(&self, _app: &NSApplication, urls: &NSArray<NSURL>) {
            let urls = urls.iter().filter_map(|u| u.absoluteString()).map(|u| u.to_string()).collect();
            self.open_external(urls);
        }
    }

    /// Serves the browser's own pages (mybrowser://settings).
    unsafe impl WKURLSchemeHandler for App {
        #[unsafe(method(webView:startURLSchemeTask:))]
        fn start_scheme_task(&self, _web: &WKWebView, task: &ProtocolObject<dyn WKURLSchemeTask>) {
            internal::serve(task);
        }

        #[unsafe(method(webView:stopURLSchemeTask:))]
        fn stop_scheme_task(&self, _web: &WKWebView, _task: &ProtocolObject<dyn WKURLSchemeTask>) {}
    }

    /// The settings page's bridge: `webkit.messageHandlers.browser.postMessage([op, ...args])`.
    unsafe impl WKScriptMessageHandlerWithReply for App {
        #[unsafe(method(userContentController:didReceiveScriptMessage:replyHandler:))]
        fn did_receive_message(
            &self,
            _c: &WKUserContentController,
            message: &WKScriptMessage,
            reply: &DynBlock<dyn Fn(*mut AnyObject, *mut NSString)>,
        ) {
            let reply = reply.copy();
            let respond = move |result: Result<String, String>| {
                let (ok, err) = match result {
                    Ok(json) => (Some(NSString::from_str(&json)), None),
                    Err(e) => (None, Some(NSString::from_str(&e))),
                };
                let ptr = |s: &Option<Retained<NSString>>| {
                    s.as_ref().map_or(std::ptr::null_mut(), |s| Retained::as_ptr(s) as *mut NSString)
                };
                reply.call((ptr(&ok) as *mut AnyObject, ptr(&err)));
            };
            // Web content can post to this handler too; only the browser's own
            // top-level pages may use it.
            if !internal::is_trusted(message) {
                return respond(Err("not allowed".into()));
            }
            match internal::message_args(message) {
                Some(args) => internal::handle(self, args, Box::new(respond)),
                None => respond(Err("expected an array of strings".into())),
            }
        }
    }

    /// resources/passwords.js: a sign-in, or focus on a login field. Only our
    /// own script world can post here; the page can't.
    unsafe impl WKScriptMessageHandler for App {
        #[unsafe(method(userContentController:didReceiveScriptMessage:))]
        fn did_receive_password_message(&self, _c: &WKUserContentController, message: &WKScriptMessage) {
            let frame = unsafe { message.frameInfo() };
            let origin = unsafe { frame.securityOrigin() };
            let page = format!("{}://{}/", unsafe { origin.protocol() }, unsafe { origin.host() });
            let Some(tab) = unsafe { message.webView() }.and_then(|w| self.tab_of_web(&w)) else { return };
            let body = unsafe { message.body() };
            let Some(body) = body.downcast_ref::<objc2_foundation::NSDictionary>() else { return };
            let get = |k: &str| body.objectForKey(&NSString::from_str(k));
            let text = |k: &str| get(k).and_then(|v| v.downcast::<NSString>().ok()).map(|s| s.to_string());
            let number = |k: &str| {
                get(k).and_then(|v| v.downcast::<objc2_foundation::NSNumber>().ok()).map_or(0.0, |n| n.doubleValue())
            };
            // Addresses fill on any page; cards only where passwords do.
            if text("op").as_deref() == Some("autofill") {
                let kind = text("kind").filter(|k| k == "address" || passwords::site_for(&page).is_some());
                return tab.offer_autofill(&kind.unwrap_or_default(), &frame, number("x"), number("y"));
            }
            // The frame's own site: a login form in a frame gets that site's accounts.
            let Some(site) = passwords::site_for(&page) else { return };
            match text("op").as_deref() {
                Some("focus") => tab.offer_passwords(&site, &frame, number("x"), number("y")),
                Some("submit") => {
                    let (user, password) = (text("username").unwrap_or_default(), text("password").unwrap_or_default());
                    if !password.is_empty() && password.len() < 4096 && user.len() < 1024 {
                        tab.offer_to_save(&site, &user, &password);
                    }
                }
                _ => {}
            }
        }
    }

    unsafe impl WKDownloadDelegate for App {
        #[unsafe(method(download:decideDestinationUsingResponse:suggestedFilename:completionHandler:))]
        fn decide_destination(
            &self,
            download: &WKDownload,
            _response: &NSURLResponse,
            suggested: &NSString,
            done: &DynBlock<dyn Fn(*mut NSURL)>,
        ) {
            let url = unsafe { download.originalRequest() }
                .and_then(|r| r.URL())
                .and_then(|u| u.absoluteString())
                .map(|s| s.to_string())
                .unwrap_or_default();
            // The package of a Web Store extension the user is adding: a
            // temporary file, installed when it arrives, not a download.
            // (Google redirects the download, so it's known by its file name,
            // `<ID>_<version>.crx`.)
            let name = suggested.to_string().to_ascii_lowercase();
            let package =
                self.ivars().expected_package.borrow().clone().filter(|id| name.ends_with(".crx") && name.contains(id));
            let path = match &package {
                Some(id) => {
                    let dir = extensions::unpack_root();
                    let _ = std::fs::create_dir_all(&dir);
                    // WebKit cancels a download whose file already exists.
                    unique_path(&dir, &format!("{id}.crx"))
                }
                None => unique_path(&self.downloads_dir(), &sanitize_filename(&suggested.to_string())),
            };
            // The file stays; a private window's download isn't listed.
            let private = unsafe { download.webView() }
                .is_some_and(|w| self.is_private_store(&*unsafe { w.configuration().websiteDataStore() }));
            let id = if private || package.is_some() { 0 } else { self.store().add_download(&url, &path.to_string_lossy()) };
            if let Some(entry) = self.ivars().downloads.borrow_mut().get_mut(&key(download)) {
                entry.0 = id;
                entry.2 = path.to_string_lossy().into_owned();
            }
            let dest = NSURL::fileURLWithPath(&NSString::from_str(&path.to_string_lossy()));
            done.call((Retained::as_ptr(&dest) as *mut NSURL,));
        }

        #[unsafe(method(downloadDidFinish:))]
        fn download_did_finish(&self, download: &WKDownload) {
            self.end_download(download, "done");
        }

        #[unsafe(method(download:didFailWithError:resumeData:))]
        fn download_did_fail(&self, download: &WKDownload, error: &NSError, _resume: Option<&NSData>) {
            // -999 is NSURLErrorCancelled.
            self.end_download(download, if error.code() == -999 { "cancelled" } else { "failed" });
        }
    }

    /// Lets extensions list windows and open tabs (`windows` / `tabs` APIs).
    unsafe impl WKWebExtensionControllerDelegate for App {
        #[unsafe(method(webExtensionController:openWindowsForExtensionContext:))]
        fn ext_open_windows(
            &self,
            _c: &WKWebExtensionController,
            _ctx: &WKWebExtensionContext,
        ) -> *mut NSArray<ProtocolObject<dyn WKWebExtensionWindow>> {
            let tabs = self.ivars().tabs.borrow().clone();
            let mut windows: Vec<Retained<ProtocolObject<dyn WKWebExtensionWindow>>> = Vec::new();
            let mut seen = Vec::new();
            for t in tabs {
                if let Some(w) = self.ext_window_for(&t) {
                    let p = Retained::as_ptr(&w) as usize;
                    if !seen.contains(&p) {
                        seen.push(p);
                        windows.push(ProtocolObject::from_retained(w));
                    }
                }
            }
            Retained::autorelease_return(NSArray::from_retained_slice(&windows))
        }

        #[unsafe(method(webExtensionController:focusedWindowForExtensionContext:))]
        fn ext_focused_window(
            &self,
            _c: &WKWebExtensionController,
            _ctx: &WKWebExtensionContext,
        ) -> *mut ProtocolObject<dyn WKWebExtensionWindow> {
            let w = self.active().and_then(|t| self.ext_window_for(&t));
            w.map_or(std::ptr::null_mut(), |w| Retained::autorelease_return(ProtocolObject::from_retained(w)))
        }

        #[unsafe(method(webExtensionController:openNewTabUsingConfiguration:forExtensionContext:completionHandler:))]
        fn ext_open_tab(
            &self,
            _c: &WKWebExtensionController,
            config: &WKWebExtensionTabConfiguration,
            _ctx: &WKWebExtensionContext,
            done: &DynBlock<dyn Fn(*mut ProtocolObject<dyn WKWebExtensionTab>, *mut NSError)>,
        ) {
            let url = unsafe { config.url() }.and_then(|u| u.absoluteString()).map(|s| s.to_string()).unwrap_or_default();
            if !tab::extension_may_load(&url) {
                let err = NSError::new(1, &NSString::from_str("my-browser"));
                return done.call((std::ptr::null_mut(), Retained::as_ptr(&err) as *mut _));
            }
            let tab = self.open_tab_in(false, &url, !unsafe { config.shouldBeActive() });
            let tab: Retained<ProtocolObject<dyn WKWebExtensionTab>> = ProtocolObject::from_retained(tab);
            done.call((Retained::as_ptr(&tab) as *mut _, std::ptr::null_mut()));
        }

        /// `runtime.sendNativeMessage`. The only app here is the offscreen
        /// stand-in asking for its background page to be able to play audio.
        #[unsafe(method(webExtensionController:sendMessage:toApplicationWithIdentifier:forExtensionContext:replyHandler:))]
        fn ext_native_message(
            &self,
            _c: &WKWebExtensionController,
            message: &AnyObject,
            app: Option<&NSString>,
            ctx: &WKWebExtensionContext,
            reply: &DynBlock<dyn Fn(*mut AnyObject, *mut NSError)>,
        ) {
            // `{ userAgent: string | undefined }`; anything else is ignored.
            let user_agent = message
                .downcast_ref::<objc2_foundation::NSDictionary>()
                .and_then(|m| m.objectForKey(&NSString::from_str("userAgent")))
                .and_then(|ua| ua.downcast::<NSString>().ok())
                .map(|ua| ua.to_string());
            if app.is_some_and(|a| a.to_string() == extensions::OFFSCREEN_APP)
                && self.host_extension_audio(ctx, user_agent.as_deref())
            {
                let ok = objc2_foundation::NSNumber::new_bool(true);
                return reply.call((Retained::as_ptr(&ok) as *mut AnyObject, std::ptr::null_mut()));
            }
            let err = NSError::new(1, &NSString::from_str("my-browser"));
            reply.call((std::ptr::null_mut(), Retained::as_ptr(&err) as *mut _));
        }

        /// `runtime.connectNative`: an extension page's WebSocket going
        /// through the network gate (netgate.rs).
        #[unsafe(method(webExtensionController:connectUsingMessagePort:forExtensionContext:completionHandler:))]
        fn ext_native_port(
            &self,
            _c: &WKWebExtensionController,
            port: &objc2_web_kit::WKWebExtensionMessagePort,
            ctx: &WKWebExtensionContext,
            done: &DynBlock<dyn Fn(*mut NSError)>,
        ) {
            if unsafe { port.applicationIdentifier() }.is_some_and(|a| a.to_string() == extensions::OFFSCREEN_APP) {
                self.net_connect(port, ctx);
                return done.call((std::ptr::null_mut(),));
            }
            let err = NSError::new(1, &NSString::from_str("my-browser"));
            done.call((Retained::as_ptr(&err) as *mut _,));
        }

        /// The extension's popup, under its toolbar button.
        #[unsafe(method(webExtensionController:presentPopupForAction:forExtensionContext:completionHandler:))]
        fn ext_present_popup(
            &self,
            _c: &WKWebExtensionController,
            action: &WKWebExtensionAction,
            ctx: &WKWebExtensionContext,
            done: &DynBlock<dyn Fn(*mut NSError)>,
        ) {
            let tab = unsafe { action.associatedTab() }.and_then(|t| self.tab_of(&t)).or_else(|| self.active());
            let (Some(popover), Some(tab)) = (unsafe { action.popupPopover() }, tab) else {
                let err = NSError::new(1, &NSString::from_str("my-browser"));
                return done.call((Retained::as_ptr(&err) as *mut _,));
            };
            let id = self.extension_id_of(ctx).unwrap_or(0);
            match tab.extension_button(id) {
                Some(b) => popover.showRelativeToRect_ofView_preferredEdge(b.bounds(), &b, objc2_foundation::NSRectEdge::MinY),
                None => {
                    let content = tab.window().contentView().unwrap();
                    let f = content.frame();
                    let corner = objc2_foundation::NSRect::new(NSPoint::new(f.size.width - 40.0, f.size.height - 30.0), objc2_foundation::NSSize::new(30.0, 28.0));
                    popover.showRelativeToRect_ofView_preferredEdge(corner, &content, objc2_foundation::NSRectEdge::MinY);
                }
            }
            if self.is_bench() {
                say!("POPUP {id} shown={}", popover.isShown() as u8);
            }
            done.call((std::ptr::null_mut(),));
        }

        /// Icon, title or badge changed: redraw the buttons.
        #[unsafe(method(webExtensionController:didUpdateAction:forExtensionContext:))]
        fn ext_action_updated(&self, _c: &WKWebExtensionController, _a: &WKWebExtensionAction, _ctx: &WKWebExtensionContext) {
            self.refresh_extension_buttons();
        }

        /// A running extension asks for more API permissions: ask the user.
        #[unsafe(method(webExtensionController:promptForPermissions:inTab:forExtensionContext:completionHandler:))]
        fn ext_prompt_permissions(
            &self,
            _c: &WKWebExtensionController,
            permissions: &objc2_foundation::NSSet<NSString>,
            tab: Option<&ProtocolObject<dyn WKWebExtensionTab>>,
            ctx: &WKWebExtensionContext,
            done: &DynBlock<dyn Fn(std::ptr::NonNull<objc2_foundation::NSSet<NSString>>, *mut objc2_foundation::NSDate)>,
        ) {
            let items: Vec<String> = permissions.iter().map(|p| p.to_string()).collect();
            let all = permissions.retain();
            let done = done.copy();
            self.prompt_extension(ctx, tab, &format!("use {}", items.join(", ")), move |ok| {
                let set = if ok { all.clone() } else { objc2_foundation::NSSet::new() };
                done.call((std::ptr::NonNull::from(&*set), std::ptr::null_mut()));
            });
        }

        /// A running extension asks to access sites: ask the user.
        #[unsafe(method(webExtensionController:promptForPermissionToAccessURLs:inTab:forExtensionContext:completionHandler:))]
        fn ext_prompt_urls(
            &self,
            _c: &WKWebExtensionController,
            urls: &objc2_foundation::NSSet<NSURL>,
            tab: Option<&ProtocolObject<dyn WKWebExtensionTab>>,
            ctx: &WKWebExtensionContext,
            done: &DynBlock<dyn Fn(std::ptr::NonNull<objc2_foundation::NSSet<NSURL>>, *mut objc2_foundation::NSDate)>,
        ) {
            let items: Vec<String> = urls.iter().filter_map(|u| u.host().map(|h| h.to_string())).collect();
            let all = urls.retain();
            let done = done.copy();
            self.prompt_extension(ctx, tab, &format!("read and change {}", items.join(", ")), move |ok| {
                let set = if ok { all.clone() } else { objc2_foundation::NSSet::new() };
                done.call((std::ptr::NonNull::from(&*set), std::ptr::null_mut()));
            });
        }

        #[unsafe(method(webExtensionController:promptForPermissionMatchPatterns:inTab:forExtensionContext:completionHandler:))]
        fn ext_prompt_patterns(
            &self,
            _c: &WKWebExtensionController,
            patterns: &objc2_foundation::NSSet<WKWebExtensionMatchPattern>,
            tab: Option<&ProtocolObject<dyn WKWebExtensionTab>>,
            ctx: &WKWebExtensionContext,
            done: &DynBlock<dyn Fn(std::ptr::NonNull<objc2_foundation::NSSet<WKWebExtensionMatchPattern>>, *mut objc2_foundation::NSDate)>,
        ) {
            let items: Vec<String> = patterns.iter().map(|p| unsafe { p.string() }.to_string()).collect();
            let all = patterns.retain();
            let done = done.copy();
            self.prompt_extension(ctx, tab, &format!("read and change {}", items.join(", ")), move |ok| {
                let set = if ok { all.clone() } else { objc2_foundation::NSSet::new() };
                done.call((std::ptr::NonNull::from(&*set), std::ptr::null_mut()));
            });
        }
    }

    impl App {
        // Quick Look's panel finds its controller in the responder chain;
        // the app delegate is the end of it.
        #[unsafe(method(acceptsPreviewPanelControl:))]
        fn accepts_preview(&self, _panel: Option<&AnyObject>) -> bool {
            true
        }

        #[unsafe(method(beginPreviewPanelControl:))]
        fn begin_preview(&self, panel: &AnyObject) {
            let _: () = unsafe { msg_send![panel, setDataSource: self] };
        }

        #[unsafe(method(endPreviewPanelControl:))]
        fn end_preview(&self, _panel: Option<&AnyObject>) {}

        #[unsafe(method(numberOfPreviewItemsInPreviewPanel:))]
        fn preview_count(&self, _panel: Option<&AnyObject>) -> isize {
            self.ivars().preview.borrow().is_some() as isize
        }

        /// NSURL is a QLPreviewItem.
        #[unsafe(method(previewPanel:previewItemAtIndex:))]
        fn preview_item(&self, _panel: Option<&AnyObject>, _i: isize) -> *mut NSURL {
            self.ivars().preview.borrow().as_ref().map_or(std::ptr::null_mut(), |u| Retained::as_ptr(u) as *mut NSURL)
        }

        /// Bench command from stdin, bounced to the main thread.
        #[unsafe(method(command:))]
        fn command_action(&self, cmd: &NSString) {
            self.command(&cmd.to_string());
        }

        // Menu actions. Tab-level ones go to the active tab explicitly rather
        // than through the responder chain, so they also work when the app
        // isn't frontmost.

        /// Quit (⌘Q). Open sheets close first, as cancelled: AppKit
        /// silently refuses to quit while one holding a text field (a page's
        /// prompt(), Search Tabs) is up.
        #[unsafe(method(quit:))]
        fn quit_action(&self, _s: Option<&AnyObject>) {
            self.quit();
        }

        #[unsafe(method(newTab:))]
        fn new_tab_action(&self, _s: Option<&AnyObject>) {
            self.new_tab();
        }

        /// The "+" button in the native tab bar.
        #[unsafe(method(newWindowForTab:))]
        fn new_window_for_tab(&self, _s: Option<&AnyObject>) {
            self.new_tab();
        }

        #[unsafe(method(newWindow:))]
        fn new_window_action(&self, _s: Option<&AnyObject>) {
            self.new_window(false);
        }

        #[unsafe(method(newPrivateWindow:))]
        fn new_private_window_action(&self, _s: Option<&AnyObject>) {
            self.new_window(true);
        }

        #[unsafe(method(reopenClosedTab:))]
        fn reopen_closed_tab_action(&self, _s: Option<&AnyObject>) {
            let last = self.ivars().closed.borrow_mut().pop();
            if let Some(url) = last {
                self.open_tab(&url, false);
            }
        }

        #[unsafe(method(closeTab:))]
        fn close_tab_action(&self, _s: Option<&AnyObject>) {
            if let Some(t) = self.active() {
                t.window().performClose(None);
            }
        }

        #[unsafe(method(openSettings:))]
        fn open_settings_action(&self, _s: Option<&AnyObject>) {
            self.open_internal("settings");
        }

        #[unsafe(method(showAbout:))]
        fn show_about_action(&self, _s: Option<&AnyObject>) {
            self.open_internal("settings#about");
        }

        #[unsafe(method(checkForUpdates:))]
        fn check_for_updates_action(&self, _s: Option<&AnyObject>) {
            self.open_tab(RELEASES, false);
        }

        #[unsafe(method(showHistory:))]
        fn show_history_action(&self, _s: Option<&AnyObject>) {
            self.open_internal("settings#history");
        }

        #[unsafe(method(showDownloads:))]
        fn show_downloads_action(&self, _s: Option<&AnyObject>) {
            self.open_internal("settings#downloads");
        }

        #[unsafe(method(duplicateTab:))]
        fn duplicate_tab_action(&self, _s: Option<&AnyObject>) {
            if let Some(t) = self.active() {
                self.open_tab_in(t.is_private(), &t.url(), false);
            }
        }

        #[unsafe(method(closeOtherTabs:))]
        fn close_other_tabs_action(&self, _s: Option<&AnyObject>) {
            for t in self.ordered_tabs().iter().filter(|t| !self.is_active(t) && !t.is_pinned()) {
                t.window().performClose(None);
            }
        }

        // Native tab-group actions, sent to the active tab's window (not the
        // key window: it may not be ours when the app is in the background).

        #[unsafe(method(moveTabToNewWindow:))]
        fn move_tab_to_new_window_action(&self, _s: Option<&AnyObject>) {
            self.with_active(|t| t.window().moveTabToNewWindow(None));
            self.sync_ext_windows();
        }

        #[unsafe(method(mergeAllWindows:))]
        fn merge_all_windows_action(&self, _s: Option<&AnyObject>) {
            self.with_active(|t| t.window().mergeAllWindows(None));
            self.sync_ext_windows();
        }

        /// Search Tabs (⇧⌘A).
        #[unsafe(method(searchTabs:))]
        fn search_tabs_action(&self, _s: Option<&AnyObject>) {
            self.with_active(|t| tabsearch::TabSearch::open(self, t, ""));
        }

        #[unsafe(method(pinTab:))]
        fn pin_tab_action(&self, _s: Option<&AnyObject>) {
            self.with_active(|t| t.set_pinned(!t.is_pinned(), false));
        }

        #[unsafe(method(muteTab:))]
        fn mute_tab_action(&self, _s: Option<&AnyObject>) {
            self.with_active(|t| t.set_muted(!t.is_muted()));
        }

        #[unsafe(method(toggleTabOverview:))]
        fn toggle_tab_overview_action(&self, _s: Option<&AnyObject>) {
            self.with_active(|t| t.window().toggleTabOverview(None));
        }

        /// AutoFill Password (⌥⌘A): saved accounts for the focused field.
        #[unsafe(method(offerPasswords:))]
        fn offer_passwords_action(&self, _s: Option<&AnyObject>) {
            self.with_active(|t| t.run_password_script("__mybOffer()"));
        }

        /// Reader mode (⇧⌘R).
        #[unsafe(method(toggleReader:))]
        fn toggle_reader_action(&self, _s: Option<&AnyObject>) {
            self.with_active(|t| t.toggle_reader());
        }

        /// Translate Page, or Show Original (View menu).
        #[unsafe(method(translatePage:))]
        fn translate_page_action(&self, _s: Option<&AnyObject>) {
            self.with_active(|t| t.toggle_translate());
        }

        /// View → Show Web Inspector (⌥⌘I), unless developer tools are off.
        #[unsafe(method(showInspector:))]
        fn show_inspector_action(&self, _s: Option<&AnyObject>) {
            self.with_active(|t| t.show_inspector());
        }

        /// An extension's keyboard command (Extensions menu; tag = id * 100 + index).
        #[unsafe(method(extensionCommand:))]
        fn extension_command_action(&self, sender: &NSMenuItem) {
            self.run_extension_command(sender.tag() as i64);
        }

        /// File → Profiles → a profile (its name is the item's title).
        #[unsafe(method(openProfile:))]
        fn open_profile_action(&self, sender: &NSMenuItem) {
            if let Err(e) = self.open_profile(&sender.title().to_string()) {
                self.with_active(|t| t.confirm(&e, &["OK"], |_| {}));
            }
        }

        #[unsafe(method(manageProfiles:))]
        fn manage_profiles_action(&self, _s: Option<&AnyObject>) {
            self.open_internal("settings#profiles");
        }

        #[unsafe(method(showExtensions:))]
        fn show_extensions_action(&self, _s: Option<&AnyObject>) {
            self.open_internal("settings#extensions");
        }

        /// Hide Element on Page… (View menu, site menu).
        #[unsafe(method(pickElement:))]
        fn pick_element_action(&self, _s: Option<&AnyObject>) {
            self.with_active(|t| t.pick_element());
        }

        /// Picture in picture for the page's video (⌥⌘P).
        #[unsafe(method(pictureInPicture:))]
        fn picture_in_picture_action(&self, _s: Option<&AnyObject>) {
            self.with_active(|t| t.picture_in_picture());
        }

        #[unsafe(method(savePage:))]
        fn save_page_action(&self, _s: Option<&AnyObject>) {
            self.with_active(|t| t.save_page(None));
        }

        #[unsafe(method(screenshotArea:))]
        fn screenshot_area_action(&self, _s: Option<&AnyObject>) {
            self.with_active(|t| t.screenshot(screenshot::Shot::Area, None, None));
        }

        #[unsafe(method(screenshotVisible:))]
        fn screenshot_visible_action(&self, _s: Option<&AnyObject>) {
            self.with_active(|t| t.screenshot(screenshot::Shot::Visible, None, None));
        }

        #[unsafe(method(screenshotFullPage:))]
        fn screenshot_full_page_action(&self, _s: Option<&AnyObject>) {
            self.with_active(|t| t.screenshot(screenshot::Shot::Full, None, None));
        }

        /// Take Screenshot → Save to File / Copy to Clipboard (remembered).
        #[unsafe(method(screenshotTo:))]
        fn screenshot_to_action(&self, sender: &NSMenuItem) {
            let _ = self.store().set_setting("screenshot_to", if sender.tag() == 1 { "clipboard" } else { "file" });
        }

        /// Take Screenshot → Record Screen…: macOS's own Screenshot app,
        /// which records the whole screen or a part of it.
        #[unsafe(method(recordScreen:))]
        fn record_screen_action(&self, _s: Option<&AnyObject>) {
            let app = NSURL::fileURLWithPath(&NSString::from_str("/System/Applications/Utilities/Screenshot.app"));
            objc2_app_kit::NSWorkspace::sharedWorkspace().openURL(&app);
        }

        #[unsafe(method(printPage:))]
        fn print_action(&self, _s: Option<&AnyObject>) {
            self.with_active(|t| t.print(None));
        }

        #[unsafe(method(toggleBookmark:))]
        fn toggle_bookmark_action(&self, _s: Option<&AnyObject>) {
            self.with_active(|t| t.toggle_bookmark());
        }

        #[unsafe(method(showBookmarks:))]
        fn show_bookmarks_action(&self, _s: Option<&AnyObject>) {
            self.open_internal("settings#bookmarks");
        }

        /// A page in the Bookmarks or History menu (its address is the
        /// item's represented object): opens in this tab, or a new one with ⌘.
        #[unsafe(method(openMenuURL:))]
        fn open_menu_url_action(&self, sender: &NSMenuItem) {
            let Some(url) = sender.representedObject().and_then(|o| o.downcast::<NSString>().ok()) else { return };
            let url = url.to_string();
            let cmd = NSApplication::sharedApplication(self.mtm())
                .currentEvent()
                .is_some_and(|e| e.modifierFlags().contains(NSEventModifierFlags::Command));
            match self.active() {
                Some(t) if !cmd => t.load(&url),
                _ => {
                    self.open_tab(&url, false);
                }
            }
        }

        #[unsafe(method(goBack:))]
        fn go_back_action(&self, _s: Option<&AnyObject>) {
            self.with_active(|t| t.go_back());
        }

        #[unsafe(method(goForward:))]
        fn go_forward_action(&self, _s: Option<&AnyObject>) {
            self.with_active(|t| t.go_forward());
        }

        #[unsafe(method(reload:))]
        fn reload_action(&self, _s: Option<&AnyObject>) {
            self.with_active(|t| t.reload());
        }

        #[unsafe(method(stop:))]
        fn stop_action(&self, _s: Option<&AnyObject>) {
            self.with_active(|t| t.stop());
        }

        #[unsafe(method(zoomIn:))]
        fn zoom_in_action(&self, _s: Option<&AnyObject>) {
            self.with_active(|t| t.zoom(Some(1)));
        }

        #[unsafe(method(zoomOut:))]
        fn zoom_out_action(&self, _s: Option<&AnyObject>) {
            self.with_active(|t| t.zoom(Some(-1)));
        }

        #[unsafe(method(actualSize:))]
        fn actual_size_action(&self, _s: Option<&AnyObject>) {
            self.with_active(|t| t.zoom(None));
        }

        #[unsafe(method(focusAddressBar:))]
        fn focus_address_bar_action(&self, _s: Option<&AnyObject>) {
            self.with_active(|t| t.focus_address_bar());
        }

        #[unsafe(method(showFind:))]
        fn show_find_action(&self, _s: Option<&AnyObject>) {
            self.with_active(|t| t.show_find());
        }

        #[unsafe(method(findNext:))]
        fn find_next_action(&self, _s: Option<&AnyObject>) {
            self.with_active(|t| self.find_in(t, false));
        }

        #[unsafe(method(findPrevious:))]
        fn find_previous_action(&self, _s: Option<&AnyObject>) {
            self.with_active(|t| self.find_in(t, true));
        }

        #[unsafe(method(minimizeWindow:))]
        fn minimize_action(&self, _s: Option<&AnyObject>) {
            self.with_active(|t| t.window().miniaturize(None));
        }

        #[unsafe(method(zoomWindow:))]
        fn zoom_window_action(&self, _s: Option<&AnyObject>) {
            self.with_active(|t| t.window().zoom(None));
        }

        #[unsafe(method(nextTab:))]
        fn next_tab_action(&self, _s: Option<&AnyObject>) {
            self.select_relative(1);
        }

        #[unsafe(method(previousTab:))]
        fn previous_tab_action(&self, _s: Option<&AnyObject>) {
            self.select_relative(-1);
        }

        /// Cmd+1..8 select that tab, Cmd+9 the last one (menu item tag).
        #[unsafe(method(selectTabByNumber:))]
        fn select_tab_by_number(&self, sender: Option<&NSMenuItem>) {
            let n = sender.map_or(1, |s| s.tag());
            let count = self.ordered_tabs().len() as isize;
            if count > 0 {
                self.select_index(if n == 9 { count - 1 } else { (n - 1).min(count - 1) } as usize);
            }
        }
    }
);

impl App {
    fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(AppIvars::default());
        unsafe { msg_send![super(this), init] }
    }

    fn quit(&self) {
        for t in self.ivars().tabs.borrow().clone() {
            if let Some(sheet) = t.window().attachedSheet() {
                t.window().endSheet_returnCode(&sheet, 1001);
            }
        }
        NSApplication::sharedApplication(self.mtm()).terminate(None);
    }

    pub fn store(&self) -> &Store {
        self.ivars().store.get().unwrap()
    }

    pub fn default_keys(&self) -> &RefCell<HashMap<String, (String, NSEventModifierFlags)>> {
        &self.ivars().default_keys
    }

    pub fn is_bench(&self) -> bool {
        self.ivars().bench.get()
    }

    pub fn is_on_top(&self) -> bool {
        self.ivars().on_top.get()
    }

    pub fn profile_dir(&self) -> Option<&Path> {
        self.ivars().profile.get().and_then(|p| p.as_deref())
    }

    fn active(&self) -> Option<Retained<Tab>> {
        self.ivars().active.borrow().clone()
    }

    fn with_active(&self, f: impl FnOnce(&Tab)) {
        if let Some(t) = self.active() {
            f(&t);
        }
    }

    pub fn is_active(&self, tab: &Tab) -> bool {
        self.ivars().active.borrow().as_deref().is_some_and(|a| std::ptr::eq(a, tab))
    }

    /// Open the profile, set up the shared engine pieces, then open tabs once
    /// the tracker blocker is ready (so no page loads unprotected).
    fn start(&self) {
        let mtm = self.mtm();
        let profile = match arg_value("--profile") {
            Some(dir) => Some(PathBuf::from(dir)),
            None if self.is_bench() => None,
            None => std::env::var_os("HOME").map(|h| PathBuf::from(h).join("Library/Application Support/my-browser")),
        };
        crash::install(profile.as_ref().map(|p| p.join("crashes")));
        let lock = profile.as_ref().map(|dir| lock_profile(dir).unwrap_or_else(|e| fatal(mtm, &e)));
        let db = profile.as_ref().map(|d| d.join("browser.db"));
        let store = Store::open(db.as_deref()).unwrap_or_else(|e| fatal(mtm, &format!("Can't open the profile: {e}")));
        store.fail_stale_downloads();
        let _ = self.ivars()._lock.set(lock);
        let _ = self.ivars().profile.set(profile.clone());
        let _ = self.ivars().store.set(store);
        self.apply_theme();
        shortcuts::apply_saved(self);
        // Which profile this copy of the browser is: on its Dock icon.
        if let Some(name) = self.profile_name() {
            NSApplication::sharedApplication(mtm).dockTile().setBadgeLabel(Some(&NSString::from_str(&name)));
        }

        // Cookies and site data: WebKit's store for this profile, or memory only.
        let data_store = match &profile {
            Some(_) => unsafe { WKWebsiteDataStore::dataStoreForIdentifier(&self.profile_uuid(), mtm) },
            None => unsafe { WKWebsiteDataStore::nonPersistentDataStore(mtm) },
        };
        let line = tune_line;
        let mut report: Vec<String> =
            privacy::harden_data_store(&data_store).into_iter().map(|(n, ok)| line(n, true, ok)).collect();
        let prefs = unsafe { WKPreferences::new(mtm) };
        for (n, ok) in privacy::harden_preferences(&prefs, self.store().flag("safe_browsing")) {
            report.push(line(n, true, ok));
        }
        // Media: element fullscreen (public, but off in WKWebView, so video
        // sites' fullscreen buttons did nothing), picture in picture and
        // screen sharing (private).
        let spec = std::env::var("MYB_TUNE").ok();
        let tune = |name, default| tune_value(spec.as_deref(), name, default);
        unsafe { prefs.setElementFullscreenEnabled(tune("fullscreen", true)) };
        for (name, setter) in [
            ("picture_in_picture", sel!(_setAllowsPictureInPictureMediaPlayback:)),
            ("screen_capture", sel!(_setScreenCaptureEnabled:)),
        ] {
            let (on, ok) = (tune(name, true), responds(&prefs, setter));
            if ok {
                let _: () = unsafe { objc2::runtime::MessageReceiver::send_message(&*prefs, setter, (Bool::new(on),)) };
            }
            report.push(line(name, on, ok));
        }
        for (n, ok) in privacy::harden_webpage_preferences(&*unsafe { WKWebpagePreferences::new(mtm) }, true) {
            report.push(line(n, true, ok));
        }
        // Per-tab mute and the sound badge.
        report.push(line("tab_mute", true, WKWebView::class().responds_to(sel!(_setPageMuted:))));
        report.push(line("playing_audio", true, WKWebView::class().responds_to(sel!(_isPlayingAudio))));
        // Settings → Tabs and the heavy-tab badge read each page's process.
        report.push(line("web_process_id", true, WKWebView::class().responds_to(sel!(_webProcessIdentifier))));
        // The PDF viewer gets the PDF WebKit loaded.
        report.push(line(
            "pdf_data",
            true,
            WKWebView::class().responds_to(sel!(_getMainResourceDataWithCompletionHandler:)),
        ));
        // The address bar shows the committed address (anti-spoofing).
        report.push(line("committed_url", true, WKWebView::class().responds_to(sel!(_committedURL))));
        // Extensions' background pages can play audio (see `host_extension_audio`).
        report.push(line(
            "ext_background_view",
            true,
            WKWebExtensionContext::class().responds_to(sel!(_backgroundWebView)),
        ));
        // Web Inspector (Settings → General → Developer tools).
        let devtools = responds(&prefs, sel!(_setDeveloperExtrasEnabled:));
        report.push(line("developer_extras", true, devtools));
        report.push(line("web_inspector", true, WKWebView::class().responds_to(sel!(_inspector))));
        let (pool, tuning) = engine_tuning(&prefs, self.store().flag("jit"), mtm);
        report.extend(tuning.into_iter().map(|(n, v, ok)| line(n, v, ok)));

        let content = unsafe { WKUserContentController::new(mtm) };
        unsafe {
            content.addScriptMessageHandlerWithReply_contentWorld_name(
                ProtocolObject::from_ref(self),
                &WKContentWorld::pageWorld(mtm),
                &NSString::from_str("browser"),
            )
        };
        // Saved passwords: resources/passwords.js in its own world, in every
        // frame (sign-in forms are often embedded), reporting to the
        // "passwords" handler there.
        if tune_value(std::env::var("MYB_TUNE").ok().as_deref(), "passwords_script", true) {
            let world = tab::passwords_world(mtm);
            unsafe {
                let script = WKUserScript::initWithSource_injectionTime_forMainFrameOnly_inContentWorld(
                    WKUserScript::alloc(mtm),
                    &NSString::from_str(PASSWORDS_JS),
                    WKUserScriptInjectionTime::AtDocumentEnd,
                    false,
                    &world,
                );
                content.addUserScript(&script);
                content.addScriptMessageHandler_contentWorld_name(
                    ProtocolObject::from_ref(self),
                    &world,
                    &NSString::from_str("passwords"),
                );
            }
        }
        let vault = match &profile {
            Some(_) => passwords::Vault::Keychain,
            None => passwords::Vault::Memory(RefCell::new(Vec::new())),
        };
        let _ = self.ivars().vault.set(vault);
        let ext_config = match &profile {
            Some(_) => unsafe {
                WKWebExtensionControllerConfiguration::configurationWithIdentifier(&self.profile_uuid(), mtm)
            },
            None => unsafe { WKWebExtensionControllerConfiguration::nonPersistentConfiguration(mtm) },
        };
        let extensions = unsafe {
            WKWebExtensionController::initWithConfiguration(WKWebExtensionController::alloc(mtm), &ext_config)
        };
        unsafe { extensions.setDelegate(Some(ProtocolObject::from_ref(self))) };
        let _ = self.ivars().data_store.set(data_store);
        let _ = self.ivars().prefs.set(prefs);
        self.apply_devtools();
        let _ = self.ivars().pool.set(pool);
        let _ = self.ivars().content.set(content);
        let _ = self.ivars().extensions.set(extensions);
        self.ivars().report.replace(report);

        self.start_extensions();
        self.watch_scroll_zoom();

        self.load_shields(move |this, [trackers, local, banners, filters]| {
            let mut report = this.ivars().report.borrow_mut();
            report.push(line("trackers", true, trackers));
            report.push(line("local_network_shield", true, local));
            report.push(line("cookie_banners", true, banners));
            report.push(line("my_filters", true, filters));
            drop(report);
            this.ready();
        });
        self.start_timer();
    }

    /// Startup finished: restore the session, open the first tab.
    fn ready(&self) {
        let restore = self.profile_dir().is_some() && self.store().flag("restore_session");
        let session = if restore { self.store().session() } else { Vec::new() };
        let mut urls = url_args();
        urls.extend(self.ivars().pending_urls.take());
        self.ivars().ready.set(true);
        // Restored tabs open asleep (no page process until clicked), each
        // window's tabs together.
        let mut prev: Option<(i64, Retained<Tab>)> = None;
        for (win, url, title, pinned) in &session {
            let tab = Tab::create(self, None, url, title, true, false);
            let next_to = prev.as_ref().filter(|(w, _)| w == win).map(|(_, t)| &**t);
            self.add_tab(&tab, next_to, false);
            if *pinned {
                tab.set_pinned(true, true);
            }
            prev = Some((*win, tab));
        }
        for u in &urls {
            let url = self.normalize(u);
            self.open_tab_in(false, &url, false);
        }
        if session.is_empty() && urls.is_empty() {
            self.new_tab();
        } else if urls.is_empty() {
            // Back to the tab that was open; only it wakes.
            let i: usize = self.store().get("session_active").and_then(|s| s.parse().ok()).unwrap_or(0);
            let tabs = self.ivars().tabs.borrow().clone();
            if let Some(t) = tabs.get(i).or(tabs.first()) {
                self.select(t);
            }
        }
        // Private UI-delegate methods we implement: WebKit must still declare
        // them, or the feature would quietly stop working.
        for (name, method) in [
            ("context_menu", sel!(_webView:getContextMenuFromProposedMenu:forElement:userInfo:completionHandler:)),
            ("page_print", sel!(_webView:printFrame:pdfFirstPageSize:completionHandler:)),
            (
                "display_capture",
                sel!(_webView:requestDisplayCapturePermissionForOrigin:initiatedByFrame:withSystemAudio:decisionHandler:),
            ),
        ] {
            self.ivars().report.borrow_mut().push(tune_line(name, true, webkit_declares(method)));
        }
        if self.is_bench() {
            for line in self.ivars().report.borrow().iter() {
                say!("{line}");
            }
            self.read_commands();
            say!("READY");
        }
        self.offer_crash_report();
    }

    /// The browser crashed since the last launch: offer to report it. Asks
    /// once per crash; nothing is sent unless the user submits the issue.
    fn offer_crash_report(&self) {
        let store = self.store();
        let since = store.get("crash_seen_ms").and_then(|s| s.parse().ok());
        store.put("crash_seen_ms", &crash::now_ms().to_string());
        // First launch: older reports are from before this profile.
        let Some(since) = since else { return };
        let crashes = self.profile_dir().map(|p| p.join("crashes"));
        // macOS's own reports are shared by every profile; tests must not see the owner's.
        let system = (!self.is_bench())
            .then(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join("Library/Logs/DiagnosticReports")))
            .flatten();
        let Some(report) = crash::reports_since(crashes.as_deref(), system.as_deref(), since).pop() else { return };
        let Some(tab) = self.active() else { return };
        let this = self.retain();
        tab.confirm(
            "my-browser quit unexpectedly. Report the crash?\n\nThe report holds the browser and macOS versions and \
             where in the code it stopped: no pages, history or personal data. It opens as a GitHub issue you can \
             read before sending, and nothing is sent otherwise.",
            &["Report…", "Don't Send"],
            move |yes| {
                if yes {
                    this.open_tab_in(false, &crash::issue_url(&crash::summary(&report)), false);
                    let file = NSURL::fileURLWithPath(&NSString::from_str(&report.to_string_lossy()));
                    objc2_app_kit::NSWorkspace::sharedWorkspace()
                        .activateFileViewerSelectingURLs(&NSArray::from_retained_slice(&[file]));
                }
            },
        );
    }

    /// Open links and files from other apps in new tabs: web pages and local
    /// files only (other schemes belong to other apps). Held until startup
    /// finishes, so nothing loads before the tracker blocker is ready.
    fn open_external(&self, urls: Vec<String>) {
        let urls: Vec<String> = urls.into_iter().filter(|u| opens_externally(u)).collect();
        if !self.ivars().ready.get() {
            self.ivars().pending_urls.borrow_mut().extend(urls);
            return;
        }
        for url in &urls {
            self.open_tab_in(false, url, false);
        }
        // Test browsers never take focus from the app the owner is using.
        if !urls.is_empty() && !self.is_bench() {
            #[allow(deprecated)]
            NSApplication::sharedApplication(self.mtm()).activateIgnoringOtherApps(true);
        }
    }

    fn profile_uuid(&self) -> Retained<NSUUID> {
        let store = self.store();
        let existing =
            store.get("profile_id").and_then(|s| NSUUID::initWithUUIDString(NSUUID::alloc(), &NSString::from_str(&s)));
        existing.unwrap_or_else(|| {
            let id = NSUUID::UUID();
            store.put("profile_id", &id.UUIDString().to_string());
            id
        })
    }

    /// A configuration for a new web view, sharing the profile's engine
    /// pieces. Private ones get the private store and no extensions.
    pub fn make_config(&self, private: bool) -> Retained<WKWebViewConfiguration> {
        let iv = self.ivars();
        let config = unsafe { WKWebViewConfiguration::new(self.mtm()) };
        unsafe {
            match private {
                true => config.setWebsiteDataStore(&self.private_store()),
                false => config.setWebsiteDataStore(iv.data_store.get().unwrap()),
            }
            config.setUserContentController(iv.content.get().unwrap());
            config.setPreferences(iv.prefs.get().unwrap());
            // No autoplay: media only plays after a click.
            config.setMediaTypesRequiringUserActionForPlayback(WKAudiovisualMediaTypes::All);
            config
                .setURLSchemeHandler_forURLScheme(Some(ProtocolObject::from_ref(self)), &NSString::from_str(INTERNAL));
            if !private {
                config.setWebExtensionController(Some(iv.extensions.get().unwrap()));
            }
            let page = WKWebpagePreferences::new(self.mtm());
            privacy::harden_webpage_preferences(&page, true);
            config.setDefaultWebpagePreferences(Some(&page));
            if let Some(ua) = self.user_agent() {
                config.setApplicationNameForUserAgent(Some(&NSString::from_str(ua)));
            }
            if let Some(pool) = iv.pool.get().unwrap() {
                #[allow(deprecated)]
                config.setProcessPool(pool);
            }
        }
        config
    }

    /// Compile (or look up) the tracker blocker, the local-network shield,
    /// the cookie-banner hider and the user's own filters with the current
    /// per-site exceptions, and put them on every tab.
    /// `done(app, [trackers_ok, local_ok, banners_ok, filters_ok])`.
    pub fn load_shields(&self, done: impl FnOnce(&App, [bool; 4]) + 'static) {
        let mtm = self.mtm();
        let store = self.store();
        let exempt = store.sites_with("shields", "off");
        let allowed = store.sites_with("local_network", "allow");
        let filters = store.get("filters").unwrap_or_default();
        let this = self.retain();
        privacy::load_tracker_rules(mtm, exempt.clone(), move |trackers| {
            privacy::load_local_rules(mtm, allowed, move |local| {
                privacy::load_banner_rules(mtm, exempt.clone(), move |banners| {
                    privacy::load_filter_rules(mtm, filters, exempt, move |mine| {
                        let ok = [trackers.is_some(), local.is_some(), banners.is_some(), mine.is_some()];
                        this.set_rule_lists([trackers, local, banners, mine]);
                        done(&this, ok);
                    });
                });
            });
        });
    }

    fn set_rule_lists(&self, lists: [Option<Retained<WKContentRuleList>>; 4]) {
        let content = self.ivars().content.get().unwrap();
        let mut slots = self.ivars().shields.borrow_mut();
        for (slot, new) in slots.iter_mut().zip(lists) {
            // Keep an old list if the new one failed to compile: fail closed.
            if let Some(new) = new
                && let Some(old) = slot.replace(new)
            {
                unsafe { content.removeContentRuleList(&old) };
            }
        }
        drop(slots);
        self.apply_trackers();
    }

    /// Add or remove the shared blockers on every tab, following the settings.
    pub fn apply_trackers(&self) {
        let content = self.ivars().content.get().unwrap();
        let lists = self.ivars().shields.borrow().clone();
        for (list, setting) in lists.into_iter().zip(SHIELD_SETTINGS) {
            let on = setting.is_empty() || self.store().flag(setting);
            if let Some(list) = list {
                unsafe {
                    content.removeContentRuleList(&list);
                    if on {
                        content.addContentRuleList(&list);
                    }
                }
            }
        }
    }

    /// Replace the user's own filters (Settings → Privacy → My filters) and
    /// put them to work. `done` gets the lines that aren't understood, or why
    /// WebKit refused them (they're then not kept).
    pub fn set_filters(&self, text: &str, done: impl FnOnce(Result<Vec<String>, String>) + 'static) {
        if text.len() > 1 << 20 {
            return done(Err("That's too many filters (1 MB at most).".into()));
        }
        let before = self.store().get("filters").unwrap_or_default();
        self.store().put("filters", text);
        let unknown = privacy::filter_rules(text, &[]).1;
        self.load_shields(move |this, ok| {
            if ok[3] {
                return done(Ok(unknown));
            }
            // The list in use stays as it was (fail closed); so do the saved filters.
            this.store().put("filters", &before);
            done(Err("WebKit couldn't use these filters (a selector may be invalid), so they weren't saved.".into()))
        });
    }

    /// Add one filter line (Hide Element…), unless it's there already.
    pub fn add_filter(&self, line: &str, done: impl FnOnce(Result<Vec<String>, String>) + 'static) {
        let text = self.store().get("filters").unwrap_or_default();
        if text.lines().any(|l| l.trim() == line) {
            return done(Ok(Vec::new()));
        }
        let sep = if text.is_empty() || text.ends_with('\n') { "" } else { "\n" };
        self.set_filters(&format!("{text}{sep}{line}\n"), done);
    }

    /// Whether this address gets the full protections: false where the user
    /// turned Shields down for the site (or a parent domain of it).
    pub fn shields_up(&self, url: &str) -> bool {
        let Some(host) = store::site_of(url) else { return true };
        !self.store().sites_with("shields", "off").iter().any(|s| host == *s || host.ends_with(&format!(".{s}")))
    }

    /// Whether the site's own scripts may run: false where the user turned
    /// JavaScript off for it.
    pub fn javascript_allowed(&self, url: &str) -> bool {
        store::site_of(url).is_none_or(|site| self.store().permission(&site, "javascript").as_deref() != Some("deny"))
    }

    /// Safari's own user-agent suffix: sites treat the browser like Safari
    /// (same engine), and it doesn't stand out as a rare user agent.
    fn user_agent(&self) -> Option<&str> {
        self.ivars()
            .user_agent
            .get_or_init(|| {
                let safari =
                    objc2_foundation::NSBundle::bundleWithPath(&NSString::from_str("/Applications/Safari.app"))?;
                let version = safari.objectForInfoDictionaryKey(&NSString::from_str("CFBundleShortVersionString"))?;
                let version = version.downcast_ref::<NSString>()?.to_string();
                version
                    .chars()
                    .all(|c| c.is_ascii_digit() || c == '.')
                    .then(|| format!("Version/{version} Safari/605.1.15"))
            })
            .as_deref()
    }

    /// The browser's look. Web views follow it, so pages see it as
    /// `prefers-color-scheme`.
    pub fn apply_theme(&self) {
        let name = match self.store().setting("theme").as_str() {
            "light" => Some(unsafe { objc2_app_kit::NSAppearanceNameAqua }),
            "dark" => Some(unsafe { objc2_app_kit::NSAppearanceNameDarkAqua }),
            _ => None,
        };
        let appearance = name.and_then(objc2_app_kit::NSAppearance::appearanceNamed);
        NSApplication::sharedApplication(self.mtm()).setAppearance(appearance.as_deref());
    }

    /// Developer tools on or off: the page menu's Inspect Element (private
    /// `_setDeveloperExtrasEnabled:`) and every web view's `isInspectable`.
    pub fn apply_devtools(&self) {
        let on = self.store().flag("devtools");
        let prefs = self.ivars().prefs.get().unwrap();
        if responds(prefs, sel!(_setDeveloperExtrasEnabled:)) {
            let _: () = unsafe { msg_send![&**prefs, _setDeveloperExtrasEnabled: Bool::new(on)] };
        }
        for t in self.ivars().tabs.borrow().iter() {
            if let Some(web) = t.web() {
                unsafe { web.setInspectable(on) };
            }
        }
    }

    /// Settings → General → Default zoom.
    pub fn default_zoom(&self) -> f64 {
        self.store().setting("default_zoom").parse::<f64>().map_or(1.0, |p| p / 100.0)
    }

    /// Gestures and the default zoom changed: every open page follows.
    pub fn apply_gestures(&self) {
        for t in self.ivars().tabs.borrow().iter() {
            t.apply_gestures();
            if let Some(web) = t.web() {
                let saved = store::site_of(&t.url()).and_then(|s| self.store().zoom(&s));
                unsafe { web.setPageZoom(saved.unwrap_or(self.default_zoom())) };
            }
            t.refresh_zoom_button();
        }
    }

    /// ⌘ + scroll zooms the page under the pointer (Settings → General). A
    /// trackpad's small steps add up to one zoom step.
    fn watch_scroll_zoom(&self) {
        let ptr = self as *const App as usize;
        let total = Cell::new(0.0f64);
        let block = RcBlock::new(move |e: std::ptr::NonNull<NSEvent>| -> *mut NSEvent {
            // The delegate lives until the process exits.
            let app = unsafe { &*(ptr as *const App) };
            let event = unsafe { e.as_ref() };
            if !event.modifierFlags().contains(NSEventModifierFlags::Command) || !app.store().flag("scroll_zoom") {
                return e.as_ptr();
            }
            let Some(tab) = event.window(app.mtm()).and_then(|w| app.tab_for_window(&w)) else { return e.as_ptr() };
            let step = if event.hasPreciseScrollingDeltas() { 40.0 } else { 1.0 };
            total.set(total.get() + event.scrollingDeltaY());
            if total.get().abs() >= step {
                tab.zoom(Some(if total.get() > 0.0 { 1 } else { -1 }));
                total.set(0.0);
            }
            std::ptr::null_mut()
        });
        let monitor = unsafe {
            NSEvent::addLocalMonitorForEventsMatchingMask_handler(objc2_app_kit::NSEventMask::ScrollWheel, &block)
        };
        let _ = self.ivars().scroll_monitor.set(monitor);
    }

    pub fn apply_safe_browsing(&self) {
        let prefs = self.ivars().prefs.get().unwrap();
        unsafe { prefs.setFraudulentWebsiteWarningEnabled(self.store().flag("safe_browsing")) };
    }

    pub fn data_store(&self) -> &WKWebsiteDataStore {
        self.ivars().data_store.get().unwrap()
    }

    /// The private windows' store, made on first use with the same
    /// protections as the profile's.
    fn private_store(&self) -> Retained<WKWebsiteDataStore> {
        let mut slot = self.ivars().private_store.borrow_mut();
        slot.get_or_insert_with(|| {
            let store = unsafe { WKWebsiteDataStore::nonPersistentDataStore(self.mtm()) };
            privacy::harden_data_store(&store);
            store
        })
        .clone()
    }

    fn is_private_store(&self, store: &WKWebsiteDataStore) -> bool {
        self.ivars().private_store.borrow().as_deref().is_some_and(|s| std::ptr::eq(s, store))
    }

    // ---- tabs

    fn new_tab(&self) {
        let home = self.store().setting("homepage");
        let tab = self.open_tab(&self.new_tab_url(), false);
        if home.is_empty() {
            tab.focus_address_bar();
        }
    }

    /// ⌘N, or ⇧⌘N for a private window.
    fn new_window(&self, private: bool) {
        self.open_window(private, &self.new_tab_url()).focus_address_bar();
    }

    /// A new window with `url` in its one tab.
    pub fn open_window(&self, private: bool, url: &str) -> Retained<Tab> {
        let tab = Tab::create(self, None, url, "", false, private);
        self.add_tab(&tab, None, true);
        tab
    }

    /// The chosen homepage; empty means the built-in (native) new tab page.
    fn new_tab_url(&self) -> String {
        self.store().setting("homepage")
    }

    /// Open `url` in a new tab next to the active one (private if it is).
    pub fn open_tab(&self, url: &str, background: bool) -> Retained<Tab> {
        let private = self.active().is_some_and(|t| t.is_private());
        self.open_tab_in(private, url, background)
    }

    /// Open `url` in a new tab of a private or a normal window: next to the
    /// active tab if it's the same kind, else the last used one of that kind,
    /// else in a new window. Links from other apps, extensions and the
    /// settings page always open normal tabs.
    pub fn open_tab_in(&self, private: bool, url: &str, background: bool) -> Retained<Tab> {
        let tab = Tab::create(self, None, url, "", false, private);
        self.add_tab(&tab, self.neighbor(private).as_deref(), !background);
        tab
    }

    /// A popup: WebKit made the configuration (with its opener's store).
    pub fn open_tab_with_config(&self, config: &WKWebViewConfiguration, private: bool) -> Retained<Tab> {
        let tab = Tab::create(self, Some(config), "", "", false, private);
        self.add_tab(&tab, self.neighbor(private).as_deref(), true);
        tab
    }

    /// Where a new private or normal tab goes.
    fn neighbor(&self, private: bool) -> Option<Retained<Tab>> {
        if let Some(a) = self.active().filter(|t| t.is_private() == private) {
            return Some(a);
        }
        let tabs = self.ivars().tabs.borrow();
        tabs.iter().filter(|t| t.is_private() == private).max_by_key(|t| t.ivars().last_active.get()).cloned()
    }

    fn add_tab(&self, tab: &Retained<Tab>, next_to: Option<&Tab>, select: bool) {
        self.ivars().tabs.borrow_mut().push(tab.clone());
        let window = tab.window();
        match next_to {
            Some(other) => other.window().addTabbedWindow_ordered(window, NSWindowOrderingMode::Above),
            None => window.center(),
        }
        // Extensions never see private tabs.
        if !tab.is_private() {
            unsafe { self.extensions().didOpenTab(ProtocolObject::from_ref(&**tab)) };
        }
        if select {
            self.select(tab);
        } else {
            tab.ivars().last_active.set(Some(Instant::now()));
        }
    }

    /// Bring a tab to front and make it the target of menu actions.
    pub fn select(&self, tab: &Tab) {
        let window = tab.window();
        if let Some(group) = window.tabGroup() {
            group.setSelectedWindow(Some(window));
        }
        if self.is_on_top() {
            // Shown, but never key: the user's keystrokes must stay in whatever
            // app they're using, not land in a benchmark window.
            window.orderFrontRegardless();
        } else {
            window.makeKeyAndOrderFront(None);
        }
        self.set_active(tab);
    }

    pub fn set_active(&self, tab: &Tab) {
        // A tab dragged to another window becomes active there.
        self.sync_ext_windows();
        if self.is_active(tab) {
            return;
        }
        let prev = self.active();
        if let Some(prev) = &prev {
            prev.ivars().last_active.set(Some(Instant::now()));
        }
        self.ivars().active.replace(Some(tab.retain()));
        tab.wake();
        tab.return_to_playing();
        if let Some(prev) = &prev {
            prev.leave_playing();
        }
        if tab.is_private() {
            return;
        }
        let prev = prev.filter(|p| !p.is_private());
        let ext = self.extensions();
        unsafe {
            ext.didActivateTab_previousActiveTab(
                ProtocolObject::from_ref(tab),
                prev.as_deref().map(ProtocolObject::from_ref),
            )
        };
        let window = self.ext_window_for(tab);
        let prev_window = prev.as_deref().and_then(|p| self.ext_window_for(p));
        if window.as_ref().map(Retained::as_ptr) != prev_window.as_ref().map(Retained::as_ptr) {
            unsafe { ext.didFocusWindow(window.as_deref().map(ProtocolObject::from_ref)) };
        }
    }

    pub fn tab_closed(&self, tab: &Tab) {
        let url = tab.url();
        // Private tabs leave nothing behind, not even Reopen Closed Tab.
        if !url.is_empty() && !url.starts_with(&format!("{INTERNAL}:")) && !tab.is_private() {
            let mut closed = self.ivars().closed.borrow_mut();
            if closed.len() >= 100 {
                closed.remove(0);
            }
            closed.push(url);
        }
        // macOS picks the next selected tab in the group; follow it, since
        // "became key" isn't sent when the app isn't frontmost.
        let next = tab
            .window()
            .tabGroup()
            .map(|g| g.windows())
            .and_then(|ws| ws.iter().find(|w| !std::ptr::eq(&**w, tab.window())))
            .and_then(|w| self.tab_for_window(&w));
        // Tell extensions; the window closes with its last tab.
        let ext_window = self.ext_window_for(tab);
        let window_closing = next.is_none();
        if !tab.is_private() {
            unsafe { self.extensions().didCloseTab_windowIsClosing(ProtocolObject::from_ref(tab), window_closing) };
        }
        if let (true, Some(w)) = (window_closing, ext_window) {
            unsafe { self.extensions().didCloseWindow(ProtocolObject::from_ref(&*w)) };
            self.ivars().ext_windows.borrow_mut().retain(|_, v| Retained::as_ptr(v) != Retained::as_ptr(&w));
        }
        self.ivars().tabs.borrow_mut().retain(|t| !std::ptr::eq(&**t, tab));
        // The last private tab took the private session with it: its cookies
        // and site data were only in memory, and now they're gone.
        if tab.is_private() && !self.ivars().tabs.borrow().iter().any(|t| t.is_private()) {
            self.ivars().private_store.replace(None);
        }
        if self.is_active(tab) {
            self.ivars().active.replace(None);
            if let Some(next) = next.or_else(|| self.ivars().tabs.borrow().last().cloned()) {
                self.select(&next);
            }
        }
    }

    fn extensions(&self) -> &WKWebExtensionController {
        self.ivars().extensions.get().unwrap()
    }

    /// The extension-facing window holding `tab` (its native tab group).
    // ponytail: a move is reported when the tab next becomes active (or right
    // away for menu moves), with its index from then; fine for tab managers.
    pub fn ext_window_for(&self, tab: &Tab) -> Option<Retained<ExtWindow>> {
        if tab.is_private() {
            return None;
        }
        let group = tab.window().tabGroup()?;
        let key = Retained::as_ptr(&group) as usize;
        if let Some(w) = self.ivars().ext_windows.borrow().get(&key) {
            return Some(w.clone());
        }
        let w = ExtWindow::new(self, &group);
        self.ivars().ext_windows.borrow_mut().insert(key, w.clone());
        unsafe { self.extensions().didOpenWindow(ProtocolObject::from_ref(&*w)) };
        Some(w)
    }

    /// Tell extensions about tabs that moved to another window (dragged,
    /// Move Tab to New Window, Merge All Windows), and about windows that
    /// lost their last tab that way.
    fn sync_ext_windows(&self) {
        if self.ivars().extensions.get().is_none() {
            return;
        }
        for t in self.ivars().tabs.borrow().clone().iter().filter(|t| !t.is_private()) {
            let Some(now) = self.ext_window_for(t) else { continue };
            let index = now.tabs().iter().position(|x| std::ptr::eq(&**x, &**t)).unwrap_or(0);
            let before = t.ivars().ext_window.replace(Some((now.clone(), index)));
            if let Some((old, old_index)) = before.filter(|(old, _)| !std::ptr::eq(&**old, &*now)) {
                unsafe {
                    self.extensions().didMoveTab_fromIndex_inWindow(
                        ProtocolObject::from_ref(&**t),
                        old_index,
                        Some(ProtocolObject::from_ref(&*old)),
                    )
                };
            }
        }
        let empty: Vec<Retained<ExtWindow>> =
            self.ivars().ext_windows.borrow().values().filter(|w| w.tabs().is_empty()).cloned().collect();
        for w in empty {
            unsafe { self.extensions().didCloseWindow(ProtocolObject::from_ref(&*w)) };
            self.ivars().ext_windows.borrow_mut().retain(|_, v| !std::ptr::eq(&**v, &*w));
        }
    }

    /// Every window's normal (not private) tabs, in tab-bar order.
    fn windows(&self) -> Vec<Vec<Retained<Tab>>> {
        let mut seen: Vec<usize> = Vec::new();
        let mut out = Vec::new();
        for t in self.ivars().tabs.borrow().iter().filter(|t| !t.is_private()) {
            let group = t.window().tabGroup();
            let key = group.as_ref().map_or(t.window() as *const NSWindow as usize, |g| Retained::as_ptr(g) as usize);
            if seen.contains(&key) {
                continue;
            }
            seen.push(key);
            out.push(match group {
                Some(g) => g.windows().iter().filter_map(|w| self.tab_for_window(&w)).collect(),
                None => vec![t.clone()],
            });
        }
        out
    }

    pub fn ext_tab_changed(&self, tab: &Tab, changed: WKWebExtensionTabChangedProperties) {
        if !tab.is_private() {
            unsafe { self.extensions().didChangeTabProperties_forTab(changed, ProtocolObject::from_ref(tab)) };
        }
    }

    pub fn tab_for_window(&self, window: &NSWindow) -> Option<Retained<Tab>> {
        self.ivars().tabs.borrow().iter().find(|t| std::ptr::eq(t.window(), window)).cloned()
    }

    /// Tabs of the active window, in tab-bar order.
    pub fn vault(&self) -> &passwords::Vault {
        self.ivars().vault.get().unwrap()
    }

    /// The tab showing this web view.
    fn tab_of_web(&self, web: &WKWebView) -> Option<Retained<Tab>> {
        self.ivars().tabs.borrow().iter().find(|t| t.web().is_some_and(|w| std::ptr::eq(&*w, web))).cloned()
    }

    /// Redraw every tab's toolbar (a bookmark changed, say).
    pub fn refresh_toolbars(&self) {
        for t in self.ivars().tabs.borrow().clone() {
            t.refresh_toolbar();
        }
    }

    /// The Bookmarks menu: its two fixed items, the 10 newest bookmarks,
    /// then (if there are more) every bookmark in a submenu, folders as
    /// submenus. Rebuilt each time it opens.
    fn fill_bookmarks_menu(&self, menu: &NSMenu) {
        while menu.numberOfItems() > 2 {
            menu.removeItemAtIndex(2);
        }
        let url = self.active().map(|t| t.url()).unwrap_or_default();
        let on = self.store().bookmark_of(&url).is_some();
        if let Some(first) = menu.itemAtIndex(0) {
            first.setTitle(&NSString::from_str(if on { "Remove Bookmark" } else { "Bookmark This Page" }));
        }
        let all = self.store().bookmarks("");
        if all.is_empty() {
            return;
        }
        menu.addItem(&NSMenuItem::separatorItem(self.mtm()));
        let mut newest: Vec<&store::Bookmark> = all.iter().collect();
        newest.sort_by_key(|b| std::cmp::Reverse(b.id));
        for b in newest.iter().take(10) {
            menu.addItem(&self.page_item(&b.title, &b.url));
        }
        if all.len() > 10 {
            let sub = NSMenu::initWithTitle(NSMenu::alloc(self.mtm()), &NSString::from_str("All Bookmarks"));
            self.add_bookmark_items(&sub, &all, "");
            let item = NSMenuItem::new(self.mtm());
            item.setTitle(&NSString::from_str("All Bookmarks"));
            item.setSubmenu(Some(&sub));
            menu.addItem(&NSMenuItem::separatorItem(self.mtm()));
            menu.addItem(&item);
        }
    }

    /// The History menu: its fixed items, then the 10 latest pages.
    fn fill_history_menu(&self, menu: &NSMenu) {
        let fixed = 4;
        while menu.numberOfItems() > fixed {
            menu.removeItemAtIndex(fixed);
        }
        let mut seen = std::collections::HashSet::new();
        let visits = self.store().history("", 100);
        let recent: Vec<_> = visits.iter().filter(|v| seen.insert(v.url.as_str())).take(10).collect();
        if !recent.is_empty() {
            menu.addItem(&NSMenuItem::separatorItem(self.mtm()));
        }
        for v in recent {
            menu.addItem(&self.page_item(&v.title, &v.url));
        }
    }

    /// A menu item that opens `url` (`openMenuURL:`).
    fn page_item(&self, title: &str, url: &str) -> Retained<NSMenuItem> {
        let title: String = if title.is_empty() { url } else { title }.chars().take(70).collect();
        let item = menu_item(self.mtm(), &title, sel!(openMenuURL:), "", NSEventModifierFlags::empty());
        unsafe { item.setRepresentedObject(Some(&NSString::from_str(url))) };
        item.setToolTip(Some(&NSString::from_str(url)));
        unsafe { item.setTarget(Some(self)) };
        item
    }

    /// Items for the bookmarks directly in `folder`, after a submenu for each
    /// folder inside it.
    fn add_bookmark_items(&self, menu: &NSMenu, all: &[store::Bookmark], folder: &str) {
        let mtm = self.mtm();
        let mut children: Vec<&str> = Vec::new();
        for b in all {
            let rest =
                if folder.is_empty() { Some(b.folder.as_str()) } else { b.folder.strip_prefix(&format!("{folder}/")) };
            if let Some(child) =
                rest.filter(|r| !r.is_empty() && b.folder != folder).map(|r| r.split('/').next().unwrap())
                && !children.contains(&child)
            {
                children.push(child);
            }
        }
        for child in children {
            let path = if folder.is_empty() { child.to_string() } else { format!("{folder}/{child}") };
            let sub = NSMenu::initWithTitle(NSMenu::alloc(mtm), &NSString::from_str(child));
            self.add_bookmark_items(&sub, all, &path);
            let item = NSMenuItem::new(mtm);
            item.setTitle(&NSString::from_str(child));
            item.setSubmenu(Some(&sub));
            menu.addItem(&item);
        }
        // ponytail: a huge folder shows its first 500; the rest are in Settings → Bookmarks.
        for b in all.iter().filter(|b| b.folder == folder).take(500) {
            menu.addItem(&self.page_item(&b.title, &b.url));
        }
    }

    /// File → Profiles: every profile (this one ticked), then Manage.
    fn fill_profiles_menu(&self, menu: &NSMenu) {
        menu.removeAllItems();
        let current = self.profile_dir().and_then(profiles::name_of);
        for name in profiles::all() {
            let item = menu_item(self.mtm(), &name, sel!(openProfile:), "", NSEventModifierFlags::empty());
            unsafe { item.setTarget(Some(self)) };
            item.setState((current.as_deref() == Some(name.as_str())) as isize);
            menu.addItem(&item);
        }
        menu.addItem(&NSMenuItem::separatorItem(self.mtm()));
        let manage =
            menu_item(self.mtm(), "Add or Manage Profiles…", sel!(manageProfiles:), "", NSEventModifierFlags::empty());
        unsafe { manage.setTarget(Some(self)) };
        menu.addItem(&manage);
    }

    /// One of the menu bar's menus, by title.
    fn menu_named(&self, title: &str) -> Option<Retained<NSMenu>> {
        let main = NSApplication::sharedApplication(self.mtm()).mainMenu()?;
        main.itemArray().iter().filter_map(|i| i.submenu()).find(|m| m.title().to_string() == title)
    }

    pub fn ordered_tabs(&self) -> Vec<Retained<Tab>> {
        let Some(active) = self.active() else { return self.ivars().tabs.borrow().clone() };
        match active.window().tabbedWindows() {
            Some(windows) => windows.iter().filter_map(|w| self.tab_for_window(&w)).collect(),
            None => vec![active],
        }
    }

    fn select_index(&self, i: usize) {
        if let Some(t) = self.ordered_tabs().get(i) {
            self.select(t);
        }
    }

    fn select_relative(&self, delta: isize) {
        let tabs = self.ordered_tabs();
        let Some(pos) = tabs.iter().position(|t| self.is_active(t)) else { return };
        let n = tabs.len() as isize;
        self.select_index((pos as isize + delta).rem_euclid(n) as usize);
    }

    pub fn open_settings_page(&self, page: &str) {
        self.open_internal(page);
    }

    fn open_internal(&self, page: &str) {
        let url = format!("{INTERNAL}://{page}");
        // Reuse an open settings tab.
        let prefix = format!("{INTERNAL}://settings");
        let existing = self.ivars().tabs.borrow().iter().find(|t| t.url().starts_with(&prefix)).cloned();
        match existing {
            Some(t) => {
                self.select(&t);
                t.load(&url);
            }
            None => {
                self.open_tab(&url, false);
            }
        }
    }

    /// Put background tabs to sleep: idle ones after the configured minutes
    /// (one minute in Low Power Mode or when the Mac runs hot), all of them
    /// when `all` (memory pressure).
    fn sleep_background_tabs(&self, all: bool) {
        let minutes: u64 = self.store().setting("tab_sleep_minutes").parse().unwrap_or(0);
        let idle = sleep_after(minutes, saving_power());
        for tab in self.ivars().tabs.borrow().clone() {
            let since = tab.ivars().last_active.get().map_or(Duration::ZERO, |t| t.elapsed());
            // Pinned tabs and tabs playing sound (music, a video in picture in
            // picture) stay awake, unless macOS runs out of memory.
            let idle = idle.filter(|_| !tab.is_pinned() && !tab.playing_audio());
            if !self.is_active(&tab) && (all || idle.is_some_and(|idle| since >= idle)) {
                tab.sleep();
            }
        }
    }

    /// "Pin Tab" / "Unpin Tab", "Mute Tab" / "Unmute Tab", for the active tab.
    fn update_window_menu(&self, menu: &NSMenu) {
        let tab = self.active();
        let (pinned, muted) = tab.as_ref().map_or((false, false), |t| (t.is_pinned(), t.is_muted()));
        for (action, title) in [
            (sel!(pinTab:), if pinned { "Unpin Tab" } else { "Pin Tab" }),
            (sel!(muteTab:), if muted { "Unmute Tab" } else { "Mute Tab" }),
        ] {
            let i = unsafe { menu.indexOfItemWithTarget_andAction(Some(self), Some(action)) };
            if let Some(item) = menu.itemAtIndex(i).filter(|_| i >= 0) {
                item.setTitle(&NSString::from_str(title));
            }
        }
    }

    /// Take Screenshot's items: what to capture, then where it goes (checked
    /// by `update_screenshot_menu`).
    pub fn screenshot_menu(&self, keys: bool) -> Retained<NSMenu> {
        let mtm = self.mtm();
        let menu = NSMenu::initWithTitle(NSMenu::alloc(mtm), &NSString::from_str("Take Screenshot"));
        let shift_cmd = NSEventModifierFlags::Command | NSEventModifierFlags::Shift;
        for (title, action, key, tag) in [
            ("Select Area…", sel!(screenshotArea:), "S", 0),
            ("Visible Part", sel!(screenshotVisible:), "", 0),
            ("Full Page", sel!(screenshotFullPage:), "", 0),
            ("-", sel!(screenshotTo:), "", 0),
            ("Save to File", sel!(screenshotTo:), "", 0),
            ("Copy to Clipboard", sel!(screenshotTo:), "", 1),
            ("-", sel!(recordScreen:), "", 0),
            ("Record Screen…", sel!(recordScreen:), "", 0),
        ] {
            if title == "-" {
                menu.addItem(&NSMenuItem::separatorItem(mtm));
                continue;
            }
            let key = if keys { key } else { "" };
            let item = menu_item(
                mtm,
                title,
                action,
                key,
                if key.is_empty() { NSEventModifierFlags::empty() } else { shift_cmd },
            );
            item.setTag(tag);
            unsafe { item.setTarget(Some(self)) };
            menu.addItem(&item);
        }
        menu
    }

    /// A check next to Save to File or Copy to Clipboard.
    fn update_screenshot_menu(&self, menu: &NSMenu) {
        let copy = self.store().setting("screenshot_to") == "clipboard";
        for item in menu.itemArray().iter().filter(|i| i.action() == Some(sel!(screenshotTo:))) {
            item.setState(((item.tag() == 1) == copy) as isize);
        }
    }

    /// The page's right-click menu (not on a link): Translate Page and
    /// Take Screenshot, so they're found where people look.
    pub fn add_page_items(&self, menu: &NSMenu, translated: bool) {
        let mtm = self.mtm();
        menu.addItem(&NSMenuItem::separatorItem(mtm));
        let title = if translated { "Show Original" } else { "Translate Page" };
        let translate = menu_item(mtm, title, sel!(translatePage:), "", NSEventModifierFlags::empty());
        unsafe { translate.setTarget(Some(self)) };
        menu.addItem(&translate);
        let holder = menu_item(mtm, "Take Screenshot", sel!(submenuAction:), "", NSEventModifierFlags::empty());
        let shots = self.screenshot_menu(false);
        self.update_screenshot_menu(&shots);
        holder.setSubmenu(Some(&shots));
        menu.addItem(&holder);
    }

    /// "Translate Page" / "Show Original", for the active tab.
    fn update_view_menu(&self, menu: &NSMenu) {
        let translated = self.active().is_some_and(|t| t.is_translated());
        let i = unsafe { menu.indexOfItemWithTarget_andAction(Some(self), Some(sel!(translatePage:))) };
        if let Some(item) = menu.itemAtIndex(i).filter(|_| i >= 0) {
            item.setTitle(&NSString::from_str(if translated { "Show Original" } else { "Translate Page" }));
        }
    }

    /// Every open tab, in the order they were opened.
    pub fn all_tabs(&self) -> Vec<Retained<Tab>> {
        self.ivars().tabs.borrow().clone()
    }

    fn check_heavy_tabs(&self) {
        for tab in self.ivars().tabs.borrow().clone() {
            tab.check_heavy();
        }
    }

    /// Settings → Tabs: every tab with its page process's memory and CPU,
    /// the browser's own process, and WebKit's other helpers (network,
    /// graphics). Private tabs are listed without their address or title.
    pub fn tabs_json(&self) -> String {
        let tabs = self.ivars().tabs.borrow().clone();
        let pids: Vec<Option<i32>> = tabs.iter().map(|t| t.pid()).collect();
        let row = |u: Option<usage::Usage>| {
            let u = u.unwrap_or_default();
            format!("\"footprint\":{},\"cpu_ms\":{},\"wakeups\":{}", u.footprint, u.cpu_ns / 1_000_000, u.wakeups)
        };
        let items: Vec<String> = tabs
            .iter()
            .zip(&pids)
            .map(|(t, pid)| {
                let (url, title) = if t.is_private() { (String::new(), "Private tab".to_string()) } else { (t.url(), t.title()) };
                let shared = pid.is_some() && pids.iter().filter(|p| *p == pid).count() > 1;
                format!(
                    "{{\"id\":{},\"url\":{},\"title\":{},\"active\":{},\"asleep\":{},\"private\":{},\"shared\":{shared},\"heavy\":{},\"pid\":{},{}}}",
                    Retained::as_ptr(t) as usize,
                    store::json_str(&url),
                    store::json_str(&title),
                    self.is_active(t),
                    t.is_asleep(),
                    t.is_private(),
                    store::json_str(&t.heavy().unwrap_or_default()),
                    pid.unwrap_or(0),
                    row(pid.and_then(usage::of))
                )
            })
            .collect();
        let helpers = usage::webkit_helpers().into_iter().filter(|p| !pids.contains(&Some(*p))).filter_map(usage::of);
        let others = helpers.fold(usage::Usage::default(), |a, u| usage::Usage {
            footprint: a.footprint + u.footprint,
            cpu_ns: a.cpu_ns + u.cpu_ns,
            wakeups: a.wakeups + u.wakeups,
        });
        format!(
            "{{\"shell\":{{{}}},\"helpers\":{{{}}},\"tabs\":[{}]}}",
            row(usage::of(std::process::id() as i32)),
            row(Some(others)),
            items.join(",")
        )
    }

    /// A tab by the id `tabs_json` gave it.
    pub fn tab_by_id(&self, id: usize) -> Option<Retained<Tab>> {
        self.ivars().tabs.borrow().iter().find(|t| Retained::as_ptr(t) as usize == id).cloned()
    }

    fn start_timer(&self) {
        let ptr = self as *const Self;
        let block = RcBlock::new(move |_t: std::ptr::NonNull<NSTimer>| {
            // The app delegate lives until the process exits.
            let app = unsafe { &*ptr };
            app.sleep_background_tabs(memory_pressure());
            app.check_heavy_tabs();
            app.stop_idle_translator();
            app.save_session();
        });
        let timer = unsafe { NSTimer::scheduledTimerWithTimeInterval_repeats_block(30.0, true, &block) };
        // Let macOS batch this with other wakeups.
        timer.setTolerance(10.0);
        let _ = self.ivars().timer.set(timer);
    }

    fn save_session(&self) {
        if self.profile_dir().is_none() || self.ivars().store.get().is_none() {
            return;
        }
        // Every window, not just the front one; private tabs never.
        let saved: Vec<(i64, Retained<Tab>)> = self
            .windows()
            .into_iter()
            .enumerate()
            .flat_map(|(w, tabs)| tabs.into_iter().map(move |t| (w as i64, t)))
            .filter(|(_, t)| !t.url().is_empty() && !t.url().starts_with(&format!("{INTERNAL}:")))
            .collect();
        let rows: Vec<store::SessionTab> = saved.iter().map(|(w, t)| (*w, t.url(), t.title(), t.is_pinned())).collect();
        self.store().save_session(&rows);
        let active = saved.iter().position(|(_, t)| self.is_active(t));
        self.store().put("session_active", &active.unwrap_or(0).to_string());
    }

    /// Address-bar input to URL: a search keyword or bang (`!w rust`), an
    /// address, or a search with the chosen engine.
    pub fn normalize(&self, input: &str) -> String {
        // A path on this Mac: its folder page, or the file.
        if let Some(path) = files::typed_path(input) {
            return if path.is_dir() { files::folder_url(&path) } else { files::file_url(&path) };
        }
        keyword_url(input, &self.store().keywords(), &self.store().setting("bangs"))
            .unwrap_or_else(|| normalize(input, &self.store().setting("search")))
    }

    // ---- downloads

    pub fn start_download(&self, download: &WKDownload) {
        unsafe { download.setDelegate(Some(ProtocolObject::from_ref(self))) };
        self.ivars().downloads.borrow_mut().insert(key(download), (0, download.retain(), String::new()));
    }

    fn end_download(&self, download: &WKDownload, status: &str) {
        let Some((id, _, path)) = self.ivars().downloads.borrow_mut().remove(&key(download)) else { return };
        let bytes = std::fs::metadata(&path).map_or(0, |m| m.len() as i64);
        if id != 0 {
            self.store().finish_download(id, status, bytes);
        }
        if self.is_bench() {
            say!("DOWNLOAD {status} {path}");
        }
        let package = Path::new(&path).starts_with(extensions::unpack_root()) && path.ends_with(".crx");
        let expected = if package { self.ivars().expected_package.take() } else { None };
        let update = expected.as_ref().and_then(|_| self.ivars().pending_update.take());
        match (expected, update) {
            (Some(_), Some((row, reply))) if status == "done" => self.finish_update(row, PathBuf::from(&path), reply),
            (Some(_), Some((_, reply))) => reply(Err(format!("The download {status}."))),
            (Some(webstore), None) if status == "done" => self.install_package(PathBuf::from(&path), webstore),
            _ => {}
        }
    }

    /// The user is adding Web Store extension `id`: its package download is
    /// expected (and nothing else is installed without being picked).
    pub fn expect_package(&self, id: &str) {
        self.ivars().expected_package.replace(Some(id.to_string()));
    }

    /// Install a downloaded package, after the usual prompt. Problems show on
    /// the active tab.
    fn install_package(&self, file: PathBuf, webstore: String) {
        let this = self.retain();
        let f = file.clone();
        internal::install_extension(
            self,
            file,
            Box::new(move |result| {
                let _ = std::fs::remove_file(&f);
                // Remember where it came from, for Update.
                let row =
                    result.as_ref().ok().and_then(|j| j.strip_prefix("{\"id\":")?.split(',').next()?.parse().ok());
                if let Some(row) = row {
                    this.store().set_extension_webstore(row, &webstore);
                }
                match result {
                    Ok(json) if this.is_bench() => say!("INSTALLED {json}"),
                    Err(e) if e != "cancelled" => {
                        if let Some(t) = this.active() {
                            t.confirm(&format!("Couldn't add the extension.\n\n{e}"), &["OK"], |_| {});
                        }
                    }
                    _ => {}
                }
            }),
        );
    }

    /// (downloads row, completed bytes, total bytes) for downloads in flight.
    pub fn download_progress(&self) -> Vec<(i64, i64, i64)> {
        self.ivars()
            .downloads
            .borrow()
            .values()
            .map(|(id, d, _)| {
                // WKDownload.progress (NSProgress), via msg_send: the binding needs an extra feature.
                let p: Retained<AnyObject> = unsafe { msg_send![&**d, progress] };
                let done: i64 = unsafe { msg_send![&*p, completedUnitCount] };
                let total: i64 = unsafe { msg_send![&*p, totalUnitCount] };
                (*id, done, total)
            })
            .collect()
    }

    pub fn cancel_download(&self, id: i64) {
        let found =
            self.ivars().downloads.borrow().values().find(|(i, ..)| *i == id && id != 0).map(|(_, d, _)| d.clone());
        if let Some(d) = found {
            unsafe { d.cancel(None) };
        }
    }

    fn downloads_dir(&self) -> PathBuf {
        let custom = self.store().setting("downloads_dir");
        if custom.is_empty() { PathBuf::from(self.default_downloads_dir()) } else { PathBuf::from(custom) }
    }

    pub fn default_downloads_dir(&self) -> String {
        std::env::var_os("HOME")
            .map(|h| PathBuf::from(h).join("Downloads").to_string_lossy().into_owned())
            .unwrap_or_else(|| std::env::temp_dir().to_string_lossy().into_owned())
    }

    /// Quick Look a file (Settings → Downloads). Quartz loads on first use,
    /// so the preview costs nothing until then. Bench mode reports it
    /// instead: the panel would take keyboard focus.
    pub fn quick_look(&self, path: &str) -> Result<(), String> {
        if !Path::new(path).exists() {
            return Err("the file is gone".into());
        }
        self.ivars().preview.replace(Some(NSURL::fileURLWithPath(&NSString::from_str(path))));
        if self.is_bench() {
            say!("PREVIEW {path}");
            return Ok(());
        }
        let quartz = objc2_foundation::NSBundle::bundleWithPath(&NSString::from_str(
            "/System/Library/Frameworks/Quartz.framework",
        ));
        let class = quartz
            .filter(|b| unsafe { b.load() })
            .and_then(|_| AnyClass::get(c"QLPreviewPanel"))
            .ok_or("Quick Look isn't available")?;
        let panel: Retained<AnyObject> = unsafe { msg_send![class, sharedPreviewPanel] };
        let _: () = unsafe { msg_send![&*panel, reloadData] };
        let _: () = unsafe { msg_send![&*panel, makeKeyAndOrderFront: Option::<&AnyObject>::None] };
        Ok(())
    }

    // ---- extensions

    /// Ask the user whether an extension may `what` (a sheet on the tab).
    fn prompt_extension(
        &self,
        ctx: &WKWebExtensionContext,
        tab: Option<&ProtocolObject<dyn WKWebExtensionTab>>,
        what: &str,
        done: impl Fn(bool) + 'static,
    ) {
        let name = extensions::extension_name(&*unsafe { ctx.webExtension() });
        let Some(tab) = tab.and_then(|t| self.tab_of(t)).or_else(|| self.active()) else { return done(false) };
        tab.confirm(&format!("“{name}” wants to {what}"), &["Allow", "Don't Allow"], done);
    }

    // ---- bench protocol (benchmark/bench.rs, tests/e2e.rs)

    /// Forward stdin lines to `command:` on the main thread. EOF (harness gone) quits.
    fn read_commands(&self) {
        // The delegate lives until the process exits, so the pointer stays valid.
        let ptr = self as *const Self as usize;
        std::thread::spawn(move || {
            let lines = std::io::stdin().lines().map_while(Result::ok);
            for line in lines.chain(std::iter::once("quit".to_string())) {
                let obj = unsafe { &*(ptr as *const AnyObject) };
                let line = NSString::from_str(&line);
                let _: () = unsafe {
                    msg_send![obj, performSelectorOnMainThread: sel!(command:), withObject: &*line, waitUntilDone: false]
                };
            }
        });
    }

    fn command(&self, cmd: &str) {
        let (name, arg) = cmd.split_once(' ').unwrap_or((cmd, ""));
        let start = |t: &Tab| t.ivars().nav_start.set(Some(Instant::now()));
        match (name, self.active()) {
            ("go", Some(t)) => {
                start(&t);
                t.load(arg);
            }
            ("back", Some(t)) => {
                start(&t);
                t.go_back();
            }
            ("forward", Some(t)) => {
                start(&t);
                t.go_forward();
            }
            ("reload", Some(t)) => {
                start(&t);
                t.reload();
            }
            ("state", Some(t)) => say!("{}", t.state()),
            // Type into the address bar and press Return (`barenter <text>`),
            // or click into it and away again (`barblur`).
            ("barenter", Some(t)) => {
                start(&t);
                t.address_bar_key(Some(arg));
            }
            // What Return on a search in the address bar saves, without loading
            // the search engine (tests stay offline): `SAVEDSEARCH`.
            ("savesearch", Some(t)) => {
                t.remember_search(arg);
                say!("SAVEDSEARCH");
            }
            ("bartype", Some(t)) => say!("{}", t.bar_type(arg)),
            ("barkey", Some(t)) => say!("{}", t.bar_press(arg)),
            ("barblur", Some(t)) => {
                t.address_bar_key(None);
                say!("BLURRED");
            }
            ("visible", Some(t)) => {
                let visible = t.window().occlusionState().contains(NSWindowOcclusionState::Visible);
                say!("VISIBLE {}", visible as u8);
            }
            ("window", Some(t)) => {
                let app_active = NSApplication::sharedApplication(self.mtm()).isActive();
                say!(
                    "WINDOW minimized={} key={} app_active={}",
                    t.window().isMiniaturized() as u8,
                    t.window().isKeyWindow() as u8,
                    app_active as u8
                );
            }
            ("newtab", _) => {
                let t = self.open_tab("", false);
                if arg.is_empty() {
                    say!("{}", self.tabs_line());
                } else {
                    start(&t);
                    t.load(arg);
                }
            }
            ("switch", _) => {
                let i: usize = arg.parse().unwrap_or(0);
                match self.ordered_tabs().get(i).cloned() {
                    // Waking reloads the page: report that load.
                    Some(t) if t.is_asleep() => {
                        start(&t);
                        self.select(&t);
                    }
                    Some(t) => {
                        self.select(&t);
                        say!("{}", self.tabs_line());
                    }
                    None => say!("ERR no tab {i}"),
                }
            }
            ("tabs", _) => say!("{}", self.tabs_line()),
            // Open Search Tabs with this query: `TABSEARCH <title>|…` as listed.
            ("tabsearch", Some(t)) => tabsearch::TabSearch::open(self, &t, arg),
            // What macOS does when another app opens a link with us.
            ("openurl", _) => {
                self.open_external(vec![arg.to_string()]);
                say!("{}", self.tabs_line());
            }
            // Click an extension's toolbar button on the active tab.
            ("extclick", Some(t)) => match self.extension_clicked(arg.parse().unwrap_or(-1), &t) {
                Ok(()) => say!("EXTCLICK ok"),
                Err(e) => say!("EXTCLICK err {e}"),
            },
            // `EXTBUTTONS <id>:<on 0|1> ...` for the active tab.
            ("extbuttons", Some(t)) => {
                let items: Vec<String> =
                    t.extension_buttons().iter().map(|(id, on)| format!("{id}:{}", *on as u8)).collect();
                say!("EXTBUTTONS {}", items.join(" "));
            }
            ("sleep", _) => {
                self.sleep_background_tabs(true);
                say!("{}", self.tabs_line());
            }
            // Run the heavy-tab check now: `HEAVY <0|1> …` per tab, in tab-bar order.
            ("heavy", _) => {
                self.check_heavy_tabs();
                let flags: Vec<String> =
                    self.ordered_tabs().iter().map(|t| (t.heavy().is_some() as u8).to_string()).collect();
                say!("HEAVY {}", flags.join(" "));
            }
            ("key", _) => say!("KEY {}", if self.press(arg) { "ok" } else { "none" }),
            ("find", Some(t)) => {
                t.show_find();
                t.set_find_text(arg);
                t.ivars().find_report.set(true);
                t.find(false);
            }
            ("findnext" | "findprev", Some(t)) => {
                t.ivars().find_report.set(true);
                self.find_in(&t, name == "findprev");
            }
            ("findbar", Some(t)) => say!("{}", t.find_state()),
            ("home", Some(t)) => say!("{}", t.home_state()),
            ("netbutton", Some(t)) => say!("NETBUTTON {}", t.net_button_shown() as u8),
            ("storebutton", Some(t)) => say!("STOREBUTTON {}", t.store_button_shown() as u8),
            // Press the Web Store button.
            ("storeclick", Some(t)) => {
                let _: () = unsafe { msg_send![&*t, addFromWebStore: Option::<&AnyObject>::None] };
                say!("STORECLICK");
            }
            ("star", Some(t)) => say!("{}", t.star_state()),
            ("tabflags", Some(t)) => say!("{}", t.flags_state()),
            // The site data sheet: `SITEDATA <name>:<kinds>|…`.
            ("sitedata", Some(t)) => t.show_site_data(),
            ("zoom", Some(t)) => say!("{}", t.zoom_state()),
            ("reader", Some(t)) => t.toggle_reader(),
            // Translate Page / Show Original: `TRANSLATED <from> <texts>`,
            // `TRANSLATE off` or `TRANSLATE err <code>`.
            ("translate", Some(t)) => t.toggle_translate(),
            // Hide Element…: the next click on the page picks; `HIDDEN <0|1>`
            // once the sheet is answered with OK.
            // `SPELL continuous=<0|1> grammar=<0|1> correct=<0|1>`, from the page's web view.
            ("spell", Some(t)) => say!("{}", t.spell_state()),
            // The PDF viewer: `PDF pages= page= sidebar= twoup= rotation= highlights=`,
            // a toolbar button (`pdfcmd <i>`, pdf::BUTTONS), selecting a page's start.
            ("pdf", Some(t)) => say!("{}", t.pdf_state()),
            ("pdfcmd", Some(t)) => {
                t.pdf_command(arg.parse().unwrap_or(usize::MAX));
                say!("{}", t.pdf_state());
            }
            ("pdfselect", Some(t)) => {
                t.pdf_select(arg.parse().unwrap_or(0));
                say!("PDFSELECT");
            }
            // View → Show Web Inspector: `INSPECTOR <0|1>` (whether there's one to show).
            ("inspector", Some(t)) => t.show_inspector(),
            ("picker", Some(t)) => {
                t.pick_element();
                say!("PICKER");
            }
            // Pick account `i` from the saved-passwords menu (`PASSWORDS …`).
            // Pick entry `i` from the autofill menu (`AUTOFILL <label>|…`).
            ("fillautofill", Some(t)) => {
                t.fill_autofill_at(arg.parse().unwrap_or(0));
                say!("FILLAUTOFILL");
            }
            ("fillpassword", Some(t)) => {
                t.fill_password_at(arg.parse().unwrap_or(0));
                say!("FILLPASSWORD");
            }
            // Run JavaScript in the page (as the page, with a user gesture):
            // `EVAL <result>`.
            ("eval", Some(t)) => match t.web() {
                Some(web) => {
                    let done = RcBlock::new(|r: *mut AnyObject, e: *mut NSError| {
                        let text: String = match (unsafe { r.as_ref() }, unsafe { e.as_ref() }) {
                            (Some(r), _) => {
                                let d: Option<Retained<NSString>> = unsafe { msg_send![r, description] };
                                d.map(|d| d.to_string()).unwrap_or_default()
                            }
                            (None, Some(e)) => format!("error {}", e.localizedDescription()),
                            _ => "undefined".into(),
                        };
                        say!("EVAL {}", text.replace('\n', " "));
                    });
                    unsafe { web.evaluateJavaScript_completionHandler(&NSString::from_str(arg), Some(&done)) };
                }
                None => say!("EVAL error no page"),
            },
            // A menu bar menu's items as shown: `MENU <title>|…` (`-` separator).
            // `menu File/Duplicate Tab` picks that item first, as a click would.
            ("menu", _) => {
                let (name, pick) = arg.split_once('/').map_or((arg, None), |(m, i)| (m, Some(i)));
                let main = NSApplication::sharedApplication(self.mtm()).mainMenu();
                let menu = main.and_then(|m| {
                    m.itemArray().iter().filter_map(|i| i.submenu()).find(|s| s.title().to_string() == name)
                });
                let Some(menu) = menu else { return say!("ERR no menu {name}") };
                // As when it opens: the delegate's update, then validation.
                let open = |menu: &NSMenu| {
                    match name {
                        "Window" => self.update_window_menu(menu),
                        "View" => self.update_view_menu(menu),
                        _ => {}
                    }
                    menu.update();
                };
                open(&menu);
                if let Some(title) = pick {
                    let index = menu.indexOfItemWithTitle(&NSString::from_str(title));
                    if index < 0 {
                        return say!("ERR no item {title}");
                    }
                    menu.performActionForItemAtIndex(index);
                    open(&menu);
                }
                let titles: Vec<String> = menu
                    .itemArray()
                    .iter()
                    .filter(|i| !i.isHidden())
                    .map(|i| if i.isSeparatorItem() { "-".into() } else { i.title().to_string() })
                    .collect();
                say!("MENU {}", titles.join("|"));
            }
            ("private", Some(t)) => say!("{}", t.private_state()),
            // `PROFILE <name> <address bar placeholder>`.
            ("profile", Some(t)) => say!("PROFILE {} {}", self.profile_name().unwrap_or_default(), t.placeholder()),
            // `linkmenu <url> [i]`: WebKit's link menu after our changes (and
            // picking item i): `LINKMENU <title>|…`.
            ("linkmenu", Some(t)) => {
                let (url, pick) = arg.split_once(' ').map_or((arg, None), |(u, i)| (u, i.parse::<isize>().ok()));
                let menu = NSMenu::new(self.mtm());
                for (title, id) in [
                    ("Open Link", "OpenLink"),
                    ("Open Link in New Window", "OpenLinkInNewWindow"),
                    ("Download Linked File", "DownloadLinkedFile"),
                    ("Copy Link", "CopyLink"),
                ] {
                    let item = menu_item(self.mtm(), title, sel!(copy:), "", NSEventModifierFlags::empty());
                    objc2_app_kit::NSUserInterfaceItemIdentification::setIdentifier(
                        &*item,
                        Some(&NSString::from_str(&format!("WKMenuItemIdentifier{id}"))),
                    );
                    menu.addItem(&item);
                }
                t.link_menu(&menu, Some(url));
                if let Some(i) = pick {
                    menu.performActionForItemAtIndex(i);
                }
                let titles: Vec<String> = menu.itemArray().iter().map(|i| i.title().to_string()).collect();
                say!("LINKMENU {}", titles.join("|"));
            }
            // Print the page to a PDF file, through the print panel's own path.
            ("printpdf", Some(t)) => t.print(Some(arg)),
            // Save the page as a Web Archive there: `SAVED <0|1>`.
            ("savepage", Some(t)) => t.save_page(Some(arg)),
            // The whole page as a PNG there: `SCREENSHOT <0|1> <width>x<height>`.
            ("screenshot", Some(t)) => {
                let mut words = arg.split(' ');
                let Some(shot) = words.next().and_then(screenshot::Shot::parse) else { return say!("ERR shot") };
                let mut next = words.next();
                // `area x,y,w,h`: that area, without the picker.
                let area = (shot == screenshot::Shot::Area)
                    .then(|| {
                        let n: Vec<f64> = next?.split(',').filter_map(|v| v.parse().ok()).collect();
                        let r = (n.len() == 4).then(|| {
                            objc2_foundation::NSRect::new(
                                NSPoint::new(n[0], n[1]),
                                objc2_foundation::NSSize::new(n[2], n[3]),
                            )
                        })?;
                        next = words.next();
                        Some(r)
                    })
                    .flatten();
                t.screenshot(shot, next, area);
            }
            // Drag on the area picker: `screenshotdrag x1,y1,x2,y2`.
            ("screenshotdrag", Some(t)) => {
                let n: Vec<f64> = arg.split(',').filter_map(|v| v.parse().ok()).collect();
                match (t.area_picker(), n.len()) {
                    (Some(p), 4) => p.drag(NSPoint::new(n[0], n[1]), NSPoint::new(n[2], n[3])),
                    _ => say!("ERR no picker"),
                }
            }
            // The Bookmarks or History menu as it would open:
            // `BOOKMARKSMENU a|b|-|Folder/|c`. With an index, picks that item
            // (as a click would) first.
            ("bookmarksmenu" | "historymenu", _) => {
                let history = name == "historymenu";
                let Some(menu) = self.menu_named(if history { "History" } else { "Bookmarks" }) else {
                    return say!("ERR no menu");
                };
                if history {
                    self.fill_history_menu(&menu)
                } else {
                    self.fill_bookmarks_menu(&menu)
                }
                if let Ok(i) = arg.parse::<isize>() {
                    menu.performActionForItemAtIndex(i);
                }
                let titles: Vec<String> = menu
                    .itemArray()
                    .iter()
                    .map(|i| match (i.isSeparatorItem(), i.hasSubmenu()) {
                        (true, _) => "-".into(),
                        (_, true) => format!("{}/", i.title()),
                        _ => i.title().to_string(),
                    })
                    .collect();
                say!("{} {}", name.to_uppercase(), titles.join("|"));
            }
            // The toolbar's home button.
            ("gohome", Some(t)) => {
                t.go_home();
                say!("{}", t.home_state());
            }
            // Answer the sheet on screen: `dialog ok|cancel [check]`.
            ("dialog", Some(t)) => {
                let ok = arg.starts_with("ok");
                say!("ANSWERED {}", t.answer_dialog(ok, arg.ends_with("check")) as u8);
            }
            // Where the selection (the current find match) starts in its text node.
            ("selection", Some(t)) => match t.web() {
                Some(web) => {
                    let done = RcBlock::new(|r: *mut AnyObject, _e: *mut NSError| {
                        let n = unsafe { r.as_ref() }
                            .and_then(|o| o.downcast_ref::<objc2_foundation::NSNumber>())
                            .map_or(-1, |n| n.integerValue());
                        say!("SELECTION {n}");
                    });
                    unsafe {
                        web.evaluateJavaScript_inFrame_inContentWorld_completionHandler(
                            &NSString::from_str(
                                "(s => s.rangeCount ? s.getRangeAt(0).startOffset : -1)(getSelection())",
                            ),
                            None,
                            &WKContentWorld::defaultClientWorld(self.mtm()),
                            Some(&done),
                        )
                    };
                }
                None => say!("SELECTION -1"),
            },
            // Esc in the find field, through the field's own key handling.
            ("findesc", Some(t)) => {
                let window = t.window();
                match window.firstResponder().and_then(|r| r.downcast::<objc2_app_kit::NSTextView>().ok()) {
                    Some(editor) => {
                        let _: () = unsafe { msg_send![&*editor, doCommandBySelector: sel!(cancelOperation:)] };
                    }
                    None => t.hide_find(),
                }
                say!("{}", t.find_state());
            }
            // (`\n` in an argument is a line break: commands are one line each.)
            ("bridge", _) => internal::handle(
                self,
                arg.split('\t').map(|a| a.replace("\\n", "\n")).collect(),
                Box::new(|r| match r {
                    Ok(json) => say!("BRIDGE ok {json}"),
                    Err(e) => say!("BRIDGE err {e}"),
                }),
            ),
            ("quit", _) => self.quit(),
            // Tests the crash reporter.
            ("crash", _) => panic!("crash requested by the bench harness"),
            _ => say!("ERR unknown command: {cmd}"),
        }
    }

    /// Cmd+G / Shift+Cmd+G: repeat the search, opening the bar if it's closed.
    fn find_in(&self, tab: &Tab, backwards: bool) {
        if !tab.is_find_open() {
            tab.show_find();
        }
        tab.find(backwards);
    }

    fn tabs_line(&self) -> String {
        let tabs = self.ordered_tabs();
        let active = tabs.iter().position(|t| self.is_active(t)).map_or(-1, |i| i as i64);
        let all = self.ivars().tabs.borrow();
        let asleep = all.iter().filter(|t| t.is_asleep()).count();
        let mut groups: Vec<usize> = Vec::new();
        for t in all.iter() {
            let first = t.window().tabbedWindows().and_then(|ws| ws.firstObject());
            let id = first.map_or(t.window() as *const NSWindow as usize, |w| Retained::as_ptr(&w) as usize);
            if !groups.contains(&id) {
                groups.push(id);
            }
        }
        format!("TABS count={} active={active} asleep={asleep} windows={}", tabs.len(), groups.len())
    }

    /// Simulate a key press like `cmd+shift+t` through the menu bar's real
    /// shortcut matching. True if a menu item took it.
    fn press(&self, combo: &str) -> bool {
        let parts: Vec<&str> = combo.split('+').collect();
        let Some((key, mods)) = parts.split_last() else { return false };
        let mut flags = NSEventModifierFlags::empty();
        for m in mods {
            flags |= match *m {
                "cmd" => NSEventModifierFlags::Command,
                "shift" => NSEventModifierFlags::Shift,
                "alt" => NSEventModifierFlags::Option,
                "ctrl" => NSEventModifierFlags::Control,
                _ => return false,
            };
        }
        let key = if *key == "tab" { "\t".to_string() } else { key.to_string() };
        // What a US keyboard produces with Shift held.
        let chars = if flags.contains(NSEventModifierFlags::Shift) {
            match key.as_str() {
                "[" => "{".to_string(),
                "]" => "}".to_string(),
                "=" => "+".to_string(),
                k => k.to_uppercase(),
            }
        } else {
            key
        };
        let chars = NSString::from_str(&chars);
        let window_number = self.active().map_or(0, |t| t.window().windowNumber());
        let event = NSEvent::keyEventWithType_location_modifierFlags_timestamp_windowNumber_context_characters_charactersIgnoringModifiers_isARepeat_keyCode(
                NSEventType::KeyDown,
                NSPoint::new(0.0, 0.0),
                flags,
                0.0,
                window_number,
                None,
                &chars,
                &chars,
                false,
                0,
            );
        let app = NSApplication::sharedApplication(self.mtm());
        match (event, app.mainMenu()) {
            (Some(e), Some(menu)) => menu.performKeyEquivalent(&e),
            _ => false,
        }
    }
}

fn key(download: &WKDownload) -> usize {
    download as *const WKDownload as usize
}

/// Value after a `--flag`.
fn arg_value(flag: &str) -> Option<String> {
    let args: Vec<String> = std::env::args().collect();
    args.iter().position(|a| a == flag).and_then(|i| args.get(i + 1)).cloned()
}

/// URLs to open from the command line: bare arguments that aren't a flag's value.
fn url_args() -> Vec<String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut out = Vec::new();
    let mut skip = false;
    for a in args {
        if std::mem::take(&mut skip) {
            continue;
        }
        if a == "--profile" {
            skip = true;
        } else if !a.starts_with("--") {
            out.push(a);
        }
    }
    out
}

/// The setting that turns each shared content blocker on ("": always on).
const SHIELD_SETTINGS: [&str; 4] = ["block_trackers", "block_local_network", "hide_cookie_banners", ""];

static PASSWORDS_JS: &str = include_str!("../resources/passwords.js");

/// Schemes the browser opens for other apps.
fn opens_externally(url: &str) -> bool {
    let scheme = url.split(':').next().unwrap_or("").to_ascii_lowercase();
    matches!(scheme.as_str(), "http" | "https" | "file")
}

/// Run `f` on the main thread soon. For completion handlers that macOS calls
/// on another queue: whatever `f` captured is only touched on the main thread.
pub fn on_main(f: impl FnOnce() + 'static) {
    unsafe extern "C" {
        static _dispatch_main_q: std::ffi::c_void;
        fn dispatch_async_f(
            queue: *const std::ffi::c_void,
            context: *mut std::ffi::c_void,
            work: extern "C" fn(*mut std::ffi::c_void),
        );
    }
    extern "C" fn run(context: *mut std::ffi::c_void) {
        let f = unsafe { Box::from_raw(context as *mut Box<dyn FnOnce()>) };
        f();
    }
    let f: Box<Box<dyn FnOnce()>> = Box::new(Box::new(f));
    unsafe { dispatch_async_f(&raw const _dispatch_main_q, Box::into_raw(f).cast(), run) };
}

/// A value carried through another thread to the main thread without being
/// touched there: for callbacks macOS runs on its own queues, whose result
/// goes back to `on_main`. Only use `.0` on the main thread.
pub struct MainOnly<T>(pub T);
unsafe impl<T> Send for MainOnly<T> {}

/// Characters that make a name display differently from what it is:
/// right-to-left overrides turn "photo\u{202E}gpj.app" into "photoppa.jpg".
fn is_spoofing_char(c: char) -> bool {
    matches!(c, '\u{200E}' | '\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}' | '\u{061C}' | '\u{FEFF}')
}

/// Keep only a plain file name: no directories, no hidden-file tricks, no
/// direction overrides.
fn sanitize_filename(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or("");
    let clean: String = base
        .chars()
        .filter(|c| !is_spoofing_char(*c))
        .map(|c| if c == ':' || c.is_control() { '_' } else { c })
        .collect();
    let clean = clean.trim().trim_start_matches('.').to_string();
    if clean.is_empty() { "download".into() } else { clean }
}

/// `dir/name`, or `dir/name (2).ext` etc. if taken.
fn unique_path(dir: &Path, name: &str) -> PathBuf {
    let candidate = dir.join(name);
    if !candidate.exists() {
        return candidate;
    }
    let (stem, ext) = match name.rsplit_once('.') {
        Some((s, e)) if !s.is_empty() => (s.to_string(), format!(".{e}")),
        _ => (name.to_string(), String::new()),
    };
    (2..).map(|n| dir.join(format!("{stem} ({n}){ext}"))).find(|p| !p.exists()).unwrap()
}

/// One running browser per profile: two would write the same cookie and
/// history files and corrupt them.
fn lock_profile(dir: &Path) -> Result<std::fs::File, String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("Can't create the profile folder {}: {e}", dir.display()))?;
    // History, logins' metadata and extensions: readable by this user only.
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
    let file = std::fs::File::create(dir.join("lock")).map_err(|e| format!("Can't lock the profile: {e}"))?;
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(_) => Err("my-browser is already running with this profile.".into()),
    }
}

fn fatal(mtm: MainThreadMarker, message: &str) -> ! {
    eprintln!("{message}");
    if !std::env::args().any(|a| a == "--bench") {
        let alert = NSAlert::new(mtm);
        alert.setMessageText(&NSString::from_str(message));
        alert.runModal();
    }
    std::process::exit(1);
}

fn memory_pressure() -> bool {
    unsafe extern "C" {
        fn sysctlbyname(
            name: *const i8,
            old: *mut std::ffi::c_void,
            oldlen: *mut usize,
            new: *mut std::ffi::c_void,
            newlen: usize,
        ) -> i32;
    }
    let mut level: i32 = 1;
    let mut len = std::mem::size_of::<i32>();
    let name = c"kern.memorystatus_vm_pressure_level";
    unsafe { sysctlbyname(name.as_ptr(), (&raw mut level).cast(), &mut len, std::ptr::null_mut(), 0) };
    level > 1
}

/// Low Power Mode is on, or the Mac is hot enough to throttle itself: a
/// background page's timers cost battery the user is trying to save.
fn saving_power() -> bool {
    let info = objc2_foundation::NSProcessInfo::processInfo();
    info.isLowPowerModeEnabled() || info.thermalState().0 >= objc2_foundation::NSProcessInfoThermalState::Serious.0
}

/// How long a background tab stays awake: the setting (0 = never), cut to
/// one minute while saving power.
fn sleep_after(minutes: u64, saving: bool) -> Option<Duration> {
    let minutes = if saving { minutes.min(1) } else { minutes };
    (minutes > 0).then(|| Duration::from_secs(minutes * 60))
}

/// Turn address-bar input into a URL: keep explicit schemes, add https:// to
/// things that look like hosts, send everything else to the search engine.
fn normalize(input: &str, search: &str) -> String {
    let s = input.trim();
    if s.contains("://") || s.starts_with("about:") || s.starts_with("data:") {
        s.to_string()
    } else if is_address(s) {
        // This Mac and the local network rarely have HTTPS: plain HTTP there.
        let host = store::site_of(&format!("http://{s}")).unwrap_or_default();
        if privacy::is_local_host(&host) { format!("http://{s}") } else { format!("https://{s}") }
    } else {
        let base = SEARCH_ENGINES.iter().find(|(name, _)| *name == search).map_or(SEARCH_ENGINES[0].1, |(_, b)| b);
        format!("{base}{}", encode_query(s))
    }
}

/// Search engines and their result addresses; the first is the default.
pub const SEARCH_ENGINES: [(&str, &str); 4] = [
    ("duckduckgo", "https://duckduckgo.com/?q="),
    ("brave", "https://search.brave.com/search?q="),
    ("startpage", "https://www.startpage.com/do/search?q="),
    ("google", "https://www.google.com/search?q="),
];

/// Built-in bangs: `!w rust` or `rust !w` searches Wikipedia. All off until
/// the user turns them on (setting `bangs`). Turned into an address here, so
/// the search engine never sees them. `%s` is the query.
pub const BANGS: [(&str, &str); 17] = [
    ("a", "https://www.amazon.com/s?k=%s"),
    ("b", "https://search.brave.com/search?q=%s"),
    ("crates", "https://crates.io/search?q=%s"),
    ("ddg", "https://duckduckgo.com/?q=%s"),
    ("g", "https://www.google.com/search?q=%s"),
    ("gh", "https://github.com/search?q=%s"),
    ("imdb", "https://www.imdb.com/find/?q=%s"),
    ("maps", "https://maps.apple.com/?q=%s"),
    ("mdn", "https://developer.mozilla.org/search?q=%s"),
    ("npm", "https://www.npmjs.com/search?q=%s"),
    ("r", "https://www.reddit.com/search/?q=%s"),
    ("rs", "https://docs.rs/releases/search?query=%s"),
    ("so", "https://stackoverflow.com/search?q=%s"),
    ("sp", "https://www.startpage.com/do/search?q=%s"),
    ("w", "https://en.wikipedia.org/wiki/Special:Search?search=%s"),
    ("wa", "https://www.wolframalpha.com/input?i=%s"),
    ("yt", "https://www.youtube.com/results?search_query=%s"),
];

/// The address for a keyword search: `!kw query` or `query !kw` with a
/// built-in bang that's on (`bangs`, comma-separated) or one of the user's
/// keywords, or `kw query` with one of the user's keywords (Settings →
/// General). None if it isn't one.
fn keyword_url(input: &str, custom: &[(String, String)], bangs: &str) -> Option<String> {
    let s = input.trim();
    let words: Vec<&str> = s.split_whitespace().collect();
    let (kw, query) = match words.as_slice() {
        [first, rest @ ..] if first.len() > 1 && first.starts_with('!') => (&first[1..], rest.to_vec()),
        [rest @ .., last] if !rest.is_empty() && last.len() > 1 && last.starts_with('!') => (&last[1..], rest.to_vec()),
        [first, rest @ ..] if !rest.is_empty() && custom.iter().any(|(k, _)| k == first) => (*first, rest.to_vec()),
        _ => return None,
    };
    let kw = kw.to_lowercase();
    let template = custom
        .iter()
        .find(|(k, _)| *k == kw)
        .map(|(_, u)| u.as_str())
        .or_else(|| BANGS.iter().find(|(k, _)| *k == kw && bangs.split(',').any(|b| b == kw)).map(|(_, u)| *u))?;
    // `+` for spaces only works in queries; %20 works in paths too.
    Some(template.replace("%s", &encode_query(&query.join(" ")).replace('+', "%20")))
}

/// Whether address-bar input is an address rather than a search.
pub fn is_address(input: &str) -> bool {
    let s = input.trim();
    s.contains("://")
        || s.starts_with("about:")
        || s.starts_with("data:")
        || (!s.contains(' ') && (s.contains('.') || s.starts_with("localhost")))
}

fn encode_query(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => (b as char).to_string(),
            b' ' => "+".to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// Read `name` from a `MYB_TUNE`-style spec (`name=0|1,...`), else `default`.
fn tune_value(spec: Option<&str>, name: &str, default: bool) -> bool {
    spec.and_then(|spec| {
        spec.split(',').find_map(|kv| {
            let (k, v) = kv.split_once('=')?;
            (k.trim() == name).then(|| v.trim() == "1")
        })
    })
    .unwrap_or(default)
}

fn tune_line(name: &str, value: bool, ok: bool) -> String {
    format!("TUNE {name}={} {}", value as u8, if ok { "ok" } else { "missing" })
}

/// Whether WebKit still declares this optional UI-delegate method. It only
/// calls the methods it knows, so a renamed one would fail silently.
fn webkit_declares(method: Sel) -> bool {
    let Some(protocol) = objc2::runtime::AnyProtocol::get(c"WKUIDelegatePrivate") else { return false };
    let found = unsafe { objc2::ffi::protocol_getMethodDescription(protocol, method, Bool::NO, Bool::YES) };
    found.name.is_some()
}

fn responds(obj: &AnyObject, sel: Sel) -> bool {
    unsafe { msg_send![obj, respondsToSelector: sel] }
}

/// Engine settings that trade unused features for memory. Defaults are the
/// benchmarked winners (benchmark/README.md); `MYB_TUNE=name=0|1,...` overrides
/// them for experiments. These are private WebKit APIs: each one is skipped if
/// this macOS doesn't have it. Returns the process pool to use and
/// (name, value, applied) for each setting.
/// (name, value, applied) for one engine setting.
type Tuning = (&'static str, bool, bool);

#[allow(deprecated)] // WKProcessPool: the only way to pass the private pool configuration.
fn engine_tuning(
    prefs: &WKPreferences,
    jit_default: bool,
    mtm: MainThreadMarker,
) -> (Option<Retained<WKProcessPool>>, Vec<Tuning>) {
    let spec = std::env::var("MYB_TUNE").ok();
    let tune = |name, default| tune_value(spec.as_deref(), name, default);
    let mut report = Vec::new();

    // Back/forward cache: keeps whole pages alive after you leave them. Off:
    // session peak 1816 -> 299 MB in the cross-site bench session.
    let pagecache = tune("pagecache", false);
    let ok = responds(prefs, sel!(_setUsesPageCache:));
    if ok {
        let _: () = unsafe { msg_send![prefs, _setUsesPageCache: Bool::new(pagecache)] };
    }
    report.push(("pagecache", pagecache, ok));

    let proccache = tune("proccache", true);
    // Off: one fewer process and ~9 MB less at idle.
    let prewarm = tune("prewarm", false);
    let suspend = tune("suspend", false);
    let jit = tune("jit", jit_default);
    let cls = AnyClass::get(c"_WKProcessPoolConfiguration");
    let pool_ok = cls.is_some() && WKProcessPool::class().responds_to(sel!(_initWithConfiguration:));
    let cfg: Option<Retained<AnyObject>> = pool_ok.then(|| unsafe { msg_send![cls.unwrap(), new] });
    let mut set = |name, setter: Sel, value: bool| {
        let ok = cfg.as_deref().is_some_and(|c| responds(c, setter));
        if let (true, Some(c)) = (ok, cfg.as_deref()) {
            // Every setter here takes one BOOL.
            let _: () = unsafe { objc2::runtime::MessageReceiver::send_message(c, setter, (Bool::new(value),)) };
        }
        report.push((name, value, ok));
    };
    // Keeps exited pages' processes around for reuse.
    set("proccache", sel!(setUsesWebProcessCache:), proccache);
    // Starts a spare page process ahead of time.
    set("prewarm", sel!(setPrewarmsProcessesAutomatically:), prewarm);
    // Suspends background page processes when memory runs short.
    set("suspend", sel!(setSuspendsWebProcessesAggressivelyOnMemoryPressure:), suspend);
    // JavaScript JIT: faster JS, more memory.
    set("jit", sel!(setJITEnabled:), jit);
    let pool = cfg.map(|cfg| unsafe { msg_send![WKProcessPool::alloc(mtm), _initWithConfiguration: &*cfg] });
    (pool, report)
}

fn menu_item(
    mtm: MainThreadMarker,
    title: &str,
    action: Sel,
    key: &str,
    mods: NSEventModifierFlags,
) -> Retained<NSMenuItem> {
    let item = unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            NSMenuItem::alloc(mtm),
            &NSString::from_str(title),
            Some(action),
            &NSString::from_str(key),
        )
    };
    item.setKeyEquivalentModifierMask(mods);
    item
}

/// The menu bar holds every keyboard shortcut. Items marked `true` go to the
/// app delegate; `false` ones go to the first responder (text editing, quit).
fn build_menu(app: &NSApplication, delegate: &App, mtm: MainThreadMarker) {
    let cmd = NSEventModifierFlags::Command;
    let cmd_alt = cmd | NSEventModifierFlags::Option;
    let ctrl = NSEventModifierFlags::Control;
    let ctrl_shift = ctrl | NSEventModifierFlags::Shift;
    let cmd_shift = cmd | NSEventModifierFlags::Shift;
    let none = NSEventModifierFlags::empty();
    // An uppercase key or a shifted symbol ("{") implies Shift.
    type Item = (&'static str, Sel, &'static str, NSEventModifierFlags, bool);
    let submenus: Vec<(&str, Vec<Item>)> = vec![
        (
            "my-browser",
            vec![
                ("About my-browser", sel!(showAbout:), "", none, true),
                ("Settings…", sel!(openSettings:), ",", cmd, true),
                ("Check for Updates…", sel!(checkForUpdates:), "", none, true),
                ("Quit my-browser", sel!(quit:), "q", cmd, true),
            ],
        ),
        (
            "File",
            vec![
                ("New Tab", sel!(newTab:), "t", cmd, true),
                ("New Window", sel!(newWindow:), "n", cmd, true),
                ("New Private Window", sel!(newPrivateWindow:), "N", cmd, true),
                ("Reopen Closed Tab", sel!(reopenClosedTab:), "T", cmd, true),
                ("Open Location", sel!(focusAddressBar:), "l", cmd, true),
                ("Close Tab", sel!(closeTab:), "w", cmd, true),
                ("Duplicate Tab", sel!(duplicateTab:), "", none, true),
                ("Close Other Tabs", sel!(closeOtherTabs:), "w", cmd_alt, true),
                ("Save Page As…", sel!(savePage:), "s", cmd, true),
                ("Print…", sel!(printPage:), "p", cmd, true),
            ],
        ),
        (
            "Edit",
            vec![
                ("Undo", sel!(undo:), "z", cmd, false),
                ("Redo", sel!(redo:), "Z", cmd, false),
                ("Cut", sel!(cut:), "x", cmd, false),
                ("Copy", sel!(copy:), "c", cmd, false),
                ("Paste", sel!(paste:), "v", cmd, false),
                ("Select All", sel!(selectAll:), "a", cmd, false),
                ("AutoFill Password", sel!(offerPasswords:), "a", cmd_alt, true),
                ("Find…", sel!(showFind:), "f", cmd, true),
                ("Find Next", sel!(findNext:), "g", cmd, true),
                ("Find Previous", sel!(findPrevious:), "G", cmd, true),
            ],
        ),
        (
            "View",
            vec![
                ("Reload Page", sel!(reload:), "r", cmd, true),
                ("Stop", sel!(stop:), ".", cmd, true),
                ("Zoom In", sel!(zoomIn:), "+", cmd, true),
                ("Zoom In", sel!(zoomIn:), "=", cmd, true),
                ("Zoom Out", sel!(zoomOut:), "-", cmd, true),
                ("Actual Size", sel!(actualSize:), "0", cmd, true),
                ("Picture in Picture", sel!(pictureInPicture:), "p", cmd_alt, true),
                ("Reader", sel!(toggleReader:), "R", cmd, true),
                ("Translate Page", sel!(translatePage:), "t", cmd_alt, true),
                ("Hide Element on Page…", sel!(pickElement:), "", none, true),
                ("Show Web Inspector", sel!(showInspector:), "i", cmd_alt, true),
            ],
        ),
        (
            "History",
            vec![
                ("Back", sel!(goBack:), "[", cmd, true),
                ("Forward", sel!(goForward:), "]", cmd, true),
                ("Show History", sel!(showHistory:), "y", cmd, true),
                ("Downloads", sel!(showDownloads:), "l", cmd_alt, true),
            ],
        ),
        (
            "Bookmarks",
            vec![
                ("Bookmark This Page", sel!(toggleBookmark:), "d", cmd, true),
                ("Show All Bookmarks", sel!(showBookmarks:), "b", cmd_alt, true),
            ],
        ),
        // Then every extension's keyboard commands (`rebuild_extension_menu`).
        ("Extensions", vec![("Manage Extensions…", sel!(showExtensions:), "", none, true)]),
        (
            "Window",
            vec![
                ("Minimize", sel!(minimizeWindow:), "m", cmd, true),
                ("Zoom", sel!(zoomWindow:), "", none, true),
                ("Show Previous Tab", sel!(previousTab:), "{", cmd, true),
                ("Show Next Tab", sel!(nextTab:), "}", cmd, true),
                ("Next Tab", sel!(nextTab:), "\t", ctrl, true),
                ("Previous Tab", sel!(previousTab:), "\t", ctrl_shift, true),
                ("Show All Tabs", sel!(toggleTabOverview:), "\\", cmd_shift, true),
                ("Search Tabs…", sel!(searchTabs:), "A", cmd, true),
                ("Pin Tab", sel!(pinTab:), "", none, true),
                ("Mute Tab", sel!(muteTab:), "", none, true),
                ("Move Tab to New Window", sel!(moveTabToNewWindow:), "", none, true),
                ("Merge All Windows", sel!(mergeAllWindows:), "", none, true),
            ],
        ),
    ];
    let main = NSMenu::new(mtm);
    for (title, items) in submenus {
        let menu = NSMenu::initWithTitle(NSMenu::alloc(mtm), &NSString::from_str(title));
        for (t, action, key, mods, to_app) in items {
            let item = menu_item(mtm, t, action, key, mods);
            if to_app {
                unsafe { item.setTarget(Some(delegate)) };
            }
            menu.addItem(&item);
        }
        if title == "File" {
            // Take Screenshot, before Print.
            let shots = delegate.screenshot_menu(true);
            shots.setDelegate(Some(ProtocolObject::from_ref(delegate)));
            let holder = menu_item(mtm, "Take Screenshot", sel!(submenuAction:), "", none);
            holder.setSubmenu(Some(&shots));
            menu.insertItem_atIndex(&holder, menu.indexOfItemWithTitle(&NSString::from_str("Print…")));
            // Filled when it opens (`fill_profiles_menu`).
            let profiles = NSMenu::initWithTitle(NSMenu::alloc(mtm), &NSString::from_str("Profiles"));
            profiles.setDelegate(Some(ProtocolObject::from_ref(delegate)));
            let holder = menu_item(mtm, "Profiles", sel!(submenuAction:), "", none);
            holder.setSubmenu(Some(&profiles));
            menu.addItem(&NSMenuItem::separatorItem(mtm));
            menu.addItem(&holder);
        }
        if title == "Edit" {
            // To the first responder (the page, or a text field), like Safari's.
            // Not in Settings → Shortcuts: it lists top-level items only.
            let spelling = NSMenu::initWithTitle(NSMenu::alloc(mtm), &NSString::from_str("Spelling and Grammar"));
            for (t, action, key) in [
                ("Show Spelling and Grammar", sel!(showGuessPanel:), ":"),
                ("Check Document Now", sel!(checkSpelling:), ";"),
                ("Check Spelling While Typing", sel!(toggleContinuousSpellChecking:), ""),
                ("Check Grammar With Spelling", sel!(toggleGrammarChecking:), ""),
                ("Correct Spelling Automatically", sel!(toggleAutomaticSpellingCorrection:), ""),
            ] {
                spelling.addItem(&menu_item(mtm, t, action, key, if key.is_empty() { none } else { cmd }));
            }
            let holder = menu_item(mtm, "Spelling and Grammar", sel!(submenuAction:), "", none);
            holder.setSubmenu(Some(&spelling));
            menu.addItem(&NSMenuItem::separatorItem(mtm));
            menu.addItem(&holder);
        }
        if title == "Window" {
            menu.addItem(&NSMenuItem::separatorItem(mtm));
            for n in 1..=9isize {
                let name = if n == 9 { "Last Tab".to_string() } else { format!("Tab {n}") };
                let item = menu_item(mtm, &name, sel!(selectTabByNumber:), &n.to_string(), cmd);
                item.setTag(n);
                unsafe { item.setTarget(Some(delegate)) };
                menu.addItem(&item);
            }
        }
        let holder = NSMenuItem::new(mtm);
        holder.setSubmenu(Some(&menu));
        main.addItem(&holder);
        if title == "Window" {
            // Lets AppKit list open windows and handle Window-menu conventions.
            app.setWindowsMenu(Some(&menu));
        }
        if ["Bookmarks", "History", "Window", "View"].contains(&title) {
            menu.setDelegate(Some(ProtocolObject::from_ref(delegate)));
        }
    }
    app.setMainMenu(Some(&main));
}

fn main() {
    let mtm = MainThreadMarker::new().expect("must run on main thread");
    let app = NSApplication::sharedApplication(mtm);
    let bench = std::env::args().any(|a| a == "--bench");
    let on_top = std::env::args().any(|a| a == "--on-top");
    // Floating over other apps' full-screen Spaces is only allowed for accessory
    // (no Dock icon) apps. Such windows can't be minimized.
    app.setActivationPolicy(if on_top {
        NSApplicationActivationPolicy::Accessory
    } else {
        NSApplicationActivationPolicy::Regular
    });
    // Spelling is checked as you type, as in Safari (WebKit's default is
    // off). Edit → Spelling and Grammar changes it; WebKit keeps the choice.
    let defaults = objc2_foundation::NSUserDefaults::standardUserDefaults();
    let on: Retained<AnyObject> = objc2_foundation::NSNumber::new_bool(true).into();
    let key = NSString::from_str("WebContinuousSpellCheckingEnabled");
    unsafe { defaults.registerDefaults(&objc2_foundation::NSDictionary::from_retained_objects(&[&*key], &[on])) };
    let delegate = App::new(mtm);
    delegate.ivars().bench.set(bench);
    delegate.ivars().on_top.set(on_top);
    build_menu(&app, &delegate, mtm);
    app.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
    // Benchmarks and tests run in the background: an active test browser
    // took the owner's typing, and when it quit macOS handed activation to
    // another test browser (which then failed `bench_window_is_on_screen`).
    if !bench {
        #[allow(deprecated)]
        app.activateIgnoringOtherApps(true);
    }
    app.run();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_address_bar_input() {
        assert_eq!(normalize("example.com", "duckduckgo"), "https://example.com");
        assert_eq!(normalize(" http://a.b/c ", "duckduckgo"), "http://a.b/c");
        assert_eq!(normalize("localhost:8080", "duckduckgo"), "http://localhost:8080", "local: no https");
        assert_eq!(normalize("192.168.1.1/admin", "duckduckgo"), "http://192.168.1.1/admin");
        assert_eq!(normalize("printer.local", "duckduckgo"), "http://printer.local");
        assert_eq!(normalize("localhost.example.com", "duckduckgo"), "https://localhost.example.com");
        assert_eq!(normalize("about:blank", "duckduckgo"), "about:blank");
        assert_eq!(normalize("rust lang & c", "duckduckgo"), "https://duckduckgo.com/?q=rust+lang+%26+c");
        assert_eq!(normalize("rust", "brave"), "https://search.brave.com/search?q=rust");
        assert_eq!(normalize("rust", "google"), "https://www.google.com/search?q=rust");
        assert_eq!(normalize("rust", "startpage"), "https://www.startpage.com/do/search?q=rust");
        assert!(is_address("example.com") && is_address("http://x") && !is_address("rust book") && !is_address("rust"));
    }

    #[test]
    fn keywords_and_bangs_search_other_sites() {
        let custom = [("w".to_string(), "https://wiki.example/find/%s".to_string())];
        assert_eq!(
            keyword_url("!yt rust talks", &[], "yt,gh,w").as_deref(),
            Some("https://www.youtube.com/results?search_query=rust%20talks")
        );
        assert_eq!(
            keyword_url("rust & c !GH", &[], "yt,gh,w").as_deref(),
            Some("https://github.com/search?q=rust%20%26%20c")
        );
        assert_eq!(
            keyword_url("!w rust", &custom, "").as_deref(),
            Some("https://wiki.example/find/rust"),
            "the user's keyword wins"
        );
        assert_eq!(
            keyword_url("w rust", &custom, "").as_deref(),
            Some("https://wiki.example/find/rust"),
            "no ! needed for their own"
        );
        assert_eq!(keyword_url("w rust", &[], "yt,gh,w"), None, "built-in bangs need the !");
        assert_eq!(keyword_url("w", &custom, ""), None, "a keyword alone is a search");
        assert_eq!(keyword_url("!nope rust", &[], "yt,gh,w"), None, "unknown bangs go to the search engine");
        assert_eq!(keyword_url("https://a.example/!w", &[], "yt,gh,w"), None);
        assert_eq!(keyword_url("hello !", &[], "yt,gh,w"), None);
        assert_eq!(keyword_url("!yt rust", &[], ""), None, "built-in bangs are off until turned on");
        assert_eq!(keyword_url("!yt rust", &[], "w"), None);
    }

    #[test]
    fn tabs_sleep_sooner_while_saving_power() {
        assert_eq!(sleep_after(5, false), Some(Duration::from_secs(300)));
        assert_eq!(sleep_after(5, true), Some(Duration::from_secs(60)));
        assert_eq!(sleep_after(0, true), None, "never means never");
        assert_eq!(sleep_after(0, false), None);
    }

    #[test]
    fn only_web_pages_and_files_open_from_other_apps() {
        for ok in ["https://a.example/", "HTTP://a.example", "file:///Users/x/page.html"] {
            assert!(opens_externally(ok), "{ok}");
        }
        for bad in ["javascript:alert(1)", "mybrowser://settings", "mailto:a@b", "data:text/html,x", ""] {
            assert!(!opens_externally(bad), "{bad}");
        }
    }

    #[test]
    fn parses_tune_spec() {
        assert!(tune_value(None, "jit", true));
        assert!(!tune_value(Some("jit=0"), "jit", true));
        assert!(tune_value(Some("prewarm=0, jit = 1"), "jit", false));
        assert!(!tune_value(Some("pagecache=0"), "page", false));
        assert!(tune_value(Some("garbage"), "jit", true));
    }

    #[test]
    fn download_names_are_safe_and_unique() {
        assert_eq!(sanitize_filename("../../etc/passwd"), "passwd");
        assert_eq!(sanitize_filename("..\\x.exe"), "x.exe");
        assert_eq!(sanitize_filename(".hidden"), "hidden");
        assert_eq!(sanitize_filename("a:b\u{7}.txt"), "a_b_.txt");
        assert_eq!(sanitize_filename("   "), "download");
        assert_eq!(sanitize_filename("photo\u{202E}gpj.app"), "photogpj.app", "no right-to-left tricks");
        assert_eq!(sanitize_filename("\u{2067}x.txt"), "x.txt");
        let dir = std::env::temp_dir().join(format!("mybrowser-dl-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(unique_path(&dir, "f.zip"), dir.join("f.zip"));
        std::fs::write(dir.join("f.zip"), b"").unwrap();
        assert_eq!(unique_path(&dir, "f.zip"), dir.join("f (2).zip"));
        std::fs::write(dir.join("f (2).zip"), b"").unwrap();
        assert_eq!(unique_path(&dir, "f.zip"), dir.join("f (3).zip"));
        std::fs::write(dir.join("README"), b"").unwrap();
        assert_eq!(unique_path(&dir, "README"), dir.join("README (2)"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn profile_lock_is_exclusive() {
        let dir = std::env::temp_dir().join(format!("mybrowser-lock-{}", std::process::id()));
        let first = lock_profile(&dir).unwrap();
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777, 0o700, "private to this user");
        assert!(lock_profile(&dir).is_err(), "second lock must fail");
        drop(first);
        assert!(lock_profile(&dir).is_ok(), "lock is released with the file");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
