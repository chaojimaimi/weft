use super::attrs::ShellMarker;
use super::osc::{parse_osc7_cwd, parse_x11_color};
use super::param;
use super::Terminal;
use crate::blocks::ShellPhase;
use crate::grid::{terminal_char_width, CellFlags, CellWidth, CursorStyle};

impl Terminal {
    /// Primary-screen TUIs such as Claude Code do not enter DEC 1049, but
    /// repeatedly use absolute cursor addressing to own the whole viewport.
    pub fn primary_screen_app_active(&self) -> bool {
        !self.alt_active
            && self.block_tracker.phase() == ShellPhase::CommandExecuting
            && self.primary_screen_cursor_ops >= 2
    }

    /// Whether this primary-screen owner has demonstrated atomic full-frame repainting.
    pub fn primary_screen_repaint_capable(&self) -> bool {
        self.primary_screen_app_active() && self.primary_screen_synchronized_frame_seen
    }

    pub(super) fn begin_primary_screen_synchronized_frame(&mut self) {
        self.synchronized_frame_cleared_rows = 0;
    }

    pub(super) fn finish_primary_screen_synchronized_frame(&mut self) {
        if self.synchronized_output_started.is_some() {
            self.primary_screen_synchronized_frame_seen |= self.primary_screen_app_active()
                && self.synchronized_frame_cleared_rows >= self.grid.num_rows;
        }
    }

    fn reset_primary_screen_synchronized_frame(&mut self) {
        self.synchronized_output_started = None;
        self.synchronized_frame_cleared_rows = 0;
        self.primary_screen_synchronized_frame_seen = false;
    }

    fn note_primary_screen_full_erase(&mut self) {
        if self.synchronized_output_started.is_some() && !self.alt_active {
            self.synchronized_frame_cleared_rows = self.grid.num_rows;
        }
    }

    fn note_primary_screen_line_erase(&mut self) {
        if self.synchronized_output_started.is_some()
            && !self.alt_active
            && self.grid.cursor.row == self.synchronized_frame_cleared_rows
        {
            self.synchronized_frame_cleared_rows += 1;
        }
    }

    fn note_primary_screen_cursor_addressing(&mut self) {
        if !self.alt_active && self.block_tracker.phase() == ShellPhase::CommandExecuting {
            self.primary_screen_cursor_ops = self.primary_screen_cursor_ops.saturating_add(1);
        }
    }
}

impl vte::Perform for Terminal {
    fn print(&mut self, c: char) {
        // v1.0 perf: vte only calls print() in ground state, so mark it.
        self.parser_in_ground_state = true;
        // v1.0 perf: cache phase once per print() call. The phase doesn't
        // change within a single print() — it only transitions on OSC 133
        // markers, which arrive via osc_dispatch, not print.
        let phase = self.block_tracker.phase();

        // Snap back to the live viewport for new content — EXCEPT when idle at
        // an integrated prompt (AtPrompt). In that state the shell may emit
        // prompt re-renders or async segments that would otherwise destroy
        // the user's scroll position while they're reading history. During
        // CommandExecuting (output streaming) and in non-integrated mode
        // (plain grid terminal), new output always resets scroll.
        if phase != ShellPhase::AtPrompt {
            self.grid.scroll_offset = 0;
        }

        // Feed the printed char to the active command block's output capture.
        // v1.0 perf: check is_capturing() here to skip the function call
        // overhead when not capturing (e.g. AtPrompt, NotIntegrated).
        if !self.alt_active && phase == ShellPhase::CommandExecuting {
            self.block_tracker.on_print(c);
        }

        if self.suppress_joined_scalar && terminal_char_width(c) > 0 {
            self.suppress_joined_scalar = false;
            return;
        }

        let is_emoji_modifier = matches!(c, '\u{1f3fb}'..='\u{1f3ff}');
        let is_regional_indicator = matches!(c, '\u{1f1e6}'..='\u{1f1ff}');
        let follows_regional_indicator = is_regional_indicator
            && self.previous_cell_position().is_some_and(|(row, col)| {
                matches!(
                    self.grid.viewport[row].cells[col].character,
                    '\u{1f1e6}'..='\u{1f1ff}'
                )
            });
        if is_emoji_modifier && self.replace_previous_grapheme(false) {
            return;
        }
        if follows_regional_indicator && self.replace_previous_grapheme(true) {
            return;
        }

        // Cell stores one base scalar. Combining marks, ZWJ and variation
        // selectors therefore cannot be retained yet, but they must never
        // consume a terminal column or trigger a deferred wrap.
        let scalar_width = if c.is_ascii() {
            1
        } else {
            terminal_char_width(c)
        };
        if scalar_width == 0 {
            let replaced = self.replace_previous_grapheme(c == '\u{fe0f}');
            if c == '\u{200d}' && replaced {
                self.suppress_joined_scalar = true;
            }
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
            // Write on new line
            let new_row = self.grid.cursor.row;
            let new_col = self.grid.cursor.col;
            self.grid.viewport[new_row].clear_wide_pair_at(new_col);
            self.grid.viewport[new_row].clear_wide_pair_at(new_col + 1);
            let cells = &mut self.grid.viewport[new_row].cells;
            let cell = &mut cells[new_col];
            cell.character = c;
            cell.fg = self.attrs.fg;
            cell.bg = self.attrs.bg;
            cell.flags = self.attrs.flags | CellFlags::DIRTY;
            cell.width = CellWidth::Full;
            // OSC 8: tag the wrapped wide-char cell with the active hyperlink.
            if let Some(id) = self.active_hyperlink_id {
                cell.flags |= CellFlags::HYPERLINK;
                self.hyperlinks.link_cell(new_row, new_col, id);
            } else {
                self.hyperlinks.unlink_cell(new_row, new_col);
            }

            if new_col + 1 < num_cols {
                let spacer = &mut cells[new_col + 1];
                spacer.character = ' ';
                spacer.flags = CellFlags::WIDE_SPACER;
                spacer.width = CellWidth::Half;
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

        {
            self.grid.viewport[row].clear_wide_pair_at(col);
            if width == CellWidth::Full {
                self.grid.viewport[row].clear_wide_pair_at(col + 1);
            }
            let cells = &mut self.grid.viewport[row].cells;
            // v1.0 fix (CJK splat): before writing a new char, clear any
            // existing wide-char pair that this write would bisect. Without
            // this, overwriting part of a double-width CJK char (or its
            // WIDE_SPACER second cell) leaves an orphaned half that manifests
            // as merged fragments and phantom spaces after a TUI app (vim)
            // scrolls via IL/DL and reprints shorter/different content. This
            // is the standard "wide splat" handling xterm/Alacritty perform.
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
            if let Some(id) = self.active_hyperlink_id {
                cell.flags |= CellFlags::HYPERLINK;
                self.hyperlinks.link_cell(row, col, id);
            } else {
                if cell.flags.contains(CellFlags::HYPERLINK) {
                    cell.flags.remove(CellFlags::HYPERLINK);
                }
                self.hyperlinks.unlink_cell(row, col);
            }

            if width == CellWidth::Full && col + 1 < num_cols {
                let spacer = &mut cells[col + 1];
                spacer.character = ' ';
                spacer.flags = CellFlags::WIDE_SPACER;
                spacer.width = CellWidth::Half;
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
                if !self.alt_active {
                    self.block_tracker.on_backspace();
                }
            }
            0x09 => {
                // Tab is a C0 control, so the print path never sees it — but
                // columnar tools (ls, etc.) separate fields with tabs. Mirror
                // the cursor's tab advance into the captured block output as
                // spaces, otherwise the block view concatenates the fields.
                let prev_col = self.grid.cursor.col;
                self.grid.advance_tab(1);
                // v1.0 perf: skip on_print calls when not capturing.
                if !self.alt_active && self.block_tracker.is_capturing() {
                    let advanced = self.grid.cursor.col.saturating_sub(prev_col);
                    for _ in 0..advanced {
                        self.block_tracker.on_print(' ');
                    }
                }
            }
            0x0A..=0x0C => {
                // LF, VT, FF → move to next line (CR+LF on Unix terminals).
                // The raw VT `index()` only moves down; Unix terminals
                // treat LF as newline (carriage return + index).
                // v1.0 perf: skip on_newline call when not capturing.
                if !self.alt_active && self.block_tracker.is_capturing() {
                    self.block_tracker.on_newline();
                }
                self.grid.carriage_return();
                if self.grid.index() {
                    self.hyperlinks.clear_cell_map();
                }
            }
            0x0D => {
                self.grid.carriage_return();
                if !self.alt_active {
                    self.block_tracker.on_carriage_return();
                }
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
        // DECRQM private-mode query: CSI ? Ps $ p. OpenTUI probes mode 2026
        // before using synchronized updates. Report it as supported and
        // currently set/reset; unknown modes remain unsupported (0).
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

        // DEC private mode: CSI ? <params> h/l
        if intermediates == [b'?'] {
            let set = action == 'h';
            for sub in params.iter() {
                if let &[mode] = sub {
                    self.handle_dec_private_mode(mode, set);
                }
            }
            return;
        }

        // Diagnostic: trace cursor-moving CSIs to pin down TUI cursor desync.
        if matches!(action, 'H' | 'f' | 'G' | 'd') {
            self.note_primary_screen_cursor_addressing();
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
            'A' => self.grid.move_up(param(params, 0, 1) as usize),
            'B' => self.grid.move_down(param(params, 0, 1) as usize),
            'C' => self.grid.move_forward(param(params, 0, 1) as usize),
            'D' => self.grid.move_backward(param(params, 0, 1) as usize),
            'E' => {
                let n = param(params, 0, 1) as usize;
                self.grid.move_down(n);
                self.grid.carriage_return();
            }
            'F' => {
                let n = param(params, 0, 1) as usize;
                self.grid.move_up(n);
                self.grid.carriage_return();
            }

            // Cursor position
            'H' | 'f' => {
                let row = param(params, 0, 1) as usize;
                let col = param(params, 1, 1) as usize;
                self.grid.goto(row, col, self.origin_mode);
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
            }
            'd' => {
                let row = param(params, 0, 1) as usize;
                self.grid.set_cursor_row(row.saturating_sub(1));
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
                    3 => self.grid.clear_scrollback(),
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
                if !self.alt_active {
                    self.block_tracker.on_erase_line(mode);
                }
            }
            'X' => {
                let count = param(params, 0, 1) as usize;
                self.grid.erase_chars(count);
            }

            // SGR
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
            'L' => self.grid.insert_blank_lines(param(params, 0, 1) as usize),
            'M' => self.grid.delete_lines(param(params, 0, 1) as usize),

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
                if self.grid.index() {
                    self.hyperlinks.clear_cell_map();
                }
            }
            (&[], 0x4D) => {
                // RI — reverse index. Same scroll invalidation as IND.
                if self.grid.reverse_index() {
                    self.hyperlinks.clear_cell_map();
                }
            }
            (&[], 0x45) => {
                // NEL — next line. Same as CR+IND.
                if self.grid.index() {
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
                if params.len() >= 3 {
                    let uri = std::str::from_utf8(params[2]).unwrap_or("");
                    if uri.is_empty() {
                        self.active_hyperlink_id = None;
                    } else {
                        let id = self.hyperlinks.register(uri.to_string());
                        self.active_hyperlink_id = Some(id);
                    }
                } else {
                    // OSC 8 ;; ST (no URI field) — clear.
                    self.active_hyperlink_id = None;
                }
            }
            "133" => {
                if params.len() > 1 {
                    match params[1] {
                        b"A" => {
                            self.primary_screen_cursor_ops = 0;
                            self.reset_primary_screen_synchronized_frame();
                            self.shell_markers.push(ShellMarker::PromptStart);
                            self.block_tracker.on_prompt_start();
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
                        }
                        b"B" => {
                            self.primary_screen_cursor_ops = 0;
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
                            self.block_tracker.on_command_start(command);
                        }
                        b"C" => {
                            self.shell_markers.push(ShellMarker::CommandOutputStart);
                            self.block_tracker.on_command_output_start();
                        }
                        b"D" => {
                            self.primary_screen_cursor_ops = 0;
                            self.reset_primary_screen_synchronized_frame();
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
                            self.block_tracker.on_command_end(exit_code);
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

    fn hook(&mut self, _params: &vte::Params, _intermediates: &[u8], _ignore: bool, action: char) {
        self.suppress_joined_scalar = false;
        tracing::trace!(action = ?action, "DCS hook (ignored in v0.1)");
    }

    fn put(&mut self, _byte: u8) {
        self.suppress_joined_scalar = false;
        // DCS data — ignored in v0.1
    }

    fn unhook(&mut self) {
        self.suppress_joined_scalar = false;
        // DCS end — ignored in v0.1
    }
}
