//! Tab drag-to-reorder gesture controller (v1.11 ghost drag).
//!
//! Extracted from `mouse_controller.rs` (v1.11.13) to keep the mouse
//! dispatch file within its audited line budget. The gesture model:
//!
//! - press on a tab label records `TabBarDragState` (click-to-switch);
//!   press on the close "×" records the same state with `close_on_release`
//!   so a click closes but a drag reorders.
//! - moves past the 4px threshold engage the ghost drag: the `SessionManager`
//!   Vec stays untouched, the dragged tab renders as a floating pill at the
//!   pointer's grip, and `insert_index` tracks the gap slot under the pointer
//!   (midpoint rule, pure functions in `layout::drag`).
//! - release commits the reorder once (`move_tab`) or executes the deferred
//!   close; `cancel_tab_drag` aborts the gesture without committing when the
//!   pointer leaves the window, focus is lost, or a tab is closed mid-drag.

use super::*;

impl App {
    /// v1.11.13: Execute a deferred close request (press landed on a tab's
    /// close "×", released without exceeding the drag threshold). Performs
    /// the same close as the pre-deferral press handler — active tab closes
    /// with confirmation, background tab closes after its own confirmation.
    /// No-op when the tab vanished between press and release.
    pub(super) fn perform_close_request(&mut self, idx: usize) {
        if idx >= self.sessions.len() {
            return;
        }
        if idx == self.sessions.active_idx() {
            let effects = self.close_active_tab_with_confirmation();
            self.drain_effects(effects);
            return;
        }
        if !self.confirm_background_tab_close(idx) {
            return;
        }
        let blocks = self
            .sessions
            .tab_mut(idx)
            .map(crate::tab::Tab::finish_pending_blocks)
            .unwrap_or_default();
        self.sessions.close_background(idx);
        self.tab_bar.hovered_tab = None;
        let mut effects = Vec::new();
        if !blocks.is_empty() {
            effects.push(crate::effect::Effect::PersistBlocks { blocks });
        }
        effects.push(crate::effect::Effect::PersistTabs);
        self.drain_effects(effects);
    }

    /// v1.11.13: Cancel an in-flight tab drag without committing. Used when
    /// the pointer leaves the window, focus is lost, or a tab is closed
    /// mid-drag — otherwise the stale `tab_drag` state would keep the ghost
    /// pill on screen and swallow every subsequent CursorMoved.
    pub(super) fn cancel_tab_drag(&mut self) {
        if self.interaction.tab_drag.take().is_some() {
            self.request_redraw();
        }
    }

    /// v1.11: Handle tab drag-to-reorder. Called from CursorMoved before
    /// the normal pointer routing so the drag works even when the pointer is
    /// over the tab bar (not terminal content).
    ///
    /// Threshold: the pointer must move > 4px (Chebyshev distance) from the
    /// press position before the ghost drag engages. Below that, the gesture
    /// is treated as a plain click (tab already switched on press, or a
    /// deferred × close pending).
    ///
    /// Once engaged, the dragged tab is rendered as a ghost pill following
    /// the pointer (grip = `grab_offset`) and `insert_index` tracks the gap
    /// slot under the pointer (midpoint rule). The `SessionManager` order is
    /// untouched — the reorder commits once on release.
    pub(super) fn handle_tab_drag_move(&mut self, x: f64, y: f64) {
        let mut drag = match self.interaction.tab_drag {
            Some(d) => d,
            None => {
                tracing::debug!("TAB_DRAG_DIAG: handle_tab_drag_move called but tab_drag is None");
                return;
            }
        };
        let n = self.sessions.len();
        if drag.drag_index >= n {
            // Tab count shrank mid-drag (e.g. Cmd+W raced the gesture).
            // Drop the drag instead of indexing out of bounds.
            self.interaction.tab_drag = None;
            self.request_redraw();
            return;
        }

        // Threshold check — don't engage the drag until the pointer has
        // moved meaningfully. This preserves the click-to-switch (and the
        // deferred × close) behavior for sub-threshold movements.
        if !drag.moved {
            let dx = (x - drag.start_x).abs();
            let dy = (y - drag.start_y).abs();
            if dx.max(dy) <= 4.0 {
                return;
            }
            drag.moved = true;
            // A real drag revokes the deferred close request.
            drag.close_on_release = false;
            // Capture the grip: pointer X offset within the dragged tab's
            // slot at lift, so the ghost pill never jumps on the first move.
            if let Some(strip) = self.tab_strip_layout() {
                let slot_x = strip.tabs_start + drag.drag_index as f32 * strip.tab_width
                    - strip.scroll_offset;
                drag.grab_offset = (x as f32 - slot_x).clamp(0.0, strip.tab_width);
            }
        }

        // Track the gap slot under the pointer (midpoint rule). The layout
        // is rebuilt from the same geometry the renderer consumes, so the
        // gap and the hit positions can never drift apart.
        let insert = match self.tab_strip_layout() {
            Some(strip) => crate::layout::insertion_index_for_x(&strip, x as f32, n),
            None => drag.insert_index,
        };
        drag.insert_index = insert;
        self.interaction.tab_drag = Some(drag);
        self.request_redraw();
    }
}
