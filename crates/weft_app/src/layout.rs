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
    /// v1.3: Per-pane origin offset `(x, y)` in physical pixels. Added to
    /// `left()` / `top()` so the grid and overlays render at the pane's
    /// split-tree-computed position. `(0.0, 0.0)` for single-pane tabs (the
    /// common case — no change from pre-v1.3 behavior).
    pub pane_origin: (f32, f32),
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
            pane_origin: (0.0, 0.0),
            clip: None,
        }
    }

    /// Left edge of the content area (= horizontal padding + chrome_left +
    /// pane_origin.x).
    #[inline]
    pub fn left(&self) -> f32 {
        self.padding_x + self.chrome_left + self.pane_origin.0
    }

    /// Right edge of the content area.
    ///
    /// When a clip rectangle is set (multi-pane mode: the active pane's
    /// split-tree rect), the right edge is clamped to the clip so pane-local
    /// overlays like the prompt and Find dialog stay inside the active pane
    /// instead of bleeding across the whole viewport.
    #[inline]
    pub fn right(&self) -> f32 {
        let base = self.viewport.0 - self.padding_x;
        match self.clip {
            Some([_, _, x1, _]) => x1.min(base),
            None => base,
        }
    }

    /// Top edge of the content area (= tab bar + vertical padding +
    /// pane_origin.y).
    #[inline]
    pub fn top(&self) -> f32 {
        self.padding_y + self.chrome_top + self.pane_origin.1
    }

    /// Bottom edge of the content area.
    ///
    /// Like [`right`](Self::right), this respects a clip rectangle so
    /// pane-local overlays are confined to the active pane in multi-pane tabs.
    #[inline]
    pub fn bottom(&self) -> f32 {
        let base = self.viewport.1 - self.padding_y;
        match self.clip {
            Some([_, _, _, y1]) => y1.min(base),
            None => base,
        }
    }

    /// Content width.
    ///
    /// In single-pane tabs this is `viewport - 2× horizontal padding`. In
    /// multi-pane tabs it is the active pane's width because [`right`](Self::right)
    /// is clipped to the active pane rect.
    #[inline]
    pub fn width(&self) -> f32 {
        self.right() - self.left()
    }

    /// Content height.
    ///
    /// In single-pane tabs this is `viewport - 2× vertical padding - tab bar`.
    /// In multi-pane tabs it is the active pane's height because
    /// [`bottom`](Self::bottom) is clipped to the active pane rect.
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

mod chrome;
mod settings;
mod surfaces;
mod terminal;

// Keep the original `layout::*` result-type paths available even when a
// result type is currently only named inside its owning submodule.
#[allow(unused_imports)]
pub use chrome::{
    layout_panel, layout_tab_strip, layout_tab_tooltip, PanelLayout, TabStripInput, TabStripLayout,
};
#[allow(unused_imports)]
pub use settings::{layout_settings, FooterButtonRects, SettingsLayout, SETTINGS_NARROW_THRESHOLD};
#[allow(unused_imports)]
pub use surfaces::{
    completion_window, layout_completion, layout_context_menu, layout_find,
    layout_palette_form_rect, layout_palette_search, palette_popup_x_range, CompletionLayout,
    ContextMenuLayout, FindLayout, PaletteSearchLayout,
};
#[allow(unused_imports)]
pub use terminal::{
    block_cwd_header_active, block_visible_rows, layout_block_view, layout_prompt,
    prompt_line_at_y, BlockViewLayout, PromptLayout,
};

#[cfg(test)]
#[path = "layout/tests.rs"]
mod tests;
