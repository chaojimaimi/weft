//! v1.11.6 (PLAN_v1116 M4 / D-j): Output-row paint bodies extracted from
//! `build_block_view_vertices` — single-chunk and multi-chunk rows. Pure
//! structural move: the M3 vertex goldens keep this byte-equal.

use std::rc::Rc;

use crate::paint::block_view::find;
use crate::paint::block_view::row_paint::RowPaintCtx;
use crate::paint::block_view::style;
use crate::paint::block_view::surfaces::canvas_for;
use crate::paint::primitives::{composite_color_over, push_quad};
use weft_core::blocks::{BlockId, StyledLine};

/// v1.10.26 (FIX_IME_PREEDIT): B-path diagnostic — record the visible
/// live-row span so the PREEDIT_DIAG log can show whether the expected
/// caret line is even inside the painted rows. Runs for BOTH chunk paths
/// (v1.11.6 P1-1: the HEAD version applied it before the chunk dispatch;
/// the M4 split had lost it from multi-chunk rows).
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

/// v1.10.26: `LaidRow::Output` with a single chunk — selection highlight,
/// optional find highlight, cached styled text, TUI caret/preedit on live
/// rows (block_id == None).
pub(super) fn paint_output_single_chunk(
    ctx: &mut RowPaintCtx<'_>,
    text: &str,
    block_id: Option<BlockId>,
    line: usize,
    style_opt: Option<&StyledLine>,
    y: f32,
) {
    let renderer = ctx.renderer;
    let canvas = canvas_for(ctx.block_canvases, block_id, ctx.theme_bg);
    let selection_canvas = composite_color_over(ctx.selection_bg, canvas);
    // v1.10.26: content-coordinate key of this row (resume
    // hints carry line == usize::MAX → not addressable).
    let key_line = (line != usize::MAX).then_some(line);
    let key_block = block_id.map(|b| b.0);
    track_preedit_diag_span(ctx, block_id, key_line);
    let selection_range = ctx.char_range_for(key_block, key_line);
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
                text,
                (bh.3, bh.4),
                ctx.cols,
                0,
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
            semantic_text: text,
            style: style_opt,
            char_offset: 0,
            fallback: ctx.output_fg,
            canvas,
            selection: selection_range.map(|(start, end)| (start, end, selection_canvas)),
            selection_painted: ctx.selection_painted,
            max_cols: ctx.cols,
            palette: ctx.palette,
            row_pitch: ctx.pitch,
        },
        style::CacheKeyInput {
            pane_session_id: ctx.cache_namespace,
            block_id: block_id.map(|b| b.0).unwrap_or(0),
            line_idx: line,
            chunk_idx: 0,
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
        if crate::block_component::tui_caret_row_matches(block_id.map(|b| b.0), line, cursor_line) {
            let caret_x = ctx.left + cursor_col as f32 * ctx.cw;
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
                renderer.push_block_tui_preedit(
                    &mut *ctx.verts,
                    crate::paint::preedit::BlockTuiPreeditParams {
                        text: preedit,
                        cursor: preedit_cursor,
                        x: caret_x,
                        y,
                        right: ctx.right,
                        cols: ctx.cols,
                        cursor_col,
                        bg_uv: ctx.bg_uv,
                        theme_bg: ctx.theme_bg,
                        accent: ctx.accent,
                    },
                );
            }
        }
    }
}

/// v1.10.26: `LaidRow::Output` with multiple soft-wrap chunks — the
/// selection range is per SOURCE line; each chunk covers a slice of it, so
/// the range is projected through the chunk's `char_offset` and clamped to
/// the chunk length.
pub(super) fn paint_output_multi_chunk(
    ctx: &mut RowPaintCtx<'_>,
    text: &str,
    chunks: &Rc<[String]>,
    block_id: Option<BlockId>,
    line: usize,
    style_opt: Option<&StyledLine>,
    y: f32,
) {
    let renderer = ctx.renderer;
    let canvas = canvas_for(ctx.block_canvases, block_id, ctx.theme_bg);
    let selection_canvas = composite_color_over(ctx.selection_bg, canvas);
    let key_line = (line != usize::MAX).then_some(line);
    let key_block = block_id.map(|b| b.0);
    // v1.11.6 (rust-reviewer P1-1): the HEAD version ran this update before
    // the chunk-count dispatch, so multi-chunk (soft-wrapped) live rows fed
    // PREEDIT_DIAG too — the M4 split had dropped it from this path.
    track_preedit_diag_span(ctx, block_id, key_line);
    let line_slice = ctx.char_range_for(key_block, key_line);
    let mut char_offset = 0;
    for (ci, chunk) in chunks.iter().enumerate() {
        let cy = y + ci as f32 * ctx.pitch;
        if cy + ctx.ch > ctx.clip_top && cy < ctx.clip_bottom {
            let selection_range = line_slice.map(|(cs, ce)| {
                let chunk_len = chunk.chars().count();
                (
                    cs.saturating_sub(char_offset).min(chunk_len),
                    ce.saturating_sub(char_offset).min(chunk_len),
                )
            });
            if let Some((cs, ce)) = selection_range {
                if ce > cs {
                    renderer.push_block_view_highlight(
                        &mut *ctx.verts,
                        ctx.left,
                        cy,
                        ctx.ch,
                        chunk,
                        cs,
                        ce,
                        ctx.selection_bg,
                        ctx.bg_uv,
                    );
                }
            }
            if let Some(bh) = ctx.find_block_highlight {
                if bh.0 == block_id.map(|b| b.0).unwrap_or(0) && bh.1 == line && !bh.2 {
                    find::push_find_highlight(
                        &mut *ctx.verts,
                        ctx.find_canvas,
                        text,
                        (bh.3, bh.4),
                        ctx.cols,
                        ci,
                        [ctx.left, cy],
                    );
                }
            }
            let t0 = std::time::Instant::now();
            let (source, styled) = if block_id.is_none() {
                (None, ctx.live_styled.clone())
            } else {
                style::block_arc_identity(ctx.blocks, block_id)
            };
            renderer.push_block_output_text_cached(
                &mut *ctx.verts,
                style::BlockOutputTextPaint {
                    x: ctx.left,
                    y: cy,
                    text: chunk,
                    semantic_text: text,
                    style: style_opt,
                    char_offset,
                    fallback: ctx.output_fg,
                    canvas,
                    // The styled text selection uses FULL-LINE
                    // coords (line_slice is already them);
                    // the highlight above projected them into
                    // the chunk.
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
                    chunk_idx: ci,
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
            // v1.10.5: wrapped cursor row — the caret lands on the cursor chunk.
            if let Some((cursor_line, cursor_col)) = ctx.tui_cursor {
                if crate::block_component::tui_caret_row_matches(
                    block_id.map(|b| b.0),
                    line,
                    cursor_line,
                ) && ci == cursor_col / ctx.cols
                {
                    let chunk_col = cursor_col % ctx.cols;
                    let caret_x = ctx.left + chunk_col as f32 * ctx.cw;
                    let (caret_area, caret_quad) =
                        crate::ime::block_view_tui_caret_geometry(caret_x, cy, ctx.cw, ctx.ch);
                    renderer.block_view_tui_caret_area.set(Some(caret_area));
                    push_quad(&mut *ctx.verts, caret_quad, ctx.bg_uv, [0.0; 4], ctx.accent);
                    if let Some((preedit, preedit_cursor)) = ctx.tui_preedit {
                        // v1.10.26 (FIX_IME_PREEDIT): B-path
                        // diagnostic — preedit drawn (wrapped
                        // cursor chunk path).
                        *ctx.caret_painted = true;
                        renderer.push_block_tui_preedit(
                            &mut *ctx.verts,
                            crate::paint::preedit::BlockTuiPreeditParams {
                                text: preedit,
                                cursor: preedit_cursor,
                                x: caret_x,
                                y: cy,
                                right: ctx.right,
                                cols: ctx.cols,
                                cursor_col: chunk_col,
                                bg_uv: ctx.bg_uv,
                                theme_bg: ctx.theme_bg,
                                accent: ctx.accent,
                            },
                        );
                    }
                }
            }
        }
        char_offset += chunk.chars().count();
    }
}
