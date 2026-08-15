//! v1.10.21: alt-screen history-peek indicator pill.
//!
//! While the user browses the history BlockView over an alt-screen TUI
//! (Shift+wheel-up), a small pill at the top of the pane reminds them of
//! the peek gestures — the plain-wheel semantics changed with
//! FIX_ALT_PEEK_WARP_ALIGNMENT ("普通滚轮=与应用交互", so an unmarked
//! wheel now exits the peek instead of scrolling history). Drawn as a
//! semi-transparent accent quad + one text row, exactly like the
//! bottom-left status hint — no layout involvement.

use crate::paint::primitives::{color_to_normalized, push_quad, scale_color_alpha};
use crate::renderer::MetalRenderer;

/// Shared by the paint path and the per-frame atlas warm-up so both always
/// agree on the exact glyph set (missing atlas glyphs are silently skipped
/// by `push_text`).
pub(crate) const PILL_TEXT: &str = "↺ 历史回看 · Shift+滚轮翻阅 · 普通滚轮/按键返回";

impl MetalRenderer {
    /// Append the peek pill's vertices (background quad + label) for the
    /// current pane's top edge. Caller must only invoke this while
    /// `terminal.is_alt_screen_history_peek()` is true.
    pub(crate) fn push_alt_peek_pill(&self, vertices: &mut Vec<f32>) {
        let cw = self.cell_width() as f32;
        let ch = self.cell_height() as f32;
        let ctx = match self.layout_ctx {
            Some(ctx) => ctx,
            None => return,
        };
        if cw <= 0.0 || ch <= 0.0 || ctx.right() - ctx.left() <= cw {
            return;
        }
        let avail_cols = ((ctx.right() - ctx.left()) / cw).floor() as usize;
        let max_cols = avail_cols.saturating_sub(2).max(1); // 1 col margin each side
        let ui = crate::ui_tokens::UiColors::from_theme(&self.theme)
            .with_increase_contrast(self.increase_contrast);
        let accent = color_to_normalized(ui.focus);
        // Slightly shorter than a full row so the pill floats over the
        // content instead of covering it.
        let text_h = ch * 0.78;
        let text_y = ctx.top() + ch * 0.12;
        let pad_x = cw * 0.5;
        let text_cols = Self::text_col_width(PILL_TEXT).min(max_cols);
        let x0 = ctx.left();
        let x1 = x0 + text_cols as f32 * cw + pad_x * 2.0;
        let (su, sv, suw, svh) = self.space_uv();
        let bg_uv = [su, sv + svh, su + suw, sv];
        // Fill + hairline border (same recipe as the settings keycaps).
        push_quad(
            vertices,
            [x0, text_y, x1, text_y + text_h],
            bg_uv,
            [0.0; 4],
            [accent[0], accent[1], accent[2], 0.12],
        );
        let stroke = self.scale() as f32;
        let border = [accent[0], accent[1], accent[2], 0.35];
        for edge in [
            [x0, text_y, x1, text_y + stroke],
            [x0, text_y + text_h - stroke, x1, text_y + text_h],
            [x0, text_y, x0 + stroke, text_y + text_h],
            [x1 - stroke, text_y, x1, text_y + text_h],
        ] {
            push_quad(vertices, edge, bg_uv, [0.0; 4], border);
        }
        let fg = scale_color_alpha(accent, 0.92);
        self.push_text_with_height(
            vertices,
            [x0 + pad_x, text_y],
            PILL_TEXT,
            fg,
            max_cols,
            text_h,
            crate::glyph::GlyphStyle::REGULAR,
        );
    }
}
