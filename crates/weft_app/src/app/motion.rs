//! v1.14.0 (PLAN_v1140 T0): the motion infrastructure hub.
//!
//! One place owns the reusable tween/easing primitives every v1.14.0
//! animation rides: overlay reveals (T1), sidebar width (T2), the tab
//! indicator + drag yield (T3), hover fades (T4). Constraints from the plan:
//!
//! - No continuous rendering: progress is pulled per-frame by the existing
//!   draw path; a single counter-gated sleep-loop tick thread (app_runtime,
//!   sibling of the blink/spinner/autoscroll timers) posts `AppEvent::Wake`
//!   at 16ms only while at least one [`Tween`] is alive, dropping to a
//!   120ms idle probe otherwise.
//! - The live-tween counter is RAII: [`Tween::new`] bumps [`MOTION_LIVE`],
//!   `Drop` decrements. Any clear/replace/batch-drop path pairs up
//!   automatically — a hand-rolled `fetch_sub` can be forgotten and would
//!   leave the tick thread spinning forever.
//! - Reduce Motion has exactly one gate: [`gated`]. A `false` return means
//!   the caller must snap straight to the end value (reveal = 1, width =
//!   target, indicator = target, hover = to-color) without creating a tween.
//!   The on/off flip is bridged at the 1Hz appearance poll
//!   (lifecycle_controller, same update point as `set_zoom_reduce_motion`),
//!   where each owning state replaces in-flight tweens with their targets.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

/// Live-tween counter. Relaxed suffices: the tick thread only needs a hint
/// for its sleep period (a stale read costs one extra 120ms probe) and both
/// writer and reader run without cross-value ordering expectations.
pub(crate) static MOTION_LIVE: AtomicUsize = AtomicUsize::new(0);

/// Number of currently alive tweens — the tick thread's activity gate
/// (`> 0` ⇒ 16ms wake cadence, `0` ⇒ 120ms idle probe).
pub(crate) fn live_tweens() -> usize {
    MOTION_LIVE.load(Ordering::Relaxed)
}

/// THE single Reduce Motion entry for every new v1.14.0 animation. Returns
/// true when motion effects may animate; false means the caller snaps the
/// owning state straight to its end value instead of creating a tween.
#[inline]
pub(crate) fn gated(reduce_motion: bool) -> bool {
    !reduce_motion
}

/// ease-out cubic: `1-(1-t)^3` with `t` clamped to [0, 1]. Fast start,
/// gentle landing — the plan's single curve for every v1.14.0 animation.
#[inline]
pub(crate) fn ease_out_cubic(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    1.0 - (1.0 - t).powi(3)
}

/// Single-channel tween: `from` → `to`, starting at `start`, duration `dur`,
/// eased with [`ease_out_cubic`]. `Instant` is injected by the caller so
/// tests drive progress with explicit instants (no clock reads in
/// `value_at`).
///
/// RAII bookkeeping: construction bumps [`MOTION_LIVE`], drop decrements it.
/// Hold tweens in the state that owns the animated property; a plain scope
/// exit, `Vec::clear`, or struct replacement drops them in pairs.
#[derive(Debug)]
pub(crate) struct Tween {
    from: f32,
    to: f32,
    start: Instant,
    dur: Duration,
}

impl Tween {
    pub(crate) fn new(from: f32, to: f32, dur: Duration, start: Instant) -> Self {
        MOTION_LIVE.fetch_add(1, Ordering::Relaxed);
        Self {
            from,
            to,
            start,
            dur,
        }
    }

    /// Eased value at `now`. Clamped: at/before `start` → `from`, at/after
    /// `start + dur` → `to` (a zero duration lands on `to` immediately,
    /// matching [`Self::finished`]).
    pub(crate) fn value_at(&self, now: Instant) -> f32 {
        let elapsed = now.saturating_duration_since(self.start);
        if elapsed >= self.dur {
            return self.to;
        }
        let t = elapsed.as_secs_f32() / self.dur.as_secs_f32();
        self.from + (self.to - self.from) * ease_out_cubic(t)
    }

    /// True when the tween has reached its target at `now`.
    pub(crate) fn finished(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.start) >= self.dur
    }

    /// The end value — the snap target for Reduce Motion and finish paths.
    /// Not yet consumed in production (the v1.14.0 batches snap via
    /// captured end-state values instead of reading the tween), so the
    /// binary crate would flag it dead; kept because it is part of the
    /// plan's §1.1 Tween contract and the inline tests pin it.
    #[cfg(test)]
    pub(crate) fn target(&self) -> f32 {
        self.to
    }
}

impl Drop for Tween {
    fn drop(&mut self) {
        MOTION_LIVE.fetch_sub(1, Ordering::Relaxed);
    }
}

/// T1 reveal timings (PLAN_v1140 §2.1): open fades in over 120ms, close
/// fades out over 100ms.
pub(crate) const REVEAL_OPEN_DUR: Duration = Duration::from_millis(120);
pub(crate) const REVEAL_CLOSE_DUR: Duration = Duration::from_millis(100);

/// T2 sidebar width timings (PLAN_v1140 §3): open sweeps 0→target over
/// 180ms, close sweeps current→0 over 150ms.
pub(crate) const SIDEBAR_OPEN_DUR: Duration = Duration::from_millis(180);
pub(crate) const SIDEBAR_CLOSE_DUR: Duration = Duration::from_millis(150);

/// v1.14.1 (F3): 8ms apply-gate for the sidebar width animation's per-frame
/// override write + reflow (PLAN_v1141 §3.2.2). Aligns the animation-driven
/// reflow cadence with the drag path's PTY-commit throttle by reusing
/// effect.rs's [`crate::effect::RESIZE_COMMIT_MIN_INTERVAL`] (常量复用 — no
/// second magic number). The tween VALUE is still computed every frame; only
/// the apply is gated. `last == None` (fresh tween) applies immediately; a
/// finished tween always applies (终值必写) so landing never stalls. Pure —
/// unit-tested without App state.
pub(crate) fn should_apply(now: Instant, last: Option<Instant>, finished: bool) -> bool {
    finished
        || last.map_or(true, |stamp| {
            now.saturating_duration_since(stamp) >= crate::effect::RESIZE_COMMIT_MIN_INTERVAL
        })
}

/// T3 tab indicator timings (PLAN_v1140 §4): the amber underline slides to
/// the new active tab over 140ms; a non-dragged tab yields its slot over
/// 100ms.
pub(crate) const TAB_INDICATOR_DUR: Duration = Duration::from_millis(140);
pub(crate) const TAB_SHIFT_DUR: Duration = Duration::from_millis(100);

/// T4 hover fade timing (PLAN_v1140 §5): every hover color transition runs
/// 100ms, both directions.
pub(crate) const HOVER_FADE_DUR: Duration = Duration::from_millis(100);

/// T4: identity of a hoverable element. One shared enum serves both fade
/// registries (tab-bar entries only ever carry `Tab*` keys; the
/// interaction registry carries the block/scrollbar keys).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HoverKey {
    /// A tab row's hover background.
    TabRow(usize),
    /// The "+" (new tab) button.
    TabPlus,
    TabArrowLeft,
    TabArrowRight,
    /// A tab's close "×" (fade-in of the icon on a hovered inactive tab).
    TabClose(usize),
    /// A block header's hover state (icon brightness ramp).
    BlockHover(weft_core::blocks::BlockId),
    /// Per-button precise-hover surface (copy/fold/diagnose).
    BlockActionCopy(weft_core::blocks::BlockId),
    BlockActionFold(weft_core::blocks::BlockId),
    BlockActionDiagnose(weft_core::blocks::BlockId),
    /// The block-view scrollbar thumb (width + color ramp).
    ScrollbarThumb,
}

/// A single hover fade: a progress tween in [0, 1] between the element's
/// idle (0) and hover (1) visuals.
///
/// Progress-based by design (plan sketch said `{ key, from, to, start }`
/// with color endpoints): the PAINT SITE owns the two endpoint colors and
/// lerps by the projected progress, so `from = 当前显示色` holds by
/// construction and a mid-flight retarget starts from the drawn value —
/// the plan's no-jump continuity rule — without threading colors through
/// the projection chain.
#[derive(Debug)]
pub(crate) struct HoverFadeEntry {
    /// Settled progress when no tween is running.
    progress: f32,
    /// Where the entry is heading (1.0 hovered / 0.0 idle) — guards the
    /// per-frame re-sync against restarting an in-flight tween.
    target: f32,
    tween: Option<Tween>,
}

/// Registry of active hover fades, keyed by element. Two instances exist:
/// one on `TabBarState` (tab keys) and one on `InteractionState`
/// (block/scrollbar keys) — the plan's 借用面 split.
#[derive(Debug, Default)]
pub(crate) struct HoverFades {
    entries: Vec<(HoverKey, HoverFadeEntry)>,
}

impl HoverFades {
    /// Per-frame sync of one key's desired hover state. Safe to call every
    /// frame: an entry already heading to the desired target is left alone
    /// (a mid-flight tween keeps running from its drawn value), only a
    /// target CHANGE retargets.
    pub(crate) fn sync_target(&mut self, key: HoverKey, on: bool, now: Instant, animated: bool) {
        let target = if on { 1.0 } else { 0.0 };
        match self.entries.iter_mut().find(|(k, _)| *k == key) {
            Some((_, entry)) => {
                if entry.target == target {
                    return;
                }
                entry.target = target;
                entry.tween =
                    animated.then(|| Tween::new(entry.progress, target, HOVER_FADE_DUR, now));
                if !animated {
                    entry.progress = target;
                }
            }
            None => {
                if !on {
                    return; // idle and untracked — nothing to do
                }
                let (progress, tween) = if animated {
                    (0.0, Some(Tween::new(0.0, 1.0, HOVER_FADE_DUR, now)))
                } else {
                    (1.0, None)
                };
                self.entries.push((
                    key,
                    HoverFadeEntry {
                        progress,
                        target: 1.0,
                        tween,
                    },
                ));
            }
        }
    }

    /// Projected progress for `key` — the paint site lerps its idle/hover
    /// colors by this. Absent entry = 0.0 (idle, the 即时色 fallback).
    /// Test-only: the draw side consumes the projected snapshot via
    /// [`fade_progress`] (the registry lives App-side, paint reads a Vec).
    #[cfg(test)]
    pub(crate) fn progress(&self, key: HoverKey, now: Instant) -> f32 {
        match self.entries.iter().find(|(k, _)| *k == key) {
            Some((_, entry)) => match &entry.tween {
                Some(tween) => tween.value_at(now),
                None => entry.progress,
            },
            None => 0.0,
        }
    }

    /// Maintenance: settle finished tweens into their progress, then drop
    /// fully-faded-out entries. A settled hover (progress 1, no tween) is
    /// KEPT — it costs no tick wakes and the fallback must stay 1.0.
    pub(crate) fn maintain(&mut self, now: Instant) {
        for (_, entry) in self.entries.iter_mut() {
            if let Some(tween) = &entry.tween {
                if tween.finished(now) {
                    entry.progress = entry.target;
                    entry.tween = None;
                }
            }
        }
        self.entries
            .retain(|(_, entry)| entry.tween.is_some() || entry.progress > 0.0);
    }

    /// Projected (key, progress) snapshot for the draw side.
    pub(crate) fn project(&self, now: Instant) -> Vec<(HoverKey, f32)> {
        self.entries
            .iter()
            .map(|(key, entry)| {
                let progress = match &entry.tween {
                    Some(tween) => tween.value_at(now),
                    None => entry.progress,
                };
                (*key, progress)
            })
            .collect()
    }

    /// Currently tracked keys (the sync pass falses every tracked key that
    /// is no longer hovered — a block that lost hover must fade out).
    pub(crate) fn keys(&self) -> Vec<HoverKey> {
        self.entries.iter().map(|(key, _)| *key).collect()
    }

    /// Drop entries whose key fails `keep` (rust-reviewer P3: a closed tab's
    /// TabRow/TabClose entry would otherwise sit at progress 1.0 forever, and
    /// a NEW tab reusing that index would inherit the stale full highlight —
    /// out-of-range keys are removed outright, not faded).
    pub(crate) fn retain(&mut self, keep: impl Fn(&HoverKey) -> bool) {
        self.entries.retain(|(key, _)| keep(key));
    }

    /// Live-tween count guard for tests.
    #[cfg(test)]
    fn tween_count(&self) -> usize {
        self.entries
            .iter()
            .filter(|(_, e)| e.tween.is_some())
            .count()
    }
}

/// Paint-side lookup: progress of `key` in a projected snapshot.
#[inline]
pub(crate) fn fade_progress(fades: &[(HoverKey, f32)], key: HoverKey) -> f32 {
    fades
        .iter()
        .find(|(k, _)| *k == key)
        .map(|(_, progress)| *progress)
        .unwrap_or(0.0)
}

/// Shared overlay reveal state machine (T1): open → (open tween 0→1) →
/// steady → Closing (close tween 1→0) → closed.
///
/// Ownership model (PLAN_v1140 §2.1): the outer state (`palette` /
/// `settings` / `find` / `note_editor`) keeps its `open` flag TRUE through
/// the whole Closing phase — every reader that draws the surface (visible
/// semantics) keeps working unchanged, while readers that route input
/// switch to the `interactive()` predicate (`open && !closing`) so a fading
/// surface never eats input. The real state teardown (`close()`, content
/// clear, focus bookkeeping) runs in the owner's `finish_close`, driven by
/// the draw frame's maintenance pass (app/focus.rs) once the tween lands.
///
/// Reduce Motion contract: `begin_*` with `animated == false` never creates
/// a tween — open snaps to 1.0, close snaps to the immediately-finished
/// Closing state (the next maintenance pass runs the real teardown).
#[derive(Debug, Default)]
pub(crate) struct Reveal {
    tween: Option<Tween>,
    closing: bool,
}

impl Reveal {
    /// Begin the open transition (0→1, 120ms). `animated == false` (Reduce
    /// Motion) snaps straight to the steady-open value.
    pub(crate) fn begin_open(&mut self, now: Instant, animated: bool) {
        self.closing = false;
        self.tween = animated.then(|| Tween::new(0.0, 1.0, REVEAL_OPEN_DUR, now));
    }

    /// Begin the close transition (1→0, 100ms) and enter Closing. A second
    /// `begin_close` while already Closing is a no-op (the first tween
    /// keeps running). `animated == false` lands in a tween-less Closing —
    /// [`Self::close_finished`] is immediately true so the next maintenance
    /// pass performs the real teardown.
    pub(crate) fn begin_close(&mut self, now: Instant, animated: bool) {
        if self.closing {
            return;
        }
        self.closing = true;
        self.tween = animated.then(|| Tween::new(1.0, 0.0, REVEAL_CLOSE_DUR, now));
    }

    /// Current reveal factor in [0, 1]: the tween's eased value while one
    /// is in flight, else the settled value (1.0 open / 0.0 closing).
    pub(crate) fn value(&self, now: Instant) -> f32 {
        match &self.tween {
            Some(tween) => tween.value_at(now),
            None => {
                if self.closing {
                    0.0
                } else {
                    1.0
                }
            }
        }
    }

    /// True while the surface is in the Closing phase (interactive readers
    /// must already treat it as closed).
    pub(crate) fn is_closing(&self) -> bool {
        self.closing
    }

    /// True while a tween is in flight (the surface keeps animating).
    /// Test-only: production code tracks tween presence through the owning
    /// state's own fields (e.g. `sidebar_width_anim.is_some()`).
    #[cfg(test)]
    pub(crate) fn animating(&self) -> bool {
        self.tween.is_some()
    }

    /// True when a Closing surface has fully faded and its owner must run
    /// the real teardown now (tween finished, or there never was one).
    pub(crate) fn close_finished(&self, now: Instant) -> bool {
        if !self.closing {
            return false;
        }
        match &self.tween {
            Some(tween) => tween.finished(now),
            None => true,
        }
    }

    /// Maintenance for the OPEN phase: drop a finished open tween so the
    /// live counter releases (value stays 1.0 via the settled branch).
    /// Closing tweens are NOT dropped here — their release is paired with
    /// the owner's real teardown in `finish_close`.
    pub(crate) fn drop_finished_open_tween(&mut self, now: Instant) {
        if !self.closing && self.tween.as_ref().is_some_and(|t| t.finished(now)) {
            self.tween = None;
        }
    }

    /// Reduce Motion flip: replace any in-flight tween with its end value.
    /// The owner checks `is_closing` afterwards — a Closing surface's end
    /// value is "closed", so it runs `finish_close` immediately.
    pub(crate) fn snap_to_end(&mut self) {
        self.tween = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ease_out_cubic_pins_endpoints_midpoint_and_out_of_range_clamp() {
        assert_eq!(ease_out_cubic(0.0), 0.0);
        assert_eq!(ease_out_cubic(1.0), 1.0);
        // 1-(1-0.5)^3 = 0.875.
        assert!((ease_out_cubic(0.5) - 0.875).abs() < 1e-6);
        // Out of range clamps to the endpoints.
        assert_eq!(ease_out_cubic(-0.4), 0.0);
        assert_eq!(ease_out_cubic(1.4), 1.0);
    }

    /// v1.14.1 (F3): the sidebar width animation's apply gate — inside the
    /// 8ms window an in-flight tween is throttled; a finished tween always
    /// applies so the landing (终值) is written.
    #[test]
    fn sidebar_anim_should_apply_truth_table() {
        let now = Instant::now();
        // Fresh tween (never applied): applies immediately.
        assert!(should_apply(now, None, false));
        // <8ms since the last apply, not finished: throttled.
        assert!(!should_apply(
            now,
            Some(now - Duration::from_millis(4)),
            false
        ));
        // ≥8ms (boundary inclusive, same as the drag-path commit gate): applies.
        assert!(should_apply(
            now,
            Some(now - crate::effect::RESIZE_COMMIT_MIN_INTERVAL),
            false
        ));
        // Finished always applies (终值必写) even inside the window.
        assert!(should_apply(now, Some(now), true));
    }

    #[test]
    fn tween_value_at_tracks_start_middle_end_and_overshoot() {
        let start = Instant::now();
        let dur = Duration::from_millis(100);
        let tween = Tween::new(0.0, 10.0, dur, start);
        // Before/at start → from.
        assert_eq!(tween.value_at(start - Duration::from_millis(5)), 0.0);
        assert_eq!(tween.value_at(start), 0.0);
        // Middle: eased, so past the linear midpoint (0.875 * 10 = 8.75).
        let mid = tween.value_at(start + Duration::from_millis(50));
        assert!((mid - 8.75).abs() < 1e-4, "mid={mid}");
        // End and overshoot clamp to the target.
        assert_eq!(tween.value_at(start + dur), 10.0);
        assert_eq!(tween.value_at(start + Duration::from_millis(500)), 10.0);
    }

    #[test]
    fn tween_finished_boundary_and_target_accessors_agree() {
        let start = Instant::now();
        let dur = Duration::from_millis(100);
        let tween = Tween::new(2.0, 42.0, dur, start);
        assert!(!tween.finished(start));
        assert!(!tween.finished(start + Duration::from_millis(99)));
        assert!(tween.finished(start + dur));
        assert!(tween.finished(start + Duration::from_millis(1000)));
        assert_eq!(tween.target(), 42.0);
        // A zero-duration tween is finished the instant it starts and its
        // value_at lands on the target (documented clamp parity).
        let instant = Tween::new(2.0, 42.0, Duration::ZERO, start);
        assert!(instant.finished(start));
        assert_eq!(instant.value_at(start), 42.0);
    }

    #[test]
    fn motion_live_counts_are_raii_paired_through_scope_drop_and_vec_clear() {
        let start = Instant::now();
        let before = live_tweens();
        {
            let _a = Tween::new(0.0, 1.0, Duration::from_millis(10), start);
            let _b = Tween::new(0.0, 1.0, Duration::from_millis(10), start);
            assert_eq!(live_tweens(), before + 2);
            // Batch clear — the drop impl pairs the decrement, no manual
            // fetch_sub anywhere.
            let batch = vec![
                Tween::new(0.0, 1.0, Duration::from_millis(10), start),
                Tween::new(0.0, 1.0, Duration::from_millis(10), start),
            ];
            assert_eq!(batch.len(), 2);
            assert_eq!(live_tweens(), before + 4);
            drop(batch);
            assert_eq!(live_tweens(), before + 2);
        }
        assert_eq!(live_tweens(), before);
    }

    #[test]
    fn reduce_motion_gate_snaps_everything_to_end_values() {
        // Reduce Motion ON → the gate is closed (no tween may be created).
        assert!(!gated(true));
        // Reduce Motion OFF → motion may animate.
        assert!(gated(false));
    }

    #[test]
    fn reveal_open_then_close_walks_the_full_state_machine() {
        let start = Instant::now();
        let mut reveal = Reveal::default();
        // Closed: no value drawn (owner's open flag gates drawing), not
        // closing, nothing animating.
        assert!(!reveal.is_closing());
        assert!(!reveal.animating());

        // Open: 0→1 over 120ms; visible = 1.0 (owner keeps open=true).
        reveal.begin_open(start, true);
        assert!(reveal.animating());
        assert!(!reveal.is_closing());
        assert_eq!(reveal.value(start), 0.0);
        assert!((reveal.value(start + Duration::from_millis(60)) - 0.875).abs() < 1e-4);
        assert_eq!(reveal.value(start + REVEAL_OPEN_DUR), 1.0);
        // Maintenance drops the finished open tween; value stays settled 1.0.
        reveal.drop_finished_open_tween(start + REVEAL_OPEN_DUR);
        assert!(!reveal.animating());
        assert_eq!(reveal.value(start + REVEAL_OPEN_DUR), 1.0);

        // Close: interactive flips off immediately, visible stays on, and
        // the real teardown becomes due exactly when the tween lands.
        let close_at = start + Duration::from_secs(1);
        reveal.begin_close(close_at, true);
        assert!(reveal.is_closing());
        assert_eq!(reveal.value(close_at), 1.0);
        assert!(!reveal.close_finished(close_at + Duration::from_millis(50)));
        assert_eq!(reveal.value(close_at + REVEAL_CLOSE_DUR), 0.0);
        assert!(reveal.close_finished(close_at + REVEAL_CLOSE_DUR));
    }

    #[test]
    fn reveal_reopen_during_closing_restarts_from_zero() {
        let start = Instant::now();
        let mut reveal = Reveal::default();
        reveal.begin_open(start, true);
        let close_at = start + REVEAL_OPEN_DUR;
        reveal.begin_close(close_at, true);
        assert!(reveal.is_closing());

        // 防抖 reopen (PLAN_v1140 §2.1): the owner's finish_close runs the
        // real teardown first; the fresh open then starts the 0→1 tween.
        reveal.begin_open(close_at + Duration::from_millis(40), true);
        assert!(!reveal.is_closing());
        assert!(reveal.animating());
        assert_eq!(reveal.value(close_at + Duration::from_millis(40)), 0.0);
    }

    #[test]
    fn reveal_second_begin_close_is_a_noop() {
        let start = Instant::now();
        let mut reveal = Reveal::default();
        reveal.begin_open(start, true);
        reveal.begin_close(start, true);
        let first_value = reveal.value(start + Duration::from_millis(50));
        // A second close request must not restart the fade.
        reveal.begin_close(start + Duration::from_millis(40), true);
        assert!((reveal.value(start + Duration::from_millis(50)) - first_value).abs() < 1e-6);
    }

    #[test]
    fn reveal_reduce_motion_snaps_open_to_steady_and_close_to_finished() {
        let start = Instant::now();
        let mut reveal = Reveal::default();
        // Reduce Motion open: no tween, settled at 1.0 immediately.
        reveal.begin_open(start, false);
        assert!(!reveal.animating());
        assert_eq!(reveal.value(start), 1.0);

        // Reduce Motion close: tween-less Closing — immediately finished so
        // the next maintenance pass performs the real teardown.
        reveal.begin_close(start, false);
        assert!(reveal.is_closing());
        assert_eq!(reveal.value(start), 0.0);
        assert!(reveal.close_finished(start));
        // The snap path (Reduce Motion flip mid-flight) behaves identically.
        let mut flying = Reveal::default();
        flying.begin_open(start, true);
        flying.begin_close(start, true);
        flying.snap_to_end();
        assert!(flying.close_finished(start));
        assert_eq!(flying.value(start), 0.0);
    }

    #[test]
    fn hover_fade_ramps_both_directions_and_survives_per_frame_resync() {
        let start = Instant::now();
        let mut fades = HoverFades::default();
        // Hover in: 0→1 over 100ms. The per-frame re-sync must NOT restart
        // the tween (it would freeze at the first frame's value).
        fades.sync_target(HoverKey::TabPlus, true, start, true);
        let mid = start + Duration::from_millis(50);
        fades.sync_target(HoverKey::TabPlus, true, mid, true);
        let first = fades.progress(HoverKey::TabPlus, mid);
        fades.sync_target(HoverKey::TabPlus, true, mid, true);
        assert!((fades.progress(HoverKey::TabPlus, mid) - first).abs() < 1e-6);
        assert!((first - 0.875).abs() < 1e-4);
        // Settled hover: tween released, progress pinned at 1.0.
        fades.sync_target(HoverKey::TabPlus, true, start + HOVER_FADE_DUR, true);
        fades.maintain(start + HOVER_FADE_DUR);
        assert_eq!(
            fades.progress(HoverKey::TabPlus, start + HOVER_FADE_DUR),
            1.0
        );
        assert_eq!(fades.tween_count(), 0);
        // Hover out: 1→0 from the drawn value; settle removes the entry.
        let out_at = start + HOVER_FADE_DUR;
        fades.sync_target(HoverKey::TabPlus, false, out_at, true);
        fades.sync_target(HoverKey::TabPlus, false, out_at, true);
        // At the tween's start instant the value is still 1.0; halfway down
        // it reads 1 - 0.875 = 0.125, and a re-sync must not restart it.
        let out_mid = out_at + Duration::from_millis(50);
        let down = fades.progress(HoverKey::TabPlus, out_mid);
        fades.sync_target(HoverKey::TabPlus, false, out_mid, true);
        assert!((fades.progress(HoverKey::TabPlus, out_mid) - down).abs() < 1e-6);
        assert!((down - 0.125).abs() < 1e-4);
        fades.sync_target(HoverKey::TabPlus, false, out_at + 2 * HOVER_FADE_DUR, true);
        fades.maintain(out_at + 2 * HOVER_FADE_DUR);
        assert_eq!(fades.progress(HoverKey::TabPlus, start), 0.0);
    }

    #[test]
    fn hover_fade_reduce_motion_snaps_and_absent_keys_fall_back_to_idle() {
        let start = Instant::now();
        let mut fades = HoverFades::default();
        // Absent key: idle fallback (即时色).
        assert_eq!(fades.progress(HoverKey::ScrollbarThumb, start), 0.0);
        // Reduce Motion hover-in: progress lands at 1.0 with no tween.
        fades.sync_target(HoverKey::ScrollbarThumb, true, start, false);
        assert_eq!(fades.progress(HoverKey::ScrollbarThumb, start), 1.0);
        assert_eq!(fades.tween_count(), 0);
        // Reduce Motion hover-out: straight back to 0, entry dropped.
        fades.sync_target(HoverKey::ScrollbarThumb, false, start, false);
        assert_eq!(fades.progress(HoverKey::ScrollbarThumb, start), 0.0);
    }

    #[test]
    fn hover_fade_project_snapshot_carries_draw_values() {
        let start = Instant::now();
        let mut fades = HoverFades::default();
        fades.sync_target(HoverKey::TabRow(3), true, start, true);
        let snapshot = fades.project(start + Duration::from_millis(50));
        assert_eq!(
            crate::app::motion::fade_progress(&snapshot, HoverKey::TabRow(3)),
            fades.progress(HoverKey::TabRow(3), start + Duration::from_millis(50))
        );
        assert_eq!(
            crate::app::motion::fade_progress(&snapshot, HoverKey::TabRow(9)),
            0.0
        );
    }
}
