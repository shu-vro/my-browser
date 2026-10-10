//! One browser tab: a native macOS window tab with its own toolbar and web view.
//!
//! A tab can be asleep: its web view (and the page process behind it) is
//! destroyed and only the URL and title are kept. Waking reloads the page.
//! Background tabs fall asleep after a while (see App::sleep_background_tabs).

use std::cell::{Cell, OnceCell, RefCell};
use std::ffi::c_void;
use std::ptr::NonNull;
use std::time::Instant;

use block2::{DynBlock, RcBlock};
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, Bool, ProtocolObject};
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, Message, define_class, msg_send, sel};
use objc2_app_kit::{
    NSAlert, NSApplication, NSAutoresizingMaskOptions, NSBackingStoreType, NSButton, NSControl,
    NSControlTextEditingDelegate, NSEventModifierFlags, NSFloatingWindowLevel, NSImage, NSMenu, NSMenuItem,
    NSModalResponseOK, NSOpenPanel, NSSearchField, NSSearchFieldDelegate, NSTextField, NSTextFieldDelegate, NSTextView,
    NSView, NSWindow, NSWindowCollectionBehavior, NSWindowDelegate, NSWindowStyleMask, NSWindowTabbingMode,
};
use objc2_foundation::{
    NSArray, NSError, NSKeyValueObservingOptions, NSNotification, NSObject, NSObjectNSKeyValueObserverRegistration,
    NSObjectProtocol, NSPoint, NSRect, NSSize, NSString, NSURL, NSURLRequest,
};
use objc2_web_kit::{
    WKContentWorld, WKDownload, WKFindConfiguration, WKFindResult, WKFrameInfo, WKMediaCaptureType, WKNavigation,
    WKNavigationAction, WKNavigationActionPolicy, WKNavigationDelegate, WKNavigationResponse,
    WKNavigationResponsePolicy, WKNavigationType, WKOpenPanelParameters, WKPermissionDecision, WKSecurityOrigin,
    WKUIDelegate, WKWebExtensionContext, WKWebExtensionTab, WKWebExtensionTabChangedProperties, WKWebExtensionWindow,
    WKWebView, WKWebViewConfiguration, WKWebpagePreferences, WKWindowFeatures,
};

use crate::App;

/// Height of the toolbar row (buttons + address bar) above the page.
const TOOLBAR_H: f64 = 32.0;
/// Height of the find bar under the toolbar, while it's open.
const FIND_H: f64 = 30.0;
const BUTTON_W: f64 = 30.0;
/// WKWebView properties whose changes refresh the toolbar.
const OBSERVED: [&str; 5] = ["URL", "title", "canGoBack", "canGoForward", "loading"];
/// Scheme of the browser's own pages (settings, history, downloads, extensions).
pub const INTERNAL: &str = "mybrowser";

#[derive(Default)]
pub struct TabIvars {
    app: OnceCell<Retained<App>>,
    window: OnceCell<Retained<NSWindow>>,
    bar: OnceCell<Retained<NSTextField>>,
    back: OnceCell<Retained<NSButton>>,
    forward: OnceCell<Retained<NSButton>>,
    reload: OnceCell<Retained<NSButton>>,
    home_button: OnceCell<Retained<NSButton>>,
    /// None while asleep (or before the first load).
    web: RefCell<Option<Retained<WKWebView>>>,
    /// Last known URL and title; what an asleep tab shows and reloads.
    url: RefCell<String>,
    title: RefCell<String>,
    pub last_active: Cell<Option<Instant>>,
    /// History row of the current page, so a late title can be filled in.
    visit: Cell<Option<i64>>,
    shown_loading: Cell<Option<bool>>,
    /// When the pending bench navigation started.
    pub nav_start: Cell<Option<Instant>>,
    /// Web pages may not navigate to internal pages. The internal page the browser itself is loading; allowed until it
    /// commits or fails (WebKit may ask about one navigation more than once).
    allow_internal: RefCell<Option<String>>,
    /// Find bar (Cmd+F): built on first use.
    find_row: OnceCell<Retained<NSView>>,
    find_field: OnceCell<Retained<NSSearchField>>,
    find_status: OnceCell<Retained<NSTextField>>,
    find_open: Cell<bool>,
    /// The term last searched for; a new term searches from the top.
    find_last: RefCell<String>,
    /// Bench: print the next find result.
    pub find_report: Cell<bool>,
    /// Site button left of the address bar: security state and the site menu.
    site_button: OnceCell<Retained<NSButton>>,
    shown_site_symbol: Cell<&'static str>,
    /// JavaScript dialogs on the current page, and whether the user blocked more.
    dialogs: Cell<u32>,
    dialogs_blocked: Cell<bool>,
    /// The sheet on screen (answered by the bench `dialog` command in tests).
    alert: RefCell<Option<Retained<NSAlert>>>,
    /// Recent page-process crashes: (count, last), to stop a crash-reload loop.
    crashes: Cell<(u32, Option<Instant>)>,
    /// The next finished load is our error page, not a visit.
    error_page: Cell<bool>,
    /// Extension buttons at the right of the toolbar, and (id, on) for each.
    ext_buttons: RefCell<Vec<(Retained<NSButton>, bool)>>,
    /// Shown while a network request waits for an answer (netgate.rs).
    net_button: RefCell<Option<Retained<NSButton>>>,
    /// Extensions clicked on in this tab: (extension id, site). They turn off
    /// when the tab leaves that site.
    pub ext_on: RefCell<Vec<(i64, String)>>,
    /// An extension waiting for the reload its click caused, to open its popup.
    pub pending_ext_action: Cell<Option<i64>>,
    /// An address the user (or the browser) asked for, shown until it commits.
    requested: RefCell<Option<String>>,
    /// Downloads this page started; the second one needs the site's permission.
    page_downloads: Cell<u32>,
    /// The native new tab page, while the tab has no page.
    home: RefCell<Option<Retained<NSView>>>,
    /// What it shows, for the bench `home` command.
    home_state: RefCell<String>,
}

/// Permissions a site asks for: (kind, site menu label, what the prompt says it wants to do).
const SITE_PERMISSIONS: [(&str, &str, &str); 4] = [
    ("camera", "Camera", "use your camera"),
    ("microphone", "Microphone", "use your microphone"),
    ("location", "Location", "see your location"),
    ("downloads", "Automatic downloads", "download multiple files"),
];

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[ivars = TabIvars]
    pub struct Tab;

    unsafe impl NSObjectProtocol for Tab {}

    unsafe impl NSWindowDelegate for Tab {
        #[unsafe(method(windowDidBecomeKey:))]
        fn window_did_become_key(&self, _n: &NSNotification) {
            self.app().set_active(self);
        }

        #[unsafe(method(windowWillClose:))]
        fn window_will_close(&self, _n: &NSNotification) {
            self.app().extensions_leave_site(self, None);
            self.destroy_web();
            self.app().tab_closed(self);
        }
    }

    unsafe impl WKNavigationDelegate for Tab {
        /// Every navigation: the safety checks in `action_policy`, then this
        /// site's protections (off only where the user turned Shields down).
        #[unsafe(method(webView:decidePolicyForNavigationAction:preferences:decisionHandler:))]
        fn decide_action(
            &self,
            _web: &WKWebView,
            action: &WKNavigationAction,
            prefs: &WKWebpagePreferences,
            decide: &DynBlock<dyn Fn(WKNavigationActionPolicy, NonNull<WKWebpagePreferences>)>,
        ) {
            let policy = self.action_policy(action);
            let url = unsafe { action.request() }.URL().and_then(|u| u.absoluteString()).map(|s| s.to_string()).unwrap_or_default();
            if is_main_frame(action) && !self.app().shields_up(&url) {
                crate::privacy::harden_webpage_preferences(prefs, false);
            }
            if policy == WKNavigationActionPolicy::Download {
                let (decide, prefs) = (decide.copy(), prefs.retain());
                return self.allow_download(move |ok| {
                    let policy = if ok { WKNavigationActionPolicy::Download } else { WKNavigationActionPolicy::Cancel };
                    decide.call((policy, NonNull::from(&*prefs)))
                });
            }
            decide.call((policy, NonNull::from(prefs)));
        }

        #[unsafe(method(webView:decidePolicyForNavigationResponse:decisionHandler:))]
        fn decide_response(
            &self,
            _web: &WKWebView,
            response: &WKNavigationResponse,
            decide: &DynBlock<dyn Fn(WKNavigationResponsePolicy)>,
        ) {
            let download = unsafe { !response.canShowMIMEType() || is_attachment(response) };
            if !download {
                return decide.call((WKNavigationResponsePolicy::Allow,));
            }
            let decide = decide.copy();
            self.allow_download(move |ok| {
                decide.call((if ok { WKNavigationResponsePolicy::Download } else { WKNavigationResponsePolicy::Cancel },))
            });
        }

        #[unsafe(method(webView:navigationAction:didBecomeDownload:))]
        fn action_became_download(&self, _web: &WKWebView, _a: &WKNavigationAction, download: &WKDownload) {
            self.app().start_download(download);
        }

        #[unsafe(method(webView:navigationResponse:didBecomeDownload:))]
        fn response_became_download(&self, _web: &WKWebView, _r: &WKNavigationResponse, download: &WKDownload) {
            self.app().start_download(download);
        }

        /// A new page replaced the old one: its title must not land on the old visit.
        #[unsafe(method(webView:didCommitNavigation:))]
        fn did_commit_navigation(&self, _web: &WKWebView, _nav: Option<&WKNavigation>) {
            self.ivars().visit.set(None);
            self.ivars().requested.replace(None);
            self.ivars().allow_internal.replace(None);
            // A new page starts with a clean slate for dialogs and downloads.
            self.ivars().dialogs.set(0);
            self.ivars().page_downloads.set(0);
            self.ivars().dialogs_blocked.set(false);
        }

        #[unsafe(method(webView:didFinishNavigation:))]
        fn did_finish_navigation(&self, _web: &WKWebView, _nav: Option<&WKNavigation>) {
            self.remember_page();
            // The title notification can arrive after this; show the final title now.
            self.refresh_toolbar();
            let url = self.url();
            let error_page = self.ivars().error_page.replace(false);
            if !error_page && (url.starts_with("http://") || url.starts_with("https://")) {
                let id = self.app().store().add_visit(&url, &self.ivars().title.borrow());
                self.ivars().visit.set(Some(id));
            }
            self.report_nav("LOADED", &url);
            if let Some(id) = self.ivars().pending_ext_action.take() {
                self.app().perform_extension_action(id, self);
            }
        }

        #[unsafe(method(webView:didFailNavigation:withError:))]
        fn did_fail_navigation(&self, _web: &WKWebView, _nav: Option<&WKNavigation>, err: &NSError) {
            self.report_nav("FAILED", &err.localizedDescription().to_string());
        }

        #[unsafe(method(webView:didFailProvisionalNavigation:withError:))]
        fn did_fail_provisional(&self, _web: &WKWebView, _nav: Option<&WKNavigation>, err: &NSError) {
            // Cancelled (-999): another load replaced this one, and that one
            // is what the user (and the bench timer) is waiting for.
            if err.code() == -999 {
                return;
            }
            self.report_nav("FAILED", &err.localizedDescription().to_string());
            self.ivars().requested.replace(None);
            self.ivars().allow_internal.replace(None);
            self.refresh_toolbar();
            // "Interrupted" (102, e.g. it became a download) isn't a failure to show.
            if err.code() == 102 {
                return;
            }
            let url = err
                .userInfo()
                .objectForKey(&NSString::from_str("NSErrorFailingURLStringKey"))
                .and_then(|u| u.downcast::<NSString>().ok())
                .map(|u| u.to_string())
                .unwrap_or_else(|| self.url());
            let certificate = err.domain().to_string() == "NSURLErrorDomain" && (-1206..=-1200).contains(&err.code());
            let detail = if certificate {
                "The site's security certificate isn't valid, so someone could be pretending to be it or reading what you send. The page wasn't opened.".to_string()
            } else {
                err.localizedDescription().to_string()
            };
            self.show_error(&url, "Can't open this page", &detail);
        }

        #[unsafe(method(webViewWebContentProcessDidTerminate:))]
        fn content_process_terminated(&self, web: &WKWebView) {
            if self.app().is_bench() {
                say!("CRASHED");
            }
            // The page's process died (crash or memory kill): bring the page
            // back, but not forever: a page that keeps crashing would burn CPU.
            let (count, last) = self.ivars().crashes.get();
            let recent = last.is_some_and(|t| t.elapsed().as_secs() < 30);
            let count = if recent { count + 1 } else { 1 };
            self.ivars().crashes.set((count, Some(Instant::now())));
            if count >= 3 {
                let url = self.url();
                self.show_error(&url, "This page keeps crashing", "It stopped three times in a row, so it wasn't reloaded again. Reload to try once more.");
            } else {
                unsafe { web.reload() };
            }
        }
    }

    unsafe impl WKUIDelegate for Tab {
        /// target=_blank links and window.open: open in a new tab of this window.
        #[unsafe(method(webView:createWebViewWithConfiguration:forNavigationAction:windowFeatures:))]
        fn create_web_view(
            &self,
            _web: &WKWebView,
            config: &WKWebViewConfiguration,
            _action: &WKNavigationAction,
            _features: &WKWindowFeatures,
        ) -> *mut WKWebView {
            // WebKit requires a web view built from exactly this configuration.
            let tab = self.app().open_tab_with_config(config);
            let web = tab.ivars().web.borrow().clone();
            web.map_or(std::ptr::null_mut(), Retained::autorelease_return)
        }

        #[unsafe(method(webViewDidClose:))]
        fn web_view_did_close(&self, _web: &WKWebView) {
            self.window().performClose(None);
        }

        #[unsafe(method(webView:runJavaScriptAlertPanelWithMessage:initiatedByFrame:completionHandler:))]
        fn js_alert(&self, _web: &WKWebView, message: &NSString, _frame: &WKFrameInfo, done: &DynBlock<dyn Fn()>) {
            let done = done.copy();
            self.page_dialog(message, &["OK"], None, move |_, _| done.call(()));
        }

        #[unsafe(method(webView:runJavaScriptConfirmPanelWithMessage:initiatedByFrame:completionHandler:))]
        fn js_confirm(&self, _web: &WKWebView, message: &NSString, _frame: &WKFrameInfo, done: &DynBlock<dyn Fn(Bool)>) {
            let done = done.copy();
            self.page_dialog(message, &["OK", "Cancel"], None, move |ok, _| done.call((Bool::new(ok),)));
        }

        #[unsafe(method(webView:runJavaScriptTextInputPanelWithPrompt:defaultText:initiatedByFrame:completionHandler:))]
        fn js_prompt(
            &self,
            _web: &WKWebView,
            prompt: &NSString,
            default: Option<&NSString>,
            _frame: &WKFrameInfo,
            done: &DynBlock<dyn Fn(*mut NSString)>,
        ) {
            let done = done.copy();
            self.page_dialog(prompt, &["OK", "Cancel"], Some(default.map(|d| d.to_string()).unwrap_or_default()), move |ok, text| {
                let text = text.map(|t| NSString::from_str(&t));
                done.call((if ok { text.map_or(std::ptr::null_mut(), |t| Retained::as_ptr(&t) as *mut _) } else { std::ptr::null_mut() },));
            });
        }

        /// Camera and microphone: the site's saved choice, else ask.
        #[unsafe(method(webView:requestMediaCapturePermissionForOrigin:initiatedByFrame:type:decisionHandler:))]
        fn media_permission(
            &self,
            _web: &WKWebView,
            origin: &WKSecurityOrigin,
            _frame: &WKFrameInfo,
            kind: WKMediaCaptureType,
            decide: &DynBlock<dyn Fn(WKPermissionDecision)>,
        ) {
            let kinds: &[&str] = match kind {
                WKMediaCaptureType::Camera => &["camera"],
                WKMediaCaptureType::Microphone => &["microphone"],
                _ => &["camera", "microphone"],
            };
            let decide = decide.copy();
            let site = unsafe { origin.host() }.to_string().to_ascii_lowercase();
            self.ask_permission(&site, kinds, move |ok| decide.call((if ok { WKPermissionDecision::Grant } else { WKPermissionDecision::Deny },)));
        }

        /// <input type=file>: a native file picker.
        #[unsafe(method(webView:runOpenPanelWithParameters:initiatedByFrame:completionHandler:))]
        fn open_panel(
            &self,
            _web: &WKWebView,
            params: &WKOpenPanelParameters,
            _frame: &WKFrameInfo,
            done: &DynBlock<dyn Fn(*mut NSArray<NSURL>)>,
        ) {
            let panel = NSOpenPanel::openPanel(self.mtm());
            unsafe {
                panel.setCanChooseFiles(true);
                panel.setCanChooseDirectories(params.allowsDirectories());
                panel.setAllowsMultipleSelection(params.allowsMultipleSelection());
            }
            let done = done.copy();
            let p = panel.clone();
            let handler = RcBlock::new(move |response: isize| {
                let urls = (response == NSModalResponseOK).then(|| p.URLs());
                done.call((urls.as_ref().map_or(std::ptr::null_mut(), |u| Retained::as_ptr(u) as *mut _),));
            });
            panel.beginSheetModalForWindow_completionHandler(self.window(), &handler);
        }
    }

    unsafe impl NSControlTextEditingDelegate for Tab {
        /// Find field keys: Enter = next, Shift+Enter = previous, Esc = close.
        #[unsafe(method(control:textView:doCommandBySelector:))]
        fn find_field_command(&self, _control: &NSControl, _view: &NSTextView, command: objc2::runtime::Sel) -> bool {
            if command == sel!(cancelOperation:) {
                self.hide_find();
                true
            } else if command == sel!(insertNewline:) {
                let shift = NSApplication::sharedApplication(self.mtm())
                    .currentEvent()
                    .is_some_and(|e| e.modifierFlags().contains(NSEventModifierFlags::Shift));
                self.find(shift);
                true
            } else {
                false
            }
        }
    }

    unsafe impl NSTextFieldDelegate for Tab {}
    unsafe impl NSSearchFieldDelegate for Tab {}

    /// The tab as web extensions see it (`tabs` API, content scripts).
    unsafe impl WKWebExtensionTab for Tab {
        #[unsafe(method(windowForWebExtensionContext:))]
        fn ext_window(&self, _c: &WKWebExtensionContext) -> *mut ProtocolObject<dyn WKWebExtensionWindow> {
            let w = self.app().ext_window_for(self);
            w.map_or(std::ptr::null_mut(), |w| Retained::autorelease_return(ProtocolObject::from_retained(w)))
        }

        #[unsafe(method(indexInWindowForWebExtensionContext:))]
        fn ext_index(&self, _c: &WKWebExtensionContext) -> usize {
            let tabs = self.app().ext_window_for(self).map(|w| w.tabs()).unwrap_or_default();
            tabs.iter().position(|t| std::ptr::eq(&**t, self)).unwrap_or(0)
        }

        #[unsafe(method(webViewForWebExtensionContext:))]
        fn ext_web_view(&self, _c: &WKWebExtensionContext) -> *mut WKWebView {
            self.web().map_or(std::ptr::null_mut(), Retained::autorelease_return)
        }

        #[unsafe(method(titleForWebExtensionContext:))]
        fn ext_title(&self, _c: &WKWebExtensionContext) -> *mut NSString {
            Retained::autorelease_return(NSString::from_str(&self.title()))
        }

        #[unsafe(method(urlForWebExtensionContext:))]
        fn ext_url(&self, _c: &WKWebExtensionContext) -> *mut NSURL {
            NSURL::URLWithString(&NSString::from_str(&self.url())).map_or(std::ptr::null_mut(), Retained::autorelease_return)
        }

        #[unsafe(method(isSelectedForWebExtensionContext:))]
        fn ext_is_selected(&self, _c: &WKWebExtensionContext) -> bool {
            self.window().tabGroup().and_then(|g| g.selectedWindow()).is_some_and(|w| std::ptr::eq(&*w, self.window()))
        }

        #[unsafe(method(isLoadingCompleteForWebExtensionContext:))]
        fn ext_loading_complete(&self, _c: &WKWebExtensionContext) -> bool {
            self.web().is_none_or(|w| unsafe { !w.isLoading() })
        }

        #[unsafe(method(loadURL:forWebExtensionContext:completionHandler:))]
        fn ext_load(&self, url: &NSURL, _c: &WKWebExtensionContext, done: &DynBlock<dyn Fn(*mut NSError)>) {
            let url = url.absoluteString().map(|u| u.to_string()).unwrap_or_default();
            if !extension_may_load(&url) {
                let err = NSError::new(1, &NSString::from_str("my-browser"));
                return done.call((Retained::as_ptr(&err) as *mut _,));
            }
            self.load(&url);
            done.call((std::ptr::null_mut(),));
        }

        #[unsafe(method(reloadFromOrigin:forWebExtensionContext:completionHandler:))]
        fn ext_reload(&self, _origin: bool, _c: &WKWebExtensionContext, done: &DynBlock<dyn Fn(*mut NSError)>) {
            self.reload();
            done.call((std::ptr::null_mut(),));
        }

        #[unsafe(method(goBackForWebExtensionContext:completionHandler:))]
        fn ext_back(&self, _c: &WKWebExtensionContext, done: &DynBlock<dyn Fn(*mut NSError)>) {
            self.go_back();
            done.call((std::ptr::null_mut(),));
        }

        #[unsafe(method(goForwardForWebExtensionContext:completionHandler:))]
        fn ext_forward(&self, _c: &WKWebExtensionContext, done: &DynBlock<dyn Fn(*mut NSError)>) {
            self.go_forward();
            done.call((std::ptr::null_mut(),));
        }

        #[unsafe(method(activateForWebExtensionContext:completionHandler:))]
        fn ext_activate(&self, _c: &WKWebExtensionContext, done: &DynBlock<dyn Fn(*mut NSError)>) {
            self.app().select(self);
            done.call((std::ptr::null_mut(),));
        }

        #[unsafe(method(closeForWebExtensionContext:completionHandler:))]
        fn ext_close(&self, _c: &WKWebExtensionContext, done: &DynBlock<dyn Fn(*mut NSError)>) {
            self.window().performClose(None);
            done.call((std::ptr::null_mut(),));
        }
    }

    impl Tab {
        /// Location (WKUIDelegate, newer than our bindings): saved choice, else ask.
        #[unsafe(method(webView:requestGeolocationPermissionForOrigin:initiatedByFrame:decisionHandler:))]
        fn location_permission(
            &self,
            _web: &WKWebView,
            origin: &WKSecurityOrigin,
            _frame: &WKFrameInfo,
            decide: &DynBlock<dyn Fn(WKPermissionDecision)>,
        ) {
            let decide = decide.copy();
            let site = unsafe { origin.host() }.to_string().to_ascii_lowercase();
            self.ask_permission(&site, &["location"], move |ok| decide.call((if ok { WKPermissionDecision::Grant } else { WKPermissionDecision::Deny },)));
        }

        /// The site button: a menu with this site's protections and permissions.
        #[unsafe(method(showSiteMenu:))]
        fn show_site_menu_action(&self, _s: Option<&AnyObject>) {
            self.show_site_menu();
        }

        /// Site menu: tag = permission index * 10 + choice (0 ask, 1 allow, 2 block).
        #[unsafe(method(sitePermission:))]
        fn site_permission_action(&self, sender: &NSMenuItem) {
            let Some(site) = crate::store::site_of(&self.url()) else { return };
            let (kind, choice) = (sender.tag() / 10, sender.tag() % 10);
            let Some((kind, ..)) = SITE_PERMISSIONS.get(kind as usize) else { return };
            let value = ["ask", "allow", "deny"][choice as usize % 3];
            let _ = self.app().store().set_permission(&site, kind, value);
        }

        /// Site menu: Shields (tag 0) or local network access (tag 1) toggled.
        #[unsafe(method(siteToggle:))]
        fn site_toggle_action(&self, sender: &NSMenuItem) {
            let Some(site) = crate::store::site_of(&self.url()) else { return };
            let store = self.app().store();
            let _ = if sender.tag() == 0 {
                let off = store.permission(&site, "shields").as_deref() == Some("off");
                store.set_permission(&site, "shields", if off { "ask" } else { "off" })
            } else {
                let allowed = store.permission(&site, "local_network").as_deref() == Some("allow");
                store.set_permission(&site, "local_network", if allowed { "ask" } else { "allow" })
            };
            // The blockers are recompiled with the new exception, then the page reloads under them.
            let this = self.retain();
            self.app().load_shields(move |_, _, _| this.reload());
        }

        /// An extension's toolbar button (tag = extension id).
        #[unsafe(method(extensionClicked:))]
        fn extension_clicked_action(&self, sender: &NSButton) {
            if let Err(e) = self.app().extension_clicked(sender.tag() as i64, self) {
                self.confirm(&e, &["OK"], |_| {});
            }
        }

        /// Right-click menu of an extension button: tag = id * 2 + (0 click, 1 always).
        #[unsafe(method(extensionAccess:))]
        fn extension_access_action(&self, sender: &NSMenuItem) {
            let (id, always) = (sender.tag() as i64 / 2, sender.tag() % 2 == 1);
            let _ = self.app().set_extension_access(id, if always { "always" } else { "click" });
        }

        #[unsafe(method(manageExtensions:))]
        fn manage_extensions_action(&self, _s: Option<&AnyObject>) {
            self.app().open_settings_page("settings#extensions");
        }

        /// New tab page: Enter in its search field.
        #[unsafe(method(homeSearch:))]
        fn home_search_action(&self, sender: &NSTextField) {
            let text = sender.stringValue().to_string();
            if !text.trim().is_empty() {
                let url = self.app().normalize(&text);
                self.load(&url);
            }
        }

        /// New tab page: a most visited site (its address is the button's tooltip).
        #[unsafe(method(homeSite:))]
        fn home_site_action(&self, sender: &NSButton) {
            if let Some(url) = sender.toolTip() {
                self.load(&url.to_string());
            }
        }

        #[unsafe(method(openHomeSettings:))]
        fn open_home_settings_action(&self, _s: Option<&AnyObject>) {
            self.app().open_settings_page("settings#general");
        }

        #[unsafe(method(openSiteSettings:))]
        fn open_site_settings_action(&self, _s: Option<&AnyObject>) {
            self.app().open_settings_page("settings#permissions");
        }

        /// KVO on the web view: keep toolbar, title and history in sync.
        #[unsafe(method(observeValueForKeyPath:ofObject:change:context:))]
        fn observe_value(&self, key: Option<&NSString>, _o: Option<&AnyObject>, _c: Option<&AnyObject>, _ctx: *mut c_void) {
            let key = key.map(|k| k.to_string()).unwrap_or_default();
            let changed = match key.as_str() {
                "URL" => WKWebExtensionTabChangedProperties::URL,
                "title" => WKWebExtensionTabChangedProperties::Title,
                "loading" => WKWebExtensionTabChangedProperties::Loading,
                _ => WKWebExtensionTabChangedProperties::None,
            };
            if changed != WKWebExtensionTabChangedProperties::None {
                self.app().ext_tab_changed(self, changed);
            }
            if key == "title" {
                self.remember_page();
                let title = self.title();
                if let (Some(id), false) = (self.ivars().visit.get(), title.is_empty()) {
                    self.app().store().set_visit_title(id, &title);
                }
            }
            self.refresh_toolbar();
        }

        /// Address bar action: fired on Enter.
        #[unsafe(method(go:))]
        fn go_action(&self, sender: &NSTextField) {
            let text = sender.stringValue().to_string();
            if !text.trim().is_empty() {
                let url = self.app().normalize(&text);
                self.load(&url);
            }
        }

        #[unsafe(method(goBack:))]
        fn go_back_action(&self, _s: Option<&AnyObject>) {
            self.go_back();
        }

        #[unsafe(method(showNetworkSettings:))]
        fn show_network_settings(&self, _s: Option<&AnyObject>) {
            self.app().open_settings_page("settings#network");
        }

        #[unsafe(method(goHome:))]
        fn go_home_action(&self, _s: Option<&AnyObject>) {
            self.go_home();
        }

        #[unsafe(method(goForward:))]
        fn go_forward_action(&self, _s: Option<&AnyObject>) {
            self.go_forward();
        }

        #[unsafe(method(reload:))]
        fn reload_action(&self, _s: Option<&AnyObject>) {
            self.reload();
        }

        #[unsafe(method(stop:))]
        fn stop_action(&self, _s: Option<&AnyObject>) {
            self.stop();
        }

        /// Typing in the find field: search as you type.
        #[unsafe(method(findChanged:))]
        fn find_changed_action(&self, _s: Option<&AnyObject>) {
            self.find(false);
        }

        #[unsafe(method(findNextButton:))]
        fn find_next_button(&self, _s: Option<&AnyObject>) {
            self.find(false);
        }

        #[unsafe(method(findPreviousButton:))]
        fn find_previous_button(&self, _s: Option<&AnyObject>) {
            self.find(true);
        }

        #[unsafe(method(closeFind:))]
        fn close_find_action(&self, _s: Option<&AnyObject>) {
            self.hide_find();
        }
    }
);

impl Tab {
    /// A new tab window. With `config` (a page opened a popup), the web view is
    /// created immediately from it and WebKit loads it. Otherwise `url` loads
    /// now, or later when the tab wakes if `asleep`.
    pub fn create(
        app: &App,
        config: Option<&WKWebViewConfiguration>,
        url: &str,
        title: &str,
        asleep: bool,
    ) -> Retained<Tab> {
        let mtm = app.mtm();
        let this = Self::alloc(mtm).set_ivars(TabIvars::default());
        let this: Retained<Tab> = unsafe { msg_send![super(this), init] };
        let _ = this.ivars().app.set(app.retain());
        this.ivars().url.replace(url.to_string());
        this.ivars().title.replace(title.to_string());
        this.build_window(mtm);
        if let Some(config) = config {
            this.attach_web(config);
        } else if !asleep && !url.is_empty() {
            this.load(url);
        } else if url.is_empty() {
            this.show_home();
        }
        this.refresh_toolbar();
        this
    }

    fn app(&self) -> &App {
        self.ivars().app.get().unwrap()
    }

    pub fn window(&self) -> &NSWindow {
        self.ivars().window.get().unwrap()
    }

    pub fn web(&self) -> Option<Retained<WKWebView>> {
        self.ivars().web.borrow().clone()
    }

    pub fn is_asleep(&self) -> bool {
        self.ivars().web.borrow().is_none() && !self.ivars().url.borrow().is_empty()
    }

    pub fn url(&self) -> String {
        self.ivars().url.borrow().clone()
    }

    pub fn title(&self) -> String {
        self.ivars().title.borrow().clone()
    }

    fn build_window(&self, mtm: MainThreadMarker) {
        let (w, h) = (1100.0, 750.0);
        let window = unsafe {
            NSWindow::initWithContentRect_styleMask_backing_defer(
                NSWindow::alloc(mtm),
                NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(w, h)),
                NSWindowStyleMask::Titled
                    | NSWindowStyleMask::Closable
                    | NSWindowStyleMask::Miniaturizable
                    | NSWindowStyleMask::Resizable,
                NSBackingStoreType::Buffered,
                false,
            )
        };
        unsafe { window.setReleasedWhenClosed(false) };
        // No state restoration: the browser restores its own session, asleep.
        window.setRestorable(false);
        // Native tabs: every tab is a window in the same tab group.
        window.setTabbingMode(NSWindowTabbingMode::Preferred);
        window.setTabbingIdentifier(&NSString::from_str("browser"));
        window.setDelegate(Some(ProtocolObject::from_ref(self)));
        if self.app().is_on_top() {
            // macOS may refuse to activate an app launched from a terminal, or open the
            // window on a Space that isn't showing. WebKit throttles hidden pages and skips
            // painting them, so benchmarks would miss rendering cost. Float on every Space.
            window.setCollectionBehavior(
                NSWindowCollectionBehavior::CanJoinAllSpaces | NSWindowCollectionBehavior::FullScreenAuxiliary,
            );
            window.setLevel(NSFloatingWindowLevel);
        }
        let content = window.contentView().unwrap();

        // Toolbar row: back, forward, reload/stop, home, site button, address bar.
        let top = h - TOOLBAR_H + 2.0;
        let pin_top = NSAutoresizingMaskOptions::ViewMinYMargin;
        let buttons = [
            ("chevron.left", "Back", sel!(goBack:), &self.ivars().back),
            ("chevron.right", "Forward", sel!(goForward:), &self.ivars().forward),
            ("arrow.clockwise", "Reload", sel!(reload:), &self.ivars().reload),
            ("house", "Home", sel!(goHome:), &self.ivars().home_button),
        ];
        for (i, (symbol, label, action, slot)) in buttons.into_iter().enumerate() {
            let button = unsafe {
                NSButton::buttonWithImage_target_action(&symbol_image(symbol, label), Some(self), Some(action), mtm)
            };
            button.setBordered(false);
            button.setFrame(NSRect::new(NSPoint::new(4.0 + i as f64 * BUTTON_W, top), NSSize::new(BUTTON_W, 28.0)));
            button.setAutoresizingMask(pin_top);
            content.addSubview(&button);
            let _ = slot.set(button);
        }
        let site = unsafe {
            NSButton::buttonWithImage_target_action(
                &symbol_image("info.circle", "Site settings"),
                Some(self),
                Some(sel!(showSiteMenu:)),
                mtm,
            )
        };
        site.setBordered(false);
        site.setFrame(NSRect::new(NSPoint::new(4.0 + 4.0 * BUTTON_W, top), NSSize::new(BUTTON_W, 28.0)));
        site.setAutoresizingMask(pin_top);
        site.setToolTip(Some(&NSString::from_str("Site settings: protections and permissions")));
        content.addSubview(&site);
        let _ = self.ivars().site_button.set(site);
        self.ivars().shown_site_symbol.set("info.circle");
        let bar_x = 8.0 + 5.0 * BUTTON_W;
        let bar = NSTextField::textFieldWithString(&NSString::from_str(""), mtm);
        bar.setFrame(NSRect::new(NSPoint::new(bar_x, top + 3.0), NSSize::new(w - bar_x - 8.0, 22.0)));
        bar.setAutoresizingMask(NSAutoresizingMaskOptions::ViewWidthSizable | pin_top);
        bar.setPlaceholderString(Some(&NSString::from_str("Search or enter address")));
        unsafe {
            bar.setTarget(Some(self));
            bar.setAction(Some(sel!(go:)));
        }
        content.addSubview(&bar);
        let _ = self.ivars().bar.set(bar);
        let _ = self.ivars().window.set(window);
        self.set_extension_buttons(&self.app().extension_buttons(self));
    }

    /// The native new tab page, filling the page area.
    fn show_home(&self) {
        let app = self.app();
        let store = app.store();
        let home = crate::home::Home {
            art: store.flag("home_art"),
            search: store.flag("home_search"),
            sites: if store.flag("home_top_sites") { store.top_sites(8) } else { Vec::new() },
        };
        self.ivars().home_state.replace(format!(
            "HOME shown=1 art={} search={} sites={}",
            home.art as u8,
            home.search as u8,
            home.sites.join(",")
        ));
        let view = crate::home::build(self, &home, self.web_frame(), self.mtm());
        view.setAutoresizingMask(
            NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewHeightSizable,
        );
        self.window().contentView().unwrap().addSubview(&view);
        if let Some(old) = self.ivars().home.replace(Some(view)) {
            old.removeFromSuperview();
        }
    }

    /// `HOME shown=<0|1> art=<0|1> search=<0|1> sites=<urls>`
    pub fn home_state(&self) -> String {
        if self.ivars().home.borrow().is_some() {
            self.ivars().home_state.borrow().clone()
        } else {
            "HOME shown=0".into()
        }
    }

    /// Create the web view (waking the tab). Uses `config` for popups.
    fn attach_web(&self, config: &WKWebViewConfiguration) -> Retained<WKWebView> {
        if let Some(home) = self.ivars().home.take() {
            home.removeFromSuperview();
        }
        let content = self.window().contentView().unwrap();
        let web =
            unsafe { WKWebView::initWithFrame_configuration(WKWebView::alloc(self.mtm()), self.web_frame(), config) };
        web.setAutoresizingMask(
            NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewHeightSizable,
        );
        unsafe {
            web.setNavigationDelegate(Some(ProtocolObject::from_ref(self)));
            web.setUIDelegate(Some(ProtocolObject::from_ref(self)));
        }
        content.addSubview(&web);
        for key in OBSERVED {
            unsafe {
                web.addObserver_forKeyPath_options_context(
                    self,
                    &NSString::from_str(key),
                    NSKeyValueObservingOptions::New,
                    std::ptr::null_mut(),
                )
            };
        }
        self.ivars().web.replace(Some(web.clone()));
        web
    }

    /// The page area: below the toolbar, and below the find bar while it's open.
    fn web_frame(&self) -> NSRect {
        let size = self.window().contentView().unwrap().frame().size;
        let top = TOOLBAR_H + if self.ivars().find_open.get() { FIND_H } else { 0.0 };
        NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(size.width, size.height - top))
    }

    /// Open the find bar (Cmd+F) and focus its field.
    pub fn show_find(&self) {
        let iv = self.ivars();
        if iv.find_row.get().is_none() {
            self.build_find_bar();
        }
        let content = self.window().contentView().unwrap();
        let row = iv.find_row.get().unwrap();
        if !iv.find_open.replace(true) {
            let size = content.frame().size;
            row.setFrame(NSRect::new(
                NSPoint::new(0.0, size.height - TOOLBAR_H - FIND_H),
                NSSize::new(size.width, FIND_H),
            ));
            content.addSubview(row);
            if let Some(web) = self.web() {
                web.setFrame(self.web_frame());
            }
        }
        let field = iv.find_field.get().unwrap();
        self.window().makeFirstResponder(Some(field));
        unsafe { field.selectText(None) };
    }

    /// Close the find bar and give the page its space back.
    pub fn hide_find(&self) {
        let iv = self.ivars();
        if !iv.find_open.replace(false) {
            return;
        }
        if let Some(row) = iv.find_row.get() {
            row.removeFromSuperview();
        }
        if let Some(web) = self.web() {
            web.setFrame(self.web_frame());
            self.window().makeFirstResponder(Some(&web));
        }
        iv.find_last.replace(String::new());
    }

    pub fn is_find_open(&self) -> bool {
        self.ivars().find_open.get()
    }

    pub fn set_find_text(&self, text: &str) {
        if let Some(f) = self.ivars().find_field.get() {
            f.setStringValue(&NSString::from_str(text));
        }
    }

    /// `FINDBAR open=<0|1> text=<term> status=<shown status>`
    pub fn find_state(&self) -> String {
        let iv = self.ivars();
        let text = iv.find_field.get().map(|f| f.stringValue().to_string()).unwrap_or_default();
        let status = iv.find_status.get().map(|s| s.stringValue().to_string()).unwrap_or_default();
        format!("FINDBAR open={} text={text} status={status}", iv.find_open.get() as u8)
    }

    fn build_find_bar(&self) {
        let mtm = self.mtm();
        let width = self.window().contentView().unwrap().frame().size.width;
        let row =
            NSView::initWithFrame(NSView::alloc(mtm), NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(width, FIND_H)));
        row.setAutoresizingMask(
            NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewMinYMargin,
        );

        let field = NSSearchField::new(mtm);
        field.setFrame(NSRect::new(NSPoint::new(8.0, 4.0), NSSize::new(260.0, 22.0)));
        field.setPlaceholderString(Some(&NSString::from_str("Find on page")));
        unsafe {
            field.setTarget(Some(self));
            field.setAction(Some(sel!(findChanged:)));
            field.setDelegate(Some(ProtocolObject::from_ref(self)));
        }
        row.addSubview(&field);

        let buttons = [
            ("chevron.up", "Previous match", sel!(findPreviousButton:), 274.0),
            ("chevron.down", "Next match", sel!(findNextButton:), 300.0),
        ];
        for (symbol, label, action, x) in buttons {
            let b = unsafe {
                NSButton::buttonWithImage_target_action(&symbol_image(symbol, label), Some(self), Some(action), mtm)
            };
            b.setBordered(false);
            b.setFrame(NSRect::new(NSPoint::new(x, 3.0), NSSize::new(26.0, 24.0)));
            row.addSubview(&b);
        }

        let status = NSTextField::labelWithString(&NSString::from_str(""), mtm);
        status.setFrame(NSRect::new(NSPoint::new(334.0, 7.0), NSSize::new(200.0, 16.0)));
        status.setTextColor(Some(&objc2_app_kit::NSColor::secondaryLabelColor()));
        row.addSubview(&status);

        let done = unsafe {
            NSButton::buttonWithTitle_target_action(
                &NSString::from_str("Done"),
                Some(self),
                Some(sel!(closeFind:)),
                mtm,
            )
        };
        done.setFrame(NSRect::new(NSPoint::new(width - 76.0, 2.0), NSSize::new(68.0, 26.0)));
        done.setAutoresizingMask(NSAutoresizingMaskOptions::ViewMinXMargin);
        row.addSubview(&done);

        let _ = self.ivars().find_field.set(field);
        let _ = self.ivars().find_status.set(status);
        let _ = self.ivars().find_row.set(row);
    }

    /// Find the field's text in the page, wrapping around. A new term starts
    /// from the top of the page; repeating it moves to the next match.
    pub fn find(&self, backwards: bool) {
        let iv = self.ivars();
        let (Some(field), Some(web)) = (iv.find_field.get(), self.web()) else { return };
        let text = field.stringValue().to_string();
        if text.is_empty() {
            iv.find_status.get().unwrap().setStringValue(&NSString::from_str(""));
            iv.find_last.replace(String::new());
            return;
        }
        let new_term = iv.find_last.replace(text.clone()) != text;
        let this = self.retain();
        let search = move || {
            let config = unsafe { WKFindConfiguration::new(this.mtm()) };
            unsafe {
                config.setBackwards(backwards);
                config.setWraps(true);
                config.setCaseSensitive(false);
            }
            let tab = this.clone();
            let done = RcBlock::new(move |result: std::ptr::NonNull<WKFindResult>| {
                let found = unsafe { result.as_ref().matchFound() };
                let label = if found { "" } else { "No matches" };
                tab.ivars().find_status.get().unwrap().setStringValue(&NSString::from_str(label));
                if tab.ivars().find_report.replace(false) {
                    say!("FIND found={}", found as u8);
                }
            });
            if let Some(web) = this.web() {
                unsafe {
                    web.findString_withConfiguration_completionHandler(&NSString::from_str(&text), Some(&config), &done)
                };
            }
        };
        if new_term {
            // Searching continues from the current selection; drop it so a new
            // term starts at the top. Runs in the browser's own script world,
            // invisible to the page.
            let search = RefCell::new(Some(search));
            let then = RcBlock::new(move |_r: *mut AnyObject, _e: *mut NSError| {
                if let Some(s) = search.take() {
                    s();
                }
            });
            unsafe {
                web.evaluateJavaScript_inFrame_inContentWorld_completionHandler(
                    &NSString::from_str("getSelection().removeAllRanges()"),
                    None,
                    &WKContentWorld::defaultClientWorld(self.mtm()),
                    Some(&then),
                )
            };
        } else {
            search();
        }
    }

    fn ensure_web(&self) -> Retained<WKWebView> {
        if let Some(web) = self.web() {
            return web;
        }
        let config = self.app().make_config();
        self.attach_web(&config)
    }

    /// Drop the web view and its page process. Observers go first: KVO on a
    /// deallocated object crashes.
    fn destroy_web(&self) {
        let Some(web) = self.ivars().web.take() else { return };
        for key in OBSERVED {
            unsafe { web.removeObserver_forKeyPath(self, &NSString::from_str(key)) };
        }
        unsafe {
            web.stopLoading();
            web.setNavigationDelegate(None);
            web.setUIDelegate(None);
        }
        web.removeFromSuperview();
    }

    /// Unload the page but keep URL and title. No-op for the active tab.
    pub fn sleep(&self) {
        if self.web().is_none() || self.app().is_active(self) {
            return;
        }
        self.remember_page();
        self.destroy_web();
        self.refresh_toolbar();
    }

    /// Reload an asleep tab's page.
    pub fn wake(&self) {
        if self.is_asleep() {
            let url = self.url();
            self.load(&url);
        }
    }

    fn remember_page(&self) {
        let Some(web) = self.web() else { return };
        if let Some(url) = unsafe { web.URL() }.and_then(|u| u.absoluteString()) {
            self.ivars().url.replace(url.to_string());
        }
        self.ivars().title.replace(unsafe { web.title() }.map(|t| t.to_string()).unwrap_or_default());
    }

    pub fn load(&self, url: &str) {
        let Some(nsurl) = NSURL::URLWithString(&NSString::from_str(url)) else { return };
        self.ivars().allow_internal.replace(url.starts_with(&format!("{INTERNAL}:")).then(|| url.to_string()));
        let web = self.ensure_web();
        self.ivars().url.replace(url.to_string());
        self.ivars().requested.replace(Some(url.to_string()));
        // The user's own load: the page's download count starts over.
        self.ivars().page_downloads.set(0);
        // Focus moves to the page, so the address bar is free to show the real URL.
        self.window().makeFirstResponder(Some(&web));
        if nsurl.isFileURL() {
            // A local page (opened from Finder or typed): it may load the
            // files next to it, like images and stylesheets.
            let dir = nsurl.URLByDeletingLastPathComponent().unwrap_or_else(|| nsurl.clone());
            unsafe { web.loadFileURL_allowingReadAccessToURL(&nsurl, &dir) };
        } else {
            unsafe { web.loadRequest(&NSURLRequest::requestWithURL(&nsurl)) };
        }
    }

    /// The blocked-requests button is showing.
    pub fn net_button_shown(&self) -> bool {
        self.ivars().net_button.borrow().is_some()
    }

    /// Home button: the new tab page in place of the page. The web view goes
    /// (and with it the page's memory), like a new tab.
    pub fn go_home(&self) {
        if self.web().is_none() && self.ivars().home.borrow().is_some() {
            return;
        }
        self.app().extensions_leave_site(self, None);
        self.destroy_web();
        self.ivars().url.replace(String::new());
        self.ivars().title.replace(String::new());
        self.show_home();
        self.refresh_toolbar();
        self.app()
            .ext_tab_changed(self, WKWebExtensionTabChangedProperties::URL | WKWebExtensionTabChangedProperties::Title);
    }

    pub fn go_back(&self) {
        if let Some(web) = self.web() {
            unsafe { web.goBack() };
        }
    }

    pub fn go_forward(&self) {
        if let Some(web) = self.web() {
            unsafe { web.goForward() };
        }
    }

    pub fn reload(&self) {
        match self.web() {
            Some(web) => unsafe {
                web.reload();
            },
            None => self.wake(),
        }
    }

    pub fn stop(&self) {
        if let Some(web) = self.web() {
            unsafe { web.stopLoading() };
        }
    }

    /// Page zoom: `None` resets, otherwise multiply.
    pub fn zoom(&self, factor: Option<f64>) {
        if let Some(web) = self.web() {
            let z = factor.map_or(1.0, |f| (unsafe { web.pageZoom() } * f).clamp(0.3, 3.0));
            unsafe { web.setPageZoom(z) };
        }
    }

    pub fn focus_address_bar(&self) {
        let bar = self.ivars().bar.get().unwrap();
        self.window().makeFirstResponder(Some(bar));
        unsafe { bar.selectText(None) };
    }

    /// Links: Cmd-click opens a new tab; web pages can't open internal pages;
    /// `download` attributes download.
    fn action_policy(&self, action: &WKNavigationAction) -> WKNavigationActionPolicy {
        let request = unsafe { action.request() };
        let url = request.URL().and_then(|u| u.absoluteString()).map(|s| s.to_string()).unwrap_or_default();
        let kind = unsafe { action.navigationType() };
        let link = kind == WKNavigationType::LinkActivated;
        let main = is_main_frame(action);
        if url.starts_with(&format!("{INTERNAL}:")) {
            let from_internal = unsafe { action.sourceFrame() }
                .is_some_and(|f| unsafe { f.securityOrigin().protocol() }.to_string() == INTERNAL);
            // Internal pages never load in a frame: a site could overlay one
            // and trick clicks into it (clickjacking "Clear data" or "Uninstall").
            let ours = self.ivars().allow_internal.borrow().as_deref() == Some(url.as_str());
            if !(ours || from_internal) || !main {
                return WKNavigationActionPolicy::Cancel;
            }
        }
        // Tracking parameters (fbclid, gclid, ...) are dropped before the page
        // loads, like Brave does. Only plain page loads: a form POST or a
        // back/forward step is left alone.
        let get = request.HTTPMethod().is_none_or(|m| m.to_string() == "GET");
        let fresh = kind != WKNavigationType::BackForward && kind != WKNavigationType::Reload;
        if main
            && get
            && fresh
            && self.app().shields_up(&url)
            && let Some(clean) = crate::privacy::strip_tracking_params(&url)
        {
            self.load(&clean);
            return WKNavigationActionPolicy::Cancel;
        }
        if unsafe { action.shouldPerformDownload() } {
            return WKNavigationActionPolicy::Download;
        }
        if link && unsafe { action.modifierFlags() }.contains(NSEventModifierFlags::Command) {
            self.app().open_tab(&url, true);
            return WKNavigationActionPolicy::Cancel;
        }
        // Leaving the site turns off extensions that were clicked on for it,
        // before the next page can load their scripts.
        if main && !self.ivars().ext_on.borrow().is_empty() {
            self.app().extensions_leave_site(self, crate::store::site_of(&url).as_deref());
        }
        WKNavigationActionPolicy::Allow
    }

    /// Sync buttons, address bar and window title.
    pub fn refresh_toolbar(&self) {
        let iv = self.ivars();
        let (Some(bar), Some(window)) = (iv.bar.get(), iv.window.get()) else { return };
        let web = self.web();
        let (back, fwd, loading) =
            web.as_ref().map_or((false, false, false), |w| unsafe { (w.canGoBack(), w.canGoForward(), w.isLoading()) });
        iv.back.get().unwrap().setEnabled(back);
        iv.forward.get().unwrap().setEnabled(fwd);
        if iv.shown_loading.replace(Some(loading)) != Some(loading) {
            let reload = iv.reload.get().unwrap();
            let (symbol, label, action) =
                if loading { ("xmark", "Stop", sel!(stop:)) } else { ("arrow.clockwise", "Reload", sel!(reload:)) };
            reload.setImage(Some(&symbol_image(symbol, label)));
            unsafe { reload.setAction(Some(action)) };
        }
        let url = match &web {
            Some(w) => self.shown_url(w),
            None => self.url(),
        };
        // Lock for HTTPS, a warning for plain HTTP, neutral for everything else.
        let (symbol, label) = if url.starts_with("https://") {
            ("lock.fill", "Secure connection. Site settings")
        } else if url.starts_with("http://") {
            ("exclamationmark.triangle", "Not secure: this page is sent unencrypted. Site settings")
        } else {
            ("info.circle", "Site settings")
        };
        if let Some(b) = iv.site_button.get() {
            if iv.shown_site_symbol.replace(symbol) != symbol {
                b.setImage(Some(&symbol_image(symbol, label)));
                b.setToolTip(Some(&NSString::from_str(label)));
            }
            b.setEnabled(crate::store::site_of(&url).is_some() && !url.starts_with(&format!("{INTERNAL}:")));
        }
        // Don't clobber what the user is typing.
        if bar.currentEditor().is_none() {
            bar.setStringValue(&NSString::from_str(&url));
        }
        let title = match &web {
            Some(w) => unsafe { w.title() }.map(|t| t.to_string()).unwrap_or_default(),
            None => self.title(),
        };
        let shown = if !title.is_empty() {
            title
        } else if url.is_empty() {
            "New Tab".to_string()
        } else {
            url
        };
        window.setTitle(&NSString::from_str(&shown));
    }

    /// The address to show: what the user asked for while it loads, otherwise
    /// the page actually on screen. WKWebView.URL switches to a page-started
    /// navigation's target before it commits, so a page could send itself to
    /// a slow bank address and keep showing its own fake login under it.
    fn shown_url(&self, web: &WKWebView) -> String {
        if let Some(r) = self.ivars().requested.borrow().clone() {
            return r;
        }
        // None if this WebKit lacks the method, Some("") if it has no address.
        let get = |sel: objc2::runtime::Sel| -> Option<String> {
            let responds: bool = unsafe { msg_send![web, respondsToSelector: sel] };
            if !responds {
                return None;
            }
            let u: Option<Retained<NSURL>> = unsafe { msg_send![web, performSelector: sel] };
            Some(u.and_then(|u| u.absoluteString()).map(|s| s.to_string()).unwrap_or_default())
        };
        // An error page shows the address that failed.
        if let Some(failed) = get(sel!(_unreachableURL)).filter(|u| !u.is_empty()) {
            return failed;
        }
        // Nothing committed yet (a new popup its opener can still write
        // into) shows as blank, not as the address it's heading to.
        get(sel!(_committedURL)).unwrap_or_else(|| {
            unsafe { web.URL() }.and_then(|u| u.absoluteString()).map(|s| s.to_string()).unwrap_or_default()
        })
    }

    /// `STATE back=<0|1> forward=<0|1> url=<address bar> title=<window title>`, read from the UI itself.
    pub fn state(&self) -> String {
        let iv = self.ivars();
        format!(
            "STATE back={} forward={} url={} title={}",
            iv.back.get().unwrap().isEnabled() as u8,
            iv.forward.get().unwrap().isEnabled() as u8,
            iv.bar.get().unwrap().stringValue(),
            self.window().title(),
        )
    }

    /// Print `<kind> <ms> <detail>` for the pending bench navigation, once.
    fn report_nav(&self, kind: &str, detail: &str) {
        if let Some(start) = self.ivars().nav_start.take() {
            say!("{kind} {:.1} {detail}", start.elapsed().as_secs_f64() * 1000.0);
        }
    }

    /// A sheet on this tab's window. `input` adds a text field; `checkbox`
    /// (title, initially on) adds a checkbox. `done(ok, text, checked)`.
    fn sheet(
        &self,
        message: &NSString,
        buttons: &[&str],
        input: Option<String>,
        checkbox: Option<(&str, bool)>,
        done: impl Fn(bool, Option<String>, bool) + 'static,
    ) {
        let mtm = self.mtm();
        let alert = NSAlert::new(mtm);
        // A blank line separates the question from the explanation under it.
        let text = message.to_string();
        let (title, detail) = text.split_once("\n\n").unwrap_or((&text, ""));
        alert.setMessageText(&NSString::from_str(title));
        if !detail.is_empty() {
            alert.setInformativeText(&NSString::from_str(detail));
        }
        for b in buttons {
            alert.addButtonWithTitle(&NSString::from_str(b));
        }
        let field = input.map(|text| {
            let f = NSTextField::textFieldWithString(&NSString::from_str(&text), mtm);
            f.setFrame(NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(260.0, 24.0)));
            alert.setAccessoryView(Some(&f));
            f
        });
        if let Some((title, on)) = checkbox {
            alert.setShowsSuppressionButton(true);
            if let Some(b) = alert.suppressionButton() {
                b.setTitle(&NSString::from_str(title));
                b.setState(if on { 1 } else { 0 });
            }
        }
        if self.app().is_bench() {
            say!("DIALOG {}", title.replace('\n', " "));
        }
        let this = self.retain();
        let a = alert.clone();
        let handler = RcBlock::new(move |response: isize| {
            this.ivars().alert.replace(None);
            let checked = a.suppressionButton().is_some_and(|b| b.state() == 1);
            // First button is NSAlertFirstButtonReturn (1000).
            done(response == 1000, field.as_ref().map(|f| f.stringValue().to_string()), checked);
        });
        self.ivars().alert.replace(Some(alert.clone()));
        alert.beginSheetModalForWindow_completionHandler(self.window(), Some(&handler));
    }

    /// alert/confirm/prompt from the page. From the second one on, the user
    /// can block the rest: an endless alert() loop would otherwise trap the
    /// tab, since a tab can't close while a sheet is up.
    fn page_dialog(
        &self,
        message: &NSString,
        buttons: &[&str],
        input: Option<String>,
        done: impl Fn(bool, Option<String>) + 'static,
    ) {
        let iv = self.ivars();
        if iv.dialogs_blocked.get() {
            return done(false, None);
        }
        let n = iv.dialogs.get() + 1;
        iv.dialogs.set(n);
        let checkbox = (n >= 2).then_some(("Don't allow more dialogs from this page", false));
        let this = self.retain();
        self.sheet(message, buttons, input, checkbox, move |ok, text, block| {
            if block {
                this.ivars().dialogs_blocked.set(true);
            }
            done(ok, text)
        });
    }

    /// Answer the sheet on screen (bench `dialog ok|cancel [check]`).
    pub fn answer_dialog(&self, ok: bool, check: bool) -> bool {
        let Some(alert) = self.ivars().alert.borrow().clone() else { return false };
        if let Some(b) = alert.suppressionButton() {
            b.setState(if check { 1 } else { 0 });
        }
        self.window().endSheet_returnCode(&alert.window(), if ok { 1000 } else { 1001 });
        true
    }

    /// A site asked for `kinds` (camera, microphone, location): use the
    /// saved choice, else ask, remembering the answer unless unticked.
    fn ask_permission(&self, site: &str, kinds: &[&str], done: impl Fn(bool) + 'static) {
        let store = self.app().store();
        let saved: Vec<Option<String>> = kinds.iter().map(|k| store.permission(site, k)).collect();
        if saved.iter().any(|v| v.as_deref() == Some("deny")) {
            return done(false);
        }
        if saved.iter().all(|v| v.as_deref() == Some("allow")) {
            return done(true);
        }
        let what = if kinds == ["camera", "microphone"] {
            "use your camera and microphone".to_string()
        } else {
            let phrases: Vec<&str> =
                kinds.iter().filter_map(|k| SITE_PERMISSIONS.iter().find(|(n, ..)| n == k)).map(|(.., p)| *p).collect();
            phrases.join(" and ")
        };
        let message = NSString::from_str(&format!("{site} wants to {what}"));
        let (this, site, kinds) =
            (self.retain(), site.to_string(), kinds.iter().map(|k| k.to_string()).collect::<Vec<_>>());
        self.sheet(
            &message,
            &["Allow", "Don't Allow"],
            None,
            Some(("Remember for this site", true)),
            move |ok, _, remember| {
                if remember {
                    for k in &kinds {
                        let _ = this.app().store().set_permission(&site, k, if ok { "allow" } else { "deny" });
                    }
                }
                done(ok)
            },
        );
    }

    /// A question as a sheet on this tab; `done(true)` for the first button.
    pub fn confirm(&self, message: &str, buttons: &[&str], done: impl Fn(bool) + 'static) {
        self.sheet(&NSString::from_str(message), buttons, None, None, move |ok, _, _| done(ok));
    }

    /// Show these extension buttons at the right of the toolbar, shrinking
    /// the address bar to make room.
    pub fn set_extension_buttons(&self, items: &[crate::extensions::ExtButton]) {
        let mtm = self.mtm();
        let iv = self.ivars();
        let (Some(window), Some(bar)) = (iv.window.get(), iv.bar.get()) else { return };
        let content = window.contentView().unwrap();
        for (b, _) in iv.ext_buttons.borrow_mut().drain(..) {
            b.removeFromSuperview();
        }
        let width = content.frame().size.width;
        let top = content.frame().size.height - TOOLBAR_H + 2.0;
        let n = items.len() as f64;
        // Left of the extension buttons: blocked requests waiting for an answer.
        let waiting = self.app().net_waiting();
        if let Some(old) = iv.net_button.take() {
            old.removeFromSuperview();
        }
        if waiting {
            let b = unsafe {
                NSButton::buttonWithImage_target_action(
                    &symbol_image("network.badge.shield.half.filled", "Blocked network requests"),
                    Some(self),
                    Some(sel!(showNetworkSettings:)),
                    mtm,
                )
            };
            b.setBordered(false);
            b.setContentTintColor(Some(&objc2_app_kit::NSColor::systemOrangeColor()));
            b.setToolTip(Some(&NSString::from_str(
                "An extension's network request was blocked. Click to allow or deny it.",
            )));
            b.setFrame(NSRect::new(NSPoint::new(width - 4.0 - (n + 1.0) * BUTTON_W, top), NSSize::new(BUTTON_W, 28.0)));
            b.setAutoresizingMask(
                NSAutoresizingMaskOptions::ViewMinXMargin | NSAutoresizingMaskOptions::ViewMinYMargin,
            );
            content.addSubview(&b);
            iv.net_button.replace(Some(b));
        }
        let mut frame = bar.frame();
        frame.size.width = width - frame.origin.x - 8.0 - (n + waiting as u8 as f64) * BUTTON_W;
        bar.setFrame(frame);
        for (i, item) in items.iter().enumerate() {
            let b = unsafe {
                NSButton::buttonWithImage_target_action(&item.image, Some(self), Some(sel!(extensionClicked:)), mtm)
            };
            b.setBordered(false);
            b.setImageScaling(objc2_app_kit::NSImageScaling::ScaleProportionallyDown);
            b.setTag(item.id as isize);
            b.setFrame(NSRect::new(
                NSPoint::new(width - 4.0 - (n - i as f64) * BUTTON_W, top),
                NSSize::new(BUTTON_W, 28.0),
            ));
            b.setAutoresizingMask(
                NSAutoresizingMaskOptions::ViewMinXMargin | NSAutoresizingMaskOptions::ViewMinYMargin,
            );
            // Dimmed while it's off on this site.
            b.setAlphaValue(if item.on { 1.0 } else { 0.45 });
            let hint = if item.always {
                "runs on every site"
            } else if item.on {
                "on for this site"
            } else {
                "off; click to turn it on for this site"
            };
            b.setToolTip(Some(&NSString::from_str(&format!("{}: {hint}", item.label))));
            let menu = NSMenu::new(mtm);
            for (title, tag, on) in
                [("Only when clicked", item.id * 2, !item.always), ("On every site", item.id * 2 + 1, item.always)]
            {
                let m = unsafe {
                    NSMenuItem::initWithTitle_action_keyEquivalent(
                        NSMenuItem::alloc(mtm),
                        &NSString::from_str(title),
                        Some(sel!(extensionAccess:)),
                        &NSString::from_str(""),
                    )
                };
                unsafe { m.setTarget(Some(self)) };
                m.setTag(tag as isize);
                m.setState(if on { 1 } else { 0 });
                menu.addItem(&m);
            }
            menu.addItem(&NSMenuItem::separatorItem(mtm));
            let manage = unsafe {
                NSMenuItem::initWithTitle_action_keyEquivalent(
                    NSMenuItem::alloc(mtm),
                    &NSString::from_str("Manage Extensions…"),
                    Some(sel!(manageExtensions:)),
                    &NSString::from_str(""),
                )
            };
            unsafe { manage.setTarget(Some(self)) };
            menu.addItem(&manage);
            unsafe { b.setMenu(Some(&menu)) };
            content.addSubview(&b);
            iv.ext_buttons.borrow_mut().push((b, item.on));
        }
    }

    /// The toolbar button of extension `id`.
    pub fn extension_button(&self, id: i64) -> Option<Retained<NSButton>> {
        self.ivars().ext_buttons.borrow().iter().find(|(b, _)| b.tag() as i64 == id).map(|(b, _)| b.clone())
    }

    /// (extension id, on) for each button, for the bench.
    pub fn extension_buttons(&self) -> Vec<(i64, bool)> {
        self.ivars().ext_buttons.borrow().iter().map(|(b, on)| (b.tag() as i64, *on)).collect()
    }

    /// A page's first download goes ahead; more downloads from the same page
    /// need the site's permission, so a page can't flood the Downloads folder.
    fn allow_download(&self, done: impl Fn(bool) + 'static) {
        let n = self.ivars().page_downloads.get();
        self.ivars().page_downloads.set(n + 1);
        match crate::store::site_of(&self.url()) {
            Some(site) if n > 0 => self.ask_permission(&site, &["downloads"], done),
            _ => done(true),
        }
    }

    /// The site menu under the site button: protections, permissions, settings.
    fn show_site_menu(&self) {
        let mtm = self.mtm();
        let url = self.url();
        let Some(site) = crate::store::site_of(&url) else { return };
        let store = self.app().store();
        let menu = NSMenu::new(mtm);
        menu.setAutoenablesItems(false);
        let add = |title: &str, action: Option<objc2::runtime::Sel>, tag: isize, on: bool| {
            let item = unsafe {
                NSMenuItem::initWithTitle_action_keyEquivalent(
                    NSMenuItem::alloc(mtm),
                    &NSString::from_str(title),
                    action,
                    &NSString::from_str(""),
                )
            };
            if action.is_some() {
                unsafe { item.setTarget(Some(self)) };
            } else {
                item.setEnabled(false);
            }
            item.setTag(tag);
            item.setState(if on { 1 } else { 0 });
            menu.addItem(&item);
            item
        };
        add(&site, None, 0, false);
        add(
            if url.starts_with("https://") { "Connection is secure" } else { "Connection is not secure" },
            None,
            0,
            false,
        );
        menu.addItem(&NSMenuItem::separatorItem(mtm));
        let shields = store.permission(&site, "shields").as_deref() != Some("off");
        add("Block trackers and fingerprinting", Some(sel!(siteToggle:)), 0, shields);
        let local = store.permission(&site, "local_network").as_deref() == Some("allow");
        add("Allow access to the local network", Some(sel!(siteToggle:)), 1, local);
        menu.addItem(&NSMenuItem::separatorItem(mtm));
        for (i, (kind, label, _)) in SITE_PERMISSIONS.iter().enumerate() {
            let current = match store.permission(&site, kind).as_deref() {
                Some("allow") => 1,
                Some("deny") => 2,
                _ => 0,
            };
            let item = add(label, None, 0, false);
            item.setEnabled(true);
            let sub = NSMenu::new(mtm);
            for (j, choice) in ["Ask", "Allow", "Block"].iter().enumerate() {
                let c = unsafe {
                    NSMenuItem::initWithTitle_action_keyEquivalent(
                        NSMenuItem::alloc(mtm),
                        &NSString::from_str(choice),
                        Some(sel!(sitePermission:)),
                        &NSString::from_str(""),
                    )
                };
                unsafe { c.setTarget(Some(self)) };
                c.setTag((i * 10 + j) as isize);
                c.setState(if j == current { 1 } else { 0 });
                sub.addItem(&c);
            }
            item.setSubmenu(Some(&sub));
        }
        menu.addItem(&NSMenuItem::separatorItem(mtm));
        add("All site settings…", Some(sel!(openSiteSettings:)), 0, false);
        let Some(button) = self.ivars().site_button.get() else { return };
        let below = NSPoint::new(0.0, button.bounds().size.height + 4.0);
        menu.popUpMenuPositioningItem_atLocation_inView(None, below, Some(button));
    }

    /// Replace the page with a short explanation, keeping the failed address
    /// in the address bar (and for Reload). Everything shown is escaped.
    fn show_error(&self, url: &str, title: &str, detail: &str) {
        let Some(web) = self.web() else { return };
        let html = format!(
            "<!doctype html><meta charset=utf-8><meta http-equiv=Content-Security-Policy content=\"default-src 'none'; style-src 'unsafe-inline'\"><title>{t}</title>\
             <style>body{{font:15px -apple-system,sans-serif;max-width:560px;margin:18vh auto;padding:0 24px;color:#333}}\
             @media(prefers-color-scheme:dark){{body{{background:#1c1c1e;color:#ddd}}}}h1{{font-size:22px}}p{{line-height:1.5}}code{{word-break:break-all;opacity:.7}}</style>\
             <h1>{t}</h1><p>{d}</p><p><code>{u}</code></p>",
            t = escape_html(title),
            d = escape_html(detail),
            u = escape_html(url)
        );
        self.ivars().error_page.set(true);
        let html = NSString::from_str(&html);
        let unreachable = NSURL::URLWithString(&NSString::from_str(url));
        let private = sel!(_loadAlternateHTMLString:baseURL:forUnreachableURL:);
        let responds: bool = unsafe { msg_send![&*web, respondsToSelector: private] };
        match unreachable {
            Some(u) if responds => {
                let _: () = unsafe {
                    msg_send![&*web, _loadAlternateHTMLString: &*html, baseURL: Option::<&NSURL>::None, forUnreachableURL: &*u]
                };
            }
            _ => {
                unsafe { web.loadHTMLString_baseURL(&html, None) };
            }
        }
    }
}

/// Text safe to put in HTML.
pub fn escape_html(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&#39;")
}

/// Extensions may point tabs at web pages and their own pages, never at the
/// browser's internal pages (which hold the settings bridge) or local files.
pub fn extension_may_load(url: &str) -> bool {
    let scheme = url.split(':').next().unwrap_or("").to_ascii_lowercase();
    !matches!(scheme.as_str(), "mybrowser" | "file" | "javascript" | "")
}

/// Whether a navigation replaces the whole page (not a frame inside it). A new
/// window has no target frame yet; it's a page too.
fn is_main_frame(action: &WKNavigationAction) -> bool {
    unsafe { action.targetFrame() }.is_none_or(|f| unsafe { f.isMainFrame() })
}

/// `Content-Disposition: attachment` means "save this", even for viewable types.
unsafe fn is_attachment(response: &WKNavigationResponse) -> bool {
    let r = unsafe { response.response() };
    let responds: bool = unsafe { msg_send![&*r, respondsToSelector: sel!(valueForHTTPHeaderField:)] };
    if !responds {
        return false;
    }
    let value: Option<Retained<NSString>> =
        unsafe { msg_send![&*r, valueForHTTPHeaderField: &*NSString::from_str("Content-Disposition")] };
    value.is_some_and(|v| v.to_string().trim_start().to_ascii_lowercase().starts_with("attachment"))
}

fn symbol_image(symbol: &str, label: &str) -> Retained<NSImage> {
    NSImage::imageWithSystemSymbolName_accessibilityDescription(
        &NSString::from_str(symbol),
        Some(&NSString::from_str(label)),
    )
    .expect("SF Symbol missing")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extensions_cannot_open_internal_pages_or_files() {
        assert!(extension_may_load("https://example.com/"));
        assert!(extension_may_load("webkit-extension://abc/options.html"));
        assert!(extension_may_load("about:blank"));
        for bad in
            ["mybrowser://settings", "MyBrowser://settings#extensions", "file:///etc/passwd", "javascript:alert(1)", ""]
        {
            assert!(!extension_may_load(bad), "{bad}");
        }
    }

    #[test]
    fn error_pages_escape_what_they_show() {
        assert_eq!(
            escape_html(r#"<img src=x onerror="a('b')">&"#),
            "&lt;img src=x onerror=&quot;a(&#39;b&#39;)&quot;&gt;&amp;"
        );
    }
}
