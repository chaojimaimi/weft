//! Row-snapshot refresh for a live `BlockViewSelection` (v1.10.25 Batch 3).
//!
//! The renderer layouts only the visible window ± overscan each frame, so the
//! per-frame row set fed to `sync_rows` is a CULLED set. The snapshot must not
//! be overwritten wholesale by it: rows that scrolled out of the layout window
//! but still lie inside the selection's `[start..end]` identity range would
//! otherwise vanish and `text()` would silently shrink. This module merges the
//! culled set with the retained scrolled-off rows so a cross-viewport selection
//! keeps its full span for `text()`/identity (the off-screen rows never render —
//! they only serve text extraction and endpoint anchoring).
//!
//! ## Coordinate-space invariant (v1.10.25, B1)
//!
//! The app feeds y in the CURRENT frame's screen space (`content_bottom_y -
//! dist + scroll_px`, `paint/block_view/rows.rs`), which translates as a block
//! whenever the view scrolls (`scroll_px`) or the content band resizes
//! (`content_bottom_y`). Retained rows froze at the PREVIOUS frame's coordinates,
//! so sorting them raw against `new_rows` mixes two coordinate systems and can
//! invert adjacent document rows once the frame moved ≥1 row pitch (a scrolled
//! row's stale y may land above/below a fresh row that belongs on the other side
//! of it). `sync_rows` therefore measures the per-row shift of every old row
//! exactly present in the new set (median, robust to single-row sub-pixel
//! noise), normalizes retained rows into the current frame by that delta, and
//! only then runs ONE sort — inside a single frame y IS document order, so the
//! merged list reads top-to-bottom correctly (newly scrolled-in rows carry their
//! own current-frame y and need no adjustment).

use super::{BlockViewRow, BlockViewSelection};

impl BlockViewSelection {
    /// v1.10.25 Batch 3: refresh the row snapshot and remap `start`/`end`
    /// row_index, merging rather than overwriting.
    ///
    /// New rows update in place as before; old snapshot rows inside the
    /// selection identity range `[start..end]` whose identity the new (culled)
    /// set does not carry are RETAINED and merged. Retention matches the exact
    /// key (kind, block_id, text); an old row whose exact identity is gone but
    /// whose (kind, text) survives in `new_rows` (e.g. a renumbered block) is
    /// REPLACED by that row, never kept alongside it (no duplicated text).
    ///
    /// Retained rows are translated into the current frame's y coordinate
    /// system by the measured frame delta (median exact-match shift; the last
    /// computed delta on zero-match frames), so the merged sort runs in ONE
    /// coordinate system where y is document order. Endpoints whose identity
    /// survives stay anchored to it and only fall back to the closest y-center
    /// (translated by the same delta) when the retained set loses it too.
    pub fn sync_rows(&mut self, new_rows: Vec<BlockViewRow>) {
        let old_rows = std::mem::take(&mut self.rows);
        let old_len = old_rows.len();
        if old_len == 0 {
            let last = new_rows.len().saturating_sub(1);
            self.start.row_index = self.start.row_index.min(last);
            self.end.row_index = self.end.row_index.min(last);
            self.rows = new_rows;
            self.frame_delta = 0.0;
            return;
        }
        // Selection identity range in the OLD snapshot.
        let lo = self
            .start
            .row_index
            .min(self.end.row_index)
            .min(old_len - 1);
        let hi = self
            .start
            .row_index
            .max(self.end.row_index)
            .min(old_len - 1);
        // Identity rules mirror the app-side retention (`selected_row_identities`
        // matches (kind, block_id, text) exactly): membership uses the exact
        // key; the legacy endpoint remap additionally falls back to (kind, text)
        // for LiveCommand-style rows without an id.
        let exact_match = |a: &BlockViewRow, b: &BlockViewRow| -> bool {
            a.kind == b.kind && a.block_id == b.block_id && a.text == b.text
        };
        let text_fallback =
            |a: &BlockViewRow, b: &BlockViewRow| -> bool { a.kind == b.kind && a.text == b.text };
        let exact_idx = |rows: &[BlockViewRow], target: &BlockViewRow| -> Option<usize> {
            rows.iter().position(|row| exact_match(row, target))
        };
        let text_idx = |rows: &[BlockViewRow], target: &BlockViewRow| -> Option<usize> {
            rows.iter().position(|row| text_fallback(row, target))
        };
        // v1.10.25 (B1): measure the per-row frame displacement for every old
        // row exactly present in the new set. All laid-out rows shift by the
        // same delta between frames (scroll_px and content_bottom_y are
        // frame-constant), so the median decays single-row sub-pixel noise; a
        // zero-match frame (jump scroll) reuses the last delta so the frozen
        // block stays aligned with the current frame.
        let mut deltas: Vec<f32> = old_rows
            .iter()
            .filter_map(|old| exact_idx(&new_rows, old).map(|ni| new_rows[ni].y_top - old.y_top))
            .collect();
        let frame_delta = if deltas.is_empty() {
            self.frame_delta
        } else {
            deltas.sort_by(|a, b| a.total_cmp(b));
            let mid = deltas.len() / 2;
            if deltas.len() % 2 == 0 {
                (deltas[mid - 1] + deltas[mid]) * 0.5
            } else {
                deltas[mid]
            }
        };
        self.frame_delta = frame_delta;
        // Keep old selection-range rows the new set no longer carries. An old
        // row is dropped when the new set already has its exact key OR a
        // (kind, text) twin — the twin (e.g. a renumbered block's row)
        // replaces it in the merged set, so `text()` never repeats a line.
        let retained: Vec<BlockViewRow> = old_rows[lo..=hi]
            .iter()
            .filter(|old| exact_idx(&new_rows, old).is_none() && text_idx(&new_rows, old).is_none())
            .cloned()
            .map(|mut row| {
                row.y_top += frame_delta;
                row.y_bottom += frame_delta;
                row
            })
            .collect();
        // Merge (stable sort; equal-y new rows stay ahead of retained ones)
        // and sort by normalized y — inside one frame y IS document order, so
        // the reading order survives without relying on stale coordinates.
        let mut merged = new_rows;
        merged.extend(retained);
        merged.sort_by(|a, b| {
            a.y_top
                .total_cmp(&b.y_top)
                .then(a.y_bottom.total_cmp(&b.y_bottom))
        });
        let remap = |old_idx: usize| -> usize {
            let old_row = old_rows.get(old_idx);
            // The merged set contains every old selection-range row via
            // `retained` (or its (kind, text) twin from `new_rows`), so a hit
            // means the endpoint stays anchored to its own row (present row or
            // off-screen survivor) — no fallback.
            if let Some(hit) =
                old_row.and_then(|old| exact_idx(&merged, old).or_else(|| text_idx(&merged, old)))
            {
                return hit;
            }
            // Identity lost even from the retained set: y-center fallback (the
            // endpoint's own y is shifted by this frame's delta first, so the
            // closest pick runs in the same coordinate system as `merged`).
            let Some(old) = old_row else {
                return merged.len().saturating_sub(1);
            };
            let old_yc = (old.y_top + old.y_bottom) * 0.5 + frame_delta;
            let mut best = 0usize;
            let mut best_d = f32::MAX;
            for (i, row) in merged.iter().enumerate() {
                let yc = (row.y_top + row.y_bottom) * 0.5;
                let d = (yc - old_yc).abs();
                if d < best_d {
                    best_d = d;
                    best = i;
                }
            }
            best
        };
        self.start.row_index = remap(self.start.row_index);
        self.end.row_index = remap(self.end.row_index);
        self.rows = merged;
    }
}

#[cfg(test)]
mod tests {
    use super::super::{BlockViewPos, BlockViewRowKind};
    use super::*;
    use crate::blocks::BlockId;

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

    fn bv_rows() -> Vec<BlockViewRow> {
        // 5 rows, index 0 = bottom. Each row 20px tall.
        vec![
            bv_row(BlockViewRowKind::Output, "world", 0.0, 20.0), // idx 0 (bottom)
            bv_row(BlockViewRowKind::Output, "hello", 20.0, 40.0), // idx 1
            bv_row(BlockViewRowKind::Separator, "", 40.0, 60.0),  // idx 2 (skipped)
            bv_row(BlockViewRowKind::Command, "echo hi", 60.0, 80.0), // idx 3
            bv_row(BlockViewRowKind::Header, "~/proj", 80.0, 100.0), // idx 4 (top)
        ]
    }

    // v1.10.25 Batch 3 (FIX_SELECTION_AND_RESIZE_REMAINING): a cross-viewport
    // block selection (span wider than the visible window) must survive the
    // per-frame row cull. `sync_rows` used to overwrite the snapshot
    // wholesale with the culled set (`self.rows = new_rows`), so rows that
    // scrolled out of the layout window but still inside the selection range
    // vanished and `text()` silently shrank to the visible fragment.
    #[test]
    fn sync_rows_merge_keeps_cross_viewport_selection_text() {
        // 8-row document, idx 0 = bottom; selection spans idx 7 down to idx 2.
        let doc: Vec<BlockViewRow> = (0..8)
            .map(|i| {
                bv_row(
                    BlockViewRowKind::Output,
                    &format!("row{i}"),
                    i as f32 * 20.0,
                    i as f32 * 20.0 + 20.0,
                )
            })
            .collect();
        let full_text = "row7\nrow6\nrow5\nrow4\nrow3\nrow2";
        let mut sel = BlockViewSelection {
            start: BlockViewPos {
                row_index: 7,
                char_index: 0,
            },
            end: BlockViewPos {
                row_index: 2,
                char_index: 4,
            },
            rows: doc.clone(),
            frame_delta: 0.0,
        };
        assert_eq!(sel.text(), full_text);

        // Frame 1: scrolled so only rows 5..=7 are laid out (the selection's
        // top tail remains visible, its bottom rows are off-screen below the
        // window). The culled set no longer carries rows 2..=4.
        sel.sync_rows(doc[5..=7].to_vec());
        assert_eq!(sel.text(), full_text, "bottom half must survive first cull");

        // Frame 2: scrolled further — the window covers rows 0..=1, no
        // selection row remains in the culled set.
        sel.sync_rows(doc[0..=1].to_vec());
        assert_eq!(sel.text(), full_text, "full span must survive total cull");

        // Frame 3: pre-materialize empty layout — endpoints must still anchor
        // to the retained rows, not degenerate.
        sel.sync_rows(Vec::new());
        assert_eq!(sel.text(), full_text, "endpoints must stay anchored");
    }

    // sync_rows fallback tests: culled rows may be absent from `new_rows`.

    #[test]
    fn sync_rows_clipped_row_stays_anchored_instead_of_y_center_fallback() {
        // Old idx 4 (Header "~/proj") is absent from new_rows but still
        // inside the selection range [3..4], so it is retained — the endpoint
        // stays anchored instead of falling back to the closest y-center.
        let old_rows = bv_rows();
        let mut sel = BlockViewSelection {
            start: BlockViewPos {
                row_index: 4,
                char_index: 0,
            },
            end: BlockViewPos {
                row_index: 3,
                char_index: 5,
            },
            rows: old_rows,
            frame_delta: 0.0,
        };
        let new_rows = vec![
            bv_row(BlockViewRowKind::Output, "world", 0.0, 20.0), // new idx 0
            bv_row(BlockViewRowKind::Output, "hello", 20.0, 40.0), // new idx 1
            bv_row(BlockViewRowKind::Command, "echo hi", 60.0, 80.0), // new idx 2
        ];
        sel.sync_rows(new_rows);
        // end ("echo hi") exactly matches the new Command at merged idx 2;
        // start ("~/proj" Header) is retained and sits above it at idx 3.
        assert_eq!(sel.end.row_index, 2);
        assert_eq!(sel.start.row_index, 3);
        assert_eq!(sel.rows.len(), 4); // merged = new set + retained Header
    }

    #[test]
    fn sync_rows_all_clipped_keeps_entire_selection_range() {
        // Every selection row offscreen or unmatchable: the whole old range
        // is retained; endpoints must stay anchored, never panic or clamp
        // to a random row.
        let old_rows = bv_rows();
        let mut sel = BlockViewSelection {
            start: BlockViewPos {
                row_index: 4,
                char_index: 0,
            },
            end: BlockViewPos {
                row_index: 0,
                char_index: 3,
            },
            rows: old_rows,
            frame_delta: 0.0,
        };
        let new_rows = vec![
            bv_row(BlockViewRowKind::Output, "completely_new", 200.0, 220.0),
            bv_row(BlockViewRowKind::Output, "also_new", 220.0, 240.0),
        ];
        sel.sync_rows(new_rows);
        // start "~/proj" Header → retained idx 4; end "world" → retained idx 0.
        assert_eq!(sel.start.row_index, 4);
        assert_eq!(sel.end.row_index, 0);
        assert!(sel.start.row_index < sel.rows.len());
        assert!(sel.end.row_index < sel.rows.len());
    }

    #[test]
    fn sync_rows_empty_new_rows_keeps_selection_rows_anchored() {
        // Empty new_rows (pre-layout first frame): the selection range is
        // fully retained, so endpoints stay pinned to their own rows.
        let old_rows = bv_rows();
        let mut sel = BlockViewSelection {
            start: BlockViewPos {
                row_index: 2,
                char_index: 0,
            },
            end: BlockViewPos {
                row_index: 0,
                char_index: 3,
            },
            rows: old_rows,
            frame_delta: 0.0,
        };
        sel.sync_rows(Vec::new());
        assert_eq!(sel.end.row_index, 0); // "world" retained at idx 0
        assert_eq!(sel.start.row_index, 2); // Separator retained at idx 2
        assert_eq!(sel.rows.len(), 3);
    }

    #[test]
    fn sync_rows_with_no_prior_snapshot_clamps_plainly() {
        // First sync (no old rows): plain assignment, indices clamp to the
        // new set with no retention bookkeeping.
        let mut sel = BlockViewSelection {
            start: BlockViewPos {
                row_index: 7,
                char_index: 0,
            },
            end: BlockViewPos {
                row_index: 0,
                char_index: 3,
            },
            rows: Vec::new(),
            frame_delta: 0.0,
        };
        let new_rows = vec![bv_row(BlockViewRowKind::Output, "only", 0.0, 20.0)];
        sel.sync_rows(new_rows);
        assert_eq!(sel.start.row_index, 0);
        assert_eq!(sel.end.row_index, 0);
        assert_eq!(sel.rows.len(), 1);
    }

    fn bv_row_with_id(
        kind: BlockViewRowKind,
        text: &str,
        y_top: f32,
        y_bottom: f32,
        id: u64,
    ) -> BlockViewRow {
        let mut row = bv_row(kind, text, y_top, y_bottom);
        row.block_id = Some(BlockId(id));
        row
    }

    // v1.10.25 (B1, rust-reviewer blocker): the app feeds sync_rows y in the
    // CURRENT frame's screen space (`content_bottom_y - dist + scroll_px`,
    // paint/block_view/rows.rs) — every laid-out row translates as a block
    // when the view scrolls or the content band resizes. Retained (off-band)
    // rows freeze at the previous frame's y, so a raw y-sort mixed coordinate
    // systems and, once the frame moved ≥1 row pitch, interleaved adjacent
    // document rows out of order. This regression drives the same uniform
    // per-frame translation the live layout produces: a ≥2-row shift (Δ=50,
    // 2.5 rows) with a middle band culled (rows 4..=8 fresh, the rest frozen),
    // then a resize shift (Δ=30), then a zero-exact-match frame that must fall
    // back to the last delta — `text()` must keep exact document order the
    // whole way. Red before the fix (frame 1 sorted ...6,9,7,10,8,11...).
    #[test]
    fn sync_rows_scrolled_frames_keep_document_order_in_text() {
        const PITCH: f32 = 20.0;
        // 12-row document, idx 0 = bottom; row i sits at y = i*20 in the
        // initial (frame 0) coordinate system.
        let doc: Vec<BlockViewRow> = (0..12)
            .map(|i| {
                bv_row(
                    BlockViewRowKind::Output,
                    &format!("row{i}"),
                    i as f32 * PITCH,
                    i as f32 * PITCH + PITCH,
                )
            })
            .collect();
        let full_text = "row11\nrow10\nrow9\nrow8\nrow7\nrow6\nrow5\nrow4\nrow3\nrow2\nrow1\nrow0";
        let mut sel = BlockViewSelection {
            start: BlockViewPos {
                row_index: 11,
                char_index: 0,
            },
            end: BlockViewPos {
                row_index: 0,
                char_index: 6,
            },
            rows: doc.clone(),
            frame_delta: 0.0,
        };
        assert_eq!(sel.text(), full_text);

        // Frame 1: the frame translates +50px (scroll/resize, 2.5 row pitches)
        // and the layout window materializes only the middle band rows 4..=8;
        // rows 0..=3 and 9..=11 are off-band and freeze at their frame-0 y.
        sel.sync_rows(
            (4..9)
                .map(|i| {
                    bv_row(
                        BlockViewRowKind::Output,
                        &format!("row{i}"),
                        i as f32 * PITCH + 50.0,
                        i as f32 * PITCH + 50.0 + PITCH,
                    )
                })
                .collect(),
        );
        assert_eq!(
            sel.text(),
            full_text,
            "a ≥2-row frame shift must not reorder frozen rows against the new band"
        );

        // Frame 2: a resize shifts content_bottom_y by +30 more (uniform
        // frame translation again); the visible band rows carry their new y.
        sel.sync_rows(
            (4..9)
                .map(|i| {
                    bv_row(
                        BlockViewRowKind::Output,
                        &format!("row{i}"),
                        i as f32 * PITCH + 80.0,
                        i as f32 * PITCH + 80.0 + PITCH,
                    )
                })
                .collect(),
        );
        assert_eq!(
            sel.text(),
            full_text,
            "a content_bottom_y change must keep normalized order"
        );

        // Frame 3: zero exact matches (entirely new in-band rows) — retention
        // falls back to the last frame delta; the frozen document rows must
        // keep their ascending index order in the merged snapshot.
        sel.sync_rows(
            (4..9)
                .map(|i| {
                    bv_row(
                        BlockViewRowKind::Output,
                        &format!("newrow{i}"),
                        i as f32 * PITCH + 120.0,
                        i as f32 * PITCH + 120.0 + PITCH,
                    )
                })
                .collect(),
        );
        let texts: Vec<&str> = sel.rows.iter().map(|r| r.text.as_str()).collect();
        for i in 1..12 {
            let prev_idx = texts
                .iter()
                .position(|t| *t == format!("row{}", i - 1))
                .expect("doc row must remain in the snapshot");
            let cur_idx = texts
                .iter()
                .position(|t| *t == format!("row{i}"))
                .expect("doc row must remain in the snapshot");
            assert!(
                prev_idx < cur_idx,
                "zero-match frame must keep frozen rows in document order ({i})"
            );
        }
    }

    // v1.10.25 (S1, rust-reviewer should-fix): retention used to check the
    // EXACT key only, while the endpoint remap additionally fell back to
    // (kind, text). When a block was renumbered (same kind+text, new id), the
    // old frozen rows were retained AND the renumbered rows entered via
    // `new_rows` — the merged set then carried the same line twice and
    // `text()` duplicated it. Retention now also treats a (kind, text) twin
    // in the new set as a replacement, so the renumbered rows win.
    #[test]
    fn sync_rows_renumbered_block_does_not_duplicate_text() {
        let old_rows = vec![
            bv_row_with_id(BlockViewRowKind::Output, "dup", 0.0, 20.0, 1),
            bv_row_with_id(BlockViewRowKind::Output, "keep", 20.0, 40.0, 1),
            bv_row_with_id(BlockViewRowKind::Command, "echo", 40.0, 60.0, 1),
        ];
        let mut sel = BlockViewSelection {
            start: BlockViewPos {
                row_index: 2,
                char_index: 0,
            },
            end: BlockViewPos {
                row_index: 0,
                char_index: 3,
            },
            rows: old_rows,
            frame_delta: 0.0,
        };
        assert_eq!(sel.text(), "echo\nkeep\ndup");
        // Same rows, renumbered block id 2: exact identity fails, the
        // (kind, text) twin must REPLACE each old row, not join it.
        let renumbered = vec![
            bv_row_with_id(BlockViewRowKind::Output, "dup", 0.0, 20.0, 2),
            bv_row_with_id(BlockViewRowKind::Output, "keep", 20.0, 40.0, 2),
            bv_row_with_id(BlockViewRowKind::Command, "echo", 40.0, 60.0, 2),
        ];
        sel.sync_rows(renumbered);
        assert_eq!(
            sel.rows.len(),
            3,
            "renumbered twins must replace the frozen rows, not add to them"
        );
        assert_eq!(sel.text(), "echo\nkeep\ndup", "no duplicated lines");
        assert_eq!(sel.start.row_index, 2, "endpoint anchors to the twin");
        assert_eq!(sel.end.row_index, 0);
    }
}
