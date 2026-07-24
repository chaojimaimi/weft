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
    block_presentation, clear_block_spacer_rows, command_resume_hints, BlockTone,
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
    pub(super) block_scroll: usize,
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
/// references) but NOT from `cache` — all cache-derived data (chunks) is
/// cloned into `Rc<[String]>` before being stored. This lets the caller
/// drop the cache borrow immediately after this call returns.
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

    // Finished blocks (walked bottom-to-top).
    // Step 2: visibility culling. Compute scroll_px and clip bounds once,
    // then for each block decide whether to expand its internal lines.
    // Blocks fully offscreen only accumulate cursor_dist without pushing
    // rows, reducing layout pass from O(n*m) to O(n + k*m) where k =
    // visible block count. A 1-block overscan on each side covers partial
    // blocks and the sticky header.
    let scroll_px = (block_scroll as f32) * pitch;
    let overscan = header_height + pitch * 2.0;

    for b in blocks.iter().rev() {
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
        let block_total_height = (hint_rows + output_rows) as f32 * pitch
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
                let vis_rows = line.chunks.len();
                cursor_dist += vis_rows as f32 * pitch;
                rows.push(cursor_dist);
                row_data.push(LaidRow::Output {
                    text,
                    chunks: Rc::clone(&line.chunks),
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
            block_scroll: 0,
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
}
