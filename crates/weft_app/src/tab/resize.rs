use super::Tab;
use crate::effect::PendingPaneResize;
use std::time::{Duration, Instant};
use weft_core::vt::Terminal;

/// v1.10.19: How long a repeated alt-screen toggle delays the pending
/// rescale recompute. Tuned above the observed SIGWINCH feedback-loop period
/// (~130ms: toggle → resize → TIOCSWINSZ → SIGWINCH → redraw → toggle), so a
/// storm is held until it goes quiet instead of recomputing per flip.
const ALT_RESCALE_DEBOUNCE: Duration = Duration::from_millis(150);

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
}
