//! v1.10.20: grid → primary-history BlockView selection migration.
//!
//! A primary-screen TUI (omp) drag starts as a viewport-relative `GridPos`
//! selection in grid mode. When the drag crosses the top edge the anchor
//! must become a `BlockViewSelection` in the primary history snapshot view —
//! otherwise the grid selection is viewport-relative and would silently
//! drift (L2) or get orphaned by the view switch (L3). This module maps the
//! anchor across the two coordinate spaces; all mouse wiring lives in
//! `mouse_controller` (single entry point: `migrate_grid_selection_to_primary_history`).

use super::*;
use weft_core::grid::{CellFlags, Row};
use weft_core::selection::{BlockViewPos, BlockViewRow, BlockViewRowKind, SelectionMode};

/// Direction of drag-selection autoscroll relative to the content edge.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum AutoscrollDir {
    Up,
    Down,
}

/// Pure logic: rows to scroll per autoscroll tick from the pixel overshoot
/// beyond the content edge. Clamped to [1, 6] so a deep overshoot can't jump
/// past the drag target and a sub-row overshoot still advances (floor would
/// freeze on tiny overshoots).
pub(crate) fn autoscroll_steps(overshoot_px: f32, cell_h: f32) -> usize {
    ((overshoot_px / cell_h).ceil() as usize).clamp(1, 6)
}

/// v1.10.25 Batch 3 (FIX_SELECTION_AND_RESIZE_REMAINING): whether the Down
/// autoscroll trigger band is physically reachable with the pointer. True
/// when at least a full line of space sits below `content_bottom` inside the
/// window — the band `content_bottom + line_h` exists on screen (a prompt /
/// bottom-chrome case). False when the content bottom is at/within one line
/// of the window bottom (TUI history snapshot view: padding 0, no prompt
/// chrome), so the trigger must shift inward to the content's bottom edge.
pub(crate) fn down_trigger_reachable(content_bottom: f32, window_bottom: f32, line_h: f32) -> bool {
    line_h > 0.0 && window_bottom - content_bottom >= line_h
}

/// The y threshold at which a held drag triggers a Down autoscroll. With
/// bottom chrome the band sits a full line below the content edge
/// (`content_bottom + line_h`); without it, the content's bottom edge row
/// itself is the band (`content_bottom - line_h`), so a drag held inside the
/// last visible line can still scroll the selection downward.
pub(crate) fn down_autoscroll_threshold(
    content_bottom: f32,
    window_bottom: f32,
    line_h: f32,
) -> f32 {
    if down_trigger_reachable(content_bottom, window_bottom, line_h) {
        content_bottom + line_h
    } else {
        content_bottom - line_h
    }
}

/// Grid column → char index into the row's snapshot text. Mirrors the
/// snapshot builder's char counting exactly: `WIDE_SPACER` cells are
/// skipped, each `EXTRA` cluster contributes its full char count (a ZWJ
/// emoji is one cell but several chars), and trailing blank cells are not
/// part of the text — so the anchor char stays aligned with the rendered
/// text the BlockView row displays.
pub fn grid_col_to_source_char_index(row: &Row, col: usize) -> usize {
    let last = row
        .cells
        .iter()
        .rposition(|cell| cell.character != ' ' && cell.character != '\0')
        .map(|index| index + 1)
        .unwrap_or(0);
    let end = col.min(last);
    let mut index = 0usize;
    for (cell_col, cell) in row.cells[..end].iter().enumerate() {
        if cell.flags.contains(CellFlags::WIDE_SPACER) {
            continue;
        }
        if cell.flags.contains(CellFlags::EXTRA) {
            index = index.saturating_add(
                row.extras
                    .grapheme_at(cell_col)
                    .map(str::chars)
                    .map(Iterator::count)
                    .unwrap_or(1),
            );
        } else {
            index += 1;
        }
    }
    index
}

/// Find the block-view position for a snapshot source line. `source_char` is
/// the char offset within the full source line; wrapped rows split the line
/// across multiple chunks, so the chunk containing `source_char` is chosen
/// (past the end clamps to the last chunk). Only live-block output rows
/// (no `block_id`) are considered — the primary history snapshot renders as
/// the in-flight block, and finalized blocks carry their own per-block line
/// indices that could collide.
///
/// `None` when the line is not present in the row snapshot (migration must
/// degrade: keep the grid selection).
pub fn block_view_pos_for_snapshot_line(
    rows: &[BlockViewRow],
    snapshot_line: usize,
    source_char: usize,
) -> Option<BlockViewPos> {
    let mut best: Option<(usize, usize)> = None; // (row_index, chunk_char_offset)
    for (index, row) in rows.iter().enumerate() {
        if row.kind != BlockViewRowKind::Output
            || row.block_id.is_some()
            || row.line != Some(snapshot_line)
        {
            continue;
        }
        // Chunks iterate bottom-to-top (offsets descending): the anchor
        // belongs to the chunk with the LARGEST offset that is still <=
        // `source_char`; chunks starting after it don't contain the char.
        if row.chunk_char_offset > source_char {
            continue;
        }
        if best.map_or(true, |(_, offset)| row.chunk_char_offset >= offset) {
            best = Some((index, row.chunk_char_offset));
        }
    }
    let (row_index, chunk_offset) = best?;
    let text_len = rows[row_index].text.chars().count();
    let char_index = source_char.saturating_sub(chunk_offset).min(text_len);
    Some(BlockViewPos {
        row_index,
        char_index,
    })
}

impl App {
    /// Vertical physical-pixel bounds of the block view's visible content
    /// area: the layout clip band (top of the clip region to the bottom of
    /// the clip region). `None` only when the block view isn't active or the
    /// renderer/terminal is unavailable; an empty history still yields the
    /// clip band (max_scroll=0 makes any autoscroll a no-op in that case).
    pub(super) fn block_content_vbounds(&self) -> Option<(f32, f32)> {
        // 可见 block 内容区 = 布局 clip。用 bv_rows first/last 会带 overscan
        // (最顶行可到 clip_top - overscan),把向上滚动的触发阈值抬到窗口外。
        self.compute_block_view_rows()
            .map(|(_, clip_top, clip_bottom)| (clip_top, clip_bottom))
    }

    /// Vertical physical-pixel bounds of the grid content area (the visible
    /// terminal band from the shared layout). Used by the grid-mode
    /// drag-autoscroll edge detection (primary-screen TUIs); mirrors
    /// `block_content_vbounds`.
    pub(super) fn grid_content_vbounds(&self) -> Option<(f32, f32)> {
        self.terminal_layout()
            .map(|layout| (layout.content.top as f32, layout.content.bottom as f32))
    }

    /// Single entry point that turns an active grid selection into a
    /// block-view selection anchored at the same source row, entering the
    /// primary history snapshot view in the process. Returns `true` on
    /// success; on failure the view mode and grid selection are left
    /// untouched (degrade — never switch views and orphan the selection).
    ///
    /// v1.10.20 (S2): refuses to migrate while the Ctrl-C interrupt capture
    /// window is active — the snapshot rewrites the transcript then, so the
    /// live-grid line mapping would not match the rendered rows and the
    /// anchor could land on the wrong snapshot line (shift). The caller
    /// keeps the grid selection and retries on a later sync.
    pub(super) fn migrate_grid_selection_to_primary_history(&mut self) -> bool {
        // Step 1 (read-only): the grid selection anchor + its snapshot line,
        // computed against the CURRENT grid. Entering the history view
        // snapshots but never mutates the grid, so the mapping stays exact.
        let (anchor, snapshot_line) = {
            let tab = self.sessions.active_mut();
            let Some(sel) = tab.selection_handler.selection.as_ref().cloned() else {
                return false;
            };
            // Rectangular (Shift+drag) selections have no block-view
            // equivalent — keep the grid selection as-is.
            if sel.mode != SelectionMode::Simple {
                return false;
            }
            let Some(terminal) = tab.terminal.as_ref() else {
                return false;
            };
            if !terminal.primary_screen_app_active() {
                return false;
            }
            // v1.10.20 (S2): during the interrupt capture window the
            // snapshot replaces the transcript (`space_primary_screen_exit_tail`),
            // so the viewport-relative mapping below would not match the
            // rendered rows — degrade instead of risking a shifted anchor.
            if terminal.primary_screen_interrupt_capture_active() {
                tracing::warn!(
                    grid_row = sel.start.row,
                    "interrupt capture window active; grid selection migration deferred"
                );
                return false;
            }
            let Some(line) = terminal.primary_screen_snapshot_line_for_viewport_row(sel.start.row)
            else {
                return false;
            };
            (sel, line)
        };
        // Step 2 (read-only): the anchor grid row's cells → source char
        // offset within its snapshot line.
        let source_char = {
            let Some(terminal) = self.sessions.active().terminal.as_ref() else {
                return false;
            };
            let Some(row) = terminal.grid().viewport.get(anchor.start.row) else {
                return false;
            };
            grid_col_to_source_char_index(row, anchor.start.col)
        };
        // Step 3: switch into the history view (snapshots synchronously).
        let was_in_history = self
            .sessions
            .active()
            .terminal
            .as_ref()
            .is_some_and(Terminal::primary_history_view);
        if !self.sessions.active_mut().enter_primary_history_if_active() {
            return false;
        }
        let restore_view = |app: &mut Self| {
            if !was_in_history {
                if let Some(terminal) = app.sessions.active_mut().terminal.as_mut() {
                    terminal.set_primary_history_view(false);
                }
            }
        };
        // Step 4: find the block-view row for the anchor (needs the switched
        // view — the history rows only exist while it renders).
        let Some((rows_snapshot, _, _)) = self.compute_block_view_rows() else {
            restore_view(self);
            return false;
        };
        let Some(anchor_pos) =
            block_view_pos_for_snapshot_line(&rows_snapshot, snapshot_line, source_char)
        else {
            restore_view(self);
            return false;
        };
        // Step 5: rebuild the selection in block space; the drag continues
        // through the block branch (pump / move) from here.
        let tab = self.sessions.active_mut();
        tab.selection_handler
            .start_block_view(anchor_pos, rows_snapshot);
        tab.selection_handler.clear_grid_selection();
        tracing::info!(
            grid_row = anchor.start.row,
            snapshot_line,
            bv_row = anchor_pos.row_index,
            "migrated grid selection into primary history block view"
        );
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use weft_core::blocks::BlockId;
    use weft_core::grid::Row;

    #[test]
    fn autoscroll_steps_zero_or_negative_overshoot_scrolls_one_row() {
        assert_eq!(autoscroll_steps(0.0, 20.0), 1);
        assert_eq!(autoscroll_steps(-5.0, 20.0), 1);
    }

    #[test]
    fn autoscroll_steps_around_one_row_boundary() {
        // Exactly one row of overshoot → 1 step; slightly more → 2.
        assert_eq!(autoscroll_steps(20.0, 20.0), 1);
        assert_eq!(autoscroll_steps(20.5, 20.0), 2);
        assert_eq!(autoscroll_steps(1.0, 20.0), 1); // sub-row still advances
    }

    #[test]
    fn autoscroll_steps_deep_overshoot_clamps_at_six() {
        assert_eq!(autoscroll_steps(2000.0, 20.0), 6);
        assert_eq!(autoscroll_steps(1_000_000.0, 1.0), 6);
    }

    #[test]
    fn autoscroll_steps_zero_cell_height_saturates_safely() {
        // inf / NaN saturate in the float→usize cast and hit the clamp.
        assert_eq!(autoscroll_steps(40.0, 0.0), 6); // inf → saturate → 6
        assert_eq!(autoscroll_steps(f32::NAN, 0.0), 1); // NaN → 0 → clamp to 1
    }

    // ── v1.10.25 Batch 3 (FIX_SELECTION_AND_RESIZE_REMAINING): Down band
    // reachability with/without bottom chrome ─────────────────────────────

    #[test]
    fn down_trigger_reachable_requires_a_full_line_of_bottom_space() {
        const LINE_H: f32 = 20.0;
        const WINDOW_H: f32 = 800.0;
        // No bottom chrome: content bottom at/within a line of the window
        // bottom — the `bottom + margin` band is physically unreachable.
        assert!(!down_trigger_reachable(WINDOW_H, WINDOW_H, LINE_H));
        assert!(!down_trigger_reachable(WINDOW_H - 10.0, WINDOW_H, LINE_H));
        // Prompt / bottom chrome: the band below the content is on screen.
        assert!(down_trigger_reachable(WINDOW_H - 20.0, WINDOW_H, LINE_H));
        assert!(down_trigger_reachable(WINDOW_H - 80.0, WINDOW_H, LINE_H));
        // Degenerate cell height must not claim reachability.
        assert!(!down_trigger_reachable(100.0, 800.0, 0.0));
    }

    #[test]
    fn down_autoscroll_threshold_two_states() {
        const LINE_H: f32 = 20.0;
        const WINDOW_H: f32 = 800.0;
        // With bottom chrome: band stays a full line below the content edge.
        assert_eq!(down_autoscroll_threshold(700.0, WINDOW_H, LINE_H), 720.0);
        // Without chrome: the threshold shifts inward to the bottom edge row
        // (`content_bottom - line_h`), making Down triggerable from the last
        // visible line of a TUI snapshot that fills the window.
        assert_eq!(down_autoscroll_threshold(WINDOW_H, WINDOW_H, LINE_H), 780.0);
        assert_eq!(
            down_autoscroll_threshold(WINDOW_H - 10.0, WINDOW_H, LINE_H),
            770.0
        );
    }

    fn row_with_text(text: &str, cols: usize) -> Row {
        let mut row = Row::new(cols);
        for (index, ch) in text.chars().enumerate() {
            row.cells[index].character = ch;
        }
        row
    }

    #[test]
    fn grid_col_to_char_index_counts_non_spacer_cells() {
        let row = row_with_text("hello", 10);
        assert_eq!(grid_col_to_source_char_index(&row, 0), 0);
        assert_eq!(grid_col_to_source_char_index(&row, 3), 3);
        assert_eq!(grid_col_to_source_char_index(&row, 9), 5, "past end clamps");
    }

    #[test]
    fn grid_col_to_char_index_honors_cjk_wide_cells() {
        // 中 (cols 0-1) 文 (cols 2-3) x (col 4); spacers at 1 and 3.
        let mut row = Row::new(10);
        row.cells[0].character = '中';
        row.cells[1].flags.insert(CellFlags::WIDE_SPACER);
        row.cells[2].character = '文';
        row.cells[3].flags.insert(CellFlags::WIDE_SPACER);
        row.cells[4].character = 'x';
        assert_eq!(grid_col_to_source_char_index(&row, 0), 0);
        // 文's right half (its spacer cell) lands at the boundary after 文.
        assert_eq!(grid_col_to_source_char_index(&row, 3), 2);
        assert_eq!(grid_col_to_source_char_index(&row, 4), 2, "'x' is char 2");
    }

    #[test]
    fn grid_col_to_char_index_counts_extra_clusters_as_full_chars() {
        // One EXTRA cell whose cluster is 2 chars ("e\u{301}"), then 'b'.
        let mut row = Row::new(10);
        row.cells[0].character = 'e';
        row.cells[0].flags.insert(CellFlags::EXTRA);
        row.extras.set_grapheme(0, std::sync::Arc::from("e\u{301}"));
        row.cells[1].character = 'b';
        assert_eq!(grid_col_to_source_char_index(&row, 1), 2);
        assert_eq!(grid_col_to_source_char_index(&row, 2), 3);
    }

    fn bv_row(
        kind: BlockViewRowKind,
        text: &str,
        line: Option<usize>,
        chunk_char_offset: usize,
        block_id: Option<BlockId>,
    ) -> BlockViewRow {
        BlockViewRow {
            kind,
            text: text.to_string(),
            block_id,
            y_top: 0.0,
            y_bottom: 20.0,
            line,
            chunk_char_offset,
            indent_cols: 0,
        }
    }

    /// Rows are bottom-to-top; a wrapped source line 0 spans chunk 0
    /// ("abcdefghij", offset 0) and chunk 1 ("klmnop", offset 10).
    fn wrapped_rows() -> Vec<BlockViewRow> {
        vec![
            bv_row(BlockViewRowKind::Output, "other", Some(1), 0, None),
            bv_row(BlockViewRowKind::Output, "klmnop", Some(0), 10, None),
            bv_row(BlockViewRowKind::Output, "abcdefghij", Some(0), 0, None),
            bv_row(BlockViewRowKind::Command, "echo", None, 0, Some(BlockId(7))),
        ]
    }

    #[test]
    fn block_view_pos_picks_the_chunk_containing_the_source_char() {
        let rows = wrapped_rows();
        let pos = block_view_pos_for_snapshot_line(&rows, 0, 3).expect("first chunk");
        assert_eq!(pos.row_index, 2);
        assert_eq!(pos.char_index, 3);
        let pos = block_view_pos_for_snapshot_line(&rows, 0, 12).expect("second chunk");
        assert_eq!(pos.row_index, 1);
        assert_eq!(pos.char_index, 2);
        // Past the source line end clamps to the last chunk's text end.
        let pos = block_view_pos_for_snapshot_line(&rows, 0, 999).expect("clamped");
        assert_eq!(pos.row_index, 1);
        assert_eq!(pos.char_index, 6);
    }

    #[test]
    fn block_view_pos_ignores_finalized_blocks_with_colliding_lines() {
        // A finalized block also has an Output row with line 0 — the live
        // (block_id None) rows must win.
        let mut rows = wrapped_rows();
        rows.push(bv_row(
            BlockViewRowKind::Output,
            "old block line 0",
            Some(0),
            0,
            Some(BlockId(3)),
        ));
        let pos = block_view_pos_for_snapshot_line(&rows, 0, 3).expect("live chunk");
        assert_eq!(pos.row_index, 2, "finalized row must not be chosen");
    }

    #[test]
    fn block_view_pos_none_when_line_unreachable() {
        let rows = wrapped_rows();
        assert_eq!(block_view_pos_for_snapshot_line(&rows, 42, 0), None);
        // Line 0 exists only on Command rows (non-selectable) → None.
        let rows = vec![bv_row(
            BlockViewRowKind::Command,
            "cmd",
            Some(0),
            0,
            Some(BlockId(1)),
        )];
        assert_eq!(block_view_pos_for_snapshot_line(&rows, 0, 0), None);
        assert_eq!(block_view_pos_for_snapshot_line(&[], 0, 0), None);
    }
}
