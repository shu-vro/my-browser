//! A browser window as web extensions see it (the `windows` API). Each native
//! tab group (one window with its tabs) gets one of these.

use std::cell::OnceCell;

use block2::DynBlock;
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{define_class, msg_send, DefinedClass, MainThreadOnly, Message};
use objc2_app_kit::NSWindowTabGroup;
use objc2_foundation::{NSArray, NSError, NSObject, NSObjectProtocol};
use objc2_web_kit::{
    WKWebExtensionContext, WKWebExtensionTab, WKWebExtensionWindow, WKWebExtensionWindowState, WKWebExtensionWindowType,
};

use crate::tab::Tab;
use crate::App;

#[derive(Default)]
pub struct ExtWindowIvars {
    app: OnceCell<Retained<App>>,
    group: OnceCell<Retained<NSWindowTabGroup>>,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[ivars = ExtWindowIvars]
    pub struct ExtWindow;

    unsafe impl NSObjectProtocol for ExtWindow {}

    unsafe impl WKWebExtensionWindow for ExtWindow {
        #[unsafe(method(tabsForWebExtensionContext:))]
        fn tabs_for(&self, _ctx: &WKWebExtensionContext) -> *mut NSArray<ProtocolObject<dyn WKWebExtensionTab>> {
            let tabs: Vec<Retained<ProtocolObject<dyn WKWebExtensionTab>>> =
                self.tabs().into_iter().map(ProtocolObject::from_retained).collect();
            Retained::autorelease_return(NSArray::from_retained_slice(&tabs))
        }

        #[unsafe(method(activeTabForWebExtensionContext:))]
        fn active_tab_for(&self, _ctx: &WKWebExtensionContext) -> *mut ProtocolObject<dyn WKWebExtensionTab> {
            let selected = self.group().selectedWindow();
            let tab = selected.and_then(|w| self.tabs().into_iter().find(|t| std::ptr::eq(t.window(), &*w)));
            tab.map_or(std::ptr::null_mut(), |t| Retained::autorelease_return(ProtocolObject::from_retained(t)))
        }

        #[unsafe(method(windowTypeForWebExtensionContext:))]
        fn window_type_for(&self, _ctx: &WKWebExtensionContext) -> WKWebExtensionWindowType {
            WKWebExtensionWindowType::Normal
        }

        #[unsafe(method(windowStateForWebExtensionContext:))]
        fn window_state_for(&self, _ctx: &WKWebExtensionContext) -> WKWebExtensionWindowState {
            let minimized = self.tabs().first().is_some_and(|t| t.window().isMiniaturized());
            if minimized { WKWebExtensionWindowState::Minimized } else { WKWebExtensionWindowState::Normal }
        }

        #[unsafe(method(isPrivateForWebExtensionContext:))]
        fn is_private_for(&self, _ctx: &WKWebExtensionContext) -> bool {
            self.app().profile_dir().is_none()
        }

        #[unsafe(method(focusForWebExtensionContext:completionHandler:))]
        fn focus_for(&self, _ctx: &WKWebExtensionContext, done: &DynBlock<dyn Fn(*mut NSError)>) {
            if let Some(w) = self.group().selectedWindow() {
                w.makeKeyAndOrderFront(None);
            }
            done.call((std::ptr::null_mut(),));
        }

        #[unsafe(method(closeForWebExtensionContext:completionHandler:))]
        fn close_for(&self, _ctx: &WKWebExtensionContext, done: &DynBlock<dyn Fn(*mut NSError)>) {
            for t in self.tabs() {
                t.window().performClose(None);
            }
            done.call((std::ptr::null_mut(),));
        }
    }
);

impl ExtWindow {
    pub fn new(app: &App, group: &NSWindowTabGroup) -> Retained<Self> {
        let this = Self::alloc(app.mtm()).set_ivars(ExtWindowIvars::default());
        let this: Retained<Self> = unsafe { msg_send![super(this), init] };
        let _ = this.ivars().app.set(app.retain());
        let _ = this.ivars().group.set(group.retain());
        this
    }

    fn app(&self) -> &App {
        self.ivars().app.get().unwrap()
    }

    pub fn group(&self) -> &NSWindowTabGroup {
        self.ivars().group.get().unwrap()
    }

    /// This window's tabs, in tab-bar order.
    pub fn tabs(&self) -> Vec<Retained<Tab>> {
        self.group().windows().iter().filter_map(|w| self.app().tab_for_window(&w)).collect()
    }
}
