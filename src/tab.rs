//! One browser tab: a native macOS window tab with its own toolbar and web view.
//!
//! A tab can be asleep: its web view (and the page process behind it) is
//! destroyed and only the URL and title are kept. Waking reloads the page.
//! Background tabs fall asleep after a while (see App::sleep_background_tabs).

use std::cell::{Cell, OnceCell, RefCell};
use std::ffi::c_void;
use std::time::Instant;

use block2::{DynBlock, RcBlock};
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, Bool, ProtocolObject};
use objc2::{define_class, msg_send, sel, DefinedClass, MainThreadMarker, MainThreadOnly, Message};
use objc2_app_kit::{
    NSAlert, NSAutoresizingMaskOptions, NSBackingStoreType, NSButton, NSEventModifierFlags, NSImage,
    NSModalResponseOK, NSOpenPanel, NSTextField, NSWindow, NSWindowCollectionBehavior, NSWindowDelegate,
    NSWindowStyleMask, NSWindowTabbingMode, NSFloatingWindowLevel,
};
use objc2_foundation::{
    NSArray, NSError, NSKeyValueObservingOptions, NSNotification, NSObject, NSObjectNSKeyValueObserverRegistration,
    NSObjectProtocol, NSPoint, NSRect, NSSize, NSString, NSURLRequest, NSURL,
};
use objc2_web_kit::{
    WKWebExtensionContext, WKWebExtensionTab, WKWebExtensionTabChangedProperties, WKWebExtensionWindow,
    WKDownload, WKFrameInfo, WKNavigation, WKNavigationAction, WKNavigationActionPolicy, WKNavigationDelegate,
    WKNavigationResponse, WKNavigationResponsePolicy, WKNavigationType, WKOpenPanelParameters, WKUIDelegate,
    WKWebView, WKWebViewConfiguration, WKWindowFeatures,
};

use crate::App;

/// Height of the toolbar row (buttons + address bar) above the page.
const TOOLBAR_H: f64 = 32.0;
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
    /// Set by our own loads; web pages may not navigate to internal pages.
    allow_internal: Cell<bool>,
}

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
            self.destroy_web();
            self.app().tab_closed(self);
        }
    }

    unsafe impl WKNavigationDelegate for Tab {
        #[unsafe(method(webView:decidePolicyForNavigationAction:decisionHandler:))]
        fn decide_action(
            &self,
            _web: &WKWebView,
            action: &WKNavigationAction,
            decide: &DynBlock<dyn Fn(WKNavigationActionPolicy)>,
        ) {
            decide.call((self.action_policy(action),));
        }

        #[unsafe(method(webView:decidePolicyForNavigationResponse:decisionHandler:))]
        fn decide_response(
            &self,
            _web: &WKWebView,
            response: &WKNavigationResponse,
            decide: &DynBlock<dyn Fn(WKNavigationResponsePolicy)>,
        ) {
            let download = unsafe { !response.canShowMIMEType() || is_attachment(response) };
            decide.call((if download { WKNavigationResponsePolicy::Download } else { WKNavigationResponsePolicy::Allow },));
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
        }

        #[unsafe(method(webView:didFinishNavigation:))]
        fn did_finish_navigation(&self, _web: &WKWebView, _nav: Option<&WKNavigation>) {
            self.remember_page();
            // The title notification can arrive after this; show the final title now.
            self.refresh_toolbar();
            let url = self.url();
            if url.starts_with("http://") || url.starts_with("https://") {
                let id = self.app().store().add_visit(&url, &self.ivars().title.borrow());
                self.ivars().visit.set(Some(id));
            }
            self.report_nav("LOADED", &url);
        }

        #[unsafe(method(webView:didFailNavigation:withError:))]
        fn did_fail_navigation(&self, _web: &WKWebView, _nav: Option<&WKNavigation>, err: &NSError) {
            self.report_nav("FAILED", &err.localizedDescription().to_string());
        }

        #[unsafe(method(webView:didFailProvisionalNavigation:withError:))]
        fn did_fail_provisional(&self, _web: &WKWebView, _nav: Option<&WKNavigation>, err: &NSError) {
            self.report_nav("FAILED", &err.localizedDescription().to_string());
        }

        #[unsafe(method(webViewWebContentProcessDidTerminate:))]
        fn content_process_terminated(&self, web: &WKWebView) {
            if self.app().is_bench() {
                println!("CRASHED");
            }
            // The page's process died (crash or memory kill): bring the page back.
            unsafe { web.reload() };
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
            self.sheet(message, &["OK"], None, move |_, _| done.call(()));
        }

        #[unsafe(method(webView:runJavaScriptConfirmPanelWithMessage:initiatedByFrame:completionHandler:))]
        fn js_confirm(&self, _web: &WKWebView, message: &NSString, _frame: &WKFrameInfo, done: &DynBlock<dyn Fn(Bool)>) {
            let done = done.copy();
            self.sheet(message, &["OK", "Cancel"], None, move |ok, _| done.call((Bool::new(ok),)));
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
            self.sheet(prompt, &["OK", "Cancel"], Some(default.map(|d| d.to_string()).unwrap_or_default()), move |ok, text| {
                let text = text.map(|t| NSString::from_str(&t));
                done.call((if ok { text.map_or(std::ptr::null_mut(), |t| Retained::as_ptr(&t) as *mut _) } else { std::ptr::null_mut() },));
            });
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
            if let Some(u) = url.absoluteString() {
                self.load(&u.to_string());
            }
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

        // Toolbar row: back, forward, reload/stop, address bar.
        let top = h - TOOLBAR_H + 2.0;
        let pin_top = NSAutoresizingMaskOptions::ViewMinYMargin;
        let buttons = [
            ("chevron.left", "Back", sel!(goBack:), &self.ivars().back),
            ("chevron.right", "Forward", sel!(goForward:), &self.ivars().forward),
            ("arrow.clockwise", "Reload", sel!(reload:), &self.ivars().reload),
        ];
        for (i, (symbol, label, action, slot)) in buttons.into_iter().enumerate() {
            let button =
                unsafe { NSButton::buttonWithImage_target_action(&symbol_image(symbol, label), Some(self), Some(action), mtm) };
            button.setBordered(false);
            button.setFrame(NSRect::new(NSPoint::new(4.0 + i as f64 * BUTTON_W, top), NSSize::new(BUTTON_W, 28.0)));
            button.setAutoresizingMask(pin_top);
            content.addSubview(&button);
            let _ = slot.set(button);
        }
        let bar_x = 8.0 + 3.0 * BUTTON_W;
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
    }

    /// Create the web view (waking the tab). Uses `config` for popups.
    fn attach_web(&self, config: &WKWebViewConfiguration) -> Retained<WKWebView> {
        let content = self.window().contentView().unwrap();
        let frame = content.frame();
        let web = unsafe {
            WKWebView::initWithFrame_configuration(
                WKWebView::alloc(self.mtm()),
                NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(frame.size.width, frame.size.height - TOOLBAR_H)),
                config,
            )
        };
        web.setAutoresizingMask(NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewHeightSizable);
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
        self.ivars().allow_internal.set(url.starts_with(&format!("{INTERNAL}:")));
        let web = self.ensure_web();
        self.ivars().url.replace(url.to_string());
        // Focus moves to the page, so the address bar is free to show the real URL.
        self.window().makeFirstResponder(Some(&web));
        unsafe { web.loadRequest(&NSURLRequest::requestWithURL(&nsurl)) };
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
        let link = unsafe { action.navigationType() } == WKNavigationType::LinkActivated;
        if url.starts_with(&format!("{INTERNAL}:")) {
            let from_internal = unsafe { action.sourceFrame() }
                .is_some_and(|f| unsafe { f.securityOrigin().protocol() }.to_string() == INTERNAL);
            if !(self.ivars().allow_internal.replace(false) || from_internal) {
                return WKNavigationActionPolicy::Cancel;
            }
        }
        if unsafe { action.shouldPerformDownload() } {
            return WKNavigationActionPolicy::Download;
        }
        if link && unsafe { action.modifierFlags() }.contains(NSEventModifierFlags::Command) {
            self.app().open_tab(&url, true);
            return WKNavigationActionPolicy::Cancel;
        }
        WKNavigationActionPolicy::Allow
    }

    /// Sync buttons, address bar and window title.
    pub fn refresh_toolbar(&self) {
        let iv = self.ivars();
        let (Some(bar), Some(window)) = (iv.bar.get(), iv.window.get()) else { return };
        let web = self.web();
        let (back, fwd, loading) = web.as_ref().map_or((false, false, false), |w| unsafe {
            (w.canGoBack(), w.canGoForward(), w.isLoading())
        });
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
            Some(w) => unsafe { w.URL() }.and_then(|u| u.absoluteString()).map(|s| s.to_string()).unwrap_or_default(),
            None => self.url(),
        };
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
            println!("{kind} {:.1} {detail}", start.elapsed().as_secs_f64() * 1000.0);
        }
    }

    /// A sheet on this tab's window. `input` adds a text field. `done(ok, text)`.
    fn sheet(&self, message: &NSString, buttons: &[&str], input: Option<String>, done: impl Fn(bool, Option<String>) + 'static) {
        let mtm = self.mtm();
        let alert = NSAlert::new(mtm);
        alert.setMessageText(message);
        for b in buttons {
            alert.addButtonWithTitle(&NSString::from_str(b));
        }
        let field = input.map(|text| {
            let f = NSTextField::textFieldWithString(&NSString::from_str(&text), mtm);
            f.setFrame(NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(260.0, 24.0)));
            alert.setAccessoryView(Some(&f));
            f
        });
        let handler = RcBlock::new(move |response: isize| {
            // First button is NSAlertFirstButtonReturn (1000).
            done(response == 1000, field.as_ref().map(|f| f.stringValue().to_string()));
        });
        alert.beginSheetModalForWindow_completionHandler(self.window(), Some(&handler));
    }
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
    NSImage::imageWithSystemSymbolName_accessibilityDescription(&NSString::from_str(symbol), Some(&NSString::from_str(label)))
        .expect("SF Symbol missing")
}
