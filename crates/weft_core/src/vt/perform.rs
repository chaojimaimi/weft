use super::attrs::ShellMarker;
use super::osc::{parse_osc7_cwd, parse_x11_color};
use super::param;
use super::replies;
use super::Terminal;
use super::PRIMARY_SCREEN_EXIT_SETTLE_DELAY;
use crate::blocks::{CapturedStyle, ShellPhase};
use crate::grid::{terminal_char_width, CellFlags, CellWidth, CursorStyle};

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
            self.grid.scroll_offset = 0;
        }

        // Feed the printed char to the active sink: in-flight block capture
        // while CommandExecuting, preexec staging between editor submit and
        // `133;B` (FIX_ORPHAN_PARSE_ERROR_OUTPUT), nothing otherwise.
        // v1.7.0-A: capture the current VT SGR attrs so the block preserves
        // program-emitted colors after the live grid scrolls away.
        {
            let style = CapturedStyle::from_attrs(self.attrs.fg, self.attrs.bg, self.attrs.flags);
            self.capture_print(c, style);
        }
        {
            let style = CapturedStyle::from_attrs(self.attrs.fg, self.attrs.bg, self.attrs.flags);
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
            self.grid.cursor.wrap_pending = false;
            self.grid.cursor.col = 0;
            let (_, bottom) = self.grid.scroll_region();
            if self.grid.cursor.row == bottom {
                self.scroll_grid_up(1);
            } else if self.grid.cursor.row < self.grid.num_rows - 1 {
                self.grid.cursor.row += 1;
            }
            // Mark the previous row as wrapped so reflow can merge it
            // back when the terminal widens.
            if self.grid.cursor.row > 0 {
                self.grid.viewport[self.grid.cursor.row - 1].wrapped = true;
            }
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
            self.grid.cursor.wrap_pending = false;
            self.grid.cursor.col = 0;
            let (_, bottom) = self.grid.scroll_region();
            if self.grid.cursor.row == bottom {
                self.scroll_grid_up(1);
            } else if self.grid.cursor.row < self.grid.num_rows - 1 {
                self.grid.cursor.row += 1;
            }
            // Mark the previous row as wrapped for reflow
            if self.grid.cursor.row > 0 {
                self.grid.viewport[self.grid.cursor.row - 1].wrapped = true;
            }
            let new_row = self.grid.cursor.row;
            let new_col = self.grid.cursor.col;
            self.prepare_primary_screen_exit_row_overwrite();
            self.include_primary_screen_viewport_row(new_row);
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
            self.grid.cursor.wrap_pending = false;
            self.grid.cursor.col = 0;
            let (_, bottom) = self.grid.scroll_region();
            if self.grid.cursor.row == bottom {
                self.scroll_grid_up(1);
            } else if self.grid.cursor.row < self.grid.num_rows - 1 {
                self.grid.cursor.row += 1;
            }
            row = self.grid.cursor.row;
            col = 0;
            if row > 0 {
                self.grid.viewport[row - 1].wrapped = true;
            }
        }
        self.prepare_primary_screen_exit_row_overwrite();
        self.include_primary_screen_viewport_row(row);

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
                    let style =
                        CapturedStyle::from_attrs(self.attrs.fg, self.attrs.bg, self.attrs.flags);
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
        self.suppress_joined_scalar = false;
        // DECRQM private-mode query; report DEC 2026 synchronized output.
        if action == 'p' && intermediates == [b'?', b'$'] {
            for sub in params.iter() {
                if let &[mode] = sub {
                    let status = match mode {
                        2026 if self.synchronized_output() => 1,
                        2026 => 2,
                        _ => 0,
                    };
                    self.respond(format!("\x1b[?{mode};{status}$y").as_bytes());
                }
            }
            return;
        }

        // DEC private mode set/unset: `CSI ? <params> h/l` only. Earlier this
        // block matched every `CSI ?` action, so `CSI ? 1 u` (a kitty
        // keyboard-mode op carrying the private marker) mis-ran
        // handle_dec_private_mode(1, false) and silently toggled DECCKM off.
        // Non-h/l actions fall through to the match below, where the per-arm
        // intermediates guards swallow them (see the 'u' arm).
        if intermediates == [b'?'] && matches!(action, 'h' | 'l') {
            let set = action == 'h';
            for sub in params.iter() {
                if let &[mode] = sub {
                    self.handle_dec_private_mode(mode, set);
                }
            }
            return;
        }

        // Diagnostic: trace cursor-moving CSIs to pin down TUI cursor desync.
        if super::capture_cursor::is_primary_screen_addressing(action, param(params, 0, 1)) {
            let absolute = super::capture_cursor::is_absolute_primary_screen_addressing(
                action,
                param(params, 0, 1),
            );
            self.note_primary_screen_cursor_addressing(absolute);
        }
        if matches!(
            action,
            'A' | 'B' | 'C' | 'D' | 'E' | 'F' | 'H' | 'f' | 'G' | 'd'
        ) {
            tracing::debug!(
                action = %action,
                p0 = param(params, 0, 1),
                p1 = param(params, 1, 1),
                before_row = self.grid.cursor.row,
                before_col = self.grid.cursor.col,
                origin = self.origin_mode,
                "csi-move"
            );
        }

        match action {
            // Cursor movement
            'A' => {
                let n = param(params, 0, 1) as usize;
                self.grid.move_up(n);
                if !self.capabilities.alt_active {
                    self.block_tracker.on_move_cursor_rows(-(n as isize));
                }
                self.capture_primary_screen_interrupt_cursor_position(false);
                // v1.10.31: CUU counts as non-trivial addressing in DEC 2026 window
                self.note_synchronized_frame_addressing();
            }
            'B' => {
                let n = param(params, 0, 1) as usize;
                self.grid.move_down(n);
                if !self.capabilities.alt_active {
                    self.block_tracker.on_move_cursor_rows(n as isize);
                }
                self.capture_primary_screen_interrupt_cursor_position(false);
                // v1.10.31: CUD counts as non-trivial addressing in DEC 2026 window
                self.note_synchronized_frame_addressing();
            }
            'C' => {
                let amount = param(params, 0, 1) as usize;
                self.grid.move_forward(amount);
                self.capture_block_cursor_column(self.grid.cursor.col);
                self.capture_primary_screen_interrupt_cursor_position(false);
            }
            'D' => {
                let amount = param(params, 0, 1) as usize;
                self.grid.move_backward(amount);
                self.capture_block_cursor_column(self.grid.cursor.col);
                self.capture_primary_screen_interrupt_cursor_position(false);
            }
            'E' => {
                let n = param(params, 0, 1) as usize;
                self.grid.move_down(n);
                self.grid.carriage_return();
                if !self.capabilities.alt_active {
                    self.block_tracker.on_move_cursor_rows(n as isize);
                    self.block_tracker.on_carriage_return();
                }
                self.capture_primary_screen_interrupt_cursor_position(false);
            }
            'F' => {
                let n = param(params, 0, 1) as usize;
                self.grid.move_up(n);
                self.grid.carriage_return();
                if !self.capabilities.alt_active {
                    self.block_tracker.on_move_cursor_rows(-(n as isize));
                    self.block_tracker.on_carriage_return();
                }
                self.capture_primary_screen_interrupt_cursor_position(false);
            }

            // Cursor position
            'H' | 'f' => {
                let row = param(params, 0, 1) as usize;
                let col = param(params, 1, 1) as usize;
                self.grid.goto(row, col, self.origin_mode);
                self.capture_primary_screen_interrupt_cursor_position(true);
                // v1.10.31: CUP counts as non-trivial addressing in DEC 2026 window
                self.note_synchronized_frame_addressing();
                tracing::debug!(
                    req_row = row,
                    req_col = col,
                    origin = self.origin_mode,
                    cur_row = self.grid.cursor.row,
                    cur_col = self.grid.cursor.col,
                    "CUP"
                );
            }
            'G' => {
                let col = param(params, 0, 1) as usize;
                self.grid.set_cursor_col(col.saturating_sub(1));
                self.capture_block_cursor_column(self.grid.cursor.col);
                self.capture_primary_screen_interrupt_cursor_position(col == 1);
                // v1.10.31: CHA counts as non-trivial addressing in DEC 2026 window
                // ONLY when column > 1 (column 1 is just carriage return, brew uses it)
                if col > 1 {
                    self.note_synchronized_frame_addressing();
                }
            }
            'd' => {
                let row = param(params, 0, 1) as usize;
                self.grid.set_cursor_row(row.saturating_sub(1));
                self.capture_primary_screen_interrupt_cursor_position(false);
                // v1.10.31: VPA counts as non-trivial addressing in DEC 2026 window
                self.note_synchronized_frame_addressing();
            }

            // Erase
            'J' => {
                let mode = param(params, 0, 0);
                match mode {
                    0 => self.grid.clear_screen_below(),
                    1 => self.grid.clear_screen_above(),
                    2 => {
                        self.note_primary_screen_full_erase();
                        self.grid.clear_screen_all();
                    }
                    3 if self.capabilities.alt_active => self.grid.clear_scrollback(),
                    3 => self.clear_primary_screen_scrollback(),
                    _ => {}
                }
            }
            'K' => {
                let mode = param(params, 0, 0);
                match mode {
                    0 => self.grid.clear_line_right(),
                    1 => self.grid.clear_line_left(),
                    2 => {
                        self.note_primary_screen_line_erase();
                        self.grid.clear_line_all();
                    }
                    _ => {}
                }
                self.capture_erase_line(mode);
                self.capture_primary_screen_interrupt_erase_line(mode);
            }
            'X' => {
                let count = param(params, 0, 1) as usize;
                self.grid.erase_chars(count);
            }

            // SGR
            //
            // FIX_OPENCODE_STARTUP_FLASH: private SGR forms (`CSI > … m`,
            // xterm's modifyOtherKeys enable/disable) carry an intermediate
            // byte that vte keeps out of `Params`. Dispatching them to
            // `handle_sgr` as if the params were bare (`>4;1m` → SGR 4;1)
            // leaked UNDERLINE|BOLD onto every later cell: opencode enables
            // modifyOtherKeys at startup, then clears rows with white-fg
            // spaces (no intervening `0m`) — the leaked attributes painted
            // full-width near-white underline rails across the cleared rows
            // (the "white horizontal bars" flash). Private SGRs are
            // terminal-behaviour controls, never text attributes: ignore.
            'm' if !intermediates.is_empty() => {
                tracing::trace!(?intermediates, ?params, "private SGR ignored");
            }
            'm' => self.handle_sgr(params),

            // ── Terminal queries (must respond, else TUIs degrade) ──
            // DA1 / primary device attributes (CSI c).
            'c' if intermediates.is_empty() => {
                // VT220-class with ANSI color — a response any TUI accepts.
                self.respond(b"\x1b[?62;1;2;4;6;9;15;22c");
            }
            // DA2 / secondary device attributes (CSI > c).
            'c' if intermediates == [b'>'] => {
                // "weft", version 0, ROM 0.
                self.respond(b"\x1b[>0;276;0c");
            }
            // DSR — device status / cursor-position report (CSI n).
            'n' => match param(params, 0, 0) {
                5 => self.respond(b"\x1b[0n"), // terminal OK
                6 => {
                    // CPR: cursor position, 1-based.
                    let r = self.grid.cursor.row + 1;
                    let c = self.grid.cursor.col + 1;
                    self.respond(format!("\x1b[{r};{c}R").as_bytes());
                }
                _ => {}
            },
            // Text-area size report (CSI t). 18 = size in chars, 14/16 = pixels.
            't' => match param(params, 0, 0) {
                18 | 19 => {
                    let rows = self.grid.num_rows;
                    let cols = self.grid.num_cols;
                    self.respond(format!("\x1b[8;{rows};{cols}t").as_bytes());
                }
                _ => {}
            },

            // Scrolling
            'S' => {
                let n = param(params, 0, 1);
                self.scroll_grid_up(if n == 0 { 1 } else { n as usize });
            }
            'T' => {
                if intermediates.is_empty() {
                    self.scroll_grid_down(param(params, 0, 1) as usize);
                }
            }

            // Scroll region
            'r' => {
                if params.is_empty() {
                    self.grid.reset_scroll_region();
                    tracing::debug!("scroll region reset");
                } else {
                    let top = param(params, 0, 1) as usize;
                    let bottom = param(params, 1, 0) as usize;
                    if bottom == 0 {
                        self.grid.reset_scroll_region();
                        tracing::debug!("scroll region reset (bottom=0)");
                    } else {
                        self.grid.set_scroll_region(top, bottom);
                        tracing::debug!(top, bottom, "scroll region set");
                    }
                }
            }

            // Cursor save/restore (SCO style)
            's' => {
                self.grid.save_cursor();
                tracing::debug!(
                    row = self.grid.cursor.row,
                    col = self.grid.cursor.col,
                    "DECSC save"
                );
            }
            // Kitty keyboard-protocol push/set/pop (`CSI >flags u`,
            // `CSI =mode u`, `CSI <u`) arrive with the leading byte in
            // intermediates (vte collects `<=>?` there). They must NOT fall
            // through to the bare-`CSI u` DECRC restore below: restore_cursor
            // would jump back to the last DECSC-saved position every time the
            // app flips a mode (opencode pushes several at startup, clobbering
            // the TUI's cursor mid-paint). Ignore the mode operations; bare
            // `CSI u` keeps its SCO DECRC alias behavior. `CSI ? u` (kitty
            // keyboard enhancement query) is swallowed here too
            // (intermediates [b'?']). Zero reply is the standard
            // non-supporter signal — crossterm, Nix etc. fall back on DA1
            // to detect the terminal; answering `?0u` would advertise a
            // protocol we never implement. Deliberate — do NOT "fix".
            'u' if !intermediates.is_empty() => {
                tracing::trace!(?intermediates, ?params, "kitty keyboard mode op ignored");
            }
            'u' => {
                self.grid.restore_cursor();
                tracing::debug!(
                    row = self.grid.cursor.row,
                    col = self.grid.cursor.col,
                    "DECRC restore"
                );
            }

            // Insert/delete
            '@' => self.grid.insert_blank(param(params, 0, 1) as usize),
            'P' => self.grid.delete_chars(param(params, 0, 1) as usize),
            'L' => self.insert_primary_screen_lines(param(params, 0, 1) as usize),
            'M' => self.delete_primary_screen_lines(param(params, 0, 1) as usize),

            // Tab stops
            'I' => self.grid.advance_tab(param(params, 0, 1) as usize),
            'Z' => self.grid.back_tab(param(params, 0, 1) as usize),
            'g' => {
                let mode = param(params, 0, 0);
                match mode {
                    0 => self.grid.clear_tabstop(),
                    3 => self.grid.clear_all_tabstops(),
                    _ => {}
                }
            }

            // XTVERSION (CSI > 0 q / CSI > q) — xterm's version query; opencode
            // sends it at startup. Reply `DCS > | weft <ver> ST`. Split by
            // exact intermediates so it cannot collide with DECSCUSR below
            // (empty or SP intermediates) nor DA2 (`CSI > c`).
            'q' if intermediates == [b'>'] => {
                self.respond(&replies::xtversion_reply());
            }
            // Cursor style (DECSCUSR — CSI <n> q)
            'q' => {
                if intermediates.is_empty() || intermediates == [b' '] {
                    // ECMA/DEC standard form is `CSI Ps SP q`; retain the
                    // no-intermediate form for compatibility with older TUIs.
                    let style = param(params, 0, 0);
                    self.cursor_style = match style {
                        0 | 1 => CursorStyle::BlinkingBlock,
                        2 => CursorStyle::Block,
                        3 => CursorStyle::BlinkingUnderline,
                        4 => CursorStyle::Underline,
                        5 => CursorStyle::BlinkingBar,
                        6 => CursorStyle::Bar,
                        _ => CursorStyle::Block,
                    };
                }
            }

            _ => {
                tracing::trace!(?params, ?intermediates, action = ?action, "unhandled CSI");
            }
        }
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
        self.suppress_joined_scalar = false;
        if params.is_empty() {
            return;
        }

        let code = std::str::from_utf8(params[0]).unwrap_or("");

        match code {
            "0" | "2" => {
                if params.len() > 1 {
                    self.title = String::from_utf8_lossy(params[1]).into_owned();
                }
            }
            "4" => {
                if params.len() >= 3 {
                    if let Ok(idx) = std::str::from_utf8(params[1])
                        .unwrap_or("")
                        .parse::<usize>()
                    {
                        if idx < 256 {
                            if let Some(color) = parse_x11_color(params[2]) {
                                self.palette[idx] = color;
                            }
                        }
                    }
                }
            }
            "11" => {
                // OSC 11 background query — answer the app theme's
                // background in xterm rgb:RR/GG/BB form so TUIs detect
                // dark vs light. Only queries (`OSC 11` / `OSC 11;?`)
                // are answered; a set request (`OSC 11;rgb:..`) is not.
                let is_query =
                    params.len() == 1 || params.get(1).is_some_and(|p| p.is_empty() || p == b"?");
                if is_query {
                    let [r, g, b] = [
                        self.background_color.r,
                        self.background_color.g,
                        self.background_color.b,
                    ];
                    self.respond(format!("\x1b]11;rgb:{r:02x}/{g:02x}/{b:02x}\x1b\\").as_bytes());
                }
            }
            "7" => {
                if params.len() > 1 {
                    if let Some(path) = parse_osc7_cwd(params[1]) {
                        self.cwd = Some(path.clone());
                        // Mirror into the block tracker so each block is stamped
                        // with the dir it ran in (for the block-view header).
                        self.block_tracker.set_cwd(Some(path));
                    }
                }
            }
            "9" => {
                // Custom OSC 9;git=<branch> — shell hook reports the git branch.
                if let Some(payload) = params.get(1) {
                    if let Ok(s) = std::str::from_utf8(payload) {
                        if let Some(branch) = s.strip_prefix("git=") {
                            self.git_branch = Some(branch.to_string());
                        }
                    }
                }
            }
            "8" => {
                // OSC 8 ; params ; URI ST — hyperlink.
                //   `OSC 8 ; ; URI ST`     → start hyperlink to URI (no id).
                //   `OSC 8 ; id=K ; URI ST`→ start hyperlink with id K (kitty
                //                            extension; we dedup by URL anyway).
                //   `OSC 8 ; ; ST`         → end hyperlink (empty URI).
                // We dedup URLs in the registry and remember the active id;
                // `print()` stamps cells with HYPERLINK while this is Some.
                // v1.6.1: register() returns 0 when URL is too long or the
                // registry is full — treat 0 as "no link" so the cell doesn't
                // get tagged with an unresolvable id.
                if params.len() >= 3 {
                    let uri = std::str::from_utf8(params[2]).unwrap_or("");
                    if uri.is_empty() {
                        self.active_hyperlink_id = None;
                    } else {
                        let id = self.hyperlinks.register(uri.to_string());
                        self.active_hyperlink_id = (id != 0).then_some(id);
                    }
                } else {
                    // OSC 8 ;; ST (no URI field) — clear.
                    self.active_hyperlink_id = None;
                }
            }
            "133" => {
                if params.len() > 1 {
                    let tagged = params.iter().skip(2).any(|p| *p == b"weft-shell");
                    if !self.capabilities.accepts_shell_marker(tagged) {
                        tracing::trace!("ignored untagged application OSC 133 zone");
                        return;
                    }
                    match params[1] {
                        b"A" => {
                            self.snapshot_primary_screen_output();
                            // v1.10.7: a screen-owned TUI session whose shell
                            // emits 133;A (precmd) while still running — e.g. pi
                            // spawns an interactive zsh that inherits
                            // WEFT_SHELL_INTEGRATION and re-emits the markers per
                            // internal command. If a pending exit ALREADY exists
                            // (the precmd pair `133;D → 133;A` arrived together,
                            // or `133;A` follows `133;D` of the previous marker
                            // run), deferring again would reset `phase` to
                            // AtPrompt and split the session block per internal
                            // command. Only defer when no exit is pending.
                            let defer_screen_exit =
                                self.block_tracker.screen_document_start().is_some()
                                    && self.block_tracker.phase() == ShellPhase::CommandExecuting
                                    && self.capabilities.primary_screen_exit.is_none();
                            self.capabilities.primary_screen_cursor_ops = 0;
                            self.capabilities.primary_screen_absolute_addressing = false;
                            self.capabilities.primary_screen_relative_addressing_seen = false;
                            self.reset_primary_screen_synchronized_frame();
                            self.attrs = Default::default();
                            self.shell_markers.push(ShellMarker::PromptStart);
                            if defer_screen_exit {
                                self.defer_primary_screen_exit(None);
                                // v1.10.7: do NOT unlock the render-mode lock
                                // here. This branch is also reachable by the
                                // FIRST marker of a nested run (pi's inner
                                // zsh prompt before any internal command
                                // completed → no pending exit yet), and
                                // unlocking would let the next CUP repaint
                                // flip the session to the live grid. The lock
                                // is released in `settle_primary_screen_exit`
                                // (the real finalize path) and at a real
                                // 133;B command start.
                            } else if self.capabilities.primary_screen_exit.is_some()
                                && self.block_tracker.screen_document_start().is_some()
                            {
                                // v1.10.7: this 133;A is the second marker of a
                                // nested precmd pair (`133;D → 133;A`) from a
                                // shell running INSIDE the screen-owned TUI — the
                                // pending exit belongs to the first marker of the
                                // run and the session is still executing. Keep it
                                // executing (do NOT let `on_prompt_start` finalize
                                // the in-flight block). The render-mode lock must
                                // SURVIVE nested markers.
                                self.block_tracker.resume_screen_command();
                            } else {
                                self.block_tracker.on_prompt_start();
                                // v1.10.7: real prompt boundary (or shell
                                // outside a screen session).
                            }
                            // Clear the git branch: the precmd hook re-emits
                            // OSC 9;git= if (and only if) the cwd is still a git
                            // repo. Without this, leaving a repo keeps the stale
                            // branch label forever (the hook sends nothing in a
                            // non-git dir, so git_branch was never cleared).
                            self.git_branch = None;
                            // Clear any stale submit flag so a missing 133;B
                            // (crashed / non-integrated sub-shell) can't pin the
                            // editor in passthrough forever.
                            self.command_from_editor = None;
                            // FIX_ORPHAN_PARSE_ERROR_OUTPUT: same staleness
                            // guard for the staging buffer — a D-less sequence
                            // (interrupted / crashed shell) must not leak its
                            // staged bytes into a LATER command's block.
                            let stale = self.drop_preexec_staging();
                            if stale > 0 {
                                tracing::warn!(
                                    bytes = stale,
                                    "133;A without 133;B/D: dropped stale pre-exec staging"
                                );
                            }
                        }
                        b"B" => {
                            // FIX_ORPHAN_PARSE_ERROR_OUTPUT (normal path): the
                            // ZLE repaint of the accepted line staged between
                            // editor submit and preexec must never enter a
                            // block — discard it here. This is also the timing
                            // mutex with the orphan-D synthesis below: B both
                            // clears the staging and consumes
                            // `command_from_editor`, so a later D can only
                            // synthesize when THIS branch did not run.
                            let staged = self.drop_preexec_staging();
                            if staged > 0 {
                                tracing::trace!(
                                    bytes = staged,
                                    "133;B discarded pre-exec staging (ZLE echo)"
                                );
                            }
                            // v1.10.7: nested-shell guard. A screen-owned TUI
                            // session (pi/openclaw) whose shell integration runs
                            // inside the app (pi spawns interactive zsh) emits
                            // `133;B` <200ms after the `133;A`/`133;D` precmd
                            // pair — that is a NEW INTERNAL COMMAND of the still
                            // running TUI, not a command typed at the shell after
                            // the app exited. Settling here would finalize the
                            // session block per internal command (one prompt
                            // split into many blocks). A real app exit leaves the
                            // user time to type the next command, so the pending
                            // exit would already have been settled by the idle
                            // timer. When the pending exit is still younger than
                            // the settle window, cancel it and keep the in-flight
                            // block: the nested command's output still flows into
                            // the screen snapshot (screen-owned capture), so the
                            // session block grows without splitting.
                            let nested_marker =
                                self.capabilities.primary_screen_exit.as_ref().is_some_and(
                                    |pending| {
                                        pending.last_activity.elapsed()
                                            < PRIMARY_SCREEN_EXIT_SETTLE_DELAY
                                    },
                                ) && self.block_tracker.screen_document_start().is_some();
                            if nested_marker {
                                self.capabilities.primary_screen_exit = None;
                                self.block_tracker.resume_screen_command();
                                self.capabilities.primary_history_view = false;
                                self.capabilities.primary_screen_cursor_ops = 0;
                                self.capabilities.primary_screen_absolute_addressing = false;
                                self.capabilities.primary_screen_relative_addressing_seen = false;
                                self.reset_primary_screen_synchronized_frame();
                                self.shell_markers.push(ShellMarker::CommandStart);
                            } else {
                                self.settle_primary_screen_exit();
                                self.capabilities.primary_history_view = false;
                                self.capabilities.primary_screen_cursor_ops = 0;
                                self.capabilities.primary_screen_absolute_addressing = false;
                                // v1.10.7: real command start — a new
                                // command re-detects its render mode at
                                // first screen ownership.
                                self.reset_primary_screen_synchronized_frame();
                                self.shell_markers.push(ShellMarker::CommandStart);
                                // 133;B (preexec): if the editor submitted the
                                // command, record that; otherwise snapshot the grid
                                // row (real shells emit a newline first, so the
                                // cursor may sit below the command —
                                // `snapshot_command_line` walks up).
                                let command = if let Some(c) = self.command_from_editor.take() {
                                    c
                                } else {
                                    self.snapshot_command_line()
                                };
                                self.freeze_primary_screen_document_candidate();
                                self.block_tracker.on_command_start(command);
                            }
                        }
                        b"C" => {
                            self.shell_markers.push(ShellMarker::CommandOutputStart);
                            self.block_tracker.on_command_output_start();
                        }
                        b"D" => {
                            // v1.10.7: see `133;A` — a `133;D` arriving while an
                            // exit is already pending is the shell integration's
                            // precmd pair (`133;D;rc → 133;A`) or the closing of
                            // a nested command marker run; the FIRST marker of
                            // the run already deferred. Deferring again would
                            // overwrite the exit code and reset `phase`, churning
                            // the session block.
                            let defer_screen_exit =
                                self.block_tracker.screen_document_start().is_some()
                                    && self.capabilities.primary_screen_exit.is_none();
                            self.snapshot_primary_screen_output();
                            self.capabilities.primary_screen_cursor_ops = 0;
                            self.capabilities.primary_screen_absolute_addressing = false;
                            self.capabilities.primary_screen_relative_addressing_seen = false;
                            self.reset_primary_screen_synchronized_frame();
                            self.attrs = Default::default();
                            let exit_code = if params.len() > 2 {
                                std::str::from_utf8(params[2])
                                    .ok()
                                    .and_then(|s| s.parse::<i32>().ok())
                                    .unwrap_or(0)
                            } else {
                                0
                            };
                            self.shell_markers
                                .push(ShellMarker::CommandEnd { exit_code });
                            if defer_screen_exit {
                                self.defer_primary_screen_exit(Some(exit_code));
                            } else if self.capabilities.primary_screen_exit.is_some()
                                && self.block_tracker.screen_document_start().is_some()
                            {
                                // v1.10.7: nested precmd pair — the first marker
                                // of this run already deferred. Do NOT let
                                // `on_command_end` finalize the in-flight
                                // session block mid-session.
                                // v1.10.7 (reviewer LOW): if the pending exit
                                // came from a `133;A` (exit_code None — the
                                // nested run's first marker), this REAL `133;D`
                                // carries the authoritative exit code of the
                                // TUI session; upgrade it so the finalized
                                // block shows the session's code, not None.
                                if let Some(pending) = &mut self.capabilities.primary_screen_exit {
                                    if pending.exit_code.is_none() {
                                        pending.exit_code = Some(exit_code);
                                    }
                                }
                            } else if let Some((command, staged)) = self.take_orphan_staging() {
                                // FIX_ORPHAN_PARSE_ERROR_OUTPUT (parse-error
                                // path): zsh rejected the line before preexec,
                                // so no `133;B` ever opened a capture window.
                                // Build the block retroactively from the
                                // staged error output instead of dropping it
                                // on the floor (previously this arm was a bare
                                // noop — blocks.db had no record at all).
                                tracing::info!(
                                    rc = exit_code,
                                    bytes = staged.as_str().len(),
                                    command = %command,
                                    "orphan 133;D: synthesizing block from pre-exec staging"
                                );
                                self.block_tracker
                                    .on_orphan_command_end(exit_code, command, staged);
                            } else {
                                self.block_tracker.on_command_end(exit_code);
                            }
                        }
                        _ => {}
                    }
                }
            }
            _ => {
                tracing::trace!(code, "unhandled OSC");
            }
        }
    }

    fn hook(&mut self, _params: &vte::Params, intermediates: &[u8], _ignore: bool, action: char) {
        self.suppress_joined_scalar = false;
        // XTGETTCAP (`DCS + q ... ST`) — the only DCS we answer; opencode
        // queries the `Ms` capability (`DCS +q4d73`) at startup. The
        // collector gets the introducer here, `put()` feeds capped payload
        // bytes, `unhook()` answers. Everything else stays ignored.
        if action == 'q' && intermediates == [b'+'] {
            self.dcs_xtgettcap.begin(action, intermediates);
            return;
        }
        tracing::trace!(action = ?action, "DCS hook (ignored)");
    }

    fn put(&mut self, byte: u8) {
        self.suppress_joined_scalar = false;
        // DCS data — fed to the collector (capped at 1KiB) only inside an
        // XTGETTCAP request.
        self.dcs_xtgettcap.push(byte);
    }

    fn unhook(&mut self) {
        self.suppress_joined_scalar = false;
        // DCS end: answer an accumulated XTGETTCAP request. We support none
        // of the queried capabilities, so every well-formed name gets a
        // negative `DCS 0 + r <hex> ST`; malformed segments are skipped. A
        // request that overflowed the collector's 1KiB cap is dropped whole
        // (`finish()` → None), silently.
        if let Some(payload) = self.dcs_xtgettcap.finish() {
            for reply in replies::xtgettcap_negative_replies(&payload) {
                self.respond(&reply);
            }
        }
    }
}
