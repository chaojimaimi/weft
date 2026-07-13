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
        let cols = terminal.grid().num_cols;
        let (total, _) = block_content_metrics(terminal, cols);
        let visible = renderer.block_visible_rows(terminal.editor().buffer.lines.len());
        let max_scroll = total.saturating_sub(visible);
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
        let layout = crate::completion_component::derive_completion_layout(
            &ctx,
            matches,
            selected,
            editor.buffer.lines.len(),
            editor.buffer.cursor,
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
        Some(terminal_layout_for_renderer(
            renderer,
            window.inner_size(),
            chrome_left,
        ))
    }

    /// Compute grid (rows, cols) from the shared terminal layout. Returns
    /// (0, 0) until the window/renderer are ready.
    pub(super) fn grid_dims(&self) -> (usize, usize) {
        self.terminal_layout()
            .map(TerminalLayout::dimensions)
            .unwrap_or((0, 0))
    }

    /// Recompute grid rows/cols from the current window + cell dimensions and
    /// resize the terminal / queue a PTY SIGWINCH. Used after a font or padding
    /// change (cell size or usable area changes) and on window resize.
    pub(super) fn recompute_layout(&mut self) {
        let (new_rows, new_cols) = self.grid_dims();
        if new_cols == 0 || new_rows == 0 {
            return;
        }
        let Some(window) = &self.window else {
            return;
        };
        let size = window.inner_size();
        if let Some(renderer) = &mut self.renderer {
            renderer.resize(window, size);
        }
        // v0.9 W5: resize every tab's terminal so non-active tabs also pick
        // up the new chrome_left (sidebar open/close shifts the grid). Only
        // the active tab sends a PTY resize immediately; background tabs get
        // their PTY resize on activation (refresh_grid_for_active_tab).
        let active = self.sessions.active_idx();
        for (i, tab) in self.sessions.tabs_mut().iter_mut().enumerate() {
            if let Some(terminal) = &mut tab.terminal {
                terminal.resize(new_rows, new_cols);
                if i == active {
                    info!(rows = new_rows, cols = new_cols, "terminal resized");
                }
            }
            if i == active {
                tab.pending_pty_resize = Some((new_rows, new_cols));
            }
        }
        self.window_runtime.last_resize_instant = std::time::Instant::now();
    }

    /// Convert pixel coordinates to grid (row, col).
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
        let (row, col) = layout.grid_position(x, y, num_rows, num_cols);
        GridPos::new(row, col)
    }

    /// Resolve the OSC 8 hyperlink URL at pixel coordinates `(x, y)`, if any.
    /// Returns `None` when the click misses a HYPERLINK-tagged cell or when
    /// the cell_map has been invalidated by a scroll (MVP trade-off: links
    /// in scrolled-off content aren't clickable).
    pub(super) fn hyperlink_at_pixel(&self, x: f64, y: f64) -> Option<String> {
        let terminal = self.sessions.active().terminal.as_ref()?;
        // Block view uses a separate scrollable layout — skip OSC 8 there.
        if self.block_view_active() {
            return None;
        }
        let pos = self.pixel_to_grid(x, y);
        terminal
            .hyperlinks()
            .url_at(pos.row, pos.col)
            .map(str::to_string)
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
        // v0.9 W5: account for the left sidebar offset (chrome_left) so
        // block-view clicks map to the correct char when the panel is open.
        let chrome_left = if self.panel.open {
            renderer.sidebar_push_width() as f64
        } else {
            0.0
        };
        let left = renderer.padding_x() as f64 + chrome_left;
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
        let target_col = ((x - left) / cw).max(0.0) as usize;
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
    /// The prompt box layout (from `layout_prompt`):
    ///   - line 0 starts at `first_line_text_x` (after the "❯ " glyph)
    ///   - lines 1+ start at `left` (= `box_x0`)
    ///   - each line is `cell_h` tall, starting at `text_y0`
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
        // Recompute the prompt geometry (matches layout_prompt).
        let n_lines = lines.len().max(1);
        let box_h = ch * (n_lines as f64 + 2.0);
        let box_y1 = (ctx.viewport.1 as f64 - ctx.padding_y as f64).max(0.0);
        let box_y0 = (box_y1 - box_h).max(0.0);
        let text_y0 = box_y0 + ch;
        let left = ctx.left() as f64;
        let prompt_chars = 2usize;
        let first_line_text_x = left + prompt_chars as f64 * cw;
        // Which line was clicked? (clamped to [0, n_lines-1])
        let mut line = ((y - text_y0) / ch) as isize;
        if line < 0 {
            line = 0;
        }
        let line = (line as usize).min(n_lines - 1);
        // X origin for this line.
        let text_x = if line == 0 { first_line_text_x } else { left };
        // Column offset in display units.
        let disp_col = ((x - text_x) / cw).max(0.0) as usize;
        // Walk the line's chars, accumulating display widths, to find the
        // char index whose cumulative width first exceeds disp_col.
        let line_str = &lines[line];
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
                return Some((line, char_idx));
            }
            col_cursor += w;
        }
        // Past the last char: clamp to end of line.
        Some((line, line_str.chars().count()))
    }

    /// Compute the prompt input box rect on-demand from the layout function,
    /// instead of reading the renderer's previous-frame cache. Returns None
    /// when not in block view or the terminal/editor isn't available.
    pub(super) fn prompt_box_rect(&self) -> Option<[f32; 4]> {
        let renderer = self.renderer.as_ref()?;
        let ctx = renderer.layout_ctx?;
        let terminal = self.sessions.active().terminal.as_ref()?;
        if !terminal.show_block_view() {
            return None;
        }
        let n_lines = terminal.editor().buffer.lines.len();
        // box_rect only depends on ctx + n_lines; cursor position doesn't affect the box bounds.
        let layout = crate::layout::layout_prompt(&ctx, n_lines, 0, 0);
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
        let region_bottom_y = renderer.viewport.1 - renderer.padding_y();
        renderer.compute_block_view_rows(crate::paint::block_view_model::BlockViewPaintModel {
            blocks: terminal.block_tracker().session_blocks(),
            region_bottom_y,
            cwd: None,
            git_branch: terminal.git_branch(),
            live: terminal.block_tracker().in_flight(),
            block_scroll: self.sessions.active().block_scroll(),
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
            .map(|t| t.mouse_protocol != MouseProtocol::Off)
            .unwrap_or(false)
    }

    /// Which foldable block (if any) owns the physical-pixel y in the last
    /// rendered block view. `None` outside the block view or off every block.
    /// Find the block at vertical position `y`. Returns `Some(id)` for
    /// completed blocks, `Some(None)` for the in-flight (running) command,
    /// or `None` when not on a block row.
    pub(super) fn block_at(&self, y: f32) -> Option<Option<BlockId>> {
        let rows = self.compute_block_view_rows();
        // Find the row whose y-range contains `y`. Prefer Command/LiveCommand
        // rows; Output rows fall back to their owning block.
        for row in &rows {
            if y >= row.y_top && y < row.y_bottom {
                use weft_core::selection::BlockViewRowKind;
                match row.kind {
                    BlockViewRowKind::Command => return Some(row.block_id),
                    BlockViewRowKind::LiveCommand => return Some(None),
                    BlockViewRowKind::Output => return Some(row.block_id),
                    _ => {}
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
        let chrome_left = renderer.layout_ctx.map(|c| c.chrome_left).unwrap_or(0.0);
        let strip = crate::layout::layout_tab_strip(crate::layout::TabStripInput {
            viewport_width: renderer.viewport().0,
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
        let max_rows =
            crate::renderer::visible_panel_rows(renderer.viewport().1, renderer.cell_height());
        let scene = crate::panel_component::build_panel_scene(
            layout.panel_rect,
            layout.search_field_rect,
            layout.list_top,
            layout.row_height,
            max_rows,
        );
        crate::panel_component::panel_target_at(&scene, x, y)
    }

    /// Resolve a physical-pixel point in the settings panel to a target.
    /// Rebuilds the settings layout from renderer geometry + current settings
    /// state. Returns `None` when the panel is closed, the renderer is absent,
    /// or the point misses every target.
    pub(super) fn settings_target_at(
        &self,
        x: f32,
        y: f32,
    ) -> Option<crate::settings_component::SettingsTarget> {
        if !self.settings.open {
            return None;
        }
        let renderer = self.renderer.as_ref()?;
        let cw = renderer.cell_width() as f32;
        let ch = renderer.cell_height() as f32;
        let (vp_w, vp_h) = renderer.viewport();

        // Footer pair widths: key_w + inner + desc_w for each of the 6 pairs.
        let pairs: [(&str, &str); 6] = [
            ("↑↓", "navigate"),
            ("⏎", "apply"),
            ("⇥", "switch"),
            ("←→", "adjust"),
            ("esc", "close"),
            ("⌘⏎", "save"),
        ];
        let inner = cw * 0.3;
        let mut footer_pair_widths = [0.0f32; 6];
        for (i, (key, desc)) in pairs.iter().enumerate() {
            let key_w = cw * crate::renderer::MetalRenderer::text_col_width(key) as f32;
            let desc_w = cw * crate::renderer::MetalRenderer::text_col_width(desc) as f32;
            footer_pair_widths[i] = key_w + inner + desc_w;
        }

        let layout = crate::layout::layout_settings(
            vp_w,
            vp_h,
            cw,
            ch,
            crate::overlay::SettingsTab::ALL.len(),
            self.settings.error.is_some(),
            &footer_pair_widths,
        )?;

        let theme_count = if self.settings.tab == crate::overlay::SettingsTab::Appearance {
            self.settings_theme_views().len().min(layout.max_rows)
        } else {
            0
        };

        let scene = crate::settings_component::build_settings_scene(
            &layout,
            crate::overlay::SettingsTab::ALL.as_slice(),
            theme_count,
            ch,
        );
        crate::settings_component::settings_target_at(&scene, x, y)
    }

    /// Check whether a point falls inside the settings panel bounding box
    /// (without requiring a hit target). Used to consume clicks that land on
    /// the panel background but miss every interactive element.
    pub(super) fn point_inside_settings_box(&self, x: f32, y: f32) -> bool {
        if !self.settings.open {
            return false;
        }
        let Some(renderer) = &self.renderer else {
            return false;
        };
        let cw = renderer.cell_width() as f32;
        let ch = renderer.cell_height() as f32;
        let (vp_w, vp_h) = renderer.viewport();
        crate::layout::layout_settings(
            vp_w,
            vp_h,
            cw,
            ch,
            crate::overlay::SettingsTab::ALL.len(),
            self.settings.error.is_some(),
            &[0.0; 6],
        )
        .map(|layout| {
            let [bx0, by0, bx1, by1] = layout.box_rect;
            x >= bx0 && x < bx1 && y >= by0 && y < by1
        })
        .unwrap_or(false)
    }
}
