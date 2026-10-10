//! The new tab page, drawn natively: a banner, a search field and the most
//! visited sites. A web page here would start a WebKit page process for every
//! new tab (measured: 72.5 MB for the HTML version vs 28.8 MB for a blank tab);
//! these few AppKit views appear instantly and cost little. Plain frames, no
//! Auto Layout or stack views, and the banner is drawn as rectangles rather
//! than text: Auto Layout and a second font cost ~2 MB on first use, measured.

use std::ptr::NonNull;

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::sel;
use objc2::{MainThreadMarker, MainThreadOnly, Message, define_class, msg_send};
use objc2_app_kit::{
    NSAppearance, NSAppearanceNameDarkAqua, NSAutoresizingMaskOptions, NSBezierPath, NSButton, NSColor, NSFont,
    NSTextField, NSTextFieldBezelStyle, NSView,
};
use objc2_foundation::{NSArray, NSPoint, NSRect, NSSize, NSString};

use crate::tab::Tab;

const ART: &str = "\
█████ █   █ █████ ████  █████ █   █ █████ █   █
█     █   █   █   █   █ █     █   █ █     ██  █
█████ █████   █   ████  █████ █████ ████  █ █ █
    █ █   █   █   █  █      █ █   █ █     █  ██
█████ █   █ █████ █   █ █████ █   █ █████ █   █";

/// One block of the banner, in points.
const CELL: (f64, f64) = (8.0, 12.0);
/// How far the echo sits behind the banner.
const ECHO: f64 = 4.0;
const WIDTH: f64 = 560.0;
const GAP: f64 = 22.0;

/// What the page shows; from the home_* settings and history.
pub struct Home {
    pub art: bool,
    pub search: bool,
    /// Most visited sites, as `scheme://host/`.
    pub sites: Vec<String>,
}

/// A color with a light and a dark variant, following the window's look.
fn adaptive(light: (f64, f64, f64), dark: (f64, f64, f64)) -> Retained<NSColor> {
    let block = RcBlock::new(move |appearance: NonNull<NSAppearance>| -> NonNull<NSColor> {
        let names = NSArray::from_slice(&[unsafe { NSAppearanceNameDarkAqua }]);
        let is_dark = unsafe { appearance.as_ref() }.bestMatchFromAppearancesWithNames(&names).is_some();
        let (r, g, b) = if is_dark { dark } else { light };
        let color = NSColor::colorWithSRGBRed_green_blue_alpha(r, g, b, 1.0);
        // Handed to AppKit, which retains it before the autorelease pool drains.
        NonNull::new(Retained::autorelease_return(color)).unwrap()
    });
    unsafe { NSColor::colorWithName_dynamicProvider(None, &block) }
}

define_class!(
    /// The banner: each █ of `ART` is a filled cell, over an offset echo.
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    struct Banner;

    impl Banner {
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            true
        }

        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, _dirty: NSRect) {
            // Dynamic colors resolve against the view's appearance while drawing.
            for (ink, offset) in [(adaptive((0.70, 0.79, 0.85), (0.27, 0.36, 0.42)), ECHO), (adaptive((0.31, 0.50, 0.61), (0.53, 0.68, 0.76)), 0.0)] {
                ink.setFill();
                for (row, line) in ART.lines().enumerate() {
                    for (col, c) in line.chars().enumerate() {
                        if c != ' ' {
                            let origin = NSPoint::new(col as f64 * CELL.0 + offset, row as f64 * CELL.1 + offset);
                            NSBezierPath::fillRect(NSRect::new(origin, NSSize::new(CELL.0, CELL.1)));
                        }
                    }
                }
            }
        }
    }
);

impl Banner {
    fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let cols = ART.lines().map(|l| l.chars().count()).max().unwrap_or(0) as f64;
        let rows = ART.lines().count() as f64;
        let frame = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(cols * CELL.0 + ECHO, rows * CELL.1 + ECHO));
        unsafe { msg_send![Self::alloc(mtm), initWithFrame: frame] }
    }
}

fn label(text: &str, font: &NSFont, color: &NSColor, mtm: MainThreadMarker) -> Retained<NSTextField> {
    let l = NSTextField::labelWithString(&NSString::from_str(text), mtm);
    l.setFont(Some(font));
    l.setTextColor(Some(color));
    l.setSelectable(false);
    l.sizeToFit();
    l
}

/// Lays views out top to bottom, each centered in the column.
struct Column {
    view: Retained<NSView>,
    /// Views with their height, top to bottom.
    items: Vec<(Retained<NSView>, f64)>,
}

impl Column {
    fn push(&mut self, view: &NSView, height: f64) {
        self.items.push((view.retain(), height));
    }

    /// Fix the frames; the column's own height is the sum.
    fn finish(self, top: f64, container_height: f64) -> Retained<NSView> {
        let height: f64 = self.items.iter().map(|(_, h)| h + GAP).sum::<f64>() - GAP;
        let mut y = height;
        for (v, h) in &self.items {
            y -= h;
            let w = v.frame().size.width.min(WIDTH);
            v.setFrameOrigin(NSPoint::new(((WIDTH - w) / 2.0).round(), y));
            self.view.addSubview(v);
            y -= GAP;
        }
        self.view.setFrame(NSRect::new(NSPoint::new(0.0, container_height - top - height), NSSize::new(WIDTH, height)));
        self.view
    }
}

/// Build the page for a `frame`-sized area; `tab` receives the actions
/// (homeSearch:, homeSite:, openHomeSettings:).
pub fn build(tab: &Tab, home: &Home, frame: NSRect, mtm: MainThreadMarker) -> Retained<NSView> {
    let container = NSView::initWithFrame(NSView::alloc(mtm), frame);
    let mut col = Column { view: NSView::new(mtm), items: Vec::new() };

    if home.art {
        let banner = Banner::new(mtm);
        let h = banner.frame().size.height;
        col.push(&banner, h);
        let by = label(
            "s  h  u  -  v  r  o",
            &NSFont::boldSystemFontOfSize(13.0),
            &adaptive((0.31, 0.50, 0.61), (0.53, 0.68, 0.76)),
            mtm,
        );
        col.push(&by, by.frame().size.height);
    }

    if home.search {
        let field = NSTextField::textFieldWithString(&NSString::from_str(""), mtm);
        field.setPlaceholderString(Some(&NSString::from_str("Search or enter address")));
        field.setFont(Some(&NSFont::systemFontOfSize(15.0)));
        field.setBezelStyle(NSTextFieldBezelStyle::RoundedBezel);
        field.setFrame(NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(WIDTH - 40.0, 30.0)));
        unsafe {
            field.setTarget(Some(tab));
            field.setAction(Some(sel!(homeSearch:)));
        }
        col.push(&field, 30.0);
    }

    // Most visited: rows of four, each row centered.
    for chunk in home.sites.chunks(4) {
        let row = NSView::new(mtm);
        let mut x = 0.0;
        let mut h: f64 = 0.0;
        for url in chunk {
            let host = url.split("://").nth(1).unwrap_or(url).trim_end_matches('/');
            let host = host.strip_prefix("www.").unwrap_or(host);
            let b = unsafe {
                NSButton::buttonWithTitle_target_action(
                    &NSString::from_str(host),
                    Some(tab),
                    Some(sel!(homeSite:)),
                    mtm,
                )
            };
            // The address to open travels in the tooltip, which also shows it.
            b.setToolTip(Some(&NSString::from_str(url)));
            b.sizeToFit();
            let size = b.frame().size;
            let w = size.width.min(160.0);
            b.setFrame(NSRect::new(NSPoint::new(x, 0.0), NSSize::new(w, size.height)));
            row.addSubview(&b);
            x += w + 10.0;
            h = h.max(size.height);
        }
        row.setFrame(NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(x - 10.0, h)));
        col.push(&row, h);
    }

    let customize = unsafe {
        NSButton::buttonWithTitle_target_action(
            &NSString::from_str("Customize"),
            Some(tab),
            Some(sel!(openHomeSettings:)),
            mtm,
        )
    };
    customize.setBordered(false);
    customize.setContentTintColor(Some(&NSColor::secondaryLabelColor()));
    customize.sizeToFit();
    col.push(&customize, customize.frame().size.height);

    let column = col.finish(90.0, frame.size.height);
    let mut f = column.frame();
    f.origin.x = ((frame.size.width - WIDTH) / 2.0).round();
    column.setFrame(f);
    // Stays centered and at the same distance from the top as the window resizes.
    column.setAutoresizingMask(
        NSAutoresizingMaskOptions::ViewMinXMargin
            | NSAutoresizingMaskOptions::ViewMaxXMargin
            | NSAutoresizingMaskOptions::ViewMinYMargin,
    );
    container.addSubview(&column);
    container
}
