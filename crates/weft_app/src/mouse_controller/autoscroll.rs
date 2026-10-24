//! F4 drag-selection autoscroll pump, moved out of `scroll.rs`
//! (rust-reviewer batch file-budget split — the T6 pixel-scroll path pushed
//! the file past the 800-line gate; verbatim code motion, `impl App`
//! cross-file block per the mouse_press submodule precedent).

use crate::selection::{autoscroll_ramp_rows, AutoscrollDir};
use crate::*;

impl App {
    /// Drag-selection autoscroll: when the pointer is held past the block
    /// content edge, scroll the viewport toward it and extend the selection
    /// endpoint to the newly revealed edge row. Returns true when a
    /// scroll+extend happened (the caller requests a redraw). v1.10.26
    /// (FIX_SELECTION_CONTENT_ANCHORS): called ONLY from the redraw entry on
    /// the 40ms timer wake — mouse-move events arm the timer
    /// (`arm_selection_autoscroll`) but never pump (the old move-driven pump
    /// stacked scrolls under event pressure: "停滞+瞬移"). Speed follows the
    /// Warp ramp — `autoscroll_ramp_rows(overshoot)` rows per tick, the
    /// fractional remainder carried so long drags accumulate sub-row progress.
    pub(crate) fn pump_selection_autoscroll(&mut self) -> bool {
        let Some((x, y)) = self.interaction.selection_drag_pos else {
            self.window_runtime
                .selection_autoscroll_active
                .store(false, Ordering::Relaxed);
            return false;
        };
        if !self.block_view_active() {
            // v1.10.20: grid-mode drag autoscroll for primary-screen TUIs.
            // The grid selection is viewport-relative and cannot scroll; a
            // drag held past the top edge migrates the anchor into the
            // primary history snapshot view, where the block branch below
            // takes over (scroll + extend). Failure degrades: keep the grid
            // selection visible, never switch views and orphan it.
            if self.selection_autoscroll_overshoot(y).is_none() {
                self.window_runtime
                    .selection_autoscroll_active
                    .store(false, Ordering::Relaxed);
                return false;
            }
            if !self.migrate_grid_selection_to_primary_history() {
                tracing::warn!(
                    y,
                    "TUI drag autoscroll migration failed; keeping grid selection"
                );
                self.window_runtime
                    .selection_autoscroll_active
                    .store(false, Ordering::Relaxed);
                return false;
            }
        }
        let Some((dir, overshoot_px)) = self.selection_autoscroll_overshoot(y) else {
            self.window_runtime
                .selection_autoscroll_active
                .store(false, Ordering::Relaxed);
            return false;
        };
        // Live max_scroll (same A1 pattern as `scroll_local_view`): the
        // per-frame cached metric is None during grid-mode execution, which
        // would clamp to 0 and make the autoscroll a no-op.
        let max_scroll = self
            .renderer
            .as_ref()
            .and_then(|renderer| {
                let tab = self.sessions.active()?;
                let terminal = tab.lock_terminal()?;
                Some(
                    renderer
                        .block_scroll_metrics(&terminal, tab.pane_session_id)
                        .2,
                )
            })
            .unwrap_or(0);
        // Warp ramp speed, whole rows with fractional carry.
        let raw = autoscroll_ramp_rows(overshoot_px);
        let total = self.interaction.selection_autoscroll_carry + raw;
        let steps = total.floor().max(1.0) as usize;
        self.interaction.selection_autoscroll_carry = total - steps as f32;
        // At the history top/bottom the saturating scroll has no net effect;
        // detect that so the 40ms timer can stop instead of idle-spinning
        // (before = immutable read, then the mutable scroll, then re-read).
        let before = self.sessions.active().map_or(0, |tab| tab.block_scroll());
        if let Some(tab) = self.sessions.active_mut() {
            match dir {
                AutoscrollDir::Up => {
                    tab.scroll_up_by(steps);
                    tab.clamp_block_scroll(max_scroll);
                }
                AutoscrollDir::Down => {
                    tab.scroll_down_by(steps);
                    tab.clamp_block_scroll(max_scroll);
                }
            }
        }
        let moved = before != self.sessions.active().map_or(0, |tab| tab.block_scroll());
        // Extend the endpoint: clamp y into the content band so the shared
        // row hit-test naturally lands on the new edge row.
        let (top, bottom) = self.block_content_vbounds().unwrap_or((0.0, 0.0));
        let cy = (y as f32).clamp(top, (bottom - 1.0).max(top));
        if let Some(anchor) = self.pixel_to_block_view_pos(x, cy as f64) {
            if let Some(tab) = self.sessions.active_mut() {
                tab.selection_handler.extend_block_view(anchor);
            }
        }
        self.window_runtime
            .selection_autoscroll_active
            .store(moved, Ordering::Relaxed);
        true
    }
}
