//! Bounded FIFO cache for completed BlockView styled-line vertices.
//!
//! v1.4.1: caches the vertex output of `push_block_output_text` for lines
//! belonging to **completed** blocks (never live/in-flight blocks). On a hit,
//! the cached vertices are copied and translated to the target (x, y);
//! UV/color components are unchanged.
//!
//! See `docs/V14_IMPLEMENTATION_PLAN.md` §5 for the full design.

use std::sync::Arc;

/// Maximum cached bytes across all entries. The plan specifies 16 MiB.
pub(crate) const MAX_CACHE_BYTES: usize = 16 * 1024 * 1024;
/// Safety cap on entry count to bound the key-vector scan. The byte budget
/// is the authoritative limit; this only prevents pathological tiny-entry
/// proliferation.
pub(crate) const MAX_CACHE_ENTRIES: usize = 512;

/// Identity key for a single styled line chunk. Two keys match only when
/// every field is identical — pane, block, line position, wrap chunk,
/// geometry, render generation, palette, and fallback color all agree.
///
/// `pane_session_id` uses `Pane::pane_session_id` (global, monotonic) so
/// entries from a closed pane are naturally evicted by namespace mismatch
/// rather than relying on explicit invalidation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct StyledLineCacheKey {
    pub(crate) pane_session_id: u64,
    pub(crate) block_id: u64,
    pub(crate) line_idx: u32,
    pub(crate) chunk_idx: u32,
    pub(crate) cols: u32,
    pub(crate) char_offset: u32,
    pub(crate) render_generation: u64,
    pub(crate) palette_fingerprint: u64,
    /// `f32::to_bits()` for each of the 4 fallback color channels.
    pub(crate) fallback_fg: [u32; 4],
}

/// A cached vertex slice plus the Arc identities it was built from. The Arcs
/// are held so the allocator cannot reuse the underlying allocation for a
/// different block — a key match without Arc identity match is treated as
/// a miss (see `StyledLineCache::lookup`).
pub(crate) struct StyledLineCacheEntry {
    /// Source text Arc identity guard. `Arc::ptr_eq` against the current
    /// block's output text is required for a hit.
    pub(crate) source: Arc<str>,
    /// Styled output Arc identity guard. When the block has no styled output
    /// this is `None`; the lookup still requires `None`-ness to match.
    pub(crate) styled: Option<Arc<weft_core::blocks::StyledOutput>>,
    /// Vertices in LOCAL coordinates — origin (0, 0). The caller translates
    /// x/y on hit via `translate_and_append_shifts_only_xy`.
    pub(crate) vertices: Arc<[f32]>,
    /// Bytes charged against the cache budget: `vertices.len() * 4` plus
    /// a small per-entry overhead for the Arcs and key.
    pub(crate) bytes: usize,
}

impl StyledLineCacheEntry {
    /// Per-entry overhead added to the raw vertex bytes when charging
    /// against the budget. Covers the key, two Arcs, and the entry struct
    /// itself (roughly — the exact layout isn't load-bearing, only that
    /// the budget is bounded).
    const ENTRY_OVERHEAD: usize = 256;
}

/// Bounded FIFO cache. Lookups are O(n) over entries — the cap is small
/// (≤512) and the common path is "hit on the most recently inserted line",
/// which is the last entry scanned.
///
/// The cache is **not** a general-purpose LRU. Completed block lines are
/// immutable, so a simple FIFO with byte-budget eviction is sufficient and
/// avoids the bookkeeping cost of an LRU.
pub(crate) struct StyledLineCache {
    entries: Vec<(StyledLineCacheKey, StyledLineCacheEntry)>,
    total_bytes: usize,
    hits: u64,
    misses: u64,
    /// v1.4.1: monotonically increasing generation counter. Bumped on any
    /// change that invalidates cached vertices (atlas rebuild, font/scale/
    /// line-height/theme/opacity change). The current value is embedded in
    /// every cache key, so a bump causes all existing entries to miss on
    /// the next lookup and be evicted by FIFO as new entries are inserted.
    /// Starts at 1; `bump_generation` uses `wrapping_add(1).max(1)` so 0
    /// is never produced even after 2^64 bumps.
    generation: u64,
}

impl Default for StyledLineCache {
    fn default() -> Self {
        Self::new()
    }
}

impl StyledLineCache {
    pub(crate) fn new() -> Self {
        Self {
            entries: Vec::new(),
            total_bytes: 0,
            hits: 0,
            misses: 0,
            generation: 1,
        }
    }

    /// Number of cached entries.
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    /// Total bytes charged against the budget.
    pub(crate) fn bytes(&self) -> usize {
        self.total_bytes
    }

    /// Cache hit count (for frame_trace reporting).
    #[cfg(test)]
    pub(crate) fn hits(&self) -> u64 {
        self.hits
    }

    /// Cache miss count (for frame_trace reporting).
    #[cfg(test)]
    pub(crate) fn misses(&self) -> u64 {
        self.misses
    }

    /// Current render generation. Embedded in every cache key at construction
    /// time so a generation bump invalidates all existing entries.
    pub(crate) fn generation(&self) -> u64 {
        self.generation
    }

    /// Bump the render generation and clear all entries. Called on atlas
    /// rebuild, font/scale/line-height/theme/opacity change — any event that
    /// invalidates the baked-in glyph UV / color / geometry of cached
    /// vertices. Hit/miss counters are preserved (lifetime counters).
    pub(crate) fn bump_generation(&mut self) {
        self.generation = self.generation.wrapping_add(1).max(1);
        self.entries.clear();
        self.total_bytes = 0;
    }

    /// Drain hit/miss counters (returns the values and resets them to 0).
    /// Called at frame end to populate `FrameCounters.styled_cache_hits/misses`.
    /// Mirrors `BlockLayoutCache::take_hit_miss_counts` so the renderer drains
    /// both caches with the same pattern.
    pub(crate) fn take_hit_miss_counts(&mut self) -> (u64, u64) {
        let h = self.hits;
        let m = self.misses;
        self.hits = 0;
        self.misses = 0;
        (h, m)
    }

    /// Remove all entries. Called when `render_generation` changes (atlas
    /// rebuild, font/scale/line-height/theme/opacity change). Equivalent to
    /// `bump_generation` without the generation increment.
    #[cfg(test)]
    pub(crate) fn clear(&mut self) {
        self.entries.clear();
        self.total_bytes = 0;
    }

    /// Reset hit/miss counters (called at the start of each frame).
    #[cfg(test)]
    pub(crate) fn reset_stats(&mut self) {
        self.hits = 0;
        self.misses = 0;
    }

    /// Look up a cached entry by key + Arc identity. Returns the cached
    /// vertices on hit; increments `hits`/`misses` accordingly.
    ///
    /// Arc identity is required: a key match with a different `Arc<str>`
    /// means the allocator reused the block id for a new block, and the
    /// cached vertices are stale.
    pub(crate) fn lookup(
        &mut self,
        key: &StyledLineCacheKey,
        source: &Arc<str>,
        styled: Option<&Arc<weft_core::blocks::StyledOutput>>,
    ) -> Option<Arc<[f32]>> {
        for (k, e) in &self.entries {
            if k == key
                && Arc::ptr_eq(&e.source, source)
                && styled_output_ptr_eq(e.styled.as_ref(), styled)
            {
                self.hits += 1;
                return Some(Arc::clone(&e.vertices));
            }
        }
        self.misses += 1;
        None
    }

    /// Insert a new entry, evicting oldest entries until the byte and entry
    /// budgets are satisfied. Entries exceeding `MAX_CACHE_BYTES` on their
    /// own are not cached (the caller still got their vertices the slow way).
    pub(crate) fn insert(
        &mut self,
        key: StyledLineCacheKey,
        source: Arc<str>,
        styled: Option<Arc<weft_core::blocks::StyledOutput>>,
        vertices: Vec<f32>,
    ) {
        let vertex_bytes = vertices.len() * std::mem::size_of::<f32>();
        let entry_bytes = vertex_bytes + StyledLineCacheEntry::ENTRY_OVERHEAD;

        // Refuse to cache entries that would single-handedly exceed the
        // budget — a 200+ column line with full styling could be ~23 KiB,
        // well within budget, but a pathological 1000-column line is not.
        if entry_bytes > MAX_CACHE_BYTES {
            return;
        }

        // Evict oldest entries (FIFO) until the new entry fits.
        while self.total_bytes + entry_bytes > MAX_CACHE_BYTES
            || self.entries.len() >= MAX_CACHE_ENTRIES
        {
            match self.entries.first() {
                Some((_, e)) => self.total_bytes = self.total_bytes.saturating_sub(e.bytes),
                None => break,
            }
            self.entries.remove(0);
        }

        self.total_bytes += entry_bytes;
        self.entries.push((
            key,
            StyledLineCacheEntry {
                source,
                styled,
                vertices: Arc::from(vertices.into_boxed_slice()),
                bytes: entry_bytes,
            },
        ));
    }
}

fn styled_output_ptr_eq(
    a: Option<&Arc<weft_core::blocks::StyledOutput>>,
    b: Option<&Arc<weft_core::blocks::StyledOutput>>,
) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(a), Some(b)) => Arc::ptr_eq(a, b),
        _ => false,
    }
}

/// Copy `vertices` and translate only the x/y position components (indices
/// 0 and 1 of each 12-float vertex), appending the result to `out`. UV
/// (indices 2–3) and color (indices 4–7, 8–11) components are unchanged.
///
/// Vertex layout (from `paint/primitives.rs::push_quad`):
/// ```text
/// [x, y, u, v, fg_r, fg_g, fg_b, fg_a, bg_r, bg_g, bg_b, bg_a]
/// ```
/// So positions are at indices `0, 1` of each 12-float group. A glyph vertex
/// (6 quads) follows the same layout.
pub(crate) fn translate_and_append_shifts_only_xy(
    out: &mut Vec<f32>,
    vertices: &[f32],
    dx: f32,
    dy: f32,
) {
    // Each vertex is 12 floats; index 0 = x, index 1 = y. We translate only
    // those two slots per vertex and copy the other 10 verbatim.
    out.reserve(vertices.len());
    let mut i = 0;
    while i + 12 <= vertices.len() {
        out.push(vertices[i] + dx);
        out.push(vertices[i + 1] + dy);
        out.extend_from_slice(&vertices[i + 2..i + 12]);
        i += 12;
    }
    // Trailing partial vertex (shouldn't happen in well-formed input, but
    // we copy it unchanged rather than panic).
    if i < vertices.len() {
        out.extend_from_slice(&vertices[i..]);
    }
}

/// Compute a 64-bit fingerprint of the 256-color palette. Used as a cache
/// key component so OSC 4/104 palette mutations invalidate cached vertices
/// that baked in resolved palette colors.
///
/// FNV-1a over the 256 × 4 byte RGBA channels. Sub-microsecond for 1 KiB
/// of input; called once per `build_block_view_vertices` (not per line).
pub(crate) fn palette_fingerprint(palette: &[weft_core::grid::Color; 256]) -> u64 {
    // FNV-1a 64-bit offset basis and prime.
    const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = FNV_OFFSET;
    for c in palette.iter() {
        hash ^= c.r as u64;
        hash = hash.wrapping_mul(FNV_PRIME);
        hash ^= c.g as u64;
        hash = hash.wrapping_mul(FNV_PRIME);
        hash ^= c.b as u64;
        hash = hash.wrapping_mul(FNV_PRIME);
        hash ^= c.a as u64;
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(pane: u64, block: u64, line: u32, chunk: u32) -> StyledLineCacheKey {
        StyledLineCacheKey {
            pane_session_id: pane,
            block_id: block,
            line_idx: line,
            chunk_idx: chunk,
            cols: 80,
            char_offset: 0,
            render_generation: 1,
            palette_fingerprint: 0,
            fallback_fg: [0; 4],
        }
    }

    fn src(s: &str) -> Arc<str> {
        Arc::from(s)
    }

    // ── lookup / insert basics ──────────────────────────────────────

    #[test]
    fn first_lookup_is_miss_second_is_hit() {
        let mut cache = StyledLineCache::new();
        let k = key(1, 10, 0, 0);
        let s = src("hello");
        assert!(cache.lookup(&k, &s, None).is_none());
        assert_eq!(cache.misses(), 1);
        assert_eq!(cache.hits(), 0);

        cache.insert(k, Arc::clone(&s), None, vec![1.0; 12]);
        assert!(cache.lookup(&k, &s, None).is_some());
        assert_eq!(cache.misses(), 1);
        assert_eq!(cache.hits(), 1);
    }

    #[test]
    fn cross_pane_does_not_hit() {
        let mut cache = StyledLineCache::new();
        let k_a = key(1, 10, 0, 0);
        let k_b = key(2, 10, 0, 0); // same block_id, different pane
        let s = src("same text");
        cache.insert(k_a, Arc::clone(&s), None, vec![1.0; 12]);
        assert!(cache.lookup(&k_b, &s, None).is_none());
    }

    #[test]
    fn source_arc_change_is_miss() {
        let mut cache = StyledLineCache::new();
        let k = key(1, 10, 0, 0);
        let s1 = src("hello");
        let s2 = src("hello"); // same content, different Arc
        cache.insert(k, Arc::clone(&s1), None, vec![1.0; 12]);
        // s2 is a different allocation → miss even though key matches
        assert!(cache.lookup(&k, &s2, None).is_none());
    }

    #[test]
    fn styled_arc_change_is_miss() {
        use weft_core::blocks::StyledOutput;
        let mut cache = StyledLineCache::new();
        let k = key(1, 10, 0, 0);
        let s = src("hello");
        let so1: Arc<StyledOutput> = Arc::new(StyledOutput::default());
        let so2: Arc<StyledOutput> = Arc::new(StyledOutput::default());
        cache.insert(k, Arc::clone(&s), Some(Arc::clone(&so1)), vec![1.0; 12]);
        // so2 is a different Arc → miss
        assert!(cache.lookup(&k, &s, Some(&so2)).is_none());
        // so1 matches → hit
        assert!(cache.lookup(&k, &s, Some(&so1)).is_some());
    }

    #[test]
    fn cols_change_is_miss() {
        let mut cache = StyledLineCache::new();
        let mut k = key(1, 10, 0, 0);
        let s = src("hello");
        cache.insert(k, Arc::clone(&s), None, vec![1.0; 12]);
        k.cols = 120;
        assert!(cache.lookup(&k, &s, None).is_none());
    }

    #[test]
    fn char_offset_change_is_miss() {
        let mut cache = StyledLineCache::new();
        let mut k = key(1, 10, 0, 0);
        let s = src("hello");
        cache.insert(k, Arc::clone(&s), None, vec![1.0; 12]);
        k.char_offset = 40;
        assert!(cache.lookup(&k, &s, None).is_none());
    }

    #[test]
    fn chunk_idx_change_is_miss() {
        let mut cache = StyledLineCache::new();
        let mut k = key(1, 10, 0, 0);
        let s = src("hello");
        cache.insert(k, Arc::clone(&s), None, vec![1.0; 12]);
        k.chunk_idx = 1;
        assert!(cache.lookup(&k, &s, None).is_none());
    }

    #[test]
    fn palette_fingerprint_change_is_miss() {
        let mut cache = StyledLineCache::new();
        let mut k = key(1, 10, 0, 0);
        let s = src("hello");
        cache.insert(k, Arc::clone(&s), None, vec![1.0; 12]);
        k.palette_fingerprint = 0xDEAD_BEEF;
        assert!(cache.lookup(&k, &s, None).is_none());
    }

    #[test]
    fn fallback_fg_change_is_miss() {
        let mut cache = StyledLineCache::new();
        let mut k = key(1, 10, 0, 0);
        let s = src("hello");
        cache.insert(k, Arc::clone(&s), None, vec![1.0; 12]);
        k.fallback_fg[0] = 1;
        assert!(cache.lookup(&k, &s, None).is_none());
    }

    #[test]
    fn render_generation_change_is_miss() {
        let mut cache = StyledLineCache::new();
        let mut k = key(1, 10, 0, 0);
        let s = src("hello");
        cache.insert(k, Arc::clone(&s), None, vec![1.0; 12]);
        k.render_generation = 2;
        assert!(cache.lookup(&k, &s, None).is_none());
    }

    // ── eviction / budget ────────────────────────────────────────────

    #[test]
    fn fifo_evicts_oldest_when_byte_budget_exceeded() {
        let mut cache = StyledLineCache::new();
        // Each entry: 1024*1024 floats = 4 MiB + 256 overhead ≈ 4 MiB.
        // MAX_CACHE_BYTES = 16 MiB. So at most 4 entries can coexist; the
        // 5th insert must evict the oldest.
        //
        // The cache uses Arc<str> identity as a guard, so we must hold the
        // SAME Arc across insert+lookup (cloning the Arc preserves the
        // allocation). Creating a fresh `Arc::from("text")` at lookup time
        // would be a different allocation and would miss by design.
        let big = vec![1.0; 1024 * 1024]; // 4 MiB of floats
        let shared_src = src("text");
        for i in 0..5 {
            let k = key(1, i as u64 * 10, 0, 0);
            cache.insert(k, Arc::clone(&shared_src), None, big.clone());
        }
        // 5 entries × ~4 MiB = 20 MiB > 16 MiB → at most 4 survive
        assert!(cache.bytes() <= MAX_CACHE_BYTES);
        assert!(cache.len() <= 4);
        // The first inserted entry (block_id=0) should have been evicted
        let k0 = key(1, 0, 0, 0);
        assert!(cache.lookup(&k0, &shared_src, None).is_none());
        // The last inserted entry (block_id=40) should still be there
        let k4 = key(1, 40, 0, 0);
        assert!(cache.lookup(&k4, &shared_src, None).is_some());
    }

    #[test]
    fn entry_cap_limits_count() {
        let mut cache = StyledLineCache::new();
        // Insert 600 tiny entries; the cap is 512.
        for i in 0..600 {
            let k = key(1, i, 0, 0);
            let s = src("x");
            cache.insert(k, s, None, vec![1.0; 12]);
        }
        assert!(cache.len() <= MAX_CACHE_ENTRIES);
        assert!(cache.len() >= 500); // most survived
    }

    #[test]
    fn oversized_entry_is_not_cached() {
        let mut cache = StyledLineCache::new();
        let k = key(1, 10, 0, 0);
        let s = src("huge");
        // 5 MiB of floats → 20 MiB bytes → exceeds 16 MiB budget single-handedly
        let huge = vec![1.0; 5 * 1024 * 1024];
        cache.insert(k, Arc::clone(&s), None, huge);
        assert_eq!(cache.len(), 0);
        assert_eq!(cache.bytes(), 0);
        assert!(cache.lookup(&k, &s, None).is_none());
    }

    #[test]
    fn total_bytes_never_exceeds_budget() {
        let mut cache = StyledLineCache::new();
        for i in 0..100 {
            let k = key(1, i, 0, 0);
            let s = src("text");
            cache.insert(k, s, None, vec![1.0; 1024]);
            assert!(
                cache.bytes() <= MAX_CACHE_BYTES,
                "after insert {i}: bytes={} > {}",
                cache.bytes(),
                MAX_CACHE_BYTES
            );
        }
    }

    // ── clear ────────────────────────────────────────────────────────

    #[test]
    fn clear_removes_all_entries_but_preserves_counters() {
        let mut cache = StyledLineCache::new();
        let k = key(1, 10, 0, 0);
        let s = src("hello");
        cache.insert(k, Arc::clone(&s), None, vec![1.0; 12]);
        let _ = cache.lookup(&k, &s, None); // hit
        let _ = cache.lookup(&key(2, 20, 0, 0), &s, None); // miss
        assert_eq!(cache.hits(), 1);
        assert_eq!(cache.misses(), 1);
        assert_eq!(cache.len(), 1);
        cache.clear();
        assert_eq!(cache.len(), 0);
        assert_eq!(cache.bytes(), 0);
        // Counters preserved across clear
        assert_eq!(cache.hits(), 1);
        assert_eq!(cache.misses(), 1);
    }

    // ── translate_and_append ─────────────────────────────────────────

    #[test]
    fn translate_only_modifies_xy_components() {
        let mut out = Vec::new();
        // Two quads (12 floats each): [x, y, u, v, fr, fg, fb, fa, br, bg, bb, ba]
        let verts = vec![
            0.0, 0.0, 0.5, 0.5, 1.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, //
            10.0, 20.0, 0.25, 0.75, 0.0, 1.0, 0.0, 0.5, 0.1, 0.2, 0.3, 0.4,
        ];
        translate_and_append_shifts_only_xy(&mut out, &verts, 100.0, 200.0);
        assert_eq!(out.len(), verts.len());
        // First vertex: (0,0) → (100,200), rest unchanged
        assert_eq!(out[0], 100.0);
        assert_eq!(out[1], 200.0);
        assert_eq!(out[2], 0.5); // u unchanged
        assert_eq!(out[7], 1.0); // fa unchanged
                                 // Second vertex: (10,20) → (110,220)
        assert_eq!(out[12], 110.0);
        assert_eq!(out[13], 220.0);
        assert_eq!(out[14], 0.25); // u unchanged
        assert_eq!(out[23], 0.4); // ba unchanged (index 23 = last channel)
    }

    #[test]
    fn translate_preserves_uv_and_color_exactly() {
        let mut out = Vec::new();
        let verts = vec![
            0.0, 0.0, 0.123, 0.456, 0.1, 0.2, 0.3, 0.4, 0.5, 0.6, 0.7, 0.8,
        ];
        translate_and_append_shifts_only_xy(&mut out, &verts, -50.0, -75.0);
        // Only x (idx 0) and y (idx 1) change; all others are byte-identical
        assert_eq!(out[0], -50.0);
        assert_eq!(out[1], -75.0);
        for i in 2..12 {
            assert_eq!(out[i], verts[i], "index {i} changed");
        }
    }

    #[test]
    fn translate_handles_empty_input() {
        let mut out = vec![1.0, 2.0];
        translate_and_append_shifts_only_xy(&mut out, &[], 10.0, 20.0);
        assert_eq!(out, vec![1.0, 2.0]); // unchanged
    }

    #[test]
    fn translate_handles_partial_trailing_vertex() {
        let mut out = Vec::new();
        // 12-float vertex + 3 trailing floats (incomplete)
        let verts = vec![
            0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, //
            1.0, 2.0, 3.0, // partial
        ];
        translate_and_append_shifts_only_xy(&mut out, &verts, 100.0, 200.0);
        assert_eq!(out.len(), verts.len());
        assert_eq!(out[0], 100.0); // translated
        assert_eq!(out[1], 200.0); // translated
        assert_eq!(out[12], 1.0); // partial: copied unchanged
        assert_eq!(out[13], 2.0);
        assert_eq!(out[14], 3.0);
    }

    // ── stats reset ──────────────────────────────────────────────────

    #[test]
    fn reset_stats_zeros_hit_miss_counters() {
        let mut cache = StyledLineCache::new();
        let k = key(1, 10, 0, 0);
        let s = src("hello");
        cache.insert(k, Arc::clone(&s), None, vec![1.0; 12]);
        let _ = cache.lookup(&k, &s, None); // hit
        let _ = cache.lookup(&key(2, 20, 0, 0), &s, None); // miss
        assert_eq!(cache.hits(), 1);
        assert_eq!(cache.misses(), 1);
        cache.reset_stats();
        assert_eq!(cache.hits(), 0);
        assert_eq!(cache.misses(), 0);
        // Entries are preserved across stats reset
        assert_eq!(cache.len(), 1);
    }

    // ── live block bypass (by design) ────────────────────────────────

    #[test]
    fn live_block_lines_are_never_cached_by_caller_convention() {
        // The cache itself doesn't know about "live" vs "completed" blocks —
        // the caller (build_block_view_vertices) is responsible for NOT
        // calling insert() for live block rows. This test documents that
        // convention: if the caller accidentally inserts a live block line,
        // the cache WILL cache it (no built-in protection). The caller must
        // guard with `block.is_completed()` or similar.
        //
        // This is by design: adding live/completed awareness to the cache
        // would require either a flag in the key (more state) or a separate
        // cache (more complexity). The simpler design is to trust the caller.
        let mut cache = StyledLineCache::new();
        let k = key(1, 999, 0, 0); // pretend block 999 is live
        let s = src("live output");
        cache.insert(k, Arc::clone(&s), None, vec![1.0; 12]);
        // The cache will happily return it — caller must not insert live rows
        assert!(cache.lookup(&k, &s, None).is_some());
    }

    // ── generation bump ─────────────────────────────────────────────

    #[test]
    fn bump_generation_invalidates_all_entries() {
        let mut cache = StyledLineCache::new();
        let gen0 = cache.generation();
        let k = key(1, 10, 0, 0);
        let s = src("hello");
        cache.insert(k, Arc::clone(&s), None, vec![1.0; 12]);
        assert!(cache.lookup(&k, &s, None).is_some());

        cache.bump_generation();
        assert_eq!(cache.generation(), gen0 + 1);
        assert_eq!(cache.len(), 0, "entries cleared on bump");
        assert_eq!(cache.bytes(), 0, "bytes cleared on bump");
        // The old key (with gen0) no longer matches — caller reconstructs
        // keys with the new generation, so old entries can't be hit.
        assert!(cache.lookup(&k, &s, None).is_none(), "old key misses");
    }

    #[test]
    fn bump_generation_preserves_hit_miss_counters() {
        let mut cache = StyledLineCache::new();
        let k = key(1, 10, 0, 0);
        let s = src("hello");
        cache.insert(k, Arc::clone(&s), None, vec![1.0; 12]);
        let _ = cache.lookup(&k, &s, None); // hit
        let _ = cache.lookup(&key(2, 20, 0, 0), &s, None); // miss
        assert_eq!(cache.hits(), 1);
        assert_eq!(cache.misses(), 1);

        cache.bump_generation();
        // Counters preserved (lifetime counters, not reset by generation bump)
        assert_eq!(cache.hits(), 1);
        assert_eq!(cache.misses(), 1);
    }

    #[test]
    fn take_hit_miss_counts_drains_and_resets() {
        let mut cache = StyledLineCache::new();
        let k = key(1, 10, 0, 0);
        let s = src("hello");
        cache.insert(k, Arc::clone(&s), None, vec![1.0; 12]);
        let _ = cache.lookup(&k, &s, None); // hit
        let _ = cache.lookup(&key(2, 20, 0, 0), &s, None); // miss
        let _ = cache.lookup(&key(3, 30, 0, 0), &s, None); // miss

        let (h, m) = cache.take_hit_miss_counts();
        assert_eq!(h, 1);
        assert_eq!(m, 2);
        // Counters reset after take
        assert_eq!(cache.hits(), 0);
        assert_eq!(cache.misses(), 0);
        // Entries preserved
        assert_eq!(cache.len(), 1);
    }

    // ── palette fingerprint ──────────────────────────────────────────

    #[test]
    fn palette_fingerprint_is_stable_for_same_palette() {
        let palette = [weft_core::grid::Color::rgb(0, 0, 0); 256];
        let fp1 = palette_fingerprint(&palette);
        let fp2 = palette_fingerprint(&palette);
        assert_eq!(fp1, fp2, "same palette → same fingerprint");
    }

    #[test]
    fn palette_fingerprint_changes_on_palette_mutation() {
        let mut palette = [weft_core::grid::Color::rgb(0, 0, 0); 256];
        let fp_before = palette_fingerprint(&palette);
        palette[42] = weft_core::grid::Color::rgb(255, 128, 0);
        let fp_after = palette_fingerprint(&palette);
        assert_ne!(fp_before, fp_after, "palette mutation → fingerprint change");
    }

    #[test]
    fn palette_fingerprint_distinguishes_all_zero_vs_all_max() {
        let zero = [weft_core::grid::Color::rgb(0, 0, 0); 256];
        let max = [weft_core::grid::Color::rgb(255, 255, 255); 256];
        assert_ne!(palette_fingerprint(&zero), palette_fingerprint(&max));
    }
}
