//! Shared layout pass for BlockView: walk blocks bottom-to-top, accumulate
//! `cursor_dist`, push `LaidRow` rows (paint + hit-testing both consume this).

use std::cell::Cell;
use std::rc::Rc;

use crate::block_component::{
    block_presentation, clear_block_spacer_rows, command_output_gap_rows, command_resume_hints,
    BlockTone,
};
use crate::paint::grid_cache::{
    block_line_chunks, command_line_chunks, screen_origin_line_chunks, BlockLayoutCache,
};
use crate::paint::live_cache::LiveLayoutCache;
use crate::paint::ui_helpers::strip_prompt_prefix;
use weft_core::blocks::{Block, BlockId, InFlightBlock, StyledLine};

/// A laid-out row in the block view. Paint-superset fields (`line`/`style`/
/// `collapsed`/`foldable`/`tone`) are ignored by hit-testing.
pub(super) enum LaidRow<'a> {
    Output {
        text: &'a str,
        chunks: Rc<[String]>,
        block_id: Option<BlockId>,
        /// Line index into the block's output; `usize::MAX` for resume hints.
        line: usize,
        /// Resolved styled line for syntax highlighting. `None` for hit-testing.
        style: Option<&'a StyledLine>,
    },
    Command {
        command: &'a str,
        /// Prompt-stripped command wrapped for the row width. `chunks.len()`
        /// MUST match `cached.command_wrap_rows` (same wrap call) so the
        /// prefix-sum and block_total_height stay in lockstep.
        chunks: Rc<[String]>,
        collapsed: bool,
        foldable: bool,
        block_id: BlockId,
    },
    Header {
        cwd: String,
        duration: String,
        status: String,
        /// 必须保留:surfaces.rs 依赖 tone 画 block 背景/rail。
        tone: BlockTone,
        block_id: BlockId,
    },
    Separator,
    LiveCommand {
        /// Kept for stage 3 (sticky-header reads); render paths use `chunks`.
        #[allow(dead_code)]
        command: &'a str,
        chunks: Rc<[String]>,
    },
    LiveHeader {
        text: String,
    },
    Blank,
    /// A line of the AI diagnose panel below a block's output. `is_first`
    /// marks the top line (close-button hit region); `is_error` tints it.
    DiagnosePanel {
        text: String,
        block_id: BlockId,
        is_first: bool,
        is_last: bool,
        is_error: bool,
    },
}

/// Output of a shared layout pass: cumulative y-distances + row metadata.
pub(super) struct LayoutPassOutput<'a> {
    /// Cumulative y-distance from `content_bottom_y` to the top of each row.
    pub(super) rows: Vec<f32>,
    /// Row metadata parallel to `rows`.
    pub(super) row_data: Vec<LaidRow<'a>>,
    /// Count of finished blocks whose internal lines were expanded
    /// (is_visible && !collapsed) — low vs. `blocks.len()` shows culling works.
    pub(super) expanded_block_count: usize,
}

/// Parameters for the shared layout pass (subset of `BlockViewPaintModel`).
pub(super) struct LayoutPassInput<'a, 'b> {
    pub(super) blocks: &'a [Block],
    pub(super) live: Option<InFlightBlock<'a>>,
    /// Pane session scope for the live-layout cache key (the cache is
    /// renderer-global; per-Tab version counters start at 0, so session id
    /// must join the key or identical version+cols would cross-hit).
    pub(super) pane_session_id: u64,
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
    /// Populate paint-only fields (`style`); hit-testing passes `false`.
    pub(super) resolve_styles: bool,
    /// Counter for `styled.line()` lookups (paint) or `None` (hit-testing).
    pub(super) styled_lookup_counter: Option<&'a Cell<usize>>,
    /// Per-block AI diagnose state; extra rows render the panel after output.
    /// Separate lifetime `'b`: output does not borrow from this field.
    pub(super) block_diagnose_state:
        &'b std::collections::HashMap<BlockId, crate::app_state::BlockDiagnoseState>,
}

/// Run the shared layout pass: walk blocks bottom-to-top, accumulate
/// `cursor_dist`, apply visibility culling, emit `LaidRow` rows (paint +
/// hit-testing both call this). Caller must `ensure_cached` every block.
/// Output borrows from `blocks`/`live`, not `cache` — visible rows only are
/// materialized into `Rc<[String]>`, keeping offscreen history un-duplicated.
/// v1.10.23: `live_cache` (synced internally) locates the visible live-line
/// window via cumulative prefix sums — no per-frame full-document scan.
pub(super) fn compute_block_layout_pass<'a, 'b>(
    input: LayoutPassInput<'a, 'b>,
    cache: &BlockLayoutCache,
    live_cache: &mut LiveLayoutCache,
) -> LayoutPassOutput<'a> {
    let LayoutPassInput {
        blocks,
        live,
        pane_session_id,
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
        block_diagnose_state,
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

    // Live in-flight block (rendered above finished blocks). v1.10.23: the
    // LiveLayoutCache's cumulative prefix sums locate the visible logical-line
    // window in O(log n); only visible ± overscan lines are sliced out.
    let scroll_px = block_scroll * pitch;
    let overscan = header_height + pitch * 2.0;
    if let Some(live) = live {
        live_cache.sync(
            live.output,
            pane_session_id,
            live.version,
            cols,
            live.screen_origin,
        );
        // threshold_low/high = viewport bottom/top edges (bottom-space).
        let (start_idx, end_idx, start_dist) = live_cache.visible_range(
            pitch,
            content_bottom_y + scroll_px - clip_bottom - overscan,
            content_bottom_y + scroll_px - clip_top + overscan,
            overscan,
        );
        cursor_dist = start_dist;
        for i in (start_idx..end_idx).rev() {
            let (byte_start, byte_end) = live_cache.line_range(i);
            let line = &live.output[byte_start..byte_end];
            let line_idx = live_cache.base_idx() + i;
            // v1.10.26 (FIX_WRAP_EPOCH_AND_VIEWPORT_KEEP B-1 + batch review
            // blocker): the live document splits by screen_origin — screen-
            // owned TUI frame rows clip on a narrow window (a `|]` border must
            // never fold onto the next line), ordinary shell output keeps
            // soft-wrap. Must use the SAME chunk function as LiveLayoutCache's
            // rebuild (both read `live.screen_origin`) or the visible-window
            // prefix sums drift from the laid-out rows.
            let chunks: Rc<[String]> = if live.screen_origin {
                Rc::from(screen_origin_line_chunks(line, cols).collect::<Vec<_>>())
            } else {
                Rc::from(block_line_chunks(line, cols).collect::<Vec<_>>())
            };
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
        if live_cache.total_lines() > 0 {
            // Blank sits above the output at the FULL layout height.
            cursor_dist = (live_cache.total_display_rows() + 1) as f32 * pitch;
            rows.push(cursor_dist);
            row_data.push(LaidRow::Blank);
        }
        // Live command wraps too; rows feed cursor_dist only (not prefix sum).
        let live_cmd_chunks: Rc<[String]> = Rc::from(command_line_chunks(
            &strip_prompt_prefix(live.command),
            cols.saturating_sub(2).max(1),
            cols,
        ));
        cursor_dist += live_cmd_chunks.len() as f32 * pitch;
        rows.push(cursor_dist);
        row_data.push(LaidRow::LiveCommand {
            command: live.command,
            chunks: live_cmd_chunks,
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

    // Finished blocks: binary-search the prefix sum for the visible range
    // (O(log n + k*m)) instead of O(n) traversal, +1 overscan per side.
    let live_cursor_dist = cursor_dist;

    let prefix_sum = cache.prefix_sum();
    let clear_ps = cache.clear_prefix_sum();
    let clear_pitch = viewport_rows as f32 * pitch;
    let n = blocks.len();

    // Visible range [start_idx, end_idx), newest-first (0 = blocks[n-1]).
    let (start_idx, end_idx) = if n == 0 || prefix_sum.len() != n + 1 {
        (0usize, n) // fallback: iterate all (no prefix sum built yet)
    } else {
        // cumulative_height(i) = prefix_sum[i]*pitch + i*header_height
        // + clear_ps[i]*clear_pitch (per-frame clear spacer).
        let threshold_low =
            content_bottom_y + scroll_px - clip_bottom - overscan - live_cursor_dist;
        let threshold_high = content_bottom_y + scroll_px - clip_top + overscan - live_cursor_dist;

        // first_visible: smallest idx where cumulative_height(idx+1) >= threshold_low
        let j_low = lower_bound_height(
            prefix_sum,
            clear_ps,
            pitch,
            header_height,
            clear_pitch,
            threshold_low,
        );
        let first_visible = j_low.saturating_sub(1);

        // last_visible+1: smallest idx where cumulative_height(idx) > threshold_high
        let j_high = upper_bound_height(
            prefix_sum,
            clear_ps,
            pitch,
            header_height,
            clear_pitch,
            threshold_high,
        );

        // 1-block overscan each side: partial blocks + sticky header.
        let start = first_visible.saturating_sub(1);
        let end = (j_high + 1).min(n);
        (start, end)
    };

    // Fast-forward cursor_dist past newer offscreen blocks via prefix sum.
    if start_idx > 0 && prefix_sum.len() == n + 1 {
        cursor_dist = live_cursor_dist
            + prefix_sum[start_idx] as f32 * pitch
            + start_idx as f32 * header_height
            + clear_ps[start_idx] as f32 * clear_pitch;
    }

    // Iterate visible range (newest to oldest).
    for idx in start_idx..end_idx {
        let b = &blocks[n - 1 - idx];
        let cached = cache.get(b.id.0);

        // Diagnose panel lines must be computed before block_total_height
        // so the panel's rows count toward visibility culling.
        let diagnose_lines: Vec<String> = if let Some(ds) = block_diagnose_state.get(&b.id) {
            if ds.is_thinking() {
                vec!["Diagnosing…".to_string()]
            } else if let Some(result) = &ds.result {
                let text = match result {
                    Ok(explanation) => format!("AI: {}", explanation),
                    Err(err) => format!("AI error: {}", err),
                };
                text.lines()
                    .flat_map(|line| {
                        block_line_chunks(line, cols.saturating_sub(2)).collect::<Vec<_>>()
                    })
                    .collect()
            } else {
                Vec::new()
            }
        } else {
            Vec::new()
        };
        let diagnose_rows = if b.collapsed { 0 } else { diagnose_lines.len() };
        let is_diagnose_error = block_diagnose_state
            .get(&b.id)
            .and_then(|ds| ds.result.as_ref())
            .map(|r| r.is_err())
            .unwrap_or(false);

        // Block height without expanding lines; cached.output_rows is O(1).
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
            + diagnose_rows as f32 * pitch
            + cached.command_wrap_rows as f32 * pitch // wrapped command
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
            // Diagnose panel rows after output; reading order is top-down,
            // the layout walks bottom-to-top, so iterate in reverse.
            for (i, line) in diagnose_lines.iter().enumerate().rev() {
                cursor_dist += pitch;
                rows.push(cursor_dist);
                row_data.push(LaidRow::DiagnosePanel {
                    text: line.clone(),
                    block_id: b.id,
                    is_first: i == 0,
                    is_last: i == diagnose_lines.len() - 1,
                    is_error: is_diagnose_error,
                });
            }
        } else {
            cursor_dist += (hint_rows + output_rows + diagnose_rows) as f32 * pitch;
        }
        if output_gap_rows > 0 {
            cursor_dist += output_gap_rows as f32 * pitch;
            rows.push(cursor_dist);
            row_data.push(LaidRow::Blank);
        }
        // Command wrap rows: same `command_line_chunks` call + args as
        // grid_cache::compute_block_layout so prefix-sum matches runtime.
        // first_cols 共用 `command_first_cols` 单一来源,防止几何漂移。
        let first_cols = crate::block_component::command_first_cols(cols, cached.foldable);
        let cmd_chunks: Rc<[String]> = if b.collapsed {
            Rc::from([strip_prompt_prefix(&b.command)]) // collapsed: single line
        } else {
            Rc::from(command_line_chunks(
                &strip_prompt_prefix(&b.command),
                first_cols,
                cols,
            ))
        };
        cursor_dist += cmd_chunks.len() as f32 * pitch;
        rows.push(cursor_dist);
        row_data.push(LaidRow::Command {
            command: &b.command,
            chunks: cmd_chunks,
            collapsed: b.collapsed,
            foldable: cached.foldable,
            block_id: b.id,
        });
        let presentation = block_presentation(b, cached.lines.len());
        cursor_dist += header_height;
        rows.push(cursor_dist);
        row_data.push(LaidRow::Header {
            cwd: presentation.cwd,
            duration: presentation.duration,
            status: presentation.status,
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

/// Smallest j where `prefix_sum[j]*pitch + j*header_height +
/// clear_ps[j]*clear_pitch >= threshold`; `len` if none. `clear_pitch`
/// recovers the per-frame clear-block spacer excluded from the prefix sum.
fn lower_bound_height(
    prefix_sum: &[usize],
    clear_ps: &[usize],
    pitch: f32,
    header_height: f32,
    clear_pitch: f32,
    threshold: f32,
) -> usize {
    let mut lo = 0usize;
    let mut hi = prefix_sum.len();
    while lo < hi {
        let mid = (lo + hi) / 2;
        let height = prefix_sum[mid] as f32 * pitch
            + mid as f32 * header_height
            + clear_ps[mid] as f32 * clear_pitch;
        if height < threshold {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    lo
}

/// Smallest j where the height formula is `> threshold` (strict upper
/// bound); `len` if none.
fn upper_bound_height(
    prefix_sum: &[usize],
    clear_ps: &[usize],
    pitch: f32,
    header_height: f32,
    clear_pitch: f32,
    threshold: f32,
) -> usize {
    let mut lo = 0usize;
    let mut hi = prefix_sum.len();
    while lo < hi {
        let mid = (lo + hi) / 2;
        let height = prefix_sum[mid] as f32 * pitch
            + mid as f32 * header_height
            + clear_ps[mid] as f32 * clear_pitch;
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

    /// Empty input → empty output. The only layout-pass test not requiring
    /// a MetalRenderer; GUI integration tests cover paint/hit-test parity.
    #[test]
    fn empty_layout_pass_produces_empty_output() {
        let input = LayoutPassInput {
            blocks: &[],
            live: None,
            pane_session_id: 1,
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
            block_diagnose_state: &std::collections::HashMap::new(),
        };
        let cache = BlockLayoutCache::default();
        let mut live_cache = LiveLayoutCache::default();
        let out = compute_block_layout_pass(input, &cache, &mut live_cache);
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
                version: 1,
                screen_origin: false,
            }),
            pane_session_id: 1,
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
            block_diagnose_state: &std::collections::HashMap::new(),
        };
        let out = compute_block_layout_pass(
            input,
            &BlockLayoutCache::default(),
            &mut LiveLayoutCache::default(),
        );
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

    // Binary-search helpers: correctness of the height formula
    // (`prefix_sum[j]*pitch + j*header_height + clear_ps[j]*clear_pitch`)
    // keeps the O(log n) fast path from skipping or duplicating blocks.
    // `clear_pitch = viewport_rows*pitch` recovers clear-block spacers
    // (excluded from the prefix sum); zeroed clear args exercise the
    // original semantics.

    #[test]
    fn lower_bound_height_empty_returns_zero() {
        // Empty prefix sum (just [0]) → always returns 0 (nothing >= threshold).
        assert_eq!(lower_bound_height(&[0], &[0], 20.0, 24.0, 0.0, 100.0), 1);
        assert_eq!(lower_bound_height(&[0], &[0], 20.0, 24.0, 0.0, 0.0), 0);
    }

    #[test]
    fn lower_bound_height_all_below_threshold_returns_len() {
        // prefix_sum = [0, 5, 10, 15], pitch=20, header=24
        // heights = [0, 5*20+1*24=124, 10*20+2*24=248, 15*20+3*24=372]
        let ps = [0, 5, 10, 15];
        // threshold=400 → all below → returns 4 (len)
        assert_eq!(lower_bound_height(&ps, &[0; 4], 20.0, 24.0, 0.0, 400.0), 4);
    }

    #[test]
    fn lower_bound_height_all_above_threshold_returns_zero() {
        let ps = [0, 5, 10, 15];
        // threshold=-10 → all above (height[0]=0 >= -10) → returns 0
        assert_eq!(lower_bound_height(&ps, &[0; 4], 20.0, 24.0, 0.0, -10.0), 0);
    }

    #[test]
    fn lower_bound_height_finds_first_ge_threshold() {
        // heights = [0, 124, 248, 372]
        let ps = [0, 5, 10, 15];
        // threshold=200 → first >= 200 is index 2 (height=248)
        assert_eq!(lower_bound_height(&ps, &[0; 4], 20.0, 24.0, 0.0, 200.0), 2);
        // threshold=124 → first >= 124 is index 1 (exact match)
        assert_eq!(lower_bound_height(&ps, &[0; 4], 20.0, 24.0, 0.0, 124.0), 1);
        // threshold=125 → first >= 125 is still index 2
        assert_eq!(lower_bound_height(&ps, &[0; 4], 20.0, 24.0, 0.0, 125.0), 2);
    }

    #[test]
    fn upper_bound_height_empty_returns_zero() {
        // upper_bound is strict > : height[0]=0 > -1 → returns 0
        assert_eq!(upper_bound_height(&[0], &[0], 20.0, 24.0, 0.0, -1.0), 0);
        // height[0]=0 > 0 is false → returns 1 (len)
        assert_eq!(upper_bound_height(&[0], &[0], 20.0, 24.0, 0.0, 0.0), 1);
    }

    #[test]
    fn upper_bound_height_all_le_threshold_returns_len() {
        // heights = [0, 124, 248, 372]
        let ps = [0, 5, 10, 15];
        // threshold=400 → all <= 400 → returns 4 (len)
        assert_eq!(upper_bound_height(&ps, &[0; 4], 20.0, 24.0, 0.0, 400.0), 4);
    }

    #[test]
    fn upper_bound_height_finds_first_gt_threshold() {
        // heights = [0, 124, 248, 372]
        let ps = [0, 5, 10, 15];
        // threshold=200 → first > 200 is index 2 (height=248)
        assert_eq!(upper_bound_height(&ps, &[0; 4], 20.0, 24.0, 0.0, 200.0), 2);
        // threshold=124 → first > 124 is index 2 (strict: 124 is not > 124)
        assert_eq!(upper_bound_height(&ps, &[0; 4], 20.0, 24.0, 0.0, 124.0), 2);
        // threshold=125 → first > 125 is index 2
        assert_eq!(upper_bound_height(&ps, &[0; 4], 20.0, 24.0, 0.0, 125.0), 2);
    }

    /// Nonzero `clear_ps` adds `clear_ps[i]*clear_pitch` to the height
    /// (viewport-sized clear spacer excluded from the prefix sum); without
    /// it, blocks above a `clear` are underestimated and culled after scroll.
    #[test]
    fn bound_height_counts_clear_spacer() {
        // prefix_sum = [0, 5, 10, 15, 20], clear_ps = [0, 0, 1, 1, 2]
        // (2 clear blocks among the 4 newest). pitch=20, header=24,
        // clear_pitch = viewport_rows=24 * pitch=20 = 480.
        let ps = [0, 5, 10, 15, 20];
        let clear_ps = [0, 0, 1, 1, 2];
        let pitch = 20.0_f32;
        let header = 24.0_f32;
        let clear_pitch = 480.0_f32;
        // heights: i=0 → 0; i=1 → 124; i=2 → 728; i=3 → 852; i=4 → 1456
        assert_eq!(
            lower_bound_height(&ps, &clear_ps, pitch, header, clear_pitch, 124.0),
            1
        );
        assert_eq!(
            lower_bound_height(&ps, &clear_ps, pitch, header, clear_pitch, 700.0),
            2
        );
        assert_eq!(
            lower_bound_height(&ps, &clear_ps, pitch, header, clear_pitch, 900.0),
            4
        );
        assert_eq!(
            lower_bound_height(&ps, &clear_ps, pitch, header, clear_pitch, 5000.0),
            5
        );
        assert_eq!(
            upper_bound_height(&ps, &clear_ps, pitch, header, clear_pitch, 124.0),
            2
        );
        assert_eq!(
            upper_bound_height(&ps, &clear_ps, pitch, header, clear_pitch, 852.0),
            4
        );
        assert_eq!(
            upper_bound_height(&ps, &clear_ps, pitch, header, clear_pitch, 1456.0),
            5
        );
    }

    /// Visible range = [lower_bound(low) - 1, upper_bound(high) + 1) with
    /// 1-block overscan; verifies the bounds produce a valid covering range.
    #[test]
    fn binary_search_visible_range_contains_expected_blocks() {
        // 10 blocks, each base_row_count=5 → prefix_sum = [0,5,10,...,50]
        let ps: Vec<usize> = (0..=10).map(|i| i * 5).collect();
        let clear_ps = vec![0; ps.len()]; // no clear blocks → clear_pitch unused
        let pitch = 20.0_f32;
        let header = 24.0_f32;
        // height(i) = 5i * 20 + i * 24 = 124i
        // Say viewport covers blocks 3..6 (heights 372..744).
        // threshold_low=350 → lower_bound finds first >= 350 → idx 3 (height=372)
        // threshold_high=760 → upper_bound finds first > 760 → idx 7 (height=868)
        let j_low = lower_bound_height(&ps, &clear_ps, pitch, header, 0.0, 350.0);
        let j_high = upper_bound_height(&ps, &clear_ps, pitch, header, 0.0, 760.0);
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

    /// Criterion micro-benchmark (run with `--release --ignored
    /// --nocapture --test-threads=1`): all_visible (±∞ clip) vs.
    /// culling_heavy (800px viewport) at 1k/5k/10k/50k blocks; the ratio
    /// shows whether visibility culling is effective.
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
                    screen_origin: false,
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
            // Build prefix sum so the binary-search fast path is exercised.
            cache.build_prefix_sum(&blocks);

            // Scenario 1: all blocks visible (clip bounds = ±∞)
            group.bench_function(format!("all_visible/{count}"), |b| {
                b.iter(|| {
                    let input = LayoutPassInput {
                        blocks: &blocks,
                        live: None,
                        pane_session_id: 1,
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
                        block_diagnose_state: &std::collections::HashMap::new(),
                    };
                    let out =
                        compute_block_layout_pass(input, &cache, &mut LiveLayoutCache::default());
                    black_box(out.expanded_block_count);
                });
            });

            // Heavy culling: 800px viewport, scroll=0 — only ~2 blocks
            // visible; the rest accumulate cursor_dist without expansion.
            group.bench_function(format!("culling_heavy/{count}"), |b| {
                b.iter(|| {
                    let input = LayoutPassInput {
                        blocks: &blocks,
                        live: None,
                        pane_session_id: 1,
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
                        block_diagnose_state: &std::collections::HashMap::new(),
                    };
                    let out =
                        compute_block_layout_pass(input, &cache, &mut LiveLayoutCache::default());
                    black_box(out.expanded_block_count);
                });
            });
        }

        group.finish();
        criterion.final_summary();
    }
}
