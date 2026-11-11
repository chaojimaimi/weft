//! F7-1b (PLAN_v1185_F7_1B_TOMBSTONES §2.2): scalar tombstones for evicted
//! cache entries. F7-1d (PLAN_v1186_F7_1D_BUDGET_RETENTION) retires the
//! set-rotation eviction (set switches now RETAIN foreign entries) and
//! mounts the budget-driven foreign eviction here instead.
//!
//! When an entry is evicted (F7-1d: only the budget-driven foreign eviction
//! arm in `enforce_table_budget`), the metrics path
//! (`block_content_metrics_with_cache`) used to fall back to O(document)
//! re-wrapping for that block on the very next activation clamp (~170ms per
//! giant block). Eviction captures three metric scalars into this bounded
//! FIFO side map; a metrics-path `get_if_cached` miss reads the stale
//! scalars O(1) — the M6-b B-3 PRESENT-stale semantics extended to absent
//! entries.
//!
//! Draw geometry NEVER reads tombstones — draw requires a real band and
//! real L1/L2 tables (layout_pass / band classification are off-limits).

use std::collections::{HashMap, HashSet, VecDeque};

use super::{BlockLayoutCache, CachedBlockLayout};
use weft_core::blocks::Block;

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
    /// F7-1d: budget-driven foreign eviction — the retention counterpart of
    /// the retired `evict_vanished_blocks`. Mounted in `enforce_table_budget`
    /// AFTER `budget::log_exempt_tables_overview` (an eviction-met early
    /// return must not swallow the overview — its regime IS persistent
    /// over-budget) and BEFORE the `no_degradable_candidates` memo (an armed
    /// memo must not skip newly arrived foreign entries); see the call site.
    ///
    /// Entries outside the current block set ("foreign": the previous tab's
    /// tables after a switch, history-retention orphans) are retained by
    /// `sync_blocks` for zero-rebuild re-entry; under budget pressure they
    /// are evicted ascending by BlockId — oldest-first (monotonic under the
    /// BlockStore pool; the local fallback sequence may collide ids, in which
    /// case the order degrades and `ensure_cached`'s entry-identity checks
    /// hold behavior at v1.18.5 — slower, never wrong). The current set is
    /// never touched, and a foreign GIANT is evictable like any other
    /// foreign entry (the giant exemption only guards the degrade walk).
    ///
    /// Every ACTUAL eviction: subtracts the entry's table bytes from the
    /// running total, captures an F7-1b tombstone (`from_entry` + `record`),
    /// retires the id from the `deferred ∪ degraded` pending union
    /// (`remove_pending`), and clears the F1 memo (the candidate pool
    /// changed). An empty-handed arm (no foreign entries, or budget met)
    /// changes nothing else and leaves the memo alone. Same walk shape as
    /// the retired fn — per-item byte-total subtraction pinned by the
    /// recompute-oracle tests below.
    pub(crate) fn evict_foreign_entries(&mut self, blocks: &[Block], budget: usize) {
        let current: HashSet<u64> = blocks.iter().map(|b| b.id.0).collect();
        let mut foreign: Vec<u64> = self
            .entries
            .keys()
            .copied()
            .filter(|id| !current.contains(id))
            .collect();
        foreign.sort_unstable(); // ascending BlockId = oldest-first
        for id in foreign {
            if self.table_bytes_total <= budget {
                break;
            }
            let Some(entry) = self.entries.remove(&id) else {
                continue;
            };
            let table_bytes = entry.estimated_table_bytes();
            self.table_bytes_total -= table_bytes;
            self.tombstones
                .record(id, BlockMetricsTombstone::from_entry(&entry));
            self.remove_pending(id);
            self.no_degradable_candidates = false;
            // WP2 A1 style (`budget degrade`): one warn per ACTUAL eviction —
            // the single observation point for budget-driven evictions.
            tracing::warn!(id, table_bytes, "budget evict");
        }
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
    use super::super::{visual_rows, GIANT_TABLE_EXEMPT_BYTES};
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

    /// Degenerate band: intersects nothing (the direct `enforce_table_budget`
    /// test calls take it, same shape as budget_tests').
    fn no_band() -> BandSync {
        BandSync {
            low_rows: 0,
            high_rows: 0,
        }
    }

    // ── F7-1d retention: set switches keep foreign entries ─────────────────

    /// §五-1 (F7-1d core): a set-rotation sync RETAINS the foreign entries —
    /// the previous tab's entries stay cached with intact scalars (the
    /// zero-rebuild re-entry data plane).
    #[test]
    fn foreign_entries_retained_across_set_rotation() {
        let tab_a: Vec<Block> = (1..=2).map(|id| mk_block(id, "alpha\nbeta\n")).collect();
        let tab_b: Vec<Block> = (3..=4).map(|id| mk_block(id, "gamma\n")).collect();
        let mut cache = BlockLayoutCache::default();
        cache.sync_blocks(&tab_a, 80, full_band());
        let golden: Vec<(u64, usize, usize)> = (1..=2)
            .map(|id| {
                let entry = cache.get(id);
                (id, entry.stale_output_rows, entry.base_row_count)
            })
            .collect();

        cache.sync_blocks(&tab_b, 80, full_band());
        for (id, output_rows, base_rows) in golden {
            let entry = cache
                .get_if_cached(id)
                .unwrap_or_else(|| panic!("foreign id {id} must be retained"));
            assert_eq!(entry.stale_output_rows, output_rows, "id {id}");
            assert_eq!(entry.base_row_count, base_rows, "id {id}");
            assert_eq!(entry.cols, 80);
            assert!(!entry.degraded);
        }
        assert_eq!(cache.table_bytes_total(), cache.recomputed_table_bytes());
    }

    /// §五-1 (F7-1d acceptance): switching back is ALL HITS — the retained
    /// entries satisfy sync_classified without a single rebuild (the
    /// `block rebuild slow` 3×90ms fix).
    #[test]
    fn reentry_after_tab_switch_is_all_hits() {
        let tab_a: Vec<Block> = (1..=3).map(|id| mk_block(id, "alpha\n")).collect();
        let tab_b: Vec<Block> = (4..=5).map(|id| mk_block(id, "gamma\n")).collect();
        let mut cache = BlockLayoutCache::default();
        cache.sync_blocks(&tab_a, 80, full_band());
        cache.sync_blocks(&tab_b, 80, full_band());
        cache.take_hit_miss_counts();

        cache.sync_blocks(&tab_a, 80, full_band());
        let (hits, misses) = cache.take_hit_miss_counts();
        assert_eq!(misses, 0, "re-entry must not rebuild (retained entries)");
        assert_eq!(hits, 3, "every tab-A entry is a Hit");
    }

    /// §五-1: re-entry after a cols change takes the WidthOnly path (L1 move
    /// reuse) — one miss per entry, entries never degraded.
    #[test]
    fn reentry_with_cols_change_is_width_only() {
        let tab_a: Vec<Block> = (1..=3).map(|id| mk_block(id, "alpha\n")).collect();
        let tab_b: Vec<Block> = (4..=5).map(|id| mk_block(id, "gamma\n")).collect();
        let mut cache = BlockLayoutCache::default();
        cache.sync_blocks(&tab_a, 80, full_band());
        cache.sync_blocks(&tab_b, 80, full_band());
        let misses_before = cache.total_misses();

        cache.sync_blocks(&tab_a, 40, full_band());
        assert_eq!(
            cache.total_misses(),
            misses_before + 3,
            "exactly one WidthOnly per re-entered entry"
        );
        for id in 1..=3 {
            let entry = cache.get(id);
            assert_eq!(entry.cols, 40);
            assert!(!entry.degraded, "WidthOnly never degrades");
        }
    }

    // ── F7-1d eviction arm: budget-driven foreign eviction ─────────────────

    /// §五-1: budget-driven foreign eviction — ascending BlockId (id 3 is
    /// far larger than 1+2, so a size-ordered implementation would evict it
    /// first and fail the retain asserts), stops once under budget (early
    /// exit), the current set is never touched, and the incremental byte
    /// total stays equal to the full recompute.
    #[test]
    fn budget_eviction_ascending_and_stops_under_budget() {
        let tab_a: Vec<Block> = vec![
            mk_block(1, "one\n"),
            mk_block(2, "one+two\n"),
            mk_block(3, "a much longer output body to enlarge its tables\n"),
        ];
        let tab_b: Vec<Block> = (4..=5).map(|id| mk_block(id, "x\n")).collect();
        let mut cache = BlockLayoutCache::default();
        cache.sync_blocks(&tab_a, 80, full_band());
        cache.sync_blocks(&tab_b, 80, full_band());
        let total = cache.table_bytes_total();
        let b1 = cache.get(1).estimated_table_bytes();
        let b2 = cache.get(2).estimated_table_bytes();
        // Budget fits exactly {3} ∪ tab B → evict 1, then 2, then stop.
        let budget = total - b1 - b2;

        cache.enforce_table_budget(&tab_b, no_band(), budget);

        assert!(cache.get_if_cached(1).is_none(), "oldest foreign evicted");
        assert!(cache.get_if_cached(2).is_none(), "second-oldest evicted");
        assert!(
            cache.get_if_cached(3).is_some(),
            "early stop retains the larger later foreign entry"
        );
        for blk in &tab_b {
            assert!(
                cache.get_if_cached(blk.id.0).is_some(),
                "current set id {} is never evicted",
                blk.id.0
            );
        }
        assert_eq!(
            cache.table_bytes_total(),
            cache.recomputed_table_bytes(),
            "incremental total == full recompute across the eviction"
        );
        assert!(cache.get_metrics_tombstone(1).is_some());
        assert!(cache.get_metrics_tombstone(2).is_some());
        assert!(
            cache.get_metrics_tombstone(3).is_none(),
            "no evict, no capture"
        );
        // rust-reviewer P3-2: the positive memo direction — an actual
        // eviction must clear the F1 short-circuit (candidate pool changed).
        // Re-arm with a TIGHTER budget so the second call is over-budget
        // (a satisfied budget early-returns before the arm and must NOT be
        // read as a memo counterexample).
        cache.no_degradable_candidates = true;
        let b3 = cache.get(3).estimated_table_bytes();
        cache.enforce_table_budget(&tab_b, no_band(), budget - b3);
        assert!(
            !cache.no_degradable_candidates,
            "actual eviction clears the no-candidates memo"
        );
        assert!(
            cache.get_if_cached(3).is_none(),
            "tighter budget evicts id 3"
        );
    }

    /// §五-1/§5-3 (F7-1d rewrite of `set_rotation_eviction_captures_tombstones`):
    /// a budget eviction captures tombstones with the exact pre-eviction
    /// scalars; surviving entries capture nothing.
    #[test]
    fn budget_eviction_captures_tombstones() {
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
        // F7-1d: the switch itself retains — no eviction, no tombstones yet.
        assert!(cache.get_if_cached(1).is_some() && cache.get_if_cached(2).is_some());
        assert!(cache.get_metrics_tombstone(1).is_none());

        // Budget pinched to evict exactly tab A's foreign entries.
        let total = cache.table_bytes_total();
        let a_bytes: usize = (1..=2)
            .map(|id| cache.get(id).estimated_table_bytes())
            .sum();
        cache.enforce_table_budget(&tab_b, no_band(), total - a_bytes);

        for (id, output_rows, command_wrap_rows, cols) in golden {
            assert!(cache.get_if_cached(id).is_none(), "id {id} evicted");
            let tomb = cache
                .get_metrics_tombstone(id)
                .expect("tombstone captured at budget eviction");
            assert_eq!(
                (tomb.output_rows, tomb.command_wrap_rows, tomb.cols),
                (output_rows, command_wrap_rows, cols)
            );
        }
        // Capture is eviction-only: the surviving tab-B entries have no
        // tombstone.
        assert!(cache.get_metrics_tombstone(3).is_none());
    }

    /// §五-1/§5-3 (F7-1d rewrite): rebuild + re-evict updates the tombstone
    /// IN PLACE to the newest scalars (staleness window = one eviction
    /// cycle).
    #[test]
    fn recapture_updates_tombstone_in_place() {
        let mut cache = BlockLayoutCache::default();
        let v1 = vec![mk_block(1, "one line\n")];
        let other = vec![mk_block(2, "x\n")];
        cache.sync_blocks(&v1, 80, full_band());
        let rows_v1 = cache.get(1).stale_output_rows;

        // First capture: budget-evict the retained block-1 entry.
        cache.sync_blocks(&other, 80, full_band());
        let total = cache.table_bytes_total();
        let b1 = cache.get(1).estimated_table_bytes();
        cache.enforce_table_budget(&other, no_band(), total - b1);
        assert_eq!(cache.get_metrics_tombstone(1).unwrap().output_rows, rows_v1);

        // Rebuild block 1 with more output, re-sync, re-evict.
        let v2 = vec![mk_block(1, "one\n--------- ten\n---------- twenty\n")];
        cache.sync_blocks(&v2, 80, full_band());
        let rows_v2 = cache.get(1).stale_output_rows;
        assert!(rows_v2 > rows_v1, "test setup: rebuild must add rows");
        cache.sync_blocks(&other, 80, full_band());
        let total = cache.table_bytes_total();
        let b1 = cache.get(1).estimated_table_bytes();
        cache.enforce_table_budget(&other, no_band(), total - b1);
        assert_eq!(
            cache.get_metrics_tombstone(1).unwrap().output_rows,
            rows_v2,
            "re-capture refreshes the scalars in place"
        );
    }

    /// §五-1: a budget eviction captures the tombstone AND retires the id
    /// from BOTH pending sets (`remove_pending`). The pending markers are
    /// pre-seeded directly: §2.4's orphan retain keeps production sync
    /// sequences from ever leaving a foreign id pending, so this pins the
    /// contract the arm must uphold regardless.
    #[test]
    fn budget_eviction_captures_tombstone_and_clears_pending() {
        let tab_a: Vec<Block> = (1..=3).map(|id| mk_block(id, "alpha\n")).collect();
        let tab_b: Vec<Block> = (4..=5).map(|id| mk_block(id, "gamma\n")).collect();
        let mut cache = BlockLayoutCache::default();
        cache.sync_blocks(&tab_a, 80, full_band());
        cache.sync_blocks(&tab_b, 80, full_band());
        cache.degraded_ids.push(1);
        cache.deferred_ids.push(1);

        let total = cache.table_bytes_total();
        let b1 = cache.get(1).estimated_table_bytes();
        cache.enforce_table_budget(&tab_b, no_band(), total - b1);

        assert!(cache.get_if_cached(1).is_none(), "foreign id 1 evicted");
        let tomb = cache
            .get_metrics_tombstone(1)
            .expect("eviction captures the tombstone");
        assert_eq!(tomb.cols, 80);
        assert!(
            !cache.degraded_ids.contains(&1) && !cache.deferred_ids.contains(&1),
            "remove_pending retires the evicted id from both sets"
        );
    }

    /// Giant-entry rig (same shape as budget_tests'): a CachedBlockLayout
    /// whose L1 estimate alone exceeds `GIANT_TABLE_EXEMPT_BYTES` — the byte
    /// estimate reads `.len()` fields only, so zeroed entries suffice, and
    /// `base_row_count = 0` keeps it out of the budget's band-exempt walk.
    fn giant_entry(cols: usize) -> CachedBlockLayout {
        CachedBlockLayout {
            output_len: 0,
            output_identity: 0,
            command_len: 0,
            collapsed: false,
            cols,
            foldable: false,
            base_row_count: 0,
            content_row_count: 0,
            is_clear: false,
            command_wrap_rows: 0,
            content: visual_rows::ContentTable {
                graphemes: vec![
                    visual_rows::GEntry {
                        byte_offset: 0,
                        width: 1,
                        flags: 0,
                    };
                    GIANT_TABLE_EXEMPT_BYTES
                        / std::mem::size_of::<visual_rows::GEntry>()
                        + 1
                ],
                line_meta: Vec::new(),
                screen_origin: false,
                trailing_trim_lines: 0,
            },
            stale_output_rows: 0,
            width: visual_rows::WidthTable {
                rows: Vec::new(),
                line_row_base: vec![0],
                hint_rows: 0,
            },
            degraded: false,
        }
    }

    /// Insert the giant entry for `id` keeping the incremental byte total
    /// exact (child-module escape hatch for the private entries map).
    fn install_giant(cache: &mut BlockLayoutCache, id: u64, cols: usize) {
        let layout = giant_entry(cols);
        let dropped = cache
            .entries
            .insert(id, layout)
            .map(|old| old.estimated_table_bytes())
            .unwrap_or(0);
        let added = cache.get(id).estimated_table_bytes();
        cache.table_bytes_total = cache.table_bytes_total - dropped + added;
    }

    /// §五-1: a FOREIGN giant is evictable (the giant exemption only guards
    /// the current set's degrade decisions), and the CURRENT oldest giant
    /// stays exempt from degradation — the over-budget walk skips it.
    #[test]
    fn foreign_giant_evictable_and_current_giant_exempt() {
        let tab_a: Vec<Block> = vec![mk_block(1, "alpha\n")];
        let tab_b: Vec<Block> = (2..=6).map(|id| mk_block(id, "x\n")).collect();
        let mut cache = BlockLayoutCache::default();
        cache.sync_blocks(&tab_a, 80, full_band());
        cache.sync_blocks(&tab_b, 80, full_band());
        install_giant(&mut cache, 1, 80); // foreign giant
        install_giant(&mut cache, 2, 80); // current, oldest walk candidate
        let giant_bytes = cache.get(1).estimated_table_bytes();
        let total = cache.table_bytes_total();
        // Stays over budget even after the foreign giant is evicted → the
        // degrade walk runs and must skip the current giant.
        let budget = total - giant_bytes - 1;

        cache.enforce_table_budget(&tab_b, no_band(), budget);

        assert!(cache.get_if_cached(1).is_none(), "foreign giant evicted");
        assert!(
            cache.get_metrics_tombstone(1).is_some(),
            "the evicted giant captures its tombstone"
        );
        assert!(!cache.get(2).degraded, "current giant exempt from degrade");
        assert_eq!(
            cache.get(2).estimated_table_bytes(),
            giant_bytes,
            "current giant's tables intact"
        );
        for id in 3..=6 {
            assert!(
                !cache.get(id).degraded,
                "newest-4 exemption holds (id {id})"
            );
        }
        assert!(cache.table_bytes_total() > budget, "legally over budget");
    }

    /// §五-1: an empty-handed arm (over budget, zero foreign entries) must
    /// not panic, must not change state, and must NOT clear the F1 memo —
    /// only an ACTUAL eviction clears it.
    #[test]
    fn eviction_arm_empty_handed_leaves_state_and_memo_intact() {
        // Memo-armed steady state: giant at the oldest candidate + newest-4
        // ⇒ the walk exhausts (same shape as budget_tests' memo test).
        let blocks: Vec<Block> = (1..=6).map(|i| mk_block(i, "alpha\n")).collect();
        let mut cache = BlockLayoutCache::default();
        cache.sync_blocks(&blocks, 80, full_band());
        install_giant(&mut cache, 1, 80);
        let giant_bytes = cache.get(1).estimated_table_bytes();
        let budget = cache.table_bytes_total() - giant_bytes - 1;
        cache.enforce_table_budget(&blocks, no_band(), budget);
        assert!(cache.no_degradable_candidates, "setup: memo armed");
        let entries_before = cache.entries.len();
        let total_before = cache.table_bytes_total();

        // The arm scans, evicts nothing, and the memo stays armed.
        cache.enforce_table_budget(&blocks, no_band(), budget);

        assert_eq!(cache.entries.len(), entries_before, "nothing evicted");
        assert_eq!(cache.table_bytes_total(), total_before);
        assert!(
            cache.no_degradable_candidates,
            "an empty-handed arm must not clear the memo"
        );
    }

    /// §五-1 (truth table, empty tab): an empty current set makes EVERY
    /// entry foreign — the arm is a full-eviction trigger with the same
    /// ascending / stop-at-budget semantics.
    #[test]
    fn empty_current_set_evicts_all_entries_foreign() {
        let tab_a: Vec<Block> = (1..=3).map(|id| mk_block(id, "alpha\n")).collect();
        let mut cache = BlockLayoutCache::default();
        cache.sync_blocks(&tab_a, 80, full_band());
        let total = cache.table_bytes_total();
        let b1 = cache.get(1).estimated_table_bytes();
        let b2 = cache.get(2).estimated_table_bytes();
        // Budget fits only the oldest entry → evicts 1, then 2, then stops.
        let budget = total - b1 - b2;

        cache.enforce_table_budget(&[], no_band(), budget);

        assert!(cache.get_if_cached(1).is_none() && cache.get_if_cached(2).is_none());
        assert!(cache.get_if_cached(3).is_some(), "stop-at-budget holds");
        assert!(
            cache.get_metrics_tombstone(3).is_none(),
            "no evict, no capture"
        );
        assert_eq!(cache.table_bytes_total(), cache.recomputed_table_bytes());
    }

    /// §5-2 orphan nail (F7-1d §2.4): a set-rotation sync clears degraded
    /// markers for vanished blocks — otherwise `pending_ids_empty()` goes
    /// permanently false (every frame pays the pending band walk) and the
    /// re-clamp flag never self-clears.
    #[test]
    fn set_rotation_clears_degraded_markers_for_vanished_blocks() {
        let tab_a: Vec<Block> = (1..=5).map(|id| mk_block(id, "alpha\n")).collect();
        let tab_b: Vec<Block> = (6..=7).map(|id| mk_block(id, "gamma\n")).collect();
        let mut cache = BlockLayoutCache::default();
        cache.sync_blocks(&tab_a, 80, full_band());
        // Degrade the oldest (outside the newest-4) via a pinched budget.
        let total = cache.table_bytes_total();
        let b1 = cache.get(1).estimated_table_bytes();
        cache.enforce_table_budget(&tab_a, no_band(), total - b1 + std::mem::size_of::<u32>());
        assert!(cache.degraded_ids.contains(&1), "setup: id 1 degraded");

        cache.sync_blocks(&tab_b, 80, full_band());

        assert!(
            !cache.degraded_ids.contains(&1),
            "orphan degraded marker cleared by the set-rotation retain"
        );
    }

    /// §五-1/§5-3 (F7-1d rewrite of
    /// `eviction_keeps_table_bytes_total_equal_to_recompute`): the per-item
    /// byte-total subtraction keeps the incremental total equal to a full
    /// recompute across a budget eviction (budget_tests' sequence pin covers
    /// the whole lifecycle).
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

        // F7-1d: the switch retains tab A; the budget eviction frees it.
        cache.sync_blocks(&tab_b, 80, full_band());
        assert!(cache.get_if_cached(1).is_some(), "foreign entries retained");
        let total = cache.table_bytes_total();
        let a_bytes: usize = (1..=3)
            .map(|id| cache.get(id).estimated_table_bytes())
            .sum();
        cache.enforce_table_budget(&tab_b, no_band(), total - a_bytes);
        assert!(cache.get_if_cached(1).is_none());
        assert_eq!(
            cache.table_bytes_total(),
            cache.recomputed_table_bytes(),
            "incremental total == full recompute after eviction"
        );
        assert!(cache.table_bytes_total() < tab_a_bytes);
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

    /// §五-1: `invalidate()` (test-only explicit eviction) must NOT capture
    /// a tombstone — behavior-unchanged declaration: capture lives ONLY in
    /// the budget eviction arm.
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
