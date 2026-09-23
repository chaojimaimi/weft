//! Appendix I: the self-managed zoom animation state machine (丙方案完整形态).
//!
//! Split out of `macos_zoom` (mod.rs keeps the setFrameSize/displayLayer
//! IMPs): the 800-line budget forces the module boundary, and the anim is a
//! self-contained unit -- pure decision table + two Mutex globals + one
//! class-level ObjC hook + the acceptance rig.
//!
//! Why the interception exists: the system zoom animation runs AppKit's own
//! runloop mode. The 41 setFrameSize callbacks execute live (our IMP runs --
//! drawable sync works) but winit's user events and Resized deliveries ALL
//! batch until the animation ends, so the layout pipeline starves no matter
//! how we deliver events (H's Wake drive proved it in the field). Fix:
//! intercept `zoom:` on the winit window class (inherited from NSWindow) and
//! DON'T call super -- instead a 220ms self-managed animation runs as
//! ordinary event-loop turns (setFrame → IMP → Resized same-pass → reflow →
//! full draw), which is exactly the drag semantics the user already endorsed
//! as smooth.
//!
//! Conventions: ZOOM_RESTORE_FRAME and plan_zoom/zoom_direction speak AppKit
//! logical frame tuples (origin.x, origin.y, w, h; bottom-left origin, the
//! NSWindow `[window frame]` shape). The ZoomAnim itself speaks winit logical
//! top-left coordinates, because step_self_zoom applies through
//! `request_inner_size` + `set_outer_position` and validates against
//! `outer_position` and `outer_size`. The winit flip is
//! `y_top = main_display_height - h - y_ns` (winit window_delegate.rs
//! flip_window_screen_coordinates), mirrored by `to_winit_top_left` so all
//! AppKit/winit conversion stays in this module.

/// Logical frame tuple: (origin.x, origin.y, width, height).
pub(crate) type ZoomFrame = (f64, f64, f64, f64);

/// Zoomed/restored decision tolerance in points (plan I-2: same epsilon for
/// the zoomed test, the reentry bookkeeping and the per-step cancel check;
/// wide enough that winit's physical read-back rounding never false-trips).
pub(crate) const ZOOM_EPSILON_PT: f64 = 1.0;

/// Animation length (plan I-2: 220ms, matching the system zoom's cadence).
const ZOOM_ANIM_DURATION: std::time::Duration = std::time::Duration::from_millis(220);

/// Direction of a zoom: invocation (pure verdict, truth-tabled).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ZoomDirection {
    /// frame is NOT at visibleFrame: enlarge to visibleFrame, remember the
    /// current frame as the restore target.
    ZoomIn,
    /// frame ≈ visibleFrame within the epsilon: shrink back to the stored
    /// restore frame.
    Restore,
}

/// What the `zoom:` IMP should do (pure decision, truth-tabled; the IMP only
/// executes it -- ObjC code itself stays untestable).
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum ZoomPlan {
    /// Forward to super (system zoom): fullscreen is handled before this,
    /// so here it means "zoomed but nowhere to return to".
    ForwardToSuper,
    /// Reentry with no restore frame: cancel the running animation and stay
    /// at the current frame.
    Cancel,
    /// Run/retarget one animation segment. `restore_write` persists the
    /// pre-zoom frame (zoom-in only -- reentry/restore never touch the
    /// stored frame, review P1-4's "no swap pollution").
    Animate {
        restore_write: Option<ZoomFrame>,
        start: ZoomFrame,
        target: ZoomFrame,
    },
}

/// Self-managed animation state. Coordinates are winit logical top-left
/// (see the module block); `last_applied` is what the previous step set the
/// window to and anchors the per-step external-movement cancel check.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ZoomAnim {
    pub origin_start: (f64, f64),
    pub size_start: (f64, f64),
    pub origin_target: (f64, f64),
    pub size_target: (f64, f64),
    pub start: std::time::Instant,
    pub duration: std::time::Duration,
    pub last_applied: ZoomFrame,
    /// Pacing floor (appendix I measurement): the pipeline costs ~6 ms per
    /// step while the display refreshes every ~16.7 ms -- steps faster than
    /// that never reach the screen. `last_step` anchors the pacing floor:
    /// `zoom_step_due` (PLAN_zoom_drawable_stall B) enforces one applied
    /// step per STEP_MIN_INTERVAL.
    pub last_step: std::time::Instant,
    pub steps: u32,
}

/// The running animation, if any.
static ZOOM_ANIM: std::sync::Mutex<Option<ZoomAnim>> = std::sync::Mutex::new(None);

/// Persisted restore target in AppKit logical frame coordinates. Lives
/// across animations (clearing the anim does NOT clear the restore frame,
/// review P1-1); rewritten only by the zoom-in entry and the non-zoomed
/// refresh in the setFrameSize IMP.
static ZOOM_RESTORE_FRAME: std::sync::Mutex<Option<ZoomFrame>> = std::sync::Mutex::new(None);

fn lock_zoom_anim() -> std::sync::MutexGuard<'static, Option<ZoomAnim>> {
    ZOOM_ANIM
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

pub(crate) fn lock_restore_frame() -> std::sync::MutexGuard<'static, Option<ZoomFrame>> {
    ZOOM_RESTORE_FRAME
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Value-copy read of the running animation (main thread; Copy by design so
/// the controller never holds the global lock across AppKit calls).
pub(crate) fn zoom_anim_peek() -> Option<ZoomAnim> {
    *lock_zoom_anim()
}

#[cfg(test)]
pub(crate) fn zoom_anim_slot() -> std::sync::MutexGuard<'static, Option<ZoomAnim>> {
    lock_zoom_anim()
}

/// True while a self-managed animation is in flight. The RedrawRequested
/// suppression gate extends with this: during the animation the main path
/// (full draw + queued grid drain) is deferred wholesale -- the per-step
/// grid rewrap of a large document costs 0.4-1 s in the shrink direction
/// (field: 1.0 s stall at cols~130), which would otherwise freeze the
/// stepping loop mid-motion. The pull supplies every frame instead; the
/// coalesced drain runs once after the animation clears.
/// Direction of the in-flight animation (appendix I-6 revision): zoom-in
/// grows both dimensions -- its per-step grid reflow is the cheap MERGE
/// direction (~10-16 ms/step, field-measured), so the main path may keep
/// tracking live. Zoom-out shrinks -- the rewrap SPLIT path costs 0.4-1 s
/// at intermediate widths (field: a 1.0 s stall at cols~130), so the main
/// path must stay deferred. Mixed-dimension targets count as zoom-out
/// (conservative: defer).
/// Pure direction predicate (truth-table tested): zoom-in when neither
/// dimension shrinks ("non-shrinking"); any shrinking dimension counts as
/// zoom-out (conservative).
pub(crate) fn zoom_direction_is_in(size_start: (f64, f64), size_target: (f64, f64)) -> bool {
    size_target.0 >= size_start.0 && size_target.1 >= size_start.1
}

pub(crate) fn zoom_anim_is_zoom_in() -> bool {
    zoom_anim_peek().is_some_and(|a| zoom_direction_is_in(a.size_start, a.size_target))
}

/// Whether an animation segment is running (setFrameSize refresh gate).
pub(crate) fn zoom_anim_active() -> bool {
    lock_zoom_anim().is_some()
}

/// Stamp the applied step and bump the step counter (Appendix I: the step
/// count lives in the anim -- the controller only logs it at completion).
/// Stamps the pacing floor after a step lands (appendix I measurement).
pub(crate) fn zoom_anim_mark_stepped() {
    let mut anim = ZOOM_ANIM
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(a) = anim.as_mut() {
        a.last_step = std::time::Instant::now();
    }
}

/// Pacing floor (appendix I measurement; NOW wired by
/// PLAN_zoom_drawable_stall — 95799d2 stamped `last_step` but no check
/// ever read it). The display refreshes every ~16.7 ms (60 Hz) or ~8.3 ms
/// (120 Hz); steps applied faster than ~12 ms never reach the screen, so
/// the floor halves the per-step pipeline + present work. Interpolation
/// is elapsed-based, so a refused step's interval folds into the next
/// applied one — the trajectory stays position-exact. Also halves the
/// pull acquisition rate that Fix A throttles (belt and braces).
pub(crate) const STEP_MIN_INTERVAL: std::time::Duration = std::time::Duration::from_millis(12);

/// Pure pacing predicate (truth-table tested): a step is due when at
/// least STEP_MIN_INTERVAL has passed since the previous applied step.
pub(crate) fn zoom_step_due(elapsed: std::time::Duration) -> bool {
    elapsed >= STEP_MIN_INTERVAL
}

pub(crate) fn zoom_anim_advance(last_applied: ZoomFrame) {
    if let Some(anim) = lock_zoom_anim().as_mut() {
        anim.last_applied = last_applied;
        anim.steps = anim.steps.saturating_add(1);
    }
}

/// Take and clear the finished animation (returns the final state so the
/// controller can log the step count).
pub(crate) fn zoom_anim_finish() -> Option<ZoomAnim> {
    lock_zoom_anim().take()
}

/// Drop the running animation (external movement / live resize / teardown);
/// the restore frame is intentionally left alone (review P1-1).
pub(crate) fn zoom_anim_cancel() {
    *lock_zoom_anim() = None;
}

/// Read the persisted restore frame (AppKit logical coordinates).
pub(crate) fn zoom_restore_frame() -> Option<ZoomFrame> {
    *lock_restore_frame()
}

pub(crate) fn set_zoom_restore_frame(frame: ZoomFrame) {
    *lock_restore_frame() = Some(frame);
}

/// Smoothstep-style ease (plan I-2): slow-in/slow-out quad. Clamped outside
/// [0, 1] so a lagging/overshooting clock can never overshoot the target
/// frame past visibleFrame.
pub(crate) fn ease_in_out_quad(p: f64) -> f64 {
    if p <= 0.0 {
        0.0
    } else if p >= 1.0 {
        1.0
    } else if p < 0.5 {
        2.0 * p * p
    } else {
        1.0 - (-2.0 * p + 2.0).powi(2) / 2.0
    }
}

/// Linear interpolation step helper (pure; truth-tabled).
pub(crate) fn lerp(a: f64, b: f64, t: f64) -> f64 {
    a + (b - a) * t
}

fn frames_approx_eq(a: ZoomFrame, b: ZoomFrame, eps: f64) -> bool {
    (a.0 - b.0).abs() <= eps
        && (a.1 - b.1).abs() <= eps
        && (a.2 - b.2).abs() <= eps
        && (a.3 - b.3).abs() <= eps
}

/// Zoomed vs restore-direction verdict (plan P1-1): within the epsilon on
/// origin AND size ≈ "already at visibleFrame" → restore; anything else
/// (including partial zooms) → zoom-in.
pub(crate) fn zoom_direction(
    frame_origin: (f64, f64),
    frame_size: (f64, f64),
    vis_origin: (f64, f64),
    vis_size: (f64, f64),
    eps: f64,
) -> ZoomDirection {
    if (frame_origin.0 - vis_origin.0).abs() <= eps
        && (frame_origin.1 - vis_origin.1).abs() <= eps
        && (frame_size.0 - vis_size.0).abs() <= eps
        && (frame_size.1 - vis_size.1).abs() <= eps
    {
        ZoomDirection::Restore
    } else {
        ZoomDirection::ZoomIn
    }
}

/// Non-zoomed gate for the routine restore-frame refresh (plan review round
/// 2 P1): only when no animation runs AND the window is not sitting at its
/// zoomed geometry. The zoom-in tail Resized (anim already cleared, frame ==
/// visibleFrame) must NOT overwrite the restore frame, or zoom-out dies.
pub(crate) fn should_refresh_restore(
    anim_active: bool,
    frame: ZoomFrame,
    vis: ZoomFrame,
    eps: f64,
) -> bool {
    !anim_active && !frames_approx_eq(frame, vis, eps)
}

/// The complete zoom: decision table (pure; the IMP executes it verbatim).
/// `frame`/`vis`/`restore` are AppKit logical frames.
pub(crate) fn plan_zoom(
    anim_active: bool,
    restore: Option<ZoomFrame>,
    frame: ZoomFrame,
    vis: ZoomFrame,
    eps: f64,
) -> ZoomPlan {
    // Reentry (review P1-4): a zoom: while an animation runs retargets to
    // the restore frame WITHOUT rewriting it (no swap pollution); with no
    // restore frame there is nowhere to go -- cancel and stay put.
    if anim_active {
        return match restore {
            Some(target) => ZoomPlan::Animate {
                restore_write: None,
                start: frame,
                target,
            },
            None => ZoomPlan::Cancel,
        };
    }
    match zoom_direction(
        (frame.0, frame.1),
        (frame.2, frame.3),
        (vis.0, vis.1),
        (vis.2, vis.3),
        eps,
    ) {
        ZoomDirection::Restore => match restore {
            Some(target) => ZoomPlan::Animate {
                restore_write: None,
                start: frame,
                target,
            },
            None => ZoomPlan::ForwardToSuper,
        },
        // Zoom-in: persist the current frame as the restore target (write
        // happens exactly here, once per animation start) and aim for
        // visibleFrame.
        ZoomDirection::ZoomIn => ZoomPlan::Animate {
            restore_write: Some(frame),
            start: frame,
            target: vis,
        },
    }
}

/// AppKit→winit origin flip (pure; mirrors winit's
/// flip_window_screen_coordinates: `y_top = main_h - h - y_ns`).
pub(crate) fn to_winit_top_left(origin: (f64, f64), size: (f64, f64), main_h: f64) -> (f64, f64) {
    (origin.0, main_h - size.1 - origin.1)
}

/// Primary screen height in points -- the exact quantity winit's flip uses
/// (`CGDisplay::main().bounds().size.height`; the primary NSScreen sits at
/// global (0, 0), so its frame height equals it). None degrades the zoom:
/// invocation to the system behavior (super forward).
/// SAFETY: class lookup + plain NSScreen getters; called on the main thread
/// inside the IMP's catch_unwind.
unsafe fn primary_screen_height() -> Option<f64> {
    use objc2::msg_send;
    use objc2::rc::Retained;
    use objc2::runtime::AnyObject;
    let class = objc2::runtime::AnyClass::get("NSScreen")?;
    // SAFETY: `screens` is NSScreen's documented class property.
    let screens: *mut AnyObject = unsafe { msg_send![class, screens] };
    // Defensive lifetime scoping (rust-reviewer A3): the getter returns
    // the floor as a bare pointer; `Retained::retain` scopes a reference so
    // it is released at scope exit (install_zoom_sequence_hook's view
    // retain is the house precedent).
    let screens = unsafe { Retained::retain(screens)? };
    // SAFETY: `firstObject` on a live NSArray.
    let primary: *mut AnyObject = unsafe { msg_send![Retained::as_ptr(&screens), firstObject] };
    // Defensive lifetime scoping: same treatment for the element reference.
    let primary = unsafe { Retained::retain(primary)? };
    // SAFETY: `frame` on a live NSScreen returns a by-value NSRect.
    let frame: objc2_foundation::NSRect = unsafe { msg_send![Retained::as_ptr(&primary), frame] };
    Some(frame.origin.y + frame.size.height)
}

/// Arm one animation segment from AppKit frames to winit coordinates and
/// kick the event loop so the first step runs this pass (Appendix H's Wake).
pub(crate) fn start_zoom_anim(start: ZoomFrame, target: ZoomFrame, main_h: f64) {
    let start_tl = to_winit_top_left((start.0, start.1), (start.2, start.3), main_h);
    let target_tl = to_winit_top_left((target.0, target.1), (target.2, target.3), main_h);
    *lock_zoom_anim() = Some(ZoomAnim {
        origin_start: start_tl,
        size_start: (start.2, start.3),
        origin_target: target_tl,
        size_target: (target.2, target.3),
        start: std::time::Instant::now(),
        duration: ZOOM_ANIM_DURATION,
        last_applied: (start_tl.0, start_tl.1, start.2, start.3),
        last_step: std::time::Instant::now(),
        steps: 0,
    });
    super::notify_resize_wake();
}

/// Appendix I step 1: replace `zoom:` on the winit window class (inherited
/// from NSWindow, so `class_respondsToSelector` is EXPECTED true -- logged
/// only; the install guard is the `class_addMethod` return value, matching
/// the setFrameSize/displayLayer house convention). Call once after window
/// creation (renderer constructor: the NSWindow exists by then).
/// Guard semantics: `class_addMethod` fails only if the class itself already
/// implements `zoom:` (a future winit does its own zoom bookkeeping) -- we
/// must never replace that, behaviour degrades to the system zoom.
pub(crate) fn install_zoom_override_hook(window: &winit::window::Window) {
    use objc2::rc::Retained;
    use objc2::runtime::AnyObject;
    use objc2_app_kit::NSView;
    use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
    // M-3 (displayLayer precedent): class_addMethod is class-level and
    // idempotent-false on re-call; the flag distinguishes "our earlier
    // install" from "a foreign implementation exists".
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
    let Some(view) = (unsafe { Retained::retain(appkit.ns_view.as_ptr().cast::<NSView>()) }) else {
        return;
    };
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        // The WinitWindow INSTANCE (ns_view -> window); its class is winit's
        // private NSWindow subclass (objc2-app-kit has no such type -- raw
        // AnyObject/Class path per plan I-2).
        // SAFETY: `[view window]` on a live view; nil before attach (caller
        // ordering guarantees the window exists).
        let Some(ns_window) = (unsafe { crate::macos_window::ns_window_of(&view) }) else {
            tracing::debug!("zoom: hook: no NSWindow on the view; not installed");
            return;
        };
        if INSTALLED_BY_THIS_MODULE.swap(true, std::sync::atomic::Ordering::AcqRel) {
            tracing::debug!("zoom: hook already installed by this module");
            return;
        }
        // SAFETY: `object_getClass` on a live object always returns the
        // initialized class object.
        let window_class = unsafe {
            objc2::ffi::object_getClass(Retained::as_ptr(&ns_window) as *mut AnyObject as *mut _)
        } as *mut _;
        let sel = objc2::sel!(zoom:);
        // Expected TRUE (inherited from NSWindow) -- diagnostic only, NEVER
        // a guard (plan I-2 review P2).
        // SAFETY: pure class/selector lookup, no side effects.
        let pre_responds: bool =
            unsafe { objc2::ffi::class_respondsToSelector(window_class, sel.as_ptr()) };
        // IMP signature: `- (void)zoom:(id)sender` -> `v@:@` (void, id self,
        // SEL _cmd, id sender).
        let imp: unsafe extern "C" fn(*mut AnyObject, objc2::runtime::Sel, *mut AnyObject) =
            zoom_imp;
        // SAFETY: transmute between the typed extern "C" fn pointer and the
        // runtime's untyped `Imp` form (same pattern as the setFrameSize and
        // displayLayer hooks).
        let imp_raw: unsafe extern "C" fn() = unsafe { std::mem::transmute(imp) };
        let types: &'static std::ffi::CStr = std::ffi::CStr::from_bytes_with_nul(b"v@:@\0")
            .expect("static encoding has no interior NUL");
        // SAFETY: adding `zoom:` to winit's window class; a false return
        // means the class already implements it and we never replace that.
        let added = unsafe {
            objc2::ffi::class_addMethod(window_class, sel.as_ptr(), Some(imp_raw), types.as_ptr())
        };
        if added {
            tracing::debug!(
                pre_responds,
                "zoom: hook installed on the winit window class"
            );
        } else {
            tracing::debug!(
                "zoom: hook not installed: the window class implements it; \
                 degrading to the system zoom animation"
            );
        }
    }))
    .unwrap_or_else(|_| {
        tracing::error!("zoom: hook install panicked; system zoom animation stays in effect");
    });
}

/// The injected `zoom:` IMP. Does NOT call super for the normal path -- that
/// is the whole point: AppKit's zoom animation would suspend winit event
/// dispatch (I-1), the self-managed animation keeps every step inside the
/// event loop. Each msg_send selector is a real NSWindow/NSScreen API
/// (frame/visibleFrame/screen/styleMask -- Apple headers). Two guards:
/// catch_unwind (VULN-005 house style) stops Rust panics, and the inner
/// `objc2::exception::catch` (review MEDIUM, dock_progress precedent) stops
/// NSExceptions -- a foreign unwind escaping an extern "C" IMP aborts the
/// process before catch_unwind could see it. Either failure degrades to a
/// logged skip / system super forward, never an abort.
unsafe extern "C" fn zoom_imp(
    this: *mut objc2::runtime::AnyObject,
    _cmd: objc2::runtime::Sel,
    sender: *mut objc2::runtime::AnyObject,
) {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        // NSException guard (review MEDIUM): degrade a throw to a super
        // forward -- the system's own zoom is the sane fallback. F1
        // (review): exception::catch's closure must not panic (a Rust
        // unwind through its extern "C" trampoline aborts before this
        // IMP's catch_unwind could see it) -- the inner catch_unwind
        // takes panics; the outer only ever sees NSExceptions.
        let forwarded = std::cell::Cell::new(false);
        let thrown = unsafe {
            objc2::exception::catch(std::panic::AssertUnwindSafe(|| {
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    zoom_imp_objc(this, sender, &forwarded);
                }));
                if outcome.is_err() {
                    tracing::error!("zoom: IMP body panicked; treated as no-op");
                }
            }))
        };
        if let Err(exception) = thrown {
            tracing::error!(
                ?exception,
                "zoom: IMP threw an NSException; forwarding to super (system fallback)"
            );
            // F3 (review): skip the fallback when the body already
            // forwarded -- a double super zoom: would re-enter the system
            // animation after a half-completed state. The super forward
            // runs right after an ObjC throw -- the highest
            // foreign-unwind-risk moment -- so it keeps its own
            // exception::catch (dock_progress badge-fallback convention).
            if !forwarded.get() {
                let fallback = unsafe {
                    objc2::exception::catch(std::panic::AssertUnwindSafe(|| {
                        zoom_imp_forward_super(this, sender);
                    }))
                };
                if fallback.is_err() {
                    tracing::error!("zoom: super forward also threw; zoom invocation dropped");
                }
            }
        }
    }))
    .unwrap_or_else(|_| {
        tracing::error!("zoom: IMP panicked; zoom suppressed for this invocation");
    });
}

/// System fallback: forward `zoom:` to super (NSWindow's implementation --
/// WinitWindow's superclass), the closest "what the system would have done".
/// Used by the degenerate paths of `zoom_imp_objc` AND after a caught
/// NSException in `zoom_imp`.
unsafe fn zoom_imp_forward_super(
    this: *mut objc2::runtime::AnyObject,
    sender: *mut objc2::runtime::AnyObject,
) {
    use objc2::msg_send;
    use objc2::ClassType;
    // SAFETY: super lookup starts at NSWindow (WinitWindow's superclass),
    // forwarding instead of recursing (setFrameSize pattern).
    let _: () = unsafe { msg_send![super(this, objc2_app_kit::NSWindow::class()), zoom: sender] };
}

/// The zoom: IMP body: invocation telemetry, geometry reads (styleMask /
/// frame / screen / visibleFrame / primary-screen height), the pure plan
/// decision and the anim writes. Every failure mode must stay inside the
/// guards in `zoom_imp` (Rust panic -> catch_unwind; NSException ->
/// exception::catch).
unsafe fn zoom_imp_objc(
    this: *mut objc2::runtime::AnyObject,
    sender: *mut objc2::runtime::AnyObject,
    forwarded: &std::cell::Cell<bool>,
) {
    use objc2::msg_send;
    // Appendix I field telemetry: attribute every zoom: invocation (nil
    // sender = programmatic, e.g. winit set_maximized / the acceptance
    // rig; a class = a real NSControl like the green button). zoom: is
    // user-rare, so the string work is free in practice.
    // SAFETY: object_getClassName on a live object (or nil, allowed).
    let sender_class = if sender.is_null() {
        "nil".to_string()
    } else {
        let name = objc2::ffi::object_getClassName(sender.cast());
        if name.is_null() {
            "unknown".to_string()
        } else {
            unsafe { std::ffi::CStr::from_ptr(name) }
                .to_string_lossy()
                .into_owned()
        }
    };
    tracing::debug!(sender_class, "zoom: invoked");
    // Fullscreen fallback (plan I-2): zoom: inside fullscreen is the
    // system's own behavior -- forward untouched.
    // SAFETY: `styleMask` on a live NSWindow returns the mask by value.
    let style_mask: objc2_app_kit::NSWindowStyleMask = unsafe { msg_send![this, styleMask] };
    if style_mask.contains(objc2_app_kit::NSWindowStyleMask::FullScreen) {
        forwarded.set(true);
        zoom_imp_forward_super(this, sender);
        return;
    }
    // Geometry (plan I-2 step 2). Screen nil (no active display) ->
    // system behavior via super forward (review P3 multi-display entry).
    // SAFETY: plain NSWindow/NSScreen getters, by-value returns.
    let frame: objc2_foundation::NSRect = unsafe { msg_send![this, frame] };
    let screen: *mut objc2::runtime::AnyObject = unsafe { msg_send![this, screen] };
    if screen.is_null() {
        forwarded.set(true);
        zoom_imp_forward_super(this, sender);
        return;
    }
    let vis: objc2_foundation::NSRect = unsafe { msg_send![screen, visibleFrame] };
    // The winit flip needs the primary display height; without it the
    // animation could aim at a mirrored Y -- degrade to the system zoom.
    let Some(main_h) = (unsafe { primary_screen_height() }) else {
        forwarded.set(true);
        zoom_imp_forward_super(this, sender);
        return;
    };
    let frame_t = (
        frame.origin.x,
        frame.origin.y,
        frame.size.width,
        frame.size.height,
    );
    let vis_t = (vis.origin.x, vis.origin.y, vis.size.width, vis.size.height);
    // Decision (pure, truth-tabled): direction, reentry, restore
    // bookkeeping all resolved here in one value.
    let plan = plan_zoom(
        zoom_anim_active(),
        zoom_restore_frame(),
        frame_t,
        vis_t,
        ZOOM_EPSILON_PT,
    );
    match plan {
        ZoomPlan::ForwardToSuper => {
            // Zoomed with no remembered restore frame (e.g. pre-hook
            // zoom): the system's own zoom is the only sane answer.
            forwarded.set(true);
            zoom_imp_forward_super(this, sender);
        }
        ZoomPlan::Cancel => {
            // Reentry with nowhere to go: stay at the current frame.
            zoom_anim_cancel();
            tracing::debug!("zoom: reentry without restore frame; animation cancelled");
        }
        ZoomPlan::Animate {
            restore_write,
            start,
            target,
        } => {
            // Persist the pre-zoom frame exactly once per zoom-in (the
            // plan's restore_write arm; reentry/restore pass None so the
            // stored frame never swaps mid-flight).
            if let Some(restore) = restore_write {
                set_zoom_restore_frame(restore);
            }
            start_zoom_anim(start, target, main_h);
            tracing::debug!(
                from = ?start,
                to = ?target,
                "zoom: self-managed animation armed (220ms, event-loop stepped)"
            );
        }
    }
}

// ============================================================================
// Appendix I local acceptance rig (formal verification hook, kept in-tree --
// unlike the temporary H-era probes). WEFT_SELF_ZOOM_TEST=1 makes
// about_to_wait fire one `[ns_window zoom:]` ~2s after startup, driving the
// injected IMP through the full self-managed animation without user input.
// Inert (a string compare per pass) unless the env var is set.

// ============================================================================
static SELF_ZOOM_TEST_START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();

static SELF_ZOOM_TEST_FIRED: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// One no-op tick unless the rig is armed; fires `zoom:` twice (2s zoom-in,
/// 6s zoom-out -- the restore direction) when the rig is armed. Called from
/// step_self_zoom (window_event_controller).
pub(crate) fn self_zoom_test_tick(window: &winit::window::Window) {
    use objc2::rc::Retained;
    use objc2::runtime::AnyObject;
    use objc2_app_kit::NSView;
    use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
    if std::env::var_os("WEFT_SELF_ZOOM_TEST").as_deref() != Some(std::ffi::OsStr::new("1")) {
        return;
    }
    let started = *SELF_ZOOM_TEST_START.get_or_init(std::time::Instant::now);
    let n = SELF_ZOOM_TEST_FIRED.load(std::sync::atomic::Ordering::Acquire);
    let base = std::env::var_os("WEFT_SELF_ZOOM_TEST_DELAY")
        .and_then(|v| v.into_string().ok())
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(2);
    let due = match n {
        0 => started.elapsed() >= std::time::Duration::from_secs(base),
        1 => started.elapsed() >= std::time::Duration::from_secs(base + 4),
        _ => false,
    };
    if !due {
        return;
    }
    SELF_ZOOM_TEST_FIRED.store(n + 1, std::sync::atomic::Ordering::Release);
    let Ok(handle) = window.window_handle() else {
        return;
    };
    let RawWindowHandle::AppKit(appkit) = handle.as_raw() else {
        return;
    };
    // SAFETY: retain of the live winit-owned NSView (house pattern).
    let Some(view) = (unsafe { Retained::retain(appkit.ns_view.as_ptr().cast::<NSView>()) }) else {
        return;
    };
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        use objc2::msg_send;
        // SAFETY: `[view window]` on a live view (attach ordering guarantees
        // the NSWindow exists).
        let Some(ns_window) = (unsafe { crate::macos_window::ns_window_of(&view) }) else {
            tracing::error!("self-zoom test rig: no NSWindow; rig skipped");
            return;
        };
        // SAFETY: `zoom:` on a live NSWindow -- our injected IMP intercepts
        // it (nil sender, the same shape winit's set_maximized sends).
        let nil_sender: *mut AnyObject = std::ptr::null_mut();
        let _: () = unsafe { msg_send![Retained::as_ptr(&ns_window), zoom: nil_sender] };
        tracing::info!("self-zoom test rig: zoom: fired");
    }))
    .unwrap_or_else(|_| {
        tracing::error!("self-zoom test rig panicked");
        SELF_ZOOM_TEST_FIRED.store(n, std::sync::atomic::Ordering::Release);
    });
}
