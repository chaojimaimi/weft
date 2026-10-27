//! Terminal row with dirty tracking.
//!
//! `dirty_occ` tracks the last modified cell index for efficient rendering.

use super::cell::{Cell, CellFlags, CellWidth};
use super::row_extras::RowExtras;

#[derive(Clone)]
pub struct Row {
    pub cells: Vec<Cell>,
    pub dirty_occ: usize,
    /// v1.15.3 P1a (PLAN_v1153 §2): exact occupied-cell count — one past the
    /// last non-default cell (`last_written_cell() + 1`; 0 = all-default
    /// row). The discriminator predicate is unchanged (`character != ' ' ||
    /// !flags.is_empty()`); only its cost changes from per-row reverse scan
    /// to O(1) reads. Maintained at the mutation funnels:
    /// - content writes hang [`Row::cell_written`] (O(1) extend),
    /// - tail erases / wide-pair clears hang [`Row::recompute_tail`]
    ///   (amortized O(1) shrink),
    /// - whole-Row construction/clear assigns 0; per-cell shift ops
    ///   (ICH/DCH) and materialization recompute once.
    ///
    /// Every `Row.cells` mutator is pinned to this classified list by the
    /// `occ_mutation_sites` source-scan test (grid/occ_mutation_sites_tests).
    pub(crate) occ: usize,
    /// Whether this row has been wrapped from the previous line.
    pub wrapped: bool,
    /// v1.6.0: sparse per-cell extension data for multi-scalar graphemes.
    /// Empty for the common case (ASCII / single-scalar cells). Entries are
    /// keyed by column index; see [`RowExtras`] for invariants.
    pub extras: RowExtras,
}

impl Row {
    /// PLAN_v1153 §7 (rollback/reference): the pre-occ implementation of the
    /// tail index, kept verbatim as the `#[cfg(test)]` oracle. Production
    /// reads `occ` instead; the property tests assert `occ - 1 ==` this scan
    /// after every operation of a randomized mixed stream.
    #[cfg(test)]
    pub(crate) fn last_written_cell(&self) -> isize {
        for (i, c) in self.cells.iter().enumerate().rev() {
            if c.character != ' ' || !c.flags.is_empty() {
                return i as isize;
            }
        }
        -1
    }

    /// v1.15.3 P1a: content-write entry point — records that `col` now holds
    /// non-default content (the caller writes the cell through a write
    /// funnel; this only extends the tail). O(1).
    ///
    /// `pub` on purpose: `Row.cells` is a public field, so out-of-crate
    /// mutation paths (weft_app test fixtures) owe the row the same
    /// notification the in-crate write funnels give.
    #[inline]
    pub fn cell_written(&mut self, col: usize) {
        if col >= self.occ {
            self.occ = col + 1;
        }
    }

    /// v1.15.3 P1a: amortized O(1) tail shrink for erase paths. Hang on
    /// EL/ED tails, `clear_wide_pair_at` spacer clears, and any site that
    /// resets cells at or behind the tail. Each shrink step consumes one
    /// earlier `cell_written` extension, so the total stays linear in the
    /// number of writes.
    pub(crate) fn recompute_tail(&mut self) {
        // Defensive clamp: a caller may have truncated `cells` (e.g.
        // `reflow::resize_row_cells` in clear_line_all) without shrinking
        // `occ` first — the invariant `occ <= cells.len()` must hold before
        // the tail indexing below.
        if self.occ > self.cells.len() {
            self.occ = self.cells.len();
        }
        while self.occ > 0 {
            let cell = self.cells[self.occ - 1];
            if cell.character != ' ' || !cell.flags.is_empty() {
                break;
            }
            self.occ -= 1;
        }
    }

    /// v1.15.3 P1a: full reverse scan (O(cols)). Only for per-cell shift ops
    /// (ICH/DCH move cells without going through write funnels — `occ` can
    /// become stale in BOTH directions there) and for test row builders that
    /// bypass the production funnels.
    pub(crate) fn recompute_full(&mut self) {
        self.occ = 0;
        for (i, c) in self.cells.iter().enumerate().rev() {
            if c.character != ' ' || !c.flags.is_empty() {
                self.occ = i + 1;
                break;
            }
        }
    }

    pub fn new(cols: usize) -> Self {
        Self {
            cells: vec![Cell::default(); cols],
            dirty_occ: 0,
            occ: 0,
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
        // v1.15.3 P1a: the resets above can clear the tail cell (spacer at
        // occ-1 / its lead) — shrink `occ` back. Amortized O(1): one bounds
        // check when the tail cell survives.
        self.recompute_tail();
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
        // v1.15.3 P1a: orphan resets can clear the tail cell — shrink `occ`.
        self.recompute_tail();
    }

    /// v1.0 perf: Clear all cells in place (reuses Vec capacity, no allocation).
    /// Equivalent to `*self = Row::new(cols)` but avoids the Vec allocation.
    pub fn clear(&mut self) {
        for cell in &mut self.cells {
            *cell = Cell::default();
        }
        self.dirty_occ = 0;
        // v1.15.3 P1a: construction-time reset (all cells are default now).
        self.occ = 0;
        self.wrapped = false;
        // v1.6.0: drop sparse grapheme extras — cleared cells have no cluster.
        self.extras.clear();
    }

    /// v1.15.3 P1b (PLAN_v1153 §3): clear in place AND renormalize the row
    /// width — the scroll-path twin of `*self = Row::new(num_cols)` without
    /// the per-scrolled-line allocation. Field-identical observable state to
    /// a fresh `Row::new(num_cols)`.
    ///
    /// Width normalization: viewport rows can outlive a narrowing resize
    /// wider than `num_cols` (rows only ever grow — `resize_dims` B-2 /
    /// `reflow::resize_row_cells`); the old `Row::new(num_cols)` replaced
    /// them with exact-width rows, so `blank_in_place` must renormalize the
    /// same way (truncate the stale right half, pad if somehow short) to
    /// stay observable-state-identical.
    pub fn blank_in_place(&mut self, num_cols: usize) {
        if self.cells.len() != num_cols {
            self.cells.resize(num_cols, Cell::default());
        }
        self.cells.fill(Cell::default());
        self.dirty_occ = 0;
        self.occ = 0;
        self.wrapped = false;
        self.extras.clear();
    }
}
