//! v1.19.0 (PLAN_v1190 §4 P1-2/P1-5/P1-6/N7): region-track live segment
//! assembly. Consumes the [`PaneRegionState`] snapshot (`paint/region_cache`)
//! and emits the exact same row shapes the capture-track live branch pushes
//! (`LaidRow::Output` per chunk + Blank/LiveCommand/LiveHeader/Separator), so
//! the downstream paint/hit-test pipeline is untouched. Deviations from the
//! capture branch are pinned by this module's tests:
//! - the row source is the grid-region snapshot (wrapped rows merged by the
//!   core, N7 styles carried on the snapshot's `RegionSlot`);
//! - leading/trailing blank logical rows are dropped at assembly (P1-6);
//! - screen-origin chunking does not apply (region rows are document rows,
//!   always soft-wrapped like ordinary shell output).

use std::borrow::Cow;
use std::rc::Rc;
use std::time::SystemTime;

use weft_core::blocks::InFlightBlock;

use super::LaidRow;
use crate::paint::grid_cache::{block_line_chunks, command_line_chunks};
use crate::paint::live_cache::count_line_chunks;
use crate::paint::region_cache::{region_display_range, PaneRegionState};
use crate::paint::ui_helpers::{live_context_label, live_elapsed_label, strip_prompt_prefix};

/// One live segment's layout output — the same tuple the capture branch
/// accumulates in `compute_block_layout_pass`'s locals.
pub(super) struct RegionLiveSegment<'a> {
    pub(super) rows: Vec<f32>,
    pub(super) row_data: Vec<LaidRow<'a>>,
    /// Distance-from-content-bottom after the segment's Separator (the
    /// finished-blocks loop's `live_cursor_dist`).
    pub(super) cursor_dist: f32,
    /// P1-6-dropped display-row total (chunk rows) — Blank height + metrics.
    pub(super) total_display_rows: usize,
    pub(super) total_logical_rows: usize,
}

/// Assemble the region-track live segment: visible-window culling over the
/// snapshot's chunk prefix sums, then the capture-branch row shapes.
#[allow(clippy::too_many_arguments)]
pub(super) fn region_live_segment<'a>(
    live: &InFlightBlock<'a>,
    state: &'a PaneRegionState,
    cols: usize,
    pitch: f32,
    header_height: f32,
    content_bottom_y: f32,
    scroll_px: f32,
    clip_top: f32,
    clip_bottom: f32,
    resolve_styles: bool,
    bump_styled: &mut dyn FnMut(usize),
    now: SystemTime,
    cwd: Option<&str>,
    git_branch: Option<&str>,
) -> RegionLiveSegment<'a> {
    let n_slots = state.slots.len();
    let total_slots = n_slots + state.dyn_slots.len();
    // Joint slot view: immutable prefix then per-frame tail (D8 tiles the
    // region seamlessly — the anchor only sits on logical-row boundaries).
    let slot_at = |i: usize| -> &'a crate::paint::region_cache::RegionSlot {
        if i < n_slots {
            &state.slots[i]
        } else {
            &state.dyn_slots[i - n_slots]
        }
    };
    // P1-6: leading/trailing blank logical rows never reach the layout.
    let (first, last) = region_display_range(total_slots, |i| slot_at(i).text.is_empty());

    // Chunk prefix sums: the immutable prefix re-uses the snapshot's table
    // (cols-keyed, rebuilt on cols changes), the viewport tail is counted
    // per frame (D8). Rebased onto the dropped prefix so the window math
    // runs in the display domain.
    let mut cums: Vec<u32> = Vec::with_capacity(total_slots + 1);
    if state.cumulative.len() == n_slots + 1 {
        cums.extend_from_slice(&state.cumulative);
    } else {
        // Defensive: the invariant was broken by a future edit — re-chunk.
        cums.push(0);
        for i in 0..n_slots {
            let last = *cums.last().unwrap();
            cums.push(last + count_line_chunks(&slot_at(i).text, cols));
        }
    }
    for i in n_slots..total_slots {
        let last = *cums.last().unwrap();
        cums.push(last + count_line_chunks(&slot_at(i).text, cols));
    }
    let base = cums[first];
    let cums: Vec<u32> = cums[first..=last].iter().map(|c| c - base).collect();
    let total_display = *cums.last().unwrap() as usize;
    let total_logical = last - first;

    let mut rows: Vec<f32> = Vec::new();
    let mut row_data: Vec<LaidRow> = Vec::new();

    if total_logical > 0 {
        let overscan = header_height + pitch * 2.0;
        let threshold_low = content_bottom_y + scroll_px - clip_bottom - overscan;
        let threshold_high = content_bottom_y + scroll_px - clip_top + overscan;
        let (start, end, start_dist) =
            region_visible_range(&cums, pitch, threshold_low, threshold_high, overscan);
        let mut cursor_dist = start_dist;
        // Chunk window -> logical-row window: row k overlaps [start, end)
        // iff its chunk span [cums[k], cums[k+1]) does. Emit each row ONCE
        // with ALL of its chunks — the capture branch's per-line loop
        // verbatim (layout_pass.rs:233-275). Emitting per display row
        // instead re-ran the owning row's full chunk run once per covered
        // window slot (F3: a 3-chunk line emitted 9 rows; caught by
        // wrapped_logical_row_chunks_into_visual_rows).
        let start_k = cums
            .partition_point(|&c| c <= start as u32)
            .saturating_sub(1);
        let end_k = cums.partition_point(|&c| c < end as u32);
        for k in (start_k..end_k).rev() {
            let slot = slot_at(first + k);
            let chunks: Vec<String> = block_line_chunks(&slot.text, cols).collect();
            cursor_dist += chunks.len() as f32 * pitch;
            // N7 style adapter: `RegionSlot.style` (a `StyledLine`) borrows
            // straight into `LaidRow::Output::style`.
            let style = if resolve_styles {
                bump_styled(1);
                Some(&slot.style)
            } else {
                None
            };
            // One entry per visual row; pushed dist = the line's top (the
            // legacy per-line value) and the paint adds `chunk_idx * pitch`.
            let mut char_offset = 0usize;
            for (ci, chunk) in chunks.iter().enumerate() {
                rows.push(cursor_dist);
                row_data.push(LaidRow::Output {
                    line_text: &slot.text,
                    text: Cow::Owned(chunk.clone()),
                    block_id: None,
                    line: k,
                    chunk_idx: ci,
                    char_offset,
                    style,
                });
                char_offset += chunk.chars().count();
            }
        }
        // Blank sits above the output at the FULL layout height (capture
        // parity: `(total_display_rows + 1) * pitch`).
        let blank_dist = (total_display + 1) as f32 * pitch;
        rows.push(blank_dist);
        row_data.push(LaidRow::Blank);
    }

    // Structure rows — semantics-equal to the capture branch (:261-283).
    let live_cmd_chunks: Rc<[String]> = Rc::from(command_line_chunks(
        &strip_prompt_prefix(live.command),
        cols.saturating_sub(2).max(1),
        cols,
    ));
    let mut cursor_dist = if total_logical > 0 {
        (total_display + 1) as f32 * pitch
    } else {
        0.0
    };
    cursor_dist += live_cmd_chunks.len() as f32 * pitch;
    rows.push(cursor_dist);
    row_data.push(LaidRow::LiveCommand {
        command: live.command,
        chunks: live_cmd_chunks,
    });
    // `now` is the caller-injected frame clock so paint/hit-test double
    // builds of one model yield identical bytes (capture-branch comment).
    let elapsed = live_elapsed_label(live.started_at, now);
    if let Some(text) = live_context_label(live.cwd.or(cwd), git_branch, Some(&elapsed)) {
        cursor_dist += pitch;
        rows.push(cursor_dist);
        row_data.push(LaidRow::LiveHeader { text });
    }
    cursor_dist += pitch;
    rows.push(cursor_dist);
    row_data.push(LaidRow::Separator);

    RegionLiveSegment {
        rows,
        row_data,
        cursor_dist,
        total_display_rows: total_display,
        total_logical_rows: total_logical,
    }
}

/// Region-track analogue of `LiveLayoutCache::visible_range`: reduce the
/// display-domain chunk prefix sums to the visible CHUNK window
/// (`± overscan`), plus the distance-from-bottom of the first emitted
/// (newest visible) line's top. Chunk `c` sits at `(total - c) * pitch`
/// from the content bottom — the newest chunk is lowest, thresholds are
/// the viewport's bottom/top edges. Both bounds are CHUNK indices, not
/// logical rows: one logical row can span several chunks, so the caller
/// first widens the window back out to whole rows (first row overlapping
/// `start` .. first row past `end`) and then emits each row once with all
/// of its chunks — capture-branch parity (layout_pass.rs:233-275).
pub(super) fn region_visible_range(
    cums: &[u32],
    pitch: f32,
    threshold_low: f32,
    threshold_high: f32,
    overscan_px: f32,
) -> (usize, usize, f32) {
    let overscan_lines = (overscan_px / pitch).ceil() as usize + 1;
    let total = match cums.last() {
        Some(&t) if t > 0 => t as usize,
        _ => return (0, 0, 0.0),
    };
    let top_rows = total as f32 - threshold_high / pitch;
    let bottom_rows = total as f32 - threshold_low / pitch;
    let newest_visible = (bottom_rows.floor().max(0.0) as usize).min(total);
    let oldest_visible = (top_rows.ceil().max(0.0) as usize).min(total);
    let start = oldest_visible.saturating_sub(overscan_lines);
    let end = (newest_visible + 1 + overscan_lines).min(total);
    // Anchor at the newest EMITTED row's full bottom (smallest prefix
    // value >= end): the caller emits whole logical rows, so the distance
    // must land on a row boundary, not mid-row.
    let at_or_after_end = cums
        .partition_point(|&c| (c as usize) < end)
        .min(cums.len().saturating_sub(1));
    let dist_at_end = total as f32 - cums[at_or_after_end] as f32;
    (start, end, dist_at_end * pitch)
}
