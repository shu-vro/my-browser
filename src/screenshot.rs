//! Screenshots (File → Take Screenshot, and the page's right-click menu):
//! the part of the page on screen, the whole page, or an area the user drags
//! out. Each is saved to a file or copied, whichever the user last chose
//! (setting `screenshot_to`). The capture itself is in `Tab::screenshot`.

use std::cell::{Cell, RefCell};

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send};
use objc2_app_kit::{
    NSBezierPath, NSBitmapImageFileType, NSBitmapImageRep, NSColor, NSCursor, NSEvent, NSImage, NSPasteboard,
    NSPasteboardTypePNG, NSRectFill, NSTextField, NSView,
};
use objc2_foundation::{NSData, NSDictionary, NSPoint, NSRect, NSSize, NSString};

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Shot {
    /// What's on screen.
    Visible,
    /// The whole page, top to bottom.
    Full,
    /// An area the user drags out.
    Area,
}

impl Shot {
    pub fn parse(s: &str) -> Option<Shot> {
        match s {
            "visible" => Some(Shot::Visible),
            "full" => Some(Shot::Full),
            "area" => Some(Shot::Area),
            _ => None,
        }
    }
}

/// Gets the area picked, or None if cancelled.
type Picked = Box<dyn FnOnce(Option<NSRect>)>;

pub struct PickerIvars {
    start: Cell<NSPoint>,
    rect: Cell<NSRect>,
    done: RefCell<Option<Picked>>,
}

define_class!(
    /// Covers the page while the user drags out an area: everything else is
    /// dimmed. Esc (or a click without dragging) cancels.
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[ivars = PickerIvars]
    pub struct AreaPicker;

    impl AreaPicker {
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            true
        }

        #[unsafe(method(acceptsFirstResponder))]
        fn accepts_first_responder(&self) -> bool {
            true
        }

        #[unsafe(method(acceptsFirstMouse:))]
        fn accepts_first_mouse(&self, _e: Option<&NSEvent>) -> bool {
            true
        }

        #[unsafe(method(resetCursorRects))]
        fn reset_cursor_rects(&self) {
            self.addCursorRect_cursor(self.bounds(), &NSCursor::crosshairCursor());
        }

        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, e: &NSEvent) {
            let p = self.convertPoint_fromView(e.locationInWindow(), None);
            self.ivars().start.set(p);
            self.ivars().rect.set(NSRect::new(p, NSSize::new(0.0, 0.0)));
            self.setNeedsDisplay(true);
        }

        #[unsafe(method(mouseDragged:))]
        fn mouse_dragged(&self, e: &NSEvent) {
            let (a, b) = (self.ivars().start.get(), self.convertPoint_fromView(e.locationInWindow(), None));
            let bounds = self.bounds().size;
            let (bx, by) = (b.x.clamp(0.0, bounds.width), b.y.clamp(0.0, bounds.height));
            self.ivars().rect.set(NSRect::new(
                NSPoint::new(a.x.min(bx), a.y.min(by)),
                NSSize::new((a.x - bx).abs(), (a.y - by).abs()),
            ));
            self.setNeedsDisplay(true);
        }

        #[unsafe(method(mouseUp:))]
        fn mouse_up(&self, _e: &NSEvent) {
            let r = self.ivars().rect.get();
            self.finish((r.size.width >= 4.0 && r.size.height >= 4.0).then_some(r));
        }

        #[unsafe(method(cancelOperation:))]
        fn cancel_operation(&self, _s: Option<&AnyObject>) {
            self.finish(None);
        }

        #[unsafe(method(keyDown:))]
        fn key_down(&self, e: &NSEvent) {
            // Esc
            if e.keyCode() == 53 {
                self.finish(None);
            }
        }

        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, _dirty: NSRect) {
            let (b, r) = (self.bounds(), self.ivars().rect.get());
            NSColor::colorWithWhite_alpha(0.0, 0.35).setFill();
            if r.size.width <= 0.0 {
                NSRectFill(b);
                return;
            }
            // Dim around the area: above, below, left, right.
            let (x2, y2) = (r.origin.x + r.size.width, r.origin.y + r.size.height);
            for dim in [
                NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(b.size.width, r.origin.y)),
                NSRect::new(NSPoint::new(0.0, y2), NSSize::new(b.size.width, b.size.height - y2)),
                NSRect::new(NSPoint::new(0.0, r.origin.y), NSSize::new(r.origin.x, r.size.height)),
                NSRect::new(NSPoint::new(x2, r.origin.y), NSSize::new(b.size.width - x2, r.size.height)),
            ] {
                NSRectFill(dim);
            }
            NSColor::whiteColor().setStroke();
            NSBezierPath::strokeRect(r);
        }
    }
);

impl AreaPicker {
    /// A picker over `frame` of `parent`; `done` gets the area in the
    /// picker's (top-left) coordinates, or None if cancelled.
    pub fn show(
        parent: &NSView,
        frame: NSRect,
        done: impl FnOnce(Option<NSRect>) + 'static,
        mtm: MainThreadMarker,
    ) -> Retained<AreaPicker> {
        let this = Self::alloc(mtm).set_ivars(PickerIvars {
            start: Cell::new(NSPoint::new(0.0, 0.0)),
            rect: Cell::new(NSRect::ZERO),
            done: RefCell::new(Some(Box::new(done))),
        });
        let this: Retained<AreaPicker> = unsafe { msg_send![super(this), initWithFrame: frame] };
        this.setAutoresizingMask(
            objc2_app_kit::NSAutoresizingMaskOptions::ViewWidthSizable
                | objc2_app_kit::NSAutoresizingMaskOptions::ViewHeightSizable,
        );
        let hint = NSTextField::labelWithString(&NSString::from_str("  Drag to select an area · Esc cancels  "), mtm);
        hint.setDrawsBackground(true);
        hint.setBackgroundColor(Some(&NSColor::colorWithWhite_alpha(0.1, 0.85)));
        hint.setTextColor(Some(&NSColor::whiteColor()));
        hint.sizeToFit();
        let size = hint.frame().size;
        hint.setFrameOrigin(NSPoint::new((frame.size.width - size.width) / 2.0, 12.0));
        hint.setAutoresizingMask(
            objc2_app_kit::NSAutoresizingMaskOptions::ViewMinXMargin
                | objc2_app_kit::NSAutoresizingMaskOptions::ViewMaxXMargin,
        );
        this.addSubview(&hint);
        parent.addSubview(&this);
        if let Some(w) = this.window() {
            w.makeFirstResponder(Some(&this));
        }
        this
    }

    /// End picking: off the window, and tell whoever asked.
    pub fn finish(&self, area: Option<NSRect>) {
        let done = self.ivars().done.take();
        self.removeFromSuperview();
        if let Some(done) = done {
            done(area);
        }
    }

    /// Drag from `a` to `b` (picker coordinates), as the mouse would (tests).
    pub fn drag(&self, a: NSPoint, b: NSPoint) {
        let event = |kind, p: NSPoint| {
            let p = self.convertPoint_toView(p, None);
            NSEvent::mouseEventWithType_location_modifierFlags_timestamp_windowNumber_context_eventNumber_clickCount_pressure(
                kind,
                p,
                objc2_app_kit::NSEventModifierFlags::empty(),
                0.0,
                self.window().map_or(0, |w| w.windowNumber()),
                None,
                0,
                1,
                1.0,
            )
        };
        use objc2_app_kit::NSEventType;
        if let (Some(down), Some(drag), Some(up)) = (
            event(NSEventType::LeftMouseDown, a),
            event(NSEventType::LeftMouseDragged, b),
            event(NSEventType::LeftMouseUp, b),
        ) {
            unsafe {
                let _: () = msg_send![self, mouseDown: &*down];
                let _: () = msg_send![self, mouseDragged: &*drag];
                let _: () = msg_send![self, mouseUp: &*up];
            }
        }
    }
}

/// An image's PNG bytes, and its size in pixels.
pub fn png_of_image(image: &NSImage) -> Option<(Retained<NSData>, (usize, usize))> {
    let tiff = image.TIFFRepresentation()?;
    let rep = NSBitmapImageRep::imageRepWithData(&tiff)?;
    png_of_rep(&rep)
}

pub fn png_of_rep(rep: &NSBitmapImageRep) -> Option<(Retained<NSData>, (usize, usize))> {
    let png = unsafe { rep.representationUsingType_properties(NSBitmapImageFileType::PNG, &NSDictionary::new()) }?;
    Some((png, (rep.pixelsWide() as usize, rep.pixelsHigh() as usize)))
}

/// Put a PNG on the clipboard. In bench mode a private pasteboard, so tests
/// leave the user's clipboard alone.
pub fn copy_png(png: &NSData, bench: bool) -> bool {
    let board = if bench { NSPasteboard::pasteboardWithUniqueName() } else { NSPasteboard::generalPasteboard() };
    board.clearContents();
    board.setData_forType(Some(png), unsafe { NSPasteboardTypePNG })
}
