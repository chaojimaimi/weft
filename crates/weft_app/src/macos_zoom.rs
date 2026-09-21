//! Programmatic-zoom sequence ownership (PLAN_zoom_sequence_ownership Z-a/Z-b).
//!
//! Double-clicking the titlebar zooms the window WITHOUT entering
//! `inLiveResize`, so the v1.11.10 same-tick-draw branch never armed and the
//! compositor stretched the previous drawable into the new bounds until a
//! frame rendered at the new size. Winit 0.30.13's `WinitWindowDelegate`
//! does not implement `windowWillResize:toSize:` (verified against the
//! registry sources), so the AppKit optional-protocol dispatch
//! (`respondsToSelector`) never reaches it — `class_addMethod` adds exactly
//! that method to the delegate class, and NSWindow starts calling it for
//! BOTH drags and zooms BEFORE committing the new bounds.
//!
//! The injected IMP does two things, both synchronous inside that callback:
//! 1. sizes the Metal layer's drawable to `toSize × contentsScale` — the
//!    `warp_view_set_frame_size` trick (Warp window.rs:1361) — so the
//!    compositor never sees `new bounds + old drawable`;
//! 2. stamps the zoom-sequence activity marker that Z-b's gates read.
//!
//! It returns `toSize` UNCHANGED: the selector returns NSSize (NOT BOOL — a
//! BOOL return would let AppKit read garbage registers as the target frame),
//! and we deliberately keep the system zoom semantics.
//!
//! Every ObjC interaction is wrapped in `catch_unwind` (VULN-005 house
//! style): any failure degrades to the pre-fix behaviour (stretch for one
//! async frame), never aborts.

use std::sync::atomic::{AtomicPtr, AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// Milliseconds since the UNIX epoch of the last `windowWillResize` callback
/// — the Z-b zoom-sequence activity marker. 0 = never active.
static ZOOM_LAST_ACTIVITY_MS: AtomicU64 = AtomicU64::new(0);
/// The window's NSView (leaked raw pointer; the view lives as long as the
/// main window, effectively process lifetime). Registered at startup right
/// after `configure_titlebar`; the IMP reaches the CAMetalLayer through it.
static ZOOM_NS_VIEW: AtomicPtr<std::ffi::c_void> = AtomicPtr::new(std::ptr::null_mut());

/// How long after the last `windowWillResize` callback the zoom sequence is
/// still considered active (PLAN §Z-b: margin for the zoom snap and rapid
/// double-clicks).
const SEQUENCE_SILENCE_MS: u64 = 300;

/// Z-b gate: is a zoom sequence active (a `windowWillResize` callback within
/// the silence window)? Read by the Resized / RedrawRequested branches.
pub(crate) fn zoom_sequence_active() -> bool {
    sequence_active_at(ZOOM_LAST_ACTIVITY_MS.load(Ordering::Acquire), now_ms())
}

/// Pure Z-b gate core (testable): a sequence is active when a callback was
/// seen at `last_ms` and `now_ms` is within the silence window. `last_ms == 0`
/// means "never active".
fn sequence_active_at(last_ms: u64, now_ms: u64) -> bool {
    last_ms != 0 && now_ms.saturating_sub(last_ms) <= SEQUENCE_SILENCE_MS
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Register the main window's NSView and inject `windowWillResize:toSize:`
/// into the delegate class. Call once from the main thread after
/// `configure_titlebar` (needs the NSView/NSWindow to exist); idempotent for
/// the pointer registration, and the method addition is guarded so a second
/// call (or a future winit that implements the method itself) is a no-op.
pub(crate) fn install_zoom_sequence_hook(window: &winit::window::Window) {
    use objc2::rc::Retained;
    use objc2::runtime::AnyObject;
    use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};

    // SAFETY: the raw-window-handle AppKit view is the live winit-owned
    // NSView for the window's lifetime; the leaked raw pointer therefore
    // stays valid for the whole process (the view outlives every frame that
    // reads it, and process teardown never races a paint).
    let Ok(handle) = window.window_handle() else {
        return;
    };
    let RawWindowHandle::AppKit(appkit) = handle.as_raw() else {
        return;
    };
    let ns_view = appkit.ns_view.as_ptr();
    ZOOM_NS_VIEW.store(ns_view.cast(), Ordering::Release);

    // objc2_app_kit::NSView retained from the raw pointer (same pattern as
    // `window_in_live_resize`).
    // SAFETY: pointer just taken from the window handle; `retain` on a live
    // object is always sound.
    let Some(view) = (unsafe { Retained::retain(ns_view.cast::<objc2_app_kit::NSView>()) }) else {
        return;
    };
    let Some(ns_window) = (unsafe { crate::macos_window::ns_window_of(&view) }) else {
        return;
    };

    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        // SAFETY: the delegate is a live Objective-C object owned by the
        // window; `delegate` returns a retained value on 0.2.x of
        // objc2-app-kit.
        let Some(delegate) = (unsafe { ns_window.delegate() }) else {
            return;
        };
        // SAFETY: `as_ptr` yields the live protocol-object pointer; casting
        // it to `AnyObject` is the standard objc2 idiom for class queries,
        // and `object_getClass` on a live object always returns the
        // initialized class object.
        let delegate_class = unsafe {
            objc2::ffi::object_getClass(Retained::as_ptr(&delegate) as *mut AnyObject as *mut _)
        } as *mut _;
        let sel = objc2::sel!(windowWillResize:toSize:);
        // V1 guard (PLAN §Z-a): if a future winit implements the method
        // itself, class_addMethod would fail anyway, but we do not even try —
        // replacing/swizzling a winit-owned implementation is out of scope.
        let already = unsafe { objc2::ffi::class_getInstanceMethod(delegate_class, sel.as_ptr()) };
        if !already.is_null() {
            tracing::info!(
                "zoom hook: delegate already implements windowWillResize:toSize:, not touching it"
            );
            return;
        }
        let imp: unsafe extern "C" fn(
            *mut AnyObject,
            objc2::runtime::Sel,
            *mut AnyObject,
            objc2_foundation::NSSize,
        ) -> objc2_foundation::NSSize = will_resize_imp;
        // IMP is the runtime's untyped `unsafe extern "C" fn()` form.
        // SAFETY: transmute between the typed extern "C" fn pointer and the
        // untyped Imp is sound for the Objective-C calling convention (same
        // pattern as the mouseDownCanMoveWindow override in macos_window.rs).
        let imp_raw: unsafe extern "C" fn() = unsafe { std::mem::transmute(imp) };
        // Type encoding: "{CGSize=dd}@:@{CGSize=dd}" = NSSize return, id
        // self, SEL _cmd, NSWindow* sender, NSSize toSize — the exact
        // signature of
        // `- (NSSize)windowWillResize:(NSWindow *)sender toSize:(NSSize)size`
        // (the sender parameter is part of the selector: omitting it made
        // method_getNumberOfArguments report 3 instead of 4).
        let types: &'static std::ffi::CStr =
            std::ffi::CStr::from_bytes_with_nul(b"{CGSize=dd}@:@{CGSize=dd}\0")
                .expect("static encoding has no interior NUL");
        let added = unsafe {
            objc2::ffi::class_addMethod(delegate_class, sel.as_ptr(), Some(imp_raw), types.as_ptr())
        };
        tracing::info!(added, "zoom hook installed on the window delegate");
    }))
    .unwrap_or_else(|_| {
        tracing::error!(
            "zoom hook install panicked; programmatic-zoom fix is DISABLED this session"
        );
    });
}

/// The injected `windowWillResize:toSize:` IMP: size the drawable FIRST
/// (before AppKit commits the bounds) for every resize, but stamp the
/// zoom-sequence marker ONLY for programmatic zooms — a user drag consults
/// this delegate at every step too, and stamping during a drag would keep
/// the zoom channel (CA flush + present binding) hot through the whole
/// drag, reviving the v1.12.2 B2 drag cost. Returns the proposed size
/// unchanged — system zoom semantics are kept verbatim.
unsafe extern "C" fn will_resize_imp(
    _this: *mut objc2::runtime::AnyObject,
    _cmd: objc2::runtime::Sel,
    _sender: *mut objc2::runtime::AnyObject,
    size: objc2_foundation::NSSize,
) -> objc2_foundation::NSSize {
    use objc2::msg_send;
    use objc2::runtime::AnyObject;

    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let ns_view = ZOOM_NS_VIEW.load(Ordering::Acquire);
        if ns_view.is_null() {
            return;
        }
        // SAFETY: the pointer was registered from the live window handle and
        // outlives every callback; `layer` and `window` getters return +0
        // borrowed pointers (never released here), so the borrows below are
        // valid for the callback's duration.
        let view = unsafe { &*(ns_view as *const AnyObject) };
        // SAFETY: `layer` on a layer-backed view always returns a non-null
        // borrowed CALayer.
        let layer: *mut AnyObject = unsafe { msg_send![view, layer] };
        if layer.is_null() {
            return;
        }
        // SAFETY: the backing layer is the CAMetalLayer attached by
        // `attach_layer_to_nsview`; both messages are plain setters/getters.
        // NOTE (rust-reviewer MEDIUM-2, corrected): the typed
        // `objc2_quartz_core::CAMetalLayer` re-export itself requires the
        // `CALayer` feature we do NOT enable (`all(CALayer, CAMetalLayer)`),
        // so both messages stay raw `msg_send!` -- each is unwind-guarded by
        // the catch_unwind above and failure degrades to the pre-fix
        // behaviour, never a wrong-layer call.
        unsafe {
            let scale: f64 = msg_send![layer, contentsScale];
            let drawable = objc2_foundation::NSSize::new(size.width * scale, size.height * scale);
            let _: () = msg_send![layer, setDrawableSize: drawable];
            // HIGH-2: a user drag runs inside inLiveResize — the zoom
            // channel is for PROGRAMMATIC resizes only (double-click zoom).
            let window: *mut AnyObject = msg_send![view, window];
            if !window.is_null() {
                let dragging: bool = msg_send![window, inLiveResize];
                if !dragging {
                    ZOOM_LAST_ACTIVITY_MS.store(now_ms(), Ordering::Release);
                }
            }
        }
    }))
    .unwrap_or_else(|_| {
        tracing::error!(
            "windowWillResize IMP panicked; drawable pre-sizing skipped for this callback"
        );
    });
    size
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sequence_gate_truth_table() {
        assert!(
            !sequence_active_at(0, 1_000),
            "never-active marker must stay inactive"
        );
        assert!(sequence_active_at(1_000, 1_000), "same instant: active");
        assert!(
            sequence_active_at(1_000, 1_299),
            "within the silence window: active"
        );
        assert!(
            sequence_active_at(1_000, 1_300),
            "boundary is inclusive (<=)"
        );
        assert!(
            !sequence_active_at(1_000, 1_301),
            "past the window: inactive"
        );
        assert!(
            sequence_active_at(5_000, 1_000),
            "clock behind the marker (adjustment): saturating_sub yields 0 <= window, the CONSERVATIVE active verdict -- keep synchronous rendering rather than risk a stretched frame"
        );
    }
}
