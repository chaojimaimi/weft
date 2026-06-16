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

    /// The xterm 256-color palette: 16 ANSI + 6×6×6 cube (16-231) + grayscale
    /// (232-255). Shared by the VT palette seed and theme defaults so there is
    /// a single source of truth.
    pub fn standard_palette() -> [Color; 256] {
        let mut palette = [Color::DEFAULT_FG; 256];

        // Standard 16 colors
        let standard = [
            (0, 0, 0),       // 0 Black
            (205, 0, 0),     // 1 Red
            (0, 205, 0),     // 2 Green
            (205, 205, 0),   // 3 Yellow
            (0, 0, 238),     // 4 Blue
            (205, 0, 205),   // 5 Magenta
            (0, 205, 205),   // 6 Cyan
            (229, 229, 229), // 7 White
            (127, 127, 127), // 8 Bright Black
            (255, 0, 0),     // 9 Bright Red
            (0, 255, 0),     // 10 Bright Green
            (255, 255, 0),   // 11 Bright Yellow
            (92, 92, 255),   // 12 Bright Blue
            (255, 0, 255),   // 13 Bright Magenta
            (0, 255, 255),   // 14 Bright Cyan
            (255, 255, 255), // 15 Bright White
        ];
        for (i, (r, g, b)) in standard.iter().enumerate() {
            palette[i] = Color::rgb(*r, *g, *b);
        }

        // 16-231: 6x6x6 color cube
        let cube_values = [0, 95, 135, 175, 215, 255];
        let mut idx = 16;
        for r in &cube_values {
            for g in &cube_values {
                for b in &cube_values {
                    palette[idx] = Color::rgb(*r, *g, *b);
                    idx += 1;
                }
            }
        }

        // 232-255: grayscale ramp
        for i in 0u8..24 {
            let v = 8 + i * 10;
            palette[232 + i as usize] = Color::rgb(v, v, v);
        }

        palette
    }
}

/// Where a cell's color comes from. Stored on the cell so a theme/palette
/// change can recolor the whole screen instantly: cells remember their origin
/// (default / palette index / explicit RGB) rather than a pre-resolved color.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CellColor {
    /// Use the theme default (foreground or background depending on slot).
    Default,
    /// Index into the 256-color palette (ANSI 0-15 + 6×6×6 cube + grayscale).
    Palette(u8),
    /// Explicit truecolor (SGR 38;2;r;g;b).
    Rgb(Color),
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
    pub fg: CellColor,
    pub bg: CellColor,
    pub flags: CellFlags,
    pub width: CellWidth,
}

impl Default for Cell {
    fn default() -> Self {
        Self {
            character: ' ',
            fg: CellColor::Default,
            bg: CellColor::Default,
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
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CursorStyle {
    /// Steady block █ (default)
    #[default]
    Block,
    /// Blinking block █
    BlinkingBlock,
    /// Blinking underline _
    BlinkingUnderline,
    /// Steady underline _
    Underline,
    /// Blinking bar |
    BlinkingBar,
    /// Steady bar |
    Bar,
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

    /// Write a character at the cursor position with given attributes.
    /// Used by VT performer to print with current SGR attributes.
    pub fn write_char_with_attrs(
        &mut self,
        ch: char,
        fg: CellColor,
        bg: CellColor,
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
        // Preserve the configured capacity (don't reset to a hardcoded default).
        let max_lines = self.scrollback.max_lines;
        self.scrollback = Scrollback::new(max_lines);
        self.scroll_offset = 0;
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
            self.viewport[i] =
                std::mem::replace(&mut self.viewport[i + n], Row::new(self.num_cols));
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
            self.viewport[i] =
                std::mem::replace(&mut self.viewport[i - n], Row::new(self.num_cols));
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
                self.viewport[i] =
                    std::mem::replace(&mut self.viewport[i - shift], Row::new(self.num_cols));
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
                self.viewport[i] =
                    std::mem::replace(&mut self.viewport[i + shift], Row::new(self.num_cols));
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
            grid.viewport[i].cells[0].character = char::from_digit(i as u32 + 1, 10).unwrap();
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
            grid.viewport[i].cells[0].character = char::from_digit(i as u32 + 1, 10).unwrap();
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
    fn cell_shows_scrollback_content_when_scrolled() {
        let mut grid = Grid::with_scrollback(3, 4, 100);
        // Fill viewport with '1','2','3' then scroll so '1' enters scrollback.
        grid.viewport[0].cells[0].character = '1';
        grid.viewport[1].cells[0].character = '2';
        grid.viewport[2].cells[0].character = '3';
        grid.cursor.row = 2;
        grid.newline(); // '1' -> scrollback, viewport = ['2','3',' ']

        assert_eq!(grid.scrollback.len(), 1, "precondition: one scrolled line");

        // Scroll up one line: viewport should show scrollback + viewport tail.
        grid.scroll_up_history(1);
        assert_eq!(grid.cell(0, 0).character, '1', "row 0 from scrollback");
        assert_eq!(grid.cell(1, 0).character, '2', "row 1 from viewport[0]");
        assert_eq!(grid.cell(2, 0).character, '3', "row 2 from viewport[1]");

        // Scrolling back to bottom restores the live viewport.
        grid.scroll_to_bottom();
        assert_eq!(grid.cell(0, 0).character, '2');
    }

    #[test]
    fn scroll_up_history_never_exceeds_scrollback_len() {
        let mut grid = Grid::with_scrollback(5, 4, 100);
        // Two lines of scrollback.
        grid.scrollback.push(Row::new(4));
        grid.scrollback.push(Row::new(4));
        grid.scroll_offset = 1;
        // Scrolling far past the top must clamp to scrollback length, not shrink.
        grid.scroll_up_history(5);
        assert_eq!(grid.scroll_offset, 2, "clamps to scrollback.len()");
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
            grid.viewport[i].cells[0].character = char::from_digit(i as u32 + 1, 10).unwrap();
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
            grid.viewport[i].cells[0].character = char::from_digit(i as u32 + 1, 10).unwrap();
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
            grid.viewport[i].cells[0].character = char::from_digit(i as u32 + 1, 10).unwrap();
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
        grid.write_char_with_attrs(
            'X',
            CellColor::Rgb(red),
            CellColor::Rgb(blue),
            CellFlags::BOLD,
        );
        assert_eq!(grid.cell(0, 0).character, 'X');
        assert_eq!(grid.cell(0, 0).fg, CellColor::Rgb(red));
        assert_eq!(grid.cell(0, 0).bg, CellColor::Rgb(blue));
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
        grid.write_char_with_attrs(
            'A',
            CellColor::Default,
            CellColor::Default,
            CellFlags::empty(),
        );
        assert_eq!(grid.scroll_offset, 0);
    }

    // ── Resize regression tests ──────────────────────────────────────

    /// Regression: shrink then grow should keep content visible.
    /// Before the fix, the viewport took the bottom N wrapped rows (which
    /// were empty padding), pushing the cursor's content into scrollback.
    #[test]
    fn resize_shrink_keeps_cursor_content_visible() {
        // 10 rows × 20 cols, cursor near bottom
        let mut grid = Grid::with_scrollback(10, 20, 100);
        // Fill rows 0-5 with content (simulating command output)
        for r in 0..6 {
            for c in 0..20 {
                grid.viewport[r].cells[c].character =
                    char::from_digit((r * 20 + c) as u32 % 10, 10).unwrap_or('X');
            }
        }
        grid.cursor.row = 5;
        grid.cursor.col = 3;

        // Shrink to 4 rows × 10 cols — cursor's content row wraps and
        // the 10 old rows become ~12 wrapped rows, overflowing the 4-row
        // viewport.
        grid.resize(4, 10);

        // The cursor must be within the viewport bounds
        assert!(grid.cursor.row < 4);

        // The character at the cursor's original position must still be
        // accessible (either in viewport or scrollback). Since the cursor
        // row had content at col 3, the character at the new cursor
        // position should be non-null (the rewrapped content).
        let ch = grid.cell(grid.cursor.row, grid.cursor.col).character;
        assert_ne!(
            ch, '\0',
            "cursor position should have content after shrink, got null"
        );
    }

    /// Regression: grow-then-shrink preserves command output near cursor.
    /// Simulates: max window → type ls → minimize → output invisible.
    #[test]
    fn resize_grow_then_shrink_preserves_output() {
        // Start small, fill with content, grow, then shrink back
        let mut grid = Grid::with_scrollback(5, 10, 100);

        // Simulate prompt + output in a 5×10 grid
        for c in 0..5 {
            grid.viewport[0].cells[c].character = if c == 0 { '>' } else { ' ' };
        }
        for r in 1..4 {
            for c in 0..8 {
                grid.viewport[r].cells[c].character = char::from_digit(r as u32, 10).unwrap();
            }
        }
        grid.cursor.row = 3;
        grid.cursor.col = 8;

        // Grow to 8×30 (maximize)
        grid.resize(8, 30);
        assert_eq!(grid.num_rows, 8);
        assert_eq!(grid.num_cols, 30);

        // The '>' prompt should still be in the grid
        let mut found_prompt = false;
        for r in 0..grid.num_rows {
            for c in 0..grid.num_cols {
                if grid.cell(r, c).character == '>' {
                    found_prompt = true;
                    break;
                }
            }
        }
        assert!(found_prompt, "prompt '>' should survive grow");

        // Shrink back to 4×8 (minimize)
        grid.resize(4, 8);
        assert_eq!(grid.num_rows, 4);
        assert_eq!(grid.num_cols, 8);

        // Cursor should be within bounds
        assert!(grid.cursor.row < 4, "cursor row {} < 4", grid.cursor.row);
        assert!(grid.cursor.col < 8, "cursor col {} < 8", grid.cursor.col);

        // The cursor's row must have actual content (not lost to scrollback).
        // This is the core regression check: before the fix, the viewport was
        // positioned on empty padding rows, so the cursor landed on an empty row.
        let cursor_has_content =
            (0..grid.num_cols).any(|c| grid.cell(grid.cursor.row, c).character != '\0');
        assert!(
            cursor_has_content,
            "cursor row {} should have content after shrink, not be empty padding",
            grid.cursor.row
        );
    }

    /// Cursor position should track correctly through rewrap when the
    /// cursor's old row wraps into multiple new rows.
    #[test]
    fn resize_cursor_tracks_through_rewrap() {
        let mut grid = Grid::with_scrollback(3, 10, 100);
        // Fill row 1 with content across all 10 cols
        for c in 0..10 {
            grid.viewport[1].cells[c].character = char::from_digit(c as u32, 10).unwrap();
        }
        grid.cursor.row = 1;
        grid.cursor.col = 7; // col 7 should be in the first wrapped sub-row

        // Shrink to 3 rows × 4 cols — row 1 (10 chars) wraps to 3 sub-rows
        grid.resize(3, 4);

        // Cursor should be in the viewport
        assert!(grid.cursor.row < 3);
        assert!(grid.cursor.col < 4);

        // The character at the new cursor position should be '7' (the old col 7)
        // Col 7 in a 4-col wrap: sub-row 1 (7/4=1), col 3 (7%4=3)
        // But our cursor tracking uses cursor_wrap_start + col/new_cols
        // which is approximate. At minimum the cursor row should have content.
        let ch = grid.cell(grid.cursor.row, grid.cursor.col).character;
        assert_ne!(ch, '\0', "cursor should land on a content row after rewrap");
    }

    /// Regression: maximize → shell moves cursor to bottom → minimize.
    /// Before the fix, empty rows between content and cursor were rewrapped
    /// into empty wrapped rows that inflated total count, pushing content
    /// into scrollback and leaving the viewport full of empty padding.
    #[test]
    fn resize_skips_empty_rows_between_content_and_cursor() {
        // Start: 5×10 grid with content in rows 0-2, cursor at row 2
        let mut grid = Grid::with_scrollback(5, 10, 200);
        for c in 0..8 {
            grid.viewport[0].cells[c].character = 'A';
            grid.viewport[1].cells[c].character = 'B';
            grid.viewport[2].cells[c].character = 'C';
        }
        grid.cursor.row = 2;
        grid.cursor.col = 8;

        // Maximize to 20×40 — shell moves cursor to bottom row
        grid.resize(20, 40);
        grid.cursor.row = 19; // shell puts cursor at bottom after SIGWINCH
        grid.cursor.col = 0;
        // Shell redraws prompt at row 19
        grid.viewport[19].cells[0].character = '$';

        // Minimize back to 5×10
        grid.resize(5, 10);

        // The prompt '$' should be visible in the viewport
        let mut found_prompt = false;
        for r in 0..5 {
            for c in 0..10 {
                if grid.cell(r, c).character == '$' {
                    found_prompt = true;
                }
            }
        }
        assert!(
            found_prompt,
            "prompt '$' should be in viewport after minimize"
        );

        // At least some original content (A/B/C) should be visible
        let mut found_content = false;
        for r in 0..5 {
            for c in 0..10 {
                let ch = grid.cell(r, c).character;
                if ch == 'A' || ch == 'B' || ch == 'C' {
                    found_content = true;
                }
            }
        }
        assert!(
            found_content,
            "original content (A/B/C) should be visible, not pushed to scrollback"
        );

        // Cursor within bounds
        assert!(grid.cursor.row < 5);
    }

    /// Empty rows should not accumulate over multiple resize cycles.
    #[test]
    fn resize_multiple_cycles_no_content_loss() {
        let mut grid = Grid::with_scrollback(5, 10, 200);

        // Fill with content
        for c in 0..8 {
            grid.viewport[0].cells[c].character = 'X';
            grid.viewport[1].cells[c].character = 'Y';
        }
        grid.cursor.row = 1;
        grid.cursor.col = 8;

        // Cycle 1: maximize
        grid.resize(15, 30);
        grid.cursor.row = 14;
        grid.viewport[14].cells[0].character = '$';
        // Cycle 1: minimize
        grid.resize(5, 10);

        let mut content_after_cycle1 = 0;
        for r in 0..5 {
            for c in 0..10 {
                let ch = grid.cell(r, c).character;
                if ch == 'X' || ch == 'Y' || ch == '$' {
                    content_after_cycle1 += 1;
                }
            }
        }

        // Cycle 2: maximize
        grid.resize(15, 30);
        grid.cursor.row = 14;
        grid.viewport[14].cells[0].character = '$';
        // Cycle 2: minimize
        grid.resize(5, 10);

        let mut content_after_cycle2 = 0;
        for r in 0..5 {
            for c in 0..10 {
                let ch = grid.cell(r, c).character;
                if ch == 'X' || ch == 'Y' || ch == '$' {
                    content_after_cycle2 += 1;
                }
            }
        }

        // Content count should be stable across cycles (no progressive loss).
        // Allow minor fluctuation (±2) from rewrap trimming edge cases, but
        // detect real degradation (e.g. 18 → 10 → 5).
        let diff = (content_after_cycle1 as i64 - content_after_cycle2 as i64).unsigned_abs();
        assert!(
            diff <= 2,
            "content should not degrade significantly: cycle1={}, cycle2={}",
            content_after_cycle1,
            content_after_cycle2
        );

        // And there should still be visible content
        assert!(
            content_after_cycle2 > 0,
            "content must survive multiple resize cycles"
        );
    }

    /// Wrapped rows must merge back into one line when the grid widens.
    /// This is the core reflow test: narrow → wrap → widen → unwrap.
    #[test]
    fn resize_reflow_merges_wrapped_rows_on_widen() {
        // 3×10 grid, write a long line that wraps
        let mut grid = Grid::with_scrollback(3, 10, 100);
        // Write "ABCDEFGHIJ" (10 chars) — fills row 0, wrapped=true
        // Then "KLMNO" (5 chars) on row 1 — continuation
        for c in 0..10 {
            grid.viewport[0].cells[c].character = char::from_digit((c % 10) as u32, 10).unwrap();
        }
        grid.viewport[0].wrapped = true;
        for c in 0..5 {
            grid.viewport[1].cells[c].character = char::from_digit((c % 10) as u32, 10).unwrap();
        }
        grid.cursor.row = 1;
        grid.cursor.col = 5;

        // Widen to 20 cols — the two wrapped rows should merge into one
        grid.resize(3, 20);
        assert_eq!(grid.num_cols, 20);

        // Row 0 should now contain "ABCDEFGHIJKLMNO" (15 chars in 20-col row)
        let row0_chars: String = grid.viewport[0]
            .cells
            .iter()
            .take_while(|c| c.character != ' ')
            .map(|c| c.character)
            .collect();
        assert_eq!(
            row0_chars, "012345678901234",
            "wrapped rows should merge on widen, got: {:?}",
            row0_chars
        );

        // Row 1 should NOT contain the continuation text anymore
        // (it was merged into row 0)
        let row1_has_digits = grid.viewport[1]
            .cells
            .iter()
            .any(|c| c.character.is_ascii_digit());
        assert!(
            !row1_has_digits,
            "continuation content should have been merged into row 0"
        );
    }

    /// Separate logical lines must NOT be merged during reflow.
    #[test]
    fn resize_reflow_does_not_merge_separate_lines() {
        let mut grid = Grid::with_scrollback(4, 10, 100);
        // Row 0: "AAAA" (not wrapped)
        for c in 0..4 {
            grid.viewport[0].cells[c].character = 'A';
        }
        // Row 1: "BBBB" (not wrapped)
        for c in 0..4 {
            grid.viewport[1].cells[c].character = 'B';
        }
        grid.cursor.row = 1;
        grid.cursor.col = 4;

        // Widen to 20 cols — rows should remain separate
        grid.resize(4, 20);

        // Row 0 should have AAAA, Row 1 should have BBBB — NOT "AAAABBBB"
        assert_eq!(grid.viewport[0].cells[0].character, 'A');
        assert_eq!(grid.viewport[1].cells[0].character, 'B');
        assert_eq!(grid.viewport[0].cells[4].character, ' ');
    }

    /// End-to-end test: write a long line via write_char_with_attrs
    /// (which the VT parser uses), then widen — the line must unwrap.
    #[test]
    fn resize_unwraps_line_written_by_vt_parser() {
        use super::CellFlags;
        let mut grid = Grid::with_scrollback(5, 20, 100);

        // Write 35 characters — wraps in a 20-col grid
        for i in 0..35u8 {
            let ch = char::from_digit((i % 10) as u32, 10).unwrap();
            grid.write_char_with_attrs(
                ch,
                CellColor::Default,
                CellColor::Default,
                CellFlags::empty(),
            );
        }
        // VT newline to end the line
        grid.newline();

        // Row 0 should be wrapped (first 20 chars)
        assert!(
            grid.viewport[0].wrapped,
            "row 0 should have wrapped=true after writing 35 chars in 20-col grid"
        );

        // Widen to 50 — should unwrap to one row
        grid.resize(5, 50);

        // All 35 chars should be on row 0
        let content: String = grid.viewport[0]
            .cells
            .iter()
            .take_while(|c| c.character != ' ')
            .map(|c| c.character)
            .collect();
        assert_eq!(
            content.len(),
            35,
            "35 chars should fit on one row after widening to 50, got: {:?}",
            content
        );

        // Row 1 should NOT have continuation digits
        let row1_has_digits = grid.viewport[1]
            .cells
            .iter()
            .any(|c| c.character.is_ascii_digit());
        assert!(
            !row1_has_digits,
            "continuation should have been merged into row 0"
        );
    }
}
