//! Grid paint model: block layout cache + line wrapping (A5 / M4 step 1).
//!
//! Finished blocks have immutable `output` (it's a detached snapshot), so
//! the per-line wrapping computation only needs to run once per block —
//! unless `cols` changes (resize) or the block's content/collapse state
//! changes. This cache eliminates the O(total_output_chars) per-frame cost
//! reintroduced when the `MAX_LAYOUT_LINES` cap was removed from historical
//! blocks.
//!
//! The live in-flight block is NOT cached (its output streams every frame).

use std::collections::HashMap;
use std::ops::Range;
use std::rc::Rc;

use weft_core::blocks::Block;

mod visual_rows;
pub(crate) use visual_rows::{completed_output_rows, trimmed_output_line_count};
mod wrapping;
pub(crate) use wrapping::{
    block_line_chunk_ranges, block_line_chunks, command_line_chunks,
    screen_origin_line_chunk_ranges, screen_origin_line_chunks,
};

pub(crate) const MAX_LAYOUT_LINES_LIVE: usize = 2000;

/// Pre-computed wrapping data for a single output line of a block.
#[derive(Clone)]
pub(crate) struct CachedLine {
    /// 0-based line index within the block's output (before trimming).
    pub(crate) idx: usize,
    /// Byte offset of this line's start within `block.output`.
    pub(crate) byte_start: usize,
    /// Byte offset of this line's end (exclusive) within `block.output`.
    pub(crate) byte_end: usize,
    /// Source-relative byte ranges for pre-wrapped chunks. The block's
    /// immutable output remains the sole owner of text bytes; visible rows
    /// materialize short strings only for the current frame.
    pub(crate) chunk_ranges: Rc<[Range<usize>]>,
}

/// Cached layout for a single finished block.
#[derive(Clone)]
pub(crate) struct CachedBlockLayout {
    /// Snapshot of `block.output.len()` — if the current block's output
    /// length differs, the cache is stale.
    pub(crate) output_len: usize,
    /// Identity of the immutable output allocation. Continuation updates can
    /// replace a block's output with equal-length text while retaining its
    /// BlockId; length alone would then reuse stale byte ranges and chunks.
    pub(crate) output_identity: usize,
    /// Snapshot of `block.command.len()`.
    pub(crate) command_len: usize,
    /// Snapshot of `block.collapsed` — toggling invalidates.
    pub(crate) collapsed: bool,
    /// `cols` used to compute wrapping — resize invalidates.
    pub(crate) cols: usize,
    /// Whether the block has any non-empty output lines (cached foldable
    /// check, avoids re-scanning the last 500 lines every frame).
    pub(crate) foldable: bool,
    /// Pre-trimmed, pre-wrapped line metadata. Trailing empty/prompt lines
    /// are already removed, matching the original trimming logic.
    pub(crate) lines: Vec<CachedLine>,
    /// R2-2: cached wrapped output row count (sum of chunks.len() across
    /// all lines + command resume hints). Excludes the header/gap/spacer
    /// because those depend on `header_rows` and `viewport_rows` which are
    /// per-frame parameters, not per-block cache keys. This lets
    /// `block_content_metrics` skip the O(m) re-wrap per block and read O(1).
    pub(crate) output_rows: usize,
    /// R2-2 (Batch 7): base row count for prefix-sum = hint_rows +
    /// output_rows + 2 (command + separator). Excludes clear_rows (depends
    /// on per-frame viewport_rows) and header_height (per-frame). Used by
    /// `BlockLayoutCache::build_prefix_sum` for O(log n) binary search to
    /// locate the first visible block, eliminating O(n) traversal of
    /// offscreen blocks.
    pub(crate) base_row_count: usize,
    /// Whether the command is a bare `clear` (produces a viewport-sized spacer).
    /// Cached so the clear-prefix-sum can count clear blocks in O(1) without
    /// putting per-frame `viewport_rows` into the cache key.
    pub(crate) is_clear: bool,
    /// R2-2 (stage 2): wrapped row count of the (prompt-stripped) command.
    /// First line uses `cols - indent` (indent=3 if foldable else 2),
    /// continuation uses `cols`. Collapsed blocks report 1 (command stays
    /// single-line). Same source as layout_pass's Command construction so
    /// `base_row_count` (prefix sum) and `block_total_height` can't drift.
    pub(crate) command_wrap_rows: usize,
    /// M5-a (PLAN_M5 §二): L1 content layer (cols-independent, output-keyed).
    pub(crate) content: visual_rows::ContentTable,
    /// M5-a (PLAN_M5 §二): L2 width layer; `hint_rows + rows.len()` = O(1) rows.
    pub(crate) width: visual_rows::WidthTable,
}

/// Per-renderer block layout cache. Keyed by `BlockId.0`.
#[derive(Default)]
pub(crate) struct BlockLayoutCache {
    entries: HashMap<u64, CachedBlockLayout>,
    /// Step 1: cache hit/miss counters for frame_trace observability.
    /// A "hit" is an `ensure_cached` call that found a fresh entry;
    /// a "miss" is one that triggered a rebuild. Reset by
    /// `take_hit_miss_counts` at the end of each frame so the trace
    /// reports per-frame deltas, not cumulative totals.
    hits: usize,
    misses: usize,
    /// v1.10.23: monotonic rebuild count (misses since creation) — the
    /// fingerprint for the `block_scroll_metrics` memo: finished-blocks
    /// metrics change iff some cache entry is rebuilt.
    misses_total: u64,
    /// R2-2 (Batch 7): prefix sum of `base_row_count`, indexed from the
    /// newest block. `prefix_sum[0] = 0`, `prefix_sum[i]` = sum of
    /// `base_row_count` for the i newest blocks. Enables O(log n) binary
    /// search to locate the first visible block, eliminating O(n)
    /// traversal of offscreen blocks.
    prefix_sum: Vec<usize>,
    /// R2-2 fix: prefix sum of clear-block count, parallel to `prefix_sum`.
    /// `clear_prefix_sum[i]` = number of clear blocks among the i newest.
    /// Multiplied by `viewport_rows * pitch` per-frame to get total clear
    /// spacer height — kept out of `base_row_count` because viewport_rows
    /// is per-frame.
    clear_prefix_sum: Vec<usize>,
    /// Block IDs from the last `build_prefix_sum` call, in newest-to-oldest
    /// order. Used to detect block set changes (add/remove) that invalidate
    /// the prefix sum.
    prefix_sum_block_ids: Vec<u64>,
    /// Set true when any `ensure_cached` call triggers a rebuild (cache
    /// miss), which may change a `base_row_count`. Cleared by
    /// `build_prefix_sum` after rebuild.
    prefix_sum_dirty: bool,
    synced_cols: Option<usize>,
    synced_len: usize,
    synced_first_id: Option<u64>,
    synced_last_id: Option<u64>,
    dirty_ids: Vec<u64>,
}

impl BlockLayoutCache {
    /// Synchronize the append-only block history incrementally. Scrolling
    /// frames check only the newest block; append, resize and explicit
    /// invalidation rebuild affected entries before refreshing the prefix sum.
    pub(crate) fn sync_blocks(&mut self, blocks: &[Block], cols: usize) {
        let first_id = blocks.first().map(|block| block.id.0);
        let last_id = blocks.last().map(|block| block.id.0);
        let append_only = self.synced_cols == Some(cols)
            && blocks.len() >= self.synced_len
            && self.synced_first_id == first_id
            && (self.synced_len == 0
                || blocks.get(self.synced_len - 1).map(|block| block.id.0) == self.synced_last_id);

        if append_only {
            for block in blocks.iter().skip(self.synced_len) {
                self.ensure_cached(block, cols);
            }
            if blocks.len() == self.synced_len {
                if let Some(block) = blocks.last() {
                    self.ensure_cached(block, cols);
                }
            }
        } else {
            for block in blocks {
                self.ensure_cached(block, cols);
            }
        }

        for id in std::mem::take(&mut self.dirty_ids) {
            if let Some(block) = blocks.iter().find(|block| block.id.0 == id) {
                self.ensure_cached(block, cols);
            }
        }
        self.synced_cols = Some(cols);
        self.synced_len = blocks.len();
        self.synced_first_id = first_id;
        self.synced_last_id = last_id;
        self.build_prefix_sum(blocks);
    }

    pub(crate) fn invalidate(&mut self, id: u64) {
        self.entries.remove(&id);
        if !self.dirty_ids.contains(&id) {
            self.dirty_ids.push(id);
        }
        self.prefix_sum_dirty = true;
        // v1.10.23: invalidate changes what the metrics fallback computes
        // (get_if_cached → None → direct), so it must bump the memo
        // fingerprint even before the entry is rebuilt.
        self.misses_total = self.misses_total.wrapping_add(1);
    }

    /// Ensure `block` has a cached layout for `cols`. Recomputes only if
    /// the block is new, its output/command changed, `collapsed` was
    /// toggled, or `cols` changed (resize).
    pub(crate) fn ensure_cached(&mut self, block: &Block, cols: usize) {
        let id = block.id.0;
        let needs_rebuild = match self.entries.get(&id) {
            None => true,
            Some(c) => {
                c.output_len != block.output.len()
                    || c.output_identity != block.output.as_ptr() as usize
                    || c.command_len != block.command.len()
                    || c.collapsed != block.collapsed
                    || c.cols != cols
            }
        };
        if needs_rebuild {
            self.misses += 1;
            self.misses_total = self.misses_total.wrapping_add(1);
            self.prefix_sum_dirty = true;
            self.entries.insert(id, compute_block_layout(block, cols));
        } else {
            self.hits += 1;
        }
    }

    /// v1.10.23: monotonic rebuild count. `block_scroll_metrics` memoizes
    /// on this — the finished-blocks metrics only change when an entry is
    /// rebuilt (base_row_count/output_rows/command_wrap_rows/is_clear), so a
    /// fingerprint of (rebuilds + block set) is exact.
    pub(crate) fn total_misses(&self) -> u64 {
        self.misses_total
    }

    pub(crate) fn get(&self, id: u64) -> &CachedBlockLayout {
        self.entries
            .get(&id)
            .expect("ensure_cached must be called before get")
    }

    /// R2-2: like `get` but returns `None` for uncached blocks instead of
    /// panicking. Used by `block_content_metrics_with_cache` to fall back
    /// to direct computation when a block hasn't been cached yet (e.g. the
    /// block was finalized between the last paint and the scrollbar layout).
    pub(crate) fn get_if_cached(&self, id: u64) -> Option<&CachedBlockLayout> {
        self.entries.get(&id)
    }

    /// Step 1: drain the per-frame hit/miss counters for frame_trace.
    /// Returns `(hits, misses)` accumulated since the last call and resets
    /// the internal accumulators. Call this once per frame at build_end.
    pub(crate) fn take_hit_miss_counts(&mut self) -> (usize, usize) {
        let h = self.hits;
        let m = self.misses;
        self.hits = 0;
        self.misses = 0;
        (h, m)
    }

    /// R2-2 (Batch 7): Build the prefix sum array for O(log n) binary
    /// search. Must be called after all blocks have been `ensure_cached`
    /// and before `prefix_sum()` is read.
    ///
    /// Rebuilds when the append-only block set changes or any cache entry was
    /// rebuilt (base_row_count may have changed). Reordering an existing
    /// history is outside this cache's contract; callers replacing history
    /// must start with a fresh cache.
    pub(crate) fn build_prefix_sum(&mut self, blocks: &[Block]) {
        // Detect block set changes by comparing IDs (newest-to-oldest).
        let ids_changed = self.prefix_sum_block_ids.len() != blocks.len()
            || self.prefix_sum_block_ids.first().copied() != blocks.last().map(|block| block.id.0)
            || self.prefix_sum_block_ids.last().copied() != blocks.first().map(|block| block.id.0);

        // First call (prefix_sum never initialized) must build even when
        // blocks is empty, so the invariant prefix_sum.len() == n + 1 holds.
        if !ids_changed && !self.prefix_sum_dirty && !self.prefix_sum.is_empty() {
            return;
        }

        // Rebuild: walk newest-to-oldest to match layout_pass traversal.
        self.prefix_sum_block_ids = blocks.iter().rev().map(|b| b.id.0).collect();
        self.prefix_sum.clear();
        self.prefix_sum.reserve(blocks.len() + 1);
        self.prefix_sum.push(0);
        self.clear_prefix_sum.clear();
        self.clear_prefix_sum.reserve(blocks.len() + 1);
        self.clear_prefix_sum.push(0);
        let mut acc = 0usize;
        let mut clear_acc = 0usize;
        for b in blocks.iter().rev() {
            let entry = self.entries.get(&b.id.0);
            let base = entry.map(|c| c.base_row_count).unwrap_or(0);
            acc += base;
            self.prefix_sum.push(acc);
            let is_clear = entry.map(|c| c.is_clear).unwrap_or(false);
            clear_acc += usize::from(is_clear);
            self.clear_prefix_sum.push(clear_acc);
        }
        debug_assert_eq!(self.prefix_sum.len(), self.clear_prefix_sum.len());
        self.prefix_sum_dirty = false;
    }

    /// R2-2 (Batch 7): Prefix sum of `base_row_count`, indexed from the
    /// newest block. `prefix_sum()[0] = 0`, `prefix_sum()[i]` = sum of
    /// `base_row_count` for the i newest blocks. Empty if
    /// `build_prefix_sum` hasn't been called.
    pub(crate) fn prefix_sum(&self) -> &[usize] {
        &self.prefix_sum
    }

    /// R2-2 fix: prefix sum of clear-block count, parallel to `prefix_sum()`.
    /// `clear_prefix_sum[i]` = number of clear blocks among the i newest
    /// blocks. Layout pass multiplies by `viewport_rows * pitch` per-frame
    /// to recover the spacer height excluded from `base_row_count`.
    pub(crate) fn clear_prefix_sum(&self) -> &[usize] {
        &self.clear_prefix_sum
    }
}

/// Compute the layout for a single block (expensive — call once, then cache).
fn compute_block_layout(block: &Block, cols: usize) -> CachedBlockLayout {
    // R2-2 fix: bare `clear` produces a viewport-sized spacer. Mirrors
    // `clear_block_spacer_rows`' command check.
    let is_clear = block.command.split_whitespace().next() == Some("clear");

    // Foldable: does the block have ANY non-empty output line in the last 500?
    // 单一来源 `block_component::block_foldable`,completed_block_* 几何路径共用。
    let foldable = crate::block_component::block_foldable(block);

    // R2-2 (stage 2): command wrap rows. Single source
    // `block_component::command_wrap_rows_for` — the prefix-sum
    // `base_row_count` and the runtime `block_total_height` both consume this
    // so scroll geometry can't drift between the two paths.
    let command_wrap_rows = crate::block_component::command_wrap_rows_for(block, cols, foldable);

    // Collect raw lines and trim trailing empty/prompt lines.
    let raw_lines: Vec<&str> = block.output.lines().collect();
    let trimmed_len = trimmed_output_line_count(&raw_lines);

    // Pre-compute wrapped chunks for each surviving line.
    let lines: Vec<CachedLine> = raw_lines[..trimmed_len]
        .iter()
        .enumerate()
        .map(|(idx, line)| {
            let byte_start = line.as_ptr() as usize - block.output.as_ptr() as usize;
            let byte_end = byte_start + line.len();
            // v1.10.26 (FIX_WRAP_EPOCH_AND_VIEWPORT_KEEP B-1): the chunk
            // strategy follows the block's ORIGIN. A screen-origin block (a
            // primary-screen TUI document) clips overwide rows to a single
            // chunk — its `|]` border must never fold onto the next line. A
            // shell-output block keeps soft-wrap (logical lines may overflow).
            let chunk_ranges: Rc<[Range<usize>]> = Rc::from(if block.screen_origin {
                screen_origin_line_chunk_ranges(line, cols)
            } else {
                block_line_chunk_ranges(line, cols)
            });
            CachedLine {
                idx,
                byte_start,
                byte_end,
                chunk_ranges,
            }
        })
        .collect();

    // R2-2: cache the wrapped output row count so block_content_metrics can
    // read O(1) instead of re-wrapping every block every frame. This sums
    // command resume hints (static strings, opencode-only) + visible output
    // lines — exactly what completed_block_output_rows computes, minus the
    // per-frame header_rows/viewport_rows terms that don't belong in the cache.
    // Collapsed blocks report 0 output rows (matching completed_block_output_rows)
    // even though `lines` is still populated for foldable detection / uncollapse.
    let output_rows = if block.collapsed {
        0
    } else {
        let hint_rows: usize = crate::block_component::command_resume_hints(block)
            .iter()
            .map(|hint| block_line_chunks(hint, cols).count())
            .sum();
        let line_rows: usize = lines.iter().map(|l| l.chunk_ranges.len()).sum();
        hint_rows + line_rows
    };

    // R2-2 (Batch 7): base_row_count for prefix-sum. Matches the
    // layout_pass formula: hint/output rows + the conditional command/output
    // breathing row + wrapped command rows + separator. clear_rows and
    // header_height are per-frame and excluded.
    let hint_rows_for_base = if block.collapsed {
        0
    } else {
        crate::block_component::command_resume_hints(block)
            .iter()
            .map(|hint| block_line_chunks(hint, cols).count())
            .sum::<usize>()
    };
    // R2-2 fix: avoid double-counting resume hints — `output_rows` above
    // already includes them, so sum raw output line rows here instead.
    let content_rows = if block.collapsed {
        0
    } else {
        let line_rows: usize = lines.iter().map(|l| l.chunk_ranges.len()).sum();
        hint_rows_for_base + line_rows
    };
    // R2-2 (stage 2): command counted by wrapped rows instead of a constant
    // 1. Single-line commands keep the old `+2` total (command + separator).
    let base_row_count = content_rows
        + crate::block_component::command_output_gap_rows(content_rows)
        + command_wrap_rows
        + 1; // separator

    // M5-a (PLAN_M5 §二): L1/L2 tables beside the legacy fields until M5-b migrates consumers.
    let content = visual_rows::build_content_table(&block.output, block.screen_origin);
    let hints = crate::block_component::command_resume_hints(block);
    let width = visual_rows::build_width_table(&content, hints, cols);

    CachedBlockLayout {
        output_len: block.output.len(),
        output_identity: block.output.as_ptr() as usize,
        command_len: block.command.len(),
        collapsed: block.collapsed,
        cols,
        foldable,
        lines,
        output_rows,
        base_row_count,
        is_clear,
        command_wrap_rows,
        content,
        width,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use weft_core::blocks::{Block, BlockId};

    fn mk_block_with_output(id: u64, command: &str, output: &str) -> Block {
        Block {
            id: BlockId(id),
            command: command.to_string(),
            cwd: None,
            output: output.into(),
            styled_output: None,
            exit_code: None,
            started_at: std::time::SystemTime::UNIX_EPOCH,
            finished_at: None,
            collapsed: false,
            screen_origin: false,
        }
    }

    #[test]
    fn block_layout_cache_computes_on_first_access() {
        let block = mk_block_with_output(1, "echo hello", "hello\nworld\n");
        let layout = compute_block_layout(&block, 80);
        assert_eq!(layout.lines.len(), 2);
        assert_eq!(layout.lines[0].idx, 0);
        assert_eq!(layout.lines[1].idx, 1);
        assert!(layout.foldable);
    }

    #[test]
    fn block_layout_cache_trims_trailing_empty() {
        let block = mk_block_with_output(1, "echo", "output\n\n\n");
        let layout = compute_block_layout(&block, 80);
        assert_eq!(
            layout.lines.len(),
            1,
            "trailing empty lines should be trimmed"
        );
        assert_eq!(
            &block.output[layout.lines[0].byte_start..layout.lines[0].byte_end],
            "output"
        );
    }

    #[test]
    fn block_layout_cache_trims_trailing_prompt() {
        let block = mk_block_with_output(1, "echo", "output\n%\n$\n#\n");
        let layout = compute_block_layout(&block, 80);
        assert_eq!(
            layout.lines.len(),
            1,
            "trailing prompt lines should be trimmed"
        );
    }

    #[test]
    fn block_layout_cache_byte_offsets_correct() {
        let block = mk_block_with_output(1, "echo", "first\nsecond\nthird\n");
        let layout = compute_block_layout(&block, 80);
        assert_eq!(layout.lines.len(), 3);
        assert_eq!(
            &block.output[layout.lines[0].byte_start..layout.lines[0].byte_end],
            "first"
        );
        assert_eq!(
            &block.output[layout.lines[1].byte_start..layout.lines[1].byte_end],
            "second"
        );
        assert_eq!(
            &block.output[layout.lines[2].byte_start..layout.lines[2].byte_end],
            "third"
        );
    }

    #[test]
    fn block_layout_cache_wraps_long_lines() {
        // 20 chars at cols=10 → 2 chunks
        let block = mk_block_with_output(1, "echo", "0123456789abcdefghij");
        let layout = compute_block_layout(&block, 10);
        assert_eq!(layout.lines.len(), 1);
        assert_eq!(
            layout.lines[0].chunk_ranges.len(),
            2,
            "20 chars at cols=10 → 2 chunks"
        );
        let line = &block.output[layout.lines[0].byte_start..layout.lines[0].byte_end];
        assert_eq!(&line[layout.lines[0].chunk_ranges[0].clone()], "0123456789");
        assert_eq!(&line[layout.lines[0].chunk_ranges[1].clone()], "abcdefghij");
    }

    #[test]
    fn block_layout_cache_foldable_false_for_empty_output() {
        let block = mk_block_with_output(1, "true", "\n\n\n");
        let layout = compute_block_layout(&block, 80);
        assert!(!layout.foldable, "all-empty output should not be foldable");
        assert_eq!(layout.lines.len(), 0, "all lines trimmed");
    }

    #[test]
    fn block_layout_cache_ensure_cached_reuses() {
        let mut cache = BlockLayoutCache::default();
        let block = mk_block_with_output(1, "echo", "hello\n");
        cache.ensure_cached(&block, 80);
        let layout1 = cache.get(1).clone();

        // Same content + cols → should NOT rebuild (same instance).
        cache.ensure_cached(&block, 80);
        let layout2 = cache.get(1).clone();
        assert_eq!(layout1.lines.len(), layout2.lines.len());
        assert_eq!(layout1.cols, layout2.cols);
    }

    #[test]
    fn block_layout_cache_rebuilds_on_output_change() {
        let mut cache = BlockLayoutCache::default();
        let block = mk_block_with_output(1, "echo", "hello\n");
        cache.ensure_cached(&block, 80);
        assert_eq!(cache.get(1).lines.len(), 1);

        // Output grew → cache should detect and rebuild.
        let block2 = mk_block_with_output(1, "echo", "hello\nworld\n");
        cache.ensure_cached(&block2, 80);
        assert_eq!(
            cache.get(1).lines.len(),
            2,
            "output change should trigger rebuild"
        );
    }

    #[test]
    fn block_layout_cache_rebuilds_for_equal_length_replacement() {
        let mut cache = BlockLayoutCache::default();
        let block = mk_block_with_output(1, "echo", "abcdef\n");
        cache.ensure_cached(&block, 80);
        assert_eq!(cache.get(1).lines[0].byte_end, 6);

        // Same BlockId and byte length, but a different immutable allocation
        // and UTF-8 boundary. Reusing the old byte range could panic while
        // slicing the replacement output.
        let replacement = mk_block_with_output(1, "echo", "中文\n");
        assert_eq!(block.output.len(), replacement.output.len());
        cache.ensure_cached(&replacement, 80);
        let line = &cache.get(1).lines[0];
        assert_eq!(&replacement.output[line.byte_start..line.byte_end], "中文");
    }

    #[test]
    fn block_layout_cache_rebuilds_on_cols_change() {
        let mut cache = BlockLayoutCache::default();
        let block = mk_block_with_output(1, "echo", "0123456789abcdefghij");
        cache.ensure_cached(&block, 10);
        assert_eq!(
            cache.get(1).lines[0].chunk_ranges.len(),
            2,
            "20 chars / cols=10 → 2 chunks"
        );

        // Resize to cols=20 → should rebuild with 1 chunk.
        cache.ensure_cached(&block, 20);
        assert_eq!(
            cache.get(1).lines[0].chunk_ranges.len(),
            1,
            "20 chars / cols=20 → 1 chunk"
        );
    }

    #[test]
    fn block_layout_cache_rebuilds_on_collapse_toggle() {
        let mut cache = BlockLayoutCache::default();
        let block = mk_block_with_output(1, "echo", "hello\n");
        cache.ensure_cached(&block, 80);
        assert!(!cache.get(1).collapsed);

        let mut block2 = block.clone();
        block2.collapsed = true;
        cache.ensure_cached(&block2, 80);
        assert!(
            cache.get(1).collapsed,
            "collapse toggle should trigger rebuild"
        );
    }

    #[test]
    fn stable_history_sync_checks_only_newest_block() {
        let blocks: Vec<_> = (1..=1000)
            .map(|id| mk_block_with_output(id, "echo", "one line\n"))
            .collect();
        let mut cache = BlockLayoutCache::default();
        cache.sync_blocks(&blocks, 80);
        cache.take_hit_miss_counts();

        cache.sync_blocks(&blocks, 80);
        let (hits, misses) = cache.take_hit_miss_counts();
        assert_eq!((hits, misses), (1, 0));
    }

    #[test]
    fn explicit_invalidation_rebuilds_non_newest_block() {
        let mut blocks = vec![
            mk_block_with_output(1, "echo", "old\n"),
            mk_block_with_output(2, "echo", "new\n"),
        ];
        let mut cache = BlockLayoutCache::default();
        cache.sync_blocks(&blocks, 80);
        cache.take_hit_miss_counts();

        blocks[0].collapsed = true;
        cache.invalidate(1);
        cache.sync_blocks(&blocks, 80);
        assert!(cache.get(1).collapsed);
        let (_, misses) = cache.take_hit_miss_counts();
        assert_eq!(misses, 1);
    }

    #[test]
    fn block_layout_cache_empty_output() {
        let block = mk_block_with_output(1, "true", "");
        let layout = compute_block_layout(&block, 80);
        assert_eq!(layout.lines.len(), 0);
        assert!(!layout.foldable);
    }

    // ── R2-2 (Batch 7): prefix sum tests ───────────────────────────────

    /// `base_row_count` includes one breathing row when output exists, plus
    /// command + separator. Collapsed/empty-output blocks remain at `2`.
    #[test]
    fn base_row_count_matches_layout_formula() {
        // 3 output lines + breathing row + command + separator = 6.
        let block = mk_block_with_output(1, "echo", "a\nb\nc\n");
        let layout = compute_block_layout(&block, 80);
        assert_eq!(layout.base_row_count, 6);

        // Collapsed → output_rows=0, hint_rows=0 → base = 2
        let mut collapsed = block;
        collapsed.collapsed = true;
        let layout_c = compute_block_layout(&collapsed, 80);
        assert_eq!(layout_c.base_row_count, 2);

        // Empty output → output_rows=0 → base = 0 + 0 + 2 = 2
        let empty = mk_block_with_output(2, "true", "");
        let layout_e = compute_block_layout(&empty, 80);
        assert_eq!(layout_e.base_row_count, 2);
    }

    /// B6 回归:长命令折行必须计入 `command_wrap_rows`(旧 `+2` 常量低估高度),
    /// 且缓存/回退两条 metrics 路径与单一来源 helper 一致,scrollbar/find 几何不漂移。
    #[test]
    fn command_wrap_rows_counts_wrapped_commands_and_stays_consistent() {
        let block = mk_block_with_output(1, "a very long command that surely wraps", "ok\n");
        let cols = 20;
        let layout = compute_block_layout(&block, cols);
        // 36 列命令,foldable → first_cols=17,折成 2 行。
        assert_eq!(layout.command_wrap_rows, 2);
        // 与 block_component 的单一来源 helper 同值(防量纲漂移)。
        assert_eq!(
            crate::block_component::command_wrap_rows_for(&block, cols, layout.foldable),
            layout.command_wrap_rows
        );
        // 折叠块命令恒 1 行。
        let mut collapsed = block.clone();
        collapsed.collapsed = true;
        assert_eq!(compute_block_layout(&collapsed, cols).command_wrap_rows, 1);

        // metrics 缓存路径(读 c.command_wrap_rows)必须与直接计算一致。
        let mut terminal = weft_core::vt::Terminal::new(24, cols);
        terminal.process(
            b"\x1b]133;A\x07a very long command that surely wraps\x1b]133;B\x07\x1b]133;C\x07ok\r\n\x1b]133;D;0\x07",
        );
        let (direct, _) = crate::block_component::block_content_metrics(&terminal, cols, 1);
        let mut cache = BlockLayoutCache::default();
        for blk in terminal.block_tracker().session_blocks() {
            cache.ensure_cached(blk, cols);
        }
        let (cached, _) = crate::block_component::block_content_metrics_with_cache(
            &terminal,
            cols,
            1,
            Some(&cache),
            None,
        );
        assert_eq!(cached, direct);
    }

    #[test]
    fn build_prefix_sum_empty_blocks() {
        let mut cache = BlockLayoutCache::default();
        cache.build_prefix_sum(&[]);
        assert_eq!(cache.prefix_sum(), &[0]);
    }

    #[test]
    fn build_prefix_sum_single_block() {
        let mut cache = BlockLayoutCache::default();
        let block = mk_block_with_output(1, "echo", "a\nb\nc\n"); // base=6
        cache.ensure_cached(&block, 80);
        cache.build_prefix_sum(&[block]);
        // prefix_sum[0]=0, prefix_sum[1]=6
        assert_eq!(cache.prefix_sum(), &[0, 6]);
    }

    #[test]
    fn build_prefix_sum_multiple_blocks_cumulative() {
        let mut cache = BlockLayoutCache::default();
        // Newest-first ordering in `blocks` vec; build_prefix_sum walks
        // .rev() so prefix_sum[1] = newest block's base, prefix_sum[2] =
        // newest + second-newest, etc.
        // blocks[0] = oldest (id=1, base=2: empty output)
        // blocks[1] = newest (id=2, base=6: 3 lines + breathing row)
        let b1 = mk_block_with_output(1, "true", "");
        let b2 = mk_block_with_output(2, "echo", "a\nb\nc\n");
        let blocks = vec![b1, b2];
        cache.ensure_cached(&blocks[0], 80);
        cache.ensure_cached(&blocks[1], 80);
        cache.build_prefix_sum(&blocks);
        // rev() walks b2 (base=6) then b1 (base=2).
        // prefix_sum = [0, 6, 8]
        assert_eq!(cache.prefix_sum(), &[0, 6, 8]);
    }

    /// Rebuild is skipped when neither block IDs nor any cache entry changed.
    /// This is the steady-state hot path (no rebuild per frame).
    #[test]
    fn build_prefix_sum_skips_rebuild_when_unchanged() {
        let mut cache = BlockLayoutCache::default();
        let block = mk_block_with_output(1, "echo", "a\nb\n");
        cache.ensure_cached(&block, 80);
        cache.build_prefix_sum(std::slice::from_ref(&block));
        let ps1 = cache.prefix_sum().to_vec();

        // Second call with same blocks — should be a no-op.
        cache.build_prefix_sum(&[block]);
        assert_eq!(cache.prefix_sum(), ps1.as_slice());
    }

    /// Adding a block triggers rebuild (ids_changed path).
    #[test]
    fn build_prefix_sum_rebuilds_on_block_added() {
        let mut cache = BlockLayoutCache::default();
        let b1 = mk_block_with_output(1, "echo", "a\n");
        cache.ensure_cached(&b1, 80);
        cache.build_prefix_sum(std::slice::from_ref(&b1));
        assert_eq!(cache.prefix_sum(), &[0, 4]); // output + breathing + structural = 4

        // Add a second block (older). blocks = [b2, b1] (b1 is newest).
        let b2 = mk_block_with_output(2, "echo", "x\ny\nz\n");
        cache.ensure_cached(&b2, 80);
        cache.build_prefix_sum(&[b2, b1.clone()]);
        // rev() → b1 (base=4) then b2 (base=6). prefix_sum = [0, 4, 10]
        assert_eq!(cache.prefix_sum(), &[0, 4, 10]);
    }

    /// A cache miss (output change) sets prefix_sum_dirty, forcing rebuild
    /// on the next build_prefix_sum call even if IDs are unchanged.
    #[test]
    fn build_prefix_sum_rebuilds_on_output_change() {
        let mut cache = BlockLayoutCache::default();
        let block = mk_block_with_output(1, "echo", "a\n");
        cache.ensure_cached(&block, 80);
        cache.build_prefix_sum(std::slice::from_ref(&block));
        assert_eq!(cache.prefix_sum(), &[0, 4]);

        // Output grows → ensure_cached triggers rebuild, sets dirty.
        let block2 = mk_block_with_output(1, "echo", "a\nb\nc\nd\n");
        cache.ensure_cached(&block2, 80);
        cache.build_prefix_sum(&[block2]);
        // base = 4 output + breathing + command + separator = 7
        assert_eq!(cache.prefix_sum(), &[0, 7]);
    }

    /// Collapsed blocks contribute base_row_count=2 to the prefix sum,
    /// matching the layout_pass formula (cursor_dist += pitch*0 + pitch
    /// + header_height + pitch for command+header+separator).
    #[test]
    fn build_prefix_sum_handles_collapsed_blocks() {
        let mut cache = BlockLayoutCache::default();
        let mut b1 = mk_block_with_output(1, "echo", "a\nb\nc\n");
        b1.collapsed = true; // base = 2
        let b2 = mk_block_with_output(2, "echo", "x\n"); // base = 4
        let blocks = vec![b1, b2];
        cache.ensure_cached(&blocks[0], 80);
        cache.ensure_cached(&blocks[1], 80);
        cache.build_prefix_sum(&blocks);
        // rev() → b2 (base=4) then b1 (base=2). prefix_sum = [0, 4, 6]
        assert_eq!(cache.prefix_sum(), &[0, 4, 6]);
    }
}
