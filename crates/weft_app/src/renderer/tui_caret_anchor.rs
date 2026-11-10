//! BlockView-mode TUI caret anchor for `MetalRenderer`, moved verbatim out
//! of `renderer.rs` (v1.13.8 S3 zero-behavior file-budget split; `impl
//! MetalRenderer` cross-file block per the renderer/draw_phases.rs
//! precedent). Sole caller: draw_phases.rs's grid-phase `tui_cursor`
//! (T16b formula-fallback seam).

use super::MetalRenderer;
use crate::paint::region_cache::{document_index_of_cursor, PaneRegionState};
use weft_core::vt::Terminal;

impl MetalRenderer {
    /// v1.10.5/6: BlockView-mode TUI caret anchor — the grid cursor mapped
    /// into the live block's snapshot text. v1.10.6 prefers the precisely
    /// tracked snapshot line over the formula (which breaks when the
    /// snapshot skips empty rows).
    ///
    /// v1.19.0 (PLAN_v1190 §4 P1-3/D6): `region_state` is this frame's
    /// region-track snapshot (the paint path syncs it before the model is
    /// built). When the flag is ON and a `133;B` anchor is sampled, the
    /// caret anchors in the region track's logical-row domain; the capture-
    /// domain anchors below are only authoritative for the flag-OFF path.
    pub(super) fn block_view_tui_cursor(
        &self,
        terminal: &Terminal,
        grid: &weft_core::grid::Grid,
        region_state: Option<&PaneRegionState>,
    ) -> Option<(usize, usize)> {
        let live = terminal.block_tracker().in_flight()?;
        if !terminal.show_block_view() {
            return None;
        }
        // v1.10.12: during an alt-screen history peek the active grid is the
        // TUI's alt grid, whose cursor is meaningless over the history BlockView
        // overlay. Suppress the caret (and the preedit caret area it seeds) so
        // nothing is drawn on top of the browsed history.
        if terminal.is_alt_screen_active() {
            return None;
        }
        // v1.19.0 (PLAN_v1190 §4 P1-3/D6): the region track lays out logical
        // rows, so the caret maps into that domain — the cursor's document
        // index (`scrollback.len() + cursor.row`, scrollback-oldest domain)
        // locates its owning RegionSlot via physical-row bounds, and the
        // slot's P1-6 display ordinal becomes the anchor line. Columns reuse
        // the capture path's wrap-pending clamp (B1') against the logical
        // row's own width.
        let begin = terminal.block_tracker().region_begin_position();
        if terminal.is_live_block_grid_render() {
            if let Some(state) = region_state {
                if begin.is_some() {
                    let cursor_doc_idx = document_index_of_cursor(grid, grid.cursor.row);
                    let row = state.display_logical_row_of_cursor(cursor_doc_idx)?;
                    let width = state.logical_row_width(row);
                    let col = crate::paint::tui_caret::wrap_pending_clamp(
                        grid.cursor.col,
                        grid.num_cols,
                        width,
                    );
                    return Some((row, col));
                }
            }
            if begin.is_some() {
                // Anchor sampled but no snapshot to match against (sync not
                // seen this frame): the capture-track formula anchors in a
                // different line domain — suppress instead of guessing.
                return None;
            }
        }
        let tracked = terminal.primary_screen_cursor_snapshot_line();
        let (line, col) = match tracked {
            // v1.10.6: the precisely tracked snapshot line beats the formula.
            Some(line) => (line, grid.cursor.col),
            None => {
                // T16b: grid cursor is untrustworthy on this path — redraw-style
                // progress lines (brew DownloadQueue, PTY-captured bytes) end every
                // tick with CHA0/CPL, parking the grid cursor at col 0 / the previous
                // line's start (0G×12, 1F×13, C×0 in one capture), and the formula
                // line map breaks on wrapped visual rows. The capture's rewrite
                // compaction makes its tail == the newest frame's end == the caret
                // position for every non-TUI command (echo-input preedit anchors at
                // the echo write head — correct too).
                // v1.13.5 T16a: the line index comes from the tracker's O(1)
                // newline ledger and the column scans ONLY the tail line — the
                // old `lines().count()` + `lines().nth()` pair cost two full
                // O(capture) passes per frame (seq bench top hotspot:
                // 2.1ms/frame at the 1 MiB capture cap).
                let line = terminal.block_tracker().live_cursor_tail_line();
                // v1.18.9: prefer the capture's OWN cursor when it sits on
                // the tail line — the nested-shell readline case (`sudo su`):
                // bash moves its caret with CUB/ICH/DCH, mirrored into the
                // capture (v1.18.7/v1.18.8), so the caret can finally follow
                // the real edit head instead of the pinned tail end. None
                // (cursor moved off the tail line for a progress repaint, or
                // phantom trailing-newline row) keeps the formula anchor;
                // wrap-pending exact-multiple ends clamp one cell (B1').
                let text = live.output;
                let tail_start = text.rfind('\n').map_or(0, |i| i + 1);
                let line_width = weft_core::grid::terminal_text_width(&text[tail_start..]);
                let col = terminal
                    .block_tracker()
                    .live_cursor_capture_anchor_col()
                    .map(|col| {
                        crate::paint::tui_caret::wrap_pending_clamp(col, grid.num_cols, line_width)
                    })
                    .unwrap_or_else(|| {
                        crate::paint::tui_caret::tui_cursor_tail_display_col(
                            live.output,
                            grid.num_cols,
                        )
                    });
                (line, col)
            }
        };
        Some((line, col))
    }
}
