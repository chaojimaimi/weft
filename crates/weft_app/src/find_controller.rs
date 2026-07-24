//! Find overlay controller extracted from the application shell.

use super::*;
use crate::block_component::{completed_block_layout_rows, completed_block_match_row_from_bottom};

pub(crate) fn toggled_find_option(current: bool) -> bool {
    !current
}

fn block_find_scroll_target(
    blocks: &[weft_core::blocks::Block],
    hit: &weft_core::find::BlockMatch,
    header_rows: usize,
    cols: usize,
    viewport_rows: usize,
    visible: usize,
    max_scroll: usize,
) -> Option<usize> {
    let block_idx = blocks.iter().position(|block| block.id == hit.block_id)?;
    let rows_below = blocks
        .iter()
        .skip(block_idx + 1)
        .map(|block| completed_block_layout_rows(block, cols, header_rows, viewport_rows))
        .sum::<usize>();
    let row_in_block = completed_block_match_row_from_bottom(&blocks[block_idx], hit, cols);
    Some(
        (rows_below + row_in_block)
            .saturating_sub(visible * 2 / 3)
            .min(max_scroll),
    )
}

fn grid_find_scroll_target(scrollback_len: usize, viewport_rows: usize, match_row: usize) -> usize {
    if match_row >= scrollback_len {
        return 0;
    }
    let upper_middle_from_top = viewport_rows / 3;
    (scrollback_len + upper_middle_from_top)
        .saturating_sub(match_row)
        .min(scrollback_len)
}

impl App {
    pub(super) fn arm_find_refresh(&mut self) {
        self.find.arm_refresh(std::time::Instant::now());
        self.find.worker.schedule_debounce(
            self.find.worker_generation,
            std::time::Duration::from_millis(160),
        );
        self.request_redraw();
    }

    /// Handle keys while the FindInGrid bar is open. The bar consumes all
    /// non-modifier keystrokes into the query input; Esc closes, Enter /
    /// Shift+Enter navigate next/prev match, Cmd+F toggles closed (handled
    /// by the keybinding resolution above, so it never reaches here).
    pub(super) fn handle_find_key(
        &mut self,
        key: KeyCode,
        mods: Modifiers,
        text: Option<&str>,
    ) -> bool {
        // Cmd+R: toggle regex mode and refresh the real async/block searches.
        if mods.contains(Modifiers::SUPER) && key == KeyCode::Char('r') {
            self.find.regex_mode = toggled_find_option(self.find.regex_mode);
            self.arm_find_refresh();
            return true;
        }
        // Cmd+I: toggle case-sensitive search.
        if mods.contains(Modifiers::SUPER) && key == KeyCode::Char('i') {
            self.find.case_sensitive = toggled_find_option(self.find.case_sensitive);
            self.arm_find_refresh();
            return true;
        }
        if mods.intersects(Modifiers::SUPER | Modifiers::CONTROL | Modifiers::ALT) {
            return false;
        }
        use crate::paint::command_surface::CommandSurfaceKeyAction;
        match crate::paint::command_surface::resolve_command_surface_key(key, mods) {
            CommandSurfaceKeyAction::Cancel => {
                self.close_find();
                self.request_redraw();
                return true;
            }
            CommandSurfaceKeyAction::Accept => {
                // Shift+Enter = previous, Enter = next.
                self.find_cycle_next_prev(!mods.contains(Modifiers::SHIFT));
                return true;
            }
            CommandSurfaceKeyAction::MoveUp => {
                self.find_cycle_next_prev(false);
                return true;
            }
            CommandSurfaceKeyAction::MoveDown => {
                self.find_cycle_next_prev(true);
                return true;
            }
            CommandSurfaceKeyAction::PageUp => {
                self.find_cycle_next_prev(false);
                return true;
            }
            CommandSurfaceKeyAction::PageDown => {
                self.find_cycle_next_prev(true);
                return true;
            }
            CommandSurfaceKeyAction::CycleFocus => {
                self.find_cycle_next_prev(!mods.contains(Modifiers::SHIFT));
                return true;
            }
            CommandSurfaceKeyAction::Unhandled => {}
        }

        match key {
            KeyCode::Backspace => {
                if self.find.query.pop().is_some() {
                    self.arm_find_refresh();
                }
                true
            }
            _ => {
                // v0.9 fix: use resolve_text_char to fall back to the key
                // char when `text` is None (winit doesn't populate text for
                // all printable keys, e.g. `-` on some layouts). This
                // matches the editor's behavior.
                if let KeyCode::Char(c) = key {
                    let resolved = resolve_text_char(text, c, mods.contains(Modifiers::SHIFT));
                    if !resolved.is_control() {
                        self.find.query.push(resolved);
                        self.arm_find_refresh();
                        return true;
                    }
                }
                if let Some(t) = text {
                    if !t.is_empty() {
                        self.find.query.push_str(t);
                        self.arm_find_refresh();
                        return true;
                    }
                }
                false
            }
        }
    }

    /// Cycle the find popup's current match forward (`next = true`) or
    /// backward (`next = false`). Used by Enter / Shift+Enter, Up/Down
    /// arrow keys, and the up/down buttons in the popup. In block view,
    /// cycles through block matches; in grid view, cycles through grid
    /// matches. No-op when there are no matches.
    pub(super) fn find_cycle_next_prev(&mut self, next: bool) {
        if !self.find.block_matches.is_empty() && self.block_view_active() {
            let len = self.find.block_matches.len();
            if next {
                self.find.block_index = (self.find.block_index + 1) % len;
            } else if self.find.block_index == 0 {
                self.find.block_index = len - 1;
            } else {
                self.find.block_index -= 1;
            }
            // v0.9 fix: auto-expand the block containing the current match so
            // the highlighted hit is visible. If a match is inside a folded
            // block's output, expanding it reveals the matching line. Command
            // matches are always visible (the command line shows even when
            // folded), so only expand for output matches.
            let need_expand = self
                .find
                .block_matches
                .get(self.find.block_index)
                .map(|bm| !bm.is_command)
                .unwrap_or(false);
            if need_expand {
                if let Some(bm) = self.find.block_matches.get(self.find.block_index) {
                    if let Some(term) = self.sessions.active_mut().terminal.as_mut() {
                        let block = term
                            .block_tracker()
                            .session_blocks()
                            .iter()
                            .find(|b| b.id == bm.block_id)
                            .cloned();
                        if let Some(b) = block {
                            if b.collapsed {
                                term.block_tracker_mut().toggle_collapse(b.id);
                            }
                        }
                    }
                }
            }
            self.scroll_to_current_find_match();
            self.request_redraw();
        } else if !self.find.matches.is_empty() {
            let len = self.find.matches.len();
            if next {
                self.find.index = (self.find.index + 1) % len;
            } else if self.find.index == 0 {
                self.find.index = len - 1;
            } else {
                self.find.index -= 1;
            }
            self.scroll_to_current_find_match();
            self.request_redraw();
        }
    }

    /// Run the search if the debounce window has elapsed. Called from the
    /// redraw path; safe to call every frame — it no-ops when no search is
    /// pending or the debounce hasn't expired.
    ///
    /// v0.9 U-P1: grid search is now async — we submit a `FindSnapshot` to
    /// the background `FindWorker` and drain results in `poll_find_worker_results`.
    /// This avoids blocking the render thread on large scrollbacks. Block
    /// search stays synchronous (block output is plain `String` — scanning
    /// is O(text size), not O(grid cells × flags), and is rarely the bottleneck).
    pub(super) fn maybe_refresh_find_results(&mut self) {
        let Some(t) = self.find.last_key else {
            return;
        };
        if t.elapsed() < std::time::Duration::from_millis(150) {
            return;
        }
        self.find.last_key = None;

        // Borrow the active tab's terminal via `active_mut()` — this borrows
        // `self.sessions` mutably, but `self.find` is a disjoint field of
        // `self`, so the find_* fields below can be mutated without conflict.
        let Some(term) = self.sessions.active_mut().terminal.as_ref() else {
            return;
        };

        // Clear any stale regex error when starting a new search.
        self.find.regex_error = None;

        // Submit grid search to the background worker (async, non-blocking).
        // The snapshot creation (~2-3ms for 10K rows) is the only main-thread
        // cost; the scan itself runs on the worker thread.
        if !self.find.query.is_empty() {
            let snapshot = std::sync::Arc::new(term.grid().find_snapshot());
            self.find.worker_generation = self.find.worker.submit(
                self.find.query.clone(),
                self.find.case_sensitive,
                self.find.regex_mode,
                snapshot,
            );
            self.find.worker_busy = true;
        } else {
            // Empty query → no matches. Clear immediately (no need to wait
            // for the worker).
            self.find.matches.clear();
            self.find.truncated = false;
            self.find.index = 0;
            self.find.worker_busy = false;
        }

        // Search block history + in-flight block synchronously when in
        // block view. This is fast (String scanning) and the matches are
        // needed immediately for the FindUI count.
        // v0.9 U-P2: pass is_regex so regex mode works in block view too.
        if term.show_block_view() {
            let blocks = term.block_tracker().session_blocks();
            let mut block_matches = match weft_core::find::find_in_blocks(
                blocks,
                &self.find.query,
                self.find.case_sensitive,
                self.find.regex_mode,
            ) {
                Ok(m) => m,
                Err(e) => {
                    self.find.regex_error = Some(e.0);
                    self.find.block_matches.clear();
                    self.find.block_truncated = false;
                    self.find.block_index = 0;
                    self.request_redraw();
                    return;
                }
            };
            if let Some(live) = term.block_tracker().in_flight() {
                match weft_core::find::find_in_flight(
                    &live,
                    &self.find.query,
                    self.find.case_sensitive,
                    self.find.regex_mode,
                ) {
                    Ok(mut live_matches) => block_matches.append(&mut live_matches),
                    Err(e) => {
                        self.find.regex_error = Some(e.0);
                    }
                }
            }
            self.find.block_truncated =
                block_matches.len() >= weft_core::find::MAX_MATCHES && !self.find.query.is_empty();
            self.find.block_matches = block_matches;
            if !self.find.block_matches.is_empty() {
                self.find.block_index =
                    self.find.block_index.min(self.find.block_matches.len() - 1);
                self.scroll_to_current_find_match();
            } else {
                self.find.block_index = 0;
            }
        } else {
            self.find.block_matches.clear();
            self.find.block_truncated = false;
            self.find.block_index = 0;
        }

        self.request_redraw();
    }

    /// Drain pending find-worker results (v0.9 U-P1). Called every frame
    /// from the redraw path. When a `Complete` or `Partial` result arrives,
    /// updates `find_matches` / `find_truncated` and scrolls to the current
    /// match. When `RegexInvalid` arrives, surfaces the error in the FindUI.
    pub(super) fn poll_find_worker_results(&mut self) {
        if !self.find.worker_busy {
            return;
        }
        while let Some(result) = self.find.worker.try_recv_result() {
            let generation = match &result {
                find_worker::FindResult::Partial { generation, .. }
                | find_worker::FindResult::Complete { generation, .. }
                | find_worker::FindResult::RegexInvalid { generation, .. }
                | find_worker::FindResult::Cancelled { generation } => *generation,
            };
            if generation != self.find.worker_generation {
                continue;
            }
            match result {
                find_worker::FindResult::Partial { matches, .. } => {
                    // Incremental results — paint them so the user sees
                    // matches appear as the scan progresses.
                    // v0.9 fix: update find_matches regardless of block view —
                    // grid matches are needed for the FindUI count and for
                    // scrolling when the user navigates. Block matches are a
                    // separate field and don't conflict.
                    if !matches.is_empty() {
                        self.find.index = self.find.index.min(matches.len() - 1);
                    } else {
                        self.find.index = 0;
                    }
                    self.find.matches = matches;
                    if !self.block_view_active() {
                        self.scroll_to_current_find_match();
                    }
                    self.request_redraw();
                }
                find_worker::FindResult::Complete {
                    matches, truncated, ..
                } => {
                    if !matches.is_empty() {
                        self.find.index = self.find.index.min(matches.len() - 1);
                    } else {
                        self.find.index = 0;
                    }
                    self.find.matches = matches;
                    self.find.truncated = truncated;
                    if !self.block_view_active() {
                        self.scroll_to_current_find_match();
                    }
                    self.find.worker_busy = false;
                    self.request_redraw();
                    // Done — break out of the drain loop.
                    break;
                }
                find_worker::FindResult::RegexInvalid { message, .. } => {
                    self.find.regex_error = Some(message);
                    self.find.matches.clear();
                    self.find.truncated = false;
                    self.find.index = 0;
                    self.find.worker_busy = false;
                    self.request_redraw();
                    break;
                }
                find_worker::FindResult::Cancelled { .. } => {
                    // A newer query is in flight — keep `find_worker_busy`
                    // true; the newer query's results will arrive soon.
                }
            }
        }
    }

    /// Scroll the viewport so the current find match is visible. In grid
    /// view, adjusts `grid.scroll_offset` to bring the match to the middle
    /// viewport row. In block view, adjusts `block_scroll_offset` to bring
    /// the matching block into the visible region.
    pub(super) fn scroll_to_current_find_match(&mut self) {
        // Block view: scroll to the block containing the current block match.
        if self.block_view_active() && !self.find.block_matches.is_empty() {
            let bm = self.find.block_matches.get(self.find.block_index).cloned();
            let Some(bm) = bm else { return };
            let Some(term) = self.sessions.active_mut().terminal.as_ref() else {
                return;
            };
            let blocks = term.block_tracker().session_blocks();
            // Bring it to roughly the upper-middle of the viewport so the
            // user sees context below and above the match.
            let Some(renderer) = self.renderer.as_ref() else {
                return;
            };
            let cwd_header = crate::layout::block_cwd_header_active(
                term.effective_input_mode() == weft_core::input::InputMode::Editor,
                term.cwd().is_some(),
            );
            let visible = renderer
                .block_visible_rows(crate::block_component::block_prompt_lines(term), cwd_header);
            // Scroll so the matching row lands at ~2/3 from the bottom of the
            // viewport (upper-middle). block_scroll_offset is "rows scrolled
            // up from the bottom", so target = rows_from_bottom - visible*2/3.
            let cols = term.grid().num_cols;
            let cache = renderer.block_layout_cache.borrow();
            let (total, _) = block_content_metrics_with_cache(
                term,
                cols,
                renderer.block_header_rows(),
                Some(&*cache),
            );
            let max_scroll = total.saturating_sub(visible);
            let Some(target) = block_find_scroll_target(
                blocks,
                &bm,
                renderer.block_header_rows(),
                cols,
                term.grid().num_rows,
                visible,
                max_scroll,
            ) else {
                return;
            };
            self.sessions.active_mut().set_block_scroll(target);
            return;
        }
        // Grid view: scroll grid to bring the match to the upper-middle of the
        // viewport (2/3 from the bottom) so context is visible below the match.
        let Some(m) = self.find.matches.get(self.find.index).copied() else {
            return;
        };
        let Some(term) = self.sessions.active_mut().terminal.as_mut() else {
            return;
        };
        let grid = term.grid_mut();
        let sb_len = grid.scrollback_len();
        let target_offset = grid_find_scroll_target(sb_len, grid.num_rows, m.row);
        if grid.scroll_offset != target_offset {
            grid.scroll_offset = target_offset;
            term.clear_hyperlink_cell_map();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{block_find_scroll_target, grid_find_scroll_target, toggled_find_option};
    use crate::app_state::FindState;
    use std::time::{Instant, SystemTime};
    use weft_core::blocks::{Block, BlockId};
    use weft_core::find::{BlockMatch, FindMatch};

    fn block(id: u64, output: &str) -> Block {
        Block {
            id: BlockId(id),
            command: format!("command-{id}"),
            cwd: None,
            output: output.into(),
            styled_output: None,
            exit_code: Some(0),
            started_at: SystemTime::UNIX_EPOCH,
            finished_at: Some(SystemTime::UNIX_EPOCH),
            collapsed: false,
        }
    }

    #[test]
    fn block_find_scroll_places_old_output_in_upper_middle() {
        let blocks = vec![block(1, "a\nb\nc\n"), block(2, "x\ny\n")];
        let hit = BlockMatch {
            block_id: BlockId(1),
            is_command: false,
            line: 0,
            col: 0,
            len: 1,
        };
        assert_eq!(
            block_find_scroll_target(&blocks, &hit, 1, 80, 24, 6, 100),
            Some(4)
        );
        assert_eq!(
            block_find_scroll_target(&blocks, &hit, 2, 80, 24, 6, 100),
            Some(5)
        );

        let mut blocks_with_clear = blocks;
        blocks_with_clear[1].command = "clear".into();
        assert_eq!(
            block_find_scroll_target(&blocks_with_clear, &hit, 1, 80, 24, 6, 100),
            Some(28)
        );
    }

    #[test]
    fn block_find_scroll_distinguishes_command_and_output_orientation() {
        let blocks = vec![block(1, "a\nb\nc\n")];
        let mut hit = BlockMatch {
            block_id: BlockId(1),
            is_command: false,
            line: 2,
            col: 0,
            len: 1,
        };
        assert_eq!(
            block_find_scroll_target(&blocks, &hit, 2, 80, 24, 3, 100),
            Some(0)
        );
        hit.is_command = true;
        assert_eq!(
            block_find_scroll_target(&blocks, &hit, 2, 80, 24, 3, 100),
            Some(2)
        );
    }

    #[test]
    fn block_find_scroll_counts_wrapped_rows_and_resume_hints() {
        let mut older = block(
            1,
            "first row is deliberately much longer than eight columns\nlast",
        );
        let newer = block(2, "newer output also wraps at narrow widths");
        let hit = BlockMatch {
            block_id: older.id,
            is_command: false,
            line: 0,
            col: 20,
            len: 4,
        };
        let wide =
            block_find_scroll_target(&[older.clone(), newer.clone()], &hit, 1, 80, 24, 3, 100)
                .unwrap();
        let narrow =
            block_find_scroll_target(&[older.clone(), newer], &hit, 1, 8, 24, 3, 100).unwrap();
        assert!(narrow > wide);

        older.command = "opencode".into();
        older.exit_code = Some(130);
        let with_hints = block_find_scroll_target(&[older], &hit, 1, 8, 24, 3, 100).unwrap();
        assert!(with_hints > 0, "resume hints must contribute visual rows");
    }

    #[test]
    fn grid_find_scroll_targets_upper_middle_or_live_viewport() {
        assert_eq!(grid_find_scroll_target(100, 30, 90), 20);
        assert_eq!(grid_find_scroll_target(100, 30, 20), 90);
        assert_eq!(grid_find_scroll_target(100, 30, 100), 0);
    }

    #[test]
    fn keyboard_find_option_toggle_rearms_existing_query() {
        assert!(toggled_find_option(false));
        assert!(!toggled_find_option(true));
    }

    #[test]
    fn query_change_invalidates_old_results_before_debounce() {
        let mut state = FindState::new_for_test();
        state.open = true;
        state.query = "new query".into();
        state.matches.push(FindMatch {
            row: 1,
            col: 2,
            len: 3,
        });
        let old_generation = state.worker_generation;
        state.arm_refresh(Instant::now());
        assert!(state.worker_generation > old_generation);
        assert!(state.worker_busy);
        assert!(state.matches.is_empty());
        assert!(state.last_key.is_some());

        state.close();
        assert!(!state.open);
        assert!(state.query.is_empty());
        assert!(!state.worker_busy);
    }
}
