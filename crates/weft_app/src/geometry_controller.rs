//! Shared layout and hit-testing controller.

use super::*;

mod targets;

/// Action triggered by clicking a button in the find popup. Produced by
/// `App::find_button_at` (this controller — the hit-test lives here) from
/// the find scene's hit-test rects.
///
/// v1.11.13 (PLAN_v11113 §M5): moved here from main.rs — the enum belongs
/// to the controller that produces and consumes it.
pub(crate) enum FindButtonAction {
    /// Click the ".*" toggle — flip regex mode (visual only).
    ToggleRegex,
    /// Click the "Aa" toggle — flip case-sensitive search.
    ToggleCase,
    /// Click the "↓" button — jump to next match.
    Next,
    /// Click the "↑" button — jump to previous match.
    Prev,
}

impl App {
    pub(super) fn active_scrollbar_layout(
        &self,
    ) -> Option<crate::scrollbar_component::ScrollbarLayout> {
        let renderer = self.renderer.as_ref()?;
        let ctx = renderer.layout_ctx?;
        // v1.12.25 (audit 3-B, P1-01): empty-tabs transient reads as None.
        let tab = self.sessions.active()?;
        let terminal = tab.lock_terminal()?;
        if !terminal.show_block_view() {
            return None;
        }
        // Step 3: read cached scroll metrics from the last draw() instead of
        // re-running block_content_metrics_with_cache (O(n)) on every mouse
        // move. The cache is written at the end of each draw() call; mouse
        // events read the previous frame's metrics (1-frame lag is
        // imperceptible for scrollbar hit-testing).
        let (total, visible, max_scroll) = renderer.cached_scroll_metrics.get()?;
        crate::scrollbar_component::scrollbar_layout(
            &ctx,
            total,
            visible,
            max_scroll,
            tab.block_scroll(),
        )
    }

    /// Build the one shared terminal layout used by PTY sizing, rendering and
    /// pointer hit-testing.
    pub(super) fn terminal_layout(&self) -> Option<TerminalLayout> {
        let (Some(window), Some(renderer)) = (&self.window, &self.renderer) else {
            return None;
        };
        let chrome_left = if self.panel.open {
            renderer.sidebar_push_width() as f64
        } else {
            0.0
        };
        let layout = terminal_layout_for_renderer(renderer, window.inner_size(), chrome_left);
        Some(layout)
    }

    /// Compute grid dimensions; returns `(0, 0)` until layout is ready.
    pub(super) fn grid_dims(&self) -> (usize, usize) {
        self.terminal_layout()
            .map(TerminalLayout::dimensions)
            .unwrap_or((0, 0))
    }

    pub(super) fn terminal_content_contains(&self, x: f64, y: f64) -> bool {
        self.terminal_layout()
            .is_some_and(|layout| layout.contains_content(x, y))
    }

    /// v1.3 Batch 6: Find the pane under the physical-pixel point `(x, y)`.
    /// Returns `None` if the point is outside the content area or no panes
    /// exist. Used by mouse hit-testing to route clicks to the correct pane.
    pub(super) fn pane_at_pixel(&self, x: f64, y: f64) -> Option<weft_core::pane_layout::PaneId> {
        let layout = self.terminal_layout()?;
        if !layout.contains_content(x, y) {
            return None;
        }
        let content_rect: weft_core::pane_layout::Rect = [
            layout.content.left as f32,
            layout.content.top as f32,
            layout.content.right as f32,
            layout.content.bottom as f32,
        ];
        self.sessions
            .active()
            .and_then(|tab| tab.pane_hit_test(x as f32, y as f32, content_rect))
    }

    /// v1.3.2: Hit-test for a draggable pane divider at the given pointer
    /// position. Returns the divider info (axis, coord, pane pair, bounds)
    /// if the pointer is within 4px of a divider, or `None`.
    pub(super) fn pane_divider_hit_test(
        &self,
        x: f32,
        y: f32,
    ) -> Option<crate::paint::pane_dividers::DraggableDivider> {
        let layout = self.terminal_layout()?;
        let content_rect: weft_core::pane_layout::Rect = [
            layout.content.left as f32,
            layout.content.top as f32,
            layout.content.right as f32,
            layout.content.bottom as f32,
        ];
        let pane_layouts = self.sessions.active()?.split_tree().layout(content_rect);
        crate::paint::pane_dividers::pane_divider_at(&pane_layouts, x, y, 4.0)
    }

    /// Recompute grid rows/cols from the current window + cell dimensions and
    /// resize the terminal / queue a PTY SIGWINCH. Used after a font or padding
    /// change (cell size or usable area changes) and on window resize.
    pub(super) fn recompute_layout(&mut self) {
        let Some(window) = &self.window else {
            return;
        };
        let size = window.inner_size();
        let Some(renderer) = &mut self.renderer else {
            return;
        };
        renderer.resize(window, size);
        let chrome_left = if self.panel.open {
            renderer.sidebar_push_width() as f64
        } else {
            0.0
        };
        let base_layout = terminal_layout_for_renderer(renderer, size, chrome_left);
        if base_layout.rows == 0 || base_layout.cols == 0 {
            return;
        }
        // v0.9 W5: resize every tab's terminal so non-active tabs also pick
        // up the new chrome_left (sidebar open/close shifts the grid). Only
        // the active tab sends a PTY resize immediately; background tabs get
        // their PTY resize on activation (refresh_grid_for_active_tab).
        //
        // v1.3 Batch 6: resize ALL panes per tab according to their split-tree
        // rects. For single-pane tabs this is equivalent to the old
        // `resize_terminal_and_queue` call (the split tree returns one rect
        // equal to the content area).
        let active = self.sessions.active_idx();
        let header_rows = renderer.block_header_rows();
        let layout_ctx = base_layout.layout_ctx();
        let content_rect: weft_core::pane_layout::Rect = [
            base_layout.content.left as f32,
            base_layout.content.top as f32,
            base_layout.content.right as f32,
            base_layout.content.bottom as f32,
        ];
        let cell_w = base_layout.cell_width as f32;
        let cell_h = base_layout.cell_height as f32;
        for (i, tab) in self.sessions.tabs_mut().iter_mut().enumerate() {
            if tab.has_terminal() {
                let resized = tab.resize_all_panes_for_rect(content_rect, cell_w, cell_h);
                if resized {
                    // v1.10.26 (FIX_SELECTION_CONTENT_ANCHORS): a terminal
                    // resize changes every line wrap and char offset — content
                    // anchors cannot survive it, so block-view selections are
                    // cleared (Warp clears on resize too). The grid selection
                    // model is untouched here (its viewport-relative drift
                    // guards live in the scroll path).
                    for pane in tab.panes_mut() {
                        if pane.selection_handler.block_view_selection.is_some() {
                            tracing::info!("terminal resized; cleared block selection");
                            // v1.10.26 (rust-reviewer S3): clear only the
                            // block-view half — the grid `Selection` and the
                            // in-progress `selecting` flag survive (they have
                            // their own viewport-relative resize semantics, and
                            // `clear()` would also drop them, contradicting the
                            // comment above).
                            pane.selection_handler.block_view_selection = None;
                            pane.selection_handler.block_doc_fingerprint = None;
                        }
                    }
                }
                if resized && i == active {
                    let active_id = tab.active_pane_id();
                    let layouts = tab.split_tree().layout(content_rect);
                    if let Some((_, rect)) = layouts.into_iter().find(|(id, _)| *id == active_id) {
                        let [x0, y0, x1, y1] = rect;
                        let w = (x1 - x0).max(0.0);
                        let h = (y1 - y0).max(0.0);
                        let cols = (w / cell_w).floor() as usize;
                        let rows = (h / cell_h).floor() as usize;
                        info!(active = ?active_id, rows, cols, pane_rect = ?rect, "terminal resized");
                    } else {
                        let (new_rows, new_cols) = base_layout.dimensions();
                        info!(rows = new_rows, cols = new_cols, "terminal resized");
                    }
                }
                let pane_layout_ctx = tab
                    .split_tree()
                    .layout(content_rect)
                    .into_iter()
                    .find_map(|(id, rect)| {
                        (id == tab.active_pane_id()).then_some(layout_ctx.for_pane(rect))
                    })
                    .unwrap_or(layout_ctx);
                let previous = tab.block_scroll();
                let reconciliation = tab
                    .with_terminal(|terminal| {
                        crate::block_component::reconciled_terminal_block_scroll(
                            terminal,
                            &pane_layout_ctx,
                            header_rows,
                            previous,
                        )
                    })
                    .flatten();
                if let Some((reconciled, total, visible)) = reconciliation {
                    if previous != reconciled {
                        info!(
                            tab = i,
                            previous,
                            reconciled,
                            total,
                            visible,
                            "reconciled block scroll during layout recompute"
                        );
                        tab.set_block_scroll(reconciled);
                    }
                }
            }
        }
        self.window_runtime.last_resize_instant = std::time::Instant::now();
        self.scroll_active_tab_into_view();
    }

    /// Convert pixel coordinates to grid (row, col).
    ///
    /// v1.3 Batch 6: for multi-pane tabs, the grid position is relative to
    /// the **active pane's** origin (not the full content area). Without this
    /// adjustment, clicking in a right-side pane would produce column indices
    /// offset by the left pane's width, clamping to the wrong cell. For
    /// single-pane tabs the pane origin equals the content origin — no change.
    pub(super) fn pixel_to_grid(&self, x: f64, y: f64) -> GridPos {
        let Some(layout) = self.terminal_layout() else {
            return GridPos::new(0, 0);
        };
        // Clamp to valid grid bounds. A click past the right/bottom edge (e.g.
        // a drag-to-select ending at the window margin) would otherwise yield
        // col == num_cols / row == num_rows and panic text_from_grid on copy.
        let (num_rows, num_cols) = self
            .sessions
            .active()
            .and_then(|tab| tab.with_terminal(|t| (t.grid().num_rows, t.grid().num_cols)))
            .unwrap_or((1, 1));

        // v1.3 Batch 6: adjust (x, y) by the active pane's origin offset.
        // `grid_position` subtracts `content.left/top` internally; we want it
        // to subtract the pane's `x0/y0` instead. The adjustment is:
        //   adjusted_x = x - (pane_x0 - content.left)
        // so that grid_position computes (adjusted_x - content.left) = (x - pane_x0).
        // v1.10.20 (改动 3): a primary-screen TUI grid view additionally
        // insets by the BlockView gutter (renderer `GridViewPolicy`
        // `inset_block_gutter` — same judgment, renderer.rs:762). The third
        // tuple element is that gutter delta: hit-testing must use the SAME
        // origin as the render or clicks land ~1.5 cols right of the visible
        // character. Alt-screen TUIs stay edge-to-edge (delta 0).
        let inset_block_gutter = self
            .sessions
            .active()
            .and_then(|tab| {
                tab.with_terminal(|t| {
                    !t.is_alt_screen_active() && t.primary_screen_owns_live_view()
                })
            })
            .unwrap_or(false);
        let (adj_x, adj_y, gutter_delta) = if layout.contains_content(x, y) {
            let content_rect: weft_core::pane_layout::Rect = [
                layout.content.left as f32,
                layout.content.top as f32,
                layout.content.right as f32,
                layout.content.bottom as f32,
            ];
            // v1.12.25 (audit 3-B, P1-01): no active tab — same degenerate
            // origin as "active pane not found" below.
            self.sessions
                .active()
                .and_then(|tab| {
                    tab.split_tree()
                        .layout(content_rect)
                        .into_iter()
                        .find(|(id, _)| *id == tab.active_pane_id())
                        .map(|(_, rect)| {
                            let [px0, py0, _, _] = rect;
                            let dx = px0 as f64 - layout.content.left;
                            let dy = py0 as f64 - layout.content.top;
                            let pane_ctx = layout.layout_ctx().for_pane(rect);
                            let origin_x = crate::terminal_geometry::grid_hit_origin_x(
                                &pane_ctx,
                                inset_block_gutter,
                            );
                            (x - dx, y - dy, origin_x - px0 as f64)
                        })
                })
                .unwrap_or((x, y, 0.0))
        } else {
            (x, y, 0.0)
        };

        let (row, col) = layout.grid_position(adj_x - gutter_delta, adj_y, num_rows, num_cols);
        GridPos::new(row, col)
    }

    /// Resolve the OSC 8 hyperlink URL at pixel coordinates `(x, y)`, if any.
    ///
    /// v1.6.1: Now resolves links via `Grid::hyperlink_id_at` (backed by
    /// `RowExtras`) instead of the viewport-relative `HyperlinkRegistry::url_at`.
    /// This means links in scrolled-off content remain clickable — the link id
    /// is stored in `RowExtras` and survives scroll/reflow/resize. The
    /// registry's `cell_map` is now just a fast-path index for the live
    /// viewport; `RowExtras` is the source of truth.
    ///
    /// In block view, dispatches to [`block_view_hyperlink_at_pixel`](Self::block_view_hyperlink_at_pixel)
    /// which resolves links from captured `StyledLine::links` spans.
    pub(super) fn hyperlink_at_pixel(&self, x: f64, y: f64) -> Option<String> {
        if self.block_view_active() {
            return self.block_view_hyperlink_at_pixel(x, y);
        }
        // T10 P1 (D9 rule 2): `terminal_content_contains` / `pixel_to_grid`
        // lock the active pane's terminal internally, so both run BEFORE the
        // guard below is taken (zero behavior change — both are pure reads).
        if !self.terminal_content_contains(x, y) {
            return None;
        }
        let pos = self.pixel_to_grid(x, y);
        let terminal = self.sessions.active().and_then(|tab| tab.lock_terminal())?;
        // v1.6.1: resolve via RowExtras (scroll-aware) → registry URL lookup.
        let id = terminal.grid().hyperlink_id_at(pos.row, pos.col)?;
        terminal.hyperlinks().url(id).map(str::to_string)
    }

    /// v1.6.1: Resolve an OSC 8 hyperlink URL at pixel coordinates `(x, y)`
    /// within the block view. Walks the cached block-view rows to find the
    /// clicked row, then looks up the `LinkSpan` in the owning block's
    /// `StyledLine`.
    ///
    /// Returns `None` when:
    /// - the click misses all rows (e.g. on the CWD bar or input box),
    /// - the row has no `line` index (Command/Header/Separator),
    /// - the block has no `styled_output` (e.g. loaded from an old DB),
    /// - the char at `char_index` has no link span.
    ///
    /// For wrapped lines (multiple chunks per source line), the chunk's
    /// `chunk_char_offset` is added to the click's `char_index` to compute
    /// the full-line char index that `StyledLine::link_at` expects.
    pub(super) fn block_view_hyperlink_at_pixel(&self, x: f64, y: f64) -> Option<String> {
        let (rows, _, _) = self.compute_block_view_rows()?;
        let row = rows.iter().find(|r| r.contains_y(y as f32))?;
        let line_idx = row.line?;
        let char_index = row.chunk_char_offset
            + weft_core::selection::pixel_x_to_char_index(
                &row.text,
                x,
                crate::layout::block_content_x_bounds(&self.renderer.as_ref()?.layout_ctx?).0
                    as f64,
                self.renderer.as_ref()?.cell_width() as f64,
                row.indent_cols,
            );
        let terminal = self.sessions.active().and_then(|tab| tab.lock_terminal())?;
        let tracker = terminal.block_tracker();
        // Resolve the StyledLine from either a finalized block or the live
        // in-flight block (block_id is None for live rows).
        let styled_line = if let Some(block_id) = row.block_id {
            // Finalized block: search session_blocks by id. Linear scan is
            // fine — click events are infrequent. session_blocks may hold
            // 1000+ blocks after Restore, but a per-click scan is still cheap.
            tracker
                .session_blocks()
                .iter()
                .find(|b| b.id == block_id)
                .and_then(|b| b.styled_output.as_deref())
                .and_then(|s| s.line(line_idx))
        } else {
            // Live in-flight block: styled_output is on InFlightBlock.
            tracker
                .in_flight()
                .and_then(|live| live.styled_output)
                .and_then(|s| s.line(line_idx))
        };
        styled_line.and_then(|line| line.link_at(char_index).map(str::to_string))
    }

    /// Convert pixel coordinates to a block-view CONTENT ANCHOR.
    ///
    /// Used in place of `pixel_to_grid` when `show_block_view()` is true: the
    /// classic grid division (`y / cell_h`) does not match the block view's
    /// `pitch = cell_h * 1.1` row spacing, inserted Header/Separator rows, the
    /// pinned CWD bar, or the scroll offset, so a grid-coordinate copy landed
    /// on the wrong line (the "复制错位" bug). This walks the renderer's cached
    /// `block_view_rows` (scroll-adjusted y bands + visible text) and maps the
    /// click to a content anchor: the hit row's `(block_id, line)` key plus a
    /// char offset into the source line, honoring CJK double-width and the
    /// chunk's `chunk_char_offset`.
    ///
    /// v1.10.26 (FIX_SELECTION_CONTENT_ANCHORS): the anchor replaces the old
    /// row-index `BlockViewPos`. Only rows with a real `(block, line)` key
    /// (Output rows) are addressable; structural rows (Command/Header/
    /// Separator/LiveCommand) carry no document line and are not anchor
    /// endpoints.
    ///
    /// Returns `None` if no row band contains `y` (e.g. on the CWD bar / input
    /// box / outside the scroll region) or the matched row isn't an Output row.
    pub(super) fn pixel_to_block_view_pos(&self, x: f64, y: f64) -> Option<BlockSelAnchor> {
        let renderer = self.renderer.as_ref()?;
        let cw = renderer.cell_width() as f64;
        if cw <= 0.0 {
            return None;
        }
        let ctx = renderer.layout_ctx?;
        let (rows, _, _) = self.compute_block_view_rows()?;
        if rows.is_empty() {
            return None;
        }
        // Find the row whose [y_top, y_bottom) contains y.
        let row_index = rows.iter().position(|r| r.contains_y(y as f32))?;
        let row = &rows[row_index];
        if row.kind != BlockViewRowKind::Output {
            return None;
        }
        let line = row.line?;
        // v1.10.13: command first lines render after the chevron + "> " indent;
        // subtract it so the char index matches the character under the cursor
        // (continuation lines and output rows are flush-left, indent 0).
        let (content_left, _) = crate::layout::block_content_x_bounds(&ctx);
        let char_index = weft_core::selection::pixel_x_to_char_index(
            &row.text,
            x,
            content_left as f64,
            cw,
            row.indent_cols,
        );
        Some(BlockSelAnchor {
            block: row.block_id.map(|b| b.0),
            line,
            char_offset: row.chunk_char_offset + char_index,
        })
    }

    /// v0.9: map a physical-pixel click (inside the prompt input box) to an
    /// editor buffer position `(line, char_col)`. Returns None when the
    /// renderer/prompt isn't available or the click is outside the box.
    ///
    pub(super) fn pixel_to_editor_pos(&self, x: f64, y: f64) -> Option<(usize, usize)> {
        use unicode_width::UnicodeWidthChar;
        let renderer = self.renderer.as_ref()?;
        let ctx = renderer.layout_ctx?;
        let cw = ctx.cell_w as f64;
        let ch = ctx.cell_h as f64;
        if cw <= 0.0 || ch <= 0.0 {
            return None;
        }
        let terminal = self.sessions.active().and_then(|tab| tab.lock_terminal())?;
        if terminal.effective_input_mode() != weft_core::input::InputMode::Editor {
            return None;
        }
        let lines = &terminal.editor().buffer.lines;
        if lines.is_empty() {
            return None;
        }
        let (visual, layout, scroll_offset) = crate::paint::prompt::prompt_layout_for_buffer(
            &ctx,
            lines,
            terminal.editor().buffer.cursor,
        );
        if x < layout.box_rect[0] as f64
            || x > layout.box_rect[2] as f64
            || y < layout.box_rect[1] as f64
            || y > layout.box_rect[3] as f64
        {
            return None;
        }
        let visual_row =
            crate::layout::prompt_line_at_y(&layout, y as f32, scroll_offset, visual.rows.len());
        let row = visual.rows.get(visual_row)?;
        // X origin for this line.
        let text_x = if visual_row == 0 {
            layout.first_line_text_x as f64
        } else {
            layout.left as f64
        };
        // Column offset in display units.
        let disp_col = ((x - text_x) / cw).max(0.0) as usize;
        // Walk the line's chars, accumulating display widths, to find the
        // char index whose cumulative width first exceeds disp_col.
        let line_str = &row.text;
        let mut col_cursor = 0usize;
        for (ci, c) in line_str.chars().enumerate() {
            let w = UnicodeWidthChar::width(c).unwrap_or(0);
            if w == 0 {
                continue;
            }
            if disp_col < col_cursor + w {
                // For double-width chars, clicking the right half advances
                // past the char (so drag-select lands after it).
                let char_idx = if disp_col > col_cursor { ci + 1 } else { ci };
                return Some((row.source_line, row.char_start + char_idx));
            }
            col_cursor += w;
        }
        Some((row.source_line, row.char_end))
    }

    /// Current prompt box, when the block editor is available.
    pub(super) fn prompt_box_rect(&self) -> Option<[f32; 4]> {
        let renderer = self.renderer.as_ref()?;
        let ctx = renderer.layout_ctx?;
        let terminal = self.sessions.active().and_then(|tab| tab.lock_terminal())?;
        if !terminal.show_block_view() {
            return None;
        }
        let (_, layout, _) = crate::paint::prompt::prompt_layout_for_buffer(
            &ctx,
            &terminal.editor().buffer.lines,
            terminal.editor().buffer.cursor,
        );
        Some(layout.box_rect)
    }

    /// Compute block-view rows on-demand for hit-testing, instead of reading
    /// the renderer's previous-frame `block_view_rows` cache. Returns
    /// `(rows, clip_top, clip_bottom)` — the clip values are the visible
    /// content band from the layout pass (rows carry overscan). `None` when
    /// not in block view / no renderer / no terminal.
    pub(super) fn compute_block_view_rows(
        &self,
    ) -> Option<(Vec<weft_core::selection::BlockViewRow>, f32, f32)> {
        let tab = self.sessions.active()?;
        let terminal = tab.lock_terminal()?;
        self.compute_block_view_rows_for(tab, &terminal)
    }

    /// Guard-carrying variant of [`Self::compute_block_view_rows`] (T10 P1):
    /// callers that already hold the active pane's terminal guard (e.g. the
    /// accessibility text walk) must use this — calling the locking wrapper
    /// inside a guard scope would self-deadlock (D9 rule 2).
    pub(super) fn compute_block_view_rows_for(
        &self,
        tab: &Tab,
        terminal: &Terminal,
    ) -> Option<(Vec<weft_core::selection::BlockViewRow>, f32, f32)> {
        let renderer = self.renderer.as_ref()?;
        if !terminal.show_block_view() {
            return None;
        }
        let editor_mode = terminal.effective_input_mode() == weft_core::input::InputMode::Editor;
        let region_bottom_y = if editor_mode {
            let ctx = renderer.layout_ctx?;
            let (_, prompt, _) = crate::paint::prompt::prompt_layout_for_buffer(
                &ctx,
                &terminal.editor().buffer.lines,
                terminal.editor().buffer.cursor,
            );
            prompt.box_rect[1]
        } else {
            let ctx = renderer.layout_ctx?;
            ctx.bottom()
        };
        Some(
            renderer.compute_block_view_rows(crate::paint::block_view_model::BlockViewPaintModel {
                blocks: terminal.block_tracker().session_blocks(),
                live_head_lines: terminal.screen_head_lines(),
                region_bottom_y,
                cwd: terminal.cwd(),
                git_branch: terminal.git_branch(),
                live: (!editor_mode)
                    .then(|| terminal.block_tracker().in_flight())
                    .flatten(),
                block_scroll: tab.block_scroll_position(),
                viewport_rows: terminal.grid().num_rows,
                block_hovered: self.interaction.block_hovered,
                block_selected: self.interaction.block_selected,
                block_action_hovered: self.interaction.block_action_hovered,
                spinner_phase: -1.0,
                find_block_highlight: renderer
                    .find_state
                    .as_ref()
                    .and_then(|find| find.block_highlight),
                palette: terminal.palette(),
                cache_namespace: tab.pane_session_id,
                block_diagnose_state: &self.block_diagnose_state,
                ai_configured: self.ai_state.is_configured(),
                tui_cursor: None,
                tui_preedit: None,
                cursor_blink_on: false,
                is_alt: terminal.is_alt_screen_active(),
                // P1: hit-test is its own event, not the paint frame — no
                // cross-event value sharing needed; only the label's shape
                // matters here.
                now: std::time::SystemTime::now(),
            }),
        )
    }

    /// True when the block view is the active renderer (Editor mode, not in
    /// an alt-screen app). Centralises the dispatch so mouse/copy paths stay
    /// consistent.
    pub(super) fn block_view_active(&self) -> bool {
        self.sessions
            .active()
            .and_then(|tab| tab.with_terminal(|t| t.show_block_view()))
            .unwrap_or(false)
    }

    /// True when the foreground program has grabbed the mouse (mouse reporting
    /// on) and the user is NOT holding Shift to force a selection. While true,
    /// clicks/drags are forwarded to the program and we must NOT start a visual
    /// selection (otherwise a stray blue cell follows the click — e.g. inside
    /// `claude`/`vim`). Standard xterm/Alacritty behavior.
    pub(super) fn mouse_reporting_active(&self) -> bool {
        if self.interaction.mods.state().shift_key() {
            return false; // Shift = force terminal selection
        }
        self.sessions
            .active()
            .and_then(|tab| tab.with_terminal(|t| t.mouse_protocol() != MouseProtocol::Off))
            .unwrap_or(false)
    }

    /// Which foldable block (if any) owns the physical-pixel y in the last
    /// rendered block view. `None` outside the block view or off every block.
    /// Find the block at vertical position `y`. Returns `Some(id)` for
    /// completed blocks, `Some(None)` for the in-flight (running) command,
    /// or `None` when not on a block row.
    pub(super) fn block_at(&self, y: f32) -> Option<Option<BlockId>> {
        let (rows, _, _) = self.compute_block_view_rows()?;
        // Find the row whose y-range contains `y`. Header/Command/Output rows
        // retain their owning finalized block; LiveCommand is in-flight.
        for row in &rows {
            if y >= row.y_top && y < row.y_bottom {
                use weft_core::selection::BlockViewRowKind;
                match &row.kind {
                    BlockViewRowKind::LiveCommand => return Some(None),
                    _ => {
                        return crate::block_component::hovered_block_for_row(
                            &row.kind,
                            row.block_id,
                        )
                        .map(Some)
                    }
                }
            }
        }
        None
    }
}
