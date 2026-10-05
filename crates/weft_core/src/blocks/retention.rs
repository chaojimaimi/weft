//! v1.11.2 X4 (PLAN_v1112 §1): in-memory block retention.
//!
//! Extracted from `blocks.rs` to keep that file within its
//! architecture-gate ceiling; this is a child module, so the impl below can
//! reach `BlockTracker`'s private fields exactly like `continuation.rs`.
//!
//! Semantics (§1.1 — do not weaken):
//! - only HEAD pops from the history Vec (continuation owns the tail);
//! - every popped id joins [`BlockTracker::evicted_ids`] and leaves
//!   `dirty_blocks` / `screen_owned_blocks` / `loaded_ids`;
//! - `session_produced_block_ids` unions evicted ids so per-tab snapshots
//!   keep referencing DB-resident history across a restart;
//! - `retained_limit == 0` disables retention entirely.

use super::{Block, BlockTracker};
use std::collections::HashSet;

/// v1.11.2 X4 (PLAN_v1112 §1): default cap on in-memory blocks per tracker.
/// Blocks beyond this are popped from the HEAD of the history Vec (oldest
/// first); their ids move to [`BlockTracker::evicted_ids`] so per-tab
/// snapshot semantics (`session_produced_block_ids`) are unchanged. 2000
/// finished commands ≈ the practical ceiling of a long-lived tab session;
/// older history stays reachable in SQLite via the panel's "load older"
/// path. `retained_limit = 0` disables retention entirely (test escape hatch).
pub const DEFAULT_BLOCKS_RETAINED_LIMIT: usize = 2000;

impl BlockTracker {
    /// v1.11.2 X4 (PLAN_v1112 §1.2): configure the in-memory retention cap.
    /// `0` disables retention (blocks accumulate unbounded — the pre-1.11.2
    /// behavior, kept as a test escape hatch).
    pub fn set_retained_limit(&mut self, limit: usize) {
        self.retained_limit = limit;
        // Re-enforce immediately so lowering the limit takes effect without
        // waiting for the next finalize.
        self.enforce_retention();
    }

    /// Evicted block ids (test/diagnostic observability).
    pub fn evicted_ids(&self) -> &HashSet<u64> {
        &self.evicted_ids
    }

    /// v1.11.2 X4: pop blocks from the HEAD of [`Self::blocks`] until the Vec
    /// is at `retained_limit`. Called after every finalize push.
    ///
    /// Invariants (PLAN_v1112 §1.1):
    /// - Only head pops — continuation logic depends on `.last()` and tail
    ///   integrity; the newest-last invariant and the "index ↔ rising id"
    ///   render assumption are untouched.
    /// - Every popped id joins `evicted_ids` and leaves `dirty_blocks` /
    ///   `screen_owned_blocks` / `loaded_ids` so the three side sets shrink
    ///   in lockstep with the Vec.
    /// - Timing note (§1.4): finalize pushed the same block to `unpersisted`
    ///   moments earlier. A popped-but-unpersisted block therefore exists in
    ///   BOTH `evicted_ids` and `unpersisted`; the app's drain persists it
    ///   normally (the id lands in SQLite), while `evicted_ids` keeps this
    ///   tab's snapshot referencing it. The interleaving is benign — no lock
    ///   needed.
    pub(super) fn enforce_retention(&mut self) {
        if self.retained_limit == 0 {
            return;
        }
        // rust-reviewer v1.11.2 Minor-4: user-pinned pages (panel "load
        // older") are exempt from eviction — but they must not crowd out
        // fresh session history either. Effective cap = the newest
        // `retained_limit` NON-PINNED blocks plus every pinned block:
        // without the non-pinned denominator a large pinned page under a
        // small limit would evict the block that JUST finalized (caught by
        // load_older_to_front_prepends_in_time_order). Memory stays bounded
        // at retained_limit + pinned because pinning only happens on
        // explicit user action.
        let pinned = &self.user_pinned_ids;
        let non_pinned_len = self
            .blocks
            .iter()
            .filter(|b| !pinned.contains(&b.id.0))
            .count();
        let mut excess = non_pinned_len.saturating_sub(self.retained_limit);
        if excess == 0 {
            return;
        }
        let mut evicted_ids: Vec<u64> = Vec::with_capacity(excess);
        self.blocks.retain(|block| {
            if excess == 0 || pinned.contains(&block.id.0) {
                return true;
            }
            excess -= 1;
            evicted_ids.push(block.id.0);
            false
        });
        for id in evicted_ids {
            self.evicted_ids.insert(id);
            self.dirty_blocks.remove(&id);
            self.screen_owned_blocks.remove(&id);
            self.loaded_ids.remove(&id);
        }
    }

    /// v1.7.6: IDs of blocks produced THIS session only (excludes blocks
    /// loaded via [`load_blocks`] on startup/Restore). Used by the app-layer
    /// `Tab::to_snapshot` to persist per-tab block ownership so each tab
    /// can restore only its own history on next launch.
    ///
    /// v1.11.2 X4: ids evicted from memory by retention are folded back in —
    /// they were produced this session too, and per-tab restore must keep
    /// referencing them (they live in SQLite even though the Vec forgot them).
    ///
    /// v1.12.24 review P1-2: ids paged in via the panel's GLOBAL "load
    /// older" query ([`load_older_to_front`]) are excluded as well — they
    /// are other tabs' blocks, not this tab's history (per-tab isolation).
    pub fn session_produced_block_ids(&self) -> Vec<u64> {
        // Invariant: evicted_ids ∩ blocks = ∅ (a pop only ever LEAVES the
        // Vec, `load_older_to_front` removes re-loaded ids from evicted_ids,
        // the allocator hands out monotonic-unique ids, and `load_blocks`
        // runs only at startup) — so a plain extend can never duplicate.
        // rust-reviewer v1.11.2 Major-2: the previous per-id `contains`
        // scan made this O(evicted × retained) on the 1 Hz to_snapshot hot
        // path; do NOT reintroduce it without proving the invariant broke.
        let mut ids = Vec::with_capacity(self.blocks.len() + self.evicted_ids.len());
        ids.extend(
            self.blocks
                .iter()
                .filter(|b| {
                    !self.loaded_ids.contains(&b.id.0) && !self.load_older_ids.contains(&b.id.0)
                })
                .map(|b| b.id.0),
        );
        ids.extend(self.evicted_ids.iter().copied());
        ids
    }

    /// v1.12.24 (N-3): full lineage for snapshots — blocks produced this
    /// session UNION those loaded from a previous restore. The v1.7.6
    /// session-only design dropped the previous generation's recall on every
    /// restart-restore cycle (audit N-3).
    pub fn lineage_block_ids(&self) -> Vec<u64> {
        let mut ids = self.session_produced_block_ids();
        ids.extend(self.loaded_ids.iter().copied());
        ids.sort_unstable();
        ids.dedup();
        ids
    }

    /// v1.11.2 X4 (PLAN_v1112 §1.3): re-insert previously persisted (older)
    /// blocks at the FRONT of the history, preserving time order. Each id:
    /// recorded in `load_older_ids`, marked dirty (vertex rebuild), cleared
    /// from `evicted_ids` if retention had evicted it, and PINNED against
    /// future eviction (review Minor-4 — the user asked for this page; the
    /// next finalize must not drop it).
    ///
    /// v1.12.24 review P1-2: the "load older" query is GLOBAL (no tab
    /// filter) — these blocks belong to other tabs, so the id lands in
    /// `load_older_ids` (NOT `loaded_ids`): they stay visible in the panel
    /// / block view but never enter `session_produced_block_ids` /
    /// `lineage_block_ids`, keeping the v1.7.6 per-tab snapshot isolation.
    pub fn load_older_to_front(&mut self, mut older: Vec<Block>) {
        if older.is_empty() {
            return;
        }
        // The DB query yields newest-first; the history Vec wants oldest
        // first overall, so re-sort ascending before prepending. Id is the
        // tiebreak so same-millisecond blocks keep the "index ↔ rising id"
        // render assumption (rust-reviewer v1.11.2 Minor-5).
        older.sort_by_key(|b| (b.started_at, b.id.0));
        for b in &older {
            self.ids.observe(b.id.0);
            self.load_older_ids.insert(b.id.0);
            self.dirty_blocks.insert(b.id.0);
            self.evicted_ids.remove(&b.id.0);
            self.user_pinned_ids.insert(b.id.0);
        }
        let existing = std::mem::take(&mut self.blocks);
        self.blocks = older;
        self.blocks.extend(existing);
    }
}
