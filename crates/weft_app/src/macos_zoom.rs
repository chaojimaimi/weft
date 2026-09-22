//! Programmatic-zoom sequence ownership (PLAN_zoom_sequence_ownership Z-d)
//! and the displayLayer pull wiring (Appendix E-2, Z-f).
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
        // `scale` is hoisted so the inline pull below can convert the
        // logical `size` into physical pixels for the watermark comparison
        // (review M-1: `last_presented` is physical; a logical request
        // would never dedupe on Retina).
        let scale: f64 = unsafe { msg_send![layer, contentsScale] };
        unsafe {
            let drawable = objc2_foundation::NSSize::new(size.width * scale, size.height * scale);
            let _: () = msg_send![layer, setDrawableSize: drawable];
        }
        // PLAN_zoom appendix G (field round 3): supply the animation frame
        // HERE. `displayLayer:` never dispatched in this environment -- the
        // probe rig proved the chain otherwise complete (makeBackingLayer
        // adoption, delegate == view, needsDisplay == true across ~150
        // commits; field: 41-step zoom, zero pulls, both with and without
        // managed hosting) -- so the trigger cannot be waited on. The
        // injected `setFrameSize:` IS the per-step callback (it runs on
        // every zoom step by construction), so the pull presents from here,
        // bound to the current CA transaction: bounds and pixels commit
        // atomically, no one-beat gap for the compositor to stretch.
        // Gate = the zoom-window flag AND the same freshen predicate the
        // draw-suppression uses, so drags (flag false -- armed only outside
        // a live-resize gesture) never take the extra present, start-up
        // (cache empty) never presents a phantom, and the lever stays
        // authoritative. The displayLayer: delegate + A1 bounds pin remain
        // armed as an inert bonus trigger: should CA ever dispatch it, its
        // watermark dedup makes a double present a no-op.
        if crate::paint::zoom_render::zoom_window_active() {
            // Physical pixels for the watermark comparison (review M-1):
            // `last_presented` is stamped from drawable texture sizes.
            let (pw, ph) = (size.width * scale, size.height * scale);
            let fresh = crate::paint::zoom_render::pull_can_freshen(pw as f32, ph as f32);
            if fresh {
                crate::paint::zoom_render::bind_pull_transaction();
                let presented =
                    crate::paint::zoom_render::redraw_cached_frame(size.width, size.height);
                crate::paint::zoom_render::unbind_pull_transaction();
                tracing::debug!(presented, "setFrameSize inline pull");
            }
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

/// PLAN_zoom Z-f (Appendix E-2): add `displayLayer:` to the winit view class
/// and wire the view as its Metal layer's delegate -- the Warp pull-model
/// pair (host_view.m:147-151 `displayLayer:` -> `warp_update_layer`). Called
/// AFTER the renderer attached the layer (constructor.rs): the delegate and
/// the redraw policy pin need the live layer, which does not exist yet where
/// `install_zoom_sequence_hook` runs (pre-`MetalRenderer::new`).
///
/// `displayLayer:` is a CALayerDelegate method, NOT an NSView lifecycle
/// method: the base NSView class has no implementation, so there is no super
/// obligation and nothing to forward to (logged pre-install via
/// `class_respondsToSelector` -- the stage-B first acceptance item). Once the
/// class implements it, layer content updates route through the delegate
/// instead of `drawRect:` -- THAT is the real switch; the
/// `layerContentsRedrawPolicy` write below is a defensive pin of the
/// documented default (`RedrawDuringViewResize`), not a behavior change.
pub(crate) fn install_display_layer_hook(window: &winit::window::Window) {
    use objc2::rc::Retained;
    use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};

    // M-3: class_addMethod is class-level and idempotent-false on re-call --
    // without this flag a second invocation (a second window/renderer) would
    // misread its OWN earlier install as "winit implements it" and silently
    // skip the delegate/policy arming the new layer needs.
    static INSTALLED_BY_THIS_MODULE: std::sync::atomic::AtomicBool =
        std::sync::atomic::AtomicBool::new(false);

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
        use objc2::msg_send;
        use objc2::runtime::AnyObject;
        use objc2_app_kit::NSViewLayerContentsRedrawPolicy;

        if !INSTALLED_BY_THIS_MODULE.load(std::sync::atomic::Ordering::Acquire) {
            // SAFETY: `object_getClass` on a live object always returns the
            // initialized class object.
            let view_class = unsafe {
                objc2::ffi::object_getClass(Retained::as_ptr(&view) as *mut AnyObject as *mut _)
            } as *mut _;
            let sel = objc2::sel!(displayLayer:);
            // SAFETY: class/selector pair from the live view's class; a pure
            // lookup with no side effects. (objc-sys defines BOOL as `bool`
            // on arm64 -- this tree's only target.)
            let pre_responds: bool =
                unsafe { objc2::ffi::class_respondsToSelector(view_class, sel.as_ptr()) };
            // IMP signature: `- (void)displayLayer:(CALayer *)layer` ->
            // `v@:@` (void return, id self, SEL _cmd, id layer).
            let imp: unsafe extern "C" fn(*mut AnyObject, objc2::runtime::Sel, *mut AnyObject) =
                display_layer_imp;
            // SAFETY: transmute between the typed extern "C" fn pointer and
            // the runtime's untyped `Imp` form is sound for the Objective-C
            // calling convention (same pattern as the setFrameSize hook).
            let imp_raw: unsafe extern "C" fn() = unsafe { std::mem::transmute(imp) };
            let types: &'static std::ffi::CStr = std::ffi::CStr::from_bytes_with_nul(b"v@:@\0")
                .expect("static encoding has no interior NUL");
            // SAFETY: adding `displayLayer:` to the winit view class. Guard
            // semantics per the Z-d convention: succeeds only because the
            // class itself lacks the selector; a false return here (we are
            // NOT the installer -- checked above) means a future winit
            // implements it and we must never replace that implementation.
            let added = unsafe {
                objc2::ffi::class_addMethod(view_class, sel.as_ptr(), Some(imp_raw), types.as_ptr())
            };
            if !added {
                tracing::debug!(
                    "displayLayer hook not installed: the view class implements it; \
                     pull degrades to the 1.12.10 push form"
                );
                return;
            }
            INSTALLED_BY_THIS_MODULE.store(true, std::sync::atomic::Ordering::Release);
            tracing::debug!(
                pre_responds,
                "displayLayer hook installed on the winit view class"
            );
        } else {
            tracing::debug!(
                "displayLayer hook already installed by this module; \
                 re-arming delegate/policy for this window's layer"
            );
        }
        // Defensive pin (E-2, review P1 correction): Apple's documented
        // default for layerContentsRedrawPolicy IS RedrawDuringViewResize;
        // this write guards against future changes and documents intent.
        // SAFETY: typed objc2-app-kit setter on the live view.
        unsafe {
            view.setLayerContentsRedrawPolicy(
                NSViewLayerContentsRedrawPolicy::NSViewLayerContentsRedrawDuringViewResize,
            );
        }
        // layer.delegate = view. CALayer.delegate is unretained (assign):
        // the view owns the layer, so the reference can never dangle.
        // SAFETY: `layer` on a live view returns the attached CAMetalLayer
        // (renderer construction has already attached it); plain getter.
        let layer: *mut AnyObject = unsafe { msg_send![Retained::as_ptr(&view), layer] };
        if layer.is_null() {
            tracing::debug!("displayLayer hook: layer not attached; delegate not set");
            return;
        }
        // SAFETY: plain `setDelegate:` on the live layer; the unretained
        // delegate (the view) outlives it.
        unsafe {
            let _: () = msg_send![layer, setDelegate: Retained::as_ptr(&view)];
        }
        // PLAN_zoom appendix F-3A (A1, the 1.12.11 field-failure ROOT
        // cause): CALayer's needsDisplayOnBoundsChange defaults to NO -- a
        // bounds change does NOT mark the layer dirty, so CA never enters a
        // display cycle and displayLayer: is NEVER dispatched (the stage-B
        // experiment: install logs all green, zero callbacks). Warp pins the
        // same flag inside makeBackingLayer (host_view.m:281) rather than
        // trusting the RedrawDuringViewResize policy alone. Field appendix G:
        // CA stayed silent even with every input in place, so the pull now
        // presents inline from the setFrameSize IMP; this pin remains as the
        // inert trigger should a future macOS dispatch the delegate.
        // SAFETY: plain setter on the live layer.
        unsafe {
            let _: () = msg_send![layer, setNeedsDisplayOnBoundsChange: true];
        }
        tracing::debug!("displayLayer delegate armed: view -> metal layer");
    }))
    .unwrap_or_else(|_| {
        tracing::error!("displayLayer hook install panicked; pull DISABLED this session");
    });
}

/// The injected `displayLayer:` IMP (CALayerDelegate). Warp host_view.m
/// shape: synchronous content supply when CA asks for it. Five steps (E-2):
/// reentry guard, watermark dedup, transaction bind, lean present, unbind.
/// `self` is the winit view; the layer argument is its CAMetalLayer.
unsafe extern "C" fn display_layer_imp(
    this: *mut objc2::runtime::AnyObject,
    _cmd: objc2::runtime::Sel,
    layer: *mut objc2::runtime::AnyObject,
) {
    use objc2::msg_send;

    let _ = this; // no super obligation: displayLayer: is not an NSView lifecycle method
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        if !crate::paint::zoom_render::pull_enabled() || layer.is_null() {
            return;
        }
        // M-1: this check MUST precede any cache access. The cache Mutex is
        // not reentrant and the in-flight lean encode holds it across ObjC
        // calls that can pump a nested displayLayer callback -- a nested
        // `pull_requests_frame` lock would deadlock the main thread.
        if crate::paint::zoom_render::pull_in_progress() {
            tracing::debug!("displayLayer pull skipped: a lean present is in flight");
            return;
        }
        // Requested size = the layer's drawableSize (physical px; the
        // setFrameSize hook keeps it in sync with the bounds).
        // SAFETY: `drawableSize` on a live CAMetalLayer returns by value.
        let size: objc2_foundation::NSSize = unsafe { msg_send![layer, drawableSize] };
        if !size.width.is_finite()
            || !size.height.is_finite()
            || size.width <= 0.0
            || size.height <= 0.0
        {
            return;
        }
        // Watermark dedup (E-3): a frame at this drawable size is already on
        // screen -- presenting again would overwrite a fresh frame with the
        // same content (and during a nested flush would rewind one step).
        if !crate::paint::zoom_render::pull_requests_frame(size.width, size.height) {
            tracing::debug!(
                width = size.width,
                height = size.height,
                "displayLayer pull skipped: watermark current"
            );
            return;
        }
        // Bind the lean present to the CURRENT CA transaction so bounds and
        // pixels commit atomically (E-2 pull present semantics), present the
        // cached frame at the requested size, then unbind (IMP-exit reset).
        crate::paint::zoom_render::bind_pull_transaction();
        let presented = crate::paint::zoom_render::redraw_cached_frame(size.width, size.height);
        crate::paint::zoom_render::unbind_pull_transaction();
        // E-4 acceptance: callback density is observable at RUST_LOG=debug.
        tracing::debug!(
            presented,
            width = size.width,
            height = size.height,
            "displayLayer pull"
        );
    }))
    .unwrap_or_else(|_| {
        tracing::error!("displayLayer IMP panicked; pull skipped for this callback");
    });
}
