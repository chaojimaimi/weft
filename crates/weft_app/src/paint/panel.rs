//! History panel vertex builder extracted from renderer.rs (A5).

use crate::paint::primitives::{color_to_normalized, push_quad};
use crate::paint::ui_helpers::{
    block_duration_str, panel_display, panel_filtered_count, strip_prompt_prefix, truncate_str,
    visible_panel_rows,
};
use crate::renderer::MetalRenderer;
use weft_core::blocks::{Block, BlockId};

/// What the sidebar history panel should draw. Built by the app only when the
/// panel is open and passed to [`crate::renderer::MetalRenderer::draw`].
pub struct PanelDrawParams<'a> {
    /// Finished blocks (oldest-first; the renderer shows newest first).
    pub blocks: &'a [Block],
    /// Panel width in physical pixels.
    pub width_px: f32,
    /// Live search filter (matches command or output, case-insensitive).
    pub query: &'a str,
    /// Index of the selected row within the newest-first filtered list.
    pub selection: usize,
    /// Id of the block whose output is expanded inline (None = all collapsed).
    pub expanded_id: Option<BlockId>,
    /// v0.9 fix: whether the search box has keyboard focus (draws accent
    /// underline so the user knows typing will go to the filter).
    pub search_focused: bool,
    /// F3-4: Block-level scroll offset (number of filtered blocks skipped
    /// from the newest end). 0 = newest visible.
    pub scroll_offset: usize,
}

impl MetalRenderer {
    /// Build vertices for the right-side history panel overlay: a translucent
    /// background, a search box, and one row per finished block (newest first,
    /// filtered by the query), color-coded by exit code. The selected row is
    /// highlighted and, if expanded, its output is shown beneath. Drawn after
    /// the grid so it composites on top via the enabled alpha blend.
    pub(crate) fn build_panel_vertices(&self, p: &PanelDrawParams) -> Vec<f32> {
        let cw = self.cell_width() as f32;
        let ch = self.cell_height() as f32;
        let _vp_w = self.viewport.0;
        let vp_h = self.viewport.1;
        let width_px = p.width_px;
        if width_px <= 0.0 || cw <= 0.0 || ch <= 0.0 {
            return Vec::new();
        }
        // v0.9 W5: panel is now a LEFT sidebar — anchor to the left edge.
        let panel_x = 0.0;
        let panel_cols = ((width_px / cw) as usize).max(1);
        let mut vertices = Vec::new();

        // v1.0 Warp-style: opaque semantic panel surface. UiColors keeps the
        // panel in the canvas luminance family for light/custom themes so
        // status text can remain readable on both surfaces.
        let theme_bg = color_to_normalized(self.theme.background);
        let ui = crate::ui_tokens::UiColors::from_theme(&self.theme);
        let panel_bg = color_to_normalized(ui.panel);
        let sel_bg = color_to_normalized(ui.selection);
        let selection_fg = color_to_normalized(ui.selection_text);
        let separator_color = color_to_normalized(self.theme.separator);
        let (su, sv, suw, svh) = self.space_uv();
        // V-swap to match grid rendering (CAMetalLayer flip compensation).
        let bg_uv = [su, sv + svh, su + suw, sv];
        // v0.9 W5: start the panel bg below the tab bar (chrome_top) so it
        // doesn't cover the tab bar, mirroring the content area.
        let chrome_top = self.layout_ctx.map(|c| c.chrome_top).unwrap_or(0.0);
        push_quad(
            &mut vertices,
            [panel_x, chrome_top, panel_x + width_px, vp_h],
            bg_uv,
            [0.0; 4],
            panel_bg,
        );

        // v0.9 fix: 1px separator on the right edge (Warp-style divider)
        // so the sidebar reads as a distinct surface, not floating content.
        push_quad(
            &mut vertices,
            [
                panel_x + width_px - 1.0,
                chrome_top,
                panel_x + width_px,
                vp_h,
            ],
            bg_uv,
            [0.0; 4],
            separator_color,
        );

        let fg = color_to_normalized(ui.text_primary);
        let dim = color_to_normalized(ui.text_secondary);
        let green = color_to_normalized(ui.success);
        let red = color_to_normalized(ui.error);

        // v0.9 fix: Warp-style search input field — a distinct rounded-look
        // box with its own background and border, so it reads as an input
        // field rather than blending into the panel. When focused, the border
        // turns accent color and a blinking cursor is drawn.
        //
        // v1.2 A4: geometry now comes from the shared `layout_panel` product
        // so the renderer and mouse handler stay in sync (no duplicated magic
        // numbers).
        let panel_layout = crate::layout::layout_panel(chrome_top, cw, ch, width_px, vp_h);
        let [field_x0, field_y0, field_x1, field_y1] = panel_layout.search_field_rect;
        // v0.9 fix: add a "History" header above the search field for a
        // clearer panel identity (Warp-style section title).
        let header_y = chrome_top + ch * 0.4;
        self.push_text(
            &mut vertices,
            panel_x + cw * 0.5,
            header_y,
            "History",
            fg,
            panel_cols,
        );
        // Input field background: slightly lighter than panel bg.
        let field_bg = [
            panel_bg[0] + (1.0 - panel_bg[0]) * 0.08,
            panel_bg[1] + (1.0 - panel_bg[1]) * 0.08,
            panel_bg[2] + (1.0 - panel_bg[2]) * 0.08,
            1.0,
        ];
        push_quad(
            &mut vertices,
            [field_x0, field_y0, field_x1, field_y1],
            bg_uv,
            [0.0; 4],
            field_bg,
        );
        // Border: accent when focused (was accent_dim — invisible in
        // Nord/Warp themes), separator otherwise.
        let border_color = if p.search_focused {
            color_to_normalized(self.theme.accent)
        } else {
            separator_color
        };
        let border_w = if p.search_focused { 2.0 } else { 1.0 };
        // Top border
        push_quad(
            &mut vertices,
            [field_x0, field_y0, field_x1, field_y0 + border_w],
            bg_uv,
            [0.0; 4],
            border_color,
        );
        // Bottom border
        push_quad(
            &mut vertices,
            [field_x0, field_y1 - border_w, field_x1, field_y1],
            bg_uv,
            [0.0; 4],
            border_color,
        );
        // Left border
        push_quad(
            &mut vertices,
            [field_x0, field_y0, field_x0 + border_w, field_y1],
            bg_uv,
            [0.0; 4],
            border_color,
        );
        // Right border
        push_quad(
            &mut vertices,
            [field_x1 - border_w, field_y0, field_x1, field_y1],
            bg_uv,
            [0.0; 4],
            border_color,
        );

        // Text inside the field: show query, or placeholder "Search…" when empty.
        let text_y = field_y0 + (field_y1 - field_y0 - ch) * 0.5;
        let text_x = field_x0 + cw * 0.4;
        let text_cols = ((field_x1 - text_x - cw * 0.4) / cw) as usize;
        if p.query.is_empty() {
            self.push_text(&mut vertices, text_x, text_y, "Search…", dim, text_cols);
        } else {
            self.push_text(&mut vertices, text_x, text_y, p.query, fg, text_cols);
        }

        // Blinking cursor at the end of the query text when focused.
        if p.search_focused {
            let blink_phase = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() % 1200)
                .unwrap_or(0);
            if blink_phase < 600 {
                let query_w = p.query.chars().count() as f32 * cw;
                let cursor_x = text_x + query_w;
                let cursor_w = 2.0_f32.max(cw * 0.12);
                push_quad(
                    &mut vertices,
                    [cursor_x, text_y, cursor_x + cursor_w, text_y + ch],
                    bg_uv,
                    [0.0; 4],
                    fg,
                );
            }
        }

        // Display list: newest-first, filtered by query, virtualized via
        // scroll_offset (skip + take).
        let max_rows = visible_panel_rows(vp_h, self.cell_height());
        let total_filtered = panel_filtered_count(p.blocks, p.query);
        let max_scroll = total_filtered.saturating_sub(max_rows);
        let scroll_offset = p.scroll_offset.min(max_scroll);
        let display = panel_display(p.blocks, p.query, scroll_offset, max_rows);
        let row_h = panel_layout.row_height;
        let mut y = panel_layout.list_top;
        let mut drawn = 0usize;

        for (i, block) in display.iter().enumerate() {
            if drawn >= max_rows || y + ch > vp_h {
                break;
            }
            let selected = i == p.selection;
            if selected {
                push_quad(
                    &mut vertices,
                    [panel_x, y - ch * 0.1, panel_x + width_px, y + ch],
                    bg_uv,
                    [0.0; 4],
                    sel_bg,
                );
            }
            let cmd_color = if selected {
                selection_fg
            } else {
                match block.exit_code {
                    Some(0) => green,
                    Some(_) => red,
                    None => dim,
                }
            };
            let dur = block_duration_str(block);
            let dur_len = dur.chars().count();
            let cmd_cols = panel_cols.saturating_sub(dur_len + 2).max(1);
            // v0.9 fix: strip prompt prefix so old blocks (captured via
            // snapshot_command_line) show just the command, matching the
            // clean format of editor-submitted commands.
            let cleaned = strip_prompt_prefix(&block.command);
            let label = truncate_str(&cleaned, cmd_cols);
            self.push_text(
                &mut vertices,
                panel_x + cw * 0.5,
                y,
                &label,
                cmd_color,
                cmd_cols,
            );
            if !dur.is_empty() {
                let dur_x = panel_x + width_px - cw * 0.5 - dur_len as f32 * cw;
                let dur_color = if selected { selection_fg } else { dim };
                self.push_text(&mut vertices, dur_x, y, &dur, dur_color, dur_len + 1);
            }
            y += row_h;
            drawn += 1;

            // Expanded output for the selected block.
            if Some(block.id) == p.expanded_id {
                let out_cols = panel_cols.saturating_sub(2).max(1);
                for line in block.output.lines().take(8) {
                    if drawn >= max_rows || y + ch > vp_h {
                        break;
                    }
                    let rendered = truncate_str(line, out_cols);
                    self.push_text(
                        &mut vertices,
                        panel_x + cw * 1.5,
                        y,
                        &rendered,
                        dim,
                        out_cols,
                    );
                    y += row_h;
                    drawn += 1;
                }
            }
        }

        // F3-4: Scrollbar indicator on the right edge of the panel list area.
        // Only drawn when there are more filtered blocks than visible (i.e.
        // the list can scroll).
        if total_filtered > max_rows {
            let track_x = panel_x + width_px - 3.0;
            let track_w = 2.0;
            let track_y0 = panel_layout.list_top;
            let track_y1 = vp_h;
            let track_h = track_y1 - track_y0;
            // Track background (subtle).
            let track_bg = [
                theme_bg[0] * 0.5 + fg[0] * 0.1,
                theme_bg[1] * 0.5 + fg[1] * 0.1,
                theme_bg[2] * 0.5 + fg[2] * 0.1,
                0.50,
            ];
            push_quad(
                &mut vertices,
                [track_x, track_y0, track_x + track_w, track_y1],
                bg_uv,
                [0.0; 4],
                track_bg,
            );
            // Thumb: proportional height, positioned by scroll_offset.
            let thumb_frac = max_rows as f32 / total_filtered as f32;
            let thumb_h = (track_h * thumb_frac).max(ch * 0.8);
            let pos_frac = if max_scroll > 0 {
                scroll_offset as f32 / max_scroll as f32
            } else {
                0.0
            };
            let avail = (track_h - thumb_h).max(0.0);
            let thumb_y0 = track_y0 + avail * pos_frac;
            let thumb_color = color_to_normalized(self.theme.accent);
            push_quad(
                &mut vertices,
                [track_x, thumb_y0, track_x + track_w, thumb_y0 + thumb_h],
                bg_uv,
                [0.0; 4],
                [thumb_color[0], thumb_color[1], thumb_color[2], 0.70],
            );
            // Up arrow indicator when not at the newest.
            if scroll_offset > 0 {
                self.push_text(&mut vertices, track_x - cw * 0.5, track_y0, "▲", dim, 2);
            }
            // Down arrow indicator when not at the oldest.
            if scroll_offset < max_scroll {
                self.push_text(
                    &mut vertices,
                    track_x - cw * 0.5,
                    track_y1 - ch,
                    "▼",
                    dim,
                    2,
                );
            }
        }

        vertices
    }
}
