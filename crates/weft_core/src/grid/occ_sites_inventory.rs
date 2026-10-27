//! v1.15.3 (PLAN_v1153 §2.2 defense line 1): the classified inventory of
//! every Row.cells mutation site under grid/+vt/, split out of
//! `occ_mutation_sites_tests.rs` when the registry outgrew the 800-line
//! file budget (registry files are data-heavy by nature — theme.rs
//! precedent). The scan test asserts every pattern-matched source line
//! appears here with the right category; new sites must be classified or
//! the test goes red.

/// Category labels (PLAN_v1153 §2.2 five-way classification).
// Non-upper-case names keep the inventory table entries readable as the
// category vocabulary itself; clippy's naming lint is allowed here.
#[allow(non_upper_case_globals)]
pub(crate) const cell_written: &str = "cell_written";
#[allow(non_upper_case_globals)]
pub(crate) const recompute_tail: &str = "recompute_tail";
#[allow(non_upper_case_globals)]
pub(crate) const dirty_no_extend: &str = "dirty_no_extend";
#[allow(non_upper_case_globals)]
pub(crate) const erase_no_extend: &str = "erase_no_extend";
#[allow(non_upper_case_globals)]
pub(crate) const construction: &str = "construction";

/// (file relative to `src/`, trimmed line, category).
pub(crate) const CLASSIFIED: &[(&str, &str, &str)] = &[
    (
        "grid/editing.rs",
        "cells[i] = std::mem::take(&mut cells[i + shift]);",
        recompute_tail,
    ),
    (
        "grid/editing.rs",
        "cells[i] = std::mem::take(&mut cells[i - shift]);",
        recompute_tail,
    ),
    (
        "grid/editing.rs",
        "for cell in &mut row.cells {",
        erase_no_extend,
    ),
    (
        "grid/editing.rs",
        "for cell in &mut self.viewport[r].cells {",
        erase_no_extend,
    ),
    (
        "grid/editing.rs",
        "for cell in &mut self.viewport[row].cells {",
        erase_no_extend,
    ),
    (
        "grid/editing.rs",
        "let cells = &mut self.viewport[row].cells;",
        recompute_tail,
    ),
    (
        "grid/editing.rs",
        "reflow::resize_row_cells(&mut self.viewport[row].cells, self.num_cols);",
        erase_no_extend,
    ),
    (
        "grid/editing.rs",
        "row.mark_dirty(self.num_cols - 1);",
        erase_no_extend,
    ),
    ("grid/editing.rs", "self.viewport[i] =", construction),
    (
        "grid/editing.rs",
        "self.viewport[i] = Row::new(self.num_cols);",
        construction,
    ),
    (
        "grid/editing.rs",
        "self.viewport[i].mark_dirty(self.num_cols - 1);",
        erase_no_extend,
    ),
    (
        "grid/editing.rs",
        "self.viewport[r].mark_dirty(self.num_cols - 1);",
        erase_no_extend,
    ),
    (
        "grid/editing.rs",
        "self.viewport[row].mark_dirty(col);",
        erase_no_extend,
    ),
    (
        "grid/editing.rs",
        "self.viewport[row].mark_dirty(end - 1);",
        erase_no_extend,
    ),
    (
        "grid/editing.rs",
        "self.viewport[row].mark_dirty(self.num_cols - 1);",
        erase_no_extend,
    ),
    (
        "grid/editing.rs",
        "self.viewport[row].mark_dirty(self.num_cols.saturating_sub(1));",
        erase_no_extend,
    ),
    (
        "grid/flat/encode.rs",
        "let cell = &mut row.cells[col];",
        construction,
    ),
    (
        "grid/flat/encode.rs",
        "let spacer = &mut row.cells[col + 1];",
        construction,
    ),
    (
        "grid/flat/encode_fast_path_tests.rs",
        "for cell in &mut written_blank.cells {",
        construction,
    ),
    (
        "grid/flat/testing.rs",
        "let cell = &mut row.cells[col];",
        construction,
    ),
    (
        "grid/flat/tests.rs",
        "let cell = &mut r0.cells[col];",
        construction,
    ),
    (
        "grid/flat/tests.rs",
        "let cell = &mut r1.cells[col];",
        construction,
    ),
    ("grid/mod.rs", "&mut r.cells[col]", cell_written),
    (
        "grid/mod.rs",
        "for cell in &mut row.cells {",
        erase_no_extend,
    ),
    (
        "grid/mod.rs",
        "let cell = &mut self.viewport[row].cells[col];",
        cell_written,
    ),
    (
        "grid/mod.rs",
        "let spacer = &mut self.viewport[row].cells[col + 1];",
        cell_written,
    ),
    ("grid/mod.rs", "r.mark_dirty(col);", cell_written),
    (
        "grid/mod.rs",
        "reflow::resize_row_cells(&mut row.cells, new_cols);",
        construction,
    ),
    (
        "grid/mod.rs",
        "row.mark_dirty(new_cols.saturating_sub(1));",
        dirty_no_extend,
    ),
    (
        "grid/mod.rs",
        "row.mark_dirty(self.num_cols - 1);",
        dirty_no_extend,
    ),
    (
        "grid/mod.rs",
        "self.viewport[row].mark_dirty(col + 1);",
        dirty_no_extend,
    ),
    (
        "grid/mod.rs",
        "self.viewport[row].mark_dirty(col);",
        dirty_no_extend,
    ),
    (
        "grid/reflow.rs",
        "cells.extend(std::iter::repeat_with(Cell::default).take(extra));",
        construction,
    ),
    ("grid/reflow.rs", "cells.truncate(new_cols);", construction),
    (
        "grid/reflow.rs",
        "resize_row_cells(&mut cells, 2);",
        construction,
    ),
    (
        "grid/reflow.rs",
        "resize_row_cells(&mut cells, 5);",
        construction,
    ),
    ("grid/row.rs", "for cell in &mut self.cells {", construction),
    (
        "grid/row.rs",
        "self.cells.resize(num_cols, Cell::default());",
        construction,
    ),
    (
        "grid/row.rs",
        "self.cells.fill(Cell::default());",
        construction,
    ),
    ("grid/row.rs", "self.mark_dirty(col + 1);", erase_no_extend),
    ("grid/row.rs", "self.mark_dirty(col - 1);", erase_no_extend),
    ("grid/row.rs", "self.mark_dirty(col);", erase_no_extend),
    (
        "grid/scroll.rs",
        "self.viewport[i].mark_dirty(self.num_cols - 1);",
        dirty_no_extend,
    ),
    (
        "grid/snapshot/tests.rs",
        "for (index, cell) in head.cells.iter_mut().take(8).enumerate() {",
        construction,
    ),
    (
        "grid/snapshot/tests.rs",
        "for (index, cell) in styled.cells.iter_mut().enumerate() {",
        construction,
    ),
    (
        "grid/snapshot/tests.rs",
        "grid.viewport[0] = head;",
        construction,
    ),
    (
        "grid/snapshot/tests.rs",
        "grid.viewport[0] = row(\"Claude banner\", 24);",
        construction,
    ),
    (
        "grid/snapshot/tests.rs",
        "grid.viewport[0] = row(\"app header line\", 40);",
        construction,
    ),
    (
        "grid/snapshot/tests.rs",
        "grid.viewport[0] = row(\"complete answer line 2\", 40);",
        construction,
    ),
    (
        "grid/snapshot/tests.rs",
        "grid.viewport[0] = row(\"final screen\", 20);",
        construction,
    ),
    (
        "grid/snapshot/tests.rs",
        "grid.viewport[0] = row(\"first document row\", 40);",
        construction,
    ),
    (
        "grid/snapshot/tests.rs",
        "grid.viewport[0] = row(\"old shell\", 12);",
        construction,
    ),
    (
        "grid/snapshot/tests.rs",
        "grid.viewport[0] = row(\"old shell\", 24);",
        construction,
    ),
    (
        "grid/snapshot/tests.rs",
        "grid.viewport[0] = row(\"owned answer tail\", 24);",
        construction,
    ),
    (
        "grid/snapshot/tests.rs",
        "grid.viewport[0] = row(\"resume tail\", 24);",
        construction,
    ),
    (
        "grid/snapshot/tests.rs",
        "grid.viewport[0] = row(\"resume\", 24);",
        construction,
    ),
    (
        "grid/snapshot/tests.rs",
        "grid.viewport[0] = styled;",
        construction,
    ),
    (
        "grid/snapshot/tests.rs",
        "grid.viewport[1] = row(\"\", 40);",
        construction,
    ),
    (
        "grid/snapshot/tests.rs",
        "grid.viewport[1] = row(\"app content line\", 40);",
        construction,
    ),
    (
        "grid/snapshot/tests.rs",
        "grid.viewport[1] = row(\"command\", 24);",
        construction,
    ),
    (
        "grid/snapshot/tests.rs",
        "grid.viewport[1] = row(\"history\", 12);",
        construction,
    ),
    (
        "grid/snapshot/tests.rs",
        "grid.viewport[1] = row(\"stale shell footer\", 24);",
        construction,
    ),
    (
        "grid/snapshot/tests.rs",
        "grid.viewport[1] = row(\"stale shell table\", 24);",
        construction,
    ),
    (
        "grid/snapshot/tests.rs",
        "grid.viewport[1] = styled;",
        construction,
    ),
    (
        "grid/snapshot/tests.rs",
        "grid.viewport[2] = row(\"Press Ctrl-C again to exit\", 40);",
        construction,
    ),
    (
        "grid/snapshot/tests.rs",
        "grid.viewport[2] = row(\"answer one\", 24);",
        construction,
    ),
    (
        "grid/snapshot/tests.rs",
        "grid.viewport[2] = row(\"claude\", 12);",
        construction,
    ),
    (
        "grid/snapshot/tests.rs",
        "grid.viewport[2] = row(\"input prompt: \", 40);",
        construction,
    ),
    (
        "grid/snapshot/tests.rs",
        "grid.viewport[2] = row(\"input row here\", 40);",
        construction,
    ),
    (
        "grid/snapshot/tests.rs",
        "grid.viewport[2] = row(\"owned prompt\", 24);",
        construction,
    ),
    (
        "grid/snapshot/tests.rs",
        "grid.viewport[2] = row(\"restored answer\", 24);",
        construction,
    ),
    (
        "grid/snapshot/tests.rs",
        "grid.viewport[3] = row(\"\", 40);",
        construction,
    ),
    (
        "grid/snapshot/tests.rs",
        "grid.viewport[3] = row(\"answer two\", 24);",
        construction,
    ),
    (
        "grid/snapshot/tests.rs",
        "grid.viewport[3] = row(\"claude --resume session-id\", 40);",
        construction,
    ),
    (
        "grid/snapshot/tests.rs",
        "grid.viewport[3] = row(\"stale shell footer\", 24);",
        construction,
    ),
    (
        "grid/snapshot/tests.rs",
        "grid.viewport[4] = row(\"prompt\", 24);",
        construction,
    ),
    (
        "grid/snapshot/tests.rs",
        "grid.viewport[4] = row(\"startup\", 12);",
        construction,
    ),
    (
        "grid/snapshot_cursor_tests.rs",
        "grid.viewport[0] = row(\"content\", 20);",
        construction,
    ),
    (
        "grid/snapshot_cursor_tests.rs",
        "grid.viewport[0] = row(\"startup help\", 40);",
        construction,
    ),
    (
        "grid/snapshot_cursor_tests.rs",
        "grid.viewport[0] = row(\"upper\", 20);",
        construction,
    ),
    (
        "grid/snapshot_cursor_tests.rs",
        "grid.viewport[1] = row(\"----------------------------------------\", 40);",
        construction,
    ),
    (
        "grid/snapshot_cursor_tests.rs",
        "grid.viewport[1] = row(\"later content\", 20);",
        construction,
    ),
    (
        "grid/snapshot_cursor_tests.rs",
        "grid.viewport[2] = row(\"\", 20);",
        construction,
    ),
    (
        "grid/snapshot_cursor_tests.rs",
        "grid.viewport[2] = row(\"\", 40);",
        construction,
    ),
    (
        "grid/snapshot_cursor_tests.rs",
        "grid.viewport[2] = row(\"lower\", 20);",
        construction,
    ),
    (
        "grid/snapshot_cursor_tests.rs",
        "grid.viewport[3] = row(\"----------------------------------------\", 40);",
        construction,
    ),
    (
        "grid/snapshot_cursor_tests.rs",
        "grid.viewport[3] = row(\"lower\", 20);",
        construction,
    ),
    (
        "grid/snapshot_line_map.rs",
        "grid.viewport[0] = row(\"\", 40);",
        construction,
    ),
    (
        "grid/snapshot_line_map.rs",
        "grid.viewport[0] = row(\"a\", 24);",
        construction,
    ),
    (
        "grid/snapshot_line_map.rs",
        "grid.viewport[0] = row(\"alpha\", 40);",
        construction,
    ),
    (
        "grid/snapshot_line_map.rs",
        "grid.viewport[0] = row(\"header\", 40);",
        construction,
    ),
    (
        "grid/snapshot_line_map.rs",
        "grid.viewport[0] = row(\"owned tail\", 40);",
        construction,
    ),
    (
        "grid/snapshot_line_map.rs",
        "grid.viewport[0] = row(\"tail\", 1_100);",
        construction,
    ),
    (
        "grid/snapshot_line_map.rs",
        "grid.viewport[1] = row(\"\", 40);",
        construction,
    ),
    (
        "grid/snapshot_line_map.rs",
        "grid.viewport[1] = row(\"b\", 24);",
        construction,
    ),
    (
        "grid/snapshot_line_map.rs",
        "grid.viewport[1] = row(\"more\", 1_100);",
        construction,
    ),
    (
        "grid/snapshot_line_map.rs",
        "grid.viewport[1] = row(\"stale shell\", 40);",
        construction,
    ),
    (
        "grid/snapshot_line_map.rs",
        "grid.viewport[2] = row(\"beta\", 40);",
        construction,
    ),
    (
        "grid/snapshot_line_map.rs",
        "grid.viewport[2] = row(\"content\", 40);",
        construction,
    ),
    (
        "grid/snapshot_line_map.rs",
        "grid.viewport[2] = row(\"only\", 40);",
        construction,
    ),
    (
        "grid/snapshot_line_map.rs",
        "grid.viewport[2] = row(\"owned prompt\", 40);",
        construction,
    ),
    (
        "grid/snapshot_line_map.rs",
        "grid.viewport[3] = row(\"gamma\", 40);",
        construction,
    ),
    (
        "grid/tests.rs",
        "grid.viewport[0] = row.clone();",
        construction,
    ),
    (
        "grid/tests.rs",
        "grid2.viewport[0] = blank.clone();",
        construction,
    ),
    (
        "vt/actions_print.rs",
        "let cells = &mut self.grid.viewport[row].cells;",
        cell_written,
    ),
    (
        "vt/actions_print.rs",
        "self.grid.viewport[row].mark_dirty(col + count - 1);",
        cell_written,
    ),
    (
        "vt/grapheme.rs",
        "let cell = &mut self.grid.viewport[row].cells[col];",
        dirty_no_extend,
    ),
    (
        "vt/grapheme.rs",
        "let cells = &mut self.grid.viewport[new_row].cells;",
        cell_written,
    ),
    (
        "vt/grapheme.rs",
        "let lead = &mut cells[new_col];",
        cell_written,
    ),
    (
        "vt/grapheme.rs",
        "let spacer = &mut cells[new_col + 1];",
        cell_written,
    ),
    (
        "vt/grapheme.rs",
        "let spacer = &mut self.grid.viewport[row].cells[col + 1];",
        cell_written,
    ),
    (
        "vt/grapheme.rs",
        "self.grid.viewport[new_row].mark_dirty(new_col + 1);",
        dirty_no_extend,
    ),
    (
        "vt/grapheme.rs",
        "self.grid.viewport[new_row].mark_dirty(new_col);",
        dirty_no_extend,
    ),
    (
        "vt/grapheme.rs",
        "self.grid.viewport[row].mark_dirty(col + 1);",
        dirty_no_extend,
    ),
    (
        "vt/grapheme.rs",
        "self.grid.viewport[row].mark_dirty(col);",
        dirty_no_extend,
    ),
    ("vt/perform.rs", "let cell = &mut cells[col];", cell_written),
    (
        "vt/perform.rs",
        "let cell = &mut cells[new_col];",
        cell_written,
    ),
    (
        "vt/perform.rs",
        "let cells = &mut self.grid.viewport[new_row].cells;",
        cell_written,
    ),
    (
        "vt/perform.rs",
        "let cells = &mut self.grid.viewport[row].cells;",
        cell_written,
    ),
    (
        "vt/perform.rs",
        "let spacer = &mut cells[col + 1];",
        cell_written,
    ),
    (
        "vt/perform.rs",
        "let spacer = &mut cells[new_col + 1];",
        cell_written,
    ),
    (
        "vt/perform.rs",
        "self.grid.viewport[new_row].mark_dirty(new_col + 1);",
        dirty_no_extend,
    ),
    (
        "vt/perform.rs",
        "self.grid.viewport[new_row].mark_dirty(new_col);",
        dirty_no_extend,
    ),
    (
        "vt/perform.rs",
        "self.grid.viewport[row].mark_dirty(col + 1);",
        dirty_no_extend,
    ),
    (
        "vt/perform.rs",
        "self.grid.viewport[row].mark_dirty(col);",
        dirty_no_extend,
    ),
    // v1.15.3 reviewer P2-1: indexed-method and indexed-compound-field
    // mutation shapes (newly scanned). Resets are erase-form writes; the
    // HYPERLINK flag writes are content writes already covered by the
    // cell_written calls that ride the same straddle/hyperlink paths.
    (
        "grid/row.rs",
        "self.cells[col - 1].reset();",
        erase_no_extend,
    ),
    (
        "grid/row.rs",
        "self.cells[col + 1].reset();",
        erase_no_extend,
    ),
    ("grid/row.rs", "self.cells[col].reset();", erase_no_extend),
    (
        "grid/editing.rs",
        "self.viewport[row].cells[c].reset();",
        erase_no_extend,
    ),
    (
        "vt/grapheme.rs",
        "self.grid.viewport[row].cells[col].reset();",
        erase_no_extend,
    ),
    (
        "vt/grapheme.rs",
        "cells[new_col].flags |= CellFlags::HYPERLINK;",
        cell_written,
    ),
    (
        "vt/grapheme.rs",
        "cells[new_col + 1].flags |= CellFlags::HYPERLINK;",
        cell_written,
    ),
    (
        "vt/perform.rs",
        "self.grid.viewport[new_row].cells[new_col + 1].flags |= CellFlags::HYPERLINK;",
        cell_written,
    ),
    (
        "vt/perform.rs",
        "self.grid.viewport[row].cells[col + 1].flags |= CellFlags::HYPERLINK;",
        cell_written,
    ),
    // Test-fixture raw flag writes surfaced by the P2-1 pattern expansion:
    // these builders construct rows directly and normalize occ with the
    // adjacent fixture helpers (recompute_full / cell_written), pinned by
    // the encode fast-path and flat golden tests.
    (
        "grid/flat/encode_fast_path_tests.rs",
        "row.cells[1].flags |= CellFlags::EXTRA;",
        construction,
    ),
    (
        "grid/flat/encode_fast_path_tests.rs",
        "row.cells[3].flags |= CellFlags::EXTRA;",
        construction,
    ),
    (
        "grid/flat/encode_fast_path_tests.rs",
        "row.cells[4].flags |= CellFlags::WIDE_SPACER;",
        construction,
    ),
    (
        "grid/flat/encode_fast_path_tests.rs",
        "row.cells[col].flags |= CellFlags::HYPERLINK;",
        construction,
    ),
    (
        "grid/flat/encode_fast_path_tests.rs",
        "row.cells[3].flags |= CellFlags::HYPERLINK;",
        construction,
    ),
    (
        "grid/flat/encode_fast_path_tests.rs",
        "row.cells[6].flags |= CellFlags::WIDE_SPACER;",
        construction,
    ),
    (
        "grid/flat/encode_fast_path_tests.rs",
        "linked.cells[col].flags |= CellFlags::HYPERLINK;",
        construction,
    ),
    (
        "grid/flat/testing.rs",
        "row.cells[col + 1].flags |= CellFlags::WIDE_SPACER;",
        construction,
    ),
    (
        "grid/flat/tests.rs",
        "rows[0].cells[num_cols - 1].flags |= CellFlags::BOLD;",
        construction,
    ),
    (
        "grid/flat/tests.rs",
        "wide_row.cells[0].flags |= CellFlags::HYPERLINK;",
        construction,
    ),
    (
        "grid/flat/tests.rs",
        "wide_row.cells[1].flags |= CellFlags::WIDE_SPACER | CellFlags::HYPERLINK;",
        construction,
    ),
    (
        "grid/flat/tests.rs",
        "linked_row.cells[2].flags |= CellFlags::HYPERLINK;",
        construction,
    ),
    (
        "grid/flat/tests.rs",
        "row.cells[1].flags |= CellFlags::EXTRA;",
        construction,
    ),
    (
        "grid/flat/tests.rs",
        "row.cells[3].flags |= CellFlags::EXTRA;",
        construction,
    ),
    (
        "grid/flat/tests.rs",
        "row.cells[4].flags |= CellFlags::WIDE_SPACER;",
        construction,
    ),
    (
        "grid/flat/tests.rs",
        "decorated.cells[0].flags |= CellFlags::BOLD;",
        construction,
    ),
    (
        "grid/flat/tests.rs",
        "decorated.cells[2].flags |= CellFlags::ITALIC;",
        construction,
    ),
];
