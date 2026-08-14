//! Shared terminal content geometry.
//!
//! The PTY row/column count must describe exactly the rectangle where Metal
//! draws grid cells. Native/titlebar chrome and content padding are outside
//! that rectangle; advertising rows underneath them makes TUI status/search
//! lines and cursors land below the visible viewport.

use crate::renderer::MetalRenderer;
use weft_core::grid::CursorStyle;
use winit::dpi::PhysicalSize;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PhysicalRect {
    pub left: f64,
    pub top: f64,
    pub right: f64,
    pub bottom: f64,
}

impl PhysicalRect {
    pub fn width(self) -> f64 {
        (self.right - self.left).max(0.0)
    }

    pub fn height(self) -> f64 {
        (self.bottom - self.top).max(0.0)
    }
}

/// The single layout product shared by PTY sizing, Metal placement and input
/// hit-testing. All values are physical pixels.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TerminalLayout {
    pub viewport: PhysicalRect,
    pub content: PhysicalRect,
    pub sidebar: Option<PhysicalRect>,
    pub cell_width: f64,
    pub cell_height: f64,
    pub padding_x: f64,
    pub padding_y: f64,
    pub chrome_top: f64,
    pub chrome_left: f64,
    pub rows: usize,
    pub cols: usize,
}

impl TerminalLayout {
    pub fn dimensions(self) -> (usize, usize) {
        (self.rows, self.cols)
    }

    /// Build the per-frame overlay context from this exact layout product.
    /// Resize handlers use this before the next draw, so they never clamp
    /// scroll state against a stale viewport from the previous frame.
    pub fn layout_ctx(self) -> crate::layout::LayoutCtx {
        let mut ctx = crate::layout::LayoutCtx::new(
            (self.viewport.right as f32, self.viewport.bottom as f32),
            self.cell_width as f32,
            self.cell_height as f32,
            self.padding_x as f32,
            self.padding_y as f32,
        );
        ctx.chrome_top = self.chrome_top as f32;
        ctx.chrome_left = self.chrome_left as f32;
        ctx
    }

    /// Whether a pointer is inside the PTY-owned cell rectangle. Chrome,
    /// padding and sidebars are excluded so their coordinates cannot clamp
    /// into Grid row/column zero.
    pub fn contains_content(self, x: f64, y: f64) -> bool {
        x >= self.content.left
            && x < self.content.right
            && y >= self.content.top
            && y < self.content.bottom
    }

    /// Map a physical pixel to a valid cell, clamped to the supplied live Grid
    /// dimensions. Supplying the live size keeps selection safe during the
    /// short interval between a window event and the corresponding SIGWINCH.
    pub fn grid_position(
        self,
        x: f64,
        y: f64,
        live_rows: usize,
        live_cols: usize,
    ) -> (usize, usize) {
        if self.cell_width <= 0.0 || self.cell_height <= 0.0 {
            return (0, 0);
        }
        let rows = live_rows.max(1);
        let cols = live_cols.max(1);
        let col =
            (((x - self.content.left) / self.cell_width).floor().max(0.0) as usize).min(cols - 1);
        let row =
            (((y - self.content.top) / self.cell_height).floor().max(0.0) as usize).min(rows - 1);
        (row, col)
    }

    #[cfg(test)]
    fn last_row_bottom(self) -> Option<f64> {
        (self.rows > 0).then_some(self.content.top + self.rows as f64 * self.cell_height)
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GridGeometry {
    pub viewport_width: f64,
    pub viewport_height: f64,
    pub cell_width: f64,
    pub cell_height: f64,
    pub padding_x: f64,
    pub padding_y: f64,
    pub chrome_top: f64,
    pub chrome_left: f64,
}

impl GridGeometry {
    pub fn layout(self) -> TerminalLayout {
        let viewport = PhysicalRect {
            left: 0.0,
            top: 0.0,
            right: self.viewport_width.max(0.0),
            bottom: self.viewport_height.max(0.0),
        };
        let content = PhysicalRect {
            left: self.padding_x + self.chrome_left,
            top: self.padding_y + self.chrome_top,
            right: (self.viewport_width - self.padding_x).max(self.padding_x + self.chrome_left),
            bottom: (self.viewport_height - self.padding_y).max(self.padding_y + self.chrome_top),
        };
        let sidebar = (self.chrome_left > 0.0).then_some(PhysicalRect {
            left: 0.0,
            top: self.chrome_top,
            right: self.chrome_left,
            bottom: self.viewport_height.max(self.chrome_top),
        });
        let (rows, cols) =
            if self.cell_width <= 0.0 || self.cell_height <= 0.0 || content.width() <= 0.0 {
                (0, 0)
            } else {
                (
                    (content.height() / self.cell_height).floor() as usize,
                    crate::layout::terminal_content_cols(
                        content.width() as f32,
                        self.cell_width as f32,
                    ),
                )
            };
        TerminalLayout {
            viewport,
            content,
            sidebar,
            cell_width: self.cell_width,
            cell_height: self.cell_height,
            padding_x: self.padding_x,
            padding_y: self.padding_y,
            chrome_top: self.chrome_top,
            chrome_left: self.chrome_left,
            rows,
            cols,
        }
    }

    pub fn dimensions(self) -> (usize, usize) {
        self.layout().dimensions()
    }
}

/// Derive PTY/Grid dimensions from the exact renderer geometry used to place
/// cells. Keeping this adapter here prevents startup, live resize and config
/// resize paths from silently choosing different chrome reservations.
pub fn dimensions_for_renderer(
    renderer: &MetalRenderer,
    viewport: PhysicalSize<u32>,
    chrome_left: f64,
) -> (usize, usize) {
    geometry_for_renderer(renderer, viewport, chrome_left).dimensions()
}

pub fn terminal_layout_for_renderer(
    renderer: &MetalRenderer,
    viewport: PhysicalSize<u32>,
    chrome_left: f64,
) -> TerminalLayout {
    geometry_for_renderer(renderer, viewport, chrome_left).layout()
}

fn geometry_for_renderer(
    renderer: &MetalRenderer,
    viewport: PhysicalSize<u32>,
    chrome_left: f64,
) -> GridGeometry {
    GridGeometry {
        viewport_width: viewport.width as f64,
        viewport_height: viewport.height as f64,
        cell_width: renderer.cell_width() as f64,
        cell_height: renderer.cell_height() as f64,
        padding_x: renderer.padding_x() as f64,
        padding_y: renderer.padding_y() as f64,
        chrome_top: renderer.tab_bar_height().max(renderer.titlebar_height()) as f64,
        chrome_left,
    }
}

/// v1.10.20: x-origin of grid hit-testing for the active pane, mirroring the
/// render policy's grid content origin (`paint::grid::grid_content_origin_x`,
/// `GridViewPolicy::inset_block_gutter`, renderer.rs:762): a primary-screen
/// TUI grid view is inset by the BlockView gutter, so pointer → cell mapping
/// must use the same origin or clicks/selection land ~1.5 cols right of the
/// visible character. Alt-screen TUIs paint edge-to-edge (`inset_block_gutter
/// = false`) — the origin is the pane's left edge, exactly as before.
pub fn grid_hit_origin_x(pane_ctx: &crate::layout::LayoutCtx, inset_block_gutter: bool) -> f64 {
    if inset_block_gutter {
        crate::layout::block_content_x_bounds(pane_ctx).0 as f64
    } else {
        pane_ctx.left() as f64
    }
}

/// Decide whether the terminal cursor should be drawn in this frame. Steady
/// cursor styles remain visible across blink phases; only the three blinking
/// styles follow the blink timer. Application-owned prompts intentionally hide
/// the grid cursor while their own caret is active.
pub fn grid_cursor_visible(
    style: CursorStyle,
    terminal_visible: bool,
    blink_on: bool,
    app_prompt_visible: bool,
) -> bool {
    // v1.10.6: `app_prompt_visible` covers the IME preedit overlay. When a
    // TUI is actively composing (preedit active) but hid the terminal cursor
    // (`?25l` — pi/openclaw manage their own cursor), force the cursor bar
    // ON so the user sees where input lands. The preedit overlay draws at
    // the grid cursor; hiding the caret underneath it makes the input
    // position invisible. (vim/less never trigger this — they show their
    // own cursor, so `app_prompt_visible` stays false for them.)
    let normal = terminal_visible && !app_prompt_visible && (!style.is_blinking() || blink_on);
    let composing_override = app_prompt_visible && !terminal_visible;
    normal || composing_override
}

#[cfg(test)]
mod tests {
    use super::{grid_cursor_visible, grid_hit_origin_x, GridGeometry, PhysicalRect};
    use weft_core::grid::CursorStyle;

    fn geometry() -> GridGeometry {
        GridGeometry {
            viewport_width: 1200.0,
            viewport_height: 800.0,
            cell_width: 12.0,
            cell_height: 20.0,
            padding_x: 10.0,
            padding_y: 10.0,
            chrome_top: 56.0,
            chrome_left: 0.0,
        }
    }

    #[test]
    fn terminal_rows_exclude_top_chrome_and_both_paddings() {
        let geometry = geometry();
        // cols = terminal_content_cols(1180, 12) = 95 (raw 98 minus 3 gutter)
        assert_eq!(geometry.dimensions(), (36, 95));
        assert!(geometry.layout().last_row_bottom().unwrap() <= 790.0);

        // The old full-window formula advertised 39 rows. Metal then drew
        // their bottoms at 846px, placing the final three rows off-screen.
        let old_rows = ((800.0_f64 - 20.0) / 20.0).floor() as usize;
        assert_eq!(old_rows, 39);
        assert!(10.0 + 56.0 + old_rows as f64 * 20.0 > 790.0);
    }

    #[test]
    fn overlay_context_is_derived_from_the_same_layout_product() {
        let layout = GridGeometry {
            chrome_left: 240.0,
            ..geometry()
        }
        .layout();
        let ctx = layout.layout_ctx();
        assert_eq!(ctx.viewport, (1200.0, 800.0));
        assert_eq!(ctx.cell_w, 12.0);
        assert_eq!(ctx.cell_h, 20.0);
        assert_eq!(ctx.chrome_top, 56.0);
        assert_eq!(ctx.chrome_left, 240.0);
        assert_eq!(ctx.left() as f64, layout.content.left);
        assert_eq!(ctx.top() as f64, layout.content.top);
    }

    #[test]
    fn sidebar_only_reduces_columns() {
        let base = geometry();
        let with_sidebar = GridGeometry {
            chrome_left: 240.0,
            ..base
        };
        // cols = terminal_content_cols(content_width, 12) — gutter subtracts 3 cols
        assert_eq!(base.dimensions(), (36, 95));
        assert_eq!(with_sidebar.dimensions(), (36, 75));
        assert_eq!(
            with_sidebar.layout().sidebar,
            Some(PhysicalRect {
                left: 0.0,
                top: 56.0,
                right: 240.0,
                bottom: 800.0,
            })
        );
    }

    #[test]
    fn degenerate_geometry_returns_zero_without_underflow() {
        assert_eq!(
            GridGeometry {
                viewport_width: 20.0,
                viewport_height: 20.0,
                cell_width: 12.0,
                cell_height: 20.0,
                padding_x: 20.0,
                padding_y: 20.0,
                chrome_top: 56.0,
                chrome_left: 240.0,
            }
            .dimensions(),
            (0, 0)
        );
    }

    #[test]
    fn only_blinking_cursor_styles_follow_the_blink_phase() {
        assert!(grid_cursor_visible(CursorStyle::Block, true, false, false));
        assert!(grid_cursor_visible(CursorStyle::Bar, true, false, false));
        assert!(grid_cursor_visible(
            CursorStyle::Underline,
            true,
            false,
            false
        ));
        assert!(!grid_cursor_visible(
            CursorStyle::BlinkingBlock,
            true,
            false,
            false
        ));
        assert!(grid_cursor_visible(
            CursorStyle::BlinkingBlock,
            true,
            true,
            false
        ));
        assert!(!grid_cursor_visible(CursorStyle::Block, false, true, false));
        assert!(!grid_cursor_visible(CursorStyle::Block, true, true, true));
    }

    #[test]
    fn app_owned_tui_preedit_suppresses_the_grid_cursor_until_cleared() {
        assert!(!grid_cursor_visible(CursorStyle::Bar, true, true, true));
        assert!(grid_cursor_visible(CursorStyle::Bar, true, true, false));
    }

    #[test]
    fn equivalent_logical_geometry_is_stable_across_scale_factors() {
        let one_x = geometry();
        let two_x = GridGeometry {
            viewport_width: one_x.viewport_width * 2.0,
            viewport_height: one_x.viewport_height * 2.0,
            cell_width: one_x.cell_width * 2.0,
            cell_height: one_x.cell_height * 2.0,
            padding_x: one_x.padding_x * 2.0,
            padding_y: one_x.padding_y * 2.0,
            chrome_top: one_x.chrome_top * 2.0,
            chrome_left: one_x.chrome_left * 2.0,
        };
        assert_eq!(one_x.dimensions(), two_x.dimensions());
        assert!(two_x.layout().last_row_bottom().unwrap() <= 1580.0);
    }

    #[test]
    fn content_rect_and_hit_testing_share_the_same_origin() {
        let layout = geometry().layout();
        assert_eq!(
            layout.content,
            PhysicalRect {
                left: 10.0,
                top: 66.0,
                right: 1190.0,
                bottom: 790.0,
            }
        );
        assert_eq!(layout.grid_position(10.0, 66.0, 36, 98), (0, 0));
        assert_eq!(layout.grid_position(22.0, 86.0, 36, 98), (1, 1));
        assert_eq!(layout.grid_position(5000.0, 5000.0, 36, 98), (35, 97));
    }

    // ── v1.10.20: grid hit origin ↔ grid render origin alignment ─────────

    fn hit_origin_ctx() -> crate::layout::LayoutCtx {
        crate::layout::LayoutCtx {
            viewport: (1080.0, 720.0),
            cell_w: 10.0,
            cell_h: 20.0,
            padding_x: 8.0,
            padding_y: 8.0,
            chrome_top: 28.0,
            chrome_left: 60.0,
            pane_origin: (0.0, 0.0),
            clip: None,
        }
    }

    /// The pointer → cell origin must coincide with the render origin
    /// (`grid_content_origin_x`): a primary TUI click must hit the character
    /// that is actually drawn under the cursor (gutter inset), and an
    /// alt-screen TUI must keep the pane edge.
    #[test]
    fn grid_hit_origin_matches_render_content_origin_at_same_geometry() {
        let ctx = hit_origin_ctx();
        let render_inset = crate::paint::grid::grid_content_origin_x(&ctx, true);
        let render_plain = crate::paint::grid::grid_content_origin_x(&ctx, false);
        assert_eq!(
            grid_hit_origin_x(&ctx, true) as f32,
            render_inset,
            "inset hit origin == inset render origin == BlockView content left"
        );
        assert_eq!(
            grid_hit_origin_x(&ctx, false) as f32,
            render_plain,
            "edge-to-edge hit origin == render origin == pane left"
        );
        // The inset is exactly the BlockView gutter (1.5 cells at this size).
        assert!(
            (grid_hit_origin_x(&ctx, true) - ctx.left() as f64 - ctx.cell_w as f64 * 1.5).abs()
                < 1e-6
        );
        assert_eq!(grid_hit_origin_x(&ctx, false), ctx.left() as f64);
    }

    /// The same alignment must hold for a split-pane context (pane_origin +
    /// clip): grid hit-testing is pane-local, like the render origin.
    #[test]
    fn grid_hit_origin_alignment_holds_for_pane_local_context() {
        let ctx = hit_origin_ctx().for_pane([100.0, 100.0, 600.0, 500.0]);
        assert_eq!(
            grid_hit_origin_x(&ctx, true) as f32,
            crate::paint::grid::grid_content_origin_x(&ctx, true)
        );
        assert_eq!(grid_hit_origin_x(&ctx, false), ctx.left() as f64);
        // And the inset origin is the pane's own left edge + gutter.
        assert!(
            (grid_hit_origin_x(&ctx, true) - ctx.left() as f64 - ctx.cell_w as f64 * 1.5).abs()
                < 1e-6
        );
    }
}
