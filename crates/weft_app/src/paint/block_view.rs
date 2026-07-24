// arch-gate: allow-over-800
// BlockView vertex builder: layout/cache/selection algorithms for the
// block view. Already extracted from renderer.rs; remaining size is the
// interdependent build_block_view_vertices + wrap + selection geometry.
//! BlockView vertex builder extracted from renderer.rs (A5).
//!
//! Layout/cache/selection algorithms are unchanged; immutable frame inputs are
//! grouped in BlockViewPaintModel while SelectionHandler stays explicitly mutable.

use crate::block_component::{spinner_char_for_phase, BlockTone};
use crate::paint::block_view::actions::{
    block_header_band_height, block_header_text_cols, push_block_header_actions,
    BlockHeaderActionPaint,
};
use crate::paint::block_view_model::BlockViewPaintModel;
use crate::paint::primitives::{color_to_normalized, push_quad};
use crate::paint::ui_helpers::{abbreviate_path, strip_prompt_prefix};
use crate::renderer::MetalRenderer;

mod actions;
mod layout_pass;
mod rows;
mod style;
use style::BlockOutputTextPaint;

// R2-3: re-export so accessibility.rs can compute the sticky block id without
// duplicating the geometric invariant (command row scrolled above clip_top).
pub(crate) use rows::sticky_block_id;

impl MetalRenderer {
    pub(crate) fn build_block_view_vertices(
        &self,
        model: BlockViewPaintModel<'_>,
        selection: &mut weft_core::selection::SelectionHandler,
    ) -> (
        Vec<f32>,
        Vec<crate::overlay::HitRegion>,
        Vec<weft_core::selection::BlockViewRow>,
    ) {
        let BlockViewPaintModel {
            blocks,
            region_bottom_y,
            cwd,
            git_branch,
            live,
            block_scroll,
            viewport_rows,
            block_hovered,
            spinner_phase,
            palette,
        } = model;
        let mut verts = Vec::new();
        let mut hit_regions: Vec<crate::overlay::HitRegion> = Vec::new();
        let mut bv_rows: Vec<weft_core::selection::BlockViewRow> = Vec::new();
        let cw = self.cell_width() as f32;
        let ch = self.cell_height() as f32;
        let vp_w = self.viewport.0;
        let vp_h = self.viewport.1;
        if cw <= 0.0 || ch <= 0.0 || vp_w <= 0.0 || vp_h <= 0.0 {
            return (verts, hit_regions, bv_rows);
        }

        let theme_bg = color_to_normalized(self.theme.background);
        let fg = color_to_normalized(self.theme.foreground);
        let prompt_c = [
            fg[0] * 0.70 + theme_bg[0] * 0.30,
            fg[1] * 0.70 + theme_bg[1] * 0.30,
            fg[2] * 0.70 + theme_bg[2] * 0.30,
            1.0,
        ];
        let dim = prompt_c;
        let separator = color_to_normalized(self.theme.separator);
        let (su, sv, suw, svh) = self.space_uv();
        let bg_uv = [su, sv + svh, su + suw, sv];

        let ctx = self.layout_ctx.expect("LayoutCtx built at draw() entry");
        let cwd_header_active = cwd.is_some() && live.is_none();
        let layout = crate::layout::layout_block_view(&ctx, region_bottom_y, cwd_header_active);
        let pitch = layout.pitch;
        let header_height = block_header_band_height(pitch, self.scale);
        let left = layout.left;
        let right = layout.right;
        let cols = layout.cols;
        let content_bottom_y = layout.clip_bottom;

        push_quad(
            &mut verts,
            [0.0, 0.0, vp_w, region_bottom_y.max(0.0)],
            bg_uv,
            [0.0; 4],
            theme_bg,
        );

        if cwd_header_active {
            let cwd = cwd.expect("active fixed CWD has text");
            let fixed_y = layout.fixed_cwd_y;
            // Step 4: 2.0px separator (was 1.5) to avoid sub-pixel blur at
            // 1× scale. At 2× scale, 2 logical px = 4 physical px — crisp.
            push_quad(
                &mut verts,
                [left, fixed_y, right, fixed_y + 2.0],
                bg_uv,
                [0.0; 4],
                separator,
            );
            let display = abbreviate_path(cwd);
            let display = if let Some(b) = git_branch {
                format!("{display} git:({b})")
            } else {
                display
            };
            if !display.is_empty() {
                self.push_text(&mut verts, left, fixed_y, &display, dim, cols);
            }
        }

        use layout_pass::{compute_block_layout_pass, LaidRow, LayoutPassInput, LayoutPassOutput};

        // Shared layout pass: single source of truth for row geometry.
        // Both this function (paint) and compute_block_view_rows (hit-testing)
        // call this, then walk the output to emit vertices or extract bv_rows.
        {
            let mut cache = self.block_layout_cache.borrow_mut();
            for b in blocks.iter() {
                cache.ensure_cached(b, cols);
            }
        }
        let layout_out = {
            let cache = self.block_layout_cache.borrow();
            compute_block_layout_pass(
                LayoutPassInput {
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
                    clip_top: layout.clip_top,
                    clip_bottom: content_bottom_y,
                    resolve_styles: true,
                },
                &cache,
            )
        };
        let LayoutPassOutput { rows, row_data } = layout_out;

        let scroll_px = (block_scroll as f32) * pitch;
        let clip_top = layout.clip_top;
        let clip_bottom = content_bottom_y;

        let selection_bg = {
            let accent = color_to_normalized(self.theme.accent);
            let bg = color_to_normalized(self.theme.background);
            let mut c = [
                accent[0] * 0.35 + bg[0] * 0.65,
                accent[1] * 0.35 + bg[1] * 0.65,
                accent[2] * 0.35 + bg[2] * 0.65,
                1.0,
            ];
            c[3] = 0.60;
            c
        };
        for (i, &dist) in rows.iter().enumerate() {
            let row_top_y = content_bottom_y - dist + scroll_px;
            let y = row_top_y;
            match &row_data[i] {
                LaidRow::Output {
                    text,
                    chunks,
                    block_id,
                    line: _,
                    style: _,
                } => {
                    if chunks.len() <= 1 {
                        bv_rows.push(weft_core::selection::BlockViewRow {
                            kind: weft_core::selection::BlockViewRowKind::Output,
                            text: text.to_string(),
                            block_id: *block_id,
                            y_top: y,
                            y_bottom: y + pitch,
                        });
                    } else {
                        for (ci, chunk) in chunks.iter().enumerate() {
                            let cy = y + ci as f32 * pitch;
                            bv_rows.push(weft_core::selection::BlockViewRow {
                                kind: weft_core::selection::BlockViewRowKind::Output,
                                text: chunk.clone(),
                                block_id: *block_id,
                                y_top: cy,
                                y_bottom: cy + pitch,
                            });
                        }
                    }
                }
                LaidRow::Command {
                    command, block_id, ..
                } => {
                    bv_rows.push(weft_core::selection::BlockViewRow {
                        kind: weft_core::selection::BlockViewRowKind::Command,
                        text: command.to_string(),
                        block_id: Some(*block_id),
                        y_top: y,
                        y_bottom: y + pitch,
                    });
                }
                LaidRow::Header { text, block_id, .. } => {
                    bv_rows.push(weft_core::selection::BlockViewRow {
                        kind: weft_core::selection::BlockViewRowKind::Header,
                        text: text.clone(),
                        block_id: Some(*block_id),
                        y_top: y,
                        y_bottom: y + header_height,
                    });
                }
                LaidRow::LiveHeader { text } => {
                    bv_rows.push(weft_core::selection::BlockViewRow {
                        kind: weft_core::selection::BlockViewRowKind::Header,
                        text: text.clone(),
                        block_id: None,
                        y_top: y,
                        y_bottom: y + pitch,
                    });
                }
                LaidRow::Separator => {
                    bv_rows.push(weft_core::selection::BlockViewRow {
                        kind: weft_core::selection::BlockViewRowKind::Separator,
                        text: String::new(),
                        block_id: None,
                        y_top: y,
                        y_bottom: y + pitch,
                    });
                }
                LaidRow::LiveCommand { command } => {
                    bv_rows.push(weft_core::selection::BlockViewRow {
                        kind: weft_core::selection::BlockViewRowKind::LiveCommand,
                        text: command.to_string(),
                        block_id: None,
                        y_top: y,
                        y_bottom: y + pitch,
                    });
                }
                LaidRow::Blank => {}
            }
        }
        if let Some(sel) = selection.block_view_selection.as_mut() {
            sel.sync_rows(bv_rows.clone());
        }
        let sel_bv = selection.block_view_selection.as_ref();
        let find_block_highlight = self.find_state.as_ref().and_then(|f| f.block_highlight);
        // F3-5: find match highlight color from the semantic token (was
        // hardcoded [0.95, 0.78, 0.20, 0.50]).
        let find_hl_bg = {
            let ui = crate::ui_tokens::UiColors::from_theme(&self.theme);
            let fm = color_to_normalized(ui.find_match);
            [fm[0], fm[1], fm[2], 0.50]
        };
        let sel_range_for_y = |row_mid_y: f32| -> Option<(usize, usize)> {
            let s = sel_bv?;
            let snap_idx = s.rows.iter().position(|r| r.contains_y(row_mid_y))?;
            let top = s.start.row_index.max(s.end.row_index);
            let bottom = s.start.row_index.min(s.end.row_index);
            if snap_idx < bottom || snap_idx > top {
                return None;
            }
            let max_char = s.rows[snap_idx].text.chars().count();
            let (c_start, c_end) = if top == bottom {
                let lo = s.start.char_index.min(s.end.char_index).min(max_char);
                let hi = s.start.char_index.max(s.end.char_index).min(max_char);
                (lo, hi)
            } else if snap_idx == top {
                let anchor = if s.start.row_index >= s.end.row_index {
                    s.start.char_index
                } else {
                    s.end.char_index
                };
                (anchor.min(max_char), max_char)
            } else if snap_idx == bottom {
                let anchor = if s.start.row_index >= s.end.row_index {
                    s.end.char_index
                } else {
                    s.start.char_index
                };
                (0, anchor.min(max_char))
            } else {
                (0, max_char)
            };
            (c_end > c_start).then_some((c_start, c_end))
        };
        let row_in_selection = |row_mid_y: f32| -> bool {
            let Some(s) = sel_bv else { return false };
            let Some(snap_idx) = s.rows.iter().position(|r| r.contains_y(row_mid_y)) else {
                return false;
            };
            let top = s.start.row_index.max(s.end.row_index);
            let bottom = s.start.row_index.min(s.end.row_index);
            snap_idx >= bottom && snap_idx <= top
        };

        for (i, &dist) in rows.iter().enumerate() {
            let row_top_y = content_bottom_y - dist + scroll_px;
            let row_height = if matches!(row_data[i], LaidRow::Header { .. }) {
                header_height
            } else {
                pitch
            };
            let row_bottom_y = row_top_y + row_height;

            if row_bottom_y < clip_top || row_top_y > clip_bottom {
                continue;
            }

            let y = row_top_y;

            match &row_data[i] {
                LaidRow::Output {
                    text,
                    chunks,
                    block_id,
                    line,
                    style,
                } => {
                    if chunks.len() <= 1 {
                        if let Some((cs, ce)) = sel_range_for_y(y + pitch * 0.5) {
                            self.push_block_view_highlight(
                                &mut verts,
                                left,
                                y,
                                ch,
                                text,
                                cs,
                                ce,
                                selection_bg,
                                bg_uv,
                            );
                        }
                        if let Some(bh) = find_block_highlight {
                            if bh.0 == block_id.map(|b| b.0).unwrap_or(0) && bh.1 == *line && !bh.2
                            {
                                let hx0 = left + bh.3 as f32 * cw;
                                let hx1 = hx0 + bh.4 as f32 * cw;
                                let hl_bg = find_hl_bg;
                                push_quad(
                                    &mut verts,
                                    [hx0, y, hx1, y + ch],
                                    bg_uv,
                                    [0.0; 4],
                                    hl_bg,
                                );
                            }
                        }
                        self.push_block_output_text(
                            &mut verts,
                            BlockOutputTextPaint {
                                x: left,
                                y,
                                text,
                                style: *style,
                                char_offset: 0,
                                fallback: fg,
                                max_cols: cols,
                                palette,
                                row_pitch: pitch,
                            },
                        );
                    } else {
                        let mut char_offset = 0;
                        for (ci, chunk) in chunks.iter().enumerate() {
                            let cy = y + ci as f32 * pitch;
                            if cy + ch > clip_top && cy < clip_bottom {
                                if let Some((cs, ce)) = sel_range_for_y(cy + pitch * 0.5) {
                                    self.push_block_view_highlight(
                                        &mut verts,
                                        left,
                                        cy,
                                        ch,
                                        chunk,
                                        cs,
                                        ce,
                                        selection_bg,
                                        bg_uv,
                                    );
                                }
                                if ci == 0 {
                                    if let Some(bh) = find_block_highlight {
                                        if bh.0 == block_id.map(|b| b.0).unwrap_or(0)
                                            && bh.1 == *line
                                            && !bh.2
                                        {
                                            let hx0 = left + bh.3 as f32 * cw;
                                            let hx1 = hx0 + bh.4 as f32 * cw;
                                            let hl_bg = find_hl_bg;
                                            push_quad(
                                                &mut verts,
                                                [hx0, cy, hx1, cy + ch],
                                                bg_uv,
                                                [0.0; 4],
                                                hl_bg,
                                            );
                                        }
                                    }
                                }
                                self.push_block_output_text(
                                    &mut verts,
                                    BlockOutputTextPaint {
                                        x: left,
                                        y: cy,
                                        text: chunk,
                                        style: *style,
                                        char_offset,
                                        fallback: fg,
                                        max_cols: cols,
                                        palette,
                                        row_pitch: pitch,
                                    },
                                );
                            }
                            char_offset += chunk.chars().count();
                        }
                    }
                }
                LaidRow::Command {
                    command,
                    collapsed,
                    foldable,
                    block_id,
                } => {
                    let (chev_w, avail_sub) = if *foldable {
                        let chev = if *collapsed { "▸" } else { "▾" };
                        self.push_text(&mut verts, left, y, chev, prompt_c, cols);
                        (cw, 3)
                    } else {
                        (0.0, 2)
                    };
                    self.push_text(&mut verts, left + chev_w, y, "❯ ", prompt_c, cols);
                    let cmd_x = left + chev_w + 2.0 * cw;
                    let avail = cols.saturating_sub(avail_sub).max(1);
                    if let Some((cs, ce)) = sel_range_for_y(y + pitch * 0.5) {
                        self.push_block_view_highlight(
                            &mut verts,
                            cmd_x,
                            y,
                            ch,
                            command,
                            cs,
                            ce,
                            selection_bg,
                            bg_uv,
                        );
                    }
                    if let Some(bh) = find_block_highlight {
                        if bh.0 == block_id.0 && bh.2 {
                            let hx0 = cmd_x + bh.3 as f32 * cw;
                            let hx1 = hx0 + bh.4 as f32 * cw;
                            let hl_bg = find_hl_bg;
                            push_quad(&mut verts, [hx0, y, hx1, y + ch], bg_uv, [0.0; 4], hl_bg);
                        }
                    }
                    let cleaned_cmd = strip_prompt_prefix(command);
                    self.push_line_tokenized(&mut verts, cmd_x, y, &cleaned_cmd, avail);
                    if *foldable {
                        hit_regions.push(crate::overlay::HitRegion {
                            x0: left,
                            y0: y,
                            x1: left + cw,
                            y1: y + pitch,
                            target: crate::overlay::HitTarget::BlockFold(*block_id),
                        });
                    }
                }
                LaidRow::Header {
                    text,
                    tone,
                    block_id,
                } => {
                    if row_in_selection(y + header_height * 0.5) {
                        push_quad(
                            &mut verts,
                            [left, y, right, y + header_height],
                            bg_uv,
                            [0.0; 4],
                            selection_bg,
                        );
                    }
                    let ui = crate::ui_tokens::UiColors::from_theme(&self.theme);
                    let color = match tone {
                        BlockTone::Success => dim,
                        BlockTone::Error => color_to_normalized(ui.error),
                        BlockTone::Warning => color_to_normalized(ui.warning),
                    };
                    let text_y = y + (header_height - pitch) * 0.5;
                    let text_cols = block_header_text_cols(left, right, cw, self.scale).min(cols);
                    self.push_text(&mut verts, left, text_y, text, color, text_cols);

                    push_block_header_actions(
                        self,
                        &mut verts,
                        &mut hit_regions,
                        blocks,
                        BlockHeaderActionPaint {
                            block_id: *block_id,
                            block_hovered,
                            y,
                            pitch,
                            right,
                            cell_width: cw,
                            cell_height: ch,
                            foreground: fg,
                        },
                    );
                }
                LaidRow::LiveHeader { text } => {
                    let ui = crate::ui_tokens::UiColors::from_theme(&self.theme);
                    self.push_text(
                        &mut verts,
                        left,
                        y,
                        text,
                        color_to_normalized(ui.focus),
                        cols,
                    );
                }
                LaidRow::Separator => {
                    if row_in_selection(y + pitch * 0.5) {
                        push_quad(
                            &mut verts,
                            [left, y, right, y + pitch],
                            bg_uv,
                            [0.0; 4],
                            selection_bg,
                        );
                    }
                    let ly = y + pitch * 0.5;
                    push_quad(
                        &mut verts,
                        [left, ly, right, ly + 1.5],
                        bg_uv,
                        [0.0; 4],
                        separator,
                    );
                }
                LaidRow::LiveCommand { command } => {
                    self.push_text(&mut verts, left, y, "❯ ", prompt_c, cols);
                    let cmd_x = left + 2.0 * cw;
                    let avail = cols.saturating_sub(2).max(1);
                    if let Some((cs, ce)) = sel_range_for_y(y + pitch * 0.5) {
                        self.push_block_view_highlight(
                            &mut verts,
                            cmd_x,
                            y,
                            ch,
                            command,
                            cs,
                            ce,
                            selection_bg,
                            bg_uv,
                        );
                    }
                    self.push_line_tokenized(&mut verts, cmd_x, y, command, avail);

                    // F3-2: Running-command activity indicator (braille spinner
                    // or static ● under Reduce Motion). Rendered at the right
                    // edge of the LiveCommand row so it doesn't overlap the
                    // command text.
                    if spinner_phase >= 0.0 {
                        let spinner_char =
                            spinner_char_for_phase(spinner_phase, self.reduce_motion);
                        let spinner_x = right - cw;
                        let ui = crate::ui_tokens::UiColors::from_theme(&self.theme);
                        let spinner_color = color_to_normalized(ui.focus);
                        self.push_text(
                            &mut verts,
                            spinner_x,
                            y,
                            &spinner_char.to_string(),
                            spinner_color,
                            1,
                        );
                    }
                }
                LaidRow::Blank => {}
            }
        }

        let sticky_block = rows::sticky_block_id(&bv_rows, clip_top, clip_bottom);
        if let Some(block) = sticky_block.and_then(|id| blocks.iter().find(|b| b.id == id)) {
            let cmd = &block.command;
            let block_cwd = block
                .cwd
                .as_deref()
                .map(abbreviate_path)
                .unwrap_or_default();
            let sticky_y = layout.clip_top;
            let header_rows = rows::sticky_header_rows(!block_cwd.is_empty());
            let sticky_bottom = sticky_y + header_rows as f32 * pitch;
            let sticky_bg = [
                theme_bg[0] + (1.0 - theme_bg[0]) * 0.08,
                theme_bg[1] + (1.0 - theme_bg[1]) * 0.08,
                theme_bg[2] + (1.0 - theme_bg[2]) * 0.08,
                1.0,
            ];
            push_quad(
                &mut verts,
                [0.0, sticky_y, vp_w, sticky_bottom],
                bg_uv,
                [0.0; 4],
                sticky_bg,
            );
            push_quad(
                &mut verts,
                [0.0, sticky_bottom, vp_w, sticky_bottom + 1.0],
                bg_uv,
                [0.0; 4],
                separator,
            );
            let command_y = if block_cwd.is_empty() {
                sticky_y
            } else {
                self.push_text(&mut verts, left, sticky_y, &block_cwd, dim, cols);
                sticky_y + pitch
            };
            self.push_text(&mut verts, left, command_y, "❯ ", prompt_c, cols);
            let cmd_x = left + 2.0 * cw;
            let avail = cols.saturating_sub(2).max(1);
            self.push_line_tokenized(&mut verts, cmd_x, command_y, cmd, avail);

            // R2-3 Phase 1: sticky header is no longer a dead paint band —
            // register copy/fold buttons + hit regions by reusing the same
            // path as the in-flow header. The geometry is naturally
            // disjoint: sticky buttons sit at y = clip_top, in-flow buttons
            // sit at y < clip_top (the in-flow header has scrolled off).
            push_block_header_actions(
                self,
                &mut verts,
                &mut hit_regions,
                blocks,
                BlockHeaderActionPaint {
                    block_id: block.id,
                    block_hovered,
                    y: sticky_y,
                    pitch,
                    right,
                    cell_width: cw,
                    cell_height: ch,
                    foreground: fg,
                },
            );
        }

        if let Some(hl_id) = self.panel_highlight {
            let mut hl_top: Option<f32> = None;
            let mut hl_bottom: Option<f32> = None;
            for (i, &dist) in rows.iter().enumerate() {
                let row_top_y = content_bottom_y - dist + scroll_px;
                let row_bottom_y = row_top_y + pitch;
                let belongs = match &row_data[i] {
                    LaidRow::Output { block_id, .. } => *block_id == Some(hl_id),
                    LaidRow::Command { block_id, .. } => *block_id == hl_id,
                    _ => false,
                };
                if belongs {
                    hl_top = Some(match hl_top {
                        Some(t) => t.min(row_top_y),
                        None => row_top_y,
                    });
                    hl_bottom = Some(match hl_bottom {
                        Some(b) => b.max(row_bottom_y),
                        None => row_bottom_y,
                    });
                }
            }
            if let (Some(top), Some(bottom)) = (hl_top, hl_bottom) {
                let y0 = top.max(clip_top);
                let y1 = bottom.min(clip_bottom);
                if y1 > y0 {
                    let accent = color_to_normalized(self.theme.accent);
                    let accent_alpha = [accent[0], accent[1], accent[2], 0.85];
                    let border_w = 2.0 * self.scale as f32;
                    push_quad(
                        &mut verts,
                        [left - border_w, y0, right + border_w, y0 + border_w],
                        bg_uv,
                        [0.0; 4],
                        accent_alpha,
                    );
                    push_quad(
                        &mut verts,
                        [left - border_w, y1 - border_w, right + border_w, y1],
                        bg_uv,
                        [0.0; 4],
                        accent_alpha,
                    );
                    push_quad(
                        &mut verts,
                        [left - border_w, y0, left, y1],
                        bg_uv,
                        [0.0; 4],
                        accent_alpha,
                    );
                    push_quad(
                        &mut verts,
                        [right, y0, right + border_w, y1],
                        bg_uv,
                        [0.0; 4],
                        accent_alpha,
                    );
                }
            }
        }

        (verts, hit_regions, bv_rows)
    }
}
