#![cfg(test)]

use super::{build_row_instances, GlyphInstance};
use weft_core::grid::{Cell, CellFlags, Color, Cursor, CursorStyle, Grid};
use weft_core::selection::SelectionHandler;

// ── v1.10.12: SGR bold/italic styles reach the Text instance ──────────
//
// The grid path maps each cell's BOLD/ITALIC flags to a GlyphStyle variant
// so the UV resolver can sample the bold/italic atlas face. This test pins
// that mapping end-to-end from cell flags to the emitted instance.

fn build_row(cells: &[Cell]) -> super::GridRowInstances {
    let cols = cells.len().max(1);
    let mut grid = Grid::new(1, cols);
    for (i, cell) in cells.iter().enumerate() {
        grid.viewport[0].cells[i] = cell.clone();
    }
    build_row_instances(
        &grid,
        &Color::standard_palette(),
        0,
        [0.8, 0.8, 0.8, 1.0],
        [0.1, 0.1, 0.2, 1.0],
        [1.0; 4],
        [0.3, 0.5, 0.7, 0.6],
        [0.22, 0.34, 0.50, 1.0],
        &Cursor::default(),
        CursorStyle::Block,
        false,
        &SelectionHandler::new(),
        1.0,
        1.0,
        10.0,
        20.0,
        0.0,
        0.0,
    )
}

#[test]
fn sgr_style_flags_map_to_glyph_style_variant() {
    use crate::glyph::GlyphStyle;
    // 'b' bold, 'i' italic, 'x' bold-italic, 'n' plain.
    let mut cells = vec![
        Cell::with_char('b'),
        Cell::with_char('i'),
        Cell::with_char('x'),
        Cell::with_char('n'),
    ];
    cells[0].flags |= CellFlags::BOLD;
    cells[1].flags |= CellFlags::ITALIC;
    cells[2].flags |= CellFlags::BOLD | CellFlags::ITALIC;
    let result = build_row(&cells);
    let styles: Vec<_> = result
        .glyph_instances
        .iter()
        .map(|gi| match gi {
            GlyphInstance::Text { style, .. } => *style,
            other => panic!("expected Text, got {other:?}"),
        })
        .collect();
    assert_eq!(
        styles,
        [
            GlyphStyle::BOLD,
            GlyphStyle::ITALIC,
            GlyphStyle::BOLD_ITALIC,
            GlyphStyle::REGULAR
        ]
    );
}
