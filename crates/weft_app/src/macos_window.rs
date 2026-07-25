//! macOS NSWindow helpers: Metal layer attachment, transparent titlebar configuration.
//!
//! Extracted from renderer.rs (M4 step 3).

use metal::MetalLayer;
use objc2::msg_send;
use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
use winit::window::Window;

/// Attach a Metal layer to the winit window's NSView.
pub(crate) unsafe fn attach_layer_to_nsview(layer: &MetalLayer, window: &Window, scale: f64) {
    let handle = window.window_handle().expect("Failed to get window handle");
    let RawWindowHandle::AppKit(appkit) = handle.as_raw() else {
        panic!("Weft requires macOS (AppKit)");
    };

    let ns_view: *mut objc2::runtime::AnyObject = appkit.ns_view.as_ptr().cast();
    let layer_ptr: *mut objc2::runtime::AnyObject =
        (&**layer) as *const _ as *mut objc2::runtime::AnyObject;

    let _: () = msg_send![ns_view, setWantsLayer: true];
    let _: () = msg_send![ns_view, setLayer: layer_ptr];
    // Retina: the backing store (drawable) is physical pixels; tell the layer its
    // contents are at the window scale so it isn't displayed at the wrong density.
    let _: () = msg_send![layer_ptr, setContentsScale: scale];
    // Metal renders with a top-left origin (framebuffer row 0 = top). The vertex
    // shader already maps logical-top → clip-top, so the drawable is upright; do NOT
    // set geometryFlipped (it would composite the framebuffer upside-down).
    let _: () = msg_send![layer_ptr, setGeometryFlipped: false];
}

/// v1.1: Configure a Warp-style transparent titlebar on the native NSWindow.
///
/// Sets `NSWindowStyleMaskFullSizeContentView` (Metal layer extends under the
/// titlebar), `titlebarAppearsTransparent` (no system titlebar chrome), and
/// `titleVisibility:hidden` (no title text). Window dragging is intentionally
/// *not* enabled for the full Metal background: doing so lets AppKit steal
/// scrollbar and terminal drags. The tab-bar controller explicitly calls
/// winit's native `drag_window()` only for empty titlebar regions.
///
/// Uses the typed `objc2-app-kit` `NSWindow` methods (safe functions) rather
/// than raw `msg_send!` to avoid the nounwind-abort panic that disabled
/// `set_dock_icon`.
pub(crate) fn configure_titlebar(window: &Window) {
    use objc2::rc::Retained;
    use objc2_app_kit::{NSView, NSWindowStyleMask, NSWindowTitleVisibility};
    use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};

    // Wrap in catch_unwind as a belt-and-suspenders guard against any ObjC
    // runtime assertion (matching the set_dock_icon defensive pattern), even
    // though these typed setters are nominally safe.
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
        let raw = match window.window_handle() {
            Ok(h) => h.as_raw(),
            Err(_) => return,
        };
        let RawWindowHandle::AppKit(appkit) = raw else {
            return; // Not macOS — nothing to configure.
        };
        // Retain the NSView from the raw handle, then reach its NSWindow.
        let ns_view: Retained<NSView> = match Retained::retain(appkit.ns_view.as_ptr().cast()) {
            Some(v) => v,
            None => return,
        };
        let ns_window = match ns_window_of(&ns_view) {
            Some(w) => w,
            None => return,
        };
        // Add FullSizeContentView (1 << 15) to the existing style mask without
        // dropping Titled/Closable/etc. (those keep the traffic lights).
        let mask = ns_window.styleMask();
        ns_window.setStyleMask(mask | NSWindowStyleMask::FullSizeContentView);
        ns_window.setTitlebarAppearsTransparent(true);
        ns_window.setTitleVisibility(NSWindowTitleVisibility::NSWindowTitleHidden);
        ns_window.setMovableByWindowBackground(false);
    }));
}

/// Helper: get the NSWindow owning an NSView (`[view window]`), retained.
pub(crate) unsafe fn ns_window_of(
    view: &objc2_app_kit::NSView,
) -> Option<objc2::rc::Retained<objc2_app_kit::NSWindow>> {
    use objc2::msg_send;
    use objc2::rc::Retained;
    use objc2::runtime::AnyObject;
    let ptr: *mut AnyObject = msg_send![view, window];
    if ptr.is_null() {
        None
    } else {
        // Retain via the NSWindow type so the returned Retained<NSWindow> is
        // properly managed. `[view window]` returns an unretained reference.
        Retained::retain(ptr.cast())
    }
}

/// Toggle the CAMetalLayer's `opaque` flag. A non-opaque layer lets a
/// transparent NSWindow show the desktop through alpha-scaled cell backgrounds.
///
/// # Safety
/// `layer` must be a live `CAMetalLayer` (or subclass). `setOpaque:` is the
/// `CALayer` property setter, so the selector is valid.
pub(crate) unsafe fn set_layer_opaque(layer: &MetalLayer, opaque: bool) {
    let layer_ptr: *mut objc2::runtime::AnyObject =
        (&**layer) as *const _ as *mut objc2::runtime::AnyObject;
    let _: () = msg_send![layer_ptr, setOpaque: opaque];
}

/// v1.2.11 fix: toggle NSWindow-level transparency at runtime.
///
/// Winit's `with_transparent()` only takes effect at window creation; once
/// the NSWindow is created with `opaque=YES` and a system background color,
/// lowering the Metal layer's alpha alone is not enough — the system
/// background fills the transparent regions, so the user sees "no change"
/// when dragging the Opacity slider below 1.0.
///
/// This helper reaches the NSWindow via the raw-window-handle AppKit handle
/// and flips two properties in lockstep with `set_layer_opaque`:
///   - `setOpaque:` → NO lets AppKit composite the window with alpha.
///   - `setBackgroundColor:` → `[NSColor clearColor]` removes the system
///     background fill so the desktop shows through.
///
/// Raising opacity back to 1.0 reverses both: `setOpaque:YES` and a solid
/// system background color (`windowBackgroundColor`) so the window looks
/// normal again. Without the background color reset, an opaque window would
/// still show a transparent corner / shadow halo.
///
/// Returns `true` if the NSWindow was successfully updated, `false` if the
/// handle could not be obtained (non-macOS, or window not yet created).
pub(crate) fn set_window_opaque(window: &Window, opaque: bool) -> bool {
    use objc2::rc::Retained;
    use objc2_app_kit::{NSColor, NSView};
    use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};

    // Belt-and-suspenders: any ObjC runtime assertion should not abort weft.
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
        let raw = match window.window_handle() {
            Ok(h) => h.as_raw(),
            Err(_) => return false,
        };
        let RawWindowHandle::AppKit(appkit) = raw else {
            return false; // Not macOS.
        };
        let ns_view: Retained<NSView> = match Retained::retain(appkit.ns_view.as_ptr().cast()) {
            Some(v) => v,
            None => return false,
        };
        let ns_window = match ns_window_of(&ns_view) {
            Some(w) => w,
            None => return false,
        };
        // Flip both the opaque flag and the background color in lockstep.
        // If we only set `setOpaque:NO` without `clearColor`, AppKit still
        // fills the window with the default system background and the user
        // sees no transparency. If we only set `clearColor` without
        // `setOpaque:NO`, AppKit ignores the alpha and the window stays
        // opaque. Both must move together.
        ns_window.setOpaque(opaque);
        if opaque {
            // Restore the standard window background so shadows and corners
            // look normal. `windowBackgroundColor` is the theme-aware default
            // (light gray in Light mode, dark gray in Dark mode).
            ns_window.setBackgroundColor(Some(&NSColor::windowBackgroundColor()));
        } else {
            // clearColor = fully transparent; the Metal layer's clear color
            // (modulated by `self.opacity`) is what the user actually sees.
            ns_window.setBackgroundColor(Some(&NSColor::clearColor()));
        }
        true
    }))
    .unwrap_or(false)
}
