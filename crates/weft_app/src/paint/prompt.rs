//! Prompt input box painter — the bottom editor area with ❯ prompt,
//! editor buffer lines, cursor bar, IME preedit, and Ctrl+R search UI.
//!
//! Extracted from `renderer.rs` (M2) to keep the main file focused on
//! GPU pipeline + frame orchestration. Still `impl MetalRenderer` because
//! it needs glyph atlas access via `push_text` / `push_line_tokenized`.

use crate::paint::primitives::{color_to_normalized, push_quad};
use crate::renderer::MetalRenderer;

impl MetalRenderer {
    /// Build vertices for the bottom editor input box (v0.5 editor takeover):
    /// a translucent panel pinned to the bottom, a `❯ <cwd>` prompt, the editor
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

        // v0.8: cursor X must use the DISPLAY width of chars before the cursor,
        // not the char count — CJK chars occupy 2 columns each, so `cc × cw`
        // leaves the caret stranded mid-cell for input like "Weft项目设计.md".
        // Sum the actual rendered columns of the first `cc` chars on this line.
        let (cl, cc) = p.cursor;
        let cursor_offset_cols = p
            .lines
            .get(cl)
            .map(|line| {
                line.chars()
                    .take(cc)
                    .map(Self::char_col_width)
                    .sum::<usize>()
            })
            .unwrap_or(cc);

        // v0.8 stage 4: layout (box rect, text_y0, cursor X/Y, bar_w) is
        // computed by the pure function in `layout.rs`. The renderer keeps
        // responsibility for vertex building, theming, and text rasterization.
        let ctx = self.layout_ctx.expect("LayoutCtx built at draw() entry");
        let layout = crate::layout::layout_prompt(
            &ctx,
            p.lines.len(),
            cl,
            cursor_offset_cols,
            p.scroll_offset,
        );
        let text_y0 = layout.text_y0;
        let left = layout.left;
        let box_cols = layout.box_cols;
        let first_line_text_x = layout.first_line_text_x;
        let cx = layout.cursor_x;
        let cy = layout.cursor_y;
        let bar_w = layout.bar_w;
        let visible_rows = layout.visible_rows;
        let scroll_offset = p.scroll_offset;

        let theme_bg = color_to_normalized(self.theme.background);
        // The input area uses the SAME background as the window (Warp style —
        // no distinct input panel). Still opaque so it cleanly covers the grid
        // rows behind it (the shell's blank prompt sits under the box).
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
        // v1.0 fix: use accent (not accent_dim) for the prompt marker ❯ —
        // accent_dim is invisible in Nord/Warp themes. The prompt ❯ is a
        // primary UI element, not dim chrome, so accent is appropriate.
        let prompt_str = "❯ ";
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
        if let Some(((sl, sc), (el, ec))) = p.selection {
            // F2 P0-1: only render selection highlight for visible lines,
            // adjusting Y by scroll_offset.
            let vis_end = (scroll_offset + visible_rows).min(p.lines.len());
            for i in (sl.max(scroll_offset))..=(el.min(vis_end.saturating_sub(1))) {
                let Some(line) = p.lines.get(i) else {
                    continue;
                };
                let y = text_y0 + (i - scroll_offset) as f32 * ch;
                let (line_start_x, max_chars) = if i == 0 {
                    let avail = box_cols.saturating_sub(prompt_chars).max(1);
                    (first_line_text_x, avail)
                } else {
                    (left, box_cols)
                };
                // Char column range within this line.
                let col_start = if i == sl { sc } else { 0 };
                let col_end = if i == el { ec } else { line.chars().count() };
                if col_start >= col_end {
                    continue;
                }
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
        let vis_end = (scroll_offset + visible_rows).min(p.lines.len());
        for i in scroll_offset..vis_end {
            let line = &p.lines[i];
            let y = text_y0 + (i - scroll_offset) as f32 * ch;
            let (start_x, max_chars) = if i == 0 {
                let avail = box_cols.saturating_sub(prompt_chars).max(1);
                (first_line_text_x, avail)
            } else {
                (left, box_cols)
            };
            self.push_line_tokenized(&mut verts, start_x, y, line, max_chars);
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
        if hint_y >= text_y0 {
            let hint_text = if p.submit_on_ctrl_enter {
                "⌃⏎ Run · ⏎ Newline"
            } else {
                "⏎ Run · ⇧⏎ Newline"
            };
            let ui = crate::ui_tokens::UiColors::from_theme(&self.theme);
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
        let mut px = x_left;
        let mut col = 0f32; // column units consumed
        for (ci, c) in text.chars().enumerate() {
            let w = unicode_width::UnicodeWidthChar::width_cjk(c).unwrap_or(0);
            if w == 0 {
                continue;
            }
            if ci >= c_end {
                break;
            }
            let cell_w = w as f32 * cw;
            if ci >= c_start {
                // This char is inside the highlight range.
                push_quad(
                    vertices,
                    [px, y_top, px + cell_w, y_top + height],
                    bg_uv,
                    [0.0; 4], // fg mask: no text contribution (pure background)
                    bg_color,
                );
            }
            px += cell_w;
            col += w as f32;
            let _ = col; // (kept for symmetry with push_text's col accounting)
        }
    }
}

/// What the bottom editor input box should draw (v0.5 editor takeover). Built
/// by the app only in Editor mode and passed to [`MetalRenderer::draw`].
pub struct PromptDrawParams<'a> {
    /// Current working directory (from OSC 7) shown after the `❯` glyph.
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

fn normalize_preedit_range(text: &str, start: usize, end: usize) -> (usize, usize) {
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
}
