//! Bounded materialized history window — the D1 read path (PLAN_S3 §二 D1).
//!
//! `Grid::cell` keeps its `&self -> &Cell` contract, but flat storage has no
//! `&Row` to lend. Instead, the grid keeps a materialized window of at most
//! `num_rows` history rows inside the grid itself:
//!
//! - **Invalidate** (`&mut`, O(1)): any path that mutates flat storage —
//!   history pushes ([`Grid::push_history_row`]), storage drains
//!   ([`Grid::resize`]), retention changes ([`Grid::set_scrollback_max_lines`]).
//!   Only flips `history_window_valid`; no recomputation.
//! - **Sync** (`&mut`, ≤ num_rows materializations): [`Grid::set_scroll_offset`]
//!   (every scroll_offset change funnels here), the `Terminal::process` tail
//!   (covers streaming output), and the resize tails.
//!
//! Invariant: every public observer that reads history through `&self`
//! ([`Grid::cell`], `grapheme_at`, `grapheme_arc_at`, `hyperlink_id_at`,
//! `displayed_row_wrapped`) runs between `&mut` operations, and every
//! mutating entry point either ends in a sync or resets the offset to 0
//! (which syncs an empty window). The `debug_assert` in the reader turns a
//! violated window into a programming error instead of silent corruption.
//!
//! This `impl Grid` block lives in flat/ (评审 P2: mod.rs has zero headroom);
//! the two window fields are declared on `Grid` in grid/mod.rs.

use super::super::row::Row;
use super::super::Grid;

impl Grid {
    /// The current scroll offset (0 = live bottom, >0 = viewing history).
    ///
    /// The field became private in T2 (all writes go through
    /// [`Grid::set_scroll_offset`] so the window can follow); this getter
    /// keeps the out-of-module readers working.
    pub fn scroll_offset(&self) -> usize {
        self.scroll_offset
    }

    /// Sets the scroll offset and re-syncs the history window.
    ///
    /// Every scroll_offset write must go through here — that is what keeps
    /// the `&self` history readers (`cell` et al) on a fresh window. Clamped
    /// to the retained history length, matching `scroll_up_history`.
    pub fn set_scroll_offset(&mut self, offset: usize) {
        // Deliberately NOT clamped to scrollback.len(): the field could hold
        // an offset past history even before T2 (readers clamp via
        // `min(sb_len)` — policy gates like `is_scrolled`/screen-exit depend
        // on the stored value). The window sync clamps on its own.
        if offset == self.scroll_offset && self.history_window_valid {
            return;
        }
        self.scroll_offset = offset;
        self.sync_history_window();
    }

    /// Applies a new retention limit to the flat history (D3 counterpart of
    /// `Scrollback::set_max_lines(usize, usize)`) and re-syncs the window —
    /// eviction changes which rows the offset maps to.
    pub(crate) fn set_scrollback_max_lines(&mut self, max_lines: usize) {
        self.scrollback.set_max_lines(max_lines, self.num_cols);
        self.sync_history_window();
    }

    /// Pushes one row into flat history and invalidates the window (O(1)).
    /// All in-grid history pushes funnel here so the invalidation cannot be
    /// forgotten; the next sync point (process tail / scroll setter / resize
    /// tail) materializes afresh.
    pub(crate) fn push_history_row(&mut self, row: Row) {
        self.scrollback.push(row);
        self.history_window_valid = false;
    }

    /// Recomputes the window: the newest `min(offset, num_rows)` history
    /// rows — exactly the slice `cell()` can observe at this offset.
    pub(crate) fn sync_history_window(&mut self) {
        let sb_len = self.scrollback.len();
        let offset = self.scroll_offset.min(sb_len);
        self.history_window.clear();
        if offset > 0 {
            let start = sb_len - offset;
            let count = offset.min(self.num_rows);
            for global in start..start + count {
                if let Some(row) = self.scrollback.get(global) {
                    self.history_window.push(row);
                }
            }
        }
        self.history_window_valid = true;
    }

    /// Borrows a row out of the synced window. `index` is the offset from
    /// the window base (in `cell(row, ..)` terms, just `row`).
    pub(crate) fn history_window_row(&self, index: usize) -> Option<&Row> {
        debug_assert!(
            self.history_window_valid,
            "history window must be synced before &self history reads"
        );
        self.history_window.get(index)
    }

    /// Initialize tab stops every 8 columns.
    ///
    /// (Moved out of grid/mod.rs: T2 added the window fields against a file
    /// already at its line ceiling; this self-contained helper is the
    /// compensating move.)
    pub(crate) fn init_tabstops(cols: usize) -> Vec<bool> {
        let mut stops = vec![false; cols + 1];
        for i in (0..cols).step_by(8) {
            stops[i] = true;
        }
        stops
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A grid with `count` single-char rows pushed straight into history.
    fn grid_with_history(count: u8, num_rows: usize, cols: usize) -> Grid {
        let mut grid = Grid::with_scrollback(num_rows, cols, 100);
        for c in b'a'..b'a' + count {
            let mut row = Row::new(cols);
            row.cells[0].character = c as char;
            grid.push_history_row(row);
        }
        grid
    }

    #[test]
    fn window_holds_the_oldest_visible_slice_bounded_by_num_rows() {
        let mut grid = grid_with_history(9, 3, 6);
        assert_eq!(grid.scrollback.len(), 9);

        // Scroll to the very top: offset 9 → window base = global 0; only
        // the first num_rows rows are observable via cell().
        grid.set_scroll_offset(9);
        assert_eq!(grid.history_window.len(), 3);
        assert_eq!(grid.cell(0, 0).character, 'a');
        assert_eq!(grid.cell(1, 0).character, 'b');
        assert_eq!(grid.cell(2, 0).character, 'c');

        // Scrolling less than a viewport: window = exactly `offset` rows,
        // starting at global sb_len - offset.
        grid.set_scroll_offset(2);
        assert_eq!(grid.history_window.len(), 2);
        assert_eq!(grid.cell(0, 0).character, 'h');
        assert_eq!(grid.cell(1, 0).character, 'i');
        assert_eq!(
            grid.cell(2, 0).character,
            ' ',
            "row 2 falls through to the live viewport"
        );
    }

    #[test]
    fn push_invalidates_and_next_sync_rebuilds() {
        let mut grid = grid_with_history(1, 2, 4);
        assert!(!grid.history_window_valid, "push must invalidate");

        grid.set_scroll_offset(1);
        assert_eq!(grid.cell(0, 0).character, 'a');

        // A second push (what scroll_up does mid-stream) invalidates again;
        // the next sync point materializes the fresh slice.
        let mut row = Row::new(4);
        row.cells[0].character = 'b';
        grid.push_history_row(row);
        assert!(!grid.history_window_valid);

        grid.sync_history_window();
        assert!(grid.history_window_valid);
        // Offset is still 1 → the window shows the NEWEST row.
        assert_eq!(grid.cell(0, 0).character, 'b');
        assert_eq!(grid.cell(1, 0).character, ' ', "live viewport tail");
    }

    #[test]
    fn setter_stores_unclamped_but_window_clamps() {
        let mut grid = Grid::with_scrollback(2, 4, 100);
        // Old field semantics: an offset past history is stored as-is (the
        // policy gates read it); the window materializes the clamped slice.
        grid.set_scroll_offset(7);
        assert_eq!(grid.scroll_offset(), 7);
        assert!(grid.history_window.is_empty(), "clamped view: no history");

        let mut row = Row::new(4);
        row.cells[0].character = 'x';
        grid.push_history_row(row);
        grid.set_scroll_offset(5);
        assert_eq!(grid.scroll_offset(), 5);
        assert_eq!(grid.history_window.len(), 1, "clamped to scrollback.len()");
        assert_eq!(grid.cell(0, 0).character, 'x');

        // Setting the same offset again is a no-op (window stays valid).
        grid.set_scroll_offset(5);
        assert!(grid.history_window_valid);
        assert_eq!(grid.history_window.len(), 1);
    }

    #[test]
    fn retention_shrink_keeps_window_consistent() {
        let mut grid = grid_with_history(5, 2, 4);
        grid.set_scroll_offset(5);
        assert_eq!(grid.cell(0, 0).character, 'a');
        assert_eq!(grid.cell(1, 0).character, 'b');

        // Shrink to 2: the three oldest rows are evicted. The offset is
        // preserved unclamped (old field semantics — readers clamp), so the
        // clamped view is 2 and the window shows the retained suffix.
        grid.set_scrollback_max_lines(2);
        assert_eq!(grid.scrollback.len(), 2);
        assert_eq!(grid.scroll_offset(), 5);
        assert_eq!(grid.cell(0, 0).character, 'd');
        assert_eq!(grid.cell(1, 0).character, 'e');
    }
}
