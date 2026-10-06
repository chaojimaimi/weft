//! Grid clearing (CSI J/K/X) + insert/delete (CSI @/P/L/M) methods.
//!
//! v1.12.27a (P1-05): moved verbatim out of grid/mod.rs (line-budget
//! split; the flat/ submodule impl blocks are the precedent). Zero
//! behavior or visibility changes; `cursor_in_scroll_region` stays
//! reachable from here at `pub(crate)` (declared in grid/cursor.rs).

use super::{reflow, Grid, Row};

impl Grid {
    // ── Clearing ─────────────────────────────────────────────────

    /// Clear screen from cursor to bottom (CSI 0 J).
    pub fn clear_screen_below(&mut self) {
        let row = self.cursor.row;
        let col = self.cursor.col;
        // Clear from cursor to end of current line
        self.viewport[row].clear_wide_pair_at(col);
        for c in col..self.num_cols {
            self.viewport[row].cells[c].reset();
            self.viewport[row].extras.clear_cell(c);
        }
        self.viewport[row].mark_dirty(self.num_cols - 1);
        // Clear all subsequent rows
        for r in (row + 1)..self.num_rows {
            for cell in &mut self.viewport[r].cells {
                cell.reset();
            }
            self.viewport[r].extras.clear();
            self.viewport[r].mark_dirty(self.num_cols - 1);
        }
    }

    /// Clear screen from top to cursor (CSI 1 J).
    pub fn clear_screen_above(&mut self) {
        let row = self.cursor.row;
        let col = self.cursor.col;
        // Clear all preceding rows
        for r in 0..row {
            for cell in &mut self.viewport[r].cells {
                cell.reset();
            }
            self.viewport[r].extras.clear();
            self.viewport[r].mark_dirty(self.num_cols - 1);
        }
        // Clear from start of current line to cursor
        self.viewport[row].clear_wide_pair_at(col);
        for c in 0..=col {
            self.viewport[row].cells[c].reset();
            self.viewport[row].extras.clear_cell(c);
        }
        self.viewport[row].mark_dirty(col);
    }

    /// Clear entire screen (CSI 2 J).
    pub fn clear_screen_all(&mut self) {
        for row in &mut self.viewport {
            for cell in &mut row.cells {
                cell.reset();
            }
            row.extras.clear();
            row.mark_dirty(self.num_cols - 1);
        }
        // Note: does NOT reset cursor position (VT behavior)
    }

    /// Clear scrollback buffer (CSI 3 J).
    pub fn clear_scrollback(&mut self) {
        self.scrollback.clear();
        self.set_scroll_offset(0);
    }

    /// Clear line from cursor to end (CSI 0 K).
    pub fn clear_line_right(&mut self) {
        let row = self.cursor.row;
        let col = self.cursor.col;
        self.viewport[row].clear_wide_pair_at(col);
        for c in col..self.num_cols {
            self.viewport[row].cells[c].reset();
            self.viewport[row].extras.clear_cell(c);
        }
        self.viewport[row].mark_dirty(self.num_cols - 1);
    }

    /// Clear line from start to cursor (CSI 1 K).
    pub fn clear_line_left(&mut self) {
        let row = self.cursor.row;
        let col = self.cursor.col;
        self.viewport[row].clear_wide_pair_at(col);
        for c in 0..=col {
            self.viewport[row].cells[c].reset();
            self.viewport[row].extras.clear_cell(c);
        }
        self.viewport[row].mark_dirty(col);
    }

    /// Clear entire line (CSI 2 K).
    pub fn clear_line_all(&mut self) {
        let row = self.cursor.row;
        for cell in &mut self.viewport[row].cells {
            cell.reset();
        }
        self.viewport[row].extras.clear();
        // v1.10.26 (FIX_WRAP_EPOCH_AND_VIEWPORT_KEEP B-2): a full-line erase
        // re-establishes the row at `num_cols`. A B-2 narrowing resize may
        // have left the row temporarily wide (rows only grow); once the TUI
        // clears and rewrites the line at the new width, the stale right half
        // is dropped — "normalizing back to num_cols" is the intended outcome.
        reflow::resize_row_cells(&mut self.viewport[row].cells, self.num_cols);
        self.viewport[row].mark_dirty(self.num_cols.saturating_sub(1));
    }

    /// Erase `count` characters starting at cursor (CSI X).
    /// Does not move cursor.
    pub fn erase_chars(&mut self, count: usize) {
        let row = self.cursor.row;
        let col = self.cursor.col;
        let end = (col + count).min(self.num_cols);
        if col < end {
            self.viewport[row].clear_wide_pair_at(col);
            self.viewport[row].clear_wide_pair_at(end - 1);
        }
        for c in col..end {
            self.viewport[row].cells[c].reset();
            self.viewport[row].extras.clear_cell(c);
        }
        if end > 0 {
            self.viewport[row].mark_dirty(end - 1);
        }
    }

    // ── Character insertion/deletion ─────────────────────────────

    /// Insert `count` blank cells at cursor, shifting existing cells right (CSI @).
    pub fn insert_blank(&mut self, count: usize) {
        let row = self.cursor.row;
        let col = self.cursor.col;
        let cells = &mut self.viewport[row].cells;

        let shift = count.min(self.num_cols - col);
        // Shift cells right
        for i in (col + shift..self.num_cols).rev() {
            cells[i] = std::mem::take(&mut cells[i - shift]);
        }
        // v1.6.0: shift extras to match the cell shift.
        self.viewport[row]
            .extras
            .shift_right(col, shift, self.num_cols);
        self.viewport[row].repair_wide_pairs();
        self.viewport[row].mark_dirty(self.num_cols - 1);
    }

    /// Delete `count` cells at cursor, shifting remaining cells left (CSI P).
    pub fn delete_chars(&mut self, count: usize) {
        let row = self.cursor.row;
        let col = self.cursor.col;
        let cells = &mut self.viewport[row].cells;

        let shift = count.min(self.num_cols - col);
        // Shift cells left
        for i in col..self.num_cols - shift {
            cells[i] = std::mem::take(&mut cells[i + shift]);
        }
        // v1.6.0: shift extras to match the cell shift.
        self.viewport[row]
            .extras
            .shift_left(col, shift, self.num_cols);
        self.viewport[row].repair_wide_pairs();
        self.viewport[row].mark_dirty(self.num_cols - 1);
    }

    /// Insert `count` blank lines at cursor row, within scroll region (CSI L).
    pub fn insert_blank_lines(&mut self, count: usize) {
        if self.cursor_in_scroll_region() {
            let row = self.cursor.row;
            let bottom = self.scroll_bottom;
            let shift = count.min(bottom - row + 1);
            if shift == 0 {
                return;
            }
            for i in (row + shift..=bottom).rev() {
                self.viewport[i] =
                    std::mem::replace(&mut self.viewport[i - shift], Row::new(self.num_cols));
            }
            for i in row..row + shift {
                if i <= bottom {
                    self.viewport[i] = Row::new(self.num_cols);
                }
            }
            // Mark all affected rows dirty — the renderer's per-row cache is
            // position-relative and can't be shifted for a partial-region
            // operation, so all moved + blanked rows must be rebuilt.
            for i in row..=bottom {
                self.viewport[i].mark_dirty(self.num_cols - 1);
            }
        }
    }

    /// Delete `count` lines at cursor row, within scroll region (CSI M).
    pub fn delete_lines(&mut self, count: usize) {
        if self.cursor_in_scroll_region() {
            let row = self.cursor.row;
            let bottom = self.scroll_bottom;
            let shift = count.min(bottom - row + 1);
            if shift == 0 {
                return;
            }
            let region_len = bottom - row + 1;
            if shift < region_len {
                for i in row..=bottom - shift {
                    self.viewport[i] =
                        std::mem::replace(&mut self.viewport[i + shift], Row::new(self.num_cols));
                }
            }
            for i in (bottom + 1 - shift)..=bottom {
                self.viewport[i] = Row::new(self.num_cols);
            }
            // Mark all affected rows dirty — same rationale as insert_blank_lines.
            for i in row..=bottom {
                self.viewport[i].mark_dirty(self.num_cols - 1);
            }
        }
    }
}
