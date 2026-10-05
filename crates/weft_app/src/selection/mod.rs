//! v1.10.26 (FIX_SELECTION_CONTENT_ANCHORS): grid → primary-history BlockView
//! migration, the app-side `SelectionContentSource` over `BlockTracker`, the
//! document-structure fingerprint, and the drag-autoscroll ramp.
//!
//! A primary-screen TUI (omp) drag starts as a viewport-relative `GridPos`
//! selection in grid mode. When the drag crosses the top edge the anchor
//! must become a content-anchored `BlockViewSelection` in the primary history
//! snapshot view — otherwise the grid selection is viewport-relative and
//! would silently drift (L2) or get orphaned by the view switch (L3). All
//! mouse wiring lives in `mouse_controller` (single entry point:
//! `migrate_grid_selection_to_primary_history`).
//!
//! The selection model itself (`BlockSelAnchor` / `SelectionInterval` /
//! `SelectionContentSource`) is pure in `weft_core`; this module bridges it
//! to the app's real document: finished blocks (output lines) + the live
//! composed document (`prefix ++ screen` — exactly the composition
//! `screen_exit.rs` publishes into the in-flight block).

use super::*;
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use weft_core::blocks::Block;
use weft_core::grid::{CellFlags, Row};
use weft_core::selection::{BlockSelAnchor, SelectionContentSource, SelectionHandler};

/// Byte ranges of a segment's lines (line index → (byte_start, byte_end)).
type LineRanges = Vec<(usize, usize)>;

/// Direction of drag-selection autoscroll relative to the content edge.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum AutoscrollDir {
    Up,
    Down,
}

/// v1.10.26 (FIX_SELECTION_CONTENT_ANCHORS): Warp ramp — rows scrolled per
/// 40ms autoscroll tick from the pixel overshoot beyond the content edge:
/// `clamp(overshoot_px^1.5 / 100, 1, 6)`. Clamped to [1, 6] so a deep
/// overshoot can't jump past the drag target and a tiny overshoot still
/// advances (floor would freeze on sub-pixel overshoots). At 400px the
/// formula yields 80 rows pre-clamp → the 6-row cap engages (the FIX doc's
/// "≈4 rows" figure is the pre-cap arithmetic note; the literal formula is
/// authoritative). Pure; the app accumulates the fractional remainder.
pub(crate) fn autoscroll_ramp_rows(overshoot_px: f32) -> f32 {
    // Negative/sub-row overshoots are clamped to 0 before the fractional
    // power (powf of a negative base would be NaN) and then floored at 1.
    (overshoot_px.max(0.0).powf(1.5) / 100.0).clamp(1.0, 6.0)
}

/// v1.10.26 Batch D (D-4): the y threshold at which a held drag triggers a
/// Down autoscroll, decided by the VIEW CONTEXT rather than geometry.
/// In a snapshot/history view (primary_history_view or an active alt-screen
/// history peek — no prompt/bottom chrome below the content) the band shifts
/// inward to the content's bottom edge row (`content_bottom - line_h`) so a
/// drag in the last visible line still scrolls the selection downward.
/// A regular block view keeps the band a full line below the content edge
/// (`content_bottom + line_h`) — the prompt chrome below makes that band
/// physically reachable, so a full-bleed output must not make the last
/// visible line mis-trigger a Down scroll (v1.10.25 ML2: geometry alone
/// cannot distinguish the two, which mis-fired on plain block views).
pub(crate) fn down_autoscroll_threshold(
    content_bottom: f32,
    line_h: f32,
    snapshot_context: bool,
) -> f32 {
    if snapshot_context {
        content_bottom - line_h
    } else {
        content_bottom + line_h
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

/// v1.10.26: the app's `SelectionContentSource` over the real document.
///
/// Segment layout matches the block view exactly:
/// - `Some(id)` → the finished block's OUTPUT lines, trimmed of trailing
///   empty/prompt lines the same way `paint/grid_cache.rs` trims them (the
///   layout emits `line.idx` into that same list, so anchors line up).
/// - `None` → the LIVE composed document (`screen_exit.rs` publishes
///   `prefix ++ screen` into the in-flight block's output), line-indexed by
///   `str::lines()` — the same indexing LiveLayoutCache uses.
///
/// Line indices are STABLE (push-window invariant): scrolled-out lines keep
/// their indices inside the composed document, so content anchors survive
/// scroll and in-place rewrites. Line byte ranges are computed lazily per
/// segment (first touch per frame) and cached for O(1) per-line lookups.
pub(crate) struct SelectionDocSource<'a> {
    /// Finished blocks in DOCUMENT order (oldest first — `session_blocks`).
    blocks: &'a [Block],
    /// The live composed document text (the in-flight block's output).
    live_output: Option<&'a str>,
    /// BlockId.u64 → index into `blocks`.
    block_index: HashMap<u64, usize>,
    /// v1.10.26 (rust-reviewer S4): cached document order — `block_order` is
    /// queried on EVERY `doc_position` look-up (per layout row per frame), so
    /// building a `Vec` on each call churned O(visible×blocks) allocations.
    /// The order starts as the `Some(id)` list + trailing `None` (live last)
    /// and is immutable for the source's lifetime.
    order: Vec<Option<u64>>,
    /// Per-segment line byte ranges, built on first access.
    line_ranges: RefCell<HashMap<Option<u64>, Rc<LineRanges>>>,
}

impl<'a> SelectionDocSource<'a> {
    /// `blocks` is the finished-block list in DOCUMENT order (oldest first);
    /// `live_output` is the composed live document text (`prefix ++ screen`,
    /// published by `screen_exit.rs` into the in-flight block's output).
    /// The caller extracts the `&str` before the paint layout pass moves the
    /// `InFlightBlock` (the `&str` is a Copy reference into the terminal's
    /// block storage and outlives the paint frame).
    pub(crate) fn new(blocks: &'a [Block], live_output: Option<&'a str>) -> Self {
        Self {
            blocks,
            live_output,
            block_index: blocks
                .iter()
                .enumerate()
                .map(|(i, b)| (b.id.0, i))
                .collect(),
            order: blocks
                .iter()
                .map(|b| Some(b.id.0))
                .chain(std::iter::once(None))
                .collect(),
            line_ranges: RefCell::new(HashMap::new()),
        }
    }

    fn line_ranges_for(&self, block: Option<u64>) -> Option<Rc<LineRanges>> {
        if let Some(cached) = self.line_ranges.borrow().get(&block) {
            return Some(cached.clone());
        }
        let ranges: LineRanges = match block {
            Some(id) => {
                let output: &str = &self.blocks.get(*self.block_index.get(&id)?)?.output;
                let lines: Vec<&str> = output.lines().collect();
                let trimmed = crate::paint::grid_cache::trimmed_output_line_count(&lines);
                let base = output.as_ptr() as usize;
                lines[..trimmed]
                    .iter()
                    .map(|line| {
                        let start = line.as_ptr() as usize - base;
                        (start, start + line.len())
                    })
                    .collect()
            }
            None => {
                let output = self.live_output?;
                let base = output.as_ptr() as usize;
                output
                    .lines()
                    .map(|line| {
                        let start = line.as_ptr() as usize - base;
                        (start, start + line.len())
                    })
                    .collect()
            }
        };
        let rc = Rc::new(ranges);
        self.line_ranges.borrow_mut().insert(block, rc.clone());
        Some(rc)
    }
}

impl SelectionContentSource for SelectionDocSource<'_> {
    fn line_count(&self, block: Option<u64>) -> usize {
        self.line_ranges_for(block).map_or(0, |ranges| ranges.len())
    }

    fn line_text(&self, block: Option<u64>, line: usize) -> Option<&str> {
        let ranges = self.line_ranges_for(block)?;
        let (start, end) = *ranges.get(line)?;
        let text = match block {
            Some(id) => &self.blocks[*self.block_index.get(&id)?].output,
            None => self.live_output?,
        };
        Some(&text[start..end])
    }

    fn block_order(&self) -> &[Option<u64>] {
        &self.order
    }
}

/// v1.10.26: structural fingerprint of the document a selection lives in:
/// the finished-block id ORDER plus the live document's HEAD length
/// (`screen_history_lines + screen_prefix_lines`). Any add/remove/reorder
/// (split, TUI exit, deletion) — OR a head grow (a superseded screen frame
/// preserved by `append_screen_history_frame` while a live-segment selection
/// is active) — invalidates content anchors → the app clears the selection.
///
/// Deliberately EXCLUDES the live segment's TAIL length — streaming append
/// keeps it growing below the anchors and must NOT clear (index-stable).
pub(crate) fn block_selection_fingerprint(blocks: &[Block], live_head_lines: usize) -> Vec<u64> {
    let mut fingerprint: Vec<u64> = blocks.iter().map(|b| b.id.0).collect();
    fingerprint.push(live_head_lines as u64);
    fingerprint
}

/// Start a content-anchored block selection and record the document
/// fingerprint it was made against, so the renderer can detect structural
/// changes next frame and clear a now-stale selection. `fingerprint` is the
/// OWNED finished-block id order (the caller computes it in a scoped borrow,
/// so no slice borrow crosses the mutable handler borrow).
pub(crate) fn start_block_selection(
    handler: &mut SelectionHandler,
    anchor: BlockSelAnchor,
    fingerprint: Vec<u64>,
) {
    handler.start_block_view(anchor);
    handler.block_doc_fingerprint = Some(fingerprint);
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
    /// content-anchored block-view selection at the same source position,
    /// entering the primary history snapshot view in the process. Returns
    /// `true` on success; on failure the view mode and grid selection are
    /// left untouched (degrade — never switch views and orphan the
    /// selection).
    ///
    /// v1.10.20 (S2): refuses to migrate while the Ctrl-C interrupt capture
    /// window is active — the snapshot rewrites the transcript then, so the
    /// live-grid line mapping would not match the rendered rows and the
    /// anchor could land on the wrong snapshot line (shift). The caller
    /// keeps the grid selection and retries on a later sync.
    ///
    /// v1.10.26 (FIX_SELECTION_CONTENT_ANCHORS): the migration is now a
    /// direct anchor assignment — `head = (None, snapshot_line)` — because
    /// `primary_screen_snapshot_line_for_viewport_row` already returns the
    /// COMPOSED document line (`prefix ++ screen`), which is the anchor's
    /// coordinate space. No row-snapshot mapping exists anymore.
    pub(super) fn migrate_grid_selection_to_primary_history(&mut self) -> bool {
        // Step 1 (read-only): the grid selection anchor + its snapshot line,
        // computed against the CURRENT grid. Entering the history view
        // snapshots but never mutates the grid, so the mapping stays exact.
        let (anchor, snapshot_line) = {
            // v1.12.25 (audit 3-B, P1-01): the empty-tabs transient has no
            // selection to migrate.
            let Some(tab) = self.sessions.active_mut() else {
                return false;
            };
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
            let Some(terminal) = self.sessions.active().and_then(|tab| tab.terminal.as_ref())
            else {
                return false;
            };
            let Some(row) = terminal.grid().viewport.get(anchor.start.row) else {
                return false;
            };
            grid_col_to_source_char_index(row, anchor.start.col)
        };
        // Step 3: switch into the history view (snapshots synchronously).
        if !self
            .sessions
            .active_mut()
            .is_some_and(|tab| tab.enter_primary_history_if_active())
        {
            return false;
        }
        // Step 4: rebuild the selection in block space; the drag continues
        // through the block branch (pump / move) from here. The anchor is a
        // direct content coordinate — no row-snapshot lookup needed.
        let anchor_pos = BlockSelAnchor {
            block: None,
            line: snapshot_line,
            char_offset: source_char,
        };
        let Some(pane) = self.sessions.active_mut() else {
            return false;
        };
        let fingerprint = pane
            .terminal
            .as_ref()
            .map(|terminal| {
                block_selection_fingerprint(
                    terminal.block_tracker().session_blocks(),
                    terminal.screen_head_lines(),
                )
            })
            .unwrap_or_default();
        start_block_selection(&mut pane.selection_handler, anchor_pos, fingerprint);
        pane.selection_handler.clear_grid_selection();
        tracing::info!(
            grid_row = anchor.start.row,
            snapshot_line,
            "migrated grid selection into primary history block view"
        );
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use weft_core::selection::SelectionContentSource;

    // ── v1.10.26: Warp ramp (replaces the legacy autoscroll_steps) ────────

    #[test]
    fn autoscroll_ramp_table() {
        // Boundary table from the FIX doc: 0/10/100/400 px overshoot.
        assert_eq!(autoscroll_ramp_rows(0.0), 1.0); // 0^1.5=0 → clamp floor
        assert_eq!(autoscroll_ramp_rows(10.0), 1.0); // 31.6/100=0.316 → floor
        assert_eq!(autoscroll_ramp_rows(100.0), 6.0); // 1000/100=10 → cap
        assert_eq!(autoscroll_ramp_rows(400.0), 6.0); // 8000/100=80 → cap
                                                      // Monotone inside the ramp band (above the 1-row floor).
        assert!(autoscroll_ramp_rows(30.0) > autoscroll_ramp_rows(20.0)); // 1.64 > 0.89 → floor
                                                                          // Negative/sub-row overshoot leads with the floor.
        assert_eq!(autoscroll_ramp_rows(-5.0), 1.0);
    }

    #[test]
    fn autoscroll_ramp_stays_capped_and_bounded() {
        for px in [0.0, 1.0, 100.0, 1_000.0, 1_000_000.0] {
            let rows = autoscroll_ramp_rows(px);
            assert!((1.0..=6.0).contains(&rows), "ramp out of band for {px}px");
        }
    }

    // ── v1.10.25 Batch 3 (FIX_SELECTION_AND_RESIZE_REMAINING) + v1.10.26
    // Batch D (D-4): Down band divergence by VIEW CONTEXT ───────────────

    #[test]
    fn down_autoscroll_threshold_diverges_by_view_context() {
        const LINE_H: f32 = 20.0;
        // Snapshot/history view (primary_history_view or alt peek): no chrome
        // below the content → the band shifts inward to the bottom edge row,
        // so a drag in the last visible line still autoscrolls Down.
        assert_eq!(down_autoscroll_threshold(800.0, LINE_H, true), 780.0);
        assert_eq!(down_autoscroll_threshold(790.0, LINE_H, true), 770.0);
        // Regular block view: the band stays a full line below the content
        // edge — the prompt chrome below makes it physically reachable, and
        // a full-bleed block must NOT make the last line mis-trigger.
        assert_eq!(down_autoscroll_threshold(700.0, LINE_H, false), 720.0);
        assert_eq!(down_autoscroll_threshold(800.0, LINE_H, false), 820.0);
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

    // ── v1.10.26: document-structure fingerprint (split/finish/delete →
    // clear; streaming append → keep) ─────────────────────────────────────

    fn fake_block(id: u64, output: &str) -> Block {
        use std::sync::Arc;
        use std::time::SystemTime;
        use weft_core::blocks::BlockId;
        Block {
            id: BlockId(id),
            command: String::new(),
            cwd: None,
            output: Arc::from(output),
            styled_output: None,
            exit_code: Some(0),
            started_at: SystemTime::now(),
            finished_at: Some(SystemTime::now()),
            collapsed: false,
            screen_origin: false,
        }
    }

    #[test]
    fn fingerprint_changes_on_finish_split_and_delete_but_not_on_live_growth() {
        // Two blocks in document order; live head length 0.
        let blocks = vec![fake_block(1, "a\n"), fake_block(2, "b\n")];
        let fp = block_selection_fingerprint(&blocks, 0);
        assert_eq!(fp, vec![1, 2, 0]);

        // A new block finishing (third block) → structure changed → clear.
        let grown = vec![
            fake_block(1, "a\n"),
            fake_block(2, "b\n"),
            fake_block(3, "c\n"),
        ];
        assert_ne!(
            block_selection_fingerprint(&grown, 0),
            fp,
            "block finish must clear"
        );

        // Split (a segment replaced by freshly numbered blocks) → changed.
        let split = vec![fake_block(40, "aa\n"), fake_block(2, "b\n")];
        assert_ne!(
            block_selection_fingerprint(&split, 0),
            fp,
            "split must clear"
        );

        // Deletion → changed.
        let deleted = vec![fake_block(1, "a\n")];
        assert_ne!(
            block_selection_fingerprint(&deleted, 0),
            fp,
            "deletion must clear"
        );

        // Streaming live append is NOT part of the fingerprint — the live
        // segment's TAIL growth must keep the anchors (index-stable).
        assert_eq!(
            block_selection_fingerprint(&blocks, 0),
            fp,
            "identical blocks → same fingerprint"
        );
    }

    #[test]
    fn fingerprint_includes_live_head_grow_from_preserved_frame_append() {
        // v1.10.26 (rust-reviewer S1): `append_screen_history_frame` prepends
        // lines to the composed document's head (into the live segment), which
        // shifts every live-segment anchor by the added lines — silent drift
        // while the finished-block order is unchanged. The fingerprint must
        // key on the head length so the painted comparison clears it.
        let blocks = vec![fake_block(1, "a\n")];
        let before = block_selection_fingerprint(&blocks, 0);
        assert_eq!(
            block_selection_fingerprint(&blocks, 0),
            before,
            "same head → same fingerprint"
        );
        let after_frame = block_selection_fingerprint(
            &blocks, 4, // a superseded frame with 4 lines was preserved
        );
        assert_ne!(
            after_frame, before,
            "a preserved-frame append to the head must clear the selection"
        );
    }

    // ── v1.10.26: SelectionDocSource line indexing vs. the layout ────────

    #[test]
    fn doc_source_indexes_finished_block_lines_like_the_layout() {
        let blocks = vec![fake_block(7, "first\nsecond\n\n"), fake_block(9, "x\n")];
        let source = SelectionDocSource::new(&blocks, None);
        let order = source.block_order();
        assert_eq!(order, vec![Some(7), Some(9), None]);
        // Trailing empty lines are trimmed exactly like grid_cache — the
        // layout's line.idx addresses the same list.
        assert_eq!(source.line_count(Some(7)), 2);
        assert_eq!(source.line_text(Some(7), 0), Some("first"));
        assert_eq!(source.line_text(Some(7), 1), Some("second"));
        assert_eq!(source.line_text(Some(7), 2), None, "trimmed away");
        assert_eq!(source.line_count(Some(9)), 1);
        assert_eq!(source.line_text(Some(9), 0), Some("x"));
        assert_eq!(source.line_count(None), 0, "no live block");
    }

    #[test]
    fn doc_source_indexes_live_document_by_lines() {
        let live_output = "alpha\nbeta\n\n"; // trailing blank line IS a str::lines entry
        let blocks: Vec<Block> = Vec::new();
        let source = SelectionDocSource::new(&blocks, Some(live_output));
        assert_eq!(
            source.line_count(None),
            3,
            "str::lines keeps the blank line"
        );
        assert_eq!(source.line_text(None, 0), Some("alpha"));
        assert_eq!(source.line_text(None, 1), Some("beta"));
        assert_eq!(source.line_text(None, 2), Some(""));
        assert_eq!(source.line_text(None, 3), None);
    }

    /// The whole refactor's end-to-end regression at the app-document level:
    /// a live document that scrolls AND gets rewritten in place keeps its
    /// anchors pointing at the same lines (text follows current content).
    #[test]
    fn migrated_live_document_anchor_survives_scroll_and_rewrite() {
        use weft_core::selection::BlockViewSelection;
        let mut doc = [
            "line-0".to_string(),
            "line-1".to_string(),
            "line-2".to_string(),
            "line-3".to_string(),
            "line-4".to_string(),
        ];
        let blocks: Vec<Block> = Vec::new();
        let joined = doc.join("\n");
        let source = SelectionDocSource::new(&blocks, Some(joined.as_str()));
        let sel = BlockViewSelection::new(
            BlockSelAnchor {
                block: None,
                line: 1,
                char_offset: 0,
            },
            BlockSelAnchor {
                block: None,
                line: 3,
                char_offset: 2,
            },
        );
        assert_eq!(sel.text(&source), "line-1\nline-2\nli");
        // In-place rewrite of the middle line: the composed document is
        // re-published with the same line indices.
        doc[2] = "rewritten".to_string();
        let joined = doc.join("\n");
        let source = SelectionDocSource::new(&blocks, Some(joined.as_str()));
        assert_eq!(
            sel.text(&source),
            "line-1\nrewritten\nli",
            "anchor follows the line position, copy reads current content"
        );
    }
}
