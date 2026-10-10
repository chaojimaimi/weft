// arch-gate: allow-over-800
// EditorBuffer: multi-line text editor with cursor + editing operations.
// Tightly coupled state (text + cursor + selection + history); splitting
// would require threading mutable borrows across modules.
//! Multi-line editor buffer for the owned input box (v0.5 "Shuttle", phase 2).
//!
//! Pure logic: text + cursor + editing operations. No I/O, no rendering.
//! The `Terminal` owns an `Editor` (which wraps this buffer) and the app
//! layer maps winit key events to these methods.

/// A multi-line text buffer with a cursor. `cursor.0` = line index,
/// `cursor.1` = char column within that line (0..=line_char_count).
///
/// v1.0 H4: derives `Serialize`/`Deserialize` so tab sessions can be
/// persisted to SQLite and restored on startup.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct EditorBuffer {
    pub lines: Vec<String>,
    pub cursor: (usize, usize),
    /// v0.9: anchor for mouse-drag selection (click-drag in the prompt box).
    /// `None` = no active selection. The selection spans between `cursor`
    /// and `selection_anchor` (order-independent). Cleared by any cursor
    /// movement key or text edit, and by `clear_selection`.
    pub selection_anchor: Option<(usize, usize)>,
    /// F2 P0-1: vertical scroll offset for multi-line input when the box is
    /// clamped to 30% of the viewport. Lines `[scroll_offset, scroll_offset +
    /// visible_rows)` are rendered. Kept at 0 when all lines fit. `#[serde(default)]`
    /// so persisted buffers from before this field deserialize with offset 0.
    #[serde(default)]
    pub scroll_offset: usize,
}

impl EditorBuffer {
    pub fn new() -> Self {
        Self {
            lines: vec![String::new()],
            cursor: (0, 0),
            selection_anchor: None,
            scroll_offset: 0,
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

    /// F2 P0-1: Adjust `scroll_offset` so the cursor line stays inside the
    /// visible window `[scroll_offset, scroll_offset + visible_rows)`. When
    /// all lines fit (`lines.len() <= visible_rows`), resets to 0. Called by
    /// the redraw path before rendering the prompt box, so any cursor
    /// movement / text edit / resize is covered.
    pub fn ensure_cursor_visible(&mut self, visible_rows: usize) {
        let n_lines = self.lines.len();
        if n_lines <= 1 || visible_rows == 0 {
            self.scroll_offset = 0;
            return;
        }
        // If everything fits, no scrolling needed.
        if n_lines <= visible_rows {
            self.scroll_offset = 0;
            return;
        }
        let cursor_line = self.cursor.0.min(n_lines - 1);
        // Scroll up if the cursor is above the visible window.
        if cursor_line < self.scroll_offset {
            self.scroll_offset = cursor_line;
            return;
        }
        // Scroll down if the cursor is at or below the visible window's end.
        let last_visible = self.scroll_offset + visible_rows;
        if cursor_line >= last_visible {
            self.scroll_offset = cursor_line + 1 - visible_rows;
        }
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

// Tests live in the gate-exempt sibling module (repo test-module
// convention, pty/tests.rs precedent) so inline test lines stay out of
// the production-file budget.
#[cfg(test)]
#[path = "editor/tests.rs"]
mod tests;
