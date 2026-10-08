//! v1.13.5 T16b (PLAN_v11217 §3.11): per-background-pane block-view vertex
//! cache.
//!
//! `renderer/panes.rs::build_background_pane_content` used to run the full
//! `build_block_view_vertices` (fresh Vec + whole model construction) for
//! EVERY background block pane on EVERY frame — 4 streaming panes cost
//! 4×O(visible)/frame. The grid side has had per-pane incremental caches
//! since v1.12.2 (B3-2); this gives the block side a rebuild gate instead
//! (block vertices have no per-row dirty concept, so the gate is
//! fingerprint + throttle, not dirty rows):
//!
//! - **Composite fingerprint** (review P1 — a narrow gate goes stale): the
//!   nine-tuple below, conjunction semantics — ALL equal reuses. `now` is
//!   deliberately NOT a member (it feeds only the live header's elapsed
//!   label; background panes freeze that label between rebuilds, which the
//!   plan accepts).
//! - **Explicit invalidation hooks** (belt to the fingerprint's braces):
//!   `bump_background_grid_generation` (set_theme / set_minimum_contrast /
//!   set_bold_is_bright / update_scale — colors are baked into vertices)
//!   and `rebuild_atlas` (font/cell geometry) clear the whole map.
//! - **Throttle**: a fingerprint CHANGE rebuilds at most every 60ms
//!   (background ≈15fps); rect/scroll-only changes are immediate —
//!   interactive geometry must never lag a throttle window.
//! - **Cleanup**: draw() retains entries against the live background pane
//!   list (session ids are globally monotonic — the B3-2 precedent), which
//!   covers pane close AND promotion to active (an active pane's
//!   interactions — collapse toggles, editor typing — mutate state outside
//!   the fingerprint; re-entry rebuilds from scratch).
//!
//! `warm_background_pane_atlases` is NOT throttled by this batch (glyph
//! pre-heat semantics — declared out of scope in the plan).

use std::time::{Duration, Instant};

/// Rebuild cadence ceiling for a changed fingerprint (bg ≈15fps).
pub(crate) const BG_BLOCK_CACHE_THROTTLE: Duration = Duration::from_millis(60);

/// Composite gate key for one background pane's cached block vertices.
/// Conjunction semantics: reuse requires EVERY member equal. Derived from
/// the pane's `Terminal` (live version, head lines, cwd, branch, editor
/// mode), the renderer (theme epoch, cell dims) and the pane frame input
/// (rect, scroll).
#[derive(Clone, PartialEq, Debug)]
pub(crate) struct BgBlockFingerprint {
    /// `BlockTracker` live-view content version — bumps on capture prints
    /// AND on the structural sites (finalize / clear_pending_capture), so a
    /// finished block appearing at 133;D invalidates too.
    pub(crate) live_output_version: u64,
    /// Composed live-document head lines (preserved frames + scroll-out
    /// prefix) — shifts every live anchor.
    pub(crate) screen_head_lines: usize,
    pub(crate) cwd: Option<String>,
    pub(crate) git_branch: Option<String>,
    /// Renderer theme/contrast/bold-is-bright/scale epoch
    /// (`background_grid_generation`).
    pub(crate) theme_generation: u64,
    /// Baked cell geometry `(cell_width, cell_height)`.
    pub(crate) cell_dims: (u32, u32),
    /// Pane rect (absolute viewport coords) — immediate member.
    pub(crate) rect: crate::layout::Rect,
    /// Block-view scroll — immediate member.
    pub(crate) block_scroll: f32,
    /// Integrated-prompt on/off (changes the region layout + prompt rows).
    pub(crate) editor_mode: bool,
}

impl BgBlockFingerprint {
    /// Whether everything EXCEPT the immediate members (rect, block_scroll)
    /// is equal — i.e. the delta is pure interactive geometry, which
    /// rebuilds at once regardless of the throttle window.
    pub(crate) fn same_content(&self, other: &Self) -> bool {
        self.live_output_version == other.live_output_version
            && self.screen_head_lines == other.screen_head_lines
            && self.cwd == other.cwd
            && self.git_branch == other.git_branch
            && self.theme_generation == other.theme_generation
            && self.cell_dims == other.cell_dims
            && self.editor_mode == other.editor_mode
    }
}

/// One background pane's cached base content (block vertices + prompt
/// vertices, in legacy legacy-stream order).
pub(crate) struct BgBlockCache {
    pub(crate) vertices: Vec<f32>,
    pub(crate) fingerprint: BgBlockFingerprint,
    /// Anchor of the throttle window — refreshed on every RebuildNow, so a
    /// continuously-streaming pane rebuilds at the cadence ceiling instead
    /// of being starved by a sliding window.
    pub(crate) last_rebuild: Instant,
}

/// Gate decision for one background block-pane frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BgBlockCacheAction {
    /// Fingerprint unchanged — extend from the cached vertices.
    Reuse,
    /// Build fresh (no entry, throttle elapsed, or immediate-only delta).
    RebuildNow,
    /// Fingerprint changed but the throttle window is still open — serve
    /// the previous frame's vertices (bg 15fps cadence).
    Throttled,
}

/// Pure gate (truth-tabled below). `cached` is the entry's fingerprint +
/// throttle anchor; `current` the freshly captured fingerprint.
pub(crate) fn bg_block_cache_action(
    cached: Option<(&BgBlockFingerprint, Instant)>,
    current: &BgBlockFingerprint,
    now: Instant,
    throttle: Duration,
) -> BgBlockCacheAction {
    let Some((cached_fp, last_rebuild)) = cached else {
        return BgBlockCacheAction::RebuildNow;
    };
    if *cached_fp == *current {
        return BgBlockCacheAction::Reuse;
    }
    // Immediate members only (rect/scroll): rebuild now — user-visible
    // geometry must never lag the throttle window.
    if cached_fp.same_content(current) {
        return BgBlockCacheAction::RebuildNow;
    }
    if now.duration_since(last_rebuild) < throttle {
        return BgBlockCacheAction::Throttled;
    }
    BgBlockCacheAction::RebuildNow
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_fp() -> BgBlockFingerprint {
        BgBlockFingerprint {
            live_output_version: 7,
            screen_head_lines: 0,
            cwd: Some("/tmp".to_string()),
            git_branch: None,
            theme_generation: 1,
            cell_dims: (8, 16),
            rect: [0.0, 0.0, 400.0, 300.0],
            block_scroll: 0.0,
            editor_mode: false,
        }
    }

    /// One shared clock: instants anchor relative to a single `now`, so two
    /// calls can't drift (two `Instant::now()` calls differ by µs and would
    /// flake the throttle comparisons).
    fn clock() -> Instant {
        std::time::Instant::now()
    }

    /// Truth table: no entry | equal fp | rect/scroll-only delta | content
    /// delta inside/outside the throttle window.
    #[test]
    fn bg_block_cache_action_truth_table() {
        let fp = base_fp();
        let throttle = Duration::from_millis(60);

        // No entry → build.
        assert_eq!(
            bg_block_cache_action(None, &fp, clock(), throttle),
            BgBlockCacheAction::RebuildNow
        );

        // Equal fingerprint → reuse, throttle irrelevant.
        let now = clock();
        assert_eq!(
            bg_block_cache_action(Some((&fp, now)), &fp, now, throttle),
            BgBlockCacheAction::Reuse
        );

        // rect-only delta → immediate even inside the throttle window.
        let now = clock();
        let mut moved = base_fp();
        moved.rect = [10.0, 0.0, 410.0, 300.0];
        assert_eq!(
            bg_block_cache_action(Some((&fp, now)), &moved, now, throttle),
            BgBlockCacheAction::RebuildNow
        );

        // scroll-only delta → immediate as well.
        let mut scrolled = base_fp();
        scrolled.block_scroll = 42.0;
        assert_eq!(
            bg_block_cache_action(Some((&fp, now)), &scrolled, now, throttle),
            BgBlockCacheAction::RebuildNow
        );

        // Content delta (streaming bump) inside the window → throttled.
        let mut streamed = base_fp();
        streamed.live_output_version += 1;
        assert_eq!(
            bg_block_cache_action(Some((&fp, now)), &streamed, now, throttle),
            BgBlockCacheAction::Throttled
        );

        // Same delta after the window → rebuild.
        let stale_now = clock();
        let stale_anchor = stale_now - Duration::from_secs(10);
        assert_eq!(
            bg_block_cache_action(Some((&fp, stale_anchor)), &streamed, stale_now, throttle),
            BgBlockCacheAction::RebuildNow
        );

        // Content delta combined with a rect delta is still throttled (the
        // immediate fast path requires the delta to be rect/scroll ONLY).
        let mut both = streamed.clone();
        both.rect = [1.0, 1.0, 401.0, 301.0];
        assert_eq!(
            bg_block_cache_action(Some((&fp, now)), &both, now, throttle),
            BgBlockCacheAction::Throttled
        );
    }

    /// cwd / git_branch / theme / cell dims / editor mode / head lines each
    /// break equality (review P1: no narrow gate).
    #[test]
    fn fingerprint_members_each_break_equality() {
        let fp = base_fp();
        let mut v = base_fp();
        v.live_output_version += 1;
        let mut h = base_fp();
        h.screen_head_lines += 1;
        let mut c = base_fp();
        c.cwd = None;
        let mut g = base_fp();
        g.git_branch = Some("main".to_string());
        let mut th = base_fp();
        th.theme_generation += 1;
        let mut d = base_fp();
        d.cell_dims = (9, 16);
        let mut e = base_fp();
        e.editor_mode = true;

        for (name, mutated) in [
            ("version", &v),
            ("head_lines", &h),
            ("cwd", &c),
            ("git_branch", &g),
            ("theme", &th),
            ("cell_dims", &d),
            ("editor_mode", &e),
        ] {
            assert_ne!(&fp, mutated, "{name} must break fingerprint equality");
            assert!(
                !fp.same_content(mutated),
                "{name} is a content member, not an immediate one"
            );
        }
    }
}
