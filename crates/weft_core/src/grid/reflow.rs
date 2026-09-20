//! Dimension-only resize helpers for [`Grid::resize_dims`](super::Grid::resize_dims).
//!
//! T5: the reflow prescan/row-pool machinery that used to live here died
//! with the legacy Phase-1..4 reflow — the D4 protocol (push viewport →
//! `Index::rebuild` → pop viewport) needs none of it. Only the
//! dimension-only helper below survives (alt/TUI-owned resizes keep it).

use super::Cell;

/// Resize a single row's cell vector to `new_cols` in place: truncate if
/// narrower, pad with default (blank) cells if wider. No content is moved
/// between rows — this preserves the app's per-cell layout exactly, which is
/// the point of the dimension-only alt-screen resize.
///
/// (Moved from grid/mod.rs: T2 added the flat window fields against a file
/// already at its line ceiling; this self-contained helper is the
/// compensating move.)
pub(super) fn resize_row_cells(cells: &mut Vec<Cell>, new_cols: usize) {
    if cells.len() == new_cols {
        return;
    }
    if cells.len() > new_cols {
        cells.truncate(new_cols);
    } else {
        let extra = new_cols - cells.len();
        cells.extend(std::iter::repeat_with(Cell::default).take(extra));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resize_row_cells_truncates_and_pads_without_moving_content() {
        let mut cells = vec![Cell::default(); 3];
        cells[0].character = 'a';
        resize_row_cells(&mut cells, 2);
        assert_eq!(cells.len(), 2);
        assert_eq!(cells[0].character, 'a');

        resize_row_cells(&mut cells, 5);
        assert_eq!(cells.len(), 5);
        assert_eq!(cells[0].character, 'a', "content keeps its cell");
        assert_eq!(cells[4].character, ' ', "padded with blanks");
    }
}
