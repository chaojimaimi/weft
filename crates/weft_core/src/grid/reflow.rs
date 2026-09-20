//! FIX-β (docs/FIX_DRAG_RESIZE_STUTTER.md): allocation-focused helpers for
//! [`Grid::resize`](super::Grid::resize)'s reflow pipeline.
//!
//! The 3B move-reflow left ~124MB of allocations per reflow step at 10k
//! scrollback: every wrapped row was built by `Row::new(new_cols)`
//! (~48MB/step) and every logical line's merge buffer grew from zero by
//! doubling (~2× the line's final length, ~24MB/step). Both are fixed at the
//! buffer level here — cell clone semantics, the two-pass structure, and the
//! Phase 2/3 boundary are untouched.

use super::{Cell, Row, RowExtras};

/// Content extent of one row: trailing BLANK cells (never-written defaults)
/// are trimmed. This matters for wrapped rows too: when a full-width char
/// would straddle the right margin the print path wraps *before* placing it,
/// leaving the last cell as a never-written default. Treating that cell as
/// content (the old `row.cells.len()` for wrapped rows) baked a phantom
/// space into the logical line on every reflow, compounding into growing
/// gaps between CJK characters. A written space is preserved because writes
/// always set the DIRTY flag, so `!flags.is_empty()` keeps it.
fn row_content_end(row: &Row) -> usize {
    row.cells
        .iter()
        .rposition(|c| c.character != ' ' || !c.flags.is_empty())
        .map_or(0, |i| i + 1)
}

/// Resize a single row's cell vector to `new_cols` in place: truncate if
/// narrower, pad with default (blank) cells if wider. No content is moved
/// between rows — this preserves the app's per-cell layout exactly, which is
/// the point of the dimension-only alt-screen resize.
///
/// (Moved from grid/mod.rs: T2 added the flat window fields against a file
/// already at its line ceiling; this self-contained helper is the
/// compensating move.)
pub(super) fn resize_row_cells(cells: &mut Vec<Cell>, new_cols: usize) {
    if cells.len() == new_cols {
        return;
    }
    if cells.len() > new_cols {
        cells.truncate(new_cols);
    } else {
        let extra = new_cols - cells.len();
        cells.extend(std::iter::repeat_with(Cell::default).take(extra));
    }
}

/// β-2 pre-scan result. `content_ends` has one entry per Phase 1 row (the
/// exact content extent Phase 2 consumes, so it never re-scans); `line_caps`
/// has one entry per Phase 2 line boundary (the exact cell total of the
/// logical line that boundary opens).
pub(super) struct ReflowPrescan {
    pub(super) content_ends: Vec<usize>,
    pub(super) line_caps: Vec<usize>,
}

/// β-2: read-only pre-scan of the rows gathered by Phase 1 — zero cell
/// copies. The grouping replicates Phase 2's
/// `prev_wrapped && !merge_buf.is_empty()` continuation guard (a group is
/// open only while accumulated cells > 0), so the two passes cannot diverge;
/// and even if a future edit broke that parity, the capacities are advisory
/// — Phase 2 would fall back to natural Vec doubling and lose only
/// precision, never correctness.
pub(super) fn prescan(all_rows: &[Row]) -> ReflowPrescan {
    let mut content_ends = Vec::with_capacity(all_rows.len());
    let mut line_caps = Vec::new();
    let mut scan_len = 0usize;
    let mut prev_wrapped = false;
    // The first row is always a boundary (prev_wrapped starts false) and
    // OPENS the first line; its capacity is recorded at the boundary that
    // closes it (or after the loop), so no phantom pre-content entry exists.
    let mut first_row = true;
    for row in all_rows {
        let content_end = row_content_end(row);
        content_ends.push(content_end);
        let is_continuation = prev_wrapped && scan_len > 0;
        prev_wrapped = row.wrapped;
        if !is_continuation {
            if first_row {
                first_row = false;
            } else {
                line_caps.push(scan_len);
            }
            scan_len = 0;
        }
        scan_len += content_end;
    }
    if !first_row {
        // The last opened line closes here (Phase 2 flushes it post-loop).
        line_caps.push(scan_len);
    }
    ReflowPrescan {
        content_ends,
        line_caps,
    }
}

/// β-1: draw a wrapped row from the pool of old rows (Phase 2 pushes each
/// row here after merging its cells), falling back to a fresh row when the
/// pool is exhausted — deep narrowing can wrap roughly 2× as many rows as
/// the old document held. Buffer identity cycles across steps: pooled rows
/// land in scrollback/viewport via Phase 4 and return here on the NEXT
/// resize, so a full narrow/widen oscillation converges to near-zero
/// reallocation. The first widening step still reallocs by design — a row
/// built at the old width has capacity == old width, and "rows only grow"
/// (the v1.10.26 history invariant) does not cover growing into a larger
/// capacity.
pub(super) fn recycled_row(pool: &mut Vec<Row>, new_cols: usize) -> Row {
    match pool.pop() {
        Some(row) => reset_pooled_row(row, new_cols),
        None => Row::new(new_cols),
    }
}

/// Reset a recycled old row to be field-identical to `Row::new(new_cols)`.
///
/// - `cells`: cleared, then default-filled to `new_cols`. `clear` + `resize`
///   never shrink capacity, so the oscillation convergence above holds.
///   Clearing (rather than only extending) is required for equivalence:
///   Phase 3 writes cells only up to the line's content end, so any stale
///   head/tail cell would render ghost content.
/// - `dirty_occ`: zeroed. A stale value survives [`Row::mark_dirty`]'s
///   max semantics and would silently widen repaints.
/// - `wrapped`: cleared. Phase 3's wrap branches set it per row, but the
///   reset is the guarantee (a recycled row can arrive with `wrapped=true`).
/// - `extras`: cleared. Redundant today (every `wrapped_rows.push` site
///   assigns `current.extras` first) — kept so the reset alone satisfies the
///   field-by-field equivalence contract with `Row::new`.
pub(super) fn reset_pooled_row(mut row: Row, new_cols: usize) -> Row {
    row.cells.clear();
    row.cells.resize(new_cols, Cell::default());
    row.dirty_occ = 0;
    row.wrapped = false;
    row.extras = RowExtras::new();
    row
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grid::{CellExtra, CellFlags, CellWidth};

    fn row_with_text(cols: usize, text: &str, wrapped: bool) -> Row {
        let mut row = Row::new(cols);
        for (col, ch) in text.chars().enumerate() {
            row.cells[col].character = ch;
        }
        row.wrapped = wrapped;
        row
    }

    // ── β-2 prescan: grouping must mirror Phase 2 exactly ────────────────

    #[test]
    fn prescan_sums_wrapped_runs_into_one_capacity_per_boundary() {
        let rows = [
            row_with_text(8, "hello", false),
            row_with_text(8, "world", true),
            row_with_text(8, "abc!", false),
        ];
        let pre = prescan(&rows);
        assert_eq!(pre.content_ends, vec![5, 5, 4]);
        // Boundary 0 opens [hello]; boundary 1 opens [world + abc!].
        assert_eq!(pre.line_caps, vec![5, 9]);
    }

    #[test]
    fn prescan_empty_rows_yield_no_caps_and_no_entries() {
        let pre = prescan(&[]);
        assert!(pre.content_ends.is_empty());
        assert!(pre.line_caps.is_empty());
    }

    #[test]
    fn prescan_blank_row_inside_wrapped_run_stays_in_the_group() {
        // Phase 2's continuation guard is `prev_wrapped && !merge_buf
        // .is_empty()`: the blank row arrives with the group still open, so
        // it merges (adding zero cells). The prescan must agree.
        let rows = [row_with_text(8, "abcd", true), Row::new(8)];
        let pre = prescan(&rows);
        assert_eq!(pre.content_ends, vec![4, 0]);
        assert_eq!(pre.line_caps, vec![4]);
    }

    #[test]
    fn prescan_leading_blank_row_opens_an_empty_line() {
        // A blank first row opens a line that stays empty (Phase 2 never
        // flushes it) — the capacity recorded for that boundary is 0, and
        // `Vec::with_capacity(0)` does not allocate.
        let rows = [Row::new(8), row_with_text(8, "tail", false)];
        let pre = prescan(&rows);
        assert_eq!(pre.content_ends, vec![0, 4]);
        assert_eq!(pre.line_caps, vec![0, 4]);
    }

    #[test]
    fn prescan_trailing_wrapped_flag_yields_a_single_line() {
        let rows = [row_with_text(8, "solo", true)];
        let pre = prescan(&rows);
        assert_eq!(pre.line_caps, vec![4]);
    }

    // ── β-1 recycle: reused row ≡ Row::new(new_cols), field by field ─────

    /// A poisoned row: stale content, stale dirty high-water mark, stale
    /// wrapped flag, stale extras.
    fn poisoned_row() -> Row {
        let mut row = Row::new(8);
        for cell in &mut row.cells {
            cell.character = 'x';
            cell.width = CellWidth::Full;
        }
        row.mark_dirty(7);
        row.wrapped = true;
        row.extras.set(
            3,
            CellExtra {
                grapheme: Some("漢".into()),
                ..CellExtra::default()
            },
        );
        row
    }

    fn assert_field_equivalent(actual: &Row, expected: &Row) {
        assert_eq!(actual.cells.len(), expected.cells.len(), "cells len");
        for (i, (a, b)) in actual.cells.iter().zip(expected.cells.iter()).enumerate() {
            assert_eq!(a.character, b.character, "col {i} character");
            assert_eq!(a.fg, b.fg, "col {i} fg");
            assert_eq!(a.bg, b.bg, "col {i} bg");
            assert_eq!(a.flags, b.flags, "col {i} flags");
            assert_eq!(a.width, b.width, "col {i} width");
            assert_eq!(a.underline_style, b.underline_style, "col {i} underline");
            assert_eq!(a.underline_color, b.underline_color, "col {i} ucolor");
        }
        assert_eq!(actual.dirty_occ, expected.dirty_occ, "dirty_occ");
        assert_eq!(actual.wrapped, expected.wrapped, "wrapped");
        assert_eq!(actual.extras, expected.extras, "extras");
    }

    #[test]
    fn reset_pooled_row_restores_field_equivalence_with_row_new() {
        let reused = reset_pooled_row(poisoned_row(), 5);
        assert_field_equivalent(&reused, &Row::new(5));
    }

    #[test]
    fn reset_pooled_row_keeps_capacity_for_oscillation() {
        // Narrowing a wider row must never shrink the cell buffer: capacity
        // retention is what lets a narrow/widen oscillation converge to
        // near-zero reallocation after one full cycle.
        let reused = reset_pooled_row(poisoned_row(), 5);
        assert!(reused.cells.capacity() >= 8);
        // And resetting an empty row up to new_cols fills with defaults.
        let grown = reset_pooled_row(Row::new(3), 6);
        assert_field_equivalent(&grown, &Row::new(6));
    }

    #[test]
    fn recycled_row_resets_pooled_rows_and_falls_back_to_fresh() {
        let mut pool = vec![poisoned_row()];
        let reused = recycled_row(&mut pool, 4);
        assert_field_equivalent(&reused, &Row::new(4));
        assert!(pool.is_empty(), "the pooled row is consumed");
        let fresh = recycled_row(&mut pool, 7);
        assert_field_equivalent(&fresh, &Row::new(7));
    }

    /// The DIRTY-flag content predicate used by `row_content_end`: a written
    /// space (flags set) is content, a never-written default is not.
    #[test]
    fn row_content_end_trims_only_never_written_defaults() {
        let mut row = Row::new(6);
        row.cells[0].character = 'a';
        row.cells[1] = Cell {
            character: ' ',
            flags: CellFlags::DIRTY,
            ..Cell::default()
        };
        assert_eq!(row_content_end(&row), 2, "written space counts as content");
        assert_eq!(row_content_end(&Row::new(6)), 0, "all defaults trim to 0");
    }
}
