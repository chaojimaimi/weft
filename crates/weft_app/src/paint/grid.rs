//! Grid cell instance builder — the per-cell GPU instance buffer for
//! the terminal grid (text, colors, cursor, selection, hyperlinks).
//!
//! Extracted from `renderer.rs` (M2). Still `impl MetalRenderer` because
//! it needs `self.atlas`, `self.theme`, `self.grid_row_cache`, etc.

use crate::paint::primitives::{color_to_normalized, push_cell_instance, resolve_cell_color};
use crate::renderer::MetalRenderer;
use weft_core::grid::{CellFlags, CellWidth, Color, CursorStyle};
use weft_core::selection::SelectionHandler;

fn primary_screen_row_hidden(
    row: usize,
    hidden_before_row: Option<usize>,
    owned_rows: Option<&[bool]>,
) -> bool {
    owned_rows
        .and_then(|owned| owned.get(row))
        .is_some_and(|owned| !owned)
        || hidden_before_row.is_some_and(|start| row < start)
}

fn primary_screen_mask_changed(previous: Option<usize>, current: Option<usize>) -> bool {
    previous != current
}

pub(crate) struct GridViewPolicy<'a> {
    pub(crate) show_cursor: bool,
    pub(crate) cursor_style: CursorStyle,
    pub(crate) hidden_before_row: Option<usize>,
    pub(crate) owned_rows: Option<&'a [bool]>,
}

impl MetalRenderer {
    /// v1.0 P1.5-B1: Build per-cell instance data for the grid. Each cell
    /// becomes a single 64-byte instance (origin/size/uv_rect/fg/bg) drawn
    /// against a static 4-vertex quad + 6-index buffer. Replaces the old
    /// 6-vertex-per-cell emission (~288 B/cell) — ~4.5x smaller per-frame
    /// upload. The per-row cache (`grid_row_cache`) stores instance floats
    /// (16 per cell) instead of vertex floats (72 per cell).
    ///
    /// R3-1: returns `(instances, dirty_row_count)` so the frame trace can
    /// report how many rows were actually rebuilt this frame. `dirty_row_count`
    /// is the number of non-hidden rows rebuilt: `num_rows` (or fewer if
    /// primary-screen masking hides some) on `force_full`, 0 on the idle
    /// fast path, and the actual dirty set size otherwise.
    pub(crate) fn build_grid_instances(
        &self,
        grid: &weft_core::grid::Grid,
        palette: &[Color; 256],
        cursor: &weft_core::grid::Cursor,
        selection: &SelectionHandler,
        policy: GridViewPolicy,
    ) -> (Vec<f32>, usize) {
        // Render at the atlas's native cell size — do NOT stretch cells to
        // fill the viewport (cw = viewport / num_cols). Stretching distorts
        // glyphs and, for full-width CJK, amplifies the baked intra-slot
        // padding into a gap that drifts wider on every resize (cw diverges
        // from cell_width as the window resizes within a column bucket). The
        // grid occupies num_cols * cell_width px; any remainder is background.
        // This also keeps rendered positions aligned with mouse hit-testing,
        // which already divides by cell_width.
        let cw = self.cell_width() as f32;
        let ch = self.cell_height() as f32;
        let num_rows = grid.num_rows;
        let num_cols = grid.num_cols;

        // Theme-derived colors (resolved per-frame from the active theme).
        let default_fg = color_to_normalized(self.theme.foreground);
        let default_bg = color_to_normalized(self.theme.background);
        let cursor_color = color_to_normalized(self.theme.cursor);
        // v1.0 fix: use an accent-based blend (35% accent + 65% background)
        // for the selection color. The old approach used `theme.selection`
        // directly, but many themes had selection colors too close to the
        // background (e.g. solarized-dark: bg #002b36, selection #073642 —
        // nearly invisible at 55% opacity). The accent color is designed
        // to contrast with the background, so blending it in ensures the
        // selection is visible across ALL themes. Each theme gets a
        // different selection tint because the accent varies per theme.
        let selection_bg = {
            let accent = color_to_normalized(self.theme.accent);
            let mut c = [
                accent[0] * 0.35 + default_bg[0] * 0.65,
                accent[1] * 0.35 + default_bg[1] * 0.65,
                accent[2] * 0.35 + default_bg[2] * 0.65,
                1.0,
            ];
            // Semi-transparent so the underlying text stays readable.
            c[3] = 0.60;
            c
        };

        // v1.0 P0-b: incremental grid rendering. Instead of iterating every
        // cell every frame, we cache per-row vertices and only rebuild rows
        // that changed (dirty-tracked by Grid). A full redraw is forced when:
        //   - the caller signals a global change (resize/theme/tab-switch)
        //   - the grid scrolled (scroll_offset differs → scrollback cells shown)
        //   - the grid dimensions changed (resize reflow)
        // Cursor row is always rebuilt (blink / move changes the cursor cell).
        let force_full = self.force_full_grid.get()
            || grid.scroll_offset != self.prev_scroll_offset.get()
            || self.grid_cache_dims.get() != (num_rows, num_cols)
            || primary_screen_mask_changed(
                self.prev_primary_screen_row_start.get(),
                policy.hidden_before_row,
            )
            // Selection overlay changes cell colors — rebuild all rows while
            // a selection is active or being dragged.
            || selection.selection.is_some()
            || selection.selecting;

        // v1.0 P0-c: stash force_full on self so draw()'s epilogue (which
        // runs after this method returns) can decide whether to skip the
        // GPU scroll blit on forced-full frames.
        self.force_full_cached.set(force_full);

        // v1.0 P0-c: CPU-side cache shift for viewport scrolls. When the
        // terminal scrolls (newline at bottom), the viewport rows shift up
        // by N — the previously-rendered content at rows 0..rows-N is now
        // at rows N..rows. Instead of rebuilding all rows, we shift the
        // per-row vertex cache to match and only rebuild the newly exposed
        // rows (empty cache entries).
        //
        // Key correctness: pre-scroll cell writes set dirty_occ on those
        // rows, which moves WITH the rows during scroll_up. So dirty rows
        // are still detected and rebuilt even after the cache shift.
        let pending_scroll = grid.take_pending_scroll();
        // v1.0 P0-c: stash on self so draw()'s epilogue can issue a GPU blit.
        self.pending_scroll_delta.set(pending_scroll);
        if !force_full && pending_scroll != 0 {
            let mut cache = self.grid_row_cache.borrow_mut();
            if cache.len() == num_rows {
                if pending_scroll > 0 {
                    // Scroll up: rows moved up, new blank rows at bottom.
                    let d = pending_scroll as usize;
                    if d < cache.len() {
                        cache.drain(0..d);
                        for _ in 0..d {
                            cache.push(Vec::new());
                        }
                    } else {
                        for c in cache.iter_mut() {
                            c.clear();
                        }
                    }
                } else {
                    // Scroll down: rows moved down, new blank rows at top.
                    let d = (-pending_scroll) as usize;
                    if d < cache.len() {
                        for _ in 0..d {
                            cache.insert(0, Vec::new());
                        }
                        cache.truncate(num_rows);
                    } else {
                        for c in cache.iter_mut() {
                            c.clear();
                        }
                    }
                }
            }
        }

        // Determine which rows need rebuilding.
        // v1.0 P1.5-B2: only rebuild the cursor row when its state actually
        // changed (blink toggle, move, or show/hide). Previously the cursor
        // row was rebuilt every frame — wasteful for steady (non-blinking)
        // cursors or idle terminals where nothing changes. `show_cursor`
        // already encodes blink phase (caller passes
        // `cursor_visible && cursor_blink_on && prompt.is_none()`), so a
        // stable `show_cursor` + stable position means the cached cursor
        // cell is still correct.
        let cursor_changed = force_full
            || self.prev_show_cursor.get() != policy.show_cursor
            || self.prev_cursor_row.get() != Some(cursor.row)
            || self.prev_cursor_col.get() != Some(cursor.col);
        let mut rows_to_rebuild: Vec<usize> = if force_full {
            (0..num_rows).collect()
        } else {
            let mut dirty: Vec<usize> = grid.dirty_rows().map(|(r, _)| r).collect();
            if cursor_changed {
                if !dirty.contains(&cursor.row) {
                    dirty.push(cursor.row);
                }
                // Previous cursor row: when the cursor moves, the old row
                // loses its cursor overlay and must be rebuilt to show plain
                // content.
                if let Some(prev) = self.prev_cursor_row.get() {
                    if prev != cursor.row && !dirty.contains(&prev) {
                        dirty.push(prev);
                    }
                }
            }
            dirty
        };

        // Update cached state for next frame's comparison.
        self.force_full_grid.set(false);
        self.prev_cursor_row.set(Some(cursor.row));
        self.prev_cursor_col.set(Some(cursor.col));
        self.prev_show_cursor.set(policy.show_cursor);
        self.prev_scroll_offset.set(grid.scroll_offset);
        self.prev_primary_screen_row_start
            .set(policy.hidden_before_row);
        self.grid_cache_dims.set((num_rows, num_cols));

        // Build vertices for dirty rows only. We build into a local Vec
        // (not the RefCell) to avoid holding a RefMut while accessing self
        // fields (atlas, layout_ctx, etc.) inside the per-cell loop.
        let mut cache = self.grid_row_cache.borrow_mut();
        if cache.len() != num_rows {
            cache.resize(num_rows, Vec::new());
        }

        // v1.0 P0-c: after cache shift, add rows with empty cache entries
        // (newly exposed by scroll) to the rebuild set.
        if !force_full && pending_scroll != 0 {
            for (i, rv) in cache.iter().enumerate() {
                if rv.is_empty() && !rows_to_rebuild.contains(&i) {
                    rows_to_rebuild.push(i);
                }
            }
        }

        // v1.0 P1.5-B2: if no rows need rebuilding this frame, skip the
        // per-cell loop + flatten memcpy + GPU upload entirely. The draw()
        // method reads `instances_unchanged` and relies on the offscreen
        // render pass's `Load` action to preserve the previous frame's grid
        // content. This is the key optimization for idle frames: no terminal
        // output, no cursor blink toggle, no scroll → no work.
        if rows_to_rebuild.is_empty() {
            drop(cache);
            self.instances_unchanged.set(true);
            return (Vec::new(), 0);
        }
        self.instances_unchanged.set(false);

        let mut rebuilt_rows = 0usize;
        for &row in &rows_to_rebuild {
            if primary_screen_row_hidden(row, policy.hidden_before_row, policy.owned_rows) {
                cache[row].clear();
                continue;
            }
            rebuilt_rows += 1;
            // v1.0 P1.5-B1: per-row instance buffer. 16 floats/cell + slack
            // for cursor bar/underline + hyperlink underline decorations.
            let mut instances = Vec::with_capacity(num_cols * 16 + 48);
            for col in 0..num_cols {
                let cell = grid.cell(row, col);

                // Skip wide char spacers (rendered as part of the preceding cell)
                if cell.flags.contains(CellFlags::WIDE_SPACER) {
                    continue;
                }

                let chrome_left = self.layout_ctx.map(|c| c.chrome_left).unwrap_or(0.0);
                let x = self.padding_x + chrome_left + col as f32 * cw;
                // v0.9 H1 fix: shift grid down by chrome_top (tab bar height)
                // so the first row isn't covered by the tab bar. The LayoutCtx
                // is set on self at the top of draw(); chrome_top is 0 when
                // there's only one tab (no tab bar drawn).
                let chrome_top = self.layout_ctx.map(|c| c.chrome_top).unwrap_or(0.0);
                let y = self.padding_y + chrome_top + row as f32 * ch;

                // Determine cell colors (resolve the cell's color-origin against
                // the palette / theme defaults).
                let mut fg = resolve_cell_color(cell.fg, default_fg, palette);
                let mut bg = resolve_cell_color(cell.bg, default_bg, palette);

                // v1.0 fix: honor SGR reverse video (DEC SGR 7 / `CSI 7m`).
                // The VT parser sets CellFlags::REVERSE on cells printed while
                // inverse video is active (e.g. `less` search-match highlight).
                // Without this swap, matched text in `less` jumps to the right
                // place but is never highlighted — it renders with normal
                // fg/bg. Swap BEFORE the alpha scaling below so the opacity is
                // applied to the (now background) color consistently.
                if cell.flags.contains(CellFlags::REVERSE) {
                    std::mem::swap(&mut fg, &mut bg);
                }

                // Scale the plain background alpha by window opacity so empty
                // cells show the desktop through them. Text/selection/cursor
                // pick their own colors with alpha 1.0 in `final_bg` below, so
                // they stay fully opaque regardless of this scaling.
                bg[3] *= self.opacity;

                // Check if this is the cursor position
                let is_cursor = policy.show_cursor && row == cursor.row && col == cursor.col;

                // Check if this cell is in the selection
                let is_selected = selection
                    .selection
                    .as_ref()
                    .is_some_and(|sel| sel.contains(row, col));

                // Look up glyph UV
                let ch_char =
                    if cell.character == '\0' || cell.flags.contains(CellFlags::WIDE_SPACER) {
                        ' '
                    } else {
                        cell.character
                    };

                let (u0, v0, u1, v1) = if let Some(glyph) = self.atlas.get(ch_char) {
                    let (u, v) = glyph.uv_origin;
                    let (uw, vh) = glyph.uv_size;
                    (u, v, u + uw, v + vh)
                } else {
                    // Character not in atlas — use space
                    let (u, v) = self
                        .atlas
                        .get(' ')
                        .map(|g| g.uv_origin)
                        .unwrap_or((0.0, 0.0));
                    let (uw, vh) = self.atlas.get(' ').map(|g| g.uv_size).unwrap_or((0.0, 0.0));
                    (u, v, u + uw, v + vh)
                };
                // Swap V to compensate for the CAMetalLayer's vertical flip: keep the
                // glyph upright on screen while clip.y maps row 0 to the top.
                let (v0, v1) = (v1, v0);

                // Override colors for cursor
                let final_fg = if is_cursor {
                    if policy.cursor_style.is_block() {
                        [0.0, 0.0, 0.0, 1.0] // Black text on cursor block
                    } else {
                        cursor_color
                    }
                } else {
                    fg
                };

                let final_bg = if is_cursor && policy.cursor_style.is_block() {
                    cursor_color
                } else if is_selected {
                    selection_bg
                } else if is_cursor && policy.cursor_style.is_bar() {
                    // Bar cursor: only highlight the left 2 pixels
                    // We'll draw the full cell with normal bg, then overlay bar later
                    bg
                } else {
                    bg
                };

                // Determine cell width for rendering
                let cell_render_width = if cell.width == CellWidth::Full && col + 1 < num_cols {
                    cw * 2.0
                } else {
                    cw
                };

                let x0 = x;
                let y0 = y;
                let x1 = x + cell_render_width;
                let y1 = y + ch;

                // v1.0 P1.5-B1: emit one instance per cell. The V-swap
                // (`(v0, v1) = (v1, v0)` above) is stored directly in the
                // instance's uv_rect: corner.y=0 (top of cell) samples v0
                // (bottom of glyph in atlas space), compensating for the
                // CAMetalLayer vertical flip — same convention as the old
                // per-vertex emission.
                push_cell_instance(
                    &mut instances,
                    [x0, y0, x1, y1],
                    [u0, v0, u1, v1],
                    final_fg,
                    final_bg,
                );

                // Draw bar/underline cursor overlay
                if is_cursor && policy.show_cursor {
                    if policy.cursor_style.is_bar() {
                        let bar_w = 2.0 * (self.viewport.0 / grid.num_cols as f32 / cw);
                        let bar_w = bar_w.max(1.0).min(cw * 0.15);
                        // v1.0 P1.5-B1: decoration instance. UV rect
                        // (0,0,0,1) samples the atlas at u=0 (empty)
                        // so mask=0 → only bg (cursor color) shows.
                        push_cell_instance(
                            &mut instances,
                            [x0, y0, x0 + bar_w, y1],
                            [0.0, 0.0, 0.0, 1.0],
                            [0.0; 4],
                            cursor_color,
                        );
                    } else if policy.cursor_style.is_underline() {
                        let line_h = 2.0;
                        push_cell_instance(
                            &mut instances,
                            [x0, y1 - line_h, x1, y1],
                            [0.0, 0.0, 0.0, 1.0],
                            [0.0; 4],
                            cursor_color,
                        );
                    }
                }

                // OSC 8 hyperlink underline: a thin cyan line at the cell's
                // baseline. Click handling is in main.rs (Cmd+Click → open URL
                // from the registry's side-map). Wide-char cells span 2 cols.
                if cell.flags.contains(CellFlags::HYPERLINK) {
                    // Step 4: 2.0px (was 1.5) to avoid sub-pixel blur at 1× scale.
                    let line_h = 2.0;
                    let link_color = [0.36, 0.62, 0.94, 1.0]; // soft cyan
                    push_cell_instance(
                        &mut instances,
                        [x0, y1 - line_h, x1, y1],
                        [0.0, 0.0, 0.0, 1.0],
                        [0.0; 4],
                        link_color,
                    );
                }
            }
            // v1.0 P1.5-B1: store this row's instances into the cache.
            cache[row] = instances;
        }

        // v1.0 P0-b: flatten the per-row cache into a single instance buffer.
        // Clean rows are reused from the previous frame; dirty rows were
        // rebuilt above. Each cell is 16 floats (one CellInstance).
        let mut out = Vec::with_capacity(num_rows * num_cols * 16);
        for rv in cache.iter() {
            out.extend_from_slice(rv);
        }
        (out, rebuilt_rows)
    }
}

#[cfg(test)]
mod tests {
    use super::{primary_screen_mask_changed, primary_screen_row_hidden};

    #[test]
    fn primary_screen_mask_hides_unowned_rows_without_mutating_the_grid() {
        let owned = [false, true, false, true];
        assert!(primary_screen_row_hidden(0, Some(1), Some(&owned)));
        assert!(!primary_screen_row_hidden(1, Some(1), Some(&owned)));
        assert!(primary_screen_row_hidden(2, Some(1), Some(&owned)));
        assert!(!primary_screen_row_hidden(3, Some(1), Some(&owned)));
        assert!(!primary_screen_row_hidden(3, None, None));
    }

    #[test]
    fn moving_or_removing_the_primary_screen_mask_invalidates_cached_rows() {
        assert!(primary_screen_mask_changed(Some(5), Some(2)));
        assert!(primary_screen_mask_changed(Some(5), None));
        assert!(primary_screen_mask_changed(None, Some(5)));
        assert!(!primary_screen_mask_changed(Some(5), Some(5)));
        assert!(!primary_screen_mask_changed(None, None));
    }
}
