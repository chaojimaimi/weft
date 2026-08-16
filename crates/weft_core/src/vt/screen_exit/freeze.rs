//! Document-boundary freeze and scroll-driven ownership transforms for the
//! primary screen.

use super::Terminal;
use crate::blocks::StyledOutput;

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

    /// v1.10.23 (FIX_OMP_CONTENT_LOSS): prepend the accumulated superseded
    /// frames to a fresh document snapshot. The history is the stable
    /// transcript head; the snapshot is the live tail. Zero-cost (text passed
    /// through unchanged) when no frames were preserved.
    pub(super) fn compose_screen_history(
        &self,
        text: String,
        styled: StyledOutput,
    ) -> (String, StyledOutput) {
        let history = &self.capabilities.screen_history;
        if history.text.is_empty() {
            return (text, styled);
        }
        let mut composed = String::with_capacity(history.text.len() + 1 + text.len());
        composed.push_str(&history.text);
        composed.push('\n');
        composed.push_str(&text);
        let offset = self.screen_history_lines() as u32;
        let mut styled = styled;
        for line in &mut styled.lines {
            line.line = line.line.saturating_add(offset);
        }
        let mut lines = history
            .styled
            .as_ref()
            .map_or_else(Vec::new, |history| history.lines.clone());
        lines.extend(styled.lines);
        (composed, StyledOutput { lines })
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
