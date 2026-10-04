//! Terminal-like single-line rewrite handling for detached block outputs.
//!
//! v1.7.0-A: In addition to the text buffer, the capture maintains a bounded
//! RLE of `CapturedStyleRun` synced with every text mutation. The text buffer
//! remains the single source of truth for cursor position and truncation; the
//! style RLE is a parallel index keyed by char position. Style runs are only
//! stored for ranges where the program emitted non-default SGR attributes —
//! default-styled ranges are implicit (no run). Over-limit runs set
//! `style_overflow` and are dropped; text is never affected.

use std::sync::atomic::{AtomicUsize, Ordering};

use super::style::{
    build_styled_output_from_runs, CapturedStyle, CapturedStyleRun, MAX_STYLE_RUNS_PER_BLOCK,
};
use super::DEFAULT_OUTPUT_CAP;

/// PLAN_v11217 §3.5 (T4): the finalize truncation marker. Reports the cap in
/// MiB and clarifies that only the block excerpt is bounded — the full output
/// remains in the grid/scrollback (the "truncation = data loss" misreading is
/// the T4 copy fix).
fn truncation_marker(cap_bytes: usize) -> String {
    format!(
        "\n…(block excerpt truncated at {} MiB — full output remains in scrollback)",
        cap_bytes / (1024 * 1024)
    )
}

#[cfg(test)]
mod crlf_tests;
mod cursor;
#[cfg(test)]
mod prompt_sp_tests;
#[cfg(test)]
mod watermark_tests;

#[derive(Debug)]
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
    /// v1.10.25 (FIX_TUI_HISTORY_INCREMENTAL): append-only scroll-out capture
    /// segment for screen-owned TUI sessions. Rows pushed out of the viewport
    /// by LF overflow / CSI S are appended here (owned rows only, in push
    /// order) instead of relying on the scrollback ring — ring eviction is
    /// decoupled from TUI history. The 50ms snapshot rebuilds the composed
    /// block text (which folds the prefix in), so `text` itself is not touched
    /// by appends.
    screen_prefix: String,
    /// Parallel styled lines for the prefix (line indices local to the prefix).
    screen_prefix_styled: Option<StyledOutput>,
    /// v1.11.12 (PLAN_v11112 M-A): line-count ledger for `screen_prefix`,
    /// maintained at every mutation point (`append_screen_prefix`,
    /// `drain_screen_prefix`, `clear`). Semantics identical to the old O(n)
    /// recompute: `screen_prefix.matches('\n').count() +
    /// usize::from(!screen_prefix.is_empty())`. The recompute was paid on
    /// every prefix append and every snapshot refresh (`screen_head_lines`).
    /// Invariant tests pin counter == recompute
    /// (`vt/screen_exit/tests.rs`).
    line_count: usize,
    /// M6-a (PLAN_M6 §A-1): rewrite watermark — the EARLIEST byte offset any
    /// content/cursor operation touched since the last sync consumption.
    /// `usize::MAX` means "pure append since the last take". Every mutation
    /// op records its earliest-reachable offset; the live layout cache's
    /// incremental append path consumes this via `take_min_write_offset` as
    /// its authoritative guard: a value below the synced boundary means the
    /// document was rewritten in place and the fast path must fall back to a
    /// full rebuild. Atomic only so the always-fresh handle can be shared
    /// through `InFlightBlock` (see `InFlightBlock::detached_watermark`).
    min_write_offset: AtomicUsize,
    /// PLAN_v11217 §3.5 (T4): the cap the finalize truncation marker reports,
    /// in bytes. Metadata only — never a bounding input (every bounding call
    /// site passes its own `max_bytes`). Defaults to [`DEFAULT_OUTPUT_CAP`]
    /// so the four construction sites that go through `Default` (tracker
    /// init, `Terminal::preexec_staging`, the interrupt tail, and the
    /// `mem::take` reset in `take_orphan_staging`) can never report
    /// "truncated at 0 MiB"; the tracker path resyncs this field to the
    /// configured value (review P2c).
    cap_bytes: usize,
}

impl Clone for OutputCapture {
    fn clone(&self) -> Self {
        Self {
            text: self.text.clone(),
            cursor: self.cursor,
            char_cursor: self.char_cursor,
            line_start_char: self.line_start_char,
            truncated: self.truncated,
            style_runs: self.style_runs.clone(),
            style_overflow: self.style_overflow,
            screen_prefix: self.screen_prefix.clone(),
            screen_prefix_styled: self.screen_prefix_styled.clone(),
            line_count: self.line_count,
            // The clone inherits the current watermark value (clones of a
            // capture are snapshots; the atomic itself must stay unique to
            // the live handle chain).
            min_write_offset: AtomicUsize::new(self.min_write_offset.load(Ordering::Relaxed)),
            cap_bytes: self.cap_bytes,
        }
    }
}

impl Default for OutputCapture {
    fn default() -> Self {
        Self {
            text: String::new(),
            cursor: 0,
            char_cursor: 0,
            line_start_char: 0,
            truncated: false,
            style_runs: Vec::new(),
            style_overflow: false,
            screen_prefix: String::new(),
            screen_prefix_styled: None,
            line_count: 0,
            min_write_offset: AtomicUsize::new(usize::MAX),
            // Review P2c fallback: every Default-constructed capture reports
            // the historical 1 MiB default, never 0.
            cap_bytes: DEFAULT_OUTPUT_CAP,
        }
    }
}

impl OutputCapture {
    pub(crate) fn as_str(&self) -> &str {
        &self.text
    }

    /// PLAN_v11217 §3.5 (T4): record the cap the finalize truncation marker
    /// should report. Metadata sync only — the tracker calls this from
    /// `set_output_cap` (live cap change) and from `on_orphan_command_end`
    /// (the staged capture swapped in was built under `Default`). Purely
    /// cosmetic until the next `take_styled`.
    pub(crate) fn set_cap_bytes(&mut self, cap_bytes: usize) {
        self.cap_bytes = cap_bytes;
    }

    /// The recorded cap (test observability for the metadata-sync contract).
    #[cfg(test)]
    pub(crate) fn cap_bytes(&self) -> usize {
        self.cap_bytes
    }

    // ── M6-a rewrite watermark (PLAN_M6 §A-1) ────────────────────────────

    /// Record the earliest byte offset an operation touched. `fetch_min`
    /// keeps the low-water mark; `usize::MAX` means "pure append so far".
    fn note_min_write_offset(&self, offset: usize) {
        self.min_write_offset.fetch_min(offset, Ordering::Relaxed);
    }

    /// Read and reset the watermark (take semantics — the live layout
    /// cache's sync consumes it exactly once, after which the capture is
    /// "pure append" again until the next in-place op). Production
    /// consumers go through `InFlightBlock::take_min_write_offset` (which
    /// swaps the shared handle); this direct twin serves the unit tests.
    #[cfg(test)]
    pub(crate) fn take_min_write_offset(&self) -> usize {
        self.min_write_offset.swap(usize::MAX, Ordering::Relaxed)
    }

    /// Shared handle for `InFlightBlock` — lets the renderer's two sync call
    /// sites take the watermark through an immutable borrow of the tracker.
    pub(crate) fn min_write_offset_handle(&self) -> &AtomicUsize {
        &self.min_write_offset
    }

    /// Current watermark value without consuming (test observability).
    #[cfg(test)]
    pub(crate) fn min_write_offset_value(&self) -> usize {
        self.min_write_offset.load(Ordering::Relaxed)
    }

    pub(crate) fn clear(&mut self) {
        self.text.clear();
        self.cursor = 0;
        self.char_cursor = 0;
        self.line_start_char = 0;
        self.truncated = false;
        self.style_runs.clear();
        self.style_overflow = false;
        self.screen_prefix.clear();
        self.screen_prefix_styled = None;
        // v1.11.12 (PLAN_v11112 M-A): the second reset mechanism (alongside
        // `ScreenHistory::default`) — the ledger must be zeroed with the text.
        self.line_count = 0;
        // M6-a: the cleared document shares no bytes with the prefix the live
        // cache already consumed. Recording offset 0 makes the next sync's
        // watermark guard fail whenever a consumption boundary exists
        // (synced_byte_end > 0), forcing the full rebuild the new document
        // needs. When synced_byte_end == 0 the cache's window is empty or a
        // single partial line at offset 0, which the append path's
        // partial-line replacement reconciles without a rebuild.
        self.min_write_offset.store(0, Ordering::Relaxed);
    }

    pub(crate) fn replace(&mut self, text: &str, max_bytes: usize) {
        // M6-a watermark: NOT recorded — `replace` is the screen-snapshot
        // path only (`replace_screen_snapshot` gates on
        // `screen_document_start.is_some()`), and a screen-origin live
        // document never takes the append fast path (the sync guard requires
        // `screen_origin == false`), so the existing guard compensates.
        // v1.10.25 (FIX_TUI_HISTORY_INCREMENTAL): the screen prefix is NOT
        // reset here — the composed text passed in already folds the prefix
        // in (the Terminal builds it from the tracker's prefix copy), and
        // that copy must survive the rebuild so the NEXT snapshot can fold it
        // in again. Only the screen segment (text buffer) is replaced.
        self.text.clear();
        self.cursor = 0;
        self.char_cursor = 0;
        self.line_start_char = 0;
        self.truncated = false;
        self.style_runs.clear();
        self.style_overflow = false;
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
        // M6-a watermark: record the pre-write cursor — an overwrite may
        // touch any byte from the cursor onward, an append only appends at
        // it; both are covered by the cursor itself.
        self.note_min_write_offset(self.cursor);
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
            // v1.12.23 audit batch 1: encode_utf8 into a stack buffer —
            // to_string() heap-allocated once per overwritten char.
            let mut buf = [0u8; 4];
            let s = c.encode_utf8(&mut buf);
            self.text.replace_range(self.cursor..end, s);
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
            // M6-a watermark: NOT recorded — this fast path is append-only
            // (cursor sits at the document end, bytes are pushed at the tail
            // and no existing byte is touched), so it can never lower the
            // watermark below an existing consumption boundary. The per-byte
            // slow path below delegates to `print`, which records.
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
        // M6-a watermark: pre-write cursor — the LF may overwrite from the
        // cursor to the line end and the cursor is always within the tail
        // line (>= the sync boundary), so this records conservatively.
        self.note_min_write_offset(self.cursor);
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
        // M6-a watermark: pre-write cursor. Modes 0/1/2 all rewrite inside the
        // current line, whose start is the consumption boundary (tail line)
        // or below it (cursor parked on an early row after CSI A — the
        // recorded cursor then forces the full-rebuild fallback).
        self.note_min_write_offset(self.cursor);
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
        // M6-a watermark: the TARGET row's start — every byte this op touches
        // (column-walk inserts, subsequent prints at the landed cursor) is at
        // or after it, and the row-materialization loop above only appends at
        // the document end. An early-row jump records below the consumption
        // boundary, which invalidates the append fast path (the intent).
        self.note_min_write_offset(line_start);
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
        let mut runs = std::mem::take(&mut self.style_runs);
        let overflow = self.style_overflow;
        let truncated = self.truncated;
        self.cursor = 0;
        self.char_cursor = 0;
        self.line_start_char = 0;
        self.truncated = false;
        self.style_overflow = false;

        // WHY: zsh's PROMPT_SP prompt-cleanup mechanism emits a full line
        // width of literal spaces + \r\r before every prompt; those bytes
        // land inside the OSC 133;C→D capture window and used to be stored
        // as trailing block output (cols=99 → exactly 99 trailing spaces).
        // Strip the trailing whitespace run at finalize. See
        // docs/FIX_CAPTURE_PROMPT_SP_SPACES.md.
        let trimmed_len = text.trim_end().len();
        if trimmed_len < text.len() {
            text.truncate(trimmed_len);
            // Style runs are char-indexed over the pre-strip buffer — drop
            // runs that fall entirely inside the stripped tail and clamp
            // the survivors so none index past the new text end.
            let new_char_count = text.chars().count() as u32;
            runs.retain(|run| run.start_char < new_char_count);
            for run in &mut runs {
                run.end_char = run.end_char.min(new_char_count);
            }
        }

        let styled = if overflow || runs.is_empty() {
            None
        } else {
            build_styled_output_from_runs(&text, &runs)
        };

        if truncated {
            let marker = truncation_marker(self.cap_bytes);
            text.push_str(&marker);
        }
        (text, styled)
    }

    /// Non-consuming twin of [`Self::take_styled`]'s StyledOutput construction:
    /// builds a snapshot from the CURRENT text + style runs without draining
    /// anything. Returns None when no non-default runs were captured or the
    /// run cap overflowed (mirroring take_styled's semantics).
    /// Deliberately does NOT apply take_styled's finalize-only transforms
    /// (PROMPT_SP tail strip, truncation marker) — those stay finalize-only.
    pub(crate) fn peek_styled(&self) -> Option<StyledOutput> {
        if self.style_overflow || self.style_runs.is_empty() {
            return None;
        }
        build_styled_output_from_runs(&self.text, &self.style_runs)
    }

    /// Whether the per-block style-run cap has overflowed. `peek_styled`
    /// conflates "no styles" and "overflow" into None; the live publisher
    /// needs them apart — overflow freezes the last good snapshot, a genuine
    /// runs-clear (all-default rewrite) must drop it.
    pub(crate) fn style_overflow(&self) -> bool {
        self.style_overflow
    }

    /// O(1) twin used by the boundary publisher's category pre-check:
    /// whether any non-default style runs exist. Together with
    /// [`Self::style_overflow`] this predicts `peek_styled().is_some()`
    /// without building a snapshot.
    pub(crate) fn style_runs_empty(&self) -> bool {
        self.style_runs.is_empty()
    }

    // ── Screen prefix (v1.10.25: scroll-out incremental capture) ────────

    /// Append one scroll-captured segment (owned rows pushed out of the
    /// viewport, already filtered and ordered by the caller's pure function)
    /// to the append-only screen prefix. The segment's styled lines carry
    /// prefix-local indices and are shifted by the current prefix line count
    /// so the prefix styled index stays contiguous.
    pub(crate) fn append_screen_prefix(&mut self, text: &str, styled: Option<StyledOutput>) {
        if text.is_empty() {
            return;
        }
        let line_offset = self.screen_prefix_line_count();
        // v1.11.12 (PLAN_v11112 M-A): line-ledger delta — one line per '\n'
        // in the segment plus exactly ONE more (the separator '\n' when the
        // prefix was non-empty, else the segment's own final unterminated
        // line — the same +1 either way).
        let appended_lines = text.matches('\n').count() + 1;
        if !self.screen_prefix.is_empty() {
            self.screen_prefix.push('\n');
        }
        self.screen_prefix.push_str(text);
        self.line_count += appended_lines;
        if let Some(styled) = styled.filter(|s| s.has_colors()) {
            let mut styled = styled;
            for line in &mut styled.lines {
                line.line = line.line.saturating_add(line_offset as u32);
            }
            match &mut self.screen_prefix_styled {
                Some(acc) => acc.lines.extend(styled.lines),
                None => self.screen_prefix_styled = Some(styled),
            }
        }
    }

    /// Drop the first `consumed` bytes of the screen prefix — the part folded
    /// into a finished block by the 1MiB split. The boundary is always a line
    /// boundary of the prefix, so the remaining text is intact.
    pub(crate) fn drain_screen_prefix(&mut self, consumed: usize) {
        if consumed == 0 {
            return;
        }
        if consumed >= self.screen_prefix.len() {
            self.screen_prefix.clear();
            self.screen_prefix_styled = None;
            self.line_count = 0;
            return;
        }
        // `consumed < len` here (the `consumed >= len` case cleared above):
        // the split boundary is a '\n' inside the prefix (or an
        // overlong-line mid-cut) — complete lines fully owned by the head
        // are exactly the newlines in the consumed bytes.
        let consumed_lines = self.screen_prefix[..consumed].matches('\n').count();
        // v1.11.12 (PLAN_v11112 M-A): mirror the ledger decrement — for a
        // partial drain the removed lines are exactly the newlines in the
        // consumed region (the remaining tail keeps its own final line).
        self.line_count = self.line_count.saturating_sub(consumed_lines);
        self.screen_prefix.drain(..consumed);
        if let Some(styled) = &mut self.screen_prefix_styled {
            styled
                .lines
                .retain(|line| (line.line as usize) >= consumed_lines);
            for line in &mut styled.lines {
                line.line = line.line.saturating_sub(consumed_lines as u32);
            }
            if styled.lines.is_empty() {
                self.screen_prefix_styled = None;
            }
        }
    }

    pub(crate) fn screen_prefix_len(&self) -> usize {
        self.screen_prefix.len()
    }

    pub(crate) fn screen_prefix_text(&self) -> &str {
        &self.screen_prefix
    }

    pub(crate) fn screen_prefix_styled(&self) -> Option<&StyledOutput> {
        self.screen_prefix_styled.as_ref()
    }

    /// Number of text lines in the screen prefix — the styled-line offset
    /// applied to the viewport segment when the three-part composed block
    /// text is built.
    ///
    /// v1.11.12 (PLAN_v11112 M-A): O(1) read of the mutation-maintained
    /// ledger instead of an O(n) full-string recompute.
    pub(crate) fn screen_prefix_line_count(&self) -> usize {
        self.line_count
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
mod cap_tests;
