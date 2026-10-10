//! `print write path (`deferred_wrap_newline` / `print_ascii_run`)` bodies for the Terminal facade. vt/mod.rs keeps the struct,
//! `process()`, and core accessors (v1.13.8 S2 zero-behavior file-budget
//! split; `impl Terminal` cross-file blocks per the screen_exit /
//! kitty_keyboard precedent). Bodies moved verbatim.
use super::Terminal;
use crate::blocks::ShellPhase;
use crate::grid::{CellFlags, CellWidth};

impl Terminal {
    /// v1.11.16 (Fix B2): shared deferred-wrap newline. Clears `wrap_pending`,
    /// moves to the next row (scrolling at the region bottom), and marks the
    /// previous row `wrapped` ONLY when the cursor actually advanced or a
    /// scroll occurred. At the last physical row outside the scroll region
    /// (DECSTBM status-line layout) neither arm fires — the cursor stays put
    /// and NO row may be marked (the old code mismarked `row-1` as a
    /// continuation of unrelated content).
    pub(super) fn deferred_wrap_newline(&mut self) {
        self.grid.cursor.wrap_pending = false;
        self.grid.cursor.col = 0;
        let (_, bottom) = self.grid.scroll_region();
        let mut advanced = false;
        if self.grid.cursor.row == bottom {
            self.scroll_grid_up(1);
            advanced = true; // content moved up; cursor.row-1 is the overflowed row
        } else if self.grid.cursor.row < self.grid.num_rows - 1 {
            self.grid.cursor.row += 1;
            advanced = true;
        }
        if advanced && self.grid.cursor.row > 0 {
            self.grid.viewport[self.grid.cursor.row - 1].wrapped = true;
        }
    }

    /// v1.0 perf: Bulk-write a run of printable ASCII bytes (0x20..=0x7E)
    /// directly to the grid, bypassing vte's per-byte state machine.
    ///
    /// v1.0 P1.5-C2: Rewritten to process one row at a time instead of
    /// per-char, eliminating several sources of per-char overhead:
    /// - **Hyperlink check**: skipped entirely when no active hyperlink AND
    ///   the cell_map is empty (99.9% of output). Old code called
    ///   `unlink_cell` (HashMap::remove) for every char.
    /// - **Dirty marking**: once per row segment, not per cell.
    /// - **Bounds check**: computed once per row (`remaining_in_row`), not
    ///   checked per char.
    /// - **Block capture**: batched via `on_print_ascii_run` (one
    ///   `push_str` instead of N `push` calls).
    /// - **Cursor access**: `cursor.col` updated once per row, not per char.
    pub(super) fn print_ascii_run(&mut self, bytes: &[u8]) {
        debug_assert!(!bytes.is_empty());
        let phase = self.block_tracker.phase();
        // Snap back to live viewport for new content — same gate as print().
        // Skipped while the user is browsing primary-screen TUI history so a
        // redraw cannot yank the viewport back to the live bottom.
        if phase != ShellPhase::AtPrompt && !self.primary_history_view() {
            self.grid.set_scroll_offset(0);
        }
        let num_cols = self.grid.num_cols;
        let fg = self.attrs.fg;
        let bg = self.attrs.bg;
        let base_flags = self.attrs.flags | CellFlags::DIRTY;

        // v1.0 P1.5-C2: Determine hyperlink handling mode once.
        // - `has_hyperlink`: active OSC 8 → every cell gets linked.
        // - `need_unlink_check`: no active link, but old cells might have
        //   HYPERLINK flag → need to check + unlink (rare).
        // - Neither: skip ALL hyperlink logic (fast path, 99.9% of output).
        let has_hyperlink = self.active_hyperlink_id.is_some();
        let need_unlink_check = !has_hyperlink && !self.hyperlinks.cell_map_is_empty();

        let mut offset = 0;
        while offset < bytes.len() {
            // Handle deferred wrap (same as print()) — once per row boundary.
            if self.grid.cursor.wrap_pending {
                self.deferred_wrap_newline();
            }

            let col = self.grid.cursor.col;

            // v1.0 P1.5-C2: resize race — cursor.col may be >= num_cols after
            // a narrowing resize. Reset to col 0 and advance row (same as the
            // old per-char bounds check, but done once per row boundary).
            let col = if col >= num_cols {
                self.deferred_wrap_newline();
                self.grid.cursor.col
            } else {
                col
            };
            // Read row AFTER the col adjustment (cursor.row may have changed).
            let row = self.grid.cursor.row;
            self.prepare_primary_screen_exit_row_overwrite();
            self.include_primary_screen_viewport_row(row);

            // How many bytes fit in the current row? No per-char bounds check.
            let remaining_in_row = num_cols - col;
            let remaining_bytes = bytes.len() - offset;
            let count = remaining_in_row.min(remaining_bytes);
            let chunk = &bytes[offset..offset + count];

            // Batch capture to the active sink — one push instead of N pushes.
            // v1.7.0-A: capture the current VT SGR attrs for the whole ASCII
            // run — all bytes share one style since the fast path only fires
            // when no SGR change occurred mid-run.
            // FIX_ORPHAN_PARSE_ERROR_OUTPUT: sink selection lives in
            // staging.rs — in-flight capture while CommandExecuting, preexec
            // staging between editor submit and 133;B.
            {
                let style = self.capture_style();
                self.capture_print_ascii_run(chunk, style);
            }
            {
                let style = self.capture_style();
                self.capture_primary_screen_interrupt_ascii(chunk, style);
            }

            // The contiguous ASCII overwrite can only split a pre-existing
            // wide glyph at its two boundaries. Pairs fully inside the range
            // are overwritten together, so repairing the first and last cell
            // preserves the row invariant without adding per-cell overhead.
            self.grid.viewport[row].clear_wide_pair_at(col);
            self.grid.viewport[row].clear_wide_pair_at(col + count - 1);

            // Write cells — tight inner loop, no per-cell wrap/bounds check.
            {
                let cells = &mut self.grid.viewport[row].cells;
                if has_hyperlink {
                    let id = self.active_hyperlink_id.unwrap();
                    let link_flags = base_flags | CellFlags::HYPERLINK;
                    for (i, &b) in chunk.iter().enumerate() {
                        let c = col + i;
                        cells[c].character = b as char;
                        cells[c].fg = fg;
                        cells[c].bg = bg;
                        cells[c].flags = link_flags;
                        cells[c].width = CellWidth::Half;
                        // v1.11.3 (PLAN_v1113 §1.1): carry underline
                        // style/color (cells are field-assigned, not
                        // default-constructed — stale values would survive).
                        cells[c].underline_style = self.attrs.underline_style;
                        cells[c].underline_color = self.attrs.underline_color;
                    }
                    // Batch-link all cells at once (borrow released).
                    for i in 0..count {
                        self.hyperlinks.link_cell(row, col + i, id);
                    }
                    // v1.6.1: also write to RowExtras for persistence/scrollback.
                    // Hyperlinks are rare (OSC 8 active), so per-cell BTreeMap
                    // insert is acceptable here — the hot ASCII fast path below
                    // never touches extras.
                    // v1.6.0 review C1: also clear orphaned grapheme extras for
                    // the overwritten range (ASCII overwrites don't extend clusters).
                    {
                        let extras = &mut self.grid.viewport[row].extras;
                        extras.clear_grapheme_range(col, col + count);
                        for i in 0..count {
                            extras.set_hyperlink(col + i, Some(id));
                        }
                    }
                } else if need_unlink_check {
                    // Slow path: some cells might have old hyperlinks to clean.
                    // Collect positions first, then unlink after writing.
                    let mut to_unlink: [usize; 128] = [0; 128];
                    let mut unlink_n = 0;
                    for (i, &b) in chunk.iter().enumerate() {
                        let c = col + i;
                        if cells[c].flags.contains(CellFlags::HYPERLINK) {
                            to_unlink[unlink_n] = c;
                            unlink_n += 1;
                        }
                        cells[c].character = b as char;
                        cells[c].fg = fg;
                        cells[c].bg = bg;
                        cells[c].flags = base_flags;
                        cells[c].width = CellWidth::Half;
                        // v1.11.3 (PLAN_v1113 §1.1): carry underline
                        // style/color (see hyperlink path).
                        cells[c].underline_style = self.attrs.underline_style;
                        cells[c].underline_color = self.attrs.underline_color;
                    }
                    for &c in to_unlink.iter().take(unlink_n) {
                        self.hyperlinks.unlink_cell(row, c);
                    }
                    // v1.6.1: clear extras for unlinked cells too.
                    // v1.6.0 review C1: also clear orphaned grapheme extras
                    // for the full overwritten range (not just unlinked cells).
                    {
                        let extras = &mut self.grid.viewport[row].extras;
                        for &c in to_unlink.iter().take(unlink_n) {
                            extras.set_hyperlink(c, None);
                        }
                        extras.clear_grapheme_range(col, col + count);
                    }
                } else {
                    // Fast path: no hyperlink logic at all.
                    for (i, &b) in chunk.iter().enumerate() {
                        let c = col + i;
                        cells[c].character = b as char;
                        cells[c].fg = fg;
                        cells[c].bg = bg;
                        cells[c].flags = base_flags;
                        cells[c].width = CellWidth::Half;
                        // v1.11.3 (PLAN_v1113 §1.1): carry underline
                        // style/color (see hyperlink path).
                        cells[c].underline_style = self.attrs.underline_style;
                        cells[c].underline_color = self.attrs.underline_color;
                    }
                    // v1.6.0 review C1: clear orphaned grapheme extras for the
                    // overwritten range. Common case: extras is empty (no
                    // multi-scalar clusters on this row) → single is_empty()
                    // check, zero per-cell cost.
                    let extras = &mut self.grid.viewport[row].extras;
                    if !extras.is_empty() {
                        extras.clear_grapheme_range(col, col + count);
                    }
                }
            }

            // Mark dirty once for the whole row segment — was per-cell.
            self.grid.viewport[row].mark_dirty(col + count - 1);

            self.grid.cursor.col += count;
            offset += count;

            // Handle end-of-row wrap.
            if self.grid.cursor.col >= num_cols {
                self.grid.cursor.wrap_pending = true;
                self.grid.cursor.col = num_cols - 1;
            }
        }
    }
}
