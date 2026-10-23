//! macOS NSWindow helpers: Metal layer attachment, transparent titlebar configuration.
//!
//! Extracted from renderer.rs (M4 step 3).

use metal::MetalLayer;
use objc2::msg_send;
use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
use winit::window::Window;

use crate::layout::Rect;

/// v1.11.6 (PLAN_v1116 M2/D-i): query whether the winit window's NSWindow is
/// currently in a live (user-drag) resize. The renderer flips
/// `presentsWithTransaction` on the CAMetalLayer while true, so the resized
/// layer bounds and the new frame commit in one Core Animation transaction
/// (Warp precedent) instead of CA stretching the previous drawable.
///
/// False when the handle cannot be obtained, the platform isn't macOS, the
/// NSView has no owning NSWindow (detached/teardown), or the ObjC query
/// panics (belt-and-suspenders — a hot-path failure must degrade, not abort).
pub(crate) fn window_in_live_resize(window: &Window) -> bool {
    use objc2::rc::Retained;
    use objc2_app_kit::NSView;
    use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};

    let Ok(handle) = window.window_handle() else {
        return false;
    };
    let RawWindowHandle::AppKit(appkit) = handle.as_raw() else {
        return false; // Not macOS — nothing to query.
    };
    // The raw handle's ns_view is a live NSView for the window's lifetime
    // (winit owns it); only `window()`/`inLiveResize` are invoked on it.
    let ns_view: *mut objc2::runtime::AnyObject = appkit.ns_view.as_ptr().cast();
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
        let view: &NSView = &*ns_view.cast();
        // `[view window]` is unretained; the typed `NSView::window()` returns
        // a retained Option. A detached view (no window) → false.
        let retained: Option<Retained<objc2_app_kit::NSWindow>> = view.window();
        retained.is_some_and(|window| window.inLiveResize())
    }))
    .unwrap_or_else(|_| {
        // v1.13.2 (WP-B): this probe runs every frame during a live resize, so
        // a persistent ObjC panic would spam a warn each frame — log at debug
        // level instead. The `false` degradation is unchanged.
        tracing::debug!("objc bridge panicked (in_live_resize probe)");
        false
    })
}

/// v1.11: NSView `mouseDownCanMoveWindow` override — returns `NO` so macOS
/// does not auto-start a window drag when the user presses on the tab bar
/// (which lives in the titlebar region under `FullSizeContentView`).
///
/// Without this override, winit's NSView inherits the default
/// `mouseDownCanMoveWindow` (which returns `YES` for non-opaque views), and
/// macOS intercepts `CursorMoved` events during the drag — preventing
/// tab-to-reorder from ever seeing pointer movement. The tab-bar controller
/// still calls `drag_window()` explicitly for empty titlebar regions, so
/// background-to-drag-window behavior is preserved.
unsafe extern "C" fn mouse_down_can_move_window(
    _self: *mut objc2::runtime::AnyObject,
    _cmd: objc2::runtime::Sel,
) -> objc2::runtime::Bool {
    objc2::runtime::Bool::NO
}

/// Attach a Metal layer to the winit window's NSView.
pub(crate) unsafe fn attach_layer_to_nsview(layer: &MetalLayer, window: &Window, scale: f64) {
    let handle = window.window_handle().expect("Failed to get window handle");
    let RawWindowHandle::AppKit(appkit) = handle.as_raw() else {
        panic!("Weft requires macOS (AppKit)");
    };

    let ns_view: *mut objc2::runtime::AnyObject = appkit.ns_view.as_ptr().cast();
    let layer_ptr: *mut objc2::runtime::AnyObject =
        (&**layer) as *const _ as *mut objc2::runtime::AnyObject;

    // VULN-005: an ObjC runtime assertion inside these raw messages must
    // degrade (window renders without a Metal layer), not abort the process.
    // The `expect`/`panic!` above are programming errors and stay outside
    // the guard so they still propagate normally.
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
        let _: () = msg_send![ns_view, setWantsLayer: true];
        let _: () = msg_send![ns_view, setLayer: layer_ptr];
        // Retina: the backing store (drawable) is physical pixels; tell the layer its
        // contents are at the window scale so it isn't displayed at the wrong density.
        let _: () = msg_send![layer_ptr, setContentsScale: scale];
        // Metal renders with a top-left origin (framebuffer row 0 = top). The vertex
        // shader already maps logical-top → clip-top, so the drawable is upright; do NOT
        // set geometryFlipped (it would composite the framebuffer upside-down).
        let _: () = msg_send![layer_ptr, setGeometryFlipped: false];
    }))
    .unwrap_or_else(|_| {
        tracing::error!(
            "attach_layer_to_nsview: ObjC panic while attaching the Metal layer; continuing without a layer"
        );
    });
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

        // v1.11: Override `mouseDownCanMoveWindow` on the NSView's class to
        // return NO. winit 0.30's NSView subclass does not override this
        // method, so it inherits the default (YES for non-opaque views). With
        // FullSizeContentView, the tab bar lives in the titlebar region; if
        // mouseDownCanMoveWindow returns YES, macOS auto-starts a window drag
        // on press+move, stealing CursorMoved events from winit and breaking
        // tab drag-to-reorder. Returning NO lets the press reach winit's
        // mouseDown handler (tab switch + tab_drag state), and the subsequent
        // CursorMoved events flow through to handle_tab_drag_move. The
        // tab-bar background still drags the window via explicit
        // drag_window() in mouse_press_controller.
        // SAFETY: `view_class` is the live Objective-C class object of the
        // window's NSView (retrieved via `object_getClass`, always valid for
        // an initialized NSView); the selector `mouseDownCanMoveWindow` and
        // the "B@:" encoding (BOOL return, id self, SEL _cmd) exactly match
        // the swapped implementation's signature; `class_replaceMethod` is
        // thread-safe and the override applies to the class (all windows
        // share the winit NSView subclass). `transmute` between the typed
        // extern "C" fn pointer and the runtime's untyped `Imp` form is sound
        // for the Objective-C calling convention (both are fn pointers).
        let view_class =
            objc2::ffi::object_getClass(&*ns_view as *const NSView as *mut _) as *mut _;
        let sel = objc2::sel!(mouseDownCanMoveWindow);
        let imp: unsafe extern "C" fn(
            *mut objc2::runtime::AnyObject,
            objc2::runtime::Sel,
        ) -> objc2::runtime::Bool = mouse_down_can_move_window;
        // IMP is `Option<unsafe extern "C" fn()>` — transmute the typed
        // function pointer to the untyped form expected by the runtime.
        let imp_raw: unsafe extern "C" fn() = std::mem::transmute(imp);
        // Type encoding: "B@:" = BOOL return, id self, SEL _cmd
        let types = b"B@:\0";
        objc2::ffi::class_replaceMethod(
            view_class,
            sel.as_ptr(),
            Some(imp_raw),
            types.as_ptr() as *const std::os::raw::c_char,
        );
    }))
    .map_err(|_| tracing::warn!("objc bridge panicked (window configure_titlebar)"));
}

/// Helper: get the NSWindow owning an NSView (`[view window]`), retained.
pub(crate) unsafe fn ns_window_of(
    view: &objc2_app_kit::NSView,
) -> Option<objc2::rc::Retained<objc2_app_kit::NSWindow>> {
    use objc2::msg_send;
    use objc2::rc::Retained;
    use objc2::runtime::AnyObject;
    // VULN-005: guard the raw message HERE so every call site is covered
    // (configure_titlebar / set_window_opaque already catch_unwind around
    // their own bodies, but ui_events.rs calls this helper bare). Nested
    // catch is redundant but harmless; a panic degrades to "no window".
    let ptr: *mut AnyObject =
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
            msg_send![view, window]
        })) {
            Ok(ptr) => ptr,
            Err(_) => {
                tracing::warn!("ns_window_of: ObjC panic in [view window]; treating as no window");
                return None;
            }
        };
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
    // VULN-005: an ObjC assertion in setOpaque: must not abort the app —
    // worst case the layer keeps its previous opacity.
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
        let _: () = msg_send![layer_ptr, setOpaque: opaque];
    }))
    .unwrap_or_else(|_| {
        tracing::error!("set_layer_opaque: ObjC panic in setOpaque:; keeping the previous opacity");
    });
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

// ── v1.13.17 S1: traffic-light gap drag + double-click system preference ──

/// What a double-click on the titlebar background should do (v1.13.17 S1),
/// resolved from the system's `AppleActionOnDoubleClick` preference.
///
/// There is no `Fill` variant: the setting's stored value `"Fill"` (macOS
/// Sequoia+ window tiling) is approximated with `Zoom` for this release —
/// Weft has no macOS 26 tiling ability to fill with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DoubleClickTitlebarAction {
    /// Toggle maximize/restore (the macOS default; stored value "Maximize",
    /// which is what System Settings' "Zoom" label writes).
    Zoom,
    /// Minimize the window (stored value "Minimize").
    Minimize,
    /// Explicitly configured no-op (stored value "None") — the double-click
    /// must not toggle anything; single-click drag stays unaffected.
    NoAction,
}

/// Pure mapping from the `AppleActionOnDoubleClick` stored string to the
/// action. `None` (key absent) / `""` / `"Maximize"` / `"Fill"` / unknown
/// values all resolve to `Zoom` — the macOS default. "Fill" is approximated
/// as Zoom (Weft has no macOS 26 fill/tiling ability; stopgap release).
pub(crate) fn double_click_action_from_pref(value: Option<&str>) -> DoubleClickTitlebarAction {
    match value {
        Some("Minimize") => DoubleClickTitlebarAction::Minimize,
        Some("None") => DoubleClickTitlebarAction::NoAction,
        _ => DoubleClickTitlebarAction::Zoom,
    }
}

/// Read `AppleActionOnDoubleClick` from the standard user defaults and map
/// it to a [`DoubleClickTitlebarAction`]. Queried live on each double-click
/// (a low-frequency path — no caching). Any broken query (null class, nil
/// value, non-UTF8, ObjC panic) degrades to the macOS default `Zoom`,
/// matching `system_appearance_is_dark`'s guarded-unwind convention.
pub(crate) fn double_click_titlebar_action() -> DoubleClickTitlebarAction {
    let raw: Option<String> = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
        read_double_click_pref_raw()
    }))
    .unwrap_or_else(|_| {
        tracing::warn!("double_click_pref: ObjC panic; using the Zoom default");
        None
    });
    double_click_action_from_pref(raw.as_deref())
}

/// SAFETY: main-thread NSUserDefaults query via class lookup + `msg_send!`;
/// every pointer is null-checked before it is messaged, and `UTF8String`'s
/// returned pointer is only read through `CStr::from_ptr` while the string
/// object is still alive in this scope (same shape as
/// `macos_system::system_appearance_is_dark`).
unsafe fn read_double_click_pref_raw() -> Option<String> {
    use objc2::runtime::AnyObject;

    let defaults_cls = objc2::ffi::objc_getClass(c"NSUserDefaults".as_ptr());
    if defaults_cls.is_null() {
        return None;
    }
    // SAFETY: `standardUserDefaults` on the looked-up class; returns a live
    // singleton (never released here).
    let defaults: *mut AnyObject =
        unsafe { msg_send![defaults_cls as *const AnyObject, standardUserDefaults] };
    if defaults.is_null() {
        return None;
    }
    let c_key = std::ffi::CString::new("AppleActionOnDoubleClick").ok()?;
    // SAFETY: `stringWithUTF8String` on the NSString class with a valid
    // C string pointer.
    let str_cls = objc2::ffi::objc_getClass(c"NSString".as_ptr());
    if str_cls.is_null() {
        return None;
    }
    let key_ns: *mut AnyObject =
        unsafe { msg_send![str_cls as *const AnyObject, stringWithUTF8String: c_key.as_ptr()] };
    if key_ns.is_null() {
        return None;
    }
    // SAFETY: `stringForKey:` on the live defaults dictionary; a missing key
    // legally returns nil (mapped to the Zoom default by the caller).
    let value_ns: *mut AnyObject = unsafe { msg_send![defaults, stringForKey: key_ns] };
    if value_ns.is_null() {
        return None;
    }
    // SAFETY: `UTF8String` on a live NSString; the pointer stays valid for
    // the remainder of this scope.
    let c_str: *const std::os::raw::c_char = unsafe { msg_send![value_ns, UTF8String] };
    if c_str.is_null() {
        return None;
    }
    let value = std::ffi::CStr::from_ptr(c_str).to_str().ok()?; // non-UTF8 → default
    Some(value.trim().to_string())
}

/// v1.13.17 S1: click point vs. rectangle test, half-open on the right/bottom
/// edges so adjacent rectangles never both claim the point (matches
/// `HitRegion::contains_half_open`'s convention).
pub(crate) fn point_in_rect(x: f64, y: f64, rect: Rect) -> bool {
    let (x, y) = (x as f32, y as f32);
    x >= rect[0] && x < rect[2] && y >= rect[1] && y < rect[3]
}

/// Flip AppKit window-base rects (bottom-left origin, logical points) to
/// Weft's top-left physical-pixel space. Pure — unit-tested with scale=1 and
/// scale=2 fixtures; `press_tab_bar`'s x/y and `traffic_lights_width()` both
/// speak physical pixels, so the ×scale must happen here, not at the caller.
fn rects_flip_to_physical(rects_ns: [[f64; 4]; 3], window_height_pt: f64, scale: f64) -> [Rect; 3] {
    rects_ns.map(|[x, y, w, h]| {
        let y_top_pt = window_height_pt - y - h;
        [
            (x * scale) as f32,
            (y_top_pt * scale) as f32,
            ((x + w) * scale) as f32,
            ((y_top_pt + h) * scale) as f32,
        ]
    })
}

/// v1.13.17 S1: the three standard traffic-light buttons' rectangles in the
/// window's top-left physical-pixel space — the same space `press_tab_bar`
/// receives its x/y in. Queried live on every press (no cache: the rects
/// move with window width/fullscreen state, and a press is low-frequency).
///
/// `None` on any failure (no handle, no NSWindow, a button absent for the
/// current style mask, ObjC panic) — the caller falls back to the previous
/// conservative "whole traffic-light strip is consumed" behavior.
pub(crate) fn traffic_button_rects(window: &Window) -> Option<[Rect; 3]> {
    use objc2::rc::Retained;
    use objc2_app_kit::{NSView, NSWindowButton};
    use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};

    let handle = window.window_handle().ok()?;
    let RawWindowHandle::AppKit(appkit) = handle.as_raw() else {
        return None; // Not macOS — nothing to query.
    };
    // SAFETY: retain of the live winit-owned NSView (install_zoom_override_hook
    // precedent); only valid for the window's lifetime, which `window` outlives.
    let view: Retained<NSView> = unsafe { Retained::retain(appkit.ns_view.as_ptr().cast()) }?;
    let ns_window = unsafe { ns_window_of(&view) }?;
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let frame = ns_window.frame();
        let scale = window.scale_factor();
        let kinds = [
            NSWindowButton::NSWindowCloseButton,
            NSWindowButton::NSWindowMiniaturizeButton,
            NSWindowButton::NSWindowZoomButton,
        ];
        let mut rects_ns = [[0.0f64; 4]; 3];
        for (slot, kind) in rects_ns.iter_mut().zip(kinds.iter()) {
            // Typed objc2-app-kit API (no msg_send!): a hidden/absent button
            // (e.g. zoom omitted by a custom style mask) yields None and the
            // whole query degrades to the caller's conservative fallback.
            let button = ns_window.standardWindowButton(*kind)?;
            // `bounds` in the button's own space + convert-to-window (nil
            // target) → window-base logical points, bottom-left origin.
            let in_window = button.convertRect_toView(button.bounds(), None);
            *slot = [
                in_window.origin.x,
                in_window.origin.y,
                in_window.size.width,
                in_window.size.height,
            ];
        }
        Some(rects_flip_to_physical(rects_ns, frame.size.height, scale))
    }))
    .unwrap_or_else(|_| {
        tracing::warn!("traffic_button_rects: ObjC panic; using the conservative fallback");
        None
    })
}

#[cfg(test)]
mod tests {
    use super::{
        double_click_action_from_pref, point_in_rect, rects_flip_to_physical,
        DoubleClickTitlebarAction,
    };

    #[test]
    fn double_click_pref_maps_every_stored_value() {
        // macOS default when the key is absent or empty.
        assert_eq!(
            double_click_action_from_pref(None),
            DoubleClickTitlebarAction::Zoom
        );
        assert_eq!(
            double_click_action_from_pref(Some("")),
            DoubleClickTitlebarAction::Zoom
        );
        // System Settings' "Zoom" label stores "Maximize".
        assert_eq!(
            double_click_action_from_pref(Some("Maximize")),
            DoubleClickTitlebarAction::Zoom
        );
        // "Fill" (macOS Sequoia+) is approximated with Zoom — no tiling here.
        assert_eq!(
            double_click_action_from_pref(Some("Fill")),
            DoubleClickTitlebarAction::Zoom
        );
        // Explicit minimize / explicitly-none.
        assert_eq!(
            double_click_action_from_pref(Some("Minimize")),
            DoubleClickTitlebarAction::Minimize
        );
        assert_eq!(
            double_click_action_from_pref(Some("None")),
            DoubleClickTitlebarAction::NoAction
        );
        // Unknown future values stay on the macOS default.
        assert_eq!(
            double_click_action_from_pref(Some("SomethingNew")),
            DoubleClickTitlebarAction::Zoom
        );
    }

    /// Bottom-left logical rect (x=10, y=20, w=30, h=40) in a 100pt-tall
    /// window at scale 1: top-left origin flips y to 100-20-40 = 40.
    #[test]
    fn flip_to_physical_at_scale_one() {
        let rects = [[10.0, 20.0, 30.0, 40.0], [0.0; 4], [0.0; 4]];
        let [out, _, _] = rects_flip_to_physical(rects, 100.0, 1.0);
        assert_eq!(out, [10.0, 40.0, 40.0, 80.0]);
    }

    /// Same rect at scale 2: every coordinate doubles in physical pixels —
    /// the 2x-display fixture (unmultiplied logical values would land the
    /// hit test at half the real position).
    #[test]
    fn flip_to_physical_at_scale_two() {
        let rects = [[10.0, 20.0, 30.0, 40.0], [0.0; 4], [0.0; 4]];
        let [out, _, _] = rects_flip_to_physical(rects, 100.0, 2.0);
        assert_eq!(out, [20.0, 80.0, 80.0, 160.0]);
    }

    #[test]
    fn point_in_rect_is_half_open() {
        let rect: crate::layout::Rect = [10.0, 20.0, 40.0, 60.0];
        assert!(point_in_rect(10.0, 20.0, rect));
        assert!(point_in_rect(39.9, 59.9, rect));
        // Right/bottom edges are exclusive…
        assert!(!point_in_rect(40.0, 30.0, rect));
        assert!(!point_in_rect(20.0, 60.0, rect));
        // …left/top edges inclusive, outside points reject.
        assert!(point_in_rect(10.0, 30.0, rect));
        assert!(!point_in_rect(9.9, 30.0, rect));
        assert!(!point_in_rect(20.0, 19.9, rect));
    }
}
