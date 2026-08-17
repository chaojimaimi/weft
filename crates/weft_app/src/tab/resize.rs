use super::Tab;
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

/// v1.10.25 Batch 3 (B1): effective cols kind under burst hysteresis.
///
/// `raw` is the live `Terminal::tui_cols_kind()`. While a DEC 1049 toggle
/// burst is in progress — the most recent flip [`ALT_RESCALE_DEBOUNCE`] or
/// less in the past — the target is locked to [`TuiColsKind::Content`]
/// (primary semantics), so the drift check's desired stays constant no
/// matter which phase the current flip landed on. A real alt TUI (vim)
/// enters with one flip and goes quiet: the window half-expires, the lock
/// drops and the live Full target converges. A storm (omp repaint feedback,
/// ~130ms period) refreshes `last_flip` every turn and locks Content
/// permanently — the Full↔Content alternation that fed the v1.10.19
/// SIGWINCH loop can no longer move the PTY cols.
fn cols_kind_with_burst_lock(
    raw: TuiColsKind,
    now: Instant,
    last_flip: Option<Instant>,
    debounce: Duration,
) -> TuiColsKind {
    if last_flip.is_some_and(|at| now.duration_since(at) < debounce) {
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
        let fresh_flip = self
            .alt_rescale_last_flip
            .is_some_and(|at| now.duration_since(at) < ALT_RESCALE_DEBOUNCE);
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

    /// v1.10.19/25: PTY cols target for one pane — full width on the alt
    /// screen (`Terminal::tui_cols_kind` → [Full][TuiColsKind::Full]),
    /// gutter-subtracted content width otherwise. Sole shared implementation
    /// for the two mirror sites (`Tab::active_pane_dimensions_for_rect` and
    /// `Tab::resize_all_panes_for_rect`) so they can never diverge.
    ///
    /// v1.10.25 Batch 3 (B1): burst hysteresis. While the most recent DEC
    /// 1049 toggle is inside [`ALT_RESCALE_DEBOUNCE`], the kind is locked to
    /// [Content][TuiColsKind::Content] before it maps to a width. A toggle
    /// storm (omp repaint: SIGWINCH → redraw → toggle, ~130ms) keeps the
    /// window fresh, so the drift check's desired stays Content == current
    /// (the winding grid already holds it) and the Full↔Content alternation
    /// that reseeded the v1.10.19 loop has no energy; a single flip that
    /// goes quiet (a real alt TUI) unlocks after the window and converges
    /// to Full.
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
            self.alt_rescale_last_flip,
            ALT_RESCALE_DEBOUNCE,
        );
        match kind {
            TuiColsKind::Full => crate::layout::terminal_full_cols(pane_width, cell_w),
            TuiColsKind::Content => crate::layout::terminal_content_cols(pane_width, cell_w),
        }
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

    #[test]
    fn alt_rescale_burst_rate_limits_until_quiet() {
        let mut tab = Tab::with_single_pane(Pane::with_terminal_only(1000));

        // Single toggle (a TUI launch): recompute immediately, no debounce.
        tab.pending_alt_rescale = true;
        tab.alt_rescale_last_flip = Some(Instant::now());
        assert!(
            tab.take_pending_alt_rescale(),
            "a lone toggle must recompute right away"
        );
        assert!(!tab.take_pending_alt_rescale(), "the flag is consumed");

        // Burst: a fresh toggle right after the consumed recompute is held
        // until the toggles go quiet (the ~130ms SIGWINCH feedback loop).
        tab.pending_alt_rescale = true;
        tab.alt_rescale_last_flip = Some(Instant::now());
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
        // A flip older than the debounce window (e.g. a legit re-toggle
        // minutes later) must not be held.
        tab.pending_alt_rescale = true;
        tab.alt_rescale_last_flip =
            Some(Instant::now() - ALT_RESCALE_DEBOUNCE - Duration::from_millis(50));
        assert!(tab.take_pending_alt_rescale());
    }

    // ── v1.10.25 Batch 3 (B1): burst hysteresis fold ────────────────────

    const LOCK_DEBOUNCE: Duration = Duration::from_millis(150);

    #[test]
    fn burst_lock_pins_content_while_a_toggle_is_fresh() {
        let now = Instant::now();
        let fresh = now - Duration::from_millis(40);
        // In-burst: the target is Content no matter which phase the current
        // flip landed on (alt would otherwise report Full).
        assert_eq!(
            cols_kind_with_burst_lock(TuiColsKind::Full, now, Some(fresh), LOCK_DEBOUNCE),
            TuiColsKind::Content,
            "burst + alt phase must not escape the Content lock"
        );
        assert_eq!(
            cols_kind_with_burst_lock(TuiColsKind::Content, now, Some(fresh), LOCK_DEBOUNCE),
            TuiColsKind::Content,
            "burst + primary phase is Content anyway"
        );
    }

    #[test]
    fn burst_lock_drops_after_silence_and_converges_to_the_live_kind() {
        let now = Instant::now();
        // A single flip (vim launch) then quiet: once the window expires the
        // live kind applies again — alt converges to Full, primary to Content.
        let stale = now - Duration::from_millis(300);
        assert_eq!(
            cols_kind_with_burst_lock(TuiColsKind::Full, now, Some(stale), LOCK_DEBOUNCE),
            TuiColsKind::Full,
            "a quieted single flip must converge to the alt Full target"
        );
        assert_eq!(
            cols_kind_with_burst_lock(TuiColsKind::Content, now, Some(stale), LOCK_DEBOUNCE),
            TuiColsKind::Content
        );
        // No flip ever recorded (fresh tab): no lock.
        assert_eq!(
            cols_kind_with_burst_lock(TuiColsKind::Full, now, None, LOCK_DEBOUNCE),
            TuiColsKind::Full
        );
    }

    #[test]
    fn burst_lock_20_flip_storm_stays_content_with_no_alternation() {
        // A flip every 50ms — each refreshes last_flip inside the window, so
        // every measurement must lock Content while the raw phase alternates
        // Full/Content. Deterministic (synthetic clock, no sleeping).
        let start = Instant::now();
        for flip in 0..20u32 {
            let now = start + Duration::from_millis(u64::from(flip) * 50);
            let last_flip = now - Duration::from_millis(20);
            let raw = if flip % 2 == 0 {
                TuiColsKind::Full
            } else {
                TuiColsKind::Content
            };
            assert_eq!(
                cols_kind_with_burst_lock(raw, now, Some(last_flip), LOCK_DEBOUNCE),
                TuiColsKind::Content,
                "flip {flip}: a storm must never let the target alternate"
            );
        }
    }
}
