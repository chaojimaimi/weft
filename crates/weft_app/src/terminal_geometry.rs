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
        let (rows, cols) = if self.cell_width <= 0.0 || self.cell_height <= 0.0 {
            (0, 0)
        } else {
            (
                (content.height() / self.cell_height).floor() as usize,
                (content.width() / self.cell_width).floor() as usize,
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
    terminal_visible && !app_prompt_visible && (!style.is_blinking() || blink_on)
}

#[cfg(test)]
mod tests {
    use super::{grid_cursor_visible, GridGeometry, PhysicalRect};
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
        assert_eq!(geometry.dimensions(), (36, 98));
        assert!(geometry.layout().last_row_bottom().unwrap() <= 790.0);

        // The old full-window formula advertised 39 rows. Metal then drew
        // their bottoms at 846px, placing the final three rows off-screen.
        let old_rows = ((800.0_f64 - 20.0) / 20.0).floor() as usize;
        assert_eq!(old_rows, 39);
        assert!(10.0 + 56.0 + old_rows as f64 * 20.0 > 790.0);
    }

    #[test]
    fn sidebar_only_reduces_columns() {
        let base = geometry();
        let with_sidebar = GridGeometry {
            chrome_left: 240.0,
            ..base
        };
        assert_eq!(base.dimensions(), (36, 98));
        assert_eq!(with_sidebar.dimensions(), (36, 78));
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
}
