//! Mouse/keyboard hit-testing and pixel→content position mapping,
//! extracted from `geometry_controller.rs` (v1.20.3 WP1 1B — zero-behavior
//! line-budget extraction; same extraction shape as the T10 P1a
//! `targets.rs` split). Visibility was `pub(super)` at the crate-root
//! module (= crate-visible); the same surface is spelled `pub(crate)`
//! here. Functions on per-mouse-move / drag / autoscroll paths carry
//! `#[inline]` (v1.17 WP2 precedent: cross-file moves of hot-path fns keep
//! the inlining candidate explicit).

use crate::tab::Tab;
use crate::App;
use weft_core::blocks::BlockId;
use weft_core::input::MouseProtocol;
use weft_core::selection::{BlockSelAnchor, BlockViewRowKind};
use weft_core::vt::Terminal;

impl App {
    /// v1.3 Batch 6: Find the pane under the physical-pixel point `(x, y)`.
    /// Returns `None` if the point is outside the content area or no panes
    /// exist. Used by mouse hit-testing to route clicks to the correct pane.
    pub(crate) fn pane_at_pixel(&self, x: f64, y: f64) -> Option<weft_core::pane_layout::PaneId> {
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
    #[inline]
    pub(crate) fn pane_divider_hit_test(
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
    pub(crate) fn hyperlink_at_pixel(&self, x: f64, y: f64) -> Option<String> {
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
    pub(crate) fn block_view_hyperlink_at_pixel(&self, x: f64, y: f64) -> Option<String> {
        let (rows, _, _, _) = self.compute_block_view_rows()?;
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
        // v1.19.2 (PLAN_v1192 §3 WP3-B): on the region track (flag ON + a
        // `133;B` begin anchor — exactly when `compute_block_view_rows` above
        // synced this pane's RegionLiveCache snapshot), a live row's `line`
        // is the P1-6 display-domain logical row, NOT a capture-stream index,
        // so `in_flight().styled_output.line()` would hit the wrong line.
        // Resolve through the synced snapshot instead; flag OFF / no snapshot
        // (capture track) keeps the capture-domain path below untouched.
        // Borrow order is the pinned contract: the region sync already ran
        // inside `compute_block_view_rows` (block_view/rows.rs) and its Ref
        // was dropped on return, so this `state()` borrow cannot race a sync.
        // Invariant: begin anchor and in_flight share a lifetime (set together
        // on 133;A, cleared together on finalize), so a live row here always
        // resolves through the cached state; if that coupling ever decouples,
        // gate this branch on the region source too.
        let region_state = if row.block_id.is_none() && terminal.is_live_block_grid_render() {
            self.renderer.as_ref().and_then(|renderer| {
                self.sessions
                    .active()
                    .and_then(|tab| renderer.region_live_cache.state(tab.pane_session_id))
            })
        } else {
            None
        };
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
        } else if let Some(state) = region_state.as_ref() {
            // Region-track live row: display row k ↔ slot `first + k`
            // (region_rows.rs stamps `line: k` over the P1-6 display range,
            // which drops leading/trailing blank slots).
            let slot_at = |i: usize| {
                if i < state.slots.len() {
                    state.slots.get(i)
                } else {
                    state.dyn_slots.get(i - state.slots.len())
                }
            };
            let total = state.slots.len() + state.dyn_slots.len();
            let (first, last) = crate::paint::region_cache::region_display_range(total, |i| {
                slot_at(i).map_or(true, |slot| slot.text.is_empty())
            });
            let slot_idx = first + line_idx;
            (slot_idx < last)
                .then(|| slot_at(slot_idx))
                .flatten()
                .map(|slot| &slot.style)
        } else {
            // Live in-flight block (capture track): styled_output is on
            // InFlightBlock.
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
    #[inline]
    pub(crate) fn pixel_to_block_view_pos(&self, x: f64, y: f64) -> Option<BlockSelAnchor> {
        let renderer = self.renderer.as_ref()?;
        let cw = renderer.cell_width() as f64;
        if cw <= 0.0 {
            return None;
        }
        let ctx = renderer.layout_ctx?;
        let (rows, _, _, _) = self.compute_block_view_rows()?;
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
    #[inline]
    pub(crate) fn pixel_to_editor_pos(&self, x: f64, y: f64) -> Option<(usize, usize)> {
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
    pub(crate) fn prompt_box_rect(&self) -> Option<[f32; 4]> {
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
    #[inline]
    pub(crate) fn compute_block_view_rows(
        &self,
    ) -> Option<crate::paint::block_view::BlockViewHitRows> {
        let tab = self.sessions.active()?;
        let terminal = tab.lock_terminal()?;
        self.compute_block_view_rows_for(tab, &terminal)
    }

    /// Guard-carrying variant of [`Self::compute_block_view_rows`] (T10 P1):
    /// callers that already hold the active pane's terminal guard (e.g. the
    /// accessibility text walk) must use this — calling the locking wrapper
    /// inside a guard scope would self-deadlock (D9 rule 2).
    pub(crate) fn compute_block_view_rows_for(
        &self,
        tab: &Tab,
        terminal: &Terminal,
    ) -> Option<crate::paint::block_view::BlockViewHitRows> {
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
                // v1.19.0 (PLAN_v1190 §4 P1-1): region track gated exactly
                // like `live` (hit-test geometry mirrors the paint path).
                region: (!editor_mode)
                    .then(|| {
                        crate::paint::region_cache::region_source_for(terminal, terminal.grid())
                    })
                    .flatten(),
            }),
        )
    }

    /// True when the block view is the active renderer (Editor mode, not in
    /// an alt-screen app). Centralises the dispatch so mouse/copy paths stay
    /// consistent.
    #[inline]
    pub(crate) fn block_view_active(&self) -> bool {
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
    #[inline]
    pub(crate) fn mouse_reporting_active(&self) -> bool {
        if self.interaction.mods.state().shift_key() {
            return false; // Shift = force terminal selection
        }
        self.sessions
            .active()
            .and_then(|tab| tab.with_terminal(|t| t.mouse_protocol() != MouseProtocol::Off))
            .unwrap_or(false)
    }

    /// Which foldable block (if any) owns the physical-pixel `y`. Returns
    /// `Some(id)` for completed blocks, `Some(None)` for the in-flight
    /// command, or `None` when not on a block/chrome row.
    #[inline]
    pub(crate) fn block_at(&self, y: f32) -> Option<Option<BlockId>> {
        let (rows, _, _, chrome_bands) = self.compute_block_view_rows()?;
        // Find the row whose y-range contains `y`. Header/Command/Output rows
        // retain their owning finalized block; LiveCommand is in-flight.
        // v1.20.0 (D8): under the overlay flag chrome bands are screen-space
        // overlays with no laid row — the band scan below keeps header hover
        // (and its inline actions) reachable; gap/separator stay no-hover.
        for row in &rows {
            if y >= row.y_top && y < row.y_bottom {
                use weft_core::selection::BlockViewRowKind;
                if matches!(&row.kind, BlockViewRowKind::LiveCommand) {
                    return Some(None);
                }
                if let Some(id) =
                    crate::block_component::hovered_block_for_row(&row.kind, row.block_id)
                {
                    return Some(Some(id));
                }
            }
        }
        crate::block_component::hovered_block_in_chrome_bands(&chrome_bands, y).map(Some)
    }
}
