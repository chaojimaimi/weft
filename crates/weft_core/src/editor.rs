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
    /// v0.9: anchor for mouse-drag selection (click-drag in the prompt box).
    /// `None` = no active selection. The selection spans between `cursor`
    /// and `selection_anchor` (order-independent). Cleared by any cursor
    /// movement key or text edit, and by `clear_selection`.
    pub selection_anchor: Option<(usize, usize)>,
}

impl EditorBuffer {
    pub fn new() -> Self {
        Self {
            lines: vec![String::new()],
            cursor: (0, 0),
            selection_anchor: None,
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
        // Reject C0/C1 control characters (ESC, NUL, …). They can't be part of
        // a legitimately typed command and would enable command-spoofing /
        // terminal-sequence injection once forwarded to the PTY. Newlines arrive
        // via `split_newline`, not here.
        if c.is_control() {
            return;
        }
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

    /// Replace the whole buffer with `text`, cursor at the end. Cheaper than
    /// per-char `insert_char` (which is O(n²) for long strings) and dedupes the
    /// history-nav / search-accept / cancel restore paths.
    pub fn set_text(&mut self, text: &str) {
        self.lines = text.split('\n').map(String::from).collect();
        let last_idx = self.lines.len().saturating_sub(1);
        let last_len = self.lines.last().map(|s| s.chars().count()).unwrap_or(0);
        self.cursor = (last_idx, last_len);
        self.selection_anchor = None;
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
        self.selection_anchor = None;
    }

    // ── v0.9: mouse selection ──────────────────────────────────────────

    /// Begin a mouse selection at `pos` (line, col). Sets the anchor and
    /// moves the cursor to `pos` so a drag extends from anchor→cursor.
    pub fn start_selection(&mut self, pos: (usize, usize)) {
        self.selection_anchor = Some(pos);
        self.cursor = pos;
    }

    /// Extend the active selection by moving the cursor to `pos`. The anchor
    /// stays put. No-op when there's no active selection.
    pub fn extend_selection(&mut self, pos: (usize, usize)) {
        if self.selection_anchor.is_some() {
            self.cursor = pos;
        }
    }

    /// Select the entire buffer (used by select-all / Cmd+A).
    pub fn select_all(&mut self) {
        let last_idx = self.lines.len().saturating_sub(1);
        let last_len = self.lines.last().map(|s| s.chars().count()).unwrap_or(0);
        self.selection_anchor = Some((0, 0));
        self.cursor = (last_idx, last_len);
    }

    pub fn clear_selection(&mut self) {
        self.selection_anchor = None;
    }

    pub fn has_selection(&self) -> bool {
        self.selection_anchor.is_some()
    }

    /// The (start, end) range of the active selection, in document order
    /// (start <= end). Returns None when there's no selection or the
    /// selection is empty (anchor == cursor).
    pub fn selection_range(&self) -> Option<((usize, usize), (usize, usize))> {
        let anchor = self.selection_anchor?;
        let (a, b) = if Self::pos_le(anchor, self.cursor) {
            (anchor, self.cursor)
        } else {
            (self.cursor, anchor)
        };
        if a == b {
            return None;
        }
        Some((a, b))
    }

    /// Lexicographic comparison: true when `p` is at or before `q`.
    fn pos_le(p: (usize, usize), q: (usize, usize)) -> bool {
        p.0 < q.0 || (p.0 == q.0 && p.1 <= q.1)
    }

    /// Selected text, lines joined by `\n`. None when there's no selection.
    pub fn selected_text(&self) -> Option<String> {
        let ((sl, sc), (el, ec)) = self.selection_range()?;
        if sl == el {
            return Some(
                self.lines[sl]
                    .chars()
                    .skip(sc)
                    .take(ec.saturating_sub(sc))
                    .collect(),
            );
        }
        let mut out = String::new();
        // First line: from sc to end.
        out.extend(self.lines[sl].chars().skip(sc));
        out.push('\n');
        // Middle lines: whole.
        for line in &self.lines[sl + 1..el] {
            out.push_str(line);
            out.push('\n');
        }
        // Last line: 0..ec.
        out.extend(self.lines[el].chars().take(ec));
        Some(out)
    }
}

impl Default for EditorBuffer {
    fn default() -> Self {
        Self::new()
    }
}

/// Owned input box state: the buffer plus command history and Ctrl+R search.
pub struct Editor {
    pub buffer: EditorBuffer,
    history: Vec<String>,
    /// Index into `history` while navigating (newest-first), or `None` when
    /// at the "live" prompt position.
    history_idx: Option<usize>,
    /// Snapshot of buffer text when history nav / search began, so cancel
    /// restores it.
    saved_text: Option<String>,
    search: Option<Search>,
    /// Active `Tab`-completion candidates, if any.
    completions: Option<CompletionState>,
}

/// Ctrl+R search state.
pub struct Search {
    pub query: String,
    /// Indices into `history` (newest-first) matching the query.
    matches: Vec<usize>,
    /// Selected position within `matches`.
    selected: usize,
}

/// `Tab`-completion state. `word_start..word_end` is the column range on the
/// cursor line to replace when a candidate is accepted.
pub struct CompletionState {
    pub matches: Vec<crate::complete::Match>,
    pub selected: usize,
    pub word_start: usize,
    pub word_end: usize,
}

impl Editor {
    pub fn new() -> Self {
        Self {
            buffer: EditorBuffer::new(),
            history: Vec::new(),
            history_idx: None,
            saved_text: None,
            search: None,
            completions: None,
        }
    }

    /// Hydrate from persisted history (oldest→newest; we navigate newest-first).
    pub fn load_history(&mut self, history: Vec<String>) {
        self.history = history;
        // Store newest-first for nav.
        self.history.reverse();
    }

    /// Command history (newest-first) for completion / display.
    pub fn history(&self) -> &[String] {
        &self.history
    }

    pub fn text(&self) -> String {
        self.buffer.text()
    }

    pub fn line_count(&self) -> usize {
        self.buffer.line_count()
    }

    /// Reset to an empty single-line buffer (after submit, or when leaving
    /// AtPrompt).
    pub fn clear(&mut self) {
        self.buffer = EditorBuffer::new();
        self.history_idx = None;
        self.saved_text = None;
        self.search = None;
    }

    pub fn is_searching(&self) -> bool {
        self.search.is_some()
    }

    /// Record a submitted command into the in-memory history (newest-first).
    /// Skips empty commands and exact-duplicates of the most recent entry.
    /// Called by `Terminal::submit_command` so ↑/↓ navigation works.
    pub fn push_history(&mut self, command: &str) {
        let trimmed = command.trim();
        if trimmed.is_empty() {
            return;
        }
        // Dedup against the newest entry only (not the whole list) — matches
        // the common zsh `HIST_FIND_NO_DUPS` behaviour without surprising the
        // user by removing older occurrences.
        if self.history.first().is_some_and(|h| h == command) {
            return;
        }
        self.history.insert(0, command.to_string());
    }

    // ── history navigation (↑/↓) ───────────────────────────────────────

    pub fn history_prev(&mut self) {
        if self.history.is_empty() {
            return;
        }
        if self.history_idx.is_none() {
            // Entering history: snapshot current text for ↓ to restore.
            self.saved_text = Some(self.buffer.text());
            self.history_idx = Some(0);
        } else if let Some(i) = self.history_idx {
            if i + 1 < self.history.len() {
                self.history_idx = Some(i + 1);
            } else {
                return; // clamp at oldest
            }
        }
        self.set_buffer_from_history();
    }

    pub fn history_next(&mut self) {
        let Some(i) = self.history_idx else {
            return;
        };
        if i == 0 {
            // Back to live position.
            self.history_idx = None;
            let restored = self.saved_text.take().unwrap_or_default();
            self.buffer.set_text(&restored);
        } else {
            self.history_idx = Some(i - 1);
            self.set_buffer_from_history();
        }
    }

    fn set_buffer_from_history(&mut self) {
        let i = self.history_idx.unwrap();
        let text = self.history[i].clone();
        self.buffer.set_text(&text);
    }

    // ── Ctrl+R search ──────────────────────────────────────────────────

    pub fn search_start(&mut self) {
        self.saved_text = Some(self.buffer.text());
        self.search = Some(Search {
            query: String::new(),
            matches: Vec::new(),
            selected: 0,
        });
        self.recompute_matches();
    }

    pub fn search_input(&mut self, c: char) {
        if let Some(s) = self.search.as_mut() {
            s.query.push(c);
            s.selected = 0;
        }
        self.recompute_matches();
    }

    pub fn search_backspace(&mut self) {
        if let Some(s) = self.search.as_mut() {
            s.query.pop();
            s.selected = 0;
        }
        self.recompute_matches();
    }

    pub fn search_next(&mut self) {
        if let Some(s) = self.search.as_mut() {
            if !s.matches.is_empty() {
                s.selected = (s.selected + 1) % s.matches.len();
            }
        }
    }

    /// Cycle to the previous match (wraps). Counterpart to `search_next` so
    /// Ctrl+R Up/Down can navigate both directions.
    pub fn search_prev(&mut self) {
        if let Some(s) = self.search.as_mut() {
            let n = s.matches.len();
            if n > 0 {
                s.selected = (s.selected + n - 1) % n;
            }
        }
    }

    pub fn search_accept(&mut self) {
        if let Some(text) = self.search_selected_text() {
            self.buffer.set_text(&text);
        }
        self.search = None;
        self.saved_text = None;
    }

    pub fn search_cancel(&mut self) {
        self.search = None;
        if let Some(text) = self.saved_text.take() {
            self.buffer.set_text(&text);
        }
    }

    /// The currently-selected match text, if any.
    pub fn search_selected_text(&self) -> Option<String> {
        let s = self.search.as_ref()?;
        let &hist_idx = s.matches.get(s.selected)?;
        Some(self.history[hist_idx].clone())
    }

    /// `(query, selected_match_text)` for rendering.
    pub fn search_view(&self) -> Option<(&str, Option<&str>)> {
        let s = self.search.as_ref()?;
        let sel = s.matches.get(s.selected).map(|&i| self.history[i].as_str());
        Some((s.query.as_str(), sel))
    }

    fn recompute_matches(&mut self) {
        let Some(query) = self.search.as_ref().map(|s| s.query.clone()) else {
            return;
        };
        // history is newest-first; matches keep that order.
        let matches: Vec<usize> = self
            .history
            .iter()
            .enumerate()
            .filter(|(_, h)| is_subsequence(&query, h))
            .map(|(i, _)| i)
            .collect();
        if let Some(s) = self.search.as_mut() {
            s.matches = matches;
            if s.selected >= s.matches.len() && !s.matches.is_empty() {
                s.selected = 0;
            }
        }
    }

    // ── Tab completion ────────────────────────────────────────────────────

    pub fn is_completing(&self) -> bool {
        self.completions.is_some()
    }

    /// Begin a completion session with `matches` (must be non-empty). The word
    /// to replace occupies columns `word_start..word_end` on the cursor line.
    pub fn start_completion(
        &mut self,
        matches: Vec<crate::complete::Match>,
        word_start: usize,
        word_end: usize,
    ) {
        if matches.is_empty() {
            return;
        }
        self.completions = Some(CompletionState {
            matches,
            selected: 0,
            word_start,
            word_end,
        });
    }

    pub fn completion_next(&mut self) {
        if let Some(c) = self.completions.as_mut() {
            if c.matches.len() > 1 {
                c.selected = (c.selected + 1) % c.matches.len();
            }
        }
    }

    pub fn completion_prev(&mut self) {
        if let Some(c) = self.completions.as_mut() {
            if c.matches.len() > 1 {
                c.selected = c.selected.checked_sub(1).unwrap_or(c.matches.len() - 1);
            }
        }
    }

    /// Accept the selected completion: replace the word range on the cursor
    /// line with the candidate's insert text, move the cursor, and clear the
    /// session. Returns false if no session was active.
    pub fn completion_accept(&mut self) -> bool {
        let st = match self.completions.take() {
            Some(s) => s,
            None => return false,
        };
        let insert = st.matches[st.selected].insert.clone();
        let line = self.buffer.cursor.0;
        if let Some(l) = self.buffer.lines.get_mut(line) {
            let chars: Vec<char> = l.chars().collect();
            let ws = st.word_start.min(chars.len());
            let we = st.word_end.min(chars.len()).max(ws);
            let insert_chars: Vec<char> = insert.chars().collect();
            let mut rebuilt: Vec<char> = Vec::with_capacity(chars.len() + insert_chars.len());
            rebuilt.extend(chars[..ws].iter());
            rebuilt.extend(insert_chars.iter());
            rebuilt.extend(chars[we..].iter());
            *l = rebuilt.iter().collect();
            self.buffer.cursor.1 = ws + insert.chars().count();
        }
        true
    }

    pub fn completion_cancel(&mut self) {
        self.completions = None;
    }

    /// `(matches, selected)` for rendering the dropdown.
    pub fn completion_view(&self) -> Option<(&[crate::complete::Match], usize)> {
        self.completions
            .as_ref()
            .map(|c| (c.matches.as_slice(), c.selected))
    }
}

impl Default for Editor {
    fn default() -> Self {
        Self::new()
    }
}

/// True when every char of `query` appears in `candidate` in order (fuzzy
/// subsequence). Empty query matches nothing (so an empty search box shows no
/// preview, matching the test).
fn is_subsequence(query: &str, candidate: &str) -> bool {
    if query.is_empty() {
        return false;
    }
    let mut q = query.chars().peekable();
    for c in candidate.chars() {
        if q.peek() == Some(&c) {
            q.next();
        }
    }
    q.peek().is_none()
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

    fn editor_with_history(history: &[&str]) -> Editor {
        let mut e = Editor::new();
        e.load_history(history.iter().map(|s| s.to_string()).collect());
        e
    }

    #[test]
    fn history_prev_fills_buffer_and_clamps() {
        let mut e = editor_with_history(&["first", "second"]);
        e.history_prev();
        assert_eq!(e.buffer.text(), "second"); // newest first
        e.history_prev();
        assert_eq!(e.buffer.text(), "first");
        e.history_prev(); // clamps at oldest
        assert_eq!(e.buffer.text(), "first");
    }

    #[test]
    fn history_next_returns_to_empty() {
        let mut e = editor_with_history(&["only"]);
        e.history_prev();
        e.history_next();
        assert_eq!(e.buffer.text(), "");
    }

    #[test]
    fn push_history_adds_commands_newest_first() {
        let mut e = Editor::new();
        assert!(e.history().is_empty());
        e.push_history("ls");
        e.push_history("git status");
        e.push_history("pwd");
        // newest-first
        assert_eq!(e.history(), &["pwd", "git status", "ls"]);
    }

    #[test]
    fn push_history_skips_empty_and_dedup_newest() {
        let mut e = Editor::new();
        e.push_history("ls");
        e.push_history(""); // skipped
        e.push_history("   "); // skipped (whitespace only)
        e.push_history("ls"); // dedup against newest
        assert_eq!(e.history(), &["ls"]);
        e.push_history("pwd");
        assert_eq!(e.history(), &["pwd", "ls"]);
    }

    #[test]
    fn submit_command_pushes_to_history() {
        // Verify that the Terminal wiring records submitted commands.
        use crate::vt::Terminal;
        let mut t = Terminal::new(24, 80);
        // Simulate typing a command into the editor.
        for c in "ls -la".chars() {
            t.editor_mut().buffer.insert_char(c);
        }
        t.submit_command();
        assert_eq!(t.editor().history(), &["ls -la"]);
        // ↑ should now recall it.
        t.editor_mut().history_prev();
        assert_eq!(t.editor().text(), "ls -la");
    }

    #[test]
    fn search_finds_subsequence_match() {
        let mut e = editor_with_history(&["git status", "git push", "ls -la"]);
        e.search_start();
        e.search_input('g');
        e.search_input('p'); // query "gp" matches "git push" only
        assert_eq!(e.search_selected_text(), Some("git push".to_string()));
    }

    #[test]
    fn search_next_cycles_through_matches() {
        let mut e = editor_with_history(&["git status", "git push"]);
        e.search_start();
        e.search_input('g');
        e.search_input('i');
        e.search_input('t'); // "git" matches both
        assert_eq!(e.search_selected_text(), Some("git push".to_string())); // newest
        e.search_next();
        assert_eq!(e.search_selected_text(), Some("git status".to_string()));
    }

    #[test]
    fn search_accept_fills_buffer() {
        let mut e = editor_with_history(&["git push"]);
        e.search_start();
        e.search_input('g');
        e.search_accept();
        assert_eq!(e.buffer.text(), "git push");
        assert!(!e.is_searching());
    }

    #[test]
    fn search_cancel_restores_original() {
        let mut e = editor_with_history(&["git push"]);
        e.buffer.insert_char('x');
        e.search_start();
        e.search_input('g');
        e.search_cancel();
        assert_eq!(e.buffer.text(), "x");
        assert!(!e.is_searching());
    }

    #[test]
    fn search_no_match_keeps_empty_selection() {
        let mut e = editor_with_history(&["ls"]);
        e.search_start();
        e.search_input('z');
        assert_eq!(e.search_selected_text(), None);
    }

    #[test]
    fn insert_char_rejects_control_chars() {
        let mut b = EditorBuffer::new();
        b.insert_char('a');
        b.insert_char('\x1b'); // ESC — spoofing/injection vector, rejected
        b.insert_char('\x00'); // NUL
        b.insert_char('b');
        assert_eq!(b.text(), "ab");
        assert_eq!(b.cursor, (0, 2));
    }

    #[test]
    fn set_text_replaces_and_positions_cursor_at_end() {
        let mut b = EditorBuffer::new();
        b.set_text("ab\ncd");
        assert_eq!(b.text(), "ab\ncd");
        assert_eq!(b.cursor, (1, 2));
        // Empty -> single empty line, cursor at origin.
        b.set_text("");
        assert_eq!(b.text(), "");
        assert_eq!(b.cursor, (0, 0));
    }

    #[test]
    fn search_prev_navigates_backwards_and_wraps() {
        let mut e = editor_with_history(&["git status", "git push"]);
        e.search_start();
        for c in "git".chars() {
            e.search_input(c);
        }
        assert_eq!(e.search_selected_text(), Some("git push".to_string())); // newest
        e.search_prev();
        assert_eq!(e.search_selected_text(), Some("git status".to_string()));
        e.search_prev(); // wraps back to newest
        assert_eq!(e.search_selected_text(), Some("git push".to_string()));
    }

    // ── Tab completion ────────────────────────────────────────────────────

    use crate::complete::{Match, MatchKind};

    fn m(label: &str, insert: &str) -> Match {
        Match {
            label: label.into(),
            kind: MatchKind::Command,
            insert: insert.into(),
            is_dir: false,
        }
    }

    #[test]
    fn start_then_next_cycles() {
        let mut e = Editor::new();
        e.start_completion(vec![m("a", "a"), m("b", "b"), m("c", "c")], 0, 1);
        assert_eq!(e.completion_view().unwrap().1, 0);
        e.completion_next();
        assert_eq!(e.completion_view().unwrap().1, 1);
        e.completion_next();
        assert_eq!(e.completion_view().unwrap().1, 2);
        e.completion_next(); // wraps
        assert_eq!(e.completion_view().unwrap().1, 0);
    }

    #[test]
    fn prev_wraps_to_last() {
        let mut e = Editor::new();
        e.start_completion(vec![m("a", "a"), m("b", "b")], 0, 1);
        e.completion_prev(); // from 0 -> wraps to last
        assert_eq!(e.completion_view().unwrap().1, 1);
    }

    #[test]
    fn cancel_clears() {
        let mut e = Editor::new();
        e.start_completion(vec![m("a", "a")], 0, 1);
        assert!(e.is_completing());
        e.completion_cancel();
        assert!(!e.is_completing());
    }

    #[test]
    fn accept_applies_insert_and_clears() {
        let mut e = Editor::new();
        e.buffer.lines = vec!["ls".to_string()];
        e.buffer.cursor = (0, 2); // cursor at end of "ls"
                                  // replace the whole word "ls" (cols 0..2) with the selected insert "lsof"
        e.start_completion(vec![m("lsof", "lsof")], 0, 2);
        assert!(e.completion_accept());
        assert_eq!(e.buffer.lines[0], "lsof");
        assert_eq!(e.buffer.cursor.1, 4); // cursor after "lsof"
        assert!(!e.is_completing());
    }

    #[test]
    fn accept_preserves_text_around_word() {
        let mut e = Editor::new();
        e.buffer.lines = vec!["echo ls more".to_string()];
        e.buffer.cursor = (0, 7); // cursor right after "ls"
                                  // word "ls" occupies cols 5..7
        e.start_completion(vec![m("lsof", "lsof")], 5, 7);
        assert!(e.completion_accept());
        assert_eq!(e.buffer.lines[0], "echo lsof more");
    }

    #[test]
    fn accept_returns_false_when_not_completing() {
        let mut e = Editor::new();
        assert!(!e.completion_accept());
    }

    #[test]
    fn start_with_empty_matches_is_noop() {
        let mut e = Editor::new();
        e.start_completion(vec![], 0, 1);
        assert!(!e.is_completing());
    }
}
