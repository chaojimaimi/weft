//! Mouse selection model.

use crate::grid::{CellFlags, Grid};

mod block_view;

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
    ///
    /// v1.6.0: `CellFlags::EXTRA` cells contribute their full grapheme cluster
    /// via [`Grid::grapheme_at`].
    pub fn text_from_grid(&self, grid: &Grid) -> String {
        let (tl, br) = self.ordered();
        // Clamp endpoints to valid bounds — a margin endpoint would otherwise
        // panic on copy (the app also clamps in pixel_to_grid).
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
                        if cell.flags.contains(CellFlags::EXTRA) {
                            if let Some(cluster) = grid.grapheme_at(row, col) {
                                result.push_str(cluster);
                                continue;
                            }
                        }
                        result.push(cell.character);
                    }

                    if row < br.row && row < grid.num_rows && !grid.viewport[row].wrapped {
                        result.push('\n');
                    }
                }
            }
            SelectionMode::Block => {
                for row in tl.row..=br.row {
                    for col in tl.col..=br.col {
                        let cell = grid.cell(row, col);
                        if cell.flags.contains(CellFlags::WIDE_SPACER) {
                            continue;
                        }
                        if cell.flags.contains(CellFlags::EXTRA) {
                            if let Some(cluster) = grid.grapheme_at(row, col) {
                                result.push_str(cluster);
                                continue;
                            }
                        }
                        result.push(cell.character);
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
    /// Block-view selection (Warp-style view); separate from `selection`.
    pub block_view_selection: Option<BlockViewSelection>,
}

impl SelectionHandler {
    pub fn new() -> Self {
        Self {
            selection: None,
            selecting: false,
            mode: SelectionMode::Simple,
            block_view_selection: None,
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
        self.block_view_selection = None;
    }

    /// v1.10.20: drop only the grid selection. `clear()` also drops the
    /// block-view selection; the migration path (grid selection → primary
    /// history BlockView selection) replaces just the grid half so the
    /// fresh block selection survives.
    pub fn clear_grid_selection(&mut self) {
        self.selection = None;
    }

    /// Get the selected text from the grid.
    pub fn selected_text(&self, grid: &Grid) -> Option<String> {
        self.selection.as_ref().map(|sel| sel.text_from_grid(grid))
    }

    // ── Block-view selection helpers ──────────────────────────────────

    /// Start a block-view selection at `pos`, capturing the current row
    /// snapshot (owned by the renderer) so subsequent text extraction is
    /// consistent with what the user saw at drag start.
    pub fn start_block_view(&mut self, pos: BlockViewPos, rows: Vec<BlockViewRow>) {
        self.block_view_selection = Some(BlockViewSelection {
            start: pos,
            end: pos,
            rows,
        });
        self.selecting = true;
    }

    /// Extend the active block-view selection endpoint (during drag).
    pub fn extend_block_view(&mut self, pos: BlockViewPos) {
        if let Some(sel) = &mut self.block_view_selection {
            sel.end = pos;
        }
    }

    /// Text from the active block-view selection, if any.
    pub fn block_view_text(&self) -> Option<String> {
        self.block_view_selection.as_ref().map(|sel| sel.text())
    }
}

impl Default for SelectionHandler {
    fn default() -> Self {
        Self::new()
    }
}

// ── Block-view selection ──────────────────────────────────────────────
// Grid coordinates no longer match visible pixels once the block view inserts
// Header/Separator rows and scrolls. Selection snapshots the renderer's row
// list, so hit-testing and text extraction see the same rows.

use crate::blocks::BlockId;

/// Kind of a block-view row. Mirrors the renderer's internal `LaidRow` but
/// lives in `weft_core` so the selection model has no GUI-crate dependency.
/// Only `Output` / `Command` rows carry selectable text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BlockViewRowKind {
    Output,
    Command,
    Header,
    Separator,
    LiveCommand,
    /// v1.8.2: AI diagnose panel row; non-selectable, carries `block_id`.
    DiagnosePanel,
}

/// A single rendered row in the block view, captured at layout time. The y
/// range is scroll-adjusted physical pixels.
#[derive(Clone, Debug)]
pub struct BlockViewRow {
    pub kind: BlockViewRowKind,
    /// Visible text of this row (post-wrap single line; empty for Separator).
    pub text: String,
    /// Owning block, if any (Command/Header/LiveCommand).
    pub block_id: Option<BlockId>,
    /// Top y of the row in physical pixels (scroll-adjusted).
    pub y_top: f32,
    /// Bottom y of the row (`y_top + pitch`).
    pub y_bottom: f32,
    /// v1.6.1: Line index into the owning block's `styled_output` (`None` for
    /// non-Output rows and resume hints); resolves OSC 8 link spans.
    pub line: Option<usize>,
    /// v1.6.1: Char offset of this row's text within its source line (0 for
    /// single-chunk rows). Add it to the chunk-local `char_index` for
    /// full-line link resolution.
    pub chunk_char_offset: usize,
    /// v1.10.13: Leading visual indent (columns) of this row's text. Command
    /// first lines indent for the chevron + "> " prompt (2-3 cols); wrapped
    /// continuation lines and output rows are flush-left (0). Hit-testing
    /// subtracts this from the pixel-derived column so clicks on a command
    /// line land on the right character.
    pub indent_cols: usize,
}

impl BlockViewRow {
    /// True if the kind carries selectable text.
    pub fn is_selectable(&self) -> bool {
        matches!(
            self.kind,
            BlockViewRowKind::Output | BlockViewRowKind::Command | BlockViewRowKind::LiveCommand
        )
    }

    /// True if a physical-pixel y falls inside this row's band.
    pub fn contains_y(&self, y: f32) -> bool {
        y >= self.y_top && y < self.y_bottom
    }
}

/// Map a column (relative to the row's text start, already indent-adjusted)
/// to a char index in `text`, honoring CJK double-width; past the last char,
/// clamps to the end.
pub(super) fn char_index_at_col(text: &str, target_col: usize) -> usize {
    let mut col_cursor = 0usize;
    for (ci, c) in text.chars().enumerate() {
        let w = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
        if w == 0 {
            continue; // zero-width (combining mark): stays on previous cell
        }
        if target_col < col_cursor + w {
            return ci;
        }
        col_cursor += w;
    }
    text.chars().count()
}

/// Pixel x → char index for a block-view row, subtracting its visual indent.
pub fn pixel_x_to_char_index(
    text: &str,
    x: f64,
    content_left: f64,
    cell_w: f64,
    indent_cols: usize,
) -> usize {
    let target_col = ((x - content_left) / cell_w).max(0.0) as usize;
    char_index_at_col(text, target_col.saturating_sub(indent_cols))
}

/// A character-cell position inside the block view: an index into the cached
/// `BlockViewRow` list plus a char index into that row's `text`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BlockViewPos {
    pub row_index: usize,
    pub char_index: usize,
}

/// A selection over the block-view row snapshot; extraction walks the rows.
#[derive(Clone, Debug)]
pub struct BlockViewSelection {
    pub start: BlockViewPos,
    pub end: BlockViewPos,
    pub rows: Vec<BlockViewRow>,
}

impl BlockViewSelection {
    /// Extract the selected text by walking the captured row snapshot (rows
    /// are bottom-to-top, text reads top-to-bottom); the top boundary row
    /// contributes its tail, the bottom boundary its head. Non-selectable
    /// rows are skipped without newlines.
    pub fn text(&self) -> String {
        if let Some(text) = block_view::wrapped_source_text(self) {
            return text;
        }
        if self.rows.is_empty() {
            return String::new();
        }
        let last = self.rows.len() - 1;
        let top = self.start.row_index.max(self.end.row_index).min(last);
        let bottom = self.start.row_index.min(self.end.row_index).min(last);
        if top < bottom {
            return String::new();
        }
        // Which endpoint is the top one? Determines boundary-row slicing.
        let (top_pos, bottom_pos) = if self.start.row_index >= self.end.row_index {
            (self.start, self.end)
        } else {
            (self.end, self.start)
        };

        let mut out = String::new();
        let mut contributed = false; // track for newline insertion
        for i in (bottom..=top).rev() {
            let row = &self.rows[i];
            if !row.is_selectable() || row.text.is_empty() {
                continue;
            }
            let chars: Vec<char> = row.text.chars().collect();
            let max_char = chars.len();
            let (c_start, c_end) = if top == bottom {
                // Single row: inclusive range between the two char indices.
                let lo = top_pos.char_index.min(bottom_pos.char_index).min(max_char);
                let hi = top_pos.char_index.max(bottom_pos.char_index).min(max_char);
                (lo, hi)
            } else if i == top {
                // Top boundary: tail from the anchor down.
                (top_pos.char_index.min(max_char), max_char)
            } else if i == bottom {
                // Bottom boundary: head up to the drag endpoint.
                (0, bottom_pos.char_index.min(max_char))
            } else {
                // Middle row: entire text.
                (0, max_char)
            };
            if c_end > c_start {
                if contributed {
                    out.push('\n');
                }
                out.extend(&chars[c_start..c_end]);
                contributed = true;
            } else if c_end == c_start && top == bottom {
                // Empty single-row selection: mark contributed anyway.
                contributed = true;
            }
        }
        out
    }

    /// Refresh the row snapshot and remap `start`/`end` row_index by matching
    /// (block_id, text, kind); scrolled-off rows map to the closest y-center.
    pub fn sync_rows(&mut self, new_rows: Vec<BlockViewRow>) {
        let remap = |old_idx: usize| -> usize {
            if old_idx >= self.rows.len() || new_rows.is_empty() {
                return 0;
            }
            let old_row = &self.rows[old_idx];
            // Exact match first (same block + text + kind).
            if let Some(i) = new_rows.iter().position(|r| {
                r.kind == old_row.kind && r.block_id == old_row.block_id && r.text == old_row.text
            }) {
                return i;
            }
            // Fallback: match by text only (LiveCommand rows have no block_id).
            if let Some(i) = new_rows
                .iter()
                .position(|r| r.kind == old_row.kind && r.text == old_row.text)
            {
                return i;
            }
            // Row scrolled off — use the new row with the closest y-center.
            let old_yc = (old_row.y_top + old_row.y_bottom) * 0.5;
            let mut best = 0usize;
            let mut best_d = f32::MAX;
            for (i, r) in new_rows.iter().enumerate() {
                let yc = (r.y_top + r.y_bottom) * 0.5;
                let d = (yc - old_yc).abs();
                if d < best_d {
                    best_d = d;
                    best = i;
                }
            }
            best
        };
        self.start.row_index = remap(self.start.row_index).min(new_rows.len().saturating_sub(1));
        self.end.row_index = remap(self.end.row_index).min(new_rows.len().saturating_sub(1));
        self.rows = new_rows;
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
        // Regression: margin endpoints used to panic text_from_grid on copy.
        let grid = filled_grid(); // 5 rows × 10 cols
        let sel = Selection::new(
            GridPos::new(0, 0),
            GridPos::new(99, 99),
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

    #[test]
    fn clear_grid_selection_keeps_block_view_selection() {
        // v1.10.20: migration drops the grid half only; the fresh block
        // selection and the in-progress drag must survive.
        let mut handler = SelectionHandler::new();
        handler.start(GridPos::new(0, 0), SelectionMode::Simple);
        let rows = vec![bv_row(BlockViewRowKind::Output, "hello", 0.0, 20.0)];
        handler.start_block_view(
            BlockViewPos {
                row_index: 0,
                char_index: 2,
            },
            rows,
        );
        handler.clear_grid_selection();
        assert!(handler.selection.is_none());
        assert!(handler.selecting, "drag continues in block space");
        assert!(handler.block_view_selection.is_some());
    }

    // ── BlockViewSelection tests ───────────────────────────────────────
    // Rows are bottom-to-top; a higher index drag reads top-to-bottom.

    fn bv_row(kind: BlockViewRowKind, text: &str, y_top: f32, y_bottom: f32) -> BlockViewRow {
        BlockViewRow {
            kind,
            text: text.to_string(),
            block_id: None,
            y_top,
            y_bottom,
            line: None,
            chunk_char_offset: 0,
            indent_cols: 0,
        }
    }

    fn bv_rows() -> Vec<BlockViewRow> {
        // 5 rows, index 0 = bottom. Each row 20px tall.
        vec![
            bv_row(BlockViewRowKind::Output, "world", 0.0, 20.0), // idx 0 (bottom)
            bv_row(BlockViewRowKind::Output, "hello", 20.0, 40.0), // idx 1
            bv_row(BlockViewRowKind::Separator, "", 40.0, 60.0),  // idx 2 (skipped)
            bv_row(BlockViewRowKind::Command, "echo hi", 60.0, 80.0), // idx 3
            bv_row(BlockViewRowKind::Header, "~/proj", 80.0, 100.0), // idx 4 (top)
        ]
    }

    #[test]
    fn bv_single_row_partial() {
        // Drag within row index 1 ("hello"), char 1..3 -> "el".
        let rows = bv_rows();
        let sel = BlockViewSelection {
            start: BlockViewPos {
                row_index: 1,
                char_index: 1,
            },
            end: BlockViewPos {
                row_index: 1,
                char_index: 3,
            },
            rows,
        };
        assert_eq!(sel.text(), "el");
    }

    #[test]
    fn bv_cross_row_skips_non_selectable() {
        // Drag idx 3 char 5 down to idx 0 char 2 → "hi" (tail) + "hello" +
        // "wo" (head); idx 2 Separator skipped, newlines only between
        // consecutive selectable contributions.
        let rows = bv_rows();
        let sel = BlockViewSelection {
            start: BlockViewPos {
                row_index: 3,
                char_index: 5,
            },
            end: BlockViewPos {
                row_index: 0,
                char_index: 2,
            },
            rows,
        };
        assert_eq!(sel.text(), "hi\nhello\nwo");
    }

    #[test]
    fn bv_reversed_endpoints() {
        // Same drag with start/end swapped — must still read top-to-bottom.
        let rows = bv_rows();
        let sel = BlockViewSelection {
            start: BlockViewPos {
                row_index: 0,
                char_index: 2,
            },
            end: BlockViewPos {
                row_index: 3,
                char_index: 5,
            },
            rows,
        };
        assert_eq!(sel.text(), "hi\nhello\nwo");
    }

    #[test]
    fn bv_empty_text_returns_empty() {
        let rows = vec![];
        let sel = BlockViewSelection {
            start: BlockViewPos {
                row_index: 0,
                char_index: 0,
            },
            end: BlockViewPos {
                row_index: 0,
                char_index: 5,
            },
            rows,
        };
        assert_eq!(sel.text(), "");
    }

    #[test]
    fn bv_char_index_clamped() {
        // Out-of-range char indices clamp to "hello"'s 5 chars -> empty slice.
        let rows = bv_rows();
        let sel = BlockViewSelection {
            start: BlockViewPos {
                row_index: 1,
                char_index: 100,
            },
            end: BlockViewPos {
                row_index: 1,
                char_index: 200,
            },
            rows,
        };
        assert_eq!(sel.text(), "");
    }

    #[test]
    fn bv_handler_lifecycle() {
        let mut h = SelectionHandler::new();
        let rows = bv_rows();
        // Press at idx 1 char 0, drag to idx 0 char 3 → "hello" + "wor".
        h.start_block_view(
            BlockViewPos {
                row_index: 1,
                char_index: 0,
            },
            rows.clone(),
        );
        assert!(h.selecting);
        assert!(h.block_view_selection.is_some());
        h.extend_block_view(BlockViewPos {
            row_index: 0,
            char_index: 3,
        });
        h.end();
        assert!(!h.selecting);
        let text = h.block_view_text().unwrap();
        assert_eq!(text, "hello\nwor");
        h.clear();
        assert!(h.block_view_selection.is_none());
    }

    // sync_rows fallback tests: culled rows may be absent from `new_rows`.

    #[test]
    fn sync_rows_clipped_row_falls_back_to_y_center() {
        // Old idx 4 (Header) absent from new_rows; y-center fallback maps
        // it to new idx 2 (Command, y-center=70).
        let old_rows = bv_rows();
        let mut sel = BlockViewSelection {
            start: BlockViewPos {
                row_index: 4,
                char_index: 0,
            },
            end: BlockViewPos {
                row_index: 3,
                char_index: 5,
            },
            rows: old_rows,
        };
        let new_rows = vec![
            bv_row(BlockViewRowKind::Output, "world", 0.0, 20.0), // new idx 0
            bv_row(BlockViewRowKind::Output, "hello", 20.0, 40.0), // new idx 1
            bv_row(BlockViewRowKind::Command, "echo hi", 60.0, 80.0), // new idx 2
        ];
        sel.sync_rows(new_rows);
        // start (old idx 4) → new idx 2 (y-center); end exact-matches idx 2.
        assert_eq!(sel.start.row_index, 2);
        assert_eq!(sel.end.row_index, 2);
    }

    #[test]
    fn sync_rows_all_clipped_does_not_panic() {
        // Every row offscreen or unmatchable: must not panic; indices clamp.
        let old_rows = bv_rows();
        let mut sel = BlockViewSelection {
            start: BlockViewPos {
                row_index: 4,
                char_index: 0,
            },
            end: BlockViewPos {
                row_index: 0,
                char_index: 3,
            },
            rows: old_rows,
        };
        let new_rows = vec![
            bv_row(BlockViewRowKind::Output, "completely_new", 200.0, 220.0),
            bv_row(BlockViewRowKind::Output, "also_new", 220.0, 240.0),
        ];
        sel.sync_rows(new_rows);
        // Both endpoints must be valid indices into new_rows (0 or 1).
        assert!(sel.start.row_index < 2);
        assert!(sel.end.row_index < 2);
    }

    #[test]
    fn sync_rows_empty_new_rows_does_not_panic() {
        // Empty new_rows (pre-layout first frame): must not panic; clamp to 0.
        let old_rows = bv_rows();
        let mut sel = BlockViewSelection {
            start: BlockViewPos {
                row_index: 2,
                char_index: 0,
            },
            end: BlockViewPos {
                row_index: 0,
                char_index: 3,
            },
            rows: old_rows,
        };
        sel.sync_rows(Vec::new());
        assert_eq!(sel.start.row_index, 0);
        assert_eq!(sel.end.row_index, 0);
    }

    // v1.7.0-E: selection must not corrupt styled output (reads plain
    // `row.text` only); extracted text must contain no ESC bytes.
    #[test]
    fn selection_does_not_corrupt_styled_output() {
        use crate::blocks::{build_styled_output_from_runs, CapturedStyle, CapturedStyleRun};
        use crate::grid::{CellColor, CellFlags};

        // Build a StyledOutput with 2 lines, each with a colored run.
        // Line 0: "error: file not found" with red fg on "error"
        // Line 1: "  see /tmp/log" with green fg on the path
        let runs = vec![
            CapturedStyleRun {
                start_char: 0,
                end_char: 5,
                style: CapturedStyle::from_attrs(
                    CellColor::Palette(1),
                    CellColor::Default,
                    CellFlags::BOLD,
                ),
            },
            CapturedStyleRun {
                start_char: 6,
                end_char: 21,
                style: CapturedStyle::default(),
            },
            // newline at 21; line 1 starts at 22 and is 14 chars ("  see /tmp/log")
            CapturedStyleRun {
                start_char: 22,
                end_char: 36,
                style: CapturedStyle::from_attrs(
                    CellColor::Palette(2),
                    CellColor::Default,
                    CellFlags::empty(),
                ),
            },
        ];
        // Text is stored separately from StyledOutput (text is authoritative
        // for search/copy; StyledOutput is the parallel style model).
        let line0_text = "error: file not found";
        let line1_text = "  see /tmp/log";
        let full_text = format!("{line0_text}\n{line1_text}");
        let styled = build_styled_output_from_runs(&full_text, &runs).expect("styled output");
        // Snapshot the styled output before selection.
        let original_line_count = styled.lines.len();
        let original_line0_spans = styled.lines[0].attributes.len();
        let original_line1_spans = styled.lines[1].attributes.len();

        // Rows reference styled-output lines; text comes from the original.
        let rows = vec![
            BlockViewRow {
                kind: BlockViewRowKind::Output,
                text: line1_text.to_string(),
                block_id: None,
                y_top: 0.0,
                y_bottom: 20.0,
                line: Some(1),
                chunk_char_offset: 0,
                indent_cols: 0,
            },
            BlockViewRow {
                kind: BlockViewRowKind::Output,
                text: line0_text.to_string(),
                block_id: None,
                y_top: 20.0,
                y_bottom: 40.0,
                line: Some(0),
                chunk_char_offset: 0,
                indent_cols: 0,
            },
        ];

        // Perform a selection spanning both rows.
        let sel = BlockViewSelection {
            start: BlockViewPos {
                row_index: 1,
                char_index: 0,
            },
            end: BlockViewPos {
                row_index: 0,
                char_index: 5,
            },
            rows,
        };

        // Extract selection text.
        let selected_text = sel.text();
        assert!(
            !selected_text.is_empty(),
            "selection text must not be empty"
        );
        // V17 §2.7: copied text contains no ANSI escape bytes.
        assert!(
            !selected_text.as_bytes().contains(&0x1b),
            "selection text contains ESC bytes"
        );

        // The styled output is unchanged — selection had no mutable path.
        assert_eq!(styled.lines.len(), original_line_count);
        assert_eq!(
            styled.lines[0].attributes.len(),
            original_line0_spans,
            "line 0 attributes corrupted by selection"
        );
        assert_eq!(
            styled.lines[1].attributes.len(),
            original_line1_spans,
            "line 1 attributes corrupted by selection"
        );
    }

    #[test]
    fn char_index_at_col_honors_cjk_and_clamps() {
        assert_eq!(char_index_at_col("abc", 0), 0);
        assert_eq!(char_index_at_col("abc", 2), 2);
        assert_eq!(char_index_at_col("abc", 9), 3); // 超尾 clamp
        assert_eq!(char_index_at_col("中文x", 1), 0); // 点击中字右半 → 中
        assert_eq!(char_index_at_col("中文x", 2), 1); // 文
        assert_eq!(char_index_at_col("中文x", 4), 2); // x
        assert_eq!(char_index_at_col("e\u{301}x", 1), 2); // combining 不占列
    }

    #[test]
    fn pixel_x_to_char_index_accounts_for_command_indent() {
        let text = "docker ps";
        // 命令首行(indent=3):文本从 content_left + 3 列开始
        assert_eq!(
            pixel_x_to_char_index(text, 3.0 * 10.0 + 5.0, 0.0, 10.0, 3),
            0
        ); // 第 0 字符
        assert_eq!(
            pixel_x_to_char_index(text, 3.0 * 10.0 + 2.5 * 10.0, 0.0, 10.0, 3),
            2
        ); // 第 2 字符
           // 点击缩进区(chevron/"> " 上)→ saturating_sub → 0 → 命令开头
        assert_eq!(pixel_x_to_char_index(text, 8.0, 0.0, 10.0, 3), 0);
        // 续行(顶格,indent=0):直接从 content_left 开始
        assert_eq!(pixel_x_to_char_index(text, 5.0, 0.0, 10.0, 0), 0);
        // 非 foldable(indent=2)
        assert_eq!(
            pixel_x_to_char_index(text, 2.0 * 10.0 + 5.0, 0.0, 10.0, 2),
            0
        );
    }

    #[test]
    fn text_extracts_wrapped_command_chunks_in_reading_order() {
        // 修复后的 bv_rows(索引大 = 视觉靠上):[2] 命令首行 [1] 续行 [0] 输出。
        let rows = vec![
            bv_row(BlockViewRowKind::Output, "output line", 0.0, 20.0),
            bv_row(BlockViewRowKind::Command, "ps --filter", 40.0, 60.0),
            bv_row(BlockViewRowKind::Command, "docker", 60.0, 80.0),
        ];
        let sel = BlockViewSelection {
            start: BlockViewPos {
                row_index: 0,
                char_index: 100,
            },
            end: BlockViewPos {
                row_index: 2,
                char_index: 0,
            },
            rows,
        };
        assert_eq!(sel.text(), "docker\nps --filter\noutput line");
    }
}
