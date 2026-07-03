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
/// Matches the existing `[f32; 4]` convention used by `completion_popup_rect`
/// and `palette_popup_rect` in `MetalRenderer`.
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
            clip: None,
        }
    }

    /// Left edge of the content area (= horizontal padding).
    #[inline]
    pub fn left(&self) -> f32 {
        self.padding_x
    }

    /// Right edge of the content area.
    #[inline]
    pub fn right(&self) -> f32 {
        self.viewport.0 - self.padding_x
    }

    /// Top edge of the content area (= vertical padding).
    #[inline]
    pub fn top(&self) -> f32 {
        self.padding_y
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

    /// Content height (viewport minus 2× vertical padding).
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A typical 800×600 window at 2× scale with 14pt Menlo (~7.2×16.8 px
    /// cells) and 8px logical padding (16px physical).
    fn sample_ctx() -> LayoutCtx {
        LayoutCtx::new((1600.0, 1200.0), 7.2, 16.8, 16.0, 16.0)
    }

    // ── LayoutCtx edges ──────────────────────────────────────────────────

    #[test]
    fn edges_use_padding() {
        let ctx = sample_ctx();
        assert_eq!(ctx.left(), 16.0);
        assert_eq!(ctx.right(), 1600.0 - 16.0);
        assert_eq!(ctx.top(), 16.0);
        assert_eq!(ctx.bottom(), 1200.0 - 16.0);
    }

    #[test]
    fn width_and_height_exclude_padding() {
        let ctx = sample_ctx();
        assert_eq!(ctx.width(), 1600.0 - 32.0);
        assert_eq!(ctx.height(), 1200.0 - 32.0);
    }

    #[test]
    fn col_x_and_row_y_step_by_cell_size() {
        let ctx = sample_ctx();
        assert_eq!(ctx.col_x(0), 16.0);
        assert_eq!(ctx.col_x(1), 16.0 + 7.2);
        assert_eq!(ctx.col_x(10), 16.0 + 72.0);
        assert_eq!(ctx.row_y(0), 16.0);
        assert_eq!(ctx.row_y(2), 16.0 + 33.6);
    }

    // ── Clip / child / visibility ───────────────────────────────────────

    #[test]
    fn child_intersect_parent_clip() {
        let parent = sample_ctx().child([100.0, 100.0, 1000.0, 1000.0]);
        let clip = parent.clip.expect("parent has clip");
        assert_eq!(clip, [100.0, 100.0, 1000.0, 1000.0]);

        // Child inside parent → intersection = child rect.
        let child = parent.child([200.0, 200.0, 800.0, 800.0]);
        let cclip = child.clip.expect("child has clip");
        assert_eq!(cclip, [200.0, 200.0, 800.0, 800.0]);

        // Child partially outside parent → clamped to parent.
        let overflowing = parent.child([50.0, 50.0, 1200.0, 1200.0]);
        assert_eq!(overflowing.clip.unwrap(), [100.0, 100.0, 1000.0, 1000.0]);
    }

    #[test]
    fn is_visible_respects_clip() {
        let ctx = sample_ctx().child([100.0, 100.0, 500.0, 500.0]);
        assert!(ctx.is_visible([200.0, 200.0, 300.0, 300.0])); // inside
        assert!(!ctx.is_visible([600.0, 200.0, 700.0, 300.0])); // outside X
        assert!(ctx.is_visible([400.0, 400.0, 600.0, 600.0])); // overlapping
    }

    #[test]
    fn is_visible_without_clip_is_true() {
        let ctx = sample_ctx();
        assert!(ctx.is_visible([0.0, 0.0, 10.0, 10.0]));
    }

    #[test]
    fn is_visible_rejects_degenerate_rect() {
        let ctx = sample_ctx();
        assert!(!ctx.is_visible([10.0, 10.0, 10.0, 20.0])); // zero width
        assert!(!ctx.is_visible([10.0, 10.0, 20.0, 10.0])); // zero height
    }

    // ── Spacing tokens ──────────────────────────────────────────────────

    #[test]
    fn horizontal_spacing_scales_with_cell_w() {
        let ctx = sample_ctx(); // cell_w = 7.2
        assert_eq!(Spacing::xs(&ctx), 7.2 * 0.25);
        assert_eq!(Spacing::sm(&ctx), 7.2 * 0.5);
        assert_eq!(Spacing::md(&ctx), 7.2);
        assert_eq!(Spacing::lg(&ctx), 7.2 * 1.5);
        assert_eq!(Spacing::xl(&ctx), 7.2 * 2.0);
    }

    #[test]
    fn vertical_spacing_scales_with_cell_h() {
        let ctx = sample_ctx(); // cell_h = 16.8
        assert_eq!(Spacing::row_xs(&ctx), 16.8 * 0.25);
        assert_eq!(Spacing::row_sm(&ctx), 16.8 * 0.5);
        assert_eq!(Spacing::row_md(&ctx), 16.8);
    }

    #[test]
    fn spacing_scales_when_font_grows() {
        // Cmd+/- font zoom: cell dimensions change, spacing follows.
        let small = LayoutCtx::new((1600.0, 1200.0), 7.2, 16.8, 16.0, 16.0);
        let big = LayoutCtx::new((1600.0, 1200.0), 10.8, 25.2, 16.0, 16.0); // 1.5×
        assert!(Spacing::md(&big) > Spacing::md(&small));
        assert!(Spacing::row_md(&big) > Spacing::row_md(&small));
        // Ratio is preserved (1.5×).
        assert!((Spacing::md(&big) / Spacing::md(&small) - 1.5).abs() < 1e-5);
    }

    #[test]
    fn zero_padding_context() {
        // A borderless context (e.g. fullscreen alt-screen) is valid.
        let ctx = LayoutCtx::new((800.0, 600.0), 8.0, 16.0, 0.0, 0.0);
        assert_eq!(ctx.left(), 0.0);
        assert_eq!(ctx.right(), 800.0);
        assert_eq!(ctx.width(), 800.0);
    }
}
