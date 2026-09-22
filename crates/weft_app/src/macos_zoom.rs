//! Programmatic-zoom sequence ownership (PLAN_zoom_sequence_ownership Z-d).
//!
//! Double-clicking the titlebar zooms the window programmatically. The zoom
//! runs INSIDE a live-resize window (`inLiveResize == true` at the old
//! windowWillResize callback -- 1.12.6 field diagnostics), so a stamp taken
//! there is both wrong and exempted; and the drawable pre-sizing that used
//! to live in that callback created a REVERSE mismatch (new drawable / old
//! bounds). Z-d moves the drawable sync to the point where the bounds are
//! actually applied: the NSView `setFrameSize:` callback.
//!
//! winit 0.30.13's `WinitView` does not implement `setFrameSize:` (verified
//! against the registry sources -- the base NSView implementation is reached
//! through inheritance), so `class_addMethod` adds exactly that method to
//! the view class and AppKit's setFrame flow calls it synchronously with the
//! new size. The IMP forwards to super FIRST (`[super setFrameSize:]` is the
//! primitive that applies the new size AND emits `frameDidChange:` -- the
//! source of winit's Resized events; omitting the forward would stall the
//! whole resize pipeline), then sizes the Metal layer's drawable to
//! `size × contentsScale` -- the `warp_view_set_frame_size` trick (Warp
//! window.rs:1361) -- so the drawable and the bounds change in the same
//! callback and the compositor never sees a mismatched pair.
//!
//! The zoom-sequence marker itself lives in the Resized path: a PROGRAMMATIC
//! zoom arrives as a one-shot size jump with inLiveResize already false
//! (`is_programmatic_resize_jump`), while a drag streams small steps inside
//! a live-resize gesture -- the guard separates the two cleanly.
//!
//! Every ObjC interaction is wrapped in `catch_unwind` + `tracing::error!`
//! (VULN-005 house style): any failure degrades to the pre-fix behaviour
//! (stretch for a frame), never aborts.

/// Add `setFrameSize:` to the winit view class. Call once from the main
/// thread after window creation; a repeated call is a no-op by virtue of the
/// `class_addMethod` return value (see the install log lines).
///
/// Guard semantics (PLAN Z-d, rust-reviewer P0): `class_addMethod` succeeds
/// when the class itself lacks the selector (our case -- the base NSView
/// implementation is reached through inheritance and remains reachable via
/// super) and returns false ONLY when the class itself already implements it
/// (i.e. a future winit implements `setFrameSize:` -- the one case we must
/// not touch). A false return therefore means "hook not installed, behaviour
/// degrades to the 1.12.7 form".
pub(crate) fn install_zoom_sequence_hook(window: &winit::window::Window) {
    use objc2::rc::Retained;
    use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};

    let Ok(handle) = window.window_handle() else {
        return;
    };
    let RawWindowHandle::AppKit(appkit) = handle.as_raw() else {
        return;
    };
    // SAFETY: the raw-window-handle AppKit view is the live winit-owned
    // NSView for the window's lifetime; `retain` on a live object is sound.
    let Some(view) =
        (unsafe { Retained::retain(appkit.ns_view.as_ptr().cast::<objc2_app_kit::NSView>()) })
    else {
        return;
    };

    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        use objc2::runtime::AnyObject;

        // SAFETY: `object_getClass` on a live object always returns the
        // initialized class object.
        let view_class = unsafe {
            objc2::ffi::object_getClass(Retained::as_ptr(&view) as *mut AnyObject as *mut _)
        } as *mut _;
        let sel = objc2::sel!(setFrameSize:);
        // IMP signature (PLAN Z-d pre-registered): setFrameSize: returns
        // void and takes the new size; the class itself does NOT implement
        // it, so this add both succeeds and takes priority over the base
        // implementation for every future frame change.
        let imp: unsafe extern "C" fn(
            *mut AnyObject,
            objc2::runtime::Sel,
            objc2_foundation::NSSize,
        ) = set_frame_size_imp;
        // SAFETY: transmute between the typed extern "C" fn pointer and the
        // runtime's untyped `Imp` form is sound for the Objective-C calling
        // convention (same pattern as the mouseDownCanMoveWindow override in
        // macos_window.rs).
        let imp_raw: unsafe extern "C" fn() = unsafe { std::mem::transmute(imp) };
        // Type encoding: "v@:{CGSize=dd}" = void return, id self, SEL _cmd,
        // NSSize argument.
        let types: &'static std::ffi::CStr =
            std::ffi::CStr::from_bytes_with_nul(b"v@:{CGSize=dd}\0")
                .expect("static encoding has no interior NUL");
        let added = unsafe {
            objc2::ffi::class_addMethod(view_class, sel.as_ptr(), Some(imp_raw), types.as_ptr())
        };
        if added {
            tracing::debug!("setFrameSize hook installed on the winit view class");
        } else {
            // Only reachable if a future winit implements the method itself:
            // never replace a winit-owned implementation (PLAN §Z-d guard).
            tracing::debug!(
                "setFrameSize hook not installed: the view class implements it; \
                 zoom channel degrades to the 1.12.7 form"
            );
        }
    }))
    .unwrap_or_else(|_| {
        tracing::error!(
            "setFrameSize hook install panicked; drawable sync is DISABLED this session"
        );
    });
}

/// The injected `setFrameSize:` IMP. Warp host_view.m:133-145 shape:
/// entry `changed` short-circuit, size validation, super FIRST (applies the
/// new size AND emits `frameDidChange:` -- the source of winit's Resized
/// events; skipping it stalls the whole resize pipeline), then the drawable
/// sync. `self` IS the target view (`setFrameSize:` is an NSView method).
unsafe extern "C" fn set_frame_size_imp(
    this: *mut objc2::runtime::AnyObject,
    _cmd: objc2::runtime::Sel,
    size: objc2_foundation::NSSize,
) {
    use objc2::msg_send;
    use objc2::ClassType;

    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        // Entry `changed` short-circuit (Warp host_view.m:134): the frame is
        // still the OLD size here -- `[super setFrameSize:]` below applies
        // the new one.
        // SAFETY: `frame` on a live view returns a by-value NSRect.
        let frame: objc2_foundation::NSRect = unsafe { msg_send![this, frame] };
        if frame.size == size {
            // SAFETY: forwarding to super is still required on the no-change
            // path (NSView bookkeeping), it just skips the drawable work.
            unsafe {
                let _: () = msg_send![
                    super(this, objc2_app_kit::NSView::class()),
                    setFrameSize: size
                ];
            }
            return;
        }
        // Size validation (Warp host_view.m:138): off-screen windows and
        // degenerate sizes must not reach the drawable.
        if !size.width.is_finite()
            || !size.height.is_finite()
            || size.width <= 0.0
            || size.height <= 0.0
        {
            return;
        }
        // Apply the new size via super FIRST: this is the primitive that
        // updates the view/layer geometry AND emits `frameDidChange:` (the
        // source of winit's Resized events). Skipping or deferring it stalls
        // the whole resize pipeline.
        // SAFETY: `super(...)` starts the method lookup at NSView (the
        // superclass), so this forwards to the base implementation instead of
        // recursing into the injected method.
        unsafe {
            let _: () = msg_send![
                super(this, objc2_app_kit::NSView::class()),
                setFrameSize: size
            ];
        }
        // Now the drawable: the bounds just took effect, so size the
        // drawable to match in the SAME main-thread callback -- the
        // compositor never sees a mismatched pair.
        // SAFETY: the layer was attached by `attach_layer_to_nsview` and
        // lives as long as the view; both messages are plain
        // setters/getters. The typed CAMetalLayer API needs the `CALayer`
        // feature we do not enable, so these stay raw msg_send (each
        // unwind-guarded above; failure degrades to the pre-fix behaviour).
        let layer: *mut objc2::runtime::AnyObject = unsafe { msg_send![this, layer] };
        if layer.is_null() {
            return;
        }
        unsafe {
            let scale: f64 = msg_send![layer, contentsScale];
            let drawable = objc2_foundation::NSSize::new(size.width * scale, size.height * scale);
            let _: () = msg_send![layer, setDrawableSize: drawable];
        }
        // PLAN Z-d C-3: callback density + sync timing are observable at
        // `RUST_LOG=debug` for the field acceptance run.
        tracing::debug!(
            width = size.width,
            height = size.height,
            "setFrameSize callback synced drawable"
        );
    }))
    .unwrap_or_else(|_| {
        tracing::error!("setFrameSize IMP panicked; drawable sync skipped for this callback");
    });
}

#[cfg(test)]
mod tests {
    use super::*;
}
