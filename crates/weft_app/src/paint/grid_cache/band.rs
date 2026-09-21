//! M6-b (PLAN_M6 §三 B-1/B-2/B-5): band-gated sync classification for
//! `BlockLayoutCache`.
//!
//! During a live drag `cols` changes every frame, which defeats the
//! append-only fast path and would WidthOnly-rebuild EVERY finished block
//! (~11.5ms per 1MiB block). The band gates that: blocks intersecting the
//! viewport band — or BELOW it (update side, closer to the content bottom) —
//! rebuild immediately; blocks fully ABOVE it (old history) defer, keeping a
//! stale-but-self-consistent entry (one height per block, prefix sum stays
//! monotonic — B-4.1). Pending blocks are band-checked on every sync (B-1)
//! and drained one-per-idle-frame by the pump (B-5).

use super::{BandSync, BlockLayoutCache, CachedBlockLayout};
use weft_core::blocks::Block;

/// M6-b: freshness verdict for one cached entry, shared by `ensure_cached`
/// and the band classification ([`classify`]) so the two can't disagree.
pub(super) enum RebuildKind {
    None,
    WidthOnly,
    Both,
}

/// Single freshness source for the whole cache: the sync classification and
/// `ensure_cached` both read THIS verdict.
pub(super) fn rebuild_kind(c: &CachedBlockLayout, block: &Block, cols: usize) -> RebuildKind {
    let output_same = c.output_len == block.output.len()
        && c.output_identity == block.output.as_ptr() as usize
        && c.content.screen_origin == block.screen_origin;
    if output_same && c.command_len == block.command.len() {
        if c.cols != cols || c.collapsed != block.collapsed {
            RebuildKind::WidthOnly
        } else {
            RebuildKind::None
        }
    } else {
        // New output allocation: both layers are stale.
        RebuildKind::Both
    }
}

impl BandSync {
    /// Derive the band both sync callers pass (paint: block_view.rs, hit
    /// testing: rows.rs). The viewport window `scroll..scroll+viewport` is
    /// padded with ≥1 viewport of overscan per side, which absorbs the layout
    /// pass's header heights, ±1-block binary-search slack and the clear
    /// spacer (all shift estimates DOWNWARD only — see `classify`).
    pub(crate) fn for_viewport(scroll_rows: usize, viewport_rows: usize) -> Self {
        let viewport = viewport_rows.max(1);
        BandSync {
            low_rows: scroll_rows.saturating_sub(viewport),
            high_rows: scroll_rows.saturating_add(viewport * 2),
        }
    }

    /// Is the block entirely ABOVE the band (older-history side, i.e. its
    /// bottom edge is farther from the content bottom than the band top)?
    /// In this bottom-anchored coordinate system "above" = larger values.
    /// The negation covers both "intersects the band" and "below it (update
    /// side)": any intersecting block has `bottom < high_rows` too, so the
    /// bottom edge alone decides the update-side test.
    fn fully_above(&self, bottom: usize) -> bool {
        bottom >= self.high_rows
    }
}

/// Per-block sync decision (B-2 truth table).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Placement {
    /// Entry fresh — nothing to rebuild (a hit, pre-M6-b behavior).
    Fresh,
    /// Rebuild this frame (uncached block, content changed, or cols/collapse
    /// mismatch intersecting-or-below the band).
    Immediate,
    /// WidthOnly mismatch entirely above the band — defer (B-2).
    Defer,
}

/// Classify one block against the band. `stale_bottom` is the block's bottom
/// edge in PRE-rebuild `base_row_count` units (sum over newer blocks).
///
/// Direction analysis (two walks, this is walk 1): header heights and clear
/// spacers are absent from `base_row_count`, biasing estimates DOWNWARD —
/// that direction is merely cost ("needlessly immediate"). The bias is NOT a
/// universal guarantee though: on a cols-grow frame an in-band giant block
/// rebuilds SHORTER, so this walk's pre-rebuild estimate can sit ABOVE a
/// visible block's true position and misclassify it `Defer`. Correctness is
/// restored SAME-FRAME by the second walk (`rebuild_pending_in_band`): it
/// runs after the immediate rebuilds collected here are applied, so its
/// intervals read post-rebuild bases on the update side and any deferred
/// block whose true position crosses into the band is rebuilt before the
/// layout pass runs. Pinned by
/// `in_band_giant_shrink_corrects_below_band_deferral_same_frame`.
pub(super) fn classify(
    entry: Option<&CachedBlockLayout>,
    block: &Block,
    cols: usize,
    band: &BandSync,
    stale_bottom: usize,
) -> Placement {
    let Some(c) = entry else {
        // Uncached block: Both-build now — keeps the `get()` precondition and
        // scroll geometry correct for new blocks (≤1MiB Once ~15-20ms).
        return Placement::Immediate;
    };
    match rebuild_kind(c, block, cols) {
        RebuildKind::None => Placement::Fresh,
        // Content changed (new output allocation): correctness first, never
        // deferred.
        RebuildKind::Both => Placement::Immediate,
        RebuildKind::WidthOnly => {
            if band.fully_above(stale_bottom) {
                Placement::Defer
            } else {
                Placement::Immediate
            }
        }
    }
}

/// Accumulate each block's `[bottom, top)` interval walking
/// newest-to-oldest (`below` = sum of newer blocks' `base_row_count`),
/// invoking `f` with the block's index into `blocks`. `base_row_count`
/// excludes header heights and clear spacers (downward bias, see
/// `classify`).
///
/// Within ONE walk every interval comes from the entry bases read at that
/// walk's own point in time. Walk 1 (`sync_classified`) deliberately reads
/// the PRE-rebuild prefix — collect decisions first, apply rebuilds after —
/// so deferral decisions are mutually consistent. Walk 2
/// (`rebuild_pending_in_band`) deliberately runs AFTER walk 1's rebuilds
/// were applied, so pending blocks' intervals are REcomputed from
/// post-rebuild (true) bases: that second reading is the same-frame
/// correction for walk 1's giant-shrink misclassification, not a redundant
/// re-walk. Reordering the two walks would silently reintroduce a one-frame
/// misplacement (see the regression test).
///
/// M6-c: also the shared interval source for the budget's band-intersection
/// exemptions (`enforce_table_budget`) — pub(super) so the parent module can
/// reuse the single walk instead of growing a drifting second one.
pub(super) fn for_each_stale_interval(
    cache: &BlockLayoutCache,
    blocks: &[Block],
    mut f: impl FnMut(usize, usize, usize),
) {
    let mut below = 0usize;
    for index in (0..blocks.len()).rev() {
        let base = cache
            .get_if_cached(blocks[index].id.0)
            .map(|c| c.base_row_count)
            .unwrap_or(0);
        f(index, below, below + base);
        below += base;
    }
}

impl BlockLayoutCache {
    /// B-2: classified non-append sync. Walks newest-to-oldest over the OLD
    /// prefix (all intervals read pre-rebuild), then rebuilds the immediate
    /// set. Deferred ids replace the pending set wholesale — blocks that
    /// vanished from the session drop out here.
    pub(super) fn sync_classified(&mut self, blocks: &[Block], cols: usize, band: BandSync) {
        let mut deferred_now = Vec::new();
        let mut rebuild_now: Vec<usize> = Vec::new();
        for_each_stale_interval(self, blocks, |index, bottom, _top| {
            let block = &blocks[index];
            let entry = self.get_if_cached(block.id.0);
            match classify(entry, block, cols, &band, bottom) {
                Placement::Fresh | Placement::Immediate => rebuild_now.push(index),
                Placement::Defer => deferred_now.push(block.id.0),
            }
        });
        for index in rebuild_now {
            self.ensure_cached(&blocks[index], cols);
        }
        self.deferred_ids = deferred_now;
    }

    /// B-1: pending-set band check — runs on EVERY sync (append-only path
    /// included) so a deferred block scrolled into the band is rebuilt the
    /// same frame (P1: streaming gates the idle pump off, so this is the only
    /// correction path while output flows). Appends never shrink the set, so
    /// every pending id is still present; orphans are impossible.
    ///
    /// M6-c: the pending set is `deferred_ids ∪ degraded_ids`. A DEGRADED
    /// block (budget dropped its tables) intersecting the band rebuilds
    /// BOTH: the degraded verdict takes priority over the deferred one, and
    /// `ensure_cached` enforces it by checking the flag before the
    /// three-state verdict — a WidthOnly over the emptied L1 would produce
    /// empty tables (never allowed). An id sitting in both sets (degraded,
    /// then cols-mismatch deferred above the band) dedupes through this
    /// single rebuild + `remove_pending`.
    pub(super) fn rebuild_pending_in_band(
        &mut self,
        blocks: &[Block],
        cols: usize,
        band: BandSync,
    ) {
        if self.deferred_ids.is_empty() && self.degraded_ids.is_empty() {
            return;
        }
        let mut rebuild_now: Vec<usize> = Vec::new();
        for_each_stale_interval(self, blocks, |index, bottom, _top| {
            let id = blocks[index].id.0;
            // Intersecting or below the band (update side): rebuild now.
            // `!fully_above(bottom)` alone IS that test — every intersecting
            // block also has `bottom < high_rows` (P3-1: the top/low terms
            // of a rectangle-intersect check add nothing here, the block's
            // bottom edge decides).
            let pending = self.deferred_ids.contains(&id) || self.degraded_ids.contains(&id);
            if pending && !band.fully_above(bottom) {
                rebuild_now.push(index);
            }
        });
        for index in rebuild_now {
            // Degraded → Both via ensure_cached's degraded-first check;
            // plain deferred → WidthOnly via the normal verdict.
            self.ensure_cached(&blocks[index], cols);
            let id = blocks[index].id.0;
            self.remove_pending(id);
        }
    }

    /// B-5: idle convergence pump — rebuild at most `max_n` deferred blocks
    /// per call: band-intersecting ones first, then the pending blocks
    /// nearest the band (walking newest-to-oldest yields ascending distance).
    /// Refreshes the prefix sum so the same-frame layout pass and scroll
    /// metrics read corrected heights.
    ///
    /// Known boundary: rebuilding one pending block shifts older pending
    /// blocks' prefix positions by the same delta; a single-block rebuild
    /// moving a block across the band's full overscan is not reachable by
    /// drag-sized cols steps, and the next sync re-classifies anyway.
    pub(crate) fn pump_deferred(
        &mut self,
        blocks: &[Block],
        cols: usize,
        band: BandSync,
        max_n: usize,
    ) {
        // M6-b P2-1: drain only on cols-stable frames — during a drag every
        // frame re-defers whatever the pump rebuilt (pure waste).
        if self.deferred_ids.is_empty() || max_n == 0 || !self.last_sync_stable {
            return;
        }
        let mut candidates: Vec<(usize, usize)> = Vec::new();
        for_each_stale_interval(self, blocks, |index, bottom, _top| {
            if self.deferred_ids.contains(&blocks[index].id.0) {
                candidates.push((bottom, index));
            }
        });
        // Non-above (intersect/below) first, then nearest-to-band first.
        candidates.sort_by_key(|&(bottom, _)| (u8::from(band.fully_above(bottom)), bottom));
        for &(_, index) in candidates.iter().take(max_n) {
            // M6-c: an id degraded AND deferred gets a Both rebuild here
            // (ensure_cached's degraded-first check) and leaves both sets.
            self.ensure_cached(&blocks[index], cols);
            self.remove_pending(blocks[index].id.0);
        }
        self.build_prefix_sum(blocks);
    }

    /// M6-b: entries currently deferred above the sync band (bench/tests).
    #[cfg(test)]
    pub(crate) fn deferred_count(&self) -> usize {
        self.deferred_ids.len()
    }

    /// M6-b B-3 observability: one `completed_output_rows` fallback fired on
    /// the metrics path (called from `block_component`).
    pub(crate) fn note_metrics_fallback(&self) {
        self.metrics_fallback_rebuilds
            .set(self.metrics_fallback_rebuilds.get() + 1);
    }

    /// M6-b B-3: how often the metrics path had no entry and computed output
    /// rows from scratch. Deferred/degraded scenarios must keep this at 0.
    #[cfg(test)]
    pub(crate) fn metrics_fallback_rebuilds(&self) -> usize {
        self.metrics_fallback_rebuilds.get()
    }
}

#[cfg(test)]
mod tests;
