//! Terminal grid: Cell, Row, Grid, Scrollback

use bitflags::bitflags;

bitflags! {
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub struct CellFlags: u16 {
        const BOLD          = 0x0001;
        const ITALIC        = 0x0002;
        const UNDERLINE     = 0x0004;
        const DOUBLE_UNDER  = 0x0008;
        const STRIKETHROUGH = 0x0010;
        const REVERSE       = 0x0020;
        const DIM           = 0x0040;
        const HIDDEN        = 0x0080;
        const DIRTY         = 0x0200;
        const WIDE_SPACER   = 0x0400;
        const CURSOR        = 0x0800;
        const SELECTION     = 0x1000;
        const HYPERLINK     = 0x2000;
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl Color {
    pub const fn rgb(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b, a: 255 }
    }

    pub const DEFAULT_FG: Color = Color::rgb(204, 204, 204);
    pub const DEFAULT_BG: Color = Color::rgb(26, 26, 46);
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CellWidth {
    Half = 1,
    Full = 2,
}

/// Terminal cell (~24 bytes).
/// Design reference: Warp 24-byte Cell + Alacritty sparse extra.
#[derive(Clone, Debug)]
pub struct Cell {
    pub character: char,
    pub fg: Color,
    pub bg: Color,
    pub flags: CellFlags,
    pub width: CellWidth,
}

impl Default for Cell {
    fn default() -> Self {
        Self {
            character: ' ',
            fg: Color::DEFAULT_FG,
            bg: Color::DEFAULT_BG,
            flags: CellFlags::empty(),
            width: CellWidth::Half,
        }
    }
}

impl Cell {
    pub fn with_char(ch: char) -> Self {
        let width = if unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0) > 1 {
            CellWidth::Full
        } else {
            CellWidth::Half
        };
        Self {
            character: ch,
            width,
            ..Self::default()
        }
    }

    pub fn reset(&mut self) {
        *self = Self::default();
    }
}

/// Terminal row with dirty tracking.
/// `dirty_occ` tracks the last modified cell index for efficient rendering.
#[derive(Clone)]
pub struct Row {
    pub cells: Vec<Cell>,
    pub dirty_occ: usize,
    /// Whether this row has been wrapped from the previous line.
    pub wrapped: bool,
}

impl Row {
    pub fn new(cols: usize) -> Self {
        Self {
            cells: vec![Cell::default(); cols],
            dirty_occ: 0,
            wrapped: false,
        }
    }

    pub fn len(&self) -> usize {
        self.cells.len()
    }

    pub fn is_empty(&self) -> bool {
        self.cells.is_empty()
    }

    pub fn is_dirty(&self) -> bool {
        self.dirty_occ > 0
    }

    pub fn clear_dirty(&mut self) {
        self.dirty_occ = 0;
    }

    pub fn mark_dirty(&mut self, col: usize) {
        self.dirty_occ = self.dirty_occ.max(col + 1);
    }
}

/// Cursor position and state.
#[derive(Clone, Debug)]
pub struct Cursor {
    pub row: usize,
    pub col: usize,
    pub visible: bool,
    pub wrap_pending: bool,
}

impl Default for Cursor {
    fn default() -> Self {
        Self {
            row: 0,
            col: 0,
            visible: true,
            wrap_pending: false,
        }
    }
}

/// Cursor style (DECSCUSR).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CursorStyle {
    /// Blinking block █
    BlinkingBlock,
    /// Steady block █
    Block,
    /// Blinking underline _
    BlinkingUnderline,
    /// Steady underline _
    Underline,
    /// Blinking bar |
    BlinkingBar,
    /// Steady bar |
    Bar,
}

impl Default for CursorStyle {
    fn default() -> Self {
        Self::Block
    }
}

/// Scrollback buffer (ring buffer).
/// Stores rows that have scrolled off the top of the viewport.
pub struct Scrollback {
    /// Ring buffer of rows.
    buffer: Vec<Row>,
    /// Maximum number of lines (configurable).
    max_lines: usize,
    /// Head pointer (next write position).
    head: usize,
    /// Current number of occupied lines.
    len: usize,
}

impl Scrollback {
    pub fn new(max_lines: usize) -> Self {
        Self {
            buffer: Vec::with_capacity(max_lines),
            max_lines,
            head: 0,
            len: 0,
        }
    }

    /// Push a row into the scrollback buffer.
    pub fn push(&mut self, row: Row) {
        if self.max_lines == 0 {
            return;
        }
        if self.len < self.max_lines {
            self.buffer.push(row);
            self.len += 1;
            self.head = self.len % self.max_lines;
        } else {
            self.buffer[self.head] = row;
            self.head = (self.head + 1) % self.max_lines;
        }
    }

    /// Pop the most recent row from scrollback (LIFO for scroll-up undo).
    pub fn pop(&mut self) -> Option<Row> {
        if self.len == 0 {
            return None;
        }
        self.len -= 1;
        if self.len < self.buffer.len() {
            // Still within the Vec, just pop
            self.head = self.len % self.max_lines;
            self.buffer.pop()
        } else {
            // Ring buffer wrap-around case
            let idx = if self.head == 0 {
                self.max_lines - 1
            } else {
                self.head - 1
            };
            self.head = idx;
            // Swap out the row
            let cols = self.buffer[idx].cells.len();
            Some(std::mem::replace(&mut self.buffer[idx], Row::new(cols)))
        }
    }

    /// Number of lines in scrollback.
    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Get a row from scrollback by index (0 = oldest, len-1 = newest).
    pub fn get(&self, index: usize) -> Option<&Row> {
        if index >= self.len {
            return None;
        }
        let start = if self.len < self.max_lines {
            0
        } else {
            self.head
        };
        let actual = (start + index) % self.max_lines.min(self.buffer.len());
        self.buffer.get(actual)
    }

    /// Resize the scrollback buffer.
    pub fn resize(&mut self, new_max: usize, cols: usize) {
        if new_max == self.max_lines {
            return;
        }
        if new_max == 0 {
            self.buffer.clear();
            self.len = 0;
            self.head = 0;
            self.max_lines = 0;
            return;
        }

        // Collect rows in order (oldest first)
        let mut rows: Vec<Row> = Vec::with_capacity(new_max);
        for i in 0..self.len.min(new_max) {
            if let Some(row) = self.get(i) {
                rows.push(row.clone());
            }
        }
        // Pad with empty rows if needed
        while rows.len() < new_max.min(self.len) {
            rows.push(Row::new(cols));
        }

        self.buffer = rows;
        self.len = self.buffer.len();
        self.head = self.len % new_max;
        self.max_lines = new_max;
    }

    /// Update maximum lines (without discarding data if growing).
    pub fn set_max_lines(&mut self, max_lines: usize, cols: usize) {
        self.resize(max_lines, cols);
    }
}

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

    pub fn cell(&self, row: usize, col: usize) -> &Cell {
        &self.viewport[row].cells[col]
    }

    pub fn cell_mut(&mut self, row: usize, col: usize) -> &mut Cell {
        let r = &mut self.viewport[row];
        r.mark_dirty(col);
        &mut r.cells[col]
    }

    /// Write a character at the cursor position with given attributes.
    /// Used by VT performer to print with current SGR attributes.
    pub fn write_char_with_attrs(
        &mut self,
        ch: char,
        fg: Color,
        bg: Color,
        flags: CellFlags,
    ) {
        // Reset scroll offset on new output
        self.scroll_offset = 0;

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

        let width = if unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0) > 1 {
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
        let width = if unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0) > 1 {
            CellWidth::Full
        } else {
            CellWidth::Half
        };

        let row = self.cursor.row;
        let col = self.cursor.col;

        if col < self.num_cols {
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

    /// Move cursor to absolute position (1-based params → 0-based).
    pub fn goto(&mut self, row: usize, col: usize) {
        self.cursor.wrap_pending = false;
        self.cursor.row = row.saturating_sub(1).min(self.num_rows - 1);
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
    pub fn index(&mut self) {
        if self.cursor.row == self.scroll_bottom {
            self.scroll_up(1);
        } else if self.cursor.row < self.num_rows - 1 {
            self.cursor.row += 1;
        }
        self.cursor.wrap_pending = false;
    }

    /// Reverse index (ESC M): move cursor up, scrolling at scroll_top.
    pub fn reverse_index(&mut self) {
        if self.cursor.row == self.scroll_top {
            self.scroll_down(1);
        } else if self.cursor.row > 0 {
            self.cursor.row -= 1;
        }
        self.cursor.wrap_pending = false;
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
        let cols = self.num_cols;
        self.scrollback = Scrollback::new(10000);
        self.scroll_offset = 0;
        let _ = (cols, ); // suppress unused warning
    }

    /// Clear line from cursor to end (CSI 0 K).
    pub fn clear_line_right(&mut self) {
        let row = self.cursor.row;
        let col = self.cursor.col;
        for c in col..self.num_cols {
            self.viewport[row].cells[c].reset();
        }
        self.viewport[row].mark_dirty(self.num_cols - 1);
    }

    /// Clear line from start to cursor (CSI 1 K).
    pub fn clear_line_left(&mut self) {
        let row = self.cursor.row;
        let col = self.cursor.col;
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
    pub fn scroll_up(&mut self, n: usize) {
        let top = self.scroll_top;
        let bottom = self.scroll_bottom;

        if n > bottom - top {
            // Push all rows in the scroll region into scrollback
            for i in top..=bottom {
                self.scrollback.push(std::mem::replace(
                    &mut self.viewport[i],
                    Row::new(self.num_cols),
                ));
            }
            return;
        }

        // Push top rows into scrollback (only if scrolling the main viewport)
        if top == 0 {
            for i in 0..n {
                self.scrollback.push(std::mem::replace(
                    &mut self.viewport[i],
                    Row::new(self.num_cols),
                ));
            }
        }

        // Shift rows up
        for i in top..=(bottom - n) {
            self.viewport[i] = std::mem::replace(&mut self.viewport[i + n], Row::new(self.num_cols));
        }
        for i in (bottom - n + 1)..=bottom {
            self.viewport[i] = Row::new(self.num_cols);
        }
    }

    /// Scroll the scroll region down by n lines.
    pub fn scroll_down(&mut self, n: usize) {
        let top = self.scroll_top;
        let bottom = self.scroll_bottom;

        if n > bottom - top {
            for i in top..=bottom {
                self.viewport[i] = Row::new(self.num_cols);
            }
            return;
        }

        for i in (top + n..=bottom).rev() {
            self.viewport[i] = std::mem::replace(&mut self.viewport[i - n], Row::new(self.num_cols));
        }
        for i in top..(top + n) {
            self.viewport[i] = Row::new(self.num_cols);
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
        let available = self.scrollback.len().saturating_sub(self.scroll_offset);
        self.scroll_offset = (self.scroll_offset + lines).min(available);
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
        self.viewport[row].mark_dirty(self.num_cols - 1);
    }

    /// Insert `count` blank lines at cursor row, within scroll region (CSI L).
    pub fn insert_blank_lines(&mut self, count: usize) {
        if self.cursor_in_scroll_region() {
            let row = self.cursor.row;
            let bottom = self.scroll_bottom;
            let shift = count.min(bottom - row + 1);
            for i in (row + shift..=bottom).rev() {
                self.viewport[i] = std::mem::replace(&mut self.viewport[i - shift], Row::new(self.num_cols));
            }
            for i in row..row + shift {
                if i <= bottom {
                    self.viewport[i] = Row::new(self.num_cols);
                }
            }
        }
    }

    /// Delete `count` lines at cursor row, within scroll region (CSI M).
    pub fn delete_lines(&mut self, count: usize) {
        if self.cursor_in_scroll_region() {
            let row = self.cursor.row;
            let bottom = self.scroll_bottom;
            let shift = count.min(bottom - row + 1);
            for i in row..=bottom - shift {
                self.viewport[i] = std::mem::replace(&mut self.viewport[i + shift], Row::new(self.num_cols));
            }
            for i in (bottom - shift + 1)..=bottom {
                self.viewport[i] = Row::new(self.num_cols);
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
    }

    // ── Resize / reset ───────────────────────────────────────────

    pub fn resize(&mut self, new_rows: usize, new_cols: usize) {
        if new_rows == self.num_rows && new_cols == self.num_cols {
            return;
        }

        // Collect all content: scrollback (oldest) + viewport (newest)
        let mut all_rows: Vec<Row> = Vec::new();

        // Add scrollback rows
        for i in 0..self.scrollback.len() {
            if let Some(row) = self.scrollback.get(i) {
                all_rows.push(row.clone());
            }
        }

        // Add viewport rows
        for row in self.viewport.drain(..) {
            all_rows.push(row);
        }

        // Re-wrap rows to new column width
        let mut wrapped_rows: Vec<Row> = Vec::new();
        for row in all_rows {
            let mut current = Row::new(new_cols);
            current.wrapped = false;
            let mut col = 0;

            for cell in row.cells {
                if col >= new_cols {
                    current.wrapped = true;
                    wrapped_rows.push(current);
                    current = Row::new(new_cols);
                    col = 0;
                }

                // Skip wide spacers from old layout
                if cell.flags.contains(CellFlags::WIDE_SPACER) {
                    continue;
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
            wrapped_rows.push(current);
        }

        // Split into scrollback and viewport
        let total = wrapped_rows.len();
        if total <= new_rows {
            // All content fits in viewport
            self.viewport = wrapped_rows;
            while self.viewport.len() < new_rows {
                self.viewport.push(Row::new(new_cols));
            }
            self.scrollback = Scrollback::new(self.scrollback.max_lines);
        } else {
            // Put overflow into scrollback
            let scrollback_count = total - new_rows;
            self.scrollback = Scrollback::new(self.scrollback.max_lines);
            for row in wrapped_rows[..scrollback_count].iter() {
                self.scrollback.push(row.clone());
            }
            self.viewport = wrapped_rows[scrollback_count..].to_vec();
        }

        self.num_rows = new_rows;
        self.num_cols = new_cols;
        self.scroll_bottom = new_rows.saturating_sub(1);
        self.scroll_top = 0;
        self.tabstops = Self::init_tabstops(new_cols);
        self.scroll_offset = 0;

        // Clamp cursor
        self.cursor.row = self.cursor.row.min(new_rows.saturating_sub(1));
        self.cursor.col = self.cursor.col.min(new_cols.saturating_sub(1));
        self.cursor.wrap_pending = false;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grid_new_creates_correct_size() {
        let grid = Grid::new(24, 80);
        assert_eq!(grid.num_rows, 24);
        assert_eq!(grid.num_cols, 80);
        assert_eq!(grid.viewport.len(), 24);
        assert_eq!(grid.viewport[0].cells.len(), 80);
    }

    #[test]
    fn cell_default_is_space() {
        let cell = Cell::default();
        assert_eq!(cell.character, ' ');
        assert_eq!(cell.width, CellWidth::Half);
    }

    #[test]
    fn write_char_advances_cursor() {
        let mut grid = Grid::new(24, 80);
        grid.write_char('A');
        assert_eq!(grid.cursor.row, 0);
        assert_eq!(grid.cursor.col, 1);
        assert_eq!(grid.cell(0, 0).character, 'A');
    }

    #[test]
    fn newline_moves_cursor_down() {
        let mut grid = Grid::new(24, 80);
        grid.write_char('A');
        grid.newline();
        assert_eq!(grid.cursor.row, 1);
        assert_eq!(grid.cursor.col, 0);
    }

    #[test]
    fn scroll_up_at_bottom() {
        let mut grid = Grid::new(5, 4);
        for i in 0..5 {
            grid.viewport[i].cells[0].character =
                char::from_digit(i as u32 + 1, 10).unwrap();
        }
        grid.cursor.row = 4;
        grid.newline();
        assert_eq!(grid.cell(0, 0).character, '2');
        assert_eq!(grid.cell(4, 0).character, ' ');
    }

    #[test]
    fn scroll_up_stores_in_scrollback() {
        let mut grid = Grid::with_scrollback(5, 4, 100);
        for i in 0..5 {
            grid.viewport[i].cells[0].character =
                char::from_digit(i as u32 + 1, 10).unwrap();
        }
        grid.cursor.row = 4;
        grid.newline();
        // Row '1' should be in scrollback
        assert_eq!(grid.scrollback.len(), 1);
        assert_eq!(grid.scrollback.get(0).unwrap().cells[0].character, '1');
    }

    #[test]
    fn scrollback_navigation() {
        let mut grid = Grid::with_scrollback(5, 4, 100);
        // Fill and scroll many lines
        for i in 0..20 {
            grid.viewport[grid.cursor.row].cells[0].character =
                char::from_digit((i % 10) as u32, 10).unwrap_or('X');
            grid.cursor.row = 4;
            grid.newline();
        }

        let sb_len = grid.scrollback.len();
        assert!(sb_len > 0, "should have scrollback entries");

        // Scroll up
        grid.scroll_up_history(3);
        assert_eq!(grid.scroll_offset, 3);

        // Scroll down
        grid.scroll_down_history(1);
        assert_eq!(grid.scroll_offset, 2);

        // Scroll to bottom
        grid.scroll_to_bottom();
        assert_eq!(grid.scroll_offset, 0);

        // Scroll to top
        grid.scroll_to_top();
        assert_eq!(grid.scroll_offset, sb_len);
    }

    #[test]
    fn clear_resets_all_cells() {
        let mut grid = Grid::new(2, 4);
        grid.write_char('X');
        grid.clear();
        assert_eq!(grid.cell(0, 0).character, ' ');
        assert_eq!(grid.cursor.row, 0);
        assert_eq!(grid.cursor.col, 0);
    }

    #[test]
    fn move_up_clamps_at_zero() {
        let mut grid = Grid::new(24, 80);
        grid.cursor.row = 3;
        grid.move_up(1);
        assert_eq!(grid.cursor.row, 2);
        grid.move_up(10);
        assert_eq!(grid.cursor.row, 0);
    }

    #[test]
    fn move_down_clamps_at_bottom() {
        let mut grid = Grid::new(5, 10);
        grid.cursor.row = 3;
        grid.move_down(1);
        assert_eq!(grid.cursor.row, 4);
        grid.move_down(5);
        assert_eq!(grid.cursor.row, 4);
    }

    #[test]
    fn move_forward_clamps_at_right_edge() {
        let mut grid = Grid::new(24, 10);
        grid.cursor.col = 8;
        grid.move_forward(1);
        assert_eq!(grid.cursor.col, 9);
        grid.move_forward(5);
        assert_eq!(grid.cursor.col, 9);
    }

    #[test]
    fn move_backward_clamps_at_zero() {
        let mut grid = Grid::new(24, 80);
        grid.cursor.col = 5;
        grid.move_backward(3);
        assert_eq!(grid.cursor.col, 2);
        grid.move_backward(10);
        assert_eq!(grid.cursor.col, 0);
    }

    #[test]
    fn goto_sets_position() {
        let mut grid = Grid::new(24, 80);
        grid.goto(10, 20); // 1-based → (9, 19)
        assert_eq!(grid.cursor.row, 9);
        assert_eq!(grid.cursor.col, 19);
    }

    #[test]
    fn goto_clamps_to_grid_bounds() {
        let mut grid = Grid::new(24, 80);
        grid.goto(100, 200);
        assert_eq!(grid.cursor.row, 23);
        assert_eq!(grid.cursor.col, 79);
    }

    #[test]
    fn clear_screen_below_clears_from_cursor() {
        let mut grid = Grid::new(5, 5);
        for r in 0..5 {
            for c in 0..5 {
                grid.viewport[r].cells[c].character = 'X';
            }
        }
        grid.cursor.row = 2;
        grid.cursor.col = 2;
        grid.clear_screen_below();
        assert_eq!(grid.cell(0, 0).character, 'X');
        assert_eq!(grid.cell(1, 0).character, 'X');
        assert_eq!(grid.cell(2, 1).character, 'X');
        assert_eq!(grid.cell(2, 2).character, ' ');
        assert_eq!(grid.cell(3, 0).character, ' ');
        assert_eq!(grid.cell(4, 4).character, ' ');
    }

    #[test]
    fn clear_screen_above_clears_to_cursor() {
        let mut grid = Grid::new(5, 5);
        for r in 0..5 {
            for c in 0..5 {
                grid.viewport[r].cells[c].character = 'X';
            }
        }
        grid.cursor.row = 2;
        grid.cursor.col = 2;
        grid.clear_screen_above();
        assert_eq!(grid.cell(0, 0).character, ' ');
        assert_eq!(grid.cell(1, 4).character, ' ');
        assert_eq!(grid.cell(2, 2).character, ' ');
        assert_eq!(grid.cell(2, 3).character, 'X');
        assert_eq!(grid.cell(3, 0).character, 'X');
    }

    #[test]
    fn clear_line_right_clears_from_cursor() {
        let mut grid = Grid::new(5, 5);
        for c in 0..5 {
            grid.viewport[0].cells[c].character = char::from_digit(c as u32 + 1, 10).unwrap();
        }
        grid.cursor.col = 2;
        grid.clear_line_right();
        assert_eq!(grid.cell(0, 1).character, '2');
        assert_eq!(grid.cell(0, 2).character, ' ');
        assert_eq!(grid.cell(0, 4).character, ' ');
    }

    #[test]
    fn clear_line_left_clears_to_cursor() {
        let mut grid = Grid::new(5, 5);
        for c in 0..5 {
            grid.viewport[0].cells[c].character = char::from_digit(c as u32 + 1, 10).unwrap();
        }
        grid.cursor.col = 2;
        grid.clear_line_left();
        assert_eq!(grid.cell(0, 0).character, ' ');
        assert_eq!(grid.cell(0, 2).character, ' ');
        assert_eq!(grid.cell(0, 3).character, '4');
    }

    #[test]
    fn scroll_down_inserts_blank_at_top() {
        let mut grid = Grid::new(5, 4);
        for i in 0..5 {
            grid.viewport[i].cells[0].character =
                char::from_digit(i as u32 + 1, 10).unwrap();
        }
        grid.scroll_down(1);
        assert_eq!(grid.cell(0, 0).character, ' ');
        assert_eq!(grid.cell(1, 0).character, '1');
        assert_eq!(grid.cell(4, 0).character, '4');
    }

    #[test]
    fn save_restore_cursor_roundtrip() {
        let mut grid = Grid::new(24, 80);
        grid.cursor.row = 5;
        grid.cursor.col = 10;
        grid.save_cursor();
        grid.goto(20, 40);
        grid.restore_cursor();
        assert_eq!(grid.cursor.row, 5);
        assert_eq!(grid.cursor.col, 10);
    }

    #[test]
    fn set_scroll_region_bounds() {
        let mut grid = Grid::new(24, 80);
        grid.set_scroll_region(5, 20);
        assert_eq!(grid.scroll_region(), (4, 19));
        assert_eq!(grid.cursor.row, 0);
        assert_eq!(grid.cursor.col, 0);
    }

    #[test]
    fn insert_blank_shifts_right() {
        let mut grid = Grid::new(5, 5);
        grid.viewport[0].cells[0].character = 'A';
        grid.viewport[0].cells[1].character = 'B';
        grid.viewport[0].cells[2].character = 'C';
        grid.cursor.col = 1;
        grid.insert_blank(1);
        assert_eq!(grid.cell(0, 0).character, 'A');
        assert_eq!(grid.cell(0, 1).character, ' ');
        assert_eq!(grid.cell(0, 2).character, 'B');
    }

    #[test]
    fn delete_chars_shifts_left() {
        let mut grid = Grid::new(5, 5);
        grid.viewport[0].cells[0].character = 'A';
        grid.viewport[0].cells[1].character = 'B';
        grid.viewport[0].cells[2].character = 'C';
        grid.cursor.col = 1;
        grid.delete_chars(1);
        assert_eq!(grid.cell(0, 0).character, 'A');
        assert_eq!(grid.cell(0, 1).character, 'C');
        assert_eq!(grid.cell(0, 4).character, ' ');
    }

    #[test]
    fn index_scrolls_at_bottom() {
        let mut grid = Grid::new(5, 4);
        for i in 0..5 {
            grid.viewport[i].cells[0].character =
                char::from_digit(i as u32 + 1, 10).unwrap();
        }
        grid.cursor.row = 4;
        grid.index();
        assert_eq!(grid.cell(0, 0).character, '2');
        assert_eq!(grid.cell(4, 0).character, ' ');
    }

    #[test]
    fn reverse_index_scrolls_at_top() {
        let mut grid = Grid::new(5, 4);
        for i in 0..5 {
            grid.viewport[i].cells[0].character =
                char::from_digit(i as u32 + 1, 10).unwrap();
        }
        grid.cursor.row = 0;
        grid.reverse_index();
        assert_eq!(grid.cell(0, 0).character, ' ');
        assert_eq!(grid.cell(1, 0).character, '1');
    }

    #[test]
    fn tab_advance_moves_to_next_tabstop() {
        let mut grid = Grid::new(24, 80);
        grid.cursor.col = 0;
        grid.advance_tab(1);
        assert_eq!(grid.cursor.col, 8);
        grid.advance_tab(1);
        assert_eq!(grid.cursor.col, 16);
    }

    #[test]
    fn back_tab_moves_to_previous_tabstop() {
        let mut grid = Grid::new(24, 80);
        grid.cursor.col = 16;
        grid.back_tab(1);
        assert_eq!(grid.cursor.col, 8);
        grid.back_tab(1);
        assert_eq!(grid.cursor.col, 0);
    }

    #[test]
    fn carriage_return_resets_col() {
        let mut grid = Grid::new(24, 80);
        grid.cursor.col = 50;
        grid.carriage_return();
        assert_eq!(grid.cursor.col, 0);
    }

    #[test]
    fn backspace_moves_left() {
        let mut grid = Grid::new(24, 80);
        grid.cursor.col = 5;
        grid.backspace();
        assert_eq!(grid.cursor.col, 4);
        grid.backspace();
        grid.backspace();
        grid.backspace();
        grid.backspace();
        grid.backspace();
        assert_eq!(grid.cursor.col, 0);
    }

    #[test]
    fn write_char_with_attrs_uses_provided_colors() {
        let mut grid = Grid::new(24, 80);
        let red = Color::rgb(255, 0, 0);
        let blue = Color::rgb(0, 0, 255);
        grid.write_char_with_attrs('X', red, blue, CellFlags::BOLD);
        assert_eq!(grid.cell(0, 0).character, 'X');
        assert_eq!(grid.cell(0, 0).fg, red);
        assert_eq!(grid.cell(0, 0).bg, blue);
        assert!(grid.cell(0, 0).flags.contains(CellFlags::BOLD));
    }

    #[test]
    fn erase_chars_clears_count_cells() {
        let mut grid = Grid::new(5, 5);
        for c in 0..5 {
            grid.viewport[0].cells[c].character = char::from_digit(c as u32 + 1, 10).unwrap();
        }
        grid.cursor.col = 1;
        grid.erase_chars(2);
        assert_eq!(grid.cell(0, 0).character, '1');
        assert_eq!(grid.cell(0, 1).character, ' ');
        assert_eq!(grid.cell(0, 2).character, ' ');
        assert_eq!(grid.cell(0, 3).character, '4');
    }

    #[test]
    fn tabstops_initialized_every_8() {
        let grid = Grid::new(24, 80);
        assert!(grid.tabstops[0]);
        assert!(grid.tabstops[8]);
        assert!(grid.tabstops[16]);
        assert!(!grid.tabstops[1]);
        assert!(!grid.tabstops[7]);
    }

    #[test]
    fn resize_preserves_content() {
        let mut grid = Grid::with_scrollback(5, 5, 100);
        for c in 0..5 {
            grid.viewport[0].cells[c].character = char::from_digit(c as u32 + 1, 10).unwrap();
        }
        grid.resize(5, 10);
        assert_eq!(grid.num_cols, 10);
        assert_eq!(grid.cell(0, 0).character, '1');
        assert_eq!(grid.cell(0, 4).character, '5');
    }

    #[test]
    fn scrollback_ring_buffer_overflow() {
        let mut grid = Grid::with_scrollback(3, 4, 5);
        // Scroll more lines than scrollback can hold
        for i in 0..10 {
            grid.viewport[2].cells[0].character =
                char::from_digit(i as u32 % 10, 10).unwrap_or('X');
            grid.cursor.row = 2;
            grid.newline();
        }
        // Scrollback should be capped at 5
        assert_eq!(grid.scrollback.len(), 5);
    }

    #[test]
    fn write_output_resets_scroll_offset() {
        let mut grid = Grid::with_scrollback(5, 4, 100);
        // Scroll up into history
        for i in 0..10 {
            grid.viewport[4].cells[0].character =
                char::from_digit(i as u32 % 10, 10).unwrap_or('X');
            grid.cursor.row = 4;
            grid.newline();
        }
        grid.scroll_up_history(3);
        assert_eq!(grid.scroll_offset, 3);
        // Writing new output should reset scroll offset
        grid.write_char_with_attrs('A', Color::DEFAULT_FG, Color::DEFAULT_BG, CellFlags::empty());
        assert_eq!(grid.scroll_offset, 0);
    }
}
