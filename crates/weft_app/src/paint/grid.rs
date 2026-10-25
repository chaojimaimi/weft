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

use crate::layout::Rect;
use crate::paint::grid_instances::{build_row_instances, GridInstanceBatch, GridRowInstances};
use crate::paint::primitives::color_to_normalized;
use crate::renderer::MetalRenderer;
use weft_core::grid::{Color, CursorStyle};
use weft_core::selection::SelectionHandler;

/// v1.4.2 Phase B3: per-row dual-stream cache entry. Re-exports B2's
/// `GridRowInstances` so the renderer's `grid_row_cache` field type reads
/// as a stable local alias (decoupling it from future B2 renames).
pub(crate) type GridRowDualCache = GridRowInstances;

/// v1.12.2 B3-2 (PLAN_S2_render): per-background-pane row cache, keyed by
/// `pane_session_id` on `MetalRenderer::background_grid_row_caches`. Gives
/// background panes the same dirty-row incremental rebuild the active pane
/// has; the fingerprint fields mirror the active path's force-full
/// conditions (plus the layout origin, which the active path gets for free
/// via its per-resize force_full).
#[derive(Default)]
pub(crate) struct BackgroundGridRowCache {
    /// Cached per-row instances (same shape as the active `grid_row_cache`).
    rows: Vec<GridRowInstances>,
    /// Grid dims the rows were built at — any change (live-resize included)
    /// forces a full rebuild, matching the active path's semantics.
    dims: (usize, usize),
    /// Grid scroll offset at build time (scroll → full, like the active path).
    scroll_offset: usize,
    /// Layout origin the row coordinates were baked with — a pane move
    /// (split drag, sidebar, chrome change) without a grid dims change must
    /// still invalidate, or cached rows render at stale positions.
    origin: (f32, f32),
    /// Primary-screen visible-row start (the only mask component the active
    /// path fingerprints — mirrored for consistency).
    hidden_before_row: Option<usize>,
    /// P1 fix (rust-reviewer, 2nd round): alt-screen state at build time.
    /// Exiting alt (DEC 1049/1047 reset) restores the primary screen WITHOUT
    /// marking any row dirty and without touching dims/scroll/origin —
    /// without this fingerprint the cache kept showing alt (vim/less/htop)
    /// remnants until a focus change or resize.
    alt_active: bool,
    /// Cache-generation snapshot. `MetalRenderer::background_grid_generation`
    /// bumps on theme / minimum-contrast / bold-is-bright changes (colors are
    /// baked into the cached rows) — the active path gets this via
    /// `force_full_grid`, which the background path must not read (multi-pane
    /// frames set it unconditionally).
    generation: u64,
}

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
    /// v1.10.19: true for a primary-screen TUI grid view. The grid content
    /// origin is inset by the BlockView gutter so scrolling up into
    /// `primary_history_view` (which switches to BlockView) keeps every
    /// column at the same physical x — without the inset, grid content
    /// renders ~1.5 cols left of BlockView content and the transcript
    /// visibly shifts right on the transition. Alt-screen TUIs stay
    /// edge-to-edge (no gutter).
    pub(crate) inset_block_gutter: bool,
}

/// v1.10.19: Grid content origin x for `ctx`.
///
/// With `inset_block_gutter` the origin is BlockView's content left edge
/// (pane left + gutter), so the grid and the `primary_history_view`
/// BlockView place column 0 at the same physical x on a scroll-up
/// transition. Without it (alt-screen TUI), the origin is the pane's left
/// edge — the TUI paints edge-to-edge.
pub(crate) fn grid_content_origin_x(
    ctx: &crate::layout::LayoutCtx,
    inset_block_gutter: bool,
) -> f32 {
    if inset_block_gutter {
        crate::layout::block_content_x_bounds(ctx).0
    } else {
        ctx.left()
    }
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

/// v1.14.0 (P1-1): scrollback index of the row just ABOVE the visible
/// viewport top (the row whose tail `px` band must fill the top gap of a
/// sub-row-shifted frame). `None` when there is no such row: the viewport
/// is already at the history top (`offset == scrollback_len`) or there is
/// no scrollback at all. Pure — unit-testable without a renderer.
pub(crate) fn subrow_extra_row_index(scrollback_len: usize, scroll_offset: usize) -> Option<usize> {
    let offset = scroll_offset.min(scrollback_len);
    scrollback_len.checked_sub(offset + 1)
}

/// v1.14.0 (P1-1): build the extra top-row instances for a sub-row-shifted
/// grid frame. Baked one full row ABOVE `origin_y_base` so that a `+px`
/// push_row shift lands its bottom `px` band inside the viewport top gap
/// (the pre-fix hole the reviewer flagged); the shifted-down bottom row's
/// overflow is accepted (per-pane scissor clipping). The row renders with
/// cursor/selection disabled — both are viewport-indexed and cannot reach
/// a row above the top. Pure — unit-testable without a renderer.
#[allow(clippy::too_many_arguments)]
pub(crate) fn build_subrow_extra_top_row(
    grid: &weft_core::grid::Grid,
    palette: &[Color; 256],
    default_fg: [f32; 4],
    default_bg: [f32; 4],
    opacity: f32,
    minimum_contrast: f32,
    cw: f32,
    ch: f32,
    origin_x: f32,
    origin_y_base: f32,
    bold_is_bright: bool,
    hyperlink_color: [f32; 4],
) -> Option<GridRowInstances> {
    let idx = subrow_extra_row_index(grid.scrollback.len(), grid.scroll_offset())?;
    let row = grid.scrollback.get(idx)?;
    let cursor = weft_core::grid::Cursor::default();
    let selection = SelectionHandler::new();
    Some(build_row_instances(
        grid,
        palette,
        0,
        default_fg,
        default_bg,
        default_fg,
        default_bg,
        default_bg,
        &cursor,
        CursorStyle::Block,
        false,
        &selection,
        opacity,
        minimum_contrast,
        cw,
        ch,
        // rust-reviewer R1: the extra row must carry the REAL content origin
        // (chrome-left/sidebar/split inset) — a hardcoded 0.0 misaligned it
        // horizontally exactly when ctx.left() != 0.
        origin_x,
        origin_y_base - ch,
        bold_is_bright,
        hyperlink_color,
        Some(&row),
    ))
}

/// v1.14.1 (F2): pure bottom-edge scissor for a sub-row-shifted grid pane.
/// `subrow == false` (px == 0) or no clip line → `pane_bottom` unchanged;
/// otherwise the bottom is the minimum of the pane edge and the clip line.
/// Pure — unit-testable without a renderer.
pub(crate) fn grid_scissor_bottom(pane_bottom: f32, clip_line: Option<f32>, subrow: bool) -> f32 {
    match (subrow, clip_line) {
        (true, Some(line)) => pane_bottom.min(line),
        _ => pane_bottom,
    }
}

/// v1.14.1 (F2): the pane rect stored into `PaneInstanceRanges` with the
/// sub-row scroll clip baked into its bottom edge (`grid_scissor_bottom`).
///
/// Clip line = the integrated prompt editor's box top
/// (`prompt_layout_for_buffer(..).1.box_rect[1]` — the block-view
/// `region_bottom_y` precedent in `renderer/panes.rs`): PTY rows are laid
/// out over the FULL content height without subtracting the editor
/// (`terminal_geometry.rs`), so a px≠0 frame's shifted-down bottom rows
/// would otherwise overrun into the prompt box. Without an editor the line
/// falls back to the grid content bottom (`ctx.top() + num_rows × cell_h`,
/// the same origin the rows are baked at). `subrow == false` (px == 0)
/// leaves `pane_rect` untouched — zero behavior change on the idle path.
/// Baking at segment construction means the zoom_render replay (which
/// copies the stored segments) inherits the clip automatically.
pub(crate) fn grid_pane_clip_rect(
    pane_rect: Rect,
    ctx: &crate::layout::LayoutCtx,
    prompt: Option<&crate::paint::prompt::PromptDrawParams<'_>>,
    num_rows: usize,
    subrow: bool,
) -> Rect {
    let clip_line = match prompt {
        Some(p) => {
            crate::paint::prompt::prompt_layout_for_buffer(ctx, p.lines, p.cursor)
                .1
                .box_rect[1]
        }
        None => ctx.top() + num_rows as f32 * ctx.cell_h,
    };
    let bottom = grid_scissor_bottom(pane_rect[3], Some(clip_line), subrow);
    [pane_rect[0], pane_rect[1], pane_rect[2], bottom]
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
        // v1.10.22: shared selection color (theme.selection base + WCAG 3:1
        // adaptive guarantee). quad → GPU, painted → text contrast benchmark.
        let sel_colors = crate::paint::selection_color::selection_colors(&self.theme);
        let selection_bg = sel_colors.quad;
        let selection_painted = sel_colors.painted;

        // T15b (PLAN_v11217 §3.10): layout-origin fingerprint, mirroring the
        // background path's `entry.origin` check below. A pane move (split
        // drag, pane close re-layout) changes the baked row coordinates
        // without touching dims, scroll, or dirty rows; the renderer's
        // per-frame unconditional force used to mask this gap.
        let chrome_top = self.layout_ctx.map(|c| c.chrome_top).unwrap_or(0.0);
        let pane_origin_y = self.layout_ctx.map(|c| c.pane_origin.1).unwrap_or(0.0);
        // v1.10.19: primary-screen TUI grid views inset by the BlockView
        // gutter (grid_content_origin_x) so a scroll-up into
        // primary_history_view doesn't shift content 1.5 cols to the right.
        let origin_x = self
            .layout_ctx
            .map(|ctx| grid_content_origin_x(&ctx, policy.inset_block_gutter))
            .unwrap_or(self.padding_x);
        let origin_y_base = self.padding_y + chrome_top + pane_origin_y;

        let force_full = self.force_full_grid.get()
            || grid.scroll_offset() != self.prev_scroll_offset.get()
            || self.grid_cache_dims.get() != (num_rows, num_cols)
            || self.prev_grid_origin.get() != (origin_x, origin_y_base)
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
        self.prev_scroll_offset.set(grid.scroll_offset());
        self.prev_primary_screen_row_start
            .set(policy.hidden_before_row);
        self.grid_cache_dims.set((num_rows, num_cols));
        // T15b: advance the origin fingerprint unconditionally after the
        // force decision (same semantics as prev_scroll_offset: only the
        // decision above snapshots it, and the force_full_grid flag path
        // does not reset it — the next frame compares against reality).
        self.prev_grid_origin.set((origin_x, origin_y_base));

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

        // v1.14.0 (PLAN_v1140 T6): a non-zero sub-row scroll offset bypasses
        // the instances_unchanged early-return — the visible rows must be
        // re-flattened with the px shift every frame (the per-row INSTANCE
        // cache itself stays valid; only the serialization shifts). px == 0
        // keeps the zero-cost idle path exactly as before. The GPU blit fast
        // path keys off instances_unchanged, so a shifted frame correctly
        // re-renders instead of blitting stale content.
        let grid_scroll_px = self.grid_scroll_px;
        if rows_to_rebuild.is_empty() && grid_scroll_px == 0.0 {
            drop(cache);
            self.instances_unchanged.set(true);
            return (GridInstanceBatch::default(), 0);
        }
        self.instances_unchanged.set(false);

        // Layout offsets (chrome_top/origin_x/origin_y_base) were computed
        // above the force-full fingerprint — T15b moved them up so the
        // origin participates in the invalidation decision.

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
                selection_painted,
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
                self.bold_is_bright,
                self.theme.link,
                None,
            );
        }
        drop(cache);

        // Flatten the per-row cache into a dual-stream batch. Glyph UVs
        // are resolved here (needs `self.atlas`) via the closure passed to
        // `GridInstanceBatch::push_row`.
        let cache = self.grid_row_cache.borrow();
        let mut batch = GridInstanceBatch::with_capacity(num_rows + 1, num_cols);
        // P1-1: a px≠0 shift moves every cached row DOWN, leaving the
        // viewport-top band [origin_y, origin_y + px) empty. Fill it with
        // the scrollback row above the visible top, pushed with the SAME
        // shift after being baked one row above the viewport.
        if grid_scroll_px != 0.0 {
            if let Some(extra) = build_subrow_extra_top_row(
                grid,
                palette,
                default_fg,
                default_bg,
                self.opacity,
                self.minimum_contrast,
                cw,
                ch,
                origin_x,
                origin_y_base,
                self.bold_is_bright,
                self.theme.link,
            ) {
                batch.push_row(&extra, grid_scroll_px, &|ch, cluster, style| {
                    self.resolve_glyph_uv(ch, cluster, style)
                });
            }
        }
        for row_inst in cache.iter() {
            batch.push_row(row_inst, grid_scroll_px, &|ch, cluster, style| {
                self.resolve_glyph_uv(ch, cluster, style)
            });
        }
        (batch, rebuilt_rows)
    }

    /// v1.3 Batch 5 / v1.4.2 Phase B3: Build grid instances for a
    /// **background** (non-active) pane. Returns a dual-stream
    /// `GridInstanceBatch` (bg + glyph) plus the number of rows rebuilt
    /// this frame (test/observability hook, mirroring the active path).
    ///
    /// - Does NOT touch `grid_row_cache` / `prev_cursor_*` / `force_full_grid`
    ///   or any other active-pane cache state.
    /// - Does NOT render the cursor or selection (active pane only).
    /// - Does NOT consume the grid's pending-scroll delta (that belongs to
    ///   the active pane's GPU scroll-blit path).
    ///
    /// v1.12.2 B3-2 (PLAN_S2_render): the old "rebuilds every cell every
    /// frame" loop is gone — rows come from a per-pane cache keyed by
    /// `pane_session_id` (the same namespace pattern as the styled-line
    /// vertex cache), rebuilt only for the grid's dirty rows. A full rebuild
    /// happens on any fingerprint change: grid dims (live-resize included),
    /// scroll offset, layout-origin move, or primary-screen mask movement —
    /// the same invalidation conditions as the active path.
    pub(crate) fn build_grid_instances_for_background_pane(
        &self,
        terminal: &weft_core::vt::Terminal,
        pane_session_id: u64,
    ) -> (GridInstanceBatch, usize) {
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
        let selection_painted = default_bg;
        let cursor = weft_core::grid::Cursor::default();
        let cursor_style = weft_core::grid::CursorStyle::Block;
        let selection = SelectionHandler::new();

        let chrome_top = self.layout_ctx.map(|c| c.chrome_top).unwrap_or(0.0);
        let pane_origin_y = self.layout_ctx.map(|c| c.pane_origin.1).unwrap_or(0.0);
        // v1.10.19: mirror the active-pane grid policy — a background pane
        // whose primary-screen TUI owns the live grid also insets by the
        // BlockView gutter so its grid content stays column-aligned with
        // the BlockView it would switch to on history scroll.
        let inset_block_gutter =
            !terminal.is_alt_screen_active() && terminal.primary_screen_owns_live_view();
        let origin_x = self
            .layout_ctx
            .map(|ctx| grid_content_origin_x(&ctx, inset_block_gutter))
            .unwrap_or(self.padding_x);
        let origin_y_base = self.padding_y + chrome_top + pane_origin_y;

        let hidden_before_row = terminal.primary_screen_visible_row_start();
        let owned_rows = terminal.primary_screen_viewport_ownership();

        // ── B3-2: fingerprint the cache entry ────────────────────────────
        let mut caches = self.background_grid_row_caches.borrow_mut();
        let entry = caches.entry(pane_session_id).or_default();
        let force_full = entry.dims != (num_rows, num_cols)
            || entry.scroll_offset != grid.scroll_offset()
            || entry.origin.0 != origin_x
            || entry.origin.1 != origin_y_base
            || entry.hidden_before_row != hidden_before_row
            // P1 fix (rust-reviewer, 2nd round): the alt→primary flip (DEC
            // 1049/1047 exit) restores the primary screen with no dirty rows
            // and identical dims/scroll/origin — only this bool changes.
            || entry.alt_active != terminal.is_alt_screen_active()
            // P0 fix (rust-reviewer): a full-viewport scroll records ONLY a
            // pending_scroll delta and marks no rows dirty — without this
            // fingerprint the cache lagged streamed content by a row forever.
            || grid.pending_scroll() != 0
            // P1-1 fix (rust-reviewer): theme/contrast/bold-is-bright changes
            // invalidate baked colors via the generation counter (the active
            // path's force_full_grid is set unconditionally on multi-pane
            // frames and must not be read here).
            || entry.generation != self.background_grid_generation.get();

        let rows_to_rebuild: Vec<usize> = if force_full {
            (0..num_rows).collect()
        } else {
            grid.dirty_rows().map(|(r, _)| r).collect()
        };

        // Keep the fingerprint fresh for the next frame.
        entry.dims = (num_rows, num_cols);
        entry.scroll_offset = grid.scroll_offset();
        entry.origin = (origin_x, origin_y_base);
        entry.hidden_before_row = hidden_before_row;
        entry.alt_active = terminal.is_alt_screen_active();
        entry.generation = self.background_grid_generation.get();
        if entry.rows.len() != num_rows {
            entry.rows.resize(num_rows, GridRowInstances::default());
        }

        let mut rebuilt_rows = 0usize;
        for &row in &rows_to_rebuild {
            if primary_screen_row_hidden(row, hidden_before_row, owned_rows) {
                // Hidden rows carry no instances — mirror the active path's
                // "hidden row ⇒ default cache entry" handling.
                entry.rows[row] = GridRowInstances::default();
                continue;
            }
            rebuilt_rows += 1;
            entry.rows[row] = build_row_instances(
                grid,
                palette,
                row,
                default_fg,
                default_bg,
                cursor_color,
                selection_bg,
                selection_painted,
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
                self.bold_is_bright,
                self.theme.link,
                None,
            );
        }

        let mut batch = GridInstanceBatch::with_capacity(num_rows, num_cols);
        for row_inst in entry.rows.iter() {
            // v1.14.0 (T6): background panes carry no sub-row scroll offset
            // (the accumulator is active-pane-only) — plan-mandated 0.0.
            batch.push_row(row_inst, 0.0, &|ch, cluster, style| {
                self.resolve_glyph_uv(ch, cluster, style)
            });
        }
        (batch, rebuilt_rows)
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
    ///
    /// v1.13.11 N2.1: the lookup ladder is shared with the text outlets via
    /// `atlas.resolve_grapheme` — cluster first, then the cell's base
    /// character (`lead`) through the styled→regular degrade rungs. The
    /// transient "cluster miss renders the lead scalar" behavior is
    /// unchanged; only the duplicated ladder is gone.
    fn resolve_glyph_uv(
        &self,
        ch: char,
        cluster: Option<&str>,
        style: crate::glyph::GlyphStyle,
    ) -> ([f32; 4], bool) {
        let lead = if ch == '\0' { ' ' } else { ch };
        // Single-scalar cells have no cluster string: a one-scalar "grapheme"
        // falls straight through resolve_grapheme to the lead ladder.
        let mut buf = [0u8; 4];
        let single = lead.encode_utf8(&mut buf);
        if let Some((glyph, is_color)) =
            self.atlas
                .resolve_grapheme(cluster.unwrap_or(single), lead, style)
        {
            let (u, v) = glyph.uv_origin;
            let (uw, vh) = glyph.uv_size;
            // V-swap compensates for CAMetalLayer's vertical flip (same
            // convention as the legacy single-stream path).
            return ([u, v + vh, u + uw, v], is_color);
        }
        let (u, v) = self
            .atlas
            .get(' ')
            .map(|g| g.uv_origin)
            .unwrap_or((0.0, 0.0));
        let (uw, vh) = self.atlas.get(' ').map(|g| g.uv_size).unwrap_or((0.0, 0.0));
        ([u, v + vh, u + uw, v], false)
    }
}

#[cfg(test)]
mod tests;
