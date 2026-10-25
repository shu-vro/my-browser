//! The PDF viewer: a PDF opened in a tab shows in Apple's PDFKit with a
//! toolbar like Edge's: page thumbnails, page number, zoom (fit width, fit
//! page), rotate, one or two pages side by side, highlight, read aloud, find
//! (⌘F, every match highlighted), print and save (with the highlights).
//!
//! WebKit still loads the PDF (so the address, history and back/forward stay
//! as for any page); when it has, the viewer covers the page with the same
//! file (`_getMainResourceDataWithCompletionHandler:`, private, `TUNE pdf_data`).
//! PDFKit is loaded on the first PDF, so it costs nothing until then.
// ponytail: the PDF is in memory twice (WebKit's page process and here);
// fine for documents, revisit if huge PDFs show up in the benchmark.

use objc2::rc::Retained;
use objc2::runtime::{AnyClass, AnyObject, Bool};
use objc2::{MainThreadMarker, MainThreadOnly, msg_send, sel};
use objc2_app_kit::{NSAutoresizingMaskOptions, NSButton, NSTextField, NSView};
use objc2_foundation::{NSArray, NSData, NSEdgeInsets, NSPoint, NSRect, NSSize, NSString};

use crate::tab::Tab;

const BAR_H: f64 = 32.0;
const THUMBS_W: f64 = 150.0;

/// Toolbar buttons: (SF Symbol, tooltip, command).
pub const BUTTONS: [(&str, &str, Command); 14] = [
    ("sidebar.left", "Page thumbnails", Command::Sidebar),
    ("chevron.up", "Previous page", Command::Previous),
    ("chevron.down", "Next page", Command::Next),
    ("minus.magnifyingglass", "Zoom out (⌘-)", Command::ZoomOut),
    ("plus.magnifyingglass", "Zoom in (⌘+)", Command::ZoomIn),
    ("arrow.left.and.right", "Fit width", Command::FitWidth),
    ("arrow.up.and.down", "Fit page", Command::FitPage),
    ("rotate.right", "Rotate", Command::Rotate),
    ("book", "Two pages side by side", Command::TwoPages),
    ("highlighter", "Highlight the selected text", Command::Highlight),
    ("speaker.wave.2", "Read aloud (from the selection, or this page)", Command::ReadAloud),
    ("magnifyingglass", "Find (⌘F)", Command::Find),
    ("printer", "Print (⌘P)", Command::Print),
    ("square.and.arrow.down", "Save, with highlights (⌘S)", Command::Save),
];

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Command {
    Sidebar,
    Previous,
    Next,
    ZoomOut,
    ZoomIn,
    FitWidth,
    FitPage,
    Rotate,
    TwoPages,
    Highlight,
    ReadAloud,
    Find,
    Print,
    Save,
}

/// An open PDF and its viewer's parts.
pub struct Viewer {
    pub root: Retained<NSView>,
    view: Retained<AnyObject>,
    document: Retained<AnyObject>,
    thumbs: Retained<NSView>,
    page_field: Retained<NSTextField>,
    pages_label: Retained<NSTextField>,
    zoom_label: Retained<NSTextField>,
    /// File name for Save.
    pub name: String,
    /// Find: the term, its matches, the current one.
    found: (String, Vec<Retained<AnyObject>>, usize),
    speech: Option<Retained<AnyObject>>,
    /// What's being read aloud, in pieces: (where the piece starts in the
    /// spoken text, its page, where it starts in the page's text), in UTF-16
    /// units like NSString's. And the word being read now.
    spoken: Vec<(usize, usize, usize)>,
    word: String,
    highlights: usize,
}

/// PDFKit, loaded on first use.
fn class(name: &std::ffi::CStr) -> Option<&'static AnyClass> {
    if AnyClass::get(name).is_none() {
        let bundle = objc2_foundation::NSBundle::bundleWithPath(&NSString::from_str(
            "/System/Library/Frameworks/PDFKit.framework",
        ))?;
        if !unsafe { bundle.load() } {
            return None;
        }
    }
    AnyClass::get(name)
}

impl Viewer {
    /// The viewer for a PDF's bytes, filling `frame` of the tab's window.
    pub fn new(tab: &Tab, data: &NSData, frame: NSRect, name: String, mtm: MainThreadMarker) -> Option<Viewer> {
        let document: Option<Retained<AnyObject>> =
            unsafe { msg_send![msg_send![class(c"PDFDocument")?, alloc], initWithData: data] };
        let document = document?;
        let root = NSView::initWithFrame(NSView::alloc(mtm), frame);
        root.setAutoresizingMask(
            NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewHeightSizable,
        );
        let (w, h) = (frame.size.width, frame.size.height);

        // The PDF itself, and thumbnails to its left (hidden at first).
        let area = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(w, h - BAR_H));
        let view: Retained<AnyObject> = unsafe { msg_send![msg_send![class(c"PDFView")?, alloc], initWithFrame: area] };
        let thumbs: Retained<NSView> = unsafe {
            msg_send![msg_send![class(c"PDFThumbnailView")?, alloc], initWithFrame: NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(THUMBS_W, h - BAR_H))]
        };
        // PDFView draws past its edges unless clipped (AppKit stopped clipping
        // by default in macOS 14): it showed through the thumbnails.
        root.setClipsToBounds(true);
        unsafe {
            let _: () = msg_send![&*view, setDocument: &*document];
            let _: () = msg_send![&*view, setAutoScales: Bool::YES];
            // kPDFDisplaySinglePageContinuous
            let _: () = msg_send![&*view, setDisplayMode: 1isize];
            let _: () = msg_send![&*view, setAutoresizingMask: NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewHeightSizable];
            let _: () = msg_send![&*thumbs, setPDFView: &*view];
            let _: () = msg_send![&*view, setClipsToBounds: Bool::YES];
            let _: () = msg_send![&*thumbs, setThumbnailSize: NSSize::new(110.0, 140.0)];
            let _: () = msg_send![&*thumbs, setBackgroundColor: &*objc2_app_kit::NSColor::windowBackgroundColor()];
            thumbs.setClipsToBounds(true);
            thumbs.setAutoresizingMask(NSAutoresizingMaskOptions::ViewHeightSizable);
            thumbs.setHidden(true);
            root.addSubview(&*(Retained::as_ptr(&view) as *const NSView));
        }
        root.addSubview(&thumbs);

        // The toolbar.
        let bar =
            NSView::initWithFrame(NSView::alloc(mtm), NSRect::new(NSPoint::new(0.0, h - BAR_H), NSSize::new(w, BAR_H)));
        bar.setAutoresizingMask(
            NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewMinYMargin,
        );
        let mut x = 6.0;
        let page_field = NSTextField::textFieldWithString(&NSString::from_str("1"), mtm);
        let pages_label = NSTextField::labelWithString(&NSString::from_str(""), mtm);
        let zoom_label = NSTextField::labelWithString(&NSString::from_str(""), mtm);
        for (i, (symbol, tip, command)) in BUTTONS.iter().enumerate() {
            let image = objc2_app_kit::NSImage::imageWithSystemSymbolName_accessibilityDescription(
                &NSString::from_str(symbol),
                Some(&NSString::from_str(tip)),
            )?;
            let b = unsafe { NSButton::buttonWithImage_target_action(&image, Some(tab), Some(sel!(pdfCommand:)), mtm) };
            b.setBordered(false);
            b.setTag(i as isize);
            b.setToolTip(Some(&NSString::from_str(tip)));
            b.setFrame(NSRect::new(NSPoint::new(x, 2.0), NSSize::new(28.0, 28.0)));
            bar.addSubview(&b);
            x += 28.0;
            // After Next: the page number; after Zoom In: the zoom.
            if *command == Command::Next {
                page_field.setFrame(NSRect::new(NSPoint::new(x + 4.0, 6.0), NSSize::new(40.0, 20.0)));
                page_field.setAlignment(objc2_app_kit::NSTextAlignment::Right);
                unsafe {
                    page_field.setTarget(Some(tab));
                    page_field.setAction(Some(sel!(pdfGoToPage:)));
                }
                pages_label.setFrame(NSRect::new(NSPoint::new(x + 48.0, 8.0), NSSize::new(56.0, 16.0)));
                bar.addSubview(&page_field);
                bar.addSubview(&pages_label);
                x += 108.0;
            } else if *command == Command::ZoomIn {
                zoom_label.setFrame(NSRect::new(NSPoint::new(x + 2.0, 8.0), NSSize::new(46.0, 16.0)));
                bar.addSubview(&zoom_label);
                x += 48.0;
            } else if matches!(command, Command::TwoPages | Command::ReadAloud) {
                x += 10.0;
            }
        }
        for l in [&pages_label, &zoom_label] {
            l.setTextColor(Some(&objc2_app_kit::NSColor::secondaryLabelColor()));
        }
        root.addSubview(&bar);

        // Page and zoom changes update the toolbar.
        let center = objc2_foundation::NSNotificationCenter::defaultCenter();
        for name in ["PDFViewPageChangedNotification", "PDFViewScaleChangedNotification"] {
            unsafe {
                center.addObserver_selector_name_object(
                    tab,
                    sel!(pdfChanged:),
                    Some(&NSString::from_str(name)),
                    Some(&*view),
                )
            };
        }
        let viewer = Viewer {
            root,
            view,
            document,
            thumbs,
            page_field,
            pages_label,
            zoom_label,
            name,
            found: (String::new(), Vec::new(), 0),
            speech: None,
            spoken: Vec::new(),
            word: String::new(),
            highlights: 0,
        };
        viewer.update();
        Some(viewer)
    }

    /// Stop observing and speaking; take the viewer off the window.
    pub fn close(&mut self, tab: &Tab) {
        let center = objc2_foundation::NSNotificationCenter::defaultCenter();
        unsafe { center.removeObserver(tab) };
        if let Some(s) = self.speech.take() {
            let _: () = unsafe { msg_send![&*s, stopSpeaking] };
        }
        self.root.removeFromSuperview();
    }

    fn pages(&self) -> isize {
        unsafe { msg_send![&*self.document, pageCount] }
    }

    fn page_index(&self) -> isize {
        let page: Option<Retained<AnyObject>> = unsafe { msg_send![&*self.view, currentPage] };
        page.map_or(0, |p| unsafe { msg_send![&*self.document, indexForPage: &*p] })
    }

    /// The page number and zoom in the toolbar.
    pub fn update(&self) {
        self.page_field.setStringValue(&NSString::from_str(&(self.page_index() + 1).to_string()));
        self.pages_label.setStringValue(&NSString::from_str(&format!("of {}", self.pages())));
        let scale: f64 = unsafe { msg_send![&*self.view, scaleFactor] };
        self.zoom_label.setStringValue(&NSString::from_str(&format!("{:.0}%", scale * 100.0)));
    }

    pub fn go_to_page(&self, n: isize) {
        let n = n.clamp(1, self.pages().max(1)) - 1;
        let page: Option<Retained<AnyObject>> = unsafe { msg_send![&*self.document, pageAtIndex: n as usize] };
        if let Some(page) = page {
            let _: () = unsafe { msg_send![&*self.view, goToPage: &*page] };
        }
        self.update();
    }

    /// A toolbar button (or ⌘+, ⌘-, ⌘0). Print, Save and Find are the tab's.
    pub fn run(&mut self, command: Command) {
        let v = &*self.view;
        let none: Option<&AnyObject> = None;
        unsafe {
            match command {
                Command::Sidebar => {
                    let show = self.thumbs.isHidden();
                    self.thumbs.setHidden(!show);
                    let mut frame: NSRect = msg_send![v, frame];
                    let full = self.root.frame().size.width;
                    frame.origin.x = if show { THUMBS_W } else { 0.0 };
                    frame.size.width = full - frame.origin.x;
                    let _: () = msg_send![v, setFrame: frame];
                }
                Command::Previous => msg_send![v, goToPreviousPage: none],
                Command::Next => msg_send![v, goToNextPage: none],
                Command::ZoomOut => msg_send![v, zoomOut: none],
                Command::ZoomIn => msg_send![v, zoomIn: none],
                Command::FitWidth => msg_send![v, setAutoScales: Bool::YES],
                Command::FitPage => {
                    let fit = self.fit_page_scale();
                    let page: Option<Retained<AnyObject>> = msg_send![v, currentPage];
                    let _: () = msg_send![v, setAutoScales: Bool::NO];
                    let _: () = msg_send![v, setScaleFactor: fit];
                    // The whole page in view, from its top.
                    if let Some(page) = page {
                        let _: () = msg_send![v, goToPage: &*page];
                    }
                }
                Command::Rotate => {
                    for i in 0..self.pages() {
                        let page: Retained<AnyObject> = msg_send![&*self.document, pageAtIndex: i as usize];
                        let r: isize = msg_send![&*page, rotation];
                        let _: () = msg_send![&*page, setRotation: (r + 90) % 360];
                    }
                    let _: () = msg_send![v, layoutDocumentView];
                }
                Command::TwoPages => {
                    let mode: isize = msg_send![v, displayMode];
                    // kPDFDisplaySinglePageContinuous (1) or kPDFDisplayTwoUpContinuous (3)
                    let _: () = msg_send![v, setDisplayMode: if mode == 3 { 1isize } else { 3isize }];
                    let _: () = msg_send![v, setDisplaysAsBook: Bool::YES];
                }
                Command::Highlight => self.highlight(),
                Command::ReadAloud | Command::Find | Command::Print | Command::Save => {}
            }
        }
        self.update();
    }

    /// The scale that shows the whole current page (both, side by side) in
    /// the view. PDFKit's `scaleFactorForSizeToFit` fits the width in the
    /// continuous modes, so Fit page did the same as Fit width.
    fn fit_page_scale(&self) -> f64 {
        let v = &*self.view;
        let page: Option<Retained<AnyObject>> = unsafe { msg_send![v, currentPage] };
        let Some(page) = page else { return 1.0 };
        // kPDFDisplayBoxCropBox, turned with the page.
        let bounds: NSRect = unsafe { msg_send![&*page, boundsForBox: 1isize] };
        let rotation: isize = unsafe { msg_send![&*page, rotation] };
        let (mut w, h) = match rotation % 180 {
            0 => (bounds.size.width, bounds.size.height),
            _ => (bounds.size.height, bounds.size.width),
        };
        let mode: isize = unsafe { msg_send![v, displayMode] };
        if mode == 3 {
            w *= 2.0;
        }
        let room: NSRect = unsafe { msg_send![v, bounds] };
        let m: NSEdgeInsets = unsafe { msg_send![v, pageBreakMargins] };
        let fit_w = (room.size.width - m.left - m.right) / w.max(1.0);
        let fit_h = (room.size.height - m.top - m.bottom) / h.max(1.0);
        fit_w.min(fit_h)
    }

    /// `PDFZOOM fits=<0|1>`: whether the whole current page is in view.
    pub fn zoom_state(&self) -> String {
        let scale: f64 = unsafe { msg_send![&*self.view, scaleFactor] };
        format!("PDFZOOM fits={}", (scale <= self.fit_page_scale() + 0.001) as u8)
    }

    /// The selected text gets a yellow highlight (kept by Save).
    fn highlight(&mut self) {
        let v = &*self.view;
        let selection: Option<Retained<AnyObject>> = unsafe { msg_send![v, currentSelection] };
        let Some(selection) = selection else { return };
        let Some(annotation_class) = class(c"PDFAnnotation") else { return };
        let lines: Retained<NSArray<AnyObject>> = unsafe { msg_send![&*selection, selectionsByLine] };
        let yellow = objc2_app_kit::NSColor::colorWithSRGBRed_green_blue_alpha(1.0, 0.9, 0.2, 0.5);
        for line in lines.iter() {
            let pages: Retained<NSArray<AnyObject>> = unsafe { msg_send![&*line, pages] };
            for page in pages.iter() {
                let bounds: NSRect = unsafe { msg_send![&*line, boundsForPage: &*page] };
                let a: Retained<AnyObject> = unsafe {
                    msg_send![msg_send![annotation_class, alloc], initWithBounds: bounds, forType: &*NSString::from_str("Highlight"), withProperties: Option::<&AnyObject>::None]
                };
                unsafe {
                    let _: () = msg_send![&*a, setColor: &*yellow];
                    let _: () = msg_send![&*page, addAnnotation: &*a];
                }
                self.highlights += 1;
            }
        }
        let _: () = unsafe { msg_send![v, clearSelection] };
    }

    /// Read the selection, or from this page on, aloud with the Mac's voice,
    /// selecting each word as it's read; again stops. `tab` hears the words
    /// (`speechSynthesizer:willSpeakWord:ofString:`).
    pub fn read_aloud(&mut self, tab: &Tab) {
        if let Some(s) = self.speech.take() {
            let _: () = unsafe { msg_send![&*s, stopSpeaking] };
            return;
        }
        let text = objc2_foundation::NSMutableString::new();
        let mut spoken = Vec::new();
        let add =
            |spoken: &mut Vec<_>, page_index: usize, page: &AnyObject, range: Option<objc2_foundation::NSRange>| {
                let s: Option<Retained<NSString>> = unsafe { msg_send![page, string] };
                let Some(s) = s else { return };
                let range = range.unwrap_or(objc2_foundation::NSRange::new(0, s.length()));
                spoken.push((text.length(), page_index, range.location));
                text.appendString(&s.substringWithRange(range));
                text.appendString(&NSString::from_str("\n"));
            };
        // The selection's text, page by page, or every page from this one.
        let selection: Option<Retained<AnyObject>> = unsafe { msg_send![&*self.view, currentSelection] };
        let pages: Option<Retained<NSArray<AnyObject>>> = selection.as_ref().map(|s| unsafe { msg_send![&**s, pages] });
        for page in pages.iter().flat_map(|p| p.iter()) {
            let sel = selection.as_deref().unwrap();
            let index: usize = unsafe { msg_send![&*self.document, indexForPage: &*page] };
            let n: usize = unsafe { msg_send![sel, numberOfTextRangesOnPage: &*page] };
            for i in 0..n {
                let range: objc2_foundation::NSRange = unsafe { msg_send![sel, rangeAtIndex: i, onPage: &*page] };
                add(&mut spoken, index, &page, Some(range));
            }
        }
        if text.to_string().trim().is_empty() {
            spoken.clear();
            text.setString(&NSString::new());
            for i in self.page_index().max(0) as usize..self.pages() as usize {
                let page: Retained<AnyObject> = unsafe { msg_send![&*self.document, pageAtIndex: i] };
                add(&mut spoken, i, &page, None);
                if text.length() > 200_000 {
                    break;
                }
            }
        }
        self.spoken = spoken;
        let Some(class) = AnyClass::get(c"NSSpeechSynthesizer") else { return };
        let synth: Retained<AnyObject> = unsafe { msg_send![class, new] };
        unsafe {
            let _: () = msg_send![&*synth, setDelegate: tab];
            if tab.app().is_bench() {
                let _: () = msg_send![&*synth, setVolume: 0.0f32];
            }
            let _: Bool = msg_send![&*synth, startSpeakingString: &*text];
        }
        self.speech = Some(synth);
    }

    /// The voice is about to read `range` of the text: select that word on
    /// its page and keep it in view.
    pub fn speaking(&mut self, range: objc2_foundation::NSRange) {
        let Some(&(start, page_index, offset)) = self.spoken.iter().rev().find(|(s, ..)| *s <= range.location) else {
            return;
        };
        let page: Option<Retained<AnyObject>> = unsafe { msg_send![&*self.document, pageAtIndex: page_index] };
        let Some(page) = page else { return };
        let at = objc2_foundation::NSRange::new(offset + range.location - start, range.length);
        let word: Option<Retained<AnyObject>> = unsafe { msg_send![&*page, selectionForRange: at] };
        let Some(word) = word else { return };
        unsafe {
            let _: () = msg_send![&*self.view, setCurrentSelection: &*word];
            let _: () = msg_send![&*self.view, scrollSelectionToVisible: Option::<&AnyObject>::None];
            let s: Option<Retained<NSString>> = msg_send![&*word, string];
            self.word = s.map(|s| s.to_string()).unwrap_or_default();
        }
    }

    /// Reading aloud ended (or was stopped).
    pub fn done_speaking(&mut self) {
        self.speech = None;
        self.word.clear();
        let _: () = unsafe { msg_send![&*self.view, clearSelection] };
    }

    /// Find in the PDF: every match highlighted, the current one selected.
    /// Returns (index from 1, count).
    pub fn find(&mut self, term: &str, backwards: bool) -> (usize, usize) {
        if self.found.0 != term {
            // NSCaseInsensitiveSearch
            let all: Retained<NSArray<AnyObject>> =
                unsafe { msg_send![&*self.document, findString: &*NSString::from_str(term), withOptions: 1usize] };
            self.found = (term.to_string(), all.iter().collect(), 0);
            let _: () = unsafe { msg_send![&*self.view, setHighlightedSelections: &*all] };
        } else if !self.found.1.is_empty() {
            let n = self.found.1.len();
            self.found.2 = if backwards { (self.found.2 + n - 1) % n } else { (self.found.2 + 1) % n };
        }
        let (_, all, i) = &self.found;
        if let Some(current) = all.get(*i) {
            unsafe {
                let _: () = msg_send![&*self.view, setCurrentSelection: &**current, animate: Bool::YES];
                let _: () = msg_send![&*self.view, goToSelection: &**current];
            }
        }
        self.update();
        (if all.is_empty() { 0 } else { i + 1 }, all.len())
    }

    pub fn clear_find(&mut self) {
        self.found = (String::new(), Vec::new(), 0);
        let _: () = unsafe { msg_send![&*self.view, setHighlightedSelections: Option::<&AnyObject>::None] };
    }

    /// The document, with any highlights, as PDF bytes (Save).
    pub fn data(&self) -> Option<Retained<NSData>> {
        unsafe { msg_send![&*self.document, dataRepresentation] }
    }

    /// The print panel, as a sheet on the tab's window.
    pub fn print(&self, window: &objc2_app_kit::NSWindow) {
        let info = objc2_app_kit::NSPrintInfo::sharedPrintInfo();
        // kPDFPrintPageScaleToFit
        let op: Option<Retained<objc2_app_kit::NSPrintOperation>> = unsafe {
            msg_send![&*self.document, printOperationForPrintInfo: &*info, scalingMode: 1isize, autoRotate: Bool::YES]
        };
        if let Some(op) = op {
            unsafe {
                op.runOperationModalForWindow_delegate_didRunSelector_contextInfo(
                    window,
                    None,
                    None,
                    std::ptr::null_mut(),
                )
            };
        }
    }

    /// `PDF pages= page= sidebar= twoup= rotation= highlights=` for the bench,
    /// and ` reading=<word>` while reading aloud.
    pub fn state(&self) -> String {
        let mode: isize = unsafe { msg_send![&*self.view, displayMode] };
        let first: Option<Retained<AnyObject>> = unsafe { msg_send![&*self.document, pageAtIndex: 0usize] };
        let rotation: isize = first.map_or(0, |p| unsafe { msg_send![&*p, rotation] });
        format!(
            "PDF pages={} page={} sidebar={} twoup={} rotation={rotation} highlights={}{}",
            self.pages(),
            self.page_index() + 1,
            !self.thumbs.isHidden() as u8,
            (mode == 3) as u8,
            self.highlights,
            if self.speech.is_some() { format!(" reading={}", self.word) } else { String::new() }
        )
    }

    /// Select the text of page `n` (tests use it before Highlight).
    pub fn select_page(&self, n: usize) {
        let page: Option<Retained<AnyObject>> = unsafe { msg_send![&*self.document, pageAtIndex: n] };
        let Some(page) = page else { return };
        let all: Option<Retained<AnyObject>> =
            unsafe { msg_send![&*page, selectionForRange: objc2_foundation::NSRange::new(0, 5)] };
        if let Some(sel) = all {
            let _: () = unsafe { msg_send![&*self.view, setCurrentSelection: &*sel] };
        }
    }
}
