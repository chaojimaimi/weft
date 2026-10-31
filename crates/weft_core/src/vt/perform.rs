use super::replies;
use super::Terminal;
use crate::blocks::ShellPhase;
use crate::grid::terminal_char_width;

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
        // WP1 D2: one capture_style() feeds both sinks — neither mutates
        // `self.attrs`, so the second snapshot was byte-identical.
        {
            let style = self.capture_style();
            self.capture_print(c, style);
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

        // WP1 D1: everything from the deferred-wrap handling on is the shared
        // write body — moved verbatim to actions_print.rs (`print_char_write`)
        // so the UTF-8 wide-char fast lane runs the identical code. The
        // preamble above (snap / capture / suppress / RI / width-0) stays
        // slow-path only; the lane re-asserts it at entry (D3 admission makes
        // those branches unreachable there).
        self.print_char_write(c, scalar_width);
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
