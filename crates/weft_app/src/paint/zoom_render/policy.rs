//! Pure decision layer for the zoom window (PLAN_zoom_sequence_ownership
//! Appendix F-3B/F-3C), split out of mod.rs under F-5's "over budget ->
//! split, no ceiling raise" rule. Each function here is truth-table tested
//! in tests.rs. Cache access goes through the parent module's private items
//! (`super::`); the main-thread-exclusivity discipline documented there
//! applies unchanged.

use super::{lock, pull_enabled, pull_in_progress, watermark_requests_pull};

/// The tightened animation-suppression gate (F-3B): `zoom_jump_hot`, checked
/// by the caller, is only NECESSARY for "the pull should supply this frame";
/// this adds the cache-side sufficiency, evaluated under one lock:
/// - lever off -> gate closed -> the exact 1.12.10 push-only form (E-5's
///   degrade promise stays intact);
/// - cache not populated (start-up) -> gate closed -> the first frame draws
///   immediately (no whitewash regression, the FIX_OPENCODE_STARTUP_FLASH
///   family risk);
/// - watermark == request (a caught-up drag tail edge) -> gate closed -> no
///   supplier freeze at the end of a drag;
/// - `pull_in_progress` (M-1 house discipline) is checked before any lock.
pub(crate) fn pull_can_freshen(width: f32, height: f32) -> bool {
    !pull_in_progress() && pull_enabled() && {
        let cache = lock();
        cache.pullable() && watermark_requests_pull(cache.last_presented, (width, height))
    }
}

/// The `about_to_wait` zoom-expiry decision for one tick (F-3B): while the
/// window is hot, re-arm the short WaitUntil wake and mark the one-shot
/// flush pending; at the first tick after it lapses, flush exactly once.
pub(crate) enum ZoomFlushAction {
    /// Window still hot: (re)arm the wake, mark the flush pending.
    Arm,
    /// Window lapsed with the flush pending: summarize + request the draw.
    Flush,
    /// Nothing zoom-related to do this tick.
    None,
}

/// Pure expiry truth table (tested): hot wins over pending; a lapsed window
/// flushes at most once (the caller consumes the pending flag).
pub(crate) fn zoom_flush_action(hot: bool, pending: bool) -> ZoomFlushAction {
    match (hot, pending) {
        (true, _) => ZoomFlushAction::Arm,
        (false, true) => ZoomFlushAction::Flush,
        (false, false) => ZoomFlushAction::None,
    }
}

/// Degrade verdict (F-3C truth table): two or more zoom steps with ZERO pull
/// frames means displayLayer never dispatched -- the 1.12.11 field
/// signature. Any pull frame, or a single-step window, is not a verdict.
pub(crate) fn pull_degrade_verdict(steps: u32, pull_delta: u64) -> bool {
    steps >= 2 && pull_delta == 0
}

/// Appendix G: true while a zoom window is open. The injected
/// `setFrameSize:` IMP reads this to decide whether the inline pull may
/// present -- the IMP has no access to `WindowRuntime` (zero-App-dependency
/// rule), so the arm/clear sites (`note_zoom_step` / `zoom_window_finished`
/// in the parent module) publish the window state here instead.
static ZOOM_WINDOW_ACTIVE: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// True while a zoom window is open (armed by a zoom-channel step, cleared
/// by the expiry verdict).
pub(crate) fn zoom_window_active() -> bool {
    ZOOM_WINDOW_ACTIVE.load(std::sync::atomic::Ordering::Acquire)
}

/// Arm/clear the zoom-window flag (parent-module lifecycle calls).
pub(crate) fn set_zoom_window_active(active: bool) {
    ZOOM_WINDOW_ACTIVE.store(active, std::sync::atomic::Ordering::Release);
}
