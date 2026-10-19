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
/// "Check for Updates…": the latest GitHub release, opened as a normal page
/// (the browser itself never checks).
pub const RELEASES: &str = "https://github.com/shu-vro/my-browser/releases/latest";

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
    /// The extension window this tab was last seen in, and its place there,
    /// to tell extensions when it moves to another window.
    pub ext_window: RefCell<Option<(Retained<crate::extwindow::ExtWindow>, usize)>>,
    /// A private window's tab: memory-only cookies and site data, no
    /// history, no session, no extensions.
    private: Cell<bool>,
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
    /// Star right of the address bar: bookmark this page (⌘D).
    star: OnceCell<Retained<NSButton>>,
    shown_star: Cell<Option<bool>>,
    /// JavaScript dialogs on the current page, and whether the user blocked more.
    dialogs: Cell<u32>,
    dialogs_blocked: Cell<bool>,
    /// The sheet on screen (answered by the bench `dialog` command in tests).
    alert: RefCell<Option<Retained<NSAlert>>>,
    /// Recent page-process crashes: (count, last), to stop a crash-reload loop.
    crashes: Cell<(u32, Option<Instant>)>,
    /// The next finished load is our error page, not a visit.
    error_page: Cell<bool>,
    /// The saved-passwords menu last offered.
    fill: RefCell<Option<FillMenu>>,
    /// The autofill menu last offered: (frame of the form, entry ids).
    autofill: RefCell<Option<(Retained<WKFrameInfo>, Vec<i64>)>>,
    /// Reader mode: the next commit is the reader page; the page shown is one.
    reader_loading: Cell<bool>,
    reader: Cell<bool>,
    /// Extension buttons at the right of the toolbar, and (id, on) for each.
    ext_buttons: RefCell<Vec<(Retained<NSButton>, bool)>>,
    /// Shown while a network request waits for an answer (netgate.rs).
    net_button: RefCell<Option<Retained<NSButton>>>,
    /// "Add to my-browser", on Chrome Web Store extension pages.
    store_button: RefCell<Option<Retained<NSButton>>>,
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
    /// The page process's usage at the last heavy-tab check, and when.
    sampled: Cell<Option<(crate::usage::Usage, Instant)>>,
    /// Why this background tab is heavy (badge in its tab), if it is.
    heavy: RefCell<Option<String>>,
    /// The address bar's suggestion list, made on first use.
    suggest: OnceCell<Retained<crate::suggest::Suggest>>,
    /// Pinned: kept at the front of its window and never put to sleep.
    pinned: Cell<bool>,
    /// The page's sound is off (kept across navigations and sleep).
    muted: Cell<bool>,
    /// Translate Page: the page shows translated text; and which run of it
    /// replies belong to (a new page or Show Original drops late ones).
    translated: Cell<bool>,
    translate_run: Cell<u64>,
    /// Its video went to picture in picture when the user left the tab.
    auto_pip: Cell<bool>,
    /// The PDF viewer over the page (pdf.rs), and a PDF on its way.
    pdf: RefCell<Option<crate::pdf::Viewer>>,
    pdf_expected: Cell<bool>,
}

/// The saved-passwords menu: (site, frame of the login form, usernames).
type FillMenu = (String, Retained<WKFrameInfo>, Vec<String>);

/// Permissions a site asks for: (kind, site menu label, what the prompt says it wants to do).
const SITE_PERMISSIONS: [(&str, &str, &str); 5] = [
    ("camera", "Camera", "use your camera"),
    ("microphone", "Microphone", "use your microphone"),
    ("screen", "Screen sharing", "share your screen"),
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
            if is_main_frame(action) {
                if !self.app().shields_up(&url) {
                    crate::privacy::harden_webpage_preferences(prefs, false);
                }
                // JavaScript off for this site: the page and its frames run no scripts.
                unsafe { prefs.setAllowsContentJavaScript(self.app().javascript_allowed(&url)) };
                if crate::store::site_of(&url).is_some_and(|h| crate::privacy::is_local_host(&h)) {
                    crate::privacy::keep_scheme(prefs);
                }
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
            if unsafe { response.isForMainFrame() } {
                let mime = unsafe { response.response().MIMEType() }.map(|m| m.to_string()).unwrap_or_default();
                self.ivars().pdf_expected.set(mime == "application/pdf");
            }
            let download = unsafe { !response.canShowMIMEType() || is_attachment(response) };
            if !download {
                return decide.call((WKNavigationResponsePolicy::Allow,));
            }
            // A file on this Mac WebKit can't show isn't downloaded (copied):
            // it opens in its app.
            let local = unsafe { response.response().URL() }.filter(|u| u.isFileURL()).and_then(|u| u.path());
            if let (Some(path), true) = (local, unsafe { response.isForMainFrame() }) {
                decide.call((WKNavigationResponsePolicy::Cancel,));
                return self.open_in_app(std::path::Path::new(&path.to_string()));
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
        fn did_commit_navigation(&self, web: &WKWebView, _nav: Option<&WKNavigation>) {
            // The site's own zoom (a zoom set on one site doesn't follow you to the next).
            let url = unsafe { web.URL() }.and_then(|u| u.absoluteString()).map(|u| u.to_string()).unwrap_or_default();
            let zoom = crate::store::site_of(&url).map_or(1.0, |site| self.app().store().zoom(&site));
            unsafe { web.setPageZoom(zoom) };
            self.ivars().visit.set(None);
            self.ivars().reader.set(self.ivars().reader_loading.replace(false));
            self.ivars().translated.set(false);
            self.close_pdf();
            self.ivars().translate_run.set(self.ivars().translate_run.get() + 1);
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
            let reader = self.ivars().reader.get();
            if !error_page && !reader && !self.is_private() && (url.starts_with("http://") || url.starts_with("https://")) {
                let id = self.app().store().add_visit(&url, &self.ivars().title.borrow());
                self.ivars().visit.set(Some(id));
            }
            if self.ivars().pdf_expected.replace(false) {
                self.show_pdf();
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
            let tab = self.app().open_tab_with_config(config, self.is_private());
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

        /// Screen sharing (`getDisplayMedia`, WKUIDelegate private): the
        /// site's saved choice, else ask; then macOS's own picker chooses
        /// what to share.
        #[unsafe(method(_webView:requestDisplayCapturePermissionForOrigin:initiatedByFrame:withSystemAudio:decisionHandler:))]
        fn display_capture_permission(
            &self,
            _web: &WKWebView,
            origin: &WKSecurityOrigin,
            _frame: &WKFrameInfo,
            _audio: Bool,
            decide: &DynBlock<dyn Fn(isize)>,
        ) {
            let decide = decide.copy();
            let site = unsafe { origin.host() }.to_string().to_ascii_lowercase();
            // WKDisplayCapturePermissionDecision: Deny 0, ScreenPrompt 1.
            self.ask_permission(&site, &["screen"], move |ok| decide.call((ok as isize,)));
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
        /// Typing in the address bar: suggestions.
        #[unsafe(method(controlTextDidChange:))]
        fn text_did_change(&self, n: &NSNotification) {
            if let Some(bar) = self.ivars().bar.get().filter(|b| n.object().is_some_and(|o| std::ptr::eq(&*o, &***b as &NSObject as *const NSObject as *const AnyObject))) {
                self.suggest_for(&bar.stringValue().to_string());
            }
        }

        #[unsafe(method(controlTextDidEndEditing:))]
        fn text_did_end_editing(&self, _n: &NSNotification) {
            self.hide_suggestions();
        }

        /// Address bar: Enter = go (handled here, so AppKit doesn't beep).
        /// Find field keys: Enter = next, Shift+Enter = previous, Esc = close.
        #[unsafe(method(control:textView:doCommandBySelector:))]
        fn find_field_command(&self, control: &NSControl, _view: &NSTextView, command: objc2::runtime::Sel) -> bool {
            let bar = self.ivars().bar.get().is_some_and(|b| std::ptr::eq::<NSControl>(&***b, control));
            if bar {
                self.bar_command(control, command)
            } else if command == sel!(cancelOperation:) {
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

        /// Site menu: Shields (tag 0), local network access (tag 1) or
        /// JavaScript (tag 2) toggled.
        #[unsafe(method(siteToggle:))]
        fn site_toggle_action(&self, sender: &NSMenuItem) {
            let Some(site) = crate::store::site_of(&self.url()) else { return };
            let store = self.app().store();
            if sender.tag() == 2 {
                let off = store.permission(&site, "javascript").as_deref() == Some("deny");
                let _ = store.set_permission(&site, "javascript", if off { "ask" } else { "deny" });
                return self.reload();
            }
            let _ = if sender.tag() == 0 {
                let off = store.permission(&site, "shields").as_deref() == Some("off");
                store.set_permission(&site, "shields", if off { "ask" } else { "off" })
            } else {
                let allowed = store.permission(&site, "local_network").as_deref() == Some("allow");
                store.set_permission(&site, "local_network", if allowed { "ask" } else { "allow" })
            };
            // The blockers are recompiled with the new exception, then the page reloads under them.
            let this = self.retain();
            self.app().load_shields(move |_, _| this.reload());
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
                self.remember_search(&text);
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

        /// Site menu: "Hide Element on Page…".
        #[unsafe(method(pickElement:))]
        fn pick_element_action(&self, _s: Option<&AnyObject>) {
            self.pick_element();
        }

        /// Site menu: "Site Data…".
        #[unsafe(method(showSiteData:))]
        fn show_site_data_action(&self, _s: Option<&AnyObject>) {
            self.show_site_data();
        }

        /// A trash button in the site data sheet: delete that site's data,
        /// then show the sheet again.
        #[unsafe(method(deleteSiteData:))]
        fn delete_site_data_action(&self, sender: &AnyObject) {
            let Some(name) = crate::sitedata::button_site(sender) else { return };
            if let Some(sheet) = self.window().attachedSheet() {
                self.window().endSheet_returnCode(&sheet, 1000);
            }
            let this = self.retain();
            crate::sitedata::delete(&self.data_store(), &name, move |_| this.show_site_data());
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
            if key == "_isPlayingAudio" {
                return self.refresh_badges();
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
        #[unsafe(method(goBack:))]
        fn go_back_action(&self, _s: Option<&AnyObject>) {
            self.go_back();
        }

        #[unsafe(method(showNetworkSettings:))]
        fn show_network_settings(&self, _s: Option<&AnyObject>) {
            self.app().open_settings_page("settings#network");
        }

        #[unsafe(method(toggleBookmark:))]
        fn toggle_bookmark_action(&self, _s: Option<&AnyObject>) {
            self.toggle_bookmark();
        }

        /// An account picked in the saved-passwords menu (tag = its index).
        #[unsafe(method(fillPassword:))]
        fn fill_password_action(&self, sender: &NSMenuItem) {
            self.fill_password_at(sender.tag() as usize);
        }

        /// An entry picked in the autofill menu (tag = its index).
        #[unsafe(method(fillAutofill:))]
        fn fill_autofill_action(&self, sender: &NSMenuItem) {
            self.fill_autofill_at(sender.tag() as usize);
        }

        #[unsafe(method(openAutofillSettings:))]
        fn open_autofill_settings_action(&self, _s: Option<&AnyObject>) {
            self.app().open_settings_page("settings#autofill");
        }

        #[unsafe(method(openPasswordSettings:))]
        fn open_password_settings_action(&self, _s: Option<&AnyObject>) {
            self.app().open_settings_page("settings#passwords");
        }

        /// "Add to my-browser" on a Chrome Web Store page: load the package
        /// from Google in this tab (it downloads), then offer to install it.
        #[unsafe(method(addFromWebStore:))]
        fn add_from_web_store_action(&self, _s: Option<&AnyObject>) {
            if let Some(id) = crate::extensions::webstore_id(&self.url()) {
                self.app().expect_package(&id);
                self.load(&crate::extensions::webstore_package_url(&id));
            }
        }

        /// Right-click menu (WKUIDelegate, private): links get "Open Link in
        /// New Tab", "…New Window" and "…Private Window". Built only when
        /// the menu opens; nothing runs in the page.
        #[unsafe(method(_webView:getContextMenuFromProposedMenu:forElement:userInfo:completionHandler:))]
        fn context_menu(
            &self,
            _web: &WKWebView,
            menu: &NSMenu,
            element: &AnyObject,
            _info: Option<&AnyObject>,
            done: &DynBlock<dyn Fn(*mut NSMenu)>,
        ) {
            self.link_menu(menu, link_url(element).as_deref());
            done.call((menu as *const NSMenu as *mut NSMenu,));
        }

        #[unsafe(method(openLinkInNewTab:))]
        fn open_link_in_new_tab(&self, sender: &NSMenuItem) {
            if let Some(url) = represented_url(sender) {
                self.app().open_tab_in(self.is_private(), &url, true);
            }
        }

        #[unsafe(method(openLinkInNewWindow:))]
        fn open_link_in_new_window(&self, sender: &NSMenuItem) {
            if let Some(url) = represented_url(sender) {
                self.app().open_window(self.is_private(), &url);
            }
        }

        #[unsafe(method(openLinkInPrivateWindow:))]
        fn open_link_in_private_window(&self, sender: &NSMenuItem) {
            if let Some(url) = represented_url(sender) {
                self.app().open_tab_in(true, &url, false);
            }
        }

        /// The print operation ended (printed, saved, or cancelled).
        #[unsafe(method(printOperationDidRun:success:contextInfo:))]
        fn print_done(&self, _op: &AnyObject, ok: Bool, _ctx: *mut c_void) {
            if self.app().is_bench() {
                say!("PRINTED {}", ok.as_bool() as u8);
            }
        }

        /// The page called `window.print()` (WKUIDelegate, private).
        #[unsafe(method(_webView:printFrame:pdfFirstPageSize:completionHandler:))]
        fn page_print(&self, _web: &WKWebView, _frame: &AnyObject, _size: NSSize, done: &DynBlock<dyn Fn()>) {
            if !self.ivars().dialogs_blocked.get() {
                self.print(None);
            }
            done.call(());
        }

        /// The PDF viewer's toolbar (tag = its place in `pdf::BUTTONS`).
        #[unsafe(method(pdfCommand:))]
        fn pdf_command_action(&self, sender: &NSButton) {
            self.pdf_command(sender.tag() as usize);
        }

        /// The PDF viewer's page number field: go to that page.
        #[unsafe(method(pdfGoToPage:))]
        fn pdf_go_to_page_action(&self, sender: &NSTextField) {
            if let Some(pdf) = self.ivars().pdf.borrow().as_ref() {
                pdf.go_to_page(sender.stringValue().to_string().trim().parse().unwrap_or(1));
            }
        }

        /// The PDF's page or zoom changed (PDFKit notification).
        #[unsafe(method(pdfChanged:))]
        fn pdf_changed(&self, _n: &NSNotification) {
            // (Busy when the change comes from our own command, which updates after.)
            if let Ok(pdf) = self.ivars().pdf.try_borrow()
                && let Some(pdf) = pdf.as_ref()
            {
                pdf.update();
            }
        }

        /// Reading a PDF aloud: the word about to be read is selected.
        #[unsafe(method(speechSynthesizer:willSpeakWord:ofString:))]
        fn will_speak_word(&self, _s: &AnyObject, range: objc2_foundation::NSRange, _text: &NSString) {
            if let Some(pdf) = self.ivars().pdf.borrow_mut().as_mut() {
                pdf.speaking(range);
            }
        }

        #[unsafe(method(speechSynthesizer:didFinishSpeaking:))]
        fn did_finish_speaking(&self, _s: &AnyObject, _finished: Bool) {
            if let Ok(mut pdf) = self.ivars().pdf.try_borrow_mut()
                && let Some(pdf) = pdf.as_mut()
            {
                pdf.done_speaking();
            }
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

        /// The sound badge in the tab.
        #[unsafe(method(toggleMute:))]
        fn toggle_mute_action(&self, _s: Option<&AnyObject>) {
            self.set_muted(!self.is_muted());
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
        private: bool,
    ) -> Retained<Tab> {
        let mtm = app.mtm();
        let this = Self::alloc(mtm).set_ivars(TabIvars::default());
        let this: Retained<Tab> = unsafe { msg_send![super(this), init] };
        let _ = this.ivars().app.set(app.retain());
        this.ivars().private.set(private);
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

    pub fn app(&self) -> &App {
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

    /// The page's process, while awake (private `_webProcessIdentifier`).
    pub fn pid(&self) -> Option<i32> {
        let web = self.web()?;
        let known: bool = unsafe { msg_send![&*web, respondsToSelector: sel!(_webProcessIdentifier)] };
        let pid: i32 = if known { unsafe { msg_send![&*web, _webProcessIdentifier] } } else { 0 };
        (pid > 0).then_some(pid)
    }

    /// Mark the tab heavy if it's in the background using a lot of memory or
    /// CPU (sampled on the sleep timer's tick); clear it otherwise.
    pub fn check_heavy(&self) {
        let usage = self.pid().filter(|_| !self.app().is_active(self)).and_then(crate::usage::of);
        let before = self.ivars().sampled.replace(usage.map(|u| (u, Instant::now())));
        let heavy = usage.and_then(|u| crate::usage::heavy(u, before.map(|(b, at)| (b, at.elapsed().as_secs_f64()))));
        if *self.ivars().heavy.borrow() != heavy {
            self.ivars().heavy.replace(heavy);
            self.refresh_badges();
        }
    }

    pub fn heavy(&self) -> Option<String> {
        self.ivars().heavy.borrow().clone()
    }

    pub fn is_pinned(&self) -> bool {
        self.ivars().pinned.get()
    }

    /// Pin or unpin. A pinned tab moves to the front of its window, after
    /// the tabs pinned before it; `restoring` keeps the order it had.
    pub fn set_pinned(&self, on: bool, restoring: bool) {
        self.ivars().pinned.set(on);
        if on
            && !restoring
            && let Some(group) = self.window().tabGroup()
        {
            let window = self.window();
            let app = self.app();
            let pinned_before = group
                .windows()
                .iter()
                .filter(|w| !std::ptr::eq(&**w, window) && app.tab_for_window(w).is_some_and(|t| t.is_pinned()))
                .count();
            let at = group.windows().iter().position(|w| std::ptr::eq(&*w, window));
            if at != Some(pinned_before) {
                // (Moving it with addTabbedWindow reordered the list but not the tab bar.)
                group.removeWindow(window);
                group.insertWindow_atIndex(window, pinned_before as isize);
                // The re-inserted tab drops its accessory view after this turn.
                let this = self.retain();
                crate::on_main(move || this.refresh_badges());
            }
            if app.is_active(self) {
                app.select(self);
            }
        }
        self.refresh_badges();
    }

    pub fn is_muted(&self) -> bool {
        self.ivars().muted.get()
    }

    pub fn set_muted(&self, on: bool) {
        self.ivars().muted.set(on);
        self.apply_mute();
        self.refresh_badges();
    }

    /// WebKit's page mute (private `_setPageMuted:`, `_WKMediaAudioMuted` = 1).
    fn apply_mute(&self) {
        let Some(web) = self.web() else { return };
        if responds(&web, sel!(_setPageMuted:)) {
            let state: usize = if self.is_muted() { 1 } else { 0 };
            let _: () = unsafe { msg_send![&*web, _setPageMuted: state] };
        }
    }

    /// Sound is playing in the page (private `_isPlayingAudio`, observed).
    pub fn playing_audio(&self) -> bool {
        self.web().is_some_and(|w| responds(&w, sel!(_isPlayingAudio)) && unsafe { msg_send![&*w, _isPlayingAudio] })
    }

    /// `TABFLAGS pinned=<0|1> muted=<0|1> page_muted=<0|1> audio=<0|1> swipe=<0|1>`;
    /// page_muted and swipe are read back from WebKit.
    pub fn flags_state(&self) -> String {
        let page_muted = self.web().is_some_and(|w| {
            let state: usize =
                if responds(&w, sel!(_mediaMutedState)) { unsafe { msg_send![&*w, _mediaMutedState] } } else { 0 };
            state & 1 == 1
        });
        format!(
            "TABFLAGS pinned={} muted={} page_muted={} audio={} swipe={}",
            self.is_pinned() as u8,
            self.is_muted() as u8,
            page_muted as u8,
            self.playing_audio() as u8,
            self.web().is_some_and(|w| unsafe { w.allowsBackForwardNavigationGestures() }) as u8
        )
    }

    /// Small icons in the tab itself (its `NSWindowTab` accessory view):
    /// pinned, sound playing or muted (click to toggle), and a heavy
    /// background tab, each with its tooltip.
    fn refresh_badges(&self) {
        let mut badges: Vec<(&str, String, Option<objc2::runtime::Sel>)> = Vec::new();
        if self.is_pinned() {
            badges.push(("pin.fill", "Pinned: stays awake".into(), None));
        }
        if self.is_muted() {
            badges.push(("speaker.slash.fill", "Muted. Click to turn sound on".into(), Some(sel!(toggleMute:))));
        } else if self.playing_audio() {
            badges.push(("speaker.wave.2.fill", "Playing sound. Click to mute".into(), Some(sel!(toggleMute:))));
        }
        if let Some(why) = self.heavy() {
            badges.push((
                "gauge.with.dots.needle.67percent",
                format!("{why}. It sleeps when left alone; see Settings → Tabs."),
                None,
            ));
        }
        let tab = self.window().tab();
        if badges.is_empty() {
            tab.setAccessoryView(None);
            tab.setToolTip(None);
            return;
        }
        let mtm = self.mtm();
        let views: Vec<Retained<NSView>> = badges
            .iter()
            .map(|(symbol, label, action)| {
                let b = unsafe {
                    NSButton::buttonWithImage_target_action(&symbol_image(symbol, label), Some(self), *action, mtm)
                };
                b.setBordered(false);
                b.setToolTip(Some(&NSString::from_str(label)));
                b.setContentTintColor(Some(&objc2_app_kit::NSColor::secondaryLabelColor()));
                Retained::into_super(Retained::into_super(b))
            })
            .collect();
        let stack = objc2_app_kit::NSStackView::stackViewWithViews(&NSArray::from_retained_slice(&views), mtm);
        stack.setSpacing(2.0);
        tab.setAccessoryView(Some(&stack));
        let tips: Vec<&str> = badges.iter().map(|(_, l, _)| l.as_str()).collect();
        tab.setToolTip(Some(&NSString::from_str(&tips.join("\n"))));
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
        // Private and normal tabs never share a window.
        window.setTabbingIdentifier(&NSString::from_str(if self.is_private() { "private" } else { "browser" }));
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
        if self.is_private() {
            // A dark address bar marks a private window, as in Safari.
            bar.setDrawsBackground(true);
            bar.setBackgroundColor(Some(&objc2_app_kit::NSColor::colorWithSRGBRed_green_blue_alpha(
                0.17, 0.17, 0.22, 1.0,
            )));
            bar.setTextColor(Some(&objc2_app_kit::NSColor::whiteColor()));
            bar.setPlaceholderString(Some(&NSString::from_str("Private: search or enter address")));
        } else if let Some(profile) = self.app().profile_name() {
            // Which profile this window is (Settings → Profiles).
            bar.setPlaceholderString(Some(&NSString::from_str(&format!("{profile}: search or enter address"))));
        } else {
            bar.setPlaceholderString(Some(&NSString::from_str("Search or enter address")));
        }
        // No action: Return is handled in `control:textView:doCommandBySelector:`.
        // An action also fires when editing ends, so clicking away reloaded the page.
        unsafe { bar.setDelegate(Some(ProtocolObject::from_ref(self))) };
        content.addSubview(&bar);
        let _ = self.ivars().bar.set(bar);
        let star = unsafe {
            NSButton::buttonWithImage_target_action(
                &symbol_image("star", "Bookmark this page"),
                Some(self),
                Some(sel!(toggleBookmark:)),
                mtm,
            )
        };
        star.setBordered(false);
        star.setAutoresizingMask(NSAutoresizingMaskOptions::ViewMinXMargin | pin_top);
        content.addSubview(&star);
        let _ = self.ivars().star.set(star);
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
            // Two-finger swipe left/right on the trackpad goes back/forward.
            web.setAllowsBackForwardNavigationGestures(true);
            // Web Inspector (and Safari's Develop menu) may attach, unless turned off.
            web.setInspectable(self.app().store().flag("devtools"));
        }
        content.addSubview(&web);
        for key in observed(&web) {
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
        self.apply_mute();
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
            if let Some(pdf) = self.ivars().pdf.borrow().as_ref() {
                pdf.root.setFrame(self.web_frame());
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
        self.find_js("__mybFind.clear()", None, |_| {});
        if let Some(pdf) = self.ivars().pdf.borrow_mut().as_mut() {
            pdf.clear_find();
            pdf.root.setFrame(self.web_frame());
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
            self.find_js("__mybFind.clear()", None, |_| {});
            return;
        }
        // In a PDF, PDFKit finds (and highlights every match).
        let in_pdf = iv.pdf.borrow_mut().as_mut().map(|pdf| pdf.find(&text, backwards));
        if let Some((i, n)) = in_pdf {
            iv.find_last.replace(text);
            let status = if n == 0 { "No matches".to_string() } else { format!("{i} of {n}") };
            return self.find_done(n > 0, &status);
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
            let (tab, term) = (this.clone(), text.clone());
            let done = RcBlock::new(move |result: std::ptr::NonNull<WKFindResult>| {
                let found = unsafe { result.as_ref().matchFound() };
                if !found {
                    tab.find_js("__mybFind.clear()", None, |_| {});
                    return tab.find_done(false, "No matches");
                }
                // Every match highlighted, the current one apart, and "3 of 17".
                let args = objc2_foundation::NSDictionary::from_retained_objects(
                    &[&*NSString::from_str("t")],
                    &[Retained::<AnyObject>::from(NSString::from_str(&term))],
                );
                let t = tab.clone();
                tab.find_js("return __mybFind.mark(t)", Some(&args), move |r| {
                    let counts =
                        r.and_then(|r| r.downcast::<NSString>().ok()).and_then(|j| find_counts(&j.to_string()));
                    let status = match counts {
                        Some((n, i)) if i > 0 => format!("{i} of {n}"),
                        Some((n, _)) if n > 1 => format!("{n} matches"),
                        _ => String::new(),
                    };
                    t.find_done(true, &status);
                });
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

    /// A find finished: show its status, and tell the bench.
    fn find_done(&self, found: bool, status: &str) {
        if let Some(label) = self.ivars().find_status.get() {
            label.setStringValue(&NSString::from_str(status));
        }
        if self.ivars().find_report.replace(false) {
            say!("FIND found={}", found as u8);
        }
    }

    /// resources/find.js's functions, then `body`.
    fn find_js(
        &self,
        body: &str,
        args: Option<&objc2_foundation::NSDictionary<NSString, AnyObject>>,
        done: impl FnOnce(Option<Retained<AnyObject>>) + 'static,
    ) {
        self.client_js(&format!("{FIND_JS}\n{body}"), args, done);
    }

    fn ensure_web(&self) -> Retained<WKWebView> {
        if let Some(web) = self.web() {
            return web;
        }
        let config = self.app().make_config(self.is_private());
        self.attach_web(&config)
    }

    /// Drop the web view and its page process. Observers go first: KVO on a
    /// deallocated object crashes.
    fn destroy_web(&self) {
        self.close_pdf();
        let Some(web) = self.ivars().web.take() else { return };
        for key in observed(&web) {
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
        self.refresh_badges();
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

    /// Address bar keys: ↑/↓ and Esc for the suggestion list, Return to go
    /// (to the chosen suggestion, else to what was typed). True if handled.
    fn bar_command(&self, control: &NSControl, command: objc2::runtime::Sel) -> bool {
        let suggest = self.ivars().suggest.get();
        if command == sel!(moveDown:) || command == sel!(moveUp:) {
            return suggest.is_some_and(|s| s.step(if command == sel!(moveDown:) { 1 } else { -1 }));
        }
        if command == sel!(cancelOperation:) {
            let shown = suggest.is_some_and(|s| s.is_shown());
            self.hide_suggestions();
            return shown;
        }
        if command != sel!(insertNewline:) {
            return false;
        }
        if let Some(item) = suggest.and_then(|s| s.chosen()) {
            self.open_suggestion(&item);
            return true;
        }
        self.hide_suggestions();
        let text = control.stringValue().to_string();
        let url = self.app().normalize(&text);
        if text.trim().is_empty() {
        } else if let Some(web) = self.web().filter(|w| {
            let typed = NSURL::URLWithString(&NSString::from_str(&url)).and_then(|u| u.absoluteString());
            typed.is_some() && typed == unsafe { w.URL() }.and_then(|u| u.absoluteString())
        }) {
            // Unchanged: back to the page, no reload (⌘R reloads).
            self.window().makeFirstResponder(Some(&web));
        } else {
            if !crate::is_address(&text) {
                self.remember_search(&text);
            }
            self.load(&url);
        }
        true
    }

    /// Keep a search typed in the address bar or the new tab page, for
    /// suggestions (not from private windows, not with suggestions off).
    pub fn remember_search(&self, text: &str) {
        let store = self.app().store();
        if !self.is_private() && store.flag("address_suggestions") {
            store.add_search(text);
        }
    }

    /// Show what matches `typed` under the address bar.
    fn suggest_for(&self, typed: &str) {
        let store = self.app().store();
        let items = if store.flag("address_suggestions") { store.suggestions(typed, 8) } else { Vec::new() };
        let Some(bar) = self.ivars().bar.get() else { return };
        self.ivars().suggest.get_or_init(|| crate::suggest::Suggest::new(self)).show(items, bar);
    }

    fn hide_suggestions(&self) {
        if let Some(s) = self.ivars().suggest.get() {
            s.hide();
        }
    }

    /// Go to a suggestion (picked with Return or a click).
    pub fn open_suggestion(&self, item: &crate::store::Suggestion) {
        self.hide_suggestions();
        match item {
            crate::store::Suggestion::Search(text) => {
                self.remember_search(text);
                let url = self.app().normalize(text);
                self.load(&url);
            }
            crate::store::Suggestion::Page(url, _) => self.load(url),
        }
    }

    /// Bench: type `text` into the address bar (`SUGGEST search:<text>|page:<url>|…`),
    /// or press a key there (`up`, `down`, `enter`, `esc`).
    pub fn bar_type(&self, text: &str) -> String {
        self.focus_address_bar();
        let Some(editor) = self.window().firstResponder().and_then(|r| r.downcast::<NSTextView>().ok()) else {
            return "SUGGEST error no editor".into();
        };
        editor.setString(&NSString::from_str(text));
        self.suggest_for(text);
        self.suggest_state()
    }

    pub fn bar_press(&self, key: &str) -> String {
        let command = match key {
            "up" => sel!(moveUp:),
            "down" => sel!(moveDown:),
            "enter" => sel!(insertNewline:),
            _ => sel!(cancelOperation:),
        };
        if let Some(editor) = self.window().firstResponder().and_then(|r| r.downcast::<NSTextView>().ok()) {
            let _: () = unsafe { msg_send![&*editor, doCommandBySelector: command] };
        }
        self.suggest_state()
    }

    fn suggest_state(&self) -> String {
        let Some(s) = self.ivars().suggest.get().filter(|s| s.is_shown()) else { return "SUGGEST ".into() };
        let chosen = s.chosen();
        let items: Vec<String> = s
            .items()
            .iter()
            .map(|i| {
                let mark = if Some(i) == chosen.as_ref() { "*" } else { "" };
                match i {
                    crate::store::Suggestion::Search(t) => format!("{mark}search:{t}"),
                    crate::store::Suggestion::Page(u, _) => format!("{mark}page:{u}"),
                }
            })
            .collect();
        format!("SUGGEST {}", items.join("|"))
    }

    pub fn load(&self, url: &str) {
        let Some(nsurl) = NSURL::URLWithString(&NSString::from_str(url)) else { return };
        // A folder shows as the browser's folder page.
        if nsurl.isFileURL()
            && let Some(path) = nsurl.path().map(|p| std::path::PathBuf::from(p.to_string())).filter(|p| p.is_dir())
        {
            return self.load(&crate::files::folder_url(&path));
        }
        self.hide_suggestions();
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

    /// A file or folder on this Mac, from a folder page: a folder's page, or
    /// the file in this tab (WebKit shows it, or `decide_response` opens it
    /// in its app).
    pub fn open_local(&self, path: &std::path::Path) {
        if path.is_dir() {
            self.load(&crate::files::folder_url(path));
        } else {
            self.load(&crate::files::file_url(path));
        }
    }

    /// A file WebKit can't show: its default app opens it. Apps, scripts and
    /// installers are only shown in Finder: opening them would run them.
    fn open_in_app(&self, path: &std::path::Path) {
        let p = path.to_string_lossy().into_owned();
        if self.app().is_bench() {
            return say!("OPENLOCAL {} {p}", if crate::files::runs_code(path) { "reveal" } else { "app" });
        }
        if crate::files::runs_code(path) {
            return reveal(&p);
        }
        objc2_app_kit::NSWorkspace::sharedWorkspace().openURL(&NSURL::fileURLWithPath(&NSString::from_str(&p)));
    }

    /// A login field got focus: show the accounts saved for this site in a
    /// menu under it. Nothing is filled until one is picked.
    pub fn offer_passwords(&self, site: &str, frame: &WKFrameInfo, x: f64, y: f64) {
        let accounts = self.app().vault().accounts(site);
        if accounts.is_empty() {
            return;
        }
        self.ivars().fill.replace(Some((site.to_string(), frame.retain(), accounts.clone())));
        // A menu is modal: the bench reports it and picks with `fillpassword`.
        if self.app().is_bench() {
            return say!("PASSWORDS {}", accounts.join("|"));
        }
        let Some(web) = self.web() else { return };
        let mtm = self.mtm();
        let menu = NSMenu::new(mtm);
        let add = |title: &str, action: objc2::runtime::Sel, tag: usize| {
            let item = unsafe {
                NSMenuItem::initWithTitle_action_keyEquivalent(
                    NSMenuItem::alloc(mtm),
                    &NSString::from_str(title),
                    Some(action),
                    &NSString::from_str(""),
                )
            };
            unsafe { item.setTarget(Some(self)) };
            item.setTag(tag as isize);
            menu.addItem(&item);
        };
        for (i, user) in accounts.iter().enumerate() {
            add(if user.is_empty() { "(no username)" } else { user }, sel!(fillPassword:), i);
        }
        menu.addItem(&NSMenuItem::separatorItem(mtm));
        add("Manage Passwords…", sel!(openPasswordSettings:), 0);
        self.pop_up_under_field(&menu, &web, frame, x, y);
    }

    /// Show a menu under a form field at (x, y) in its frame.
    fn pop_up_under_field(&self, menu: &NSMenu, web: &WKWebView, frame: &WKFrameInfo, x: f64, y: f64) {
        // The field's bottom-left corner, from page (CSS) pixels to view
        // points. A frame's field is relative to the frame, whose place in
        // the page is unknown (it may be another site): the pointer then.
        let point = if unsafe { frame.isMainFrame() } {
            let zoom = unsafe { web.pageZoom() };
            let (px, py) = (x * zoom, y * zoom + 2.0);
            if web.isFlipped() { NSPoint::new(px, py) } else { NSPoint::new(px, web.bounds().size.height - py) }
        } else {
            web.convertPoint_fromView(self.window().mouseLocationOutsideOfEventStream(), None)
        };
        menu.popUpMenuPositioningItem_atLocation_inView(None, point, Some(web));
    }

    /// An address or card field got focus: show the saved addresses (or
    /// cards) in a menu under it. Nothing is filled until one is picked.
    pub fn offer_autofill(&self, kind: &str, frame: &WKFrameInfo, x: f64, y: f64) {
        if kind.is_empty() {
            return;
        }
        let entries = self.app().store().autofill(Some(kind));
        if entries.is_empty() {
            return;
        }
        self.ivars().autofill.replace(Some((frame.retain(), entries.iter().map(|e| e.id).collect())));
        let labels: Vec<String> = entries.iter().map(crate::autofill::label).collect();
        if self.app().is_bench() {
            return say!("AUTOFILL {}", labels.join("|"));
        }
        let Some(web) = self.web() else { return };
        let mtm = self.mtm();
        let menu = NSMenu::new(mtm);
        let add = |title: &str, action: objc2::runtime::Sel, tag: usize| {
            let item = unsafe {
                NSMenuItem::initWithTitle_action_keyEquivalent(
                    NSMenuItem::alloc(mtm),
                    &NSString::from_str(title),
                    Some(action),
                    &NSString::from_str(""),
                )
            };
            unsafe { item.setTarget(Some(self)) };
            item.setTag(tag as isize);
            menu.addItem(&item);
        };
        for (i, l) in labels.iter().enumerate() {
            add(l, sel!(fillAutofill:), i);
        }
        menu.addItem(&NSMenuItem::separatorItem(mtm));
        add(if kind == "card" { "Manage Cards…" } else { "Manage Addresses…" }, sel!(openAutofillSettings:), 0);
        self.pop_up_under_field(&menu, &web, frame, x, y);
    }

    /// Fill entry `i` of the last autofill menu into its form (a card after
    /// Touch ID, like a password).
    pub fn fill_autofill_at(&self, i: usize) {
        let Some((frame, ids)) = self.ivars().autofill.borrow().clone() else { return };
        let Some(entry) = ids.get(i).and_then(|id| self.app().store().autofill(None).into_iter().find(|e| e.id == *id))
        else {
            return;
        };
        let card = entry.kind == "card";
        let this = self.retain();
        let fill = move || {
            let values = crate::autofill::values(this.app(), &entry);
            let keys: Vec<Retained<NSString>> = values.iter().map(|(k, _)| NSString::from_str(k)).collect();
            let vals: Vec<Retained<AnyObject>> = values.iter().map(|(_, v)| NSString::from_str(v).into()).collect();
            let keys: Vec<&NSString> = keys.iter().map(|k| &**k).collect();
            let v = objc2_foundation::NSDictionary::from_retained_objects(&keys, &vals);
            let args = objc2_foundation::NSDictionary::from_retained_objects(
                &[&*NSString::from_str("v")],
                &[Retained::<AnyObject>::from(v)],
            );
            if let Some(web) = this.web() {
                unsafe {
                    web.callAsyncJavaScript_arguments_inFrame_inContentWorld_completionHandler(
                        &NSString::from_str("return __mybAutofill(v)"),
                        Some(&args),
                        Some(&frame),
                        &passwords_world(this.mtm()),
                        None,
                    )
                };
            }
        };
        if card {
            crate::passwords::confirm_owner(
                self.app(),
                "fill your card details",
                Box::new(move |ok| {
                    if ok {
                        fill()
                    }
                }),
            );
        } else {
            fill();
        }
    }

    /// Fill account `i` of the last menu into the login form, if the page is
    /// still on the site it was saved for.
    pub fn fill_password_at(&self, i: usize) {
        let Some((site, frame, accounts)) = self.ivars().fill.borrow().clone() else { return };
        let (Some(user), Some(web)) = (accounts.get(i), self.web()) else { return };
        let now = unsafe { web.URL() }.and_then(|u| u.absoluteString()).map(|u| u.to_string()).unwrap_or_default();
        // (A frame's site is checked by the fill script, inside the frame.)
        if unsafe { frame.isMainFrame() } && crate::passwords::site_for(&now).as_deref() != Some(site.as_str()) {
            return;
        }
        let user = user.clone();
        let this = self.retain();
        crate::passwords::confirm_owner(
            self.app(),
            &format!("fill your password for {site}"),
            Box::new(move |ok| {
                if ok {
                    this.fill_password(&site, &frame, &user);
                }
            }),
        );
    }

    /// Fill `user`'s password into `frame`, if the tab is still on `site`.
    fn fill_password(&self, site: &str, frame: &WKFrameInfo, user: &str) {
        let Some(web) = self.web() else { return };
        let now = unsafe { web.URL() }.and_then(|u| u.absoluteString()).map(|u| u.to_string()).unwrap_or_default();
        if unsafe { frame.isMainFrame() } && crate::passwords::site_for(&now).as_deref() != Some(site) {
            return;
        }
        let Some(password) = self.app().vault().password(site, user) else { return };
        // The script fills only if the frame is still on `site` (it may have
        // navigated since the menu was shown).
        let keys = [NSString::from_str("u"), NSString::from_str("p"), NSString::from_str("h")];
        let values: [Retained<AnyObject>; 3] =
            [NSString::from_str(user).into(), NSString::from_str(&password).into(), NSString::from_str(site).into()];
        let keys: Vec<&NSString> = keys.iter().map(|k| &**k).collect();
        let args = objc2_foundation::NSDictionary::from_retained_objects(&keys, &values);
        unsafe {
            web.callAsyncJavaScript_arguments_inFrame_inContentWorld_completionHandler(
                &NSString::from_str("__mybFill(u, p, h)"),
                Some(&args),
                Some(frame),
                &passwords_world(self.mtm()),
                None,
            )
        };
    }

    /// The page signed in: offer to save the password (not in private
    /// windows, not where the user said never, not if it's already saved).
    pub fn offer_to_save(&self, site: &str, user: &str, password: &str) {
        let app = self.app();
        if self.is_private()
            || self.window().attachedSheet().is_some()
            || app.store().permission(site, "passwords").as_deref() == Some("deny")
            || app.vault().password(site, user).as_deref() == Some(password)
        {
            return;
        }
        let who = if user.is_empty() { "this site".to_string() } else { user.to_string() };
        let message = format!(
            "Save the password for {who} on {site}?\n\nIt's kept in your Mac's keychain and filled in only when you \
             pick it on this site."
        );
        let (this, site, user, password) = (self.retain(), site.to_string(), user.to_string(), password.to_string());
        self.sheet(
            &NSString::from_str(&message),
            &["Save Password", "Not Now"],
            None,
            Some(("Never for this site", false)),
            move |save, _, never| {
                let app = this.app();
                if save {
                    if let Err(e) = app.vault().save(&site, &user, &password) {
                        this.confirm(&e, &["OK"], |_| {});
                    }
                } else if never {
                    let _ = app.store().set_permission(&site, "passwords", "deny");
                }
            },
        );
    }

    /// Run one of resources/passwords.js's functions in this page.
    pub fn run_password_script(&self, js: &str) {
        if let Some(web) = self.web() {
            unsafe {
                web.evaluateJavaScript_inFrame_inContentWorld_completionHandler(
                    &NSString::from_str(js),
                    None,
                    &passwords_world(self.mtm()),
                    None,
                )
            };
        }
    }

    /// Reader mode (⇧⌘R): the page's article alone, in large type, no
    /// scripts. Again (or Reload) loads the page again. Costs nothing until used.
    pub fn toggle_reader(&self) {
        let Some(web) = self.web() else { return };
        if self.ivars().reader.get() {
            // WebKit replaced the page's history entry with the reader page
            // (same address), so it's loaded again rather than gone back to.
            let url = unsafe { web.URL() }.and_then(|u| u.absoluteString()).map(|u| u.to_string());
            if let Some(url) = url {
                self.load(&url);
            }
            return self.report_reader("off");
        }
        let this = self.retain();
        let done = RcBlock::new(move |r: *mut AnyObject, _e: *mut NSError| {
            let json = unsafe { r.as_ref() }.and_then(|r| r.downcast_ref::<NSString>()).map(|s| s.to_string());
            match json.as_deref().and_then(reader_page) {
                Some(html) => this.show_reader(&html),
                None => this.report_reader("none"),
            }
        });
        unsafe {
            web.evaluateJavaScript_inFrame_inContentWorld_completionHandler(
                &NSString::from_str(READER_JS),
                None,
                &WKContentWorld::defaultClientWorld(self.mtm()),
                Some(&done),
            )
        };
    }

    /// Show the reader page in place of the page, at the page's address (so
    /// its images and links resolve).
    fn show_reader(&self, html: &str) {
        let Some(web) = self.web() else { return };
        let base = unsafe { web.URL() };
        self.ivars().reader_loading.set(true);
        unsafe { web.loadHTMLString_baseURL(&NSString::from_str(html), base.as_deref()) };
        self.report_reader("on");
    }

    fn report_reader(&self, state: &str) {
        if self.app().is_bench() {
            say!("READER {state}");
        }
    }

    /// `SPELL continuous=<0|1> grammar=<0|1> correct=<0|1>`: the checkmarks
    /// the web view gives the Spelling and Grammar items.
    pub fn spell_state(&self) -> String {
        let Some(web) = self.web() else { return "SPELL none".into() };
        let get = |action| {
            let item = unsafe {
                NSMenuItem::initWithTitle_action_keyEquivalent(
                    NSMenuItem::alloc(self.mtm()),
                    &NSString::from_str(""),
                    Some(action),
                    &NSString::from_str(""),
                )
            };
            let _: bool = unsafe { msg_send![&*web, validateUserInterfaceItem: &*item] };
            (item.state() == 1) as u8
        };
        format!(
            "SPELL continuous={} grammar={} correct={}",
            get(sel!(toggleContinuousSpellChecking:)),
            get(sel!(toggleGrammarChecking:)),
            get(sel!(toggleAutomaticSpellingCorrection:))
        )
    }

    /// Web Inspector for this page (private `_inspector`), if developer
    /// tools are on. `INSPECTOR <0|1>` in bench mode, without opening it.
    pub fn show_inspector(&self) {
        let on = self.app().store().flag("devtools");
        let inspector: Option<Retained<AnyObject>> = self
            .web()
            .filter(|w| on && responds(w, sel!(_inspector)))
            .and_then(|w| unsafe { msg_send![&*w, _inspector] });
        if self.app().is_bench() {
            return say!("INSPECTOR {}", inspector.is_some() as u8);
        }
        if let Some(i) = inspector.filter(|i| responds(i, sel!(show))) {
            let _: () = unsafe { msg_send![&*i, show] };
        }
    }

    pub fn is_translated(&self) -> bool {
        self.ivars().translated.get()
    }

    /// Translate Page, or Show Original if it's translated. The text goes to
    /// the translator a part at a time, each part shown as it comes back.
    pub fn toggle_translate(&self) {
        let iv = self.ivars();
        if self.web().is_none() {
            return self.translate_failed("none", "There's no page to translate.");
        }
        iv.translate_run.set(iv.translate_run.get() + 1);
        if iv.translated.replace(false) {
            self.translate_js("__mybTr.restore()", None, |_| {});
            return self.bench_line("TRANSLATE off");
        }
        let (this, run) = (self.retain(), iv.translate_run.get());
        self.translate_js("return JSON.stringify(__mybTr.collect())", None, move |r| {
            let texts = r
                .and_then(|r| r.downcast::<NSString>().ok())
                .map_or_else(Vec::new, |j| crate::translate::parse_texts(&j.to_string()));
            if texts.is_empty() {
                return this.translate_failed("empty", "There's no text on this page to translate.");
            }
            this.translate_part(std::rc::Rc::new(texts), 0, run);
        });
    }

    fn translate_part(&self, texts: std::rc::Rc<Vec<String>>, at: usize, run: u64) {
        let end = (at + crate::translate::CHUNK).min(texts.len());
        let to = self.app().store().setting("translate_to");
        let this = self.retain();
        let part = texts[at..end].to_vec();
        self.app().translate(
            part,
            &to,
            Box::new(move |outcome| {
                // The tab went to another page, or back to the original.
                if this.ivars().translate_run.get() != run {
                    return;
                }
                match outcome {
                    Ok((from, done)) => {
                        let done: Vec<Retained<NSString>> = done.iter().map(|t| NSString::from_str(t)).collect();
                        let keys = [NSString::from_str("at"), NSString::from_str("texts")];
                        let values: [Retained<AnyObject>; 2] = [
                            objc2_foundation::NSNumber::new_usize(at).into(),
                            NSArray::from_retained_slice(&done).into(),
                        ];
                        let keys: Vec<&NSString> = keys.iter().map(|k| &**k).collect();
                        let args = objc2_foundation::NSDictionary::from_retained_objects(&keys, &values);
                        this.translate_js("return __mybTr.apply(at, texts)", Some(&args), |_| {});
                        this.ivars().translated.set(true);
                        if end < texts.len() {
                            this.translate_part(texts, end, run);
                        } else {
                            this.bench_line(&format!("TRANSLATED {from} {}", texts.len()));
                        }
                    }
                    Err((code, message)) => this.translate_failed(&code, &message),
                }
            }),
        );
    }

    /// resources/translate.js's functions, then `body`.
    fn translate_js(
        &self,
        body: &str,
        args: Option<&objc2_foundation::NSDictionary<NSString, AnyObject>>,
        done: impl FnOnce(Option<Retained<AnyObject>>) + 'static,
    ) {
        self.client_js(&format!("{}\n{body}", crate::translate::TRANSLATE_JS), args, done);
    }

    /// Run `script` (an async function body, with `args` as its variables) in
    /// the browser's own script world of the main frame, where the page
    /// can't see it; `done` gets what it returns.
    fn client_js(
        &self,
        script: &str,
        args: Option<&objc2_foundation::NSDictionary<NSString, AnyObject>>,
        done: impl FnOnce(Option<Retained<AnyObject>>) + 'static,
    ) {
        let Some(web) = self.web() else { return };
        let done = std::cell::Cell::new(Some(done));
        let block = RcBlock::new(move |r: *mut AnyObject, _e: *mut NSError| {
            if let Some(done) = done.take() {
                done(unsafe { Retained::retain(r) });
            }
        });
        unsafe {
            web.callAsyncJavaScript_arguments_inFrame_inContentWorld_completionHandler(
                &NSString::from_str(script),
                args,
                None,
                &WKContentWorld::defaultClientWorld(self.mtm()),
                Some(&block),
            )
        };
    }

    /// Hide Element…: the user clicks an element on the page, then confirms
    /// (and may edit) its selector; from then on it's hidden on this site by
    /// a filter (Settings → Privacy → My filters), a content-blocker rule.
    pub fn pick_element(&self) {
        let url = self.url();
        let Some(site) = crate::store::site_of(&url).filter(|_| url.starts_with("http")) else { return };
        let this = self.retain();
        self.client_js(PICKER_JS, None, move |r| {
            let Some(picked) = r.and_then(|r| r.downcast::<NSString>().ok()).map(|s| s.to_string()) else { return };
            let message = format!(
                "Hide this element on {site}?\n\nIt stays hidden on this site from now on. Change or remove it in \
                 Settings → Privacy → My filters."
            );
            let (tab, site) = (this.clone(), site.clone());
            this.sheet(&NSString::from_str(&message), &["Hide", "Cancel"], Some(picked), None, move |ok, text, _| {
                let selector = text.unwrap_or_default().trim().to_string();
                if !ok || selector.is_empty() || selector.contains(['\n', '\r']) {
                    return;
                }
                let t = tab.clone();
                let hide = selector.clone();
                tab.app().add_filter(&format!("{site}##{selector}"), move |r| match r {
                    Ok(_) => {
                        // Now, on this page too (the rule covers the next loads).
                        let args = objc2_foundation::NSDictionary::from_retained_objects(
                            &[&*NSString::from_str("s")],
                            &[Retained::<AnyObject>::from(NSString::from_str(&hide))],
                        );
                        t.client_js(
                            "document.querySelectorAll(s).forEach(e => e.style.setProperty('display', 'none', 'important'))",
                            Some(&args),
                            |_| {},
                        );
                        t.bench_line("HIDDEN 1");
                    }
                    Err(e) => {
                        t.bench_line("HIDDEN 0");
                        t.confirm(&e, &["OK"], |_| {});
                    }
                });
            });
        });
    }

    /// Tell the user why the page wasn't translated (the bench gets the code).
    fn translate_failed(&self, code: &str, message: &str) {
        if self.app().is_bench() {
            return say!("TRANSLATE err {code}");
        }
        if code != "install" {
            return self.confirm(message, &["OK"], |_| {});
        }
        self.confirm(message, &["Open Language & Region", "Not Now"], |open| {
            let url = "x-apple.systempreferences:com.apple.Localization-Settings.extension";
            if let (true, Some(url)) = (open, NSURL::URLWithString(&NSString::from_str(url))) {
                objc2_app_kit::NSWorkspace::sharedWorkspace().openURL(&url);
            }
        });
    }

    fn bench_line(&self, line: &str) {
        if self.app().is_bench() {
            say!("{line}");
        }
    }

    /// Picture in picture for the page's video: WebKit's own toggle (private),
    /// else the standard API on the largest playing video.
    pub fn picture_in_picture(&self) {
        let Some(web) = self.web() else { return };
        let responds = |s| -> bool { unsafe { msg_send![&*web, respondsToSelector: s] } };
        if responds(sel!(_canTogglePictureInPicture)) && responds(sel!(_togglePictureInPicture)) {
            let can: bool = unsafe { msg_send![&*web, _canTogglePictureInPicture] };
            if can {
                let _: () = unsafe { msg_send![&*web, _togglePictureInPicture] };
                return;
            }
        }
        let js = "(() => { if (document.pictureInPictureElement) return document.exitPictureInPicture(); \
                  const v = [...document.querySelectorAll('video')].filter(v => v.readyState > 0) \
                  .sort((a, b) => (b.clientWidth * b.clientHeight) - (a.clientWidth * a.clientHeight))[0]; \
                  if (v) v.requestPictureInPicture(); })()";
        unsafe {
            web.evaluateJavaScript_inFrame_inContentWorld_completionHandler(
                &NSString::from_str(js),
                None,
                &WKContentWorld::defaultClientWorld(self.mtm()),
                None,
            )
        };
    }

    /// WebKit finished loading a PDF: show it in the PDF viewer, over the page.
    fn show_pdf(&self) {
        let Some(web) = self.web().filter(|w| responds(w, sel!(_getMainResourceDataWithCompletionHandler:))) else {
            return;
        };
        let this = self.retain();
        let done = RcBlock::new(move |data: *mut objc2_foundation::NSData, _e: *mut NSError| {
            let Some(data) = (unsafe { data.as_ref() }) else { return };
            let url = this.url();
            let name = NSURL::URLWithString(&NSString::from_str(&url))
                .and_then(|u| u.lastPathComponent())
                .map(|n| n.to_string())
                .filter(|n| n.to_ascii_lowercase().ends_with(".pdf"))
                .unwrap_or_else(|| "document.pdf".into());
            let Some(viewer) = crate::pdf::Viewer::new(&this, data, this.web_frame(), name, this.mtm()) else { return };
            this.close_pdf();
            this.window().contentView().unwrap().addSubview(&viewer.root);
            this.ivars().pdf.replace(Some(viewer));
            this.bench_line("PDFVIEW");
        });
        let _: () = unsafe { msg_send![&*web, _getMainResourceDataWithCompletionHandler: &*done] };
    }

    fn close_pdf(&self) {
        let old = self.ivars().pdf.take();
        if let Some(mut viewer) = old {
            viewer.close(self);
        }
    }

    /// `PDF pages=… page=… …`, or `PDF none`.
    pub fn pdf_state(&self) -> String {
        self.ivars().pdf.borrow().as_ref().map_or("PDF none".into(), |p| p.state())
    }

    /// A PDF toolbar button (bench: `pdfcmd <i>`).
    pub fn pdf_command(&self, i: usize) {
        use crate::pdf::Command;
        let Some((_, _, command)) = crate::pdf::BUTTONS.get(i) else { return };
        match command {
            Command::Find => self.show_find(),
            Command::Print => self.print(None),
            Command::Save => self.save_page(None),
            Command::ReadAloud => {
                if let Some(pdf) = self.ivars().pdf.borrow_mut().as_mut() {
                    pdf.read_aloud(self);
                }
            }
            c => {
                if let Some(pdf) = self.ivars().pdf.borrow_mut().as_mut() {
                    pdf.run(*c);
                }
            }
        }
    }

    /// Select the start of page `n` in the PDF (tests, before Highlight).
    pub fn pdf_select(&self, n: usize) {
        if let Some(pdf) = self.ivars().pdf.borrow().as_ref() {
            pdf.select_page(n);
        }
    }

    /// The user left this tab while its video plays with sound: keep showing
    /// it in picture in picture (Settings → General). Silent videos (ads,
    /// backgrounds) are left alone.
    pub fn leave_playing(&self) {
        if !self.playing_audio() || !self.app().store().flag("auto_pip") {
            return;
        }
        let this = self.retain();
        self.client_js(
            "if (document.pictureInPictureElement) return 0; \
             const v = [...document.querySelectorAll('video')].filter(v => !v.paused && !v.ended && v.readyState > 1 \
               && !v.disablePictureInPicture).sort((a, b) => b.clientWidth * b.clientHeight - a.clientWidth * a.clientHeight)[0]; \
             if (!v) return 0; \
             try { await v.requestPictureInPicture(); return 1 } catch { return 0 }",
            None,
            move |r| {
                let on = r.and_then(|r| r.downcast::<objc2_foundation::NSNumber>().ok()).is_some_and(|n| n.boolValue());
                this.ivars().auto_pip.set(on);
                if on {
                    this.bench_line("AUTOPIP 1");
                }
            },
        );
    }

    /// Back to a tab whose video went to picture in picture when it was left:
    /// the video comes back into the page.
    pub fn return_to_playing(&self) {
        if self.ivars().auto_pip.replace(false) {
            self.client_js("if (document.pictureInPictureElement) await document.exitPictureInPicture()", None, |_| {});
        }
    }

    /// Put our link items in place of WebKit's "Open Link in New Window"
    /// (which would open a tab anyway). Other items stay as WebKit made them.
    pub fn link_menu(&self, menu: &NSMenu, link: Option<&str>) {
        let Some(link) = link.filter(|l| bookmarkable(l)) else { return };
        let is_new_window = |i: isize| {
            let id =
                menu.itemAtIndex(i).and_then(|m| objc2_app_kit::NSUserInterfaceItemIdentification::identifier(&*m));
            id.is_some_and(|id| id.to_string() == "WKMenuItemIdentifierOpenLinkInNewWindow")
        };
        let Some(at) = (0..menu.numberOfItems()).find(|&i| is_new_window(i)) else { return };
        menu.removeItemAtIndex(at);
        let mut items = vec![
            ("Open Link in New Tab", sel!(openLinkInNewTab:)),
            ("Open Link in New Window", sel!(openLinkInNewWindow:)),
        ];
        if !self.is_private() {
            items.push(("Open Link in Private Window", sel!(openLinkInPrivateWindow:)));
        }
        for (k, (title, action)) in items.into_iter().enumerate() {
            let item = unsafe {
                NSMenuItem::initWithTitle_action_keyEquivalent(
                    NSMenuItem::alloc(self.mtm()),
                    &NSString::from_str(title),
                    Some(action),
                    &NSString::from_str(""),
                )
            };
            unsafe {
                item.setTarget(Some(self));
                item.setRepresentedObject(Some(&NSString::from_str(link)));
            }
            menu.insertItem_atIndex(&item, at + k as isize);
        }
    }

    /// Print the page (⌘P, or its own `window.print()`): the system print
    /// panel, whose PDF menu also saves it as a PDF. With `to`, saves a PDF
    /// there without the panel (tests use it). One at a time: a page can't
    /// stack print panels.
    pub fn print(&self, to: Option<&str>) {
        let Some(web) = self.web() else { return };
        if self.window().attachedSheet().is_some() {
            return;
        }
        if let (Some(pdf), None) = (self.ivars().pdf.borrow().as_ref(), to) {
            return pdf.print(self.window());
        }
        let info: Retained<objc2_app_kit::NSPrintInfo> =
            unsafe { msg_send![&*objc2_app_kit::NSPrintInfo::sharedPrintInfo(), copy] };
        if let Some(path) = to {
            info.setJobDisposition(unsafe { objc2_app_kit::NSPrintSaveJob });
            let url = NSURL::fileURLWithPath(&NSString::from_str(path));
            let _: () =
                unsafe { msg_send![&*info.dictionary(), setObject: &*url, forKey: objc2_app_kit::NSPrintJobSavingURL] };
        }
        let op = unsafe { web.printOperationWithPrintInfo(&info) };
        op.setShowsPrintPanel(to.is_none());
        op.setShowsProgressPanel(to.is_none());
        // WebKit prints blank pages unless the operation's view has a size.
        if let Some(view) = op.view() {
            view.setFrame(web.bounds());
        }
        unsafe {
            op.runOperationModalForWindow_delegate_didRunSelector_contextInfo(
                self.window(),
                Some(self),
                Some(sel!(printOperationDidRun:success:contextInfo:)),
                std::ptr::null_mut(),
            )
        };
    }

    /// Save the page as a Web Archive (⌘S): one file with the page and its
    /// images and styles, readable offline (here, in Safari, in Quick Look).
    /// With `to`, saves there without the panel (tests use it).
    pub fn save_page(&self, to: Option<&str>) {
        let Some(web) = self.web() else { return };
        if self.window().attachedSheet().is_some() {
            return;
        }
        let write = move |data: &objc2_foundation::NSData, path: &str| -> bool {
            data.writeToFile_atomically(&NSString::from_str(path), true)
        };
        // A PDF saves as itself, with its highlights.
        let pdf = self.ivars().pdf.borrow().as_ref().and_then(|p| Some((p.data()?, p.name.clone())));
        if let Some((data, name)) = pdf {
            if let Some(path) = to {
                return say!("SAVED {}", write(&data, path) as u8);
            }
            let panel = objc2_app_kit::NSSavePanel::savePanel(self.mtm());
            panel.setNameFieldStringValue(&NSString::from_str(&name));
            let (p, this) = (panel.clone(), self.retain());
            let handler = RcBlock::new(move |response: isize| {
                let path = (response == NSModalResponseOK).then(|| p.URL()).flatten().and_then(|u| u.path());
                if let Some(path) = path
                    && !write(&data, &path.to_string())
                {
                    this.confirm("Couldn't save the PDF.", &["OK"], |_| {});
                }
            });
            return panel.beginSheetModalForWindow_completionHandler(self.window(), &handler);
        }
        if let Some(path) = to.map(str::to_string) {
            let done = RcBlock::new(move |data: *mut objc2_foundation::NSData, _e: *mut NSError| {
                let ok = unsafe { data.as_ref() }.is_some_and(|d| write(d, &path));
                say!("SAVED {}", ok as u8);
            });
            return unsafe { web.createWebArchiveDataWithCompletionHandler(&done) };
        }
        let panel = objc2_app_kit::NSSavePanel::savePanel(self.mtm());
        let name = crate::sanitize_filename(&self.title());
        let name = if name == "download" { "Page".to_string() } else { name };
        panel.setNameFieldStringValue(&NSString::from_str(&format!("{name}.webarchive")));
        let (p, this) = (panel.clone(), self.retain());
        let handler = RcBlock::new(move |response: isize| {
            let Some(path) = (response == NSModalResponseOK).then(|| p.URL()).flatten().and_then(|u| u.path()) else {
                return;
            };
            let (path, tab) = (path.to_string(), this.clone());
            let done = RcBlock::new(move |data: *mut objc2_foundation::NSData, _e: *mut NSError| {
                if !unsafe { data.as_ref() }.is_some_and(|d| write(d, &path)) {
                    tab.confirm("Couldn't save the page.", &["OK"], |_| {});
                }
            });
            if let Some(web) = this.web() {
                unsafe { web.createWebArchiveDataWithCompletionHandler(&done) };
            }
        });
        panel.beginSheetModalForWindow_completionHandler(self.window(), &handler);
    }

    /// Bookmark this page, or remove its bookmark (⌘D, the star).
    pub fn toggle_bookmark(&self) {
        let url = self.web().and_then(|w| unsafe { w.URL() }).and_then(|u| u.absoluteString()).map(|u| u.to_string());
        let url = url.unwrap_or_else(|| self.url());
        if !bookmarkable(&url) {
            return;
        }
        let store = self.app().store();
        match store.bookmark_of(&url) {
            Some(id) => store.remove_bookmark(id),
            None => {
                let title =
                    self.web().and_then(|w| unsafe { w.title() }).map(|t| t.to_string()).unwrap_or(self.title());
                store.add_bookmark(&url, &title, "");
            }
        }
        self.app().refresh_toolbars();
    }

    /// `STAR on=<0|1>`, read from the star's image.
    pub fn star_state(&self) -> String {
        let on = self
            .ivars()
            .star
            .get()
            .and_then(|s| s.image())
            .and_then(|i| i.accessibilityDescription())
            .is_some_and(|d| d.to_string() == "Bookmarked");
        format!("STAR on={}", on as u8)
    }

    /// The Web Store's "Add to my-browser" button is showing.
    pub fn store_button_shown(&self) -> bool {
        self.ivars().store_button.borrow().is_some()
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
            // Reloading the reader page means the page itself.
            Some(_) if self.ivars().reader.get() => self.toggle_reader(),
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

    /// Page zoom: `None` resets, otherwise multiply. Remembered for the
    /// site (not from private windows) and used on its pages from then on.
    pub fn zoom(&self, factor: Option<f64>) {
        if let Some(web) = self.web() {
            let z = factor.map_or(1.0, |f| (unsafe { web.pageZoom() } * f).clamp(0.3, 3.0));
            unsafe { web.setPageZoom(z) };
            if let (Some(site), false) = (crate::store::site_of(&self.url()), self.is_private()) {
                self.app().store().set_zoom(&site, z);
            }
        }
    }

    /// `ZOOM <level>`, read from the web view.
    pub fn zoom_state(&self) -> String {
        format!("ZOOM {:.2}", self.web().map_or(1.0, |w| unsafe { w.pageZoom() }))
    }

    pub fn focus_address_bar(&self) {
        let bar = self.ivars().bar.get().unwrap();
        self.window().makeFirstResponder(Some(bar));
        unsafe { bar.selectText(None) };
    }

    /// Bench: edit the address bar through its field editor, then press
    /// Return (`Some(text)`) or move focus to the page (`None`).
    pub fn address_bar_key(&self, text: Option<&str>) {
        self.focus_address_bar();
        let Some(editor) = self.window().firstResponder().and_then(|r| r.downcast::<NSTextView>().ok()) else { return };
        match text {
            Some(text) => {
                editor.setString(&NSString::from_str(text));
                let _: () = unsafe { msg_send![&*editor, doCommandBySelector: sel!(insertNewline:)] };
            }
            None => {
                if let Some(web) = self.web() {
                    self.window().makeFirstResponder(Some(&web));
                }
            }
        }
    }

    /// Links: Cmd-click opens a new tab; web pages can't open internal pages;
    /// `download` attributes download.
    fn action_policy(&self, action: &WKNavigationAction) -> WKNavigationActionPolicy {
        let request = unsafe { action.request() };
        let url = request.URL().and_then(|u| u.absoluteString()).map(|s| s.to_string()).unwrap_or_default();
        let kind = unsafe { action.navigationType() };
        let link = kind == WKNavigationType::LinkActivated;
        let main = is_main_frame(action);
        // The page now showing, not the source frame: WebKit reports an
        // empty origin and about:blank for frames of custom schemes, which
        // blocked the settings sidebar's own #links.
        let from_internal = self
            .web()
            .and_then(|w| unsafe { w.URL() })
            .and_then(|u| u.scheme())
            .is_some_and(|s| s.to_string() == INTERNAL);
        // A folder page's links: files open here (or in their app), and
        // "Show in Finder".
        if from_internal && main && link {
            for (prefix, show) in [("open", false), ("reveal", true)] {
                let Some(path) = url.strip_prefix(&format!("{INTERNAL}://{prefix}")) else { continue };
                let path = NSURL::URLWithString(&NSString::from_str(&format!("file://{path}"))).and_then(|u| u.path());
                match path.map(|p| p.to_string()) {
                    Some(p) if show => reveal(&p),
                    Some(p) => self.open_local(std::path::Path::new(&p)),
                    None => {}
                }
                return WKNavigationActionPolicy::Cancel;
            }
        }
        if url.starts_with(&format!("{INTERNAL}:")) {
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
            self.app().open_tab_in(self.is_private(), &url, true);
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
        // Lock for HTTPS, a computer for this Mac and the local network, a
        // warning for other plain HTTP, neutral for everything else.
        let local = crate::store::site_of(&url).is_some_and(|h| crate::privacy::is_local_host(&h));
        let (symbol, label) = if url.starts_with("https://") {
            ("lock.fill", "Secure connection. Site settings")
        } else if url.starts_with("http://") && local {
            (
                "desktopcomputer",
                "Local address: this Mac or your network, loaded as typed (not upgraded to HTTPS). Site settings",
            )
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
        // The Web Store button comes and goes with the page.
        let store = !self.is_private() && crate::extensions::webstore_id(&self.url()).is_some();
        if store != iv.store_button.borrow().is_some() {
            self.app().refresh_extension_buttons_for(self);
        }
        if let Some(star) = iv.star.get() {
            let on = bookmarkable(&url) && self.app().store().bookmark_of(&url).is_some();
            if iv.shown_star.replace(Some(on)) != Some(on) {
                let (symbol, label) = if on { ("star.fill", "Bookmarked") } else { ("star", "Bookmark this page") };
                star.setImage(Some(&symbol_image(symbol, label)));
                star.setToolTip(Some(&NSString::from_str(if on {
                    "Remove bookmark (⌘D)"
                } else {
                    "Bookmark (⌘D)"
                })));
            }
            star.setEnabled(bookmarkable(&url));
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
        let a = alert.clone();
        self.present(&alert, move |response| {
            let checked = a.suppressionButton().is_some_and(|b| b.state() == 1);
            // First button is NSAlertFirstButtonReturn (1000).
            done(response == 1000, field.as_ref().map(|f| f.stringValue().to_string()), checked);
        });
    }

    /// Show `alert` as this tab's sheet; `done(response)`. The bench
    /// `dialog` command answers it.
    pub fn present(&self, alert: &NSAlert, done: impl Fn(isize) + 'static) {
        let this = self.retain();
        let handler = RcBlock::new(move |response: isize| {
            this.ivars().alert.replace(None);
            done(response);
        });
        self.ivars().alert.replace(Some(alert.retain()));
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
        let Some(alert) = self.ivars().alert.borrow().clone() else {
            // Another sheet (the print panel): OK or Cancel.
            let Some(sheet) = self.window().attachedSheet() else { return false };
            self.window().endSheet_returnCode(&sheet, if ok { 1 } else { 0 });
            return true;
        };
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
        // A private window's answers last for that page only.
        let remember = (!self.is_private()).then_some(("Remember for this site", true));
        self.sheet(&message, &["Allow", "Don't Allow"], None, remember, move |ok, _, remember| {
            if remember {
                for k in &kinds {
                    let _ = this.app().store().set_permission(&site, k, if ok { "allow" } else { "deny" });
                }
            }
            done(ok)
        });
    }

    pub fn is_private(&self) -> bool {
        self.ivars().private.get()
    }

    pub fn placeholder(&self) -> String {
        self.ivars().bar.get().and_then(|b| b.placeholderString()).map(|p| p.to_string()).unwrap_or_default()
    }

    /// `PRIVATE <0|1>`, read from the address bar's placeholder.
    pub fn private_state(&self) -> String {
        let placeholder = self.ivars().bar.get().and_then(|b| b.placeholderString()).map(|p| p.to_string());
        format!("PRIVATE {}", placeholder.is_some_and(|p| p.starts_with("Private")) as u8)
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
        // Left of those: "Add to my-browser" on a Chrome Web Store page.
        if let Some(old) = iv.store_button.take() {
            old.removeFromSuperview();
        }
        let store = !self.is_private() && crate::extensions::webstore_id(&self.url()).is_some();
        if store {
            let b = unsafe {
                NSButton::buttonWithImage_target_action(
                    &symbol_image("puzzlepiece.extension.fill", "Add to my-browser"),
                    Some(self),
                    Some(sel!(addFromWebStore:)),
                    mtm,
                )
            };
            b.setBordered(false);
            b.setContentTintColor(Some(&objc2_app_kit::NSColor::controlAccentColor()));
            b.setToolTip(Some(&NSString::from_str("Add this extension to my-browser")));
            let x = width - 4.0 - (n + waiting as u8 as f64 + 1.0) * BUTTON_W;
            b.setFrame(NSRect::new(NSPoint::new(x, top), NSSize::new(BUTTON_W, 28.0)));
            b.setAutoresizingMask(
                NSAutoresizingMaskOptions::ViewMinXMargin | NSAutoresizingMaskOptions::ViewMinYMargin,
            );
            content.addSubview(&b);
            iv.store_button.replace(Some(b));
        }
        // The star sits between the address bar and those buttons.
        let right = n + waiting as u8 as f64 + store as u8 as f64 + 1.0;
        if let Some(star) = iv.star.get() {
            star.setFrame(NSRect::new(NSPoint::new(width - 4.0 - right * BUTTON_W, top), NSSize::new(BUTTON_W, 28.0)));
        }
        let mut frame = bar.frame();
        frame.size.width = width - frame.origin.x - 8.0 - right * BUTTON_W;
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

    /// This tab's cookies and site data: the profile's, or the private windows'.
    fn data_store(&self) -> Retained<objc2_web_kit::WKWebsiteDataStore> {
        match self.web() {
            Some(w) => unsafe { w.configuration().websiteDataStore() },
            None => self.app().data_store().retain(),
        }
    }

    /// "On-device site data" for the site on screen.
    pub fn show_site_data(&self) {
        if let Some(host) = crate::store::site_of(&self.url()) {
            crate::sitedata::show(self, &self.data_store(), &host);
        }
    }

    pub fn app_is_bench(&self) -> bool {
        self.app().is_bench()
    }

    pub fn open_settings(&self, page: &str) {
        self.app().open_settings_page(page);
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
        let local = crate::privacy::is_local_host(&site);
        add(
            match (url.starts_with("https://"), local) {
                (true, _) => "Connection is secure",
                (false, true) => "Local address (this Mac or your network)",
                (false, false) => "Connection is not secure",
            },
            None,
            0,
            false,
        );
        menu.addItem(&NSMenuItem::separatorItem(mtm));
        let shields = store.permission(&site, "shields").as_deref() != Some("off");
        add("Block trackers and fingerprinting", Some(sel!(siteToggle:)), 0, shields);
        let local = store.permission(&site, "local_network").as_deref() == Some("allow");
        add("Allow access to the local network", Some(sel!(siteToggle:)), 1, local);
        let javascript = store.permission(&site, "javascript").as_deref() != Some("deny");
        add("Allow JavaScript", Some(sel!(siteToggle:)), 2, javascript);
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
        add("Hide Element on Page…", Some(sel!(pickElement:)), 0, false);
        add("Site Data…", Some(sel!(showSiteData:)), 0, false);
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

/// Select a file or folder in a Finder window.
fn reveal(path: &str) {
    let url = NSURL::fileURLWithPath(&NSString::from_str(path));
    objc2_app_kit::NSWorkspace::sharedWorkspace()
        .activateFileViewerSelectingURLs(&NSArray::from_retained_slice(&[url]));
}

/// Text safe to put in HTML.
pub fn escape_html(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&#39;")
}

static READER_JS: &str = include_str!("../resources/reader.js");
static PICKER_JS: &str = include_str!("../resources/picker.js");
static FIND_JS: &str = include_str!("../resources/find.js");

/// resources/find.js's `{count, current}`.
fn find_counts(json: &str) -> Option<(i64, i64)> {
    use objc2_foundation::{NSData, NSDictionary, NSJSONReadingOptions, NSJSONSerialization, NSNumber};
    let value = NSJSONSerialization::JSONObjectWithData_options_error(
        &NSData::with_bytes(json.as_bytes()),
        NSJSONReadingOptions::empty(),
    )
    .ok()?;
    let d = value.downcast_ref::<NSDictionary>()?;
    let n = |k: &str| {
        d.objectForKey(&NSString::from_str(k))
            .and_then(|v| v.downcast::<NSNumber>().ok())
            .map(|n| n.integerValue() as i64)
    };
    Some((n("count")?, n("current")?))
}

/// The script world of resources/passwords.js: pages can't reach into it.
pub fn passwords_world(mtm: MainThreadMarker) -> Retained<WKContentWorld> {
    unsafe { WKContentWorld::worldWithName(&NSString::from_str("passwords"), mtm) }
}

/// The reader page for resources/reader.js's result (`{title, site, html}`),
/// or None if it found no article. The article HTML was cleaned by the
/// script, and the page's policy forbids every script anyway.
fn reader_page(json: &str) -> Option<String> {
    use objc2_foundation::{NSData, NSDictionary, NSJSONReadingOptions, NSJSONSerialization};
    let data = NSData::with_bytes(json.as_bytes());
    let value = NSJSONSerialization::JSONObjectWithData_options_error(&data, NSJSONReadingOptions::empty()).ok()?;
    let dict = value.downcast_ref::<NSDictionary>()?;
    let field = |k: &str| {
        dict.objectForKey(&NSString::from_str(k)).and_then(|v| v.downcast::<NSString>().ok()).map(|s| s.to_string())
    };
    let (title, site, html) = (field("title")?, field("site").unwrap_or_default(), field("html")?);
    Some(format!(
        "<!doctype html><html><head><meta charset=utf-8>\
         <meta http-equiv=Content-Security-Policy content=\"default-src 'none'; img-src * data:; media-src *; style-src 'unsafe-inline'\">\
         <title>{t}</title><style>\
         :root{{color-scheme:light dark}}\
         body{{font:21px/1.6 ui-serif,Georgia,serif;max-width:36em;margin:56px auto;padding:0 28px;color:#26231f;background:#fbfaf6}}\
         @media(prefers-color-scheme:dark){{body{{color:#ddd;background:#1c1c1e}}a{{color:#7ab7ff}}}}\
         h1{{font:700 36px/1.2 -apple-system,sans-serif;margin:.2em 0 .8em}}h2,h3{{font-family:-apple-system,sans-serif;line-height:1.3}}\
         .site{{font:14px -apple-system,sans-serif;opacity:.6;margin:0}}\
         img,video,picture{{max-width:100%;height:auto}}figure{{margin:1.5em 0}}figcaption{{font-size:15px;opacity:.7}}\
         pre{{overflow:auto;font-size:15px;line-height:1.4}}blockquote{{margin-left:0;padding-left:1em;border-left:3px solid #8884}}\
         table{{border-collapse:collapse;font-size:16px}}td,th{{border:1px solid #8884;padding:4px 8px}}\
         </style></head><body><p class=site>{s}</p><h1>{t}</h1>{html}</body></html>",
        t = escape_html(&title),
        s = escape_html(&site),
    ))
}

/// The link under the pointer, from WebKit's context-menu element info.
fn link_url(element: &AnyObject) -> Option<String> {
    let responds = |o: &AnyObject, s| -> bool { unsafe { msg_send![o, respondsToSelector: s] } };
    if !responds(element, sel!(hitTestResult)) {
        return None;
    }
    let hit: Option<Retained<AnyObject>> = unsafe { msg_send![element, hitTestResult] };
    let hit = hit.filter(|h| responds(h, sel!(absoluteLinkURL)))?;
    let url: Option<Retained<NSURL>> = unsafe { msg_send![&*hit, absoluteLinkURL] };
    url?.absoluteString().map(|s| s.to_string())
}

/// The address a link menu item carries.
fn represented_url(item: &NSMenuItem) -> Option<String> {
    item.representedObject()?.downcast::<NSString>().ok().map(|s| s.to_string())
}

/// Pages that can be bookmarked: web pages and local files.
pub fn bookmarkable(url: &str) -> bool {
    url.starts_with("https://") || url.starts_with("http://") || url.starts_with("file://")
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

fn responds(obj: &AnyObject, sel: objc2::runtime::Sel) -> bool {
    unsafe { msg_send![obj, respondsToSelector: sel] }
}

/// Web view properties observed: OBSERVED, and whether sound is playing
/// where this WebKit reports it (private).
fn observed(web: &WKWebView) -> Vec<&'static str> {
    let mut keys = OBSERVED.to_vec();
    if responds(web, sel!(_isPlayingAudio)) {
        keys.push("_isPlayingAudio");
    }
    keys
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
    fn reader_pages_escape_the_title_and_forbid_scripts() {
        let page = reader_page(r#"{"title":"<b>Hi</b>","site":"a.example","html":"<p>text</p>"}"#).unwrap();
        assert!(page.contains("<title>&lt;b&gt;Hi&lt;/b&gt;</title>") && page.contains("<p>text</p>"));
        assert!(page.contains("default-src 'none'") && !page.contains("script-src"), "no scripts at all");
        assert!(page.find("Content-Security-Policy").unwrap() < page.find("<p>text").unwrap(), "policy comes first");
        assert!(reader_page("").is_none() && reader_page(r#"{"title":"x"}"#).is_none());
    }

    #[test]
    fn error_pages_escape_what_they_show() {
        assert_eq!(
            escape_html(r#"<img src=x onerror="a('b')">&"#),
            "&lt;img src=x onerror=&quot;a(&#39;b&#39;)&quot;&gt;&amp;"
        );
    }
}
