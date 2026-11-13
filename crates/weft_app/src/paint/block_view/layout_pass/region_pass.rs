//! v1.19.0 (PLAN_v1190 §4 P1-1): the region-track layout pass.
//!
//! `compute_region_layout_pass` is the flag-ON twin of
//! `compute_block_layout_pass`'s live branch: the live segment is assembled
//! from the grid-region snapshot (`region_rows::region_live_segment`) while
//! the finished-blocks rows come from the shared
//! `completed_pass::assemble_completed_rows` (v1.19.2, PLAN_v1192 §1.1b —
//! the formerly duplicated capture/region loop). The branch-isolation
//! tests in `layout_pass/region_rows_tests.rs` pin the shared shapes.

use super::{
    completed_pass::{assemble_completed_rows, CompletedPassCtx},
    region_rows, LaidRow, LayoutPassInput, LayoutPassOutput,
};
use crate::paint::grid_cache::BlockLayoutCache;

/// Region-track dispatch target (see the guard in `compute_block_layout_pass`):
/// identical geometry contract, region-snapshot live segment. The capture
/// track's `LiveLayoutCache` is deliberately NOT synced here — under the
/// flag the captured output is not the rendered document.
pub(super) fn compute_region_layout_pass<'a, 'b>(
    input: &LayoutPassInput<'a, 'b, '_>,
    cache: &'a BlockLayoutCache,
) -> LayoutPassOutput<'a> {
    // v1.20.7 (PLAN_v1207 §3 D3): the pseudo-block frame routes here with
    // live=None (no in-flight block between submit and the shell's 133;B
    // marker) — the live segment then renders region rows only, without the
    // LiveCommand/LiveHeader chrome.
    let live = input.live.as_ref();
    let Some(region_state) = input.region_state else {
        unreachable!("region track requires a snapshot");
    };

    let blocks = input.blocks;
    let cwd = input.cwd;
    let git_branch = input.git_branch;
    let viewport_rows = input.viewport_rows;
    let cols = input.cols;
    let pitch = input.pitch;
    let header_height = input.header_height;
    let content_bottom_y = input.content_bottom_y;
    let clip_top = input.clip_top;
    let clip_bottom = input.clip_bottom;
    let resolve_styles = input.resolve_styles;
    let block_diagnose_state = input.block_diagnose_state;
    let now = input.now;
    let counter = input.styled_lookup_counter;
    let mut bump_styled = |n: usize| {
        if let Some(c) = counter {
            c.set(c.get().saturating_add(n));
        }
    };

    // Live in-flight block (rendered above finished blocks): assembled from
    // the grid-region snapshot (P1-2) — wrapped rows merged by the core,
    // P1-6 blank-trimmed, chunk prefix sums from the snapshot's immutable
    // prefix plus the per-frame viewport tail (D8).
    let scroll_px = input.block_scroll * pitch;
    // v1.20.0 (PLAN_v1200 §4 D6): same flag-gated overscan growth as the
    // capture track (uniform per-block overlay chrome px).
    let overscan = if input.complete_block_overlay {
        header_height + pitch * 2.0 + header_height + pitch * 2.0
    } else {
        header_height + pitch * 2.0
    };
    let segment = region_rows::region_live_segment(
        live,
        region_state,
        cols,
        pitch,
        header_height,
        content_bottom_y,
        scroll_px,
        clip_top,
        clip_bottom,
        resolve_styles,
        &mut bump_styled,
        now,
        cwd,
        git_branch,
    );
    let mut rows: Vec<f32> = segment.rows;
    let mut row_data: Vec<LaidRow> = segment.row_data;
    let cursor_dist = segment.cursor_dist;
    // Segment totals are consumed by block_scroll_metrics through its own
    // snapshot read (P1-8), not through the pass output.
    let _ = (segment.total_display_rows, segment.total_logical_rows);

    // v1.19.2 (PLAN_v1192 §1.1b): the finished-blocks loop — byte-identical
    // to the capture track's — now lives in the shared
    // `assemble_completed_rows`; this track contributes only its live
    // segment (above, from the grid-region snapshot).
    let (expanded_block_count, overlay_rows) = assemble_completed_rows(
        CompletedPassCtx {
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
            styled_lookup_counter: counter,
            cursor_dist,
            complete_block_overlay: input.complete_block_overlay,
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
#[path = "region_rows_tests.rs"]
mod region_rows_tests;
