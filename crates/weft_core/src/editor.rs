//! Multi-line editor buffer for the owned input box (v0.5 "Shuttle", phase 2).
//!
//! Pure logic: text + cursor + editing operations. No I/O, no rendering.
//! The `Terminal` owns an `Editor` (which wraps this buffer) and the app
//! layer maps winit key events to these methods.

/// A multi-line text buffer with a cursor. `cursor.0` = line index,
/// `cursor.1` = char column within that line (0..=line_char_count).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EditorBuffer {
    pub lines: Vec<String>,
    pub cursor: (usize, usize),
}

impl EditorBuffer {
    pub fn new() -> Self {
        Self {
            lines: vec![String::new()],
            cursor: (0, 0),
        }
    }

    pub fn line_count(&self) -> usize {
        self.lines.len()
    }

    /// char index → byte index within a line (clamps to end).
    fn byte_idx(line: &str, char_idx: usize) -> usize {
        line.char_indices()
            .nth(char_idx)
            .map(|(b, _)| b)
            .unwrap_or(line.len())
    }

    pub fn insert_char(&mut self, c: char) {
        let (line, col) = self.cursor;
        let byte = Self::byte_idx(&self.lines[line], col);
        self.lines[line].insert(byte, c);
        self.cursor.1 += 1;
    }

    pub fn delete_backspace(&mut self) {
        let (line, col) = self.cursor;
        if col > 0 {
            let prev = Self::byte_idx(&self.lines[line], col - 1);
            let cur = Self::byte_idx(&self.lines[line], col);
            self.lines[line].replace_range(prev..cur, "");
            self.cursor.1 -= 1;
        } else if line > 0 {
            // Merge current line into the previous one (multi-line backspace).
            let cur = self.lines.remove(line);
            let prev_line = line - 1;
            let prev_len = self.lines[prev_line].chars().count();
            self.lines[prev_line].push_str(&cur);
            self.cursor = (prev_line, prev_len);
        }
    }

    pub fn move_left(&mut self) {
        let (line, col) = self.cursor;
        if col > 0 {
            self.cursor.1 -= 1;
        } else if line > 0 {
            let prev_len = self.lines[line - 1].chars().count();
            self.cursor = (line - 1, prev_len);
        }
    }

    pub fn move_right(&mut self) {
        let (line, col) = self.cursor;
        let line_len = self.lines[line].chars().count();
        if col < line_len {
            self.cursor.1 += 1;
        } else if line + 1 < self.lines.len() {
            self.cursor = (line + 1, 0);
        }
    }

    /// Whole-buffer text, lines joined by `\n` (no trailing newline).
    pub fn text(&self) -> String {
        self.lines.join("\n")
    }
}

impl Default for EditorBuffer {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_is_one_empty_line_cursor_origin() {
        let b = EditorBuffer::new();
        assert_eq!(b.lines, vec!["".to_string()]);
        assert_eq!(b.cursor, (0, 0));
        assert_eq!(b.text(), "");
    }

    #[test]
    fn insert_char_appends_and_advances_cursor() {
        let mut b = EditorBuffer::new();
        b.insert_char('a');
        b.insert_char('b');
        assert_eq!(b.text(), "ab");
        assert_eq!(b.cursor, (0, 2));
    }

    #[test]
    fn insert_char_in_middle() {
        let mut b = EditorBuffer::new();
        b.insert_char('a');
        b.insert_char('c');
        b.cursor.1 = 1; // between a and c
        b.insert_char('b');
        assert_eq!(b.text(), "abc");
        assert_eq!(b.cursor, (0, 2));
    }

    #[test]
    fn backspace_deletes_char_behind_cursor() {
        let mut b = EditorBuffer::new();
        for c in "ab".chars() {
            b.insert_char(c);
        }
        b.delete_backspace();
        assert_eq!(b.text(), "a");
        assert_eq!(b.cursor, (0, 1));
    }

    #[test]
    fn backspace_at_line_start_merges_lines() {
        let mut b = EditorBuffer::new();
        b.lines = vec!["ab".to_string(), "cd".to_string()];
        b.cursor = (1, 0);
        b.delete_backspace();
        assert_eq!(b.text(), "abcd");
        assert_eq!(b.cursor, (0, 2));
        assert_eq!(b.line_count(), 1);
    }

    #[test]
    fn backspace_at_origin_is_noop() {
        let mut b = EditorBuffer::new();
        b.delete_backspace();
        assert_eq!(b.cursor, (0, 0));
        assert_eq!(b.text(), "");
    }

    #[test]
    fn move_left_within_line() {
        let mut b = EditorBuffer::new();
        for c in "ab".chars() {
            b.insert_char(c);
        }
        b.move_left();
        assert_eq!(b.cursor, (0, 1));
        b.move_left();
        assert_eq!(b.cursor, (0, 0));
    }

    #[test]
    fn move_left_at_line_start_goes_to_prev_line_end() {
        let mut b = EditorBuffer::new();
        b.lines = vec!["ab".to_string(), "cd".to_string()];
        b.cursor = (1, 0);
        b.move_left();
        assert_eq!(b.cursor, (0, 2));
    }

    #[test]
    fn move_right_within_and_across_lines() {
        let mut b = EditorBuffer::new();
        b.lines = vec!["ab".to_string(), "cd".to_string()];
        b.cursor = (0, 2); // end of line 0
        b.move_right(); // → line 1, col 0
        assert_eq!(b.cursor, (1, 0));
        b.move_right();
        b.move_right(); // → end of line 1
        b.move_right(); // clamps, stays
        assert_eq!(b.cursor, (1, 2));
    }
}
