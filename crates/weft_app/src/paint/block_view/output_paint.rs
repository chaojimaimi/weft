//! v1.11.6 (PLAN_v1116 M4 / D-j): Output-row paint bodies extracted from
//! `build_block_view_vertices` — single-chunk and multi-chunk rows. Pure
//! structural move: the M3 vertex goldens keep this byte-equal.
//!
//! M5-b (PLAN_M5 §三 R-a): the layout pass emits ONE `LaidRow::Output` per
//! visible visual row with zero-copy text slices, so the legacy single/multi
//! chunk dispatch collapses into [`paint_output_row`] — `chunk_idx` /
//! `char_offset` are 0 and `text` spans the line for whole-line rows, which
//! reduces every expression below to its legacy single-path form exactly.

use super::layout_pass::LaidRow;
use crate::paint::block_view::find;
use crate::paint::block_view::row_paint::RowPaintCtx;
use crate::paint::block_view::style;
use crate::paint::block_view::surfaces::canvas_for;
use crate::paint::primitives::{composite_color_over, push_quad};
use weft_core::blocks::BlockId;

/// v1.10.26 (FIX_IME_PREEDIT): B-path diagnostic — record the visible
/// live-row span so the PREEDIT_DIAG log can show whether the expected
/// caret line is even inside the painted rows.
fn track_preedit_diag_span(
    ctx: &mut RowPaintCtx<'_>,
    block_id: Option<BlockId>,
    key_line: Option<usize>,
) {
    if ctx.preedit_track.is_some() && block_id.is_none() {
        if let Some(l) = key_line {
            *ctx.first_live_line = Some(ctx.first_live_line.map_or(l, |f: usize| f.min(l)));
            *ctx.last_live_line = Some(ctx.last_live_line.map_or(l, |f: usize| f.max(l)));
        }
    }
}

/// T16c: caret x for the whole-line path. col == cols (满行 wrap_pending,
/// 逻辑位置在右缘外一格)钳到网格右缘内侧 2px —— caret 贴在末字符右缘
/// 而非压字(col==cols-1)或画出可视区。2.0 与 ime.rs block_view_tui_
/// caret_geometry 的 [x, x+2.0] caret 宽度保持同步(注释级约定)。
fn caret_x_for(whole_line: bool, chunk_col: usize, left: f32, cw: f32, cols: usize) -> f32 {
    if whole_line && chunk_col >= cols {
        left + cols as f32 * cw - 2.0
    } else {
        left + chunk_col as f32 * cw
    }
}

/// Paint ONE visual output row. `text` is the row's own text; `line_text`
/// is the full source line (semantic text + find range base). Whole-line
/// rows (`text` spans `line_text`) keep the legacy single-chunk semantics:
/// the raw selection range, `chunk_idx` 0 / `char_offset` 0, and the
/// renderer's `max_cols` clip on the full-line text (screen-origin rows
/// rely on that clip).
pub(super) fn paint_output_row(ctx: &mut RowPaintCtx<'_>, laid: &LaidRow<'_>, y: f32) {
    let LaidRow::Output {
        line_text,
        text,
        block_id,
        line,
        chunk_idx,
        char_offset,
        style,
    } = laid
    else {
        unreachable!("paint_output_row takes a LaidRow::Output row")
    };
    let line = *line;
    let chunk_idx = *chunk_idx;
    let char_offset = *char_offset;
    let block_id = *block_id;
    let style = *style;
    let renderer = ctx.renderer;
    let canvas = canvas_for(ctx.block_canvases, block_id, ctx.theme_bg);
    let selection_canvas = composite_color_over(ctx.selection_bg, canvas);
    // v1.10.26: content-coordinate key of this row (resume
    // hints carry line == usize::MAX → not addressable).
    let key_line = (line != usize::MAX).then_some(line);
    let key_block = block_id.map(|b| b.0);
    // v1.11.6 (rust-reviewer P1-1): both legacy paths ran this update (the
    // single path before the dispatch; the M4 split had briefly dropped it
    // from multi-chunk rows) — keep it unconditional per row.
    track_preedit_diag_span(ctx, block_id, key_line);
    let line_slice = ctx.char_range_for(key_block, key_line);
    let whole_line = text.len() == line_text.len();
    // Legacy single path used the raw (whole-line) range directly; the multi
    // path projected it into the chunk and skipped empty overlaps.
    let selection_range = if whole_line {
        line_slice
    } else {
        line_slice
            .map(|(cs, ce)| {
                let chunk_len = text.chars().count();
                (
                    cs.saturating_sub(char_offset).min(chunk_len),
                    ce.saturating_sub(char_offset).min(chunk_len),
                )
            })
            .filter(|(cs, ce)| ce > cs)
    };
    if let Some((cs, ce)) = selection_range {
        renderer.push_block_view_highlight(
            &mut *ctx.verts,
            ctx.left,
            y,
            ctx.ch,
            text,
            cs,
            ce,
            ctx.selection_bg,
            ctx.bg_uv,
        );
    }
    if let Some(bh) = ctx.find_block_highlight {
        if bh.0 == block_id.map(|b| b.0).unwrap_or(0) && bh.1 == line && !bh.2 {
            find::push_find_highlight(
                &mut *ctx.verts,
                ctx.find_canvas,
                line_text,
                (bh.3, bh.4),
                ctx.cols,
                chunk_idx,
                [ctx.left, y],
            );
        }
    }
    let t0 = std::time::Instant::now();
    // v1.10.5: live rows (block_id == None) read styles
    // from the in-flight block's styled_output.
    let (source, styled) = if block_id.is_none() {
        (None, ctx.live_styled.clone())
    } else {
        style::block_arc_identity(ctx.blocks, block_id)
    };
    let source = (line != usize::MAX).then_some(source).flatten();
    renderer.push_block_output_text_cached(
        &mut *ctx.verts,
        style::BlockOutputTextPaint {
            x: ctx.left,
            y,
            text,
            semantic_text: line_text,
            style,
            char_offset,
            fallback: ctx.output_fg,
            canvas,
            // The styled text selection uses FULL-LINE coords
            // (line_slice is already them) — both legacy paths.
            selection: line_slice.map(|(start, end)| (start, end, selection_canvas)),
            selection_painted: ctx.selection_painted,
            max_cols: ctx.cols,
            palette: ctx.palette,
            row_pitch: ctx.pitch,
        },
        style::CacheKeyInput {
            pane_session_id: ctx.cache_namespace,
            block_id: block_id.map(|b| b.0).unwrap_or(0),
            line_idx: line,
            chunk_idx,
            render_generation: ctx.render_generation,
            palette_fingerprint: ctx.palette_fp,
            source,
            styled,
        },
        &renderer.styled_line_cache,
    );
    renderer
        .styled_paint_us_counter
        .set(renderer.styled_paint_us_counter.get() + t0.elapsed().as_micros() as u64);
    // v1.10.5: grid cursor is invisible in document mode,
    // so TUI caret/preedit paint only on live rows.
    if let Some((cursor_line, cursor_col)) = ctx.tui_cursor {
        // Legacy single path matched the row unconditionally; the wrapped
        // (multi-chunk) path only on the cursor's own chunk.
        if crate::block_component::tui_caret_row_matches(block_id.map(|b| b.0), line, cursor_line)
            && (whole_line || chunk_idx == cursor_col / ctx.cols)
        {
            let chunk_col = if whole_line {
                cursor_col
            } else {
                cursor_col % ctx.cols
            };
            let caret_x = caret_x_for(whole_line, chunk_col, ctx.left, ctx.cw, ctx.cols);
            let (caret_area, caret_quad) =
                crate::ime::block_view_tui_caret_geometry(caret_x, y, ctx.cw, ctx.ch);
            renderer.block_view_tui_caret_area.set(Some(caret_area));
            // Steady-on: blink timer only wakes for AtPrompt, so cursor_blink_on is stale.
            push_quad(&mut *ctx.verts, caret_quad, ctx.bg_uv, [0.0; 4], ctx.accent);
            if let Some((preedit, preedit_cursor)) = ctx.tui_preedit {
                // v1.10.26 (FIX_IME_PREEDIT): the caret
                // matched — the preedit was drawn on a
                // live row (path = "B").
                *ctx.caret_painted = true;
                // T16c: at the col == cols edge the caret parks 2px inside the
                // right edge, but the preedit anchor keeps the legacy cols-1
                // clamp — pixel-identical at the boundary, identity below it.
                let preedit_x = caret_x.min(ctx.left + ctx.cols.saturating_sub(1) as f32 * ctx.cw);
                renderer.push_block_tui_preedit(
                    &mut *ctx.verts,
                    crate::paint::preedit::BlockTuiPreeditParams {
                        text: preedit,
                        cursor: preedit_cursor,
                        x: preedit_x,
                        y,
                        right: ctx.right,
                        cols: ctx.cols,
                        cursor_col: chunk_col.min(ctx.cols.saturating_sub(1)),
                        bg_uv: ctx.bg_uv,
                        theme_bg: ctx.theme_bg,
                        accent: ctx.accent,
                    },
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::caret_x_for;

    #[test]
    fn caret_x_regular_column_is_unchanged() {
        // whole_line path, col < cols: plain left + col * cw.
        assert_eq!(caret_x_for(true, 5, 10.0, 8.0, 80), 50.0);
    }

    #[test]
    fn caret_x_full_line_parks_inside_right_edge() {
        // col == cols: the 2px caret hugs the grid's right edge from inside.
        assert_eq!(caret_x_for(true, 80, 10.0, 8.0, 80), 648.0);
    }

    #[test]
    fn caret_x_beyond_cols_is_defensively_clamped() {
        assert_eq!(caret_x_for(true, 85, 10.0, 8.0, 80), 648.0);
    }

    #[test]
    fn caret_x_multi_chunk_path_never_parks() {
        // !whole_line: the modulo mapping keeps chunk_col <= cols-1 upstream.
        assert_eq!(caret_x_for(false, 85, 10.0, 8.0, 80), 690.0);
    }

    #[test]
    fn caret_x_edge_is_half_a_cell_right_of_last_cell_left() {
        // Semantic lock: the parked caret sits at the last glyph's RIGHT edge
        // (x + 2.0 == grid right edge), not at the last cell's left edge:
        // edge_x - last_cell_x == cw - 2.0.
        let (left, cw, cols) = (10.0, 8.0, 80);
        let edge = caret_x_for(true, cols, left, cw, cols);
        let last_cell = left + (cols - 1) as f32 * cw;
        assert_eq!(edge - last_cell, cw - 2.0);
    }
}
