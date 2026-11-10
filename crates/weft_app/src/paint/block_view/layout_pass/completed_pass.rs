//! v1.19.2 (PLAN_v1192 §1): the finished-blocks segment of the shared
//! layout pass. Both tracks carried byte-identical copies of this loop —
//! the capture track (`layout_pass.rs`, was lines 308-605) and the region
//! track (`region_pass.rs`, was lines 95-392) — and must stay in lockstep,
//! so one shared `assemble_completed_rows` now serves both. The live
//! segments intentionally stay separate (capture hand-walks
//! `LiveLayoutCache`, region calls `region_rows`).

use std::borrow::Cow;
use std::cell::Cell;
use std::rc::Rc;

use super::{
    bounds::{lower_bound_height, upper_bound_height},
    LaidRow,
};
use crate::block_component::{
    block_presentation, clear_block_spacer_rows, command_output_gap_rows, command_resume_hints,
};
use crate::paint::grid_cache::{
    block_line_chunks, char_offset_at, command_line_chunks, BlockLayoutCache,
};
use crate::paint::ui_helpers::strip_prompt_prefix;
use weft_core::blocks::{Block, BlockId};

/// Inputs of the finished-blocks assembly — review N1's authoritative 15:
/// exactly what both legacy loop copies read, no more and no less (the
/// live segment's `now`/`cwd`/`git_branch`/`pane_session_id`/`live_cache`/
/// `live`/`region`/`region_state` never enter the completed segment).
pub(crate) struct CompletedPassCtx<'a, 'b> {
    pub(crate) blocks: &'a [Block],
    pub(crate) cache: &'a BlockLayoutCache,
    /// Separate lifetime `'b` (mirrors `LayoutPassInput`): output rows do
    /// not borrow from the diagnose state.
    pub(crate) block_diagnose_state:
        &'b std::collections::HashMap<BlockId, crate::app_state::BlockDiagnoseState>,
    pub(crate) cols: usize,
    pub(crate) viewport_rows: usize,
    pub(crate) pitch: f32,
    pub(crate) header_height: f32,
    pub(crate) content_bottom_y: f32,
    /// `block_scroll * pitch` — both callers precompute it for the live segment.
    pub(crate) scroll_px: f32,
    /// `header_height + pitch * 2.0` — ditto.
    pub(crate) overscan: f32,
    pub(crate) clip_top: f32,
    pub(crate) clip_bottom: f32,
    /// Populate paint-only `style` fields; hit-testing passes `false`.
    pub(crate) resolve_styles: bool,
    /// `styled.line()` lookup counter (paint) or `None` (hit-testing).
    pub(crate) styled_lookup_counter: Option<&'a Cell<usize>>,
    /// Initial cursor distance = the live segment's output (`layout_pass.rs`
    /// :313 / `region_pass.rs`:102). The value after the finished-blocks walk
    /// is dead on both tracks (only three fields return) — not propagated.
    pub(crate) cursor_dist: f32,
}

/// Assemble the finished-blocks segment: binary-search the prefix sum for
/// the visible range, then walk it newest-to-oldest emitting resume-hint /
/// output / diagnose / command / header / separator / clear rows.
/// Extraction-only (PLAN_v1192 §1): the loop body is the capture track's
/// legacy loop verbatim — zero behavior change.
///
/// Contract (carried from `compute_block_layout_pass`): the caller must
/// `ensure_cached` every block. A missing/stale prefix sum is tolerated —
/// the visible-window search falls back to a full walk.
///
/// Appends to `rows`/`row_data` and returns the expanded-block count.
pub(crate) fn assemble_completed_rows<'a, 'b>(
    ctx: CompletedPassCtx<'a, 'b>,
    rows: &mut Vec<f32>,
    row_data: &mut Vec<LaidRow<'a>>,
) -> usize {
    let CompletedPassCtx {
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
        cursor_dist: live_cursor_dist,
    } = ctx;
    let mut cursor_dist = live_cursor_dist;
    let mut expanded_block_count = 0usize;
    let bump_styled = |n: usize| {
        if let Some(c) = styled_lookup_counter {
            c.set(c.get().saturating_add(n));
        }
    };

    // Finished blocks: binary-search the prefix sum for the visible range
    // (O(log n + k*m)) instead of O(n) traversal, +1 overscan per side.

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

        // Block height without expanding lines; L2 tables are O(1) reads.
        // M5-b: hint/output rows read straight off the L2 width table — the
        // legacy per-frame hint re-wrap is gone (same shared machine, same
        // numbers, pinned by the visual_rows tests).
        let hint_rows = if b.collapsed {
            0
        } else {
            cached.width.hint_rows as usize
        };
        let output_rows = if b.collapsed {
            0
        } else {
            cached.width.rows.len()
        };
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
            // Resume hints: two static strings, wrapped on demand (not worth
            // an L2 entry). One entry per visual row, pushed dist = hint top.
            for hint in command_resume_hints(b).iter().rev() {
                let chunks: Rc<[String]> =
                    Rc::from(block_line_chunks(hint, cols).collect::<Vec<_>>());
                cursor_dist += chunks.len() as f32 * pitch;
                let mut char_offset = 0usize;
                for (ci, chunk) in chunks.iter().enumerate() {
                    rows.push(cursor_dist);
                    row_data.push(LaidRow::Output {
                        line_text: hint,
                        text: Cow::Owned(chunk.clone()),
                        block_id: Some(b.id),
                        line: usize::MAX,
                        chunk_idx: ci,
                        char_offset,
                        style: None,
                    });
                    char_offset += chunk.chars().count();
                }
            }
            // Output lines (M5-b R-a): binary-search the L2 window per block
            // and emit one row per VISIBLE visual row — zero-copy slices of
            // the block's immutable output, no per-line Rc<[String]>.
            // Skipped lines still advance cursor_dist by their exact legacy
            // per-line `+= rows * pitch` increments, so every pushed dist
            // (a line's top) is byte-identical to the legacy pass.
            let table = &cached.content;
            let width = &cached.width;
            let total_rows = width.rows.len();
            if total_rows > 0 {
                // Row r (0-based from the lines-region top) spans dist
                // [region_top - (r+1)*pitch, region_top - r*pitch]; its top
                // band must intersect [clip_top, clip_bottom] (the paint
                // loop's own keep test — block-level overscan already
                // bounded `is_visible`).
                let region_top = cursor_dist + total_rows as f32 * pitch;
                let row_top_y =
                    |r: usize| content_bottom_y + scroll_px - (region_top - r as f32 * pitch);
                let first_raw = ((clip_top - row_top_y(0)) / pitch).floor() as isize;
                let last_raw = ((clip_bottom - row_top_y(0)) / pitch).ceil() as isize;
                if first_raw <= last_raw {
                    // One row of slack for f32 wobble; the paint loop /
                    // bv_rows keep-test re-filters exactly.
                    let first_row = (first_raw.max(0) as usize)
                        .saturating_sub(1)
                        .min(total_rows - 1);
                    let last_row = ((last_raw.max(0) as usize).saturating_add(1))
                        .min(total_rows - 1)
                        .max(first_row);
                    let base = &width.line_row_base;
                    // Line containing a row index (line_row_base is strictly
                    // increasing — every line owns ≥ 1 row).
                    let line_of = |r: usize| {
                        base.partition_point(|&b| (b as usize) <= r)
                            .saturating_sub(1)
                    };
                    let l_first = line_of(first_row);
                    let l_last = line_of(last_row);
                    let n_lines = table.line_meta.len();
                    // Fast-forward: lines BELOW the window (bottom-up walk).
                    for l in ((l_last + 1)..n_lines).rev() {
                        cursor_dist += (base[l + 1] - base[l]) as f32 * pitch;
                    }
                    for l in (l_first..=l_last).rev() {
                        // The line's own legacy increment → cursor now sits
                        // at the line's TOP dist (the legacy pushed value).
                        cursor_dist += (base[l + 1] - base[l]) as f32 * pitch;
                        let line_range = table.line_range(l);
                        let line_text = &b.output[line_range];
                        let lo = (first_row as u32).max(base[l]);
                        let hi = (last_row as u32 + 1).min(base[l + 1]);
                        let style = if resolve_styles {
                            b.styled_output.as_deref().and_then(|styled| {
                                bump_styled(1);
                                styled.line(l)
                            })
                        } else {
                            None
                        };
                        // Top-down within the line: the legacy paint path
                        // emitted chunk 0..n vertices in that order, and the
                        // vertex goldens lock the buffer sequence.
                        for r in lo..hi {
                            let row = &width.rows[r as usize];
                            let chunk_idx = (r - base[l]) as usize;
                            rows.push(cursor_dist);
                            row_data.push(LaidRow::Output {
                                line_text,
                                // A single-row line paints its FULL text
                                // (legacy single-chunk path behavior —
                                // screen-origin rows rely on max_cols clip).
                                text: if base[l + 1] - base[l] == 1 {
                                    Cow::Borrowed(line_text)
                                } else {
                                    Cow::Borrowed(table.row_text(line_text, row))
                                },
                                block_id: Some(b.id),
                                line: l,
                                chunk_idx,
                                char_offset: char_offset_at(line_text, table.row_byte_start(row)),
                                style,
                            });
                        }
                    }
                    // Fast-forward: lines ABOVE the window.
                    for l in (0..l_first).rev() {
                        cursor_dist += (base[l + 1] - base[l]) as f32 * pitch;
                    }
                } else {
                    // Nothing visible: advance by the whole lines region.
                    cursor_dist += total_rows as f32 * pitch;
                }
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
        // M5-b: surviving source-line count reads L1 (no legacy lines vec).
        let presentation = block_presentation(b, cached.content.line_meta.len());
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

    expanded_block_count
}
