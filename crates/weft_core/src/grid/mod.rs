// arch-gate: allow-over-800
//! Terminal grid: Cell, Row, Grid, Scrollback
mod cell;
mod cursor;
mod display;
mod row;
mod scrollback;

pub use cell::{
    terminal_char_width, terminal_grapheme_glyph, terminal_text_width, Cell, CellColor, CellFlags,
    CellWidth, Color,
};
pub use cursor::{Cursor, CursorStyle};
pub use row::Row;
pub use scrollback::Scrollback;

#[cfg(test)]
mod tests;

/// Terminal grid: visible viewport + scrollback buffer.
pub struct Grid {
    pub viewport: Vec<Row>,
    pub num_rows: usize,
    pub num_cols: usize,
    pub cursor: Cursor,
    saved_cursor: Option<Cursor>,
    scroll_top: usize,
    scroll_bottom: usize,
    /// Tab stop positions (true = tab stop, false = no stop).
    tabstops: Vec<bool>,
    /// Scrollback buffer for lines scrolled off the top.
    pub scrollback: Scrollback,
    /// Current scroll offset (0 = no scroll, >0 = viewing history).
    pub scroll_offset: usize,
    /// v1.0 P0-c: Pending viewport scroll delta for the renderer. Positive =
    /// rows scrolled up (content moved up, new blank rows at bottom).
    /// Negative = rows scrolled down (content moved down, new blank rows at
    /// top). The renderer reads this via [`take_pending_scroll`] to shift
    /// its per-row vertex cache, avoiding a full rebuild on scroll.
    pending_scroll: std::cell::Cell<i32>,
}

impl Grid {
    pub fn new(rows: usize, cols: usize) -> Self {
        Self::with_scrollback(rows, cols, 10000)
    }

    pub fn with_scrollback(rows: usize, cols: usize, scrollback_lines: usize) -> Self {
        let scroll_bottom = rows.saturating_sub(1);
        let tabstops = Self::init_tabstops(cols);
        Self {
            viewport: (0..rows).map(|_| Row::new(cols)).collect(),
            num_rows: rows,
            num_cols: cols,
            cursor: Cursor::default(),
            saved_cursor: None,
            scroll_top: 0,
            scroll_bottom,
            tabstops,
            scrollback: Scrollback::new(scrollback_lines),
            scroll_offset: 0,
            pending_scroll: std::cell::Cell::new(0),
        }
    }

    /// Initialize tab stops every 8 columns.
    fn init_tabstops(cols: usize) -> Vec<bool> {
        let mut stops = vec![false; cols + 1];
        for i in (0..cols).step_by(8) {
            stops[i] = true;
        }
        stops
    }

    // ── Cell access ──────────────────────────────────────────────

    /// Read a cell for rendering.
    ///
    /// When the user has scrolled up (`scroll_offset > 0`), the viewport is a
    /// window into the combined `[scrollback] ++ [viewport]` line sequence.
    /// Row `r` of the visible window maps to global line
    /// `scrollback.len() - offset + r`: lines below `scrollback.len()` come
    /// from history, the rest from the live viewport.
    pub fn cell(&self, row: usize, col: usize) -> &Cell {
        let sb_len = self.scrollback.len();
        let offset = self.scroll_offset.min(sb_len);
        if offset > 0 {
            let global = sb_len - offset + row;
            if global < sb_len {
                if let Some(history_row) = self.scrollback.get(global) {
                    return &history_row.cells[col];
                }
            } else {
                return &self.viewport[global - sb_len].cells[col];
            }
        }
        &self.viewport[row].cells[col]
    }

    pub fn cell_mut(&mut self, row: usize, col: usize) -> &mut Cell {
        let r = &mut self.viewport[row];
        r.mark_dirty(col);
        &mut r.cells[col]
    }

    /// Extract a single **live viewport** row's text — skipping wide-char
    /// spacers and trimming trailing blank/default cells. `row` is a viewport
    /// index (like `cursor.row`). Used to snapshot the command line at OSC
    /// 133;B (the prompt row, before any output scrolls it into history).
    pub fn row_text(&self, row: usize) -> String {
        if row >= self.num_rows {
            return String::new();
        }
        let cells = &self.viewport[row].cells;
        // Extent: index after the last non-blank cell (blank = never-written
        // space / NUL).
        let last = cells
            .iter()
            .take(self.num_cols)
            .rposition(|c| c.character != ' ' && c.character != '\0')
            .map(|i| i + 1)
            .unwrap_or(0);
        let mut out = String::with_capacity(last);
        for cell in cells.iter().take(last) {
            if cell.flags.contains(CellFlags::WIDE_SPACER) {
                continue;
            }
            out.push(if cell.character == '\0' {
                ' '
            } else {
                cell.character
            });
        }
        out
    }

    /// Write a character at the cursor position with given attributes.
    /// Used by VT performer to print with current SGR attributes.
    pub fn write_char_with_attrs(
        &mut self,
        ch: char,
        fg: CellColor,
        bg: CellColor,
        flags: CellFlags,
    ) {
        // Note: scroll_offset reset is handled by the caller (Terminal::print)
        // which knows the shell phase. Resetting here unconditionally would
        // destroy the user's scroll position during AtPrompt idle when the
        // shell re-renders its prompt.

        // Handle deferred wrap before writing
        if self.cursor.wrap_pending {
            self.cursor.wrap_pending = false;
            self.cursor.col = 0;
            if self.cursor.row == self.scroll_bottom {
                self.scroll_up(1);
            } else if self.cursor.row < self.num_rows - 1 {
                self.cursor.row += 1;
            }
            // Mark the row as wrapped
            if self.cursor.row > 0 {
                self.viewport[self.cursor.row - 1].wrapped = true;
            }
        }

        let width = if terminal_char_width(ch) > 1 {
            CellWidth::Full
        } else {
            CellWidth::Half
        };

        let row = self.cursor.row;
        let col = self.cursor.col;

        // If wide char would straddle line boundary, wrap first
        if width == CellWidth::Full && col + 1 >= self.num_cols {
            // Move to next line
            self.cursor.col = 0;
            if self.cursor.row == self.scroll_bottom {
                self.scroll_up(1);
            } else if self.cursor.row < self.num_rows - 1 {
                self.cursor.row += 1;
            }
            // Mark the row as wrapped
            if self.cursor.row > 0 {
                self.viewport[self.cursor.row - 1].wrapped = true;
            }
            // Recalculate row/col after wrap
            return self.write_char_with_attrs(ch, fg, bg, flags);
        }

        if col < self.num_cols {
            self.viewport[row].clear_wide_pair_at(col);
            if width == CellWidth::Full {
                self.viewport[row].clear_wide_pair_at(col + 1);
            }
            let cell = &mut self.viewport[row].cells[col];
            cell.character = ch;
            cell.fg = fg;
            cell.bg = bg;
            cell.flags = flags | CellFlags::DIRTY;
            cell.width = width;

            self.viewport[row].mark_dirty(col);
            self.cursor.col += width as usize;

            // Mark spacer cell for wide characters
            if width == CellWidth::Full && col + 1 < self.num_cols {
                let spacer = &mut self.viewport[row].cells[col + 1];
                spacer.character = ' ';
                spacer.flags = CellFlags::WIDE_SPACER;
                spacer.width = CellWidth::Half;
                self.viewport[row].mark_dirty(col + 1);
            }
        }

        if self.cursor.col >= self.num_cols {
            self.cursor.wrap_pending = true;
            self.cursor.col = self.num_cols - 1;
        }
    }

    /// Write a character at the cursor position and advance.
    /// Preserves existing fg/bg (for direct/test use).
    pub fn write_char(&mut self, ch: char) {
        let width = if terminal_char_width(ch) > 1 {
            CellWidth::Full
        } else {
            CellWidth::Half
        };

        let row = self.cursor.row;
        let col = self.cursor.col;

        if col < self.num_cols {
            self.viewport[row].clear_wide_pair_at(col);
            if width == CellWidth::Full {
                self.viewport[row].clear_wide_pair_at(col + 1);
            }
            // Read current attributes before mutable borrow
            let fg = self.viewport[row].cells[col].fg;
            let bg = self.viewport[row].cells[col].bg;

            let cell = &mut self.viewport[row].cells[col];
            cell.character = ch;
            cell.fg = fg;
            cell.bg = bg;
            cell.flags.insert(CellFlags::DIRTY);
            cell.width = width;

            self.viewport[row].mark_dirty(col);

            self.cursor.col += width as usize;

            if width == CellWidth::Full && col + 1 < self.num_cols {
                let spacer = &mut self.viewport[row].cells[col + 1];
                spacer.character = ' ';
                spacer.flags = CellFlags::WIDE_SPACER;
                spacer.width = CellWidth::Half;
                self.viewport[row].mark_dirty(col + 1);
            }
        }

        // Handle wrap
        if self.cursor.col >= self.num_cols {
            self.cursor.wrap_pending = true;
            self.cursor.col = self.num_cols - 1;
        }
    }

    // ── Cursor movement ──────────────────────────────────────────

    /// Move cursor up by `rows` lines, clamping at scroll_top or row 0.
    pub fn move_up(&mut self, rows: usize) {
        self.cursor.wrap_pending = false;
        let min_row = if self.cursor_in_scroll_region() {
            self.scroll_top
        } else {
            0
        };
        self.cursor.row = self.cursor.row.saturating_sub(rows).max(min_row);
    }

    /// Move cursor down by `rows` lines, clamping at scroll_bottom or last row.
    pub fn move_down(&mut self, rows: usize) {
        self.cursor.wrap_pending = false;
        let max_row = if self.cursor_in_scroll_region() {
            self.scroll_bottom
        } else {
            self.num_rows - 1
        };
        self.cursor.row = (self.cursor.row + rows).min(max_row);
    }

    /// Move cursor forward (right) by `cols` columns.
    pub fn move_forward(&mut self, cols: usize) {
        self.cursor.wrap_pending = false;
        self.cursor.col = (self.cursor.col + cols).min(self.num_cols - 1);
    }

    /// Move cursor backward (left) by `cols` columns.
    pub fn move_backward(&mut self, cols: usize) {
        self.cursor.wrap_pending = false;
        self.cursor.col = self.cursor.col.saturating_sub(cols);
    }

    /// Move cursor to absolute row (0-based).
    pub fn set_cursor_row(&mut self, row: usize) {
        self.cursor.wrap_pending = false;
        self.cursor.row = row.min(self.num_rows - 1);
    }

    /// Move cursor to absolute column (0-based).
    pub fn set_cursor_col(&mut self, col: usize) {
        self.cursor.wrap_pending = false;
        self.cursor.col = col.min(self.num_cols - 1);
    }

    /// Move cursor to the position given by CUP/HVP (1-based params).
    ///
    /// Under DECOM (origin mode), the row is relative to the scroll-region top
    /// and clamped to the region — full-screen TUI apps (e.g. `claude`, vim)
    /// set a scroll region + DECOM and expect CUP to be region-relative. We
    /// support only vertical margins (DECSTBM); columns stay absolute
    /// (no DECSLRM left/right margins).
    pub fn goto(&mut self, row: usize, col: usize, origin_mode: bool) {
        self.cursor.wrap_pending = false;
        let (origin_row, max_row) = if origin_mode {
            (self.scroll_top, self.scroll_bottom)
        } else {
            (0, self.num_rows - 1)
        };
        let r = row.saturating_sub(1);
        self.cursor.row = (origin_row + r).min(max_row);
        self.cursor.col = col.saturating_sub(1).min(self.num_cols - 1);
    }

    /// Carriage return: move cursor to column 0.
    pub fn carriage_return(&mut self) {
        self.cursor.wrap_pending = false;
        self.cursor.col = 0;
    }

    /// Backspace: move cursor left one column (clamped at 0).
    pub fn backspace(&mut self) {
        if self.cursor.wrap_pending {
            self.cursor.wrap_pending = false;
        } else {
            self.cursor.col = self.cursor.col.saturating_sub(1);
        }
    }

    // ── Cursor save/restore ──────────────────────────────────────

    pub fn save_cursor(&mut self) {
        self.saved_cursor = Some(self.cursor.clone());
    }

    pub fn restore_cursor(&mut self) {
        if let Some(saved) = &self.saved_cursor {
            self.cursor = saved.clone();
        }
    }

    // ── Line feed / index ────────────────────────────────────────

    /// Index (ESC D / LF): move cursor down, scrolling at scroll_bottom.
    /// Move cursor down one row, scrolling the region if at the bottom.
    /// Returns `true` if a scroll occurred (callers with viewport-relative
    /// side state — e.g. OSC 8 cell_map — should invalidate it).
    pub fn index(&mut self) -> bool {
        let scrolled = if self.cursor.row == self.scroll_bottom {
            self.scroll_up(1);
            true
        } else if self.cursor.row < self.num_rows - 1 {
            self.cursor.row += 1;
            false
        } else {
            false
        };
        self.cursor.wrap_pending = false;
        scrolled
    }

    /// Reverse index (ESC M): move cursor up, scrolling at scroll_top.
    /// Returns `true` if a scroll occurred (see [`index`](Self::index)).
    pub fn reverse_index(&mut self) -> bool {
        let scrolled = if self.cursor.row == self.scroll_top {
            self.scroll_down(1);
            true
        } else if self.cursor.row > 0 {
            self.cursor.row -= 1;
            false
        } else {
            false
        };
        self.cursor.wrap_pending = false;
        scrolled
    }

    /// Move cursor to next line (newline).
    pub fn newline(&mut self) {
        self.cursor.wrap_pending = false;
        self.cursor.col = 0;

        if self.cursor.row == self.scroll_bottom {
            self.scroll_up(1);
        } else if self.cursor.row < self.num_rows - 1 {
            self.cursor.row += 1;
        }
    }

    // ── Clearing ─────────────────────────────────────────────────

    /// Clear screen from cursor to bottom (CSI 0 J).
    pub fn clear_screen_below(&mut self) {
        let row = self.cursor.row;
        let col = self.cursor.col;
        // Clear from cursor to end of current line
        self.viewport[row].clear_wide_pair_at(col);
        for c in col..self.num_cols {
            self.viewport[row].cells[c].reset();
        }
        self.viewport[row].mark_dirty(self.num_cols - 1);
        // Clear all subsequent rows
        for r in (row + 1)..self.num_rows {
            for cell in &mut self.viewport[r].cells {
                cell.reset();
            }
            self.viewport[r].mark_dirty(self.num_cols - 1);
        }
    }

    /// Clear screen from top to cursor (CSI 1 J).
    pub fn clear_screen_above(&mut self) {
        let row = self.cursor.row;
        let col = self.cursor.col;
        // Clear all preceding rows
        for r in 0..row {
            for cell in &mut self.viewport[r].cells {
                cell.reset();
            }
            self.viewport[r].mark_dirty(self.num_cols - 1);
        }
        // Clear from start of current line to cursor
        self.viewport[row].clear_wide_pair_at(col);
        for c in 0..=col {
            self.viewport[row].cells[c].reset();
        }
        self.viewport[row].mark_dirty(col);
    }

    /// Clear entire screen (CSI 2 J).
    pub fn clear_screen_all(&mut self) {
        for row in &mut self.viewport {
            for cell in &mut row.cells {
                cell.reset();
            }
            row.mark_dirty(self.num_cols - 1);
        }
        // Note: does NOT reset cursor position (VT behavior)
    }

    /// Clear scrollback buffer (CSI 3 J).
    pub fn clear_scrollback(&mut self) {
        // Preserve the configured capacity (don't reset to a hardcoded default).
        let max_lines = self.scrollback.max_lines;
        self.scrollback = Scrollback::new(max_lines);
        self.scroll_offset = 0;
    }

    /// Clear line from cursor to end (CSI 0 K).
    pub fn clear_line_right(&mut self) {
        let row = self.cursor.row;
        let col = self.cursor.col;
        self.viewport[row].clear_wide_pair_at(col);
        for c in col..self.num_cols {
            self.viewport[row].cells[c].reset();
        }
        self.viewport[row].mark_dirty(self.num_cols - 1);
    }

    /// Clear line from start to cursor (CSI 1 K).
    pub fn clear_line_left(&mut self) {
        let row = self.cursor.row;
        let col = self.cursor.col;
        self.viewport[row].clear_wide_pair_at(col);
        for c in 0..=col {
            self.viewport[row].cells[c].reset();
        }
        self.viewport[row].mark_dirty(col);
    }

    /// Clear entire line (CSI 2 K).
    pub fn clear_line_all(&mut self) {
        let row = self.cursor.row;
        for cell in &mut self.viewport[row].cells {
            cell.reset();
        }
        self.viewport[row].mark_dirty(self.num_cols - 1);
    }

    /// Erase `count` characters starting at cursor (CSI X).
    /// Does not move cursor.
    pub fn erase_chars(&mut self, count: usize) {
        let row = self.cursor.row;
        let col = self.cursor.col;
        let end = (col + count).min(self.num_cols);
        if col < end {
            self.viewport[row].clear_wide_pair_at(col);
            self.viewport[row].clear_wide_pair_at(end - 1);
        }
        for c in col..end {
            self.viewport[row].cells[c].reset();
        }
        if end > 0 {
            self.viewport[row].mark_dirty(end - 1);
        }
    }

    // ── Scrolling ────────────────────────────────────────────────

    /// Scroll the scroll region up by n lines.
    /// Lines scrolled off the top go into the scrollback buffer.
    ///
    /// For full-viewport scrolls (the common streaming case), records a
    /// `pending_scroll` delta so the renderer can shift its per-row vertex
    /// cache — an O(n) optimization over a full rebuild. For scroll-region
    /// scrolls (DECSTBM, used by TUI apps like `less`), the renderer's cache
    /// shift can't be used because it operates on the entire cache while the
    /// scroll only affected `[top..=bottom]`. In that case, all rows in the
    /// scroll region are marked dirty so the renderer rebuilds them.
    pub fn scroll_up(&mut self, n: usize) {
        let top = self.scroll_top;
        let bottom = self.scroll_bottom;
        let full_viewport = top == 0 && bottom == self.num_rows - 1;

        if n > bottom - top {
            // Push all rows in the scroll region into scrollback
            if top == 0 {
                for i in top..=bottom {
                    self.scrollback.push(std::mem::replace(
                        &mut self.viewport[i],
                        Row::new(self.num_cols),
                    ));
                }
            } else {
                for i in top..=bottom {
                    self.viewport[i].clear();
                }
            }
            if full_viewport {
                self.pending_scroll
                    .set(self.pending_scroll.get() + (bottom - top + 1) as i32);
            } else {
                // Scroll region: mark affected rows dirty for rebuild.
                for i in top..=bottom {
                    self.viewport[i].mark_dirty(self.num_cols - 1);
                }
            }
            return;
        }

        if full_viewport {
            // v1.0 perf: Full-viewport scroll using rotate_left.
            // For n=1 (the common streaming case): 1 Row alloc (was 2 with
            // drain+extend, was ~24 with the old shift loop).
            // rotate_left moves [0] to [n-1], shifts [1..] to [0..n-1].
            // We take the old [0] into scrollback first, insert a fresh
            // empty Row at [0], then rotate — the empty Row ends up at [n-1].
            for _ in 0..n {
                let old_top = std::mem::replace(&mut self.viewport[0], Row::new(self.num_cols));
                self.scrollback.push(old_top);
                self.viewport.rotate_left(1);
            }
            self.pending_scroll
                .set(self.pending_scroll.get() + n as i32);
        } else {
            // Scroll region (or partial viewport): rotate in place, then
            // clear the exposed bottom rows. For top==0, push the old top
            // rows to scrollback before rotating.
            if top == 0 {
                for i in 0..n {
                    self.scrollback.push(std::mem::replace(
                        &mut self.viewport[i],
                        Row::new(self.num_cols),
                    ));
                }
            }
            self.viewport[top..=bottom].rotate_left(n);
            for i in (bottom + 1 - n)..=bottom {
                self.viewport[i].clear();
            }
            // Mark all rows in the scroll region dirty — the renderer's
            // per-row cache is position-relative and can't be shifted for a
            // partial-region scroll, so rebuild all affected rows.
            for i in top..=bottom {
                self.viewport[i].mark_dirty(self.num_cols - 1);
            }
        }
    }

    /// Scroll the scroll region down by n lines.
    ///
    /// Like [`scroll_up`](Self::scroll_up), only full-viewport scrolls use
    /// `pending_scroll` for the renderer cache shift. Scroll-region scrolls
    /// mark affected rows dirty instead.
    pub fn scroll_down(&mut self, n: usize) {
        let top = self.scroll_top;
        let bottom = self.scroll_bottom;
        let full_viewport = top == 0 && bottom == self.num_rows - 1;

        if n > bottom - top {
            for i in top..=bottom {
                self.viewport[i].clear();
            }
            if full_viewport {
                self.pending_scroll
                    .set(self.pending_scroll.get() - (bottom - top + 1) as i32);
            } else {
                for i in top..=bottom {
                    self.viewport[i].mark_dirty(self.num_cols - 1);
                }
            }
            return;
        }

        // v1.0 perf: rotate_right + clear — zero allocations (was O(num_rows)).
        self.viewport[top..=bottom].rotate_right(n);
        for i in top..(top + n) {
            self.viewport[i].clear();
        }
        if full_viewport {
            self.pending_scroll
                .set(self.pending_scroll.get() - n as i32);
        } else {
            for i in top..=bottom {
                self.viewport[i].mark_dirty(self.num_cols - 1);
            }
        }
    }

    /// Set scroll region (CSI r). Parameters are 1-based.
    pub fn set_scroll_region(&mut self, top: usize, bottom: usize) {
        let top = top.saturating_sub(1);
        let bottom = bottom.saturating_sub(1).min(self.num_rows - 1);
        if top < bottom {
            self.scroll_top = top;
            self.scroll_bottom = bottom;
            // Move cursor to home position
            self.cursor.row = 0;
            self.cursor.col = 0;
            self.cursor.wrap_pending = false;
        }
    }

    /// Reset scroll region to full viewport.
    pub fn reset_scroll_region(&mut self) {
        self.scroll_top = 0;
        self.scroll_bottom = self.num_rows - 1;
    }

    // ── Scrollback navigation ────────────────────────────────────

    /// Scroll viewport up (view older history).
    pub fn scroll_up_history(&mut self, lines: usize) {
        // Offset can never exceed the number of available history lines;
        // clamping to `scrollback.len()` keeps `cell()` indexing in bounds.
        let max = self.scrollback.len();
        self.scroll_offset = (self.scroll_offset + lines).min(max);
    }

    /// Scroll viewport down (view newer content).
    pub fn scroll_down_history(&mut self, lines: usize) {
        self.scroll_offset = self.scroll_offset.saturating_sub(lines);
    }

    /// Scroll to the very top of history.
    pub fn scroll_to_top(&mut self) {
        self.scroll_offset = self.scrollback.len();
    }

    /// Scroll to the bottom (current output).
    pub fn scroll_to_bottom(&mut self) {
        self.scroll_offset = 0;
    }

    /// Check if we're viewing history (scrolled up).
    pub fn is_scrolled(&self) -> bool {
        self.scroll_offset > 0
    }

    /// Get the total number of scrollback lines.
    pub fn scrollback_len(&self) -> usize {
        self.scrollback.len()
    }

    // ── Character insertion/deletion ─────────────────────────────

    /// Insert `count` blank cells at cursor, shifting existing cells right (CSI @).
    pub fn insert_blank(&mut self, count: usize) {
        let row = self.cursor.row;
        let col = self.cursor.col;
        let cells = &mut self.viewport[row].cells;

        let shift = count.min(self.num_cols - col);
        // Shift cells right
        for i in (col + shift..self.num_cols).rev() {
            cells[i] = std::mem::take(&mut cells[i - shift]);
        }
        self.viewport[row].repair_wide_pairs();
        self.viewport[row].mark_dirty(self.num_cols - 1);
    }

    /// Delete `count` cells at cursor, shifting remaining cells left (CSI P).
    pub fn delete_chars(&mut self, count: usize) {
        let row = self.cursor.row;
        let col = self.cursor.col;
        let cells = &mut self.viewport[row].cells;

        let shift = count.min(self.num_cols - col);
        // Shift cells left
        for i in col..self.num_cols - shift {
            cells[i] = std::mem::take(&mut cells[i + shift]);
        }
        self.viewport[row].repair_wide_pairs();
        self.viewport[row].mark_dirty(self.num_cols - 1);
    }

    /// Insert `count` blank lines at cursor row, within scroll region (CSI L).
    pub fn insert_blank_lines(&mut self, count: usize) {
        if self.cursor_in_scroll_region() {
            let row = self.cursor.row;
            let bottom = self.scroll_bottom;
            let shift = count.min(bottom - row + 1);
            if shift == 0 {
                return;
            }
            for i in (row + shift..=bottom).rev() {
                self.viewport[i] =
                    std::mem::replace(&mut self.viewport[i - shift], Row::new(self.num_cols));
            }
            for i in row..row + shift {
                if i <= bottom {
                    self.viewport[i] = Row::new(self.num_cols);
                }
            }
            // Mark all affected rows dirty — the renderer's per-row cache is
            // position-relative and can't be shifted for a partial-region
            // operation, so all moved + blanked rows must be rebuilt.
            for i in row..=bottom {
                self.viewport[i].mark_dirty(self.num_cols - 1);
            }
        }
    }

    /// Delete `count` lines at cursor row, within scroll region (CSI M).
    pub fn delete_lines(&mut self, count: usize) {
        if self.cursor_in_scroll_region() {
            let row = self.cursor.row;
            let bottom = self.scroll_bottom;
            let shift = count.min(bottom - row + 1);
            if shift == 0 {
                return;
            }
            let region_len = bottom - row + 1;
            if shift < region_len {
                for i in row..=bottom - shift {
                    self.viewport[i] =
                        std::mem::replace(&mut self.viewport[i + shift], Row::new(self.num_cols));
                }
            }
            for i in (bottom + 1 - shift)..=bottom {
                self.viewport[i] = Row::new(self.num_cols);
            }
            // Mark all affected rows dirty — same rationale as insert_blank_lines.
            for i in row..=bottom {
                self.viewport[i].mark_dirty(self.num_cols - 1);
            }
        }
    }

    // ── Tab stops ────────────────────────────────────────────────

    /// Set a tab stop at the current cursor column (ESC H / HTS).
    pub fn set_tabstop(&mut self) {
        if self.cursor.col < self.tabstops.len() {
            self.tabstops[self.cursor.col] = true;
        }
    }

    /// Clear tab stop at cursor column (CSI 0g).
    pub fn clear_tabstop(&mut self) {
        if self.cursor.col < self.tabstops.len() {
            self.tabstops[self.cursor.col] = false;
        }
    }

    /// Clear all tab stops (CSI 3g).
    pub fn clear_all_tabstops(&mut self) {
        for stop in &mut self.tabstops {
            *stop = false;
        }
    }

    /// Advance cursor to next tab stop (CSI I / HT).
    pub fn advance_tab(&mut self, count: usize) {
        self.cursor.wrap_pending = false;
        for _ in 0..count {
            let mut next = self.cursor.col + 1;
            while next < self.num_cols && next < self.tabstops.len() {
                if self.tabstops[next] {
                    break;
                }
                next += 1;
            }
            self.cursor.col = next.min(self.num_cols - 1);
        }
    }

    /// Move cursor back to previous tab stop (CSI Z).
    pub fn back_tab(&mut self, count: usize) {
        self.cursor.wrap_pending = false;
        for _ in 0..count {
            if self.cursor.col == 0 {
                break;
            }
            let mut prev = self.cursor.col;
            loop {
                if prev == 0 {
                    break;
                }
                prev -= 1;
                if self.tabstops[prev] {
                    break;
                }
            }
            self.cursor.col = prev;
        }
    }

    // ── Dirty tracking helpers ───────────────────────────────────

    pub fn mark_all_dirty(&mut self) {
        for row in &mut self.viewport {
            row.dirty_occ = row.cells.len();
        }
    }

    pub fn clear_all_dirty(&mut self) {
        for row in &mut self.viewport {
            row.clear_dirty();
        }
        // v1.0 P0-c: also clear pending_scroll so stale scroll deltas don't
        // trigger cache shifts on frames where the renderer didn't observe
        // the scroll (e.g., force_full took precedence).
        self.pending_scroll.set(0);
    }

    /// v1.0 P0-b Layer 1: Iterate over dirty viewport rows.
    ///
    /// Yields `(row_idx, dirty_col_extent)` — the row index and the number of
    /// leading cells that may have changed (0..extent). Callers should rebuild
    /// vertices for these rows only; clean rows can be reused from a cache.
    ///
    /// **Scrollback caveat**: only the live viewport is tracked. When
    /// `scroll_offset > 0`, rendered cells come from scrollback history (which
    /// has no dirty flags) — callers MUST force a full redraw in that case.
    pub fn dirty_rows(&self) -> impl Iterator<Item = (usize, usize)> + '_ {
        self.viewport.iter().enumerate().filter_map(|(i, r)| {
            if r.dirty_occ > 0 {
                Some((i, r.dirty_occ))
            } else {
                None
            }
        })
    }

    /// v1.0 P0-b Layer 1: Whether ANY viewport row is dirty.
    pub fn has_dirty(&self) -> bool {
        self.viewport.iter().any(|r| r.dirty_occ > 0)
    }

    /// v1.0 P0-c: Take and reset the pending viewport scroll delta.
    /// The renderer calls this to read how many rows the viewport shifted
    /// since the last frame, then shifts its per-row vertex cache
    /// accordingly. Returns 0 when no scroll occurred.
    pub fn take_pending_scroll(&self) -> i32 {
        self.pending_scroll.take()
    }

    /// v1.0 fix: discard the pending scroll delta AND mark every viewport row
    /// dirty. Used for alt-screen apps (vim/less/man) after a scroll: those
    /// apps repaint their whole screen after scrolling, so the renderer's
    /// scroll-blit + per-row cache-shift optimization (designed for shell
    /// streaming where moved rows keep their content) is WRONG here — it
    /// shifts stale content that the app is about to overwrite, producing the
    /// "only the top row moves, rows overlap and merge" rendering corruption.
    /// Forcing a full rebuild (no blit) makes alt-screen scrolls correct.
    pub fn discard_scroll_and_dirty_all(&mut self) {
        self.pending_scroll.set(0);
        self.mark_all_dirty();
    }

    // ── Resize / reset ───────────────────────────────────────────

    /// Dimension-only resize: change `num_rows`/`num_cols` and reshape the
    /// viewport rows **without reflowing** content. Rows are truncated or
    /// blank-padded to `new_cols`; the viewport is grown with blank rows or
    /// truncated to `new_rows`. The cursor is clamped into range.
    ///
    /// This is the correct resize for the **active** grid when an alt-screen
    /// TUI app (less/vim/man) is running. Those apps paint their content with
    /// absolute cursor positioning at a specific width, and they repaint
    /// themselves on SIGWINCH. A reflow (the full `resize`) would relocate
    /// their characters to wrong cells mid-drag, producing the "content
    /// squished into the top-left corner" artifact — because the grid gets
    /// rewrapped at the new width while the app still thinks it drew at the
    /// old width (SIGWINCH is debounced and only delivered after the drag
    /// settles). Matching Alacritty/Warp, we keep the active grid's layout
    /// untouched and let the app repaint on SIGWINCH.
    ///
    /// The full reflow in [`resize`](Self::resize) is still used for the
    /// inactive grid and for the primary screen at a shell prompt (where
    /// scrollback rewrapping is expected and there is no TUI app to repaint).
    pub fn resize_dims(&mut self, new_rows: usize, new_cols: usize) {
        if new_rows == self.num_rows && new_cols == self.num_cols {
            return;
        }
        let old_cols = self.num_cols;

        // Reshape each existing viewport row to the new width: truncate if
        // narrower, pad with blank cells if wider. Do NOT merge/split rows —
        // the app owns the layout.
        if new_cols != old_cols {
            for row in &mut self.viewport {
                resize_row_cells(&mut row.cells, new_cols);
                // Truncation may have dropped the rightmost dirty cell; mark
                // the whole row dirty so the renderer repaints it fully.
                row.mark_dirty(new_cols.saturating_sub(1));
            }
        }

        // Grow or truncate the viewport to the new row count.
        if new_rows > self.num_rows {
            let extra = new_rows - self.num_rows;
            // Add blank rows at the BOTTOM (common convention: TUI apps clear
            // newly exposed rows themselves on SIGWINCH).
            for _ in 0..extra {
                self.viewport.push(Row::new(new_cols));
            }
        } else if new_rows < self.num_rows {
            self.viewport.truncate(new_rows);
        }

        self.num_rows = new_rows;
        self.num_cols = new_cols;
        self.scroll_bottom = new_rows.saturating_sub(1);
        self.scroll_top = 0;
        self.tabstops = Self::init_tabstops(new_cols);
        self.scroll_offset = 0;

        // Clamp the cursor into the new bounds. The app will reposition it on
        // its next paint; clamping here just keeps internal invariants safe.
        self.cursor.row = self.cursor.row.min(new_rows.saturating_sub(1));
        self.cursor.col = self.cursor.col.min(new_cols.saturating_sub(1));
        self.cursor.wrap_pending = false;

        self.mark_all_dirty();
    }

    pub fn resize(&mut self, new_rows: usize, new_cols: usize) {
        if new_rows == self.num_rows && new_cols == self.num_cols {
            return;
        }

        // ── Phase 1: Collect all rows ────────────────────────────────
        let mut all_rows: Vec<Row> = Vec::new();
        for i in 0..self.scrollback.len() {
            if let Some(row) = self.scrollback.get(i) {
                all_rows.push(row.clone());
            }
        }
        let scrollback_len = all_rows.len();
        for row in self.viewport.drain(..) {
            all_rows.push(row);
        }
        let old_cursor_all_idx = scrollback_len + self.cursor.row;

        // ── Phase 2: Group into logical lines ────────────────────────
        // A logical line is a sequence of rows where 2nd+ rows have
        // wrapped=true. Merging wrapped rows into one cell buffer allows
        // proper reflow: narrowing wraps, widening unwraps.
        struct LogicalLine {
            cells: Vec<Cell>,
            has_cursor: bool,
            cursor_buf_offset: usize,
        }

        let mut lines: Vec<LogicalLine> = Vec::new();
        let mut merge_buf: Vec<Cell> = Vec::new();
        let mut merge_has_cursor = false;
        let mut merge_cursor_offset: usize = 0;

        let flush_line =
            |buf: Vec<Cell>, has_cur: bool, cur_off: usize, lines: &mut Vec<LogicalLine>| {
                let empty =
                    !has_cur && buf.iter().all(|c| c.character == ' ' && c.flags.is_empty());
                if !empty {
                    lines.push(LogicalLine {
                        cells: buf,
                        has_cursor: has_cur,
                        cursor_buf_offset: cur_off,
                    });
                }
            };

        // Track the PREVIOUS row's wrapped flag. wrapped=true means
        // "this row's content continues on the next row", so we check
        // prev_wrapped to detect if the current row is a continuation.
        let mut prev_wrapped = false;

        for (all_idx, row) in all_rows.into_iter().enumerate() {
            let is_cursor_row = all_idx == old_cursor_all_idx;

            // Content extent: trim trailing BLANK cells (never-written defaults).
            // This matters for wrapped rows too: when a full-width char would
            // straddle the right margin the print path wraps *before* placing
            // it, leaving the last cell as a never-written default. Treating
            // that cell as content (the old `row.cells.len()` for wrapped rows)
            // baked a phantom space into the logical line on every reflow,
            // compounding into growing gaps between CJK characters. A written
            // space is preserved because writes always set the DIRTY flag, so
            // `!flags.is_empty()` keeps it.
            let content_end = row
                .cells
                .iter()
                .rposition(|c| c.character != ' ' || !c.flags.is_empty())
                .map(|i| i + 1)
                .unwrap_or(0);

            let is_continuation = prev_wrapped && !merge_buf.is_empty();
            prev_wrapped = row.wrapped;

            if !is_continuation {
                // Flush previous logical line
                if !merge_buf.is_empty() {
                    flush_line(
                        std::mem::take(&mut merge_buf),
                        merge_has_cursor,
                        merge_cursor_offset,
                        &mut lines,
                    );
                }
                merge_has_cursor = false;
                merge_cursor_offset = 0;
            }

            // Track cursor offset in the merged buffer.
            // Use cursor.col directly — the cursor can legitimately be beyond
            // content (e.g. after a CSI cursor-move on an empty line at col 5).
            if is_cursor_row {
                merge_cursor_offset = merge_buf.len() + self.cursor.col;
                merge_has_cursor = true;
            }

            merge_buf.extend(row.cells.iter().take(content_end).cloned());
        }
        // Flush last line
        if !merge_buf.is_empty() {
            flush_line(merge_buf, merge_has_cursor, merge_cursor_offset, &mut lines);
        }

        // ── Phase 3: Rewrap each logical line ────────────────────────
        let mut wrapped_rows: Vec<Row> = Vec::new();
        let mut cursor_wrap_start = 0;
        let mut new_cursor_col = 0;

        for line in &lines {
            let line_start = wrapped_rows.len();
            if line.has_cursor {
                cursor_wrap_start = line_start;
            }

            let mut current = Row::new(new_cols);
            current.wrapped = false;
            let mut col: usize = 0;

            for (buf_idx, cell) in line.cells.iter().enumerate() {
                // Record cursor position when we reach its offset
                if line.has_cursor && buf_idx == line.cursor_buf_offset {
                    new_cursor_col = col;
                }

                // Wrap to next sub-row if current is full
                if col >= new_cols {
                    current.wrapped = true;
                    wrapped_rows.push(current);
                    current = Row::new(new_cols);
                    col = 0;
                    if line.has_cursor && buf_idx == line.cursor_buf_offset {
                        new_cursor_col = 0;
                    }
                }

                // Skip wide spacers from old layout
                if cell.flags.contains(CellFlags::WIDE_SPACER) {
                    continue;
                }

                // Wide char at last column doesn't fit — wrap first.
                if cell.width == CellWidth::Full && col + 1 >= new_cols && col > 0 {
                    current.wrapped = true;
                    wrapped_rows.push(current);
                    current = Row::new(new_cols);
                    col = 0;
                    if line.has_cursor && buf_idx == line.cursor_buf_offset {
                        new_cursor_col = 0;
                    }
                }

                if col < new_cols {
                    current.cells[col] = cell.clone();
                    current.mark_dirty(col);

                    if cell.width == CellWidth::Full && col + 1 < new_cols {
                        current.cells[col + 1].character = ' ';
                        current.cells[col + 1].flags = CellFlags::WIDE_SPACER;
                        current.cells[col + 1].width = CellWidth::Half;
                        current.mark_dirty(col + 1);
                    }

                    col += cell.width as usize;
                }
            }
            // Handle cursor at end of content (beyond all cells)
            if line.has_cursor && line.cursor_buf_offset >= line.cells.len() {
                if col >= new_cols {
                    current.wrapped = true;
                    wrapped_rows.push(current);
                    current = Row::new(new_cols);
                    col = 0;
                }
                new_cursor_col = col;
            }
            wrapped_rows.push(current);
        }

        // ── Phase 4: Split into scrollback + viewport ────────────────
        let total = wrapped_rows.len();
        let (vp_start, new_cursor_row) = if total <= new_rows {
            let cursor_row = cursor_wrap_start.min(total.saturating_sub(1));
            (0, cursor_row)
        } else {
            let ideal_start = cursor_wrap_start.saturating_sub(new_rows.saturating_sub(1));
            let max_start = total.saturating_sub(new_rows);
            let vp_start = ideal_start.min(max_start);
            let cursor_row = cursor_wrap_start.saturating_sub(vp_start);
            (vp_start, cursor_row)
        };

        if total <= new_rows {
            let mut vp = Vec::with_capacity(new_rows);
            vp.extend(wrapped_rows);
            vp.resize(new_rows, Row::new(new_cols));
            self.viewport = vp;
            self.scrollback = Scrollback::new(self.scrollback.max_lines);
        } else {
            self.scrollback = Scrollback::new(self.scrollback.max_lines);
            for row in wrapped_rows[..vp_start].iter() {
                self.scrollback.push(row.clone());
            }
            self.viewport = wrapped_rows[vp_start..vp_start + new_rows].to_vec();
        }

        self.num_rows = new_rows;
        self.num_cols = new_cols;
        self.scroll_bottom = new_rows.saturating_sub(1);
        self.scroll_top = 0;
        self.tabstops = Self::init_tabstops(new_cols);
        self.scroll_offset = 0;

        self.cursor.row = new_cursor_row.min(new_rows.saturating_sub(1));
        self.cursor.col = new_cursor_col.min(new_cols.saturating_sub(1));
        self.cursor.wrap_pending = false;
        // v1.0 P0-b: all rows are new/rearranged after a reflow.
        self.mark_all_dirty();
    }

    /// Clear the entire screen and reset cursor.
    pub fn clear(&mut self) {
        for row in &mut self.viewport {
            for cell in &mut row.cells {
                cell.reset();
            }
            row.mark_dirty(self.num_cols - 1);
        }
        self.cursor = Cursor::default();
    }

    // ── Internal helpers ─────────────────────────────────────────

    /// Check if cursor is within the scroll region.
    fn cursor_in_scroll_region(&self) -> bool {
        self.cursor.row >= self.scroll_top && self.cursor.row <= self.scroll_bottom
    }

    /// Get scroll region boundaries (read-only).
    pub fn scroll_region(&self) -> (usize, usize) {
        (self.scroll_top, self.scroll_bottom)
    }
}

/// Resize a single row's cell vector to `new_cols` in place: truncate if
/// narrower, pad with default (blank) cells if wider. No content is moved
/// between rows — this preserves the app's per-cell layout exactly, which is
/// the point of the dimension-only alt-screen resize.
fn resize_row_cells(cells: &mut Vec<Cell>, new_cols: usize) {
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
