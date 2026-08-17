//! Document-boundary freeze and scroll-driven ownership transforms for the
//! primary screen.

use super::Terminal;
use crate::blocks::{
    extract_owned_pushed_rows, line_boundary_at_or_before, styled_lines_from, styled_lines_in,
    StyledOutput, MAX_OUTPUT_BYTES,
};

/// v1.10.23 (FIX_OMP_CONTENT_LOSS): minimum preserved-frame size (snapshot
/// text lines). Smaller frames are resize-jitter repaints — preserving them
/// would fragment the block history with tiny partial frames.
const PRIMARY_SCREEN_FRAME_PRESERVE_MIN_LINES: usize = 4;

impl Terminal {
    /// v1.10.23 (FIX_OMP_CONTENT_LOSS): discard the scrollback of a DEC 2026
    /// full-frame repaint AFTER preserving the superseded document into the
    /// in-flight block's history (see [`Self::preserve_superseded_primary_screen_frame`]),
    /// so streamed paragraphs survive the clear and history review stays
    /// complete. `preserve_viewport` is true only when the caller is about to
    /// blank the viewport as well (CSI 2J). The synchronized-frame-finish
    /// caller keeps the freshly repainted viewport, so it preserves only the
    /// scrollback rows (the part the clear actually destroys) — including the
    /// surviving viewport would duplicate the current frame in the block
    /// history.
    pub(super) fn discard_superseded_primary_screen_frame(&mut self, preserve_viewport: bool) {
        let preserved_lines = self.preserve_superseded_primary_screen_frame(preserve_viewport);
        self.clear_primary_screen_scrollback();
        let origin = self.grid.scrollback.position();
        self.capabilities.primary_screen_document_candidate = origin;
        if self.block_tracker.screen_document_start().is_some() {
            self.block_tracker.set_screen_document_start(origin);
        }
        tracing::info!(
            origin,
            preserved_lines,
            "discarded superseded atomic primary-screen frame"
        );
    }

    /// Snapshot the document content about to be destroyed by the scrollback
    /// clear and append it to the preservation history. Returns the number of
    /// preserved text lines (0 when skipped).
    ///
    /// Screen-owned sessions only: print capture is OFF while screen-owned
    /// (`BlockTracker::is_capturing` requires no `screen_document_start`), so
    /// this snapshot is the only transcript source for the discarded frame —
    /// there is no double-capture risk.
    ///
    /// `include_viewport` — true: the caller is about to blank the viewport
    /// too (CSI 2J), so the whole document (scrollback + viewport from
    /// `document_start`) is preserved. false: the viewport already holds the
    /// NEW frame (every row was cleared + repainted inside the sync window) —
    /// only the scrollback rows from `document_start` are at risk, so those
    /// are preserved and the surviving frame is left out (no duplication).
    fn preserve_superseded_primary_screen_frame(&mut self, include_viewport: bool) -> usize {
        let Some(document_start) = self.block_tracker.screen_document_start() else {
            return 0;
        };
        let (text, styled) = if include_viewport {
            let (text, styled, _) = self.primary_screen_document_snapshot(document_start);
            (text, styled)
        } else {
            // Ownership masks still apply: only rows the TUI actually owned
            // belong to the document transcript.
            let (text, styled, _) = self
                .grid
                .document_snapshot_from_position_with_ownership_masks_and_resolver(
                    document_start,
                    &self.capabilities.primary_screen_ownership.scrollback,
                    &[],
                    |id| self.hyperlinks.url(id).map(std::sync::Arc::<str>::from),
                );
            (text, styled)
        };
        let lines = text.lines().count();
        if lines < PRIMARY_SCREEN_FRAME_PRESERVE_MIN_LINES {
            return 0;
        }
        self.append_screen_history_frame(&text, styled);
        lines
    }

    /// v1.10.23 (FIX_OMP_CONTENT_LOSS): append one superseded document frame
    /// to the preservation history that [`Self::compose_screen_history`]
    /// prepends to every screen snapshot. Bounded at `MAX_OUTPUT_BYTES` —
    /// frames beyond the cap are dropped (head-keeping, matching the snapshot
    /// truncation semantics).
    fn append_screen_history_frame(&mut self, text: &str, styled: StyledOutput) {
        let offset = self.screen_history_lines();
        let history = &mut self.capabilities.screen_history;
        if text.is_empty() {
            return;
        }
        if history.text.len() >= crate::blocks::MAX_OUTPUT_BYTES {
            // Capacity drop must be observable — the frame is preserved
            // nowhere else after the scrollback clear below.
            tracing::warn!(
                bytes = history.text.len(),
                dropped = text.len(),
                "screen history at 1MiB cap; dropping superseded frame"
            );
            return;
        }
        if !history.text.is_empty() {
            history.text.push('\n');
        }
        history.text.push_str(text);
        if styled.has_colors() {
            let mut styled = styled;
            for line in &mut styled.lines {
                line.line = line.line.saturating_add(offset as u32);
            }
            match &mut history.styled {
                Some(acc) => acc.lines.extend(styled.lines),
                None => history.styled = Some(styled),
            }
        }
    }

    /// Number of text lines in the preserved-frame history — the offset
    /// prepended to every screen snapshot (caret anchor and drag-selection
    /// migration must shift by it to stay on the rendered block rows).
    pub(super) fn screen_history_lines(&self) -> usize {
        let text = &self.capabilities.screen_history.text;
        text.matches('\n').count() + usize::from(!text.is_empty())
    }

    /// v1.10.25 (FIX_TUI_HISTORY_INCREMENTAL): number of text lines in the
    /// scroll-out prefix — the second segment of the three-part composed
    /// block text. Caret and drag-selection offsets shift by it.
    pub(super) fn screen_prefix_lines(&self) -> usize {
        self.block_tracker.screen_prefix_line_count()
    }

    /// v1.10.26 (rust-reviewer S1): total lines prepended to the live composed
    /// document's head (preserved-frame history + scroll-out prefix). Live
    /// content anchors are indices INTO the composed document, so a head grow
    /// (`append_screen_history_frame` — a superseded frame preserved while a
    /// live-segment selection is active) silently shifts every live anchor.
    /// The app includes this in the block-selection fingerprint so a head
    /// change clears the selection (same semantic as a structural block
    /// change; a tail append stays index-stable and never clears).
    pub fn screen_head_lines(&self) -> usize {
        self.screen_history_lines() + self.screen_prefix_lines()
    }

    /// v1.10.23 (FIX_OMP_CONTENT_LOSS): prepend the accumulated superseded
    /// frames to a fresh document snapshot. The history is the stable
    /// transcript head; the snapshot is the live tail. Zero-cost (text passed
    /// through unchanged) when no frames were preserved.
    ///
    /// v1.10.25 (FIX_TUI_HISTORY_INCREMENTAL): the composed block text is now
    /// THREE parts in document order — preserved-frame history, scroll-out
    /// prefix (captured from rows pushed out of the viewport), and the
    /// current viewport snapshot. The prefix is appended by
    /// [`Self::capture_scrolled_out_screen_rows`] and folded in here.
    pub(super) fn compose_screen_history(
        &self,
        text: String,
        styled: StyledOutput,
    ) -> (String, StyledOutput) {
        let history = &self.capabilities.screen_history;
        let prefix_text = self.block_tracker.screen_prefix_text();
        if history.text.is_empty() && prefix_text.is_empty() {
            return (text, styled);
        }
        let mut composed =
            String::with_capacity(history.text.len() + prefix_text.len() + 2 + text.len());
        if !history.text.is_empty() {
            composed.push_str(&history.text);
            composed.push('\n');
        }
        if !prefix_text.is_empty() {
            composed.push_str(prefix_text);
            composed.push('\n');
        }
        composed.push_str(&text);
        let history_lines = self.screen_history_lines();
        let prefix_lines = self.screen_prefix_lines();
        let mut styled = styled;
        for line in &mut styled.lines {
            line.line = line
                .line
                .saturating_add((history_lines + prefix_lines) as u32);
        }
        let mut lines = history
            .styled
            .as_ref()
            .map_or_else(Vec::new, |history| history.lines.clone());
        if let Some(prefix_styled) = self.block_tracker.screen_prefix_styled() {
            let mut shifted = prefix_styled.lines.clone();
            for line in &mut shifted {
                line.line = line.line.saturating_add(history_lines as u32);
            }
            lines.extend(shifted);
        }
        lines.extend(styled.lines);
        (composed, StyledOutput { lines })
    }

    /// v1.10.26 (FIX_IME_PREEDIT): map the cursor's row WITHIN the live
    /// viewport segment to an absolute line in the CURRENT in-flight screen
    /// block, derived from the composed text structure itself.
    ///
    /// [`Self::compose_screen_history`] appends the viewport segment LAST, so
    /// it occupies the final `segment_len` bytes of the in-flight block's
    /// text; the count of `\n` before that suffix is exactly the head offset
    /// (preserved-frame history + scroll-out prefix + separators). Deriving
    /// from the published text (instead of re-adding
    /// `screen_history_lines() + screen_prefix_lines()`) makes the anchor
    /// same-frame, same-source — a 1MiB split consuming the head, or the
    /// prefix growing between rate-limited publishes, cannot leave it out of
    /// sync with the rows the BlockView paints (`tui_caret_row_matches` then
    /// never matches and the caret + IME preedit silently vanish).
    pub(super) fn composed_cursor_snapshot_line(
        &self,
        cursor_line: Option<usize>,
        segment_len: usize,
    ) -> Option<usize> {
        let cursor_line = cursor_line?;
        let output = self.block_tracker.in_flight()?.output;
        // The segment is an intact str tail in the normal replace/split
        // paths, so `head_len` is a char boundary; a degraded >1MiB segment
        // split truncates inside the segment (`output.len() < segment_len`),
        // the saturating head becomes 0, and the raw cursor row still anchors
        // the caret as well as the pre-split heuristic could.
        let head_len = output.len().saturating_sub(segment_len);
        // v1.10.26 review B1 guard: every regular publish pairs the output
        // with its segment length, but degraded paths (interrupt window,
        // >1MiB truncation) can leave the pair briefly describing different
        // strings — back off to the nearest char boundary instead of
        // panicking on a mid-character slice.
        let mut head_len = head_len;
        while head_len > 0 && !output.is_char_boundary(head_len) {
            head_len -= 1;
        }
        Some(cursor_line + output[..head_len].matches('\n').count())
    }

    /// v1.10.25 (FIX_TUI_HISTORY_INCREMENTAL): incrementally capture rows
    /// pushed out of the viewport by a scroll (LF overflow via
    /// `index_primary_screen`, CSI S via `scroll_grid_rows`) while a
    /// screen-owned TUI owns the document. Owned rows are appended to the
    /// in-flight block's screen prefix BEFORE the scrollback ring can evict
    /// them, so ring eviction is decoupled from TUI history. Must run BEFORE
    /// `transform_primary_screen_rows`: the viewport ownership mask has not
    /// been rotated yet, so its first `pushed` entries describe the pushed
    /// rows. The interrupt-capture window is skipped — that path owns its own
    /// frozen transcript + tail capture.
    pub(super) fn capture_scrolled_out_screen_rows(&mut self, origin_before: u64) {
        let Some(document_start) = self.block_tracker.screen_document_start() else {
            return;
        };
        if self.capabilities.primary_screen_interrupt_capture.is_some() {
            return;
        }
        let pushed = self
            .grid
            .scrollback
            .position()
            .saturating_sub(origin_before) as usize;
        if pushed == 0 {
            return;
        }
        let owned: Vec<bool> = self
            .capabilities
            .primary_screen_ownership
            .viewport
            .as_deref()
            // No mask (sparse repainters) ⇒ all rows owned — same semantics
            // as the snapshot walk with no mask.
            .map_or_else(
                || vec![true; pushed],
                |mask| mask.iter().take(pushed).copied().collect(),
            );
        let scrollback_from = self.grid.scrollback.len().saturating_sub(pushed);
        let url_resolver = |id: u32| -> Option<std::sync::Arc<str>> {
            self.hyperlinks.url(id).map(std::sync::Arc::<str>::from)
        };
        let rows = self
            .grid
            .snapshot_rows_from_scrollback(scrollback_from, url_resolver);
        let (text, styled) = extract_owned_pushed_rows(&rows, &owned);
        if !text.is_empty() {
            self.block_tracker.append_screen_prefix(&text, styled);
        }
        // Observation fallback: the ring evicted rows older than the screen
        // document start — only possible for the pre-capture window (every
        // row pushed during screen ownership is prefix-captured first, so a
        // normal session never evicts an uncovered row).
        let oldest_retained = self
            .grid
            .scrollback
            .position()
            .saturating_sub(self.grid.scrollback.len() as u64);
        if oldest_retained < document_start {
            tracing::debug!(
                document_start,
                oldest_retained,
                "scrollback ring eviction reached rows before the screen document start (pre-capture window)"
            );
        }
    }

    /// v1.10.25 (FIX_TUI_HISTORY_INCREMENTAL): at screen-ownership start,
    /// move the retained owned rows that predate the capture (the TUI's
    /// pre-threshold frames, still in the scrollback) into the screen prefix
    /// and flip their ownership. Without this the composed transcript would
    /// put newer prefix rows BEFORE those older retained rows; with it the
    /// prefix always holds every row older than the viewport snapshot, in
    /// document order.
    pub(super) fn rebase_screen_prefix_at_capture_start(&mut self) {
        let Some(document_start) = self.block_tracker.screen_document_start() else {
            return;
        };
        let scrollback_from = self.grid.scrollback.index_since(document_start);
        let owned: Vec<bool> = self
            .capabilities
            .primary_screen_ownership
            .scrollback
            .iter()
            .skip(scrollback_from)
            .copied()
            .collect();
        if !owned.iter().any(|owned| *owned) {
            return;
        }
        let url_resolver = |id: u32| -> Option<std::sync::Arc<str>> {
            self.hyperlinks.url(id).map(std::sync::Arc::<str>::from)
        };
        let rows = self
            .grid
            .snapshot_rows_from_scrollback(scrollback_from, url_resolver);
        let (text, styled) = extract_owned_pushed_rows(&rows, &owned);
        if !text.is_empty() {
            self.block_tracker.append_screen_prefix(&text, styled);
        }
        // The captured rows now live in the prefix — flip them so the
        // snapshot walk skips them (keeping them owned would duplicate them
        // in the composed block text).
        for entry in self
            .capabilities
            .primary_screen_ownership
            .scrollback
            .iter_mut()
            .skip(scrollback_from)
        {
            *entry = false;
        }
    }

    /// v1.10.25 (FIX_TUI_HISTORY_INCREMENTAL): 1MiB chunking for screen-owned
    /// TUI history. The composed text exceeded `MAX_OUTPUT_BYTES`: settle the
    /// head chunk(s) as finished blocks (contiguous ids, byte-seamless text)
    /// and continue the tail in the in-flight block, so long sessions no
    /// longer truncate at the snapshot budget. The boundary never cuts inside
    /// the live viewport segment (`segment_len` bytes at the composed tail) —
    /// the snapshot is re-captured wholesale on the next refresh, so a
    /// partial segment in a finished block would duplicate the surviving
    /// rows. Degradation branch: when the viewport segment ALONE exceeds
    /// `MAX_OUTPUT_BYTES` (no chunkable history/prefix remains before it),
    /// splitting would have to cut the segment — the split falls back to the
    /// old snapshot truncation (head-keeping, tail styles dropped, see
    /// [`Self::trim_screen_history`]) and warns. Ordinary command-output
    /// capture (the print path) is untouched — its truncation semantics are
    /// unchanged.
    pub(super) fn split_screen_history(
        &mut self,
        composed: String,
        styled: StyledOutput,
        segment_len: usize,
    ) {
        let history_len = self.capabilities.screen_history.text.len();
        // The '\n' separator between the history and the prefix exists only
        // when the history is non-empty (see `compose_screen_history`).
        let history_sep = usize::from(history_len > 0);
        let prefix_len = self.block_tracker.screen_prefix_len();
        let composed_len = composed.len();
        let mut heads: Vec<(String, Option<StyledOutput>)> = Vec::new();
        let mut rest = composed;
        let mut line_at = 0usize;
        while rest.len() > MAX_OUTPUT_BYTES {
            let natural = line_boundary_at_or_before(&rest, MAX_OUTPUT_BYTES);
            let clamp = rest.len().saturating_sub(segment_len);
            if clamp == 0 {
                // The entire remaining text is the live viewport segment
                // (>1MiB). Settling any of it in a finished block would
                // duplicate the settled rows on the next refresh (the
                // segment is re-captured wholesale) — unbounded growth.
                // Fall back to the pre-split snapshot truncation: settle the
                // heads split so far, truncate the segment at the 1MiB
                // budget in the in-flight block, drop tail styles (matching
                // `replace_screen_snapshot`'s truncation semantics).
                let mut end = MAX_OUTPUT_BYTES.min(rest.len());
                while end > 0 && !rest.is_char_boundary(end) {
                    end -= 1;
                }
                let tail = rest[..end].to_string();
                let consumed_bytes = composed_len - rest.len();
                let consumed_h = consumed_bytes.min(history_len);
                let consumed_p = consumed_bytes
                    .saturating_sub(history_len + history_sep)
                    .min(prefix_len);
                self.trim_screen_history(consumed_h);
                let head_count = heads.len();
                self.block_tracker
                    .split_screen_history(heads, tail, None, consumed_p);
                tracing::warn!(
                    heads = head_count,
                    bytes = rest.len(),
                    "screen-owned viewport segment alone exceeds 1MiB; degrading to snapshot truncation (chunking disabled)"
                );
                return;
            }
            let boundary = natural.min(clamp);
            // Complete lines fully owned by the head — a head ending in '\n'
            // covers exactly `matches('\n')` line indices (the trailing '\n'
            // terminates the last line; it does not open a new one). A
            // mid-line cut leaves the partial line's styled index with the
            // tail, where the line completes.
            let head_lines = rest[..boundary].matches('\n').count();
            let head_styled = styled_lines_in(&styled, line_at, head_lines);
            heads.push((rest[..boundary].to_string(), head_styled));
            line_at += head_lines;
            rest = rest[boundary..].to_string();
        }
        let tail_styled = styled_lines_from(&styled, line_at);
        let consumed_bytes = composed_len - rest.len();
        // The heads consume the history part first, then the prefix part
        // (the composed is history + '\n' + prefix + '\n' + segment).
        let consumed_h = consumed_bytes.min(history_len);
        let consumed_p = consumed_bytes
            .saturating_sub(history_len + history_sep)
            .min(prefix_len);
        self.trim_screen_history(consumed_h);
        let head_count = heads.len();
        let tail_bytes = rest.len();
        self.block_tracker
            .split_screen_history(heads, rest, tail_styled, consumed_p);
        tracing::info!(
            heads = head_count,
            tail_bytes,
            "splitting long TUI history block at 1MiB"
        );
    }

    /// Drop the first `consumed` bytes of the preserved-frame history (the
    /// part folded into a finished block by the 1MiB split). The boundary is
    /// a line boundary of the history, so the remaining text is intact; kept
    /// styled lines are re-indexed.
    fn trim_screen_history(&mut self, consumed: usize) {
        if consumed == 0 {
            return;
        }
        let history = &mut self.capabilities.screen_history;
        let consumed = consumed.min(history.text.len());
        if consumed == 0 {
            return;
        }
        let consumed_lines = history.text[..consumed].matches('\n').count()
            + usize::from(consumed == history.text.len());
        // `consumed < len`: the split boundary is a '\n' inside the history
        // (or an overlong-line mid-cut) — every line fully owned by the head
        // ends with '\n' in the consumed region, so `matches('\n')` counts
        // them. `consumed == len`: the whole history was folded into the
        // head, whose boundary sits past the history end — the last line is
        // completed by the head's separator '\n' and must be counted too.
        history.text.drain(..consumed);
        if let Some(styled) = &mut history.styled {
            styled
                .lines
                .retain(|line| (line.line as usize) >= consumed_lines);
            for line in &mut styled.lines {
                line.line = line.line.saturating_sub(consumed_lines as u32);
            }
            if styled.lines.is_empty() {
                history.styled = None;
            }
        }
    }

    /// Freeze the shell/TUI boundary at OSC 133;B, before the launched
    /// program can paint text that happens to equal its command name.
    pub(in crate::vt) fn freeze_primary_screen_document_candidate(&mut self) {
        let viewport_start = (0..self.grid.num_rows)
            .rev()
            .find(|&row| !self.grid.row_text(row).trim().is_empty())
            .map_or(0, |row| row.saturating_add(1));
        self.capabilities.primary_screen_document_candidate = self
            .grid
            .scrollback
            .position()
            .saturating_add(viewport_start as u64);
        self.capabilities.primary_screen_ownership.scrollback =
            vec![false; self.grid.scrollback.len()];
        self.capabilities.primary_screen_ownership.viewport = Some(vec![false; self.grid.num_rows]);
        tracing::debug!(
            viewport_start,
            document_start = self.capabilities.primary_screen_document_candidate,
            "primary-screen document boundary"
        );
    }

    pub(in crate::vt) fn include_primary_screen_viewport_row(&mut self, row: usize) {
        if self.capabilities.alt_active {
            return;
        }
        if let Some(touched) = &mut self.capabilities.primary_screen_ownership.viewport {
            if let Some(owned) = touched.get_mut(row) {
                *owned = true;
            }
        }
        let position = self.grid.scrollback.position().saturating_add(row as u64);
        if self.block_tracker.phase() == crate::blocks::ShellPhase::CommandExecuting {
            self.capabilities.primary_screen_document_candidate = self
                .capabilities
                .primary_screen_document_candidate
                .min(position);
        }
        self.block_tracker
            .include_screen_document_position(position);
    }

    pub(super) fn transform_primary_screen_rows(
        &mut self,
        origin_before: u64,
        origin_after: u64,
        top: usize,
        bottom: usize,
        count: usize,
        down: bool,
    ) {
        if self.capabilities.alt_active {
            return;
        }
        let transform = |start| {
            transform_document_start(start, origin_before, origin_after, top, bottom, count, down)
        };
        if self.block_tracker.phase() == crate::blocks::ShellPhase::CommandExecuting {
            self.capabilities.primary_screen_document_candidate =
                transform(self.capabilities.primary_screen_document_candidate);
        }
        if let Some(start) = self.block_tracker.screen_document_start() {
            self.block_tracker
                .set_screen_document_start(transform(start));
        }
        if let Some(owned) = &mut self.capabilities.primary_screen_ownership.viewport {
            let pushed = origin_after.saturating_sub(origin_before) as usize;
            if !down && top == 0 && pushed > 0 {
                self.capabilities
                    .primary_screen_ownership
                    .scrollback
                    .extend(owned.iter().take(pushed.min(owned.len())).copied());
                if self.capabilities.primary_screen_ownership.scrollback.len()
                    > self.grid.scrollback.len()
                {
                    let expired = self
                        .capabilities
                        .primary_screen_ownership
                        .scrollback
                        .len()
                        .saturating_sub(self.grid.scrollback.len());
                    self.capabilities
                        .primary_screen_ownership
                        .scrollback
                        .drain(..expired);
                }
                while self.capabilities.primary_screen_ownership.scrollback.len()
                    < self.grid.scrollback.len()
                {
                    self.capabilities
                        .primary_screen_ownership
                        .scrollback
                        .insert(0, false);
                }
                // v1.10.25 (FIX_TUI_HISTORY_INCREMENTAL): rows pushed out
                // during a screen-owned session were captured into the
                // in-flight block's screen prefix by
                // `capture_scrolled_out_screen_rows`; flip their ownership so
                // the snapshot walk skips them (their text lives in the
                // prefix — keeping them owned would duplicate them in the
                // composed block text). Same gates as the capture.
                if self.block_tracker.screen_document_start().is_some()
                    && self.capabilities.primary_screen_interrupt_capture.is_none()
                {
                    let mask = &mut self.capabilities.primary_screen_ownership.scrollback;
                    let from = mask.len().saturating_sub(pushed);
                    for entry in &mut mask[from..] {
                        *entry = false;
                    }
                }
            }
            transform_viewport_ownership(owned, top, bottom, count, down);
        }
    }

    pub(in crate::vt) fn clear_primary_screen_scrollback(&mut self) {
        self.grid.clear_scrollback();
        self.capabilities
            .primary_screen_ownership
            .scrollback
            .clear();
    }
}

fn transform_viewport_ownership(
    owned: &mut [bool],
    top: usize,
    bottom: usize,
    count: usize,
    down: bool,
) {
    let Some(region) = owned.get_mut(top..=bottom) else {
        return;
    };
    let count = count.min(region.len());
    if count == 0 {
        return;
    }
    if down {
        region.rotate_right(count);
        region[..count].fill(false);
    } else {
        region.rotate_left(count);
        let clear_from = region.len() - count;
        region[clear_from..].fill(false);
    }
}

fn transform_document_start(
    start: u64,
    origin_before: u64,
    origin_after: u64,
    top: usize,
    bottom: usize,
    count: usize,
    down: bool,
) -> u64 {
    if start < origin_before || top > bottom {
        return start;
    }
    let row = start.saturating_sub(origin_before) as usize;
    let count = count.min(bottom - top + 1);
    if down {
        if (top..=bottom).contains(&row) {
            origin_after.saturating_add(row.saturating_add(count).min(bottom + 1) as u64)
        } else {
            start.saturating_add(origin_after.saturating_sub(origin_before))
        }
    } else if top == 0 {
        if row <= bottom + 1 {
            start
        } else {
            start.saturating_add(origin_after.saturating_sub(origin_before))
        }
    } else if (top + 1..=bottom).contains(&row) {
        origin_after.saturating_add(row.saturating_sub(count).max(top) as u64)
    } else {
        start.saturating_add(origin_after.saturating_sub(origin_before))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absolute_document_start_tracks_viewport_row_rotations() {
        assert_eq!(transform_document_start(3, 0, 1, 0, 5, 1, false), 3);
        assert_eq!(transform_document_start(3, 0, 0, 0, 5, 1, true), 4);
        assert_eq!(transform_document_start(3, 0, 0, 1, 5, 1, false), 2);
        assert_eq!(transform_document_start(3, 0, 0, 1, 5, 1, true), 4);
    }

    #[test]
    fn viewport_ownership_follows_scrolls_and_insert_delete_lines() {
        let mut owned = vec![true, false, true, false, true];
        transform_viewport_ownership(&mut owned, 0, 4, 1, false);
        assert_eq!(owned, [false, true, false, true, false]);

        transform_viewport_ownership(&mut owned, 1, 4, 2, true);
        assert_eq!(owned, [false, false, false, true, false]);

        transform_viewport_ownership(&mut owned, 1, 4, 1, false);
        assert_eq!(owned, [false, false, true, false, false]);
    }
}
