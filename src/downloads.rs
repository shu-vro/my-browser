//! The downloads button right of the address bar and its list, as in Safari.
//! A download is written to `<name>.download` and renamed when it finishes;
//! cancelled and failed ones delete it, so a half file never sits in the
//! folder under its real name. The list holds this session's downloads of
//! the window's kind (private ones only in private windows), newest first.
//! Built when the button is clicked, freed when the popover closes; while it's
//! open the app's half-second download tick refreshes it.

use std::cell::{OnceCell, RefCell};
use std::path::{Path, PathBuf};

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2::{DefinedClass, MainThreadOnly, Message, define_class, msg_send, sel};
use objc2_app_kit::{
    NSBox, NSBoxType, NSButton, NSColor, NSControlSize, NSFont, NSImage, NSImageView, NSLineBreakMode, NSPopover,
    NSPopoverBehavior, NSPopoverDelegate, NSProgressIndicator, NSProgressIndicatorStyle, NSTextField, NSView,
    NSViewController, NSWorkspace,
};
use objc2_foundation::{
    NSArray, NSNotification, NSObject, NSObjectProtocol, NSPoint, NSRect, NSRectEdge, NSSize, NSString, NSURL,
};

use crate::App;

/// A download of this session, for the toolbar list.
#[derive(Clone)]
pub struct Item {
    /// Unique for the session (from 1).
    pub key: usize,
    /// Where the file ends up (it's `<path>.download` until then).
    pub path: String,
    /// active | done | cancelled | failed
    pub status: &'static str,
    pub bytes: i64,
    /// 0 while the size isn't known.
    pub total: i64,
    pub private: bool,
}

/// Where a download is written until it finishes.
pub fn part_path(path: &Path) -> PathBuf {
    let mut p = path.as_os_str().to_owned();
    p.push(".download");
    PathBuf::from(p)
}

fn size(b: i64) -> String {
    match b {
        b if b < 1024 => format!("{b} bytes"),
        b if b < 1 << 20 => format!("{} KB", b >> 10),
        b if b < 1 << 30 => format!("{:.1} MB", b as f64 / (1 << 20) as f64),
        b => format!("{:.2} GB", b as f64 / (1 << 30) as f64),
    }
}

/// The line under a download's name.
pub fn status_text(item: &Item) -> String {
    match item.status {
        "active" if item.total > 0 => format!("{} of {}", size(item.bytes), size(item.total)),
        "active" => size(item.bytes),
        "done" if !Path::new(&item.path).exists() => "Moved or deleted".into(),
        "done" => size(item.bytes),
        "cancelled" => "Cancelled".into(),
        _ => "Failed".into(),
    }
}

/// What the toolbar button's bar shows: done / total over the active
/// downloads, `None` with none active (or no size known yet).
pub fn overall(items: &[Item]) -> Option<f64> {
    let active: Vec<_> = items.iter().filter(|i| i.status == "active").collect();
    let total: i64 = active.iter().map(|i| i.total).sum();
    if active.is_empty() {
        None
    } else if total <= 0 || active.iter().any(|i| i.total <= 0) {
        Some(0.0)
    } else {
        Some(active.iter().map(|i| i.bytes).sum::<i64>() as f64 / total as f64)
    }
}

thread_local! {
    /// The list on screen: what keeps it alive until its popover closes.
    static OPEN: RefCell<Option<Retained<Panel>>> = const { RefCell::new(None) };
}

const W: f64 = 340.0;
const ROW_H: f64 = 48.0;
const FOOT_H: f64 = 36.0;
/// Rows listed at most; Show All Downloads has the rest.
const ROWS: usize = 8;

#[derive(Default)]
pub struct PanelIvars {
    app: OnceCell<Retained<App>>,
    private: OnceCell<bool>,
    popover: OnceCell<Retained<NSPopover>>,
    /// What's listed: (key, status), to tell when to rebuild.
    shape: RefCell<Vec<(usize, &'static str)>>,
    /// Per row: its progress bar and status line, updated in place.
    rows: RefCell<Vec<(Retained<NSProgressIndicator>, Retained<NSTextField>)>>,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[ivars = PanelIvars]
    pub struct Panel;

    unsafe impl NSObjectProtocol for Panel {}

    unsafe impl NSPopoverDelegate for Panel {
        #[unsafe(method(popoverDidClose:))]
        fn popover_did_close(&self, _n: &NSNotification) {
            OPEN.with(|o| o.borrow_mut().take());
        }
    }

    impl Panel {
        /// A row's name: open the file (done), else nothing.
        #[unsafe(method(openItem:))]
        fn open_item(&self, sender: &NSButton) {
            if let Some(item) = self.item(sender.tag() as usize).filter(|i| i.status == "done") {
                NSWorkspace::sharedWorkspace().openURL(&NSURL::fileURLWithPath(&NSString::from_str(&item.path)));
            }
        }

        /// A row's button: cancel (active) or show in Finder (done).
        #[unsafe(method(itemButton:))]
        fn item_button(&self, sender: &NSButton) {
            let Some(item) = self.item(sender.tag() as usize) else { return };
            if item.status == "active" {
                self.app().cancel_download_key(item.key);
            } else if Path::new(&item.path).exists() {
                let url = NSURL::fileURLWithPath(&NSString::from_str(&item.path));
                NSWorkspace::sharedWorkspace().activateFileViewerSelectingURLs(&NSArray::from_retained_slice(&[url]));
            }
        }

        #[unsafe(method(showAll:))]
        fn show_all(&self, _s: Option<&AnyObject>) {
            self.close();
            self.app().open_internal("settings#downloads");
        }

        #[unsafe(method(clearList:))]
        fn clear_list(&self, _s: Option<&AnyObject>) {
            self.app().clear_shelf(self.private());
            if self.items().is_empty() {
                self.close();
            } else {
                self.refresh();
            }
        }
    }
);

impl Panel {
    /// Show the list under `button` (a tab's downloads button).
    pub fn open(app: &App, private: bool, button: &NSButton) {
        let mtm = app.mtm();
        let this = Self::alloc(mtm).set_ivars(PanelIvars::default());
        let this: Retained<Self> = unsafe { msg_send![super(this), init] };
        let _ = this.ivars().app.set(app.retain());
        let _ = this.ivars().private.set(private);
        let popover = NSPopover::new(mtm);
        popover.setBehavior(NSPopoverBehavior::Transient);
        popover.setDelegate(Some(ProtocolObject::from_ref(&*this)));
        let _ = this.ivars().popover.set(popover.clone());
        this.rebuild();
        popover.showRelativeToRect_ofView_preferredEdge(button.bounds(), button, NSRectEdge::MinY);
        OPEN.with(|o| o.replace(Some(this)));
    }

    /// The tick: new rows or a changed status rebuild the list, else the
    /// bars and lines move.
    pub fn refresh_open() {
        if let Some(p) = OPEN.with(|o| o.borrow().clone()) {
            p.refresh();
        }
    }

    fn app(&self) -> &App {
        self.ivars().app.get().unwrap()
    }

    fn private(&self) -> bool {
        *self.ivars().private.get().unwrap()
    }

    fn items(&self) -> Vec<Item> {
        let mut items = self.app().shelf(self.private());
        items.truncate(ROWS);
        items
    }

    fn item(&self, row: usize) -> Option<Item> {
        self.items().into_iter().nth(row)
    }

    fn close(&self) {
        if let Some(p) = self.ivars().popover.get() {
            p.close();
        }
    }

    fn refresh(&self) {
        let items = self.items();
        let shape: Vec<_> = items.iter().map(|i| (i.key, i.status)).collect();
        if *self.ivars().shape.borrow() != shape {
            return self.rebuild();
        }
        for (item, (bar, line)) in items.iter().zip(self.ivars().rows.borrow().iter()) {
            set_bar(bar, item);
            line.setStringValue(&NSString::from_str(&status_text(item)));
        }
    }

    fn rebuild(&self) {
        let mtm = self.mtm();
        let items = self.items();
        let h = items.len().max(1) as f64 * ROW_H + FOOT_H + 8.0;
        let root = NSView::initWithFrame(NSView::alloc(mtm), NSRect::new(NSPoint::ZERO, NSSize::new(W, h)));
        let mut rows = Vec::new();
        if items.is_empty() {
            let empty = label("No downloads", 13.0, &NSColor::secondaryLabelColor(), mtm);
            empty.setFrame(NSRect::new(NSPoint::new(16.0, h - ROW_H + 14.0), NSSize::new(W - 32.0, 20.0)));
            root.addSubview(&empty);
        }
        for (i, item) in items.iter().enumerate() {
            let y = h - 4.0 - (i + 1) as f64 * ROW_H;
            let name = Path::new(&item.path).file_name().map_or(String::new(), |n| n.to_string_lossy().into_owned());
            // The file's own icon once it's there, a document before.
            let image = if item.status == "done" && Path::new(&item.path).exists() {
                Some(NSWorkspace::sharedWorkspace().iconForFile(&NSString::from_str(&item.path)))
            } else {
                NSImage::imageWithSystemSymbolName_accessibilityDescription(&NSString::from_str("doc"), None)
            };
            if let Some(image) = image {
                let icon = NSImageView::imageViewWithImage(&image, mtm);
                icon.setFrame(NSRect::new(NSPoint::new(12.0, y + 8.0), NSSize::new(32.0, 32.0)));
                root.addSubview(&icon);
            }
            let title = unsafe {
                NSButton::buttonWithTitle_target_action(
                    &NSString::from_str(&name),
                    Some(self),
                    Some(sel!(openItem:)),
                    mtm,
                )
            };
            title.setBordered(false);
            title.setTag(i as isize);
            title.setAlignment(objc2_app_kit::NSTextAlignment::Left);
            title.setFont(Some(&NSFont::systemFontOfSize(13.0)));
            title.setLineBreakMode(NSLineBreakMode::ByTruncatingMiddle);
            title.setToolTip(Some(&NSString::from_str(&item.path)));
            title.setFrame(NSRect::new(NSPoint::new(52.0, y + 25.0), NSSize::new(W - 52.0 - 44.0, 18.0)));
            root.addSubview(&title);
            let bar = NSProgressIndicator::initWithFrame(
                NSProgressIndicator::alloc(mtm),
                NSRect::new(NSPoint::new(54.0, y + 14.0), NSSize::new(W - 54.0 - 46.0, 8.0)),
            );
            bar.setStyle(NSProgressIndicatorStyle::Bar);
            bar.setControlSize(NSControlSize::Small);
            set_bar(&bar, item);
            root.addSubview(&bar);
            let line = label(&status_text(item), 11.0, &NSColor::secondaryLabelColor(), mtm);
            let line_y = if item.status == "active" { y + 1.0 } else { y + 8.0 };
            line.setFrame(NSRect::new(NSPoint::new(54.0, line_y), NSSize::new(W - 54.0 - 46.0, 15.0)));
            root.addSubview(&line);
            let (symbol, tip) = match item.status {
                "active" => ("xmark.circle.fill", "Cancel and delete the partial file"),
                _ if Path::new(&item.path).exists() => ("magnifyingglass.circle.fill", "Show in Finder"),
                _ => ("", ""),
            };
            if let Some(image) = (!symbol.is_empty())
                .then(|| {
                    NSImage::imageWithSystemSymbolName_accessibilityDescription(
                        &NSString::from_str(symbol),
                        Some(&NSString::from_str(tip)),
                    )
                })
                .flatten()
            {
                let b = unsafe {
                    NSButton::buttonWithImage_target_action(&image, Some(self), Some(sel!(itemButton:)), mtm)
                };
                b.setBordered(false);
                b.setTag(i as isize);
                b.setToolTip(Some(&NSString::from_str(tip)));
                b.setContentTintColor(Some(&NSColor::secondaryLabelColor()));
                b.setFrame(NSRect::new(NSPoint::new(W - 38.0, y + 12.0), NSSize::new(24.0, 24.0)));
                root.addSubview(&b);
            }
            rows.push((bar, line));
        }
        let line = NSBox::new(mtm);
        line.setBoxType(NSBoxType::Separator);
        line.setFrame(NSRect::new(NSPoint::new(0.0, FOOT_H), NSSize::new(W, 1.0)));
        root.addSubview(&line);
        for (title, action, x) in [("Show All Downloads", sel!(showAll:), 8.0), ("Clear", sel!(clearList:), W - 68.0)] {
            let b = unsafe {
                NSButton::buttonWithTitle_target_action(&NSString::from_str(title), Some(self), Some(action), mtm)
            };
            b.setBordered(false);
            b.setFont(Some(&NSFont::systemFontOfSize(12.0)));
            b.setContentTintColor(Some(&NSColor::controlAccentColor()));
            b.sizeToFit();
            let mut f = b.frame();
            f.origin =
                NSPoint::new(if x > W / 2.0 { W - 12.0 - f.size.width } else { 12.0 }, (FOOT_H - f.size.height) / 2.0);
            b.setFrame(f);
            root.addSubview(&b);
        }
        let controller = NSViewController::new(mtm);
        controller.setView(&root);
        let popover = self.ivars().popover.get().unwrap();
        popover.setContentSize(NSSize::new(W, h));
        popover.setContentViewController(Some(&controller));
        self.ivars().shape.replace(items.iter().map(|i| (i.key, i.status)).collect());
        self.ivars().rows.replace(rows);
    }
}

fn set_bar(bar: &NSProgressIndicator, item: &Item) {
    bar.setHidden(item.status != "active");
    bar.setIndeterminate(item.total <= 0);
    bar.setMaxValue(item.total.max(1) as f64);
    bar.setDoubleValue(item.bytes as f64);
}

fn label(text: &str, size: f64, color: &NSColor, mtm: objc2::MainThreadMarker) -> Retained<NSTextField> {
    let l = NSTextField::labelWithString(&NSString::from_str(text), mtm);
    l.setFont(Some(&NSFont::systemFontOfSize(size)));
    l.setTextColor(Some(color));
    l.setLineBreakMode(NSLineBreakMode::ByTruncatingTail);
    l
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(status: &'static str, bytes: i64, total: i64) -> Item {
        Item { key: 1, path: "/nonexistent/a.zip".into(), status, bytes, total, private: false }
    }

    #[test]
    fn status_lines_and_overall_progress() {
        assert_eq!(status_text(&item("active", 512, 0)), "512 bytes");
        assert_eq!(status_text(&item("active", 3 << 20, 10 << 20)), "3.0 MB of 10.0 MB");
        assert_eq!(status_text(&item("done", 5, 5)), "Moved or deleted");
        assert_eq!(status_text(&item("cancelled", 0, 0)), "Cancelled");
        assert_eq!(status_text(&item("failed", 0, 0)), "Failed");
        assert_eq!(overall(&[item("done", 1, 1)]), None);
        assert_eq!(overall(&[item("active", 1, 4), item("active", 1, 4), item("done", 9, 9)]), Some(0.25));
        assert_eq!(overall(&[item("active", 1, 0)]), Some(0.0), "size not known yet");
        assert_eq!(part_path(Path::new("/d/a.zip")), PathBuf::from("/d/a.zip.download"));
    }
}
