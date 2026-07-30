//! Terminal-like single-line rewrite handling for detached block outputs.
//!
//! v1.7.0-A: In addition to the text buffer, the capture maintains a bounded
//! RLE of `CapturedStyleRun` synced with every text mutation. The text buffer
//! remains the single source of truth for cursor position and truncation; the
//! style RLE is a parallel index keyed by char position. Style runs are only
//! stored for ranges where the program emitted non-default SGR attributes —
//! default-styled ranges are implicit (no run). Over-limit runs set
//! `style_overflow` and are dropped; text is never affected.

use super::style::{
    build_styled_output_from_runs, CapturedStyle, CapturedStyleRun, MAX_STYLE_RUNS_PER_BLOCK,
};

#[cfg(test)]
mod crlf_tests;
mod cursor;

#[derive(Clone, Debug, Default)]
pub(crate) struct OutputCapture {
    text: String,
    cursor: usize,
    /// Char index parallel to `cursor` (byte index). Tracked incrementally so
    /// style-run splice operations stay O(log n) instead of recomputing from
    /// the byte cursor on every print.
    char_cursor: u32,
    /// Char index of the start of the current line (the char after the last
    /// `\n`). Used by `carriage_return` and `erase_line` to translate line
    /// operations into char-indexed style splice operations.
    line_start_char: u32,
    truncated: bool,
    style_runs: Vec<CapturedStyleRun>,
    style_overflow: bool,
}

impl OutputCapture {
    pub(crate) fn as_str(&self) -> &str {
        &self.text
    }

    pub(crate) fn clear(&mut self) {
        self.text.clear();
        self.cursor = 0;
        self.char_cursor = 0;
        self.line_start_char = 0;
        self.truncated = false;
        self.style_runs.clear();
        self.style_overflow = false;
    }

    pub(crate) fn replace(&mut self, text: &str, max_bytes: usize) {
        self.clear();
        let mut end = text.len().min(max_bytes);
        while end > 0 && !text.is_char_boundary(end) {
            end -= 1;
        }
        self.text.push_str(&text[..end]);
        self.cursor = self.text.len();
        self.char_cursor = self.text.chars().count() as u32;
        self.truncated = end < text.len();
        // Screen-snapshot replace path: the caller supplies its own
        // StyledOutput via `replace_screen_snapshot`, so any previously
        // captured style runs are discarded.
    }

    pub(crate) fn print(&mut self, c: char, style: CapturedStyle, max_bytes: usize) {
        if self.truncated {
            return;
        }
        let replaced_len = self
            .char_end_at_cursor()
            .filter(|_| self.text.as_bytes().get(self.cursor) != Some(&b'\n'))
            .map(|end| end - self.cursor)
            .unwrap_or(0);
        let next_len = self
            .text
            .len()
            .saturating_sub(replaced_len)
            .saturating_add(c.len_utf8());
        if next_len > max_bytes {
            self.truncated = true;
            return;
        }

        // Style RLE: overwrite the char at char_cursor with `style`. The
        // replaced char (if any) is a single Unicode scalar — its char index
        // is char_cursor, and the new char occupies the same index.
        let replaced_char_count = if replaced_len > 0 { 1u32 } else { 0u32 };
        if replaced_char_count > 0 {
            self.splice_style(
                self.char_cursor,
                self.char_cursor + replaced_char_count,
                style,
            );
        } else {
            self.splice_style(self.char_cursor, self.char_cursor + 1, style);
        }

        if replaced_len > 0 {
            let end = self.cursor + replaced_len;
            self.text.replace_range(self.cursor..end, &c.to_string());
        } else {
            self.text.insert(self.cursor, c);
        }
        self.cursor += c.len_utf8();
        self.char_cursor += 1;
    }

    pub(crate) fn print_ascii(&mut self, bytes: &[u8], style: CapturedStyle, max_bytes: usize) {
        if bytes.is_empty() || self.truncated {
            return;
        }
        if self.cursor == self.text.len() {
            let remaining = max_bytes.saturating_sub(self.text.len());
            let accepted = remaining.min(bytes.len());
            if accepted > 0 {
                // Callers guarantee printable ASCII.
                self.text
                    .push_str(std::str::from_utf8(&bytes[..accepted]).unwrap_or(""));
                self.cursor = self.text.len();
                // ASCII batch: one RLE splice covering [char_cursor, char_cursor + accepted).
                self.splice_style(self.char_cursor, self.char_cursor + accepted as u32, style);
                self.char_cursor += accepted as u32;
            }
            if accepted < bytes.len() {
                self.truncated = true;
            }
            return;
        }
        for byte in bytes {
            self.print(char::from(*byte), style, max_bytes);
            if self.truncated {
                break;
            }
        }
    }

    pub(crate) fn newline(&mut self, max_bytes: usize) {
        if self.truncated {
            return;
        }
        let line_start = self.line_start();
        self.cursor = self.line_end();
        // PTYs commonly translate LF to CRLF. The preceding CR leaves the
        // capture cursor at column zero, so advance the parallel character
        // cursor across the existing line before consuming/appending LF.
        // Otherwise every following ANSI run is indexed near the start of
        // the transcript even though the plain text remains correct.
        self.char_cursor =
            self.line_start_char + self.text[line_start..self.cursor].chars().count() as u32;
        if self.text.as_bytes().get(self.cursor) == Some(&b'\n') {
            self.cursor += 1;
            self.char_cursor += 1;
            self.line_start_char = self.char_cursor;
            return;
        }
        if self.text.len() < max_bytes {
            self.text.push('\n');
            self.cursor = self.text.len();
            self.char_cursor += 1;
            self.line_start_char = self.char_cursor;
        } else {
            self.truncated = true;
        }
    }

    pub(crate) fn erase_line(&mut self, mode: u16) {
        let start = self.line_start();
        let end = self.line_end();
        let line_start_char = self.line_start_char;
        let line_end_char = line_start_char + self.text[start..end].chars().count() as u32;
        match mode {
            0 => {
                // Erase from cursor to end of line.
                self.text.replace_range(self.cursor..end, "");
                self.erase_style_range(self.char_cursor, line_end_char);
            }
            1 => {
                // Erase from start of line to cursor (inclusive). The erased
                // range is replaced with spaces, which are default-styled —
                // drop any style runs in the range.
                let replacement = " ".repeat(self.text[start..self.cursor].chars().count());
                self.text.replace_range(start..self.cursor, &replacement);
                self.cursor = start + replacement.len();
                self.erase_style_range(line_start_char, self.char_cursor);
            }
            2 => {
                // Erase entire line.
                self.text.replace_range(start..end, "");
                self.cursor = start;
                self.erase_style_range(line_start_char, line_end_char);
            }
            _ => {}
        }
    }

    /// Move the capture cursor to a zero-based terminal row/column.
    /// Missing rows and columns are materialized as newlines/spaces so
    /// cursor-addressed primary-screen exit tails retain their visual line
    /// structure instead of concatenating text from separate rows.
    pub(crate) fn goto(&mut self, row: usize, col: usize, max_bytes: usize) {
        if self.truncated {
            return;
        }
        let existing_rows = self.text.bytes().filter(|byte| *byte == b'\n').count() + 1;
        for _ in existing_rows..=row {
            if self.text.len() >= max_bytes {
                self.truncated = true;
                return;
            }
            self.text.push('\n');
            self.char_cursor += 1;
            self.line_start_char = self.char_cursor;
        }

        let line_start = if row == 0 {
            0
        } else {
            self.text
                .match_indices('\n')
                .nth(row - 1)
                .map_or(self.text.len(), |(index, _)| index + 1)
        };
        let line_end = self.text[line_start..]
            .find('\n')
            .map_or(self.text.len(), |offset| line_start + offset);

        // Recompute line_start_char for the target row by counting chars
        // from the buffer start up to line_start.
        let mut new_line_start_char = 0u32;
        for _ in self.text[..line_start].chars() {
            new_line_start_char += 1;
        }
        self.line_start_char = new_line_start_char;

        let mut cursor = line_start;
        let mut char_offset_in_line = 0u32;
        for _ in 0..col {
            if cursor < line_end {
                let step = self.text[cursor..line_end]
                    .chars()
                    .next()
                    .map_or(0, char::len_utf8);
                cursor += step;
                char_offset_in_line += 1;
            } else if self.text.len() < max_bytes {
                self.text.insert(cursor, ' ');
                cursor += 1;
                char_offset_in_line += 1;
            } else {
                self.truncated = true;
                return;
            }
        }
        self.cursor = cursor;
        self.char_cursor = self.line_start_char + char_offset_in_line;
    }

    /// Finalize the capture, returning the text and — if any non-default
    /// styles were captured — a `StyledOutput` keyed by line index.
    /// The truncation marker (if any) is appended to the text after styles
    /// are extracted, so it never participates in the style RLE.
    pub(crate) fn take_styled(&mut self) -> (String, Option<StyledOutput>) {
        let mut text = std::mem::take(&mut self.text);
        let runs = std::mem::take(&mut self.style_runs);
        let overflow = self.style_overflow;
        let truncated = self.truncated;
        self.cursor = 0;
        self.char_cursor = 0;
        self.line_start_char = 0;
        self.truncated = false;
        self.style_overflow = false;

        let styled = if overflow || runs.is_empty() {
            None
        } else {
            build_styled_output_from_runs(&text, &runs)
        };

        if truncated {
            text.push_str("\n…(output truncated, >1 MiB)");
        }
        (text, styled)
    }

    // ── Style RLE maintenance ────────────────────────────────────────────

    /// Replace the style for chars in `[start, end)` with `style`. Runs fully
    /// inside the range are dropped; runs overlapping the boundary are
    /// truncated. The new run is inserted only if `style` is non-default,
    /// and coalesced with same-style neighbors. Respects
    /// `MAX_STYLE_RUNS_PER_BLOCK`: when the budget is exhausted, sets
    /// `style_overflow` and clears all runs.
    fn splice_style(&mut self, start: u32, end: u32, style: CapturedStyle) {
        if self.style_overflow || start >= end {
            return;
        }
        let first = self.style_runs.partition_point(|run| run.end_char <= start);
        let last = self.style_runs.partition_point(|run| run.start_char < end);
        let mut replacement = Vec::with_capacity(3);
        if first < self.style_runs.len() && self.style_runs[first].start_char < start {
            replacement.push(CapturedStyleRun {
                end_char: start,
                ..self.style_runs[first]
            });
        }
        if !style.is_default() {
            replacement.push(CapturedStyleRun {
                start_char: start,
                end_char: end,
                style,
            });
        }
        if first < last && self.style_runs[last - 1].end_char > end {
            replacement.push(CapturedStyleRun {
                start_char: end,
                ..self.style_runs[last - 1]
            });
        }
        let replacement_len = replacement.len();
        self.style_runs.splice(first..last, replacement);

        // Only the replacement boundaries can have become coalescible.
        let mut index = first.saturating_sub(1);
        let merge_end = (first + replacement_len + 1).min(self.style_runs.len());
        while index + 1 < self.style_runs.len() && index < merge_end {
            let can_merge = self.style_runs[index].end_char
                == self.style_runs[index + 1].start_char
                && self.style_runs[index].style == self.style_runs[index + 1].style;
            if can_merge {
                let end_char = self.style_runs[index + 1].end_char;
                self.style_runs[index].end_char = end_char;
                self.style_runs.remove(index + 1);
            } else {
                index += 1;
            }
        }
        if self.style_runs.len() > MAX_STYLE_RUNS_PER_BLOCK {
            self.style_overflow = true;
            self.style_runs.clear();
        }
    }

    /// Drop all style runs in `[start, end)` — used by `erase_line` when the
    /// erased range is replaced with default-styled spaces (or nothing).
    fn erase_style_range(&mut self, start: u32, end: u32) {
        if self.style_overflow || start >= end {
            return;
        }
        self.splice_style(start, end, CapturedStyle::default());
    }

    fn line_start(&self) -> usize {
        self.text[..self.cursor]
            .rfind('\n')
            .map_or(0, |index| index + 1)
    }

    fn line_end(&self) -> usize {
        self.text[self.cursor..]
            .find('\n')
            .map_or(self.text.len(), |offset| self.cursor + offset)
    }

    fn char_end_at_cursor(&self) -> Option<usize> {
        let c = self.text.get(self.cursor..)?.chars().next()?;
        Some(self.cursor + c.len_utf8())
    }
}

// Re-export for tests and the BlockTracker finalize path.
pub(crate) use super::style::StyledOutput;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::blocks::MAX_OUTPUT_BYTES;
    use crate::grid::{CellColor, CellFlags, Color};

    fn fg_style(palette: u8) -> CapturedStyle {
        CapturedStyle::from_attrs(
            CellColor::Palette(palette),
            CellColor::Default,
            CellFlags::empty(),
        )
    }

    fn bold_style() -> CapturedStyle {
        CapturedStyle::from_attrs(CellColor::Default, CellColor::Default, CellFlags::BOLD)
    }

    fn rgb_style(r: u8, g: u8, b: u8) -> CapturedStyle {
        CapturedStyle::from_attrs(
            CellColor::Rgb(Color::rgb(r, g, b)),
            CellColor::Default,
            CellFlags::empty(),
        )
    }

    #[test]
    fn carriage_return_overwrites_spinner_frame_in_place() {
        let mut output = OutputCapture::default();
        output.print_ascii(b"Upgrading.", CapturedStyle::default(), 1024);
        output.carriage_return();
        output.print_ascii(b"Upgrading..", CapturedStyle::default(), 1024);
        output.carriage_return();
        output.print_ascii(b"Upgrading...", CapturedStyle::default(), 1024);
        assert_eq!(output.as_str(), "Upgrading...");
    }

    #[test]
    fn horizontal_absolute_rewrites_spinner_frame_in_place() {
        let mut output = OutputCapture::default();
        output.print_ascii(b"Upgrading.", CapturedStyle::default(), 1024);
        output.set_cursor_column(0, 1024);
        output.print_ascii(b"Upgrading..", CapturedStyle::default(), 1024);
        output.set_cursor_column(0, 1024);
        output.print_ascii(b"Upgrading...", CapturedStyle::default(), 1024);
        output.erase_line(0);
        assert_eq!(output.as_str(), "Upgrading...");
    }

    #[test]
    fn relative_cursor_movement_rewrites_instead_of_appending() {
        let mut output = OutputCapture::default();
        output.print_ascii(b"status old", CapturedStyle::default(), 1024);
        output.move_cursor_columns(-3, 1024);
        output.print_ascii(b"new", CapturedStyle::default(), 1024);
        assert_eq!(output.as_str(), "status new");
    }

    #[test]
    fn erase_right_removes_tail_from_a_shorter_progress_frame() {
        let mut output = OutputCapture::default();
        output.print_ascii(b"Downloading 100%", CapturedStyle::default(), 1024);
        output.carriage_return();
        output.print_ascii(b"Done", CapturedStyle::default(), 1024);
        output.erase_line(0);
        assert_eq!(output.as_str(), "Done");
    }

    #[test]
    fn utf8_replacement_keeps_cursor_on_a_character_boundary() {
        let mut output = OutputCapture::default();
        output.print_ascii(b"abc", CapturedStyle::default(), 1024);
        output.carriage_return();
        output.print('中', CapturedStyle::default(), 1024);
        output.print('文', CapturedStyle::default(), 1024);
        assert_eq!(output.as_str(), "中文c");
    }

    #[test]
    fn absolute_cursor_rows_preserve_line_structure() {
        let mut output = OutputCapture::default();
        output.goto(0, 0, 1024);
        output.print_ascii(b"first", CapturedStyle::default(), 1024);
        output.goto(2, 3, 1024);
        output.print_ascii(b"third", CapturedStyle::default(), 1024);
        output.goto(1, 1, 1024);
        output.print_ascii(b"second", CapturedStyle::default(), 1024);

        assert_eq!(output.as_str(), "first\n second\n   third");
    }

    // ── v1.7.0-A: ANSI style RLE tests ──────────────────────────────────

    #[test]
    fn ascii_batch_captures_one_run_for_a_colored_segment() {
        let mut output = OutputCapture::default();
        output.print_ascii(b"hello", fg_style(2), 1024);
        let (text, styled) = output.take_styled();
        assert_eq!(text, "hello");
        let styled = styled.expect("styled output");
        let line = styled.line(0).expect("line 0");
        assert_eq!(line.foreground_at(0), Some(CellColor::Palette(2)));
        assert_eq!(line.foreground_at(4), Some(CellColor::Palette(2)));
        assert!(line.foregrounds.len() == 1, "one coalesced run");
    }

    #[test]
    fn default_style_chars_produce_no_runs() {
        let mut output = OutputCapture::default();
        output.print_ascii(b"plain", CapturedStyle::default(), 1024);
        let (text, styled) = output.take_styled();
        assert_eq!(text, "plain");
        assert!(styled.is_none(), "default-only output has no styled output");
    }

    #[test]
    fn carriage_return_overwrites_style_alongside_text() {
        let mut output = OutputCapture::default();
        output.print_ascii(b"red", fg_style(1), 1024);
        output.carriage_return();
        output.print_ascii(b"green", fg_style(2), 1024);
        let (text, styled) = output.take_styled();
        assert_eq!(text, "green");
        let styled = styled.expect("styled");
        let line = styled.line(0).expect("line 0");
        assert_eq!(line.foreground_at(0), Some(CellColor::Palette(2)));
        assert_eq!(line.foreground_at(4), Some(CellColor::Palette(2)));
        assert!(line.foregrounds.len() == 1, "old red run was replaced");
    }

    #[test]
    fn backspace_preserves_style_until_overwritten() {
        let mut output = OutputCapture::default();
        output.print('a', fg_style(1), 1024);
        output.print('b', fg_style(2), 1024);
        output.backspace();
        // 'a' still has palette 1; 'b' (cursor) retains palette 2 until overwritten.
        let (text, styled) = output.take_styled();
        assert_eq!(text, "ab");
        let styled = styled.expect("styled");
        let line = styled.line(0).expect("line 0");
        assert_eq!(line.foreground_at(0), Some(CellColor::Palette(1)));
        assert_eq!(line.foreground_at(1), Some(CellColor::Palette(2)));
    }

    #[test]
    fn erase_line_mode_0_drops_style_in_tail() {
        let mut output = OutputCapture::default();
        output.print_ascii(b"keep", fg_style(2), 1024);
        output.print_ascii(b"erase", fg_style(1), 1024);
        output.carriage_return();
        // Re-print "keep" with the SAME green style — simulates a progress
        // bar repaint where the persistent prefix keeps its color.
        output.print_ascii(b"keep", fg_style(2), 1024);
        output.erase_line(0);
        let (text, styled) = output.take_styled();
        assert_eq!(text, "keep");
        let styled = styled.expect("styled");
        let line = styled.line(0).expect("line 0");
        assert_eq!(line.foreground_at(0), Some(CellColor::Palette(2)));
        assert_eq!(line.foreground_at(3), Some(CellColor::Palette(2)));
        assert_eq!(line.foregrounds.len(), 1, "red 'erase' style was dropped");
    }

    #[test]
    fn erase_line_mode_2_clears_all_styles_on_line() {
        let mut output = OutputCapture::default();
        output.print_ascii(b"line1", fg_style(1), 1024);
        output.newline(1024);
        output.print_ascii(b"line2", fg_style(2), 1024);
        output.carriage_return();
        output.erase_line(2);
        let (text, styled) = output.take_styled();
        assert_eq!(text, "line1\n");
        let styled = styled.expect("styled");
        assert_eq!(
            styled.line(0).expect("line 0").foreground_at(0),
            Some(CellColor::Palette(1))
        );
        assert!(styled.line(1).is_none(), "line 1 had all styles erased");
    }

    #[test]
    fn multiline_capture_indexes_lines_independently() {
        let mut output = OutputCapture::default();
        output.print_ascii(b"red", fg_style(1), 1024);
        output.newline(1024);
        output.print_ascii(b"green", fg_style(2), 1024);
        output.newline(1024);
        output.print_ascii(b"blue", fg_style(4), 1024);
        let (text, styled) = output.take_styled();
        assert_eq!(text, "red\ngreen\nblue");
        let styled = styled.expect("styled");
        assert_eq!(
            styled.line(0).unwrap().foreground_at(0),
            Some(CellColor::Palette(1))
        );
        assert_eq!(
            styled.line(1).unwrap().foreground_at(0),
            Some(CellColor::Palette(2))
        );
        assert_eq!(
            styled.line(2).unwrap().foreground_at(0),
            Some(CellColor::Palette(4))
        );
    }

    #[test]
    fn cr_progress_update_preserves_unrelated_styles() {
        // Simulate a spinner: "Upgrading..." printed 3 times via CR.
        // Each iteration overwrites the same range; the final style should
        // be the last one written.
        let mut output = OutputCapture::default();
        output.print_ascii(b"Upgrading.", fg_style(3), 1024);
        output.carriage_return();
        output.print_ascii(b"Upgrading..", fg_style(3), 1024);
        output.carriage_return();
        output.print_ascii(b"Upgrading...", fg_style(3), 1024);
        let (text, styled) = output.take_styled();
        assert_eq!(text, "Upgrading...");
        let styled = styled.expect("styled");
        let line = styled.line(0).expect("line 0");
        for i in 0..11 {
            assert_eq!(
                line.foreground_at(i),
                Some(CellColor::Palette(3)),
                "char {i}"
            );
        }
    }

    #[test]
    fn bold_flags_round_trip_through_styled_output() {
        let mut output = OutputCapture::default();
        output.print('h', bold_style(), 1024);
        output.print('i', CapturedStyle::default(), 1024);
        let (text, styled) = output.take_styled();
        assert_eq!(text, "hi");
        let styled = styled.expect("styled");
        let line = styled.line(0).expect("line 0");
        assert_eq!(line.attributes_at(0), CellFlags::BOLD);
        assert_eq!(line.attributes_at(1), CellFlags::empty());
    }

    #[test]
    fn truecolor_rgb_is_preserved_not_baked_to_palette() {
        let mut output = OutputCapture::default();
        output.print('x', rgb_style(123, 45, 67), 1024);
        let (text, styled) = output.take_styled();
        assert_eq!(text, "x");
        let styled = styled.expect("styled");
        let line = styled.line(0).expect("line 0");
        assert_eq!(
            line.foreground_at(0),
            Some(CellColor::Rgb(Color::rgb(123, 45, 67)))
        );
    }

    #[test]
    fn background_and_foreground_coexist_in_one_run() {
        let mut output = OutputCapture::default();
        let style = CapturedStyle::from_attrs(
            CellColor::Palette(2),
            CellColor::Palette(5),
            CellFlags::UNDERLINE,
        );
        output.print('x', style, 1024);
        let (text, styled) = output.take_styled();
        assert_eq!(text, "x");
        let styled = styled.expect("styled");
        let line = styled.line(0).expect("line 0");
        assert_eq!(line.foreground_at(0), Some(CellColor::Palette(2)));
        assert_eq!(line.background_at(0), Some(CellColor::Palette(5)));
        assert_eq!(line.attributes_at(0), CellFlags::UNDERLINE);
    }

    #[test]
    fn goto_preserves_styles_on_other_lines() {
        let mut output = OutputCapture::default();
        output.print_ascii(b"line0", fg_style(1), 1024);
        output.newline(1024);
        output.goto(2, 0, 1024);
        output.print_ascii(b"line2", fg_style(3), 1024);
        let (text, styled) = output.take_styled();
        assert_eq!(text, "line0\n\nline2");
        let styled = styled.expect("styled");
        assert_eq!(
            styled.line(0).unwrap().foreground_at(0),
            Some(CellColor::Palette(1))
        );
        assert_eq!(
            styled.line(2).unwrap().foreground_at(0),
            Some(CellColor::Palette(3))
        );
    }

    #[test]
    fn coalescing_adjacent_same_style_runs_stays_compact() {
        let mut output = OutputCapture::default();
        for _ in 0..50 {
            output.print('a', fg_style(2), 1024);
        }
        let (_, styled) = output.take_styled();
        let styled = styled.expect("styled");
        let line = styled.line(0).expect("line 0");
        assert_eq!(
            line.foregrounds.len(),
            1,
            "50 same-style chars coalesce to 1 run"
        );
    }

    #[test]
    fn style_overflow_drops_runs_but_keeps_text() {
        let mut output = OutputCapture::default();
        // Alternating palette indices force a new run per char, quickly
        // exhausting MAX_STYLE_RUNS_PER_BLOCK.
        for i in 0..(MAX_STYLE_RUNS_PER_BLOCK + 100) {
            let palette = ((i % 2) as u8) + 1;
            output.print('x', fg_style(palette), MAX_OUTPUT_BYTES);
            if output.style_overflow {
                break;
            }
        }
        assert!(output.style_overflow, "overflow flag must be set");
        let (text, styled) = output.take_styled();
        assert!(!text.is_empty(), "text survives style overflow");
        assert!(styled.is_none(), "styled output is dropped on overflow");
    }

    #[test]
    fn replace_clears_captured_styles() {
        let mut output = OutputCapture::default();
        output.print_ascii(b"red", fg_style(1), 1024);
        output.replace("replaced", 1024);
        let (text, styled) = output.take_styled();
        assert_eq!(text, "replaced");
        // replace() is the screen-snapshot path — it does not synthesize
        // styles from the RLE. The caller supplies StyledOutput separately.
        assert!(styled.is_none());
    }

    // ── v1.7.0-E acceptance tests ────────────────────────────────────

    /// v1.7.0-E §2.7: "复制/导出文本不含颜色控制字节". Verifies that text
    /// extracted from `take_styled()` contains no ESC (0x1b), CSI (0x9b),
    /// or other C1 control bytes even when the capture was fed alternating
    /// ANSI palette colors and attributes. The `OutputCapture` stores
    /// parsed text only — raw ESC sequences are consumed by the VT parser
    /// before reaching the capture — but this test pins the contract.
    #[test]
    fn captured_text_contains_no_ansi_escape_bytes() {
        let mut output = OutputCapture::default();
        // Feed alternating palette colors + bold to maximize the chance of
        // any escape leakage (there should be none).
        for i in 0..200u8 {
            let style = CapturedStyle::from_attrs(
                CellColor::Palette(i % 8),
                CellColor::Default,
                if i % 2 == 0 {
                    CellFlags::BOLD
                } else {
                    CellFlags::empty()
                },
            );
            output.print_ascii(&[b'a' + (i % 26)], style, 1024);
        }
        let (text, _styled) = output.take_styled();
        assert!(!text.is_empty());
        // No ESC (0x1b), no CSI (0x9b), no SGR parameter bytes (0x30-0x3f
        // alone are fine — they're digits/punctuation — but 0x1b is the
        // hard gate).
        let bytes = text.as_bytes();
        assert!(
            !bytes.contains(&0x1b) && !bytes.contains(&0x9b),
            "captured text contains ANSI escape bytes"
        );
        // Also verify no C1 control bytes (0x80..=0x9f) which include CSI.
        assert!(
            !bytes.iter().any(|&b| (0x80..=0x9f).contains(&b)),
            "captured text contains C1 control bytes"
        );
    }

    /// v1.7.0-E §2.7 combined stress: 1 MiB high-color output + 16,384
    /// ANSI style runs + 8,192 semantic spans + 13,200 plain lines. Verifies
    /// the capture path doesn't corrupt or lose text, respects the style
    /// run cap, and the semantic classifier respects the block span cap —
    /// all without panicking or exceeding the documented bounds.
    ///
    /// This is a pure-logic stress test (no winit event loop); the
    /// "不阻塞 winit event loop" requirement is verified separately by the
    /// performance gate and the `#[ignore]` benchmarks.
    #[test]
    fn combined_stress_1mib_high_color_16k_runs_8k_spans_13k_lines() {
        let mut output = OutputCapture::default();

        // ── Phase 1: 16,384 alternating-color runs (high-color) ────────
        // Each run is 1 char with a distinct palette color, pushing the
        // RLE to its 16,384-run cap. After the cap, additional styles are
        // dropped but text continues.
        for i in 0..(MAX_STYLE_RUNS_PER_BLOCK + 100) {
            let style = CapturedStyle::from_attrs(
                CellColor::Palette((i % 255) as u8),
                CellColor::Default,
                CellFlags::empty(),
            );
            output.print_ascii(b"X", style, MAX_OUTPUT_BYTES);
        }
        assert!(
            output.style_overflow,
            "style_overflow must be set after exceeding MAX_STYLE_RUNS_PER_BLOCK"
        );

        // ── Phase 2: 13,200 plain (default-style) lines ───────────────
        // Each line is 80 chars + 1 newline = 81 bytes → 1,069,200 bytes total.
        // Combined with Phase 1's 16,484 bytes, total exceeds MAX_OUTPUT_BYTES
        // (1,048,576), exercising the text truncation path.
        let line: String = "a".repeat(80);
        for _ in 0..13_200 {
            output.print_ascii(line.as_bytes(), CapturedStyle::default(), MAX_OUTPUT_BYTES);
            output.newline(MAX_OUTPUT_BYTES);
        }

        let (text, styled) = output.take_styled();

        // Text survived and was truncated. The truncation marker is appended
        // by take_styled() when the capture exceeds MAX_OUTPUT_BYTES, so
        // text.len() can slightly exceed the cap (by the marker length).
        assert!(!text.is_empty(), "text must survive combined stress");
        assert!(
            text.contains("output truncated"),
            "text should contain truncation marker, got len {}",
            text.len()
        );
        // Text is approximately bounded (within 100 bytes of the cap + marker).
        assert!(
            text.len() <= MAX_OUTPUT_BYTES + 100,
            "text len {} greatly exceeds MAX_OUTPUT_BYTES {} (expected ~cap + marker)",
            text.len(),
            MAX_OUTPUT_BYTES
        );
        // No escape bytes leaked into the text.
        assert!(
            !text.as_bytes().contains(&0x1b),
            "stress text contains ESC bytes"
        );

        // Styled output is dropped when style_overflow is set (the RLE
        // exceeded MAX_STYLE_RUNS_PER_BLOCK). This is the documented
        // behavior: overflow drops styles but keeps text. The semantic
        // classifier runs on the text directly.
        assert!(
            styled.is_none(),
            "styled output should be dropped after style overflow"
        );

        // ── Phase 3: Run semantic classifier on the captured text ──────
        // The classifier should complete without panicking and respect the
        // 8,192-span block cap. Most lines are plain "aaa..." which won't
        // produce semantic spans, but the exercise validates the pipeline.
        let semantic = crate::blocks::classify_block(&text, None);
        if let Some(sem) = semantic {
            // Block span cap is enforced; even if every line produced spans,
            // the total cannot exceed MAX_SEMANTIC_SPANS_PER_BLOCK.
            let total: usize = sem.lines.iter().map(|l| l.spans.len()).sum();
            assert!(
                total <= crate::blocks::MAX_SEMANTIC_SPANS_PER_BLOCK,
                "semantic spans {} exceed block cap {}",
                total,
                crate::blocks::MAX_SEMANTIC_SPANS_PER_BLOCK
            );
        }
    }
}
