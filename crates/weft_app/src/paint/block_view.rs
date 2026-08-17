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
            live_head_lines,
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
            block_diagnose_state,
            ai_configured,
            tui_cursor,
            tui_preedit,
            cursor_blink_on: _,
        } = model;
        let mut verts = Vec::new();
        let mut hit_regions: Vec<crate::overlay::HitRegion> = Vec::new();
        let bv_rows: Vec<weft_core::selection::BlockViewRow> = Vec::new();
        let cw = self.cell_width() as f32;
        let ch = self.cell_height() as f32;
        let vp_w = self.viewport.0;
        let vp_h = self.viewport.1;
        if cw <= 0.0 || ch <= 0.0 || vp_w <= 0.0 || vp_h <= 0.0 {
            return (verts, hit_regions, bv_rows);
        }

        let theme_bg = scale_color_alpha(color_to_normalized(self.theme.background), self.opacity);
        let fg = color_to_normalized(self.theme.foreground);
        let accent = color_to_normalized(self.theme.accent);
        let output_fg = color_to_normalized(self.theme.output.output_default);
        let prompt_c = [
            fg[0] * 0.70 + theme_bg[0] * 0.30,
            fg[1] * 0.70 + theme_bg[1] * 0.30,
            fg[2] * 0.70 + theme_bg[2] * 0.30,
            1.0,
        ];
        let cwd_c =
            crate::paint::primitives::derive_cwd_gray(color_to_normalized(self.theme.foreground));
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
        // v1.10.5: live rows read styles from the in-flight block before `live` is moved.
        let live_styled = live
            .as_ref()
            .and_then(|live| live.styled_output.map(std::sync::Arc::clone));
        // v1.10.26: the live composed document for the selection source is
        // extracted BEFORE the layout pass moves `live` (raw &str into the
        // terminal's block storage; outlives the frame).
        let live_output_for_source = live.as_ref().map(|live| live.output);
        let layout_out = {
            let cache = self.block_layout_cache.borrow();
            compute_block_layout_pass(
                LayoutPassInput {
                    blocks,
                    live,
                    pane_session_id: cache_namespace,
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
                    block_diagnose_state,
                },
                &cache,
                &mut self.live_layout_cache.borrow_mut(),
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

        // v1.10.22: shared selection color (theme.selection base + WCAG 3:1
        // adaptive guarantee, evaluated against both the raw theme bg and
        // the stripe canvas bg — see selection_color.rs). The composite
        // chain below stays; error/warning tone and hover rows sit on more
        // heavily tinted canvases and may land slightly under 3:1 there
        // (accepted boundary, still far above the pre-fix 1.27-1.75).
        let selection_bg = crate::paint::selection_color::selection_colors(&self.theme).quad;
        // v1.10.26 (FIX_SELECTION_CONTENT_ANCHORS): the selection is
        // content-anchored — no row snapshot, no `sync_rows`, no
        // `selected_row_identities` retention. `bv_rows` culls to the pure
        // visible window again. A structural document change (split / block
        // finish / deletion — the finished-block id order differs from the
        // fingerprint recorded at selection start) makes the anchors stale,
        // so the selection is cleared right here. Streaming append to the
        // live segment is NOT part of the fingerprint and never clears.
        if selection.block_view_selection.is_some() {
            // v1.10.26 (rust-reviewer S1): the fingerprint keys on the live
            // document's HEAD length too — a preserved superseded frame
            // prepends lines into the composed document and silently shifts
            // every live-segment anchor even though the finished-block order
            // is unchanged.
            let fingerprint =
                crate::selection::block_selection_fingerprint(blocks, live_head_lines);
            if selection.block_doc_fingerprint.as_deref() != Some(fingerprint.as_slice()) {
                tracing::info!(
                    ?fingerprint,
                    "block document structure changed; cleared anchor selection"
                );
                selection.clear();
            }
        }
        let doc_source = crate::selection::SelectionDocSource::new(blocks, live_output_for_source);
        // Per-frame derived selection interval (single direction: content
        // source → highlight; never snapshot → content).
        let sel_interval = selection
            .block_view_selection
            .map(|sel| sel.interval(&doc_source));
        let bv_rows = rows::build_bv_rows(
            &rows,
            &row_data,
            rows::BvRowsGeometry {
                pitch,
                header_height,
                content_bottom_y,
                scroll_px,
                clip_top,
                clip_bottom,
                cull: true,
            },
        );
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
        // v1.10.26 (FIX_SELECTION_CONTENT_ANCHORS): per-frame DERIVED highlight
        // helpers. `key_block` / `key_line` are the layout row's content
        // coordinates; rows with a real line are tested against the anchor
        // interval; structural rows (Command/Header/LiveCommand/Separator —
        // no line) fall back to their owning segment's intersection so a
        // block visibly inside the selection is fully banded.
        let char_range_for =
            |key_block: Option<u64>, key_line: Option<usize>| -> Option<(usize, usize)> {
                let iv = sel_interval.as_ref()?;
                let line = key_line?;
                iv.char_range(&doc_source, key_block, line)
            };
        let row_in_selection = |key_block: Option<u64>, key_line: Option<usize>| -> bool {
            let Some(iv) = sel_interval.as_ref() else {
                return false;
            };
            match key_line {
                Some(line) => iv.contains(&doc_source, key_block, line),
                None => iv.block_intersects(&doc_source, key_block),
            }
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
            // v1.10.26: the owning segment key of a structural row. A
            // Separator follows its block's Header (or the live header), so
            // its association is the PRECEDING row's key.
            let row_block_key = |row: &LaidRow<'_>| match row {
                LaidRow::Output { block_id, .. } => block_id.map(|b| b.0),
                LaidRow::Command { block_id, .. } => Some(block_id.0),
                LaidRow::Header { block_id, .. } => Some(block_id.0),
                LaidRow::DiagnosePanel { block_id, .. } => Some(block_id.0),
                _ => None, // LiveCommand/LiveHeader/Separator → live segment
            };
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
                    // v1.10.26: content-coordinate key of this row (resume
                    // hints carry line == usize::MAX → not addressable).
                    let key_line = (*line != usize::MAX).then_some(*line);
                    let key_block = block_id.map(|b| b.0);
                    if chunks.len() <= 1 {
                        let selection_range = char_range_for(key_block, key_line);
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
                        // v1.10.5: live rows (block_id == None) read styles
                        // from the in-flight block's styled_output.
                        let (source, styled) = if block_id.is_none() {
                            (None, live_styled.clone())
                        } else {
                            style::block_arc_identity(blocks, *block_id)
                        };
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
                        // v1.10.5: grid cursor is invisible in document mode,
                        // so TUI caret/preedit paint only on live rows.
                        if let Some((cursor_line, cursor_col)) = tui_cursor {
                            if crate::block_component::tui_caret_row_matches(
                                block_id.map(|b| b.0),
                                *line,
                                cursor_line,
                            ) {
                                let caret_x = left + cursor_col as f32 * cw;
                                let (caret_area, caret_quad) =
                                    crate::ime::block_view_tui_caret_geometry(caret_x, y, cw, ch);
                                self.block_view_tui_caret_area.set(Some(caret_area));
                                // Steady-on: blink timer only wakes for AtPrompt, so cursor_blink_on is stale.
                                push_quad(&mut verts, caret_quad, bg_uv, [0.0; 4], accent);
                                if let Some((preedit, preedit_cursor)) = tui_preedit {
                                    self.push_block_tui_preedit(
                                        &mut verts,
                                        crate::paint::preedit::BlockTuiPreeditParams {
                                            text: preedit,
                                            cursor: preedit_cursor,
                                            x: caret_x,
                                            y,
                                            right,
                                            cols,
                                            cursor_col,
                                            bg_uv,
                                            theme_bg,
                                            accent,
                                        },
                                    );
                                }
                            }
                        }
                    } else {
                        // v1.10.26: the selection range is per SOURCE line;
                        // each chunk covers a slice of it, so the range is
                        // projected through the chunk's `char_offset` and
                        // clamped to the chunk length.
                        let line_slice = char_range_for(key_block, key_line);
                        let mut char_offset = 0;
                        for (ci, chunk) in chunks.iter().enumerate() {
                            let cy = y + ci as f32 * pitch;
                            if cy + ch > clip_top && cy < clip_bottom {
                                let selection_range = line_slice.map(|(cs, ce)| {
                                    let chunk_len = chunk.chars().count();
                                    (
                                        cs.saturating_sub(char_offset).min(chunk_len),
                                        ce.saturating_sub(char_offset).min(chunk_len),
                                    )
                                });
                                if let Some((cs, ce)) = selection_range {
                                    if ce > cs {
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
                                let (source, styled) = if block_id.is_none() {
                                    (None, live_styled.clone())
                                } else {
                                    style::block_arc_identity(blocks, *block_id)
                                };
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
                                        // The styled text selection uses FULL-LINE
                                        // coords (line_slice is already them);
                                        // the highlight above projected them into
                                        // the chunk.
                                        selection: line_slice
                                            .map(|(start, end)| (start, end, selection_canvas)),
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
                                // v1.10.5: wrapped cursor row — the caret lands on the cursor chunk.
                                if let Some((cursor_line, cursor_col)) = tui_cursor {
                                    if crate::block_component::tui_caret_row_matches(
                                        block_id.map(|b| b.0),
                                        *line,
                                        cursor_line,
                                    ) && ci == cursor_col / cols
                                    {
                                        let chunk_col = cursor_col % cols;
                                        let caret_x = left + chunk_col as f32 * cw;
                                        let (caret_area, caret_quad) =
                                            crate::ime::block_view_tui_caret_geometry(
                                                caret_x, cy, cw, ch,
                                            );
                                        self.block_view_tui_caret_area.set(Some(caret_area));
                                        push_quad(&mut verts, caret_quad, bg_uv, [0.0; 4], accent);
                                        if let Some((preedit, preedit_cursor)) = tui_preedit {
                                            self.push_block_tui_preedit(
                                                &mut verts,
                                                crate::paint::preedit::BlockTuiPreeditParams {
                                                    text: preedit,
                                                    cursor: preedit_cursor,
                                                    x: caret_x,
                                                    y: cy,
                                                    right,
                                                    cols,
                                                    cursor_col: chunk_col,
                                                    bg_uv,
                                                    theme_bg,
                                                    accent,
                                                },
                                            );
                                        }
                                    }
                                }
                            }
                            char_offset += chunk.chars().count();
                        }
                    }
                }
                LaidRow::Command {
                    command,
                    chunks,
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
                    self.push_text(&mut verts, left + chev_w, y, "> ", prompt_c, cols);
                    let cmd_x = left + chev_w + 2.0 * cw;
                    let first_avail = cols.saturating_sub(avail_sub).max(1);
                    let cmd_color = color_to_normalized(self.theme.foreground);
                    // 匹配范围相对原始 command,换算到 strip 后(chunks 基准)。
                    let cleaned_cmd = strip_prompt_prefix(command);
                    let prefix_chars = command.chars().count() - cleaned_cmd.chars().count();
                    for (ci, chunk) in chunks.iter().enumerate() {
                        let cy = y + ci as f32 * pitch;
                        if cy + ch <= clip_top || cy >= clip_bottom {
                            continue;
                        }
                        let (lx, lavail) = if ci == 0 {
                            (cmd_x, first_avail)
                        } else {
                            (left, cols)
                        };
                        // v1.10.26: command rows carry no content line, so
                        // they are banded whole-row when their block's segment
                        // intersects the selection (no partial char slicing).
                        if row_in_selection(Some(block_id.0), None) {
                            self.push_block_view_highlight(
                                &mut verts,
                                lx,
                                cy,
                                ch,
                                chunk,
                                0,
                                chunk.chars().count(),
                                selection_bg,
                                bg_uv,
                            );
                        }
                        if let Some(bh) = find_block_highlight {
                            if bh.0 == block_id.0 && bh.2 {
                                let hit_start = bh.3.saturating_sub(prefix_chars);
                                let hit_end =
                                    bh.3.saturating_add(bh.4).saturating_sub(prefix_chars);
                                if hit_end > hit_start {
                                    for (rci, dcol, dlen) in
                                        find::find_chunk_visual_ranges(chunks, hit_start, hit_end)
                                    {
                                        if rci != ci {
                                            continue;
                                        }
                                        let x0 = lx + dcol as f32 * cw;
                                        push_quad(
                                            &mut verts,
                                            [x0, cy, x0 + dlen as f32 * cw, cy + ch],
                                            bg_uv,
                                            [0.0; 4],
                                            find_hl_bg,
                                        );
                                    }
                                }
                            }
                        }
                        self.push_text(&mut verts, lx, cy, chunk, cmd_color, lavail);
                    }
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
                    cwd,
                    duration,
                    status,
                    tone,
                    block_id,
                } => {
                    if row_in_selection(Some(block_id.0), None) {
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
                    let cwd_c = crate::paint::primitives::derive_cwd_gray(color_to_normalized(
                        self.theme.foreground,
                    ));
                    let meta_c = color_to_normalized(self.theme.output.metadata);
                    let status_c = match tone {
                        BlockTone::Success => cwd_c, // 成功块 status 恒为空,不会用到
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
                            block_header_text_cols(
                                left + 2.0 * cw,
                                right,
                                cw,
                                self.scale,
                                ai_configured,
                            )
                            .min(cols.saturating_sub(2)),
                        )
                    } else {
                        (
                            left,
                            block_header_text_cols(left, right, cw, self.scale, ai_configured)
                                .min(cols),
                        )
                    };
                    let mut x = text_x;
                    let mut remaining = text_cols;
                    self.push_header_segment(
                        &mut verts,
                        &mut x,
                        text_y,
                        &mut remaining,
                        cwd,
                        cwd_c,
                        cw,
                    );
                    if !duration.is_empty() {
                        self.push_header_segment(
                            &mut verts,
                            &mut x,
                            text_y,
                            &mut remaining,
                            " · ",
                            meta_c,
                            cw,
                        );
                        self.push_header_segment(
                            &mut verts,
                            &mut x,
                            text_y,
                            &mut remaining,
                            duration,
                            meta_c,
                            cw,
                        );
                    }
                    if !status.is_empty() {
                        self.push_header_segment(
                            &mut verts,
                            &mut x,
                            text_y,
                            &mut remaining,
                            " · ",
                            meta_c,
                            cw,
                        );
                        self.push_header_segment(
                            &mut verts,
                            &mut x,
                            text_y,
                            &mut remaining,
                            status,
                            status_c,
                            cw,
                        );
                    }

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
                            ai_configured,
                        },
                    );
                }
                LaidRow::LiveHeader { text } => {
                    // v1.10.26: banded whole-row when the live segment
                    // intersects (parity with the old row-index highlight).
                    if row_in_selection(None, None) {
                        push_quad(
                            &mut verts,
                            [left, y, right, y + pitch],
                            bg_uv,
                            [0.0; 4],
                            selection_bg,
                        );
                    }
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
                    let key = if i == 0 {
                        None
                    } else {
                        row_block_key(&row_data[i - 1])
                    };
                    if row_in_selection(key, None) {
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
                LaidRow::LiveCommand { command, chunks } => {
                    self.push_text(&mut verts, left, y, "> ", prompt_c, cols);
                    let cmd_x = left + 2.0 * cw;
                    let first_avail = cols.saturating_sub(2).max(1);
                    let cmd_color = color_to_normalized(self.theme.foreground);
                    // 匹配范围相对 live.command,换算到 strip 后(chunks 基准)。
                    let cleaned_live = strip_prompt_prefix(command);
                    let prefix_chars = command.chars().count() - cleaned_live.chars().count();
                    for (ci, chunk) in chunks.iter().enumerate() {
                        let cy = y + ci as f32 * pitch;
                        if cy + ch <= clip_top || cy >= clip_bottom {
                            continue;
                        }
                        let (lx, lavail) = if ci == 0 {
                            (cmd_x, first_avail)
                        } else {
                            (left, cols)
                        };
                        // v1.10.26: the live command carries no content line; it is banded
                        // whole-row when the live segment intersects.
                        if row_in_selection(None, None) {
                            self.push_block_view_highlight(
                                &mut verts,
                                lx,
                                cy,
                                ch,
                                chunk,
                                0,
                                chunk.chars().count(),
                                selection_bg,
                                bg_uv,
                            );
                        }
                        if let Some(bh) = find_block_highlight {
                            if bh.2 && bh.0 == 0 {
                                let hit_start = bh.3.saturating_sub(prefix_chars);
                                let hit_end =
                                    bh.3.saturating_add(bh.4).saturating_sub(prefix_chars);
                                if hit_end > hit_start {
                                    for (rci, dcol, dlen) in
                                        find::find_chunk_visual_ranges(chunks, hit_start, hit_end)
                                    {
                                        if rci != ci {
                                            continue;
                                        }
                                        let x0 = lx + dcol as f32 * cw;
                                        push_quad(
                                            &mut verts,
                                            [x0, cy, x0 + dlen as f32 * cw, cy + ch],
                                            bg_uv,
                                            [0.0; 4],
                                            find_hl_bg,
                                        );
                                    }
                                }
                            }
                        }
                        self.push_text(&mut verts, lx, cy, chunk, cmd_color, lavail);
                    }

                    if spinner_phase >= 0.0 {
                        let spinner_char =
                            spinner_char_for_phase(spinner_phase, self.reduce_motion);
                        let spinner_x = right - cw;
                        let ui = crate::ui_tokens::UiColors::from_theme(&self.theme)
                            .with_increase_contrast(self.increase_contrast);
                        let spinner_color = color_to_normalized(ui.focus);
                        let last_y = y + chunks.len().saturating_sub(1) as f32 * pitch;
                        self.push_text(
                            &mut verts,
                            spinner_x,
                            last_y,
                            &spinner_char.to_string(),
                            spinner_color,
                            1,
                        );
                    }
                }
                LaidRow::Blank => {}
                LaidRow::DiagnosePanel {
                    text,
                    block_id,
                    is_first,
                    is_last,
                    is_error,
                } => {
                    // v1.8.2: diagnose panel below output, tinted to stand out.
                    let panel_bg = if *is_error {
                        let err = color_to_normalized(self.theme.output.failure);
                        [
                            err[0] * 0.15 + theme_bg[0] * 0.85,
                            err[1] * 0.15 + theme_bg[1] * 0.85,
                            err[2] * 0.15 + theme_bg[2] * 0.85,
                            1.0,
                        ]
                    } else {
                        // Success/info: accent-tinted.
                        let acc = color_to_normalized(self.theme.accent);
                        [
                            acc[0] * 0.12 + theme_bg[0] * 0.88,
                            acc[1] * 0.12 + theme_bg[1] * 0.88,
                            acc[2] * 0.12 + theme_bg[2] * 0.88,
                            1.0,
                        ]
                    };
                    push_quad(
                        &mut verts,
                        [frame_left, y, frame_right, y + pitch],
                        bg_uv,
                        [0.0; 4],
                        panel_bg,
                    );
                    // Panel text with a 1-cell left indent for visual hierarchy.
                    let text_color = if *is_error {
                        color_to_normalized(self.theme.output.failure)
                    } else {
                        fg
                    };
                    let avail_cols = cols.saturating_sub(2).max(1);
                    self.push_text(&mut verts, left + cw, y, text, text_color, avail_cols);
                    // Close button (×) on the first row, right-aligned.
                    if *is_first {
                        let close_x = right - cw * 1.5;
                        let close_color = [fg[0], fg[1], fg[2], fg[3] * 0.6];
                        self.push_text(&mut verts, close_x, y, "×", close_color, 1);
                        hit_regions.push(crate::overlay::HitRegion {
                            x0: close_x,
                            y0: y,
                            x1: close_x + cw * 1.5,
                            y1: y + pitch,
                            target: crate::overlay::HitTarget::BlockDiagnoseClose(*block_id),
                        });
                    }
                    // Bottom border on the last row.
                    if *is_last {
                        let (by0, by1) = snap_physical_rect(y + pitch - 1.0, y + pitch);
                        push_quad(
                            &mut verts,
                            [frame_left, by0, frame_right, by1],
                            bg_uv,
                            [0.0; 4],
                            separator,
                        );
                    }
                }
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
            let cmd_color = color_to_normalized(self.theme.foreground);
            self.push_text(&mut verts, cmd_x, command_y, cmd, cmd_color, avail);

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
                    ai_configured,
                },
            );
        }

        if let Some(hl_id) = self.panel_highlight {
            let mut hl_top: Option<f32> = None;
            let mut hl_bottom: Option<f32> = None;
            for (i, &dist) in rows.iter().enumerate() {
                let row_top_y = content_bottom_y - dist + scroll_px;
                let row_bottom_y = row_top_y
                    + match &row_data[i] {
                        LaidRow::Command { chunks, .. } => chunks.len().max(1) as f32 * pitch,
                        _ => pitch,
                    };
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

    /// Header 分段着色:推进 x/remaining,消除 4 段重复样板。
    #[allow(clippy::too_many_arguments)]
    fn push_header_segment(
        &self,
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
        self.push_text(verts, *x, y, seg, color, *remaining);
        *x += w as f32 * cw;
        *remaining = remaining.saturating_sub(w);
    }
}
