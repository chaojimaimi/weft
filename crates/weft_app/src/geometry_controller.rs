//! Shared layout and hit-testing controller.

use super::*;

impl App {
    pub(super) fn active_scrollbar_layout(
        &self,
    ) -> Option<crate::scrollbar_component::ScrollbarLayout> {
        let renderer = self.renderer.as_ref()?;
        let ctx = renderer.layout_ctx?;
        let terminal = self.sessions.active().terminal.as_ref()?;
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
            self.sessions.active().block_scroll(),
        )
    }
    pub(super) fn palette_scene(
        &self,
    ) -> Option<crate::scene::Scene<crate::palette_component::PaletteTarget>> {
        if !self.palette.open {
            return None;
        }
        let ctx = self.renderer.as_ref()?.layout_ctx?;
        let labels: Vec<String> = match &self.palette.submode {
            PaletteSubMode::SelectTheme { buffer, themes } => {
                let query = buffer.to_lowercase();
                themes
                    .iter()
                    .filter(|name| query.is_empty() || name.to_lowercase().contains(&query))
                    .cloned()
                    .collect()
            }
            _ => self
                .palette
                .results
                .iter()
                .map(|entry| match entry {
                    PaletteEntry::Workflow(workflow) => workflow.name.clone(),
                    PaletteEntry::Builtin(command) => command.label().to_string(),
                    // v1.5.1: Profile entries render as "Switch Profile: <name>".
                    PaletteEntry::Profile { name, .. } => {
                        format!("Switch Profile: {name}")
                    }
                    // v1.7.1: Search hits use the document title as label.
                    PaletteEntry::SearchHit(hit) => hit.doc.title.clone(),
                    PaletteEntry::Runbook(entry) => entry.command.clone(),
                    // v1.8.1: AI suggestions use the generated command as label.
                    PaletteEntry::AiSuggestion { command, .. } => command.clone(),
                })
                .collect(),
        };
        let layout = crate::palette_component::derive_palette_layout(
            &ctx,
            labels.len(),
            self.palette.selection,
            self.palette.form.as_ref().map(|form| form.var_names.len()),
            self.interaction.popup_max_rows,
            self.interaction.popup_width_scale,
        );
        Some(crate::palette_component::build_palette_scene(
            layout, &labels, ctx.cell_h,
        ))
    }

    pub(super) fn completion_scene(
        &self,
    ) -> Option<crate::scene::Scene<crate::completion_component::CompletionTarget>> {
        let renderer = self.renderer.as_ref()?;
        let ctx = renderer.layout_ctx?;
        let terminal = self.sessions.active().terminal.as_ref()?;
        if terminal.effective_input_mode() != weft_core::input::InputMode::Editor
            || terminal.editor().search_view().is_some()
        {
            return None;
        }
        let editor = terminal.editor();
        let (matches, selected) = editor.completion_view()?;
        let (visual, _, _) = crate::paint::prompt::prompt_layout_for_buffer(
            &ctx,
            &editor.buffer.lines,
            editor.buffer.cursor,
        );
        let layout = crate::completion_component::derive_completion_layout(
            &ctx,
            matches,
            selected,
            visual.rows.len(),
            (visual.cursor_row, visual.cursor_display_col),
            self.interaction.popup_max_rows,
            self.interaction.popup_width_scale,
        )?;
        Some(crate::completion_component::build_completion_scene(
            layout, matches, ctx.cell_h,
        ))
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
            .pane_hit_test(x as f32, y as f32, content_rect)
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
        let pane_layouts = self.sessions.active().split_tree().layout(content_rect);
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
            if tab.terminal.is_some() {
                let resized = tab.resize_all_panes_for_rect(content_rect, cell_w, cell_h);
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
                let reconciliation = tab.terminal.as_ref().and_then(|terminal| {
                    crate::block_component::reconciled_terminal_block_scroll(
                        terminal,
                        &pane_layout_ctx,
                        header_rows,
                        previous,
                    )
                });
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
            .terminal
            .as_ref()
            .map(|t| (t.grid().num_rows, t.grid().num_cols))
            .unwrap_or((1, 1));

        // v1.3 Batch 6: adjust (x, y) by the active pane's origin offset.
        // `grid_position` subtracts `content.left/top` internally; we want it
        // to subtract the pane's `x0/y0` instead. The adjustment is:
        //   adjusted_x = x - (pane_x0 - content.left)
        // so that grid_position computes (adjusted_x - content.left) = (x - pane_x0).
        let (adj_x, adj_y) = if layout.contains_content(x, y) {
            let content_rect: weft_core::pane_layout::Rect = [
                layout.content.left as f32,
                layout.content.top as f32,
                layout.content.right as f32,
                layout.content.bottom as f32,
            ];
            let active_id = self.sessions.active().active_pane_id();
            self.sessions
                .active()
                .split_tree()
                .layout(content_rect)
                .into_iter()
                .find(|(id, _)| *id == active_id)
                .map(|(_, rect)| {
                    let [px0, py0, _, _] = rect;
                    let dx = px0 as f64 - layout.content.left;
                    let dy = py0 as f64 - layout.content.top;
                    (x - dx, y - dy)
                })
                .unwrap_or((x, y))
        } else {
            (x, y)
        };

        let (row, col) = layout.grid_position(adj_x, adj_y, num_rows, num_cols);
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
        let terminal = self.sessions.active().terminal.as_ref()?;
        if !self.terminal_content_contains(x, y) {
            return None;
        }
        let pos = self.pixel_to_grid(x, y);
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
        let pos = self.pixel_to_block_view_pos(x, y)?;
        let rows = self.compute_block_view_rows();
        let row = rows.get(pos.row_index)?;
        let line_idx = row.line?;
        let char_index = row.chunk_char_offset + pos.char_index;
        let terminal = self.sessions.active().terminal.as_ref()?;
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

    /// Convert pixel coordinates to a block-view position.
    ///
    /// Used in place of `pixel_to_grid` when `show_block_view()` is true: the
    /// classic grid division (`y / cell_h`) does not match the block view's
    /// `pitch = cell_h * 1.1` row spacing, inserted Header/Separator rows, the
    /// pinned CWD bar, or the scroll offset, so a grid-coordinate copy landed
    /// on the wrong line (the "复制错位" bug). This walks the renderer's cached
    /// `block_view_rows` (scroll-adjusted y bands + visible text) and maps the
    /// click to a char index in the matched row, honoring CJK double-width.
    ///
    /// Returns `None` if no row band contains `y` (e.g. on the CWD bar / input
    /// box / outside the scroll region) or the matched row isn't selectable.
    pub(super) fn pixel_to_block_view_pos(&self, x: f64, y: f64) -> Option<BlockViewPos> {
        let renderer = self.renderer.as_ref()?;
        let cw = renderer.cell_width() as f64;
        if cw <= 0.0 {
            return None;
        }
        let ctx = renderer.layout_ctx?;
        let rows = self.compute_block_view_rows();
        if rows.is_empty() {
            return None;
        }
        // Find the row whose [y_top, y_bottom) contains y.
        let row_index = rows.iter().position(|r| r.contains_y(y as f32))?;
        let row = &rows[row_index];
        if !matches!(
            row.kind,
            BlockViewRowKind::Output | BlockViewRowKind::Command | BlockViewRowKind::LiveCommand
        ) {
            return None;
        }
        // Map pixel x → char index by accumulating each char's display width.
        // A click in the right half of a double-width cell rounds to that
        // cell's index (so dragging across it selects the whole CJK char).
        let mut col_cursor = 0usize; // column units consumed so far
        let (content_left, _) = crate::layout::block_content_x_bounds(&ctx);
        let target_col = ((x as f32 - content_left) / ctx.cell_w).max(0.0) as usize;
        for (ci, c) in row.text.chars().enumerate() {
            let w = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
            if w == 0 {
                // Zero-width (combining mark): belongs to the previous cell,
                // don't advance the column cursor.
                continue;
            }
            // Click lands in this char if it's before the far edge of the cell.
            // For double-width, the char occupies [col_cursor, col_cursor+2);
            // a click anywhere in that range maps to this char.
            if target_col < col_cursor + w {
                return Some(BlockViewPos {
                    row_index,
                    char_index: ci,
                });
            }
            col_cursor += w;
        }
        // Past the last char: clamp to end.
        Some(BlockViewPos {
            row_index,
            char_index: row.text.chars().count(),
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
        let Some(terminal) = &self.sessions.active().terminal else {
            return None;
        };
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
        let terminal = self.sessions.active().terminal.as_ref()?;
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
    /// the renderer's previous-frame `block_view_rows` cache. Returns empty
    /// when not in block view.
    pub(super) fn compute_block_view_rows(&self) -> Vec<weft_core::selection::BlockViewRow> {
        let renderer = match self.renderer.as_ref() {
            Some(r) => r,
            None => return Vec::new(),
        };
        let terminal = match self.sessions.active().terminal.as_ref() {
            Some(t) => t,
            None => return Vec::new(),
        };
        if !terminal.show_block_view() {
            return Vec::new();
        }
        let editor_mode = terminal.effective_input_mode() == weft_core::input::InputMode::Editor;
        let region_bottom_y = if editor_mode {
            let Some(ctx) = renderer.layout_ctx else {
                return Vec::new();
            };
            let (_, prompt, _) = crate::paint::prompt::prompt_layout_for_buffer(
                &ctx,
                &terminal.editor().buffer.lines,
                terminal.editor().buffer.cursor,
            );
            prompt.box_rect[1]
        } else {
            let Some(ctx) = renderer.layout_ctx else {
                return Vec::new();
            };
            ctx.bottom()
        };
        renderer.compute_block_view_rows(crate::paint::block_view_model::BlockViewPaintModel {
            blocks: terminal.block_tracker().session_blocks(),
            region_bottom_y,
            cwd: terminal.cwd(),
            git_branch: terminal.git_branch(),
            live: (!editor_mode)
                .then(|| terminal.block_tracker().in_flight())
                .flatten(),
            block_scroll: self.sessions.active().block_scroll_position(),
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
            cache_namespace: self.sessions.active().pane_session_id,
        })
    }

    /// True when the block view is the active renderer (Editor mode, not in
    /// an alt-screen app). Centralises the dispatch so mouse/copy paths stay
    /// consistent.
    pub(super) fn block_view_active(&self) -> bool {
        self.sessions
            .active()
            .terminal
            .as_ref()
            .map(|t| t.show_block_view())
            .unwrap_or(false)
    }

    /// F3-3: Check whether a physical-pixel point `(x, y)` lands on the
    /// sidebar's right-edge resize handle. Returns `true` only when:
    ///   - the history panel is open,
    ///   - the window is NOT Compact (sidebar is push mode, not overlay),
    ///   - the point is within `tolerance` px of the sidebar's right edge,
    ///   - the point is within the viewport's vertical extent.
    ///
    /// `tolerance` is in physical pixels (≈4 px each side of the edge). The
    /// pure geometry lives in `ui_tokens::sidebar_edge_hit` (unit-tested
    /// independently of the renderer/App state).
    pub(super) fn sidebar_resize_hit(&self, x: f32, y: f32, tolerance: f32) -> bool {
        if !self.panel.open {
            return false;
        }
        let Some(renderer) = &self.renderer else {
            return false;
        };
        // Compact windows don't support sidebar resize (overlay drawer).
        if renderer.sidebar_push_width() == 0.0 {
            return false;
        }
        let edge = renderer.sidebar_width();
        let vp_h = renderer.viewport().1;
        crate::ui_tokens::sidebar_edge_hit(x, edge, tolerance, vp_h, y)
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
            .terminal
            .as_ref()
            .map(|t| t.mouse_protocol() != MouseProtocol::Off)
            .unwrap_or(false)
    }

    /// Which foldable block (if any) owns the physical-pixel y in the last
    /// rendered block view. `None` outside the block view or off every block.
    /// Find the block at vertical position `y`. Returns `Some(id)` for
    /// completed blocks, `Some(None)` for the in-flight (running) command,
    /// or `None` when not on a block row.
    pub(super) fn block_at(&self, y: f32) -> Option<Option<BlockId>> {
        let rows = self.compute_block_view_rows();
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

    /// Hit-test the find popup's clickable buttons. Returns the action the
    /// click should trigger, or `None` when the click landed outside any
    /// button (or the find popup isn't open). Layout, hit testing and semantic
    /// bounds are produced by the same Find Scene component.
    pub(super) fn find_button_at(&self, x: f32, y: f32) -> Option<FindButtonAction> {
        let renderer = self.renderer.as_ref()?;
        let ctx = renderer.layout_ctx?;
        let total = if self.block_view_active() {
            self.find.block_matches.len()
        } else {
            self.find.matches.len()
        };
        let scene =
            crate::find_component::build_find_scene(crate::layout::layout_find(&ctx, total));
        crate::find_component::find_target_at(&scene, x, y).map(|target| match target {
            crate::find_component::FindTarget::Previous => FindButtonAction::Prev,
            crate::find_component::FindTarget::Next => FindButtonAction::Next,
            crate::find_component::FindTarget::ToggleCase => FindButtonAction::ToggleCase,
            crate::find_component::FindTarget::ToggleRegex => FindButtonAction::ToggleRegex,
        })
    }

    /// v0.9: map a physical-pixel click to a palette results-list row
    /// index. Returns `Some(idx)` when the click lands inside a visible
    /// results row, `None` otherwise (outside the popup, on the query/banner
    /// row, in workflow form mode, or below the last visible row). Border-drag
    /// clicks are handled earlier by `check_popup_border_drag`, so they never
    /// reach here.
    ///
    /// Geometry and hit targets come from the shared Palette Scene.
    pub(super) fn palette_row_at(&self, x: f64, y: f64) -> Option<usize> {
        let scene = self.palette_scene()?;
        match crate::palette_component::palette_target_at(&scene, x as f32, y as f32) {
            Some(crate::palette_component::PaletteTarget::Item(index)) => Some(index),
            _ => None,
        }
    }

    /// Resolve a physical-pixel point in the tab bar to the topmost target.
    /// Rebuilds the tab-strip layout from the current renderer geometry +
    /// `TabBarState.scroll_offset`, then queries the shared TabBar Scene.
    /// Returns `None` when the renderer is absent or the point is outside the
    /// bar.
    pub(super) fn tab_bar_target_at(
        &self,
        x: f32,
        y: f32,
    ) -> Option<crate::tab_bar_component::TabBarTarget> {
        let renderer = self.renderer.as_ref()?;
        if y > renderer.tab_bar_height() {
            return None;
        }
        let chrome_left = self.tab_bar_chrome_left();
        let strip = crate::layout::layout_tab_strip(crate::layout::TabStripInput {
            viewport_width: self.tab_bar_layout_right(),
            bar_height: renderer.tab_bar_height(),
            cell_width: renderer.cell_width() as f32,
            padding_x: renderer.padding_x(),
            chrome_left,
            traffic_lights_width: renderer.traffic_lights_width(),
            tab_count: self.sessions.len(),
            requested_scroll_offset: self.tab_bar.scroll_offset,
        });
        let scene = crate::tab_bar_component::build_tab_bar_scene(
            strip,
            self.sessions.len(),
            renderer.cell_width() as f32,
            renderer.cell_height() as f32,
        );
        crate::tab_bar_component::tab_bar_target_at(&scene, x, y)
    }

    /// Resolve a physical-pixel point in the history panel to a target.
    /// Rebuilds the panel layout from renderer geometry (single source of
    /// truth — no duplicated magic numbers). Returns `None` when the panel
    /// is closed, the renderer is absent, or the point misses every target.
    pub(super) fn panel_target_at(
        &self,
        x: f32,
        y: f32,
    ) -> Option<crate::panel_component::PanelTarget> {
        if !self.panel.open {
            return None;
        }
        let renderer = self.renderer.as_ref()?;
        let chrome_top = renderer.layout_ctx.map(|c| c.chrome_top).unwrap_or(0.0);
        let layout = crate::layout::layout_panel(
            chrome_top,
            renderer.cell_width() as f32,
            renderer.cell_height() as f32,
            renderer.sidebar_width(),
            renderer.viewport().1,
        );
        let max_rows = crate::paint::ui_helpers::visible_panel_rows(
            renderer.viewport().1,
            renderer.cell_height(),
        );
        let scene = crate::panel_component::build_panel_scene(
            layout.panel_rect,
            layout.search_field_rect,
            layout.list_top,
            layout.row_height,
            max_rows,
        );
        crate::panel_component::panel_target_at(&scene, x, y)
    }

    pub(super) fn active_panel_scrollbar_layout(
        &self,
    ) -> Option<crate::panel_scrollbar::PanelScrollbarLayout> {
        if !self.panel.open {
            return None;
        }
        let renderer = self.renderer.as_ref()?;
        // Batch 5 Step 2: read cached metrics from the last draw() instead of
        // re-running panel_filtered_count (O(n) over all blocks) on every
        // mouse move. The cache is written at the end of build_panel_vertices;
        // mouse events read the previous frame's metrics (1-frame lag is
        // imperceptible for scrollbar hit-testing).
        let (total, visible, _max_scroll) = renderer.cached_panel_scroll_metrics.get()?;
        let chrome_top = renderer.layout_ctx.map(|ctx| ctx.chrome_top).unwrap_or(0.0);
        let layout = crate::layout::layout_panel(
            chrome_top,
            renderer.cell_width() as f32,
            renderer.cell_height() as f32,
            renderer.sidebar_width(),
            renderer.viewport().1,
        );
        crate::panel_scrollbar::panel_scrollbar_layout(
            layout.panel_rect,
            layout.list_top,
            total,
            visible,
            self.panel.scroll_offset,
            renderer.cell_height() as f32 * 0.8,
        )
    }
}
