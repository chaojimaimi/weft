//! Shared layout pass for BlockView: walk blocks bottom-to-top, accumulate
//! `cursor_dist`, push `LaidRow` rows (paint + hit-testing both consume this).

use std::borrow::Cow;
use std::cell::Cell;
use std::rc::Rc;

use crate::block_component::{
    block_presentation, clear_block_spacer_rows, command_output_gap_rows, command_resume_hints,
    BlockTone,
};
use crate::paint::grid_cache::{
    block_line_chunks, char_offset_at, command_line_chunks, screen_origin_line_chunks,
    BlockLayoutCache,
};

#[cfg(test)]
#[path = "layout_pass/bench.rs"]
mod bench;
mod bounds;
#[cfg(test)]
#[path = "layout_pass/window_tests.rs"]
mod window_tests;
use crate::paint::live_cache::LiveLayoutCache;
use crate::paint::ui_helpers::strip_prompt_prefix;
use bounds::{lower_bound_height, upper_bound_height};
use weft_core::blocks::{Block, BlockId, InFlightBlock, StyledLine};

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
/// M5-b (PLAN_M5 §三 R-a): output rows are emitted O(visible rows) — the
/// L2 `line_row_base` is binary-searched per block and only visible visual
/// rows slice the block's immutable `output` (zero-copy). The output DOES
/// borrow from `cache` now (row text derives from `CachedBlockLayout`),
/// so the cache borrow must outlive the laid rows. Live rows materialize
/// per-chunk strings as before (`LiveLayoutCache` is out of M5 scope).
pub(super) fn compute_block_layout_pass<'a, 'b>(
    input: LayoutPassInput<'a, 'b>,
    cache: &'a BlockLayoutCache,
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

    LayoutPassOutput {
        rows,
        row_data,
        expanded_block_count,
    }
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
                min_write_offset: InFlightBlock::detached_watermark(),
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
        let cache = BlockLayoutCache::default();
        let out = compute_block_layout_pass(input, &cache, &mut LiveLayoutCache::default());
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
}
