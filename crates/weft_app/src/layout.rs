// arch-gate: allow-over-800
// LayoutCtx + TerminalLayout + all shared layout formulas (grid position,
// sidebar metrics, chrome offsets). These are interdependent formulas that
// share private helpers; splitting would require re-exporting dozens of
// private fns or duplicating math.
//! Layout context and spacing tokens (v0.8 "Plisse" stage 1).
//!
//! All overlay vertex builders share a single [`LayoutCtx`] so coordinates
//! are derived from semantic methods (`left()`, `col_x(n)`, …) instead of
//! hand-rolled `f32` arithmetic (`* 0.5`, `* 0.65`, …). [`Spacing`] provides
//! the named spacing scale (xs/sm/md/lg/xl) so every overlay uses the same
//! rhythm and spacing scales with the font size.
//!
//! See `docs/v0.8_PLAN.md` §4.1 (stage 1 — layout infrastructure).

/// Axis-aligned rectangle in physical pixels: `[x0, y0, x1, y1]`.
/// Shared by renderer, Scene components and pointer hit testing.
pub type Rect = [f32; 4];

/// Layout context: the single source of truth for coordinate math in every
/// overlay builder. Constructed once at the top of `draw()` and threaded
/// through `build_*_vertices` calls.
///
/// All values are in **physical pixels** (already multiplied by the Retina
/// `scale`), matching the existing renderer convention.
#[derive(Clone, Copy, Debug)]
pub struct LayoutCtx {
    /// Physical-pixel viewport size `(width, height)`.
    pub viewport: (f32, f32),
    /// One cell width in physical pixels (advance of `'M'` in the active font).
    pub cell_w: f32,
    /// One cell height in physical pixels (`font_size × line_height × scale`).
    pub cell_h: f32,
    /// Content padding in physical pixels (logical config value × scale).
    pub padding_x: f32,
    pub padding_y: f32,
    /// v0.9 H1: Height of the tab bar at the top of the window (physical px).
    /// The content area starts below the tab bar + padding_y. 0 when no
    /// tab bar is drawn (single tab).
    pub chrome_top: f32,
    /// v0.9 W5: Width of the left sidebar (history panel in sidebar mode) in
    /// physical px. The content area starts to the right of the sidebar. 0 when
    /// the panel is closed.
    pub chrome_left: f32,
    /// Optional clip rectangle for nested overlays (children stay inside).
    /// `None` means "use the full content rect". Stored as `[x0, y0, x1, y1]`.
    pub clip: Option<Rect>,
}

#[allow(dead_code)] // methods adopted incrementally as overlays migrate
impl LayoutCtx {
    /// Build a top-level context (no clip) from the renderer's per-frame state.
    pub fn new(
        viewport: (f32, f32),
        cell_w: f32,
        cell_h: f32,
        padding_x: f32,
        padding_y: f32,
    ) -> Self {
        Self {
            viewport,
            cell_w,
            cell_h,
            padding_x,
            padding_y,
            chrome_top: 0.0,
            chrome_left: 0.0,
            clip: None,
        }
    }

    /// Left edge of the content area (= horizontal padding + chrome_left).
    #[inline]
    pub fn left(&self) -> f32 {
        self.padding_x + self.chrome_left
    }

    /// Right edge of the content area.
    #[inline]
    pub fn right(&self) -> f32 {
        self.viewport.0 - self.padding_x
    }

    /// Top edge of the content area (= tab bar + vertical padding).
    #[inline]
    pub fn top(&self) -> f32 {
        self.padding_y + self.chrome_top
    }

    /// Bottom edge of the content area.
    #[inline]
    pub fn bottom(&self) -> f32 {
        self.viewport.1 - self.padding_y
    }

    /// Content width (viewport minus 2× horizontal padding).
    #[inline]
    pub fn width(&self) -> f32 {
        self.right() - self.left()
    }

    /// Content height (viewport minus 2× vertical padding, minus tab bar).
    #[inline]
    pub fn height(&self) -> f32 {
        self.bottom() - self.top()
    }

    /// X coordinate of the left edge of column `col` (0-based).
    #[inline]
    pub fn col_x(&self, col: usize) -> f32 {
        self.left() + col as f32 * self.cell_w
    }

    /// Y coordinate of the top edge of row `row` (0-based).
    #[inline]
    pub fn row_y(&self, row: usize) -> f32 {
        self.top() + row as f32 * self.cell_h
    }

    /// Return a child context clipped to `rect` (coordinates stay absolute;
    /// the child's `clip` is the intersection of the parent's clip and `rect`).
    /// Builders use `clip` to short-circuit quads outside the visible region.
    pub fn child(&self, rect: Rect) -> Self {
        let child_clip = match self.clip {
            Some(parent) => [
                rect[0].max(parent[0]),
                rect[1].max(parent[1]),
                rect[2].min(parent[2]),
                rect[3].min(parent[3]),
            ],
            None => rect,
        };
        Self {
            clip: Some(child_clip),
            ..*self
        }
    }

    /// True if `rect` intersects the active clip (or the content area when
    /// no clip is set). Builders use this to skip fully occluded quads.
    pub fn is_visible(&self, rect: Rect) -> bool {
        let (x0, y0, x1, y1) = (rect[0], rect[1], rect[2], rect[3]);
        if x1 <= x0 || y1 <= y0 {
            return false;
        }
        match self.clip {
            Some([cx0, cy0, cx1, cy1]) => x1 > cx0 && x0 < cx1 && y1 > cy0 && y0 < cy1,
            None => true,
        }
    }
}

/// Named spacing scale. Every overlay pulls gaps/padding from these helpers
/// so spacing is uniform and scales with the font size (cell dimensions).
///
/// Conventions:
/// - `xs` / `sm` / `md` / `lg` / `xl` — horizontal (character-width based)
/// - `row_xs` / `row_sm` / `row_md` — vertical (line-height based)
///
/// Replace ad-hoc `* 0.5`, `* 0.65`, `* 0.3` with the closest token.
pub struct Spacing;

#[allow(dead_code)] // methods are adopted incrementally as overlays migrate
impl Spacing {
    /// Extra-small horizontal gap: 0.25 cell.
    #[inline]
    pub fn xs(ctx: &LayoutCtx) -> f32 {
        ctx.cell_w * 0.25
    }

    /// Small horizontal gap: 0.5 cell (replaces most `* 0.5`).
    #[inline]
    pub fn sm(ctx: &LayoutCtx) -> f32 {
        ctx.cell_w * 0.5
    }

    /// Medium horizontal gap: 1 cell (the default rhythm unit).
    #[inline]
    pub fn md(ctx: &LayoutCtx) -> f32 {
        ctx.cell_w
    }

    /// Large horizontal gap: 1.5 cells (replaces most `* 0.65` + slack).
    #[inline]
    pub fn lg(ctx: &LayoutCtx) -> f32 {
        ctx.cell_w * 1.5
    }

    /// Extra-large horizontal gap: 2 cells.
    #[inline]
    pub fn xl(ctx: &LayoutCtx) -> f32 {
        ctx.cell_w * 2.0
    }

    /// Extra-small vertical gap: 0.25 line.
    #[inline]
    pub fn row_xs(ctx: &LayoutCtx) -> f32 {
        ctx.cell_h * 0.25
    }

    /// Small vertical gap: 0.5 line.
    #[inline]
    pub fn row_sm(ctx: &LayoutCtx) -> f32 {
        ctx.cell_h * 0.5
    }

    /// Medium vertical gap: 1 line (block separator rhythm).
    #[inline]
    pub fn row_md(ctx: &LayoutCtx) -> f32 {
        ctx.cell_h
    }
}

// ── Overlay layouts (v0.8 stage 4 — U2) ────────────────────────────────
//
// Pure coordinate functions: given a `LayoutCtx` + the immutable inputs an
// overlay needs, compute the rectangles and column anchors the renderer
// should draw into. No `MetalRenderer`, no `push_quad`, no theme lookup —
// just math. The unit tests below assert coordinates without touching the
// GPU. Each `*Layout` is consumed by the matching `build_*_vertices` in
// `renderer.rs`; that function keeps responsibility for vertex building,
// theming, and text rasterization.

/// Layout for the completion popup. Mirrors the geometry computed inline by
/// `MetalRenderer::build_completion_vertices` (renderer.rs:1606). The
/// renderer reads these fields instead of recomputing them.
#[derive(Clone, Copy, Debug)]
pub struct CompletionLayout {
    /// Popup rectangle `[x0, y0, x1, y1]` in physical pixels.
    pub popup_rect: Rect,
    /// Left edge of the icon column (icons drawn from here).
    pub icon_x: f32,
    /// Left edge of the label column (text drawn from here).
    pub label_x: f32,
    /// Left edge of the suffix column ("File"/"Directory"/…).
    pub suffix_x: f32,
    /// Visible label width in character columns (after popup width clamp).
    pub label_cols: usize,
    /// Fixed suffix column width (10 cols, "Directory" is the longest).
    pub suffix_cols: usize,
    /// Inclusive start index into `matches` of the first visible row.
    pub start: usize,
    /// Exclusive end index into `matches` of the last visible row.
    pub end: usize,
}

/// Compute the visible-row window for a completion popup.
///
/// `anchor_y` is the popup's bottom edge in physical pixels (the prompt
/// input box's top edge). The popup grows upward, so the number of rows
/// that fit is limited by how much vertical space is above the anchor.
///
/// Returns `(start, end, shown)`. `start` is clamped so the `selected`
/// row stays visible: it scrolls only when `selected` falls outside the
/// window, then snaps to keep `selected` at the bottom row.
pub fn completion_window(
    anchor_y: f32,
    cell_h: f32,
    popup_max_rows: usize,
    selected: usize,
    items_count: usize,
) -> (usize, usize, usize) {
    // Empty input → empty window. Caller (`build_completion_vertices`)
    // already early-returns when `matches.is_empty()`, but be defensive.
    if items_count == 0 {
        return (0, 0, 0);
    }
    let avail_rows = ((anchor_y / cell_h).ceil() as usize).saturating_sub(1);
    let max_rows = popup_max_rows.min(avail_rows.max(1));
    let start = selected.saturating_sub(max_rows - 1);
    let end = (start + max_rows).min(items_count);
    (start, end, end - start)
}

/// Compute the full completion layout given a precomputed visible-window
/// slice and the widest visible label width.
///
/// The split mirrors the original inline logic: the renderer first computes
/// `(start, end, shown)` via [`completion_window`], then walks
/// `matches[start..end]` to find `max_label_cols`, then calls this function
/// to derive the popup rectangle and column anchors.
#[allow(clippy::too_many_arguments)]
pub fn layout_completion(
    ctx: &LayoutCtx,
    start: usize,
    end: usize,
    max_label_cols: usize,
    anchor_y: f32,
    box_x0: f32,
    popup_width_scale: f32,
) -> CompletionLayout {
    let cw = ctx.cell_w;
    let ch = ctx.cell_h;
    let vp_w = ctx.viewport.0;
    let padding_x = ctx.padding_x;

    let shown = end - start;
    // Floor at 10 so an empty (or short) visible slice still has a sane
    // label column — matches the original `.unwrap_or(10)` default.
    let max_label_cols = max_label_cols.max(10);
    let suffix_cols = 10usize;
    let gap_cols = 2usize;
    // popup_cols = left_pad(1) + icon(2) + label + gap(2) + suffix(10) + right_pad(1)
    let popup_cols = 1 + 2 + max_label_cols + gap_cols + suffix_cols + 1;
    let popup_max_cols = ((vp_w * popup_width_scale) / cw) as usize;
    let popup_cols = popup_cols.clamp(25, popup_max_cols.max(25));
    let popup_w = popup_cols as f32 * cw;
    let popup_x0 = box_x0;
    let popup_x1 = (popup_x0 + popup_w).min(vp_w - padding_x);

    let pad = Spacing::sm(ctx); // cw * 0.5
    let icon_w = 2.0 * cw;
    let label_x = popup_x0 + pad + icon_w;
    let label_cols = popup_cols
        .saturating_sub(1 + 2 + gap_cols + suffix_cols + 1)
        .max(5);

    let top_pad = Spacing::row_sm(ctx); // ch * 0.5
    let popup_h = shown as f32 * ch + top_pad;
    let popup_top = anchor_y - popup_h;

    let suffix_x = label_x + (max_label_cols.min(label_cols) + gap_cols) as f32 * cw;
    let icon_x = popup_x0 + pad;

    CompletionLayout {
        popup_rect: [popup_x0, popup_top, popup_x1, anchor_y],
        icon_x,
        label_x,
        suffix_x,
        label_cols,
        suffix_cols,
        start,
        end,
    }
}

// ── Command Palette ───────────────────────────────────────────────────
//
// The palette has two modes:
//   • search mode — query box at top, results list below
//   • form mode   — workflow variable-fill form (no results list)
// Both share the same horizontally-centered popup rect and the same
// vertical anchor (`vp_h * 0.15` from the top).

/// Layout for the palette's search mode. Mirrors the geometry computed
/// inline by `MetalRenderer::build_palette_vertices` (renderer.rs:1759)
/// when no workflow form is active.
#[derive(Clone, Copy, Debug)]
pub struct PaletteSearchLayout {
    /// Outer popup rect `[x0, y0, x1, y1]`.
    pub popup_rect: Rect,
    /// Y of the query/banner row (text baseline region).
    pub query_y: f32,
    /// Y of the separator below the query row.
    pub sep_y: f32,
    /// Y of the first results row (top edge).
    pub results_y: f32,
    /// X of the query/banner text left edge.
    pub query_x: f32,
    /// X of the result label column.
    pub label_x: f32,
    /// X of the right-aligned kind suffix column.
    pub suffix_x: f32,
    /// Visible results window (inclusive start, exclusive end). Visible row
    /// count = `end - start`.
    pub start: usize,
    pub end: usize,
}

/// Compute the centered popup X range for the palette (shared by search
/// and form modes). Returns `(x0, x1)`; the Y range depends on mode.
/// `popup_width_scale` is typically 0.6.
pub fn palette_popup_x_range(ctx: &LayoutCtx, popup_width_scale: f32) -> (f32, f32) {
    let vp_w = ctx.viewport.0;
    let popup_w = vp_w * popup_width_scale;
    let popup_x0 = (vp_w - popup_w) / 2.0;
    (popup_x0, popup_x0 + popup_w)
}

/// Compute the search-mode layout. `entries_len` is the total result count;
/// `selection` is the currently highlighted row; `popup_max_rows` clamps
/// the visible window (matches `MetalRenderer.popup_max_rows`).
#[allow(clippy::too_many_arguments)]
pub fn layout_palette_search(
    ctx: &LayoutCtx,
    entries_len: usize,
    selection: usize,
    popup_max_rows: usize,
    popup_width_scale: f32,
) -> PaletteSearchLayout {
    let cw = ctx.cell_w;
    let ch = ctx.cell_h;
    let vp_h = ctx.viewport.1;

    let (popup_x0, popup_x1) = palette_popup_x_range(ctx, popup_width_scale);
    let max_results = popup_max_rows.min(entries_len.max(1));
    let shown = max_results.min(entries_len);
    // +2 rows for header (query + separator) + 0.5ch top padding.
    let popup_h = (shown as f32 + 2.0) * ch + ch * 0.5;
    let popup_top = vp_h * 0.15;
    let popup_bottom = popup_top + popup_h;

    let query_y = popup_top + ch * 0.5;
    let sep_y = query_y + ch;
    let results_y = sep_y + ch;
    let query_x = popup_x0 + cw * 0.5;
    let label_x = popup_x0 + cw * 0.5;
    // Right-aligned suffix: 10 cols wide, 0.5cw margin from the right edge.
    let suffix_x = popup_x1 - cw * 0.5 - 10.0 * cw;

    let start = selection.saturating_sub(max_results.saturating_sub(1));
    let end = (start + max_results).min(entries_len);

    PaletteSearchLayout {
        popup_rect: [popup_x0, popup_top, popup_x1, popup_bottom],
        query_y,
        sep_y,
        results_y,
        query_x,
        label_x,
        suffix_x,
        start,
        end,
    }
}

/// Compute the form-mode popup rect for the palette's workflow
/// variable-fill sub-mode. `n_fields` is the number of form fields
/// (excludes the 3 chrome rows: title, separator, submit hint).
pub fn layout_palette_form_rect(ctx: &LayoutCtx, n_fields: usize, popup_width_scale: f32) -> Rect {
    let ch = ctx.cell_h;
    let vp_h = ctx.viewport.1;
    let (popup_x0, popup_x1) = palette_popup_x_range(ctx, popup_width_scale);
    let popup_top = vp_h * 0.15;
    let popup_h = (n_fields as f32 + 3.0) * ch + ch * 0.5;
    [popup_x0, popup_top, popup_x1, popup_top + popup_h]
}

// ── Tab strip ─────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug)]
pub struct TabStripInput {
    pub viewport_width: f32,
    pub bar_height: f32,
    pub cell_width: f32,
    pub padding_x: f32,
    pub chrome_left: f32,
    pub traffic_lights_width: f32,
    pub tab_count: usize,
    pub requested_scroll_offset: f32,
}

#[derive(Clone, Copy, Debug)]
pub struct TabStripLayout {
    pub bar_rect: Rect,
    pub tabs_start: f32,
    pub tab_width: f32,
    pub overflowing: bool,
    pub scroll_offset: f32,
    pub max_scroll: f32,
    pub visible_left: f32,
    pub visible_right: f32,
    pub arrow_width: f32,
    pub plus_width: f32,
    pub plus_rect: Rect,
    pub left_arrow_rect: Option<Rect>,
    pub right_arrow_rect: Option<Rect>,
}

impl TabStripLayout {
    pub fn tab_rect(self, index: usize) -> Rect {
        let x0 = self.tabs_start + index as f32 * self.tab_width - self.scroll_offset;
        [
            x0.clamp(self.visible_left, self.visible_right),
            self.bar_rect[1],
            (x0 + self.tab_width).clamp(self.visible_left, self.visible_right),
            self.bar_rect[3],
        ]
    }

    pub fn scroll_offset_for_tab(self, index: usize) -> f32 {
        if !self.overflowing {
            return 0.0;
        }
        let unscrolled_x0 = self.tabs_start + index as f32 * self.tab_width;
        let x0 = unscrolled_x0 - self.scroll_offset;
        let x1 = x0 + self.tab_width;
        let target = if x0 < self.visible_left {
            unscrolled_x0 - self.visible_left
        } else if x1 > self.visible_right {
            unscrolled_x0 + self.tab_width - self.visible_right
        } else {
            self.scroll_offset
        };
        target.clamp(0.0, self.max_scroll)
    }
}

pub fn layout_tab_strip(input: TabStripInput) -> TabStripLayout {
    let max_tab_width = input.cell_width * 20.0;
    let min_tab_width = input.cell_width * 15.0;
    let arrow_width = input.cell_width * 2.5;
    let plus_width = input.cell_width * 3.0;
    let right_padding = input.padding_x * 0.5;
    let traffic_offset = if input.chrome_left > 0.0 {
        0.0
    } else {
        input.traffic_lights_width
    };
    let tabs_start = input.chrome_left + traffic_offset + input.padding_x;
    let right_reserve = plus_width + right_padding;
    let available = (input.viewport_width - tabs_start - right_reserve).max(0.0);
    let count = input.tab_count as f32;
    let total_at_max = count * max_tab_width;
    let total_at_min = count * min_tab_width;
    let (tab_width, overflowing) = if input.tab_count == 0 || total_at_max <= available {
        (max_tab_width, false)
    } else if total_at_min <= available {
        (
            (available / count).clamp(min_tab_width, max_tab_width),
            false,
        )
    } else {
        (min_tab_width, true)
    };
    let total_tab_width = count * tab_width;
    let visible_left = if overflowing {
        tabs_start + arrow_width
    } else {
        tabs_start
    };
    let visible_right = if overflowing {
        input.viewport_width - right_reserve - arrow_width
    } else {
        input.viewport_width - right_reserve
    }
    .max(visible_left);
    let max_scroll = if overflowing {
        (total_tab_width - (visible_right - visible_left)).max(0.0)
    } else {
        0.0
    };
    let scroll_offset = input.requested_scroll_offset.clamp(0.0, max_scroll);
    let plus_x0 = if overflowing {
        visible_right + arrow_width
    } else {
        tabs_start + total_tab_width
    };
    let bar_rect = [0.0, 0.0, input.viewport_width, input.bar_height];
    TabStripLayout {
        bar_rect,
        tabs_start,
        tab_width,
        overflowing,
        scroll_offset,
        max_scroll,
        visible_left,
        visible_right,
        arrow_width,
        plus_width,
        plus_rect: [plus_x0, 0.0, plus_x0 + plus_width, input.bar_height],
        left_arrow_rect: overflowing.then_some([
            tabs_start,
            0.0,
            tabs_start + arrow_width,
            input.bar_height,
        ]),
        right_arrow_rect: overflowing.then_some([
            visible_right,
            0.0,
            visible_right + arrow_width,
            input.bar_height,
        ]),
    }
}

// ── Context menu ───────────────────────────────────────────────────────
//
// Right-click menu on a block. Fixed 3 items (Copy Command / Copy Output /
// Toggle Fold). Mirrors `MetalRenderer::build_context_menu_vertices`
// (renderer.rs:2027). The menu is anchored at the click point and
// clamped to the viewport's right edge.

/// Layout for the block context menu.
#[derive(Clone, Copy, Debug)]
pub struct ContextMenuLayout {
    /// Outer menu rect `[x0, y0, x1, y1]`.
    pub menu_rect: Rect,
    /// Per-item Y (top edge). Length = items.len().
    pub item_y: [f32; 4],
    /// Exact hit-test rectangles for the four actions.
    pub item_rects: [Rect; 4],
    /// X of the item label text.
    pub text_x: f32,
    /// Y of the separator after item `i` (None for the last item).
    pub separator_ys: [f32; 3],
}

/// Compute the context menu layout. `(x, y)` is the click anchor. The menu
/// is left-clamped so its right edge stays inside the viewport.
pub fn layout_context_menu(ctx: &LayoutCtx, x: f32, y: f32, scale: f32) -> ContextMenuLayout {
    let cw = ctx.cell_w;
    let ch = ctx.cell_h;
    let vp_w = ctx.viewport.0;
    let vp_h = ctx.viewport.1;

    let item_h = ch * 1.2;
    let menu_w = 180.0 * scale;
    let menu_h = 4.0 * item_h + ch * 0.4;

    // Clamp so the right edge stays inside the viewport (with 4px gutter).
    let menu_x0 = x.min(vp_w - menu_w - 4.0).max(0.0);
    // v0.9 fix: if the menu would extend below the viewport, flip it upward
    // so it opens above the click point instead of being clipped.
    let menu_y0 = if y + menu_h > vp_h - 4.0 {
        (y - menu_h).max(4.0)
    } else {
        y
    };
    let menu_x1 = menu_x0 + menu_w;
    let menu_y1 = menu_y0 + menu_h;

    let item_y = [
        menu_y0 + ch * 0.2,
        menu_y0 + ch * 0.2 + item_h,
        menu_y0 + ch * 0.2 + 2.0 * item_h,
        menu_y0 + ch * 0.2 + 3.0 * item_h,
    ];
    let separator_ys = [item_y[0] + item_h, item_y[1] + item_h, item_y[2] + item_h];
    let text_x = menu_x0 + cw * 0.4;
    let item_rects = item_y.map(|item_top| [menu_x0, item_top, menu_x1, item_top + item_h]);

    ContextMenuLayout {
        menu_rect: [menu_x0, menu_y0, menu_x1, menu_y1],
        item_y,
        item_rects,
        text_x,
        separator_ys,
    }
}

#[cfg(test)]
impl ContextMenuLayout {
    pub fn item_at(self, x: f32, y: f32) -> Option<usize> {
        self.item_rects.iter().position(|rect| {
            let [x0, y0, x1, y1] = *rect;
            x >= x0 && x < x1 && y >= y0 && y < y1
        })
    }
}

// ── Find utility bar ──────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FindLayout {
    pub popup_rect: Rect,
    pub line_y: f32,
    pub text_x0: f32,
    pub text_x1: f32,
    pub regex_x: f32,
    pub case_x: f32,
    pub down_x: f32,
    pub up_x: f32,
    pub regex_rect: Rect,
    pub case_rect: Rect,
    pub down_rect: Option<Rect>,
    pub up_rect: Option<Rect>,
}

pub fn layout_find(ctx: &LayoutCtx, total_matches: usize) -> FindLayout {
    let cw = ctx.cell_w;
    let ch = ctx.cell_h;
    let target_w = 500.0_f32;
    let right_margin = 20.0;
    let min_w = cw * 48.0;
    let popup_w = target_w.min(ctx.width() - right_margin - 20.0).max(min_w);
    let popup_h = (ch * 1.75).max(ch + 16.0);
    let popup_x1 = ctx.right() - right_margin;
    let popup_x0 = popup_x1 - popup_w;
    let popup_y0 = ctx.top() + 10.0;
    let popup_y1 = popup_y0 + popup_h;

    let border_w = 1.0;
    let stripe_w = 3.0;
    let inner_pad_x = 8.0;
    let text_x0 = popup_x0 + border_w + stripe_w + inner_pad_x;
    let text_x1 = popup_x1 - border_w - inner_pad_x;
    let line_y = popup_y0 + border_w + ((popup_h - border_w * 2.0 - ch) * 0.5).max(0.0);
    let click_pad = 2.0;
    let gap_w = cw;

    let text_width = |text: &str| unicode_width::UnicodeWidthStr::width_cjk(text) as f32 * cw;
    let regex_w = text_width(".*");
    let case_w = text_width("Aa");
    let down_w = text_width("↓");
    let up_w = text_width("↑");
    let regex_x = text_x1 - regex_w;
    let case_x = regex_x - gap_w - case_w;
    let down_x = case_x - gap_w - down_w;
    let up_x = down_x - gap_w - up_w;
    let button_rect = |x: f32, width: f32| {
        [
            x - click_pad,
            line_y - click_pad,
            x + width + click_pad,
            line_y + ch + click_pad,
        ]
    };
    FindLayout {
        popup_rect: [popup_x0, popup_y0, popup_x1, popup_y1],
        line_y,
        text_x0,
        text_x1,
        regex_x,
        case_x,
        down_x,
        up_x,
        regex_rect: button_rect(regex_x, regex_w),
        case_rect: button_rect(case_x, case_w),
        down_rect: (total_matches > 0).then(|| button_rect(down_x, down_w)),
        up_rect: (total_matches > 0).then(|| button_rect(up_x, up_w)),
    }
}

// ── Prompt (editor input box) ─────────────────────────────────────────
//
// The input box sits at the bottom of the viewport. Its height grows with
// the number of editor lines (1 pad row + N text rows + 1 pad row). The
// prompt glyph "❯ " occupies 2 columns on line 0 only — subsequent lines
// start at `left`. The caret X depends on the cumulative display width of
// the chars before the cursor (CJK chars are 2 cols wide), so the renderer
// computes `cursor_offset_cols` and passes it in.

/// Layout for the prompt input box. Mirrors the geometry computed inline
/// by `MetalRenderer::build_prompt_vertices` (renderer.rs:1370).
#[derive(Clone, Copy, Debug)]
pub struct PromptLayout {
    /// Outer box rect `[x0, y0, x1, y1]`.
    pub box_rect: Rect,
    /// Y of the first text row (one pad row below `box_y0`).
    pub text_y0: f32,
    /// X of the box's left edge (also where line 1+ starts).
    pub left: f32,
    /// Box width in character columns.
    pub box_cols: usize,
    /// X where line 0's text starts (after "❯ ", = `left + 2*cw`).
    pub first_line_text_x: f32,
    /// Caret X (top-left of the bar).
    pub cursor_x: f32,
    /// Caret Y (top edge).
    pub cursor_y: f32,
    /// Caret bar width in physical pixels.
    pub bar_w: f32,
    /// F2 P0-1: number of text rows visible in the clamped box. The renderer
    /// only draws lines `[scroll_offset, scroll_offset + visible_rows)`.
    pub visible_rows: usize,
    /// Physical height of one editor text row.
    pub row_height: f32,
}

/// Compute the prompt layout. `n_lines` is the editor buffer's line count
/// (clamped to ≥1). `cursor_line`/`cursor_offset_cols` locate the caret;
/// `cursor_offset_cols` is the cumulative display-column width of the
/// chars before the cursor on that line (CJK = 2 cols), pre-computed by
/// the renderer via `char_col_width`. `scroll_offset` is the editor
/// buffer's internal scroll offset (F2 P0-1) — cursor Y is adjusted so
/// the caret lands in the visible window.
pub fn layout_prompt(
    ctx: &LayoutCtx,
    n_lines: usize,
    cursor_line: usize,
    cursor_offset_cols: usize,
    scroll_offset: usize,
) -> PromptLayout {
    let cw = ctx.cell_w;
    let ch = ctx.cell_h;
    let vp_w = ctx.viewport.0;
    let vp_h = ctx.viewport.1;

    let n_lines = n_lines.max(1);
    // F2 P0-1: clamp the box to 30% of the viewport so multi-line input
    // scrolls internally instead of squeezing history content. The raw
    // height is 1 pad row + N text rows + 1 pad row.
    let raw_box_h = ch * (n_lines as f32 + 2.0);
    let max_box_h = vp_h * 0.30;
    let box_h = raw_box_h.min(max_box_h);
    // Visible text rows that fit in the clamped box (≥1).
    let visible_rows = (((box_h / ch).floor() - 2.0).max(1.0) as usize)
        .max(1)
        .min(n_lines);

    let box_y1 = (vp_h - ctx.padding_y).max(0.0);
    let box_y0 = (box_y1 - box_h).max(0.0);
    let box_x0 = ctx.left();
    let box_x1 = (vp_w - ctx.padding_x).max(box_x0);

    let text_y0 = box_y0 + ch;
    let left = box_x0;
    let box_cols = (((box_x1 - left) / cw).max(1.0)) as usize;

    let prompt_chars = 2usize; // "❯ "
    let first_line_text_x = left + prompt_chars as f32 * cw;

    // F2 P0-1: cursor Y accounts for the scroll offset so the caret lands
    // inside the visible window.
    let cursor_row_in_window = cursor_line.saturating_sub(scroll_offset) as f32;
    let cy = text_y0 + cursor_row_in_window * ch;
    let text_start_x = if cursor_line == 0 {
        first_line_text_x
    } else {
        left
    };
    let cx = text_start_x + cursor_offset_cols as f32 * cw;
    let bar_w = (cw * 0.12).max(2.0);

    PromptLayout {
        box_rect: [box_x0, box_y0, box_x1, box_y1],
        text_y0,
        left,
        box_cols,
        first_line_text_x,
        cursor_x: cx,
        cursor_y: cy,
        bar_w,
        visible_rows,
        row_height: ch,
    }
}

/// Map a physical Y coordinate to a document line in the currently visible
/// prompt window. Padding/hint-row clicks clamp to the nearest visible line.
pub fn prompt_line_at_y(
    layout: &PromptLayout,
    y: f32,
    scroll_offset: usize,
    n_lines: usize,
) -> usize {
    let n_lines = n_lines.max(1);
    let start = scroll_offset.min(n_lines - 1);
    let visible = layout.visible_rows.min(n_lines - start).max(1);
    let row =
        if layout.row_height.is_finite() && layout.row_height > 0.0 && layout.text_y0.is_finite() {
            ((y - layout.text_y0) / layout.row_height).floor() as isize
        } else {
            0
        };
    start + row.clamp(0, visible as isize - 1) as usize
}

/// Visible block rows above a prompt whose height is capped by
/// [`layout_prompt`]. This is shared by drawing and scroll clamping.
pub fn block_visible_rows(ctx: &LayoutCtx, prompt_lines: usize, cwd_header_active: bool) -> usize {
    if ctx.cell_h <= 0.0 || ctx.height() <= 0.0 {
        return 1;
    }
    let prompt = layout_prompt(ctx, prompt_lines, 0, 0, 0);
    let block = layout_block_view(ctx, prompt.box_rect[1], cwd_header_active);
    let height = (block.clip_bottom - ctx.top()).max(0.0);
    ((height / block.pitch).floor() as usize).max(1)
}

/// The fixed CWD band exists only while the owned editor prompt is visible.
/// A running command retains Terminal.cwd but renders a live block instead.
pub fn block_cwd_header_active(editor_mode: bool, cwd_present: bool) -> bool {
    editor_mode && cwd_present
}

// ── Block view (Warp-style history) ────────────────────────────────────
//
// Top-level geometry for the scrollable block-history region. Per-row Y
// coordinates are data-driven (each row's distance accumulates from the
// cumulative output line count of all blocks below it), so the renderer
// walks the block list and tracks `cursor_dist` itself. What we extract
// here is the *frame* geometry: pitch, left/right/cols, the fixed CWD
// line position (Editor mode), and the clip region.

/// Layout for the block view's outer frame. Mirrors the top-level geometry
/// computed inline by `MetalRenderer::build_block_view_vertices`
/// (renderer.rs:2126).
#[derive(Clone, Copy, Debug)]
pub struct BlockViewLayout {
    /// Row pitch in physical pixels (= ch * 1.1).
    pub pitch: f32,
    /// Left edge of the content area (= padding_x).
    pub left: f32,
    /// Right edge of the content area (= vp_w - padding_x).
    pub right: f32,
    /// Content width in character columns.
    pub cols: usize,
    /// Top of the clip region (viewport top padding).
    pub clip_top: f32,
    /// Bottom of the clip region (= content_bottom_y). Rows below this
    /// are clipped.
    pub clip_bottom: f32,
    /// Y of the fixed CWD header row. Only meaningful when
    /// `cwd_header_active == true` was passed to [`layout_block_view`].
    pub fixed_cwd_y: f32,
}

/// Compute the block view's frame layout. `region_bottom_y` is the bottom
/// edge of the block region (top of the input box in Editor mode, or the
/// screen bottom in CommandExecuting). `cwd_header_active` should be true
/// when a CWD line will be rendered (Editor mode + no live block).
pub fn layout_block_view(
    ctx: &LayoutCtx,
    region_bottom_y: f32,
    cwd_header_active: bool,
) -> BlockViewLayout {
    let cw = ctx.cell_w;
    let ch = ctx.cell_h;
    let vp_w = ctx.viewport.0;

    let pitch = ch * 1.1;
    let left = ctx.left();
    let right = vp_w - ctx.padding_x;
    let cols = (((right - left) / cw).max(1.0)) as usize;

    // v0.9 H1: clip_top must include chrome_top (tab bar height) so the
    // sticky header and scrollable content start below the tab bar. ctx.top()
    // = padding_y + chrome_top; when there's no tab bar, chrome_top is 0 and
    // this reduces to the old `ctx.padding_y`.
    let clip_top = ctx.top();
    let (content_bottom_y, fixed_cwd_y) = if cwd_header_active {
        // Editor mode: CWD line + divider pinned to the bottom; scrollable
        // content sits ABOVE the CWD line (2 pitches up: one for CWD text,
        // one for the separator that the renderer draws above it).
        (region_bottom_y - 2.0 * pitch, region_bottom_y - pitch)
    } else {
        // CommandExecuting (live block) or no CWD: scrollable content goes
        // all the way to region_bottom_y.
        (region_bottom_y, 0.0)
    };

    BlockViewLayout {
        pitch,
        left,
        right,
        cols,
        clip_top,
        clip_bottom: content_bottom_y,
        fixed_cwd_y,
    }
}

// ── Panel (history sidebar) layout ──────────────────────────────────

/// Layout product for the history sidebar. Shared between
/// `build_panel_vertices` (renderer) and `mouse_press_controller` so the
/// search-field and row geometry never drift apart.
///
/// All values are in physical pixels.
#[derive(Clone, Copy, Debug)]
pub struct PanelLayout {
    /// Full sidebar rect `[x0, y0, x1, y1]` (starts at chrome_top).
    pub panel_rect: Rect,
    /// Search input field rect (clickable → focus search).
    pub search_field_rect: Rect,
    /// Y of the first history row's top edge.
    pub list_top: f32,
    /// Per-row height (pitch).
    pub row_height: f32,
}

/// Compute panel geometry from a layout context + sidebar width.
///
/// Mirrors the constants in `build_panel_vertices`:
/// - header at `chrome_top + ch*0.4`
/// - search field at `chrome_top + ch*1.6`, height `ch*1.4`
/// - list starts at `field_y1 + ch*0.4`
/// - row height (pitch) = `ch*1.1`
pub fn layout_panel(
    chrome_top: f32,
    cell_w: f32,
    cell_h: f32,
    sidebar_width: f32,
    viewport_h: f32,
) -> PanelLayout {
    let panel_x = 0.0;
    let panel_rect = [panel_x, chrome_top, panel_x + sidebar_width, viewport_h];
    let field_pad_x = cell_w * 0.5;
    let field_pad_y = cell_h * 1.6;
    let field_h = cell_h * 1.4;
    let field_y0 = chrome_top + field_pad_y;
    let field_y1 = field_y0 + field_h;
    let search_field_rect = [
        panel_x + field_pad_x,
        field_y0,
        panel_x + sidebar_width - field_pad_x,
        field_y1,
    ];
    let list_top = field_y1 + cell_h * 0.4;
    let row_height = cell_h * 1.1;
    PanelLayout {
        panel_rect,
        search_field_rect,
        list_top,
        row_height,
    }
}

// ── Settings panel layout ───────────────────────────────────────────

/// Footer button hit rects for the settings panel.
#[derive(Clone, Copy, Debug, Default)]
pub struct FooterButtonRects {
    pub apply: Option<Rect>,
    pub close: Option<Rect>,
    pub save: Option<Rect>,
}

/// Layout product for the settings panel. Shared between
/// `build_settings_vertices` (renderer) and `mouse_press_controller` so the
/// sidebar/content/footer geometry never drifts apart.
#[derive(Clone, Debug)]
pub struct SettingsLayout {
    /// Full panel bounding box `[x0, y0, x1, y1]`.
    pub box_rect: Rect,
    /// F5: Sidebar area `[x0, y0, x1, y1]` where category list is rendered.
    /// In narrow drill-down content mode this is zero-sized (sidebar hidden).
    pub sidebar_rect: Rect,
    /// F5: Width of the sidebar column (0 when sidebar hidden).
    #[allow(dead_code)]
    pub sidebar_width: f32,
    /// F5: Top of the first sidebar category row.
    pub sidebar_top: f32,
    /// F5: True when the sidebar is visible (wide mode, or narrow sidebar view).
    pub show_sidebar: bool,
    /// F5: True when the content form is visible (wide mode, or narrow content view).
    pub show_content: bool,
    /// Legacy: tab bar Y baseline — kept for footer backward compat. F5
    /// repurposes this as the top of the sidebar/content area.
    #[allow(dead_code)]
    pub tab_bar_y: f32,
    /// Legacy: width of each tab slot — kept for backward compat. F5 doesn't
    /// use a horizontal tab bar; this is now the sidebar row pitch.
    #[allow(dead_code)]
    pub tab_width: f32,
    /// Content area X bounds (excludes sidebar when split).
    pub content_x0: f32,
    pub content_x1: f32,
    /// Top of the content area (after error banner if present).
    pub content_top: f32,
    /// Max visible rows in the content area.
    pub max_rows: usize,
    /// Footer Y baseline.
    pub footer_y: f32,
    /// Footer button rects (only clickable buttons; None if culled).
    pub footer_buttons: FooterButtonRects,
}

/// F5: Narrow-window breakpoint (logical points). Below this, the settings
/// panel switches to single-column drill-down mode.
pub const SETTINGS_NARROW_THRESHOLD: f32 = 640.0;

/// Compute settings panel geometry. Returns `None` when the viewport/cell
/// dimensions are degenerate (matching the renderer's guard clause).
///
/// F5: `is_narrow` switches to single-column drill-down (sidebar OR content).
/// `drill_down` is only meaningful when `is_narrow` is true: false = show
/// sidebar, true = show content. In wide mode both are always visible.
///
/// `footer_pair_widths` provides the measured widths of the 6 footer pairs
/// (key+desc) in physical pixels — the caller computes these via
/// `text_col_width`. Pairs 1 (apply), 4 (close), 5 (save) carry hit rects.
#[allow(clippy::too_many_arguments)]
pub fn layout_settings(
    vp_w: f32,
    vp_h: f32,
    cw: f32,
    ch: f32,
    _tab_count: usize,
    has_error: bool,
    footer_pair_widths: &[f32; 6],
    is_narrow: bool,
    drill_down: bool,
) -> Option<SettingsLayout> {
    if cw <= 0.0 || ch <= 0.0 || vp_w <= 0.0 || vp_h <= 0.0 {
        return None;
    }
    let box_w = vp_w * 0.72;
    let box_h = vp_h * 0.78;
    let box_x0 = (vp_w - box_w) / 2.0;
    let box_x1 = box_x0 + box_w;
    let box_y0 = (vp_h - box_h) / 2.0;
    let box_y1 = box_y0 + box_h;

    // F5: sidebar width — wide enough for "Keybindings" (11 chars) + padding.
    // Capped at 40% of box width so the content area stays usable.
    let sidebar_w = (cw * 14.0).max(box_w * 0.28).min(box_w * 0.40);

    // F5: visibility flags. Wide mode = both visible. Narrow mode = one at a time.
    let (show_sidebar, show_content) = if is_narrow {
        (!drill_down, drill_down)
    } else {
        (true, true)
    };

    let area_top = box_y0 + ch * 2.0;
    let footer_y = box_y1 - ch * 1.5;
    let content_bottom = footer_y - ch * 0.5;
    let content_base = area_top + ch * 0.5;
    let content_top = if has_error {
        content_base + ch
    } else {
        content_base
    };
    let content_h = (content_bottom - content_top).max(0.0);
    let max_rows = (content_h / ch).max(1.0) as usize;

    // F5: sidebar rect. In narrow mode the sidebar spans the full box width.
    let sidebar_rect = if show_sidebar {
        let sx1 = if is_narrow {
            box_x1
        } else {
            box_x0 + sidebar_w
        };
        [box_x0, area_top, sx1, content_bottom]
    } else {
        [0.0; 4]
    };

    // F5: content X bounds. In split mode content starts after sidebar.
    // In narrow sidebar-only mode, content_x0 == content_x1 (no content drawn).
    let pad_x = cw * 1.5;
    let (content_x0, content_x1) = if show_content {
        if is_narrow {
            // Narrow content mode: full box width.
            (box_x0 + pad_x, box_x1 - pad_x)
        } else {
            // Split mode: content starts after sidebar + separator.
            (box_x0 + sidebar_w + cw * 0.5, box_x1 - pad_x)
        }
    } else {
        // Content hidden — degenerate bounds.
        (box_x1, box_x1)
    };

    // Footer pair layout: left-to-right from content_x0, gap between pairs.
    // `footer_pair_widths` already includes the per-pair `inner` gap (key↔desc)
    // measured by the caller via text_col_width — we only add the inter-pair
    // `gap` here.
    let gap = cw * crate::settings_component::FOOTER_HINT_GAP_CELLS;
    let footer_start = if is_narrow {
        box_x0 + pad_x
    } else {
        content_x0
    };
    let footer_end = box_x1 - pad_x;
    let mut fx = footer_start;
    let mut apply = None;
    let mut close = None;
    let mut save = None;
    for (i, &pair_w) in footer_pair_widths.iter().enumerate() {
        if fx + pair_w > footer_end {
            break;
        }
        // Pairs: 0=navigate, 1=apply, 2=switch, 3=adjust, 4=close, 5=save
        match i {
            1 => apply = Some([fx, footer_y, fx + pair_w, footer_y + ch]),
            4 => close = Some([fx, footer_y, fx + pair_w, footer_y + ch]),
            5 => save = Some([fx, footer_y, fx + pair_w, footer_y + ch]),
            _ => {}
        }
        fx += pair_w + gap;
    }

    Some(SettingsLayout {
        box_rect: [box_x0, box_y0, box_x1, box_y1],
        sidebar_rect,
        sidebar_width: if show_sidebar {
            sidebar_rect[2] - sidebar_rect[0]
        } else {
            0.0
        },
        sidebar_top: area_top + ch * 0.5,
        show_sidebar,
        show_content,
        tab_bar_y: area_top,
        tab_width: ch, // repurposed as sidebar row pitch
        content_x0,
        content_x1,
        content_top,
        max_rows,
        footer_y,
        footer_buttons: FooterButtonRects { apply, close, save },
    })
}

#[cfg(test)]
#[path = "layout/tests.rs"]
mod tests;
