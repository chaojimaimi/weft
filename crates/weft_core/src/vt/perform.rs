use super::replies;
use super::Terminal;
use crate::blocks::ShellPhase;
use crate::grid::{terminal_char_width, CellFlags, CellWidth};

impl vte::Perform for Terminal {
    fn print(&mut self, c: char) {
        // v1.0 perf: vte only calls print() in ground state, so mark it.
        self.parser_in_ground_state = true;
        // v1.0 perf: cache phase once per print() call. The phase doesn't
        // change within a single print() — it only transitions on OSC 133
        // markers, which arrive via osc_dispatch, not print.
        let phase = self.block_tracker.phase();

        // Snap back to the live viewport for new content — EXCEPT when idle at
        // an integrated prompt (AtPrompt) or when the user is deliberately
        // browsing history of a primary-screen TUI (primary_history_view).
        // In those states the app may emit re-renders or async segments that
        // would otherwise destroy the user's scroll position while they're
        // reading history. During CommandExecuting (output streaming) and in
        // non-integrated mode (plain grid terminal), new output resets scroll.
        if phase != ShellPhase::AtPrompt && !self.primary_history_view() {
            self.grid.set_scroll_offset(0);
        }

        // Feed the printed char to the active sink: in-flight block capture
        // while CommandExecuting, preexec staging between editor submit and
        // `133;B` (FIX_ORPHAN_PARSE_ERROR_OUTPUT), nothing otherwise.
        // v1.7.0-A: capture the current VT SGR attrs so the block preserves
        // program-emitted colors after the live grid scrolls away.
        {
            let style = self.capture_style();
            self.capture_print(c, style);
        }
        {
            let style = self.capture_style();
            self.capture_primary_screen_interrupt_print(c, style);
        }

        // v1.6.0: Handle the char after ZWJ. Instead of dropping it (v1.5
        // behavior), append it to the previous cell's grapheme cluster so
        // ZWJ emoji sequences (👩‍🔬, 👨‍👩‍👧) are preserved.
        if self.suppress_joined_scalar && terminal_char_width(c) > 0 {
            self.suppress_joined_scalar = false;
            self.append_scalar_to_previous_cluster(c);
            return;
        }
        self.suppress_joined_scalar = false;

        // v1.6.0: Regional indicator pair — second RI extends the first into
        // a flag cluster (🇺🇸). The first RI already consumed 2 cells; the
        // second RI appends to its extras without advancing the cursor.
        let is_regional_indicator = matches!(c, '\u{1f1e6}'..='\u{1f1ff}');
        let follows_regional_indicator = is_regional_indicator
            && self.previous_cell_position().is_some_and(|(row, col)| {
                matches!(
                    self.grid.viewport[row].cells[col].character,
                    '\u{1f1e6}'..='\u{1f1ff}'
                )
            });
        if follows_regional_indicator && self.append_scalar_to_previous_cluster(c) {
            return;
        }

        // Cell stores one base scalar. Combining marks, ZWJ and variation
        // selectors therefore cannot be retained yet, but they must never
        // consume a terminal column or trigger a deferred wrap.
        //
        // v1.6.0: Width-0 scalars (combining marks, ZWJ, VS, emoji modifiers)
        // are appended to the previous cell's RowExtras grapheme cluster
        // instead of being replaced with U+FFFD/U+FF1F fallbacks. The cell's
        // `character` keeps the lead scalar; the full cluster lives in extras.
        let scalar_width = if c.is_ascii() {
            1
        } else {
            terminal_char_width(c)
        };
        if scalar_width == 0 {
            if self.append_scalar_to_previous_cluster(c) {
                if c == '\u{200d}' {
                    self.suppress_joined_scalar = true;
                }
                return;
            }
            // No previous cell to extend — drop the combining scalar.
            return;
        }

        // Handle deferred wrap
        if self.grid.cursor.wrap_pending {
            self.deferred_wrap_newline();
        }

        // v1.0 perf: ASCII fast path — all ASCII chars are width 1.
        // This skips the unicode_width lookup for the common case (terminal
        // output is predominantly ASCII: digits, letters, punctuation).
        // For 58KB of `seq` output, this saves ~58000 lookup calls.
        let width = if scalar_width > 1 {
            CellWidth::Full
        } else {
            CellWidth::Half
        };

        let num_cols = self.grid.num_cols;
        let row = self.grid.cursor.row;
        let col = self.grid.cursor.col;

        // Wide char straddling line boundary → wrap first
        if width == CellWidth::Full && col + 1 >= num_cols {
            self.deferred_wrap_newline();
            let new_row = self.grid.cursor.row;
            let new_col = self.grid.cursor.col;
            self.prepare_primary_screen_exit_row_overwrite();
            self.include_primary_screen_viewport_row(new_row);
            // T3 review P1: bisected far-half registry accounting (see the
            // standard site) — the fresh pair below re-tags new_col and
            // new_col+1; only a slot beyond them can keep a stale entry.
            let next_bisected = new_col + 2 < num_cols
                && self.grid.viewport[new_row].cells[new_col + 1].width == CellWidth::Full
                && self.grid.viewport[new_row].cells[new_col + 2]
                    .flags
                    .contains(CellFlags::WIDE_SPACER);
            self.grid.viewport[new_row].clear_wide_pair_at(new_col);
            self.grid.viewport[new_row].clear_wide_pair_at(new_col + 1);
            // v1.6.0 review C1: clear orphaned grapheme extras for wide-char overwrite.
            self.grid.viewport[new_row].extras.clear_grapheme(new_col);
            if new_col + 1 < num_cols {
                self.grid.viewport[new_row]
                    .extras
                    .clear_grapheme(new_col + 1);
            }
            {
                let cells = &mut self.grid.viewport[new_row].cells;
                let cell = &mut cells[new_col];
                cell.character = c;
                cell.fg = self.attrs.fg;
                cell.bg = self.attrs.bg;
                cell.flags = self.attrs.flags | CellFlags::DIRTY;
                cell.width = CellWidth::Full;
                // v1.11.3 (§1.1): carry underline fields (field-assigned).
                cell.underline_style = self.attrs.underline_style;
                cell.underline_color = self.attrs.underline_color;
                // OSC 8: tag the wrapped wide-char cell with the active hyperlink.
                if self.active_hyperlink_id.is_some() {
                    cell.flags |= CellFlags::HYPERLINK;
                }

                if new_col + 1 < num_cols {
                    let spacer = &mut cells[new_col + 1];
                    spacer.character = ' ';
                    spacer.flags = CellFlags::WIDE_SPACER;
                    spacer.width = CellWidth::Half;
                }
            }
            // v1.6.1: update hyperlink registry + RowExtras after releasing the
            // `cells` borrow (same pattern as the standard print path above).
            if let Some(id) = self.active_hyperlink_id {
                self.hyperlinks.link_cell(new_row, new_col, id);
                self.grid.viewport[new_row]
                    .extras
                    .set_hyperlink(new_col, Some(id));
            } else {
                self.hyperlinks.unlink_cell(new_row, new_col);
                self.grid.viewport[new_row]
                    .extras
                    .set_hyperlink(new_col, None);
            }
            // T3 (P3-2 ruling): the spacer half carries the lead's hyperlink
            // so viewport rows hover like materialized history rows (both
            // halves of the glyph are clickable). Plain assignment above
            // already dropped any stale HYPERLINK flag; registry/extras are
            // cleaned symmetrically with the lead.
            if new_col + 1 < num_cols {
                if let Some(id) = self.active_hyperlink_id {
                    self.hyperlinks.link_cell(new_row, new_col + 1, id);
                    self.grid.viewport[new_row]
                        .extras
                        .set_hyperlink(new_col + 1, Some(id));
                    self.grid.viewport[new_row].cells[new_col + 1].flags |= CellFlags::HYPERLINK;
                } else {
                    self.hyperlinks.unlink_cell(new_row, new_col + 1);
                    self.grid.viewport[new_row]
                        .extras
                        .set_hyperlink(new_col + 1, None);
                }
            }
            if next_bisected {
                self.hyperlinks.unlink_cell(new_row, new_col + 2);
            }
            self.grid.viewport[new_row].mark_dirty(new_col);
            if new_col + 1 < num_cols {
                self.grid.viewport[new_row].mark_dirty(new_col + 1);
            }

            self.grid.cursor.col = new_col + 2;
            if self.grid.cursor.col >= num_cols {
                self.grid.cursor.wrap_pending = true;
                self.grid.cursor.col = num_cols - 1;
            }
            return;
        }

        // Write the character. Normally `col < num_cols` because the deferred-
        // wrap / wide-char logic above keeps the cursor in bounds, but during a
        // resize the grid narrows immediately while the PTY SIGWINCH is still
        // in flight — the shell keeps emitting at the old width and the cursor
        // can momentarily point past the last column. Instead of silently
        // dropping those characters, wrap to the next line so nothing is lost.
        // The shell will repaint correctly once it learns the new width.
        let mut row = row;
        let mut col = col;
        if col >= num_cols {
            self.deferred_wrap_newline();
            row = self.grid.cursor.row;
            col = 0;
        }
        self.prepare_primary_screen_exit_row_overwrite();
        self.include_primary_screen_viewport_row(row);

        // T3 review P1: a narrow write that bisects a wide pair clears the
        // far half's cell + extras (inside clear_wide_pair_at) — its
        // registry entry must follow. Detected before the clears mutate the
        // row.
        let lead_bisected = col > 0
            && self.grid.viewport[row].cells[col]
                .flags
                .contains(CellFlags::WIDE_SPACER)
            && self.grid.viewport[row].cells[col - 1].width == CellWidth::Full;
        let spacer_bisected = width == CellWidth::Half
            && col + 1 < num_cols
            && self.grid.viewport[row].cells[col].width == CellWidth::Full
            && self.grid.viewport[row].cells[col + 1]
                .flags
                .contains(CellFlags::WIDE_SPACER);

        {
            self.grid.viewport[row].clear_wide_pair_at(col);
            if width == CellWidth::Full {
                self.grid.viewport[row].clear_wide_pair_at(col + 1);
            }
            // v1.6.0 review C1: clear orphaned grapheme extras before overwriting.
            // A non-combining scalar replaces the cell's content; any multi-scalar
            // grapheme cluster stored in extras becomes stale and must be removed
            // (preserving hyperlink_id, which is handled below).
            self.grid.viewport[row].extras.clear_grapheme(col);
            if width == CellWidth::Full {
                self.grid.viewport[row].extras.clear_grapheme(col + 1);
            }
            // v1.0 fix (CJK splat): before writing a new char, clear any
            // existing wide-char pair that this write would bisect. Without
            // this, overwriting part of a double-width CJK char (or its
            // WIDE_SPACER second cell) leaves an orphaned half that manifests
            // as merged fragments and phantom spaces after a TUI app (vim)
            // scrolls via IL/DL and reprints shorter/different content. This
            // is the standard "wide splat" handling xterm/Alacritty perform.
            {
                let cells = &mut self.grid.viewport[row].cells;
                let cell = &mut cells[col];
                cell.character = c;
                cell.fg = self.attrs.fg;
                cell.bg = self.attrs.bg;
                cell.flags = self.attrs.flags | CellFlags::DIRTY;
                cell.width = width;
                // v1.11.3 (§1.1): carry underline style/color (see wrap site).
                cell.underline_style = self.attrs.underline_style;
                cell.underline_color = self.attrs.underline_color;

                // OSC 8: tag the cell with HYPERLINK and record its (row,col)→id
                // in the registry side-map. When the active hyperlink is None
                // (overwriting a previously tagged cell), drop the side-map entry
                // and clear the flag so the underline disappears.
                if self.active_hyperlink_id.is_some() {
                    cell.flags |= CellFlags::HYPERLINK;
                } else if cell.flags.contains(CellFlags::HYPERLINK) {
                    cell.flags.remove(CellFlags::HYPERLINK);
                }

                if width == CellWidth::Full && col + 1 < num_cols {
                    let spacer = &mut cells[col + 1];
                    spacer.character = ' ';
                    spacer.flags = CellFlags::WIDE_SPACER;
                    spacer.width = CellWidth::Half;
                }
            }
            // v1.6.1: update hyperlink registry + RowExtras after releasing the
            // `cells` borrow so we don't violate the borrow checker (cells and
            // extras are different fields of the same Row, but the borrow
            // checker can't prove non-aliasing through `&mut self.grid.viewport[row]`).
            if let Some(id) = self.active_hyperlink_id {
                self.hyperlinks.link_cell(row, col, id);
                self.grid.viewport[row].extras.set_hyperlink(col, Some(id));
            } else {
                self.hyperlinks.unlink_cell(row, col);
                self.grid.viewport[row].extras.set_hyperlink(col, None);
            }
            // T3 (P3-2 ruling): spacer carries the lead's hyperlink — see the
            // wrap-first site above. `spacer.flags = WIDE_SPACER` in the write
            // above already cleared a stale HYPERLINK flag; the id/registry
            // follow the lead symmetrically.
            if width == CellWidth::Full && col + 1 < num_cols {
                if let Some(id) = self.active_hyperlink_id {
                    self.hyperlinks.link_cell(row, col + 1, id);
                    self.grid.viewport[row]
                        .extras
                        .set_hyperlink(col + 1, Some(id));
                    self.grid.viewport[row].cells[col + 1].flags |= CellFlags::HYPERLINK;
                } else {
                    self.hyperlinks.unlink_cell(row, col + 1);
                    self.grid.viewport[row].extras.set_hyperlink(col + 1, None);
                }
            }
            // T3 review P1: registry entries of bisected wide-pair halves die
            // with the pair (extras already handled by clear_wide_pair_at).
            if lead_bisected {
                self.hyperlinks.unlink_cell(row, col - 1);
            }
            if spacer_bisected {
                self.hyperlinks.unlink_cell(row, col + 1);
            }
            self.grid.viewport[row].mark_dirty(col);
            if width == CellWidth::Full && col + 1 < num_cols {
                self.grid.viewport[row].mark_dirty(col + 1);
            }
        }
        self.grid.cursor.col += width as usize;

        if self.grid.cursor.col >= num_cols {
            self.grid.cursor.wrap_pending = true;
            self.grid.cursor.col = num_cols - 1;
        }
    }

    fn execute(&mut self, byte: u8) {
        self.suppress_joined_scalar = false;
        match byte {
            0x07 => { /* BEL — bell, ignored in v0.1 */ }
            0x08 => {
                self.grid.backspace();
                self.capture_backspace();
                self.capture_primary_screen_interrupt_backspace();
            }
            0x09 => {
                // Tab is a C0 control, so the print path never sees it — but
                // columnar tools (ls, etc.) separate fields with tabs. Mirror
                // the cursor's tab advance into the active sink as spaces,
                // otherwise the block view concatenates the fields.
                let prev_col = self.grid.cursor.col;
                self.grid.advance_tab(1);
                {
                    let advanced = self.grid.cursor.col.saturating_sub(prev_col);
                    let style = self.capture_style();
                    for _ in 0..advanced {
                        self.capture_print(' ', style);
                    }
                    for _ in 0..self.grid.cursor.col.saturating_sub(prev_col) {
                        self.capture_primary_screen_interrupt_print(' ', style);
                    }
                }
            }
            0x0A..=0x0C => {
                // LF, VT, FF → move to next line (CR+LF on Unix terminals).
                // The raw VT `index()` only moves down; Unix terminals
                // treat LF as newline (carriage return + index).
                self.capture_newline();
                self.capture_primary_screen_interrupt_newline();
                self.grid.carriage_return();
                if self.index_primary_screen() {
                    self.hyperlinks.clear_cell_map();
                }
            }
            0x0D => {
                self.grid.carriage_return();
                self.capture_carriage_return();
                self.capture_primary_screen_interrupt_carriage_return();
            }
            _ => tracing::trace!(byte, "unhandled execute"),
        }
    }

    fn csi_dispatch(
        &mut self,
        params: &vte::Params,
        intermediates: &[u8],
        _ignore: bool,
        action: char,
    ) {
        // Trait-block delegate — the body lives verbatim in actions_csi.rs
        // (v1.13.8 S2 zero-behavior split; `vte::Perform` stays one impl).
        Terminal::csi_dispatch(self, params, intermediates, _ignore, action);
    }

    fn esc_dispatch(&mut self, intermediates: &[u8], _ignore: bool, byte: u8) {
        self.suppress_joined_scalar = false;
        match (intermediates, byte) {
            (&[], 0x37) => self.grid.save_cursor(),    // DECSC
            (&[], 0x38) => self.grid.restore_cursor(), // DECRC
            (&[], 0x44) => {
                // IND — index. May scroll the region; if so, OSC 8 cell_map
                // entries (viewport-relative) go stale.
                if self.index_primary_screen() {
                    self.hyperlinks.clear_cell_map();
                }
            }
            (&[], 0x4D) => {
                // RI — reverse index. Same scroll invalidation as IND.
                if self.reverse_index_primary_screen() {
                    self.hyperlinks.clear_cell_map();
                }
            }
            (&[], 0x45) => {
                // NEL — next line. Same as CR+IND.
                if self.index_primary_screen() {
                    self.hyperlinks.clear_cell_map();
                }
                self.grid.carriage_return();
            }
            (&[], 0x48) => self.grid.set_tabstop(), // HTS
            (&[], 0x63) => self.reset(),            // RIS
            (&[], 0x3D) | (&[], 0x3E) => { /* DECKPAM/DECKPNM — ignored */ }
            _ => {
                tracing::trace!(?intermediates, byte, "unhandled ESC");
            }
        }
    }

    fn osc_dispatch(&mut self, params: &[&[u8]], _bell_terminated: bool) {
        // Trait-block delegate — the body lives verbatim in actions_osc.rs
        // (v1.13.8 S2 zero-behavior split; `vte::Perform` stays one impl).
        Terminal::osc_dispatch(self, params, _bell_terminated);
    }

    fn hook(&mut self, _params: &vte::Params, intermediates: &[u8], _ignore: bool, action: char) {
        self.suppress_joined_scalar = false;
        // DCS queries we answer: XTGETTCAP (`+q`, opencode `Ms` probe) and
        // v1.11.3 DECRQSS (`$q`, nvim `ESC P $ q m ST` probe, §2.2); the
        // rest stays ignored.
        if action == 'q' && (intermediates == b"+" || intermediates == b"$") {
            self.dcs_query.begin(action, intermediates);
            return;
        }
        tracing::trace!(action = ?action, "DCS hook (ignored)");
    }

    fn put(&mut self, byte: u8) {
        self.suppress_joined_scalar = false;
        // DCS data — collected only inside `+q` / `$q` requests.
        self.dcs_query.push(byte);
    }

    fn unhook(&mut self) {
        self.suppress_joined_scalar = false;
        // DCS end: answer an accumulated query (replies.rs shapes the
        // bytes; a 1KiB-cap overflow is dropped whole, silently).
        if let Some((kind, payload)) = self.dcs_query.finish() {
            for reply in replies::dcs_query_replies(kind, &payload, &self.attrs) {
                self.respond(&reply);
            }
        }
    }
}
