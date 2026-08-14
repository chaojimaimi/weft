//! Grid cell instance builder — the per-cell GPU instance buffer for
//! the terminal grid (text, colors, cursor, selection, hyperlinks).
//!
//! Extracted from `renderer.rs` (M2). Still `impl MetalRenderer` because
//! it needs `self.atlas`, `self.theme`, `self.grid_row_cache`, etc.
//!
//! v1.4.2 Phase B3: rewired to use B2's pure-logic `build_row_instances`
//! collector. The per-row cache now stores `GridRowInstances` (bg runs +
//! glyph instances) instead of a flat `Vec<f32>`. Each frame flattens the
//! cache into a `GridInstanceBatch` (bg_stream + glyph_stream) for the
//! Metal dual-stream pipeline.

use crate::paint::grid_instances::{build_row_instances, GridInstanceBatch, GridRowInstances};
use crate::paint::primitives::color_to_normalized;
use crate::renderer::MetalRenderer;
use weft_core::grid::{Color, CursorStyle};
use weft_core::selection::SelectionHandler;

/// v1.4.2 Phase B3: per-row dual-stream cache entry. Re-exports B2's
/// `GridRowInstances` so the renderer's `grid_row_cache` field type reads
/// as a stable local alias (decoupling it from future B2 renames).
pub(crate) type GridRowDualCache = GridRowInstances;

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
    /// v1.10.4: true when the terminal is in alt-screen mode (DEC 1049).
    /// Used to soften the cursor color for underline/bar styles so they
    /// don't appear as a jarring bright line at TUI input edges.
    pub(crate) is_alt_screen: bool,
}

/// v1.10.4: Resolve the effective cursor color for the grid path.
///
/// Alt-screen TUIs (opencode/vim/htop) position the cursor at their
/// input/prompt edges. A bright cursor color (#f0d4a8 in weft-warm)
/// renders as a jarring "white bar" when the cursor style is Underline/Bar
/// (a thin line spanning the cell). Blend the cursor color toward the
/// foreground at 50% so the cursor stays visible but no longer dominates
/// the TUI's own visual hierarchy.
///
/// Block cursors keep full intensity (they're position indicators that
/// need to be unmistakable — dimming them would hurt usability in vim/etc).
fn alt_screen_cursor_color(
    raw_cursor: [f32; 4],
    default_fg: [f32; 4],
    is_alt_screen: bool,
    cursor_style: CursorStyle,
) -> [f32; 4] {
    if is_alt_screen && !cursor_style.is_block() {
        [
            raw_cursor[0] * 0.5 + default_fg[0] * 0.5,
            raw_cursor[1] * 0.5 + default_fg[1] * 0.5,
            raw_cursor[2] * 0.5 + default_fg[2] * 0.5,
            raw_cursor[3],
        ]
    } else {
        raw_cursor
    }
}

impl MetalRenderer {
    /// v1.0 P1.5-B1: Build per-cell instance data for the grid. Each cell
    /// becomes a single 64-byte instance (origin/size/uv_rect/fg/bg) drawn
    /// against a static 4-vertex quad + 6-index buffer. Replaces the old
    /// 6-vertex-per-cell emission (~288 B/cell) — ~4.5x smaller per-frame
    /// upload. The per-row cache (`grid_row_cache`) stores instance floats
    /// (16 per cell) instead of vertex floats (72 per cell).
    ///
    /// v1.4.2 Phase B3: now produces a dual-stream `GridInstanceBatch`
    /// (bg runs + glyph instances) instead of a single flat `Vec<f32>`.
    /// The per-row cache stores `GridRowInstances` (B2 types). Dirty rows
    /// are rebuilt by calling B2's `build_row_instances`; clean rows are
    /// reused from the previous frame. The flatten step resolves glyph UVs
    /// via `self.atlas` and splits into `bg_stream` (8 floats/run) +
    /// `glyph_stream` (16 floats/cell).
    ///
    /// R3-1: returns `(batch, dirty_row_count)` so the frame trace can
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
    ) -> (GridInstanceBatch, usize) {
        let cw = self.cell_width() as f32;
        let ch = self.cell_height() as f32;
        let num_rows = grid.num_rows;
        let num_cols = grid.num_cols;

        let default_fg = color_to_normalized(self.theme.foreground);
        let default_bg = color_to_normalized(self.theme.background);
        let raw_cursor_color = color_to_normalized(self.theme.cursor);
        let cursor_color = alt_screen_cursor_color(
            raw_cursor_color,
            default_fg,
            policy.is_alt_screen,
            policy.cursor_style,
        );
        let selection_bg = {
            let accent = color_to_normalized(self.theme.accent);
            let mut c = [
                accent[0] * 0.35 + default_bg[0] * 0.65,
                accent[1] * 0.35 + default_bg[1] * 0.65,
                accent[2] * 0.35 + default_bg[2] * 0.65,
                1.0,
            ];
            c[3] = 0.60;
            c
        };

        let force_full = self.force_full_grid.get()
            || grid.scroll_offset != self.prev_scroll_offset.get()
            || self.grid_cache_dims.get() != (num_rows, num_cols)
            || primary_screen_mask_changed(
                self.prev_primary_screen_row_start.get(),
                policy.hidden_before_row,
            )
            || selection.selection.is_some()
            || selection.selecting;

        self.force_full_cached.set(force_full);

        let pending_scroll = grid.take_pending_scroll();
        self.pending_scroll_delta.set(pending_scroll);
        if !force_full && pending_scroll != 0 {
            let mut cache = self.grid_row_cache.borrow_mut();
            if cache.len() == num_rows {
                if pending_scroll > 0 {
                    let d = pending_scroll as usize;
                    if d < cache.len() {
                        cache.drain(0..d);
                        for _ in 0..d {
                            cache.push(GridRowInstances::default());
                        }
                    } else {
                        for c in cache.iter_mut() {
                            *c = GridRowInstances::default();
                        }
                    }
                } else {
                    let d = (-pending_scroll) as usize;
                    if d < cache.len() {
                        for _ in 0..d {
                            cache.insert(0, GridRowInstances::default());
                        }
                        cache.truncate(num_rows);
                    } else {
                        for c in cache.iter_mut() {
                            *c = GridRowInstances::default();
                        }
                    }
                }
            }
        }

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
                if let Some(prev) = self.prev_cursor_row.get() {
                    if prev != cursor.row && !dirty.contains(&prev) {
                        dirty.push(prev);
                    }
                }
            }
            dirty
        };

        self.force_full_grid.set(false);
        self.prev_cursor_row.set(Some(cursor.row));
        self.prev_cursor_col.set(Some(cursor.col));
        self.prev_show_cursor.set(policy.show_cursor);
        self.prev_scroll_offset.set(grid.scroll_offset);
        self.prev_primary_screen_row_start
            .set(policy.hidden_before_row);
        self.grid_cache_dims.set((num_rows, num_cols));

        let mut cache = self.grid_row_cache.borrow_mut();
        if cache.len() != num_rows {
            cache.resize(num_rows, GridRowInstances::default());
        }

        if !force_full && pending_scroll != 0 {
            for (i, rv) in cache.iter().enumerate() {
                if rv.bg_instances.is_empty() && !rows_to_rebuild.contains(&i) {
                    rows_to_rebuild.push(i);
                }
            }
        }

        if rows_to_rebuild.is_empty() {
            drop(cache);
            self.instances_unchanged.set(true);
            return (GridInstanceBatch::default(), 0);
        }
        self.instances_unchanged.set(false);

        // Layout offsets read from `self.layout_ctx` (set by draw() entry).
        let chrome_left = self.layout_ctx.map(|c| c.chrome_left).unwrap_or(0.0);
        let pane_origin_x = self.layout_ctx.map(|c| c.pane_origin.0).unwrap_or(0.0);
        let chrome_top = self.layout_ctx.map(|c| c.chrome_top).unwrap_or(0.0);
        let pane_origin_y = self.layout_ctx.map(|c| c.pane_origin.1).unwrap_or(0.0);
        let origin_x = self.padding_x + chrome_left + pane_origin_x;
        let origin_y_base = self.padding_y + chrome_top + pane_origin_y;

        let mut rebuilt_rows = 0usize;
        for &row in &rows_to_rebuild {
            if primary_screen_row_hidden(row, policy.hidden_before_row, policy.owned_rows) {
                cache[row] = GridRowInstances::default();
                continue;
            }
            rebuilt_rows += 1;
            cache[row] = build_row_instances(
                grid,
                palette,
                row,
                default_fg,
                default_bg,
                cursor_color,
                selection_bg,
                cursor,
                policy.cursor_style,
                policy.show_cursor,
                selection,
                self.opacity,
                self.minimum_contrast,
                cw,
                ch,
                origin_x,
                origin_y_base + row as f32 * ch,
            );
        }
        drop(cache);

        // Flatten the per-row cache into a dual-stream batch. Glyph UVs
        // are resolved here (needs `self.atlas`) via the closure passed to
        // `GridInstanceBatch::push_row`.
        let cache = self.grid_row_cache.borrow();
        let mut batch = GridInstanceBatch::with_capacity(num_rows, num_cols);
        for row_inst in cache.iter() {
            batch.push_row(row_inst, &|ch, cluster, style| {
                self.resolve_glyph_uv(ch, cluster, style)
            });
        }
        (batch, rebuilt_rows)
    }

    /// v1.3 Batch 5 / v1.4.2 Phase B3: Build grid instances for a
    /// **background** (non-active) pane. Returns a dual-stream
    /// `GridInstanceBatch` (bg + glyph) instead of a flat `Vec<f32>`.
    ///
    /// - Does NOT touch `grid_row_cache` / `prev_cursor_*` / `force_full_grid`
    ///   or any other active-pane cache state.
    /// - Does NOT render the cursor or selection (active pane only).
    /// - Rebuilds every cell every frame (no dirty-row tracking). Acceptable
    ///   because background panes are typically idle.
    pub(crate) fn build_grid_instances_for_background_pane(
        &self,
        terminal: &weft_core::vt::Terminal,
    ) -> GridInstanceBatch {
        let grid = terminal.grid();
        let palette = terminal.palette();
        let cw = self.cell_width() as f32;
        let ch = self.cell_height() as f32;
        let num_rows = grid.num_rows;
        let num_cols = grid.num_cols;

        let default_fg = color_to_normalized(self.theme.foreground);
        let default_bg = color_to_normalized(self.theme.background);
        // Cursor/selection colors unused (background pane has no cursor/selection).
        let cursor_color = default_fg;
        let selection_bg = default_bg;
        let cursor = weft_core::grid::Cursor::default();
        let cursor_style = weft_core::grid::CursorStyle::Block;
        let selection = SelectionHandler::new();

        let chrome_left = self.layout_ctx.map(|c| c.chrome_left).unwrap_or(0.0);
        let pane_origin_x = self.layout_ctx.map(|c| c.pane_origin.0).unwrap_or(0.0);
        let chrome_top = self.layout_ctx.map(|c| c.chrome_top).unwrap_or(0.0);
        let pane_origin_y = self.layout_ctx.map(|c| c.pane_origin.1).unwrap_or(0.0);
        let origin_x = self.padding_x + chrome_left + pane_origin_x;
        let origin_y_base = self.padding_y + chrome_top + pane_origin_y;

        let hidden_before_row = terminal.primary_screen_visible_row_start();
        let owned_rows = terminal.primary_screen_viewport_ownership();

        let mut batch = GridInstanceBatch::with_capacity(num_rows, num_cols);
        for row in 0..num_rows {
            if primary_screen_row_hidden(row, hidden_before_row, owned_rows) {
                continue;
            }
            let row_inst = build_row_instances(
                grid,
                palette,
                row,
                default_fg,
                default_bg,
                cursor_color,
                selection_bg,
                &cursor,
                cursor_style,
                false,
                &selection,
                self.opacity,
                self.minimum_contrast,
                cw,
                ch,
                origin_x,
                origin_y_base + row as f32 * ch,
            );
            batch.push_row(&row_inst, &|ch, cluster, style| {
                self.resolve_glyph_uv(ch, cluster, style)
            });
        }
        batch
    }

    /// Resolve a cell's glyph UV rect (+ color-atlas flag) from the atlas.
    ///
    /// v1.6.0: multi-scalar graphemes (EXTRA flag) resolve via the cluster
    /// atlas path; single-scalar cells use the fast `atlas.get_style(ch)`
    /// path. A cluster that hasn't been rasterized yet falls through to the
    /// lead scalar so the cell isn't blank — the cluster rasterizes on a
    /// later frame (or via warmup).
    ///
    /// v1.10.4: returns `is_color` for glyphs stored in the RGBA color
    /// atlas (color emoji) so `push_row` can emit the fg.a=2.0 sentinel.
    ///
    /// v1.10.12: `style` (bold/italic from the cell's SGR flags) selects
    /// the atlas face. Clusters (multi-scalar graphemes) always resolve
    /// regular — fallback fonts have no style faces.
    fn resolve_glyph_uv(
        &self,
        ch: char,
        cluster: Option<&str>,
        style: crate::glyph::GlyphStyle,
    ) -> ([f32; 4], bool) {
        if let Some(cluster) = cluster {
            if let Some(glyph) = self.atlas.get_cluster(cluster) {
                let (u, v) = glyph.uv_origin;
                let (uw, vh) = glyph.uv_size;
                return ([u, v + vh, u + uw, v], glyph.is_color);
            }
        }
        let ch_resolved = if ch == '\0' { ' ' } else { ch };
        if let Some(glyph) = self.atlas.get_style(ch_resolved, style) {
            let (u, v) = glyph.uv_origin;
            let (uw, vh) = glyph.uv_size;
            // V-swap compensates for CAMetalLayer's vertical flip (same
            // convention as the legacy single-stream path).
            ([u, v + vh, u + uw, v], glyph.is_color)
        } else if let Some(glyph) = self
            .atlas
            .get_style(ch_resolved, crate::glyph::GlyphStyle::REGULAR)
        {
            // v1.10.12: styled face not rasterized yet (non-ASCII char before
            // warmup) — degrade to the regular face instead of a blank cell.
            let (u, v) = glyph.uv_origin;
            let (uw, vh) = glyph.uv_size;
            ([u, v + vh, u + uw, v], glyph.is_color)
        } else {
            let (u, v) = self
                .atlas
                .get(' ')
                .map(|g| g.uv_origin)
                .unwrap_or((0.0, 0.0));
            let (uw, vh) = self.atlas.get(' ').map(|g| g.uv_size).unwrap_or((0.0, 0.0));
            ([u, v + vh, u + uw, v], false)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{alt_screen_cursor_color, primary_screen_mask_changed, primary_screen_row_hidden};
    use weft_core::grid::CursorStyle;

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

    // ── v1.10.4: alt-screen cursor color softening ──────────────────────

    #[test]
    fn alt_screen_blends_underline_cursor_toward_foreground() {
        let cursor = [0.94, 0.83, 0.66, 1.0]; // #f0d4a8 amber-white
        let fg = [0.88, 0.83, 0.77, 1.0]; // #e0d4c4 warm cream
        let softened = alt_screen_cursor_color(cursor, fg, true, CursorStyle::Underline);
        // 50% blend: each channel = (cursor + fg) / 2
        for i in 0..3 {
            assert!((softened[i] - (cursor[i] + fg[i]) * 0.5).abs() < 1e-6);
        }
        assert_eq!(softened[3], cursor[3], "alpha unchanged");
    }

    #[test]
    fn alt_screen_keeps_block_cursor_at_full_intensity() {
        let cursor = [0.94, 0.83, 0.66, 1.0];
        let fg = [0.88, 0.83, 0.77, 1.0];
        let result = alt_screen_cursor_color(cursor, fg, true, CursorStyle::Block);
        assert_eq!(
            result, cursor,
            "Block cursor keeps full intensity in alt-screen"
        );
    }

    #[test]
    fn primary_screen_keeps_cursor_at_full_intensity() {
        let cursor = [0.94, 0.83, 0.66, 1.0];
        let fg = [0.5, 0.5, 0.5, 1.0];
        let result = alt_screen_cursor_color(cursor, fg, false, CursorStyle::Underline);
        assert_eq!(
            result, cursor,
            "non-alt-screen keeps cursor at full intensity"
        );
    }

    #[test]
    fn alt_screen_bar_cursor_also_blended() {
        let cursor = [0.94, 0.83, 0.66, 1.0];
        let fg = [0.2, 0.2, 0.2, 1.0];
        let result = alt_screen_cursor_color(cursor, fg, true, CursorStyle::Bar);
        assert_ne!(result, cursor, "Bar cursor is softened in alt-screen");
        assert!(result[0] < cursor[0], "blended toward darker fg");
    }
}
