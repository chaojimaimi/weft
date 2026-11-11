//! Shared layout pass for BlockView: walk blocks bottom-to-top, accumulate
//! `cursor_dist`, push `LaidRow` rows (paint + hit-testing both consume this).

use std::borrow::Cow;
use std::cell::Cell;
use std::rc::Rc;

use crate::block_component::BlockTone;
use crate::paint::grid_cache::{
    block_line_chunks, command_line_chunks, screen_origin_line_chunks, BlockLayoutCache,
};

#[cfg(test)]
#[path = "layout_pass/bench.rs"]
mod bench;
// v1.13.11: `pub(crate)` so the renderer's block-view warmup can reach the
// shared height bounds (the layout formula's authoritative home).
pub(crate) mod bounds;
#[cfg(test)]
#[path = "layout_pass/window_tests.rs"]
mod window_tests;
use crate::paint::live_cache::LiveLayoutCache;
use crate::paint::region_cache::{PaneRegionState, RegionSource};
use crate::paint::ui_helpers::{live_context_label, live_elapsed_label, strip_prompt_prefix};
use weft_core::blocks::{Block, BlockId, InFlightBlock, StyledLine};

// v1.19.0 (PLAN_v1190 §4 P1-1): flag-gated region-track live layout.
#[path = "layout_pass/region_pass.rs"]
mod region_pass;
#[path = "layout_pass/region_rows.rs"]
mod region_rows;
// v1.19.2 (PLAN_v1192 §1.1b): the finished-blocks loop, shared by the
// capture track (compute_block_layout_pass) and the region track
// (region_pass::compute_region_layout_pass).
#[path = "layout_pass/completed_pass.rs"]
mod completed_pass;

// v1.20.0 (PLAN_v1200 §4 D1): screen-space chrome carrier.
use super::overlay::OverlayChromeRow;

/// A laid-out row in the block view. Paint-superset fields (`line`/`style`/
/// `collapsed`/`foldable`/`tone`) are ignored by hit-testing.
pub(super) enum LaidRow<'a> {
    /// ONE visual output row (M5-b, PLAN_M5 §三 R-a: one entry per EMITTED
    /// visual row, not per source line). `text` is the row's own text —
    /// zero-copy byte slice of `block.output` for finished blocks, an owned
    /// clone for hints/live lines (not L2-backed). `line_text` is the full
    /// source line (semantic text for styled paint and find ranges); a row
    /// that spans its whole line carries `text == line_text` so the legacy
    /// single-chunk behavior (paint the line, renderer `max_cols` clips —
    /// relied on by screen-origin rows) is preserved byte-for-byte.
    /// `chunk_idx` is the row's index within its line's visual rows — the
    /// styled cache key `chunk_idx` AND the y offset (`y + chunk_idx *
    /// pitch`, byte-equal to the legacy per-line push plus per-chunk
    /// offsets). `char_offset` counts the chars before this row within the
    /// source line (consumption-time UTF-8 prefix count, `char_offset_at`).
    Output {
        line_text: &'a str,
        text: Cow<'a, str>,
        block_id: Option<BlockId>,
        /// Line index into the block's output; `usize::MAX` for resume hints.
        line: usize,
        chunk_idx: usize,
        char_offset: usize,
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
    /// v1.20.0 (PLAN_v1200 §4 D1): completed-block chrome (gap/header/
    /// separator) emitted as screen-space overlays when
    /// `complete_block_overlay` is ON. Empty under flag OFF (chrome stays in
    /// `row_data`). Populated only for blocks the assembly pass walked.
    pub(super) overlay_rows: Vec<OverlayChromeRow>,
}

/// Parameters for the shared layout pass (subset of `BlockViewPaintModel`).
pub(super) struct LayoutPassInput<'a, 'b, 'c> {
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
    /// v1.20.0 (PLAN_v1200 §4 D10): completed-block chrome overlay switch —
    /// flag ON stops emitting gap/Header/Separator LaidRows and emits
    /// screen-space `OverlayChromeRow`s instead (D1/D3/D7/D8 ride the same
    /// switch).
    pub(super) complete_block_overlay: bool,
    /// Per-block AI diagnose state; extra rows render the panel after output.
    /// Separate lifetime `'b`: output does not borrow from this field.
    pub(super) block_diagnose_state:
        &'b std::collections::HashMap<BlockId, crate::app_state::BlockDiagnoseState>,
    /// P1: frame clock for the live header's elapsed label (passed through
    /// from `BlockViewPaintModel::now`, so paint and hit-test share one read).
    pub(super) now: std::time::SystemTime,
    /// v1.19.0 (PLAN_v1190 §4 P1-1): flag-ON region track — grid-region
    /// source and its synced snapshot (`RegionLiveCache`). Both `None` on
    /// the capture path (flag OFF); `region` borrows the grid (`'c`).
    pub(super) region: Option<RegionSource<'c>>,
    pub(super) region_state: Option<&'a PaneRegionState>,
}

/// Run the shared layout pass: walk blocks bottom-to-top, accumulate
/// `cursor_dist`, apply visibility culling, emit `LaidRow` rows (paint +
/// hit-testing both call this). Caller must `ensure_cached` every block.
/// M5-b (PLAN_M5 §三 R-a): output rows are emitted O(visible rows) — the
/// L2 `line_row_base` is binary-searched per block and only visible visual
/// rows slice the block's immutable `output` (zero-copy). The output DOES
/// borrow from `cache` now (row text derives from `CachedBlockLayout`),
/// so the cache borrow must outlive the laid rows. Live rows materialize
/// per-chunk strings as before (`LiveLayoutCache` is out of M5 scope).
pub(super) fn compute_block_layout_pass<'a, 'b, 'c>(
    input: LayoutPassInput<'a, 'b, 'c>,
    cache: &'a BlockLayoutCache,
    live_cache: &mut LiveLayoutCache,
) -> LayoutPassOutput<'a> {
    // v1.19.0 (PLAN_v1190 §4 P1-1): region-track dispatch — under the flag
    // the live block renders from the grid-region snapshot; the capture
    // branch below stays byte-for-byte untouched (flag OFF path).
    if let (Some(_), Some(_)) = (&input.region, input.region_state) {
        if input.live.is_some() {
            return region_pass::compute_region_layout_pass(&input, cache);
        }
    }
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
        complete_block_overlay,
        block_diagnose_state,
        now,
        region: _,
        region_state: _,
    } = input;

    let mut rows: Vec<f32> = Vec::new();
    let mut row_data: Vec<LaidRow> = Vec::new();
    let mut cursor_dist = 0.0;
    let bump_styled = |n: usize| {
        if let Some(c) = styled_lookup_counter {
            c.set(c.get().saturating_add(n));
        }
    };

    // Live in-flight block (rendered above finished blocks). v1.10.23: the
    // LiveLayoutCache's cumulative prefix sums locate the visible logical-line
    // window in O(log n); only visible ± overscan lines are sliced out.
    let scroll_px = block_scroll * pitch;
    // v1.20.0 (PLAN_v1200 §4 D6): flag ON widens the overscan by the uniform
    // per-block overlay chrome pixel height (gap 1×pitch + header band +
    // separator 1×pitch) so the window still covers blocks whose chrome sits
    // just outside the content bounds.
    let overscan = if complete_block_overlay {
        header_height + pitch * 2.0 + header_height + pitch * 2.0
    } else {
        header_height + pitch * 2.0
    };
    if let Some(live) = live {
        live_cache.sync(
            live.output,
            pane_session_id,
            live.version,
            cols,
            live.screen_origin,
            // M6-a: the capture's rewrite watermark — the append guard's
            // authoritative signal. Take + sync are 1:1; a real value here
            // and in block_scroll_metrics (either may be the frame's first
            // consumer) is mandatory, a dummy MAX would short-circuit the
            // guard at whichever call runs first.
            live.take_min_write_offset(),
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
            cursor_dist += chunks.len() as f32 * pitch;
            let style = if resolve_styles {
                live.styled_output.and_then(|styled| {
                    bump_styled(1);
                    styled.line(line_idx)
                })
            } else {
                None
            };
            // One entry per visual row; pushed dist = the line's top (the
            // legacy per-line value) and the paint adds `chunk_idx * pitch` —
            // byte-equal to the legacy single-entry-per-line rendering.
            let mut char_offset = 0usize;
            for (ci, chunk) in chunks.iter().enumerate() {
                rows.push(cursor_dist);
                row_data.push(LaidRow::Output {
                    line_text: line,
                    text: Cow::Owned(chunk.clone()),
                    block_id: None,
                    line: line_idx,
                    chunk_idx: ci,
                    char_offset,
                    style,
                });
                char_offset += chunk.chars().count();
            }
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
        // P1: the live header gains the running elapsed label. `now` is the
        // caller-injected frame clock, so the paint/hit-test double build of
        // one model yields identical bytes.
        let elapsed = live_elapsed_label(live.started_at, now);
        if let Some(text) = live_context_label(live.cwd.or(cwd), git_branch, Some(&elapsed)) {
            cursor_dist += pitch;
            rows.push(cursor_dist);
            row_data.push(LaidRow::LiveHeader { text });
        }
        cursor_dist += pitch;
        rows.push(cursor_dist);
        row_data.push(LaidRow::Separator);
    }

    // Finished blocks: binary-search the prefix sum for the visible range,
    // then assemble their rows. v1.19.2 (PLAN_v1192 §1.1b): the capture and
    // region tracks duplicated this loop byte-for-byte; it now lives in
    // completed_pass::assemble_completed_rows. Each track keeps its own live
    // segment (hand-rolled above / region_live_segment) — only the finished
    // blocks are unified.
    let (expanded_block_count, overlay_rows) = completed_pass::assemble_completed_rows(
        completed_pass::CompletedPassCtx {
            blocks,
            cache,
            block_diagnose_state,
            cols,
            viewport_rows,
            pitch,
            header_height,
            content_bottom_y,
            scroll_px,
            overscan,
            clip_top,
            clip_bottom,
            resolve_styles,
            styled_lookup_counter,
            cursor_dist,
            complete_block_overlay,
        },
        &mut rows,
        &mut row_data,
    );

    LayoutPassOutput {
        rows,
        row_data,
        expanded_block_count,
        overlay_rows,
    }
}

#[cfg(test)]
#[path = "layout_pass/gap_tests.rs"]
mod gap_tests;

// v1.19.2 (PLAN_v1192 §1.1a): the inline test module moved to
// layout_pass/pass_tests.rs (test files are exempt from the 800-line gate).
#[cfg(test)]
#[path = "layout_pass/pass_tests.rs"]
mod pass_tests;
