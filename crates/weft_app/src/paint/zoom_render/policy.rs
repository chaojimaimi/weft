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

/// Resize-cascade force-commit bit (Appendix I-6, revised by
/// PLAN_zoom_drawable_stall Phase C): during a self-managed zoom animation
/// in EITHER direction background panes commit per step, or a live
/// background TUI stays at its old width for the whole animation (field:
/// split + top intermediate state). The elapsed-debounce leg is composed
/// by the caller; this is only the force bit.
pub(crate) fn cascade_force_commit(anim_active: bool) -> bool {
    anim_active
}

/// Diagnostic-build threshold predicate (field forensics 2026-09-23): the
/// occasional ~1 s mid-animation stall must name its home -- a step GAP over
/// the floor means the loop failed to wake between passes; a per-step cost
/// over the floor means the AppKit setter section (the stepper passes the
/// combined request_inner_size + set_outer_position elapsed; `inner_us` in
/// the same log decomposes it) or the inline-pull CA commit (a single call
/// at the IMP site) blocked. Floors: gap 50 ms (field-normal gaps are
/// 2-24 ms), cost 20 ms (setters and the pull normally cost microseconds to
/// low single-digit ms). Strictly-greater so the boundary values stay quiet.
pub(crate) fn zoom_diag_anomalous(gap_us: u64, call_us: u64) -> bool {
    gap_us > 50_000 || call_us > 20_000
}

/// Pull present-rate limiter (PLAN_zoom_drawable_stall A): the inline
/// pull must acquire drawables at most once per PULL_MIN_INTERVAL — the
/// 60 Hz period, the lowest common refresh. The 3-drawable pool is
/// returned at display cadence; an unpaced pull (field: one per 2-24 ms
/// step) outpaces returns and sends nextDrawable into its ≤1 s blocking
/// cap (13 measured 1.001-1.005 s animation freezes). At 120 Hz this
/// presents every 2nd refresh — no worse than the field's self-limited
/// ~20-30 ms-per-present baseline the user accepted.
pub(crate) const PULL_MIN_INTERVAL: std::time::Duration = std::time::Duration::from_millis(16);

/// Pure throttle predicate (truth-table tested): a pull may acquire a
/// drawable when there is no previous present, or when at least
/// PULL_MIN_INTERVAL has passed since it.
pub(crate) fn pull_present_allowed(
    last_present_at: Option<std::time::Instant>,
    now: std::time::Instant,
) -> bool {
    match last_present_at {
        None => true,
        Some(at) => now.duration_since(at) >= PULL_MIN_INTERVAL,
    }
}
