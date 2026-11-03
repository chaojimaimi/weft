//! F7-1b (PLAN_v1185_F7_1B_TOMBSTONES §2.2): scalar tombstones for
//! retain-evicted cache entries.
//!
//! When the set-rotation `sync_blocks` path evicts an entry (tab switch,
//! history retention), the metrics path (`block_content_metrics_with_cache`)
//! used to fall back to O(document) re-wrapping for that block on the very
//! next activation clamp (~170ms per giant block). Eviction now captures
//! three metric scalars into this bounded FIFO side map; a metrics-path
//! `get_if_cached` miss reads the stale scalars O(1) — the M6-b B-3
//! PRESENT-stale semantics extended to absent entries.
//!
//! Draw geometry NEVER reads tombstones — draw requires a real band and
//! real L1/L2 tables (layout_pass / band classification are off-limits).

use std::collections::{HashMap, HashSet, VecDeque};

use super::{BlockLayoutCache, CachedBlockLayout};

/// Bounded side-map capacity: ~24B payload + map/queue overhead ≈ ~100B per
/// record → a ~400KB upper bound. Overflow evicts the OLDEST record; that
/// block's metrics then degrade to the v1.18.4 O(document) fallback —
/// slower, never wrong.
const TOMBSTONE_CAP: usize = 4096;

/// F7-1b: metric scalars captured at retain-eviction. Copy, ~24B.
/// Draw geometry NEVER reads these — draw requires real band (真实 band).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct BlockMetricsTombstone {
    pub(crate) output_rows: usize,       // = entry.stale_output_rows
    pub(crate) command_wrap_rows: usize, // = entry.command_wrap_rows
    pub(crate) cols: usize,              // = entry.cols（观测/测试用；读点不做 cols 门控）
}

impl BlockMetricsTombstone {
    /// Snapshot the three metric scalars off a live entry (capture must
    /// happen while the entry is still in `entries`, i.e. BEFORE the retain).
    fn from_entry(entry: &CachedBlockLayout) -> Self {
        Self {
            output_rows: entry.stale_output_rows,
            command_wrap_rows: entry.command_wrap_rows,
            cols: entry.cols,
        }
    }
}

/// Bounded FIFO side map keyed by `BlockId.0` (globally unique, never
/// reused — block_id_sequence). `order` is the eviction queue: existing ids
/// update IN PLACE (the FIFO age is preserved), new ids push back and the
/// oldest is dropped past [`TOMBSTONE_CAP`].
// Derived Default (both fields are trivially default) so the main file's
// `#[derive(Default)]` on `BlockLayoutCache` keeps compiling untouched.
#[derive(Default)]
pub(crate) struct TombstoneMap {
    map: HashMap<u64, BlockMetricsTombstone>,
    order: VecDeque<u64>, // FIFO 逐出序（insert-order，更新原位不重排）
}

impl TombstoneMap {
    /// Capture/update one tombstone. An existing id updates in place (FIFO
    /// position kept — re-capture must not refresh its age); a new id pushes
    /// back, evicting the oldest record past [`TOMBSTONE_CAP`].
    fn record(&mut self, id: u64, tombstone: BlockMetricsTombstone) {
        // rust-reviewer P3-1: pin the order/map sync invariant so a future
        // record rewrite cannot desynchronize them silently.
        debug_assert_eq!(self.order.len(), self.map.len());
        if self.map.insert(id, tombstone).is_none() {
            self.order.push_back(id);
            if self.order.len() > TOMBSTONE_CAP {
                if let Some(oldest) = self.order.pop_front() {
                    self.map.remove(&oldest);
                }
            }
        }
    }

    /// O(1) stale-scalar read. Returns a Copy — no borrow of the cache.
    fn get(&self, id: u64) -> Option<BlockMetricsTombstone> {
        self.map.get(&id).copied()
    }
}

impl BlockLayoutCache {
    /// M6-c byte-total subtraction + F7-1b tombstone capture share ONE
    /// evicted-entry walk; the two retains are the verbatim tail moved out
    /// of `sync_blocks` (v1.15.2 identity-move convention — only the
    /// byte-total segment is a per-item rewrite of the former one-shot sum,
    /// arithmetic identical, pinned by budget_tests' recompute oracle).
    /// Capture MUST precede the retain: the scalars are read off the
    /// entries being evicted.
    pub(crate) fn evict_vanished_blocks(&mut self, live_ids: &HashSet<u64>) {
        let evicted: Vec<(u64, usize, BlockMetricsTombstone)> = self
            .entries
            .iter()
            .filter(|(id, _)| !live_ids.contains(id))
            .map(|(id, c)| {
                (
                    *id,
                    c.estimated_table_bytes(),
                    BlockMetricsTombstone::from_entry(c),
                )
            })
            .collect();
        for (id, table_bytes, tombstone) in evicted {
            // M6-c: evicted entries' tables leave the running byte total.
            self.table_bytes_total -= table_bytes;
            // F7-1b: the scalars stay reachable for the metrics path.
            self.tombstones.record(id, tombstone);
        }
        self.entries.retain(|id, _| live_ids.contains(id));
        // M6-c: degraded markers for vanished blocks would be orphans.
        self.degraded_ids.retain(|id| live_ids.contains(id));
    }

    /// F7-1b: the ONLY tombstone read point — the metrics loop in
    /// `block_component::block_content_metrics_with_cache`, and only when
    /// `get_if_cached` missed. Draw geometry must never call this.
    pub(crate) fn get_metrics_tombstone(&self, id: u64) -> Option<BlockMetricsTombstone> {
        self.tombstones.get(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paint::grid_cache::BandSync;
    use weft_core::blocks::{Block, BlockId};

    fn mk_block(id: u64, output: &str) -> Block {
        Block {
            id: BlockId(id),
            command: "echo".to_string(),
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

    /// Pre-M6-b semantics: a band covering everything defers nothing.
    fn full_band() -> BandSync {
        BandSync {
            low_rows: 0,
            high_rows: usize::MAX,
        }
    }

    /// §五-1 capture: a set-rotation sync evicts tab-A's entries and captures
    /// their scalars into tombstones; surviving entries capture nothing.
    #[test]
    fn set_rotation_eviction_captures_tombstones() {
        let tab_a: Vec<Block> = (1..=2).map(|id| mk_block(id, "alpha\nbeta\n")).collect();
        let tab_b: Vec<Block> = (3..=4).map(|id| mk_block(id, "gamma\n")).collect();
        let mut cache = BlockLayoutCache::default();
        cache.sync_blocks(&tab_a, 80, full_band());
        // Pre-eviction scalars, straight from the live entries.
        let golden: Vec<(u64, usize, usize, usize)> = (1..=2)
            .map(|id| {
                let entry = cache.get(id);
                (
                    id,
                    entry.stale_output_rows,
                    entry.command_wrap_rows,
                    entry.cols,
                )
            })
            .collect();

        cache.sync_blocks(&tab_b, 80, full_band());
        assert!(cache.get_if_cached(1).is_none() && cache.get_if_cached(2).is_none());
        for (id, output_rows, command_wrap_rows, cols) in golden {
            let tomb = cache
                .get_metrics_tombstone(id)
                .expect("tombstone captured at eviction");
            assert_eq!(
                (tomb.output_rows, tomb.command_wrap_rows, tomb.cols),
                (output_rows, command_wrap_rows, cols)
            );
        }
        // Capture is eviction-only: the surviving tab-B entries have no
        // tombstone.
        assert!(cache.get_metrics_tombstone(3).is_none());
    }

    /// §五-1 re-capture: rebuild + re-evict updates the tombstone IN PLACE
    /// to the newest scalars (staleness window = one eviction cycle).
    #[test]
    fn recapture_updates_tombstone_in_place() {
        let mut cache = BlockLayoutCache::default();
        let v1 = vec![mk_block(1, "one line\n")];
        cache.sync_blocks(&v1, 80, full_band());
        let rows_v1 = cache.get(1).stale_output_rows;
        let other = vec![mk_block(2, "x\n")];
        cache.sync_blocks(&other, 80, full_band());
        assert_eq!(cache.get_metrics_tombstone(1).unwrap().output_rows, rows_v1);

        // Rebuild block 1 with more output, re-sync, re-evict.
        let v2 = vec![mk_block(1, "one\n--------- ten\n---------- twenty\n")];
        cache.sync_blocks(&v2, 80, full_band());
        let rows_v2 = cache.get(1).stale_output_rows;
        assert!(rows_v2 > rows_v1, "test setup: rebuild must add rows");
        cache.sync_blocks(&other, 80, full_band());
        assert_eq!(
            cache.get_metrics_tombstone(1).unwrap().output_rows,
            rows_v2,
            "re-capture refreshes the scalars"
        );
    }

    /// §五-1 FIFO cap: the (CAP+1)-th insert evicts the OLDEST record and
    /// an in-place update must NOT refresh the FIFO age — the just-updated
    /// id 0 is still the one evicted.
    #[test]
    fn fifo_cap_evicts_oldest_and_in_place_update_keeps_order() {
        let mut map = TombstoneMap::default();
        let tomb = |rows: usize| BlockMetricsTombstone {
            output_rows: rows,
            command_wrap_rows: 1,
            cols: 80,
        };
        for id in 0..TOMBSTONE_CAP as u64 {
            map.record(id, tomb(id as usize));
        }
        // In-place update: value changes, FIFO age must not.
        map.record(0, tomb(usize::MAX));
        assert_eq!(map.get(0).unwrap().output_rows, usize::MAX);
        // Overflow by one → the oldest BY INSERT ORDER (id 0) is evicted
        // even though it was just updated.
        map.record(TOMBSTONE_CAP as u64, tomb(TOMBSTONE_CAP));
        assert!(map.get(0).is_none(), "oldest FIFO entry evicted at cap+1");
        for id in 1..=TOMBSTONE_CAP as u64 {
            assert!(map.get(id).is_some(), "id {id} must survive");
        }
        assert_eq!(map.get(TOMBSTONE_CAP as u64).unwrap(), tomb(TOMBSTONE_CAP));
    }

    /// §五-1 identity move: the per-item byte-total subtraction keeps the
    /// incremental total equal to a full recompute across the eviction
    /// (budget_tests' sequence pin covers the whole lifecycle; this pins the
    /// new capture walk itself).
    #[test]
    fn eviction_keeps_table_bytes_total_equal_to_recompute() {
        let tab_a: Vec<Block> = (1..=3).map(|id| mk_block(id, "row\n")).collect();
        let tab_b: Vec<Block> = (4..=5).map(|id| mk_block(id, "x\n")).collect();
        let mut cache = BlockLayoutCache::default();
        cache.sync_blocks(&tab_a, 80, full_band());
        let tab_a_bytes: usize = (1..=3)
            .map(|id| cache.get(id).estimated_table_bytes())
            .sum();
        assert_eq!(cache.table_bytes_total(), tab_a_bytes);

        cache.sync_blocks(&tab_b, 80, full_band());
        assert!(cache.get_if_cached(1).is_none());
        assert_eq!(
            cache.table_bytes_total(),
            cache.recomputed_table_bytes(),
            "incremental total == full recompute after eviction"
        );
        assert!(cache.table_bytes_total() < tab_a_bytes);
    }

    /// §五-1: `invalidate()` (test-only explicit eviction) must NOT capture
    /// a tombstone — behavior-unchanged declaration: capture lives ONLY in
    /// the sync eviction walk.
    #[test]
    fn invalidate_produces_no_tombstone() {
        let mut cache = BlockLayoutCache::default();
        let block = mk_block(1, "alpha\n");
        cache.ensure_cached(&block, 80);
        cache.invalidate(1);
        assert!(cache.get_if_cached(1).is_none());
        assert!(
            cache.get_metrics_tombstone(1).is_none(),
            "invalidate() is not a tombstone capture point"
        );
    }
}
