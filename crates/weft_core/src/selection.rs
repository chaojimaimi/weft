//! Mouse selection model.

use crate::grid::{CellFlags, Grid};
use std::cmp::Ordering;

#[cfg(test)]
mod anchor_tests;

/// A point in the grid (row, col), 0-based.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GridPos {
    pub row: usize,
    pub col: usize,
}

impl GridPos {
    pub fn new(row: usize, col: usize) -> Self {
        Self { row, col }
    }
}

/// Selection mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SelectionMode {
    /// Character-level selection.
    Simple,
    /// Line-level selection (whole lines).
    Line,
    /// Block selection (rectangular).
    Block,
}

/// An active selection region.
#[derive(Clone, Debug)]
pub struct Selection {
    /// Start position (where the mouse was pressed).
    pub start: GridPos,
    /// End position (where the mouse was dragged to).
    pub end: GridPos,
    /// Selection mode.
    pub mode: SelectionMode,
}

impl Selection {
    pub fn new(start: GridPos, end: GridPos, mode: SelectionMode) -> Self {
        Self { start, end, mode }
    }

    /// Get the ordered (top-left to bottom-right) range.
    pub fn ordered(&self) -> (GridPos, GridPos) {
        if self.start.row < self.end.row
            || (self.start.row == self.end.row && self.start.col <= self.end.col)
        {
            (self.start, self.end)
        } else {
            (self.end, self.start)
        }
    }

    /// Check if a cell position is within the selection.
    pub fn contains(&self, row: usize, col: usize) -> bool {
        let (tl, br) = self.ordered();
        match self.mode {
            SelectionMode::Simple => {
                if row < tl.row || row > br.row {
                    return false;
                }
                if row == tl.row && col < tl.col {
                    return false;
                }
                if row == br.row && col > br.col {
                    return false;
                }
                true
            }
            SelectionMode::Line => row >= tl.row && row <= br.row,
            SelectionMode::Block => {
                row >= tl.row && row <= br.row && col >= tl.col && col <= br.col
            }
        }
    }

    /// Extract the selected text from the grid.
    ///
    /// v1.6.0: `CellFlags::EXTRA` cells contribute their full grapheme cluster
    /// via [`Grid::grapheme_at`].
    /// v1.12.27a (P1-01): the three copy branches' inline cell walks are
    /// consolidated into the private [`row_cells_text`] Grid-level helper.
    pub fn text_from_grid(&self, grid: &Grid) -> String {
        let (tl, br) = self.ordered();
        // Clamp endpoints to valid bounds — a margin endpoint would otherwise
        // panic on copy (the app also clamps in pixel_to_grid).
        if grid.num_rows == 0 || grid.num_cols == 0 {
            return String::new();
        }
        let max_row = grid.num_rows - 1;
        let max_col = grid.num_cols - 1;
        let tl = GridPos::new(tl.row.min(max_row), tl.col.min(max_col));
        let br = GridPos::new(br.row.min(max_row), br.col.min(max_col));
        let mut result = String::new();

        match self.mode {
            SelectionMode::Simple | SelectionMode::Line => {
                for row in tl.row..=br.row {
                    let col_start = if row == tl.row { tl.col } else { 0 };
                    let col_end = if row == br.row {
                        br.col
                    } else {
                        grid.num_cols - 1
                    };

                    result.push_str(&row_cells_text(grid, row, col_start, col_end, true));

                    if row < br.row && row < grid.num_rows && !grid.viewport[row].wrapped {
                        result.push('\n');
                    }
                }
            }
            SelectionMode::Block => {
                for row in tl.row..=br.row {
                    result.push_str(&row_cells_text(grid, row, tl.col, br.col, false));
                    if row < br.row {
                        result.push('\n');
                    }
                }
            }
        }

        result
    }
}

/// v1.12.27a (P1-01): the ONE Grid-level cell-walk rule shared by the
/// selection copy branches (previously three inline copies). The walk is
/// the same skip-`WIDE_SPACER` / EXTRA-cluster rule as the Row-level
/// `grid::walk_cells`, but selection reads through the scroll-aware
/// `Grid::cell` / `Grid::grapheme_at` pair — a different layer than the
/// Row walker — so it consolidates HERE instead of forcing materialized
/// rows through the Row walker.
///
/// Semantics kept verbatim per branch: `trim = true` (`Simple`/`Line`)
/// first finds the last cell whose char is NOT a literal space — which
/// deliberately COUNTS `\0` cells as content — then walks
/// `[col_start..=last]`; `trim = false` (`Block`) walks the range whole.
/// `ch` is pushed RAW: selection passes `\0` through unchanged (unlike
/// display/snapshot, which map it to a space — known quirk, recorded in
/// PROGRESS; not "fixed" here under the zero-behavior red line).
fn row_cells_text(grid: &Grid, row: usize, col_start: usize, col_end: usize, trim: bool) -> String {
    let mut out = String::new();
    let mut last = col_end;
    if trim {
        last = 0;
        for col in col_start..=col_end {
            let cell = grid.cell(row, col);
            if cell.flags.contains(CellFlags::WIDE_SPACER) {
                continue;
            }
            if cell.character != ' ' {
                last = col;
            }
        }
    }
    for col in col_start..=last {
        let cell = grid.cell(row, col);
        if cell.flags.contains(CellFlags::WIDE_SPACER) {
            continue;
        }
        if cell.flags.contains(CellFlags::EXTRA) {
            if let Some(cluster) = grid.grapheme_at(row, col) {
                out.push_str(cluster);
                continue;
            }
        }
        out.push(cell.character);
    }
    out
}

/// Mouse button for selection events.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
    Other,
}

/// Manages mouse selection state.
pub struct SelectionHandler {
    /// Currently active selection, if any.
    pub selection: Option<Selection>,
    /// Whether we're currently selecting (mouse held down).
    pub selecting: bool,
    /// The selection mode for the current drag.
    pub mode: SelectionMode,
    /// Block-view selection (Warp-style view); separate from `selection`.
    pub block_view_selection: Option<BlockViewSelection>,
    /// v1.10.26 (FIX_SELECTION_CONTENT_ANCHORS): opaque structural
    /// fingerprint of the document the block selection was made against (the
    /// app stores the finished-block id order). When the next frame's
    /// fingerprint differs, the document structure changed (split / block
    /// finish / deletion) and the anchors are stale — the app clears the
    /// selection.
    pub block_doc_fingerprint: Option<Vec<u64>>,
}

impl SelectionHandler {
    pub fn new() -> Self {
        Self {
            selection: None,
            selecting: false,
            mode: SelectionMode::Simple,
            block_view_selection: None,
            block_doc_fingerprint: None,
        }
    }

    /// Start a new selection at the given position.
    pub fn start(&mut self, pos: GridPos, mode: SelectionMode) {
        self.selection = Some(Selection::new(pos, pos, mode));
        self.selecting = true;
        self.mode = mode;
    }

    /// Extend the selection to a new endpoint (during drag).
    pub fn extend(&mut self, pos: GridPos) {
        if let Some(sel) = &mut self.selection {
            sel.end = pos;
        }
    }

    /// Finish the selection (mouse released).
    pub fn end(&mut self) {
        self.selecting = false;
    }

    /// Clear the selection.
    pub fn clear(&mut self) {
        self.selection = None;
        self.selecting = false;
        self.block_view_selection = None;
        self.block_doc_fingerprint = None;
    }

    /// v1.10.20: drop only the grid selection. `clear()` also drops the
    /// block-view selection; the migration path (grid selection → primary
    /// history BlockView selection) replaces just the grid half so the
    /// fresh block selection survives.
    pub fn clear_grid_selection(&mut self) {
        self.selection = None;
    }

    /// Get the selected text from the grid.
    pub fn selected_text(&self, grid: &Grid) -> Option<String> {
        self.selection.as_ref().map(|sel| sel.text_from_grid(grid))
    }

    // ── Block-view selection helpers ──────────────────────────────────

    /// v1.10.26 (FIX_SELECTION_CONTENT_ANCHORS): start a content-anchored
    /// block-view selection at `anchor`. The anchor lives in the unified
    /// logical document (block output lines / live composed document), so no
    /// renderer row snapshot is captured — text extraction and per-frame
    /// highlight derive from the current content source instead.
    pub fn start_block_view(&mut self, anchor: BlockSelAnchor) {
        self.block_view_selection = Some(BlockViewSelection::new(anchor, anchor));
        self.selecting = true;
    }

    /// Extend the active block-view selection endpoint (during drag).
    pub fn extend_block_view(&mut self, anchor: BlockSelAnchor) {
        if let Some(sel) = &mut self.block_view_selection {
            sel.tail = anchor;
        }
    }

    /// Text from the active block-view selection, read through the current
    /// content source, if any.
    pub fn block_view_text(&self, source: &dyn SelectionContentSource) -> Option<String> {
        self.block_view_selection
            .as_ref()
            .map(|sel| sel.text(source))
    }
}

impl Default for SelectionHandler {
    fn default() -> Self {
        Self::new()
    }
}

// ── Block-view selection ──────────────────────────────────────────────
// v1.10.26 (FIX_SELECTION_CONTENT_ANCHORS): selection now anchors in the
// unified logical document (content coordinates) instead of a renderer row
// snapshot. Hit-testing still produces `BlockViewRow` bands (y geometry +
// text + block/line keys), but a selection stores only the two endpoint
// anchors; every highlight/text/interval query is DERIVED per frame from the
// current `SelectionContentSource` (Warp model — "index space" breakage and
// the Θ(span×window) copy cost are gone with the snapshot).

use crate::blocks::BlockId;

/// Kind of a block-view row. Mirrors the renderer's internal `LaidRow` but
/// lives in `weft_core` so the selection model has no GUI-crate dependency.
/// Only `Output` / `Command` rows carry selectable text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BlockViewRowKind {
    Output,
    Command,
    Header,
    Separator,
    LiveCommand,
    /// v1.8.2: AI diagnose panel row; non-selectable, carries `block_id`.
    DiagnosePanel,
}

/// A single rendered row in the block view, captured at layout time. The y
/// range is scroll-adjusted physical pixels. Used for hit-testing and the
/// `(block, line)` key that maps a pixel row back to a content anchor.
#[derive(Clone, Debug)]
pub struct BlockViewRow {
    pub kind: BlockViewRowKind,
    /// Visible text of this row (post-wrap single line; empty for Separator).
    pub text: String,
    /// Owning block, if any (Command/Header/LiveCommand).
    pub block_id: Option<BlockId>,
    /// Top y of the row in physical pixels (scroll-adjusted).
    pub y_top: f32,
    /// Bottom y of the row (`y_top + pitch`).
    pub y_bottom: f32,
    /// Line index into the owning block's output (`None` for structural
    /// rows and resume hints); resolves OCR 8 link spans and — together with
    /// `block_id` — forms the content-coordinate anchor key.
    pub line: Option<usize>,
    /// Char offset of this row's text within its source line (0 for
    /// single-chunk rows). Add it to the chunk-local `char_index` for the
    /// full-line anchor char offset.
    pub chunk_char_offset: usize,
    /// v1.10.13: Leading visual indent (columns) of this row's text. Command
    /// first lines indent for the chevron + "> " prompt (2-3 cols); wrapped
    /// continuation lines and output rows are flush-left (0). Hit-testing
    /// subtracts this from the pixel-derived column so clicks on a command
    /// line land on the right character.
    pub indent_cols: usize,
}

impl BlockViewRow {
    /// True if the kind carries selectable text.
    pub fn is_selectable(&self) -> bool {
        matches!(
            self.kind,
            BlockViewRowKind::Output | BlockViewRowKind::Command | BlockViewRowKind::LiveCommand
        )
    }

    /// True if a physical-pixel y falls inside this row's band.
    pub fn contains_y(&self, y: f32) -> bool {
        y >= self.y_top && y < self.y_bottom
    }
}

/// Map a column (relative to the row's text start, already indent-adjusted)
/// to a char index in `text`, honoring CJK double-width; past the last char,
/// clamps to the end.
pub(super) fn char_index_at_col(text: &str, target_col: usize) -> usize {
    let mut col_cursor = 0usize;
    for (ci, c) in text.chars().enumerate() {
        let w = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
        if w == 0 {
            continue; // zero-width (combining mark): stays on previous cell
        }
        if target_col < col_cursor + w {
            return ci;
        }
        col_cursor += w;
    }
    text.chars().count()
}

/// Pixel x → char index for a block-view row, subtracting its visual indent.
pub fn pixel_x_to_char_index(
    text: &str,
    x: f64,
    content_left: f64,
    cell_w: f64,
    indent_cols: usize,
) -> usize {
    let target_col = ((x - content_left) / cell_w).max(0.0) as usize;
    char_index_at_col(text, target_col.saturating_sub(indent_cols))
}

// ── Content anchors (v1.10.26) ───────────────────────────────────────────

/// Content-coordinate anchor into the unified logical document.
///
/// `block = Some(id)` addresses a finished block's output lines; `None`
/// addresses the live composed document (`prefix ++ screen` — the primary
/// history snapshot while a TUI runs, or the streaming in-flight block).
/// `line` is the line index INSIDE that segment, `char_offset` a char offset
/// within the line (copy endpoints). Anchors are stable across scroll: a
/// line's index never changes once assigned (the push-window invariant keeps
/// scrolled-out lines addressable at their original indices), so neither
/// content rewrites nor viewport changes can silently move a selection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BlockSelAnchor {
    pub block: Option<u64>,
    /// Line index in the segment's logical document.
    pub line: usize,
    /// Char offset within the line (copy-ends and endpoint char slicing).
    pub char_offset: usize,
}

impl BlockSelAnchor {
    pub fn new(block: Option<u64>, line: usize, char_offset: usize) -> Self {
        Self {
            block,
            line,
            char_offset,
        }
    }
}

/// The content a block-view selection reads. Implemented by `weft_app` over
/// the `BlockTracker` (finished blocks = output lines; live = the composed
/// `prefix ++ screen` document) and by in-memory sources in tests.
///
/// The document ORDER of segments is the sole authority for cross-segment
/// comparisons: `order[0]` is the top of the document, the live segment
/// (`None`) is ALWAYS last.
pub trait SelectionContentSource {
    /// Number of logical lines in the segment (`0` for an absent segment).
    fn line_count(&self, block: Option<u64>) -> usize;
    /// The segment's line text; `None` when the segment or line is absent.
    fn line_text(&self, block: Option<u64>, line: usize) -> Option<&str>;
    /// Document order of the segments, top-to-bottom, `None` (live) last.
    /// v1.10.26 (rust-reviewer S4): returns a SLICE, not an owned `Vec` —
    /// `doc_position` queries this per frame, so an allocation per query
    /// churned O(visible×blocks). Impls cache the order (the app's
    /// `SelectionDocSource` builds it once at construction).
    fn block_order(&self) -> &[Option<u64>];
}

/// Position of a segment in the document order; `len` when absent.
fn doc_position(source: &dyn SelectionContentSource, block: Option<u64>) -> usize {
    let order = source.block_order();
    order
        .iter()
        .position(|key| *key == block)
        .unwrap_or(order.len())
}

/// Document-order comparison of two anchors: segment position → line →
/// char_offset (exactly the order the FIX doc's comparator mandates).
fn anchor_cmp(
    source: &dyn SelectionContentSource,
    a: &BlockSelAnchor,
    b: &BlockSelAnchor,
) -> Ordering {
    doc_position(source, a.block)
        .cmp(&doc_position(source, b.block))
        .then(a.line.cmp(&b.line))
        .then(a.char_offset.cmp(&b.char_offset))
}

/// Clamp a possibly-stale anchor into an existing segment (`lo = min(n-1,line)`,
/// mirroring Warp `update_selection_after_height_change`). A segment with zero
/// lines keeps the raw line (no line can ever match it → safe degrade).
fn clamp_anchor(source: &dyn SelectionContentSource, anchor: BlockSelAnchor) -> BlockSelAnchor {
    let n = source.line_count(anchor.block);
    if n == 0 {
        return anchor;
    }
    BlockSelAnchor {
        line: anchor.line.min(n - 1),
        ..anchor
    }
}

/// A content-anchored block-view selection (Warp model).
///
/// No row snapshot, no `frame_delta`, no sync machinery: `head` is the drag
/// anchor, `tail` the drag endpoint; every query derives from the CURRENT
/// `SelectionContentSource`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BlockViewSelection {
    /// The drag anchor (start).
    pub head: BlockSelAnchor,
    /// The drag endpoint.
    pub tail: BlockSelAnchor,
}

impl BlockViewSelection {
    pub fn new(head: BlockSelAnchor, tail: BlockSelAnchor) -> Self {
        Self { head, tail }
    }

    /// True when both endpoints coincide (a click without any drag).
    pub fn is_empty(&self) -> bool {
        self.head == self.tail
    }

    /// Derive the ordered selection interval against `source`.
    pub fn interval(&self, source: &dyn SelectionContentSource) -> SelectionInterval {
        SelectionInterval::new(source, self.head, self.tail)
    }

    /// Extract the selected text by reading the CURRENT content source
    /// between the clamped, ordered endpoints (document order). Soft-wrapped
    /// lines are served whole by the source, so copy needs no chunk
    /// reassembly; endpoint lines are sliced by their `char_offset`s.
    pub fn text(&self, source: &dyn SelectionContentSource) -> String {
        self.interval(source).text(source)
    }
}

/// The ordered, per-frame derived range of a block selection. Built once per
/// frame against the current source and queried per visible layout row
/// (O(visible) — no snapshot walking, no Θ(span×window) copy cost).
pub struct SelectionInterval {
    lo: BlockSelAnchor,
    hi: BlockSelAnchor,
    lo_pos: usize,
    hi_pos: usize,
    empty: bool,
}

impl SelectionInterval {
    pub(crate) fn new(
        source: &dyn SelectionContentSource,
        head: BlockSelAnchor,
        tail: BlockSelAnchor,
    ) -> Self {
        if head == tail {
            let pos = doc_position(source, head.block);
            return Self {
                lo: head,
                hi: tail,
                lo_pos: pos,
                hi_pos: pos,
                empty: true,
            };
        }
        // Clamp stale lines into the current segment bounds BEFORE ordering so
        // a shrink cannot invert the two endpoints.
        let (head, tail) = (clamp_anchor(source, head), clamp_anchor(source, tail));
        let (lo, hi) = if anchor_cmp(source, &head, &tail).is_le() {
            (head, tail)
        } else {
            (tail, head)
        };
        let lo_pos = doc_position(source, lo.block);
        let hi_pos = doc_position(source, hi.block);
        Self {
            lo,
            hi,
            lo_pos,
            hi_pos,
            empty: false,
        }
    }

    pub fn empty(&self) -> bool {
        self.empty
    }

    /// True when the segment is absent from the document (deleted/split away)
    /// or its position falls outside the selection span.
    fn segment_within(
        &self,
        source: &dyn SelectionContentSource,
        block: Option<u64>,
    ) -> Option<usize> {
        if self.empty {
            return None;
        }
        let pos = doc_position(source, block);
        if pos >= source.block_order().len() {
            return None; // absent segment
        }
        if pos < self.lo_pos || pos > self.hi_pos {
            return None;
        }
        Some(pos)
    }

    /// Whether a layout row with key `(block, line)` is inside the selection.
    /// Inclusive at both endpoints; absent segments and out-of-range lines
    /// are never highlighted.
    pub fn contains(
        &self,
        source: &dyn SelectionContentSource,
        block: Option<u64>,
        line: usize,
    ) -> bool {
        let Some(pos) = self.segment_within(source, block) else {
            return false;
        };
        if line >= source.line_count(block) {
            return false;
        }
        if self.lo_pos == self.hi_pos {
            return line >= self.lo.line && line <= self.hi.line;
        }
        if pos == self.lo_pos {
            return line >= self.lo.line;
        }
        if pos == self.hi_pos {
            return line <= self.hi.line;
        }
        true
    }

    /// Whether ANY structural row of the segment (command/header/separator —
    /// rows that carry no `(block, line)` of their own but belong to a block
    /// visually inside the span) should be banded whole-row.
    ///
    /// v1.10.26 (rust-reviewer S2): banded ONLY when the selection straddles
    /// the segment — lo or hi lives in a DIFFERENT segment. A selection wholly
    /// inside the segment (both endpoints in it) leaves its structural rows
    /// unbanded; previously the "any line in segment" test lit up the whole
    /// header/command when selecting one word mid-block. This is the old
    /// row-snapshot straddle rule: a segment's structural rows sit outside
    /// the selected lines unless the span reaches into a neighboring segment.
    pub fn block_intersects(
        &self,
        source: &dyn SelectionContentSource,
        block: Option<u64>,
    ) -> bool {
        let Some(pos) = self.segment_within(source, block) else {
            return false;
        };
        if source.line_count(block) == 0 {
            return false;
        }
        !(self.lo_pos == pos && self.hi_pos == pos)
    }

    /// Char slice of `(block, line)` inside the selection, in line-char
    /// coordinates (the paint path converts this to chunk columns via the
    /// chunk's `char_offset`). `Some((0, len))` for fully covered lines;
    /// endpoint lines are clamped to their `char_offset`s.
    pub fn char_range(
        &self,
        source: &dyn SelectionContentSource,
        block: Option<u64>,
        line: usize,
    ) -> Option<(usize, usize)> {
        if !self.contains(source, block, line) {
            return None;
        }
        let len = source
            .line_text(block, line)
            .map(|text| text.chars().count())
            .unwrap_or(0);
        let pos = doc_position(source, block);
        let mut start = 0;
        let mut end = len;
        if pos == self.lo_pos && line == self.lo.line {
            start = self.lo.char_offset.min(len);
        }
        if pos == self.hi_pos && line == self.hi.line {
            end = self.hi.char_offset.min(len);
        }
        (end > start).then_some((start, end))
    }

    /// Accumulate the selected text line by line in document order. Lines
    /// with an empty contribution are skipped without inserting blank lines,
    /// mirroring the legacy snapshot walker's newline rules.
    pub fn text(&self, source: &dyn SelectionContentSource) -> String {
        if self.empty {
            return String::new();
        }
        let mut out = String::new();
        let mut contributed = false;
        let order = source.block_order();
        for (pos, block) in order.iter().enumerate() {
            if pos < self.lo_pos || pos > self.hi_pos {
                continue;
            }
            let block = *block;
            let n = source.line_count(block);
            if n == 0 {
                continue;
            }
            let (line_lo, line_hi) = if self.lo_pos == self.hi_pos {
                (self.lo.line.min(n - 1), self.hi.line.min(n - 1))
            } else if pos == self.lo_pos {
                (self.lo.line.min(n - 1), n - 1)
            } else if pos == self.hi_pos {
                (0, self.hi.line.min(n - 1))
            } else {
                (0, n - 1)
            };
            for line in line_lo..=line_hi {
                let len = source
                    .line_text(block, line)
                    .map(|t| t.chars().count())
                    .unwrap_or(0);
                let mut start = 0;
                let mut end = len;
                if pos == self.lo_pos && line == self.lo.line {
                    start = self.lo.char_offset.min(len);
                }
                if pos == self.hi_pos && line == self.hi.line {
                    end = self.hi.char_offset.min(len);
                }
                if end <= start {
                    continue;
                }
                if contributed {
                    out.push('\n');
                }
                if let Some(text) = source.line_text(block, line) {
                    out.extend(text.chars().skip(start).take(end - start));
                }
                contributed = true;
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grid::Grid;

    fn filled_grid() -> Grid {
        let mut grid = Grid::new(5, 10);
        for row in 0..5 {
            for col in 0..10 {
                let ch = char::from_digit((row * 10 + col) as u32 % 36, 36).unwrap_or(' ');
                grid.viewport[row].cells[col].character = ch;
            }
        }
        grid
    }

    #[test]
    fn simple_selection_text() {
        let grid = filled_grid();
        let sel = Selection::new(
            GridPos::new(0, 0),
            GridPos::new(0, 4),
            SelectionMode::Simple,
        );
        let text = sel.text_from_grid(&grid);
        assert!(!text.is_empty());
    }

    #[test]
    fn out_of_bounds_end_does_not_panic() {
        // Regression: margin endpoints used to panic text_from_grid on copy.
        let grid = filled_grid(); // 5 rows × 10 cols
        let sel = Selection::new(
            GridPos::new(0, 0),
            GridPos::new(99, 99),
            SelectionMode::Simple,
        );
        let text = sel.text_from_grid(&grid); // must not panic
        assert!(!text.is_empty(), "should still capture in-bounds content");

        let block = Selection::new(
            GridPos::new(0, 0),
            GridPos::new(99, 99),
            SelectionMode::Block,
        );
        let _ = block.text_from_grid(&grid); // must not panic
    }

    #[test]
    fn selection_ordered() {
        let sel = Selection::new(
            GridPos::new(3, 5),
            GridPos::new(1, 2),
            SelectionMode::Simple,
        );
        let (tl, br) = sel.ordered();
        assert_eq!(tl.row, 1);
        assert_eq!(tl.col, 2);
        assert_eq!(br.row, 3);
        assert_eq!(br.col, 5);
    }

    #[test]
    fn selection_contains() {
        let sel = Selection::new(
            GridPos::new(1, 2),
            GridPos::new(3, 5),
            SelectionMode::Simple,
        );
        assert!(sel.contains(1, 3));
        assert!(sel.contains(2, 0));
        assert!(!sel.contains(0, 0));
        assert!(!sel.contains(4, 0));
    }

    #[test]
    fn block_selection_contains() {
        let sel = Selection::new(GridPos::new(1, 2), GridPos::new(3, 5), SelectionMode::Block);
        assert!(sel.contains(2, 3));
        assert!(!sel.contains(2, 1));
        assert!(!sel.contains(2, 6));
    }

    #[test]
    fn handler_start_extend_end() {
        let mut handler = SelectionHandler::new();
        handler.start(GridPos::new(0, 0), SelectionMode::Simple);
        assert!(handler.selecting);
        handler.extend(GridPos::new(2, 5));
        handler.end();
        assert!(!handler.selecting);
        assert!(handler.selection.is_some());
    }

    #[test]
    fn handler_clear() {
        let mut handler = SelectionHandler::new();
        handler.start(GridPos::new(0, 0), SelectionMode::Simple);
        handler.clear();
        assert!(handler.selection.is_none());
        assert!(!handler.selecting);
    }

    #[test]
    fn clear_grid_selection_keeps_block_view_selection() {
        // v1.10.20: migration drops the grid half only; the fresh block
        // selection and the in-progress drag must survive.
        let mut handler = SelectionHandler::new();
        handler.start(GridPos::new(0, 0), SelectionMode::Simple);
        handler.start_block_view(BlockSelAnchor::new(None, 0, 2));
        handler.clear_grid_selection();
        assert!(handler.selection.is_none());
        assert!(handler.selecting, "drag continues in block space");
        assert!(handler.block_view_selection.is_some());
    }

    #[test]
    fn handler_block_lifecycle_anchor_based() {
        // v1.10.26: start/extend/clear work on content anchors; text() reads
        // through the current source (the anchor survives content rewrites —
        // see anchor_tests for the full regression).
        let mut h = SelectionHandler::new();
        h.start_block_view(BlockSelAnchor::new(None, 1, 0));
        assert!(h.selecting);
        assert!(h.block_view_selection.is_some());
        h.extend_block_view(BlockSelAnchor::new(None, 0, 3));
        h.end();
        assert!(!h.selecting);
        // Empty source: no lines to select.
        struct EmptySource;
        impl SelectionContentSource for EmptySource {
            fn line_count(&self, _block: Option<u64>) -> usize {
                0
            }
            fn line_text(&self, _block: Option<u64>, _line: usize) -> Option<&str> {
                None
            }
            fn block_order(&self) -> &[Option<u64>] {
                const ORDER: [Option<u64>; 1] = [None];
                &ORDER
            }
        }
        assert_eq!(h.block_view_text(&EmptySource), Some(String::new()));
        h.clear();
        assert!(h.block_view_selection.is_none());
    }

    #[test]
    fn char_index_at_col_honors_cjk_and_clamps() {
        assert_eq!(char_index_at_col("abc", 0), 0);
        assert_eq!(char_index_at_col("abc", 2), 2);
        assert_eq!(char_index_at_col("abc", 9), 3); // 超尾 clamp
        assert_eq!(char_index_at_col("中文x", 1), 0); // 点击中字右半 → 中
        assert_eq!(char_index_at_col("中文x", 2), 1); // 文
        assert_eq!(char_index_at_col("中文x", 4), 2); // x
        assert_eq!(char_index_at_col("e\u{301}x", 1), 2); // combining 不占列
    }

    #[test]
    fn pixel_x_to_char_index_accounts_for_command_indent() {
        let text = "docker ps";
        // 命令首行(indent=3):文本从 content_left + 3 列开始
        assert_eq!(
            pixel_x_to_char_index(text, 3.0 * 10.0 + 5.0, 0.0, 10.0, 3),
            0
        ); // 第 0 字符
        assert_eq!(
            pixel_x_to_char_index(text, 3.0 * 10.0 + 2.5 * 10.0, 0.0, 10.0, 3),
            2
        ); // 第 2 字符
           // 点击缩进区(chevron/"> " 上)→ saturating_sub → 0 → 命令开头
        assert_eq!(pixel_x_to_char_index(text, 8.0, 0.0, 10.0, 3), 0);
        // 续行(顶格,indent=0):直接从 content_left 开始
        assert_eq!(pixel_x_to_char_index(text, 5.0, 0.0, 10.0, 0), 0);
        // 非 foldable(indent=2)
        assert_eq!(
            pixel_x_to_char_index(text, 2.0 * 10.0 + 5.0, 0.0, 10.0, 2),
            0
        );
    }

    // ── v1.10.26: the anchor-model core tests live in `anchor_tests` ──────

    fn bv_row(kind: BlockViewRowKind, text: &str, y_top: f32, y_bottom: f32) -> BlockViewRow {
        BlockViewRow {
            kind,
            text: text.to_string(),
            block_id: None,
            y_top,
            y_bottom,
            line: None,
            chunk_char_offset: 0,
            indent_cols: 0,
        }
    }

    // v1.10.26: BlockViewRow remains the HIT-TEST row model (y bands + text +
    // block/line keys); the selection itself no longer snapshots rows. This
    // test pins the row predicates the hit-testing path relies on.
    #[test]
    fn bv_row_selectability_and_y_containment() {
        let output = bv_row(BlockViewRowKind::Output, "hello", 0.0, 20.0);
        assert!(output.is_selectable());
        assert!(output.contains_y(5.0));
        assert!(!output.contains_y(20.0)); // half-open band
        assert_eq!(output.text, "hello");
        let header = bv_row(BlockViewRowKind::Header, "~/proj", 40.0, 64.0);
        assert!(!header.is_selectable());
    }
}
