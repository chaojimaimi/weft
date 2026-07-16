use super::Terminal;
use crate::grid::{Cell, CellFlags, CellWidth};

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

    /// Replace the grapheme immediately before the cursor with an explicit
    /// width-preserving fallback. Cells intentionally remain fixed-size and
    /// one-scalar; this avoids silently showing only part of a cluster.
    pub(super) fn replace_previous_grapheme(&mut self, expand_to_wide: bool) -> bool {
        let num_cols = self.grid.num_cols;
        let Some((row, col)) = self.previous_cell_position() else {
            return false;
        };
        let old_width = self.grid.viewport[row].cells[col].width;
        if expand_to_wide
            && old_width == CellWidth::Half
            && self.grid.cursor.wrap_pending
            && col + 1 >= num_cols
        {
            return self.relocate_wide_fallback(row, col);
        }
        let make_wide = old_width == CellWidth::Full
            || (expand_to_wide && !self.grid.cursor.wrap_pending && col + 1 < num_cols);
        if make_wide && old_width == CellWidth::Half {
            self.grid.viewport[row].clear_wide_pair_at(col + 1);
        }
        {
            let cell = &mut self.grid.viewport[row].cells[col];
            cell.character = if make_wide { '\u{ff1f}' } else { '\u{fffd}' };
            cell.width = if make_wide {
                CellWidth::Full
            } else {
                CellWidth::Half
            };
            cell.flags.insert(CellFlags::DIRTY);
        }
        self.grid.viewport[row].mark_dirty(col);
        if make_wide && old_width == CellWidth::Half {
            let spacer = &mut self.grid.viewport[row].cells[col + 1];
            spacer.character = ' ';
            spacer.flags = CellFlags::WIDE_SPACER | CellFlags::DIRTY;
            spacer.width = CellWidth::Half;
            self.grid.viewport[row].mark_dirty(col + 1);
            if self.grid.cursor.col == col + 1 {
                self.grid.cursor.col += 1;
                if self.grid.cursor.col >= num_cols {
                    self.grid.cursor.wrap_pending = true;
                    self.grid.cursor.col = num_cols - 1;
                }
            }
        }
        true
    }

    fn relocate_wide_fallback(&mut self, row: usize, col: usize) -> bool {
        if self.grid.num_cols < 2 {
            return false;
        }
        let mut source = self.grid.viewport[row].cells[col].clone();
        self.hyperlinks.unlink_cell(row, col);
        let old = &mut self.grid.viewport[row].cells[col];
        *old = Cell::default();
        old.flags.insert(CellFlags::DIRTY);
        self.grid.viewport[row].mark_dirty(col);

        self.grid.cursor.wrap_pending = false;
        self.grid.cursor.col = 0;
        let (_, bottom) = self.grid.scroll_region();
        if self.grid.cursor.row == bottom {
            self.scroll_grid_up(1);
        } else if self.grid.cursor.row + 1 < self.grid.num_rows {
            self.grid.cursor.row += 1;
        }
        let new_row = self.grid.cursor.row;
        if new_row > 0 {
            self.grid.viewport[new_row - 1].wrapped = true;
        }
        self.hyperlinks.unlink_cell(new_row, 0);
        self.hyperlinks.unlink_cell(new_row, 1);
        self.grid.viewport[new_row].clear_wide_pair_at(0);
        self.grid.viewport[new_row].clear_wide_pair_at(1);
        source.character = '\u{ff1f}';
        source.width = CellWidth::Full;
        source.flags.remove(CellFlags::WIDE_SPACER);
        source.flags.insert(CellFlags::DIRTY);
        if let Some(id) = self.active_hyperlink_id {
            source.flags.insert(CellFlags::HYPERLINK);
            self.hyperlinks.link_cell(new_row, 0, id);
        } else {
            source.flags.remove(CellFlags::HYPERLINK);
        }
        self.grid.viewport[new_row].cells[0] = source;
        let spacer = &mut self.grid.viewport[new_row].cells[1];
        spacer.character = ' ';
        spacer.flags = CellFlags::WIDE_SPACER | CellFlags::DIRTY;
        spacer.width = CellWidth::Half;
        self.grid.viewport[new_row].mark_dirty(0);
        self.grid.viewport[new_row].mark_dirty(1);
        self.grid.cursor.col = 2;
        if self.grid.cursor.col >= self.grid.num_cols {
            self.grid.cursor.wrap_pending = true;
            self.grid.cursor.col = self.grid.num_cols - 1;
        }
        true
    }
}
