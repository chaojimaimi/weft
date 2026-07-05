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
    /// Block-view selection, used when the Warp-style block view is active.
    /// Mutually exclusive with `selection` in practice (the app dispatches
    /// based on `show_block_view()`), but kept as a separate field so the two
    /// models don't entangle.
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
//
// The classic `Selection` / `GridPos` model indexes the terminal Grid by
// (row, col). In the Warp-style block view the visible content is laid out
// from `Block.output` strings with a `pitch = cell_h * 1.1` row spacing plus
// inserted Header / Separator / CWD rows and a scroll offset — so grid
// coordinates and visible pixel positions no longer correspond, and a
// grid-based copy lands on the wrong line (the user-visible "复制错位" bug).
//
// `BlockViewSelection` is a self-contained model driven by a snapshot of the
// *visible* rows produced by the renderer (`BlockViewRow`). Hit-testing and
// text extraction both operate on the same row list, so what you see is what
// you copy.

use crate::blocks::BlockId;

/// Kind of a block-view row. Mirrors the renderer's internal `LaidRow` but
/// lives in `weft_core` so the selection model has no dependency on the GUI
/// crate. Only `Output` / `Command` rows carry selectable text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BlockViewRowKind {
    Output,
    Command,
    Header,
    Separator,
    LiveCommand,
}

/// A single rendered row in the block view, captured at layout time. The y
/// range is in physical pixels and already includes the scroll offset, so the
/// hit-test is a simple range check.
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

/// A character-cell position inside the block view, expressed as an index
/// into the cached `BlockViewRow` list plus a char index into that row's
/// `text`. Renderer-side hit-testing converts pixel (x, y) to this.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BlockViewPos {
    pub row_index: usize,
    pub char_index: usize,
}

/// A selection over the block-view row snapshot. The `rows` vector is the
/// layout captured when the drag started; text extraction walks it directly,
/// bypassing the Grid entirely.
#[derive(Clone, Debug)]
pub struct BlockViewSelection {
    pub start: BlockViewPos,
    pub end: BlockViewPos,
    pub rows: Vec<BlockViewRow>,
}

impl BlockViewSelection {
    /// Extract the selected text by walking the captured row snapshot.
    ///
    /// Rows are laid out bottom-to-top (index 0 = closest to the input box);
    /// text reads top-to-bottom, so we walk from the larger row_index (top)
    /// down to the smaller one (bottom). `start` is the mouse-press anchor,
    /// `end` is the current drag endpoint — their relative position determines
    /// which side of each boundary row is included:
    ///
    /// - top row: from `0..top.char_index` (if start is the top anchor) or
    ///   `top.char_index..max` (if start is the bottom anchor)
    /// - bottom row: the complementary side
    ///
    /// Non-selectable rows in the range are skipped without contributing text
    /// or a newline (the newline is added only between two selectable rows
    /// that both contribute text).
    pub fn text(&self) -> String {
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
                // Top boundary: include [top_pos.char_index, max) — the part
                // of the row BELOW the anchor (normal terminal selection: drag
                // starts at the anchor and extends downward, so the top row
                // contributes its tail, not its head).
                (top_pos.char_index.min(max_char), max_char)
            } else if i == bottom {
                // Bottom boundary: include [0, bottom_pos.char_index) — the
                // part of the row ABOVE the drag endpoint.
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
                // Empty single-row selection: still mark as contributed so
                // nothing surprising happens, but yields empty string.
                contributed = true;
            }
        }
        out
    }

    /// Refresh the row snapshot to the current frame's rows and remap
    /// `start`/`end` row_index by matching (block_id, text, kind). Rows that
    /// scrolled off the visible region (not found in `new_rows`) are remapped
    /// to the new row with the closest y-center — this keeps the selection
    /// anchored to the same screen position instead of jumping to a
    /// proportional index that may point at completely different content.
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
            // Fallback: match by text only (handles rows whose block_id is
            // None, like LiveCommand, or where block_id changed identity).
            if let Some(i) = new_rows
                .iter()
                .position(|r| r.kind == old_row.kind && r.text == old_row.text)
            {
                return i;
            }
            // Row scrolled off — find the new row whose y-center is closest
            // to the old row's y-center. This keeps the selection at roughly
            // the same screen position instead of mapping to unrelated
            // content via a proportional index.
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

    // ── BlockViewSelection tests ───────────────────────────────────────
    //
    // The rows vector is laid out bottom-to-top (index 0 = closest to the
    // input box). A drag from a higher index (top) to a lower index (bottom)
    // selects text reading top-to-bottom.

    fn bv_row(kind: BlockViewRowKind, text: &str, y_top: f32, y_bottom: f32) -> BlockViewRow {
        BlockViewRow {
            kind,
            text: text.to_string(),
            block_id: None,
            y_top,
            y_bottom,
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
        // Drag from row 3 ("echo hi") char 5 down to row 0 ("world") char 2.
        // top = idx 3, top_pos.char_index = 5 -> [5,7) = "hi" (tail of row)
        // idx 2 Separator skipped (no text, no newline)
        // idx 1 "hello" whole
        // bottom = idx 0, bottom_pos.char_index = 2 -> [0,2) = "wo" (head of row)
        // Newlines inserted only between consecutive selectable contributions.
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
        // Same physical drag as above but with start/end swapped — text must
        // still read top-to-bottom and match.
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
        // Out-of-range char index must clamp, not panic.
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
        // "hello" has 5 chars; both indices clamp to 5 -> empty slice.
        assert_eq!(sel.text(), "");
    }

    #[test]
    fn bv_handler_lifecycle() {
        let mut h = SelectionHandler::new();
        let rows = bv_rows();
        // Press at idx 1 ("hello") char 0, drag to idx 0 ("world") char 3.
        // top = idx 1, top_pos.char_index = 0 -> [0,5) = "hello" (full row)
        // bottom = idx 0, bottom_pos.char_index = 3 -> [0,3) = "wor"
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
}
