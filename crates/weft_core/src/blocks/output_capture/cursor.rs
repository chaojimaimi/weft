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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::blocks::MAX_OUTPUT_BYTES;

    /// `set_cursor_column` past the end of the line pads with spaces so a
    /// subsequent print lands at the requested column.
    #[test]
    fn set_cursor_column_pads_to_column_with_spaces() {
        let mut output = OutputCapture::default();
        output.print_ascii(b"abc", CapturedStyle::default(), MAX_OUTPUT_BYTES);
        output.set_cursor_column(6, MAX_OUTPUT_BYTES);
        output.print('X', CapturedStyle::default(), MAX_OUTPUT_BYTES);
        assert_eq!(output.as_str(), "abc   X");
    }

    /// A wide CJK char (display width 2) whose second column is the target:
    /// the cursor lands at the char start, then a space is emitted so the
    /// wide char is pushed right and overwritten by the next print.
    #[test]
    fn set_cursor_column_into_wide_char_emits_space_and_overwrites() {
        let mut output = OutputCapture::default();
        output.print('你', CapturedStyle::default(), MAX_OUTPUT_BYTES);
        output.set_cursor_column(1, MAX_OUTPUT_BYTES);
        output.print('X', CapturedStyle::default(), MAX_OUTPUT_BYTES);
        assert_eq!(output.as_str(), " X");
    }

    /// `backspace` only rewinds within the current line; it stops at the
    /// line start rather than crossing into the previous line.
    #[test]
    fn backspace_stops_at_line_start() {
        let mut output = OutputCapture::default();
        output.print_ascii(b"abc", CapturedStyle::default(), MAX_OUTPUT_BYTES);
        output.newline(MAX_OUTPUT_BYTES);
        output.print_ascii(b"def", CapturedStyle::default(), MAX_OUTPUT_BYTES);
        // Three backspaces reach line start (column 0 of "def"); a fourth
        // is a no-op.
        for _ in 0..4 {
            output.backspace();
        }
        output.print('X', CapturedStyle::default(), MAX_OUTPUT_BYTES);
        assert_eq!(output.as_str(), "abc\nXef");
    }

    /// `backspace` retreats across all bytes of a multibyte char, not just
    /// one byte, so a subsequent print overwrites the whole char.
    #[test]
    fn backspace_across_multibyte_char() {
        let mut output = OutputCapture::default();
        output.print('你', CapturedStyle::default(), MAX_OUTPUT_BYTES);
        output.print('好', CapturedStyle::default(), MAX_OUTPUT_BYTES);
        output.backspace();
        output.print('X', CapturedStyle::default(), MAX_OUTPUT_BYTES);
        assert_eq!(output.as_str(), "你X");
    }

    /// `move_cursor_columns` with a negative delta larger than the current
    /// column saturates to column 0 rather than underflowing.
    #[test]
    fn move_cursor_columns_negative_saturates_to_zero() {
        let mut output = OutputCapture::default();
        output.print_ascii(b"abc", CapturedStyle::default(), MAX_OUTPUT_BYTES);
        output.move_cursor_columns(-100, MAX_OUTPUT_BYTES);
        output.print('X', CapturedStyle::default(), MAX_OUTPUT_BYTES);
        assert_eq!(output.as_str(), "Xbc");
    }
}
