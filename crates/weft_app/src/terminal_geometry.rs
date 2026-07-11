//! Shared terminal content geometry.
//!
//! The PTY row/column count must describe exactly the rectangle where Metal
//! draws grid cells. Native/titlebar chrome and content padding are outside
//! that rectangle; advertising rows underneath them makes TUI status/search
//! lines and cursors land below the visible viewport.

use crate::renderer::MetalRenderer;
use weft_core::grid::CursorStyle;
use winit::dpi::PhysicalSize;

#[derive(Clone, Copy, Debug)]
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
    pub fn dimensions(self) -> (usize, usize) {
        if self.cell_width <= 0.0 || self.cell_height <= 0.0 {
            return (0, 0);
        }
        let usable_width = (self.viewport_width - 2.0 * self.padding_x - self.chrome_left).max(0.0);
        let usable_height =
            (self.viewport_height - 2.0 * self.padding_y - self.chrome_top).max(0.0);
        let cols = (usable_width / self.cell_width).floor() as usize;
        let rows = (usable_height / self.cell_height).floor() as usize;
        (rows, cols)
    }

    #[cfg(test)]
    fn last_row_bottom(self) -> Option<f64> {
        let (rows, _) = self.dimensions();
        (rows > 0).then_some(self.padding_y + self.chrome_top + rows as f64 * self.cell_height)
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
    .dimensions()
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
    use super::{grid_cursor_visible, GridGeometry};
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
        assert!(geometry.last_row_bottom().unwrap() <= 790.0);

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
        assert!(two_x.last_row_bottom().unwrap() <= 1580.0);
    }
}
