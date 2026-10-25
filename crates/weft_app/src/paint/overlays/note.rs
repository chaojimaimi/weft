//! Note-editor overlay vertex builder, moved verbatim out of
//! `overlays.rs` (v1.13.9 backlog clear zero-behavior file-budget split;
//! `impl MetalRenderer` cross-file block per the vt/modes.rs precedent).

use super::{color_to_normalized, push_quad, NoteEditorDrawState};
use crate::paint::text::{
    note_card_width, note_display_window, NOTE_CARD_ACCENT_STRIPE_W, NOTE_CARD_INNER_PAD,
    NOTE_CARD_LABEL, NOTE_CARD_LINE_H_CELLS, NOTE_CARD_LINE_H_MIN_PX, NOTE_CARD_TOP_OFFSET,
};
use crate::renderer::MetalRenderer;

impl MetalRenderer {
    /// v1.7.3-C: Build the inline note editor overlay — a top-center card
    /// showing "Note: [buffer|]" with a caret and a hint footer. Uses the
    /// same command-surface shell as Find/ContextMenu for visual consistency.
    pub(crate) fn build_note_editor_vertices(&self, state: &NoteEditorDrawState) -> Vec<f32> {
        use crate::paint::command_surface::{build_command_surface_shell, CommandSurfaceShell};

        let cw = self.cell_width() as f32;
        let ch = self.cell_height() as f32;
        let ctx = match &self.layout_ctx {
            Some(c) => *c,
            None => return Vec::new(),
        };

        // Layout: top-center card, 60% of viewport width (clamped to
        // [40cw, 80cw]). Height = 1 input line + 1 hint line + padding.
        // v1.12.24.1 (P1-3): geometry constants are the shared `paint::text`
        // note-card consts (single source of truth with the IME anchor).
        let vp_w = ctx.viewport.0;
        let target_w = note_card_width(vp_w, cw);
        let popup_x0 = (vp_w - target_w) * 0.5;
        let popup_x1 = popup_x0 + target_w;
        let popup_y0 = ctx.top() + NOTE_CARD_TOP_OFFSET;
        let line_h = (ch * NOTE_CARD_LINE_H_CELLS).max(ch + NOTE_CARD_LINE_H_MIN_PX);
        let hint_h = ch * 1.2;
        let popup_y1 = popup_y0 + line_h + hint_h + ch * 0.3;

        let mut verts = Vec::new();
        let (su, sv, suw, svh) = self.space_uv();
        let bg_uv = [su, sv + svh, su + suw, sv];

        let theme_bg = color_to_normalized(self.theme.background);
        let accent = color_to_normalized(self.theme.accent);
        let fg = color_to_normalized(self.theme.foreground);
        let sep = color_to_normalized(self.theme.separator);
        let prompt_c = color_to_normalized(
            crate::ui_tokens::UiColors::from_theme(&self.theme)
                .with_increase_contrast(self.increase_contrast)
                .text_secondary,
        );

        // Shell (shadow + bg + border). v1.14.0 (T5): token-driven material
        // (compact radius + dual-layer soft shadow). v1.14.1 (F1): tokens
        // hoisted so the ring + stripe below read the SAME radius source.
        let surface_tokens = crate::ui_tokens::SurfaceTokens::compact()
            .scaled(self.scale as f32)
            .for_environment(self.theme.is_dark(), self.increase_contrast);
        build_command_surface_shell(
            &mut verts,
            CommandSurfaceShell::canonical(
                [popup_x0, popup_y0, popup_x1, popup_y1],
                surface_tokens,
                false,
                theme_bg,
                bg_uv,
            ),
        );

        // Focus ring — the note editor captures keyboard input.
        // v1.14.1 (F1): rounded ring following the compact shell's corners
        // (was the square build_focus_ring) — outer edge = the card rect
        // outer-expanded 1px (v1.13.17 convention), radius = card radius + 1.
        {
            use crate::paint::primitives::{
                focus_ring_alpha, focus_ring_thickness, push_rounded_rect_ring,
            };
            let ring_color = [
                accent[0],
                accent[1],
                accent[2],
                focus_ring_alpha(self.increase_contrast),
            ];
            push_rounded_rect_ring(
                &mut verts,
                [
                    popup_x0 - 1.0,
                    popup_y0 - 1.0,
                    popup_x1 + 1.0,
                    popup_y1 + 1.0,
                ],
                surface_tokens.radius + 1.0,
                focus_ring_thickness(self.increase_contrast),
                ring_color,
                bg_uv,
            );
        }

        // Accent left stripe — branded accent. v1.14.1 (F1): the stripe
        // follows the card's rounded left contour (concentric corner
        // sectors) — per the caller contract it receives the stripe's own
        // outer geometry: the card rect + card radius verbatim (no border
        // inset on the note card).
        let stripe_w = NOTE_CARD_ACCENT_STRIPE_W;
        crate::paint::primitives::push_rounded_left_stripe(
            &mut verts,
            [popup_x0, popup_y0, popup_x1, popup_y1],
            surface_tokens.radius,
            stripe_w,
            accent,
            crate::paint::primitives::ROUNDED_RECT_SEGMENTS,
        );

        // Input line: "Note: " + buffer + caret.
        let inner_pad = NOTE_CARD_INNER_PAD;
        let text_x = popup_x0 + stripe_w + inner_pad;
        let text_w = popup_x1 - popup_x0 - stripe_w - inner_pad * 2.0;
        let line_y = popup_y0 + (line_h - ch) * 0.5;

        let label = NOTE_CARD_LABEL;
        let label_cells = label.len(); // ASCII
        self.push_text(&mut verts, text_x, line_y, label, prompt_c, label_cells);

        let buf_x = text_x + label_cells as f32 * cw;
        let buf_max_cells = ((text_x + text_w - buf_x) / cw).floor() as usize;
        // v1.12.24.1 (N-4): cell-based trailing display window — the v1.7.3
        // char-count math drew the caret at half the CJK text extent and
        // never scrolled once the card's cell budget filled.
        let preedit_cells = Self::text_col_width(&state.ime_preedit);
        let win = note_display_window(&state.buffer, state.cursor, preedit_cells, buf_max_cells);
        if !win.display.is_empty() {
            let buf_cols = Self::text_col_width(&win.display);
            self.push_text(&mut verts, buf_x, line_y, &win.display, fg, buf_cols);
        }

        // Caret: blink-driven block cursor at the caret position (clamped
        // to the visible display region so it doesn't overflow the popup).
        let caret_x = buf_x + win.caret_cell as f32 * cw;
        if self.cursor_blink_on {
            push_quad(
                &mut verts,
                [caret_x, line_y, caret_x + cw * 0.5, line_y + ch],
                bg_uv,
                [0.0; 4],
                fg,
            );
        }

        // v1.12.24 (N-1): IME preedit inline at the caret (palette precedent),
        // accent-colored + underlined. Width = text_col_width (CJK = 2 cols);
        // visible truncation rides push_text's column budget.
        if !state.ime_preedit.is_empty() {
            let preedit_max = buf_max_cells.saturating_sub(win.caret_cell);
            let cols = Self::text_col_width(&state.ime_preedit).min(preedit_max) as f32;
            let underline = [caret_x, line_y + ch - 1.0, caret_x + cols * cw, line_y + ch];
            let preedit = &state.ime_preedit;
            self.push_text(&mut verts, caret_x, line_y, preedit, accent, preedit_max);
            push_quad(&mut verts, underline, bg_uv, [0.0; 4], accent);
        }

        // Separator between input and hint.
        let sep_y = popup_y0 + line_h;
        push_quad(
            &mut verts,
            [popup_x0 + 2.0, sep_y, popup_x1 - 2.0, sep_y + 1.0],
            bg_uv,
            [0.0; 4],
            sep,
        );

        // Hint footer: "Enter to save · Esc to cancel"
        let hint_y = sep_y + ch * 0.15;
        let hint = "\u{23ce} save  \u{238b} cancel";
        self.push_text(
            &mut verts,
            text_x,
            hint_y,
            hint,
            prompt_c,
            (text_w / cw * 0.9) as usize,
        );

        verts
    }
}
