//! The launcher (Window → Search Tabs…, ⇧⌘A): one search field over the open
//! tabs of the window's kind, then bookmarks, history (not in private
//! windows) and the menu bar's commands with their shortcuts. A sheet on the
//! active tab: a material panel, no chrome, a cell-based table. Return opens
//! the selected (first) row, ↑/↓ move, Tab/⇧Tab go round, Esc closes,
//! double-click opens. Built when asked for, freed when the sheet closes.
//! Titles and addresses come from pages: plain strings, never markup.

use std::cell::{OnceCell, RefCell};

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject, Sel};
use objc2::{DefinedClass, MainThreadOnly, Message, define_class, msg_send, sel};
use objc2_app_kit::{
    NSApplication, NSBackingStoreType, NSBox, NSBoxType, NSColor, NSControl, NSControlTextEditingDelegate,
    NSFocusRingType, NSFont, NSFontAttributeName, NSForegroundColorAttributeName, NSImage, NSImageCell, NSImageScaling,
    NSImageView, NSLineBreakMode, NSMenu, NSMenuItem, NSPanel, NSScrollView, NSTableColumn, NSTableView,
    NSTableViewDataSource, NSTableViewStyle, NSTextAlignment, NSTextField, NSTextFieldCell, NSTextFieldDelegate,
    NSTextView, NSVisualEffectBlendingMode, NSVisualEffectMaterial, NSVisualEffectState, NSVisualEffectView,
    NSWindowButton, NSWindowDelegate, NSWindowOrderingMode, NSWindowStyleMask, NSWindowTitleVisibility,
};
use objc2_foundation::{
    NSAttributedString, NSDictionary, NSIndexSet, NSInteger, NSMutableAttributedString, NSNotification, NSObject,
    NSObjectProtocol, NSPoint, NSRect, NSSize, NSString,
};

use crate::App;
use crate::tab::Tab;

thread_local! {
    /// The launcher on screen, if any: what keeps it alive. Taken when it
    /// closes, and it's freed with its panel.
    static OPEN: RefCell<Option<Retained<TabSearch>>> = const { RefCell::new(None) };
}

/// What a row opens. Kinds are listed in this order for equal matches.
#[derive(Clone)]
enum Target {
    Tab(Retained<Tab>),
    Command(Retained<NSMenuItem>),
    Bookmark(String),
    History(String),
}

#[derive(Clone)]
struct Row {
    target: Target,
    title: String,
    /// The address, or for a command the menu it's in.
    detail: String,
    /// A command's shortcut.
    hint: String,
}

impl Row {
    fn kind(&self) -> (&'static str, &'static str) {
        match self.target {
            Target::Tab(_) => ("tab", "square.on.square"),
            Target::Command(_) => ("command", "command"),
            Target::Bookmark(_) => ("bookmark", "star"),
            Target::History(_) => ("history", "clock"),
        }
    }

    fn order(&self) -> u8 {
        match self.target {
            Target::Tab(_) => 0,
            Target::Command(_) => 1,
            Target::Bookmark(_) => 2,
            Target::History(_) => 3,
        }
    }
}

/// Bookmarks and history pages listed at most, each.
const PAGES: usize = 8;
const W: f64 = 640.0;
const FIELD_H: f64 = 56.0;
const LIST_H: f64 = 300.0;

#[derive(Default)]
pub struct SearchIvars {
    app: OnceCell<Retained<App>>,
    host: OnceCell<Retained<Tab>>,
    /// The open tabs of the host's kind, most recently used first.
    tabs: RefCell<Vec<Row>>,
    /// The menu bar's commands, in menu order.
    commands: RefCell<Vec<Row>>,
    /// What's listed for the query, best first.
    shown: RefCell<Vec<Row>>,
    field: OnceCell<Retained<NSTextField>>,
    table: OnceCell<Retained<NSTableView>>,
    panel: OnceCell<Retained<NSPanel>>,
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
        fn value(&self, _t: &NSTableView, c: Option<&NSTableColumn>, row: NSInteger) -> *mut AnyObject {
            let shown = self.ivars().shown.borrow();
            let (Some(r), Some(c)) = (shown.get(row as usize), c) else { return std::ptr::null_mut() };
            let value: Retained<AnyObject> = match c.identifier().to_string().as_str() {
                "icon" => {
                    let (kind, symbol) = r.kind();
                    let image = NSImage::imageWithSystemSymbolName_accessibilityDescription(
                        &NSString::from_str(symbol),
                        Some(&NSString::from_str(kind)),
                    );
                    let Some(image) = image else { return std::ptr::null_mut() };
                    image.into()
                }
                "hint" => NSString::from_str(&r.hint).into(),
                _ => label(&r.title, &r.detail).into(),
            };
            Retained::autorelease_return(value)
        }
    }

    unsafe impl NSControlTextEditingDelegate for TabSearch {
        #[unsafe(method(controlTextDidChange:))]
        fn text_changed(&self, _n: &NSNotification) {
            self.filter();
        }

        /// Keys in the field: ↑/↓, Tab/⇧Tab, Return, Esc.
        #[unsafe(method(control:textView:doCommandBySelector:))]
        fn command(&self, _c: &NSControl, _v: &NSTextView, command: Sel) -> bool {
            self.key(command)
        }
    }

    unsafe impl NSTextFieldDelegate for TabSearch {}

    unsafe impl NSWindowDelegate for TabSearch {
        /// A click elsewhere closes it, as a menu would.
        #[unsafe(method(windowDidResignKey:))]
        fn resigned(&self, _n: &NSNotification) {
            self.finish(false);
        }
    }

    impl TabSearch {
        /// A double-clicked row.
        #[unsafe(method(pick:))]
        fn pick(&self, _s: Option<&AnyObject>) {
            self.finish(true);
        }    }
);

/// A row's text: the title, then the address in the secondary color.
fn label(title: &str, detail: &str) -> Retained<NSAttributedString> {
    let styled = |text: &str, font: &NSFont, color: &NSColor| {
        let keys = unsafe { [NSFontAttributeName, NSForegroundColorAttributeName] };
        let attrs = NSDictionary::<_, AnyObject>::from_slices(&keys, &[font.as_ref(), color.as_ref()]);
        unsafe { NSAttributedString::new_with_attributes(&NSString::from_str(text), &attrs) }
    };
    let out = NSMutableAttributedString::new();
    let title = if title.is_empty() { detail } else { title };
    out.appendAttributedString(&styled(title, &NSFont::systemFontOfSize(14.0), &NSColor::labelColor()));
    if title != detail && !detail.is_empty() {
        let detail = format!("   {detail}");
        out.appendAttributedString(&styled(&detail, &NSFont::systemFontOfSize(12.0), &NSColor::secondaryLabelColor()));
    }
    Retained::into_super(out)
}

/// One line of at most 300 characters: page titles can hold anything.
fn clean(s: &str) -> String {
    s.chars().map(|c| if c.is_control() { ' ' } else { c }).take(300).collect()
}

impl TabSearch {
    /// Open the launcher on `host`, the active tab. Private windows only find
    /// private tabs and no history; normal windows never find private tabs.
    /// `query` fills the field first. In the bench, `all` reports every row
    /// (`LAUNCHER kind:title [shortcut]|…`), otherwise the tabs (`TABSEARCH title|…`).
    pub fn open(app: &App, host: &Tab, query: &str, all: bool) {
        // One at a time (⇧⌘A in another window moves it there).
        Self::answer(false);
        let mtm = app.mtm();
        let this: Retained<Self> = {
            let this = Self::alloc(mtm).set_ivars(SearchIvars::default());
            unsafe { msg_send![super(this), init] }
        };
        let _ = this.ivars().app.set(app.retain());
        let _ = this.ivars().host.set(host.retain());
        let mut tabs: Vec<Retained<Tab>> =
            app.all_tabs().into_iter().filter(|t| t.is_private() == host.is_private()).collect();
        // Most recently used first; the tab you're on last.
        tabs.sort_by_key(|t| (app.is_active(t), std::cmp::Reverse(t.ivars().last_active.get())));
        this.ivars().tabs.replace(
            tabs.into_iter()
                .map(|t| Row {
                    title: clean(&t.title()),
                    detail: clean(&t.url()),
                    hint: String::new(),
                    target: Target::Tab(t),
                })
                .collect(),
        );
        this.ivars().commands.replace(commands(app));

        let panel = NSPanel::initWithContentRect_styleMask_backing_defer(
            NSPanel::alloc(mtm),
            NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(W, FIELD_H + LIST_H)),
            NSWindowStyleMask::Titled | NSWindowStyleMask::FullSizeContentView,
            NSBackingStoreType::Buffered,
            true,
        );
        // Titled for its rounded corners, shadow and keyboard focus; the title
        // bar is hidden under the content.
        unsafe { panel.setReleasedWhenClosed(false) };
        panel.setTitlebarAppearsTransparent(true);
        panel.setTitleVisibility(NSWindowTitleVisibility::Hidden);
        for b in [NSWindowButton::CloseButton, NSWindowButton::MiniaturizeButton, NSWindowButton::ZoomButton] {
            if let Some(b) = panel.standardWindowButton(b) {
                b.setHidden(true);
            }
        }
        panel.setMovable(false);
        panel.setHidesOnDeactivate(false);
        panel.setDelegate(Some(ProtocolObject::from_ref(&*this)));
        let rect = |x: f64, y: f64, w: f64, h: f64| NSRect::new(NSPoint::new(x, y), NSSize::new(w, h));
        let content =
            NSVisualEffectView::initWithFrame(NSVisualEffectView::alloc(mtm), rect(0.0, 0.0, W, FIELD_H + LIST_H));
        content.setMaterial(NSVisualEffectMaterial::Popover);
        content.setBlendingMode(NSVisualEffectBlendingMode::BehindWindow);
        content.setState(NSVisualEffectState::Active);
        panel.setContentView(Some(&content));

        let glass =
            NSImage::imageWithSystemSymbolName_accessibilityDescription(&NSString::from_str("magnifyingglass"), None);
        if let Some(glass) = glass {
            let icon = NSImageView::imageViewWithImage(&glass, mtm);
            icon.setFrame(rect(18.0, LIST_H + 16.0, 24.0, 24.0));
            icon.setImageScaling(NSImageScaling::ScaleProportionallyUpOrDown);
            icon.setContentTintColor(Some(&NSColor::secondaryLabelColor()));
            content.addSubview(&icon);
        }
        let field = NSTextField::textFieldWithString(&NSString::from_str(query), mtm);
        field.setFrame(rect(52.0, LIST_H + 14.0, W - 70.0, 28.0));
        field.setBezeled(false);
        field.setDrawsBackground(false);
        field.setFocusRingType(NSFocusRingType::None);
        field.setFont(Some(&NSFont::systemFontOfSize(22.0)));
        field.setPlaceholderString(Some(&NSString::from_str("Search tabs, bookmarks, history and commands")));
        field.setUsesSingleLineMode(true);
        unsafe { field.setDelegate(Some(ProtocolObject::from_ref(&*this))) };
        content.addSubview(&field);
        let line = NSBox::initWithFrame(NSBox::alloc(mtm), rect(0.0, LIST_H, W, 1.0));
        line.setBoxType(NSBoxType::Separator);
        content.addSubview(&line);

        let table = NSTableView::initWithFrame(NSTableView::alloc(mtm), rect(0.0, 0.0, W, LIST_H));
        table.setStyle(NSTableViewStyle::Inset);
        table.setHeaderView(None);
        // The spacing centers each line in its row: cells draw at their top.
        table.setRowHeight(20.0);
        table.setIntercellSpacing(NSSize::new(6.0, 12.0));
        table.setBackgroundColor(&NSColor::clearColor());
        table.setFocusRingType(NSFocusRingType::None);
        // Typing stays in the field; a click still selects.
        table.setRefusesFirstResponder(true);
        let hint_w = 110.0;
        for (id, width) in [("icon", 22.0), ("text", W - hint_w - 22.0 - 60.0), ("hint", hint_w)] {
            let column = NSTableColumn::initWithIdentifier(NSTableColumn::alloc(mtm), &NSString::from_str(id));
            column.setWidth(width);
            column.setEditable(false);
            if id == "icon" {
                let cell = NSImageCell::new(mtm);
                cell.setImageScaling(NSImageScaling::ScaleProportionallyDown);
                unsafe { column.setDataCell(&cell) };
            } else {
                let cell = NSTextFieldCell::new(mtm);
                cell.setLineBreakMode(NSLineBreakMode::ByTruncatingTail);
                if id == "hint" {
                    cell.setAlignment(NSTextAlignment::Right);
                    cell.setTextColor(Some(&NSColor::secondaryLabelColor()));
                    cell.setFont(Some(&NSFont::systemFontOfSize(13.0)));
                }
                unsafe { column.setDataCell(&cell) };
            }
            table.addTableColumn(&column);
        }
        unsafe {
            table.setDataSource(Some(ProtocolObject::from_ref(&*this)));
            table.setTarget(Some(&this));
            table.setDoubleAction(Some(sel!(pick:)));
        }
        let scroll = NSScrollView::initWithFrame(NSScrollView::alloc(mtm), rect(0.0, 0.0, W, LIST_H));
        scroll.setDrawsBackground(false);
        scroll.setHasVerticalScroller(true);
        scroll.setAutohidesScrollers(true);
        scroll.setDocumentView(Some(&table));
        content.addSubview(&scroll);

        let _ = this.ivars().field.set(field.clone());
        let _ = this.ivars().table.set(table);
        let _ = this.ivars().panel.set(panel.clone());
        this.filter();
        if app.is_bench() {
            let shown = this.ivars().shown.borrow();
            let line: Vec<String> = shown
                .iter()
                .filter(|r| all || matches!(r.target, Target::Tab(_)))
                .map(|r| match all {
                    true => format!("{}:{} {}", r.kind().0, r.title, r.hint).trim_end().to_string(),
                    false => r.title.clone(),
                })
                .collect();
            say!("{} {}", if all { "LAUNCHER" } else { "TABSEARCH" }, line.join("|"));
        }
        // A child panel near the top of the tab's window, not a sheet: ending
        // a sheet makes its window key and the app active, which a bench
        // window (`--on-top`) must never do. There it's shown, never key.
        let window = host.window();
        let frame = window.frame();
        let h = FIELD_H + LIST_H;
        panel.setFrameOrigin(NSPoint::new(
            frame.origin.x + ((frame.size.width - W) / 2.0).max(0.0),
            frame.origin.y + (frame.size.height - h - 110.0).max(0.0),
        ));
        panel.setLevel(window.level());
        unsafe { window.addChildWindow_ordered(&panel, NSWindowOrderingMode::Above) };
        panel.makeFirstResponder(Some(&field));
        OPEN.with(|o| o.replace(Some(this.clone())));
        if !app.is_on_top() {
            panel.makeKeyAndOrderFront(None);
        }
    }

    /// Close the open launcher, opening its selected row if `ok` (Return, the
    /// bench's `dialog ok|cancel`). False if none was open.
    pub fn answer(ok: bool) -> bool {
        let open = OPEN.with(|o| o.borrow().clone());
        open.inspect(|l| l.finish(ok)).is_some()
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
            self.finish(true);
            return true;
        } else if command == sel!(cancelOperation:) {
            self.finish(false);
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

    /// List what matches the field: tabs first (switching is what this is
    /// mostly for), then commands, bookmarks and history by how well they
    /// match. An empty field lists the tabs.
    fn filter(&self) {
        let iv = self.ivars();
        let query = iv.field.get().map(|f| f.stringValue().to_string()).unwrap_or_default();
        let ranked = |rows: &[Row]| -> Vec<(u32, Row)> {
            rows.iter().filter_map(|r| rank(&query, &r.title, &r.detail).map(|n| (n, r.clone()))).collect()
        };
        let mut tabs = ranked(&iv.tabs.borrow());
        tabs.sort_by_key(|(n, _)| *n);
        let mut rest = Vec::new();
        // The longest word narrows the database query; `rank` checks them all.
        if let (Some(word), Some(app), Some(host)) =
            (query.split_whitespace().max_by_key(|w| w.len()), iv.app.get(), iv.host.get())
        {
            rest = ranked(&iv.commands.borrow());
            // One letter matches half the menu bar: the best few.
            rest.sort_by_key(|(n, _)| *n);
            rest.truncate(PAGES);
            let store = app.store();
            let mut seen: Vec<String> = iv.tabs.borrow().iter().map(|r| r.detail.clone()).collect();
            let mut pages = |kind: fn(String) -> Target, found: Vec<(String, String)>| {
                let rows: Vec<Row> = found
                    .into_iter()
                    .filter(|(url, _)| !seen.contains(url))
                    .map(|(url, title)| Row {
                        title: clean(&title),
                        detail: clean(&url),
                        hint: String::new(),
                        target: kind(url),
                    })
                    .collect();
                let matched: Vec<(u32, Row)> = ranked(&rows).into_iter().take(PAGES).collect();
                seen.extend(matched.iter().map(|(_, r)| r.detail.clone()));
                matched
            };
            // ponytail: every bookmark matching the word is read, then ranked; a
            // LIMIT in `Store::bookmarks` if someone keeps tens of thousands.
            rest.extend(pages(Target::Bookmark, store.bookmarks(word).into_iter().map(|b| (b.url, b.title)).collect()));
            if !host.is_private() {
                let visited = store.suggestions(word, 40).into_iter().filter_map(|s| match s {
                    crate::store::Suggestion::Page(url, title) => Some((url, title)),
                    crate::store::Suggestion::Search(_) => None,
                });
                rest.extend(pages(Target::History, visited.collect()));
            }
            rest.sort_by_key(|(n, r)| (*n, r.order()));
        }
        iv.shown.replace(tabs.into_iter().chain(rest).map(|(_, r)| r).collect());
        if let Some(table) = iv.table.get() {
            table.reloadData();
            table.selectRowIndexes_byExtendingSelection(&NSIndexSet::indexSetWithIndex(0), false);
            table.scrollRowToVisible(0);
        }
    }

    fn finish(&self, ok: bool) {
        // Taken first: ordering the panel out resigns key, which comes back here.
        let Some(_this) = OPEN.with(|o| o.borrow_mut().take_if(|l| std::ptr::eq(&**l, self))) else { return };
        let iv = self.ivars();
        if let (Some(panel), Some(host), Some(app)) = (iv.panel.get(), iv.host.get(), iv.app.get()) {
            let window = host.window();
            window.removeChildWindow(panel);
            panel.orderOut(None);
            if !app.is_on_top() && window.isVisible() {
                window.makeKeyWindow();
            }
        }
        if ok {
            self.open_selected();
        }
    }

    /// Open the selected row as its menu item, or the History and Bookmarks
    /// menus, would.
    fn open_selected(&self) {
        let iv = self.ivars();
        let row = iv.table.get().map_or(0, |t| t.selectedRow().max(0) as usize);
        let Some(row) = iv.shown.borrow().get(row).cloned() else { return };
        let (Some(app), Some(host)) = (iv.app.get(), iv.host.get()) else { return };
        match row.target {
            Target::Tab(tab) => app.select(&tab),
            Target::Bookmark(url) | Target::History(url) => host.load(&url),
            // A click on the item: its action, validated, through the menu.
            Target::Command(item) => {
                // (The menu bar holds its menus; an item gone from one finds none.)
                if let Some(menu) = unsafe { item.menu() } {
                    let i = menu.indexOfItem(&item);
                    if i >= 0 {
                        menu.performActionForItemAtIndex(i);
                    }
                }
            }
        }
    }
}

/// The menu bar's commands: the items sent to the browser itself (not text
/// editing, the window list or the page lists), enabled, as the menus show
/// them when opened, with their menu and shortcut.
fn commands(app: &App) -> Vec<Row> {
    fn walk(app: &App, menu: &NSMenu, path: &str, out: &mut Vec<Row>) {
        match menu.title().to_string().as_str() {
            "Window" => app.update_window_menu(menu),
            "View" => app.update_view_menu(menu),
            _ => {}
        }
        menu.update();
        for item in menu.itemArray() {
            if item.isSeparatorItem() || item.isHidden() || !item.isEnabled() {
                continue;
            }
            let title = clean(&item.title().to_string());
            if let Some(sub) = item.submenu() {
                walk(app, &sub, &format!("{path} › {title}"), out);
                continue;
            }
            let to_app =
                item.target().is_some_and(|t| std::ptr::eq(&*t as *const AnyObject, (app as &AnyObject) as *const _));
            let skip = [sel!(searchTabs:), sel!(openMenuURL:)];
            if !to_app || item.action().is_none_or(|a| skip.contains(&a)) {
                continue;
            }
            let hint = crate::shortcuts::display(&item.keyEquivalent().to_string(), item.keyEquivalentModifierMask());
            // Zoom In is there twice (⌘+ and ⌘=).
            if out.iter().any(|r| r.title == title && r.detail == path) {
                continue;
            }
            out.push(Row { title, detail: path.to_string(), hint, target: Target::Command(item) });
        }
    }
    let mut out = Vec::new();
    if let Some(bar) = NSApplication::sharedApplication(app.mtm()).mainMenu() {
        for holder in bar.itemArray() {
            if let Some(menu) = holder.submenu() {
                walk(app, &menu, &menu.title().to_string(), &mut out);
            }
        }
    }
    out
}

/// Tab/⇧Tab: the row `by` away from `row` among `n`, going round at the ends.
pub fn cycle(row: NSInteger, by: NSInteger, n: NSInteger) -> NSInteger {
    if n <= 0 { 0 } else { (row.max(0) + by).rem_euclid(n) }
}

/// How well a row matches: lower is better, None if it doesn't. The query's
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

    #[test]
    fn page_text_stays_one_plain_line() {
        assert_eq!(clean("a\nb\tc\u{7}"), "a b c ");
        assert_eq!(clean(&"x".repeat(1000)).len(), 300);
    }
}
