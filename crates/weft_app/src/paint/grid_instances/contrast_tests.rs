use super::{build_row_instances, GlyphInstance};
use weft_core::grid::{Cell, CellColor, Color, Cursor, CursorStyle, Grid};
use weft_core::selection::SelectionHandler;

#[test]
fn terminal_grid_applies_minimum_contrast_without_touching_background() {
    const BACKGROUND: [f32; 4] = [0.1, 0.1, 0.2, 1.0];
    let mut grid = Grid::new(1, 1);
    let mut cell = Cell::with_char('A');
    cell.fg = CellColor::Rgb(Color::rgb(80, 45, 20));
    grid.viewport[0].cells[0] = cell;
    let result = build_row_instances(
        &grid,
        &Color::standard_palette(),
        0,
        [0.8, 0.8, 0.8, 1.0],
        BACKGROUND,
        [1.0; 4],
        [0.3, 0.5, 0.7, 0.6],
        &Cursor::default(),
        CursorStyle::Block,
        false,
        &SelectionHandler::new(),
        1.0,
        7.0,
        10.0,
        20.0,
        0.0,
        0.0,
    );
    assert_eq!(result.bg_instances[0].bg, BACKGROUND);
    let GlyphInstance::Text { fg, .. } = &result.glyph_instances[0] else {
        panic!("expected text glyph");
    };
    assert!(crate::paint::primitives::text_contrast_ratio(*fg, BACKGROUND) >= 6.99);
    assert!(fg[0] > fg[1] && fg[1] > fg[2]);
}
