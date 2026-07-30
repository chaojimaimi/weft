//! Prompt input box painter — the bottom editor area with a `>` prompt,
//! editor buffer lines, cursor bar, IME preedit, and Ctrl+R search UI.
//!
//! Extracted from `renderer.rs` (M2) to keep the main file focused on
//! GPU pipeline + frame orchestration. Still `impl MetalRenderer` because
//! it needs glyph atlas access via `push_text` / `push_line_tokenized`.

use crate::paint::primitives::{
    color_to_normalized, composite_color_over, push_quad, scale_color_alpha,
};
use crate::renderer::MetalRenderer;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PromptVisualRow {
    pub(crate) source_line: usize,
    pub(crate) char_start: usize,
    pub(crate) char_end: usize,
    pub(crate) text: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PromptVisualLayout {
    pub(crate) rows: Vec<PromptVisualRow>,
    pub(crate) cursor_row: usize,
    pub(crate) cursor_display_col: usize,
}

/// Soft-wrap editor lines for display without inserting newlines into the
/// command sent to the shell. The first visual row reserves two columns for
/// the prompt glyph; continuation rows use the pane's full width.
pub(crate) fn prompt_visual_layout(
    lines: &[String],
    cursor: (usize, usize),
    box_cols: usize,
) -> PromptVisualLayout {
    let mut rows = Vec::new();
    let source_lines: &[String] = if lines.is_empty() { &[] } else { lines };
    if source_lines.is_empty() {
        rows.push(PromptVisualRow {
            source_line: 0,
            char_start: 0,
            char_end: 0,
            text: String::new(),
        });
    }
    for (line_index, line) in source_lines.iter().enumerate() {
        let chars: Vec<char> = line.chars().collect();
        let mut start = 0usize;
        let mut width = 0usize;
        let mut capacity = if line_index == 0 {
            box_cols.saturating_sub(2).max(1)
        } else {
            box_cols.max(1)
        };
        for (index, ch) in chars.iter().copied().enumerate() {
            let char_width = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
            if index > start && width + char_width > capacity {
                rows.push(PromptVisualRow {
                    source_line: line_index,
                    char_start: start,
                    char_end: index,
                    text: chars[start..index].iter().collect(),
                });
                start = index;
                width = 0;
                capacity = box_cols.max(1);
            }
            width += char_width;
        }
        rows.push(PromptVisualRow {
            source_line: line_index,
            char_start: start,
            char_end: chars.len(),
            text: chars[start..].iter().collect(),
        });
        let cursor_at_final_line_end = line_index + 1 == source_lines.len()
            && cursor.0 == line_index
            && cursor.1 == chars.len();
        if !chars.is_empty() && width == capacity && cursor_at_final_line_end {
            rows.push(PromptVisualRow {
                source_line: line_index,
                char_start: chars.len(),
                char_end: chars.len(),
                text: String::new(),
            });
        }
    }

    let cursor_line = cursor.0.min(source_lines.len().saturating_sub(1));
    let cursor_char = cursor.1.min(
        source_lines
            .get(cursor_line)
            .map(|line| line.chars().count())
            .unwrap_or(0),
    );
    let cursor_row = rows
        .iter()
        .enumerate()
        .rev()
        .find(|(_, row)| {
            row.source_line == cursor_line
                && row.char_start <= cursor_char
                && cursor_char <= row.char_end
        })
        .map(|(index, _)| index)
        .unwrap_or(0);
    let cursor_display_col = rows
        .get(cursor_row)
        .and_then(|row| source_lines.get(row.source_line).map(|line| (row, line)))
        .map(|(row, line)| {
            line.chars()
                .skip(row.char_start)
                .take(cursor_char.saturating_sub(row.char_start))
                .map(|ch| unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0))
                .sum()
        })
        .unwrap_or(0);
    PromptVisualLayout {
        rows,
        cursor_row,
        cursor_display_col,
    }
}

pub(crate) fn prompt_layout_for_buffer(
    ctx: &crate::layout::LayoutCtx,
    lines: &[String],
    cursor: (usize, usize),
) -> (PromptVisualLayout, crate::layout::PromptLayout, usize) {
    let box_cols = crate::layout::prompt_content_cols(ctx);
    let visual = prompt_visual_layout(lines, cursor, box_cols);
    let initial = crate::layout::layout_prompt(
        ctx,
        visual.rows.len(),
        visual.cursor_row,
        visual.cursor_display_col,
        0,
    );
    let scroll = visual
        .cursor_row
        .saturating_add(1)
        .saturating_sub(initial.visible_rows);
    let layout = crate::layout::layout_prompt(
        ctx,
        visual.rows.len(),
        visual.cursor_row,
        visual.cursor_display_col,
        scroll,
    );
    (visual, layout, scroll)
}

impl MetalRenderer {
    /// Build vertices for the bottom editor input box (v0.5 editor takeover):
    /// a translucent panel pinned to the bottom, a `>` prompt, the editor
    /// buffer lines, a cursor bar, and the Ctrl+R search UI when active. Drawn
    /// after the grid so it composites on top via the enabled alpha blend.
    pub(crate) fn build_prompt_vertices(
        &self,
        p: &PromptDrawParams,
        cursor_blink_phase: f32,
        cursor_blink_on: bool,
    ) -> Vec<f32> {
        let mut verts = Vec::new();
        let cw = self.cell_width() as f32;
        let ch = self.cell_height() as f32;
        let vp_w = self.viewport.0;
        let vp_h = self.viewport.1;
        if cw <= 0.0 || ch <= 0.0 || vp_w <= 0.0 || vp_h <= 0.0 {
            return verts;
        }

        // v0.8 stage 4: layout (box rect, text_y0, cursor X/Y, bar_w) is
        // computed by the pure function in `layout.rs`. The renderer keeps
        // responsibility for vertex building, theming, and text rasterization.
        let ctx = self.layout_ctx.expect("LayoutCtx built at draw() entry");
        let box_cols = crate::layout::prompt_content_cols(&ctx);
        let visual = prompt_visual_layout(p.lines, p.cursor, box_cols);
        let initial_layout = crate::layout::layout_prompt(
            &ctx,
            visual.rows.len(),
            visual.cursor_row,
            visual.cursor_display_col,
            0,
        );
        let visual_scroll = visual
            .cursor_row
            .saturating_add(1)
            .saturating_sub(initial_layout.visible_rows);
        let _source_scroll_offset = p.scroll_offset;
        let layout = crate::layout::layout_prompt(
            &ctx,
            visual.rows.len(),
            visual.cursor_row,
            visual.cursor_display_col,
            visual_scroll,
        );
        let text_y0 = layout.text_y0;
        let left = layout.left;
        let box_cols = layout.box_cols;
        let first_line_text_x = layout.first_line_text_x;
        let cx = layout.cursor_x;
        let cy = layout.cursor_y;
        let bar_w = layout.bar_w;
        let visible_rows = layout.visible_rows;
        let scroll_offset = visual_scroll;

        let theme_bg = scale_color_alpha(color_to_normalized(self.theme.background), self.opacity);
        // The input area uses the SAME background as the window (Warp style —
        // no distinct input panel). Its alpha follows the window opacity so
        // the prompt does not mask transparency applied by the Metal clear.
        let box_bg = theme_bg;
        let fg = color_to_normalized(self.theme.foreground);
        let accent = color_to_normalized(self.theme.cursor);
        let (su, sv, suw, svh) = self.space_uv();
        // V-swap to match grid rendering (CAMetalLayer flip compensation).
        let bg_uv = [su, sv + svh, su + suw, sv];

        // Uniform window background for the input area (no distinct panel).
        push_quad(&mut verts, layout.box_rect, bg_uv, [0.0; 4], box_bg);

        // Ctrl+R search UI replaces the normal prompt.
        if let Some((query, selected)) = p.search {
            let label = "search: ";
            self.push_text(&mut verts, left, text_y0, label, fg, box_cols);
            let qx = left + label.chars().count() as f32 * cw;
            self.push_text(&mut verts, qx, text_y0, query, accent, box_cols);
            if let Some(m) = selected {
                self.push_text(&mut verts, left, text_y0 + ch, m, fg, box_cols);
            }
            return verts;
        }

        // Prompt glyph only — cwd lives in the block history, not the input
        // box (Warp style), so the typed command never runs into the path.
        // Use accent (not accent_dim) for the prompt marker. It is a
        // primary UI element, not dim chrome, so accent is appropriate.
        // Keep the marker in the configured monospace font. The previous
        // heavy-angle glyph commonly fell back to a symbol font whose visual
        // baseline sat below the first command row.
        let prompt_str = "> ";
        let prompt_chars = 2;
        let prompt_c = color_to_normalized(self.theme.accent);
        if scroll_offset == 0 {
            self.push_text(
                &mut verts,
                left,
                text_y0,
                prompt_str,
                prompt_c,
                prompt_chars,
            );
        }

        // Editor buffer lines (line 0 starts after the prompt).
        // v0.9: draw a selection highlight for the active mouse-drag range.
        // v1.0 fix: accent-based blend for selection visibility across all themes.
        let sel_bg = {
            let accent = color_to_normalized(self.theme.accent);
            let bg = color_to_normalized(self.theme.background);
            [
                accent[0] * 0.35 + bg[0] * 0.65,
                accent[1] * 0.35 + bg[1] * 0.65,
                accent[2] * 0.35 + bg[2] * 0.65,
                0.60,
            ]
        };
        let selection_canvas = composite_color_over(sel_bg, box_bg);
        let mut visual_selection_ranges = vec![None; visual.rows.len()];
        if let Some(((sl, sc), (el, ec))) = p.selection {
            // F2 P0-1: only render selection highlight for visible lines,
            // adjusting Y by scroll_offset.
            let vis_end = (scroll_offset + visible_rows).min(visual.rows.len());
            for (i, selection_range) in visual_selection_ranges
                .iter_mut()
                .enumerate()
                .take(vis_end)
                .skip(scroll_offset)
            {
                let row = &visual.rows[i];
                if row.source_line < sl || row.source_line > el {
                    continue;
                }
                let line = &row.text;
                let y = text_y0 + (i - scroll_offset) as f32 * ch;
                let (line_start_x, max_chars) = if i == 0 {
                    let avail = box_cols.saturating_sub(prompt_chars).max(1);
                    (first_line_text_x, avail)
                } else {
                    (left, box_cols)
                };
                // Char column range within this line.
                let source_start = if row.source_line == sl { sc } else { 0 };
                let source_end = if row.source_line == el {
                    ec
                } else {
                    usize::MAX
                };
                let col_start = source_start.max(row.char_start) - row.char_start;
                let col_end = source_end.min(row.char_end).saturating_sub(row.char_start);
                if col_start >= col_end {
                    continue;
                }
                *selection_range = Some((col_start, col_end));
                // Convert char columns → display columns (CJK = 2 cells).
                let chars: Vec<char> = line.chars().collect();
                let disp_start: usize = chars
                    .iter()
                    .take(col_start)
                    .map(|c| Self::char_col_width(*c))
                    .sum();
                let disp_len: usize = chars
                    .iter()
                    .skip(col_start)
                    .take(col_end - col_start)
                    .map(|c| Self::char_col_width(*c))
                    .sum();
                let disp_len = disp_len.min(max_chars.saturating_sub(disp_start));
                if disp_len > 0 {
                    let x0 = line_start_x + disp_start as f32 * cw;
                    push_quad(
                        &mut verts,
                        [x0, y, x0 + disp_len as f32 * cw, y + ch],
                        bg_uv,
                        [0.0; 4],
                        sel_bg,
                    );
                }
            }
        }
        // F2 P0-1: only render the visible window [scroll_offset, scroll_offset +
        // visible_rows), adjusting each line's Y by scroll_offset.
        let vis_end = (scroll_offset + visible_rows).min(visual.rows.len());
        for (i, selection_range) in visual_selection_ranges
            .iter()
            .enumerate()
            .take(vis_end)
            .skip(scroll_offset)
        {
            let line = &visual.rows[i].text;
            let y = text_y0 + (i - scroll_offset) as f32 * ch;
            let (start_x, max_chars) = if i == 0 {
                let avail = box_cols.saturating_sub(prompt_chars).max(1);
                (first_line_text_x, avail)
            } else {
                (left, box_cols)
            };
            let selection = selection_range.map(|(start, end)| (start, end, selection_canvas));
            self.push_line_tokenized_on_canvas(
                &mut verts,
                [start_x, y],
                line,
                max_chars,
                box_bg,
                selection,
            );
        }

        // ── v0.8 signature: warm cursor breath + amber glow ─────────────
        // Smooth sin() alpha over a 2400ms period (phase in radians).
        // sin maps [0, 2π) → [-1, 1]; we remap to [0.25, 1.0] so the caret
        // never fully disappears (calmer than hard on/off). The glow halo
        // is a wider, very-low-alpha amber quad behind the caret that
        // breathes in sync (peaks at ~0.25 alpha).
        //
        // When `cursor_blink_on` is false (window unfocused OR user is
        // actively selecting — see main.rs::RedrawRequested), the breath
        // freezes at peak alpha so the caret stays visible but calm.
        if !p.focused {
            return verts;
        }

        let s = if cursor_blink_on {
            cursor_blink_phase.sin()
        } else {
            1.0_f32
        };
        let caret_alpha = 0.625 + 0.375 * s; // → [0.25, 1.0]
        let glow_alpha = 0.15 + 0.10 * s; // → [0.05, 0.25]
        let accent_color = [accent[0], accent[1], accent[2], accent[3] * caret_alpha];
        // Glow: a wider quad (~3× bar width, full cell height) behind the
        // caret. Drawn first so the caret composites on top.
        let glow_pad = bar_w * 1.5;
        let glow_color = [accent[0], accent[1], accent[2], glow_alpha.max(0.0)];
        push_quad(
            &mut verts,
            [cx - glow_pad, cy, cx + bar_w + glow_pad, cy + ch],
            bg_uv,
            [0.0; 4],
            glow_color,
        );
        // Caret itself.
        push_quad(
            &mut verts,
            [cx, cy, cx + bar_w, cy + ch],
            bg_uv,
            [0.0; 4],
            accent_color,
        );

        // IME preedit right after the cursor.
        if let Some(preedit) = p.preedit {
            if !preedit.is_empty() {
                let preedit_x = cx + bar_w;
                self.push_text(&mut verts, preedit_x, cy, preedit, accent, box_cols);

                // F2 P1-4: render the composition caret within the preedit
                // string using the byte-range cursor from `Ime::Preedit`.
                // Lightweight version: a thin caret bar at `start` when
                // `start == end`, or a translucent highlight over `[start, end)`.
                if let Some((start, end)) = p.preedit_cursor {
                    let (start_clamped, end_clamped) = normalize_preedit_range(preedit, start, end);
                    // Convert byte offsets → display column widths.
                    let prefix_str = &preedit[..start_clamped];
                    let prefix_cols: usize = prefix_str.chars().map(Self::char_col_width).sum();
                    let caret_x = preedit_x + prefix_cols as f32 * cw;
                    if start_clamped == end_clamped {
                        // Thin caret bar.
                        let preedit_bar_w = (cw * 0.12).max(2.0);
                        push_quad(
                            &mut verts,
                            [caret_x, cy, caret_x + preedit_bar_w, cy + ch],
                            bg_uv,
                            [0.0; 4],
                            accent,
                        );
                    } else {
                        // Highlight the selected range within the preedit.
                        let sel_str = &preedit[start_clamped..end_clamped];
                        let sel_cols: usize = sel_str.chars().map(Self::char_col_width).sum();
                        if sel_cols > 0 {
                            let sel_color = [
                                accent[0] * 0.35 + theme_bg[0] * 0.65,
                                accent[1] * 0.35 + theme_bg[1] * 0.65,
                                accent[2] * 0.35 + theme_bg[2] * 0.65,
                                0.70,
                            ];
                            push_quad(
                                &mut verts,
                                [caret_x, cy, caret_x + sel_cols as f32 * cw, cy + ch],
                                bg_uv,
                                [0.0; 4],
                                sel_color,
                            );
                        }
                    }
                }
            }
        }

        // F2 P1-3: lightweight hint line at the bottom pad row of the prompt
        // box. Shows the Enter / Shift+Enter (or Ctrl+Enter / Enter) key
        // semantics. Dim color, more prominent when multi-line input is active.
        let hint_y = layout.box_rect[3] - ch;
        if let Some(hint_text) = prompt_hint_text(p.submit_on_ctrl_enter, p.lines.len()) {
            let ui = crate::ui_tokens::UiColors::from_theme(&self.theme)
                .with_increase_contrast(self.increase_contrast);
            let dim_c = color_to_normalized(ui.text_secondary);
            // Multi-line input makes the hint slightly more visible (0.70
            // alpha vs 0.45) since the user is actively composing a multi-line
            // command and the key semantics matter more.
            let alpha = if p.lines.len() > 1 { 0.70 } else { 0.45 };
            let hint_color = [dim_c[0], dim_c[1], dim_c[2], alpha];
            let hint_cols = Self::text_col_width(hint_text);
            self.push_text(&mut verts, left, hint_y, hint_text, hint_color, hint_cols);
        }

        verts
    }

    /// Push a selection-highlight background quad for a character range of
    /// `text`, honoring CJK double-width so the highlight exactly covers the
    /// selected glyphs. Called before `push_text` so the text renders on top
    /// of the highlight (matching grid-view selection rendering).
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn push_block_view_highlight(
        &self,
        vertices: &mut Vec<f32>,
        x_left: f32,
        y_top: f32,
        height: f32,
        text: &str,
        c_start: usize,
        c_end: usize,
        bg_color: [f32; 4],
        bg_uv: [f32; 4],
    ) {
        if c_end <= c_start || text.is_empty() {
            return;
        }
        let cw = self.cell_width() as f32;
        let prefix = text.chars().take(c_start).collect::<String>();
        let selected = text
            .chars()
            .skip(c_start)
            .take(c_end - c_start)
            .collect::<String>();
        let x_start = x_left + weft_core::grid::terminal_text_width(&prefix) as f32 * cw;
        let cell_w = weft_core::grid::terminal_text_width(&selected) as f32 * cw;
        if cell_w > 0.0 {
            push_quad(
                vertices,
                [x_start, y_top, x_start + cell_w, y_top + height],
                bg_uv,
                [0.0; 4], // fg mask: no text contribution (pure background)
                bg_color,
            );
        }
    }
}

/// What the bottom editor input box should draw (v0.5 editor takeover). Built
/// by the app only in Editor mode and passed to [`MetalRenderer::draw`].
pub struct PromptDrawParams<'a> {
    /// Whether this pane owns keyboard focus. Background panes retain their
    /// editor contents but do not paint a caret.
    pub focused: bool,
    /// Current working directory (from OSC 7); rendered in the history band.
    pub cwd: Option<&'a str>,
    /// Editor buffer lines (line 0 follows the prompt).
    pub lines: &'a [String],
    /// Cursor position (line index, char column).
    pub cursor: (usize, usize),
    /// Active IME preedit string, drawn right after the cursor.
    pub preedit: Option<&'a str>,
    /// F2 P1-4: Preedit cursor byte range `(start, end)` within `preedit`.
    /// `None` hides the composition caret. `Some((s, e))` with `s == e`
    /// renders a thin caret at byte offset `s`. `s < e` renders a highlight
    /// over the `[s, e)` byte range.
    pub preedit_cursor: Option<(usize, usize)>,
    /// `(query, selected_match)` when Ctrl+R search is active (replaces the
    /// normal prompt rendering).
    pub search: Option<(&'a str, Option<&'a str>)>,
    /// v0.9: active mouse-drag selection range `((start_line, start_col),
    /// (end_line, end_col))` in document order, or None when no selection.
    pub selection: Option<((usize, usize), (usize, usize))>,
    /// F2 P0-1: vertical scroll offset for multi-line input when the box is
    /// clamped to 30% of the viewport. The renderer only draws lines
    /// `[scroll_offset, scroll_offset + visible_rows)`.
    pub scroll_offset: usize,
    /// F2 P1-3: When true, the hint line shows `⌃⏎ Run · ⏎ Newline`
    /// (Ctrl+Enter submits). When false, `⏎ Run · ⇧⏎ Newline` (Enter submits,
    /// Shift+Enter newlines).
    pub submit_on_ctrl_enter: bool,
}

fn prompt_hint_text(submit_on_ctrl_enter: bool, line_count: usize) -> Option<&'static str> {
    if submit_on_ctrl_enter {
        Some("⌃⏎ Run · ⏎ New line")
    } else if line_count > 1 {
        Some("⏎ Run · ⇧⏎ New line")
    } else {
        None
    }
}

pub(crate) fn normalize_preedit_range(text: &str, start: usize, end: usize) -> (usize, usize) {
    fn floor_boundary(text: &str, offset: usize) -> usize {
        let mut offset = offset.min(text.len());
        while offset > 0 && !text.is_char_boundary(offset) {
            offset -= 1;
        }
        offset
    }

    let start = floor_boundary(text, start);
    let end = floor_boundary(text, end).max(start);
    (start, end)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preedit_range_clamps_to_utf8_boundaries() {
        assert_eq!(normalize_preedit_range("啊b", 1, 2), (0, 0));
        assert_eq!(normalize_preedit_range("啊b", 3, 4), (3, 4));
        assert_eq!(normalize_preedit_range("啊b", 99, 99), (4, 4));
    }

    #[test]
    fn default_single_line_prompt_hides_redundant_key_hint() {
        assert_eq!(prompt_hint_text(false, 1), None);
        assert_eq!(prompt_hint_text(false, 2), Some("⏎ Run · ⇧⏎ New line"));
        assert_eq!(prompt_hint_text(true, 1), Some("⌃⏎ Run · ⏎ New line"));
    }

    #[test]
    fn long_single_line_soft_wraps_without_changing_source() {
        let lines = vec!["abcdefghij".to_string()];
        let visual = prompt_visual_layout(&lines, (0, 10), 6);
        let texts: Vec<_> = visual.rows.iter().map(|row| row.text.as_str()).collect();
        assert_eq!(texts, vec!["abcd", "efghij", ""]);
        assert_eq!(visual.cursor_row, 2);
        assert_eq!(visual.cursor_display_col, 0);
        assert_eq!(lines, vec!["abcdefghij"]);
    }

    #[test]
    fn soft_wrap_counts_cjk_display_columns() {
        let lines = vec!["ab你好cd".to_string()];
        let visual = prompt_visual_layout(&lines, (0, 4), 6);
        let texts: Vec<_> = visual.rows.iter().map(|row| row.text.as_str()).collect();
        assert_eq!(texts, vec!["ab你", "好cd"]);
        assert_eq!(visual.cursor_row, 1);
        assert_eq!(visual.cursor_display_col, 2);
    }

    #[test]
    fn full_logical_line_before_newline_does_not_add_blank_visual_row() {
        let lines = vec!["abcd".to_string(), "next".to_string()];
        let visual = prompt_visual_layout(&lines, (1, 4), 6);
        let texts: Vec<_> = visual.rows.iter().map(|row| row.text.as_str()).collect();
        assert_eq!(texts, vec!["abcd", "next"]);
    }

    #[test]
    fn exact_capacity_final_cursor_keeps_prompt_box_geometry_consistent() {
        let ctx = crate::layout::LayoutCtx::new((60.0, 400.0), 10.0, 20.0, 0.0, 0.0);
        let lines = vec!["abcd".to_string()];
        let (visual, layout, _) = prompt_layout_for_buffer(&ctx, &lines, (0, 4));
        assert_eq!(visual.rows.len(), 2);
        assert_eq!(
            layout.box_rect,
            crate::layout::layout_prompt(&ctx, visual.rows.len(), 1, 0, 0).box_rect
        );
    }
}
