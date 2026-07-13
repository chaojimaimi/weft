//! History panel controller extracted from the application shell.

use super::*;

impl App {
    /// Count of blocks visible in the panel (newest-first, query-filtered).
    pub(super) fn panel_visible_count(&self) -> usize {
        let Some(terminal) = self.sessions.active().terminal.as_ref() else {
            return 0;
        };
        let blocks = terminal.block_tracker().blocks();
        let visible = terminal.grid().num_rows;
        blocks
            .iter()
            .rev()
            .filter(|b| block_matches_query(b, &self.panel.query))
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
        let terminal = self.sessions.active().terminal.as_ref()?;
        let visible = terminal.grid().num_rows;
        terminal
            .block_tracker()
            .blocks()
            .iter()
            .rev()
            .filter(|b| block_matches_query(b, &self.panel.query))
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
            let Some(t) = self.sessions.active().terminal.as_ref() else {
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
                if let Some(t) = self.sessions.active_mut().terminal.as_mut() {
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
        // Only meaningful in block view (grid view has no block layout).
        if !self.block_view_active() {
            return;
        }
        let Some(term) = self.sessions.active_mut().terminal.as_ref() else {
            return;
        };
        let cwd_header_active = crate::layout::block_cwd_header_active(
            term.effective_input_mode() == weft_core::input::InputMode::Editor,
            term.cwd().is_some(),
        );
        let blocks = term.block_tracker().session_blocks();
        let block_idx = blocks.iter().position(|b| b.id == block_id);
        let Some(block_idx) = block_idx else { return };

        // Count rows from the bottom up to the target block's command line.
        // Layout (bottom→top): Output[N-1] at row 0, …, Output[0] at row
        // N-1, Command at row N, Header at row N+1, Separator at row N+2.
        // For each block BELOW the target (i.e. with higher index), add its
        // full height (3 + output_lines).
        let trim_output_lines = |b: &weft_core::blocks::Block| -> usize {
            if b.collapsed {
                return 0;
            }
            let mut lines: Vec<&str> = b.output.lines().collect();
            while lines.last().is_some_and(|l| {
                let t = l.trim();
                t.is_empty() || matches!(t, "%" | "$" | "#")
            }) {
                lines.pop();
            }
            lines.len()
        };
        let mut rows_from_bottom = 0usize;
        for (i, b) in blocks.iter().enumerate().rev() {
            if i == block_idx {
                break;
            }
            rows_from_bottom += 3 + trim_output_lines(b);
        }
        // Position the block's command line at ~1/3 from the bottom of the
        // viewport so the user sees the command + most of its output above.
        let Some(renderer) = self.renderer.as_ref() else {
            return;
        };
        let visible = renderer.block_visible_rows(1, cwd_header_active);
        let target = rows_from_bottom.saturating_sub(visible / 3).max(0);
        self.sessions.active_mut().set_block_scroll(target);

        // Arm the highlight: accent border around the block for 1.5s.
        self.panel.highlight = Some(block_id);
        self.panel.highlight_until =
            Some(std::time::Instant::now() + std::time::Duration::from_millis(1500));
        self.request_redraw();
    }

    /// Local scrollback navigation (page up/down, top, bottom).
    pub(super) fn scroll_action(&mut self, action: Action) {
        let tab = self.sessions.active_mut();
        let Some(terminal) = &mut tab.terminal else {
            return;
        };
        let rows = terminal.grid().num_rows;
        let cols = terminal.grid().num_cols;
        // Block view uses a dedicated scroll offset.
        if terminal.show_block_view() {
            let (total, _) = block_content_metrics(terminal, cols);
            // Compute visible rows from the renderer's actual geometry.
            let prompt_lines = terminal.editor().buffer.lines.len();
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
            let max_scroll = total.saturating_sub(visible);
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
