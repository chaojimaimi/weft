//! Find overlay controller extracted from the application shell.

use super::*;

impl App {
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
        // Cmd+R: toggle regex mode (visual indicator only — actual regex
        // search engine not yet wired, so this doesn't change results yet).
        if mods.contains(Modifiers::SUPER) && key == KeyCode::Char('r') {
            self.find.regex_mode = !self.find.regex_mode;
            self.request_redraw();
            return true;
        }
        // Cmd+I: toggle case-sensitive search.
        if mods.contains(Modifiers::SUPER) && key == KeyCode::Char('i') {
            self.find.case_sensitive = !self.find.case_sensitive;
            // Re-run the search immediately so the toggle is reflected.
            self.find.last_key = Some(std::time::Instant::now());
            self.request_redraw();
            return true;
        }
        if mods.intersects(Modifiers::SUPER | Modifiers::CONTROL | Modifiers::ALT) {
            return false;
        }
        match key {
            KeyCode::Escape => {
                self.find.open = false;
                self.request_redraw();
                true
            }
            KeyCode::Enter => {
                // Shift+Enter = previous, Enter = next.
                self.find_cycle_next_prev(!mods.contains(Modifiers::SHIFT));
                true
            }
            KeyCode::Up | KeyCode::Down => {
                // Arrow keys cycle prev/next, mirroring Warp's find popup.
                self.find_cycle_next_prev(key == KeyCode::Down);
                true
            }
            KeyCode::Backspace => {
                if self.find.query.pop().is_some() {
                    self.find.last_key = Some(std::time::Instant::now());
                    self.request_redraw();
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
                        self.find.last_key = Some(std::time::Instant::now());
                        self.request_redraw();
                        return true;
                    }
                }
                if let Some(t) = text {
                    if !t.is_empty() {
                        self.find.query.push_str(t);
                        self.find.last_key = Some(std::time::Instant::now());
                        self.request_redraw();
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
                    if let Some(term) = self.sessions.tabs[self.sessions.active_tab]
                        .terminal
                        .as_mut()
                    {
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

        // Access the active tab's terminal via direct field indexing so the
        // borrow is split to `self.sessions.tabs` — the find_* fields below can then
        // be mutated without a borrow conflict (going through `self.tab()`
        // would borrow all of `self`).
        let Some(term) = self.sessions.tabs[self.sessions.active_tab]
            .terminal
            .as_ref()
        else {
            return;
        };

        // Clear any stale regex error when starting a new search.
        self.find.regex_error = None;

        // Submit grid search to the background worker (async, non-blocking).
        // The snapshot creation (~2-3ms for 10K rows) is the only main-thread
        // cost; the scan itself runs on the worker thread.
        if !self.find.query.is_empty() {
            let snapshot = std::sync::Arc::new(term.grid().find_snapshot());
            self.find.worker.submit(
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
            match result {
                find_worker::FindResult::Partial { matches } => {
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
                find_worker::FindResult::Complete { matches, truncated } => {
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
                find_worker::FindResult::RegexInvalid(msg) => {
                    self.find.regex_error = Some(msg);
                    self.find.matches.clear();
                    self.find.truncated = false;
                    self.find.index = 0;
                    self.find.worker_busy = false;
                    self.request_redraw();
                    break;
                }
                find_worker::FindResult::Cancelled => {
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
            let Some(term) = self.sessions.tabs[self.sessions.active_tab]
                .terminal
                .as_ref()
            else {
                return;
            };
            // Find the block's index in session_blocks to compute its row
            // offset from the bottom. Blocks are laid out bottom-to-top:
            // the newest (highest index) is at the bottom. The row offset
            // from the bottom = sum of rows of all blocks BELOW it + its
            // own offset within. We approximate by scrolling to bring the
            // block's command line to the middle of the viewport.
            let blocks = term.block_tracker().session_blocks();
            let block_idx = blocks.iter().position(|b| b.id == bm.block_id);
            let Some(block_idx) = block_idx else { return };
            // Count rows from the bottom up to this block's matching line.
            // Actual layout (bottom→top within a block):
            //   Output[N-1] (last printed)  → row 1 from bottom
            //   Output[N-2]                 → row 2
            //   …
            //   Output[0] (first printed)   → row N
            //   Command                     → row N+1
            //   Header                      → row N+2
            //   Separator                   → row N+3
            // where N = trimmed output line count. So for an output match at
            // `bm.line`, the in-block offset from the bottom is `N - bm.line`.
            // For a command match, it's `N + 1`.
            // (The previous code used `bm.line + 3` which treated the layout
            // as top-to-bottom — that was inverted, causing the viewport to
            // jump to the wrong position and the highlight to land off-screen.)
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
            let matching_output_lines = trim_output_lines(&blocks[block_idx]);
            let line_in_block = if bm.is_command {
                matching_output_lines + 1
            } else {
                matching_output_lines.saturating_sub(bm.line)
            };
            rows_from_bottom += line_in_block;
            // Bring it to roughly the middle of the viewport.
            let Some(renderer) = self.renderer.as_ref() else {
                return;
            };
            let visible = renderer.block_visible_rows(1);
            // Scroll so the matching row lands at ~visible/2 from the bottom
            // of the viewport. block_scroll_offset is "rows scrolled up from
            // the bottom", so target = rows_from_bottom - visible/2.
            // (Previously this was ADDING visible/2, which scrolled PAST the
            // match — the highlight was drawn but outside the clip region.)
            let cols = term.grid().num_cols;
            let (total, _) = block_content_metrics(term, cols);
            let max_scroll = total.saturating_sub(visible);
            let target = rows_from_bottom.saturating_sub(visible / 2).min(max_scroll);
            self.sessions.tabs[self.sessions.active_tab].block_scroll_offset = target;
            return;
        }
        // Grid view: scroll grid to bring the match to the middle row.
        let Some(m) = self.find.matches.get(self.find.index).copied() else {
            return;
        };
        let Some(term) = self.sessions.tabs[self.sessions.active_tab]
            .terminal
            .as_mut()
        else {
            return;
        };
        let grid = term.grid_mut();
        let sb_len = grid.scrollback_len();
        let mid = grid.num_rows / 2;
        let target_offset = if m.row >= sb_len {
            0
        } else {
            (sb_len + mid).saturating_sub(m.row).min(sb_len)
        };
        if grid.scroll_offset != target_offset {
            grid.scroll_offset = target_offset;
            term.clear_hyperlink_cell_map();
        }
    }
}
