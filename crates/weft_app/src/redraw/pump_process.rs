//! `run_redraw`'s pump/process segment, moved verbatim from
//! `redraw_controller.rs` (v1.12.25 3-B-2 P2-02, former :55-236).
//!
//! Zero-rewrite: every statement and comment below is byte-identical to the
//! original except the four inline `return`s, which became
//! [`PhaseOutcome::Abort`] per the plan's explicit conversion contract — the
//! caller returns from `run_redraw` at the same points.

use crate::effect;
use crate::redraw::PhaseOutcome;
use weft_core::vt::Terminal;

impl crate::App {
    /// Former `run_redraw` :55-76 — PTY pump + post-drain empty-tabs guard.
    ///
    /// Returns `(outcome, had_output)`; `had_output` is the value produced by
    /// `process_messages` and is only meaningful with [`PhaseOutcome::Continue`]
    /// (on `Abort` the caller returns immediately and never reads it).
    pub(crate) fn redraw_pump_phase(&mut self) -> (PhaseOutcome, bool) {
        self.pump_pty();
        // v1.11.15 review P2: tabs may ALREADY be empty here — emptied by a
        // previous event's process_messages while a drag gesture stayed
        // armed. pump_selection_autoscroll derefs the active tab through
        // block_view_active() when `selection_drag_pos` is set, so the
        // early return must precede it (the just-emptied window below is a
        // separate guard).
        if self.sessions.tabs().is_empty() {
            return (PhaseOutcome::Abort, false);
        }
        // Drag-selection autoscroll: the 40ms timer wakes this path while a
        // drag is held past the block content edge, so the viewport keeps
        // scrolling (and the selection extending) even with a still pointer.
        self.pump_selection_autoscroll();
        let had_output = self.process_messages();
        // v1.11.15 (FIX B, PLAN_v11115_EXIT_RACE_MOUSE_LEAK §2): the last
        // shell can exit inside process_messages (remove_dead empties
        // `tabs`); every line below derefs the active tab. run_redraw
        // returns (), so a plain return is the whole guard.
        if self.sessions.tabs().is_empty() {
            return (PhaseOutcome::Abort, false);
        }
        (PhaseOutcome::Continue, had_output)
    }

    /// Former `run_redraw` :77-236 — alt-rescale, suppression gates, pollers,
    /// output snap, layout convergence, and the resize-cascade drain.
    pub(crate) fn redraw_process_phase(&mut self, forced: bool, had_output: bool) -> PhaseOutcome {
        // v1.10.4: if the active pane entered/exited alt-screen (DEC 1049),
        // recompute geometry so PTY cols switch between full-width (TUI)
        // and gutter-subtracted (BlockView). Must happen before the render
        // so the grid dimensions match the new screen mode this frame.
        if self
            .sessions
            .active_mut()
            .is_some_and(|tab| tab.take_pending_alt_rescale())
        {
            self.recompute_layout();
        }
        if crate::redraw_gates::redraw_suppressed(
            forced,
            self.sessions
                .active()
                .and_then(|tab| tab.terminal.as_ref())
                .is_some_and(Terminal::synchronized_output),
            crate::input_router::route_session_input(!self.sessions.is_empty()),
        ) {
            // WHY (v1.11.10 M-B/D-j): synchronized output deliberately
            // suppresses presents so a TUI can move a big cursor region
            // without showing every intermediate state. During a live drag
            // the frozen/stretched frame is the worse artifact (D-j), so the
            // forced path draws the current state anyway — TUI repaints and
            // heals once its sync block ends. Non-forced keeps the
            // suppression.
            return PhaseOutcome::Abort;
        }
        if crate::redraw_gates::redraw_suppressed(
            forced,
            false,
            crate::input_router::route_session_input(!self.sessions.is_empty()),
        ) {
            // WHY (v1.11.10 M-B/D-d): route-consume means a modal captured
            // the pointer and owns the frame — skip the draw so the modal's
            // repaint is not fought. The forced path ignores the route: a
            // live-resize frame must commit regardless, and the modal renders
            // on top in the same tick's draw. Structural note: gate 1 already
            // returns on !forced && consume, so this gate is unreachable in
            // the current flow — kept as an independent future divergence
            // point (rust-reviewer 2026-08-30).
            return PhaseOutcome::Abort;
        }
        self.update_cursor_blink();
        self.update_spinner();
        // FindInGrid debounce: when 150ms have elapsed since the last
        // keystroke, run the search and update `find_matches`.
        self.maybe_refresh_find_results();
        // v0.9 U-D1: drain pending find-worker results (async grid search).
        self.poll_find_worker_results();
        // v1.7.1: drain palette search worker results (async FTS5 search).
        self.poll_palette_search_results();
        // v0.9 U-D1: poll macOS system appearance (throttled to 1Hz).
        self.poll_system_appearance();

        // During command execution, new output streams in — snap the
        // block view to the bottom so the user sees fresh content.
        // (At prompt / idle, preserve the user's scroll position.)
        // R2-1: also skip snapping when the user has detached to a fixed
        // document row. Without this guard, the first frame of output
        // after scroll_up_by would snap_to_bottom and discard the user's
        // scroll position — the exact bug this enum was introduced to fix.
        if had_output {
            let snap_to_bottom = self
                .sessions
                .active()
                .and_then(|tab| tab.terminal.as_ref())
                .map(|t| {
                    crate::block_component::should_follow_running_output(
                        t.block_tracker().phase(),
                        t.primary_history_view(),
                    )
                })
                .unwrap_or(false)
                && matches!(
                    self.sessions.active().map(|tab| tab.block_scroll_anchor()),
                    Some(crate::tab::BlockScrollAnchor::FollowBottom)
                );
            if snap_to_bottom {
                if let Some(tab) = self.sessions.active_mut() {
                    tab.snap_to_bottom();
                }
            }
        }

        // If the grid row count drifted from what the terminal holds
        // (font/padding/window-size change, the one-time convergence from the
        // spawn size to the padded size) — recompute.
        let desired = self
            .terminal_layout()
            .and_then(|layout| {
                self.sessions.active().and_then(|tab| {
                    tab.active_pane_dimensions_for_rect(
                        [
                            layout.content.left as f32,
                            layout.content.top as f32,
                            layout.content.right as f32,
                            layout.content.bottom as f32,
                        ],
                        layout.cell_width as f32,
                        layout.cell_height as f32,
                    )
                })
            })
            .unwrap_or((0, 0));
        let current = self
            .sessions
            .active()
            .and_then(|tab| tab.terminal.as_ref())
            .map(|t| (t.grid().num_rows, t.grid().num_cols))
            .unwrap_or((0, 0));
        if desired.0 != 0 && desired.1 != 0 && desired != current {
            self.recompute_layout();
        }

        // Flush the PTY SIGWINCH (TIOCSWINSZ) so the foreground app
        // repaints at the new size.
        //
        // The active pane commits every redraw: delaying until a resize
        // cascade pauses leaves its PTY at the old width while the Grid is
        // already narrow, so long synchronized progress lines auto-wrap and
        // later `CSI A` repaints cannot erase the extra rows. Background tabs
        // remain settle-debounced; both their PTY and Grid retain the old
        // geometry together until the transaction commits.
        // PLAN_zoom_drawable_stall Phase C (revises Appendix I-6): commit
        // background panes per step in BOTH animation directions. The
        // zoom-in arm predates it (a live background TUI stayed at its old
        // width, field: split + top); zoom-out joins it because C3's
        // main-path un-suppression feeds it per step -- otherwise the right
        // pane would stay at the old width (the same field symptom).
        // Review MEDIUM-2 wiring anchor: the force bit is the pure truth
        // table (zoom_render::cascade_force_commit); the elapsed-debounce
        // leg stays here at the call site.
        let elapsed_debounce = self.window_runtime.last_resize_instant.elapsed()
            > std::time::Duration::from_millis(100);
        let force =
            crate::paint::zoom_render::cascade_force_commit(crate::macos_zoom::zoom_anim_active());
        let cascade_settled = elapsed_debounce || force;
        let pending: Vec<_> = self
            .sessions
            .tabs()
            .iter()
            .map(|tab| (tab.session_id, tab.pending_pane_resizes()))
            .collect();
        // v1.12.25 (audit 3-B, P1-01): the empty-tabs guard at the top of
        // run_redraw makes `None` unreachable here — an empty effects list
        // keeps the no-drain no-op without a panic.
        let resize_effects = self
            .sessions
            .active()
            .map(|tab| {
                effect::pending_resize_effects(
                    &pending,
                    tab.session_id,
                    cascade_settled,
                    std::time::Instant::now(),
                )
            })
            .unwrap_or_default();
        self.drain_effects(resize_effects);

        PhaseOutcome::Continue
    }
}
