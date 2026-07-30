//! Shared BlockView layout pass — pure geometry computation shared by
//! `build_block_view_vertices` (paint) and `compute_block_view_rows`
//! (hit-testing). Eliminates ~200 lines of duplicated layout logic.
//!
//! Both paths walk blocks bottom-to-top, accumulate `cursor_dist`, and
//! push `LaidRow` entries. The only difference is that paint additionally
//! emits vertices + hit regions, while hit-testing only extracts `bv_rows`.
//! By sharing this pass, visibility culling and y-band computation live
//! in one place.

use std::cell::Cell;
use std::rc::Rc;

use crate::block_component::{
    block_presentation, clear_block_spacer_rows, command_output_gap_rows, command_resume_hints,
    BlockTone,
};
use crate::paint::grid_cache::{block_line_chunks, BlockLayoutCache, MAX_LAYOUT_LINES_LIVE};
use weft_core::blocks::{Block, BlockId, InFlightBlock, StyledLine};

/// A laid-out row in the block view. Contains paint-superset fields so both
/// paint and hit-testing can consume the same layout pass output. Fields
/// like `line`/`style`/`collapsed`/`foldable`/`tone` are only used by paint;
/// hit-testing ignores them.
pub(super) enum LaidRow<'a> {
    Output {
        text: &'a str,
        chunks: Rc<[String]>,
        block_id: Option<BlockId>,
        /// Line index into the block's output (or `usize::MAX` for resume
        /// hints). Paint uses this to look up `StyledLine`.
        line: usize,
        /// Resolved styled line for syntax highlighting. `None` for hit-testing.
        style: Option<&'a StyledLine>,
    },
    Command {
        command: &'a str,
        collapsed: bool,
        foldable: bool,
        block_id: BlockId,
    },
    Header {
        text: String,
        tone: BlockTone,
        block_id: BlockId,
    },
    Separator,
    LiveCommand {
        command: &'a str,
    },
    LiveHeader {
        text: String,
    },
    Blank,
}

/// Output of a shared layout pass: cumulative y-distances + row metadata.
pub(super) struct LayoutPassOutput<'a> {
    /// Cumulative y-distance for each row (ascending). `rows[i]` is the
    /// distance from `content_bottom_y` to the top of row `i`.
    pub(super) rows: Vec<f32>,
    /// Row metadata parallel to `rows`.
    pub(super) row_data: Vec<LaidRow<'a>>,
    /// Batch 6 Step 1: actual count of finished blocks whose internal lines
    /// were expanded (is_visible && !collapsed). Excludes blocks that only
    /// accumulated cursor_dist in the else branch. When this is << blocks.len(),
    /// visibility culling is doing its job.
    pub(super) expanded_block_count: usize,
}

/// Parameters for the shared layout pass. Extracted from `BlockViewPaintModel`
/// so the pass function doesn't need the full model (some fields like
/// `palette`/`spinner_phase`/`block_hovered` are paint-only).
pub(super) struct LayoutPassInput<'a> {
    pub(super) blocks: &'a [Block],
    pub(super) live: Option<InFlightBlock<'a>>,
    pub(super) cwd: Option<&'a str>,
    pub(super) git_branch: Option<&'a str>,
    pub(super) block_scroll: f32,
    pub(super) viewport_rows: usize,
    pub(super) cols: usize,
    pub(super) pitch: f32,
    pub(super) header_height: f32,
    pub(super) content_bottom_y: f32,
    pub(super) clip_top: f32,
    pub(super) clip_bottom: f32,
    /// Whether to populate paint-only fields (`style`). Hit-testing passes
    /// `false` to skip `styled_output` lookups.
    pub(super) resolve_styles: bool,
    /// Batch 6 Step 1: optional counter incremented for every `styled.line()`
    /// lookup performed. Paint passes `Some(&Cell)` to collect data for the
    /// styled-line caching decision; hit-testing passes `None`.
    pub(super) styled_lookup_counter: Option<&'a Cell<usize>>,
}

/// Run the shared layout pass: walk blocks bottom-to-top, accumulate
/// `cursor_dist`, apply visibility culling, and emit `LaidRow` entries.
///
/// This is the single source of truth for block-view row geometry. Both
/// `build_block_view_vertices` and `compute_block_view_rows` call this and
/// then walk the output to either emit vertices or extract `bv_rows`.
///
/// `cache` must have `ensure_cached` called for every block before invoking
/// this function (the caller does this in a separate mutable borrow).
///
/// The returned `LayoutPassOutput` borrows from `blocks`/`live` (for text
/// references) but NOT from `cache`: source ranges are materialized only for
/// visible rows into `Rc<[String]>`. This lets the caller drop the cache
/// borrow immediately and keeps offscreen history from duplicating text.
pub(super) fn compute_block_layout_pass<'a>(
    input: LayoutPassInput<'a>,
    cache: &BlockLayoutCache,
) -> LayoutPassOutput<'a> {
    let LayoutPassInput {
        blocks,
        live,
        cwd,
        git_branch,
        block_scroll,
        viewport_rows,
        cols,
        pitch,
        header_height,
        content_bottom_y,
        clip_top,
        clip_bottom,
        resolve_styles,
        styled_lookup_counter,
    } = input;

    let mut rows: Vec<f32> = Vec::new();
    let mut row_data: Vec<LaidRow> = Vec::new();
    let mut cursor_dist = 0.0;
    let mut expanded_block_count = 0usize;
    let bump_styled = |n: usize| {
        if let Some(c) = styled_lookup_counter {
            c.set(c.get().saturating_add(n));
        }
    };

    // Live in-flight block (rendered above finished blocks).
    if let Some(live) = live {
        let all_lines: Vec<&str> = live.output.lines().collect();
        let skip = all_lines.len().saturating_sub(MAX_LAYOUT_LINES_LIVE);
        let live_lines: Vec<&str> = all_lines[skip..].to_vec();
        let base_idx = skip;
        for (i, line) in live_lines.iter().enumerate().rev() {
            let line_idx = base_idx + i;
            let chunks: Rc<[String]> = Rc::from(block_line_chunks(line, cols).collect::<Vec<_>>());
            let vis_rows = chunks.len();
            cursor_dist += vis_rows as f32 * pitch;
            rows.push(cursor_dist);
            row_data.push(LaidRow::Output {
                text: line,
                chunks,
                block_id: None,
                line: line_idx,
                style: if resolve_styles {
                    live.styled_output.and_then(|styled| {
                        bump_styled(1);
                        styled.line(line_idx)
                    })
                } else {
                    None
                },
            });
        }
        if !live_lines.is_empty() {
            cursor_dist += pitch;
            rows.push(cursor_dist);
            row_data.push(LaidRow::Blank);
        }
        cursor_dist += pitch;
        rows.push(cursor_dist);
        row_data.push(LaidRow::LiveCommand {
            command: live.command,
        });
        if let Some(text) = crate::block_component::live_context_label(live.cwd.or(cwd), git_branch)
        {
            cursor_dist += pitch;
            rows.push(cursor_dist);
            row_data.push(LaidRow::LiveHeader { text });
        }
        cursor_dist += pitch;
        rows.push(cursor_dist);
        row_data.push(LaidRow::Separator);
    }

    // Finished blocks: R2-2 (Batch 7) prefix-sum binary search.
    //
    // Instead of O(n) traversal of all blocks, binary search for the
    // visible range using the prefix sum of `base_row_count` and only
    // iterate those blocks + 1 overscan on each side. Offscreen blocks
    // are skipped entirely (no push to rows/row_data), reducing layout
    // pass from O(n + k*m) to O(log n + k*m) where k = visible count.
    //
    // `sync_rows` (selection.rs:409) already handles rows scrolling out
    // of the visible set by remapping to the closest y-center, so
    // omitting offscreen blocks from rows/row_data is safe.
    let scroll_px = block_scroll * pitch;
    let overscan = header_height + pitch * 2.0;
    let live_cursor_dist = cursor_dist;

    let prefix_sum = cache.prefix_sum();
    let n = blocks.len();

    // Determine the visible block range [start_idx, end_idx) in
    // "newest-first" index space (0 = newest = blocks[n-1]).
    let (start_idx, end_idx) = if n == 0 || prefix_sum.len() != n + 1 {
        (0usize, n) // fallback: iterate all (no prefix sum built yet)
    } else {
        // cumulative_height(i) = prefix_sum[i] * pitch + i * header_height
        // = total height of the i newest finished blocks (excl. live).
        // clear_rows (per-frame, only for `clear` command) is intentionally
        // excluded from the prefix sum; this may cause ±1 block of slop
        // at the edges, covered by the 1-block overscan below.
        let threshold_low =
            content_bottom_y + scroll_px - clip_bottom - overscan - live_cursor_dist;
        let threshold_high = content_bottom_y + scroll_px - clip_top + overscan - live_cursor_dist;

        // first_visible: smallest idx where cumulative_height(idx+1) >= threshold_low
        let j_low = lower_bound_height(prefix_sum, pitch, header_height, threshold_low);
        let first_visible = j_low.saturating_sub(1);

        // last_visible+1: smallest idx where cumulative_height(idx) > threshold_high
        let j_high = upper_bound_height(prefix_sum, pitch, header_height, threshold_high);

        // 1-block overscan on each side to cover partial blocks and the
        // sticky header (which references the topmost visible block).
        let start = first_visible.saturating_sub(1);
        let end = (j_high + 1).min(n);
        (start, end)
    };

    // Fast-forward cursor_dist to the start of the visible range.
    // Blocks before start_idx (newer, already below the viewport) are
    // skipped — their height is accounted for via the prefix sum.
    if start_idx > 0 && prefix_sum.len() == n + 1 {
        cursor_dist = live_cursor_dist
            + prefix_sum[start_idx] as f32 * pitch
            + start_idx as f32 * header_height;
    }

    // Iterate visible range (newest to oldest).
    for idx in start_idx..end_idx {
        let b = &blocks[n - 1 - idx];
        let cached = cache.get(b.id.0);

        // Compute this block's total height without expanding internal
        // lines. Uses cached.output_rows (O(1)) for the output portion.
        let hint_rows: usize = if b.collapsed {
            0
        } else {
            command_resume_hints(b)
                .iter()
                .map(|hint| block_line_chunks(hint, cols).count())
                .sum()
        };
        let output_rows = if b.collapsed { 0 } else { cached.output_rows };
        let clear_rows = clear_block_spacer_rows(&b.command, viewport_rows);
        let output_gap_rows = command_output_gap_rows(hint_rows + output_rows);
        let block_total_height = (hint_rows + output_rows) as f32 * pitch
            + output_gap_rows as f32 * pitch
            + pitch // command
            + header_height
            + pitch // separator
            + clear_rows as f32 * pitch;

        let block_dist_before = cursor_dist;
        let block_dist_after = cursor_dist + block_total_height;
        let block_y_top = content_bottom_y - block_dist_after + scroll_px;
        let block_y_bottom = content_bottom_y - block_dist_before + scroll_px;

        let is_visible =
            block_y_bottom >= clip_top - overscan && block_y_top <= clip_bottom + overscan;

        if is_visible && !b.collapsed {
            expanded_block_count += 1;
            for hint in command_resume_hints(b).iter().rev() {
                let chunks: Rc<[String]> =
                    Rc::from(block_line_chunks(hint, cols).collect::<Vec<_>>());
                cursor_dist += chunks.len() as f32 * pitch;
                rows.push(cursor_dist);
                row_data.push(LaidRow::Output {
                    text: hint,
                    chunks,
                    block_id: Some(b.id),
                    line: usize::MAX,
                    style: None,
                });
            }
            for line in cached.lines.iter().rev() {
                let text = &b.output[line.byte_start..line.byte_end];
                let vis_rows = line.chunk_ranges.len();
                cursor_dist += vis_rows as f32 * pitch;
                rows.push(cursor_dist);
                row_data.push(LaidRow::Output {
                    text,
                    chunks: Rc::from(
                        line.chunk_ranges
                            .iter()
                            .map(|range| text[range.clone()].to_string())
                            .collect::<Vec<_>>(),
                    ),
                    block_id: Some(b.id),
                    line: line.idx,
                    style: if resolve_styles {
                        b.styled_output.as_deref().and_then(|styled| {
                            bump_styled(1);
                            styled.line(line.idx)
                        })
                    } else {
                        None
                    },
                });
            }
        } else {
            cursor_dist += (hint_rows + output_rows) as f32 * pitch;
        }
        if output_gap_rows > 0 {
            cursor_dist += output_gap_rows as f32 * pitch;
            rows.push(cursor_dist);
            row_data.push(LaidRow::Blank);
        }
        cursor_dist += pitch;
        rows.push(cursor_dist);
        row_data.push(LaidRow::Command {
            command: &b.command,
            collapsed: b.collapsed,
            foldable: cached.foldable,
            block_id: b.id,
        });
        let presentation = block_presentation(b, cached.lines.len());
        cursor_dist += header_height;
        rows.push(cursor_dist);
        row_data.push(LaidRow::Header {
            text: presentation.label,
            tone: presentation.tone,
            block_id: b.id,
        });
        cursor_dist += pitch;
        rows.push(cursor_dist);
        row_data.push(LaidRow::Separator);
        if clear_rows > 0 {
            cursor_dist += clear_rows as f32 * pitch;
            rows.push(cursor_dist);
            row_data.push(LaidRow::Blank);
        }
    }

    LayoutPassOutput {
        rows,
        row_data,
        expanded_block_count,
    }
}

/// R2-2 (Batch 7): Find smallest j where `prefix_sum[j] * pitch +
/// j * header_height >= threshold`. Returns `prefix_sum.len()` if all
/// elements are below threshold.
fn lower_bound_height(
    prefix_sum: &[usize],
    pitch: f32,
    header_height: f32,
    threshold: f32,
) -> usize {
    let mut lo = 0usize;
    let mut hi = prefix_sum.len();
    while lo < hi {
        let mid = (lo + hi) / 2;
        let height = prefix_sum[mid] as f32 * pitch + mid as f32 * header_height;
        if height < threshold {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    lo
}

/// R2-2 (Batch 7): Find smallest j where `prefix_sum[j] * pitch +
/// j * header_height > threshold` (upper bound). Returns
/// `prefix_sum.len()` if all elements are <= threshold.
fn upper_bound_height(
    prefix_sum: &[usize],
    pitch: f32,
    header_height: f32,
    threshold: f32,
) -> usize {
    let mut lo = 0usize;
    let mut hi = prefix_sum.len();
    while lo < hi {
        let mid = (lo + hi) / 2;
        let height = prefix_sum[mid] as f32 * pitch + mid as f32 * header_height;
        if height <= threshold {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    lo
}

#[cfg(test)]
#[path = "layout_pass/gap_tests.rs"]
mod gap_tests;

#[cfg(test)]
mod tests {
    use super::*;

    /// Smoke test: empty input produces empty output. This is the only
    /// layout-pass test that doesn't require a MetalRenderer; full
    /// equivalence between paint and hit-testing paths is covered by the
    /// 1073+ workspace tests that exercise `compute_block_view_rows` and
    /// `build_block_view_vertices` indirectly through GUI integration tests.
    #[test]
    fn empty_layout_pass_produces_empty_output() {
        let input = LayoutPassInput {
            blocks: &[],
            live: None,
            cwd: None,
            git_branch: None,
            block_scroll: 0.0,
            viewport_rows: 0,
            cols: 80,
            pitch: 20.0,
            header_height: 24.0,
            content_bottom_y: 800.0,
            clip_top: 0.0,
            clip_bottom: 800.0,
            resolve_styles: false,
            styled_lookup_counter: None,
        };
        let cache = BlockLayoutCache::default();
        let out = compute_block_layout_pass(input, &cache);
        assert!(out.rows.is_empty());
        assert!(out.row_data.is_empty());
        assert_eq!(out.expanded_block_count, 0);
    }

    #[test]
    fn live_output_keeps_newest_row_complete_at_viewport_bottom() {
        let output = (0..20)
            .map(|line| format!("line-{line}\n"))
            .collect::<String>();
        let input = LayoutPassInput {
            blocks: &[],
            live: Some(InFlightBlock {
                command: "long-running-command",
                cwd: Some("/tmp"),
                output: &output,
                styled_output: None,
            }),
            cwd: None,
            git_branch: None,
            block_scroll: 0.0,
            viewport_rows: 6,
            cols: 80,
            pitch: 20.0,
            header_height: 24.0,
            content_bottom_y: 120.0,
            clip_top: 0.0,
            clip_bottom: 120.0,
            resolve_styles: false,
            styled_lookup_counter: None,
        };
        let out = compute_block_layout_pass(input, &BlockLayoutCache::default());
        let newest = out
            .row_data
            .iter()
            .position(|row| matches!(row, LaidRow::Output { text, .. } if *text == "line-19"))
            .expect("newest live row");
        let newest_top = 120.0 - out.rows[newest];

        assert_eq!(newest_top, 100.0);
        assert_eq!(newest_top + 20.0, 120.0);
        assert!(out.rows.iter().any(|distance| 120.0 - distance < 0.0));
    }

    // ── R2-2 (Batch 7): binary search helpers ─────────────────────────
    //
    // `lower_bound_height` / `upper_bound_height` find the visible block
    // range via binary search on the prefix sum. Their correctness is what
    // keeps the O(log n) fast path from skipping or duplicating blocks.
    // The height formula is `prefix_sum[j] * pitch + j * header_height`.

    #[test]
    fn lower_bound_height_empty_returns_zero() {
        // Empty prefix sum (just [0]) → always returns 0 (nothing >= threshold).
        assert_eq!(lower_bound_height(&[0], 20.0, 24.0, 100.0), 1);
        assert_eq!(lower_bound_height(&[0], 20.0, 24.0, 0.0), 0);
    }

    #[test]
    fn lower_bound_height_all_below_threshold_returns_len() {
        // prefix_sum = [0, 5, 10, 15], pitch=20, header=24
        // heights = [0, 5*20+1*24=124, 10*20+2*24=248, 15*20+3*24=372]
        let ps = [0, 5, 10, 15];
        // threshold=400 → all below → returns 4 (len)
        assert_eq!(lower_bound_height(&ps, 20.0, 24.0, 400.0), 4);
    }

    #[test]
    fn lower_bound_height_all_above_threshold_returns_zero() {
        let ps = [0, 5, 10, 15];
        // threshold=-10 → all above (height[0]=0 >= -10) → returns 0
        assert_eq!(lower_bound_height(&ps, 20.0, 24.0, -10.0), 0);
    }

    #[test]
    fn lower_bound_height_finds_first_ge_threshold() {
        // heights = [0, 124, 248, 372]
        let ps = [0, 5, 10, 15];
        // threshold=200 → first >= 200 is index 2 (height=248)
        assert_eq!(lower_bound_height(&ps, 20.0, 24.0, 200.0), 2);
        // threshold=124 → first >= 124 is index 1 (exact match)
        assert_eq!(lower_bound_height(&ps, 20.0, 24.0, 124.0), 1);
        // threshold=125 → first >= 125 is still index 2
        assert_eq!(lower_bound_height(&ps, 20.0, 24.0, 125.0), 2);
    }

    #[test]
    fn upper_bound_height_empty_returns_zero() {
        // upper_bound is strict > : height[0]=0 > -1 → returns 0
        assert_eq!(upper_bound_height(&[0], 20.0, 24.0, -1.0), 0);
        // height[0]=0 > 0 is false → returns 1 (len)
        assert_eq!(upper_bound_height(&[0], 20.0, 24.0, 0.0), 1);
    }

    #[test]
    fn upper_bound_height_all_le_threshold_returns_len() {
        // heights = [0, 124, 248, 372]
        let ps = [0, 5, 10, 15];
        // threshold=400 → all <= 400 → returns 4 (len)
        assert_eq!(upper_bound_height(&ps, 20.0, 24.0, 400.0), 4);
    }

    #[test]
    fn upper_bound_height_finds_first_gt_threshold() {
        // heights = [0, 124, 248, 372]
        let ps = [0, 5, 10, 15];
        // threshold=200 → first > 200 is index 2 (height=248)
        assert_eq!(upper_bound_height(&ps, 20.0, 24.0, 200.0), 2);
        // threshold=124 → first > 124 is index 2 (strict: 124 is not > 124)
        assert_eq!(upper_bound_height(&ps, 20.0, 24.0, 124.0), 2);
        // threshold=125 → first > 125 is index 2
        assert_eq!(upper_bound_height(&ps, 20.0, 24.0, 125.0), 2);
    }

    /// The visible range is [lower_bound(threshold_low) - 1, upper_bound(threshold_high) + 1)
    /// with 1-block overscan on each side. This test verifies the bounds
    /// produce a valid range that contains the visible blocks.
    #[test]
    fn binary_search_visible_range_contains_expected_blocks() {
        // 10 blocks, each base_row_count=5 → prefix_sum = [0,5,10,...,50]
        let ps: Vec<usize> = (0..=10).map(|i| i * 5).collect();
        let pitch = 20.0_f32;
        let header = 24.0_f32;
        // height(i) = 5i * 20 + i * 24 = 124i
        // Say viewport covers blocks 3..6 (heights 372..744).
        // threshold_low=350 → lower_bound finds first >= 350 → idx 3 (height=372)
        // threshold_high=760 → upper_bound finds first > 760 → idx 7 (height=868)
        let j_low = lower_bound_height(&ps, pitch, header, 350.0);
        let j_high = upper_bound_height(&ps, pitch, header, 760.0);
        assert_eq!(j_low, 3);
        assert_eq!(j_high, 7);
        // first_visible = j_low - 1 = 2, with overscan start=1
        // end = j_high + 1 = 8
        // Visible range [1, 8) covers blocks 1..7 — includes 3..6 with overscan.
        let first_visible = j_low.saturating_sub(1);
        let start = first_visible.saturating_sub(1);
        let end = (j_high + 1).min(10);
        assert_eq!(start, 1);
        assert_eq!(end, 8);
    }

    /// Batch 6 Step 3 (R2-2): criterion micro-benchmark for
    /// `compute_block_layout_pass`.
    ///
    /// Run with:
    /// ```sh
    /// cargo test --release -p weft_app --bin weft \
    ///   bench_layout_pass -- --nocapture --ignored --test-threads=1
    /// ```
    ///
    /// Measures two scenarios at 1k/5k/10k/50k blocks:
    /// - `all_visible`: clip bounds = ±∞ (every block expanded)
    /// - `culling_heavy`: 800px viewport, scroll=0 (only ~2 blocks visible)
    ///
    /// The ratio proves whether visibility culling is effective, and the
    /// absolute numbers drive the prefix-sum decision (BATCH4/5 deferred
    /// pending real data).
    #[test]
    #[ignore = "criterion micro-benchmark; run with --release --ignored --nocapture"]
    fn bench_layout_pass() {
        use criterion::{black_box, Criterion};
        use std::sync::Arc;
        use std::time::SystemTime;
        use weft_core::blocks::{Block, BlockId};

        fn make_blocks(count: usize) -> Vec<Block> {
            let line = "x".repeat(80);
            let output: Arc<str> = Arc::from(
                (0..20u32)
                    .map(|i| format!("{i:03}: {line}\n"))
                    .collect::<String>()
                    .as_str(),
            );
            (0..count)
                .map(|i| Block {
                    id: BlockId(i as u64),
                    command: format!("echo test_{i}"),
                    cwd: Some("/home/user".into()),
                    output: Arc::clone(&output),
                    styled_output: None,
                    exit_code: Some(0),
                    started_at: SystemTime::now(),
                    finished_at: Some(SystemTime::now()),
                    collapsed: false,
                })
                .collect()
        }

        let mut criterion = Criterion::default().sample_size(10);
        let mut group = criterion.benchmark_group("layout_pass");

        for &count in &[1_000usize, 5_000, 10_000, 50_000] {
            let blocks = make_blocks(count);
            let mut cache = BlockLayoutCache::default();
            for b in &blocks {
                cache.ensure_cached(b, 80);
            }
            // R2-2 (Batch 7): build prefix sum so the binary-search fast
            // path is exercised (otherwise the fallback iterates all blocks).
            cache.build_prefix_sum(&blocks);

            // Scenario 1: all blocks visible (clip bounds = ±∞)
            group.bench_function(format!("all_visible/{count}"), |b| {
                b.iter(|| {
                    let input = LayoutPassInput {
                        blocks: &blocks,
                        live: None,
                        cwd: None,
                        git_branch: None,
                        block_scroll: 0.0,
                        viewport_rows: 40,
                        cols: 80,
                        pitch: 20.0,
                        header_height: 24.0,
                        content_bottom_y: 800.0,
                        clip_top: -1e9,
                        clip_bottom: 1e9,
                        resolve_styles: true,
                        styled_lookup_counter: None,
                    };
                    let out = compute_block_layout_pass(input, &cache);
                    black_box(out.expanded_block_count);
                });
            });

            // Scenario 2: heavy culling — 800px viewport, scroll=0.
            // Only the bottom ~2 blocks are visible; the rest accumulate
            // cursor_dist without expanding internal lines.
            group.bench_function(format!("culling_heavy/{count}"), |b| {
                b.iter(|| {
                    let input = LayoutPassInput {
                        blocks: &blocks,
                        live: None,
                        cwd: None,
                        git_branch: None,
                        block_scroll: 0.0,
                        viewport_rows: 40,
                        cols: 80,
                        pitch: 20.0,
                        header_height: 24.0,
                        content_bottom_y: 800.0,
                        clip_top: 0.0,
                        clip_bottom: 800.0,
                        resolve_styles: true,
                        styled_lookup_counter: None,
                    };
                    let out = compute_block_layout_pass(input, &cache);
                    black_box(out.expanded_block_count);
                });
            });
        }

        group.finish();
        criterion.final_summary();
    }
}
