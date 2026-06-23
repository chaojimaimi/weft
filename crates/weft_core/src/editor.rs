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

    pub fn delete_forward(&mut self) {
        let (line, col) = self.cursor;
        let line_len = self.lines[line].chars().count();
        if col < line_len {
            let cur = Self::byte_idx(&self.lines[line], col);
            let next = Self::byte_idx(&self.lines[line], col + 1);
            self.lines[line].replace_range(cur..next, "");
        } else if line + 1 < self.lines.len() {
            // Merge next line into current (forward delete at line end).
            let next = self.lines.remove(line + 1);
            self.lines[line].push_str(&next);
        }
    }

    pub fn move_line_home(&mut self) {
        self.cursor.1 = 0;
    }

    pub fn move_line_end(&mut self) {
        let line = self.cursor.0;
        self.cursor.1 = self.lines[line].chars().count();
    }

    /// Delete the word (and trailing whitespace) to the left of the cursor.
    pub fn delete_word_back(&mut self) {
        let (line, col) = self.cursor;
        if col == 0 {
            self.delete_backspace();
            return;
        }
        let chars: Vec<char> = self.lines[line].chars().collect();
        let mut new_col = col;
        // Eat trailing whitespace.
        while new_col > 0 && chars[new_col - 1].is_whitespace() {
            new_col -= 1;
        }
        // Eat one word.
        while new_col > 0 && !chars[new_col - 1].is_whitespace() {
            new_col -= 1;
        }
        let prev = Self::byte_idx(&self.lines[line], new_col);
        let cur = Self::byte_idx(&self.lines[line], col);
        self.lines[line].replace_range(prev..cur, "");
        self.cursor.1 = new_col;
    }

    pub fn clear_line(&mut self) {
        let line = self.cursor.0;
        self.lines[line].clear();
        self.cursor.1 = 0;
    }

    pub fn delete_to_end(&mut self) {
        let (line, col) = self.cursor;
        let byte = Self::byte_idx(&self.lines[line], col);
        self.lines[line].truncate(byte);
    }

    /// Split the current line at the cursor (Shift+Enter).
    pub fn split_newline(&mut self) {
        let (line, col) = self.cursor;
        let byte = Self::byte_idx(&self.lines[line], col);
        let tail: String = self.lines[line].drain(byte..).collect();
        self.lines.insert(line + 1, tail);
        self.cursor = (line + 1, 0);
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

    #[test]
    fn delete_forward_removes_char_at_cursor() {
        let mut b = EditorBuffer::new();
        b.lines = vec!["ab".to_string()];
        b.cursor = (0, 0);
        b.delete_forward();
        assert_eq!(b.text(), "b");
        assert_eq!(b.cursor, (0, 0));
    }

    #[test]
    fn delete_forward_at_line_end_merges_next_line() {
        let mut b = EditorBuffer::new();
        b.lines = vec!["ab".to_string(), "cd".to_string()];
        b.cursor = (0, 2);
        b.delete_forward();
        assert_eq!(b.text(), "abcd");
        assert_eq!(b.cursor, (0, 2));
    }

    #[test]
    fn move_home_and_end_within_line() {
        let mut b = EditorBuffer::new();
        b.lines = vec!["abc".to_string()];
        b.cursor = (0, 1);
        b.move_line_end();
        assert_eq!(b.cursor, (0, 3));
        b.move_line_home();
        assert_eq!(b.cursor, (0, 0));
    }

    #[test]
    fn delete_word_back_eats_trailing_word() {
        let mut b = EditorBuffer::new();
        b.lines = vec!["foo bar".to_string()];
        b.cursor = (0, 7);
        b.delete_word_back();
        assert_eq!(b.text(), "foo ");
        assert_eq!(b.cursor, (0, 4));
    }

    #[test]
    fn delete_word_back_eats_preceding_whitespace() {
        let mut b = EditorBuffer::new();
        b.lines = vec!["foo  ".to_string()];
        b.cursor = (0, 5);
        b.delete_word_back();
        // Eats trailing spaces then the word "foo".
        assert_eq!(b.text(), "");
    }

    #[test]
    fn clear_line_empties_current_line() {
        let mut b = EditorBuffer::new();
        b.lines = vec!["abc".to_string()];
        b.cursor = (0, 2);
        b.clear_line();
        assert_eq!(b.text(), "");
        assert_eq!(b.cursor, (0, 0));
    }

    #[test]
    fn delete_to_end_truncates_after_cursor() {
        let mut b = EditorBuffer::new();
        b.lines = vec!["abcdef".to_string()];
        b.cursor = (0, 2);
        b.delete_to_end();
        assert_eq!(b.text(), "ab");
        assert_eq!(b.cursor, (0, 2));
    }

    #[test]
    fn split_newline_breaks_line_at_cursor() {
        let mut b = EditorBuffer::new();
        b.lines = vec!["abcd".to_string()];
        b.cursor = (0, 2);
        b.split_newline();
        assert_eq!(b.text(), "ab\ncd");
        assert_eq!(b.cursor, (1, 0));
        assert_eq!(b.line_count(), 2);
    }
}
