use super::{AltFlipHistory, Tab};
use crate::effect::PendingPaneResize;
use std::time::{Duration, Instant};
use weft_core::vt::{Terminal, TuiColsKind};

/// v1.10.19: How long a repeated alt-screen toggle delays the pending
/// rescale recompute. Tuned above the observed SIGWINCH feedback-loop period
/// (~130ms: toggle → resize → TIOCSWINSZ → SIGWINCH → redraw → toggle), so a
/// storm is held until it goes quiet instead of recomputing per flip.
///
/// v1.10.25 Batch 3 (B1): the same window drives the cols-mirror burst
/// hysteresis below.
const ALT_RESCALE_DEBOUNCE: Duration = Duration::from_millis(150);

/// v1.10.25 Batch 3 (B1) + v1.10.26 Batch D (D-1) + v1.10.27
/// (FIX_RESIZE_DOUBLE_REDRAW): burst hysteresis — FREEZE, not Content-lock.
///
/// The "storm" signature is the **two-flip** record in [`AltFlipHistory`]:
/// two flips from the same source pane inside the debounce window, the most
/// recent still fresh. While a DEC 1049 toggle storm is in progress the
/// effective target is **frozen at the pane's current grid size**
/// (`terminal.grid()`, the same source the ioctl dedup compares against) —
/// both rows and cols — so the drift check's desired == current and the PTY
/// is not touched for the whole burst. Returns `Some((rows, cols))` while
/// frozen; `None` lets the caller map the live `raw_kind` as usual.
///
/// v1.10.26 D-1 (two-flip signature, Content-lock) fixed the single-fling
/// mis-fire but left a double redraw on the omp resize loop: the lock pinned
/// cols to the *Content constant recomputed from the new window size* (91),
/// which differed from the first size already sent (94) — one extra ioctl
/// inside the burst (SIGWINCH → omp repaint → re-toggle), then another full
/// repaint on the quiet convergence back to 94. v1.10.27 freezes at `current`
/// instead: desired == current → zero ioctl mid-storm → the app repaints
/// exactly once, on the single post-quiet jump to the final value.
///
/// Anti-oscillation invariant: with no storm the target follows the live
/// kind; the instant a fresh same-pane burst forms, desired == current kills
/// the drift energy that used to feed the Full↔Content alternation (no
/// ioctl → no new SIGWINCH → no new flip). After the quiet convergence the
/// grid holds the final value; if the app returns to toggling, the NEW burst
/// freezes at that already-converged value — so a storm can never become
/// self-sustaining, no weaker than the v1.10.26 Content-lock. A lone flip (a
/// real alt TUI launch, `count == 1`) never forms the signature and
/// converges to its live kind immediately; a different pane's storm never
/// freezes this pane.
fn cols_target_with_burst_freeze(
    now: Instant,
    history: Option<AltFlipHistory>,
    target_pane: weft_core::pane_layout::PaneId,
    current_dim: (usize, usize),
    debounce: Duration,
) -> Option<(usize, usize)> {
    let storm = history.is_some_and(|h| {
        h.count >= 2
            && h.src_pane == target_pane
            && now.saturating_duration_since(h.newer) < debounce
            && h.newer.duration_since(h.older) < debounce
    });
    storm.then_some(current_dim)
}

impl Tab {
    /// v1.10.19: Debounced consumer of the pending alt-screen rescale.
    ///
    /// A single toggle (a TUI launch) recomputes immediately — no width
    /// flash on the first frame. A toggle that arrives inside
    /// [`ALT_RESCALE_DEBOUNCE`] of the last consumed recompute marks a burst
    /// (the resize feedback loop toggles every ~130ms): the flag is held,
    /// rate-limiting a sustained burst to at most one recompute per debounce
    /// window (not strictly one for the whole burst — once `last_taken`
    /// goes stale a recompute is consumed even mid-burst). Each recompute
    /// can queue a PTY resize — per-flip recomputes are the energy that
    /// keeps the SIGWINCH → alt-toggle loop alive, and the winsize ioctl
    /// dedup in `pane.rs` makes the leftover rate-limited recomputes
    /// idempotent.
    ///
    /// v1.10.25 Batch 3 (B1): this gate alone does NOT bound the loop. The
    /// active tab's per-frame drift check (redraw_controller.rs) recomputes
    /// unconditionally whenever desired != current — it never calls this
    /// method — and the ioctl dedup is a simple inequality that cannot
    /// suppress an alternating Full/Content pair. The anti-cycle guard is
    /// [`Tab::burst_locked_cols`], applied at both cols mirror sites.
    pub(crate) fn take_pending_alt_rescale(&mut self) -> bool {
        if !self.pending_alt_rescale {
            return false;
        }
        let now = Instant::now();
        // The recent-flip freshness is read from the flip history's newest
        // instant (v1.10.26 D-2: armed even by an even-count batch, because
        // the history is driven by the u64 flip-counter diff).
        let fresh_flip = self
            .alt_flip_history
            .is_some_and(|h| now.duration_since(h.newer) < ALT_RESCALE_DEBOUNCE);
        let fresh_take = self
            .alt_rescale_last_taken
            .is_some_and(|at| now.duration_since(at) < ALT_RESCALE_DEBOUNCE);
        if fresh_flip && fresh_take {
            return false;
        }
        self.pending_alt_rescale = false;
        self.alt_rescale_last_taken = Some(now);
        true
    }

    /// v1.10.25 Batch 3 (FIX_SELECTION_AND_RESIZE_REMAINING): arm the
    /// one-shot RESIZE_PROBE stage-3 gate — the next PTY output logs the
    /// post-resize repaint latency, then disarms.
    pub(crate) fn arm_resize_output_probe(&mut self) {
        self.resize_output_probe = Some(Instant::now());
    }

    /// v1.10.25 Batch 3: consume the stage-3 gate if armed; returns the
    /// elapsed time since it was armed (the ioctl-to-repaint gap).
    pub(crate) fn take_resize_output_probe(&mut self) -> Option<std::time::Duration> {
        self.resize_output_probe.take().map(|armed| armed.elapsed())
    }

    /// v1.10.19/25/26/27: PTY (rows, cols) target for one pane — full width
    /// on the alt screen (`Terminal::tui_cols_kind` → [Full][TuiColsKind::Full]),
    /// gutter-subtracted content width otherwise, geometry-derived rows. Sole
    /// shared implementation for the two mirror sites
    /// (`Tab::active_pane_dimensions_for_rect` and
    /// `Tab::resize_all_panes_for_rect`) so they can never diverge.
    ///
    /// v1.10.25 Batch 3 (B1) + v1.10.26 (D-1) + v1.10.27
    /// (FIX_RESIZE_DOUBLE_REDRAW): burst hysteresis is scoped per pane. While
    /// the [two-flip storm signature](Self::alt_flip_history) is fresh and
    /// belongs to `pane_id` (the pane being laid out), the target is FROZEN at
    /// the pane's current grid size — both rows and cols straight from
    /// `terminal.grid()`, the same source the ioctl dedup compares against.
    /// A toggle storm (omp repaint: SIGWINCH → redraw → toggle, ~130ms) keeps
    /// the window fresh, so desired == current and the drift check queues
    /// nothing — zero ioctl for the whole burst, the opposite of the v1.10.26
    /// lock which pinned a *recomputed* Content constant a resize had already
    /// moved past. The app repaints exactly once, on the single post-quiet
    /// jump (no "抖动两下"). A lone flip (a real alt TUI launch) never forms
    /// the two-flip signature and converges to Full on the first measurement;
    /// a different pane's storm never freezes this pane.
    pub(super) fn burst_locked_cols(
        &self,
        pane_id: weft_core::pane_layout::PaneId,
        pane_width: f32,
        pane_height: f32,
        cell_w: f32,
        cell_h: f32,
    ) -> (usize, usize) {
        let Some(terminal) = self
            .panes
            .get(&pane_id)
            .and_then(|pane| pane.terminal.as_ref())
        else {
            // No live terminal — fall back to the Content-width mapping (prior
            // behaviour) so a phantom pane still reports a sane target.
            let cols = crate::layout::terminal_content_cols(pane_width, cell_w);
            return ((pane_height / cell_h).floor() as usize, cols);
        };
        let current = (terminal.grid().num_rows, terminal.grid().num_cols);
        if let Some(frozen) = cols_target_with_burst_freeze(
            Instant::now(),
            self.alt_flip_history,
            pane_id,
            current,
            ALT_RESCALE_DEBOUNCE,
        ) {
            return frozen;
        }
        let cols = match terminal.tui_cols_kind() {
            TuiColsKind::Full => crate::layout::terminal_full_cols(pane_width, cell_w),
            TuiColsKind::Content => crate::layout::terminal_content_cols(pane_width, cell_w),
        };
        let rows = (pane_height / cell_h).floor() as usize;
        (rows, cols)
    }

    /// v1.10.26 Batch D (D-1/D-2): record the alt-screen flips detected in
    /// one PTY batch, driving the storm signature for `burst_locked_cols`.
    /// The source pane is the active pane (only the active pane's terminal is
    /// processed). `batch_flips` is the `Terminal::alt_flip_count` diff across
    /// a `process()` call — ≥1 guarantees a real flip; a batch-internal h→l
    /// pair (net-zero `alt_active`) still counts as two flips (D-2) and
    /// refreshes the debounce window the old boolean detection missed.
    pub(crate) fn record_alt_flip_instants(&mut self, batch_flips: u64) {
        debug_assert!(batch_flips > 0);
        let now = std::time::Instant::now();
        let src_pane = self.active_pane;
        if batch_flips >= 2 {
            // A single batch with ≥2 toggles lands both storm instants at
            // `now` — a sub-µs double flip, the tightest possible storm.
            self.alt_flip_history = Some(AltFlipHistory {
                src_pane,
                older: now,
                newer: now,
                count: 2,
            });
            return;
        }
        // A single flip: append to the same pane's record. A pane switch
        // restarts the record so pane A's storm cannot arm pane B's lock.
        let prior = match self.alt_flip_history {
            Some(h) if h.src_pane == src_pane => h,
            _ => AltFlipHistory {
                src_pane,
                older: now,
                newer: now,
                count: 0,
            },
        };
        self.alt_flip_history = Some(AltFlipHistory {
            src_pane,
            older: prior.newer,
            newer: now,
            count: prior.count.saturating_add(1).min(2),
        });
    }

    /// Read every pane's pending PTY resize and synchronized-frame state.
    pub(crate) fn pending_pane_resizes(&self) -> Vec<PendingPaneResize> {
        let mut out = Vec::new();
        for (id, pane) in &self.panes {
            if let Some(dim) = pane.pending_pty_resize {
                let synchronized = pane
                    .terminal
                    .as_ref()
                    .is_some_and(Terminal::synchronized_output);
                out.push(PendingPaneResize::new(*id, dim, synchronized));
            }
        }
        out
    }

    pub(crate) fn any_synchronized_output(&self) -> bool {
        self.panes.values().any(|pane| {
            pane.terminal
                .as_ref()
                .is_some_and(Terminal::synchronized_output)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pane::Pane;
    use weft_core::pane_layout::PaneId;

    fn single_flip(src: PaneId, at: Instant) -> AltFlipHistory {
        AltFlipHistory {
            src_pane: src,
            older: at,
            newer: at,
            count: 1,
        }
    }

    fn double_flip(src: PaneId, older: Instant, newer: Instant) -> AltFlipHistory {
        AltFlipHistory {
            src_pane: src,
            older,
            newer,
            count: 2,
        }
    }

    #[test]
    fn alt_rescale_burst_rate_limits_until_quiet() {
        let mut tab = Tab::with_single_pane(Pane::with_terminal_only(1000));
        let src = tab.active_pane;

        // Single toggle (a TUI launch): recompute immediately, no debounce.
        tab.pending_alt_rescale = true;
        tab.alt_flip_history = Some(single_flip(src, Instant::now()));
        assert!(
            tab.take_pending_alt_rescale(),
            "a lone toggle must recompute right away"
        );
        assert!(!tab.take_pending_alt_rescale(), "the flag is consumed");

        // Burst: a fresh toggle right after the consumed recompute is held
        // until the toggles go quiet (the ~130ms SIGWINCH feedback loop).
        tab.pending_alt_rescale = true;
        tab.alt_flip_history = Some(single_flip(src, Instant::now()));
        assert!(
            !tab.take_pending_alt_rescale(),
            "a burst inside the window must hold the recompute"
        );
        assert!(
            !tab.take_pending_alt_rescale(),
            "still holding while the burst continues"
        );
        std::thread::sleep(ALT_RESCALE_DEBOUNCE + Duration::from_millis(20));
        assert!(
            tab.take_pending_alt_rescale(),
            "once the burst goes quiet the recompute is consumed"
        );
        assert!(!tab.take_pending_alt_rescale(), "flag consumed");
    }

    #[test]
    fn alt_rescale_stale_flip_recomputes_immediately() {
        let mut tab = Tab::with_single_pane(Pane::with_terminal_only(1000));
        let src = tab.active_pane;
        // A flip older than the debounce window (e.g. a legit re-toggle
        // minutes later) must not be held.
        tab.pending_alt_rescale = true;
        tab.alt_flip_history = Some(single_flip(
            src,
            Instant::now() - ALT_RESCALE_DEBOUNCE - Duration::from_millis(50),
        ));
        assert!(tab.take_pending_alt_rescale());
    }

    // ── v1.10.25 (B1) + v1.10.26 (D-1) + v1.10.27: burst freeze ──

    const LOCK_DEBOUNCE: Duration = Duration::from_millis(150);
    const SRC_PANE: PaneId = PaneId(1);
    const OTHER_PANE: PaneId = PaneId(2);
    const CURRENT: (usize, usize) = (30, 77);

    /// D-1: an isolated single flip (a real vim/less launch) must NEVER freeze
    /// — the live alt Full kind applies immediately (the caller's raw mapping
    /// converges, no Content hold).
    #[test]
    fn lone_single_flip_never_freezes_at_current() {
        let now = Instant::now();
        let fresh = now - Duration::from_millis(5);
        assert_eq!(
            cols_target_with_burst_freeze(
                now,
                Some(single_flip(SRC_PANE, fresh)),
                SRC_PANE,
                CURRENT,
                LOCK_DEBOUNCE,
            ),
            None,
            "a lone fresh flip must return no freeze — the live kind converges"
        );
        // Same even when the lone flip is stale.
        assert_eq!(
            cols_target_with_burst_freeze(
                now,
                Some(single_flip(SRC_PANE, now - Duration::from_millis(500))),
                SRC_PANE,
                CURRENT,
                LOCK_DEBOUNCE,
            ),
            None
        );
        // No history at all (fresh tab) → never freeze.
        assert_eq!(
            cols_target_with_burst_freeze(now, None, SRC_PANE, CURRENT, LOCK_DEBOUNCE),
            None
        );
    }

    /// D-1 + v1.10.27: two flips from the SAME pane inside the window (the
    /// storm signature, most recent fresh) FREEZE the target at the pane's
    /// current grid size — in EITHER phase, rows AND cols together. The
    /// freeze no longer depends on which phase the flips landed on; it pins
    /// `current` so desired == current and the ioctl stream is silent.
    #[test]
    fn double_flip_within_window_freezes_at_current_grid() {
        let now = Instant::now();
        let older = now - Duration::from_millis(110);
        let newer = now - Duration::from_millis(30);
        assert_eq!(
            cols_target_with_burst_freeze(
                now,
                Some(double_flip(SRC_PANE, older, newer)),
                SRC_PANE,
                CURRENT,
                LOCK_DEBOUNCE,
            ),
            Some(CURRENT),
            "a fresh same-pane double flip must freeze rows+cols at the current grid"
        );
        // Rows are frozen too — a vertically-changed window must not drift rows.
        let current = (40, 100);
        assert_eq!(
            cols_target_with_burst_freeze(
                now,
                Some(double_flip(SRC_PANE, older, newer)),
                SRC_PANE,
                current,
                LOCK_DEBOUNCE,
            ),
            Some(current),
            "the freeze carries both rows and cols from the live grid"
        );
    }

    /// D-1 + v1.10.27: the storm signature expires once the flips go quiet —
    /// no freeze (the caller converges to the live kind again).
    #[test]
    fn double_flip_goes_stale_and_unfreezes() {
        let now = Instant::now();
        // Both flips inside the window but long past — a quieted storm.
        let older = now - Duration::from_millis(700);
        let newer = now - Duration::from_millis(600);
        assert_eq!(
            cols_target_with_burst_freeze(
                now,
                Some(double_flip(SRC_PANE, older, newer)),
                SRC_PANE,
                CURRENT,
                LOCK_DEBOUNCE,
            ),
            None,
            "a quieted double flip must release the freeze — the live target converges"
        );
        // Also: a fresh older flip but a stale newest flip.
        assert_eq!(
            cols_target_with_burst_freeze(
                now,
                Some(double_flip(
                    SRC_PANE,
                    now - Duration::from_millis(200),
                    now - Duration::from_millis(160)
                )),
                SRC_PANE,
                CURRENT,
                LOCK_DEBOUNCE,
            ),
            None
        );
    }

    /// D-1 (v1.10.19 "A 面板风暴误伤 B 面板"): pane A's storm must not freeze
    /// pane B's target — the signature is scoped to the source pane.
    #[test]
    fn cross_pane_storm_does_not_freeze_other_pane() {
        let now = Instant::now();
        let h = double_flip(
            SRC_PANE,
            now - Duration::from_millis(100),
            now - Duration::from_millis(20),
        );
        assert_eq!(
            cols_target_with_burst_freeze(now, Some(h), OTHER_PANE, CURRENT, LOCK_DEBOUNCE),
            None,
            "a storm from pane A must not freeze pane B's target"
        );
        // The same source pane is frozen as expected.
        assert_eq!(
            cols_target_with_burst_freeze(now, Some(h), SRC_PANE, CURRENT, LOCK_DEBOUNCE),
            Some(CURRENT)
        );
    }

    /// v1.10.26 D-1 + v1.10.27: a 20-flip storm always pins the CURRENT grid —
    /// no alternation, no escape to a recomputed constant mid-burst. The old
    /// lock pinned Content (a value a resize had moved past); the freeze pins
    /// whatever the grid already holds, so zero drift reaches the ioctl path.
    #[test]
    fn burst_20_flip_storm_pins_current_grid_with_no_alternation() {
        // A fresh two-flip record every 50ms — each new flip keeps the storm
        // signature (< 150ms interval, newest fresh), so every measurement
        // must freeze at `CURRENT`. Deterministic (synthetic clock, no
        // sleeping).
        let start = Instant::now();
        for flip in 0..20u32 {
            let now = start + Duration::from_millis(u64::from(flip) * 50);
            let h = double_flip(
                SRC_PANE,
                now - Duration::from_millis(40),
                now - Duration::from_millis(10),
            );
            assert_eq!(
                cols_target_with_burst_freeze(now, Some(h), SRC_PANE, CURRENT, LOCK_DEBOUNCE),
                Some(CURRENT),
                "flip {flip}: a storm must hold the target at current for the whole burst"
            );
        }
    }

    /// v1.10.27 anti-oscillation: a burst that re-forms AFTER a quiet
    /// convergence freezes at the CONVERGED value. The first burst pins the
    /// pre-convergence current; after quiet the grid holds the final value,
    /// and a returning fresh pair freezes at that already-converged value —
    /// still zero additional drift, so the storm cannot become self-sustaining.
    #[test]
    fn burst_reforming_after_convergence_freezes_at_the_converged_value() {
        let start = Instant::now();
        // First burst: pinned at the pre-convergence current for the whole
        // burst window (~150ms).
        let first_burst = double_flip(SRC_PANE, start, start);
        for flip in 0..4u32 {
            let now = start + Duration::from_millis(u64::from(flip) * 40);
            assert_eq!(
                cols_target_with_burst_freeze(
                    now,
                    Some(first_burst),
                    SRC_PANE,
                    (30, 94),
                    LOCK_DEBOUNCE
                ),
                Some((30, 94)),
                "first burst pins current"
            );
        }
        // Quiet: the live kind converges — the grid now holds the final size,
        // say 91. The app then returns to toggling (another fresh pair inside
        // the window): the new burst freezes at the already-converged 91, so
        // there is still no further ioctl energy.
        let second_burst = double_flip(
            SRC_PANE,
            start + Duration::from_millis(560),
            start + Duration::from_millis(590),
        );
        for flip in 0..3u32 {
            let now = start + Duration::from_millis(600 + u64::from(flip) * 40);
            assert_eq!(
                cols_target_with_burst_freeze(now, Some(second_burst), SRC_PANE, (30, 91), LOCK_DEBOUNCE),
                Some((30, 91)),
                "a returning burst pins the already-converged value — the storm cannot self-sustain"
            );
        }
    }
}
