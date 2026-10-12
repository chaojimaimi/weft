//! Screen-prefix (v1.10.25 scroll-out incremental capture) method family,
//! moved verbatim out of `output_capture.rs` (v1.13.9 backlog clear
//! zero-behavior file-budget split; `impl OutputCapture` cross-file block
//! per the cursor.rs precedent).

use super::{OutputCapture, StyledOutput};

impl OutputCapture {
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
}
