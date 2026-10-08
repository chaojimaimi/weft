use crate::paint::block_view::actions::{
    block_header_band_height, push_block_header_actions, BlockHeaderActionPaint,
};
use crate::paint::block_view_model::BlockViewPaintModel;
use crate::paint::grid_cache::BandSync;
use crate::paint::primitives::{
    color_to_normalized, push_quad, scale_color_alpha, snap_physical_rect,
};
use crate::paint::ui_helpers::abbreviate_path;
use crate::renderer::MetalRenderer;

mod actions;
#[cfg(test)]
mod bench;
mod find;
mod layout_pass;
mod output_paint;
mod row_paint;
mod rows;
mod style;
mod surfaces;

#[cfg(test)]
#[path = "block_view/streaming_bench.rs"]
mod streaming_bench;

pub(crate) use actions::block_header_action_rects;
pub(crate) use rows::sticky_block_id;

#[cfg(test)]
#[path = "block_view/b_path_tests.rs"]
mod b_path_tests;

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
            is_alt,
            now,
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
        let prompt_c = crate::paint::color_math::mix_fg_over_bg(fg, theme_bg, 0.30);
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
            // v1.10.34: draw the separator INSIDE the band the layout
            // reserves for it ([clip_bottom, fixed_cwd_y) — one pitch), with
            // a gap above the CWD text. It was previously drawn at
            // [fixed_y, fixed_y+2.0] — flush against the text's top edge,
            // which read as the CWD being visually cut off by the line.
            // The gap (0.35*pitch above the text top, i.e. 0.65*pitch below
            // the band top) keeps breathing room to the text while staying
            // clear of the scrolled content above.
            let gap = 0.35 * pitch;
            let (sep_y0, sep_y1) = snap_physical_rect(fixed_y - gap - 2.0, fixed_y - gap);
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

        /// v1.10.26: the owning segment key of a structural row. A Separator
        /// follows its block's Header (or the live header), so its association is
        /// the PRECEDING row's key. LiveCommand/LiveHeader/Separator → live segment.
        fn row_block_key(row: &LaidRow<'_>) -> Option<u64> {
            match row {
                LaidRow::Output { block_id, .. } => block_id.map(|b| b.0),
                LaidRow::Command { block_id, .. } => Some(block_id.0),
                LaidRow::Header { block_id, .. } => Some(block_id.0),
                LaidRow::DiagnosePanel { block_id, .. } => Some(block_id.0),
                _ => None,
            }
        }

        {
            let mut cache = self.block_layout_cache.borrow_mut();
            // M6-b B-1: same band as rows.rs; B-5 pump gate also in rows.rs.
            let band = BandSync::for_viewport(block_scroll as usize, viewport_rows);
            cache.sync_blocks(blocks, cols, band);
            if self.block_view_pump_idle(live.as_ref()) {
                cache.pump_deferred(blocks, cols, band, 1);
            }
        }
        self.styled_lookup_counter.set(0);
        self.styled_paint_us_counter.set(0);
        let palette_fp = self.block_palette_fingerprint(palette);
        let render_generation = self.styled_cache_generation();
        // v1.10.5: live rows read styles from the in-flight block before `live` is moved.
        let live_styled = live
            .as_ref()
            .and_then(|live| live.styled_output.map(std::sync::Arc::clone));
        // v1.10.26: the live composed document (selection source) is extracted
        // BEFORE the layout pass moves `live` (its &str outlives the frame).
        let live_output_for_source = live.as_ref().map(|live| live.output);
        // M5-b: laid rows borrow from the cache (zero-copy) until `row_data`'s last read.
        let cache = self.block_layout_cache.borrow();
        let layout_out = compute_block_layout_pass(
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
                now,
            },
            &cache,
            &mut self.live_layout_cache.borrow_mut(),
        );
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
        // v1.11.6 (M7/D-e): also take `.painted` — the opaque C1 text
        // contrast benchmark for selected cells (grid-same source, zero
        // new computation — one cached selection_colors() call).
        let selection_colors = crate::paint::selection_color::selection_colors(&self.theme);
        let selection_bg = selection_colors.quad;
        let selection_painted = selection_colors.painted;
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

        // v1.10.26 (FIX_IME_PREEDIT): B-path PREEDIT_DIAG tracking. Only
        // populated while a preedit is active, so the hot path is untouched
        // on the common (no-IME) frames. `first/last_live_line` record the
        // visible LIVE row span (block_id None, content line present) the
        // caret-matching loop actually iterated; `caret_painted` flips when
        // a live row matched `tui_caret_row_matches` and drew the preedit.
        let preedit_track = tui_preedit.map(|(p, _)| p);
        let mut first_live_line: Option<usize> = None;
        let mut last_live_line: Option<usize> = None;
        let mut caret_painted = false;

        // v1.11.6 (PLAN_v1116 M4/D-j): per-row painters share one ctx. The
        // ctx holds every cross-arm value (F17); its borrows end at the
        // loop's last use, so the post-loop reads of `caret_painted` /
        // `first_live_line` / `last_live_line` and the `verts` return below
        // stay valid.
        let mut pctx = row_paint::RowPaintCtx {
            verts: &mut verts,
            hit_regions: &mut hit_regions,
            cw,
            ch,
            pitch,
            header_height,
            left,
            right,
            frame_left,
            frame_right,
            cols,
            clip_top,
            clip_bottom,
            bg_uv,
            theme_bg,
            fg,
            accent,
            output_fg,
            prompt_c,
            separator,
            selection_bg,
            selection_painted,
            find_hl_bg,
            find_canvas,
            preedit_track,
            first_live_line: &mut first_live_line,
            last_live_line: &mut last_live_line,
            caret_painted: &mut caret_painted,
            blocks,
            palette,
            cache_namespace,
            ai_configured,
            block_hovered,
            block_action_hovered,
            spinner_phase,
            find_block_highlight,
            tui_cursor,
            tui_preedit,
            live_styled: &live_styled,
            palette_fp,
            render_generation,
            block_canvases: &block_canvases,
            sel_interval: &sel_interval,
            doc_source: &doc_source,
            renderer: self,
        };

        for (i, &dist) in rows.iter().enumerate() {
            let row_top_y = content_bottom_y - dist + scroll_px;
            // M5-b: Output entries are per-visual-row — their band sits
            // `chunk_idx` pitches below the pushed (line-top) dist, exactly
            // the legacy `y + ci * pitch` expression.
            let (row_top_y, row_height) = match &row_data[i] {
                LaidRow::Output { chunk_idx, .. } => (row_top_y + *chunk_idx as f32 * pitch, pitch),
                LaidRow::Header { .. } => (row_top_y, header_height),
                _ => (row_top_y, pitch),
            };
            let row_bottom_y = row_top_y + row_height;

            if row_bottom_y < clip_top || row_top_y > clip_bottom {
                continue;
            }

            let y = row_top_y;

            match &row_data[i] {
                LaidRow::Output { .. } => {
                    output_paint::paint_output_row(&mut pctx, &row_data[i], y);
                }
                LaidRow::Command {
                    command,
                    chunks,
                    collapsed,
                    foldable,
                    block_id,
                } => row_paint::paint_command(
                    &mut pctx, command, chunks, *collapsed, *foldable, *block_id, y,
                ),
                LaidRow::Header {
                    cwd,
                    duration,
                    status,
                    tone,
                    block_id,
                } => row_paint::paint_header(&mut pctx, cwd, duration, status, *tone, *block_id, y),
                LaidRow::LiveHeader { text } => row_paint::paint_live_header(&mut pctx, text, y),
                LaidRow::Separator => {
                    let key = if i == 0 {
                        None
                    } else {
                        row_block_key(&row_data[i - 1])
                    };
                    row_paint::paint_separator(&mut pctx, key, y);
                }
                LaidRow::LiveCommand { command, chunks } => {
                    row_paint::paint_live_command(&mut pctx, command, chunks, y)
                }
                LaidRow::Blank => row_paint::paint_blank(&mut pctx),
                LaidRow::DiagnosePanel {
                    text,
                    block_id,
                    is_first,
                    is_last,
                    is_error,
                } => row_paint::paint_diagnose_panel(
                    &mut pctx, text, *block_id, *is_first, *is_last, *is_error, y,
                ),
            }
        }

        // v1.10.26 (FIX_IME_PREEDIT): B-path diagnostic — emitted once per
        // frame while a BlockView-mode TUI preedit is active. The NORMAL
        // drawn path ("B") stays debug to avoid hot-path noise; the abnormal
        // "active but not painted" paths are promoted to info so a recurrence
        // under the default log level leaves evidence without users needing
        // `RUST_LOG=weft_app=debug`. "empty-text" is a macOS-internal marked-
        // text lifecycle event, not a bug — keep it quiet.
        if let Some((preedit, _)) = tui_preedit {
            let empty = preedit.is_empty();
            let path = if empty {
                "empty-text"
            } else if tui_cursor.is_none() {
                "B-suppressed-none"
            } else if !caret_painted {
                "B-no-match"
            } else {
                "B"
            };
            let abnormal = !empty && (tui_cursor.is_none() || !caret_painted);
            if abnormal {
                tracing::info!(
                    show_block_view = true,
                    is_alt,
                    cursor_col = tui_cursor.map(|c| c.1),
                    preedit_len = preedit.chars().count(),
                    expected_cursor_line = tui_cursor.map(|c| c.0),
                    first_live_line,
                    last_live_line,
                    caret_painted,
                    path,
                    "PREEDIT_DIAG"
                );
            } else {
                tracing::debug!(
                    show_block_view = true,
                    is_alt,
                    cursor_col = tui_cursor.map(|c| c.1),
                    preedit_len = preedit.chars().count(),
                    expected_cursor_line = tui_cursor.map(|c| c.0),
                    first_live_line,
                    last_live_line,
                    caret_painted,
                    path,
                    "PREEDIT_DIAG"
                );
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
            let sticky_bg = crate::paint::color_math::lighten_to_white(theme_bg, 0.08);
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
}
