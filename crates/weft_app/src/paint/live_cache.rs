//! Live (in-flight) block layout cache — v1.10.23 Phase 3
//! (FIX_LIVE_BLOCK_SCROLL_PERF); M6-a incremental sync (PLAN_M6 §A-2).
//!
//! The live block's output streams (omp snapshots at ~50ms, regular commands
//! per print), so wrapping it every scroll tick was O(document): 3×
//! `lines().collect()` + a 2000-line uncapped materialization + per-frame
//! String clones. This cache keys on the block tracker's content `version`
//! (bumped on EVERY mutation — version equality ⇔ byte-identical output)
//! plus `cols` AND the pane's `pane_session_id` (one global cache is shared
//! by every tab/pane, and per-Tab version counters start at 0).
//!
//! M6-a: a version bump no longer means a full O(document) rescan. The cache
//! keeps `synced_byte_end` (the offset after the last consumed complete
//! line) plus a bounded suffix window; when the capture's rewrite watermark
//! proves nothing below that boundary was touched since the last sync, only
//! the newly appended tail is folded in — O(new bytes + one partial line).
//! Streaming output is not always pure append (`on_move_cursor_rows` mirrors
//! CSI A/B/E/F so progress bars repaint early rows), hence the watermark
//! (`min_write_offset`) recorded per capture op is the authoritative guard;
//! the byte-level "\n at boundary" canary is depth-on-defense only.
//!
//! Stored state per version:
//!
//! - `window`: the newest `min(doc_lines, MAX_LAYOUT_LINES_LIVE)` lines
//!   (continuously capped — every append pops the head as needed), each with
//!   its byte range into the live output.
//! - `abs_cum` / `abs_cums`: display-row prefix sums in REBUILD-EPOCH
//!   absolute space. "Absolute" means: not renumbered when the window's head
//!   is popped, so popping is O(1) amortized. The origin is re-seeded at each
//!   full rebuild; only differences within an epoch are ever consumed.
//! - `cumulative`: the window-RELATIVE prefix the `cumulative()` accessor
//!   contracts to, materialized at sync time (O(MAX) ≈ 8KB, bounded).

use std::collections::VecDeque;

use weft_core::grid::terminal_text_width;

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

/// One line of the bounded suffix window.
#[derive(Clone, Copy)]
struct WindowLine {
    /// Byte range `(start, end)` into the live output, mirroring
    /// `str::lines()` exactly (trailing `\r` stripped).
    byte_range: (usize, usize),
    /// Display rows of all window lines BEFORE this one, in rebuild-epoch
    /// absolute space (saturating — 1MiB-capped documents cannot overflow
    /// u32 rows, the guard is belt-and-braces).
    abs_cum: u32,
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
    /// M6-a: offset after the last consumed COMPLETE line (the append fast
    /// path's invariant: `window`'s last entry starts here exactly when the
    /// document's final line is still growing). Reset on every full rebuild.
    synced_byte_end: usize,
    /// Bounded suffix window, oldest-first; length is always
    /// `min(doc_lines, MAX_LAYOUT_LINES_LIVE)` (continuous cap — no slack
    /// band, so the accessors stay value-identical to a full rebuild).
    window: VecDeque<WindowLine>,
    /// Epoch-absolute row prefix per window line + the end value:
    /// `abs_cums[i] = window[i].abs_cum` for `i < len`, `abs_cums[len]` =
    /// prefix AFTER the window's last line. Kept contiguous for the
    /// `visible_window` binary search.
    abs_cums: Vec<u32>,
    /// Absolute line index of the window's first line (= `base_idx`).
    abs_first_line: usize,
    /// Window-relative cumulative: `cumulative[i]` = display rows of window
    /// lines `[0..i)`; len = window + 1. Materialized per mutating sync for
    /// the `cumulative() -> &[u32]` contract.
    cumulative: Vec<u32>,
    /// Full rebuilds since creation (test observability — a HIT or an
    /// incremental append does not count).
    #[cfg(test)]
    rebuilds: usize,
    /// Incremental appends since creation (test observability).
    #[cfg(test)]
    appends: usize,
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
    ///
    /// M6-a: `min_write_offset` is the capture's rewrite watermark taken via
    /// `InFlightBlock::take_min_write_offset()` — EVERY sync call must pass a
    /// freshly taken value (take and sync are 1:1), or the append guard's
    /// authority is lost. On a key hit nothing was mutated since the last
    /// consumption, so the (already reset) watermark is untouched here.
    pub(crate) fn sync(
        &mut self,
        output: &str,
        pane_session_id: u64,
        version: u64,
        cols: usize,
        screen_origin: bool,
        min_write_offset: usize,
    ) {
        if self.pane_session_id == pane_session_id
            && self.version == version
            && self.cols == cols
            && self.screen_origin == screen_origin
        {
            return;
        }

        // Incremental guard chain (PLAN_M6 §A-2): every condition must prove
        // `output[..synced_byte_end]` is byte-identical to what the window
        // was built from, so the tail can be folded in without a rescan.
        // Any failure falls back to the full rebuild (= the pre-M6-a path)
        // and resets the consumption boundary.
        let can_append = self.pane_session_id == pane_session_id
            && self.cols == cols
            && !screen_origin
            && self.screen_origin == screen_origin
            && output.len() >= self.synced_byte_end
            // Authoritative guard: any capture op touching below the
            // consumption boundary pulls the watermark under it (CSI A
            // progress-bar repaints, backspace-into-history, early gotos).
            && min_write_offset >= self.synced_byte_end
            // Depth-on-defense canary: the boundary must still sit right
            // after a '\n' (or at the document start).
            && (self.synced_byte_end == 0
                || output.as_bytes()[self.synced_byte_end - 1] == b'\n');

        if can_append {
            self.append_tail(output, cols);
            #[cfg(test)]
            {
                self.appends += 1;
            }
        } else {
            self.rebuild_full(output, cols, screen_origin);
            #[cfg(test)]
            {
                self.rebuilds += 1;
            }
        }

        self.pane_session_id = pane_session_id;
        self.version = version;
        self.cols = cols;
        self.screen_origin = screen_origin;
    }

    /// Total display rows of the tail window. Identical to the pre-cache
    /// formula `lines()[skip..].map(|l| <chunk_count>).sum()` — screen-origin
    /// lines are one row each (clip); ordinary lines count their soft-wrap
    /// chunks.
    pub(crate) fn total_display_rows(&self) -> usize {
        let base = self.abs_cums.first().copied().unwrap_or(0);
        let end = self.abs_cums.last().copied().unwrap_or(0);
        end.saturating_sub(base) as usize
    }

    pub(crate) fn cumulative(&self) -> &[u32] {
        &self.cumulative
    }

    pub(crate) fn total_lines(&self) -> usize {
        self.window.len()
    }

    pub(crate) fn base_idx(&self) -> usize {
        self.abs_first_line
    }

    /// Byte range of window line `i` (oldest-first) into the live output.
    pub(crate) fn line_range(&self, i: usize) -> (usize, usize) {
        self.window[i].byte_range
    }

    /// Full-rebuild count since creation — a cache HIT and an incremental
    /// append are both `sync` calls that return without incrementing this.
    /// Test-only (asserts hit/miss keys and guard fallbacks).
    #[cfg(test)]
    pub(crate) fn rebuilds(&self) -> usize {
        self.rebuilds
    }

    /// Incremental-append count since creation. Test-only (asserts the fast
    /// path is actually taken by the guard chain).
    #[cfg(test)]
    pub(crate) fn appends(&self) -> usize {
        self.appends
    }

    /// Visible logical-line window `[start, end)` (oldest-first index space)
    /// via binary search on the epoch-absolute row table, +overscan per side
    /// — the live analogue of the finished-blocks prefix-sum culling.
    ///
    /// Geometry (distance-from-content-bottom space, matching the layout
    /// pass): the live block sits at the bottom; line `i` (oldest-first)
    /// spans `[total - cumulative[i+1], total - cumulative[i]) * pitch`,
    /// so the newest line is lowest and `threshold_low`/`threshold_high`
    /// are the viewport's bottom/top edges.
    ///
    /// Searches the ABSOLUTE table with thresholds translated by the window
    /// total (`cum_rel[j] <= total_rel - t  ⇔  abs[j] <= abs[n] - t`, since
    /// both sides shift by the window base) — no relative re-materialization
    /// on this per-frame path.
    pub(crate) fn visible_window(
        &self,
        pitch: f32,
        threshold_low: f32,
        threshold_high: f32,
        overscan_px: f32,
    ) -> (usize, usize) {
        let overscan_lines = (overscan_px / pitch).ceil() as usize + 1;
        let n = self.total_lines();
        if n == 0 || self.abs_cums.len() != n + 1 {
            return (0, 0);
        }
        let total = self.abs_cums[n] as f32;
        // Mirror the viewport band into cumulative-row space (cumulative is
        // an oldest-first prefix, so the band's top edge maps to a lower
        // bound and its bottom edge to an upper bound).
        let top_rows = total - threshold_high / pitch; // line tops must be >= this
        let bottom_rows = total - threshold_low / pitch; // line bottoms must be <= this
                                                         // bottommost visible line: largest i with abs_cums[i+1] <= bottom_rows
        let newest_visible = last_le(&self.abs_cums, bottom_rows).saturating_sub(1);
        // topmost visible line: smallest i with abs_cums[i] >= top_rows
        let oldest_visible = first_ge(&self.abs_cums, top_rows);
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

    // ── M6-a incremental append (PLAN_M6 §A-2, guard chain passed) ──────

    /// Fold the bytes after `synced_byte_end` into the window. The tail's
    /// complete lines are appended (the first replaces the previous sync's
    /// still-growing partial line when one exists); a document not ending
    /// with '\n' leaves its last line growing — the boundary stays at its
    /// start so the next sync re-reads it. Cost: O(new bytes + one partial).
    fn append_tail(&mut self, output: &str, cols: usize) {
        let tail_start = self.synced_byte_end;
        debug_assert!(output.is_char_boundary(tail_start));
        let tail = &output[tail_start..];
        let ends_with_newline = output.ends_with('\n');
        let base = tail.as_ptr() as usize;
        let tail_ranges: Vec<(usize, usize)> = tail
            .lines()
            .map(|line| {
                let start = tail_start + (line.as_ptr() as usize - base);
                (start, start + line.len())
            })
            .collect();
        // The window's last entry is the growing partial line iff it starts
        // exactly at the consumption boundary (complete lines always end
        // before it — the boundary sits right after their '\n').
        let replaces_partial =
            matches!(self.window.back(), Some(w) if w.byte_range.0 == tail_start);

        if tail_ranges.is_empty() {
            // No new bytes since the last sync. If the previous partial line
            // was erased in the meantime (EL on the tail), drop it — the
            // document now ends at the boundary. Otherwise nothing changed.
            if replaces_partial {
                self.window.pop_back();
                let end = self.window_end_cum(cols, output);
                self.refresh_window_tables(end);
            }
            return;
        }

        let mut end_cum = self.abs_cums.last().copied().unwrap_or(0);
        for (i, &(start, end)) in tail_ranges.iter().enumerate() {
            let rows = count_line_chunks(&output[start..end], cols);
            if i == 0 && replaces_partial {
                // The partial line grew (or was rewritten in place): replace
                // it, keeping its epoch-absolute row prefix.
                let back = self.window.back_mut().expect("replaces_partial");
                back.byte_range = (start, end);
                end_cum = back.abs_cum.saturating_add(rows);
            } else {
                self.window.push_back(WindowLine {
                    byte_range: (start, end),
                    abs_cum: end_cum,
                });
                end_cum = end_cum.saturating_add(rows);
            }
        }
        // Continuous cap: the window is always the newest min(doc, MAX)
        // lines — pop per overflow, no renumbering (absolute prefix space).
        while self.window.len() > MAX_LAYOUT_LINES_LIVE {
            self.window.pop_front();
            self.abs_first_line += 1;
        }
        self.synced_byte_end = if ends_with_newline {
            output.len()
        } else {
            // The last tail line is still growing — do not consume past it.
            tail_ranges[tail_ranges.len() - 1].0
        };
        self.refresh_window_tables(end_cum);
    }

    /// Full rebuild — the pre-M6-a path, now bounded-memory: a single
    /// O(document) `lines()` scan keeps only the newest MAX line ranges,
    /// then chunk-counts just the window. Re-seeds the epoch origin and the
    /// consumption boundary (past the last COMPLETE line).
    fn rebuild_full(&mut self, output: &str, cols: usize, screen_origin: bool) {
        self.window.clear();
        self.abs_first_line = 0;
        let base = output.as_ptr() as usize;
        let mut ranges: VecDeque<(usize, usize)> = VecDeque::new();
        let mut total_lines = 0usize;
        for line in output.lines() {
            let start = line.as_ptr() as usize - base;
            ranges.push_back((start, start + line.len()));
            if ranges.len() > MAX_LAYOUT_LINES_LIVE {
                ranges.pop_front();
            }
            total_lines += 1;
        }
        self.abs_first_line = total_lines - ranges.len();
        // The window restarts the epoch-absolute row prefix at zero: only
        // differences within an epoch are ever consumed, so the origin is
        // arbitrary.
        let mut acc = 0u32;
        for &(start, end) in &ranges {
            // v1.10.26 Batch B review blocker (BL-1): the live layout splits
            // by screen_origin. Screen-owned TUI frames are hard terminal
            // rows — one clipped row per line no matter the width. Ordinary
            // shell output soft-wraps. Must stay in lockstep with
            // layout_pass's per-line chunk function (driven by the same
            // `screen_origin`), or the visible-window prefix sums drift
            // from the laid-out rows.
            let rows = if screen_origin {
                screen_origin_line_chunks(&output[start..end], cols).count() as u32
            } else {
                count_line_chunks(&output[start..end], cols)
            };
            self.window.push_back(WindowLine {
                byte_range: (start, end),
                abs_cum: acc,
            });
            acc = acc.saturating_add(rows);
        }
        self.synced_byte_end = if output.ends_with('\n') || ranges.is_empty() {
            output.len()
        } else {
            // Leave the growing final line unconsumed — its start becomes
            // the append path's partial-line anchor.
            ranges.back().expect("non-empty ranges").0
        };
        self.refresh_window_tables(acc);
    }

    /// Rebuild the materialized tables from the window: `abs_cums` gets the
    /// per-line epoch-absolute prefixes plus the end value (len = window+1),
    /// `cumulative` the window-relative view `cumulative()` contracts to —
    /// `cumulative[i+1]` is the INCLUSIVE prefix of window lines `[0..=i]`,
    /// i.e. `abs_cums[i+1] - base` (the per-line `abs_cum` is exclusive).
    /// O(MAX) per mutating sync (≈8KB — bounded, not O(document)).
    fn refresh_window_tables(&mut self, end_cum: u32) {
        let base = self.window.front().map_or(0, |w| w.abs_cum);
        self.abs_cums.clear();
        self.cumulative.clear();
        self.abs_cums.reserve(self.window.len() + 1);
        self.cumulative.reserve(self.window.len() + 1);
        for w in &self.window {
            self.abs_cums.push(w.abs_cum);
        }
        self.abs_cums.push(end_cum);
        self.cumulative.push(0);
        for i in 1..=self.window.len() {
            let inclusive = self.abs_cums[i];
            self.cumulative.push(inclusive.saturating_sub(base));
        }
    }

    /// Row prefix after the window's last line — recomputed from the output
    /// (only used on the rare erased-partial path where the stored end value
    /// is stale). Append path only, so plain soft-wrap chunking applies.
    fn window_end_cum(&self, cols: usize, output: &str) -> u32 {
        match self.window.back() {
            None => 0,
            Some(w) => {
                let (start, end) = w.byte_range;
                w.abs_cum
                    .saturating_add(count_line_chunks(&output[start..end], cols))
            }
        }
    }
}

/// Chunk (visual-row) count of one ordinary live-layout line.
///
/// M6-a fast path: a line whose WHOLE display width fits `cols` is always
/// exactly one chunk, so the full grapheme + wrap machine only runs for
/// overflowing lines (the append path's per-line cost collapses from
/// grapheme segmentation + String materialization to one width scan).
/// Equivalence argument over `wrap_line_ranges` (all branches):
/// - `cols == 0` → `once(0..line_len)` — one chunk for any line;
/// - prose (`Structure(None)`): a break requires `col + width > cols` for
///   some cluster; with whole-line width ≤ cols (widths are additive, see
///   the gauge branch's verified comment) no cluster overflows → the single
///   final `start..line_byte_len` range;
/// - every structure kind (PureBox / ProgressGauge under tolerance /
///   TableRow) emits `once(0..grapheme_prefix_end(entries, cols))`, which
///   consumes the whole line when it fits.
fn count_line_chunks(line: &str, cols: usize) -> u32 {
    if cols == 0 || terminal_text_width(line) <= cols {
        1
    } else {
        block_line_chunks(line, cols).count() as u32
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
mod tests;
