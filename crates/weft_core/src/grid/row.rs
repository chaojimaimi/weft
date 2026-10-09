//! Terminal row with dirty tracking.
//!
//! `dirty_occ` tracks the last modified cell index for efficient rendering.

use super::cell::{Cell, CellFlags, CellWidth};
use super::row_extras::RowExtras;

#[derive(Clone)]
pub struct Row {
    pub cells: Vec<Cell>,
    pub dirty_occ: usize,
    /// Whether this row has been wrapped from the previous line.
    pub wrapped: bool,
    /// v1.6.0: sparse per-cell extension data for multi-scalar graphemes.
    /// Empty for the common case (ASCII / single-scalar cells). Entries are
    /// keyed by column index; see [`RowExtras`] for invariants.
    pub extras: RowExtras,
}

impl Row {
    /// F1 (PLAN_v1137 §1): index of the last non-default cell (a
    /// never-written blank is `' '` + empty flags), or -1 for an all-default
    /// row. Load-bearing for `FlatStorage::encode_row`'s tail truncation:
    /// written cells set DIRTY and erase/wide-pair clears use the
    /// all-default `reset()`, so `flags.is_empty()` ⇒ all-default attributes
    /// — the suffix past this index emits no bytes unless the encoder's
    /// running attribute state is non-default (then exactly one reset space
    /// cell is materialized there).
    pub(crate) fn last_written_cell(&self) -> isize {
        for (i, c) in self.cells.iter().enumerate().rev() {
            if c.character != ' ' || !c.flags.is_empty() {
                return i as isize;
            }
        }
        -1
    }

    pub fn new(cols: usize) -> Self {
        Self {
            cells: vec![Cell::default(); cols],
            dirty_occ: 0,
            wrapped: false,
            extras: RowExtras::new(),
        }
    }

    pub fn len(&self) -> usize {
        self.cells.len()
    }

    pub fn is_empty(&self) -> bool {
        self.cells.is_empty()
    }

    pub fn is_dirty(&self) -> bool {
        self.dirty_occ > 0
    }

    pub fn clear_dirty(&mut self) {
        self.dirty_occ = 0;
    }

    pub fn mark_dirty(&mut self, col: usize) {
        self.dirty_occ = self.dirty_occ.max(col + 1);
    }

    /// Clear the other half of a wide glyph occupying `col`, if any.
    ///
    /// Call this before overwriting or clearing a cell. A write can target
    /// either the full-width leading cell or its `WIDE_SPACER`; leaving the
    /// other half behind breaks the row invariant and later TUI repaints can
    /// combine unrelated glyph halves into visible corruption.
    pub(crate) fn clear_wide_pair_at(&mut self, col: usize) {
        if col >= self.cells.len() {
            return;
        }
        if self.cells[col].flags.contains(CellFlags::WIDE_SPACER)
            && col > 0
            && self.cells[col - 1].width == CellWidth::Full
        {
            self.cells[col - 1].reset();
            // v1.6.0: a cleared lead cell loses its multi-scalar cluster.
            self.extras.clear_cell(col - 1);
            self.mark_dirty(col - 1);
        }
        if self.cells[col].width == CellWidth::Full
            && col + 1 < self.cells.len()
            && self.cells[col + 1].flags.contains(CellFlags::WIDE_SPACER)
        {
            self.cells[col + 1].reset();
            self.mark_dirty(col + 1);
            // v1.6.0: the lead cell at `col` is being cleared by the caller;
            // drop its cluster too so it doesn't leak into a new char.
            self.extras.clear_cell(col);
            // T3 review P1: the spacer half carries the lead's hyperlink
            // (OSC 8 two-half hover shape), so its extras entry — hyperlink
            // only, a spacer never holds a grapheme — must die with the
            // pair. Left behind, flat encoding sees "default cell + stale
            // extras" and re-materializes a phantom clickable link on a
            // blank history cell.
            self.extras.clear_cell(col + 1);
        }
    }

    /// Remove orphaned wide-cell halves after an operation that shifts cells.
    pub(crate) fn repair_wide_pairs(&mut self) {
        for col in 0..self.cells.len() {
            let orphan_spacer = self.cells[col].flags.contains(CellFlags::WIDE_SPACER)
                && (col == 0 || self.cells[col - 1].width != CellWidth::Full);
            let orphan_lead = self.cells[col].width == CellWidth::Full
                && (col + 1 >= self.cells.len()
                    || !self.cells[col + 1].flags.contains(CellFlags::WIDE_SPACER));
            if orphan_spacer || orphan_lead {
                self.cells[col].reset();
                self.mark_dirty(col);
            }
        }
    }

    /// v1.0 perf: Clear all cells in place (reuses Vec capacity, no allocation).
    /// Equivalent to `*self = Row::new(cols)` but avoids the Vec allocation.
    pub fn clear(&mut self) {
        for cell in &mut self.cells {
            *cell = Cell::default();
        }
        self.dirty_occ = 0;
        self.wrapped = false;
        // v1.6.0: drop sparse grapheme extras — cleared cells have no cluster.
        self.extras.clear();
    }
}
