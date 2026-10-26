//! Pure decision layer for the zoom window (PLAN_zoom_sequence_ownership
//! Appendix F-3B/F-3C), split out of mod.rs under F-5's "over budget ->
//! split, no ceiling raise" rule. Each function here is truth-table tested
//! in tests.rs. Cache access goes through the parent module's private items
//! (`super::`); the main-thread-exclusivity discipline documented there
//! applies unchanged.

use std::sync::{Mutex, OnceLock};
use std::time::Instant;

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

/// Process-level step-wake deadline (v1.14.5 R1, PLAN_v1145): armed by
/// `step_self_zoom` with `last_step + STEP_MIN_INTERVAL`, consumed by
/// `zoom_wait_policy` through `zoom_control_flow`. Replaces the retired
/// request_redraw spin chain — a pacing refusal now arms a deadline instead
/// of forcing a full frame. House pattern = the parent module's FRAME_CACHE
/// (`OnceLock<Mutex<_>>`, mod.rs:180): the Mutex only crosses Rust's
/// static/borrow boundary; usage stays main-thread-exclusive like
/// everything zoom-related.
static STEP_WAIT: OnceLock<Mutex<Option<Instant>>> = OnceLock::new();

fn step_wait_slot() -> &'static Mutex<Option<Instant>> {
    STEP_WAIT.get_or_init(|| Mutex::new(None))
}

/// Arm (Some) or clear (None) the step-wake deadline. `None` is the reset
/// path for every early-exit / cancel / finish / Flush route — a stale
/// deadline must never outlive the animation (the H1 busy-loop shape).
pub(crate) fn set_zoom_step_wait(deadline: Option<Instant>) {
    *step_wait_slot()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = deadline;
}

/// The currently armed step-wake deadline, if any.
pub(crate) fn zoom_step_wait() -> Option<Instant> {
    *step_wait_slot()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The composed control flow for one `about_to_wait` pass (v1.14.5 R1):
/// winit's `ControlFlow` is translated at the call site so this type — and
/// the truth table producing it — stay headless-testable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ZoomControlFlow {
    /// Sleep until the (earliest relevant) deadline.
    WaitUntil(Instant),
    /// Resting flow; also the explicit stale-deadline reset.
    Wait,
}

/// Compose the step-pacing wake with the zoom-window decision into ONE
/// determined control flow per pass (truth-table tested; review R1-P1: every
/// branch terminates in a known state, closing the stale-WaitUntil busy
/// loop — the shape H1 already fixed at the Flush arm). `step_wait` is the
/// step-wake deadline (`None` = not armed); `hot_flush` is the hot-window
/// deadline passed in as `zoom_jump_until + 2ms` (the caller
/// (`zoom_wait_policy`) keeps that grace).
///
/// - Arm (window hot): `WaitUntil(min(step_wait, hot_flush))` — the ≤300 ms
///   hot-window deadline must NOT squash the denser step wake. Null
///   semantics: whichever deadline exists (min when both); both `None` ->
///   `Wait` as the safe default (unreachable in practice — hot implies
///   `zoom_jump_until` is Some, the caller's Arm guard keeps that
///   invariant explicit).
/// - None (nothing zoom-related this tick): `step_wait = Some(d)` ->
///   `WaitUntil(d)` — the anim's first pass's ONLY wake source, because at
///   that moment `hot` is not yet armed by the first step's Resized (review
///   P2 correction; while the window is hot the flow always walks Arm).
///   `None` -> `Wait`: the EXPLICIT reset — a stale deadline left armed
///   would busy-loop (winit WaitUntil(past) maps to
///   CFRunLoopTimerSetNextFireDate(f64::MIN), firing immediately forever).
/// - Flush: `Wait` regardless of step_wait — terminal for the whole zoom
///   sequence; the caller clears the global alongside.
pub(crate) fn zoom_control_flow(
    action: ZoomFlushAction,
    step_wait: Option<Instant>,
    hot_flush: Option<Instant>,
) -> ZoomControlFlow {
    match action {
        ZoomFlushAction::Arm => match (step_wait, hot_flush) {
            (Some(step), Some(hot)) => ZoomControlFlow::WaitUntil(step.min(hot)),
            (Some(step), None) => ZoomControlFlow::WaitUntil(step),
            (None, Some(hot)) => ZoomControlFlow::WaitUntil(hot),
            // Safe default; unreachable (hot => zoom_jump_until is Some).
            (None, None) => ZoomControlFlow::Wait,
        },
        ZoomFlushAction::None => match step_wait {
            Some(deadline) => ZoomControlFlow::WaitUntil(deadline),
            // Explicit reset — the P1 fix core; see the contract above.
            None => ZoomControlFlow::Wait,
        },
        // Terminal: nothing may re-arm behind the flush verdict.
        ZoomFlushAction::Flush => ZoomControlFlow::Wait,
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

/// Pull present-rate limiter (PLAN_zoom_drawable_stall A): the inline
/// pull must acquire drawables at most once per PULL_MIN_INTERVAL — the
/// 60 Hz period, the lowest common refresh. The 3-drawable pool is
/// returned at display cadence; an unpaced pull (field: one per 2-24 ms
/// step) outpaces returns and sends nextDrawable into its ≤1 s blocking
/// cap (13 measured 1.001-1.005 s animation freezes). At 120 Hz this
/// presents every 2nd refresh — no worse than the field's self-limited
/// ~20-30 ms-per-present baseline the user accepted.
pub(crate) const PULL_MIN_INTERVAL: std::time::Duration = std::time::Duration::from_millis(16);

/// Pull-present throttle (truth-table tested; PLAN_zoom_drawable_stall A,
/// Phase E): the limiter throttles ONE-SHOT / non-gesture pulls to one
/// drawable acquisition per PULL_MIN_INTERVAL — an unpaced pull sends
/// nextDrawable into its ≤1 s blocking cap. Live-resize and self-zoom pulls
/// stand down UPSTREAM instead (`redraw_cached_frame`'s `zoom_anim_active` /
/// stand-down checks), so no bypass parameter is needed here.
pub(crate) fn pull_present_allowed(
    last_present_at: Option<std::time::Instant>,
    now: std::time::Instant,
) -> bool {
    match last_present_at {
        None => true,
        Some(at) => now.duration_since(at) >= PULL_MIN_INTERVAL,
    }
}
