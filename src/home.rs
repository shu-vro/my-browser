//! The new tab page, drawn natively: a banner, a search field, the most
//! visited sites as letter tiles and a status line of numbers the browser
//! already has. A web page here would start a WebKit page process for every
//! new tab (measured: 72.5 MB for the HTML version vs 28.8 MB for a blank
//! tab); these few AppKit views appear instantly and cost little. Plain
//! frames, no Auto Layout or stack views, the system font only: Auto Layout
//! and a second font cost ~2 MB on first use, measured.
//!
//! The memory is in layer bitmaps (footprint's "CoreAnimation"): every view
//! that draws gets one, ~35 KB plus its area. So only the banner and the
//! labels draw; the background, the pills and the tiles' squares are layer
//! properties (colors, gradients, corners), composited without a bitmap.
//! Measured: drawing the tiles in drawRect cost +4.7 MB, one label per tile
//! (letter and name) more than two small ones, cards with three labels each
//! 3x the one-line status.

use std::cell::OnceCell;
use std::ffi::c_void;
use std::ptr::NonNull;

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::{AnyClass, AnyObject};
use objc2::{AnyThread, DefinedClass, MainThreadMarker, MainThreadOnly, Message, define_class, msg_send, sel};
use objc2_app_kit::{
    NSAppearance, NSAppearanceCustomization, NSAppearanceNameDarkAqua, NSAutoresizingMaskOptions, NSBezierPath,
    NSButton, NSColor, NSFocusRingType, NSFont, NSFontAttributeName, NSFontWeightMedium, NSFontWeightSemibold,
    NSForegroundColorAttributeName, NSImage, NSImageView, NSLineBreakMode, NSTextField, NSView,
};
use objc2_foundation::{
    NSArray, NSAttributedString, NSDictionary, NSMutableArray, NSMutableAttributedString, NSNumber, NSPoint, NSRect,
    NSSize, NSString,
};

use crate::App;
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
const WIDTH: f64 = 600.0;
/// A site tile: the colored square and the name under it.
const TILE: (f64, f64) = (104.0, 92.0);
const SQUARE: f64 = 60.0;
/// The banner's ink, left to right, in light and dark.
const INK_LIGHT: [(f64, f64, f64); 2] = [(0.27, 0.42, 0.86), (0.58, 0.36, 0.86)];
const INK_DARK: [(f64, f64, f64); 2] = [(0.50, 0.63, 1.0), (0.78, 0.60, 1.0)];

/// One part of the status line: the number in bold, then the rest.
pub type Stat = (String, String);

/// What the page shows; from the home_* settings, history and open tabs.
pub struct Home {
    pub art: bool,
    pub search: bool,
    /// Most visited sites, as `scheme://host/`.
    pub sites: Vec<String>,
    pub stats: Vec<Stat>,
}

/// The status line, from what the browser knows already: its tabs, its
/// processes' memory (`usage`, read once now; nothing samples in the
/// background) and the shields settings. Nothing is counted for this and
/// nothing leaves the Mac.
pub fn stats(app: &App, this: &Tab) -> Vec<Stat> {
    let tabs = app.all_tabs();
    let asleep = tabs.iter().filter(|t| t.is_asleep()).count();
    // A new tab shows its page before it's listed.
    let open = tabs.len() + !tabs.iter().any(|t| std::ptr::eq(&**t, this)) as usize;
    let mut bytes = crate::usage::of(std::process::id() as i32).map_or(0, |u| u.footprint);
    bytes += crate::usage::webkit_helpers().into_iter().filter_map(crate::usage::of).map(|u| u.footprint).sum::<u64>();
    let store = app.store();
    let (trackers, banners) = (store.flag("block_trackers"), store.flag("hide_cookie_banners"));
    status(open, asleep, bytes, trackers, banners)
}

fn status(open: usize, asleep: usize, bytes: u64, trackers: bool, banners: bool) -> Vec<Stat> {
    let tabs = if open == 1 { "1 tab".to_string() } else { format!("{open} tabs") };
    let shields = match (trackers, banners) {
        (true, true) => "Shields on",
        (true, false) => "Trackers blocked",
        (false, true) => "Cookie banners hidden",
        (false, false) => "Shields off",
    };
    vec![
        (tabs, if asleep == 0 { String::new() } else { format!(", {asleep} asleep") }),
        (format!("{} MB", bytes / (1024 * 1024)), " in use".into()),
        (shields.into(), String::new()),
    ]
}

/// The name a site goes by: `en.wikipedia.org` → `wikipedia`,
/// `bbc.co.uk` → `bbc`, `localhost` → `localhost`.
fn site_name(host: &str) -> &str {
    let host = host.split(':').next().unwrap_or(host);
    if host.parse::<std::net::Ipv4Addr>().is_ok() {
        return host;
    }
    let labels: Vec<&str> = host.split('.').filter(|l| !l.is_empty()).collect();
    match labels.as_slice() {
        [] => host,
        [one] => one,
        // ponytail: guesses two-part suffixes (co.uk, com.au) by length; the public suffix list if it matters.
        [.., name, second, tld] if second.len() <= 3 && tld.len() == 2 && !name.is_empty() => name,
        [.., name, _] => name,
    }
}

/// A hue for a name, the same every time (FNV-1a).
fn hue(name: &str) -> f64 {
    // Picked by hand (blue, indigo, violet, pink, rose, red, orange, amber,
    // green, teal): evenly spread hues bunch up in reds and purples.
    const HUES: [f64; 10] = [0.58, 0.66, 0.75, 0.86, 0.95, 0.01, 0.07, 0.12, 0.38, 0.48];
    let h = name.bytes().fold(0x811c9dc5u32, |h, b| (h ^ b as u32).wrapping_mul(0x01000193));
    HUES[h as usize % HUES.len()]
}

fn is_dark(view: &NSView) -> bool {
    let names = NSArray::from_slice(&[unsafe { NSAppearanceNameDarkAqua }]);
    view.effectiveAppearance().bestMatchFromAppearancesWithNames(&names).is_some()
}

fn rgb(c: (f64, f64, f64), alpha: f64) -> Retained<NSColor> {
    NSColor::colorWithSRGBRed_green_blue_alpha(c.0, c.1, c.2, alpha)
}

/// A color with a light and a dark variant, following the window's look.
fn adaptive(light: (f64, f64, f64), dark: (f64, f64, f64)) -> Retained<NSColor> {
    let block = RcBlock::new(move |appearance: NonNull<NSAppearance>| -> NonNull<NSColor> {
        let names = NSArray::from_slice(&[unsafe { NSAppearanceNameDarkAqua }]);
        let is_dark = unsafe { appearance.as_ref() }.bestMatchFromAppearancesWithNames(&names).is_some();
        let color = rgb(if is_dark { dark } else { light }, 1.0);
        // Handed to AppKit, which retains it before the autorelease pool drains.
        NonNull::new(Retained::autorelease_return(color)).unwrap()
    });
    unsafe { NSColor::colorWithName_dynamicProvider(None, &block) }
}

/// `color`'s CGColor, valid while `color` lives.
fn cg(color: &NSColor) -> *const c_void {
    unsafe { msg_send![color, CGColor] }
}

/// Sets a layer's colors (`backgroundColor`, `borderColor`).
fn set_layer_color(layer: &AnyObject, border: bool, color: &NSColor) {
    let c = cg(color);
    unsafe {
        if border {
            let _: () = msg_send![layer, setBorderColor: c];
        } else {
            let _: () = msg_send![layer, setBackgroundColor: c];
        }
    }
}

/// A CAGradientLayer's colors, top to bottom (or center out when radial).
fn set_gradient(layer: &AnyObject, colors: &[Retained<NSColor>]) {
    let array = NSMutableArray::<AnyObject>::new();
    for c in colors {
        // CGColor is a CF type, bridged: the array retains it.
        array.addObject(unsafe { &*(cg(c) as *const AnyObject) });
    }
    let _: () = unsafe { msg_send![layer, setColors: &*array] };
}

fn gradient_layer() -> Retained<AnyObject> {
    let class = AnyClass::get(c"CAGradientLayer").expect("QuartzCore");
    unsafe { msg_send![class, layer] }
}

define_class!(
    /// The page itself: a soft radial glow from the top, as a gradient layer
    /// (drawn by the window server, no bitmap of the page's size).
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    struct Backdrop;

    impl Backdrop {
        #[unsafe(method_id(makeBackingLayer))]
        fn make_backing_layer(&self) -> Retained<AnyObject> {
            let layer = gradient_layer();
            unsafe {
                let _: () = msg_send![&*layer, setType: &*NSString::from_str("radial")];
                let _: () = msg_send![&*layer, setStartPoint: NSPoint::new(0.5, 1.0)];
                let _: () = msg_send![&*layer, setEndPoint: NSPoint::new(1.15, -0.1)];
                let stops = NSArray::from_retained_slice(&[NSNumber::new_f64(0.0), NSNumber::new_f64(0.55), NSNumber::new_f64(1.0)]);
                let _: () = msg_send![&*layer, setLocations: &*stops];
            }
            layer
        }

        // A gradient layer never asks its view for updateLayer, so the
        // colors are set whenever the look can change.
        #[unsafe(method(viewDidMoveToWindow))]
        fn did_move_to_window(&self) {
            self.set_colors();
        }

        #[unsafe(method(viewDidChangeEffectiveAppearance))]
        fn did_change_appearance(&self) {
            self.set_colors();
        }
    }
);

impl Backdrop {
    fn set_colors(&self) {
        let Some(layer) = self.layer() else { return };
        let colors = if is_dark(self) {
            [rgb((0.16, 0.16, 0.29), 1.0), rgb((0.11, 0.11, 0.15), 1.0), rgb((0.09, 0.09, 0.11), 1.0)]
        } else {
            [rgb((0.87, 0.89, 1.0), 1.0), rgb((0.955, 0.95, 0.99), 1.0), rgb((0.985, 0.985, 0.99), 1.0)]
        };
        set_gradient(&layer, &colors);
    }
}

define_class!(
    /// A rounded, translucent panel (the search pill, the status line):
    /// layer colors only. `ivars`: corner radius.
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[ivars = f64]
    struct Plate;

    impl Plate {
        #[unsafe(method(wantsUpdateLayer))]
        fn wants_update_layer(&self) -> bool {
            true
        }

        #[unsafe(method(updateLayer))]
        fn update_layer(&self) {
            let Some(layer) = self.layer() else { return };
            let (fill, edge) = if is_dark(self) {
                (rgb((1.0, 1.0, 1.0), 0.07), rgb((1.0, 1.0, 1.0), 0.09))
            } else {
                (rgb((1.0, 1.0, 1.0), 0.78), rgb((0.10, 0.12, 0.30), 0.08))
            };
            set_layer_color(&layer, false, &fill);
            set_layer_color(&layer, true, &edge);
            unsafe {
                let _: () = msg_send![&*layer, setCornerRadius: *self.ivars()];
                let _: () = msg_send![&*layer, setBorderWidth: 1.0f64];
            }
        }
    }
);

impl Plate {
    fn new(size: NSSize, radius: f64, mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(radius);
        let plate: Retained<Self> =
            unsafe { msg_send![super(this), initWithFrame: NSRect::new(NSPoint::new(0.0, 0.0), size)] };
        plate.setWantsLayer(true);
        plate
    }
}

define_class!(
    /// A most visited site: a gradient square with the site's initial and its
    /// name under it. A button (keyboard, VoiceOver, target/action) that draws
    /// nothing itself: the square is a gradient sublayer, the text two labels.
    /// `ivars`: the site's hue, and the square once made.
    #[unsafe(super(NSButton))]
    #[thread_kind = MainThreadOnly]
    #[ivars = (f64, OnceCell<Retained<AnyObject>>)]
    struct Tile;

    impl Tile {
        #[unsafe(method(wantsUpdateLayer))]
        fn wants_update_layer(&self) -> bool {
            true
        }

        #[unsafe(method(updateLayer))]
        fn update_layer(&self) {
            let Some(layer) = self.layer() else { return };
            let square = self.ivars().1.get_or_init(|| {
                let s = gradient_layer();
                let x = ((TILE.0 - SQUARE) / 2.0).round();
                // Buttons are flipped: y = 0 is the top.
                let rect = NSRect::new(NSPoint::new(x, 0.0), NSSize::new(SQUARE, SQUARE));
                unsafe {
                    let _: () = msg_send![&*s, setFrame: rect];
                    let _: () = msg_send![&*s, setCornerRadius: 16.0f64];
                    let _: () = msg_send![&*s, setCornerCurve: &*NSString::from_str("continuous")];
                    let _: () = msg_send![&*layer, insertSublayer: &*s, atIndex: 0u32];
                }
                s
            });
            let h = self.ivars().0;
            let pressed: bool = unsafe { msg_send![self, isHighlighted] };
            let (s, b) = if is_dark(self) { (0.55, 0.72) } else { (0.48, 0.90) };
            let b = if pressed { b - 0.15 } else { b };
            let top = NSColor::colorWithHue_saturation_brightness_alpha(h, s, b, 1.0);
            let bottom = NSColor::colorWithHue_saturation_brightness_alpha((h + 0.06) % 1.0, s + 0.15, b - 0.18, 1.0);
            set_gradient(square, &[top, bottom]);
        }

        /// The labels inside don't take clicks: the tile does.
        #[unsafe(method(hitTest:))]
        fn hit_test(&self, point: NSPoint) -> *mut NSView {
            let f = self.frame();
            let inside = point.x >= f.origin.x && point.x < f.origin.x + f.size.width && point.y >= f.origin.y && point.y < f.origin.y + f.size.height;
            if inside { self as *const Self as *mut NSView } else { std::ptr::null_mut() }
        }
    }
);

impl Tile {
    fn new(url: &str, tab: &Tab, mtm: MainThreadMarker) -> Retained<Self> {
        let host = url.split("://").nth(1).unwrap_or(url).trim_end_matches('/');
        let host = host.strip_prefix("www.").unwrap_or(host);
        let name = site_name(host);
        let this = Self::alloc(mtm).set_ivars((hue(name), OnceCell::new()));
        let frame = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(TILE.0, TILE.1));
        let tile: Retained<Self> = unsafe { msg_send![super(this), initWithFrame: frame] };
        tile.setBordered(false);
        // No title (a button draws its title even with updateLayer); VoiceOver
        // reads the label instead.
        tile.setTitle(&NSString::from_str(""));
        let _: () = unsafe { msg_send![&*tile, setAccessibilityLabel: &*NSString::from_str(host)] };
        unsafe {
            tile.setTarget(Some(tab));
            tile.setAction(Some(sel!(homeSite:)));
        }
        // The address to open travels in the tooltip, which also shows it.
        tile.setToolTip(Some(&NSString::from_str(url)));
        tile.setWantsLayer(true);

        let initial: String = name.chars().next().map(|c| c.to_uppercase().collect()).unwrap_or_default();
        // Two labels, each as small as its text: a label's bitmap grows with
        // its frame (one label for both was measured larger).
        let font = NSFont::systemFontOfSize_weight(26.0, unsafe { NSFontWeightSemibold });
        let letter = label(&initial, &font, &NSColor::whiteColor(), mtm);
        let size = letter.frame().size;
        letter.setFrameOrigin(NSPoint::new(
            ((TILE.0 - size.width) / 2.0).round(),
            ((SQUARE - size.height) / 2.0).round(),
        ));
        tile.addSubview(&letter);
        let caption = label(host, &NSFont::systemFontOfSize(11.5), &NSColor::secondaryLabelColor(), mtm);
        caption.setLineBreakMode(NSLineBreakMode::ByTruncatingTail);
        let size = caption.frame().size;
        let w = size.width.min(TILE.0);
        caption.setFrame(NSRect::new(
            NSPoint::new(((TILE.0 - w) / 2.0).round(), SQUARE + 8.0),
            NSSize::new(w, size.height),
        ));
        tile.addSubview(&caption);
        tile
    }
}

define_class!(
    /// The banner: each █ of `ART` is a filled cell, its color sweeping left
    /// to right, over a faint offset echo. The page's only drawn view.
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
            let [from, to] = if is_dark(self) { INK_DARK } else { INK_LIGHT };
            let cols = ART.lines().map(|l| l.chars().count()).max().unwrap_or(1) as f64;
            for (alpha, offset) in [(0.22, ECHO), (1.0, 0.0)] {
                for (row, line) in ART.lines().enumerate() {
                    for (col, c) in line.chars().enumerate() {
                        if c != ' ' {
                            let t = col as f64 / (cols - 1.0);
                            let mix = |a: f64, b: f64| a + (b - a) * t;
                            rgb((mix(from.0, to.0), mix(from.1, to.1), mix(from.2, to.2)), alpha).setFill();
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

fn view(v: &NSView) -> Retained<NSView> {
    v.retain()
}

fn label(text: &str, font: &NSFont, color: &NSColor, mtm: MainThreadMarker) -> Retained<NSTextField> {
    let l = NSTextField::labelWithString(&NSString::from_str(text), mtm);
    l.setFont(Some(font));
    l.setTextColor(Some(color));
    l.setSelectable(false);
    l.sizeToFit();
    l
}

/// Views laid out side by side, `gap` apart, in a view of their size.
fn row(views: &[Retained<NSView>], gap: f64, mtm: MainThreadMarker) -> Retained<NSView> {
    let row = NSView::new(mtm);
    let (mut x, mut h) = (0.0, 0.0f64);
    for v in views {
        v.setFrameOrigin(NSPoint::new(x, 0.0));
        row.addSubview(v);
        x += v.frame().size.width + gap;
        h = h.max(v.frame().size.height);
    }
    row.setFrame(NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new((x - gap).max(0.0), h)));
    row
}

/// The status line: one label (one bitmap) on a pill, the numbers in bold.
fn status_line(stats: &[Stat], mtm: MainThreadMarker) -> Retained<NSView> {
    let all = NSMutableAttributedString::new();
    let strong = NSFont::systemFontOfSize_weight(12.0, unsafe { NSFontWeightSemibold });
    let plain = NSFont::systemFontOfSize(12.0);
    for (i, (bold, rest)) in stats.iter().enumerate() {
        let sep = if i == 0 { "" } else { "   \u{b7}   " };
        for (text, font, color) in [
            (sep, &plain, NSColor::tertiaryLabelColor()),
            (bold.as_str(), &strong, NSColor::labelColor()),
            (rest.as_str(), &plain, NSColor::secondaryLabelColor()),
        ] {
            let any = |o: &AnyObject| o.retain();
            let attrs = NSDictionary::from_retained_objects(
                &unsafe { [NSFontAttributeName, NSForegroundColorAttributeName] },
                &[any(font), any(&color)],
            );
            let part = unsafe {
                NSAttributedString::initWithString_attributes(
                    NSAttributedString::alloc(),
                    &NSString::from_str(text),
                    Some(&attrs),
                )
            };
            all.appendAttributedString(&part);
        }
    }
    let text = NSTextField::labelWithAttributedString(&all, mtm);
    text.setSelectable(false);
    text.sizeToFit();
    let size = text.frame().size;
    let plate = Plate::new(NSSize::new(size.width + 36.0, size.height + 14.0), (size.height + 14.0) / 2.0, mtm);
    text.setFrameOrigin(NSPoint::new(18.0, 7.0));
    plate.addSubview(&text);
    plate.setToolTip(Some(&NSString::from_str(
        "Read on this Mac when the page opened. Asleep tabs use no memory. Memory counts the browser and its WebKit processes. Shields: Settings \u{2192} Privacy.",
    )));
    view(&plate)
}

/// The search pill: a magnifier and a borderless field on a rounded plate.
fn search_field(tab: &Tab, mtm: MainThreadMarker) -> Retained<NSView> {
    let (w, h) = (WIDTH - 40.0, 46.0);
    let plate = Plate::new(NSSize::new(w, h), h / 2.0, mtm);
    if let Some(image) =
        NSImage::imageWithSystemSymbolName_accessibilityDescription(&NSString::from_str("magnifyingglass"), None)
    {
        let icon = NSImageView::imageViewWithImage(&image, mtm);
        icon.setContentTintColor(Some(&NSColor::secondaryLabelColor()));
        icon.setFrame(NSRect::new(NSPoint::new(18.0, (h - 18.0) / 2.0), NSSize::new(18.0, 18.0)));
        plate.addSubview(&icon);
    }
    let field = NSTextField::textFieldWithString(&NSString::from_str(""), mtm);
    field.setPlaceholderString(Some(&NSString::from_str("Search or enter address")));
    field.setFont(Some(&NSFont::systemFontOfSize(16.0)));
    field.setBezeled(false);
    field.setBordered(false);
    field.setDrawsBackground(false);
    field.setFocusRingType(NSFocusRingType::None);
    field.sizeToFit();
    let fh = field.frame().size.height;
    field.setFrame(NSRect::new(NSPoint::new(46.0, ((h - fh) / 2.0).round()), NSSize::new(w - 46.0 - 20.0, fh)));
    unsafe {
        field.setTarget(Some(tab));
        field.setAction(Some(sel!(homeSearch:)));
    }
    plate.addSubview(&field);
    view(&plate)
}

/// Build the page for a `frame`-sized area; `tab` receives the actions
/// (homeSearch:, homeSite:, openHomeSettings:).
pub fn build(tab: &Tab, home: &Home, frame: NSRect, mtm: MainThreadMarker) -> Retained<NSView> {
    let page: Retained<Backdrop> = unsafe { msg_send![Backdrop::alloc(mtm), initWithFrame: frame] };
    page.setWantsLayer(true);
    // Top to bottom: each view and the space under it.
    let mut items: Vec<(Retained<NSView>, f64)> = Vec::new();

    if home.art {
        items.push((view(&Banner::new(mtm)), 12.0));
        let by = label(
            "s  h  u  -  v  r  o",
            &NSFont::systemFontOfSize_weight(12.0, unsafe { NSFontWeightMedium }),
            &adaptive(INK_LIGHT[0], INK_DARK[0]),
            mtm,
        );
        items.push((view(&by), 34.0));
    }

    if home.search {
        items.push((search_field(tab, mtm), 34.0));
    }

    // Most visited: rows of four.
    for chunk in home.sites.chunks(4) {
        let tiles: Vec<Retained<NSView>> = chunk.iter().map(|url| view(&Tile::new(url, tab, mtm))).collect();
        items.push((row(&tiles, 8.0, mtm), 10.0));
    }
    if let Some(last) = items.last_mut().filter(|_| !home.sites.is_empty()) {
        last.1 = 28.0;
    }

    if !home.stats.is_empty() {
        items.push((status_line(&home.stats, mtm), 20.0));
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
    items.push((view(&customize), 0.0));

    // One column, centered across and, as far as it fits, down the page.
    let height: f64 = items.iter().map(|(v, gap)| v.frame().size.height + gap).sum();
    let column = NSView::new(mtm);
    let mut y = height;
    for (v, gap) in &items {
        let size = v.frame().size;
        y -= size.height;
        v.setFrameOrigin(NSPoint::new(((WIDTH - size.width.min(WIDTH)) / 2.0).round(), y.round()));
        column.addSubview(v);
        y -= gap;
    }
    let top = ((frame.size.height - height) / 2.0 - 20.0).max(40.0);
    column.setFrame(NSRect::new(
        NSPoint::new(((frame.size.width - WIDTH) / 2.0).round(), (frame.size.height - top - height).round()),
        NSSize::new(WIDTH, height),
    ));
    // Stays centered as the window resizes.
    column.setAutoresizingMask(
        NSAutoresizingMaskOptions::ViewMinXMargin
            | NSAutoresizingMaskOptions::ViewMaxXMargin
            | NSAutoresizingMaskOptions::ViewMinYMargin
            | NSAutoresizingMaskOptions::ViewMaxYMargin,
    );
    page.addSubview(&column);
    view(&page)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn site_names_and_colors() {
        assert_eq!(site_name("en.wikipedia.org"), "wikipedia");
        assert_eq!(site_name("news.ycombinator.com"), "ycombinator");
        assert_eq!(site_name("bbc.co.uk"), "bbc");
        assert_eq!(site_name("docs.rs"), "docs");
        assert_eq!(site_name("localhost:3000"), "localhost");
        assert_eq!(site_name("127.0.0.1:8080"), "127.0.0.1");
        assert_eq!(hue("google"), hue("google"), "same site, same color");
        let names = ["github", "youtube", "wikipedia", "ycombinator", "google", "reddit", "docs", "apple"];
        let mut hues: Vec<u32> = names.iter().map(|n| (hue(n) * 100.0) as u32).collect();
        hues.dedup();
        assert!(hues.len() >= 5, "sites get different colors: {hues:?}");
    }

    #[test]
    fn status_says_what_it_counts() {
        let s = status(5, 2, 312 * 1024 * 1024, true, false);
        assert_eq!(s[0], ("5 tabs".to_string(), ", 2 asleep".to_string()));
        assert_eq!(s[1].0, "312 MB");
        assert_eq!(s[2].0, "Trackers blocked");
        let s = status(1, 0, 0, false, false);
        assert_eq!(s[0], ("1 tab".to_string(), String::new()));
        assert_eq!(s[2].0, "Shields off");
    }
}
