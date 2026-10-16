//! Search Tabs (⇧⌘A): a sheet with a search field over a list of the open
//! tabs, by title and address. Return switches to the first (or selected)
//! match, ↑/↓ move, Tab/⇧Tab go round the list, Esc closes. Built when asked for, gone when closed.

use std::cell::{OnceCell, RefCell};

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject, Sel};
use objc2::{DefinedClass, MainThreadOnly, Message, define_class, msg_send, sel};
use objc2_app_kit::{
    NSAlert, NSControl, NSControlTextEditingDelegate, NSScrollView, NSSearchField, NSSearchFieldDelegate,
    NSTableColumn, NSTableView, NSTableViewDataSource, NSTextFieldDelegate, NSTextView, NSView,
};
use objc2_foundation::{
    NSIndexSet, NSInteger, NSNotification, NSObject, NSObjectProtocol, NSPoint, NSRect, NSSize, NSString,
};

use crate::App;
use crate::tab::Tab;

#[derive(Default)]
pub struct SearchIvars {
    app: OnceCell<Retained<App>>,
    /// Every tab that can be found, most recently used first: (tab, title, address).
    tabs: RefCell<Vec<(Retained<Tab>, String, String)>>,
    /// Indexes into `tabs` matching the query, best first.
    shown: RefCell<Vec<usize>>,
    field: OnceCell<Retained<NSSearchField>>,
    table: OnceCell<Retained<NSTableView>>,
    alert: OnceCell<Retained<NSAlert>>,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[ivars = SearchIvars]
    pub struct TabSearch;

    unsafe impl NSObjectProtocol for TabSearch {}

    unsafe impl NSTableViewDataSource for TabSearch {
        #[unsafe(method(numberOfRowsInTableView:))]
        fn rows(&self, _t: &NSTableView) -> NSInteger {
            self.ivars().shown.borrow().len() as NSInteger
        }

        #[unsafe(method(tableView:objectValueForTableColumn:row:))]
        fn value(&self, _t: &NSTableView, _c: Option<&NSTableColumn>, row: NSInteger) -> *mut AnyObject {
            let tabs = self.ivars().tabs.borrow();
            let Some((_, title, url)) = self.ivars().shown.borrow().get(row as usize).map(|&i| &tabs[i]) else {
                return std::ptr::null_mut();
            };
            let text = if title.is_empty() { url.clone() } else { format!("{title}  —  {url}") };
            Retained::autorelease_return(NSString::from_str(&text)).cast()
        }
    }

    unsafe impl NSControlTextEditingDelegate for TabSearch {
        #[unsafe(method(controlTextDidChange:))]
        fn text_changed(&self, _n: &NSNotification) {
            self.filter();
        }

        /// Keys in the search field: ↑/↓ move through the list, Return
        /// switches, Esc closes.
        #[unsafe(method(control:textView:doCommandBySelector:))]
        fn command(&self, _c: &NSControl, _v: &NSTextView, command: Sel) -> bool {
            self.key(command)
        }
    }

    unsafe impl NSTextFieldDelegate for TabSearch {}
    unsafe impl NSSearchFieldDelegate for TabSearch {}

    impl TabSearch {
        /// A double-clicked row.
        #[unsafe(method(pick:))]
        fn pick(&self, _s: Option<&AnyObject>) {
            self.finish(1000);
        }
    }
);

impl TabSearch {
    /// Open Search Tabs on `host`, the active tab, over the tabs of its kind
    /// (private windows only find private tabs, and the other way round).
    /// `query` fills the field first (the bench uses it).
    pub fn open(app: &App, host: &Tab, query: &str) {
        let mtm = app.mtm();
        let this: Retained<Self> = {
            let this = Self::alloc(mtm).set_ivars(SearchIvars::default());
            unsafe { msg_send![super(this), init] }
        };
        let _ = this.ivars().app.set(app.retain());
        let mut tabs: Vec<Retained<Tab>> =
            app.all_tabs().into_iter().filter(|t| t.is_private() == host.is_private()).collect();
        // Most recently used first; the tab you're on last.
        tabs.sort_by_key(|t| (app.is_active(t), std::cmp::Reverse(t.ivars().last_active.get())));
        this.ivars().tabs.replace(tabs.into_iter().map(|t| (t.clone(), t.title(), t.url())).collect());

        let (w, h) = (480.0, 300.0);
        let view = NSView::initWithFrame(NSView::alloc(mtm), NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(w, h)));
        let field = NSSearchField::new(mtm);
        field.setFrame(NSRect::new(NSPoint::new(0.0, h - 24.0), NSSize::new(w, 24.0)));
        field.setPlaceholderString(Some(&NSString::from_str("Title or address")));
        field.setStringValue(&NSString::from_str(query));
        unsafe { field.setDelegate(Some(ProtocolObject::from_ref(&*this))) };
        view.addSubview(&field);
        let table = NSTableView::initWithFrame(
            NSTableView::alloc(mtm),
            NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(w, h - 32.0)),
        );
        let column = NSTableColumn::initWithIdentifier(NSTableColumn::alloc(mtm), &NSString::from_str("tab"));
        column.setWidth(w - 20.0);
        column.setEditable(false);
        table.addTableColumn(&column);
        table.setHeaderView(None);
        unsafe {
            table.setDataSource(Some(ProtocolObject::from_ref(&*this)));
            table.setTarget(Some(&this));
            table.setDoubleAction(Some(sel!(pick:)));
        }
        let scroll = NSScrollView::initWithFrame(
            NSScrollView::alloc(mtm),
            NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(w, h - 32.0)),
        );
        scroll.setHasVerticalScroller(true);
        scroll.setDocumentView(Some(&table));
        view.addSubview(&scroll);

        let alert = NSAlert::new(mtm);
        alert.setMessageText(&NSString::from_str("Search Tabs"));
        alert.addButtonWithTitle(&NSString::from_str("Switch"));
        alert.addButtonWithTitle(&NSString::from_str("Cancel"));
        alert.setAccessoryView(Some(&view));
        let _ = this.ivars().field.set(field.clone());
        let _ = this.ivars().table.set(table);
        let _ = this.ivars().alert.set(alert.clone());
        this.filter();
        if app.is_bench() {
            let tabs = this.ivars().tabs.borrow();
            let titles: Vec<&str> = this.ivars().shown.borrow().iter().map(|&i| tabs[i].1.as_str()).collect();
            say!("TABSEARCH {}", titles.join("|"));
        }
        // The sheet's handler keeps this alive until the sheet closes.
        let keep = this.clone();
        host.present(&alert, move |response| keep.done(response));
        alert.window().makeFirstResponder(Some(&field));
    }

    /// (`define_class!` methods returning bool can't return early.)
    fn key(&self, command: Sel) -> bool {
        let Some(table) = self.ivars().table.get() else { return false };
        let n = self.ivars().shown.borrow().len() as NSInteger;
        let current = table.selectedRow();
        let row = if command == sel!(moveUp:) {
            (current - 1).clamp(0, n - 1)
        } else if command == sel!(moveDown:) {
            (current + 1).clamp(0, n - 1)
        } else if command == sel!(insertTab:) {
            cycle(current, 1, n)
        } else if command == sel!(insertBacktab:) {
            cycle(current, -1, n)
        } else if command == sel!(insertNewline:) {
            self.finish(1000);
            return true;
        } else if command == sel!(cancelOperation:) {
            self.finish(1001);
            return true;
        } else {
            return false;
        };
        if n > 0 {
            table.selectRowIndexes_byExtendingSelection(&NSIndexSet::indexSetWithIndex(row as usize), false);
            table.scrollRowToVisible(row);
        }
        true
    }

    fn filter(&self) {
        let query = self.ivars().field.get().map(|f| f.stringValue().to_string()).unwrap_or_default();
        let mut shown: Vec<(u32, usize)> = self
            .ivars()
            .tabs
            .borrow()
            .iter()
            .enumerate()
            .filter_map(|(i, (_, title, url))| rank(&query, title, url).map(|r| (r, i)))
            .collect();
        shown.sort();
        self.ivars().shown.replace(shown.into_iter().map(|(_, i)| i).collect());
        if let Some(table) = self.ivars().table.get() {
            table.reloadData();
            table.selectRowIndexes_byExtendingSelection(&NSIndexSet::indexSetWithIndex(0), false);
        }
    }

    fn finish(&self, response: NSInteger) {
        if let Some(alert) = self.ivars().alert.get() {
            let window = alert.window();
            if let Some(parent) = window.sheetParent() {
                parent.endSheet_returnCode(&window, response);
            }
        }
    }

    fn done(&self, response: NSInteger) {
        if response != 1000 {
            return;
        }
        let row = self.ivars().table.get().map_or(0, |t| t.selectedRow().max(0) as usize);
        let tab = self.ivars().shown.borrow().get(row).map(|&i| self.ivars().tabs.borrow()[i].0.clone());
        if let (Some(tab), Some(app)) = (tab, self.ivars().app.get()) {
            app.select(&tab);
        }
    }
}

/// Tab/⇧Tab: the row `by` away from `row` among `n`, going round at the ends.
pub fn cycle(row: NSInteger, by: NSInteger, n: NSInteger) -> NSInteger {
    if n <= 0 { 0 } else { (row.max(0) + by).rem_euclid(n) }
}

/// How well a tab matches: lower is better, None if it doesn't. The query's
/// words must all appear (in any case) in the title or address; a word at
/// the start of the title beats one inside it, and the title beats the address.
pub fn rank(query: &str, title: &str, url: &str) -> Option<u32> {
    let (title, url) = (title.to_lowercase(), url.to_lowercase());
    let url = url.split_once("://").map_or(url.as_str(), |(_, rest)| rest).trim_start_matches("www.");
    query.to_lowercase().split_whitespace().try_fold(0, |score, word| {
        let r = match (title.find(word), url.find(word)) {
            (Some(0), _) => 0,
            (Some(_), _) => 1,
            (None, Some(0)) => 2,
            (None, Some(_)) => 3,
            (None, None) => return None,
        };
        Some(score + r)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tabs_match_every_word_title_first() {
        assert_eq!(rank("", "Anything", "https://a.example/"), Some(0), "an empty query lists everything");
        assert_eq!(rank("rust", "Rust Book", "https://doc.rust-lang.org/book/"), Some(0));
        assert_eq!(rank("book", "Rust Book", "https://doc.rust-lang.org/book/"), Some(1));
        assert_eq!(rank("github", "Pull requests", "https://www.github.com/pulls"), Some(2), "www. and scheme skipped");
        assert_eq!(rank("pulls", "Issues", "https://github.com/pulls"), Some(3));
        assert_eq!(rank("RUST book", "Rust Book", ""), Some(1), "words in any case, all must match");
        assert_eq!(rank("rust python", "Rust Book", "https://rust.example/"), None);
    }

    #[test]
    fn tab_goes_round_the_list() {
        assert_eq!(cycle(0, 1, 3), 1);
        assert_eq!(cycle(2, 1, 3), 0, "past the last, back to the first");
        assert_eq!(cycle(0, -1, 3), 2, "⇧Tab before the first, to the last");
        assert_eq!(cycle(-1, 1, 3), 1, "nothing selected counts as the first");
        assert_eq!(cycle(0, 1, 0), 0, "no rows");
    }
}
