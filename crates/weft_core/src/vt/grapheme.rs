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

        let need_expand =
            new_width > old_width && self.grid.viewport[row].cells[col].width == CellWidth::Half;
        // v1.13.12 FIX-C: promotion ON the last column (`col + 1 >= num_cols`)
        // — the cluster now renders width 2 but no spacer cell exists. With
        // num_cols >= 2, straddle exactly like the wide-char print path
        // (perform.rs): deferred wrap first, then the whole cluster cell
        // moves to the next row's col 0 (Full + spacer). num_cols < 2 keeps
        // the old no-expand behavior (a Full pair has nowhere to live).
        let straddle = need_expand && num_cols >= 2 && col + 1 >= num_cols;
        {
            let cell = &mut self.grid.viewport[row].cells[col];
            cell.flags.insert(CellFlags::EXTRA | CellFlags::DIRTY);
        }
        if straddle {
            self.straddle_expanded_cluster(row, col);
            return true;
        }
        if need_expand && col + 1 < num_cols {
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
            // v1.15.3 P1a: the new spacer is non-default content (combining-
            // mark promotion path).
            self.grid.viewport[row].cell_written(col + 1);
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

    /// v1.13.12 FIX-C: a cluster that just grew to width 2 while sitting on
    /// the LAST column moves to the next row's col 0. Mirrors the wide-char
    /// straddle in `perform.rs` (`deferred_wrap_newline` + the
    /// primary-screen exit/ownership hooks + hyperlink registry / extras
    /// migration, spacer half included).
    pub(super) fn straddle_expanded_cluster(&mut self, row: usize, col: usize) {
        // Snapshot everything the move carries BEFORE the wrap: at the
        // scroll-region bottom `deferred_wrap_newline` scrolls the grid and
        // invalidates the `row` index.
        let cell = self.grid.viewport[row].cells[col];
        let cluster = self.grid.viewport[row].extras.grapheme_arc_at(col);
        let link = self.grid.viewport[row].extras.hyperlink_id_at(col);

        // Vacate the old slot — the cluster now lives on the next row.
        self.grid.viewport[row].cells[col].reset();
        self.grid.viewport[row].extras.clear_cell(col);
        self.hyperlinks.unlink_cell(row, col);
        self.grid.viewport[row].mark_dirty(col);
        // v1.15.3 reviewer P3-1: a vacated tail slot would leave occ one
        // high until the next tail event (safe direction, but keep exact).
        self.grid.viewport[row].recompute_tail();

        self.deferred_wrap_newline();
        let new_row = self.grid.cursor.row;
        let new_col = self.grid.cursor.col; // 0 right after the wrap
        self.prepare_primary_screen_exit_row_overwrite();
        self.include_primary_screen_viewport_row(new_row);

        // T3 review P1 (P2-1): mirror the wrap-first print site — detect a
        // bisected wide pair's FAR half BEFORE the clears mutate the row.
        // `clear_wide_pair_at` resets cells + extras but NOT the hyperlink
        // registry side-map, so a linked lead at new_col+1 whose spacer sits
        // at new_col+2 leaves a stale clickable link behind unless it is
        // unlinked here.
        let num_cols = self.grid.num_cols;
        let next_bisected = new_col + 2 < num_cols
            && self.grid.viewport[new_row].cells[new_col + 1].width == CellWidth::Full
            && self.grid.viewport[new_row].cells[new_col + 2]
                .flags
                .contains(CellFlags::WIDE_SPACER);

        // Destination hygiene, same as the wrap-first print site: clear
        // stale pair halves and orphaned extras before the write.
        self.grid.viewport[new_row].clear_wide_pair_at(new_col);
        self.grid.viewport[new_row].clear_wide_pair_at(new_col + 1);
        self.grid.viewport[new_row].extras.clear_grapheme(new_col);
        self.grid.viewport[new_row]
            .extras
            .clear_grapheme(new_col + 1);
        {
            let cells = &mut self.grid.viewport[new_row].cells;
            let lead = &mut cells[new_col];
            lead.character = cell.character;
            lead.fg = cell.fg;
            lead.bg = cell.bg;
            lead.flags = cell.flags | CellFlags::DIRTY;
            lead.width = CellWidth::Full;
            lead.underline_style = cell.underline_style;
            lead.underline_color = cell.underline_color;
            let spacer = &mut cells[new_col + 1];
            spacer.character = ' ';
            spacer.flags = CellFlags::WIDE_SPACER;
            spacer.width = CellWidth::Half;
        }
        // Re-attach the cluster string and the hyperlink on the new row;
        // the spacer carries the lead's link (T3 P3-2 two-half hover shape).
        if let Some(cluster) = cluster {
            self.grid.viewport[new_row]
                .extras
                .set_grapheme(new_col, cluster);
        }
        match link {
            Some(id) => {
                self.hyperlinks.link_cell(new_row, new_col, id);
                self.hyperlinks.link_cell(new_row, new_col + 1, id);
            }
            None => {
                self.hyperlinks.unlink_cell(new_row, new_col);
                self.hyperlinks.unlink_cell(new_row, new_col + 1);
            }
        }
        self.grid.viewport[new_row]
            .extras
            .set_hyperlink(new_col, link);
        self.grid.viewport[new_row]
            .extras
            .set_hyperlink(new_col + 1, link);
        {
            let cells = &mut self.grid.viewport[new_row].cells;
            match link {
                Some(_) => {
                    cells[new_col].flags |= CellFlags::HYPERLINK;
                    cells[new_col + 1].flags |= CellFlags::HYPERLINK;
                }
                None => {
                    cells[new_col].flags.remove(CellFlags::HYPERLINK);
                    cells[new_col + 1].flags.remove(CellFlags::HYPERLINK);
                }
            }
        }
        self.grid.viewport[new_row].mark_dirty(new_col);
        self.grid.viewport[new_row].mark_dirty(new_col + 1);
        // v1.15.3 P1a: the relocated pair is non-default content (lead carries
        // the cluster, spacer the WIDE_SPACER flag).
        self.grid.viewport[new_row].cell_written(new_col);
        self.grid.viewport[new_row].cell_written(new_col + 1);
        // T3 review P1: the bisected far half's registry entry dies with
        // its pair (extras were handled by clear_wide_pair_at).
        if next_bisected {
            self.hyperlinks.unlink_cell(new_row, new_col + 2);
        }

        // Cursor lands past the moved pair, clamped like the print path
        // (num_cols == 2 → col 2 → wrap_pending + clamp to 1).
        self.grid.cursor.col = new_col + 2;
        if self.grid.cursor.col >= self.grid.num_cols {
            self.grid.cursor.wrap_pending = true;
            self.grid.cursor.col = self.grid.num_cols - 1;
        }
    }
}
