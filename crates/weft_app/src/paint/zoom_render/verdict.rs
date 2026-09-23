//! Zoom-window verdict/observation state (PLAN_zoom_sequence_ownership
//! Appendix F-3C), split out of mod.rs under F-5's "over budget -> split, no
//! ceiling raise" rule (mod.rs sat exactly at the 800-line gate). The
//! watch/verdict counters are one cohesion: opened by the first zoom-channel
//! step, closed by the deterministic WaitUntil expiry, keyed off the
//! presented-pull count. Threading matches the parent module (main-thread
//! exclusive; the Mutex crosses static/borrow boundaries, not contention).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::Instant;

// The degrade verdict comes from the pure decision layer (policy.rs) through
// the parent's re-export -- the same name path the pre-split module used.
use super::pull_degrade_verdict;

/// Pull frames presented since process start (the zoom-window verdict keys
/// off this counter). Incremented only via `record_presented_pull`.
static PULL_PRESENT_COUNT: AtomicU64 = AtomicU64::new(0);
/// Pull count at the last zoom-window close: the verdict baseline for the
/// next window (a window-open baseline would swallow the burst's pulls).
static LAST_CLOSE_COUNT: AtomicU64 = AtomicU64::new(0);
/// Zoom-window watch `(start, pull baseline, steps)` (Appendix F-3C):
/// opened by the first zoom-channel step (`note_zoom_step`), closed by the
/// deterministic WaitUntil expiry (`zoom_window_finished`) -- the old
/// per-callback debug poll was invisible at the default `filter=info`
/// (F-1 leg 4).
static ZOOM_WATCH: Mutex<Option<(Instant, u64, u32)>> = Mutex::new(None);

fn zoom_watch() -> MutexGuard<'static, Option<(Instant, u64, u32)>> {
    ZOOM_WATCH
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Bump the presented-pull counter (the parent's `redraw_cached_frame` calls
/// this after a successful present; the counter lives here beside its only
/// reader, `zoom_window_finished`).
pub(crate) fn record_presented_pull() {
    PULL_PRESENT_COUNT.fetch_add(1, Ordering::Release);
}

/// Per-step watch update (F-3C, Resized arm): the first zoom-channel step
/// opens the window (steps = 1, baseline = the pull count before the
/// animation); later steps extend the CURRENT window -- overlapping windows
/// join the live one and the baseline keeps its pre-animation value.
pub(crate) fn note_zoom_step() {
    let mut watch = zoom_watch();
    if let Some((_, _, steps)) = watch.as_mut() {
        *steps += 1;
    } else {
        // G-3 baseline: the burst's pulls land before the first Resized, so
        // the baseline is the previous window's close count, not the count
        // at open (1.12.14 field bug: 41 pulls, verdict said zero).
        let baseline = LAST_CLOSE_COUNT.load(Ordering::Acquire);
        *watch = Some((Instant::now(), baseline, 1));
    }
}

/// Close the watch and issue the verdict (F-3C, from zoom_wait_policy's
/// expiry branch): a degraded window warns that displayLayer never
/// dispatched; otherwise the window settles with `zoom settled` --
/// M > 0 is the field acceptance signal that the pull actually works.
/// `steps >= 1` always holds for a live watch (`note_zoom_step` opens at 1),
/// so no separate guard is needed (review L2).
pub(crate) fn zoom_window_finished() {
    let mut watch = zoom_watch();
    let Some((_start, baseline, steps)) = *watch else {
        return;
    };
    *watch = None;
    let count = PULL_PRESENT_COUNT.load(Ordering::Acquire);
    let pull_delta = count.saturating_sub(baseline);
    LAST_CLOSE_COUNT.store(count, Ordering::Release);
    if pull_degrade_verdict(steps, pull_delta) {
        tracing::warn!(
            "zoom window ended degraded: steps={} pull_frames={} -- the inline pull never \
             presented (check pull gate / frame-cache state)",
            steps,
            pull_delta
        );
    } else {
        tracing::info!("zoom settled: steps={} pull_frames={}", steps, pull_delta);
    }
}
