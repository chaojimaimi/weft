//! v1.11.6 (PLAN_v1116 M4 / D-j): per-`LaidRow` paint bodies extracted from
//! `build_block_view_vertices` — pure structural move with byte-equal output
//! (locked by the M3 vertex goldens). Command / Header / LiveHeader /
//! Separator / LiveCommand / Blank / DiagnosePanel live here; Output rows
//! live in the sibling `output_paint` module.
//!
//! `RowPaintCtx` carries the cross-arm state shared with the loop (F17
//! inventory): output buffers, geometry, colors, tracking vars, model
//! references, cache handles and selection data. The old selection closures
//! were data-ized (architect P2-8): `sel_interval` + `doc_source` are ctx
//! fields and `char_range_for` / `row_in_selection` are ctx methods, so the
//! ctx holds no `impl Fn` generics and no RefCell borrows (borrows stay in
//! the caller's prep section).

use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

use crate::block_component::{spinner_char_for_phase, BlockTone};
use crate::paint::block_view::actions::{
    block_header_text_cols, push_block_header_actions, BlockHeaderActionPaint,
};
use crate::paint::block_view::find::{self, FindHighlightCanvas};
use crate::paint::primitives::{
    color_to_normalized, derive_cwd_gray, push_quad, snap_physical_rect,
};
use crate::paint::ui_helpers::strip_prompt_prefix;
use crate::renderer::MetalRenderer;
use crate::selection::SelectionDocSource;
use weft_core::blocks::{Block, BlockId, StyledOutput};
use weft_core::grid::Color;
use weft_core::selection::SelectionInterval;

/// Per-frame paint state shared by every row painter. Constructed once in
/// `build_block_view_vertices` (after the layout pass / selection prep) and
/// passed `&mut` through every arm of the loop. All references borrow data
/// that outlives the loop; the borrows end at the ctx's last use, so the
/// caller can keep reading `verts` / `caret_painted` etc. after the loop.
pub(super) struct RowPaintCtx<'a> {
    // Output buffers.
    pub(super) verts: &'a mut Vec<f32>,
    pub(super) hit_regions: &'a mut Vec<crate::overlay::HitRegion>,
    // Geometry.
    pub(super) cw: f32,
    pub(super) ch: f32,
    pub(super) pitch: f32,
    pub(super) header_height: f32,
    pub(super) left: f32,
    pub(super) right: f32,
    pub(super) frame_left: f32,
    pub(super) frame_right: f32,
    pub(super) cols: usize,
    pub(super) clip_top: f32,
    pub(super) clip_bottom: f32,
    pub(super) bg_uv: [f32; 4],
    // Colors.
    pub(super) theme_bg: [f32; 4],
    pub(super) fg: [f32; 4],
    pub(super) accent: [f32; 4],
    pub(super) output_fg: [f32; 4],
    pub(super) prompt_c: [f32; 4],
    pub(super) separator: [f32; 4],
    pub(super) selection_bg: [f32; 4],
    /// v1.11.6 (PLAN_v1116 M7/D-e): painted selection color — the C1
    /// text-contrast benchmark for selected cells (opaque; grid-same
    /// source `selection_colors().painted`).
    pub(super) selection_painted: [f32; 4],
    pub(super) find_hl_bg: [f32; 4],
    pub(super) find_canvas: FindHighlightCanvas,
    // v1.10.26 (FIX_IME_PREEDIT): B-path tracking — written inside the
    // loop, read by the caller's post-loop PREEDIT_DIAG log.
    pub(super) preedit_track: Option<&'a str>,
    pub(super) first_live_line: &'a mut Option<usize>,
    pub(super) last_live_line: &'a mut Option<usize>,
    pub(super) caret_painted: &'a mut bool,
    // Model references.
    pub(super) blocks: &'a [Block],
    pub(super) palette: &'a [Color; 256],
    pub(super) cache_namespace: u64,
    pub(super) ai_configured: bool,
    pub(super) block_hovered: Option<BlockId>,
    pub(super) block_action_hovered: Option<crate::block_component::BlockHeaderAction>,
    pub(super) spinner_phase: f32,
    pub(super) find_block_highlight: Option<(u64, usize, bool, usize, usize)>,
    pub(super) tui_cursor: Option<(usize, usize)>,
    pub(super) tui_preedit: Option<(&'a str, Option<(usize, usize)>)>,
    pub(super) live_styled: &'a Option<Arc<StyledOutput>>,
    // Cache handles.
    pub(super) palette_fp: u64,
    pub(super) render_generation: u64,
    pub(super) block_canvases: &'a HashMap<BlockId, [f32; 4]>,
    // Selection (formerly the `char_range_for` / `row_in_selection`
    // closures; now data + methods per architect P2-8).
    pub(super) sel_interval: &'a Option<SelectionInterval>,
    pub(super) doc_source: &'a SelectionDocSource<'a>,
    // All &self access to MetalRenderer methods/fields goes through this.
    pub(super) renderer: &'a MetalRenderer,
}

impl<'a> RowPaintCtx<'a> {
    /// v1.10.26 (FIX_SELECTION_CONTENT_ANCHORS): the `char_range_for`
    /// closure, data-ized. `key_block` / `key_line` are the layout row's
    /// content coordinates; rows with a real line are tested against the
    /// anchor interval; structural rows (no line) fall back to the owning
    /// segment's intersection.
    pub(super) fn char_range_for(
        &self,
        key_block: Option<u64>,
        key_line: Option<usize>,
    ) -> Option<(usize, usize)> {
        let iv = self.sel_interval.as_ref()?;
        let line = key_line?;
        iv.char_range(self.doc_source, key_block, line)
    }

    /// v1.10.26 (FIX_SELECTION_CONTENT_ANCHORS): the `row_in_selection`
    /// closure, data-ized — whole-row band test for structural rows.
    pub(super) fn row_in_selection(&self, key_block: Option<u64>, key_line: Option<usize>) -> bool {
        let Some(iv) = self.sel_interval.as_ref() else {
            return false;
        };
        match key_line {
            Some(line) => iv.contains(self.doc_source, key_block, line),
            None => iv.block_intersects(self.doc_source, key_block),
        }
    }
}

/// v1.10.26: `LaidRow::Command` — prompt-stripped command chunks with a
/// fold chevron, whole-row selection banding and find-match quads.
pub(super) fn paint_command(
    ctx: &mut RowPaintCtx<'_>,
    command: &str,
    chunks: &Rc<[String]>,
    collapsed: bool,
    foldable: bool,
    block_id: BlockId,
    y: f32,
) {
    let renderer = ctx.renderer;
    let (chev_w, avail_sub) = if foldable {
        let chev = if collapsed { "▸" } else { "▾" };
        renderer.push_text(&mut *ctx.verts, ctx.left, y, chev, ctx.prompt_c, ctx.cols);
        (ctx.cw, 3)
    } else {
        (0.0, 2)
    };
    renderer.push_text(
        &mut *ctx.verts,
        ctx.left + chev_w,
        y,
        "> ",
        ctx.prompt_c,
        ctx.cols,
    );
    let cmd_x = ctx.left + chev_w + 2.0 * ctx.cw;
    let first_avail = ctx.cols.saturating_sub(avail_sub).max(1);
    let cmd_color = color_to_normalized(renderer.theme.foreground);
    // 匹配范围相对原始 command,换算到 strip 后(chunks 基准)。
    let cleaned_cmd = strip_prompt_prefix(command);
    let prefix_chars = command.chars().count() - cleaned_cmd.chars().count();
    for (ci, chunk) in chunks.iter().enumerate() {
        let cy = y + ci as f32 * ctx.pitch;
        if cy + ctx.ch <= ctx.clip_top || cy >= ctx.clip_bottom {
            continue;
        }
        let (lx, lavail) = if ci == 0 {
            (cmd_x, first_avail)
        } else {
            (ctx.left, ctx.cols)
        };
        // v1.10.26: command rows carry no content line, so
        // they are banded whole-row when their block's segment
        // intersects the selection (no partial char slicing).
        if ctx.row_in_selection(Some(block_id.0), None) {
            renderer.push_block_view_highlight(
                &mut *ctx.verts,
                lx,
                cy,
                ctx.ch,
                chunk,
                0,
                chunk.chars().count(),
                ctx.selection_bg,
                ctx.bg_uv,
            );
        }
        if let Some(bh) = ctx.find_block_highlight {
            if bh.0 == block_id.0 && bh.2 {
                let hit_start = bh.3.saturating_sub(prefix_chars);
                let hit_end = bh.3.saturating_add(bh.4).saturating_sub(prefix_chars);
                if hit_end > hit_start {
                    for (rci, dcol, dlen) in
                        find::find_chunk_visual_ranges(chunks, hit_start, hit_end)
                    {
                        if rci != ci {
                            continue;
                        }
                        let x0 = lx + dcol as f32 * ctx.cw;
                        push_quad(
                            &mut *ctx.verts,
                            [x0, cy, x0 + dlen as f32 * ctx.cw, cy + ctx.ch],
                            ctx.bg_uv,
                            [0.0; 4],
                            ctx.find_hl_bg,
                        );
                    }
                }
            }
        }
        renderer.push_text(&mut *ctx.verts, lx, cy, chunk, cmd_color, lavail);
    }
    if foldable {
        ctx.hit_regions.push(crate::overlay::HitRegion {
            x0: ctx.left,
            y0: y,
            x1: ctx.left + ctx.cw,
            y1: y + ctx.pitch,
            target: crate::overlay::HitTarget::BlockFold(block_id),
        });
    }
}

/// v1.10.26: `LaidRow::Header` — selection band + CWD/duration/status
/// segments (bookmark star when flagged) + copy/fold/diagnose actions.
pub(super) fn paint_header(
    ctx: &mut RowPaintCtx<'_>,
    cwd: &str,
    duration: &str,
    status: &str,
    tone: BlockTone,
    block_id: BlockId,
    y: f32,
) {
    let renderer = ctx.renderer;
    if ctx.row_in_selection(Some(block_id.0), None) {
        push_quad(
            &mut *ctx.verts,
            [ctx.left, y, ctx.right, y + ctx.header_height],
            ctx.bg_uv,
            [0.0; 4],
            ctx.selection_bg,
        );
    }
    let ui = crate::ui_tokens::UiColors::from_theme(&renderer.theme)
        .with_increase_contrast(renderer.increase_contrast);
    let cwd_c = derive_cwd_gray(color_to_normalized(renderer.theme.foreground));
    let meta_c = color_to_normalized(renderer.theme.output.metadata);
    let status_c = match tone {
        BlockTone::Success => cwd_c, // 成功块 status 恒为空,不会用到
        BlockTone::Error => color_to_normalized(ui.error),
        BlockTone::Warning => color_to_normalized(ui.warning),
    };
    let text_y = y + (ctx.header_height - ctx.pitch) * 0.5;
    let bookmarked = renderer.bookmarked_blocks.contains(&block_id);
    let (text_x, text_cols) = if bookmarked {
        let star_color = color_to_normalized(renderer.theme.accent);
        renderer.push_text(&mut *ctx.verts, ctx.left, text_y, "★", star_color, 1);
        (
            ctx.left + 2.0 * ctx.cw,
            block_header_text_cols(
                ctx.left + 2.0 * ctx.cw,
                ctx.right,
                ctx.cw,
                renderer.scale,
                ctx.ai_configured,
            )
            .min(ctx.cols.saturating_sub(2)),
        )
    } else {
        (
            ctx.left,
            block_header_text_cols(
                ctx.left,
                ctx.right,
                ctx.cw,
                renderer.scale,
                ctx.ai_configured,
            )
            .min(ctx.cols),
        )
    };
    let mut x = text_x;
    let mut remaining = text_cols;
    push_header_segment(
        renderer,
        &mut *ctx.verts,
        &mut x,
        text_y,
        &mut remaining,
        cwd,
        cwd_c,
        ctx.cw,
    );
    if !duration.is_empty() {
        push_header_segment(
            renderer,
            &mut *ctx.verts,
            &mut x,
            text_y,
            &mut remaining,
            " · ",
            meta_c,
            ctx.cw,
        );
        push_header_segment(
            renderer,
            &mut *ctx.verts,
            &mut x,
            text_y,
            &mut remaining,
            duration,
            meta_c,
            ctx.cw,
        );
    }
    if !status.is_empty() {
        push_header_segment(
            renderer,
            &mut *ctx.verts,
            &mut x,
            text_y,
            &mut remaining,
            " · ",
            meta_c,
            ctx.cw,
        );
        push_header_segment(
            renderer,
            &mut *ctx.verts,
            &mut x,
            text_y,
            &mut remaining,
            status,
            status_c,
            ctx.cw,
        );
    }

    push_block_header_actions(
        renderer,
        &mut *ctx.verts,
        &mut *ctx.hit_regions,
        ctx.blocks,
        BlockHeaderActionPaint {
            block_id,
            block_hovered: ctx.block_hovered,
            action_hovered: ctx.block_action_hovered,
            y,
            pitch: ctx.pitch,
            right: ctx.right,
            cell_width: ctx.cw,
            cell_height: ctx.ch,
            foreground: ctx.fg,
            ai_configured: ctx.ai_configured,
        },
    );
}

/// v1.10.26: `LaidRow::LiveHeader` — the in-flight block's header line,
/// banded whole-row when the live segment intersects the selection.
pub(super) fn paint_live_header(ctx: &mut RowPaintCtx<'_>, text: &str, y: f32) {
    let renderer = ctx.renderer;
    // v1.10.26: banded whole-row when the live segment
    // intersects (parity with the old row-index highlight).
    if ctx.row_in_selection(None, None) {
        push_quad(
            &mut *ctx.verts,
            [ctx.left, y, ctx.right, y + ctx.pitch],
            ctx.bg_uv,
            [0.0; 4],
            ctx.selection_bg,
        );
    }
    let ui = crate::ui_tokens::UiColors::from_theme(&renderer.theme)
        .with_increase_contrast(renderer.increase_contrast);
    renderer.push_text(
        &mut *ctx.verts,
        ctx.left,
        y,
        text,
        color_to_normalized(ui.focus),
        ctx.cols,
    );
}

/// v1.10.26: `LaidRow::Separator` — a thin line between blocks. Its owning
/// segment key (the PRECEDING row's key, `None` for the first row) is
/// resolved by the caller and passed in.
pub(super) fn paint_separator(ctx: &mut RowPaintCtx<'_>, key: Option<u64>, y: f32) {
    if ctx.row_in_selection(key, None) {
        push_quad(
            &mut *ctx.verts,
            [ctx.left, y, ctx.right, y + ctx.pitch],
            ctx.bg_uv,
            [0.0; 4],
            ctx.selection_bg,
        );
    }
    let ly = y + ctx.pitch * 0.5;
    let (sep_y0, sep_y1) = snap_physical_rect(ly, ly + 1.5);
    push_quad(
        &mut *ctx.verts,
        [ctx.frame_left, sep_y0, ctx.frame_right, sep_y1],
        ctx.bg_uv,
        [0.0; 4],
        ctx.separator,
    );
}

/// v1.10.26: `LaidRow::LiveCommand` — the running command line with a
/// spinner glyph on the last row when `spinner_phase >= 0.0`.
pub(super) fn paint_live_command(
    ctx: &mut RowPaintCtx<'_>,
    command: &str,
    chunks: &Rc<[String]>,
    y: f32,
) {
    let renderer = ctx.renderer;
    renderer.push_text(&mut *ctx.verts, ctx.left, y, "> ", ctx.prompt_c, ctx.cols);
    let cmd_x = ctx.left + 2.0 * ctx.cw;
    let first_avail = ctx.cols.saturating_sub(2).max(1);
    let cmd_color = color_to_normalized(renderer.theme.foreground);
    // 匹配范围相对 live.command,换算到 strip 后(chunks 基准)。
    let cleaned_live = strip_prompt_prefix(command);
    let prefix_chars = command.chars().count() - cleaned_live.chars().count();
    for (ci, chunk) in chunks.iter().enumerate() {
        let cy = y + ci as f32 * ctx.pitch;
        if cy + ctx.ch <= ctx.clip_top || cy >= ctx.clip_bottom {
            continue;
        }
        let (lx, lavail) = if ci == 0 {
            (cmd_x, first_avail)
        } else {
            (ctx.left, ctx.cols)
        };
        // v1.10.26: the live command carries no content line; it is banded
        // whole-row when the live segment intersects.
        if ctx.row_in_selection(None, None) {
            renderer.push_block_view_highlight(
                &mut *ctx.verts,
                lx,
                cy,
                ctx.ch,
                chunk,
                0,
                chunk.chars().count(),
                ctx.selection_bg,
                ctx.bg_uv,
            );
        }
        if let Some(bh) = ctx.find_block_highlight {
            if bh.2 && bh.0 == 0 {
                let hit_start = bh.3.saturating_sub(prefix_chars);
                let hit_end = bh.3.saturating_add(bh.4).saturating_sub(prefix_chars);
                if hit_end > hit_start {
                    for (rci, dcol, dlen) in
                        find::find_chunk_visual_ranges(chunks, hit_start, hit_end)
                    {
                        if rci != ci {
                            continue;
                        }
                        let x0 = lx + dcol as f32 * ctx.cw;
                        push_quad(
                            &mut *ctx.verts,
                            [x0, cy, x0 + dlen as f32 * ctx.cw, cy + ctx.ch],
                            ctx.bg_uv,
                            [0.0; 4],
                            ctx.find_hl_bg,
                        );
                    }
                }
            }
        }
        renderer.push_text(&mut *ctx.verts, lx, cy, chunk, cmd_color, lavail);
    }

    if ctx.spinner_phase >= 0.0 {
        let spinner_char = spinner_char_for_phase(ctx.spinner_phase, renderer.reduce_motion);
        let spinner_x = ctx.right - ctx.cw;
        let ui = crate::ui_tokens::UiColors::from_theme(&renderer.theme)
            .with_increase_contrast(renderer.increase_contrast);
        let spinner_color = color_to_normalized(ui.focus);
        let last_y = y + chunks.len().saturating_sub(1) as f32 * ctx.pitch;
        renderer.push_text(
            &mut *ctx.verts,
            spinner_x,
            last_y,
            &spinner_char.to_string(),
            spinner_color,
            1,
        );
    }
}

/// `LaidRow::Blank` — spacer row for a completed `clear` band; paints
/// nothing. Kept as an explicit arm so every `LaidRow` variant has a
/// painter (dispatch symmetry).
pub(super) fn paint_blank(_ctx: &mut RowPaintCtx<'_>) {}

/// v1.8.2: `LaidRow::DiagnosePanel` — tinted panel below a block's output
/// with a close button (first row) and a bottom border (last row).
pub(super) fn paint_diagnose_panel(
    ctx: &mut RowPaintCtx<'_>,
    text: &str,
    block_id: BlockId,
    is_first: bool,
    is_last: bool,
    is_error: bool,
    y: f32,
) {
    let renderer = ctx.renderer;
    // v1.8.2: diagnose panel below output, tinted to stand out.
    let panel_bg = if is_error {
        let err = color_to_normalized(renderer.theme.output.failure);
        [
            err[0] * 0.15 + ctx.theme_bg[0] * 0.85,
            err[1] * 0.15 + ctx.theme_bg[1] * 0.85,
            err[2] * 0.15 + ctx.theme_bg[2] * 0.85,
            1.0,
        ]
    } else {
        // Success/info: accent-tinted.
        let acc = color_to_normalized(renderer.theme.accent);
        [
            acc[0] * 0.12 + ctx.theme_bg[0] * 0.88,
            acc[1] * 0.12 + ctx.theme_bg[1] * 0.88,
            acc[2] * 0.12 + ctx.theme_bg[2] * 0.88,
            1.0,
        ]
    };
    push_quad(
        &mut *ctx.verts,
        [ctx.frame_left, y, ctx.frame_right, y + ctx.pitch],
        ctx.bg_uv,
        [0.0; 4],
        panel_bg,
    );
    // Panel text with a 1-cell left indent for visual hierarchy.
    let text_color = if is_error {
        color_to_normalized(renderer.theme.output.failure)
    } else {
        ctx.fg
    };
    let avail_cols = ctx.cols.saturating_sub(2).max(1);
    renderer.push_text(
        &mut *ctx.verts,
        ctx.left + ctx.cw,
        y,
        text,
        text_color,
        avail_cols,
    );
    // Close button (×) on the first row, right-aligned.
    if is_first {
        let close_x = ctx.right - ctx.cw * 1.5;
        let close_color = [ctx.fg[0], ctx.fg[1], ctx.fg[2], ctx.fg[3] * 0.6];
        renderer.push_text(&mut *ctx.verts, close_x, y, "×", close_color, 1);
        ctx.hit_regions.push(crate::overlay::HitRegion {
            x0: close_x,
            y0: y,
            x1: close_x + ctx.cw * 1.5,
            y1: y + ctx.pitch,
            target: crate::overlay::HitTarget::BlockDiagnoseClose(block_id),
        });
    }
    // Bottom border on the last row.
    if is_last {
        let (by0, by1) = snap_physical_rect(y + ctx.pitch - 1.0, y + ctx.pitch);
        push_quad(
            &mut *ctx.verts,
            [ctx.frame_left, by0, ctx.frame_right, by1],
            ctx.bg_uv,
            [0.0; 4],
            ctx.separator,
        );
    }
}

/// Header 分段着色:推进 x/remaining,消除 4 段重复样板。
#[allow(clippy::too_many_arguments)]
pub(super) fn push_header_segment(
    renderer: &MetalRenderer,
    verts: &mut Vec<f32>,
    x: &mut f32,
    y: f32,
    remaining: &mut usize,
    seg: &str,
    color: [f32; 4],
    cw: f32,
) {
    if *remaining == 0 {
        return;
    }
    let w = weft_core::grid::terminal_text_width(seg).min(*remaining);
    renderer.push_text(verts, *x, y, seg, color, *remaining);
    *x += w as f32 * cw;
    *remaining = remaining.saturating_sub(w);
}
