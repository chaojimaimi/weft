use super::{CapturedStyle, OutputCapture};

impl OutputCapture {
    pub(crate) fn carriage_return(&mut self) {
        self.cursor = self.line_start();
        self.char_cursor = self.line_start_char;
    }

    pub(crate) fn set_cursor_column(&mut self, column: usize, max_bytes: usize) {
        let start = self.line_start();
        let end = self.line_end();
        let line = &self.text[start..end];
        let mut display_col = 0usize;
        let mut byte_offset = line.len();
        let mut char_offset = line.chars().count() as u32;
        for (index, (byte, ch)) in line.char_indices().enumerate() {
            let width = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
            if display_col >= column || display_col + width > column {
                byte_offset = byte;
                char_offset = index as u32;
                break;
            }
            display_col += width;
        }
        self.cursor = start + byte_offset;
        self.char_cursor = self.line_start_char + char_offset;
        while display_col < column && self.text.len() < max_bytes {
            self.print(' ', CapturedStyle::default(), max_bytes);
            display_col += 1;
        }
    }

    pub(crate) fn move_cursor_columns(&mut self, delta: isize, max_bytes: usize) {
        let start = self.line_start();
        let current = crate::grid::terminal_text_width(&self.text[start..self.cursor]);
        self.set_cursor_column(current.saturating_add_signed(delta), max_bytes);
    }

    pub(crate) fn backspace(&mut self) {
        let start = self.line_start();
        if self.cursor > start {
            self.cursor -= 1;
            while !self.text.is_char_boundary(self.cursor) {
                self.cursor -= 1;
            }
            self.char_cursor = self.char_cursor.saturating_sub(1);
        }
    }
}
