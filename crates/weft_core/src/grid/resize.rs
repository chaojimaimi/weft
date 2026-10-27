//! D4 resize protocol bodies (v1.13.13 FIX-F: moved verbatim out of
//! `grid/mod.rs` to keep it under the line budget, per the
//! vt/actions_print.rs "bodies moved verbatim" precedent). `Grid::resize`
//! and `resize_dims` stay in `mod.rs`; external consumers keep compiling
//! through the `crate::grid::GridRowMap` re-export.

use super::{Grid, Row};

impl Grid {
    /// Full reflow resize: the D4 protocol below. The thin facade stays next
    /// to `resize_impl` (moved together with the protocol body in v1.13.13
    /// FIX-F; `resize_dims` — the no-reflow variant — remains in `mod.rs`).
    pub fn resize(&mut self, new_rows: usize, new_cols: usize) {
        self.resize_impl(new_rows, new_cols);
    }

    /// D4 resize protocol (PLAN_S3 §二 D4, ported from Warp
    /// `grid/resize.rs::resize_storage`): push the whole viewport into flat
    /// storage without truncation, convert the cursor to a content offset,
    /// rebuild the row index at the new width (zero content copies), map the
    /// offset back to a position, pop the new viewport off the bottom of
    /// storage (window anchor = cursor logical line + document tail, see
    /// the `vp_start` note below), then apply the retention limit.
    ///
    /// This replaces the T2 bridge (materialize everything → legacy rewrap →
    /// re-encode, three full passes) with one `Index::rebuild`.
    pub(super) fn resize_impl(&mut self, new_rows: usize, new_cols: usize) -> GridRowMap {
        if new_rows == self.num_rows && new_cols == self.num_cols {
            return GridRowMap::identity();
        }
        let old_num_rows = self.num_rows;
        let sb_pre = self.scrollback.len();

        // ── D4-1/2: cursor state + push the whole viewport (no truncation).
        let cursor_row = self.cursor.row.min(old_num_rows.saturating_sub(1));
        let logical_col = if self.cursor.wrap_pending {
            self.num_cols
        } else {
            self.cursor.col.min(self.num_cols.saturating_sub(1))
        };
        let viewport_rows = std::mem::take(&mut self.viewport);
        let cursor_flat_target = sb_pre + cursor_row;
        let mut cursor_flat = sb_pre;
        let mut old_ends: Vec<usize> = Vec::with_capacity(sb_pre + old_num_rows);
        let mut old_wrapped: Vec<bool> = Vec::with_capacity(sb_pre + old_num_rows);
        for i in 0..sb_pre {
            old_ends.push(self.scrollback.content_range_end(i).unwrap_or(0));
            old_wrapped.push(self.scrollback.row_wraps(i));
        }
        // All-blank viewport rows collapse here, exactly as the legacy
        // reflow's `flush_line` dropped empty lines (unless the cursor sat
        // on them): otherwise blank-row debris accumulates in scrollback
        // across resize cycles. Collapsed rows record a zero-width entry in
        // the row map so mask/anchor indices stay aligned with their callers.
        for (idx, row) in viewport_rows.into_iter().enumerate() {
            let is_cursor_row = sb_pre + idx == cursor_flat_target;
            if !is_cursor_row && row_is_all_blank(&row) {
                // Collapsed rows still occupy a map entry (zero-width) —
                // old_ends/old_wrapped stay in FULL-DOCUMENT index space so
                // the ownership mask and anchor lookups key on the same
                // indices the callers hold.
                old_ends.push(old_ends.last().copied().unwrap_or(0));
                old_wrapped.push(row.wrapped);
                continue;
            }
            self.scrollback.push_without_truncation(&row);
            self.history_window_valid = false;
            old_ends.push(
                self.scrollback
                    .content_range_end(self.scrollback.len() - 1)
                    .unwrap_or(0),
            );
            old_wrapped.push(self.scrollback.row_wraps(self.scrollback.len() - 1));
            if sb_pre + idx < cursor_flat_target {
                cursor_flat += 1;
            }
        }

        // ── D4-3: cursor → content offset (anchored to the cell left of the
        // cursor so the position tracks its content through the re-wrap).
        let anchor = self.scrollback.cursor_anchor(cursor_flat, logical_col);

        // ── D4-4: re-wrap at the new width (content bytes untouched).
        self.scrollback.set_columns(new_cols);

        // ── Row map: old document rows (scrollback then viewport) → the
        // post-rebuild flat rows their content occupies. Content offsets are
        // stable, so a two-pointer walk over the per-row ranges is exact.
        let new_total = self.scrollback.len();
        let new_ends: Vec<usize> = (0..new_total)
            .map(|i| self.scrollback.content_range_end(i).unwrap_or(usize::MAX))
            .collect();
        let map = build_row_map(&old_ends, &new_ends);

        // ── D4-5: content offset → (row, col, wrap_pending).
        let (_, cursor_col_new, wrap_pending) =
            self.scrollback.cursor_point_from_anchor(anchor, new_cols);

        // Document-space walk-back: the map (and every downstream consumer —
        // ownership mask, frozen boundary) is keyed by document row, not by
        // the post-collapse pushed sequence.
        let mut line_first = sb_pre + cursor_row;
        while line_first > 0 && old_wrapped[line_first - 1] {
            line_first -= 1;
        }
        let line_new_start = map.get(line_first).copied().map_or(0, |(first, _)| first);
        // v1.13.12 FIX-A: the old `min(L-(nr-1), T-nr)` pinned the cursor
        // logical line's FIRST row to the viewport bottom and the pop-back
        // dropped every row after it — non-blank rows BELOW the cursor
        // (`CSI A` progress-bar frames) and wrap continuations included
        // (blank rows below the cursor collapse at push time above).
        // `min(L, T-nr)` = reflow 型终端（kitty/WezTerm 类）通用语义:
        // (a) tail fits (B+k < nr) → window at the document tail, rows
        // below the cursor kept; (b) else cursor line at TOP, rows below
        // fall off the bottom edge. Prompt shape (k=1, B=0, T=L+1):
        // min(L, L+1-nr) = L+1-nr — the old value exactly, point-identical.
        let vp_start = line_new_start.min(new_total.saturating_sub(new_rows));
        let mut popped = self.scrollback.pop_rows(new_total.saturating_sub(vp_start));
        let popped_real = new_rows.min(popped.len());
        let mut viewport_rows: Vec<Row> = popped.drain(..popped_real).collect();
        if viewport_rows.len() < new_rows {
            viewport_rows.resize(new_rows, Row::new(new_cols));
        }
        self.viewport = viewport_rows;

        // ── Dimensions, cursor, and the retention limit.
        self.num_rows = new_rows;
        self.num_cols = new_cols;
        self.scroll_bottom = new_rows.saturating_sub(1);
        self.scroll_top = 0;
        self.tabstops = Self::init_tabstops(new_cols);

        self.cursor.row = line_new_start
            .saturating_sub(vp_start)
            .min(new_rows.saturating_sub(1));
        self.cursor.col = cursor_col_new.min(new_cols.saturating_sub(1));
        self.cursor.wrap_pending = wrap_pending;
        self.set_scroll_offset(0);
        // v1.0 P0-b: all rows are new/rearranged after a reflow.
        self.mark_all_dirty();
        self.scrollback.apply_max_rows();

        GridRowMap {
            old_row_new_range: map,
            flat_rows: vp_start,
            popped: popped_real,
            viewport_rows: new_rows,
        }
    }
}

/// The legacy reflow's blank-line predicate (`flush_line`): every cell
/// is a never-written default. Written blanks (DIRTY) keep their row.
/// v1.15.3 P1a: `occ == 0` is the same predicate by contract — read O(1)
/// instead of scanning the row.
fn row_is_all_blank(row: &Row) -> bool {
    row.occ == 0
}

/// Old row `i` (of `old_ends.len()`) maps to the post-rebuild rows
/// `[first, last]` — the rows containing its first and last byte. Both
/// range lists partition the same content stream in order. Collapsed
/// zero-byte rows anchor at the row containing their position. Stateless
/// per-row partition (no carry-over state to go stale across rows).
fn build_row_map(old_ends: &[usize], new_ends: &[usize]) -> Vec<(usize, usize)> {
    if old_ends.is_empty() || new_ends.is_empty() {
        return Vec::new();
    }
    // First new row whose end lies past `byte` — the row containing it.
    let row_containing = |byte: usize| -> usize {
        let candidate = new_ends.partition_point(|&end| end <= byte);
        candidate.min(new_ends.len() - 1)
    };
    let mut map = Vec::with_capacity(old_ends.len());
    let mut prev_end = 0usize;
    for &end in old_ends {
        if end == prev_end {
            let at = row_containing(prev_end);
            map.push((at, at));
        } else {
            let first = row_containing(prev_end);
            let last = row_containing(end - 1);
            map.push((first, last));
        }
        prev_end = end;
    }
    map
}

/// Old→new row mapping computed by the D4 resize protocol: for each
/// pre-resize document row (scrollback rows first, then viewport rows), the
/// inclusive post-rebuild flat-row range its content occupies.
pub(crate) struct GridRowMap {
    pub old_row_new_range: Vec<(usize, usize)>,
    /// Flat rows remaining after the viewport pop-back.
    pub flat_rows: usize,
    /// Rows popped back into the viewport (before blank padding).
    pub popped: usize,
    /// The new viewport height — mask consumers pad up to it.
    pub viewport_rows: usize,
}

impl GridRowMap {
    pub(crate) fn identity() -> Self {
        Self {
            old_row_new_range: Vec::new(),
            flat_rows: 0,
            popped: 0,
            viewport_rows: 0,
        }
    }
}
