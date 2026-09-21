use std::sync::atomic::Ordering;

use super::{CapturedStyle, OutputCapture};

impl OutputCapture {
    pub(crate) fn carriage_return(&mut self) {
        self.cursor = self.line_start();
        self.char_cursor = self.line_start_char;
        // M6-a watermark: the line start of the CURRENT line. While the
        // cursor is on the tail line this equals the sync boundary exactly
        // (`>=` in the guard, so a CRLF spinner never false-trips); after a
        // CSI A up-move it is an early row start, which correctly pulls the
        // watermark below the boundary.
        self.min_write_offset
            .fetch_min(self.cursor, Ordering::Relaxed);
    }

    pub(crate) fn set_cursor_column(&mut self, column: usize, max_bytes: usize) {
        // M6-a watermark: the movement itself is NOT recorded — it is
        // line-confined (cursor lands within `[line_start, line_end]`, and
        // the current line's start IS the sync boundary) and changes no
        // bytes. The column padding below emits through `print`, which
        // records its own offsets.
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
        // M6-a watermark: NOT recorded — same line-confined argument as
        // `set_cursor_column` (which this delegates to; its padding path
        // records through `print`).
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
        // M6-a watermark: the POST-move cursor — the next write overwrites
        // from here, and the move never crosses the current line's start
        // (== the sync boundary while on the tail line).
        self.min_write_offset
            .fetch_min(self.cursor, Ordering::Relaxed);
    }

    /// Move the capture cursor by `delta` rows (negative = up, positive =
    /// down). The cursor lands at the start of the target row.
    ///
    /// This is a simplified version of CSI A/B (cursor up/down) that moves
    /// to the row start rather than preserving the column. Multi-line
    /// progress bars like `ollama pull` and `brew upgrade` always follow
    /// `\033[<N>A` with `\r` before repainting, so landing at column zero
    /// matches the observed behaviour.
    ///
    /// Saturates at the first/last row boundary.
    pub(crate) fn move_cursor_rows(&mut self, delta: isize) {
        if delta == 0 {
            return;
        }
        let target = if delta < 0 {
            let up = (-delta) as usize;
            let mut current = self.line_start();
            for _ in 0..up {
                if current == 0 {
                    break;
                }
                // `current - 1` is the trailing '\n' of the previous row
                // (or a char inside it). Scan backwards for the '\n' that
                // starts the row above.
                current = self.text[..current - 1].rfind('\n').map_or(0, |i| i + 1);
            }
            current
        } else {
            let mut current = self.cursor;
            for _ in 0..delta {
                if current >= self.text.len() {
                    break;
                }
                match self.text[current..].find('\n') {
                    Some(offset) => current += offset + 1,
                    None => break,
                }
            }
            current
        };
        self.cursor = target;
        self.char_cursor = self.text[..target].chars().count() as u32;
        self.line_start_char = self.char_cursor;
        // M6-a watermark: the TARGET cursor. A negative move (CSI A — the
        // `ollama pull` / `brew upgrade` progress-bar repaint) lands on an
        // early row, pulling the watermark below the sync boundary so the
        // live layout cache's append fast path falls back to a full rebuild.
        self.min_write_offset.fetch_min(target, Ordering::Relaxed);
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

    /// `move_cursor_rows(-n)` moves the cursor up n rows so a subsequent
    /// CR + print overwrites that row in place instead of appending.
    #[test]
    fn move_cursor_rows_up_overwrites_target_row() {
        let mut output = OutputCapture::default();
        output.print_ascii(b"line1", CapturedStyle::default(), MAX_OUTPUT_BYTES);
        output.newline(MAX_OUTPUT_BYTES);
        output.print_ascii(b"line2", CapturedStyle::default(), MAX_OUTPUT_BYTES);
        output.newline(MAX_OUTPUT_BYTES);
        output.print_ascii(b"line3", CapturedStyle::default(), MAX_OUTPUT_BYTES);
        // cursor at end of "line3". Move up 2 rows → "line1" row start.
        output.move_cursor_rows(-2);
        output.carriage_return();
        output.print_ascii(b"LINE1", CapturedStyle::default(), MAX_OUTPUT_BYTES);
        assert_eq!(output.as_str(), "LINE1\nline2\nline3");
    }

    /// Moving up and then down lands back on the original row, so the
    /// print still overwrites in place.
    #[test]
    fn move_cursor_rows_down_returns_to_original_row() {
        let mut output = OutputCapture::default();
        output.print_ascii(b"line1", CapturedStyle::default(), MAX_OUTPUT_BYTES);
        output.newline(MAX_OUTPUT_BYTES);
        output.print_ascii(b"line2", CapturedStyle::default(), MAX_OUTPUT_BYTES);
        output.move_cursor_rows(-1);
        output.move_cursor_rows(1);
        output.carriage_return();
        output.print_ascii(b"LINE2", CapturedStyle::default(), MAX_OUTPUT_BYTES);
        assert_eq!(output.as_str(), "line1\nLINE2");
    }

    /// Moving up past the first row saturates at row 0 instead of
    /// underflowing.
    #[test]
    fn move_cursor_rows_up_saturates_at_first_row() {
        let mut output = OutputCapture::default();
        output.print_ascii(b"line1", CapturedStyle::default(), MAX_OUTPUT_BYTES);
        output.newline(MAX_OUTPUT_BYTES);
        output.print_ascii(b"line2", CapturedStyle::default(), MAX_OUTPUT_BYTES);
        output.move_cursor_rows(-100);
        output.carriage_return();
        output.print_ascii(b"LINE1", CapturedStyle::default(), MAX_OUTPUT_BYTES);
        assert_eq!(output.as_str(), "LINE1\nline2");
    }

    /// Moving down past the last row saturates at the text end so the
    /// next print appends instead of overwriting.
    #[test]
    fn move_cursor_rows_down_saturates_at_last_row() {
        let mut output = OutputCapture::default();
        output.print_ascii(b"line1", CapturedStyle::default(), MAX_OUTPUT_BYTES);
        output.newline(MAX_OUTPUT_BYTES);
        output.print_ascii(b"line2", CapturedStyle::default(), MAX_OUTPUT_BYTES);
        output.move_cursor_rows(100);
        output.print_ascii(b" appended", CapturedStyle::default(), MAX_OUTPUT_BYTES);
        assert_eq!(output.as_str(), "line1\nline2 appended");
    }

    /// Multi-line progress bar scenario: paint 3 rows (each followed by
    /// `\n`), move up 3 rows, repaint all 3 rows, and verify the row count
    /// stays at 3 (no growth). This is the `ollama pull` / `brew upgrade`
    /// pattern: `\033[3A` + `\r` + new content + `\033[K` per row.
    #[test]
    fn move_cursor_rows_multiline_progress_repaint() {
        let mut output = OutputCapture::default();
        // Initial paint: 3 rows, each followed by \n.
        output.print_ascii(b"a:   0%", CapturedStyle::default(), MAX_OUTPUT_BYTES);
        output.newline(MAX_OUTPUT_BYTES);
        output.print_ascii(b"b:   0%", CapturedStyle::default(), MAX_OUTPUT_BYTES);
        output.newline(MAX_OUTPUT_BYTES);
        output.print_ascii(b"c:   0%", CapturedStyle::default(), MAX_OUTPUT_BYTES);
        output.newline(MAX_OUTPUT_BYTES);
        // 3 newlines leave cursor on an empty row 4. Move up 3 rows to
        // land on row 1 ("a:   0%").
        output.move_cursor_rows(-3);
        // Repaint row 1 (new content shorter → erase_line clears tail)
        output.carriage_return();
        output.print_ascii(b"a: 50%", CapturedStyle::default(), MAX_OUTPUT_BYTES);
        output.erase_line(0);
        output.newline(MAX_OUTPUT_BYTES);
        // Repaint row 2
        output.carriage_return();
        output.print_ascii(b"b: 30%", CapturedStyle::default(), MAX_OUTPUT_BYTES);
        output.erase_line(0);
        output.newline(MAX_OUTPUT_BYTES);
        // Repaint row 3
        output.carriage_return();
        output.print_ascii(b"c: 10%", CapturedStyle::default(), MAX_OUTPUT_BYTES);
        output.erase_line(0);

        assert_eq!(
            output.as_str(),
            "a: 50%\nb: 30%\nc: 10%\n",
            "progress repaint must not grow rows"
        );
    }

    /// `move_cursor_rows` on an empty buffer is a no-op (cursor stays at 0).
    #[test]
    fn move_cursor_rows_empty_buffer_is_noop() {
        let mut output = OutputCapture::default();
        output.move_cursor_rows(-1);
        output.move_cursor_rows(1);
        output.move_cursor_rows(-100);
        assert_eq!(output.as_str(), "");
    }

    /// `move_cursor_rows` when the cursor is in the middle of a row still
    /// navigates by full rows (up from the current line start, not the
    /// cursor column).
    #[test]
    fn move_cursor_rows_from_mid_row_uses_line_start() {
        let mut output = OutputCapture::default();
        output.print_ascii(b"line1", CapturedStyle::default(), MAX_OUTPUT_BYTES);
        output.newline(MAX_OUTPUT_BYTES);
        output.print_ascii(b"line2", CapturedStyle::default(), MAX_OUTPUT_BYTES);
        // cursor in the middle of "line2" (byte 7, after "li")
        output.cursor = 8;
        output.char_cursor = 9;
        // move up 1 row → "line1" row start (byte 0)
        output.move_cursor_rows(-1);
        output.carriage_return();
        output.print_ascii(b"LINE1", CapturedStyle::default(), MAX_OUTPUT_BYTES);
        assert_eq!(output.as_str(), "LINE1\nline2");
    }

    // ── M6-a (PLAN_M6 §A-1): rewrite watermark accounting ───────────────

    /// Backspace and row moves record their (post-move) cursor: inside the
    /// tail line that stays at/above the boundary; a negative row move (CSI
    /// A) lands on an early row and pulls the watermark below it.
    #[test]
    fn watermark_backspace_and_row_moves_follow_the_cursor() {
        let mut output = OutputCapture::default();
        output.print_ascii(b"one\ntwo", CapturedStyle::default(), MAX_OUTPUT_BYTES);
        output.take_min_write_offset();
        // Tail line "two" starts at byte 4; cursor 7.
        output.backspace();
        assert_eq!(
            output.min_write_offset_value(),
            6,
            "backspace records its post-move cursor"
        );
        output.move_cursor_rows(-1);
        assert_eq!(
            output.min_write_offset_value(),
            0,
            "negative row move must invalidate the append fast path"
        );
    }

    /// CHA/CUB horizontal moves are line-confined and touch no bytes — they
    /// must NOT record (the tail line's start equals the sync boundary, so
    /// a record could only be redundant; recording nothing keeps the
    /// watermark "pure append" for pure cursor traffic).
    #[test]
    fn watermark_horizontal_moves_never_record() {
        let mut output = OutputCapture::default();
        output.print_ascii(b"one\ntwo", CapturedStyle::default(), MAX_OUTPUT_BYTES);
        output.take_min_write_offset();
        output.set_cursor_column(1, MAX_OUTPUT_BYTES);
        output.move_cursor_columns(-2, MAX_OUTPUT_BYTES);
        assert_eq!(
            output.min_write_offset_value(),
            usize::MAX,
            "line-confined moves must not lower the watermark"
        );
        assert_eq!(output.as_str(), "one\ntwo");
    }
}
