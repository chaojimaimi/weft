//! Test-only helpers for the flat storage modules (port of Warp's
//! `testing.rs`): a string → weft-Rows builder that mirrors the VT print
//! path's layout semantics, plus materialized-row equality assertions.
//! Compiles under cfg(test) only — zero production footprint.

use std::sync::Arc;

use unicode_segmentation::UnicodeSegmentation as _;

use super::grapheme::Grapheme;
use super::FlatStorage;
use crate::grid::cell::{CellFlags, CellWidth};
use crate::grid::row::Row;

/// Asserts that materialized rows equal the expected rows cell-for-cell:
/// characters, colors, style flags, widths, underline slots, derived flags
/// (`WIDE_SPACER` / `HYPERLINK` / `EXTRA`), and the sparse `RowExtras` map.
pub(crate) fn assert_rows_equal(actual: &[Row], expected: &[Row], msg: &str) {
    assert_eq!(
        actual.len(),
        expected.len(),
        "{msg}: expected {} rows but got {}",
        expected.len(),
        actual.len(),
    );
    for (row_idx, (a, e)) in actual.iter().zip(expected.iter()).enumerate() {
        assert_eq!(a.wrapped, e.wrapped, "{msg}: row {row_idx} wrapped flag");
        assert_eq!(a.cells.len(), e.cells.len(), "{msg}: row {row_idx} width");
        for (col, (ac, ec)) in a.cells.iter().zip(e.cells.iter()).enumerate() {
            assert_eq!(
                ac.character, ec.character,
                "{msg}: ({row_idx},{col}) character"
            );
            assert_eq!(ac.fg, ec.fg, "{msg}: ({row_idx},{col}) fg");
            assert_eq!(ac.bg, ec.bg, "{msg}: ({row_idx},{col}) bg");
            // DIRTY is the write-marker (D2: not persisted; materialization
            // re-adds it per the VT print convention) — compare modulo it.
            let flag_mask = CellFlags::DIRTY.complement();
            assert_eq!(
                ac.flags & flag_mask,
                ec.flags & flag_mask,
                "{msg}: ({row_idx},{col}) flags"
            );
            assert_eq!(ac.width, ec.width, "{msg}: ({row_idx},{col}) width");
            assert_eq!(
                ac.underline_style, ec.underline_style,
                "{msg}: ({row_idx},{col}) underline style"
            );
            assert_eq!(
                ac.underline_color, ec.underline_color,
                "{msg}: ({row_idx},{col}) underline color"
            );
        }
        assert_eq!(a.extras, e.extras, "{msg}: row {row_idx} extras");
    }
}

impl FlatStorage {
    /// Encodes `string` (wrapped at the storage's column count) and pushes it.
    pub(crate) fn push_rows_from_string(&mut self, string: &str) {
        let rows = to_rows(string, self.columns);
        self.extend(rows);
    }
}

/// Builds a storage pre-filled with `content` (used to port Warp's
/// `from_content_using_rows`; the allocation-hint parameter was dropped —
/// it only pre-sized a VecDeque).
pub(crate) fn from_content(content: &str, columns: usize) -> FlatStorage {
    let mut storage = FlatStorage::new(columns, usize::MAX);
    storage.push_rows_from_string(content);
    storage
}

/// Lays a string out into weft [`Row`]s exactly as the VT print path would:
/// wide chars take two cells (spacer flagged), multi-scalar clusters land in
/// `RowExtras` with `EXTRA` set, soft wraps set `wrapped`, newlines terminate
/// rows. This is the independent reference for `Index::rebuild` equivalence
/// tests.
pub(crate) fn to_rows(s: &str, columns: usize) -> Vec<Row> {
    assert!(columns >= 2, "reference layout needs at least 2 columns");
    let mut rows = vec![Row::new(columns)];
    let mut needs_new_row = false;
    // Cells written in the newest row (0 = nothing yet).
    let mut occupied = 0usize;

    for grapheme_str in s.graphemes(true) {
        let grapheme = Grapheme::new_from_str(grapheme_str);

        if needs_new_row {
            needs_new_row = false;
            rows.push(Row::new(columns));
            occupied = 0;
        }

        if grapheme.starts_new_row() {
            // Don't append an extra empty row for a trailing newline.
            needs_new_row = true;
            continue;
        }

        let cell_width = grapheme.cell_width() as usize;
        if cell_width == 0 {
            continue;
        }

        if occupied + cell_width > columns {
            // Soft wrap: the abandoned row is a wrapped continuation. A wide
            // char moves to the next row whole — the leftover cell stays a
            // blank default (weft has no leading-spacer cell).
            rows.last_mut().expect("rows is never empty").wrapped = true;
            rows.push(Row::new(columns));
            occupied = 0;
        }

        let col = occupied;
        let row = rows.last_mut().expect("rows is never empty");

        let mut chars = grapheme.chars();
        let base = chars.next().expect("grapheme is non-empty");
        let has_tail = chars.next().is_some();

        {
            let cell = &mut row.cells[col];
            cell.character = base;
            if has_tail {
                cell.flags |= CellFlags::EXTRA;
            }
        }
        if cell_width == 2 {
            row.cells[col].width = CellWidth::Full;
            row.cells[col + 1].flags |= CellFlags::WIDE_SPACER;
        }
        if has_tail {
            row.extras.set_grapheme(col, Arc::from(grapheme_str));
        }

        occupied += cell_width;
    }

    if !needs_new_row {
        // Strings that don't end in a newline must fill the last row exactly
        // — otherwise the wrap expectation would be ambiguous.
        assert!(
            occupied == columns,
            "all non-filled rows must explicitly end in a newline to avoid surprises and incorrect tests"
        );
        rows.last_mut().expect("rows is never empty").wrapped = true;
    }

    rows
}
