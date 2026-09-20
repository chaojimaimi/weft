//! Primary-screen snapshot pipeline — viewport-row mapping into the composed
//! document, rate-limited refresh, and publish/split.

use super::tail::{merge_primary_screen_interrupt_tail, space_primary_screen_exit_tail};
use super::Terminal;
use super::PRIMARY_HISTORY_SNAPSHOT_INTERVAL;
use crate::blocks::{StyledOutput, MAX_OUTPUT_BYTES};
use std::time::Instant;

impl Terminal {
    /// v1.10.20: snapshot line index of a live viewport row, computed with
    /// the exact same walk parameters as [`Self::primary_screen_document_snapshot`]
    /// (same document start and ownership masks). `None` when no screen
    /// document is captured or the row is skipped by the snapshot (unowned /
    /// empty). Used by the drag-selection anchor migration — the mapping
    /// must match the snapshot the history BlockView renders, and the
    /// empty-row skip breaks any 1:1 row arithmetic.
    pub fn primary_screen_snapshot_line_for_viewport_row(
        &self,
        viewport_row: usize,
    ) -> Option<usize> {
        let document_start = self.block_tracker.screen_document_start()?;
        let viewport_origin = self.grid.scrollback.position();
        let (scrollback_start, viewport_start) = if document_start <= viewport_origin {
            (self.grid.scrollback.index_since(document_start), 0)
        } else {
            (
                self.grid.scrollback.len(),
                document_start.saturating_sub(viewport_origin) as usize,
            )
        };
        let line = match self
            .capabilities
            .primary_screen_ownership
            .viewport
            .as_deref()
        {
            Some(owned) => self.grid.snapshot_line_index_for_viewport_row(
                viewport_row,
                scrollback_start,
                viewport_start,
                Some(&self.capabilities.primary_screen_ownership.scrollback),
                Some(owned),
            ),
            None => self.grid.snapshot_line_index_for_viewport_row(
                viewport_row,
                scrollback_start,
                viewport_start,
                None,
                None,
            ),
        };
        // v1.10.23: the rendered block prepends the preserved-frame history,
        // so the anchor's rendered line shifts by that many lines.
        // v1.10.25: the scroll-out prefix shifts it too (three-part compose).
        line.map(|line| line + self.screen_history_lines() + self.screen_prefix_lines())
    }

    /// v1.10.12: alt-screen history peek — true while the user is browsing the
    /// terminal's history BlockView over an alt-screen TUI (omp/less/man).
    pub fn is_alt_screen_history_peek(&self) -> bool {
        self.capabilities.alt_screen_history_peek
    }

    /// v1.10.12: enter/exit the alt-screen history peek. Entering makes
    /// `show_block_view()` true (BlockView overlays the TUI); exiting restores
    /// the live alt grid. Does NOT touch `grid.scroll_offset` or primary-screen
    /// snapshots — block positioning uses the pane's `block_scroll_anchor`.
    pub fn set_alt_screen_history_peek(&mut self, on: bool) {
        self.capabilities.alt_screen_history_peek = on;
    }

    /// v1.10.6: the cursor's line index in the most recent primary-screen
    /// snapshot. `None` until the first snapshot, or when the cursor sat on
    /// a row the snapshot omitted (unowned, leading, or trailing empty).
    pub fn primary_screen_cursor_snapshot_line(&self) -> Option<usize> {
        self.capabilities.primary_screen_cursor_snapshot_line
    }

    /// v1.10.6: refresh just the cursor's snapshot line, without the
    /// rate-limit or `replace_screen_snapshot` side effects. Called on
    /// every keystroke so the caret/preedit have a precise row even when
    /// no PTY output has arrived yet (IME preedit, idle TUI).
    /// v1.10.7 (reviewer MEDIUM): skip the full document rebuild when the
    /// cursor position is unchanged since the last caret refresh — the
    /// tracked line only depends on the cursor's row, and this call has no
    /// rate limit.
    pub fn snapshot_primary_screen_output_for_caret(&mut self) {
        if self.block_tracker.screen_document_start().is_none() {
            return;
        }
        let cursor = (self.grid.cursor.row, self.grid.cursor.col);
        if self.capabilities.last_caret_snapshot_cursor == Some(cursor) {
            return;
        }
        self.capabilities.last_caret_snapshot_cursor = Some(cursor);
        let document_start = self.block_tracker.screen_document_start().unwrap_or(0);
        let (_, _, cursor_line) = self.primary_screen_document_snapshot(document_start);
        // v1.10.26 (FIX_IME_PREEDIT): re-anchor from the published composed
        // text (`freeze::composed_cursor_snapshot_line`) — the prefix grows
        // between 50ms publishes, so counted offsets drift past painted rows.
        let segment_len = self
            .capabilities
            .primary_screen_cursor_segment_len
            .unwrap_or(0);
        self.capabilities.primary_screen_cursor_snapshot_line =
            self.composed_cursor_snapshot_line(cursor_line, segment_len);
    }

    pub fn set_primary_history_view(&mut self, active: bool) {
        let entering = active && !self.capabilities.primary_history_view;
        self.capabilities.primary_history_view = active;
        if active {
            self.grid.set_scroll_offset(0);
        } else {
            self.capabilities.primary_history_snapshot_at = None;
        }
        if entering && self.primary_screen_app_active() {
            self.snapshot_primary_screen_output();
            self.capabilities.primary_history_snapshot_at = Some(Instant::now());
            tracing::debug!(
                bytes = self
                    .block_tracker
                    .in_flight()
                    .map_or(0, |live| live.output.len()),
                "snapshotted primary-screen TUI for history browsing"
            );
        }
    }

    /// Coalesced by the app after it drains the current frame's PTY batches,
    /// then rate-limited here so a high-frequency TUI cannot rescan the capped
    /// document on every display frame.
    pub fn refresh_primary_history_snapshot(&mut self) -> bool {
        self.refresh_primary_history_snapshot_at(Instant::now())
    }

    /// v1.10.4: immediate snapshot refresh for keypress-driven redraws.
    ///
    /// Keystrokes are low-frequency (tens of ms apart at most) compared to
    /// the display-frame rate limit, and they drive the TUI's repaint — a
    /// selection change must show up on the next frame. Waiting out the 50ms
    /// window makes the browsing view lag a blink behind, which reads as a
    /// flicker: frame N shows the old selection, frame N+1 the new one.
    ///
    /// v1.10.4 (round 4): gate on screen-ownership instead of
    /// `primary_history_view`. Once a primary-screen TUI is screen-owned
    /// (`screen_document_start` set), `is_capturing()` returns false and the
    /// live block is ONLY updated through this snapshot — so a relative-only
    /// TUI (openclaw) kept in the BlockView needs the refresh even while
    /// following the live tail (history browsing off).
    ///
    /// v1.10.7: the v1.10.4 MEDIUM-1 absolute-addressing skip is REMOVED —
    /// the snapshot is the ONLY content source for screen-owned blocks, so
    /// it must refresh regardless of the transient addressing mode. The rate
    /// limit still bounds the rescan cost. (v1.11.8 M-C1: the
    /// `primary_screen_absolute_addressing` flag itself is gone — zero
    /// readers since v1.10.12; this doc keeps the skip's rationale record.)
    pub fn refresh_primary_history_snapshot_now(&mut self) -> bool {
        if self.block_tracker.screen_document_start().is_none() {
            return false;
        }
        self.snapshot_primary_screen_output();
        self.capabilities.primary_history_snapshot_at = Some(Instant::now());
        true
    }

    pub(in crate::vt) fn refresh_primary_history_snapshot_at(&mut self, now: Instant) -> bool {
        if self.block_tracker.screen_document_start().is_none() {
            return false;
        }
        if self
            .capabilities
            .primary_history_snapshot_at
            .is_some_and(|previous| {
                now.saturating_duration_since(previous) < PRIMARY_HISTORY_SNAPSHOT_INTERVAL
            })
        {
            return false;
        }
        self.snapshot_primary_screen_output();
        self.capabilities.primary_history_snapshot_at = Some(now);
        true
    }

    pub(in crate::vt) fn snapshot_primary_screen_output(&mut self) {
        if let Some(capture) = &self.capabilities.primary_screen_interrupt_capture {
            let (text, styled) = merge_primary_screen_interrupt_tail(
                capture.frozen_text.clone(),
                capture.frozen_styled.clone(),
                &capture.tail,
            );
            let segment_len = text.len();
            let (text, styled) = self.compose_screen_history(text, styled);
            self.publish_screen_snapshot(text, styled, segment_len);
            // v1.10.26 review B1: the merged text replaced the in-flight
            // output — the stored segment length must describe THIS string,
            // or a mid-window keypress slices at a stale offset (a
            // non-char-boundary panic under CJK). The raw grid cursor row
            // anchors the caret here; the interrupt window is transient.
            self.capabilities.primary_screen_cursor_segment_len = Some(segment_len);
            return;
        }
        let Some(document_start) = self.block_tracker.screen_document_start() else {
            return;
        };
        // v1.11.12 (PLAN_v11112 M-A): per-refresh phase timings (document walk
        // / space-tail / compose / publish). The four Instant reads are
        // negligible; the SNAPSHOT_PHASES line itself (including the O(n)
        // line count) is only materialized when DEBUG is enabled, so
        // production logs stay silent and `--nocapture`/RUST_LOG=debug can
        // still attribute refresh cost. The aggregate feeds the .13
        // representation-layer decision (frozen head/tail split).
        let walk_start = Instant::now();
        let (text, styled, cursor_line) = self.primary_screen_document_snapshot(document_start);
        let after_walk = Instant::now();
        let (text, styled) = space_primary_screen_exit_tail(text, styled);
        let after_tail = Instant::now();
        // v1.10.23 (FIX_OMP_CONTENT_LOSS): prepend the preserved superseded
        // frames so the block transcript stays complete across full-frame
        // repaints; the cursor line shifts by the prepended history.
        // v1.10.25 (FIX_TUI_HISTORY_INCREMENTAL): the scroll-out prefix joins
        // between the frames and the snapshot.
        let segment_len = text.len();
        let (text, styled) = self.compose_screen_history(text, styled);
        let after_compose = Instant::now();
        let composed_lines = tracing::enabled!(tracing::Level::DEBUG).then(|| text.lines().count());
        self.publish_screen_snapshot(text, styled, segment_len);
        let after_publish = Instant::now();
        if let Some(lines) = composed_lines {
            let us = |from: Instant, to: Instant| (to - from).as_micros() as u64;
            tracing::debug!(
                "SNAPSHOT_PHASES walk={} tail={} compose={} publish={} total={} lines={}",
                us(walk_start, after_walk),
                us(after_walk, after_tail),
                us(after_tail, after_compose),
                us(after_compose, after_publish),
                us(walk_start, after_publish),
                lines,
            );
        }
        // v1.10.6/25/26 (FIX_IME_PREEDIT): store the caret snapshot line
        // AFTER publish from the composed text actually written
        // (`freeze::composed_cursor_snapshot_line`) — pre-split offsets or
        // recomputed head counts drift past the painted rows on splits.
        self.capabilities.primary_screen_cursor_snapshot_line =
            self.composed_cursor_snapshot_line(cursor_line, segment_len);
        self.capabilities.primary_screen_cursor_segment_len = Some(segment_len);
    }

    /// v1.10.25 (FIX_TUI_HISTORY_INCREMENTAL): publish a composed screen
    /// snapshot to the in-flight block — or split it into 1MiB finished
    /// blocks plus an in-flight tail when the session history crossed
    /// `MAX_OUTPUT_BYTES` (long TUI sessions no longer truncate at the
    /// snapshot budget).
    fn publish_screen_snapshot(&mut self, text: String, styled: StyledOutput, segment_len: usize) {
        if text.len() > MAX_OUTPUT_BYTES {
            self.split_screen_history(text, styled, segment_len);
        } else {
            self.block_tracker.replace_screen_snapshot(&text, styled);
        }
    }
}
