//! Address bar suggestions: while you type, a list under the bar of past
//! searches and visited pages that match (`Store::suggestions`, all from the
//! profile's browser.db; nothing is asked of a search engine). ↑/↓ choose,
//! Return opens, Esc closes, a click opens. The panel is made on first use.

use std::cell::{OnceCell, RefCell};

use objc2::rc::{Retained, Weak};
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2::{DefinedClass, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{
    NSBackingStoreType, NSPanel, NSScrollView, NSTableColumn, NSTableView, NSTableViewDataSource, NSTextField,
    NSWindowOrderingMode, NSWindowStyleMask,
};
use objc2_foundation::{NSIndexSet, NSInteger, NSObject, NSObjectProtocol, NSPoint, NSRect, NSSize, NSString};

use crate::store::Suggestion;
use crate::tab::Tab;

const ROW_H: f64 = 24.0;
const MAX_ROWS: usize = 8;

#[derive(Default)]
pub struct SuggestIvars {
    tab: OnceCell<Weak<Tab>>,
    items: RefCell<Vec<Suggestion>>,
    panel: OnceCell<Retained<NSPanel>>,
    table: OnceCell<Retained<NSTableView>>,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[ivars = SuggestIvars]
    pub struct Suggest;

    unsafe impl NSObjectProtocol for Suggest {}

    unsafe impl NSTableViewDataSource for Suggest {
        #[unsafe(method(numberOfRowsInTableView:))]
        fn rows(&self, _t: &NSTableView) -> NSInteger {
            self.ivars().items.borrow().len() as NSInteger
        }

        #[unsafe(method(tableView:objectValueForTableColumn:row:))]
        fn value(&self, _t: &NSTableView, _c: Option<&NSTableColumn>, row: NSInteger) -> *mut AnyObject {
            let Some(item) = self.ivars().items.borrow().get(row as usize).cloned() else {
                return std::ptr::null_mut();
            };
            Retained::autorelease_return(NSString::from_str(&label(&item))).cast()
        }
    }

    impl Suggest {
        /// A row was clicked.
        #[unsafe(method(clicked:))]
        fn clicked(&self, _s: Option<&AnyObject>) {
            let row = self.ivars().table.get().map_or(-1, |t| t.clickedRow());
            let item = self.ivars().items.borrow().get(row.max(0) as usize).cloned();
            if let (Some(item), Some(tab)) = (item.filter(|_| row >= 0), self.tab()) {
                tab.open_suggestion(&item);
            }
        }
    }
);

/// How a suggestion reads in the list.
pub fn label(item: &Suggestion) -> String {
    match item {
        Suggestion::Search(text) => format!("🔍  {text}"),
        Suggestion::Page(url, title) if title.is_empty() => url.clone(),
        Suggestion::Page(url, title) => format!("{title}  —  {url}"),
    }
}

impl Suggest {
    pub fn new(tab: &Tab) -> Retained<Self> {
        let this = Self::alloc(tab.mtm()).set_ivars(SuggestIvars::default());
        let this: Retained<Self> = unsafe { msg_send![super(this), init] };
        let _ = this.ivars().tab.set(Weak::from(tab));
        this
    }

    fn tab(&self) -> Option<Retained<Tab>> {
        self.ivars().tab.get().and_then(|t| t.load())
    }

    pub fn items(&self) -> Vec<Suggestion> {
        self.ivars().items.borrow().clone()
    }

    pub fn is_shown(&self) -> bool {
        self.ivars().panel.get().is_some_and(|p| p.isVisible())
    }

    /// Show what matches `items` under `bar`, nothing chosen yet, or hide
    /// the list if nothing does.
    pub fn show(&self, items: Vec<Suggestion>, bar: &NSTextField) {
        let empty = items.is_empty();
        let rows = items.len().min(MAX_ROWS);
        self.ivars().items.replace(items);
        if empty {
            return self.hide();
        }
        let Some(window) = bar.window() else { return };
        let (panel, table) = self.build();
        table.reloadData();
        unsafe { table.deselectAll(None) };
        let below = window.convertRectToScreen(bar.convertRect_toView(bar.bounds(), None));
        let height = rows as f64 * ROW_H + 6.0;
        panel.setFrame_display(
            NSRect::new(
                NSPoint::new(below.origin.x, below.origin.y - height - 2.0),
                NSSize::new(below.size.width, height),
            ),
            true,
        );
        // A column doesn't follow its scroll view's width: without this it
        // stays 100 points wide and cuts every row short.
        if let Some(column) = table.tableColumns().firstObject() {
            column.setWidth(below.size.width - 6.0);
        }
        if !panel.isVisible() {
            unsafe { window.addChildWindow_ordered(&panel, NSWindowOrderingMode::Above) };
        }
    }

    pub fn hide(&self) {
        if let Some(panel) = self.ivars().panel.get().filter(|p| p.isVisible()) {
            if let Some(parent) = panel.parentWindow() {
                parent.removeChildWindow(panel);
            }
            panel.orderOut(None);
        }
    }

    /// ↑/↓ in the bar: move the choice. False if the list isn't shown.
    pub fn step(&self, by: NSInteger) -> bool {
        let (Some(table), true) = (self.ivars().table.get(), self.is_shown()) else { return false };
        let n = self.ivars().items.borrow().len() as NSInteger;
        let row = if table.selectedRow() < 0 && by < 0 { n - 1 } else { (table.selectedRow() + by).clamp(0, n - 1) };
        table.selectRowIndexes_byExtendingSelection(&NSIndexSet::indexSetWithIndex(row as usize), false);
        table.scrollRowToVisible(row);
        true
    }

    /// The chosen suggestion, if one is.
    pub fn chosen(&self) -> Option<Suggestion> {
        let table = self.ivars().table.get().filter(|_| self.is_shown())?;
        self.ivars().items.borrow().get(usize::try_from(table.selectedRow()).ok()?).cloned()
    }

    fn build(&self) -> (Retained<NSPanel>, Retained<NSTableView>) {
        if let (Some(p), Some(t)) = (self.ivars().panel.get(), self.ivars().table.get()) {
            return (p.clone(), t.clone());
        }
        let mtm = self.mtm();
        let frame = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(400.0, 200.0));
        // Never key: typing stays in the address bar while the list is up.
        let panel = NSPanel::initWithContentRect_styleMask_backing_defer(
            NSPanel::alloc(mtm),
            frame,
            NSWindowStyleMask::Borderless | NSWindowStyleMask::NonactivatingPanel,
            NSBackingStoreType::Buffered,
            true,
        );
        unsafe { panel.setReleasedWhenClosed(false) };
        panel.setBecomesKeyOnlyIfNeeded(true);
        panel.setHasShadow(true);
        let table = NSTableView::initWithFrame(NSTableView::alloc(mtm), frame);
        let column = NSTableColumn::initWithIdentifier(NSTableColumn::alloc(mtm), &NSString::from_str("s"));
        column.setEditable(false);
        column.setResizingMask(objc2_app_kit::NSTableColumnResizingOptions::AutoresizingMask);
        table.addTableColumn(&column);
        table.setHeaderView(None);
        table.setRowHeight(ROW_H - 4.0);
        table.setRefusesFirstResponder(true);
        table.setColumnAutoresizingStyle(
            objc2_app_kit::NSTableViewColumnAutoresizingStyle::UniformColumnAutoresizingStyle,
        );
        unsafe {
            table.setDataSource(Some(ProtocolObject::from_ref(self)));
            table.setTarget(Some(self));
            table.setAction(Some(sel!(clicked:)));
        }
        let scroll = NSScrollView::initWithFrame(NSScrollView::alloc(mtm), frame);
        scroll.setDocumentView(Some(&table));
        scroll.setAutoresizingMask(
            objc2_app_kit::NSAutoresizingMaskOptions::ViewWidthSizable
                | objc2_app_kit::NSAutoresizingMaskOptions::ViewHeightSizable,
        );
        panel.setContentView(Some(&scroll));
        let _ = self.ivars().panel.set(panel.clone());
        let _ = self.ivars().table.set(table.clone());
        (panel, table)
    }
}
