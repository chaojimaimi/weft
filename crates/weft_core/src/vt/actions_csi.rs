//! `csi_dispatch` body for the `vte::Perform` trait impl. perform.rs keeps
//! the single trait block and delegates here (v1.13.8 S2 zero-behavior
//! file-budget split; body moved verbatim, `impl Terminal` cross-file block
//! per the kitty_keyboard.rs / screen_exit precedent).

use super::param;
use super::replies;
use super::Terminal;
use crate::grid::CursorStyle;

impl Terminal {
    pub(super) fn csi_dispatch(
        &mut self,
        params: &vte::Params,
        intermediates: &[u8],
        _ignore: bool,
        action: char,
    ) {
        self.suppress_joined_scalar = false;
        // DECRQM private-mode query; report DEC 2026 synchronized output.
        if action == 'p' && intermediates == b"?$" {
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
        if intermediates == b"?" && matches!(action, 'h' | 'l') {
            let set = action == 'h';
            for sub in params.iter() {
                if let &[mode] = sub {
                    self.handle_dec_private_mode(mode, set);
                }
            }
            return;
        }

        // Primary-screen TUI classification: every counted addressing op
        // feeds `primary_screen_cursor_ops` (the >= 2 takeover/scroll
        // threshold). v1.10.38 excludes horizontal hops C/D here — see
        // capture_cursor::is_primary_screen_addressing for the byte-level
        // rationale. Also traces cursor-moving CSIs for cursor-desync forensics.
        if super::capture_cursor::is_primary_screen_addressing(action, param(params, 0, 1)) {
            // v1.11.8 (PLAN_v1118 M-C1): the negation of
            // `is_absolute_primary_screen_addressing` moved here — the
            // deleted `primary_screen_absolute_addressing` flag was the
            // param's only dedicated consumer; `relative_addressing_seen`
            // still needs the inverted bit.
            self.note_primary_screen_cursor_addressing(
                !super::capture_cursor::is_absolute_primary_screen_addressing(
                    action,
                    param(params, 0, 1),
                ),
            );
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
            'c' if intermediates == b">" => {
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
            // Kitty keyboard-protocol ops (`CSI >flags u` push / `CSI <u`
            // pop / `CSI =flags;mode u` set / `CSI ? u` query) arrive with
            // the leading byte in intermediates (vte collects `<=>?` there)
            // and must NOT fall through to the bare-`CSI u` DECRC restore
            // below. The dispatch (enabled gate, dual-stack, M1 in-place
            // set) lives in kitty_keyboard.rs (PLAN_v1114 §1.2); the DEC
            // private-mode gate above is h/l-only, so `CSI ? u` never leaks
            // into handle_dec_private_mode.
            'u' if !intermediates.is_empty() => {
                self.kitty_keyboard_op(intermediates, params);
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
            'q' if intermediates == b">" => {
                self.respond(&replies::xtversion_reply());
            }
            // Cursor style (DECSCUSR — CSI <n> q)
            'q' => {
                if intermediates.is_empty() || intermediates == b" " {
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
}
