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

/// v1.10.25 Batch 3 (B1) + v1.10.26 Batch D (D-1): effective cols kind under
/// burst hysteresis.
///
/// `raw` is the live `Terminal::tui_cols_kind()`. The "storm" signature is
/// now the **two-flip** record in [`AltFlipHistory`]: two flips from the same
/// source pane inside the debounce window, the most recent still fresh. While
/// a DEC 1049 toggle storm is in progress the target is locked to
/// [`TuiColsKind::Content`] (primary semantics), so the drift check's desired
/// stays constant no matter which phase the current flip landed on.
///
/// D-1 fixes the mis-fire the B1 single-fling window caused: a real alt TUI
/// (vim/less) enters with ONE flip and goes quiet — a lone `count == 1`
/// record never locks, so the live Full target converges immediately (no
/// wrong-width flash). The omp repaint feedback loop (~130ms period) keeps
/// two fresh flips on record and is locked to Content permanently — the
/// Full↔Content alternation that fed the v1.10.19 SIGWINCH loop can no longer
/// move the PTY cols. The lock is scoped to `src_pane == target_pane`, so
/// pane A's storm cannot widen pane B's real vim (`burst_locked_cols`).
fn cols_kind_with_burst_lock(
    raw: TuiColsKind,
    now: Instant,
    history: Option<AltFlipHistory>,
    target_pane: weft_core::pane_layout::PaneId,
    debounce: Duration,
) -> TuiColsKind {
    let storm = history.is_some_and(|h| {
        h.count >= 2
            && h.src_pane == target_pane
            && now.saturating_duration_since(h.newer) < debounce
            && h.newer.duration_since(h.older) < debounce
    });
    if storm {
        TuiColsKind::Content
    } else {
        raw
    }
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

    /// v1.10.19/25/26: PTY cols target for one pane — full width on the alt
    /// screen (`Terminal::tui_cols_kind` → [Full][TuiColsKind::Full]),
    /// gutter-subtracted content width otherwise. Sole shared implementation
    /// for the two mirror sites (`Tab::active_pane_dimensions_for_rect` and
    /// `Tab::resize_all_panes_for_rect`) so they can never diverge.
    ///
    /// v1.10.25 Batch 3 (B1) + v1.10.26 (D-1): burst hysteresis is scoped per
    /// pane. While the [two-flip storm signature](Self::alt_flip_history) is
    /// fresh and belongs to `pane_id` (the pane being laid out), the kind is
    /// locked to [Content][TuiColsKind::Content] before it maps to a width. A
    /// toggle storm (omp repaint: SIGWINCH → redraw → toggle, ~130ms) keeps
    /// the window fresh, so the drift check's desired stays Content == current
    /// (the winding grid already holds it) and the Full↔Content alternation
    /// that reseeded the v1.10.19 loop has no energy. A lone flip (a real alt
    /// TUI launch) never forms the two-flip signature and converges to Full on
    /// the first measurement; a different pane's storm never locks this pane.
    pub(super) fn burst_locked_cols(
        &self,
        pane_id: weft_core::pane_layout::PaneId,
        pane_width: f32,
        cell_w: f32,
    ) -> usize {
        let raw = self
            .panes
            .get(&pane_id)
            .and_then(|pane| pane.terminal.as_ref())
            .map(Terminal::tui_cols_kind)
            .unwrap_or(TuiColsKind::Content);
        let kind = cols_kind_with_burst_lock(
            raw,
            Instant::now(),
            self.alt_flip_history,
            pane_id,
            ALT_RESCALE_DEBOUNCE,
        );
        match kind {
            TuiColsKind::Full => crate::layout::terminal_full_cols(pane_width, cell_w),
            TuiColsKind::Content => crate::layout::terminal_content_cols(pane_width, cell_w),
        }
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

    // ── v1.10.25 Batch 3 (B1) + v1.10.26 Batch D (D-1): burst hysteresis ──

    const LOCK_DEBOUNCE: Duration = Duration::from_millis(150);
    const SRC_PANE: PaneId = PaneId(1);
    const OTHER_PANE: PaneId = PaneId(2);

    /// D-1: an isolated single flip (a real vim/less launch) must NEVER lock
    /// — the live alt Full kind applies immediately, no Content hold.
    #[test]
    fn lone_single_flip_never_locks_content() {
        let now = Instant::now();
        let fresh = now - Duration::from_millis(5);
        assert_eq!(
            cols_kind_with_burst_lock(
                TuiColsKind::Full,
                now,
                Some(single_flip(SRC_PANE, fresh)),
                SRC_PANE,
                LOCK_DEBOUNCE,
            ),
            TuiColsKind::Full,
            "a lone fresh flip must converge to the live Full kind"
        );
        // Same even when the lone flip is stale.
        assert_eq!(
            cols_kind_with_burst_lock(
                TuiColsKind::Full,
                now,
                Some(single_flip(SRC_PANE, now - Duration::from_millis(500))),
                SRC_PANE,
                LOCK_DEBOUNCE,
            ),
            TuiColsKind::Full
        );
    }

    /// D-1: two flips from the SAME pane inside the window (the storm
    /// signature, most recent fresh) lock to Content in either phase.
    #[test]
    fn double_flip_within_window_locks_content() {
        let now = Instant::now();
        let older = now - Duration::from_millis(110);
        let newer = now - Duration::from_millis(30);
        assert_eq!(
            cols_kind_with_burst_lock(
                TuiColsKind::Full,
                now,
                Some(double_flip(SRC_PANE, older, newer)),
                SRC_PANE,
                LOCK_DEBOUNCE,
            ),
            TuiColsKind::Content,
            "a fresh same-pane double flip must lock Content even on the alt phase"
        );
        assert_eq!(
            cols_kind_with_burst_lock(
                TuiColsKind::Content,
                now,
                Some(double_flip(SRC_PANE, older, newer)),
                SRC_PANE,
                LOCK_DEBOUNCE,
            ),
            TuiColsKind::Content,
            "primary phase is Content anyway"
        );
    }

    /// D-1: the storm signature expires once the flips go quiet — the live
    /// kind converges again (the TUI that just stopped toggling ends on its
    /// real phase). No history → no lock.
    #[test]
    fn double_flip_goes_stale_and_unlocks() {
        let now = Instant::now();
        // Both flips inside the window but long past — a quieted storm.
        let older = now - Duration::from_millis(700);
        let newer = now - Duration::from_millis(600);
        assert_eq!(
            cols_kind_with_burst_lock(
                TuiColsKind::Full,
                now,
                Some(double_flip(SRC_PANE, older, newer)),
                SRC_PANE,
                LOCK_DEBOUNCE,
            ),
            TuiColsKind::Full,
            "a quieted double flip must converge to the live alt Full target"
        );
        // Also: fresh join but a stale newest flip.
        assert_eq!(
            cols_kind_with_burst_lock(
                TuiColsKind::Content,
                now,
                Some(double_flip(
                    SRC_PANE,
                    now - Duration::from_millis(200),
                    now - Duration::from_millis(160)
                )),
                SRC_PANE,
                LOCK_DEBOUNCE,
            ),
            TuiColsKind::Content
        );
        // Never recorded a flip (fresh tab): no lock.
        assert_eq!(
            cols_kind_with_burst_lock(TuiColsKind::Full, now, None, SRC_PANE, LOCK_DEBOUNCE),
            TuiColsKind::Full
        );
    }

    /// D-1 (v1.10.19 "A 面板风暴误伤 B 面板"): pane A's storm must not lock
    /// pane B's cols — the signature is scoped to the source pane.
    #[test]
    fn cross_pane_storm_does_not_lock_other_pane() {
        let now = Instant::now();
        let h = double_flip(
            SRC_PANE,
            now - Duration::from_millis(100),
            now - Duration::from_millis(20),
        );
        assert_eq!(
            cols_kind_with_burst_lock(TuiColsKind::Full, now, Some(h), OTHER_PANE, LOCK_DEBOUNCE),
            TuiColsKind::Full,
            "a storm from pane A must not widen pane B's real vim"
        );
        // The same source pane is locked as expected.
        assert_eq!(
            cols_kind_with_burst_lock(TuiColsKind::Full, now, Some(h), SRC_PANE, LOCK_DEBOUNCE),
            TuiColsKind::Content
        );
    }

    #[test]
    fn burst_lock_20_flip_storm_stays_content_with_no_alternation() {
        // A fresh two-flip record every 50ms — each new flip keeps the storm
        // signature (< 150ms interval, newest fresh), so every measurement
        // must lock Content while the raw phase alternates Full/Content.
        // Deterministic (synthetic clock, no sleeping).
        let start = Instant::now();
        for flip in 0..20u32 {
            let now = start + Duration::from_millis(u64::from(flip) * 50);
            let h = double_flip(
                SRC_PANE,
                now - Duration::from_millis(40),
                now - Duration::from_millis(10),
            );
            let raw = if flip % 2 == 0 {
                TuiColsKind::Full
            } else {
                TuiColsKind::Content
            };
            assert_eq!(
                cols_kind_with_burst_lock(raw, now, Some(h), SRC_PANE, LOCK_DEBOUNCE),
                TuiColsKind::Content,
                "flip {flip}: a storm must never let the target alternate"
            );
        }
    }
}
