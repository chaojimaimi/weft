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

use std::cell::Cell;
use std::collections::HashMap;

use weft_core::blocks::Block;

use band::{rebuild_kind, RebuildKind};
mod band;
mod visual_rows;
pub(crate) use visual_rows::{char_offset_at, completed_output_rows, trimmed_output_line_count};
#[cfg(test)]
pub(crate) use wrapping::block_line_chunk_ranges;
mod wrapping;
pub(crate) use wrapping::{block_line_chunks, command_line_chunks, screen_origin_line_chunks};

pub(crate) const MAX_LAYOUT_LINES_LIVE: usize = 2000;

/// M6-c (PLAN_M6 §三): total byte budget across all cached L1/L2 layout
/// tables. A 1MiB block's tables are ~6-8MB, so the budget tolerates ~30-40
/// giant blocks; normal sessions never approach it. Exceeding it degrades
/// the OLDEST non-exempt blocks to metrics-only shells (R5: a degraded block
/// re-entering the band pays one ~15-20ms Both rebuild — accepted, single).
const LAYOUT_TABLE_BUDGET_BYTES: usize = 256 * 1024 * 1024;

/// M6-c: the newest K blocks always keep their tables (never degraded) — the
/// live block's neighborhood is the highest-frequency render/hit-test target
/// and re-degrading it would thrash against the band check every frame.
const BUDGET_EXEMPT_NEWEST_BLOCKS: usize = 4;

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
    /// M5-b (PLAN_M5 §二): L2 width layer — the ONLY row source. Output rows
    /// read `hint_rows + rows.len()` (O(1)); visible frames slice
    /// `rows` windows via `line_row_base` (no per-line materialization).
    pub(crate) width: visual_rows::WidthTable,
    /// M6-b B-3: uncollapsed output row count at build time
    /// (`hint_rows + width.rows.len()`). The metrics path reads THIS scalar
    /// even when the entry's `cols` are stale (band-deferred; M6-c degraded)
    /// so metrics stay同源 with the prefix sum's stale `base_row_count`.
    pub(crate) stale_output_rows: usize,
    /// M6-c (PLAN_M6 §三): tables dropped by the memory budget
    /// (`enforce_table_budget`). A degraded entry is a metrics-only shell:
    /// every scalar (base_row_count / stale_output_rows / command_wrap_rows /
    /// is_clear / cols / foldable / collapsed) stays exact, so the prefix sum
    /// and the metrics path have zero drift — but `content`/`width` are
    /// empty. Revived exclusively by a Both rebuild (the band check rebuilds
    /// a degraded block when it scrolls into the band); `ensure_cached`
    /// checks this flag BEFORE the three-state verdict because a WidthOnly
    /// over the emptied L1 would produce empty tables.
    pub(crate) degraded: bool,
}

impl CachedBlockLayout {
    /// M6-c: byte estimate of the L1/L2 tables ONLY (all scalars excluded —
    /// they survive degradation). Formula mapped onto the M5 structures:
    /// - L1 `ContentTable`: `graphemes.len() × size_of::<GEntry>()` (8B:
    ///   u32 span + width + flags + padding) + `line_meta.len() ×
    ///   size_of::<LMeta>()` (one entry per surviving source line);
    /// - L2 `WidthTable`: `rows.len() × size_of::<VisualRow>()` (16B,
    ///   compile-pinned in visual_rows.rs) + `line_row_base.len() ×
    ///   size_of::<u32>()`.
    ///
    /// Estimate purpose: budget governance, NOT exact measurement — Vec
    /// capacities and allocator overhead are deliberately ignored.
    pub(crate) fn estimated_table_bytes(&self) -> usize {
        let l1 = self.content.graphemes.len() * std::mem::size_of::<visual_rows::GEntry>()
            + self.content.line_meta.len() * std::mem::size_of::<visual_rows::LMeta>();
        let l2 = self.width.rows.len() * std::mem::size_of::<visual_rows::VisualRow>()
            + self.width.line_row_base.len() * std::mem::size_of::<u32>();
        l1 + l2
    }
}

/// M6-b (PLAN_M6 §三 B-1): viewport row band in distance-from-content-bottom
/// coordinates (`base_row_count` row units). Blocks intersecting the band or
/// below it rebuild immediately; blocks fully above (older history) may defer
/// their cols rebuild. Both sync callers derive the same band from their own
/// geometry via `band::BandSync::for_viewport`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct BandSync {
    pub(crate) low_rows: usize,
    pub(crate) high_rows: usize,
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
    /// M6-b B-2: ids deferred above the sync band (stale cols entry kept).
    /// B-1 band-checks this set every sync; the B-5 pump drains it. M6-c
    /// unions `degraded_ids` into this pending set.
    deferred_ids: Vec<u64>,
    /// M6-b B-3: metrics-path fallbacks (`completed_output_rows` because no
    /// entry existed). Deferred scenarios must hold this at 0.
    metrics_fallback_rebuilds: Cell<usize>,
    /// M6-b P2-1: whether the LAST sync took the append-only fast path
    /// (cols-stable frame). The idle pump only drains on stable frames —
    /// during a drag every frame re-defers whatever the pump rebuilt, so
    /// pumping there is pure waste.
    last_sync_stable: bool,
    /// M6-c: running byte total of all entries' L1/L2 tables (Σ
    /// `estimated_table_bytes`), maintained incrementally at every
    /// insert/evict/degrade/rebuild point; tests pin it against a full
    /// recompute. `enforce_table_budget` early-returns on one compare while
    /// under budget.
    table_bytes_total: usize,
    /// M6-c: ids whose tables the budget dropped. Unioned into the B-1
    /// per-frame band check (`rebuild_pending_in_band`): a degraded block
    /// scrolling into the band Both-rebuilds the same frame. Without
    /// membership in the pending set, a cols-fresh degraded entry would
    /// report a Hit forever and the render would read empty tables.
    degraded_ids: Vec<u64>,
}

impl BlockLayoutCache {
    /// Synchronize the append-only block history incrementally. Scrolling
    /// frames check only the newest block; append, resize and explicit
    /// invalidation rebuild affected entries before refreshing the prefix sum.
    /// M6-b (PLAN_M6 §三 B-1/B-2): `band` gates cols rebuilds — blocks
    /// intersecting/below it rebuild now, blocks fully above defer; the
    /// pending set is band-checked on EVERY sync (incl. append-only).
    pub(crate) fn sync_blocks(&mut self, blocks: &[Block], cols: usize, band: BandSync) {
        let first_id = blocks.first().map(|block| block.id.0);
        let last_id = blocks.last().map(|block| block.id.0);
        let append_only = self.synced_cols == Some(cols)
            && blocks.len() >= self.synced_len
            && self.synced_first_id == first_id
            && (self.synced_len == 0
                || blocks.get(self.synced_len - 1).map(|block| block.id.0) == self.synced_last_id);

        // Explicit invalidation outranks band deferral — rebuild first so the
        // band classification below sees fresh entries.
        for id in std::mem::take(&mut self.dirty_ids) {
            self.deferred_ids.retain(|&pending| pending != id);
            if let Some(block) = blocks.iter().find(|block| block.id.0 == id) {
                self.ensure_cached(block, cols);
            }
        }

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
            self.sync_classified(blocks, cols, band);
            // M5-b P2-1 (PLAN_M5 §二 内存): the set shrank or rotated (history
            // retention, `clear`, tab switch) — evict entries for blocks no
            // longer in the session set, or L1 tables (~8B/cluster) would
            // accumulate across session lifetimes. The append-only scroll
            // fast path above skips this (no shrink possible).
            let ids: std::collections::HashSet<u64> =
                blocks.iter().map(|block| block.id.0).collect();
            let before = self.entries.len();
            // M6-c: evicted entries' tables leave the running byte total.
            self.table_bytes_total -= self
                .entries
                .iter()
                .filter(|(id, _)| !ids.contains(id))
                .map(|(_, c)| c.estimated_table_bytes())
                .sum::<usize>();
            self.entries.retain(|id, _| ids.contains(id));
            // M6-c: degraded markers for vanished blocks would be orphans.
            self.degraded_ids.retain(|id| ids.contains(id));
            if self.entries.len() != before {
                // Evicted entries change what get_if_cached/metrics compute.
                self.prefix_sum_dirty = true;
                self.misses_total = self.misses_total.wrapping_add(1);
            }
        }

        // M6-b B-1: pending band check runs on every sync (incl. the
        // append-only fast path) so a deferred block scrolled into the band is
        // rebuilt the same frame — the idle pump is gated off while streaming.
        self.rebuild_pending_in_band(blocks, cols, band);
        self.synced_cols = Some(cols);
        self.synced_len = blocks.len();
        self.synced_first_id = first_id;
        self.synced_last_id = last_id;
        self.build_prefix_sum(blocks);
        // M6-c: budget enforcement runs LAST (after the band check and the
        // prefix-sum refresh). Degrading never touches scalars, so the just-
        // built prefix sum stays valid. One usize compare while under budget.
        self.enforce_table_budget(blocks, band, LAYOUT_TABLE_BUDGET_BYTES);
        // M6-b P2-1: append-only is the cols-stable criterion — the pump
        // reads this to stay idle while `cols` keeps changing.
        self.last_sync_stable = append_only;
    }

    /// Explicitly evict one entry (M5-b P2-2: collapse toggles no longer
    /// call this — the WidthOnly rebuild path preserves the L1 table).
    #[cfg(test)]
    pub(crate) fn invalidate(&mut self, id: u64) {
        if let Some(c) = self.entries.remove(&id) {
            self.table_bytes_total -= c.estimated_table_bytes();
        }
        self.remove_pending(id);
        if !self.dirty_ids.contains(&id) {
            self.dirty_ids.push(id);
        }
        self.prefix_sum_dirty = true;
        // v1.10.23: invalidation changes what the metrics fallback computes
        // (get_if_cached → None → direct), so it must bump the memo
        // fingerprint even before the entry is rebuilt.
        self.misses_total = self.misses_total.wrapping_add(1);
    }

    /// Ensure `block` has a cached layout for `cols`. Recomputes only if
    /// the block is new, its output/command changed, `collapsed` was
    /// toggled, or `cols` changed (resize).
    /// M5-b (PLAN_M5 §二): L1 is keyed by output identity + len — a cols
    /// change (or collapse toggle) reuses the stored L1 table and rebuilds
    /// ONLY the L2 width table (never touches text), which is what keeps a
    /// resize-commit frame inside the G2 budget for large histories.
    pub(crate) fn ensure_cached(&mut self, block: &Block, cols: usize) {
        let id = block.id.0;
        let needs_rebuild = match self.entries.get(&id) {
            None => RebuildKind::Both,
            // M6-c: degraded check BEFORE the three-state verdict — the entry's
            // tables are gone, so a WidthOnly would wrap the EMPTY L1 into
            // empty tables and a Hit would hand the render an empty entry.
            // Only a full Both rebuild restores a degraded entry.
            Some(c) if c.degraded => RebuildKind::Both,
            Some(c) => rebuild_kind(c, block, cols),
        };
        match needs_rebuild {
            RebuildKind::None => self.hits += 1,
            RebuildKind::WidthOnly => {
                self.misses += 1;
                self.misses_total = self.misses_total.wrapping_add(1);
                self.prefix_sum_dirty = true;
                // Take the old entry out so the L1 table MOVES into the new
                // entry (no clone of the per-cluster tables). Pending-set
                // membership is NOT touched here — M6-b semantics: the
                // append-only ensure-last path can WidthOnly-rebuild the
                // newest deferred block while its stale marker survives
                // (self-healed by the band check's Hit-path removal); only
                // the band check and Both rebuilds retire pending ids.
                let mut prev = self.entries.remove(&id).expect("checked above");
                self.table_bytes_total -= prev.estimated_table_bytes();
                prev = compute_block_layout_with_content(block, cols, prev.content);
                self.table_bytes_total += prev.estimated_table_bytes();
                self.entries.insert(id, prev);
            }
            RebuildKind::Both => {
                self.misses += 1;
                self.misses_total = self.misses_total.wrapping_add(1);
                self.prefix_sum_dirty = true;
                let prev_bytes = self
                    .entries
                    .get(&id)
                    .map(|c| c.estimated_table_bytes())
                    .unwrap_or(0);
                let next = compute_block_layout(block, cols);
                self.table_bytes_total -= prev_bytes;
                self.table_bytes_total += next.estimated_table_bytes();
                self.entries.insert(id, next);
                self.remove_pending(id);
            }
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

    /// M6-c: current total table bytes — frame_trace `layout_table_bytes`
    /// plus tests.
    pub(crate) fn table_bytes_total(&self) -> usize {
        self.table_bytes_total
    }

    /// M6-c: blocks currently deferred above the sync band (frame_trace
    /// `deferred_blocks`).
    pub(crate) fn deferred_blocks(&self) -> usize {
        self.deferred_ids.len()
    }

    /// M6-c: drop `id` from BOTH pending sets (deferred + degraded). The
    /// dedup point for the `deferred ∪ degraded` pending union: whichever way
    /// an id became pending, one rebuild retires it everywhere.
    fn remove_pending(&mut self, id: u64) {
        if let Some(pos) = self.deferred_ids.iter().position(|&p| p == id) {
            self.deferred_ids.swap_remove(pos);
        }
        if let Some(pos) = self.degraded_ids.iter().position(|&p| p == id) {
            self.degraded_ids.swap_remove(pos);
        }
    }

    /// M6-c: drop one entry's L1/L2 tables, keeping every scalar. The
    /// metrics/prefix-sum inputs (base_row_count, stale_output_rows,
    /// command_wrap_rows, is_clear, cols, foldable, collapsed) are untouched,
    /// so degradation is zero-drift by construction. Returns true if THIS
    /// call degraded the entry (already-degraded entries are skipped).
    fn degrade_entry(&mut self, id: u64) -> bool {
        let Some(entry) = self.entries.get_mut(&id) else {
            return false;
        };
        if entry.degraded {
            return false;
        }
        let before = entry.estimated_table_bytes();
        entry.content = visual_rows::ContentTable {
            graphemes: Vec::new(),
            line_meta: Vec::new(),
            screen_origin: entry.content.screen_origin,
            trailing_trim_lines: entry.content.trailing_trim_lines,
        };
        // line_row_base keeps the documented prefix invariant
        // (`len == line_meta.len() + 1`, `last == rows.len()`): an L2 of zero
        // lines is `[0]`.
        entry.width = visual_rows::WidthTable {
            rows: Vec::new(),
            line_row_base: vec![0],
            hint_rows: entry.width.hint_rows,
        };
        entry.degraded = true;
        self.table_bytes_total = self.table_bytes_total - before + entry.estimated_table_bytes();
        if !self.degraded_ids.contains(&id) {
            self.degraded_ids.push(id);
        }
        true
    }

    /// M6-c: enforce the total table-byte budget. Over budget, degrade the
    /// OLDEST eligible blocks first (single oldest-first walk — each round
    /// continues from the oldest not-yet-degraded block) to metrics-only
    /// entries. Exempt: blocks intersecting `band` (they are rendered this
    /// frame) and the newest [`BUDGET_EXEMPT_NEWEST_BLOCKS`] blocks (live
    /// neighborhood). When every remaining block is exempt the total may
    /// legitimately stay over budget (e.g. one giant block larger than the
    /// budget itself) — the walk simply runs out of candidates and the next
    /// frame's early return is skipped until something degradable appears.
    /// `budget` is a parameter so tests can pin the behavior with tiny
    /// values; production passes [`LAYOUT_TABLE_BUDGET_BYTES`].
    fn enforce_table_budget(&mut self, blocks: &[Block], band: BandSync, budget: usize) {
        // Per-frame budget check: zero cost while under budget.
        if self.table_bytes_total <= budget {
            return;
        }
        // Band-intersection exemptions, from the same interval walk the band
        // classification uses (entries are post-sync fresh here).
        let mut band_exempt: std::collections::HashSet<u64> = std::collections::HashSet::new();
        band::for_each_stale_interval(self, blocks, |index, bottom, top| {
            // Intersect = not fully above AND not fully below the band.
            if bottom < band.high_rows && top > band.low_rows {
                band_exempt.insert(blocks[index].id.0);
            }
        });
        let first_exempt_index = blocks.len().saturating_sub(BUDGET_EXEMPT_NEWEST_BLOCKS);
        for (index, block) in blocks.iter().enumerate() {
            if self.table_bytes_total <= budget {
                break;
            }
            if index >= first_exempt_index || band_exempt.contains(&block.id.0) {
                continue;
            }
            self.degrade_entry(block.id.0);
        }
    }

    /// M6-c: full recompute of the byte total — the test-only oracle for the
    /// incremental-maintenance invariant.
    #[cfg(test)]
    fn recomputed_table_bytes(&self) -> usize {
        self.entries
            .values()
            .map(|c| c.estimated_table_bytes())
            .sum()
    }

    /// M6-c: degraded-entry count (tests).
    #[cfg(test)]
    pub(crate) fn degraded_count(&self) -> usize {
        self.degraded_ids.len()
    }
}

/// Compute the layout for a single block (expensive — call once, then cache).
fn compute_block_layout(block: &Block, cols: usize) -> CachedBlockLayout {
    let content = visual_rows::build_content_table(&block.output, block.screen_origin);
    compute_block_layout_with_content(block, cols, content)
}

/// Same, with a caller-provided L1 table — the cols-only rebuild path moves
/// the stored table in instead of re-enumerating the output (M5-b).
fn compute_block_layout_with_content(
    block: &Block,
    cols: usize,
    content: visual_rows::ContentTable,
) -> CachedBlockLayout {
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

    // M5-b (PLAN_M5 §二/§五): L1/L2 tables are the SOLE layout source — the
    // legacy per-line CachedLine build is gone (double-build made G2 worse
    // than baseline). L2 re-runs the shared wrap machine over the L1 tables
    // (never touches text).
    let hints = crate::block_component::command_resume_hints(block);
    let width = visual_rows::build_width_table(&content, hints, cols);

    // R2-2 (Batch 7): base_row_count for prefix-sum. Matches the
    // layout_pass formula: hint/output rows + the conditional command/output
    // breathing row + wrapped command rows + separator. clear_rows and
    // header_height are per-frame and excluded. Rows read straight off L2
    // (hint_rows + rows.len()); collapsed blocks report 0 output rows
    // (matching completed_block_output_rows) even though the tables are
    // still populated for foldable detection / uncollapse.
    let content_rows = if block.collapsed {
        0
    } else {
        width.hint_rows as usize + width.rows.len()
    };
    // R2-2 (stage 2): command counted by wrapped rows instead of a constant
    // 1. Single-line commands keep the old `+2` total (command + separator).
    let base_row_count = content_rows
        + crate::block_component::command_output_gap_rows(content_rows)
        + command_wrap_rows
        + 1; // separator

    CachedBlockLayout {
        output_len: block.output.len(),
        output_identity: block.output.as_ptr() as usize,
        command_len: block.command.len(),
        collapsed: block.collapsed,
        cols,
        foldable,
        base_row_count,
        is_clear,
        command_wrap_rows,
        content,
        stale_output_rows: width.hint_rows as usize + width.rows.len(),
        width,
        degraded: false,
    }
}

// Tests live in grid_cache/tests.rs (commit-gate-exempt) — the file sat at
// its audited ceiling when M6-b added the band-gated sync surface. M6-c adds
// the budget/degradation suite as a second companion file (same `#[path]`
// pattern as visual_rows_tests.rs; the path is relative to paint/ because
// grid_cache.rs is a non-mod.rs file).
#[cfg(test)]
#[path = "grid_cache/budget_tests.rs"]
mod budget_tests;
#[cfg(test)]
mod tests;
