//! v1.10.20: viewport row → snapshot line index mapping for the primary
//! history snapshot (drag-selection anchor migration).
//!
//! [`Grid::snapshot_line_index_for_viewport_row`] REPLAYS the exact walk of
//! the snapshot builder in [`super::snapshot`]
//! (`document_snapshot_with_url_resolver`) — the same empty-row skip +
//! `pending_empty` collapse and the same text-budget abort — to answer
//! "which snapshot line does this live viewport row land on" WITHOUT
//! building the snapshot text. The mapping must match the snapshot the
//! history BlockView renders; `viewport_row - viewport_start` arithmetic
//! drifts on every blank row.
//!
//! The rules stay single-sourced with the builder, not copied:
//! - [`super::snapshot::SNAPSHOT_TEXT_BUDGET`] — the text-budget abort bound
//! - [`snapshot_row_extent`] — which cells count as text (the builder's
//!   `styled_row` imports this helper, so both walks share the rule)
//! - the regression tests below cross-check the mapping against the real
//!   builder output (`document_snapshot_from_position_with_resolver` /
//!   `_with_ownership_masks`), so any drift between the two walks fails.

use super::snapshot::SNAPSHOT_TEXT_BUDGET;
use super::{CellFlags, Grid, Row};

/// Shared cell-extent rule: last non-blank cell of a row's snapshot text
/// (cells beyond it are not part of the text). `pub(crate)` so the snapshot
/// builder (`styled_row` in `super::snapshot`) counts the same cells as this
/// replay — the line mapping can't drift from the snapshot it maps into.
///
/// v1.10.23 (FIX_OMP_CONTENT_LOSS): the extent is the row's OWN width, not
/// `num_cols` — narrowing resizes now leave scrollback rows at their original
/// width (see `Scrollback::resize_cols`), and the snapshot must contain the
/// complete history line. Viewport rows are always exactly `num_cols` wide,
/// so for them the two widths coincide.
pub(crate) fn snapshot_row_extent(row: &Row, _num_cols: usize) -> usize {
    row.cells
        .iter()
        .rposition(|cell| cell.character != ' ' && cell.character != '\0')
        .map(|index| index + 1)
        .unwrap_or(0)
}

impl Grid {
    /// Replay the snapshot walk — the exact same skip/filter rules as
    /// [`Grid::document_snapshot_with_url_resolver`] in `super::snapshot` —
    /// and return the snapshot line index that viewport row `viewport_row`
    /// lands on.
    ///
    /// This is NOT `viewport_row - viewport_start` arithmetic: empty rows are
    /// skipped and `pending_empty` collapses them into the next non-empty
    /// line, so a direct subtraction drifts on every blank row. Used by the
    /// drag-selection anchor migration (grid selection → primary history
    /// BlockView selection), where the mapping must exactly match the
    /// snapshot that `primary_screen_document_snapshot` built.
    ///
    /// Returns `None` when the row is unowned (ownership filter), empty
    /// (skipped by the walk), outside the walked range, or the snapshot
    /// would have aborted (text budget) before reaching it.
    pub fn snapshot_line_index_for_viewport_row(
        &self,
        viewport_row: usize,
        scrollback_start: usize,
        viewport_start: usize,
        scrollback_owned: Option<&[bool]>,
        viewport_owned: Option<&[bool]>,
    ) -> Option<usize> {
        if viewport_row >= self.num_rows {
            return None;
        }
        if !viewport_owned.map_or(true, |owned| {
            owned.get(viewport_row).copied().unwrap_or(false)
        }) {
            return None;
        }
        let mut state = SnapshotWalkState::default();
        // Scrollback rows precede the viewport in document order.
        for index in scrollback_start..self.scrollback.len() {
            if !scrollback_owned.map_or(true, |owned| owned.get(index).copied().unwrap_or(false)) {
                continue;
            }
            let Some(row) = self.scrollback.get(index) else {
                break;
            };
            // A non-empty row that overflows the text budget aborts the
            // snapshot; nothing after it exists in the document.
            if walk_snapshot_row(&mut state, &row, self.num_cols).is_some()
                && state.text_len > SNAPSHOT_TEXT_BUDGET
            {
                return None;
            }
        }
        let viewport_start = viewport_start.min(self.num_rows);
        for index in viewport_start..=viewport_row {
            // Intermediate viewport rows are filtered by the ownership mask
            // exactly like the snapshot builder's `filter` before the walk.
            if !viewport_owned.map_or(true, |owned| owned.get(index).copied().unwrap_or(false)) {
                continue;
            }
            let row = self.viewport.get(index)?;
            let line = walk_snapshot_row(&mut state, row, self.num_cols);
            if index == viewport_row {
                // The target row itself: even when it overflows the budget
                // its (truncated) line still exists in the snapshot text.
                return line;
            }
            if state.text_len > SNAPSHOT_TEXT_BUDGET {
                return None;
            }
        }
        None
    }
}

/// Replay state of the snapshot walk (see
/// [`Grid::document_snapshot_with_url_resolver`] in `super::snapshot`): the
/// empty-row skip + `pending_empty` collapse and the text byte count used
/// for the truncation abort.
#[derive(Default)]
struct SnapshotWalkState {
    started: bool,
    line_index: usize,
    pending_empty: usize,
    text_len: usize,
}

/// Advance the walk over one document row. Returns the row's snapshot line
/// index when it is non-empty, `None` when it is skipped as empty.
fn walk_snapshot_row(state: &mut SnapshotWalkState, row: &Row, num_cols: usize) -> Option<usize> {
    let row_len = snapshot_row_text_len(row, num_cols);
    if row_len == 0 {
        if state.started {
            state.pending_empty += 1;
        }
        return None;
    }
    if state.started {
        state.line_index = state.line_index.saturating_add(1 + state.pending_empty);
        // The snapshot pushes `pending_empty + 1` newlines (1 byte each).
        state.text_len = state
            .text_len
            .saturating_add(state.pending_empty.saturating_add(1));
    } else {
        state.started = true;
    }
    state.pending_empty = 0;
    state.text_len = state.text_len.saturating_add(row_len);
    Some(state.line_index)
}

/// Byte length of the row's snapshot text, mirroring `styled_row`'s push
/// logic exactly (skip `WIDE_SPACER`; `EXTRA` cells contribute their full
/// cluster; `\0` renders as a space).
fn snapshot_row_text_len(row: &Row, num_cols: usize) -> usize {
    let last = snapshot_row_extent(row, num_cols);
    let mut len = 0usize;
    for (col, cell) in row.cells[..last].iter().enumerate() {
        if cell.flags.contains(CellFlags::WIDE_SPACER) {
            continue;
        }
        let cluster = cell
            .flags
            .contains(CellFlags::EXTRA)
            .then(|| row.extras.grapheme_at(col))
            .flatten();
        let ch = if cell.character == '\0' {
            ' '
        } else {
            cell.character
        };
        len += cluster.map(str::len).unwrap_or_else(|| ch.len_utf8());
    }
    len
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::blocks::MAX_OUTPUT_BYTES;

    fn row(text: &str, cols: usize) -> Row {
        let mut row = Row::new(cols);
        for (index, ch) in text.chars().enumerate() {
            row.cells[index].character = ch;
        }
        row
    }

    // ── v1.10.20: snapshot_line_index_for_viewport_row ─────────────────

    /// The mapping must exactly match the snapshot text the walk builds:
    /// empty viewport rows are skipped and collapse into the next non-empty
    /// line, so `viewport_row` and snapshot line are NOT 1:1.
    #[test]
    fn viewport_row_mapping_skips_empty_rows_like_the_snapshot() {
        let mut grid = Grid::with_scrollback(4, 40, 4);
        grid.viewport[0] = row("alpha", 40);
        grid.viewport[1] = row("", 40);
        grid.viewport[2] = row("beta", 40);
        grid.viewport[3] = row("gamma", 40);
        let document_start = grid.scrollback.position();

        let (text, _, _) =
            grid.document_snapshot_from_position_with_resolver(document_start, |_| None);
        assert_eq!(text, "alpha\n\nbeta\ngamma");

        let map = |viewport_row| {
            grid.snapshot_line_index_for_viewport_row(
                viewport_row,
                grid.scrollback.index_since(document_start),
                0,
                None,
                None,
            )
        };
        assert_eq!(map(0), Some(0));
        assert_eq!(map(1), None, "empty row is skipped");
        assert_eq!(map(2), Some(2), "empty row collapses into the next line");
        assert_eq!(map(3), Some(3));
    }

    /// Leading empty rows (before the first non-empty row) are skipped
    /// without shifting the first line, and rows before a viewport_start are
    /// outside the document.
    #[test]
    fn leading_empties_do_not_shift_and_viewport_start_clips() {
        let mut grid = Grid::with_scrollback(3, 40, 3);
        grid.viewport[0] = row("", 40);
        grid.viewport[1] = row("", 40);
        grid.viewport[2] = row("only", 40);

        assert_eq!(
            grid.snapshot_line_index_for_viewport_row(0, 0, 0, None, None),
            None
        );
        assert_eq!(
            grid.snapshot_line_index_for_viewport_row(1, 0, 0, None, None),
            None
        );
        assert_eq!(
            grid.snapshot_line_index_for_viewport_row(2, 0, 0, None, None),
            Some(0),
            "leading empties never shift the first line"
        );
        // viewport_start=2: rows 0-1 are outside the document walk.
        assert_eq!(
            grid.snapshot_line_index_for_viewport_row(1, 0, 2, None, None),
            None
        );
        assert_eq!(
            grid.snapshot_line_index_for_viewport_row(2, 0, 2, None, None),
            Some(0)
        );
    }

    /// The ownership filters must skip unowned rows exactly like the
    /// snapshot's mask-aware builder (scrollback and viewport).
    #[test]
    fn ownership_filters_skip_unowned_rows_like_the_snapshot() {
        let mut grid = Grid::with_scrollback(3, 40, 3);
        grid.scrollback.push(row("unowned old", 40));
        grid.scrollback.push(row("owned page", 40));
        grid.viewport[0] = row("owned tail", 40);
        grid.viewport[1] = row("stale shell", 40);
        grid.viewport[2] = row("owned prompt", 40);

        let snapshot = grid
            .document_snapshot_from_position_with_ownership_masks(
                0,
                &[false, true],
                &[true, false, true],
            )
            .0;
        assert_eq!(snapshot, "owned page\nowned tail\nowned prompt");

        let scrollback_owned = [false, true];
        let viewport_owned = [true, false, true];
        let map = |row| {
            grid.snapshot_line_index_for_viewport_row(
                row,
                0,
                0,
                Some(&scrollback_owned),
                Some(&viewport_owned),
            )
        };
        assert_eq!(
            map(0),
            Some(1),
            "viewport row 0 follows the owned scrollback row"
        );
        assert_eq!(map(1), None, "unowned viewport row is filtered");
        assert_eq!(map(2), Some(2));
        // Without the masks, the same row maps to its plain walk position
        // (both scrollback rows are counted).
        assert_eq!(
            grid.snapshot_line_index_for_viewport_row(0, 0, 0, None, None),
            Some(2)
        );
    }

    /// The scrollback chain before the document start is walked first, so a
    /// document starting inside the scrollback shifts the viewport lines.
    #[test]
    fn document_start_inside_scrollback_counts_prior_rows() {
        let mut grid = Grid::with_scrollback(3, 40, 3);
        grid.scrollback.push(row("shell old", 40));
        grid.scrollback.push(row("banner", 40));
        grid.viewport[0] = row("header", 40);
        grid.viewport[1] = row("", 40);
        grid.viewport[2] = row("content", 40);
        let document_start = grid.scrollback.position() - 1; // start at "banner"

        let (text, _, _) =
            grid.document_snapshot_from_position_with_resolver(document_start, |_| None);
        assert_eq!(text, "banner\nheader\n\ncontent");

        let map = |row| {
            grid.snapshot_line_index_for_viewport_row(
                row,
                grid.scrollback.index_since(document_start),
                0,
                None,
                None,
            )
        };
        assert_eq!(map(0), Some(1), "banner occupies line 0");
        assert_eq!(map(1), None);
        assert_eq!(map(2), Some(3));
    }

    /// Out-of-range rows, and rows beyond a text-budget abort, map to None.
    #[test]
    fn out_of_range_and_budget_aborted_rows_map_to_none() {
        let mut grid = Grid::with_scrollback(4, 24, 4);
        grid.viewport[0] = row("a", 24);
        grid.viewport[1] = row("b", 24);
        assert_eq!(
            grid.snapshot_line_index_for_viewport_row(4, 0, 0, None, None),
            None
        );
        assert_eq!(
            grid.snapshot_line_index_for_viewport_row(9, 0, 0, None, None),
            None
        );

        // Overload the 1 MiB text budget: the snapshot aborts at the first
        // overflowing row, so rows after it don't exist.
        let cols = 1_100;
        let mut grid = Grid::with_scrollback(2, cols, 1_100);
        let full = "x".repeat(cols);
        for _ in 0..1_100 {
            grid.scrollback.push(row(&full, cols));
        }
        grid.viewport[0] = row("tail", 1_100);
        grid.viewport[1] = row("more", 1_100);
        let (text, _) = grid.document_snapshot_from(0);
        assert!(
            text.len() > MAX_OUTPUT_BYTES,
            "budget is actually exhausted"
        );
        assert_eq!(
            grid.snapshot_line_index_for_viewport_row(0, 0, 0, None, None),
            None,
            "viewport rows after the abort don't exist in the snapshot"
        );
        assert_eq!(
            grid.snapshot_line_index_for_viewport_row(1, 0, 0, None, None),
            None
        );
    }
}
