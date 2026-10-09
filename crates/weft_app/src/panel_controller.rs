//! History panel controller extracted from the application shell.

use super::*;
use crate::block_component::{completed_block_layout_rows, completed_block_output_rows};

impl App {
    pub(super) fn update_sidebar_drag(&mut self, pointer_x: f64) -> bool {
        let Some(drag) = self.interaction.sidebar_drag else {
            return false;
        };
        let Some(renderer) = &mut self.renderer else {
            return true;
        };
        let width = crate::ui_tokens::sidebar_width_after_drag(
            drag.start_width,
            drag.start_x,
            pointer_x,
            renderer.scale() as f32,
        );
        renderer.set_sidebar_width(Some(width));
        self.recompute_layout();
        self.request_redraw();
        true
    }

    /// v1.3.2: Update the pane divider ratio from the pointer position during
    /// a drag. Computes `new_ratio` from the drag state's `bounds` + the
    /// current pointer, calls `tab.set_pane_ratio`, then `recompute_layout`
    /// (which handles the throttled PTY resize). No-op if no drag is active.
    pub(super) fn update_pane_divider_drag(&mut self, x: f32, y: f32) -> bool {
        let Some(drag) = self.interaction.pane_divider_drag else {
            return false;
        };
        let [bx0, _by0, bx1, _by1] = drag.bounds;
        let new_ratio = match drag.axis {
            crate::paint::pane_dividers::DividerAxis::Vertical => {
                ((x - bx0) / (bx1 - bx0).max(1.0)).clamp(0.0, 1.0)
            }
            crate::paint::pane_dividers::DividerAxis::Horizontal => {
                let [_bx0, by0, _bx1, by1] = drag.bounds;
                ((y - by0) / (by1 - by0).max(1.0)).clamp(0.0, 1.0)
            }
        };
        // v1.12.25 (audit 3-B, P1-01): empty-tabs transient — nothing to
        // re-ratio; keep consuming the drag (same as the no-divider arm).
        match self
            .sessions
            .active_mut()
            .map(|tab| tab.set_pane_ratio(drag.first, drag.second, new_ratio))
        {
            Some(Ok(true)) => {
                self.recompute_layout();
                self.request_redraw();
                true
            }
            Some(Ok(false)) | None => true, // no divider / no tab; keep consuming
            Some(Err(e)) => {
                tracing::warn!(error = ?e, "pane divider drag: set_ratio failed; aborting drag");
                self.interaction.pane_divider_drag = None;
                false // release the drag so subsequent moves don't re-log
            }
        }
    }

    pub(super) fn finish_panel_scrollbar_drag(&mut self) -> bool {
        if self.interaction.panel_scrollbar_drag.take().is_none() {
            return false;
        }
        self.request_redraw();
        true
    }

    pub(super) fn update_panel_scrollbar_drag(&mut self, pointer_y: f32) -> bool {
        let Some(drag) = self.interaction.panel_scrollbar_drag else {
            return false;
        };
        self.panel.scroll_offset = crate::panel_scrollbar::scroll_offset_for_pointer(
            &drag.layout,
            pointer_y,
            drag.grab_offset,
        );
        self.clamp_panel_scroll();
        self.clamp_panel_selection();
        self.request_redraw();
        true
    }

    /// F3-4: Maximum number of block rows that fit in the panel's visible list
    /// area, based on the renderer's actual viewport and cell height. Falls
    /// back to the grid row count when the renderer isn't available yet.
    fn panel_max_visible(&self) -> usize {
        if let Some(r) = self.renderer.as_ref() {
            return visible_panel_rows(r.viewport.1, r.cell_height());
        }
        self.sessions
            .active()
            .and_then(|tab| tab.with_terminal(|t| t.grid().num_rows))
            .unwrap_or(0)
    }

    /// F3-4: Total count of blocks matching the panel query (no truncation).
    /// Used to clamp `scroll_offset` so the list can't scroll past the end.
    pub(super) fn panel_total_filtered(&self) -> usize {
        let total = self
            .sessions
            .active()
            .and_then(|tab| {
                tab.with_terminal(|t| {
                    panel_filtered_count(t.block_tracker().blocks(), &self.panel.query)
                })
            })
            .unwrap_or(0);
        total
    }

    /// F3-4: Clamp `panel.scroll_offset` to `[0, max(0, total - visible)]`.
    pub(super) fn clamp_panel_scroll(&mut self) {
        let total = self.panel_total_filtered();
        let visible = self.panel_max_visible();
        self.panel.scroll_offset =
            crate::paint::ui_helpers::clamp_panel_scroll(total, visible, self.panel.scroll_offset);
    }

    /// Count of blocks visible in the current panel window (after
    /// `scroll_offset` skip, newest-first, query-filtered).
    pub(super) fn panel_visible_count(&self) -> usize {
        let Some(terminal) = self.sessions.active().and_then(|tab| tab.lock_terminal()) else {
            return 0;
        };
        let blocks = terminal.block_tracker().blocks();
        let visible = self.panel_max_visible();
        blocks
            .iter()
            .rev()
            .filter(|b| block_matches_query(b, &self.panel.query))
            .skip(self.panel.scroll_offset)
            .take(visible)
            .count()
    }

    /// Keep the selection inside the filtered, visible list.
    pub(super) fn clamp_panel_selection(&mut self) {
        let max = self.panel_visible_count();
        if max == 0 {
            self.panel.selection = 0;
        } else {
            self.panel.selection = self.panel.selection.min(max - 1);
        }
    }

    /// The [`BlockId`] of the currently selected panel row, if any.
    pub(super) fn panel_selected_block_id(&self) -> Option<BlockId> {
        let terminal = self.sessions.active().and_then(|tab| tab.lock_terminal())?;
        let visible = self.panel_max_visible();
        terminal
            .block_tracker()
            .blocks()
            .iter()
            .rev()
            .filter(|b| block_matches_query(b, &self.panel.query))
            .skip(self.panel.scroll_offset)
            .take(visible)
            .nth(self.panel.selection)
            .map(|b| b.id)
    }

    /// v0.9 fix: send the panel's currently-selected command to the prompt
    /// editor (Warp-style "click/Enter to rerun"). Looks up the block by id,
    /// strips any prompt prefix, and sets the editor buffer. Silently no-ops
    /// when not at the prompt (command running / alt-screen active) to avoid
    /// stashing text that would resurface unexpectedly.
    pub(super) fn send_panel_selection_to_input(&mut self) {
        let block_id = match self.panel_selected_block_id() {
            Some(id) => id,
            None => return,
        };
        // Borrow the terminal immutably to find the command, then release
        // before mutating the editor.
        let cmd: Option<String> = {
            let Some(t) = self.sessions.active().and_then(|tab| tab.lock_terminal()) else {
                return;
            };
            if t.effective_input_mode() != weft_core::input::InputMode::Editor {
                return;
            }
            t.block_tracker()
                .blocks()
                .iter()
                .find(|b| b.id == block_id)
                .map(|b| strip_prompt_prefix(&b.command))
        };
        if let Some(cmd) = cmd {
            if !cmd.is_empty() {
                if let Some(mut t) = self
                    .sessions
                    .active_mut()
                    .and_then(|tab| tab.lock_terminal())
                {
                    t.editor_mut().buffer.set_text(&cmd);
                    // v0.9: select all so Cmd+C copies the command without
                    // needing a drag-select first. The user can still adjust
                    // the selection by clicking in the prompt box.
                    t.editor_mut().buffer.select_all();
                }
                // Unfocus the panel so the editor gets subsequent keystrokes.
                self.panel.search_focused = false;
                self.request_redraw();
            }
        }
    }

    /// v0.9 W2: scroll the terminal's block view so the panel-selected block
    /// is visible, and arm a 1.5s accent highlight. Called when the user
    /// clicks a row in the history panel (or presses Cmd+Enter while the
    /// panel is open).
    pub(super) fn scroll_to_panel_selection(&mut self) {
        let block_id = match self.panel_selected_block_id() {
            Some(id) => id,
            None => return,
        };
        self.scroll_to_block_id(block_id);
    }

    pub(super) fn scroll_to_block_id(&mut self, block_id: BlockId) {
        // Only meaningful in block view (grid view has no block layout).
        if !self.block_view_active() {
            return;
        }
        // T10 P1 (D9 rule 2): `set_block_scroll` below re-enters the
        // terminal lock (→ sync_primary_history_view), so this guard covers
        // only the read/layout phase and is dropped before the scroll.
        let target = {
            let Some(term) = self
                .sessions
                .active_mut()
                .and_then(|tab| tab.lock_terminal())
            else {
                return;
            };
            let cwd_header_active = crate::layout::block_cwd_header_active(
                term.effective_input_mode() == weft_core::input::InputMode::Editor,
                term.cwd().is_some(),
            );
            let prompt_lines = crate::block_component::block_prompt_lines(&term);
            let blocks = term.block_tracker().session_blocks();
            let viewport_rows = term.grid().num_rows;
            let cols = term.grid().num_cols;
            let block_idx = blocks.iter().position(|b| b.id == block_id);
            let Some(block_idx) = block_idx else { return };

            // Count rows from the bottom up to the target block's command line.
            // Layout (bottom→top): Output[N-1] at row 0, …, Output[0] at row
            // N-1, Command at row N, then the accessible Header band and gap.
            // For each block BELOW the target (i.e. with higher index), add its
            // full height using the renderer's current Header row span.
            let header_rows = self.renderer.as_ref().map_or(1, |r| r.block_header_rows());
            let mut rows_from_bottom = 0usize;
            for (i, b) in blocks.iter().enumerate().rev() {
                if i == block_idx {
                    break;
                }
                rows_from_bottom +=
                    completed_block_layout_rows(b, cols, header_rows, viewport_rows);
            }
            rows_from_bottom += completed_block_output_rows(&blocks[block_idx], cols);
            // Position the block's command line at ~1/3 from the bottom of the
            // viewport so the user sees the command + most of its output above.
            let Some(renderer) = self.renderer.as_ref() else {
                return;
            };
            let visible = renderer.block_visible_rows(prompt_lines, cwd_header_active);
            // v1.11.16: `saturating_sub` already floors at 0 — the trailing
            // `.max(0)` was dead (clippy::unnecessary_min_or_max under CI's
            // `-D warnings`).
            rows_from_bottom.saturating_sub(visible / 3)
        };
        if let Some(tab) = self.sessions.active_mut() {
            tab.set_block_scroll(target);
        }

        // Arm the highlight: accent border around the block for 1.5s.
        self.panel.highlight = Some(block_id);
        self.panel.highlight_until =
            Some(std::time::Instant::now() + std::time::Duration::from_millis(1500));
        self.request_redraw();
    }

    pub(super) fn navigate_to_block(&mut self, block_id: BlockId) -> bool {
        let target = self
            .sessions
            .tabs()
            .iter()
            .enumerate()
            .find_map(|(tab_index, tab)| {
                tab.panes().find_map(|(pane_id, pane)| {
                    pane.with_terminal(|terminal| {
                        terminal
                            .block_tracker()
                            .session_blocks()
                            .iter()
                            .any(|block| block.id == block_id)
                    })
                    .unwrap_or(false)
                    .then_some((tab_index, pane_id))
                })
            });
        let Some((tab_index, pane_id)) = target else {
            return false;
        };
        self.sessions.set_active(tab_index);
        // v1.12.25 (audit 3-B, P1-01): empty-tabs transient — target lookup
        // already returned false above, so `None` here is defensive only.
        if self
            .sessions
            .active_mut()
            .is_some_and(|tab| tab.set_active_pane(pane_id).is_err())
        {
            return false;
        }
        self.scroll_to_block_id(block_id);
        true
    }

    /// Local scrollback navigation (page up/down, top, bottom).
    pub(super) fn scroll_action(&mut self, action: Action) {
        let header_rows = self.renderer.as_ref().map_or(1, |r| r.block_header_rows());
        // v1.12.25 (audit 3-B, P1-01): empty-tabs transient — no view to
        // scroll; ignore the action.
        let Some(tab) = self.sessions.active_mut() else {
            return;
        };
        if matches!(
            action,
            Action::ScrollPageUp | Action::ScrollLineUp | Action::ScrollToTop
        ) {
            tab.enter_primary_history_if_active();
        }
        // T10 P1 (D9 rule 2): the block-branch scroll calls re-enter the
        // terminal lock (set_block_scroll → sync_primary_history_view), so the
        // guard below only covers the read/metrics phase and is released
        // before the match.
        //
        // [P2 TOCTOU 登记·评审裁定本轮不修] the read/metrics lock and the
        // apply lock are deliberately SEPARATE (the scroll calls re-enter);
        // the parse worker can change grid/metrics between the two, so the
        // scroll target can be computed from one-batch-stale metrics. Same
        // staleness the pre-worker pump produced between its read and the
        // same-frame scroll; accepted for P2.
        let (rows, _cols, block_view, max_scroll) = {
            let Some(terminal) = tab.lock_terminal() else {
                return;
            };
            let rows = terminal.grid().num_rows;
            let cols = terminal.grid().num_cols;
            // Block view uses a dedicated scroll offset.
            let block_view = terminal.show_block_view();
            let max_scroll = if block_view {
                let cache = self
                    .renderer
                    .as_ref()
                    .map(|r| r.block_layout_cache.borrow());
                let (total, _) = block_content_metrics_with_cache(
                    &terminal,
                    cols,
                    header_rows,
                    cache.as_deref(),
                    None,
                );
                // Compute visible rows from the renderer's actual geometry.
                let prompt_lines = crate::block_component::block_prompt_lines(&terminal);
                let visible = self
                    .renderer
                    .as_ref()
                    .map(|r| {
                        let cwd_header = crate::layout::block_cwd_header_active(
                            terminal.effective_input_mode() == weft_core::input::InputMode::Editor,
                            terminal.cwd().is_some(),
                        );
                        r.block_visible_rows(prompt_lines, cwd_header)
                    })
                    .unwrap_or(rows);
                total.saturating_sub(visible)
            } else {
                0
            };
            (rows, cols, block_view, max_scroll)
        };
        if block_view {
            match action {
                Action::ScrollPageUp => {
                    tab.scroll_up_by(rows);
                    tab.clamp_block_scroll(max_scroll);
                }
                Action::ScrollPageDown => {
                    tab.scroll_down_by(rows);
                }
                Action::ScrollLineUp => {
                    tab.scroll_up_by(1);
                    tab.clamp_block_scroll(max_scroll);
                }
                Action::ScrollLineDown => {
                    tab.scroll_down_by(1);
                }
                Action::ScrollToTop => {
                    tab.set_block_scroll(max_scroll);
                }
                Action::ScrollToBottom => tab.snap_to_bottom(),
                _ => {}
            }
        } else {
            let Some(mut terminal) = tab.lock_terminal() else {
                return;
            };
            let grid = terminal.grid_mut();
            match action {
                Action::ScrollPageUp => grid.scroll_up_history(rows),
                Action::ScrollPageDown => grid.scroll_down_history(rows),
                Action::ScrollLineUp => grid.scroll_up_history(1),
                Action::ScrollLineDown => grid.scroll_down_history(1),
                Action::ScrollToTop => grid.scroll_to_top(),
                Action::ScrollToBottom => grid.scroll_to_bottom(),
                _ => {}
            }
        }
        self.request_redraw();
    }
}
