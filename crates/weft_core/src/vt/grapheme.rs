use super::Terminal;
use crate::grid::{terminal_char_width, CellFlags, CellWidth};

impl Terminal {
    pub(super) fn previous_cell_position(&self) -> Option<(usize, usize)> {
        let num_cols = self.grid.num_cols;
        if num_cols == 0 || self.grid.cursor.row >= self.grid.num_rows {
            return None;
        }
        let row = self.grid.cursor.row;
        let mut col = if self.grid.cursor.wrap_pending {
            self.grid.cursor.col.min(num_cols - 1)
        } else {
            self.grid.cursor.col.checked_sub(1)?.min(num_cols - 1)
        };
        if self.grid.viewport[row].cells[col]
            .flags
            .contains(CellFlags::WIDE_SPACER)
            && col > 0
        {
            col -= 1;
        }
        (self.grid.viewport[row].cells[col].character != ' ').then_some((row, col))
    }

    /// v1.6.0: Append a scalar to the previous cell's grapheme cluster.
    ///
    /// The full cluster string is stored in [`RowExtras`](crate::grid::RowExtras)
    /// and the cell's `EXTRA` flag is set so consumers (selection, copy,
    /// renderer) know to consult the extras for the full cluster. The cell's
    /// `character` field keeps the lead scalar.
    ///
    /// If the appended scalar changes the cluster's terminal width (e.g.
    /// VS16 promoting `*` from width 1 to width 2), the cell is expanded to
    /// `CellWidth::Full` and a `WIDE_SPACER` is inserted at `col + 1`,
    /// advancing the cursor to preserve column alignment.
    ///
    /// This replaces the v1.5 `replace_previous_grapheme` fallback for:
    /// - Combining marks (width 0) that extend the previous cluster
    /// - ZWJ sequences where the following base char joins the cluster
    /// - Regional indicator pairs (second RI extends the first into a flag)
    /// - Skin tone modifiers on emoji
    /// - Variation selectors (VS15/VS16)
    ///
    /// Returns `true` if the scalar was appended. Returns `false` if there
    /// is no previous cell to extend (the scalar is then dropped).
    pub(super) fn append_scalar_to_previous_cluster(&mut self, scalar: char) -> bool {
        let Some((row, col)) = self.previous_cell_position() else {
            return false;
        };
        let num_cols = self.grid.num_cols;
        let base_char = self.grid.viewport[row].cells[col].character;
        self.grid.viewport[row]
            .extras
            .append_scalar(col, base_char, scalar);

        // Recompute the cluster's terminal width. If it grew from 1 to 2
        // (e.g. VS16 on a narrow base), expand the cell to Full and add a
        // WIDE_SPACER so the next char lands at the correct column.
        //
        // v1.6.0: We compute the width from the base scalar (via
        // `terminal_char_width`, which returns 0 for emoji modifiers and
        // other combining marks) rather than `UnicodeWidthStr::width`,
        // because the latter sums per-char widths and would count emoji
        // modifiers (U+1F3FB..U+1F3FF) as width 2 each, producing 4 for
        // "👩🏽" instead of the correct 2. VS16 (\u{FE0F}) is special: it
        // promotes a narrow base to emoji width (2), so we detect it
        // explicitly.
        let grapheme = self.grid.viewport[row]
            .extras
            .grapheme_at(col)
            .unwrap_or("");
        let new_width = if grapheme.contains('\u{fe0f}') {
            // VS16 promotes the cluster to emoji width (2).
            2
        } else {
            terminal_char_width(base_char)
        };
        let old_width = self.grid.viewport[row].cells[col].width as usize;

        let need_expand = new_width > old_width
            && self.grid.viewport[row].cells[col].width == CellWidth::Half
            && col + 1 < num_cols;
        {
            let cell = &mut self.grid.viewport[row].cells[col];
            cell.flags.insert(CellFlags::EXTRA | CellFlags::DIRTY);
        }
        if need_expand {
            // Cluster became wider — expand to Full and insert a spacer.
            // Clear whatever was at col+1 first (could be a written char or
            // the cursor's current position).
            self.grid.viewport[row].clear_wide_pair_at(col + 1);
            // v1.6.0 review M3: clear orphaned grapheme extras at col+1
            // before overwriting with WIDE_SPACER. Without this, a previous
            // multi-scalar cluster at col+1 leaves a stale entry that causes
            // incorrect cluster strings when future combining marks arrive.
            self.grid.viewport[row].extras.clear_grapheme(col + 1);
            {
                let cell = &mut self.grid.viewport[row].cells[col];
                cell.width = CellWidth::Full;
            }
            let spacer = &mut self.grid.viewport[row].cells[col + 1];
            spacer.character = ' ';
            spacer.flags = CellFlags::WIDE_SPACER;
            spacer.width = CellWidth::Half;
            self.grid.viewport[row].mark_dirty(col + 1);
            // Advance the cursor by 1 to account for the new spacer. The
            // cursor was at col+1 (after the base char); it should now be
            // at col+2 (after the spacer).
            if self.grid.cursor.row == row && self.grid.cursor.col == col + 1 {
                self.grid.cursor.col = col + 2;
                if self.grid.cursor.col >= num_cols {
                    self.grid.cursor.wrap_pending = true;
                    self.grid.cursor.col = num_cols - 1;
                }
            }
        }
        self.grid.viewport[row].mark_dirty(col);
        true
    }
}
