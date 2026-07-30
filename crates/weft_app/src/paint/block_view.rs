// arch-gate: allow-over-800
use crate::block_component::{spinner_char_for_phase, BlockTone};
use crate::paint::block_view::actions::{
    block_header_band_height, block_header_text_cols, push_block_header_actions,
    BlockHeaderActionPaint,
};
use crate::paint::block_view_model::BlockViewPaintModel;
use crate::paint::primitives::{
    color_to_normalized, composite_color_over, push_quad, scale_color_alpha, snap_physical_rect,
};
use crate::paint::ui_helpers::{abbreviate_path, strip_prompt_prefix};
use crate::renderer::MetalRenderer;

mod actions;
mod find;
mod layout_pass;
mod rows;
mod style;
mod surfaces;

pub(crate) use actions::block_header_action_rects;
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
            block_selected,
            block_action_hovered,
            spinner_phase,
            find_block_highlight,
            palette,
            cache_namespace,
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

        let theme_bg = scale_color_alpha(color_to_normalized(self.theme.background), self.opacity);
        let fg = color_to_normalized(self.theme.foreground);
        let output_fg = color_to_normalized(self.theme.output.output_default);
        let prompt_c = [
            fg[0] * 0.70 + theme_bg[0] * 0.30,
            fg[1] * 0.70 + theme_bg[1] * 0.30,
            fg[2] * 0.70 + theme_bg[2] * 0.30,
            1.0,
        ];
        let cwd_c = color_to_normalized(self.theme.output.cwd);
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
        let (frame_left, frame_right) = (layout.frame_left, layout.frame_right);
        let cols = layout.cols;
        let content_bottom_y = layout.clip_bottom;

        push_quad(
            &mut verts,
            [
                frame_left,
                layout.clip_top,
                frame_right,
                region_bottom_y.max(0.0),
            ],
            bg_uv,
            [0.0; 4],
            theme_bg,
        );

        if cwd_header_active {
            let cwd = cwd.expect("active fixed CWD has text");
            let fixed_y = layout.fixed_cwd_y;
            let (sep_y0, sep_y1) = snap_physical_rect(fixed_y, fixed_y + 2.0);
            push_quad(
                &mut verts,
                [frame_left, sep_y0, frame_right, sep_y1],
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
                self.push_text(&mut verts, left, fixed_y, &display, cwd_c, cols);
            }
        }

        use layout_pass::{compute_block_layout_pass, LaidRow, LayoutPassInput, LayoutPassOutput};

        {
            let mut cache = self.block_layout_cache.borrow_mut();
            cache.sync_blocks(blocks, cols);
        }
        self.styled_lookup_counter.set(0);
        self.styled_paint_us_counter.set(0);
        let palette_fp = self.block_palette_fingerprint(palette);
        let render_generation = self.styled_cache_generation();
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
                    styled_lookup_counter: Some(&self.styled_lookup_counter),
                },
                &cache,
            )
        };
        let LayoutPassOutput {
            rows,
            row_data,
            expanded_block_count,
        } = layout_out;
        self.last_expanded_block_count.set(expanded_block_count);

        let scroll_px = block_scroll * pitch;
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
                    line,
                    style: _,
                } => {
                    let line_idx = (*line != usize::MAX).then_some(*line);
                    if chunks.len() <= 1 {
                        bv_rows.push(weft_core::selection::BlockViewRow {
                            kind: weft_core::selection::BlockViewRowKind::Output,
                            text: text.to_string(),
                            block_id: *block_id,
                            y_top: y,
                            y_bottom: y + pitch,
                            line: line_idx,
                            chunk_char_offset: 0,
                        });
                    } else {
                        let mut offset = 0usize;
                        for (ci, chunk) in chunks.iter().enumerate() {
                            let cy = y + ci as f32 * pitch;
                            bv_rows.push(weft_core::selection::BlockViewRow {
                                kind: weft_core::selection::BlockViewRowKind::Output,
                                text: chunk.clone(),
                                block_id: *block_id,
                                y_top: cy,
                                y_bottom: cy + pitch,
                                line: line_idx,
                                chunk_char_offset: offset,
                            });
                            offset += chunk.chars().count();
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
                        line: None,
                        chunk_char_offset: 0,
                    });
                }
                LaidRow::Header { text, block_id, .. } => {
                    bv_rows.push(weft_core::selection::BlockViewRow {
                        kind: weft_core::selection::BlockViewRowKind::Header,
                        text: text.clone(),
                        block_id: Some(*block_id),
                        y_top: y,
                        y_bottom: y + header_height,
                        line: None,
                        chunk_char_offset: 0,
                    });
                }
                LaidRow::LiveHeader { text } => {
                    bv_rows.push(weft_core::selection::BlockViewRow {
                        kind: weft_core::selection::BlockViewRowKind::Header,
                        text: text.clone(),
                        block_id: None,
                        y_top: y,
                        y_bottom: y + pitch,
                        line: None,
                        chunk_char_offset: 0,
                    });
                }
                LaidRow::Separator => {
                    bv_rows.push(weft_core::selection::BlockViewRow {
                        kind: weft_core::selection::BlockViewRowKind::Separator,
                        text: String::new(),
                        block_id: None,
                        y_top: y,
                        y_bottom: y + pitch,
                        line: None,
                        chunk_char_offset: 0,
                    });
                }
                LaidRow::LiveCommand { command } => {
                    bv_rows.push(weft_core::selection::BlockViewRow {
                        kind: weft_core::selection::BlockViewRowKind::LiveCommand,
                        text: command.to_string(),
                        block_id: None,
                        y_top: y,
                        y_bottom: y + pitch,
                        line: None,
                        chunk_char_offset: 0,
                    });
                }
                LaidRow::Blank => {}
            }
        }
        if let Some(sel) = selection.block_view_selection.as_mut() {
            sel.sync_rows(bv_rows.clone());
        }
        let sel_bv = selection.block_view_selection.as_ref();
        let find_hl_bg = {
            let ui = crate::ui_tokens::UiColors::from_theme(&self.theme)
                .with_increase_contrast(self.increase_contrast);
            let fm = color_to_normalized(ui.find_match);
            [fm[0], fm[1], fm[2], 0.50]
        };
        let find_canvas = find::FindHighlightCanvas {
            cell_width: cw,
            cell_height: ch,
            background_uv: bg_uv,
            color: find_hl_bg,
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

        let block_canvases = surfaces::push_block_surfaces(
            self,
            &mut verts,
            &rows,
            &row_data,
            &layout,
            surfaces::BlockSurfaceState {
                scroll_px,
                header_height,
                hovered: block_hovered,
                selected: block_selected,
            },
        );

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
                    let canvas = surfaces::canvas_for(&block_canvases, *block_id, theme_bg);
                    let selection_canvas = composite_color_over(selection_bg, canvas);
                    if chunks.len() <= 1 {
                        let selection_range = sel_range_for_y(y + pitch * 0.5);
                        if let Some((cs, ce)) = selection_range {
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
                                find::push_find_highlight(
                                    &mut verts,
                                    find_canvas,
                                    text,
                                    (bh.3, bh.4),
                                    cols,
                                    0,
                                    [left, y],
                                );
                            }
                        }
                        let t0 = std::time::Instant::now();
                        let (source, styled) = style::block_arc_identity(blocks, *block_id);
                        let source = (*line != usize::MAX).then_some(source).flatten();
                        self.push_block_output_text_cached(
                            &mut verts,
                            style::BlockOutputTextPaint {
                                x: left,
                                y,
                                text,
                                semantic_text: text,
                                style: *style,
                                char_offset: 0,
                                fallback: output_fg,
                                canvas,
                                selection: selection_range
                                    .map(|(start, end)| (start, end, selection_canvas)),
                                max_cols: cols,
                                palette,
                                row_pitch: pitch,
                            },
                            style::CacheKeyInput {
                                pane_session_id: cache_namespace,
                                block_id: block_id.map(|b| b.0).unwrap_or(0),
                                line_idx: *line,
                                chunk_idx: 0,
                                render_generation,
                                palette_fingerprint: palette_fp,
                                source,
                                styled,
                            },
                            &self.styled_line_cache,
                        );
                        self.styled_paint_us_counter.set(
                            self.styled_paint_us_counter.get() + t0.elapsed().as_micros() as u64,
                        );
                    } else {
                        let mut char_offset = 0;
                        for (ci, chunk) in chunks.iter().enumerate() {
                            let cy = y + ci as f32 * pitch;
                            if cy + ch > clip_top && cy < clip_bottom {
                                let selection_range = sel_range_for_y(cy + pitch * 0.5);
                                if let Some((cs, ce)) = selection_range {
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
                                if let Some(bh) = find_block_highlight {
                                    if bh.0 == block_id.map(|b| b.0).unwrap_or(0)
                                        && bh.1 == *line
                                        && !bh.2
                                    {
                                        find::push_find_highlight(
                                            &mut verts,
                                            find_canvas,
                                            text,
                                            (bh.3, bh.4),
                                            cols,
                                            ci,
                                            [left, cy],
                                        );
                                    }
                                }
                                let t0 = std::time::Instant::now();
                                let (source, styled) = style::block_arc_identity(blocks, *block_id);
                                self.push_block_output_text_cached(
                                    &mut verts,
                                    style::BlockOutputTextPaint {
                                        x: left,
                                        y: cy,
                                        text: chunk,
                                        semantic_text: text,
                                        style: *style,
                                        char_offset,
                                        fallback: output_fg,
                                        canvas,
                                        selection: selection_range.map(|(start, end)| {
                                            (
                                                char_offset + start,
                                                char_offset + end,
                                                selection_canvas,
                                            )
                                        }),
                                        max_cols: cols,
                                        palette,
                                        row_pitch: pitch,
                                    },
                                    style::CacheKeyInput {
                                        pane_session_id: cache_namespace,
                                        block_id: block_id.map(|b| b.0).unwrap_or(0),
                                        line_idx: *line,
                                        chunk_idx: ci,
                                        render_generation,
                                        palette_fingerprint: palette_fp,
                                        source,
                                        styled,
                                    },
                                    &self.styled_line_cache,
                                );
                                self.styled_paint_us_counter.set(
                                    self.styled_paint_us_counter.get()
                                        + t0.elapsed().as_micros() as u64,
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
                    let canvas = block_canvases.get(block_id).copied().unwrap_or(theme_bg);
                    let selection_canvas = composite_color_over(selection_bg, canvas);
                    let (chev_w, avail_sub) = if *foldable {
                        let chev = if *collapsed { "▸" } else { "▾" };
                        self.push_text(&mut verts, left, y, chev, prompt_c, cols);
                        (cw, 3)
                    } else {
                        (0.0, 2)
                    };
                    self.push_text(&mut verts, left + chev_w, y, "> ", prompt_c, cols);
                    let cmd_x = left + chev_w + 2.0 * cw;
                    let avail = cols.saturating_sub(avail_sub).max(1);
                    let selection_range = sel_range_for_y(y + pitch * 0.5);
                    if let Some((cs, ce)) = selection_range {
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
                            find::push_find_highlight(
                                &mut verts,
                                find_canvas,
                                command,
                                (bh.3, bh.4),
                                usize::MAX,
                                0,
                                [cmd_x, y],
                            );
                        }
                    }
                    let cleaned_cmd = strip_prompt_prefix(command);
                    self.push_line_tokenized_on_canvas(
                        &mut verts,
                        [cmd_x, y],
                        &cleaned_cmd,
                        avail,
                        canvas,
                        selection_range.map(|(start, end)| (start, end, selection_canvas)),
                    );
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
                    let ui = crate::ui_tokens::UiColors::from_theme(&self.theme)
                        .with_increase_contrast(self.increase_contrast);
                    let color = match tone {
                        BlockTone::Success => color_to_normalized(self.theme.output.success),
                        BlockTone::Error => color_to_normalized(ui.error),
                        BlockTone::Warning => color_to_normalized(ui.warning),
                    };
                    let text_y = y + (header_height - pitch) * 0.5;
                    let bookmarked = self.bookmarked_blocks.contains(block_id);
                    let (text_x, text_cols) = if bookmarked {
                        let star_color = color_to_normalized(self.theme.accent);
                        self.push_text(&mut verts, left, text_y, "★", star_color, 1);
                        (
                            left + 2.0 * cw,
                            block_header_text_cols(left + 2.0 * cw, right, cw, self.scale)
                                .min(cols.saturating_sub(2)),
                        )
                    } else {
                        (
                            left,
                            block_header_text_cols(left, right, cw, self.scale).min(cols),
                        )
                    };
                    self.push_text(&mut verts, text_x, text_y, text, color, text_cols);

                    push_block_header_actions(
                        self,
                        &mut verts,
                        &mut hit_regions,
                        blocks,
                        BlockHeaderActionPaint {
                            block_id: *block_id,
                            block_hovered,
                            action_hovered: block_action_hovered,
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
                    let ui = crate::ui_tokens::UiColors::from_theme(&self.theme)
                        .with_increase_contrast(self.increase_contrast);
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
                    let (sep_y0, sep_y1) = snap_physical_rect(ly, ly + 1.5);
                    push_quad(
                        &mut verts,
                        [frame_left, sep_y0, frame_right, sep_y1],
                        bg_uv,
                        [0.0; 4],
                        separator,
                    );
                }
                LaidRow::LiveCommand { command } => {
                    let selection_canvas = composite_color_over(selection_bg, theme_bg);
                    self.push_text(&mut verts, left, y, "> ", prompt_c, cols);
                    let cmd_x = left + 2.0 * cw;
                    let avail = cols.saturating_sub(2).max(1);
                    let selection_range = sel_range_for_y(y + pitch * 0.5);
                    if let Some((cs, ce)) = selection_range {
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
                    self.push_line_tokenized_on_canvas(
                        &mut verts,
                        [cmd_x, y],
                        command,
                        avail,
                        theme_bg,
                        selection_range.map(|(start, end)| (start, end, selection_canvas)),
                    );

                    if spinner_phase >= 0.0 {
                        let spinner_char =
                            spinner_char_for_phase(spinner_phase, self.reduce_motion);
                        let spinner_x = right - cw;
                        let ui = crate::ui_tokens::UiColors::from_theme(&self.theme)
                            .with_increase_contrast(self.increase_contrast);
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
                [frame_left, sticky_y, frame_right, sticky_bottom],
                bg_uv,
                [0.0; 4],
                sticky_bg,
            );
            push_quad(
                &mut verts,
                {
                    let (y0, y1) = snap_physical_rect(sticky_bottom, sticky_bottom + 1.0);
                    [frame_left, y0, frame_right, y1]
                },
                bg_uv,
                [0.0; 4],
                separator,
            );
            let command_y = if block_cwd.is_empty() {
                sticky_y
            } else {
                self.push_text(&mut verts, left, sticky_y, &block_cwd, cwd_c, cols);
                sticky_y + pitch
            };
            self.push_text(&mut verts, left, command_y, "> ", prompt_c, cols);
            let cmd_x = left + 2.0 * cw;
            let avail = cols.saturating_sub(2).max(1);
            self.push_line_tokenized_on_canvas(
                &mut verts,
                [cmd_x, command_y],
                cmd,
                avail,
                sticky_bg,
                None,
            );

            push_block_header_actions(
                self,
                &mut verts,
                &mut hit_regions,
                blocks,
                BlockHeaderActionPaint {
                    block_id: block.id,
                    block_hovered,
                    action_hovered: block_action_hovered,
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
