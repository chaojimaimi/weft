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
    /// v1.12.26 (P1-03): active IME composition for the search box, rendered
    /// inline after the query (palette/find precedent).
    pub panel_ime_preedit: &'a str,
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
        let ui = crate::ui_tokens::UiColors::from_theme(&self.theme)
            .with_increase_contrast(self.increase_contrast);
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

        // F6: Focus ring — an additional accessibility indicator drawn
        // slightly outside the field bounds when the search input has
        // keyboard focus. Thickness/alpha scale with Increase Contrast.
        if p.search_focused {
            use crate::paint::primitives::{
                build_focus_ring, focus_ring_alpha, focus_ring_thickness,
            };
            let accent = color_to_normalized(self.theme.accent);
            let ring_color = [
                accent[0],
                accent[1],
                accent[2],
                focus_ring_alpha(self.increase_contrast),
            ];
            build_focus_ring(
                &mut vertices,
                [
                    field_x0 - 2.0,
                    field_y0 - 2.0,
                    field_x1 + 2.0,
                    field_y1 + 2.0,
                ],
                ring_color,
                focus_ring_thickness(self.increase_contrast),
            );
        }

        // Text inside the field: show query, or placeholder "Search…" when empty.
        // v1.12.26 (P1-03): the text origin comes from PanelLayout so the
        // draw path and the panel IME anchor (`panel_ime_area`) read ONE
        // formula and cannot drift.
        let text_y = panel_layout.search_text_y;
        let text_x = panel_layout.search_text_x;
        let text_cols = ((field_x1 - text_x - cw * 0.4) / cw) as usize;
        // v1.12.27a (P1-06②): the caret/preedit x used to ride the FULL
        // query width while the drawn text is column-budgeted by push_text,
        // so an over-long query pushed the caret past the field's right
        // edge. Find-overlay `query_display` precedent (paint/overlays.rs):
        // show the query tail under a "…" prefix within a budget that
        // reserves 1 col for the caret + the active preedit, then drive the
        // caret x and the preedit budget from the SHOWN width.
        let preedit_cells = MetalRenderer::text_col_width(p.panel_ime_preedit);
        let query_budget = text_cols.saturating_sub(1).saturating_sub(preedit_cells);
        let (query_display, query_cols) = panel_query_display(p.query, query_budget);
        let mut cursor_x = text_x + query_cols as f32 * cw;
        if p.query.is_empty() && p.panel_ime_preedit.is_empty() {
            // v1.12.26 (P1-03): the placeholder only yields to query OR live
            // composition — an active preedit draws in its place, never
            // underneath the dim "Search…" ghost.
            self.push_text(&mut vertices, text_x, text_y, "Search…", dim, text_cols);
        } else {
            self.push_text(&mut vertices, text_x, text_y, &query_display, fg, text_cols);
        }

        // v1.12.26 (P1-03): IME preedit inline after the query (palette/
        // find precedent), accent-colored + underlined. Width math is
        // text_col_width-based (N-4 lesson: char-count math drew CJK carets
        // at half extent); over-wide compositions truncate via push_text's
        // column budget, and the caret below rides the SHOWN width.
        let accent = color_to_normalized(self.theme.accent);
        if !p.panel_ime_preedit.is_empty() {
            let preedit_max = text_cols.saturating_sub(query_cols);
            if preedit_max > 0 {
                let shown = preedit_cells.min(preedit_max) as f32;
                self.push_text(
                    &mut vertices,
                    cursor_x,
                    text_y,
                    p.panel_ime_preedit,
                    accent,
                    preedit_max,
                );
                let underline = [
                    cursor_x,
                    text_y + ch - 1.0,
                    cursor_x + shown * cw,
                    text_y + ch,
                ];
                push_quad(&mut vertices, underline, bg_uv, [0.0; 4], accent);
                cursor_x += shown * cw;
            }
        }

        // Blinking cursor at the end of the query text when focused.
        // v1.12.26 (P1-01): the caret follows the renderer-global blink
        // phase (`cursor_blink_on`), matching palette/note — the old
        // private wall-clock phase never repainted when no redraw was
        // scheduled, freezing the caret on its last frame.
        if p.search_focused && self.cursor_blink_on {
            let cursor_w = 2.0_f32.max(cw * 0.12);
            push_quad(
                &mut vertices,
                [cursor_x, text_y, cursor_x + cursor_w, text_y + ch],
                bg_uv,
                [0.0; 4],
                fg,
            );
        }

        // Display list: newest-first, filtered by query, virtualized via
        // scroll_offset (skip + take).
        let max_rows = visible_panel_rows(vp_h, self.cell_height());
        let total_filtered = panel_filtered_count(p.blocks, p.query);
        let max_scroll = total_filtered.saturating_sub(max_rows);
        // Batch 5 Step 2: cache metrics for active_panel_scrollbar_layout
        // (mouse handlers). Written here once per frame; read on every mouse
        // move to avoid re-running panel_filtered_count (O(n) over all blocks).
        self.cached_panel_scroll_metrics
            .set(Some((total_filtered, max_rows, max_scroll)));
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
        if let Some(scrollbar) = crate::panel_scrollbar::panel_scrollbar_layout(
            panel_layout.panel_rect,
            panel_layout.list_top,
            total_filtered,
            max_rows,
            scroll_offset,
            ch * 0.8,
        ) {
            let track_x = scrollbar.track[0];
            let track_w = scrollbar.track[2] - scrollbar.track[0];
            let track_y0 = scrollbar.track[1];
            let track_y1 = scrollbar.track[3];
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
            let thumb_color = color_to_normalized(self.theme.accent);
            push_quad(
                &mut vertices,
                scrollbar.thumb,
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

        // v1.11.2 X4 (PLAN_v1112 §1.3): footer「加载更早」button — paged
        // history out of SQLite when retention has evicted older blocks.
        // Hidden entirely when the tab has no blocks (PLAN_v1112 §8: the
        // button must not render for an empty list). Shares footer_rect with
        // the scene builder so paint and hit-testing cannot drift.
        if !p.blocks.is_empty() {
            let [fx0, fy0, fx1, fy1] = panel_layout.footer_rect;
            // Hairline separator above the footer so it reads as a distinct
            // action zone, not a stray row.
            push_quad(
                &mut vertices,
                [
                    panel_x + cw * 0.5,
                    fy0 - ch * 0.25,
                    panel_x + width_px - cw * 0.5,
                    fy0 - ch * 0.25 + 1.0,
                ],
                bg_uv,
                [0.0; 4],
                separator_color,
            );
            let label = "加载更早";
            // Reviewer Minor-7: CJK glyphs render at double cell width —
            // count terminal columns, not chars, or the label centers
            // ~one CJK width off to the right.
            let label_cols: usize = label
                .chars()
                .map(weft_core::grid::terminal_char_width)
                .sum();
            let label_w = label_cols as f32 * cw;
            let center_x = (fx0 + fx1) * 0.5 - label_w * 0.5;
            let center_y = (fy0 + fy1) * 0.5 - ch * 0.5;
            self.push_text(
                &mut vertices,
                center_x,
                center_y,
                label,
                dim,
                ((fx1 - fx0) / cw) as usize,
            );
        }

        vertices
    }
}

/// v1.12.27a (P1-06②): the panel search field's shown query text and its
/// displayed column width. The caret sits at the END of the query, so an
/// over-long query keeps the tail under a "…" prefix (the find overlay's
/// `query_display` precedent, paint/overlays.rs). The returned width drives
/// the caret x and the IME preedit budget so both stay inside the field's
/// right edge — the old full-width caret math escaped the field for long
/// queries.
fn panel_query_display(query: &str, budget_cols: usize) -> (String, usize) {
    let full_w = MetalRenderer::text_col_width(query);
    if full_w <= budget_cols {
        return (query.to_string(), full_w);
    }
    use unicode_segmentation::UnicodeSegmentation;
    let mut kept: Vec<&str> = Vec::new();
    let mut w = MetalRenderer::text_col_width("…");
    for grapheme in query.graphemes(true).rev() {
        let width = MetalRenderer::text_col_width(grapheme);
        if w + width > budget_cols {
            break;
        }
        kept.push(grapheme);
        w += width;
    }
    kept.reverse();
    let mut s = String::from("…");
    for grapheme in kept {
        s.push_str(grapheme);
    }
    (s, w)
}

#[cfg(test)]
mod tests {
    use super::*;

    // v1.12.27a (P1-06②): the caret x is `text_x + shown_cols * cw`, so the
    // shown width saturating (never exceeding) the call-site budget — which
    // already reserves the caret col out of `text_cols` — is exactly the
    // "caret stays inside the field's right edge" contract.
    #[test]
    fn overlong_query_shown_width_stays_within_budget() {
        let budget = 19; // text_cols 20 minus the reserved caret col
        let ascii = "a".repeat(60);
        let (shown, cols) = panel_query_display(&ascii, budget);
        assert_eq!(cols, budget, "tail truncation saturates the budget");
        assert!(shown.starts_with('…'));
        assert_eq!(MetalRenderer::text_col_width(&shown), cols);

        let cjk = "中".repeat(40);
        let (shown, cols) = panel_query_display(&cjk, budget);
        assert!(cols <= budget, "CJK query never exceeds the budget");
        assert!(shown.starts_with('…'));
        assert_eq!(MetalRenderer::text_col_width(&shown), cols);
    }

    // Short queries pass through verbatim — the common path is unchanged.
    #[test]
    fn short_query_passes_through_verbatim() {
        let (shown, cols) = panel_query_display("vim", 19);
        assert_eq!(shown, "vim");
        assert_eq!(cols, 3);
    }

    // A tail grapheme that cannot fit is dropped whole, never overpainted.
    #[test]
    fn wide_tail_grapheme_that_does_not_fit_is_dropped() {
        let (shown, cols) = panel_query_display("ab中", 3);
        assert_eq!(shown, "…中");
        assert_eq!(cols, 3);
        let (shown, cols) = panel_query_display("中", 1);
        assert_eq!(shown, "…");
        assert_eq!(cols, 1);
    }
}
