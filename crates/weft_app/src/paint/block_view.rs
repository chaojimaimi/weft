//! BlockView vertex builder extracted from renderer.rs (A5).
//!
//! Layout/cache/selection algorithms are unchanged; immutable frame inputs are
//! grouped in BlockViewPaintModel while SelectionHandler stays explicitly mutable.

use std::rc::Rc;

use crate::paint::block_view_model::BlockViewPaintModel;
use crate::paint::grid_cache::wrap_line_chunks;
use crate::paint::primitives::{color_to_normalized, push_quad};
use crate::paint::ui_helpers::{abbreviate_path, block_duration_str, strip_prompt_prefix};
use crate::renderer::MetalRenderer;
use weft_core::blocks::BlockId;

impl MetalRenderer {
    /// Compute block-view rows for hit-testing WITHOUT building vertices.
    /// This is the M1 on-demand alternative to caching `block_view_rows`
    /// during `draw()`. The geometry_controller calls this when a mouse
    /// event arrives, so hit-testing uses current-frame data instead of
    /// the renderer's previous-frame cache.
    ///
    /// The layout logic mirrors `build_block_view_vertices` (layout pass +
    /// bv_rows extraction). Both paths use the same `layout_block_view` +
    /// `wrap_line_chunks` + `block_layout_cache` so the y-bands are identical.
    pub(crate) fn compute_block_view_rows(
        &self,
        model: BlockViewPaintModel<'_>,
    ) -> Vec<weft_core::selection::BlockViewRow> {
        use weft_core::selection::{BlockViewRow, BlockViewRowKind};

        let BlockViewPaintModel {
            blocks,
            region_bottom_y,
            cwd,
            git_branch: _,
            live,
            block_scroll,
        } = model;

        let cw = self.cell_width() as f32;
        let ch = self.cell_height() as f32;
        let vp_w = self.viewport.0;
        let vp_h = self.viewport.1;
        if cw <= 0.0 || ch <= 0.0 || vp_w <= 0.0 || vp_h <= 0.0 {
            return Vec::new();
        }

        let ctx = match self.layout_ctx {
            Some(c) => c,
            None => return Vec::new(),
        };
        let cwd_header_active = cwd.is_some() && live.is_none();
        let layout = crate::layout::layout_block_view(&ctx, region_bottom_y, cwd_header_active);
        let pitch = layout.pitch;
        let content_bottom_y = layout.clip_bottom;
        let cols = layout.cols;

        // Layout pass: compute cumulative y-distances + row data.
        // Mirrors build_block_view_vertices lines 96-209.
        enum LaidRow<'a> {
            Output {
                text: &'a str,
                chunks: Rc<[String]>,
                block_id: Option<BlockId>,
            },
            Command {
                command: &'a str,
                block_id: BlockId,
            },
            Header,
            Separator,
            LiveCommand {
                command: &'a str,
            },
            Blank,
        }

        let mut rows: Vec<f32> = Vec::new();
        let mut row_data: Vec<LaidRow> = Vec::new();
        let mut cursor_dist = 0.0;

        if let Some(live) = live {
            const MAX_LAYOUT_LINES_LIVE: usize = 2000;
            let all_lines: Vec<&str> = live.output.lines().collect();
            let skip = all_lines.len().saturating_sub(MAX_LAYOUT_LINES_LIVE);
            let live_lines: Vec<&str> = all_lines[skip..].to_vec();
            for line in live_lines.iter().rev() {
                let chunks: Rc<[String]> =
                    Rc::from(wrap_line_chunks(line, cols).collect::<Vec<_>>());
                let vis_rows = chunks.len();
                cursor_dist += vis_rows as f32 * pitch;
                rows.push(cursor_dist);
                row_data.push(LaidRow::Output {
                    text: line,
                    chunks,
                    block_id: None,
                });
            }
            cursor_dist += pitch;
            rows.push(cursor_dist);
            row_data.push(LaidRow::LiveCommand {
                command: live.command,
            });
            cursor_dist += pitch;
            rows.push(cursor_dist);
            row_data.push(LaidRow::Separator);
        }

        {
            let mut cache = self.block_layout_cache.borrow_mut();
            for b in blocks.iter() {
                cache.ensure_cached(b, cols);
            }
        }
        {
            let cache = self.block_layout_cache.borrow();
            for b in blocks.iter().rev() {
                let cached = cache.get(b.id.0);
                if !b.collapsed {
                    for line in cached.lines.iter().rev() {
                        let text = &b.output[line.byte_start..line.byte_end];
                        let vis_rows = line.chunks.len();
                        cursor_dist += vis_rows as f32 * pitch;
                        rows.push(cursor_dist);
                        row_data.push(LaidRow::Output {
                            text,
                            chunks: Rc::clone(&line.chunks),
                            block_id: Some(b.id),
                        });
                    }
                }
                cursor_dist += pitch;
                rows.push(cursor_dist);
                row_data.push(LaidRow::Command {
                    command: &b.command,
                    block_id: b.id,
                });
                cursor_dist += pitch;
                rows.push(cursor_dist);
                row_data.push(LaidRow::Header);
                cursor_dist += pitch;
                rows.push(cursor_dist);
                row_data.push(LaidRow::Separator);
                if b.command.split_whitespace().next() == Some("clear") {
                    cursor_dist += vp_h;
                    rows.push(cursor_dist);
                    row_data.push(LaidRow::Blank);
                }
            }
        }

        // Extract bv_rows from the layout data.
        // Mirrors build_block_view_vertices lines 229-302 (bv_rows portion only).
        let scroll_px = (block_scroll as f32) * pitch;
        let mut bv_rows = Vec::new();

        for (i, &dist) in rows.iter().enumerate() {
            let row_top_y = content_bottom_y - dist + scroll_px;
            match &row_data[i] {
                LaidRow::Output {
                    text,
                    chunks,
                    block_id,
                } => {
                    if chunks.len() <= 1 {
                        bv_rows.push(BlockViewRow {
                            kind: BlockViewRowKind::Output,
                            text: text.to_string(),
                            block_id: *block_id,
                            y_top: row_top_y,
                            y_bottom: row_top_y + pitch,
                        });
                    } else {
                        for (ci, chunk) in chunks.iter().enumerate() {
                            let cy = row_top_y + ci as f32 * pitch;
                            bv_rows.push(BlockViewRow {
                                kind: BlockViewRowKind::Output,
                                text: chunk.clone(),
                                block_id: *block_id,
                                y_top: cy,
                                y_bottom: cy + pitch,
                            });
                        }
                    }
                }
                LaidRow::Command { command, block_id } => {
                    bv_rows.push(BlockViewRow {
                        kind: BlockViewRowKind::Command,
                        text: command.to_string(),
                        block_id: Some(*block_id),
                        y_top: row_top_y,
                        y_bottom: row_top_y + pitch,
                    });
                }
                LaidRow::Header => {
                    bv_rows.push(BlockViewRow {
                        kind: BlockViewRowKind::Header,
                        text: String::new(),
                        block_id: None,
                        y_top: row_top_y,
                        y_bottom: row_top_y + pitch,
                    });
                }
                LaidRow::Separator => {
                    bv_rows.push(BlockViewRow {
                        kind: BlockViewRowKind::Separator,
                        text: String::new(),
                        block_id: None,
                        y_top: row_top_y,
                        y_bottom: row_top_y + pitch,
                    });
                }
                LaidRow::LiveCommand { command } => {
                    bv_rows.push(BlockViewRow {
                        kind: BlockViewRowKind::LiveCommand,
                        text: command.to_string(),
                        block_id: None,
                        y_top: row_top_y,
                        y_bottom: row_top_y + pitch,
                    });
                }
                LaidRow::Blank => {}
            }
        }

        bv_rows
    }

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

        if let Some(cwd) = cwd {
            if live.is_none() {
                let fixed_y = layout.fixed_cwd_y;
                push_quad(
                    &mut verts,
                    [left, fixed_y, right, fixed_y + 1.5],
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
        }

        enum LaidRow<'a> {
            Output {
                text: &'a str,
                chunks: Rc<[String]>,
                block_id: Option<BlockId>,
                line: usize,
            },
            Command {
                command: &'a str,
                collapsed: bool,
                foldable: bool,
                block_id: BlockId,
            },
            Header {
                text: String,
            },
            Separator,
            LiveCommand {
                command: &'a str,
            },
            Blank,
        }

        let mut rows: Vec<f32> = Vec::new();
        let mut row_data: Vec<LaidRow> = Vec::new();
        let mut cursor_dist = 0.0;

        if let Some(live) = live {
            const MAX_LAYOUT_LINES_LIVE: usize = 2000;
            let all_lines: Vec<&str> = live.output.lines().collect();
            let skip = all_lines.len().saturating_sub(MAX_LAYOUT_LINES_LIVE);
            let live_lines: Vec<&str> = all_lines[skip..].to_vec();
            let base_idx = skip;
            for (i, line) in live_lines.iter().enumerate().rev() {
                let line_idx = base_idx + i;
                let chunks: Rc<[String]> =
                    Rc::from(wrap_line_chunks(line, cols).collect::<Vec<_>>());
                let vis_rows = chunks.len();
                cursor_dist += vis_rows as f32 * pitch;
                rows.push(cursor_dist);
                row_data.push(LaidRow::Output {
                    text: line,
                    chunks,
                    block_id: None,
                    line: line_idx,
                });
            }
            cursor_dist += pitch;
            rows.push(cursor_dist);
            row_data.push(LaidRow::LiveCommand {
                command: live.command,
            });
            cursor_dist += pitch;
            rows.push(cursor_dist);
            row_data.push(LaidRow::Separator);
        }

        {
            let mut cache = self.block_layout_cache.borrow_mut();
            for b in blocks.iter() {
                cache.ensure_cached(b, cols);
            }
        }

        {
            let cache = self.block_layout_cache.borrow();
            for b in blocks.iter().rev() {
                let cached = cache.get(b.id.0);
                if !b.collapsed {
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
                        });
                    }
                }
                cursor_dist += pitch;
                rows.push(cursor_dist);
                row_data.push(LaidRow::Command {
                    command: &b.command,
                    collapsed: b.collapsed,
                    foldable: cached.foldable,
                    block_id: b.id,
                });
                let dur = block_duration_str(b);
                let bcwd = b
                    .cwd
                    .as_deref()
                    .map(abbreviate_path)
                    .unwrap_or_else(|| "~".to_string());
                let header = if dur.is_empty() {
                    bcwd
                } else {
                    format!("{bcwd} ({dur})")
                };
                cursor_dist += pitch;
                rows.push(cursor_dist);
                row_data.push(LaidRow::Header { text: header });
                cursor_dist += pitch;
                rows.push(cursor_dist);
                row_data.push(LaidRow::Separator);
                if b.command.split_whitespace().next() == Some("clear") {
                    cursor_dist += vp_h;
                    rows.push(cursor_dist);
                    row_data.push(LaidRow::Blank);
                }
            }
        }

        let scroll_px = (block_scroll as f32) * pitch;
        let clip_top = layout.clip_top;
        let clip_bottom = content_bottom_y;

        let mut topmost_block_info: Option<(String, String)> = None;

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
            let row_bottom_y = row_top_y + pitch;
            let _ = row_bottom_y; // unused (no clip in pre-pass)
            let y = row_top_y;
            match &row_data[i] {
                LaidRow::Output {
                    text,
                    chunks,
                    block_id,
                    line: _,
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
                LaidRow::Header { text: _ } => {
                    bv_rows.push(weft_core::selection::BlockViewRow {
                        kind: weft_core::selection::BlockViewRowKind::Header,
                        text: String::new(),
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
            let row_bottom_y = row_top_y + pitch;

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
                                let hl_bg = [0.95, 0.78, 0.20, 0.50];
                                push_quad(
                                    &mut verts,
                                    [hx0, y, hx1, y + ch],
                                    bg_uv,
                                    [0.0; 4],
                                    hl_bg,
                                );
                            }
                        }
                        self.push_text(&mut verts, left, y, text, fg, cols);
                    } else {
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
                                            let hl_bg = [0.95, 0.78, 0.20, 0.50];
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
                                self.push_text(&mut verts, left, cy, chunk, fg, cols);
                            }
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
                            let hl_bg = [0.95, 0.78, 0.20, 0.50];
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
                LaidRow::Header { text } => {
                    if row_in_selection(y + pitch * 0.5) {
                        push_quad(
                            &mut verts,
                            [left, y, right, y + pitch],
                            bg_uv,
                            [0.0; 4],
                            selection_bg,
                        );
                    }
                    self.push_text(&mut verts, left, y, text, dim, cols);
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
                }
                LaidRow::Blank => {}
            }

            if block_scroll > 0 && row_top_y <= clip_top + pitch {
                if let LaidRow::Command { command, .. } = &row_data[i] {
                    let cmd_str: &str = command;
                    for b in blocks.iter().rev() {
                        if b.command == cmd_str {
                            topmost_block_info = Some((
                                cmd_str.to_string(),
                                b.cwd.as_deref().map(abbreviate_path).unwrap_or_default(),
                            ));
                            break;
                        }
                    }
                }
            }
        }

        if block_scroll > 0 {
            if let Some((cmd, block_cwd)) = &topmost_block_info {
                let sticky_y = layout.clip_top;
                let sticky_bg = [
                    theme_bg[0] + (1.0 - theme_bg[0]) * 0.08,
                    theme_bg[1] + (1.0 - theme_bg[1]) * 0.08,
                    theme_bg[2] + (1.0 - theme_bg[2]) * 0.08,
                    1.0,
                ];
                push_quad(
                    &mut verts,
                    [0.0, sticky_y, vp_w, sticky_y + pitch],
                    bg_uv,
                    [0.0; 4],
                    sticky_bg,
                );
                push_quad(
                    &mut verts,
                    [0.0, sticky_y + pitch, vp_w, sticky_y + pitch + 1.0],
                    bg_uv,
                    [0.0; 4],
                    separator,
                );
                self.push_text(&mut verts, left, sticky_y, "❯ ", prompt_c, cols);
                let cmd_x = left + 2.0 * cw;
                let avail = cols.saturating_sub(2).max(1);
                self.push_line_tokenized(&mut verts, cmd_x, sticky_y, cmd, avail);
                if !block_cwd.is_empty() {
                    let cmd_cols = Self::text_col_width(cmd);
                    let cwd_x = cmd_x + (cmd_cols + 2) as f32 * cw;
                    let cwd_avail = cols.saturating_sub(2 + cmd_cols + 2).max(1);
                    self.push_text(&mut verts, cwd_x, sticky_y, block_cwd, dim, cwd_avail);
                }
            }
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
