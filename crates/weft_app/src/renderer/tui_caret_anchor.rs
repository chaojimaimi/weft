//! BlockView-mode TUI caret anchor for `MetalRenderer`, moved verbatim out
//! of `renderer.rs` (v1.13.8 S3 zero-behavior file-budget split; `impl
//! MetalRenderer` cross-file block per the renderer/draw_phases.rs
//! precedent). Sole caller: draw_phases.rs's grid-phase `tui_cursor`
//! (T16b formula-fallback seam).

use super::MetalRenderer;
use weft_core::vt::Terminal;

impl MetalRenderer {
    /// v1.10.5/6: BlockView-mode TUI caret anchor — the grid cursor mapped
    /// into the live block's snapshot text. v1.10.6 prefers the precisely
    /// tracked snapshot line over the formula (which breaks when the
    /// snapshot skips empty rows).
    pub(super) fn block_view_tui_cursor(
        &self,
        terminal: &Terminal,
        grid: &weft_core::grid::Grid,
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
                let col = crate::paint::tui_caret::tui_cursor_tail_display_col(
                    live.output,
                    grid.num_cols,
                );
                (line, col)
            }
        };
        Some((line, col))
    }
}
