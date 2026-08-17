//! Live (in-flight) block layout cache — v1.10.23 Phase 3
//! (FIX_LIVE_BLOCK_SCROLL_PERF).
//!
//! The live block's output streams (omp snapshots at ~50ms, regular commands
//! per print), so wrapping it every scroll tick was O(document): 3×
//! `lines().collect()` + a 2000-line uncapped materialization + per-frame
//! String clones. This cache keys on the block tracker's content `version`
//! (bumped on EVERY mutation — version equality ⇔ byte-identical output)
//! plus `cols` AND the pane's `pane_session_id` (one global cache is shared
//! by every tab/pane, and per-Tab version counters start at 0), and stores:
//!
//! - `cumulative`: prefix sums of per-line display rows (wrap counts), for
//!   O(log n) binary-search location of the visible logical-line window,
//!   mirroring the finished-blocks prefix-sum culling in `layout_pass.rs`.
//! - `line_ranges`: byte ranges of each tail-window line into the live
//!   output (exact `str::lines()` semantics), so the layout pass slices only
//!   the visible lines — no `lines().collect()` on the scroll tick.
//!
//! Rebuild is O(document) but happens once per content version (snapshot
//! rate-limited to ~50ms), never per scroll tick.

use crate::paint::grid_cache::{
    block_line_chunks, screen_origin_line_chunks, MAX_LAYOUT_LINES_LIVE,
};

/// Fingerprint of every input `block_scroll_metrics` reads. A matching key
/// guarantees an identical `(total, visible, max_scroll)`, so the wheel
/// handler and the same-frame redraw scrollbar can share one computation.
/// `cache_rebuilds` is the monotonic `BlockLayoutCache` miss count — the
/// finished-blocks metrics only change when a cache entry is rebuilt (block
/// set + first/last ids + len cover membership).
/// Memoized `block_scroll_metrics` result: `(key, (total, visible, max))`.
pub(crate) type BlockScrollMetricsMemo = Option<(BlockScrollMetricsKey, (usize, usize, usize))>;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct BlockScrollMetricsKey {
    /// Pane session scope. Without it, the empty-block-set key
    /// `(len, first, last) = (0, None, None)` of two fresh tabs is identical
    /// even though their scroll extents diverge as each fills up.
    pub(crate) pane_session_id: u64,
    pub(crate) cols: u32,
    pub(crate) num_rows: u32,
    /// `Some(version)` while a live block exists; `None` at prompt.
    pub(crate) live_version: Option<u64>,
    pub(crate) blocks_len: u32,
    pub(crate) first_block_id: Option<u64>,
    pub(crate) last_block_id: Option<u64>,
    pub(crate) cache_rebuilds: u64,
    pub(crate) header_rows: u32,
    pub(crate) visible: u32,
    /// Whether the live block shows a cwd line — `block_content_metrics_with_cache`
    /// adds `usize::from(live.cwd.or(terminal.cwd()).is_some())` to the total, so
    /// an OSC 7 cwd change mid-command must invalidate the memo.
    pub(crate) cwd_present: bool,
}

/// Cumulative layout of the live block's output tail window
/// (newest `MAX_LAYOUT_LINES_LIVE` lines, oldest-first indexing).
#[derive(Default)]
pub(crate) struct LiveLayoutCache {
    /// Pane session scoping the key — the cache is shared across tabs/panes.
    pane_session_id: u64,
    /// Content version from `BlockTracker::in_flight().version`.
    version: u64,
    /// Wrapping width key — resize invalidates.
    cols: usize,
    /// v1.10.26 Batch B review blocker (BL-1): whether the live document is
    /// screen-origin (`screen_document_start` set). Screen-owned frames clip
    /// per line (one row); ordinary shell output soft-wraps (multiple rows per
    /// long line). Joined to the key — the flag flips exactly at a version
    /// bump (`on_command_start` clears / `begin_screen_owned_output` sets), but
    /// keeping it in the equality test makes the split explicit and future-proof.
    screen_origin: bool,
    /// Number of logical lines in the tail window.
    total_lines: usize,
    /// Raw index of the window's first line within the full document
    /// (= `max(0, doc_lines - MAX_LAYOUT_LINES_LIVE)`).
    base_idx: usize,
    /// `cumulative[i]` = display rows of window lines `[0..i)`; len = total+1.
    cumulative: Vec<u32>,
    /// Byte range `(start, end)` into the live output for each window line,
    /// mirroring `str::lines()` exactly (trailing `\r` stripped).
    line_ranges: Vec<(usize, usize)>,
    /// Rebuilds since creation (test observability for the hit/miss keys).
    #[cfg(test)]
    rebuilds: usize,
}

impl LiveLayoutCache {
    /// Rebuild the cumulative tables unless `(pane_session_id, version, cols,
    /// screen_origin)` already match. Callers hold either the layout pass or the
    /// metrics path; both are idempotent so the paint + hit-testing passes share
    /// one build. `pane_session_id` disambiguates panes/tabs that coincidentally
    /// share version+cols (per-Tab version counters start at 0) — without it,
    /// one pane's byte ranges could be sliced against another's output
    /// (silent misrender or a mid-multibyte-char slice panic). `screen_origin`
    /// flips exactly at a version bump, but joining it keeps the wrap-vs-clip
    /// split explicit in the key.
    pub(crate) fn sync(
        &mut self,
        output: &str,
        pane_session_id: u64,
        version: u64,
        cols: usize,
        screen_origin: bool,
    ) {
        if self.pane_session_id == pane_session_id
            && self.version == version
            && self.cols == cols
            && self.screen_origin == screen_origin
        {
            return;
        }
        #[cfg(test)]
        {
            self.rebuilds += 1;
        }
        self.pane_session_id = pane_session_id;
        self.version = version;
        self.cols = cols;
        self.screen_origin = screen_origin;
        self.line_ranges.clear();
        self.cumulative.clear();
        for line in output.lines() {
            let start = line.as_ptr() as usize - output.as_ptr() as usize;
            self.line_ranges.push((start, start + line.len()));
        }
        let skip = self.line_ranges.len().saturating_sub(MAX_LAYOUT_LINES_LIVE);
        self.base_idx = skip;
        self.total_lines = self.line_ranges.len() - skip;
        self.cumulative.reserve(self.total_lines + 1);
        self.cumulative.push(0);
        let mut acc = 0u32;
        for &(start, end) in &self.line_ranges[skip..] {
            // v1.10.26 Batch B review blocker (BL-1): the live layout splits by
            // screen_origin. Screen-owned TUI frames are hard terminal rows —
            // one clipped row per line no matter the width. Ordinary shell
            // output soft-wraps: each line counts as many rows as its wrapped
            // chunks. Must stay in lockstep with layout_pass's per-line chunk
            // function (driven by the same `live.screen_origin`), or the
            // visible-window prefix sums drift from the laid-out rows.
            let rows = if self.screen_origin {
                screen_origin_line_chunks(&output[start..end], cols).count() as u32
            } else {
                block_line_chunks(&output[start..end], cols).count() as u32
            };
            acc += rows;
            self.cumulative.push(acc);
        }
    }

    /// Total display rows of the tail window (= `cumulative.last()`).
    /// Identical to the pre-cache formula
    /// `lines()[skip..].map(|l| <chunk_count>).sum()` — `screen_origin` lines
    /// are one row each (clip); ordinary lines count their soft-wrap chunks.
    pub(crate) fn total_display_rows(&self) -> usize {
        self.cumulative.last().copied().unwrap_or(0) as usize
    }

    pub(crate) fn cumulative(&self) -> &[u32] {
        &self.cumulative
    }

    pub(crate) fn total_lines(&self) -> usize {
        self.total_lines
    }

    pub(crate) fn base_idx(&self) -> usize {
        self.base_idx
    }

    /// Byte range of window line `i` (oldest-first) into the live output.
    pub(crate) fn line_range(&self, i: usize) -> (usize, usize) {
        self.line_ranges[self.base_idx + i]
    }

    /// Rebuild count since creation — a cache HIT is a `sync` that returns
    /// without incrementing this. Test-only (asserts hit/miss keys).
    #[cfg(test)]
    pub(crate) fn rebuilds(&self) -> usize {
        self.rebuilds
    }

    /// Visible logical-line window `[start, end)` (oldest-first index space)
    /// via binary search on `cumulative`, +overscan per side — the live
    /// analogue of the finished-blocks prefix-sum culling.
    ///
    /// Geometry (distance-from-content-bottom space, matching the layout
    /// pass): the live block sits at the bottom; line `i` (oldest-first)
    /// spans `[total - cumulative[i+1], total - cumulative[i]) * pitch`,
    /// so the newest line is lowest and `threshold_low`/`threshold_high`
    /// are the viewport's bottom/top edges.
    pub(crate) fn visible_window(
        &self,
        pitch: f32,
        threshold_low: f32,
        threshold_high: f32,
        overscan_px: f32,
    ) -> (usize, usize) {
        let overscan_lines = (overscan_px / pitch).ceil() as usize + 1;
        let n = self.total_lines;
        if n == 0 || self.cumulative.len() != n + 1 {
            return (0, 0);
        }
        let total = self.cumulative[n] as f32;
        // Mirror the viewport band into cumulative-row space (cumulative is
        // an oldest-first prefix, so the band's top edge maps to a lower
        // bound and its bottom edge to an upper bound).
        let top_rows = total - threshold_high / pitch; // line tops must be >= this
        let bottom_rows = total - threshold_low / pitch; // line bottoms must be <= this
                                                         // bottommost visible line: largest i with cumulative[i+1] <= bottom_rows
        let newest_visible = last_le(&self.cumulative, bottom_rows).saturating_sub(1);
        // topmost visible line: smallest i with cumulative[i] >= top_rows
        let oldest_visible = first_ge(&self.cumulative, top_rows);
        (
            oldest_visible.saturating_sub(overscan_lines),
            (newest_visible + 1 + overscan_lines).min(n),
        )
    }

    /// `visible_window` + the distance-from-bottom of the first emitted
    /// (newest visible) line's top — the layout pass's cursor fast-forward
    /// for culled lines, kept next to the window math.
    pub(crate) fn visible_range(
        &self,
        pitch: f32,
        threshold_low: f32,
        threshold_high: f32,
        overscan_px: f32,
    ) -> (usize, usize, f32) {
        let (start, end) = self.visible_window(pitch, threshold_low, threshold_high, overscan_px);
        let dist_at_end = self.total_display_rows() as f32 * pitch
            - self.cumulative().get(end).copied().unwrap_or(0) as f32 * pitch;
        (start, end, dist_at_end)
    }
}

/// Largest j in `[0..=n]` with `cumulative[j] as f32 <= rows`; `0` when none.
fn last_le(cumulative: &[u32], rows: f32) -> usize {
    let mut lo = 0usize;
    let mut hi = cumulative.len();
    while lo < hi {
        let mid = (lo + hi) / 2;
        if cumulative[mid] as f32 <= rows {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    lo.saturating_sub(1)
}

/// Smallest j in `[0..=n]` with `cumulative[j] as f32 >= rows`; `n` when none.
fn first_ge(cumulative: &[u32], rows: f32) -> usize {
    let mut lo = 0usize;
    let mut hi = cumulative.len();
    while lo < hi {
        let mid = (lo + hi) / 2;
        if (cumulative[mid] as f32) < rows {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    lo.min(cumulative.len() - 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fixed pane_session_id for the shared-key tests; the session-scoping
    /// tests below pass ids explicitly. Defaults to the screen-origin (TUI)
    /// slice so the layout-math tests keep one-clipped-row-per-line geometry;
    /// the ordinary soft-wrap split is exercised by the dedicated tests under
    /// `sync(..., screen_origin: false)`.
    fn sync(cache: &mut LiveLayoutCache, output: &str, version: u64, cols: usize) {
        cache.sync(output, 0, version, cols, true);
    }

    /// Rows-per-line by the live layout split (v1.10.26 Batch B review
    /// blocker BL-1): screen-origin lines clip to exactly one row regardless
    /// of width; ordinary shell-output lines count their soft-wrap chunks.
    fn wrap_counts(output: &str, cols: usize, screen_origin: bool) -> Vec<usize> {
        output
            .lines()
            .map(|line| {
                if screen_origin {
                    screen_origin_line_chunks(line, cols).count()
                } else {
                    block_line_chunks(line, cols).count()
                }
            })
            .collect()
    }

    #[test]
    fn cumulative_screen_origin_counts_one_per_line() {
        let output = "0123456789abcdefghij\none\ntwo lines\n";
        let mut cache = LiveLayoutCache::default();
        sync(&mut cache, output, 7, 8);
        // B-1: a 20-char line at cols 8 (screen-origin) is CLIPPED to one
        // display row (no soft-wrap) — screen-origin counts are 1 per line.
        let counts = wrap_counts(output, 8, true);
        assert_eq!(counts, vec![1, 1, 1]);
        let mut acc = 0;
        for (i, c) in counts.iter().enumerate() {
            acc += c;
            assert_eq!(cache.cumulative()[i + 1] as usize, acc, "prefix at {i}");
        }
        assert_eq!(cache.total_display_rows(), 3);
        assert_eq!(cache.total_lines(), 3);
        assert_eq!(cache.base_idx(), 0);
        // line_ranges must slice back the exact lines() texts (incl. \r strip).
        assert_eq!(
            &output[cache.line_range(0).0..cache.line_range(0).1],
            "0123456789abcdefghij"
        );
        assert_eq!(
            &output[cache.line_range(2).0..cache.line_range(2).1],
            "two lines"
        );
    }

    #[test]
    fn cr_stripped_ranges_match_lines() {
        let output = "a\r\nb\r\nc";
        let mut cache = LiveLayoutCache::default();
        sync(&mut cache, output, 1, 80);
        let lines: Vec<&str> = output.lines().collect();
        for (i, line) in lines.iter().enumerate() {
            let (s, e) = cache.line_range(i);
            assert_eq!(&output[s..e], *line, "range {i}");
        }
    }

    #[test]
    fn cache_hit_skips_rebuild_version_and_cols_keys() {
        // B-1: the live layout is screen-origin (clip-not-wrap), so wrapping
        // is cols-INDEPENDENT — the cache still re-keys on cols/version, but
        // a rebuild produces identical cumulative.
        let output = format!("{}\nbbb\n", "x".repeat(60));
        let mut cache = LiveLayoutCache::default();
        sync(&mut cache, &output, 3, 80);
        assert_eq!(cache.rebuilds(), 1);
        let cumulative = cache.cumulative().to_vec();

        // Same version + cols → HIT: no rebuild, cumulative identical.
        sync(&mut cache, &output, 3, 80);
        assert_eq!(cache.rebuilds(), 1, "same version+cols must be a hit");
        assert_eq!(cache.cumulative(), cumulative.as_slice());

        // cols change → MISS (re-key) but cumulative IDENTICAL: the
        // screen-origin clip layout yields one row per line regardless of
        // width.
        sync(&mut cache, &output, 3, 40);
        assert_eq!(cache.rebuilds(), 2, "cols change must be a miss");
        assert_eq!(
            cache.cumulative().last().copied().unwrap_or(0),
            2,
            "60-char line clips to 1 row + 'bbb' = 2 rows total"
        );

        // version change → MISS even with same cols (and identical bytes:
        // the reset must re-key, not reuse).
        sync(&mut cache, &output, 4, 40);
        let cumulative2 = cache.cumulative().to_vec();
        assert_eq!(cache.rebuilds(), 3, "version change must be a miss");
        sync(&mut cache, &output, 5, 40);
        assert_eq!(cache.rebuilds(), 4, "version change must be a miss");
        // Same bytes + same cols → identical layout after rebuild (no drift).
        assert_eq!(cache.cumulative(), cumulative2.as_slice());
    }

    /// B1 (review blocker): the cache is shared across panes but version
    /// counters are per-Tab (start at 0) — two panes coincidentally at the
    /// same version+cols must NOT hit each other's entries (slicing one
    /// pane's bytes with the other's ranges).
    #[test]
    fn different_pane_session_same_version_cols_misses() {
        let output = "aaa\nbbb\n";
        let mut cache = LiveLayoutCache::default();
        cache.sync(output, 10, 7, 8, true); // pane A, version 7, screen-origin
        assert_eq!(cache.rebuilds(), 1);
        let pane_a_cumulative = cache.cumulative().to_vec();
        // Pane B: same version+cols, different session → MISS.
        cache.sync(output, 11, 7, 8, true);
        assert_eq!(
            cache.rebuilds(),
            2,
            "same version+cols across sessions must miss"
        );
        // Alternating back to pane A rebuilds again — no cross-session reuse.
        cache.sync(output, 10, 7, 8, true);
        assert_eq!(cache.rebuilds(), 3, "session switch must not reuse");
        assert_eq!(cache.cumulative(), pane_a_cumulative.as_slice());
        cache.sync(output, 11, 7, 8, true);
        assert_eq!(cache.rebuilds(), 4);
    }

    #[test]
    fn same_pane_session_same_version_cols_hits() {
        let output = "aaa\nbbb\n";
        let mut cache = LiveLayoutCache::default();
        cache.sync(output, 10, 7, 8, true);
        assert_eq!(cache.rebuilds(), 1);
        cache.sync(output, 10, 7, 8, true);
        assert_eq!(cache.rebuilds(), 1, "same session+version+cols is a hit");
    }

    // ── BL-1 (Batch B review blocker): split the live layout by screen_origin
    // ────────────────────────────────────────────────────────────────────────

    /// A plain shell command (no `screen_document_start`) emits long streaming
    /// lines. While the command is in flight they must SOFT-WRAP like finished
    /// shell-output blocks — the initial Batch B "live is always screen-origin"
    /// clip was a functional regression (a `make` diagnostic line was chopped
    /// at the window edge mid-stream).
    #[test]
    fn ordinary_live_long_line_soft_wraps_with_complete_content() {
        let output = format!("{}\nshort line\n", "x".repeat(40));
        let mut cache = LiveLayoutCache::default();
        // screen_origin = false: ordinary streaming output.
        cache.sync(&output, 0, 3, 8, false);
        // 40-char line at cols 8 → 5 wrapped rows; "short line" (10 cols) → 2.
        let counts = wrap_counts(&output, 8, false);
        assert_eq!(counts, vec![5, 2], "ordinary long line soft-wraps");
        assert_eq!(cache.total_display_rows(), 7);
        // Content is preserved across the wrapped rows (nothing clipped).
        let mut reconstructed = String::new();
        for i in 0..cache.total_lines() {
            let (s, e) = cache.line_range(i);
            reconstructed.push_str(&output[s..e]);
            reconstructed.push('\n');
        }
        assert_eq!(reconstructed, output, "soft-wrap must not lose text");
    }

    /// The screen-owned TUI case: a long frame row is a hard terminal row that
    /// CLIPS to one display row — the `|]` border must never fold.
    #[test]
    fn screen_owned_live_long_line_clips_to_one_row() {
        let line = format!("[|{}|]", "x".repeat(40));
        let output = format!("{line}\n");
        let mut cache = LiveLayoutCache::default();
        cache.sync(&output, 0, 3, 8, true);
        assert_eq!(
            cache.total_display_rows(),
            1,
            "screen-origin long line is clipped, not wrapped ({line})"
        );
        assert_eq!(cache.total_lines(), 1);
        let counts = wrap_counts(&output, 8, true);
        assert_eq!(counts, vec![1]);
    }

    /// The two live modes flip mid-command (`begin_screen_owned_output` after
    /// plain streaming): the version bump must invalidate the cache and the
    /// rebuilt layout must switch to clip — same bytes, different row counts.
    #[test]
    fn screen_origin_switch_invalidates_cache_and_clips() {
        let output = format!("{}\n", "y".repeat(40));
        let mut cache = LiveLayoutCache::default();
        // Ordinary phase: long line soft-wraps → 40-col line at cols 10 = 4 rows.
        cache.sync(&output, 0, 7, 10, false);
        assert_eq!(cache.rebuilds(), 1);
        assert_eq!(cache.total_display_rows(), 4, "ordinary: wrapped rows");
        // Same bytes+cols, but the tracker bump from screen handoff changed
        // state → the flag join forces a MISS, and the rebuilt layout clips.
        cache.sync(&output, 0, 8, 10, true);
        assert_eq!(cache.rebuilds(), 2, "screen_origin flip must invalidate");
        assert_eq!(
            cache.total_display_rows(),
            1,
            "same bytes now clip to one row — the switch took effect"
        );
        // Flipping back (new command, plain output) rebuilds again.
        cache.sync(&output, 0, 9, 10, false);
        assert_eq!(cache.rebuilds(), 3);
        assert_eq!(cache.total_display_rows(), 4);
    }

    #[test]
    fn tail_window_caps_at_max_layout_lines() {
        let output = (0..3000).map(|i| format!("line {i}\n")).collect::<String>();
        let mut cache = LiveLayoutCache::default();
        sync(&mut cache, &output, 1, 80);
        assert_eq!(cache.total_lines(), MAX_LAYOUT_LINES_LIVE);
        assert_eq!(cache.base_idx(), 1000);
        // The window starts at raw line 1000 ("line 999\n" is the last
        // excluded line); ranges slice the exact line bytes (no newline).
        let (s, e) = cache.line_range(0);
        assert_eq!(&output[s..e], "line 1000");
        let (_, e) = cache.line_range(MAX_LAYOUT_LINES_LIVE - 1);
        assert_eq!(&output[e - "line 2999".len()..e], "line 2999");
    }

    // ── visible_window: cumulative 二分窗口边界 ─────────────────────────

    /// Viewport sits at the bottom (following live): the window hugs the
    /// NEWEST lines — line 19 (newest) stays inside, the older lines are
    /// culled. 20 lines × 1 display row @ pitch 20 → total 400px; viewport
    /// 6 rows (120px), overscan 3 lines.
    #[test]
    fn visible_window_following_bottom_hugs_newest() {
        let output = (0..20).map(|i| format!("l{i}\n")).collect::<String>();
        let mut cache = LiveLayoutCache::default();
        sync(&mut cache, &output, 1, 80);
        let pitch = 20.0;
        // scroll_px = 0 → bottom edge at -overscan(64px), top edge at
        // 160px; overscan 64px → 5 lines.
        let (start, end) = cache.visible_window(pitch, -40.0, 160.0, 64.0);
        assert_eq!((start, end), (7, 20));
        assert!(start <= 19 && end > 19, "newest line must stay in window");
        // Only 13 of 20 lines materialized.
        assert!(end - start < 20);
        // The visible band (120px = 6 rows) is fully covered: lines 12..19
        // are inside [start, end).
        assert!(start <= 12 && end > 19);
    }

    #[test]
    fn visible_window_tail_less_than_one_screen() {
        // 3 lines, viewport 6 rows → whole document visible, full window.
        let output = "a\nb\nc\n";
        let mut cache = LiveLayoutCache::default();
        sync(&mut cache, output, 1, 80);
        let (start, end) = cache.visible_window(20.0, -40.0, 160.0, 64.0);
        assert_eq!((start, end), (0, 3));
    }

    #[test]
    fn visible_window_scrolled_to_oldest_edge() {
        // 100 lines → total 2000px. Scroll far past the block: the window
        // falls back to the top (oldest) lines nearest the band.
        let output = (0..100).map(|i| format!("l{i}\n")).collect::<String>();
        let mut cache = LiveLayoutCache::default();
        sync(&mut cache, &output, 1, 80);
        let (start, end) = cache.visible_window(20.0, 10_000.0, 10_400.0, 64.0);
        assert_eq!((start, end), (0, 6));

        // Band [1900, 1950]px cuts lines 2..4 (tops 1960/1940/1920,
        // bottoms 1940/1920/1900); window = [l-5, f+1+5).
        let (start2, end2) = cache.visible_window(20.0, 1900.0, 1950.0, 64.0);
        assert_eq!((start2, end2), (0, 10));
        assert!(start2 <= 2 && end2 > 4, "band lines 2..4 must be inside");
        // Window covers the band in cumulative-row space: start row <= top
        // bound (2.5), end row >= bottom bound (5).
        let cum = cache.cumulative();
        assert!(cum[start2] as f32 <= 2.5);
        assert!(cum[end2] as f32 >= 5.0);
    }

    #[test]
    fn visible_window_empty_output() {
        let mut cache = LiveLayoutCache::default();
        sync(&mut cache, "", 1, 80);
        assert_eq!(cache.visible_window(20.0, 0.0, 100.0, 64.0), (0, 0));
        assert_eq!(cache.total_display_rows(), 0);
    }

    /// The `BlockTracker` live-output version — the cache key — bumps on
    /// every mutation (print/newline/ascii/snapshot-replace/clear) and stays
    /// put while the output is unchanged (scroll ticks don't invalidate).
    #[test]
    fn tracker_live_output_version_tracks_mutations() {
        use weft_core::blocks::{BlockTracker, CapturedStyle, StyledOutput};
        let mut t = BlockTracker::new();
        t.on_prompt_start();
        let v_before = t.in_flight().map(|l| l.version);
        // Not capturing: prints must NOT bump (output unchanged).
        t.on_print('x', CapturedStyle::default());
        assert_eq!(t.in_flight().map(|l| l.version), v_before);

        t.on_command_start("echo".to_string());
        let v0 = t.in_flight().expect("command executing").version;
        t.on_print('a', CapturedStyle::default());
        assert_eq!(t.in_flight().unwrap().version, v0 + 1, "print bumps");
        t.on_newline();
        t.on_print_ascii_run(b"bcd", CapturedStyle::default());
        assert_eq!(t.in_flight().unwrap().version, v0 + 3, "newline+ascii bump");

        // Screen-owned path: handoff clears output, snapshot replace bumps.
        t.on_command_end(0);
        t.on_prompt_start();
        t.on_command_start("tui".to_string());
        let v_owned = t.in_flight().unwrap().version;
        t.begin_screen_owned_output(0);
        assert_eq!(
            t.in_flight().unwrap().version,
            v_owned + 1,
            "screen handoff clears output"
        );
        let styled = StyledOutput { lines: Vec::new() };
        t.replace_screen_snapshot("frame one\n", styled.clone());
        let v1 = t.in_flight().unwrap().version;
        t.replace_screen_snapshot("frame two\n", styled);
        assert_eq!(
            t.in_flight().unwrap().version,
            v1 + 1,
            "snapshot replace bumps"
        );
    }

    /// The window never skips a line the old full materialization showed:
    /// every line whose band intersects the viewport must be inside the
    /// emitted window, and the emitted count stays bounded.
    #[test]
    fn visible_window_covers_band_exactly() {
        let output = (0..50).map(|i| format!("l{i}\n")).collect::<String>();
        let mut cache = LiveLayoutCache::default();
        sync(&mut cache, &output, 1, 80);
        let pitch = 20.0;
        let total = 50.0_f32 * pitch; // 1000px
        for band_top in [0.0_f32, 200.0, 500.0, 900.0, 980.0] {
            let band_bottom = band_top + 200.0;
            let (start, end) = cache.visible_window(pitch, band_top, band_bottom, 40.0);
            assert!(start < end, "band {band_top}");
            // Every visible line (top >= band_top, bottom <= band_bottom)
            // is inside the window.
            for i in 0..50usize {
                let top = total - cache.cumulative()[i] as f32 * pitch;
                let bottom = total - cache.cumulative()[i + 1] as f32 * pitch;
                if top >= band_top && bottom <= band_bottom {
                    assert!(
                        start <= i && i < end,
                        "band {band_top}: visible line {i} culled"
                    );
                }
            }
            // Materialization bounded: band rows + 2×overscan + slack.
            let band_rows = (band_bottom - band_top) / pitch;
            assert!(
                end - start <= band_rows as usize + 2 * 2 + 4,
                "band {band_top}"
            );
        }
    }
}
