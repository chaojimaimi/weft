//! Mouse selection model.

use crate::grid::{CellFlags, Grid};

/// A point in the grid (row, col), 0-based.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GridPos {
    pub row: usize,
    pub col: usize,
}

impl GridPos {
    pub fn new(row: usize, col: usize) -> Self {
        Self { row, col }
    }
}

/// Selection mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SelectionMode {
    /// Character-level selection.
    Simple,
    /// Line-level selection (whole lines).
    Line,
    /// Block selection (rectangular).
    Block,
}

/// An active selection region.
#[derive(Clone, Debug)]
pub struct Selection {
    /// Start position (where the mouse was pressed).
    pub start: GridPos,
    /// End position (where the mouse was dragged to).
    pub end: GridPos,
    /// Selection mode.
    pub mode: SelectionMode,
}

impl Selection {
    pub fn new(start: GridPos, end: GridPos, mode: SelectionMode) -> Self {
        Self { start, end, mode }
    }

    /// Get the ordered (top-left to bottom-right) range.
    pub fn ordered(&self) -> (GridPos, GridPos) {
        if self.start.row < self.end.row
            || (self.start.row == self.end.row && self.start.col <= self.end.col)
        {
            (self.start, self.end)
        } else {
            (self.end, self.start)
        }
    }

    /// Check if a cell position is within the selection.
    pub fn contains(&self, row: usize, col: usize) -> bool {
        let (tl, br) = self.ordered();
        match self.mode {
            SelectionMode::Simple => {
                if row < tl.row || row > br.row {
                    return false;
                }
                if row == tl.row && col < tl.col {
                    return false;
                }
                if row == br.row && col > br.col {
                    return false;
                }
                true
            }
            SelectionMode::Line => row >= tl.row && row <= br.row,
            SelectionMode::Block => {
                row >= tl.row && row <= br.row && col >= tl.col && col <= br.col
            }
        }
    }

    /// Extract the selected text from the grid.
    pub fn text_from_grid(&self, grid: &Grid) -> String {
        let (tl, br) = self.ordered();
        // Clamp endpoints to valid grid bounds — a selection endpoint past the
        // right/bottom margin (col == num_cols / row == num_rows) would
        // otherwise index out of bounds below and panic on copy. (The app also
        // clamps in pixel_to_grid; this is defense in depth for any caller.)
        if grid.num_rows == 0 || grid.num_cols == 0 {
            return String::new();
        }
        let max_row = grid.num_rows - 1;
        let max_col = grid.num_cols - 1;
        let tl = GridPos::new(tl.row.min(max_row), tl.col.min(max_col));
        let br = GridPos::new(br.row.min(max_row), br.col.min(max_col));
        let mut result = String::new();

        match self.mode {
            SelectionMode::Simple | SelectionMode::Line => {
                for row in tl.row..=br.row {
                    let col_start = if row == tl.row { tl.col } else { 0 };
                    let col_end = if row == br.row {
                        br.col
                    } else {
                        grid.num_cols - 1
                    };

                    let mut last_char_col = 0;
                    for col in col_start..=col_end {
                        let cell = grid.cell(row, col);
                        if cell.flags.contains(CellFlags::WIDE_SPACER) {
                            continue;
                        }
                        if cell.character != ' ' {
                            last_char_col = col;
                        }
                    }

                    for col in col_start..=last_char_col {
                        let cell = grid.cell(row, col);
                        if cell.flags.contains(CellFlags::WIDE_SPACER) {
                            continue;
                        }
                        result.push(cell.character);
                    }

                    if row < br.row {
                        // Check if the row is wrapped
                        if row < grid.num_rows && !grid.viewport[row].wrapped {
                            result.push('\n');
                        }
                    }
                }
            }
            SelectionMode::Block => {
                for row in tl.row..=br.row {
                    for col in tl.col..=br.col {
                        let cell = grid.cell(row, col);
                        if !cell.flags.contains(CellFlags::WIDE_SPACER) {
                            result.push(cell.character);
                        }
                    }
                    if row < br.row {
                        result.push('\n');
                    }
                }
            }
        }

        result
    }
}

/// Mouse button for selection events.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
    Other,
}

/// Manages mouse selection state.
pub struct SelectionHandler {
    /// Currently active selection, if any.
    pub selection: Option<Selection>,
    /// Whether we're currently selecting (mouse held down).
    pub selecting: bool,
    /// The selection mode for the current drag.
    pub mode: SelectionMode,
}

impl SelectionHandler {
    pub fn new() -> Self {
        Self {
            selection: None,
            selecting: false,
            mode: SelectionMode::Simple,
        }
    }

    /// Start a new selection at the given position.
    pub fn start(&mut self, pos: GridPos, mode: SelectionMode) {
        self.selection = Some(Selection::new(pos, pos, mode));
        self.selecting = true;
        self.mode = mode;
    }

    /// Extend the selection to a new endpoint (during drag).
    pub fn extend(&mut self, pos: GridPos) {
        if let Some(sel) = &mut self.selection {
            sel.end = pos;
        }
    }

    /// Finish the selection (mouse released).
    pub fn end(&mut self) {
        self.selecting = false;
    }

    /// Clear the selection.
    pub fn clear(&mut self) {
        self.selection = None;
        self.selecting = false;
    }

    /// Get the selected text from the grid.
    pub fn selected_text(&self, grid: &Grid) -> Option<String> {
        self.selection.as_ref().map(|sel| sel.text_from_grid(grid))
    }
}

impl Default for SelectionHandler {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grid::Grid;

    fn filled_grid() -> Grid {
        let mut grid = Grid::new(5, 10);
        for row in 0..5 {
            for col in 0..10 {
                let ch = char::from_digit((row * 10 + col) as u32 % 36, 36).unwrap_or(' ');
                grid.viewport[row].cells[col].character = ch;
            }
        }
        grid
    }

    #[test]
    fn simple_selection_text() {
        let grid = filled_grid();
        let sel = Selection::new(
            GridPos::new(0, 0),
            GridPos::new(0, 4),
            SelectionMode::Simple,
        );
        let text = sel.text_from_grid(&grid);
        assert!(!text.is_empty());
    }

    #[test]
    fn out_of_bounds_end_does_not_panic() {
        // Regression: a drag-to-select ending at the window margin used to
        // produce col == num_cols / row == num_rows and panic text_from_grid
        // on Cmd+C. Endpoints are now clamped to valid bounds.
        let grid = filled_grid(); // 5 rows × 10 cols
        let sel = Selection::new(
            GridPos::new(0, 0),
            GridPos::new(99, 99), // far past the grid
            SelectionMode::Simple,
        );
        let text = sel.text_from_grid(&grid); // must not panic
        assert!(!text.is_empty(), "should still capture in-bounds content");

        let block = Selection::new(
            GridPos::new(0, 0),
            GridPos::new(99, 99),
            SelectionMode::Block,
        );
        let _ = block.text_from_grid(&grid); // must not panic
    }

    #[test]
    fn selection_ordered() {
        let sel = Selection::new(
            GridPos::new(3, 5),
            GridPos::new(1, 2),
            SelectionMode::Simple,
        );
        let (tl, br) = sel.ordered();
        assert_eq!(tl.row, 1);
        assert_eq!(tl.col, 2);
        assert_eq!(br.row, 3);
        assert_eq!(br.col, 5);
    }

    #[test]
    fn selection_contains() {
        let sel = Selection::new(
            GridPos::new(1, 2),
            GridPos::new(3, 5),
            SelectionMode::Simple,
        );
        assert!(sel.contains(1, 3));
        assert!(sel.contains(2, 0));
        assert!(!sel.contains(0, 0));
        assert!(!sel.contains(4, 0));
    }

    #[test]
    fn block_selection_contains() {
        let sel = Selection::new(GridPos::new(1, 2), GridPos::new(3, 5), SelectionMode::Block);
        assert!(sel.contains(2, 3));
        assert!(!sel.contains(2, 1));
        assert!(!sel.contains(2, 6));
    }

    #[test]
    fn handler_start_extend_end() {
        let mut handler = SelectionHandler::new();
        handler.start(GridPos::new(0, 0), SelectionMode::Simple);
        assert!(handler.selecting);
        handler.extend(GridPos::new(2, 5));
        handler.end();
        assert!(!handler.selecting);
        assert!(handler.selection.is_some());
    }

    #[test]
    fn handler_clear() {
        let mut handler = SelectionHandler::new();
        handler.start(GridPos::new(0, 0), SelectionMode::Simple);
        handler.clear();
        assert!(handler.selection.is_none());
        assert!(!handler.selecting);
    }
}
